//! The broker on a real named pipe (§111.8): identities from tokens, handles
//! moved and copied with DuplicateHandle, and a process's exit seen through
//! its handle. Runs under Wine and on Windows. The second process is this
//! test binary run again (`helper`), so it has a pid of its own.
//!
//! The device is a stand-in: a file opened without sharing, so that, like a
//! virtio-serial port, it has one opener at a time.
#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use wc_broker::client::Client;
use wc_broker::server::{Broker, Config};
use wc_broker::sys::windows::{DeviceAt, DeviceLoc, PIPE_SDDL, Windows};
use wc_broker::{Allow, DeviceSpec, Release};
use windows_sys::Win32::System::Memory::{FILE_MAP_ALL_ACCESS, MapViewOfFile, UnmapViewOfFile};

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("wc-broker-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A broker in this process on a pipe of its own; returns the pipe's name.
fn start(d: &std::path::Path, tag: &str) -> String {
    std::fs::write(d.join("dev"), vec![0u8; 64]).unwrap();
    let keys = d.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(keys.join("wcivs.key"), "07".repeat(32)).unwrap();
    let pipe = format!(r"\\.\pipe\wc-broker-test-{tag}-{}", std::process::id());
    // The test runs as an ordinary user, who is not in IU under Wine or a
    // batch logon: the pipe admits the creator's own account here.
    let sddl = PIPE_SDDL.replace("(A;;0x0012008b;;;IU)", "(A;;0x0012008b;;;IU)(A;;0x0012008b;;;OW)(A;;0x0012008b;;;AU)");
    let w = Windows::bind(
        &pipe,
        &sddl,
        vec![DeviceAt { name: "dev".into(), loc: DeviceLoc::Path(d.join("dev").to_string_lossy().into_owned()) }],
    )
    .unwrap();
    let b = Broker::new(
        w,
        Config {
            devices: vec![DeviceSpec { name: "dev".into(), allow: Allow::AnyUser }],
            services: Vec::new(),
            keys: Some(keys),
            release: Release::OwnerFree,
            budget: Some(256 << 20),
            run: 7,
        },
    );
    std::thread::spawn(move || b.serve());
    // The first instance exists once `serve` has made it.
    let t = Instant::now();
    while Client::connect(&pipe).is_err() {
        assert!(t.elapsed() < Duration::from_secs(5), "the broker's pipe never appeared");
        std::thread::sleep(Duration::from_millis(10));
    }
    pipe
}

fn wait_for(mut f: impl FnMut() -> bool) -> Option<Duration> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        if f() {
            return Some(t.elapsed());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

fn map(h: &OwnedHandle, len: usize) -> *mut u8 {
    // SAFETY: a section handle of at least `len` bytes.
    let v = unsafe { MapViewOfFile(h.as_raw_handle() as _, FILE_MAP_ALL_ACCESS, 0, 0, len) };
    assert!(!v.Value.is_null(), "MapViewOfFile: {}", std::io::Error::last_os_error());
    v.Value.cast()
}

fn unmap(p: *mut u8) {
    // SAFETY: a view `map` made.
    unsafe { UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: p.cast() }) };
}

/// The second process. `WC_BROKER_TEST_DO` says what it does:
/// * `hold`: take the device, write its pid into it, say HELD, wait for stdin;
/// * `read ID`: open region ID, check it says `parent`, write `child`, say READ;
/// * `orphan`: alloc a region of pool `keep`, write `orphaned`, say ALLOC ID, wait.
#[test]
fn helper() {
    let Ok(pipe) = std::env::var("WC_BROKER_TEST_PIPE") else { return };
    let what = std::env::var("WC_BROKER_TEST_DO").unwrap();
    let mut c = Client::connect(&pipe).unwrap();
    c.hello("helper").unwrap();
    let w: Vec<&str> = what.split(' ').collect();
    // What the helper holds lives until stdin ends.
    let mut keep: Vec<OwnedHandle> = Vec::new();
    match w.as_slice() {
        ["hold"] => {
            let h = c.device("dev").unwrap();
            // The stand-in is opened overlapped, as a port is: an overlapped
            // write, as the doorbell code writes a port.
            write_overlapped(&h, format!("held by {}", std::process::id()).as_bytes());
            keep.push(h);
            println!("HELD");
        }
        ["read", id] => {
            let (r, hs) = c.open(id.parse().unwrap()).unwrap();
            let p = map(&hs[0], r.len as usize);
            // SAFETY: inside the view just made.
            let got = unsafe { std::slice::from_raw_parts(p, 6) }.to_vec();
            assert_eq!(&got, b"parent", "the child reads what the parent wrote");
            // SAFETY: as above.
            unsafe { std::ptr::copy_nonoverlapping(b"child!".as_ptr(), p.add(4096), 6) };
            unmap(p);
            println!("READ");
        }
        ["orphan"] => {
            let (r, hs) = c.alloc("keep", 1 << 20, false).unwrap();
            let p = map(&hs[0], r.len as usize);
            // SAFETY: inside the view.
            unsafe { std::ptr::copy_nonoverlapping(b"orphaned".as_ptr(), p, 8) };
            println!("ALLOC {}", r.id);
        }
        _ => panic!("{what}"),
    }
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    drop(keep);
}

