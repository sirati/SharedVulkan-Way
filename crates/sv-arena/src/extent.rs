//! Extents, runs, and the provider/locator/attacher split.
//!
//! docs/design.md §5.1 and §5.7. The vocabulary matters here because three words that
//! sound alike mean different things:
//!
//! - An **extent** is one allocation made by the side inside the VM, covering
//!   the control region *and* the whole arena. It is the unit of committing, of
//!   stamping, of locating and of attaching (D22).
//! - A **slab** is a sub-range of an extent that the GPU side imports as one
//!   `VkDeviceMemory`. It is an import unit and a failure unit, and **not** an
//!   attach unit (§5.2).
//! - A **run** is a maximal contiguous range of the *backing file*. Runs are
//!   what `mmap(MAP_FIXED)` is called once per, and their count is the thing
//!   fragmentation actually costs (§5.7).
//!
//! # The run list is not protocol, and this module is where that is enforced
//!
//! > **The run list does not cross the wire, and it never did have to.** On the
//! > locating paths the guest does not know it — that is the entire reason
//! > `ScanLocator` exists — and on the AWE path what crosses is the frame list,
//! > from which the runs are derived on the far side. — docs/design.md §5.7
//!
//! So [`RunList`] has no encoder and no decoder, and it is not reachable from
//! any `sv_transport::cold` message. What crosses is
//! [`sv_transport::cold::ExtentDesc`], which describes *how to find* the pages.
//! The test `a_run_list_has_no_wire_form` is the executable version of this
//! paragraph — it is a compile-time property, so what the test can check is that
//! the descriptor's size does not vary with the run count.

use crate::{Error, Result};
use sv_transport::cold::{ExtentDesc, Granularity, Locator};
use sv_transport::region::PAGE;

/// A maximal contiguous range of the backing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Run {
    /// Offset in the backing object — a memfd offset on Linux.
    pub file_offset: u64,
    /// Offset within the extent, which is what the two sides agree on (§1).
    pub extent_offset: u64,
    pub len: u64,
}

/// The runs of one extent, in extent order.
///
/// Side-local and derived. `sv-hostd` computes it once per generation and hands
/// it to each client with the fd, "which is a local optimisation and not a
/// protocol feature" (§5.7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunList {
    runs: Vec<Run>,
}

impl RunList {
    pub fn new(runs: Vec<Run>) -> RunList {
        RunList { runs }
    }

