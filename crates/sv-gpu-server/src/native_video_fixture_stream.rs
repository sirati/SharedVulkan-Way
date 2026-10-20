//! Unwired, portable multi-frame proof input. This preserves the checked-in
//! original bitstream and imports the production header/history/DPB models.
//! Source provenance: examples/assets/videotoolbox-h264/provenance.txt.
#[path = "native_video_dpb.rs"]
mod dpb;
#[path = "native_video_h264.rs"]
mod h264;
#[path = "native_video_lists.rs"]
mod lists;
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
}