fn spawn(pipe: &str, what: &str) -> (std::process::Child, BufReader<std::process::ChildStdout>, String) {
    let mut c = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "helper", "--nocapture", "--test-threads=1"])
        .env("WC_BROKER_TEST_PIPE", pipe)
        .env("WC_BROKER_TEST_DO", what)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(c.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "the helper ended before saying anything");
        if let Some(i) = ["HELD", "READ", "ALLOC"].iter().find_map(|k| line.find(k)) {
            line = line[i..].to_string();
            break;
        }
    }
    (c, out, line.trim().to_string())
}

/// hello; a device moved into a child, refused to this process while the
/// child holds it, and free the moment the child is killed.
#[test]
fn a_device_moves_into_its_holder_and_comes_back_when_it_is_killed() {
    let d = dir("device");
    let pipe = start(&d, "device");
    let mut me = Client::connect(&pipe).unwrap();
    let h = me.hello("test").unwrap();
    assert!(h.user.starts_with("sid:S-1-5-"), "{h:?}");
    assert_eq!(h.pid, std::process::id());
    let (mut child, _out, _) = spawn(&pipe, "hold");
    let cpid = child.id();
    let e = me.device("dev").unwrap_err().to_string();
    assert!(e.contains(&format!("held by process {cpid}")), "{e}");
    // The broker kept no handle on the device: this process cannot open the
    // file either (one opener), because the child has it.
    assert!(std::fs::OpenOptions::new().write(true).open(d.join("dev")).is_err(), "the child is the device's only opener");
    let t = Instant::now();
    child.kill().unwrap();
    let _ = child.wait();
    let freed = wait_for(|| me.device("dev").is_ok()).expect("the device came back");
    eprintln!("device free {:?} after the holder was killed ({:?} after kill())", freed, t.elapsed());
    let s = me.stats().unwrap();
    assert!(s.contains("devices-held=1"), "{s}");
    std::fs::remove_dir_all(&d).ok();
}

/// A region (an unnamed section) made by the broker, mapped in two
/// processes, the same pages in both; an owner's death leaves an orphan the
/// same user adopts; the release rule holds it till then.
#[test]
fn a_section_is_shared_by_handle_and_an_orphan_is_adopted() {
    let d = dir("region");
    let pipe = start(&d, "region");
    let mut me = Client::connect(&pipe).unwrap();
    me.hello("test").unwrap();
    let (r, hs) = me.alloc("gpu", 1 << 20, false).unwrap();
    assert_eq!((r.pool.as_str(), r.len, hs.len()), ("gpu", 1 << 20, 1));
    let p = map(&hs[0], 1 << 20);
    // SAFETY: inside the view.
    unsafe { std::ptr::copy_nonoverlapping(b"parent".as_ptr(), p, 6) };
    let (mut child, _out, _) = spawn(&pipe, &format!("read {}", r.id));
    // SAFETY: inside the view; the child wrote it before saying READ.
    let back = unsafe { std::slice::from_raw_parts(p.add(4096), 6) }.to_vec();
    assert_eq!(&back, b"child!", "the child's write is in this process's view");
    let _ = child.stdin.take();
    let _ = child.wait();
    unmap(p);

    let (mut orphaner, _o, said) = spawn(&pipe, "orphan");
    let id: u64 = said.strip_prefix("ALLOC ").unwrap().parse().unwrap();
    orphaner.kill().unwrap();
    let _ = orphaner.wait();
    assert!(wait_for(|| me.stats().unwrap().contains("orphans=1")).is_some(), "{}", me.stats().unwrap());
    let adopted = me.adopt("keep").unwrap();
    assert_eq!(adopted, vec![(id, 1 << 20)]);
    let (_, hs2) = me.open(id).unwrap();
    let q = map(&hs2[0], 1 << 20);
    // SAFETY: inside the view.
    assert_eq!(unsafe { std::slice::from_raw_parts(q, 8) }, b"orphaned", "the orphan's pages were kept");
    unmap(q);
    me.close(id).unwrap();
    me.free(id).unwrap();
    me.free(r.id).unwrap();
    assert!(me.stats().unwrap().contains("regions=0"), "{}", me.stats().unwrap());
    let e = me.alloc("gpu", 2 << 20, true).map(|_| ()).unwrap_err().to_string();
    eprintln!("alloc large without SeLockMemoryPrivilege: {e}");
    std::fs::remove_dir_all(&d).ok();
}

