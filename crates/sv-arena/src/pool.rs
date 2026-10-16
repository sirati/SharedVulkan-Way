//! The GPU path's shared memory as one pool, leased to sessions on demand
//! (waycompose-design.md §97).
//!
//! # What this replaces
//!
//! §87 cut the 256 MiB ivshmem device into three fixed 80 MiB slots, one per
//! session; §95 found Firefox's two-window working set (53 MiB) filling a
//! 56 MiB share and fragmenting it, and §96 found VulkanMod wanting more than
//! 240 MiB of mappable memory in one session. Fixed equal slots cannot serve
//! both: the slot that fits Minecraft starves Firefox's second session, and
//! the slots that fit Firefox's three cannot hold Minecraft at all.
//!
//! So the device is a **pool**. One owner — the guest's GPU process — leases
//! ranges of it to sessions as they ask and takes them back as they release
//! them; a session holds exactly what it uses, plus a little slack, and what
//! it gives back is another session's at once.
//!
//! # Addresses
//!
//! The pool may span several devices: the boot device is region 0, and
//! regions hot-plugged at run time are 1, 2, … ([`MAX_REGIONS`]). A **pool
//! offset** names a byte in any of them: `region << REGION_SHIFT | offset`.
//! Every lease, every wire offset and every app-side pointer is computed from
//! one, so a region added later needs no renumbering of anything that exists.
//! 4 GiB a region is far more than any ivshmem device here and costs only
//! address space, which the app side reserves and never commits.
//!
//! # What the pool does not do
//!
//! It never touches the memory. Mapping, importing into a driver and stamping
//! are the caller's; this type is bookkeeping in private memory, as design.md
//! §5.2 asks of every allocator in the project, and is checked by Miri against
//! nothing but itself.

use crate::tlsf::Tlsf;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Bits of a pool offset that are the offset inside a region.
pub const REGION_SHIFT: u32 = 32;
/// The largest region, and the stride between regions in the pool's address
/// space.
pub const REGION_STRIDE: u64 = 1 << REGION_SHIFT;
/// Regions a pool can have: the GPU device and up to seven more, each its own
/// device configured beside it.
pub const MAX_REGIONS: usize = 8;

/// The command-blob reserve a session leases when it starts. §0.28 measured
/// the reserve's working set in Firefox's soaks at 2–3 MiB; 8 MiB is §87's
/// reserve, and a fuller one makes recording wait for the GPU side to read,
/// never fail.
pub const BLOB_LEASE: u64 = 8 << 20;

/// A pool offset from a region and an offset in it.
pub const fn pool_offset(region: u32, offset: u64) -> u64 {
    ((region as u64) << REGION_SHIFT) | offset
}

/// The region and the offset in it of a pool offset.
pub const fn split(pool_offset: u64) -> (u32, u64) {
    ((pool_offset >> REGION_SHIFT) as u32, pool_offset & (REGION_STRIDE - 1))
}

/// What a lease is for. Only accounting and policy read it: a session's
/// control region is not counted against its cap, and only the two leases a
/// session starts with — its control region and its command-blob reserve —
/// may use the reserve kept in region 0 for sessions that have not started
/// yet ([`Purpose::starts_a_session`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Purpose {
    /// The session's rings and directories.
    Control,
    /// The command-blob reserve.
    Blobs,
    /// A chunk the app side sub-allocates small allocations from.
    Chunk,
    /// One allocation too big to share a chunk.
    Dedicated,
}

impl Purpose {
    /// One of the leases a session cannot start without. Measured (§97): with
    /// only the control region allowed into the reserve, a second process
    /// started beside one that had filled the pool got its rings and was then
    /// refused its blob reserve, and its ICD refused to load.
    pub fn starts_a_session(self) -> bool {
        matches!(self, Purpose::Control | Purpose::Blobs)
    }

    pub fn from_u32(v: u32) -> Option<Purpose> {
        Some(match v {
            0 => Purpose::Control,
            1 => Purpose::Blobs,
            2 => Purpose::Chunk,
            3 => Purpose::Dedicated,
            _ => return None,
        })
    }
    pub fn as_u32(self) -> u32 {
        self as u32
    }
    fn name(self) -> &'static str {
        match self {
            Purpose::Control => "control",
            Purpose::Blobs => "blobs",
            Purpose::Chunk => "chunk",
            Purpose::Dedicated => "dedicated",
        }
    }
}

