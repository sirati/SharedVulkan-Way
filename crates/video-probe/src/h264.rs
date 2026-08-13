//! Enough H.264 to fill in the `StdVideo*` structures Vulkan Video wants.
//!
//! Vulkan Video does not parse. `vkCmdDecodeVideoKHR` takes the slice data as
//! bytes and takes everything above the slice — the sequence and picture
//! parameter sets, the picture's frame number and picture order count — as
//! filled-in C structures. So a decoder over Vulkan Video is a bitstream parser
//! plus a driver call, and the parser is the part that has to exist here.
//!
//! This is deliberately the smallest parser that can be honest about what it
//! does not handle: it refuses scaling matrices, slice groups and field
//! coding rather than ignoring them, because a parser that quietly skips a
//! syntax element produces a picture that is wrong in a way no return code
//! reports. Everything it refuses is listed in `docs/video-decode.md`.

use anyhow::{Result, bail};

/// A reader over an RBSP — the NAL payload with emulation-prevention bytes
/// already removed.
pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn u1(&mut self) -> Result<u32> {
        let byte = self.pos >> 3;
        if byte >= self.data.len() {
            bail!("bitstream ended inside a syntax element");
        }
        let bit = 7 - (self.pos & 7);
        self.pos += 1;
        Ok(((self.data[byte] >> bit) & 1) as u32)
    }

    pub fn u(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            bail!("u({n}) is wider than this reader");
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.u1()?;
        }
        Ok(v)
    }

    /// Unsigned exp-Golomb, 9.1.
    pub fn ue(&mut self) -> Result<u32> {
        let mut leading = 0u32;
        while self.u1()? == 0 {
            leading += 1;
            if leading > 32 {
                bail!("exp-Golomb code longer than 32 leading zeroes — desynchronised");
            }
        }
        if leading == 0 {
            return Ok(0);
        }
        let rest = self.u(leading)?;
        Ok((1u32 << leading) - 1 + rest)
    }

    /// Signed exp-Golomb, 9.1.1.
    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()?;
        let k64 = i64::from(k);
        Ok((if k % 2 == 1 { (k64 + 1) / 2 } else { -(k64 / 2) }) as i32)
    }

    /// 7.2's `more_rbsp_data()`: true if anything but the stop bit and its
    /// trailing zeroes remains.
    pub fn more_rbsp_data(&self) -> bool {
        let total = self.data.len() * 8;
        if self.pos >= total {
            return false;
        }
        // Find the last set bit in the buffer; that is the rbsp_stop_one_bit.
        let mut last = None;
        for i in (0..total).rev() {
            let b = (self.data[i >> 3] >> (7 - (i & 7))) & 1;
            if b == 1 {
                last = Some(i);
                break;
            }
        }
        match last {
            Some(stop) => self.pos < stop,
            None => false,
        }
    }
}

/// Strip emulation-prevention bytes: `00 00 03 xx` becomes `00 00 xx`.
pub fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeroes = 0;
    for &b in nal {
        if zeroes >= 2 && b == 3 {
            zeroes = 0;
            continue;
        }
        if b == 0 {
            zeroes += 1;
        } else {
            zeroes = 0;
        }
        out.push(b);
    }
    out
}

/// One NAL unit located inside an Annex B stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nal {
    /// Offset of the start code, which is what Vulkan wants in `pSliceOffsets`.
    pub start: usize,
    /// Offset of the byte after the NAL header, i.e. the first payload byte.
    pub payload: usize,
    /// One past the last payload byte.
    pub end: usize,
    pub nal_ref_idc: u8,
    pub nal_unit_type: u8,
}

/// Split an Annex B elementary stream into NAL units.
///
/// The offsets are into the original buffer, start codes included, because that
/// is what `VkVideoDecodeH264PictureInfoKHR::pSliceOffsets` means: an offset
/// into the bitstream buffer at which the slice's NAL unit begins, start code
/// and all.
pub fn split_annex_b(data: &[u8]) -> Vec<Nal> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let sc = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((sc, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &(sc, hdr)) in starts.iter().enumerate() {
        if hdr >= data.len() {
            continue;
        }
        let end = starts.get(n + 1).map(|&(s, _)| s).unwrap_or(data.len());
        let b = data[hdr];
        out.push(Nal {
            start: sc,
            payload: hdr + 1,
            end,
            nal_ref_idc: (b >> 5) & 3,
            nal_unit_type: b & 0x1f,
        });
    }
    out
}

