//! How long the poll loop waits between turns, and why it is not `Sleep`.
//!
//! # The measurement this exists because of
//!
//! docs/waycompose-design.md §13.8. On the guest, with 40 samples each:
//!
//! ```text
//!   Sleep(0)    0.000 ms      Sleep(1)   15.813 ms
//!   Sleep(5)   15.882 ms      Sleep(16)  31.299 ms
//! ```
//!
//! `Sleep(1)` and `Sleep(5)` are the same number, so this is a *quantum* rather
//! than an overhead: everything below one 15.6 ms scheduler tick is rounded up
//! to one tick, and `Sleep(16)` — one frame at 60 Hz — costs two.
//!
//! # What that costs, and what it does *not* cost
//!
//! It is a latency floor. Every UI event crosses this loop — a close, a resize
//! configure, a keystroke — so all of them were up to 15.9 ms late and are now
//! up to 5.1 ms late. That is the whole of the justification and it is enough.
//!
//! It is **not** a throughput fix, and §13.8 records this section claiming that
//! it was. The story was that `State::tick_frames`' 16 ms gate, sampled by a
//! 15.9 ms turn, is missed on most turns and halves an animating client's frame
//! rate. The arithmetic worked and the buckets fitted. Then the timer landed and
//! the frame rate did not move — 34.6 a second against 38.8 — because the thing
//! actually being measured was a PPM per frame written over virtiofs by the
//! harness. `--poll-coarse` exists so that the next version of that question is
//! asked with one binary and one flag.
//!
//! # Why not `timeBeginPeriod`
//!
//! Because it is **global**. `timeBeginPeriod(1)` raises the timer resolution
//! for the whole machine for as long as any process holds it, which is a real
//! power cost on a laptop and is somebody else's battery. The user ruled it out
//! and the ruling costs nothing, because Windows 10 1803 added the scoped
//! version of the same thing.
//!
//! # What this is instead
//!
//! `CreateWaitableTimerExW` with `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`. The
//! precision applies to *this wait*, not to the system, and it measured 5.141 ms
//! for a 5 ms wait and 1.595 ms for a 1 ms one — the number asked for, within
//! the noise, with no global effect.
//!
//! It is created once per thread and reused, because creating a timer object
//! per turn would be a syscall pair 200 times a second to save nothing.
//!
//! # The fallbacks, and why there are two
//!
//! The flag is Windows 10 1803. On anything older `CreateWaitableTimerExW`
//! fails with `ERROR_INVALID_PARAMETER` and this falls back to `Sleep`, which is
//! the behaviour every stage up to 8 had — a degradation to the old cadence
//! rather than a failure to run. On Unix `Ticker` is `thread::sleep`, because
//! Linux's timer resolution is not the problem this solves and `nanosleep`
//! already honours a 5 ms request.

use std::time::Duration;

/// A reusable short wait.
///
/// Construct once per thread, then call [`Ticker::wait`] every turn.
pub struct Ticker {
    imp: imp::Inner,
}

impl Ticker {
    pub fn new() -> Self {
        Self { imp: imp::Inner::new() }
    }

    /// The old behaviour: plain `Sleep`, with its 15.6 ms quantum.
    ///
    /// This exists so that the two cadences can be compared **in one binary,
    /// with one flag**, which is the only form of that comparison worth
    /// anything. §13.8's first attempt at the frame-rate claim compared a
    /// number taken today against a number taken on a different tree, on a
    /// different transport, with a different instrument in the path — and was
    /// wrong. `--poll-coarse` is the control that would have caught it.
    pub fn coarse() -> Self {
        Self { imp: imp::Inner::coarse() }
    }

    /// Wait approximately `d`, as precisely as the platform allows.
    pub fn wait(&self, d: Duration) {
        self.imp.wait(d);
    }

    /// What this ticker actually is, for the startup line. A run whose cadence
    /// silently fell back to `Sleep` and a run that did not are 15.9 ms and
    /// 5.1 ms per turn, and there is no way to tell them apart afterwards
    /// except by having said so.
    pub fn describe(&self) -> &'static str {
        self.imp.describe()
    }
}

