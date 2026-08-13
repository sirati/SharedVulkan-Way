//! The scan locator and the stitching attacher: `Mode::Qemu`'s two halves.
//!
//! design.md §5.1, D21: the attaching side finds the pages **by content**, not
//! by being told where they are, because outside AWE "Windows does not report
//! frame numbers to user mode at all". The guest stamps every 4 KiB page of the
//! extent with `{magic, run_id, page_index}` before publishing it, and this
//! module sweeps the backing object for the magic.
//!
//! **That premise was narrowed on 2026-08-13 and this module's role with it**
//! (design.md §0.14, §5.1; `measured-gpu-side.md`, "Asking Windows where the
//! pages are"). Windows reports no frames through `QueryWorkingSetEx`, which is
//! what was measured — but it does through
//! `NtQuerySystemInformation(SystemSuperfetchInformation)` /
//! `SuperfetchPfnQuery`, for any allocator that produces ordinary mapped pages,
//! including the guest driver's own `vkAllocateMemory` memory, in ~330 ms
//! whatever the window is, and *without needing the pages to still contain the
//! stamp*. The scan agreed with it on 16384 of 16384 pages.
//!
//! The scan is therefore no longer the only locator — but it stays, and stays
//! on every path, because it is the only one that **verifies**: it is the
//! cross-check that the undocumented interface is telling the truth, run once at
//! attach while the stamps still exist (R28). What the query removes is the
//! *re-location* limitation below, not this module. The host half of the frame
//! path — `PfnLocator`, translating a run list through
//! [`crate::qemu::RegionTable::gpa_to_offset`] — is implemented in `arena-host`
//! and is not yet in this daemon.
//!
//! # Why the scan is the locator on every path, including large pages
//!
//! > The scan's cost is linear in *guest RAM*, not in arena size — the sweep
//! > reads all 13312 MiB whatever the window is, and 32× the arena cost 0.5 s
//! > more. … It is also the only locator that **verifies rather than trusts**:
//! > it finds pages by their contents, so it cannot produce R21's silent wrong
//! > answer the way a translated frame list or a computed formula can.
//! > — design.md §5.1
//!
//! Measured: 1.8 s at 64 MiB, 2.02 s at 1 GiB, 2.32 s at 2 GiB.
//!
//! # What this adds over `arena-host scan`, which is its seed
//!
//! Three things, and each of them is a way the probe could report a wrong
//! answer that a daemon may not:
//!
//! 1. **Offsets that are not guest RAM are skipped.** The backing object is
//!    sized to the guest's RAM but the flat view maps it in pieces; the holes
//!    between the pieces are never written by the guest, so a stamp found there
//!    is a stale one from a previous boot of a previous run. `arena-host`
//!    sweeps the whole file and would accept it.
//! 2. **The page map is validated as a permutation.** Every index in
//!    `0..pages` exactly once. A duplicate index means two pages claim to be
//!    the same page of the extent — which is what a stale stamp elsewhere in
//!    RAM looks like — and the honest response is to refuse, not to take the
//!    first.
//! 3. **The extent's page count comes from the descriptor**, so "found fewer
//!    than expected" is an error with numbers in it rather than a short map
//!    that attaches successfully and is missing its tail.
//!
//! # `unsafe`
//!
//! The attacher, and only the attacher: reserving a `PROT_NONE` range and
//! `mmap(MAP_FIXED)`ing runs into it has no safe spelling. The scan is a
//! `pread` loop over a `Vec<u8>`.

use crate::qemu::{RegionTable, read_at};
use arena_record as rec;
use sv_arena::extent::{PageMap, RunList};
use sv_transport::cold::ExtentDesc;

/// How the scan went, in the numbers a human needs to judge it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub bytes_swept: u64,
    pub pages_expected: usize,
    pub pages_found: usize,
    /// Pages carrying the magic but a *different* run id. Not an error — a
    /// previous run's stamps survive until the memory is reused — but a large
    /// number next to a short map is the signature of scanning for the wrong
    /// run id.
    pub other_runs: usize,
    /// Indices claimed by more than one page. Always fatal.
    pub duplicates: usize,
    /// Offsets that carried a matching stamp but are not RAM in the flat view.
    pub outside_ram: usize,
}

