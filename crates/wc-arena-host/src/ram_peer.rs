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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
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

/// One mapped region the writer thread works on. The scan takes `busy` and
/// sets `stop` before it unmaps, so no write is ever in flight into a region
/// after its `UNMAPPED` (the in-process form of a mapping lease, §108.4 rule 4).
struct Job {
    entry: u32,
    base: usize,
    len: u64,
    epoch: u64,
    stop: AtomicBool,
    busy: Mutex<JobState>,
}

#[derive(Default)]
struct JobState {
    filled: bool,
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
    let auto = opt("--table-off").as_deref() == Some("auto");
    let table_off: u64 = if auto { 0 } else { opt("--table-off").map_or(Ok(4096), |s| s.parse()).map_err(|e| format!("--table-off: {e}"))? };
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
    // `auto`: the GPU pool says where its table is (poolhdr word 32); wait for it.
    let header_table = |b: &SharedRegion| {
        let mut h = [0u8; 64];
        let _ = b.read_bytes(0, &mut h);
        sv_transport::poolhdr::region_table(&h)
    };
    let table_off = if auto {
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
    } else {
        table_off
    };
    let mut peer = Peer::attach(boot, table_off, ram, run)?;
    if a.iter().any(|x| x == "--publish") {
        peer.publish_to(&boot_path, &ram_path.to_string_lossy());
    }
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


