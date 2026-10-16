//! The allocator: a segregated free-list over 4096-byte pages, per slab.
//!
//! docs/design.md §5.2, and §5.5 for deferred free. The design's own summary:
//!
//! > - Granularity is **4096 bytes**, dictated by the measured
//! >   `minImportedHostPointerAlignment` and by the page granularity of the
//! >   shared mapping.
//! > - Size classes are powers of two from 4 KiB to 4 MiB, each a free list of
//! >   page runs; anything larger is served from a per-slab first-fit list of runs.
//! > - Buddy allocation was the alternative and is rejected: it wastes up to 2×
//! >   on the mid-size allocations that dominate … and the arena is a fixed
//! >   pre-negotiated window (R11) where waste converts directly into
//! >   `VK_ERROR_OUT_OF_DEVICE_MEMORY`.
//! > - The allocator lives **entirely on the app side**.
//!
//! # Why the free set is two maps and a set of class lists
//!
//! The authoritative structure is [`FreeSet::by_start`], a `BTreeMap` from start
//! page to length. Everything else is an index over it:
//!
//! - `by_end` maps the *end* of each free run back to its start, which is what
//!   makes coalescing on free an O(log n) lookup rather than a scan. Without it,
//!   freeing is where an allocator quietly becomes quadratic.
//! - `classes[c]` holds the starts of free runs whose length is in
//!   `[2^c, 2^(c+1))`. Entries may be **stale** — a run that has since been
//!   split or merged — and are validated against `by_start` when popped. Lazy
//!   deletion is used rather than eager removal because removing an arbitrary
//!   element from a `Vec` is O(n) and there is no invariant that needs it gone
//!   sooner: a stale entry costs one comparison at the moment it is examined.
//!
//! The one hazard of lazy deletion is unbounded growth in a pathological
//! alloc/free pattern, so [`FreeSet::compact_class`] drops stale entries as it
//! walks them, which bounds the list by the number of *live* runs of that class
//! amortised over the walk.
//!
//! # `unsafe`
//!
//! None. Not one line of this module touches the shared mapping — it computes
//! offsets, and the caller turns an offset into bytes. That is what makes it
//! Miri-testable against a plain `Vec<u8>`, exactly as docs/design.md §5.2 asks.

use crate::canary::{CanaryPolicy, CANARY_LEN};
use crate::{Error, GRANULARITY, Result, round_up};
use std::collections::BTreeMap;

/// Size classes: powers of two from 4 KiB (1 page) to 4 MiB (1024 pages), plus
/// one list for everything larger.
const CLASSES: usize = 12;
const BIG_CLASS: usize = CLASSES - 1;

/// The class a run of `pages` belongs in.
fn class_of(pages: u32) -> usize {
    debug_assert!(pages > 0);
    ((31 - pages.leading_zeros()) as usize).min(BIG_CLASS)
}

/// A slab's free pages.
#[derive(Debug, Default)]
struct FreeSet {
    by_start: BTreeMap<u32, u32>,
    by_end: BTreeMap<u32, u32>,
    classes: Vec<Vec<u32>>,
    free_pages: u32,
}

impl FreeSet {
    fn new() -> FreeSet {
        FreeSet {
            by_start: BTreeMap::new(),
            by_end: BTreeMap::new(),
            classes: vec![Vec::new(); CLASSES],
            free_pages: 0,
        }
    }

    fn insert(&mut self, start: u32, pages: u32) {
        debug_assert!(pages > 0);
        self.by_start.insert(start, pages);
        self.by_end.insert(start + pages, start);
        self.classes[class_of(pages)].push(start);
        self.free_pages += pages;
    }

    fn remove(&mut self, start: u32, pages: u32) {
        self.by_start.remove(&start);
        self.by_end.remove(&(start + pages));
        self.free_pages -= pages;
        // The class list keeps its entry; `is_live` will reject it later.
    }

