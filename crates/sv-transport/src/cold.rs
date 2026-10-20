//! The cold path: the handshake messages, and the **name** handoff.
//!
//! design.md D10: "A socket: **virtio-serial** at first because it is already
//! attached, **vsock** once `viosock` is installed. Handshake, capability
//! negotiation, wakeups, teardown. **Never data.**" On the same machine it is a
//! Unix socket, which is what §13 steps 1–6 run over.
//!
//! # Nothing in this module passes a descriptor
//!
//! > **No file descriptor or handle ever crosses between the app side and the
//! > GPU side.** What crosses is a *name*, which the peer opens for itself.
//! > — docs/design.md §1a
//!
//! This module used to carry a `SCM_RIGHTS` pair, and every message it framed
//! was therefore a message only a Unix socket could deliver — which is a
//! same-machine mechanism wearing the cold path's clothes, since a virtio-serial
//! port or a vsock carries bytes and nothing else. It is now a byte stream in
//! the strict sense: every field of every message is a number or a string, and
//! [`Attachment::object`] is the name the peer opens with
//! [`crate::region::open_named`].
//!
//! The one descriptor handoff that survives is `sv_gpu_server`'s exported
//! dma-buf, which has no name to give; it lives in [`crate::localfd`], which
//! documents why it is still there and what measurement would remove it.
//!
//! # Why the locator descriptor rides here and not in the control region
//!
//! > **The locator descriptor must ride the cold path and cannot live in the
//! > control region**, for the obvious reason that the control region is one of
//! > the two things being located. — docs/design.md §4.3
//!
//! Hence [`ExtentDesc`] is a cold-path message type and lives in this crate
//! rather than in `sv-arena`. The split is worth stating once: `ExtentDesc`
//! describes *how to find* an extent and crosses the wire; `sv_arena::Extent`
//! and the run list describe *where it is* on one side and never do (§5.7,
//! "The run list does not cross the wire, and it never did have to").
//!
//! # `unsafe`
//!
//! None. This module is safe code over `sv-wire` and `std`, which is what
//! removing the ancillary-data path bought.

use crate::control::{Geometry, ProtocolIds, ENDIAN_MARKER};
use crate::{Error, Result};
use sv_wire::{Decoder, Encoder, pad4};

/// How the guest obtained the extent's pages. docs/design.md §4.3:
///
/// > **`granularity` is asserted, never assumed.** The GPU side calls
/// > `QueryWorkingSetEx` on the extent and publishes the `LargePage` bit it
/// > actually observed, not the flag it passed to `VirtualAlloc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Granularity {
    /// 4 KiB pages, `VirtualAlloc` + `VirtualLock`. Measured at 1471 runs per
    /// 64 MiB, and the path on which the R26 canary is required.
    Small4K = 0,
    /// 2 MiB large pages. Measured at 22 runs per 64 MiB and **1 per GiB**, and
    /// the path on which R26 is removed by construction.
    Large2M = 1,
}

impl Granularity {
    pub fn from_u32(v: u32) -> Option<Granularity> {
        match v {
            0 => Some(Granularity::Small4K),
            1 => Some(Granularity::Large2M),
            _ => None,
        }
    }

    /// Bytes in one allocation unit of this granularity.
    pub fn unit(self) -> u64 {
        match self {
            Granularity::Small4K => 4096,
            Granularity::Large2M => 2 << 20,
        }
    }
}

/// One run of physically contiguous frames, as the guest reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PfnRun {
    pub first_frame: u64,
    pub frames: u64,
}

impl PfnRun {
    /// The frame numbers this run stands for, in page order.
    ///
    /// The expansion exists because the *encoding* is run-length while the
    /// *meaning* is one frame per page of the extent, and the translating side
    /// needs the second form. Keeping the two spellings one function apart
    /// rather than re-deriving the arithmetic at each call site is what stops a
    /// frame list meaning two different things on the two sides of the wire.
    pub fn frame_numbers(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.frames).map(move |i| self.first_frame + i)
    }
}

/// How the attaching side is to find the extent. docs/design.md §5.1's table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locator {
    /// Sweep the memfd for the stamp. **v1 on every path**: 1.8 s at 64 MiB,
    /// 2.02 s at 1 GiB, 2.32 s at 2 GiB, because the cost is linear in guest RAM
    /// and not in arena size — and it is the only locator that *verifies* rather
    /// than trusts.
    Scan,
    /// One record every 2 MiB. Sound only on the large-page path. Recorded in
    /// the design, not implemented in v1.
    Probe,
    /// A reported frame list, run-length encoded.
    ///
    /// **No longer AWE-only** (docs/design.md §0.14, 2026-08-13). Windows reports the
    /// frame behind every page of any ordinary mapped window through
    /// `NtQuerySystemInformation(SystemSuperfetchInformation)` /
    /// `SuperfetchPfnQuery`, in ~330 ms whatever the window is, and — unlike
    /// [`Locator::Scan`] — *without the pages still containing the stamp*.
    /// Measured against the scan on the same 64 MiB window: byte-identical
    /// offset tables, 16384/16384 pages.
    ///
    /// It is the locator that **trusts** rather than verifies, which is why
    /// `security-model.md` R28 has the scan check it once, at attach, on the one
    /// occasion when the stamps still exist to check it against.
    Pfns(Vec<PfnRun>),
    /// **No locating at all** (waycompose-design.md §87): the extent is a
    /// contiguous slice of an ivshmem device's BAR, starting `region_offset`
    /// bytes into it, and the attaching side opens the file QEMU backs that BAR
    /// with — by its path, as the user, with a group bit — and reads the pages
    /// there. The stamps are still written and still checked, once, at attach,
    /// so this is a locator that names a place and is then verified, not one
    /// that is trusted.
    Ivshmem { region_offset: u64 },
    /// **A session of the leased pool** (waycompose-design.md §97): the control
    /// region is the `control_pages` pages at `control_offset` of the boot
    /// device, stamped and checked as [`Locator::Ivshmem`]'s slot was, and
    /// there is no arena in the extent at all — `arena_pages` is zero, and the
    /// arena is the whole pool, leased a range at a time over the rings.
    /// `pool_nonce` is the number the pool's owner wrote into the pool header
    /// at the start of the device; the attaching side checks it there, which
    /// is what says that the file it opened is the pool this session belongs
    /// to and not merely a file with stamps in it.
    IvshmemPool { control_offset: u64, pool_nonce: u64 },
}

