//! Rings: SPSC, per queue, plus the reply and reverse directions, plus the
//! `RingBarrier` that puts the application's happens-before back together.
//!
//! docs/design.md §4.4, taken from Venus's `vn_ring` and gfxstream's ASG, which
//! converged independently: a single-producer/single-consumer circular buffer,
//! `head` and `tail` on separate cache lines with explicit acquire/release
//! ordering, and a consumer that spins while work arrives and parks only after
//! `idle_timeout_us` with an empty ring.
//!
//! # One implementation, four directions
//!
//! A ring slot owns two byte buffers — the command buffer and the reply buffer —
//! and four counters, two per buffer. So there are four roles: command producer,
//! command consumer, reply producer, reply consumer. All four are
//! [`RingProducer`] and [`RingConsumer`] over different `(data, head, tail)`
//! triples. **The reverse ring of §4.6 is not a fifth thing**: it is a ring in
//! the directory whose command producer happens to be the GPU side. design.md
//! budgets it at "roughly 200 lines"; built this way it is none, and the 200
//! lines are spent instead on the framing checks below.
//!
//! # Framing, and deviation 1
//!
//! > **A `u32` byte length in every command header.** Venus's command length is
//! > implicit in the type, and the spec says plainly that an unrecognised
//! > command cannot be skipped. That is R13 and it is the worst failure mode
//! > this project has: two builds do not fail to connect, they desynchronise and
//! > misinterpret everything downstream. — docs/design.md §6.2
//!
//! `sv-wire`'s [`sv_wire::CommandHeader`] carries that `cmd_size`. This module
//! carries a *second*, independent length in the ring's own packet header, and
//! [`RingConsumer::next`] checks the two against each other. That is not
//! redundancy for its own sake: the framing length is what lets the ring skip a
//! packet whose *contents* it cannot decode, which is the property §6.2 is
//! actually asking for, and two independently-derived lengths that agree is a
//! far stronger statement than one length that is trusted.
//!
//! A packet is:
//!
//! ```text
//! +0  len:u32     total bytes, header included, a multiple of 16
//! +4  kind:u16    COMMAND | PAD
//! +6  flags:u16   HAS_BARRIER
//! +8  seqno:u64
//! [+16 barrier: ring:u32, _pad:u32, seqno:u64]      if HAS_BARRIER
//! [   payload: an sv-wire command stream          ]
//! ```
//!
//! # Records never straddle the wrap
//!
//! Venus copies across the wrap with two memcpys. We do not: when a packet will
//! not fit contiguously, the producer publishes a `PAD` record covering the rest
//! of the buffer and starts again at zero. The cost is a bounded amount of
//! wasted tail — at most one packet's worth, on a ring the design sizes at
//! 64 KiB for descriptors — and what it buys is that **every packet is one
//! contiguous byte range**, so the consumer's snapshot is one copy and the
//! bounds arithmetic has one case instead of three. On a ring that carries
//! descriptors rather than data (§4.4: "the rings stay small — 64 KiB —
//! precisely because the arena carries the data") that is the right trade.
//!
//! # `unsafe`
//!
//! None. This module is safe code over [`crate::region::SharedRegion`], which is
//! the entire justification for that type's atomic-and-copy API (R7).

use crate::control::{
    Control, RC_CMD_HEAD, RC_CMD_PARKED, RC_CMD_TAIL, RC_CMD_WAKE, RC_COMPLETED_SEQNO,
    RC_FATAL_CODE, RC_FATAL_MSG, RC_FATAL_MSG_LEN, RC_FATAL_SEQNO, RC_IDLE_TIMEOUT_US,
    RC_NEXT_SEQNO, RC_NOTIFY_COUNT, RC_REPLY_HEAD, RC_REPLY_PARKED, RC_REPLY_TAIL, RC_REPLY_WAKE,
    RC_STATUS, RingStatus,
};
use crate::region::SharedRegion;
use crate::{Error, Result};
use core::sync::atomic::Ordering;
use std::sync::Arc;

/// Bytes in a ring packet header.
pub const PACKET_HEADER_SIZE: u64 = 16;
/// Bytes in the optional barrier that follows it.
pub const BARRIER_SIZE: u64 = 16;
/// Every packet's length is a multiple of this, which is also the alignment of
/// the buffer positions the consumer reads from.
pub const PACKET_ALIGN: u64 = 16;

const KIND_COMMAND: u16 = 1;
const KIND_PAD: u16 = 2;
const KIND_BYTES: u16 = 3;
const FLAG_HAS_BARRIER: u16 = 1 << 0;

/// Bits 8..12 of `flags`: how many of the packet's trailing bytes are alignment
/// padding rather than payload.
///
/// **Only meaningful on [`PacketKind::Bytes`], and load-bearing there.** Every
/// packet length is rounded up to [`PACKET_ALIGN`], so a payload of 37 bytes is
/// framed in 64 and the consumer would otherwise read 48 — 37 of stream and 11
/// of zeroes. On a command lane that is harmless because the `sv-wire` header
/// declares the true length and the framing check tolerates exactly one
/// alignment unit of slack; on a byte lane there is no such header, and 11
/// injected NUL bytes is a corrupted wayland message stream, silently, on
/// roughly fourteen packets in fifteen.
///
/// Four bits suffice: the padding is `total - (header + payload)` where `total`
/// is the next multiple of 16, so it is always in `0..16`.
const FLAG_PAD_SHIFT: u16 = 8;
const FLAG_PAD_MASK: u16 = 0x0F00;

/// What a packet's payload is, which decides which framing checks apply to it.
///
/// # Why a third kind exists, and why it is not a second ring
///
/// waycompose-design.md §36 moves the Wayland connection off TCP and onto this
/// ring. A Wayland connection is an **opaque byte stream**: the bytes are a
/// wayland-protocol message stream, and nothing in this crate parses them or is
/// allowed to. That collides with exactly one thing above — the deviation-1
/// cross-check in [`RingConsumer::next`], which reads a `u32` at offset 8 of
/// every command-lane payload of at least [`sv_wire::COMMAND_HEADER_SIZE`] (12)
/// bytes and requires it to agree with the ring's own framing length. A Wayland
/// chunk has a `wl_display` object id and an opcode there, and would be rejected
/// as a [`crate::Error::FramingDisagreement`] on almost every packet.
///
/// [`PacketKind::Bytes`] turns off **that one check and nothing else**. The
/// length, the alignment, the wrap/`PAD` rule, the `pos + len` bound, the
/// zero-length guard, the seqno, the barrier field and the doorbell are all
/// still there. The check being skipped is a cross-check *between two encodings
/// of the same length*, and on this kind there is only one encoding, so there is
/// nothing to cross-check — it is not a check being weakened, it is a check
/// whose second input does not exist.
///
/// That distinction is worth being pedantic about because §6.2 calls
/// desynchronisation "the worst failure mode this project has". A `Bytes` ring
/// is not more exposed to it: the ring's own `len` still frames every packet
/// exactly, and a byte stream has no commands to desynchronise *between* — the
/// consumer hands the bytes to a wayland parser that does its own framing and
/// that will itself fail loudly on a short read. What is lost is the ability to
/// skip a single undecodable packet, and a byte stream has no such thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    /// An `sv-wire` command or reply. Cross-checked against its own header.
    Command,
    /// Opaque bytes. Framed, never parsed.
    Bytes,
}

/// A ring, named the only way a handle is allowed to be named: by its index in
/// the directory (R3). There is no pointer here and there never will be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RingId(pub u32);

impl RingId {
    pub fn index(self) -> u32 {
        self.0
    }
}

