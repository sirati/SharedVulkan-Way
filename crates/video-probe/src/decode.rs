//! Decode H.264 through `VK_KHR_video_decode_queue`, and write the frames out.
//!
//! Scope, stated before the code rather than discovered in it:
//!
//! - **One codec.** H.264, 4:2:0 8-bit, progressive.
//! - **Intra only.** Every access unit must be an IDR. The fixture is encoded
//!   with `keyint=1` so that it is. Inter prediction needs reference picture
//!   list construction and picture-order-count arithmetic, which is a decoder
//!   and not a probe, and getting it half right produces a picture that looks
//!   nearly correct — the worst kind of result.
//! - **One slice per picture is not required**; multiple slices per picture are
//!   handled, because `pSliceOffsets` is an array and refusing to fill it in
//!   would be a self-inflicted limit.
//!
//! The output is NV12 (or whatever the driver's decode DST format is, reported
//! per run) at the *coded* size, cropped to the display size on the way out, so
//! that it can be compared byte-for-byte against a software decode.

use crate::caps::{CodecProfile, Probe, with_profile};
use crate::h264;
use anyhow::{Context, Result, anyhow, bail};
use ash::vk;
use std::ffi::CStr;

/// The `StdVideo*Flags` structures are bindgen bitfield wrappers over a byte
/// array, and bindgen gives them neither `Default` nor a field a caller can
/// write. All-zero is the correct starting point — every flag in them is a
/// boolean whose "absent" value is 0 — and there is no safe spelling of it.
fn zeroed_flags<T: Copy>() -> T {
    // SAFETY: `T` is one of the four `StdVideo*Flags` structs, each of which is
    // a `#[repr(C)]` aggregate of `[u8; N]` and padding. Every bit pattern is
    // valid, so zero is valid.
    unsafe { std::mem::zeroed() }
}

/// Everything the caller gets back, so that the harness can assert on it.
pub struct Decoded {
    pub device_name: String,
    pub frames: usize,
    pub coded: (u32, u32),
    pub display: (u32, u32),
    pub format: vk::Format,
    pub bytes_in: usize,
    pub bytes_out: usize,
    pub submit_ms: Vec<f64>,
}

pub fn run(input: &str, out: Option<&str>) -> Result<()> {
    let bitstream = std::fs::read(input).with_context(|| format!("reading {input}"))?;
    let d = decode(&bitstream, out)?;

    let mut ms = d.submit_ms.clone();
    ms.sort_by(f64::total_cmp);
    let median = ms.get(ms.len() / 2).copied().unwrap_or(f64::NAN);
    println!(
        "decoded {} frames of {}x{} (coded {}x{}) as {:?} on {}",
        d.frames, d.display.0, d.display.1, d.coded.0, d.coded.1, d.format, d.device_name
    );
    println!(
        "bitstream {} bytes in, {} bytes of frames out — {:.1}x expansion",
        d.bytes_in,
        d.bytes_out,
        d.bytes_out as f64 / d.bytes_in as f64
    );
    println!(
        "decode submit->fence: median {median:.3} ms over {} frames",
        d.submit_ms.len()
    );
    Ok(())
}

fn decode(bitstream: &[u8], out: Option<&str>) -> Result<Decoded> {
    let nals = h264::split_annex_b(bitstream);
    if nals.is_empty() {
        bail!("no NAL units — is this an Annex B elementary stream?");
    }

    let sps_nal = nals
        .iter()
        .find(|n| n.nal_unit_type == 7)
        .ok_or_else(|| anyhow!("no SPS in the stream"))?;
    let pps_nal = nals
        .iter()
        .find(|n| n.nal_unit_type == 8)
        .ok_or_else(|| anyhow!("no PPS in the stream"))?;
    let sps = h264::parse_sps(&h264::rbsp(&bitstream[sps_nal.payload..sps_nal.end]))?;
    let pps = h264::parse_pps(&h264::rbsp(&bitstream[pps_nal.payload..pps_nal.end]))?;
    if sps.chroma_format_idc != 1 || sps.bit_depth_luma_minus8 != 0 {
        bail!(
            "only 4:2:0 8-bit is handled; this stream is chroma_format_idc {} bit_depth_luma {}",
            sps.chroma_format_idc,
            sps.bit_depth_luma_minus8 + 8
        );
    }
    if !sps.frame_mbs_only_flag {
        bail!("this stream is not frame-only coded; field decoding is out of scope");
    }

    // Group slices into access units. A slice with first_mb_in_slice == 0
    // starts a new picture; anything else continues the current one.
    let mut frames: Vec<Vec<h264::Nal>> = Vec::new();
    for n in &nals {
        if n.nal_unit_type != 1 && n.nal_unit_type != 5 {
            continue;
        }
        let hdr = h264::parse_slice_header(
            &h264::rbsp(&bitstream[n.payload..n.end]),
            n.nal_unit_type == 5,
            &sps,
            &pps,
        )?;
        if hdr.first_mb_in_slice == 0 {
            frames.push(vec![*n]);
        } else if let Some(f) = frames.last_mut() {
            f.push(*n);
        }
    }
    if frames.is_empty() {
        bail!("no slices in the stream");
    }
    // Refuse a stream with inter prediction rather than decode it wrongly.
    for f in &frames {
        if f[0].nal_unit_type != 5 {
            bail!(
                "this stream has a non-IDR picture; encode the fixture with keyint=1 \
                 (see docs/video-decode.md — inter prediction is out of scope)"
            );
        }
    }

    let (coded_w, coded_h) = sps.coded_size();
    let display = sps.display_size();

    let profile = Probe {
        label: "h264 decode",
        op: vk::VideoCodecOperationFlagsKHR::DECODE_H264,
        chroma: vk::VideoChromaSubsamplingFlagsKHR::TYPE_420,
        luma_depth: vk::VideoComponentBitDepthFlagsKHR::TYPE_8,
        chroma_depth: vk::VideoComponentBitDepthFlagsKHR::TYPE_8,
        codec: CodecProfile::H264 {
            idc: match sps.profile_idc {
                66 => vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_BASELINE,
                77 => vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN,
                _ => vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
            },
            layout: vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE,
        },
    };

    // SAFETY: the loader is loaded once and lives for the whole process.
    let entry = unsafe { ash::Entry::load() }.context("loading the Vulkan loader")?;
    let app = vk::ApplicationInfo::default()
        .application_name(c"video-probe")
        .api_version(vk::make_api_version(0, 1, 3, 0));
    let ci = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: `ci` outlives the call.
    let instance = unsafe { entry.create_instance(&ci, None) }.context("vkCreateInstance")?;
    let r = decode_on_a_device(&entry, &instance, &profile, &sps, &pps, bitstream, &frames, out);
    // SAFETY: nothing derived from the instance outlives this.
    unsafe { instance.destroy_instance(None) };
    let mut d = r?;
    d.coded = (coded_w, coded_h);
    d.display = display;
    Ok(d)
}