/// One leased range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    /// Pool offset of the first byte.
    pub offset: u64,
    pub len: u64,
    pub session: u64,
    pub purpose: Purpose,
}

impl Lease {
    pub fn region(&self) -> u32 {
        split(self.offset).0
    }
}

/// The pool's policy, fixed when it is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    /// The pool's allocation unit, a power of two. Every lease is a multiple
    /// of it and aligned to it. 64 KiB: the GPU's large page, and a thousand
    /// times fewer blocks than 4 KiB would make of a 256 MiB device.
    pub granule: u64,
    /// Bytes at the start of region 0 that are never leased: the pool header
    /// the guest and the host's growth service talk through.
    pub header: u64,
    /// Bytes of region 0 only a session's start may use (a control region
    /// and a blob reserve): the room a session that has not started yet needs
    /// to start at all. Without it one
    /// application that filled the pool would leave Firefox's next process —
    /// `glxtest`, then the browser — with no rings to attach to.
    pub reserve: u64,
    /// The most a session may hold, control region excepted: three quarters
    /// of the pool. One application may use most of it, never the last
    /// quarter, which is what the applications beside it work in. Measured
    /// (§97): with the cap at the pool less 48 MiB, a second process beside a
    /// full one started and could then allocate nothing at all.
    pub session_cap: u64,
}

impl PoolConfig {
    /// The defaults of §97, for a pool that can grow to `max_capacity` bytes.
    pub fn for_capacity(max_capacity: u64) -> PoolConfig {
        let reserve = 24 << 20;
        PoolConfig {
            granule: 64 << 10,
            header: 64 << 10,
            reserve,
            session_cap: (max_capacity / 4 * 3).max(reserve),
        }
    }
}

/// Why a lease was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolError {
    /// The session holds `held` and asked for `asked` more; it may hold `cap`.
    /// Growth would not help: this is the session's limit, not the pool's.
    OverQuota { session: u64, held: u64, asked: u64, cap: u64 },
    /// No region has a free block this large that it may give. `largest_free`
    /// is the largest one any region has, `free` the total, and `reserved`
    /// the room kept for a session that has not started, which this lease
    /// could not use. Growth would help.
    NoSpace { asked: u64, largest_free: u64, free: u64, reserved: u64 },
    /// A zero-length request, or one that no region could ever hold.
    BadSize(u64),
    /// No lease starts at this offset.
    NotLeased(u64),
    /// The region exists already, is out of range, is too large or still
    /// holds leases.
    Region(&'static str),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::OverQuota { session, held, asked, cap } => write!(
                f,
                "session {session} holds {} KiB and asked for {} KiB more; its cap is {} KiB",
                held >> 10,
                asked >> 10,
                cap >> 10
            ),
            PoolError::NoSpace { asked, largest_free, free, reserved } if *largest_free >= *asked => write!(
                f,
                "{} KiB is free ({} KiB in one block), but {} KiB of it is kept for a session that has not \
                 started, and the rest is less than the {} KiB asked for",
                free >> 10,
                largest_free >> 10,
                reserved >> 10,
                asked >> 10
            ),
            PoolError::NoSpace { asked, largest_free, free, .. } => write!(
                f,
                "no region has {} KiB free in one block (largest {} KiB, {} KiB free in all)",
                asked >> 10,
                largest_free >> 10,
                free >> 10
            ),
            PoolError::BadSize(n) => write!(f, "a lease of {n} bytes"),
            PoolError::NotLeased(o) => write!(f, "nothing is leased at {o:#x}"),
            PoolError::Region(why) => write!(f, "region: {why}"),
        }
    }
}

impl std::error::Error for PoolError {}

#[derive(Debug, Clone)]
struct Region {
    tlsf: Tlsf,
    leases: u32,
    /// No new leases: the region is on its way out.
    draining: bool,
}