impl Locator {
    /// Run-length encode a frame list into the wire form.
    ///
    /// A run continues only when consecutive *pages of the extent* sit on
    /// consecutive *frames*. That is stricter than "this frame is one more than
    /// the last", and the difference is not academic: frames handed out in
    /// reverse page order are contiguous by that looser test, would encode as
    /// one run, and would have the far side map the extent backwards — §1's
    /// invariant broken by a locator that looked like it worked.
    ///
    /// This is the same rule `awe_probe::superfetch::Located::runs` counts
    /// attach cost with, and a test in that crate holds the two to it.
    pub fn from_frames(frames: &[u64]) -> Locator {
        let mut runs: Vec<PfnRun> = Vec::new();
        for f in frames {
            match runs.last_mut() {
                Some(r) if r.first_frame + r.frames == *f => r.frames += 1,
                _ => runs.push(PfnRun { first_frame: *f, frames: 1 }),
            }
        }
        Locator::Pfns(runs)
    }

    /// The runs, if this is a frame-list locator.
    pub fn pfn_runs(&self) -> Option<&[PfnRun]> {
        match self {
            Locator::Pfns(runs) => Some(runs),
            _ => None,
        }
    }

    /// How many pages the carried frame list describes — zero on the locating
    /// paths, which carry no addresses at all.
    ///
    /// Saturating rather than wrapping, because this number exists to be
    /// compared against an extent's page count before a descriptor is believed,
    /// and a wrapped sum is precisely how a corrupt list would arrange to agree.
    pub fn frame_count(&self) -> u64 {
        match self {
            Locator::Pfns(runs) => runs.iter().fold(0u64, |a, r| a.saturating_add(r.frames)),
            _ => 0,
        }
    }

    /// The frame numbers in page order. Empty on the locating paths.
    pub fn frame_numbers(&self) -> impl Iterator<Item = u64> + '_ {
        self.pfn_runs().unwrap_or(&[]).iter().flat_map(|r| r.frame_numbers())
    }
}

/// What crosses about an extent. docs/design.md §4.3:
///
/// > `ExtentDesc { run_id, granularity, control_pages, arena_pages, locator }`.
/// > It is small in every case: on the two locating paths it carries no
/// > addresses at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentDesc {
    pub run_id: u64,
    pub granularity: Granularity,
    pub control_pages: u64,
    pub arena_pages: u64,
    pub locator: Locator,
}

impl ExtentDesc {
    fn size(&self) -> usize {
        8 + 4 + 8 + 8
            + 4
            + match &self.locator {
                Locator::Scan | Locator::Probe => 8,
                Locator::Pfns(runs) => 8 + runs.len() * 16,
                Locator::Ivshmem { .. } => 8 + 8,
                Locator::IvshmemPool { .. } => 8 + 16,
            }
    }

    fn encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.u64(self.run_id)?;
        e.u32(self.granularity as u32)?;
        e.u64(self.control_pages)?;
        e.u64(self.arena_pages)?;
        match &self.locator {
            Locator::Scan => {
                e.u32(0)?;
                e.array_size(0)?;
            }
            Locator::Probe => {
                e.u32(1)?;
                e.array_size(0)?;
            }
            Locator::Pfns(runs) => {
                e.u32(2)?;
                e.array_size(runs.len() as u64)?;
                for r in runs {
                    e.u64(r.first_frame)?;
                    e.u64(r.frames)?;
                }
            }
            Locator::Ivshmem { region_offset } => {
                e.u32(3)?;
                e.array_size(0)?;
                e.u64(*region_offset)?;
            }
            Locator::IvshmemPool { control_offset, pool_nonce } => {
                e.u32(4)?;
                e.array_size(0)?;
                e.u64(*control_offset)?;
                e.u64(*pool_nonce)?;
            }
        }
        Ok(())
    }

    fn decode(d: &mut Decoder<'_>) -> Result<ExtentDesc> {
        let run_id = d.u64()?;
        let g = d.u32()?;
        let granularity =
            Granularity::from_u32(g).ok_or(Error::BadColdMessage("unknown granularity"))?;
        let control_pages = d.u64()?;
        let arena_pages = d.u64()?;
        let tag = d.u32()?;
        // Branch on the tag *before* reading the count. Reading it with
        // `array_size` first would bounds-check it against the bytes remaining
        // and fail with `ImplausibleCount`, which is true but useless: the thing
        // actually wrong is that a locating path carried a frame list, and that
        // is what the person reading the log needs to be told.
        let locator = match tag {
            0 | 1 => {
                if d.u64()? != 0 {
                    return Err(Error::BadColdMessage("a locating path carried a frame list"));
                }
                if tag == 0 { Locator::Scan } else { Locator::Probe }
            }
            2 => {
                let n = d.array_size(16)?;
                let mut runs = Vec::with_capacity(n);
                for _ in 0..n {
                    let run = PfnRun { first_frame: d.u64()?, frames: d.u64()? };
                    // A run of no frames names no page, so a list of them claims
                    // a run count while describing nothing — and "the frame
                    // count agrees with the page count" is exactly the check the
                    // attaching side makes before believing a descriptor. The
                    // encoder cannot produce one; refusing it here is what makes
                    // that a property of the wire rather than of us.
                    if run.frames == 0 {
                        return Err(Error::BadColdMessage("a zero-length frame run"));
                    }
                    if run.first_frame.checked_add(run.frames).is_none() {
                        return Err(Error::BadColdMessage("a frame run past the end of memory"));
                    }
                    runs.push(run);
                }
                Locator::Pfns(runs)
            }
            3 => {
                if d.u64()? != 0 {
                    return Err(Error::BadColdMessage("an ivshmem locator carried a frame list"));
                }
                Locator::Ivshmem { region_offset: d.u64()? }
            }
            4 => {
                if d.u64()? != 0 {
                    return Err(Error::BadColdMessage("an ivshmem locator carried a frame list"));
                }
                Locator::IvshmemPool { control_offset: d.u64()?, pool_nonce: d.u64()? }
            }
            _ => return Err(Error::BadColdMessage("unknown locator")),
        };
        Ok(ExtentDesc { run_id, granularity, control_pages, arena_pages, locator })
    }
}

