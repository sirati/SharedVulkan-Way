//! `sv-export-probe` — can the GPU allocate the arena and export it?
//!
//! # The question, and why it is worth a probe of its own
//!
//! `sv_arena::import` records a measurement that shaped the design: **RADV and
//! NVIDIA both refuse a `MAP_SHARED` host pointer**, so on a Linux GPU side
//! `VK_EXT_external_memory_host` is not the import path. The conclusion drawn
//! from that was "we need a dma-buf", and the mechanism reached for was
//! `/dev/udmabuf` — which on this host is `root:kvm` 0660 with our account not
//! in `kvm`, so the Linux GPU side was blocked on a group change.
//!
//! **That conclusion was wrong, and the evidence that it was wrong is that CUDA
//! needs no privilege.** There are two directions across the dma-buf boundary
//! and only one of them is restricted:
//!
//! - *Userspace allocates, kernel wraps it* — `/dev/udmabuf` pins arbitrary user
//!   pages on demand. That is a denial-of-service lever, so it is root-ish, and
//!   it exists largely for QEMU.
//! - *The GPU allocates, then exports* — `vkGetMemoryFdKHR` on memory allocated
//!   with `VkExportMemoryAllocateInfo`, which is an ioctl on a render node the
//!   process already has open. `/dev/dri/renderD128` on this host is
//!   `crw-rw-rw-`. This is why every ordinary GPU program needs no privilege.
//!
//! Our arena was a memfd only because `sv-hostd` created it, and that is a
//! *design choice*, not a constraint. Inverting it — the side that can allocate
//! memory the other can map, allocates — removes the privilege problem entirely
//! and unblocks the real GPUs on this machine rather than only lavapipe.
//!
//! # What this probe establishes, in order
//!
//! Each step can fail on its own and the report says which:
//!
//! 1. the device offers `VK_KHR_external_memory_fd` (+ `VK_EXT_external_memory_dma_buf`);
//! 2. `vkAllocateMemory` accepts `VkExportMemoryAllocateInfo` on a host-visible
//!    type;
//! 3. `vkGetMemoryFdKHR` returns a descriptor;
//! 4. **`mmap` of that descriptor succeeds** — the step that is genuinely not
//!    guaranteed. A dma-buf is CPU-mappable only if the exporter implements
//!    `mmap` for it, which is usual for host-visible memory and not promised;
//! 5. bytes written through the `mmap` are visible through `vkMapMemory` of the
//!    same allocation, and bytes written through `vkMapMemory` are visible
//!    through the `mmap`. That is the arena invariant (§1) stated as an
//!    experiment, and it is what makes the two views the same physical pages
//!    rather than two copies.
//!
//! Step 5 in *both* directions matters: a one-directional check passes against a
//! driver that quietly hands back a bounce buffer.