    /// Is `start` still a free run of a length in class `c`?
    fn is_live(&self, start: u32, c: usize) -> Option<u32> {
        let pages = *self.by_start.get(&start)?;
        (class_of(pages) == c).then_some(pages)
    }

    /// Drop stale entries from a class list while looking for a run of at least
    /// `want` pages. Returns the start of a suitable run, removing it.
    fn take_from_class(&mut self, c: usize, want: u32) -> Option<(u32, u32)> {
        let mut keep = Vec::with_capacity(self.classes[c].len());
        let mut found = None;
        // Walk from the back so that recently freed runs are reused first,
        // which keeps the hot pages hot; the ordering is not load-bearing.
        while let Some(start) = self.classes[c].pop() {
            match self.is_live(start, c) {
                None => continue, // stale: this is the lazy deletion
                Some(pages) if found.is_none() && pages >= want => found = Some((start, pages)),
                Some(_) => keep.push(start),
            }
        }
        self.classes[c] = keep;
        if let Some((start, pages)) = found {
            self.remove(start, pages);
        }
        found
    }

    /// Find `want` pages, first fit within the smallest class that can hold it.
    fn alloc(&mut self, want: u32) -> Option<u32> {
        let first = class_of(want);
        for c in first..CLASSES {
            // Within `first` a run may be shorter than `want` (class `c` holds
            // `[2^c, 2^(c+1))` and `want` may be anywhere in that range), so the
            // search has to compare. Above `first` the smallest possible run is
            // already larger, but comparing costs nothing and means one code path.
            if let Some((start, pages)) = self.take_from_class(c, want) {
                if pages > want {
                    self.insert(start + want, pages - want);
                }
                return Some(start);
            }
        }
        None
    }

    /// Return `pages` at `start`, coalescing with either neighbour.
    fn free(&mut self, start: u32, pages: u32) {
        let mut lo = start;
        let mut hi = start + pages;
        if let Some(&prev_start) = self.by_end.get(&start) {
            let prev_pages = self.by_start[&prev_start];
            self.remove(prev_start, prev_pages);
            lo = prev_start;
        }
        if let Some(&next_pages) = self.by_start.get(&hi) {
            self.remove(hi, next_pages);
            hi += next_pages;
        }
        self.insert(lo, hi - lo);
    }

    fn largest(&self) -> u32 {
        self.by_start.values().copied().max().unwrap_or(0)
    }

    /// The number of live free runs. Fragmentation, as a number a test can look
    /// at — a free-list bug usually shows up here long before it shows up as a
    /// wrong answer.
    fn run_count(&self) -> usize {
        self.by_start.len()
    }
}

/// One slab: an import unit, a failure unit, and nothing else (§5.2).
#[derive(Debug)]
struct Slab {
    index: u32,
    /// Offset of the slab within the arena.
    base: u64,
    pages: u32,
    free: FreeSet,
    /// Every page whose index is a multiple of this is a canary page and is
    /// never handed out. `None` means the slab has no canaries at all, which is
    /// the large-page path where R26 is removed by construction.
    ///
    /// A stride rather than a list because the list would be 16,384 entries per
    /// slab at `PerPage { stride: 1 }`, and a stride answers the only question
    /// anyone asks of it — "is this page reserved?" — in one modulo.
    canary_stride: Option<u32>,
    reserved_pages: u32,
    imported: bool,
}

impl Slab {
    fn is_reserved(&self, page: u32) -> bool {
        self.canary_stride.is_some_and(|s| page.is_multiple_of(s))
    }
}

/// An app-side `VkDeviceMemory` backed by the arena.
///
/// docs/design.md §5.3: "a triple `(slab_id, offset_in_slab, size)` — a sub-range of
/// a slab, 4096-aligned, whose `offset_in_slab + size` never exceeds the slab".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    pub slab: u32,
    pub offset_in_slab: u64,
    /// The rounded size, which is what `VkMemoryRequirements` reports.
    pub size: u64,
}

