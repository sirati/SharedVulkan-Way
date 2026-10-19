//! §103.14: the session's control and sound channels, as byte channels of the
//! ring region the macOS guest keeps in its own RAM. Nothing here is a socket
//! to the guest, a forward or ssh.
//!
//! **Control.** The guest's main process writes one request per line: `run`,
//! `action`, `open` and `down`, in exactly the words `wc-session` takes from a
//! forced command, plus `ping` every half second. Each request is run through
//! `wc-session` itself, with the words as `SSH_ORIGINAL_COMMAND` and this
//! session's id put first, so the validation that guards the ssh path (§94: the
//! guest names an application, never a command) is the same code here. The
//! reply is one line, `ok …` or `err …`.
//!
//! **Liveness.** The ring region and the arena are guest RAM. When the guest's
//! main process dies, macOS frees and reuses those pages, and a proxy still
//! writing pixels or ring bytes into them would corrupt whatever macOS put
//! there. So the pings are a lease: after one has been seen, five seconds
//! without one end this process at once, before it touches the region again,
//! and the session is taken down.
//!
//! **Sound.** The session's PulseAudio monitor (`wc-session`'s `audio.sock`,
//! s16le 48 kHz stereo) is copied into a channel the guest plays from.

use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sv_transport::control::Control;
use sv_transport::wlbytes::{ByteChannel, ReadOutcome, OWNER_AUDIO, OWNER_CONTROL};

/// What `wc-session`'s `proxy` tells this process (`--session-control`,
/// `--session-id`, `--audio-socket`).
#[derive(Clone, Debug, Default)]
pub struct Services {
    pub session: Option<PathBuf>,
    pub id: Option<String>,
    pub audio: Option<PathBuf>,
}

static LAST_PING: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Start the services the arguments name. Each runs on a thread of its own.
pub fn start(control: &Arc<Control>, svc: &Services) {
    if let (Some(session), Some(id)) = (svc.session.clone(), svc.id.clone()) {
        match ByteChannel::claim(control, OWNER_CONTROL) {
            Ok(ch) => {
                println!("[proxy] session control on ring slot {} (no ssh)", ch.slot());
                let c = Arc::clone(control);
                std::thread::Builder::new()
                    .name("session-control".into())
                    .spawn(move || serve_control(c, ch, session.clone(), id.clone()))
                    .ok();
                std::thread::Builder::new().name("lease".into()).spawn(watch_lease).ok();
            }
            Err(e) => println!("[proxy] no session control ring: {e}"),
        }
    }
    if let Some(sock) = svc.audio.clone() {
        match ByteChannel::claim(control, OWNER_AUDIO) {
            Ok(ch) => {
                println!("[proxy] session sound on ring slot {} (no ssh)", ch.slot());
                let c = Arc::clone(control);
                std::thread::Builder::new().name("session-sound".into()).spawn(move || serve_audio(c, ch, sock)).ok();
            }
            Err(e) => println!("[proxy] no session sound ring: {e}"),
        }
    }
}

/// Read lines from a channel, polling when idle (the guest has no doorbell).
struct Lines {
    ch: ByteChannel,
    buf: Vec<u8>,
}

impl Lines {
    fn next(&mut self) -> Option<String> {
        let mut tmp = [0u8; 4096];
        loop {
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                return Some(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
            }
            match self.ch.read(&mut tmp) {
                Ok(ReadOutcome::Bytes(n)) => self.buf.extend_from_slice(&tmp[..n]),
                Ok(ReadOutcome::Eof) | Err(_) => return None,
                Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }
}

fn write_all(ch: &mut ByteChannel, mut b: &[u8]) -> bool {
    let t0 = Instant::now();
    while !b.is_empty() {
        match ch.write(b) {
            Ok(n) if n > 0 => b = &b[n..],
            Ok(_) if t0.elapsed() < Duration::from_secs(2) => std::thread::sleep(Duration::from_millis(1)),
            _ => return false,
        }
    }
    true
}

/// The words a request may have, checked again by `wc-session` itself.
fn words_ok(line: &str) -> bool {
    !line.is_empty()
        && line.len() < 16 * 1024
        && line.chars().all(|c| c.is_ascii_alphanumeric() || " +/=._-".contains(c))
}

fn serve_control(_control: Arc<Control>, ch: ByteChannel, session: PathBuf, id: String) {
    let mut lines = Lines { ch, buf: Vec::new() };
    while let Some(line) = lines.next() {
        if line == "ping" {
            LAST_PING.store(now_ms(), Ordering::Relaxed);
            continue;
        }
        let reply = if !words_ok(&line) {
            "err refused: a request is words of [A-Za-z0-9 +/=._-]".to_string()
        } else {
            // The verb and this session's id, then the guest's words: the
            // guest cannot name another session.
            let mut w = line.split_whitespace();
            let verb = w.next().unwrap_or("");
            let rest: Vec<&str> = w.collect();
            let cmd = std::iter::once(verb).chain(std::iter::once(id.as_str())).chain(rest).collect::<Vec<_>>().join(" ");
            let t0 = Instant::now();
            match std::process::Command::new("bash").arg(&session).env("SSH_ORIGINAL_COMMAND", &cmd).output() {
                Ok(o) if o.status.success() => {
                    format!("ok {} ({} ms)", String::from_utf8_lossy(&o.stdout).trim().replace('\n', "; "), t0.elapsed().as_millis())
                }
                Ok(o) => format!("err {}", String::from_utf8_lossy(&o.stderr).trim().replace('\n', "; ")),
                Err(e) => format!("err {e}"),
            }
        };
        println!("[proxy] session control: {line:?} -> {reply}");
        if !write_all(&mut lines.ch, format!("{reply}\n").as_bytes()) {
            break;
        }
        if line.starts_with("down") {
            break;
        }
    }
}

/// Five seconds without a ping, once pings have started, end the process:
/// the guest's pages are no longer the session's.
fn watch_lease() {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let last = LAST_PING.load(Ordering::Relaxed);
        if last != 0 && now_ms().saturating_sub(last) > 5000 {
            println!(
                "[proxy] LEASE LOST: no ping from the guest's main process for 5 s; its RAM may \
                 already be reused, so this proxy stops before touching it again"
            );
            std::process::exit(4);
        }
    }
}

fn serve_audio(_control: Arc<Control>, mut ch: ByteChannel, sock: PathBuf) {
    let mut buf = vec![0u8; 3840];
    let mut dropped = 0u64;
    loop {
        let Ok(s) = std::os::unix::net::UnixStream::connect(&sock) else {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        };
        let mut r = std::io::BufReader::new(s);
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // A full ring means the guest is not playing: drop rather
                    // than stall the monitor, as the guest's own queue does.
                    if ch.free_bytes().map_or(0, |f| f as usize) < n {
                        dropped += 1;
                        if dropped.is_power_of_two() {
                            println!("[proxy] session sound: {dropped} chunk(s) dropped on a full ring");
                        }
                        continue;
                    }
                    if !write_all(&mut ch, &buf[..n]) {
                        return;
                    }
                }
            }
        }
        let _ = r.fill_buf();
    }
}
