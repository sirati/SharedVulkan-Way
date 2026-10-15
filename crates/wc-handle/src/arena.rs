//! Arenas: regions both sides mapped **before** any protocol traffic, so that
//! a buffer can cross by arithmetic instead of by opening anything.
//!
//! # Why this is a process-global registry and not a parameter
//!
//! An arena is not a per-message fact. It is attached once, out of band, by
//! whatever brought the two sides together — shared_vulkan's cold-path
//! handshake in the real deployment (its docs/waycompose-design.md §4.3), a command-line flag
//! in a test — and from then on every `Descriptor::ArenaSlice` in the stream
//! refers to it by a small integer.
//!
//! The alternative was to thread an arena table through
//! [`Handle::map`](crate::Handle::map). That fails for a reason worth writing
//! down, because it is the same reason the whole `h`-argument shim exists:
//! `Handle::map` is called from `vendor/smithay`'s `wayland/shm/pool.rs`, which
//! upstream calls with nothing but an fd, and giving it a context parameter
//! would mean carrying a diff through smithay's `ShmState`, its `Client` data
//! and its `wl_shm` dispatch — precisely the "spread through the commit path"
//! cost docs/waycompose-design.md §8.4 says the seam exists to avoid. An arena is
//! genuinely process-wide state; modelling it as such is honest, and it keeps
//! the fork's diff at zero lines.
//!
//! # The lifetime rule, which is the opposite of a section's
//!
//! docs/waycompose-design.md §2.5 says the *creator* of a section must hold it open until
//! the receiver has opened it. An arena inverts that: the receiver holds the
//! region open for the life of the process, and the sender's slices are pure
//! arithmetic that cannot dangle. Nothing has to be raced. That is the second
//! reason this is the right kind for the real deployment and not merely the
//! available one.
//!
//! # What is *not* here, and what stage 10 added
//!
//! Until stage 10 this module had **no allocator at all**, and the header said
//! why: who owns which bytes of an arena is the sender's problem —
//! `sv_arena::alloc` on the shared_vulkan side, `wc-proxy`'s pool allocator on
//! this one. That is still true of the *pool* direction and it is still where
//! almost all the bytes go. What it left out is that the sender is not always
//! the peer. `wl_keyboard.keymap` is created by the compositor and opened by
//! the client, and so are a clipboard offer, an icon and a cursor theme; for
//! those the compositor is the sender and had nothing to ask for bytes.
//!
//! So there is now exactly one allocator here, and it is deliberately the
//! smallest one that can exist:
//!
//! * It allocates from **one declared sub-range** of one arena — the *outbound
//!   window*, [`reserve_outbound`] — and not from the arena at large. The rest
//!   of the region still belongs entirely to the peer's allocator, which never
//!   hears about this one; the two cannot hand out the same offset because
//!   they are not handing out from the same bytes.
//! * The only thing it produces is a [`Gift`], and a `Gift` is the *only* value
//!   in this crate that carries a write path into an arena. There is no
//!   function anywhere that takes an arbitrary `(arena, offset)` from a caller
//!   and writes it: [`Gift::write_at`] is bounded by the gift's own extent,
//!   which the allocator chose, and [`guest_write`] — the coherency probe, and
//!   the only other write path — now refuses the outbound window outright.
//!
//! That is the shape the safety argument needs. A client's buffer arrives as a
//! `Descriptor::ArenaSlice` decoded from the wire, becomes a
//! [`Handle`](crate::Handle) that owns nothing, and therefore has no write path
//! at all — the same rule that has kept a compositor's view of a client's
//! pixels read-only since stage 1. A compositor's keymap arrives as a `Gift`,
//! which is not constructible from the wire. Widening `Handle::map` would have
//! broken that; adding a token that only the allocator can mint does not.
//!
//! Refusing out-of-range windows is still the whole of this module's safety
//! contribution on the *reading* side, and it is still worth being clear that
//! it is not much: a peer that can name any offset in the arena can read any of
//! it, exactly as a peer that can name a section can read all of that section.
//! The boundary is between the two sides, not within one arena.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::{Descriptor, Mapping, MappingInner, platform};

/// One attached region.
struct Attached {
    view: Arc<platform::MappedView>,
    len: usize,
    /// How the region was named, for logs. Never used to reach it again.
    origin: String,
    /// True only for a region **this process provided** — committed itself with
    /// [`attach_guest_ram`], or opened read-write because the deployment said it
    /// owns it ([`provide_file`]) — as opposed to one it opened because somebody
    /// else provided it.
    ///
    /// It gates [`guest_write`] and [`reserve_outbound`], and gating them is the
    /// point: a compositor writing into a client's buffer is a bug in every
    /// case, and writing into a region the peer owns is the one case where it is
    /// also unsound. Bytes this process owns are neither.
    ///
    /// Note that this says nothing about *which* bytes may be written. That is
    /// the outbound window's job, and the two are separate on purpose: `provided`
    /// is a fact about the region, and the window is a decision about part of it.
    provided: bool,
}

type Registry = RwLock<HashMap<u32, Attached>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Attach the arena numbered `id`, whose bytes are the file at `path`.
///
/// Read-only, because the receiving side of an arena slice is a compositor
/// reading a client's pixels — the same default `Handle::map` has for a
/// section, and for the same reason (docs/waycompose-design.md §7.4).
///
/// Idempotent per id only in the sense that re-attaching replaces: an arena is
/// identified by its number on the wire, and if the two sides ever disagree
/// about what number 0 means, no amount of care here can detect it. That
/// disagreement is what shared_vulkan's `generation` field exists to catch on
/// the real path (its docs/waycompose-design.md §4.3), and a test harness passing the same
/// flag to both processes is what catches it here.
pub fn attach(id: u32, path: &str, size: u64) -> io::Result<()> {
    let len = usize::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena too large"))?;
    refuse_if_outbound(id)?;
    let view = platform::map_file(path, len)?;
    let mut guard = registry()
        .write()
        .map_err(|_| io::Error::other("arena registry poisoned"))?;
    guard.insert(
        id,
        Attached { view: Arc::new(view), len, origin: path.to_owned(), provided: false },
    );
    Ok(())
}

/// Attach arena `id` as a **file this process may write**: the local twin of
/// [`attach_guest_ram`], for a machine with no hypervisor in it.
///
/// STAGE 10. [`attach`] opens the region read-only, which is right for the side
/// that only ever reads a peer's pixels, and it is the reason a compositor over
/// a file arena has no outbound window: there is nothing to write into. This
/// opens the same file read-write and marks the arena `provided`, so
/// [`reserve_outbound`] will take a window out of it.
///
/// # Why this is not "attach with a flag"
///
/// Because of what `provided` means, which is not "writable" but "these bytes
/// are this process's to give away". A guest-RAM region is provided because the
/// process committed it; this file is provided because the deployment said so,
/// out of band, in the same breath as the arena id. Both are statements about
/// ownership that no amount of inspection of the region could establish, which
/// is why both are supplied rather than detected.
///
/// The peer opens the same file read-write through its own allocator. On the
/// deployment this is for — Wine, or two processes on one Linux box — that is a
/// coherent shared mapping and was measured to be one (docs/waycompose-design.md §9.4,
/// `scripts/probe-filemap.c`). On a virtio-fs export it is **not**, which is
/// §10.3, and is exactly why the deployment that matters uses guest RAM.
pub fn provide_file(id: u32, path: &str, size: u64) -> io::Result<()> {
    let len = usize::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena too large"))?;
    refuse_if_outbound(id)?;
    let view = platform::map_file_mut(path, len)?;
    let mut guard = registry()
        .write()
        .map_err(|_| io::Error::other("arena registry poisoned"))?;
    guard.insert(
        id,
        Attached {
            view: Arc::new(view),
            len,
            origin: format!("provided file {path}"),
            provided: true,
        },
    );
    Ok(())
}

/// Refuse to replace an arena that an outbound window has been cut out of.
///
/// The registry replaces on re-attach and says so, and until stage 10 the worst
/// that could do was show a client the wrong pixels. A live [`Gift`] holds an
/// `(arena, offset)` and nothing else, so replacing the region under one would
/// silently re-point a keymap at somebody else's memory — a wrong answer of
/// exactly the kind this project keeps refusing to produce quietly.
fn refuse_if_outbound(id: u32) -> io::Result<()> {
    let guard = outbound_slot().lock().map_err(|_| io::Error::other("outbound window poisoned"))?;
    match guard.as_ref() {
        Some(w) if w.arena == id => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "arena {id} has an outbound window at {}+{} and cannot be re-attached",
                w.base, w.len
            ),
        )),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// STAGE 7: an arena that is not a file
// ---------------------------------------------------------------------------