/// A timeline value an allocation must outlive before it can be reissued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineWatermark {
    /// Slot in the control region's timeline mirror table.
    pub slot: u32,
    pub value: u64,
}

#[derive(Debug)]
struct Retired {
    alloc: Allocation,
    waits: Vec<TimelineWatermark>,
}

/// The arena allocator. One per app-side process.
#[derive(Debug)]
pub struct ArenaAllocator {
    slab_size: u64,
    arena_len: u64,
    slabs: Vec<Slab>,
    canary: CanaryPolicy,
    retired: Vec<Retired>,
}

impl ArenaAllocator {
    pub fn new(arena_len: u64, slab_size: u64, canary: CanaryPolicy) -> Result<ArenaAllocator> {
        if slab_size == 0 || !slab_size.is_multiple_of(GRANULARITY) {
            return Err(Error::BadGeometry("slab size must be a non-zero multiple of 4096"));
        }
        if arena_len == 0 || !arena_len.is_multiple_of(GRANULARITY) {
            return Err(Error::BadGeometry("arena length must be a non-zero multiple of 4096"));
        }
        let slab_count = arena_len.div_ceil(slab_size);
        let slab_count =
            u32::try_from(slab_count).map_err(|_| Error::BadGeometry("too many slabs"))?;

        let slab_pages = u32::try_from(slab_size / GRANULARITY)
            .map_err(|_| Error::BadGeometry("a slab larger than a u32 can page-index"))?;

        // The canary costs a whole *page*, not 64 bytes, because the allocator's
        // granularity is 4096 — see `canary.rs` for why that is 16× design.md
        // §5.2's figure.
        const { assert!(CANARY_LEN <= GRANULARITY) };
        let canary_stride = match canary {
            CanaryPolicy::None => None,
            // Only page 0 of each slab, which a stride of "the whole slab"
            // expresses without a second code path.
            CanaryPolicy::PerSlab { .. } => Some(slab_pages),
            CanaryPolicy::PerPage { stride, .. } => Some(stride.max(1)),
        };

        let mut slabs = Vec::with_capacity(slab_count as usize);
        for index in 0..slab_count {
            let base = u64::from(index) * slab_size;
            // The last slab may be short if the arena is not a whole multiple.
            let len = slab_size.min(arena_len - base);
            let pages = (len / GRANULARITY) as u32;
            let mut slab = Slab {
                index,
                base,
                pages,
                free: FreeSet::new(),
                canary_stride,
                reserved_pages: 0,
                imported: false,
            };
            // Lay the free runs down *between* the canary pages. With
            // `PerSlab` that is one run covering the rest of the slab; with
            // `PerPage { stride }` it is one run per stride, and the largest
            // allocation the slab can serve becomes `stride - 1` pages — which
            // is the real cost of that policy and is why it is not the default.
            let mut page = 0u32;
            while page < pages {
                if slab.is_reserved(page) {
                    slab.reserved_pages += 1;
                    page += 1;
                    continue;
                }
                let start = page;
                while page < pages && !slab.is_reserved(page) {
                    page += 1;
                }
                slab.free.insert(start, page - start);
            }
            if slab.free.free_pages == 0 {
                return Err(Error::BadGeometry("a slab with no allocatable pages"));
            }
            slabs.push(slab);
        }
        Ok(ArenaAllocator { slab_size, arena_len, slabs, canary, retired: Vec::new() })
    }

    pub fn arena_len(&self) -> u64 {
        self.arena_len
    }
    pub fn slab_size(&self) -> u64 {
        self.slab_size
    }
    pub fn slab_count(&self) -> u32 {
        self.slabs.len() as u32
    }
    pub fn canary_policy(&self) -> CanaryPolicy {
        self.canary
    }

    /// Bytes the allocator will never hand out, across the whole arena.
    ///
    /// The number docs/design.md §5.2 gets wrong; see `canary.rs`.
    pub fn reserved_bytes(&self) -> u64 {
        self.slabs.iter().map(|s| u64::from(s.reserved_pages) * GRANULARITY).sum()
    }

