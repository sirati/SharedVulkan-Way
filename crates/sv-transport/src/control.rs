//! The control region: fixed layout, handshake, generation, heartbeats.
//!
//! design.md §4.2 gives the layout as a table and §4.3 gives the handshake as a
//! numbered sequence. Both are reproduced here as code, and the offsets below
//! are that table transcribed — if the table moves, [`LAYOUT_VERSION`] moves
//! with it and two builds refuse each other instead of misreading each other.
//!
//! # The sharedgl lesson, made into a test
//!
//! > sharedgl has no readiness flag at all, because `READY_HINT` was aliased
//! > away by `SHM_SLOT_MASK` at the same offset (sgl §5, §7.2b). Do not repeat
//! > that. — design.md §4.3
//!
//! "Do not repeat that" is not a mechanism, so here is the mechanism. Every
//! extent of the region is produced by [`Layout::compute`] and enumerated by
//! [`Layout::sections`]; [`Layout::validate`] proves that the sections are
//! pairwise disjoint, page-aligned and inside `control_size`. It runs at
//! creation, at attach, and in a test. An overlapping field is not a thing to
//! notice in review — it fails the handshake on both sides, by name.
//!
//! The readiness signal itself is then doubled deliberately: [`Flags::READY`]
//! is written *before* the rest of the header is finished, and [`MAGIC`] is
//! written **last with a release store**. A peer requires both. That is one more
//! check than strictly necessary, and it is the check that fails loudly if the
//! flags word is ever aliased by something else — see
//! `a_ready_flag_that_is_aliased_away_fails_the_handshake`.

use crate::region::{PAGE, SharedRegion, page_up};
use crate::{Error, Result};
use core::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `b"SHVRK1\0\0"` read as a little-endian `u64`.
///
/// design.md §4.2 gives the constant as `0x5348_5652_4B31_0000`. Written to the
/// region as little-endian bytes, so a big-endian peer loads a byte-swapped
/// value and refuses — which is why "refuse big-endian peers at handshake"
/// needs no field of its own. The cold path checks it a second time and
/// explicitly ([`crate::cold::Hello`]), because a diagnostic that says
/// "big-endian peer" is worth more than one that says "bad magic".
pub const MAGIC: u64 = 0x5348_5652_4B31_0000;

/// Version of the header table below. Bumped only when the table changes.
pub const LAYOUT_VERSION: u32 = 1;

/// A little-endian marker, exchanged on the cold path.
pub const ENDIAN_MARKER: u32 = 0x0403_0201;

// -- header field offsets, design.md §4.2 -----------------------------------

pub const OFF_MAGIC: u64 = 0x00;
pub const OFF_LAYOUT_VERSION: u64 = 0x08;
pub const OFF_WIRE_VERSION: u64 = 0x0C;
pub const OFF_PROTOCOL_HASH: u64 = 0x10;
pub const OFF_GENERATION: u64 = 0x18;
pub const OFF_FLAGS: u64 = 0x1C;
pub const OFF_CONTROL_SIZE: u64 = 0x20;
pub const OFF_ARENA_SIZE: u64 = 0x28;
pub const OFF_ARENA_SLAB_SIZE: u64 = 0x30;
pub const OFF_RING_SLOT_COUNT: u64 = 0x38;
pub const OFF_RING_DATA_BYTES: u64 = 0x3C;
pub const OFF_RING_DIR_OFFSET: u64 = 0x40;
pub const OFF_SLAB_DIR_OFFSET: u64 = 0x48;
pub const OFF_TIMELINE_DIR_OFFSET: u64 = 0x50;
pub const OFF_REVERSE_RING_OFFSET: u64 = 0x58;
pub const OFF_OWNER_HEARTBEAT: u64 = 0x60;
pub const OFF_PEER_HEARTBEAT: u64 = 0x68;
/// design.md §4.2 lists `owner_epoch_ns, peer_epoch_ns` as a single row at
/// 0x70. They are two `u64`s and cannot share an offset; this is where the
/// second one goes. Diagnostics only, per that table.
pub const OFF_OWNER_EPOCH_NS: u64 = 0x70;
pub const OFF_PEER_EPOCH_NS: u64 = 0x78;
/// Two fields the design's table does not name, and which the geometry needs
/// once the slab directory is a real thing with a bounded number of entries.
pub const OFF_SLAB_DIR_COUNT: u64 = 0x80;
pub const OFF_TIMELINE_COUNT: u64 = 0x84;
/// Where the ring control blocks and data buffers start. Derivable from the
/// geometry, and published anyway so that the attaching side reads the layout
/// rather than recomputing it and being subtly right in a different way.
pub const OFF_RING_CTRL_OFFSET: u64 = 0x88;
pub const OFF_RING_DATA_OFFSET: u64 = 0x90;

/// The header occupies one page; everything after 0x98 is reserved and zero.
pub const HEADER_SIZE: u64 = PAGE as u64;

/// `flags`, design.md §4.2.
pub struct Flags;
impl Flags {
    /// The owner finished writing the header. Set *before* `magic`.
    pub const READY: u32 = 1 << 0;
    pub const OWNER_FATAL: u32 = 1 << 1;
    pub const PEER_FATAL: u32 = 1 << 2;
    pub const SHUTDOWN: u32 = 1 << 3;
}

// -- ring directory, design.md §4.2 -----------------------------------------

/// One cache line per entry, so two rings' directory entries never share one.
pub const RING_DIR_ENTRY_SIZE: u64 = 64;
pub const RD_STATE: u64 = 0x00;
pub const RD_KIND: u64 = 0x04;
pub const RD_OWNER_ID: u64 = 0x08;
pub const RD_CMD_OFF: u64 = 0x10;
pub const RD_CMD_LEN: u64 = 0x14;
pub const RD_REPLY_OFF: u64 = 0x18;
pub const RD_REPLY_LEN: u64 = 0x1C;
pub const RD_CTRL_OFF: u64 = 0x20;
pub const RD_DEPENDS_RING: u64 = 0x24;

/// `state`, design.md §4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RingState {
    Free = 0,
    Claimed = 1,
    Live = 2,
    Dead = 3,
}

impl RingState {
    pub fn from_u32(v: u32) -> Option<RingState> {
        Some(match v {
            0 => RingState::Free,
            1 => RingState::Claimed,
            2 => RingState::Live,
            3 => RingState::Dead,
            _ => return None,
        })
    }
}

/// `kind`, design.md §4.2 and the table in §4.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RingKind {
    Instance = 1,
    Device = 2,
    Queue = 3,
    /// GPU→app. design.md §4.6: built from day one, not retrofitted.
    Reverse = 4,
    /// One Wayland connection. waycompose-design.md §36.
    ///
    /// The odd one out in this enum, and deliberately in it anyway. The first
    /// four carry an `sv-wire` command stream; this one carries an opaque byte
    /// stream, framed with [`crate::ring::PacketKind::Bytes`]. It is here rather
    /// than in a second directory because **a Wayland connection and a Vulkan
    /// queue are the same kind of thing to this layer**: a claim on a ring slot
    /// in one session's control region, with one lane each way and a doorbell
    /// per lane. Giving Wayland its own directory would have been giving it its
    /// own transport, which is exactly the correction §36 exists to record.
    ///
    /// A ring of this kind is claimed once per *connection*, never once per
    /// client — §0.12's `--clients N` lesson, applied one level down.
    Wayland = 5,
}

impl RingKind {
    pub fn from_u32(v: u32) -> Option<RingKind> {
        Some(match v {
            1 => RingKind::Instance,
            2 => RingKind::Device,
            3 => RingKind::Queue,
            4 => RingKind::Reverse,
            5 => RingKind::Wayland,
            _ => return None,
        })
    }
}

// -- ring control block, design.md §4.2 -------------------------------------

/// One page per ring, "deliberately over-sized so producer and consumer never
/// share a cache line, which is the mistake sharedgl's single global `SUBMIT`
/// int makes structural".
pub const RING_CTRL_SIZE: u64 = PAGE as u64;
pub const RC_CMD_TAIL: u64 = 0x000;
pub const RC_NEXT_SEQNO: u64 = 0x008;
pub const RC_CMD_HEAD: u64 = 0x040;
pub const RC_COMPLETED_SEQNO: u64 = 0x048;
pub const RC_STATUS: u64 = 0x080;
pub const RC_IDLE_TIMEOUT_US: u64 = 0x084;
pub const RC_NOTIFY_COUNT: u64 = 0x088;
pub const RC_REPLY_TAIL: u64 = 0x0C0;
pub const RC_REPLY_HEAD: u64 = 0x100;
pub const RC_FATAL_CODE: u64 = 0x140;
pub const RC_FATAL_SEQNO: u64 = 0x148;
pub const RC_FATAL_MSG: u64 = 0x150;
pub const RC_FATAL_MSG_LEN: u64 = 160;