/// docs/design.md §4.3 step 2, in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub ids: ProtocolIds,
    pub generation: u32,
    pub extent: ExtentDesc,
    pub geometry: Geometry,
    /// [`crate::control::ENDIAN_MARKER`] as the sender wrote it.
    pub endianness: u32,
    pub pointer_width: u32,
}

impl Hello {
    /// The check docs/design.md §4.3 step 4 makes explicit, made here because a
    /// diagnostic that says "big-endian peer" is worth more than one that says
    /// "bad magic".
    pub fn check_against(&self, ours: ProtocolIds) -> Result<()> {
        if self.endianness != ENDIAN_MARKER || self.pointer_width != 64 {
            return Err(Error::IncompatiblePeer {
                endianness: self.endianness,
                pointer_width: self.pointer_width,
            });
        }
        if self.ids.layout_version != ours.layout_version {
            return Err(Error::LayoutVersionMismatch {
                ours: ours.layout_version,
                theirs: self.ids.layout_version,
            });
        }
        if self.ids.wire_version != ours.wire_version {
            return Err(Error::WireVersionMismatch {
                ours: ours.wire_version,
                theirs: self.ids.wire_version,
            });
        }
        if self.ids.protocol_hash != ours.protocol_hash {
            return Err(Error::ProtocolHashMismatch {
                ours: ours.protocol_hash,
                theirs: self.ids.protocol_hash,
            });
        }
        Ok(())
    }
}

/// What a client is told so that it can open the shared object **itself**.
///
/// The run list is *not* in here: §5.7 says it is a side-local derived artefact,
/// and in the development loop it is the identity run anyway. What is in here is
/// the name of the object and the geometry of the two views the client is to
/// build from it, which is the only thing the client cannot work out for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// **The name, which is the whole of the handoff** (docs/design.md §1a).
    ///
    /// A path the peer opens with [`crate::region::open_named`]:
    ///
    /// - `Mode::Local` — a `/dev/shm` object the daemon created
    ///   ([`crate::region::shm_create`]);
    /// - `Mode::Qemu` — QEMU's memory backend, named by its `mem-path` when it
    ///   has one and by `/proc/<qemu-pid>/fd/<n>` when it is a memfd. The app
    ///   side opening the hypervisor's own backing object is the case design.md
    ///   §1a calls out as legal: no descriptor crosses, and the name is resolved
    ///   by whoever holds it.
    ///
    /// On the Windows axis nothing has to cross at all — both sides derive the
    /// section names from the session string ([`crate::winshm::section_name`]) —
    /// so that axis has obeyed this rule since it was written, and this field is
    /// how the Linux axis catches up.
    pub object: String,
    /// The arena's object, when it is a *different* one from [`Self::object`].
    ///
    /// Equal to `object` on every path where both regions are sub-ranges of one
    /// object, which is all of them but one.
    ///
    /// **Empty means "the arena has no name"** — the quarantined
    /// [`crate::localfd`] path, where the arena is a dma-buf the GPU side
    /// exported and a descriptor accompanies the message. A dma-buf is not a
    /// nameable object, so this is not an omission that could be fixed by
    /// naming it; it is the marker of the one provider that a VM boundary
    /// cannot carry. Nothing that has to survive a real hypervisor may leave it
    /// empty, and the only sender that does is `sv-gpu-server serve` on Linux —
    /// docs/design.md §0.7 for the measurements that keep it alive.
    pub arena_object: String,
    pub control_offset: u64,
    pub control_len: u64,
    pub arena_offset: u64,
    pub arena_len: u64,
    pub generation: u32,
    /// Runs the daemon stitched the view from. Zero means "the extent is one
    /// contiguous range of the named object", which is the development loop and is
    /// also what a 1 GiB large-page extent measured as (§5.7).
    pub run_count: u64,
}

