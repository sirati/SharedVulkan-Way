//! The transport: the control region, the rings, the handshake, and the wait
//! primitive.
//!
//! This is design.md §4 made executable. The shape of the crate follows the
//! shape of that section:
//!
//! | module | design.md |
//! |---|---|
//! | [`region`] | §4.1, the mapping itself. **The only module here with `unsafe`.** |
//! | [`control`] | §4.2 layout, §4.3 handshake, generation, heartbeats |
//! | [`ring`] | §4.4 rings, reply rings, the `RingBarrier`, §4.6 reverse ring |
//! | [`park`] | §4.5 `Parker`/`Doorbell` behind a trait, timeouts on every wait |
//! | [`cold`] | §4.3 step 2–3, the cold-path socket and the **name** handoff |
//!
//! # The three things this crate exists to get right
//!
//! **1. The readiness flag is real.** sharedgl has no readiness flag at all,
//! because its `READY_HINT` was aliased away by a `SHM_SLOT_MASK` at the same
//! offset (sgl §5, §7.2b). The answer is not "be careful": it is that the layout
//! is computed by one function, every field's extent is derived from that
//! function, and [`control::Layout::validate`] proves at run time — and in a
//! test — that no two things in the region overlap and that everything is inside
//! it. An aliasing bug of sharedgl's kind cannot be written here without
//! failing a test that names it.
//!
//! **2. A restarted peer is detected, not deadlocked against** (R4). Every ring
//! handle carries the `generation` it was created under and every ring operation
//! checks it, so a peer that restarts turns every outstanding operation into
//! [`Error::PeerRestarted`] rather than into a wait that never ends. Waits have
//! deadlines and there is no API for an infinite one.
//!
//! **3. Every offset is checked against the region length** (R2). There is one
//! bounds check, in [`region::SharedRegion::check`], and every other access in
//! this crate and in `sv-arena` goes through it. Handles are directory indices,
//! never pointers (R3) — see [`ring::RingId`].
//!
//! # `unsafe`
//!
//! R7 says confine it to a small number of reviewed modules with stated
//! invariants. In this crate that is exactly [`region`] (the mapping) and the
//! quarantined `SCM_RIGHTS` handoff in `localfd`. **[`cold`] has none**, which
//! is new: the cold path used to need `sendmsg` to pass a descriptor and now
//! passes a name, so the module that defines the protocol is safe code over
//! `sv-wire` and `std`. `ring`, `control` and `park` contain none either: they
//! are safe code over [`region::SharedRegion`]'s atomic-and-copy API, which is
//! the point of that API existing.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod cold;
pub mod control;
/// The quarantined `SCM_RIGHTS` handoff — see the module's own documentation for
/// what still needs it and what measurement would remove it.
#[cfg(target_os = "linux")]
pub mod localfd;
pub mod park;
pub mod region;
pub mod ring;
pub mod stream;
pub mod winpipe;
pub mod winshm;

use core::fmt;

