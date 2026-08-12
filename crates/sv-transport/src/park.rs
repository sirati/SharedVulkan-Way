//! The wait primitive, behind a trait, with a deadline on every wait.
//!
//! design.md §4.5 and D9:
//!
//! > Ship polling. sharedgl proves polling works and demonstrates nothing
//! > whatsoever about interrupts, since it never uses `ivshmem-doorbell` and
//! > never issues the doorbell IOCTLs its own vendored driver implements. We are
//! > not architecting around an interrupt path nobody has measured.
//!
//! So there are two traits and one implementation, and the whole point of the
//! traits is that the second implementation changes no caller.
//!
//! # Timeouts are mandatory, and that is enforced by the types
//!
//! > **Timeouts are mandatory on every wait.** R4 is a denial of service in both
//! > directions, and a polling transport makes deadlock easy by accident.
//!
//! "Mandatory" is easy to write and easy to forget, so [`Parker::park`] takes a
//! [`Deadline`] by value and there is no constructor for an infinite one. A
//! caller that wants to wait for ever has to write the loop that re-arms the
//! deadline, and in writing it has to decide what happens each time it expires —
//! which is exactly the decision R4 says must not be skipped. [`wait_until`]
//! makes that decision once, correctly, and is what callers should use:
//! on expiry it consults the peer heartbeat and the generation counter before
//! choosing between "slow" and "gone".

use crate::control::{Control, Liveness, LivenessMonitor, RingStatus};
use crate::ring::{RingCtrl, RingId};
use crate::{Error, Result};
use std::time::{Duration, Instant};

/// A point in time a wait must not pass. There is deliberately no `Deadline`
/// that never expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn after(d: Duration) -> Deadline {
        Deadline(Instant::now() + d)
    }
    pub fn at(t: Instant) -> Deadline {
        Deadline(t)
    }
    pub fn expired(&self, now: Instant) -> bool {
        now >= self.0
    }
    pub fn remaining(&self, now: Instant) -> Duration {
        self.0.saturating_duration_since(now)
    }
    pub fn instant(&self) -> Instant {
        self.0
    }
}

/// Why a park stopped. design.md §4.5's `Woken | TimedOut | Fatal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkOutcome {
    Woken,
    TimedOut,
    Fatal,
}

/// What a parker is waiting on.
///
/// design.md writes the signature as `park(ring, deadline)`. A ring alone is not
/// enough to wait on — the parker has to know *what about* the ring it is
/// waiting for, and different call sites wait for different things (a packet
/// arriving, space appearing, a barrier's seqno passing). So the ring travels
/// with a readiness predicate. The shape of the design's signature is preserved;
/// what it means is made precise.
pub struct WaitSite<'a> {
    pub ring: RingId,
    /// Has the thing we are waiting for happened?
    pub ready: &'a dyn Fn() -> bool,
    /// Has the ring gone fatal underneath us?
    pub status: &'a dyn Fn() -> RingStatus,
}

impl<'a> WaitSite<'a> {
    pub fn new(
        ring: RingId,
        ready: &'a dyn Fn() -> bool,
        status: &'a dyn Fn() -> RingStatus,
    ) -> Self {
        WaitSite { ring, ready, status }
    }
}