#[cfg(not(target_os = "linux"))]
fn main() {
    // The probe `mmap`s an exported descriptor, which is the whole question it
    // asks. A Windows GPU side allocates the arena a different way entirely
    // (step 0's `VirtualAlloc` path) and this measurement does not apply to it.
    eprintln!("sv-export-probe: Linux-only — it mmaps an exported descriptor");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
use ash::vk;
#[cfg(target_os = "linux")]
use std::ffi::CStr;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            eprintln!("no Vulkan loader: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let app = vk::ApplicationInfo::default()
        .application_name(c"sv-export-probe")
        .api_version(vk::API_VERSION_1_1);
    let ci = vk::InstanceCreateInfo::default().application_info(&app);
    let instance = match unsafe { entry.create_instance(&ci, None) } {
        Ok(i) => i,
        Err(e) => {
            eprintln!("vkCreateInstance: {e:?}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let pds = unsafe { instance.enumerate_physical_devices() }.unwrap_or_default();
    println!("{} physical device(s)\n", pds.len());

    let mut any_ok = false;
    for (i, &pd) in pds.iter().enumerate() {
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy();
        println!("--- device {i}: {name} ---");
        match probe(&instance, pd) {
            Ok(r) => {
                println!("  RESULT: {r}");
                any_ok = true;
            }
            Err(e) => println!("  RESULT: unusable — {e}"),
        }
        println!();
    }
    unsafe { instance.destroy_instance(None) };
    if any_ok { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::FAILURE }
}

#[cfg(target_os = "linux")]
/// Bytes to move. Small: the question is whether the mechanism works, not how
/// fast it is, and a probe that allocates a gigabyte on somebody's desktop GPU
/// is a probe people stop running.
const SIZE: u64 = 1 << 20;

#[cfg(target_os = "linux")]
fn probe(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Result<String, String> {
    // -- 1. the extensions ---------------------------------------------------
    let exts = unsafe { instance.enumerate_device_extension_properties(pd) }
        .map_err(|e| format!("vkEnumerateDeviceExtensionProperties: {e:?}"))?;
    let has = |want: &CStr| {
        exts.iter().any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == want)
    };
    if !has(c"VK_KHR_external_memory_fd") {
        return Err("no VK_KHR_external_memory_fd".into());
    }
    let has_dmabuf = has(c"VK_EXT_external_memory_dma_buf");

    let qfams = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let qf = qfams
        .iter()
        .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
        .ok_or("no compute queue family")? as u32;

    let prio = [1.0f32];
    let qi = [vk::DeviceQueueCreateInfo::default().queue_family_index(qf).queue_priorities(&prio)];
    let mut names = vec![c"VK_KHR_external_memory_fd".as_ptr()];
    if has_dmabuf {
        names.push(c"VK_EXT_external_memory_dma_buf".as_ptr());
    }
    let dci = vk::DeviceCreateInfo::default().queue_create_infos(&qi).enabled_extension_names(&names);
    let device = unsafe { instance.create_device(pd, &dci, None) }
        .map_err(|e| format!("vkCreateDevice: {e:?}"))?;
    let ext_fd = ash::khr::external_memory_fd::Device::new(instance, &device);

    let out = (|| -> Result<String, String> {
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };

        // Every handle type worth trying, most specific first. DMA_BUF is what
        // an unrelated process can mmap; OPAQUE_FD is the portable fallback and
        // on some drivers is the same object underneath.
        let mut kinds: Vec<(&str, vk::ExternalMemoryHandleTypeFlags)> = Vec::new();
        if has_dmabuf {
            kinds.push(("dma-buf", vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT));
        }
        kinds.push(("opaque-fd", vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD));

        let mut notes = Vec::new();
        for (kind_name, kind) in kinds {
            // -- 2. a host-visible, host-coherent type that permits export ----
            //
            // `HOST_VISIBLE | HOST_COHERENT` because §5.6 decides the arena is
            // write-back and coherent on both sides; a type needing explicit
            // flushes would be a different design, not a slower one.
            let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let mut tried_any = false;
            for ti in 0..mem_props.memory_type_count {
                let t = mem_props.memory_types[ti as usize];
                if !t.property_flags.contains(want) {
                    continue;
                }
                tried_any = true;
                match try_type(instance, pd, &device, &ext_fd, ti, kind) {
                    Ok(bidirectional) => {
                        let heap = mem_props.memory_heaps[t.heap_index as usize];
                        let dl = t.property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL);
                        return Ok(format!(
                            "{kind_name} export works — memory type {ti} \
                             ({}{}HOST_VISIBLE|HOST_COHERENT), heap {} of {} MiB, \
                             mmap of the exported fd succeeded, \
                             coherency {}",
                            if dl { "DEVICE_LOCAL|" } else { "" },
                            "",
                            t.heap_index,
                            heap.size >> 20,
                            if bidirectional {
                                "verified in both directions"
                            } else {
                                "verified host→gpu only"
                            }
                        ));
                    }
                    Err(e) => notes.push(format!("{kind_name} type {ti}: {e}")),
                }
            }
            if !tried_any {
                notes.push(format!("{kind_name}: no HOST_VISIBLE|HOST_COHERENT memory type"));
            }
        }
        Err(notes.join("; "))
    })();

    unsafe {
        let _ = device.device_wait_idle();
        device.destroy_device(None);
    }
    out
}

#[cfg(target_os = "linux")]
/// One (memory type, handle type) pair, carried all the way to bytes.
fn try_type(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    device: &ash::Device,
    ext_fd: &ash::khr::external_memory_fd::Device,
    type_index: u32,
    kind: vk::ExternalMemoryHandleTypeFlags,
) -> Result<bool, String> {
    // Ask first. `vkGetPhysicalDeviceExternalBufferProperties` says whether this
    // handle type is exportable at all, and a driver that says no here will fail
    // the allocation with a less informative error.
    let ebi = vk::PhysicalDeviceExternalBufferInfo::default()
        .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_SRC)
        .handle_type(kind);
    let mut ebp = vk::ExternalBufferProperties::default();
    unsafe { instance.get_physical_device_external_buffer_properties(pd, &ebi, &mut ebp) };
    if !ebp
        .external_memory_properties
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
    {
        return Err("the driver does not advertise this handle type as exportable".into());
    }

    // -- 2. allocate for export ---------------------------------------------
    let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(kind);
    let ai = vk::MemoryAllocateInfo::default()
        .allocation_size(SIZE)
        .memory_type_index(type_index)
        .push_next(&mut export);
    let mem = unsafe { device.allocate_memory(&ai, None) }
        .map_err(|e| format!("vkAllocateMemory(export): {e:?}"))?;

    let result = (|| -> Result<bool, String> {
        // -- 3. export ------------------------------------------------------
        let gi = vk::MemoryGetFdInfoKHR::default().memory(mem).handle_type(kind);
        let fd = unsafe { ext_fd.get_memory_fd(&gi) }
            .map_err(|e| format!("vkGetMemoryFdKHR: {e:?}"))?;
        // Owned from here on, so every path below closes it exactly once.
        let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };

        // -- 4. mmap it -----------------------------------------------------
        //
        // The step the coordinator's note flagged as "test rather than assume".
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SIZE as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(format!(
                "mmap of the exported fd failed: {} — the exporter does not \
                 implement mmap for this memory",
                std::io::Error::last_os_error()
            ));
        }
        let unmap = || unsafe { libc::munmap(p, SIZE as usize) };

        // -- 5. the same physical pages, both ways --------------------------
        let host = p as *mut u8;
        let gpu = match unsafe { device.map_memory(mem, 0, SIZE, vk::MemoryMapFlags::empty()) } {
            Ok(q) => q as *mut u8,
            Err(e) => {
                unmap();
                return Err(format!("vkMapMemory of the exported allocation: {e:?}"));
            }
        };

        // host → driver
        let pattern: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(31) ^ 0xA5) as u8).collect();
        unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), host, pattern.len()) };
        let mut back = vec![0u8; pattern.len()];
        unsafe { std::ptr::copy_nonoverlapping(gpu, back.as_mut_ptr(), back.len()) };
        let forward_ok = back == pattern;

        // driver → host, at a different offset so a stale read cannot pass by
        // reading the bytes the first direction already wrote.
        let mark: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(17) ^ 0x5A) as u8).collect();
        unsafe { std::ptr::copy_nonoverlapping(mark.as_ptr(), gpu.add(8192), mark.len()) };
        let mut back2 = vec![0u8; mark.len()];
        unsafe { std::ptr::copy_nonoverlapping(host.add(8192), back2.as_mut_ptr(), back2.len()) };
        let reverse_ok = back2 == mark;

        unsafe { device.unmap_memory(mem) };
        unmap();

        if !forward_ok {
            return Err("bytes written through the mmap were not visible through \
                        vkMapMemory: the two views are not the same pages"
                .into());
        }
        Ok(reverse_ok)
    })();

    // **lavapipe segfaults here**, in `lvp_FreeMemory` → `llvmpipe_free_memory_fd`
    // → `os_free_fd`, on mesa 26.1.1. It is a driver bug and not ours — the
    // export, the `mmap` and both coherency directions have already succeeded by
    // the time this runs — but a probe that dies in a destructor reports nothing
    // at all, which is the worst outcome for a measurement. `SV_PROBE_LEAK=1`
    // leaks the allocation so the result can be printed; the process is about to
    // exit anyway.
    if std::env::var_os("SV_PROBE_LEAK").is_none() {
        unsafe { device.free_memory(mem, None) };
    }
    result
}