#[derive(Debug, Clone)]
pub struct Sps {
    pub id: u8,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub max_num_ref_frames: u8,
    pub pic_width_in_mbs_minus1: u32,
    pub pic_height_in_map_units_minus1: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub qpprime_y_zero_transform_bypass_flag: bool,
    pub separate_colour_plane_flag: bool,
    pub delta_pic_order_always_zero_flag: bool,
    pub frame_cropping_flag: bool,
    pub crop: [u32; 4], // left, right, top, bottom
    pub constraint_set: [bool; 6],
}

impl Sps {
    /// Coded luma size in samples, before cropping.
    pub fn coded_size(&self) -> (u32, u32) {
        let w = (self.pic_width_in_mbs_minus1 + 1) * 16;
        let h = (self.pic_height_in_map_units_minus1 + 1) * 16
            * if self.frame_mbs_only_flag { 1 } else { 2 };
        (w, h)
    }

    /// Displayed luma size, after the frame cropping rectangle.
    pub fn display_size(&self) -> (u32, u32) {
        let (w, h) = self.coded_size();
        if !self.frame_cropping_flag {
            return (w, h);
        }
        // 7.4.2.1.1: `CropUnitY` is `SubHeightC * (2 - frame_mbs_only_flag)`,
        // so it doubles for a field-coded stream. For 4:2:0 the units are 2
        // and 2 in a progressive stream.
        let fields = if self.frame_mbs_only_flag { 1 } else { 2 };
        let (cx, cy) = match self.chroma_format_idc {
            1 => (2, 2 * fields),
            2 => (2, fields),
            _ => (1, fields),
        };
        (
            w - cx * (self.crop[0] + self.crop[1]),
            h - cy * (self.crop[2] + self.crop[3]),
        )
    }
}

pub fn parse_sps(rbsp: &[u8]) -> Result<Sps> {
    let mut b = Bits::new(rbsp);
    let profile_idc = b.u(8)? as u8;
    let mut constraint_set = [false; 6];
    for c in &mut constraint_set {
        *c = b.u1()? == 1;
    }
    let _reserved = b.u(2)?;
    let level_idc = b.u(8)? as u8;
    let id = b.ue()? as u8;

    let mut chroma_format_idc = 1;
    let mut separate_colour_plane_flag = false;
    let mut bit_depth_luma_minus8 = 0;
    let mut bit_depth_chroma_minus8 = 0;
    let mut qpprime = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = b.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane_flag = b.u1()? == 1;
        }
        bit_depth_luma_minus8 = b.ue()? as u8;
        bit_depth_chroma_minus8 = b.ue()? as u8;
        qpprime = b.u1()? == 1;
        if b.u1()? == 1 {
            bail!("this SPS carries a scaling matrix, which this parser refuses rather than ignores");
        }
    }

    let log2_max_frame_num_minus4 = b.ue()? as u8;
    let pic_order_cnt_type = b.ue()? as u8;
    let mut log2_max_pic_order_cnt_lsb_minus4 = 0;
    let delta_pic_order_always_zero_flag = false;
    match pic_order_cnt_type {
        0 => log2_max_pic_order_cnt_lsb_minus4 = b.ue()? as u8,
        1 => bail!("pic_order_cnt_type 1 needs an offset table this crate does not carry"),
        2 => {}
        other => bail!("pic_order_cnt_type {other} is not a thing"),
    }
    let max_num_ref_frames = b.ue()? as u8;
    let gaps = b.u1()? == 1;
    let pic_width_in_mbs_minus1 = b.ue()?;
    let pic_height_in_map_units_minus1 = b.ue()?;
    let frame_mbs_only_flag = b.u1()? == 1;
    let mut mb_adaptive_frame_field_flag = false;
    if !frame_mbs_only_flag {
        mb_adaptive_frame_field_flag = b.u1()? == 1;
    }
    let direct_8x8_inference_flag = b.u1()? == 1;
    let frame_cropping_flag = b.u1()? == 1;
    let mut crop = [0u32; 4];
    if frame_cropping_flag {
        for c in &mut crop {
            *c = b.ue()?;
        }
    }
    // VUI is skipped on purpose: nothing below the slice layer reads it, and
    // `StdVideoH264SequenceParameterSet::pSequenceParameterSetVui` is allowed
    // to be NULL as long as the flag says so.

    Ok(Sps {
        id,
        profile_idc,
        level_idc,
        chroma_format_idc,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        log2_max_frame_num_minus4,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4,
        max_num_ref_frames,
        pic_width_in_mbs_minus1,
        pic_height_in_map_units_minus1,
        frame_mbs_only_flag,
        mb_adaptive_frame_field_flag,
        direct_8x8_inference_flag,
        gaps_in_frame_num_value_allowed_flag: gaps,
        qpprime_y_zero_transform_bypass_flag: qpprime,
        separate_colour_plane_flag,
        delta_pic_order_always_zero_flag,
        frame_cropping_flag,
        crop,
        constraint_set,
    })
}

