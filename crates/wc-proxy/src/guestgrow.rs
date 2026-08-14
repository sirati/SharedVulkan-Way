//! Growth over guest RAM: ask the compositor for a region, then go and find it.
//!
//! GROWTH, and this is the piece that makes every other piece of the stage do
//! something. [`crate::pool::PlanGrowth`] re-reads the plan and takes any region
//! the locator has added; this is what *causes* one to be added.
//!
//! # The sequence, and the ordering hazard it is arranged around
//!
//! 1. The allocator cannot place a pool and calls [`RegionSource::provide`].
//! 2. A [`wc_handle::grow::Request`] goes to the compositor **over the shared
//!    memory both sides already map** — `sv-transport`'s control region and one
//!    ring, which is the channel shared_vulkan already uses across this
//!    boundary. Not a socket, and not a new mechanism.
//! 3. The compositor commits and pins a region, stamps it, queries the page
//!    frames behind it, and answers with a run id, a page count and the frame
//!    list.
//! 4. This side writes the frame list out and runs `wc-arena-host --append`,
//!    which translates it into file offsets and adds a `region <id>` record to
//!    the plan.
//! 5. The plan is re-read and the new region mapped.
//!
//! **Step 4 must finish before anything is written into the region**, because
//! the first pool allocated into it overwrites the stamps (§12.10). Here that
//! ordering is not a convention to be remembered — it is the control flow.
//! `provide` is synchronous and the allocator does not learn the region exists
//! until this function returns.
//!
//! # Why the locator is a subprocess and not a function call
//!
//! Because guest-physical-address translation exists exactly once across the
//! two projects, in `sv-hostd`, and `crates/wc-arena-host` is the one binary
//! that links it. Calling it from here would mean either a second
//! implementation — which would be wrong in a way that looks exactly like
//! working — or making `wc-proxy` depend on `sv-hostd` and dragging QMP and
//! `/proc` into a process that otherwise needs neither. The proxy already runs
//! as root on the VM host (`scripts/wc-session` starts it in a root transient
//! unit so it can open the backing object), so there is no privilege boundary
//! to cross: it is a fork and an exec.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crate::guestram::{Plan, StitchedRegion};
use crate::pool::{ProvidedRegion, RegionSource};

/// How long to wait for the compositor to answer.
///
/// Generous on purpose. The guest's half is a `VirtualAlloc` plus a
/// `VirtualLock` of every page plus a stamp of every page plus a
/// `SuperfetchPfnQuery` — tens to hundreds of milliseconds — and it is done on
/// the compositor's poll loop, which is also drawing. A timeout that fired
/// early would turn a slow growth into a refusal *and* leave the guest holding
/// a region nobody came to collect.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

/// Everything the locator needs that is not in the request.
#[derive(Debug, Clone)]
pub struct Locator {
    /// `wc-arena-host`.
    pub program: PathBuf,
    pub qmp: PathBuf,
    pub vm: String,
    pub plan: PathBuf,
}

pub struct GuestGrowth {
    locator: Locator,
    /// Region ids already mapped, so the same region is never taken twice.
    taken: Vec<u32>,
    /// The arena the compositor's outbound window — and therefore the control
    /// region — lives in.
    channel: Option<Channel>,
    /// Where the channel is, resolved from the plan once.
    ///
    /// Neither side is *told* this. The compositor derives it from the window
    /// it reserved and this side from the `outbound` line in the plan, both
    /// through `wc_handle::grow::CONTROL_BYTES`. One origin, for the same
    /// reason the outbound window itself has one (§14.7): a number typed twice
    /// is a number that can differ, and the symptom here would be two
    /// `Control`s at two addresses each waiting for a peer writing somewhere
    /// else.
    where_: Option<(u32, u64)>,
}

struct Channel {
    /// Kept alive: the control region is a pointer into this mapping. Dropping
    /// it would unmap the ring under the `Control`.
    _region: StitchedRegion,
    _control: sv_transport::control::Control,
    host: wc_handle::grow::HostEnd,
}

impl GuestGrowth {
    pub fn new(locator: Locator, already: &[u32]) -> GuestGrowth {
        GuestGrowth { locator, taken: already.to_vec(), channel: None, where_: None }
    }

