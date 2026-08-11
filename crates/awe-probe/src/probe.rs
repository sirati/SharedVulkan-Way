//! Step 0, guest half: is the arena's memory real on both sides at once?
//!
//! `vk-probe` established that this driver will import *ordinary* process
//! memory as `VkDeviceMemory` and hand back the same virtual address
//! (`docs/measured-gpu-side.md`). That retires R9 and nothing else. The
//! question this program exists for is R20: will the same driver accept memory
//! that is simultaneously
//!
//!   (a) guest RAM,
//!   (b) an AWE allocation whose physical frames we chose, and
//!   (c) mapped by a Linux process on the other side of the hypervisor?
//!
//! No prior art combines all three, so it has to be measured. The program runs
//! in the Windows guest, allocates the window, stamps every page with a record
//! that identifies it, and then hands off to the Linux side through files on
//! the virtio-fs share, waiting to be told when to look again. The host proves
//! it can see the pages by writing into them; this program proves the writes
//! arrived, imports the same pages into Vulkan, and checks the host's bytes are
//! visible through the `VkDeviceMemory` mapping.
//!
//! It is a console program with no window and no GPU work beyond an import, so
//! it is safe to run over ssh in a guest whose interactive session belongs to
//! somebody else.
//!
//! Cross-compiled from the Linux dev shell:
//!   cargo build -p awe-probe --release --target x86_64-pc-windows-gnu

use crate::win;
use crate::win::*;
use arena_record as rec;
use ash::vk;
use std::ffi::{CStr, c_void};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PAGE: usize = rec::PAGE;

/// How the window was obtained, which is the difference between measuring R19
/// and merely working around it.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// `AllocateUserPhysicalPages` — we know the guest-physical frames.
    Awe,
    /// `AllocateUserPhysicalPages2` with `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE`
    /// — frames *and* 2 MiB contiguity, if the kernel will do it.
    Awe2Large,
    /// `VirtualAlloc(MEM_LARGE_PAGES)` — no frame numbers, but every 2 MiB is
    /// physically contiguous and non-pageable by construction.
    LargePages,
    /// `VirtualAlloc` + `VirtualLock` — ordinary committed memory, pinned into
    /// the working set. The frames are unknown to us, so the host has to find
    /// the pages by searching for their contents.
    LockedVa,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Awe => "awe",
            Mode::Awe2Large => "awe2-large",
            Mode::LargePages => "large-pages",
            Mode::LockedVa => "locked-va",
        }
    }
    fn code(self) -> u32 {
        match self {
            Mode::Awe => rec::MODE_AWE,
            Mode::Awe2Large => rec::MODE_AWE2_LARGE,
            Mode::LargePages => rec::MODE_LARGE_PAGES,
            Mode::LockedVa => rec::MODE_LOCKED_VA,
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "awe" => Mode::Awe,
            "awe2" | "awe2-large" => Mode::Awe2Large,
            "large" | "large-pages" => Mode::LargePages,
            "locked" | "locked-va" => Mode::LockedVa,
            _ => return None,
        })
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

macro_rules! log {
    ($($a:tt)*) => {{
        println!("[{}] {}", now_ms(), format!($($a)*));
        let _ = std::io::stdout().flush();
    }};
}

