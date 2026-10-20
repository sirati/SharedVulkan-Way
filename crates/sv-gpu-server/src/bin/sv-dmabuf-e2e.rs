//! `sv-dmabuf-e2e` — the dma-buf path end to end, as Zink and GBM drive it
//! (waycompose-design.md §104).
//!
//! **This is the app side**, like `sv-compute-e2e`: every Vulkan call goes
//! through the real loader into `libsv_icd.so` and across the wire. It needs
//! `SV_DRM_NODE` naming a character device (any: `/dev/null` will do).
//!
//! ```text
//! VK_DRIVER_FILES=target/release/sv_icd.json SV_SOCKET=… SV_DRM_NODE=/dev/null sv-dmabuf-e2e
//! ```
//!
//! What it does is what Zink does for KWin, minus GL:
//!
//! 1. the six extensions, the render node in `VkPhysicalDeviceDrmPropertiesEXT`,
//!    one LINEAR modifier per format, and a `DMA_BUF` image-format answer;
//! 2. a modifier-list image with `DMA_BUF` export, dedicated exportable memory,
//!    `vkGetMemoryFdKHR` → a token, read back to the section's NAME;
//! 3. a second image imported from that token with an explicit plane layout;
//! 4. a clear of the *imported* image behind a `FOREIGN` → graphics barrier,
//!    submitted with an exportable semaphore whose `SYNC_FD` export is -1 at
//!    once (§104.11); then, as the compositor does, the token's sync page and
//!    the fence record it names, until the record says the clear is done;
//! 5. the section opened by NAME on this machine: the pixels are the clear
//!    colour. Nothing copied them there — that is the zero-copy proof;
//! 6. a sync fd imported into a semaphore whose wait is then dropped (no
//!    hang), `vkMapMemory` refused on shared memory, and the section's name
//!    gone once both allocations are freed.
//! 7. §112: an NV12 frame exported as one dma-buf, filled by a copy, read by
//!    name at its reported plane layouts, and imported back plane by plane
//!    (R8 and R8G8 at the chroma offset), as Firefox samples a decoded frame.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sv-dmabuf-e2e: the dma-buf token is a Linux memfd; nothing to test here");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
use ash::vk;

const W: u32 = 256;
const H: u32 = 128;
const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;
/// 0.2, 0.4, 0.6, 0.8 are exact in UNORM8: 51, 102, 153, 204.
const CLEAR: [f32; 4] = [0.2, 0.4, 0.6, 0.8];
/// The clear colour as B8G8R8A8 bytes.
const PIXEL: [u8; 4] = [153, 102, 51, 204];

