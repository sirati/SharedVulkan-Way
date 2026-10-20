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
struct Sps {
    id: u32,
    frame_bits: u32,
    poc: u32,
    poc_bits: u32,
}
fn sps(nal: &[u8]) -> Result<Sps, String> {
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
                            next = (last + b.se()? + 256).rem_euclid(256)
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
    match poc {
        0 => {
            poc_bits = b
                .ue()?
                .checked_add(4)
                .filter(|n| *n <= 16)
                .ok_or("invalid POC width")?
        }
        1 => {
            b.read(1)?;
            b.se()?;
            b.se()?;
            let count = b.ue()?;
            if count > 255 {
                return Err("invalid POC cycle".into());
            }
            for _ in 0..count {
                b.se()?;
            }
        }
        2 => {}
        _ => return Err("invalid POC type".into()),
    }
    b.ue()?;
    b.read(1)?;
    b.ue()?;
    b.ue()?;
    if b.read(1)? != 1 {
        return Err("requires progressive SPS".into());
    }
    Ok(Sps {
        id,
        frame_bits,
        poc,
        poc_bits,
    })
}
struct Pps {
    id: u32,
    bottom_poc: bool,
    redundant: bool,
}
fn pps(nal: &[u8], sps: &Sps) -> Result<Pps, String> {
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
    b.ue()?;
    b.ue()?;
    b.read(3)?;
    b.se()?;
    b.se()?;
    b.se()?;
    b.read(2)?;
    let redundant = b.read(1)? != 0;
    Ok(Pps {
        id,
        bottom_poc,
        redundant,
    })
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
            2 => {
                let value = 2 * i32::from(frame_num) - i32::from(nal[0] & 96 == 0);
                [value, value]
            }
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
}