/// `sign`: the identity is this process's token's, and a record whose
/// identity is rewritten does not verify.
#[test]
fn sign_covers_the_callers_own_identity() {
    use wc_handle::macboot::{Auth, Identity, Record};
    let d = dir("sign");
    let pipe = start(&d, "sign");
    let mut me = Client::connect(&pipe).unwrap();
    let h = me.hello("test").unwrap();
    let req = wc_broker::proto::SignReq {
        install: "wcivs".into(),
        session: "wcivs-1".into(),
        index: 2,
        arena_run: 0x77,
        arena_pages: 32768,
        outbound: 1 << 20,
        rings_run: 0x77,
        rings_pages: 1536,
        gpu: true,
        seq: 5,
    };
    let (auth, id) = me.sign(&req).unwrap();
    assert_eq!(id.os, "windows");
    assert_eq!(format!("sid:{}", id.id), h.user, "the identity is the caller's SID");
    eprintln!("signed for {}", id.text());
    let rec = Record { session: "wcivs-1".into(), arena_run: 0x77, arena_pages: 32768, outbound: 1 << 20, rings_run: 0x77, rings_pages: 1536, gpu: true };
    let key = [7u8; 32];
    assert!(auth.verifies_identity(&rec, 2, Some(&id), &key));
    let other = Identity { user: "someone-else".into(), id: "S-1-5-21-9-9-9-1002".into(), ..id.clone() };
    assert!(!auth.verifies_identity(&rec, 2, Some(&other), &key));
    assert!(!Auth { ..auth.clone() }.verifies_identity(&rec, 3, Some(&id), &key));
    assert!(me.sign(&wc_broker::proto::SignReq { install: "nope".into(), ..req }).is_err());
    std::fs::remove_dir_all(&d).ok();
}

/// The control round trip of a request (§111.8's "no slowdown": only
/// requests pay it, never the data path).
#[test]
fn the_control_round_trip() {
    let d = dir("rtt");
    let pipe = start(&d, "rtt");
    let mut me = Client::connect(&pipe).unwrap();
    me.hello("test").unwrap();
    for _ in 0..50 {
        me.stats().unwrap();
    }
    let mut v: Vec<u128> = (0..2000)
        .map(|_| {
            let t = Instant::now();
            me.stats().unwrap();
            t.elapsed().as_nanos()
        })
        .collect();
    v.sort_unstable();
    let mean = v.iter().sum::<u128>() / v.len() as u128;
    eprintln!(
        "broker RTT over {} stats requests: mean {} us, p50 {} us, p99 {} us",
        v.len(),
        mean / 1000,
        v[v.len() / 2] / 1000,
        v[v.len() * 99 / 100] / 1000
    );
    // A region alloc+free, the request a growth makes.
    let t = Instant::now();
    for _ in 0..200 {
        let (r, _h) = me.alloc("gpu", 2 << 20, false).unwrap();
        me.free(r.id).unwrap();
    }
    eprintln!("alloc + free of a 2 MiB region: {} us each", t.elapsed().as_micros() / 200);
    std::fs::remove_dir_all(&d).ok();
}

fn write_overlapped(h: &OwnedHandle, bytes: &[u8]) {
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::CreateEventW;
    // SAFETY: a fresh event, a zeroed OVERLAPPED at offset 0, a live buffer;
    // waited for before returning.
    unsafe {
        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.hEvent = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        let mut n = 0u32;
        WriteFile(h.as_raw_handle() as _, bytes.as_ptr(), bytes.len() as u32, &mut n, &mut ov);
        assert!(GetOverlappedResult(h.as_raw_handle() as _, &ov, &mut n, 1) != 0, "{}", std::io::Error::last_os_error());
        assert_eq!(n as usize, bytes.len());
        windows_sys::Win32::Foundation::CloseHandle(ov.hEvent);
    }
}

/// A holder, for `a_crashed_broker_gets_its_regions_back_from_the_holder`.
#[test]
fn holder_main() {
    let Ok(pipe) = std::env::var("WC_BROKER_TEST_HOLDER_PIPE") else { return };
    wc_broker::sys::windows::holder::serve(&pipe, wc_broker::sys::windows::holder::HOLDER_SDDL).unwrap();
}