pub fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => {
            println!("[dmabuf] PASS — the GPU wrote into a named section and this process read it there");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[dmabuf] FAIL: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn check(ok: bool, what: impl std::fmt::Display) -> Result<(), String> {
    if ok {
        println!("[dmabuf] ok   {what}");
        Ok(())
    } else {
        Err(format!("{what}"))
    }
}

#[cfg(target_os = "linux")]
fn node() -> Result<(u32, u32), String> {
    use std::os::unix::fs::MetadataExt;
    let p = std::env::var("SV_DRM_NODE").map_err(|_| "SV_DRM_NODE is not set")?;
    let m = std::fs::metadata(&p).map_err(|e| format!("{p}: {e}"))?;
    Ok((libc::major(m.rdev()), libc::minor(m.rdev())))
}

#[cfg(not(target_os = "linux"))]
fn node() -> Result<(u32, u32), String> {
    Err("Linux only".into())
}

fn run() -> Result<(), String> {
    let (major, minor) = node()?;
    let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("no Vulkan loader: {e}"))?;
    let app = vk::ApplicationInfo::default().application_name(c"sv-dmabuf-e2e").api_version(vk::API_VERSION_1_2);
    let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }
        .map_err(|e| format!("vkCreateInstance: {e:?}"))?;
    let pds = unsafe { instance.enumerate_physical_devices() }.map_err(|e| format!("{e:?}"))?;
    let &pd = pds.first().ok_or("no physical device")?;

    // -- 1. what Zink asks at screen creation --------------------------------
    let exts = unsafe { instance.enumerate_device_extension_properties(pd) }.map_err(|e| format!("{e:?}"))?;
    let has = |n: &str| exts.iter().any(|e| e.extension_name_as_c_str().is_ok_and(|c| c.to_str() == Ok(n)));
    let wanted = [
        c"VK_KHR_external_memory_fd",
        c"VK_EXT_external_memory_dma_buf",
        c"VK_EXT_image_drm_format_modifier",
        c"VK_EXT_queue_family_foreign",
        c"VK_EXT_physical_device_drm",
        c"VK_KHR_external_semaphore_fd",
    ];
    for w in &wanted {
        check(has(w.to_str().unwrap()), format_args!("{} is advertised", w.to_str().unwrap()))?;
    }
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    unsafe { instance.get_physical_device_properties2(pd, &mut p2) };
    let _ = p2;
    check(
        drm.has_render == vk::TRUE && drm.render_major == i64::from(major) && drm.render_minor == i64::from(minor),
        format_args!(
            "VkPhysicalDeviceDrmPropertiesEXT names the node: render {}:{} (want {major}:{minor}), primary {}",
            drm.render_major, drm.render_minor, drm.has_primary
        ),
    )?;

    // Zink's way: one call, room for 128.
    let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); 128];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
    let mut fp3 = vk::FormatProperties3::default();
    let mut fp2 = vk::FormatProperties2::default().push_next(&mut list).push_next(&mut fp3);
    unsafe { instance.get_physical_device_format_properties2(pd, FORMAT, &mut fp2) };
    let linear = fp2.format_properties.linear_tiling_features;
    let _ = fp2;
    let n = list.drm_format_modifier_count as usize;
    check(
        n == 1
            && mods[0].drm_format_modifier == 0
            && mods[0].drm_format_modifier_plane_count == 1
            && mods[0].drm_format_modifier_tiling_features == linear
            && linear.contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT),
        format_args!(
            "B8G8R8A8: {n} modifier(s), LINEAR with the linear features {:#x}",
            mods[0].drm_format_modifier_tiling_features.as_raw()
        ),
    )?;
    // And the count-only form.
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
    unsafe { instance.get_physical_device_format_properties2(pd, FORMAT, &mut fp2) };
    let _ = fp2;
    check(list.drm_format_modifier_count == 1, "the count-only modifier query says 1")?;

    let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::TRANSFER_DST
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::SAMPLED;
    let mut modinfo = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(0)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut extinfo = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let ifi = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(FORMAT)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .push_next(&mut modinfo)
        .push_next(&mut extinfo);
    let mut eifp = vk::ExternalImageFormatProperties::default();
    let mut ifp = vk::ImageFormatProperties2::default().push_next(&mut eifp);
    let r = unsafe { instance.get_physical_device_image_format_properties2(pd, &ifi, &mut ifp) };
    let max = ifp.image_format_properties.max_extent;
    let _ = ifp;
    let em = eifp.external_memory_properties;
    check(
        r.is_ok()
            && em.external_memory_features.contains(
                vk::ExternalMemoryFeatureFlags::EXPORTABLE | vk::ExternalMemoryFeatureFlags::IMPORTABLE,
            )
            && em.compatible_handle_types == vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT
            && max.width >= W,
        format_args!(
            "a LINEAR-modifier DMA_BUF image: {r:?}, features {:#x}, compatible {:#x}, max {}x{}",
            em.external_memory_features.as_raw(),
            em.compatible_handle_types.as_raw(),
            max.width,
            max.height
        ),
    )?;
    // Another modifier is not supported.
    let mut bad = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default().drm_format_modifier(0x0100_0000_0000_0001);
    let ifi_bad = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(FORMAT)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .push_next(&mut bad);
    let mut ifp = vk::ImageFormatProperties2::default();
    let r = unsafe { instance.get_physical_device_image_format_properties2(pd, &ifi_bad, &mut ifp) };
    check(r == Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED), format_args!("an X-tiled modifier is refused ({r:?})"))?;
    let mut sem_props = vk::ExternalSemaphoreProperties::default();
    unsafe {
        instance.get_physical_device_external_semaphore_properties(
            pd,
            &vk::PhysicalDeviceExternalSemaphoreInfo::default()
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
            &mut sem_props,
        )
    };
    check(
        sem_props.external_semaphore_features.contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE),
        "a SYNC_FD semaphore is exportable",
    )?;

    // -- the device -----------------------------------------------------------
    let names: Vec<*const std::ffi::c_char> = wanted.iter().filter(|n| **n != c"VK_EXT_physical_device_drm").map(|n| n.as_ptr()).collect();
    let prio = [1.0f32];
    let qi = [vk::DeviceQueueCreateInfo::default().queue_family_index(0).queue_priorities(&prio)];
    let device = unsafe {
        instance.create_device(
            pd,
            &vk::DeviceCreateInfo::default().queue_create_infos(&qi).enabled_extension_names(&names),
            None,
        )
    }
    .map_err(|e| format!("vkCreateDevice with the dma-buf extensions: {e:?}"))?;
    let queue = unsafe { device.get_device_queue(0, 0) };
    let r = work(&instance, &device, queue).and_then(|()| nv12(&instance, pd, &device, queue));
    unsafe {
        let _ = device.device_wait_idle();
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    r
}

