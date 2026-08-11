//! The four ways to get arena-shaped memory out of Windows, and one owner type
//! that keeps whichever one worked alive.
//!
//! These are the allocators `docs/measured-gpu-side.md` measured — the runs in
//! guest-physical space, the driver accepting all four, and the recommendation
//! to use `MEM_LARGE_PAGES` with 4 KiB locked pages as the per-slab fallback
//! all refer to exactly this code. Anything measuring "the arena" has to
//! allocate through here or it is measuring something else.

use crate::win;
use crate::win::*;
use arena_record as rec;
use std::ffi::c_void;
use std::time::{Duration, Instant};

const PAGE: usize = rec::PAGE;

/// How the window was obtained, which is the difference between measuring R19
/// and merely working around it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// `AllocateUserPhysicalPages` — we know the guest-physical frames.
    Awe,
    /// `AllocateUserPhysicalPages2` with `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE`
    /// — frames *and* 2 MiB contiguity, if the kernel will do it. Measured: it
    /// will not.
    Awe2Large,
    /// `VirtualAlloc(MEM_LARGE_PAGES)` — no frame numbers, but every 2 MiB is
    /// physically contiguous and non-pageable by construction. The chosen path.
    LargePages,
    /// `VirtualAlloc` + `VirtualLock` — ordinary committed memory, pinned into
    /// the working set. The frames are unknown to us, so the host has to find
    /// the pages by searching for their contents. The fallback.
    LockedVa,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Awe => "awe",
            Mode::Awe2Large => "awe2-large",
            Mode::LargePages => "large-pages",
            Mode::LockedVa => "locked-va",
        }
    }
    pub fn code(self) -> u32 {
        match self {
            Mode::Awe => rec::MODE_AWE,
            Mode::Awe2Large => rec::MODE_AWE2_LARGE,
            Mode::LargePages => rec::MODE_LARGE_PAGES,
            Mode::LockedVa => rec::MODE_LOCKED_VA,
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "awe" => Mode::Awe,
            "awe2" | "awe2-large" => Mode::Awe2Large,
            "large" | "large-pages" => Mode::LargePages,
            "locked" | "locked-va" => Mode::LockedVa,
            _ => return None,
        })
    }
    /// Does this allocator need the size to be a multiple of the large page?
    pub fn needs_large_granularity(self) -> bool {
        matches!(self, Mode::LargePages | Mode::Awe2Large)
    }
}

pub struct AweWindow {
    pub base: *mut c_void,
    pub pfns: Vec<usize>,
}

pub struct LockedWindow {
    pub base: *mut c_void,
    /// Large-page allocations are non-pageable by construction and were never
    /// `VirtualLock`ed, so unlocking them on the way out would just fail.
    pub locked: bool,
}

/// A window that is alive, whichever allocator produced it.
///
/// It owns the release path, which the step-0 probe had to spell out at every
/// early return. A probe that panics or returns early now still gives the pages
/// back — on a machine somebody is using, that matters more than it would in a
/// test harness.
pub struct Window {
    pub mode: Mode,
    pub base: *mut c_void,
    /// Pages actually held. An AWE allocation may be short, so this is not
    /// necessarily what was asked for.
    pub pages: usize,
    awe: Option<AweWindow>,
    locked: Option<LockedWindow>,
}

/// Round a page count up to the large-page granularity if any of the allocators
/// in `order` needs it. A large-page allocation is a multiple of
/// `GetLargePageMinimum()` or it is nothing, and quietly measuring a different
/// size than the one asked for is worse than saying so.
pub fn round_pages(order: &[Mode], pages: usize) -> usize {
    let large_min = unsafe { win::GetLargePageMinimum() };
    if !order.iter().any(|m| m.needs_large_granularity()) || large_min <= PAGE {
        return pages;
    }
    let per = large_min / PAGE;
    if pages.is_multiple_of(per) {
        return pages;
    }
    let rounded = pages.div_ceil(per) * per;
    crate::log!("rounding {pages} pages up to {rounded} ({large_min}-byte granularity)");
    rounded
}

