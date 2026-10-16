//! A two-level segregated-fit (TLSF) allocator over one range of offsets
//! (waycompose-design.md §97).
//!
//! # Why TLSF, and why here
//!
//! §95 measured what killed Firefox on the guest: not a leak and not a working
//! set larger than the arena, but **fragmentation** — 6 MiB free in a 56 MiB
//! arena whose largest free run had fallen to 1.9 MiB, below the 2 MiB slab
//! Zink asked for. The GPU path's memory is now one pool shared by every
//! session (`crate::pool`), carved into leases of very different sizes: 16 MiB
//! control regions, 8 MiB chunks, 64 MiB staging buffers. That is the workload
//! TLSF was designed for:
//!
//! * **bounded time**: allocation and free are O(1) in the number of blocks —
//!   two bitmap scans and a list unlink — so a pool with thousands of leases
//!   costs the same per call as an empty one;
//! * **immediate coalescing**: a freed block merges with both physical
//!   neighbours at once, so free space is always the fewest, largest runs the
//!   live blocks allow;
//! * **good fit, not first fit**: the search rounds the request up to the next
//!   size class, so any block in the class it lands in fits, and the block
//!   chosen is from the smallest class that can serve the request. Large free
//!   runs are not chipped away by small requests while a smaller run would do.
//!
//! Buddy allocation was considered again (docs/design.md §5.2 rejected it for the
//! per-session arena) and rejected again for the same reason: it rounds every
//! block to a power of two, and an 8.3 MB swapchain readback would cost 16 MiB.
//!
//! # Shape
//!
//! The range is `[0, len)` in bytes, managed in units of `granule` bytes (a
//! power of two). Blocks are nodes in a `Vec`, linked two ways: physically, to
//! the blocks before and after them in the range, which is what coalescing
//! walks; and, when free, into the free list of their size class. The classes
//! are TLSF's usual two levels — the first by the position of the highest set
//! bit of the size, the second dividing each first-level class into
//! [`SL_COUNT`] equal parts — with a bitmap per level so that "the smallest
//! non-empty class at least this big" is two `trailing_zeros`.
//!
//! **Everything is in private memory.** The allocator never reads or writes
//! the range it manages, which is what lets the same type serve the guest's
//! pool (whose range is a PCI BAR) and the app side's session heap (whose
//! range is a lease in a file), and lets Miri check it against nothing at all.
//!
//! # The fallback the textbook leaves out
//!
//! Rounding the request up to the next class boundary is what makes the
//! search O(1), and it has a price: a request for 17 units with exactly one
//! free block of 17 units, in the class `[16, 18)`, is rounded to 18 and looks
//! in `[18, 20)` and above — and fails. On a pool that is nearly full, which is
//! precisely when an answer matters, that is an out-of-memory with the memory
//! right there. So when the rounded search finds nothing, the request's *own*
//! class is walked for a block that fits. It is the one non-O(1) step, it runs
//! only on the path that would otherwise fail, and it is bounded by the number
//! of free blocks in one class.

use std::collections::HashMap;

/// Second-level subdivisions per first-level class, as a power of two.
pub const SL_LOG: u32 = 4;
/// Second-level subdivisions per first-level class.
pub const SL_COUNT: usize = 1 << SL_LOG;
/// First-level classes. Sizes are `u64` units, so the highest set bit is at
/// most 63, which maps to first-level index `63 - SL_LOG + 1 = 60`.
const FL_COUNT: usize = 64;

const NIL: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
struct Node {
    /// Start, in units.
    start: u64,
    /// Length, in units. Never zero for a live node.
    len: u64,
    free: bool,
    prev_phys: u32,
    next_phys: u32,
    prev_free: u32,
    next_free: u32,
}

/// What [`Tlsf::stats`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Bytes managed.
    pub len: u64,
    /// Bytes in allocated blocks.
    pub used: u64,
    /// Bytes in free blocks.
    pub free: u64,
    /// The largest single free block, in bytes: the largest request that can
    /// succeed right now.
    pub largest_free: u64,
    /// Free blocks. One means no fragmentation at all.
    pub free_blocks: usize,
    /// Allocated blocks.
    pub used_blocks: usize,
}

