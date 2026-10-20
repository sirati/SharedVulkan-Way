//! Logical H.264 NV12 queries backed by the actual sampled Metal import path.
//! Nonvideo queries remain on the existing native/PRIME path.
use crate::{driver::Driver, server::GpuServer};
use ash::vk;
use sv_proto as p;

const FORMAT: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
fn video_usage() -> vk::ImageUsageFlags {
    vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR
}
fn unsupported() -> vk::Result {
    vk::Result::ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR
}
fn format_error() -> vk::Result {
    vk::Result::ERROR_FORMAT_NOT_SUPPORTED
}

fn backing(driver: &Driver) -> Result<(vk::ImageFormatProperties, u32), vk::Result> {
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(
        vk::ExternalMemoryHandleTypeFlags::from_raw(crate::iosurface::MTLTEXTURE),
    );
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(FORMAT)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(vk::ImageUsageFlags::SAMPLED)
        .push_next(&mut external_info);
    let mut external = vk::ExternalImageFormatProperties::default();
    let mut conversion = vk::SamplerYcbcrConversionImageFormatProperties::default();
    let mut output = vk::ImageFormatProperties2::default()
        .push_next(&mut external)
        .push_next(&mut conversion);
    unsafe {
        driver
            .instance
            .get_physical_device_image_format_properties2(driver.pd, &info, &mut output)
    }?;
    let props = output.image_format_properties;
    if !crate::metal_image::importable(
        external
            .external_memory_properties
            .external_memory_features
            .as_raw(),
        external
            .external_memory_properties
            .compatible_handle_types
            .as_raw(),
        props.sample_counts.as_raw(),
    ) || props.max_extent.width < 64
        || props.max_extent.height < 64
        || props.max_extent.depth < 1
        || props.max_mip_levels < 1
        || props.max_array_layers < 1
    {
        return Err(format_error());
    }
    Ok((props, conversion.combined_image_sampler_descriptor_count))
}

fn candidate(info: &p::VkPhysicalDeviceImageFormatInfo2) -> bool {
    info.usage.0 & video_usage().as_raw() != 0
        || info.p_next.iter().any(|link| {
            if let p::PNextLink::VkVideoProfileListInfoKHR(list) = link {
                list.p_profiles.iter().any(|profile| {
                    profile.p_next.iter().any(|link| {
                        matches!(link, p::PNextLink::VkVideoDecodeH264ProfileInfoKHR(_))
                    })
                })
            } else {
                false
            }
        })
}
fn validate(
    info: &p::VkPhysicalDeviceImageFormatInfo2,
) -> Result<crate::native_video::Profile, vk::Result> {
    if info.format.0 != FORMAT.as_raw()
        || info.type_.0 != vk::ImageType::TYPE_2D.as_raw()
        || info.tiling.0 != vk::ImageTiling::LINEAR.as_raw()
        || info.flags.0 != 0
    {
        return Err(format_error());
    }
    let usage = vk::ImageUsageFlags::from_raw(info.usage.0);
    if usage.is_empty() || !(video_usage() | vk::ImageUsageFlags::SAMPLED).contains(usage) {
        return Err(format_error());
    }
    let [p::PNextLink::VkVideoProfileListInfoKHR(list)] = info.p_next.as_slice() else {
        // Logical external import/export is not supported by the dedicated-only
        // allocation path. Internal MTLTexture imports do not grant that API.
        return Err(unsupported());
    };
    if !list.p_next.is_empty() || list.profile_count != 1 || list.p_profiles.len() != 1 {
        return Err(unsupported());
    }
    crate::native_video_dispatch::profile(&list.p_profiles[0])
}

