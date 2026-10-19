//! An `Opener` that never holds the host-integration serve loop (§106).
//!
//! On macOS an open can wait on the user: `/usr/bin/open` of a file in the
//! virtio-fs share, or the first one after a reinstall, sits until the
//! "would like to access files on a network volume" dialog is answered. The
//! serve loop answers Linux's requests one after another, so an open run on
//! it would leave every later request (and the session's other host
//! integration) unanswered for as long as the dialog stays up (measured:
//! "no answer from the host in 5 s").
//!
//! [`Bounded`] runs each open on a worker thread and waits at most `wait`
//! for its outcome. If it ends in time, that outcome is the reply. If not,
//! the open keeps going on its worker (it completes when the user answers)
//! and the reply is what `on_timeout` says: on macOS, "the Mac is asking for
//! permission; answer the dialog" while a TCC dialog is on screen, and
//! "ok" otherwise (`open` was started and is still launching the
//! application). Pure: the platform's opener and its dialog check are
//! given, so this is tested on every target.

use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use wc_hostint::host::Opener;
use wc_hostint::wire::Target;

/// What to answer when an open has not ended within the wait.
pub type OnTimeout = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// How long an open may take before the serve loop answers without it.
pub const WAIT: Duration = Duration::from_secs(2);

/// The reply while macOS shows a permission dialog for the open.
pub const ASKING: &str = "the Mac is asking for permission; answer the dialog";

pub struct Bounded<O> {
    inner: O,
    wait: Duration,
    on_timeout: OnTimeout,
}

impl<O: Opener + Clone + Send + 'static> Bounded<O> {
    pub fn new(inner: O, wait: Duration, on_timeout: OnTimeout) -> Self {
        Bounded { inner, wait, on_timeout }
    }

    fn run(&self, f: impl FnOnce(&mut O) -> Result<(), String> + Send + 'static) -> Result<(), String> {
        let (tx, rx) = sync_channel(1);
        let mut o = self.inner.clone();
        let spawned = std::thread::Builder::new().name("hostint-open".into()).spawn(move || {
            let _ = tx.send(f(&mut o));
        });
        if let Err(e) = spawned {
            return Err(format!("no thread to open it on: {e}"));
        }
        match rx.recv_timeout(self.wait) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => (self.on_timeout)(),
            Err(RecvTimeoutError::Disconnected) => Err("the open failed".into()),
        }
    }
}

impl<O: Opener + Clone + Send + 'static> Opener for Bounded<O> {
    fn open_url(&mut self, url: &str) -> Result<(), String> {
        let u = url.to_string();
        self.run(move |o| o.open_url(&u))
    }
    fn open_path(&mut self, path: &str) -> Result<(), String> {
        let p = path.to_string();
        self.run(move |o| o.open_path(&p))
    }
    fn open_with(&mut self, app: &str, target: &Target) -> Result<(), String> {
        let (a, t) = (app.to_string(), target.clone());
        self.run(move |o| o.open_with(&a, &t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;
    use wc_hostint::host::{Config, Service};
    use wc_hostint::wire::{Msg, PathMap, Req};

    /// Blocks on paths ("a dialog is up") and opens URLs at once.
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Vec<String>>>);
    impl Opener for Fake {
        fn open_url(&mut self, u: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("url {u}"));
            Ok(())
        }
        fn open_path(&mut self, p: &str) -> Result<(), String> {
            std::thread::sleep(Duration::from_secs(3));
            self.0.lock().unwrap().push(format!("path {p}"));
            Ok(())
        }
        fn open_with(&mut self, _: &str, _: &Target) -> Result<(), String> {
            Err("refused by the platform".into())
        }
    }

    fn line(n: u64, req: Req) -> String {
        Msg::Req { n, req }.encode()
    }

    #[test]
    fn a_blocked_open_does_not_hold_the_next_request() {
        let fake = Fake::default();
        let asking: OnTimeout = Arc::new(|| Err(ASKING.into()));
        let mut o = Bounded::new(fake.clone(), Duration::from_millis(200), asking);
        let mut svc = Service::new(Config {
            platform: "macos",
            schemes: wc_hostint::openurl::default_schemes(),
            paths: PathMap { shares: vec![("/Users/q/Share".into(), "/srv/share".into())], drop: None },
            windows_paths: false,
        });
        let t0 = Instant::now();
        let r1 = svc.on_line(&line(1, Req::Path("/Users/q/Share/a.txt".into())), &mut o, Instant::now()).unwrap();
        assert!(t0.elapsed() < Duration::from_secs(1), "answered within the wait, not after the open");
        assert!(r1.starts_with("rep 1 err "), "{r1}");
        assert_eq!(Msg::decode(&r1).unwrap(), Msg::Rep { n: 1, result: Err(ASKING.into()) });
        // The next request is answered at once, while the first still waits.
        let r2 = svc.on_line(&line(2, Req::Url("https://example.org/".into())), &mut o, Instant::now()).unwrap();
        assert_eq!(r2, "rep 2 ok");
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert_eq!(*fake.0.lock().unwrap(), ["url https://example.org/"]);
        // An outcome that arrives in time is the reply.
        let mut o2 = Bounded::new(fake.clone(), Duration::from_secs(2), Arc::new(|| Ok(())));
        assert_eq!(o2.open_with("x", &Target::Url("https://a/".into())), Err("refused by the platform".into()));
        // The blocked open completes on its worker.
        std::thread::sleep(Duration::from_millis(3200));
        assert!(fake.0.lock().unwrap().contains(&"path /Users/q/Share/a.txt".to_string()));
    }

    #[test]
    fn a_slow_open_with_no_dialog_counts_as_started() {
        let mut o = Bounded::new(Fake::default(), Duration::from_millis(100), Arc::new(|| Ok(())));
        assert_eq!(o.open_path("/x"), Ok(()));
    }
}