pub fn run() {
    let mut workdir = PathBuf::from("Z:\\probe");
    let mut pages: usize = 256;
    // Which allocators to try, in order. The default reproduces the original
    // step-0 run; `--mode` pins one so a measurement can name its mechanism.
    let mut order: Vec<Mode> = vec![Mode::Awe, Mode::LockedVa];
    let mut stress = 0usize;
    let mut stress_only = false;
    let mut wsex_only = false;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--workdir" => {
                workdir = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--pages" => {
                pages = args[i + 1].parse().expect("--pages takes a number");
                i += 2;
            }
            "--mode" => {
                order = args[i + 1]
                    .split(',')
                    .map(|m| Mode::parse(m).unwrap_or_else(|| panic!("unknown mode {m}")))
                    .collect();
                i += 2;
            }
            // Repeat the large-page allocation N times to see whether it is
            // reliable or merely lucky. Nothing is held between attempts.
            "--stress" => {
                stress = args[i + 1].parse().expect("--stress takes a number");
                i += 2;
            }
            // Answer only the reliability question and exit, without holding
            // anything or involving the host.
            "--stress-only" => {
                stress_only = true;
                i += 1;
            }
            // Allocate, ask Windows what it will say about the pages, and stop.
            // No host, no Vulkan, nothing held.
            "--wsex-only" => {
                wsex_only = true;
                i += 1;
            }
            other => panic!("unknown argument {other}"),
        }
    }
    let mut size = pages * PAGE;

    // A run id makes every claim falsifiable: a page that matches was written
    // by *this* run, not left over from a previous one.
    let run_id = (now_ms() as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(17)
        ^ (std::process::id() as u64) << 32;

    log!("awe-probe: run_id=0x{run_id:016x} pages={pages} ({size} bytes) workdir={workdir:?}");

    // ---- 1. the privilege AWE needs ----------------------------------------
    let priv_state = match win::enable_privilege("SeLockMemoryPrivilege") {
        Ok(true) => "enabled".to_string(),
        Ok(false) => "not-held".to_string(),
        Err(e) => format!("error: {e}"),
    };
    log!("SeLockMemoryPrivilege: {priv_state}");

    // ---- 2. the window -----------------------------------------------------
    let large_min = unsafe { win::GetLargePageMinimum() };
    log!("GetLargePageMinimum: {large_min} bytes");

    // A large-page allocation is a multiple of the large page or it is nothing,
    // so round the window up rather than quietly measuring a different size.
    let wants_large = order
        .iter()
        .any(|m| matches!(m, Mode::LargePages | Mode::Awe2Large));
    if wants_large && large_min > PAGE {
        let per = large_min / PAGE;
        if !pages.is_multiple_of(per) {
            let rounded = pages.div_ceil(per) * per;
            log!("rounding {pages} pages up to {rounded} ({large_min}-byte granularity)");
            pages = rounded;
            size = pages * PAGE;
        }
    }

    if stress > 0 {
        stress_large(size, stress);
    }
    if stress_only {
        return;
    }

    let mut awe_state: Option<AweWindow> = None;
    let mut lock_state: Option<LockedWindow> = None;
    let mut chosen: Option<(Mode, *mut c_void)> = None;

    for m in &order {
        let attempt = match m {
            Mode::Awe => try_awe(pages, false).map(Acquired::Awe),
            Mode::Awe2Large => try_awe(pages, true).map(Acquired::Awe),
            Mode::LargePages => try_large(size).map(Acquired::Va),
            Mode::LockedVa => try_locked(size).map(Acquired::Va),
        };
        match attempt {
            Ok(Acquired::Awe(w)) => {
                log!(
                    "{} window: base=0x{:x} pages={} — frames known, first pfn 0x{:x}",
                    m.as_str(),
                    w.base as usize,
                    w.pfns.len(),
                    w.pfns[0]
                );
                chosen = Some((*m, w.base));
                awe_state = Some(w);
                break;
            }
            Ok(Acquired::Va(w)) => {
                log!(
                    "{} window: base=0x{:x} size={}",
                    m.as_str(),
                    w.base as usize,
                    size
                );
                chosen = Some((*m, w.base));
                lock_state = Some(w);
                break;
            }
            Err(e) => log!("{} unavailable: {e}", m.as_str()),
        }
    }

    let Some((mode, base)) = chosen else {
        log!("NO ALLOCATOR WORKED — nothing to measure");
        return;
    };

    // An AWE allocator may hand back fewer frames than asked for; the window is
    // then whatever we actually own, and every count below has to follow it.
    if let Some(w) = &awe_state
        && w.pfns.len() != pages
    {
        pages = w.pfns.len();
        size = pages * PAGE;
        log!("window is {pages} pages ({size} bytes) after the short allocation");
    }

    let window: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(base as *mut u8, size) };

    // ---- 3. stamp every page ----------------------------------------------
    for p in 0..pages {
        let gpa = awe_state
            .as_ref()
            .map(|w| (w.pfns[p] as u64) << 12)
            .unwrap_or(0);
        let page = rec::make_guest_page(run_id, p as u32, mode.code(), gpa);
        window[p * PAGE..(p + 1) * PAGE].copy_from_slice(&page);
    }
    log!("stamped {pages} pages with SVARENA1/{run_id:016x}");

    // What will Windows tell user mode about these pages?
    let wsex = wsex_report(base, size, large_min);

    if wsex_only {
        cleanup(awe_state, lock_state, size);
        return;
    }

    // ---- 4. tell the host ---------------------------------------------------
    let mut status = String::new();
    status.push_str(&format!("run_id={run_id:016x}\n"));
    status.push_str(&format!("mode={}\n", mode.as_str()));
    status.push_str(&format!("pages={pages}\n"));
    status.push_str(&format!("page_size={PAGE}\n"));
    status.push_str(&format!("base_va={:016x}\n", base as usize));
    status.push_str(&format!("privilege={priv_state}\n"));
    status.push_str(&wsex);
    if let Some(w) = &awe_state {
        for (i, pfn) in w.pfns.iter().enumerate() {
            status.push_str(&format!(
                "pfn {i} {:x} gpa {:x}\n",
                pfn,
                (*pfn as u64) << 12
            ));
        }
    }
    write_file(&workdir.join("guest-status.txt"), &status);
    log!("wrote guest-status.txt");

    // ---- 5. wait for the host to write into our pages -----------------------
    let host1 = match wait_for(&workdir.join("host-wrote.txt"), 300) {
        Some(s) => s,
        None => {
            log!("TIMEOUT waiting for host-wrote.txt — giving up");
            cleanup(awe_state, lock_state, size);
            return;
        }
    };
    let host1_stamp = parse_hex_field(&host1, "stamp").unwrap_or(0);
    log!("host says it wrote stamp=0x{host1_stamp:016x}");

    let mut seen_ok = 0usize;
    let mut index_bad = 0usize;
    for p in 0..pages {
        let page = &window[p * PAGE..(p + 1) * PAGE];
        if &rec::tag_at(page, rec::OFF_HOST1) == rec::HOST1_TAG
            && rec::get_u64(page, rec::OFF_HOST1_STAMP) == host1_stamp
        {
            if rec::get_u32(page, rec::OFF_HOST1_INDEX) == p as u32 {
                seen_ok += 1;
            } else {
                index_bad += 1;
            }
        }
    }
    log!(
        "host->guest through the plain window: {seen_ok}/{pages} pages carry the host record \
         ({index_bad} with a wrong page index)"
    );

    // ---- 6. the actual gate: import these pages into Vulkan -----------------
    let vk_stamp = run_id ^ 0x5CA1_AB1E_D00D_F00D;
    let vk = vulkan_import(base, size, pages, run_id, host1_stamp, vk_stamp);

    let mut out = String::new();
    out.push_str(&format!("run_id={run_id:016x}\n"));
    out.push_str(&format!("host1_seen_plain={seen_ok}\n"));
    out.push_str(&format!("host1_index_bad={index_bad}\n"));
    out.push_str(&format!("vk_stamp={vk_stamp:016x}\n"));
    out.push_str(&vk);
    write_file(&workdir.join("guest-vk.txt"), &out);
    log!("wrote guest-vk.txt");

    // ---- 7. host writes again while the import is live ----------------------
    // The mapping is dropped by now (vulkan_import unmaps before returning) but
    // the memory is still ours, still locked, and still at the same frames. A
    // second host write proves the sharing was not a one-shot artefact of
    // whatever the driver did at import time.
    if let Some(s) = wait_for(&workdir.join("host-done.txt"), 300) {
        let host2_stamp = parse_hex_field(&s, "stamp2").unwrap_or(0);
        let mut ok2 = 0usize;
        for p in 0..pages {
            let page = &window[p * PAGE..(p + 1) * PAGE];
            if &rec::tag_at(page, rec::OFF_HOST2) == rec::HOST2_TAG
                && rec::get_u64(page, rec::OFF_HOST2_STAMP) == host2_stamp
                && rec::get_u32(page, rec::OFF_HOST2_INDEX) == p as u32
            {
                ok2 += 1;
            }
        }
        log!("second host write, after the import: {ok2}/{pages} pages match");
        write_file(
            &workdir.join("guest-final.txt"),
            &format!("run_id={run_id:016x}\nhost2_seen={ok2}\npages={pages}\n"),
        );
    } else {
        log!("TIMEOUT waiting for host-done.txt");
    }

    cleanup(awe_state, lock_state, size);
    log!("done");
}

