//! The pre-split spelling: an outbound window that is a tail of a region the
//! process already committed.
//!
//! Its own file because the outbound window is a process-global singleton — see
//! `outbound_own_region.rs`.
//!
//! This is not a compatibility nicety. The launcher is being changed
//! independently of this work and old plans still read, so **both spellings are
//! live at once** and both have to keep meaning what they mean.

/// A committed id still takes a tail, still at the same offset, and still
/// reports no run id — because nothing new was committed, so there is nothing
/// for the host's locator to be told about.
#[test]
fn a_committed_id_still_takes_a_tail_of_that_region() {
    // §85: no hypervisor here, so nothing outside can write a moved page; and
    // this machine reserves no hugetlb pages. Without this the commit below
    // is refused and the test would skip instead of running.
    wc_handle::arena::allow_small_pages_that_may_corrupt_guest_memory();
    const ARENA: u64 = 1 << 20;
    const WINDOW: u64 = 64 << 10;
    let id = 9_501;
    if wc_handle::arena::attach_guest_ram(id, ARENA).is_err() {
        eprintln!("could not commit {ARENA} bytes here");
        return;
    }
    let w = wc_handle::arena::reserve_outbound(id, WINDOW).expect("a tail window");
    assert_eq!(w.arena, id);
    assert_eq!(w.offset, ARENA - WINDOW, "the tail, as it always was");
    assert_eq!(w.len, WINDOW);
    assert_eq!(w.run_id, None, "nothing new was committed, so nothing to report");
}
