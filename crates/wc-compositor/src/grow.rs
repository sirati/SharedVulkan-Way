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
    /// path: docs/waycompose-design.md §5.1's rule is that a failed locate fails the run, and
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

/// Linux has a page-frame database too, and it is `/proc/self/pagemap`.
///
/// STAGE 70, and the comment this replaced was wrong rather than merely
/// incomplete: it said "there is no page-frame database off Windows", which was
/// true of the only guest this project had when it was written and stopped
/// being true the moment a **Linux** guest became a topology somebody wanted
/// tested. The consequence was not a missing feature but a *failing run*: with
/// no frame list the host falls back to the content sweep, and §7 measured that
/// sweep unreliable on a loaded machine — five consecutive runs giving
/// "16377 of 16384 pages", "1 page index claimed twice", "28 claimed twice".
/// The first Linux-guest run of `wc-vm-damage.sh` reproduced it exactly, at
/// 16316 of 16384, and could not locate its own arena.
///
/// The format is documented in the kernel's `admin-guide/mm/pagemap.rst`: one
/// 64-bit entry per virtual page, indexed by virtual page number, bit 63 is
/// *present* and bits 0..54 are the frame number.
///
/// # The privilege, and why a zero is a refusal rather than an answer
///
/// Since Linux 4.0 a process without `CAP_SYS_ADMIN` reads **zeroes** for the
/// frame number instead of being refused — the entry is still returned, the
/// present bit is still set, and only the number is blanked. That is a
/// deliberate hardening against Rowhammer-style attacks and it is also the
/// worst possible failure shape for this caller, because a list of zeroes is a
/// perfectly well-formed frame list that would send the host looking for page 0
/// sixteen thousand times. So a zero frame behind a present page is treated as
/// a refusal and reported as one.
#[cfg(target_os = "linux")]
pub fn frames(arena: u32, pages: u32) -> Result<(Vec<u64>, u128), String> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let started = std::time::Instant::now();
    let base = wc_handle::arena::region_base(arena)
        .ok_or_else(|| format!("arena {arena} is not a region this process provided"))?;
    let mut f = std::fs::File::open("/proc/self/pagemap")
        .map_err(|e| format!("/proc/self/pagemap: {e}"))?;

    let page = 4096usize;
    let mut out = Vec::with_capacity(pages as usize);
    for i in 0..pages as usize {
        let vpn = (base + i * page) / page;
        f.seek(SeekFrom::Start(vpn as u64 * 8))
            .map_err(|e| format!("/proc/self/pagemap seek: {e}"))?;
        let mut buf = [0u8; 8];
        f.read_exact(&mut buf).map_err(|e| format!("/proc/self/pagemap read: {e}"))?;
        let entry = u64::from_ne_bytes(buf);
        if entry & (1 << 63) == 0 {
            return Err(format!(
                "page {i} of {pages} is not present in /proc/self/pagemap; the region \
                 is not fully faulted in"
            ));
        }
        let pfn = entry & ((1u64 << 55) - 1);
        if pfn == 0 {
            return Err(
                "/proc/self/pagemap returned frame 0 for a present page, which is what \
                 it does without CAP_SYS_ADMIN -- run the compositor as root in the \
                 guest, or the host will locate by content sweep instead"
                    .into(),
            );
        }
        out.push(pfn);
    }
    Ok((out, started.elapsed().as_millis()))
}