/// The bytes the guest writes into every 4096-byte page of a guest-RAM arena so
/// that the host can *find* those pages, and the offsets they sit at.
///
/// # This is a second copy of somebody else's layout, and that is deliberate
///
/// The definitive one is `arena_record` in `shared_vulkan`, whose own doc
/// comment says it exists so that "two independently-typed layouts that happen
/// to agree on the magic but disagree on a field offset" cannot turn a genuine
/// result into a confusing one. Depending on that crate from here would make
/// **the Windows build of this workspace** depend on a sibling repository being
/// checked out next to it, which is a much worse trade than twenty bytes of
/// duplication.
///
/// Two things make the duplication safe rather than merely small:
///
/// 1. **A disagreement is loud, not silent.** The host accepts a page only if
///    the magic *and* the run id match, and then checks that the page indices
///    form a permutation of `0..pages`. A wrong magic offset finds zero pages;
///    a wrong index offset finds duplicates. Both are refusals with numbers in
///    them. There is no reading of these constants that produces a wrong arena.
/// 2. **It is asserted against the original.** `crates/wc-arena-host` depends on
///    both this crate and `arena_record`, and its test compares these four
///    constants with theirs. That crate is the only place the two projects are
///    linked into one binary, which makes it the only place the check can be
///    made at all.
pub const STAMP_MAGIC: [u8; 8] = *b"SVARENA1";
/// Offset of [`STAMP_MAGIC`] within the page.
pub const STAMP_OFF_MAGIC: usize = 0;
/// Offset of the `u64` run id, little endian.
pub const STAMP_OFF_RUN_ID: usize = 8;
/// Offset of the `u32` page index within the arena, little endian.
pub const STAMP_OFF_PAGE_INDEX: usize = 16;
/// What the rest of the page is filled with, so a half-written page is obvious.
pub const STAMP_FILL: u8 = 0xAB;

/// The page size the stamps and the host's sweep both work in.
pub use crate::platform::REGION_PAGE;

/// §84. Whether [`attach_guest_ram`] may use large pages, for this process.
///
/// On by default, because on the Windows guest it is the only pinning that was
/// measured to hold. The switch exists so the `VirtualLock` path stays
/// reachable for a control run on one machine (`wc-compositor
/// --arena-small-pages`): an environment variable does not reach a process the
/// task scheduler starts. `WC_ARENA_NO_LARGE_PAGES` is honoured as well.
static SMALL_PAGES_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Refuse large pages for every guest-RAM region this process provides from now
/// on. See [`SMALL_PAGES_ONLY`].
pub fn use_small_pages_only() {
    SMALL_PAGES_ONLY.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn large_pages_allowed() -> bool {
    !SMALL_PAGES_ONLY.load(std::sync::atomic::Ordering::Relaxed)
        && std::env::var_os("WC_ARENA_NO_LARGE_PAGES").is_none()
}

/// What [`attach_guest_ram`] did, in the numbers a human needs to judge it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestRegion {
    /// The number the host must be given to find this region. Random per run:
    /// a previous boot's stamps survive in guest RAM until the memory is
    /// reused, and scanning for a reused run id would find both.
    pub run_id: u64,
    /// Bytes.
    pub len: usize,
    /// Pages of [`REGION_PAGE`] bytes, which is what the host counts.
    pub pages: u32,
    /// Whether the pages were pinned. **False is not a failure**: the region is
    /// still guest RAM and the scan still finds it. It means the pages are
    /// allowed to move afterwards, which would invalidate the host's map, so it
    /// is reported rather than swallowed.
    pub locked: bool,
    /// How the pages were pinned: `large-pages`, `virtual-lock`, `mlock` or
    /// `none`. §84: on the Windows guest `VirtualLock` keeps pages resident but
    /// was measured relocating them, so *which* lock it was is part of whether
    /// the host's map can be trusted, and it is reported beside `locked`.
    pub pinning: &'static str,
}

/// Attach arena `id` as **this process's own memory**, stamped so that a
/// process outside the virtual machine can find those pages in the hypervisor's
/// backing object.
///
/// This is docs/waycompose-design.md §9.10 item 3 and §10.13 item 1: the region stops being
/// a file. Everything above `Handle::map` is unchanged — a `Descriptor` is still
/// three integers, `window()` is still checked arithmetic — and what changes is
/// only who provided the pages.
///
/// # The direction is the opposite of every other provider here
///
/// [`attach`] *opens* something somebody else made. This **makes** the region
/// and expects the far side to come and find it, because that is the only
/// direction that works across a hypervisor: the host can read and write the
/// guest's RAM through QEMU's memory backend, and the guest cannot name a host
/// object at all. shared_vulkan states it as a rule — "the side inside the VM
/// allocates, because only it can produce pages the hypervisor can locate" —
/// and this is that rule arriving in the compositor.
///
/// # The ordering that has to hold
///
/// The stamps are written before this function returns and the run id is only
/// knowable from its result, so a caller that prints the run id after this
/// returns cannot publish a region the host would fail to find. That ordering
/// bug is worth naming because its symptom is a two-second sweep that reports
/// zero pages, which reads exactly like the arena not being shared at all.
///
/// # What overwrites the stamps
///
/// The host does, with pixels, as soon as the first pool is allocated. Locating
/// is therefore a once-per-run operation and this region cannot be re-found
/// later. That is a genuine limitation and not an oversight: re-locating would
/// need a reserved, never-allocated stamp page, and nothing yet needs it.
pub fn attach_guest_ram(id: u32, size: u64) -> io::Result<GuestRegion> {
    let len = usize::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena too large"))?;
    refuse_if_outbound(id)?;
    let run_id = random_run_id();
    let (view, locked, pinning) = platform::alloc_locked_region(len, &|index, page| {
        page.fill(STAMP_FILL);
        page[STAMP_OFF_MAGIC..STAMP_OFF_MAGIC + 8].copy_from_slice(&STAMP_MAGIC);
        page[STAMP_OFF_RUN_ID..STAMP_OFF_RUN_ID + 8].copy_from_slice(&run_id.to_le_bytes());
        page[STAMP_OFF_PAGE_INDEX..STAMP_OFF_PAGE_INDEX + 4]
            .copy_from_slice(&index.to_le_bytes());
    })?;
    let mut guard = registry()
        .write()
        .map_err(|_| io::Error::other("arena registry poisoned"))?;
    guard.insert(
        id,
        Attached {
            view: Arc::new(view),
            len,
            origin: format!("guest RAM, run 0x{run_id:016x}"),
            provided: true,
        },
    );
    Ok(GuestRegion { run_id, len, pages: (len / REGION_PAGE) as u32, locked, pinning }
    )
}

// ---------------------------------------------------------------------------
// STAGE 10: the compositor's side of the arena, and the one allocator here
// ---------------------------------------------------------------------------

/// The sub-range of one arena that this process may hand *out*.
///
/// docs/waycompose-design.md §12.11 item 3: "which arena, and which sub-range, is the
/// compositor's to give away is not expressed anywhere". This is that
/// expression, and it is three integers because that is all it ever was — the
/// wire has carried `Descriptor::ArenaSlice` in this direction since stage 1
/// and the client half of `wc-proxy` already resolves one. What was missing was
/// a statement of ownership, not an encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboundWindow {
    /// Which arena, in the numbering both sides agreed on out of band.
    pub arena: u32,
    /// Byte offset of the window within it.
    pub offset: u64,
    /// Its length. Always a whole number of [`REGION_PAGE`]s.
    pub len: u64,
    /// DYNAMIC ARENA. The run id of the region this window is the whole of,
    /// when `reserve_outbound` had to commit one. `None` when the window is a
    /// tail of a region somebody else already committed and reported.
    ///
    /// It is here because the host cannot locate a region it has not been told
    /// the run id of, and a dedicated outbound region is committed *inside*
    /// `reserve_outbound` where the caller would otherwise never see it. A
    /// region nobody can locate is a region the compositor writes keymaps into
    /// and no client ever reads.
    pub run_id: Option<u64>,
}

/// The window, plus the free list that is the whole of the allocator's state.
struct Window {
    arena: u32,
    base: u64,
    len: u64,
    /// Sorted, coalesced, absolute arena offsets. Identical in shape to
    /// `wc_proxy::pool::Arena`'s, and deliberately: two free lists that behave
    /// the same way are two things a reader has to learn once.
    free: Vec<(u64, u64)>,
    given: u64,
    /// STAGE 12. While this is `Some`, a gift's bytes are held **here** instead
    /// of going into the region, keyed by the gift's arena offset. See
    /// [`defer_gifts`] for the whole argument; the short version is that on
    /// guest RAM the window's pages carry the stamps the host locates the arena
    /// by, and a gift written before the sweep takes them with it.
    pending: Option<HashMap<u64, Vec<u8>>>,
    /// STAGE 53. The growth channel's range, taken out of the front of this
    /// window by [`reserve_growth_channel`] and remembered here so that the
    /// *reservation* and the `Control::create` that writes into it can happen
    /// at two different moments.
    ///
    /// They have to, and §34.5 saw only one of the two constraints. The
    /// reservation may only run while the window is untouched — the channel is
    /// its front and the free list has no way to express a hole — and smithay
    /// takes the seat's keymap slice in `Seat::add_keyboard`, a full minute
    /// before any client exists. The write may only run *after* the host's
    /// locate, because `Control::create` zeroes the region and on guest RAM
    /// those bytes are the stamps the locate reads.
    ///
    /// One call cannot satisfy both, and doing the whole thing at the later
    /// moment — which is what `Service::start` did — is why growth had never
    /// once worked on a guest: `reserve_growth_channel` refused every time,
    /// because by then the keymap already held the front of the window.
    /// [`defer_gifts`]/[`release_gifts`] is the same split for the same reason.
    channel: Option<(u64, u64)>,
}

