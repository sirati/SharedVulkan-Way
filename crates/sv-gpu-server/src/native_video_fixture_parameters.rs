//! Canonical wire metadata shared by the native fixture and portable regression.
use ash::vk;
use sv_proto as p;
pub(crate) const CANONICAL_SPS: &[u8] = &[
    0x67, 0x42, 0xc0, 0x0a, 0xda, 0x10, 0x9f, 0x88, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0xc8, 0x3c,
    0x22, 0x11, 0xa8,
];
fn flags<const N: usize>(field: &vk::native::__BindgenBitfieldUnit<[u8; N]>) -> u32 {
    // Std structs have padding to four bytes, but their bitfield backing storage
    // is only one or two bytes. Reading padding through get() is out of bounds.
    assert!(N > 0 && N <= 4);
    field.get(0, (N * 8) as u8) as u32
}
pub(crate) fn parameter_sets() -> p::VkVideoDecodeH264SessionParametersAddInfoKHR {
    // These canonical Std fields were independently pixel-tested against the
    // original cropped 64x50 Baseline stream. Their SPS differs only in omitted
    // MV restriction bounds, which StdVideo cannot represent directly.
    let mut sps_flags: vk::native::StdVideoH264SpsFlags = unsafe { std::mem::zeroed() };
    sps_flags.set_constraint_set0_flag(1);
    sps_flags.set_constraint_set1_flag(1);
    sps_flags.set_frame_mbs_only_flag(1);
    sps_flags.set_direct_8x8_inference_flag(1);
    sps_flags.set_frame_cropping_flag(1);
    sps_flags.set_vui_parameters_present_flag(1);
    let mut vui_flags: vk::native::StdVideoH264SpsVuiFlags = unsafe { std::mem::zeroed() };
    vui_flags.set_timing_info_present_flag(1);
    vui_flags.set_bitstream_restriction_flag(1);
    let sps = p::StdVideoH264SequenceParameterSet {
        flags: p::StdVideoH264SpsFlags(flags(&sps_flags._bitfield_1)),
        profile_idc: p::StdVideoH264ProfileIdc(66),
        level_idc: p::StdVideoH264LevelIdc(0),
        chroma_format_idc: p::StdVideoH264ChromaFormatIdc(1),
        pic_order_cnt_type: p::StdVideoH264PocType(2),
        max_num_ref_frames: 1,
        pic_width_in_mbs_minus1: 3,
        pic_height_in_map_units_minus1: 3,
        frame_crop_bottom_offset: 7,
        p_sequence_parameter_set_vui: Some(Box::new(p::StdVideoH264SequenceParameterSetVui {
            flags: p::StdVideoH264SpsVuiFlags(flags(&vui_flags._bitfield_1)),
            num_units_in_tick: 1,
            time_scale: 50,
            max_dec_frame_buffering: 1,
            ..Default::default()
        })),
        ..Default::default()
    };
    let mut pps_flags: vk::native::StdVideoH264PpsFlags = unsafe { std::mem::zeroed() };
    pps_flags.set_deblocking_filter_control_present_flag(1);
    let pps = p::StdVideoH264PictureParameterSet {
        flags: p::StdVideoH264PpsFlags(flags(&pps_flags._bitfield_1)),
        pic_init_qp_minus26: -14,
        ..Default::default()
    };
    p::VkVideoDecodeH264SessionParametersAddInfoKHR {
        std_sps_count: 1,
        p_std_sp_ss: vec![sps],
        std_pps_count: 1,
        p_std_pp_ss: vec![pps],
        ..Default::default()
    }
}
pub(crate) fn idr_picture() -> p::StdVideoDecodeH264PictureInfo {
    let mut native: vk::native::StdVideoDecodeH264PictureInfoFlags = unsafe { std::mem::zeroed() };
    native.set_IdrPicFlag(1);
    native.set_is_reference(1);
    native.set_is_intra(1);
    p::StdVideoDecodeH264PictureInfo {
        flags: p::StdVideoDecodeH264PictureInfoFlags(flags(&native._bitfield_1)),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_wire_fixture_parameters_serialize_to_verified_nals() {
        let sets = parameter_sets();
        let mut pool = sv_gpu_entry::Raise::default();
        let raised = sv_gpu_entry::raise_vk_video_decode_h264_session_parameters_add_info_khr(
            &sets, &mut pool,
        )
        .unwrap();
        const {
            assert!(
                std::mem::size_of::<sv_gpu_entry::StdVideoH264SequenceParameterSet>()
                    == std::mem::size_of::<vk::native::StdVideoH264SequenceParameterSet>()
            );
            assert!(
                std::mem::size_of::<sv_gpu_entry::StdVideoH264PictureParameterSet>()
                    == std::mem::size_of::<vk::native::StdVideoH264PictureParameterSet>()
            );
        }
        let sps = unsafe {
            &*raised
                .p_std_sp_ss
                .cast::<vk::native::StdVideoH264SequenceParameterSet>()
        };
        let pps = unsafe {
            &*raised
                .p_std_pp_ss
                .cast::<vk::native::StdVideoH264PictureParameterSet>()
        };
        assert_eq!(
            unsafe { crate::video_nv12::h264_parameters::sps_nal(sps) }.unwrap(),
            CANONICAL_SPS
        );
        let original_pps = [0x68, 0xce, 0x03, 0xbc, 0x80];
        assert_eq!(
            unsafe { crate::video_nv12::h264_parameters::pps_nal(pps, sps) }.unwrap(),
            original_pps
        );
        let stream = include_bytes!("../examples/assets/videotoolbox-h264/progressive-64x50.h264");
        assert!(
            stream
                .windows(original_pps.len())
                .any(|nal| nal == original_pps)
        );
    }
    #[test]
    fn actual_wire_fixture_picture_flags_raise_without_reading_padding() {
        let picture = idr_picture();
        let mut pool = sv_gpu_entry::Raise::default();
        let raised =
            sv_gpu_entry::raise_std_video_decode_h264_picture_info(&picture, &mut pool).unwrap();
        const {
            assert!(
                std::mem::size_of::<sv_gpu_entry::StdVideoDecodeH264PictureInfo>()
                    == std::mem::size_of::<vk::native::StdVideoDecodeH264PictureInfo>()
            );
        }
        let native = unsafe {
            &*((&raised as *const sv_gpu_entry::StdVideoDecodeH264PictureInfo)
                .cast::<vk::native::StdVideoDecodeH264PictureInfo>())
        };
        assert_eq!(native.flags.IdrPicFlag(), 1);
        assert_eq!(native.flags.is_reference(), 1);
        assert_eq!(native.flags.is_intra(), 1);
        assert_eq!(native.flags.field_pic_flag(), 0);
        assert_eq!(native.flags.bottom_field_flag(), 0);
        assert_eq!(native.frame_num, 0);
        assert_eq!(native.PicOrderCnt, [0, 0]);
    }
}
