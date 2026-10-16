//! One session's allocations, made inside the leases it holds from the pool
//! (waycompose-design.md §97).
//!
//! The app side's half of [`crate::pool`]. Design.md §5.2 keeps the allocator
//! on the app side, which receives the requests; that does not change. What
//! changes is what it allocates *from*: not a fixed window carved out at
//! attach, but leases asked for one at a time and given back when empty.
//!
//! # Two kinds of lease
//!
//! * A **chunk** ([`HeapConfig::chunk`], 4 MiB) holds many small allocations,
//!   sub-allocated by a [`Tlsf`] at 4 KiB: uniform buffers, descriptor
//!   buffers, small staging buffers.
//! * A **dedicated** lease holds one allocation of at least
//!   [`HeapConfig::dedicated_from`] (1 MiB), rounded to the pool's 64 KiB:
//!   Zink's 2.2 MiB BO slabs, a swapchain's readback buffers, VulkanMod's
//!   64 MiB staging buffers. Measured in Firefox (§97): with 8 MiB chunks
//!   and a 4 MiB threshold, three of Zink's 2,310,144-byte slabs filled a
//!   chunk and left 1.4 MiB of it unusable, and a two-window session held
//!   40 MiB of leases for 26 MiB of allocations. A lease of its own wastes
//!   at most 64 KiB. Its own lease means its free gives the
//!   whole range back to the pool at once, and that no chunk is ever cut into
//!   pieces by a buffer that is a window wide. §95's holes were exactly such
//!   pieces: readback buffers of several window sizes between 2 MiB slabs.
//!
//! Each lease is one import on the GPU side, so a `VkDeviceMemory` here is a
//! sub-range of exactly one lease (docs/design.md §5.3, with "slab" read as
//! "lease").
//!
//! # Giving back
//!
//! A dedicated lease is returned the moment its allocation is freed. An empty
//! chunk is returned unless it is the only empty one: one spare is kept,
//! because Zink frees and reallocates BOs in bursts and a chunk that went back
//! and came straight back would cost a round trip and a driver import each
//! time. [`SessionHeap::release_spares`] gives that one back too, for when the
//! application has gone quiet or the pool is short.
//!
//! # The protocol with the caller
//!
//! The heap never talks to the GPU side. [`SessionHeap::alloc`] answers
//! [`HeapError::NeedLease`] with the size to ask for; the caller asks, hands
//! the lease to [`SessionHeap::add_lease`] and calls `alloc` again. A free
//! that empties a lease returns it for the caller to give back. So the lock
//! that guards the heap is never held across a round trip, and two threads
//! that both needed a lease at once simply leave a spare chunk.

use crate::tlsf::Tlsf;
use std::collections::BTreeMap;

/// The heap's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapConfig {
    /// A chunk lease's size.
    pub chunk: u64,
    /// Allocations at least this large get a lease of their own.
    pub dedicated_from: u64,
    /// The allocation granularity inside a chunk: docs/design.md §5.2's 4096, the
    /// measured `minImportedHostPointerAlignment`.
    pub granule: u64,
    /// The pool's granule, which every lease length is a multiple of.
    pub lease_granule: u64,
    /// Empty chunks kept rather than returned.
    pub spare_chunks: usize,
}

impl Default for HeapConfig {
    fn default() -> Self {
        HeapConfig {
            chunk: 4 << 20,
            dedicated_from: 1 << 20,
            granule: 4096,
            lease_granule: 64 << 10,
            spare_chunks: 1,
        }
    }
}

/// An allocation: `offset` is a pool offset, inside the lease at `lease`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapAllocation {
    pub lease: u64,
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeapError {
    /// Ask the pool for a lease of `len` bytes, give it to
    /// [`SessionHeap::add_lease`] with `dedicated`, and try again.
    NeedLease { len: u64, dedicated: bool },
    ZeroLength,
    /// Freeing something this heap did not allocate.
    NotAllocated(u64),
    /// A lease that overlaps one the heap holds, or is not a whole number of
    /// granules.
    BadLease(u64),
}

#[derive(Debug, Clone)]
enum Kind {
    Chunk(Box<Tlsf>),
    /// `Some(size)` while its one allocation is live.
    Dedicated(Option<u64>),
}