/// A TLSF allocator over `[0, len)`.
#[derive(Debug, Clone)]
pub struct Tlsf {
    granule_shift: u32,
    len_units: u64,
    nodes: Vec<Node>,
    /// Indices of `nodes` that are not in use, for reuse.
    spare: Vec<u32>,
    fl_bitmap: u64,
    sl_bitmap: [u32; FL_COUNT],
    heads: Vec<[u32; SL_COUNT]>,
    /// Allocated blocks by start unit — what `free` looks an offset up in.
    used: HashMap<u64, u32>,
    used_units: u64,
}

/// `(first level, second level)` of a size in units. `units >= 1`.
fn mapping(units: u64) -> (usize, usize) {
    debug_assert!(units >= 1);
    if units < SL_COUNT as u64 {
        (0, units as usize)
    } else {
        let f = 63 - units.leading_zeros();
        let fl = (f - SL_LOG + 1) as usize;
        let sl = ((units >> (f - SL_LOG)) as usize) ^ SL_COUNT;
        (fl, sl)
    }
}

/// The class to *search* for a request of `units`: rounded up to the next
/// class boundary, so that every block in the returned class is at least
/// `units` long. `None` when the rounding overflows, which no real request
/// reaches.
fn mapping_search(units: u64) -> Option<(usize, usize)> {
    if units < SL_COUNT as u64 {
        return Some(mapping(units));
    }
    let f = 63 - units.leading_zeros();
    let round = (1u64 << (f - SL_LOG)) - 1;
    Some(mapping(units.checked_add(round)?))
}