#[derive(Debug)]
pub enum LocateError {
    Io(std::io::Error),
    /// The map is short. The numbers are in the report.
    Incomplete(Box<ScanReport>),
    /// Two pages claim the same index — a stale stamp, or two live runs sharing
    /// a run id. Refusing is the only safe answer: taking either one produces an
    /// arena in which one page is somebody else's.
    Duplicate(Box<ScanReport>),
    /// A page index beyond the extent. The guest and the descriptor disagree
    /// about how big the extent is.
    IndexOutOfRange { index: u32, pages: usize },
}

impl std::fmt::Display for LocateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocateError::Io(e) => write!(f, "reading the backing object: {e}"),
            LocateError::Incomplete(r) => write!(
                f,
                "located {} of {} pages after sweeping {} MiB ({} pages carried the magic \
                 with a different run id, {} were outside guest RAM)",
                r.pages_found,
                r.pages_expected,
                r.bytes_swept >> 20,
                r.other_runs,
                r.outside_ram
            ),
            LocateError::Duplicate(r) => write!(
                f,
                "{} page index(es) were claimed twice — a stale stamp or a repeated run id; \
                 refusing rather than choosing",
                r.duplicates
            ),
            LocateError::IndexOutOfRange { index, pages } => {
                write!(f, "a page claims index {index} in a {pages}-page extent")
            }
        }
    }
}

impl std::error::Error for LocateError {}

impl From<std::io::Error> for LocateError {
    fn from(e: std::io::Error) -> Self {
        LocateError::Io(e)
    }
}

/// Sweep the backing object for one run's stamped pages.
///
/// `regions` is what makes this a daemon rather than a probe: offsets that the
/// flat view does not call RAM are not the guest's memory and a stamp there is
/// not the guest's stamp.
pub fn scan(
    file: &std::fs::File,
    file_len: u64,
    regions: &RegionTable,
    desc: &ExtentDesc,
) -> Result<(PageMap, ScanReport), LocateError> {
    let pages = (desc.control_pages + desc.arena_pages) as usize;
    // 32 MiB, which is what the step-0 scan used and what its 1.8 s at 64 MiB
    // and 2.32 s at 2 GiB were measured with. Not tuned since, because the cost
    // is dominated by touching 13 GiB of page cache.
    const CHUNK: usize = 32 << 20;
    let mut buf = vec![0u8; CHUNK];
    let mut offsets = vec![u64::MAX; pages];
    let mut report = ScanReport {
        bytes_swept: 0,
        pages_expected: pages,
        pages_found: 0,
        other_runs: 0,
        duplicates: 0,
        outside_ram: 0,
    };

    let mut base = 0u64;
    while base < file_len {
        let want = CHUNK.min((file_len - base) as usize);
        let got = read_at(file, base, &mut buf[..want])?;
        if got == 0 {
            break;
        }
        report.bytes_swept += got as u64;
        for p in (0..got).step_by(rec::PAGE) {
            if p + rec::PAGE > got {
                break;
            }
            let page = &buf[p..p + rec::PAGE];
            if &rec::tag_at(page, rec::OFF_MAGIC) != rec::MAGIC {
                continue;
            }
            if rec::get_u64(page, rec::OFF_RUN_ID) != desc.run_id {
                report.other_runs += 1;
                continue;
            }
            let off = base + p as u64;
            if !regions.is_ram_offset(off) {
                report.outside_ram += 1;
                continue;
            }
            let index = rec::get_u32(page, rec::OFF_PAGE_INDEX);
            let i = index as usize;
            if i >= pages {
                return Err(LocateError::IndexOutOfRange { index, pages });
            }
            if offsets[i] != u64::MAX {
                report.duplicates += 1;
                continue;
            }
            offsets[i] = off;
            report.pages_found += 1;
        }
        base += got as u64;
    }

    if report.duplicates > 0 {
        return Err(LocateError::Duplicate(Box::new(report)));
    }
    if report.pages_found != pages {
        return Err(LocateError::Incomplete(Box::new(report)));
    }
    Ok((PageMap::new(offsets), report))
}

// ---------------------------------------------------------------------------
// the attacher
// ---------------------------------------------------------------------------

