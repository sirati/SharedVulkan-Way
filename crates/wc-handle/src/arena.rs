//! Arenas: regions both sides mapped **before** any protocol traffic, so that
//! a buffer can cross by arithmetic instead of by opening anything.
//!
//! # Why this is a process-global registry and not a parameter
//!
//! An arena is not a per-message fact. It is attached once, out of band, by
//! whatever brought the two sides together — shared_vulkan's cold-path
//! handshake in the real deployment (its design.md §4.3), a command-line flag
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
//! cost docs/design.md §8.4 says the seam exists to avoid. An arena is
//! genuinely process-wide state; modelling it as such is honest, and it keeps
//! the fork's diff at zero lines.
//!
//! # The lifetime rule, which is the opposite of a section's
//!
//! docs/design.md §2.5 says the *creator* of a section must hold it open until
//! the receiver has opened it. An arena inverts that: the receiver holds the
//! region open for the life of the process, and the sender's slices are pure
//! arithmetic that cannot dangle. Nothing has to be raced. That is the second
//! reason this is the right kind for the real deployment and not merely the
//! available one.
//!
//! # What is *not* here
//!
//! No allocator. Who owns which bytes of an arena is the sender's problem —
//! `sv_arena::alloc` on the shared_vulkan side, `wc-proxy`'s pool allocator on
//! this one. This module resolves `(arena, offset, len)` to memory and refuses
//! anything out of range. Refusing is the whole of its safety contribution and
//! it is worth being clear that it is not much: a peer that can name any offset
//! in the arena can read any of it, exactly as a peer that can name a section
//! can read all of that section. The boundary is between the two sides, not
//! within one arena.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, OnceLock, RwLock};

use crate::{Mapping, MappingInner, platform};

/// One attached region.
struct Attached {
    view: Arc<platform::MappedView>,
    len: usize,
    /// How the region was named, for logs. Never used to reach it again.
    origin: String,
    /// True only for a region **this process committed** ([`attach_guest_ram`]),
    /// as opposed to one it opened because somebody else provided it.
    ///
    /// It exists to gate [`guest_write`], and gating it is the point: a
    /// compositor writing into a client's buffer is a bug in every case, and
    /// writing into a region the peer owns is the one case where it is also
    /// unsound. This process's own memory is neither.
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
/// section, and for the same reason (docs/design.md §7.4).
///
/// Idempotent per id only in the sense that re-attaching replaces: an arena is
/// identified by its number on the wire, and if the two sides ever disagree
/// about what number 0 means, no amount of care here can detect it. That
/// disagreement is what shared_vulkan's `generation` field exists to catch on
/// the real path (its design.md §4.3), and a test harness passing the same
/// flag to both processes is what catches it here.
pub fn attach(id: u32, path: &str, size: u64) -> io::Result<()> {
    let len = usize::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena too large"))?;
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
}

/// Attach arena `id` as **this process's own memory**, stamped so that a
/// process outside the virtual machine can find those pages in the hypervisor's
/// backing object.
///
/// This is docs/design.md §9.10 item 3 and §10.13 item 1: the region stops being
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
    let run_id = random_run_id();
    let (view, locked) = platform::alloc_locked_region(len, &|index, page| {
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
    Ok(GuestRegion { run_id, len, pages: (len / REGION_PAGE) as u32, locked }
    )
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
/// direction worked (docs/design.md §10.3) and every check above the pixels
/// passed anyway (§10.7), which is why this is measured directly rather than
/// inferred from a window looking right.
///
/// Nothing on the frame path calls it and nothing should.
pub fn guest_write(arena: u32, offset: u64, bytes: &[u8]) -> io::Result<()> {
    let guard = registry()
        .read()
        .map_err(|_| io::Error::other("arena registry poisoned"))?;
    let Some(a) = guard.get(&arena) else {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("arena {arena} is not attached")));
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
    // SAFETY: the region is `PAGE_READWRITE` memory this process committed in
    // `attach_guest_ram` and holds for its whole life, and `offset..end` was
    // just checked against its length. Concurrent reads of these bytes by the
    // host are the point of the function; they are shared memory, which is a
    // `wl_shm` hazard rather than a Rust one — see `Mapping::as_slice`.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), a.view.ptr().cast_mut().add(offset), bytes.len());
    }
    Ok(())
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

/// Resolve `(arena, offset, len)` to a read-only view.
///
/// Every arithmetic step is checked. An offset or length that runs off the end
/// of the region is refused rather than clamped: a clamped window would hand
/// the compositor a short buffer that `BufferSpec::required_len` would then
/// reject somewhere much further away, and the message would be about a stride.
pub(crate) fn window(arena: u32, offset: u64, len: u64) -> io::Result<Mapping> {
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
