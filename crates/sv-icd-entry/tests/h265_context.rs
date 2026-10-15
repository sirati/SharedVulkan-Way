//! H.265's HRD sub-layer arrays, whose length lives in the VPS or SPS above them.
//!
//! `StdVideoH265HrdParameters::pSubLayerHrdParametersNal` is
//! `len="*_max_sub_layers_minus1 + 1"`, and until docs/video-decode.md §7 no
//! lowering of it existed — so neither did one of the VPS, the SPS, or any
//! H.265 session-parameters structure, and those links fell silently out of
//! their chains. These check the count is taken from the right ancestor, that
//! exactly that many elements are read (a fourth element here would be past the
//! end of the array the "application" allocated, which Miri would report), and
//! that the HRD struct on its own — with no ancestor to give the count — is
//! refused rather than guessed at.
//!
//! Under Miri as well: `cargo +nightly miri test -p sv-icd-entry --test h265_context`.

// A Vulkan Std struct is `Default` plus the two or three fields under test, as
// in `lowering.rs`.
#![allow(clippy::field_reassign_with_default)]

use sv_icd_entry::*;

fn sub_layers(base: u32) -> [StdVideoH265SubLayerHrdParameters; 3] {
    let mut s = [
        StdVideoH265SubLayerHrdParameters::default(),
        StdVideoH265SubLayerHrdParameters::default(),
        StdVideoH265SubLayerHrdParameters::default(),
    ];
    for (i, l) in s.iter_mut().enumerate() {
        l.bit_rate_value_minus1[0] = base + i as u32;
        l.cbr_flag = 0x10 + i as u32;
    }
    s
}

#[test]
fn the_vps_gives_its_hrd_arrays_their_length() {
    let nal = sub_layers(1000);
    let vcl = sub_layers(2000);
    let mut hrd = StdVideoH265HrdParameters::default();
    hrd.p_sub_layer_hrd_parameters_nal = nal.as_ptr();
    hrd.p_sub_layer_hrd_parameters_vcl = vcl.as_ptr();
    let mut vps = StdVideoH265VideoParameterSet::default();
    vps.vps_max_sub_layers_minus1 = 2;
    vps.p_hrd_parameters = &hrd;
    // SAFETY: every pointer names a live local of the right type and extent.
    let w = unsafe { lower_std_video_h265_video_parameter_set(&vps) }.expect("lowers");
    let h = w.p_hrd_parameters.expect("the HRD parameters crossed");
    assert_eq!(h.p_sub_layer_hrd_parameters_nal.len(), 3);
    assert_eq!(h.p_sub_layer_hrd_parameters_vcl.len(), 3);
    for i in 0..3 {
        assert_eq!(h.p_sub_layer_hrd_parameters_nal[i].bit_rate_value_minus1[0], 1000 + i as u32);
        assert_eq!(h.p_sub_layer_hrd_parameters_vcl[i].bit_rate_value_minus1[0], 2000 + i as u32);
        assert_eq!(h.p_sub_layer_hrd_parameters_nal[i].cbr_flag, 0x10 + i as u32);
    }
}

#[test]
fn the_sps_gives_its_vuis_hrd_arrays_their_length_two_levels_down() {
    let nal = sub_layers(7);
    let mut hrd = StdVideoH265HrdParameters::default();
    hrd.p_sub_layer_hrd_parameters_nal = nal.as_ptr();
    let mut vui = StdVideoH265SequenceParameterSetVui::default();
    vui.p_hrd_parameters = &hrd;
    let mut sps = StdVideoH265SequenceParameterSet::default();
    // Two sub-layers: only the first two of the three elements may be read.
    sps.sps_max_sub_layers_minus1 = 1;
    sps.p_sequence_parameter_set_vui = &vui;
    // SAFETY: as above.
    let w = unsafe { lower_std_video_h265_sequence_parameter_set(&sps) }.expect("lowers");
    let h = w
        .p_sequence_parameter_set_vui
        .and_then(|v| v.p_hrd_parameters)
        .expect("the VUI's HRD parameters crossed");
    assert_eq!(h.p_sub_layer_hrd_parameters_nal.len(), 2);
    assert!(h.p_sub_layer_hrd_parameters_vcl.is_empty(), "a null array is empty");
    assert_eq!(h.p_sub_layer_hrd_parameters_nal[1].bit_rate_value_minus1[0], 8);
}

#[test]
fn an_hrd_with_no_ancestor_to_count_for_it_is_refused() {
    let nal = sub_layers(0);
    let mut hrd = StdVideoH265HrdParameters::default();
    hrd.p_sub_layer_hrd_parameters_nal = nal.as_ptr();
    // SAFETY: as above.
    let r = unsafe { lower_std_video_h265_hrd_parameters(&hrd) };
    assert!(matches!(r, Err(LowerError::Shape("h265_max_sub_layers"))), "{r:?}");
}
