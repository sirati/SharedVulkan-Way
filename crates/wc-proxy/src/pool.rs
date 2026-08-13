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
/// they have to understand to do it. Measured in `docs/design.md` §9.
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
/// measurement that decides between the two, and it is in `docs/design.md` §9.
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
    /// Holds the object open. docs/design.md §2.5's lifetime rule: the creator
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
/// (docs/design.md §2.2 (c)) and constructed here for the first time. In the
/// real deployment the region is shared_vulkan's passthrough arena — guest RAM
/// the host found by walking QEMU's flat view (its design.md §5.1) — and this
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
/// (shared_vulkan design.md §5.2), which is the measured
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
/// (docs/design.md §15.8), which is why this exists.
///
/// The invariant this preserves is the one that was always the point: **two
/// allocators over one region would hand out the same offsets**, which is a
/// client's pixels written on top of another client's — or on top of a live
/// keymap — with nothing in any log. Sharing the *allocator* is the opposite of
/// that: four connections asking one free list for space is the same operation
/// as one connection asking it four times.
/// Where an [`Arena`]'s bytes actually are.
///
/// STAGE 7 SPLIT THIS IN TWO, and docs/design.md §12 is why. Everything above
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
/// (docs/design.md §20.3): `arena 0 has no run of 2342912 bytes (3932160 free,
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

/// A pool at most this big is allocated from the far end of the region.
///
/// 64 KiB is chosen from what clients actually create rather than from a round
/// number: a 32x32 cursor is 4096 bytes, a 128x128 icon is 65536, and every
/// `wl_shm` pool Firefox opens for content is at least fifteen times this. So
/// the line falls in a genuine gap in the distribution, which is what makes
/// "small" a fact about the client rather than a knob.
pub const SMALL_POOL: u64 = 64 * 1024;

/// The whole of the allocator's mutable state, behind one lock.
///
/// It is one struct rather than four fields so that "the free list, the live
/// map and the counters move together" is a property of the type instead of a
/// thing every method has to remember.
#[derive(Default)]
struct Alloc {
    free: Vec<(u64, u64)>,
    live: HashMap<u64, (u64, u64)>,
    next: u64,
    published: u64,
    policy: Policy,
}

