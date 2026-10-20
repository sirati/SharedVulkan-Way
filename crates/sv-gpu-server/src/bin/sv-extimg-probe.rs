//! `sv-extimg-probe` — can the GPU side render into a LINEAR image whose memory
//! is a named shared section, imported as a host pointer? (waycompose-design.md
//! §104)
//!
//! ```text
//! sv-extimg-probe [--device N] [--width W] [--height H] [--image] [--render] [--video]
//! ```
//!
//! KWin's frames are to live in a section the GPU process creates and the
//! compositor opens by name, so that nothing copies them. That needs the driver
//! to bind a `VK_IMAGE_TILING_LINEAR` colour image to memory imported with
//! `VK_EXT_external_memory_host` from a mapped view of that section, and to
//! render into it. The arena already does that for *buffers* on Intel's Windows
//! driver (§87.3); images are a different question for a driver.
//!
//! # The steps, in order of risk, each opt-in past the first
//!
//! 1. **Query** (always): the format's linear features, and
//!    `vkGetPhysicalDeviceImageFormatProperties2` with
//!    `VkPhysicalDeviceExternalImageFormatInfo{HOST_ALLOCATION}` for the
//!    usages KWin needs; `vkGetMemoryHostPointerPropertiesEXT` on a view of a
//!    fresh section. Nothing is created on the device.
//! 2. **`--image`**: create the external LINEAR image only when step 1 said
//!    IMPORTABLE, print its memory requirements and row pitch, destroy it.
//! 3. **`--render`**: import the section view, bind the image, clear it with
//!    `vkCmdClearColorImage`, then clear a sub-rectangle inside a render pass
//!    (the colour-attachment write path), wait, and check the bytes through
//!    the section on the CPU.

use ash::vk;
use std::time::Instant;
use sv_gpu_server::driver::Driver;

const FORMATS: [(vk::Format, &str); 2] =
    [(vk::Format::B8G8R8A8_UNORM, "B8G8R8A8_UNORM"), (vk::Format::R8G8B8A8_UNORM, "R8G8B8A8_UNORM")];

fn usage() -> vk::ImageUsageFlags {
    vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::SAMPLED
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::TRANSFER_DST
}

/// A shared mapping the compositor could open by name.
struct Shared {
    ptr: *mut u8,
    len: usize,
    name: String,
    #[cfg(windows)]
    _section: sv_transport::winshm::Section,
}

impl Shared {
    #[cfg(windows)]
    fn create(len: usize) -> Result<Shared, Box<dyn std::error::Error>> {
        let name = format!("Local\\sv-extimg-probe-{}", std::process::id());
        let s = sv_transport::winshm::Section::create(&name, len as u64)?;
        // SAFETY: the pointer lives as long as `_section`, which `Shared` holds.
        Ok(Shared { ptr: unsafe { s.base_ptr() }, len, name, _section: s })
    }

    #[cfg(unix)]
    fn create(len: usize) -> Result<Shared, Box<dyn std::error::Error>> {
        use std::os::fd::AsRawFd;
        let name = format!("/dev/shm/sv-extimg-probe-{}", std::process::id());
        let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&name)?;
        f.set_len(len as u64)?;
        // SAFETY: a fresh shared mapping of a file we own; checked below.
        let p = unsafe {
            libc::mmap(core::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, f.as_raw_fd(), 0)
        };
        let _ = std::fs::remove_file(&name);
        if p == libc::MAP_FAILED {
            return Err("mmap".into());
        }
        Ok(Shared { ptr: p.cast(), len, name })
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            println!("sv-extimg-probe: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let flag = |k: &str| args.iter().any(|a| a == k);
    let device: usize = arg("--device").map_or(Ok(0), |s| s.parse())?;
    let width: u32 = arg("--width").map_or(Ok(1280), |s| s.parse())?;
    let height: u32 = arg("--height").map_or(Ok(800), |s| s.parse())?;

    let driver = Driver::open(device)?;
    println!(
        "driver: {} ({}), driver version {:#x}, queue family {}",
        driver.name,
        driver.driver_key(),
        driver.props.driver_version,
        driver.queue_family
    );
    let ext = driver.external_host.as_ref().ok_or("no VK_EXT_external_memory_host")?;
    let mut host_props = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
    let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut host_props);
    unsafe { driver.instance.get_physical_device_properties2(driver.pd, &mut p2) };
    let align = host_props.min_imported_host_pointer_alignment.max(4096) as usize;
    println!("minImportedHostPointerAlignment {}", host_props.min_imported_host_pointer_alignment);

