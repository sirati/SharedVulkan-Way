//! How long does a doorbell take to cross a real hypervisor?
//!
//! waycompose-design.md §36.6. `vdoorbell`'s measurement — 9.26 ms for an
//! eleven-wait present against 137.8 ms for the 20 ms slice — was taken with a
//! `socketpair` standing in for the virtio-serial port. That is faithful in
//! every respect the *mechanism* cares about (a byte pipe where a write ends a
//! blocked read) and says nothing about the one quantity that matters here:
//! **a socketpair write is a function call into the same kernel, and a
//! virtio-serial write is a vmexit.** One is tens of nanoseconds and the other
//! is not, and the whole argument for the doorbell rests on the second being
//! small compared to a 20 ms slice rather than on it being free.
//!
//! So this measures it, across the real boundary, on the development guest —
//! `nix build .#svGuest`, whose RAM is a `memory-backend-memfd` and whose
//! virtio-serial ports are the same mechanism `testhost` carries. Two halves:
//!
//! ```text
//!   host  : sv-bell-probe host  --port <run>/bell.sock  [--count N]
//!   guest : sv-bell-probe guest --port /dev/virtio-ports/org.sirati.svbell.0
//! ```
//!
//! The guest half echoes every byte. The host half sends one and waits for it
//! back, timing the round trip. **A doorbell is one way**, so the figure to
//! carry forward is half the round trip — and the round trip is what can be
//! measured without two synchronised clocks, which across a VM boundary is
//! exactly the thing there is no cheap way to have.
//!
//! # What the number is compared against
//!
//! Three things, all printed together, because a latency with nothing beside it
//! is not evidence:
//!
//! 1. **The 20 ms slice** — what every cross-VM wait cost before §36.6.
//! 2. **The ladder's guest floor, 1 ms** — `WaitOnAddress` takes whole
//!    milliseconds, so that is the best a shortened slice can ever do there.
//! 3. **A same-kernel `socketpair` round trip**, measured in this same process,
//!    which is the reproduction's cost and the lower bound the vmexit is added
//!    to.
//!
//! # Why a separate port and not the cold path
//!
//! The cold path carries framed `ColdMessage`s; the doorbell carries bare bytes
//! with no framing at all, and §36.6.3 argues that is not an oversight — a lost
//! byte costs one slice and a spurious byte costs one syscall, so there is
//! nothing to resynchronise. Sharing one port would mean giving the doorbell a
//! frame purely so the two could coexist, which is the property it must not
//! have.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn usage() -> ! {
    eprintln!(
        "usage:\n  \
         sv-bell-probe guest --port <path> [--seconds N]\n  \
         sv-bell-probe host  --port <path> [--count N]"
    );
    std::process::exit(2);
}

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let port = arg("--port").unwrap_or_else(|| usage());
    match mode.as_str() {
        "guest" => guest(&port),
        "host" => host(&port),
        _ => usage(),
    }
}

/// Echo every byte, forever. The other half of a doorbell.
///
/// Reads and writes on **one** descriptor: `crate::stream::open_virtio_port`'s
/// doc warns that two descriptors on one port deadlock, because the driver
/// serialises them and a reader blocked in one stops the other making progress.
fn guest(port: &str) {
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(120);
    let mut f = match sv_transport::stream::open_virtio_port(
        port.rsplit('/').next().unwrap_or(port),
    ) {
        Ok(f) => f,
        Err(e) => {
            println!("[bell-probe] cannot open {port}: {e}");
            std::process::exit(1);
        }
    };
    println!("[bell-probe] guest echoing on {port} for {seconds}s");
    let _ = std::io::stdout().flush();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut buf = [0u8; 64];
    let mut echoed = 0u64;
    while Instant::now() < deadline {
        match f.read(&mut buf) {
            // **Not an end.** A virtio-serial port reads EOF whenever the host
            // end of the chardev has nobody connected, and the guest half
            // necessarily opens its side before the host process starts — the
            // harness has to see "echoing" in the log to know it is safe to
            // measure. Treating this as the end made the guest exit during the
            // ten seconds before the host connected, and present as
            // "echoed 0 bytes" with no error anywhere.
            //
            // The sleep matters: without it this is a busy loop on a fd that
            // will return 0 as fast as it is asked, which is the core-spinning
            // failure this project keeps meeting.
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            Ok(n) => {
                if f.write_all(&buf[..n]).and_then(|()| f.flush()).is_err() {
                    break;
                }
                echoed += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                println!("[bell-probe] guest read failed: {e}");
                break;
            }
        }
    }
    println!("[bell-probe] guest echoed {echoed} bytes");
}