/// Cross-ring ordering. docs/design.md §4.4:
///
/// > Because rings are independent, an object created on the device ring may not
/// > yet exist on the GPU side when a queue ring references it. The app's own
/// > happens-before does not survive the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingBarrier {
    pub ring: u32,
    pub seqno: u64,
}

/// Which end of a ring slot a producer or consumer is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The command buffer: app→GPU on an ordinary ring, GPU→app on the reverse
    /// ring. Whichever it is, the *producer* is the side that owns the ring.
    Command,
    /// The reply buffer, always the opposite direction to `Command`.
    Reply,
}

/// The three offsets that make a byte ring, resolved once at construction.
#[derive(Debug, Clone, Copy)]
struct Lanes {
    data_off: u64,
    capacity: u64,
    head_off: u64,
    tail_off: u64,
    /// Which lane this is. Kept because the two carry *different headers* —
    /// `CommandHeader` one way and `ReplyHeader` the other — and the framing
    /// cross-check has to read the right one. See `RingConsumer::next`.
    lane: Lane,
}

fn lanes(control: &Control, slot: u32, lane: Lane) -> Result<Lanes> {
    let ((cmd_off, cmd_len), (reply_off, reply_len)) = control.ring_buffers(slot)?;
    let ctrl = control.ring_ctrl_offset(slot)?;
    Ok(match lane {
        Lane::Command => Lanes {
            data_off: cmd_off,
            capacity: u64::from(cmd_len),
            head_off: ctrl + RC_CMD_HEAD,
            tail_off: ctrl + RC_CMD_TAIL,
            lane,
        },
        Lane::Reply => Lanes {
            data_off: reply_off,
            capacity: u64::from(reply_len),
            head_off: ctrl + RC_REPLY_HEAD,
            tail_off: ctrl + RC_REPLY_TAIL,
            lane,
        },
    })
}

/// The status half of a ring control block: who is parked, what went fatal.
///
/// Held by both sides, and separate from the producer and consumer because both
/// of them need it and neither owns it.
#[derive(Clone)]
pub struct RingCtrl {
    region: Arc<SharedRegion>,
    base: u64,
    id: RingId,
    generation: u32,
}

impl RingCtrl {
    pub fn open(control: &Control, slot: u32) -> Result<RingCtrl> {
        Ok(RingCtrl {
            region: Arc::clone(control.region()),
            base: control.ring_ctrl_offset(slot)?,
            id: RingId(slot),
            generation: control.generation(),
        })
    }

    pub fn id(&self) -> RingId {
        self.id
    }

    pub fn status(&self) -> RingStatus {
        RingStatus::from_u32(
            self.region
                .atomic_u32(self.base + RC_STATUS)
                .map(|a| a.load(Ordering::Acquire))
                .unwrap_or(RingStatus::Fatal as u32),
        )
    }

    pub fn set_status(&self, s: RingStatus) -> Result<()> {
        self.region.atomic_u32(self.base + RC_STATUS)?.store(s as u32, Ordering::Release);
        Ok(())
    }