    /// The identity run list: the extent is one contiguous range of the file.
    ///
    /// This is the development loop, and it is *also* what a 1 GiB large-page
    /// extent measured as — 1 run (§5.7's table). So this is not a mock: it is
    /// the best real case, and the case the intended path aims at.
    pub fn contiguous(file_offset: u64, len: u64) -> RunList {
        RunList { runs: vec![Run { file_offset, extent_offset: 0, len }] }
    }

    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    pub fn len(&self) -> usize {
        self.runs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Total bytes covered.
    pub fn bytes(&self) -> u64 {
        self.runs.iter().map(|r| r.len).sum()
    }

    /// Check the list actually describes an extent: covering `[0, len)` of the
    /// extent exactly once, in order, with no gap and no overlap.
    ///
    /// A gap would be a hole in the offset space, and §1's invariant — every
    /// byte at the same offset on both sides — is precisely what a hole breaks.
    /// The locator is the code most likely to get this subtly wrong, so the
    /// check is here rather than in the locator, where it would be marking its
    /// own homework.
    pub fn validate(&self, extent_len: u64) -> Result<()> {
        let mut at = 0u64;
        for r in &self.runs {
            if r.extent_offset != at {
                return Err(Error::BadGeometry("the run list has a gap or an overlap"));
            }
            if r.len == 0 {
                return Err(Error::BadGeometry("a zero-length run"));
            }
            at = at
                .checked_add(r.len)
                .ok_or(Error::BadGeometry("the run list overflows"))?;
        }
        if at != extent_len {
            return Err(Error::BadGeometry("the run list does not cover the extent"));
        }
        Ok(())
    }

    /// Refuse loudly if attaching would need more mappings than the host allows.
    ///
    /// > `sv-hostd` reads `vm.max_map_count` at startup, compares it against the
    /// > run count it just built, and **refuses loudly with the actual numbers**
    /// > rather than letting a client discover it at the 1048577th mapping.
    /// > — docs/design.md §5.7
    ///
    /// The kernel cannot merge these VMAs — adjacent runs are by definition
    /// non-adjacent in the file — so **VMA count equals run count exactly**.
    pub fn check_map_count(&self, max_map_count: usize) -> Result<()> {
        // A margin, because the process has other mappings: its own binary, the
        // control region, the stack. Ten per cent of a million is generous and
        // the failure it prevents is an `mmap` returning ENOMEM halfway through
        // building an arena, which is "recoverable in principle and horrible in
        // practice".
        let budget = max_map_count - max_map_count / 10;
        if self.runs.len() > budget {
            return Err(Error::TooManyRuns { runs: self.runs.len(), max_map_count });
        }
        Ok(())
    }
}

/// A page map, as a locator produces it: one file offset per 4 KiB page of the
/// extent, in extent order.
///
/// Transient by design — "8 bytes per page and is dropped once coalesced"
/// (§5.7). At 2 GiB that is 4 MiB of `Vec`, which is why it does not survive the
/// call that made it.
#[derive(Debug, Clone, Default)]
pub struct PageMap {
    offsets: Vec<u64>,
}

impl PageMap {
    pub fn new(offsets: Vec<u64>) -> PageMap {
        PageMap { offsets }
    }

    pub fn pages(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// The offsets themselves, in extent order.
    ///
    /// Exposed for exactly one caller: `security-model.md` R28's cross-check,
    /// which compares the map the scan built against the map the frame list
    /// translated to, page for page. Comparing the *coalesced* run lists would
    /// not do — two different page maps can coalesce to run lists that differ
    /// only in ways a reader has to reconstruct, and the number R28 needs is
    /// "which page index disagreed".
    pub fn offsets(&self) -> &[u64] {
        &self.offsets
    }

    /// Coalesce consecutive pages into runs.
    ///
    /// This is `arena-host`'s coalescer, which step 0 wrote and which design.md
    /// §3 names as the seed of `sv-hostd`. The measured payoff: 16384 pages
    /// become 1471 runs on the 4 KiB path and 22 on the large-page path at
    /// 64 MiB, and **one run at a gigabyte**.
    pub fn coalesce(&self) -> RunList {
        let mut runs: Vec<Run> = Vec::new();
        for (i, &off) in self.offsets.iter().enumerate() {
            let extent_offset = i as u64 * PAGE as u64;
            match runs.last_mut() {
                Some(last) if last.file_offset + last.len == off => last.len += PAGE as u64,
                _ => runs.push(Run { file_offset: off, extent_offset, len: PAGE as u64 }),
            }
        }
        RunList { runs }
    }
}

/// One committed extent, as the side that allocated it sees it.
///
/// The counterpart of [`ExtentDesc`], which is what crosses. This is the local
/// half and it holds the run list; the wire half holds a locator.
#[derive(Debug, Clone)]
pub struct Extent {
    pub desc: ExtentDesc,
    pub runs: RunList,
}

impl Extent {
    /// Bytes in the control region half of the extent.
    pub fn control_len(&self) -> u64 {
        self.desc.control_pages * PAGE as u64
    }

    /// Bytes in the arena half.
    pub fn arena_len(&self) -> u64 {
        self.desc.arena_pages * PAGE as u64
    }

    pub fn total_len(&self) -> u64 {
        self.control_len() + self.arena_len()
    }

    /// Pages in the extent, control region and arena together.
    pub fn pages(&self) -> u64 {
        self.desc.control_pages + self.desc.arena_pages
    }