struct Bound {
    image: vk::Image,
    memory: vk::DeviceMemory,
}

fn work(instance: &ash::Instance, d: &ash::Device, queue: vk::Queue) -> Result<(), String> {
    let memfd = ash::khr::external_memory_fd::Device::new(instance, d);
    let modifier = ash::ext::image_drm_format_modifier::Device::new(instance, d);
    let semfd = ash::khr::external_semaphore_fd::Device::new(instance, d);
    let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC;

    // -- 2. GBM's allocation: a modifier list, DMA_BUF export ----------------
    let mods = [0x0100_0000_0000_0002u64, 0]; // Y-tiled first, as a GBM caller might list
    let mut list = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&mods);
    let mut ext = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let ci = image_info(usage).push_next(&mut list).push_next(&mut ext);
    let image_a = unsafe { d.create_image(&ci, None) }.map_err(|e| format!("vkCreateImage(export): {e:?}"))?;
    let mut mp = vk::ImageDrmFormatModifierPropertiesEXT::default();
    unsafe { modifier.get_image_drm_format_modifier_properties(image_a, &mut mp) }.map_err(|e| format!("{e:?}"))?;
    check(mp.drm_format_modifier == 0, "the exported image's modifier is LINEAR")?;
    let a = allocate(d, image_a, None)?;
    let fd = unsafe {
        memfd.get_memory_fd(
            &vk::MemoryGetFdInfoKHR::default().memory(a.memory).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
        )
    }
    .map_err(|e| format!("vkGetMemoryFdKHR: {e:?}"))?;
    // SAFETY: the descriptor vkGetMemoryFdKHR just gave us.
    let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let token = sv_drm::token::read(std::os::fd::AsFd::as_fd(&owned)).map_err(|e| format!("the fd is not a token: {e}"))?;
    check(
        token.is_path() && token.name.starts_with("/dev/shm/sv-shared-"),
        format_args!("vkGetMemoryFdKHR gave a sealed token: {} bytes, name {}", token.size, token.name),
    )?;
    let fd2 = unsafe {
        memfd.get_memory_fd(
            &vk::MemoryGetFdInfoKHR::default().memory(a.memory).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
        )
    }
    .map_err(|e| format!("vkGetMemoryFdKHR (2nd): {e:?}"))?;
    check(fd2 != fd, "a second vkGetMemoryFdKHR gives a new descriptor")?;
    let mut props = vk::MemoryFdPropertiesKHR::default();
    unsafe { memfd.get_memory_fd_properties(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, fd2, &mut props) }
        .map_err(|e| format!("vkGetMemoryFdPropertiesKHR: {e:?}"))?;
    check(props.memory_type_bits == 1, format_args!("the token is memory type 0 ({:#x})", props.memory_type_bits))?;

    let layout = unsafe {
        d.get_image_subresource_layout(
            image_a,
            vk::ImageSubresource { aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT, mip_level: 0, array_layer: 0 },
        )
    };
    check(
        layout.row_pitch >= u64::from(W) * 4 && layout.offset == 0,
        format_args!("plane 0: offset {} row pitch {}", layout.offset, layout.row_pitch),
    )?;

    // A wrong plane layout on import is the specification's error.
    let wrong = [vk::SubresourceLayout { offset: 0, size: 0, row_pitch: layout.row_pitch + 64, array_pitch: 0, depth_pitch: 0 }];
    let mut ex = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(0).plane_layouts(&wrong);
    let mut ext = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let ci_bad = image_info(usage).push_next(&mut ex).push_next(&mut ext);
    let r = unsafe { d.create_image(&ci_bad, None) };
    check(
        r == Err(vk::Result::ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT),
        format_args!("an import with another row pitch is refused ({:?})", r.map(|_| ())),
    )?;

    // -- 3. the import, as EGL's dma-buf import makes it ---------------------
    let planes = [vk::SubresourceLayout { offset: layout.offset, size: 0, row_pitch: layout.row_pitch, array_pitch: 0, depth_pitch: 0 }];
    let mut ex = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(0).plane_layouts(&planes);
    let mut ext = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let ci_b = image_info(usage).push_next(&mut ex).push_next(&mut ext);
    let image_b = unsafe { d.create_image(&ci_b, None) }.map_err(|e| format!("vkCreateImage(import): {e:?}"))?;
    let b = allocate(d, image_b, Some(fd2))?;
    println!("[dmabuf] imported the token into a second image");

    // -- 4. clear the imported image, as a frame would -----------------------
    let pool = unsafe {
        d.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(0), None)
    }
    .map_err(|e| format!("{e:?}"))?;
    let cb = unsafe {
        d.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1))
    }
    .map_err(|e| format!("{e:?}"))?[0];
    let range = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    unsafe {
        d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).map_err(|e| format!("{e:?}"))?;
        // Acquire from FOREIGN, as Zink does for an imported dma-buf.
        let acquire = vk::ImageMemoryBarrier::default()
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(0)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .image(image_b)
            .subresource_range(range);
        d.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[acquire],
        );
        d.cmd_clear_color_image(
            cb,
            image_b,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &vk::ClearColorValue { float32: CLEAR },
            &[range],
        );
        // Release to FOREIGN, through the synchronization2 form.
        let release = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_queue_family_index(0)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .image(image_b)
            .subresource_range(range);
        let rel = [release];
        d.cmd_pipeline_barrier2(cb, &vk::DependencyInfo::default().image_memory_barriers(&rel));
        d.end_command_buffer(cb).map_err(|e| format!("{e:?}"))?;
    }
    let mut esci = vk::ExportSemaphoreCreateInfo::default().handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let sem = unsafe { d.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut esci), None) }
        .map_err(|e| format!("vkCreateSemaphore(export SYNC_FD): {e:?}"))?;
    let cbs = [cb];
    let sigs = [sem];
    let submit = [vk::SubmitInfo::default().command_buffers(&cbs).signal_semaphores(&sigs)];
    let t0 = std::time::Instant::now();
    unsafe { d.queue_submit(queue, &submit, vk::Fence::null()) }.map_err(|e| format!("vkQueueSubmit: {e:?}"))?;
    let sync = unsafe {
        semfd.get_semaphore_fd(
            &vk::SemaphoreGetFdInfoKHR::default().semaphore(sem).handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
        )
    }
    .map_err(|e| format!("vkGetSemaphoreFdKHR(SYNC_FD): {e:?}"))?;
    // What `zink_screen_import_dmabuf_semaphore` does next: the memory's fd,
    // for DMA_BUF_IOCTL_IMPORT_SYNC_FILE (which fails ENOTTY on a memfd).
    let fd3 = unsafe {
        memfd.get_memory_fd(
            &vk::MemoryGetFdInfoKHR::default().memory(b.memory).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
        )
    }
    .map_err(|e| format!("vkGetMemoryFdKHR after the export: {e:?}"))?;
    let exported_in = t0.elapsed();
    unsafe { libc::close(fd3) };
    check(sync == -1, format_args!("the SYNC_FD export is -1, returned without waiting, after {exported_in:.1?}"))?;

    // -- 4b. what the compositor does (§104.11): the number from the token's
    // sync page, then the fence record until it says the work is complete.
    check(
        sv_drm::token::record_name_ok(&token.record),
        format_args!("the token names the session's fence record {}", token.record),
    )?;
    let page = sv_drm::token::SyncPage::open(std::os::fd::AsFd::as_fd(&owned))
        .map_err(|e| format!("the sync page: {e}"))?
        .ok_or("a version-1 token: no sync page")?;
    let seq = page.load();
    check(seq > 0, format_args!("the export stored work number {seq} in the token's sync page"))?;
    let waited = wait_record(&token.record, seq, std::time::Duration::from_secs(20))?;
    check(true, format_args!("the fence record reached {seq} {waited:.1?} after the export returned"))?;

    // -- 5. the pixels, read from the section by name ------------------------
    let bytes = std::fs::read(&token.name).map_err(|e| format!("open {}: {e}", token.name))?;
    check(bytes.len() as u64 >= token.size, format_args!("{} is {} bytes", token.name, bytes.len()))?;
    let mut wrong = 0usize;
    for y in 0..H as usize {
        let row = layout.offset as usize + y * layout.row_pitch as usize;
        for x in 0..W as usize {
            if bytes[row + x * 4..row + x * 4 + 4] != PIXEL {
                wrong += 1;
            }
        }
    }
    let first = &bytes[layout.offset as usize..layout.offset as usize + 4];
    check(
        wrong == 0,
        format_args!(
            "every one of {} pixels in the section is the clear colour {PIXEL:?} (first {first:?}, {wrong} wrong) — written by the GPU, read here, copied by nobody",
            W * H
        ),
    )?;

    // -- 6. the rest ----------------------------------------------------------
    let r = unsafe { d.map_memory(b.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) };
    check(r.is_err_and(|e| e == vk::Result::ERROR_MEMORY_MAP_FAILED), "vkMapMemory of a dma-buf allocation is refused")?;

    let sem2 = unsafe { d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }.map_err(|e| format!("{e:?}"))?;
    unsafe {
        semfd.import_semaphore_fd(
            &vk::ImportSemaphoreFdInfoKHR::default()
                .semaphore(sem2)
                .flags(vk::SemaphoreImportFlags::TEMPORARY)
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
                .fd(-1),
        )
    }
    .map_err(|e| format!("vkImportSemaphoreFdKHR(SYNC_FD, -1): {e:?}"))?;
    let fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None) }.map_err(|e| format!("{e:?}"))?;
    let waits = [sem2];
    let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
    let submit = [vk::SubmitInfo::default().wait_semaphores(&waits).wait_dst_stage_mask(&stages)];
    unsafe { d.queue_submit(queue, &submit, fence) }.map_err(|e| format!("{e:?}"))?;
    let r = unsafe { d.wait_for_fences(&[fence], true, 5_000_000_000) };
    check(r.is_ok(), format_args!("a wait on an imported sync fd is dropped, not hung ({r:?})"))?;

    unsafe {
        d.destroy_fence(fence, None);
        d.destroy_semaphore(sem2, None);
        d.destroy_semaphore(sem, None);
        d.destroy_command_pool(pool, None);
        d.destroy_image(image_b, None);
        d.free_memory(b.memory, None);
        let _ = d.device_wait_idle();
    }
    check(std::path::Path::new(&token.name).exists(), "the section outlives the import while the export lives")?;
    unsafe {
        d.destroy_image(a.image, None);
        d.free_memory(a.memory, None);
        let _ = d.device_wait_idle();
    }
    check(!std::path::Path::new(&token.name).exists(), "the section's name is gone once both allocations are freed")?;
    let _ = image_a;
    Ok(())
}


