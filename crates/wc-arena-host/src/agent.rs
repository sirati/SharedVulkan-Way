//! `wc-arena-host agent` (waycompose-design.md §111.10): the mapped user's
//! half of the session service.
//!
//! The session service (`sessiond --users`) runs as a system account of its
//! own and maps each boot record's identity to a Linux user. It does not
//! start anything as that user itself: it connects to the user's agent, a
//! socket the user's own systemd manager listens on (`Accept=yes`, one
//! instance per session), and asks it to start the session. So the proxy, the
//! applications, the sound and host integration run in that user's manager,
//! with their home, groups and session, and no polkit rule or root helper is
//! involved anywhere.
//!
//! Both ends check who the other is with `SO_PEERCRED`: the service that the
//! socket belongs to the mapped user (its manager made it), the agent that the
//! caller is the service's account or root.
//!
//! ```text
//! service -> agent   start INSTALL ID RUNID INDEX GPU ARENA-REGION GPU-REGION
//! agent -> service   ok MESSAGE | err MESSAGE      (after up, proxy and gpu)
//! agent -> service   ended MESSAGE                 (after down, when the proxy unit ended)
//! ```
//!
//! `ARENA-REGION` and `GPU-REGION` are the broker regions the service
//! published for this user (`-`: none). The session's processes open them, and
//! the doorbell hub's services, through the broker (`WC_BROKER`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wc_handle::macboot;

fn say(msg: &str) {
    eprintln!("wc-session-agent: {msg}");
}

/// What the service asks the agent to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Start {
    pub install: String,
    pub id: String,
    pub run: u64,
    pub index: u8,
    pub gpu: bool,
    pub arena: Option<u64>,
    pub gpu_region: Option<u64>,
}

impl Start {
    pub fn line(&self) -> String {
        let r = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |v| v.to_string());
        format!(
            "start {} {} 0x{:016x} {} {} {} {}",
            self.install,
            self.id,
            self.run,
            self.index,
            u8::from(self.gpu),
            r(self.arena),
            r(self.gpu_region)
        )
    }

    /// The service's line, checked as untrusted input: it is a line on a
    /// socket, whoever the socket's peer turned out to be.
    pub fn parse(line: &str) -> Result<Start, String> {
        let w: Vec<&str> = line.trim_end().split(' ').collect();
        let [verb, install, id, run, index, gpu, arena, gpu_region] = w[..] else {
            return Err("start INSTALL ID RUNID INDEX GPU ARENA-REGION GPU-REGION".into());
        };
        if verb != "start" {
            return Err(format!("unknown request {verb:?}"));
        }
        if !macboot::valid_installation(install) || !macboot::valid_id(id) {
            return Err(format!("bad installation {install:?} or session id {id:?}"));
        }
        let run = run
            .strip_prefix("0x")
            .filter(|h| !h.is_empty() && h.len() <= 16)
            .and_then(|h| u64::from_str_radix(h, 16).ok())
            .ok_or("bad run id")?;
        let region = |s: &str| -> Result<Option<u64>, String> {
            if s == "-" { Ok(None) } else { s.parse().map(Some).map_err(|_| format!("bad region {s:?}")) }
        };
        Ok(Start {
            install: install.into(),
            id: id.into(),
            run,
            index: index.parse().map_err(|_| "the index is 0..=255")?,
            gpu: match gpu {
                "0" => false,
                "1" => true,
                _ => return Err("gpu is 0 or 1".into()),
            },
            arena: region(arena)?,
            gpu_region: region(gpu_region)?,
        })
    }
}

/// The environment a session's `wc-session` verbs run with.
pub fn session_env(s: &Start, broker: Option<&str>) -> Vec<(&'static str, String)> {
    let mut env = vec![("WC_LAUNCH_SERVICE", "1".to_string()), ("WC_LAUNCH_SESSION_INDEX", s.index.to_string())];
    if let (Some(b), Some(a)) = (broker, s.arena) {
        env.push(("WC_BROKER", b.to_string()));
        env.push(("WC_LAUNCH_BROKER_ARENA", a.to_string()));
        if let Some(g) = s.gpu_region {
            env.push(("WC_LAUNCH_BROKER_GPU", g.to_string()));
        }
    }
    env
}