    pub fn free_bytes(&self) -> u64 {
        self.slabs.iter().map(|s| u64::from(s.free.free_pages) * GRANULARITY).sum()
    }

    /// The longest run of free bytes in any one slab: the largest allocation
    /// that would succeed now, which `free_bytes` alone cannot say.
    pub fn largest_free_run(&self) -> u64 {
        self.slabs.iter().map(|s| u64::from(s.free.largest()) * GRANULARITY).max().unwrap_or(0)
    }

    /// Allocate `size` bytes, rounded up to the granularity.
    ///
    /// Slabs are tried in order, so a session touches the fewest slabs it can —
    /// which is what makes lazy import (§5.2) actually bound the number of
    /// driver imports rather than merely defer them.
    pub fn alloc(&mut self, size: u64) -> Result<Allocation> {
        if size == 0 {
            return Err(Error::ZeroLength);
        }
        let want = round_up(size);
        if want > self.max_allocation() {
            return Err(Error::LargerThanSlab { requested: want, slab_size: self.slab_size });
        }
        let pages = (want / GRANULARITY) as u32;
        for slab in &mut self.slabs {
            if let Some(start) = slab.free.alloc(pages) {
                return Ok(Allocation {
                    slab: slab.index,
                    offset_in_slab: u64::from(start) * GRANULARITY,
                    size: want,
                });
            }
        }
        Err(Error::OutOfArena {
            requested: want,
            largest_free: self
                .slabs
                .iter()
                .map(|s| u64::from(s.free.largest()) * GRANULARITY)
                .max()
                .unwrap_or(0),
        })
    }

    /// The largest single allocation this arena can serve.
    ///
    /// Not simply the slab size: a `VkDeviceMemory` is a sub-range of one slab
    /// (§5.3), and the canary pages punch that slab into pieces. Under
    /// `PerSlab` the answer is one page short of a slab; under
    /// `PerPage { stride }` it is `stride - 1` pages, which is the honest cost
    /// of that policy and the reason it is offered rather than adopted.
    pub fn max_allocation(&self) -> u64 {
        self.slabs
            .iter()
            .map(|s| match s.canary_stride {
                None => u64::from(s.pages) * GRANULARITY,
                Some(stride) => u64::from(stride.min(s.pages) - 1) * GRANULARITY,
            })
            .max()
            .unwrap_or(0)
    }

    /// Return an allocation immediately. Only correct when nothing can still be
    /// referencing it; [`ArenaAllocator::free_deferred`] is the one to use from
    /// `vkFreeMemory`.
    pub fn free_now(&mut self, a: Allocation) -> Result<()> {
        let slab = self
            .slabs
            .get_mut(a.slab as usize)
            .ok_or(Error::NotAllocated { slab: a.slab, offset_in_slab: a.offset_in_slab })?;
        if !a.offset_in_slab.is_multiple_of(GRANULARITY) || !a.size.is_multiple_of(GRANULARITY) || a.size == 0 {
            return Err(Error::NotAllocated { slab: a.slab, offset_in_slab: a.offset_in_slab });
        }
        let start = (a.offset_in_slab / GRANULARITY) as u32;
        let pages = (a.size / GRANULARITY) as u32;
        if u64::from(start) + u64::from(pages) > u64::from(slab.pages) {
            return Err(Error::NotAllocated { slab: a.slab, offset_in_slab: a.offset_in_slab });
        }
        // Freeing a canary page would put it in the free list and the next
        // allocation would overwrite the R26 detector with application data —
        // silently disabling the thing whose entire job is to make a silent
        // failure loud.
        if (start..start + pages).any(|p| slab.is_reserved(p)) {
            return Err(Error::NotAllocated { slab: a.slab, offset_in_slab: a.offset_in_slab });
        }
        // A double free would put the same page in the free set twice, which is
        // the bug that turns into two `VkDeviceMemory`s over one range and then
        // into corruption. Catch it here, where it is still a caller error.
        if slab.free.by_start.range(..=start).next_back().is_some_and(|(&s, &n)| s + n > start) {
            return Err(Error::NotAllocated { slab: a.slab, offset_in_slab: a.offset_in_slab });
        }
        slab.free.free(start, pages);
        Ok(())
    }

