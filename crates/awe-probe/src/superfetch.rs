//! Asking Windows where the pages are, instead of sweeping 13 GiB for them.
//!
//! `design.md` §5.1 states, as a measured fact, that **"Windows does not report
//! frame numbers to user mode outside AWE at all"**. That was measured through
//! `QueryWorkingSetEx`, which is the documented API, and it is true of it: the
//! `ReservedUlong` where a frame could hide is zero for every page of every
//! allocator (`measured-gpu-side.md`, "Can we have both?").
//!
//! It is not true of the machine. The PFN database — one entry per physical
//! page, holding what the page is for, which process owns it and at what
//! virtual address — is reachable from user mode through
//! `NtQuerySystemInformation(SystemSuperfetchInformation)` with
//! `SuperfetchPfnQuery`. That is what Sysinternals' RAMMap draws its "Physical
//! Pages" tab from, and it is a query *by frame*: the caller hands in an array
//! of `MMPFN_IDENTITY` with the `PageFrameIndex` filled in and the kernel fills
//! in the rest.
//!
//! So the direction is inverted relative to what the design wanted — one asks
//! "what is at frame N", not "what frame is my page P on" — and the answer is
//! obtained by asking about every frame in the machine and keeping the ones
//! whose virtual address falls in our window. That is O(guest RAM) like the
//! host's scan, but it is a kernel-side walk of a table rather than a 13 GiB
//! read over a memfd, and, unlike the scan, **it does not depend on the pages
//! containing anything in particular.** A window that has been overwritten with
//! pixels locates exactly as well as one that has just been stamped, which is
//! the property §5.1's "an extent must be located before it is used, and cannot
//! be re-located afterwards" gives up.
//!
//! # What it costs
//!
//! Two privileges, both of which an elevated token on this guest already holds:
//! `SeProfileSingleProcessPrivilege` and `SeDebugPrivilege`. No driver, no
//! signing, no code-integrity setting, and — unlike `MEM_LARGE_PAGES` — no
//! `SeLockMemoryPrivilege`. It is an undocumented interface, which is the real
//! cost and is stated as such in the document rather than hidden here.
//!
//! # Structures
//!
//! Declared by hand from `phnt` (`ntpfapi.h`, `ntmmapi.h`), for the same reason
//! [`crate::win`] declares the Win32 surface by hand: a reader checking whether
//! we laid `MMPFN_IDENTITY` out correctly should see the exact widths we used.
//! The bitfields are decoded arithmetically because Rust has none, and the
//! decoding is unit-tested against values taken from the header.

#![allow(non_camel_case_types, non_snake_case)]
#![allow(clippy::upper_case_acronyms)]

use sv_transport::cold::Locator;

/// `MEMORY_FRAME_INFORMATION.UseDescription`, the `MMPFNUSE_*` constants.
pub const MMPFNUSE_PROCESSPRIVATE: u64 = 0;
pub const MMPFNUSE_FILE: u64 = 1;
pub const MMPFNUSE_PAGEFILEMAPPED: u64 = 2;
pub const MMPFNUSE_PAGETABLE: u64 = 3;
pub const MMPFNUSE_PAGEDPOOL: u64 = 4;
pub const MMPFNUSE_NONPAGEDPOOL: u64 = 5;
pub const MMPFNUSE_SYSTEMPTE: u64 = 6;
pub const MMPFNUSE_SESSIONPRIVATE: u64 = 7;
pub const MMPFNUSE_METAFILE: u64 = 8;
pub const MMPFNUSE_AWEPAGE: u64 = 9;
pub const MMPFNUSE_DRIVERLOCKPAGE: u64 = 10;
pub const MMPFNUSE_KERNELSTACK: u64 = 11;

