//! Bounded header-only H.264 inspection directly over original EBSP bytes.
//! Compressed payload is neither copied nor rewritten.
use std::ops::Range;
struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
    value: u8,
    left: u8,
    zeros: u8,
}
impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            value: 0,
            left: 0,
            zeros: 0,
        }
    }
    fn read(&mut self, count: u32) -> Result<u32, String> {
        if count > 32 {
            return Err("H264 integer width exceeds 32 bits".into());
        }
        let mut value = 0;
        for _ in 0..count {
            if self.left == 0 {
                let mut byte = *self.bytes.get(self.at).ok_or("truncated H264 header")?;
                self.at += 1;
                if self.zeros == 2 && byte == 3 {
                    byte = *self
                        .bytes
                        .get(self.at)
                        .ok_or("truncated emulation prevention")?;
                    self.at += 1;
                    if byte > 3 {
                        return Err("invalid emulation prevention byte".into());
                    }
                    self.zeros = 0;
                }
                self.zeros = if byte == 0 {
                    self.zeros.saturating_add(1).min(2)
                } else {
                    0
                };
                self.value = byte;
                self.left = 8;
            }
            self.left -= 1;
            value = (value << 1) | u32::from((self.value >> self.left) & 1);
        }
        Ok(value)
    }
    fn ue(&mut self) -> Result<u32, String> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err("H264 Exp-Golomb overflow".into());
            }
        }
        Ok(((1u32 << zeros) - 1) + self.read(zeros)?)
    }
    fn se(&mut self) -> Result<i32, String> {
        let n = self.ue()?;
        let value = if n & 1 == 1 {
            i64::from(n) / 2 + 1
        } else {
            -(i64::from(n) / 2)
        };
        i32::try_from(value).map_err(|_| "H264 signed Exp-Golomb overflow".into())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Summary {
    pub idr: bool,
    pub intra: bool,
    pub reference: bool,
}
fn nals<'a>(
    bytes: &'a [u8],
    range: &Range<usize>,
    offsets: &[u32],
) -> Result<Vec<&'a [u8]>, String> {
    let bytes = bytes
        .get(range.clone())
        .ok_or("compressed range out of bounds")?;
    if offsets.is_empty() {
        return Err("empty slice list".into());
    }
    if offsets.windows(2).any(|o| o[0] >= o[1]) {
        return Err("unordered slice offsets".into());
    }
    offsets
        .iter()
        .enumerate()
        .map(|(i, &offset)| {
            let end = offsets.get(i + 1).map_or(bytes.len(), |&n| n as usize);
            let b = bytes
                .get(offset as usize..end)
                .ok_or("slice offset out of bounds")?;
            let prefix = if b.starts_with(&[0, 0, 0, 1]) {
                4
            } else if b.starts_with(&[0, 0, 1]) {
                3
            } else {
                return Err("missing Annex B prefix".into());
            };
            let nal = b
                .get(prefix..)
                .filter(|n| !n.is_empty())
                .ok_or("missing slice header")?;
            if nal[0] & 128 != 0 || !matches!(nal[0] & 31, 1 | 5) {
                return Err("unsupported selected slice NAL".into());
            }
            Ok(nal)
        })
        .collect()
}
pub(super) fn summary(
    bytes: &[u8],
    range: &Range<usize>,
    offsets: &[u32],
) -> Result<Summary, String> {
    let mut result = None;
    for nal in nals(bytes, range, offsets)? {
        let mut b = Bits::new(&nal[1..]);
        b.ue()?;
        let slice_type = b.ue()?;
        if slice_type > 9 || matches!(slice_type % 5, 3 | 4) {
            return Err("unsupported H264 slice type".into());
        }
        let current = Summary {
            idr: nal[0] & 31 == 5,
            intra: slice_type % 5 == 2,
            reference: nal[0] & 96 != 0,
        };
        if current.idr && (!current.intra || !current.reference) {
            return Err("IDR must be a reference I slice".into());
        }
        if result.is_some_and(|old| old != current) {
            return Err("inconsistent picture slice headers".into());
        }
        result = Some(current);
    }
    result.ok_or_else(|| "empty slice list".into())
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Sequence {
    pub id: u32,
    pub frame_bits: u32,
    pub poc: u32,
    pub poc_bits: u32,
    pub delta_zero: bool,
    pub max_refs: u32,
    pub gaps_allowed: bool,
    pub offset_non_ref: i32,
    pub offset_top_bottom: i32,
    pub cycle_offsets: Vec<i32>,
    pub coded_extent: [u32; 2],
}
fn sps(nal: &[u8]) -> Result<Sequence, String> {
    if nal.first().map(|b| b & 31) != Some(7) {
        return Err("expected SPS".into());
    }
    let mut b = Bits::new(&nal[1..]);
    let profile = b.read(8)?;
    b.read(16)?;
    let id = b.ue()?;
    if !matches!(profile, 66 | 77 | 100) || id > 31 {
        return Err("unsupported SPS".into());
    }
    if profile == 100 {
        if b.ue()? != 1 || b.ue()? != 0 || b.ue()? != 0 {
            return Err("requires 8bit 420 SPS".into());
        }
        b.read(1)?;
        if b.read(1)? != 0 {
            for i in 0..8 {
                if b.read(1)? != 0 {
                    let mut last = 8;
                    let mut next = 8;
                    for _ in 0..if i < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next =
                                (i64::from(last) + i64::from(b.se()?) + 256).rem_euclid(256) as i32
                        }
                        if next != 0 {
                            last = next
                        }
                    }
                }
            }
        }
    }
    let frame_bits = b
        .ue()?
        .checked_add(4)
        .filter(|n| *n <= 16)
        .ok_or("invalid frame_num width")?;
    let poc = b.ue()?;
    let mut poc_bits = 0;
    let mut delta_zero = false;
    let mut offset_non_ref = 0;
    let mut offset_top_bottom = 0;
    let mut cycle_offsets = Vec::new();
    match poc {
        0 => {
            poc_bits = b
                .ue()?
                .checked_add(4)
                .filter(|n| *n <= 16)
                .ok_or("invalid POC width")?
        }
        1 => {
            delta_zero = b.read(1)? != 0;
            offset_non_ref = b.se()?;
            offset_top_bottom = b.se()?;
            let count = b.ue()?;
            if count > 255 {
                return Err("invalid POC cycle".into());
            }
            for _ in 0..count {
                cycle_offsets.push(b.se()?);
            }
        }
        2 => {}
        _ => return Err("invalid POC type".into()),
    }
    let max_refs = b.ue()?;
    if max_refs > 16 {
        return Err("too many SPS reference frames".into());
    }
    let gaps_allowed = b.read(1)? != 0;
    let width = b
        .ue()?
        .checked_add(1)
        .and_then(|n| n.checked_mul(16))
        .filter(|n| *n <= 8192)
        .ok_or("invalid SPS coded width")?;
    let height = b
        .ue()?
        .checked_add(1)
        .and_then(|n| n.checked_mul(16))
        .filter(|n| *n <= 8192)
        .ok_or("invalid SPS coded height")?;
    if b.read(1)? != 1 {
        return Err("requires progressive SPS".into());
    }
    Ok(Sequence {
        id,
        frame_bits,
        poc,
        poc_bits,
        delta_zero,
        max_refs,
        gaps_allowed,
        offset_non_ref,
        offset_top_bottom,
        cycle_offsets,
        coded_extent: [width, height],
    })
}
struct Pps {
    id: u32,
    bottom_poc: bool,
    redundant: bool,
    default_active: [u32; 2],
    weighted_pred: bool,
    weighted_bipred: u32,
}
fn pps(nal: &[u8], sps: &Sequence) -> Result<Pps, String> {
    if nal.first().map(|b| b & 31) != Some(8) {
        return Err("expected PPS".into());
    }
    let mut b = Bits::new(&nal[1..]);
    let id = b.ue()?;
    if id > 255 || b.ue()? != sps.id {
        return Err("PPS/SPS identity mismatch".into());
    }
    b.read(1)?;
    let bottom_poc = b.read(1)? != 0;
    if b.ue()? != 0 {
        return Err("slice groups unsupported".into());
    }
    let default_active = [active_count(b.ue()?)?, active_count(b.ue()?)?];
    let weighted_pred = b.read(1)? != 0;
    let weighted_bipred = b.read(2)?;
    if weighted_bipred > 2 {
        return Err("invalid weighted_bipred_idc".into());
    }
    b.se()?;
    b.se()?;
    b.se()?;
    b.read(2)?;
    let redundant = b.read(1)? != 0;
    Ok(Pps {
        id,
        bottom_poc,
        redundant,
        default_active,
        weighted_pred,
        weighted_bipred,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    ForgetShort { difference_minus1: u32 },
    ForgetLong { pic_num: u32 },
    Promote { difference_minus1: u32, index: u32 },
    LimitLong { max_index_plus1: u32 },
    ForgetAll,
    CurrentLong { index: u32 },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Marking {
    NonReference,
    Idr {
        no_output_prior: bool,
        long_term: bool,
    },
    Sliding,
    Adaptive(Vec<Operation>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Modification {
    Subtract(u32),
    Add(u32),
    Long(u32),
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Poc {
    pub lsb: Option<u32>,
    pub bottom_delta: i32,
    pub delta: [i32; 2],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Slice {
    pub first_mb: u32,
    /// P=0, B=1, I=2. SP/SI are deliberately unsupported profiles here.
    pub kind: u32,
    pub active: [u32; 2],
    pub modifications: [Vec<Modification>; 2],
}
#[derive(Clone, Debug)]
pub(super) struct Parsed {
    pub sequence: Sequence,
    pub idr: bool,
    pub reference: bool,
    pub frame_num: u32,
    pub pps_id: u32,
    pub idr_pic_id: Option<u32>,
    pub poc: Poc,
    pub max_frame_num: u32,
    pub max_refs: u32,
    pub marking: Marking,
    pub slices: Vec<Slice>,
}
fn active_count(minus1: u32) -> Result<u32, String> {
    minus1
        .checked_add(1)
        .filter(|n| *n <= 32)
        .ok_or_else(|| "invalid active reference count".into())
}
fn modifications(b: &mut Bits<'_>) -> Result<Vec<Modification>, String> {
    let mut result = Vec::new();
    if b.read(1)? == 0 {
        return Ok(result);
    }
    for _ in 0..64 {
        match b.ue()? {
            0 => result.push(Modification::Subtract(b.ue()?)),
            1 => result.push(Modification::Add(b.ue()?)),
            2 => result.push(Modification::Long(b.ue()?)),
            3 => return Ok(result),
            _ => return Err("invalid reference-list modification".into()),
        }
    }
    Err("too many reference-list modifications".into())
}
fn weights(b: &mut Bits<'_>, active: [u32; 2], bi: bool) -> Result<(), String> {
    if b.ue()? > 7 || b.ue()? > 7 {
        return Err("invalid prediction weight denominator".into());
    }
    for count in [active[0], if bi { active[1] } else { 0 }] {
        for _ in 0..count {
            if b.read(1)? != 0 {
                b.se()?;
                b.se()?;
            }
            if b.read(1)? != 0 {
                for _ in 0..2 {
                    b.se()?;
                    b.se()?;
                }
            }
        }
    }
    Ok(())
}
fn marking(b: &mut Bits<'_>, idr: bool, reference: bool) -> Result<Marking, String> {
    if !reference {
        return Ok(Marking::NonReference);
    }
    if idr {
        return Ok(Marking::Idr {
            no_output_prior: b.read(1)? != 0,
            long_term: b.read(1)? != 0,
        });
    }
    if b.read(1)? == 0 {
        return Ok(Marking::Sliding);
    }
    let mut result = Vec::new();
    for _ in 0..64 {
        result.push(match b.ue()? {
            0 => return Ok(Marking::Adaptive(result)),
            1 => Operation::ForgetShort {
                difference_minus1: b.ue()?,
            },
            2 => Operation::ForgetLong { pic_num: b.ue()? },
            3 => Operation::Promote {
                difference_minus1: b.ue()?,
                index: b.ue()?,
            },
            4 => Operation::LimitLong {
                max_index_plus1: b.ue()?,
            },
            5 => Operation::ForgetAll,
            6 => Operation::CurrentLong { index: b.ue()? },
            _ => return Err("invalid memory-management operation".into()),
        });
    }
    Err("too many memory-management operations".into())
}
/// Parse only header syntax through dec_ref_pic_marking. Lists and marking are
/// metadata; every compressed byte stays in the caller's original shared range.
/// No implicit VT reference state is inferred from these results.
pub(super) fn parse(
    bytes: &[u8],
    range: &Range<usize>,
    offsets: &[u32],
    sps_nal: &[u8],
    pps_nal: &[u8],
) -> Result<Parsed, String> {
    let s = sps(sps_nal)?;
    let p = pps(pps_nal, &s)?;
    let mut parsed: Option<Parsed> = None;
    let mut claimed_type = None;
    let mut seen_types = 0u32;
    for nal in nals(bytes, range, offsets)? {
        let idr = nal[0] & 31 == 5;
        let reference = nal[0] & 96 != 0;
        let mut b = Bits::new(&nal[1..]);
        let first_mb = b.ue()?;
        let raw_type = b.ue()?;
        let kind = raw_type % 5;
        if raw_type > 9 || kind > 2 || idr && (!reference || kind != 2) {
            return Err("unsupported or invalid slice type".into());
        }
        seen_types |= 1 << kind;
        if raw_type >= 5 {
            claimed_type = Some(kind)
        }
        if claimed_type.is_some_and(|kind| seen_types != 1 << kind) {
            return Err("all-slices type declaration mismatch".into());
        }
        let pps_id = b.ue()?;
        if pps_id != p.id {
            return Err("slice PPS mismatch".into());
        }
        let frame_num = b.read(s.frame_bits)?;
        let idr_pic_id = if idr { Some(b.ue()?) } else { None };
        let mut poc = Poc::default();
        match s.poc {
            0 => {
                poc.lsb = Some(b.read(s.poc_bits)?);
                if p.bottom_poc {
                    poc.bottom_delta = b.se()?;
                }
            }
            1 if !s.delta_zero => {
                poc.delta[0] = b.se()?;
                if p.bottom_poc {
                    poc.delta[1] = b.se()?;
                }
            }
            _ => {}
        }
        if p.redundant && b.ue()? != 0 {
            return Err("redundant coded pictures unsupported".into());
        }
        if kind == 1 {
            b.read(1)?;
        }
        let mut active = if kind == 2 {
            [0, 0]
        } else if kind == 0 {
            [p.default_active[0], 0]
        } else {
            p.default_active
        };
        if kind != 2 && b.read(1)? != 0 {
            active[0] = active_count(b.ue()?)?;
            if kind == 1 {
                active[1] = active_count(b.ue()?)?;
            }
        }
        if active.iter().any(|count| *count > 16) {
            return Err("progressive active reference count exceeds sixteen".into());
        }
        let mut lists = [Vec::new(), Vec::new()];
        if kind != 2 {
            lists[0] = modifications(&mut b)?;
            if kind == 1 {
                lists[1] = modifications(&mut b)?;
            }
        }
        if p.weighted_pred && kind == 0 || p.weighted_bipred == 1 && kind == 1 {
            weights(&mut b, active, kind == 1)?;
        }
        let marking = marking(&mut b, idr, reference)?;
        let slice = Slice {
            first_mb,
            kind,
            active,
            modifications: lists,
        };
        if let Some(old) = &mut parsed {
            if old.idr != idr
                || old.reference != reference
                || old.frame_num != frame_num
                || old.pps_id != pps_id
                || old.idr_pic_id != idr_pic_id
                || old.poc != poc
                || old.marking != marking
            {
                return Err("inconsistent picture identity or marking across slices".into());
            }
            old.slices.push(slice);
        } else {
            parsed = Some(Parsed {
                sequence: s.clone(),
                idr,
                reference,
                frame_num,
                pps_id,
                idr_pic_id,
                poc,
                max_frame_num: 1 << s.frame_bits,
                max_refs: s.max_refs,
                marking,
                slices: vec![slice],
            });
        }
    }
    parsed.ok_or_else(|| "empty slice list".into())
}
/// A non-IDR intra bootstrap must not make unexplained DPB changes. Parse
/// through dec_ref_pic_marking for every I slice and permit only default marking.
/// Full MMCO/reference-list reconciliation for dependent pictures is separate.
pub(super) fn independent_intra(
    bytes: &[u8],
    range: &Range<usize>,
    offsets: &[u32],
    sps_nal: &[u8],
    pps_nal: &[u8],
    frame_num: u16,
    pic_order_cnt: [i32; 2],
) -> Result<(), String> {
    let s = sps(sps_nal)?;
    let p = pps(pps_nal, &s)?;
    for nal in nals(bytes, range, offsets)? {
        if nal[0] & 31 != 1 {
            return Err("expected non-IDR intra bootstrap".into());
        }
        let mut b = Bits::new(&nal[1..]);
        b.ue()?;
        let ty = b.ue()?;
        if ty > 9 || ty % 5 != 2 || b.ue()? != p.id {
            return Err("expected matching I slice/PPS".into());
        }
        if b.read(s.frame_bits)? != u32::from(frame_num) {
            return Err("slice frame_num differs from metadata".into());
        }
        let expected_poc = match s.poc {
            0 => {
                let lsb = b.read(s.poc_bits)? as i32;
                let maximum = 1i32 << s.poc_bits;
                // Fresh decoder previous POC MSB/LSB are zero. Reject metadata
                // whose POC relied on reference history before this reset.
                let top = if lsb > maximum / 2 {
                    lsb - maximum
                } else {
                    lsb
                };
                let delta = if p.bottom_poc { b.se()? } else { 0 };
                [top, top.checked_add(delta).ok_or("bootstrap POC overflow")?]
            }
            2 => return Err("POC type2 non-IDR bootstrap awaits hardware validation".into()),
            // POC type1 needs the cycle offsets and previous-frame state
            // reconciled before it can bootstrap this staging runtime.
            _ => return Err("POC type1 intra bootstrap is not implemented".into()),
        };
        if expected_poc != pic_order_cnt {
            return Err("intra bootstrap POC depends on unavailable prior state".into());
        }
        if p.redundant && b.ue()? != 0 {
            return Err("redundant slices unsupported".into());
        }
        if nal[0] & 96 != 0 && b.read(1)? != 0 {
            return Err("adaptive MMCO in intra bootstrap is unsupported".into());
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn derives_stock_intra_and_inter_from_original_slice_type() {
        assert_eq!(
            summary(&[0, 0, 1, 0x41, 0xb8], &(0..5), &[0]).unwrap(),
            Summary {
                idr: false,
                intra: true,
                reference: true
            }
        );
        assert!(
            !summary(&[0, 0, 1, 0x41, 0xe0], &(0..5), &[0])
                .unwrap()
                .intra
        );
        assert!(summary(&[0, 0, 1, 0x65, 0xb8], &(0..5), &[0]).unwrap().idr);
    }
    #[test]
    fn rejects_truncated_and_mixed_slice_types() {
        assert!(summary(&[0, 0, 1, 0x41, 0], &(0..5), &[0]).is_err());
        assert!(
            summary(
                &[0, 0, 1, 0x41, 0xb8, 0, 0, 1, 0x41, 0xe0],
                &(0..10),
                &[0, 5]
            )
            .is_err()
        );
    }
    #[test]
    fn bit_reader_skips_prevention_without_copying() {
        let mut b = Bits::new(&[0, 0, 3, 1]);
        assert_eq!(b.read(24).unwrap(), 1);
        assert!(Bits::new(&[0, 0, 3, 4]).read(32).is_err());
    }
    #[test]
    fn original_open_gop_i_has_default_marking_and_independent_poc() {
        let stream =
            include_bytes!("../examples/assets/videotoolbox-h264/non-idr-intra-64x50.h264");
        assert_eq!(
            summary(stream, &(35..156), &[0]).unwrap(),
            Summary {
                idr: false,
                intra: true,
                reference: true
            }
        );
        independent_intra(
            stream,
            &(35..156),
            &[0],
            &stream[4..26],
            &stream[30..35],
            3,
            [8, 8],
        )
        .unwrap();
        assert!(
            independent_intra(
                stream,
                &(35..156),
                &[0],
                &stream[4..26],
                &stream[30..35],
                2,
                [8, 8]
            )
            .is_err()
        );
        assert!(
            independent_intra(
                stream,
                &(35..156),
                &[0],
                &stream[4..26],
                &stream[30..35],
                3,
                [24, 24]
            )
            .is_err()
        );
    }
    #[test]
    fn bootstrap_rejects_hidden_adaptive_reference_marking() {
        let original =
            include_bytes!("../examples/assets/videotoolbox-h264/non-idr-intra-64x50.h264");
        let s = sps(&original[4..26]).unwrap();
        let p = pps(&original[30..35], &s).unwrap();
        assert_eq!(s.poc, 0);
        let mut b = Bits::new(&original[40..156]);
        b.ue().unwrap();
        b.ue().unwrap();
        b.ue().unwrap();
        b.read(s.frame_bits).unwrap();
        b.read(s.poc_bits).unwrap();
        if p.bottom_poc {
            b.se().unwrap();
        }
        if p.redundant {
            b.ue().unwrap();
        }
        let marking_bit = b.at * 8 - usize::from(b.left);
        assert_eq!(b.read(1).unwrap(), 0);
        let mut altered = original.to_vec();
        altered[40 + marking_bit / 8] |= 1 << (7 - marking_bit % 8);
        assert!(
            independent_intra(
                &altered,
                &(35..156),
                &[0],
                &altered[4..26],
                &altered[30..35],
                3,
                [8, 8]
            )
            .unwrap_err()
            .contains("adaptive MMCO")
        );
    }
    #[derive(Default)]
    struct Writer(Vec<bool>);
    impl Writer {
        fn bit(&mut self, b: bool) {
            self.0.push(b)
        }
        fn fixed(&mut self, n: u32, count: u32) {
            for i in (0..count).rev() {
                self.bit(n >> i & 1 != 0)
            }
        }
        fn ue(&mut self, n: u32) {
            let v = n + 1;
            let width = 32 - v.leading_zeros();
            for _ in 1..width {
                self.bit(false)
            }
            self.fixed(v, width)
        }
        fn se(&mut self, n: i32) {
            self.ue(if n <= 0 {
                (-n as u32) * 2
            } else {
                n as u32 * 2 - 1
            })
        }
        fn nal(mut self, header: u8) -> Vec<u8> {
            self.bit(true);
            while self.0.len() % 8 != 0 {
                self.bit(false)
            }
            let mut out = vec![header];
            let mut zeros = 0;
            for chunk in self.0.chunks(8) {
                let n = chunk.iter().fold(0u8, |v, b| (v << 1) | u8::from(*b));
                if zeros == 2 && n <= 3 {
                    out.push(3);
                    zeros = 0
                }
                out.push(n);
                zeros = if n == 0 { zeros + 1 } else { 0 };
            }
            out
        }
    }
    fn parameters(weighted: bool, bipred: u32) -> (Vec<u8>, Vec<u8>) {
        let mut s = Writer::default();
        s.fixed(66, 8);
        s.fixed(0, 8);
        s.fixed(30, 8);
        for _ in 0..4 {
            s.ue(0)
        }
        s.ue(4);
        s.bit(false);
        s.ue(3);
        s.ue(3);
        s.bit(true);
        s.bit(true);
        s.bit(false);
        s.bit(false);
        let mut p = Writer::default();
        p.ue(0);
        p.ue(0);
        p.bit(false);
        p.bit(false);
        p.ue(0);
        p.ue(0);
        p.ue(0);
        p.bit(weighted);
        p.fixed(bipred, 2);
        p.se(0);
        p.se(0);
        p.se(0);
        p.bit(true);
        p.bit(false);
        p.bit(false);
        (s.nal(0x67), p.nal(0x68))
    }
    fn slice_prefix(kind: u32) -> Writer {
        let mut b = Writer::default();
        b.ue(0);
        b.ue(kind);
        b.ue(0);
        b.fixed(3, 4);
        b.fixed(6, 4);
        b
    }
    fn one_slice(b: Writer, s: &[u8], p: &[u8], reference: bool) -> Parsed {
        let mut bytes = vec![0, 0, 1];
        bytes.extend(b.nal(if reference { 0x41 } else { 0x01 }));
        parse(&bytes, &(0..bytes.len()), &[0], s, p).unwrap()
    }
    #[test]
    fn parses_weighted_p_list_modifications_and_all_mmco_operands() {
        let (s, p) = parameters(true, 0);
        let mut b = slice_prefix(0);
        b.bit(true);
        b.ue(1); // override two L0 references
        b.bit(true);
        b.ue(0);
        b.ue(0);
        b.ue(2);
        b.ue(3);
        b.ue(3); // subtract, long, end
        b.ue(0);
        b.ue(0); // luma/chroma weight denominators
        b.bit(true);
        b.se(1);
        b.se(-2);
        b.bit(true);
        for _ in 0..2 {
            b.se(1);
            b.se(0)
        }
        b.bit(false);
        b.bit(false); // second reference has default weights
        b.bit(true); // adaptive marking
        b.ue(1);
        b.ue(2);
        b.ue(2);
        b.ue(3);
        b.ue(3);
        b.ue(4);
        b.ue(1);
        b.ue(4);
        b.ue(3);
        b.ue(6);
        b.ue(2);
        b.ue(0);
        let picture = one_slice(b, &s, &p, true);
        assert_eq!(picture.slices[0].active, [2, 0]);
        assert_eq!(
            picture.slices[0].modifications[0],
            vec![Modification::Subtract(0), Modification::Long(3)]
        );
        assert_eq!(
            picture.marking,
            Marking::Adaptive(vec![
                Operation::ForgetShort {
                    difference_minus1: 2
                },
                Operation::ForgetLong { pic_num: 3 },
                Operation::Promote {
                    difference_minus1: 4,
                    index: 1
                },
                Operation::LimitLong { max_index_plus1: 3 },
                Operation::CurrentLong { index: 2 }
            ])
        );
    }
    #[test]
    fn parses_weighted_b_override_lists_and_nonreference_marking() {
        let (s, p) = parameters(false, 1);
        let mut b = slice_prefix(1);
        b.bit(true); // direct_spatial_mv_pred_flag
        b.bit(true);
        b.ue(0);
        b.ue(1); // L0=1, L1=2
        b.bit(false);
        b.bit(true);
        b.ue(1);
        b.ue(0);
        b.ue(3); // L1 add modification
        b.ue(0);
        b.ue(0);
        for _ in 0..3 {
            b.bit(false);
            b.bit(false)
        }
        let picture = one_slice(b, &s, &p, false);
        assert_eq!(picture.slices[0].active, [1, 2]);
        assert_eq!(
            picture.slices[0].modifications[1],
            vec![Modification::Add(0)]
        );
        assert_eq!(picture.marking, Marking::NonReference);
    }
    #[test]
    fn parses_mmco5_without_guessing_past_marking_boundary() {
        let (s, p) = parameters(false, 0);
        let mut b = slice_prefix(2);
        b.bit(true);
        b.ue(5);
        b.ue(0);
        assert_eq!(
            one_slice(b, &s, &p, true).marking,
            Marking::Adaptive(vec![Operation::ForgetAll])
        );
    }
    #[test]
    fn parses_every_original_baseline_fixture_reference_header() {
        let bytes = include_bytes!("../examples/assets/videotoolbox-h264/progressive-64x50.h264");
        let mut units = Vec::new();
        let mut i = 0;
        while i + 3 < bytes.len() {
            let prefix = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if bytes[i..].starts_with(&[0, 0, 1]) {
                3
            } else {
                i += 1;
                continue;
            };
            units.push((i, i + prefix));
            i += prefix;
        }
        let s = units
            .iter()
            .enumerate()
            .find(|(_, (_, start))| bytes[*start] & 31 == 7)
            .unwrap()
            .0;
        let p = units
            .iter()
            .enumerate()
            .find(|(_, (_, start))| bytes[*start] & 31 == 8)
            .unwrap()
            .0;
        let sps = &bytes[units[s].1..units[s + 1].0];
        let pps = &bytes[units[p].1..units[p + 1].0];
        let mut frames = 0;
        for (i, &(start, payload)) in units.iter().enumerate() {
            if !matches!(bytes[payload] & 31, 1 | 5) {
                continue;
            }
            let end = units.get(i + 1).map_or(bytes.len(), |n| n.0);
            let picture = parse(bytes, &(start..end), &[0], sps, pps).unwrap();
            assert_eq!(picture.frame_num, frames % 4);
            assert_eq!(picture.max_refs, 1);
            assert_eq!(
                picture.slices[0].active,
                if frames % 4 == 0 { [0, 0] } else { [1, 0] }
            );
            if frames % 4 == 0 {
                assert!(matches!(
                    picture.marking,
                    Marking::Idr {
                        long_term: false,
                        ..
                    }
                ))
            } else {
                assert_eq!(picture.marking, Marking::Sliding)
            }
            frames += 1;
        }
        assert_eq!(frames, 8);
    }
    #[test]
    fn progressive_active_reference_limit_is_sixteen_not_field_limit_thirty_two() {
        let (s, p) = parameters(false, 0);
        let mut b = slice_prefix(0);
        b.bit(true);
        b.ue(16);
        let mut bytes = vec![0, 0, 1];
        bytes.extend(b.nal(0x41));
        assert!(
            parse(&bytes, &(0..bytes.len()), &[0], &s, &p)
                .unwrap_err()
                .contains("exceeds sixteen")
        );
    }
    fn sps_for_poc(kind: u32) -> Vec<u8> {
        let mut b = Writer::default();
        b.fixed(66, 8);
        b.fixed(0, 8);
        b.fixed(30, 8);
        b.ue(0);
        b.ue(0);
        b.ue(kind);
        if kind == 1 {
            b.bit(false);
            b.se(-1);
            b.se(2);
            b.ue(2);
            b.se(2);
            b.se(2);
        }
        b.ue(3);
        b.bit(false);
        b.ue(3);
        b.ue(3);
        b.bit(true);
        b.bit(true);
        b.bit(false);
        b.bit(false);
        b.nal(0x67)
    }
    #[test]
    fn parses_poc_type1_delta_before_reference_marking() {
        let s = sps_for_poc(1);
        let (_, p) = parameters(false, 0);
        let mut b = Writer::default();
        b.ue(0);
        b.ue(0);
        b.ue(0);
        b.fixed(3, 4);
        b.se(-2);
        b.bit(false);
        b.bit(false); // default active count and no list changes
        b.bit(true);
        b.ue(1);
        b.ue(1);
        b.ue(0);
        let parsed = one_slice(b, &s, &p, true);
        assert_eq!(parsed.poc.delta, [-2, 0]);
        assert_eq!(
            parsed.marking,
            Marking::Adaptive(vec![Operation::ForgetShort {
                difference_minus1: 1
            }])
        );
    }
    #[test]
    fn unproven_poc2_non_idr_bootstrap_stays_gated() {
        let s = sps_for_poc(2);
        let (_, p) = parameters(false, 0);
        let mut b = Writer::default();
        b.ue(0);
        b.ue(2);
        b.ue(0);
        b.fixed(3, 4);
        b.bit(false);
        let mut bytes = vec![0, 0, 1];
        bytes.extend(b.nal(0x41));
        assert!(
            independent_intra(&bytes, &(0..bytes.len()), &[0], &s, &p, 3, [6, 6])
                .unwrap_err()
                .contains("awaits hardware validation")
        );
    }
}