impl Default for Ticker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{
        CreateWaitableTimerExW, SetWaitableTimer, Sleep, WaitForSingleObject,
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
    };

    pub struct Inner {
        /// Null when the high-resolution flag was refused; see the module docs.
        timer: HANDLE,
    }

    // SAFETY: a waitable timer handle is a kernel object usable from any thread,
    // and `Inner` is only ever used from the thread that made it. The impls
    // exist so that a `Ticker` can live in a struct that is `Send`.
    unsafe impl Send for Inner {}

    impl Inner {
        /// Deliberately a null handle, which is the same state a Windows too
        /// old for the flag lands in, so the fallback path is the one being
        /// measured rather than a second imitation of it.
        pub fn coarse() -> Self {
            Self { timer: std::ptr::null_mut() }
        }

        pub fn new() -> Self {
            // SAFETY: null security attributes and a null name ask for an
            // unnamed, unshared timer; a failure comes back as null and is
            // handled by `wait`.
            let timer = unsafe {
                CreateWaitableTimerExW(
                    std::ptr::null(),
                    std::ptr::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS,
                )
            };
            Self { timer }
        }

        pub fn wait(&self, d: Duration) {
            if self.timer.is_null() {
                // SAFETY: `Sleep` has no failure mode. Saturating because a
                // duration longer than 49 days is not a poll interval.
                unsafe {
                    Sleep(d.as_millis().min(u32::MAX as u128) as u32);
                }
                return;
            }
            // Negative is relative, in 100 ns units — the same unit
            // `QueryPerformanceCounter` resolves to on this machine (§10.8), so
            // if this were imprecise it would not be the clock's fault.
            let due = -(d.as_nanos().min(i64::MAX as u128 * 100) as i64 / 100);
            // SAFETY: a live timer handle, a live due time, no completion
            // routine, and an infinite wait on a timer that is certain to fire.
            unsafe {
                SetWaitableTimer(self.timer, &due, 0, None, std::ptr::null(), 0);
                WaitForSingleObject(self.timer, 0xFFFF_FFFF);
            }
        }

        pub fn describe(&self) -> &'static str {
            if self.timer.is_null() {
                "Sleep (no high-resolution timer on this Windows; ~15.6 ms quantum)"
            } else {
                "high-resolution waitable timer"
            }
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            if !self.timer.is_null() {
                // SAFETY: created by `new`, closed exactly once.
                unsafe {
                    CloseHandle(self.timer);
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::time::Duration;

    pub struct Inner;

    impl Inner {
        pub fn new() -> Self {
            Inner
        }

        pub fn coarse() -> Self {
            Inner
        }

        pub fn wait(&self, d: Duration) {
            std::thread::sleep(d);
        }

        pub fn describe(&self) -> &'static str {
            "thread::sleep"
        }
    }
}

/// When frame callbacks are answered: a steady grid at the display's refresh
/// (docs/waycompose-design.md §88).
///
/// # What it replaced, and the measurement that said so
///
/// Window callbacks were answered on the first turn at least 16 ms after the
/// previous answer, and subsurface callbacks at once, on the turn their commit
/// landed in. Firefox draws into a subsurface, so its frame clock was the round
/// trip to this process and back — 18.8 ms through the guest's tunnel, which
/// was the whole of a 21 ms frame — and with turns 5 ms apart, the 16 ms gate
/// came round every 20 ms. Neither is a display. A grid is: every callback
/// asked for before a tick is answered on that tick, the loop wakes *on* the
/// tick rather than on the next 5 ms turn after it, and a client that keeps up
/// draws at exactly the refresh rate however far away it is.
///
/// # The phase
///
/// When DWM can say when the last vblank was (`DwmGetCompositionTimingInfo`,
/// an interactive session), the grid is laid on it and its period is the
/// display's, re-read every second. When it cannot — session 0, Wine, Unix —
/// the grid runs free at 60 Hz, which is what `wl_output` advertises.
pub struct FrameClock {
    period: Duration,
    next: std::time::Instant,
    rephased: Option<std::time::Instant>,
    source: &'static str,
}

/// A tick this early still counts, because a high-resolution wait can return
/// a fraction of a millisecond before it was asked to (§13.8 measured 5.141 ms
/// for 5), and missing the tick for that would cost a whole period.
const EARLY: Duration = Duration::from_micros(500);

impl FrameClock {
    pub const DEFAULT_PERIOD: Duration = Duration::from_nanos(16_666_667);

    pub fn new() -> Self {
        let mut c = FrameClock {
            period: Self::DEFAULT_PERIOD,
            next: std::time::Instant::now() + Self::DEFAULT_PERIOD,
            rephased: None,
            source: "free-running 60 Hz",
        };
        c.rephase(std::time::Instant::now());
        c
    }

    /// Whether a tick has come, and if so, move to the next one. Ticks missed
    /// while the loop was busy are skipped, not replayed: one answer per
    /// callback, never a burst.
    pub fn due(&mut self, now: std::time::Instant) -> bool {
        if now + EARLY < self.next {
            return false;
        }
        while self.next <= now + EARLY {
            self.next += self.period;
        }
        if self.rephased.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
            self.rephase(now);
        }
        true
    }

    /// How long until the next tick, for the loop's wait.
    pub fn until_next(&self, now: std::time::Instant) -> Duration {
        self.next.saturating_duration_since(now)
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    /// What the grid is laid on, for the startup line.
    pub fn describe(&self) -> &'static str {
        self.source
    }

    fn rephase(&mut self, now: std::time::Instant) {
        self.rephased = Some(now);
        if let Some((vblank_in, period)) = imp_vblank::next_vblank() {
            // Plausible displays only: 24 to 360 Hz.
            if period >= Duration::from_micros(2_777) && period <= Duration::from_micros(41_667) {
                self.period = period;
                self.next = now + vblank_in;
                self.source = "DWM vblank";
                return;
            }
        }
        self.source = "free-running 60 Hz";
    }
}

