//! The host's side of the channel: the security boundary for everything
//! Linux asks the host to open, and the pushes of the host's audio devices,
//! handlers and path map.
//!
//! [`Service`] is the decision logic, pure and tested. [`serve`] is the loop
//! the host's long-lived process runs on its own thread (the Windows
//! compositor, the macOS main process). It owns the channel, so pushes and
//! requests never race on it, and it returns when the channel ends.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::assoc::NEVER_CLAIMED;
use crate::audio::State;
use crate::openurl::{dangerous_path, validate_host_path, validate_url, RateLimiter};
use crate::wire::{Handler, Msg, PathMap, Req, Target, VERSION};

/// What the platform does once a request has passed every check. Each call
/// gets a value the host itself produced or checked: a re-serialised URL, a
/// host path inside a share, or the host's own identity for a handler.
pub trait Opener {
    /// Open a URL with the configured or default browser.
    fn open_url(&mut self, url: &str) -> Result<(), String>;
    /// Open a host file with the host's default application for it.
    fn open_path(&mut self, path: &str) -> Result<(), String>;
    /// Open a URL or file with one application, named by the host's own
    /// identity for it (an executable path, a bundle id).
    fn open_with(&mut self, app: &str, target: &Target) -> Result<(), String>;
}

/// A handler as the host offers it, with the host's identity for it, which
/// never crosses to Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostHandler {
    pub offer: Handler,
    pub app: String,
}

pub struct Config {
    pub platform: &'static str,
    /// Schemes `url` requests may have.
    pub schemes: Vec<String>,
    pub paths: PathMap,
    /// Host paths may be opened only under these roots: the shares' host
    /// sides.
    pub windows_paths: bool,
}

pub struct Service {
    cfg: Config,
    limiter: RateLimiter,
    handlers: Vec<HostHandler>,
    last_audio: Option<String>,
    last_handlers: Option<String>,
}

impl Service {
    pub fn new(cfg: Config) -> Self {
        Self { cfg, limiter: RateLimiter::for_open(), handlers: Vec::new(), last_audio: None, last_handlers: None }
    }

    /// The lines that open the conversation: hello, the path map.
    pub fn greeting(&self) -> Vec<String> {
        vec![
            Msg::Hello { version: VERSION, platform: Some(self.cfg.platform.into()) }.encode(),
            Msg::Paths(self.cfg.paths.clone()).encode(),
        ]
    }

    fn roots(&self) -> Vec<String> {
        self.cfg.paths.shares.iter().map(|(h, _)| h.clone()).collect()
    }

    /// A host path Linux may have opened: inside a share, and not of a type
    /// whose opening runs or installs something (security-model R37).
    fn check_path(&self, p: &str) -> Result<String, String> {
        let p = validate_host_path(p, &self.roots(), self.cfg.windows_paths)?;
        if dangerous_path(&p, self.cfg.windows_paths) {
            return Err("that type of file runs or installs something, and is never opened for Linux".into());
        }
        Ok(p)
    }

    /// Check one request and, if it passes, open it. Refused requests do not
    /// count against the rate limit; the limit is checked last, before the
    /// opener runs.
    pub fn decide(&mut self, req: &Req, opener: &mut dyn Opener, now: Instant) -> Result<(), String> {
        enum Act {
            Url(String),
            Path(String),
            With(String, Target),
        }
        let act = match req {
            Req::Url(u) => Act::Url(validate_url(u, &self.cfg.schemes)?),
            Req::Path(p) => Act::Path(self.check_path(p)?),
            Req::With { token, target } => {
                let h = self.handlers.iter().find(|h| h.offer.token == *token).ok_or("no such host application")?;
                let t = match target {
                    Target::Url(u) => {
                        let allowed: Vec<String> =
                            h.offer.schemes.iter().filter(|s| !NEVER_CLAIMED.contains(&s.as_str())).cloned().collect();
                        Target::Url(validate_url(u, &allowed)?)
                    }
                    Target::HostPath(p) => {
                        Target::HostPath(self.check_path(p)?)
                    }
                };
                Act::With(h.app.clone(), t)
            }
        };
        if !self.limiter.allow(now) {
            return Err("rate limit exceeded (10 in 10 s)".into());
        }
        match act {
            Act::Url(u) => opener.open_url(&u),
            Act::Path(p) => opener.open_path(&p),
            Act::With(app, t) => opener.open_with(&app, &t),
        }
    }