    let jobs: Arc<Mutex<BTreeMap<u32, Arc<Job>>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let bad_total = Arc::new(AtomicU64::new(0));
    let fill = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
    if traffic_on {
        let (jobs, bad_total, fill) = (jobs.clone(), bad_total.clone(), fill.clone());
        std::thread::spawn(move || writer(jobs, bad_total, fill));
    }
    let (mut mapped_n, mut unmapped_n, mut refused_n) = (0u64, 0u64, 0u64);
    let mut last_beat = Instant::now();
    let mut last_stats = Instant::now();
    loop {
        // Wait for a token or 1 ms, whichever first; drain the rest.
        let _ = wrx.recv_timeout(Duration::from_millis(1));
        while wrx.try_recv().is_ok() {}
        let mut stopped: BTreeMap<u32, JobState> = BTreeMap::new();
        let evs = peer.scan(|m| {
            let job = jobs.lock().unwrap().remove(&m.entry);
            if let Some(j) = job {
                j.stop.store(true, Ordering::SeqCst);
                // Wait out the unit the writer may be in the middle of.
                let st = std::mem::take(&mut *j.busy.lock().unwrap());
                stopped.insert(m.entry, st);
            }
        });
        let mut answered = false;
        for ev in &evs {
            match ev {
                PeerEvent::Mapped { entry, k, len, runs, locate_us, map_us } => {
                    answered = true;
                    mapped_n += 1;
                    let m = &peer.maps[entry];
                    println!("[peer] {} MAPPED entry {entry} k {k} {} MiB, {runs} run(s), locate {locate_us} us, map+verify {map_us} us", ts(), len >> 20);
                    if traffic_on {
                        let base = unsafe { m.view.base_ptr() } as usize;
                        let j = Job { entry: *entry, base, len: *len, epoch: m.epoch, stop: AtomicBool::new(false), busy: Mutex::new(JobState::default()) };
                        jobs.lock().unwrap().insert(*entry, Arc::new(j));
                    }
                }
                PeerEvent::Refused { entry, k, reason, why } => {
                    answered = true;
                    refused_n += 1;
                    println!("[peer] {} REFUSED entry {entry} k {k}: {reason:?} ({why})", ts());
                }
                PeerEvent::Unmapped { entry, k } => {
                    answered = true;
                    unmapped_n += 1;
                    let t = stopped.get(entry);
                    println!(
                        "[peer] {} UNMAPPED entry {entry} k {k} (counters written {} times, guest pattern {})",
                        ts(),
                        t.map_or(0, |t| t.counter),
                        t.map_or("n/a", |t| if t.guest_checked { "checked" } else { "never seen" })
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
        if last_beat.elapsed() >= Duration::from_millis(BEAT_MS) {
            peer.beat();
            if auto {
                let o = header_table(peer.boot());
                if o != 0 && o != peer.table_off() {
                    println!("[peer] {} the region table moved to +{o}", ts());
                    let _ = peer.set_table_off(o);
                }
            }
            last_beat = Instant::now();
        }
        if last_stats.elapsed() >= Duration::from_secs(stats_secs) {
            let bytes: u64 = peer.maps.values().map(|m| m.len).sum();
            let (fb, fns) = (fill.0.load(Ordering::Relaxed), fill.1.load(Ordering::Relaxed));
            println!(
                "[peer] {} STATS mapped now {} regions {} MiB; total mapped {mapped_n} unmapped {unmapped_n} refused {refused_n}; guest-pattern bad words {}; fill {:.2} GB/s avg",
                ts(),
                peer.maps.len(),
                bytes >> 20,
                bad_total.load(Ordering::Relaxed),
                if fns > 0 { fb as f64 / fns as f64 } else { 0.0 }
            );
            last_stats = Instant::now();
        }
    }
}

fn w64(base: usize, off: u64, v: u64) {
    unsafe { std::ptr::write_volatile((base + off as usize) as *mut u64, v) }
}
fn r64(base: usize, off: u64) -> u64 {
    unsafe { std::ptr::read_volatile((base + off as usize) as *const u64) }
}

/// The stand-in for a proxy writing pixels: fill each new region once, then
/// write every unit's counter on every pass until the region is retired.
/// Works one 2 MiB unit at a time and looks at `stop` between units.
fn writer(jobs: Arc<Mutex<BTreeMap<u32, Arc<Job>>>>, bad_total: Arc<AtomicU64>, fill: Arc<(AtomicU64, AtomicU64)>) {
    loop {
        let list: Vec<Arc<Job>> = jobs.lock().unwrap().values().cloned().collect();
        for j in list {
            let mut st = j.busy.lock().unwrap();
            if j.stop.load(Ordering::SeqCst) {
                continue;
            }
            if !st.filled {
                let t = Instant::now();
                let mut done = true;
                for u in 0..j.len / UNIT {
                    if j.stop.load(Ordering::SeqCst) {
                        done = false;
                        break;
                    }
                    let mut off = u * UNIT + HOST_FROM;
                    while off < (u + 1) * UNIT {
                        w64(j.base, off, pattern(j.epoch, off));
                        off += 8;
                    }
                }
                if !done {
                    continue;
                }
                std::sync::atomic::fence(Ordering::Release);
                w64(j.base, OFF_HOST_FILLED, j.epoch);
                let ns = t.elapsed().as_nanos() as u64;
                fill.0.fetch_add(j.len, Ordering::Relaxed);
                fill.1.fetch_add(ns, Ordering::Relaxed);
                println!("[peer] {} entry {} filled {} MiB in {:.1} ms ({:.2} GB/s)", ts(), j.entry, j.len >> 20, ns as f64 / 1e6, j.len as f64 / ns as f64);
                st.filled = true;
            }
            st.counter += 1;
            for u in 0..j.len / UNIT {
                w64(j.base, u * UNIT + OFF_COUNTER, st.counter);
            }
            if !st.guest_checked && r64(j.base, OFF_GUEST_DONE) == j.epoch {
                std::sync::atomic::fence(Ordering::Acquire);
                let mut bad = 0u64;
                for u in 0..j.len / UNIT {
                    let mut off = u * UNIT + GUEST_PAGE;
                    while off < u * UNIT + HOST_FROM {
                        if r64(j.base, off) != guest_pattern(j.epoch, off) {
                            bad += 1;
                        }
                        off += 8;
                    }
                }
                st.guest_checked = true;
                if bad > 0 {
                    bad_total.fetch_add(bad, Ordering::Relaxed);
                    println!("[peer] {} entry {}: {bad} word(s) of the guest's pattern wrong", ts(), j.entry);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
