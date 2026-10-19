//! `WC_FRAME_TRACE`: one timeline of a frame, written by every process it
//! crosses (docs/waycompose-design.md §88).
//!
//! A frame of an accelerated Firefox on the guest's desktop is produced by
//! Firefox on the host, forwarded command by command to the guest's GPU,
//! copied back into an arena, handed to `wc-proxy` as a `wl_shm` buffer, copied
//! into the compositor's arena, read by the compositor in the guest and painted
//! by GDI. Five processes on two machines; each knew how long its own part
//! took, and none of them could say where the other 22 ms of a 25 ms frame
//! went. This is the instrument that can: each process appends one line per
//! event to its own file, with a timestamp from a clock all processes on one
//! machine share, and `scripts/wc-frame-timeline.py` lines them up.
//!
//! # Cost
//!
//! Off, one relaxed load of a `OnceLock` per call site. On, one `format!` into
//! a buffered file under a mutex, flushed at most every 250 ms; a frame emits a
//! handful of lines. That is cheap enough to leave on for a measured run and is
//! not meant to be on otherwise.
//!
//! # The clock
//!
//! Microseconds of `SystemTime`, taken as `SystemTime` once and advanced by
//! `Instant` after that, so it is monotonic within a process and comparable
//! between processes on one machine (they all read the same wall clock at
//! start, to well under a microsecond of each other's error). Across the VM
//! boundary the two wall clocks are not the same clock; the analysis estimates
//! their offset from events both sides see rather than trusting them.
//!
//! **`WC_FRAME_TRACE_CLOCK=mono`** (docs/waycompose-design.md §89) replaces the
//! wall clock with the machine's monotonic counter: `QueryPerformanceCounter`
//! in microseconds on Windows, `CLOCK_MONOTONIC` on Linux. Every process on a
//! machine reads the *same* counter, so two processes started minutes apart
//! agree exactly even while the wall clock is being slewed — which a guest's
//! time service does, and which across a one-minute input run is milliseconds.
//! It is also the clock `probe-tunnel-latency.exe` and `wc-clock-sync.py`
//! stamp with, so the offset between the two machines can be measured rather
//! than estimated from the traffic being timed.
//!
//! # The file
//!
//! `WC_FRAME_TRACE=<path>`, with `%p` replaced by the process id, opened for
//! append. A process that is killed loses at most its last 250 ms.

use std::fmt;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Sink {
    out: std::io::BufWriter<std::fs::File>,
    last_flush: Instant,
}

struct Trace {
    epoch_instant: Instant,
    epoch_us: u64,
    sink: Mutex<Sink>,
}

fn trace() -> Option<&'static Trace> {
    static T: OnceLock<Option<Trace>> = OnceLock::new();
    T.get_or_init(|| {
        let path = std::env::var("WC_FRAME_TRACE").ok().filter(|p| !p.is_empty())?;
        let path = path.replace("%p", &std::process::id().to_string());
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
        let epoch_instant = Instant::now();
        let mono = std::env::var("WC_FRAME_TRACE_CLOCK").is_ok_and(|v| v == "mono");
        let epoch_us = match mono.then(mono_us).flatten() {
            Some(us) => us,
            None => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(0),
        };
        Some(Trace {
            epoch_instant,
            epoch_us,
            sink: Mutex::new(Sink {
                out: std::io::BufWriter::with_capacity(1 << 16, file),
                last_flush: epoch_instant,
            }),
        })
    })
    .as_ref()
}

/// The machine's monotonic counter in microseconds, the clock
/// `WC_FRAME_TRACE_CLOCK=mono` asks for. Read once, at the first event; the
/// process's `Instant` advances it after that, and `Instant` is this same
/// counter on both platforms, so the two never drift apart.
#[cfg(windows)]
pub fn mono_us() -> Option<u64> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
    }
    let (mut c, mut f) = (0i64, 0i64);
    // SAFETY: two out-parameters on this stack frame.
    let ok = unsafe { QueryPerformanceCounter(&mut c) != 0 && QueryPerformanceFrequency(&mut f) != 0 };
    (ok && f > 0).then(|| (c as u128 * 1_000_000 / f as u128) as u64)
}