pub fn use_name(u: u64) -> &'static str {
    match u {
        0 => "process-private",
        1 => "file",
        2 => "pagefile-mapped",
        3 => "page-table",
        4 => "paged-pool",
        5 => "nonpaged-pool",
        6 => "system-pte",
        7 => "session-private",
        8 => "metafile",
        9 => "awe",
        10 => "driver-lock",
        11 => "kernel-stack",
        // `phnt`'s list stops at 11. Build 26200 reports **13** for every page
        // of a `VirtualAlloc(MEM_LARGE_PAGES)` window — measured, not guessed —
        // and fills in the owner and the virtual address exactly as it does for
        // an ordinary private page, so nothing downstream needs to care what
        // the code means. It is named here so that a future reader does not
        // conclude the dump returned garbage.
        13 => "large-page (undocumented, build 26200)",
        _ => "unnamed",
    }
}

/// `MMPFN_IDENTITY`, x64: three pointer-sized words.
///
/// `u1` is a union of four bitfield structs that all share the same low nine
/// bits (`UseDescription:4`, `ListDescription:3`, `Cold:1`, `Pinned:1`) and
/// then overlay a 48-bit payload whose meaning depends on the use — the header
/// offers a page directory base, a process id and a file offset as three
/// readings of the same bits, and on build 26200 a private page gets the
/// **process id** ([`MMPFN_IDENTITY::payload48`]). `u2` is the virtual address
/// for everything that is not a mapped file.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct MMPFN_IDENTITY {
    pub u1: u64,
    pub PageFrameIndex: usize,
    pub u2: u64,
}

impl MMPFN_IDENTITY {
    /// A query is an identity with only the frame number filled in.
    pub fn query(pfn: usize) -> Self {
        MMPFN_IDENTITY {
            u1: 0,
            PageFrameIndex: pfn,
            u2: 0,
        }
    }
    /// `MEMORY_FRAME_INFORMATION.UseDescription`, bits 0..4.
    pub fn use_description(&self) -> u64 {
        self.u1 & 0xf
    }
    /// `MEMORY_FRAME_INFORMATION.ListDescription`, bits 4..7.
    pub fn list_description(&self) -> u64 {
        (self.u1 >> 4) & 0x7
    }
    /// The 48-bit payload at bit 9, which the header overlays four ways.
    ///
    /// `phnt` offers `PAGEDIR_INFORMATION.PageDirectoryBase` (a CR3) and
    /// `UNIQUE_PROCESS_INFORMATION.UniqueProcessKey` (a process id) as two
    /// readings of the same bits, and which one a private page gets is not
    /// documented anywhere.
    ///
    /// **Measured, build 26200: it is the process id.** A run whose own
    /// `GetCurrentProcessId` was 17380 saw every page of its window reported
    /// with payload `0x43e4` — 17380. So this is an owner *identifier*, and
    /// the design does not get a CR3 out of it for free.
    pub fn payload48(&self) -> u64 {
        (self.u1 >> 9) & ((1u64 << 48) - 1)
    }
    /// `u2.VirtualAddress`, valid for everything that is not a mapped file.
    pub fn virtual_address(&self) -> u64 {
        self.u2
    }
}

/// One page of a window, as the PFN database reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    /// Index of the page within the window, derived from the virtual address.
    pub index: u32,
    pub pfn: usize,
    /// `payload48` of the identity — measured to be the owning **process id**
    /// (`MMPFN_IDENTITY.u1.e4.UniqueProcessKey`).
    pub owner: u64,
    pub use_description: u64,
}

/// What a dump found for one window, before it is believed.
#[derive(Clone, Debug, Default)]
pub struct Located {
    pub pages: Vec<Found>,
    /// Every distinct `owner` seen in the window's address range, with how many
    /// pages each claimed. More than one entry means two processes hold the
    /// same virtual addresses, which is the normal case across a machine and
    /// the reason the owner has to be part of the filter.
    pub owners: Vec<(u64, usize)>,
    pub frames_examined: usize,
    pub ranges: Vec<(usize, usize)>,
}

