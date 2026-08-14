//! Where a `wl_shm` pool's bytes go so that the compositor can read them
//! **without anything being passed**.
//!
//! # The rule, restated because it is the whole design
//!
//! No file descriptor and no handle crosses the boundary, ever, even when both
//! sides happen to be on one machine. A descriptor created locally out of
//! memory that side already reaches is fine; a descriptor *passed* is not. A
//! name crossing, which the peer then opens itself, is the sanctioned pattern.
//!
//! The client's pool fd is created by the client and consumed by this process.
//! It never goes anywhere. What crosses is one of the two things a
//! [`PoolTransport`] can produce, and neither is a descriptor:
//!
//! | transport | what crosses | what the peer does with it |
//! |---|---|---|
//! | [`SharedFile`] | a path | opens it, maps it |
//! | [`Arena`] | `(arena, offset, len)` | arithmetic; it mapped the region before any traffic |
//!
//! # Why the pool cannot be arena-backed directly, which is the question the
//! brief asked
//!
//! It would be better if the client's pool *were* the shared region, because
//! then a commit would cost nothing at all. It cannot be, and the reason is
//! structural rather than an implementation gap:
//!
//! **`wl_shm.create_pool` is the client handing the server memory it has
//! already allocated.** The protocol has no request in the other direction — no
//! way for a server to say "allocate your next pool here". The client calls
//! `memfd_create` (or `shm_open`, or `mkstemp`) inside libwayland, sizes it,
//! maps it, draws into it, and only then tells us it exists. By the time this
//! proxy learns of a pool, the pages are chosen. Nothing on Linux can re-point
//! an existing mapping at another object's pages, so there is no trick
//! available: the bytes must be copied at least once on this side of the
//! boundary.
//!
//! The honest way to state it: **a copy per frame is the price of the client
//! being unmodified**, and it is a price paid on the host, which has spare CPU,
//! rather than in the guest, which has the GPU. An arena-backed pool is
//! reachable only by a client that allocates from the arena — which is what
//! `docs/goal-firefox.md`'s accelerated path is, where Firefox's WebRender
//! renders through shared_vulkan into arena memory and no `wl_shm` pool exists
//! at all. That is why this seam matters more than the copy it currently
//! performs.
//!
//! So the measurable question is not *whether* to copy but *how much*, which is
//! [`CopyScope`], and *into what*, which is the two transports.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use wc_handle::{Descriptor, Handle, MappingMut};

/// How much of a pool is copied when a surface commits.
///
/// All three are correct; they differ in how much they copy and in how much
/// they have to understand to do it. Measured in `docs/waycompose-design.md` §9.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyScope {
    /// The whole pool, every commit. Understands nothing beyond "this surface's
    /// buffer lives in that pool" — the simplest correct proxy there is, and
    /// the one whose cost is set by the client's allocation habits rather than
    /// by what it drew.
    Pool,
    /// The attached buffer's extent within the pool. Needs `create_buffer`'s
    /// offset, stride and height.
    Buffer,
    /// Only the rows the client damaged, within the attached buffer. Needs all
    /// of the above plus `wl_surface.damage`/`damage_buffer` accumulation.
    Damage,
}

impl CopyScope {
    pub fn parse(s: &str) -> Option<CopyScope> {
        match s {
            "pool" => Some(CopyScope::Pool),
            "buffer" => Some(CopyScope::Buffer),
            "damage" => Some(CopyScope::Damage),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CopyScope::Pool => "pool",
            CopyScope::Buffer => "buffer",
            CopyScope::Damage => "damage",
        }
    }
}

/// A published region: what the peer will be told, and the key this side uses
/// to find the memory again.
#[derive(Clone, Debug)]
pub struct Region {
    /// Exactly the bytes that cross. Never contains a descriptor of any kind
    /// the peer would have to be *given* rather than open.
    pub descriptor: Descriptor,
    pub size: u64,
    key: u64,
}

impl Region {
    /// A one-line description for the log, so that a run says which mechanism
    /// carried its pixels rather than leaving it to be inferred.
    pub fn describe(&self) -> String {
        match &self.descriptor {
            Descriptor::SharedFile { path, size } => format!("shared-file {path} ({size} bytes)"),
            Descriptor::ArenaSlice { arena, offset, len } => {
                format!("arena {arena} slice {offset}+{len}")
            }
            other => format!("{other:?}"),
        }
    }
}

/// Somewhere a pool's bytes can live such that only a name or an offset
/// crosses.
pub trait PoolTransport {
    /// For the log and for the harness's "which transport served this" check.
    fn name(&self) -> &'static str;

    /// Publish `size` bytes. Fails rather than degrading: a transport that
    /// quietly fell back to another would defeat the point of naming which one
    /// ran.
    fn create(&mut self, size: u64) -> io::Result<Region>;

    /// Stop publishing. The bytes may be reused by a later [`create`].
    ///
    /// [`create`]: PoolTransport::create
    fn destroy(&mut self, region: &Region);

    /// The region's bytes, for writing.
    ///
    /// One virtual call per commit rather than one per row: the copy loop above
    /// this is the hot path and the transport is not.
    fn bytes_mut(&mut self, region: &Region) -> Option<&mut [u8]>;

    /// Bytes currently published, for the log.
    fn published(&self) -> u64;

    /// **Peak resident bytes**, regions at the peak, the floor, and what is
    /// held right now — the memory question, for a transport that has an answer.
    ///
    /// Reported per connection rather than once for the process because the
    /// process-wide line at the end of `run()` never prints: a harness kills
    /// this proxy rather than waiting for it, so a memory figure that only
    /// existed at clean shutdown was a figure nobody ever read. Every
    /// connection reporting the same arena is redundant and printed anyway,
    /// because a number that is not there is worse than a number twice.
    fn memory(&self) -> Option<MemoryReport> {
        None
    }

    /// One frame has gone by. Returns every region handed back as a result,
    /// as `(id, bytes, origin)`, for the caller to log.
    ///
    /// DYNAMIC ARENA. The release policy is written in frames rather than
    /// seconds because what makes a region cold is that nothing drew in it, and
    /// a clock cannot tell a cold region from a paused video. A transport with
    /// nothing to give back says so by giving back nothing, which is the honest
    /// answer for [`SharedFile`] — it has one object per pool and unlinks it on
    /// `destroy`, so it has no notion of a region at all.
    fn frame(&mut self) -> Vec<(u32, u64, String)> {
        Vec::new()
    }

    /// Read `(arena, offset, len)` of a region this transport holds, if it holds
    /// one.
    ///
    /// STAGE 10, and it is the *server*-to-client direction: the compositor now
    /// cuts the objects it creates — a `wl_keyboard` keymap first — out of its
    /// own window of the same arena, and this is how they are read back here to
    /// be turned into a descriptor for a Linux client
    /// (`crate::session::materialise`).
    ///
    /// It is on the transport rather than on `wc_handle::arena` because this
    /// process reaches the region through `guestram::StitchedRegion` — 1500
    /// `mmap(MAP_FIXED)` calls into one reservation — and registering that with
    /// `wc_handle` would mean a second way to hold the same pages, whose two
    /// halves could disagree. There is one mapping and one owner of it.
    ///
    /// `None` for a transport with no arena, which is a real answer rather than
    /// a failure: `--transport shared-file` never sees an arena slice unless the
    /// compositor was configured against a region this proxy does not have, and
    /// that misconfiguration should say so in those words.
    fn peer_slice(&self, _arena: u32, _offset: u64, _len: u64) -> Option<&[u8]> {
        None
    }
}

// ---------------------------------------------------------------------------
// SharedFile: one object per pool, named by path
// ---------------------------------------------------------------------------

/// One file per pool, in a directory both sides can reach.
///
/// The straightforward transport, and the one that works under Wine with no
/// hypervisor in the picture. Its cost is an object creation per pool — a
/// `open(O_CREAT)`, an `ftruncate` and an `mmap` here, and a `CreateFileW` plus
/// `CreateFileMappingW` plus `MapViewOfFile` there — which is cheap on a local
/// filesystem and is exactly what is *not* cheap across a hypervisor, where the
/// peer's open is a round trip through a filesystem daemon. That is the
/// measurement that decides between the two, and it is in `docs/waycompose-design.md` §9.
pub struct SharedFile {
    dir: PathBuf,
    /// How the peer spells `dir`. `Z:\tmp\wc-pools` for Wine; a drive letter on
    /// the guest's virtio-fs mount for the real thing. There is no way to
    /// derive this and a proxy that guessed would be wrong on the deployment
    /// that matters, so it is required rather than defaulted.
    peer_prefix: String,
    next: u64,
    live: HashMap<u64, Live>,
    published: u64,
}

struct Live {
    /// Holds the object open. docs/waycompose-design.md §2.5's lifetime rule: the creator
    /// must outlive the peer's use of the name, and for a file that is also
    /// what keeps it off the filesystem afterwards, since the `Handle`'s drop
    /// unlinks it.
    _handle: Handle,
    view: MappingMut,
    size: u64,
}

impl SharedFile {
    pub fn new(dir: &Path, peer_prefix: &str) -> io::Result<SharedFile> {
        std::fs::create_dir_all(dir)?;
        // The pool files are readable by their path, so the directory is the
        // only access control there is. 0700 is what an `XDG_RUNTIME_DIR` uses
        // and is the same standard the compositor's own socket is held to.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(SharedFile {
            dir: dir.to_path_buf(),
            peer_prefix: peer_prefix.to_owned(),
            next: 0,
            live: HashMap::new(),
            published: 0,
        })
    }
}

impl PoolTransport for SharedFile {
    fn name(&self) -> &'static str {
        "shared-file"
    }

    fn create(&mut self, size: u64) -> io::Result<Region> {
        if size == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a pool cannot be empty"));
        }
        let key = self.next;
        self.next += 1;
        // 128 bits of randomness in the name, for the same reason
        // `wc_handle`'s section names carry it: the name is the only thing
        // between this pool and any other process that can see the directory.
        let stem = format!("wc-pool-{key:04x}-{}", random_hex());
        let local = self.dir.join(&stem);
        let local_str = local.to_string_lossy().into_owned();
        let peer = format!("{}{}", self.peer_prefix, stem);
        let handle = Handle::create_shared_file(&local_str, &peer, size)?;
        let view = handle.map_mut()?;
        self.published += size;
        self.live.insert(key, Live { _handle: handle, view, size });
        Ok(Region { descriptor: Descriptor::SharedFile { path: peer, size }, size, key })
    }

    fn destroy(&mut self, region: &Region) {
        if let Some(live) = self.live.remove(&region.key) {
            self.published -= live.size;
        }
    }

    fn bytes_mut(&mut self, region: &Region) -> Option<&mut [u8]> {
        Some(self.live.get_mut(&region.key)?.view.as_mut_slice())
    }

    fn published(&self) -> u64 {
        self.published
    }
}

// ---------------------------------------------------------------------------
// Arena: one region for everything, sliced
// ---------------------------------------------------------------------------

/// Every pool is a slice of one region both sides mapped once, before any
/// Wayland traffic.
///
/// This is `Descriptor::ArenaSlice`, reserved in the type since stage 1
/// (docs/waycompose-design.md §2.2 (c)) and constructed here for the first time. In the
/// real deployment the region is shared_vulkan's passthrough arena — guest RAM
/// the host found by walking QEMU's flat view (its docs/waycompose-design.md §5.1) — and this
/// process is handed a range of it. In the same-machine test the region is one
/// file, attached by both sides from a flag.
///
/// **What is identical between those two cases is the part that matters**: no
/// object is created per pool, nothing is opened per pool, and the descriptor
/// is three integers. What differs is only who provided the pages, which is
/// exactly the seam the two projects meet at.
///
/// # The allocator
///
/// First fit over a sorted free list, coalescing on release, 4096-byte
/// granularity. The granularity is not arbitrary: it is `sv_arena::GRANULARITY`
/// (shared_vulkan docs/waycompose-design.md §5.2), which is the measured
/// `minImportedHostPointerAlignment` on the target *and* the page granularity
/// of the shared mapping. A pool that begins on a page boundary is also a pool
/// whose peer-side mapping can be a whole number of pages, which is what makes
/// the peer's side of a slice free rather than merely cheap.
///
/// First fit rather than anything cleverer because a `wl_shm` client holds two
/// or three pools at a time. If that changes the allocator is 60 lines and it
/// is tested; replacing it does not touch the wire.
///
/// # One allocator, several connections
///
/// STAGE 12. An `Arena` is a handle on a region and on **one** free list;
/// [`Arena::share`] makes another handle on the same two things, and every
/// Wayland connection the proxy serves gets one. Firefox opens four
/// (docs/waycompose-design.md §15.8), which is why this exists.
///
/// The invariant this preserves is the one that was always the point: **two
/// allocators over one region would hand out the same offsets**, which is a
/// client's pixels written on top of another client's — or on top of a live
/// keymap — with nothing in any log. Sharing the *allocator* is the opposite of
/// that: four connections asking one free list for space is the same operation
/// as one connection asking it four times.
/// Where an [`Arena`]'s bytes actually are.
///
/// STAGE 7 SPLIT THIS IN TWO, and docs/waycompose-design.md §12 is why. Everything above
/// this enum — the allocator, the free list, the descriptor, the compositor's
/// side of it — is identical for both, which is exactly the claim §9.10 item 3
/// made and this stage had to cash: "substituting the real provider changes
/// `Arena::open_or_create` and nothing above it."
enum Backing {
    /// A file both sides open. The same-machine test shape, and — measured in
    /// §10.3 and §10.7 — **not a usable live surface** on a virtio-fs export,
    /// because two live mappings of one file on that mount do not see each
    /// other's stores.
    File {
        /// Held for its lifetime rule, not for its bytes.
        _handle: Handle,
        view: MappingMut,
    },
    /// The guest's own RAM, found in QEMU's memory backend and stitched into
    /// one view. Not a file, and therefore not subject to a mount's caching.
    GuestRam(crate::guestram::StitchedRegion),
}

impl Backing {
    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Backing::File { view, .. } => view.as_mut_slice(),
            Backing::GuestRam(r) => r.as_mut_slice(),
        }
    }
}

/// The region, reachable from several session threads at once.
///
/// STAGE 12, and this type is where the whole multi-connection question lands.
/// The `Backing` above is *one* mapping — a `MappingMut` or a
/// `StitchedRegion` — and neither is `Sync`, deliberately: `wc_handle` says
/// "a read-only view can be shared freely, a writable one is exclusive to
/// whoever holds the `&mut`". That rule is right for one owner and it is the
/// wrong shape for four Wayland connections into one arena, so it is replaced
/// here by a narrower one rather than widened over there.
///
/// # The rule that replaces it
///
/// **The only writable references handed out are the extents one allocator has
/// handed out, and that allocator never overlaps two of them.** So the `&mut`s
/// that exist at any moment are disjoint sub-slices of one allocation, which is
/// what `slice::split_at_mut` produces and is sound for the same reason. The
/// allocator is [`Alloc`], it is behind a single [`Mutex`], its extents are
/// asserted disjoint by `arena_slices_never_overlap`, and a `Region`'s key is
/// only ever known to the one session that asked for it.
///
/// The read side is narrower still: [`Arena::peer_slice`] refuses anything
/// outside the compositor's outbound window, and the outbound window is exactly
/// the range [`Arena::reserve_outbound`] removed from the free list. So a
/// shared `&[u8]` and any `&mut [u8]` are disjoint **by a check**, not by an
/// argument.
struct SharedBacking {
    /// Held for its lifetime and its `Drop`; never dereferenced again, because
    /// both of its accessors want a whole-region slice and this type must never
    /// form one while sub-slices are out.
    _backing: Backing,
    base: *mut u8,
    len: usize,
}

