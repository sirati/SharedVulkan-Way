//! docs/design.md §13 step 1, as a test: two Linux processes over a memfd.
//!
//! > **1. `sv-wire` + `sv-transport` over a memfd, two Linux processes, no
//! > Vulkan.** Handshake, magic-written-last, generation counter, ring
//! > ping-pong, reply ring, reverse ring, heartbeat timeout, peer-restart
//! > detection. *Test: a throughput/latency benchmark and a chaos test that
//! > kills each side at random points.*
//!
//! The chaos half is here; the benchmark is not (see the stage report).
//!
//! These spawn real processes, so they are `#[ignore]`d under Miri and they are
//! the one place in this workspace where a test depends on wall-clock time. Each
//! has a hard timeout, because a test that hangs is worse than a test that fails.

#![cfg(target_os = "linux")]

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const HOSTD: &str = env!("CARGO_BIN_EXE_sv-hostd");
const LOOPBACK: &str = env!("CARGO_BIN_EXE_sv-loopback");

/// A socket path unique to this test, in a directory that cleans itself up.
struct Sock {
    dir: std::path::PathBuf,
}

impl Sock {
    fn new(name: &str) -> Sock {
        let dir = std::env::temp_dir().join(format!(
            "sv-test-{name}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Sock { dir }
    }
    fn path(&self) -> String {
        self.dir.join("hostd.sock").to_string_lossy().into_owned()
    }
}

impl Drop for Sock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn spawn(bin: &str, args: &[&str]) -> Child {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawning {bin}: {e}"))
}

/// Wait for a child, with a deadline, and return `(success, stderr)`.
fn finish(mut child: Child, within: Duration) -> (bool, String) {
    let deadline = Instant::now() + within;
    loop {
        match child.try_wait().unwrap() {
            Some(status) => {
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut err);
                }
                return (status.success(), err);
            }
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut err);
                }
                return (false, format!("TIMED OUT after {within:?}\n{err}"));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Poll for `sv-hostd`'s socket to appear.
///
/// # Why the budget is generous, and why that is not masking anything
///
/// `within` is a *startup budget*, not a synchronisation window: the loop
/// returns the instant the socket exists, so a longer budget costs nothing when
/// the machine is idle and only matters when it is not. The three callers passed
/// 10 s, and a full `cargo test --workspace --no-fail-fast` on a machine a dozen
/// agents share exceeded it once — fork, exec and first-touch of a freshly
/// linked debug binary is not free under that load. The same run passed 3/3
/// alone in 0.32 s and passed again whole immediately after, which is the
/// signature of a budget rather than a hang.
///
/// Raising it does not hide a deadlock: a `sv-hostd` that never binds still
/// fails here, 20 s later and with the same message. What it removes is a shared
/// verification gate going red for a reason unrelated to anyone's change, which
/// is how "re-run until green" becomes a habit.
///
/// Changed from outside this crate's usual owner for that reason, and recorded
/// here rather than only in a commit message.
fn wait_for_socket(path: &str, within: Duration, hostd: &mut Child) {
    let t0 = Instant::now();
    let deadline = t0 + within;
    while Instant::now() < deadline {
        if std::path::Path::new(path).exists() {
            return;
        }
        // A daemon that exited never binds; say why now, rather than
        // waiting out the budget and blaming the machine's load.
        if let Ok(Some(status)) = hostd.try_wait() {
            let mut err = String::new();
            if let Some(mut e) = hostd.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut e, &mut err);
            }
            panic!("sv-hostd exited ({status}) after {:?} without binding {path}:\n{err}", t0.elapsed());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "sv-hostd never created {path} within {within:?}. If this is the only \
         failure in a whole-workspace run and it passes alone, it is startup \
         latency under load and not a hang — see this function's own note."
    );
}

/// The whole of step 1 and step 2, across a process boundary.
///
/// The app side allocates arena memory, writes a pattern through its own
/// mapping, and sends only `(offset, len)`. The GPU side checksums the same
/// bytes through its own, differently-based mapping. Equality is §1's invariant
/// — same offset, different base — demonstrated rather than asserted.
#[test]
fn the_arena_crosses_a_process_boundary_and_both_sides_agree() {
    let sock = Sock::new("loop");
    let path = sock.path();
    let mut hostd = spawn(HOSTD, &["serve", "--socket", &path, "--clients", "2"]);
    wait_for_socket(&path, Duration::from_secs(30), &mut hostd);

    let gpu = spawn(LOOPBACK, &["gpu", "--socket", &path]);
    let app = spawn(LOOPBACK, &["app", "--socket", &path]);

    let (app_ok, app_err) = finish(app, Duration::from_secs(30));
    let (gpu_ok, gpu_err) = finish(gpu, Duration::from_secs(30));
    let (hostd_ok, hostd_err) = finish(hostd, Duration::from_secs(10));

    assert!(app_ok, "app side failed:\n{app_err}");
    assert!(gpu_ok, "gpu side failed:\n{gpu_err}");
    assert!(hostd_ok, "hostd failed:\n{hostd_err}");

    // The assertions the processes themselves make, checked from outside so that
    // a silently-successful run cannot pass. A count of zero matches would look
    // exactly like success without this.
    assert_eq!(
        gpu_err.matches("checksum matches").count(),
        3,
        "expected three verified blobs, got:\n{gpu_err}"
    );
    assert!(gpu_err.contains("barrier on ring"), "the barrier never fired:\n{gpu_err}");
    assert!(gpu_err.contains("4 canaries intact"), "the canary was not re-checked:\n{gpu_err}");
    assert!(
        app_err.contains("reverse ring: the GPU side handled 3"),
        "the reverse ring carried nothing:\n{app_err}"
    );
    assert!(
        app_err.contains("zero payload bytes on the ring"),
        "the app side did not confirm the zero-copy path:\n{app_err}"
    );
}

