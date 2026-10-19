//! §107: the ivshmem BAR cut into regions, one set per session, so that every
//! installation's proxy can share the one device the driver lets one process
//! map.
//!
//! Until §107 each host proxy's allocator owned the whole arena, `[0, len)`
//! less the compositor's outbound window: one compositor, one session, one
//! allocator. A compositor serving several installations has several proxies
//! on the same bytes, and two allocators handing out the same offsets is the
//! failure the outbound window was made to rule out (stage 10). So the
//! compositor, the one process that sees every session, owns the arena and
//! **grants regions** of it: whole granules, each with an arena id of its own,
//! each owned by exactly one session. A proxy allocates only inside the regions
//! it was granted, asks for another when it runs out (`grow` on its arena
//! channel), and gives an empty one back (`release`); a session that ends gives
//! back all of them. Nothing is fixed: no slots, no per-installation share of
//! the device, just a free list.
//!
//! This module is the bookkeeping and nothing else. It does not map, write or
//! tell anybody anything; `wc-compositor`'s session service drives it and
//! registers each grant as an arena id (`arena::attach_ivshmem_window`).
//!
//! # Bounds, for fault isolation
//!
//! A session may hold at most [`BarPool::cap`] bytes, three quarters of the
//! pool by default, so one installation filling its windows can never leave
//! another without room for its first region. Exhaustion is a refusal with
//! the numbers in it, never a panic and never a region taken from somebody
//! else. Region ids are never reused within a compositor's run, so a stale
//! descriptor naming a released region cannot come to name a new owner's one.

use std::collections::BTreeMap;

/// The granule regions are cut in: 1 MiB. Every ivshmem BAR is a whole number
/// of MiB, a region offset that is a multiple of this is page-aligned for the
/// host's `mmap` of the file at `HEADER_BYTES + offset`, and it is coarse
/// enough that the free list stays a handful of runs.
pub const GRANULE: u64 = 1 << 20;

/// The first region id a pool hands out. Arena 0 is the whole BAR as the
/// compositor registers it (it carries the outbound window); grants are named
/// from here up so that the two can never be confused in a log or on the wire.
pub const FIRST_ID: u32 = 0x100;

/// One region: `[offset, offset + len)` in **arena** offsets (relative to the
/// ivshmem header page), owned by `session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    pub id: u32,
    pub session: u8,
    pub offset: u64,
    pub len: u64,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The session would hold more than its cap.
    OverCap { session: u8, held: u64, asked: u64, cap: u64 },
    /// No free run is long enough.
    Exhausted { asked: u64, free: u64, largest: u64 },
    /// A region that is not this session's, or no region at all.
    NotYours { session: u8, id: u32 },
    /// Session 0 is "no session" and is never granted anything.
    NoSession,
    /// A zero-byte request.
    Empty,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::OverCap { session, held, asked, cap } => write!(
                f,
                "session {session} holds {held} bytes and asked for {asked} more; its cap is {cap} \
                 (the rest is kept for the other installations)"
            ),
            Refusal::Exhausted { asked, free, largest } => write!(
                f,
                "the shared arena has no {asked}-byte run: {free} bytes free, the largest run {largest}"
            ),
            Refusal::NotYours { session, id } => write!(f, "region {id:#x} is not session {session}'s"),
            Refusal::NoSession => write!(f, "session 0 is not a session"),
            Refusal::Empty => write!(f, "a region of no bytes"),
        }
    }
}

/// The compositor's view of the shared arena.
#[derive(Debug, Clone)]
pub struct BarPool {
    limit: u64,
    /// Sorted, coalesced `(offset, len)` runs.
    free: Vec<(u64, u64)>,
    grants: BTreeMap<u32, Grant>,
    next_id: u32,
    cap: u64,
}

impl BarPool {
    /// A pool over arena offsets `[0, limit)`: the arena less the outbound
    /// window, which the caller keeps out by passing its offset as `limit`.
    /// `limit` is rounded down to the granule.
    pub fn new(limit: u64) -> BarPool {
        let limit = limit / GRANULE * GRANULE;
        BarPool {
            limit,
            free: if limit == 0 { Vec::new() } else { vec![(0, limit)] },
            grants: BTreeMap::new(),
            next_id: FIRST_ID,
            cap: (limit / 4 * 3) / GRANULE * GRANULE,
        }
    }

    /// The bytes the pool covers.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Most bytes one session may hold at once.
    pub fn cap(&self) -> u64 {
        self.cap
    }

    /// Change the per-session cap (rounded down to the granule, at least one).
    pub fn set_cap(&mut self, cap: u64) {
        self.cap = (cap / GRANULE * GRANULE).max(GRANULE).min(self.limit);
    }

    /// Bytes `session` holds now.
    pub fn held(&self, session: u8) -> u64 {
        self.grants.values().filter(|g| g.session == session).map(|g| g.len).sum()
    }