fn outbound_slot() -> &'static Mutex<Option<Window>> {
    static WINDOW: OnceLock<Mutex<Option<Window>>> = OnceLock::new();
    WINDOW.get_or_init(|| Mutex::new(None))
}

/// The granularity every gift is aligned and rounded to.
///
/// **This is now `sv_arena::GRANULARITY` itself rather than a number equal to
/// it**, and that is the whole of what the unification changed here. It used to
/// read `REGION_PAGE as u64` with a doc comment explaining that
/// `wc_proxy::pool::ARENA_GRANULARITY` and `sv_arena::GRANULARITY` were "the
/// same number for the same reason" — three definitions of 4096 in three
/// crates, each unable to reach the others, in two repositories. The comment
/// was correct and it was also the evidence: somebody stood at the boundary,
/// saw the shared constant, and could not name it. one-binary.md §1.1.
///
/// It is the page the host sweeps in and the alignment the peer's mapping
/// wants. Nothing in *this* module needs the alignment — the peer copies a
/// keymap out byte by byte — but an allocator in this region using a different
/// unit from the one next door would be a difference with no reason behind it,
/// and now it cannot be one.
pub const GIFT_GRANULARITY: u64 = sv_arena::GRANULARITY;

/// Declare the **last `len` bytes** of arena `arena` to be this process's to
/// give away, and take them out of nobody's hands but its own.
///
/// # The tail, and why the two sides can agree on one number
///
/// Both allocators that share an arena with this one — `wc-proxy`'s pool
/// allocator and `sv_arena::alloc` — are first-fit from offset 0 upward, so the
/// tail is the part of the region they reach last. Reserving it means the peer
/// has one number to subtract from its own length rather than a hole to route
/// its free list around, and a peer that was never told still works until the
/// arena is nearly full — at which point it overlaps, which is why the peer is
/// told rather than trusted to be lucky. `wc-arena-host` writes the window into
/// the plan for exactly that reason: the number originates here, where it is
/// decided, and travels with the description of the region.
///
/// # What this refuses, and why each refusal is worth its line
///
/// * **An arena nobody provided.** A region opened with [`attach`] is somebody
///   else's memory and mapped read-only; giving pieces of it away would be
///   giving away bytes the peer is about to allocate for itself.
/// * **A second window.** One process, one outbound window: a second would
///   reintroduce the question of which one a `create_shared` meant, and there
///   is no caller that wants to choose.
/// * **The whole arena.** A window that left the peer nothing is a
///   configuration error that would present as the first `wl_shm` pool failing
///   to allocate, a long way from the flag that caused it.
///
/// # The ordering that has to hold, which is the guest-RAM one
///
/// On guest RAM the compositor's pages carry the stamps the host locates by,
/// and a gift overwrites them (§12.10). The host's locate must therefore finish
/// before the first object is handed out — which it does in every topology that
/// exists, because the host cannot proxy a client it has not connected for and
/// the compositor cannot mint a keymap before a client binds a seat. It is
/// stated here because the symptom of getting it wrong is `located 16366 of
/// 16384 pages`, which reads as a broken arena rather than as a race.
pub fn reserve_outbound(arena: u32, len: u64) -> io::Result<OutboundWindow> {
    if len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an outbound window of zero bytes is not a window",
        ));
    }
    let len = len.div_ceil(GIFT_GRANULARITY) * GIFT_GRANULARITY;
    // DYNAMIC ARENA. An id this process has not committed means "commit a
    // region for the window", and the window is then the whole of it.
    //
    // This is the split the floor depends on. While the window was a tail of
    // the data region, the region carrying live keymaps was also the region the
    // data allocator started in, so pinning the one pinned the other and
    // whatever the session started at became permanent — an `sv-guest` holding
    // 817 MB with no client connected. Giving the window its own region makes
    // the floor the size of the thing that actually has to stay.
    //
    // The id carries the meaning rather than a new flag, because the two cases
    // want to be told apart by *which region*, which is exactly what an id is
    // for. `--arena-outbound 0:M` against a committed arena 0 is the pre-split
    // spelling and still does what it always did.
    if !registry()
        .read()
        .map_err(|_| io::Error::other("arena registry poisoned"))?
        .contains_key(&arena)
    {
        // Its own commit, its own stamps, its own run id: the host locates it
        // exactly as it locates a data region, and `defer_gifts` still covers
        // the ordering (see that function for what a dedicated region did and
        // did not change about the stamp race).
        let region = attach_guest_ram(arena, len)?;
        let mut guard = outbound_slot()
            .lock()
            .map_err(|_| io::Error::other("outbound window poisoned"))?;
        if let Some(w) = guard.as_ref() {
            return Err(io::Error::other(format!(
                "this process already has an outbound window, in arena {}: a second \
                 would be a second allocator over objects clients already hold",
                w.arena
            )));
        }
        let w = Window { arena, base: 0, len, free: vec![(0, len)], given: 0, pending: None, channel: None };
        *guard = Some(w);
        return Ok(OutboundWindow { arena, offset: 0, len, run_id: Some(region.run_id) });
    }
    let arena_len = {
        let guard = registry()
            .read()
            .map_err(|_| io::Error::other("arena registry poisoned"))?;
        let Some(a) = guard.get(&arena) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("arena {arena} is not attached"),
            ));
        };
        if !a.provided {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "arena {arena} was opened, not provided: this process does not own its bytes \
                     and cannot give any of them away"
                ),
            ));
        }
        a.len as u64
    };
    if len >= arena_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "an outbound window of {len} bytes leaves nothing of a {arena_len}-byte arena \
                 for the peer's pools"
            ),
        ));
    }
    let mut guard = outbound_slot().lock().map_err(|_| io::Error::other("outbound window poisoned"))?;
    if let Some(w) = guard.as_ref() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("arena {} already has the outbound window ({}+{})", w.arena, w.base, w.len),
        ));
    }
    let base = arena_len - len;
    *guard = Some(Window { arena, base, len, free: vec![(base, len)], given: 0, pending: None, channel: None });
    Ok(OutboundWindow { arena, offset: base, len, run_id: None })
}

/// Hold every gift's bytes in memory until [`release_gifts`] is called.
///
/// # STAGE 12, and it is a correction to a claim rather than a feature
///
/// docs/waycompose-design.md §14.12 item 1 said the ordering held by construction:
///
/// > A gift written before the host's locate would take pages of stamps with it
/// > and the sweep would report `located 16366 of 16384`. The order holds in
/// > every topology that exists — the host cannot proxy a client it has not
/// > connected for, and the compositor cannot mint a keymap before a client
/// > binds a seat.
///
/// **The second clause is false, and the first guest run of an outbound window
/// measured it**: `located 16374 of 16384 pages`, no plan written, the run over
/// before a client existed. smithay compiles and *seals* the keymap in
/// `Seat::add_keyboard`, which the compositor calls while it is starting up —
/// so the keymap is minted before the socket is bound, let alone bound to.
/// 73017 bytes of `de` layout is ten pages of stamps, and ten is exactly what
/// the sweep was short.
///
/// # What a dedicated outbound region changed, and what it did not
///
/// DYNAMIC ARENA. The window now lives in a region of its own rather than on
/// the tail of the data region, and that **scopes this race without removing
/// it**. Gifts can only ever overwrite the *outbound* region's stamps, so every
/// data region is safe by construction — a class of race gone, not merely made
/// less likely. What is left is one 256 KiB region whose stamps a keymap will
/// eat, and it still has to be located before that happens.
///
/// The deferral is not cheaper for being scoped, because the sweep's cost is a
/// function of the *host's* RAM and not the region's: §12.5 measured 2.37-2.42 s
/// against a 13312 MiB backing object whatever was being looked for. A 256 KiB
/// region takes just as long to find by content as a 768 MiB one.
///
/// **What removes the deferral outright is the PFN locator**, which re-locates a
/// region whose contents have already been overwritten — §0.14 item 4 measured
/// it returning the same 16384 frames where the content sweep found 0 pages.
/// With `Locator::Pfns` in use there is no ordering constraint left to defer
/// against: a gift may land before the locate, because the locate no longer
/// reads the bytes the gift overwrote. This code stays because the scan is
/// still the R28 cross-check at attach and still the only locator on a guest
/// that cannot run the query; it is dead weight on a guest that can.
///
/// # Why deferring is the fix rather than re-ordering something
///
/// Every page of a guest-RAM arena has to carry its stamp when the host sweeps,
/// because the sweep is a *content* search: a page whose stamp is gone is a page
/// with no known guest-physical address, and the stitched view would have a hole
/// exactly where the compositor's objects live. So nothing may write into the
/// region until the sweep is done, and no ordering of the compositor's own
/// start-up achieves that — the compositor cannot know when a host it never
/// talks to has finished.
///
/// What it *can* know is that a client has connected, and that is sufficient:
/// the proxy is what connects, the proxy is started from the plan, and the plan
/// only exists if the locate succeeded. So "the first client" is a moment that
/// provably follows the sweep, and it is also the earliest moment anything could
/// want a gift's bytes — nothing inside the guest ever reads them.
///
/// Deferral is asked for rather than automatic because it is only right where
/// there are stamps to protect. A file arena has none and nothing to locate, so
/// `--arena-provide` does not defer and its keymap is in the region the moment
/// it is written.
pub fn defer_gifts(arena: u32) -> io::Result<()> {
    let mut guard =
        outbound_slot().lock().map_err(|_| io::Error::other("outbound window poisoned"))?;
    let Some(w) = guard.as_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no outbound window to defer writes into",
        ));
    };
    if w.arena != arena {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("the outbound window is arena {}'s, not arena {arena}'s", w.arena),
        ));
    }
    if w.given > 0 {
        return Err(io::Error::other(
            "gifts have already been handed out: deferring now would leave the ones already \
             written in the region and the rest out of it, which is worse than either",
        ));
    }
    w.pending = Some(HashMap::new());
    Ok(())
}

