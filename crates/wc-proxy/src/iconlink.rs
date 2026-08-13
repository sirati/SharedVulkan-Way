//! A second connection to the compositor, on which this program is an ordinary
//! Wayland client, used to publish icon pixels.
//!
//! # Why a second connection, when the first one is right there
//!
//! Because the icon has to be attached to a window that belongs to *somebody
//! else*.
//!
//! `xdg-toplevel-icon-v1` is the protocol for icons and it takes an
//! `xdg_toplevel` object. A Wayland object belongs to exactly one client, and
//! the toplevel belongs to the application — so the only way to use that
//! protocol on the application's behalf would be to inject requests into the
//! application's own connection, minting object ids as if they were its.
//!
//! **That is not possible, and it is measured rather than assumed.**
//! `wayland-backend`'s server-side object map (`rs/map.rs`, `insert_in_at`)
//! accepts a new id only when it is the next free one:
//!
//! ```text
//! match id.cmp(&store.len()) {
//!     Ordering::Greater => Err(()),          // "not the next free id"
//! ```
//!
//! so ids cannot be taken from a high reserved band the client will never
//! reach; they can only be taken from exactly the sequence the client is
//! itself allocating from. Taking one means the client's next `new_id` is a
//! duplicate and the compositor kills the connection. The alternative —
//! renumbering every object id in both directions — would put a rewrite in
//! front of every message in the protocol to save a socket, and docs/design.md
//! §9.3 is explicit that not understanding most messages is what makes this
//! program small enough to trust.
//!
//! So: a second connection, `wc_icon_manager_v1`, and icons keyed by
//! application id instead of by object. The compositor advertises the global;
//! see `crates/wc-compositor/protocols/wc-icon-v1.xml`, which is the file this
//! module reads its opcodes out of.
//!
//! # This speaks the compositor's dialect, and carries no descriptors
//!
//! The far end is the Windows compositor, so the wire is the in-band-`h`
//! dialect of docs/design.md §2.3. Nothing here sends an `h` in either
//! encoding: an icon is a few kilobytes of array argument, so the question of
//! how a descriptor would cross does not arise. That is the point of the
//! protocol having been designed as bytes rather than as `wl_buffer`s.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::icons::{self, Image, Search};
use crate::spec::Spec;
use crate::wire::{self, Header, HEADER_LEN};

/// The protocol this module speaks, as the compositor's own copy of it. Not a
/// byte copy in this crate: the two ends of a private interface drifting apart
/// is the one failure a copy would allow and this include cannot.
const ICON_XML: &str = include_str!("../../wc-compositor/protocols/wc-icon-v1.xml");

const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;
const SYNC: u32 = 3;
const MANAGER: u32 = 4;

/// How many bytes of pixels go in one `chunk`.
///
/// `wayland-backend`'s receive buffer is 4096 bytes
/// (`rs/mod.rs: DEFAULT_MAX_BUFFER_SIZE`) and a message that does not fit in it
/// cannot be read at all, so this is a hard limit of the far end rather than a
/// tuning choice. 3072 leaves room for the header, the length word and the
/// padding several times over.
const CHUNK: usize = 3072;

/// Which opcodes to use, read from the XML rather than written down here.
struct Opcodes {
    get_registry: u16,
    bind: u16,
    sync: u16,
    chunk: u16,
    image: u16,
    publish: u16,
    global_event: u16,
    error_event: u16,
    manager_version: u32,
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
            chunk: req("wc_icon_manager_v1", "chunk")?,
            image: req("wc_icon_manager_v1", "image")?,
            publish: req("wc_icon_manager_v1", "publish")?,
            global_event: ev("wl_registry", "global")?,
            error_event: ev("wl_display", "error")?,
            manager_version: 1,
        })
    }
}

/// What the link is doing.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Phase {
    /// `get_registry` and `sync` are on the wire; globals are arriving.
    Enumerating,
    /// The manager is bound and publishes go straight out.
    Bound,
    /// The compositor has no `wc_icon_manager_v1`, or something failed. Said
    /// once, and then this is a no-op for the rest of the run — a missing icon
    /// must not be a reason for anything else to stop.
    Off,
}

pub struct IconLink {
    sock: UnixStream,
    out: Vec<u8>,
    inbuf: Vec<u8>,
    ops: Opcodes,
    phase: Phase,
    search: Search,
    /// App ids seen before the manager was bound, and their pixels. Two
    /// parallel vectors rather than one of pairs because they are drained
    /// together and never indexed apart.
    queued: Vec<String>,
    queued_images: Vec<Vec<Image>>,
    /// App ids already resolved, so that a client which sets the same app id on
    /// twenty windows costs one directory walk.
    done: HashSet<String>,
    pub published: usize,
    pub empty: usize,
}