    /// `vkFreeMemory`: retire the range against the timelines it could still be
    /// referenced by. docs/design.md §5.5 mechanism 1:
    ///
    /// > `vkFreeMemory` does not return the range to the free list. It moves it
    /// > to a retire list tagged with the current value of every timeline the
    /// > allocation could be referenced by; the allocator reclaims it only once
    /// > the GPU side's mirrored timeline values have passed.
    pub fn free_deferred(&mut self, a: Allocation, waits: &[TimelineWatermark]) {
        self.retired.push(Retired { alloc: a, waits: waits.to_vec() });
    }

    /// Reclaim every retired allocation whose timelines have passed.
    ///
    /// `mirror(slot)` reads the control region's timeline mirror, which is a
    /// local acquire load and not a round trip (docs/design.md §4.2). Returns how
    /// many were reclaimed.
    pub fn reclaim(&mut self, mirror: &dyn Fn(u32) -> u64) -> Result<usize> {
        let mut still_waiting = Vec::new();
        let mut reclaimed = 0;
        for r in std::mem::take(&mut self.retired) {
            if r.waits.iter().all(|w| mirror(w.slot) >= w.value) {
                self.free_now(r.alloc)?;
                reclaimed += 1;
            } else {
                still_waiting.push(r);
            }
        }
        self.retired = still_waiting;
        Ok(reclaimed)
    }

    pub fn retired_count(&self) -> usize {
        self.retired.len()
    }

    /// The arena offset an allocation begins at, bounds-checked (R2).
    pub fn arena_offset(&self, a: Allocation) -> Result<u64> {
        let slab = self
            .slabs
            .get(a.slab as usize)
            .ok_or(Error::NotAllocated { slab: a.slab, offset_in_slab: a.offset_in_slab })?;
        let off = slab.base + a.offset_in_slab;
        let end = off
            .checked_add(a.size)
            .ok_or(Error::OutOfBounds { offset: off, len: a.size, arena_len: self.arena_len })?;
        if end > self.arena_len {
            return Err(Error::OutOfBounds { offset: off, len: a.size, arena_len: self.arena_len });
        }
        Ok(off)
    }

    /// Which slabs have been touched and therefore need importing (§5.2's lazy
    /// import). Marking is the caller's business because the import itself is a
    /// round trip this crate does not make.
    pub fn slab_needs_import(&self, slab: u32) -> bool {
        self.slabs.get(slab as usize).is_some_and(|s| !s.imported)
    }

    pub fn mark_imported(&mut self, slab: u32) {
        if let Some(s) = self.slabs.get_mut(slab as usize) {
            s.imported = true;
        }
    }

    /// `(base, len)` of a slab in the arena.
    pub fn slab_span(&self, slab: u32) -> Result<(u64, u64)> {
        let s = self
            .slabs
            .get(slab as usize)
            .ok_or(Error::BadGeometry("no such slab"))?;
        Ok((s.base, u64::from(s.pages) * GRANULARITY))
    }

