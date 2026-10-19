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

/// Read lines from a channel, sleeping on its lane when idle: woken by the
/// doorbell, or after a millisecond when there is none (§103.15).
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
                Ok(_) => {
                    let _ = self.ch.wait_readable(crate::rings::service_slice());
                }
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
        // §103.15: `down` is answered here and done by the host's session
        // service, which runs `wc-session down` once this process (its unit)
        // has ended. Running it from here stopped this process's own unit
        // before the reply went out, and the guest waited a minute for it.
        // §105: `raw down` too, which is how the Windows launcher says it.
        let first = line.split_whitespace().find(|w| *w != "raw");
        if first == Some("down") {
            println!("[proxy] session control: \"down\": ending; the session service takes the session down");
            let reply: &[u8] = if line.starts_with("raw ") { b"ok-raw 0\n" } else { b"ok down\n" };
            let _ = write_all(&mut lines.ch, reply);
            std::thread::sleep(Duration::from_millis(50));
            std::process::exit(0);
        }
        let (raw, line) = match line.strip_prefix("raw ") {
            Some(rest) => (true, rest.to_string()),
            None => (false, line),
        };
        let (reply, body): (String, Vec<u8>) = if !words_ok(&line) {
            ("err refused: a request is words of [A-Za-z0-9 +/=._-]".to_string(), Vec::new())
        } else {
            let cmd = command_words(&line, &id);
            let t0 = Instant::now();
            match std::process::Command::new("bash").arg(&session).env("SSH_ORIGINAL_COMMAND", &cmd).output() {
                // §105: `raw` asks for the verb's standard output exactly, as
                // the jump list's `app` and `frequent` print several lines.
                Ok(o) if o.status.success() && raw => (format!("ok-raw {}", o.stdout.len()), o.stdout),
                Ok(o) if o.status.success() => (
                    format!("ok {} ({} ms)", String::from_utf8_lossy(&o.stdout).trim().replace('\n', "; "), t0.elapsed().as_millis()),
                    Vec::new(),
                ),
                Ok(o) => (format!("err {}", String::from_utf8_lossy(&o.stderr).trim().replace('\n', "; ")), Vec::new()),
                Err(e) => (format!("err {e}"), Vec::new()),
            }
        };
        // A page's URL is the user's history: `open` is logged by its verb.
        let shown = if line.starts_with("open ") { "open …".to_string() } else { line.clone() };
        println!("[proxy] session control: {shown:?} -> {reply}");
        let mut out = format!("{reply}\n").into_bytes();
        out.extend_from_slice(&body);
        if !write_all(&mut lines.ch, &out) {
            break;
        }
    }
}

/// The verbs that name no session (§92's jump-list data, an icon): the
/// guest's words go to `wc-session` as they are. Every other verb gets this
/// session's id put after it, so the guest cannot name another session.
const SESSIONLESS: &[&str] = &["app", "frequent", "icon", "ping"];

/// The `SSH_ORIGINAL_COMMAND` a request becomes.
pub fn command_words(line: &str, id: &str) -> String {
    let mut w = line.split_whitespace();
    let verb = w.next().unwrap_or("");
    let rest: Vec<&str> = w.collect();
    if SESSIONLESS.contains(&verb) {
        std::iter::once(verb).chain(rest).collect::<Vec<_>>().join(" ")
    } else {
        std::iter::once(verb).chain(std::iter::once(id)).chain(rest).collect::<Vec<_>>().join(" ")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_gets_this_sessions_id_and_jump_list_verbs_do_not() {
        assert_eq!(command_words("run firefox 96 30", "wcns-1"), "run wcns-1 firefox 96 30");
        assert_eq!(command_words("gpu", "wcns-1"), "gpu wcns-1");
        assert_eq!(command_words("app firefox light", "wcns-1"), "app firefox light");
        assert_eq!(command_words("frequent firefox", "wcns-1"), "frequent firefox");
        // A guest that puts another id first only adds a word after its own.
        assert_eq!(command_words("down other-1", "wcns-1"), "down wcns-1 other-1");
    }

    #[test]
    fn only_plain_words_are_requests() {
        assert!(words_ok("run firefox 96 30"));
        assert!(words_ok("open firefox aHR0cHM6Ly9leGFtcGxlLm9yZy8= 96 30"));
        assert!(!words_ok("run firefox; rm -rf ~"));
        assert!(!words_ok("run $(id)"));
        assert!(!words_ok(""));
    }
}
