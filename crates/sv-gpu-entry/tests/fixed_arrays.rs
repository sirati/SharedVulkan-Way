//! The GPU side's `raise` of a two-dimensional fixed array, element for
//! element — the other half of `sv-icd-entry`'s `tests/fixed_arrays.rs`, and
//! the same shadowed loop index (docs/video-decode.md §7).

#[test]
fn a_two_dimensional_scaling_list_raises_every_element_in_place() {
    let mut w = sv_proto::StdVideoH264ScalingLists::default();
    for i in 0..6 {
        for j in 0..16 {
            w.scaling_list4x4[i][j] = (i * 16 + j) as u8;
        }
    }
    for i in 0..2 {
        for j in 0..64 {
            w.scaling_list8x8[i][j] = (100 + i * 64 + j) as u8;
        }
    }
    let mut pool = sv_gpu_entry::Raise::default();
    let c = sv_gpu_entry::raise_std_video_h264_scaling_lists(&w, &mut pool).expect("raises");
    for i in 0..6 {
        for j in 0..16 {
            assert_eq!(c.scaling_list4x4[i][j], (i * 16 + j) as u8, "4x4 [{i}][{j}]");
        }
    }
    for i in 0..2 {
        for j in 0..64 {
            assert_eq!(c.scaling_list8x8[i][j], (100 + i * 64 + j) as u8, "8x8 [{i}][{j}]");
        }
    }
}