#[derive(Debug, Clone)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    pub transform_8x8_mode_flag: bool,
}

pub fn parse_pps(rbsp: &[u8]) -> Result<Pps> {
    let mut b = Bits::new(rbsp);
    let id = b.ue()? as u8;
    let sps_id = b.ue()? as u8;
    let entropy_coding_mode_flag = b.u1()? == 1;
    let bottom_field_pic_order_in_frame_present_flag = b.u1()? == 1;
    let num_slice_groups_minus1 = b.ue()?;
    if num_slice_groups_minus1 > 0 {
        bail!("slice groups (FMO) are not handled");
    }
    let num_ref_idx_l0_default_active_minus1 = b.ue()? as u8;
    let num_ref_idx_l1_default_active_minus1 = b.ue()? as u8;
    let weighted_pred_flag = b.u1()? == 1;
    let weighted_bipred_idc = b.u(2)? as u8;
    let pic_init_qp_minus26 = b.se()? as i8;
    let pic_init_qs_minus26 = b.se()? as i8;
    let chroma_qp_index_offset = b.se()? as i8;
    let deblocking_filter_control_present_flag = b.u1()? == 1;
    let constrained_intra_pred_flag = b.u1()? == 1;
    let redundant_pic_cnt_present_flag = b.u1()? == 1;

    let mut transform_8x8_mode_flag = false;
    let mut second_chroma_qp_index_offset = chroma_qp_index_offset;
    if b.more_rbsp_data() {
        transform_8x8_mode_flag = b.u1()? == 1;
        if b.u1()? == 1 {
            bail!("this PPS carries a scaling matrix, which this parser refuses rather than ignores");
        }
        second_chroma_qp_index_offset = b.se()? as i8;
    }

    Ok(Pps {
        id,
        sps_id,
        entropy_coding_mode_flag,
        bottom_field_pic_order_in_frame_present_flag,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        weighted_pred_flag,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        pic_init_qs_minus26,
        chroma_qp_index_offset,
        second_chroma_qp_index_offset,
        deblocking_filter_control_present_flag,
        constrained_intra_pred_flag,
        redundant_pic_cnt_present_flag,
        transform_8x8_mode_flag,
    })
}

/// The part of the slice header the picture info needs.
#[derive(Debug, Clone, Copy)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type: u32,
    pub pps_id: u8,
    pub frame_num: u16,
    pub field_pic_flag: bool,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u32,
}