    pub fn idle_timeout_us(&self) -> u32 {
        self.region
            .atomic_u32(self.base + RC_IDLE_TIMEOUT_US)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn set_idle_timeout_us(&self, us: u32) -> Result<()> {
        self.region.atomic_u32(self.base + RC_IDLE_TIMEOUT_US)?.store(us, Ordering::Relaxed);
        Ok(())
    }

    /// Count a notification. docs/design.md §4.4: "the producer checks `IDLE` before
    /// paying for a notification", and this is how one measures whether that
    /// check is earning its keep.
    pub fn note_notify(&self) -> Result<u64> {
        Ok(self.region.atomic_u64(self.base + RC_NOTIFY_COUNT)?.fetch_add(1, Ordering::AcqRel))
    }

    pub fn notify_count(&self) -> u64 {
        self.region
            .atomic_u64(self.base + RC_NOTIFY_COUNT)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// The seqno the consumer has finished. The target of every barrier.
    pub fn completed_seqno(&self) -> Result<u64> {
        Ok(self.region.atomic_u64(self.base + RC_COMPLETED_SEQNO)?.load(Ordering::Acquire))
    }

    /// Record why this ring died, then mark it fatal.
    ///
    /// The message goes down before the status, so a reader that sees `FATAL`
    /// has, by that release/acquire pair, seen the message — which is the whole
    /// value of the field, and is the same ordering rule as `magic`.
    pub fn set_fatal(&self, code: u32, seqno: u64, msg: &str) -> Result<()> {
        let mut buf = [0u8; RC_FATAL_MSG_LEN as usize];
        let bytes = msg.as_bytes();
        let n = bytes.len().min(buf.len() - 1);
        buf[..n].copy_from_slice(&bytes[..n]);
        self.region.write_bytes(self.base + RC_FATAL_MSG, &buf)?;
        self.region.atomic_u64(self.base + RC_FATAL_SEQNO)?.store(seqno, Ordering::Relaxed);
        self.region.atomic_u32(self.base + RC_FATAL_CODE)?.store(code, Ordering::Release);
        self.set_status(RingStatus::Fatal)
    }

    pub fn fatal(&self) -> Result<Option<(u32, u64, String)>> {
        if self.status() != RingStatus::Fatal {
            return Ok(None);
        }
        let code = self.region.atomic_u32(self.base + RC_FATAL_CODE)?.load(Ordering::Acquire);
        let seqno = self.region.atomic_u64(self.base + RC_FATAL_SEQNO)?.load(Ordering::Relaxed);
        let mut buf = [0u8; RC_FATAL_MSG_LEN as usize];
        self.region.read_bytes(self.base + RC_FATAL_MSG, &mut buf)?;
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        Ok(Some((code, seqno, String::from_utf8_lossy(&buf[..end]).into_owned())))
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The doorbell words for one lane: `(wake, parked)`.
    ///
    /// See [`crate::park::Bell`] for the handshake they implement and
    /// `control.rs` for why there is a pair per lane rather than one `status`
    /// word for the ring.
    pub fn bell(&self, lane: Lane) -> Result<crate::park::Bell<'_>> {
        let (wake_off, parked_off) = match lane {
            Lane::Command => (RC_CMD_WAKE, RC_CMD_PARKED),
            Lane::Reply => (RC_REPLY_WAKE, RC_REPLY_PARKED),
        };
        Ok(crate::park::Bell {
            wake: self.region.atomic_u32(self.base + wake_off)?,
            parked: self.region.atomic_u32(self.base + parked_off)?,
            token: crate::park::lane_token(self.id.0, lane),
        })
    }
}

// ---------------------------------------------------------------------------
// Producer
// ---------------------------------------------------------------------------

/// The producing end of one lane of one ring.
///
/// # Why the payload is staged rather than written in place
///
/// The producer could be handed a `&mut [u8]` over its reserved span and encode
/// straight into the ring, saving a copy. It is not, and the reason is
/// [`crate::region`]'s invariant I3: a `&mut [u8]` over bytes another process
/// can write is undefined behaviour in this language whatever the protocol says,
/// and R7 says this is the one place to be strict rather than clever. So the
/// caller encodes into [`RingProducer::staging`], a private buffer that is
/// reused, and [`RingProducer::commit`] copies it in.
///
/// The cost is one `memcpy` of a *descriptor* — docs/design.md §4.4 is explicit that
/// the rings carry descriptors and the arena carries the data, and §6.5 puts
/// recorded `vkCmd*` streams in arena blobs precisely so they never pass through
/// here. If a measurement ever shows this copy on a profile, the fix is a
/// written aliasing argument and a `&mut [u8]`, not a quiet change.
pub struct RingProducer {
    region: Arc<SharedRegion>,
    lanes: Lanes,
    ctrl: RingCtrl,
    generation_off: u64,
    staging: Vec<u8>,
}

impl RingProducer {
    pub fn open(control: &Control, slot: u32, lane: Lane) -> Result<RingProducer> {
        Ok(RingProducer {
            region: Arc::clone(control.region()),
            lanes: lanes(control, slot, lane)?,
            ctrl: RingCtrl::open(control, slot)?,
            generation_off: crate::control::OFF_GENERATION,
            staging: Vec::new(),
        })
    }

    pub fn id(&self) -> RingId {
        self.ctrl.id
    }

    pub fn ctrl(&self) -> &RingCtrl {
        &self.ctrl
    }

    /// The largest payload this ring can carry. Anything larger belongs in an
    /// arena blob (docs/design.md §6.5) and the error says so.
    pub fn max_payload(&self) -> u64 {
        self.lanes.capacity - PACKET_HEADER_SIZE - BARRIER_SIZE
    }

    /// A reusable encode buffer. Clear it, encode into it, then [`Self::push`].
    pub fn staging(&mut self) -> &mut Vec<u8> {
        &mut self.staging
    }

    fn check_generation(&self) -> Result<()> {
        let now = self.region.atomic_u32(self.generation_off)?.load(Ordering::Acquire);
        if now != self.ctrl.generation {
            return Err(Error::PeerRestarted { expected: self.ctrl.generation, found: now });
        }
        Ok(())
    }

    /// Allocate a sequence number.
    ///
    /// **Sequence numbers start at 1.** Zero is reserved to mean "nothing has
    /// completed", which is what a zeroed `completed_seqno` in a freshly created
    /// control block says. Starting at zero instead would make the very first
    /// barrier — against the very first packet — trivially satisfied before that
    /// packet had run, which is the one case a barrier exists for.
    fn next_seqno(&self) -> Result<u64> {
        Ok(self.region.atomic_u64(self.ctrl.base + RC_NEXT_SEQNO)?.fetch_add(1, Ordering::AcqRel)
            + 1)
    }

    /// Send `payload`, allocating a sequence number for it.
    ///
    /// Returns the seqno, which is what a barrier on another ring will name and
    /// what the reply will carry.
    ///
    /// **The seqno is allocated only once the space is secured.** A
    /// [`Error::RingFull`] therefore leaves no hole in the sequence. A hole
    /// would be harmless to `completed >= barrier.seqno` — barriers are only
    /// ever emitted against seqnos that were really produced — but it would make
    /// every seqno-based diagnostic lie, and `fatal_seqno` naming a command that
    /// was never sent is a bad hour for whoever is reading it.
    pub fn push(&mut self, payload: &[u8], barrier: Option<RingBarrier>) -> Result<u64> {
        let total = self.frame_len(PacketKind::Command, payload, barrier)?;
        let (tail, pos) = self.place(total)?;
        let seqno = self.next_seqno()?;
        self.write_packet(tail, pos, total, seqno, PacketKind::Command, payload, barrier)?;
        Ok(seqno)
    }

    /// Send `payload` as opaque bytes ([`PacketKind::Bytes`]).
    ///
    /// **An empty payload is legal here and nowhere else, and it means end of
    /// stream on this lane.** `push` rejects an empty payload because a
    /// zero-length command is not a command; but a byte stream is precisely the
    /// thing that *has* an end, and the peer closing its half is the ordinary
    /// termination of a Wayland connection rather than an error. Spelling EOF as
    /// a real packet rather than as a directory state change is what keeps it
    /// *ordered against the bytes before it*: a client's last request and its
    /// disconnect arrive in the order it made them, which a `kill_ring` racing
    /// the reader would not guarantee.
    ///
    /// Returns the seqno, as `push` does. Byte streams do not use barriers, so
    /// there is no barrier argument: a barrier orders one ring against another,
    /// and a Wayland connection is ordered only against itself.
    pub fn push_bytes(&mut self, payload: &[u8]) -> Result<u64> {
        let total = self.frame_len(PacketKind::Bytes, payload, None)?;
        let (tail, pos) = self.place(total)?;
        let seqno = self.next_seqno()?;
        self.write_packet(tail, pos, total, seqno, PacketKind::Bytes, payload, None)?;
        Ok(seqno)
    }

    /// Send `payload` under a sequence number the caller chose. This is how a
    /// reply names the command it answers.
    pub fn push_seq(
        &mut self,
        seqno: u64,
        payload: &[u8],
        barrier: Option<RingBarrier>,
    ) -> Result<()> {
        let total = self.frame_len(PacketKind::Command, payload, barrier)?;
        let (tail, pos) = self.place(total)?;
        self.write_packet(tail, pos, total, seqno, PacketKind::Command, payload, barrier)
    }

    /// Validate the request and return the framed length.
    fn frame_len(
        &self,
        kind: PacketKind,
        payload: &[u8],
        barrier: Option<RingBarrier>,
    ) -> Result<u64> {
        self.check_generation()?;
        if payload.is_empty() && kind == PacketKind::Command {
            // A zero-length command is not a command. Rejecting it here rather
            // than framing it is what stops the consumer from being handed a
            // record it can neither decode nor skip. On a `Bytes` lane the same
            // record is decodable and means EOF — see `push_bytes`.
            return Err(Error::BadPacketLength { ring: self.ctrl.id.0, len: 0 });
        }
        let barrier_len = if barrier.is_some() { BARRIER_SIZE } else { 0 };
        let body = PACKET_HEADER_SIZE + barrier_len + payload.len() as u64;
        let total = body.next_multiple_of(PACKET_ALIGN);
        if total > self.lanes.capacity {
            return Err(Error::PacketTooLarge { need: total, capacity: self.lanes.capacity });
        }
        Ok(total)
    }

    /// Find `total` contiguous bytes, padding to the wrap if necessary.
    ///
    /// Returns `(tail, pos)`: the monotonic counter to publish from, and the
    /// buffer position it maps to.
    fn place(&self, total: u64) -> Result<(u64, u64)> {
        let mask = self.lanes.capacity - 1;
        let mut tail = self.region.atomic_u64(self.lanes.tail_off)?.load(Ordering::Relaxed);
        let mut pos = tail & mask;
        let to_end = self.lanes.capacity - pos;

        if total > to_end {
            // Publish a pad covering the rest of the buffer, then start at zero.
            // The pad is a real record: the consumer sees it, skips it and
            // releases the space, which is what stops this from deadlocking when
            // the ring is nearly full. `to_end` is at least one header, because
            // both the capacity and every packet length are multiples of 16.
            let head = self.region.atomic_u64(self.lanes.head_off)?.load(Ordering::Acquire);
            let free = self.lanes.capacity - (tail - head);
            if free < to_end {
                return Err(Error::RingFull { ring: self.ctrl.id.0, need: to_end, free });
            }
            let mut pad = [0u8; PACKET_HEADER_SIZE as usize];
            pad[0..4].copy_from_slice(&(to_end as u32).to_le_bytes());
            pad[4..6].copy_from_slice(&KIND_PAD.to_le_bytes());
            self.region.write_bytes(self.lanes.data_off + pos, &pad)?;
            tail += to_end;
            self.region.atomic_u64(self.lanes.tail_off)?.store(tail, Ordering::Release);
            pos = 0;
        }

        let head = self.region.atomic_u64(self.lanes.head_off)?.load(Ordering::Acquire);
        let free = self.lanes.capacity - (tail - head);
        if total > free {
            return Err(Error::RingFull { ring: self.ctrl.id.0, need: total, free });
        }
        Ok((tail, pos))
    }

    // Eight arguments, and grouping them would be worse: `tail`/`pos`/`total`
    // come from `place`, `seqno` from `next_seqno`, and `kind`/`payload`/
    // `barrier` from the caller. A struct would name that split without making
    // it any harder to pass the wrong `pos`, which is the only mistake here that
    // matters.
    #[allow(clippy::too_many_arguments)]
    fn write_packet(
        &self,
        tail: u64,
        pos: u64,
        total: u64,
        seqno: u64,
        kind: PacketKind,
        payload: &[u8],
        barrier: Option<RingBarrier>,
    ) -> Result<()> {
        // Assemble the record privately, then copy it in as one range. Assembling
        // in place would publish a half-written header to a peer that is not
        // supposed to be looking at it yet — true, and exactly the kind of "it is
        // fine because nobody looks" this crate declines to rely on.
        let mut rec = vec![0u8; total as usize];
        rec[0..4].copy_from_slice(&(total as u32).to_le_bytes());
        let tag = match kind {
            PacketKind::Command => KIND_COMMAND,
            PacketKind::Bytes => KIND_BYTES,
        };
        rec[4..6].copy_from_slice(&tag.to_le_bytes());
        let mut flags = if barrier.is_some() { FLAG_HAS_BARRIER } else { 0 };
        if kind == PacketKind::Bytes {
            let barrier_len = if barrier.is_some() { BARRIER_SIZE } else { 0 };
            let pad = total - PACKET_HEADER_SIZE - barrier_len - payload.len() as u64;
            debug_assert!(pad < PACKET_ALIGN);
            flags |= (pad as u16) << FLAG_PAD_SHIFT;
        }
        rec[6..8].copy_from_slice(&flags.to_le_bytes());
        rec[8..16].copy_from_slice(&seqno.to_le_bytes());
        let mut at = PACKET_HEADER_SIZE as usize;
        if let Some(b) = barrier {
            rec[at..at + 4].copy_from_slice(&b.ring.to_le_bytes());
            rec[at + 8..at + 16].copy_from_slice(&b.seqno.to_le_bytes());
            at += BARRIER_SIZE as usize;
        }
        rec[at..at + payload.len()].copy_from_slice(payload);

        self.region.write_bytes(self.lanes.data_off + pos, &rec)?;
        // The publication. Everything above happens-before this store, so a
        // consumer that acquire-loads the new tail sees a complete record — and
        // a producer that dies before reaching this line has published nothing.
        self.region.atomic_u64(self.lanes.tail_off)?.store(tail + total, Ordering::Release);
        // docs/design.md §4.4: "the producer checks `IDLE` before paying for a
        // notification". `Bell::ring` is that check — an acquire load, and the
        // syscall only when somebody is actually asleep. It has to be *after*
        // the store above or a woken consumer could find nothing.
        self.ctrl.bell(self.lanes.lane)?.ring();
        Ok(())
    }

    /// Bytes free in the lane, right now.
    pub fn free_bytes(&self) -> Result<u64> {
        let tail = self.region.atomic_u64(self.lanes.tail_off)?.load(Ordering::Relaxed);
        let head = self.region.atomic_u64(self.lanes.head_off)?.load(Ordering::Acquire);
        Ok(self.lanes.capacity - (tail - head))
    }

    pub fn capacity(&self) -> u64 {
        self.lanes.capacity
    }
}

// ---------------------------------------------------------------------------
// Consumer
// ---------------------------------------------------------------------------

/// One packet, copied out of the ring into the consumer's private buffer.
///
/// The payload is a snapshot: docs/design.md §5.5's distinction between a value read
/// *out* of shared memory, which the act of reading pins, and one the peer can
/// still change under you.
#[derive(Debug)]
pub struct PacketView<'a> {
    pub seqno: u64,
    pub barrier: Option<RingBarrier>,
    pub payload: &'a [u8],
    /// Which framing contract this packet was written under. A `Bytes` packet
    /// with an empty `payload` is end of stream; a `Command` packet can never be
    /// empty, so the two cases cannot be confused.
    pub kind: PacketKind,
}

/// The consuming end of one lane of one ring.
pub struct RingConsumer {
    region: Arc<SharedRegion>,
    lanes: Lanes,
    ctrl: RingCtrl,
    generation_off: u64,
    buf: Vec<u8>,
    /// The record currently held by [`RingConsumer::next`], which
    /// [`RingConsumer::advance`] releases. `None` means nothing is held.
    pending: Option<Pending>,
    wraps: u64,
    pads: u64,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    head: u64,
    len: u64,
    seqno: u64,
}

impl RingConsumer {
    pub fn open(control: &Control, slot: u32, lane: Lane) -> Result<RingConsumer> {
        Ok(RingConsumer {
            region: Arc::clone(control.region()),
            lanes: lanes(control, slot, lane)?,
            ctrl: RingCtrl::open(control, slot)?,
            generation_off: crate::control::OFF_GENERATION,
            buf: Vec::new(),
            pending: None,
            wraps: 0,
            pads: 0,
        })
    }