impl Window {
    pub fn size(&self) -> usize {
        self.pages * PAGE
    }

    /// The window as bytes. Sound because we own the reservation for the whole
    /// lifetime of `self` and nothing else in the process maps it.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.base as *mut u8, self.pages * PAGE) }
    }

    /// The guest-physical frames, when the allocator reported them. `None` for
    /// the two `VirtualAlloc` paths, which is why the host has to scan.
    pub fn pfns(&self) -> Option<&[usize]> {
        self.awe.as_ref().map(|w| w.pfns.as_slice())
    }

    /// Try each allocator in turn and keep the first that works.
    pub fn acquire(order: &[Mode], pages: usize) -> Result<Window, String> {
        let size = pages * PAGE;
        let mut last = String::from("no allocator was tried");
        for m in order {
            let attempt = match m {
                Mode::Awe => try_awe(pages, false).map(Acquired::Awe),
                Mode::Awe2Large => try_awe(pages, true).map(Acquired::Awe),
                Mode::LargePages => try_large(size).map(Acquired::Va),
                Mode::LockedVa => try_locked(size).map(Acquired::Va),
            };
            match attempt {
                Ok(Acquired::Awe(w)) => {
                    crate::log!(
                        "{} window: base=0x{:x} pages={} — frames known, first pfn 0x{:x}",
                        m.as_str(),
                        w.base as usize,
                        w.pfns.len(),
                        w.pfns[0]
                    );
                    // A short allocation is a documented outcome: the window is
                    // then whatever we actually own, and every count downstream
                    // has to follow it rather than what was asked for.
                    let got = w.pfns.len();
                    if got != pages {
                        crate::log!("window is {got} pages ({} bytes) after the short allocation", got * PAGE);
                    }
                    return Ok(Window {
                        mode: *m,
                        base: w.base,
                        pages: got,
                        awe: Some(w),
                        locked: None,
                    });
                }
                Ok(Acquired::Va(w)) => {
                    crate::log!("{} window: base=0x{:x} size={size}", m.as_str(), w.base as usize);
                    return Ok(Window {
                        mode: *m,
                        base: w.base,
                        pages,
                        awe: None,
                        locked: Some(w),
                    });
                }
                Err(e) => {
                    crate::log!("{} unavailable: {e}", m.as_str());
                    last = e;
                }
            }
        }
        Err(last)
    }

    /// Write the `SVARENA1` record into every page, so the host can find them
    /// by content and so a page that turns up at the wrong offset says so.
    pub fn stamp(&mut self, run_id: u64) {
        let mode = self.mode;
        let pages = self.pages;
        let gpas: Vec<u64> = (0..pages)
            .map(|p| self.pfns().map(|f| (f[p] as u64) << 12).unwrap_or(0))
            .collect();
        let w = self.as_mut_slice();
        for p in 0..pages {
            let page = rec::make_guest_page(run_id, p as u32, mode.code(), gpas[p]);
            w[p * PAGE..(p + 1) * PAGE].copy_from_slice(&page);
        }
        crate::log!("stamped {pages} pages with SVARENA1/{run_id:016x}");
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let size = self.pages * PAGE;
        unsafe {
            if let Some(w) = self.awe.take() {
                MapUserPhysicalPages(w.base, w.pfns.len() as ULONG_PTR, std::ptr::null());
                VirtualFree(w.base, 0, MEM_RELEASE);
                let mut n: ULONG_PTR = w.pfns.len();
                FreeUserPhysicalPages(GetCurrentProcess(), &mut n, w.pfns.as_ptr());
            }
            if let Some(w) = self.locked.take() {
                if w.locked {
                    VirtualUnlock(w.base, size);
                }
                VirtualFree(w.base, 0, MEM_RELEASE);
            }
        }
    }
}

/// What an allocator handed back, so the caller can keep it alive without
/// caring which one succeeded.
pub enum Acquired {
    Awe(AweWindow),
    Va(LockedWindow),
}

