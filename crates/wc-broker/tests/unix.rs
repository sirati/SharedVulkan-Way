//! The broker on a real Unix socket (§111.7): identities from the kernel,
//! fds over SCM_RIGHTS, and a process's exit seen by its pidfd or kqueue.
//! A second process is this test binary run again (`helper`), so it has a pid
//! of its own.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wc_broker::client::Client;
use wc_broker::server::{Broker, Config};
use wc_broker::sys::unix::{DevicePath, Unix};
use wc_broker::{Allow, DeviceSpec, Release, User};

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("wc-broker-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn start(d: &Path, other_only: bool) -> String {
    std::fs::write(d.join("arena"), vec![0u8; 4096]).unwrap();
    std::fs::write(d.join("port"), b"").unwrap();
    let sock = d.join("broker.sock");
    let unix = Unix::bind(
        &sock,
        vec![
            DevicePath { name: "arena".into(), path: d.join("arena") },
            DevicePath { name: "port".into(), path: d.join("port") },
        ],
    )
    .unwrap()
    .with_services(vec![DevicePath { name: "hub".into(), path: d.join("hub.sock") }]);
    let devices = vec![
        DeviceSpec { name: "arena".into(), allow: Allow::AnyUser },
        DeviceSpec {
            name: "port".into(),
            allow: if other_only { Allow::Users(vec![User::uid(4_000_000)]) } else { Allow::AnyUser },
        },
    ];
    std::fs::create_dir_all(d.join("keys")).unwrap();
    let services = vec![DeviceSpec { name: "hub".into(), allow: Allow::AnyUser }];
    let config = Config { devices, services, keys: Some(d.join("keys")), release: Release::OwnerFree, budget: Some(64 << 20), run: 7 };
    let b = Broker::new(unix, config);
    std::thread::spawn(move || b.serve());
    sock.to_string_lossy().into_owned()
}

fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(5) {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// The second process: takes the device named in `WC_BROKER_TEST_DEVICE`,
/// writes its pid into it, says so on stdout, and lives until stdin ends.
#[test]
fn helper() {
    let Ok(path) = std::env::var("WC_BROKER_TEST_SOCK") else { return };
    let mut c = Client::connect(&path).unwrap();
    c.hello("helper").unwrap();
    let fd = c.device(&std::env::var("WC_BROKER_TEST_DEVICE").unwrap()).unwrap();
    let mut f = std::fs::File::from(fd);
    write!(f, "held by {}", std::process::id()).unwrap();
    println!("HELD");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
}

fn spawn_helper(sock: &str, device: &str) -> (std::process::Child, std::process::ChildStdout) {
    let mut c = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "helper", "--nocapture", "--test-threads=1"])
        .env("WC_BROKER_TEST_SOCK", sock)
        .env("WC_BROKER_TEST_DEVICE", device)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = c.stdout.take().unwrap();
    let mut got = Vec::new();
    let mut b = [0u8; 256];
    while !String::from_utf8_lossy(&got).contains("HELD") {
        let n = out.read(&mut b).unwrap();
        assert!(n > 0, "the helper ended without the device: {}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&b[..n]);
    }
    (c, out)
}

/// A device is moved into one process; the next process is refused while
/// that one lives, and gets it the moment it exits. A subscriber hears both.
#[test]
fn a_device_comes_back_when_its_holder_exits() {
    let d = dir("device");
    let sock = start(&d, false);
    let mut me = Client::connect(&sock).unwrap();
    let h = me.hello("test").unwrap();
    assert_eq!(h.pid, std::process::id());
    me.subscribe().unwrap();

    let (mut child, _out) = spawn_helper(&sock, "arena");
    let held = std::fs::read_to_string(d.join("arena")).unwrap();
    assert!(held.starts_with(&format!("held by {}", child.id())), "{held:?}");
    let e = me.device("arena").unwrap_err().to_string();
    assert!(e.contains(&format!("held by process {} of the same user", child.id())), "{e}");

    drop(child.stdin.take());
    child.wait().unwrap();
    let ev = me.next_event().unwrap();
    assert_eq!(ev, format!("!exited {} helper", child.id()));
    assert_eq!(me.next_event().unwrap(), "!device-free arena");
    let fd = me.device("arena").expect("free again after the holder's exit");
    let mut f = std::fs::File::from(fd);
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(b"mine").unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_device_for_another_user_is_refused() {
    let d = dir("allow");
    let sock = start(&d, true);
    let mut me = Client::connect(&sock).unwrap();
    let e = me.device("port").unwrap_err().to_string();
    assert!(e.contains("is not for uid:"), "{e}");
    assert!(me.device("nothing").unwrap_err().to_string().contains("no device"));
    let _ = std::fs::remove_dir_all(&d);
}

fn map(fd: &OwnedFd, len: usize) -> *mut u8 {
    // SAFETY: a region fd of at least `len` bytes, mapped shared.
    let p = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0)
    };
    assert_ne!(p, libc::MAP_FAILED);
    p.cast()
}