/// The consumer's half of a park.
pub trait Parker: Send + Sync {
    fn park(&self, site: &WaitSite<'_>, deadline: Deadline) -> ParkOutcome;
}

/// The producer's half: tell a parked consumer that there is work.
pub trait Doorbell: Send + Sync {
    /// Called *only* when the ring's status is `IDLE`, which is design.md
    /// §4.4's "the producer checks `IDLE` before paying for a notification".
    fn ring(&self, ring: RingId);
}

/// A doorbell that does nothing, which is the correct one for a spinning
/// consumer and is not a placeholder.
///
/// With [`SpinYield`] on the other side there is genuinely nothing to do: the
/// consumer is polling shared memory, so the release store that published the
/// packet *is* the notification. Naming it rather than making the doorbell an
/// `Option` keeps the call site identical when a real doorbell arrives.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoDoorbell;

impl Doorbell for NoDoorbell {
    fn ring(&self, _ring: RingId) {}
}

/// design.md §4.5 implementation 1: "bounded spin with `pause`, then `yield`,
/// then sleep with exponential backoff to a 200 µs ceiling. v1 on both sides."
#[derive(Debug, Clone, Copy)]
pub struct SpinYield {
    /// Iterations of `pause` before yielding.
    pub spins: u32,
    /// Yields before sleeping.
    pub yields: u32,
    pub initial_sleep: Duration,
    pub max_sleep: Duration,
}

impl Default for SpinYield {
    fn default() -> Self {
        SpinYield {
            spins: 256,
            yields: 32,
            initial_sleep: Duration::from_micros(2),
            max_sleep: Duration::from_micros(200),
        }
    }
}

impl SpinYield {
    /// A parker that never sleeps. For tests, and for Miri, where a sleep is
    /// wall-clock time an interpreter cannot afford.
    pub fn eager() -> Self {
        SpinYield {
            spins: 8,
            yields: 0,
            initial_sleep: Duration::ZERO,
            max_sleep: Duration::ZERO,
        }
    }
}

impl Parker for SpinYield {
    fn park(&self, site: &WaitSite<'_>, deadline: Deadline) -> ParkOutcome {
        // The cheap check first: the overwhelmingly common case is that the
        // thing already happened and this function is a load and a branch.
        if (site.ready)() {
            return ParkOutcome::Woken;
        }
        for _ in 0..self.spins {
            core::hint::spin_loop();
            if (site.ready)() {
                return ParkOutcome::Woken;
            }
        }
        for _ in 0..self.yields {
            std::thread::yield_now();
            if (site.ready)() {
                return ParkOutcome::Woken;
            }
            if (site.status)() == RingStatus::Fatal {
                return ParkOutcome::Fatal;
            }
        }
        let mut sleep = self.initial_sleep;
        loop {
            if (site.ready)() {
                return ParkOutcome::Woken;
            }
            if (site.status)() == RingStatus::Fatal {
                return ParkOutcome::Fatal;
            }
            let now = Instant::now();
            if deadline.expired(now) {
                return ParkOutcome::TimedOut;
            }
            if sleep.is_zero() {
                std::thread::yield_now();
            } else {
                std::thread::sleep(sleep.min(deadline.remaining(now)));
                sleep = (sleep * 2).min(self.max_sleep);
            }
        }
    }
}

/// Wait for `ready`, and turn a timeout into a diagnosis rather than into a
/// retry loop.
///
/// This is the function callers should use. It is where design.md §4.5's last
/// paragraph lives:
///
/// > A wait that expires checks the peer heartbeat and the generation counter
/// > before deciding between "slow" and "gone".
///
/// `overall` bounds the whole wait; the parker is re-armed with a shorter
/// deadline each round so that liveness is re-examined periodically rather than
/// once at the very end — a peer that died in the first millisecond of a
/// thirty-second wait should not cost thirty seconds to notice.
pub fn wait_until(
    control: &Control,
    ctrl: &RingCtrl,
    parker: &dyn Parker,
    ready: &dyn Fn() -> bool,
    overall: Deadline,
    liveness_interval: Duration,
) -> Result<()> {
    let status = || ctrl.status();
    let site = WaitSite::new(ctrl.id(), ready, &status);
    let mut monitor = LivenessMonitor::new(Instant::now(), liveness_interval * 3);
    loop {
        // Beat before parking. A side that is blocked here is *alive*, and if it
        // stops saying so then two peers waiting on each other — the normal
        // shape of a request and its reply — each declare the other dead. See
        // `Control::beat_now`.
        control.beat_now()?;
        let now = Instant::now();
        if overall.expired(now) {
            return Err(Error::TimedOut { ring: ctrl.id().index() });
        }
        let slice = Deadline::at(now + liveness_interval.min(overall.remaining(now)));
        match parker.park(&site, slice) {
            ParkOutcome::Woken => return Ok(()),
            ParkOutcome::Fatal => {
                let code = ctrl.fatal()?.map(|(c, _, _)| c).unwrap_or(0);
                return Err(Error::RingFatal { ring: ctrl.id().index(), code });
            }
            ParkOutcome::TimedOut => match monitor.poll(control, Instant::now())? {
                Liveness::Alive | Liveness::Slow => continue,
                Liveness::Gone => return Err(Error::PeerGone { ring: ctrl.id().index() }),
                Liveness::Restarted { generation } => {
                    return Err(Error::PeerRestarted {
                        expected: control.generation(),
                        found: generation,
                    });
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Geometry, ProtocolIds, RingKind};
    use crate::region::SharedRegion;
    use crate::ring::{Lane, RingConsumer, RingProducer};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn setup() -> (Control, Control, u32) {
        let g = Geometry::small();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let ids = ProtocolIds::new(1, 1);
        let owner = Control::create(Arc::clone(&region), g, ids, 1).unwrap();
        let peer = Control::attach(region, ids).unwrap();
        let slot = peer.claim_ring(RingKind::Device, 1, None).unwrap();
        (owner, peer, slot)
    }

    #[test]
    fn a_ready_condition_costs_no_wait_at_all() {
        let (owner, _peer, slot) = setup();
        let ctrl = RingCtrl::open(&owner, slot).unwrap();
        let ready = || true;
        let status = || ctrl.status();
        let site = WaitSite::new(ctrl.id(), &ready, &status);
        assert_eq!(SpinYield::default().park(&site, Deadline::after(Duration::ZERO)), ParkOutcome::Woken);
    }

    #[test]
    fn a_wait_that_will_never_be_satisfied_times_out_rather_than_hangs() {
        let (owner, _peer, slot) = setup();
        let ctrl = RingCtrl::open(&owner, slot).unwrap();
        let ready = || false;
        let status = || ctrl.status();
        let site = WaitSite::new(ctrl.id(), &ready, &status);
        let t0 = Instant::now();
        assert_eq!(
            SpinYield::eager().park(&site, Deadline::after(Duration::from_millis(5))),
            ParkOutcome::TimedOut
        );
        assert!(t0.elapsed() < Duration::from_secs(5), "the deadline was not honoured");
    }

    #[test]
    fn a_fatal_ring_stops_the_wait_instead_of_burning_the_deadline() {
        let (owner, _peer, slot) = setup();
        let ctrl = RingCtrl::open(&owner, slot).unwrap();
        ctrl.set_fatal(7, 1, "decoder gave up").unwrap();
        let ready = || false;
        let status = || ctrl.status();
        let site = WaitSite::new(ctrl.id(), &ready, &status);
        assert_eq!(
            SpinYield::eager().park(&site, Deadline::after(Duration::from_secs(30))),
            ParkOutcome::Fatal
        );
    }

    #[test]
    fn wait_until_reports_a_dead_peer_rather_than_a_timeout() {
        let (owner, _peer, slot) = setup();
        let ctrl = RingCtrl::open(&owner, slot).unwrap();
        // The peer never beats and never delivers.
        let err = wait_until(
            &owner,
            &ctrl,
            &SpinYield::eager(),
            &|| false,
            Deadline::after(Duration::from_millis(60)),
            Duration::from_millis(5),
        )
        .unwrap_err();
        assert_eq!(err, Error::PeerGone { ring: slot });
    }

    #[test]
    fn wait_until_reports_a_restarted_peer_by_name() {
        let (owner, peer, slot) = setup();
        let ctrl = RingCtrl::open(&owner, slot).unwrap();
        peer.beat(1, 0).unwrap();
        owner
            .region()
            .atomic_u32(crate::control::OFF_GENERATION)
            .unwrap()
            .store(9, Ordering::Release);
        let err = wait_until(
            &owner,
            &ctrl,
            &SpinYield::eager(),
            &|| false,
            Deadline::after(Duration::from_millis(50)),
            Duration::from_millis(2),
        )
        .unwrap_err();
        assert_eq!(err, Error::PeerRestarted { expected: 1, found: 9 });
    }

    /// The mutual false positive, reproduced and then prevented.
    ///
    /// Before `wait_until` beat on every slice, two sides blocked on each other
    /// each watched the other's heartbeat freeze and each returned `PeerGone`.
    /// It did not show up in a single process — everything was too fast to
    /// notice — and it failed immediately once the two sides were put in
    /// separate containers, which is the argument for that harness in one test.
    #[test]
    #[cfg_attr(miri, ignore = "sleeps for a third of a second to let a heartbeat go stale")]
    fn a_side_blocked_in_a_wait_still_says_it_is_alive() {
        let (owner, peer, slot) = setup();
        let ctrl = RingCtrl::open(&owner, slot).unwrap();
        let owner = std::sync::Arc::new(owner);
        let waiter = std::sync::Arc::clone(&owner);

        // The owner blocks on something that will never happen, for longer than
        // the liveness timeout it would be judged by.
        let handle = std::thread::spawn(move || {
            wait_until(
                &waiter,
                &ctrl,
                &SpinYield::eager(),
                &|| false,
                Deadline::after(Duration::from_millis(300)),
                Duration::from_millis(20),
            )
        });

        // Meanwhile the peer watches the owner's heartbeat with a timeout well
        // inside the wait. It must never see it stall.
        let t0 = Instant::now();
        let mut mon = LivenessMonitor::new(t0, Duration::from_millis(60));
        let mut sightings = 0;
        while t0.elapsed() < Duration::from_millis(250) {
            // The watcher beats too. Liveness is symmetric: the waiting thread
            // is itself judging *this* side, and a watcher that only watched
            // would be declared gone by the thing it was watching.
            peer.beat_now().unwrap();
            match mon.poll(&peer, Instant::now()).unwrap() {
                Liveness::Alive => sightings += 1,
                Liveness::Slow => {}
                bad => panic!("a blocked but healthy peer was reported {bad:?}"),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // A count of zero would mean the loop never observed a beat at all,
        // which would make the assertion above vacuous.
        assert!(sightings > 3, "only {sightings} heartbeats seen from a blocked peer");

        // And the wait itself still ends in a timeout rather than in PeerGone.
        assert_eq!(handle.join().unwrap().unwrap_err(), Error::TimedOut { ring: slot });
    }

    /// The trait's whole reason for existing: a second implementation, swapped
    /// in at the call site with no other change. If this compiles, an
    /// event-driven parker can replace the polling one without touching
    /// `wait_until` or any caller of it.
    #[test]
    fn a_second_parker_implementation_needs_no_caller_change() {
        struct CountingParker(std::sync::atomic::AtomicU32);
        impl Parker for CountingParker {
            fn park(&self, site: &WaitSite<'_>, _d: Deadline) -> ParkOutcome {
                self.0.fetch_add(1, Ordering::Relaxed);
                if (site.ready)() { ParkOutcome::Woken } else { ParkOutcome::TimedOut }
            }
        }
        let (owner, peer, slot) = setup();
        let ctrl = RingCtrl::open(&owner, slot).unwrap();
        let mut prod = RingProducer::open(&peer, slot, Lane::Command).unwrap();
        let mut cons = RingConsumer::open(&owner, slot, Lane::Command).unwrap();
        let mut cmd = vec![0u8; 16];
        cmd[8..12].copy_from_slice(&16u32.to_le_bytes());
        prod.push(&cmd, None).unwrap();

        let parker = CountingParker(std::sync::atomic::AtomicU32::new(0));
        let arrived = AtomicBool::new(false);
        let ready = || {
            let r = !cons.is_empty().unwrap();
            arrived.store(r, Ordering::Relaxed);
            r
        };
        wait_until(
            &owner,
            &ctrl,
            &parker,
            &ready,
            Deadline::after(Duration::from_millis(50)),
            Duration::from_millis(5),
        )
        .unwrap();
        assert!(arrived.load(Ordering::Relaxed));
        assert_eq!(parker.0.load(Ordering::Relaxed), 1, "the custom parker was not used");
        assert!(cons.next().unwrap().is_some());
    }

    #[test]
    fn the_doorbell_is_only_paid_for_when_the_consumer_has_parked() {
        // design.md §4.4: "the producer checks `IDLE` before paying for a
        // notification". The check is the producer's, so this test is about the
        // status word being usable for it — the mechanism, not a policy buried
        // in the producer.
        struct Counting(std::sync::atomic::AtomicU32);
        impl Doorbell for Counting {
            fn ring(&self, _r: RingId) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let (owner, _peer, slot) = setup();
        let ctrl = RingCtrl::open(&owner, slot).unwrap();
        let bell = Counting(std::sync::atomic::AtomicU32::new(0));

        ctrl.set_status(RingStatus::Alive).unwrap();
        if ctrl.status() == RingStatus::Idle {
            bell.ring(ctrl.id());
        }
        assert_eq!(bell.0.load(Ordering::Relaxed), 0, "rang a doorbell nobody needed");

        ctrl.set_status(RingStatus::Idle).unwrap();
        if ctrl.status() == RingStatus::Idle {
            bell.ring(ctrl.id());
            ctrl.note_notify().unwrap();
        }
        assert_eq!(bell.0.load(Ordering::Relaxed), 1);
        assert_eq!(ctrl.notify_count(), 1);
        NoDoorbell.ring(ctrl.id());
    }
}