/// `AllocateUserPhysicalPages` + `MapUserPhysicalPages`.
///
/// This is the arrangement `design.md` §5.1 wants: the frame numbers come back
/// to user mode, and inside a VM they are guest-physical, which is what makes
/// the host able to find the pages at all.
///
/// With `large = true` it goes through `AllocateUserPhysicalPages2` and asks for
/// `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE`, which is the only documented way to
/// get frame numbers *and* 2 MiB physical contiguity at once. The count
/// semantics of that flag are not spelled out anywhere legible, so both
/// readings are tried and whichever the kernel accepts is reported.
pub fn try_awe(pages: usize, large: bool) -> Result<AweWindow, String> {
    unsafe {
        let proc = GetCurrentProcess();
        let mut want: ULONG_PTR = pages;
        let mut pfns: Vec<ULONG_PTR> = vec![0; pages];

        let ok = if large {
            let Some((alloc2, from)) = win::allocate_user_physical_pages2() else {
                return Err(format!(
                    "AllocateUserPhysicalPages2 is exported by none of {:?}",
                    win::ALLOC2_DLLS
                ));
            };
            crate::log!("AllocateUserPhysicalPages2 resolved from {from}");
            let mut ep =
                MEM_EXTENDED_PARAMETER::attribute_flags(MEM_EXTENDED_PARAMETER_NONPAGED_LARGE);
            let r = alloc2(proc, &mut want, pfns.as_mut_ptr(), &mut ep, 1);
            if r == 0 {
                // Reading two: the count is in large pages, not small ones.
                let e1 = GetLastError();
                let large_min = GetLargePageMinimum();
                let per = if large_min > PAGE { large_min / PAGE } else { 512 };
                let mut want2: ULONG_PTR = pages / per;
                if want2 > 0 {
                    let r2 = alloc2(proc, &mut want2, pfns.as_mut_ptr(), &mut ep, 1);
                    if r2 != 0 {
                        return Err(format!(
                            "AllocateUserPhysicalPages2(NONPAGED_LARGE) took a large-page \
                             count ({want2} of {}) but not a small-page one (error {e1}) — \
                             the frame array then needs interpreting before use",
                            pages / per
                        ));
                    }
                }
                return Err(format!(
                    "AllocateUserPhysicalPages2(NONPAGED_LARGE) failed: {e1} \
                     (small-page count), {} (large-page count)",
                    GetLastError()
                ));
            }
            r
        } else {
            AllocateUserPhysicalPages(proc, &mut want, pfns.as_mut_ptr())
        };

        if ok == 0 {
            let e = GetLastError();
            let why = if e == ERROR_PRIVILEGE_NOT_HELD {
                " (ERROR_PRIVILEGE_NOT_HELD — SeLockMemoryPrivilege is not in this token)"
            } else {
                ""
            };
            return Err(format!("AllocateUserPhysicalPages failed: {e}{why}"));
        }
        if want != pages {
            crate::log!("asked for {pages} frames, got {want}");
        }
        if want == 0 {
            return Err("AllocateUserPhysicalPages returned zero frames".into());
        }
        let pages = want;
        pfns.truncate(want);

        // How contiguous are the frames we were given? The number the host-side
        // run count reflects, measured here without needing the host.
        let runs = 1 + pfns.windows(2).filter(|w| w[1] != w[0] + 1).count();
        crate::log!(
            "{} frames, {runs} contiguous runs ({:.1} pages per run)",
            pfns.len(),
            pfns.len() as f64 / runs as f64
        );

        let base = VirtualAlloc(
            std::ptr::null_mut(),
            pages * PAGE,
            MEM_RESERVE | MEM_PHYSICAL,
            PAGE_READWRITE,
        );
        if base.is_null() {
            let e = GetLastError();
            let mut n: ULONG_PTR = pages;
            FreeUserPhysicalPages(proc, &mut n, pfns.as_ptr());
            return Err(format!("VirtualAlloc(MEM_PHYSICAL) failed: {e}"));
        }
        if MapUserPhysicalPages(base, pages as ULONG_PTR, pfns.as_ptr()) == 0 {
            let e = GetLastError();
            VirtualFree(base, 0, MEM_RELEASE);
            let mut n: ULONG_PTR = pages;
            FreeUserPhysicalPages(proc, &mut n, pfns.as_ptr());
            return Err(format!("MapUserPhysicalPages failed: {e}"));
        }
        Ok(AweWindow { base, pfns })
    }
}

