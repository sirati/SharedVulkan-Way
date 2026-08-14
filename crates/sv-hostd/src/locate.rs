//! The scan locator and the stitching attacher: `Mode::Qemu`'s two halves.
//!
//! docs/design.md §5.1, D21: the attaching side finds the pages **by content**, not
//! by being told where they are, because outside AWE "Windows does not report
//! frame numbers to user mode at all". The guest stamps every 4 KiB page of the
//! extent with `{magic, run_id, page_index}` before publishing it, and this
//! module sweeps the backing object for the magic.
//!
//! **That premise was narrowed on 2026-08-13 and this module's role with it**
//! (docs/design.md §0.14, §5.1; `measured-gpu-side.md`, "Asking Windows where the
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
//! *re-location* limitation below, not this module.
//!
//! The host half of the frame path is [`PfnLocator`], which translates the
//! descriptor's frame list through [`crate::qemu::RegionTable::gpa_to_offset`],
//! and [`cross_check`], which is R28 as a function: the two page maps compared
//! index by index, and a disagreement fatal with the page number in it. Both
//! live in this module because they are the other half of the same decision, and
//! reading `scan` without them now gives half the picture.
//!
//! # Why the scan is the locator on every path, including large pages
//!
//! > The scan's cost is linear in *guest RAM*, not in arena size — the sweep
//! > reads all 13312 MiB whatever the window is, and 32× the arena cost 0.5 s
//! > more. … It is also the only locator that **verifies rather than trusts**:
//! > it finds pages by their contents, so it cannot produce R21's silent wrong
//! > answer the way a translated frame list or a computed formula can.
//! > — docs/design.md §5.1
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

/// How the translation went, in the numbers §5.7 prices attaching in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PfnReport {
    pub pages: usize,
    /// Runs in the frame list the guest sent — the guest's own count, in
    /// *frames*. Measured at 26 per 64 MiB on the large-page path and up to one
    /// per page on the 4 KiB one.
    pub frame_runs: usize,
    /// Runs in the *file* after translation. Usually the same number, and
    /// deliberately counted separately: guest-physical contiguity and backing
    /// object contiguity are different claims, and R21 is what happens when the
    /// two are assumed to be one.
    pub file_runs: usize,
    pub first_frame: u64,
}

/// What the two locators made of the same extent (`security-model.md` R28).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossCheck {
    pub pages: usize,
    pub agreed: usize,
    pub disagreed: usize,
    /// The first page index the two disagreed about, with the scan's offset and
    /// the frame list's. One example is worth more than a count here: it is the
    /// number a person needs to work out *how* the undocumented interface moved,
    /// and a uniform shift looks entirely different from a random one.
    pub first_disagreement: Option<(usize, u64, u64)>,
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
    /// The descriptor asked to be located by frame list and did not carry one.
    NotAFrameList,
    /// The frame list is not one frame per page of the extent. The guest and the
    /// descriptor disagree about the size of the thing being described, and
    /// translating the shorter of the two would produce an arena with a hole.
    FrameCountMismatch { frames: u64, pages: u64 },
    /// A reported frame is not guest RAM in the flat view — the PCI hole, or
    /// past the end of memory. **This is R21's exact failure with the mistake
    /// caught**: the arithmetic produced a number, and the region table says
    /// there is nothing there.
    FrameNotRam { index: usize, frame: u64, gpa: u64 },
    /// The scan and the frame list disagree about where a page is. R28: fatal to
    /// the session and loud, because the undocumented interface has either moved
    /// or is being read wrongly, and the two possibilities are indistinguishable
    /// from here.
    CrossCheckFailed(Box<CrossCheck>),
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
            LocateError::NotAFrameList => {
                write!(f, "the descriptor does not carry a frame list to translate")
            }
            LocateError::FrameCountMismatch { frames, pages } => write!(
                f,
                "the frame list names {frames} frames for a {pages}-page extent — the guest and \
                 the descriptor disagree about its size"
            ),
            LocateError::FrameNotRam { index, frame, gpa } => write!(
                f,
                "page {index} is reported on frame 0x{frame:x} (gpa 0x{gpa:012x}), which the \
                 flat view does not call RAM"
            ),
            LocateError::CrossCheckFailed(c) => {
                write!(
                    f,
                    "the scan and the frame list disagree about {} of {} pages",
                    c.disagreed, c.pages
                )?;
                if let Some((i, scan, pfn)) = c.first_disagreement {
                    write!(
                        f,
                        "; first at page {i}: the scan found it at offset 0x{scan:012x}, the \
                         frame list translates it to 0x{pfn:012x}"
                    )?;
                }
                Ok(())
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
// the frame-list locator (docs/design.md §5.1's `PfnLocator` / `SuperfetchPfnLocator`)
// ---------------------------------------------------------------------------

/// Bits between a frame number and a guest-physical address. The PFN database
/// indexes 4 KiB frames whatever the *allocation* granularity was, which is the
/// same reason the guest stamps every 4 KiB page on every path (§5.1).
const FRAME_SHIFT: u32 = 12;

/// Turn the frame list the guest reported into memfd offsets.
///
/// The other half of the arrangement §5.1 settles on: the scan locates an extent
/// once and this re-locates it thereafter, because it reads the kernel's PFN
/// database rather than the extent's contents and therefore does not care that
/// the stamps were overwritten by arena data an instant after they were written.
/// Measured: 0.064 s against the scan's 2.308 s on the same 64 MiB window, with
/// byte-identical offset tables.
///
/// **The translation is read, never computed** (R21). Every frame goes through
/// [`RegionTable::gpa_to_offset`], which is built from QEMU's own `info mtree
/// -f`; a frame that lands in the PCI hole or past the end of RAM is
/// [`LocateError::FrameNotRam`] and not an offset that happens to parse. On this
/// machine an identity mapping would be wrong by exactly a gigabyte above 4 GiB
/// — which is a perfectly valid page, so the error would be silent.
///
/// This deliberately does **not** implement [`sv_arena::extent::ArenaLocator`],
/// for the same reason [`scan`] does not: that trait returns
/// `sv_arena::Error::BadGeometry(&'static str)`, and every failure here is one
/// whose value is the numbers in it. A locator that could only say "bad
/// geometry" about a frame in the PCI hole would be the R21 diagnosis thrown
/// away at the type boundary.
pub struct PfnLocator<'a> {
    pub regions: &'a RegionTable,
}