/// §112, step 7: a video frame's way. An NV12 image made the way Firefox's
/// Vulkan decoder and Zink's VA-API export make theirs (LINEAR modifier,
/// DMA_BUF export, one dma-buf with two memory planes), filled by a copy, and
/// imported back the way Firefox's compositor samples it: plane 0 as an R8
/// image, plane 1 as an R8G8 image at the chroma plane's offset in the same
/// dma-buf. Both imports read back what the copy wrote; the section, read by
/// name, holds it at the layouts the export reported.
fn nv12(instance: &ash::Instance, pd: vk::PhysicalDevice, d: &ash::Device, queue: vk::Queue) -> Result<(), String> {
    const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
    const Y: u8 = 0x50;
    const UV: [u8; 2] = [0x60, 0x70];
    let memfd = ash::khr::external_memory_fd::Device::new(instance, d);

    let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); 8];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
    let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
    unsafe { instance.get_physical_device_format_properties2(pd, NV12, &mut fp2) };
    let _ = fp2;
    let n = list.drm_format_modifier_count as usize;
    check(
        n == 1 && mods[0].drm_format_modifier == 0 && mods[0].drm_format_modifier_plane_count == 2,
        format_args!("NV12: {n} modifier(s), LINEAR with {} memory planes", mods[0].drm_format_modifier_plane_count),
    )?;

    let usage = vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC;
    let lin = [0u64];
    let mut list = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&lin);
    let mut ext = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let ci = image_info(usage).format(NV12).push_next(&mut list).push_next(&mut ext);
    let frame = unsafe { d.create_image(&ci, None) }.map_err(|e| format!("vkCreateImage(NV12 export): {e:?}"))?;
    let f = allocate(d, frame, None)?;
    let fd = unsafe {
        memfd.get_memory_fd(&vk::MemoryGetFdInfoKHR::default().memory(f.memory).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT))
    }
    .map_err(|e| format!("vkGetMemoryFdKHR(NV12): {e:?}"))?;
    // SAFETY: the descriptor vkGetMemoryFdKHR just gave us.
    let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let token = sv_drm::token::read(std::os::fd::AsFd::as_fd(&owned)).map_err(|e| format!("NV12 fd is not a token: {e}"))?;
    let plane = |a: vk::ImageAspectFlags| unsafe {
        d.get_image_subresource_layout(frame, vk::ImageSubresource { aspect_mask: a, mip_level: 0, array_layer: 0 })
    };
    let p0 = plane(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT);
    let p1 = plane(vk::ImageAspectFlags::MEMORY_PLANE_1_EXT);
    check(
        p0.row_pitch >= u64::from(W) && p1.row_pitch >= u64::from(W) && p1.offset >= p0.offset + p0.row_pitch * u64::from(H),
        format_args!("NV12 memory planes: Y at {} pitch {}, CbCr at {} pitch {}", p0.offset, p0.row_pitch, p1.offset, p1.row_pitch),
    )?;

    // A host-visible staging buffer, both ways.
    let len = (W * H + W * H / 2) as u64;
    let props = unsafe { instance.get_physical_device_memory_properties(pd) };
    let host = (0..props.memory_type_count)
        .find(|&i| {
            props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)
        })
        .ok_or("no host-visible memory type")?;
    let staging = unsafe {
        d.create_buffer(
            &vk::BufferCreateInfo::default().size(len).usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST),
            None,
        )
    }
    .map_err(|e| format!("{e:?}"))?;
    let req = unsafe { d.get_buffer_memory_requirements(staging) };
    let smem = unsafe { d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(host), None) }
        .map_err(|e| format!("{e:?}"))?;
    unsafe { d.bind_buffer_memory(staging, smem, 0) }.map_err(|e| format!("{e:?}"))?;
    let ptr = unsafe { d.map_memory(smem, 0, len, vk::MemoryMapFlags::empty()) }.map_err(|e| format!("{e:?}"))? as *mut u8;
    // SAFETY: a mapping of `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts_mut(ptr, len as usize) };
    let ylen = (W * H) as usize;
    bytes[..ylen].fill(Y);
    for c in bytes[ylen..].chunks_mut(2) {
        c.copy_from_slice(&UV);
    }

    let pool = unsafe { d.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(0), None) }.map_err(|e| format!("{e:?}"))?;
    let run = |rec: &dyn Fn(vk::CommandBuffer)| -> Result<(), String> {
        let cb = unsafe { d.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1)) }
            .map_err(|e| format!("{e:?}"))?[0];
        unsafe { d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()) }.map_err(|e| format!("{e:?}"))?;
        rec(cb);
        unsafe { d.end_command_buffer(cb) }.map_err(|e| format!("{e:?}"))?;
        let fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None) }.map_err(|e| format!("{e:?}"))?;
        let cbs = [cb];
        unsafe { d.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&cbs)], fence) }.map_err(|e| format!("{e:?}"))?;
        let r = unsafe { d.wait_for_fences(&[fence], true, 10_000_000_000) };
        unsafe { d.destroy_fence(fence, None) };
        r.map_err(|e| format!("the copy's fence: {e:?}"))
    };
    let barrier = |img: vk::Image, aspect: vk::ImageAspectFlags, old: vk::ImageLayout, new: vk::ImageLayout| {
        vk::ImageMemoryBarrier::default()
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .old_layout(old)
            .new_layout(new)
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .image(img)
            .subresource_range(vk::ImageSubresourceRange { aspect_mask: aspect, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 })
    };
    let region = |off: u64, aspect: vk::ImageAspectFlags, w: u32, h: u32| vk::BufferImageCopy {
        buffer_offset: off,
        buffer_row_length: 0,
        buffer_image_height: 0,
        image_subresource: vk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: 0, base_array_layer: 0, layer_count: 1 },
        image_offset: vk::Offset3D::default(),
        image_extent: vk::Extent3D { width: w, height: h, depth: 1 },
    };
    run(&|cb| unsafe {
        let b = [
            barrier(frame, vk::ImageAspectFlags::PLANE_0, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL),
            barrier(frame, vk::ImageAspectFlags::PLANE_1, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL),
        ];
        d.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &b);
        d.cmd_copy_buffer_to_image(
            cb,
            staging,
            frame,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[region(0, vk::ImageAspectFlags::PLANE_0, W, H), region(ylen as u64, vk::ImageAspectFlags::PLANE_1, W / 2, H / 2)],
        );
    })?;

    // The section, by name, at the layouts the export reported.
    let sec = std::fs::read(&token.name).map_err(|e| format!("open {}: {e}", token.name))?;
    let mut wrong = 0;
    for y in 0..H as usize {
        for x in 0..W as usize {
            wrong += usize::from(sec[p0.offset as usize + y * p0.row_pitch as usize + x] != Y);
        }
    }
    for y in 0..(H / 2) as usize {
        for x in 0..(W / 2) as usize {
            let o = p1.offset as usize + y * p1.row_pitch as usize + x * 2;
            wrong += usize::from(sec[o..o + 2] != UV);
        }
    }
    check(wrong == 0, format_args!("the NV12 frame is in the section at the reported layouts ({wrong} samples wrong)"))?;

    // Firefox's compositor: each plane its own image on the same dma-buf.
    for (fmt, pl, w, h, want) in [
        (vk::Format::R8_UNORM, p0, W, H, vec![Y]),
        (vk::Format::R8G8_UNORM, p1, W / 2, H / 2, UV.to_vec()),
    ] {
        let layouts = [vk::SubresourceLayout { offset: pl.offset, size: 0, row_pitch: pl.row_pitch, array_pitch: 0, depth_pitch: 0 }];
        let mut ex = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(0).plane_layouts(&layouts);
        let mut ext = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let ci = image_info(usage).format(fmt).extent(vk::Extent3D { width: w, height: h, depth: 1 }).push_next(&mut ex).push_next(&mut ext);
        let img = unsafe { d.create_image(&ci, None) }
            .map_err(|e| format!("vkCreateImage({fmt:?} import at offset {} pitch {}): {e:?}", pl.offset, pl.row_pitch))?;
        let dup = unsafe { libc::dup(fd) };
        let b = allocate(d, img, Some(dup))?;
        bytes.fill(0);
        run(&|cb| unsafe {
            let bar = [barrier(img, vk::ImageAspectFlags::COLOR, vk::ImageLayout::GENERAL, vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
            d.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &bar);
            d.cmd_copy_image_to_buffer(cb, img, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, staging, &[region(0, vk::ImageAspectFlags::COLOR, w, h)]);
        })?;
        let n = (w * h) as usize * want.len();
        let bad = bytes[..n].chunks(want.len()).filter(|c| *c != want.as_slice()).count();
        check(bad == 0, format_args!("{fmt:?} imported at offset {} reads the plane back ({bad} of {} texels wrong)", pl.offset, w * h))?;
        unsafe {
            d.destroy_image(img, None);
            d.free_memory(b.memory, None);
        }
    }
    unsafe {
        let _ = d.device_wait_idle();
        d.destroy_command_pool(pool, None);
        d.unmap_memory(smem);
        d.destroy_buffer(staging, None);
        d.free_memory(smem, None);
        d.destroy_image(frame, None);
        d.free_memory(f.memory, None);
    }
    Ok(())
}
/// Wait, as the compositor does, until the fence record at `path` says
/// `completed >= seq`; how long that took.
pub fn wait_record(path: &str, seq: u64, limit: std::time::Duration) -> Result<std::time::Duration, String> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).map_err(|e| format!("open the fence record {path}: {e}"))?;
    let t = std::time::Instant::now();
    let mut b = [0u8; 16];
    loop {
        f.read_exact_at(&mut b, 0).map_err(|e| format!("read {path}: {e}"))?;
        if &b[..8] != b"SVFENCE1" {
            return Err(format!("{path} is not a fence record"));
        }
        let done = u64::from_le_bytes(b[8..16].try_into().unwrap());
        if done >= seq {
            return Ok(t.elapsed());
        }
        if t.elapsed() > limit {
            return Err(format!("{path} stayed at {done}, below {seq}, for {limit:?}"));
        }
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
}