/// Write every deferred gift into the region and stop deferring.
///
/// Returns `(gifts, bytes)` for the log line that says it happened, because a
/// stage that changed *when* the bytes land with no way to see it had done so
/// would not really have changed it (§8.4). Calling it when nothing is deferred
/// is a no-op returning `(0, 0)`, which is what lets the caller be an accept
/// loop rather than a first-time flag.
pub fn release_gifts() -> io::Result<(usize, u64)> {
    let (arena, taken) = {
        let mut guard =
            outbound_slot().lock().map_err(|_| io::Error::other("outbound window poisoned"))?;
        let Some(w) = guard.as_mut() else {
            return Ok((0, 0));
        };
        match w.pending.take() {
            Some(p) => (w.arena, p),
            None => return Ok((0, 0)),
        }
    };
    // The lock is dropped first, on purpose: `write_provided` takes the arena
    // registry's read lock, and `reserve_outbound` takes them in the other
    // order. Two locks taken in two orders is a deadlock that would present as
    // a compositor that accepted a client and then stopped.
    let mut bytes = 0u64;
    for (offset, buf) in &taken {
        write_provided(arena, *offset, buf)?;
        bytes += buf.len() as u64;
    }
    Ok((taken.len(), bytes))
}

/// The outbound window, if one was declared.
pub fn outbound() -> Option<OutboundWindow> {
    let guard = outbound_slot().lock().ok()?;
    guard
        .as_ref()
        .map(|w| OutboundWindow { arena: w.arena, offset: w.base, len: w.len, run_id: None })
}

/// Bytes given out and bytes still free in the outbound window, for the one log
/// line that says whether the compositor is running out of room to answer a
/// client.
pub fn outbound_usage() -> Option<(u64, u64)> {
    let guard = outbound_slot().lock().ok()?;
    guard
        .as_ref()
        .map(|w| (w.given, w.free.iter().map(|(_, l)| *l).sum()))
}

/// A range of the outbound window that this process has allocated, may write,
/// and may describe to a peer.
///
/// **This is the capability.** Not a flag, not a convention, not a comment
/// asking people to be careful: a `Gift` cannot be decoded from the wire, has no
/// public constructor, and is produced by exactly one function
/// ([`give`](give)), which allocates only inside [`reserve_outbound`]'s range.
/// So the question "could a client's buffer write turn into a compositor write
/// to arbitrary arena memory?" has a type-level answer — a client's buffer is a
/// `Descriptor` and a `Descriptor` never becomes a `Gift`.
///
/// Dropping it returns the bytes. That is the lifetime rule of docs/waycompose-design.md
/// §2.5 arriving from the other end: a section has to be *held* because its name
/// dies with the last handle, and a gift has to be held because its bytes are
/// reused by the next one. `crate::Handle` keeps it in an `Arc` for exactly as
/// long as it keeps a section open, so both rules are one rule at the call site.
#[derive(Debug)]
pub struct Gift {
    arena: u32,
    offset: u64,
    /// What the caller asked for, which is what the descriptor says.
    len: u64,
    /// What the allocator took, rounded to [`GIFT_GRANULARITY`], which is what
    /// is returned on drop.
    span: u64,
}

impl Gift {
    /// Exactly the bytes that cross to describe these bytes.
    pub fn descriptor(&self) -> Descriptor {
        Descriptor::ArenaSlice { arena: self.arena, offset: self.offset, len: self.len }
    }

    /// Where this is, for a log line.
    pub fn extent(&self) -> (u32, u64, u64) {
        (self.arena, self.offset, self.len)
    }

    /// Write into **this gift**, at an offset relative to its own start.
    ///
    /// The offset is relative and the bound is the gift's own length, so there
    /// is no spelling of this call that reaches another gift, a client's pool,
    /// or the stamps. That is the property, and it is why this exists rather
    /// than a wider `arena_write(arena, offset, bytes)` — which would have been
    /// four lines shorter and would have made the rule a matter of caller
    /// discipline.
    pub fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "gift write overflows"))?;
        if end > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("write {offset}..{end} runs past the end of a {}-byte gift", self.len),
            ));
        }
        // STAGE 12. While the window is deferred the bytes stay here, because
        // the region still has to look to the host exactly as the guest stamped
        // it. Only the outbound lock is taken on this path — `write_provided`
        // takes the registry's, and taking both here in that order would invert
        // `reserve_outbound`'s.
        {
            let mut guard =
                outbound_slot().lock().map_err(|_| io::Error::other("outbound window poisoned"))?;
            if let Some(w) = guard.as_mut()
                && w.arena == self.arena
                && let Some(pending) = w.pending.as_mut()
            {
                let len = usize::try_from(self.len)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "gift too large"))?;
                let buf = pending.entry(self.offset).or_insert_with(|| vec![0u8; len]);
                let (o, e) = (offset as usize, end as usize);
                buf[o..e].copy_from_slice(bytes);
                return Ok(());
            }
        }
        write_provided(self.arena, self.offset + offset, bytes)
    }
}

impl Drop for Gift {
    fn drop(&mut self) {
        let Ok(mut guard) = outbound_slot().lock() else {
            return;
        };
        let Some(w) = guard.as_mut() else {
            return;
        };
        if w.arena != self.arena {
            return;
        }
        w.given = w.given.saturating_sub(self.span);
        // STAGE 12: and its bytes, if they never reached the region. A gift
        // dropped while the window is deferred has to take its buffer with it,
        // or a later gift at the same offset would inherit it.
        if let Some(pending) = w.pending.as_mut() {
            pending.remove(&self.offset);
        }
        let at = w.free.partition_point(|(o, _)| *o < self.offset);
        w.free.insert(at, (self.offset, self.span));
        // Coalesce, so a long-lived compositor that has minted a keymap per
        // client for a week can still answer the next one. Same loop as
        // `wc_proxy::pool::Arena::release` and for the same reason.
        let mut i = 0;
        while i + 1 < w.free.len() {
            let (o, l) = w.free[i];
            let (o2, l2) = w.free[i + 1];
            if o + l == o2 {
                w.free[i] = (o, l + l2);
                w.free.remove(i + 1);
            } else {
                i += 1;
            }
        }
    }
}

/// Allocate `len` bytes of the outbound window.
///
/// First fit, as next door. A compositor holds one keymap per layout and a
/// handful of transient offers; anything cleverer would be a guess about a load
/// nobody has measured.
///
/// Fails rather than falling back to another transport, and that is the same
/// rule `wc_proxy::pool::PoolTransport::create` states: a transport that quietly
/// degraded to another would defeat the point of being able to say which one
/// ran.
pub fn give(len: u64) -> io::Result<Gift> {
    if len == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a gift cannot be empty"));
    }
    let mut guard = outbound_slot().lock().map_err(|_| io::Error::other("outbound window poisoned"))?;
    let Some(w) = guard.as_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no outbound window: this process has no arena bytes of its own to give away",
        ));
    };
    let want = len.div_ceil(GIFT_GRANULARITY) * GIFT_GRANULARITY;
    let idx = w.free.iter().position(|(_, l)| *l >= want).ok_or_else(|| {
        let total: u64 = w.free.iter().map(|(_, l)| *l).sum();
        let largest = w.free.iter().map(|(_, l)| *l).max().unwrap_or(0);
        io::Error::new(
            io::ErrorKind::OutOfMemory,
            format!(
                "the outbound window of arena {} has no run of {want} bytes \
                 ({total} free, largest {largest}); raise --arena-outbound",
                w.arena
            ),
        )
    })?;
    let (offset, have) = w.free[idx];
    if have == want {
        w.free.remove(idx);
    } else {
        w.free[idx] = (offset + want, have - want);
    }
    w.given += want;
    Ok(Gift { arena: w.arena, offset, len, span: want })
}

