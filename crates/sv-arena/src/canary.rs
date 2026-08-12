//! The R26 canary: bytes the allocator never hands out, checked at handshake
//! and on a timer.
//!
//! # The failure it detects
//!
//! `VirtualLock` promises *residency*, not frame stability. The locate step runs
//! once and its output is a table of memfd offsets, so if a frame moved
//! afterwards the app side would keep reading the old offset — **no fault, no
//! error, just an arena where one side's writes stop reaching the other**
//! (`security-model.md` R26). The per-page stamps that made step 0's
//! verification work are overwritten the moment arena data lands in those pages,
//! so "is this still my page?" would otherwise have nothing to compare against.
//!
//! Hence bytes that are never handed out and never overwritten.
//!
//! # What it costs, which is not what design.md §5.2 says
//!
//! > **On a 4 KiB-granularity extent the allocator reserves a canary per slab**
//! > and never hands those bytes to an application: the **first 64 bytes of each
//! > slab-sized block** … Cost: 64 bytes per 64 MiB, which is 1 part in a
//! > million, against a failure mode whose alternative is silence. — §5.2
//!
//! The canary is 64 bytes and 64 bytes in 64 MiB is indeed 1 part in a million.
//! **The cost to the allocator is not 64 bytes.** §5.2's own preceding
//! paragraph sets the allocator's granularity at 4096 bytes — the measured
//! `minImportedHostPointerAlignment` — so the remaining 4032 bytes of the page
//! holding the canary cannot be handed out either. They are not free; they are
//! unallocatable. The real figure is:
//!
//! | | per 64 MiB slab | fraction |
//! |---|---:|---|
//! | design.md §5.2 claims | 64 B | 1 / 1,048,576 |
//! | **what it actually costs** | **4096 B** | **1 / 16,384 (0.0061%)** |
//! | R26's measured per-page canary (`gpu-probe`, 256 B/page) | 4 MiB | 6.25% |
//!
//! Sixteen times the stated figure, and still negligible — 0.006% of a 2 GiB
//! arena is 128 KiB. The number is worth correcting anyway, because the third
//! row is the one that matters and the design has not yet chosen between rows
//! two and three.
//!
//! # Per-slab and per-page detect different things
//!
//! R26's own note, after the measurement, is the sharpest statement of this:
//!
//! > per-page canaries are what make a *wrong page* detectable, per-slab
//! > canaries only detect a wrong slab. Which granularity the arena wants is a
//! > design decision this measurement can now inform.
//!
//! A per-slab canary sits in the slab's first page. If *that* page's frame
//! moves, it fires. If any of the slab's other 16,383 pages moves, **it does
//! not** — and the failure R26 describes is per-page relocation by the memory
//! manager, which has no reason to prefer page zero. So the per-slab canary
//! detects a small fraction of the failure it was introduced for.
//!
//! This module implements the granularity design.md §5.2 specifies
//! ([`CanaryPolicy::PerSlab`]) and makes the alternative available
//! ([`CanaryPolicy::PerPage`]) rather than choosing on the design's behalf. The
//! honest summary for the design to act on: **at 4096-byte allocator
//! granularity, per-slab costs 0.0061% and catches roughly 1 page in 16,384;
//! per-page costs 6.25% and catches all of them.** The middle ground the design
//! has not considered — a canary every N pages — costs `4096/N` per slab-page
//! and catches `1/N` of relocations, and is a one-line change to
//! [`CanaryPolicy`] if anyone wants it.

use crate::{Error, GRANULARITY, Result};
use sv_transport::region::SharedRegion;

/// `b"SVCANRY1"`, distinct from `arena-record`'s `SVARENA1` stamp because they
/// mean different things: the stamp is a locating device that is *expected* to
/// be overwritten, the canary is expected never to be.
pub const CANARY_MAGIC: &[u8; 8] = b"SVCANRY1";

/// Bytes in one canary record. design.md §5.2's "first 64 bytes".
pub const CANARY_LEN: u64 = 64;

