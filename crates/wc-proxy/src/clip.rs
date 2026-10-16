//! A third connection to the compositor, on which this program is an ordinary
//! Wayland client, used to carry clipboard **bytes**.
//!
//! # Why the bytes need a connection of their own at all
//!
//! Because `wl_data_offer.receive` is `(string mime_type, h fd)` and
//! `wl_data_source.send` is `(string mime_type, h fd)`, and those two `h`s are
//! the only ones in the core protocol that this project cannot honour.
//!
//! The distinction that makes `wl_keyboard.keymap` workable and these two not
//! is *which way the data flows through the descriptor*. A keymap is data
//! flowing **compositor → client**, so the proxy can create a `memfd` on this
//! side, copy the bytes the compositor named into it, and hand the client a
//! descriptor it made itself (docs/waycompose-design.md §14.7). A clipboard transfer is a
//! **pipe**: the client writes and somebody on the other side of a hypervisor
//! reads, or the reverse. A name can be opened by the far side; a pipe's other
//! end cannot be conjured from a name, and there is nothing to copy at the
//! moment the descriptor is handed over because the bytes do not exist yet.
//!
//! So the transfer is **terminated here**, in the one process that is on the
//! same machine as both the client and its pipe. The client's fd never goes
//! anywhere: it is read from, or written into, by this process. What crosses
//! the boundary is an array of bytes with a mime type on it, exactly as icon
//! pixels do (§16.5) and for the same measured reason — `wayland-backend`'s
//! buffers are 4096 bytes, so a payload arrives in [`CHUNK`]-sized pieces.
//!
//! # Why not the icon connection, which is already here
//!
//! Because `--no-icons` exists, because an icon resolver that fails must not
//! take the clipboard with it, and because the two have nothing to say to each
//! other. A socket is cheap; a shared failure mode is not.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::spec::Spec;
use crate::wire::{self, Header, HEADER_LEN};

/// The protocol this module speaks, as the compositor's own copy of it. An
/// `include_str!` rather than a byte copy in this crate, for the reason
/// `iconlink` gives: two ends of a private interface drifting apart is the one
/// failure a copy would allow and an include cannot.
pub const CLIPBOARD_XML: &str = include_str!("../../wc-compositor/protocols/wc-clipboard-v1.xml");

const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;
const SYNC: u32 = 3;
const MANAGER: u32 = 4;

/// How many bytes of payload go in one `chunk`.
///
/// `wayland-backend`'s receive buffer is 4096 bytes
/// (`rs/mod.rs: DEFAULT_MAX_BUFFER_SIZE`) and a message that does not fit in it
/// cannot be read at all. Identical to the icon feed's and to the compositor's
/// `crate::clipboard::CHUNK`; a hard limit of the far end rather than a tuning
/// choice.
pub const CHUNK: usize = 3072;

/// Which opcodes to use, read from the XML rather than written down here.
struct Opcodes {
    get_registry: u16,
    bind: u16,
    sync: u16,
    chunk: u16,
    data: u16,
    publish: u16,
    start_drag: u16,
    cancel_drag: u16,
    fetch: u16,
    chunk_event: u16,
    done_event: u16,
    fail_event: u16,
    drag_finished_event: u16,
    global_event: u16,
    error_event: u16,
}