    /// One line from Linux: the reply to send, if any.
    pub fn on_line(&mut self, line: &str, opener: &mut dyn Opener, now: Instant) -> Option<String> {
        match Msg::decode(line) {
            Ok(Msg::Req { n, req }) => {
                let result = self.decide(&req, opener, now);
                Some(Msg::Rep { n, result }.encode())
            }
            Ok(Msg::Hello { .. }) => None,
            // Anything else is not Linux's to send; say nothing, since a
            // reply would need a request number.
            Ok(_) | Err(_) => None,
        }
    }

    /// The audio state as a line, or `None` if Linux already has it.
    pub fn audio(&mut self, s: &State) -> Option<String> {
        let l = Msg::Audio(s.clone()).encode();
        (self.last_audio.as_ref() != Some(&l)).then(|| {
            self.last_audio = Some(l.clone());
            l
        })
    }

    /// New handlers: kept for `with` requests, and the offer as a line if it
    /// changed.
    pub fn handlers(&mut self, hs: Vec<HostHandler>) -> Option<String> {
        let l = Msg::Handlers(hs.iter().map(|h| h.offer.clone()).collect()).encode();
        self.handlers = hs;
        (self.last_handlers.as_ref() != Some(&l)).then(|| {
            self.last_handlers = Some(l.clone());
            l
        })
    }

    /// Forget what was sent, so a reconnecting Linux side gets it all again.
    pub fn resend_all(&mut self) {
        self.last_audio = None;
        self.last_handlers = None;
    }
}

/// What the I/O loop waits on.
pub enum Poll {
    Line(String),
    /// Nothing within the timeout.
    Idle,
    /// The channel ended (EOF, or the peer is gone).
    Closed,
}

/// The channel, as the host process provides it. `poll_line` must wait on
/// the doorbell (not spin) for at most `timeout`.
pub trait Link {
    fn poll_line(&mut self, timeout: Duration) -> Poll;
    fn send_line(&mut self, line: &str) -> bool;
}

/// Something the host's other threads tell the service.
pub enum Event {
    Audio(State),
    Handlers(Vec<HostHandler>),
    Stop,
}

