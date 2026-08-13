//! Committing another region of the guest's RAM **on demand**, and saying
//! where its pages are.
//!
//! GROWTH. This is the half of dynamic arena growth that had never been
//! written, and its absence is why growth worked over the file fixture and was
//! inert over the substrate the project exists for.
//!
//! # The direction, which is the whole difficulty
//!
//! The side that runs out of room is the **host**: `wc-proxy`'s allocator is
//! the one that cannot place a 4.6 MB pool. The side that can produce more
//! memory is the **guest**, and only the guest — waycompose §12.2, "the side
//! inside the VM allocates, because only it can produce pages the hypervisor
//! can locate". So growth needs a signal that travels host to guest, which is
//! the direction nothing in this project had ever needed before: the arena
//! handshake is guest to host (the guest commits, stamps, and says a 64-bit
//! number), and the launcher's ssh channel is guest to host as well, because
//! the guest is the ssh *client* and the host key is behind a forced command.
//!
//! # What this module does when asked
//!
//! Three things, in an order that matters:
//!
//! 1. [`wc_handle::arena::attach_guest_ram`] — `VirtualAlloc` + `VirtualLock`,
//!    every page stamped with a fresh run id. Registered under the id the
//!    *host's allocator* chose, because that number is what every
//!    `Descriptor::ArenaSlice { arena, .. }` naming the new region will carry.
//! 2. [`frames`] — `SuperfetchPfnQuery`, so the host can locate the region by
//!    translating a frame list instead of searching guest RAM for the stamps.
//! 3. Answer, with the run id, the page count and the frame list.
//!
//! Nothing is written into the region here, and that is the ordering hazard
//! paid attention to rather than got away with. A grown region is located
//! *after* it is committed and *before* the host allocates anything into it,
//! so its stamps are intact at the one moment they are read — which is the same
//! argument [`wc_handle::arena::defer_gifts`] makes for the startup region, in
//! a case where it happens to hold without any deferral being needed.
//!
//! # Why the frame list is produced here and not asked for later
//!
//! Because the host cannot ask. `SuperfetchPfnQuery` is a *reverse* lookup over
//! the guest kernel's page-frame database, keyed by the virtual address of the
//! window in the process that owns it; there is no host-side equivalent and no
//! way to run it from outside the VM. It has to be taken on the same side that
//! made the allocation, in the same call, and travel with the answer.
//!
//! And it is the reason growth is shippable at all. The content sweep costs
//! **what the guest costs, not what the region costs** — measured at 2832–3101
//! ms across a 24x range of arena sizes, because it is a search for stamps
//! across the whole 13 GiB backing object. Paying that per growth would stall a
//! running Firefox for three seconds. The frame list turns the host's side into
//! a table lookup per page against `info mtree -f`: 0.064 s.

/// A region this process has just committed, described well enough for the host
/// to find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grown {
    /// The number the region was registered under, which is the number the
    /// host's allocator chose and every descriptor into it will name.
    pub id: u32,
    /// Random per region. The host needs it for the sweep, and it goes in the
    /// plan either way because it is what a human greps for.
    pub run_id: u64,
    pub len: u64,
    pub pages: u32,
    /// False means the pages may move, which would invalidate whatever the host
    /// mapped. Reported rather than swallowed, exactly as at start-up.
    pub locked: bool,
    /// Page frame numbers, one per 4096-byte page, in page order.
    ///
    /// Empty means the query was unavailable or refused, and the host must fall
    /// back to the content sweep — which will *work*, because a freshly
    /// committed region still carries its stamps, and will simply cost three
    /// seconds. That degradation is deliberate and is the only fallback in the
    /// path: design.md §5.1's rule is that a failed locate fails the run, and
    /// this is not a failed locate, it is a slow one.
    pub frames: Vec<u64>,
    /// What the query cost, in milliseconds. Reported because a stage that
    /// claimed a cost it did not record would be making §8.4's mistake, and
    /// because this number is the guest's half of what a growth costs from a
    /// click's point of view.
    pub query_ms: u128,
    /// Why there is no frame list, when there is none. Carried to the host so
    /// that "the sweep ran" has a reason next to it in the log rather than
    /// being a thing somebody has to go and find out.
    pub frames_unavailable: Option<String>,
}

/// Commit a region of at least `bytes` and describe it.
///
/// `bytes` is rounded up to a whole number of pages by the allocator below;
/// the host is told the length that came back rather than the one asked for,
/// because a pool may not straddle two regions and a caller that got less than
/// it asked for has to know.
pub fn commit(id: u32, bytes: u64) -> std::io::Result<Grown> {
    let region = wc_handle::arena::attach_guest_ram(id, bytes)?;
    let (frames, query_ms, frames_unavailable) = match frames(id, region.pages) {
        Ok((f, ms)) => (f, ms, None),
        Err(e) => (Vec::new(), 0, Some(e)),
    };
    Ok(Grown {
        id,
        run_id: region.run_id,
        len: region.len as u64,
        pages: region.pages,
        locked: region.locked,
        frames,
        query_ms,
        frames_unavailable,
    })
}

