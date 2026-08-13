//! One allocator, several consumers, and windows that cannot overlap.
//!
//! `docs/one-binary.md` §3. This module exists because the same allocator was
//! written three times across two repositories, and because the reason it was
//! written three times — that two programs cannot share a free list — stops
//! being true the moment they are one program.
//!
//! # The three that this replaces
//!
//! | where | what it allocated | why it existed separately |
//! |---|---|---|
//! | [`crate::alloc::ArenaAllocator`] | `VkDeviceMemory` sub-ranges, app side | the original |
//! | `wc_proxy::pool::Alloc` | client `wl_shm` pools copied into the arena | a different repository; mirrors `GRANULARITY = 4096` **by hand** |
//! | `wc_handle::arena`'s outbound window | compositor-minted objects (keymaps, gifts) | inside the guest, where the first two are not |
//!
//! Three free lists over what the topology wants to be one region is not a
//! tidiness problem. It is the reason `docs/firefox-accel.md` §7's copy 2
//! exists: two allocators cannot hand out the same offset, so the bytes one
//! produced had to be copied into the other's territory before the far side
//! could name them.
//!
//! # The rule that makes one allocator safe for several consumers
//!
//! It is not a bigger lock. It is that **the consumers are separated by
//! reservation rather than by lock scope**, and the reservation is made once,
//! before anybody is sharing:
//!
//! * a [`Window`] is a page-granular range cut out of the arena and removed from
//!   the free list, so the general allocator can never hand out a byte of it;
//! * windows are checked disjoint against each other and against the arena's
//!   bounds when they are cut;
//! * cutting one takes `&mut self`, and [`SharedArena::share`] is what produces
//!   the second reference — so **the type makes "before the first consumer" a
//!   compile-and-refcount fact rather than a doc comment.** waycompose §17.2
//!   found that shape for `reserve_outbound` and it is the right one; this is
//!   the same trick, generalised to more than one window and moved to the crate
//!   that owns the region.
//!
//! # What the lock does and does not cover
//!
//! [`SharedArena::alloc`] takes the mutex for the free-list edit and drops it
//! before returning. **Nothing holds it across a copy.** waycompose §17.2
//! measured a commit at 414 µs of `memcpy` and said plainly that four
//! connections holding one mutex across that would serialise every client's
//! frames against every other's; that cost is not paid here because the lock
//! protects the free list and the free list only.
//!
//! # What one arena costs and what it saves
//!
//! `docs/firefox-accel.md` rejected a shared arena for the GPU side, for a
//! reason that was correct at the time and is worth restating rather than
//! quietly dropping: *"the descriptor names the whole object"*, so a partitioned
//! arena would have handed every client an fd it could map in full, and one
//! client not reading another's buffers would have been a convention rather than
//! a fact. It chose `N × --arena` bytes of device memory instead — **2 GiB at
//! `--clients 8 --arena 256M`.**
//!
//! In the unified topology that trade is different in both directions, and
//! `one-binary.md` §6 argues it in full. The short form: on the guest side there
//! is no descriptor to hand anybody, because one process owns the region and the
//! consumers are threads; on the host side the isolation the old design bought
//! is **genuinely weakened** and is documented as such rather than assumed away.
//! What is bought is the 2 GiB, and the fact that a frame the GPU wrote and a
//! frame the compositor presents can be the same bytes.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::alloc::{Allocation, ArenaAllocator};
use crate::{Error, GRANULARITY, Result};

/// A page-granular range of the arena, removed from the free list and owned by
/// one consumer.
///
/// A window is not an allocation: the allocator will never return any part of it
/// from [`SharedArena::alloc`], and it is never freed. It is how two consumers
/// of one region stay out of each other's way without a lock and without
/// trusting each other's arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Which window this is, for messages and for [`SharedArena::window`].
    pub role: Role,
    /// Offset from the base of the arena. Page-granular.
    pub offset: u64,
    /// Length in bytes. Page-granular.
    pub len: u64,
}

impl Window {
    /// Does this window contain the whole of `[offset, offset+len)`?
    ///
    /// The bounds check that replaces an argument. waycompose §17.3 narrowed
    /// `peer_slice` from "any range of the region, with a comment explaining why
    /// that was all right" to exactly this, and recorded why: with one session
    /// the difference is invisible, and with four it is the difference between
    /// reading a keymap and reading another client's buffer while it is being
    /// copied into.
    pub fn contains(&self, offset: u64, len: u64) -> bool {
        match offset.checked_add(len) {
            Some(end) => offset >= self.offset && end <= self.offset + self.len,
            None => false,
        }
    }

