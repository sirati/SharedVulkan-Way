//! The peer's loop (§108.2, §108.9): attach to a pool's region table, then
//! read it on every doorbell token, every reconnect of the doorbell, and at
//! least every beat, and ring back after every answer.
//!
//! One loop for both hosts of a [`Peer`]: the broker daemon
//! ([`crate::brokered`]) and the test VM's instrument (`wc-arena-host
//! ram-peer`).

use crate::peer::{Mapped, Peer, PeerEvent};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use sv_transport::region::SharedRegion;
use sv_transport::regiontab::{BEAT_MS, REGION_TOKEN};

/// Where the guest's RAM is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ram {
    /// A file: the RAM backend itself, or a stand-in for it.
    Path(PathBuf),
    /// QEMU's RAM memfd, found through `/proc/PID/fd`.
    QemuPid(u32),
    /// As `QemuPid`, with the pid read from a file (QEMU's `-pidfile`).
    QemuPidFile(PathBuf),
}

/// Where the region table is in the bootstrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableOff {
    /// At this offset.
    Fixed(u64),
    /// Where the GPU pool's header says (`poolhdr` word 32), followed if it
    /// moves.
    Auto,
}

pub struct Config {
    /// The bootstrap's backing file (the GPU device's), which holds the table.
    pub boot: PathBuf,
    pub table_off: TableOff,
    pub ram: Ram,
    /// The pool's doorbell: a Unix socket (QEMU's chardev or the bell hub's).
    /// `None`: the table is polled every millisecond.
    pub bell: Option<PathBuf>,
}

/// The memfd QEMU backs guest RAM with: the link must name `memory-backend`,
/// never the display surface's memfd (the same rule as `sv_hostd::qemu`).
pub fn find_memfd(pid: u32) -> Result<PathBuf, String> {
    let dir = format!("/proc/{pid}/fd");
    for e in std::fs::read_dir(&dir).map_err(|e| format!("{dir}: {e}"))? {
        let p = e.map_err(|e| e.to_string())?.path();
        if let Ok(l) = std::fs::read_link(&p)
            && l.to_string_lossy().contains("memfd:memory-backend")
        {
            return Ok(p);
        }
    }
    Err(format!("no memfd:memory-backend among {dir}"))
}

impl Ram {
    pub fn path(&self) -> Result<PathBuf, String> {
        match self {
            Ram::Path(p) => Ok(p.clone()),
            Ram::QemuPid(pid) => find_memfd(*pid),
            Ram::QemuPidFile(f) => {
                let s = std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
                find_memfd(s.trim().parse().map_err(|e| format!("{}: {e}", f.display()))?)
            }
        }
    }
}

fn header_table(b: &SharedRegion) -> u64 {
    let mut h = [0u8; 64];
    let _ = b.read_bytes(0, &mut h);
    sv_transport::poolhdr::region_table(&h)
}

/// What [`attach`] opened.
pub struct Attached {
    pub peer: Peer,
    pub boot_len: u64,
    pub ram_path: PathBuf,
    pub ram_len: u64,
    auto: bool,
}