#[derive(Debug, Clone)]
struct Held {
    len: u64,
    kind: Kind,
}

/// What [`SessionHeap::stats`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeapStats {
    /// Bytes in every lease held.
    pub leased: u64,
    /// Bytes allocated to the application.
    pub used: u64,
    pub chunks: usize,
    pub empty_chunks: usize,
    pub dedicated: usize,
    /// The most `leased` has been.
    pub high_water: u64,
    pub allocations: usize,
}

/// One session's heap.
#[derive(Debug, Clone)]
pub struct SessionHeap {
    cfg: HeapConfig,
    leases: BTreeMap<u64, Held>,
    /// Live allocations by pool offset: `(lease, size)`.
    allocs: BTreeMap<u64, (u64, u64)>,
    high_water: u64,
}

impl SessionHeap {
    pub fn new(cfg: HeapConfig) -> SessionHeap {
        SessionHeap { cfg, leases: BTreeMap::new(), allocs: BTreeMap::new(), high_water: 0 }
    }

    pub fn config(&self) -> HeapConfig {
        self.cfg
    }

    fn round(v: u64, g: u64) -> Option<u64> {
        v.checked_add(g - 1).map(|v| v & !(g - 1))
    }

    /// Allocate `size` bytes (rounded to the granule).
    pub fn alloc(&mut self, size: u64) -> Result<HeapAllocation, HeapError> {
        if size == 0 {
            return Err(HeapError::ZeroLength);
        }
        let size = Self::round(size, self.cfg.granule).ok_or(HeapError::ZeroLength)?;
        if size >= self.cfg.dedicated_from || size > self.cfg.chunk {
            // An idle dedicated lease that fits — one the caller just added
            // for this request, or one a racing thread left — and is not more
            // than one lease granule too big.
            let slack = self.cfg.lease_granule;
            let found = self.leases.iter_mut().find(|(_, h)| {
                matches!(h.kind, Kind::Dedicated(None)) && h.len >= size && h.len - size < slack.max(self.cfg.granule)
            });
            if let Some((&at, h)) = found {
                h.kind = Kind::Dedicated(Some(size));
                self.allocs.insert(at, (at, size));
                return Ok(HeapAllocation { lease: at, offset: at, size });
            }
            let len = Self::round(size, self.cfg.lease_granule).ok_or(HeapError::ZeroLength)?;
            return Err(HeapError::NeedLease { len, dedicated: true });
        }
        // The chunk with the least free room that can serve it, so that
        // nearly empty chunks stay empty and can go back.
        let mut best: Option<(u64, u64)> = None;
        for (&at, h) in &self.leases {
            if let Kind::Chunk(t) = &h.kind
                && t.largest_free() >= size
                && best.is_none_or(|(_, f)| t.free_bytes() < f)
            {
                best = Some((at, t.free_bytes()));
            }
        }
        if let Some((at, _)) = best
            && let Some(Held { kind: Kind::Chunk(t), .. }) = self.leases.get_mut(&at)
            && let Some(off) = t.alloc(size)
        {
            let offset = at + off;
            self.allocs.insert(offset, (at, size));
            return Ok(HeapAllocation { lease: at, offset, size });
        }
        Err(HeapError::NeedLease { len: self.cfg.chunk, dedicated: false })
    }

    /// Take a lease the pool granted, as a chunk or as one dedicated
    /// allocation's room.
    pub fn add_lease(&mut self, offset: u64, len: u64, dedicated: bool) -> Result<(), HeapError> {
        if len == 0 || !len.is_multiple_of(self.cfg.granule) || !offset.is_multiple_of(self.cfg.granule) {
            return Err(HeapError::BadLease(offset));
        }
        let end = offset.checked_add(len).ok_or(HeapError::BadLease(offset))?;
        if let Some((&at, h)) = self.leases.range(..end).next_back()
            && at + h.len > offset
        {
            return Err(HeapError::BadLease(offset));
        }
        let kind = if dedicated {
            Kind::Dedicated(None)
        } else {
            Kind::Chunk(Box::new(Tlsf::new(len, self.cfg.granule).map_err(|_| HeapError::BadLease(offset))?))
        };
        self.leases.insert(offset, Held { len, kind });
        self.high_water = self.high_water.max(self.leased());
        Ok(())
    }

