//! Convert Vulkan StdVideo parameter sets to H.264 NAL units. Only metadata is
//! encoded here; compressed slices remain in their original shared allocation.
use ash::vk::native::{
    StdVideoH264PictureParameterSet as Pps, StdVideoH264ScalingLists as Scaling,
    StdVideoH264SequenceParameterSet as Sps, StdVideoH264SequenceParameterSetVui as Vui,
};
struct Writer {
    bytes: Vec<u8>,
    bit: u8,
}
impl Writer {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bit: 0,
        }
    }
    fn bits(&mut self, value: u32, count: u8) {
        for shift in (0..count).rev() {
            if self.bit == 0 {
                self.bytes.push(0);
            }
            *self.bytes.last_mut().unwrap() |= (((value >> shift) & 1) as u8) << (7 - self.bit);
            self.bit = (self.bit + 1) % 8;
        }
    }
    fn flag(&mut self, value: u32) {
        self.bits(value, 1);
    }
    fn ue(&mut self, value: u32) {
        let code = u64::from(value) + 1;
        let width = 64 - code.leading_zeros();
        for _ in 1..width {
            self.flag(0);
        }
        for shift in (0..width).rev() {
            self.flag(((code >> shift) & 1) as u32);
        }
    }
    fn se(&mut self, value: i32) -> Result<(), String> {
        let code = if value <= 0 {
            -i64::from(value) * 2
        } else {
            i64::from(value) * 2 - 1
        };
        self.ue(u32::try_from(code).map_err(|_| "signed H.264 value too large")?);
        Ok(())
    }
    fn nal(mut self, header: u8) -> Vec<u8> {
        self.flag(1);
        while self.bit != 0 {
            self.flag(0);
        }
        let mut result = vec![header];
        let mut zeros = 0;
        for byte in self.bytes {
            if zeros == 2 && byte <= 3 {
                result.push(3);
                zeros = 0;
            }
            result.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        result
    }
}
fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
unsafe fn scaling(w: &mut Writer, pointer: *const Scaling, count: usize) -> Result<(), String> {
    let lists = unsafe { pointer.as_ref() }.ok_or("scaling matrix flag has no scaling lists")?;
    let mask = (1u16 << count) - 1;
    require(
        lists.scaling_list_present_mask & !mask == 0
            && lists.use_default_scaling_matrix_mask & !lists.scaling_list_present_mask == 0,
        "invalid H.264 scaling masks",
    )?;
    for i in 0..count {
        let present = (lists.scaling_list_present_mask >> i) & 1;
        w.flag(u32::from(present));
        if present == 0 {
            continue;
        }
        if lists.use_default_scaling_matrix_mask & (1 << i) != 0 {
            w.se(-8)?;
            continue;
        }
        let list: &[u8] = if i < 6 {
            &lists.ScalingList4x4[i]
        } else {
            &lists.ScalingList8x8[i - 6]
        };
        let side = if i < 6 { 4 } else { 8 };
        let mut last = 8i32;
        // StdVideo stores scaling matrices in raster order. H.264 sends zigzag.
        for diagonal in 0..(2 * side - 1) {
            let first = if diagonal < side {
                0
            } else {
                diagonal + 1 - side
            };
            let last_row = diagonal.min(side - 1);
            let rows: Box<dyn Iterator<Item = usize>> = if diagonal % 2 == 0 {
                Box::new((first..=last_row).rev())
            } else {
                Box::new(first..=last_row)
            };
            for row in rows {
                let next = i32::from(list[row * side + diagonal - row]);
                require(next != 0, "zero H.264 scaling coefficient")?;
                let delta = (next - last + 128).rem_euclid(256) - 128;
                w.se(delta)?;
                last = next;
            }
        }
    }
    Ok(())
}
unsafe fn vui(w: &mut Writer, pointer: *const Vui) -> Result<(), String> {
    let v = unsafe { pointer.as_ref() }.ok_or("VUI flag has no VUI data")?;
    let f = &v.flags;
    require(
        f.nal_hrd_parameters_present_flag() == 0
            && f.vcl_hrd_parameters_present_flag() == 0
            && f.bitstream_restriction_flag() == 0,
        "StdVideo cannot represent all H.264 HRD/restriction syntax; these VUI modes are unsupported",
    )?;
    w.flag(f.aspect_ratio_info_present_flag());
    if f.aspect_ratio_info_present_flag() != 0 {
        require(
            v.aspect_ratio_idc <= 16 || v.aspect_ratio_idc == 255,
            "invalid H.264 aspect ratio",
        )?;
        w.bits(v.aspect_ratio_idc, 8);
        if v.aspect_ratio_idc == 255 {
            require(v.sar_width != 0 && v.sar_height != 0, "empty extended SAR")?;
            w.bits(u32::from(v.sar_width), 16);
            w.bits(u32::from(v.sar_height), 16);
        }
    }
    w.flag(f.overscan_info_present_flag());
    if f.overscan_info_present_flag() != 0 {
        w.flag(f.overscan_appropriate_flag());
    }
    w.flag(f.video_signal_type_present_flag());
    if f.video_signal_type_present_flag() != 0 {
        require(v.video_format <= 5, "invalid H.264 video format")?;
        w.bits(u32::from(v.video_format), 3);
        w.flag(f.video_full_range_flag());
        w.flag(f.color_description_present_flag());
        if f.color_description_present_flag() != 0 {
            w.bits(u32::from(v.colour_primaries), 8);
            w.bits(u32::from(v.transfer_characteristics), 8);
            w.bits(u32::from(v.matrix_coefficients), 8);
        }
    }
    w.flag(f.chroma_loc_info_present_flag());
    if f.chroma_loc_info_present_flag() != 0 {
        require(
            v.chroma_sample_loc_type_top_field <= 5 && v.chroma_sample_loc_type_bottom_field <= 5,
            "invalid H.264 chroma location",
        )?;
        w.ue(u32::from(v.chroma_sample_loc_type_top_field));
        w.ue(u32::from(v.chroma_sample_loc_type_bottom_field));
    }
    w.flag(f.timing_info_present_flag());
    if f.timing_info_present_flag() != 0 {
        require(
            v.num_units_in_tick != 0 && v.time_scale != 0,
            "invalid H.264 VUI timing",
        )?;
        w.bits(v.num_units_in_tick, 32);
        w.bits(v.time_scale, 32);
        w.flag(f.fixed_frame_rate_flag());
    }
    w.flag(0);
    w.flag(0); // nal_hrd_parameters_present, vcl_hrd_parameters_present
    w.flag(0); // pic_struct_present_flag has no StdVideo field, use absent.
    w.flag(0); // bitstream_restriction_flag
    Ok(())
}
/// # Safety
/// Every non-null pointer selected by an SPS flag must refer to its full valid
/// object or array for this call. The returned NAL owns all encoded metadata.
pub unsafe fn sps_nal(s: &Sps) -> Result<Vec<u8>, String> {
    let f = &s.flags;
    require(
        matches!(s.profile_idc, 66 | 77 | 100)
            && s.chroma_format_idc == 1
            && s.bit_depth_luma_minus8 == 0
            && s.bit_depth_chroma_minus8 == 0,
        "requires Baseline/Main/High 8-bit 4:2:0",
    )?;
    require(
        f.frame_mbs_only_flag() == 1
            && f.mb_adaptive_frame_field_flag() == 0
            && f.separate_colour_plane_flag() == 0,
        "interlaced/separate-plane H.264 unsupported",
    )?;
    require(
        s.seq_parameter_set_id < 32
            && s.log2_max_frame_num_minus4 <= 12
            && s.max_num_ref_frames <= 16
            && s.pic_width_in_mbs_minus1 < 512
            && s.pic_height_in_map_units_minus1 < 512,
        "invalid H.264 SPS limits",
    )?;
    require(
        s.profile_idc == 100
            || (f.seq_scaling_matrix_present_flag() == 0
                && f.qpprime_y_zero_transform_bypass_flag() == 0),
        "high-profile syntax in Baseline/Main SPS",
    )?;
    let levels = [
        10, 11, 12, 13, 20, 21, 22, 30, 31, 32, 40, 41, 42, 50, 51, 52, 60, 61, 62,
    ];
    let level = *levels
        .get(s.level_idc as usize)
        .ok_or("invalid StdVideo H.264 level")?;
    let mut w = Writer::new();
    w.bits(s.profile_idc, 8);
    for flag in [
        f.constraint_set0_flag(),
        f.constraint_set1_flag(),
        f.constraint_set2_flag(),
        f.constraint_set3_flag(),
        f.constraint_set4_flag(),
        f.constraint_set5_flag(),
    ] {
        w.flag(flag);
    }
    w.bits(0, 2);
    w.bits(level, 8);
    w.ue(u32::from(s.seq_parameter_set_id));
    if s.profile_idc == 100 {
        w.ue(1);
        w.ue(0);
        w.ue(0);
        w.flag(f.qpprime_y_zero_transform_bypass_flag());
        w.flag(f.seq_scaling_matrix_present_flag());
        if f.seq_scaling_matrix_present_flag() != 0 {
            unsafe {
                scaling(&mut w, s.pScalingLists, 8)?;
            }
        }
    }
    w.ue(u32::from(s.log2_max_frame_num_minus4));
    require(s.pic_order_cnt_type <= 2, "invalid H.264 POC type")?;
    w.ue(s.pic_order_cnt_type);
    match s.pic_order_cnt_type {
        0 => {
            require(
                s.log2_max_pic_order_cnt_lsb_minus4 <= 12,
                "invalid H.264 POC width",
            )?;
            w.ue(u32::from(s.log2_max_pic_order_cnt_lsb_minus4));
        }
        1 => {
            w.flag(f.delta_pic_order_always_zero_flag());
            w.se(s.offset_for_non_ref_pic)?;
            w.se(s.offset_for_top_to_bottom_field)?;
            let count = usize::from(s.num_ref_frames_in_pic_order_cnt_cycle);
            w.ue(count as u32);
            if count != 0 {
                require(!s.pOffsetForRefFrame.is_null(), "missing SPS POC offsets")?;
                for &offset in unsafe { std::slice::from_raw_parts(s.pOffsetForRefFrame, count) } {
                    w.se(offset)?;
                }
            }
        }
        _ => {}
    }
    w.ue(u32::from(s.max_num_ref_frames));
    w.flag(f.gaps_in_frame_num_value_allowed_flag());
    w.ue(s.pic_width_in_mbs_minus1);
    w.ue(s.pic_height_in_map_units_minus1);
    w.flag(1);
    w.flag(f.direct_8x8_inference_flag());
    w.flag(f.frame_cropping_flag());
    if f.frame_cropping_flag() != 0 {
        let width = (s.pic_width_in_mbs_minus1 + 1) * 16;
        let height = (s.pic_height_in_map_units_minus1 + 1) * 16;
        require(
            (u64::from(s.frame_crop_left_offset) + u64::from(s.frame_crop_right_offset)) * 2
                < u64::from(width)
                && (u64::from(s.frame_crop_top_offset) + u64::from(s.frame_crop_bottom_offset)) * 2
                    < u64::from(height),
            "invalid H.264 crop rectangle",
        )?;
        for value in [
            s.frame_crop_left_offset,
            s.frame_crop_right_offset,
            s.frame_crop_top_offset,
            s.frame_crop_bottom_offset,
        ] {
            w.ue(value);
        }
    }
    w.flag(f.vui_parameters_present_flag());
    if f.vui_parameters_present_flag() != 0 {
        unsafe {
            vui(&mut w, s.pSequenceParameterSetVui)?;
        }
    }
    Ok(w.nal(0x67))
}
/// # Safety
/// The PPS scaling-list pointer selected by its flag must be valid for this call.
pub unsafe fn pps_nal(p: &Pps, s: &Sps) -> Result<Vec<u8>, String> {
    require(
        p.seq_parameter_set_id == s.seq_parameter_set_id,
        "PPS refers to another SPS",
    )?;
    require(
        p.num_ref_idx_l0_default_active_minus1 <= 31
            && p.num_ref_idx_l1_default_active_minus1 <= 31
            && p.weighted_bipred_idc <= 2
            && (-26..=25).contains(&p.pic_init_qp_minus26)
            && (-26..=25).contains(&p.pic_init_qs_minus26)
            && (-12..=12).contains(&p.chroma_qp_index_offset)
            && (-12..=12).contains(&p.second_chroma_qp_index_offset),
        "invalid H.264 PPS fields",
    )?;
    let f = &p.flags;
    let extension = f.transform_8x8_mode_flag() != 0
        || f.pic_scaling_matrix_present_flag() != 0
        || p.second_chroma_qp_index_offset != p.chroma_qp_index_offset;
    require(
        !extension || s.profile_idc == 100,
        "high-profile PPS syntax in Baseline/Main",
    )?;
    require(
        s.profile_idc != 66 || f.entropy_coding_mode_flag() == 0,
        "CABAC in Baseline PPS",
    )?;
    let mut w = Writer::new();
    w.ue(u32::from(p.pic_parameter_set_id));
    w.ue(u32::from(p.seq_parameter_set_id));
    w.flag(f.entropy_coding_mode_flag());
    w.flag(f.bottom_field_pic_order_in_frame_present_flag());
    w.ue(0); // StdVideo has no FMO representation.
    w.ue(u32::from(p.num_ref_idx_l0_default_active_minus1));
    w.ue(u32::from(p.num_ref_idx_l1_default_active_minus1));
    w.flag(f.weighted_pred_flag());
    w.bits(p.weighted_bipred_idc, 2);
    w.se(i32::from(p.pic_init_qp_minus26))?;
    w.se(i32::from(p.pic_init_qs_minus26))?;
    w.se(i32::from(p.chroma_qp_index_offset))?;
    w.flag(f.deblocking_filter_control_present_flag());
    w.flag(f.constrained_intra_pred_flag());
    w.flag(f.redundant_pic_cnt_present_flag());
    if extension {
        w.flag(f.transform_8x8_mode_flag());
        w.flag(f.pic_scaling_matrix_present_flag());
        if f.pic_scaling_matrix_present_flag() != 0 {
            unsafe {
                scaling(
                    &mut w,
                    p.pScalingLists,
                    if f.transform_8x8_mode_flag() != 0 {
                        8
                    } else {
                        6
                    },
                )?;
            }
        }
        w.se(i32::from(p.second_chroma_qp_index_offset))?;
    }
    Ok(w.nal(0x68))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn baseline() -> Sps {
        // Every field is an integer or pointer, so all-zero is a valid Rust value.
        let mut s: Sps = unsafe { std::mem::zeroed() };
        s.profile_idc = 66;
        s.level_idc = 7;
        s.chroma_format_idc = 1;
        s.pic_order_cnt_type = 2;
        s.max_num_ref_frames = 1;
        s.pic_width_in_mbs_minus1 = 1;
        s.pic_height_in_map_units_minus1 = 1;
        s.flags.set_frame_mbs_only_flag(1);
        s.flags.set_direct_8x8_inference_flag(1);
        s
    }
    #[test]
    fn encodes_baseline_syntax_and_trailing_bits() {
        assert_eq!(
            unsafe { sps_nal(&baseline()) }.unwrap(),
            [0x67, 0x42, 0, 0x1e, 0xda, 0x25, 0x90]
        );
    }
    #[test]
    fn refuses_missing_selected_objects_and_unsupported_vui() {
        let mut s = baseline();
        s.profile_idc = 100;
        s.flags.set_seq_scaling_matrix_present_flag(1);
        assert!(
            unsafe { sps_nal(&s) }
                .unwrap_err()
                .contains("scaling lists")
        );
        s.flags.set_seq_scaling_matrix_present_flag(0);
        s.flags.set_vui_parameters_present_flag(1);
        let mut v: Vui = unsafe { std::mem::zeroed() };
        v.flags.set_bitstream_restriction_flag(1);
        s.pSequenceParameterSetVui = &v;
        assert!(unsafe { sps_nal(&s) }.unwrap_err().contains("restriction"));
    }
    #[test]
    fn inserts_emulation_prevention_bytes() {
        let mut w = Writer::new();
        w.bits(0, 24);
        assert_eq!(w.nal(0x67), [0x67, 0, 0, 3, 0, 0x80]);
    }
    #[test]
    fn rejects_pps_for_wrong_sps() {
        let mut p: Pps = unsafe { std::mem::zeroed() };
        p.seq_parameter_set_id = 1;
        assert!(unsafe { pps_nal(&p, &baseline()) }.is_err());
    }
}