/// Every message on the cold path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColdMessage {
    Hello(Box<Hello>),
    Accept { generation: u32 },
    Refuse { code: u32, reason: String },
    /// `sv-hostd` → client, carrying the object's **name** and the geometry.
    Attach(Attachment),
    /// The client asks for one.
    AttachRequest,
    /// `svPing` / `svShutdown` of docs/design.md §6.3, on the cold path where they
    /// belong: a ring cannot carry liveness for a peer whose rings are the thing
    /// in doubt.
    Ping,
    Shutdown,
    /// `sv-hostd` → **ICD**, following an [`ColdMessage::Attach`] whose
    /// `run_count` is non-zero: how to stitch the extent out of the object the
    /// `Attach` named.
    ///
    /// # This is a different boundary from the one §5.7 is about
    ///
    /// docs/design.md §5.7 says, firmly, "**the run list does not cross the wire, and
    /// it never did have to**", and §4.3's `ExtentDesc` carries a *locator*
    /// rather than addresses. Both are about the **guest↔host** boundary, where
    /// the guest cannot know the runs — that is the entire reason `ScanLocator`
    /// exists.
    ///
    /// This is the **daemon↔ICD** boundary, and there the picture is the
    /// opposite: both processes are the app side, on one kernel, and design.md
    /// §3's own topology box says `sv-hostd` "hands **name + runs** to the ICDs".
    /// It has to, and the reason is structural rather than a matter of taste:
    /// **a scattered extent has no single offset whose `mmap` reproduces
    /// it.** The daemon's contiguous view is many `mmap(MAP_FIXED)` calls into
    /// one reservation, and a view is not a file. So either the runs reach the
    /// client and it stitches its own — one `mmap` per run, the cost §5.7
    /// already budgets — or the daemon copies, which §1's second invariant
    /// forbids.
    ///
    /// `Attachment::run_count` was already in the protocol for this, with 0
    /// meaning "one contiguous range". What was missing was the message that
    /// carries the runs when it is not.
    Runs(Vec<AttachRun>),
    /// The client asks for an [`ColdMessage::Attach`] **with descriptors**
    /// (waycompose-design.md §112): a client in a sandbox that may not open
    /// names, reached through the render node. The `Attach` that answers it
    /// carries no names; it comes with the control object's descriptor and, when
    /// the arena is a different object, the arena's second
    /// ([`crate::localfd`]). The connection then stays open for
    /// [`ColdMessage::RegionRequest`]s. A daemon that predates it refuses it as
    /// an unexpected message.
    AttachRequestFds,
    /// Client → `sv-hostd`, after an `AttachRequestFds` attach: the descriptor
    /// of the leased pool's region `k`, which a named client would open as
    /// `poolhdr::region_path`.
    RegionRequest { k: u32 },
    /// The answer: region `k` is `len` bytes, and its descriptor comes with
    /// this message. A refusal is [`ColdMessage::Refuse`].
    Region { k: u32, len: u64 },
}

/// One run of the daemon's stitched view, as the ICD needs it.
///
/// Deliberately *not* `sv_arena::extent::Run`: this is the wire form of a
/// same-machine handoff and that is the side-local derived type. They hold the
/// same three numbers, and keeping them separate is what stops the arena's
/// internal representation becoming protocol by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachRun {
    /// Offset in the object the `Attach` named.
    pub file_offset: u64,
    /// Offset within the extent — the number both sides agree on (§1).
    pub extent_offset: u64,
    pub len: u64,
}

const TAG_HELLO: u32 = 1;
const TAG_ACCEPT: u32 = 2;
const TAG_REFUSE: u32 = 3;
const TAG_ATTACH: u32 = 4;
const TAG_ATTACH_REQUEST: u32 = 5;
const TAG_PING: u32 = 6;
const TAG_SHUTDOWN: u32 = 7;
const TAG_RUNS: u32 = 8;
const TAG_ATTACH_REQUEST_FDS: u32 = 9;
const TAG_REGION_REQUEST: u32 = 10;
const TAG_REGION: u32 = 11;

/// The most runs one `Runs` message may carry.
///
/// A 2 GiB arena of 4 KiB guest pages measured ~47,000 runs (§5.7), and each is
/// 24 bytes, so the message is ~1.1 MiB — which is why [`MAX_COLD_MESSAGE`] is
/// 1 MiB and this is checked rather than assumed. Beyond it the daemon refuses
/// rather than truncating: half a run list is an arena with a hole in it.
pub const MAX_ATTACH_RUNS: usize = (MAX_COLD_MESSAGE - 16) / 24;

impl ColdMessage {
    fn size(&self) -> usize {
        4 + match self {
            ColdMessage::Hello(h) => 4 + 4 + 8 + 4 + h.extent.size() + 8 * 3 + 4 * 3 + 4 + 4,
            ColdMessage::Accept { .. } => 4,
            ColdMessage::Refuse { reason, .. } => 4 + 8 + pad4(reason.len() + 1),
            ColdMessage::Attach(a) => {
                8 + pad4(a.object.len() + 1)
                    + 8
                    + pad4(a.arena_object.len() + 1)
                    + 8 * 4
                    + 4
                    + 8
            }
            ColdMessage::Runs(r) => 8 + r.len() * 24,
            ColdMessage::RegionRequest { .. } => 4,
            ColdMessage::Region { .. } => 4 + 8,
            ColdMessage::AttachRequest
            | ColdMessage::AttachRequestFds
            | ColdMessage::Ping
            | ColdMessage::Shutdown => 0,
        }
    }