    /// Find the control region in the plan: the front of the outbound window of
    /// whichever region declares one.
    fn locate_channel(&mut self) -> io::Result<(u32, u64)> {
        if let Some(v) = self.where_ {
            return Ok(v);
        }
        let plans = Plan::read_all(&self.locator.plan)?;
        let found = plans.iter().find_map(|p| p.outbound.map(|(off, _)| (p.id, off)));
        let v = found.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{}: no region declares an outbound window, so there is nowhere for \
                     the growth channel to be: the compositor was started without \
                     --arena-outbound",
                    self.locator.plan.display()
                ),
            )
        })?;
        self.where_ = Some(v);
        Ok(v)
    }

    /// Attach to the channel the compositor published, once.
    ///
    /// `NotReady` is the normal answer before the compositor's first client, so
    /// the caller gets a message that says to wait rather than one that says
    /// something is broken.
    fn channel(&mut self) -> io::Result<&mut Channel> {
        if self.channel.is_none() {
            let (region_id, offset) = self.locate_channel()?;
            let plans = Plan::read_all(&self.locator.plan)?;
            let plan = plans
                .iter()
                .find(|p| p.id == region_id)
                .ok_or_else(|| io::Error::other(format!("region {region_id} left the plan")))?;
            let mut region = StitchedRegion::attach(plan)?;
            let len = wc_handle::grow::CONTROL_BYTES as usize;
            let off = offset as usize;
            let bytes = region.as_mut_slice();
            if off + len > bytes.len() {
                return Err(io::Error::other(format!(
                    "the growth channel at {off}+{len} runs past a {}-byte region",
                    bytes.len()
                )));
            }
            let ptr = bytes[off..off + len].as_mut_ptr();
            // SAFETY: `ptr` is inside a mapping this struct owns and holds for
            // its own lifetime, the range is the one both sides derive from the
            // plan's `outbound` line, and `SharedRegion::from_raw`'s backing is
            // `Borrowed` so its drop frees nothing. Moving `Channel` moves the
            // `StitchedRegion` struct but not its `mmap`, so the pointer stays
            // valid.
            let shared = std::sync::Arc::new(unsafe {
                sv_transport::region::SharedRegion::from_raw(ptr, len)
            });
            let control = sv_transport::control::Control::attach(
                shared,
                wc_handle::grow::protocol_ids(),
            )
            .map_err(|e| match e {
                sv_transport::Error::NotReady => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the compositor has not published a growth channel yet; it does so \
                     when its first client connects, because creating it any earlier \
                     would zero the stamps the host locates the arena by",
                ),
                e => io::Error::other(format!("growth channel: {e}")),
            })?;
            let host = wc_handle::grow::HostEnd::attach(&control)?;
            self.channel = Some(Channel { _region: region, _control: control, host });
        }
        Ok(self.channel.as_mut().expect("just built"))
    }

    /// Run `wc-arena-host --append` for a region the guest has just committed.
    fn append_to_plan(&self, id: u32, reply: &wc_handle::grow::Reply) -> io::Result<()> {
        // The frame list next to the plan, named for the region, because a
        // human reading a failed growth wants both and they are the same story.
        let frames_path = self.locator.plan.with_extension(format!("frames.{id}"));
        let mut args: Vec<std::ffi::OsString> = vec![
            "--qmp".into(),
            self.locator.qmp.clone().into(),
            "--vm".into(),
            self.locator.vm.clone().into(),
            "--run".into(),
            format!("0x{:016x}", reply.run_id).into(),
            "--pages".into(),
            reply.pages.to_string().into(),
            "--region".into(),
            id.to_string().into(),
            "--append".into(),
            "--plan".into(),
            self.locator.plan.clone().into(),
        ];
        if reply.frames.is_empty() {
            // The sweep still works on a *fresh* region — nothing has
            // overwritten its stamps yet — it just costs ~3 s instead of
            // 0.064 s. Said out loud, because a growth that silently took
            // fifty times longer is a stall nobody can explain afterwards.
            eprintln!(
                "[proxy] growing region {id} without a frame list ({}); the locator will \
                 sweep, which costs seconds rather than milliseconds",
                if reply.message.is_empty() { "no reason given" } else { &reply.message }
            );
        } else {
            std::fs::write(&frames_path, frame_list_text(&reply.frames))?;
            args.push("--frames".into());
            args.push(frames_path.clone().into());
        }
        let out = std::process::Command::new(&self.locator.program).args(&args).output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "{} {}: {}{}",
                self.locator.program.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim(),
                String::from_utf8_lossy(&out.stdout).trim(),
            )));
        }
        // The locator's own report, forwarded rather than swallowed: it carries
        // which locator ran and how many pages it placed, which is the evidence
        // for every number this stage claims.
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.starts_with("located") || line.starts_with("plan:") {
                println!("[proxy] locate: {line}");
            }
        }
        Ok(())
    }
}

/// The frame list in the format `wc-arena-host --frames` parses.
///
/// The same three lines as `wc_compositor::grow::frame_list_text`, on the other
/// side of the boundary. Not shared through a crate because the two never run
/// in the same process and the format is the *parser's* — `sv_hostd::locate::
/// parse_frame_list` — which both are written against and which has its own
/// tests.
fn frame_list_text(frames: &[u64]) -> String {
    let mut s = String::with_capacity(frames.len() * 20);
    for (i, f) in frames.iter().enumerate() {
        s.push_str(&format!("pfn {i} {f:x}\n"));
    }
    s
}