impl Tlsf {
    /// An allocator over `[0, len)` bytes in units of `granule`, all free.
    ///
    /// `granule` must be a power of two and `len` a non-zero multiple of it.
    pub fn new(len: u64, granule: u64) -> Result<Tlsf, &'static str> {
        if granule == 0 || !granule.is_power_of_two() {
            return Err("the granule must be a power of two");
        }
        if len == 0 || !len.is_multiple_of(granule) {
            return Err("the length must be a non-zero multiple of the granule");
        }
        let mut t = Tlsf {
            granule_shift: granule.trailing_zeros(),
            len_units: len >> granule.trailing_zeros(),
            nodes: Vec::new(),
            spare: Vec::new(),
            fl_bitmap: 0,
            sl_bitmap: [0; FL_COUNT],
            heads: vec![[NIL; SL_COUNT]; FL_COUNT],
            used: HashMap::new(),
            used_units: 0,
        };
        let n = t.node(Node {
            start: 0,
            len: t.len_units,
            free: true,
            prev_phys: NIL,
            next_phys: NIL,
            prev_free: NIL,
            next_free: NIL,
        });
        t.insert_free(n);
        Ok(t)
    }

    pub fn granule(&self) -> u64 {
        1 << self.granule_shift
    }

    /// Bytes managed.
    pub fn len(&self) -> u64 {
        self.len_units << self.granule_shift
    }

    pub fn is_empty(&self) -> bool {
        self.used.is_empty()
    }

    /// Bytes allocated.
    pub fn used_bytes(&self) -> u64 {
        self.used_units << self.granule_shift
    }

    /// Bytes free.
    pub fn free_bytes(&self) -> u64 {
        (self.len_units - self.used_units) << self.granule_shift
    }

    /// A request rounded to what it will actually take.
    pub fn round(&self, bytes: u64) -> Option<u64> {
        let g = self.granule();
        bytes.checked_add(g - 1).map(|b| b & !(g - 1))
    }

    fn node(&mut self, n: Node) -> u32 {
        match self.spare.pop() {
            Some(i) => {
                self.nodes[i as usize] = n;
                i
            }
            None => {
                self.nodes.push(n);
                (self.nodes.len() - 1) as u32
            }
        }
    }

    fn retire(&mut self, i: u32) {
        self.nodes[i as usize].len = 0;
        self.spare.push(i);
    }

    fn insert_free(&mut self, i: u32) {
        let (fl, sl) = mapping(self.nodes[i as usize].len);
        let head = self.heads[fl][sl];
        {
            let n = &mut self.nodes[i as usize];
            n.free = true;
            n.prev_free = NIL;
            n.next_free = head;
        }
        if head != NIL {
            self.nodes[head as usize].prev_free = i;
        }
        self.heads[fl][sl] = i;
        self.fl_bitmap |= 1 << fl;
        self.sl_bitmap[fl] |= 1 << sl;
    }

    fn remove_free(&mut self, i: u32) {
        let n = self.nodes[i as usize];
        let (fl, sl) = mapping(n.len);
        if n.prev_free != NIL {
            self.nodes[n.prev_free as usize].next_free = n.next_free;
        } else {
            self.heads[fl][sl] = n.next_free;
        }
        if n.next_free != NIL {
            self.nodes[n.next_free as usize].prev_free = n.prev_free;
        }
        if self.heads[fl][sl] == NIL {
            self.sl_bitmap[fl] &= !(1 << sl);
            if self.sl_bitmap[fl] == 0 {
                self.fl_bitmap &= !(1 << fl);
            }
        }
        let n = &mut self.nodes[i as usize];
        n.free = false;
        n.prev_free = NIL;
        n.next_free = NIL;
    }

    /// The head of the smallest non-empty class at or above `(fl, sl)`.
    fn find_suitable(&self, fl: usize, sl: usize) -> Option<u32> {
        let sl_map = self.sl_bitmap[fl] & (!0u32 << sl);
        let (fl, sl) = if sl_map != 0 {
            (fl, sl_map.trailing_zeros() as usize)
        } else {
            let above = if fl + 1 >= FL_COUNT { 0 } else { self.fl_bitmap & (!0u64 << (fl + 1)) };
            if above == 0 {
                return None;
            }
            let fl = above.trailing_zeros() as usize;
            (fl, self.sl_bitmap[fl].trailing_zeros() as usize)
        };
        let h = self.heads[fl][sl];
        (h != NIL).then_some(h)
    }

    /// Allocate `bytes` (rounded up to the granule). Returns the byte offset,
    /// or `None` when no free block is large enough.
    pub fn alloc(&mut self, bytes: u64) -> Option<u64> {
        if bytes == 0 {
            return None;
        }
        let units = self.round(bytes)? >> self.granule_shift;
        let found = match mapping_search(units).and_then(|(fl, sl)| self.find_suitable(fl, sl)) {
            Some(i) => Some(i),
            None => {
                // The request's own class, walked: see the module comment.
                let (fl, sl) = mapping(units);
                let mut i = self.heads[fl][sl];
                while i != NIL && self.nodes[i as usize].len < units {
                    i = self.nodes[i as usize].next_free;
                }
                (i != NIL).then_some(i)
            }
        }?;
        self.remove_free(found);
        let n = self.nodes[found as usize];
        if n.len > units {
            // Split: the tail stays free.
            let tail = self.node(Node {
                start: n.start + units,
                len: n.len - units,
                free: true,
                prev_phys: found,
                next_phys: n.next_phys,
                prev_free: NIL,
                next_free: NIL,
            });
            if n.next_phys != NIL {
                self.nodes[n.next_phys as usize].prev_phys = tail;
            }
            let b = &mut self.nodes[found as usize];
            b.len = units;
            b.next_phys = tail;
            self.insert_free(tail);
        }
        self.used.insert(n.start, found);
        self.used_units += units;
        Some(n.start << self.granule_shift)
    }

    /// The size of the allocated block at `offset`, in bytes.
    pub fn size_of(&self, offset: u64) -> Option<u64> {
        if !offset.is_multiple_of(self.granule()) {
            return None;
        }
        let i = *self.used.get(&(offset >> self.granule_shift))?;
        Some(self.nodes[i as usize].len << self.granule_shift)
    }

    /// Free the block at `offset`. Returns its size in bytes, or `None` if no
    /// block was allocated there — a double free or a wild offset, which the
    /// caller reports; the allocator's state is unchanged either way.
    pub fn free(&mut self, offset: u64) -> Option<u64> {
        if !offset.is_multiple_of(self.granule()) {
            return None;
        }
        let mut i = self.used.remove(&(offset >> self.granule_shift))?;
        let size = self.nodes[i as usize].len;
        self.used_units -= size;
        // Merge with the block before.
        let prev = self.nodes[i as usize].prev_phys;
        if prev != NIL && self.nodes[prev as usize].free {
            self.remove_free(prev);
            let cur = self.nodes[i as usize];
            let p = &mut self.nodes[prev as usize];
            p.len += cur.len;
            p.next_phys = cur.next_phys;
            if cur.next_phys != NIL {
                self.nodes[cur.next_phys as usize].prev_phys = prev;
            }
            self.retire(i);
            i = prev;
        }
        // Merge with the block after.
        let next = self.nodes[i as usize].next_phys;
        if next != NIL && self.nodes[next as usize].free {
            self.remove_free(next);
            let nx = self.nodes[next as usize];
            let c = &mut self.nodes[i as usize];
            c.len += nx.len;
            c.next_phys = nx.next_phys;
            if nx.next_phys != NIL {
                self.nodes[nx.next_phys as usize].prev_phys = i;
            }
            self.retire(next);
        }
        self.insert_free(i);
        Some(size << self.granule_shift)
    }

    /// The largest free block, in bytes. Scans one class: the highest
    /// non-empty one, whose blocks are the only candidates.
    pub fn largest_free(&self) -> u64 {
        if self.fl_bitmap == 0 {
            return 0;
        }
        let fl = 63 - self.fl_bitmap.leading_zeros() as usize;
        let sl = 31 - self.sl_bitmap[fl].leading_zeros() as usize;
        let mut best = 0;
        let mut i = self.heads[fl][sl];
        while i != NIL {
            best = best.max(self.nodes[i as usize].len);
            i = self.nodes[i as usize].next_free;
        }
        best << self.granule_shift
    }

    pub fn stats(&self) -> Stats {
        let free_blocks = self.nodes.iter().filter(|n| n.len != 0 && n.free).count();
        Stats {
            len: self.len(),
            used: self.used_bytes(),
            free: self.free_bytes(),
            largest_free: self.largest_free(),
            free_blocks,
            used_blocks: self.used.len(),
        }
    }

    /// Every allocated block as `(offset, len)` in bytes, in no order.
    pub fn allocations(&self) -> Vec<(u64, u64)> {
        self.used
            .values()
            .map(|&i| {
                let n = self.nodes[i as usize];
                (n.start << self.granule_shift, n.len << self.granule_shift)
            })
            .collect()
    }

    /// Check every invariant, for tests: the blocks tile `[0, len)` exactly,
    /// physical links agree both ways, no two free blocks touch, every free
    /// block is in exactly the free list of its class, the bitmaps say which
    /// lists are non-empty, and the counters match the blocks.
    pub fn check(&self) -> Result<(), String> {
        // Find the first block: the one whose start is 0.
        let first = self
            .nodes
            .iter()
            .position(|n| n.len != 0 && n.start == 0)
            .ok_or("no block starts at 0")? as u32;
        if self.nodes[first as usize].prev_phys != NIL {
            return Err("the first block has a predecessor".into());
        }
        let (mut at, mut i, mut prev, mut prev_free) = (0u64, first, NIL, false);
        let (mut used, mut live) = (0u64, 0usize);
        while i != NIL {
            let n = self.nodes[i as usize];
            if n.len == 0 {
                return Err(format!("block {i} in the chain is retired"));
            }
            if n.start != at {
                return Err(format!("block {i} starts at {} where {at} was expected", n.start));
            }
            if n.prev_phys != prev {
                return Err(format!("block {i}'s predecessor link is wrong"));
            }
            if n.free && prev_free {
                return Err(format!("block {i} is free next to a free block"));
            }
            if !n.free {
                used += n.len;
                if self.used.get(&n.start) != Some(&i) {
                    return Err(format!("allocated block {i} is not in the used map"));
                }
            }
            live += 1;
            prev_free = n.free;
            at += n.len;
            prev = i;
            i = n.next_phys;
        }
        if at != self.len_units {
            return Err(format!("the blocks cover {at} of {} units", self.len_units));
        }
        if used != self.used_units {
            return Err(format!("{used} units in used blocks, the counter says {}", self.used_units));
        }
        if live != self.nodes.len() - self.spare.len() {
            return Err("a live node is not in the physical chain".into());
        }
        if self.used.len() != self.nodes.iter().filter(|n| n.len != 0 && !n.free).count() {
            return Err("the used map and the blocks disagree".into());
        }
        let mut listed = 0usize;
        for fl in 0..FL_COUNT {
            for sl in 0..SL_COUNT {
                let mut j = self.heads[fl][sl];
                let mut back = NIL;
                let nonempty = j != NIL;
                if nonempty != (self.sl_bitmap[fl] & (1 << sl) != 0) {
                    return Err(format!("class ({fl},{sl}) bitmap is wrong"));
                }
                while j != NIL {
                    let n = self.nodes[j as usize];
                    if !n.free || n.len == 0 {
                        return Err(format!("class ({fl},{sl}) lists a block that is not free"));
                    }
                    if mapping(n.len) != (fl, sl) {
                        return Err(format!("block {j} is listed in the wrong class"));
                    }
                    if n.prev_free != back {
                        return Err(format!("block {j}'s free-list back link is wrong"));
                    }
                    listed += 1;
                    back = j;
                    j = n.next_free;
                }
            }
            if (self.sl_bitmap[fl] != 0) != (self.fl_bitmap & (1 << fl) != 0) {
                return Err(format!("first-level bitmap bit {fl} is wrong"));
            }
        }
        let free = self.nodes.iter().filter(|n| n.len != 0 && n.free).count();
        if listed != free {
            return Err(format!("{listed} blocks listed, {free} free"));
        }
        Ok(())
    }
}

