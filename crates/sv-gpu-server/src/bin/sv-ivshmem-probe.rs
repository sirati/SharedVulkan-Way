//! `sv-ivshmem-probe` — does the guest's Vulkan driver take an ivshmem BAR
//! mapping as a host pointer? (waycompose-design.md §87)
//!
//! ```text
//! sv-ivshmem-probe [--slot 0x1d] [--device N] [--offset BYTES] [--bytes BYTES]
//!                  [--import] [--staging]
//! ```
//!
//! # The question
//!
//! The GPU side imports its whole arena with `VK_EXT_external_memory_host`
//! (`arena::import_whole`): every host-visible allocation an application makes
//! is a sub-range of that one import, and the GPU reads and writes it in place.
//! Until §87 the arena was large-page guest RAM, which Intel's driver was
//! measured importing (docs/design.md §0.1). Moving it onto an ivshmem device
//! changes what the pointer *is*: a user-mode mapping of a PCI BAR that the
//! ivshmem driver made with `MmMapLockedPagesSpecifyCache` over an MDL for I/O
//! space. Whether a driver will pin and translate that is a property of the
//! driver, and docs/design.md §5.1's old reason ivshmem was dropped was
//! exactly this question on Linux, answered no.
//!
//! # The steps, in order of risk, each opt-in past the first
//!
//! 1. **Query** (always): `vkGetMemoryHostPointerPropertiesEXT` on a heap
//!    pointer (the control) and on the BAR. A query; nothing is imported.
//! 2. **`--import`**: `vkAllocateMemory` with the BAR pointer, then one GPU
//!    copy `BAR → device-local → BAR` and a `vkCmdFillBuffer` of a marker into
//!    the BAR, checked by the CPU here and readable on the VM host in the
//!    backing file at the printed offset.
//! 3. **`--staging`**: the cost of the fallback if the import is refused: a
//!    CPU copy between the BAR and driver-allocated host-visible memory.
//!
//! The probe never touches the BAR's first 4 KiB, so a header someone else
//! wrote there survives it.

#[cfg(not(windows))]
fn main() {
    eprintln!("sv-ivshmem-probe: a Windows guest's probe (an ivshmem device is a guest's)");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            println!("sv-ivshmem-probe: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn parse_num(s: &str) -> Result<u64, Box<dyn std::error::Error>> {
    Ok(match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16)?,
        None => s.parse()?,
    })
}