/// Ordinary committed memory, locked into the working set.
///
/// The fallback when large pages are unavailable. It gives up the frame numbers
/// — the host has to find these pages by searching guest RAM for their contents
/// — but the pages are still guest RAM, still pinned, and still the thing the
/// driver is asked to import. `VirtualLock` needs no privilege, only working-set
/// quota, which is why it is reachable from an account that cannot use AWE.
pub fn try_locked(size: usize) -> Result<LockedWindow, String> {
    unsafe {
        let base = VirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        );
        if base.is_null() {
            return Err(format!("VirtualAlloc failed: {}", GetLastError()));
        }
        // VirtualLock fails with ERROR_WORKING_SET_QUOTA unless the working set
        // is big enough to hold the region as well as everything else.
        let (mut lo, mut hi) = (0usize, 0usize);
        if GetProcessWorkingSetSize(GetCurrentProcess(), &mut lo, &mut hi) != 0 {
            SetProcessWorkingSetSize(GetCurrentProcess(), lo + size * 2, hi + size * 2);
        }
        // Touch first: locking uncommitted-but-reserved pages is meaningless and
        // the driver will want them present anyway.
        std::ptr::write_bytes(base as *mut u8, rec::FILL, size);
        if VirtualLock(base, size) == 0 {
            let e = GetLastError();
            crate::log!("VirtualLock failed: {e} — continuing with unlocked memory");
        }
        Ok(LockedWindow { base, locked: true })
    }
}

/// `VirtualAlloc(MEM_LARGE_PAGES)`.
///
/// Each large page is one physically contiguous 2 MiB block and is never paged,
/// so a 64 MiB window is 22 runs in guest-physical space instead of the ~1471
/// that 4 KiB pages measured. The costs are that the size must be a multiple of
/// `GetLargePageMinimum()`, that it needs `SeLockMemoryPrivilege`, and that it
/// can simply fail when the guest's physical memory is too fragmented.
pub fn try_large(size: usize) -> Result<LockedWindow, String> {
    unsafe {
        let base = VirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES,
            PAGE_READWRITE,
        );
        if base.is_null() {
            let e = GetLastError();
            let why = match e {
                ERROR_PRIVILEGE_NOT_HELD => {
                    " (ERROR_PRIVILEGE_NOT_HELD — large pages need SeLockMemoryPrivilege too)"
                }
                1450 => " (ERROR_NO_SYSTEM_RESOURCES — no contiguous physical blocks left)",
                _ => "",
            };
            return Err(format!("VirtualAlloc(MEM_LARGE_PAGES) failed: {e}{why}"));
        }
        std::ptr::write_bytes(base as *mut u8, rec::FILL, size);
        Ok(LockedWindow {
            base,
            locked: false,
        })
    }
}

/// Does the large-page allocation work every time, or was it luck?
///
/// A slab allocator that intermittently cannot get its memory is worse than one
/// that is merely slow. Each attempt is released immediately: nothing is held,
/// and the guest is a production machine.
pub fn stress_large(size: usize, tries: usize) {
    let mut ok = 0usize;
    let mut worst = 0u32;
    let mut slowest = Duration::ZERO;
    for _ in 0..tries {
        let t = Instant::now();
        match try_large(size) {
            Ok(w) => {
                ok += 1;
                slowest = slowest.max(t.elapsed());
                unsafe { VirtualFree(w.base, 0, MEM_RELEASE) };
            }
            Err(_) => worst = unsafe { GetLastError() },
        }
    }
    crate::log!(
        "large-page stress: {ok}/{tries} allocations of {} MiB succeeded, \
         slowest {:?}, last failure error {worst}",
        size / (1024 * 1024),
        slowest
    );
}

