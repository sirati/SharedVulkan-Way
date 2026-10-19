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
    .unwrap();
    let devices = vec![
        DeviceSpec { name: "arena".into(), allow: Allow::AnyUser },
        DeviceSpec {
            name: "port".into(),
            allow: if other_only { Allow::Users(vec![User::uid(4_000_000)]) } else { Allow::AnyUser },
        },
    ];
    let b = Broker::new(unix, Config { devices, release: Release::OwnerFree, budget: Some(64 << 20), run: 7 });
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