    /// Encode into `out`, replacing its contents. Uses `sv-wire`'s exact-fill
    /// encoder, so a `size`/`encode` disagreement is caught here rather than
    /// producing a message the far side silently mis-parses — the same
    /// discipline the generated protocol is held to (docs/design.md §7.2).
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.resize(self.size(), 0);
        let mut e = Encoder::new(out);
        match self {
            ColdMessage::Hello(h) => {
                e.u32(TAG_HELLO)?;
                e.u32(h.ids.layout_version)?;
                e.u32(h.ids.wire_version)?;
                e.u64(h.ids.protocol_hash)?;
                e.u32(h.generation)?;
                h.extent.encode(&mut e)?;
                e.u64(h.geometry.control_size)?;
                e.u64(h.geometry.arena_size)?;
                e.u64(h.geometry.arena_slab_size)?;
                e.u32(h.geometry.ring_slot_count)?;
                e.u32(h.geometry.ring_data_bytes)?;
                e.u32(h.geometry.timeline_count)?;
                e.u32(h.endianness)?;
                e.u32(h.pointer_width)?;
            }
            ColdMessage::Accept { generation } => {
                e.u32(TAG_ACCEPT)?;
                e.u32(*generation)?;
            }
            ColdMessage::Refuse { code, reason } => {
                e.u32(TAG_REFUSE)?;
                e.u32(*code)?;
                e.c_string(reason.as_bytes())?;
            }
            ColdMessage::Attach(a) => {
                e.u32(TAG_ATTACH)?;
                e.c_string(a.object.as_bytes())?;
                e.c_string(a.arena_object.as_bytes())?;
                e.u64(a.control_offset)?;
                e.u64(a.control_len)?;
                e.u64(a.arena_offset)?;
                e.u64(a.arena_len)?;
                e.u32(a.generation)?;
                e.u64(a.run_count)?;
            }
            ColdMessage::Runs(runs) => {
                e.u32(TAG_RUNS)?;
                e.u64(runs.len() as u64)?;
                for r in runs {
                    e.u64(r.file_offset)?;
                    e.u64(r.extent_offset)?;
                    e.u64(r.len)?;
                }
            }
            ColdMessage::AttachRequest => e.u32(TAG_ATTACH_REQUEST)?,
            ColdMessage::AttachRequestFds => e.u32(TAG_ATTACH_REQUEST_FDS)?,
            ColdMessage::RegionRequest { k } => {
                e.u32(TAG_REGION_REQUEST)?;
                e.u32(*k)?;
            }
            ColdMessage::Region { k, len } => {
                e.u32(TAG_REGION)?;
                e.u32(*k)?;
                e.u64(*len)?;
            }
            ColdMessage::Ping => e.u32(TAG_PING)?,
            ColdMessage::Shutdown => e.u32(TAG_SHUTDOWN)?,
        }
        e.finish()?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<ColdMessage> {
        let mut d = Decoder::new(bytes);
        let msg = match d.u32()? {
            TAG_HELLO => {
                let ids = ProtocolIds {
                    layout_version: d.u32()?,
                    wire_version: d.u32()?,
                    protocol_hash: d.u64()?,
                };
                let generation = d.u32()?;
                let extent = ExtentDesc::decode(&mut d)?;
                let geometry = Geometry {
                    control_size: d.u64()?,
                    arena_size: d.u64()?,
                    arena_slab_size: d.u64()?,
                    ring_slot_count: d.u32()?,
                    ring_data_bytes: d.u32()?,
                    timeline_count: d.u32()?,
                };
                ColdMessage::Hello(Box::new(Hello {
                    ids,
                    generation,
                    extent,
                    geometry,
                    endianness: d.u32()?,
                    pointer_width: d.u32()?,
                }))
            }
            TAG_ACCEPT => ColdMessage::Accept { generation: d.u32()? },
            TAG_REFUSE => {
                let code = d.u32()?;
                let reason = String::from_utf8_lossy(d.c_string()?).into_owned();
                ColdMessage::Refuse { code, reason }
            }
            TAG_ATTACH => ColdMessage::Attach(Attachment {
                // Lossy rather than fatal for the same reason `Refuse`'s string
                // is: a name that is not UTF-8 will fail to open by name, with
                // the name in the message, which is a better diagnostic than
                // "the attach message was malformed".
                object: String::from_utf8_lossy(d.c_string()?).into_owned(),
                arena_object: String::from_utf8_lossy(d.c_string()?).into_owned(),
                control_offset: d.u64()?,
                control_len: d.u64()?,
                arena_offset: d.u64()?,
                arena_len: d.u64()?,
                generation: d.u32()?,
                run_count: d.u64()?,
            }),
            TAG_RUNS => {
                let n = d.u64()?;
                // Bound before allocating. A corrupted or hostile length here is
                // R2's shape — `n` comes off the wire and `Vec::with_capacity`
                // would take it at its word.
                let n = usize::try_from(n)
                    .ok()
                    .filter(|n| *n <= MAX_ATTACH_RUNS)
                    .ok_or(Error::BadColdMessage("implausible run count"))?;
                let mut runs = Vec::with_capacity(n);
                for _ in 0..n {
                    runs.push(AttachRun {
                        file_offset: d.u64()?,
                        extent_offset: d.u64()?,
                        len: d.u64()?,
                    });
                }
                ColdMessage::Runs(runs)
            }
            TAG_ATTACH_REQUEST => ColdMessage::AttachRequest,
            TAG_ATTACH_REQUEST_FDS => ColdMessage::AttachRequestFds,
            TAG_REGION_REQUEST => ColdMessage::RegionRequest { k: d.u32()? },
            TAG_REGION => ColdMessage::Region { k: d.u32()?, len: d.u64()? },
            TAG_PING => ColdMessage::Ping,
            TAG_SHUTDOWN => ColdMessage::Shutdown,
            _ => return Err(Error::BadColdMessage("unknown message tag")),
        };
        if d.remaining() != 0 {
            return Err(Error::BadColdMessage("trailing bytes"));
        }
        Ok(msg)
    }
}

// ---------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------

/// The largest cold-path message we will read. The cold path carries a
/// handshake, never data (D10), so this is generous by two orders of magnitude
/// and exists so that a corrupt length is an error rather than an allocation.
pub const MAX_COLD_MESSAGE: usize = 1 << 20;

#[cfg(unix)]
mod unix_socket {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    /// Send one framed message: a four-byte little-endian length, then the body.
    ///
    /// **No ancillary data**, and that is the point of the function existing in
    /// this shape. The bytes it writes are the bytes a virtio-serial port or a
    /// vsock would carry, so the handshake that works between two processes on
    /// one kernel is the same one that works across a hypervisor. A cold path
    /// that needed `sendmsg` was a cold path only a Unix socket could implement,
    /// which is docs/design.md §1a's objection stated as a fact about the code.
    pub fn send_bytes(sock: &UnixStream, payload: &[u8]) -> Result<()> {
        let mut framed = Vec::with_capacity(payload.len() + 4);
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(payload);
        sock.try_clone()
            .and_then(|mut s| s.write_all(&framed))
            .map_err(|e| Error::Os { call: "write", errno: e.raw_os_error().unwrap_or(0) })
    }