    /// Free runs in a slab. Diagnostics, and the number the fragmentation tests
    /// assert on.
    pub fn free_run_count(&self, slab: u32) -> usize {
        self.slabs.get(slab as usize).map(|s| s.free.run_count()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canary::CanaryPolicy;

    const SLAB: u64 = 1 << 20; // 1 MiB = 256 pages, so a test is fast and exact
    const ARENA: u64 = 4 << 20;

    fn allocator() -> ArenaAllocator {
        ArenaAllocator::new(ARENA, SLAB, CanaryPolicy::None).unwrap()
    }

    #[test]
    fn class_boundaries_are_powers_of_two() {
        assert_eq!(class_of(1), 0);
        assert_eq!(class_of(2), 1);
        assert_eq!(class_of(3), 1);
        assert_eq!(class_of(4), 2);
        assert_eq!(class_of(1023), 9);
        assert_eq!(class_of(1024), 10);
        // 4 MiB is 1024 pages and is the largest *class*; everything above goes
        // to the first-fit list, which is what docs/design.md §5.2 specifies.
        assert_eq!(class_of(2048), BIG_CLASS);
        assert_eq!(class_of(u32::MAX), BIG_CLASS);
    }

    #[test]
    fn an_allocation_is_page_aligned_and_rounded_honestly() {
        let mut a = allocator();
        let x = a.alloc(1).unwrap();
        assert_eq!(x.size, 4096, "a 1-byte buffer costs a page, and says so");
        assert_eq!(x.offset_in_slab % GRANULARITY, 0);
        let y = a.alloc(4097).unwrap();
        assert_eq!(y.size, 8192);
        assert_ne!(x.offset_in_slab, y.offset_in_slab);
    }

    #[test]
    fn zero_and_oversized_allocations_are_refused_by_name() {
        let mut a = allocator();
        assert_eq!(a.alloc(0).unwrap_err(), Error::ZeroLength);
        assert!(matches!(a.alloc(SLAB + 1), Err(Error::LargerThanSlab { .. })));
        // Exactly one slab fits, because a VkDeviceMemory is a sub-range of one
        // slab and the whole slab is a legal sub-range of itself.
        assert!(a.alloc(SLAB).is_ok());
    }

    /// The property docs/design.md §13 step 2 asks for: allocations never overlap.
    /// Driven by a deterministic PRNG rather than a proptest dependency — the
    /// workspace has four external crates and this does not need to be the fifth.
    #[test]
    fn allocations_never_overlap_under_random_traffic() {
        let mut a = allocator();
        let mut live: Vec<Allocation> = Vec::new();
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        let mut allocated = 0u64;
        let mut freed = 0u64;
        // Miri interprets every instruction, so the same test runs a tenth of the
        // traffic there. The shape of the traffic is what matters, not its volume.
        let rounds = if cfg!(miri) { 2_000 } else { 20_000 };
        for _ in 0..rounds {
            if !live.is_empty() && next() % 2 == 0 {
                let i = (next() % live.len() as u64) as usize;
                let a_ = live.swap_remove(i);
                a.free_now(a_).unwrap();
                freed += 1;
            } else {
                let size = 1 + next() % (64 * 1024);
                if let Ok(x) = a.alloc(size) {
                    // The invariant, checked against every live allocation.
                    let xs = a.arena_offset(x).unwrap();
                    for other in &live {
                        let os = a.arena_offset(*other).unwrap();
                        assert!(
                            xs + x.size <= os || os + other.size <= xs,
                            "{x:?} at {xs} overlaps {other:?} at {os}"
                        );
                    }
                    live.push(x);
                    allocated += 1;
                }
            }
        }
        // A count of exactly zero deserves as much suspicion as a perfect one:
        // if the allocator had refused everything, the overlap check above would
        // have passed vacuously 20,000 times.
        assert!(allocated > rounds / 4, "only {allocated} allocations succeeded");
        assert!(freed > rounds / 10, "only {freed} frees happened");

        // And after draining, the arena is whole again: one free run per slab,
        // which is the strongest statement available that coalescing works.
        for x in live {
            a.free_now(x).unwrap();
        }
        assert_eq!(a.free_bytes(), ARENA);
        for s in 0..a.slab_count() {
            assert_eq!(a.free_run_count(s), 1, "slab {s} did not coalesce back to one run");
        }
    }

    #[test]
    fn a_full_arena_reports_the_largest_run_it_could_not_serve() {
        let mut a = ArenaAllocator::new(SLAB, SLAB, CanaryPolicy::None).unwrap();
        let mut held = Vec::new();
        while let Ok(x) = a.alloc(64 * 1024) {
            held.push(x);
        }
        assert_eq!(held.len(), (SLAB / (64 * 1024)) as usize);
        let err = a.alloc(64 * 1024).unwrap_err();
        assert_eq!(err, Error::OutOfArena { requested: 65536, largest_free: 0 });
        assert_eq!(a.free_bytes(), 0);

        a.free_now(held.pop().unwrap()).unwrap();
        assert!(a.alloc(64 * 1024).is_ok());
    }

    #[test]
    fn coalescing_puts_a_split_run_back_together() {
        let mut a = ArenaAllocator::new(SLAB, SLAB, CanaryPolicy::None).unwrap();
        let x = a.alloc(4096).unwrap();
        let y = a.alloc(4096).unwrap();
        let z = a.alloc(4096).unwrap();
        assert_eq!(a.free_run_count(0), 1);
        // Free the outer two first, so the middle free has a neighbour on each
        // side — the case a coalescer that only looks forward gets wrong.
        a.free_now(x).unwrap();
        a.free_now(z).unwrap();
        assert_eq!(a.free_run_count(0), 2, "x and z are separated by y");
        a.free_now(y).unwrap();
        assert_eq!(a.free_run_count(0), 1, "freeing the middle must merge all three");
        assert_eq!(a.free_bytes(), SLAB);
    }

    #[test]
    fn a_double_free_is_refused_rather_than_corrupting_the_free_list() {
        let mut a = allocator();
        let x = a.alloc(8192).unwrap();
        a.free_now(x).unwrap();
        assert!(matches!(a.free_now(x), Err(Error::NotAllocated { .. })));
        // …and a range that was never allocated at all.
        assert!(matches!(
            a.free_now(Allocation { slab: 0, offset_in_slab: 512 * 4096, size: 4096 }),
            Err(Error::NotAllocated { .. })
        ));
        assert!(matches!(
            a.free_now(Allocation { slab: 99, offset_in_slab: 0, size: 4096 }),
            Err(Error::NotAllocated { .. })
        ));
        // A misaligned or zero-length free is a caller bug, not a rounding job.
        assert!(matches!(
            a.free_now(Allocation { slab: 0, offset_in_slab: 1, size: 4096 }),
            Err(Error::NotAllocated { .. })
        ));
    }

    /// docs/design.md §13 step 2's other property: "a freed range is not reissued
    /// before its timeline value passes".
    #[test]
    fn a_deferred_free_is_not_reissued_until_its_timeline_passes() {
        let mut a = ArenaAllocator::new(SLAB, SLAB, CanaryPolicy::None).unwrap();
        // Fill the slab so that reissuing the freed range is the *only* way a
        // later allocation can succeed — otherwise this test would pass whether
        // or not the retire list worked.
        let mut held = Vec::new();
        while let Ok(x) = a.alloc(4096) {
            held.push(x);
        }
        let victim = held.pop().unwrap();
        let victim_off = a.arena_offset(victim).unwrap();
        a.free_deferred(victim, &[TimelineWatermark { slot: 3, value: 100 }]);
        assert_eq!(a.retired_count(), 1);

        // The GPU has not reached 100 yet.
        let behind = |_slot: u32| 99u64;
        assert_eq!(a.reclaim(&behind).unwrap(), 0);
        assert!(matches!(a.alloc(4096), Err(Error::OutOfArena { .. })), "reissued too early");

        // It has now.
        let passed = |_slot: u32| 100u64;
        assert_eq!(a.reclaim(&passed).unwrap(), 1);
        assert_eq!(a.retired_count(), 0);
        let reissued = a.alloc(4096).unwrap();
        assert_eq!(a.arena_offset(reissued).unwrap(), victim_off);
    }

    #[test]
    fn a_retirement_waits_for_every_timeline_not_just_one() {
        let mut a = allocator();
        let x = a.alloc(4096).unwrap();
        a.free_deferred(
            x,
            &[
                TimelineWatermark { slot: 0, value: 10 },
                TimelineWatermark { slot: 1, value: 20 },
            ],
        );
        // The first has passed, the second has not. An allocator that reclaimed
        // on `any` rather than `all` would hand this range back while the GPU is
        // still reading it.
        let mixed = |slot: u32| if slot == 0 { 10 } else { 19 };
        assert_eq!(a.reclaim(&mixed).unwrap(), 0);
        let both = |slot: u32| if slot == 0 { 10 } else { 20 };
        assert_eq!(a.reclaim(&both).unwrap(), 1);
    }

    #[test]
    fn arena_offsets_are_bounds_checked_against_the_arena() {
        let a = allocator();
        // A hand-built allocation past the end of its slab: the R2 check has to
        // catch it even though the slab index is valid.
        let bad = Allocation { slab: 3, offset_in_slab: SLAB, size: 4096 };
        assert!(matches!(a.arena_offset(bad), Err(Error::OutOfBounds { .. })));
        let overflow = Allocation { slab: 0, offset_in_slab: 0, size: u64::MAX };
        assert!(matches!(a.arena_offset(overflow), Err(Error::OutOfBounds { .. })));
    }

    #[test]
    fn allocations_land_in_the_slab_they_claim() {
        let mut a = allocator();
        // Fill slab 0 entirely, so the next allocation must come from slab 1.
        let mut held = Vec::new();
        while let Ok(x) = a.alloc(256 * 1024) {
            held.push(x);
            if held.len() > 32 {
                break;
            }
        }
        for x in &held {
            let off = a.arena_offset(*x).unwrap();
            let (base, len) = a.slab_span(x.slab).unwrap();
            assert!(off >= base && off + x.size <= base + len, "{x:?} escaped its slab");
        }
        assert!(held.iter().any(|x| x.slab != 0), "every allocation came from one slab");
    }

    #[test]
    fn lazy_deletion_does_not_let_the_class_lists_grow_without_bound() {
        let mut a = ArenaAllocator::new(SLAB, SLAB, CanaryPolicy::None).unwrap();
        // Repeatedly split and merge the same region. A class list that only
        // ever grew would end up with hundreds of thousands of stale entries.
        let rounds = if cfg!(miri) { 2_000 } else { 50_000 };
        for _ in 0..rounds {
            let x = a.alloc(4096).unwrap();
            a.free_now(x).unwrap();
        }
        let total: usize = a.slabs[0].free.classes.iter().map(|c| c.len()).sum();
        assert!(total < 1000, "class lists grew to {total} entries");
        assert_eq!(a.free_bytes(), SLAB);
        assert_eq!(a.free_run_count(0), 1);
    }

    #[test]
    fn lazy_import_tracks_which_slabs_were_touched() {
        let mut a = allocator();
        assert!(a.slab_needs_import(0));
        let _ = a.alloc(4096).unwrap();
        a.mark_imported(0);
        assert!(!a.slab_needs_import(0));
        assert!(a.slab_needs_import(1), "an untouched slab still needs importing");
    }

    #[test]
    fn an_arena_that_is_not_a_whole_number_of_slabs_still_works() {
        // 3.5 slabs: the last one is short, and its page count has to reflect
        // that or the allocator hands out offsets past the end of the arena.
        let mut a = ArenaAllocator::new(SLAB * 7 / 2, SLAB, CanaryPolicy::None).unwrap();
        assert_eq!(a.slab_count(), 4);
        let (base, len) = a.slab_span(3).unwrap();
        assert_eq!(len, SLAB / 2, "the last slab is short");
        assert_eq!(base + len, SLAB * 7 / 2);
        let mut held = Vec::new();
        while let Ok(x) = a.alloc(64 * 1024) {
            a.arena_offset(x).unwrap();
            held.push(x);
        }
        assert_eq!(held.len() as u64, (SLAB * 7 / 2) / (64 * 1024));
    }
}