// SAFETY: `base` is the one mapping `_backing` owns and outlives every
// reference this type hands out. Sharing it between threads is sound because of
// the rule in the doc comment above: the writable references are disjoint
// allocator extents and the readable ones are inside the outbound window, which
// the allocator does not hand out. It is shared memory — the compositor and,
// over guest RAM, the guest itself write it too — which is a `wl_shm` hazard
// rather than a Rust one, and is the hazard `wc_handle::Mapping` documents.
unsafe impl Send for SharedBacking {}
unsafe impl Sync for SharedBacking {}

impl SharedBacking {
    fn new(mut backing: Backing) -> SharedBacking {
        let whole = backing.as_mut_slice();
        let (base, len) = (whole.as_mut_ptr(), whole.len());
        SharedBacking { _backing: backing, base, len }
    }

    /// The address of `offset..offset + len`, or `None` if that is not inside
    /// the region.
    ///
    /// A pointer rather than a slice, so that the two places a *reference* is
    /// made out of it are the two places the argument for making one has to be
    /// written down. Handing back a `&mut [u8]` from `&self` would put the whole
    /// claim in one function and its callers would inherit it silently — which
    /// is also what `clippy::mut_from_ref` is about.
    fn range(&self, offset: usize, len: usize) -> Option<*mut u8> {
        if offset.checked_add(len)? > self.len {
            return None;
        }
        // SAFETY: `offset + len <= self.len`, so this stays inside the mapping.
        Some(unsafe { self.base.add(offset) })
    }
}

/// Which free run an allocation takes.
///
/// # STAGE 16, and it is a choice so that the choice could be measured
///
/// Stage 12's allocator is [`Policy::FirstFit`] and it fragmented under a video
/// (docs/waycompose-design.md §20.3): `arena 0 has no run of 2342912 bytes (3932160 free,
/// largest 1744896)`. Two-thirds more free space than the request, in pieces
/// none of which was big enough.
///
/// Replacing it outright would have left that sentence resting on a claim about
/// the code rather than on a measurement, which is the §8.4 rule. So the old
/// policy stays, reachable by `--arena-policy first-fit`, and
/// `a_video_sized_churn_fragments_first_fit_and_not_the_default` runs the same
/// synthetic churn through both in one process. §20.4 has the matrix that chose
/// between six candidates, and it is worth reading before changing this,
/// because **the obvious answer was wrong**: rounding requests to size classes,
/// TLSF-style, made every occupancy *worse*, and what did the work was
/// choosing the run rather than reshaping the request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    /// Stage 12: **first fit**, page granularity. The lowest run that is big
    /// enough, split at its head, remainder left where it is.
    FirstFit,
    /// Stage 16: **best fit, with small pools taken from the far end**. Page
    /// granularity, as before — the change is entirely in *which* run is
    /// chosen and from which end it is cut.
    ///
    /// Two independent halves, both measured in §20.4's matrix:
    ///
    /// * **Best fit.** The smallest adequate run rather than the lowest. First
    ///   fit shaves the low end of whatever large run it meets first, so a
    ///   region's big runs are eaten by requests that a smaller hole would have
    ///   held perfectly well — "lowest" and "smallest" are unrelated, and only
    ///   one of them preserves the ability to answer a large request later.
    /// * **Segregating the small pools.** A pool of at most [`SMALL_POOL`]
    ///   bytes is cut from the *highest* run, at that run's tail. Cursors,
    ///   icons and tooltips are this size and, in Firefox's case, permanent —
    ///   and one permanent 8 KiB extent in the middle of the region is what
    ///   stops the megabyte holes on either side of it from ever coalescing.
    ///   Sending them to the far end costs nothing and keeps the space the
    ///   video competes for in one piece.
    ///
    /// The two together refuse nothing at the occupancy where first fit refuses
    /// 62 pools, and a fifth as many at the occupancy where first fit refuses
    /// 633 (§20.4).
    #[default]
    BestFitSegregated,
}

impl Policy {
    pub fn parse(s: &str) -> Option<Policy> {
        match s {
            "first-fit" => Some(Policy::FirstFit),
            "best-fit" => Some(Policy::BestFitSegregated),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Policy::FirstFit => "first-fit",
            Policy::BestFitSegregated => "best-fit",
        }
    }
}

/// A sensible size for a **dedicated outbound region**, from what it holds.
///
/// # Why the outbound window gets its own region now
///
/// `reserve_outbound` argued for putting the window at the tail of the data
/// region like this: "the tail is the part of the region they reach last.
/// Reserving it means the peer has one number to subtract from its own length
/// rather than a hole to route its free list around."
///
/// **That argument's premise was one region, and multi-region removed it.** With
/// a `Vec<Slot>` the peer subtracts nothing — it simply never allocates from
/// that region — so the tail's only advantage is gone. Its disadvantage is not:
/// a region carrying live keymaps can never be reclaimed, so whatever the
/// session started at became a permanent floor. An `sv-guest` was found holding
/// 817 MB with no client connected, and that floor is why.
///
/// Splitting them makes the floor the *outbound* size instead of the *startup*
/// size, and every data region — including the first — fully reclaimable.
///
/// # The number, from evidence
///
/// The window's first and largest customer is a keymap: waycompose §15.11
/// measured the `us` keymap the client receives at **35,572 bytes** and the
/// `de` keymap after a layout change at **37,532 bytes**, which §18 rounds to
/// exactly ten pages. A layout change mints a new one before the old is
/// dropped, so one keymap is not enough and two is the real minimum.
///
/// 256 KiB is six keymaps. It is chosen with room rather than at the minimum
/// because the other customers are known and coming — the clipboard offer and
/// the icon feed both mint objects this way — and because being wrong upward
/// here costs 256 KiB while being wrong downward costs a growth on the path
/// that hands a client its keyboard. Against the 768 MiB floor it replaces it
/// is 0.03%.
///
/// It is not a ceiling: an outbound region that fills grows like any other
/// (see [`Arena::reclaim_regions`] for what stays pinned and why).
pub const OUTBOUND_REGION_BYTES: u64 = 256 << 10;

/// The size of the first region growth asks for, and the base of the geometric
/// series after it.
///
/// # Why 16 MiB, and why not `--arena-size`
///
/// The step used to default to whatever the arena started at, which meant a
/// session started at 768 MiB grew in 768 MiB jumps — the first growth doubling
/// a large arena. That was not a number anybody chose; it was two unrelated
/// quantities sharing a default.
///
/// 16 MiB is chosen from the pool sizes clients actually ask for. The pressure
/// trace has Firefox creating pools of **4608000** and **18837504** bytes, and
/// the video pool that started all of this is **2340864**. So one 16 MiB region
/// holds a video's worth of pools several times over, and the one pool larger
/// than the step gets a region sized exactly for it — [`Arena::grow`] asks for
/// `max(want, step)` precisely so that a request bigger than the step is served
/// by one region rather than failing against a series of regions none of which
/// can hold it.
///
/// # Geometric, because the cost of growing is the locate
///
/// Growing costs 6.7 us of bookkeeping on the host and **~0.4 s of locate** on
/// the deployment (§0.14: 0.064 s host + ~330 ms guest, and 2.4 s if the PFN
/// query is not used). What matters is therefore the *number* of growths, not
/// their size, and a fixed small step turns a 768 MiB working set into 48
/// locates. So the step doubles: 16, 32, 64 MiB, and then stays at 64.
///
/// It stays rather than doubling forever because the other cost is overshoot —
/// memory committed and not used — and that is the thing the user actually
/// complained about. Capping at [`GROW_STEP_CAP`] bounds the overshoot at one
/// region's worth while still reaching 768 MiB in fourteen growths rather than
/// forty-eight.
pub const GROW_STEP: u64 = 16 << 20;

/// The largest a single growth will ask for. See [`GROW_STEP`] — this bounds
/// how much can be committed and then not used.
pub const GROW_STEP_CAP: u64 = 64 << 20;

/// A pool at most this big is allocated from the far end of the region.
///
/// 64 KiB is chosen from what clients actually create rather than from a round
/// number: a 32x32 cursor is 4096 bytes, a 128x128 icon is 65536, and every
/// `wl_shm` pool Firefox opens for content is at least fifteen times this. So
/// the line falls in a genuine gap in the distribution, which is what makes
/// "small" a fact about the client rather than a knob.
pub const SMALL_POOL: u64 = 64 * 1024;

/// The whole of **one region's** allocator state, behind the arena's lock.
///
/// It is one struct rather than four fields so that "the free list, the live
/// map and the counters move together" is a property of the type instead of a
/// thing every method has to remember.
#[derive(Default)]
struct Alloc {
    free: Vec<(u64, u64)>,
    live: HashMap<u64, (u64, u64)>,
    published: u64,
    policy: Policy,
}

/// One mapped region of a multi-region arena: the bytes, and the facts about
/// them that never change once it is mapped.
///
/// DYNAMIC ARENA. This is what used to be spread across [`ArenaInner`]'s
/// `region`, `origin`, `size` and `outbound` fields, when there was exactly one
/// of each.
///
/// # A pool may not straddle two regions, and that is a refusal rather than a
/// gap
///
/// Two regions are two separate `VirtualAlloc`s in the guest, located at two
/// unrelated ranges of QEMU's backing object and `mmap`ped at two unrelated
/// host addresses. There is no arithmetic that reaches from one into the other.
/// A buffer spanning both would therefore have to be described to the
/// compositor as a *list* of runs — which is precisely the guest's own page
/// layout, and docs/design.md §5.7 is explicit that "the run list does not cross the
/// wire, and it never did have to".
///
/// So `Descriptor::ArenaSlice { arena, offset, len }` names one region and one
/// contiguous range of it, and a pool too large for any single region is
/// refused rather than split. This is the same refusal `sv_arena::handle::
/// Slice::validate` makes for a handle spanning two slabs (D4): naming memory
/// that is two imports is naming something nobody ever mapped as one thing.
struct RegionMap {
    /// The number in `Descriptor::ArenaSlice { arena, .. }`. **An integer, and
    /// never a descriptor** (docs/waycompose-design.md §1a): growth adds regions, and a region
    /// is named the same way the first one always was.
    id: u32,
    backing: SharedBacking,
    /// How the region was obtained, for the one log line that says it.
    origin: String,
    len: u64,
    /// The peer's outbound window, `(offset, len)`, which this allocator has
    /// been told to stay out of. See [`Arena::reserve_outbound`].
    ///
    /// Per region, because `one-binary.md` §3.1a is emphatic that the two
    /// ranges must not be merged into one free list: one holds client pixels,
    /// the other holds objects the compositor has already handed to clients and
    /// which are still live. A grown region has no outbound window at all — the
    /// compositor declared one, once, in the region it committed at startup,
    /// and nothing about growth gives it a second.
    outbound: Option<(u64, u64)>,
}

/// One region and the allocator over it.
struct Slot {
    map: Arc<RegionMap>,
    alloc: Alloc,
    /// The tick at which this region last became empty, or `None` while it
    /// holds anything. See [`Arena::reclaim_regions`].
    idle_since: Option<u64>,
}

/// Every region, the one key space over all of them, and the counters growth
/// and release are judged by.
struct Arenas {
    slots: Vec<Slot>,
    /// Allocation keys are unique across *regions*, not within one, so a
    /// `Region` can be looked up without being told which region it is in.
    next_key: u64,
    /// The next region id to hand out. Monotonic and **never reused**: a
    /// released region's id must not come back, because a client may still name
    /// it in a request already in flight, and answering that with a different
    /// region's bytes is the silent wrong answer this file exists to prevent.
    next_id: u32,
    policy: Policy,
    /// Advanced by the session once per commit. The unit of the release policy
    /// is a frame rather than a second, because what makes a pool cold is that
    /// nothing drew with it, and a clock cannot tell a cold pool from a paused
    /// video.
    tick: u64,
    grown: u32,
    grow_nanos: u64,
    grow_refused: u32,
    released_regions: u32,
    released_bytes: u64,
    /// The largest this arena has ever been: the maximum, over the run, of the
    /// sum of the sizes of the regions committed **at one moment**.
    ///
    /// # This is the number a memory question is actually asking
    ///
    /// It exists because a throughput figure was published next to a memory
    /// question and was read as one. A run of this proxy copied **1158 MiB**
    /// under growth, and that is cumulative traffic across 262 copies — every
    /// frame of a video, counted again each time it is drawn. The bytes *held*
    /// at the peak were a small fraction of it. The two numbers differ by the
    /// number of frames, so they are never close and the larger one is always
    /// the traffic; publishing only that invites exactly the reading it got.
    ///
    /// So peak is a first-class figure, reported beside throughput and never
    /// instead of it, and [`Arenas::peak_regions`] goes with it because "1.2
    /// GiB" and "1.2 GiB in one region" and "1.2 GiB in nineteen" are three
    /// different facts about whether release can ever help.
    peak_bytes: u64,
    /// How many regions were committed at the moment [`Arenas::peak_bytes`] was
    /// reached. Release can only ever return regions, so the floor this arena
    /// can fall back to is the startup region's size however many are added —
    /// see [`Arena::reclaim_regions`], which never returns it.
    peak_regions: usize,
    /// The base of the geometric growth series. See [`GROW_STEP`].
    grow_step: u64,
    /// Frames a region must be empty before it is returned. Zero means never.
    /// See [`Arena::reclaim_regions`] for why this is frames and not seconds.
    reclaim_idle: u64,
}

impl Arenas {
    /// Record the high-water mark, after anything that changes what is
    /// committed.
    ///
    /// Called on growth and at construction, and deliberately **not** on
    /// release: a peak that fell when memory was returned would not be a peak.
    fn note_peak(&mut self) {
        let now = self.size();
        if now > self.peak_bytes {
            self.peak_bytes = now;
            self.peak_regions = self.slots.len();
        }
    }
}