    /// Read one framed message into `out`.
    pub fn recv_bytes(sock: &UnixStream, out: &mut Vec<u8>) -> Result<()> {
        let mut s = sock;
        let mut lenbuf = [0u8; 4];
        s.read_exact(&mut lenbuf).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => Error::BadColdMessage("peer closed the cold path"),
            _ => Error::Os { call: "read_exact", errno: e.raw_os_error().unwrap_or(0) },
        })?;
        let len = u32::from_le_bytes(lenbuf) as usize;
        if len > MAX_COLD_MESSAGE {
            return Err(Error::BadColdMessage("cold-path message is implausibly long"));
        }
        out.clear();
        out.resize(len, 0);
        s.read_exact(out)
            .map_err(|e| Error::Os { call: "read_exact", errno: e.raw_os_error().unwrap_or(0) })?;
        Ok(())
    }

    /// Send a [`ColdMessage`].
    pub fn send(sock: &UnixStream, msg: &ColdMessage) -> Result<()> {
        let mut buf = Vec::new();
        msg.encode(&mut buf)?;
        send_bytes(sock, &buf)
    }

    /// Receive a [`ColdMessage`].
    pub fn recv(sock: &UnixStream) -> Result<ColdMessage> {
        let mut buf = Vec::new();
        recv_bytes(sock, &mut buf)?;
        ColdMessage::decode(&buf)
    }
}

#[cfg(unix)]
pub use unix_socket::{recv, recv_bytes, send, send_bytes};

