//! Unwired, portable multi-frame proof input. This preserves the checked-in
//! original bitstream and imports the production header/history/DPB models.
//! Source provenance: examples/assets/videotoolbox-h264/provenance.txt.
#[path = "native_video_dpb.rs"]
mod dpb;
#[path = "native_video_h264.rs"]
mod h264;
#[path = "native_video_lists.rs"]
mod lists;
#[cfg(test)]
#[path = "video_nv12/h264_parameters.rs"]
mod parameter_encoder;
#[path = "native_video_poc.rs"]
mod poc;
use std::{collections::HashMap, ops::Range};
pub(crate) const SHA256: &str = "51cdf8efcda0a076eee679c57765d055ee904f3f77a1275dbb63d6261e39dc99";
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub frame_num: u16,
    pub poc: [i32; 2],
    pub long_term: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Prior {
    pub frame_index: usize,
    pub metadata: Metadata,
}
pub(crate) struct Frame {
    /// Exact original framed VCL AU. This fixture has one VCL NAL per picture.
    pub range: Range<usize>,
    pub slice_offsets: Vec<u32>,
    pub frame_num: u16,
    pub poc: [i32; 2],
    pub is_reference: bool,
    pub is_intra: bool,
    pub idr: bool,
    pub idr_pic_id: Option<u32>,
    pub setup: Option<Metadata>,
    /// Union of actual per-slice lists, in first-use order.
    pub references: Vec<Prior>,
    /// Uniform source luma oracle for original PCM/skip cases; baseline uses its spatial pattern.
    pub expected_y: Option<u8>,
}
pub(crate) struct Stream {
    pub bytes: &'static [u8],
    /// NAL payload ranges including NAL headers, excluding Annex B prefixes.
    pub sps: Range<usize>,
    pub pps: Range<usize>,
    pub frames: Vec<Frame>,
}
fn metadata(entry: &dpb::Entry) -> Result<Metadata, String> {
    Ok(Metadata {
        frame_num: u16::try_from(entry.long_term.unwrap_or(entry.frame_num))
            .map_err(|_| "fixture reference FrameNum overflow")?,
        poc: entry.poc,
        long_term: entry.long_term.is_some(),
    })
}
pub(crate) fn progressive_original() -> Result<Stream, String> {
    let bytes: &'static [u8] =
        include_bytes!("../examples/assets/videotoolbox-h264/progressive-64x50.h264");
    let mut nals = Vec::new();
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
        nals.push((at, at + prefix));
        at += prefix;
    }
    let mut sps = None;
    let mut pps = None;
    let mut ranges = Vec::new();
    for (index, &(start, payload)) in nals.iter().enumerate() {
        let end = nals.get(index + 1).map_or(bytes.len(), |next| next.0);
        match bytes[payload] & 31 {
            7 => {
                if sps
                    .as_ref()
                    .is_some_and(|old: &Range<usize>| bytes[old.clone()] != bytes[payload..end])
                {
                    return Err("fixture changes original SPS".into());
                }
                sps = Some(payload..end);
            }
            8 => {
                if pps
                    .as_ref()
                    .is_some_and(|old: &Range<usize>| bytes[old.clone()] != bytes[payload..end])
                {
                    return Err("fixture changes original PPS".into());
                }
                pps = Some(payload..end);
            }
            1 | 5 => ranges.push(start..end),
            _ => {}
        }
    }
    let sps = sps.ok_or("fixture SPS missing")?;
    let pps = pps.ok_or("fixture PPS missing")?;
    if ranges.len() != 8 {
        return Err("fixture must have exactly eight original pictures".into());
    }
    let expected_ranges = [
        (590, 749),
        (755, 928),
        (934, 1109),
        (1115, 1290),
        (1332, 1491),
        (1497, 1670),
        (1676, 1835),
        (1841, 1997),
    ];
    let expected_frame = [0, 1, 2, 3, 0, 1, 2, 3];
    let expected_poc = [0, 2, 4, 6, 0, 2, 4, 6];
    let expected_prior = [
        None,
        Some(0),
        Some(1),
        Some(2),
        None,
        Some(4),
        Some(5),
        Some(6),
    ];
    let mut history = poc::History::default();
    let mut codec = dpb::Dpb::default();
    let mut owners = HashMap::new();
    let mut frames = Vec::new();
    for (index, range) in ranges.into_iter().enumerate() {
        if (range.start, range.end) != expected_ranges[index] {
            return Err(format!("original fixture AU boundary differs at {index}"));
        }
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
                .ok_or("fixture list has no original producer")?;
            if references
                .iter()
                .any(|prior: &Prior| prior.frame_index == frame_index)
            {
                continue;
            }
            let entry = codec
                .entries
                .iter()
                .find(|entry| entry.generation == *generation)
                .ok_or("fixture list generation missing")?;
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
        let idr = index == 0 || index == 4;
        let intra = parsed.slices.iter().all(|slice| slice.kind == 2);
        if parsed.frame_num != expected_frame[index]
            || order.poc != [expected_poc[index]; 2]
            || !parsed.reference
            || parsed.idr != idr
            || intra != idr
            || parsed.idr_pic_id != (if idr { Some((index / 4) as u32) } else { None })
            || references
                .iter()
                .map(|prior| prior.frame_index)
                .collect::<Vec<_>>()
                != expected_prior[index].into_iter().collect::<Vec<_>>()
            || setup.as_ref().is_none_or(|value| {
                value.frame_num != expected_frame[index] as u16
                    || value.poc != [expected_poc[index]; 2]
                    || value.long_term
            })
        {
            return Err(format!(
                "original fixture differs from fixed frame oracle at {index}"
            ));
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
            expected_y: None,
        });
        codec = plan.next;
        history = order.next;
    }
    Ok(Stream {
        bytes,
        sps,
        pps,
        frames,
    })
}