// -- the doorbell, one pair of words per lane --------------------------------
//
// design.md §4.5 lists four parkers and says the second — "`Futex` — Linux-only,
// works across processes on shared memory. Used by the same-machine development
// loop, where it is strictly better than spinning and costs nothing" — is the
// one the development loop should use. These are the words it needs.
//
// **Why a pair per lane rather than reusing `status` and `notify_count`.**
// `RC_STATUS` is one word for the whole ring, and both lanes park: the app side
// waits on the reply lane while the GPU side waits on the command lane, so one
// `IDLE` flag cannot say which of the two is asleep. And `RC_NOTIFY_COUNT` is a
// `u64` counter for diagnostics; a futex word must be a `u32` at a known address
// because that is what the kernel compares against.
//
// `wake` is bumped by the producer and is the address both sides name; `parked`
// is set by the consumer immediately before it waits, so the producer can skip
// the syscall entirely in the common case — design.md §4.4's "the producer
// checks `IDLE` before paying for a notification", made per-lane.
//
// They are at 0x200 and 0x240 rather than in the gap after `RC_NOTIFY_COUNT`
// because `RC_FATAL_MSG` runs to 0x1F0, and because a doorbell word shared with
// anything else is a cache line two threads fight over on every packet.
pub const RC_CMD_WAKE: u64 = 0x200;
pub const RC_CMD_PARKED: u64 = 0x204;
pub const RC_REPLY_WAKE: u64 = 0x240;
pub const RC_REPLY_PARKED: u64 = 0x244;

/// `status`, design.md §4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RingStatus {
    /// The consumer has parked; the producer must pay for a notification.
    Idle = 0,
    /// The consumer is spinning; a notification is not needed.
    Alive = 1,
    Fatal = 2,
}

impl RingStatus {
    pub fn from_u32(v: u32) -> RingStatus {
        match v {
            1 => RingStatus::Alive,
            2 => RingStatus::Fatal,
            _ => RingStatus::Idle,
        }
    }
}

// -- slab directory, design.md §4.2 and §5.2 --------------------------------

pub const SLAB_DIR_ENTRY_SIZE: u64 = 64;
pub const SD_STATE: u64 = 0x00;
pub const SD_CACHE_CLASS: u64 = 0x04;
pub const SD_ARENA_OFFSET: u64 = 0x08;
pub const SD_SIZE: u64 = 0x10;
pub const SD_GPU_MEMORY_TYPE: u64 = 0x18;
pub const SD_IMPORT_STATE: u64 = 0x1C;
pub const SD_REFCOUNT: u64 = 0x20;

/// `cache_class`. design.md D11 and §5.6: exactly one legal value in v1.
///
/// The field exists so that the mechanism to vary it exists; R10 is retired *by
/// construction*, and a second value would have to arrive with a measurement
/// and a transport that can give both sides a matching mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CacheClass {
    WriteBack = 0,
}

// -- timeline mirror, design.md §4.2 ----------------------------------------

pub const TIMELINE_ENTRY_SIZE: u64 = 64;
pub const TL_SEMAPHORE_ID: u64 = 0x00;
pub const TL_VALUE: u64 = 0x08;
pub const TL_WAITER_COUNT: u64 = 0x10;
/// design.md §4.2: "Timeline mirror table — 1024 × 64 B".
pub const TIMELINE_COUNT_DEFAULT: u32 = 1024;

// ---------------------------------------------------------------------------
// Geometry and layout
// ---------------------------------------------------------------------------

/// The numbers the owner picks and publishes. design.md §4.2's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub control_size: u64,
    pub arena_size: u64,
    pub arena_slab_size: u64,
    pub ring_slot_count: u32,
    /// Bytes in one ring's command buffer. The reply buffer is the same size,
    /// which is where §4.2's `64 × (64 + 64) KiB` comes from.
    pub ring_data_bytes: u32,
    pub timeline_count: u32,
}

impl Default for Geometry {
    fn default() -> Self {
        Geometry {
            control_size: 16 << 20,
            arena_size: 2 << 30,
            arena_slab_size: 64 << 20,
            ring_slot_count: 64,
            ring_data_bytes: 64 << 10,
            timeline_count: TIMELINE_COUNT_DEFAULT,
        }
    }
}

impl Geometry {
    /// A geometry small enough to test with, with the same shape as the real
    /// one. Every ratio the code cares about is preserved; only the magnitudes
    /// shrink.
    pub fn small() -> Self {
        Geometry {
            control_size: 1 << 20,
            arena_size: 4 << 20,
            arena_slab_size: 1 << 20,
            ring_slot_count: 4,
            ring_data_bytes: 8 << 10,
            timeline_count: 16,
        }
    }

    /// The geometry a live session uses, for an arena of `arena_size`.
    ///
    /// **Both sides must choose the same one**, and the reason this is a
    /// function here rather than a constant in each binary is that they did not:
    /// `sv-hostd` served `demo_geometry`'s 1 MiB control region while
    /// `sv-gpu-server` wanted 16 MiB, and the session died at `Control::create`
    /// with "the layout does not fit in control_size". The owner publishes the
    /// geometry and the peer reads it (§4.3), so only the *owner* strictly needs
    /// this — but on the memfd path the owner is not the side that sized the
    /// file, so both do.
    ///
    /// It differs from [`Geometry::default`] in one respect, and the reason is a
    /// consequence of what design.md §6.5 does not yet have: a
    /// `vkCreateShaderModule` carries its SPIR-V **inline on the ring**, because
    /// the arena-blob path is not built. §4.2's 64 KiB ring cannot hold a real
    /// shader module, so the data buffer is 256 KiB and the slot count drops
    /// from 64 to 16 to pay for it. §4.2's own arithmetic then lands on the same
    /// 16 MiB control region it specifies.
    ///
    /// # The slab size is 64 MiB, and it was 4
    ///
    /// **A `VkDeviceMemory` is a sub-range of one slab (§5.3), so the slab size
    /// is a hard ceiling on every host-visible allocation an application can
    /// make**, and `ArenaAllocator::alloc` refuses a larger one with
    /// `LargerThanSlab` — which the ICD reports as
    /// `VK_ERROR_OUT_OF_DEVICE_MEMORY`, a result an application is entitled to
    /// see and has no way to interpret.
    ///
    /// At 4 MiB that ceiling is below one 1080p frame (8.29 MB), which is how it
    /// was found: `vkCreateSwapchainKHR` at 1920×1080 failed while the identical
    /// call at 256×256 succeeded. Nothing before this stage had asked for a
    /// single allocation that big — Zink stages in pieces and DXVK's probe is
    /// 256×256 — so a limit that would stop any real application went unnoticed.
    ///
    /// 64 MiB is design.md §5.2's own number and this was a silent deviation
    /// from it. It costs nothing measurable: the slab directory shrinks by the
    /// same factor it grows, and §0.1 measured that a *larger* window is more
    /// contiguous rather than less.
    pub fn for_arena(arena_size: u64) -> Self {
        Geometry {
            control_size: 16 << 20,
            arena_size,
            arena_slab_size: 64 << 20,
            ring_slot_count: 16,
            ring_data_bytes: 256 << 10,
            timeline_count: 64,
        }
    }
}

/// Where everything is. Computed once, published, and read rather than
/// recomputed by the attaching side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub geometry: Geometry,
    pub ring_dir_offset: u64,
    pub slab_dir_offset: u64,
    pub slab_dir_count: u32,
    pub timeline_dir_offset: u64,
    pub ring_ctrl_offset: u64,
    pub ring_data_offset: u64,
    /// The control block of the reverse ring, which is always slot 0.
    pub reverse_ring_offset: u64,
    /// Bytes actually used. `<= control_size`.
    pub used: u64,
}

/// The reverse ring's directory slot. Fixed, so the app side can find it
/// without walking the directory and without a round trip (design.md §4.6).
pub const REVERSE_RING_SLOT: u32 = 0;

impl Layout {
    /// Place every section, in the order design.md §4.2's sizing paragraph
    /// lists them, page-aligned between sections.
    pub fn compute(g: Geometry) -> Result<Layout> {
        if g.ring_slot_count == 0 {
            return Err(Error::BadGeometry("a region with no rings"));
        }
        if g.arena_slab_size == 0 || !g.arena_slab_size.is_multiple_of(PAGE as u64) {
            return Err(Error::BadGeometry("slab size must be a non-zero multiple of 4096"));
        }
        if !g.arena_size.is_multiple_of(PAGE as u64) {
            return Err(Error::BadGeometry("arena size must be a multiple of 4096"));
        }
        // The ring framing aligns to 16 and needs the capacity to be a power of
        // two so that `head % capacity` is a mask rather than a division. Both
        // are the ring's requirements, checked here because this is where the
        // geometry is decided rather than where it is discovered.
        if !g.ring_data_bytes.is_power_of_two() || g.ring_data_bytes < 4096 {
            return Err(Error::BadGeometry("ring_data_bytes must be a power of two, >= 4096"));
        }

        let mut at = HEADER_SIZE;
        let ring_dir_offset = at;
        at = page_up(at + u64::from(g.ring_slot_count) * RING_DIR_ENTRY_SIZE);

        let slab_dir_count = u32::try_from(g.arena_size.div_ceil(g.arena_slab_size))
            .map_err(|_| Error::BadGeometry("more slabs than a u32 can index"))?;
        let slab_dir_offset = at;
        at = page_up(at + u64::from(slab_dir_count) * SLAB_DIR_ENTRY_SIZE);

        let timeline_dir_offset = at;
        at = page_up(at + u64::from(g.timeline_count) * TIMELINE_ENTRY_SIZE);

        let ring_ctrl_offset = at;
        at = page_up(at + u64::from(g.ring_slot_count) * RING_CTRL_SIZE);

        let ring_data_offset = at;
        let per_ring_data = 2 * u64::from(g.ring_data_bytes);
        at = page_up(at + u64::from(g.ring_slot_count) * per_ring_data);

        let layout = Layout {
            geometry: g,
            ring_dir_offset,
            slab_dir_offset,
            slab_dir_count,
            timeline_dir_offset,
            ring_ctrl_offset,
            ring_data_offset,
            reverse_ring_offset: ring_ctrl_offset + u64::from(REVERSE_RING_SLOT) * RING_CTRL_SIZE,
            used: at,
        };
        layout.validate()?;
        Ok(layout)
    }

