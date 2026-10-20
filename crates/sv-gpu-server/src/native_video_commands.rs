//! Synthetic coding commands retain owned metadata and original wire resource
//! IDs. No decoder, resource lookup or GPU execution occurs during recording.
use crate::{
    native_video::{
        Decode, H264Picture, H264Reference, NativeVideo, Picture, Reference, VideoResult,
    },
    video_recording::Plans,
};
use ash::vk;
use std::sync::Arc;
use sv_proto as p;
fn bad() -> vk::Result {
    vk::Result::ERROR_INITIALIZATION_FAILED
}
fn unsupported() -> vk::Result {
    vk::Result::ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR
}
fn picture(p: &p::VkVideoPictureResourceInfoKHR) -> VideoResult<Picture> {
    if !p.p_next.is_empty() {
        return Err(unsupported());
    }
    Ok(Picture {
        image_view: p.image_view_binding.0,
        layer: p.base_array_layer,
        offset: vk::Offset2D {
            x: p.coded_offset.x,
            y: p.coded_offset.y,
        },
        extent: vk::Extent2D {
            width: p.coded_extent.width,
            height: p.coded_extent.height,
        },
    })
}
fn picture_metadata(p: &p::StdVideoDecodeH264PictureInfo) -> VideoResult<H264Picture> {
    if p.flags.0 & !0x3f != 0 {
        return Err(unsupported());
    }
    let mut pool = sv_gpu_entry::Raise::default();
    let raised =
        sv_gpu_entry::raise_std_video_decode_h264_picture_info(p, &mut pool).map_err(|_| bad())?;
    const {
        assert!(
            std::mem::size_of::<sv_gpu_entry::StdVideoDecodeH264PictureInfo>()
                == std::mem::size_of::<vk::native::StdVideoDecodeH264PictureInfo>()
        );
        assert!(
            std::mem::align_of::<sv_gpu_entry::StdVideoDecodeH264PictureInfo>()
                == std::mem::align_of::<vk::native::StdVideoDecodeH264PictureInfo>()
        );
    }
    // Both bindings describe Khronos's StdVideo C ABI; no pointer is retained.
    H264Picture::from_std(unsafe {
        &*((&raised as *const sv_gpu_entry::StdVideoDecodeH264PictureInfo).cast())
    })
}
fn reference_metadata(p: &p::StdVideoDecodeH264ReferenceInfo) -> VideoResult<H264Reference> {
    if p.flags.0 & !0xf != 0 {
        return Err(unsupported());
    }
    let mut pool = sv_gpu_entry::Raise::default();
    let raised = sv_gpu_entry::raise_std_video_decode_h264_reference_info(p, &mut pool)
        .map_err(|_| bad())?;
    const {
        assert!(
            std::mem::size_of::<sv_gpu_entry::StdVideoDecodeH264ReferenceInfo>()
                == std::mem::size_of::<vk::native::StdVideoDecodeH264ReferenceInfo>()
        );
        assert!(
            std::mem::align_of::<sv_gpu_entry::StdVideoDecodeH264ReferenceInfo>()
                == std::mem::align_of::<vk::native::StdVideoDecodeH264ReferenceInfo>()
        );
    }
    H264Reference::from_std(unsafe {
        &*((&raised as *const sv_gpu_entry::StdVideoDecodeH264ReferenceInfo).cast())
    })
}
fn reference(slot: &p::VkVideoReferenceSlotInfoKHR) -> VideoResult<Reference> {
    let index = u32::try_from(slot.slot_index).map_err(|_| bad())?;
    let [p::PNextLink::VkVideoDecodeH264DpbSlotInfoKHR(h264)] = slot.p_next.as_slice() else {
        return Err(unsupported());
    };
    if !h264.p_next.is_empty() {
        return Err(unsupported());
    }
    Ok(Reference {
        slot: index,
        picture: picture(slot.p_picture_resource.as_deref().ok_or_else(unsupported)?)?,
        h264: reference_metadata(h264.p_std_reference_info.as_deref().ok_or_else(bad)?)?,
    })
}
fn reject(video: &mut NativeVideo, plans: &mut Plans, command: u64) {
    plans.poison(
        command,
        "invalid or unsupported synthetic video coding command",
    );
    video.reset_command(command);
}
/// Parent passes resolved synthetic IDs, never native-driver handles.
pub fn begin(
    video: &mut NativeVideo,
    plans: &mut Plans,
    command: u64,
    session: u64,
    parameters: u64,
    info: &p::VkVideoBeginCodingInfoKHR,
) -> VideoResult<()> {
    let result = (|| {
        if !info.p_next.is_empty()
            || info.flags.0 != 0
            || info.reference_slot_count as usize != info.p_reference_slots.len()
        {
            return Err(unsupported());
        }
        let mut refs = Vec::new();
        let mut bound = Vec::new();
        for slot in &info.p_reference_slots {
            if slot.slot_index == -1 {
                // Stock Zink binds the upcoming setup resource with slot -1.
                // It is not an active DPB reference and must not become one.
                bound.push(picture(
                    slot.p_picture_resource.as_deref().ok_or_else(unsupported)?,
                )?);
                if !slot.p_next.is_empty() {
                    let mut copy = slot.clone();
                    copy.slot_index = 0;
                    reference(&copy)?;
                }
            } else {
                let r = reference(slot)?;
                bound.push(r.picture.clone());
                refs.push(r);
            }
        }
        plans.begin_scope(command).map_err(|_| bad())?;
        video.begin(command, session, parameters)?;
        video.begin_references(command, refs.into())?;
        video.set_scope_bound_pictures(command, bound.into())?;
        Ok(())
    })();
    if result.is_err() {
        reject(video, plans, command)
    }
    result
}
pub fn control(
    video: &mut NativeVideo,
    plans: &mut Plans,
    command: u64,
    info: &p::VkVideoCodingControlInfoKHR,
) -> VideoResult<()> {
    let result = (|| {
        if !info.p_next.is_empty() || info.flags.0 != vk::VideoCodingControlFlagsKHR::RESET.as_raw()
        {
            return Err(unsupported());
        }
        if !video.scope_active(command) {
            return Err(bad());
        }
        video.reset_scope(command)?;
        Ok(())
    })();
    if result.is_err() {
        reject(video, plans, command)
    }
    result
}
pub fn decode(
    video: &mut NativeVideo,
    plans: &mut Plans,
    command: u64,
    info: &p::VkVideoDecodeInfoKHR,
) -> VideoResult<()> {
    let result = (|| {
        if info.flags.0 != 0 || info.reference_slot_count as usize != info.p_reference_slots.len() {
            return Err(unsupported());
        }
        let [p::PNextLink::VkVideoDecodeH264PictureInfoKHR(h264)] = info.p_next.as_slice() else {
            return Err(unsupported());
        };
        if !h264.p_next.is_empty()
            || h264.slice_count == 0
            || h264.slice_count as usize != h264.p_slice_offsets.len()
        {
            return Err(unsupported());
        }
        let metadata = picture_metadata(h264.p_std_picture_info.as_deref().ok_or_else(bad)?)?;
        let session = video.scope_session(command)?;
        let refs: Vec<Reference> = info
            .p_reference_slots
            .iter()
            .map(reference)
            .collect::<VideoResult<_>>()?;
        let setup = info
            .p_setup_reference_slot
            .as_deref()
            .map(reference)
            .transpose()?;
        let bound = video.scope_bound_pictures(command)?;
        if refs
            .iter()
            .chain(setup.iter())
            .any(|r| !bound.contains(&r.picture))
        {
            return Err(unsupported());
        }
        let sps = metadata.sps_id;
        let pps = metadata.pps_id;
        let job = Decode {
            session,
            configuration: video.configuration(command, sps, pps)?,
            source_buffer: info.src_buffer.0,
            source_offset: info.src_buffer_offset,
            source_range: info.src_buffer_range,
            slice_offsets: h264.p_slice_offsets.clone().into(),
            destination: picture(&info.dst_picture_resource)?,
            setup,
            references: refs.into(),
            h264: metadata,
            begin_before: false,
            reset_before: false,
            begin_references: Arc::from([]),
        };
        video.record(command, job, sps, pps)?;
        plans
            .decode(command, video.last_decode(command)?)
            .map_err(|_| bad())?;
        Ok(())
    })();
    if result.is_err() {
        reject(video, plans, command)
    }
    result
}
pub fn end(
    video: &mut NativeVideo,
    plans: &mut Plans,
    command: u64,
    info: &p::VkVideoEndCodingInfoKHR,
) -> VideoResult<()> {
    let result = (|| {
        if !info.p_next.is_empty() || info.flags.0 != 0 {
            return Err(unsupported());
        }
        video.end(command)?;
        plans.end_scope(command).map_err(|_| bad())?;
        Ok(())
    })();
    if result.is_err() {
        reject(video, plans, command)
    }
    result
}