#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use ash::vk;
    use std::sync::Arc;
    use std::time::Instant;
    use sv_gpu_server::{arena, driver::Driver};
    use sv_transport::region::SharedRegion;

    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let flag = |k: &str| args.iter().any(|a| a == k);
    let slot = arg("--slot").map_or(Ok(0x1d), |s| parse_num(&s))? as u8;
    let device: usize = arg("--device").map_or(Ok(0), |s| s.parse())?;
    let offset = arg("--offset").map_or(Ok(1 << 20), |s| parse_num(&s))?;
    let bytes = arg("--bytes").map_or(Ok(4 << 20), |s| parse_num(&s))?;

    let t = Instant::now();
    let (view, size, path) = wc_handle::map_ivshmem_slot(slot)?;
    println!("mapped ivshmem slot {slot:#04x} ({path}): {size} bytes in {:.2?}", t.elapsed());
    if offset < 4096 || offset % 4096 != 0 || offset + bytes > size || bytes % 8192 != 0 {
        return Err(format!("--offset {offset} --bytes {bytes} do not fit a {size}-byte BAR past its header page").into());
    }
    let bar = view.ptr().cast_mut();

    let driver = Driver::open(device)?;
    println!(
        "driver: {} ({}), API {}.{}.{}, driver version {:#x}",
        driver.name,
        driver.driver_key(),
        vk::api_version_major(driver.props.api_version),
        vk::api_version_minor(driver.props.api_version),
        vk::api_version_patch(driver.props.api_version),
        driver.props.driver_version,
    );
    let ext = driver
        .external_host
        .as_ref()
        .ok_or("the driver does not offer VK_EXT_external_memory_host")?;
    let mut host_props = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
    let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut host_props);
    unsafe { driver.instance.get_physical_device_properties2(driver.pd, &mut p2) };
    println!(
        "minImportedHostPointerAlignment {}",
        host_props.min_imported_host_pointer_alignment
    );
    for (i, t) in driver.mem_props.memory_types[..driver.mem_props.memory_type_count as usize]
        .iter()
        .enumerate()
    {
        println!("  memory type {i}: heap {} {:?}", t.heap_index, t.property_flags);
    }

    let query = |ptr: *mut u8| -> (vk::Result, u32) {
        let mut hp = vk::MemoryHostPointerPropertiesEXT::default();
        let r = unsafe {
            (ext.fp().get_memory_host_pointer_properties_ext)(
                driver.device.handle(),
                vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
                ptr.cast(),
                &mut hp,
            )
        };
        (r, hp.memory_type_bits)
    };

    // The control: ordinary heap memory, page-aligned, the kind of pointer the
    // driver was measured accepting.
    let layout = std::alloc::Layout::from_size_align(bytes as usize, 65536)?;
    let heap = unsafe { std::alloc::alloc_zeroed(layout) };
    if heap.is_null() {
        return Err("no heap for the control".into());
    }
    let (r, bits) = query(heap);
    println!("QUERY heap pointer: {r:?}, memoryTypeBits {bits:#x}");
    let (r, bits) = query(unsafe { bar.add(offset as usize) });
    println!("QUERY ivshmem BAR pointer (+{offset:#x}): {r:?}, memoryTypeBits {bits:#x}");
    let bar_ok = r == vk::Result::SUCCESS && bits != 0;

    if flag("--staging") {
        staging(&driver, bar, offset, bytes)?;
    }

    if !flag("--import") {
        println!("no --import: stopping after the query");
        return Ok(());
    }
    if !bar_ok {
        println!("not importing: the query refused the BAR pointer");
        return Ok(());
    }

    // SAFETY: the view is live for the rest of this function and the region
    // only borrows it.
    let region = Arc::new(unsafe { SharedRegion::from_raw(bar, size as usize) });
    let t = Instant::now();
    let imported = arena::import_slab(&driver, &region, offset, bytes);
    let (memory, type_index) = match imported {
        Ok(v) => v,
        Err(e) => {
            println!("IMPORT refused: {e}");
            return Ok(());
        }
    };
    println!("IMPORT ok: {bytes} bytes at +{offset:#x}, memory type {type_index}, in {:.2?}", t.elapsed());

    let half = bytes / 2;
    let dev = &driver.device;
    let mk_buffer = |size: u64, external: bool| -> Result<vk::Buffer, vk::Result> {
        let mut ext_ci = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
        let mut ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        if external {
            ci = ci.push_next(&mut ext_ci);
        }
        unsafe { dev.create_buffer(&ci, None) }
    };
    let src = mk_buffer(half, true)?;
    let dst = mk_buffer(half, true)?;
    unsafe {
        dev.bind_buffer_memory(src, memory, 0)?;
        dev.bind_buffer_memory(dst, memory, half)?;
    }
    let mid = mk_buffer(half, false)?;
    let req = unsafe { dev.get_buffer_memory_requirements(mid) };
    let local = driver
        .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
        .ok_or("no device-local memory type")?;
    let mid_mem = unsafe {
        dev.allocate_memory(
            &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(local),
            None,
        )?
    };
    unsafe { dev.bind_buffer_memory(mid, mid_mem, 0)? };

    // The pattern, written by the CPU through the BAR mapping.
    let words = (half / 4) as usize;
    let seed = 0x5356_4750u32; // "SVGP"
    let pat = |i: usize| (i as u32).wrapping_mul(2_654_435_761) ^ seed;
    unsafe {
        let s = bar.add(offset as usize).cast::<u32>();
        let d = bar.add((offset + half) as usize).cast::<u32>();
        for i in 0..words {
            s.add(i).write_volatile(pat(i));
            d.add(i).write_volatile(0);
        }
    }
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);

    let pool = unsafe {
        dev.create_command_pool(
            &vk::CommandPoolCreateInfo::default().queue_family_index(driver.queue_family),
            None,
        )?
    };
    let cb = unsafe {
        dev.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )?[0]
    };
    let region_all = [vk::BufferCopy { src_offset: 0, dst_offset: 0, size: half }];
    let barrier = |cb| unsafe {
        dev.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::HOST_WRITE)
                .dst_access_mask(
                    vk::AccessFlags::TRANSFER_READ
                        | vk::AccessFlags::TRANSFER_WRITE
                        | vk::AccessFlags::HOST_READ,
                )],
            &[],
            &[],
        )
    };
    // The marker: the last 4 KiB of the destination, filled by the GPU, so the
    // VM host can read GPU-written bytes out of the backing file.
    let marker_off = half - 4096;
    unsafe {
        dev.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default())?;
        barrier(cb);
        dev.cmd_copy_buffer(cb, src, mid, &region_all);
        barrier(cb);
        dev.cmd_copy_buffer(cb, mid, dst, &region_all);
        barrier(cb);
        dev.cmd_fill_buffer(cb, dst, marker_off, 4096, 0x5356_4750);
        barrier(cb);
        dev.end_command_buffer(cb)?;
    }
    let fence = unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None)? };
    let t = Instant::now();
    unsafe {
        dev.queue_submit(driver.queue, &[vk::SubmitInfo::default().command_buffers(&[cb])], fence)?;
    }
    let waited = unsafe { dev.wait_for_fences(&[fence], true, 10_000_000_000) };
    let took = t.elapsed();
    if let Err(e) = waited {
        println!("GPU copy: the fence did not signal: {e:?} after {took:.2?}");
        return Ok(());
    }
    let mut wrong = 0usize;
    let mut first_wrong = None;
    unsafe {
        let d = bar.add((offset + half) as usize).cast::<u32>();
        for i in 0..(marker_off / 4) as usize {
            if d.add(i).read_volatile() != pat(i) {
                wrong += 1;
                first_wrong.get_or_insert(i);
            }
        }
        for i in (marker_off / 4) as usize..words {
            if d.add(i).read_volatile() != 0x5356_4750 {
                wrong += 1;
                first_wrong.get_or_insert(i);
            }
        }
    }
    println!(
        "GPU copy BAR -> device-local -> BAR, {} KiB each way, + fill: {took:.2?}; {wrong} of {words} words wrong{}",
        half >> 10,
        first_wrong.map(|i| format!(" (first at word {i})")).unwrap_or_default()
    );
    println!(
        "host check: bytes {:#x}..{:#x} of the backing file should read 50 47 56 53 repeated",
        offset + half + marker_off,
        offset + bytes
    );

    // Throughput, for the record: the same two copies five more times.
    let mut best = std::time::Duration::MAX;
    for _ in 0..5 {
        unsafe {
            dev.reset_fences(&[fence])?;
            dev.queue_submit(driver.queue, &[vk::SubmitInfo::default().command_buffers(&[cb])], fence)?;
        }
        let t = Instant::now();
        unsafe { dev.wait_for_fences(&[fence], true, 10_000_000_000)? };
        best = best.min(t.elapsed());
    }
    println!(
        "GPU copy, best of 5 (submit to fence): {best:.2?} for 2 x {} KiB = {:.2} GB/s",
        half >> 10,
        (2 * half) as f64 / best.as_secs_f64() / 1e9
    );

    unsafe {
        dev.destroy_fence(fence, None);
        dev.destroy_command_pool(pool, None);
        dev.destroy_buffer(src, None);
        dev.destroy_buffer(dst, None);
        dev.destroy_buffer(mid, None);
        dev.free_memory(mid_mem, None);
        dev.free_memory(memory, None);
        std::alloc::dealloc(heap, layout);
    }
    drop(region);
    drop(view);
    println!("done");
    Ok(())
}