    /// The regions `session` holds now, in id order.
    pub fn regions_of(&self, session: u8) -> Vec<Grant> {
        self.grants.values().filter(|g| g.session == session).copied().collect()
    }

    /// The region named `id`, whoever holds it.
    pub fn get(&self, id: u32) -> Option<Grant> {
        self.grants.get(&id).copied()
    }

    /// Free bytes, and the longest free run.
    pub fn free(&self) -> (u64, u64) {
        (self.free.iter().map(|r| r.1).sum(), self.free.iter().map(|r| r.1).max().unwrap_or(0))
    }

    /// How many regions are out.
    pub fn granted(&self) -> usize {
        self.grants.len()
    }

    /// A region of at least `min_len` bytes for `session`: first fit, rounded
    /// up to the granule. The region's id is new for this pool.
    pub fn grant(&mut self, session: u8, min_len: u64) -> Result<Grant, Refusal> {
        if session == 0 {
            return Err(Refusal::NoSession);
        }
        if min_len == 0 {
            return Err(Refusal::Empty);
        }
        let len = min_len.div_ceil(GRANULE).saturating_mul(GRANULE);
        let held = self.held(session);
        if held.saturating_add(len) > self.cap {
            return Err(Refusal::OverCap { session, held, asked: len, cap: self.cap });
        }
        let Some(i) = self.free.iter().position(|r| r.1 >= len) else {
            let (free, largest) = self.free();
            return Err(Refusal::Exhausted { asked: len, free, largest });
        };
        let (off, run) = self.free[i];
        if run == len {
            self.free.remove(i);
        } else {
            self.free[i] = (off + len, run - len);
        }
        let g = Grant { id: self.next_id, session, offset: off, len };
        self.next_id = self.next_id.checked_add(1).unwrap_or(FIRST_ID);
        self.grants.insert(g.id, g);
        Ok(g)
    }

    /// `session` gives region `id` back. Refused, and nothing changes, if the
    /// region is not that session's.
    pub fn release(&mut self, session: u8, id: u32) -> Result<Grant, Refusal> {
        match self.grants.get(&id) {
            Some(g) if g.session == session && session != 0 => {}
            _ => return Err(Refusal::NotYours { session, id }),
        }
        let g = self.grants.remove(&id).expect("checked above");
        self.put_back(g.offset, g.len);
        Ok(g)
    }

    /// The session ended: every region it held comes back. Returns them, so
    /// the caller can unregister their arena ids.
    pub fn reclaim(&mut self, session: u8) -> Vec<Grant> {
        let gone: Vec<Grant> = self.regions_of(session);
        for g in &gone {
            self.grants.remove(&g.id);
            self.put_back(g.offset, g.len);
        }
        gone
    }

    fn put_back(&mut self, off: u64, len: u64) {
        let i = self.free.partition_point(|r| r.0 < off);
        self.free.insert(i, (off, len));
        // Coalesce with the neighbours.
        if i + 1 < self.free.len() && self.free[i].0 + self.free[i].1 == self.free[i + 1].0 {
            self.free[i].1 += self.free[i + 1].1;
            self.free.remove(i + 1);
        }
        if i > 0 && self.free[i - 1].0 + self.free[i - 1].1 == self.free[i].0 {
            self.free[i - 1].1 += self.free[i].1;
            self.free.remove(i);
        }
    }