    /// Every section, as `(name, offset, len)`.
    ///
    /// This function is the layout. [`Layout::validate`] and the tests both work
    /// from it, so a section that is not listed here is a section that is not
    /// checked — which is the one failure mode worth watching, and is why the
    /// list is short and the naming is boring.
    pub fn sections(&self) -> Vec<(&'static str, u64, u64)> {
        let g = self.geometry;
        vec![
            ("header", 0, HEADER_SIZE),
            (
                "ring_dir",
                self.ring_dir_offset,
                u64::from(g.ring_slot_count) * RING_DIR_ENTRY_SIZE,
            ),
            (
                "slab_dir",
                self.slab_dir_offset,
                u64::from(self.slab_dir_count) * SLAB_DIR_ENTRY_SIZE,
            ),
            (
                "timeline_dir",
                self.timeline_dir_offset,
                u64::from(g.timeline_count) * TIMELINE_ENTRY_SIZE,
            ),
            ("ring_ctrl", self.ring_ctrl_offset, u64::from(g.ring_slot_count) * RING_CTRL_SIZE),
            (
                "ring_data",
                self.ring_data_offset,
                u64::from(g.ring_slot_count) * 2 * u64::from(g.ring_data_bytes),
            ),
        ]
    }

    /// Prove the layout is a layout: everything inside the region, nothing
    /// overlapping anything, every section page-aligned.
    ///
    /// This is the anti-`SHM_SLOT_MASK` check. It costs a dozen comparisons at
    /// handshake and it is the difference between "we were careful" and "an
    /// aliased field cannot reach production".
    pub fn validate(&self) -> Result<()> {
        let mut s = self.sections();
        for (_, off, _) in &s {
            if !off.is_multiple_of(PAGE as u64) {
                return Err(Error::BadGeometry("a section is not page-aligned"));
            }
        }
        s.sort_by_key(|(_, off, _)| *off);
        for w in s.windows(2) {
            let (_, off_a, len_a) = w[0];
            let (_, off_b, _) = w[1];
            if off_a + len_a > off_b {
                return Err(Error::BadGeometry("two sections of the control region overlap"));
            }
        }
        if self.used > self.geometry.control_size {
            return Err(Error::BadGeometry("the layout does not fit in control_size"));
        }
        Ok(())
    }

    /// The directory entry of ring `slot`.
    pub fn ring_dir_entry(&self, slot: u32) -> Result<u64> {
        if slot >= self.geometry.ring_slot_count {
            return Err(Error::NoSuchRing { ring: slot });
        }
        Ok(self.ring_dir_offset + u64::from(slot) * RING_DIR_ENTRY_SIZE)
    }

    /// The control block of ring `slot`.
    pub fn ring_ctrl(&self, slot: u32) -> Result<u64> {
        if slot >= self.geometry.ring_slot_count {
            return Err(Error::NoSuchRing { ring: slot });
        }
        Ok(self.ring_ctrl_offset + u64::from(slot) * RING_CTRL_SIZE)
    }

    /// The command and reply data buffers of ring `slot`, as `(offset, len)`
    /// each.
    pub fn ring_data(&self, slot: u32) -> Result<((u64, u32), (u64, u32))> {
        if slot >= self.geometry.ring_slot_count {
            return Err(Error::NoSuchRing { ring: slot });
        }
        let n = u64::from(self.geometry.ring_data_bytes);
        let base = self.ring_data_offset + u64::from(slot) * 2 * n;
        Ok((
            (base, self.geometry.ring_data_bytes),
            (base + n, self.geometry.ring_data_bytes),
        ))
    }

    pub fn slab_dir_entry(&self, index: u32) -> Result<u64> {
        if index >= self.slab_dir_count {
            return Err(Error::BadGeometry("slab index outside the directory"));
        }
        Ok(self.slab_dir_offset + u64::from(index) * SLAB_DIR_ENTRY_SIZE)
    }

    pub fn timeline_entry(&self, index: u32) -> Result<u64> {
        if index >= self.geometry.timeline_count {
            return Err(Error::BadGeometry("timeline slot outside the table"));
        }
        Ok(self.timeline_dir_offset + u64::from(index) * TIMELINE_ENTRY_SIZE)
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// What the two sides have to agree about before anything else happens.
///
/// `protocol_hash` comes from `sv_proto::PROTOCOL_HASH`, which the generator
/// computed over the manifest it emitted (design.md §4.3, §6.2 deviation 2).
/// This crate does not depend on `sv-proto` — the transport must be testable
/// without a 126,000-line dependency — so the value is passed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolIds {
    pub layout_version: u32,
    pub wire_version: u32,
    pub protocol_hash: u64,
}

impl ProtocolIds {
    pub fn new(wire_version: u32, protocol_hash: u64) -> Self {
        ProtocolIds { layout_version: LAYOUT_VERSION, wire_version, protocol_hash }
    }
}

/// Which side of the region this process is.
///
/// design.md D20: the side inside the VM owns and publishes; the other side
/// attaches. Nothing below §5 assumes which of the two holds the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Peer,
}

// ---------------------------------------------------------------------------
// The control region
// ---------------------------------------------------------------------------

/// A mapped, validated control region.
#[derive(Debug)]
pub struct Control {
    region: Arc<SharedRegion>,
    layout: Layout,
    generation: u32,
    role: Role,
    /// This side's heartbeat counter. Owned here rather than by the caller so
    /// that every place which needs to say "still alive" — including
    /// [`crate::park::wait_until`], which is the one that matters — can do so
    /// without threading a tick through its signature.
    beat: core::sync::atomic::AtomicU64,
}

impl Control {
    /// Create and publish, as the owner. design.md §4.3 step 1.
    ///
    /// Order matters and is the whole of the step: zero everything, write every
    /// field, set `READY`, and only then store `magic` with `Release`. A peer
    /// that acquire-loads a non-zero `magic` has, by that release/acquire pair,
    /// seen every prior store.
    pub fn create(
        region: Arc<SharedRegion>,
        geometry: Geometry,
        ids: ProtocolIds,
        generation: u32,
    ) -> Result<Control> {
        let layout = Layout::compute(geometry)?;
        if (region.len() as u64) < geometry.control_size {
            return Err(Error::RegionTooSmall {
                needed: geometry.control_size,
                mapped: region.len(),
            });
        }

        // Zero first: a re-initialisation over a used region must not leave one
        // ring's stale head behind another's fresh tail.
        region.fill(0, geometry.control_size, 0)?;
        region.atomic_u32(OFF_LAYOUT_VERSION)?.store(ids.layout_version, Ordering::Relaxed);
        region.atomic_u32(OFF_WIRE_VERSION)?.store(ids.wire_version, Ordering::Relaxed);
        region.atomic_u64(OFF_PROTOCOL_HASH)?.store(ids.protocol_hash, Ordering::Relaxed);
        region.atomic_u32(OFF_GENERATION)?.store(generation, Ordering::Relaxed);
        region.atomic_u64(OFF_CONTROL_SIZE)?.store(geometry.control_size, Ordering::Relaxed);
        region.atomic_u64(OFF_ARENA_SIZE)?.store(geometry.arena_size, Ordering::Relaxed);
        region
            .atomic_u64(OFF_ARENA_SLAB_SIZE)?
            .store(geometry.arena_slab_size, Ordering::Relaxed);
        region
            .atomic_u32(OFF_RING_SLOT_COUNT)?
            .store(geometry.ring_slot_count, Ordering::Relaxed);
        region.atomic_u32(OFF_RING_DATA_BYTES)?.store(geometry.ring_data_bytes, Ordering::Relaxed);
        region.atomic_u64(OFF_RING_DIR_OFFSET)?.store(layout.ring_dir_offset, Ordering::Relaxed);
        region.atomic_u64(OFF_SLAB_DIR_OFFSET)?.store(layout.slab_dir_offset, Ordering::Relaxed);
        region
            .atomic_u64(OFF_TIMELINE_DIR_OFFSET)?
            .store(layout.timeline_dir_offset, Ordering::Relaxed);
        region
            .atomic_u64(OFF_REVERSE_RING_OFFSET)?
            .store(layout.reverse_ring_offset, Ordering::Relaxed);
        region.atomic_u32(OFF_SLAB_DIR_COUNT)?.store(layout.slab_dir_count, Ordering::Relaxed);
        region
            .atomic_u32(OFF_TIMELINE_COUNT)?
            .store(geometry.timeline_count, Ordering::Relaxed);
        region.atomic_u64(OFF_RING_CTRL_OFFSET)?.store(layout.ring_ctrl_offset, Ordering::Relaxed);
        region.atomic_u64(OFF_RING_DATA_OFFSET)?.store(layout.ring_data_offset, Ordering::Relaxed);

        let ctl = Control {
            region,
            layout,
            generation,
            role: Role::Owner,
            beat: core::sync::atomic::AtomicU64::new(0),
        };
        ctl.write_ring_directory()?;
        // READY before magic, magic last. Both are checked on attach.
        ctl.region.atomic_u32(OFF_FLAGS)?.store(Flags::READY, Ordering::Release);
        ctl.region.atomic_u64(OFF_MAGIC)?.store(MAGIC, Ordering::Release);
        Ok(ctl)
    }