#[cfg(target_os = "macos")]
mod host {
    use super::*;
    use crate::{native_video_dispatch::DispatchState, server::GpuServer};
    use sv_gpu_entry::GpuHost;
    fn active(state: &DispatchState, command: u64) -> bool {
        state.video.scope_active(command)
            || state.plans.is_video(command)
            || state.plans.poisoned(command).is_some()
    }
    impl GpuServer {
        fn native_recording_answer(
            &mut self,
            command: u64,
            result: VideoResult<()>,
        ) -> sv_wire::Result<()> {
            if result.is_err() {
                let actual = self.map_handle(p::HandleType::VkCommandBuffer, command)?;
                self.poison_command_buffer(
                    actual,
                    "invalid or unsupported synthetic video coding command",
                );
            }
            Ok(())
        }
        pub(crate) fn native_cmd_begin_video_coding_khr(
            &mut self,
            a: &p::VkCmdBeginVideoCodingKHRArgs,
        ) -> Option<sv_wire::Result<()>> {
            let mut state = self.shared.native_video.lock().unwrap();
            let info = a.p_begin_info.as_deref()?;
            if !state.owns_session(info.video_session.0) && !active(&state, a.command_buffer.0) {
                return None;
            }
            let result = (|| {
                if !state.available() {
                    return Err(unsupported());
                }
                let session = state.session(info.video_session.0).ok_or_else(bad)?;
                let parameters = state
                    .parameters(info.video_session_parameters.0)
                    .ok_or_else(bad)?;
                let DispatchState { video, plans, .. } = &mut *state;
                begin(video, plans, a.command_buffer.0, session, parameters, info)
            })();
            if result.is_err() {
                let DispatchState { video, plans, .. } = &mut *state;
                reject(video, plans, a.command_buffer.0)
            }
            drop(state);
            Some(self.native_recording_answer(a.command_buffer.0, result))
        }
        pub(crate) fn native_cmd_control_video_coding_khr(
            &mut self,
            a: &p::VkCmdControlVideoCodingKHRArgs,
        ) -> Option<sv_wire::Result<()>> {
            let mut state = self.shared.native_video.lock().unwrap();
            if !active(&state, a.command_buffer.0) {
                return None;
            }
            let result = if !state.available() {
                Err(unsupported())
            } else {
                match a.p_coding_control_info.as_deref() {
                    Some(info) => {
                        let DispatchState { video, plans, .. } = &mut *state;
                        control(video, plans, a.command_buffer.0, info)
                    }
                    None => Err(bad()),
                }
            };
            if result.is_err() {
                let DispatchState { video, plans, .. } = &mut *state;
                reject(video, plans, a.command_buffer.0)
            }
            drop(state);
            Some(self.native_recording_answer(a.command_buffer.0, result))
        }
        pub(crate) fn native_cmd_decode_video_khr(
            &mut self,
            a: &p::VkCmdDecodeVideoKHRArgs,
        ) -> Option<sv_wire::Result<()>> {
            let mut state = self.shared.native_video.lock().unwrap();
            if !active(&state, a.command_buffer.0) {
                return None;
            }
            let result = if !state.available() {
                Err(unsupported())
            } else {
                match a.p_decode_info.as_deref() {
                    Some(info) => {
                        let DispatchState { video, plans, .. } = &mut *state;
                        decode(video, plans, a.command_buffer.0, info)
                    }
                    None => Err(bad()),
                }
            };
            if result.is_err() {
                let DispatchState { video, plans, .. } = &mut *state;
                reject(video, plans, a.command_buffer.0)
            }
            drop(state);
            Some(self.native_recording_answer(a.command_buffer.0, result))
        }
        pub(crate) fn native_cmd_end_video_coding_khr(
            &mut self,
            a: &p::VkCmdEndVideoCodingKHRArgs,
        ) -> Option<sv_wire::Result<()>> {
            let mut state = self.shared.native_video.lock().unwrap();
            if !active(&state, a.command_buffer.0) {
                return None;
            }
            let result = if !state.available() {
                Err(unsupported())
            } else {
                match a.p_end_coding_info.as_deref() {
                    Some(info) => {
                        let DispatchState { video, plans, .. } = &mut *state;
                        end(video, plans, a.command_buffer.0, info)
                    }
                    None => Err(bad()),
                }
            };
            if result.is_err() {
                let DispatchState { video, plans, .. } = &mut *state;
                reject(video, plans, a.command_buffer.0)
            }
            drop(state);
            Some(self.native_recording_answer(a.command_buffer.0, result))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        native_video::{Profile, Ready},
        video_recording::{Barrier, End, Step},
    };
    fn fixture() -> (NativeVideo, Plans, u64, u64) {
        let mut video = NativeVideo::new(Ready {
            hardware_h264: true,
            joined_nv12_import: true,
            native_queue_bound_decode: true,
        });
        let info = vk::VideoSessionCreateInfoKHR::default()
            .picture_format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .reference_picture_format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .max_coded_extent(vk::Extent2D {
                width: 32,
                height: 32,
            })
            .max_dpb_slots(4)
            .max_active_reference_pictures(4);
        let session = video.create_session(Profile { idc: 100 }, &info).unwrap();
        let parameters = video.create_parameters(session, None, 1, 1).unwrap();
        let mut s: vk::native::StdVideoH264SequenceParameterSet = unsafe { std::mem::zeroed() };
        s.profile_idc = 66;
        s.level_idc = 7;
        s.chroma_format_idc = 1;
        s.pic_order_cnt_type = 2;
        s.max_num_ref_frames = 1;
        s.pic_width_in_mbs_minus1 = 1;
        s.pic_height_in_map_units_minus1 = 1;
        s.flags.set_frame_mbs_only_flag(1);
        s.flags.set_direct_8x8_inference_flag(1);
        let p: vk::native::StdVideoH264PictureParameterSet = unsafe { std::mem::zeroed() };
        unsafe { video.initialize_parameters(parameters, &[s], &[p]) }.unwrap();
        let mut plans = Plans::default();
        plans.begin(7).unwrap();
        (video, plans, session, parameters)
    }
    fn wire_picture() -> p::VkVideoPictureResourceInfoKHR {
        p::VkVideoPictureResourceInfoKHR {
            coded_extent: p::VkExtent2D {
                width: 32,
                height: 32,
            },
            image_view_binding: p::VkImageView(91),
            ..Default::default()
        }
    }
    fn wire_slot(index: i32) -> p::VkVideoReferenceSlotInfoKHR {
        p::VkVideoReferenceSlotInfoKHR {
            slot_index: index,
            p_picture_resource: Some(Box::new(wire_picture())),
            p_next: vec![p::PNextLink::VkVideoDecodeH264DpbSlotInfoKHR(Box::new(
                p::VkVideoDecodeH264DpbSlotInfoKHR {
                    p_std_reference_info: Some(Box::new(
                        p::StdVideoDecodeH264ReferenceInfo::default(),
                    )),
                    ..Default::default()
                },
            ))],
        }
    }
    fn wire_begin() -> p::VkVideoBeginCodingInfoKHR {
        p::VkVideoBeginCodingInfoKHR {
            reference_slot_count: 1,
            p_reference_slots: vec![wire_slot(-1)],
            ..Default::default()
        }
    }
    fn wire_decode(reference: bool) -> p::VkVideoDecodeInfoKHR {
        let picture = p::StdVideoDecodeH264PictureInfo {
            flags: p::StdVideoDecodeH264PictureInfoFlags(if reference { 1 << 4 } else { 0 }),
            ..Default::default()
        };
        p::VkVideoDecodeInfoKHR {
            src_buffer: p::VkBuffer(81),
            src_buffer_offset: 13,
            src_buffer_range: 10,
            dst_picture_resource: wire_picture(),
            p_setup_reference_slot: Some(Box::new(wire_slot(0))),
            p_next: vec![p::PNextLink::VkVideoDecodeH264PictureInfoKHR(Box::new(
                p::VkVideoDecodeH264PictureInfoKHR {
                    p_std_picture_info: Some(Box::new(picture)),
                    slice_count: 1,
                    p_slice_offsets: vec![0],
                    ..Default::default()
                },
            ))],
            ..Default::default()
        }
    }
    #[test]
    fn stock_zink_placeholder_reset_and_owned_decode_keep_original_order_and_ids() {
        let (mut video, mut plans, session, parameters) = fixture();
        let barrier = Barrier::Legacy(p::VkCmdPipelineBarrierArgs {
            command_buffer: p::VkCommandBuffer(7),
            ..Default::default()
        });
        plans.barrier(barrier.clone()).unwrap();
        begin(
            &mut video,
            &mut plans,
            7,
            session,
            parameters,
            &wire_begin(),
        )
        .unwrap();
        control(
            &mut video,
            &mut plans,
            7,
            &p::VkVideoCodingControlInfoKHR {
                flags: p::VkVideoCodingControlFlagsKHR(
                    vk::VideoCodingControlFlagsKHR::RESET.as_raw(),
                ),
                ..Default::default()
            },
        )
        .unwrap();
        plans.barrier(barrier).unwrap();
        let mut input = wire_decode(true);
        decode(&mut video, &mut plans, 7, &input).unwrap();
        input.src_buffer = p::VkBuffer(999);
        input.dst_picture_resource.image_view_binding = p::VkImageView(999);
        end(
            &mut video,
            &mut plans,
            7,
            &p::VkVideoEndCodingInfoKHR::default(),
        )
        .unwrap();
        let End::Video(steps) = plans.end(7).unwrap() else {
            panic!()
        };
        assert_eq!(steps.len(), 3);
        let Step::Decode(job) = &steps[2] else {
            panic!()
        };
        assert_eq!(job.source_buffer, 81);
        assert_eq!(job.source_offset, 13);
        assert_eq!(job.destination.image_view, 91);
        assert!(job.begin_before);
        assert!(job.reset_before);
        assert!(job.begin_references.is_empty());
        assert_eq!(job.setup.as_ref().unwrap().slot, 0);
        // Upstream does not populate IdrPicFlag/is_intra. Preserve reported bits.
        assert!(!job.h264.idr_pic_flag);
        assert!(!job.h264.is_intra);
        assert!(job.h264.is_reference);
    }
    #[test]
    fn nonreference_setup_is_retained_for_explicit_slot_invalidation() {
        let (mut video, mut plans, s, p) = fixture();
        begin(&mut video, &mut plans, 7, s, p, &wire_begin()).unwrap();
        decode(&mut video, &mut plans, 7, &wire_decode(false)).unwrap();
        let job = video.last_decode(7).unwrap();
        assert!(!job.h264.is_reference);
        assert!(job.setup.is_some());
    }
    #[test]
    fn malformed_decode_poison_prevents_publishing_partial_recording() {
        let (mut video, mut plans, s, p) = fixture();
        begin(&mut video, &mut plans, 7, s, p, &wire_begin()).unwrap();
        let mut input = wire_decode(true);
        input.reference_slot_count = 1;
        assert!(decode(&mut video, &mut plans, 7, &input).is_err());
        assert!(plans.poisoned(7).is_some());
        assert!(!video.scope_active(7));
        assert!(plans.end(7).is_err());
    }
    #[test]
    fn external_setup_binding_and_interlaced_metadata_are_refused() {
        let (mut video, mut plans, s, p) = fixture();
        begin(&mut video, &mut plans, 7, s, p, &wire_begin()).unwrap();
        let mut input = wire_decode(true);
        input
            .p_setup_reference_slot
            .as_mut()
            .unwrap()
            .p_picture_resource
            .as_mut()
            .unwrap()
            .image_view_binding = p::VkImageView(92);
        assert!(decode(&mut video, &mut plans, 7, &input).is_err());
        let (mut video, mut plans, s, p) = fixture();
        begin(&mut video, &mut plans, 7, s, p, &wire_begin()).unwrap();
        let mut input = wire_decode(true);
        let p::PNextLink::VkVideoDecodeH264PictureInfoKHR(h) = &mut input.p_next[0] else {
            panic!()
        };
        h.p_std_picture_info.as_mut().unwrap().flags.0 |= 1;
        assert!(decode(&mut video, &mut plans, 7, &input).is_err());
    }
}