/// The page frame numbers behind the region registered as `arena`.
///
/// `Err` is a *reason*, not a failure: every caller degrades to the sweep. The
/// string is carried across to the host so the reason appears next to the three
/// seconds it explains.
#[cfg(windows)]
pub fn frames(arena: u32, pages: u32) -> Result<(Vec<u64>, u128), String> {
    // The window has to be named by its address because that is the only name
    // the page-frame database knows it by: the query enumerates the machine's
    // frames and keeps the ones whose recorded virtual address falls inside the
    // window. See `wc_handle::arena::region_base`.
    let base = wc_handle::arena::region_base(arena)
        .ok_or_else(|| format!("arena {arena} is not a region this process provided"))?;
    // Asked before anything expensive, so that a refusal is reported as a
    // refusal rather than as an empty window. The two privileges it wants —
    // `SeProfileSingleProcessPrivilege` and `SeDebugPrivilege` — are default
    // rights of `BUILTIN\Administrators` and were already enabled on the
    // account this project runs as (security-model.md R28: "what it costs in
    // rights: nothing new"). They are enabled here anyway, because "already
    // enabled on the machine it was measured on" is not a thing this process
    // can check by assuming.
    for p in ["SeProfileSingleProcessPrivilege", "SeDebugPrivilege"] {
        if let Err(e) = awe_probe::win::enable_privilege(p) {
            return Err(format!("{p}: {e}"));
        }
    }
    awe_probe::superfetch::available()
        .map_err(|e| format!("SuperfetchMemoryRangesQuery: {e}"))?;
    // 65536 identities is a 1.5 MiB buffer; the whole machine in one call
    // would be 82 MiB, and a single allocation that large is a worse failure
    // mode than a loop. The number is `sf-probe`'s and `gpu-probe`'s, and is
    // about the *machine's* frame count rather than about this window.
    let (frames, ms) = awe_probe::superfetch::locate_frames(base, pages as usize, 1 << 16)
        .map_err(|e| format!("SuperfetchPfnQuery: {e}"))?;
    Ok((frames.into_iter().map(|f| f as u64).collect(), ms))
}

/// There is no page-frame database off Windows, and the Linux build of this
/// compositor is the one `cargo test` runs. Degrading to the sweep is the
/// honest answer and is what the caller already handles.
#[cfg(not(windows))]
pub fn frames(_arena: u32, _pages: u32) -> Result<(Vec<u64>, u128), String> {
    Err("SuperfetchPfnQuery is a Windows interface and this is not Windows".into())
}

/// The frame list in the format `wc-arena-host --frames` reads, which is
/// `sv-hostd qemu --frames`' format, which is what `sf-probe` already writes.
///
/// One format across three programs rather than three, and it is text because
/// the thing on the other end of it is a person as often as it is
/// `sv_hostd::locate::parse_frame_list`. The `gpa` column that format also
/// carries is deliberately omitted: it is the frame shifted by the *guest*, and
/// believing it would be believing the guest's arithmetic about the host's
/// address space instead of doing the translation against `info mtree -f` —
/// which is R21's entire subject. The parser ignores it for the same reason.
pub fn frame_list_text(frames: &[u64]) -> String {
    let mut s = String::with_capacity(frames.len() * 20);
    for (i, f) in frames.iter().enumerate() {
        s.push_str(&format!("pfn {i} {f:x}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_list_is_written_in_the_format_the_locator_parses() {
        // Indices decimal and consecutive, frames hex, one line each. The
        // locator refuses a list whose indices jump, because a frame list with
        // a hole in it translates to an arena with a hole in it.
        assert_eq!(frame_list_text(&[0x1a2b3, 0x1a2b4, 0xff]), "pfn 0 1a2b3\npfn 1 1a2b4\npfn 2 ff\n");
        assert_eq!(frame_list_text(&[]), "");
    }

    /// Off Windows there is no page-frame database, and the answer has to be a
    /// stated reason rather than an empty list — because an empty list and "the
    /// query said this region has no pages" are the same value and very
    /// different facts.
    #[cfg(not(windows))]
    #[test]
    fn without_a_page_frame_database_the_refusal_says_so() {
        let e = frames(0, 4).unwrap_err();
        assert!(e.contains("Windows"), "{e}");
    }
}