    /// Re-initialise in place, bumping the generation. design.md §4.3 step 1's
    /// second sentence.
    ///
    /// Zeroing `magic` *first* is what makes this safe against a peer that is
    /// mid-handshake: it sees `NotReady` rather than a half-rewritten header.
    pub fn reinitialise(self) -> Result<Control> {
        let region = Arc::clone(&self.region);
        let geometry = self.layout.geometry;
        let ids = self.protocol_ids()?;
        let next = self.generation.wrapping_add(1);
        region.atomic_u64(OFF_MAGIC)?.store(0, Ordering::Release);
        region.atomic_u32(OFF_GENERATION)?.store(next, Ordering::Release);
        drop(self);
        Control::create(region, geometry, ids, next)
    }

    /// Attach as the peer. design.md §4.3 step 4: check in this order and
    /// refuse with a named error on any mismatch.
    ///
    /// The order is the design's, and it is the useful order: the cheapest and
    /// most diagnostic checks first, so that the error a person sees is the
    /// first thing that is actually wrong rather than a downstream consequence.
    pub fn attach(region: Arc<SharedRegion>, ids: ProtocolIds) -> Result<Control> {
        let magic = region.atomic_u64(OFF_MAGIC)?.load(Ordering::Acquire);
        if magic == 0 {
            return Err(Error::NotReady);
        }
        if magic != MAGIC {
            return Err(Error::MagicMismatch { found: magic });
        }
        // The flags word is read *after* the acquire load of magic, so it is
        // ordered; a `magic` without `READY` means the word was clobbered, which
        // is precisely sharedgl's failure and is worth refusing over.
        let flags = region.atomic_u32(OFF_FLAGS)?.load(Ordering::Acquire);
        if flags & Flags::READY == 0 {
            return Err(Error::NotReady);
        }

        let layout_version = region.atomic_u32(OFF_LAYOUT_VERSION)?.load(Ordering::Relaxed);
        if layout_version != ids.layout_version {
            return Err(Error::LayoutVersionMismatch {
                ours: ids.layout_version,
                theirs: layout_version,
            });
        }
        let wire_version = region.atomic_u32(OFF_WIRE_VERSION)?.load(Ordering::Relaxed);
        if wire_version != ids.wire_version {
            return Err(Error::WireVersionMismatch { ours: ids.wire_version, theirs: wire_version });
        }
        let hash = region.atomic_u64(OFF_PROTOCOL_HASH)?.load(Ordering::Relaxed);
        if hash != ids.protocol_hash {
            return Err(Error::ProtocolHashMismatch { ours: ids.protocol_hash, theirs: hash });
        }

        let geometry = Geometry {
            control_size: region.atomic_u64(OFF_CONTROL_SIZE)?.load(Ordering::Relaxed),
            arena_size: region.atomic_u64(OFF_ARENA_SIZE)?.load(Ordering::Relaxed),
            arena_slab_size: region.atomic_u64(OFF_ARENA_SLAB_SIZE)?.load(Ordering::Relaxed),
            ring_slot_count: region.atomic_u32(OFF_RING_SLOT_COUNT)?.load(Ordering::Relaxed),
            ring_data_bytes: region.atomic_u32(OFF_RING_DATA_BYTES)?.load(Ordering::Relaxed),
            timeline_count: region.atomic_u32(OFF_TIMELINE_COUNT)?.load(Ordering::Relaxed),
        };
        if geometry.control_size > region.len() as u64 {
            return Err(Error::RegionTooSmall {
                needed: geometry.control_size,
                mapped: region.len(),
            });
        }

        // Recompute rather than trust the published offsets, then check that
        // the two agree. Trusting them would make a corrupt header into an
        // out-of-bounds access; recomputing without comparing would hide a
        // genuine version skew. Doing both catches each other's failure mode.
        let layout = Layout::compute(geometry)?;
        let published = [
            (layout.ring_dir_offset, region.atomic_u64(OFF_RING_DIR_OFFSET)?),
            (layout.slab_dir_offset, region.atomic_u64(OFF_SLAB_DIR_OFFSET)?),
            (layout.timeline_dir_offset, region.atomic_u64(OFF_TIMELINE_DIR_OFFSET)?),
            (layout.reverse_ring_offset, region.atomic_u64(OFF_REVERSE_RING_OFFSET)?),
            (layout.ring_ctrl_offset, region.atomic_u64(OFF_RING_CTRL_OFFSET)?),
            (layout.ring_data_offset, region.atomic_u64(OFF_RING_DATA_OFFSET)?),
        ];
        for (ours, theirs) in published {
            if ours != theirs.load(Ordering::Relaxed) {
                return Err(Error::BadGeometry("the published layout is not the computed one"));
            }
        }

        let generation = region.atomic_u32(OFF_GENERATION)?.load(Ordering::Acquire);
        Ok(Control {
            region,
            layout,
            generation,
            role: Role::Peer,
            beat: core::sync::atomic::AtomicU64::new(0),
        })
    }

    fn protocol_ids(&self) -> Result<ProtocolIds> {
        Ok(ProtocolIds {
            layout_version: self.region.atomic_u32(OFF_LAYOUT_VERSION)?.load(Ordering::Relaxed),
            wire_version: self.region.atomic_u32(OFF_WIRE_VERSION)?.load(Ordering::Relaxed),
            protocol_hash: self.region.atomic_u64(OFF_PROTOCOL_HASH)?.load(Ordering::Relaxed),
        })
    }

    /// Write the geometry-derived fields of every directory entry.
    ///
    /// Done once, by the owner, so that claiming a ring is a state transition
    /// and never an allocation. The attaching side then *reads* `cmd_off` and
    /// `cmd_len` and bounds-checks them (R2) rather than computing them, which
    /// is the same discipline §4.3 applies to the section offsets.
    fn write_ring_directory(&self) -> Result<()> {
        for slot in 0..self.layout.geometry.ring_slot_count {
            let e = self.layout.ring_dir_entry(slot)?;
            let ((cmd_off, cmd_len), (reply_off, reply_len)) = self.layout.ring_data(slot)?;
            let ctrl = self.layout.ring_ctrl(slot)?;
            self.region
                .atomic_u32(e + RD_CMD_OFF)?
                .store(u32::try_from(cmd_off).unwrap_or(u32::MAX), Ordering::Relaxed);
            self.region.atomic_u32(e + RD_CMD_LEN)?.store(cmd_len, Ordering::Relaxed);
            self.region
                .atomic_u32(e + RD_REPLY_OFF)?
                .store(u32::try_from(reply_off).unwrap_or(u32::MAX), Ordering::Relaxed);
            self.region.atomic_u32(e + RD_REPLY_LEN)?.store(reply_len, Ordering::Relaxed);
            self.region
                .atomic_u32(e + RD_CTRL_OFF)?
                .store(u32::try_from(ctrl).unwrap_or(u32::MAX), Ordering::Relaxed);
            self.region.atomic_u32(e + RD_DEPENDS_RING)?.store(u32::MAX, Ordering::Relaxed);
        }
        // The reverse ring exists from the first instant the region does
        // (design.md §4.6), so slot 0 is claimed here rather than negotiated.
        self.set_ring(REVERSE_RING_SLOT, RingKind::Reverse, 0, None)?;
        Ok(())
    }

    fn set_ring(
        &self,
        slot: u32,
        kind: RingKind,
        owner_id: u64,
        depends: Option<u32>,
    ) -> Result<()> {
        let e = self.layout.ring_dir_entry(slot)?;
        self.region.atomic_u32(e + RD_KIND)?.store(kind as u32, Ordering::Relaxed);
        self.region.atomic_u64(e + RD_OWNER_ID)?.store(owner_id, Ordering::Relaxed);
        self.region
            .atomic_u32(e + RD_DEPENDS_RING)?
            .store(depends.unwrap_or(u32::MAX), Ordering::Relaxed);
        self.region.atomic_u32(e + RD_STATE)?.store(RingState::Live as u32, Ordering::Release);
        Ok(())
    }

