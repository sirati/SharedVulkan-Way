//! §105.13: the control ring's requests answered in this process, in
//! microseconds, instead of a `wc-session` process each.
//!
//! Until §105.13 every line on the control ring became `bash wc-session`
//! with the words in `SSH_ORIGINAL_COMMAND`: 15–25 ms of process creation,
//! a shell and its `od`s, for a reply. The user's bound is a mean below
//! 0.2 ms. So this process answers what it can itself:
//!
//! * `rtt`: nothing, at once (the launcher's `--ping`);
//! * `status`: whether this session's proxy socket is there, as `wc-session
//!   status` says it;
//! * `app`, `icon`, `frequent`: from a cache this process keeps warm. At its
//!   start a thread asks `wc-session` for every application of the
//!   installation's `apps.ini` (both themes), and an answer older than
//!   [`STALE`] is refreshed in the background after it was served. A miss is
//!   asked synchronously, once, and cached;
//! * `run`, `action`, `open`, `gpu`: checked here (the application is a
//!   section of `apps.ini`, an action is one its desktop entry lists, the two
//!   numbers are numbers in range), answered, and **then** started: the
//!   `wc-session` that starts the process runs on a thread of its own and
//!   never sits in the reply's round trip. `wc-session` checks every word
//!   again (§94); what it says goes to this process's log.
//!
//! Everything else goes to `wc-session` as before.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// How old a cached answer may be before it is refreshed (after serving it).
pub const STALE: Duration = Duration::from_secs(30);

struct Entry {
    out: Vec<u8>,
    at: Instant,
}

/// The fast half of the control ring.
pub struct Fast {
    session: PathBuf,
    id: String,
    cache: Mutex<HashMap<String, Entry>>,
    refreshing: Mutex<HashSet<String>>,
    apps: Mutex<(Option<SystemTime>, Vec<String>)>,
}

/// What a request becomes.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// The reply's body, now.
    Now(Vec<u8>),
    /// Not this module's: run `wc-session` and wait for it.
    Slow,
    /// Refused here, with why.
    Refused(String),
}

impl Fast {
    pub fn new(session: PathBuf, id: String) -> Arc<Fast> {
        Arc::new(Fast {
            session,
            id,
            cache: Mutex::new(HashMap::new()),
            refreshing: Mutex::new(HashSet::new()),
            apps: Mutex::new((None, Vec::new())),
        })
    }

    fn base(&self) -> PathBuf {
        self.session.parent().map(Path::to_path_buf).unwrap_or_default()
    }

    /// The `[sections]` of the installation's `apps.ini`, re-read when it
    /// changes (a deploy writes it).
    pub fn apps(&self) -> Vec<String> {
        let path = self.base().join("apps.ini");
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut g = self.apps.lock().unwrap_or_else(|e| e.into_inner());
        if g.0 != mtime || mtime.is_none() {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            g.1 = sections(&text);
            g.0 = mtime;
        }
        g.1.clone()
    }

    /// Fill the cache with every application's answers, on a thread.
    pub fn prefetch(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new().name("control-prefetch".into()).spawn(move || {
            let t0 = Instant::now();
            let mut n = 0;
            for app in me.apps() {
                for words in [format!("app {app} light"), format!("app {app} dark"), format!("frequent {app}"), format!("icon {app}")] {
                    if let Ok(out) = me.run_sync(&words) {
                        me.store(&words, out);
                        n += 1;
                    }
                }
            }
            println!("[proxy] session control: {n} answer(s) cached in {:?}", t0.elapsed());
        });
    }