/// Open the bootstrap and guest RAM and attach a peer (waits up to 600 s for
/// the pool to publish its table under [`TableOff::Auto`]).
pub fn attach(cfg: &Config) -> Result<Attached, String> {
    let bp = &cfg.boot;
    let bf = std::fs::OpenOptions::new().read(true).write(true).open(bp).map_err(|e| format!("{}: {e}", bp.display()))?;
    let blen = bf.metadata().map_err(|e| e.to_string())?.len();
    let boot = SharedRegion::map_fd(bf.as_fd(), 0, blen as usize).map_err(|e| format!("mapping {}: {e}", bp.display()))?;
    let ram_path = cfg.ram.path()?;
    let ram = std::fs::OpenOptions::new().read(true).write(true).open(&ram_path).map_err(|e| format!("{}: {e}", ram_path.display()))?;
    let ram_len = ram.metadata().map_err(|e| e.to_string())?.len();
    let off = match cfg.table_off {
        TableOff::Fixed(o) => o,
        TableOff::Auto => {
            let t = Instant::now();
            loop {
                let o = header_table(&boot);
                if o != 0 {
                    break o;
                }
                if t.elapsed() > Duration::from_secs(600) {
                    return Err("no region table published in 600 s".into());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let run = std::process::id() as u64 ^ (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64) << 20);
    let peer = Peer::attach(boot, off, ram, run)?;
    Ok(Attached { peer, boot_len: blen, ram_path, ram_len, auto: cfg.table_off == TableOff::Auto })
}

/// Wakes the loop: a doorbell token, a reconnect, or anything else that
/// changed what a scan would decide (a region drained on the host).
#[derive(Clone)]
pub struct Waker(mpsc::Sender<()>);

impl Waker {
    pub fn wake(&self) {
        let _ = self.0.send(());
    }
}

/// The loop's wake channel, made before the loop so that what it serves can
/// hold a [`Waker`] from the start.
pub fn channel() -> (Waker, mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    (Waker(tx), rx)
}

fn ts() -> String {
    format!("{:.3}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64()))
}

/// Connect the doorbell and turn every byte into a wake; when the socket
/// goes, reconnect, and a (re)connect is a wake too (the rescan rule,
/// §103.14). Returns the writing half the loop rings back on.
fn bell(path: &Path, waker: Waker) -> Result<Arc<Mutex<Option<UnixStream>>>, String> {
    let s = UnixStream::connect(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let w = Arc::new(Mutex::new(Some(s.try_clone().map_err(|e| e.to_string())?)));
    let w2 = Arc::clone(&w);
    let p = path.to_path_buf();
    std::thread::Builder::new()
        .name("peer-bell".into())
        .spawn(move || {
            let mut r = s;
            let mut b = [0u8; 256];
            loop {
                while let Ok(n) = r.read(&mut b) {
                    if n == 0 {
                        break;
                    }
                    waker.wake();
                }
                *w2.lock().unwrap_or_else(|e| e.into_inner()) = None;
                eprintln!("[peer] {} the doorbell closed; reconnecting", ts());
                r = loop {
                    std::thread::sleep(Duration::from_secs(1));
                    if let Ok(s) = UnixStream::connect(&p) {
                        break s;
                    }
                };
                if let Ok(c) = r.try_clone() {
                    *w2.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
                }
                waker.wake();
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(w)
}

/// Run the peer for ever. `before_unmap` stops any writer of a region before
/// the peer's own view of it goes; `events` sees every pass's events and is
/// called once a beat with none, for statistics.
pub fn run(
    mut a: Attached,
    cfg: &Config,
    waker: Waker,
    rx: mpsc::Receiver<()>,
    mut before_unmap: impl FnMut(&Mapped),
    mut events: impl FnMut(&Peer, &[PeerEvent]),
) -> Result<(), String> {
    let bell_w = match &cfg.bell {
        Some(p) => Some(bell(p, waker)?),
        None => None,
    };
    let slice = if bell_w.is_some() { Duration::from_millis(BEAT_MS) } else { Duration::from_millis(1) };
    let mut last_beat = Instant::now();
    loop {
        let _ = rx.recv_timeout(slice);
        while rx.try_recv().is_ok() {}
        let evs = a.peer.scan(&mut before_unmap);
        let answered = evs.iter().any(|e| !matches!(e, PeerEvent::ProviderRun { .. }));
        if answered
            && let Some(w) = &bell_w
            && let Some(b) = w.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            let _ = b.write_all(&[REGION_TOKEN]);
        }
        if !evs.is_empty() {
            events(&a.peer, &evs);
        }
        if last_beat.elapsed() >= Duration::from_millis(BEAT_MS) {
            a.peer.beat();
            if a.auto {
                let o = header_table(a.peer.boot());
                if o != 0 && o != a.peer.table_off() {
                    eprintln!("[peer] {} the region table moved to +{o}", ts());
                    let _ = a.peer.set_table_off(o);
                }
            }
            events(&a.peer, &[]);
            last_beat = Instant::now();
        }
    }
}
