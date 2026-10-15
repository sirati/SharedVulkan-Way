//! Frame callbacks answered on this side of the VM boundary, a bounded number
//! ahead of the compositor (docs/waycompose-design.md §88).
//!
//! # Why the proxy answers them at all
//!
//! A client that draws on `wl_surface.frame` — Firefox does — draws its next
//! frame when the compositor says `done`. The compositor is in the guest and
//! its answer comes back through the launcher's ssh tunnel, whose guest end is
//! Windows' `ssh.exe`, and §88 measured what that costs: a small message from
//! the guest waits until the next message from the host wakes `ssh.exe`, 0.5 to
//! 47 ms with nothing coming the other way, p50 14 ms during a scroll. The
//! compositor answered at once and Firefox still waited 18.8 ms a frame, which
//! made the frame rate that wait. No setting of ours reaches inside `ssh.exe`.
//!
//! So the proxy keeps the client's frame clock itself: every callback asked for
//! before a tick is answered on the tick, at the display's rate, and the
//! request still goes to the compositor, whose own `done` arrives later and is
//! swallowed. The compositor stays in charge of *whether* the client may draw,
//! through a credit: at most [`Pacer::depth`] callbacks may be answered here
//! that the compositor has not answered yet. A compositor that stops answering
//! — a window it wants throttled, or one that has gone — stops the client after
//! that many frames, as it would have stopped it directly.
//!
//! # The object's life is untouched
//!
//! A `wl_callback` is destroyed by its `done`, and its id is freed by the
//! compositor's `wl_display.delete_id`, which the proxy forwards as it always
//! did. The client cannot reuse the id before that, so the compositor's late
//! `done` for it cannot be mistaken for anything else; and the `delete_id` is
//! also what returns the credit if the compositor never sends a `done` at all.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A tick this early still counts; `poll` rounds its timeout to milliseconds.
const EARLY: Duration = Duration::from_micros(500);

pub struct Pacer {
    period: Duration,
    next: Instant,
    depth: usize,
    /// Asked for and not yet answered to the client, in order.
    pending: Vec<Pending>,
    /// Answered here and not yet by the compositor: callback -> surface.
    ahead: HashMap<u32, u32>,
}

struct Pending {
    callback: u32,
    surface: u32,
    /// The compositor has already answered: answering it costs no credit.
    acked: bool,
    /// The compositor has also freed the id. Its `delete_id` is held until the
    /// client has had its `done`, or the client would be told an object it is
    /// still waiting on no longer exists, and would wait for ever.
    deleted: bool,
}

/// What the compositor's `done` for a callback means here.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// A frame callback: drop it. If the proxy had answered it the credit is
    /// back; if not, the proxy answers it on its next tick without spending
    /// one. The client hears one clock, the proxy's — forwarding the
    /// compositor's answers as well gave it two, and a client that draws on
    /// either ran at twice the rate (measured under Wine: 120 fps).
    Swallow,
    /// Not a frame callback (a `wl_display.sync`, say): forward untouched.
    NotOurs,
}