    /// Publish this extent as located by its **frame list** rather than by its
    /// contents: `desc.locator` becomes [`Locator::Pfns`] (docs/design.md §0.14).
    ///
    /// This is the guest-side half of the frame path, and it is deliberately
    /// separate from [`ArenaProvider::commit`]: a provider that cannot ask its
    /// own kernel where the pages are must not be able to express a frame list,
    /// and one that can — `awe_probe::superfetch` on Windows, measured at
    /// ~330 ms whatever the window is — should not have to build an `ExtentDesc`
    /// by hand to say so.
    ///
    /// **The stamps are not skipped, and the caller must not skip them.** R28
    /// makes the content scan the cross-check that the undocumented query is
    /// telling the truth, run once at attach while the stamps still exist. A
    /// guest that published a frame list *instead* of stamping would leave the
    /// far side with a locator it cannot check and no way to notice a shifted
    /// field — R21's silent wrong answer, arrived at by a shortcut.
    ///
    /// `frames` is one frame number per 4 KiB page of the extent, in extent
    /// order. A list of the wrong length is refused rather than truncated: a
    /// short frame list translates into an arena with a hole in it, and nothing
    /// downstream of the locator would notice.
    pub fn locate_by_frames(&mut self, frames: &[u64]) -> Result<()> {
        let pages = self.pages();
        if frames.len() as u64 != pages {
            return Err(Error::BadGeometry("the frame list is not one frame per page"));
        }
        if pages == 0 {
            return Err(Error::BadGeometry("an extent with no pages"));
        }
        self.desc.locator = Locator::from_frames(frames);
        Ok(())
    }