impl IconLink {
    /// Connect and start the handshake. The socket is the same one the session
    /// uses; this is a second connection to it, so the compositor sees two
    /// clients.
    pub fn connect(upstream: &Path, spec: &Spec, search: Search) -> Result<IconLink, String> {
        let mut spec = spec.clone();
        spec.load(ICON_XML).map_err(|e| format!("{e}"))?;
        let ops = Opcodes::from(&spec)?;
        let sock = UnixStream::connect(upstream)
            .map_err(|e| format!("second connection to the compositor: {e}"))?;
        sock.set_nonblocking(true).map_err(|e| format!("{e}"))?;
        let mut link = IconLink {
            sock,
            out: Vec::new(),
            inbuf: Vec::new(),
            ops,
            phase: Phase::Enumerating,
            search,
            queued: Vec::new(),
            queued_images: Vec::new(),
            done: HashSet::new(),
            published: 0,
            empty: 0,
        };
        // The libwayland handshake, by hand: ask for the registry, then a sync
        // whose `done` is the moment the global list is known to be complete.
        // Without the sync there is no way to distinguish "the manager has not
        // arrived yet" from "this compositor does not have one", and the
        // difference decides whether to wait or to give up.
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

    fn send(&mut self, object: u32, opcode: u16, body: &[u8]) {
        self.out.extend_from_slice(&wire::message(object, opcode, body));
    }

    /// An app id has gone past on the *other* connection. Resolve it once and
    /// publish whatever came back — including nothing, which is an answer.
    ///
    /// The directory walk and the PNG decode happen inline, on the pump's
    /// thread. That is deliberate and it is bounded: it is one walk per
    /// distinct app id per run, measured at a few milliseconds on the machine
    /// this was written on, and the alternative — a thread and a channel —
    /// would buy latency that nothing is waiting for. A window is not shown
    /// until its first buffer, which is several round trips later.
    pub fn note_app_id(&mut self, app_id: &str, log: &mut dyn FnMut(&str)) {
        if self.phase == Phase::Off || app_id.is_empty() || !self.done.insert(app_id.to_owned()) {
            return;
        }
        let started = std::time::Instant::now();
        let resolution = icons::resolve(app_id, &self.search);
        log(&format!(
            "icon: {} ({:.1} ms)",
            resolution.summary(),
            started.elapsed().as_secs_f64() * 1000.0
        ));
        if resolution.images.is_empty() {
            self.empty += 1;
        } else {
            self.published += 1;
        }
        if self.phase == Phase::Bound {
            self.publish(app_id, &resolution.images);
        } else {
            // Queued rather than resolved-and-queued: the pixels would have to
            // be held anyway, and this way the resolution shows up in the log
            // in the order the app ids arrived rather than in a burst later.
            self.queued.push(app_id.to_owned());
            self.queued_images.push(resolution.images);
        }
    }

    fn publish(&mut self, app_id: &str, images: &[Image]) {
        for image in images {
            for part in image.pixels.chunks(CHUNK) {
                let mut body = Vec::with_capacity(part.len() + 8);
                wire::push_bytes(&mut body, part);
                self.send(MANAGER, self.ops.chunk, &body);
            }
            let mut body = image.width.to_ne_bytes().to_vec();
            body.extend_from_slice(&image.height.to_ne_bytes());
            self.send(MANAGER, self.ops.image, &body);
        }
        let mut body = Vec::new();
        wire::push_bytes(&mut body, &nul(app_id));
        self.send(MANAGER, self.ops.publish, &body);
    }

    /// Push whatever is queued. Never blocks: `WouldBlock` leaves the rest for
    /// the next turn of the pump.
    pub fn on_writable(&mut self) -> io::Result<()> {
        use std::io::Write as _;
        while !self.out.is_empty() {
            // `&UnixStream` implements `Write`, so this borrows rather than
            // needing a mutable socket while `self.out` is also borrowed.
            match (&self.sock).write(&self.out) {
                Ok(0) => break,
                Ok(n) => drop(self.out.drain(..n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Read and act on whatever the compositor has said.
    pub fn on_readable(&mut self, log: &mut dyn FnMut(&str)) -> io::Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut &self.sock, &mut buf) {
                Ok(0) => {
                    if self.phase != Phase::Off {
                        log("icon: the compositor closed the icon connection");
                        self.phase = Phase::Off;
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

    fn event(&mut self, h: Header, body: &[u8], log: &mut dyn FnMut(&str)) {
        if h.object == DISPLAY && h.opcode == self.ops.error_event {
            // The compositor refused something. This is the connection that
            // dies, not the application's, which is the whole reason the icon
            // feed is a separate client.
            log(&format!("icon: the compositor rejected a request: {}", error_text(body)));
            self.phase = Phase::Off;
            return;
        }
        if h.object == REGISTRY && h.opcode == self.ops.global_event {
            if let Some((name, interface, version)) = global(body)
                && interface == "wc_icon_manager_v1" {
                    let mut b = name.to_ne_bytes().to_vec();
                    wire::push_bytes(&mut b, &nul(&interface));
                    b.extend_from_slice(&version.min(self.ops.manager_version).to_ne_bytes());
                    b.extend_from_slice(&MANAGER.to_ne_bytes());
                    self.send(REGISTRY, self.ops.bind, &b);
                    self.phase = Phase::Bound;
                    log(&format!("icon: bound wc_icon_manager_v1 v{version} as object {MANAGER}"));
                    let queued: Vec<(String, Vec<Image>)> = self
                        .queued
                        .drain(..)
                        .zip(self.queued_images.drain(..))
                        .collect();
                    for (app_id, images) in queued {
                        self.publish(&app_id, &images);
                    }
                }
            return;
        }
        if h.object == SYNC {
            // `wl_callback.done` for the sync sent at connect: every global has
            // now been announced, so a manager that has not arrived does not
            // exist.
            if self.phase == Phase::Enumerating {
                log(
                    "icon: this compositor has no wc_icon_manager_v1, so nothing can publish \
                     an icon for an app id; windows will get the system's generic one",
                );
                self.phase = Phase::Off;
                self.queued.clear();
                self.queued_images.clear();
            }
        }
    }

    /// One line for the closing log.
    pub fn summary(&self) -> String {
        format!(
            "icons: {} app id(s) resolved to pixels, {} resolved to nothing, feed {}",
            self.published,
            self.empty,
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

/// `wl_registry.global`: name, interface, version.
fn global(body: &[u8]) -> Option<(u32, String, u32)> {
    let name = u32::from_ne_bytes(body.get(0..4)?.try_into().ok()?);
    let len = u32::from_ne_bytes(body.get(4..8)?.try_into().ok()?) as usize;
    let text = body.get(8..8 + len.checked_sub(1)?)?;
    let padded = 8 + len.div_ceil(4) * 4;
    let version = u32::from_ne_bytes(body.get(padded..padded + 4)?.try_into().ok()?);
    Some((name, String::from_utf8_lossy(text).into_owned(), version))
}

/// `wl_display.error`: object id, code, message.
fn error_text(body: &[u8]) -> String {
    let object = body
        .get(0..4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_ne_bytes)
        .unwrap_or(0);
    let code = body
        .get(4..8)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_ne_bytes)
        .unwrap_or(0);
    let len = body
        .get(8..12)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_ne_bytes)
        .unwrap_or(0) as usize;
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

    #[test]
    fn a_registry_global_event_parses() {
        let mut body = 7u32.to_ne_bytes().to_vec();
        wire::push_bytes(&mut body, &nul("wc_icon_manager_v1"));
        body.extend_from_slice(&1u32.to_ne_bytes());
        assert_eq!(global(&body), Some((7, "wc_icon_manager_v1".into(), 1)));
        assert_eq!(global(&[0, 0, 0]), None);
    }

    #[test]
    fn the_opcodes_come_out_of_the_xml_the_compositor_was_generated_from() {
        let mut spec = Spec::builtin().unwrap();
        spec.load(ICON_XML).unwrap();
        let ops = Opcodes::from(&spec).unwrap();
        // Positional, and that is the check: if somebody adds a request to the
        // middle of the XML, both ends move together and this test still
        // passes — which is the property that a hand-written table would not
        // have.
        let iface = spec.interface("wc_icon_manager_v1").unwrap();
        assert_eq!(iface.requests[ops.chunk as usize].name, "chunk");
        assert_eq!(iface.requests[ops.image as usize].name, "image");
        assert_eq!(iface.requests[ops.publish as usize].name, "publish");
        assert_eq!(ops.get_registry, 1, "wl_display.get_registry is opcode 1");
    }
}
