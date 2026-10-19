//! §106: the host-integration channel on the host. The `OWNER_HOSTINT` ring
//! slot, as a [`wc_hostint::host::Link`], and the thread that serves it.
//!
//! The process that owns the session's rings calls [`spawn`] once it has the
//! slot: on Windows the compositor (the only process that can map the
//! ivshmem device), on macOS the main process. The thread is its own, never
//! the Wayland thread, and it ends when the ring reaches EOF or the peer
//! dies. Requests are checked by `wc_hostint::host::Service` before the
//! platform's opener runs (security-model.md, "Opening links on the host").

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use sv_transport::wlbytes::{ByteChannel, ReadOutcome};
use wc_hostint::host::{Event, Link, Opener, Poll, Service};
use wc_hostint::wire::LineBuf;

/// How a reader sleeps until the ring has bytes, for at most the given time.
/// It must wait on the doorbell (`ByteChannel::wait_readable` with
/// `ringlink::ring_wait_slice()` once the rings have one), not spin.
pub type Wait = Box<dyn FnMut(&ByteChannel, Duration) + Send>;

pub struct RingLink {
    ch: ByteChannel,
    lines: LineBuf,
    wait: Wait,
}

impl RingLink {
    pub fn new(ch: ByteChannel, wait: Wait) -> Self {
        Self { ch, lines: LineBuf::default(), wait }
    }
}

impl Link for RingLink {
    fn poll_line(&mut self, timeout: Duration) -> Poll {
        let deadline = Instant::now() + timeout;
        let mut tmp = [0u8; 4096];
        loop {
            if let Some(l) = self.lines.next_line() {
                return Poll::Line(l);
            }
            match self.ch.read(&mut tmp) {
                Ok(ReadOutcome::Bytes(n)) => self.lines.push(&tmp[..n]),
                Ok(ReadOutcome::Eof) | Err(_) => return Poll::Closed,
                Ok(ReadOutcome::Empty) => {
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
        log(&format!("hostint: serving ring slot {}", ch.slot()));
        let mut link = RingLink::new(ch, wait);
        wc_hostint::host::serve(&mut link, &mut svc, opener.as_mut(), &events, Duration::from_millis(250), &mut *log);
        log("hostint: the channel ended");
    })
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
/// association handlers. Returns once the thread runs.
#[cfg(windows)]
pub fn start_windows(ch: ByteChannel, cfg: HostCfg, say: fn(&str)) {
    use wc_hostplat::win;
    let (tx, rx) = std::sync::mpsc::channel();
    // The audio endpoints are not this channel's: they are the sound ring's
    // device side (§103.16).
    let htx = tx.clone();
    let _ = std::thread::Builder::new().name("hostint-handlers".into()).spawn(move || {
        loop {
            if htx.send(Event::Handlers(win::handlers::handlers())).is_err() {
                return;
            }
            std::thread::sleep(HANDLERS_EVERY);
        }
    });
    drop(tx);
    let svc = service("windows", &cfg, true);
    let opener = win::open::WinOpener { browser: cfg.browser.clone(), browser_args: cfg.browser_args.clone() };
    if let Err(e) = spawn(ch, doorbell_wait(), svc, Box::new(opener), rx, Box::new(move |l: &str| say(l))) {
        say(&format!("hostint: no thread: {e}"));
    }
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
