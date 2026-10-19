//! `wc-arena-host ram-peer`: the host's half of a region table (§108.4), with
//! the §108.11 instrument on top: every region it maps is filled with a
//! pattern, its counters are written for as long as it is mapped (the
//! stand-in for a proxy writing pixels), and the guest's answer is checked.
//!
//! ```text
//! wc-arena-host ram-peer --boot FILE [--table-off 4096] (--ram PATH | --qemu-pid PID)
//!                        [--bell SOCKET] [--stats-secs 30] [--no-traffic]
//! ```
//!
//! `--boot` is the bootstrap device's backing file. `--ram` is the guest RAM
//! backend; with `--qemu-pid` the memfd QEMU holds is found through
//! `/proc/PID/fd` (the same rule as `sv_hostd::qemu`: the link must name
//! `memory-backend`, never the display surface's memfd).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use sv_ram::peer::{Peer, PeerEvent};
use sv_ram::testlayout::*;
use sv_transport::lpstamp::UNIT;
use sv_transport::region::SharedRegion;
use sv_transport::regiontab::{BEAT_MS, REGION_TOKEN};

fn ts() -> String {
    format!("{:.3}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64())
}

fn find_memfd(pid: u32) -> Result<std::path::PathBuf, String> {
    let dir = format!("/proc/{pid}/fd");
    for e in std::fs::read_dir(&dir).map_err(|e| format!("{dir}: {e}"))? {
        let p = e.map_err(|e| e.to_string())?.path();
        if let Ok(l) = std::fs::read_link(&p) {
            if l.to_string_lossy().contains("memfd:memory-backend") {
                return Ok(p);
            }
        }
    }
    Err(format!("no memfd:memory-backend among {dir}"))
}

struct Traffic {
    epoch: u64,
    guest_checked: bool,
    counter: u64,
}

pub fn main(args: &[String]) -> std::process::ExitCode {
    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wc-arena-host ram-peer: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(a: &[String]) -> Result<(), String> {
    let opt = |k: &str| a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned();
    let boot_path = opt("--boot").ok_or("--boot FILE is required")?;
    let table_off: u64 = opt("--table-off").map_or(Ok(4096), |s| s.parse()).map_err(|e| format!("--table-off: {e}"))?;
    let stats_secs: u64 = opt("--stats-secs").map_or(Ok(30), |s| s.parse()).map_err(|e| format!("--stats-secs: {e}"))?;
    let traffic_on = !a.iter().any(|x| x == "--no-traffic");
    let ram_path = match (opt("--ram"), opt("--qemu-pid")) {
        (Some(p), _) => p.into(),
        (None, Some(pid)) => find_memfd(pid.parse().map_err(|e| format!("--qemu-pid: {e}"))?)?,
        _ => return Err("--ram PATH or --qemu-pid PID is required".into()),
    };
    let bf = std::fs::OpenOptions::new().read(true).write(true).open(&boot_path).map_err(|e| format!("{boot_path}: {e}"))?;
    let blen = bf.metadata().map_err(|e| e.to_string())?.len() as usize;
    let boot = SharedRegion::map_fd(bf.as_fd(), 0, blen).map_err(|e| format!("mapping {boot_path}: {e}"))?;
    let ram = std::fs::OpenOptions::new().read(true).write(true).open(&ram_path).map_err(|e| format!("{}: {e}", ram_path.display()))?;
    let ram_len = ram.metadata().map_err(|e| e.to_string())?.len();
    let run = std::process::id() as u64 ^ (Instant::now().elapsed().as_nanos() as u64) << 20;
    let mut peer = Peer::attach(boot, table_off, ram, run)?;
    eprintln!("[peer] {} boot {boot_path} ({} MiB), table at {table_off}, guest RAM {} ({} MiB)", ts(), blen >> 20, ram_path.display(), ram_len >> 20);

    // The doorbell: QEMU's chardev socket (or a hub's). A reader thread turns
    // tokens into wakes; every answer rings back.
    let (wtx, wrx) = mpsc::channel::<()>();
    let mut bell_w = match opt("--bell") {
        Some(p) => {
            let s = std::os::unix::net::UnixStream::connect(&p).map_err(|e| format!("{p}: {e}"))?;
            let mut r = s.try_clone().map_err(|e| e.to_string())?;
            std::thread::spawn(move || {
                let mut b = [0u8; 256];
                while let Ok(n) = r.read(&mut b) {
                    if n == 0 {
                        break;
                    }
                    let _ = wtx.send(());
                }
            });
            Some(s)
        }
        None => None,
    };

    let mut traffic: BTreeMap<u32, Traffic> = BTreeMap::new();
    let (mut mapped_n, mut unmapped_n, mut refused_n, mut bad_total) = (0u64, 0u64, 0u64, 0u64);
    let (mut fill_bytes, mut fill_ns) = (0u64, 0u128);
    let mut last_beat = Instant::now();
    let mut last_stats = Instant::now();
    loop {
        // Wait for a token or 1 ms, whichever first; drain the rest.
        let _ = wrx.recv_timeout(Duration::from_millis(1));
        while wrx.try_recv().is_ok() {}
        let evs = peer.scan(|m| {
            // the writer is this thread: nothing runs on the region after this
            let _ = m;
        });
        let mut answered = false;
        for ev in &evs {
            match ev {
                PeerEvent::Mapped { entry, k, len, runs, locate_us, map_us } => {
                    answered = true;
                    mapped_n += 1;
                    let m = &peer.maps[entry];
                    let mut line = format!("[peer] {} MAPPED entry {entry} k {k} {} MiB, {runs} run(s), locate {locate_us} us, map+verify {map_us} us", ts(), len >> 20);
                    if traffic_on {
                        let t = Instant::now();
                        let base = unsafe { m.view.base_ptr() } as usize;
                        for u in 0..len / UNIT {
                            let mut off = u * UNIT + HOST_FROM;
                            let end = (u + 1) * UNIT;
                            while off < end {
                                unsafe { std::ptr::write_volatile((base + off as usize) as *mut u64, pattern(m.epoch, off)) };
                                off += 8;
                            }
                        }
                        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                        unsafe { std::ptr::write_volatile((base + OFF_HOST_FILLED as usize) as *mut u64, m.epoch) };
                        let ns = t.elapsed().as_nanos();
                        fill_bytes += len;
                        fill_ns += ns;
                        line.push_str(&format!(", filled in {:.1} ms ({:.2} GB/s)", ns as f64 / 1e6, *len as f64 / ns as f64));
                        traffic.insert(*entry, Traffic { epoch: m.epoch, guest_checked: false, counter: 0 });
                    }
                    println!("{line}");
                }
                PeerEvent::Refused { entry, k, reason, why } => {
                    answered = true;
                    refused_n += 1;
                    println!("[peer] {} REFUSED entry {entry} k {k}: {reason:?} ({why})", ts());
                }
                PeerEvent::Unmapped { entry, k } => {
                    answered = true;
                    unmapped_n += 1;
                    let t = traffic.remove(entry);
                    println!(
                        "[peer] {} UNMAPPED entry {entry} k {k} (wrote its counters {} times, guest pattern {})",
                        ts(),
                        t.as_ref().map_or(0, |t| t.counter),
                        t.as_ref().map_or("n/a", |t| if t.guest_checked { "checked" } else { "never seen" })
                    );
                }
                PeerEvent::ProviderRun { run } => println!("[peer] {} provider run {run:016x}", ts()),
            }
        }
        if answered {
            if let Some(b) = bell_w.as_mut() {
                let _ = b.write_all(&[REGION_TOKEN]);
            }
        }
        // The writers: every mapped region's counters, every pass, until the
        // region is retired. A host that kept writing after the guest freed
        // the memory would show up in the guest's canary.
        if traffic_on {
            for (entry, t) in traffic.iter_mut() {
                let Some(m) = peer.maps.get(entry) else { continue };
                let base = unsafe { m.view.base_ptr() } as usize;
                t.counter += 1;
                for u in 0..m.len / UNIT {
                    unsafe { std::ptr::write_volatile((base + (u * UNIT + OFF_COUNTER) as usize) as *mut u64, t.counter) };
                }
                if !t.guest_checked && unsafe { std::ptr::read_volatile((base + OFF_GUEST_DONE as usize) as *const u64) } == t.epoch {
                    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                    let mut bad = 0u64;
                    for u in 0..m.len / UNIT {
                        let mut off = u * UNIT + GUEST_PAGE;
                        while off < u * UNIT + HOST_FROM {
                            if unsafe { std::ptr::read_volatile((base + off as usize) as *const u64) } != guest_pattern(t.epoch, off) {
                                bad += 1;
                            }
                            off += 8;
                        }
                    }
                    t.guest_checked = true;
                    bad_total += bad;
                    if bad > 0 {
                        println!("[peer] {} entry {entry}: {bad} word(s) of the guest's pattern wrong", ts());
                    }
                }
            }
        }
        if last_beat.elapsed() >= Duration::from_millis(BEAT_MS) {
            peer.beat();
            last_beat = Instant::now();
        }
        if last_stats.elapsed() >= Duration::from_secs(stats_secs) {
            let bytes: u64 = peer.maps.values().map(|m| m.len).sum();
            println!(
                "[peer] {} STATS mapped now {} regions {} MiB; total mapped {mapped_n} unmapped {unmapped_n} refused {refused_n}; guest-pattern bad words {bad_total}; fill {:.2} GB/s avg",
                ts(),
                peer.maps.len(),
                bytes >> 20,
                if fill_ns > 0 { fill_bytes as f64 / fill_ns as f64 } else { 0.0 }
            );
            last_stats = Instant::now();
        }
    }
}
