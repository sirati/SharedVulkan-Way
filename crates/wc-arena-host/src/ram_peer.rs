//! `wc-arena-host ram-peer`: the host's half of a region table (§108.4), with
//! the §108.12 instrument on top: every region it maps is filled with a
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sv_ram::peer::PeerEvent;
use sv_ram::testlayout::*;
use sv_transport::lpstamp::UNIT;

fn ts() -> String {
    format!("{:.3}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64())
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
    use sv_ram::peerloop::{self, Ram, TableOff};
    let opt = |k: &str| a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned();
    let boot = opt("--boot").ok_or("--boot FILE is required")?;
    let table_off = match opt("--table-off").as_deref() {
        Some("auto") => TableOff::Auto,
        Some(s) => TableOff::Fixed(s.parse().map_err(|e| format!("--table-off: {e}"))?),
        None => TableOff::Fixed(4096),
    };
    let stats_secs: u64 = opt("--stats-secs").map_or(Ok(30), |s| s.parse()).map_err(|e| format!("--stats-secs: {e}"))?;
    let traffic_on = !a.iter().any(|x| x == "--no-traffic");
    let ram = match (opt("--ram"), opt("--qemu-pid")) {
        (Some(p), _) => Ram::Path(p.into()),
        (None, Some(pid)) => Ram::QemuPid(pid.parse().map_err(|e| format!("--qemu-pid: {e}"))?),
        _ => return Err("--ram PATH or --qemu-pid PID is required".into()),
    };
    if a.iter().any(|x| x == "--publish") {
        return Err("--publish is gone: other host processes get regions from the broker (sv_ram::brokered, §108.15)".into());
    }
    let cfg = peerloop::Config { boot: boot.clone().into(), table_off, ram, bell: opt("--bell").map(Into::into) };
    let att = peerloop::attach(&cfg)?;
    eprintln!(
        "[peer] {} boot {boot} ({} MiB), table at {}, guest RAM {} ({} MiB)",
        ts(),
        att.boot_len >> 20,
        att.peer.table_off(),
        att.ram_path.display(),
        att.ram_len >> 20
    );

    let jobs: Arc<Mutex<BTreeMap<u32, Arc<Job>>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let bad_total = Arc::new(AtomicU64::new(0));
    let fill = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
    if traffic_on {
        let (jobs, bad_total, fill) = (jobs.clone(), bad_total.clone(), fill.clone());
        std::thread::spawn(move || writer(jobs, bad_total, fill));
    }
    let (mut mapped_n, mut unmapped_n, mut refused_n) = (0u64, 0u64, 0u64);
    let mut last_stats = Instant::now();
    // What the writer had done in each region it was stopped in, for the
    // UNMAPPED line.
    let stopped: Arc<Mutex<BTreeMap<u32, JobState>>> = Arc::default();
    let (waker, rx) = peerloop::channel();
    let jobs2 = Arc::clone(&jobs);
    let stopped2 = Arc::clone(&stopped);
    peerloop::run(
        att,
        &cfg,
        waker,
        rx,
        move |m| {
            let job = jobs2.lock().unwrap().remove(&m.entry);
            if let Some(j) = job {
                j.stop.store(true, Ordering::SeqCst);
                // Wait out the unit the writer may be in the middle of.
                let st = std::mem::take(&mut *j.busy.lock().unwrap());
                stopped2.lock().unwrap().insert(m.entry, st);
            }
        },
        |peer, evs| {
            for ev in evs {
                match ev {
                    PeerEvent::Mapped { entry, k, len, runs, locate_us, map_us } => {
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
                        refused_n += 1;
                        println!("[peer] {} REFUSED entry {entry} k {k}: {reason:?} ({why})", ts());
                    }
                    PeerEvent::Unmapped { entry, k } => {
                        unmapped_n += 1;
                        let t = stopped.lock().unwrap().remove(entry);
                        println!(
                            "[peer] {} UNMAPPED entry {entry} k {k} (counters written {} times, guest pattern {})",
                            ts(),
                            t.as_ref().map_or(0, |t| t.counter),
                            t.as_ref().map_or("n/a", |t| if t.guest_checked { "checked" } else { "never seen" })
                        );
                    }
                    PeerEvent::ProviderRun { run } => println!("[peer] {} provider run {run:016x}", ts()),
                }
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
        },
    )
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
