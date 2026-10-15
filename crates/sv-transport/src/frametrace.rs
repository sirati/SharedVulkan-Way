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
        let epoch_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
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