    /// Do two windows overlap? Checked when the second is cut.
    fn overlaps(&self, offset: u64, len: u64) -> bool {
        let (a0, a1) = (self.offset, self.offset + self.len);
        let (b0, b1) = (offset, offset.saturating_add(len));
        a0 < b1 && b0 < a1
    }
}

/// Who a [`Window`] belongs to.
///
/// A closed set rather than a string, because every member is a decision in
/// `one-binary.md` §3 and adding one should mean editing that section. The two
/// directions are named for who *writes* them, which is the only thing that
/// makes the disjointness argument mean anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Written by the GPU side, read by the compositor. The destination of
    /// `svPresentBlit` (§0.6) and the source of a presented frame — the two
    /// being the same bytes is the point of the whole exercise
    /// (`firefox-accel.md` §7).
    Present,
    /// Written by the host side, read by the compositor. Pixels copied in from
    /// an unmodified `wl_shm` client, which still costs the one copy waycompose
    /// §9.5 shows is structural: `wl_shm.create_pool` is the client handing the
    /// server memory it has *already* allocated, and nothing on Linux can
    /// re-point an existing mapping at another object's pages.
    Inbound,
    /// Written by the compositor, read by the host side. waycompose's
    /// `reserve_outbound`: keymaps and other server-minted objects
    /// (§14.8 — with an arena, `--shared-dir` goes away entirely).
    Outbound,
}

impl core::fmt::Display for Role {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Role::Present => f.write_str("present"),
            Role::Inbound => f.write_str("inbound"),
            Role::Outbound => f.write_str("outbound"),
        }
    }
}

#[derive(Debug)]
struct Inner {
    alloc: Mutex<ArenaAllocator>,
    windows: Vec<Window>,
    arena_len: u64,
    slab_size: u64,
}

/// One arena's allocator, shared by every consumer of that arena.
///
/// Cloning is [`SharedArena::share`] rather than `Clone`, so that "this is
/// another consumer joining" is a thing that appears in the code rather than an
/// incidental `.clone()`.
#[derive(Debug)]
pub struct SharedArena {
    inner: Arc<Inner>,
}

impl SharedArena {
    /// Build the allocator for a region that is already mapped.
    ///
    /// Takes the geometry rather than the mapping: the allocator lives entirely
    /// in private memory (§5.2, "so the GPU side cannot be corrupted by an
    /// allocator bug and the allocator can be Miri-tested against a plain
    /// `Vec<u8>`"), and this module keeps that property.
    pub fn new(arena_len: u64, slab_size: u64, canary: crate::canary::CanaryPolicy) -> Result<SharedArena> {
        let alloc = ArenaAllocator::new(arena_len, slab_size, canary)?;
        Ok(SharedArena {
            inner: Arc::new(Inner {
                alloc: Mutex::new(alloc),
                windows: Vec::new(),
                arena_len,
                slab_size,
            }),
        })
    }

    /// Cut a window out of the arena, before anybody else is holding it.
    ///
    /// Refuses in four ways, and the fourth is the one that is a type doing the
    /// work rather than a check:
    ///
    /// 1. a length that is not page-granular, or zero;
    /// 2. a window that would not fit in the arena;
    /// 3. a window that overlaps one already cut;
    /// 4. **any call at all once [`SharedArena::share`] has been called.**
    ///    `Arc::get_mut` returns `None` the moment a second reference exists, so
    ///    the refcount *is* the check. waycompose §17.2: "a window cannot be
    ///    moved under a session that already holds the arena, and that is
    ///    checked at the only moment it could go wrong rather than asserted in a
    ///    doc comment."
    ///
    /// Windows are cut from the bottom of the arena upwards, in the order they
    /// are asked for, so the general free list keeps one contiguous run at the
    /// top and the fragmentation argument of §5.7 is unaffected. The offset is
    /// **not** chosen by this function — it is whatever the allocator returned,
    /// which is the only way the free list and the window can be guaranteed to
    /// agree.
    pub fn reserve(&mut self, role: Role, len: u64) -> Result<Window> {
        if len == 0 {
            return Err(Error::ZeroLength);
        }
        if !len.is_multiple_of(GRANULARITY) {
            return Err(Error::BadGeometry("a window's length must be page-granular"));
        }

        let inner = Arc::get_mut(&mut self.inner).ok_or(Error::BadGeometry(
            "a window cannot be reserved once the arena is shared — see one-binary.md §3",
        ))?;

        // Take it out of the free list, so the general allocator can never
        // return a byte of it. This is the step that makes the separation a
        // fact rather than an intention: a window that were merely recorded
        // would still be allocatable.
        let offset = {
            let mut alloc = inner.alloc.lock().expect("the arena allocator mutex is poisoned");
            reserve_from_free_list(&mut alloc, len)?
        };

        if let Some(clash) = inner.windows.iter().find(|w| w.overlaps(offset, len)) {
            let _ = clash;
            return Err(Error::BadGeometry("two windows would overlap"));
        }

        let w = Window { role, offset, len };
        inner.windows.push(w);
        Ok(w)
    }