/// A broker with a holder, in a process of its own, until it is killed.
#[test]
fn broker_main() {
    let Ok(pipe) = std::env::var("WC_BROKER_TEST_BROKER_PIPE") else { return };
    let hpipe = std::env::var("WC_BROKER_TEST_HOLDER_PIPE").unwrap();
    let sddl = PIPE_SDDL.replace("(A;;0x0012008b;;;IU)", "(A;;0x0012008b;;;IU)(A;;0x0012008b;;;OW)(A;;0x0012008b;;;AU)");
    let spawn = wc_broker::sys::windows::holder::Spawn {
        exe: std::env::current_exe().unwrap(),
        args: vec!["--exact".into(), "holder_main".into(), "--nocapture".into(), "--test-threads=1".into()],
        env: vec![("WC_BROKER_TEST_HOLDER_PIPE".into(), hpipe.clone())],
    };
    let w = Windows::bind(&pipe, &sddl, Vec::new()).unwrap().with_holder(&hpipe, spawn).unwrap();
    println!("HOLDER {}", w.holder_pid().unwrap());
    let _ = std::io::stdout().flush();
    let b = Broker::new(
        w,
        Config { devices: Vec::new(), services: Vec::new(), keys: None, release: Release::OwnerFree, budget: None, run: 9 },
    );
    b.serve().unwrap();
}

fn spawn_broker(pipe: &str, hpipe: &str) -> (std::process::Child, u32) {
    let mut c = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "broker_main", "--nocapture", "--test-threads=1"])
        .env("WC_BROKER_TEST_BROKER_PIPE", pipe)
        .env("WC_BROKER_TEST_HOLDER_PIPE", hpipe)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(c.stdout.take().unwrap());
    let mut line = String::new();
    let pid = loop {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "the broker ended before starting");
        if let Some(i) = line.find("HOLDER ") {
            break line[i + 7..].trim().parse().unwrap();
        }
    };
    std::thread::spawn(move || for _ in out.lines() {});
    (c, pid)
}

/// §111.8's holder: a region whose provider has let go survives the
/// broker's crash, comes back as an orphan in the next broker, and survives
/// the holder's crash too.
#[test]
fn a_crashed_broker_gets_its_regions_back_from_the_holder() {
    let tag = std::process::id();
    let pipe = format!(r"\\.\pipe\wc-broker-test-hb-{tag}");
    let hpipe = format!(r"\\.\pipe\wc-broker-test-holder-{tag}");
    let (mut b1, h1) = spawn_broker(&pipe, &hpipe);
    let connect = || {
        let t = Instant::now();
        loop {
            match Client::connect(&pipe) {
                Ok(c) => break c,
                Err(e) => {
                    assert!(t.elapsed() < Duration::from_secs(10), "{e}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    };
    let mut c = connect();
    c.hello("provider").unwrap();
    let (r, hs) = c.alloc("keep", 1 << 20, false).unwrap();
    let p = map(&hs[0], 1 << 20);
    // SAFETY: inside the view.
    unsafe { std::ptr::copy_nonoverlapping(b"survives".as_ptr(), p, 8) };
    unmap(p);
    drop(hs);
    drop(c);
    // Now only the broker and the holder hold the section. The broker dies.
    b1.kill().unwrap();
    let _ = b1.wait();
    let (mut b2, h2) = spawn_broker(&pipe, &hpipe);
    assert_eq!(h1, h2, "the second broker found the same holder");
    let mut c = connect();
    c.hello("provider").unwrap();
    assert!(c.stats().unwrap().contains("orphans=1"), "{}", c.stats().unwrap());
    assert_eq!(c.adopt("keep").unwrap(), vec![(r.id, 1 << 20)]);
    let (_, hs) = c.open(r.id).unwrap();
    let q = map(&hs[0], 1 << 20);
    // SAFETY: inside the view.
    assert_eq!(unsafe { std::slice::from_raw_parts(q, 8) }, b"survives", "the pages outlived the broker");
    unmap(q);
    drop(hs);
    c.close(r.id).unwrap();
    // The holder dies: the broker starts another and gives it the region.
    let k = std::process::Command::new("taskkill").args(["/F", "/PID", &h1.to_string()]).output();
    eprintln!("taskkill holder {h1}: {:?}", k.map(|o| o.status));
    std::thread::sleep(Duration::from_millis(500));
    // Kill this broker too: if the new holder had not been refilled, the
    // region would be gone now.
    b2.kill().unwrap();
    let _ = b2.wait();
    drop(c);
    let (mut b3, h3) = spawn_broker(&pipe, &hpipe);
    assert_ne!(h3, h1, "a new holder");
    let mut c = connect();
    c.hello("provider").unwrap();
    assert_eq!(c.adopt("keep").unwrap(), vec![(r.id, 1 << 20)], "the refilled holder kept it");
    let (_, hs) = c.open(r.id).unwrap();
    let q = map(&hs[0], 1 << 20);
    // SAFETY: inside the view.
    assert_eq!(unsafe { std::slice::from_raw_parts(q, 8) }, b"survives");
    unmap(q);
    drop(hs);
    c.close(r.id).unwrap();
    c.free(r.id).unwrap();
    b3.kill().unwrap();
    let _ = b3.wait();
    wc_broker::sys::windows::holder::quit(&hpipe).unwrap();
}
