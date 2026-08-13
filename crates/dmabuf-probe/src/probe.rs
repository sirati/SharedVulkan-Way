//! The Linux half. See the crate docs for what is being asked and why.
//!
//! # `unsafe`
//!
//! Confined to three things: the `memfd_create`/`fcntl`/`ioctl`/`mmap` calls,
//! `ash`'s Vulkan entry points, and reading back through the two mappings. The
//! probe never keeps a raw pointer past the scope that made it.

use ash::vk;
use std::ffi::{CStr, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

// ---------------------------------------------------------------------------
// udmabuf
// ---------------------------------------------------------------------------

/// `linux/udmabuf.h`. Reproduced rather than bindgen'd: it is two fields and a
/// flag, and the alternative is a build dependency on kernel headers for a
/// probe.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct UdmabufCreate {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

/// `UDMABUF_CREATE = _IOW('u', 0x42, struct udmabuf_create)`.
const UDMABUF_CREATE: libc::c_ulong = 0x4018_7542;
/// `UDMABUF_FLAGS_CLOEXEC`.
const UDMABUF_FLAGS_CLOEXEC: u32 = 0x01;

/// `F_SEAL_SHRINK`, which udmabuf requires: it pins the pages for the lifetime
/// of the dma-buf, and a memfd that can shrink underneath it would leave the
/// importer holding pages that are no longer the file's.
const F_SEAL_SHRINK: libc::c_int = 0x0002;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn errno_text() -> String {
    let e = std::io::Error::last_os_error();
    format!("{} ({})", e, e.raw_os_error().unwrap_or(0))
}

/// A sealed memfd of `len` bytes: the arena's real backing, as `sv-hostd`
/// creates it.
fn sealed_memfd(len: u64) -> Result<OwnedFd, String> {
    let name = c"sv-dmabuf-probe";
    // `MFD_ALLOW_SEALING` at creation is not optional and cannot be added later
    // — which is exactly why `sv_transport::region::memfd` already passes it.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(format!("memfd_create: {}", errno_text()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
        return Err(format!("ftruncate: {}", errno_text()));
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, F_SEAL_SHRINK) } < 0 {
        return Err(format!("F_ADD_SEALS(F_SEAL_SHRINK): {}", errno_text()));
    }
    Ok(fd)
}

/// A window backed by a file on a real filesystem, for the comparison row.
///
/// Not what the arena is — it is here only to isolate which variable a
/// contradicted measurement turned on. A memfd is shmem; a file on ext4/btrfs is
/// page cache with a writeback path, and a driver's user-pointer import may
/// legitimately treat the two differently.
fn open_backing_file(path: &std::path::Path, len: u64) -> Result<OwnedFd, String> {
    use std::os::fd::IntoRawFd;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    f.set_len(len).map_err(|e| format!("set_len: {e}"))?;
    Ok(unsafe { OwnedFd::from_raw_fd(f.into_raw_fd()) })
}