    /// Another consumer joins.
    ///
    /// After this, [`SharedArena::reserve`] can never succeed again — see its
    /// fourth refusal.
    pub fn share(&self) -> SharedArena {
        SharedArena { inner: Arc::clone(&self.inner) }
    }

    /// How many consumers hold this arena.
    pub fn sharers(&self) -> usize {
        Arc::strong_count(&self.inner)
    }

    /// The window cut for a role, if one was.
    pub fn window(&self, role: Role) -> Option<Window> {
        self.inner.windows.iter().find(|w| w.role == role).copied()
    }

    /// Every window cut, in the order they were cut.
    pub fn windows(&self) -> &[Window] {
        &self.inner.windows
    }

    pub fn arena_len(&self) -> u64 {
        self.inner.arena_len
    }

    pub fn slab_size(&self) -> u64 {
        self.inner.slab_size
    }

    /// Allocate from the general pool.
    ///
    /// Takes the lock for the free-list edit and drops it before returning.
    pub fn alloc(&self, size: u64) -> Result<Allocation> {
        self.lock().alloc(size)
    }

    /// Free something allocated by [`SharedArena::alloc`].
    pub fn free_now(&self, a: Allocation) -> Result<()> {
        self.lock().free_now(a)
    }

    /// Turn an [`Allocation`] into the [`crate::handle::Slice`] that names it on
    /// the wire.
    ///
    /// This is the join between the two halves of the unification: the allocator
    /// produces an `Allocation`, the far side is told a `Slice`, and **they are
    /// the same bytes** because one allocator produced both. With two allocators
    /// this function could not have existed, which is why the copy did.
    pub fn slice_of(&self, a: Allocation, arena: u32) -> crate::handle::Slice {
        crate::handle::Slice {
            arena,
            slab: a.slab,
            offset_in_slab: a.offset_in_slab,
            len: a.size,
        }
    }

    /// Check that a slice a peer described is one this consumer is allowed to
    /// read, given the window it claims to be in.
    ///
    /// The refusal carries the numbers, because waycompose §17.3's version of
    /// this refusal is the difference between reading a keymap and reading
    /// another client's buffer, and a bare `false` would not say which.
    pub fn check_in_window(&self, role: Role, offset: u64, len: u64) -> Result<()> {
        let w = self
            .window(role)
            .ok_or(Error::BadGeometry("no window is reserved for that role"))?;
        if !w.contains(offset, len) {
            return Err(Error::OutOfBounds { offset, len, arena_len: w.offset + w.len });
        }
        Ok(())
    }

    /// The allocator itself, for the operations this wrapper does not forward.
    ///
    /// Callers must not hold this across a copy; see the module header.
    pub fn lock(&self) -> MutexGuard<'_, ArenaAllocator> {
        self.inner.alloc.lock().expect("the arena allocator mutex is poisoned")
    }
}