pub fn parse_slice_header(rbsp: &[u8], idr: bool, sps: &Sps, pps: &Pps) -> Result<SliceHeader> {
    let mut b = Bits::new(rbsp);
    let first_mb_in_slice = b.ue()?;
    let slice_type = b.ue()?;
    let pps_id = b.ue()? as u8;
    if sps.separate_colour_plane_flag {
        let _colour_plane_id = b.u(2)?;
    }
    let frame_num = b.u(u32::from(sps.log2_max_frame_num_minus4) + 4)? as u16;
    let mut field_pic_flag = false;
    if !sps.frame_mbs_only_flag {
        field_pic_flag = b.u1()? == 1;
        if field_pic_flag {
            bail!("field coding is not handled");
        }
    }
    let mut idr_pic_id = 0;
    if idr {
        idr_pic_id = b.ue()? as u16;
    }
    let mut pic_order_cnt_lsb = 0;
    if sps.pic_order_cnt_type == 0 {
        pic_order_cnt_lsb = b.u(u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4)?;
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            let _delta_pic_order_cnt_bottom = b.se()?;
        }
    }
    Ok(SliceHeader {
        first_mb_in_slice,
        slice_type,
        pps_id,
        frame_num,
        field_pic_flag,
        idr_pic_id,
        pic_order_cnt_lsb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a bit string, spaces ignored, into bytes padded with zeroes.
    ///
    /// The test data below is a sequence of variable-length codes, so writing it
    /// as byte literals means grouping the bits by eight and losing the one
    /// thing a reader needs to see — where each code begins and ends.
    fn bits(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        let (mut acc, mut n) = (0u8, 0);
        for c in s.chars().filter(|c| *c != ' ') {
            acc = (acc << 1) | u8::from(c == '1');
            n += 1;
            if n == 8 {
                v.push(acc);
                acc = 0;
                n = 0;
            }
        }
        if n > 0 {
            v.push(acc << (8 - n));
        }
        v
    }

    /// The five shortest codes in table 9-1, in order.
    const FIVE_CODES: &str = "1 010 011 00100 00101";

    #[test]
    fn exp_golomb_matches_the_table_in_9_1() {
        let data = bits(FIVE_CODES);
        let mut b = Bits::new(&data);
        assert_eq!(b.ue().unwrap(), 0);
        assert_eq!(b.ue().unwrap(), 1);
        assert_eq!(b.ue().unwrap(), 2);
        assert_eq!(b.ue().unwrap(), 3);
        assert_eq!(b.ue().unwrap(), 4);
    }

    #[test]
    fn signed_exp_golomb_alternates_around_zero() {
        // The same five codes read as `se`: 0, 1, 2, 3, 4 become 0, 1, -1, 2, -2.
        let data = bits(FIVE_CODES);
        let mut b = Bits::new(&data);
        assert_eq!(b.se().unwrap(), 0);
        assert_eq!(b.se().unwrap(), 1);
        assert_eq!(b.se().unwrap(), -1);
        assert_eq!(b.se().unwrap(), 2);
        assert_eq!(b.se().unwrap(), -2);
    }

    #[test]
    fn a_desynchronised_reader_errors_rather_than_looping() {
        let data = [0u8; 8];
        let mut b = Bits::new(&data);
        assert!(b.ue().is_err());
    }

    #[test]
    fn running_off_the_end_is_an_error() {
        let data = [0xffu8];
        let mut b = Bits::new(&data);
        assert_eq!(b.u(8).unwrap(), 0xff);
        assert!(b.u1().is_err());
    }

    #[test]
    fn emulation_prevention_bytes_are_removed_and_nothing_else_is() {
        assert_eq!(rbsp(&[0, 0, 3, 1]), vec![0, 0, 1]);
        assert_eq!(rbsp(&[0, 0, 3, 0]), vec![0, 0, 0]);
        assert_eq!(rbsp(&[0, 0, 3, 3]), vec![0, 0, 3]);
        // A 03 that is not preceded by two zeroes stays.
        assert_eq!(rbsp(&[1, 3, 4]), vec![1, 3, 4]);
        // Three zeroes then 03: the counter resets after the escape.
        assert_eq!(rbsp(&[0, 0, 0, 3, 1]), vec![0, 0, 0, 1]);
    }

    #[test]
    fn annex_b_splitting_finds_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 0xaa, // SPS, four-byte start code
            0, 0, 1, 0x68, 0xbb, // PPS, three-byte
            0, 0, 0, 1, 0x65, 0xcc, 0xdd, // IDR slice
        ];
        let nals = split_annex_b(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0].nal_unit_type, 7);
        assert_eq!(nals[0].start, 0);
        assert_eq!(nals[1].nal_unit_type, 8);
        assert_eq!(nals[1].start, 6);
        assert_eq!(nals[2].nal_unit_type, 5);
        assert_eq!(nals[2].start, 11);
        assert_eq!(nals[2].end, data.len());
        assert_eq!(nals[2].nal_ref_idc, 3);
    }

    /// A start code preceded by a zero belongs to the four-byte form, and
    /// getting that wrong shifts every slice offset by one byte — which a
    /// driver reports as a corrupt picture and not as an error.
    #[test]
    fn a_leading_zero_is_claimed_by_the_start_code_that_follows_it() {
        let data = [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68];
        let nals = split_annex_b(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[1].start, 5);
    }

    /// A hand-built SPS, so the parser is checked against something whose
    /// values are known rather than against whatever ffmpeg happened to write.
    /// Baseline, 16x16, one reference frame, no cropping.
    #[test]
    fn a_hand_built_baseline_sps_round_trips() {
        // profile_idc=66, constraint 000000, reserved 00, level_idc=30,
        // sps_id=0 (ue:1), log2_max_frame_num_minus4=0 (1),
        // pic_order_cnt_type=0 (1), log2_max_poc_lsb_minus4=0 (1),
        // max_num_ref_frames=1 (010), gaps=0, width_mbs_minus1=0 (1),
        // height_map_units_minus1=0 (1), frame_mbs_only=1,
        // direct_8x8=1, cropping=0, vui=0
        let mut v = vec![66u8, 0u8, 30u8];
        v.extend(bits("1 1 1 1 010 0 1 1 1 1 0 0"));
        let sps = parse_sps(&v).unwrap();
        assert_eq!(sps.profile_idc, 66);
        assert_eq!(sps.level_idc, 30);
        assert_eq!(sps.id, 0);
        assert_eq!(sps.max_num_ref_frames, 1);
        assert_eq!(sps.chroma_format_idc, 1);
        assert!(sps.frame_mbs_only_flag);
        assert_eq!(sps.coded_size(), (16, 16));
        assert_eq!(sps.display_size(), (16, 16));
    }

    #[test]
    fn cropping_is_applied_in_chroma_units() {
        let mut sps = Sps {
            id: 0,
            profile_idc: 100,
            level_idc: 30,
            chroma_format_idc: 1,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 0,
            max_num_ref_frames: 1,
            pic_width_in_mbs_minus1: 19, // 320
            pic_height_in_map_units_minus1: 11, // 192
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: false,
            direct_8x8_inference_flag: true,
            gaps_in_frame_num_value_allowed_flag: false,
            qpprime_y_zero_transform_bypass_flag: false,
            separate_colour_plane_flag: false,
            delta_pic_order_always_zero_flag: false,
            frame_cropping_flag: true,
            crop: [0, 0, 0, 6], // 192 - 2*6 = 180
            constraint_set: [false; 6],
        };
        assert_eq!(sps.coded_size(), (320, 192));
        assert_eq!(sps.display_size(), (320, 180));
        sps.frame_cropping_flag = false;
        assert_eq!(sps.display_size(), (320, 192));
    }

    #[test]
    fn more_rbsp_data_stops_at_the_stop_bit() {
        // 1000_0000: one bit of payload then the stop bit at position 0.
        let d = [0b1000_0000u8];
        let b = Bits::new(&d);
        assert!(!b.more_rbsp_data(), "the stop bit alone is not more data");
        // 1100_0000: payload bit then stop bit; after reading one bit there is
        // nothing left.
        let d = [0b1100_0000u8];
        let mut b = Bits::new(&d);
        assert!(b.more_rbsp_data());
        b.u1().unwrap();
        assert!(!b.more_rbsp_data());
    }
}
