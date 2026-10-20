//! Wire interception for synthetic VideoToolbox sessions. Synthetic handles are
//! kept apart from the Vulkan driver's object table, including tombstones.
use crate::{
    native_video::{NativeVideo, Profile, VideoResult},
    server::GpuServer,
};
use ash::vk;
use std::collections::{HashMap, HashSet};
use sv_proto as p;
const HEADER: &[u8] = b"VK_STD_vulkan_video_codec_h264_decode\0";
const VERSION: u32 = 1 << 22;
fn fail() -> vk::Result {
    vk::Result::ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR
}
fn invalid() -> vk::Result {
    vk::Result::ERROR_INITIALIZATION_FAILED
}
fn wire_result(r: VideoResult<()>) -> p::VkResult {
    p::VkResult(r.err().map_or(0, |e| e.as_raw()))
}
#[derive(Default)]
pub struct DispatchState {
    pub plans:crate::video_recording::Plans,
    pub video: NativeVideo,
    failure: Option<vk::Result>,
    sessions: HashMap<u64, u64>,
    parameters: HashMap<u64, u64>,
    session_ids: HashSet<u64>,
    parameter_ids: HashSet<u64>,
}
impl DispatchState {
    /// This must remain false until native decode, joined NV12 import and the
    /// queue worker are all validated together. Constructors never enable it.
    pub fn fail(&mut self, error: vk::Result) {
        if self.failure.is_none() {
            self.failure = Some(error)
        }
    }
    pub fn failure(&self) -> Option<vk::Result> {
        self.failure
    }
    pub fn available(&self) -> bool {
        self.failure.is_none() && self.video.available()
    }
    pub fn owns_session(&self, wire: u64) -> bool {
        self.session_ids.contains(&wire)
    }
    pub fn owns_parameters(&self, wire: u64) -> bool {
        self.parameter_ids.contains(&wire)
    }
    pub fn session(&self, wire: u64) -> Option<u64> {
        self.sessions.get(&wire).copied()
    }
    pub fn parameters(&self, wire: u64) -> Option<u64> {
        self.parameters.get(&wire).copied()
    }
}
fn profile(p: &p::VkVideoProfileInfoKHR) -> VideoResult<Profile> {
    let [p::PNextLink::VkVideoDecodeH264ProfileInfoKHR(h)] = p.p_next.as_slice() else {
        return Err(fail());
    };
    if !h.p_next.is_empty() {
        return Err(fail());
    }
    Profile::validate(
        &vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::from_raw(
                p.video_codec_operation.0 as u32,
            ))
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::from_raw(
                p.chroma_subsampling.0,
            ))
            .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::from_raw(
                p.luma_bit_depth.0,
            ))
            .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::from_raw(
                p.chroma_bit_depth.0,
            )),
        &vk::VideoDecodeH264ProfileInfoKHR::default()
            .std_profile_idc(h.std_profile_idc.0 as u32)
            .picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::from_raw(
                h.picture_layout.0 as u32,
            )),
    )
}
fn header() -> p::VkExtensionProperties {
    let mut h = p::VkExtensionProperties::default();
    h.extension_name[..HEADER.len()].copy_from_slice(HEADER);
    h.spec_version = VERSION;
    h
}
fn chain_caps(links: &mut [p::PNextLink]) -> VideoResult<()> {
    let mut decode = false;
    let mut h264 = false;
    for link in links {
        match link {
            p::PNextLink::VkVideoDecodeCapabilitiesKHR(v) if !decode => {
                decode = true;
                v.flags = p::VkVideoDecodeCapabilityFlagsKHR(
                    vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT.as_raw(),
                );
                if !v.p_next.is_empty() {
                    return Err(fail());
                }
            }
            p::PNextLink::VkVideoDecodeH264CapabilitiesKHR(v) if !h264 => {
                h264 = true;
                v.max_level_idc = p::StdVideoH264LevelIdc(15);
                v.field_offset_granularity = p::VkOffset2D::default();
                if !v.p_next.is_empty() {
                    return Err(fail());
                }
            }
            _ => return Err(fail()),
        }
    }
    if !decode || !h264 {
        return Err(fail());
    }
    Ok(())
}
impl GpuServer {
    pub(crate) fn native_get_physical_device_video_capabilities_khr(
        &mut self,
        a: &p::VkGetPhysicalDeviceVideoCapabilitiesKHRArgs,
    ) -> Option<sv_wire::Result<p::VkGetPhysicalDeviceVideoCapabilitiesKHRReply>> {
        let state = self.shared.native_video.lock().unwrap();
        let mut reply = p::VkGetPhysicalDeviceVideoCapabilitiesKHRReply::default();
        let r = (|| -> VideoResult<()> {
            if !state.available() {
                return Err(fail());
            }
            let profile = profile(a.p_video_profile.as_deref().ok_or_else(invalid)?)?;
            let c = state.video.capabilities(profile)?;
            let mut out = a.p_capabilities.clone().ok_or_else(invalid)?;
            chain_caps(&mut out.p_next)?;
            out.flags = p::VkVideoCapabilityFlagsKHR(c.flags.as_raw());
            out.min_bitstream_buffer_offset_alignment = c.min_bitstream_buffer_offset_alignment;
            out.min_bitstream_buffer_size_alignment = c.min_bitstream_buffer_size_alignment;
            out.picture_access_granularity = p::VkExtent2D {
                width: c.picture_access_granularity.width,
                height: c.picture_access_granularity.height,
            };
            out.min_coded_extent = p::VkExtent2D {
                width: c.min_coded_extent.width,
                height: c.min_coded_extent.height,
            };
            out.max_coded_extent = p::VkExtent2D {
                width: c.max_coded_extent.width,
                height: c.max_coded_extent.height,
            };
            out.max_dpb_slots = c.max_dpb_slots;
            out.max_active_reference_pictures = c.max_active_reference_pictures;
            out.std_header_version = header();
            reply.p_capabilities = Some(out);
            Ok(())
        })();
        reply.result = wire_result(r);
        Some(Ok(reply))
    }
    pub(crate) fn native_create_video_session_khr(
        &mut self,
        a: &p::VkCreateVideoSessionKHRArgs,
    ) -> Option<sv_wire::Result<p::VkCreateVideoSessionKHRReply>> {
        let mut state = self.shared.native_video.lock().unwrap();
        let mut reply = p::VkCreateVideoSessionKHRReply::default();
        let r = (|| -> VideoResult<()> {
            if !state.available() {
                return Err(fail());
            }
            let c = a.p_create_info.as_deref().ok_or_else(invalid)?;
            if !c.p_next.is_empty() || c.queue_family_index != self.shared.driver.queue_family {
                return Err(fail());
            }
            let h = c.p_std_header_version.as_deref().ok_or_else(invalid)?;
            if h.extension_name != header().extension_name || h.spec_version != VERSION {
                return Err(fail());
            }
            let p = profile(c.p_video_profile.as_deref().ok_or_else(invalid)?)?;
            let wire = a.p_video_session.ok_or_else(invalid)?.0;
            if wire == 0 || state.session_ids.contains(&wire) {
                return Err(invalid());
            }
            let ci = vk::VideoSessionCreateInfoKHR::default()
                .flags(vk::VideoSessionCreateFlagsKHR::from_raw(c.flags.0))
                .queue_family_index(c.queue_family_index)
                .picture_format(vk::Format::from_raw(c.picture_format.0))
                .reference_picture_format(vk::Format::from_raw(c.reference_picture_format.0))
                .max_coded_extent(vk::Extent2D {
                    width: c.max_coded_extent.width,
                    height: c.max_coded_extent.height,
                })
                .max_dpb_slots(c.max_dpb_slots)
                .max_active_reference_pictures(c.max_active_reference_pictures);
            let id = state.video.create_session(p, &ci)?;
            state.sessions.insert(wire, id);
            state.session_ids.insert(wire);
            reply.p_video_session = a.p_video_session;
            Ok(())
        })();
        reply.result = wire_result(r);
        Some(Ok(reply))
    }
}
fn add_sets(
    video: &mut NativeVideo,
    id: u64,
    sequence: Option<u32>,
    add: &p::VkVideoDecodeH264SessionParametersAddInfoKHR,
) -> VideoResult<()> {
    if !add.p_next.is_empty()
        || add.std_sps_count as usize != add.p_std_sp_ss.len()
        || add.std_pps_count as usize != add.p_std_pp_ss.len()
    {
        return Err(invalid());
    }
    let mut pool = sv_gpu_entry::Raise::default();
    let c =
        sv_gpu_entry::raise_vk_video_decode_h264_session_parameters_add_info_khr(add, &mut pool)
            .map_err(|_| invalid())?;
    // Both definitions describe the same Khronos StdVideo C ABI. The raise pool
    // owns every nested array until synchronous NAL serialization finishes.
    const {
        assert!(
            std::mem::size_of::<sv_gpu_entry::StdVideoH264SequenceParameterSet>()
                == std::mem::size_of::<vk::native::StdVideoH264SequenceParameterSet>()
        );
        assert!(
            std::mem::align_of::<sv_gpu_entry::StdVideoH264SequenceParameterSet>()
                == std::mem::align_of::<vk::native::StdVideoH264SequenceParameterSet>()
        );
        assert!(
            std::mem::size_of::<sv_gpu_entry::StdVideoH264PictureParameterSet>()
                == std::mem::size_of::<vk::native::StdVideoH264PictureParameterSet>()
        );
        assert!(
            std::mem::align_of::<sv_gpu_entry::StdVideoH264PictureParameterSet>()
                == std::mem::align_of::<vk::native::StdVideoH264PictureParameterSet>()
        );
    }
    let sps = if add.std_sps_count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(c.p_std_sp_ss.cast(), add.std_sps_count as usize) }
    };
    let pps = if add.std_pps_count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(c.p_std_pp_ss.cast(), add.std_pps_count as usize) }
    };
    unsafe {
        match sequence {
            Some(n) => video.update_parameters(id, n, sps, pps),
            None => video.initialize_parameters(id, sps, pps),
        }
    }
}
impl GpuServer {
    pub(crate) fn native_create_video_session_parameters_khr(
        &mut self,
        a: &p::VkCreateVideoSessionParametersKHRArgs,
    ) -> Option<sv_wire::Result<p::VkCreateVideoSessionParametersKHRReply>> {
        let mut state = self.shared.native_video.lock().unwrap();
        let c = a.p_create_info.as_deref()?;
        let session = state.sessions.get(&c.video_session.0).copied();
        if session.is_none() && !state.session_ids.contains(&c.video_session.0) {
            return None;
        }
        let mut reply = p::VkCreateVideoSessionParametersKHRReply::default();
        let r = (|| -> VideoResult<()> {
            let session = session.ok_or_else(invalid)?;
            if c.flags.0 != 0 {
                return Err(fail());
            }
            let [p::PNextLink::VkVideoDecodeH264SessionParametersCreateInfoKHR(h)] =
                c.p_next.as_slice()
            else {
                return Err(fail());
            };
            if !h.p_next.is_empty() {
                return Err(fail());
            }
            let wire = a.p_video_session_parameters.ok_or_else(invalid)?.0;
            if wire == 0 || state.parameter_ids.contains(&wire) {
                return Err(invalid());
            }
            let template = if c.video_session_parameters_template.0 == 0 {
                None
            } else {
                Some(
                    *state
                        .parameters
                        .get(&c.video_session_parameters_template.0)
                        .ok_or_else(invalid)?,
                )
            };
            let id = state.video.create_parameters(
                session,
                template,
                h.max_std_sps_count as usize,
                h.max_std_pps_count as usize,
            )?;
            if let Some(add) = h.p_parameters_add_info.as_deref() {
                if let Err(e) = add_sets(&mut state.video, id, None, add) {
                    state.video.destroy_parameters(id);
                    return Err(e);
                }
            }
            state.parameters.insert(wire, id);
            state.parameter_ids.insert(wire);
            reply.p_video_session_parameters = a.p_video_session_parameters;
            Ok(())
        })();
        reply.result = wire_result(r);
        Some(Ok(reply))
    }
    pub(crate) fn native_update_video_session_parameters_khr(
        &mut self,
        a: &p::VkUpdateVideoSessionParametersKHRArgs,
    ) -> Option<sv_wire::Result<p::VkUpdateVideoSessionParametersKHRReply>> {
        let mut state = self.shared.native_video.lock().unwrap();
        if !state.parameter_ids.contains(&a.video_session_parameters.0) {
            return None;
        }
        let r = (|| -> VideoResult<()> {
            let id = *state
                .parameters
                .get(&a.video_session_parameters.0)
                .ok_or_else(invalid)?;
            let c = a.p_update_info.as_deref().ok_or_else(invalid)?;
            let [p::PNextLink::VkVideoDecodeH264SessionParametersAddInfoKHR(add)] =
                c.p_next.as_slice()
            else {
                return Err(fail());
            };
            add_sets(&mut state.video, id, Some(c.update_sequence_count), add)
        })();
        Some(Ok(p::VkUpdateVideoSessionParametersKHRReply {
            result: wire_result(r),
        }))
    }
    pub(crate) fn native_destroy_video_session_khr(
        &mut self,
        a: &p::VkDestroyVideoSessionKHRArgs,
    ) -> Option<sv_wire::Result<()>> {
        let mut state = self.shared.native_video.lock().unwrap();
        if !state.session_ids.contains(&a.video_session.0) {
            return None;
        }
        if let Some(id) = state.sessions.remove(&a.video_session.0) {
            state.video.destroy_session(id)
        }
        Some(Ok(()))
    }
    pub(crate) fn native_destroy_video_session_parameters_khr(
        &mut self,
        a: &p::VkDestroyVideoSessionParametersKHRArgs,
    ) -> Option<sv_wire::Result<()>> {
        let mut state = self.shared.native_video.lock().unwrap();
        if !state.parameter_ids.contains(&a.video_session_parameters.0) {
            return None;
        }
        if let Some(id) = state.parameters.remove(&a.video_session_parameters.0) {
            state.video.destroy_parameters(id)
        }
        Some(Ok(()))
    }
    pub(crate) fn native_get_video_session_memory_requirements_khr(
        &mut self,
        a: &p::VkGetVideoSessionMemoryRequirementsKHRArgs,
    ) -> Option<sv_wire::Result<p::VkGetVideoSessionMemoryRequirementsKHRReply>> {
        let state = self.shared.native_video.lock().unwrap();
        if !state.session_ids.contains(&a.video_session.0) {
            return None;
        }
        let r = state
            .sessions
            .get(&a.video_session.0)
            .ok_or_else(invalid)
            .and_then(|&id| state.video.memory_requirements(id).map(|_| ()));
        Some(Ok(p::VkGetVideoSessionMemoryRequirementsKHRReply {
            result: wire_result(r),
            p_memory_requirements_count: a.p_memory_requirements_count.map(|_| 0),
            p_memory_requirements: vec![],
        }))
    }
    pub(crate) fn native_bind_video_session_memory_khr(
        &mut self,
        a: &p::VkBindVideoSessionMemoryKHRArgs,
    ) -> Option<sv_wire::Result<p::VkBindVideoSessionMemoryKHRReply>> {
        let state = self.shared.native_video.lock().unwrap();
        if !state.session_ids.contains(&a.video_session.0) {
            return None;
        }
        let r =
            if a.bind_session_memory_info_count != 0 || !a.p_bind_session_memory_infos.is_empty() {
                Err(invalid())
            } else {
                state
                    .sessions
                    .get(&a.video_session.0)
                    .ok_or_else(invalid)
                    .and_then(|&id| state.video.bind_memory(id, &[]))
            };
        Some(Ok(p::VkBindVideoSessionMemoryKHRReply {
            result: wire_result(r),
        }))
    }
}
impl GpuServer {
    pub(crate) fn native_get_physical_device_video_format_properties_khr(
        &mut self,
        a: &p::VkGetPhysicalDeviceVideoFormatPropertiesKHRArgs,
    ) -> Option<sv_wire::Result<p::VkGetPhysicalDeviceVideoFormatPropertiesKHRReply>> {
        let state = self.shared.native_video.lock().unwrap();
        let mut reply = p::VkGetPhysicalDeviceVideoFormatPropertiesKHRReply::default();
        let r = (|| -> VideoResult<()> {
            if !state.available() {
                return Err(fail());
            }
            let info = a.p_video_format_info.as_deref().ok_or_else(invalid)?;
            let [p::PNextLink::VkVideoProfileListInfoKHR(list)] = info.p_next.as_slice() else {
                return Err(fail());
            };
            if !list.p_next.is_empty() || list.profile_count != 1 || list.p_profiles.len() != 1 {
                return Err(fail());
            }
            profile(&list.p_profiles[0])?;
            let supported = vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
                | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR
                | vk::ImageUsageFlags::SAMPLED;
            let requested = vk::ImageUsageFlags::from_raw(info.image_usage.0);
            if requested.is_empty()
                || !supported.contains(requested)
                || !requested.intersects(
                    vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
                        | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR,
                )
            {
                return Err(fail());
            }
            let count = a.p_video_format_property_count.ok_or_else(invalid)?;
            if a.p_video_format_properties.is_empty() {
                reply.p_video_format_property_count = Some(1);
                return Ok(());
            }
            if count == 0 {
                reply.p_video_format_property_count = Some(0);
                return Err(vk::Result::INCOMPLETE);
            }
            if a.p_video_format_properties
                .iter()
                .any(|v| !v.p_next.is_empty())
            {
                return Err(fail());
            }
            let mut out = p::VkVideoFormatPropertiesKHR::default();
            out.format = p::VkFormat(vk::Format::G8_B8R8_2PLANE_420_UNORM.as_raw());
            out.image_type = p::VkImageType(vk::ImageType::TYPE_2D.as_raw());
            out.image_tiling = p::VkImageTiling(vk::ImageTiling::LINEAR.as_raw());
            out.image_usage_flags = p::VkImageUsageFlags(supported.as_raw());
            reply.p_video_format_property_count = Some(1);
            reply.p_video_format_properties = vec![out];
            Ok(())
        })();
        reply.result = wire_result(r);
        Some(Ok(reply))
    }
}