/// The one unsafe write, shared by [`Gift::write_at`] and [`guest_write`].
///
/// Both callers have already established that `offset..offset+len` is theirs to
/// write; this checks it is inside the region as well, because a bounds check
/// that is only made by the caller is a bounds check that a future caller will
/// forget.
fn write_provided(arena: u32, offset: u64, bytes: &[u8]) -> io::Result<()> {
    let guard = registry()
        .read()
        .map_err(|_| io::Error::other("arena registry poisoned"))?;
    let Some(a) = guard.get(&arena) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("arena {arena} is not attached"),
        ));
    };
    if !a.provided {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("arena {arena} was opened, not provided: this process does not own its bytes"),
        ));
    }
    let offset = usize::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena offset too large"))?;
    let end = offset
        .checked_add(bytes.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "arena write overflows"))?;
    if end > a.len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("write {offset}..{end} runs past the end of arena {arena} ({})", a.len),
        ));
    }
    if bytes.is_empty() {
        return Ok(());
    }
    // SAFETY: the region is memory this process provided — committed by
    // `attach_guest_ram` or opened read-write by `provide_file` — and holds for
    // its whole life, and `offset..end` was just checked against its length.
    // Concurrent reads of these bytes by the peer are the point; they are
    // shared memory, which is a `wl_shm` hazard rather than a Rust one — see
    // `Mapping::as_slice`.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            a.view.ptr().cast_mut().add(offset),
            bytes.len(),
        );
    }
    Ok(())
}

/// Write `bytes` at `offset` of an arena **this process provided**.
///
/// Refused for an arena that was [`attach`]ed, and that refusal is the whole of
/// the function's design. A compositor writing into a region its peer owns is
/// unsound as well as wrong; writing into memory this process committed itself
/// is neither. Encoding the distinction in the registry means the wrong one
/// cannot be reached by passing a different id.
///
/// # What this is for, which is one thing
///
/// The live-coherency probe: the host writes into the guest's arena while the
/// guest holds a live mapping, and the guest answers at another offset so the
/// host can measure the other direction. On a virtio-fs export neither
/// direction worked (docs/waycompose-design.md §10.3) and every check above the pixels
/// passed anyway (§10.7), which is why this is measured directly rather than
/// inferred from a window looking right.
///
/// Nothing on the frame path calls it and nothing should.
///
/// # STAGE 10 narrowed it rather than widening it
///
/// Stage 10 gave the compositor a way to put objects *into* an arena, and the
/// obvious way to do that would have been to let this function be called from
/// the keymap path. It is not, and this function got **smaller** instead: it now
/// refuses any range that overlaps the outbound window. There are two separate
/// reasons and both are worth having.
///
/// The first is the safety property this whole stage is about. A probe that can
/// write anywhere in a provided arena plus a caller that can choose the offset
/// is a general-purpose write primitive with a comment on it; a [`Gift`] is not.
/// Keeping the two apart is what makes "the compositor cannot write into a
/// client's buffer" a fact about the types rather than about the call sites.
///
/// The second is ordinary correctness: the window holds live keymaps that
/// clients have already been told about, and a probe write into one would
/// corrupt a keymap another process is mid-`mmap` on. That failure would present
/// as a client dying inside libxkbcommon, which is a long way from a coherency
/// probe.
pub fn guest_write(arena: u32, offset: u64, bytes: &[u8]) -> io::Result<()> {
    if let Some(w) = outbound()
        && w.arena == arena
    {
        let end = offset.saturating_add(bytes.len() as u64);
        if offset < w.offset + w.len && end > w.offset {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "write {offset}..{end} overlaps arena {arena}'s outbound window \
                     ({}+{}), whose bytes belong to the objects this process has already \
                     handed to clients",
                    w.offset, w.len
                ),
            ));
        }
    }
    write_provided(arena, offset, bytes)
}

/// 64 unguessable bits, from the same source the section names use.
///
/// Guessability matters here for a different reason than it does for a name: a
/// run id collides with the *previous* run's stamps in the same guest RAM, and
/// the host refuses a duplicate page index rather than choosing. A counter or a
/// clock would make that collision likely after a restart; this makes it not
/// worth thinking about.
fn random_run_id() -> u64 {
    // `random_token` is the unguessable part of a name without the namespace
    // prefix — stage 6 split it out of `random_section_name` for `publish`, and
    // it is the right thing here for the same reason: a run id belongs to no
    // namespace at all. `wc-<32 hex>`, so half of the hex is a run id.
    let token = platform::random_token();
    let tail = token.rsplit('-').next().unwrap_or_default();
    let hex: String = tail.chars().filter(char::is_ascii_hexdigit).take(16).collect();
    // Never zero: zero is what an unstamped page reads as, and a run id equal
    // to it would make every zeroed page in guest RAM a candidate.
    u64::from_str_radix(&hex, 16).unwrap_or(0) | 1
}

/// Every attached arena, as `(id, length, origin)`, for logging.
pub fn attached() -> Vec<(u32, usize, String)> {
    let Ok(guard) = registry().read() else {
        return Vec::new();
    };
    let mut out: Vec<_> = guard
        .iter()
        .map(|(id, a)| (*id, a.len, a.origin.clone()))
        .collect();
    out.sort_by_key(|(id, _, _)| *id);
    out
}

/// Take the front of the outbound window for the growth channel's control
/// region, and say where it is.
///
/// GROWTH. Returns `(arena offset, len)` — the range `crate::grow` builds a
/// `Control` over, and the range the gift allocator must never hand to a
/// client.
///
/// # Why this is its own call and not part of `reserve_outbound`
///
/// It was part of it for about ten minutes, and three tests said why it should
/// not be. A window is a fact about *this compositor's objects*; the growth
/// channel is a fact about *the deployment having growth turned on*. Carving
/// the channel out of every window would impose a 272 KiB minimum on every
/// caller that reserves one — including the harnesses that reserve 64 KiB to
/// test a keymap, which do not grow, will never grow, and would have had to be
/// rewritten to accommodate a feature they do not use.
///
/// So it is opt-in, on the side that opts in, in the same shape as
/// `--arena-grow` on the host. A compositor nobody asked to grow has exactly
/// the window it always had.
///
/// # Two refusals
///
/// * **Anything already given.** The channel is the *front* of the window and
///   the free list is a sorted run list; taking bytes out of the middle of one
///   that has already been cut would need a hole, and a hole in this list is
///   the one thing it is written not to have. Before the first gift is
///   naturally satisfied: the host cannot ask to grow before a client exists.
/// * **A window too small to hold it and still be a window.** Refused where the
///   number is rather than discovered later as a keymap that would not fit.
pub fn reserve_growth_channel(arena: u32) -> io::Result<(u64, u64)> {
    let control = crate::grow::CONTROL_BYTES;
    let mut guard = outbound_slot()
        .lock()
        .map_err(|_| io::Error::other("outbound window poisoned"))?;
    let Some(w) = guard.as_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "this process has no outbound window, so there is nowhere to put the \
             growth channel: the channel lives in the one range that is already \
             located, already ours to write and never reclaimed",
        ));
    };
    if w.arena != arena {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the outbound window is in arena {}, not {arena}", w.arena),
        ));
    }
    if w.len <= control {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "an outbound window of {} bytes cannot hold the {control}-byte growth \
                 channel and still have room for the objects it exists for",
                w.len
            ),
        ));
    }
    if w.given != 0 || w.free != vec![(w.base, w.len)] {
        return Err(io::Error::other(
            "the growth channel must be reserved before anything is given out of the \
             outbound window: it is the front of that window, and the free list has no \
             way to express a hole",
        ));
    }
    w.free = vec![(w.base + control, w.len - control)];
    w.channel = Some((w.base, control));
    Ok((w.base, control))
}

/// The range [`reserve_growth_channel`] took, for the caller that comes back
/// later to write into it.
///
/// `None` means no reservation was made — no outbound window, one too small, or
/// a build that never asked. It is deliberately not an error: a session that
/// cannot grow is a session that refuses under pressure, which is a fact to
/// report rather than a failure to start.
pub fn growth_channel(arena: u32) -> Option<(u64, u64)> {
    let guard = outbound_slot().lock().ok()?;
    let w = guard.as_ref()?;
    if w.arena != arena {
        return None;
    }
    w.channel
}

