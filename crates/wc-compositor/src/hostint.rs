//! §106: the host-integration channel on the host. The `OWNER_HOSTINT` ring
//! slot, as a [`wc_hostint::host::Link`], and the thread that serves it.
//!
//! The process that owns the session's rings calls [`spawn`] once it has the
//! slot: on Windows the compositor (the only process that can map the
//! ivshmem device), on macOS the main process. The thread is its own, never
//! the Wayland thread, and it ends when the ring reaches EOF, when the slot
//! is DEAD with nothing left to read, or when its owner says stop (§107.11:
//! the session ended), whichever comes first. A reader that outlived its
//! session read the next owner's bytes once the slot was recycled.
//! Requests are checked by `wc_hostint::host::Service` before the platform's
//! opener runs (security-model.md, "Opening links on the host").

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use sv_transport::control::{Control, RingState};
use sv_transport::wlbytes::{ByteChannel, ReadOutcome};
use wc_hostint::host::{Event, Link, Opener, Poll, Service};
use wc_hostint::wire::LineBuf;

/// How a reader sleeps until the ring has bytes, for at most the given time.
/// It must wait on the doorbell (`ByteChannel::wait_readable` with
/// `ringlink::ring_wait_slice()` once the rings have one), not spin.
pub type Wait = Box<dyn FnMut(&ByteChannel, Duration) + Send>;

/// §107.11: whether the channel's owner wants it to end now (its session
/// has ended). Asked before every read and write.
pub type Stop = Box<dyn Fn() -> bool + Send>;

pub struct RingLink {
    ch: ByteChannel,
    lines: LineBuf,
    wait: Wait,
    /// The region the slot is in, to see it DEAD: the process's ring region
    /// unless the caller gave another.
    control: Option<Arc<Control>>,
    stop: Stop,
}

impl RingLink {
    pub fn new(ch: ByteChannel, wait: Wait) -> Self {
        Self { ch, lines: LineBuf::default(), wait, control: crate::ringlink::shared_control(), stop: Box::new(|| false) }
    }

    /// Ends also when `stop` says so, and sees the slot DEAD in `control`.
    pub fn with_stop(ch: ByteChannel, wait: Wait, control: Option<Arc<Control>>, stop: Stop) -> Self {
        Self { ch, lines: LineBuf::default(), wait, control: control.or_else(crate::ringlink::shared_control), stop }
    }

    /// The slot is closed: its owner said stop, or it is DEAD and every byte
    /// before that has been read (as `RingLink::reap` takes a DEAD slot).
    fn ended(&self) -> bool {
        if (self.stop)() {
            return true;
        }
        self.control.as_ref().is_some_and(|c| c.ring_state(self.ch.slot()).ok() == Some(RingState::Dead))
            && !self.ch.readable().unwrap_or(false)
    }
}

impl Link for RingLink {
    fn poll_line(&mut self, timeout: Duration) -> Poll {
        let deadline = Instant::now() + timeout;
        let mut tmp = [0u8; 4096];
        loop {
            if (self.stop)() {
                return Poll::Closed;
            }
            if let Some(l) = self.lines.next_line() {
                return Poll::Line(l);
            }
            match self.ch.read(&mut tmp) {
                Ok(ReadOutcome::Bytes(n)) => self.lines.push(&tmp[..n]),
                Ok(ReadOutcome::Eof) | Err(_) => return Poll::Closed,
                Ok(ReadOutcome::Empty) => {
                    if self.ended() {
                        return Poll::Closed;
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        return Poll::Idle;
                    }
                    (self.wait)(&self.ch, deadline - now);
                }
            }
        }
    }

    fn send_line(&mut self, line: &str) -> bool {
        if self.ended() {
            return false;
        }
        let mut b = Vec::with_capacity(line.len() + 1);
        b.extend_from_slice(line.as_bytes());
        b.push(b'\n');
        let mut rest = &b[..];
        let t0 = Instant::now();
        while !rest.is_empty() {
            match self.ch.write(rest) {
                Ok(n) if n > 0 => rest = &rest[n..],
                // A full ring: the Linux side is slow. Two seconds, then the
                // peer is taken as gone.
                Ok(_) if t0.elapsed() < Duration::from_secs(2) => std::thread::sleep(Duration::from_millis(2)),
                _ => return false,
            }
        }
        true
    }
}

