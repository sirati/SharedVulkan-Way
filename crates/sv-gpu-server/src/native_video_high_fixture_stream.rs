//! Original High100 payloads and owned exact StdVideo-representable metadata.
use super::stream::{Frame, Metadata, Prior, Stream};
use ash::vk;
use std::{collections::HashMap, ops::Range};
use sv_proto as p;
#[path = "native_video_dpb.rs"]
mod dpb;
#[cfg(test)]
#[path = "video_nv12/h264_parameters.rs"]
mod encoder;
#[path = "native_video_h264.rs"]
mod h264;
#[path = "native_video_lists.rs"]
mod lists;
#[path = "native_video_poc.rs"]
mod poc;
#[derive(Clone, Copy, Debug)]
pub(crate) enum HighCase {
    CavlcPcm,
    Cabac,
}
impl HighCase {
    pub(crate) fn oracle(self) -> &'static [u8] {
        match self {
            Self::CavlcPcm => include_bytes!(
                "../examples/assets/videotoolbox-h264/original-high-cavlc-pcm-64x64.yuv"
            ),
            Self::Cabac => {
                include_bytes!("../examples/assets/videotoolbox-h264/original-high-cabac-64x64.yuv")
            }
        }
    }
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::CavlcPcm => include_bytes!(
                "../examples/assets/videotoolbox-h264/original-high-cavlc-pcm-64x64.h264"
            ),
            Self::Cabac => include_bytes!(
                "../examples/assets/videotoolbox-h264/original-high-cabac-64x64.h264"
            ),
        }
    }
    pub(crate) fn parameter_sets(
        self,
    ) -> Result<p::VkVideoDecodeH264SessionParametersAddInfoKHR, String> {
        let stream = high_original(self)?;
        let sps = parse_sps(&stream.bytes[stream.sps])?;
        let pps = parse_pps(&stream.bytes[stream.pps])?;
        Ok(p::VkVideoDecodeH264SessionParametersAddInfoKHR {
            std_sps_count: 1,
            p_std_sp_ss: vec![sps],
            std_pps_count: 1,
            p_std_pp_ss: vec![pps],
            ..Default::default()
        })
    }
}
fn bits<const N: usize>(v: &vk::native::__BindgenBitfieldUnit<[u8; N]>) -> u32 {
    assert!(N > 0 && N <= 4);
    v.get(0, (N * 8) as u8) as u32
}
struct Reader {
    data: Vec<u8>,
    at: usize,
    omitted: Vec<Range<usize>>,
}
impl Reader {
    fn new(nal: &[u8]) -> Self {
        let mut data = Vec::new();
        let mut zeros = 0;
        for &v in &nal[1..] {
            if zeros == 2 && v == 3 {
                zeros = 0;
                continue;
            }
            data.push(v);
            zeros = if v == 0 { zeros + 1 } else { 0 };
        }
        Self {
            data,
            at: 0,
            omitted: Vec::new(),
        }
    }
    fn read(&mut self, n: usize) -> Result<u32, String> {
        if n > 32 || self.at + n > self.data.len() * 8 {
            return Err("truncated High fixture syntax".into());
        }
        let mut value = 0;
        for _ in 0..n {
            value = (value << 1) | u32::from((self.data[self.at / 8] >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Ok(value)
    }
    fn ue(&mut self) -> Result<u32, String> {
        let mut n = 0;
        while self.read(1)? == 0 {
            n += 1;
            if n > 30 {
                return Err("High fixture Golomb overflow".into());
            }
        }
        Ok(((1u32 << n) - 1) + self.read(n)?)
    }
    fn se(&mut self) -> Result<i32, String> {
        let v = self.ue()?;
        Ok(if v & 1 != 0 {
            ((v + 1) / 2) as i32
        } else {
            -(v as i32 / 2)
        })
    }
    fn more(&self) -> bool {
        let tail = (self.at..self.data.len() * 8)
            .map(|i| (self.data[i / 8] >> (7 - i % 8)) & 1)
            .collect::<Vec<_>>();
        !tail.is_empty() && !(tail[0] == 1 && tail[1..].iter().all(|&x| x == 0))
    }
}
fn parse_sps(nal: &[u8]) -> Result<p::StdVideoH264SequenceParameterSet, String> {
    Ok(parse_sps_details(nal)?.0)
}
fn parse_sps_details(nal: &[u8]) -> Result<(p::StdVideoH264SequenceParameterSet, Vec<u8>), String> {
    let mut b = Reader::new(nal);
    let mut f: vk::native::StdVideoH264SpsFlags = unsafe { std::mem::zeroed() };
    let profile = b.read(8)?;
    if profile != 100 {
        return Err("High fixture profile is not100".into());
    }
    f.set_constraint_set0_flag(b.read(1)?);
    f.set_constraint_set1_flag(b.read(1)?);
    f.set_constraint_set2_flag(b.read(1)?);
    f.set_constraint_set3_flag(b.read(1)?);
    f.set_constraint_set4_flag(b.read(1)?);
    f.set_constraint_set5_flag(b.read(1)?);
    if b.read(2)? != 0 {
        return Err("reserved SPS flags".into());
    }
    let rawlevel = b.read(8)?;
    let level = [
        10, 11, 12, 13, 20, 21, 22, 30, 31, 32, 40, 41, 42, 50, 51, 52,
    ]
    .iter()
    .position(|&x| x == rawlevel)
    .ok_or("unsupported fixture level")?;
    let mut s = p::StdVideoH264SequenceParameterSet {
        profile_idc: p::StdVideoH264ProfileIdc(100),
        level_idc: p::StdVideoH264LevelIdc(level as i32),
        seq_parameter_set_id: b.ue()? as u8,
        ..Default::default()
    };
    s.chroma_format_idc = p::StdVideoH264ChromaFormatIdc(b.ue()? as i32);
    if s.chroma_format_idc.0 != 1 {
        return Err("High fixture not420".into());
    }
    s.bit_depth_luma_minus8 = b.ue()? as u8;
    s.bit_depth_chroma_minus8 = b.ue()? as u8;
    if s.bit_depth_luma_minus8 != 0 || s.bit_depth_chroma_minus8 != 0 {
        return Err("High fixture not8bit".into());
    }
    f.set_qpprime_y_zero_transform_bypass_flag(b.read(1)?);
    let scaling = b.read(1)?;
    if scaling != 0 {
        return Err("fixture SPS scaling lists unsupported".into());
    }
    s.log2_max_frame_num_minus4 = b.ue()? as u8;
    s.pic_order_cnt_type = p::StdVideoH264PocType(b.ue()? as i32);
    match s.pic_order_cnt_type.0 {
        0 => s.log2_max_pic_order_cnt_lsb_minus4 = b.ue()? as u8,
        2 => {}
        _ => return Err("fixture expects POC0or2".into()),
    }
    s.max_num_ref_frames = b.ue()? as u8;
    f.set_gaps_in_frame_num_value_allowed_flag(b.read(1)?);
    s.pic_width_in_mbs_minus1 = b.ue()?;
    s.pic_height_in_map_units_minus1 = b.ue()?;
    let frame = b.read(1)?;
    if frame != 1 {
        return Err("interlaced High fixture".into());
    }
    f.set_frame_mbs_only_flag(frame);
    f.set_direct_8x8_inference_flag(b.read(1)?);
    let crop = b.read(1)?;
    f.set_frame_cropping_flag(crop);
    if crop != 0 {
        s.frame_crop_left_offset = b.ue()?;
        s.frame_crop_right_offset = b.ue()?;
        s.frame_crop_top_offset = b.ue()?;
        s.frame_crop_bottom_offset = b.ue()?;
    }
    let vui = b.read(1)?;
    f.set_vui_parameters_present_flag(vui);
    if vui != 0 {
        s.p_sequence_parameter_set_vui = Some(Box::new(parse_vui(&mut b)?));
    }
    s.flags = p::StdVideoH264SpsFlags(bits(&f._bitfield_1));
    let semantics = (0..b.at)
        .filter(|i| !b.omitted.iter().any(|range| range.contains(i)))
        .map(|i| (b.data[i / 8] >> (7 - i % 8)) & 1)
        .collect();
    if b.more() {
        return Err("unconsumed High SPS syntax".into());
    }
    Ok((s, semantics))
}
fn parse_vui(b: &mut Reader) -> Result<p::StdVideoH264SequenceParameterSetVui, String> {
    let mut v = p::StdVideoH264SequenceParameterSetVui::default();
    let mut f: vk::native::StdVideoH264SpsVuiFlags = unsafe { std::mem::zeroed() };
    let aspect = b.read(1)?;
    f.set_aspect_ratio_info_present_flag(aspect);
    if aspect != 0 {
        v.aspect_ratio_idc = p::StdVideoH264AspectRatioIdc(b.read(8)? as i32);
        if v.aspect_ratio_idc.0 == 255 {
            v.sar_width = b.read(16)? as u16;
            v.sar_height = b.read(16)? as u16;
        }
    }
    let over = b.read(1)?;
    f.set_overscan_info_present_flag(over);
    if over != 0 {
        f.set_overscan_appropriate_flag(b.read(1)?);
    }
    let signal = b.read(1)?;
    f.set_video_signal_type_present_flag(signal);
    if signal != 0 {
        v.video_format = b.read(3)? as u8;
        f.set_video_full_range_flag(b.read(1)?);
        let color = b.read(1)?;
        f.set_color_description_present_flag(color);
        if color != 0 {
            v.colour_primaries = b.read(8)? as u8;
            v.transfer_characteristics = b.read(8)? as u8;
            v.matrix_coefficients = b.read(8)? as u8;
        }
    }
    let loc = b.read(1)?;
    f.set_chroma_loc_info_present_flag(loc);
    if loc != 0 {
        v.chroma_sample_loc_type_top_field = b.ue()? as u8;
        v.chroma_sample_loc_type_bottom_field = b.ue()? as u8;
    }
    let timing = b.read(1)?;
    f.set_timing_info_present_flag(timing);
    if timing != 0 {
        v.num_units_in_tick = b.read(32)?;
        v.time_scale = b.read(32)?;
        f.set_fixed_frame_rate_flag(b.read(1)?);
    }
    if b.read(1)? != 0 || b.read(1)? != 0 {
        return Err("fixture HRD unsupported".into());
    }
    if b.read(1)? != 0 {
        return Err("fixture pic_struct syntax not representable".into());
    }
    let restriction = b.read(1)?;
    f.set_bitstream_restriction_flag(restriction);
    if restriction != 0 {
        // These five values are absent from StdVideo. Retain original payload;
        // tests separately prove that only their canonical bounds change.
        let omitted_start = b.at;
        b.read(1)?;
        b.ue()?;
        b.ue()?;
        b.ue()?;
        b.ue()?;
        b.omitted.push(omitted_start..b.at);
        v.max_num_reorder_frames = b.ue()? as u8;
        v.max_dec_frame_buffering = b.ue()? as u8;
    }
    v.flags = p::StdVideoH264SpsVuiFlags(bits(&f._bitfield_1));
    Ok(v)
}
fn parse_pps(nal: &[u8]) -> Result<p::StdVideoH264PictureParameterSet, String> {
    let mut b = Reader::new(nal);
    let mut f: vk::native::StdVideoH264PpsFlags = unsafe { std::mem::zeroed() };
    let mut pps = p::StdVideoH264PictureParameterSet {
        pic_parameter_set_id: b.ue()? as u8,
        seq_parameter_set_id: b.ue()? as u8,
        ..Default::default()
    };
    f.set_entropy_coding_mode_flag(b.read(1)?);
    f.set_bottom_field_pic_order_in_frame_present_flag(b.read(1)?);
    if b.ue()? != 0 {
        return Err("fixture FMO unsupported".into());
    }
    pps.num_ref_idx_l0_default_active_minus1 = b.ue()? as u8;
    pps.num_ref_idx_l1_default_active_minus1 = b.ue()? as u8;
    f.set_weighted_pred_flag(b.read(1)?);
    pps.weighted_bipred_idc = p::StdVideoH264WeightedBipredIdc(b.read(2)? as i32);
    pps.pic_init_qp_minus26 = b.se()? as i8;
    pps.pic_init_qs_minus26 = b.se()? as i8;
    pps.chroma_qp_index_offset = b.se()? as i8;
    f.set_deblocking_filter_control_present_flag(b.read(1)?);
    f.set_constrained_intra_pred_flag(b.read(1)?);
    f.set_redundant_pic_cnt_present_flag(b.read(1)?);
    pps.second_chroma_qp_index_offset = pps.chroma_qp_index_offset;
    if b.more() {
        f.set_transform_8x8_mode_flag(b.read(1)?);
        if b.read(1)? != 0 {
            return Err("fixture PPS scaling unsupported".into());
        }
        pps.second_chroma_qp_index_offset = b.se()? as i8;
    }
    pps.flags = p::StdVideoH264PpsFlags(bits(&f._bitfield_1));
    Ok(pps)
}
fn ranges(bytes: &[u8]) -> Vec<(Range<usize>, Range<usize>)> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 3 < bytes.len() {
        let prefix = if bytes[at..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[at..].starts_with(&[0, 0, 1]) {
            3
        } else {
            at += 1;
            continue;
        };
        starts.push((at, at + prefix));
        at += prefix;
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, &(start, payload))| {
            let end = starts.get(i + 1).map_or(bytes.len(), |v| v.0);
            (start..end, payload..end)
        })
        .collect()
}
fn metadata(entry: &dpb::Entry) -> Result<Metadata, String> {
    Ok(Metadata {
        frame_num: u16::try_from(entry.long_term.unwrap_or(entry.frame_num))
            .map_err(|_| "High fixture frame overflow")?,
        poc: entry.poc,
        long_term: entry.long_term.is_some(),
    })
}
pub(crate) fn high_original(case: HighCase) -> Result<Stream, String> {
    let bytes = case.bytes();
    let mut sps = None;
    let mut pps = None;
    let mut aus = Vec::new();
    for (range, payload) in ranges(bytes) {
        match bytes[payload.start] & 31 {
            7 => {
                if sps.is_some() {
                    return Err("repeated fixture SPS".into());
                }
                sps = Some(payload);
            }
            8 => {
                if pps.is_some() {
                    return Err("repeated fixture PPS".into());
                }
                pps = Some(payload);
            }
            1 | 5 => aus.push(range),
            _ => {}
        }
    }
    let sps = sps.ok_or("missing High SPS")?;
    let pps = pps.ok_or("missing High PPS")?;
    let count = match case {
        HighCase::CavlcPcm => 2,
        HighCase::Cabac => 3,
    };
    if aus.len() != count || case.oracle().len() != count * 6144 {
        return Err("High fixture frame count mismatch".into());
    }
    let mut history = poc::History::default();
    let mut codec = dpb::Dpb::default();
    let mut owners = HashMap::new();
    let mut frames = Vec::new();
    for (index, range) in aus.into_iter().enumerate() {
        let parsed = h264::parse(
            bytes,
            &range,
            &[0],
            &bytes[sps.clone()],
            &bytes[pps.clone()],
        )?;
        let order = history.plan(&parsed, false)?;
        let current = dpb::Picture {
            frame_num: parsed.frame_num,
            poc: order.poc,
        };
        let lists = codec.reference_lists(current, &parsed)?;
        let mut references = Vec::new();
        for generation in lists.iter().flat_map(|pair| pair.iter()).flatten() {
            let frame_index = *owners
                .get(generation)
                .ok_or("missing High reference producer")?;
            if references
                .iter()
                .any(|p: &Prior| p.frame_index == frame_index)
            {
                continue;
            }
            let entry = codec
                .entries
                .iter()
                .find(|entry| entry.generation == *generation)
                .ok_or("missing High DPB entry")?;
            references.push(Prior {
                frame_index,
                metadata: metadata(entry)?,
            });
        }
        let plan = codec.plan(
            current,
            &parsed.marking,
            dpb::Limits {
                max_frame_num: parsed.max_frame_num,
                max_refs: parsed.max_refs,
            },
        )?;
        let setup = plan.current.as_ref().map(metadata).transpose()?;
        let intra = parsed.slices.iter().all(|slice| slice.kind == 2);
        let expected_intra = matches!(case, HighCase::CavlcPcm) || index == 0;
        let expected_prior = if expected_intra {
            vec![]
        } else {
            vec![index - 1]
        };
        if parsed.frame_num != index as u32
            || order.poc != [index as i32 * 2; 2]
            || !parsed.reference
            || parsed.idr != (index == 0)
            || intra != expected_intra
            || parsed.sequence.coded_extent != [64, 64]
            || references.iter().map(|p| p.frame_index).collect::<Vec<_>>() != expected_prior
        {
            return Err(format!(
                "High original source disagrees with fixed frame oracle at {index}"
            ));
        }
        let expected_y = if index == 0 { 40 } else { 120 };
        let oracle = &case.oracle()[index * 6144..(index + 1) * 6144];
        let uniform = oracle[..4096].iter().all(|&v| v == expected_y)
            && oracle[4096..5120].iter().all(|&v| v == 80)
            && oracle[5120..].iter().all(|&v| v == 180);
        if !uniform {
            return Err(format!("High frame {index} requires per-pixel GPU oracle"));
        }
        if let Some(entry) = &plan.current {
            owners.insert(entry.generation, index);
        }
        owners.retain(|generation, _| {
            plan.next
                .entries
                .iter()
                .any(|entry| entry.generation == *generation)
        });
        frames.push(Frame {
            range,
            slice_offsets: vec![0],
            frame_num: parsed.frame_num as u16,
            poc: order.poc,
            is_reference: parsed.reference,
            is_intra: intra,
            idr: parsed.idr,
            idr_pic_id: parsed.idr_pic_id,
            setup,
            references,
            expected_y: Some(expected_y),
        });
        history = order.next;
        codec = plan.next;
    }
    Ok(Stream {
        bytes,
        sps,
        pps,
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn high_original_frames_match_independent_software_oracles() {
        for case in [HighCase::CavlcPcm, HighCase::Cabac] {
            let stream = high_original(case).unwrap();
            assert_eq!(stream.bytes[stream.sps.start + 1], 100);
            assert_eq!(
                stream.frames.len(),
                if matches!(case, HighCase::Cabac) {
                    3
                } else {
                    2
                }
            );
            assert!(stream.frames[0].references.is_empty());
            if matches!(case, HighCase::Cabac) {
                assert_eq!(stream.frames[2].references[0].frame_index, 1);
            }
        }
    }
    #[test]
    fn owned_std_metadata_roundtrips_exact_pps_and_representable_sps() {
        for case in [HighCase::CavlcPcm, HighCase::Cabac] {
            let original = high_original(case).unwrap();
            let sets = case.parameter_sets().unwrap();
            let mut pool = sv_gpu_entry::Raise::default();
            let native = sv_gpu_entry::raise_vk_video_decode_h264_session_parameters_add_info_khr(
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
                &*native
                    .p_std_sp_ss
                    .cast::<vk::native::StdVideoH264SequenceParameterSet>()
            };
            let pps = unsafe {
                &*native
                    .p_std_pp_ss
                    .cast::<vk::native::StdVideoH264PictureParameterSet>()
            };
            let encoded = unsafe { encoder::sps_nal(sps) }.unwrap();
            assert_eq!(
                unsafe { encoder::pps_nal(pps, sps) }.unwrap(),
                &original.bytes[original.pps.clone()]
            );
            // Compare original and canonical raw SPS syntax with only the
            // explicitly Std-omitted VUI restriction bounds excluded.
            assert_eq!(
                parse_sps_details(&encoded).unwrap().1,
                parse_sps_details(&original.bytes[original.sps.clone()])
                    .unwrap()
                    .1
            );
            let reparsed = parse_sps(&encoded).unwrap();
            assert_eq!(reparsed, sets.p_std_sp_ss[0]);
            if matches!(case, HighCase::CavlcPcm) {
                assert_eq!(encoded, &original.bytes[original.sps]);
            } else {
                // Original and reconstructed SPS agree in every representable
                // field. Only Std-omitted VUI resource bounds may differ.
                assert_eq!(sets.p_std_sp_ss[0].profile_idc.0, 100);
                assert_eq!(pps.flags.entropy_coding_mode_flag(), 1);
                assert_eq!(pps.flags.transform_8x8_mode_flag(), 1);
            }
        }
    }
}