impl PfnLocator<'_> {
    pub fn locate(&self, desc: &ExtentDesc) -> Result<(PageMap, PfnReport), LocateError> {
        let runs = desc.locator.pfn_runs().ok_or(LocateError::NotAFrameList)?;
        let pages = desc.control_pages + desc.arena_pages;
        let frames = desc.locator.frame_count();
        // Checked before anything is translated, because the interesting
        // failure is a list that is the wrong length rather than one that is
        // wrong about a page: the first is a disagreement about what is being
        // described and the second is a disagreement about where it is.
        if frames != pages {
            return Err(LocateError::FrameCountMismatch { frames, pages });
        }
        let pages = usize::try_from(pages)
            .map_err(|_| LocateError::Io(std::io::Error::other("the extent does not fit")))?;

        let mut offsets = Vec::with_capacity(pages);
        for (index, frame) in desc.locator.frame_numbers().enumerate() {
            // Checked, because a frame number large enough to shift off the top
            // of a u64 would wrap into low memory — an offset that is in RAM,
            // that reads, and that is somebody else's.
            let gpa = frame
                .checked_mul(1 << FRAME_SHIFT)
                .ok_or(LocateError::FrameNotRam { index, frame, gpa: u64::MAX })?;
            let off = self
                .regions
                .gpa_to_offset(gpa)
                .ok_or(LocateError::FrameNotRam { index, frame, gpa })?;
            offsets.push(off);
        }

        let map = PageMap::new(offsets);
        let report = PfnReport {
            pages,
            frame_runs: runs.len(),
            file_runs: map.coalesce().len(),
            first_frame: runs.first().map(|r| r.first_frame).unwrap_or(0),
        };
        Ok((map, report))
    }
}

