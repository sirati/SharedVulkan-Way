//! The two types `Clock` needs from the platform: an identifier for *which*
//! clock, and a point on it.
//!
//! ADDED BY THE WAYCOMPOSE FORK, in the shape upstream already accepted once:
//! smithay PR #1027 ("portability: Fallback for `SealedFile`") added exactly
//! this kind of `target_os` fallback to `utils/clock.rs` and
//! `utils/sealed_file.rs` for FreeBSD. This is a third branch of the same
//! argument.
//!
//! On Unix these are `rustix::time::{ClockId, Timespec}` unchanged, so nothing
//! about the Unix build moves — the same syscall, the same struct, the same
//! `raw_os_error` if it ever fails.
//!
//! # What the Windows clock is, and where it differs
//!
//! `rustix` provides no `time` module for Windows (six of smithay's eight
//! requested rustix features provide nothing there; see docs/design.md §5.5),
//! so the Windows arm is built from `std`.
//!
//! **`Realtime` is exact.** `SystemTime::UNIX_EPOCH` is the same epoch as
//! `CLOCK_REALTIME`, so a timestamp taken here means the same thing a Linux one
//! does.
//!
//! **`Monotonic` has a different epoch, and this is a real behavioural
//! difference rather than a detail.** On Linux `CLOCK_MONOTONIC` counts from
//! boot and is readable by every process, so a client can call
//! `clock_gettime(CLOCK_MONOTONIC)` itself and compare its answer with the
//! compositor's — which is precisely what `wp_presentation` expects it to do.
//! Here the epoch is *first use within this process*, because `std::Instant` is
//! opaque and exposes no shared origin.
//!
//! Consequences, stated so that nobody discovers them from a wrong animation:
//!
//! * Ordering, differences and elapsed times are all correct. Every existing
//!   caller inside smithay uses `Time` this way — `as_millis()` for input event
//!   timestamps, `elapsed()` for frame callbacks — and all of those are
//!   unaffected.
//! * A client that compares a compositor timestamp against its own clock will
//!   be off by however long the compositor had been running. Today nothing does
//!   this, because `wp_presentation` needs pixels and there are none yet. When
//!   presentation feedback is implemented (stage 4+), this must be replaced with
//!   `QueryPerformanceCounter`/`QueryPerformanceFrequency`, which *is* a
//!   system-wide epoch a client can also read, and `wp_presentation.clock_id`
//!   must advertise it.
//! * Resolution is not the problem: `std::Instant` is already QPC-backed on
//!   Windows. Only the origin is.

#[cfg(unix)]
pub use rustix::time::{ClockId, Nsecs, Secs, Timespec};

#[cfg(unix)]
#[inline]
pub fn clock_gettime(id: ClockId) -> Timespec {
    rustix::time::clock_gettime(id)
}

#[cfg(windows)]
mod windows_impl {
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    /// Matches `rustix::time::Secs` so the arithmetic in `clock.rs` is
    /// identical on both platforms.
    pub type Secs = i64;
    /// Matches `rustix::time::Nsecs`.
    pub type Nsecs = i64;

    /// Which clock to read. Only the two smithay actually names exist; adding
    /// a variant here would be adding a clock we have no way to read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum ClockId {
        /// Counts forward and never jumps. Epoch is first use in this process
        /// — see the module docs, this differs from Linux.
        Monotonic,
        /// Wall clock, Unix epoch. Identical in meaning to `CLOCK_REALTIME`.
        Realtime,
    }

    /// A point in time, laid out like `struct timespec`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Timespec {
        /// Whole seconds.
        pub tv_sec: Secs,
        /// Nanoseconds within the second, always in `0..1_000_000_000`.
        pub tv_nsec: Nsecs,
    }

    /// The monotonic origin. Captured once, on the first read, rather than in a
    /// constructor: `Clock::new()` is called from several places at different
    /// times and they must all agree about zero.
    static MONOTONIC_EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

    #[inline]
    pub fn clock_gettime(id: ClockId) -> Timespec {
        let d = match id {
            ClockId::Monotonic => MONOTONIC_EPOCH.get_or_init(Instant::now).elapsed(),
            // A clock set backwards before the Unix epoch is the only way this
            // can fail, and returning zero for it is better than panicking in a
            // compositor's hot path.
            ClockId::Realtime => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default(),
        };
        Timespec {
            tv_sec: d.as_secs() as Secs,
            tv_nsec: d.subsec_nanos() as Nsecs,
        }
    }
}

#[cfg(windows)]
pub use windows_impl::{clock_gettime, ClockId, Nsecs, Secs, Timespec};