/// Reduce a raw list of hits to the pages of one window.
///
/// The virtual address is not by itself an identifier: every process has its
/// own address space and a 64 MiB window at `0x2_0000_0000` is a plausible
/// address in any of them. The owner — the process id the identity carries — is
/// what separates them. [`Located::mine`] is the right tool when the caller
/// knows its own; this is the fallback for when it does not, and it *elects*:
/// the owner holding the most pages of the range wins, and the caller checks
/// the result covers the window exactly.
pub fn elect_owner(hits: &[Found], pages: usize) -> Option<u64> {
    let mut owners: Vec<(u64, usize)> = Vec::new();
    for h in hits {
        match owners.iter_mut().find(|(o, _)| *o == h.owner) {
            Some((_, n)) => *n += 1,
            None => owners.push((h.owner, 1)),
        }
    }
    owners.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    // A window that is not even half there is not a window; refusing is better
    // than electing a stranger with three pages in the same address range.
    owners
        .first()
        .filter(|(_, n)| *n * 2 > pages)
        .map(|(o, _)| *o)
}

impl Located {
    /// The frame list in page order, or the indices that are missing.
    ///
    /// This is the shape `arena-host translate` and `sv-hostd`'s `PfnLocator`
    /// consume, and the check is the same permutation check `locate.rs` makes
    /// of a scan: every index in `0..pages` exactly once. A partial answer is
    /// refused rather than padded, because a frame list with a hole in it
    /// translates to an arena with a hole in it and nothing downstream notices.
    pub fn frames(&self, pages: usize) -> Result<Vec<usize>, String> {
        let mut out = vec![usize::MAX; pages];
        let mut dup = 0usize;
        for f in &self.pages {
            let i = f.index as usize;
            if i >= pages {
                return Err(format!("page index {i} is outside a {pages}-page window"));
            }
            if out[i] != usize::MAX {
                dup += 1;
            }
            out[i] = f.pfn;
        }
        let missing: Vec<usize> = out
            .iter()
            .enumerate()
            .filter(|(_, p)| **p == usize::MAX)
            .map(|(i, _)| i)
            .collect();
        if !missing.is_empty() || dup != 0 {
            return Err(format!(
                "incomplete: {} of {pages} pages missing (first {:?}), {dup} duplicated",
                missing.len(),
                missing.first()
            ));
        }
        Ok(out)
    }

    /// Keep only the pages this process owns.
    ///
    /// Stronger than [`elect_owner`] and the one to use when the caller knows
    /// its own process id, which it always does: the answer is then not a
    /// majority vote but an identity check, and a window that comes back short
    /// is short rather than quietly somebody else's.
    pub fn mine(&self, pid: u64) -> Vec<Found> {
        self.pages
            .iter()
            .copied()
            .filter(|f| f.owner == pid)
            .collect()
    }

    /// The frame list as the cold path carries it: [`Locator::Pfns`].
    ///
    /// The conversion is a widening and a delegation, and it is written that way
    /// on purpose. Run-length encoding the list here as well as in
    /// [`Locator::from_frames`] would be the same arithmetic in two places, and
    /// the failure mode of the two disagreeing is a descriptor that names the
    /// right number of pages and the wrong ones — which nothing downstream of
    /// the locator can detect. [`Located::runs`] stays because the *count* is
    /// what §5.7 prices attaching in and a probe reports it without building a
    /// wire message; the test below holds the two to the same answer.
    pub fn locator(frames: &[usize]) -> Locator {
        let wide: Vec<u64> = frames.iter().map(|f| *f as u64).collect();
        Locator::from_frames(&wide)
    }

    /// Contiguous runs of frames, in the units §5.7 counts attach cost in.
    pub fn runs(frames: &[usize]) -> Vec<(usize, usize)> {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            match runs.last_mut() {
                // A run continues only if it is contiguous in *both* spaces:
                // consecutive window pages sitting on consecutive frames.
                Some((start, len)) if *start + *len == *f && i > 0 => *len += 1,
                _ => runs.push((*f, 1)),
            }
        }
        runs
    }
}

#[cfg(windows)]
pub use sys::*;

#[cfg(windows)]
mod sys {
    use super::*;
    use std::ffi::c_void;
    use std::time::Instant;