/// macOS (§103.15, "the clock trap"): `mach_absolute_time` scaled by
/// `mach_timebase_info`, which is the counter `Instant` reads there. Never
/// the raw `cntvct_el0` or `cntfrq_el0` from user space: on Apple Silicon
/// `cntfrq_el0` reads 1 GHz and the raw counter is not the 24 MHz one, while
/// `mach_absolute_time` ticks at 24 MHz with timebase 125/3 (measured on the
/// real Mac), and a Linux guest's counter tracks it within a few hundred
/// ticks. Linux's `CLOCK_MONOTONIC` id (1) is not a valid clock on macOS.
#[cfg(target_os = "macos")]
pub fn mono_us() -> Option<u64> {
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    let mut tb = Timebase { numer: 0, denom: 0 };
    // SAFETY: an out-parameter on this stack frame; the call has no other effect.
    if unsafe { mach_timebase_info(&mut tb) } != 0 || tb.denom == 0 {
        return None;
    }
    // SAFETY: no arguments, no effect.
    let t = unsafe { mach_absolute_time() };
    Some((t as u128 * tb.numer as u128 / tb.denom as u128 / 1000) as u64)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn mono_us() -> Option<u64> {
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    unsafe extern "C" {
        fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
    }
    // CLOCK_MONOTONIC is 1 on Linux, the only Unix this crate's tracing runs on.
    const CLOCK_MONOTONIC: i32 = 1;
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: an out-parameter on this stack frame, of the layout `clock_gettime`
    // writes on a 64-bit Linux.
    let rc = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
    (rc == 0).then(|| ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000)
}

#[cfg(not(any(unix, windows)))]
pub fn mono_us() -> Option<u64> {
    None
}

/// Whether `WC_FRAME_TRACE` names a file this process could open.
pub fn enabled() -> bool {
    trace().is_some()
}

/// Microseconds on the shared clock; see the module header. Zero when tracing
/// is off, so a caller that stores one unconditionally pays nothing for it.
pub fn now_us() -> u64 {
    trace().map_or(0, |t| t.epoch_us + t.epoch_instant.elapsed().as_micros() as u64)
}

/// The shared clock's reading for an `Instant` taken earlier.
pub fn at_us(i: Instant) -> u64 {
    trace().map_or(0, |t| {
        t.epoch_us + i.saturating_duration_since(t.epoch_instant).as_micros() as u64
    })
}

/// One line: `<us> <event> <fields>`. Nothing when tracing is off.
pub fn emit(event: &str, fields: fmt::Arguments<'_>) {
    emit_at(now_us(), event, fields);
}

/// [`emit`] with the timestamp supplied, for an event whose moment was taken
/// before the line could be written.
pub fn emit_at(us: u64, event: &str, fields: fmt::Arguments<'_>) {
    let Some(t) = trace() else { return };
    let mut s = t.sink.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(s.out, "{us} {event} {fields}");
    if s.last_flush.elapsed() >= Duration::from_millis(250) {
        let _ = s.out.flush();
        s.last_flush = Instant::now();
    }
}

/// Write out what is buffered. For the end of a run.
pub fn flush() {
    if let Some(t) = trace() {
        let mut s = t.sink.lock().unwrap_or_else(|e| e.into_inner());
        let _ = s.out.flush();
        s.last_flush = Instant::now();
    }
}

/// The machine's monotonic counter in nanoseconds: `QueryPerformanceCounter`
/// on Windows, `CLOCK_MONOTONIC` on Linux (the clock bpftrace's `nsecs` reads,
/// so a hop trace lines up with a kernel trace of the same run).
#[cfg(windows)]
pub fn mono_ns() -> u64 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
    }
    static FREQ: OnceLock<u64> = OnceLock::new();
    let f = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: an out-parameter on this stack frame.
        unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1) as u64
    });
    let mut c = 0i64;
    // SAFETY: as above.
    unsafe { QueryPerformanceCounter(&mut c) };
    (c as u128 * 1_000_000_000 / f as u128) as u64
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn mono_ns() -> u64 {
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    unsafe extern "C" {
        fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
    }
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: as in `mono_us`.
    unsafe { clock_gettime(1, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
pub fn mono_ns() -> u64 {
    mono_us().unwrap_or(0) * 1000
}

/// `WC_HOP_TRACE` (waycompose-design.md §105.14): one stamp per hop of a
/// control request, so the round trip can be cut into its pieces across both
/// machines and lined up with a kernel trace of the host.
///
/// Off, a relaxed load. On, the stamp is a counter read and a push under an
/// uncontended mutex; a thread of its own writes the lines (`<ns> <hop>`) to
/// the file every half second, so no hop ever pays for the write.
struct Hops {
    buf: Mutex<Vec<(u64, &'static str)>>,
    file: Mutex<std::fs::File>,
}

impl Hops {
    /// Write out what is buffered. The buffer is swapped for an empty one of
    /// the same capacity under the lock, so a hop never waits for the write.
    fn flush(&self) {
        let mut taken = Vec::with_capacity(1 << 16);
        std::mem::swap(&mut taken, &mut *self.buf.lock().unwrap_or_else(|e| e.into_inner()));
        if taken.is_empty() {
            return;
        }
        let mut out = String::with_capacity(taken.len() * 24);
        for (ns, name) in taken {
            use std::fmt::Write as _;
            let _ = writeln!(out, "{ns} {name}");
        }
        let _ = self.file.lock().unwrap_or_else(|e| e.into_inner()).write_all(out.as_bytes());
    }
}

fn hops() -> Option<&'static Hops> {
    static H: OnceLock<Option<&'static Hops>> = OnceLock::new();
    *H.get_or_init(|| {
        let path = std::env::var("WC_HOP_TRACE").ok().filter(|p| !p.is_empty())?;
        let path = path.replace("%p", &std::process::id().to_string());
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
        let h: &'static Hops =
            Box::leak(Box::new(Hops { buf: Mutex::new(Vec::with_capacity(1 << 16)), file: Mutex::new(file) }));
        let _ = std::thread::Builder::new().name("hop-trace".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(500));
                h.flush();
            }
        });
        Some(h)
    })
}

