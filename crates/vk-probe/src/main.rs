//! Ask a real driver whether the arena can work.
//!
//! The whole design rests on one property of the GPU-side driver: can it take
//! a plain pointer to memory we allocated and import it as `VkDeviceMemory`?
//! That is `VK_EXT_external_memory_host`. If it is present, the passthrough
//! arena is possible and the GPU DMAs straight out of application memory. If it
//! is absent, the arena needs a different mechanism entirely and the
//! architecture changes.
//!
//! Nothing about that question can be answered by reading documentation, so
//! this asks the driver. It is a console program with no window and no GPU work
//! — safe to run over ssh in a guest whose interactive session belongs to
//! somebody else.
//!
//! Cross-compiled for Windows from the Linux dev shell:
//!   cargo build -p vk-probe --release --target x86_64-pc-windows-gnu

use anyhow::{Result, bail};
use ash::vk;
use std::ffi::CStr;

/// The extension the arena depends on.
const WANTED: &CStr = c"VK_EXT_external_memory_host";

fn main() -> Result<()> {
    // `Entry::load` finds vulkan-1.dll on Windows and libvulkan.so on Linux,
    // so the same binary answers the question on either side.
    let entry = unsafe { ash::Entry::load() };
    let entry = match entry {
        Ok(e) => e,
        Err(e) => bail!("no Vulkan loader: {e}"),
    };

    let app = vk::ApplicationInfo::default()
        .application_name(c"shared_vulkan probe")
        .api_version(vk::API_VERSION_1_1);
    let create = vk::InstanceCreateInfo::default().application_info(&app);
    let instance = unsafe { entry.create_instance(&create, None) }?;

    let devices = unsafe { instance.enumerate_physical_devices() }?;
    if devices.is_empty() {
        println!("NO PHYSICAL DEVICES — the loader found no driver");
        unsafe { instance.destroy_instance(None) };
        return Ok(());
    }

    for (i, pd) in devices.iter().enumerate() {
        let props = unsafe { instance.get_physical_device_properties(*pd) };
        let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) };
        println!("=== device {i}: {}", name.to_string_lossy());
        println!(
            "    type={:?} api={}.{}.{} driver=0x{:x}",
            props.device_type,
            vk::api_version_major(props.api_version),
            vk::api_version_minor(props.api_version),
            vk::api_version_patch(props.api_version),
            props.driver_version,
        );

        let exts = unsafe { instance.enumerate_device_extension_properties(*pd) }?;
        let has_host_import = exts.iter().any(|e| {
            (unsafe { CStr::from_ptr(e.extension_name.as_ptr()) }) == WANTED
        });

        println!(
            "    VK_EXT_external_memory_host: {}",
            if has_host_import { "YES — arena import is possible" } else { "NO" }
        );

        // The alignment is not a detail: it becomes the granularity of the
        // arena allocator, because every imported region must be aligned to it.
        if has_host_import {
            let mut host_props =
                vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
            let mut props2 =
                vk::PhysicalDeviceProperties2::default().push_next(&mut host_props);
            unsafe { instance.get_physical_device_properties2(*pd, &mut props2) };
            println!(
                "    minImportedHostPointerAlignment: {} bytes",
                host_props.min_imported_host_pointer_alignment
            );
        }

        // Which heaps exist, and which are host-visible. Arena-backed memory has
        // to be advertised as one of these, so knowing the real table tells us
        // what our forwarded table may plausibly claim.
        let mem = unsafe { instance.get_physical_device_memory_properties(*pd) };
        println!("    heaps:");
        for h in 0..mem.memory_heap_count as usize {
            let heap = mem.memory_heaps[h];
            println!(
                "      [{h}] {:>6} MiB flags={:?}",
                heap.size / (1024 * 1024),
                heap.flags
            );
        }
        println!("    memory types:");
        for t in 0..mem.memory_type_count as usize {
            let ty = mem.memory_types[t];
            println!("      [{t}] heap={} flags={:?}", ty.heap_index, ty.property_flags);
        }

        // Other extensions that matter to the design, reported together so one
        // run answers every question we currently have.
        for probe in [
            c"VK_KHR_external_memory_win32",
            c"VK_KHR_external_memory_fd",
            c"VK_EXT_external_memory_dma_buf",
            c"VK_KHR_buffer_device_address",
            c"VK_KHR_timeline_semaphore",
        ] {
            let present = exts.iter().any(|e| {
                (unsafe { CStr::from_ptr(e.extension_name.as_ptr()) }) == probe
            });
            println!("    {:<38} {}", probe.to_string_lossy(), if present { "yes" } else { "no" });
        }
        println!("    ({} device extensions total)", exts.len());
    }

    // The extension being advertised is not the same claim as an import
    // succeeding: vkGetMemoryHostPointerPropertiesEXT returning
    // memoryTypeBits == 0 is a legal answer, and some drivers fail the
    // allocation outright (security-model.md R9). So try it for real.
    if let Err(e) = try_import(&instance, devices[0]) {
        println!("\nIMPORT TEST FAILED: {e:#}");
    }

    unsafe { instance.destroy_instance(None) };
    Ok(())
}