/// Serve the channel on a thread of its own until it ends. `log` gets one
/// line per request.
pub fn spawn(
    ch: ByteChannel,
    wait: Wait,
    mut svc: Service,
    mut opener: Box<dyn Opener + Send>,
    events: Receiver<Event>,
    mut log: Box<dyn FnMut(&str) + Send>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("hostint".into()).spawn(move || {
        serve_link(RingLink::new(ch, wait), &mut svc, opener.as_mut(), &events, &mut *log);
    })
}

/// Serve `link` on this thread until it ends; the caller's thread is the
/// reader, so whatever it holds for the slot is held until this returns.
pub fn serve_link(mut link: RingLink, svc: &mut Service, opener: &mut dyn Opener, events: &Receiver<Event>, log: &mut dyn FnMut(&str)) {
    let slot = link.ch.slot();
    log(&format!("hostint: serving ring slot {slot}"));
    wc_hostint::host::serve(&mut link, svc, opener, events, Duration::from_millis(250), log);
    log(&format!("hostint: the channel ended (slot {slot})"));
}

/// The wait every caller should pass to [`spawn`]: sleep on the lane's
/// doorbell (§103.15), at most one `ring_wait_slice`, which is long when the
/// rings have a doorbell and a millisecond when they do not.
pub fn doorbell_wait() -> Wait {
    Box::new(|ch, t| {
        let _ = ch.wait_readable(t.min(crate::ring_wait_slice()));
    })
}

// ------------------------------------------------- the Windows host's half --

/// What the launcher tells the compositor about host integration, on its
/// command line: `--hostint` turns it on, `--hostint-share HOST=LINUX` and
/// `--hostint-drop HOST=LINUX` are the path map (wc-launch's `[hostint]`),
/// `--hostint-browser EXE` the browser for Linux's links (the user's
/// default when absent) and `--hostint-browser-arg X`, repeated, its fixed
/// arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostCfg {
    pub on: bool,
    pub paths: wc_hostint::wire::PathMap,
    pub browser: Option<String>,
    /// `--hostint-browser-arg X`, repeated: the browser's fixed arguments,
    /// before the URL.
    pub browser_args: Vec<String>,
}

static HOSTCFG: std::sync::Mutex<Option<HostCfg>> = std::sync::Mutex::new(None);

fn pair(v: &str) -> Result<(String, String), String> {
    let (h, l) = v.split_once('=').ok_or("wants HOST=LINUX")?;
    if h.is_empty() || !l.starts_with('/') || v.chars().any(char::is_control) {
        return Err(format!("{v:?} is not HOST=LINUX with an absolute Linux path"));
    }
    Ok((h.to_string(), l.to_string()))
}