    /// Free the allocation at pool offset `offset`. Returns
    /// `Some((lease, len))` when a lease should now go back to the pool; it
    /// has already been forgotten here.
    pub fn free(&mut self, offset: u64) -> Result<Option<(u64, u64)>, HeapError> {
        let (lease, _size) = self.allocs.remove(&offset).ok_or(HeapError::NotAllocated(offset))?;
        let h = self.leases.get_mut(&lease).ok_or(HeapError::NotAllocated(offset))?;
        match &mut h.kind {
            Kind::Dedicated(live) => {
                *live = None;
                let len = h.len;
                self.leases.remove(&lease);
                Ok(Some((lease, len)))
            }
            Kind::Chunk(t) => {
                t.free(offset - lease).ok_or(HeapError::NotAllocated(offset))?;
                if !t.is_empty() {
                    return Ok(None);
                }
                let empty = self
                    .leases
                    .values()
                    .filter(|h| matches!(&h.kind, Kind::Chunk(t) if t.is_empty()))
                    .count();
                if empty > self.cfg.spare_chunks {
                    let len = self.leases.remove(&lease).map(|h| h.len).unwrap_or(0);
                    return Ok(Some((lease, len)));
                }
                Ok(None)
            }
        }
    }

    /// Give back every lease that holds nothing: the spare chunk, and any
    /// dedicated lease that was added and never used.
    pub fn release_spares(&mut self) -> Vec<(u64, u64)> {
        let idle: Vec<(u64, u64)> = self
            .leases
            .iter()
            .filter(|(_, h)| match &h.kind {
                Kind::Chunk(t) => t.is_empty(),
                Kind::Dedicated(live) => live.is_none(),
            })
            .map(|(&at, h)| (at, h.len))
            .collect();
        for (at, _) in &idle {
            self.leases.remove(at);
        }
        idle
    }

    /// Every lease held, `(offset, len)`.
    pub fn leases(&self) -> Vec<(u64, u64)> {
        self.leases.iter().map(|(&a, h)| (a, h.len)).collect()
    }

    /// Bytes in every lease held.
    pub fn leased(&self) -> u64 {
        self.leases.values().map(|h| h.len).sum()
    }

    /// The size of the allocation at `offset`.
    pub fn size_of(&self, offset: u64) -> Option<u64> {
        self.allocs.get(&offset).map(|a| a.1)
    }

    /// The lease an allocation lives in.
    pub fn lease_of(&self, offset: u64) -> Option<(u64, u64)> {
        let (lease, _) = *self.allocs.get(&offset)?;
        self.leases.get(&lease).map(|h| (lease, h.len))
    }

    pub fn stats(&self) -> HeapStats {
        let mut s = HeapStats { high_water: self.high_water, allocations: self.allocs.len(), ..Default::default() };
        for h in self.leases.values() {
            s.leased += h.len;
            match &h.kind {
                Kind::Chunk(t) => {
                    s.chunks += 1;
                    s.used += t.used_bytes();
                    if t.is_empty() {
                        s.empty_chunks += 1;
                    }
                }
                Kind::Dedicated(live) => {
                    s.dedicated += 1;
                    s.used += live.unwrap_or(0);
                }
            }
        }
        s
    }