const OFF_MAGIC: usize = 0;
const OFF_RUN_ID: usize = 8;
const OFF_SLAB_INDEX: usize = 16;
const OFF_PAGE_INDEX: usize = 20;

/// Where canaries go, and how much of the arena that costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryPolicy {
    /// Large-page extents. "Large pages remove the question, because a large
    /// page is not a candidate for relocation by the ordinary paging machinery"
    /// (§5.1), so there is nothing to detect and nothing to pay.
    None,
    /// design.md §5.2: the first 64 bytes of each slab-sized block. Costs one
    /// page per slab at 4096-byte granularity.
    PerSlab { run_id: u64 },
    /// R26 as measured. Costs one page in every `stride` pages, and detects a
    /// relocation of any page whose index is a multiple of `stride`. `stride: 1`
    /// is the 6.25%-equivalent that `gpu-probe` exercised.
    PerPage { run_id: u64, stride: u32 },
}

impl CanaryPolicy {
    /// The policy an extent of this granularity wants.
    pub fn for_extent(needs_canary: bool, run_id: u64) -> CanaryPolicy {
        if needs_canary { CanaryPolicy::PerSlab { run_id } } else { CanaryPolicy::None }
    }

    pub fn run_id(self) -> u64 {
        match self {
            CanaryPolicy::None => 0,
            CanaryPolicy::PerSlab { run_id } | CanaryPolicy::PerPage { run_id, .. } => run_id,
        }
    }
}