/// A small deterministic generator for the property tests here and in
/// [`crate::pool`] and [`crate::heap`]: the workspace takes no test
/// dependencies, and a fixed seed is what makes a failure reproducible.
#[cfg(test)]
pub(crate) struct Rng(pub u64);

#[cfg(test)]
impl Rng {
    pub fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: u64 = 1024;
    const M: u64 = 1024 * K;

    #[test]
    fn the_class_mapping_is_monotonic_and_the_search_class_always_fits() {
        let mut last = (0, 0);
        for units in 1..5000u64 {
            let c = mapping(units);
            assert!(c >= last, "{units}: {c:?} < {last:?}");
            last = c;
            // Every size in the search class is >= units.
            let (fl, sl) = mapping_search(units).unwrap();
            let smallest = (1..20000u64).find(|&u| mapping(u) == (fl, sl)).unwrap();
            assert!(smallest >= units, "{units}: class ({fl},{sl}) starts at {smallest}");
        }
        assert_eq!(mapping(u64::MAX).0, 60);
        assert!(mapping_search(u64::MAX).is_none());
    }

    #[test]
    fn a_fresh_allocator_is_one_free_block_and_gives_out_offsets_in_order() {
        let mut t = Tlsf::new(64 * M, 64 * K).unwrap();
        t.check().unwrap();
        assert_eq!(t.stats().free_blocks, 1);
        assert_eq!(t.largest_free(), 64 * M);
        assert_eq!(t.alloc(1), Some(0));
        assert_eq!(t.size_of(0), Some(64 * K), "rounded to the granule");
        assert_eq!(t.alloc(64 * K), Some(64 * K));
        assert_eq!(t.alloc(3 * M), Some(128 * K));
        t.check().unwrap();
        assert_eq!(t.used_bytes(), 3 * M + 128 * K);
    }