/// §105.13's start, the same wherever it runs: `up`, then `gpu` beside
/// `proxy`. On failure the session is taken down again.
pub fn start_session(ws: &Path, s: &Start, env: &[(&'static str, String)]) -> Result<String, String> {
    let id = s.id.as_str();
    let run = format!("0x{:016x}", s.run);
    let t_up = Instant::now();
    let up = crate::sessiond::verb_env(ws, &["up", id, &run, "ivshmem"], env);
    let d_up = t_up.elapsed();
    let gpu = (s.gpu && up.is_ok()).then(|| {
        let (ws, id, env) = (ws.to_path_buf(), id.to_string(), env.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>());
        std::thread::spawn(move || {
            let t = Instant::now();
            (crate::sessiond::verb_env(&ws, &["gpu", &id], &env), t.elapsed())
        })
    });
    let t_proxy = Instant::now();
    let started = up.and_then(|u| Ok(format!("{u}; {}", crate::sessiond::verb_env(ws, &["proxy", id], env)?)));
    let d_proxy = t_proxy.elapsed();
    let gpu = gpu.and_then(|h| h.join().ok());
    let mut msg = match started {
        Ok(t) => t,
        Err(e) => {
            let _ = crate::sessiond::verb_env(ws, &["down", id], env);
            return Err(e);
        }
    };
    let mut d_gpu = Duration::ZERO;
    if let Some((r, d)) = gpu {
        d_gpu = d;
        match r {
            Ok(g) => msg = format!("{msg}; {g}"),
            // Not fatal, as on every path: the application renders in software.
            Err(e) => msg = format!("{msg}; NO GPU: {e}"),
        }
    }
    say(&format!("session {id}: up {d_up:?}, proxy {d_proxy:?}, gpu {d_gpu:?} (beside the proxy)"));
    Ok(msg)
}

struct Args {
    installs: PathBuf,
    daemon_user: Option<String>,
    broker: Option<String>,
    listen: Option<PathBuf>,
}

fn parse(argv: &[String]) -> Result<Args, String> {
    let mut a = Args { installs: "/var/lib/waycompose/installs".into(), daemon_user: None, broker: None, listen: None };
    let mut it = argv.iter();
    while let Some(k) = it.next() {
        let mut v = || it.next().cloned().ok_or(format!("{k} needs a value"));
        match k.as_str() {
            "--installs" => a.installs = v()?.into(),
            "--daemon-user" => a.daemon_user = Some(v()?),
            "--broker" => a.broker = Some(v()?),
            // Without systemd's socket activation (the harnesses): accept here.
            "--listen" => a.listen = Some(v()?.into()),
            _ => return Err(format!("unknown argument {k}")),
        }
    }
    Ok(a)
}

/// The uid of account `name`.
pub fn uid_of(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: live out-parameters and a live, sized buffer.
    let r = unsafe { libc::getpwnam_r(c.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    (r == 0 && !out.is_null()).then_some(pw.pw_uid)
}

/// Serve one connection from the service.
fn serve(conn: UnixStream, a: &Args) {
    // SAFETY: no preconditions.
    let me = unsafe { libc::geteuid() };
    let peer = match wc_broker::sys::unix::peer_of(&conn) {
        Ok((_, uid)) => uid,
        Err(e) => {
            say(&format!("cannot tell who connected: {e}"));
            return;
        }
    };
    let allowed = peer == 0 || a.daemon_user.as_deref().map_or(peer == me, |u| uid_of(u) == Some(peer));
    let mut w = match conn.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    if !allowed {
        say(&format!("refused a connection from uid {peer}: not the session service's account"));
        let _ = writeln!(w, "err uid {peer} is not the session service");
        return;
    }
    let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
    let mut line = String::new();
    if BufReader::new(std::io::Read::take(&conn, 4096)).read_line(&mut line).is_err() {
        return;
    }
    let start = match Start::parse(&line) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(w, "err {e}");
            return;
        }
    };
    let install = match crate::sessiond_ivshmem::lookup(&a.installs, &start.install) {
        Ok(i) => i,
        Err(e) => {
            let _ = writeln!(w, "err {e}");
            return;
        }
    };
    let ws = install.session();
    let mut env = session_env(&start, a.broker.as_deref());
    // The session's run directories in this user's runtime directory (0700,
    // theirs), not in a /tmp name another user could have made first.
    if std::env::var_os("WC_LAUNCH_RUN").is_none()
        && let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR")
    {
        env.push(("WC_LAUNCH_RUN", format!("{}/wc-launch", rt.to_string_lossy())));
    }
    if a.broker.is_none() || start.arena.is_none() {
        say(&format!("session {}: NO BROKER: its processes open the devices by path", start.id));
    }
    say(&format!("session {} of installation {} as uid {me}", start.id, install.name));
    match start_session(&ws, &start, &env) {
        Ok(msg) => {
            let _ = writeln!(w, "ok {}", msg.replace('\n', "; "));
        }
        Err(e) => {
            say(&format!("session {}: FAILED: {e}", start.id));
            let _ = writeln!(w, "err {}", e.replace('\n', "; "));
            return;
        }
    }
    // The session lives as long as its proxy unit; the service hears when it
    // ends, and if the service is gone meanwhile the session is still ended
    // here, as the user's.
    while crate::sessiond::unit_active(&start.id) {
        std::thread::sleep(Duration::from_secs(2));
    }
    let r = crate::sessiond::verb_env(&ws, &["down", &start.id], &env);
    say(&format!("session {} ended: {r:?}", start.id));
    let _ = writeln!(w, "ended {}", r.unwrap_or_else(|e| e).replace('\n', "; "));
}

pub fn main(argv: &[String]) -> std::process::ExitCode {
    let a = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("wc-arena-host agent: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    // SAFETY: no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("wc-arena-host agent runs as the user whose sessions it starts; refusing to run as root");
        return std::process::ExitCode::from(2);
    }
    match &a.listen {
        None => {
            // systemd's Accept=yes: the connection is stdin.
            use std::os::fd::FromRawFd;
            // SAFETY: fd 0 is the accepted socket, and this process owns it.
            let conn = unsafe { UnixStream::from_raw_fd(0) };
            serve(conn, &a);
        }
        Some(p) => {
            let _ = std::fs::remove_file(p);
            let l = match std::os::unix::net::UnixListener::bind(p) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("wc-arena-host agent: {}: {e}", p.display());
                    return std::process::ExitCode::from(1);
                }
            };
            let a = std::sync::Arc::new(a);
            for c in l.incoming().flatten() {
                let a = std::sync::Arc::clone(&a);
                std::thread::spawn(move || serve(c, &a));
            }
        }
    }
    std::process::ExitCode::SUCCESS
}