    /// Does this extent need the R26 canary?
    ///
    /// Only on the 4 KiB path: "large pages remove the failure by construction"
    /// because a large page is not a candidate for relocation by the ordinary
    /// paging machinery (§5.1, §5.2).
    pub fn needs_canary(&self) -> bool {
        self.desc.granularity == Granularity::Small4K
    }
}

// ---------------------------------------------------------------------------
// The three traits (docs/design.md §5.1)
// ---------------------------------------------------------------------------

/// The side inside the VM: allocates and describes.
pub trait ArenaProvider {
    /// Commit an extent covering both regions.
    fn commit(&mut self, control_len: u64, arena_len: u64) -> Result<Extent>;
    /// Stamp every page so the far side can find them by content. Skipped only
    /// when frames are known, which is the AWE path and is not taken (§5.1).
    fn stamp(&mut self, extent: &Extent, run_id: u64) -> Result<()>;
    /// `Some` only on the AWE path. `Option` is "the honest encoding of what
    /// step 0 found: page enumeration is a *capability of one allocation path*,
    /// not a property of the guest".
    fn frames(&self, extent: &Extent) -> Option<RunList>;
}

/// The attaching side: turns "how to find it" into "where it is".
pub trait ArenaLocator {
    fn locate(&self, desc: &ExtentDesc) -> Result<PageMap>;
}

/// The attaching side: turns runs into one virtually contiguous view.
pub trait ArenaAttacher {
    /// Reserve a `PROT_NONE` range and `mmap(MAP_FIXED)` each run into it.
    fn attach(&self, runs: &RunList) -> Result<()>;
}

/// The locator for the case where the extent is already contiguous in the file.
///
/// **This is not a stub.** It is the correct locator for two real situations:
/// the same-machine development loop, where `sv-hostd` created the memfd itself
/// and therefore knows exactly where the extent is; and the Linux-guest
/// topology, where the arena is a memfd shared between two containers and there
/// is no hypervisor in between to hide the layout. Locating by content
/// ([`Locator::Scan`]) exists because *Windows in a VM* cannot report its own
/// frames — it is a workaround for an absence, not the general case.
#[derive(Debug, Clone, Copy)]
pub struct ContiguousLocator {
    pub file_offset: u64,
}

impl ArenaLocator for ContiguousLocator {
    fn locate(&self, desc: &ExtentDesc) -> Result<PageMap> {
        let pages = desc
            .control_pages
            .checked_add(desc.arena_pages)
            .ok_or(Error::BadGeometry("extent page count overflows"))?;
        let pages = usize::try_from(pages).map_err(|_| Error::BadGeometry("extent too large"))?;
        Ok(PageMap::new(
            (0..pages).map(|i| self.file_offset + i as u64 * PAGE as u64).collect(),
        ))
    }
}

/// Build the descriptor for a contiguous extent.
pub fn contiguous_extent(
    run_id: u64,
    file_offset: u64,
    control_len: u64,
    arena_len: u64,
) -> Result<Extent> {
    if !control_len.is_multiple_of(PAGE as u64) || !arena_len.is_multiple_of(PAGE as u64) {
        return Err(Error::BadGeometry("an extent is a whole number of pages"));
    }
    if arena_len == 0 {
        return Err(Error::BadGeometry("an extent with no arena"));
    }
    Ok(Extent {
        desc: ExtentDesc {
            run_id,
            // A memfd is 4 KiB-granular unless it was created with hugetlb, and
            // saying `Large2M` here would switch off the canary on a path that
            // has not earned it. Understating granularity is the safe direction:
            // it costs one page per slab and buys the R26 detector.
            granularity: Granularity::Small4K,
            control_pages: control_len / PAGE as u64,
            arena_pages: arena_len / PAGE as u64,
            locator: Locator::Scan,
        },
        runs: RunList::contiguous(file_offset, control_len + arena_len),
    })
}

/// Read `vm.max_map_count`, or the kernel default if it cannot be read.
#[cfg(target_os = "linux")]
pub fn max_map_count() -> usize {
    std::fs::read_to_string("/proc/sys/vm/max_map_count")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        // The historical default. Being wrong in this direction means refusing
        // an arena that would have worked, which beats an ENOMEM halfway
        // through building one.
        .unwrap_or(65530)
}

#[cfg(not(target_os = "linux"))]
pub fn max_map_count() -> usize {
    // Windows has no per-process VMA limit of this kind, and the attaching side
    // is not Windows on any configuration measured so far. Returning a large
    // number rather than pretending to know is the honest answer.
    usize::MAX / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(pages: u64, locator: Locator) -> ExtentDesc {
        ExtentDesc {
            run_id: 1,
            granularity: Granularity::Small4K,
            control_pages: 1,
            arena_pages: pages,
            locator,
        }
    }

    #[test]
    fn coalescing_turns_pages_into_runs() {
        // Four pages, contiguous in the file but not at the file's start.
        let pm = PageMap::new(vec![8192, 12288, 16384, 20480]);
        let runs = pm.coalesce();
        assert_eq!(runs.len(), 1, "contiguous pages must become one run");
        assert_eq!(runs.runs()[0], Run { file_offset: 8192, extent_offset: 0, len: 16384 });
        runs.validate(16384).unwrap();
    }

    #[test]
    fn a_scattered_page_map_becomes_one_run_per_break() {
        // The shape step 0 measured: pages in the file order the guest happened
        // to get, not the order it asked for.
        let pm = PageMap::new(vec![4096, 8192, 40960, 45056, 4096 * 100]);
        let runs = pm.coalesce();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs.bytes(), 5 * PAGE as u64);
        runs.validate(5 * PAGE as u64).unwrap();
        // Extent offsets are dense even though file offsets are not — that *is*
        // the §1 invariant, stated as an assertion.
        assert_eq!(runs.runs()[0].extent_offset, 0);
        assert_eq!(runs.runs()[1].extent_offset, 2 * PAGE as u64);
        assert_eq!(runs.runs()[2].extent_offset, 4 * PAGE as u64);
    }