/// A virtually contiguous view of an extent whose pages are scattered in the
/// backing object.
///
/// > `sv-hostd` … reserves one `PROT_NONE` range per region and
/// > `mmap(MAP_FIXED)`s each run into it. The result is virtually contiguous
/// > with identical internal offsets — the §1 invariant — and nothing above the
/// > attacher ever learns that it was stitched. — design.md §4.3
///
/// The reservation is one `mmap` of the whole extent; each run then replaces its
/// slice of it. Reserving first matters: mapping the runs one at a time and
/// hoping the kernel puts them adjacently is not a thing that works, and the
/// failure would be a view whose offsets are wrong rather than an error.
pub struct StitchedView {
    base: *mut std::ffi::c_void,
    len: usize,
    runs: usize,
}

// The pointer is to a shared mapping which is only ever read and written through
// bounds-checked accessors; the same reasoning `sv_transport::region` uses.
unsafe impl Send for StitchedView {}
unsafe impl Sync for StitchedView {}

impl StitchedView {
    pub fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.base
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// How many `mmap(MAP_FIXED)` calls it took. The measured figure this is
    /// compared against: 1471 per 64 MiB on the 4 KiB path, 22 on large pages,
    /// and **1 per GiB**.
    pub fn runs(&self) -> usize {
        self.runs
    }

    /// Read bytes out of the stitched view, by offset within the extent.
    ///
    /// Present so a caller can check the §1 invariant — that extent offset *n*
    /// is the same byte on both sides — without another `unsafe` block
    /// somewhere else.
    pub fn read(&self, offset: u64, dst: &mut [u8]) -> Option<()> {
        let end = offset.checked_add(dst.len() as u64)?;
        if end > self.len as u64 {
            return None;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                (self.base as *const u8).add(offset as usize),
                dst.as_mut_ptr(),
                dst.len(),
            )
        };
        Some(())
    }

    pub fn write(&self, offset: u64, src: &[u8]) -> Option<()> {
        let end = offset.checked_add(src.len() as u64)?;
        if end > self.len as u64 {
            return None;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                src.as_ptr(),
                (self.base as *mut u8).add(offset as usize),
                src.len(),
            )
        };
        Some(())
    }
}

impl Drop for StitchedView {
    fn drop(&mut self) {
        // One `munmap` for the whole reservation, however many runs went into
        // it — §5.1's `detach(view)`.
        unsafe { libc::munmap(self.base, self.len) };
    }
}