struct Chosen {
    pd: vk::PhysicalDevice,
    name: String,
    decode_family: u32,
    copy_family: u32,
}

fn choose_device(
    instance: &ash::Instance,
    video_fns: &ash::khr::video_queue::Instance,
    profile: &Probe,
) -> Result<Chosen> {
    let want = std::env::var("SV_VIDEO_DEVICE").ok();
    // SAFETY: the instance is live.
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    let mut refusals = Vec::new();
    for pd in devices {
        // SAFETY: `pd` came from this instance.
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        if let Some(w) = &want
            && !name.to_lowercase().contains(&w.to_lowercase())
        {
            continue;
        }
        if vk::api_version_minor(props.api_version) < 3
            || vk::api_version_major(props.api_version) < 1
        {
            refusals.push(format!("{name}: reports Vulkan below 1.3"));
            continue;
        }
        // SAFETY: `pd` is live.
        let exts = unsafe { instance.enumerate_device_extension_properties(pd) }?;
        let have = |n: &CStr| {
            exts.iter()
                .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == n)
        };
        if !(have(vk::KHR_VIDEO_QUEUE_NAME)
            && have(vk::KHR_VIDEO_DECODE_QUEUE_NAME)
            && have(vk::KHR_VIDEO_DECODE_H264_NAME))
        {
            refusals.push(format!("{name}: no H.264 decode extensions"));
            continue;
        }

        // SAFETY: count-then-fill with a chain that outlives both calls.
        let n = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
        let mut vids = vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
        let mut fams = vec![vk::QueueFamilyProperties2::default(); n];
        for (f, v) in fams.iter_mut().zip(vids.iter_mut()) {
            *f = f.push_next(v);
        }
        unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut fams) };
        let flags: Vec<vk::QueueFlags> = fams
            .iter()
            .map(|f| f.queue_family_properties.queue_flags)
            .collect();
        drop(fams);
        let ops: Vec<vk::VideoCodecOperationFlagsKHR> =
            vids.iter().map(|v| v.video_codec_operations).collect();
        let Some(decode_family) = flags
            .iter()
            .position(|f| f.contains(vk::QueueFlags::VIDEO_DECODE_KHR))
        else {
            refusals.push(format!("{name}: no queue family with VIDEO_DECODE"));
            continue;
        };
        if !ops[decode_family].contains(vk::VideoCodecOperationFlagsKHR::DECODE_H264) {
            refusals.push(format!("{name}: its decode family does not do H.264"));
            continue;
        }

        // Ask the driver whether it will take this exact profile before
        // committing to the device. An extension name is not an answer.
        //
        // The two `pNext` structures are not optional and their absence is not
        // reported: RADV writes the decode capabilities through the chain
        // without checking that there is one, so a bare `VkVideoCapabilitiesKHR`
        // segfaults inside the driver with nothing printed. Measured
        // 2026-08-13; it cost a run to find because the crash has no message.
        let mut decode_caps = vk::VideoDecodeCapabilitiesKHR::default();
        let mut h264_caps = vk::VideoDecodeH264CapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default();
        caps = caps.push_next(&mut decode_caps).push_next(&mut h264_caps);
        let res = with_profile(profile, |p| unsafe {
            (video_fns.fp().get_physical_device_video_capabilities_khr)(pd, p, &mut caps)
        });
        if res != vk::Result::SUCCESS {
            refusals.push(format!("{name}: this profile is {res:?}"));
            continue;
        }

        let transfer_capable = |q: &vk::QueueFlags| {
            q.intersects(
                vk::QueueFlags::TRANSFER | vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE,
            )
        };
        let copy_family = if transfer_capable(&flags[decode_family]) {
            decode_family
        } else {
            flags
                .iter()
                .position(transfer_capable)
                .ok_or_else(|| anyhow!("{name}: no queue family can copy the decoded frame out"))?
        };
        return Ok(Chosen {
            pd,
            name,
            decode_family: decode_family as u32,
            copy_family: copy_family as u32,
        });
    }
    bail!(
        "no device can decode this profile:\n  {}",
        refusals.join("\n  ")
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_on_a_device(
    entry: &ash::Entry,
    instance: &ash::Instance,
    profile: &Probe,
    sps: &h264::Sps,
    pps: &h264::Pps,
    bitstream: &[u8],
    frames: &[Vec<h264::Nal>],
    out: Option<&str>,
) -> Result<Decoded> {
    let video_fns = ash::khr::video_queue::Instance::new(entry, instance);
    let chosen = choose_device(instance, &video_fns, profile)?;
    println!(
        "device: {} — decode queue family {}, copy queue family {}",
        chosen.name, chosen.decode_family, chosen.copy_family
    );

    // The chain borrows `caps` mutably for as long as it lives, so read the
    // codec-specific answer out of it and drop the chain before going on.
    let (caps, decode_flags) = {
        let mut decode_caps = vk::VideoDecodeCapabilitiesKHR::default();
        let mut h264_caps = vk::VideoDecodeH264CapabilitiesKHR::default();
        let mut chained = vk::VideoCapabilitiesKHR::default();
        chained = chained.push_next(&mut decode_caps).push_next(&mut h264_caps);
        let res = with_profile(profile, |p| unsafe {
            (video_fns.fp().get_physical_device_video_capabilities_khr)(chosen.pd, p, &mut chained)
        });
        if res != vk::Result::SUCCESS {
            bail!("vkGetPhysicalDeviceVideoCapabilitiesKHR: {res:?}");
        }
        // Field by field rather than a copy, so that a member added to the
        // structure by a later header is a compile error here rather than an
        // uninitialised value carried silently into `vkCreateVideoSessionKHR`.
        let plain = vk::VideoCapabilitiesKHR {
            flags: chained.flags,
            min_bitstream_buffer_offset_alignment: chained.min_bitstream_buffer_offset_alignment,
            min_bitstream_buffer_size_alignment: chained.min_bitstream_buffer_size_alignment,
            picture_access_granularity: chained.picture_access_granularity,
            min_coded_extent: chained.min_coded_extent,
            max_coded_extent: chained.max_coded_extent,
            max_dpb_slots: chained.max_dpb_slots,
            max_active_reference_pictures: chained.max_active_reference_pictures,
            std_header_version: chained.std_header_version,
            ..Default::default()
        };
        (plain, decode_caps.flags)
    };
    let distinct = decode_flags.contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT);
    println!(
        "caps: dpb slots {} active refs {} decode flags {decode_flags:?} bitstream align {}/{}",
        caps.max_dpb_slots,
        caps.max_active_reference_pictures,
        caps.min_bitstream_buffer_offset_alignment,
        caps.min_bitstream_buffer_size_alignment,
    );

    let (coded_w, coded_h) = sps.coded_size();
    let extent = vk::Extent2D {
        width: coded_w.max(caps.min_coded_extent.width),
        height: coded_h.max(caps.min_coded_extent.height),
    };
    if extent.width > caps.max_coded_extent.width || extent.height > caps.max_coded_extent.height {
        bail!(
            "{}x{} is larger than this device's maximum coded extent {}x{}",
            extent.width,
            extent.height,
            caps.max_coded_extent.width,
            caps.max_coded_extent.height
        );
    }

    let dst_fmt = pick_format(
        &video_fns,
        chosen.pd,
        profile,
        vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::TRANSFER_SRC,
    )?;
    let dpb_fmt = pick_format(
        &video_fns,
        chosen.pd,
        profile,
        vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR,
    )?;
    println!("formats: dst {dst_fmt:?} dpb {dpb_fmt:?} (distinct dst {distinct})");

    // --- device -----------------------------------------------------------
    let prio = [1.0f32];
    let mut qcis = vec![
        vk::DeviceQueueCreateInfo::default()
            .queue_family_index(chosen.decode_family)
            .queue_priorities(&prio),
    ];
    if chosen.copy_family != chosen.decode_family {
        qcis.push(
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(chosen.copy_family)
                .queue_priorities(&prio),
        );
    }
    let ext_names = [
        vk::KHR_VIDEO_QUEUE_NAME.as_ptr(),
        vk::KHR_VIDEO_DECODE_QUEUE_NAME.as_ptr(),
        vk::KHR_VIDEO_DECODE_H264_NAME.as_ptr(),
    ];
    let mut vk13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
    let dci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&qcis)
        .enabled_extension_names(&ext_names)
        .push_next(&mut vk13);
    // SAFETY: every pointer in `dci` outlives the call.
    let device = unsafe { instance.create_device(chosen.pd, &dci, None) }
        .context("vkCreateDevice with the video decode extensions")?;
    let r = run_session(
        instance, &device, &video_fns, &chosen, profile, &caps, sps, pps, bitstream, frames,
        extent, dst_fmt, dpb_fmt, distinct, out,
    );
    // SAFETY: `run_session` destroys everything it created before returning.
    unsafe {
        let _ = device.device_wait_idle();
        device.destroy_device(None);
    }
    let mut d = r?;
    d.device_name = chosen.name;
    Ok(d)
}

