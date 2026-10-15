//! A two-dimensional fixed array, lowered element for element.
//!
//! `StdVideoH264ScalingLists::ScalingList4x4` is `uint8_t[6][16]`, and until
//! docs/video-decode.md §7 the generated loops for both dimensions were named
//! `__i`: the inner one shadowed the outer, every read was `[__i][__i]` with
//! the inner index, and the seventh read of the first dimension panicked out of
//! bounds — inside `vkCmdDecodeVideoKHR`'s lowering, under ffmpeg. Every element
//! here has a distinct value, so a transposition or a repeated row fails too,
//! not only the out-of-bounds read.
//!
//! Under Miri as well: `cargo +nightly miri test -p sv-icd-entry --test fixed_arrays`.

use sv_icd_entry::*;

#[test]
fn a_two_dimensional_scaling_list_lowers_every_element_in_place() {
    let mut c = StdVideoH264ScalingLists::default();
    for i in 0..6 {
        for j in 0..16 {
            c.scaling_list4x4[i][j] = (i * 16 + j) as u8;
        }
    }
    for i in 0..2 {
        for j in 0..64 {
            c.scaling_list8x8[i][j] = (100 + i * 64 + j) as u8;
        }
    }
    c.scaling_list_present_mask = 0x0abc;
    // SAFETY: `c` is a live, fully initialised `#[repr(C)]` value.
    let w = unsafe { lower_std_video_h264_scaling_lists(&c) }.expect("lowers");
    for i in 0..6 {
        for j in 0..16 {
            assert_eq!(w.scaling_list4x4[i][j], (i * 16 + j) as u8, "4x4 [{i}][{j}]");
        }
    }
    for i in 0..2 {
        for j in 0..64 {
            assert_eq!(w.scaling_list8x8[i][j], (100 + i * 64 + j) as u8, "8x8 [{i}][{j}]");
        }
    }
    assert_eq!(w.scaling_list_present_mask, 0x0abc);
}
