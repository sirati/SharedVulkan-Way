//! The survey. For every physical device the loader offers, report what
//! `VK_KHR_video_queue` and the decode extensions say — per device, per codec,
//! with the profile capabilities rather than the extension name.
//!
//! Why the capabilities and not the name: an extension name says the driver has
//! an entry point. `vkGetPhysicalDeviceVideoCapabilitiesKHR` is the call that
//! says whether *this* profile — this codec, this chroma subsampling, this bit
//! depth — is decodable, and it is the one that returns
//! `VK_ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR` for a codec whose
//! extension is present. Every number below comes from that call or from
//! `vkGetPhysicalDeviceVideoFormatPropertiesKHR`.

use anyhow::{Context, Result};
use ash::vk;
use std::ffi::CStr;

/// A decode profile worth asking about, named the way a person would name it.
pub struct Probe {
    pub label: &'static str,
    pub op: vk::VideoCodecOperationFlagsKHR,
    pub chroma: vk::VideoChromaSubsamplingFlagsKHR,
    pub luma_depth: vk::VideoComponentBitDepthFlagsKHR,
    pub chroma_depth: vk::VideoComponentBitDepthFlagsKHR,
    /// The codec-specific `pNext` on the profile.
    pub codec: CodecProfile,
}

#[derive(Clone, Copy)]
pub enum CodecProfile {
    H264 {
        idc: vk::native::StdVideoH264ProfileIdc,
        layout: vk::VideoDecodeH264PictureLayoutFlagsKHR,
    },
    H265 {
        idc: vk::native::StdVideoH265ProfileIdc,
    },
    Av1 {
        profile: vk::native::StdVideoAV1Profile,
        film_grain: bool,
    },
}

const D8: vk::VideoComponentBitDepthFlagsKHR = vk::VideoComponentBitDepthFlagsKHR::TYPE_8;
const D10: vk::VideoComponentBitDepthFlagsKHR = vk::VideoComponentBitDepthFlagsKHR::TYPE_10;
const C420: vk::VideoChromaSubsamplingFlagsKHR = vk::VideoChromaSubsamplingFlagsKHR::TYPE_420;

/// The list is short on purpose. These are the profiles a browser meets: H.264
/// is what every stream on the open web falls back to, H.265 is what a phone
/// camera writes, AV1 is what YouTube serves when it can. 4:2:0 8-bit because
/// that is what all three of those are, plus Main 10 for HDR.
pub fn probes() -> Vec<Probe> {
    use vk::native::*;
    let progressive = vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE;
    vec![
        Probe {
            label: "h264 baseline 4:2:0 8-bit progressive",
            op: vk::VideoCodecOperationFlagsKHR::DECODE_H264,
            chroma: C420,
            luma_depth: D8,
            chroma_depth: D8,
            codec: CodecProfile::H264 {
                idc: StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_BASELINE,
                layout: progressive,
            },
        },
        Probe {
            label: "h264 main     4:2:0 8-bit progressive",
            op: vk::VideoCodecOperationFlagsKHR::DECODE_H264,
            chroma: C420,
            luma_depth: D8,
            chroma_depth: D8,
            codec: CodecProfile::H264 {
                idc: StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN,
                layout: progressive,
            },
        },
        Probe {
            label: "h264 high     4:2:0 8-bit progressive",
            op: vk::VideoCodecOperationFlagsKHR::DECODE_H264,
            chroma: C420,
            luma_depth: D8,
            chroma_depth: D8,
            codec: CodecProfile::H264 {
                idc: StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
                layout: progressive,
            },
        },
        Probe {
            label: "h264 high     4:2:0 8-bit interlaced (interleaved)",
            op: vk::VideoCodecOperationFlagsKHR::DECODE_H264,
            chroma: C420,
            luma_depth: D8,
            chroma_depth: D8,
            codec: CodecProfile::H264 {
                idc: StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
                layout: vk::VideoDecodeH264PictureLayoutFlagsKHR::INTERLACED_INTERLEAVED_LINES,
            },
        },
        Probe {
            label: "h265 main     4:2:0 8-bit",
            op: vk::VideoCodecOperationFlagsKHR::DECODE_H265,
            chroma: C420,
            luma_depth: D8,
            chroma_depth: D8,
            codec: CodecProfile::H265 {
                idc: StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN,
            },
        },
        Probe {
            label: "h265 main10   4:2:0 10-bit",
            op: vk::VideoCodecOperationFlagsKHR::DECODE_H265,
            chroma: C420,
            luma_depth: D10,
            chroma_depth: D10,
            codec: CodecProfile::H265 {
                idc: StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10,
            },
        },
        Probe {
            label: "av1  main     4:2:0 8-bit  (film grain)",
            op: vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
            chroma: C420,
            luma_depth: D8,
            chroma_depth: D8,
            codec: CodecProfile::Av1 {
                profile: StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
                film_grain: true,
            },
        },
        Probe {
            label: "av1  main     4:2:0 8-bit  (no film grain)",
            op: vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
            chroma: C420,
            luma_depth: D8,
            chroma_depth: D8,
            codec: CodecProfile::Av1 {
                profile: StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
                film_grain: false,
            },
        },
        Probe {
            label: "av1  main     4:2:0 10-bit (no film grain)",
            op: vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
            chroma: C420,
            luma_depth: D10,
            chroma_depth: D10,
            codec: CodecProfile::Av1 {
                profile: StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
                film_grain: false,
            },
        },
    ]
}