/// Take `len` contiguous bytes out of the free list permanently, and report
/// where they turned out to be.
///
/// A `VkDeviceMemory` may not span two slabs (§5.3, D4), so the allocator will
/// never return more than one slab at a time and a multi-slab window has to be
/// taken as several allocations. **The offset is discovered, not chosen**: each
/// piece is asked for, then checked to be contiguous with the last. Choosing an
/// offset and hoping the allocator agreed is how a window and a free list come
/// to disagree, and a window the allocator can still hand out is worse than no
/// window at all.
///
/// The one condition under which this legitimately cannot succeed is a per-slab
/// canary (§5.2, R26): the canary sits inside each slab, so no range crossing a
/// slab boundary is contiguous. That is refused by name rather than as a generic
/// failure, because the caller's fix is to size the window to one slab and the
/// message should say so.
fn reserve_from_free_list(alloc: &mut ArenaAllocator, len: u64) -> Result<u64> {
    // Refuse up front when it cannot possibly fit, so that the common failure
    // reports the size that was asked for rather than the remainder of a
    // half-finished loop. §5.2's `OutOfArena` is "a normal condition rather than
    // a fault", and a normal condition should read clearly.
    let free = alloc.free_bytes();
    if len > free {
        return Err(Error::OutOfArena { requested: len, largest_free: free });
    }

    let mut taken: Vec<Allocation> = Vec::new();
    let mut remaining = len;
    let mut first: Option<u64> = None;
    let mut next: u64 = 0;

    // Every exit that is not success gives the pieces back. A failed reservation
    // that consumed arena would be a leak at startup that nothing would ever
    // report, which is the quiet kind.
    let outcome = loop {
        if remaining == 0 {
            break Ok(first.unwrap_or(0));
        }
        // The largest single allocation the arena can currently serve. Using
        // this rather than the slab size is what makes the loop correct when a
        // canary or an earlier window has already consumed part of a slab.
        let want = remaining.min(alloc.max_allocation());
        if want == 0 {
            break Err(Error::OutOfArena { requested: len, largest_free: free });
        }
        let got = match alloc.alloc(want) {
            Ok(g) => g,
            Err(e) => break Err(e),
        };
        taken.push(got);
        let at = match alloc.arena_offset(got) {
            Ok(a) => a,
            Err(e) => break Err(e),
        };
        match first {
            None => first = Some(at),
            Some(_) if at != next => {
                break Err(Error::BadGeometry(
                    "a window must be contiguous, and this arena's slabs are not \
                     (a per-slab canary means a window may not cross a slab boundary)",
                ));
            }
            Some(_) => {}
        }
        next = at + got.size;
        remaining -= got.size;
    };

    if outcome.is_err() {
        for a in taken {
            // Best effort: the allocator just handed these out, so a refusal
            // here would mean the free list is already inconsistent and the
            // original error is the one worth reporting.
            let _ = alloc.free_now(a);
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canary::CanaryPolicy;

    const SLAB: u64 = 1024 * 1024;
    const ARENA: u64 = 8 * SLAB;

    fn arena() -> SharedArena {
        SharedArena::new(ARENA, SLAB, CanaryPolicy::None).unwrap()
    }

    #[test]
    fn one_allocator_serves_several_consumers() {
        let a = arena();
        let b = a.share();
        let c = a.share();
        assert_eq!(a.sharers(), 3);

        // Two consumers allocating from one free list never get the same bytes.
        // This is the property that three separate allocators could not have,
        // and it is the whole reason the copy existed.
        let x = b.alloc(4096).unwrap();
        let y = c.alloc(4096).unwrap();
        assert_ne!(
            (x.slab, x.offset_in_slab),
            (y.slab, y.offset_in_slab),
            "two allocators over one region hand out the same offsets; one does not"
        );
    }

    #[test]
    fn a_window_cannot_be_reserved_once_the_arena_is_shared() {
        let mut a = arena();
        a.reserve(Role::Present, SLAB).unwrap();
        let _b = a.share();
        // waycompose §17.2's rule, as a refcount rather than a doc comment.
        assert!(matches!(a.reserve(Role::Inbound, SLAB), Err(Error::BadGeometry(_))));
    }

    #[test]
    fn windows_do_not_overlap_and_the_general_pool_cannot_reach_them() {
        let mut a = arena();
        let present = a.reserve(Role::Present, 2 * SLAB).unwrap();
        let inbound = a.reserve(Role::Inbound, SLAB).unwrap();
        assert!(!present.overlaps(inbound.offset, inbound.len));

        // Drain the general pool completely and assert that nothing it returned
        // lands in either window. A window that were merely recorded rather than
        // removed from the free list would fail exactly here.
        let mut got = Vec::new();
        while let Ok(al) = a.alloc(SLAB) {
            got.push(al);
        }
        assert!(!got.is_empty(), "the general pool should still hold five slabs");
        for al in got {
            let off = a.lock().arena_offset(al).unwrap();
            assert!(!present.contains(off, al.size), "the allocator reached into the present window");
            assert!(!inbound.contains(off, al.size), "the allocator reached into the inbound window");
        }
    }

    #[test]
    fn a_window_that_does_not_fit_is_refused_with_the_numbers() {
        let mut a = arena();
        a.reserve(Role::Present, 7 * SLAB).unwrap();
        match a.reserve(Role::Inbound, 2 * SLAB) {
            Err(Error::OutOfArena { requested, largest_free }) => {
                assert_eq!(requested, 2 * SLAB);
                assert_eq!(largest_free, SLAB);
            }
            other => panic!("expected OutOfArena with the numbers, got {other:?}"),
        }
    }

    #[test]
    fn a_non_granular_window_is_refused() {
        let mut a = arena();
        assert!(matches!(a.reserve(Role::Present, 100), Err(Error::BadGeometry(_))));
        assert!(matches!(a.reserve(Role::Present, 0), Err(Error::ZeroLength)));
    }

    #[test]
    fn reading_outside_a_window_is_refused_with_the_numbers() {
        let mut a = arena();
        let w = a.reserve(Role::Outbound, SLAB).unwrap();
        a.check_in_window(Role::Outbound, w.offset, 4096).unwrap();
        a.check_in_window(Role::Outbound, w.offset + w.len - 4096, 4096).unwrap();
        // One page past the end: with one session invisible, with four it is one
        // client's pixels being handed to another (§17.3).
        assert!(matches!(
            a.check_in_window(Role::Outbound, w.offset + w.len, 4096),
            Err(Error::OutOfBounds { .. })
        ));
        assert!(matches!(
            a.check_in_window(Role::Present, w.offset, 4096),
            Err(Error::BadGeometry(_))
        ));
    }

    #[test]
    fn an_allocation_becomes_the_slice_that_names_it() {
        // The join: one allocator produces the bytes and the name, so they
        // cannot disagree. This is the function that could not exist while there
        // were two arenas, and copy 2 is what stood in for it.
        let a = arena();
        let al = a.alloc(4096 * 10).unwrap();
        let slice = a.slice_of(al, 0);
        slice.validate(a.arena_len(), a.slab_size()).unwrap();
        assert_eq!(
            slice.arena_offset(a.slab_size()).unwrap(),
            a.lock().arena_offset(al).unwrap(),
            "the handle and the allocator must agree about where the bytes are"
        );
    }

    #[test]
    fn four_consumers_share_one_arena_without_overlapping() {
        // waycompose §17.3's test, moved to the crate that owns the region and
        // widened from four connections of one program to four consumers of one
        // arena. The second assertion is the one that matters: overlapping
        // offsets that nobody writes look exactly like working code.
        use std::collections::HashSet;
        let a = arena();
        let shares: Vec<SharedArena> = (0..4).map(|_| a.share()).collect();
        let mut seen: HashSet<(u32, u64)> = HashSet::new();
        let mut all = Vec::new();
        for s in &shares {
            for _ in 0..8 {
                let al = s.alloc(4096).unwrap();
                assert!(seen.insert((al.slab, al.offset_in_slab)), "two consumers got one page");
                all.push(al);
            }
        }
        assert_eq!(all.len(), 32);
        // And the ranges really are disjoint as byte ranges, not merely as keys.
        let mut spans: Vec<(u64, u64)> = all
            .iter()
            .map(|al| (a.lock().arena_offset(*al).unwrap(), al.size))
            .collect();
        spans.sort_unstable();
        for pair in spans.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0, "two allocations overlap");
        }
    }

    #[test]
    fn one_arena_is_what_the_per_session_arenas_cost() {
        // docs/firefox-accel.md: "N × --arena bytes of device memory, so
        // --clients 8 --arena 256M is 2 GiB". The arithmetic is recorded as a
        // test so the number in one-binary.md §6 cannot drift from the code.
        let per_session: u64 = 256 * 1024 * 1024;
        let clients: u64 = 8;
        assert_eq!(per_session * clients, 2 * 1024 * 1024 * 1024);
        // One arena serving all eight is one of them.
        assert_eq!(per_session, per_session * clients / clients);
    }
}
