//! The session: publish the control region, then drain rings until told to stop.
//!
//! # One thread per ring, and it is not a preference
//!
//! design.md §3 draws "ring consumers (1 thread/ring, spin-then-park)" and
//! `sv-loopback` records why in a comment that was written after the fact:
//!
//! > A single thread draining both rings deadlocks the first time a queue packet
//! > carries a barrier: it blocks waiting for a device-ring seqno that only it
//! > could have advanced. That is not a hypothetical — it is what this harness
//! > did before this comment existed, and it hung.
//!
//! So the topology is load-bearing. What this file adds over the loopback is
//! that the threads share a driver and an object table, which is a second
//! ordering problem on top of the first: the table is behind a mutex, and the
//! **barrier is waited on before the lock is taken**. Waiting for another ring
//! while holding the table would reintroduce exactly the deadlock the threads
//! were separated to avoid, one level down.

use crate::server::{GpuServer, Shared};
use std::sync::Arc;
use std::time::{Duration, Instant};
use sv_transport::control::{Control, Flags, REVERSE_RING_SLOT, RingKind, RingState, RingStatus};
use sv_transport::park::{Deadline, SpinYield, wait_until};
use sv_transport::ring::{Lane, RingConsumer, RingCtrl, RingProducer, barrier_satisfied};

/// How long a ring thread will sit with nothing arriving before it concludes the
/// app side is gone. R4 says every wait has a deadline and there is no API for an
/// infinite one; this is the session's.
pub const IDLE_LIMIT: Duration = Duration::from_secs(120);