/// Where another region comes from when the arena runs out.
///
/// # Why this is a port and not a function
///
/// Because the two substrates get a region by genuinely different means and
/// only one of them is the deployment. Over guest RAM the guest commits, pins
/// and stamps a region and the *host* goes and finds it (waycompose §12.2: "the
/// side inside the VM allocates, because only it can produce pages the
/// hypervisor can locate"), which is a round trip through a program running at
/// a different privilege. Over a file — the Wine fixture, and only ever the
/// Wine fixture — it is one `set_len`.
///
/// Making it a trait keeps the second from pretending to be the first.
/// docs/waycompose-design.md §5.1's rule is that a failed locate fails the run and there is
/// deliberately no fallback, so a `RegionSource` that quietly served a file
/// when guest RAM was asked for would be exactly the "same-machine path wearing
/// the design's clothes" §1a warns about. Each implementation serves one
/// substrate and refuses rather than substitutes.
pub trait RegionSource: Send {
    /// Obtain a region of at least `min_len` bytes, to be known as `id`.
    ///
    /// Returning `Err` is normal and survivable — the guest can simply be out
    /// of RAM — and the error must carry the size asked for and whatever the
    /// source can say about why, because that message is the only evidence a
    /// user will have. `wc_handle::arena::give`'s `OutOfMemory` is the house
    /// style and the one this follows.
    fn provide(&mut self, id: u32, min_len: u64) -> io::Result<ProvidedRegion>;

    /// Hand a region back. Called once the arena has stopped listing it and
    /// nothing can still be naming its bytes.
    ///
    /// Best-effort by construction: the mapping is already gone by the time
    /// this runs, so a source that cannot tell the guest to decommit has
    /// nothing to do and says so by doing nothing.
    fn release(&mut self, _id: u32) {}

    /// What this source is, for the log line that says where a region came
    /// from — the one difference a reader of the output can see between the
    /// deployment and the fixture.
    fn describe(&self) -> String;
}

/// What an arena is holding, as a thing that can be printed.
///
/// Exists so that the memory figure and the throughput figures cannot be
/// confused for one another: this type carries only bytes *held*, it names
/// itself "resident" wherever it is rendered, and it says so next to the
/// throughput line rather than instead of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryReport {
    pub arena: u32,
    /// The maximum, over the run, of the sum of region sizes committed at once.
    pub peak_bytes: u64,
    pub peak_regions: usize,
    pub now_bytes: u64,
    pub now_regions: usize,
    /// The startup region, which is never returned. Memory cannot fall below
    /// this, which is the argument for starting small.
    pub floor_bytes: u64,
    pub grown: u32,
    pub released_regions: u32,
    pub released_bytes: u64,
}

impl core::fmt::Display for MemoryReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        write!(
            f,
            "arena {} memory: peak resident {:.1} MiB in {} region(s); \
             now {:.1} MiB in {} region(s); floor {:.1} MiB (the startup region, \
             never returned); grew {}, released {} region(s) / {:.1} MiB \
             [held, not copied]",
            self.arena,
            mib(self.peak_bytes),
            self.peak_regions,
            mib(self.now_bytes),
            self.now_regions,
            mib(self.floor_bytes),
            self.grown,
            self.released_regions,
            mib(self.released_bytes),
        )
    }
}

/// A region a [`RegionSource`] produced.
///
/// Constructed only through the two functions below, which is what keeps
/// [`Backing`] private. That is not tidiness: the two substrates are not
/// interchangeable — docs/waycompose-design.md §5.1 says a failed locate fails the run and
/// there is deliberately no fallback — and a constructor taking an arbitrary
/// backing would be the seam through which a file quietly stood in for guest
/// RAM.
pub struct ProvidedRegion {
    backing: Backing,
    origin: String,
    len: u64,
}

impl ProvidedRegion {
    /// A region of the guest's RAM, described by a plan the locator wrote.
    ///
    /// The deployment path. Everything expensive already happened: the guest
    /// committed, pinned and stamped the pages and the host located them. What
    /// is left is `mmap(MAP_FIXED)` per run into one reservation, which §12.5
    /// measured at **4.16–4.24 ms** for ~1510 runs.
    pub fn guest_ram(plan: &crate::guestram::Plan) -> io::Result<ProvidedRegion> {
        let region = crate::guestram::StitchedRegion::attach(plan)?;
        let origin = format!(
            "guest RAM, run 0x{:016x}, {} run(s) of {}",
            plan.run_id,
            region.runs(),
            region.origin()
        );
        Ok(ProvidedRegion { backing: Backing::GuestRam(region), origin, len: plan.len })
    }

    /// A region backed by a file on this machine.
    ///
    /// **The Wine fixture, and only ever the Wine fixture.** docs/waycompose-design.md §5.1 is
    /// explicit that guest RAM is the substrate and that the file-backed arena
    /// exists so a machine with no hypervisor can run the same code. A
    /// deployment that reached this by accident would be a same-machine path
    /// wearing the design's clothes, so the caller has to ask for it by name.
    pub fn file(path: &Path, len: u64) -> io::Result<ProvidedRegion> {
        let len = len.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let name = path.to_string_lossy().into_owned();
        let handle = if path.exists() {
            Handle::open_shared_file(&name, &name, len)?
        } else {
            Handle::create_shared_file(&name, &name, len)?
        };
        let view = handle.map_mut()?;
        Ok(ProvidedRegion {
            backing: Backing::File { _handle: handle, view },
            origin: name,
            len,
        })
    }
}

/// Growth over guest RAM: re-read the plan and take up any region the locator
/// has added since the last look.
///
/// # Why the proxy reads a file rather than asking for memory
///
/// Because it cannot ask. waycompose §12.2: "the side inside the VM allocates,
/// because only it can produce pages the hypervisor can locate", and the host
/// then *finds* them. This process is on the host and has no channel into the
/// guest at all — the guest says a 64-bit run id and the host goes and reads
/// its memory. So the shape of growth is the shape of the original handshake,
/// run again: the guest commits and stamps another region, the locator finds it
/// and **appends** it to the plan, and this notices.
///
/// Appending rather than rewriting is what makes it work at all. The first pool
/// writes pixels over the stamps of the region it lands in (§12.10), so a
/// locator that rewrote the plan from scratch would have to re-locate regions
/// whose stamps are gone — which the content sweep cannot do, and which is
/// exactly stage 16's objection to growth. `SuperfetchPfnQuery` answers it
/// (§0.14 item 4) and is what makes a rewrite safe; until it is wired, append
/// is what is *correct*, and this reads an appended plan either way.
pub struct PlanGrowth {
    path: PathBuf,
    /// Region ids already taken up, so the same region is never mapped twice.
    taken: Vec<u32>,
}

impl PlanGrowth {
    pub fn new(path: &Path, already: &[u32]) -> PlanGrowth {
        PlanGrowth { path: path.to_path_buf(), taken: already.to_vec() }
    }
}

impl RegionSource for PlanGrowth {
    fn provide(&mut self, _id: u32, min_len: u64) -> io::Result<ProvidedRegion> {
        let plans = crate::guestram::Plan::read_all(&self.path)?;
        let Some(plan) = plans
            .iter()
            .find(|p| !self.taken.contains(&p.id) && p.len >= min_len)
        else {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "{} names {} region(s) and none is both unmapped and at least \
                     {min_len} bytes: the guest has not committed one, or the locator \
                     has not appended it yet",
                    self.path.display(),
                    plans.len()
                ),
            ));
        };
        let provided = ProvidedRegion::guest_ram(plan)?;
        self.taken.push(plan.id);
        Ok(provided)
    }

    fn describe(&self) -> String {
        format!("guest RAM, as the locator appends it to {}", self.path.display())
    }
}

/// Growth over files, for the fixture. One file per region, in a directory.
///
/// Named apart from [`PlanGrowth`] rather than being a mode of it, because a
/// deployment must not be able to reach this by misconfiguration — see
/// [`ProvidedRegion::file`].
pub struct FileGrowth {
    dir: PathBuf,
    step: u64,
}

impl FileGrowth {
    /// `step` is a floor on a region's size. It is normally zero, because
    /// [`Arena::grow`] already applies the geometric series and asks for
    /// `max(want, step)`; a second floor here would be a second policy.
    pub fn new(dir: &Path, step: u64) -> FileGrowth {
        FileGrowth { dir: dir.to_path_buf(), step }
    }
}

impl RegionSource for FileGrowth {
    fn provide(&mut self, id: u32, min_len: u64) -> io::Result<ProvidedRegion> {
        // `min_len` is what the arena asked for and it already carries the
        // growth policy. Serving *more* than was asked for would be this
        // fixture inventing memory the deployment would not have committed.
        ProvidedRegion::file(&self.dir.join(format!("region-{id}")), min_len.max(self.step))
    }

    fn release(&mut self, id: u32) {
        // The fixture's analogue of the guest decommitting: the bytes stop
        // being charged to anybody the moment the name is gone and the last
        // mapping is dropped. `du` on the directory is what "the memory came
        // back" looks like here.
        let _ = std::fs::remove_file(self.dir.join(format!("region-{id}")));
    }

    fn describe(&self) -> String {
        format!("files under {} in {}-byte steps (fixture)", self.dir.display(), self.step)
    }
}

/// The regions, the one allocator over them, and everything that does not
/// change once a client exists.
struct ArenaInner {
    /// The id of the region committed at startup. `Arena::id()` still answers
    /// this, because it is what every log line and every existing harness means
    /// by "the arena".
    id: u32,
    arenas: Mutex<Arenas>,
    /// `None` means this arena cannot grow, which is every arena built before
    /// this stage and is still the right answer for a test that wants to
    /// observe a refusal.
    source: Mutex<Option<Box<dyn RegionSource>>>,
}

pub struct Arena {
    inner: Arc<ArenaInner>,
}

/// The allocator's granularity, and the alignment of every offset it returns.
/// See the note on [`Arena`].
///
/// **One definition, in the crate that owns the arena.** This was a literal
/// `4096` while shared_vulkan was a different repository; it is now
/// `sv_arena::GRANULARITY`, which is where the number is argued from the
/// measured `minImportedHostPointerAlignment` and the page granularity of the
/// shared mapping. `wc_handle::arena::GIFT_GRANULARITY` is the same constant
/// for the same reason. one-binary.md §1.1.
pub const ARENA_GRANULARITY: u64 = sv_arena::GRANULARITY;

impl Arena {
    /// Take the region at `local_path` as arena `id`, creating it only if it is
    /// not already there.
    ///
    /// **Open-or-create rather than create**, and that is not a convenience.
    /// An arena is provided *out of band*, before either side starts, by
    /// whatever brought the two sides together — shared_vulkan's cold-path
    /// handshake in the real deployment, a harness here. Both the compositor
    /// and this process attach to the same region, and whichever starts first
    /// must not be able to pull it out from under the other. An earlier version
    /// unlinked and re-created it, and the compositor mapped a file that no
    /// longer had a name: it read zeroes forever and reported nothing wrong.
    ///
    /// The peer is told `id` and nothing else about it. Describing the arena in
    /// the Wayland stream instead would mean the first pool of the first client
    /// decided where every later one lived.
    pub fn open_or_create(id: u32, local_path: &Path, size: u64) -> io::Result<Arena> {
        let size = size.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY;
        if let Some(parent) = local_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let path = local_path.to_string_lossy().into_owned();
        // The peer path is unused: an arena slice names no path at all. It is
        // passed as the local one so the descriptor the `Handle` carries is at
        // least truthful about the object this process holds.
        let handle = if local_path.exists() {
            Handle::open_shared_file(&path, &path, size)?
        } else {
            Handle::create_shared_file(&path, &path, size)?
        };
        let view = handle.map_mut()?;
        Ok(Arena::over(id, Backing::File { _handle: handle, view }, &path, size))
    }

    /// A second handle on **the same region and the same allocator**, for
    /// another Wayland connection.
    ///
    /// STAGE 12, and it is the whole of docs/waycompose-design.md §16.2. Until this stage
    /// `--transport arena` refused the second connection by name, on the grounds
    /// that there is one region and one allocator (§12.13 item 6) — and the
    /// premise was right while the conclusion was not. **One allocator is the
    /// invariant; one client was an artefact of `Box<dyn PoolTransport>` being
    /// moved into one session's thread.** Four connections through one allocator
    /// cannot overlap, for exactly the reason two pools of one connection
    /// cannot: the free list is the only thing that decides an offset, and there
    /// is still only one of it.
    ///
    /// What this is *not* is a second arena id. §15.14 guessed that
    /// multi-connection support would be "one arena id per client, a
    /// `shared_vulkan` cold-path handshake question", and that would have been a
    /// second **region** — a second `attach_guest_ram` in the guest, a second
    /// 13 GiB sweep on the host (2.4 s, §12.5) and a second plan, per
    /// connection, four times over for one Firefox. The id names the region, not
    /// the client, and nothing about a region wants to be per-client.
    pub fn share(&self) -> Arena {
        Arena { inner: Arc::clone(&self.inner) }
    }

    /// How many sessions hold this arena, for the log line that says so.
    pub fn sharers(&self) -> usize {
        Arc::strong_count(&self.inner)
    }