// ------------------------------------------------------------ service side

/// The service's end: connect to `user`'s agent at `sock`, check that the
/// socket is that user's, and ask it to start the session. Returns the
/// agent's message and the connection, on which `ended` arrives later.
pub fn call(sock: &Path, uid: u32, start: &Start) -> Result<(String, BufReader<UnixStream>), String> {
    let s = UnixStream::connect(sock).map_err(|e| {
        format!("the Linux user's session agent at {} is not there ({e}): is its manager running (linger)?", sock.display())
    })?;
    let (_, owner) = wc_broker::sys::unix::peer_of(&s).map_err(|e| format!("{}: {e}", sock.display()))?;
    if owner != uid {
        return Err(format!("{} is uid {owner}'s socket, not uid {uid}'s: refusing to start a session through it", sock.display()));
    }
    let _ = s.set_read_timeout(Some(Duration::from_secs(120)));
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{}", start.line()).map_err(|e| format!("{}: {e}", sock.display()))?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).map_err(|e| format!("the session agent did not answer: {e}"))?;
    let line = line.trim_end();
    if let Some(m) = line.strip_prefix("ok ") {
        let _ = r.get_ref().set_read_timeout(None);
        return Ok((m.to_string(), r));
    }
    Err(line.strip_prefix("err ").unwrap_or(line).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_line_round_trips_and_bad_ones_are_refused() {
        let s = Start {
            install: "wcivs".into(),
            id: "wcivs-9".into(),
            run: 0xAB,
            index: 3,
            gpu: true,
            arena: Some(4),
            gpu_region: None,
        };
        assert_eq!(Start::parse(&s.line()), Ok(s.clone()));
        for bad in [
            "",
            "start wcivs wcivs-9 0xab 3 1 4",
            "start ../x wcivs-9 0xab 3 1 4 -",
            "start wcivs wcivs-9 ab 3 1 4 -",
            "start wcivs wcivs-9 0xab 300 1 4 -",
            "start wcivs wcivs-9 0xab 3 2 4 -",
            "start wcivs wcivs-9 0xab 3 1 x -",
            "run wcivs wcivs-9 0xab 3 1 4 -",
        ] {
            assert!(Start::parse(bad).is_err(), "{bad:?}");
        }
        let env = session_env(&s, Some("/run/b.sock"));
        assert!(env.contains(&("WC_LAUNCH_BROKER_ARENA", "4".into())));
        assert!(!env.iter().any(|(k, _)| *k == "WC_LAUNCH_BROKER_GPU"));
        assert!(!session_env(&s, None).iter().any(|(k, _)| *k == "WC_BROKER"), "no broker: the old paths");
    }
}