/// Neither Windows nor Linux. Degrading to the sweep is the honest answer and
/// is what the caller already handles.
#[cfg(not(any(windows, target_os = "linux")))]
pub fn frames(_arena: u32, _pages: u32) -> Result<(Vec<u64>, u128), String> {
    Err("no page-frame database on this platform".into())
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

/// The compositor's end of the growth channel, from creation to servicing.
///
/// # When this is created, and why it cannot be earlier
///
/// `Control::create` **zeroes** the control region, and on guest RAM those
/// bytes are stamp pages — the ones the host locates the arena by. Creating the
/// channel before the host's locate would delete the stamps out from under it,
/// and `sv-gpu-server`'s own bring-up records what that looks like: *"located
/// 8192 of 12288 pages — all 4096 control pages missing. Nothing said why; the
/// count was simply short."*
///
/// So it is created at the first client accept, alongside
/// [`wc_handle::arena::release_gifts`], which is the signal §12.10 already
/// built for exactly this hazard: a client can only have arrived through a
/// proxy that was started from a plan, and the plan exists only if the locate
/// succeeded. Same event, same argument, and no new flag to get wrong.
///
/// # Why there is no flag
///
/// The channel is created whenever there is an outbound window big enough to
/// hold it. A deployment that has one is a deployment where the host is the
/// party allocating pools and can therefore run out; a harness with a 64 KiB
/// window is testing a keymap and is told, once, that it is not getting a
/// channel. Making it a flag would mean a launcher that forgot it produced a
/// session which refuses under pressure for no visible reason — which is
/// exactly the failure this whole stage exists to remove.
pub struct Service {
    /// Held because the ring's producer and consumer resolve their offsets
    /// through the layout it published, and because dropping it would drop the
    /// last `Arc` on the region view.
    control: sv_transport::control::Control,
    /// `None` until the host claims the ring, which is every session that never
    /// grows. Not an error: most sessions never grow.
    end: Option<wc_handle::grow::GuestEnd>,
    pub served: u32,
    pub refused: u32,
}

impl Service {
    /// Take the front of the outbound window and publish a control region in
    /// it. `Ok(None)` when there is no window, or none big enough — a stated
    /// fact, not a failure.
    /// STAGE 53. Takes the range that was **already reserved** at start-up
    /// rather than reserving one now.
    ///
    /// The two halves are split because their constraints point in opposite
    /// directions and only one of them was seen when this was written: the
    /// reservation must happen before the seat's keymap takes the front of the
    /// outbound window, and the `Control::create` below must happen after the
    /// host's locate has read the stamps those same pages carry. Reserving here
    /// meant `reserve_growth_channel` refused on every guest session that ever
    /// ran — see `wc_handle::arena::Window::channel`.
    pub fn start() -> Result<Option<Service>, String> {
        let Some(w) = wc_handle::arena::outbound() else {
            return Ok(None);
        };
        let Some((offset, len)) = wc_handle::arena::growth_channel(w.arena) else {
            return Ok(None);
        };
        let base = wc_handle::arena::region_base(w.arena)
            .ok_or_else(|| format!("arena {} is not a region this process provided", w.arena))?;
        // SAFETY: `region_base` answers only for a region this process
        // committed and holds for the life of the process, `reserve_growth_channel`
        // has just taken `[offset, +len)` of it out of every other allocator's
        // reach, and `SharedRegion::from_raw`'s backing is `Borrowed` — its drop
        // frees nothing and the memory outlives it.
        let region = std::sync::Arc::new(unsafe {
            sv_transport::region::SharedRegion::from_raw(
                (base + offset as usize) as *mut u8,
                len as usize,
            )
        });
        let control = sv_transport::control::Control::create(
            region,
            wc_handle::grow::geometry(),
            wc_handle::grow::protocol_ids(),
            1,
        )
        .map_err(|e| format!("growth channel: {e}"))?;
        Ok(Some(Service { control, end: None, served: 0, refused: 0 }))
    }

    /// One turn of the poll loop. Never blocks.
    ///
    /// Returns a line to log when something happened. One request per turn
    /// rather than draining: answering one commits and pins guest RAM and
    /// queries the page-frame database, which is tens of milliseconds, and
    /// doing several in one turn of a 5 ms loop would be a visible stall to buy
    /// memory nobody asked for twice.
    pub fn service(&mut self) -> Option<String> {
        if self.end.is_none() {
            match wc_handle::grow::GuestEnd::open(&self.control) {
                Ok(Some(e)) => self.end = Some(e),
                Ok(None) => return None,
                Err(e) => return Some(format!("growth channel: {e}")),
            }
        }
        let end = self.end.as_mut()?;
        let mut note = None;
        let served = end.service(&mut |req| {
            let started = std::time::Instant::now();
            // RELEASE, and it is the half that actually returns memory. The
            // host unmapping its view frees nothing: the pages are *this*
            // process's, committed and `VirtualLock`ed, so they are non-pageable
            // and gone from Windows entirely until `VirtualFree` runs here.
            // `sv-guest.exe` holding hundreds of megabytes with no client
            // connected is exactly what it looks like when the host thinks it
            // released memory and this side was never told.
            if req.op == wc_handle::grow::Op::Release {
                return match wc_handle::arena::release_guest_ram(req.region) {
                    Ok(bytes) => {
                        // "the guest's operating system" and not "Windows",
                        // because this line is printed by whichever build is in
                        // the guest and §72 read it off a **Linux** one. A log
                        // line that names the wrong operating system is a line
                        // that will be quoted as evidence about it.
                        note = Some(format!(
                            "wc-compositor: released arena {}: {bytes} bytes back to the \
                             guest's operating system in {} ms",
                            req.region,
                            started.elapsed().as_millis()
                        ));
                        wc_handle::grow::Reply {
                            status: 0,
                            run_id: 0,
                            len: bytes,
                            pages: 0,
                            locked: false,
                            frames: Vec::new(),
                            query_ms: started.elapsed().as_millis() as u32,
                            message: String::new(),
                        }
                    }
                    Err(e) => {
                        note = Some(format!(
                            "wc-compositor: could not release arena {}: {e}",
                            req.region
                        ));
                        wc_handle::grow::Reply {
                            status: 1,
                            run_id: 0,
                            len: 0,
                            pages: 0,
                            locked: false,
                            frames: Vec::new(),
                            query_ms: 0,
                            message: format!("{e}"),
                        }
                    }
                };
            }
            match commit(req.region, req.bytes) {
                Ok(g) => {
                    note = Some(format!(
                        "wc-compositor: grew arena {}: run 0x{:016x}, {} bytes, {} pages, \
                         locked={}, {} frame(s) in {} ms, {} ms total{}",
                        g.id,
                        g.run_id,
                        g.len,
                        g.pages,
                        g.locked,
                        g.frames.len(),
                        g.query_ms,
                        started.elapsed().as_millis(),
                        g.frames_unavailable
                            .as_deref()
                            .map(|r| format!(" (no frame list: {r})"))
                            .unwrap_or_default(),
                    ));
                    wc_handle::grow::Reply {
                        status: 0,
                        run_id: g.run_id,
                        len: g.len,
                        pages: g.pages,
                        locked: g.locked,
                        frames: g.frames,
                        query_ms: g.query_ms as u32,
                        message: g.frames_unavailable.unwrap_or_default(),
                    }
                }
                Err(e) => {
                    // A refusal is an answer. The host counts it, the client
                    // survives it (§20), and the reason travels rather than
                    // being left in a log the host cannot read.
                    note = Some(format!(
                        "wc-compositor: refused to grow arena {} by {} bytes: {e}",
                        req.region, req.bytes
                    ));
                    wc_handle::grow::Reply {
                        status: 1,
                        run_id: 0,
                        len: 0,
                        pages: 0,
                        locked: false,
                        frames: Vec::new(),
                        query_ms: 0,
                        message: format!("{e}"),
                    }
                }
            }
        });
        match served {
            Ok(Some((_, reply))) => {
                if reply.status == 0 {
                    self.served += 1;
                } else {
                    self.refused += 1;
                }
                note
            }
            Ok(None) => None,
            Err(e) => Some(format!("wc-compositor: growth channel: {e}")),
        }
    }
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

    /// A refusal has to be a **stated reason** rather than an empty list,
    /// because an empty list and "the query said this region has no pages" are
    /// the same value and very different facts.
    ///
    /// STAGE 70 rewrote this test rather than deleting it, and what it used to
    /// assert is the interesting part: that the error mentions *Windows*,
    /// because off Windows there was "no page-frame database". That was true of
    /// the only guest this project had and false as soon as a Linux guest was
    /// one — Linux has `/proc/self/pagemap`. The test was encoding the belief
    /// rather than the invariant, so it failed the moment the belief was
    /// corrected, which is exactly what a test defending a belief does.
    ///
    /// The invariant survives the correction: whatever this platform can or
    /// cannot do, a caller that cannot be given frames is told why. Asking for
    /// an arena nobody provided is the one refusal every platform shares.
    #[test]
    fn a_frame_query_that_cannot_be_answered_says_why() {
        let e = frames(0, 4).unwrap_err();
        assert!(!e.is_empty(), "a refusal must carry a reason");
        // `frames` is reached with an arena id that was never registered, so
        // every platform's first check is the same one.
        assert!(
            e.contains("not a region this process provided") || e.contains("page-frame"),
            "the reason should name the arena or the platform, got: {e}"
        );
    }
}