/// Errors cross thread boundaries as strings: a worker's failure has to be
/// reportable by the thread that joins it, and `Box<dyn Error>` is not `Send`.
fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Wait for the app side to claim its rings, then serve them.
///
/// Returns the number of commands dispatched.
pub fn serve(
    control: Arc<Control>,
    shared: Arc<Shared>,
    slot_count: u32,
    ready_deadline: Duration,
) -> Result<u64, String> {
    // The app side claims rings after it attaches, and the order it claims them
    // in is its business. Watch the directory rather than assuming one.
    let until = Instant::now() + ready_deadline;
    let mut slots: Vec<u32> = Vec::new();
    while Instant::now() < until {
        slots.clear();
        for slot in 0..slot_count {
            let live = control.ring_state(slot).map(|s| s == RingState::Live).unwrap_or(false);
            if !live || slot == REVERSE_RING_SLOT {
                continue;
            }
            match control.ring_kind(slot) {
                Ok(RingKind::Instance) | Ok(RingKind::Device) | Ok(RingKind::Queue) => {
                    slots.push(slot)
                }
                _ => {}
            }
        }
        if !slots.is_empty() {
            // Give the app side a moment to finish claiming the rest: it claims
            // instance, device and queue in three separate calls, and starting a
            // thread for the first one alone would leave the others unserved.
            std::thread::sleep(Duration::from_millis(20));
            slots.clear();
            for slot in 0..slot_count {
                let live = control.ring_state(slot).map(|s| s == RingState::Live).unwrap_or(false);
                if live
                    && slot != REVERSE_RING_SLOT
                    && matches!(
                        control.ring_kind(slot),
                        Ok(RingKind::Instance) | Ok(RingKind::Device) | Ok(RingKind::Queue)
                    )
                {
                    slots.push(slot);
                }
            }
            break;
        }
        control.beat_now().map_err(err)?;
        std::thread::sleep(Duration::from_millis(5));
    }
    if slots.is_empty() {
        return Err("the app side never claimed a ring".into());
    }

    for &slot in &slots {
        RingCtrl::open(&control, slot).map_err(err)?.set_status(RingStatus::Alive).map_err(err)?;
    }
    eprintln!(
        "[gpu] serving rings {:?} on {} ({})",
        slots,
        shared.driver.name,
        shared.driver.driver_key()
    );

    // The heartbeat has to keep going while the ring threads block, and it
    // cannot be any one of them: a ring with nothing on it parks, and a parked
    // ring that owed the heartbeat would look to the app side exactly like a GPU
    // side that died (R4's "slow or gone" distinction, from the wrong end).
    let beating = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let heart = {
        let control = Arc::clone(&control);
        let beating = Arc::clone(&beating);
        std::thread::spawn(move || {
            while beating.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = control.beat_now();
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };

    let mut workers = Vec::new();
    for slot in slots {
        let control = Arc::clone(&control);
        let shared = Arc::clone(&shared);
        workers.push(std::thread::spawn(move || drain(control, shared, slot)));
    }

    let mut total = 0u64;
    let mut first_error = None;
    for w in workers {
        match w.join() {
            Ok(Ok(n)) => total += n,
            Ok(Err(e)) => {
                first_error.get_or_insert(e);
            }
            Err(_) => {
                first_error.get_or_insert("a ring consumer panicked".to_string());
            }
        }
    }
    beating.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = heart.join();

    match first_error {
        Some(e) => Err(e),
        None => Ok(total),
    }
}

/// One ring, until the app side shuts down or stops talking.
fn drain(control: Arc<Control>, shared: Arc<Shared>, slot: u32) -> Result<u64, String> {
    let mut cons = RingConsumer::open(&control, slot, Lane::Command).map_err(err)?;
    let mut replies = RingProducer::open(&control, slot, Lane::Reply).map_err(err)?;
    let ctrl = RingCtrl::open(&control, slot).map_err(err)?;
    let parker = SpinYield::default();
    let mut srv = GpuServer::new(Arc::clone(&shared));
    let mut handled = 0u64;
    let mut reply = Vec::new();
    let mut idle_since = Instant::now();

    loop {
        if control.flags().map_err(err)? & Flags::SHUTDOWN != 0 {
            break;
        }
        if idle_since.elapsed() > IDLE_LIMIT {
            return Err(format!("ring {slot}: nothing for {IDLE_LIMIT:?}; the app side is gone"));
        }

        // A short slice, so the shutdown flag and the idle limit are both
        // checked often. The long deadline belongs to the loop, not to one wait
        // inside it — the mistake `sv-loopback`'s app side records.
        //
        // The liveness interval is 200 ms and not the slice, because
        // `wait_until` builds its window as three times this value and §4.2 only
        // promises a heartbeat every 100 ms. A 25 ms interval gave a 75 ms
        // window, which declared a perfectly healthy `vulkaninfo` dead between
        // two of its own calls.
        let slice = Deadline::after(Duration::from_millis(50));
        match wait_until(
            &control,
            &ctrl,
            &parker,
            &|| !cons.is_empty().unwrap_or(false),
            slice,
            Duration::from_millis(200),
        ) {
            Ok(()) => {}
            Err(sv_transport::Error::TimedOut { .. }) => continue,
            // The app side going away is how a session normally ends, not a
            // failure: it has already got its replies.
            Err(sv_transport::Error::PeerGone { .. }) => break,
            Err(e) => return Err(err(e)),
        }

        // `next` and `advance` are two calls precisely so a barrier can be
        // honoured between them (R22).
        let Some((seqno, barrier, payload)) = ({
            cons.next().map_err(err)?.map(|p| (p.seqno, p.barrier, p.payload.to_vec()))
        }) else {
            continue;
        };
        idle_since = Instant::now();

        if let Some(b) = barrier {
            // Before the object table is touched. A barrier waited on with the
            // table held would deadlock against the ring that has to advance it.
            wait_until(
                &control,
                &ctrl,
                &parker,
                &|| barrier_satisfied(&control, b).unwrap_or(false),
                Deadline::after(Duration::from_secs(10)),
                Duration::from_millis(50),
            )
            .map_err(|e| format!("ring {slot} seqno {seqno}: barrier on ring {}: {e}", b.ring))?;
        }

        reply.clear();
        let mut dec = sv_wire::Decoder::new(&payload);
        match sv_proto::dispatch(&mut dec, &mut srv, &mut reply) {
            Ok(op) => {
                if !reply.is_empty() {
                    replies.push_seq(seqno, &reply, None).map_err(err)?;
                }
                handled += 1;
                let _ = op;
            }
            Err(e) => {
                // R13's detector firing, or a handle that has not arrived. Both
                // are fatal to the ring and both must say which command, because
                // "the stream desynchronised" without an opcode is undebuggable.
                let detail = match srv.last_error.take() {
                    Some(d) => format!("{e} ({d})"),
                    None => e.to_string(),
                };
                let _ = ctrl.set_fatal(1, seqno, &detail);
                return Err(format!("ring {slot} seqno {seqno}: {detail}"));
            }
        }
        cons.advance().map_err(err)?;
    }
    Ok(handled)
}