/// On Windows the cold path is a named pipe, and it is not written.
///
/// Stated rather than stubbed: design.md requires either side to be either OS,
/// and `testhost` exercises Linux-app-side only. A named-pipe implementation
/// of exactly these four functions is what a Windows app side needs, and
/// nothing above this module would change.
#[cfg(not(unix))]
pub fn send(_sock: &(), _msg: &ColdMessage) -> Result<()> {
    Err(Error::Unsupported("the cold-path socket is Unix-only in this build"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> ColdMessage {
        ColdMessage::Hello(Box::new(Hello {
            ids: ProtocolIds::new(3, 0x0123_4567_89ab_cdef),
            generation: 7,
            extent: ExtentDesc {
                run_id: 0xdead_beef_cafe_f00d,
                granularity: Granularity::Large2M,
                control_pages: 4096,
                arena_pages: 524288,
                locator: Locator::Scan,
            },
            geometry: Geometry::default(),
            endianness: ENDIAN_MARKER,
            pointer_width: 64,
        }))
    }

    #[test]
    fn every_message_round_trips() {
        let msgs = [
            hello(),
            ColdMessage::Accept { generation: 7 },
            ColdMessage::Refuse { code: 2, reason: "protocol hash mismatch".into() },
            ColdMessage::Attach(Attachment {
                object: "/dev/shm/sv.e2e".into(),
                arena_object: "/dev/shm/sv.e2e".into(),
                control_offset: 0,
                control_len: 16 << 20,
                arena_offset: 16 << 20,
                arena_len: 2 << 30,
                generation: 7,
                run_count: 1,
            }),
            ColdMessage::AttachRequest,
            ColdMessage::Ping,
            ColdMessage::Shutdown,
            ColdMessage::AttachRequestFds,
            ColdMessage::RegionRequest { k: 3 },
            ColdMessage::Region { k: 3, len: 1 << 30 },
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            m.encode(&mut buf).unwrap();
            assert_eq!(&ColdMessage::decode(&buf).unwrap(), m, "{m:?}");
        }
        assert_eq!(msgs.len(), 10, "every variant must appear above");
    }

    /// §87: the ivshmem locator crosses as an offset and decodes to the same
    /// offset, and a frame list smuggled in front of it is refused rather than
    /// skipped.
    #[test]
    fn an_ivshmem_locator_round_trips_and_carries_one_offset() {
        let mut h = hello();
        if let ColdMessage::Hello(b) = &mut h {
            b.extent.locator = Locator::Ivshmem { region_offset: 80 << 20 };
            assert_eq!(b.extent.size(), 8 + 4 + 8 + 8 + 4 + 16);
        }
        let mut buf = Vec::new();
        h.encode(&mut buf).unwrap();
        assert_eq!(ColdMessage::decode(&buf).unwrap(), h);
        assert_eq!(Locator::Ivshmem { region_offset: 1 }.frame_count(), 0);
        assert_eq!(Locator::Ivshmem { region_offset: 1 }.pfn_runs(), None);
    }

    /// §97: a pooled session's locator carries the control region's offset and
    /// the pool's nonce, both intact, and no frames.
    #[test]
    fn a_pool_locator_round_trips_with_its_nonce() {
        let mut h = hello();
        if let ColdMessage::Hello(b) = &mut h {
            b.extent.locator = Locator::IvshmemPool { control_offset: 64 << 10, pool_nonce: 0x5650_4f4f_4c00_0001 };
            b.extent.arena_pages = 0;
            assert_eq!(b.extent.size(), 8 + 4 + 8 + 8 + 4 + 24);
        }
        let mut buf = Vec::new();
        h.encode(&mut buf).unwrap();
        assert_eq!(ColdMessage::decode(&buf).unwrap(), h);
        assert_eq!(Locator::IvshmemPool { control_offset: 0, pool_nonce: 0 }.frame_count(), 0);
    }

    /// docs/design.md §4.3: the descriptor "is small in every case: on the two
    /// locating paths it carries no addresses at all". Worth an assertion,
    /// because the superseded design *did* put a page list on the wire and the
    /// reason it no longer does is a measurement, not a preference.
    #[test]
    fn a_scan_locator_carries_no_addresses_whatever_the_extent_size() {
        let small = ExtentDesc {
            run_id: 1,
            granularity: Granularity::Large2M,
            control_pages: 1,
            arena_pages: 1,
            locator: Locator::Scan,
        };
        let huge = ExtentDesc { arena_pages: 524_288, ..small.clone() };
        assert_eq!(small.size(), huge.size());
        assert!(small.size() < 64, "an ExtentDesc is a handful of words");

        // …and the AWE path is the counter-example that shows the saving is
        // real: 5863 runs per 64 MiB, measured, at 16 bytes each.
        let awe = ExtentDesc {
            locator: Locator::Pfns(vec![PfnRun { first_frame: 1, frames: 1 }; 5863]),
            ..small.clone()
        };
        assert!(awe.size() > 90_000);
    }

    /// The encoding is runs and the meaning is one frame per page, so the two
    /// spellings must be each other's inverse. Anything else is a locator that
    /// is right about how many pages it found and wrong about which.
    #[test]
    fn a_frame_list_run_length_encodes_and_expands_back_to_itself() {
        let frames: Vec<u64> = vec![10, 11, 12, 40, 100, 101];
        let l = Locator::from_frames(&frames);
        assert_eq!(
            l.pfn_runs().unwrap(),
            &[
                PfnRun { first_frame: 10, frames: 3 },
                PfnRun { first_frame: 40, frames: 1 },
                PfnRun { first_frame: 100, frames: 2 },
            ]
        );
        assert_eq!(l.frame_count(), 6);
        assert_eq!(l.frame_numbers().collect::<Vec<_>>(), frames);

        // The measured shapes as arithmetic: a 64 MiB window whose frames are
        // one block is one run, and the worst case — 16384 pages, no two frames
        // adjacent — is one run per page, which is the number §5.7 prices
        // attaching in.
        let contiguous: Vec<u64> = (0..16384).collect();
        assert_eq!(Locator::from_frames(&contiguous).pfn_runs().unwrap().len(), 1);
        let scattered: Vec<u64> = (0..16384).map(|i| i * 2).collect();
        assert_eq!(Locator::from_frames(&scattered).pfn_runs().unwrap().len(), 16384);

        // And the locating paths carry nothing, which is what makes asking them
        // for frames safe rather than a panic waiting to happen.
        assert_eq!(Locator::from_frames(&[]).frame_count(), 0);
        assert_eq!(Locator::Scan.frame_count(), 0);
        assert_eq!(Locator::Scan.pfn_runs(), None);
        assert_eq!(Locator::Scan.frame_numbers().count(), 0);
    }

    /// Frames that are contiguous in the wrong direction are **not** one run. A
    /// coalescer that only asked "is this frame one more than the last" would
    /// say they were, and the far side would stitch the extent backwards with
    /// every page present and every offset wrong.
    #[test]
    fn frames_contiguous_in_the_wrong_direction_are_one_run_per_page() {
        let l = Locator::from_frames(&[12, 11, 10]);
        assert_eq!(l.pfn_runs().unwrap().len(), 3);
        assert_eq!(l.frame_numbers().collect::<Vec<_>>(), vec![12, 11, 10]);
    }

    /// The frame list survives the wire unchanged, which is what lets the host
    /// translate what the guest measured instead of what it inferred.
    #[test]
    fn a_frame_list_round_trips_through_the_hello() {
        let frames: Vec<u64> = vec![0x4_2000, 0x4_2001, 0x4_2002, 0x9_9999];
        let ColdMessage::Hello(mut h) = hello() else { unreachable!() };
        h.extent.control_pages = 1;
        h.extent.arena_pages = 3;
        h.extent.locator = Locator::from_frames(&frames);
        let msg = ColdMessage::Hello(h);
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let back = ColdMessage::decode(&buf).unwrap();
        assert_eq!(back, msg);
        let ColdMessage::Hello(h) = back else { unreachable!() };
        assert_eq!(h.extent.locator.frame_numbers().collect::<Vec<_>>(), frames);
        assert_eq!(h.extent.locator.frame_count(), h.extent.control_pages + h.extent.arena_pages);
    }

    /// A run of no frames names no memory, so a list of them would satisfy a
    /// run-count check while describing nothing. The encoder cannot produce
    /// one, so the decoder is the only place this can be refused.
    #[test]
    fn a_zero_length_frame_run_is_refused_on_the_wire() {
        // run_id, granularity, two page counts, the tag, the run count, and one
        // 16-byte run: 56 bytes, and `finish` insists the buffer is filled
        // exactly rather than merely fitting.
        let build = |first_frame: u64, frames: u64| {
            let mut buf = vec![0u8; 56];
            let mut e = Encoder::new(&mut buf);
            e.u64(1).unwrap(); // run_id
            e.u32(0).unwrap(); // Small4K
            e.u64(1).unwrap(); // control_pages
            e.u64(0).unwrap(); // arena_pages
            e.u32(2).unwrap(); // Locator::Pfns
            e.array_size(1).unwrap();
            e.u64(first_frame).unwrap();
            e.u64(frames).unwrap();
            assert_eq!(e.finish().unwrap(), 56);
            buf
        };

        let zero = build(7, 0);
        assert!(matches!(
            ExtentDesc::decode(&mut Decoder::new(&zero)),
            Err(Error::BadColdMessage(_))
        ));

        // …and a run that runs off the end of the address space, which would
        // wrap round into low memory when expanded.
        let wrap = build(u64::MAX, 2);
        assert!(matches!(
            ExtentDesc::decode(&mut Decoder::new(&wrap)),
            Err(Error::BadColdMessage(_))
        ));

        // The same shape with one real frame decodes, so the two refusals above
        // are about the run and not about the message.
        let good = build(7, 1);
        assert_eq!(
            ExtentDesc::decode(&mut Decoder::new(&good)).unwrap().locator,
            Locator::Pfns(vec![PfnRun { first_frame: 7, frames: 1 }])
        );
    }

    #[test]
    fn a_locating_path_that_carries_a_frame_list_is_refused() {
        // Hand-built, because `encode` will not produce one.
        let mut buf = vec![0u8; 40];
        let mut e = Encoder::new(&mut buf);
        e.u64(1).unwrap(); // run_id
        e.u32(0).unwrap(); // Small4K
        e.u64(1).unwrap();
        e.u64(1).unwrap();
        e.u32(0).unwrap(); // Locator::Scan
        e.array_size(1).unwrap(); // …with a run list
        e.finish().unwrap();
        let mut d = Decoder::new(&buf);
        assert!(matches!(ExtentDesc::decode(&mut d), Err(Error::BadColdMessage(_))));
    }

    #[test]
    fn a_hello_from_a_big_endian_or_32_bit_peer_is_refused_by_name() {
        let ours = ProtocolIds::new(3, 0x0123_4567_89ab_cdef);
        let ColdMessage::Hello(h) = hello() else { unreachable!() };
        h.check_against(ours).unwrap();

        let mut be = h.clone();
        be.endianness = ENDIAN_MARKER.swap_bytes();
        assert!(matches!(be.check_against(ours), Err(Error::IncompatiblePeer { .. })));

        let mut narrow = h.clone();
        narrow.pointer_width = 32;
        assert!(matches!(narrow.check_against(ours), Err(Error::IncompatiblePeer { .. })));

        let mut skew = h.clone();
        skew.ids.protocol_hash = 1;
        assert!(matches!(skew.check_against(ours), Err(Error::ProtocolHashMismatch { .. })));
    }

    #[test]
    fn a_message_with_trailing_bytes_is_refused() {
        let mut buf = Vec::new();
        ColdMessage::Ping.encode(&mut buf).unwrap();
        buf.extend_from_slice(&[0, 0, 0, 0]);
        assert!(matches!(ColdMessage::decode(&buf), Err(Error::BadColdMessage(_))));
    }

    #[test]
    fn an_unknown_tag_is_refused_rather_than_ignored() {
        let mut buf = vec![0u8; 4];
        Encoder::new(&mut buf).u32(999).unwrap();
        assert!(matches!(ColdMessage::decode(&buf), Err(Error::BadColdMessage(_))));
    }

    /// The handoff, end to end, with **no descriptor anywhere**: a name crosses
    /// a byte stream and the receiver opens the object itself.
    ///
    /// This is the test that replaced `a_descriptor_crosses_the_socket_and_names
    /// _the_same_memory`, and the difference between them is docs/design.md §1a. The
    /// proof of sharing is the same and is the only part worth keeping: a write
    /// through the sender's mapping is visible through the receiver's.
    #[cfg(target_os = "linux")]
    #[test]
    #[cfg_attr(miri, ignore = "socketpair and open(2) are syscalls Miri does not implement")]
    fn a_name_crosses_the_socket_and_the_peer_opens_the_same_memory() {
        use crate::region::{SharedRegion, shm_create, shm_path, shm_unlink};
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;

        let (a, b) = UnixStream::pair().unwrap();
        let path = shm_path(&format!("cold-test.{}", std::process::id()));
        let file = shm_create(&path, 8192).unwrap();
        let msg = ColdMessage::Attach(Attachment {
            object: path.clone(),
            arena_object: path.clone(),
            control_offset: 0,
            control_len: 4096,
            arena_offset: 4096,
            arena_len: 4096,
            generation: 1,
            run_count: 0,
        });
        send(&a, &msg).unwrap();

        let got = recv(&b).unwrap();
        assert_eq!(got, msg);
        let ColdMessage::Attach(at) = got else { unreachable!() };
        assert!(!at.object.is_empty(), "the name is the handoff");

        // The receiver opens it by name. Nothing was handed to it but text.
        let theirs = crate::region::open_named(&at.object).unwrap();
        let here = SharedRegion::map_fd(file.as_fd(), 0, 4096).unwrap();
        let there = SharedRegion::map_fd(theirs.as_fd(), 0, 4096).unwrap();
        here.write_bytes(128, b"crossed").unwrap();
        let mut out = [0u8; 7];
        there.read_bytes(128, &mut out).unwrap();
        assert_eq!(&out, b"crossed");

        // Unlinking releases the name and not the memory: both mappings and both
        // descriptors are still live, which is what makes it safe for the daemon
        // to unlink as soon as its clients have accepted.
        shm_unlink(&path);
        here.write_bytes(128, b"after__").unwrap();
        there.read_bytes(128, &mut out).unwrap();
        assert_eq!(&out, b"after__");
    }

    /// A `/dev/shm` object is not sealable, which is why the local-udmabuf
    /// answer for RADV exists only where the object is QEMU's own memfd.
    ///
    /// Measured rather than reasoned, and pinned here because it is the fact
    /// that decides whether `Provider::GpuExport` can ever be deleted
    /// (docs/design.md §0.7). If a future kernel makes `F_ADD_SEALS` work on a named
    /// object, this test fails and the finding is stale — which is the point.
    #[cfg(target_os = "linux")]
    #[test]
    #[cfg_attr(miri, ignore = "fcntl is a syscall Miri does not implement")]
    fn a_named_object_cannot_be_sealed_and_therefore_cannot_back_a_udmabuf() {
        use crate::region::{shm_create, shm_path, shm_unlink};
        use std::os::fd::AsRawFd;

        unsafe extern "C" {
            fn fcntl(fd: i32, cmd: i32, ...) -> i32;
        }
        const F_GET_SEALS: i32 = 1034;
        const F_ADD_SEALS: i32 = 1033;
        const F_SEAL_SEAL: i32 = 0x0001;
        const F_SEAL_SHRINK: i32 = 0x0002;

        let path = shm_path(&format!("seal-test.{}", std::process::id()));
        let file = shm_create(&path, 4096).unwrap();
        // SAFETY: `fcntl` with these two commands takes an `int` and returns
        // one; the descriptor is live for the call.
        let seals = unsafe { fcntl(file.as_raw_fd(), F_GET_SEALS, 0) };
        let added = unsafe { fcntl(file.as_raw_fd(), F_ADD_SEALS, F_SEAL_SHRINK) };
        shm_unlink(&path);

        assert_eq!(seals & F_SEAL_SEAL, F_SEAL_SEAL, "a /dev/shm object is born sealed shut");
        assert_eq!(added, -1, "F_SEAL_SHRINK must be refused on a named object");
    }
}