    pub fn store(&self, words: &str, out: Vec<u8>) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).insert(words.to_string(), Entry { out, at: Instant::now() });
    }

    /// `wc-session` for `words`, as the ring's slow path runs it.
    pub fn run_sync(&self, words: &str) -> Result<Vec<u8>, String> {
        let cmd = crate::macsvc::command_words(words, &self.id);
        let o = std::process::Command::new("bash")
            .arg(&self.session)
            .env("SSH_ORIGINAL_COMMAND", &cmd)
            .output()
            .map_err(|e| e.to_string())?;
        if o.status.success() {
            Ok(o.stdout)
        } else {
            Err(String::from_utf8_lossy(&o.stderr).trim().replace('\n', "; "))
        }
    }

    fn refresh_later(self: &Arc<Self>, words: String) {
        {
            let mut r = self.refreshing.lock().unwrap_or_else(|e| e.into_inner());
            if !r.insert(words.clone()) {
                return;
            }
        }
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new().name("control-refresh".into()).spawn(move || {
            if let Ok(out) = me.run_sync(&words) {
                me.store(&words, out);
            }
            me.refreshing.lock().unwrap_or_else(|e| e.into_inner()).remove(&words);
        });
    }

    fn start_later(self: &Arc<Self>, words: String) {
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new().name("control-start".into()).spawn(move || {
            let t0 = Instant::now();
            match me.run_sync(&words) {
                Ok(out) => println!(
                    "[proxy] session control: {:?} started in {:?}: {}",
                    shown(&words),
                    t0.elapsed(),
                    String::from_utf8_lossy(&out).trim().replace('\n', "; ")
                ),
                Err(e) => println!("[proxy] session control: {:?} FAILED after its answer: {e}", shown(&words)),
            }
        });
    }

    /// The answer to one request (already checked by `words_ok`).
    pub fn answer(self: &Arc<Self>, line: &str) -> Answer {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w.as_slice() {
            ["rtt"] => Answer::Now(Vec::new()),
            // A payload of N bytes, for measuring what an answer's size costs.
            ["rtt", n] => match n.parse::<usize>() {
                Ok(n) if n <= 1 << 20 => Answer::Now(vec![b'.'; n]),
                _ => Answer::Refused("rtt N: N is 0..=1048576".into()),
            },
            ["status"] => {
                let root = std::env::var_os("WC_LAUNCH_RUN").map(PathBuf::from).unwrap_or_else(|| "/tmp/wc-launch".into());
                let d = root.join(&self.id);
                let up = d.join("wl-proxy").exists();
                Answer::Now(format!("status {} {}\n", if up { "up" } else { "down" }, d.display()).into_bytes())
            }
            ["app" | "icon" | "frequent", ..] => {
                let hit = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(line).map(|e| (e.out.clone(), e.at.elapsed()));
                match hit {
                    Some((out, age)) => {
                        if age > STALE {
                            self.refresh_later(line.to_string());
                        }
                        Answer::Now(out)
                    }
                    None => Answer::Slow,
                }
            }
            [verb @ ("run" | "action" | "open"), app, rest @ ..] => match self.check_start(verb, app, rest) {
                Ok(()) => {
                    self.start_later(line.to_string());
                    Answer::Now(format!("{verb} accepted {app}\n").into_bytes())
                }
                Err(e) => Answer::Refused(e),
            },
            ["gpu", ..] => {
                self.start_later(line.to_string());
                Answer::Now(b"gpu accepted\n".to_vec())
            }
            _ => Answer::Slow,
        }
    }

    /// What can be refused here without `wc-session`. The application must be
    /// one of `apps.ini`'s, the numbers in `wc-session`'s ranges, and an
    /// action one the application's desktop entry lists, if its entry is
    /// cached.
    fn check_start(&self, verb: &str, app: &str, rest: &[&str]) -> Result<(), String> {
        if !self.apps().iter().any(|a| a == app) {
            return Err(format!("{verb}: no application {app} on this host"));
        }
        let (arg, nums) = match verb {
            "run" => (None, rest),
            _ => match rest.split_first() {
                Some((a, n)) => (Some(*a), n),
                None => return Err(format!("{verb}: an action or a URL is missing")),
            },
        };
        let [dpi, caption] = nums else { return Err(format!("{verb}: DPI and caption are two numbers")) };
        let num = |s: &str, lo: u32, hi: u32| s.parse::<u32>().ok().filter(|v| (lo..=hi).contains(v));
        if num(dpi, 48, 960).is_none() || num(caption, 0, 512).is_none() {
            return Err(format!("{verb}: DPI 48..960 and caption 0..512"));
        }
        if verb == "action"
            && let Some(id) = arg
        {
            let meta = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(&format!("app {app} light")).map(|e| e.out.clone());
            if let Some(m) = meta
                && !String::from_utf8_lossy(&m).lines().any(|l| l.split_whitespace().nth(1) == Some(id) && l.starts_with("action "))
            {
                return Err(format!("action: {app}'s desktop entry has no action {id}"));
            }
        }
        Ok(())
    }
}

