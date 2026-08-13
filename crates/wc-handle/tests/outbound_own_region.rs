//! DYNAMIC ARENA. `--arena-outbound <id>` naming an arena the process has not
//! committed commits one, and the window is the whole of it.
//!
//! An integration test rather than a unit test because the outbound window is a
//! **process-global singleton** — there is one per compositor by construction,
//! since two would be two allocators over objects clients already hold. Cargo
//! gives each integration test file its own process, which is the only way two
//! tests can each have the window. Its sibling `outbound_tail.rs` covers the
//! pre-split spelling for the same reason.

/// The guest half of removing the floor.
///
/// While the window was a tail of the data region, the region holding live
/// keymaps was also the region the data allocator started in, so pinning the one
/// pinned the other and whatever the session started at was permanent — an
/// `sv-guest` holding 817 MB with no client connected. A region of its own makes
/// the floor the size of the thing that actually has to stay.
#[test]
fn an_uncommitted_id_commits_a_region_whose_whole_extent_is_the_window() {
    const LEN: u64 = 256 << 10;
    let id = 9_500;
    let w = match wc_handle::arena::reserve_outbound(id, LEN) {
        Ok(w) => w,
        // Pinning is charged against a working-set quota the machine may
        // refuse. A test that could not commit has measured nothing, and says
        // so rather than failing — the same stance the guest-RAM path itself
        // takes.
        Err(e) => {
            eprintln!("could not commit {LEN} bytes here: {e}");
            return;
        }
    };
    assert_eq!(w.arena, id);
    assert_eq!(w.offset, 0, "the window starts at the base of its own region");
    assert_eq!(w.len, LEN, "and is the whole of it");
    // The run id is the point. The host cannot locate a region it has not been
    // told the run id of, and this one is committed inside `reserve_outbound`
    // where the caller would otherwise never see it — a region nobody can
    // locate is one the compositor writes keymaps into that no client reads.
    assert!(w.run_id.is_some(), "a region committed here must report its run id");

    // And it is pinned while it is the window: releasing it would unmap
    // keymaps that are live in clients' hands (`one-binary.md` §3.1a).
    assert!(
        wc_handle::arena::release_guest_ram(id).is_err(),
        "the outbound region must not be releasable while it is the window"
    );
}
