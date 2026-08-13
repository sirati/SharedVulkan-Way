//! The **guest-side** arena provider on Linux: allocate guest RAM, pin it,
//! stamp it.
//!
//! design.md §5.1 gives the rule — "the side inside the VM allocates, because
//! only it can produce pages the hypervisor can locate" — and then describes
//! only the Windows implementation, because `testhost`'s guest is Windows.
//! This is the Linux one, and it exists because the development guest of §13
//! step 7 has to be a machine that can be rebooted.
//!
//! It is not a mock of the Windows path. It is the same three steps, with the
//! same failure modes, in the spelling this kernel uses:
//!
//! | step | Windows (`awe-probe::alloc`) | here |
//! |---|---|---|
//! | commit | `VirtualAlloc(MEM_RESERVE\|MEM_COMMIT\|MEM_LARGE_PAGES)` | `mmap(MAP_ANONYMOUS\|MAP_POPULATE)`, `MAP_HUGETLB` where asked |
//! | pin | implicit — a large page is never paged | `mlock`, which is what makes the frames stop moving (R26) |
//! | assert | `QueryWorkingSetEx`'s `LargePage` bit | `/proc/self/smaps`'s `KernelPageSize` |
//! | stamp | one `arena_record` per 4 KiB page | the same, from the same crate |
//!
//! The mapping half is [`sv_transport::region::LockedWindow`], because this
//! crate is `forbid(unsafe_code)` and intends to stay that way.
//!
//! # What this provider cannot do, and why that is the design working
//!
//! [`ArenaProvider::frames`] returns `None`. Linux *has*
//! `/proc/self/pagemap`, and since 4.0 it returns **zeroed** frame numbers to an
//! unprivileged process — so the guest cannot enumerate its own frames here
//! either, for a different reason than Windows and with the same consequence.
//! Reading pagemap as root would work and is deliberately not done: if the guest
//! half needed privilege, the topology would stop being one an ordinary
//! application can use. `ScanLocator` is the locator on this path exactly as
//! D21 says it is on every path.
//!
//! **The stamp is per 4 KiB page whatever the allocator did**, which §5.1 calls
//! "the one rule that is correct whatever the allocator did".

use crate::extent::{ArenaProvider, Extent, RunList};
use crate::{Error, Result};
use sv_transport::cold::{ExtentDesc, Granularity, Locator};
use sv_transport::region::{LockedWindow, PAGE};

/// What the guest asked the kernel for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pages {
    /// Ordinary 4 KiB anonymous pages, `mlock`ed. Always available.
    Small,
    /// `MAP_HUGETLB`, 2 MiB. Needs a pre-reserved pool (`vm.nr_hugepages`) and
    /// fails cleanly without one — the Linux analogue of
    /// `VirtualAlloc(MEM_LARGE_PAGES)` needing `SeLockMemoryPrivilege`, and with
    /// the same argument behind it: 1471 runs per 64 MiB become 22, and a
    /// gigabyte becomes 1 (`measured-gpu-side.md`).
    Huge2M,
}

/// The Linux guest's [`ArenaProvider`].
pub struct GuestRamProvider {
    pages: Pages,
    /// Held so the window outlives the descriptor the far side is locating
    /// against. Dropping the provider releases the arena, which is why the GPU
    /// side holds it for the whole session.
    window: Option<LockedWindow>,
}

impl GuestRamProvider {
    pub fn new(pages: Pages) -> GuestRamProvider {
        GuestRamProvider { pages, window: None }
    }

    /// The committed window, once [`ArenaProvider::commit`] has run.
    pub fn window(&self) -> Option<&LockedWindow> {
        self.window.as_ref()
    }

    /// Take the window, for handing to the driver as a host pointer.
    pub fn take_window(&mut self) -> Option<LockedWindow> {
        self.window.take()
    }

    /// What the kernel actually gave us, read back rather than assumed.
    pub fn observed_page_size(&self) -> Option<u64> {
        self.window.as_ref()?.observed_page_size()
    }

    /// Commit and stamp in one call, in that order.
    ///
    /// The ordering is not fussiness. The host's locator is a *content search*,
    /// so a descriptor published before the stamps land describes something the
    /// host cannot find, and the symptom is a two-second scan that returns zero
    /// pages — which reads exactly like the arena not being shared at all.
    /// Making the two steps one call is the cheapest way to make that ordering
    /// unwritable.
    pub fn commit_and_stamp(&mut self, control_len: u64, arena_len: u64) -> Result<Extent> {
        let extent = self.commit(control_len, arena_len)?;
        let run_id = extent.desc.run_id;
        self.stamp(&extent, run_id)?;
        Ok(extent)
    }
}