    /// Take the first free directory slot for a ring of `kind`.
    ///
    /// design.md §4.3 step 5: the peer claims rings. The claim is a CAS from
    /// `FREE` to `CLAIMED`, so two threads racing for the last slot produce one
    /// winner and one [`Error::NoFreeRing`] rather than one silent aliasing.
    pub fn claim_ring(
        &self,
        kind: RingKind,
        owner_id: u64,
        depends: Option<u32>,
    ) -> Result<u32> {
        if let Some(d) = depends
            && d >= self.layout.geometry.ring_slot_count
        {
            return Err(Error::NoSuchRing { ring: d });
        }
        for slot in 0..self.layout.geometry.ring_slot_count {
            let e = self.layout.ring_dir_entry(slot)?;
            let state = self.region.atomic_u32(e + RD_STATE)?;
            if state
                .compare_exchange(
                    RingState::Free as u32,
                    RingState::Claimed as u32,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                self.set_ring(slot, kind, owner_id, depends)?;
                return Ok(slot);
            }
        }
        Err(Error::NoFreeRing)
    }

    /// Mark a ring dead. Its buffers are not reused within a generation: a slot
    /// that has been `DEAD` stays `DEAD` until the region is re-initialised,
    /// because the alternative is deciding when the peer has stopped reading a
    /// buffer, which is the one lifetime race with no cheap answer (§5.2's
    /// "slabs are never unimported while the process lives", same argument).
    pub fn kill_ring(&self, slot: u32) -> Result<()> {
        let e = self.layout.ring_dir_entry(slot)?;
        self.region.atomic_u32(e + RD_STATE)?.store(RingState::Dead as u32, Ordering::Release);
        Ok(())
    }

    pub fn ring_state(&self, slot: u32) -> Result<RingState> {
        let e = self.layout.ring_dir_entry(slot)?;
        RingState::from_u32(self.region.atomic_u32(e + RD_STATE)?.load(Ordering::Acquire))
            .ok_or(Error::NoSuchRing { ring: slot })
    }

    pub fn ring_kind(&self, slot: u32) -> Result<RingKind> {
        let e = self.layout.ring_dir_entry(slot)?;
        RingKind::from_u32(self.region.atomic_u32(e + RD_KIND)?.load(Ordering::Acquire))
            .ok_or(Error::NoSuchRing { ring: slot })
    }

    /// The device ring a queue ring orders against (design.md §4.4's
    /// `depends_ring`, and the target of every barrier that queue emits).
    pub fn ring_depends(&self, slot: u32) -> Result<Option<u32>> {
        let e = self.layout.ring_dir_entry(slot)?;
        let d = self.region.atomic_u32(e + RD_DEPENDS_RING)?.load(Ordering::Acquire);
        Ok(if d == u32::MAX { None } else { Some(d) })
    }

    /// The command and reply buffers of a ring, **read from the directory and
    /// bounds-checked**, not recomputed (R2).
    pub fn ring_buffers(&self, slot: u32) -> Result<((u64, u32), (u64, u32))> {
        let e = self.layout.ring_dir_entry(slot)?;
        let cmd_off = u64::from(self.region.atomic_u32(e + RD_CMD_OFF)?.load(Ordering::Acquire));
        let cmd_len = self.region.atomic_u32(e + RD_CMD_LEN)?.load(Ordering::Acquire);
        let reply_off =
            u64::from(self.region.atomic_u32(e + RD_REPLY_OFF)?.load(Ordering::Acquire));
        let reply_len = self.region.atomic_u32(e + RD_REPLY_LEN)?.load(Ordering::Acquire);
        self.region.check(cmd_off, u64::from(cmd_len))?;
        self.region.check(reply_off, u64::from(reply_len))?;
        if !cmd_len.is_power_of_two() || !reply_len.is_power_of_two() {
            return Err(Error::BadGeometry("a ring buffer length is not a power of two"));
        }
        Ok(((cmd_off, cmd_len), (reply_off, reply_len)))
    }

    pub fn ring_ctrl_offset(&self, slot: u32) -> Result<u64> {
        let e = self.layout.ring_dir_entry(slot)?;
        let off = u64::from(self.region.atomic_u32(e + RD_CTRL_OFF)?.load(Ordering::Acquire));
        self.region.check(off, RING_CTRL_SIZE)?;
        Ok(off)
    }

    pub fn region(&self) -> &Arc<SharedRegion> {
        &self.region
    }
    pub fn layout(&self) -> &Layout {
        &self.layout
    }
    pub fn generation(&self) -> u32 {
        self.generation
    }
    pub fn role(&self) -> Role {
        self.role
    }

    /// Read the live generation and compare it against the one this handle was
    /// made under. design.md §4.3: "Every ring operation carries the generation
    /// it was created under; a mismatch means the peer restarted."
    pub fn check_generation(&self) -> Result<()> {
        let now = self.region.atomic_u32(OFF_GENERATION)?.load(Ordering::Acquire);
        if now != self.generation {
            return Err(Error::PeerRestarted { expected: self.generation, found: now });
        }
        Ok(())
    }

    /// Publish a heartbeat, allocating the tick.
    ///
    /// **Every wait must call this, not only the caller's main loop.** If a side
    /// stops beating while it is blocked, then two peers that block at the same
    /// moment — which is the normal state of a request/response protocol — each
    /// watch the other's heartbeat freeze and each conclude the other is gone.
    /// That is a mutual false positive, it is reachable in ordinary operation
    /// rather than under a fault, and it was reproduced by running the two sides
    /// in separate containers where the timing is slower than in one process.
    /// design.md §4.2 says the heartbeat is "updated at least every 100 ms" and
    /// §4.5 says an expired wait consults it, but neither says who beats while
    /// you are waiting; [`crate::park::wait_until`] does.
    /// **Does not read the clock.** `owner_epoch_ns`/`peer_epoch_ns` are
    /// "diagnostics only" (§4.2), and this runs on every slice of every wait —
    /// a `clock_gettime(CLOCK_REALTIME)` there would be a syscall on the
    /// liveness path to populate a field nothing reads. Call
    /// [`Control::stamp_epoch`] once at handshake instead. It is also what makes
    /// this testable under Miri, which refuses a real-time clock under isolation
    /// and is right to.
    pub fn beat_now(&self) -> Result<()> {
        let tick = self.beat.fetch_add(1, Ordering::Relaxed) + 1;
        let (hb, _) = self.heartbeat_offsets();
        self.region.atomic_u64(hb)?.store(tick, Ordering::Release);
        Ok(())
    }

    /// Record this side's wall clock, for diagnosing skew between the two.
    /// Called at handshake, not on the heartbeat path.
    pub fn stamp_epoch(&self) -> Result<()> {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let (_, ep) = self.heartbeat_offsets();
        self.region.atomic_u64(ep)?.store(ns, Ordering::Relaxed);
        Ok(())
    }

    fn heartbeat_offsets(&self) -> (u64, u64) {
        match self.role {
            Role::Owner => (OFF_OWNER_HEARTBEAT, OFF_OWNER_EPOCH_NS),
            Role::Peer => (OFF_PEER_HEARTBEAT, OFF_PEER_EPOCH_NS),
        }
    }

    /// Publish a heartbeat with a caller-chosen tick. design.md §4.2: at least
    /// every 100 ms.
    pub fn beat(&self, tick: u64, epoch_ns: u64) -> Result<()> {
        let (hb, ep) = match self.role {
            Role::Owner => (OFF_OWNER_HEARTBEAT, OFF_OWNER_EPOCH_NS),
            Role::Peer => (OFF_PEER_HEARTBEAT, OFF_PEER_EPOCH_NS),
        };
        self.region.atomic_u64(ep)?.store(epoch_ns, Ordering::Relaxed);
        self.region.atomic_u64(hb)?.store(tick, Ordering::Release);
        Ok(())
    }

    /// The other side's heartbeat.
    pub fn peer_beat(&self) -> Result<u64> {
        let off = match self.role {
            Role::Owner => OFF_PEER_HEARTBEAT,
            Role::Peer => OFF_OWNER_HEARTBEAT,
        };
        Ok(self.region.atomic_u64(off)?.load(Ordering::Acquire))
    }

    pub fn flags(&self) -> Result<u32> {
        Ok(self.region.atomic_u32(OFF_FLAGS)?.load(Ordering::Acquire))
    }

    pub fn set_flag(&self, flag: u32) -> Result<()> {
        self.region.atomic_u32(OFF_FLAGS)?.fetch_or(flag, Ordering::AcqRel);
        Ok(())
    }

    // -- slab directory ------------------------------------------------------

    /// Publish a slab's geometry. design.md §4.2: "Written by the app side when
    /// it requests a slab, completed by the GPU side when the import succeeds."
    pub fn write_slab(&self, index: u32, arena_offset: u64, size: u64) -> Result<()> {
        let e = self.layout.slab_dir_entry(index)?;
        self.region.atomic_u64(e + SD_ARENA_OFFSET)?.store(arena_offset, Ordering::Relaxed);
        self.region.atomic_u64(e + SD_SIZE)?.store(size, Ordering::Relaxed);
        self.region
            .atomic_u32(e + SD_CACHE_CLASS)?
            .store(CacheClass::WriteBack as u32, Ordering::Relaxed);
        self.region.atomic_u32(e + SD_STATE)?.store(1, Ordering::Release);
        Ok(())
    }