fn stats(mut v: Vec<f64>) -> (f64, f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    (v[0], at(0.5), at(0.99), v[v.len() - 1])
}

fn host(port: &str) {
    let count: usize = arg("--count").and_then(|s| s.parse().ok()).unwrap_or(2000);

    // The same-kernel baseline, in this process, so the two numbers are taken
    // by the same code on the same machine at the same moment.
    let (a, b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let echo = std::thread::spawn(move || {
        let mut b = b;
        let mut buf = [0u8; 1];
        while b.read(&mut buf).map(|n| n > 0).unwrap_or(false) {
            if b.write_all(&buf).is_err() {
                break;
            }
        }
    });
    let mut local = Vec::with_capacity(count);
    {
        let mut a = a;
        let mut buf = [0u8; 1];
        for _ in 0..count {
            let t0 = Instant::now();
            a.write_all(&[7]).unwrap();
            a.read_exact(&mut buf).unwrap();
            local.push(t0.elapsed().as_secs_f64() * 1e6);
        }
    }
    let _ = echo.join();

    // And the real thing.
    let mut s = match std::os::unix::net::UnixStream::connect(port) {
        Ok(s) => s,
        Err(e) => {
            println!("[bell-probe] cannot connect to {port}: {e}");
            std::process::exit(1);
        }
    };
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    // A few warm-up trips, discarded: the first byte through a virtio-serial
    // port pays for the guest half's first scheduling and for the port's own
    // open, and neither is what a doorbell in a running session costs.
    let mut buf = [0u8; 1];
    for _ in 0..50 {
        if s.write_all(&[1]).and_then(|()| s.flush()).is_err() {
            break;
        }
        if s.read_exact(&mut buf).is_err() {
            println!("[bell-probe] the guest half did not answer; is it running?");
            std::process::exit(1);
        }
    }

    let mut cross = Vec::with_capacity(count);
    for _ in 0..count {
        let t0 = Instant::now();
        if s.write_all(&[1]).and_then(|()| s.flush()).is_err() {
            break;
        }
        if s.read_exact(&mut buf).is_err() {
            break;
        }
        cross.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    if cross.len() < count / 2 {
        println!("[bell-probe] only {} of {count} round trips completed", cross.len());
        std::process::exit(1);
    }

    let samples = cross.len();
    let (lmin, lmed, lp99, lmax) = stats(local);
    let (cmin, cmed, cp99, cmax) = stats(cross);

    println!();
    println!("doorbell round trip, {samples} samples");
    println!("                       min      median      p99        max");
    println!(
        "  same kernel      {lmin:8.1} {lmed:9.1} {lp99:9.1} {lmax:9.1}  µs   (the socketpair \
         reproduction)"
    );
    println!(
        "  across the VM    {cmin:8.1} {cmed:9.1} {cp99:9.1} {cmax:9.1}  µs   (virtio-serial)"
    );
    println!();
    // A doorbell is one way. The round trip is what can be measured without two
    // synchronised clocks, which across a VM boundary there is no cheap way to
    // have, so the one-way figure is stated as half and labelled as such.
    let one_way = cmed / 2.0;
    println!("  one-way doorbell (median round trip / 2): {one_way:.1} µs");
    println!();
    println!("  against the alternatives, per wait:");
    println!("    fixed 20 ms slice      20000.0 µs   {:>8.0}x this", 20000.0 / one_way);
    println!("    ladder floor on guest   1000.0 µs   {:>8.0}x this", 1000.0 / one_way);
    println!("    this doorbell         {one_way:9.1} µs");
    println!();
    println!(
        "  eleven waits (one present): {:.2} ms with the doorbell, 220.00 ms with the slice",
        one_way * 11.0 / 1000.0
    );
}