impl Default for FrameClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
mod imp_vblank {
    use std::time::Duration;
    use windows_sys::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo};
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    /// Time from now to DWM's next vblank, and the refresh period. `None`
    /// when DWM is not composing this session.
    pub fn next_vblank() -> Option<(Duration, Duration)> {
        let mut info: DWM_TIMING_INFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<DWM_TIMING_INFO>() as u32;
        let (mut now, mut freq) = (0i64, 0i64);
        // SAFETY: `info` is a correctly sized, writable DWM_TIMING_INFO with
        // its size field set, and a null window asks for the desktop's timing;
        // the counter calls write one i64 each.
        let ok = unsafe {
            DwmGetCompositionTimingInfo(std::ptr::null_mut(), &mut info) == 0
                && QueryPerformanceCounter(&mut now) != 0
                && QueryPerformanceFrequency(&mut freq) != 0
        };
        let (period, last) = (info.qpcRefreshPeriod as i64, info.qpcVBlank as i64);
        if !ok || freq <= 0 || period <= 0 || last <= 0 {
            return None;
        }
        // The first vblank after now.
        let behind = (now - last).rem_euclid(period);
        let ahead = period - behind;
        let to = |ticks: i64| Duration::from_nanos((ticks as i128 * 1_000_000_000 / freq as i128) as u64);
        Some((to(ahead), to(period)))
    }
}

#[cfg(not(windows))]
mod imp_vblank {
    pub fn next_vblank() -> Option<(std::time::Duration, std::time::Duration)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// The one thing worth asserting on the platform that runs `cargo test`:
    /// that a wait waits, and that it is not the 15.6 ms quantum this module
    /// exists to escape. The Windows number is §13.8's and cannot be checked
    /// here; this is the guard against `wait` becoming a no-op.
    #[test]
    fn a_short_wait_waits_and_is_short() {
        let t = Ticker::new();
        let start = Instant::now();
        for _ in 0..4 {
            t.wait(Duration::from_millis(5));
        }
        let each = start.elapsed() / 4;
        assert!(each >= Duration::from_millis(4), "returned early: {each:?}");
        // Generous, because this runs on a loaded build machine. It still fails
        // if a turn costs a scheduler tick, which is the regression.
        assert!(each < Duration::from_millis(15), "too slow: {each:?}");
    }

    #[test]
    fn it_says_what_it_is() {
        assert!(!Ticker::new().describe().is_empty());
    }
}

#[cfg(test)]
mod frame_clock {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_tick_is_due_once_per_period_and_missed_ticks_are_skipped() {
        let mut c = FrameClock::new();
        let p = c.period();
        let t0 = Instant::now();
        // Not before the first tick (allowing the early margin).
        assert!(!c.due(t0));
        let first = t0 + c.until_next(t0);
        assert!(c.due(first));
        // Not again within the same period.
        assert!(!c.due(first + p / 2));
        assert!(c.due(first + p));
        // Five periods late: one answer, and the next tick is ahead of now.
        let late = first + p * 6 + p / 3;
        assert!(c.due(late));
        assert!(!c.due(late));
        assert!(c.until_next(late) <= p);
    }
}