impl Opcodes {
    fn from(spec: &Spec) -> Result<Opcodes, String> {
        let req = |iface: &str, name: &str| -> Result<u16, String> {
            let i = spec
                .interface(iface)
                .ok_or_else(|| format!("no {iface} in the protocol table"))?;
            i.requests
                .iter()
                .position(|m| m.name == name)
                .map(|p| p as u16)
                .ok_or_else(|| format!("{iface} has no {name} request"))
        };
        let ev = |iface: &str, name: &str| -> Result<u16, String> {
            let i = spec
                .interface(iface)
                .ok_or_else(|| format!("no {iface} in the protocol table"))?;
            i.events
                .iter()
                .position(|m| m.name == name)
                .map(|p| p as u16)
                .ok_or_else(|| format!("{iface} has no {name} event"))
        };
        Ok(Opcodes {
            get_registry: req("wl_display", "get_registry")?,
            sync: req("wl_display", "sync")?,
            bind: req("wl_registry", "bind")?,
            chunk: req("wc_clipboard_manager_v1", "chunk")?,
            data: req("wc_clipboard_manager_v1", "data")?,
            publish: req("wc_clipboard_manager_v1", "publish")?,
            start_drag: req("wc_clipboard_manager_v1", "start_drag")?,
            cancel_drag: req("wc_clipboard_manager_v1", "cancel_drag")?,
            fetch: req("wc_clipboard_manager_v1", "fetch")?,
            chunk_event: ev("wc_clipboard_manager_v1", "chunk")?,
            done_event: ev("wc_clipboard_manager_v1", "done")?,
            fail_event: ev("wc_clipboard_manager_v1", "fail")?,
            drag_finished_event: ev("wc_clipboard_manager_v1", "drag_finished")?,
            global_event: ev("wl_registry", "global")?,
            error_event: ev("wl_display", "error")?,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Phase {
    Enumerating,
    Bound,
    /// The compositor has no `wc_clipboard_manager_v1`, or something failed.
    /// Said once, and then this is a no-op for the rest of the run — a
    /// clipboard that cannot cross must cost pastes, not windows.
    Off,
}

/// What a completed fetch turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// The bytes, which may legitimately be empty.
    Bytes(Vec<u8>),
    /// The compositor could not answer, with its reason.
    Failed(String),
}

/// Which system selection a fetch is asking about. Mirrors the XML's `source`
/// enum; the numbers are the wire's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Clipboard = 0,
    Drag = 1,
}

pub struct ClipLink {
    sock: UnixStream,
    out: Vec<u8>,
    inbuf: Vec<u8>,
    ops: Opcodes,
    phase: Phase,
    /// Fetches issued and not yet answered, with whatever has arrived so far.
    open: HashMap<u32, Vec<u8>>,
    /// Answers ready for the session to deliver into a client's pipe.
    answers: Vec<(u32, Answer)>,
    /// Publishes that arrived before the manager was bound. A selection is not
    /// worth losing to a race on a socket that was going to be ready a
    /// millisecond later.
    queued: Vec<Vec<(String, Vec<u8>)>>,
    /// The action a `start_drag` came back with, waiting to be collected.
    drag_result: Option<u32>,
    /// A `start_drag` is outstanding. Tracked so that a feed which dies
    /// mid-gesture still produces the one `drag_finished` the client is owed —
    /// a drag with no end is the failure the whole stage exists to remove.
    drag_running: bool,
    pub published: usize,
    pub fetched: usize,
    pub failed: usize,
    pub drags: usize,
    pub bytes_out: u64,
    pub bytes_in: u64,
}

impl ClipLink {
    /// Connect and start the handshake, on the same socket the session uses.
    /// The compositor sees another client.
    pub fn connect(upstream: &Path, spec: &Spec) -> Result<ClipLink, String> {
        let mut spec = spec.clone();
        spec.load(CLIPBOARD_XML).map_err(|e| format!("{e}"))?;
        let ops = Opcodes::from(&spec)?;
        let sock = UnixStream::connect(upstream)
            .map_err(|e| format!("third connection to the compositor: {e}"))?;
        sock.set_nonblocking(true).map_err(|e| format!("{e}"))?;
        let mut link = ClipLink {
            sock,
            out: Vec::new(),
            inbuf: Vec::new(),
            ops,
            phase: Phase::Enumerating,
            open: HashMap::new(),
            answers: Vec::new(),
            queued: Vec::new(),
            drag_result: None,
            drag_running: false,
            published: 0,
            fetched: 0,
            failed: 0,
            drags: 0,
            bytes_out: 0,
            bytes_in: 0,
        };
        // The libwayland handshake by hand, as `iconlink` does it and for the
        // same reason: without the sync there is no way to tell "the manager
        // has not arrived yet" from "this compositor has none".
        link.send(DISPLAY, link.ops.get_registry, &REGISTRY.to_ne_bytes());
        link.send(DISPLAY, link.ops.sync, &SYNC.to_ne_bytes());
        Ok(link)
    }

    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn wants_write(&self) -> bool {
        !self.out.is_empty()
    }

    /// Whether anything can still be expected from the far end.
    pub fn live(&self) -> bool {
        self.phase != Phase::Off
    }

    fn send(&mut self, object: u32, opcode: u16, body: &[u8]) {
        self.out.extend_from_slice(&wire::message(object, opcode, body));
    }

    /// Install a selection on the system clipboard.
    pub fn publish(&mut self, payloads: Vec<(String, Vec<u8>)>) {
        if self.phase == Phase::Off {
            return;
        }
        if self.phase != Phase::Bound {
            self.queued.push(payloads);
            return;
        }
        self.published += 1;
        self.stage(&payloads);
        self.send(MANAGER, self.ops.publish, &[]);
    }

    /// Fill the compositor's staging area: `chunk* data`, once per mime type.
    ///
    /// **Shared by `publish` and `start_drag`, deliberately.** A clipboard
    /// selection and a drag's payload are the same bytes in the same formats;
    /// building them twice would be two ways for the same client's data to
    /// arrive differently, which is exactly the "two mechanisms" failure this
    /// interface was shaped to avoid.
    fn stage(&mut self, payloads: &[(String, Vec<u8>)]) {
        for (mime, bytes) in payloads {
            self.bytes_out += bytes.len() as u64;
            for part in bytes.chunks(CHUNK) {
                let mut body = Vec::with_capacity(part.len() + 8);
                wire::push_bytes(&mut body, part);
                self.send(MANAGER, self.ops.chunk, &body);
            }
            let mut body = Vec::new();
            wire::push_bytes(&mut body, &nul(mime));
            self.send(MANAGER, self.ops.data, &body);
        }
    }

    /// Hand the staged set to the system's drag and drop instead of to the
    /// clipboard. Answered exactly once, with [`ClipLink::take_drag_result`].
    pub fn start_drag(&mut self, payloads: Vec<(String, Vec<u8>)>, actions: u32) -> bool {
        if self.phase != Phase::Bound {
            // Unlike a publish, a drag is not worth queueing: the gesture is
            // over by the time a queued one would run, and a drag that starts
            // late is worse than one that does not start.
            return false;
        }
        self.stage(&payloads);
        self.drags += 1;
        self.drag_running = true;
        self.send(MANAGER, self.ops.start_drag, &actions.to_ne_bytes());
        true
    }

    /// The source withdrew.
    pub fn cancel_drag(&mut self) {
        if self.phase == Phase::Bound {
            self.send(MANAGER, self.ops.cancel_drag, &[]);
        }
    }

    /// The action a finished drag ended with, once, if one has finished.
    pub fn take_drag_result(&mut self) -> Option<u32> {
        let r = self.drag_result.take();
        if r.is_some() {
            self.drag_running = false;
        }
        r
    }

    /// Ask for the system clipboard, or the drag in progress, in one mime type.
    ///
    /// Returns false if there is no feed to ask, which the caller turns into a
    /// closed pipe — an empty paste rather than a client left waiting.
    pub fn fetch(&mut self, token: u32, source: Source, mime: &str) -> bool {
        if self.phase == Phase::Off {
            return false;
        }
        self.open.insert(token, Vec::new());
        let mut body = token.to_ne_bytes().to_vec();
        body.extend_from_slice(&(source as u32).to_ne_bytes());
        wire::push_bytes(&mut body, &nul(mime));
        self.send(MANAGER, self.ops.fetch, &body);
        true
    }

    /// Fetches that have been answered since the last call.
    pub fn take_answers(&mut self) -> Vec<(u32, Answer)> {
        std::mem::take(&mut self.answers)
    }

    /// §93. The feed is being dropped by its owner: everything still open is
    /// answered, and nothing more is sent.
    pub fn abandon(&mut self) {
        self.give_up();
    }

    pub fn on_writable(&mut self) -> io::Result<()> {
        use std::io::Write as _;
        if self.phase == Phase::Off {
            self.out.clear();
            return Ok(());
        }
        while !self.out.is_empty() {
            match (&self.sock).write(&self.out) {
                Ok(0) => break,
                Ok(n) => drop(self.out.drain(..n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn on_readable(&mut self, log: &mut dyn FnMut(&str)) -> io::Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut &self.sock, &mut buf) {
                Ok(0) => {
                    if self.phase != Phase::Off {
                        log("clipboard: the compositor closed the clipboard connection");
                        self.give_up();
                    }
                    return Ok(());
                }
                Ok(n) => self.inbuf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        let mut at = 0usize;
        while let Some(h) = Header::parse(&self.inbuf[at..]) {
            if h.size < HEADER_LEN || self.inbuf.len() - at < h.size {
                break;
            }
            let body = self.inbuf[at + HEADER_LEN..at + h.size].to_vec();
            at += h.size;
            self.event(h, &body, log);
        }
        self.inbuf.drain(..at);
        Ok(())
    }

    /// Nothing more is coming. Every fetch still open is failed rather than
    /// left hanging, because on the other end of each one is a client blocked
    /// on `read`.
    fn give_up(&mut self) {
        self.phase = Phase::Off;
        // §93: and nothing more is written to it. The queued rest of a large
        // selection used to go on being written into the closed socket, and
        // the EPIPE was the error that ended the application's connection.
        self.out.clear();
        if self.drag_running {
            // The one `drag_finished` the client is owed, manufactured here
            // because nobody else can send it now. `none` is the truth: no drop
            // happened. A drag with no end is precisely the failure this stage
            // exists to remove, and a dead feed is the easiest way to produce
            // one.
            self.drag_result = Some(0);
        }
        for (token, _) in std::mem::take(&mut self.open) {
            self.answers
                .push((token, Answer::Failed("the clipboard feed went away".into())));
        }
        self.queued.clear();
    }

    fn event(&mut self, h: Header, body: &[u8], log: &mut dyn FnMut(&str)) {
        if h.object == DISPLAY && h.opcode == self.ops.error_event {
            log(&format!(
                "clipboard: the compositor rejected a request: {}",
                error_text(body)
            ));
            self.give_up();
            return;
        }
        if h.object == REGISTRY && h.opcode == self.ops.global_event {
            if let Some((name, interface, version)) = global(body)
                && interface == "wc_clipboard_manager_v1"
            {
                let mut b = name.to_ne_bytes().to_vec();
                wire::push_bytes(&mut b, &nul(&interface));
                b.extend_from_slice(&version.min(1).to_ne_bytes());
                b.extend_from_slice(&MANAGER.to_ne_bytes());
                self.send(REGISTRY, self.ops.bind, &b);
                self.phase = Phase::Bound;
                log(&format!(
                    "clipboard: bound wc_clipboard_manager_v1 v{version} as object {MANAGER}"
                ));
                for payloads in std::mem::take(&mut self.queued) {
                    self.publish(payloads);
                }
            }
            return;
        }
        if h.object == SYNC {
            if self.phase == Phase::Enumerating {
                log(
                    "clipboard: this compositor has no wc_clipboard_manager_v1, so nothing can \
                     carry clipboard bytes across the boundary; copy and paste will do nothing \
                     rather than failing",
                );
                self.give_up();
            }
            return;
        }
        if h.object != MANAGER {
            return;
        }
        if h.opcode == self.ops.chunk_event {
            if let Some((token, data)) = token_and_array(body)
                && let Some(buf) = self.open.get_mut(&token)
            {
                self.bytes_in += data.len() as u64;
                buf.extend_from_slice(data);
            }
        } else if h.opcode == self.ops.done_event {
            if let Some(token) = word(body, 0)
                && let Some(buf) = self.open.remove(&token)
            {
                self.fetched += 1;
                self.answers.push((token, Answer::Bytes(buf)));
            }
        } else if h.opcode == self.ops.fail_event {
            if let Some(token) = word(body, 0)
                && self.open.remove(&token).is_some()
            {
                self.failed += 1;
                let reason = token_and_string(body).unwrap_or_default();
                self.answers.push((token, Answer::Failed(reason)));
            }
        } else if h.opcode == self.ops.drag_finished_event {
            self.drag_result = Some(word(body, 0).unwrap_or(0));
        }
    }

    /// One line for the closing log.
    pub fn summary(&self) -> String {
        format!(
            "clipboard: {} selection(s) published ({} bytes out), {} paste(s) answered \
             ({} bytes in), {} refused, {} drag(s) started, feed {}",
            self.published,
            self.bytes_out,
            self.fetched,
            self.bytes_in,
            self.failed,
            self.drags,
            match self.phase {
                Phase::Bound => "bound",
                Phase::Enumerating => "never finished binding",
                Phase::Off => "off",
            }
        )
    }
}

/// A Wayland string is NUL-terminated inside its length.
fn nul(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

fn word(body: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(body.get(at..at + 4)?.try_into().ok()?))
}

/// `(uint token, array data)`.
fn token_and_array(body: &[u8]) -> Option<(u32, &[u8])> {
    let token = word(body, 0)?;
    let len = word(body, 4)? as usize;
    Some((token, body.get(8..8 + len)?))
}

/// `(uint token, string reason)`, returning only the reason.
fn token_and_string(body: &[u8]) -> Option<String> {
    let len = word(body, 4)? as usize;
    let text = body.get(8..8 + len.checked_sub(1)?)?;
    Some(String::from_utf8_lossy(text).into_owned())
}

/// `wl_registry.global`: name, interface, version.
fn global(body: &[u8]) -> Option<(u32, String, u32)> {
    let name = word(body, 0)?;
    let len = word(body, 4)? as usize;
    let text = body.get(8..8 + len.checked_sub(1)?)?;
    let padded = 8 + len.div_ceil(4) * 4;
    let version = word(body, padded)?;
    Some((name, String::from_utf8_lossy(text).into_owned(), version))
}

/// `wl_display.error`: object id, code, message.
fn error_text(body: &[u8]) -> String {
    let object = word(body, 0).unwrap_or(0);
    let code = word(body, 4).unwrap_or(0);
    let len = word(body, 8).unwrap_or(0) as usize;
    let text = len
        .checked_sub(1)
        .and_then(|n| body.get(12..12 + n))
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    format!("object {object}, code {code}: {text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §93. A feed whose compositor end went away fails its write once — the
    /// pump drops the feed on that and keeps the client — and once abandoned
    /// it answers every open paste and writes nothing more. Before, the
    /// queued rest of a large selection went on being written into the
    /// closed socket and the EPIPE ended the application's connection.
    #[test]
    fn a_dead_feed_answers_its_pastes_and_writes_nothing_more() {
        let dir = std::env::temp_dir().join(format!("wc-clip-dead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut link = ClipLink::connect(&path, &Spec::builtin().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        assert!(link.fetch(7, Source::Clipboard, "text/plain"));
        drop(server);
        // A big enough write that the closed peer cannot be hidden by a buffer.
        link.out.extend_from_slice(&vec![0u8; 1 << 20]);
        assert!(link.on_writable().is_err(), "the closed socket is an error, once");
        link.abandon();
        let answers = link.take_answers();
        assert!(
            matches!(answers.as_slice(), [(7, Answer::Failed(_))]),
            "the open paste is answered: {answers:?}"
        );
        assert!(!link.wants_write());
        assert!(link.on_writable().is_ok(), "and nothing is written any more");
        assert!(!link.fetch(8, Source::Clipboard, "text/plain"), "a dead feed takes no fetch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_opcodes_come_out_of_the_xml_the_compositor_was_generated_from() {
        let mut spec = Spec::builtin().unwrap();
        spec.load(CLIPBOARD_XML).unwrap();
        let ops = Opcodes::from(&spec).unwrap();
        let iface = spec.interface("wc_clipboard_manager_v1").unwrap();
        // Positional, which is the check: somebody adding a request in the
        // middle of the XML moves both ends together and this still passes,
        // where a hand-written table would silently disagree.
        assert_eq!(iface.requests[ops.chunk as usize].name, "chunk");
        assert_eq!(iface.requests[ops.data as usize].name, "data");
        assert_eq!(iface.requests[ops.publish as usize].name, "publish");
        assert_eq!(iface.requests[ops.fetch as usize].name, "fetch");
        assert_eq!(iface.events[ops.chunk_event as usize].name, "chunk");
        assert_eq!(iface.events[ops.done_event as usize].name, "done");
        assert_eq!(iface.events[ops.fail_event as usize].name, "fail");
    }

    /// **Nothing in this interface carries a descriptor**, and that is the
    /// whole reason it exists. Asserted against the XML rather than trusted,
    /// because the file is shared with the compositor and an `h` added to it
    /// there would compile perfectly here and then be unrepresentable on the
    /// wire that crosses the boundary.
    #[test]
    fn no_message_in_the_clipboard_protocol_carries_an_h() {
        let mut spec = Spec::default();
        spec.load(CLIPBOARD_XML).unwrap();
        let iface = spec.interface("wc_clipboard_manager_v1").unwrap();
        for m in iface.requests.iter().chain(iface.events.iter()) {
            assert_eq!(m.fd_count(), 0, "wc_clipboard_manager_v1.{} carries an h", m.name);
        }
    }

    #[test]
    fn a_chunk_event_body_parses() {
        let mut body = 9u32.to_ne_bytes().to_vec();
        wire::push_bytes(&mut body, b"hello");
        assert_eq!(token_and_array(&body), Some((9, &b"hello"[..])));
        // A truncated one is None rather than a panic: the far end is another
        // process and this is a byte parser.
        assert_eq!(token_and_array(&body[..6]), None);
    }

    #[test]
    fn a_fail_event_body_yields_its_reason() {
        let mut body = 3u32.to_ne_bytes().to_vec();
        wire::push_bytes(&mut body, &nul("no such format"));
        assert_eq!(token_and_string(&body).as_deref(), Some("no such format"));
    }
}