/// The fallback's price: a CPU copy between the BAR and memory the driver
/// allocated and mapped, both directions, best of five.
#[cfg(windows)]
fn staging(
    driver: &sv_gpu_server::driver::Driver,
    bar: *mut u8,
    offset: u64,
    bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    use ash::vk;
    let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    for (label, flags) in [("cached", want | vk::MemoryPropertyFlags::HOST_CACHED), ("coherent", want)] {
        let Some(ty) = driver.memory_type(u32::MAX, flags) else { continue };
        let mem = unsafe {
            driver.device.allocate_memory(
                &vk::MemoryAllocateInfo::default().allocation_size(bytes).memory_type_index(ty),
                None,
            )?
        };
        let p = unsafe { driver.device.map_memory(mem, 0, bytes, vk::MemoryMapFlags::empty())? }.cast::<u8>();
        let mut to = std::time::Duration::MAX;
        let mut from = std::time::Duration::MAX;
        for _ in 0..5 {
            let t = std::time::Instant::now();
            unsafe { std::ptr::copy_nonoverlapping(bar.add(offset as usize), p, bytes as usize) };
            to = to.min(t.elapsed());
            let t = std::time::Instant::now();
            unsafe { std::ptr::copy_nonoverlapping(p, bar.add(offset as usize), bytes as usize) };
            from = from.min(t.elapsed());
        }
        println!(
            "STAGING {label} (type {ty}): BAR -> driver {to:.2?}, driver -> BAR {from:.2?} for {} KiB ({:.2} / {:.2} GB/s)",
            bytes >> 10,
            bytes as f64 / to.as_secs_f64() / 1e9,
            bytes as f64 / from.as_secs_f64() / 1e9,
        );
        unsafe {
            driver.device.unmap_memory(mem);
            driver.device.free_memory(mem, None);
        }
    }
    Ok(())
}