/// The 64 bytes themselves.
pub fn record(run_id: u64, slab_index: u32, page_index: u32) -> [u8; CANARY_LEN as usize] {
    let mut r = [0u8; CANARY_LEN as usize];
    r[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(CANARY_MAGIC);
    r[OFF_RUN_ID..OFF_RUN_ID + 8].copy_from_slice(&run_id.to_le_bytes());
    r[OFF_SLAB_INDEX..OFF_SLAB_INDEX + 4].copy_from_slice(&slab_index.to_le_bytes());
    r[OFF_PAGE_INDEX..OFF_PAGE_INDEX + 4].copy_from_slice(&page_index.to_le_bytes());
    r
}

/// Every offset in the arena that carries a canary, under `policy`.
///
/// One function so that the writer and the checker cannot disagree about where
/// the canaries are — the same reason `arena-record` exists as a shared crate
/// rather than as two copies of a layout.
pub fn offsets(policy: CanaryPolicy, arena_len: u64, slab_size: u64) -> Vec<(u64, u32, u32)> {
    let mut out = Vec::new();
    match policy {
        CanaryPolicy::None => {}
        CanaryPolicy::PerSlab { .. } => {
            let mut base = 0u64;
            let mut slab = 0u32;
            while base < arena_len {
                out.push((base, slab, 0));
                base += slab_size;
                slab += 1;
            }
        }
        CanaryPolicy::PerPage { stride, .. } => {
            let stride = stride.max(1);
            let pages = arena_len / GRANULARITY;
            let mut page = 0u64;
            while page < pages {
                let off = page * GRANULARITY;
                out.push((off, (off / slab_size) as u32, page as u32));
                page += u64::from(stride);
            }
        }
    }
    out
}

/// Write every canary. The GPU side does this "after the extent is stamped and
/// located" (§5.2) — after, because the stamp and the canary occupy the same
/// bytes and the canary is the one that has to survive.
pub fn write_all(
    arena: &SharedRegion,
    policy: CanaryPolicy,
    arena_len: u64,
    slab_size: u64,
) -> Result<usize> {
    let run_id = policy.run_id();
    let sites = offsets(policy, arena_len, slab_size);
    for &(off, slab, page) in &sites {
        arena.write_bytes(off, &record(run_id, slab, page))?;
    }
    Ok(sites.len())
}

/// Check every canary.
///
/// > A canary mismatch is fatal to the session, not something to repair: by the
/// > time it is noticed the two sides have already disagreed about memory.
/// > — design.md §5.1
///
/// Returns how many were checked, so that a caller can tell "all correct" from
/// "there were none to check" — which is the same class of false negative as a
/// test that passes because it asserted nothing.
pub fn verify_all(
    arena: &SharedRegion,
    policy: CanaryPolicy,
    arena_len: u64,
    slab_size: u64,
) -> Result<usize> {
    let run_id = policy.run_id();
    let sites = offsets(policy, arena_len, slab_size);
    let mut got = [0u8; CANARY_LEN as usize];
    for &(off, slab, page) in &sites {
        let want = record(run_id, slab, page);
        arena.read_bytes(off, &mut got)?;
        if got != want {
            return Err(Error::CanaryMismatch {
                slab,
                expected: want[..16].try_into().unwrap(),
                found: got[..16].try_into().unwrap(),
            });
        }
    }
    Ok(sites.len())
}

/// Drives the periodic check without owning a thread or reading the clock.
///
/// design.md §5.2 asks for the canary to be re-checked "at every handshake and
/// on a timer". A timer that lives inside this crate would need a thread, and a
/// thread would need a shutdown protocol; the caller already has an event loop,
/// so it gets asked instead.
#[derive(Debug, Clone, Copy)]
pub struct CanaryTimer {
    interval: std::time::Duration,
    last: std::time::Instant,
}

impl CanaryTimer {
    pub fn new(now: std::time::Instant, interval: std::time::Duration) -> CanaryTimer {
        CanaryTimer { interval, last: now }
    }

    /// Should the canary be checked now?
    pub fn due(&mut self, now: std::time::Instant) -> bool {
        if now.duration_since(self.last) >= self.interval {
            self.last = now;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const SLAB: u64 = 1 << 20;
    const ARENA: u64 = 4 << 20;

    fn arena() -> SharedRegion {
        SharedRegion::heap(ARENA as usize).unwrap()
    }

    #[test]
    fn a_per_slab_canary_is_one_per_slab() {
        let sites = offsets(CanaryPolicy::PerSlab { run_id: 1 }, ARENA, SLAB);
        assert_eq!(sites.len(), 4);
        assert_eq!(sites[0], (0, 0, 0));
        assert_eq!(sites[3], (3 * SLAB, 3, 0));
    }

    #[test]
    fn writing_then_verifying_passes_and_says_how_many_it_checked() {
        let a = arena();
        let p = CanaryPolicy::PerSlab { run_id: 0xabc };
        assert_eq!(write_all(&a, p, ARENA, SLAB).unwrap(), 4);
        // The count is the point: `verify_all` returning Ok(0) would mean
        // "nothing was checked", which reads identically to success at the call
        // site unless the number comes back.
        assert_eq!(verify_all(&a, p, ARENA, SLAB).unwrap(), 4);
    }

    #[test]
    fn a_disturbed_canary_is_detected_and_names_the_slab() {
        let a = arena();
        let p = CanaryPolicy::PerSlab { run_id: 0xabc };
        write_all(&a, p, ARENA, SLAB).unwrap();
        // One byte, in the third slab: the shape of a frame that moved.
        a.write_bytes(2 * SLAB + 3, &[0xFF]).unwrap();
        match verify_all(&a, p, ARENA, SLAB) {
            Err(Error::CanaryMismatch { slab, .. }) => assert_eq!(slab, 2),
            other => panic!("a moved frame went undetected: {other:?}"),
        }
    }

    /// The failure a *different* run's canary would represent: the extent was
    /// re-located and the offsets now point at somebody else's arena.
    #[test]
    fn a_canary_from_another_run_does_not_pass() {
        let a = arena();
        write_all(&a, CanaryPolicy::PerSlab { run_id: 1 }, ARENA, SLAB).unwrap();
        assert!(verify_all(&a, CanaryPolicy::PerSlab { run_id: 2 }, ARENA, SLAB).is_err());
    }

    #[test]
    fn a_canary_carrying_the_wrong_slab_index_does_not_pass() {
        // The R21 failure mode arriving by R26's route: the offsets are off by
        // one slab, so every canary is intact but in the wrong place.
        let a = arena();
        let p = CanaryPolicy::PerSlab { run_id: 1 };
        write_all(&a, p, ARENA, SLAB).unwrap();
        a.write_bytes(0, &record(1, 1, 0)).unwrap();
        assert!(verify_all(&a, p, ARENA, SLAB).is_err());
    }

    #[test]
    fn the_none_policy_checks_nothing_and_admits_it() {
        let a = arena();
        assert_eq!(write_all(&a, CanaryPolicy::None, ARENA, SLAB).unwrap(), 0);
        assert_eq!(verify_all(&a, CanaryPolicy::None, ARENA, SLAB).unwrap(), 0);
    }

    /// The cost table in this module's documentation, as arithmetic.
    #[test]
    fn the_canary_costs_what_this_module_says_and_not_what_the_design_says() {
        let slab = 64u64 << 20;
        let arena = 2u64 << 30;

        // Per slab: one page per slab, not 64 bytes per slab.
        let per_slab = offsets(CanaryPolicy::PerSlab { run_id: 1 }, arena, slab);
        assert_eq!(per_slab.len(), 32);
        let cost = per_slab.len() as u64 * GRANULARITY;
        assert_eq!(cost, 128 * 1024);
        assert_eq!(slab / GRANULARITY, 16_384, "1 part in 16384 per slab");
        // design.md §5.2's figure, for contrast: 64 B × 32 slabs = 2 KiB.
        assert_eq!(per_slab.len() as u64 * CANARY_LEN, 2048);
        assert_eq!(cost / (per_slab.len() as u64 * CANARY_LEN), 64, "64× the bytes claimed");

        // Per page at stride 1: 6.25% if the record is 256 bytes, and 100% of
        // the page if the allocator's granularity forces a whole page — which is
        // why `gpu-probe` reserved 256 bytes *within* each page rather than
        // asking the allocator for it. That distinction is the reason row three
        // of the table is not simply "16384× row two".
        let per_page = offsets(CanaryPolicy::PerPage { run_id: 1, stride: 1 }, slab, slab);
        assert_eq!(per_page.len() as u64, slab / GRANULARITY);
        assert_eq!(256 * 100 / GRANULARITY, 6, "gpu-probe's 256 B/page is 6.25%");
    }

    #[test]
    fn a_strided_canary_is_the_middle_ground() {
        let sites = offsets(CanaryPolicy::PerPage { run_id: 1, stride: 16 }, SLAB, SLAB);
        assert_eq!(sites.len(), (SLAB / GRANULARITY / 16) as usize);
        // Catches 1 page in 16 for 1/16 of the per-page cost — the trade the
        // design has not yet made, available as a one-word change.
        let a = SharedRegion::heap(SLAB as usize).unwrap();
        let p = CanaryPolicy::PerPage { run_id: 1, stride: 16 };
        assert_eq!(write_all(&a, p, SLAB, SLAB).unwrap(), 16);
        assert_eq!(verify_all(&a, p, SLAB, SLAB).unwrap(), 16);
        a.write_bytes(16 * GRANULARITY, &[0]).unwrap();
        assert!(verify_all(&a, p, SLAB, SLAB).is_err());
    }

    #[test]
    fn the_timer_fires_on_the_interval_and_not_before() {
        let t0 = Instant::now();
        let mut t = CanaryTimer::new(t0, Duration::from_millis(100));
        assert!(!t.due(t0));
        assert!(!t.due(t0 + Duration::from_millis(99)));
        assert!(t.due(t0 + Duration::from_millis(100)));
        assert!(!t.due(t0 + Duration::from_millis(150)));
        assert!(t.due(t0 + Duration::from_millis(200)));
    }
}