fn pick_format(
    video_fns: &ash::khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    profile: &Probe,
    usage: vk::ImageUsageFlags,
) -> Result<vk::Format> {
    // The usage we ask for is the usage we intend, and a driver that cannot
    // give us TRANSFER_SRC on a decode target says so by returning nothing
    // rather than by failing later at image creation.
    let mut want = usage;
    for _ in 0..2 {
        let formats = with_profile(profile, |p| {
            let profiles = [*p];
            let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
            let info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
                .image_usage(want)
                .push_next(&mut list);
            let mut n = 0u32;
            // SAFETY: count-then-fill against the same `info`.
            unsafe {
                let r = (video_fns.fp().get_physical_device_video_format_properties_khr)(
                    pd,
                    &info,
                    &mut n,
                    std::ptr::null_mut(),
                );
                if r != vk::Result::SUCCESS || n == 0 {
                    return Vec::new();
                }
                let mut v = vec![vk::VideoFormatPropertiesKHR::default(); n as usize];
                let r = (video_fns.fp().get_physical_device_video_format_properties_khr)(
                    pd,
                    &info,
                    &mut n,
                    v.as_mut_ptr(),
                );
                if r != vk::Result::SUCCESS {
                    return Vec::new();
                }
                v.truncate(n as usize);
                v
            }
        });
        if let Some(f) = formats.first() {
            return Ok(f.format);
        }
        // Drop the transfer bit and try again, so that the failure message can
        // say which half of the request was refused.
        if want.contains(vk::ImageUsageFlags::TRANSFER_SRC) {
            want &= !vk::ImageUsageFlags::TRANSFER_SRC;
        } else {
            break;
        }
    }
    bail!("no video format for usage {usage:?}")
}

struct Owned {
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    memories: Vec<vk::DeviceMemory>,
    buffers: Vec<vk::Buffer>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_session(
    instance: &ash::Instance,
    device: &ash::Device,
    _video_fns: &ash::khr::video_queue::Instance,
    chosen: &Chosen,
    profile: &Probe,
    caps: &vk::VideoCapabilitiesKHR<'_>,
    sps: &h264::Sps,
    pps: &h264::Pps,
    bitstream: &[u8],
    frames: &[Vec<h264::Nal>],
    extent: vk::Extent2D,
    dst_fmt: vk::Format,
    dpb_fmt: vk::Format,
    distinct: bool,
    out: Option<&str>,
) -> Result<Decoded> {
    let vq = ash::khr::video_queue::Device::new(instance, device);
    let vdq = ash::khr::video_decode_queue::Device::new(instance, device);
    // SAFETY: the device is live and both families were requested at creation.
    let decode_queue = unsafe { device.get_device_queue(chosen.decode_family, 0) };
    let copy_queue = unsafe { device.get_device_queue(chosen.copy_family, 0) };
    // SAFETY: `chosen.pd` came from `instance`.
    let mem_props = unsafe { instance.get_physical_device_memory_properties(chosen.pd) };

    let mut owned = Owned {
        images: Vec::new(),
        views: Vec::new(),
        memories: Vec::new(),
        buffers: Vec::new(),
    };

    // --- session ----------------------------------------------------------
    let dpb_slots = 1u32.max(u32::from(sps.max_num_ref_frames) + 1).min(caps.max_dpb_slots);
    let active_refs = caps.max_active_reference_pictures.min(dpb_slots);
    let std_version = caps.std_header_version;
    let session = with_profile(profile, |p| {
        let ci = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(chosen.decode_family)
            .video_profile(p)
            .picture_format(dst_fmt)
            .max_coded_extent(extent)
            .reference_picture_format(dpb_fmt)
            .max_dpb_slots(dpb_slots)
            .max_active_reference_pictures(active_refs)
            .std_header_version(&std_version);
        let mut s = vk::VideoSessionKHR::null();
        // SAFETY: `ci` and the profile chain outlive the call.
        let r = unsafe {
            (vq.fp().create_video_session_khr)(device.handle(), &ci, std::ptr::null(), &mut s)
        };
        (r, s)
    });
    if session.0 != vk::Result::SUCCESS {
        bail!("vkCreateVideoSessionKHR: {:?}", session.0);
    }
    let session = session.1;

    let result = (|| -> Result<Decoded> {
        // Session memory: count-then-fill, then one allocation per bind index.
        let mut n = 0u32;
        // SAFETY: count query with a null array.
        unsafe {
            let _ = (vq.fp().get_video_session_memory_requirements_khr)(
                device.handle(),
                session,
                &mut n,
                std::ptr::null_mut(),
            );
        };
        let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); n as usize];
        // SAFETY: the array is `n` long.
        unsafe {
            let _ = (vq.fp().get_video_session_memory_requirements_khr)(
                device.handle(),
                session,
                &mut n,
                reqs.as_mut_ptr(),
            );
        };
        reqs.truncate(n as usize);
        let mut binds = Vec::with_capacity(reqs.len());
        for r in &reqs {
            let mem = alloc(
                device,
                &mem_props,
                r.memory_requirements,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                "session",
            )?;
            owned.memories.push(mem);
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(r.memory_bind_index)
                    .memory(mem)
                    .memory_offset(0)
                    .memory_size(r.memory_requirements.size),
            );
        }
        // SAFETY: `binds` outlives the call.
        let r = unsafe {
            (vq.fp().bind_video_session_memory_khr)(
                device.handle(),
                session,
                binds.len() as u32,
                binds.as_ptr(),
            )
        };
        if r != vk::Result::SUCCESS {
            bail!("vkBindVideoSessionMemoryKHR: {r:?}");
        }
        println!("session memory: {} bind(s), {} bytes total", reqs.len(),
            reqs.iter().map(|r| r.memory_requirements.size).sum::<u64>());