    /// The GPU side's half: the memory type the import actually used.
    pub fn complete_slab_import(&self, index: u32, memory_type: u32) -> Result<()> {
        let e = self.layout.slab_dir_entry(index)?;
        self.region.atomic_u32(e + SD_GPU_MEMORY_TYPE)?.store(memory_type, Ordering::Relaxed);
        self.region.atomic_u32(e + SD_IMPORT_STATE)?.store(1, Ordering::Release);
        Ok(())
    }

    pub fn slab_import_state(&self, index: u32) -> Result<(u32, u32)> {
        let e = self.layout.slab_dir_entry(index)?;
        Ok((
            self.region.atomic_u32(e + SD_IMPORT_STATE)?.load(Ordering::Acquire),
            self.region.atomic_u32(e + SD_GPU_MEMORY_TYPE)?.load(Ordering::Relaxed),
        ))
    }

    pub fn slab_refcount_add(&self, index: u32, delta: i32) -> Result<u32> {
        let e = self.layout.slab_dir_entry(index)?;
        let a = self.region.atomic_u32(e + SD_REFCOUNT)?;
        Ok(if delta >= 0 {
            a.fetch_add(delta as u32, Ordering::AcqRel)
        } else {
            a.fetch_sub((-delta) as u32, Ordering::AcqRel)
        })
    }

    // -- timeline mirror -----------------------------------------------------

    /// Bind a semaphore id to a mirror slot (`svBindTimelineMirror`, §6.3).
    pub fn bind_timeline(&self, slot: u32, semaphore_id: u64) -> Result<()> {
        let e = self.layout.timeline_entry(slot)?;
        self.region.atomic_u64(e + TL_VALUE)?.store(0, Ordering::Relaxed);
        self.region.atomic_u64(e + TL_SEMAPHORE_ID)?.store(semaphore_id, Ordering::Release);
        Ok(())
    }

    /// The GPU side stores the value with a release store after every signal.
    pub fn signal_timeline(&self, slot: u32, value: u64) -> Result<()> {
        let e = self.layout.timeline_entry(slot)?;
        self.region.atomic_u64(e + TL_VALUE)?.store(value, Ordering::Release);
        Ok(())
    }

    /// …so that an app-side `vkGetSemaphoreCounterValue` is a local acquire
    /// load and not a round trip (design.md §4.2, fwd §2.9).
    pub fn timeline_value(&self, slot: u32) -> Result<u64> {
        let e = self.layout.timeline_entry(slot)?;
        Ok(self.region.atomic_u64(e + TL_VALUE)?.load(Ordering::Acquire))
    }

    pub fn timeline_semaphore(&self, slot: u32) -> Result<u64> {
        let e = self.layout.timeline_entry(slot)?;
        Ok(self.region.atomic_u64(e + TL_SEMAPHORE_ID)?.load(Ordering::Acquire))
    }