/// Read a frame list out of the `guest-status.txt` the guest's probe writes.
///
/// A run driven by hand has no cold path — the frame list arrives on the shared
/// folder rather than down a virtio-serial port — and this parses the format
/// `awe-probe`'s `sf-probe` already writes and `arena-host translate` already
/// reads:
///
/// ```text
/// pfn <page index, decimal> <frame, hex> gpa <gpa, hex>
/// ```
///
/// The **frame** is authoritative and the `gpa` field is ignored, deliberately.
/// It is the frame shifted by the guest, so believing it would be believing the
/// guest's arithmetic about the host's address space instead of doing the
/// translation here, against the region table read off `info mtree -f` — which
/// is R21's whole subject.
///
/// The indices must be `0..n` in order. Anything else is a file with a hole in
/// it, and a frame list with a hole translates to an arena with a hole.
pub fn parse_frame_list(text: &str) -> Result<Vec<u64>, String> {
    let mut frames = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() != Some(&"pfn") {
            continue;
        }
        if f.len() < 3 {
            return Err(format!("malformed frame line: {line:?}"));
        }
        let index: usize = f[1].parse().map_err(|_| format!("bad page index in {line:?}"))?;
        let frame = u64::from_str_radix(f[2].trim_start_matches("0x"), 16)
            .map_err(|_| format!("bad frame number in {line:?}"))?;
        if index != frames.len() {
            return Err(format!(
                "the frame list jumps from page {} to page {index} — it has a hole in it",
                frames.len()
            ));
        }
        frames.push(frame);
    }
    if frames.is_empty() {
        return Err("no `pfn` lines: this is not a frame list".into());
    }
    Ok(frames)
}

