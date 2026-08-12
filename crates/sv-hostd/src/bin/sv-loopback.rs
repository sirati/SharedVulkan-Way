//! `sv-loopback` — the two ends, as two processes, over one memfd.
//!
//! ```text
//! sv-loopback gpu  --socket <path>     # the owner: publishes, consumes, replies
//! sv-loopback app  --socket <path>     # the peer: attaches, allocates, submits
//! ```
//!
//! This is design.md §13 step 1 and step 2 executed for real:
//!
//! > **1. `sv-wire` + `sv-transport` over a memfd, two Linux processes, no
//! > Vulkan.** Handshake, magic-written-last, generation counter, ring
//! > ping-pong, reply ring, reverse ring, heartbeat timeout, peer-restart
//! > detection.
//!
//! and it is simultaneously the **Linux-host/Linux-guest topology**, not a
//! stand-in for it. The two processes share nothing but a Unix socket and the
//! descriptor that crosses it, so they run unchanged in two containers.
//!
//! # What it proves that a unit test cannot
//!
//! The app side allocates from the arena, writes a pattern *through its own
//! mapping*, and sends only `(offset, len)`. The GPU side reads those bytes
//! *through its own, differently-based mapping* and checksums them. If the
//! offsets meant different things on the two sides — §1's invariant — the
//! checksum would differ. **No payload crosses the ring**, which is the
//! requirement document's second invariant stated as a program.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sv-loopback: Linux-only (memfd and SCM_RIGHTS)");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => {
            eprintln!("sv-loopback: ok");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sv-loopback: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Errors cross thread boundaries as strings: a worker's failure has to be
/// reportable by the thread that joins it, and `Box<dyn Error>` is not `Send`.
#[cfg(target_os = "linux")]
fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[cfg(target_os = "linux")]
mod demo {
    //! The tiny protocol the loopback speaks. Not Vulkan: the point is the
    //! transport, and a real opcode table arrives with `sv-proto`.

    use sv_wire::{CommandHeader, Decoder, Encoder, Result as WireResult};

    pub const OP_VERIFY_BLOB: u32 = 1;
    pub const OP_DEBUG_MESSAGE: u32 = 2;

    /// "Checksum `len` bytes at arena offset `off` and tell me what you get."
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VerifyBlob {
        pub off: u64,
        pub len: u64,
        pub expect: u64,
    }

    impl VerifyBlob {
        pub const SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 24;

        pub fn encode(&self, out: &mut Vec<u8>) -> WireResult<()> {
            out.clear();
            out.resize(Self::SIZE, 0);
            let mut e = Encoder::new(out);
            CommandHeader {
                cmd_type: OP_VERIFY_BLOB,
                cmd_flags: sv_wire::CMD_FLAG_GENERATE_REPLY,
                cmd_size: Self::SIZE as u32,
            }
            .encode(&mut e)?;
            e.u64(self.off)?;
            e.u64(self.len)?;
            e.u64(self.expect)?;
            e.finish()?;
            Ok(())
        }

        pub fn decode(bytes: &[u8]) -> WireResult<VerifyBlob> {
            let mut d = Decoder::new(bytes);
            let h = CommandHeader::decode(&mut d)?;
            // Deviation 1 in use: the length in the header is checked against
            // what decoding actually consumed, so a skewed build fails here
            // rather than three commands later.
            if h.cmd_size as usize != Self::SIZE {
                return Err(sv_wire::WireError::CommandLengthMismatch {
                    declared: h.cmd_size,
                    consumed: Self::SIZE,
                });
            }
            Ok(VerifyBlob { off: d.u64()?, len: d.u64()?, expect: d.u64()? })
        }
    }

    /// A reverse-ring debug message. The reverse ring is a **command** lane in
    /// the GPU→app direction, so it carries a `CommandHeader`.
    pub fn encode_small(out: &mut Vec<u8>, op: u32, a: u64, b: u64) -> WireResult<()> {
        let size = sv_wire::COMMAND_HEADER_SIZE + 16;
        out.clear();
        out.resize(size, 0);
        let mut e = Encoder::new(out);
        CommandHeader { cmd_type: op, cmd_flags: 0, cmd_size: size as u32 }.encode(&mut e)?;
        e.u64(a)?;
        e.u64(b)?;
        e.finish()?;
        Ok(())
    }

    pub fn decode_small(bytes: &[u8]) -> WireResult<(u32, u64, u64)> {
        let mut d = Decoder::new(bytes);
        let h = CommandHeader::decode(&mut d)?;
        Ok((h.cmd_type, d.u64()?, d.u64()?))
    }

