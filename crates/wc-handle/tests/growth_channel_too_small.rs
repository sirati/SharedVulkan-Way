//! A window too small to hold the growth channel is refused where the number
//! is, rather than discovered later as a keymap that will not fit.
//!
//! Its own file because it needs a *small* outbound window and
//! `growth_channel.rs` needs a large one, and the window is a process-global
//! singleton — so they cannot be two tests in one binary.
//!
//! This is also the case that decided the design. Carving the channel out of
//! `reserve_outbound` itself imposed this 272 KiB minimum on every caller, and
//! three existing tests — reserving 64 KiB windows to exercise a keymap, which
//! do not grow and never will — failed. That is why the channel is opt-in and
//! its own call.

#[test]
fn a_window_smaller_than_the_channel_is_refused() {
    const ARENA: u64 = 1 << 20;
    const WINDOW: u64 = 64 << 10;
    let id = 9_603;
    if wc_handle::arena::attach_guest_ram(id, ARENA).is_err() {
        eprintln!("could not commit {ARENA} bytes here");
        return;
    }
    let _w = wc_handle::arena::reserve_outbound(id, WINDOW).expect("a tail window");
    assert!(
        WINDOW < wc_handle::grow::CONTROL_BYTES,
        "this test is only meaningful while the window is under the channel's size"
    );
    let e = wc_handle::arena::reserve_growth_channel(id).expect_err("too small");
    assert!(e.to_string().contains("cannot hold"), "{e}");

    // And the window is untouched: a refused reservation must not have taken
    // anything, or a compositor that asked and was told no would then hand out
    // gifts from the wrong place.
    let gift = wc_handle::arena::give(4096).expect("a gift out of the whole window");
    let (_, off, _) = gift.extent();
    assert_eq!(off, _w.offset, "the first gift still starts at the window's own front");
}