/// A region is one memory for every holder; its owner's connection ending
/// leaves it held as an orphan, which the next connection adopts and frees.
#[test]
fn a_region_is_shared_kept_after_its_owner_and_adopted() {
    let d = dir("region");
    let sock = start(&d, false);
    let mut a = Client::connect(&sock).unwrap();
    let (r, fds) = a.alloc("gpu", 1 << 20, false).unwrap();
    assert_eq!((r.pool.as_str(), r.len, r.pieces.len()), ("gpu", 1 << 20, 1));
    let pa = map(&fds[0], 1 << 20);
    // SAFETY: inside the mapping.
    unsafe { *pa.add(4096) = 0x5a };

    let mut b = Client::connect(&sock).unwrap();
    let (r2, fds2) = b.open(r.id).unwrap();
    assert_eq!(r2.id, r.id);
    let pb = map(&fds2[0], 1 << 20);
    // SAFETY: inside the mapping.
    assert_eq!(unsafe { *pb.add(4096) }, 0x5a, "one memory, two mappings");
    assert!(b.free(r.id).unwrap_err().to_string().contains("not this connection's"));

    assert!(a.alloc("gpu", 64 << 20, false).unwrap_err().to_string().starts_with("budget"));
    drop(a);
    let mut c = Client::connect(&sock).unwrap();
    assert!(wait_for(|| c.stats().unwrap().contains("orphans=1")), "{}", c.stats().unwrap());
    // SAFETY: still mapped; the region is held.
    assert_eq!(unsafe { *pb.add(4096) }, 0x5a);
    assert_eq!(c.adopt("wayland").unwrap(), vec![]);
    assert_eq!(c.adopt("gpu").unwrap(), vec![(r.id, 1 << 20)]);
    c.free(r.id).unwrap();
    assert!(c.stats().unwrap().contains("regions=0"));
    let _ = std::fs::remove_dir_all(&d);
}