    /// The measured cases from docs/design.md §5.7's table, as arithmetic over the
    /// coalescer rather than as prose. If the coalescer ever regressed, the
    /// numbers this project quotes would stop being reproducible.
    #[test]
    fn the_measured_fragmentation_cases_reproduce() {
        // 64 MiB of 4 KiB pages, every other page taken from elsewhere: the
        // worst case, one run per page.
        let scattered: Vec<u64> = (0..16384).map(|i| (i as u64 * 2 + 1) * PAGE as u64).collect();
        assert_eq!(PageMap::new(scattered).coalesce().len(), 16384);

        // A 1 GiB large-page window came back as *one* run. 262144 pages.
        let one_run: Vec<u64> = (0..262144).map(|i| i as u64 * PAGE as u64).collect();
        let runs = PageMap::new(one_run).coalesce();
        assert_eq!(runs.len(), 1, "a 1 GiB large-page extent is one run (measured)");
        assert_eq!(runs.bytes(), 1 << 30);
    }

    #[test]
    fn a_run_list_with_a_gap_is_refused() {
        let bad = RunList::new(vec![
            Run { file_offset: 0, extent_offset: 0, len: PAGE as u64 },
            // Extent offset skips a page: a hole in the offset space, which is
            // exactly what §1's invariant forbids.
            Run { file_offset: 8192, extent_offset: 2 * PAGE as u64, len: PAGE as u64 },
        ]);
        assert!(bad.validate(3 * PAGE as u64).is_err());

        let short = RunList::contiguous(0, PAGE as u64);
        assert!(short.validate(2 * PAGE as u64).is_err(), "a short run list must not pass");
        short.validate(PAGE as u64).unwrap();
    }

    #[test]
    fn a_zero_length_run_is_refused() {
        let bad = RunList::new(vec![Run { file_offset: 0, extent_offset: 0, len: 0 }]);
        assert!(bad.validate(0).is_err());
    }

    #[test]
    fn too_many_runs_is_refused_up_front_with_the_numbers() {
        // docs/design.md §5.7: a 2 GiB arena is ~47,000 VMAs on the 4 KiB path, which
        // "sits at 4.5% of the per-process limit" on this host's 1048576.
        let two_gib = RunList::new(vec![Run { file_offset: 0, extent_offset: 0, len: 1 }; 47_000]);
        two_gib.check_map_count(1_048_576).unwrap();
        // …and fits the *old* default of 65530 too, which is the point §5.7
        // actually makes: at that default the 4 KiB path "would have capped the
        // arena at roughly 2.8 GiB", not at 2. 65530 / 23 runs-per-MiB = 2849
        // MiB, and the cap is what bites rather than 2 GiB itself.
        two_gib.check_map_count(65_530).unwrap();

        // A 3 GiB arena at the same 23 runs per MiB does not fit.
        let three_gib =
            RunList::new(vec![Run { file_offset: 0, extent_offset: 0, len: 1 }; 3 * 1024 * 23]);
        let err = three_gib.check_map_count(65_530).unwrap_err();
        assert_eq!(err, Error::TooManyRuns { runs: 70_656, max_map_count: 65_530 });
        // The margin is real: a run count exactly at the limit must fail rather
        // than squeak through and leave no room for the process's own mappings.
        assert!(two_gib.check_map_count(47_000).is_err());
    }

    #[test]
    fn a_contiguous_locator_produces_one_run() {
        let d = desc(15, Locator::Scan);
        let pm = ContiguousLocator { file_offset: 0 }.locate(&d).unwrap();
        assert_eq!(pm.pages(), 16);
        let runs = pm.coalesce();
        assert_eq!(runs.len(), 1);
        runs.validate(16 * PAGE as u64).unwrap();
    }