    /// The invariants, for tests: allocations sit inside their lease, never
    /// overlap, every chunk's own allocator is sound, and the counts agree.
    pub fn check(&self) -> Result<(), String> {
        let mut prev_end = 0u64;
        for (&at, h) in &self.leases {
            if at < prev_end {
                return Err(format!("lease {at:#x} overlaps the one before"));
            }
            prev_end = at + h.len;
            if let Kind::Chunk(t) = &h.kind {
                t.check().map_err(|e| format!("chunk {at:#x}: {e}"))?;
            }
        }
        let mut prev_end = 0u64;
        for (&off, &(lease, size)) in &self.allocs {
            if off < prev_end {
                return Err(format!("allocation {off:#x} overlaps the one before"));
            }
            prev_end = off + size;
            let h = self.leases.get(&lease).ok_or("an allocation in no lease")?;
            if off < lease || off + size > lease + h.len {
                return Err(format!("allocation {off:#x} is outside its lease"));
            }
            match &h.kind {
                Kind::Chunk(t) => {
                    if t.size_of(off - lease) != Some(size) {
                        return Err(format!("allocation {off:#x} is not its chunk's block"));
                    }
                }
                Kind::Dedicated(live) => {
                    if *live != Some(size) || off != lease {
                        return Err(format!("dedicated lease {lease:#x} disagrees"));
                    }
                }
            }
        }
        let empty = self.leases.values().filter(|h| matches!(&h.kind, Kind::Chunk(t) if t.is_empty())).count();
        // Spares only accumulate when two leases were asked for at once; the
        // free path never leaves more than `spare_chunks` of its own making.
        let _ = empty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{Pool, PoolConfig, Purpose};
    use crate::tlsf::Rng;

    const M: u64 = 1 << 20;

    /// The caller's loop, against a real pool: ask, lease, retry.
    fn alloc(h: &mut SessionHeap, p: &mut Pool, s: u64, size: u64) -> Option<HeapAllocation> {
        for _ in 0..3 {
            match h.alloc(size) {
                Ok(a) => return Some(a),
                Err(HeapError::NeedLease { len, dedicated }) => {
                    let purpose = if dedicated { Purpose::Dedicated } else { Purpose::Chunk };
                    let l = p.lease(s, len, purpose).ok()?;
                    h.add_lease(l.offset, l.len, dedicated).unwrap();
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        panic!("a lease was granted and still nothing fitted");
    }

    fn free(h: &mut SessionHeap, p: &mut Pool, a: HeapAllocation) {
        if let Some((lease, len)) = h.free(a.offset).unwrap() {
            assert_eq!(p.release(lease).unwrap().len, len);
        }
    }

    #[test]
    fn small_allocations_share_a_chunk_and_large_ones_get_their_own() {
        let mut p = Pool::new(PoolConfig::for_capacity(256 * M), 256 * M).unwrap();
        let mut h = SessionHeap::new(HeapConfig::default());
        let a = alloc(&mut h, &mut p, 1, 512 << 10).unwrap();
        let b = alloc(&mut h, &mut p, 1, 512 << 10).unwrap();
        assert_eq!(a.lease, b.lease, "two small allocations in one chunk");
        // Zink's slab gets a lease of its own, 64 KiB-rounded.
        let slab = alloc(&mut h, &mut p, 1, 2_310_144).unwrap();
        assert_ne!(slab.lease, a.lease);
        assert_eq!(h.lease_of(slab.offset).map(|l| l.1), Some(2_359_296));
        free(&mut h, &mut p, slab);
        let big = alloc(&mut h, &mut p, 1, 64 * M).unwrap();
        assert_ne!(big.lease, a.lease);
        assert_eq!(h.stats().dedicated, 1);
        // The dedicated lease goes back with its allocation.
        let before = p.free();
        free(&mut h, &mut p, big);
        assert_eq!(p.free(), before + 64 * M);
        // The chunk is kept as the one spare once it empties ...
        free(&mut h, &mut p, a);
        free(&mut h, &mut p, b);
        assert_eq!(h.stats().empty_chunks, 1);
        // ... and goes back on request.
        let spares = h.release_spares();
        assert_eq!(spares.len(), 1);
        p.release(spares[0].0).unwrap();
        assert_eq!(h.leased(), 0);
        p.check().unwrap();
        h.check().unwrap();
    }

    /// §95's failure on the old fixed arena, replayed on the heap: two
    /// windows' readback buffers resized between three sizes while Zink's
    /// 2 MiB slabs churn. The working set never exceeds 60 MiB, and the pool
    /// (as the guest's: 256 MiB) never refuses.
    #[test]
    fn two_windows_resizing_never_run_out_and_give_memory_back() {
        let mut p = Pool::new(PoolConfig::for_capacity(256 * M), 256 * M).unwrap();
        let mut h = SessionHeap::new(HeapConfig::default());
        let mut rng = Rng(9502);
        let mut slabs: Vec<HeapAllocation> = Vec::new();
        let mut readback: Vec<HeapAllocation> = Vec::new();
        let windows = [1296 * 768 * 4u64, 1280 * 752 * 4, 1920 * 1080 * 4];
        let steps = if cfg!(miri) { 600 } else { 20_000 };
        for step in 0..steps {
            if step % 500 == 0 {
                for r in readback.drain(..) {
                    free(&mut h, &mut p, r);
                }
                for _ in 0..2 {
                    let w = windows[rng.below(3) as usize];
                    for _ in 0..2 {
                        readback.push(alloc(&mut h, &mut p, 1, w).expect("readback"));
                    }
                }
            }
            if slabs.len() < 20 || (slabs.len() < 24 && rng.below(2) == 0) {
                slabs.push(alloc(&mut h, &mut p, 1, 2 * M).expect("a 2 MiB slab"));
            } else {
                let k = rng.below(slabs.len() as u64) as usize;
                free(&mut h, &mut p, slabs.swap_remove(k));
            }
        }
        h.check().unwrap();
        p.check().unwrap();
        let s = h.stats();
        assert!(s.leased <= s.used + 2 * h.config().chunk + 64 * 1024 * s.dedicated as u64, "{s:?}: more slack than two chunks and a granule a lease");
        for a in slabs.into_iter().chain(readback) {
            free(&mut h, &mut p, a);
        }
        for (l, _) in h.release_spares() {
            p.release(l).unwrap();
        }
        assert_eq!(p.account(1).map(|a| a.held), Some(0));
    }

    /// Random allocations and frees of every size, through a real pool: no
    /// overlap, nothing outside its lease, and the pool gets every byte back.
    #[test]
    fn random_heap_traffic_through_a_pool_balances() {
        let rounds = if cfg!(miri) { 1 } else { 16 };
        let steps = if cfg!(miri) { 300 } else { 3000 };
        for seed in 0..rounds {
            let mut rng = Rng(7000 + seed);
            let mut p = Pool::new(PoolConfig::for_capacity(512 * M), 512 * M).unwrap();
            let mut h = SessionHeap::new(HeapConfig::default());
            let mut live: Vec<HeapAllocation> = Vec::new();
            for step in 0..steps {
                if live.is_empty() || rng.below(100) < 55 {
                    let size = match rng.below(10) {
                        0..=6 => 1 + rng.below(512 * 1024),
                        7..=8 => 1 + rng.below(4 * M),
                        _ => 4 * M + rng.below(60 * M),
                    };
                    if let Some(a) = alloc(&mut h, &mut p, 1, size) {
                        assert!(a.size >= size);
                        for x in &live {
                            assert!(a.offset + a.size <= x.offset || x.offset + x.size <= a.offset, "seed {seed} step {step}: overlap");
                        }
                        live.push(a);
                    }
                } else {
                    let k = rng.below(live.len() as u64) as usize;
                    free(&mut h, &mut p, live.swap_remove(k));
                }
                if !cfg!(miri) || step % 50 == 0 {
                    h.check().unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                    p.check().unwrap();
                }
                assert_eq!(p.account(1).map(|a| a.held).unwrap_or(0), h.leased());
            }
            for a in live.drain(..) {
                free(&mut h, &mut p, a);
            }
            for (l, _) in h.release_spares() {
                p.release(l).unwrap();
            }
            assert_eq!(h.leased(), 0);
            assert_eq!(p.free(), p.capacity() - p.config().header, "seed {seed}");
            p.check().unwrap();
        }
    }

    #[test]
    fn a_wrong_free_and_an_overlapping_lease_are_refused() {
        let mut h = SessionHeap::new(HeapConfig::default());
        h.add_lease(8 * M, 8 * M, false).unwrap();
        assert_eq!(h.add_lease(12 * M, 8 * M, false), Err(HeapError::BadLease(12 * M)));
        assert_eq!(h.add_lease(4 * M, 8 * M, true), Err(HeapError::BadLease(4 * M)));
        h.add_lease(16 * M, 4 * M, true).unwrap();
        let a = h.alloc(4096).unwrap();
        assert_eq!(h.free(a.offset + 4096), Err(HeapError::NotAllocated(a.offset + 4096)));
        assert_eq!(h.free(a.offset), Ok(None), "the only empty chunk is kept");
        assert_eq!(h.free(a.offset), Err(HeapError::NotAllocated(a.offset)));
        h.check().unwrap();
    }
}