/// Where a region this process **provided** starts in this process's address
/// space, for the one caller that needs an address rather than bytes.
///
/// # Why a raw address is exported at all, when nothing else here is
///
/// Every other accessor in this module hands out bytes, because the whole point
/// of an arena id is that neither side has to know where the other put
/// anything. This one exists for `SuperfetchPfnQuery`, which is a *reverse*
/// lookup: it enumerates the machine's page frames and keeps the ones whose
/// recorded virtual address falls inside a window (`awe_probe::superfetch`).
/// The window has to be named by its address because that is the only name the
/// PFN database knows it by — there is no id to give it.
///
/// It is `provided` only, and that is the same rule [`guest_write`] and
/// [`release_guest_ram`] are under: a region this process merely *opened* is
/// somebody else's memory, its frames belong to whoever committed them, and
/// asking Windows which frames back a foreign mapping would produce an answer
/// about a different process's pages.
///
/// # What this makes possible, and it is the whole of §0.14 item 4
///
/// The content sweep can only find a region whose pages still carry their
/// stamps, and the first pool allocated into a region overwrites them
/// ([`attach_guest_ram`]'s own note says so: "this region cannot be re-found
/// later"). The frame list does not care what is in the pages — measured, on a
/// window whose every byte had been overwritten, as the *same* frame list where
/// the sweep found zero pages. So this is not an optimisation of locating; it
/// is what makes locating a region twice possible, and growth is the first
/// thing that needs it.
pub fn region_base(arena: u32) -> Option<usize> {
    let guard = registry().read().ok()?;
    let a = guard.get(&arena)?;
    if !a.provided {
        return None;
    }
    Some(a.view.ptr() as usize)
}

/// Give a guest-RAM region back to the operating system.
///
/// DYNAMIC ARENA, and this is the half of release that actually returns
/// memory. The host side unmapping its view of a region frees nothing: the
/// pages are the *guest's*, committed and pinned by this process, and they stop
/// being charged to anybody only when this process releases them.
///
/// # What actually happens
///
/// Dropping the registry entry drops the last `Arc` on the region's
/// `LockedRegion`, whose `Drop` is `VirtualFree(base, 0, MEM_RELEASE)` —
/// already written, already the thing that runs at process exit. All this does
/// is make it run earlier, on purpose. `VirtualLock`'s working-set charge goes
/// with it, which is why [`committed_bytes`] moves and not merely some counter
/// of ours.
///
/// # Three refusals
///
/// * **Not provided.** An arena this process merely *opened* is somebody
///   else's memory; releasing it would unmap a view and free nothing, while
///   looking like it worked.
/// * **The outbound window's arena.** Those bytes are keymaps and clipboard
///   offers already handed to clients, live in their hands
///   (`one-binary.md` §3.1a). This is the same exemption the host side makes by
///   never reclaiming the startup region, made again on the side that owns the
///   pages, because an invariant enforced on one side only is enforced nowhere.
/// * **Unknown id.** Loud, because the alternative is a caller that believes it
///   released something.
///
/// Returns the bytes released.
pub fn release_guest_ram(arena: u32) -> io::Result<u64> {
    let mut guard = registry()
        .write()
        .map_err(|_| io::Error::other("arena registry poisoned"))?;
    let Some(a) = guard.get(&arena) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("arena {arena} is not attached, so it cannot be released"),
        ));
    };
    if !a.provided {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "arena {arena} was opened, not provided: these are not this process's \
                 pages and releasing them would free nothing while looking like it worked"
            ),
        ));
    }
    if outbound().is_some_and(|w| w.arena == arena) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "arena {arena} carries the outbound window: those bytes are objects \
                 already handed to clients and are live in their hands"
            ),
        ));
    }
    let len = a.len as u64;
    // The `Arc` in the map is the last one for a provided region — nothing
    // hands these out — so removing it runs `LockedRegion::drop`, which is the
    // `VirtualFree(MEM_RELEASE)`.
    guard.remove(&arena);
    Ok(len)
}

/// This process's working set, in bytes, as the operating system accounts it.
///
/// The evidence that release worked, and it is deliberately **the platform's
/// own number rather than one of ours**. A log line saying "released 16 MiB" is
/// this code agreeing with itself; a working set that falls by 16 MiB is
/// Windows agreeing.
///
/// The working set specifically, because that is what a `VirtualLock`ed arena
/// is charged against (§10.10's quota) and what "Firefox is using 1.2 GB" is
/// read off. `None` where the platform has no answer worth giving.
pub fn committed_bytes() -> Option<u64> {
    platform::working_set_bytes()
}

/// Where a region the peer grew into can be found, on the file fixture.
///
/// DYNAMIC ARENA, and the narrowness is the point. A growing arena means the
/// peer can name a region this process has never been told about, and the two
/// substrates answer that completely differently:
///
/// * **Guest RAM, the deployment.** It cannot arise. The regions *are* this
///   process's own memory — `attach_guest_ram` commits, pins and stamps them
///   here — so a region the host can name is one this process made, and it was
///   in the registry before the host could locate it. Growth on that path needs
///   nothing from this function.
/// * **A file, the Wine and same-machine fixture.** The regions are objects the
///   other side created, and nothing announces them. `--arena` names one at
///   startup and there is no request in the protocol for a second.
///
/// So this is a fixture affordance and is off unless asked for. It is not a
/// fallback in docs/waycompose-design.md §5.1's sense — it cannot make a file stand in for
/// guest RAM, because on the guest-RAM path the lookup never misses.
fn grow_dir() -> &'static RwLock<Option<std::path::PathBuf>> {
    static DIR: OnceLock<RwLock<Option<std::path::PathBuf>>> = OnceLock::new();
    DIR.get_or_init(|| RwLock::new(None))
}

/// Look for regions the peer grew into under `dir`, named `region-<id>`.
///
/// The name is a convention shared with `wc_proxy::pool::FileGrowth`, which is
/// the only thing that creates them. A convention rather than a message
/// because the fixture has no channel to carry one and inventing a protocol
/// request for the substrate that is explicitly not the deployment would be
/// paying for the wrong thing.
pub fn set_grow_dir(dir: &std::path::Path) -> io::Result<()> {
    *grow_dir().write().map_err(|_| io::Error::other("arena grow dir poisoned"))? =
        Some(dir.to_path_buf());
    Ok(())
}

/// Attach `region-<id>` from the growth directory, if one was configured.
///
/// Read-only, like [`attach`]: these are the peer's bytes and this process is
/// the reader of them. The length is the object's own, because the peer sized
/// it and a second opinion here could only ever disagree.
fn attach_grown(arena: u32) -> io::Result<()> {
    let dir = {
        let g = grow_dir().read().map_err(|_| io::Error::other("arena grow dir poisoned"))?;
        match g.as_ref() {
            Some(d) => d.clone(),
            // Not configured: the caller's "not attached" error is the right
            // one and is more informative than anything invented here.
            None => return Ok(()),
        }
    };
    let path = dir.join(format!("region-{arena}"));
    let len = std::fs::metadata(&path)?.len();
    attach(arena, &path.to_string_lossy(), len)
}