/// Original PCM/skip streams independently decoded by FFmpeg with exact pixels.
/// These cases are source proofs, not authorization to open native capability gates.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PcmCase {
    Marking(u8),
    Gap(u8),
    Bootstrap(u8),
    BSkip,
}
impl PcmCase {
    /// Exact wire metadata for original PCM fixtures. Keep the owning wire
    /// value and Raise pool alive while native Std pointers are consumed.
    pub(crate) fn parameter_sets(
        self,
    ) -> Result<sv_proto::VkVideoDecodeH264SessionParametersAddInfoKHR, String> {
        use ash::vk;
        use sv_proto as p;
        let (poc, gaps, profile) = match self {
            Self::Marking(poc) | Self::Bootstrap(poc) if poc <= 2 => (poc, false, 66),
            Self::Gap(poc) if poc <= 2 => (poc, true, 66),
            Self::BSkip => (0, false, 77),
            _ => return Err("fixture POC type must be zero, one, or two".into()),
        };
        fn flags<const N: usize>(value: &vk::native::__BindgenBitfieldUnit<[u8; N]>) -> u32 {
            assert!(N > 0 && N <= 4);
            value.get(0, (N * 8) as u8) as u32
        }
        let mut sps_flags: vk::native::StdVideoH264SpsFlags = unsafe { std::mem::zeroed() };
        sps_flags.set_frame_mbs_only_flag(1);
        sps_flags.set_direct_8x8_inference_flag(1);
        sps_flags.set_gaps_in_frame_num_value_allowed_flag(u32::from(gaps));
        sps_flags.set_delta_pic_order_always_zero_flag(u32::from(poc == 1));
        let sps = p::StdVideoH264SequenceParameterSet {
            flags: p::StdVideoH264SpsFlags(flags(&sps_flags._bitfield_1)),
            profile_idc: p::StdVideoH264ProfileIdc(profile),
            // Std enum zero serializes H.264 level_idc10.
            level_idc: p::StdVideoH264LevelIdc(0),
            chroma_format_idc: p::StdVideoH264ChromaFormatIdc(1),
            pic_order_cnt_type: p::StdVideoH264PocType(i32::from(poc)),
            num_ref_frames_in_pic_order_cnt_cycle: u8::from(poc == 1),
            p_offset_for_ref_frame: if poc == 1 { vec![2] } else { vec![] },
            max_num_ref_frames: 4,
            pic_width_in_mbs_minus1: 3,
            pic_height_in_map_units_minus1: 3,
            ..Default::default()
        };
        let mut pps_flags: vk::native::StdVideoH264PpsFlags = unsafe { std::mem::zeroed() };
        pps_flags.set_deblocking_filter_control_present_flag(1);
        let pps = p::StdVideoH264PictureParameterSet {
            flags: p::StdVideoH264PpsFlags(flags(&pps_flags._bitfield_1)),
            ..Default::default()
        };
        Ok(p::VkVideoDecodeH264SessionParametersAddInfoKHR {
            std_sps_count: 1,
            p_std_sp_ss: vec![sps],
            std_pps_count: 1,
            p_std_pp_ss: vec![pps],
            ..Default::default()
        })
    }
}
pub(crate) fn pcm_original(case: PcmCase) -> Result<Stream, String> {
    use h264::{Marking, Operation};
    let (bytes, poc_type): (&'static [u8], u32) = match case {
        PcmCase::Marking(0) => (
            include_bytes!(
                "../examples/assets/videotoolbox-h264/original-pcm-poc0-marking-64x64.h264"
            ),
            0,
        ),
        PcmCase::Marking(1) => (
            include_bytes!(
                "../examples/assets/videotoolbox-h264/original-pcm-poc1-marking-64x64.h264"
            ),
            1,
        ),
        PcmCase::Marking(2) => (
            include_bytes!(
                "../examples/assets/videotoolbox-h264/original-pcm-poc2-marking-64x64.h264"
            ),
            2,
        ),
        PcmCase::Gap(0) => (
            include_bytes!("../examples/assets/videotoolbox-h264/original-pcm-poc0-gap-64x64.h264"),
            0,
        ),
        PcmCase::Gap(1) => (
            include_bytes!("../examples/assets/videotoolbox-h264/original-pcm-poc1-gap-64x64.h264"),
            1,
        ),
        PcmCase::Gap(2) => (
            include_bytes!("../examples/assets/videotoolbox-h264/original-pcm-poc2-gap-64x64.h264"),
            2,
        ),
        PcmCase::Bootstrap(0) => (
            include_bytes!(
                "../examples/assets/videotoolbox-h264/original-pcm-poc0-bootstrap-64x64.h264"
            ),
            0,
        ),
        PcmCase::Bootstrap(1) => (
            include_bytes!(
                "../examples/assets/videotoolbox-h264/original-pcm-poc1-bootstrap-64x64.h264"
            ),
            1,
        ),
        PcmCase::Bootstrap(2) => (
            include_bytes!(
                "../examples/assets/videotoolbox-h264/original-pcm-poc2-bootstrap-64x64.h264"
            ),
            2,
        ),
        PcmCase::BSkip => (
            include_bytes!("../examples/assets/videotoolbox-h264/original-pcm-b-skip-64x64.h264"),
            0,
        ),
        _ => return Err("fixture POC type must be zero, one, or two".into()),
    };
    let mut nals = Vec::new();
    let mut at = 0;
    while at + 4 < bytes.len() {
        if bytes[at..].starts_with(&[0, 0, 0, 1]) {
            nals.push(at);
            at += 4;
        } else {
            at += 1;
        }
    }
    if nals.len() < 3 || nals[0] != 0 || bytes[4] & 31 != 7 || bytes[nals[1] + 4] & 31 != 8 {
        return Err("original PCM fixture parameter framing mismatch".into());
    }
    let sps = 4..nals[1];
    let pps = nals[1] + 4..nals[2];
    let (expected_frame, expected_poc, expected_y, expected_prior): (
        &[u32],
        &[i32],
        &[u8],
        &[&[usize]],
    ) = match case {
        PcmCase::Marking(_) => (
            &[0, 1, 2, 3, 1, 2, 3],
            &[0, 2, 4, 6, 2, 4, 6],
            &[40, 80, 40, 120, 120, 160, 160],
            &[&[], &[], &[0], &[], &[3], &[], &[5]],
        ),
        PcmCase::Gap(_) => (&[0, 3, 4], &[0, 6, 8], &[40, 80, 80], &[&[], &[], &[1]]),
        PcmCase::Bootstrap(_) => (&[3], &[6], &[120], &[&[]]),
        PcmCase::BSkip => (
            &[0, 1, 2, 2],
            &[0, 4, 2, 6],
            &[40, 120, 80, 120],
            &[&[], &[], &[0, 1], &[1]],
        ),
    };
    if nals.len() != expected_frame.len() + 2 {
        return Err("original PCM fixture AU count mismatch".into());
    }
    let mut history = poc::History::default();
    let mut codec = dpb::Dpb::default();
    let mut owners = HashMap::new();
    let mut frames = Vec::new();
    for index in 0..expected_frame.len() {
        let start = nals[index + 2];
        let end = nals.get(index + 3).copied().unwrap_or(bytes.len());
        let range = start..end;
        let parsed = h264::parse(
            bytes,
            &range,
            &[0],
            &bytes[sps.clone()],
            &bytes[pps.clone()],
        )?;
        let history_plan = history.plan_unproven(&parsed, matches!(case, PcmCase::Bootstrap(_)))?;
        let limits = dpb::Limits {
            max_frame_num: parsed.max_frame_num,
            max_refs: parsed.max_refs,
        };
        let inferred = codec.synthesize_unproven(&history_plan.missing, limits)?;
        let current = dpb::Picture {
            frame_num: parsed.frame_num,
            poc: history_plan.poc,
        };
        let lists = inferred.reference_lists(current, &parsed)?;
        let mut references = Vec::new();
        for generation in lists.iter().flat_map(|pair| pair.iter()).flatten() {
            let frame_index = *owners
                .get(generation)
                .ok_or("PCM fixture predicts a non-existing frame")?;
            if references
                .iter()
                .any(|prior: &Prior| prior.frame_index == frame_index)
            {
                continue;
            }
            let entry = inferred
                .entries
                .iter()
                .find(|entry| entry.generation == *generation)
                .ok_or("PCM source reference entry missing")?;
            references.push(Prior {
                frame_index,
                metadata: metadata(entry)?,
            });
        }
        let plan = inferred.plan(current, &parsed.marking, limits)?;
        let setup = plan.current.as_ref().map(metadata).transpose()?;
        let expected_idr = index == 0 && !matches!(case, PcmCase::Bootstrap(_));
        let expected_intra = match case {
            PcmCase::Marking(_) => matches!(index, 0 | 1 | 3 | 5),
            PcmCase::Gap(_) => index < 2,
            PcmCase::Bootstrap(_) => true,
            PcmCase::BSkip => index < 2,
        };
        let expected_reference = !(matches!(case, PcmCase::BSkip) && index == 2);
        if parsed.sequence.poc != poc_type
            || parsed.sequence.coded_extent != [64, 64]
            || parsed.max_refs != 4
            || parsed.frame_num != expected_frame[index]
            || history_plan.poc != [expected_poc[index]; 2]
            || parsed.idr != expected_idr
            || parsed.reference != expected_reference
            || parsed.slices.iter().all(|slice| slice.kind == 2) != expected_intra
            || references
                .iter()
                .map(|prior| prior.frame_index)
                .collect::<Vec<_>>()
                != expected_prior[index]
        {
            return Err(format!(
                "original PCM source disagrees with independent frame oracle at {index}"
            ));
        }
        let expected_marking = if expected_idr {
            Marking::Idr {
                no_output_prior: false,
                long_term: false,
            }
        } else if !expected_reference {
            Marking::NonReference
        } else if matches!(case, PcmCase::Marking(_)) {
            match index {
                1 => Marking::Adaptive(vec![
                    Operation::LimitLong { max_index_plus1: 2 },
                    Operation::Promote {
                        difference_minus1: 0,
                        index: 1,
                    },
                ]),
                3 => Marking::Adaptive(vec![Operation::ForgetAll]),
                5 => Marking::Adaptive(vec![
                    Operation::LimitLong { max_index_plus1: 1 },
                    Operation::CurrentLong { index: 0 },
                ]),
                _ => Marking::Sliding,
            }
        } else {
            Marking::Sliding
        };
        if parsed.marking != expected_marking {
            return Err(format!("original PCM source marking differs at {index}"));
        }
        let expected_setup_frame = if matches!(case, PcmCase::Marking(_)) && matches!(index, 3 | 5)
        {
            0
        } else {
            expected_frame[index] as u16
        };
        let expected_setup_poc = if matches!(case, PcmCase::Marking(_)) && index == 3 {
            [0; 2]
        } else {
            [expected_poc[index]; 2]
        };
        let expected_setup_long = matches!(case, PcmCase::Marking(_)) && index == 5;
        if setup
            .as_ref()
            .map(|value| (value.frame_num, value.poc, value.long_term))
            != expected_reference.then_some((
                expected_setup_frame,
                expected_setup_poc,
                expected_setup_long,
            ))
        {
            return Err(format!("original PCM source setup differs at {index}"));
        }
        // The gap case must have exactly the two inferred reference frames.
        if history_plan.missing.len()
            != if matches!(case, PcmCase::Gap(_)) && index == 1 {
                2
            } else {
                0
            }
        {
            return Err("original PCM source gap oracle differs".into());
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
            poc: history_plan.poc,
            is_reference: parsed.reference,
            is_intra: expected_intra,
            idr: parsed.idr,
            idr_pic_id: parsed.idr_pic_id,
            setup,
            references,
            expected_y: Some(expected_y[index]),
        });
        codec = plan.next;
        history = history_plan.next;
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
    fn fixed_original_eight_frame_producer_and_reference_contract() {
        let stream = progressive_original().unwrap();
        assert_eq!(stream.frames.len(), 8);
        for (index, frame) in stream.frames.iter().enumerate() {
            assert!(
                stream.bytes[frame.range.clone()].starts_with(&[0, 0, 0, 1])
                    || stream.bytes[frame.range.clone()].starts_with(&[0, 0, 1])
            );
            assert_eq!(frame.slice_offsets, vec![0]);
            if let Some(prior) = frame.references.first() {
                assert_eq!(prior.frame_index, index - 1);
                assert_eq!(
                    Some(&prior.metadata),
                    stream.frames[index - 1].setup.as_ref()
                );
            }
        }
        assert_eq!(stream.bytes[stream.sps.start] & 31, 7);
        assert_eq!(stream.bytes[stream.pps.start] & 31, 8);
        assert_eq!(stream.frames[4].idr_pic_id, Some(1));
    }
    #[test]
    fn original_pcm_streams_cover_actual_marking_gaps_bootstrap_and_b_lists() {
        for ty in 0..=2 {
            let marking = pcm_original(PcmCase::Marking(ty)).unwrap();
            assert_eq!(
                marking.frames[2].references[0].metadata,
                Metadata {
                    frame_num: 1,
                    poc: [0; 2],
                    long_term: true
                }
            );
            assert_eq!(marking.frames[3].setup.as_ref().unwrap().poc, [0; 2]);
            assert_eq!(
                marking.frames[6].references[0].metadata,
                Metadata {
                    frame_num: 0,
                    poc: [4; 2],
                    long_term: true
                }
            );
            assert_eq!(pcm_original(PcmCase::Gap(ty)).unwrap().frames.len(), 3);
            assert_eq!(
                pcm_original(PcmCase::Bootstrap(ty)).unwrap().frames[0].poc,
                [6; 2]
            );
        }
        let b = pcm_original(PcmCase::BSkip).unwrap();
        assert!(!b.frames[2].is_reference);
        assert_eq!(
            b.frames[2]
                .references
                .iter()
                .map(|prior| prior.frame_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(b.frames[2].expected_y, Some(80));
        assert_eq!(b.frames[3].references[0].frame_index, 1);
    }
    #[test]
    fn bootstrap_nals_are_selected_unchanged_from_complete_original_sources() {
        for ty in 0..=2 {
            let source: &[u8] = match ty {
                0 => include_bytes!(
                    "../examples/assets/videotoolbox-h264/original-pcm-poc0-bootstrap-source-64x64.h264"
                ),
                1 => include_bytes!(
                    "../examples/assets/videotoolbox-h264/original-pcm-poc1-bootstrap-source-64x64.h264"
                ),
                _ => include_bytes!(
                    "../examples/assets/videotoolbox-h264/original-pcm-poc2-bootstrap-source-64x64.h264"
                ),
            };
            let selected = pcm_original(PcmCase::Bootstrap(ty)).unwrap();
            let frame = &selected.frames[0];
            assert_eq!(
                &selected.bytes[..frame.range.start],
                &source[..frame.range.start]
            );
            assert!(source.ends_with(&selected.bytes[frame.range.clone()]));
            assert!(source.len() > selected.bytes.len());
        }
    }
    #[test]
    fn original_pcm_wire_parameters_raise_to_exact_original_sps_pps() {
        use ash::vk;
        let mut cases = vec![PcmCase::BSkip];
        for ty in 0..=2 {
            cases.extend([
                PcmCase::Marking(ty),
                PcmCase::Gap(ty),
                PcmCase::Bootstrap(ty),
            ]);
        }
        for case in cases {
            let stream = pcm_original(case).unwrap();
            let sets = case.parameter_sets().unwrap();
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
                unsafe { parameter_encoder::sps_nal(sps) }.unwrap(),
                stream.bytes[stream.sps.clone()]
            );
            assert_eq!(
                unsafe { parameter_encoder::pps_nal(pps, sps) }.unwrap(),
                stream.bytes[stream.pps.clone()]
            );
            let poc = match case {
                PcmCase::Marking(ty) | PcmCase::Gap(ty) | PcmCase::Bootstrap(ty) => ty,
                PcmCase::BSkip => 0,
            };
            assert_eq!(
                sps.flags.delta_pic_order_always_zero_flag(),
                u32::from(poc == 1)
            );
            assert_eq!(
                sps.flags.gaps_in_frame_num_value_allowed_flag(),
                u32::from(matches!(case, PcmCase::Gap(_)))
            );
            assert_eq!(
                sps.num_ref_frames_in_pic_order_cnt_cycle,
                u8::from(poc == 1)
            );
            if poc == 1 {
                assert!(!sps.pOffsetForRefFrame.is_null());
                assert_eq!(unsafe { *sps.pOffsetForRefFrame }, 2);
            }
            assert_eq!(pps.flags.deblocking_filter_control_present_flag(), 1);
            assert_eq!(pps.pic_init_qp_minus26, 0);
        }
        assert!(PcmCase::Marking(3).parameter_sets().is_err());
    }
}
