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

/// A grant, as the compositor answers `grow`: `ok ID OFFSET LEN` (a window of
/// its BAR, region 0) or, since §110 (§108.6's form), `ok ID region K OFFSET
/// LEN` (a range of the one pool's region K). Returns `(id, region, offset,
/// len)`.
pub fn parse_grant(reply: &str) -> io::Result<(u32, u32, u64, u64)> {
    let w: Vec<&str> = reply.split_whitespace().collect();
    match w.as_slice() {
        ["ok", id, "region", k, off, len] => match (id.parse(), k.parse(), off.parse(), len.parse()) {
            (Ok(id), Ok(k), Ok(off), Ok(len)) => Ok((id, k, off, len)),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, format!("a grant that is not numbers: {reply:?}"))),
        },
        ["ok", id, off, len] => match (id.parse(), off.parse(), len.parse()) {
            (Ok(id), Ok(off), Ok(len)) => Ok((id, 0, off, len)),
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
    /// §110: the files the one pool's other regions are (`--arena-region
    /// K=PATH`), by region.
    regions: std::collections::HashMap<u32, PathBuf>,
}

impl BarWindows {
    pub fn new(path: &Path, run: u64, index: u8) -> BarWindows {
        BarWindows { path: path.to_path_buf(), run, index, regions: Default::default() }
    }

    /// §110: region `k` of the one pool is the file at `path`.
    pub fn with_region(mut self, k: u32, path: &Path) -> BarWindows {
        self.regions.insert(k, path.to_path_buf());
        self
    }
}

impl RegionSource for BarWindows {
    fn provide(&mut self, _id: u32, min_len: u64) -> io::Result<ProvidedRegion> {
        let reply = request(&format!("grow {min_len}"))?;
        let (id, region, off, len) = parse_grant(&reply).map_err(|e| {
            io::Error::new(e.kind(), format!("session {}: {min_len} bytes of the shared arena refused: {e}", self.index))
        })?;
        let mapped = match region {
            0 => ProvidedRegion::ivshmem_window(&self.path, self.run, id, off, len),
            k => match self.regions.get(&k) {
                Some(p) => ProvidedRegion::region_window(p, id, off, len),
                // §108: arena region K ≥ 2 is the GPU pool's region K - 1,
                // grown from guest RAM, which the broker gives this user.
                #[cfg(target_os = "linux")]
                None if k >= 2 && self.regions.contains_key(&1) => {
                    let r = ProvidedRegion::ram_window(k - 1, id, off, len);
                    if r.is_ok() {
                        println!("[proxy] arena region {id:#x}: guest-RAM window {off}+{len} of arena region {k} (gpu.{}, §108)", k - 1);
                    }
                    r
                }
                None => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("region {k} of the pool was granted, and this proxy was given no file for it (--arena-region)"),
                )),
            },
        };
        match mapped {
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
        assert_eq!(parse_grant("ok 256 16777216 8388608").unwrap(), (256, 0, 16 << 20, 8 << 20));
        assert_eq!(parse_grant("ok 257 region 1 1048576 2097152").unwrap(), (257, 1, 1 << 20, 2 << 20));
        assert!(parse_grant("ok 257 region x 1 2").is_err());
        let e = parse_grant("err the shared arena has no 8388608-byte run").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::OutOfMemory);
        assert!(e.to_string().contains("no 8388608-byte run"));
        assert!(parse_grant("ok 1 2").is_err());
        assert!(parse_grant("ok a b c").is_err());
    }
}