/// Write out the hops stamped so far: for a process about to exit.
pub fn hop_flush() {
    if let Some(h) = hops() {
        h.flush();
    }
}

/// Stamp hop `name` now. Nothing when `WC_HOP_TRACE` is unset.
#[inline]
pub fn hop(name: &'static str) {
    if let Some(h) = hops() {
        let ns = mono_ns();
        h.buf.lock().unwrap_or_else(|e| e.into_inner()).push((ns, name));
    }
}

/// Microseconds in a `Duration`, saturating, for the fields of a line.
pub fn us(d: Duration) -> u64 {
    d.as_micros().min(u64::MAX as u128) as u64
}

/// Per-opcode counters for a window of time: how many, and how long in total.
///
/// Used by the ICD for the round trips between two presents and by the GPU
/// side for what its driver spent each second. Small and linear on purpose: a
/// frame touches a few dozen distinct opcodes.
#[derive(Default)]
pub struct OpTally {
    ops: Vec<(u32, u64, u64)>,
}

impl OpTally {
    pub const fn new() -> OpTally {
        OpTally { ops: Vec::new() }
    }

    pub fn add(&mut self, op: u32, d: Duration) {
        let ns = d.as_nanos().min(u64::MAX as u128) as u64;
        match self.ops.iter_mut().find(|e| e.0 == op) {
            Some(e) => {
                e.1 += 1;
                e.2 += ns;
            }
            None => self.ops.push((op, 1, ns)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Total count and nanoseconds.
    pub fn total(&self) -> (u64, u64) {
        self.ops.iter().fold((0, 0), |(n, t), e| (n + e.1, t + e.2))
    }

    /// The `k` most expensive opcodes, as `name:count:us` joined by commas.
    pub fn top(&self, k: usize, name: impl Fn(u32) -> String) -> String {
        let mut v: Vec<&(u32, u64, u64)> = self.ops.iter().collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.2));
        v.iter()
            .take(k)
            .map(|e| format!("{}:{}:{}", name(e.0), e.1, e.2 / 1000))
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn clear(&mut self) {
        self.ops.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §89. The `mono` clock is the counter `Instant` reads, so the two must
    /// advance together; a wall clock would not, while it is being slewed.
    #[test]
    #[cfg_attr(miri, ignore = "FFI: clock_gettime / QueryPerformanceCounter")]
    fn the_monotonic_clock_advances_with_instant() {
        let (a, i) = (mono_us().expect("a monotonic counter"), Instant::now());
        std::thread::sleep(Duration::from_millis(20));
        let (b, j) = (mono_us().expect("a monotonic counter"), Instant::now());
        assert!(b > a, "the counter went from {a} to {b}");
        let skew = (b - a) as i64 - (j - i).as_micros() as i64;
        assert!(skew.abs() < 5_000, "the counter and Instant disagree by {skew} us over 20 ms");
    }

    #[test]
    fn a_tally_sums_per_opcode_and_sorts_by_time() {
        let mut t = OpTally::default();
        t.add(7, Duration::from_micros(10));
        t.add(3, Duration::from_micros(500));
        t.add(7, Duration::from_micros(30));
        assert_eq!(t.total(), (3, 540_000));
        assert_eq!(t.top(2, |o| format!("op{o}")), "op3:1:500,op7:2:40");
        t.clear();
        assert!(t.is_empty());
    }
}