    #[test]
    fn freeing_coalesces_both_ways_back_to_one_block() {
        let mut t = Tlsf::new(16 * M, 4 * K).unwrap();
        let a = t.alloc(M).unwrap();
        let b = t.alloc(M).unwrap();
        let c = t.alloc(M).unwrap();
        assert_eq!(t.free(b), Some(M));
        t.check().unwrap();
        assert_eq!(t.stats().free_blocks, 2);
        assert_eq!(t.free(a), Some(M));
        t.check().unwrap();
        assert_eq!(t.stats().free_blocks, 2, "a merged forward into b's hole");
        assert_eq!(t.free(c), Some(M));
        t.check().unwrap();
        let s = t.stats();
        assert_eq!((s.free_blocks, s.largest_free, s.used), (1, 16 * M, 0));
    }

    #[test]
    fn a_double_free_and_a_wild_offset_change_nothing() {
        let mut t = Tlsf::new(M, 4 * K).unwrap();
        let a = t.alloc(8 * K).unwrap();
        assert_eq!(t.free(a + 4 * K), None, "inside a block, not its start");
        assert_eq!(t.free(a + 1), None, "unaligned");
        assert_eq!(t.free(a), Some(8 * K));
        assert_eq!(t.free(a), None, "twice");
        t.check().unwrap();
        assert_eq!(t.free_bytes(), M);
    }