    #[test]
    fn a_contiguous_extent_is_whole_pages_and_carries_a_canary() {
        let e = contiguous_extent(7, 0, 4 * PAGE as u64, 64 * PAGE as u64).unwrap();
        assert_eq!(e.control_len(), 4 * PAGE as u64);
        assert_eq!(e.arena_len(), 64 * PAGE as u64);
        assert_eq!(e.total_len(), 68 * PAGE as u64);
        e.runs.validate(e.total_len()).unwrap();
        // A memfd is 4 KiB-granular, so the canary is required. Claiming
        // `Large2M` here would silently switch off the R26 detector.
        assert!(e.needs_canary());

        assert!(contiguous_extent(7, 0, 100, 4096).is_err(), "not a whole page");
        assert!(contiguous_extent(7, 0, 4096, 0).is_err(), "no arena");
    }

    /// The guest-side half of docs/design.md §0.14: an extent that *was* going to be
    /// found by content is re-described as one whose frames are known, and the
    /// descriptor then carries the frame list instead of nothing.
    #[test]
    fn an_extent_can_be_republished_as_located_by_its_frame_list() {
        let mut e = contiguous_extent(7, 0, PAGE as u64, 3 * PAGE as u64).unwrap();
        assert_eq!(e.desc.locator, Locator::Scan, "the locating path is the default");
        assert_eq!(e.pages(), 4);

        // Three pages on adjacent frames and one elsewhere: two runs, which is
        // the shape a real window has and the reason the wire form is runs.
        e.locate_by_frames(&[0x4_2000, 0x4_2001, 0x4_2002, 0x9_9999]).unwrap();
        let runs = e.desc.locator.pfn_runs().expect("the descriptor now carries frames");
        assert_eq!(runs.len(), 2);
        assert_eq!(e.desc.locator.frame_count(), e.pages());

        // Nothing else about the extent moved: the locator says how to find the
        // pages and not which pages they are.
        assert_eq!(e.desc.run_id, 7);
        assert_eq!(e.desc.control_pages, 1);
        assert_eq!(e.desc.arena_pages, 3);
    }

    /// A frame list of the wrong length translates into an arena with a hole in
    /// it, and nothing downstream of the locator would notice — so the refusal
    /// has to happen here, where the page count is still known.
    #[test]
    fn a_frame_list_that_is_not_one_frame_per_page_is_refused() {
        let mut e = contiguous_extent(7, 0, PAGE as u64, 3 * PAGE as u64).unwrap();
        assert!(e.locate_by_frames(&[1, 2, 3]).is_err(), "one page short");
        assert!(e.locate_by_frames(&[1, 2, 3, 4, 5]).is_err(), "one page long");
        assert!(e.locate_by_frames(&[]).is_err());
        // …and the descriptor is untouched by a refusal, rather than left
        // half-updated with a list it already rejected.
        assert_eq!(e.desc.locator, Locator::Scan);
        e.locate_by_frames(&[1, 2, 3, 4]).unwrap();
    }

    /// docs/design.md §4.3: the descriptor "carries no addresses at all" on the
    /// locating paths. A compiler cannot be asked to prove the run list has no
    /// wire form, so what is asserted is the observable consequence.
    #[test]
    fn a_run_list_has_no_wire_form() {
        let mut buf = Vec::new();
        let one = sv_transport::cold::ColdMessage::Hello(Box::new(sv_transport::cold::Hello {
            ids: sv_transport::control::ProtocolIds::new(1, 1),
            generation: 1,
            extent: desc(1, Locator::Scan),
            geometry: sv_transport::control::Geometry::default(),
            endianness: sv_transport::control::ENDIAN_MARKER,
            pointer_width: 64,
        }));
        one.encode(&mut buf).unwrap();
        let small = buf.len();

        let sv_transport::cold::ColdMessage::Hello(mut h) = one else { unreachable!() };
        // A 2 GiB arena: 524288 pages, and on the 4 KiB path ~47,000 runs.
        h.extent.arena_pages = 524_288;
        sv_transport::cold::ColdMessage::Hello(h).encode(&mut buf).unwrap();
        assert_eq!(
            buf.len(),
            small,
            "the Hello grew with the extent size, so something about the layout is on the wire"
        );
    }
}