/// Dedicated type-0 memory for `image`, exportable as a dma-buf, or imported
/// from `fd` — the chain Zink builds in either case.
fn allocate(d: &ash::Device, image: vk::Image, fd: Option<i32>) -> Result<Bound, String> {
    let mut ded_req = vk::MemoryDedicatedRequirements::default();
    let mut req = vk::MemoryRequirements2::default().push_next(&mut ded_req);
    unsafe { d.get_image_memory_requirements2(&vk::ImageMemoryRequirementsInfo2::default().image(image), &mut req) };
    let r = req.memory_requirements;
    let _ = req;
    check(
        r.memory_type_bits == 1 && ded_req.requires_dedicated_allocation == vk::TRUE,
        format_args!(
            "a dma-buf image wants type 0 only ({:#x}), dedicated, {} bytes",
            r.memory_type_bits, r.size
        ),
    )?;
    let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mut import = vk::ImportMemoryFdInfoKHR::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT).fd(fd.unwrap_or(-1));
    let mut ai = vk::MemoryAllocateInfo::default().allocation_size(r.size).memory_type_index(0);
    if fd.is_some() {
        ai = ai.push_next(&mut import);
    }
    let ai = ai.push_next(&mut export).push_next(&mut ded);
    let memory = unsafe { d.allocate_memory(&ai, None) }.map_err(|e| format!("vkAllocateMemory(dma-buf): {e:?}"))?;
    unsafe { d.bind_image_memory(image, memory, 0) }.map_err(|e| format!("vkBindImageMemory: {e:?}"))?;
    Ok(Bound { image, memory })
}

fn image_info<'a>(usage: vk::ImageUsageFlags) -> vk::ImageCreateInfo<'a> {
    vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(FORMAT)
        .extent(vk::Extent3D { width: W, height: H, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
}
}