        // --- session parameters (the SPS and the PPS) ---------------------
        let std_sps = build_std_sps(sps);
        let std_pps = build_std_pps(pps);
        let add = vk::VideoDecodeH264SessionParametersAddInfoKHR::default()
            .std_sp_ss(std::slice::from_ref(&std_sps))
            .std_pp_ss(std::slice::from_ref(&std_pps));
        let mut h264_params = vk::VideoDecodeH264SessionParametersCreateInfoKHR::default()
            .max_std_sps_count(1)
            .max_std_pps_count(1)
            .parameters_add_info(&add);
        let pci = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(session)
            .push_next(&mut h264_params);
        let mut params = vk::VideoSessionParametersKHR::null();
        // SAFETY: the whole chain, including the Std structures, outlives the
        // call. The driver copies what it needs; nothing here is retained.
        let r = unsafe {
            (vq.fp().create_video_session_parameters_khr)(
                device.handle(),
                &pci,
                std::ptr::null(),
                &mut params,
            )
        };
        if r != vk::Result::SUCCESS {
            bail!("vkCreateVideoSessionParametersKHR: {r:?}");
        }

        let d = decode_frames(
            device, &vq, &vdq, chosen, profile, caps, sps, pps, bitstream, frames, extent, dst_fmt,
            dpb_fmt, distinct, dpb_slots, session, params, decode_queue, copy_queue, &mem_props,
            &mut owned, out,
        );
        // SAFETY: no command buffer referencing the parameters is in flight.
        unsafe {
            (vq.fp().destroy_video_session_parameters_khr)(
                device.handle(),
                params,
                std::ptr::null(),
            )
        };
        d
    })();

    // SAFETY: `decode_frames` waits for idle before returning, so nothing here
    // is still in use.
    unsafe {
        let _ = device.device_wait_idle();
        (vq.fp().destroy_video_session_khr)(device.handle(), session, std::ptr::null());
        for v in owned.views {
            device.destroy_image_view(v, None);
        }
        for i in owned.images {
            device.destroy_image(i, None);
        }
        for b in owned.buffers {
            device.destroy_buffer(b, None);
        }
        for m in owned.memories {
            device.free_memory(m, None);
        }
    }
    result
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_frames(
    device: &ash::Device,
    vq: &ash::khr::video_queue::Device,
    vdq: &ash::khr::video_decode_queue::Device,
    chosen: &Chosen,
    profile: &Probe,
    caps: &vk::VideoCapabilitiesKHR<'_>,
    sps: &h264::Sps,
    pps: &h264::Pps,
    bitstream: &[u8],
    frames: &[Vec<h264::Nal>],
    extent: vk::Extent2D,
    dst_fmt: vk::Format,
    dpb_fmt: vk::Format,
    distinct: bool,
    dpb_slots: u32,
    session: vk::VideoSessionKHR,
    params: vk::VideoSessionParametersKHR,
    decode_queue: vk::Queue,
    copy_queue: vk::Queue,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    owned: &mut Owned,
    out: Option<&str>,
) -> Result<Decoded> {
    let families = [chosen.decode_family, chosen.copy_family];
    let shared = chosen.decode_family != chosen.copy_family;

    // --- images -----------------------------------------------------------
    let dpb_usage = vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR
        | if distinct {
            vk::ImageUsageFlags::empty()
        } else {
            vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::TRANSFER_SRC
        };
    let (dpb_image, dpb_views) = make_image(
        device, mem_props, profile, dpb_fmt, extent, dpb_slots, dpb_usage,
        if distinct { &[] } else { &families }, shared && !distinct, owned,
    )?;
    let (dst_image, dst_views) = if distinct {
        make_image(
            device,
            mem_props,
            profile,
            dst_fmt,
            extent,
            1,
            vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::TRANSFER_SRC,
            &families,
            shared,
            owned,
        )?
    } else {
        (dpb_image, dpb_views.clone())
    };

    // --- bitstream and readback buffers -----------------------------------
    let max_frame: usize = frames
        .iter()
        .map(|f| f.iter().map(|n| n.end - n.start).sum::<usize>())
        .max()
        .unwrap_or(0);
    let align = caps.min_bitstream_buffer_size_alignment.max(1);
    let bs_size = ((max_frame as u64).div_ceil(align) * align).max(align);
    let (bs_buf, bs_mem, bs_ptr) = make_host_buffer(
        device,
        mem_props,
        profile,
        bs_size,
        vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR,
        owned,
    )?;

    let luma = (extent.width * extent.height) as u64;
    let read_size = luma + luma / 2;
    let (rb_buf, _rb_mem, rb_ptr) = make_host_buffer(
        device,
        mem_props,
        profile,
        read_size,
        vk::BufferUsageFlags::TRANSFER_DST,
        owned,
    )?;

    // --- command buffers and sync ----------------------------------------
    let decode_pool = make_pool(device, chosen.decode_family)?;
    let copy_pool = if shared {
        make_pool(device, chosen.copy_family)?
    } else {
        decode_pool
    };
    let decode_cb = alloc_cb(device, decode_pool)?;
    let copy_cb = alloc_cb(device, copy_pool)?;
    // SAFETY: the device is live; both objects are destroyed below.
    let sem = unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?;
    let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;

    let mut out_file = out.map(std::fs::File::create).transpose()?;
    let mut submit_ms = Vec::new();
    let mut bytes_in = 0usize;
    let mut bytes_out = 0usize;
    let mut dst_layout = vk::ImageLayout::UNDEFINED;
    let mut dpb_layout = vk::ImageLayout::UNDEFINED;
    let display = sps.display_size();

    for (n, slices) in frames.iter().enumerate() {
        // Copy the frame's slice NALs, start codes and all, into the bitstream
        // buffer and record where each one begins.
        let mut offsets = Vec::with_capacity(slices.len());
        let mut at = 0usize;
        for s in slices {
            offsets.push(at as u32);
            let bytes = &bitstream[s.start..s.end];
            // SAFETY: the buffer was sized from the largest frame.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), bs_ptr.add(at), bytes.len()) };
            at += bytes.len();
        }
        bytes_in += at;
        let range = (at as u64).div_ceil(align) * align;

        let hdr = h264::parse_slice_header(
            &h264::rbsp(&bitstream[slices[0].payload..slices[0].end]),
            true,
            sps,
            pps,
        )?;
        if hdr.slice_type % 5 != 2 && hdr.slice_type % 5 != 4 {
            bail!(
                "frame {n} is an IDR whose slice_type is {} — not an I or SI slice, \
                 so it references pictures this decoder does not keep",
                hdr.slice_type
            );
        }

        let mut pic_flags: vk::native::StdVideoDecodeH264PictureInfoFlags = zeroed_flags();
        pic_flags.set_field_pic_flag(u32::from(hdr.field_pic_flag));
        pic_flags.set_is_intra(1);
        pic_flags.set_IdrPicFlag(1);
        pic_flags.set_bottom_field_flag(0);
        pic_flags.set_is_reference(1);
        pic_flags.set_complementary_field_pair(0);
        // An IDR resets both counters, so `frame_num` and the picture order
        // count are zero by definition however the slice header spells them.
        // They are read back from the header rather than assumed, so that a
        // stream which disagrees fails here instead of decoding to the wrong
        // reference.
        if hdr.frame_num != 0 || hdr.pic_order_cnt_lsb != 0 {
            bail!(
                "frame {n} is an IDR with frame_num {} and pic_order_cnt_lsb {} — \
                 an IDR resets both to zero, so this stream is not what it says",
                hdr.frame_num,
                hdr.pic_order_cnt_lsb
            );
        }
        let std_pic = vk::native::StdVideoDecodeH264PictureInfo {
            flags: pic_flags,
            seq_parameter_set_id: sps.id,
            pic_parameter_set_id: hdr.pps_id,
            reserved1: 0,
            reserved2: 0,
            frame_num: hdr.frame_num,
            idr_pic_id: hdr.idr_pic_id,
            PicOrderCnt: [0, 0],
        };
        let mut ref_flags: vk::native::StdVideoDecodeH264ReferenceInfoFlags = zeroed_flags();
        ref_flags.set_top_field_flag(0);
        ref_flags.set_bottom_field_flag(0);
        ref_flags.set_used_for_long_term_reference(0);
        ref_flags.set_is_non_existing(0);
        let std_ref = vk::native::StdVideoDecodeH264ReferenceInfo {
            flags: ref_flags,
            FrameNum: 0,
            reserved: 0,
            PicOrderCnt: [0, 0],
        };

        // --- record the decode -------------------------------------------
        // SAFETY: the command buffer is not in flight — the previous
        // iteration waited on `fence` before reaching here.
        unsafe {
            device.reset_command_buffer(decode_cb, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                decode_cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
        }
        let mut barriers = Vec::new();
        if dpb_layout != vk::ImageLayout::VIDEO_DECODE_DPB_KHR {
            barriers.push(
                image_barrier(dpb_image, dpb_slots, dpb_layout, vk::ImageLayout::VIDEO_DECODE_DPB_KHR),
            );
            dpb_layout = vk::ImageLayout::VIDEO_DECODE_DPB_KHR;
        }
        if distinct && dst_layout != vk::ImageLayout::VIDEO_DECODE_DST_KHR {
            barriers.push(
                image_barrier(dst_image, 1, dst_layout, vk::ImageLayout::VIDEO_DECODE_DST_KHR),
            );
        }
        if !barriers.is_empty() {
            let dep = vk::DependencyInfo::default().image_memory_barriers(&barriers);
            // SAFETY: `dep` outlives the call.
            unsafe { device.cmd_pipeline_barrier2(decode_cb, &dep) };
        }
        if distinct {
            dst_layout = vk::ImageLayout::VIDEO_DECODE_DST_KHR;
        }

        let dpb_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D { x: 0, y: 0 })
            .coded_extent(extent)
            .base_array_layer(0)
            .image_view_binding(dpb_views[0]);
        let dst_res = if distinct {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_offset(vk::Offset2D { x: 0, y: 0 })
                .coded_extent(extent)
                .base_array_layer(0)
                .image_view_binding(dst_views[0])
        } else {
            dpb_res
        };

        let mut slot_h264 =
            vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(&std_ref);
        let begin_slots = [vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(-1)
            .picture_resource(&dpb_res)];
        let begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(session)
            .video_session_parameters(params)
            .reference_slots(&begin_slots);
        // SAFETY: `begin` and everything it points at outlive the call.
        unsafe { (vq.fp().cmd_begin_video_coding_khr)(decode_cb, &begin) };
        if n == 0 {
            let ctl = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::RESET);
            // SAFETY: inside a video coding scope.
            unsafe { (vq.fp().cmd_control_video_coding_khr)(decode_cb, &ctl) };
        }

        let setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(0)
            .picture_resource(&dpb_res)
            .push_next(&mut slot_h264);
        let mut h264_pic = vk::VideoDecodeH264PictureInfoKHR::default()
            .std_picture_info(&std_pic)
            .slice_offsets(&offsets);
        let info = vk::VideoDecodeInfoKHR::default()
            .src_buffer(bs_buf)
            .src_buffer_offset(0)
            .src_buffer_range(range)
            .dst_picture_resource(dst_res)
            .setup_reference_slot(&setup)
            .push_next(&mut h264_pic);
        // SAFETY: every structure in the chain outlives the call.
        unsafe { (vdq.fp().cmd_decode_video_khr)(decode_cb, &info) };
        let end = vk::VideoEndCodingInfoKHR::default();
        // SAFETY: matches the begin above.
        unsafe {
            (vq.fp().cmd_end_video_coding_khr)(decode_cb, &end);
            device.end_command_buffer(decode_cb)?;
        }

        // --- record the copy ---------------------------------------------
        // SAFETY: not in flight, same argument as above.
        unsafe {
            device.reset_command_buffer(copy_cb, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                copy_cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
        }
        let src_layout = if distinct {
            vk::ImageLayout::VIDEO_DECODE_DST_KHR
        } else {
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR
        };
        let to_src = [image_barrier(
            dst_image,
            1,
            src_layout,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        )];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_src);
        // SAFETY: `dep` outlives the call.
        unsafe { device.cmd_pipeline_barrier2(copy_cb, &dep) };
        let regions = [
            plane_copy(vk::ImageAspectFlags::PLANE_0, extent.width, extent.height, 0),
            plane_copy(
                vk::ImageAspectFlags::PLANE_1,
                extent.width / 2,
                extent.height / 2,
                luma,
            ),
        ];
        // SAFETY: the buffer is large enough for both planes.
        unsafe {
            device.cmd_copy_image_to_buffer(
                copy_cb,
                dst_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                rb_buf,
                &regions,
            );
        }
        let back = [image_barrier(
            dst_image,
            1,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            src_layout,
        )];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&back);
        // SAFETY: `dep` outlives the call.
        unsafe {
            device.cmd_pipeline_barrier2(copy_cb, &dep);
            device.end_command_buffer(copy_cb)?;
        }
        if distinct {
            dst_layout = src_layout;
        }

        // --- submit -------------------------------------------------------
        let t0 = std::time::Instant::now();
        let dcb = [decode_cb];
        let sems = [sem];
        let submit = [vk::SubmitInfo::default()
            .command_buffers(&dcb)
            .signal_semaphores(&sems)];
        // SAFETY: `submit` outlives the call.
        unsafe { device.queue_submit(decode_queue, &submit, vk::Fence::null())? };
        let ccb = [copy_cb];
        let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        let submit = [vk::SubmitInfo::default()
            .wait_semaphores(&sems)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&ccb)];
        // SAFETY: same.
        unsafe { device.queue_submit(copy_queue, &submit, fence)? };
        // SAFETY: the fence was created unsignalled and is reset below.
        unsafe {
            device.wait_for_fences(&[fence], true, 5_000_000_000)?;
            device.reset_fences(&[fence])?;
        }
        submit_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

        // --- read it back --------------------------------------------------
        // SAFETY: the copy has completed and the mapping is coherent.
        let bytes =
            unsafe { std::slice::from_raw_parts(rb_ptr as *const u8, read_size as usize) };
        if let Some(f) = &mut out_file {
            use std::io::Write;
            // Crop on the way out so the file can be compared with a software
            // decode of the same stream without a second cropping step.
            for y in 0..display.1 as usize {
                f.write_all(&bytes[y * extent.width as usize..][..display.0 as usize])?;
            }
            let uv = luma as usize;
            for y in 0..(display.1 / 2) as usize {
                f.write_all(&bytes[uv + y * extent.width as usize..][..display.0 as usize])?;
            }
            bytes_out += (display.0 * display.1 + display.0 * display.1 / 2) as usize;
        } else {
            bytes_out += read_size as usize;
        }
        if n == 0 {
            let mean: f64 =
                bytes[..luma as usize].iter().map(|&b| f64::from(b)).sum::<f64>() / luma as f64;
            println!("frame 0: mean luma {mean:.2}, first bytes {:?}", &bytes[..8]);
        }
    }

    // SAFETY: the last fence wait means nothing is in flight.
    unsafe {
        device.destroy_semaphore(sem, None);
        device.destroy_fence(fence, None);
        device.destroy_command_pool(decode_pool, None);
        if shared {
            device.destroy_command_pool(copy_pool, None);
        }
        device.unmap_memory(bs_mem);
    }

    Ok(Decoded {
        device_name: String::new(),
        frames: frames.len(),
        coded: (extent.width, extent.height),
        display,
        format: dst_fmt,
        bytes_in,
        bytes_out,
        submit_ms,
    })
}