/// Everything that can go wrong in the transport.
///
/// One flat enum rather than a per-module tree, because these values end up in
/// a ring's `fatal_msg` and in a log line, and a nested error is harder to read
/// there than a flat one is to match on here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    // -- region (R2) --------------------------------------------------------
    /// An offset or length fell outside the mapping. This is the R2 check, and
    /// the realistic thing it catches is our own arithmetic, not an attacker.
    OutOfBounds { off: u64, len: u64, region_len: usize },
    /// An atomic field is not naturally aligned. A layout bug, always.
    Misaligned { off: u64, align: u64 },

    // -- handshake (§4.3) ---------------------------------------------------
    /// `magic` is zero: the owner has not published yet.
    NotReady,
    /// `magic` is neither zero nor ours. On a big-endian peer this is what a
    /// correct region looks like, which is why endianness needs no separate
    /// field — see [`control::MAGIC`].
    MagicMismatch { found: u64 },
    /// The header layout itself differs.
    LayoutVersionMismatch { ours: u32, theirs: u32 },
    /// The generated protocol differs.
    WireVersionMismatch { ours: u32, theirs: u32 },
    /// R13: two builds of our own code that would desynchronise rather than
    /// fail. Dump `sv_proto::PROTOCOL_MANIFEST` on both sides and diff it.
    ProtocolHashMismatch { ours: u64, theirs: u64 },
    /// The peer is not little-endian, or not 64-bit.
    IncompatiblePeer { endianness: u32, pointer_width: u32 },
    /// The header says the control region is larger than what we mapped.
    RegionTooSmall { needed: u64, mapped: usize },
    /// The geometry in the header is not self-consistent — see
    /// [`control::Layout::validate`].
    BadGeometry(&'static str),

    // -- liveness (R4) ------------------------------------------------------
    /// The peer restarted: the generation moved under us. Every handle, offset
    /// and timeline the other side holds is stale, and the app side turns this
    /// into `VK_ERROR_DEVICE_LOST` (design.md §4.3).
    PeerRestarted { expected: u32, found: u32 },
    /// A wait hit its deadline. Always carries which ring, because "something
    /// timed out" is not a diagnosis.
    TimedOut { ring: u32 },
    /// The peer marked the ring fatal, or its heartbeat stopped.
    PeerGone { ring: u32 },
    /// The peer wrote a fatal status into the ring control block.
    RingFatal { ring: u32, code: u32 },

    // -- rings (§4.4) -------------------------------------------------------
    /// The ring has no room. Not an error in itself — the producer waits — but
    /// it is an error to the caller that asked not to wait.
    RingFull { ring: u32, need: u64, free: u64 },
    /// A packet longer than the ring can ever hold. This is a caller bug: the
    /// payload belongs in an arena blob (design.md §6.5).
    PacketTooLarge { need: u64, capacity: u64 },
    /// A framing length that cannot be right. A zero-length record would make
    /// the consumer read the same bytes for ever, so it is rejected by name
    /// rather than by "and then nothing happens".
    BadPacketLength { ring: u32, len: u32 },
    /// The ring's framing length and the `sv-wire` command header's `cmd_size`
    /// disagree. Two independent lengths, so a desynchronisation is caught at
    /// the first bad packet rather than propagated (deviation 1, R13).
    FramingDisagreement { ring: u32, framed: u32, declared: u32 },
    /// A directory slot that is not what the caller thinks it is.
    NoSuchRing { ring: u32 },
    /// Claiming a ring that somebody else already claimed.
    RingBusy { ring: u32 },
    /// The directory is full.
    NoFreeRing,
    /// A barrier naming a ring that does not exist, or that never reaches the
    /// seqno. R22 says this is fatal and loud rather than skipped.
    UnsatisfiableBarrier { ring: u32, seqno: u64 },

    // -- plumbing -----------------------------------------------------------
    /// A wire encoding error from a cold-path message.
    Wire(sv_wire::WireError),
    /// An OS error, with the call that produced it.
    Os { call: &'static str, errno: i32 },
    /// A cold-path message that is not one of ours.
    BadColdMessage(&'static str),
    /// Something this build does not implement — always with the reason.
    Unsupported(&'static str),
}

impl From<sv_wire::WireError> for Error {
    fn from(e: sv_wire::WireError) -> Self {
        Error::Wire(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::OutOfBounds { off, len, region_len } => write!(
                f,
                "offset {off}+{len} is outside the {region_len}-byte region"
            ),
            Error::Misaligned { off, align } => {
                write!(f, "offset {off} is not {align}-byte aligned")
            }
            Error::NotReady => write!(f, "the owner has not published the region yet"),
            Error::MagicMismatch { found } => {
                write!(f, "magic is 0x{found:016x}, which is not ours (a byte-swapped magic means a big-endian peer)")
            }
            Error::LayoutVersionMismatch { ours, theirs } => {
                write!(f, "control-region layout version {theirs}, we speak {ours}")
            }
            Error::WireVersionMismatch { ours, theirs } => {
                write!(f, "wire version {theirs}, we speak {ours}")
            }
            Error::ProtocolHashMismatch { ours, theirs } => write!(
                f,
                "protocol hash 0x{theirs:016x} against our 0x{ours:016x}: different generated protocols"
            ),
            Error::IncompatiblePeer { endianness, pointer_width } => write!(
                f,
                "peer endianness marker 0x{endianness:08x}, pointer width {pointer_width}"
            ),
            Error::RegionTooSmall { needed, mapped } => {
                write!(f, "the header needs {needed} bytes, {mapped} are mapped")
            }
            Error::BadGeometry(why) => write!(f, "inconsistent control-region geometry: {why}"),
            Error::PeerRestarted { expected, found } => {
                write!(f, "peer restarted: generation {expected} became {found}")
            }
            Error::TimedOut { ring } => write!(f, "wait on ring {ring} timed out"),
            Error::PeerGone { ring } => write!(f, "peer of ring {ring} is gone"),
            Error::RingFatal { ring, code } => write!(f, "ring {ring} is fatal, code {code}"),
            Error::RingFull { ring, need, free } => {
                write!(f, "ring {ring} needs {need} bytes, {free} free")
            }
            Error::PacketTooLarge { need, capacity } => write!(
                f,
                "a {need}-byte packet cannot fit a {capacity}-byte ring; the payload belongs in the arena"
            ),
            Error::BadPacketLength { ring, len } => {
                write!(f, "ring {ring} carries a packet of length {len}")
            }
            Error::FramingDisagreement { ring, framed, declared } => write!(
                f,
                "ring {ring}: framing says {framed} bytes, the command header says {declared}"
            ),
            Error::NoSuchRing { ring } => write!(f, "no ring {ring}"),
            Error::RingBusy { ring } => write!(f, "ring {ring} is already claimed"),
            Error::NoFreeRing => write!(f, "the ring directory is full"),
            Error::UnsatisfiableBarrier { ring, seqno } => {
                write!(f, "barrier on ring {ring} seqno {seqno} cannot be satisfied")
            }
            Error::Wire(e) => write!(f, "wire: {e}"),
            Error::Os { call, errno } => write!(f, "{call} failed: errno {errno}"),
            Error::BadColdMessage(why) => write!(f, "bad cold-path message: {why}"),
            Error::Unsupported(why) => write!(f, "unsupported: {why}"),
        }
    }
}

impl core::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;