/// §111.6: the broker signs a boot record with the key only it can read and
/// the identity the kernel gave it; the caller names neither.
#[test]
fn a_record_is_signed_for_the_caller_and_only_with_a_key() {
    use wc_broker::proto::SignReq;
    use wc_handle::macboot::{Record, parse_key};
    let d = dir("sign");
    let sock = start(&d, false);
    let mut me = Client::connect(&sock).unwrap();
    let req = SignReq {
        install: "wcivs".into(),
        session: "wcivs-1".into(),
        index: 2,
        arena_run: 0xABCD,
        arena_pages: 32768,
        outbound: 1 << 20,
        rings_run: 0xABCD,
        rings_pages: 1536,
        gpu: false,
        seq: 4,
    };
    assert!(me.sign(&req).unwrap_err().to_string().contains("no key for installation wcivs"));
    let hex = "11".repeat(32);
    std::fs::write(d.join("keys/wcivs.key"), &hex).unwrap();
    let (auth, id) = me.sign(&req).unwrap();
    // SAFETY: no arguments.
    let uid = unsafe { libc::geteuid() };
    assert_eq!(id.id, uid.to_string());
    assert_eq!(id.os, if cfg!(target_os = "macos") { "macos" } else { "linux" });
    let rec = Record {
        session: req.session.clone(),
        arena_run: req.arena_run,
        arena_pages: req.arena_pages,
        outbound: req.outbound,
        rings_run: req.rings_run,
        rings_pages: req.rings_pages,
        gpu: req.gpu,
    };
    let key = parse_key(&hex).unwrap();
    assert!(auth.verifies_identity(&rec, 2, Some(&id), &key));
    let other = wc_handle::macboot::Identity { user: "someone-else".into(), ..id.clone() };
    assert!(!auth.verifies_identity(&rec, 2, Some(&other), &key));
    assert!(me.sign(&SignReq { install: "../x".into(), ..req }).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

/// `connect` gives a fresh connection to a local service each time.
#[test]
fn a_service_is_reached_through_the_broker() {
    let d = dir("connect");
    let sock = start(&d, false);
    let hub = std::os::unix::net::UnixListener::bind(d.join("hub.sock")).unwrap();
    let mut me = Client::connect(&sock).unwrap();
    for i in 0..2u8 {
        let fd = me.connect_service("hub").unwrap();
        let (mut s, _) = hub.accept().unwrap();
        let mut c = std::os::unix::net::UnixStream::from(fd);
        c.write_all(&[i]).unwrap();
        let mut b = [9u8];
        s.read_exact(&mut b).unwrap();
        assert_eq!(b[0], i);
    }
    assert!(me.connect_service("nope").unwrap_err().to_string().contains("no service"));
    let _ = std::fs::remove_dir_all(&d);
}

// ------------------------------------------------- the keeper (§111.9)

/// A broker set up as the macOS LaunchDaemon's keeper: regions wired by it,
/// a pool's orphans released by the boot page's word, or after `after`.
fn start_keeper(d: &Path, after: Duration) -> String {
    let sock = d.join("broker.sock");
    let unix = Unix::bind(&sock, vec![]).unwrap().mac_keeper();
    let config = Config {
        devices: vec![],
        services: vec![],
        keys: None,
        release: Release::Probe { after: Some(after) },
        budget: None,
        run: 9,
    };
    let b = Broker::new(unix, config);
    std::thread::spawn(move || b.serve());
    sock.to_string_lossy().into_owned()
}

/// The second process of the keeper test: `main` allocates the session's
/// arena and ring window in pool `s1` and writes the boot page into the ring
/// window, as the macOS main process does (`wc_handle::macboot`); `gpu`
/// ties itself to the pool, as the GPU server does. Both live until killed.
#[test]
fn keeper_helper() {
    let Ok(path) = std::env::var("WC_BROKER_TEST_KEEPER") else { return };
    let role = std::env::var("WC_BROKER_TEST_ROLE").unwrap();
    let mut c = Client::connect(&path).unwrap();
    c.hello(&role).unwrap();
    if role == "main" {
        let (arena, _fds) = c.alloc("s1", 4 * 4096, false).unwrap();
        let (rings, fds) = c.alloc("s1", 2 * 4096, false).unwrap();
        let p = map(&fds[0], 2 * 4096);
        // SAFETY: page 0 of the mapping just made.
        let page = unsafe { std::slice::from_raw_parts_mut(p, 4096) };
        page[..8].copy_from_slice(&wc_handle::arena::STAMP_MAGIC);
        page[wc_handle::arena::STAMP_OFF_RUN_ID..wc_handle::arena::STAMP_OFF_RUN_ID + 8].copy_from_slice(&7u64.to_le_bytes());
        wc_handle::macboot::Record {
            session: "s1".into(),
            arena_run: 7,
            arena_pages: 4,
            outbound: 0,
            rings_run: 7,
            rings_pages: 2,
            gpu: true,
        }
        .write(page);
        assert!(wc_handle::macboot::is_boot_page(page));
        page[4096 - 1] = 0x5a;
        println!("REGIONS {} {}", arena.id, rings.id);
    } else {
        c.tie("s1").unwrap();
        println!("TIED");
    }
    let _ = std::io::stdout().flush();
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

// The child is returned; every caller kills and waits for it.
#[allow(clippy::zombie_processes)]
fn spawn_keeper_helper(sock: &str, role: &str) -> (std::process::Child, String) {
    let mut c = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "keeper_helper", "--nocapture", "--test-threads=1"])
        .env("WC_BROKER_TEST_KEEPER", sock)
        .env("WC_BROKER_TEST_ROLE", role)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = c.stdout.take().unwrap();
    let mut got = Vec::new();
    let mut b = [0u8; 256];
    loop {
        let text = String::from_utf8_lossy(&got).into_owned();
        // libtest's "test keeper_helper ... " comes first on the same line.
        if let Some(at) = text.find("REGIONS").or_else(|| text.find("TIED"))
            && let Some(l) = text[at..].lines().next().filter(|_| text[at..].contains('\n'))
        {
            return (c, l.to_string());
        }
        let n = out.read(&mut b).unwrap();
        if n == 0 {
            let _ = c.wait();
            panic!("the helper ended early: {text}");
        }
        got.extend_from_slice(&b[..n]);
    }
}

fn wait_exit(c: &mut std::process::Child, within: Duration) -> Option<std::process::ExitStatus> {
    let t = Instant::now();
    while t.elapsed() < within {
        if let Some(s) = c.try_wait().unwrap() {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// §103.15's panic case, folded into the broker: the main process killed
/// with SIGKILL leaves its regions held (the pages keep what it wrote); the
/// host's release word in the boot page frees the whole pool and ends the
/// GPU server tied to it, and nothing before.
#[test]
fn a_killed_main_process_leaves_its_pool_held_until_the_release_word() {
    use std::os::unix::process::ExitStatusExt;
    let d = dir("keeper");
    let sock = start_keeper(&d, Duration::from_secs(600));
    let mut me = Client::connect(&sock).unwrap();
    me.hello("test").unwrap();
    me.subscribe().unwrap();
    let (mut main, line) = spawn_keeper_helper(&sock, "main");
    let ids: Vec<u64> = line.split(' ').skip(1).map(|w| w.parse().unwrap()).collect();
    let (mut gpu, _) = spawn_keeper_helper(&sock, "gpu");

    main.kill().unwrap();
    assert_eq!(main.wait().unwrap().signal(), Some(libc::SIGKILL));
    assert!(wait_for(|| me.stats().unwrap().contains("orphans=2")), "{}", me.stats().unwrap());
    // The host's view: the ring window, still held, still as written.
    let (_, fds) = me.open(ids[1]).unwrap();
    let p = map(&fds[0], 2 * 4096);
    // SAFETY: inside the mapping.
    assert_eq!(unsafe { *p.add(4096 - 1) }, 0x5a);
    std::thread::sleep(Duration::from_millis(2500));
    assert!(me.stats().unwrap().contains("orphans=2"), "nothing goes before the word");
    assert!(gpu.try_wait().unwrap().is_none(), "the GPU server lives until the release");

    // The host's session service writes the release word.
    // SAFETY: the release word of the boot page, 8-aligned.
    unsafe {
        (*(p.add(wc_handle::macboot::OFF_RELEASED) as *const std::sync::atomic::AtomicU64))
            .store(wc_handle::macboot::RELEASED, std::sync::atomic::Ordering::Release)
    };
    let st = wait_exit(&mut gpu, Duration::from_secs(5)).expect("the tied GPU server is ended after the release");
    assert_eq!(st.signal(), Some(libc::SIGKILL));
    assert!(wait_for(|| me.stats().unwrap().contains("orphans=0")), "{}", me.stats().unwrap());
    assert!(me.stats().unwrap().contains("regions=0"));
    let _ = std::fs::remove_dir_all(&d);
}

/// The keeper's limit: with no release word, the pool goes after its time
/// and its tied process with it; an arena page that merely holds a nonzero
/// word where the release word would be releases nothing.
#[test]
fn a_pool_with_no_release_goes_after_its_limit() {
    use std::os::unix::process::ExitStatusExt;
    let d = dir("keeper-limit");
    let sock = start_keeper(&d, Duration::from_secs(3));
    let mut me = Client::connect(&sock).unwrap();
    let (mut main, line) = spawn_keeper_helper(&sock, "main");
    let ids: Vec<u64> = line.split(' ').skip(1).map(|w| w.parse().unwrap()).collect();
    let (mut gpu, _) = spawn_keeper_helper(&sock, "gpu");
    // The arena (no boot page) carries the release value at the offset.
    let (_, fds) = me.open(ids[0]).unwrap();
    let p = map(&fds[0], 4 * 4096);
    // SAFETY: inside the mapping, 8-aligned.
    unsafe { *(p.add(wc_handle::macboot::OFF_RELEASED) as *mut u64) = wc_handle::macboot::RELEASED };
    main.kill().unwrap();
    main.wait().unwrap();
    assert!(wait_for(|| me.stats().unwrap().contains("orphans=2")));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(me.stats().unwrap().contains("orphans=2"), "an arena page is not a boot page");
    let st = wait_exit(&mut gpu, Duration::from_secs(6)).expect("ended at the limit");
    assert_eq!(st.signal(), Some(libc::SIGKILL));
    assert!(me.stats().unwrap().contains("regions=0"), "{}", me.stats().unwrap());
    let _ = std::fs::remove_dir_all(&d);
}

/// §112: a connection handed in rather than accepted (sv-drm forwards one end
/// of a socket pair a sandboxed process made) is served like any other, and
/// its identity is the pair's maker, as the kernel says.
#[test]
fn a_forwarded_connection_is_served_as_its_maker() {
    let d = dir("inject");
    std::fs::write(d.join("arena"), vec![0u8; 4096]).unwrap();
    let unix = Unix::bind(d.join("broker.sock"), vec![DevicePath { name: "arena".into(), path: d.join("arena") }]).unwrap();
    let inject = unix.injector();
    let devices = vec![DeviceSpec { name: "arena".into(), allow: Allow::AnyUser }];
    let config = Config { devices, services: vec![], keys: None, release: Release::OwnerFree, budget: None, run: 9 };
    let b = Broker::new(unix, config);
    std::thread::spawn(move || b.serve());
    let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    inject.send(theirs).unwrap();
    let mut s = mine;
    s.write_all(b"hello sandboxed\n").unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(&s), &mut line).unwrap();
    // SAFETY: no arguments.
    let uid = unsafe { libc::geteuid() };
    assert_eq!(line.trim(), format!("ok hello 0000000000000009 uid:{uid} {}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
}

/// `on_leased` sees a lease, with its holder's pid, before the fds arrive.
#[test]
fn a_lease_is_told_before_its_fds_are_sent() {
    let d = dir("leased");
    let unix = Unix::bind(d.join("broker.sock"), vec![]).unwrap();
    let config = Config { devices: vec![], services: vec![], keys: None, release: Release::OwnerFree, budget: None, run: 3 };
    let b = Broker::new(unix, config);
    let (tx, rx) = std::sync::mpsc::channel();
    b.on_leased(Box::new(move |id, pid| {
        let _ = tx.send((id, pid));
    }));
    let memfd = {
        // SAFETY: a static name, flags only.
        let fd = unsafe { libc::memfd_create(c"leased".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: a fresh fd we own; a live fd.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        // SAFETY: a live fd.
        assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 4096) }, 0);
        fd
    };
    let id = b.publish(User::uid(unsafe { libc::geteuid() }), "gpu.0", 1, vec![(Unix::object(memfd), 0, 4096)]);
    let b2 = b.clone();
    std::thread::spawn(move || b2.serve());
    let mut c = Client::connect(&d.join("broker.sock").to_string_lossy()).unwrap();
    let (r, _fds) = c.find("gpu.0").unwrap();
    assert_eq!(r.id, id);
    assert_eq!(rx.try_recv().unwrap(), (id, std::process::id()), "told before the reply was read");
    let _ = std::fs::remove_dir_all(&d);
}

/// A failed durable record refuses the lease without leaking handles or a
/// lease that would prevent the region draining.
#[test]
fn a_failed_lease_record_sends_no_handles_and_leaves_no_holder() {
    let d = dir("lease-record-failed");
    let unix = Unix::bind(d.join("broker.sock"), vec![]).unwrap();
    let b = Broker::new(
        unix,
        Config {
            devices: vec![],
            services: vec![],
            keys: None,
            release: Release::OwnerFree,
            budget: None,
            run: 3,
        },
    );
    b.on_leased_checked(Box::new(|_, _| Err(std::io::Error::other("disk full"))));
    let fd = unsafe { libc::memfd_create(c"lease-record".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 4096) }, 0);
    let id = b.publish(
        User::uid(unsafe { libc::geteuid() }),
        "gpu.0",
        1,
        vec![(Unix::object(fd), 0, 4096)],
    );
    let serving = b.clone();
    std::thread::spawn(move || serving.serve());
    let mut c = Client::connect(&d.join("broker.sock").to_string_lossy()).unwrap();
    let error = c.find("gpu.0").unwrap_err();
    assert!(error.to_string().contains("disk full"), "{error}");
    assert!(b.holders(id).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}