/// Wrap a sealed memfd as a dma-buf. The step this whole probe exists to reach.
fn udmabuf_wrap(memfd: &OwnedFd, len: u64) -> Result<OwnedFd, String> {
    let dev = unsafe { libc::open(c"/dev/udmabuf".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if dev < 0 {
        let e = errno();
        // Distinguish the two absences, because they need different answers: a
        // missing module is `modprobe udmabuf`, a permission problem is a group.
        let why = if e == libc::ENOENT {
            "/dev/udmabuf does not exist — the udmabuf module is not loaded"
        } else if e == libc::EACCES || e == libc::EPERM {
            "/dev/udmabuf is not openable by this account — it is root:kvm 0660 here \
             and the account is not in `kvm`. This is the Linux-GPU-side analogue of \
             SeLockMemoryPrivilege (R25): a group change, reversible, no code path affected"
        } else {
            "open /dev/udmabuf failed"
        };
        return Err(format!("{why}: {}", errno_text()));
    }
    let dev = unsafe { OwnedFd::from_raw_fd(dev) };
    let mut req = UdmabufCreate {
        memfd: memfd.as_raw_fd() as u32,
        flags: UDMABUF_FLAGS_CLOEXEC,
        offset: 0,
        size: len,
    };
    let dmabuf = unsafe { libc::ioctl(dev.as_raw_fd(), UDMABUF_CREATE, &mut req) };
    if dmabuf < 0 {
        return Err(format!("UDMABUF_CREATE: {}", errno_text()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(dmabuf as RawFd) })
}

// ---------------------------------------------------------------------------
// mappings
// ---------------------------------------------------------------------------

struct Mapping {
    base: *mut c_void,
    len: usize,
}

impl Mapping {
    fn shared(fd: RawFd, len: usize) -> Result<Mapping, String> {
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(format!("mmap(MAP_SHARED): {}", errno_text()));
        }
        Ok(Mapping { base, len })
    }

    fn anonymous(len: usize) -> Result<Mapping, String> {
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(format!("mmap(anon): {}", errno_text()));
        }
        Ok(Mapping { base, len })
    }

    fn write_u32(&self, index: usize, v: u32) {
        unsafe { (self.base as *mut u32).add(index).write_volatile(v) }
    }

    fn read_u32(&self, index: usize) -> u32 {
        unsafe { (self.base as *const u32).add(index).read_volatile() }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base, self.len) };
    }
}

// ---------------------------------------------------------------------------
// the four rows
// ---------------------------------------------------------------------------

/// What one attempt produced. `Refused` is a result, not an error: the whole
/// point is to record *which* code each driver gives, since two of them have
/// already been observed lying about the reason (`OUT_OF_DEVICE_MEMORY` for a
/// mapping the driver simply does not accept).
enum Outcome {
    Imported { memory_type_bits: u32, coherent: Option<CoherencyCheck> },
    Refused(String),
    Skipped(String),
}

struct CoherencyCheck {
    /// Bytes written through the backing mapping, read through `vkMapMemory`.
    forward: bool,
    /// Bytes written through `vkMapMemory`, read through the backing mapping.
    reverse: bool,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Imported { memory_type_bits, coherent: None } => {
                write!(f, "IMPORTED  memoryTypeBits=0x{memory_type_bits:x}  (not host-visible, no coherency check)")
            }
            Outcome::Imported { memory_type_bits, coherent: Some(c) } => write!(
                f,
                "IMPORTED  memoryTypeBits=0x{memory_type_bits:x}  host->vk {}  vk->host {}",
                if c.forward { "ok" } else { "MISMATCH" },
                if c.reverse { "ok" } else { "MISMATCH" },
            ),
            Outcome::Refused(why) => write!(f, "refused: {why}"),
            Outcome::Skipped(why) => write!(f, "skipped: {why}"),
        }
    }
}

/// Default window. Overridable, because the first run of this probe contradicted
/// a recorded measurement and window size was one of the two variables that
/// differed — see `main`.
const DEFAULT_LEN: u64 = 4 << 20;

fn device_name(props: &vk::PhysicalDeviceProperties) -> String {
    unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy().into_owned()
}

fn has_ext(exts: &[vk::ExtensionProperties], want: &CStr) -> bool {
    exts.iter().any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == want)
}

