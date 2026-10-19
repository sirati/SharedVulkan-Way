//! `sv-guest ram …`: the keeper, and the instruments that proved guest-RAM
//! regions on a Windows test VM before any real guest saw them
//! (waycompose-design.md §108.11).
//!
//! ```text
//! sv-guest ram keeper                      the SYSTEM keeper (§108.10)
//! sv-guest ram test [opts]                 a provider serving N sessions that grow and shrink
//! sv-guest ram canary --mib N              ordinary memory, filled and checked: a host write into
//!                                          freed guest RAM shows up here as corruption
//! sv-guest ram hog --mib N --secs S        memory pressure: touch N MiB, free, repeat
//! ```

pub fn main(args: Vec<String>) -> std::process::ExitCode {
    #[cfg(windows)]
    {
        match imp::main(args) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("sv-guest ram: {e}");
                std::process::ExitCode::from(1)
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        eprintln!("sv-guest ram: the keeper and its instruments are the Windows guest's");
        std::process::ExitCode::from(2)
    }
}

#[cfg(windows)]
use sv_ram::testlayout::*;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};
    use sv_ram::provider::{Event, Provider};
    use sv_ram::winsub::KeeperSubstrate;
    use sv_transport::region::SharedRegion;
    use sv_transport::regiontab::REGION_TOKEN;

    fn opt<'a>(a: &'a [String], k: &str) -> Option<&'a str> {
        a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).map(|s| s.as_str())
    }

    fn size(s: &str) -> Result<u64, String> {
        let (n, m) = match s.chars().last() {
            Some('G') => (&s[..s.len() - 1], 1u64 << 30),
            Some('M') => (&s[..s.len() - 1], 1 << 20),
            _ => (s, 1),
        };
        n.parse::<u64>().map(|v| v * m).map_err(|e| format!("{s}: {e}"))
    }

    pub fn main(args: Vec<String>) -> Result<(), String> {
        sv_ram::winsys::fine_timer();
        match args.first().map(|s| s.as_str()) {
            Some("keeper") => sv_ram::keeper::serve(),
            Some("test") => test(&args[1..]),
            Some("canary") => canary(&args[1..]),
            Some("hog") => hog(&args[1..]),
            Some("jitter") => jitter(&args[1..]),
            Some("stats") => {
                let (mut k, _) = sv_ram::keeper::KeeperClient::connect("stats")?;
                println!("{}", k.stats()?);
                Ok(())
            }
            _ => Err("usage: sv-guest ram keeper|test|canary|hog|stats".into()),
        }
    }

    fn ts() -> String {
        format!("{:.3}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64())
    }

    /// The doorbell: a reader thread turning tokens into a condvar wake.
    struct Bell {
        port: Option<Arc<sv_transport::winpipe::Pipe>>,
        wake: Arc<(Mutex<u64>, Condvar)>,
    }

    impl Bell {
        fn open(name: Option<&str>) -> Bell {
            let wake = Arc::new((Mutex::new(0u64), Condvar::new()));
            let port = name.and_then(|n| match sv_transport::winpipe::open_virtio_port(n, 3000) {
                Ok(p) => Some(Arc::new(p)),
                Err(e) => {
                    eprintln!("[ram] no doorbell ({n}: {e}); polling");
                    None
                }
            });
            if let Some(p) = port.clone() {
                let w = wake.clone();
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut b = [0u8; 64];
                    loop {
                        match (&*p).read(&mut b) {
                            Ok(0) | Err(_) => std::thread::sleep(Duration::from_millis(5)),
                            Ok(_) => {
                                *w.0.lock().unwrap() += 1;
                                w.1.notify_all();
                            }
                        }
                    }
                });
            }
            Bell { port, wake }
        }
        fn ring(&self) {
            if let Some(p) = &self.port {
                use std::io::Write;
                let _ = (&**p).write_all(&[REGION_TOKEN]);
            }
        }
        fn wait(&self, seen: &mut u64, d: Duration) {
            let g = self.wake.0.lock().unwrap();
            let (g, _) = self.wake.1.wait_timeout_while(g, d, |n| *n == *seen).unwrap();
            *seen = *g;
        }
    }

    /// A session's request to the provider thread.
    enum Req {
        Grow { len: u64, reply: mpsc::Sender<Result<Granted, String>> },
        Retire { entry: u32, reply: mpsc::Sender<u64> },
    }
    struct Granted {
        entry: u32,
        epoch: u64,
        base: usize,
        len: u64,
        rtt_us: u64,
        alloc_us: u64,
    }

    #[derive(Default)]
    struct Totals {
        grows: AtomicU64,
        bytes: AtomicU64,
        refused: AtomicU64,
        errors: AtomicU64,
        verified_bytes: AtomicU64,
    }

    fn test(a: &[String]) -> Result<(), String> {
        let slot: u8 = u8::from_str_radix(opt(a, "--slot").unwrap_or("0x09").trim_start_matches("0x"), 16).map_err(|e| e.to_string())?;
        let table_off: u64 = opt(a, "--table-off").map_or(Ok(4096), |s| s.parse()).map_err(|e: std::num::ParseIntError| e.to_string())?;
        let sessions: usize = opt(a, "--sessions").map_or(Ok(1), |s| s.parse()).map_err(|e: std::num::ParseIntError| e.to_string())?;
        let cycles: u64 = opt(a, "--cycles").map_or(Ok(1), |s| s.parse()).map_err(|e: std::num::ParseIntError| e.to_string())?;
        let secs: u64 = opt(a, "--secs").map_or(Ok(0), |s| s.parse()).map_err(|e: std::num::ParseIntError| e.to_string())?;
        let sizes: Vec<u64> = opt(a, "--sizes").unwrap_or("64M").split(',').map(size).collect::<Result<_, _>>()?;
        let hold_ms: u64 = opt(a, "--hold-ms").map_or(Ok(500), |s| s.parse()).map_err(|e: std::num::ParseIntError| e.to_string())?;
        let stride: u64 = opt(a, "--verify-stride").map_or(Ok(8), |s| s.parse()).map_err(|e: std::num::ParseIntError| e.to_string())?;
        let pool = opt(a, "--pool").unwrap_or("test").to_string();
        let bell_name = opt(a, "--bell");
        let leak_pid_file = opt(a, "--pid-file");
        if let Some(p) = leak_pid_file {
            let _ = std::fs::write(p, std::process::id().to_string());
        }

        let (view, bar_len, path) = wc_handle::map_ivshmem_slot(slot).map_err(|e| format!("bootstrap device: {e}"))?;
        eprintln!("[ram] {} bootstrap {path}: {} MiB, table at {table_off}", ts(), bar_len >> 20);
        // SAFETY: `view` lives until the end of this function, past every user.
        let boot = unsafe { SharedRegion::from_raw(view.ptr() as *mut u8, bar_len as usize) };
        let sub = KeeperSubstrate::connect(&pool)?;
        let run = u64::from_le_bytes(sv_ram::winsys::random::<8>());
        let mut prov = Provider::start(boot, table_off, sub, run)?;
        let adopted = prov.regions.len();
        eprintln!("[ram] {} provider run {run:016x}, keeper run {:016x}, {adopted} orphan(s) adopted", ts(), prov.substrate.keeper.run);
        let bell = Arc::new(Bell::open(bell_name));
        bell.ring();

        let stop = Arc::new(AtomicBool::new(false));
        let totals = Arc::new(Totals::default());
        let (tx, rx) = mpsc::channel::<Req>();
        let mut workers = Vec::new();
        for s in 0..sessions {
            let (tx, stop, totals, sizes) = (tx.clone(), stop.clone(), totals.clone(), sizes.clone());
            workers.push(std::thread::spawn(move || session(s, tx, stop, totals, &sizes, cycles, hold_ms, stride)));
        }
        drop(tx);

        // The provider thread: the pool owner. Serves grows and retires,
        // polls answers, beats.
        let t_end = (secs > 0).then(|| Instant::now() + Duration::from_secs(secs));
        let mut pending_grow: Vec<(u32, u64, mpsc::Sender<Result<Granted, String>>)> = Vec::new();
        let mut pending_retire: Vec<(u32, mpsc::Sender<u64>)> = Vec::new();
        let mut next_k = 1u32;
        let mut last_beat = Instant::now();
        let mut seen = 0u64;
        let mut senders_gone = false;
        loop {
            if t_end.is_some_and(|t| Instant::now() >= t) {
                stop.store(true, Ordering::Relaxed);
            }
            loop {
                match rx.try_recv() {
                    Ok(Req::Grow { len, reply }) => {
                        let k = next_k;
                        next_k += 1;
                        match prov.announce(k, len) {
                            Ok(entry) => {
                                pending_grow.push((entry, len, reply));
                                bell.ring();
                            }
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                    Ok(Req::Retire { entry, reply }) => {
                        prov.retire(entry);
                        pending_retire.push((entry, reply));
                        bell.ring();
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        senders_gone = true;
                        break;
                    }
                }
            }
            for ev in prov.poll()? {
                match ev {
                    Event::Mapped { entry, rtt_us, .. } => {
                        if let Some(i) = pending_grow.iter().position(|p| p.0 == entry) {
                            let (entry, len, reply) = pending_grow.swap_remove(i);
                            let r = &prov.regions[&entry];
                            let base = unsafe { r.view.as_ref().unwrap().base_ptr() } as usize;
                            let _ = reply.send(Ok(Granted { entry, epoch: r.epoch, base, len, rtt_us, alloc_us: r.alloc_us }));
                        }
                    }
                    Event::Refused { k, reason } => {
                        eprintln!("[ram] {} region {k} REFUSED: {reason:?}", ts());
                        totals.refused.fetch_add(1, Ordering::Relaxed);
                        // the entry is gone; find its pending grow by elimination
                        pending_grow.retain(|(e, _, reply)| {
                            if prov.regions.contains_key(e) {
                                true
                            } else {
                                let _ = reply.send(Err(format!("refused: {reason:?}")));
                                false
                            }
                        });
                    }
                    Event::Freed { k, wait_us } => {
                        let freed: Vec<u32> = pending_retire.iter().filter(|(e, _)| !prov.regions.contains_key(e)).map(|p| p.0).collect();
                        pending_retire.retain(|(e, reply)| {
                            if freed.contains(e) {
                                let _ = reply.send(wait_us);
                                false
                            } else {
                                true
                            }
                        });
                        if freed.is_empty() {
                            eprintln!("[ram] {} orphan region {k} freed after the host let go ({wait_us} us)", ts());
                        }
                    }
                }
            }
            if last_beat.elapsed() >= Duration::from_millis(200) {
                prov.beat();
                last_beat = Instant::now();
            }
            if senders_gone && prov.regions.is_empty() && pending_grow.is_empty() {
                break;
            }
            bell.wait(&mut seen, Duration::from_millis(1));
        }
        for w in workers {
            let _ = w.join();
        }
        let k = prov.substrate.keeper.stats().unwrap_or_default();
        println!(
            "[ram] {} DONE large-page views={} grows={} bytes={} MiB verified={} MiB refused={} errors={} keeper={k}",
            ts(),
            prov.substrate.large_translations,
            totals.grows.load(Ordering::Relaxed),
            totals.bytes.load(Ordering::Relaxed) >> 20,
            totals.verified_bytes.load(Ordering::Relaxed) >> 20,
            totals.refused.load(Ordering::Relaxed),
            totals.errors.load(Ordering::Relaxed),
        );
        drop(prov);
        drop(view);
        if totals.errors.load(Ordering::Relaxed) > 0 { Err("errors".into()) } else { Ok(()) }
    }

    fn rd(base: usize, off: u64) -> u64 {
        unsafe { std::ptr::read_volatile((base + off as usize) as *const u64) }
    }
    fn wr(base: usize, off: u64, v: u64) {
        unsafe { std::ptr::write_volatile((base + off as usize) as *mut u64, v) }
    }

    /// One session: grow, check what the host wrote, write back, hold, give back.
    fn session(s: usize, tx: mpsc::Sender<Req>, stop: Arc<AtomicBool>, totals: Arc<Totals>, sizes: &[u64], cycles: u64, hold_ms: u64, stride: u64) {
        let unit = sv_transport::lpstamp::UNIT;
        let mut n = 0u64;
        while !stop.load(Ordering::Relaxed) && (cycles == 0 || n < cycles) {
            let len = sizes[((n as usize) + s) % sizes.len()];
            n += 1;
            let (rtx, rrx) = mpsc::channel();
            if tx.send(Req::Grow { len, reply: rtx }).is_err() {
                return;
            }
            let g = match rrx.recv() {
                Ok(Ok(g)) => g,
                Ok(Err(e)) => {
                    eprintln!("[ram] {} s{s} grow {} MiB failed: {e}", ts(), len >> 20);
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                Err(_) => return,
            };
            totals.grows.fetch_add(1, Ordering::Relaxed);
            totals.bytes.fetch_add(len, Ordering::Relaxed);
            // The host fills, then says so.
            let t = Instant::now();
            while rd(g.base, OFF_HOST_FILLED) != g.epoch {
                if t.elapsed() > Duration::from_secs(20) {
                    eprintln!("[ram] {} s{s} entry {} epoch {}: the host never filled", ts(), g.entry, g.epoch);
                    totals.errors.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let fill_wait = t.elapsed();
            // Check every word the host wrote.
            let t = Instant::now();
            let mut bad = 0u64;
            let mut first_bad = None;
            let units = g.len / unit;
            for u in 0..units {
                let mut off = u * unit + HOST_FROM;
                while off < (u + 1) * unit {
                    if rd(g.base, off) != pattern(g.epoch, off) {
                        bad += 1;
                        first_bad.get_or_insert(off);
                    }
                    off += stride;
                }
            }
            let verify = t.elapsed();
            totals.verified_bytes.fetch_add(g.len, Ordering::Relaxed);
            // The host's counters must be moving.
            let c0: Vec<u64> = (0..units).map(|u| rd(g.base, u * unit + OFF_COUNTER)).collect();
            // Every counter must move within 2 s: the host is still writing.
            let t = Instant::now();
            let mut still;
            loop {
                std::thread::sleep(Duration::from_millis(5));
                still = (0..units).filter(|&u| rd(g.base, u * unit + OFF_COUNTER) == c0[u as usize]).count();
                if still == 0 || t.elapsed() > Duration::from_secs(2) {
                    break;
                }
            }
            // Our pattern for the host to check.
            for u in 0..units {
                let mut off = u * unit + GUEST_PAGE;
                while off < u * unit + HOST_FROM {
                    wr(g.base, off, guest_pattern(g.epoch, off));
                    off += 8;
                }
            }
            std::sync::atomic::fence(Ordering::Release);
            wr(g.base, OFF_GUEST_DONE, g.epoch);
            if bad > 0 || still > 0 {
                totals.errors.fetch_add(1, Ordering::Relaxed);
            }
            println!(
                "[ram] {} s{s} grow {} MiB entry {} alloc {} us, announce->mapped {} us, host fill {:.1} ms, verify {:.1} ms ({:.2} GB/s), bad words {bad}{}, still counters {still}",
                ts(),
                g.len >> 20,
                g.entry,
                g.alloc_us,
                g.rtt_us,
                fill_wait.as_secs_f64() * 1e3,
                verify.as_secs_f64() * 1e3,
                (g.len as f64 / verify.as_secs_f64()) / 1e9,
                first_bad.map(|o| format!(" (first at {o:#x})")).unwrap_or_default(),
            );
            std::thread::sleep(Duration::from_millis(hold_ms));
            let (rtx, rrx) = mpsc::channel();
            if tx.send(Req::Retire { entry: g.entry, reply: rtx }).is_err() {
                return;
            }
            match rrx.recv() {
                Ok(wait_us) => println!("[ram] {} s{s} retire entry {} -> freed after {wait_us} us", ts(), g.entry),
                Err(_) => return,
            }
        }
    }

    // ------------------------------------------------------------ canary, hog

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualAlloc(a: *mut core::ffi::c_void, n: usize, t: u32, p: u32) -> *mut core::ffi::c_void;
        fn VirtualFree(a: *mut core::ffi::c_void, n: usize, t: u32) -> i32;
        fn SetPriorityClass(p: isize, c: u32) -> i32;
        fn SetThreadPriority(t: isize, p: i32) -> i32;
        fn GetCurrentProcess() -> isize;
        fn GetCurrentThread() -> isize;
    }

    fn valloc(len: usize) -> Result<usize, String> {
        let p = unsafe { VirtualAlloc(core::ptr::null_mut(), len, 0x3000, 0x04) };
        if p.is_null() { Err(format!("VirtualAlloc {len}: {}", std::io::Error::last_os_error())) } else { Ok(p as usize) }
    }
    fn vfree(p: usize) {
        unsafe { VirtualFree(p as *mut _, 0, 0x8000) };
    }

    fn canary(a: &[String]) -> Result<(), String> {
        let mib: usize = opt(a, "--mib").unwrap_or("512").parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
        let cycle = Duration::from_secs(opt(a, "--cycle-secs").unwrap_or("20").parse().map_err(|e: std::num::ParseIntError| e.to_string())?);
        let secs: u64 = opt(a, "--secs").unwrap_or("0").parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
        let len = mib << 20;
        let t_end = (secs > 0).then(|| Instant::now() + Duration::from_secs(secs));
        let (mut checks, mut cyc, mut corrupt) = (0u64, 0u64, 0u64);
        while t_end.is_none_or(|t| Instant::now() < t) {
            let p = valloc(len)?;
            let seed = u64::from_le_bytes(sv_ram::winsys::random::<8>());
            let word = |i: usize| (i as u64).wrapping_mul(0x2545_F491_4F6C_DD1D) ^ seed;
            for i in 0..len / 8 {
                wr(p, i as u64 * 8, word(i));
            }
            let t = Instant::now();
            while t.elapsed() < cycle && t_end.is_none_or(|e| Instant::now() < e) {
                std::thread::sleep(Duration::from_secs(2));
                let mut bad = 0u64;
                let mut first = None;
                for i in 0..len / 8 {
                    if rd(p, i as u64 * 8) != word(i) {
                        bad += 1;
                        first.get_or_insert(i * 8);
                    }
                }
                checks += 1;
                if bad > 0 {
                    corrupt += bad;
                    println!("[canary] {} CORRUPTION: {bad} word(s) of {mib} MiB changed, first at +{:#x}", ts(), first.unwrap());
                }
            }
            vfree(p);
            cyc += 1;
            println!("[canary] {} cycle {cyc}: {checks} checks, {corrupt} corrupt words so far", ts());
        }
        if corrupt > 0 { Err(format!("{corrupt} corrupt words")) } else { Ok(()) }
    }

    /// A scheduling-stall monitor: sleep 1 ms in a loop and report every
    /// oversleep past the threshold, and the worst per minute. A guest that
    /// hangs or stalls during an allocation shows up here before ssh notices.
    fn jitter(a: &[String]) -> Result<(), String> {
        let secs: u64 = opt(a, "--secs").unwrap_or("3600").parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
        let thr = Duration::from_millis(opt(a, "--threshold-ms").unwrap_or("50").parse().map_err(|e: std::num::ParseIntError| e.to_string())?);
        // Time-critical, so that only a stall of the whole system (a kernel
        // lock, a DPC storm, the host descheduling the vCPU) delays it, and
        // not the instruments' own busy threads.
        unsafe {
            SetPriorityClass(GetCurrentProcess(), 0x80);
            SetThreadPriority(GetCurrentThread(), 15);
        }
        let end = Instant::now() + Duration::from_secs(secs);
        let (mut worst, mut minute, mut over) = (Duration::ZERO, Instant::now(), 0u64);
        while Instant::now() < end {
            let t = Instant::now();
            std::thread::sleep(Duration::from_millis(1));
            let d = t.elapsed();
            worst = worst.max(d);
            if d > thr {
                over += 1;
                println!("[jitter] {} STALL {:.1} ms", ts(), d.as_secs_f64() * 1e3);
            }
            if minute.elapsed() >= Duration::from_secs(60) {
                println!("[jitter] {} minute: worst sleep(1 ms) {:.1} ms, {over} stall(s) so far", ts(), worst.as_secs_f64() * 1e3);
                worst = Duration::ZERO;
                minute = Instant::now();
            }
        }
        Ok(())
    }

    fn hog(a: &[String]) -> Result<(), String> {
        let mib: usize = opt(a, "--mib").unwrap_or("1024").parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
        let secs: u64 = opt(a, "--secs").unwrap_or("60").parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
        let len = mib << 20;
        let t_end = Instant::now() + Duration::from_secs(secs);
        let mut rounds = 0;
        while Instant::now() < t_end {
            let p = match valloc(len) {
                Ok(p) => p,
                Err(e) => {
                    println!("[hog] {} {e}", ts());
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };
            for pass in 0..3u64 {
                for i in (0..len).step_by(4096) {
                    wr(p, i as u64, pass ^ i as u64);
                }
            }
            vfree(p);
            rounds += 1;
            let (total, avail) = sv_transport::winram::memory_status();
            println!("[hog] {} round {rounds}: {mib} MiB touched 3x; avail {} of {} MiB", ts(), avail >> 20, total >> 20);
        }
        Ok(())
    }
}