#[cfg(test)]
fn dummy_pps() -> h264::Pps {
    h264::Pps {
        id: 0,
        sps_id: 0,
        entropy_coding_mode_flag: false,
        bottom_field_pic_order_in_frame_present_flag: false,
        num_ref_idx_l0_default_active_minus1: 0,
        num_ref_idx_l1_default_active_minus1: 0,
        weighted_pred_flag: false,
        weighted_bipred_idc: 0,
        pic_init_qp_minus26: 0,
        pic_init_qs_minus26: 0,
        chroma_qp_index_offset: 0,
        second_chroma_qp_index_offset: 0,
        deblocking_filter_control_present_flag: false,
        constrained_intra_pred_flag: false,
        redundant_pic_cnt_present_flag: false,
        transform_8x8_mode_flag: false,
    }
}


pub fn build_std_sps(s: &h264::Sps) -> vk::native::StdVideoH264SequenceParameterSet {
    let mut f: vk::native::StdVideoH264SpsFlags = zeroed_flags();
    f.set_constraint_set0_flag(u32::from(s.constraint_set[0]));
    f.set_constraint_set1_flag(u32::from(s.constraint_set[1]));
    f.set_constraint_set2_flag(u32::from(s.constraint_set[2]));
    f.set_constraint_set3_flag(u32::from(s.constraint_set[3]));
    f.set_constraint_set4_flag(u32::from(s.constraint_set[4]));
    f.set_constraint_set5_flag(u32::from(s.constraint_set[5]));
    f.set_direct_8x8_inference_flag(u32::from(s.direct_8x8_inference_flag));
    f.set_mb_adaptive_frame_field_flag(u32::from(s.mb_adaptive_frame_field_flag));
    f.set_frame_mbs_only_flag(u32::from(s.frame_mbs_only_flag));
    f.set_delta_pic_order_always_zero_flag(u32::from(s.delta_pic_order_always_zero_flag));
    f.set_separate_colour_plane_flag(u32::from(s.separate_colour_plane_flag));
    f.set_gaps_in_frame_num_value_allowed_flag(u32::from(s.gaps_in_frame_num_value_allowed_flag));
    f.set_qpprime_y_zero_transform_bypass_flag(u32::from(s.qpprime_y_zero_transform_bypass_flag));
    f.set_frame_cropping_flag(u32::from(s.frame_cropping_flag));
    f.set_seq_scaling_matrix_present_flag(0);
    // The VUI is not carried, so the flag must say so: a driver that trusts the
    // flag and follows a null pointer is a crash inside the driver.
    f.set_vui_parameters_present_flag(0);
    vk::native::StdVideoH264SequenceParameterSet {
        flags: f,
        profile_idc: u32::from(s.profile_idc),
        level_idc: level_idc_enum(s.level_idc),
        chroma_format_idc: s.chroma_format_idc,
        seq_parameter_set_id: s.id,
        bit_depth_luma_minus8: s.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: s.bit_depth_chroma_minus8,
        log2_max_frame_num_minus4: s.log2_max_frame_num_minus4,
        pic_order_cnt_type: u32::from(s.pic_order_cnt_type),
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        log2_max_pic_order_cnt_lsb_minus4: s.log2_max_pic_order_cnt_lsb_minus4,
        num_ref_frames_in_pic_order_cnt_cycle: 0,
        max_num_ref_frames: s.max_num_ref_frames,
        reserved1: 0,
        pic_width_in_mbs_minus1: s.pic_width_in_mbs_minus1,
        pic_height_in_map_units_minus1: s.pic_height_in_map_units_minus1,
        frame_crop_left_offset: s.crop[0],
        frame_crop_right_offset: s.crop[1],
        frame_crop_top_offset: s.crop[2],
        frame_crop_bottom_offset: s.crop[3],
        reserved2: 0,
        pOffsetForRefFrame: std::ptr::null(),
        pScalingLists: std::ptr::null(),
        pSequenceParameterSetVui: std::ptr::null(),
    }
}