    /// A reply, which is **not** the same shape.
    ///
    /// This originally reused `encode_small` and put a `CommandHeader` on the
    /// reply lane. It worked, because the only reader was this file — and it
    /// hid a live bug in `RingConsumer`, which read every payload's declared
    /// length from the command header's offset. A real `sv_wire::ReplyHeader` is
    /// `{cmd_type, reply_size}` and puts its length four bytes earlier, so the
    /// consumer read a reply's first payload field as a length; the first
    /// forwarded `vkCreateInstance` came back declaring zero bytes. The
    /// transport is fixed and so is this, and the two had to be fixed together
    /// because each was covering for the other.
    pub fn encode_reply_small(out: &mut Vec<u8>, op: u32, a: u64, b: u64) -> WireResult<()> {
        let size = sv_wire::REPLY_HEADER_SIZE + 16;
        out.clear();
        out.resize(size, 0);
        let mut e = Encoder::new(out);
        sv_wire::ReplyHeader { cmd_type: op, reply_size: size as u32 }.encode(&mut e)?;
        e.u64(a)?;
        e.u64(b)?;
        e.finish()?;
        Ok(())
    }

    pub fn decode_reply_small(bytes: &[u8]) -> WireResult<(u32, u64, u64)> {
        let mut d = Decoder::new(bytes);
        let h = sv_wire::ReplyHeader::decode(&mut d)?;
        Ok((h.cmd_type, d.u64()?, d.u64()?))
    }

    /// A cheap, order-sensitive checksum. Order-sensitive on purpose: a
    /// translation that got the pages right but in the wrong order — R21's exact
    /// failure — has to be detectable, and a sum of bytes would not detect it.
    pub fn checksum(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let role = args.get(1).cloned().unwrap_or_default();
    let socket = arg("--socket").ok_or("--socket is required")?;
    match role.as_str() {
        "gpu" => {
            // How many command packets each ring should expect. Defaults match
            // what the app side sends; the chaos test raises the device count so
            // that the GPU side is left waiting for a packet that will never
            // come, which is R4's case made deterministic rather than timed.
            let d: usize = arg("--expect-device").map_or(Ok(2), |s| s.parse())?;
            let q: usize = arg("--expect-queue").map_or(Ok(1), |s| s.parse())?;
            gpu_side(&socket, d, q)
        }
        "app" => app_side(&socket, !args.iter().any(|a| a == "--no-wait")),
        _ => {
            eprintln!(
                "usage: sv-loopback <gpu|app> --socket <path> [--expect-device N] [--expect-queue N]"
            );
            Err("bad role".into())
        }
    }
}

/// Connect to `sv-hostd` and get the descriptor and the view geometry.
#[cfg(target_os = "linux")]
fn attach(
    socket: &str,
) -> Result<
    (std::os::fd::OwnedFd, sv_transport::cold::Attachment),
    Box<dyn std::error::Error>,
> {
    use std::os::unix::net::UnixStream;
    use sv_transport::cold::{ColdMessage, recv, send};

    // The daemon may not be listening yet; a bounded retry rather than an
    // unbounded one, because R4 applies to startup too.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let sock = loop {
        match UnixStream::connect(socket) {
            Ok(s) => break s,
            Err(e) if std::time::Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(Box::new(e)),
        }
    };
    send(&sock, &ColdMessage::AttachRequest)?;
    let (msg, fd) = recv(&sock)?;
    match msg {
        ColdMessage::Attach(a) => {
            Ok((fd.ok_or("sv-hostd sent no descriptor")?, a))
        }
        ColdMessage::Refuse { code, reason } => Err(format!("refused {code}: {reason}").into()),
        other => Err(format!("unexpected {other:?}").into()),
    }
}