/// Check the frame list against the scan, page for page. `security-model.md`
/// R28, and the whole reason §5.1 does not delete the scan.
///
/// > the frame list is cross-checked against the content scan **once, at
/// > attach**, on the single occasion when the stamps still exist and the two
/// > locators can be compared page for page. A mismatch is fatal to the session
/// > and loud. After that the query is trusted, because after that there is
/// > nothing left to check it against.
///
/// The comparison is of *page maps* and not of coalesced run lists, deliberately.
/// Two maps that differ can coalesce into run lists that differ in ways a reader
/// has to reconstruct, and the number needed here is which page index
/// disagreed — because a uniform shift (the interface's fields moved) and a
/// single wrong page (a frame moved under us) are different diagnoses and are
/// told apart by exactly that.
pub fn cross_check(scan: &PageMap, pfns: &PageMap) -> Result<CrossCheck, LocateError> {
    let pages = scan.pages().max(pfns.pages());
    let mut c = CrossCheck { pages, agreed: 0, disagreed: 0, first_disagreement: None };
    for i in 0..pages {
        match (scan.offsets().get(i), pfns.offsets().get(i)) {
            (Some(a), Some(b)) if a == b => c.agreed += 1,
            (a, b) => {
                c.disagreed += 1;
                if c.first_disagreement.is_none() {
                    // `u64::MAX` for "this locator had no page here at all",
                    // which is what a length disagreement looks like and is the
                    // value the scan already uses for an unfilled slot.
                    c.first_disagreement =
                        Some((i, a.copied().unwrap_or(u64::MAX), b.copied().unwrap_or(u64::MAX)));
                }
            }
        }
    }
    if c.disagreed > 0 {
        return Err(LocateError::CrossCheckFailed(Box::new(c)));
    }
    Ok(c)
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
/// > attacher ever learns that it was stitched. — docs/design.md §4.3
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

    // ---- the frame-list locator (§0.14, R28) --------------------------------

    /// `testhost`'s flat view: 13312 MiB behind a 3 GiB below-4G split, so RAM
    /// resumes at 4 GiB and the offset lags the GPA by the size of the hole.
    /// These are the numbers `arena-host`'s own translation test uses, and they
    /// are here so the daemon and the probe cannot drift apart about them.
    fn testhost() -> RegionTable {
        RegionTable::new(vec![
            RamAlias { gpa: 0, len: 0xc000_0000, file_offset: 0 },
            RamAlias { gpa: 0x1_0000_0000, len: 0x2_8000_0000, file_offset: 0xc000_0000 },
        ])
    }

    fn pfn_desc(frames: &[u64], control_pages: u64, arena_pages: u64) -> ExtentDesc {
        ExtentDesc {
            locator: sv_transport::cold::Locator::from_frames(frames),
            ..desc(1, control_pages, arena_pages)
        }
    }

    /// The measured case, as arithmetic: a frame list translates to the offsets
    /// the scan would have found, and it does so above 4 GiB where an identity
    /// mapping would be wrong by exactly a gigabyte — a valid page, so the
    /// mistake would be silent (R21).
    #[test]
    fn a_frame_list_translates_to_memfd_offsets_through_the_region_table() {
        let regions = testhost();
        // Three frames below the hole and one above it.
        let frames = [0x10f, 0x110, 0x111, 0x2b6a6d];
        let desc = pfn_desc(&frames, 1, 3);
        let (map, report) = PfnLocator { regions: &regions }.locate(&desc).unwrap();

        assert_eq!(map.pages(), 4);
        assert_eq!(
            map.offsets(),
            &[0x10f000, 0x110000, 0x111000, 0x2_76a6_d000],
            "the low frames are identity-mapped; the high one lags by the 1 GiB hole"
        );
        // Two runs in frame space, and two in the file — but that they agree is
        // a fact about this layout rather than a law, which is why the report
        // carries both.
        assert_eq!(report.frame_runs, 2);
        assert_eq!(report.file_runs, 2);
        assert_eq!(report.pages, 4);
        assert_eq!(report.first_frame, 0x10f);
    }

    /// R21 with the mistake caught rather than made: a frame in the PCI hole is
    /// an error naming the frame, not an offset that reads something else.
    #[test]
    fn a_frame_that_is_not_guest_ram_is_refused_by_name() {
        let regions = testhost();
        // 0xc0000 << 12 = 0xc000_0000, the first byte of the PCI hole.
        let err = PfnLocator { regions: &regions }
            .locate(&pfn_desc(&[0x10f, 0xc_0000], 1, 1))
            .unwrap_err();
        match err {
            LocateError::FrameNotRam { index, frame, gpa } => {
                assert_eq!((index, frame, gpa), (1, 0xc_0000, 0xc000_0000));
            }
            other => panic!("expected FrameNotRam, got {other}"),
        }

        // Past the end of RAM, which is the other way to be in no region.
        assert!(matches!(
            PfnLocator { regions: &regions }.locate(&pfn_desc(&[0x40_0000], 1, 0)),
            Err(LocateError::FrameNotRam { .. })
        ));

        // And a frame number so large that gpa arithmetic wraps into low
        // memory — which would otherwise translate, read, and be somebody
        // else's page.
        assert!(matches!(
            PfnLocator { regions: &regions }.locate(&pfn_desc(&[u64::MAX / 2], 1, 0)),
            Err(LocateError::FrameNotRam { .. })
        ));
    }

    /// A list of the wrong length is a disagreement about how big the extent is,
    /// and translating the shorter of the two would build an arena with a hole
    /// in it that nothing downstream would notice.
    #[test]
    fn a_frame_list_that_does_not_cover_the_extent_is_refused_before_translating() {
        let regions = testhost();
        let err =
            PfnLocator { regions: &regions }.locate(&pfn_desc(&[0x10f, 0x110], 1, 3)).unwrap_err();
        assert!(
            matches!(err, LocateError::FrameCountMismatch { frames: 2, pages: 4 }),
            "got {err}"
        );

        // A descriptor on a *locating* path has no frames to translate at all,
        // and asking must say so rather than produce an empty map.
        let err = PfnLocator { regions: &regions }.locate(&desc(1, 1, 3)).unwrap_err();
        assert!(matches!(err, LocateError::NotAFrameList), "got {err}");
    }

    /// R28 in the shape it is meant to run in: the same extent located both
    /// ways, at attach, while the stamps still exist. The measured result was
    /// byte-identical offset tables over 16384 pages, and this is that
    /// comparison over four.
    #[test]
    fn the_scan_and_the_frame_list_agree_about_the_same_extent() {
        let len = 1 << 20;
        // The pages are scattered and out of order in the file, so agreement is
        // a real result rather than both locators independently producing
        // `i * 4096`.
        let placement = [(0u32, 0x8_0000u64), (1, 0x8_1000), (2, 0x1_0000), (3, 0x1_1000)];
        let f = stamped_file(len, 0xabcd, &placement);
        let regions = RegionTable::new(vec![RamAlias { gpa: 0, len, file_offset: 0 }]);
        let d = desc(0xabcd, 1, 3);
        let (scanned, _) = scan(&f, len, &regions, &d).unwrap();

        // The frame list the guest would have reported for the same pages: this
        // alias is identity-mapped, so the frame is the offset shifted down.
        let frames: Vec<u64> = placement
            .iter()
            .map(|(i, off)| (*i, off >> 12))
            .collect::<std::collections::BTreeMap<_, _>>()
            .into_values()
            .collect();
        let translated = PfnLocator { regions: &regions }
            .locate(&pfn_desc(&frames, 1, 3))
            .unwrap()
            .0;

        let c = cross_check(&scanned, &translated).unwrap();
        assert_eq!(c.agreed, 4);
        assert_eq!(c.disagreed, 0);
        assert_eq!(c.first_disagreement, None);
        assert_eq!(scanned.offsets(), translated.offsets());
    }

    /// The failure R28 exists to catch, and the reason the scan is not deleted:
    /// an undocumented interface that has quietly started answering from a
    /// shifted field. The frame list is confident, self-consistent, translates
    /// cleanly, and names the wrong memory — so nothing but the scan can tell.
    #[test]
    fn a_frame_list_that_disagrees_with_the_scan_is_fatal_and_says_which_page() {
        let len = 1 << 20;
        let placement = [(0u32, 0x8_0000u64), (1, 0x8_1000)];
        let f = stamped_file(len, 5, &placement);
        let regions = RegionTable::new(vec![RamAlias { gpa: 0, len, file_offset: 0 }]);
        let (scanned, _) = scan(&f, len, &regions, &desc(5, 1, 1)).unwrap();

        // Page 1 reported one frame too far along: a single-page error, which is
        // what a frame moving under us looks like.
        let translated = PfnLocator { regions: &regions }
            .locate(&pfn_desc(&[0x80, 0x82], 1, 1))
            .unwrap()
            .0;
        let err = cross_check(&scanned, &translated).unwrap_err();
        match err {
            LocateError::CrossCheckFailed(ref c) => {
                assert_eq!(c.agreed, 1);
                assert_eq!(c.disagreed, 1);
                assert_eq!(c.first_disagreement, Some((1, 0x8_1000, 0x8_2000)));
            }
            ref other => panic!("expected CrossCheckFailed, got {other}"),
        }
        // The message carries the page and both offsets, because a uniform
        // shift and one moved frame are different diagnoses.
        let text = err.to_string();
        assert!(text.contains("first at page 1"), "{text}");
        assert!(text.contains("0x000000081000") && text.contains("0x000000082000"), "{text}");
    }

    /// Two maps of different lengths disagree everywhere past the shorter one,
    /// and the missing side has to be reported as missing rather than as an
    /// offset of zero — which is a real offset.
    #[test]
    fn a_short_map_disagrees_rather_than_comparing_only_what_it_has() {
        let long = PageMap::new(vec![0x1000, 0x2000, 0x3000]);
        let short = PageMap::new(vec![0x1000, 0x2000]);
        let err = cross_check(&long, &short).unwrap_err();
        match err {
            LocateError::CrossCheckFailed(c) => {
                assert_eq!(c.pages, 3);
                assert_eq!(c.agreed, 2);
                assert_eq!(c.first_disagreement, Some((2, 0x3000, u64::MAX)));
            }
            other => panic!("expected CrossCheckFailed, got {other}"),
        }
    }

    /// The exact bytes `sf-probe` writes, header lines and all, because the two
    /// programs meet on a file on a shared folder and nothing else checks that
    /// they still agree about its format.
    #[test]
    fn a_guest_status_file_parses_into_a_frame_list() {
        let text = "run_id=0123456789abcdef\n\
                    mode=large\n\
                    pages=3\n\
                    page_size=4096\n\
                    base_va=0000000200000000\n\
                    locator=superfetch\n\
                    pfn 0 42000 gpa 42000000\n\
                    pfn 1 42001 gpa 42001000\n\
                    pfn 2 99999 gpa 99999000\n";
        assert_eq!(parse_frame_list(text).unwrap(), vec![0x4_2000, 0x4_2001, 0x9_9999]);
    }

    /// The `gpa` column is the guest's own arithmetic on the frame, and doing
    /// the translation from it rather than from the frame would be believing the
    /// guest about the host's address space — which is R21's entire subject. So
    /// a garbled `gpa` must not change the answer, and a garbled *frame* must.
    #[test]
    fn the_frame_is_authoritative_and_the_gpa_column_is_not_believed() {
        let lying = "pfn 0 42000 gpa deadbeef\npfn 1 42001 gpa 0\n";
        assert_eq!(parse_frame_list(lying).unwrap(), vec![0x4_2000, 0x4_2001]);
        assert!(parse_frame_list("pfn 0 zzz gpa 1000\n").is_err());
    }

    #[test]
    fn a_frame_list_with_a_hole_or_no_frames_is_refused() {
        let holed = "pfn 0 100 gpa 100000\npfn 2 102 gpa 102000\n";
        let e = parse_frame_list(holed).unwrap_err();
        assert!(e.contains("page 1 to page 2") || e.contains("hole"), "{e}");
        assert!(parse_frame_list("run_id=1\npages=0\n").is_err(), "a header with no frames");
        assert!(parse_frame_list("pfn 0\n").is_err(), "a truncated line");
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