/// `[name]` headers of an ini.
pub fn sections(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix('[')?.strip_suffix(']').map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect()
}

/// A request as the log shows it: a URL is the user's history.
pub fn shown(line: &str) -> String {
    if line.starts_with("open ") { "open …".into() } else { line.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> (PathBuf, Arc<Fast>) {
        let d = std::env::temp_dir().join(format!("wc-ctlfast-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("apps.ini"), "[firefox]\nexec = /x\n\n[vkcube]\nexec = /y\n").unwrap();
        // A wc-session that records what it was asked and answers.
        std::fs::write(
            d.join("wc-session"),
            format!("echo \"$SSH_ORIGINAL_COMMAND\" >> {}/asked\necho \"answer to $SSH_ORIGINAL_COMMAND\"\necho action new-window TmV3\n", d.display()),
        )
        .unwrap();
        let f = Fast::new(d.join("wc-session"), "wcns-1".into());
        (d, f)
    }

    #[test]
    fn rtt_and_status_never_start_a_process() {
        let (d, f) = fixture("rtt");
        assert_eq!(f.answer("rtt"), Answer::Now(Vec::new()));
        assert!(matches!(f.answer("status"), Answer::Now(b) if String::from_utf8_lossy(&b).starts_with("status down")));
        assert!(!d.join("asked").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_cached_answer_is_served_and_a_miss_is_slow() {
        let (d, f) = fixture("cache");
        assert_eq!(f.answer("app firefox light"), Answer::Slow);
        f.store("app firefox light", b"cached".to_vec());
        assert_eq!(f.answer("app firefox light"), Answer::Now(b"cached".to_vec()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_start_is_checked_here_and_answered_before_it_runs() {
        let (d, f) = fixture("start");
        assert!(matches!(f.answer("run nosuch 96 30"), Answer::Refused(e) if e.contains("no application")));
        assert!(matches!(f.answer("run firefox 96 9999"), Answer::Refused(_)));
        assert!(matches!(f.answer("action firefox 96 30"), Answer::Refused(_)), "an action without its id");
        f.store("app firefox light", b"action new-window TmV3\n".to_vec());
        assert!(matches!(f.answer("action firefox nosuch 96 30"), Answer::Refused(e) if e.contains("no action")));
        let t0 = Instant::now();
        assert_eq!(f.answer("action firefox new-window 96 30"), Answer::Now(b"action accepted firefox\n".to_vec()));
        assert!(t0.elapsed() < Duration::from_millis(5), "the answer waited for the start");
        // The start itself happens after the answer, with the session's id put in.
        let t0 = Instant::now();
        while !d.join("asked").exists() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(d.join("asked")).unwrap().trim(), "action wcns-1 firefox new-window 96 30");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn prefetch_fills_both_themes_frequent_and_icon_of_every_application() {
        let (d, f) = fixture("prefetch");
        f.prefetch();
        let t0 = Instant::now();
        while f.cache.lock().unwrap().len() < 8 && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(f.cache.lock().unwrap().len(), 8);
        assert!(matches!(f.answer("frequent vkcube"), Answer::Now(b) if String::from_utf8_lossy(&b).contains("answer to frequent vkcube")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