/// Allocate ordinary page-aligned memory and ask the driver to import it as
/// `VkDeviceMemory`.
///
/// This is step 0 of the arena, reduced to its smallest honest form. It does
/// NOT test the case the design actually needs — memory that is simultaneously
/// guest RAM and mapped by the host across the hypervisor — because that needs
/// the host side in the loop. It tests the half that can fail on its own: does
/// this driver accept a host pointer at all?
fn try_import(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Result<()> {
    use std::alloc::{Layout, alloc, dealloc};

    println!("\n=== host-pointer import test");

    // Page-aligned and page-sized, because minImportedHostPointerAlignment is
    // 4096 and the import is rejected outright if either is wrong.
    const SIZE: usize = 1024 * 1024;
    let layout = Layout::from_size_align(SIZE, 4096)?;
    let ptr = unsafe { alloc(layout) };
    if ptr.is_null() {
        bail!("allocation failed");
    }
    // Touch every page: the driver has to pin these, and untouched pages of a
    // fresh allocation may not be committed yet.
    unsafe { std::ptr::write_bytes(ptr, 0xAB, SIZE) };

    let queue_prio = [1.0f32];
    let queue_info = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(0)
        .queue_priorities(&queue_prio)];
    let ext_names = [c"VK_EXT_external_memory_host".as_ptr()];
    let dev_info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_info)
        .enabled_extension_names(&ext_names);
    let device = unsafe { instance.create_device(pd, &dev_info, None) }?;

    let host_mem = ash::ext::external_memory_host::Device::new(instance, &device);

    // ash 0.38 ships no safe wrapper for this entry point, only the loaded
    // function pointer, so call it directly.
    let mut host_props = vk::MemoryHostPointerPropertiesEXT::default();
    let res = unsafe {
        (host_mem.fp().get_memory_host_pointer_properties_ext)(
            device.handle(),
            vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
            ptr as *const std::ffi::c_void,
            &mut host_props,
        )
    };
    println!("    getMemoryHostPointerProperties: {res:?}");
    println!("    memoryTypeBits: 0x{:x}", host_props.memory_type_bits);

    if host_props.memory_type_bits == 0 {
        println!("    -> driver accepts no memory type for this pointer (R9 realised)");
        unsafe { device.destroy_device(None); dealloc(ptr, layout) };
        return Ok(());
    }

    // Any type the driver named will do; prefer a host-visible one, since that
    // is what an arena allocation has to be.
    let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
    let chosen = (0..mem_props.memory_type_count).find(|i| {
        let bit = 1u32 << i;
        host_props.memory_type_bits & bit != 0
            && mem_props.memory_types[*i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
    });
    let Some(type_index) = chosen else {
        println!("    -> no host-visible type among the permitted ones");
        unsafe { device.destroy_device(None); dealloc(ptr, layout) };
        return Ok(());
    };
    println!("    importing into memory type {type_index}");

    let mut import =
        vk::ImportMemoryHostPointerInfoEXT::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)
            .host_pointer(ptr as *mut std::ffi::c_void);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(SIZE as u64)
        .memory_type_index(type_index)
        .push_next(&mut import);

    match unsafe { device.allocate_memory(&alloc_info, None) } {
        Ok(mem) => {
            println!("    vkAllocateMemory(import): OK — THE ARENA MECHANISM WORKS");

            // If the import is real, mapping it must show the bytes we wrote
            // through the original pointer: same memory, two names for it.
            match unsafe { device.map_memory(mem, 0, SIZE as u64, vk::MemoryMapFlags::empty()) } {
                Ok(mapped) => {
                    let first = unsafe { *(mapped as *const u8) };
                    let same_address = std::ptr::eq(mapped as *const u8, ptr as *const u8);
                    println!(
                        "    mapped ok: first byte = 0x{first:02x} (wrote 0xAB), \
                         same address = {same_address}"
                    );
                    // Write through the Vulkan mapping, read through ours.
                    unsafe { *(mapped as *mut u8) = 0x5C };
                    let via_original = unsafe { *ptr };
                    println!(
                        "    wrote 0x5C via Vulkan mapping, read 0x{via_original:02x} via \
                         the original pointer -> {}",
                        if via_original == 0x5C { "SHARED, zero-copy confirmed" } else { "NOT shared" }
                    );
                    unsafe { device.unmap_memory(mem) };
                }
                Err(e) => println!("    map failed: {e:?}"),
            }
            unsafe { device.free_memory(mem, None) };
        }
        Err(e) => println!("    vkAllocateMemory(import) FAILED: {e:?}"),
    }

    unsafe { device.destroy_device(None); dealloc(ptr, layout) };
    Ok(())
}