/// `StdVideoH264LevelIdc` is an enum of levels, not the `level_idc` byte. 3.1
/// is `level_idc == 31` in the bitstream and enum value 7 in the header, and
/// mapping the byte straight through gives a driver a level it has never heard
/// of — which RADV accepts and NVIDIA rejects.
pub fn level_idc_enum(level_idc: u8) -> vk::native::StdVideoH264LevelIdc {
    match level_idc {
        10 => 0,
        11 => 1,
        12 => 2,
        13 => 3,
        20 => 4,
        21 => 5,
        22 => 6,
        30 => 7,
        31 => 8,
        32 => 9,
        40 => 10,
        41 => 11,
        42 => 12,
        50 => 13,
        51 => 14,
        52 => 15,
        60 => 16,
        61 => 17,
        62 => 18,
        // Anything unrecognised is clamped to the top rather than passed
        // through, because a value outside the enum is undefined behaviour in
        // the driver and "6.2" is the most permissive thing we can claim.
        _ => 18,
    }
}

pub fn build_std_pps(p: &h264::Pps) -> vk::native::StdVideoH264PictureParameterSet {
    let mut f: vk::native::StdVideoH264PpsFlags = zeroed_flags();
    f.set_transform_8x8_mode_flag(u32::from(p.transform_8x8_mode_flag));
    f.set_redundant_pic_cnt_present_flag(u32::from(p.redundant_pic_cnt_present_flag));
    f.set_constrained_intra_pred_flag(u32::from(p.constrained_intra_pred_flag));
    f.set_deblocking_filter_control_present_flag(u32::from(
        p.deblocking_filter_control_present_flag,
    ));
    f.set_weighted_pred_flag(u32::from(p.weighted_pred_flag));
    f.set_bottom_field_pic_order_in_frame_present_flag(u32::from(
        p.bottom_field_pic_order_in_frame_present_flag,
    ));
    f.set_entropy_coding_mode_flag(u32::from(p.entropy_coding_mode_flag));
    f.set_pic_scaling_matrix_present_flag(0);
    vk::native::StdVideoH264PictureParameterSet {
        flags: f,
        seq_parameter_set_id: p.sps_id,
        pic_parameter_set_id: p.id,
        num_ref_idx_l0_default_active_minus1: p.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: p.num_ref_idx_l1_default_active_minus1,
        weighted_bipred_idc: u32::from(p.weighted_bipred_idc),
        pic_init_qp_minus26: p.pic_init_qp_minus26,
        pic_init_qs_minus26: p.pic_init_qs_minus26,
        chroma_qp_index_offset: p.chroma_qp_index_offset,
        second_chroma_qp_index_offset: p.second_chroma_qp_index_offset,
        pScalingLists: std::ptr::null(),
    }
}