/// What one session holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    /// Bytes held, control region included.
    pub held: u64,
    /// Bytes held that count against the cap.
    pub counted: u64,
    /// The most `held` has been.
    pub high_water: u64,
    pub leases: u32,
    /// Leases refused, for the log.
    pub refused: u32,
}

/// The pool: every region, every session's account and every lease.
#[derive(Debug, Clone)]
pub struct Pool {
    cfg: PoolConfig,
    regions: Vec<Option<Region>>,
    sessions: BTreeMap<u64, Account>,
    leases: BTreeMap<u64, Lease>,
}

impl Pool {
    /// A pool of one region, region 0, of `len` bytes.
    pub fn new(cfg: PoolConfig, len: u64) -> Result<Pool, PoolError> {
        let mut p = Pool { cfg, regions: vec![None; MAX_REGIONS], sessions: BTreeMap::new(), leases: BTreeMap::new() };
        p.add_region(0, len)?;
        Ok(p)
    }

    pub fn config(&self) -> PoolConfig {
        self.cfg
    }

    /// Add region `id` of `len` bytes, all free but its first `header`.
    pub fn add_region(&mut self, id: u32, len: u64) -> Result<(), PoolError> {
        let slot = self.regions.get_mut(id as usize).ok_or(PoolError::Region("id out of range"))?;
        if slot.is_some() {
            return Err(PoolError::Region("exists already"));
        }
        if len > REGION_STRIDE {
            return Err(PoolError::Region("larger than the stride"));
        }
        let mut tlsf = Tlsf::new(len, self.cfg.granule).map_err(PoolError::Region)?;
        if self.cfg.header > 0 {
            let at = tlsf.alloc(self.cfg.header).ok_or(PoolError::Region("the header does not fit"))?;
            debug_assert_eq!(at, 0);
        }
        *slot = Some(Region { tlsf, leases: 0, draining: false });
        Ok(())
    }

    /// Stop leasing from region `id`, so that it empties. Undone by
    /// [`Pool::undrain`].
    pub fn drain(&mut self, id: u32) {
        if let Some(Some(r)) = self.regions.get_mut(id as usize) {
            r.draining = true;
        }
    }

    pub fn undrain(&mut self, id: u32) {
        if let Some(Some(r)) = self.regions.get_mut(id as usize) {
            r.draining = false;
        }
    }

    /// Remove region `id`, which must hold no lease and must not be region 0.
    /// Returns its length.
    pub fn remove_region(&mut self, id: u32) -> Result<u64, PoolError> {
        if id == 0 {
            return Err(PoolError::Region("region 0 is the boot device"));
        }
        let slot = self.regions.get_mut(id as usize).ok_or(PoolError::Region("id out of range"))?;
        match slot {
            None => Err(PoolError::Region("no such region")),
            Some(r) if r.leases > 0 => Err(PoolError::Region("still holds leases")),
            Some(r) => {
                let len = r.tlsf.len();
                *slot = None;
                Ok(len)
            }
        }
    }

    /// Regions other than 0 that hold no lease: what the growth service may
    /// take back.
    pub fn empty_regions(&self) -> Vec<u32> {
        self.regions
            .iter()
            .enumerate()
            .skip(1)
            .filter_map(|(i, r)| r.as_ref().filter(|r| r.leases == 0).map(|_| i as u32))
            .collect()
    }