/// The extension whose presence a profile of this codec needs. Asking a driver
/// for an H.265 profile when it does not have `VK_KHR_video_decode_h265` is
/// undefined rather than merely unsupported, so the survey checks first.
fn codec_extension(op: vk::VideoCodecOperationFlagsKHR) -> &'static CStr {
    match op {
        vk::VideoCodecOperationFlagsKHR::DECODE_H264 => vk::KHR_VIDEO_DECODE_H264_NAME,
        vk::VideoCodecOperationFlagsKHR::DECODE_H265 => vk::KHR_VIDEO_DECODE_H265_NAME,
        vk::VideoCodecOperationFlagsKHR::DECODE_AV1 => vk::KHR_VIDEO_DECODE_AV1_NAME,
        _ => vk::KHR_VIDEO_QUEUE_NAME,
    }
}

/// Build the chained profile info for one probe and hand it to `f`.
///
/// The chain has to outlive the call and the codec structs are three different
/// types, so this is a callback rather than a returned value. It is also the
/// one place in this crate that has to be careful: `VkVideoProfileInfoKHR` is a
/// `pNext` chain of *input* structures, and a dangling link in it is a driver
/// read of freed memory rather than a Rust error.
pub fn with_profile<R>(p: &Probe, f: impl FnOnce(&vk::VideoProfileInfoKHR<'_>) -> R) -> R {
    let base = vk::VideoProfileInfoKHR::default()
        .video_codec_operation(p.op)
        .chroma_subsampling(p.chroma)
        .luma_bit_depth(p.luma_depth)
        .chroma_bit_depth(p.chroma_depth);
    match p.codec {
        CodecProfile::H264 { idc, layout } => {
            let mut c = vk::VideoDecodeH264ProfileInfoKHR::default()
                .std_profile_idc(idc)
                .picture_layout(layout);
            f(&base.push_next(&mut c))
        }
        CodecProfile::H265 { idc } => {
            let mut c = vk::VideoDecodeH265ProfileInfoKHR::default().std_profile_idc(idc);
            f(&base.push_next(&mut c))
        }
        CodecProfile::Av1 {
            profile,
            film_grain,
        } => {
            let mut c = vk::VideoDecodeAV1ProfileInfoKHR::default()
                .std_profile(profile)
                .film_grain_support(film_grain);
            f(&base.push_next(&mut c))
        }
    }
}

fn queue_flag_names(f: vk::QueueFlags) -> String {
    let mut v = Vec::new();
    for (bit, name) in [
        (vk::QueueFlags::GRAPHICS, "GRAPHICS"),
        (vk::QueueFlags::COMPUTE, "COMPUTE"),
        (vk::QueueFlags::TRANSFER, "TRANSFER"),
        (vk::QueueFlags::SPARSE_BINDING, "SPARSE"),
        (vk::QueueFlags::PROTECTED, "PROTECTED"),
        (vk::QueueFlags::VIDEO_DECODE_KHR, "VIDEO_DECODE"),
        (vk::QueueFlags::VIDEO_ENCODE_KHR, "VIDEO_ENCODE"),
        (vk::QueueFlags::OPTICAL_FLOW_NV, "OPTICAL_FLOW"),
    ] {
        if f.contains(bit) {
            v.push(name);
        }
    }
    if v.is_empty() {
        "-".into()
    } else {
        v.join("|")
    }
}

/// Every codec operation bit `vk.xml` 1.4.350 defines, by value.
///
/// By value rather than by ash constant on purpose: ash 0.38.0+1.3.281 predates
/// `VK_KHR_video_decode_vp9` and `VK_KHR_video_encode_av1`, and both RADV and
/// the NVIDIA driver report bits for them. A survey that named only the bits
/// its bindings know would print a blank where a codec is, which is the worst
/// kind of negative result — indistinguishable from a driver that has none.
const CODEC_OPS: &[(u32, &str)] = &[
    (0x0000_0001, "DECODE_H264"),
    (0x0000_0002, "DECODE_H265"),
    (0x0000_0004, "DECODE_AV1"),
    (0x0000_0008, "DECODE_VP9"),
    (0x0001_0000, "ENCODE_H264"),
    (0x0002_0000, "ENCODE_H265"),
    (0x0004_0000, "ENCODE_AV1"),
];

fn codec_op_names(f: vk::VideoCodecOperationFlagsKHR) -> String {
    let raw = f.as_raw();
    let mut v = Vec::new();
    let mut known = 0u32;
    for &(bit, name) in CODEC_OPS {
        known |= bit;
        if raw & bit != 0 {
            v.push(name.to_string());
        }
    }
    let rest = raw & !known;
    if rest != 0 {
        v.push(format!("unnamed(0x{rest:x})"));
    }
    if v.is_empty() {
        "-".into()
    } else {
        v.join("|")
    }
}

/// `VkVideoDecodeH264CapabilitiesKHR::maxLevelIdc` is typed
/// `StdVideoH264LevelIdc`, which is an *enum* — 0 is level 1.0, 18 is level 6.2
/// — and is not the `level_idc` byte in the bitstream, where level 5.2 is the
/// number 52. Both spellings are printed, because drivers disagree about which
/// one they mean and a bare number hides the disagreement.
fn h264_level(v: u32) -> String {
    const LEVELS: &[&str] = &[
        "1.0", "1.1", "1.2", "1.3", "2.0", "2.1", "2.2", "3.0", "3.1", "3.2", "4.0", "4.1", "4.2",
        "5.0", "5.1", "5.2", "6.0", "6.1", "6.2",
    ];
    match LEVELS.get(v as usize) {
        Some(l) => format!("StdVideo enum: level {l}"),
        None if v.is_multiple_of(10) || v < 100 => format!(
            "outside the StdVideo enum; as a bitstream level_idc it is {}.{}",
            v / 10,
            v % 10
        ),
        None => "outside the StdVideo enum and not a level_idc either".into(),
    }
}

/// Same shape for H.265, where the StdVideo enum runs 0 (level 1.0) to 13
/// (level 6.2) and the bitstream's `general_level_idc` is the level times 30.
fn h265_level(v: u32) -> String {
    const LEVELS: &[&str] = &[
        "1.0", "2.0", "2.1", "3.0", "3.1", "4.0", "4.1", "5.0", "5.1", "5.2", "6.0", "6.1", "6.2",
    ];
    match LEVELS.get(v as usize) {
        Some(l) => format!("StdVideo enum: level {l}"),
        None if v.is_multiple_of(3) => format!(
            "outside the StdVideo enum; as a general_level_idc it is level {}.{}",
            v / 30,
            (v % 30) / 3
        ),
        None => "outside the StdVideo enum and not a general_level_idc either".into(),
    }
}

pub fn run() -> Result<()> {
    // SAFETY: the loader is loaded once, from this thread, and lives for the
    // whole process.
    let entry = unsafe { ash::Entry::load() }.context("loading the Vulkan loader")?;
    let instance_version = unsafe { entry.try_enumerate_instance_version() }?
        .unwrap_or(vk::make_api_version(0, 1, 0, 0));
    println!(
        "loader instance version {}.{}.{}",
        vk::api_version_major(instance_version),
        vk::api_version_minor(instance_version),
        vk::api_version_patch(instance_version)
    );

    let app = vk::ApplicationInfo::default()
        .application_name(c"video-probe")
        .api_version(vk::make_api_version(0, 1, 3, 0));
    let ci = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: `ci` is fully initialised and outlives the call.
    let instance = unsafe { entry.create_instance(&ci, None) }.context("vkCreateInstance")?;
    let video_fns = ash::khr::video_queue::Instance::new(&entry, &instance);

    // SAFETY: the instance is live.
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    println!("physical devices: {}", devices.len());
    for (i, &pd) in devices.iter().enumerate() {
        println!();
        report_device(&instance, &video_fns, i, pd)?;
    }

    // SAFETY: nothing derived from the instance is used after this point.
    unsafe { instance.destroy_instance(None) };
    Ok(())
}

fn report_device(
    instance: &ash::Instance,
    video_fns: &ash::khr::video_queue::Instance,
    index: usize,
    pd: vk::PhysicalDevice,
) -> Result<()> {
    let mut driver = vk::PhysicalDeviceDriverProperties::default();
    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
    // SAFETY: `props` outlives the call and its chain is well formed.
    unsafe { instance.get_physical_device_properties2(pd, &mut props) };
    let p = props.properties;
    let name = unsafe { CStr::from_ptr(p.device_name.as_ptr()) }.to_string_lossy();
    let driver_info = unsafe { CStr::from_ptr(driver.driver_info.as_ptr()) }.to_string_lossy();
    println!(
        "device {index}: {name}\n  vendor 0x{:04x} device 0x{:04x} type {:?} api {}.{}.{} driverID {:?} driverInfo {driver_info:?}",
        p.vendor_id,
        p.device_id,
        p.device_type,
        vk::api_version_major(p.api_version),
        vk::api_version_minor(p.api_version),
        vk::api_version_patch(p.api_version),
        driver.driver_id,
    );

    // SAFETY: the physical device is live.
    let exts = unsafe { instance.enumerate_device_extension_properties(pd) }?;
    let have = |n: &CStr| {
        exts.iter()
            .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == n)
    };
    let mut video_exts: Vec<String> = exts
        .iter()
        .filter_map(|e| {
            let s = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) }.to_string_lossy();
            s.contains("video").then(|| format!("{s}({})", e.spec_version))
        })
        .collect();
    video_exts.sort();
    if video_exts.is_empty() {
        println!("  video extensions: none");
        return Ok(());
    }
    println!("  video extensions: {}", video_exts.join(" "));

    if !have(vk::KHR_VIDEO_QUEUE_NAME) {
        println!("  no VK_KHR_video_queue — nothing further to ask");
        return Ok(());
    }

    // Queue families, with the video codec operations chained in. This is the
    // half of the answer that says *where* a decode can be submitted, and it is
    // the half `shared_vulkan` has to mirror: docs/design.md §0.4 item 7 makes the
    // queue-family table ours rather than the driver's.
    // SAFETY: the count-then-fill pair is ash's, and the chain outlives it.
    let count = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
    let mut video_props = vec![vk::QueueFamilyVideoPropertiesKHR::default(); count];
    let mut fam_props = vec![vk::QueueFamilyProperties2::default(); count];
    for (f, v) in fam_props.iter_mut().zip(video_props.iter_mut()) {
        *f = f.push_next(v);
    }
    unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut fam_props) };
    let families: Vec<_> = fam_props
        .iter()
        .map(|f| f.queue_family_properties)
        .collect();
    drop(fam_props);
    for (i, q) in families.iter().enumerate() {
        println!(
            "  queue family {i}: count {} flags {} codecs {}",
            q.queue_count,
            queue_flag_names(q.queue_flags),
            codec_op_names(video_props[i].video_codec_operations),
        );
    }

    for probe in probes() {
        if !have(codec_extension(probe.op)) {
            continue;
        }
        report_profile(video_fns, pd, &probe);
    }
    Ok(())
}