    /// The allocator, with poisoning treated as recoverable.
    ///
    /// Nothing under this lock does anything but arithmetic on a `Vec` and a
    /// `HashMap`, so a panic elsewhere in a session thread cannot have left the
    /// free list half-updated. Refusing to serve the other three connections
    /// because a fourth panicked would be turning a contained failure into a
    /// total one, which is the opposite of what the accept loop is for.
    fn alloc(&self) -> MutexGuard<'_, Arenas> {
        self.inner.arenas.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the region `wc-arena-host` located in the guest as arena `id`.
    ///
    /// STAGE 7, and the whole of §10.13 item 1 on this side. The plan names an
    /// object and a list of runs; [`crate::guestram`] opens the one and maps the
    /// others. Nothing about the *arena* changes — the allocator below is the
    /// same code, the descriptor is the same three integers, and the compositor
    /// cannot tell which of these two constructors ran.
    ///
    /// # The size comes from the plan, not from a flag
    ///
    /// `--arena-size` is meaningless here: the guest allocated the region and
    /// its length is a fact about what it allocated. A flag that disagreed
    /// would either truncate the arena — leaving guest pages nobody uses — or
    /// run off the end of it, and the second is a fault in the proxy on the
    /// first large client. Taking it from the plan removes the disagreement.
    ///
    /// # A plan may name more than one region
    ///
    /// DYNAMIC ARENA. All of them are taken up here, in the order the plan
    /// lists them, and the *first* is the startup region — the one whose id
    /// `Arena::id()` answers and the only one that may carry the compositor's
    /// outbound window. A plan written before this stage names one region and
    /// means exactly what it always did.
    ///
    /// `id` still names the startup region because it is what `--arena-id` has
    /// always meant and what the compositor was told; a plan that numbers its
    /// first region differently is refused rather than quietly renumbered,
    /// because the compositor and this proxy disagreeing about which number is
    /// which is the failure `outbound` travelling in the plan exists to prevent.
    pub fn from_plan(id: u32, plan_path: &Path) -> io::Result<Arena> {
        let plans = crate::guestram::Plan::read_all(plan_path)?;
        let first = &plans[0];
        if first.id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{}: the first region is {} and --arena-id says {id}; the compositor \
                     was told one number and this would map another",
                    plan_path.display(),
                    first.id
                ),
            ));
        }
        for plan in &plans {
            if plan.len % ARENA_GRANULARITY != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "region {}: a {}-byte region is not a whole number of \
                         {ARENA_GRANULARITY}-byte pages",
                        plan.id, plan.len
                    ),
                ));
            }
        }
        let provided = ProvidedRegion::guest_ram(first)?;
        let mut arena =
            Arena::over(id, provided.backing, &provided.origin, provided.len);
        if let Some((_, len)) = first.outbound {
            arena.reserve_outbound(len)?;
        }
        for plan in &plans[1..] {
            let provided = ProvidedRegion::guest_ram(plan)?;
            let mut a = arena.alloc();
            Arena::adopt(
                &mut a,
                RegionMap {
                    id: plan.id,
                    backing: SharedBacking::new(provided.backing),
                    origin: provided.origin,
                    len: provided.len,
                    outbound: None,
                },
            );
        }
        Ok(arena)
    }

    /// Every region id this arena currently holds, for handing to a
    /// [`PlanGrowth`] so it does not map one twice.
    pub fn region_ids(&self) -> Vec<u32> {
        self.alloc().slots.iter().map(|s| s.map.id).collect()
    }

    /// Take the last `len` bytes of this arena out of the free list, because the
    /// peer declared them **its** to hand to clients.
    ///
    /// STAGE 10 and docs/waycompose-design.md §12.11 item 3. The allocator above owns the
    /// whole region by default, which was true while the only things in an arena
    /// were `wl_shm` pools this process published. A compositor that can create
    /// objects for its clients is a second allocator over the same bytes, and two
    /// allocators handing out the same offsets is the failure this exists to make
    /// impossible: it would put a client's pixels on top of another client's
    /// keymap, and it would do it silently.
    ///
    /// The tail rather than an arbitrary hole, because the free list is a sorted
    /// run list and shortening it is exact; a hole would be a special case in the
    /// one piece of code that must not have one.
    ///
    /// **This must be called before the first pool**, and it is: both callers do
    /// it during construction, from the plan or from a flag, before any client
    /// exists. It refuses rather than assumes, so a later call with anything
    /// allocated is an error and not a quietly moved boundary.
    /// **STAGE 12 gave it a third refusal, and the type does the work.** It
    /// takes `&mut self` and reaches the region through `Arc::get_mut`, so an
    /// arena that any session already holds cannot have its window moved: the
    /// refcount *is* "before the first client", which is stronger than the free
    /// list being untouched and is checked at the only moment it could go wrong.
    pub fn reserve_outbound(&mut self, len: u64) -> io::Result<()> {
        let Some(inner) = Arc::get_mut(&mut self.inner) else {
            return Err(io::Error::other(
                "the peer's outbound window must be reserved before any client is served: \
                 this arena is already shared with a session",
            ));
        };
        let arenas = inner.arenas.get_mut().unwrap_or_else(|e| e.into_inner());
        // The window belongs to the region the compositor committed at startup,
        // which is the first one. A grown region is memory the compositor never
        // declared anything in.
        if arenas.slots.len() != 1 {
            return Err(io::Error::other(
                "the peer's outbound window must be reserved before the arena has grown: \
                 the window is a fact about the region the compositor committed, and a \
                 later region is not that region",
            ));
        }
        let size = arenas.slots[0].map.len;
        if arenas.slots[0].alloc.published > 0 || arenas.slots[0].alloc.free != vec![(0, size)] {
            return Err(io::Error::other(
                "the peer's outbound window must be reserved before anything is allocated",
            ));
        }
        // A window of the *whole* region is the dedicated-outbound-region case
        // and is now the preferred shape: it is what lets every data region be
        // reclaimed, because the thing that must never be unmapped lives in a
        // region of its own rather than on the tail of one that would otherwise
        // be free to go. "Leaves nothing of the arena" was the right refusal
        // when there was one region and the arena was that region; with several
        // it leaves the other regions, which is the point.
        if len == 0 || len > size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("an outbound window of {len} bytes does not fit in a {size}-byte region"),
            ));
        }
        let len = len.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY;
        let slot = &mut arenas.slots[0];
        // `RegionMap` is behind an `Arc` because a released region's mapping
        // outlives the allocator's listing of it. Nothing is sharing this one
        // yet — `Arc::get_mut` on the outer handle has already proved no session
        // holds the arena — so the window can still be written into it.
        Arc::get_mut(&mut slot.map)
            .expect("no session holds the arena, so nothing holds its first region")
            .outbound = Some((size - len, len));
        slot.alloc.free = vec![(0, size - len)];
        Ok(())
    }

    /// The peer's window, `(offset, len)`, for the log line that says this proxy
    /// knows about it. `None` is the answer for every deployment before stage 10
    /// and for every one whose compositor creates nothing.
    ///
    /// The window of the *startup* region, which is the only one that has one.
    pub fn outbound(&self) -> Option<(u64, u64)> {
        let arenas = self.alloc();
        arenas.slots.iter().find(|s| s.map.id == self.inner.id)?.map.outbound
    }

    fn over(id: u32, backing: Backing, origin: &str, size: u64) -> Arena {
        Arena {
            inner: Arc::new(ArenaInner {
                id,
                arenas: Mutex::new(Arenas {
                    slots: vec![Slot::new(
                        Arc::new(RegionMap {
                            id,
                            backing: SharedBacking::new(backing),
                            origin: origin.to_owned(),
                            len: size,
                            outbound: None,
                        }),
                        Policy::default(),
                    )],
                    next_key: 0,
                    next_id: id.saturating_add(1),
                    policy: Policy::default(),
                    tick: 0,
                    grown: 0,
                    grow_nanos: 0,
                    grow_refused: 0,
                    released_regions: 0,
                    released_bytes: 0,
                    peak_bytes: size,
                    peak_regions: 1,
                    grow_step: GROW_STEP,
                    reclaim_idle: 0,
                }),
                source: Mutex::new(None),
            }),
        }
    }

    /// Add a region this arena already holds a mapping for.
    ///
    /// DYNAMIC ARENA. The plan-reading path uses this for every region after
    /// the first, and growth uses it for the ones it obtains later. Both go
    /// through one function so that "a region joins the arena" has one spelling
    /// and one place to get the bookkeeping right.
    fn adopt(arenas: &mut Arenas, map: RegionMap) -> u32 {
        let id = map.id;
        let policy = arenas.policy;
        arenas.slots.push(Slot::new(Arc::new(map), policy));
        arenas.next_id = arenas.next_id.max(id.saturating_add(1));
        arenas.note_peak();
        id
    }

    pub fn id(&self) -> u32 {
        self.inner.id
    }

    /// Where the region came from, for the log line that is the only difference
    /// a reader of the output can see between the two substrates.
    ///
    /// The startup region's origin. A grown arena says how it grew through
    /// [`Arena::growth`] rather than by making this string longer every time,
    /// because this one is printed once at startup and the other is the thing
    /// that changes.
    pub fn origin(&self) -> String {
        let arenas = self.alloc();
        arenas
            .slots
            .iter()
            .find(|s| s.map.id == self.inner.id)
            .map(|s| s.map.origin.clone())
            .unwrap_or_default()
    }

    /// Every byte this arena currently holds, across every region.
    pub fn size(&self) -> u64 {
        self.alloc().size()
    }

    /// How many regions are mapped right now.
    pub fn regions(&self) -> usize {
        self.alloc().slots.len()
    }

    /// `(times grown, nanoseconds spent growing, times growth was refused,
    /// regions released, bytes released)`.
    ///
    /// The numbers a run has to print for growth to be a claim rather than a
    /// hope. The second is the one that decides whether this is a product: a
    /// 2.3 s stall mid-video and a 64 ms one are different features, and only a
    /// measurement can say which was built.
    pub fn growth(&self) -> (u32, u64, u32, u32, u64) {
        let a = self.alloc();
        (a.grown, a.grow_nanos, a.grow_refused, a.released_regions, a.released_bytes)
    }

    /// **Peak resident arena bytes**, and how many regions were committed at
    /// that moment.
    ///
    /// The answer to "how much memory is this using", which is a different
    /// question from every throughput figure this proxy prints and must never
    /// be read off one. See [`Arenas::peak_bytes`].
    ///
    /// Resident in the sense that matters here: bytes the guest has committed
    /// and pinned for the arena and which the host has mapped. It is not an
    /// RSS reading of either process — the host's mapping of guest RAM is the
    /// same physical pages the guest already counted, so adding the two would
    /// double-count every byte.
    pub fn peak(&self) -> (u64, usize) {
        let a = self.alloc();
        (a.peak_bytes, a.peak_regions)
    }

    /// The least this arena can ever shrink to: the startup region, which
    /// [`Arena::reclaim_regions`] never returns.
    ///
    /// # Why there is a floor at all, and what it argues for
    ///
    /// The startup region carries the compositor's outbound window — keymaps
    /// and clipboard offers the compositor has already handed to clients, which
    /// are live in those clients' hands (`one-binary.md` §3.1a). Unmapping the
    /// region would take them with it, so it stays for the life of the session
    /// however empty it gets.
    ///
    /// That exemption is correct and it has a consequence worth stating
    /// plainly: **memory can never fall below the size the session started at.**
    /// A session started at 768 MiB has a 768 MiB floor and release can do
    /// nothing about it, which is the argument for starting small and growing
    /// rather than for sizing the startup region generously.
    pub fn floor(&self) -> u64 {
        self.alloc().floor()
    }

    /// Give this arena a way to obtain more memory.
    ///
    /// `&mut self`, for the same reason [`Arena::reserve_outbound`] takes it:
    /// an arena that acquired the ability to grow half way through a session
    /// would make the log of what it did depend on when the flag arrived.
    pub fn set_source(&mut self, source: Box<dyn RegionSource>) -> io::Result<()> {
        let Some(inner) = Arc::get_mut(&mut self.inner) else {
            return Err(io::Error::other(
                "the region source must be set before any client is served: \
                 this arena is already shared with a session",
            ));
        };
        *inner.source.get_mut().unwrap_or_else(|e| e.into_inner()) = Some(source);
        Ok(())
    }

    /// The base of the geometric growth series, in bytes. Zero keeps
    /// [`GROW_STEP`], which is the number argued from measured pool sizes.
    pub fn set_grow_step(&mut self, bytes: u64) -> io::Result<()> {
        let Some(inner) = Arc::get_mut(&mut self.inner) else {
            return Err(io::Error::other(
                "the growth step must be chosen before any client is served: \
                 this arena is already shared with a session",
            ));
        };
        if bytes != 0 {
            inner.arenas.get_mut().unwrap_or_else(|e| e.into_inner()).grow_step = bytes;
        }
        Ok(())
    }

    /// How many frames a region must be empty before [`PoolTransport::frame`]
    /// returns it. Zero — the default — never returns one.
    ///
    /// Off unless asked for, like growth itself, so that a run that did not opt
    /// into this stage behaves exactly as it did before it.
    pub fn set_reclaim_idle(&mut self, frames: u64) -> io::Result<()> {
        let Some(inner) = Arc::get_mut(&mut self.inner) else {
            return Err(io::Error::other(
                "the reclamation threshold must be chosen before any client is served: \
                 this arena is already shared with a session",
            ));
        };
        inner.arenas.get_mut().unwrap_or_else(|e| e.into_inner()).reclaim_idle = frames;
        Ok(())
    }

    /// Choose how this allocator picks a run. See [`Policy`].
    ///
    /// Takes `&mut self` for the same reason [`Arena::reserve_outbound`] does,
    /// and it is not about the lock: changing the policy once extents are out
    /// would produce a free list whose runs were sized by one rule and consumed
    /// by another, which is a measurement of neither. `Arc::get_mut` is "before
    /// the first client" expressed as a type.
    pub fn set_policy(&mut self, policy: Policy) -> io::Result<()> {
        let Some(inner) = Arc::get_mut(&mut self.inner) else {
            return Err(io::Error::other(
                "the allocation policy must be chosen before any client is served: \
                 this arena is already shared with a session",
            ));
        };
        let arenas = inner.arenas.get_mut().unwrap_or_else(|e| e.into_inner());
        arenas.policy = policy;
        for slot in &mut arenas.slots {
            slot.alloc.policy = policy;
        }
        Ok(())
    }

    /// The policy in force, for the log line that says which allocator ran.
    pub fn policy(&self) -> Policy {
        self.alloc().policy
    }

    /// Free bytes, and the largest contiguous run of them **in any one region**.
    ///
    /// The second number is the one that matters: an arena with room for a pool
    /// and nowhere to put it is the fragmentation failure shared_vulkan's
    /// docs/design.md §5.7 is about, and reporting only the total would hide it.
    /// Growth does not change that — it adds a region whose largest run is the
    /// whole of it — and summing the largest runs across regions would be a
    /// second way to hide the same thing, since no pool may straddle two.
    pub fn free_bytes(&self) -> (u64, u64) {
        self.alloc().free_bytes()
    }

    /// Advance the frame counter the release policy is written in terms of, and
    /// return any region that has become returnable.
    ///
    /// Called once per commit by the session. See [`Arena::reclaim_regions`]
    /// for what "returnable" means and why it is not simply "empty".
    pub fn tick(&self) -> u64 {
        let mut a = self.alloc();
        a.tick += 1;
        a.tick
    }

    /// Give back every region that is empty and has stayed empty.
    ///
    /// # The policy, and why it cannot wait for the client to say so
    ///
    /// waycompose §17.8 measured Firefox over a 25-second run: **`wl_shm_pool.
    /// destroy` zero times**, one `wl_buffer` destroyed in three, against 18
    /// `wl_region`s and 2 `wl_surface`s destroyed in the same trace. It is not a
    /// client that destroys nothing; it is a client that does not destroy
    /// *these*. So a policy that returns a region when its last pool is
    /// destroyed returns almost nothing, and the machinery underneath is not
    /// what is missing — §17.8 also measured a pool at offset 28672 being reused
    /// and coalesced, so the free list works.
    ///
    /// What makes a region returnable here is therefore a fact about this
    /// process rather than a message from the client: **nothing is allocated in
    /// it, and nothing has been for `idle_frames` frames.** The second clause is
    /// not a timer, it is a guard against thrash — a region emptied and refilled
    /// on alternate frames would be unmapped and re-located every other frame,
    /// and a re-locate is the most expensive thing this design does.
    ///
    /// # What is safe to return, and what is not
    ///
    /// Only a region with no live extents, which is stronger than "no live
    /// pools". A client is entitled to re-attach a buffer it never destroyed,
    /// and an extent it might name is a live extent — `destroy` is the only
    /// thing that ends one, and until it runs the region stays. So this can
    /// only ever return memory the *proxy* has already stopped using, which is
    /// why it is safe without knowing anything about the client's intentions.
    ///
    /// The startup region is never returned. It carries the compositor's
    /// outbound window — live keymaps the compositor has already handed to
    /// clients (`one-binary.md` §3.1a) — and unmapping it would take those with
    /// it.
    pub fn reclaim_regions(&self, idle_frames: u64) -> Vec<(u32, u64, String)> {
        let mut released = Vec::new();
        {
            let mut a = self.alloc();
            let (tick, base) = (a.tick, self.inner.id);
            // Mark newly empty regions, so that "empty since" is a fact the
            // policy can read rather than one it has to guess.
            for slot in &mut a.slots {
                if slot.is_empty() {
                    slot.idle_since.get_or_insert(tick);
                } else {
                    slot.idle_since = None;
                }
            }
            let mut i = 0;
            while i < a.slots.len() {
                let slot = &a.slots[i];
                // What is pinned is the region holding the compositor's
                // outbound window, and **only** that region.
                //
                // It was "the startup region" until the window got a region of
                // its own, and the difference is the whole of the user's
                // objection: pinning the startup region made its size a
                // permanent floor, so an `sv-guest` started at 768 MiB held
                // 768 MiB with no client connected. Pinning the outbound region
                // instead makes the floor the size of the thing that actually
                // has to stay — the keymaps and clipboard offers already handed
                // to clients, live in their hands (`one-binary.md` §3.1a).
                //
                // A data region is now reclaimable whether or not it is the one
                // the session started with. `base` is still the id the
                // compositor was told and still names the arena; it no longer
                // confers immortality.
                let _ = base;
                let cold = !slot.is_outbound()
                    && slot.is_empty()
                    && slot.idle_since.is_some_and(|t| tick.saturating_sub(t) >= idle_frames);
                if !cold {
                    i += 1;
                    continue;
                }
                let slot = a.slots.remove(i);
                a.released_regions += 1;
                a.released_bytes += slot.map.len;
                released.push((slot.map.id, slot.map.len, slot.map.origin.clone()));
                // Dropping the last `Arc` here is what actually unmaps the
                // region — `StitchedRegion`'s `Drop` munmaps the whole
                // reservation in one call regardless of how many runs are in
                // it (§5.7). The guest's committed bytes go down when it is
                // told, below, and not before.
                drop(slot);
            }
        }
        if !released.is_empty()
            && let Ok(mut src) = self.inner.source.lock()
            && let Some(src) = src.as_mut()
        {
            for (id, _, _) in &released {
                src.release(*id);
            }
        }
        released
    }
}