/// Run the service until the channel closes or [`Event::Stop`]. Pushes are
/// sent within `tick` of arriving. Logs one line per request through `log`.
pub fn serve(link: &mut dyn Link, svc: &mut Service, opener: &mut dyn Opener, events: &Receiver<Event>, tick: Duration, log: &mut dyn FnMut(&str)) {
    for l in svc.greeting() {
        if !link.send_line(&l) {
            return;
        }
    }
    loop {
        loop {
            let line = match events.try_recv() {
                Ok(Event::Audio(s)) => svc.audio(&s),
                Ok(Event::Handlers(h)) => svc.handlers(h),
                Ok(Event::Stop) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => break,
            };
            if let Some(l) = line {
                if !link.send_line(&l) {
                    return;
                }
            }
        }
        match link.poll_line(tick) {
            Poll::Closed => return,
            Poll::Idle => {}
            Poll::Line(l) => {
                if let Ok(Msg::Hello { .. }) = Msg::decode(&l) {
                    // Linux (re)started its side: send everything again.
                    svc.resend_all();
                    for g in svc.greeting() {
                        link.send_line(&g);
                    }
                    continue;
                }
                let reply = svc.on_line(&l, opener, Instant::now());
                // Debug-format so control characters cannot forge log lines.
                log(&format!("hostint: {:?} -> {:?}", l.chars().take(200).collect::<String>(), reply));
                if let Some(r) = reply {
                    if !link.send_line(&r) {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::b64;

    #[derive(Default)]
    struct Rec(Vec<String>);
    impl Opener for Rec {
        fn open_url(&mut self, u: &str) -> Result<(), String> {
            self.0.push(format!("url {u}"));
            Ok(())
        }
        fn open_path(&mut self, p: &str) -> Result<(), String> {
            self.0.push(format!("path {p}"));
            Ok(())
        }
        fn open_with(&mut self, a: &str, t: &Target) -> Result<(), String> {
            self.0.push(format!("with {a} {t:?}"));
            Ok(())
        }
    }

    fn win() -> Service {
        let mut s = Service::new(Config {
            platform: "windows",
            schemes: crate::openurl::default_schemes(),
            paths: PathMap { shares: vec![("Z:\\".into(), "/srv/winshare".into())], drop: None },
            windows_paths: true,
        });
        s.handlers(vec![HostHandler {
            offer: Handler { token: "n0".into(), name: "Notepad".into(), mimes: vec!["text/plain".into()], default_for: vec![], schemes: vec![] },
            app: "C:\\Windows\\System32\\notepad.exe".into(),
        }]);
        s
    }

    fn req(n: u64, r: Req) -> String {
        Msg::Req { n, req: r }.encode()
    }

    #[test]
    fn opens_what_passes_and_refuses_the_rest() {
        let mut s = win();
        let mut o = Rec::default();
        let t = Instant::now();
        assert_eq!(s.on_line(&req(1, Req::Url("HTTPS://Example.org".into())), &mut o, t).unwrap(), "rep 1 ok");
        assert_eq!(o.0, ["url https://example.org/"]);
        let r = s.on_line(&req(2, Req::Url("file:///C:/Windows/notepad.exe".into())), &mut o, t).unwrap();
        assert!(r.starts_with("rep 2 err "), "{r}");
        assert_eq!(s.on_line(&req(3, Req::Path("Z:\\a.txt".into())), &mut o, t).unwrap(), "rep 3 ok");
        assert!(s.on_line(&req(4, Req::Path("C:\\Windows\\win.ini".into())), &mut o, t).unwrap().starts_with("rep 4 err"));
        assert_eq!(
            s.on_line(&req(5, Req::With { token: "n0".into(), target: Target::HostPath("Z:\\b.txt".into()) }), &mut o, t).unwrap(),
            "rep 5 ok"
        );
        assert!(s.on_line(&req(6, Req::With { token: "zz".into(), target: Target::HostPath("Z:\\b.txt".into()) }), &mut o, t).unwrap().starts_with("rep 6 err"));
        // Notepad takes no URL scheme, so a URL "with" it is refused.
        assert!(s.on_line(&req(7, Req::With { token: "n0".into(), target: Target::Url("https://x/".into()) }), &mut o, t).unwrap().starts_with("rep 7 err"));
        let r = s.on_line(&req(8, Req::Path("Z:\\setup.exe".into())), &mut o, t).unwrap();
        assert!(r.starts_with("rep 8 err"), "{r}");
        assert_eq!(o.0.len(), 3);
        assert_eq!(o.0[2], "with C:\\Windows\\System32\\notepad.exe HostPath(\"Z:\\\\b.txt\")");
        // Lines Linux has no business sending get no answer.
        assert_eq!(s.on_line(&format!("audio {}", b64(b"")), &mut o, t), None);
        assert_eq!(s.on_line("run firefox", &mut o, t), None);
    }

    #[test]
    fn the_rate_limit_counts_only_what_would_open() {
        let mut s = win();
        let mut o = Rec::default();
        let t = Instant::now();
        for i in 0..20 {
            s.on_line(&req(i, Req::Url("javascript:x".into())), &mut o, t);
        }
        for i in 0..10 {
            assert!(s.on_line(&req(i, Req::Url("https://a/".into())), &mut o, t).unwrap().ends_with("ok"));
        }
        let r = s.on_line(&req(11, Req::Url("https://a/".into())), &mut o, t).unwrap();
        assert!(r.starts_with("rep 11 err"));
        assert_eq!(o.0.len(), 10);
    }

    #[test]
    fn pushes_are_deduplicated() {
        let mut s = win();
        let st = State::default();
        assert!(s.audio(&st).is_some());
        assert!(s.audio(&st).is_none());
        s.resend_all();
        assert!(s.audio(&st).is_some());
    }

    struct Script {
        input: Vec<Poll>,
        out: Vec<String>,
    }
    impl Link for Script {
        fn poll_line(&mut self, _: Duration) -> Poll {
            if self.input.is_empty() { Poll::Closed } else { self.input.remove(0) }
        }
        fn send_line(&mut self, l: &str) -> bool {
            self.out.push(l.into());
            true
        }
    }

    #[test]
    fn serve_greets_answers_and_ends_at_eof() {
        let mut s = win();
        let mut o = Rec::default();
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Event::Audio(State::default())).unwrap();
        let mut link = Script {
            input: vec![Poll::Idle, Poll::Line(req(1, Req::Url("https://a/".into()))), Poll::Line("hello 1".into())],
            out: vec![],
        };
        serve(&mut link, &mut s, &mut o, &rx, Duration::from_millis(1), &mut |_| {});
        assert!(link.out[0].starts_with("hello 1 windows"));
        assert!(link.out[1].starts_with("paths "));
        assert!(link.out[2].starts_with("audio "));
        assert_eq!(link.out[3], "rep 1 ok");
        assert!(link.out[4].starts_with("hello 1 windows"), "a Linux hello gets the greeting again");
    }
}
