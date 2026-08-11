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
    /// `VirtualAlloc` + `VirtualLock` — ordinary committed memory, pinned into
    /// the working set. The frames are unknown to us, so the host has to find
    /// the pages by searching for their contents.
    LockedVa,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Awe => "awe",
            Mode::LockedVa => "locked-va",
        }
    }
    fn code(self) -> u32 {
        match self {
            Mode::Awe => rec::MODE_AWE,
            Mode::LockedVa => rec::MODE_LOCKED_VA,
        }
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
            other => panic!("unknown argument {other}"),
        }
    }
    let size = pages * PAGE;

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
    let mut awe_state: Option<AweWindow> = None;
    let mut lock_state: Option<LockedWindow> = None;

    let (mode, base): (Mode, *mut c_void) = match try_awe(pages) {
        Ok(w) => {
            let b = w.base;
            log!(
                "AWE window: base=0x{:x} pages={} — frames known",
                b as usize,
                w.pfns.len()
            );
            awe_state = Some(w);
            (Mode::Awe, b)
        }
        Err(e) => {
            log!("AWE unavailable: {e}");
            log!("falling back to VirtualAlloc + VirtualLock (frames unknown to us)");
            let w = try_locked(size).expect("even the fallback allocation failed");
            let b = w.base;
            log!("locked window: base=0x{:x} size={}", b as usize, size);
            lock_state = Some(w);
            (Mode::LockedVa, b)
        }
    };

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

    // ---- 4. tell the host ---------------------------------------------------
    let mut status = String::new();
    status.push_str(&format!("run_id={run_id:016x}\n"));
    status.push_str(&format!("mode={}\n", mode.as_str()));
    status.push_str(&format!("pages={pages}\n"));
    status.push_str(&format!("page_size={PAGE}\n"));
    status.push_str(&format!("base_va={:016x}\n", base as usize));
    status.push_str(&format!("privilege={priv_state}\n"));
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
}

/// `AllocateUserPhysicalPages` + `MapUserPhysicalPages`.
///
/// This is the arrangement `design.md` §5.1 wants: the frame numbers come back
/// to user mode, and inside a VM they are guest-physical, which is what makes
/// the host able to find the pages at all.
fn try_awe(pages: usize) -> Result<AweWindow, String> {
    unsafe {
        let proc = GetCurrentProcess();
        let mut want: ULONG_PTR = pages;
        let mut pfns: Vec<ULONG_PTR> = vec![0; pages];
        if AllocateUserPhysicalPages(proc, &mut want, pfns.as_mut_ptr()) == 0 {
            let e = GetLastError();
            let why = if e == ERROR_PRIVILEGE_NOT_HELD {
                " (ERROR_PRIVILEGE_NOT_HELD — SeLockMemoryPrivilege is not in this token)"
            } else {
                ""
            };
            return Err(format!("AllocateUserPhysicalPages failed: {e}{why}"));
        }
        if want != pages {
            return Err(format!(
                "AllocateUserPhysicalPages gave {want} of {pages} pages"
            ));
        }
        pfns.truncate(want);

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
        Ok(LockedWindow { base })
    }
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
            VirtualUnlock(w.base, size);
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