    /// Every byte is accounted for exactly once: free runs and grants tile
    /// `[0, limit)` with no overlap. For tests and for a debug assertion.
    pub fn consistent(&self) -> bool {
        let mut runs: Vec<(u64, u64)> = self.free.clone();
        runs.extend(self.grants.values().map(|g| (g.offset, g.len)));
        runs.sort();
        let mut at = 0;
        for (o, l) in runs {
            if o != at || l == 0 {
                return false;
            }
            at = o + l;
        }
        at == self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn grants_are_whole_granules_with_new_ids_and_never_overlap() {
        let mut p = BarPool::new(100 * MIB + 12345);
        assert_eq!(p.limit(), 100 * MIB, "rounded down to the granule");
        let a = p.grant(1, 16 * MIB).unwrap();
        let b = p.grant(2, 1).unwrap();
        let c = p.grant(1, 3 * MIB + 1).unwrap();
        assert_eq!((a.offset, a.len), (0, 16 * MIB));
        assert_eq!((b.offset, b.len), (16 * MIB, MIB), "one byte is one granule");
        assert_eq!(c.len, 4 * MIB);
        assert!(a.id >= FIRST_ID && b.id > a.id && c.id > b.id);
        assert_eq!(p.held(1), 20 * MIB);
        assert_eq!(p.regions_of(1).len(), 2);
        assert!(p.consistent());
    }

    #[test]
    fn a_session_grows_and_releases_only_its_own_regions() {
        let mut p = BarPool::new(64 * MIB);
        let a = p.grant(1, 8 * MIB).unwrap();
        let b = p.grant(2, 8 * MIB).unwrap();
        // Another session's region is refused, and nothing moves.
        assert_eq!(p.release(1, b.id), Err(Refusal::NotYours { session: 1, id: b.id }));
        assert_eq!(p.release(0, a.id), Err(Refusal::NotYours { session: 0, id: a.id }));
        assert_eq!(p.release(1, 0xdead), Err(Refusal::NotYours { session: 1, id: 0xdead }));
        assert_eq!(p.held(2), 8 * MIB);
        // Its own comes back and coalesces.
        assert_eq!(p.release(1, a.id).unwrap(), a);
        assert_eq!(p.release(1, a.id), Err(Refusal::NotYours { session: 1, id: a.id }), "twice is refused");
        assert!(p.consistent());
        assert_eq!(p.free(), (56 * MIB, 48 * MIB));
    }

    #[test]
    fn a_session_that_ends_gives_back_everything_and_another_installation_reuses_it() {
        let mut p = BarPool::new(64 * MIB);
        let a1 = p.grant(1, 16 * MIB).unwrap();
        let _b = p.grant(2, 8 * MIB).unwrap();
        let a2 = p.grant(1, 16 * MIB).unwrap();
        let gone = p.reclaim(1);
        assert_eq!(gone, vec![a1, a2]);
        assert_eq!(p.held(1), 0);
        assert!(p.consistent());
        // Installation 3 gets the bytes installation 1 had, under a new id.
        let c = p.grant(3, 16 * MIB).unwrap();
        assert_eq!(c.offset, a1.offset, "first fit lands in the freed run");
        assert!(c.id > a2.id, "ids are never reused, so a stale descriptor names nothing");
        assert!(p.get(a1.id).is_none());
        assert_eq!(p.reclaim(1), vec![], "reclaiming twice is nothing");
    }

    #[test]
    fn exhaustion_and_the_cap_refuse_cleanly_and_take_nothing_from_anyone() {
        let mut p = BarPool::new(32 * MIB);
        assert_eq!(p.cap(), 24 * MIB, "three quarters by default");
        let a = p.grant(1, 20 * MIB).unwrap();
        // Over the cap: refused, though the bytes exist.
        match p.grant(1, 8 * MIB) {
            Err(Refusal::OverCap { session: 1, held, asked, cap }) => {
                assert_eq!((held, asked, cap), (20 * MIB, 8 * MIB, 24 * MIB))
            }
            other => panic!("{other:?}"),
        }
        // Another installation still has room for its first region.
        let b = p.grant(2, 8 * MIB).unwrap();
        // The arena is full: a third is refused with the numbers.
        let e = p.grant(3, 8 * MIB).unwrap_err();
        assert_eq!(e, Refusal::Exhausted { asked: 8 * MIB, free: 4 * MIB, largest: 4 * MIB });
        assert!(e.to_string().contains("no 8388608-byte run"));
        // Nobody lost anything.
        assert_eq!(p.get(a.id), Some(a));
        assert_eq!(p.get(b.id), Some(b));
        assert!(p.consistent());
        // A small one still fits.
        assert!(p.grant(3, 4 * MIB).is_ok());
        assert_eq!(p.grant(3, 1), Err(Refusal::Exhausted { asked: MIB, free: 0, largest: 0 }));
        assert_eq!(p.grant(0, MIB), Err(Refusal::NoSession));
        assert_eq!(p.grant(4, 0), Err(Refusal::Empty));
    }

    #[test]
    fn fragmented_frees_coalesce_back_into_one_run() {
        let mut p = BarPool::new(16 * MIB);
        p.set_cap(16 * MIB);
        let g: Vec<Grant> = (0..16).map(|i| p.grant(1 + (i % 3) as u8, MIB).unwrap()).collect();
        assert_eq!(p.free(), (0, 0));
        // Free every other one, then the rest, in a scrambled order.
        for x in g.iter().step_by(2) {
            p.release(x.session, x.id).unwrap();
            assert!(p.consistent());
        }
        assert_eq!(p.free(), (8 * MIB, MIB));
        for s in [2u8, 1, 3] {
            p.reclaim(s);
            assert!(p.consistent());
        }
        assert_eq!(p.free(), (16 * MIB, 16 * MIB), "one run again");
        assert_eq!(p.granted(), 0);
    }

    #[test]
    fn an_empty_pool_refuses_and_the_cap_is_bounded() {
        let mut p = BarPool::new(GRANULE - 1);
        assert_eq!(p.limit(), 0);
        assert!(matches!(p.grant(1, 1), Err(Refusal::OverCap { .. } | Refusal::Exhausted { .. })));
        let mut q = BarPool::new(8 * MIB);
        q.set_cap(1 << 40);
        assert_eq!(q.cap(), 8 * MIB);
        q.set_cap(0);
        assert_eq!(q.cap(), MIB);
    }
}