impl Slot {
    fn new(map: Arc<RegionMap>, policy: Policy) -> Slot {
        // A region's free list is the whole of it minus the outbound window,
        // which is always a tail. Three cases, and the first is the one this
        // stage added:
        //
        // * the window is the **whole** region — a dedicated outbound region.
        //   Nothing is allocatable, so the free list is empty rather than
        //   `[(0, 0)]`, which would be a zero-length run for `pick` to trip on;
        // * the window is a tail of a data region — the pre-split spelling,
        //   still read, still correct, and still carrying a floor;
        // * no window — an ordinary data region, free to its end.
        let usable = match map.outbound {
            Some((base, _)) => base,
            None => map.len,
        };
        let free = if usable == 0 { Vec::new() } else { vec![(0, usable)] };
        Slot {
            map,
            alloc: Alloc { free, policy, ..Alloc::default() },
            idle_since: Some(0),
        }
    }

    /// Does this region exist to hold objects the compositor hands to clients?
    ///
    /// True for a dedicated outbound region and for a data region carrying a
    /// tail window. Either way the region is pinned for the session — see
    /// [`Arena::reclaim_regions`].
    fn is_outbound(&self) -> bool {
        self.map.outbound.is_some()
    }

    /// Nothing is out of this region and nothing ever will be until something
    /// is allocated from it again.
    fn is_empty(&self) -> bool {
        self.alloc.live.is_empty() && self.alloc.published == 0
    }
}

impl Arenas {
    /// Total free bytes, and the largest run in any one region.
    ///
    /// The second number stays per-region rather than being summed, and that is
    /// the honest one: an arena with 200 MiB free across four regions still
    /// cannot serve a 100 MiB pool if no single region has 100 MiB contiguous.
    /// Summing it would turn "the allocator cannot place this" into a number
    /// that says it can.
    fn free_bytes(&self) -> (u64, u64) {
        let total = self
            .slots
            .iter()
            .flat_map(|s| s.alloc.free.iter())
            .map(|(_, l)| *l)
            .sum();
        let largest = self
            .slots
            .iter()
            .flat_map(|s| s.alloc.free.iter())
            .map(|(_, l)| *l)
            .max()
            .unwrap_or(0);
        (total, largest)
    }

    fn published(&self) -> u64 {
        self.slots.iter().map(|s| s.alloc.published).sum()
    }

    /// The least this arena can shrink to: the region(s) holding the
    /// compositor.s outbound window, which are the only ones never reclaimed.
    ///
    /// Was the startup region.s size, which made whatever a session started
    /// with a permanent floor. Now it is the size of the thing that actually
    /// has to stay.
    fn floor(&self) -> u64 {
        self.slots.iter().filter(|s| s.is_outbound()).map(|s| s.map.len).sum()
    }

    fn size(&self) -> u64 {
        self.slots.iter().map(|s| s.map.len).sum()
    }

    /// Place `want` bytes in whichever region can take them, returning the
    /// region and the offset within it.
    ///
    /// **Regions are tried oldest first, and that is deliberate.** Packing into
    /// the regions that already exist is what makes the newest one become empty
    /// and therefore returnable; spreading allocations evenly would keep every
    /// region a little bit occupied and nothing would ever come back. Release
    /// is a consequence of allocation order as much as of any policy.
    fn place(&mut self, want: u64) -> Option<(Arc<RegionMap>, u64, u64)> {
        for slot in &mut self.slots {
            let Some((idx, from_tail)) = slot.alloc.pick(want) else {
                continue;
            };
            let (offset, len) = slot.alloc.free[idx];
            let at = if from_tail { offset + len - want } else { offset };
            if len == want {
                slot.alloc.free.remove(idx);
            } else if from_tail {
                slot.alloc.free[idx] = (offset, len - want);
            } else {
                slot.alloc.free[idx] = (offset + want, len - want);
            }
            let key = self.next_key;
            self.next_key += 1;
            slot.alloc.live.insert(key, (at, want));
            slot.alloc.published += want;
            slot.idle_since = None;
            return Some((Arc::clone(&slot.map), at, key));
        }
        None
    }
}

impl Alloc {
    /// Which run of the free list a `want`-byte request takes, by the policy in
    /// force. `None` means no run is big enough, which is the caller's error to
    /// describe because only it knows the numbers worth printing.
    ///
    /// Ties go to the lowest offset in both policies, so an allocator's answer
    /// is a function of its free list and not of a `HashMap`'s iteration order —
    /// which matters because `arena_slices_never_overlap` and the fragmentation
    /// reproduction both compare two runs of the same sequence.
    /// Returns the index and whether the extent is cut from that run's **tail**
    /// rather than its head.
    fn pick(&self, want: u64) -> Option<(usize, bool)> {
        if self.policy == Policy::FirstFit {
            return self.free.iter().position(|(_, l)| *l >= want).map(|i| (i, false));
        }
        // A small pool goes to the highest run, cut from that run's tail, so
        // that the permanent ones end up packed against the far end of the
        // region instead of scattered through the space the large pools need.
        // See [`SMALL_POOL`].
        if want <= SMALL_POOL {
            return self
                .free
                .iter()
                .enumerate()
                .rev()
                .find(|(_, (_, l))| *l >= want)
                .map(|(i, _)| (i, true));
        }
        // `min_by_key` returns the *first* minimum, and the list is sorted by
        // offset, so the tie-break is the lowest address for free.
        self.free
            .iter()
            .enumerate()
            .filter(|(_, (_, l))| *l >= want)
            .min_by_key(|(_, (_, l))| *l)
            .map(|(i, _)| (i, false))
    }

    fn release(&mut self, offset: u64, len: u64) {
        let at = self.free.partition_point(|(o, _)| *o < offset);
        self.free.insert(at, (offset, len));
        // Coalesce with the neighbour on each side. Done here rather than at
        // allocation time so that the free list is always in its canonical
        // form, which is what makes `free_bytes`'s second number meaningful.
        let mut i = 0;
        while i + 1 < self.free.len() {
            let (o, l) = self.free[i];
            let (o2, l2) = self.free[i + 1];
            if o + l == o2 {
                self.free[i] = (o, l + l2);
                self.free.remove(i + 1);
            } else {
                i += 1;
            }
        }
    }
}

impl PoolTransport for Arena {
    fn name(&self) -> &'static str {
        "arena"
    }

    /// Place a pool, growing the arena if no region can take it.
    ///
    /// # The order — place, then grow, then place again — is the whole policy
    ///
    /// Growth is what happens when the allocator has already failed, never
    /// before. That ordering is not an optimisation, it is what keeps the
    /// fragmentation work honest: an arena that grew whenever a region was
    /// merely *tight* would make [`Policy::BestFitSegregated`] untestable,
    /// because every measurement of how well runs are chosen would be
    /// contaminated by memory that was added instead of used. §20.4's matrix
    /// compares policies at a fixed size, and it still can.
    ///
    /// # Refusing is still a thing this function does
    ///
    /// Even with growth the guest can be out of RAM, and then this returns
    /// `OutOfMemory` with the numbers exactly as it always did. **That is not a
    /// regression to be designed away.** The connection Firefox lost was not
    /// closed by this refusal; it was closed because an unannounced pool is an
    /// object the compositor does not have, and the client's next
    /// `wl_shm_pool.create_buffer` was forwarded to it anyway — a request on an
    /// unknown object is a protocol error. The caller absorbs the refusal and
    /// replays from the client's own bytes, and an allocator that never refused
    /// would have hidden that bug rather than fixed it.
    fn create(&mut self, size: u64) -> io::Result<Region> {
        if size == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a pool cannot be empty"));
        }
        let want = size.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY;
        if let Some(region) = self.place(want, size) {
            return Ok(region);
        }
        // Nothing had room. Ask for another region, and say how much is needed
        // rather than a fixed step, so that a pool larger than the growth step
        // is served by one region rather than by a sequence of regions none of
        // which can hold it. A pool may not straddle two (see [`RegionMap`]).
        match self.grow(want) {
            Ok(Some(note)) => {
                if let Some(region) = self.place(want, size) {
                    let _ = note;
                    return Ok(region);
                }
            }
            Ok(None) => {}
            Err(e) => {
                self.alloc().grow_refused += 1;
                let a = self.alloc();
                let (total, largest) = a.free_bytes();
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    format!(
                        "arena {} cannot serve {want} bytes and cannot grow: {e} \
                         ({total} free across {} region(s), largest run {largest}, \
                         {} bytes out to {} connection(s), {} policy)",
                        self.inner.id,
                        a.slots.len(),
                        a.published(),
                        Arc::strong_count(&self.inner),
                        a.policy.as_str(),
                    ),
                ));
            }
        }
        let a = self.alloc();
        let (total, largest) = a.free_bytes();
        Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            format!(
                "arena {} has no run of {want} bytes ({total} free across {} region(s), \
                 largest run {largest}, {} bytes out to {} connection(s), {} policy); \
                 raise --arena-size",
                self.inner.id,
                a.slots.len(),
                a.published(),
                Arc::strong_count(&self.inner),
                a.policy.as_str(),
            ),
        ))
    }

    fn destroy(&mut self, region: &Region) {
        let mut a = self.alloc();
        let tick = a.tick;
        for slot in &mut a.slots {
            if let Some((offset, len)) = slot.alloc.live.remove(&region.key) {
                slot.alloc.published -= len;
                slot.alloc.release(offset, len);
                if slot.alloc.live.is_empty() && slot.alloc.published == 0 {
                    slot.idle_since = Some(tick);
                }
                return;
            }
        }
    }

    fn bytes_mut(&mut self, region: &Region) -> Option<&mut [u8]> {
        // The lock is held only for the lookup, not for the copy. A commit is
        // 126 us of `memcpy` for Firefox (docs/waycompose-design.md §15.10) and four
        // connections holding one mutex across it would serialise every client's
        // frame path against every other's — which would be a real cost of
        // sharing the arena, and there is no reason to pay it: the extent this
        // returns cannot be handed to anybody else while it is live.
        let (map, offset) = {
            let a = self.alloc();
            let slot = a.slots.iter().find(|s| s.alloc.live.contains_key(&region.key))?;
            (Arc::clone(&slot.map), slot.alloc.live[&region.key].0)
        };
        let start = usize::try_from(offset).ok()?;
        let len = usize::try_from(region.size).ok()?;
        let p = map.backing.range(start, len)?;
        // SAFETY: `region.key` is live in the allocator, so `start..start + len`
        // is an extent no other `Region` overlaps — the allocator hands out
        // disjoint runs and a key belongs to exactly one session — and `&mut
        // self` rules out a second reference to it from this handle. The
        // compositor may be reading these bytes at the same time, which is the
        // `wl_shm` race `wl_buffer.release` arbitrates and not a Rust one.
        //
        // The mapping outlives the returned slice: a region is only ever
        // unmapped by `reclaim_regions`, which refuses any region with a live
        // extent in it, and this key is one. So the `Arc` dropped at the end of
        // this function is never the last.
        Some(unsafe { std::slice::from_raw_parts_mut(p, len) })
    }

    fn published(&self) -> u64 {
        self.alloc().published()
    }

    fn memory(&self) -> Option<MemoryReport> {
        let a = self.alloc();
        Some(MemoryReport {
            arena: self.inner.id,
            peak_bytes: a.peak_bytes,
            peak_regions: a.peak_regions,
            now_bytes: a.size(),
            now_regions: a.slots.len(),
            floor_bytes: a.floor(),
            grown: a.grown,
            released_regions: a.released_regions,
            released_bytes: a.released_bytes,
        })
    }

    fn frame(&mut self) -> Vec<(u32, u64, String)> {
        self.tick();
        // `reclaim_idle == 0` means "never", not "immediately". Returning a
        // region the instant it empties is the thrash the idle count exists to
        // prevent, and it is not a behaviour worth having a spelling for.
        let idle = self.alloc().reclaim_idle;
        if idle == 0 {
            return Vec::new();
        }
        self.reclaim_regions(idle)
    }

    /// **Only the compositor's outbound window**, and the narrowness is the
    /// point.
    ///
    /// The bytes this returns are read while other threads are copying pixels
    /// into the same region, so the range has to be one that no pool can ever
    /// occupy. That range exists and has a name: it is the window
    /// [`Arena::reserve_outbound`] took out of the free list, which is exactly
    /// the set of bytes the compositor may describe to a client. Anything
    /// outside it is either a misconfiguration or a compositor describing a
    /// client's pool back to another client, and both should be refused with the
    /// numbers in the message rather than served.
    ///
    /// STAGE 12 narrowed this from "the whole region". Before it there was one
    /// session and the widest possible read was harmless; with four sessions the
    /// widest possible read overlaps another session's pixels while it is being
    /// written, and the check is what makes [`SharedBacking`]'s two accessors
    /// provably disjoint instead of merely conventionally so.
    fn peer_slice(&self, arena: u32, offset: u64, len: u64) -> Option<&[u8]> {
        // Growth does not widen this. The readable range is the outbound
        // window and nothing else, so the region named here must be one that
        // has a window — naming any other is a compositor describing something
        // it did not mint, which is refused rather than served.
        //
        // The id is no longer required to be the startup region's: once the
        // window has a region of its own, that region has its own id, and
        // hard-coding `self.inner.id` here would refuse the only region this
        // function exists to read.
        let map = {
            let a = self.alloc();
            Arc::clone(&a.slots.iter().find(|s| s.map.id == arena && s.is_outbound())?.map)
        };
        let (base, wlen) = map.outbound?;
        if offset < base || offset.checked_add(len)? > base + wlen {
            return None;
        }
        let start = usize::try_from(offset).ok()?;
        let len = usize::try_from(len).ok()?;
        let p = map.backing.range(start, len)?;
        // SAFETY: the range is inside the mapping and inside the outbound
        // window, which the allocator never hands out — so no `bytes_mut` on any
        // thread can be holding a `&mut` that overlaps it. The compositor writes
        // it, once, before it names it in a message this proxy has yet to read.
        // The region is the startup one, which `reclaim_regions` never returns.
        Some(unsafe { std::slice::from_raw_parts(p.cast_const(), len) })
    }
}