    pub fn timeline_waiters_add(&self, slot: u32, delta: i32) -> Result<()> {
        let e = self.layout.timeline_entry(slot)?;
        let a = self.region.atomic_u32(e + TL_WAITER_COUNT)?;
        if delta >= 0 {
            a.fetch_add(delta as u32, Ordering::AcqRel);
        } else {
            a.fetch_sub((-delta) as u32, Ordering::AcqRel);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

/// What a timed-out wait concluded. design.md §4.5: "A wait that expires checks
/// the peer heartbeat and the generation counter before deciding between 'slow'
/// and 'gone'."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The heartbeat moved since we last looked.
    Alive,
    /// It has not moved, but not for long enough to call it.
    Slow,
    /// It has not moved for longer than the timeout.
    Gone,
    /// The generation changed: not slow, not gone — *restarted*.
    Restarted { generation: u32 },
}

/// Watches one peer's heartbeat.
///
/// Deliberately takes `now` rather than reading the clock, so the timeout logic
/// is testable without sleeping. A test that has to sleep to exercise a timeout
/// is a test that will be flaky on a loaded machine, and this crate's tests run
/// under Miri, where sleeping is worse than useless.
///
/// # Shared memory cannot tell "descheduled" from "exited". Nothing here can.
///
/// It is tempting to look for a better signal than elapsed time — the intuition
/// being that a descheduled peer is still mapped and still has a consistent
/// header, while an exited one does not. **That intuition is wrong, and it is
/// worth writing down because it is the first idea everybody has.** A shared
/// mapping outlives the process that made it. A peer that has exited leaves the
/// region byte-for-byte as it was: the header is still valid, the ring still
/// consistent, `parked` still whatever it last was. A peer that is merely
/// starved leaves exactly the same thing. **The two are indistinguishable by
/// reading the region**, and any scheme that claims otherwise is a timeout
/// wearing a disguise.
///
/// So the honest options are out-of-band, and there are two:
///
/// 1. **The connection closes.** When a process exits, the OS closes its cold
///    path — Unix socket, TCP, virtio-serial — and the peer's read returns EOF.
///    That is an *event*, not a threshold: load-independent, immediate, and
///    already relied on elsewhere (`sv-gpu-server serve` moves the socket into
///    the session thread precisely so that closing it is how the client learns
///    the GPU side went away). It is **sufficient but not necessary**: it
///    catches exit and crash, and says nothing about a process that is alive
///    and wedged.
/// 2. **Ask the kernel about the process.** Same machine only, and it needs a
///    pid on the wire, which §1a's naming discipline exists to avoid.
///
/// **The design that follows** is that a timeout is a *backstop for the wedged
/// case*, not the primary detector — the primary detector should be (1), and
/// wiring it into this type is the real fix. A backstop is allowed to be
/// generous, because the case it catches is rare and the cost of being wrong
/// about it is a live client dropped mid-session.
///
/// # Until then: the budget is *running* time, not wall time
///
/// The threshold was pure elapsed wall time and it chose "gone" over "slow" for
/// a healthy peer at load 133 — reproduced here at load 80, two failures in ten,
/// where the waiting side declared its watcher `PeerGone` instead of timing out.
/// That is not a flaky test. **It is the production failure mode**: on a loaded
/// host a live client is dropped, and the symptom is an unexplained disconnect.
///
/// Wall time is the wrong denominator because it advances while nobody runs. So
/// this monitor credits back the time it can prove *it* was starved — a poll
/// that should have happened every `cadence` and happened ten times later is
/// evidence about this thread's scheduling, not about the peer, on a machine
/// where the peer was almost certainly starved too. What remains is a budget of
/// time during which this side was running and *could* have seen a beat.
///
/// That is not a number tuned to make a test pass; it is the same threshold
/// measured against a better clock. And it degrades the right way: on an idle
/// machine the credit is zero and the behaviour is exactly what it was.
pub struct LivenessMonitor {
    last_value: u64,
    last_change: Instant,
    timeout: Duration,
    /// How often the caller intends to call [`LivenessMonitor::poll`].
    cadence: Duration,
    /// When it last actually did.
    last_poll: Instant,
    /// Wall time during which **this** side demonstrably was not running.
    ///
    /// See the type's own documentation: this is what makes the timeout a
    /// budget of *running* time rather than of wall time.
    stall_credit: Duration,
}

impl LivenessMonitor {
    /// `cadence` is how often the caller promises to poll. It is not a timer —
    /// nothing schedules from it — it is the yardstick against which an
    /// *overrun* is recognised, and the overrun is the evidence. Pass the same
    /// interval the caller's loop is built around.
    pub fn new(now: Instant, timeout: Duration, cadence: Duration) -> Self {
        LivenessMonitor {
            last_value: 0,
            last_change: now,
            timeout,
            cadence,
            last_poll: now,
            stall_credit: Duration::ZERO,
        }
    }

    /// How much of the budget has been returned because this side was starved.
    /// Diagnostics, and the number to print when a peer is declared gone.
    pub fn stall_credit(&self) -> Duration {
        self.stall_credit
    }

    pub fn poll(&mut self, control: &Control, now: Instant) -> Result<Liveness> {
        // **Credit this side's own starvation before judging the peer.**
        //
        // A poll that was meant to happen every `cadence` and actually happened
        // after ten times that is not evidence about the peer; it is evidence
        // that *this* thread was not scheduled, on a machine where the peer
        // almost certainly was not either. Wall time advanced for both of them
        // and neither got to run.
        //
        // The excess over twice the cadence is added back to the budget, so what
        // the timeout measures is time during which this side was actually
        // running and *could* have noticed a beat. Twice rather than once
        // because ordinary jitter is not starvation and should not inflate
        // anything.
        let gap = now.saturating_duration_since(self.last_poll);
        self.last_poll = now;
        if let Some(excess) = gap.checked_sub(self.cadence.saturating_mul(2)) {
            self.stall_credit += excess;
        }

        let gen_now = control.region.atomic_u32(OFF_GENERATION)?.load(Ordering::Acquire);
        if gen_now != control.generation {
            return Ok(Liveness::Restarted { generation: gen_now });
        }
        let beat = control.peer_beat()?;
        if beat != self.last_value {
            self.last_value = beat;
            self.last_change = now;
            // A peer that spoke proves the budget was not needed. Forgetting the
            // credit here keeps it from accumulating across a long healthy
            // session into a number that would never declare anything gone.
            self.stall_credit = Duration::ZERO;
            return Ok(Liveness::Alive);
        }
        Ok(if now.duration_since(self.last_change) >= self.timeout + self.stall_credit {
            Liveness::Gone
        } else {
            Liveness::Slow
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(g: Geometry) -> Arc<SharedRegion> {
        Arc::new(SharedRegion::heap(g.control_size as usize).unwrap())
    }

    fn ids() -> ProtocolIds {
        ProtocolIds::new(7, 0xfeed_face_dead_beef)
    }

    #[test]
    fn the_default_geometry_is_the_designs_and_fits_its_stated_size() {
        let g = Geometry::default();
        let l = Layout::compute(g).unwrap();
        // design.md §4.2: "4 KiB header + 4 KiB directory + 64 KiB timelines +
        // 64 × 4 KiB control + 64 × (64 + 64) KiB data ≈ 8.6 MiB, which is why
        // the region default is 16 MiB."
        assert!(l.used < 16 << 20, "the default layout must fit the default region");
        assert!(
            (8 << 20..9 << 20).contains(&l.used),
            "the design predicts ≈8.6 MiB; this build computes {} bytes",
            l.used
        );
        assert_eq!(l.slab_dir_count, 32, "2 GiB of 64 MiB slabs");
    }

    /// The anti-`SHM_SLOT_MASK` test, named after the bug it exists to prevent.
    #[test]
    fn no_two_sections_of_the_control_region_overlap() {
        for g in [Geometry::default(), Geometry::small()] {
            let l = Layout::compute(g).unwrap();
            let s = l.sections();
            assert!(!s.is_empty());
            // The pairwise check, written out rather than delegated to
            // `validate`, so that this test fails if `validate` is ever weakened.
            let mut found = 0usize;
            for (i, a) in s.iter().enumerate() {
                for b in s.iter().skip(i + 1) {
                    let overlap = a.1 < b.1 + b.2 && b.1 < a.1 + a.2;
                    assert!(!overlap, "{} overlaps {}", a.0, b.0);
                    found += 1;
                }
            }
            // A count of exactly zero comparisons would make the loop above a
            // no-op that passes, which is the shape of a false negative this
            // project has already been bitten by once.
            assert_eq!(found, s.len() * (s.len() - 1) / 2);
            assert!(found >= 15);
        }
    }

    #[test]
    fn a_layout_whose_sections_overlap_is_refused() {
        // Constructed by hand, because `compute` will not produce one — which is
        // the point: `validate` is a check on a value, so it can be tested with
        // a value `compute` would never return.
        let mut l = Layout::compute(Geometry::small()).unwrap();
        l.timeline_dir_offset = l.slab_dir_offset;
        assert_eq!(l.validate(), Err(Error::BadGeometry("two sections of the control region overlap")));
    }

    #[test]
    fn geometry_that_cannot_work_is_refused_at_compute_time() {
        let bad = |f: fn(&mut Geometry)| {
            let mut g = Geometry::small();
            f(&mut g);
            Layout::compute(g)
        };
        assert!(bad(|g| g.ring_slot_count = 0).is_err());
        assert!(bad(|g| g.arena_slab_size = 0).is_err());
        assert!(bad(|g| g.arena_slab_size = 4097).is_err());
        assert!(bad(|g| g.arena_size = 4097).is_err());
        assert!(bad(|g| g.ring_data_bytes = 3000).is_err(), "not a power of two");
        assert!(bad(|g| g.ring_data_bytes = 2048).is_err(), "smaller than a page");
        assert!(bad(|g| g.control_size = 4096).is_err(), "does not fit");
    }

    #[test]
    fn a_created_region_attaches() {
        let g = Geometry::small();
        let r = region(g);
        let owner = Control::create(Arc::clone(&r), g, ids(), 1).unwrap();
        let peer = Control::attach(Arc::clone(&r), ids()).unwrap();
        assert_eq!(peer.generation(), 1);
        assert_eq!(peer.layout(), owner.layout());
        assert_eq!(peer.role(), Role::Peer);
        assert_eq!(owner.role(), Role::Owner);
        // The reverse ring exists before anyone asked for one.
        assert_eq!(peer.ring_kind(REVERSE_RING_SLOT).unwrap(), RingKind::Reverse);
        assert_eq!(peer.ring_state(REVERSE_RING_SLOT).unwrap(), RingState::Live);
        assert_eq!(peer.layout().reverse_ring_offset, peer.ring_ctrl_offset(REVERSE_RING_SLOT).unwrap());
    }

    #[test]
    fn an_unpublished_region_is_not_ready_rather_than_garbage() {
        let g = Geometry::small();
        let r = region(g);
        assert_eq!(Control::attach(r, ids()).unwrap_err(), Error::NotReady);
    }

    /// sharedgl's bug, reproduced deliberately: something else writes the word
    /// the readiness flag lives in. The handshake must fail.
    #[test]
    fn a_ready_flag_that_is_aliased_away_fails_the_handshake() {
        let g = Geometry::small();
        let r = region(g);
        let _owner = Control::create(Arc::clone(&r), g, ids(), 1).unwrap();
        assert!(Control::attach(Arc::clone(&r), ids()).is_ok());
        // A mask at the same offset, which is exactly `SHM_SLOT_MASK` over
        // `READY_HINT`.
        r.atomic_u32(OFF_FLAGS).unwrap().store(0xFFFF_0000, Ordering::Release);
        assert_eq!(Control::attach(Arc::clone(&r), ids()).unwrap_err(), Error::NotReady);
    }

    #[test]
    fn the_handshake_refuses_in_the_order_the_design_gives() {
        let g = Geometry::small();
        let r = region(g);
        let _owner = Control::create(Arc::clone(&r), g, ids(), 1).unwrap();

        // magic
        r.atomic_u64(OFF_MAGIC).unwrap().store(MAGIC.swap_bytes(), Ordering::Release);
        assert!(matches!(
            Control::attach(Arc::clone(&r), ids()),
            Err(Error::MagicMismatch { .. })
        ));
        r.atomic_u64(OFF_MAGIC).unwrap().store(MAGIC, Ordering::Release);

        // layout_version before wire_version: break both, expect the first.
        r.atomic_u32(OFF_LAYOUT_VERSION).unwrap().store(99, Ordering::Relaxed);
        r.atomic_u32(OFF_WIRE_VERSION).unwrap().store(99, Ordering::Relaxed);
        assert_eq!(
            Control::attach(Arc::clone(&r), ids()).unwrap_err(),
            Error::LayoutVersionMismatch { ours: LAYOUT_VERSION, theirs: 99 }
        );
        r.atomic_u32(OFF_LAYOUT_VERSION).unwrap().store(LAYOUT_VERSION, Ordering::Relaxed);
        assert_eq!(
            Control::attach(Arc::clone(&r), ids()).unwrap_err(),
            Error::WireVersionMismatch { ours: 7, theirs: 99 }
        );
        r.atomic_u32(OFF_WIRE_VERSION).unwrap().store(7, Ordering::Relaxed);

        // protocol_hash — R13, the one that would otherwise desynchronise
        // silently rather than fail.
        r.atomic_u64(OFF_PROTOCOL_HASH).unwrap().store(1, Ordering::Relaxed);
        assert_eq!(
            Control::attach(Arc::clone(&r), ids()).unwrap_err(),
            Error::ProtocolHashMismatch { ours: 0xfeed_face_dead_beef, theirs: 1 }
        );
    }

    #[test]
    fn a_published_layout_that_disagrees_with_the_computed_one_is_refused() {
        let g = Geometry::small();
        let r = region(g);
        let _owner = Control::create(Arc::clone(&r), g, ids(), 1).unwrap();
        r.atomic_u64(OFF_RING_DIR_OFFSET).unwrap().store(0x10_0000, Ordering::Relaxed);
        assert_eq!(
            Control::attach(Arc::clone(&r), ids()).unwrap_err(),
            Error::BadGeometry("the published layout is not the computed one")
        );
    }

    #[test]
    fn a_region_smaller_than_its_header_claims_is_refused() {
        let g = Geometry::small();
        let big = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let _owner = Control::create(Arc::clone(&big), g, ids(), 1).unwrap();
        // Same bytes, a mapping half the size. This is the check design.md §4.3
        // step 4 ends with, and it is the one that stops a truncated attach from
        // reading a ring buffer that is not there.
        let mut bytes = vec![0u8; g.control_size as usize / 2];
        big.read_bytes(0, &mut bytes).unwrap();
        let small = Arc::new(SharedRegion::heap(bytes.len()).unwrap());
        small.write_bytes(0, &bytes).unwrap();
        assert!(matches!(
            Control::attach(small, ids()),
            Err(Error::RegionTooSmall { .. })
        ));
    }

    #[test]
    fn reinitialising_bumps_the_generation_and_a_stale_handle_notices() {
        let g = Geometry::small();
        let r = region(g);
        let owner = Control::create(Arc::clone(&r), g, ids(), 1).unwrap();
        let stale_peer = Control::attach(Arc::clone(&r), ids()).unwrap();
        stale_peer.check_generation().unwrap();

        let owner = owner.reinitialise().unwrap();
        assert_eq!(owner.generation(), 2);
        assert_eq!(
            stale_peer.check_generation().unwrap_err(),
            Error::PeerRestarted { expected: 1, found: 2 }
        );
        // …and a fresh attach is fine, which is what makes this a reconnect
        // mechanism rather than a way to lose a region.
        assert_eq!(Control::attach(Arc::clone(&r), ids()).unwrap().generation(), 2);
    }

    #[test]
    fn claiming_rings_hands_out_distinct_slots_and_then_runs_out() {
        let g = Geometry::small();
        let r = region(g);
        let owner = Control::create(Arc::clone(&r), g, ids(), 1).unwrap();
        let peer = Control::attach(Arc::clone(&r), ids()).unwrap();
        // Slot 0 is the reverse ring, so `ring_slot_count - 1` remain.
        let instance = peer.claim_ring(RingKind::Instance, 0, None).unwrap();
        let device = peer.claim_ring(RingKind::Device, 1, None).unwrap();
        let queue = peer.claim_ring(RingKind::Queue, 2, Some(device)).unwrap();
        assert_ne!(instance, device);
        assert_ne!(device, queue);
        assert_ne!(instance, REVERSE_RING_SLOT);
        assert_eq!(peer.ring_depends(queue).unwrap(), Some(device));
        assert_eq!(peer.ring_depends(device).unwrap(), None);
        assert_eq!(owner.ring_kind(queue).unwrap(), RingKind::Queue);
        assert_eq!(peer.claim_ring(RingKind::Queue, 3, None).unwrap_err(), Error::NoFreeRing);
        // A dependency on a ring that does not exist is refused, not stored.
        assert!(matches!(
            peer.claim_ring(RingKind::Queue, 4, Some(99)),
            Err(Error::NoSuchRing { ring: 99 })
        ));
    }

    #[test]
    fn every_rings_buffers_are_inside_the_region_and_disjoint() {
        let g = Geometry::default();
        let r = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let ctl = Control::create(r, g, ids(), 1).unwrap();
        let mut spans = Vec::new();
        for slot in 0..g.ring_slot_count {
            let ((c_off, c_len), (r_off, r_len)) = ctl.ring_buffers(slot).unwrap();
            spans.push((c_off, u64::from(c_len)));
            spans.push((r_off, u64::from(r_len)));
            spans.push((ctl.ring_ctrl_offset(slot).unwrap(), RING_CTRL_SIZE));
        }
        assert_eq!(spans.len() as u32, g.ring_slot_count * 3);
        spans.sort();
        for w in spans.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0, "{:?} overlaps {:?}", w[0], w[1]);
        }
        let last = spans.last().unwrap();
        assert!(last.0 + last.1 <= g.control_size);
    }

    #[test]
    fn heartbeats_go_in_opposite_directions() {
        let g = Geometry::small();
        let r = region(g);
        let owner = Control::create(Arc::clone(&r), g, ids(), 1).unwrap();
        let peer = Control::attach(Arc::clone(&r), ids()).unwrap();
        owner.beat(11, 1_000).unwrap();
        peer.beat(22, 2_000).unwrap();
        assert_eq!(owner.peer_beat().unwrap(), 22);
        assert_eq!(peer.peer_beat().unwrap(), 11);
    }

    #[test]
    fn liveness_distinguishes_slow_from_gone_from_restarted() {
        let g = Geometry::small();
        let r = region(g);
        let owner = Control::create(Arc::clone(&r), g, ids(), 1).unwrap();
        let peer = Control::attach(Arc::clone(&r), ids()).unwrap();
        let t0 = Instant::now();
        let mut mon = LivenessMonitor::new(t0, Duration::from_millis(500), Duration::from_millis(100));

        peer.beat(1, 0).unwrap();
        assert_eq!(mon.poll(&owner, t0).unwrap(), Liveness::Alive);
        // **Poll at the cadence this monitor was told to expect.** It credits
        // back time it can prove it was not running, and a caller that declares
        // a 100 ms cadence and then jumps 500 ms has, by that definition, not
        // been running for 300 ms of it — so the budget is extended and the
        // verdict is `Slow`. That is the monitor working, not failing, and a
        // test that skipped the intervening polls was asserting against a
        // caller the type does not model.
        for step in 1..=4 {
            let at = t0 + Duration::from_millis(100 * step);
            assert_eq!(mon.poll(&owner, at).unwrap(), Liveness::Slow, "at {step}00ms");
        }
        assert_eq!(mon.poll(&owner, t0 + Duration::from_millis(500)).unwrap(), Liveness::Gone);
        peer.beat(2, 0).unwrap();
        assert_eq!(mon.poll(&owner, t0 + Duration::from_millis(600)).unwrap(), Liveness::Alive);

        r.atomic_u32(OFF_GENERATION).unwrap().store(9, Ordering::Release);
        assert_eq!(
            mon.poll(&owner, t0 + Duration::from_millis(700)).unwrap(),
            Liveness::Restarted { generation: 9 }
        );
    }

    #[test]
    fn the_timeline_mirror_is_a_local_load() {
        let g = Geometry::small();
        let r = region(g);
        let owner = Control::create(Arc::clone(&r), g, ids(), 1).unwrap();
        let peer = Control::attach(Arc::clone(&r), ids()).unwrap();
        peer.bind_timeline(3, 0xabc).unwrap();
        assert_eq!(owner.timeline_semaphore(3).unwrap(), 0xabc);
        assert_eq!(peer.timeline_value(3).unwrap(), 0);
        owner.signal_timeline(3, 42).unwrap();
        assert_eq!(peer.timeline_value(3).unwrap(), 42);
        assert!(peer.bind_timeline(g.timeline_count, 1).is_err());
    }

    #[test]
    fn the_slab_directory_is_written_by_one_side_and_completed_by_the_other() {
        let g = Geometry::small();
        let r = region(g);
        let owner = Control::create(Arc::clone(&r), g, ids(), 1).unwrap();
        let peer = Control::attach(Arc::clone(&r), ids()).unwrap();
        peer.write_slab(2, 2 << 20, 1 << 20).unwrap();
        assert_eq!(owner.slab_import_state(2).unwrap(), (0, 0));
        owner.complete_slab_import(2, 1).unwrap();
        assert_eq!(peer.slab_import_state(2).unwrap(), (1, 1));
        assert!(peer.write_slab(999, 0, 0).is_err());
    }

    #[test]
    fn the_fatal_message_slot_fits_inside_a_ring_control_block() {
        const { assert!(RC_FATAL_MSG + RC_FATAL_MSG_LEN <= RING_CTRL_SIZE) };
        // Each of the design's six lines is on its own 64-byte cache line, which
        // is the whole reason the block is a page.
        for off in [RC_CMD_TAIL, RC_CMD_HEAD, RC_STATUS, RC_REPLY_TAIL, RC_REPLY_HEAD, RC_FATAL_CODE]
        {
            assert_eq!(off % 64, 0, "0x{off:x} is not on a cache-line boundary");
        }
        assert_ne!(RC_CMD_TAIL / 64, RC_CMD_HEAD / 64, "producer and consumer share a line");
    }
}

/// How stale does a **healthy** peer's heartbeat get on a loaded machine?
///
/// The backstop timeout has to sit above that, and this is the measurement that
/// sets it rather than a number chosen to make a test pass.
#[cfg(test)]
mod beat_staleness {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as AO};

    /// A peer beats as fast as `wait_until` does — once per liveness interval,
    /// 20 ms in the ICD's configuration. A watcher samples how long it has been
    /// since the value last changed. On an idle machine that is ~20 ms; the
    /// question is the *tail* when the machine is not idle, because that tail is
    /// what a backstop must clear.
    #[test]
    #[ignore = "a stopwatch: reports the scheduling tail, which is the machine's"]
    fn how_stale_a_healthy_heartbeat_gets() {
        let g = Geometry::small();
        let region = Arc::new(SharedRegion::heap(g.control_size as usize).unwrap());
        let ids = ProtocolIds::new(1, 1);
        let owner = Arc::new(Control::create(Arc::clone(&region), g, ids, 1).unwrap());
        let peer = Control::attach(region, ids).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let beater = {
            let owner = Arc::clone(&owner);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(AO::Relaxed) {
                    let _ = owner.beat_now();
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
        };

        let mut last = 0u64;
        let mut last_change = Instant::now();
        let mut worst = Duration::ZERO;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(6) {
            let b = peer.peer_beat().unwrap();
            let now = Instant::now();
            if b != last {
                last = b;
                last_change = now;
            } else {
                worst = worst.max(now.duration_since(last_change));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        stop.store(true, AO::Relaxed);
        beater.join().unwrap();
        println!(
            "healthy peer beating every 20ms: worst observed staleness {worst:?} \
             (load {})",
            std::fs::read_to_string("/proc/loadavg")
                .map(|s| s.split(' ').next().unwrap_or("?").to_string())
                .unwrap_or_else(|_| "?".into())
        );
    }
}