/// Stitch `runs` of `file` into one contiguous view.
pub fn attach(file: &std::fs::File, runs: &RunList) -> std::io::Result<StitchedView> {
    use std::os::fd::AsRawFd;
    let total = runs.bytes();
    let len = usize::try_from(total)
        .map_err(|_| std::io::Error::other("the extent does not fit in a usize"))?;

    // Reserve. PROT_NONE so that a bug which leaves a hole faults immediately
    // rather than reading zeroes — the same reasoning as R21's, applied to our
    // own mistakes instead of QEMU's layout.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    let view = StitchedView { base, len, runs: runs.len() };

    for r in runs.runs() {
        let at = unsafe { (base as *mut u8).add(r.extent_offset as usize) } as *mut std::ffi::c_void;
        let got = unsafe {
            libc::mmap(
                at,
                r.len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                file.as_raw_fd(),
                r.file_offset as i64,
            )
        };
        if got == libc::MAP_FAILED {
            // `view`'s Drop unmaps the reservation, including whatever was
            // already stitched into it. Failing half way through must not leak
            // a partly-built arena.
            return Err(std::io::Error::last_os_error());
        }
        if got != at {
            return Err(std::io::Error::other(
                "MAP_FIXED did not honour the address — the reservation was not respected",
            ));
        }
    }
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::{RamAlias, RegionTable};
    use std::io::Write;
    use sv_transport::cold::{Granularity, Locator};
    use sv_transport::region::PAGE;

    fn desc(run_id: u64, control_pages: u64, arena_pages: u64) -> ExtentDesc {
        ExtentDesc {
            run_id,
            granularity: Granularity::Small4K,
            control_pages,
            arena_pages,
            locator: Locator::Scan,
        }
    }

    /// A fake "guest RAM" file with an extent stamped into it at the offsets
    /// given, in whatever order. That the *file* order and the *extent* order
    /// differ is the whole point: the scan has to reconstruct the extent from
    /// the indices, not from where it found things.
    fn stamped_file(len: u64, run_id: u64, placement: &[(u32, u64)]) -> std::fs::File {
        let mut f = tempfile();
        f.set_len(len).unwrap();
        for &(index, off) in placement {
            let page = rec::make_guest_page(run_id, index, rec::MODE_LARGE_PAGES, 0);
            use std::os::unix::fs::FileExt;
            f.write_at(&page, off).unwrap();
        }
        f.flush().unwrap();
        f
    }

    fn tempfile() -> std::fs::File {
        let path = std::env::temp_dir()
            .join(format!("sv-hostd-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        // Unlink immediately: the fd keeps it alive and nothing has to clean up
        // after a failing test.
        let _ = std::fs::remove_file(&path);
        f
    }

    fn all_ram(len: u64) -> RegionTable {
        RegionTable::new(vec![RamAlias { gpa: 0, len, file_offset: 0 }])
    }

    #[test]
    fn the_scan_reconstructs_an_extent_scattered_through_the_file() {
        let len = 1 << 20;
        // Four pages of extent, deliberately out of order and not adjacent.
        let placement = [(2u32, 0x1_0000u64), (0, 0x8_0000), (3, 0x1_1000), (1, 0x8_1000)];
        let f = stamped_file(len, 0xabcd, &placement);
        let (map, report) = scan(&f, len, &all_ram(len), &desc(0xabcd, 1, 3)).unwrap();
        assert_eq!(report.pages_found, 4);
        assert_eq!(report.duplicates, 0);
        assert_eq!(map.pages(), 4);
        // Extent order, not file order: pages 0,1 are at 0x80000/0x81000 and
        // pages 2,3 at 0x10000/0x11000, so the coalescer sees two runs.
        let runs = map.coalesce();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs.runs()[0].file_offset, 0x8_0000);
        assert_eq!(runs.runs()[0].extent_offset, 0);
        assert_eq!(runs.runs()[1].file_offset, 0x1_0000);
        assert_eq!(runs.runs()[1].extent_offset, 2 * PAGE as u64);
        runs.validate(4 * PAGE as u64).unwrap();
    }

    #[test]
    fn a_short_map_is_an_error_with_the_numbers_in_it() {
        let len = 1 << 20;
        let f = stamped_file(len, 7, &[(0, 0), (1, 4096)]);
        let err = scan(&f, len, &all_ram(len), &desc(7, 1, 3)).unwrap_err();
        match err {
            LocateError::Incomplete(r) => {
                assert_eq!(r.pages_found, 2);
                assert_eq!(r.pages_expected, 4);
            }
            other => panic!("expected Incomplete, got {other}"),
        }
    }

    /// A stale stamp from a previous run of the *same* run id claiming an index
    /// that a live page also claims. Taking either is an arena with somebody
    /// else's page in it; the only safe answer is to refuse.
    #[test]
    fn a_duplicate_index_is_refused_rather_than_resolved() {
        let len = 1 << 20;
        let f = stamped_file(len, 9, &[(0, 0), (1, 4096), (1, 0x4_0000)]);
        let err = scan(&f, len, &all_ram(len), &desc(9, 1, 1)).unwrap_err();
        assert!(matches!(err, LocateError::Duplicate(_)), "got {err}");
    }

    /// The daemon's addition to the probe: a stamp outside the flat view's RAM
    /// is not the guest's memory, and counting it would produce an arena page
    /// backed by something the guest cannot see.
    #[test]
    fn a_stamp_outside_guest_ram_is_not_counted() {
        let len = 1 << 20;
        // Page 1 is stamped at 0x40000, which the region table says is a hole.
        let f = stamped_file(len, 5, &[(0, 0), (1, 0x4_0000)]);
        let regions = RegionTable::new(vec![
            RamAlias { gpa: 0, len: 0x1000, file_offset: 0 },
            RamAlias { gpa: 0x10_0000, len: 0x1000, file_offset: 0x10_0000 },
        ]);
        let err = scan(&f, len, &regions, &desc(5, 1, 1)).unwrap_err();
        match err {
            LocateError::Incomplete(r) => {
                assert_eq!(r.pages_found, 1);
                assert_eq!(r.outside_ram, 1, "the hole page was seen and rejected");
            }
            other => panic!("expected Incomplete, got {other}"),
        }
    }

    #[test]
    fn another_runs_stamps_are_counted_and_not_taken() {
        let len = 1 << 20;
        let mut placement = vec![(0u32, 0u64), (1, 4096)];
        let f = stamped_file(len, 1, &placement);
        // Overwrite one page with a different run's stamp.
        {
            use std::os::unix::fs::FileExt;
            let page = rec::make_guest_page(2, 0, rec::MODE_LARGE_PAGES, 0);
            f.write_at(&page, 0x2_0000).unwrap();
        }
        placement.clear();
        let (_, report) = scan(&f, len, &all_ram(len), &desc(1, 1, 1)).unwrap();
        assert_eq!(report.pages_found, 2);
        assert_eq!(report.other_runs, 1);
    }

    #[test]
    fn an_index_past_the_end_of_the_extent_is_refused() {
        let len = 1 << 20;
        let f = stamped_file(len, 3, &[(0, 0), (99, 4096)]);
        let err = scan(&f, len, &all_ram(len), &desc(3, 1, 1)).unwrap_err();
        assert!(matches!(err, LocateError::IndexOutOfRange { index: 99, pages: 2 }), "got {err}");
    }

    /// The invariant of §1, as an experiment: after stitching, extent offset *n*
    /// is the byte the guest wrote at extent offset *n*, whatever the file
    /// offsets were.
    #[test]
    fn stitching_makes_the_extent_offsets_the_same_on_both_sides() {
        let len = 1 << 20;
        // Reverse order in the file, so a stitcher that ignored `extent_offset`
        // would produce a view that is exactly backwards — and a view that is
        // merely *shuffled* would still read as "some bytes are wrong", which is
        // why the marker below carries the index.
        let placement = [(0u32, 0x3000u64), (1, 0x2000), (2, 0x1000), (3, 0x0000)];
        let f = stamped_file(len, 0x1234, &placement);
        let (map, _) = scan(&f, len, &all_ram(len), &desc(0x1234, 1, 3)).unwrap();
        let runs = map.coalesce();
        assert_eq!(runs.len(), 4, "reversed pages cannot coalesce");

        let view = attach(&f, &runs).unwrap();
        assert_eq!(view.len(), 4 * PAGE);
        assert_eq!(view.runs(), 4);
        for i in 0..4u32 {
            let mut magic = [0u8; 8];
            view.read(i as u64 * PAGE as u64 + rec::OFF_MAGIC as u64, &mut magic).unwrap();
            assert_eq!(&magic, rec::MAGIC);
            let mut idx = [0u8; 4];
            view.read(i as u64 * PAGE as u64 + rec::OFF_PAGE_INDEX as u64, &mut idx).unwrap();
            assert_eq!(u32::from_le_bytes(idx), i, "extent offset {i} holds page {i}");
        }

        // And the view is writable *through* to the file, which is what makes
        // it an arena rather than a snapshot.
        view.write(PAGE as u64, b"HOSTWRT1").unwrap();
        let mut back = [0u8; 8];
        use std::os::unix::fs::FileExt;
        // Page 1 lives at file offset 0x2000.
        f.read_at(&mut back, 0x2000).unwrap();
        assert_eq!(&back, b"HOSTWRT1");
    }

    #[test]
    fn reading_past_the_end_of_a_stitched_view_is_none_rather_than_a_fault() {
        let len = 1 << 20;
        let f = stamped_file(len, 1, &[(0, 0)]);
        let (map, _) = scan(&f, len, &all_ram(len), &desc(1, 1, 0)).unwrap();
        let view = attach(&f, &map.coalesce()).unwrap();
        let mut buf = [0u8; 16];
        assert!(view.read(PAGE as u64 - 8, &mut buf).is_none());
        assert!(view.read(u64::MAX, &mut buf).is_none());
        assert!(view.read(0, &mut buf).is_some());
    }
}