    // ---- 0. §112, --video: queries only, for a decoded frame's images ----
    if flag("--video") {
        video_queries(&driver);
        return Ok(());
    }

    // ---- 1. queries ------------------------------------------------------
    let mut importable = None;
    for (format, fname) in FORMATS {
        let fp = unsafe { driver.instance.get_physical_device_format_properties(driver.pd, format) };
        println!("QUERY {fname}: linearTilingFeatures {:?}", fp.linear_tiling_features);
        for (tiling, tname, ext_type) in [
            (vk::ImageTiling::LINEAR, "LINEAR", Some(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)),
            (vk::ImageTiling::LINEAR, "LINEAR (no external)", None),
            (vk::ImageTiling::OPTIMAL, "OPTIMAL", Some(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)),
        ] {
            let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default();
            let mut info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(tiling)
                .usage(usage());
            if let Some(t) = ext_type {
                ext_info = ext_info.handle_type(t);
                info = info.push_next(&mut ext_info);
            }
            let mut ext_props = vk::ExternalImageFormatProperties::default();
            let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
            let r = unsafe {
                driver.instance.get_physical_device_image_format_properties2(driver.pd, &info, &mut props)
            };
            let ifp = props.image_format_properties;
            let em = ext_props.external_memory_properties;
            println!(
                "QUERY {fname} {tname}: {:?} maxExtent {}x{} externalMemoryFeatures {:?} compatible {:?}",
                r.err().unwrap_or(vk::Result::SUCCESS),
                ifp.max_extent.width,
                ifp.max_extent.height,
                em.external_memory_features,
                em.compatible_handle_types
            );
            if r.is_ok()
                && tiling == vk::ImageTiling::LINEAR
                && ext_type.is_some()
                && em.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
                && importable.is_none()
            {
                importable = Some((format, fname));
            }
        }
    }

    let stride_guess = (width as usize * 4).next_multiple_of(256);
    let len = (stride_guess * height as usize + (1 << 20)).next_multiple_of(align.max(65536));
    let shared = Shared::create(len)?;
    println!("section {} ({} bytes)", shared.name, shared.len);
    let mut hp = vk::MemoryHostPointerPropertiesEXT::default();
    let r = unsafe {
        (ext.fp().get_memory_host_pointer_properties_ext)(
            driver.device.handle(),
            vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
            shared.ptr.cast(),
            &mut hp,
        )
    };
    println!("QUERY section view as host pointer: {r:?}, memoryTypeBits {:#x}", hp.memory_type_bits);
    let section_ok = r == vk::Result::SUCCESS && hp.memory_type_bits != 0;

    let Some((format, fname)) = importable else {
        println!("VERDICT: no LINEAR colour format is importable from a host allocation; stopping");
        return Ok(());
    };
    println!("candidate: {fname} LINEAR, HOST_ALLOCATION importable; section host pointer ok: {section_ok}");
    if !flag("--image") {
        println!("no --image: stopping after the queries");
        return Ok(());
    }

    // ---- 2. the image ----------------------------------------------------
    let dev = &driver.device;
    let mut ext_ci =
        vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
    let ci = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D { width, height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut ext_ci);
    let image = unsafe { dev.create_image(&ci, None)? };
    let req = unsafe { dev.get_image_memory_requirements(image) };
    let layout = unsafe {
        dev.get_image_subresource_layout(
            image,
            vk::ImageSubresource { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, array_layer: 0 },
        )
    };
    println!(
        "IMAGE {width}x{height}: size {} alignment {} memoryTypeBits {:#x} (host pointer bits {:#x}, common {:#x}); rowPitch {} offset {}",
        req.size,
        req.alignment,
        req.memory_type_bits,
        hp.memory_type_bits,
        req.memory_type_bits & hp.memory_type_bits,
        layout.row_pitch,
        layout.offset
    );
    let common = req.memory_type_bits & hp.memory_type_bits;
    if !flag("--render") || common == 0 || !section_ok || req.size as usize > shared.len {
        unsafe { dev.destroy_image(image, None) };
        println!("not rendering (--render {}, common bits {common:#x}, section ok {section_ok})", flag("--render"));
        return Ok(());
    }