impl ArenaProvider for GuestRamProvider {
    fn commit(&mut self, control_len: u64, arena_len: u64) -> Result<Extent> {
        let total = control_len
            .checked_add(arena_len)
            .ok_or(Error::BadGeometry("the extent length overflows"))?;
        if total == 0 || !total.is_multiple_of(PAGE as u64) {
            return Err(Error::BadGeometry("an extent is a whole number of pages"));
        }
        if arena_len == 0 {
            return Err(Error::BadGeometry("an extent with no arena"));
        }
        let window = LockedWindow::commit(total, self.pages == Pages::Huge2M)?;
        self.window = Some(window);

        Ok(Extent {
            desc: ExtentDesc {
                // A run id the host scans for. Random per run, because two live
                // extents sharing an id is exactly the duplicate-index case
                // `sv-hostd`'s scan refuses — correctly, since it cannot tell
                // which pages belong to which.
                run_id: fresh_run_id(),
                // Understated on purpose, and corrected by the caller from
                // `observed_page_size` if it asks. Claiming `Large2M` here would
                // switch off the R26 canary on a path that has not earned it.
                granularity: Granularity::Small4K,
                control_pages: control_len / PAGE as u64,
                arena_pages: arena_len / PAGE as u64,
                locator: Locator::Scan,
            },
            // The guest cannot know its own runs — that is the whole reason the
            // scan exists — so the local run list is empty rather than a lie.
            // §5.7: "the run list does not cross the wire, and it never did have
            // to"; on this side it does not even exist.
            runs: RunList::default(),
        })
    }

    fn stamp(&mut self, extent: &Extent, run_id: u64) -> Result<()> {
        let pages = (extent.desc.control_pages + extent.desc.arena_pages) as usize;
        let window = self.window.as_mut().ok_or(Error::BadGeometry("nothing committed"))?;
        if pages * PAGE > window.len() {
            return Err(Error::BadGeometry("the extent is larger than the window"));
        }
        let bytes = window.as_mut_slice();
        for i in 0..pages {
            let page = arena_record::make_guest_page(
                run_id,
                i as u32,
                arena_record::MODE_LOCKED_VA,
                // The guest genuinely does not know its GPA on this path, and 0
                // is the record's own encoding of "unknown". Putting a virtual
                // address here would be a number that *looks* like a GPA and is
                // not — R21's shape in miniature.
                0,
            );
            bytes[i * PAGE..(i + 1) * PAGE].copy_from_slice(&page);
        }
        Ok(())
    }

    /// `None` — this provider does not know its frames.
    ///
    /// It is no longer true that it *could* not. design.md §0.14: on Windows,
    /// `NtQuerySystemInformation(SystemSuperfetchInformation)` with
    /// `SuperfetchPfnQuery` reports the frame behind every page of an ordinary
    /// mapped window in ~330 ms, measured against the live guest and agreeing
    /// with the host's scan on 16384 of 16384 pages, with no privilege the
    /// account does not already hold.
    ///
    /// **That path is now wired, and it does not run through here.** A provider
    /// that can ask its kernel where its pages are calls
    /// [`crate::extent::Extent::locate_by_frames`], which sets `desc.locator` to
    /// [`Locator::Pfns`]; `awe_probe::superfetch::pfn_locator` is the Windows
    /// side of it and `sv_hostd::locate::PfnLocator` the host side. This
    /// provider is Linux and still returns `None`, for the reason at the top of
    /// the module: `/proc/self/pagemap` has returned zeroed frame numbers to
    /// unprivileged processes since 4.0, and reading it as root would make the
    /// guest half need privilege — which is the property that would stop this
    /// being a topology an ordinary application can use.
    ///
    /// The return type is also the wrong shape for the frame path and that is
    /// not an oversight: a [`RunList`] is *file* offsets, which is a fact about
    /// the host's backing object, and a guest that knew those would not need a
    /// locator at all.
    fn frames(&self, _extent: &Extent) -> Option<RunList> {
        None
    }
}