pub fn main() -> std::process::ExitCode {
    // Two knobs, and both exist because the first run of this probe contradicted
    // a measurement already in `measured-gpu-side.md` — NVIDIA imported a
    // `MAP_SHARED` mapping that the record says it refuses with
    // `OUT_OF_DEVICE_MEMORY`. Two things differed between the runs: the window
    // was 4 MiB here against 128 MiB there, and the backing was a **memfd** here
    // against a file on a real filesystem there. Both are testable in a minute
    // and guessing between them is not acceptable when the answer decides
    // whether the reverse direction needs a privilege change at all.
    let mut len = DEFAULT_LEN;
    let mut file: Option<std::path::PathBuf> = None;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--size-mib" => {
                len = args[i + 1].parse::<u64>().expect("--size-mib takes a number") << 20;
                i += 2;
            }
            // Back the "shared" row with a file on a real filesystem instead of
            // a memfd, which is what the contradicted measurement used.
            "--file" => {
                file = Some(std::path::PathBuf::from(&args[i + 1]));
                i += 2;
            }
            other => {
                eprintln!("unknown argument {other}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    let len = len;

    println!("dmabuf-probe: can a Linux GPU side import an arena it did not allocate?");
    println!(
        "window: {} MiB   shared backing: {}\n",
        len >> 20,
        match &file {
            Some(p) => format!("file {}", p.display()),
            None => "memfd (what sv-hostd actually creates)".to_string(),
        }
    );

    // ---- the backings, built once and shared by every device ---------------
    let memfd = match &file {
        Some(p) => open_backing_file(p, len),
        None => sealed_memfd(len),
    };
    let memfd = match memfd {
        Ok(f) => f,
        Err(e) => {
            eprintln!("could not create the arena's memfd: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let memfd_map = match Mapping::shared(memfd.as_raw_fd(), len as usize) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("could not map the memfd: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let anon_map = match Mapping::anonymous(len as usize) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("could not map anonymous memory: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let dmabuf = udmabuf_wrap(&memfd, len);
    match &dmabuf {
        Ok(fd) => println!("udmabuf: wrapped the memfd as dma-buf fd {}\n", fd.as_raw_fd()),
        Err(e) => {
            println!("udmabuf: UNAVAILABLE — {e}");
            println!(
                "         the `fd(udmabuf)` row below cannot be measured on this account.\n"
            );
        }
    }

    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            eprintln!("no Vulkan loader: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let app = vk::ApplicationInfo::default()
        .application_name(c"dmabuf-probe")
        .api_version(vk::API_VERSION_1_1);
    let instance = match unsafe {
        entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None)
    } {
        Ok(i) => i,
        Err(e) => {
            eprintln!("vkCreateInstance: {e:?}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let devices = unsafe { instance.enumerate_physical_devices() }.unwrap_or_default();
    println!("{} physical device(s)\n", devices.len());

    let mut any_dmabuf_import = false;
    for pd in devices {
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let exts =
            unsafe { instance.enumerate_device_extension_properties(pd) }.unwrap_or_default();
        println!("=== {}", device_name(&props));
        let host_ptr_ext = has_ext(&exts, c"VK_EXT_external_memory_host");
        let fd_ext = has_ext(&exts, c"VK_KHR_external_memory_fd");
        let dmabuf_ext = has_ext(&exts, c"VK_EXT_external_memory_dma_buf");
        println!(
            "    external_memory_host={host_ptr_ext} external_memory_fd={fd_ext} external_memory_dma_buf={dmabuf_ext}"
        );

        let Some(dev) = create_device(&instance, pd, host_ptr_ext, fd_ext, dmabuf_ext) else {
            println!("    vkCreateDevice failed — skipping this device\n");
            continue;
        };

        let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };

        // Row 1: ordinary anonymous memory. The control: a device that fails
        // here imports nothing and the other rows say nothing about dma-buf.
        let r = if host_ptr_ext {
            try_host_pointer(&instance, &dev, pd, &mem_props, &anon_map)
        } else {
            Outcome::Skipped("no VK_EXT_external_memory_host".into())
        };
        println!("    host-ptr(anon)        {r}");

        // Row 2: the arena's real shape on a host pointer. Measured refused by
        // NVIDIA and RADV; this re-runs it here so the contrast is in one report.
        let r = if host_ptr_ext {
            try_host_pointer(&instance, &dev, pd, &mem_props, &memfd_map)
        } else {
            Outcome::Skipped("no VK_EXT_external_memory_host".into())
        };
        println!("    host-ptr(memfd)       {r}");

        // Row 3: the memfd handed straight to the fd import. It is one word
        // different in the source from row 4 and it does not work; recording the
        // error it gives is worth more than asserting that it cannot.
        let r = if fd_ext && dmabuf_ext {
            try_fd_import(&instance, &dev, &mem_props, memfd.as_raw_fd(), &memfd_map)
        } else {
            Outcome::Skipped("no VK_KHR_external_memory_fd + VK_EXT_external_memory_dma_buf".into())
        };
        println!("    fd(memfd-as-dmabuf)   {r}");

        // Row 4: the reading.
        let r = match (&dmabuf, fd_ext && dmabuf_ext) {
            (Ok(fd), true) => {
                let o = try_fd_import(&instance, &dev, &mem_props, fd.as_raw_fd(), &memfd_map);
                if matches!(o, Outcome::Imported { .. }) {
                    any_dmabuf_import = true;
                }
                o
            }
            (Ok(_), false) => Outcome::Skipped("device does not advertise dma-buf import".into()),
            (Err(e), _) => Outcome::Skipped(format!("no dma-buf to import — {e}")),
        };
        println!("    fd(udmabuf)           {r}\n");

        unsafe { dev.destroy_device(None) };
    }

    unsafe { instance.destroy_instance(None) };
    drop(memfd_map);
    drop(anon_map);

    if any_dmabuf_import {
        println!("verdict: at least one driver imported the arena's memfd through udmabuf.");
        println!("         The reverse direction (GPU side on the host, app side in the VM)");
        println!("         has a mechanism.");
        std::process::ExitCode::SUCCESS
    } else {
        println!("verdict: no driver imported the arena through a dma-buf in this run.");
        println!("         If the udmabuf line above says UNAVAILABLE, that is the reason and");
        println!("         the question is unanswered rather than answered negatively.");
        std::process::ExitCode::FAILURE
    }
}

fn create_device(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    host_ptr: bool,
    fd: bool,
    dmabuf: bool,
) -> Option<ash::Device> {
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let family = families.iter().position(|f| {
        f.queue_flags.contains(vk::QueueFlags::COMPUTE) || f.queue_flags.contains(vk::QueueFlags::TRANSFER)
    })? as u32;
    let prio = [1.0f32];
    let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
    let mut names: Vec<*const i8> = Vec::new();
    if host_ptr {
        names.push(c"VK_EXT_external_memory_host".as_ptr());
    }
    if fd {
        names.push(c"VK_KHR_external_memory_fd".as_ptr());
    }
    if dmabuf {
        names.push(c"VK_EXT_external_memory_dma_buf".as_ptr());
    }
    let ci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&qci)
        .enabled_extension_names(&names);
    unsafe { instance.create_device(pd, &ci, None) }.ok()
}

/// Pick a memory type that the driver permits *and* that we can map, preferring
/// host-visible so the coherency check can run at all.
fn pick_type(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want_host_visible: bool,
) -> Option<(u32, bool)> {
    let mut fallback = None;
    for i in 0..mem_props.memory_type_count {
        if bits & (1 << i) == 0 {
            continue;
        }
        let hv = mem_props.memory_types[i as usize]
            .property_flags
            .contains(vk::MemoryPropertyFlags::HOST_VISIBLE);
        if hv || !want_host_visible {
            if hv {
                return Some((i, true));
            }
            fallback.get_or_insert((i, false));
        }
    }
    fallback
}

fn try_host_pointer(
    instance: &ash::Instance,
    dev: &ash::Device,
    pd: vk::PhysicalDevice,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    map: &Mapping,
) -> Outcome {
    let ext = ash::ext::external_memory_host::Device::new(instance, dev);
    let mut props = vk::MemoryHostPointerPropertiesEXT::default();
    let r = unsafe {
        (ext.fp().get_memory_host_pointer_properties_ext)(
            dev.handle(),
            vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
            map.base,
            &mut props,
        )
    };
    if r != vk::Result::SUCCESS {
        return Outcome::Refused(format!("vkGetMemoryHostPointerPropertiesEXT: {r:?}"));
    }
    let Some((ty, host_visible)) = pick_type(mem_props, props.memory_type_bits, true) else {
        return Outcome::Refused(format!(
            "no usable memory type in mask 0x{:x}",
            props.memory_type_bits
        ));
    };
    let _ = pd;
    let mut import = vk::ImportMemoryHostPointerInfoEXT::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)
        .host_pointer(map.base);
    let ai = vk::MemoryAllocateInfo::default()
        .allocation_size(map.len as u64)
        .memory_type_index(ty)
        .push_next(&mut import);
    match unsafe { dev.allocate_memory(&ai, None) } {
        Ok(mem) => {
            let coherent = host_visible.then(|| coherency(dev, mem, map));
            unsafe { dev.free_memory(mem, None) };
            Outcome::Imported { memory_type_bits: props.memory_type_bits, coherent }
        }
        Err(e) => Outcome::Refused(format!("vkAllocateMemory: {e:?}")),
    }
}

fn try_fd_import(
    instance: &ash::Instance,
    dev: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    fd: RawFd,
    map: &Mapping,
) -> Outcome {
    let ext = ash::khr::external_memory_fd::Device::new(instance, dev);
    // The driver says which types can take this handle. Asking is not optional:
    // guessing a type index is how an import fails with a misleading code.
    let mut props = vk::MemoryFdPropertiesKHR::default();
    // The fd is consumed by a successful vkAllocateMemory, so every call gets
    // its own duplicate and the caller keeps ownership of the original.
    let query_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if query_fd < 0 {
        return Outcome::Refused(format!("dup: {}", errno_text()));
    }
    let query_fd = unsafe { OwnedFd::from_raw_fd(query_fd) };
    let r = unsafe {
        (ext.fp().get_memory_fd_properties_khr)(
            dev.handle(),
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            query_fd.as_raw_fd(),
            &mut props,
        )
    };
    if r != vk::Result::SUCCESS {
        return Outcome::Refused(format!("vkGetMemoryFdPropertiesKHR: {r:?}"));
    }
    let Some((ty, host_visible)) = pick_type(mem_props, props.memory_type_bits, true) else {
        return Outcome::Refused(format!(
            "no usable memory type in mask 0x{:x}",
            props.memory_type_bits
        ));
    };
    let import_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if import_fd < 0 {
        return Outcome::Refused(format!("dup: {}", errno_text()));
    }
    let mut import = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(import_fd);
    let ai = vk::MemoryAllocateInfo::default()
        .allocation_size(map.len as u64)
        .memory_type_index(ty)
        .push_next(&mut import);
    match unsafe { dev.allocate_memory(&ai, None) } {
        Ok(mem) => {
            let coherent = host_visible.then(|| coherency(dev, mem, map));
            unsafe { dev.free_memory(mem, None) };
            Outcome::Imported { memory_type_bits: props.memory_type_bits, coherent }
        }
        Err(e) => {
            // vkAllocateMemory consumes the fd only on success, so on failure it
            // is still ours to close.
            unsafe { libc::close(import_fd) };
            Outcome::Refused(format!("vkAllocateMemory: {e:?}"))
        }
    }
}

/// The check that separates "shared" from "the driver made a copy".
fn coherency(dev: &ash::Device, mem: vk::DeviceMemory, map: &Mapping) -> CoherencyCheck {
    let Ok(ptr) =
        (unsafe { dev.map_memory(mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) })
    else {
        return CoherencyCheck { forward: false, reverse: false };
    };
    let vk_view = ptr as *mut u32;
    // host -> vulkan
    map.write_u32(0, 0xA1B2_C3D4);
    map.write_u32(1023, 0x1122_3344);
    let forward = unsafe { vk_view.read_volatile() } == 0xA1B2_C3D4
        && unsafe { vk_view.add(1023).read_volatile() } == 0x1122_3344;
    // vulkan -> host
    unsafe { vk_view.add(2).write_volatile(0x5566_7788) };
    let reverse = map.read_u32(2) == 0x5566_7788;
    unsafe { dev.unmap_memory(mem) };
    CoherencyCheck { forward, reverse }
}