impl Pacer {
    pub fn new(hz: u32, depth: usize) -> Pacer {
        let period = Duration::from_nanos(1_000_000_000 / u64::from(hz.max(1)));
        Pacer { period, next: Instant::now() + period, depth: depth.max(1), pending: Vec::new(), ahead: HashMap::new() }
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    /// `wl_surface.frame`: the client wants `callback` answered.
    pub fn requested(&mut self, callback: u32, surface: u32) {
        self.pending.push(Pending { callback, surface, acked: false, deleted: false });
    }

    /// The compositor sent `done` for `callback`.
    pub fn answered(&mut self, callback: u32) -> Answer {
        if self.ahead.remove(&callback).is_some() {
            return Answer::Swallow;
        }
        match self.pending.iter_mut().find(|e| e.callback == callback) {
            Some(e) => {
                e.acked = true;
                Answer::Swallow
            }
            None => Answer::NotOurs,
        }
    }

    /// `wl_display.delete_id` from the compositor. `true`: hold it, the proxy
    /// sends it after its own `done` for that callback. `false`: forward it.
    pub fn deleted(&mut self, id: u32) -> bool {
        // Answered here already: the client has had its `done`, the id may go.
        if self.ahead.remove(&id).is_some() {
            return false;
        }
        match self.pending.iter_mut().find(|e| e.callback == id) {
            Some(e) => {
                // A `delete_id` with no `done` before it still means the
                // compositor will not answer: free, like an answer.
                e.acked = true;
                e.deleted = true;
                true
            }
            None => false,
        }
    }

    /// `wl_surface.destroy`: its callbacks hold no credit. They are still
    /// answered, on the next tick, because the client still holds them.
    pub fn surface_destroyed(&mut self, surface: u32) {
        self.ahead.retain(|_, s| *s != surface);
        for e in self.pending.iter_mut().filter(|e| e.surface == surface) {
            e.acked = true;
        }
    }

    /// If a tick has come, the callbacks to answer on it, oldest first, as
    /// many as the credit allows, each with whether its held `delete_id`
    /// follows. Missed ticks are skipped, never replayed.
    pub fn tick(&mut self, now: Instant) -> Vec<(u32, bool)> {
        if now + EARLY < self.next {
            return Vec::new();
        }
        while self.next <= now + EARLY {
            self.next += self.period;
        }
        // In order: one the compositor has already answered costs nothing, one
        // it has not needs a credit, and the first that cannot have one stops
        // the tick, so callbacks are never answered out of order.
        let mut out = Vec::new();
        let mut taken = 0;
        for e in &self.pending {
            if !e.acked {
                if self.ahead.len() >= self.depth {
                    break;
                }
                self.ahead.insert(e.callback, e.surface);
            }
            out.push((e.callback, e.deleted));
            taken += 1;
        }
        self.pending.drain(..taken);
        out
    }

    /// How long the loop may sleep before a tick that has something to
    /// answer. `None` when nothing is waiting, so an idle proxy sleeps as long
    /// as it did before.
    pub fn until_tick(&self, now: Instant) -> Option<Duration> {
        (!self.pending.is_empty()).then(|| self.next.saturating_duration_since(now))
    }

    /// How many answers are out ahead of the compositor.
    pub fn ahead(&self) -> usize {
        self.ahead.len()
    }
}

/// `wl_callback.done`'s argument: milliseconds of `CLOCK_MONOTONIC`, the clock
/// a Linux compositor would use, wrapping as the protocol's `uint` does.
pub fn now_ms() -> u32 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid clock id and a writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_tick(p: &Pacer) -> Instant {
        p.next
    }

    fn ids(v: Vec<(u32, bool)>) -> Vec<u32> {
        v.into_iter().map(|(c, _)| c).collect()
    }

    #[test]
    fn callbacks_are_answered_on_the_tick_and_not_before() {
        let mut p = Pacer::new(60, 3);
        p.requested(10, 5);
        let t = at_tick(&p);
        assert!(p.tick(t - Duration::from_millis(5)).is_empty());
        assert_eq!(ids(p.tick(t)), vec![10]);
        // The same tick does not answer twice.
        p.requested(11, 5);
        assert!(p.tick(t).is_empty());
        assert_eq!(ids(p.tick(t + p.period)), vec![11]);
    }

    #[test]
    fn the_credit_stops_a_client_the_compositor_has_not_answered() {
        let mut p = Pacer::new(60, 2);
        let mut t = at_tick(&p);
        for cb in 1..=4 {
            p.requested(cb, 7);
        }
        assert_eq!(ids(p.tick(t)), vec![1, 2]);
        t += p.period;
        assert!(p.tick(t).is_empty(), "two ahead is the limit");
        // The compositor answers the first: it is swallowed and one more may go.
        assert_eq!(p.answered(1), Answer::Swallow);
        assert!(!p.deleted(1), "its delete_id goes straight through");
        t += p.period;
        assert_eq!(ids(p.tick(t)), vec![3]);
        // delete_id without a done also returns the credit.
        assert!(!p.deleted(2));
        t += p.period;
        assert_eq!(ids(p.tick(t)), vec![4]);
    }

    #[test]
    fn a_compositor_that_answers_first_waits_for_the_tick_and_its_delete_id_is_held() {
        let mut p = Pacer::new(60, 1);
        p.requested(9, 1);
        assert_eq!(p.answered(9), Answer::Swallow, "the client hears the proxy's clock only");
        assert!(p.deleted(9), "held: the client has not had its done");
        p.requested(10, 1);
        // 9 is free and carries its delete_id; 10 takes the one credit.
        assert_eq!(p.tick(at_tick(&p)), vec![(9, true), (10, false)]);
        assert_eq!(p.ahead(), 1);
        assert_eq!(p.answered(42), Answer::NotOurs, "a sync is not a frame callback");
        assert!(!p.deleted(42));
    }

    #[test]
    fn a_destroyed_surface_holds_no_credit_and_its_callbacks_are_still_answered() {
        let mut p = Pacer::new(60, 1);
        p.requested(3, 8);
        let t = at_tick(&p);
        assert_eq!(ids(p.tick(t)), vec![3]);
        p.requested(4, 8);
        p.surface_destroyed(8);
        assert_eq!(p.ahead(), 0);
        p.requested(5, 9);
        assert_eq!(ids(p.tick(t + p.period)), vec![4, 5]);
    }
}