    // ---- 3. import, bind, render, read back through the section ----------
    let type_index = common.trailing_zeros();
    let import_len = (req.size as usize).next_multiple_of(align) as u64;
    let mut import = vk::ImportMemoryHostPointerInfoEXT::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)
        .host_pointer(shared.ptr.cast());
    let t = Instant::now();
    let memory = unsafe {
        dev.allocate_memory(
            &vk::MemoryAllocateInfo::default().allocation_size(import_len).memory_type_index(type_index).push_next(&mut import),
            None,
        )?
    };
    unsafe { dev.bind_image_memory(image, memory, 0)? };
    println!("IMPORT+BIND ok: {import_len} bytes, memory type {type_index}, {:.2?}", t.elapsed());

    let sub = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    let view = unsafe {
        dev.create_image_view(
            &vk::ImageViewCreateInfo::default().image(image).view_type(vk::ImageViewType::TYPE_2D).format(format).subresource_range(sub),
            None,
        )?
    };
    let attachment = [vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)
        .initial_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .final_layout(vk::ImageLayout::GENERAL)];
    let color_ref = [vk::AttachmentReference { attachment: 0, layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL }];
    let subpass = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_ref)];
    let rp = unsafe {
        dev.create_render_pass(&vk::RenderPassCreateInfo::default().attachments(&attachment).subpasses(&subpass), None)?
    };
    let views = [view];
    let fb = unsafe {
        dev.create_framebuffer(
            &vk::FramebufferCreateInfo::default().render_pass(rp).attachments(&views).width(width).height(height).layers(1),
            None,
        )?
    };
    let pool = unsafe {
        dev.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(driver.queue_family), None)?
    };
    let cb = unsafe {
        dev.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1),
        )?[0]
    };
    let barrier = |cb, old, new, src: vk::AccessFlags, dst: vk::AccessFlags| unsafe {
        dev.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(src)
                .dst_access_mask(dst)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(sub)],
        )
    };
    // The colour written: 0x11223344 as bytes [0x44,0x33,0x22,0x11] in BGRA.
    let full = vk::ClearColorValue { float32: [0.2, 0.4, 0.6, 1.0] };
    let rect = vk::ClearColorValue { float32: [1.0, 0.0, 0.0, 1.0] };
    unsafe {
        dev.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default())?;
        barrier(cb, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE);
        dev.cmd_clear_color_image(cb, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &full, &[sub]);
        barrier(
            cb,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE | vk::AccessFlags::COLOR_ATTACHMENT_READ,
        );
        dev.cmd_begin_render_pass(
            cb,
            &vk::RenderPassBeginInfo::default()
                .render_pass(rp)
                .framebuffer(fb)
                .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: vk::Extent2D { width, height } }),
            vk::SubpassContents::INLINE,
        );
        dev.cmd_clear_attachments(
            cb,
            &[vk::ClearAttachment { aspect_mask: vk::ImageAspectFlags::COLOR, color_attachment: 0, clear_value: vk::ClearValue { color: rect } }],
            &[vk::ClearRect {
                rect: vk::Rect2D { offset: vk::Offset2D { x: 16, y: 16 }, extent: vk::Extent2D { width: 64, height: 32 } },
                base_array_layer: 0,
                layer_count: 1,
            }],
        );
        dev.cmd_end_render_pass(cb);
        dev.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[vk::MemoryBarrier::default().src_access_mask(vk::AccessFlags::MEMORY_WRITE).dst_access_mask(vk::AccessFlags::HOST_READ)],
            &[],
            &[],
        );
        dev.end_command_buffer(cb)?;
    }
    let fence = unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None)? };
    let t = Instant::now();
    unsafe {
        dev.queue_submit(driver.queue, &[vk::SubmitInfo::default().command_buffers(&[cb])], fence)?;
        dev.wait_for_fences(&[fence], true, 5_000_000_000)?;
    }
    println!("RENDER done in {:.2?}", t.elapsed());

    let px = |x: usize, y: usize| -> [u8; 4] {
        let o = layout.offset as usize + y * layout.row_pitch as usize + x * 4;
        // SAFETY: inside the imported, bound range of the live section view.
        unsafe { std::ptr::read_volatile(shared.ptr.add(o).cast::<[u8; 4]>()) }
    };
    let (bg, fg) = (px(width as usize - 1, height as usize - 1), px(20, 20));
    let (r, g, b) = if format == vk::Format::B8G8R8A8_UNORM { (2, 1, 0) } else { (0, 1, 2) };
    let near = |v: u8, want: f32| (v as f32 - want * 255.0).abs() <= 2.0;
    let bg_ok = near(bg[r], 0.2) && near(bg[g], 0.4) && near(bg[b], 0.6) && bg[3] == 255;
    let fg_ok = fg[r] == 255 && fg[g] == 0 && fg[b] == 0 && fg[3] == 255;
    println!("READBACK through the section: background {bg:?} ok={bg_ok}, rectangle {fg:?} ok={fg_ok}");
    println!("VERDICT: {}", if bg_ok && fg_ok { "zero-copy LINEAR rendering into a section works" } else { "MISMATCH" });

    unsafe {
        dev.destroy_fence(fence, None);
        dev.destroy_command_pool(pool, None);
        dev.destroy_framebuffer(fb, None);
        dev.destroy_render_pass(rp, None);
        dev.destroy_image_view(view, None);
        dev.destroy_image(image, None);
        dev.free_memory(memory, None);
    }
    Ok(())
}