impl GpuServer {
    pub(crate) fn native_video_format_properties(
        &self,
        format: p::VkFormat,
        props: &mut p::VkFormatProperties,
    ) {
        if format.0 != FORMAT.as_raw() || !self.shared.driver.native_video_queue_contract() {
            return;
        }
        let state = self
            .shared
            .native_video
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !state.available() {
            return;
        }
        drop(state);
        if backing(&self.shared.driver).is_err() {
            return;
        }
        // Copy only features reported for the exact linear native format. This
        // includes its actual YCbCr filtering/reconstruction capabilities.
        let native = unsafe {
            self.shared
                .driver
                .instance
                .get_physical_device_format_properties(self.shared.driver.pd, FORMAT)
        };
        props.linear_tiling_features.0 |= native.linear_tiling_features.as_raw()
            | vk::FormatFeatureFlags::VIDEO_DECODE_OUTPUT_KHR.as_raw()
            | vk::FormatFeatureFlags::VIDEO_DECODE_DPB_KHR.as_raw();
    }
    pub(crate) fn native_video_image_format_properties2(
        &self,
        args: &p::VkGetPhysicalDeviceImageFormatProperties2Args,
    ) -> Option<sv_wire::Result<p::VkGetPhysicalDeviceImageFormatProperties2Reply>> {
        let info = args.p_image_format_info.as_deref()?;
        if !candidate(info) {
            return None;
        }
        let result = (|| {
            let state = self
                .shared
                .native_video
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !state.available() || !self.shared.driver.native_video_queue_contract() {
                return Err(unsupported());
            }
            let profile = validate(info)?;
            let caps = state.video.capabilities(profile)?;
            drop(state);
            let (native, descriptors) = backing(&self.shared.driver)?;
            let mut out = args
                .p_image_format_properties
                .clone()
                .ok_or_else(format_error)?;
            for link in &mut out.p_next {
                match link {
                    p::PNextLink::VkExternalImageFormatProperties(properties)
                        if properties.p_next.is_empty() =>
                    {
                        // No logical external handle was requested or accepted.
                        // Internal backing importability is deliberately private.
                        properties.external_memory_properties =
                            p::VkExternalMemoryProperties::default();
                    }
                    p::PNextLink::VkSamplerYcbcrConversionImageFormatProperties(properties)
                        if properties.p_next.is_empty() =>
                    {
                        properties.combined_image_sampler_descriptor_count = descriptors;
                    }
                    _ => return Err(unsupported()),
                }
            }
            let props = &mut out.image_format_properties;
            props.max_extent = p::VkExtent3D {
                width: 64.min(caps.max_coded_extent.width),
                height: 64.min(caps.max_coded_extent.height),
                depth: 1,
            };
            props.max_mip_levels = 1;
            props.max_array_layers = 1;
            props.sample_counts = p::VkSampleCountFlags(vk::SampleCountFlags::TYPE_1.as_raw());
            props.max_resource_size = native.max_resource_size;
            Ok(out)
        })();
        Some(Ok(match result {
            Ok(props) => p::VkGetPhysicalDeviceImageFormatProperties2Reply {
                result: p::VkResult(0),
                p_image_format_properties: Some(props),
            },
            Err(error) => p::VkGetPhysicalDeviceImageFormatProperties2Reply {
                result: p::VkResult(error.as_raw()),
                p_image_format_properties: None,
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn query() -> p::VkPhysicalDeviceImageFormatInfo2 {
        let mut h264 = p::VkVideoDecodeH264ProfileInfoKHR::default();
        h264.std_profile_idc = p::StdVideoH264ProfileIdc(66);
        let mut profile = p::VkVideoProfileInfoKHR::default();
        profile.video_codec_operation = p::VkVideoCodecOperationFlagBitsKHR(
            vk::VideoCodecOperationFlagsKHR::DECODE_H264.as_raw() as i32,
        );
        profile.chroma_subsampling = p::VkVideoChromaSubsamplingFlagsKHR(
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_420.as_raw(),
        );
        profile.luma_bit_depth = p::VkVideoComponentBitDepthFlagsKHR(
            vk::VideoComponentBitDepthFlagsKHR::TYPE_8.as_raw(),
        );
        profile.chroma_bit_depth = profile.luma_bit_depth;
        profile.p_next = vec![p::PNextLink::VkVideoDecodeH264ProfileInfoKHR(Box::new(
            h264,
        ))];
        let mut list = p::VkVideoProfileListInfoKHR::default();
        list.profile_count = 1;
        list.p_profiles = vec![profile];
        let mut info = p::VkPhysicalDeviceImageFormatInfo2::default();
        info.format = p::VkFormat(FORMAT.as_raw());
        info.type_ = p::VkImageType(vk::ImageType::TYPE_2D.as_raw());
        info.tiling = p::VkImageTiling(vk::ImageTiling::LINEAR.as_raw());
        info.usage = p::VkImageUsageFlags((video_usage() | vk::ImageUsageFlags::SAMPLED).as_raw());
        info.p_next = vec![p::PNextLink::VkVideoProfileListInfoKHR(Box::new(list))];
        info
    }
    #[test]
    fn ordinary_nv12_queries_are_not_intercepted() {
        let mut info = query();
        info.p_next.clear();
        info.usage = p::VkImageUsageFlags(vk::ImageUsageFlags::SAMPLED.as_raw());
        assert!(!candidate(&info));
        info.usage = p::VkImageUsageFlags(video_usage().as_raw());
        assert!(candidate(&info));
        assert!(validate(&info).is_err());
    }
    #[test]
    fn logical_contract_refuses_native_transfer_optimal_and_external_requests() {
        let info = query();
        assert_eq!(validate(&info).unwrap().idc, 66);
        let mut transfer = info.clone();
        transfer.usage.0 |= vk::ImageUsageFlags::TRANSFER_SRC.as_raw();
        assert!(validate(&transfer).is_err());
        let mut optimal = info.clone();
        optimal.tiling = p::VkImageTiling(vk::ImageTiling::OPTIMAL.as_raw());
        assert!(validate(&optimal).is_err());
        let mut external = info;
        external
            .p_next
            .push(p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(
                Box::default(),
            ));
        assert!(validate(&external).is_err());
    }
    #[test]
    fn unsupported_profile_shapes_are_refused_before_backing_query() {
        for (chroma, depth, layout) in [
            (
                vk::VideoChromaSubsamplingFlagsKHR::TYPE_420.as_raw(),
                vk::VideoComponentBitDepthFlagsKHR::TYPE_8.as_raw(),
                1,
            ),
            (
                vk::VideoChromaSubsamplingFlagsKHR::TYPE_420.as_raw(),
                vk::VideoComponentBitDepthFlagsKHR::TYPE_10.as_raw(),
                0,
            ),
            (
                vk::VideoChromaSubsamplingFlagsKHR::TYPE_422.as_raw(),
                vk::VideoComponentBitDepthFlagsKHR::TYPE_8.as_raw(),
                0,
            ),
        ] {
            let mut info = query();
            let p::PNextLink::VkVideoProfileListInfoKHR(list) = &mut info.p_next[0] else {
                panic!()
            };
            list.p_profiles[0].chroma_subsampling.0 = chroma;
            list.p_profiles[0].luma_bit_depth.0 = depth;
            let p::PNextLink::VkVideoDecodeH264ProfileInfoKHR(h264) =
                &mut list.p_profiles[0].p_next[0]
            else {
                panic!()
            };
            h264.picture_layout.0 = layout;
            assert!(validate(&info).is_err());
        }
    }
}
