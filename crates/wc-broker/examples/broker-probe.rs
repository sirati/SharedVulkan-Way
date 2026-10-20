//! A probe of the Windows broker on a real guest (§111.8's VM tests): each
//! subcommand is one thing a user's process tries, and prints what happened
//! as `RESULT key=value ...` lines a harness can grep.
//!
//! ```text
//! broker-probe hold DEVICE SECS        take DEVICE from the broker, map it if
//!                                      it is ivshmem, hold it SECS seconds
//! broker-probe try DEVICE              ask the broker for DEVICE once
//! broker-probe direct SLOT|PORT        open the device without the broker
//! broker-probe alloc POOL MIB SECS     a region; write a marker; hold it
//! broker-probe steal PID HANDLE ID     another user's region: by its handle
//!                                      in PID, and by the broker's `open ID`
//! broker-probe adopt POOL              adopt this user's orphans of POOL, read and free them
//! broker-probe sign INSTALL            the broker's signature and identity
//! broker-probe sign-check INSTALL      verify current/retired keys from stdin, metadata only
//! broker-probe rtt N                   N stats requests, timed
//! broker-probe bw SLOT direct|broker   memcpy bandwidth through the mapped BAR
//! ```
#[cfg(not(windows))]
fn main() {
    eprintln!("broker-probe is a Windows program");
}