impl RegionSource for GuestGrowth {
    fn provide(&mut self, id: u32, min_len: u64) -> io::Result<ProvidedRegion> {
        let started = std::time::Instant::now();
        let reply = {
            let channel = self.channel()?;
            channel.host.request(
                wc_handle::grow::Request {
                    op: wc_handle::grow::Op::Grow,
                    region: id,
                    bytes: min_len,
                },
                ANSWER_TIMEOUT,
            )?
        };
        if reply.status != 0 {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "the compositor refused to commit {min_len} bytes for region {id}: {}",
                    reply.message
                ),
            ));
        }
        let asked = started.elapsed();
        self.append_to_plan(id, &reply)?;
        let located = started.elapsed() - asked;

        let plans = Plan::read_all(&self.locator.plan)?;
        let plan = plans.iter().find(|p| p.id == id).ok_or_else(|| {
            io::Error::other(format!(
                "{}: the locator reported success but the plan names no region {id}",
                self.locator.plan.display()
            ))
        })?;
        let provided = ProvidedRegion::guest_ram(plan)?;
        self.taken.push(id);
        println!(
            "[proxy] region {id}: {} bytes of guest RAM — {} ms in the guest, {} ms locating \
             ({})",
            plan.len,
            asked.as_millis(),
            located.as_millis(),
            if reply.frames.is_empty() { "swept" } else { "frame list" },
        );
        Ok(provided)
    }

    /// Tell the guest to give the pages back.
    ///
    /// **This is the half that returns memory, and without it release is a
    /// no-op that looks like one.** The arena has already stopped listing the
    /// region and dropped its mapping by the time this runs — but that mapping
    /// was the *host's second view* of pages the guest committed and
    /// `VirtualLock`ed. `VirtualLock`ed pages are non-pageable, so they are not
    /// merely reserved in the guest, they are gone from Windows entirely, and
    /// the only call that returns them is a `VirtualFree` on the side that made
    /// them.
    ///
    /// Best-effort by construction, which is what [`RegionSource::release`]
    /// promises: the region is already unlisted, so there is nothing to undo if
    /// this fails. A failure is reported and not retried — the guest saying no
    /// means the memory is still charged, which the next
    /// [`crate::pool::MemoryReport`] will show without this having to remember
    /// it.
    fn release(&mut self, id: u32) {
        let Ok(channel) = self.channel() else {
            return;
        };
        let asked = wc_handle::grow::Request {
            op: wc_handle::grow::Op::Release,
            region: id,
            bytes: 0,
        };
        match channel.host.request(asked, ANSWER_TIMEOUT) {
            Ok(r) if r.status == 0 => {
                println!("[proxy] region {id}: {} bytes returned to the guest", r.len);
                self.taken.retain(|t| *t != id);
            }
            Ok(r) => eprintln!(
                "[proxy] region {id}: the compositor did not release it ({}); those pages \
                 are still committed in the guest",
                r.message
            ),
            Err(e) => eprintln!(
                "[proxy] region {id}: could not ask the compositor to release it ({e}); \
                 those pages are still committed in the guest"
            ),
        }
    }

    fn describe(&self) -> String {
        format!(
            "guest RAM, committed on demand by the compositor over the shared-memory \
             channel and located by {}",
            self.locator.program.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The format is the locator's, and it is the same on both sides of the
    /// boundary. Indices decimal and consecutive, frames hex — a list whose
    /// indices jump is refused by `parse_frame_list`, because a frame list with
    /// a hole in it translates to an arena with a hole in it.
    #[test]
    fn the_frame_list_matches_what_the_locator_parses() {
        assert_eq!(frame_list_text(&[0x1a2b3, 0xff]), "pfn 0 1a2b3\npfn 1 ff\n");
        assert_eq!(frame_list_text(&[]), "");
    }

    /// A plan with no outbound window has nowhere to put the channel, and
    /// saying so names the flag that is missing rather than reporting an
    /// address that does not work.
    #[test]
    fn a_plan_with_no_outbound_window_says_which_flag_is_missing() {
        let dir = std::env::temp_dir().join(format!("wc-guestgrow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("arena.plan");
        std::fs::write(&plan, "object /proc/1/fd/9\nlen 8192\nrun 0x1\nr 0 0x0 8192\n").unwrap();
        let mut g = GuestGrowth::new(
            Locator {
                program: "wc-arena-host".into(),
                qmp: "/run/qmp".into(),
                vm: "windows-vm".into(),
                plan,
            },
            &[0],
        );
        let e = g.locate_channel().unwrap_err();
        assert!(e.to_string().contains("--arena-outbound"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