    pub fn id(&self) -> RingId {
        self.ctrl.id
    }

    pub fn ctrl(&self) -> &RingCtrl {
        &self.ctrl
    }

    /// How many times the buffer has wrapped, and how many pad records were
    /// skipped. Diagnostics — and the positive control for the wrap test, which
    /// would otherwise pass without ever having wrapped.
    pub fn wrap_stats(&self) -> (u64, u64) {
        (self.wraps, self.pads)
    }

    fn check_generation(&self) -> Result<()> {
        let now = self.region.atomic_u32(self.generation_off)?.load(Ordering::Acquire);
        if now != self.ctrl.generation {
            return Err(Error::PeerRestarted { expected: self.ctrl.generation, found: now });
        }
        Ok(())
    }

    pub fn is_empty(&self) -> Result<bool> {
        let head = self.region.atomic_u64(self.lanes.head_off)?.load(Ordering::Relaxed);
        let tail = self.region.atomic_u64(self.lanes.tail_off)?.load(Ordering::Acquire);
        Ok(head == tail)
    }

    /// Copy out the next packet without releasing its space.
    ///
    /// Two-phase on purpose: a packet carrying a [`RingBarrier`] must not be
    /// decoded until the barrier is satisfied, and the ring must not reclaim the
    /// bytes until the packet has been *processed*. A one-shot `pop` would force
    /// the caller to choose which of those to get wrong.
    // Not `Iterator::next`, and deliberately not renamed to satisfy the lint.
    // An `Iterator` yields until exhaustion and has no way to express "nothing
    // yet, ask again" as distinct from "no more, ever" — which is the entire
    // distinction a ring consumer exists to make. It also could not return the
    // `Result` that every framing check needs, nor pair with `advance` to hold a
    // packet across a barrier wait. `next` is the right name for what this does.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<PacketView<'_>>> {
        self.check_generation()?;
        if self.pending.is_some() {
            return Err(Error::BadPacketLength { ring: self.ctrl.id.0, len: u32::MAX });
        }
        let mask = self.lanes.capacity - 1;
        loop {
            let head = self.region.atomic_u64(self.lanes.head_off)?.load(Ordering::Relaxed);
            let tail = self.region.atomic_u64(self.lanes.tail_off)?.load(Ordering::Acquire);
            if head == tail {
                return Ok(None);
            }
            let inflight = tail.wrapping_sub(head);
            let pos = head & mask;

            let mut hdr = [0u8; PACKET_HEADER_SIZE as usize];
            if inflight < PACKET_HEADER_SIZE {
                return Err(Error::BadPacketLength {
                    ring: self.ctrl.id.0,
                    len: inflight as u32,
                });
            }
            self.region.read_bytes(self.lanes.data_off + pos, &mut hdr)?;
            let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
            let kind = u16::from_le_bytes([hdr[4], hdr[5]]);
            let flags = u16::from_le_bytes([hdr[6], hdr[7]]);
            let seqno = u64::from_le_bytes(hdr[8..16].try_into().unwrap());

            // Framing checks, all of them, before a single byte is trusted. A
            // zero length is called out by name because it is the one that turns
            // a corrupt ring into an infinite loop rather than an error.
            let len64 = u64::from(len);
            if len64 < PACKET_HEADER_SIZE
                || len64 % PACKET_ALIGN != 0
                || len64 > inflight
                || pos + len64 > self.lanes.capacity
            {
                return Err(Error::BadPacketLength { ring: self.ctrl.id.0, len });
            }

            if kind == KIND_PAD {
                // Skip it here rather than handing it up: a pad is framing, not
                // protocol, and nothing above this line should have to know the
                // buffer has an end.
                self.pads += 1;
                self.wraps += 1;
                self.region
                    .atomic_u64(self.lanes.head_off)?
                    .store(head + len64, Ordering::Release);
                continue;
            }
            let packet_kind = match kind {
                KIND_COMMAND => PacketKind::Command,
                KIND_BYTES => PacketKind::Bytes,
                _ => return Err(Error::BadPacketLength { ring: self.ctrl.id.0, len }),
            };

            let mut at = PACKET_HEADER_SIZE;
            let barrier = if flags & FLAG_HAS_BARRIER != 0 {
                if len64 < PACKET_HEADER_SIZE + BARRIER_SIZE {
                    return Err(Error::BadPacketLength { ring: self.ctrl.id.0, len });
                }
                let mut b = [0u8; BARRIER_SIZE as usize];
                self.region.read_bytes(self.lanes.data_off + pos + at, &mut b)?;
                at += BARRIER_SIZE;
                Some(RingBarrier {
                    ring: u32::from_le_bytes(b[0..4].try_into().unwrap()),
                    seqno: u64::from_le_bytes(b[8..16].try_into().unwrap()),
                })
            } else {
                None
            };

            let mut payload_len = len64 - at;
            if packet_kind == PacketKind::Bytes {
                // Strip the alignment padding the producer declared. Checked
                // rather than trusted: a `pad` larger than the packet's own
                // payload would underflow into a huge length, which is the one
                // way a four-bit field could hurt.
                let pad = u64::from((flags & FLAG_PAD_MASK) >> FLAG_PAD_SHIFT);
                if pad >= PACKET_ALIGN || pad > payload_len {
                    return Err(Error::BadPacketLength { ring: self.ctrl.id.0, len });
                }
                payload_len -= pad;
            } else if payload_len == 0 {
                // Zero length is fatal for a command and ordinary for a byte
                // stream, where it is end of stream. See `push_bytes`.
                return Err(Error::BadPacketLength { ring: self.ctrl.id.0, len });
            }
            self.buf.clear();
            self.buf.resize(payload_len as usize, 0);
            self.region.read_bytes(self.lanes.data_off + pos + at, &mut self.buf)?;

            // Deviation 1's second half: the framing length and the sv-wire
            // header's own declared size are two independent statements about
            // the same bytes, and a desynchronised stream is caught here rather
            // than three commands later.
            //
            // This is also where the ring's one-command-per-packet rule is
            // enforced. Batching several commands into one packet would save a
            // header and would cost the seqno its meaning: a barrier names a
            // seqno and a reply answers one, so a packet holding three commands
            // could be neither ordered against nor replied to individually.
            // Commands that genuinely want batching go into an arena blob and
            // travel as one `svExecuteCommandBlob` descriptor (docs/design.md §6.5).
            //
            // **THE SIZE IS AT A DIFFERENT OFFSET ON THE TWO LANES**, and
            // reading it from the command position on a reply lane was a live
            // bug. A `sv_wire::CommandHeader` is `{cmd_type, cmd_flags,
            // cmd_size}` and puts its length at byte 8; a `ReplyHeader` is
            // `{cmd_type, reply_size}` and puts its length at byte 4. Checking a
            // reply at offset 8 reads the reply's *first payload field* as a
            // length — for `vkCreateInstance` that is `VkResult`, so a
            // successful reply declared a length of zero and every forwarded
            // command failed at the first round trip.
            //
            // It was invisible until a real reply crossed: `sv-loopback` builds
            // its replies with a `CommandHeader`, because it predates the
            // generated protocol and had no `ReplyHeader` to use.
            let size_at = match self.lanes.lane {
                Lane::Command => 8,
                Lane::Reply => 4,
            };
            let header_len = match self.lanes.lane {
                Lane::Command => sv_wire::COMMAND_HEADER_SIZE,
                Lane::Reply => sv_wire::REPLY_HEADER_SIZE,
            };
            //
            // Skipped entirely on a `Bytes` lane: see [`PacketKind`]. There is
            // no second encoding of the length there to cross-check against, and
            // reading a wayland object id as a `cmd_size` would reject the
            // stream rather than validate it.
            if packet_kind == PacketKind::Command && self.buf.len() >= header_len {
                let declared =
                    u32::from_le_bytes(self.buf[size_at..size_at + 4].try_into().unwrap());
                // The payload is zero-padded up to the packet alignment, so the
                // framed payload is `declared` rounded up to 16 — never less,
                // and never a whole alignment unit more.
                let framed = payload_len as u32;
                if u64::from(declared) > payload_len
                    || u64::from(declared) + PACKET_ALIGN <= payload_len
                {
                    return Err(Error::FramingDisagreement {
                        ring: self.ctrl.id.0,
                        framed,
                        declared,
                    });
                }
            }

            self.pending = Some(Pending { head, len: len64, seqno });
            return Ok(Some(PacketView { seqno, barrier, payload: &self.buf, kind: packet_kind }));
        }
    }