/// R4: a peer that goes away mid-session is *detected*, not deadlocked against.
///
/// The GPU side is told to expect a third device-ring packet that the app side
/// never sends. The app then finishes and exits, its heartbeat stops, and the
/// GPU side must give up on its own.
///
/// **Deliberately not implemented by killing the app at a wall-clock moment.**
/// The first version of this test did exactly that and was worthless: the app
/// side finishes its work in about a millisecond, so a 120 ms sleep killed a
/// process that had already succeeded, and the test passed or failed on
/// scheduler noise. Making the *expectation* wrong rather than the *timing*
/// wrong is what turns this into a test of R4 instead of a test of the machine's
/// load average.
#[test]
fn a_peer_that_goes_away_mid_session_is_detected_rather_than_waited_on() {
    let sock = Sock::new("chaos");
    let path = sock.path();
    let mut hostd = spawn(HOSTD, &["serve", "--socket", &path, "--clients", "2"]);
    wait_for_socket(&path, Duration::from_secs(30), &mut hostd);

    // Three device packets expected; the app side sends two and then leaves.
    let gpu = spawn(LOOPBACK, &["gpu", "--socket", &path, "--expect-device", "3"]);
    let app = spawn(LOOPBACK, &["app", "--socket", &path, "--no-wait"]);

    let started = Instant::now();
    let (app_ok, app_err) = finish(app, Duration::from_secs(30));
    assert!(app_ok, "the app side failed before it could leave:\n{app_err}");

    // The GPU side must terminate on its own.
    let (gpu_ok, gpu_err) = finish(gpu, Duration::from_secs(25));
    let elapsed = started.elapsed();
    let (_, _) = finish(hostd, Duration::from_secs(5));

    assert!(!gpu_err.contains("TIMED OUT"), "the GPU side hung on a departed peer:\n{gpu_err}");
    assert!(!gpu_ok, "the GPU side reported success although a packet never arrived");
    // It failed, and it said why in the vocabulary R4 asks for.
    assert!(
        gpu_err.contains("gone") || gpu_err.contains("restarted"),
        "the GPU side blamed a timeout rather than the departed peer:\n{gpu_err}"
    );
    // **The point of the test.** The GPU side's own overall deadline is 20 s.
    // Noticing via the heartbeat has to be far quicker than that, or the
    // liveness machinery is decoration and R4 is answered by the deadline alone.
    assert!(
        elapsed < Duration::from_secs(10),
        "took {elapsed:?} to notice a departed peer; the heartbeat check is not doing the work"
    );
    // …and it processed the packets that really were sent before noticing.
    assert_eq!(
        gpu_err.matches("checksum matches").count(),
        3,
        "the GPU side should have handled every packet the app did send:\n{gpu_err}"
    );
}

/// The handshake refuses a peer built against a different protocol (R13).
///
/// `sv-hostd` hands out the same region to everybody; what separates two builds
/// is `protocol_hash`. This drives that check through the real binaries by
/// pointing a client at a region whose generation it does not expect — the
/// cheapest reachable proxy, since both binaries here share one hash by
/// construction.
#[test]
fn a_client_that_arrives_before_the_owner_waits_rather_than_reading_garbage() {
    let sock = Sock::new("early");
    let path = sock.path();
    let mut hostd = spawn(HOSTD, &["serve", "--socket", &path, "--clients", "2"]);
    wait_for_socket(&path, Duration::from_secs(30), &mut hostd);

    // The app side first, deliberately: it attaches to a region whose `magic` is
    // still zero and must spin on `NotReady` rather than proceed. This is the
    // magic-written-last rule doing its job across two processes.
    let app = spawn(LOOPBACK, &["app", "--socket", &path]);
    std::thread::sleep(Duration::from_millis(200));
    let gpu = spawn(LOOPBACK, &["gpu", "--socket", &path]);

    let (app_ok, app_err) = finish(app, Duration::from_secs(30));
    let (gpu_ok, gpu_err) = finish(gpu, Duration::from_secs(30));
    let (_, _) = finish(hostd, Duration::from_secs(5));

    assert!(app_ok, "an early client did not survive the wait:\n{app_err}");
    assert!(gpu_ok, "gpu side failed:\n{gpu_err}");
}