/// A run id that two live extents will not share.
fn fresh_run_id() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // Mix in the pid so two processes starting in the same nanosecond differ,
    // and force a bit so the id is never zero — zero is what a failed hex parse
    // produces on the far side, and `measured-gpu-side.md` records a run lost to
    // exactly that.
    (t.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ ((std::process::id() as u64) << 32)) | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> GuestRamProvider {
        GuestRamProvider::new(Pages::Small)
    }

    #[test]
    fn a_committed_window_is_the_right_size_and_is_pinned() {
        let mut p = provider();
        let e = p.commit(4 * PAGE as u64, 60 * PAGE as u64).unwrap();
        assert_eq!(e.desc.control_pages, 4);
        assert_eq!(e.desc.arena_pages, 60);
        assert_eq!(e.total_len(), 64 * PAGE as u64);
        // `LockedWindow::commit` fails rather than returning an unpinned window,
        // so reaching here at all is the pinning assertion.
        assert_eq!(p.window().expect("commit installs the window").len(), 64 * PAGE);
    }

    #[test]
    fn every_page_carries_its_own_index_after_stamping() {
        let mut p = provider();
        let e = p.commit_and_stamp(PAGE as u64, 15 * PAGE as u64).unwrap();
        let bytes = p.window().unwrap().as_slice();
        for i in 0..16usize {
            let page = &bytes[i * PAGE..(i + 1) * PAGE];
            assert_eq!(&arena_record::tag_at(page, arena_record::OFF_MAGIC), arena_record::MAGIC);
            assert_eq!(arena_record::get_u64(page, arena_record::OFF_RUN_ID), e.desc.run_id);
            assert_eq!(arena_record::get_u32(page, arena_record::OFF_PAGE_INDEX), i as u32);
        }
    }

    /// The host locates by content, so a run id of zero is indistinguishable
    /// from a failed hex parse on the far side — which cost a real run once
    /// already (`measured-gpu-side.md`, "A false negative worth recording").
    #[test]
    fn a_run_id_is_never_zero_and_two_providers_differ() {
        let mut a = provider();
        let mut b = provider();
        let ea = a.commit(PAGE as u64, PAGE as u64).unwrap();
        let eb = b.commit(PAGE as u64, PAGE as u64).unwrap();
        assert_ne!(ea.desc.run_id, 0);
        assert_ne!(eb.desc.run_id, 0);
        assert_ne!(ea.desc.run_id, eb.desc.run_id);
    }

    #[test]
    fn a_ragged_or_empty_extent_is_refused() {
        let mut p = provider();
        assert!(p.commit(100, 4096).is_err(), "not a whole page");
        assert!(p.commit(4096, 0).is_err(), "no arena");
        assert!(p.commit(0, 0).is_err());
    }

    /// This provider does not enumerate its own frames, and on Linux it cannot:
    /// `/proc/self/pagemap` zeroes them for an unprivileged process. design.md
    /// §0.14 measured that *Windows* will report them through
    /// `SuperfetchPfnQuery`, and that path exists —
    /// [`Extent::locate_by_frames`] is where it lands — but it is not this
    /// provider's, so `None` and `Locator::Scan` stay the honest answers here.
    #[test]
    fn the_guest_does_not_claim_to_know_its_frames() {
        let mut p = provider();
        let e = p.commit(PAGE as u64, PAGE as u64).unwrap();
        assert!(p.frames(&e).is_none());
        assert!(e.runs.is_empty(), "the guest side has no run list to give");
    }

    /// Granularity is understated rather than claimed, and what was actually
    /// obtained is read back from the kernel rather than inferred from the flags
    /// that were passed.
    #[test]
    fn granularity_is_understated_and_the_real_page_size_is_read_back() {
        let mut p = provider();
        let e = p.commit(PAGE as u64, PAGE as u64).unwrap();
        assert_eq!(e.desc.granularity, Granularity::Small4K);
        assert!(e.needs_canary());
        assert_eq!(p.observed_page_size(), Some(4096));
    }

    /// Stamping before committing is a caller error and must not be a panic or,
    /// worse, a write to nothing.
    #[test]
    fn stamping_without_a_window_is_refused() {
        let mut p = provider();
        let e = Extent {
            desc: ExtentDesc {
                run_id: 1,
                granularity: Granularity::Small4K,
                control_pages: 1,
                arena_pages: 1,
                locator: Locator::Scan,
            },
            runs: RunList::default(),
        };
        assert!(p.stamp(&e, 1).is_err());
    }
}