// ---------------------------------------------------------------------------
// Small Vulkan helpers. Nothing clever; they exist so the decode path above
// reads as the sequence of Vulkan Video calls it is.
// ---------------------------------------------------------------------------

fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(want)
    })
}

fn alloc(
    device: &ash::Device,
    props: &vk::PhysicalDeviceMemoryProperties,
    req: vk::MemoryRequirements,
    want: vk::MemoryPropertyFlags,
    what: &str,
) -> Result<vk::DeviceMemory> {
    let ty = find_memory_type(props, req.memory_type_bits, want)
        .or_else(|| find_memory_type(props, req.memory_type_bits, vk::MemoryPropertyFlags::empty()))
        .ok_or_else(|| anyhow!("no memory type for bits {:#x}", req.memory_type_bits))?;
    // Printed because *where* each allocation lands is a result here, not a
    // detail: through the forwarder the bitstream is meant to be in the arena
    // (host-visible) and the session, DPB and decode target never are
    // (docs/design.md §0.21.2). A type without the wanted flags is the
    // fallback above firing, and the line says so.
    let flags = props.memory_types[ty as usize].property_flags;
    println!(
        "memory: {what} {} B, bits {:#x} -> type {ty} {flags:?}{}",
        req.size,
        req.memory_type_bits,
        if flags.contains(want) { "" } else { " (NOT the wanted flags)" }
    );
    let ai = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(ty);
    // SAFETY: `ai` outlives the call.
    Ok(unsafe { device.allocate_memory(&ai, None) }?)
}

#[allow(clippy::too_many_arguments)]
fn make_image(
    device: &ash::Device,
    props: &vk::PhysicalDeviceMemoryProperties,
    profile: &Probe,
    format: vk::Format,
    extent: vk::Extent2D,
    layers: u32,
    usage: vk::ImageUsageFlags,
    families: &[u32],
    concurrent: bool,
    owned: &mut Owned,
) -> Result<(vk::Image, Vec<vk::ImageView>)> {
    let image = with_profile(profile, |p| {
        let profiles = [*p];
        let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
        let mut ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(layers)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut list);
        if concurrent {
            ci = ci
                .sharing_mode(vk::SharingMode::CONCURRENT)
                .queue_family_indices(families);
        }
        // SAFETY: `ci` and its chain outlive the call.
        unsafe { device.create_image(&ci, None) }
    })?;
    owned.images.push(image);
    // SAFETY: the image was just created.
    let req = unsafe { device.get_image_memory_requirements(image) };
    let what = if usage.contains(vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR) { "dpb image" } else { "decode target image" };
    let mem = alloc(device, props, req, vk::MemoryPropertyFlags::DEVICE_LOCAL, what)?;
    owned.memories.push(mem);
    // SAFETY: the memory is large enough and unbound.
    unsafe { device.bind_image_memory(image, mem, 0) }?;

    let mut views = Vec::with_capacity(layers as usize);
    for layer in 0..layers {
        let vci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: layer,
                layer_count: 1,
            });
        // SAFETY: `vci` outlives the call.
        let v = unsafe { device.create_image_view(&vci, None) }?;
        owned.views.push(v);
        views.push(v);
    }
    Ok((image, views))
}

