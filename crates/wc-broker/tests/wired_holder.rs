//! Linux process-lifecycle proof of the same Unix mmap/mlock holder used on
//! macOS. This kills the broker itself, not a client main process. Darwin
//! physical-page stability still requires its own disposable-VM proof.
#![cfg(target_os = "linux")]
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::FileExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use wc_broker::client::Client;
use wc_broker::server::{Broker, Config};
use wc_broker::sys::unix::Unix;
use wc_broker::Release;

#[test]
fn broker_fixture() {
    let Ok(dir) = std::env::var("WC_HOLDER_FIXTURE") else { return };
    let unix = Unix::bind(format!("{dir}/sock"), vec![]).unwrap().mac_keeper()
        .with_persistent_holder(std::path::Path::new(env!("CARGO_BIN_EXE_wc-broker"))).unwrap();
    let broker = Broker::new(unix, Config { devices: vec![], services: vec![], keys: None,
        release: Release::Probe { after: None }, budget: None, run: 1 });
    println!("READY");
    broker.serve().unwrap();
}

fn wait(mut pred: impl FnMut() -> bool) {
    let start = Instant::now();
    while !pred() {
        assert!(start.elapsed() < Duration::from_secs(8), "lifecycle condition timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn locked(pid: u32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default().lines()
        .find_map(|l| l.strip_prefix("VmLck:")).and_then(|v| v.split_whitespace().next()?.parse().ok()).unwrap_or(0)
}
fn exited(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s.split(')').nth(1).is_some_and(|s| s.trim_start().starts_with('Z')))
}
struct Cleanup {
    broker: std::process::Child,
    regions: Vec<std::os::fd::OwnedFd>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        // Test-only host release, including unwind paths. No real VM or host
        // mapping uses these anonymous Linux fixture pages.
        for fd in &self.regions { release(fd); }
        let _ = self.broker.kill();
        let _ = self.broker.wait();
    }
}
fn release(fd: &std::os::fd::OwnedFd) {
    let f = fs::File::from(fd.try_clone().unwrap());
    f.write_all_at(&wc_handle::arena::STAMP_MAGIC, 0).unwrap();
    f.write_all_at(&wc_handle::macboot::MAGIC, wc_handle::macboot::OFF as u64).unwrap();
    f.write_all_at(&wc_handle::macboot::RELEASED.to_le_bytes(), wc_handle::macboot::OFF_RELEASED as u64).unwrap();
}

#[test]
fn broker_sigkill_preserves_independent_wires_until_each_pools_host_release() {
    let dir = std::env::temp_dir().join(format!("wch-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let mut broker = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "broker_fixture", "--nocapture"])
        .env("WC_HOLDER_FIXTURE", &dir).stdout(Stdio::piped()).spawn().unwrap();
    let mut output = BufReader::new(broker.stdout.take().unwrap());
    let mut line = String::new();
    loop { line.clear(); assert!(output.read_line(&mut line).unwrap() > 0); if line.trim() == "READY" { break; } }
    let mut cleanup = Cleanup { broker, regions: Vec::new() };
    // The helper test runs on a harness thread. Linux records its children
    // on that task, not necessarily on the process's main thread.
    let children = fs::read_dir(format!("/proc/{}/task", cleanup.broker.id())).unwrap()
        .flatten().filter_map(|e| fs::read_to_string(e.path().join("children")).ok()).collect::<Vec<_>>().join(" ");
    let holder: u32 = children.split_whitespace().next().unwrap().parse().unwrap();
    let mut client = Client::connect(&dir.join("sock").to_string_lossy()).unwrap();
    client.hello("main").unwrap();
    let (temporary, _) = client.alloc("temporary", 4096, false).unwrap();
    let (_, a) = client.alloc("a:session", 4096, false).unwrap();
    cleanup.regions.push(a[0].try_clone().unwrap());
    let (_, _arena) = client.alloc("a:session", 4096, false).unwrap();
    let (_, b) = client.alloc("b.session", 4096, false).unwrap();
    cleanup.regions.push(b[0].try_clone().unwrap());
    wait(|| locked(holder) == 16);
    client.free(temporary.id).unwrap();
    wait(|| locked(holder) == 12);
    let sentinel = fs::File::from(b[0].try_clone().unwrap());
    sentinel.write_all_at(b"still owned", 2048).unwrap();
    cleanup.broker.kill().unwrap();
    assert!(!cleanup.broker.wait().unwrap().success());
    // Exceed the former 10-second shutdown window. The independent process
    // still owns three mappings, not merely a surviving descriptor.
    std::thread::sleep(Duration::from_secs(11));
    assert!(!exited(holder));
    assert_eq!(locked(holder), 12);
    // The retention process is outside the dead broker's session and ignores
    // a normal shutdown signal until host release proves reclamation safe.
    assert_eq!(unsafe { libc::getsid(holder as i32) }, holder as i32);
    assert_eq!(unsafe { libc::kill(holder as i32, libc::SIGTERM) }, 0);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(locked(holder), 12);
    let mut bytes = [0u8; 11]; sentinel.read_exact_at(&mut bytes, 2048).unwrap(); assert_eq!(&bytes, b"still owned");
    release(&a[0]);
    wait(|| locked(holder) == 4);
    assert!(!exited(holder), "another pool must keep its independent wire");
    release(&b[0]);
    wait(|| exited(holder));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_exited_holder_prevents_a_region_grant() {
    let dir = std::env::temp_dir().join(format!("wch-fail-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    // A Rust test runner invoked with filter __hold-wired exits without
    // accepting the private protocol. The broker must return no region/fd.
    let unix = Unix::bind(dir.join("sock"), vec![]).unwrap().mac_keeper()
        .with_persistent_holder(&std::env::current_exe().unwrap()).unwrap();
    let broker = Broker::new(unix, Config { devices: vec![], services: vec![], keys: None,
        release: Release::Probe { after: None }, budget: None, run: 1 });
    std::thread::spawn(move || broker.serve());
    let mut client = Client::connect(&dir.join("sock").to_string_lossy()).unwrap();
    client.hello("main").unwrap();
    assert!(client.alloc("refused", 4096, false).is_err());
    assert!(client.stats().unwrap().contains("regions=0"));
    fs::remove_dir_all(dir).unwrap();
}