// ---------------------------------------------------------------------------
// windows allocation
// ---------------------------------------------------------------------------

struct AweWindow {
    base: *mut c_void,
    pfns: Vec<usize>,
}

struct LockedWindow {
    base: *mut c_void,
    /// Large-page allocations are non-pageable by construction and were never
    /// `VirtualLock`ed, so unlocking them on the way out would just fail.
    locked: bool,
}

/// What an allocator handed back, so the caller can keep it alive without
/// caring which one succeeded.
enum Acquired {
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
fn try_awe(pages: usize, large: bool) -> Result<AweWindow, String> {
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
            log!("AllocateUserPhysicalPages2 resolved from {from}");
            let mut ep =
                MEM_EXTENDED_PARAMETER::attribute_flags(MEM_EXTENDED_PARAMETER_NONPAGED_LARGE);
            let r = alloc2(proc, &mut want, pfns.as_mut_ptr(), &mut ep, 1);
            if r == 0 {
                // Reading two: the count is in large pages, not small ones.
                let e1 = GetLastError();
                let large_min = GetLargePageMinimum();
                let per = if large_min > PAGE {
                    large_min / PAGE
                } else {
                    512
                };
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
        // A short allocation is a documented outcome, not an error: the call
        // succeeds and reports how many frames it actually found. Take what we
        // were given and say so — the interesting question is what those frames
        // look like, and refusing them would throw that away.
        if want != pages {
            log!("asked for {pages} frames, got {want}");
        }
        if want == 0 {
            return Err("AllocateUserPhysicalPages returned zero frames".into());
        }
        let pages = want;
        pfns.truncate(want);

        // How contiguous are the frames we were given? This is the number the
        // host-side run count reflects, measured here without needing the host,
        // and the whole reason large pages are interesting.
        let runs = 1 + pfns.windows(2).filter(|w| w[1] != w[0] + 1).count();
        log!(
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
/// The fallback when the AWE privilege is not granted. It gives up the frame
/// numbers — the host has to find these pages by searching guest RAM for their
/// contents — but the pages are still guest RAM, still pinned, and still the
/// thing the driver is asked to import, so the R20 question is still on the
/// table. `VirtualLock` needs no privilege, only working-set quota, which is
/// why it is reachable from an account that cannot use AWE.
fn try_locked(size: usize) -> Result<LockedWindow, String> {
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
            log!("VirtualLock failed: {e} — continuing with unlocked memory");
        }
        Ok(LockedWindow { base, locked: true })
    }
}

/// `VirtualAlloc(MEM_LARGE_PAGES)`.
///
/// Each large page is one physically contiguous 2 MiB block and is never paged,
/// so a 64 MiB window is at most 32 runs in guest-physical space instead of the
/// ~1471 that 4 KiB pages measured. The costs are that the size must be a
/// multiple of `GetLargePageMinimum()`, that it needs the same
/// `SeLockMemoryPrivilege` AWE does, and that it can simply fail when the
/// guest's physical memory is too fragmented to find contiguous blocks —
/// which is a property worth measuring rather than hoping about.
fn try_large(size: usize) -> Result<LockedWindow, String> {
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
                1450 => " (ERROR_NO_SYTEM_RESOURCES — no contiguous physical blocks left)",
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
/// that is merely slow, so this repeats the same allocation and reports how
/// often it succeeded and what it failed with. Each attempt is released
/// immediately: nothing is held, and the guest is a production machine.
fn stress_large(size: usize, tries: usize) {
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
    log!(
        "large-page stress: {ok}/{tries} allocations of {} MiB succeeded, \
         slowest {:?}, last failure error {worst}",
        size / (1024 * 1024),
        slowest
    );
}

fn cleanup(awe: Option<AweWindow>, locked: Option<LockedWindow>, size: usize) {
    unsafe {
        if let Some(w) = awe {
            MapUserPhysicalPages(w.base, w.pfns.len() as ULONG_PTR, std::ptr::null());
            VirtualFree(w.base, 0, MEM_RELEASE);
            let mut n: ULONG_PTR = w.pfns.len();
            FreeUserPhysicalPages(GetCurrentProcess(), &mut n, w.pfns.as_ptr());
        }
        if let Some(w) = locked {
            if w.locked {
                VirtualUnlock(w.base, size);
            }
            VirtualFree(w.base, 0, MEM_RELEASE);
        }
    }
}

// ---------------------------------------------------------------------------
// the import
// ---------------------------------------------------------------------------

/// Record a key/value both in the log and in the status file the host reads.
fn say(out: &mut String, k: &str, v: String) {
    log!("vk: {k}={v}");
    out.push_str(&format!("{k}={v}\n"));
}

/// Import the window as `VkDeviceMemory` and look at it through the driver's
/// own mapping. Returns key=value lines for the status file.
fn vulkan_import(
    base: *mut c_void,
    size: usize,
    pages: usize,
    run_id: u64,
    host1_stamp: u64,
    vk_stamp: u64,
) -> String {
    let mut out = String::new();

    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            say(&mut out, "import", format!("no-loader ({e})"));
            return out;
        }
    };
    let app = vk::ApplicationInfo::default()
        .application_name(c"shared_vulkan awe-probe")
        .api_version(vk::API_VERSION_1_1);
    let ci = vk::InstanceCreateInfo::default().application_info(&app);
    let instance = match unsafe { entry.create_instance(&ci, None) } {
        Ok(i) => i,
        Err(e) => {
            say(&mut out, "import", format!("no-instance ({e:?})"));
            return out;
        }
    };
    let pds = match unsafe { instance.enumerate_physical_devices() } {
        Ok(v) if !v.is_empty() => v,
        _ => {
            say(&mut out, "import", "no-physical-device".into());
            unsafe { instance.destroy_instance(None) };
            return out;
        }
    };
    let pd = pds[0];
    let props = unsafe { instance.get_physical_device_properties(pd) };
    say(
        &mut out,
        "device",
        unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
            .to_string_lossy()
            .to_string(),
    );

    let prio = [1.0f32];
    let qi = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(0)
        .queue_priorities(&prio)];
    let exts = [c"VK_EXT_external_memory_host".as_ptr()];
    let dci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&qi)
        .enabled_extension_names(&exts);
    let device = match unsafe { instance.create_device(pd, &dci, None) } {
        Ok(d) => d,
        Err(e) => {
            say(&mut out, "import", format!("no-device ({e:?})"));
            unsafe { instance.destroy_instance(None) };
            return out;
        }
    };
    let host_mem = ash::ext::external_memory_host::Device::new(&instance, &device);

    let mut hp = vk::MemoryHostPointerPropertiesEXT::default();
    let res = unsafe {
        (host_mem.fp().get_memory_host_pointer_properties_ext)(
            device.handle(),
            vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
            base as *const c_void,
            &mut hp,
        )
    };
    say(
        &mut out,
        "getMemoryHostPointerProperties",
        format!("{res:?}"),
    );
    say(
        &mut out,
        "memoryTypeBits",
        format!("0x{:x}", hp.memory_type_bits),
    );
    if hp.memory_type_bits == 0 {
        say(&mut out, "import", "refused-no-memory-type".into());
        unsafe {
            device.destroy_device(None);
            instance.destroy_instance(None);
        }
        return out;
    }

    let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
    let chosen = (0..mem_props.memory_type_count).find(|i| {
        hp.memory_type_bits & (1 << i) != 0
            && mem_props.memory_types[*i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
    });
    let Some(ti) = chosen else {
        say(&mut out, "import", "no-host-visible-type".into());
        unsafe {
            device.destroy_device(None);
            instance.destroy_instance(None);
        }
        return out;
    };
    say(&mut out, "memory_type_index", format!("{ti}"));

    let mut imp = vk::ImportMemoryHostPointerInfoEXT::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)
        .host_pointer(base);
    let ai = vk::MemoryAllocateInfo::default()
        .allocation_size(size as u64)
        .memory_type_index(ti)
        .push_next(&mut imp);

    match unsafe { device.allocate_memory(&ai, None) } {
        Err(e) => say(
            &mut out,
            "import",
            format!("vkAllocateMemory-FAILED ({e:?})"),
        ),
        Ok(mem) => {
            say(&mut out, "import", "ok".into());
            match unsafe { device.map_memory(mem, 0, size as u64, vk::MemoryMapFlags::empty()) } {
                Err(e) => say(&mut out, "map", format!("failed ({e:?})")),
                Ok(mapped) => {
                    say(&mut out, "map", "ok".into());
                    say(
                        &mut out,
                        "mapped_same_address",
                        format!("{}", std::ptr::eq(mapped, base)),
                    );
                    let view = unsafe { std::slice::from_raw_parts_mut(mapped as *mut u8, size) };

                    // Does the driver's mapping show what the guest wrote...
                    let mut guest_ok = 0usize;
                    // ...and, the point of the whole exercise, what the *host*
                    // wrote from the other side of the hypervisor?
                    let mut host_ok = 0usize;
                    for p in 0..pages {
                        let page = &view[p * PAGE..(p + 1) * PAGE];
                        if rec::is_ours(page, run_id)
                            && rec::get_u32(page, rec::OFF_PAGE_INDEX) == p as u32
                        {
                            guest_ok += 1;
                        }
                        if &rec::tag_at(page, rec::OFF_HOST1) == rec::HOST1_TAG
                            && rec::get_u64(page, rec::OFF_HOST1_STAMP) == host1_stamp
                            && rec::get_u32(page, rec::OFF_HOST1_INDEX) == p as u32
                        {
                            host_ok += 1;
                        }
                    }
                    say(
                        &mut out,
                        "guest_records_via_vk",
                        format!("{guest_ok}/{pages}"),
                    );
                    say(
                        &mut out,
                        "host_records_via_vk",
                        format!("{host_ok}/{pages}"),
                    );

                    // Write back through the Vulkan mapping, so the host can
                    // confirm the reverse direction of the same claim.
                    for p in 0..pages {
                        let page = &mut view[p * PAGE..(p + 1) * PAGE];
                        page[rec::OFF_VK..rec::OFF_VK + 8].copy_from_slice(rec::VK_TAG);
                        rec::put_u64(page, rec::OFF_VK_STAMP, vk_stamp);
                        rec::put_u32(page, rec::OFF_VK_INDEX, p as u32);
                    }
                    say(&mut out, "wrote_vk_record", format!("{pages} pages"));
                    unsafe { device.unmap_memory(mem) };
                }
            }
            unsafe { device.free_memory(mem, None) };
        }
    }

    unsafe {
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    out
}