    /// The fallback: an exact fit in the request's own class is found even
    /// though rounding the request up skips that class.
    #[test]
    fn an_exact_fit_below_the_rounded_class_is_found() {
        // 4 KiB units. Fill 1 MiB, then free one block of 17 units with used
        // blocks on both sides, so it is the only free memory.
        let mut t = Tlsf::new(4 * K * 64, 4 * K).unwrap();
        let a = t.alloc(4 * K * 10).unwrap();
        let hole = t.alloc(4 * K * 17).unwrap();
        let _b = t.alloc(4 * K * 37).unwrap();
        assert_eq!(t.free_bytes(), 0);
        t.free(hole).unwrap();
        assert_eq!(t.alloc(4 * K * 17), Some(hole));
        t.check().unwrap();
        let _ = a;
    }

    /// The property the user asked for: random allocation and free sequences
    /// never hand out overlapping ranges, never lose a byte, and coalesce back
    /// to a single block once everything is freed.
    #[test]
    fn random_sequences_never_overlap_never_leak_and_coalesce() {
        let rounds = if cfg!(miri) { 2 } else { 40 };
        let steps = if cfg!(miri) { 300 } else { 4000 };
        for seed in 0..rounds {
            let mut rng = Rng(seed);
            let granule = [4 * K, 64 * K][(seed % 2) as usize];
            let len = granule * (256 + rng.below(4096));
            let mut t = Tlsf::new(len, granule).unwrap();
            let mut live: Vec<(u64, u64)> = Vec::new();
            for step in 0..steps {
                let grow = live.is_empty() || rng.below(100) < 55;
                if grow {
                    // A mix of sizes: mostly small, sometimes huge.
                    let want = match rng.below(10) {
                        0..=5 => 1 + rng.below(4 * granule),
                        6..=8 => 1 + rng.below(64 * granule),
                        _ => 1 + rng.below(len / 2),
                    };
                    let before = t.largest_free();
                    match t.alloc(want) {
                        Some(off) => {
                            let size = t.size_of(off).unwrap();
                            assert!(size >= want && size - want < granule);
                            assert!(off + size <= len, "past the end");
                            for &(o, s) in &live {
                                assert!(off + size <= o || o + s <= off, "seed {seed} step {step}: overlap");
                            }
                            live.push((off, size));
                        }
                        None => {
                            // It may only fail when nothing large enough is free.
                            assert!(before < t.round(want).unwrap(), "seed {seed} step {step}: refused {want} with {before} free in one block");
                        }
                    }
                } else {
                    let k = rng.below(live.len() as u64) as usize;
                    let (off, size) = live.swap_remove(k);
                    assert_eq!(t.free(off), Some(size));
                }
                if !cfg!(miri) || step % 50 == 0 {
                    t.check().unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                }
                let held: u64 = live.iter().map(|l| l.1).sum();
                assert_eq!(t.used_bytes(), held, "seed {seed} step {step}: leaked");
            }
            for (off, size) in live.drain(..) {
                assert_eq!(t.free(off), Some(size));
            }
            t.check().unwrap();
            let s = t.stats();
            assert_eq!((s.free_blocks, s.largest_free, s.used), (1, len, 0), "seed {seed}: not coalesced");
        }
    }

    /// §95's failure, replayed: a working set of 2 MiB slabs and odd-sized
    /// readback buffers that churn. In a 64 MiB range holding 48 MiB live,
    /// a 2 MiB request must still succeed — good fit keeps the large runs
    /// whole where first fit chipped them.
    #[test]
    fn firefoxs_churn_keeps_a_two_mib_run_free() {
        let len = 64 * M;
        let mut t = Tlsf::new(len, 4 * K).unwrap();
        let mut rng = Rng(95);
        let mut live: Vec<u64> = Vec::new();
        let sizes = [2 * M, 2 * M, 2 * M, 4_423_680, 3_317_760, 7_962_624 / 2, 64 * K, 256 * K];
        for _ in 0..(if cfg!(miri) { 400 } else { 20_000 }) {
            let held = t.used_bytes();
            if held < 46 * M || live.is_empty() {
                let s = sizes[rng.below(sizes.len() as u64) as usize];
                if let Some(o) = t.alloc(s) {
                    live.push(o);
                }
            } else {
                let k = rng.below(live.len() as u64) as usize;
                t.free(live.swap_remove(k)).unwrap();
            }
            if t.used_bytes() <= 46 * M {
                assert!(t.largest_free() >= 2 * M, "{:?}", t.stats());
            }
        }
        t.check().unwrap();
    }
}