#[cfg(windows)]
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let r = win::run(&a);
    if let Err(e) = r {
        println!("RESULT error={e:?}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod win {
    use std::os::windows::io::{AsRawHandle, IntoRawHandle};
    use std::time::{Duration, Instant};
    use wc_broker::client::Client;
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    use windows_sys::Win32::System::Memory::{FILE_MAP_ALL_ACCESS, MapViewOfFile};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE};

    fn client(role: &str) -> Result<Client, String> {
        let mut c = Client::connect(&wc_broker::client::path()).map_err(|e| e.to_string())?;
        let h = c.hello(role).map_err(|e| e.to_string())?;
        println!("RESULT hello user={} pid={}", h.user, h.pid);
        Ok(c)
    }

    fn slot(name: &str) -> Option<u8> {
        match name {
            "arena" => Some(0x1c),
            "gpu" => Some(0x1d),
            _ => None,
        }
    }

    pub fn run(a: &[String]) -> Result<(), String> {
        let arg = |i: usize| a.get(i).cloned().unwrap_or_default();
        match arg(0).as_str() {
            "raw-acl-fixture" => {
                if std::env::var("WC_PRIVATE_RAW_ACL_FIXTURE").as_deref() != Ok("yes") {
                    return Err("private disposable raw ACL fixture opt-in required".into());
                }
                use wc_broker::sys::windows::{DeviceLoc, acl, default_devices};
                let permissive = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGW;;;WD)";
                let part = arg(1);
                if part != "live" && part != "pnp" { return Err("fixture selects live or pnp".into()); }
                let target = arg(2);
                if !["", "wcbell", "svbell"].contains(&target.as_str()) { return Err("fixture target is bars, wcbell or svbell".into()); }
                for d in default_devices() {
                    if target.is_empty() {
                        if !matches!(d.loc, DeviceLoc::IvshmemSlot(_)) { continue; }
                    } else if d.name != target { continue; }
                    let f = acl::find(&d).map_err(|e| e.to_string())?.ok_or("missing private-clone device")?;
                    if part == "live" { acl::set_live(&f.live, permissive)?; }
                    else { acl::set_property(&f.instance, Some(permissive))?; }
                    println!("RESULT private-fixture-permissive-{part} {}", f.instance);
                }
                Ok(())
            }
            "protect-raw" | "protect-devices" => {
                use wc_broker::sys::windows::{DeviceLoc, acl, default_devices};
                for d in default_devices() {
                    if arg(0) == "protect-raw" && !matches!(d.loc, DeviceLoc::IvshmemSlot(_)) { continue; }
                    let f = acl::find(&d).map_err(|e| e.to_string())?.ok_or("missing private-clone device")?;
                    acl::set_property(&f.instance, Some(acl::DEVICE_SDDL))?;
                    acl::set_live(&f.live, acl::DEVICE_SDDL)?;
                    println!("RESULT protected-raw {}", f.instance);
                }
                Ok(())
            }
            "sentinel-write" | "sentinel-read" => {
                let mut c = client("raw-bar-sentinel")?;
                let h = c.device(&arg(1)).map_err(|e| e.to_string())?;
                // SAFETY: the broker moved an unmapped ivshmem device here.
                let d =
                    unsafe { wc_handle::IvshmemDevice::from_raw(h.into_raw_handle() as _, arg(1)) };
                let (view, size, _) =
                    wc_handle::map_ivshmem_device(d).map_err(|e| e.to_string())?;
                let marker = b"wc-private-clone-prior-user-sentinel";
                let offset = 65536;
                if size < (offset + marker.len()) as u64 {
                    return Err("BAR too short".into());
                }
                if arg(0) == "sentinel-write" {
                    // SAFETY: checked above; this probe is only for a private
                    // clone with no compositor or host/GPU writer.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            marker.as_ptr(),
                            view.ptr().cast_mut().add(offset),
                            marker.len(),
                        )
                    };
                    println!("RESULT sentinel-written size={size}");
                } else {
                    // SAFETY: checked above; no private-clone writer is live.
                    let found =
                        unsafe { std::slice::from_raw_parts(view.ptr().add(offset), marker.len()) };
                    println!(
                        "RESULT prior-user-sentinel-visible={} size={size}",
                        found == marker
                    );
                }
                Ok(())
            }
            "hold" => {
                let mut c = client("probe")?;
                let t = Instant::now();
                let h = c.device(&arg(1)).map_err(|e| e.to_string())?;
                let us = t.elapsed().as_micros();
                if slot(&arg(1)).is_some() {
                    // SAFETY: the broker moved an unmapped ivshmem device here.
                    let d = unsafe { wc_handle::IvshmemDevice::from_raw(h.into_raw_handle() as _, arg(1)) };
                    let (view, size, _) = wc_handle::map_ivshmem_device(d).map_err(|e| e.to_string())?;
                    // SAFETY: inside the view.
                    unsafe { std::ptr::write_volatile(view.ptr().cast_mut().add(8192), 0xA5) };
                    println!("RESULT held device={} us={us} mapped={size}", arg(1));
                    std::thread::sleep(Duration::from_secs(arg(2).parse().unwrap_or(60)));
                    drop(view);
                } else {
                    println!("RESULT held device={} us={us}", arg(1));
                    std::thread::sleep(Duration::from_secs(arg(2).parse().unwrap_or(60)));
                    drop(h);
                }
                Ok(())
            }
            "try" => {
                let mut c = client("probe")?;
                match c.device(&arg(1)) {
                    Ok(_) => println!("RESULT got device={}", arg(1)),
                    Err(e) => println!("RESULT refused device={} why={:?}", arg(1), e.to_string()),
                }
                Ok(())
            }
            "direct" => {
                let x = arg(1);
                let r = if let Some(s) = slot(&x) {
                    wc_handle::open_ivshmem_slot(s).map(|_| ()).map_err(|e| e.to_string())
                } else {
                    sv_transport::winpipe::open_virtio_port_handle(&x, 200).map(|_| ()).map_err(|e| e.to_string())
                };
                match r {
                    Ok(()) => println!("RESULT direct-opened {x}"),
                    Err(e) => println!("RESULT direct-refused {x} why={e:?}"),
                }
                Ok(())
            }
            "alloc" => {
                let mut c = client("probe")?;
                let mib: u64 = arg(2).parse().unwrap_or(4);
                let (r, hs) = c.alloc(&arg(1), mib << 20, false).map_err(|e| e.to_string())?;
                // SAFETY: a section of the length the broker said.
                let v = unsafe { MapViewOfFile(hs[0].as_raw_handle() as _, FILE_MAP_ALL_ACCESS, 0, 0, r.len as usize) };
                // SAFETY: inside the view.
                unsafe { std::ptr::copy_nonoverlapping(b"user1-secret".as_ptr(), v.Value.cast::<u8>(), 12) };
                println!(
                    "RESULT alloc id={} pid={} handle={:#x}",
                    r.id,
                    std::process::id(),
                    hs[0].as_raw_handle() as usize
                );
                std::thread::sleep(Duration::from_secs(arg(3).parse().unwrap_or(60)));
                Ok(())
            }
            "steal" => {
                let pid: u32 = arg(1).parse().map_err(|_| "pid")?;
                let h = usize::from_str_radix(arg(2).trim_start_matches("0x"), 16).map_err(|_| "handle")?;
                // SAFETY: plain call.
                let p = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
                if p.is_null() {
                    println!("RESULT openprocess-refused pid={pid} why={:?}", std::io::Error::last_os_error().to_string());
                } else {
                    let mut mine: HANDLE = std::ptr::null_mut();
                    // SAFETY: a process handle and a value in it.
                    let ok = unsafe { DuplicateHandle(p, h as HANDLE, GetCurrentProcess(), &mut mine, 0, 0, DUPLICATE_SAME_ACCESS) };
                    println!("RESULT openprocess-ALLOWED pid={pid} duplicate={}", ok != 0);
                }
                let mut c = client("probe")?;
                match c.open(arg(3).parse().unwrap_or(0)) {
                    Ok(_) => println!("RESULT broker-open-ALLOWED id={}", arg(3)),
                    Err(e) => println!("RESULT broker-open-refused id={} why={:?}", arg(3), e.to_string()),
                }
                match c.adopt("gpu") {
                    Ok(v) => println!("RESULT adopt-gpu got={}", v.len()),
                    Err(e) => println!("RESULT adopt-gpu err={e}"),
                }
                Ok(())
            }
            "adopt" => {
                let mut c = client("probe")?;
                let v = c.adopt(&arg(1)).map_err(|e| e.to_string())?;
                println!("RESULT adopted pool={} n={}", arg(1), v.len());
                for (id, len) in v {
                    let (_, hs) = c.open(id).map_err(|e| e.to_string())?;
                    // SAFETY: a section of `len` bytes.
                    let p = unsafe { MapViewOfFile(hs[0].as_raw_handle() as _, FILE_MAP_ALL_ACCESS, 0, 0, len as usize) };
                    // SAFETY: inside the view.
                    let b = unsafe { std::slice::from_raw_parts(p.Value.cast::<u8>(), 12) };
                    println!("RESULT region id={id} len={len} starts={:?}", String::from_utf8_lossy(b));
                    c.close(id).map_err(|e| e.to_string())?;
                    c.free(id).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            "sign" | "sign-check" => {
                let mut c = client("probe")?;
                let req = wc_broker::proto::SignReq {
                    install: arg(1),
                    session: format!("{}-1", arg(1)),
                    index: 1,
                    arena_run: 0x77,
                    arena_pages: 16384,
                    outbound: 1 << 20,
                    rings_run: 0x77,
                    rings_pages: 1536,
                    gpu: false,
                    seq: 1,
                };
                let t = Instant::now();
                let (auth, id) = c.sign(&req).map_err(|e| e.to_string())?;
                let us = t.elapsed().as_micros();
                if arg(0) == "sign-check" {
                    use std::io::Read;
                    let mut input = String::new();
                    std::io::stdin().read_to_string(&mut input).map_err(|_| "reading verification input failed")?;
                    let mut lines = input.lines();
                    let current = lines.next().and_then(wc_handle::macboot::parse_key).ok_or("invalid current verification key")?;
                    let retired = lines.next().and_then(wc_handle::macboot::parse_key).ok_or("invalid retired verification key")?;
                    if lines.next().is_some() { return Err("unexpected verification input".into()); }
                    let rec = wc_handle::macboot::Record {
                        session: req.session.clone(), arena_run: req.arena_run,
                        arena_pages: req.arena_pages, outbound: req.outbound,
                        rings_run: req.rings_run, rings_pages: req.rings_pages, gpu: req.gpu,
                    };
                    let current_ok = auth.verifies_identity(&rec, req.index, Some(&id), &current);
                    let retired_ok = auth.verifies_identity(&rec, req.index, Some(&id), &retired);
                    println!("RESULT rotated-signature current={current_ok} retired={retired_ok}");
                    if !current_ok || retired_ok { return Err("key rotation verification failed".into()); }
                    return Ok(());
                }
                let mac: String = auth.mac.iter().map(|b| format!("{b:02x}")).collect();
                println!("RESULT signed us={us} identity={:?} mac={mac}", id.text());
                // With the key (a test's), check what the host's sessiond
                // checks: this identity verifies, a rewritten one does not.
                if let Some(key) = wc_handle::macboot::parse_key(&arg(2)) {
                    let rec = wc_handle::macboot::Record {
                        session: req.session.clone(),
                        arena_run: 0x77,
                        arena_pages: 16384,
                        outbound: 1 << 20,
                        rings_run: 0x77,
                        rings_pages: 1536,
                        gpu: false,
                    };
                    let own = auth.verifies_identity(&rec, 1, Some(&id), &key);
                    let other = wc_handle::macboot::Identity {
                        os: "windows".into(),
                        user: "intruder".into(),
                        id: "S-1-5-21-1-1-1-9999".into(),
                    };
                    let rewritten = auth.verifies_identity(&rec, 1, Some(&other), &key);
                    let none = auth.verifies_identity(&rec, 1, None, &key);
                    println!("RESULT verify own={own} rewritten={rewritten} removed={none}");
                }
                Ok(())
            }
            "rtt" => {
                let mut c = client("probe")?;
                let n: usize = arg(1).parse().unwrap_or(2000);
                for _ in 0..50 {
                    c.stats().map_err(|e| e.to_string())?;
                }
                let mut v: Vec<u128> = (0..n)
                    .map(|_| {
                        let t = Instant::now();
                        let _ = c.stats();
                        t.elapsed().as_nanos()
                    })
                    .collect();
                v.sort_unstable();
                let mean = v.iter().sum::<u128>() / n as u128;
                println!(
                    "RESULT rtt n={n} mean_us={:.1} p50_us={:.1} p99_us={:.1}",
                    mean as f64 / 1000.0,
                    v[n / 2] as f64 / 1000.0,
                    v[n * 99 / 100] as f64 / 1000.0
                );
                let t = Instant::now();
                let (r, _h) = c.alloc("gpu", 2 << 20, false).map_err(|e| e.to_string())?;
                let alloc_us = t.elapsed().as_micros();
                c.free(r.id).map_err(|e| e.to_string())?;
                println!("RESULT alloc2mib_us={alloc_us}");
                Ok(())
            }
            "bw" => {
                let s = slot(&arg(1)).ok_or("bw arena|gpu")?;
                let t0 = Instant::now();
                let (view, size, path) = if arg(2) == "broker" {
                    let mut c = client("probe")?;
                    let h = c.device(&arg(1)).map_err(|e| e.to_string())?;
                    // SAFETY: as in `hold`.
                    let d = unsafe { wc_handle::IvshmemDevice::from_raw(h.into_raw_handle() as _, arg(1)) };
                    wc_handle::map_ivshmem_device(d).map_err(|e| e.to_string())?
                } else {
                    wc_handle::map_ivshmem_slot(s).map_err(|e| e.to_string())?
                };
                let open_us = t0.elapsed().as_micros();
                let n = (size as usize).min(32 << 20);
                let src = vec![0x5Au8; n];
                let mut dst = vec![0u8; n];
                let p = view.ptr().cast_mut();
                let t = Instant::now();
                let reps = 20;
                for _ in 0..reps {
                    // SAFETY: inside the view, `n` bytes.
                    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), p, n) };
                    // SAFETY: as above.
                    unsafe { std::ptr::copy_nonoverlapping(p, dst.as_mut_ptr(), n) };
                }
                let secs = t.elapsed().as_secs_f64();
                println!(
                    "RESULT bw how={} path={path:?} open_map_us={open_us} write+read_gib_s={:.2}",
                    arg(2),
                    (2 * reps * n) as f64 / secs / (1u64 << 30) as f64
                );
                Ok(())
            }
            _ => Err("usage: see the source".into()),
        }
    }
}
