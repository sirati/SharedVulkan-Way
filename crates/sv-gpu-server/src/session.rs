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
use sv_transport::park::{Deadline, Futex, wait_until};
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

/// The `VkResult` inside an encoded reply, if it has one.
///
/// Read out of the bytes rather than threaded back through `dispatch`, which
/// keeps this entirely out of the generated code. Two callers want it: `SV_TRACE`
/// and the check that a fire-and-forget command's failure is not silence.
fn result_of(op: sv_proto::Opcode, reply: &[u8]) -> Option<i32> {
    if !op.returns_result() {
        return None;
    }
    reply
        .get(sv_wire::REPLY_HEADER_SIZE..sv_wire::REPLY_HEADER_SIZE + 4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// design.md §6.3's non-Vulkan commands, on the GPU side.
///
/// Returns how many Vulkan commands it executed, so the session's count means
/// the same thing whether a `vkCmd*` crossed on its own or inside a blob.
///
/// # The copy, and why §6.5's "genuinely zero-copy" is not achievable here
///
/// §6.5 says of the blob:
///
/// > With the arena this is genuinely zero-copy from record to execute: the
/// > bytes the application's `vkCmdDraw` produced are the bytes the decoder
/// > reads.
///
/// The decoder reads a `&[u8]`, and a `&[u8]` over memory another process can
/// write is undefined behaviour in this language whatever the protocol says —
/// `sv_transport::region`'s invariant I3, and the same rule `RingProducer`
/// already accepts a copy to obey. So the blob is read *out* of the arena into
/// a reused buffer and decoded from there.
///
/// That is not merely a language concession. design.md §5.5 already draws the
/// distinction that settles it:
///
/// > a value the GPU side reads *out* of the arena into a local variable is
/// > snapshotted by the act of reading, so checking it is an ordinary check and
/// > worth doing
///
/// A command blob is exactly such a value: every bounds check the sub-decoder
/// makes is meaningless against bytes the peer can still change underneath it.
/// **The snapshot is what makes the restricted sub-decoder mean anything**, and
/// §6.5's zero-copy sentence is in tension with §5.5's own rule. What the blob
/// actually buys is not the absence of a copy but the absence of *framing*: one
/// packet, one wake-up and one contiguous `memcpy` for a whole command buffer,
/// against one of each per `vkCmd*`.
fn protocol_command(
    payload: &[u8],
    srv: &mut sv_gpu_entry::Forward<GpuServer>,
    blob: &mut Vec<u8>,
    reply: &mut Vec<u8>,
) -> Result<u64, String> {
    match sv_proto::sv::peek_opcode(payload) {
        Some(sv_proto::sv::SV_EXECUTE_COMMAND_BLOB) => {
            let d = sv_proto::sv::ExecuteCommandBlob::decode(payload).map_err(err)?;
            let arena = Arc::clone(&srv.host().shared.arena.region);
            let end = d.arena_offset.checked_add(d.len).ok_or("a blob whose range overflows")?;
            if end > arena.len() as u64 || d.len == 0 {
                return Err(format!(
                    "a blob at {}+{} is outside the {}-byte arena",
                    d.arena_offset,
                    d.len,
                    arena.len()
                ));
            }
            blob.clear();
            blob.resize(d.len as usize, 0);
            arena.read_bytes(d.arena_offset, blob).map_err(err)?;
            let mut dec = sv_wire::Decoder::new(blob);
            let n = sv_proto::subdecode(&mut dec, srv).map_err(|e| {
                match srv.host().last_error.take() {
                    Some(detail) => format!("in a command blob: {e} ({detail})"),
                    None => format!("in a command blob: {e}"),
                }
            })?;
            Ok(n as u64)
        }
        Some(sv_proto::sv::SV_IMPORT_ARENA_SLAB) => {
            let d = sv_proto::sv::ImportArenaSlab::decode(payload).map_err(err)?;
            let shared = Arc::clone(&srv.host().shared);
            // Already covered? Then this is the GPU-allocated provider, where
            // the arena *is* a `vkAllocateMemory` this process made before the
            // app side existed and there is nothing to import. Answering rather
            // than refusing keeps the app side's rule uniform — "ask before you
            // use a slab" — with no flag anywhere saying which provider is in
            // force, and it costs one round trip per slab once per session.
            //
            // Doing it here rather than by pre-stamping the slab directory at
            // startup also removes a race: the app side can allocate as soon as
            // it has claimed its rings, which is before `serve` would have got
            // to any stamping.
            if shared.objects().arena_slab(d.arena_offset).is_some() {
                sv_proto::sv::ImportArenaSlabReply {
                    result: 0,
                    memory_type: shared.arena.memory_type,
                }
                .encode(reply)
                .map_err(err)?;
                return Ok(0);
            }
            // A failed import is design.md §5.2's "hard error surfaced as
            // `VK_ERROR_OUT_OF_DEVICE_MEMORY` from the triggering
            // `vkAllocateMemory`" — so it is a *reply*, not a fatal ring. The
            // app side is waiting for this answer and can act on it.
            let (result, memory_type) = match crate::arena::import_slab(
                &shared.driver,
                &shared.arena.region,
                d.arena_offset,
                d.len,
            ) {
                Ok((memory, ty)) => {
                    let first = {
                        let mut o = shared.objects();
                        let first = !o.any_slab_imported();
                        o.set_slab(d.slab, memory);
                        first
                    };
                    if first {
                        // §0.5 item 3's startup check, moved to the first moment
                        // the arena's memory type exists. See
                        // `arena::check_external_buffers`.
                        if let Err(e) = crate::arena::check_external_buffers(
                            &shared.driver,
                            shared.arena.external_handle_type,
                            ty,
                        ) {
                            return Err(format!("{e}"));
                        }
                    }
                    (0, ty)
                }
                Err(e) => {
                    eprintln!("[gpu] slab {} import failed: {e}", d.slab);
                    // `VK_ERROR_OUT_OF_DEVICE_MEMORY`.
                    (-2, 0)
                }
            };
            sv_proto::sv::ImportArenaSlabReply { result, memory_type }
                .encode(reply)
                .map_err(err)?;
            Ok(0)
        }
        Some(other) => Err(format!("unknown protocol command {other:#x}")),
        None => Err("a packet too short to carry an opcode".to_string()),
    }
}

/// One ring, until the app side shuts down or stops talking.
fn drain(control: Arc<Control>, shared: Arc<Shared>, slot: u32) -> Result<u64, String> {
    let mut cons = RingConsumer::open(&control, slot, Lane::Command).map_err(err)?;
    let mut replies = RingProducer::open(&control, slot, Lane::Reply).map_err(err)?;
    let ctrl = RingCtrl::open(&control, slot).map_err(err)?;
    // design.md §4.5 implementation 2. A ring thread that spun here would cost a
    // core per ring for as long as the session lives; one that slept cost 59.6 µs
    // of Linux timer slack on every command that arrived after a quiet moment,
    // which is most of them in any workload that uses more than one ring.
    let parker = Futex::default();
    // `Forward` is the generated half: 221 of the 239 commands are answered
    // inside it, and `GpuServer` supplies only the eighteen whose meaning is
    // ours (design.md §7.2 pass 12).
    let mut srv = sv_gpu_entry::Forward::new(GpuServer::new(Arc::clone(&shared)));
    let mut handled = 0u64;
    let mut reply = Vec::new();
    // The blob is copied out of the arena before it is decoded, into a buffer
    // that is reused for the life of the ring, so a steady workload allocates
    // nothing here after its first frame. See `protocol_command` for why the
    // copy is not the thing design.md §6.5 was trying to avoid.
    let mut blob = Vec::new();
    let mut idle_since = Instant::now();
    let trace = std::env::var_os("SV_TRACE").is_some();

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
            Some(Lane::Command),
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
                None,
                &parker,
                &|| barrier_satisfied(&control, b).unwrap_or(false),
                Deadline::after(Duration::from_secs(10)),
                Duration::from_millis(50),
            )
            .map_err(|e| format!("ring {slot} seqno {seqno}: barrier on ring {}: {e}", b.ring))?;
        }

        reply.clear();

        // design.md §6.3's commands are ours rather than Vulkan's and are
        // routed before the generated dispatcher ever sees them — `Opcode` has
        // no variant for one, and that is the point: `sv_proto::dispatch` is
        // the *Vulkan* dispatcher and one of these arriving there would be a
        // bug rather than a protocol.
        if sv_proto::sv::peek_opcode(&payload).is_some_and(sv_proto::sv::is_protocol) {
            match protocol_command(&payload, &mut srv, &mut blob, &mut reply) {
                Ok(n) => handled += n,
                Err(detail) => {
                    let _ = ctrl.set_fatal(1, seqno, &detail);
                    eprintln!("[gpu] ring {slot} seqno {seqno} FATAL: {detail}");
                    return Err(format!("ring {slot} seqno {seqno}: {detail}"));
                }
            }
            if !reply.is_empty() {
                replies.push_seq(seqno, &reply, None).map_err(err)?;
            }
            cons.advance().map_err(err)?;
            continue;
        }

        let mut dec = sv_wire::Decoder::new(&payload);
        match sv_proto::dispatch(&mut dec, &mut srv, &mut reply) {
            Ok(op) => {
                // `SV_TRACE=1` prints every command and the `VkResult` its reply
                // carries. It exists because the failure mode of a forwarder is
                // "the application says something failed" with no way to tell
                // *which* of two hundred commands, and reconstructing that from
                // an application's own error message is guesswork. The result is
                // read out of the encoded reply rather than threaded back
                // through `dispatch`, which keeps the tracing entirely out of
                // the generated code.
                if trace {
                    match result_of(op, &reply) {
                        Some(0) | None => eprintln!("[trace] {}", op.name()),
                        Some(c) => eprintln!("[trace] {} -> VkResult({c})", op.name()),
                    }
                }
                // A reply is produced for every command that has one; whether it
                // is *sent* is the caller's business.
                if sv_proto::sv::wants_reply(&payload) {
                    if !reply.is_empty() {
                        replies.push_seq(seqno, &reply, None).map_err(err)?;
                    }
                } else if let Some(code) = result_of(op, &reply)
                    && code != 0
                {
                    // Nobody is waiting for this answer, so a failure has
                    // nowhere to go but here. design.md D15 buys submit latency
                    // by not waiting for the result; the price is that a
                    // submission the driver refuses is a session-level failure
                    // rather than a `VkResult` the application can handle, and
                    // the one thing that must not happen is for it to be
                    // neither.
                    let detail = format!("{} answered VkResult({code}), unasked", op.name());
                    let _ = ctrl.set_fatal(1, seqno, &detail);
                    eprintln!("[gpu] ring {slot} seqno {seqno} FATAL: {detail}");
                    return Err(format!("ring {slot} seqno {seqno}: {detail}"));
                }
                handled += 1;
            }
            Err(e) => {
                // R13's detector firing, or a handle that has not arrived. Both
                // are fatal to the ring and both must say which command, because
                // "the stream desynchronised" without an opcode is undebuggable.
                let detail = match srv.host().last_error.take() {
                    Some(d) => format!("{e} ({d})"),
                    None => e.to_string(),
                };
                let _ = ctrl.set_fatal(1, seqno, &detail);
                // Said here as well as returned, because the return value only
                // reaches a person if the process exits normally — and a ring
                // going fatal usually means the app side is about to be killed,
                // taking this process's exit path with it. The app side sees
                // "ring N is fatal, code 1" and nothing else; this is the line
                // that says which command and why.
                eprintln!("[gpu] ring {slot} seqno {seqno} FATAL: {detail}");
                return Err(format!("ring {slot} seqno {seqno}: {detail}"));
            }
        }
        cons.advance().map_err(err)?;
    }
    Ok(handled)
}
