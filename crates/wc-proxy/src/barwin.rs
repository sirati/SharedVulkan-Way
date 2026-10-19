//! §107: the arena of one session of a guest compositor shared by several
//! installations (docs/waycompose-design.md §107).
//!
//! The ivshmem BAR is one device and every installation's proxy maps it. Until
//! §107 each proxy's allocator owned all of it, which is right for one session
//! and is two allocators handing out the same offsets for two. Now the guest's
//! compositor owns the BAR and grants **windows** of it, each a region with an
//! arena id of its own; this proxy allocates only inside the windows it was
//! granted. This is that proxy's half: a [`RegionSource`] that asks for a
//! window on the session's arena slot (`OWNER_ARENA`, `grow N`) and gives an
//! empty one back (`release ID`), which is §34's growth with a window of a BAR
//! where §34 had a region of guest RAM.
//!
//! The slot is claimed at the proxy's start ([`start`]), before any client,
//! because the compositor looks for it for a while after the boot and not for
//! ever. Each request is one line out and one back, under a lock: the arena's
//! growth already runs under its own source lock, so there is never more
//! than one in flight anyway.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use sv_transport::control::Control;
use sv_transport::wlbytes::{self, ByteChannel, ReadOutcome, OWNER_ARENA};

use crate::pool::{ProvidedRegion, RegionSource};

struct Link {
    ch: ByteChannel,
    buf: Vec<u8>,
}

static LINK: OnceLock<Mutex<Option<Link>>> = OnceLock::new();

fn link() -> &'static Mutex<Option<Link>> {
    LINK.get_or_init(|| Mutex::new(None))
}

/// Claim the session's arena slot on `control` for session `index`.
pub fn start(control: &Arc<Control>, index: u8) -> io::Result<u32> {
    let ch = ByteChannel::claim(control, wlbytes::with_session(OWNER_ARENA, index))
        .map_err(|e| io::Error::other(format!("the arena slot: {e}")))?;
    let slot = ch.slot();
    *link().lock().unwrap_or_else(|e| e.into_inner()) = Some(Link { ch, buf: Vec::new() });
    Ok(slot)
}

/// One request on the arena slot, and its one-line answer.
pub fn request(line: &str) -> io::Result<String> {
    let mut g = link().lock().unwrap_or_else(|e| e.into_inner());
    let l = g.as_mut().ok_or_else(|| io::Error::other("no arena slot (the proxy was not started for a shared compositor)"))?;
    let mut out = format!("{line}\n").into_bytes();
    let t0 = Instant::now();
    let deadline = Duration::from_secs(10);
    while !out.is_empty() {
        match l.ch.write(&out) {
            Ok(0) if t0.elapsed() < deadline => std::thread::sleep(Duration::from_millis(1)),
            Ok(0) => return Err(io::Error::new(io::ErrorKind::TimedOut, "the arena slot stayed full")),
            Ok(n) => drop(out.drain(..n)),
            Err(e) => return Err(io::Error::other(format!("the arena slot: {e}"))),
        }
    }
    let mut tmp = [0u8; 512];
    loop {
        if let Some(i) = l.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = l.buf.drain(..=i).collect();
            return Ok(String::from_utf8_lossy(&line[..i]).trim().to_string());
        }
        match l.ch.read(&mut tmp) {
            Ok(ReadOutcome::Bytes(n)) => l.buf.extend_from_slice(&tmp[..n]),
            Ok(ReadOutcome::Eof) => return Err(io::Error::other("the compositor closed the arena slot")),
            Ok(ReadOutcome::Empty) if t0.elapsed() < deadline => {
                let _ = l.ch.wait_readable(crate::rings::service_slice().min(Duration::from_millis(5)));
            }
            Ok(ReadOutcome::Empty) => {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the compositor did not answer on the arena slot"));
            }
            Err(e) => return Err(io::Error::other(format!("the arena slot: {e}"))),
        }
    }
}

/// `ok ID OFFSET LEN`, as the compositor grants a window.
pub fn parse_grant(reply: &str) -> io::Result<(u32, u64, u64)> {
    let w: Vec<&str> = reply.split_whitespace().collect();
    match w.as_slice() {
        ["ok", id, off, len] => match (id.parse(), off.parse(), len.parse()) {
            (Ok(id), Ok(off), Ok(len)) => Ok((id, off, len)),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, format!("a grant that is not numbers: {reply:?}"))),
        },
        ["err", ..] => Err(io::Error::new(io::ErrorKind::OutOfMemory, reply.trim_start_matches("err ").to_string())),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, format!("not a grant: {reply:?}"))),
    }
}

/// Growth by windows of the shared BAR: the [`RegionSource`] of a shared
/// compositor's session.
pub struct BarWindows {
    path: PathBuf,
    run: u64,
    index: u8,
}

impl BarWindows {
    pub fn new(path: &Path, run: u64, index: u8) -> BarWindows {
        BarWindows { path: path.to_path_buf(), run, index }
    }
}

impl RegionSource for BarWindows {
    fn provide(&mut self, _id: u32, min_len: u64) -> io::Result<ProvidedRegion> {
        let reply = request(&format!("grow {min_len}"))?;
        let (id, off, len) = parse_grant(&reply).map_err(|e| {
            io::Error::new(e.kind(), format!("session {}: {min_len} bytes of the shared arena refused: {e}", self.index))
        })?;
        match ProvidedRegion::ivshmem_window(&self.path, self.run, id, off, len) {
            Ok(r) => Ok(r),
            Err(e) => {
                // Not mappable here: the window goes straight back.
                let _ = request(&format!("release {id}"));
                Err(e)
            }
        }
    }

    fn release(&mut self, id: u32) {
        match request(&format!("release {id}")) {
            Ok(r) if r == "ok" => {}
            Ok(r) => println!("[proxy] arena region {id:#x}: the compositor answered {r:?} to its release"),
            Err(e) => println!("[proxy] arena region {id:#x}: release not answered ({e})"),
        }
    }

    fn describe(&self) -> String {
        format!("windows of the shared ivshmem BAR {}, granted to session {} by the guest's compositor (§107)", self.path.display(), self.index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_parses_and_a_refusal_carries_its_reason() {
        assert_eq!(parse_grant("ok 256 16777216 8388608").unwrap(), (256, 16 << 20, 8 << 20));
        let e = parse_grant("err the shared arena has no 8388608-byte run").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::OutOfMemory);
        assert!(e.to_string().contains("no 8388608-byte run"));
        assert!(parse_grant("ok 1 2").is_err());
        assert!(parse_grant("ok a b c").is_err());
    }
}