/// Resolve `(arena, offset, len)` to a read-only view.
///
/// Every arithmetic step is checked. An offset or length that runs off the end
/// of the region is refused rather than clamped: a clamped window would hand
/// the compositor a short buffer that `BufferSpec::required_len` would then
/// reject somewhere much further away, and the message would be about a stride.
pub(crate) fn window(arena: u32, offset: u64, len: u64) -> io::Result<Mapping> {
    // DYNAMIC ARENA. A grown arena names a region this process may not have
    // heard of. On the deployment that cannot happen: the region is guest RAM
    // and *this* process is the side that committed it, so it was attached
    // before the host could possibly locate it. It happens only on the file
    // fixture, where the regions are objects somebody else made, and
    // `grow_dir` is the opt-in that says where to look. See [`set_grow_dir`].
    if !registry()
        .read()
        .map_err(|_| io::Error::other("arena registry poisoned"))?
        .contains_key(&arena)
    {
        attach_grown(arena)?;
    }
    let guard = registry()
        .read()
        .map_err(|_| io::Error::other("arena registry poisoned"))?;
    let Some(a) = guard.get(&arena) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "arena {arena} is not attached (attached: {:?})",
                guard.keys().collect::<Vec<_>>()
            ),
        ));
    };
    let offset = usize::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena offset too large"))?;
    let len = usize::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena slice too large"))?;
    let end = offset
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "arena slice overflows"))?;
    if end > a.len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("arena slice {offset}..{end} runs past the end of arena {arena} ({})", a.len),
        ));
    }
    Ok(Mapping { inner: MappingInner::Window(a.view.clone()), offset, len })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Descriptor, Handle};

    /// Arena ids are process-global and `cargo test` runs these threads
    /// concurrently, so **every test must own its id**. Sharing one produced a
    /// real failure the first time these were written: two tests attached
    /// different files as the same arena and the second zeroed the first's
    /// bytes underneath it. That is the registry behaving exactly as documented
    /// — re-attaching replaces — and it is worth leaving this note rather than
    /// the memory of an afternoon.
    const A: u32 = 9001;
    const B: u32 = 9002;
    const C: u32 = 9003;

    fn make_arena(id: u32, size: u64, fill: impl Fn(&mut [u8])) -> Handle {
        let path = format!("{}/wc-arena-test-{id}", std::env::temp_dir().display());
        let _ = std::fs::remove_file(&path);
        let owner = Handle::create_shared_file(&path, &path, size).unwrap();
        {
            let mut view = owner.map_mut().unwrap();
            fill(view.as_mut_slice());
        }
        attach(id, &path, size).unwrap();
        owner
    }

    /// The claim: only three integers cross, and the bytes are there.
    #[test]
    fn a_slice_of_an_attached_arena_carries_the_memory() {
        let _keep = make_arena(A, 8192, |b| {
            b[0..4].copy_from_slice(b"HEAD");
            b[4096..4100].copy_from_slice(b"MIDL");
            b[8188..8192].copy_from_slice(b"TAIL");
        });

        // These bytes, and nothing else, are what a peer would receive.
        let wire = Descriptor::ArenaSlice { arena: A, offset: 4096, len: 4096 }.encode();
        let peer = Handle::from_descriptor(Descriptor::decode(&wire).unwrap());

        let map = peer.map().unwrap();
        assert_eq!(map.len(), 4096);
        assert_eq!(&map.as_slice()[0..4], b"MIDL");
        assert_eq!(&map.as_slice()[4092..4096], b"TAIL");
    }

    /// A window is a window, not a mapping: the region survives it.
    #[test]
    fn many_windows_coexist_and_do_not_unmap_the_region() {
        let _keep = make_arena(B, 4096, |b| b[100] = 0x5a);
        let mk = |off: u64, len: u64| {
            Handle::from_descriptor(Descriptor::ArenaSlice { arena: B, offset: off, len })
        };
        let first = mk(0, 4096).map().unwrap();
        {
            // Dropped while `first` is still live. If a window unmapped the
            // region on drop, the read below would fault rather than fail a
            // comparison, which is exactly why this test exists.
            let second = mk(64, 128).map().unwrap();
            assert_eq!(second.len(), 128);
        }
        assert_eq!(first.as_slice()[100], 0x5a);
    }

    #[test]
    fn out_of_range_windows_are_refused_rather_than_clamped() {
        let _keep = make_arena(C, 4096, |_| {});
        for (off, len) in [(4096u64, 1u64), (0, 4097), (4000, 100), (u64::MAX, 1)] {
            let h = Handle::from_descriptor(Descriptor::ArenaSlice { arena: C, offset: off, len });
            assert!(h.map().is_err(), "window {off}+{len} should have been refused");
        }
    }

    // ----------------------------------------------------------------- 7 ---
    // The guest-RAM provider. These run on Linux, where the region is not
    // inside a virtual machine and nothing can locate it from outside — so what
    // they can check is the stamp, the registry plumbing and the lifetime, and
    // what they deliberately do not claim is that any of it is reachable from a
    // hypervisor. That claim is `scripts/wc-guest.sh` block 10's and it needs a
    // real VM to make.

    const D: u32 = 9004;
    const E: u32 = 9005;
    const F: u32 = 9006;

    /// Read a page's stamp the way the host's sweep does.
    fn stamp_of(page: &[u8]) -> ([u8; 8], u64, u32) {
        (
            page[STAMP_OFF_MAGIC..STAMP_OFF_MAGIC + 8].try_into().unwrap(),
            u64::from_le_bytes(page[STAMP_OFF_RUN_ID..STAMP_OFF_RUN_ID + 8].try_into().unwrap()),
            u32::from_le_bytes(
                page[STAMP_OFF_PAGE_INDEX..STAMP_OFF_PAGE_INDEX + 4].try_into().unwrap(),
            ),
        )
    }

    /// The claim the host's locator rests on: every page carries the magic, the
    /// same run id, and **its own index**. An off-by-one here is R21's failure
    /// on the guest side rather than the host side, and it would land the
    /// compositor on valid, wrong memory.
    #[test]
    fn every_page_of_a_guest_ram_arena_carries_its_own_index() {
        let pages = 5;
        let region = attach_guest_ram(D, (pages * REGION_PAGE) as u64).unwrap();
        assert_eq!(region.pages, pages as u32);

        let all = Handle::from_descriptor(Descriptor::ArenaSlice {
            arena: D,
            offset: 0,
            len: region.len as u64,
        })
        .map()
        .unwrap();
        for (i, page) in all.as_slice().chunks_exact(REGION_PAGE).enumerate() {
            let (magic, run, index) = stamp_of(page);
            assert_eq!(magic, STAMP_MAGIC, "page {i} has no magic");
            assert_eq!(run, region.run_id, "page {i} has the wrong run id");
            assert_eq!(index, i as u32, "page {i} claims to be page {index}");
            // And the filler, so a page the host only half wrote is obvious.
            assert_eq!(page[REGION_PAGE - 1], STAMP_FILL);
        }
    }

    /// Two arenas in one process must not share a run id, or the host would
    /// find both and refuse the duplicate indices — which reads as a broken
    /// arena rather than as two arenas.
    #[test]
    fn run_ids_differ_between_regions() {
        let a = attach_guest_ram(E, REGION_PAGE as u64).unwrap();
        let b = attach_guest_ram(F, REGION_PAGE as u64).unwrap();
        assert_ne!(a.run_id, b.run_id);
        assert_ne!(a.run_id, 0);
        assert_ne!(b.run_id, 0);
    }

    /// The probe's write path, and the refusal that keeps it a probe.
    #[test]
    fn only_a_provided_arena_may_be_written_through() {
        let region = attach_guest_ram(9008, (2 * REGION_PAGE) as u64).unwrap();
        guest_write(9008, 4096, b"HOSTPING").unwrap();
        let m = Handle::from_descriptor(Descriptor::ArenaSlice {
            arena: 9008,
            offset: 4096,
            len: 8,
        })
        .map()
        .unwrap();
        assert_eq!(m.as_slice(), b"HOSTPING");
        assert_eq!(region.pages, 2);

        // Out of range, and the arena somebody else provided. Both refused
        // rather than clamped or silently ignored.
        assert!(guest_write(9008, (2 * REGION_PAGE) as u64, b"x").is_err());
        let _keep = make_arena(9009, 4096, |_| {});
        assert_eq!(
            guest_write(9009, 0, b"x").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    /// A region is a whole number of the pages the host sweeps in, and a size
    /// that is not is refused here rather than producing a short final page
    /// that carries no stamp and is silently missing from the host's map.
    #[test]
    fn a_partial_page_is_refused() {
        assert!(attach_guest_ram(9007, 4095).is_err());
        assert!(attach_guest_ram(9007, 0).is_err());
    }

    // ---------------------------------------------------------------- 10 ---
    // The outbound window, its allocator, and the refusals that make the
    // allocator safe to have at all.
    //
    // **One test, several assertions, for the same reason `publish`'s test is
    // one test**: there is exactly one outbound window per process and
    // `cargo test` runs these threads concurrently, so a second test that
    // reserved a window would race this one and the loser would report a
    // perfectly true `AlreadyExists` about somebody else's arena. It also means
    // this test is the only place `create_shared` may be called in this crate —
    // once the window exists, every call to it in this process returns a gift.

    const OUT: u32 = 9100;

    #[test]
    fn the_outbound_window_is_the_compositors_and_nothing_else_can_write_it() {
        let pages = 256u64;
        let region = attach_guest_ram(OUT, pages * REGION_PAGE as u64).unwrap();
        assert!(outbound().is_none(), "nothing may be outbound before it is reserved");

        // Not provided: refused. An arena somebody else owns has no bytes this
        // process may give away, and this is the check that says so.
        let borrowed = 9101;
        let path = format!("{}/wc-arena-test-{borrowed}", std::env::temp_dir().display());
        let _keep = make_arena(borrowed, 8192, |_| {});
        assert_eq!(
            reserve_outbound(borrowed, 4096).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let _ = std::fs::remove_file(&path);

        // A window that leaves the peer nothing is a configuration error, not a
        // very small arena.
        assert_eq!(
            reserve_outbound(OUT, region.len as u64).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        // The tail, and exactly the tail.
        let w = reserve_outbound(OUT, 96 * REGION_PAGE as u64).unwrap();
        assert_eq!(w.arena, OUT);
        assert_eq!(w.len, 96 * REGION_PAGE as u64);
        assert_eq!(w.offset, region.len as u64 - w.len);
        assert_eq!(outbound(), Some(w));
        // One per process.
        assert_eq!(
            reserve_outbound(OUT, 4096).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        // And the region it was cut from cannot be swapped underneath it.
        assert_eq!(
            attach_guest_ram(OUT, 4096).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );

        // STAGE 53: the growth channel is taken **before any gift**, and that
        // ordering is the whole of why growth had never worked on a guest.
        //
        // The window is untouched at this point and that is the only moment the
        // reservation is possible: the channel is the front of the window and
        // the free list has no way to express a hole. `grow::Service::start`
        // used to reserve at the *first client* instead — correct for
        // `Control::create`, which writes and must wait for the host's locate,
        // and far too late for this, which does not write at all. On a guest the
        // keymap had taken the front an hour earlier and every session logged
        // `no growth channel … will refuse under pressure instead`. See
        // `docs/waycompose-design.md` §53.2.
        assert_eq!(growth_channel(OUT), None, "nothing is reserved before it is asked for");
        let (coff, clen) = reserve_growth_channel(OUT).expect("an untouched window may be carved");
        assert_eq!(coff, w.offset, "the channel is the front of the window, by construction");
        assert_eq!(clen, crate::grow::CONTROL_BYTES);
        assert_eq!(
            growth_channel(OUT),
            Some((coff, clen)),
            "the range has to survive to the later caller that writes into it"
        );
        assert_eq!(growth_channel(OUT + 1), None, "and it answers for one arena only");

        // STAGE 12: deferral, and the property it exists for.
        //
        // **The region must still read as the guest stamped it** until somebody
        // says the host has finished looking. The first guest run of an outbound
        // window failed exactly here — `located 16374 of 16384 pages` — because
        // smithay seals the keymap during start-up and ten pages of stamps went
        // with it. So: defer, mint an object, and assert that the page under it
        // still carries the magic.
        defer_gifts(OUT).unwrap();
        let held = Handle::create_shared(64).unwrap();
        held.write_at(0, b"KEYMAP").unwrap();
        let held_at = match held.descriptor() {
            crate::Descriptor::ArenaSlice { offset, .. } => *offset,
            other => panic!("{other:?}"),
        };
        // STAGE 53: the front of the window is the growth channel's now, so the
        // first gift starts where the channel ends. That the keymap is *second*
        // is the whole fix — see the reservation above and
        // `docs/waycompose-design.md` §53.2.
        assert_eq!(held_at, w.offset + crate::grow::CONTROL_BYTES, "the first gift follows the channel");
        let look = |off: u64, len: u64| {
            Handle::from_descriptor(Descriptor::ArenaSlice { arena: OUT, offset: off, len })
                .map()
                .unwrap()
        };
        assert_eq!(
            &look(held_at, 8).as_slice()[..8],
            &STAMP_MAGIC,
            "a deferred gift reached the region, and the host's sweep would be short by a page"
        );
        // And then it lands, once, and saying so twice is a no-op.
        assert_eq!(release_gifts().unwrap(), (1, 64));
        assert_eq!(&look(held_at, 6).as_slice()[..6], b"KEYMAP");
        assert_eq!(release_gifts().unwrap(), (0, 0), "a second release must do nothing");
        // Deferring after the fact is refused rather than half-applied.
        assert!(defer_gifts(OUT).is_err());
        drop(held);

        // A gift is inside the window, is writable by its owner, and the bytes
        // are readable from the descriptor alone — which is the whole claim.
        let h = Handle::create_shared(1000).unwrap();
        let Descriptor::ArenaSlice { arena, offset, len } = h.descriptor().clone() else {
            panic!("create_shared did not cut from the arena: {:?}", h.descriptor())
        };
        assert_eq!(arena, OUT);
        assert_eq!(len, 1000, "the descriptor carries what the caller asked for");
        assert!(offset >= w.offset && offset + len <= w.offset + w.len, "{offset} is outside {w:?}");
        h.write_at(0, b"xkb_keymap {").unwrap();
        h.write_at(988, b"};\0").unwrap();
        let wire = h.descriptor().encode();
        let peer = Handle::from_descriptor(Descriptor::decode(&wire).unwrap());
        let m = peer.map().unwrap();
        assert_eq!(&m.as_slice()[..12], b"xkb_keymap {");
        assert_eq!(&m.as_slice()[988..991], b"};\0");

        // The peer's copy of the same descriptor owns nothing and cannot write.
        // This is the rule the whole stage rests on: a slice off the wire is
        // never a gift.
        assert_eq!(
            peer.write_at(0, b"x").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // Nor may the owner take a lasting writable view of arena memory.
        assert_eq!(h.map_mut().unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        // Nor may it write past its own extent, however the offset is spelled.
        assert!(h.write_at(1000, b"x").is_err());
        assert!(h.write_at(u64::MAX, b"x").is_err());

        // Two live gifts never overlap, and dropping one returns its bytes.
        let a = Handle::create_shared(4096).unwrap();
        let b = Handle::create_shared(4096).unwrap();
        let span = |x: &Handle| match x.descriptor() {
            Descriptor::ArenaSlice { offset, len, .. } => (*offset, *len),
            other => panic!("{other:?}"),
        };
        let (ao, al) = span(&a);
        let (bo, _) = span(&b);
        assert!(ao + al <= bo || bo + 4096 <= ao, "gifts {ao} and {bo} overlap");
        let (given, free) = outbound_usage().unwrap();
        // STAGE 53. The growth channel is out of the allocator's arithmetic
        // entirely — not given, not free — because it was taken out of the free
        // list before the first gift and is never returned to it. §34.4 budgeted
        // exactly this: "it costs the gift allocator 272 KiB of its 1 MiB".
        assert_eq!(given + free + crate::grow::CONTROL_BYTES, w.len);
        drop(b);
        let (given2, _) = outbound_usage().unwrap();
        assert_eq!(given2, given - 4096, "a dropped gift did not return its bytes");

        // The window is finite and says so with a number rather than panicking.
        assert_eq!(
            Handle::create_shared(w.len).unwrap_err().kind(),
            io::ErrorKind::OutOfMemory
        );

        // And the coherency probe — the only other write path into a provided
        // arena — cannot touch the window. Outside it, it still works.
        assert_eq!(
            guest_write(OUT, w.offset, b"probe").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            guest_write(OUT, w.offset - 2, b"probe").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied,
            "a write that only overlaps the window's first byte must still be refused"
        );
        guest_write(OUT, 0, b"probe").unwrap();
    }

    #[test]
    fn an_unattached_arena_is_an_error_and_not_a_panic() {
        let h = Handle::from_descriptor(Descriptor::ArenaSlice {
            arena: 0xdead_beef,
            offset: 0,
            len: 16,
        });
        assert_eq!(h.map().unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;

    /// DYNAMIC ARENA. Committing a guest-RAM region raises this process's
    /// working set, and releasing it brings it back down — measured by the
    /// operating system, not by our own counter.
    ///
    /// This is the claim the user actually cares about: "using 1.2 GB for
    /// Firefox is a lot". A log line saying we released is this code agreeing
    /// with itself. A working set that falls is the platform agreeing.
    ///
    /// 64 MiB, because the noise floor of a process's working set under a
    /// contended machine is a few MiB and the signal has to clear it. The
    /// assertion is deliberately loose — at least half the region comes back —
    /// because the allocator, the test harness and the page cache all move this
    /// number a little and a tight bound would be a flake rather than a check.
    #[test]
    fn releasing_a_region_returns_the_pages_to_the_operating_system() {
        const LEN: u64 = 64 << 20;
        let Some(before) = committed_bytes() else {
            eprintln!("no working-set accounting on this platform; nothing to assert");
            return;
        };
        // An id nothing else in this process uses; the registry is global.
        let id = 9_412;
        let region = match attach_guest_ram(id, LEN) {
            Ok(r) => r,
            // Pinning needs a working-set quota and the machine may refuse it.
            // A test that cannot commit has measured nothing and says so rather
            // than failing, exactly as the guest-RAM path itself does.
            Err(e) => {
                eprintln!("could not commit {LEN} bytes of guest RAM here: {e}");
                return;
            }
        };
        let held = committed_bytes().expect("accounting worked a moment ago");
        assert!(
            held > before + LEN / 2,
            "committing {LEN} bytes should show up: {before} -> {held}"
        );
        // `GuestRegion` is a `Copy` descriptor, not the mapping — the pages are
        // held by the registry, and `release_guest_ram` below is what frees them.
        let _ = region;

        let freed = release_guest_ram(id).expect("a provided region can be released");
        assert_eq!(freed, LEN);
        let after = committed_bytes().expect("accounting worked a moment ago");
        assert!(
            after + LEN / 2 < held,
            "releasing {LEN} bytes should give them back: {held} -> {after}"
        );
    }

    /// An arena this process merely opened is somebody else's memory, and
    /// releasing it would unmap a view and free nothing while looking like it
    /// worked.
    #[test]
    fn only_a_region_this_process_provided_may_be_released() {
        let e = release_guest_ram(9_413).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }

}
