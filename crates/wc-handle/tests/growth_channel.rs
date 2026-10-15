//! `reserve_growth_channel`: the front of the outbound window becomes the
//! growth channel's control region, and the gift allocator stops being able to
//! reach it.
//!
//! Its own file because the outbound window is a process-global singleton — the
//! same reason `outbound_tail.rs` and `outbound_own_region.rs` are each their
//! own. Both checks here run inside one test for the same reason: they need the
//! same window, in a known order, and two `#[test]`s in one binary would race
//! for it.

/// The bytes the channel takes stop being giveable; the ones after it do not;
/// and a second reservation is refused.
///
/// The free-list assertion is the one that matters and it is not visible in the
/// return value. `reserve_growth_channel` answers `(offset, len)`, and an
/// implementation that answered correctly while forgetting to shorten the free
/// list would pass any test of its return value — and would hand a client a
/// keymap on top of a live ring, which presents as a growth channel that stops
/// answering some time after an unrelated client connected.
#[test]
fn the_channel_takes_the_front_and_gifts_come_from_after_it() {
    // §85: no hypervisor here, so nothing outside can write a moved page; and
    // this machine reserves no hugetlb pages. Without this the commit below
    // is refused and the test would skip instead of running.
    wc_handle::arena::allow_small_pages_that_may_corrupt_guest_memory();
    const ARENA: u64 = 4 << 20;
    // Comfortably more than `CONTROL_BYTES`, so a window is left over.
    const WINDOW: u64 = 1 << 20;
    let id = 9_601;
    if wc_handle::arena::attach_guest_ram(id, ARENA).is_err() {
        eprintln!("could not commit {ARENA} bytes here");
        return;
    }
    let w = wc_handle::arena::reserve_outbound(id, WINDOW).expect("a tail window");

    let (offset, len) = wc_handle::arena::reserve_growth_channel(id).expect("the channel");
    assert_eq!(offset, w.offset, "the channel is the *front* of the window");
    assert_eq!(len, wc_handle::grow::CONTROL_BYTES);

    // The first gift must land after the channel, not on top of it.
    let gift = wc_handle::arena::give(4096).expect("a gift out of what is left");
    let (arena, gift_off, gift_len) = gift.extent();
    assert_eq!(arena, id);
    assert!(
        gift_off >= offset + len,
        "a gift at {gift_off} landed inside the growth channel at {offset}+{len}"
    );
    assert!(gift_off + gift_len <= w.offset + w.len, "and still inside the window");

    // And a second reservation is refused — by then a gift has been made, which
    // is the case the refusal actually protects. The channel is the front of
    // the window and the free list is a sorted run list with no way to express
    // a hole, so there is nowhere for a second one to go.
    let e = wc_handle::arena::reserve_growth_channel(id).expect_err("only one channel");
    assert!(
        e.to_string().contains("before anything is given"),
        "the message has to name the ordering rather than just say no: {e}"
    );
}