    /// `SYSTEM_INFORMATION_CLASS::SystemSuperfetchInformation`.
    const SYSTEM_SUPERFETCH_INFORMATION: u32 = 79;
    const SUPERFETCH_INFORMATION_VERSION: u32 = 45;
    /// `'kuhC'` as a C multi-character constant, which is 0x6B756843 and
    /// therefore the bytes `Chuk` in memory.
    const SUPERFETCH_INFORMATION_MAGIC: u32 = u32::from_le_bytes(*b"Chuk");

    const SUPERFETCH_PFN_QUERY: u32 = 6;
    const SUPERFETCH_MEMORY_RANGES_QUERY: u32 = 17;

    const PF_PFN_PRIO_REQUEST_VERSION: u32 = 1;
    const PF_PHYSICAL_MEMORY_RANGE_INFO_V1_VERSION: u32 = 1;
    const PF_PHYSICAL_MEMORY_RANGE_INFO_V2_VERSION: u32 = 2;

    #[repr(C)]
    struct SUPERFETCH_INFORMATION {
        Version: u32,
        Magic: u32,
        InfoClass: u32,
        Data: *mut c_void,
        Length: u32,
    }

    /// `PF_PFN_PRIO_REQUEST` up to but not including the identity array.
    ///
    /// `MemInfo` is a `SYSTEM_MEMORY_LIST_INFORMATION`: five `SIZE_T`, two
    /// arrays of eight, and one more — 22 words, 176 bytes. It is output the
    /// caller does not need, and it is declared as an opaque block rather than
    /// spelled out because getting its *size* right is the only thing that
    /// matters to the identities that follow it.
    #[repr(C)]
    #[derive(Default)]
    struct PfnPrioHeader {
        Version: u32,
        RequestFlags: u32,
        PfnCount: usize,
        MemInfo: [usize; 22],
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PF_PHYSICAL_MEMORY_RANGE {
        BasePfn: usize,
        PageCount: usize,
    }

    unsafe extern "system" {
        fn NtQuerySystemInformation(
            SystemInformationClass: u32,
            SystemInformation: *mut c_void,
            SystemInformationLength: u32,
            ReturnLength: *mut u32,
        ) -> i32;
    }

    fn superfetch(class: u32, data: *mut c_void, len: u32) -> i32 {
        let mut sfi = SUPERFETCH_INFORMATION {
            Version: SUPERFETCH_INFORMATION_VERSION,
            Magic: SUPERFETCH_INFORMATION_MAGIC,
            InfoClass: class,
            Data: data,
            Length: len,
        };
        let mut ret = 0u32;
        // SAFETY: `sfi` is a correctly sized `SUPERFETCH_INFORMATION` and
        // `data`/`len` describe a buffer the caller owns for the call's
        // duration. The kernel writes only inside it.
        unsafe {
            NtQuerySystemInformation(
                SYSTEM_SUPERFETCH_INFORMATION,
                (&raw mut sfi).cast(),
                size_of::<SUPERFETCH_INFORMATION>() as u32,
                &raw mut ret,
            )
        }
    }

    /// The machine's populated physical memory ranges, as frame numbers.
    ///
    /// Wanted for two reasons: it bounds the frames worth asking about — on a
    /// 13 GiB guest the top of RAM is above 4 GiB but the PCI hole in the
    /// middle is not RAM and asking about it is wasted work — and it is the
    /// cheapest possible check that this interface answers at all on this
    /// build, before a 100 MiB allocation is made on the strength of it.
    pub fn memory_ranges() -> Result<Vec<(usize, usize)>, String> {
        // V2 first: it is what a current kernel returns, and the two differ in
        // the header, so guessing wrong reads the range count out of a flags
        // field. Fall back to V1 rather than assume.
        for (version, header) in [
            (PF_PHYSICAL_MEMORY_RANGE_INFO_V2_VERSION, 16usize),
            (PF_PHYSICAL_MEMORY_RANGE_INFO_V1_VERSION, 8usize),
        ] {
            let mut buf = vec![0u8; header + 512 * size_of::<PF_PHYSICAL_MEMORY_RANGE>()];
            buf[0..4].copy_from_slice(&version.to_le_bytes());
            let st = superfetch(
                SUPERFETCH_MEMORY_RANGES_QUERY,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
            );
            if st != 0 {
                continue;
            }
            let count = if header == 16 {
                usize::from_le_bytes(buf[8..16].try_into().unwrap())
            } else {
                u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize
            };
            if count == 0 || count > 512 {
                continue;
            }
            let mut out = Vec::with_capacity(count);
            for i in 0..count {
                let o = header + i * 16;
                out.push((
                    usize::from_le_bytes(buf[o..o + 8].try_into().unwrap()),
                    usize::from_le_bytes(buf[o + 8..o + 16].try_into().unwrap()),
                ));
            }
            return Ok(out);
        }
        Err("SuperfetchMemoryRangesQuery refused both V2 and V1".into())
    }