/// The owner: publishes the control region, consumes commands, replies, and
/// sends one reverse-ring message.
#[cfg(target_os = "linux")]
fn gpu_side(
    socket: &str,
    expect_device: usize,
    expect_queue: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use sv_arena::canary::{self, CanaryPolicy};
    use sv_hostd::{demo_geometry, demo_ids, map_views};
    use sv_transport::control::{Control, REVERSE_RING_SLOT, RingKind, RingStatus};
    use sv_transport::park::{Deadline, SpinYield, wait_until};
    use sv_transport::ring::{Lane, RingConsumer, RingCtrl, RingProducer, barrier_satisfied};

    let (fd, at) = attach(socket)?;
    let (control_region, arena) = map_views(fd.as_fd(), &at)?;
    let g = demo_geometry();
    let control = Control::create(control_region, g, demo_ids(), at.generation)?;
    control.stamp_epoch()?;
    eprintln!("[gpu] published generation {}", control.generation());

    // R26: the canary goes down after the extent is located and before the app
    // side allocates anything. On this path the extent is a 4 KiB-granular
    // memfd, so the canary is required.
    let policy = CanaryPolicy::PerSlab { run_id: 0xC0FFEE };
    let n = canary::write_all(&arena, policy, g.arena_size, g.arena_slab_size)?;
    eprintln!("[gpu] wrote {n} canaries");

    // The app side claims its rings once it has attached; wait for the device
    // ring to appear rather than assuming an order.
    let deadline = Deadline::after(std::time::Duration::from_secs(10));
    let mut device = None;
    while std::time::Instant::now() < deadline.instant() {
        for slot in 0..g.ring_slot_count {
            if control.ring_state(slot)? == sv_transport::control::RingState::Live
                && control.ring_kind(slot)? == RingKind::Device
            {
                device = Some(slot);
            }
        }
        if device.is_some() {
            break;
        }
        control.beat(1, 0)?;
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let device = device.ok_or("the app side never claimed a device ring")?;
    let queue = (0..g.ring_slot_count)
        .find(|s| control.ring_kind(*s).map(|k| k == RingKind::Queue).unwrap_or(false))
        .ok_or("no queue ring")?;
    eprintln!("[gpu] device ring {device}, queue ring {queue}");

    let mut reverse = RingProducer::open(&control, REVERSE_RING_SLOT, Lane::Command)?;
    for slot in [device, queue] {
        RingCtrl::open(&control, slot)?.set_status(RingStatus::Alive)?;
    }

    // ONE THREAD PER RING, which is design.md §3's topology ("ring consumers,
    // 1 thread/ring, spin-then-park") and is **required** rather than merely
    // tidy. A single thread draining both rings deadlocks the first time a
    // queue packet carries a barrier: it blocks waiting for a device-ring
    // seqno that only it could have advanced. That is not a hypothetical — it
    // is what this harness did before this comment existed, and it hung.
    let overall = Deadline::after(std::time::Duration::from_secs(20));
    let control = std::sync::Arc::new(control);
    let mut workers = Vec::new();
    for (which, slot, expect) in
        [("device", device, expect_device), ("queue", queue, expect_queue)]
    {
        let control = std::sync::Arc::clone(&control);
        let arena = std::sync::Arc::clone(&arena);
        workers.push(std::thread::spawn(move || -> Result<usize, String> {
            let mut cons = RingConsumer::open(&control, slot, Lane::Command).map_err(err)?;
            let mut rep = RingProducer::open(&control, slot, Lane::Reply).map_err(err)?;
            let ctrl = RingCtrl::open(&control, slot).map_err(err)?;
            let parker = SpinYield::default();
            let mut out = Vec::new();
            let mut handled = 0usize;

            while handled < expect {
                // Wait for work, beating while blocked, with a deadline (R4).
                wait_until(
                    &control,
                    &ctrl,
                    Some(Lane::Command),
                    &parker,
                    &|| !cons.is_empty().unwrap_or(false),
                    overall,
                    std::time::Duration::from_millis(100),
                )
                .map_err(err)?;

                // `next` and `advance` are two calls precisely so a barrier can
                // be honoured between them: the packet must not be decoded
                // until the ring it names has caught up (R22).
                let Some((seqno, barrier, payload)) = ({
                    cons.next().map_err(err)?.map(|p| (p.seqno, p.barrier, p.payload.to_vec()))
                }) else {
                    continue;
                };

                if let Some(b) = barrier {
                    wait_until(
                        &control,
                        &ctrl,
                        None,
                        &parker,
                        &|| barrier_satisfied(&control, b).unwrap_or(false),
                        overall,
                        std::time::Duration::from_millis(100),
                    )
                    .map_err(err)?;
                    eprintln!(
                        "[gpu] {which} seqno {seqno}: barrier on ring {} seqno {} satisfied",
                        b.ring, b.seqno
                    );
                }

                let cmd = demo::VerifyBlob::decode(&payload).map_err(err)?;
                // Read the app side's bytes through *our* mapping of the arena.
                // The ring carried 24 bytes; the data never left the arena.
                let mut bytes = vec![0u8; cmd.len as usize];
                arena.read_bytes(cmd.off, &mut bytes).map_err(err)?;
                let got = demo::checksum(&bytes);
                if got != cmd.expect {
                    return Err(format!(
                        "{which} seqno {seqno}: checksum {got} against the app side's {}",
                        cmd.expect
                    ));
                }
                eprintln!(
                    "[gpu] {which} seqno {seqno}: {} bytes at arena+{}, checksum matches",
                    cmd.len, cmd.off
                );
                demo::encode_reply_small(&mut out, demo::OP_VERIFY_BLOB, got, cmd.expect)
                    .map_err(err)?;
                rep.push_seq(seqno, &out, None).map_err(err)?;
                cons.advance().map_err(err)?;
                handled += 1;
            }
            Ok(handled)
        }));
    }

    let mut handled = 0usize;
    for w in workers {
        handled += w.join().map_err(|_| "a ring consumer panicked")??;
    }
    // The canary, checked after a session's worth of allocation rather than
    // only before it — which is the half of R26 that catches a frame moving
    // *during* use.
    canary::verify_all(&arena, policy, g.arena_size, g.arena_slab_size)?;
    let mut out = Vec::new();

    // design.md §4.6: the reverse ring, carrying what a validation layer would.
    demo::encode_small(&mut out, demo::OP_DEBUG_MESSAGE, handled as u64, 0)?;
    reverse.push(&out, None)?;
    eprintln!("[gpu] handled {handled} commands, sent a reverse-ring message");

    // Keep beating while the app side finishes reading, then say goodbye. The
    // budget is generous because a heartbeat that stops early is indistinguish-
    // able from a peer that died, and on a loaded machine that is a flaky test
    // rather than a bug.
    for _ in 0..1200 {
        control.beat_now()?;
        if control.flags()? & sv_transport::control::Flags::SHUTDOWN != 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let checked = canary::verify_all(&arena, policy, g.arena_size, g.arena_slab_size)?;
    eprintln!("[gpu] {checked} canaries intact at shutdown");
    Ok(())
}

/// The peer: attaches, allocates from the arena, writes patterns, submits.
#[cfg(target_os = "linux")]
fn app_side(socket: &str, wait_for_replies: bool) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use sv_arena::alloc::ArenaAllocator;
    use sv_arena::canary::CanaryPolicy;
    use sv_hostd::{demo_geometry, demo_ids, map_views};
    use sv_transport::control::{Control, Flags, REVERSE_RING_SLOT, RingKind};
    use sv_transport::park::{Deadline, SpinYield, wait_until};
    use sv_transport::ring::{BarrierTracker, Lane, RingConsumer, RingCtrl, RingProducer};

    let (fd, at) = attach(socket)?;
    let (control_region, arena) = map_views(fd.as_fd(), &at)?;
    let g = demo_geometry();

    // The owner may not have published yet. `NotReady` is the expected answer
    // and retrying it is the whole reason `magic` is written last.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let control = loop {
        match Control::attach(std::sync::Arc::clone(&control_region), demo_ids()) {
            Ok(c) => break c,
            Err(sv_transport::Error::NotReady) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(Box::new(e)),
        }
    };
    control.stamp_epoch()?;
    eprintln!("[app] attached, generation {}", control.generation());

    let device = control.claim_ring(RingKind::Device, 1, None)?;
    let queue = control.claim_ring(RingKind::Queue, 2, Some(device))?;
    eprintln!("[app] claimed device ring {device}, queue ring {queue}");

    let mut dev_p = RingProducer::open(&control, device, Lane::Command)?;
    let mut dev_rc = RingConsumer::open(&control, device, Lane::Reply)?;
    let mut q_p = RingProducer::open(&control, queue, Lane::Command)?;
    let mut q_rc = RingConsumer::open(&control, queue, Lane::Reply)?;
    let mut reverse = RingConsumer::open(&control, REVERSE_RING_SLOT, Lane::Command)?;

    // The allocator's canary policy must match what the GPU side laid down, or
    // the allocator would hand out the canary pages.
    let mut alloc = ArenaAllocator::new(
        g.arena_size,
        g.arena_slab_size,
        CanaryPolicy::PerSlab { run_id: 0xC0FFEE },
    )?;
    eprintln!(
        "[app] arena {} MiB, {} slabs, {} bytes reserved for canaries",
        g.arena_size >> 20,
        alloc.slab_count(),
        alloc.reserved_bytes()
    );

    let parker = SpinYield::default();
    let mut tracker = BarrierTracker::new(device);
    let mut buf = Vec::new();
    let mut expected = Vec::new();

    // Two packets on the device ring, then one on the queue ring that must be
    // ordered after them — which is what the barrier is for.
    for i in 0..3u32 {
        let size = 4096 * (i as u64 + 1);
        let a = alloc.alloc(size)?;
        let off = alloc.arena_offset(a)?;

        // Write the pattern through *our* mapping. Nothing of this crosses.
        let pattern: Vec<u8> = (0..size).map(|k| (k.wrapping_mul(31) ^ u64::from(i)) as u8).collect();
        arena.write_bytes(off, &pattern)?;
        let sum = demo::checksum(&pattern);

        demo::VerifyBlob { off, len: size, expect: sum }.encode(&mut buf)?;
        if i < 2 {
            let seqno = dev_p.push(&buf, None)?;
            tracker.note_produced(seqno);
            eprintln!("[app] device seqno {seqno}: {size} bytes at arena+{off}");
        } else {
            let barrier = tracker.take();
            let seqno = q_p.push(&buf, barrier)?;
            eprintln!("[app] queue seqno {seqno}: {size} bytes at arena+{off}, barrier {barrier:?}");
            assert!(barrier.is_some(), "a queue packet after two device packets needs a barrier");
        }
        expected.push(sum);
    }

    // `--no-wait`: send and leave. Used by the chaos test to produce a peer that
    // genuinely goes away, rather than one that is merely slow — the two look
    // identical from the other side until the heartbeat stops, which is exactly
    // the distinction R4 asks the transport to make.
    if !wait_for_replies {
        eprintln!("[app] sent {} commands, leaving without waiting", expected.len());
        return Ok(());
    }

    // Collect the three replies.
    let overall = Deadline::after(std::time::Duration::from_secs(20));
    let dev_ctrl = RingCtrl::open(&control, device)?;
    let q_ctrl = RingCtrl::open(&control, queue)?;
    let mut got = 0usize;
    while got < 3 {
        // The overall deadline is checked *here*, by the loop that owns it.
        // Swallowing `TimedOut` from `wait_until` without this turns a missing
        // reply into a busy spin that never ends — which is R4's denial of
        // service arriving by our own hand rather than the peer's.
        if std::time::Instant::now() > overall.instant() {
            return Err(format!("only {got} of 3 replies arrived").into());
        }
        control.beat_now()?;
        for (cons, ctrl) in [(&mut dev_rc, &dev_ctrl), (&mut q_rc, &q_ctrl)] {
            // Stop as soon as every reply is in. Without this the loop waits on
            // a lane that will never deliver again, and `wait_until` correctly
            // reports the GPU side as gone once it has finished and stopped
            // beating — a true answer to a question that should not have been
            // asked.
            if got >= 3 {
                break;
            }
            // A short slice per lane, so neither lane starves the other. The
            // long deadline belongs to the loop, not to one wait inside it.
            let slice = Deadline::after(std::time::Duration::from_millis(50));
            match wait_until(
                &control,
                ctrl,
                Some(Lane::Reply),
                &parker,
                &|| !cons.is_empty().unwrap_or(false),
                slice,
                std::time::Duration::from_millis(25),
            ) {
                Ok(()) => {}
                // A lane with nothing on it yet is not an error; the other lane
                // may still be delivering.
                Err(sv_transport::Error::TimedOut { .. }) => continue,
                Err(e) => return Err(Box::new(e)),
            }
            let payload = match cons.next()? {
                Some(p) => p.payload.to_vec(),
                None => continue,
            };
            let (op, computed, claimed) = demo::decode_reply_small(&payload)?;
            cons.advance()?;
            if op != demo::OP_VERIFY_BLOB || computed != claimed {
                return Err(format!(
                    "the GPU side read different bytes: it computed {computed}, we sent {claimed}"
                )
                .into());
            }
            if !expected.contains(&computed) {
                return Err("a reply that matches no command we sent".into());
            }
            got += 1;
            eprintln!("[app] reply {got}/3 verified");
        }
    }

    // The reverse ring, which design.md §4.6 says is built from day one.
    wait_until(
        &control,
        &RingCtrl::open(&control, REVERSE_RING_SLOT)?,
        Some(Lane::Command),
        &parker,
        &|| !reverse.is_empty().unwrap_or(false),
        overall,
        std::time::Duration::from_millis(200),
    )?;
    let msg = reverse.next()?.ok_or("no reverse-ring message")?;
    let (op, n, _) = demo::decode_small(msg.payload)?;
    reverse.advance()?;
    if op != demo::OP_DEBUG_MESSAGE {
        return Err("the reverse ring carried the wrong opcode".into());
    }
    eprintln!("[app] reverse ring: the GPU side handled {n} commands");

    control.set_flag(Flags::SHUTDOWN)?;
    eprintln!("[app] all three blobs verified with zero payload bytes on the ring");
    Ok(())
}