/// The region, the one allocator over it, and everything that does not change
/// once a client exists.
struct ArenaInner {
    id: u32,
    region: SharedBacking,
    /// How the region was obtained, for the one log line that says it.
    origin: String,
    size: u64,
    /// The peer's outbound window, `(offset, len)`, which this allocator has
    /// been told to stay out of. See [`Arena::reserve_outbound`].
    outbound: Option<(u64, u64)>,
    alloc: Mutex<Alloc>,
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
    /// STAGE 12, and it is the whole of docs/design.md §16.2. Until this stage
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
    fn alloc(&self) -> MutexGuard<'_, Alloc> {
        self.inner.alloc.lock().unwrap_or_else(|e| e.into_inner())
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
    pub fn from_plan(id: u32, plan_path: &Path) -> io::Result<Arena> {
        let plan = crate::guestram::Plan::read(plan_path)?;
        let size = plan.len;
        if size % ARENA_GRANULARITY != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a {size}-byte arena is not a whole number of {ARENA_GRANULARITY}-byte pages"),
            ));
        }
        let region = crate::guestram::StitchedRegion::attach(&plan)?;
        let origin = format!(
            "guest RAM, run 0x{:016x}, {} run(s) of {}",
            plan.run_id,
            region.runs(),
            region.origin()
        );
        let mut arena = Arena::over(id, Backing::GuestRam(region), &origin, size);
        if let Some((_, len)) = plan.outbound {
            arena.reserve_outbound(len)?;
        }
        Ok(arena)
    }

    /// Take the last `len` bytes of this arena out of the free list, because the
    /// peer declared them **its** to hand to clients.
    ///
    /// STAGE 10 and docs/design.md §12.11 item 3. The allocator above owns the
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
        let size = inner.size;
        {
            let alloc = inner.alloc.get_mut().unwrap_or_else(|e| e.into_inner());
            if alloc.published > 0 || alloc.free != vec![(0, size)] {
                return Err(io::Error::other(
                    "the peer's outbound window must be reserved before anything is allocated",
                ));
            }
        }
        if len == 0 || len >= size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("an outbound window of {len} bytes leaves nothing of a {size}-byte arena"),
            ));
        }
        let len = len.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY;
        inner.outbound = Some((size - len, len));
        inner.alloc.get_mut().unwrap_or_else(|e| e.into_inner()).free = vec![(0, size - len)];
        Ok(())
    }

    /// The peer's window, `(offset, len)`, for the log line that says this proxy
    /// knows about it. `None` is the answer for every deployment before stage 10
    /// and for every one whose compositor creates nothing.
    pub fn outbound(&self) -> Option<(u64, u64)> {
        self.inner.outbound
    }

    fn over(id: u32, backing: Backing, origin: &str, size: u64) -> Arena {
        Arena {
            inner: Arc::new(ArenaInner {
                id,
                region: SharedBacking::new(backing),
                origin: origin.to_owned(),
                size,
                outbound: None,
                alloc: Mutex::new(Alloc {
                    free: vec![(0, size)],
                    ..Alloc::default()
                }),
            }),
        }
    }

    pub fn id(&self) -> u32 {
        self.inner.id
    }

    /// Where the region came from, for the log line that is the only difference
    /// a reader of the output can see between the two substrates.
    pub fn origin(&self) -> &str {
        &self.inner.origin
    }

    pub fn size(&self) -> u64 {
        self.inner.size
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
        inner.alloc.get_mut().unwrap_or_else(|e| e.into_inner()).policy = policy;
        Ok(())
    }

    /// The policy in force, for the log line that says which allocator ran.
    pub fn policy(&self) -> Policy {
        self.alloc().policy
    }

    /// Free bytes, and the largest contiguous run of them. The second number is
    /// the one that matters: an arena with room for a pool and nowhere to put
    /// it is the fragmentation failure shared_vulkan's design.md §5.7 is about,
    /// and reporting only the total would hide it.
    pub fn free_bytes(&self) -> (u64, u64) {
        let alloc = self.alloc();
        (
            alloc.free.iter().map(|(_, l)| *l).sum(),
            alloc.free.iter().map(|(_, l)| *l).max().unwrap_or(0),
        )
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

    fn create(&mut self, size: u64) -> io::Result<Region> {
        if size == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a pool cannot be empty"));
        }
        let mut alloc = self.alloc();
        let want = size.div_ceil(ARENA_GRANULARITY) * ARENA_GRANULARITY;
        let (idx, from_tail) = alloc.pick(want).ok_or_else(|| {
            let total: u64 = alloc.free.iter().map(|(_, l)| *l).sum();
            let largest = alloc.free.iter().map(|(_, l)| *l).max().unwrap_or(0);
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "arena {} has no run of {want} bytes ({total} free, largest {largest}, \
                     {} bytes out to {} connection(s), {} policy); raise --arena-size",
                    self.inner.id,
                    alloc.published,
                    Arc::strong_count(&self.inner),
                    alloc.policy.as_str(),
                ),
            )
        })?;
        let (run, len) = alloc.free[idx];
        let offset = if from_tail { run + len - want } else { run };
        if len == want {
            alloc.free.remove(idx);
        } else if from_tail {
            alloc.free[idx] = (run, len - want);
        } else {
            alloc.free[idx] = (run + want, len - want);
        }
        let key = alloc.next;
        alloc.next += 1;
        alloc.live.insert(key, (offset, want));
        alloc.published += want;
        Ok(Region {
            descriptor: Descriptor::ArenaSlice { arena: self.inner.id, offset, len: size },
            size,
            key,
        })
    }

    fn destroy(&mut self, region: &Region) {
        let mut alloc = self.alloc();
        if let Some((offset, len)) = alloc.live.remove(&region.key) {
            alloc.published -= len;
            alloc.release(offset, len);
        }
    }

    fn bytes_mut(&mut self, region: &Region) -> Option<&mut [u8]> {
        // The lock is held only for the lookup, not for the copy. A commit is
        // 126 us of `memcpy` for Firefox (docs/design.md §15.10) and four
        // connections holding one mutex across it would serialise every client's
        // frame path against every other's — which would be a real cost of
        // sharing the arena, and there is no reason to pay it: the extent this
        // returns cannot be handed to anybody else while it is live.
        let (offset, _) = *self.alloc().live.get(&region.key)?;
        let start = usize::try_from(offset).ok()?;
        let len = usize::try_from(region.size).ok()?;
        let p = self.inner.region.range(start, len)?;
        // SAFETY: `region.key` is live in the allocator, so `start..start + len`
        // is an extent no other `Region` overlaps — the allocator hands out
        // disjoint runs and a key belongs to exactly one session — and `&mut
        // self` rules out a second reference to it from this handle. The
        // compositor may be reading these bytes at the same time, which is the
        // `wl_shm` race `wl_buffer.release` arbitrates and not a Rust one.
        Some(unsafe { std::slice::from_raw_parts_mut(p, len) })
    }

    fn published(&self) -> u64 {
        self.alloc().published
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
        if arena != self.inner.id {
            return None;
        }
        let (base, wlen) = self.inner.outbound?;
        if offset < base || offset.checked_add(len)? > base + wlen {
            return None;
        }
        let start = usize::try_from(offset).ok()?;
        let len = usize::try_from(len).ok()?;
        let p = self.inner.region.range(start, len)?;
        // SAFETY: the range is inside the mapping and inside the outbound
        // window, which the allocator never hands out — so no `bytes_mut` on any
        // thread can be holding a `&mut` that overlaps it. The compositor writes
        // it, once, before it names it in a message this proxy has yet to read.
        Some(unsafe { std::slice::from_raw_parts(p.cast_const(), len) })
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
}