    /// Ask the kernel to fill in the identities of a batch of frames.
    ///
    /// Chunked rather than one call for the whole machine: 3.4 million
    /// identities is an 82 MiB buffer, and a single allocation that large is a
    /// worse failure mode than a loop. The chunk size is the caller's.
    pub fn pfn_query(ids: &mut [MMPFN_IDENTITY]) -> Result<(), String> {
        let header = size_of::<PfnPrioHeader>();
        let mut buf = vec![0u8; header + size_of_val(ids)];
        {
            let h = PfnPrioHeader {
                Version: PF_PFN_PRIO_REQUEST_VERSION,
                RequestFlags: 0,
                PfnCount: ids.len(),
                MemInfo: [0; 22],
            };
            // SAFETY: `h` is `repr(C)` and plain data; `buf` is at least
            // `header` bytes long by construction.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&raw const h).cast::<u8>(),
                    buf.as_mut_ptr(),
                    header,
                );
            }
        }
        for (i, id) in ids.iter().enumerate() {
            let o = header + i * size_of::<MMPFN_IDENTITY>();
            buf[o + 8..o + 16].copy_from_slice(&id.PageFrameIndex.to_le_bytes());
        }
        let st = superfetch(
            SUPERFETCH_PFN_QUERY,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
        );
        if st != 0 {
            return Err(format!("SuperfetchPfnQuery: NTSTATUS 0x{st:08x}"));
        }
        for (i, id) in ids.iter_mut().enumerate() {
            let o = header + i * size_of::<MMPFN_IDENTITY>();
            id.u1 = u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
            id.PageFrameIndex = usize::from_le_bytes(buf[o + 8..o + 16].try_into().unwrap());
            id.u2 = u64::from_le_bytes(buf[o + 16..o + 24].try_into().unwrap());
        }
        Ok(())
    }

    /// Is this interface answering at all on this build?
    ///
    /// Called before anything expensive so that a refusal is reported as a
    /// refusal rather than as an empty window.
    pub fn available() -> Result<Vec<(usize, usize)>, String> {
        let ranges = memory_ranges()?;
        let mut probe = [MMPFN_IDENTITY::query(ranges[0].0 + 1)];
        pfn_query(&mut probe)?;
        Ok(ranges)
    }

    /// Locate a window by asking about every frame in the machine.
    ///
    /// Returns the hits *and* the owners seen, because the interesting failure
    /// is not "found nothing", it is "found the right count under two different
    /// page directory bases", and a caller that cannot see that cannot judge
    /// its own answer.
    pub fn locate(base: usize, pages: usize, chunk: usize) -> Result<Located, String> {
        let ranges = memory_ranges()?;
        let lo = base as u64;
        let hi = lo + (pages * arena_record::PAGE) as u64;
        let mut out = Located {
            ranges: ranges.clone(),
            ..Default::default()
        };
        let mut ids = vec![MMPFN_IDENTITY::default(); chunk];
        for (rbase, rcount) in &ranges {
            let mut done = 0usize;
            while done < *rcount {
                let n = chunk.min(rcount - done);
                for (k, id) in ids[..n].iter_mut().enumerate() {
                    *id = MMPFN_IDENTITY::query(rbase + done + k);
                }
                pfn_query(&mut ids[..n])?;
                out.frames_examined += n;
                done += n;
                for id in &ids[..n] {
                    let va = id.virtual_address();
                    if va < lo || va >= hi {
                        continue;
                    }
                    let u = id.use_description();
                    // A mapped file's `u2` is a file object, not an address, so
                    // it must not be read as one. Everything else in the union
                    // is a virtual address.
                    if u == MMPFNUSE_FILE || u == MMPFNUSE_METAFILE {
                        continue;
                    }
                    out.pages.push(Found {
                        index: ((va - lo) / arena_record::PAGE as u64) as u32,
                        pfn: id.PageFrameIndex,
                        owner: id.payload48(),
                        use_description: u,
                    });
                }
            }
        }
        let mut owners: Vec<(u64, usize)> = Vec::new();
        for h in &out.pages {
            match owners.iter_mut().find(|(o, _)| *o == h.owner) {
                Some((_, n)) => *n += 1,
                None => owners.push((h.owner, 1)),
            }
        }
        owners.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        out.owners = owners;
        Ok(out)
    }

    /// [`locate`], timed, with the elected owner's pages kept.
    pub fn locate_timed(
        base: usize,
        pages: usize,
        chunk: usize,
    ) -> Result<(Located, u128), String> {
        let t = Instant::now();
        let mut l = locate(base, pages, chunk)?;
        let ms = t.elapsed().as_millis();
        // Our own process id is the exact filter; the election is only the
        // fallback for a caller that does not have one.
        let pid = std::process::id() as u64;
        let mine = l.mine(pid);
        l.pages = if mine.is_empty() {
            match elect_owner(&l.pages, pages) {
                Some(o) => l.pages.iter().copied().filter(|f| f.owner == o).collect(),
                None => l.pages,
            }
        } else {
            mine
        };
        Ok((l, ms))
    }

    /// Locate a window and return **one frame per page, in page order**, or say
    /// why not.
    ///
    /// This is [`locate_timed`] with the two checks a caller must not be
    /// trusted to remember applied: the pages are reduced to this process's own
    /// (or, failing that, to the elected owner's), and the result is checked as
    /// a permutation of `0..pages` by [`Located::frames`] before any of it is
    /// believed. A window that comes back short is an error with the missing
    /// count in it rather than a shorter list, because a frame list with a hole
    /// translates into an arena with a hole and nothing downstream notices.
    pub fn locate_frames(base: usize, pages: usize, chunk: usize) -> Result<(Vec<usize>, u128), String> {
        let (located, ms) = locate_timed(base, pages, chunk)?;
        Ok((located.frames(pages)?, ms))
    }

    /// The same, as the thing that actually crosses the cold path.
    ///
    /// design.md §0.14: this is the call that turns "Windows will tell us where
    /// the pages are" into a descriptor the host can act on. What comes back is
    /// run-length encoded — 26 runs per 64 MiB on the large-page path, up to one
    /// per page on the 4 KiB one — and it is the *whole* of what the guest has
    /// to say about where its memory is. No handle, no descriptor, no pointer:
    /// a frame number is an integer, which is the property that lets it cross a
    /// VM boundary at all.
    pub fn pfn_locator(base: usize, pages: usize, chunk: usize) -> Result<(Locator, u128), String> {
        let (frames, ms) = locate_frames(base, pages, chunk)?;
        Ok((Located::locator(&frames), ms))
    }

    /// Ask the same question the other way: bound the work by looking only at
    /// the frames a previous answer named.
    ///
    /// This is what re-location costs once the window has been located once —
    /// `pages` identities rather than three and a half million — and it is the
    /// number that decides whether a periodic re-check is affordable. It
    /// answers "are these frames still ours, at these addresses", which is
    /// exactly the R26 canary's question with a definite answer instead of a
    /// heuristic one.
    pub fn recheck(base: usize, frames: &[usize]) -> Result<(usize, u128), String> {
        let t = Instant::now();
        let mut ids: Vec<MMPFN_IDENTITY> =
            frames.iter().map(|f| MMPFN_IDENTITY::query(*f)).collect();
        pfn_query(&mut ids)?;
        let lo = base as u64;
        let mut ok = 0usize;
        for (i, id) in ids.iter().enumerate() {
            if id.virtual_address() == lo + (i * arena_record::PAGE) as u64 {
                ok += 1;
            }
        }
        Ok((ok, t.elapsed().as_millis()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bitfield decode, against a value laid out by hand from the header:
    /// `UseDescription:4`, `ListDescription:3`, `Cold:1`, `Pinned:1`, then the
    /// 48-bit payload.
    #[test]
    fn the_identity_bitfields_decode_where_the_header_puts_them() {
        let payload: u64 = 0x0000_1234_5678_9abc;
        let id = MMPFN_IDENTITY {
            u1: MMPFNUSE_PROCESSPRIVATE | (MMPFNLIST_ACTIVE << 4) | (payload << 9),
            PageFrameIndex: 0x4_2000,
            u2: 0x0000_0200_0000_1000,
        };
        assert_eq!(id.use_description(), MMPFNUSE_PROCESSPRIVATE);
        assert_eq!(id.list_description(), MMPFNLIST_ACTIVE);
        assert_eq!(id.payload48(), payload);
        assert_eq!(id.virtual_address(), 0x0000_0200_0000_1000);
    }

    const MMPFNLIST_ACTIVE: u64 = 6;

    /// An AWE page reports a different use, and the payload is then a process
    /// id rather than a page directory base. The decode must not care.
    #[test]
    fn an_awe_identity_decodes_with_the_same_shifts() {
        let id = MMPFN_IDENTITY {
            u1: MMPFNUSE_AWEPAGE | (1234u64 << 9),
            PageFrameIndex: 7,
            u2: 0,
        };
        assert_eq!(id.use_description(), MMPFNUSE_AWEPAGE);
        assert_eq!(id.payload48(), 1234);
    }

    fn hit(index: u32, pfn: usize, owner: u64) -> Found {
        Found {
            index,
            pfn,
            owner,
            use_description: MMPFNUSE_PROCESSPRIVATE,
        }
    }

    /// Two processes with the same virtual addresses is the normal case, and
    /// the majority owner is the one holding the whole window.
    #[test]
    fn the_owner_holding_the_window_is_elected_over_a_stranger_in_the_same_range() {
        let mut hits: Vec<Found> = (0..64)
            .map(|i| hit(i, 0x1000 + i as usize, 0xaaaa))
            .collect();
        hits.push(hit(3, 0x9999, 0xbbbb));
        hits.push(hit(4, 0x999a, 0xbbbb));
        assert_eq!(elect_owner(&hits, 64), Some(0xaaaa));
    }

    /// A range in which nobody holds a majority is not a window, and electing
    /// the largest minority would hand back somebody else's frames.
    #[test]
    fn no_owner_is_elected_when_none_holds_the_window() {
        let hits = vec![hit(0, 1, 0xaaaa), hit(1, 2, 0xbbbb), hit(2, 3, 0xcccc)];
        assert_eq!(elect_owner(&hits, 64), None);
    }

    #[test]
    fn a_complete_dump_becomes_a_frame_list_in_page_order() {
        let l = Located {
            pages: vec![hit(2, 30, 1), hit(0, 10, 1), hit(1, 20, 1)],
            ..Default::default()
        };
        assert_eq!(l.frames(3).unwrap(), vec![10, 20, 30]);
    }

    /// The permutation check `locate.rs` makes of a scan, made here of a dump:
    /// a hole is an error with a number in it, not a shorter list.
    #[test]
    fn a_dump_with_a_hole_is_refused_rather_than_padded() {
        let l = Located {
            pages: vec![hit(0, 10, 1), hit(2, 30, 1)],
            ..Default::default()
        };
        let e = l.frames(3).unwrap_err();
        assert!(e.contains("1 of 3 pages missing"), "{e}");
    }

    #[test]
    fn an_index_past_the_window_is_an_error_and_not_a_panic() {
        let l = Located {
            pages: vec![hit(9, 10, 1)],
            ..Default::default()
        };
        assert!(l.frames(3).unwrap_err().contains("outside"));
    }

    /// Runs are what §5.7 counts attach cost in: consecutive window pages on
    /// consecutive frames. A window that is contiguous but *reversed* is not
    /// one run, and a naive "is this frame one more than the last" would say it
    /// was.
    #[test]
    fn runs_count_the_mappings_an_attacher_would_make() {
        assert_eq!(Located::runs(&[10, 11, 12, 20, 21]), vec![(10, 3), (20, 2)]);
        assert_eq!(
            Located::runs(&[12, 11, 10]),
            vec![(12, 1), (11, 1), (10, 1)]
        );
        assert_eq!(Located::runs(&[]), vec![]);
        assert_eq!(Located::runs(&[5]), vec![(5, 1)]);
    }

    /// The probe's run counter and the wire's run-length encoder must be the
    /// same function seen twice, because they are what the two sides of the
    /// handshake each believe about the same window. If they ever parted, the
    /// guest would report a run count the host could not reproduce from the
    /// descriptor it was sent — and the descriptor is the one that gets mapped.
    #[test]
    fn the_run_count_a_probe_reports_is_the_run_count_the_wire_carries() {
        for frames in [
            vec![10usize, 11, 12, 20, 21],
            // Reversed: contiguous by the loose test, three runs by the right
            // one. This is the case that would silently map an extent backwards.
            vec![12, 11, 10],
            vec![5],
            vec![],
            // A 64 MiB large-page window as measured: one block of frames.
            (0..16384).collect(),
        ] {
            let counted = Located::runs(&frames);
            let carried = Located::locator(&frames);
            let runs = carried.pfn_runs().expect("locator() always produces Pfns");
            assert_eq!(runs.len(), counted.len(), "run counts differ for {frames:?}");
            for (r, (first, len)) in runs.iter().zip(&counted) {
                assert_eq!((r.first_frame, r.frames), (*first as u64, *len as u64));
            }
            // …and the wire form expands back to the frame list it came from,
            // so nothing is lost between the two spellings.
            assert_eq!(
                carried.frame_numbers().collect::<Vec<_>>(),
                frames.iter().map(|f| *f as u64).collect::<Vec<_>>()
            );
        }
    }

    /// A dump is turned into a descriptor's locator by way of the permutation
    /// check, not around it: the frame list that crosses the wire is the one
    /// [`Located::frames`] has already vouched for.
    #[test]
    fn a_dump_becomes_a_locator_only_after_it_survives_the_permutation_check() {
        let good = Located {
            pages: vec![hit(1, 41, 7), hit(0, 40, 7), hit(2, 99, 7)],
            ..Default::default()
        };
        let frames = good.frames(3).unwrap();
        let l = Located::locator(&frames);
        assert_eq!(l.frame_count(), 3);
        assert_eq!(l.pfn_runs().unwrap().len(), 2, "40,41 coalesce; 99 does not");

        let holed = Located { pages: vec![hit(0, 40, 7), hit(2, 99, 7)], ..Default::default() };
        assert!(holed.frames(3).is_err(), "a hole must not reach the wire at all");
    }

    /// Knowing one's own process id turns the majority vote into an identity
    /// check, and a stranger's pages in the same address range then simply are
    /// not ours — even when the stranger has more of them.
    #[test]
    fn our_own_pages_are_selected_by_process_id_and_not_by_majority() {
        let mut pages = vec![hit(0, 10, 4242), hit(1, 11, 4242)];
        pages.extend((0..9).map(|i| hit(i, 900 + i as usize, 9999)));
        let l = Located {
            pages,
            ..Default::default()
        };
        assert_eq!(l.mine(4242).len(), 2);
        assert_eq!(elect_owner(&l.pages, 2), Some(9999));
    }
}