/// §112, `--video`: what a VA-API decode through Zink needs of the driver,
/// asked and never created. A decoded NV12 frame that leaves the GPU process
/// as a dma-buf is a LINEAR image on host-allocation memory (a section), and
/// the compositor's side imports its planes as R8 and R8G8 images. Each is
/// asked with `HOST_ALLOCATION` for the transfer and sampling usages, and
/// NV12 also as a decode target with an H.264 profile list, as Zink asks.
/// Every query here is valid usage; nothing touches the device.
fn video_queries(driver: &Driver) {
    let plain = vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED;
    let mut h264 = vk::VideoDecodeH264ProfileInfoKHR::default()
        .std_profile_idc(vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH)
        .picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE);
    let profile = vk::VideoProfileInfoKHR::default()
        .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_H264)
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .push_next(&mut h264);
    let profiles = [profile];
    for (format, fname, usage, video) in [
        (vk::Format::G8_B8R8_2PLANE_420_UNORM, "NV12", plain, false),
        (vk::Format::G8_B8R8_2PLANE_420_UNORM, "NV12 +DECODE_DST", plain | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR, true),
        (vk::Format::R8_UNORM, "R8", plain, false),
        (vk::Format::R8G8_UNORM, "R8G8", plain, false),
    ] {
        for (tiling, tname, host) in [
            (vk::ImageTiling::LINEAR, "LINEAR host", true),
            (vk::ImageTiling::LINEAR, "LINEAR", false),
            (vk::ImageTiling::OPTIMAL, "OPTIMAL", false),
        ] {
            let mut ext_info =
                vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
            let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
            let mut info = vk::PhysicalDeviceImageFormatInfo2::default().format(format).ty(vk::ImageType::TYPE_2D).tiling(tiling).usage(usage);
            if host {
                info = info.push_next(&mut ext_info);
            }
            if video {
                info = info.push_next(&mut list);
            }
            let mut ext_props = vk::ExternalImageFormatProperties::default();
            let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
            // SAFETY: a query on the driver's own physical device, valid chains.
            let r = unsafe { driver.instance.get_physical_device_image_format_properties2(driver.pd, &info, &mut props) };
            let max = props.image_format_properties.max_extent;
            let _ = props;
            let em = ext_props.external_memory_properties;
            println!(
                "VIDEO {fname} {tname}: {:?} maxExtent {}x{} externalMemoryFeatures {:?}",
                r.err().unwrap_or(vk::Result::SUCCESS),
                max.width,
                max.height,
                em.external_memory_features
            );
        }
    }
}
