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

    unsafe { instance.destroy_instance(None) };
    Ok(())
}