    /// Release the packet [`RingConsumer::next`] returned and publish its seqno
    /// as completed — which is what every barrier against this ring waits on.
    pub fn advance(&mut self) -> Result<()> {
        let Pending { head, len, seqno } = match self.pending.take() {
            Some(p) => p,
            None => return Ok(()),
        };
        // `completed_seqno` before `head`: a producer that sees the space free
        // must already be able to see the seqno that freed it. The other order
        // would let a barrier fail against a packet that has demonstrably run.
        self.region
            .atomic_u64(self.ctrl.base + RC_COMPLETED_SEQNO)?
            .store(seqno, Ordering::Release);
        self.region.atomic_u64(self.lanes.head_off)?.store(head + len, Ordering::Release);
        Ok(())
    }

    pub fn capacity(&self) -> u64 {
        self.lanes.capacity
    }
}

// ---------------------------------------------------------------------------
// Barriers (R22)
// ---------------------------------------------------------------------------

/// Has the barrier been satisfied?
///
/// One acquire load in the common case, which is docs/design.md §4.4's claim and is
/// worth keeping true: this function must not grow an allocation or a lock.
pub fn barrier_satisfied(control: &Control, b: RingBarrier) -> Result<bool> {
    let ctrl = control.ring_ctrl_offset(b.ring)?;
    let completed = control
        .region()
        .atomic_u64(ctrl + RC_COMPLETED_SEQNO)?
        .load(Ordering::Acquire);
    // Seqnos are allocated by `fetch_add` from zero, and `completed_seqno` holds
    // the seqno of the last packet *finished*. So "seqno n has been processed"
    // is `completed >= n`, and the initial state (nothing completed, seqno 0
    // outstanding) is distinguished by the fact that a barrier is only ever
    // emitted against a seqno that has already been produced.
    Ok(completed >= b.seqno)
}