/// Ask Windows what it will tell user mode about the pages of the window.
///
/// Measured answer: nothing page-specific. It is kept for the `LargePage` bit,
/// which lets a probe assert what it actually got rather than what it asked for
/// — a slab that silently fell back to 4 KiB pages has different properties
/// (R26), and every measurement should name its real allocator.
pub fn wsex_report(base: *mut c_void, size: usize) -> String {
    let large_min = unsafe { win::GetLargePageMinimum() };
    let mut out = String::new();

    for (label, stride) in [
        ("4KiB-stride", PAGE),
        (
            "large-page-stride",
            if large_min > PAGE { large_min } else { PAGE },
        ),
    ] {
        if stride > size {
            continue;
        }
        let n = size / stride;
        let mut info: Vec<win::PSAPI_WORKING_SET_EX_INFORMATION> = (0..n)
            .map(|i| win::PSAPI_WORKING_SET_EX_INFORMATION {
                VirtualAddress: (base as usize + i * stride) as *mut c_void,
                VirtualAttributes: 0,
            })
            .collect();

        let bytes = std::mem::size_of::<win::PSAPI_WORKING_SET_EX_INFORMATION>() * n;
        let t = Instant::now();
        let ok = unsafe {
            QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr() as *mut c_void,
                bytes as u32,
            )
        };
        let elapsed = t.elapsed();
        if ok == 0 {
            crate::log!("QueryWorkingSetEx({label}) FAILED: {}", unsafe {
                GetLastError()
            });
            continue;
        }

        let bits: Vec<win::WsBits> = info
            .iter()
            .map(|i| win::WsBits(i.VirtualAttributes))
            .collect();
        let valid = bits.iter().filter(|b| b.valid()).count();
        let large = bits.iter().filter(|b| b.large_page()).count();
        let locked = bits.iter().filter(|b| b.locked()).count();
        let shared = bits.iter().filter(|b| b.shared()).count();
        let bad = bits.iter().filter(|b| b.bad()).count();
        let hi = bits.iter().map(|b| b.reserved_high()).max().unwrap_or(0);
        let mid = bits.iter().map(|b| b.reserved_mid()).max().unwrap_or(0);
        let distinct_hi = {
            let mut v: Vec<u64> = bits.iter().map(|b| b.reserved_high()).collect();
            v.sort_unstable();
            v.dedup();
            v.len()
        };

        let line = format!(
            "wsex[{label}]: n={n} in {elapsed:?} ({:.1} ns/entry)\n  \
             valid={valid} largePage={large} locked={locked} shared={shared} bad={bad} \
             shareCount={} node={} prot=0x{:x}\n  \
             raw[0]=0x{:016x} raw[1]=0x{:016x}\n  \
             reserved bits 24..30 max={mid}, bits 32..63 max=0x{hi:x} over {distinct_hi} distinct value(s)",
            elapsed.as_nanos() as f64 / n as f64,
            bits[0].share_count(),
            bits[0].node(),
            bits[0].win32_protection(),
            bits[0].0,
            if bits.len() > 1 { bits[1].0 } else { 0 },
        );
        crate::log!("{line}");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Did we actually get large pages? The `LargePage` bit is the only honest
/// answer — `MEM_LARGE_PAGES` succeeding is a claim about the request, and a
/// measurement should not report the request.
pub fn is_large_page_backed(base: *mut c_void) -> Option<bool> {
    let mut info = [win::PSAPI_WORKING_SET_EX_INFORMATION {
        VirtualAddress: base,
        VirtualAttributes: 0,
    }];
    let bytes = std::mem::size_of::<win::PSAPI_WORKING_SET_EX_INFORMATION>();
    let ok = unsafe {
        QueryWorkingSetEx(
            GetCurrentProcess(),
            info.as_mut_ptr() as *mut c_void,
            bytes as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(win::WsBits(info[0].VirtualAttributes).large_page())
}
