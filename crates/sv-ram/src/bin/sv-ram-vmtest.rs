//! The NixOS VM test's tool (waycompose-design.md §108.18): one binary, three
//! roles.
//!
//! ```text
//! sv-ram-vmtest provider [--bar PATH]     in the nested guest: the GPU pool's
//!                                         provider, growing from hugetlb pages
//! sv-ram-vmtest ctl --boot FILE OP ...    the test driver (root on the VM
//!                                         host): a command to the provider
//!                                         through the control page
//! sv-ram-vmtest client map POOL --until FILE [--expect-fail]
//!                                         a client user: a region from the
//!                                         broker, mapped as the ICD maps it
//! sv-ram-vmtest client open PATH          a client user: try to open a file
//!                                         read-write (expected to fail)
//! ```
//!
//! The control page is page 10 of the bootstrap (inside the pool header's
//! 64 KiB that are never leased), so the driver reaches the provider through
//! the same shared memory as everything else, without a network:
//!
//! ```text
//!   0  u64 command seq (driver)       64  u64 answered seq (provider)
//!   8  u32 op                         72  u32 result: 0 ok, else refused
//!  12  u32 k                         128  answer text, NUL-terminated
//!  16  u64 bytes
//! ```
//!
//! Ops: `grow BYTES` (answer: `k=K` once the host mapped it), `lease K BYTES`
//! (a lease of the pool in region K: the stand-in for a GPU import),
//! `release OFFSET`, `retire K` (refused while the pool holds a lease in K),
//! `status K`, `check K` (the host's pattern seen in K's every unit).

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let r = match a.first().map(String::as_str) {
        Some("provider") => imp::provider(&a[1..]),
        Some("ctl") => imp::ctl(&a[1..]),
        Some("client") => imp::client(&a[1..]),
        _ => Err("usage: sv-ram-vmtest provider|ctl|client ...".into()),
    };
    match r {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sv-ram-vmtest: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sv-ram-vmtest runs on Linux only");
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::BTreeMap;
    use std::os::fd::AsFd;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    use sv_arena::pool::{Pool, PoolConfig, Purpose, REGION_STRIDE};
    use sv_ram::hugesub::HugeTlb;
    use sv_ram::provider::{Event, Phase, Provider};
    use sv_transport::lpstamp::UNIT;
    use sv_transport::poolhdr::PoolHeader;
    use sv_transport::region::SharedRegion;

    pub const TABLE_OFF: u64 = 4096;
    pub const CTL: u64 = 10 * 4096;
    /// Where in each 2 MiB unit the guest's and the host's patterns are,
    /// after the stamp.
    const GUEST_AT: u64 = 4096;
    const HOST_AT: u64 = 8192;

    fn opt(a: &[String], k: &str) -> Option<String> {
        a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned()
    }

    fn map_file(path: &str) -> Result<(std::fs::File, SharedRegion), String> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
        let len = f.metadata().map_err(|e| e.to_string())?.len();
        let r = SharedRegion::map_fd(f.as_fd(), 0, len as usize).map_err(|e| format!("mapping {path}: {e}"))?;
        Ok((f, r))
    }

    /// The ivshmem device's BAR 2 in the guest.
    fn find_bar() -> Result<String, String> {
        for e in std::fs::read_dir("/sys/bus/pci/devices").map_err(|e| e.to_string())? {
            let p = e.map_err(|e| e.to_string())?.path();
            let rd = |n: &str| std::fs::read_to_string(p.join(n)).unwrap_or_default().trim().to_string();
            if rd("vendor") == "0x1af4" && rd("device") == "0x1110" {
                return Ok(p.join("resource2").to_string_lossy().into_owned());
            }
        }
        Err("no ivshmem device (1af4:1110)".into())
    }

    fn pattern(epoch: u64, unit: u64, host: bool) -> u64 {
        (epoch.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ unit.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)) ^ if host { 0xFFFF } else { 0 }
    }

    fn w64(r: &SharedRegion, off: u64, v: u64) {
        r.atomic_u64(off).expect("inside").store(v, Ordering::SeqCst);
    }
    fn r64(r: &SharedRegion, off: u64) -> u64 {
        r.atomic_u64(off).expect("inside").load(Ordering::SeqCst)
    }

    fn answer(boot: &SharedRegion, seq: u64, ok: bool, text: &str) {
        let mut t = text.as_bytes().to_vec();
        t.truncate(250);
        t.push(0);
        let _ = boot.write_bytes(CTL + 128, &t);
        boot.atomic_u32(CTL + 72).expect("inside").store(u32::from(!ok), Ordering::SeqCst);
        w64(boot, CTL + 64, seq);
        println!("[provider] {} {}", if ok { "ok" } else { "refused" }, text);
    }

    pub fn provider(a: &[String]) -> Result<(), String> {
        let bar = match opt(a, "--bar") {
            Some(b) => b,
            None => find_bar()?,
        };
        let (_f, boot) = map_file(&bar)?;
        let (_f2, b2) = map_file(&bar)?;
        let run = std::process::id() as u64 | 0x5EED_0000_0000;
        PoolHeader::new(&boot).map_err(|e| e.to_string())?.publish(run);
        let mut p = Provider::start(b2, TABLE_OFF, HugeTlb::new(), run)?;
        PoolHeader::new(&boot).map_err(|e| e.to_string())?.set_region_table(TABLE_OFF);
        let cfg = PoolConfig { granule: 64 << 10, header: 64 << 10, reserve: 0, session_cap: u64::MAX / 2 };
        let mut pool = Pool::new(cfg, 64 << 10).map_err(|e| e.to_string())?;
        pool.open_session(1, "vmtest");
        println!("[provider] bar {bar}, run {run:016x}, table at +{TABLE_OFF}, control page at +{CTL}");
        let mut seen = r64(&boot, CTL);
        w64(&boot, CTL + 64, seen);
        let mut next_k = 1u32;
        // Regions waiting to be mapped: entry -> (k, seq to answer).
        let mut growing: BTreeMap<u32, (u32, u64)> = BTreeMap::new();
        let mut last_beat = Instant::now();
        loop {
            for ev in p.poll()? {
                match ev {
                    Event::Mapped { k, entry, rtt_us } => {
                        let r = &p.regions[&entry];
                        // The guest's pattern in every unit, for the client to see.
                        if let Some(v) = &r.view {
                            for u in 0..r.len / UNIT {
                                w64(v, u * UNIT + GUEST_AT, pattern(r.epoch, u, false));
                            }
                        }
                        pool.add_region(k, r.len).map_err(|e| e.to_string())?;
                        if let Some((k, seq)) = growing.remove(&entry) {
                            answer(&boot, seq, true, &format!("k={k} epoch={} mapped in {rtt_us} us", r.epoch));
                        }
                    }
                    Event::Refused { k, reason } => {
                        let seq = growing.values().find(|g| g.0 == k).map(|g| g.1);
                        growing.retain(|_, g| g.0 != k);
                        if let Some(seq) = seq {
                            answer(&boot, seq, false, &format!("k={k} refused by the host: {reason:?}"));
                        }
                    }
                    Event::Freed { k, wait_us } => println!("[provider] region {k} freed, {wait_us} us after its retire"),
                }
            }
            let seq = r64(&boot, CTL);
            if seq != seen {
                seen = seq;
                let op = boot.atomic_u32(CTL + 8).expect("inside").load(Ordering::SeqCst);
                let k = boot.atomic_u32(CTL + 12).expect("inside").load(Ordering::SeqCst);
                let bytes = r64(&boot, CTL + 16);
                let entry_of = |p: &Provider<HugeTlb>, k: u32| p.regions.values().filter(|r| r.k == k).map(|r| r.entry).max();
                match op {
                    1 => {
                        let k = next_k;
                        next_k += 1;
                        match p.announce(k, bytes) {
                            Ok(e) => {
                                println!("[provider] region {k}: {} MiB announced in entry {e}", bytes >> 20);
                                growing.insert(e, (k, seq));
                            }
                            Err(e) => answer(&boot, seq, false, &format!("grow: {e}")),
                        }
                    }
                    2 => {
                        // A lease in region k only: every other region drained meanwhile.
                        let others: Vec<u32> = pool.regions().into_iter().map(|r| r.0).filter(|&r| r != k).collect();
                        for &o in &others {
                            pool.drain(o);
                        }
                        let l = pool.lease(1, bytes, Purpose::Dedicated);
                        for &o in &others {
                            pool.undrain(o);
                        }
                        match l {
                            Ok(l) => answer(&boot, seq, true, &format!("offset={:#x} region={}", l.offset, l.region())),
                            Err(e) => answer(&boot, seq, false, &format!("lease: {e}")),
                        }
                    }
                    3 => match pool.release(bytes) {
                        Ok(_) => answer(&boot, seq, true, "released"),
                        Err(e) => answer(&boot, seq, false, &format!("release: {e}")),
                    },
                    4 => {
                        // §108.4 rule 2: a region that holds a lease is never retired.
                        pool.drain(k);
                        match pool.remove_region(k) {
                            Ok(_) => match entry_of(&p, k) {
                                Some(e) => {
                                    p.retire(e);
                                    answer(&boot, seq, true, &format!("k={k} retiring"));
                                }
                                None => answer(&boot, seq, false, &format!("no region {k}")),
                            },
                            Err(e) => {
                                pool.undrain(k);
                                answer(&boot, seq, false, &format!("retire {k}: {e}"));
                            }
                        }
                    }
                    5 => {
                        let s = match entry_of(&p, k).map(|e| &p.regions[&e]) {
                            None => "freed".to_string(),
                            Some(r) => match r.phase {
                                Phase::Announced => "announced".into(),
                                Phase::Mapped => "mapped".into(),
                                Phase::Retiring => "retiring".into(),
                            },
                        };
                        answer(&boot, seq, true, &s);
                    }
                    6 => {
                        let r = entry_of(&p, k).map(|e| &p.regions[&e]);
                        match r.and_then(|r| r.view.as_ref().map(|v| (r, v))) {
                            None => answer(&boot, seq, false, &format!("no mapped region {k}")),
                            Some((r, v)) => {
                                let bad = (0..r.len / UNIT).filter(|&u| r64(v, u * UNIT + HOST_AT) != pattern(r.epoch, u, true)).count();
                                answer(&boot, seq, bad == 0, &format!("k={k}: {bad} of {} units without the host's pattern", r.len / UNIT));
                            }
                        }
                    }
                    _ => answer(&boot, seq, false, &format!("op {op}?")),
                }
            }
            if last_beat.elapsed() >= Duration::from_millis(250) {
                p.beat();
                PoolHeader::new(&boot).map_err(|e| e.to_string())?.beat();
                last_beat = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn ctl(a: &[String]) -> Result<(), String> {
        let boot = opt(a, "--boot").ok_or("--boot FILE")?;
        let rest: Vec<&str> = a.iter().map(String::as_str).skip_while(|x| *x != "--boot").skip(2).collect();
        let num = |s: Option<&&str>| -> Result<u64, String> {
            let s = s.ok_or("a number is missing")?;
            let (n, m) = match s.strip_suffix('M') {
                Some(n) => (n, 1u64 << 20),
                None => (*s, 1),
            };
            let v = if let Some(h) = n.strip_prefix("0x") { u64::from_str_radix(h, 16) } else { n.parse() };
            v.map(|v| v * m).map_err(|e| format!("{s}: {e}"))
        };
        let (op, k, bytes) = match rest.as_slice() {
            ["grow", b] => (1, 0, num(Some(b))?),
            ["lease", k, b] => (2, num(Some(k))? as u32, num(Some(b))?),
            ["release", o] => (3, 0, num(Some(o))?),
            ["retire", k] => (4, num(Some(k))? as u32, 0),
            ["status", k] => (5, num(Some(k))? as u32, 0),
            ["check", k] => (6, num(Some(k))? as u32, 0),
            _ => return Err("ctl OP: grow BYTES | lease K BYTES | release OFFSET | retire K | status K | check K".into()),
        };
        let (_f, r) = map_file(&boot)?;
        let seq = r64(&r, CTL) + 1;
        r.atomic_u32(CTL + 8).expect("inside").store(op, Ordering::SeqCst);
        r.atomic_u32(CTL + 12).expect("inside").store(k, Ordering::SeqCst);
        w64(&r, CTL + 16, bytes);
        w64(&r, CTL, seq);
        let t = Instant::now();
        while r64(&r, CTL + 64) != seq {
            if t.elapsed() > Duration::from_secs(60) {
                return Err("no answer from the provider in 60 s".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut b = [0u8; 252];
        r.read_bytes(CTL + 128, &mut b).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]).into_owned();
        println!("{text}");
        if r.atomic_u32(CTL + 72).expect("inside").load(Ordering::SeqCst) == 0 { Ok(()) } else { Err(format!("refused: {text}")) }
    }

    pub fn client(a: &[String]) -> Result<(), String> {
        match a.first().map(String::as_str) {
            Some("open") => {
                let p = a.get(1).ok_or("client open PATH")?;
                match std::fs::OpenOptions::new().read(true).write(true).open(p) {
                    Ok(_) => Err(format!("OPENED {p}: this user must not reach it")),
                    Err(e) => {
                        println!("refused {p}: {e}");
                        Ok(())
                    }
                }
            }
            Some("map") => client_map(&a[1..]),
            _ => Err("client map POOL --until FILE | client open PATH".into()),
        }
    }

    /// What the ICD does with a guest-RAM region (sv-icd `map_ram_region`):
    /// `find`, each part at its fixed place in a reserved stride, the lease
    /// held while mapped; then `PROT_NONE` and `close`. Meanwhile it checks
    /// once every 100 ms that the mapping is still at its place and still
    /// shows the guest's pattern.
    fn client_map(a: &[String]) -> Result<(), String> {
        let pool = a.first().ok_or("client map POOL")?;
        let until = opt(a, "--until").ok_or("--until FILE")?;
        let expect_fail = a.iter().any(|x| x == "--expect-fail");
        let (r, fds) = match wc_broker::client::find_region(pool) {
            Ok(x) => x,
            Err(e) if expect_fail => {
                println!("refused {pool}: {e}");
                return Ok(());
            }
            Err(e) => return Err(format!("{pool}: {e}")),
        };
        if expect_fail {
            return Err(format!("GOT {pool} (broker region {}): this user must not", r.id));
        }
        let k: u64 = pool.rsplit('.').next().and_then(|s| s.parse().ok()).ok_or("POOL is gpu.<k>")?;
        let view = SharedRegion::reserve((REGION_STRIDE * (k + 1)) as usize).map_err(|e| e.to_string())?;
        let at = k * REGION_STRIDE;
        for (p, fd) in r.pieces.iter().zip(&fds) {
            view.map_fixed(at + p.offset, fd.as_fd(), 0, p.len).map_err(|e| format!("part at {}: {e}", p.offset))?;
        }
        drop(fds);
        let base = unsafe { view.base_ptr() } as usize + at as usize;
        let units = r.len / UNIT;
        let guest_ok = |v: &SharedRegion| (0..units).all(|u| r64(v, at + u * UNIT + GUEST_AT) == pattern(r.epoch, u, false));
        if !guest_ok(&view) {
            return Err(format!("{pool}: the guest's pattern is not in every unit"));
        }
        for u in 0..units {
            w64(&view, at + u * UNIT + HOST_AT, pattern(r.epoch, u, true));
        }
        println!(
            "MAPPED {pool} broker region {} epoch {} {} MiB in {} part(s) at {base:#x}; the guest's pattern seen, the host's written",
            r.id,
            r.epoch,
            r.len >> 20,
            r.pieces.len()
        );
        let mut checks = 0u64;
        while std::path::Path::new(&until).exists() {
            std::thread::sleep(Duration::from_millis(100));
            let now = unsafe { view.base_ptr() } as usize + at as usize;
            if now != base || !guest_ok(&view) {
                return Err(format!("{pool}: MOVED or changed after {checks} checks (base {base:#x} -> {now:#x})"));
            }
            checks += 1;
        }
        view.unmap_fixed(at, r.len).map_err(|e| e.to_string())?;
        if let Err(e) = wc_broker::client::close_region(r.id) {
            // A daemon restarted meanwhile: the lease ended with its connection.
            println!("close {pool}: {e}");
        }
        println!("STABLE {pool}: same address and bytes over {checks} checks; unmapped, lease closed");
        Ok(())
    }
}