/// The app-side emission rule, R22.
///
/// > the barrier is emitted conservatively (any packet referencing an id created
/// > on another ring since the last barrier) rather than cleverly
/// > — `security-model.md` R22
///
/// This tracker is deliberately blunter still: it emits a barrier on the *first
/// packet after any creation*, whether or not that packet references the created
/// id. Being cleverer means tracking which ids a packet touches, and the failure
/// mode of getting that wrong is an intermittent "unknown handle" under load —
/// which is the single worst class of bug to ship, because it reproduces on the
/// user's machine and not on ours. The cost of being blunt is one extra acquire
/// load on the GPU side, already satisfied in the common case.
///
/// One tracker per queue ring. The ring it orders against is the queue's
/// `depends_ring` from the directory (docs/design.md §4.2, §4.4), which is why that
/// field exists.
#[derive(Debug, Clone)]
pub struct BarrierTracker {
    depends_ring: u32,
    /// The highest seqno produced on the dependency ring since we last emitted.
    pending: Option<u64>,
    last_emitted: Option<u64>,
    emitted: u64,
    suppressed: u64,
}

impl BarrierTracker {
    pub fn new(depends_ring: u32) -> Self {
        BarrierTracker {
            depends_ring,
            pending: None,
            last_emitted: None,
            emitted: 0,
            suppressed: 0,
        }
    }

    /// Called by the app side whenever it produces a packet on the dependency
    /// ring that creates or mutates an id this ring might name.
    pub fn note_produced(&mut self, seqno: u64) {
        self.pending = Some(match self.pending {
            Some(p) => p.max(seqno),
            None => seqno,
        });
    }

    /// The barrier to attach to the next packet on this queue ring, if any.
    pub fn take(&mut self) -> Option<RingBarrier> {
        let want = self.pending.take()?;
        if self.last_emitted.is_some_and(|l| l >= want) {
            self.suppressed += 1;
            return None;
        }
        self.last_emitted = Some(want);
        self.emitted += 1;
        Some(RingBarrier { ring: self.depends_ring, seqno: want })
    }

    /// Put back a barrier whose packet was not sent after all.
    ///
    /// [`BarrierTracker::take`] both clears `pending` and advances
    /// `last_emitted`, so a caller that took a barrier and then failed to push
    /// cannot simply call `note_produced` again — the next `take` would see
    /// `last_emitted >= want` and *suppress* it. The barrier would be silently
    /// lost, and a lost barrier is R22: an intermittent "unknown handle" under
    /// load, which is the single worst class of bug to ship.
    ///
    /// One caller: the producer retrying after a full ring.
    pub fn restore(&mut self, b: RingBarrier) {
        debug_assert_eq!(b.ring, self.depends_ring);
        self.pending = Some(match self.pending {
            Some(p) => p.max(b.seqno),
            None => b.seqno,
        });
        self.last_emitted = match self.last_emitted {
            Some(l) if l >= b.seqno => b.seqno.checked_sub(1),
            other => other,
        };
        self.emitted = self.emitted.saturating_sub(1);
    }

    /// `(emitted, suppressed)`. A conservative rule that never suppresses is a
    /// rule that is not working, and a rule that suppresses everything is a bug;
    /// exposing the counters is how a test can tell those apart.
    pub fn stats(&self) -> (u64, u64) {
        (self.emitted, self.suppressed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Geometry, ProtocolIds, RingKind};

    struct Fixture {
        control_owner: Control,
        control_peer: Control,
        region: Arc<SharedRegion>,
    }

    fn fixture() -> Fixture {
        let mut g = Geometry::small();
        g.ring_data_bytes = 4096;
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let ids = ProtocolIds::new(1, 0x1234);
        let control_owner = Control::create(Arc::clone(&region), g, ids, 1).unwrap();
        let control_peer = Control::attach(Arc::clone(&region), ids).unwrap();
        Fixture { control_owner, control_peer, region }
    }

    /// A command of `n` bytes that looks like an sv-wire stream, so the framing
    /// cross-check has something real to check.
    fn command(opcode: u32, payload_len: usize) -> Vec<u8> {
        let total = sv_wire::COMMAND_HEADER_SIZE + payload_len;
        let mut v = vec![0u8; total];
        v[0..4].copy_from_slice(&opcode.to_le_bytes());
        v[8..12].copy_from_slice(&(total as u32).to_le_bytes());
        for (i, b) in v[sv_wire::COMMAND_HEADER_SIZE..].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        v
    }

    /// The reply lane's shape, which is **not** the command lane's.
    ///
    /// `sv_wire::ReplyHeader` is `{cmd_type, reply_size}` — eight bytes, with
    /// the length at offset 4, where a `CommandHeader` has `cmd_flags`.
    fn reply(opcode: u32, payload_len: usize) -> Vec<u8> {
        let total = sv_wire::REPLY_HEADER_SIZE + payload_len;
        let mut v = vec![0u8; total];
        v[0..4].copy_from_slice(&opcode.to_le_bytes());
        v[4..8].copy_from_slice(&(total as u32).to_le_bytes());
        for (i, b) in v[sv_wire::REPLY_HEADER_SIZE..].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        v
    }

    #[test]
    fn a_packet_makes_the_round_trip() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        let mut c = RingConsumer::open(&f.control_owner, slot, Lane::Command).unwrap();

        assert!(c.next().unwrap().is_none());
        let cmd = command(42, 20);
        let seqno = p.push(&cmd, None).unwrap();
        assert_eq!(seqno, 1, "seqnos start at 1; 0 means 'nothing completed'");
        assert_eq!(c.ctrl().completed_seqno().unwrap(), 0);

        let pkt = c.next().unwrap().expect("a packet was pushed");
        assert_eq!(pkt.seqno, 1);
        assert!(pkt.barrier.is_none());
        assert_eq!(pkt.payload, &cmd[..]);
        c.advance().unwrap();
        assert!(c.next().unwrap().is_none());
        assert_eq!(c.ctrl().completed_seqno().unwrap(), 1);
    }

    #[test]
    fn the_reply_lane_is_the_same_ring_going_the_other_way() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut cmd_p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        let mut cmd_c = RingConsumer::open(&f.control_owner, slot, Lane::Command).unwrap();
        let mut rep_p = RingProducer::open(&f.control_owner, slot, Lane::Reply).unwrap();
        let mut rep_c = RingConsumer::open(&f.control_peer, slot, Lane::Reply).unwrap();

        let seqno = cmd_p.push(&command(7, 8), None).unwrap();
        let got = cmd_c.next().unwrap().unwrap().seqno;
        cmd_c.advance().unwrap();
        assert_eq!(got, seqno);