impl Arena {
    /// Try every region, in the order they joined. `None` means none had room,
    /// which is the caller's cue to grow.
    fn place(&self, want: u64, size: u64) -> Option<Region> {
        let mut a = self.alloc();
        let (map, offset, key) = a.place(want)?;
        Some(Region {
            descriptor: Descriptor::ArenaSlice { arena: map.id, offset, len: size },
            size,
            key,
        })
    }

    /// Obtain another region big enough for `want`, and take it into the arena.
    ///
    /// `Ok(None)` means this arena has no source and therefore cannot grow,
    /// which is the pre-growth behaviour and is a normal answer rather than a
    /// failure — a test that wants to observe a refusal builds one this way.
    ///
    /// # What growth costs, and why the number is recorded rather than assumed
    ///
    /// Over guest RAM a new region is a `VirtualAlloc` and a *locate*. The
    /// locate is the whole cost: docs/waycompose-design.md §12.5 measured the content sweep at
    /// **2.37–2.42 s** for a 13312 MiB backing object, and it is a function of
    /// the guest's RAM rather than of the region's size. A 2.4 s stall in the
    /// middle of a video is not a feature, which is why §0.14's
    /// `SuperfetchPfnQuery` — **0.064 s** on the host, ~330 ms of guest CPU,
    /// verified against the sweep 16384/16384 — is a prerequisite of this stage
    /// and not an optimisation of it.
    ///
    /// It is also what makes growth *possible* rather than merely fast. Stage
    /// 16 rejected growth on the grounds that §12.10's stamps are overwritten
    /// by the first pool, so a region cannot be re-located; that objection was
    /// correct against the sweep and is answered by the query, which re-located
    /// a window whose every byte had been overwritten and returned the same
    /// 16384 frames where the sweep found **0 pages**.
    ///
    /// Either way the elapsed time is measured here and reported by
    /// [`Arena::growth`], because a stage that claimed a cost it did not record
    /// would be making the mistake §8.4 is about.
    fn grow(&self, want: u64) -> io::Result<Option<String>> {
        let (id, min_len) = {
            let a = self.alloc();
            // Geometric from `grow_step`, capped. See [`GROW_STEP`]: the cost
            // of growing is the locate, so the number of growths is what wants
            // bounding, and the cap is what bounds the overshoot in exchange.
            let step = a
                .grow_step
                .saturating_mul(1u64 << a.grown.min(2))
                .min(GROW_STEP_CAP.max(a.grow_step));
            (a.next_id, want.max(step))
        };
        let started = std::time::Instant::now();
        let provided = {
            let mut src = self.inner.source.lock().unwrap_or_else(|e| e.into_inner());
            let Some(src) = src.as_mut() else {
                return Ok(None);
            };
            src.provide(id, min_len)?
        };
        let elapsed = started.elapsed().as_nanos() as u64;
        if provided.len < want {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "a region of {} bytes was asked for and {} came back; a pool may not \
                     straddle two regions, so a short one cannot serve it",
                    want, provided.len
                ),
            ));
        }
        let note = format!("region {id}: {} bytes from {}", provided.len, provided.origin);
        let mut a = self.alloc();
        Arena::adopt(
            &mut a,
            RegionMap {
                id,
                backing: SharedBacking::new(provided.backing),
                origin: provided.origin,
                len: provided.len,
                outbound: None,
            },
        );
        a.grown += 1;
        a.grow_nanos += elapsed;
        Ok(Some(note))
    }
}