/// One of the `--hostint*` flags. Returns whether `flag` was one.
pub fn parse_flag(flag: &str, value: &mut dyn FnMut() -> Option<String>) -> Result<bool, String> {
    let mut g = HOSTCFG.lock().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert_with(HostCfg::default);
    let mut need = || value().ok_or(format!("{flag} needs a value"));
    match flag {
        "--hostint" => c.on = true,
        "--hostint-share" => c.paths.shares.push(pair(&need()?)?),
        "--hostint-drop" => c.paths.drop = Some(pair(&need()?)?),
        "--hostint-browser" => c.browser = Some(need()?),
        "--hostint-browser-arg" => {
            let v = need()?;
            if v.chars().any(char::is_control) || v.len() > 1024 || c.browser_args.len() >= 16 {
                return Err(format!("{flag} {v:?}: at most 16, each at most 1024 bytes, no control characters"));
            }
            c.browser_args.push(v);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// The configuration, if `--hostint` was given.
pub fn configured() -> Option<HostCfg> {
    HOSTCFG.lock().unwrap_or_else(|e| e.into_inner()).clone().filter(|c| c.on)
}

/// The host's service: http and https for links, the configured shares.
pub fn service(platform: &'static str, cfg: &HostCfg, windows_paths: bool) -> Service {
    Service::new(wc_hostint::host::Config {
        platform,
        schemes: wc_hostint::openurl::default_schemes(),
        paths: cfg.paths.clone(),
        windows_paths,
    })
}

/// How often the host's applications are enumerated again: installing one
/// is rare, and enumerating takes about two seconds.
#[cfg(windows)]
const HANDLERS_EVERY: Duration = Duration::from_secs(600);

/// Windows: serve the `OWNER_HOSTINT` slot (already opened by the caller,
/// `session_svc`'s service channel) with the shell opener and the
/// association handlers, on the caller's thread, until the slot ends or
/// `stop` says so (§107.11).
#[cfg(windows)]
pub fn serve_windows(ch: ByteChannel, cfg: HostCfg, say: fn(&str), stop: Stop) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use wc_hostplat::win;
    let (tx, rx) = std::sync::mpsc::channel();
    // The audio endpoints are not this channel's: they are the sound ring's
    // device side (§103.16). The handlers thread looks at `done` every
    // second, so it goes with the channel rather than at its next round.
    let done = Arc::new(AtomicBool::new(false));
    let d2 = Arc::clone(&done);
    let _ = std::thread::Builder::new().name("hostint-handlers".into()).spawn(move || {
        loop {
            if tx.send(Event::Handlers(win::handlers::handlers())).is_err() {
                return;
            }
            let t0 = Instant::now();
            while t0.elapsed() < HANDLERS_EVERY {
                if d2.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
    let mut svc = service("windows", &cfg, true);
    let mut opener = win::open::WinOpener { browser: cfg.browser.clone(), browser_args: cfg.browser_args.clone() };
    serve_link(RingLink::with_stop(ch, doorbell_wait(), None, stop), &mut svc, &mut opener, &rx, &mut |l: &str| say(l));
    done.store(true, Ordering::SeqCst);
}

/// TEST FIXTURE ONLY (`scripts/wc-hostint-win-local.sh`): the Linux build
/// of the compositor standing in for the Windows guest. It serves the slot
/// exactly as [`serve_windows`] does, with the same `Service` (so every
/// check that decides what may open is the real one, Windows paths
/// included), and an opener that opens nothing: it logs what Windows would
/// have opened and answers `ok`. No handlers are offered. On the caller's
/// thread, until the slot ends or `stop` says so.
#[cfg(target_os = "linux")]
pub fn serve_logging(ch: ByteChannel, cfg: HostCfg, say: fn(&str), stop: Stop) {
    serve_logging_in(ch, None, cfg, say, stop)
}

/// [`serve_logging`] with the slot's region given (the tests' own).
#[cfg(target_os = "linux")]
fn serve_logging_in(ch: ByteChannel, control: Option<Arc<Control>>, cfg: HostCfg, say: fn(&str), stop: Stop) {
    struct LogOpener(fn(&str));
    impl Opener for LogOpener {
        fn open_url(&mut self, url: &str) -> Result<(), String> {
            (self.0)(&format!("hostint: would open url {url}"));
            Ok(())
        }
        fn open_path(&mut self, path: &str) -> Result<(), String> {
            (self.0)(&format!("hostint: would open path {path}"));
            Ok(())
        }
        fn open_with(&mut self, app: &str, target: &wc_hostint::wire::Target) -> Result<(), String> {
            (self.0)(&format!("hostint: would open with {app}: {target:?}"));
            Ok(())
        }
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = tx.send(Event::Handlers(Vec::new()));
    let mut svc = service("windows", &cfg, true);
    serve_link(RingLink::with_stop(ch, doorbell_wait(), control, stop), &mut svc, &mut LogOpener(say), &rx, &mut |l: &str| say(l));
    // Held to here: the service ends when every sender is gone, and this
    // one stands for `serve_windows`' handlers thread.
    drop(tx);
}

#[cfg(test)]
mod flag_tests {
    use super::*;

    #[test]
    fn flags_make_the_path_map() {
        let mut vals = vec!["Z:\\=/srv/winshare".to_string(), "Z:\\waycompose-open=/srv/winshare/waycompose-open".into()].into_iter();
        let mut next = || vals.next();
        assert!(!parse_flag("--other", &mut next).unwrap());
        assert!(configured().is_none(), "off until --hostint");
        assert!(parse_flag("--hostint-share", &mut next).unwrap());
        assert!(parse_flag("--hostint-drop", &mut next).unwrap());
        assert!(parse_flag("--hostint", &mut || None).unwrap());
        let c = configured().unwrap();
        assert_eq!(c.paths.shares, [("Z:\\".to_string(), "/srv/winshare".to_string())]);
        assert_eq!(c.paths.drop.as_ref().unwrap().1, "/srv/winshare/waycompose-open");
        assert!(parse_flag("--hostint-share", &mut || Some("Z:\\=relative".into())).is_err());
        assert!(parse_flag("--hostint-browser", &mut || None).is_err());
        assert!(parse_flag("--hostint-browser-arg", &mut || Some("--new-window".into())).unwrap());
        assert!(parse_flag("--hostint-browser-arg", &mut || Some("--user-data-dir=C:\\T".into())).unwrap());
        assert!(parse_flag("--hostint-browser-arg", &mut || Some("a\nb".into())).is_err());
        assert_eq!(configured().unwrap().browser_args, ["--new-window", "--user-data-dir=C:\\T"]);
    }
}

/// §107.11: a session's hostint thread stops when its session ends, its slot
/// is recycled only after that, and the slot's next owner, a Wayland client
/// of another session, gets every byte it sends. The guest's reopen round of
/// 2026-10-19: foot on vkcube's old hostint slot, its bytes read by a thread
/// that outlived its session, and no window.
#[cfg(all(test, target_os = "linux"))]
mod reopen_tests {
    use super::*;
    use crate::ringlink::{has_reader, RingLink as Links, SlotReader};
    use std::io::Read as _;
    use std::sync::atomic::{AtomicBool, Ordering};
    use sv_transport::control::{Geometry, ProtocolIds};
    use sv_transport::wlbytes::{with_session, Endpoint, OWNER_HOSTINT};

    /// The tests share `READERS` and slot numbers; one at a time.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn quiet(_: &str) {}

    /// The compositor's ring region (it owns it, as over ivshmem), its
    /// links, and the proxy's view of it.
    fn rings() -> (Links, Arc<Control>, Control) {
        let g = Geometry::small();
        let r = Arc::new(sv_transport::region::SharedRegion::heap(g.control_size as usize).unwrap());
        let ids = ProtocolIds::new(1, 2);
        let owner = Arc::new(Control::create(Arc::clone(&r), g, ids, 1).unwrap());
        (Links::over(Arc::clone(&owner)), owner, Control::attach(r, ids).unwrap())
    }

    fn read_some(ch: &mut ByteChannel, within: Duration) -> Vec<u8> {
        let t0 = Instant::now();
        let mut buf = [0u8; 4096];
        while t0.elapsed() < within {
            match ch.read(&mut buf).unwrap() {
                ReadOutcome::Bytes(n) => return buf[..n].to_vec(),
                ReadOutcome::Eof => return Vec::new(),
                ReadOutcome::Empty => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        Vec::new()
    }

    /// One session's hostint, served as `session_svc` serves it, ended by
    /// `by_stop` (the session's end), `by_dead` (the slot killed), or both.
    fn a_session_ends_and_its_slot_goes_to_the_next(by_stop: bool, by_dead: bool) {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let (mut links, comp, proxy) = rings();
        let mut svc_ch = ByteChannel::claim(&proxy, with_session(OWNER_HOSTINT, 3)).unwrap();
        let slot = svc_ch.slot();
        let ch = ByteChannel::open(&comp, slot, Endpoint::Peer).unwrap();
        let ended = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        {
            let (comp, ended) = (Arc::clone(&comp), Arc::clone(&ended));
            std::thread::spawn(move || {
                let _reader = SlotReader::new(slot);
                let stop: Stop = Box::new(move || ended.load(Ordering::SeqCst));
                serve_logging_in(ch, Some(comp), HostCfg { on: true, ..HostCfg::default() }, quiet, stop);
                drop(_reader);
                let _ = done_tx.send(());
            });
        }
        // It serves: the host's hello comes first.
        let hello = read_some(&mut svc_ch, Duration::from_secs(2));
        assert!(String::from_utf8_lossy(&hello).starts_with("hello"), "no hello: {hello:?}");
        assert!(has_reader(slot));

        // The session ends, as `session_svc::end` ends it.
        if by_stop {
            ended.store(true, Ordering::SeqCst);
        }
        if by_dead {
            comp.kill_ring(slot).unwrap();
        }
        assert!(
            done_rx.recv_timeout(Duration::from_secs(3)).is_ok(),
            "the session's hostint thread is still reading slot {slot} (stop {by_stop}, dead {by_dead})"
        );
        assert!(!has_reader(slot), "its reader is given up");
        if !by_dead {
            comp.kill_ring(slot).unwrap();
        }
        drop(svc_ch);
        assert_eq!(links.reap(), 1, "slot {slot} comes back once nothing reads it");

        // The next session's first client claims the lowest free slot: this one.
        let mut client = ByteChannel::claim(&proxy, with_session(7, 4)).unwrap();
        assert_eq!(client.slot(), slot);
        let accepted = links.accept();
        assert_eq!(accepted.len(), 1);
        let (_, mut theirs, session, feed) = accepted.into_iter().next().unwrap();
        assert_eq!((session, feed), (4, false));
        theirs.set_nonblocking(true).unwrap();

        // Every byte it sends arrives, in order.
        let sent: Vec<u8> = (0..256 * 1024u32).map(|i| (i * 7 + i / 251) as u8).collect();
        let mut got = Vec::with_capacity(sent.len());
        let (mut at, t0) = (0, Instant::now());
        let mut buf = vec![0u8; 64 * 1024];
        while got.len() < sent.len() && t0.elapsed() < Duration::from_secs(10) {
            if at < sent.len() {
                at += client.write(&sent[at..(at + 16 * 1024).min(sent.len())]).unwrap();
            }
            links.pump();
            while let Ok(n) = theirs.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
        }
        assert_eq!(got.len(), sent.len(), "the new owner lost {} byte(s)", sent.len() - got.len());
        assert!(got == sent, "the new owner's bytes came out changed");
    }

    #[test]
    fn a_session_end_stops_its_hostint_and_the_next_owner_gets_every_byte() {
        a_session_ends_and_its_slot_goes_to_the_next(true, true);
    }

    #[test]
    fn the_session_end_alone_stops_its_hostint() {
        a_session_ends_and_its_slot_goes_to_the_next(true, false);
    }

    #[test]
    fn a_dead_slot_alone_stops_its_hostint() {
        a_session_ends_and_its_slot_goes_to_the_next(false, true);
    }

    /// Whatever the quarantine says, a slot a thread still reads is not
    /// given out.
    #[test]
    fn a_slot_with_a_reader_is_not_recycled() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let (mut links, comp, proxy) = rings();
        let ch = ByteChannel::claim(&proxy, with_session(OWNER_HOSTINT, 5)).unwrap();
        let slot = ch.slot();
        let reader = SlotReader::new(slot);
        comp.kill_ring(slot).unwrap();
        drop(ch);
        assert_eq!(links.reap(), 0);
        assert_eq!(comp.ring_state(slot).unwrap(), RingState::Dead);
        drop(reader);
        assert_eq!(links.reap(), 1);
        assert_eq!(comp.ring_state(slot).unwrap(), RingState::Free);
    }
}