    /// `(id, len)` of every region.
    pub fn regions(&self) -> Vec<(u32, u64)> {
        self.regions
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.as_ref().map(|r| (i as u32, r.tlsf.len())))
            .collect()
    }

    /// The lowest region id not in use, for the next region to be added.
    pub fn next_region_id(&self) -> Option<u32> {
        self.regions.iter().position(|r| r.is_none()).map(|i| i as u32)
    }

    /// Bytes in every region.
    pub fn capacity(&self) -> u64 {
        self.regions.iter().flatten().map(|r| r.tlsf.len()).sum()
    }

    /// Bytes free in every region.
    pub fn free(&self) -> u64 {
        self.regions.iter().flatten().map(|r| r.tlsf.free_bytes()).sum()
    }

    /// The largest free block in any region.
    pub fn largest_free(&self) -> u64 {
        self.regions.iter().flatten().map(|r| r.tlsf.largest_free()).max().unwrap_or(0)
    }

    /// Open an account for `session`, named for the log.
    pub fn open_session(&mut self, session: u64, name: &str) {
        self.sessions.entry(session).or_default().name = name.to_string();
    }

    pub fn set_name(&mut self, session: u64, name: &str) {
        if let Some(a) = self.sessions.get_mut(&session) {
            a.name = name.to_string();
        }
    }

    pub fn account(&self, session: u64) -> Option<&Account> {
        self.sessions.get(&session)
    }

    /// Lease `len` bytes (rounded up to the granule) to `session`.
    ///
    /// Regions are tried lowest first, so that the regions added last empty
    /// first and can be given back. In region 0 only a session's start may
    /// take the last [`PoolConfig::reserve`] bytes.
    pub fn lease(&mut self, session: u64, len: u64, purpose: Purpose) -> Result<Lease, PoolError> {
        let g = self.cfg.granule;
        let len = match len.checked_add(g - 1) {
            Some(l) if len > 0 && l & !(g - 1) <= REGION_STRIDE => l & !(g - 1),
            _ => return Err(PoolError::BadSize(len)),
        };
        let counted = purpose != Purpose::Control;
        {
            let a = self.sessions.entry(session).or_default();
            if counted && a.counted.saturating_add(len) > self.cfg.session_cap {
                a.refused += 1;
                return Err(PoolError::OverQuota { session, held: a.counted, asked: len, cap: self.cfg.session_cap });
            }
        }
        let reserve = if purpose.starts_a_session() { 0 } else { self.cfg.reserve };
        let mut got = None;
        for (id, r) in self.regions.iter_mut().enumerate() {
            let Some(r) = r.as_mut() else { continue };
            if r.draining {
                continue;
            }
            if id == 0 && r.tlsf.free_bytes() < len.saturating_add(reserve) {
                continue;
            }
            let Some(off) = r.tlsf.alloc(len) else { continue };
            if id == 0 && reserve > 0 && r.tlsf.largest_free() < reserve.min(r.tlsf.free_bytes()) {
                // It fitted, but it split the room a new session needs.
                r.tlsf.free(off);
                continue;
            }
            r.leases += 1;
            got = Some(pool_offset(id as u32, off));
            break;
        }
        let Some(offset) = got else {
            if let Some(a) = self.sessions.get_mut(&session) {
                a.refused += 1;
            }
            return Err(PoolError::NoSpace {
                asked: len,
                largest_free: self.largest_free(),
                free: self.free(),
                reserved: reserve,
            });
        };
        let lease = Lease { offset, len, session, purpose };
        self.leases.insert(offset, lease);
        let a = self.sessions.entry(session).or_default();
        a.held += len;
        if counted {
            a.counted += len;
        }
        a.leases += 1;
        a.high_water = a.high_water.max(a.held);
        Ok(lease)
    }

    /// Give back the lease at `offset`.
    pub fn release(&mut self, offset: u64) -> Result<Lease, PoolError> {
        let lease = self.leases.remove(&offset).ok_or(PoolError::NotLeased(offset))?;
        let (id, local) = split(offset);
        let r = self.regions.get_mut(id as usize).and_then(|r| r.as_mut()).ok_or(PoolError::NotLeased(offset))?;
        let freed = r.tlsf.free(local);
        debug_assert_eq!(freed, Some(lease.len));
        r.leases -= 1;
        if let Some(a) = self.sessions.get_mut(&lease.session) {
            a.held -= lease.len;
            if lease.purpose != Purpose::Control {
                a.counted -= lease.len;
            }
            a.leases -= 1;
        }
        Ok(lease)
    }

    /// The lease at `offset`, if one starts there.
    pub fn lease_at(&self, offset: u64) -> Option<Lease> {
        self.leases.get(&offset).copied()
    }

    /// Every lease `session` holds, lowest offset first.
    pub fn leases_of(&self, session: u64) -> Vec<Lease> {
        self.leases.values().filter(|l| l.session == session).copied().collect()
    }

    /// Close `session`'s account and give back everything it still holds.
    /// Returns what was given back.
    pub fn close_session(&mut self, session: u64) -> Vec<Lease> {
        let held = self.leases_of(session);
        for l in &held {
            let _ = self.release(l.offset);
        }
        self.sessions.remove(&session);
        held
    }

    /// Keep `session`'s leases for ever: its driver may still use them
    /// (waycompose-design.md §87's quarantine). The account is renamed so the
    /// log says why the memory is gone.
    pub fn quarantine(&mut self, session: u64) {
        if let Some(a) = self.sessions.get_mut(&session)
            && !a.name.ends_with(" (quarantined)")
        {
            a.name.push_str(" (quarantined)");
        }
    }

    /// One line for the log: every region, and every session's holding.
    pub fn report(&self) -> String {
        let mut s = String::new();
        let mi = |b: u64| b as f64 / (1 << 20) as f64;
        let _ = write!(s, "pool {:.1}/{:.1} MiB leased", mi(self.capacity() - self.free()), mi(self.capacity()));
        for (id, r) in self.regions.iter().enumerate() {
            let Some(r) = r else { continue };
            let st = r.tlsf.stats();
            let _ = write!(
                s,
                "; region {id}{} {:.1}/{:.1} MiB in {} lease(s), largest free {:.1} MiB in {} free block(s)",
                if r.draining { " (draining)" } else { "" },
                mi(st.used),
                mi(st.len),
                r.leases,
                mi(st.largest_free),
                st.free_blocks
            );
        }
        for (id, a) in &self.sessions {
            let mut by: BTreeMap<Purpose, (u32, u64)> = BTreeMap::new();
            for l in self.leases.values().filter(|l| l.session == *id) {
                let e = by.entry(l.purpose).or_default();
                e.0 += 1;
                e.1 += l.len;
            }
            let parts: Vec<String> =
                by.iter().map(|(p, (n, b))| format!("{n} {} {:.1}", p.name(), mi(*b))).collect();
            let _ = write!(
                s,
                "; session {id} {} {:.1} MiB (high {:.1}, cap {:.0}{}) [{}]",
                if a.name.is_empty() { "?" } else { &a.name },
                mi(a.held),
                mi(a.high_water),
                mi(self.cfg.session_cap),
                if a.refused > 0 { format!(", {} refused", a.refused) } else { String::new() },
                parts.join(", ")
            );
        }
        s
    }

    /// Check every region's allocator and that the lease table, the region
    /// counts and the accounts agree. For tests.
    pub fn check(&self) -> Result<(), String> {
        let mut per_region = [0u32; MAX_REGIONS];
        let mut per_session: BTreeMap<u64, (u64, u64, u32)> = BTreeMap::new();
        for (off, l) in &self.leases {
            if *off != l.offset {
                return Err("a lease is filed under the wrong offset".into());
            }
            let (id, local) = split(l.offset);
            let r = self.regions.get(id as usize).and_then(|r| r.as_ref()).ok_or("a lease in no region")?;
            if r.tlsf.size_of(local) != Some(l.len) {
                return Err(format!("the lease at {off:#x} is not the region's block"));
            }
            per_region[id as usize] += 1;
            let e = per_session.entry(l.session).or_default();
            e.0 += l.len;
            if l.purpose != Purpose::Control {
                e.1 += l.len;
            }
            e.2 += 1;
        }
        for (id, r) in self.regions.iter().enumerate() {
            let Some(r) = r else { continue };
            r.tlsf.check().map_err(|e| format!("region {id}: {e}"))?;
            if r.leases != per_region[id] {
                return Err(format!("region {id} counts {} leases, the table {}", r.leases, per_region[id]));
            }
            let header = usize::from(self.cfg.header > 0);
            if r.tlsf.stats().used_blocks != per_region[id] as usize + header {
                return Err(format!("region {id} has blocks no lease names"));
            }
        }
        for (id, a) in &self.sessions {
            let (held, counted, n) = per_session.get(id).copied().unwrap_or_default();
            if (a.held, a.counted, a.leases) != (held, counted, n) {
                return Err(format!("session {id}'s account is wrong"));
            }
            if a.counted > self.cfg.session_cap {
                return Err(format!("session {id} is over its cap"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlsf::Rng;

    const M: u64 = 1 << 20;

    fn testhost() -> Pool {
        Pool::new(PoolConfig::for_capacity(768 * M), 256 * M).unwrap()
    }

    #[test]
    fn offsets_carry_the_region() {
        assert_eq!(split(pool_offset(3, 0x1234_0000)), (3, 0x1234_0000));
        assert_eq!(pool_offset(1, 0), REGION_STRIDE);
    }

    /// The case §87's slots could not serve: Firefox's three sessions and
    /// Minecraft's 300 MiB, at once, once the pool has grown by one region.
    #[test]
    fn firefox_and_minecraft_fit_when_the_total_does() {
        let mut p = testhost();
        let ctl = 16 * M;
        for s in 1..=3 {
            p.open_session(s, "firefox");
            p.lease(s, ctl, Purpose::Control).unwrap();
            p.lease(s, 8 * M, Purpose::Blobs).unwrap();
        }
        // The browser's 53 MiB working set, in chunks.
        for _ in 0..7 {
            p.lease(2, 8 * M, Purpose::Chunk).unwrap();
        }
        p.open_session(9, "java");
        p.lease(9, ctl, Purpose::Control).unwrap();
        p.lease(9, 8 * M, Purpose::Blobs).unwrap();
        // VulkanMod's two 64 MiB staging buffers fit in region 0 ...
        p.lease(9, 64 * M, Purpose::Dedicated).unwrap();
        let e = p.lease(9, 64 * M, Purpose::Dedicated).unwrap_err();
        assert!(matches!(e, PoolError::NoSpace { .. }), "{e}");
        // ... and the second one needs region 1.
        p.add_region(1, 256 * M).unwrap();
        let l = p.lease(9, 64 * M, Purpose::Dedicated).unwrap();
        assert_eq!(l.region(), 1);
        for _ in 0..12 {
            p.lease(9, 8 * M, Purpose::Chunk).unwrap();
        }
        p.check().unwrap();
        println!("{}", p.report());
    }

    #[test]
    fn a_session_cannot_take_the_reserve_or_more_than_its_cap() {
        let mut p = Pool::new(PoolConfig { session_cap: 200 * M, ..PoolConfig::for_capacity(256 * M) }, 256 * M).unwrap();
        p.open_session(1, "greedy");
        let mut held = 0;
        while let Ok(l) = p.lease(1, 8 * M, Purpose::Chunk) {
            held += l.len;
        }
        assert!(held <= 200 * M);
        let e = p.lease(1, 8 * M, Purpose::Chunk).unwrap_err();
        assert!(matches!(e, PoolError::OverQuota { .. }), "{e}");
        // A new session still starts: its control region and its blob
        // reserve fit in the reserve.
        p.open_session(2, "next");
        p.lease(2, 16 * M, Purpose::Control).unwrap();
        p.lease(2, 8 * M, Purpose::Blobs).unwrap();
        p.check().unwrap();

        // Without the cap, the reserve alone keeps the start possible.
        let mut p = Pool::new(PoolConfig { session_cap: u64::MAX, ..PoolConfig::for_capacity(256 * M) }, 256 * M).unwrap();
        while p.lease(1, 2 * M, Purpose::Chunk).is_ok() {}
        assert!(p.free() >= p.config().reserve);
        // What refuses the next ordinary lease is the reserve, and the refusal
        // says so (measured on the guest: the first wording said no block was
        // large enough beside a free 17.6 MiB one).
        let e = p.lease(3, 2 * M, Purpose::Chunk).unwrap_err();
        assert!(e.to_string().contains("kept for a session that has not started"), "{e}");
        p.lease(2, 16 * M, Purpose::Control).unwrap();
        p.lease(2, 8 * M, Purpose::Blobs).unwrap();
        // The reserve is spent on that start; nothing ordinary gets it.
        assert!(matches!(p.lease(2, 2 * M, Purpose::Chunk), Err(PoolError::NoSpace { .. })));
        p.check().unwrap();
    }

    #[test]
    fn closing_a_session_gives_everything_back_and_regions_empty_lowest_last() {
        let mut p = testhost();
        p.add_region(1, 128 * M).unwrap();
        p.open_session(1, "a");
        let a: Vec<Lease> = (0..20).map(|_| p.lease(1, 8 * M, Purpose::Chunk).unwrap()).collect();
        assert!(a.iter().all(|l| l.region() == 0), "region 0 first");
        p.lease(1, 100 * M, Purpose::Dedicated).unwrap();
        assert_eq!(p.empty_regions(), Vec::<u32>::new());
        let back = p.close_session(1);
        assert_eq!(back.len(), 21);
        assert_eq!(p.empty_regions(), vec![1]);
        assert_eq!(p.free(), p.capacity() - 2 * p.config().header);
        assert_eq!(p.remove_region(1), Ok(128 * M));
        assert!(p.remove_region(0).is_err());
        p.check().unwrap();
    }

    #[test]
    fn a_draining_region_takes_no_new_lease_and_cannot_be_removed_while_leased() {
        let mut p = testhost();
        p.add_region(1, 64 * M).unwrap();
        p.drain(0);
        let l = p.lease(1, 8 * M, Purpose::Chunk).unwrap();
        assert_eq!(l.region(), 1);
        p.undrain(0);
        p.drain(1);
        assert_eq!(p.lease(1, 8 * M, Purpose::Chunk).unwrap().region(), 0);
        assert!(p.remove_region(1).is_err());
        p.release(l.offset).unwrap();
        assert_eq!(p.remove_region(1), Ok(64 * M));
        assert_eq!(p.release(l.offset), Err(PoolError::NotLeased(l.offset)));
        p.check().unwrap();
    }

    /// Random sessions, leases, releases, region additions and removals:
    /// leases never overlap, the accounts always balance, nothing leaks, and
    /// once every session closes each region is one free block again.
    #[test]
    fn random_pool_traffic_never_overlaps_or_leaks() {
        let rounds = if cfg!(miri) { 1 } else { 20 };
        let steps = if cfg!(miri) { 250 } else { 3000 };
        for seed in 0..rounds {
            let mut rng = Rng(1000 + seed);
            let mut p = Pool::new(PoolConfig::for_capacity(1024 * M), 256 * M).unwrap();
            let mut live: Vec<Lease> = Vec::new();
            for step in 0..steps {
                match rng.below(100) {
                    0..=54 => {
                        let s = 1 + rng.below(5);
                        let (len, purpose) = match rng.below(10) {
                            0 => (16 * M, Purpose::Control),
                            1..=6 => (8 * M, Purpose::Chunk),
                            _ => (1 + rng.below(96 * M), Purpose::Dedicated),
                        };
                        if let Ok(l) = p.lease(s, len, purpose) {
                            let (r, o) = split(l.offset);
                            for x in &live {
                                let (xr, xo) = split(x.offset);
                                assert!(r != xr || o + l.len <= xo || xo + x.len <= o, "seed {seed} step {step}: overlap");
                            }
                            live.push(l);
                        }
                    }
                    55..=89 if !live.is_empty() => {
                        let k = rng.below(live.len() as u64) as usize;
                        p.release(live.swap_remove(k).offset).unwrap();
                    }
                    90..=94 => {
                        if let Some(id) = p.next_region_id() {
                            p.add_region(id, (1 + rng.below(4)) * 64 * M).unwrap();
                        }
                    }
                    95..=97 => {
                        if let Some(&id) = p.empty_regions().first() {
                            p.remove_region(id).unwrap();
                        }
                    }
                    _ => {
                        let s = 1 + rng.below(5);
                        let back = p.close_session(s);
                        live.retain(|l| !back.contains(l));
                    }
                }
                if !cfg!(miri) || step % 50 == 0 {
                    p.check().unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                }
            }
            for s in 1..=5 {
                p.close_session(s);
            }
            p.check().unwrap();
            let n = p.regions().len() as u64;
            assert_eq!(p.free(), p.capacity() - n * p.config().header, "seed {seed}: leaked");
            assert_eq!(p.largest_free(), p.regions().iter().map(|r| r.1 - p.config().header).max().unwrap());
        }
    }
}