fn report_profile(
    video_fns: &ash::khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    probe: &Probe,
) {
    let mut decode_caps = vk::VideoDecodeCapabilitiesKHR::default();
    let mut h264 = vk::VideoDecodeH264CapabilitiesKHR::default();
    let mut h265 = vk::VideoDecodeH265CapabilitiesKHR::default();
    let mut av1 = vk::VideoDecodeAV1CapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default();
    caps = caps.push_next(&mut decode_caps);
    caps = match probe.codec {
        CodecProfile::H264 { .. } => caps.push_next(&mut h264),
        CodecProfile::H265 { .. } => caps.push_next(&mut h265),
        CodecProfile::Av1 { .. } => caps.push_next(&mut av1),
    };

    let res = with_profile(probe, |profile| {
        // SAFETY: both structures outlive the call; the profile chain is built
        // by `with_profile` and is not touched after it returns.
        unsafe {
            (video_fns.fp().get_physical_device_video_capabilities_khr)(pd, profile, &mut caps)
        }
    });
    if res != vk::Result::SUCCESS {
        println!("  {}: NOT SUPPORTED ({res:?})", probe.label);
        return;
    }

    let std_name = unsafe { CStr::from_ptr(caps.std_header_version.extension_name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    println!("  {}: SUPPORTED", probe.label);
    println!(
        "      coded extent {}x{} .. {}x{}, granularity {}x{}",
        caps.min_coded_extent.width,
        caps.min_coded_extent.height,
        caps.max_coded_extent.width,
        caps.max_coded_extent.height,
        caps.picture_access_granularity.width,
        caps.picture_access_granularity.height,
    );
    println!(
        "      dpb slots {} active refs {} flags {:?}",
        caps.max_dpb_slots, caps.max_active_reference_pictures, caps.flags
    );
    println!(
        "      bitstream buffer alignment: offset {} size {}",
        caps.min_bitstream_buffer_offset_alignment, caps.min_bitstream_buffer_size_alignment
    );
    // `VK_MAKE_VIDEO_STD_VERSION` packs 10/12/10 rather than Vulkan's 3/7/12/12,
    // so printing it with the ordinary helper gives a nonsense triple.
    let v = caps.std_header_version.spec_version;
    println!(
        "      std header {std_name} v{}.{}.{}",
        v >> 22,
        (v >> 12) & 0x3ff,
        v & 0xfff
    );
    println!("      decode flags {:?}", decode_caps.flags);
    match probe.codec {
        CodecProfile::H264 { .. } => println!(
            "      h264 maxLevelIdc {} [{}] fieldOffsetGranularity {}x{}",
            h264.max_level_idc,
            h264_level(h264.max_level_idc),
            h264.field_offset_granularity.x,
            h264.field_offset_granularity.y
        ),
        CodecProfile::H265 { .. } => println!(
            "      h265 maxLevelIdc {} [{}]",
            h265.max_level_idc,
            h265_level(h265.max_level_idc)
        ),
        CodecProfile::Av1 { .. } => println!("      av1  maxLevel {}", av1.max_level),
    }

    for (usage, what) in [
        (vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR, "dst"),
        (vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR, "dpb"),
    ] {
        let formats = with_profile(probe, |profile| {
            let profiles = [*profile];
            let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
            let info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
                .image_usage(usage)
                .push_next(&mut list);
            let mut n = 0u32;
            // SAFETY: count-then-fill, both calls with the same `info`.
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
                let mut out = vec![vk::VideoFormatPropertiesKHR::default(); n as usize];
                let r = (video_fns.fp().get_physical_device_video_format_properties_khr)(
                    pd,
                    &info,
                    &mut n,
                    out.as_mut_ptr(),
                );
                if r != vk::Result::SUCCESS {
                    return Vec::new();
                }
                out.truncate(n as usize);
                out
            }
        });
        if formats.is_empty() {
            println!("      {what} formats: none reported");
            continue;
        }
        for f in &formats {
            println!(
                "      {what} format {:?} tiling {:?} usage {:?}",
                f.format, f.image_tiling, f.image_usage_flags
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The survey is only as good as its list, and the list is hand-written.
    /// If a codec loses its probe the report goes quiet rather than wrong,
    /// which is exactly the failure this project keeps finding.
    #[test]
    fn every_decode_codec_this_crate_knows_has_at_least_one_probe() {
        let ps = probes();
        for op in [
            vk::VideoCodecOperationFlagsKHR::DECODE_H264,
            vk::VideoCodecOperationFlagsKHR::DECODE_H265,
            vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
        ] {
            assert!(ps.iter().any(|p| p.op == op), "no probe for {op:?}");
        }
    }

    #[test]
    fn every_probe_names_the_extension_that_gates_it() {
        for p in probes() {
            let ext = codec_extension(p.op);
            assert_ne!(
                ext,
                vk::KHR_VIDEO_QUEUE_NAME,
                "{} fell through to the catch-all",
                p.label
            );
        }
    }

    /// VP9 decode postdates ash 0.38.0+1.3.281. RADV and the NVIDIA driver both
    /// report it, so a report that named only the bits ash knows would show a
    /// blank where a codec is. Pinned so that a later ash bump is noticed.
    #[test]
    fn vp9_is_named_even_though_ash_does_not_know_it() {
        let f = vk::VideoCodecOperationFlagsKHR::from_raw(0x8);
        assert_eq!(codec_op_names(f), "DECODE_VP9");
    }

    /// The names ash *does* have must still agree with the table, or the table
    /// is a second source of truth that has drifted.
    #[test]
    fn the_hand_written_bits_agree_with_ash() {
        for (bit, name) in [
            (vk::VideoCodecOperationFlagsKHR::DECODE_H264, "DECODE_H264"),
            (vk::VideoCodecOperationFlagsKHR::DECODE_H265, "DECODE_H265"),
            (vk::VideoCodecOperationFlagsKHR::DECODE_AV1, "DECODE_AV1"),
            (vk::VideoCodecOperationFlagsKHR::ENCODE_H264, "ENCODE_H264"),
            (vk::VideoCodecOperationFlagsKHR::ENCODE_H265, "ENCODE_H265"),
        ] {
            let found = CODEC_OPS.iter().find(|&&(_, n)| n == name).unwrap().0;
            assert_eq!(found, bit.as_raw(), "{name}");
        }
    }

    #[test]
    fn an_unnamed_bit_is_reported_rather_than_dropped() {
        let f = vk::VideoCodecOperationFlagsKHR::from_raw(0x8000);
        assert!(codec_op_names(f).contains("unnamed(0x8000)"));
    }

    #[test]
    fn no_codec_operations_reads_as_a_dash() {
        assert_eq!(
            codec_op_names(vk::VideoCodecOperationFlagsKHR::empty()),
            "-"
        );
        assert_eq!(queue_flag_names(vk::QueueFlags::empty()), "-");
    }

    /// The chain is the one thing in this file that can be wrong without the
    /// compiler noticing: a `pNext` pointing at a dropped temporary. Building
    /// every profile and reading the link back is cheap insurance.
    #[test]
    fn every_probe_builds_a_chain_whose_pnext_is_the_codec_struct() {
        for p in probes() {
            with_profile(&p, |profile| {
                assert_eq!(profile.video_codec_operation, p.op);
                assert!(!profile.p_next.is_null(), "{}: no codec struct", p.label);
                // SAFETY: the chain is alive inside the callback, and every
                // Vulkan structure starts with its `sType`.
                let stype = unsafe { *(profile.p_next as *const vk::StructureType) };
                let want = match p.codec {
                    CodecProfile::H264 { .. } => vk::StructureType::VIDEO_DECODE_H264_PROFILE_INFO_KHR,
                    CodecProfile::H265 { .. } => vk::StructureType::VIDEO_DECODE_H265_PROFILE_INFO_KHR,
                    CodecProfile::Av1 { .. } => vk::StructureType::VIDEO_DECODE_AV1_PROFILE_INFO_KHR,
                };
                assert_eq!(stype, want, "{}", p.label);
            });
        }
    }
}