        // A reply carries the seqno of the command it answers, so it is matched
        // by number rather than by position — which is what makes replies a
        // stream and not sharedgl's single mailbox (§4.4).
        rep_p.push_seq(seqno, &reply(7, 4), None).unwrap();
        let r = rep_c.next().unwrap().unwrap();
        assert_eq!(r.seqno, seqno);
        rep_c.advance().unwrap();
    }

    /// The two lanes carry different headers, and the framing check has to read
    /// the right one.
    ///
    /// This is a regression test, and the regression it guards against was live:
    /// the consumer read every payload's declared length from the *command*
    /// header's offset 8, so on a reply lane it read the reply's first payload
    /// field. A `vkCreateInstance` reply whose first field is `VK_SUCCESS`
    /// declared a length of zero, and the very first forwarded command failed
    /// with `FramingDisagreement { framed: 32, declared: 0 }`. It was invisible
    /// because the only reply lane in the tree at the time — `sv-loopback`'s —
    /// hand-encoded its replies with a `CommandHeader`.
    #[test]
    fn each_lane_checks_its_own_header() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();

        // A well-formed reply passes on the reply lane…
        let mut rep_p = RingProducer::open(&f.control_owner, slot, Lane::Reply).unwrap();
        let mut rep_c = RingConsumer::open(&f.control_peer, slot, Lane::Reply).unwrap();
        rep_p.push_seq(1, &reply(7, 24), None).unwrap();
        assert!(rep_c.next().unwrap().is_some());
        rep_c.advance().unwrap();

        // …and a *command*-shaped payload on the reply lane does not, because
        // its offset-4 word is `cmd_flags`, not a length. This is the check
        // doing its job rather than a limitation: the two headers are different
        // and a stream carrying the wrong one is desynchronised (R13).
        rep_p.push_seq(2, &command(7, 24), None).unwrap();
        assert!(
            matches!(rep_c.next(), Err(Error::FramingDisagreement { .. })),
            "a command header on the reply lane must be caught, not decoded"
        );
    }

    #[test]
    fn a_zero_length_command_is_refused_by_the_producer() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        assert_eq!(
            p.push(&[], None).unwrap_err(),
            Error::BadPacketLength { ring: slot, len: 0 }
        );
        // …and nothing was published, so the ring is exactly as it was.
        let mut c = RingConsumer::open(&f.control_owner, slot, Lane::Command).unwrap();
        assert!(c.next().unwrap().is_none());
    }

    /// The consumer's half of the same problem: a length word of zero in the
    /// ring is an error, not a record of no bytes that gets read for ever.
    #[test]
    fn a_zero_length_record_in_the_ring_is_an_error_and_not_a_spin() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        let mut c = RingConsumer::open(&f.control_owner, slot, Lane::Command).unwrap();
        p.push(&command(1, 16), None).unwrap();
        // Corrupt the length in place, as a desynchronised producer would.
        let ((cmd_off, _), _) = f.control_owner.ring_buffers(slot).unwrap();
        f.region.write_bytes(cmd_off, &0u32.to_le_bytes()).unwrap();
        assert_eq!(
            c.next().unwrap_err(),
            Error::BadPacketLength { ring: slot, len: 0 }
        );
    }

    #[test]
    fn a_misaligned_or_overlong_record_is_refused() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        let ((cmd_off, cap), _) = f.control_owner.ring_buffers(slot).unwrap();
        for bad in [1u32, 15, 17, cap + 16, u32::MAX] {
            p.push(&command(1, 16), None).unwrap();
            f.region.write_bytes(cmd_off, &bad.to_le_bytes()).unwrap();
            let mut c = RingConsumer::open(&f.control_owner, slot, Lane::Command).unwrap();
            assert!(
                matches!(c.next(), Err(Error::BadPacketLength { .. })),
                "length {bad} was accepted"
            );
            // Reset the lane for the next case.
            f.region
                .atomic_u64(f.control_owner.ring_ctrl_offset(slot).unwrap() + RC_CMD_HEAD)
                .unwrap()
                .store(0, Ordering::Release);
            f.region
                .atomic_u64(f.control_owner.ring_ctrl_offset(slot).unwrap() + RC_CMD_TAIL)
                .unwrap()
                .store(0, Ordering::Release);
        }
    }

    /// Deviation 1's payoff: two independent lengths, and a disagreement caught
    /// at the first bad packet rather than propagated (R13).
    #[test]
    fn framing_and_the_command_header_must_agree() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        let mut c = RingConsumer::open(&f.control_owner, slot, Lane::Command).unwrap();
        let mut cmd = command(9, 32);
        // The command header now claims more bytes than the frame carries — the
        // shape a version skew takes when one side's generator emits a longer
        // struct than the other's expects.
        cmd[8..12].copy_from_slice(&9999u32.to_le_bytes());
        p.push(&cmd, None).unwrap();
        assert!(matches!(c.next(), Err(Error::FramingDisagreement { .. })));
    }

    #[test]
    fn a_full_ring_says_so_and_recovers_when_drained() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        let mut c = RingConsumer::open(&f.control_owner, slot, Lane::Command).unwrap();

        // 12-byte command header + 228 = 240 bytes of payload, and a 16-byte
        // packet header on top: 256 exactly, which divides the 4096-byte lane.
        // Choosing a size that divides the capacity means no pad record is ever
        // produced here, so this test is about fullness and nothing else.
        let cmd = command(1, 228);
        let mut sent = 0u64;
        loop {
            match p.push(&cmd, None) {
                Ok(_) => sent += 1,
                Err(Error::RingFull { .. }) => break,
                Err(e) => panic!("unexpected {e}"),
            }
            assert!(sent < 1000, "the ring never filled");
        }
        assert_eq!(sent, p.capacity() / 256, "the ring should hold exactly its capacity");
        assert!(sent > 0, "a ring that accepted nothing is not a full ring");

        // Drain one, send one: the classic property a broken free-space
        // calculation gets wrong in exactly one direction.
        for _ in 0..3 {
            assert!(c.next().unwrap().is_some());
            c.advance().unwrap();
            p.push(&cmd, None).unwrap();
            assert!(matches!(p.push(&cmd, None), Err(Error::RingFull { .. })));
        }
    }

    #[test]
    fn a_packet_larger_than_the_ring_is_a_caller_error_not_a_wait() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        let huge = command(1, p.capacity() as usize);
        assert!(matches!(p.push(&huge, None), Err(Error::PacketTooLarge { .. })));
        // The boundary is exact, and a payload of exactly `max_payload` fits.
        let big = command(1, p.max_payload() as usize - sv_wire::COMMAND_HEADER_SIZE);
        p.push(&big, None).unwrap();
    }

    /// The wrap, with a positive control: if the ring never actually wrapped,
    /// this test would pass while testing nothing, which is the false negative
    /// this project has already been bitten by once.
    #[test]
    fn the_ring_wraps_many_times_and_loses_nothing() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        let mut c = RingConsumer::open(&f.control_owner, slot, Lane::Command).unwrap();

        // Deliberately not a divisor of the capacity, so the wrap lands
        // mid-buffer and the pad path is exercised rather than avoided.
        let mut expect = 1u64;
        let rounds: u32 = if cfg!(miri) { 400 } else { 2000 };
        for i in 0..rounds {
            let payload = 20 + (i as usize % 7) * 12;
            let cmd = command(i, payload);
            loop {
                match p.push(&cmd, None) {
                    Ok(s) => {
                        assert_eq!(s, expect);
                        expect += 1;
                        break;
                    }
                    Err(Error::RingFull { .. }) => {
                        let got = c.next().unwrap().expect("full ring with nothing in it").seqno;
                        c.advance().unwrap();
                        assert!(got < expect);
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        }
        let mut drained = 0;
        while let Some(pkt) = c.next().unwrap() {
            assert!(pkt.payload.len() >= sv_wire::COMMAND_HEADER_SIZE);
            drained += 1;
            c.advance().unwrap();
        }
        let (wraps, pads) = c.wrap_stats();
        // Scaled to the traffic, so the positive control keeps its teeth at both
        // volumes: a wrap every ~46 packets at these sizes, so rounds/64 is a
        // floor the test must clear rather than a number that happens to pass.
        let floor = u64::from(rounds / 64);
        assert!(wraps > floor, "only {wraps} wraps in {rounds} packets: the wrap was not tested");
        assert!(pads > floor, "only {pads} pad records: the pad path was not exercised");
        assert!(drained > 0);
        assert_eq!(expect, u64::from(rounds) + 1);
    }

    /// A producer that dies between reserving and publishing. The publication is
    /// the release store of `tail`, so a death before it is invisible — which is
    /// the property that makes a crashed peer a timeout rather than a corrupt
    /// ring.
    #[test]
    fn a_producer_that_dies_before_publishing_leaves_nothing_behind() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        let mut c = RingConsumer::open(&f.control_owner, slot, Lane::Command).unwrap();
        p.push(&command(1, 16), None).unwrap();
        assert!(c.next().unwrap().is_some());
        c.advance().unwrap();

        // Simulate the half-written record: bytes in the buffer past `tail`,
        // exactly as a producer that was killed mid-`write_bytes` would leave.
        let ((cmd_off, _), _) = f.control_owner.ring_buffers(slot).unwrap();
        let tail = f
            .region
            .atomic_u64(f.control_owner.ring_ctrl_offset(slot).unwrap() + RC_CMD_TAIL)
            .unwrap()
            .load(Ordering::Acquire);
        f.region.write_bytes(cmd_off + (tail % p.capacity()), &[0xCC; 64]).unwrap();
        assert!(c.next().unwrap().is_none(), "unpublished bytes must not be visible");

        // And the ring still works afterwards: the garbage is simply overwritten.
        p.push(&command(2, 16), None).unwrap();
        let got = c.next().unwrap().unwrap();
        assert_eq!(got.seqno, 2);
    }

    #[test]
    fn a_generation_bump_under_a_reader_is_an_error_not_a_stale_read() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let mut p = RingProducer::open(&f.control_peer, slot, Lane::Command).unwrap();
        let mut c = RingConsumer::open(&f.control_owner, slot, Lane::Command).unwrap();
        p.push(&command(1, 16), None).unwrap();
        assert!(c.next().unwrap().is_some());
        c.advance().unwrap();

        // The peer restarted. R4: detected rather than deadlocked against.
        f.region
            .atomic_u32(crate::control::OFF_GENERATION)
            .unwrap()
            .store(2, Ordering::Release);
        assert_eq!(
            c.next().unwrap_err(),
            Error::PeerRestarted { expected: 1, found: 2 }
        );
        assert_eq!(
            p.push(&command(1, 16), None).unwrap_err(),
            Error::PeerRestarted { expected: 1, found: 2 }
        );
    }

    #[test]
    fn a_barrier_rides_the_packet_and_is_satisfied_by_the_other_rings_progress() {
        let f = fixture();
        let device = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let queue = f.control_peer.claim_ring(RingKind::Queue, 2, Some(device)).unwrap();

        let mut dev_p = RingProducer::open(&f.control_peer, device, Lane::Command).unwrap();
        let mut dev_c = RingConsumer::open(&f.control_owner, device, Lane::Command).unwrap();
        let mut q_p = RingProducer::open(&f.control_peer, queue, Lane::Command).unwrap();
        let mut q_c = RingConsumer::open(&f.control_owner, queue, Lane::Command).unwrap();

        // The app creates an object on the device ring, then submits on the
        // queue ring. The GPU side may reach the submit first.
        let create = dev_p.push(&command(10, 16), None).unwrap();
        let mut tracker = BarrierTracker::new(device);
        tracker.note_produced(create);
        let barrier = tracker.take();
        assert_eq!(barrier, Some(RingBarrier { ring: device, seqno: create }));
        q_p.push(&command(11, 16), barrier).unwrap();

        let pkt = q_c.next().unwrap().unwrap();
        let b = pkt.barrier.expect("the barrier must survive the wire");
        assert_eq!(b, RingBarrier { ring: device, seqno: create });
        assert!(!barrier_satisfied(&f.control_owner, b).unwrap(), "nothing has run yet");

        // The device ring's consumer catches up.
        dev_c.next().unwrap().unwrap();
        dev_c.advance().unwrap();
        assert!(barrier_satisfied(&f.control_owner, b).unwrap());
        q_c.advance().unwrap();
    }

    #[test]
    fn a_barrier_naming_a_ring_that_does_not_exist_is_an_error() {
        let f = fixture();
        assert!(matches!(
            barrier_satisfied(&f.control_owner, RingBarrier { ring: 99, seqno: 0 }),
            Err(Error::NoSuchRing { ring: 99 })
        ));
    }

    #[test]
    fn the_tracker_emits_conservatively_and_suppresses_the_redundant_ones() {
        let mut t = BarrierTracker::new(3);
        assert_eq!(t.take(), None, "nothing produced, nothing to order against");
        t.note_produced(5);
        t.note_produced(7);
        // One barrier for both creations, naming the later seqno: waiting for 7
        // implies waiting for 5.
        assert_eq!(t.take(), Some(RingBarrier { ring: 3, seqno: 7 }));
        assert_eq!(t.take(), None);
        t.note_produced(6);
        // Already covered by the barrier at 7 — suppressed, not emitted.
        assert_eq!(t.take(), None);
        t.note_produced(9);
        assert_eq!(t.take(), Some(RingBarrier { ring: 3, seqno: 9 }));
        let (emitted, suppressed) = t.stats();
        assert_eq!(emitted, 2);
        assert_eq!(suppressed, 1, "a rule that never suppresses is not the rule");
    }

    #[test]
    fn a_fatal_message_survives_the_round_trip_and_is_ordered_after_its_status() {
        let f = fixture();
        let slot = f.control_peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let ctrl_a = RingCtrl::open(&f.control_owner, slot).unwrap();
        let ctrl_b = RingCtrl::open(&f.control_peer, slot).unwrap();
        assert_eq!(ctrl_b.fatal().unwrap(), None);
        ctrl_a.set_fatal(13, 99, "command 42 declared 8 bytes and consumed 12").unwrap();
        let (code, seqno, msg) = ctrl_b.fatal().unwrap().expect("a fatal ring reports why");
        assert_eq!((code, seqno), (13, 99));
        assert_eq!(msg, "command 42 declared 8 bytes and consumed 12");
        assert_eq!(ctrl_b.status(), RingStatus::Fatal);

        // A message longer than the slot is truncated, not written past it.
        ctrl_a.set_fatal(1, 0, &"x".repeat(1000)).unwrap();
        assert_eq!(ctrl_b.fatal().unwrap().unwrap().2.len(), RC_FATAL_MSG_LEN as usize - 1);
    }

    #[test]
    fn the_reverse_ring_needs_no_special_case() {
        // docs/design.md §4.6 budgets ~200 lines for it. It is the same ring with the
        // producer on the other side, so this test is the whole of it.
        let f = fixture();
        let slot = crate::control::REVERSE_RING_SLOT;
        assert_eq!(f.control_peer.ring_kind(slot).unwrap(), RingKind::Reverse);
        let mut gpu = RingProducer::open(&f.control_owner, slot, Lane::Command).unwrap();
        let mut app = RingConsumer::open(&f.control_peer, slot, Lane::Command).unwrap();
        gpu.push(&command(0xdead, 32), None).unwrap();
        let msg = app.next().unwrap().expect("a debug message");
        assert_eq!(u32::from_le_bytes(msg.payload[0..4].try_into().unwrap()), 0xdead);
        app.advance().unwrap();
    }
}
