//! Device changes come in bursts: plugging in a headset is a device added,
//! two state changes and two default changes within a few milliseconds.
//! Linux should see the state once, after the burst, and within a second of
//! its start however long the burst goes on.

use std::time::{Duration, Instant};

/// The burst is over after this long without a change.
pub const QUIET: Duration = Duration::from_millis(100);
/// However long a burst lasts, the state is sent this long after its first
/// change. Below the one second the host's audio state promises.
pub const MAX: Duration = Duration::from_millis(800);

/// How much longer to wait for a burst that began at `first` and was last
/// added to at `last`, or `None` if the state should be taken now.
pub fn wait_left(first: Instant, last: Instant, now: Instant) -> Option<Duration> {
    let until = (last + QUIET).min(first + MAX);
    (until > now).then(|| until - now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_change_waits_for_quiet() {
        let t = Instant::now();
        assert_eq!(wait_left(t, t, t), Some(QUIET));
        assert_eq!(wait_left(t, t, t + Duration::from_millis(40)), Some(Duration::from_millis(60)));
        assert_eq!(wait_left(t, t, t + QUIET), None);
    }

    #[test]
    fn a_long_burst_is_cut_at_max() {
        let t = Instant::now();
        // A change every 50 ms forever: never quiet, sent at MAX.
        let last = t + Duration::from_millis(750);
        assert_eq!(wait_left(t, last, last), Some(Duration::from_millis(50)));
        assert_eq!(wait_left(t, t + MAX, t + MAX), None);
    }
}