// ---------------------------------------------------------------------------
// rendezvous over the virtio-fs share
// ---------------------------------------------------------------------------

fn write_file(path: &Path, content: &str) {
    if let Err(e) = fs::write(path, content) {
        log!("failed to write {path:?}: {e}");
    }
}

fn wait_for(path: &Path, secs: u64) -> Option<String> {
    log!("waiting for {path:?} (up to {secs}s)");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        // An empty read is a file the other side has created but not finished
        // writing — over virtio-fs that window is real, so wait it out.
        if let Ok(s) = fs::read_to_string(path)
            && !s.trim().is_empty()
        {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

fn parse_hex_field(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        if let Some(v) = line.trim().strip_prefix(&format!("{key}=")) {
            return u64::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// QueryWorkingSetEx
// ---------------------------------------------------------------------------

/// Ask Windows what it will tell user mode about the pages of the window.
///
/// This exists to settle a specific question: large pages give physical
/// contiguity but no frame numbers, AWE gives frame numbers but the worst
/// contiguity measured, and if some documented call reported frames for
/// large-page memory we could have both. `QueryWorkingSetEx` is the candidate,
/// because it is the one user-mode call that reports per-page physical
/// *attributes* to a caller holding `SeLockMemoryPrivilege`.
///
/// So: dump every bit it returns, including the ones documented as reserved,
/// because a frame number would have to be hiding in those or nowhere.
fn wsex_report(base: *mut c_void, size: usize, large_min: usize) -> String {
    let mut out = String::new();
    let pages = size / PAGE;

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
            log!("QueryWorkingSetEx({label}) FAILED: {}", unsafe {
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
        // The whole point: is there anything in the reserved bits?
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
        log!("{line}");
        out.push_str(&line);
        out.push('\n');
    }
    let _ = pages;
    out
}