/// 128 bits from `/dev/urandom`, hex encoded.
fn random_hex() -> String {
    use std::io::Read as _;
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        // Greppable, exactly as `wc_handle`'s degraded name is: refusing to run
        // would be worse, and a caller must be able to tell.
        return format!("weak-{:x}", std::process::id());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wc-proxy-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The claim both transports must satisfy: bytes written here are readable
    /// by a peer that was told **only the descriptor**. Read back through
    /// `wc_handle::Handle::map`, which is the compositor's own path.
    fn round_trip(t: &mut dyn PoolTransport) {
        let region = t.create(8192).unwrap();
        {
            let bytes = t.bytes_mut(&region).unwrap();
            assert_eq!(bytes.len(), 8192);
            bytes[0..4].copy_from_slice(b"HEAD");
            bytes[8188..8192].copy_from_slice(b"TAIL");
        }
        // Only these bytes cross.
        let wire = region.descriptor.encode();
        let peer = Handle::from_descriptor(Descriptor::decode(&wire).unwrap());
        let map = peer.map().unwrap();
        assert_eq!(map.len(), 8192, "{}", t.name());
        assert_eq!(&map.as_slice()[0..4], b"HEAD", "{}", t.name());
        assert_eq!(&map.as_slice()[8188..8192], b"TAIL", "{}", t.name());
    }

    #[test]
    fn a_shared_file_region_is_readable_from_its_descriptor_alone() {
        let d = tmpdir("sf");
        // The peer prefix is how the *peer* spells the directory, and the peer
        // in this test is this process, so it is the local path. In a real run
        // it is `Z:\tmp\wc-pools\` or a virtio-fs drive letter, and the whole
        // point of the parameter is that nothing can derive one from the other.
        let mut t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
        round_trip(&mut t);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_arena_slice_is_readable_from_its_descriptor_alone() {
        let d = tmpdir("ar");
        std::fs::create_dir_all(&d).unwrap();
        let mut t = Arena::open_or_create(7100, &d.join("arena"), 1 << 20).unwrap();
        wc_handle::arena::attach(7100, &d.join("arena").to_string_lossy(), 1 << 20).unwrap();
        round_trip(&mut t);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Two pools must not overlap. The failure this rules out is the worst one
    /// available — two surfaces silently sharing bytes — and it would present
    /// as one window showing the other's contents.
    #[test]
    fn arena_slices_never_overlap() {
        let d = tmpdir("ar2");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7101, &d.join("arena"), 1 << 20).unwrap();
        let mut spans = Vec::new();
        for size in [4096u64, 100, 8192, 1, 65536, 4097] {
            let r = a.create(size).unwrap();
            let Descriptor::ArenaSlice { offset, len, .. } = r.descriptor else {
                panic!("wrong kind")
            };
            assert_eq!(offset % ARENA_GRANULARITY, 0, "offset {offset} is not page aligned");
            assert_eq!(len, size, "the descriptor must carry what the client asked for");
            spans.push((offset, size.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY));
        }
        spans.sort();
        for w in spans.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0, "{:?} overlaps {:?}", w[0], w[1]);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn freed_arena_space_is_reused_and_coalesced() {
        let d = tmpdir("ar3");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7102, &d.join("arena"), 64 * 4096).unwrap();
        let regions: Vec<_> = (0..8).map(|_| a.create(4096).unwrap()).collect();
        assert_eq!(a.published(), 8 * 4096);
        for r in &regions {
            a.destroy(r);
        }
        assert_eq!(a.published(), 0);
        // Coalesced back into one run, which is the property that lets a long
        // run of create/destroy cycles not fragment the arena to death.
        let (total, largest) = a.free_bytes();
        assert_eq!(total, 64 * 4096);
        assert_eq!(largest, 64 * 4096, "the free list did not coalesce");
        // And the whole thing is allocatable again.
        assert!(a.create(64 * 4096).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_over_large_pool_is_a_named_error_rather_than_a_panic() {
        let d = tmpdir("ar4");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7103, &d.join("arena"), 16 * 4096).unwrap();
        let err = a.create(17 * 4096).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        assert!(err.to_string().contains("--arena-size"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_pool_is_refused_by_both() {
        let d = tmpdir("z");
        std::fs::create_dir_all(&d).unwrap();
        assert!(SharedFile::new(&d, "Z:\\").unwrap().create(0).is_err());
        assert!(Arena::open_or_create(7104, &d.join("a"), 4096).unwrap().create(0).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn shared_file_names_do_not_repeat() {
        let d = tmpdir("sf2");
        let mut t = SharedFile::new(&d, "Z:\\tmp\\").unwrap();
        let a = t.create(4096).unwrap();
        let b = t.create(4096).unwrap();
        assert_ne!(a.descriptor, b.descriptor);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// STAGE 12, and the test the stage exists for.
    ///
    /// Four handles on one arena — Firefox's count — each allocating and each
    /// **writing its own name into its own slice**, from four threads at once.
    /// The assertions are the two failures that sharing an allocator could
    /// produce and that nothing else in this file would catch: two connections
    /// handed overlapping offsets, and a connection's bytes turning up in
    /// another's slice. The second is the one that matters, because overlapping
    /// offsets with nobody writing them look exactly like working code.
    #[test]
    fn four_connections_share_one_arena_without_overlapping() {
        let d = tmpdir("share");
        std::fs::create_dir_all(&d).unwrap();
        let a = Arena::open_or_create(7105, &d.join("arena"), 4 << 20).unwrap();
        assert_eq!(a.sharers(), 1);

        let handles: Vec<_> = (0..4u8)
            .map(|n| {
                let mut t = a.share();
                std::thread::spawn(move || {
                    let mut mine = Vec::new();
                    for i in 0..8u64 {
                        let r = t.create(4096 * (i + 1)).unwrap();
                        let Descriptor::ArenaSlice { offset, len, .. } = r.descriptor else {
                            panic!("wrong kind")
                        };
                        let bytes = t.bytes_mut(&r).unwrap();
                        bytes.fill(b'A' + n);
                        mine.push((offset, len));
                    }
                    // Read them back through this session's own handle after
                    // every other session has finished writing, which is what
                    // makes this a test of the region rather than of the loop.
                    mine
                })
            })
            .collect();
        let mut spans: Vec<(u64, u64)> = Vec::new();
        for h in handles {
            spans.extend(h.join().unwrap());
        }
        assert_eq!(a.sharers(), 1, "every session's handle must have been dropped");
        assert_eq!(spans.len(), 32);
        spans.sort();
        for w in spans.windows(2) {
            assert!(
                w[0].0 + w[0].1.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY <= w[1].0,
                "{:?} overlaps {:?} — two connections were given the same bytes",
                w[0],
                w[1]
            );
        }
        // Each slice still holds one letter, so nothing wrote through anybody
        // else's extent.
        let map = Handle::open_shared_file(
            &d.join("arena").to_string_lossy(),
            &d.join("arena").to_string_lossy(),
            4 << 20,
        )
        .unwrap()
        .map()
        .unwrap();
        for (offset, len) in spans {
            let s = &map.as_slice()[offset as usize..(offset + len) as usize];
            assert!(
                s.iter().all(|b| *b == s[0]) && (b'A'..=b'D').contains(&s[0]),
                "slice {offset}+{len} is not one connection's bytes: {:?}",
                &s[..8.min(s.len())]
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The window a compositor may describe, and nothing else.
    ///
    /// STAGE 12 narrowed `peer_slice` from the whole region to the outbound
    /// window. With one connection the difference was invisible; with four it is
    /// the difference between reading a keymap and reading another client's
    /// buffer while it is being copied into.
    #[test]
    fn a_peer_slice_outside_the_outbound_window_is_refused() {
        let d = tmpdir("peerwin");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7106, &d.join("arena"), 64 * 4096).unwrap();
        a.reserve_outbound(8 * 4096).unwrap();
        let (base, len) = a.outbound().unwrap();
        assert!(a.peer_slice(7106, base, len).is_some(), "the window itself must be readable");
        assert!(a.peer_slice(7106, base, len + 1).is_none(), "past the end of the arena");
        assert!(a.peer_slice(7106, base - 4096, 4096).is_none(), "below the window");
        // And the range a pool would occupy, which is the one that matters.
        let r = a.create(4096).unwrap();
        let Descriptor::ArenaSlice { offset, len, .. } = r.descriptor else { panic!() };
        assert!(
            a.peer_slice(7106, offset, len).is_none(),
            "a pool's own bytes must not be readable as a compositor's object"
        );
        assert!(a.peer_slice(7107, base, 4096).is_none(), "another arena's id");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The window is a property of the deployment and is fixed before any client
    /// exists. Once one does, the refcount says so.
    #[test]
    fn an_outbound_window_cannot_be_reserved_once_a_connection_holds_the_arena() {
        let d = tmpdir("late");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7107, &d.join("arena"), 64 * 4096).unwrap();
        let held = a.share();
        let e = a.reserve_outbound(4096).unwrap_err();
        assert!(e.to_string().contains("already shared with a session"), "{e}");
        drop(held);
        a.reserve_outbound(4096).expect("and it is allowed again once nothing holds it");
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---------------------------------------------------------------- 16 ---
    // Fragmentation, reproduced.

    /// A deterministic stand-in for what a video does to a pool allocator.
    ///
    /// **Deliberately synthetic, and that is the point.** A YouTube video
    /// fragments the arena in about ten seconds, but it does so through Firefox,
    /// a compositor, a hypervisor and a guest, and a run of it can only ever say
    /// "it broke again". This says which allocator broke, at what occupancy, on
    /// exactly the same request sequence, in 40 milliseconds, in `cargo test`.
    ///
    /// The sequence has the two ingredients the live failure had and nothing
    /// else:
    ///
    /// 1. **A churn of large pools whose sizes wander.** Firefox's video pools
    ///    were 2340864 and 2342912 bytes and the arena had 3932160 free in a
    ///    largest run of 1744896 — sizes near each other but never equal, which
    ///    is what leaves a splinter per cycle at page granularity.
    /// 2. **A few small pools that never go away**, created between the large
    ///    ones. Cursors, tooltips and icons are exactly this, and a small
    ///    permanent extent in the middle of a region is what stops the large
    ///    holes on either side of it from ever coalescing.
    ///
    /// Neither is a guess about Firefox: 1 is the numbers in the log line, and 2
    /// is why 197 of 215 pools being released still left the arena unable to
    /// answer.
    fn video_churn(
        policy: Policy,
        arena_size: u64,
        rounds: usize,
        live_max: usize,
    ) -> (u32, u64, u64) {
        let d = tmpdir(&format!("churn-{}", policy.as_str()));
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7110, &d.join("arena"), arena_size).unwrap();
        a.set_policy(policy).unwrap();

        // An LCG rather than `rand`: the sequence has to be identical for both
        // policies and identical between runs, or the comparison is not one.
        let mut seed = 0x2545_F491u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed >> 8
        };

        // **A pool's life is the client's business, not the allocator's**, and
        // modelling that is the difference between a reproduction and an
        // artefact. The first version of this retired a pool only when a *new*
        // one had been allocated, so one refusal froze the live set and every
        // later round refused too — which made a bad allocator look catastrophic
        // and a good one look bad. Firefox does not stop destroying pools
        // because this proxy could not find room for one. So the schedule is
        // fixed: whatever happened at round `r`, the slot from round
        // `r - live_max` is retired at round `r`.
        let mut history: std::collections::VecDeque<Option<Region>> =
            std::collections::VecDeque::new();
        let mut pinned: Vec<Region> = Vec::new();
        let mut failures = 0u32;
        for round in 0..rounds {
            // A small pool that is never released, every so often. Sizes are
            // the ones a cursor theme and a tooltip actually produce.
            if round % 64 == 0
                && pinned.len() < 12
                && let Ok(r) = a.create(4096 + (next() % 6) as u64 * 4096)
            {
                pinned.push(r);
            }
            // The video's own buffer: 1.6 to 2.6 MB, wandering, page aligned
            // exactly as a `stride * height` would be.
            let size = 1_600_000 + (next() % 1_000_000) as u64;
            let size = size / 4 * 4;
            match a.create(size) {
                Ok(r) => history.push_back(Some(r)),
                Err(_) => {
                    failures += 1;
                    history.push_back(None);
                }
            }
            if history.len() > live_max
                && let Some(Some(r)) = history.pop_front()
            {
                a.destroy(&r);
            }
        }
        let (total, largest) = a.free_bytes();
        let _ = std::fs::remove_dir_all(&d);
        (failures, total, largest)
    }

    /// **The stage's reproduction.** The same 4000-round churn, at an occupancy
    /// where the arena is a little over half full, through both allocators.
    ///
    /// What it asserts is not "the new one is better on average" — it is that
    /// the old one reaches a state where it refuses a pool it has the bytes for,
    /// which is the live failure, and that the new one does not reach that state
    /// at all.
    #[test]
    #[ignore = "sweep, for choosing the policy and the reproduction's constants"]
    fn sweep_policies() {
        let all = [Policy::FirstFit, Policy::BestFitSegregated];
        eprint!("{:>5}", "live");
        for p in all {
            eprint!(" {:>18}", p.as_str());
        }
        eprintln!();
        for live_max in [16usize, 20, 22, 24, 25, 26, 27, 28, 29, 30, 31] {
            eprint!("{live_max:5}");
            for p in all {
                let (f, _, l) = video_churn(p, 64 << 20, 4000, live_max);
                eprint!(" {f:6} ({:>9})", l);
            }
            eprintln!();
        }
    }

    #[test]
    fn a_video_sized_churn_fragments_first_fit_and_not_the_default() {
        let arena = 64 << 20;
        // Three occupancies rather than one, because a single number would not
        // distinguish "the new policy is better" from "the new policy happens
        // to be better here". 25 and 26 are where first fit begins to refuse
        // pools it has the bytes for; 31 is past the point where the arena is
        // genuinely too small for the working set, and is included so that the
        // claim is bounded rather than triumphant.
        let mut rows = Vec::new();
        for live_max in [25usize, 26, 31] {
            let (old_fail, _, old_largest) = video_churn(Policy::FirstFit, arena, 4000, live_max);
            let (new_fail, new_free, new_largest) =
                video_churn(Policy::BestFitSegregated, arena, 4000, live_max);
            eprintln!(
                "live {live_max}: first-fit {old_fail} refusals (largest {old_largest}) | \
                 best-fit {new_fail} refusals (largest {new_largest}, {new_free} free)"
            );
            rows.push((live_max, old_fail, new_fail));
        }

        // 1. The reproduction still reproduces. If first fit ever stops
        //    refusing, this test has stopped measuring the failure it was
        //    written for and must be made harsher rather than deleted.
        for (live_max, old_fail, _) in &rows {
            assert!(
                *old_fail > 0,
                "at {live_max} live pools first fit answered every request, so the reproduction \
                 no longer reproduces"
            );
        }
        // 2. At the occupancies where the arena genuinely has room, the new
        //    policy refuses nothing at all. This is the live failure: 3932160
        //    bytes free and a 2342912-byte pool refused.
        assert_eq!(rows[0].2, 0, "the default refused a pool it had room for at 25 live");
        assert!(rows[1].2 <= 3, "the default refused {} pools at 26 live", rows[1].2);
        // 3. And it is never worse, including past the point where the working
        //    set does not fit — which is the honest half, because a policy that
        //    won in the middle and lost at the edge would have to be reported as
        //    such rather than as a fix.
        for (live_max, old_fail, new_fail) in &rows {
            assert!(
                new_fail <= old_fail,
                "at {live_max} live pools the default refused {new_fail} against first fit's \
                 {old_fail}: the new policy is worse here and the matrix in §20.4 is wrong"
            );
        }
    }

    /// The small-pool end, which is the half of the policy that is not best fit.
    ///
    /// A cursor-sized pool must come from the *top* of the region and a
    /// content-sized one from the bottom, because the whole point is that the
    /// permanent small extents never land in the middle of the space the large
    /// ones need.
    #[test]
    fn small_pools_are_cut_from_the_far_end_and_large_ones_from_the_near() {
        let d = tmpdir("ends");
        std::fs::create_dir_all(&d).unwrap();
        let size = 128 * 4096;
        let mut a = Arena::open_or_create(7111, &d.join("arena"), size).unwrap();
        let at = |r: &Region| match r.descriptor {
            Descriptor::ArenaSlice { offset, .. } => offset,
            _ => panic!("wrong kind"),
        };

        let big = a.create(SMALL_POOL * 2).unwrap();
        let small = a.create(4096).unwrap();
        assert_eq!(at(&big), 0, "a large pool starts at the bottom of the region");
        assert_eq!(
            at(&small),
            size - 4096,
            "a small pool is the last page of the region, not the one after the large pool"
        );
        // A second small one packs against the first rather than starting a
        // second hole.
        let small2 = a.create(4096).unwrap();
        assert_eq!(at(&small2), size - 8192);
        // Exactly at the threshold is still small; one byte over is not.
        let edge = a.create(SMALL_POOL).unwrap();
        assert_eq!(at(&edge), size - 8192 - SMALL_POOL);
        let over = a.create(SMALL_POOL + 1).unwrap();
        assert_eq!(at(&over), SMALL_POOL * 2, "one byte over the threshold is a large pool");

        // And first fit does none of it, which is what makes this a policy
        // rather than a property of the free list.
        let mut b = Arena::open_or_create(7112, &d.join("arena2"), size).unwrap();
        b.set_policy(Policy::FirstFit).unwrap();
        let b_big = b.create(SMALL_POOL * 2).unwrap();
        let b_small = b.create(4096).unwrap();
        assert_eq!(at(&b_big), 0);
        assert_eq!(at(&b_small), SMALL_POOL * 2, "first fit puts the cursor next to the content");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_scope_parses_exactly_the_three() {
        for s in ["pool", "buffer", "damage"] {
            assert_eq!(CopyScope::parse(s).unwrap().as_str(), s);
        }
        assert!(CopyScope::parse("none").is_none());
        for s in ["first-fit", "best-fit"] {
            assert_eq!(Policy::parse(s).unwrap().as_str(), s);
        }
        assert!(Policy::parse("worst-fit").is_none());
    }

    // -----------------------------------------------------------------
    // DYNAMIC ARENA: growth, and returning what growth took
    // -----------------------------------------------------------------

    /// The crash, reproduced and then survived.
    ///
    /// The user's YouTube failure in one assertion: an arena with no room for
    /// the next pool. Before this stage `create` returned `OutOfMemory` and the
    /// pool went unannounced; now the arena obtains another region and the pool
    /// is served. The numbers are the shape of the real one — a video-sized
    /// pool against an arena that has already been filled — rather than a round
    /// number chosen to make the test pass.
    #[test]
    fn an_arena_with_no_room_grows_instead_of_refusing() {
        let d = tmpdir("grow");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7200, &d.join("arena"), 64 * 4096).unwrap();
        a.set_source(Box::new(FileGrowth::new(&d, 64 * 4096))).unwrap();
        a.set_grow_step(64 * 4096).unwrap();

        // Fill the startup region completely.
        let _hold = a.create(64 * 4096).unwrap();
        assert_eq!(a.free_bytes(), (0, 0), "the startup region is full");
        assert_eq!(a.regions(), 1);

        // The request that used to be fatal.
        let grown = a.create(32 * 4096).expect("an arena that can grow must not refuse this");
        assert_eq!(a.regions(), 2, "a second region was taken up");
        let (times, nanos, refused, _, _) = a.growth();
        assert_eq!((times, refused), (1, 0));
        assert!(nanos > 0, "growth must record what it cost, not assume it");

        // And the pool really is in the new region, named by its own id.
        match grown.descriptor {
            Descriptor::ArenaSlice { arena, .. } => {
                assert_ne!(arena, 7200, "a grown pool lives in the region that was added");
                assert_eq!(arena, 7201, "region ids are handed out in order and never reused");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A pool larger than the growth step gets a region big enough for it,
    /// because a pool may not straddle two regions.
    ///
    /// The alternative — growing by a fixed step and then failing anyway
    /// because no single region can hold the request — is the failure mode that
    /// would make growth look like it worked while the client still froze.
    #[test]
    fn a_pool_larger_than_the_growth_step_gets_a_region_that_fits_it() {
        let d = tmpdir("grow-big");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7201, &d.join("arena"), 8 * 4096).unwrap();
        a.set_source(Box::new(FileGrowth::new(&d, 4096))).unwrap();
        a.set_grow_step(4096).unwrap();
        let _fill = a.create(8 * 4096).unwrap();

        let big = a.create(100 * 4096).expect("one region must be able to hold it");
        assert_eq!(big.size, 100 * 4096);
        // One region, not twenty-five of the step size.
        assert_eq!(a.regions(), 2);
        assert!(a.size() >= 108 * 4096);
    }

    /// An arena with no source behaves exactly as it did before this stage, and
    /// the message still carries both numbers.
    ///
    /// This is the case the fragmentation matrix measures. If growth had been
    /// made unconditional, `a_video_sized_churn_fragments_first_fit_and_not_the_default`
    /// would pass by adding memory rather than by placing it well, and the
    /// comparison between the two policies would mean nothing.
    #[test]
    fn an_arena_with_no_source_still_refuses_with_the_numbers() {
        let d = tmpdir("nogrow");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7202, &d.join("arena"), 8 * 4096).unwrap();
        let _fill = a.create(8 * 4096).unwrap();
        let e = a.create(4096).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::OutOfMemory);
        let m = e.to_string();
        assert!(m.contains("no run of 4096 bytes"), "{m}");
        assert!(m.contains("0 free across 1 region(s)"), "{m}");
        assert_eq!(a.regions(), 1, "no source means no growth, silently or otherwise");
    }

    /// A source that cannot serve the request produces `OutOfMemory` carrying
    /// both the size asked for and the source's own reason — and the arena
    /// stays usable.
    ///
    /// `one-binary.md` §3.1b: arena exhaustion "is a thing a compositor
    /// survives in a degraded state, not a thing that kills a client".
    #[test]
    fn a_guest_that_is_out_of_ram_refuses_survivably_and_says_both_numbers() {
        struct Broke;
        impl RegionSource for Broke {
            fn provide(&mut self, _id: u32, min_len: u64) -> io::Result<ProvidedRegion> {
                Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    format!("the guest has no {min_len} bytes to commit"),
                ))
            }
            fn describe(&self) -> String {
                "a guest with no memory left".into()
            }
        }
        let d = tmpdir("oom");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7203, &d.join("arena"), 8 * 4096).unwrap();
        a.set_grow_step(4096).unwrap();
        a.set_source(Box::new(Broke)).unwrap();
        let fill = a.create(8 * 4096).unwrap();

        let e = a.create(4096).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::OutOfMemory);
        let m = e.to_string();
        assert!(m.contains("cannot serve 4096 bytes"), "{m}");
        assert!(m.contains("the guest has no 4096 bytes to commit"), "{m}");
        assert_eq!(a.growth().2, 1, "the refusal is counted");

        // Degraded, alive: the arena still serves what it can once space frees.
        a.destroy(&fill);
        assert!(a.create(4096).is_ok(), "a refusal must not poison the allocator");
    }

    /// A region that empties is returned, and the bytes stop being charged.
    ///
    /// The measurement that matters is the last one: `FileGrowth::release`
    /// unlinks the file, so "the memory came back" is a fact about the
    /// filesystem rather than a log line claiming it. On guest RAM the same
    /// drop unmaps the stitched reservation and the guest is told to decommit.
    #[test]
    fn an_empty_region_is_returned_and_its_bytes_actually_go_away() {
        let d = tmpdir("release");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7204, &d.join("arena"), 8 * 4096).unwrap();
        a.set_source(Box::new(FileGrowth::new(&d, 16 * 4096))).unwrap();
        a.set_grow_step(16 * 4096).unwrap();
        let _fill = a.create(8 * 4096).unwrap();
        let grown = a.create(4096).unwrap();
        assert_eq!(a.regions(), 2);
        let region_file = d.join("region-7205");
        assert!(region_file.exists(), "the grown region is a real object");

        // Still live: nothing is returned while an extent is out, whatever the
        // client did or did not say.
        for _ in 0..10 {
            a.tick();
        }
        assert!(a.reclaim_regions(2).is_empty(), "a live extent pins its region");
        assert_eq!(a.regions(), 2);

        // Now empty, but not yet cold. The delay is the anti-thrash guard.
        a.destroy(&grown);
        a.tick();
        assert!(a.reclaim_regions(4).is_empty(), "emptiness alone is not enough");

        // Cold.
        for _ in 0..5 {
            a.tick();
        }
        let released = a.reclaim_regions(4);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].0, 7205);
        assert_eq!(a.regions(), 1);
        assert_eq!(a.growth().3, 1, "one region released");
        assert_eq!(a.growth().4, 16 * 4096, "and the bytes are counted");
        assert!(!region_file.exists(), "the bytes are gone, not merely unlisted");
    }

    /// A dedicated outbound region makes the floor the size of the *window*,
    /// not the size the session started at — and every data region, including
    /// the first, becomes reclaimable.
    ///
    /// This is the user's objection answered. An `sv-guest` was found holding
    /// 817 MB with no client connected, because the region carrying the
    /// compositor's keymaps was also the region the data allocator started in,
    /// and pinning the one pinned the other. Separating them costs
    /// [`OUTBOUND_REGION_BYTES`] and removes the floor entirely from the data
    /// side.
    #[test]
    fn a_separate_outbound_region_drops_the_floor_to_the_window_size() {
        use crate::guestram::Plan;
        let d = tmpdir("split-outbound");
        std::fs::create_dir_all(&d).unwrap();
        let backing = d.join("backing");
        // A 256 KiB outbound region and an 8-page data region behind it.
        let out_len = OUTBOUND_REGION_BYTES;
        let data_len = 8 * 4096u64;
        std::fs::write(&backing, vec![0u8; (out_len + data_len) as usize]).unwrap();
        let plan_path = d.join("plan");
        std::fs::write(
            &plan_path,
            format!(
                "region 0\nobject {0}\nlen {out_len}\nrun 0x1\noutbound 0 {out_len}\n\
                 r 0 0 {out_len}\n\
                 region 1\nobject {0}\nlen {data_len}\nrun 0x2\nr 0 {out_len} {data_len}\n",
                backing.display()
            ),
        )
        .unwrap();
        // The plan reads: a region that is entirely window is the good case now.
        let plans = Plan::read_all(&plan_path).unwrap();
        assert_eq!(plans[0].outbound, Some((0, out_len)));
        assert_eq!(plans[1].outbound, None);

        let mut a = Arena::from_plan(0, &plan_path).unwrap();
        a.set_source(Box::new(FileGrowth::new(&d, 0))).unwrap();
        a.set_grow_step(data_len).unwrap();
        assert_eq!(a.regions(), 2);
        assert_eq!(a.floor(), out_len, "the floor is the window, not the startup size");

        // The outbound region hands out nothing: it is not in the free list at
        // all, so a pool can never land on a live keymap.
        assert_eq!(a.free_bytes(), (data_len, data_len));
        let pool = a.create(data_len).unwrap();
        let Descriptor::ArenaSlice { arena, .. } = pool.descriptor else {
            panic!("expected an arena slice");
        };
        assert_eq!(arena, 1, "pools go in the data region, never the outbound one");

        // The compositor can still read its own window, by that region's id.
        assert!(a.peer_slice(0, 0, 4096).is_some());
        assert!(a.peer_slice(1, 0, 4096).is_none(), "a data region has no window to read");

        // And now the part that used to be impossible: the data region the
        // session started with is reclaimed, and the arena falls to the window.
        a.destroy(&pool);
        for _ in 0..10 {
            a.tick();
        }
        let released = a.reclaim_regions(2);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].0, 1);
        assert_eq!(a.regions(), 1, "only the outbound region is left");
        assert_eq!(a.size(), out_len);
        assert_eq!(a.size(), a.floor(), "an idle arena settles at the window size");
    }

    /// The startup region is never returned, however empty and however cold.
    ///
    /// It carries the compositor's outbound window — keymaps the compositor has
    /// already handed to clients — and unmapping it would take those with it.
    /// `one-binary.md` §3.1a is the argument; this is the assertion.
    #[test]
    fn the_startup_region_is_never_reclaimed() {
        let d = tmpdir("keep-base");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7206, &d.join("arena"), 8 * 4096).unwrap();
        a.reserve_outbound(4096).unwrap();
        a.set_source(Box::new(FileGrowth::new(&d, 4096))).unwrap();
        a.set_grow_step(4096).unwrap();
        for _ in 0..100 {
            a.tick();
        }
        assert!(a.reclaim_regions(1).is_empty());
        assert_eq!(a.regions(), 1);
        assert_eq!(a.outbound(), Some((7 * 4096, 4096)));
    }

    /// Growth does not widen what the compositor may describe back to a client.
    ///
    /// Only the startup region has an outbound window, so a `peer_slice` naming
    /// a grown region is a compositor describing something it did not mint —
    /// refused rather than served, exactly as an out-of-window offset is.
    #[test]
    fn a_grown_region_has_no_outbound_window_and_cannot_be_peer_read() {
        let d = tmpdir("peer");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7207, &d.join("arena"), 8 * 4096).unwrap();
        a.reserve_outbound(4096).unwrap();
        a.set_source(Box::new(FileGrowth::new(&d, 8 * 4096))).unwrap();
        a.set_grow_step(8 * 4096).unwrap();
        let _fill = a.create(7 * 4096).unwrap();
        let grown = a.create(4096).unwrap();
        let Descriptor::ArenaSlice { arena: grown_id, .. } = grown.descriptor else {
            panic!("expected an arena slice");
        };

        // The startup region's window still reads.
        assert!(a.peer_slice(7207, 7 * 4096, 4096).is_some());
        // The grown region has none, and asking for one is refused.
        assert!(a.peer_slice(grown_id, 0, 4096).is_none());
    }

    /// Extents in two different regions may share neither an offset nor a
    /// meaning: the region id is part of the name.
    ///
    /// Two regions are mapped at unrelated host addresses, so offset 0 of
    /// region A and offset 0 of region B are different bytes. This is the
    /// property that makes `Descriptor::ArenaSlice`'s `arena` field
    /// load-bearing rather than decorative.
    #[test]
    fn the_same_offset_in_two_regions_is_two_different_places() {
        let d = tmpdir("distinct");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7208, &d.join("arena"), 4096).unwrap();
        a.set_source(Box::new(FileGrowth::new(&d, 4096))).unwrap();
        a.set_grow_step(4096).unwrap();
        let first = a.create(4096).unwrap();
        let second = a.create(4096).unwrap();
        assert_eq!(a.regions(), 2);

        let (Descriptor::ArenaSlice { arena: a1, offset: o1, .. }, Descriptor::ArenaSlice { arena: a2, offset: o2, .. }) =
            (&first.descriptor, &second.descriptor)
        else {
            panic!("expected arena slices");
        };
        assert_eq!((*o1, *o2), (0, 0), "both at offset 0 of their own region");
        assert_ne!(a1, a2, "and told apart by the region id alone");

        // Writing one must not disturb the other, which is what "different
        // bytes" actually means.
        a.bytes_mut(&first).unwrap()[0..4].copy_from_slice(b"ONE.");
        a.bytes_mut(&second).unwrap()[0..4].copy_from_slice(b"TWO.");
        assert_eq!(&a.bytes_mut(&first).unwrap()[0..4], b"ONE.");
        assert_eq!(&a.bytes_mut(&second).unwrap()[0..4], b"TWO.");
    }

    /// Peak resident bytes is a high-water mark, not a running total, and it
    /// does not fall when memory is returned.
    ///
    /// The distinction this pins is the one that caused a 1158 MiB throughput
    /// figure to be read as 1158 MiB of memory. Peak answers "how big did it
    /// ever get"; it is bounded by what was committed at one moment, and
    /// repeated allocation of the same bytes does not move it.
    #[test]
    fn peak_is_the_high_water_mark_and_not_the_traffic() {
        let d = tmpdir("peak");
        std::fs::create_dir_all(&d).unwrap();
        let mut a = Arena::open_or_create(7400, &d.join("arena"), 8 * 4096).unwrap();
        a.set_source(Box::new(FileGrowth::new(&d, 8 * 4096))).unwrap();
        a.set_grow_step(8 * 4096).unwrap();
        assert_eq!(a.peak(), (8 * 4096, 1));
        // No outbound region, so nothing is pinned and the floor is zero. The
        // startup size is no longer a floor — that was the whole complaint.
        assert_eq!(a.floor(), 0);

        // Churn the same bytes many times over. Traffic is enormous; the arena
        // never grows, so the peak does not move.
        for _ in 0..50 {
            let r = a.create(8 * 4096).unwrap();
            a.destroy(&r);
        }
        assert_eq!(a.peak(), (8 * 4096, 1), "reusing bytes is not using more of them");
        assert_eq!(a.regions(), 1);

        // Now genuinely hold two regions at once.
        let hold = a.create(8 * 4096).unwrap();
        let grown = a.create(4096).unwrap();
        assert_eq!(a.regions(), 2);
        assert_eq!(a.peak(), (16 * 4096, 2), "two regions committed at one moment");

        // Release brings the arena down but must not bring the peak down: a
        // high-water mark that fell would not be one.
        a.destroy(&grown);
        for _ in 0..10 {
            a.tick();
        }
        assert_eq!(a.reclaim_regions(2).len(), 1);
        assert_eq!(a.size(), 8 * 4096, "the arena really did shrink");
        assert_eq!(a.peak(), (16 * 4096, 2), "and the peak still remembers");
        a.destroy(&hold);

        // With no outbound region there is **no floor at all**, and that is the
        // change the user asked for: the startup region is reclaimable like any
        // other, so an arena nobody is using falls to nothing.
        //
        // It used to be pinned, which made whatever a session started with a
        // permanent floor — the reason an `sv-guest` held 817 MB with no client
        // connected.
        for _ in 0..10 {
            a.tick();
        }
        a.reclaim_regions(1);
        assert_eq!(a.floor(), 0, "nothing here holds objects handed to clients");
        assert_eq!(a.size(), 0, "so an idle arena releases every byte");
        assert_eq!(a.regions(), 0);
    }

    /// Growth is geometric from a small base and then flat, and the default
    /// base is not `--arena-size`.
    ///
    /// The number of growths is what wants bounding, because each one costs a
    /// locate (~0.4 s on the deployment) rather than the 6.7 us of bookkeeping
    /// measured locally. The cap is what stops that argument from justifying
    /// unbounded overshoot.
    #[test]
    fn growth_is_geometric_from_a_small_base_and_then_flat() {
        assert_eq!(GROW_STEP, 16 << 20, "argued from the pool sizes clients ask for");
        assert_eq!(GROW_STEP_CAP, 64 << 20);

        let d = tmpdir("geometric");
        std::fs::create_dir_all(&d).unwrap();
        // A 4 KiB startup region, so that every allocation forces a growth and
        // the sizes the arena asks for are the only thing being measured.
        let mut a = Arena::open_or_create(7500, &d.join("arena"), 4096).unwrap();
        a.set_source(Box::new(FileGrowth::new(&d, 0))).unwrap();
        let _fill = a.create(4096).unwrap();

        // Each round: drain every free run, so the next 4096-byte request has
        // nowhere to go and must grow. Without the drain the second request
        // simply lands in the 16 MiB region the first one added.
        let mut held = Vec::new();
        let mut sizes = Vec::new();
        for _ in 0..5 {
            loop {
                let (total, largest) = a.free_bytes();
                if total == 0 {
                    break;
                }
                held.push(a.create(largest).unwrap());
            }
            let before = a.size();
            held.push(a.create(4096).unwrap());
            sizes.push(a.size() - before);
        }
        assert_eq!(
            sizes,
            vec![16 << 20, 32 << 20, 64 << 20, 64 << 20, 64 << 20],
            "16, 32, 64 and then flat at the cap"
        );

        // Reaching 768 MiB takes fourteen growths rather than the forty-eight a
        // flat 16 MiB step would need. That is the whole argument for doubling.
        let need = 768u64 << 20;
        let mut total = 0u64;
        let mut n = 0u32;
        while total < need {
            total += (GROW_STEP << n.min(2)).min(GROW_STEP_CAP);
            n += 1;
        }
        assert_eq!(n, 14, "fourteen locates to reach 768 MiB, not forty-eight");
    }

    /// What growth costs, measured rather than asserted.
    ///
    /// Ignored by default because it is an instrument and not a gate — it
    /// prints numbers and asserts almost nothing, and docs/waycompose-design.md §13.8's
    /// retraction is the reason that distinction is kept sharp. Run it with
    ///
    /// ```text
    /// cargo test -p wc-proxy -- --ignored --nocapture growth_costs
    /// ```
    ///
    /// It measures the *host* half only: `mmap` and bookkeeping, with no guest
    /// and no locate. That is the honest scope of a local measurement, and the
    /// locate is the term that dominates the real one — §12.5 measured the
    /// content sweep at 2.37–2.42 s and §0.14 measured the PFN query at 0.064 s
    /// on the host plus ~330 ms of guest CPU. This number is what is left when
    /// that term is removed, and it is what growth would cost if locating were
    /// free.
    #[test]
    #[ignore = "an instrument: prints what growth costs, gates nothing"]
    fn growth_costs_what_it_costs() {
        let d = tmpdir("cost");
        std::fs::create_dir_all(&d).unwrap();
        const REGION: u64 = 64 << 20;
        const POOL: u64 = 2_340_864; // the pool that killed Firefox

        let mut a = Arena::open_or_create(7300, &d.join("arena"), REGION).unwrap();
        a.set_source(Box::new(FileGrowth::new(&d, REGION))).unwrap();

        // Fill the startup region with video-sized pools, as the real client
        // does, and time the ones that do not grow.
        let mut held = Vec::new();
        let mut in_region = Vec::new();
        loop {
            let t = std::time::Instant::now();
            let before = a.regions();
            let Ok(r) = a.create(POOL) else { break };
            let dt = t.elapsed().as_nanos() as u64;
            if a.regions() == before {
                in_region.push(dt);
            }
            held.push(r);
            if a.regions() > 3 {
                break;
            }
        }
        let (grown, nanos, refused, _, _) = a.growth();
        let plain: u64 = in_region.iter().sum::<u64>() / in_region.len().max(1) as u64;
        println!("\n--- growth cost, host half only (no guest, no locate) ---");
        println!("  region size            {REGION} bytes");
        println!("  pool size              {POOL} bytes (the one that killed Firefox)");
        println!("  pools placed           {}", held.len());
        println!("  allocations, no growth {} ns mean over {}", plain, in_region.len());
        println!("  growths                {grown}  ({refused} refused)");
        println!("  growth total           {nanos} ns");
        println!("  growth mean            {} ns", nanos / u64::from(grown.max(1)));
        println!("  arena now              {} bytes in {} region(s)", a.size(), a.regions());
        println!("\n  the term this cannot measure is the locate:");
        println!("    content sweep   2.37-2.42 s   (docs/waycompose-design.md 12.5, and it cannot re-locate)");
        println!("    PFN query       0.064 s host + ~330 ms guest   (docs/design.md 0.14)");
        assert!(grown >= 1, "the point of the measurement is that growth happened");
    }

    /// A plan naming several regions maps all of them, and the first is the one
    /// the compositor was told about.
    #[test]
    fn a_multi_region_plan_is_taken_up_whole() {
        use crate::guestram::{Plan, Run};
        let d = tmpdir("plan-multi");
        std::fs::create_dir_all(&d).unwrap();
        let backing = d.join("backing");
        std::fs::write(&backing, vec![0u8; 3 * 4096]).unwrap();
        let plan_path = d.join("plan");
        std::fs::write(
            &plan_path,
            format!(
                "region 4\nobject {0}\nlen 4096\nrun 0x1\nr 0 0 4096\n\
                 region 9\nobject {0}\nlen 8192\nrun 0x2\nr 0 4096 8192\n",
                backing.display()
            ),
        )
        .unwrap();
        assert_eq!(Plan::read_all(&plan_path).unwrap().len(), 2);

        let a = Arena::from_plan(4, &plan_path).unwrap();
        assert_eq!(a.id(), 4, "the first region is the arena the compositor knows");
        assert_eq!(a.regions(), 2);
        assert_eq!(a.size(), 3 * 4096);
        assert_eq!(a.region_ids(), vec![4, 9]);
        let _ = Run { arena_offset: 0, file_offset: 0, len: 0 };

        // And a plan whose first region is not `--arena-id` is refused rather
        // than renumbered.
        let Err(e) = Arena::from_plan(9, &plan_path) else {
            panic!("a plan whose first region is not --arena-id must be refused");
        };
        assert!(e.to_string().contains("the first region is 4"), "{e}");
    }
}