fn make_host_buffer(
    device: &ash::Device,
    props: &vk::PhysicalDeviceMemoryProperties,
    profile: &Probe,
    size: u64,
    usage: vk::BufferUsageFlags,
    owned: &mut Owned,
) -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8)> {
    let buffer = with_profile(profile, |p| {
        let profiles = [*p];
        let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
        let mut ci = vk::BufferCreateInfo::default().size(size).usage(usage);
        // A buffer that is not a video resource must not carry a profile list;
        // one that is must. The readback buffer is the first kind.
        if usage.contains(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR) {
            ci = ci.push_next(&mut list);
        }
        // SAFETY: `ci` outlives the call.
        unsafe { device.create_buffer(&ci, None) }
    })?;
    owned.buffers.push(buffer);
    // SAFETY: the buffer was just created.
    let req = unsafe { device.get_buffer_memory_requirements(buffer) };
    let mem = alloc(
        device,
        props,
        req,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        if usage.contains(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR) { "bitstream buffer" } else { "readback buffer" },
    )?;
    owned.memories.push(mem);
    // SAFETY: the allocation is at least `req.size` and nothing else is bound.
    unsafe { device.bind_buffer_memory(buffer, mem, 0) }?;
    // SAFETY: the memory is host visible and stays mapped for the run.
    let ptr = unsafe { device.map_memory(mem, 0, req.size, vk::MemoryMapFlags::empty()) }?;
    Ok((buffer, mem, ptr.cast::<u8>()))
}

fn make_pool(device: &ash::Device, family: u32) -> Result<vk::CommandPool> {
    let ci = vk::CommandPoolCreateInfo::default()
        .queue_family_index(family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    // SAFETY: `ci` outlives the call.
    Ok(unsafe { device.create_command_pool(&ci, None) }?)
}

fn alloc_cb(device: &ash::Device, pool: vk::CommandPool) -> Result<vk::CommandBuffer> {
    let ai = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    // SAFETY: `ai` outlives the call.
    Ok(unsafe { device.allocate_command_buffers(&ai) }?[0])
}

fn image_barrier(
    image: vk::Image,
    layers: u32,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::MEMORY_WRITE | vk::AccessFlags2::MEMORY_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .dst_access_mask(vk::AccessFlags2::MEMORY_WRITE | vk::AccessFlags2::MEMORY_READ)
        .old_layout(from)
        .new_layout(to)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: layers,
        })
}

fn plane_copy(
    aspect: vk::ImageAspectFlags,
    width: u32,
    height: u32,
    offset: u64,
) -> vk::BufferImageCopy {
    vk::BufferImageCopy {
        buffer_offset: offset,
        buffer_row_length: 0,
        buffer_image_height: 0,
        image_subresource: vk::ImageSubresourceLayers {
            aspect_mask: aspect,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        },
        image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
        image_extent: vk::Extent3D {
            width,
            height,
            depth: 1,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The level enum is the one field where the obvious conversion — pass the
    /// byte through — is wrong, and wrong silently on the driver that tolerates
    /// it. Pin the whole table.
    #[test]
    fn level_idc_bytes_map_onto_the_std_enum() {
        assert_eq!(level_idc_enum(10), 0);
        assert_eq!(level_idc_enum(30), 7);
        assert_eq!(level_idc_enum(31), 8);
        assert_eq!(level_idc_enum(40), 10);
        assert_eq!(level_idc_enum(51), 14);
        assert_eq!(level_idc_enum(62), 18);
        // Not a level anyone defines: clamped, not passed through.
        assert_eq!(level_idc_enum(99), 18);
        assert_eq!(level_idc_enum(0), 18);
    }

    #[test]
    fn the_std_sps_never_claims_a_vui_it_does_not_carry() {
        let sps = h264::Sps {
            id: 0,
            profile_idc: 100,
            level_idc: 31,
            chroma_format_idc: 1,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 2,
            max_num_ref_frames: 1,
            pic_width_in_mbs_minus1: 19,
            pic_height_in_map_units_minus1: 11,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: false,
            direct_8x8_inference_flag: true,
            gaps_in_frame_num_value_allowed_flag: false,
            qpprime_y_zero_transform_bypass_flag: false,
            separate_colour_plane_flag: false,
            delta_pic_order_always_zero_flag: false,
            frame_cropping_flag: true,
            crop: [0, 0, 0, 6],
            constraint_set: [false, true, false, false, false, false],
        };
        let std = build_std_sps(&sps);
        assert_eq!(std.flags.vui_parameters_present_flag(), 0);
        assert!(std.pSequenceParameterSetVui.is_null());
        assert!(std.pScalingLists.is_null());
        assert!(std.pOffsetForRefFrame.is_null());
        assert_eq!(std.flags.seq_scaling_matrix_present_flag(), 0);
        assert_eq!(std.flags.constraint_set1_flag(), 1);
        assert_eq!(std.flags.constraint_set0_flag(), 0);
        assert_eq!(std.level_idc, 8);
        assert_eq!(std.frame_crop_bottom_offset, 6);
        assert_eq!(std.max_num_ref_frames, 1);
    }

    #[test]
    fn the_std_pps_carries_the_flags_the_parser_read() {
        let mut pps = dummy_pps();
        pps.entropy_coding_mode_flag = true;
        pps.transform_8x8_mode_flag = true;
        pps.id = 3;
        pps.sps_id = 1;
        pps.pic_init_qp_minus26 = -4;
        let std = build_std_pps(&pps);
        assert_eq!(std.flags.entropy_coding_mode_flag(), 1);
        assert_eq!(std.flags.transform_8x8_mode_flag(), 1);
        assert_eq!(std.flags.weighted_pred_flag(), 0);
        assert_eq!(std.pic_parameter_set_id, 3);
        assert_eq!(std.seq_parameter_set_id, 1);
        assert_eq!(std.pic_init_qp_minus26, -4);
        assert!(std.pScalingLists.is_null());
    }

    /// The two plane copies have to tile exactly onto the NV12 layout, or the
    /// output file is a picture with a displaced chroma plane — which looks
    /// like a decode fault and is not one.
    #[test]
    fn the_two_plane_copies_tile_onto_nv12() {
        let (w, h) = (320u32, 192u32);
        let luma = u64::from(w * h);
        let y = plane_copy(vk::ImageAspectFlags::PLANE_0, w, h, 0);
        let uv = plane_copy(vk::ImageAspectFlags::PLANE_1, w / 2, h / 2, luma);
        assert_eq!(y.buffer_offset, 0);
        assert_eq!(uv.buffer_offset, luma);
        // The chroma plane of a 4:2:0 two-plane image has half the rows and
        // half the columns, but two bytes per sample, so it is half the size.
        let uv_bytes = u64::from(uv.image_extent.width * uv.image_extent.height) * 2;
        assert_eq!(luma + uv_bytes, luma + luma / 2);
    }

    #[test]
    fn a_barrier_covers_every_layer_it_is_given() {
        let b = image_barrier(
            vk::Image::null(),
            4,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
        );
        assert_eq!(b.subresource_range.layer_count, 4);
        assert_eq!(b.old_layout, vk::ImageLayout::UNDEFINED);
        assert_eq!(b.new_layout, vk::ImageLayout::VIDEO_DECODE_DPB_KHR);
    }
}
