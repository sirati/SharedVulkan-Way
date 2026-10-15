//! `sv-hostd`: the daemon that owns the arena's backing and hands it out.
//!
//! docs/design.md §3 forced this component into existence:
//!
//! > Mapping guest RAM means opening QEMU's memfd through
//! > `/proc/<qemu-pid>/fd/<n>`, which an arbitrary application process should
//! > not be able to do and often cannot. It also centralises the GPA→offset
//! > translation, which is the fragile part (R21), and lets several application
//! > processes share one attachment. **In the same-machine development loop it
//! > degenerates to "create a memfd, `ftruncate`, hand it out"**, which is why
//! > steps 1–6 of §13 need no VM at all.
//!
//! The last sentence of that quote is marked superseded in §3 as of §1a: "hand
//! it out" meant `SCM_RIGHTS`, and what the daemon hands out now is a *name*.
//! Everything else in it still holds.
//!
//! # Two modes, one daemon, and the second is not a mock
//!
//! - [`Mode::Local`] creates the named object itself. This serves the same-machine
//!   development loop **and the Linux-host/Linux-guest topology**, where the two
//!   sides are separate processes — or separate containers — on one kernel and
//!   there is no hypervisor to hide the layout. The extent is contiguous in the
//!   file because `sv-hostd` made it that way, so the locator is
//!   [`sv_arena::extent::ContiguousLocator`] and the run list has one entry.
//! - [`Mode::Qemu`] opens QEMU's memfd, parses `info mtree -f` into a region
//!   table, locates the guest's pages by content and coalesces them into runs.
//!   Built in §13 step 7, from `crates/arena-host`'s scan, translation and
//!   coalescing. See [`qemu`] for the region table and [`locate`] for the scan
//!   and the stitcher.
//!
//!   **It is not Windows-specific.** Everything in it is about QEMU's memory
//!   backend and says nothing about what the guest runs, which is why it is
//!   developed against a Linux guest where the VM can be restarted freely and
//!   only then pointed at the production machine.
//!
//! The distinction the two modes share, and the reason they are one daemon:
//! **everything above the run list is identical**. The client receives an
//! `Attachment` and a *name*, opens the object itself, maps two views, and never
//! learns whether the extent was one `mmap` or forty-seven thousand (§5.3,
//! §5.7).
//!
//! # What the daemon does not do any more: pass a descriptor
//!
//! docs/design.md §1a: **no file descriptor or handle ever crosses between the app
//! side and the GPU side**, and a name crossing — which the peer then opens
//! itself — is the mechanism that replaces it. This daemon used to hand its
//! memfd out over `SCM_RIGHTS`. It now creates a *named* object under
//! `/dev/shm` in [`Mode::Local`], and in [`Mode::Qemu`] it publishes the name of
//! QEMU's backing object ([`qemu::backend_name`]). Both are text, both fit down
//! a byte stream, and the daemon's own descriptor never leaves the process.
//!
//! # Why the daemon proper is Linux-only
//!
//! `sv-hostd` runs on the **app side**, and its job is to own a shared object
//! and publish its name over a socket. What is Linux-specific is now only the
//! *spelling* — `/dev/shm` and `/proc` — rather than the mechanism, and that is
//! a real narrowing: a Windows app side needs a named section and a named pipe
//! and **no `DuplicateHandle`**, exactly as `sv-gpu-server`'s Windows half
//! already demonstrates.
//!
//! That split is checked by `cargo check --target x86_64-pc-windows-gnu`, which
//! is why it is a `cfg` and not a comment.

#[cfg(target_os = "linux")]
use sv_arena::extent::{
    ArenaLocator, ContiguousLocator, Extent, RunList, contiguous_extent, max_map_count,
};
#[cfg(target_os = "linux")]
use sv_transport::cold::Attachment;
use sv_transport::control::{Control, Geometry, ProtocolIds};
use sv_transport::region::{PAGE, SharedRegion};
use std::sync::Arc;

#[cfg(target_os = "linux")]
pub mod broker;
pub mod locate;
#[cfg(target_os = "linux")]
pub mod qemu;

/// How the daemon obtained the extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The daemon created the backing itself — a **named** object under
    /// `/dev/shm` — and hands out its name: development loop, and the
    /// Linux/Linux topology.
    Local,
    /// The extent is guest RAM, reached through QEMU's memory backend and
    /// located by content. The topology design.md was written for.
    Qemu,
}

/// Which of §5.1's locators to run when attaching to a guest's extent.
///
/// The default is [`LocatorChoice::Scan`] and stays the default: it is the only
/// locator that *verifies* rather than trusts, it needs nothing from the guest
/// but a run id, and every measurement this project quotes was taken with it.
/// The other two are opt-in because the frame list rests on an undocumented
/// Windows interface (R28) and nothing should acquire that dependency by
/// accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocatorChoice {
    /// Sweep the backing object for the guest's stamps. 1.8 s at 64 MiB, 2.32 s
    /// at 2 GiB, measured — the cost is linear in guest RAM, not in arena size.
    #[default]
    Scan,
    /// Translate the frame list the descriptor carries, and do not sweep.
    /// 0.064 s, measured, and the only locator that works once the stamps have
    /// been overwritten by arena data (§0.14). Trusted, not verified.
    Pfns,
    /// Both, compared page for page. **This is R28's attach-time cross-check**,
    /// and the arrangement §5.1 settles on: the one occasion on which the stamps
    /// still exist is the one occasion on which the query can be checked.
    Both,
}

impl LocatorChoice {
    pub fn parse(s: &str) -> Option<LocatorChoice> {
        match s {
            "scan" => Some(LocatorChoice::Scan),
            "pfns" => Some(LocatorChoice::Pfns),
            "both" => Some(LocatorChoice::Both),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LocatorChoice::Scan => "scan",
            LocatorChoice::Pfns => "pfns",
            LocatorChoice::Both => "both",
        }
    }
}

/// What each locator that ran had to say. `None` means it did not run, which is
/// a different statement from "it found nothing" and is why these are options
/// rather than zeroed reports.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default)]
pub struct LocateOutcome {
    pub scan: Option<locate::ScanReport>,
    pub pfns: Option<locate::PfnReport>,
    /// Present only on [`LocatorChoice::Both`]. Its absence is what a session
    /// that took the frame list on trust looks like from the outside.
    pub cross_check: Option<locate::CrossCheck>,
}

/// The daemon's view of the one extent it serves.
#[cfg(target_os = "linux")]
pub struct Hostd {
    pub mode: Mode,
    pub geometry: Geometry,
    pub extent: Extent,
    pub runs: RunList,
    /// The daemon's **own** descriptor on the object. It is never given away —
    /// docs/design.md §1a — and exists so the daemon can map and verify the extent
    /// before it tells anyone the name.
    pub fd: std::os::fd::OwnedFd,
    /// The name a client opens for itself: a `/dev/shm` path on [`Mode::Local`],
    /// QEMU's backing name on [`Mode::Qemu`] ([`qemu::backend_name`]).
    pub object: String,
    /// Whether this daemon created `object` and must therefore unlink it.
    /// False for [`Mode::Qemu`], where the object is the hypervisor's.
    pub owns_name: bool,
    pub generation: u32,
}

#[cfg(target_os = "linux")]
impl Drop for Hostd {
    fn drop(&mut self) {
        // Releasing the *name*, not the memory: every client that opened it
        // holds its own descriptor and its mappings stay valid. Doing it here
        // rather than on an exit path means a daemon that dies of an error
        // leaves nothing behind in `/dev/shm` either.
        if self.owns_name {
            sv_transport::region::shm_unlink(&self.object);
        }
    }
}

/// Errors that are the daemon's rather than the transport's.
#[derive(Debug)]
pub enum HostdError {
    Arena(sv_arena::Error),
    Transport(sv_transport::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for HostdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostdError::Arena(e) => write!(f, "arena: {e}"),
            HostdError::Transport(e) => write!(f, "transport: {e}"),
            HostdError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for HostdError {}

impl From<sv_arena::Error> for HostdError {
    fn from(e: sv_arena::Error) -> Self {
        HostdError::Arena(e)
    }
}
impl From<sv_transport::Error> for HostdError {
    fn from(e: sv_transport::Error) -> Self {
        HostdError::Transport(e)
    }
}
impl From<std::io::Error> for HostdError {
    fn from(e: std::io::Error) -> Self {
        HostdError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, HostdError>;

#[cfg(target_os = "linux")]
impl Hostd {
    /// Create the backing and describe it. The whole of `Mode::Local`.
    pub fn local(geometry: Geometry, run_id: u64, generation: u32) -> Result<Hostd> {
        let control_len = geometry.control_size;
        let arena_len = geometry.arena_size;
        let total = control_len + arena_len;

        // **A named object, not a memfd** (docs/design.md §1a). A memfd has no name,
        // so the only way to give one to the GPU side is `SCM_RIGHTS`, and a
        // descriptor may not cross the boundary — on the topology this mode
        // stands in for, the GPU side is a VM and there is nothing for a
        // descriptor to cross.
        //
        // The name carries the run id so that two daemons on one machine cannot
        // collide, and so that a leftover from a crash is identifiable.
        //
        // What this costs, measured and not guessed: a `/dev/shm` object cannot
        // be sealed, so it can never be wrapped by `/dev/udmabuf`. The old
        // comment here said `MFD_ALLOW_SEALING` "is not decoration" because it
        // kept the AMD dma-buf path open; that path is closed on this handoff
        // and the reason is in docs/design.md §0.7. It is *not* closed on the
        // cross-VM handoff, where the object is QEMU's own sealed memfd.
        let object = sv_transport::region::shm_path(&format!("arena.{run_id:016x}"));
        let file = sv_transport::region::shm_create(&object, total)?;
        let fd = std::os::fd::OwnedFd::from(file);

        let extent = contiguous_extent(run_id, 0, control_len, arena_len)?;
        let locator = ContiguousLocator { file_offset: 0 };
        let runs = locator.locate(&extent.desc)?.coalesce();
        runs.validate(total)?;
        // §5.7: refuse loudly with the actual numbers rather than letting a
        // client discover it at the 1048577th mapping.
        runs.check_map_count(max_map_count())?;

        Ok(Hostd {
            mode: Mode::Local,
            geometry,
            extent,
            runs,
            fd,
            object,
            owns_name: true,
            generation,
        })
    }

    /// Attach to an extent the *guest* allocated, located by content.
    ///
    /// The whole of `Mode::Qemu`. Everything about it is discovered:
    ///
    /// 1. find QEMU, its memory backend and the backend's descriptor;
    /// 2. read `info mtree -f` into a region table — never a formula (R21);
    /// 3. sweep the backend for the run's stamps, in extent order;
    /// 4. coalesce into runs and check them against `vm.max_map_count`.
    ///
    /// What is *not* done here is the stitching, because the run list is what a
    /// client needs and each client stitches its own view — the mapping is
    /// per-process, and a daemon that stitched once could not hand the result
    /// to anyone. [`locate::attach`] is the function a client calls.
    ///
    /// The geometry comes from the descriptor rather than from the caller: in
    /// this direction the guest owns the region (D20) and the app side is told
    /// how big it is. Passing a geometry in would be the app side deciding
    /// something it does not get to decide.
    pub fn qemu(
        qmp: &std::path::Path,
        hint: &str,
        desc: &sv_transport::cold::ExtentDesc,
        generation: u32,
    ) -> Result<(Hostd, qemu::RegionTable, locate::ScanReport)> {
        let (hostd, regions, outcome) =
            Hostd::qemu_located(qmp, hint, desc, generation, LocatorChoice::Scan)?;
        // `LocatorChoice::Scan` always produces one, and saying so here keeps
        // this signature — which every existing caller depends on — free of the
        // option the general form has to carry.
        let report = outcome
            .scan
            .ok_or_else(|| HostdError::Io(std::io::Error::other("the scan produced no report")))?;
        Ok((hostd, regions, report))
    }

    /// [`Hostd::qemu`], with §5.1's locator chosen rather than assumed.
    ///
    /// The two locators fail in opposite directions and the choice is which
    /// failure the session is exposed to (§0.14, R28):
    ///
    /// - [`LocatorChoice::Scan`] cannot produce a silently wrong answer, and
    ///   cannot run at all once the stamps are gone.
    /// - [`LocatorChoice::Pfns`] does not care what is in the memory, and cannot
    ///   prove itself.
    /// - [`LocatorChoice::Both`] runs each against the other's weakness, which
    ///   is what R28 requires at attach and what makes the frame list usable
    ///   afterwards. **A disagreement is an error and not a preference**: by the
    ///   time the two locators differ, one of them is naming somebody else's
    ///   memory and there is no way to tell from here which.
    ///
    /// On `Both` the frame list's map is the one that is kept. The two are equal
    /// — that is what was just checked — so this is not a choice about *this*
    /// attach; it is the honest statement that the frame list is what the
    /// session will re-locate with from now on.
    pub fn qemu_located(
        qmp: &std::path::Path,
        hint: &str,
        desc: &sv_transport::cold::ExtentDesc,
        generation: u32,
        choice: LocatorChoice,
    ) -> Result<(Hostd, qemu::RegionTable, LocateOutcome)> {
        let target = qemu::QemuTarget::discover(qmp, hint).map_err(HostdError::Io)?;
        let locate_err = |e| match e {
            locate::LocateError::Io(e) => HostdError::Io(e),
            other => HostdError::Io(std::io::Error::other(
                <locate::LocateError as std::string::ToString>::to_string(&other),
            )),
        };
        let sweep = || {
            locate::scan(&target.file, target.file_len, &target.regions, desc).map_err(locate_err)
        };
        let translate = || locate::PfnLocator { regions: &target.regions }.locate(desc).map_err(locate_err);

        let mut outcome = LocateOutcome::default();
        let map = match choice {
            LocatorChoice::Scan => {
                let (map, report) = sweep()?;
                outcome.scan = Some(report);
                map
            }
            LocatorChoice::Pfns => {
                let (map, report) = translate()?;
                outcome.pfns = Some(report);
                map
            }
            LocatorChoice::Both => {
                // The scan first, because it is the one that can fail for an
                // ordinary reason — a run id that is not in this guest's memory
                // — and paying 2 s to discover that after a successful
                // translation would report the wrong problem.
                let (scanned, scan_report) = sweep()?;
                let (translated, pfn_report) = translate()?;
                outcome.scan = Some(scan_report);
                outcome.pfns = Some(pfn_report);
                outcome.cross_check =
                    Some(locate::cross_check(&scanned, &translated).map_err(locate_err)?);
                translated
            }
        };
        let runs = map.coalesce();
        let total = (desc.control_pages + desc.arena_pages) * PAGE as u64;
        runs.validate(total)?;
        runs.check_map_count(max_map_count())?;

        let extent = Extent { desc: desc.clone(), runs: runs.clone() };
        // The descriptor is the daemon's own view of the backend; `Hostd` holds
        // it as an `OwnedFd` so the mapping outlives the discovery.
        use std::os::fd::{AsFd, OwnedFd};
        let fd: OwnedFd = target.file.as_fd().try_clone_to_owned().map_err(HostdError::Io)?;
        // The name a client will open. It is QEMU's, so this daemon must not
        // unlink it.
        let object = qemu::backend_name(&target.fd_path);
        let geometry = Geometry {
            control_size: desc.control_pages * PAGE as u64,
            arena_size: desc.arena_pages * PAGE as u64,
            ..Geometry::default()
        };
        Ok((
            Hostd {
                mode: Mode::Qemu,
                geometry,
                extent,
                runs,
                fd,
                object,
                owns_name: false,
                generation,
            },
            target.regions,
            outcome,
        ))
    }

    /// The message a client gets. It carries a **name**, not a descriptor.
    pub fn attachment(&self) -> Attachment {
        Attachment {
            object: self.object.clone(),
            // One object, two sub-ranges — §4.1's two independent mappings, from
            // one name. The provider that needs two objects is the quarantined
            // one, and this daemon is not it.
            arena_object: self.object.clone(),
            control_offset: 0,
            control_len: self.geometry.control_size,
            arena_offset: self.geometry.control_size,
            arena_len: self.geometry.arena_size,
            generation: self.generation,
            // Zero means "one contiguous range", which is both the development
            // loop and the best measured real case (1 GiB large-page = 1 run).
            run_count: if self.runs.len() == 1 { 0 } else { self.runs.len() as u64 },
        }
    }

    pub fn total_len(&self) -> u64 {
        self.geometry.control_size + self.geometry.arena_size
    }
}

#[cfg(target_os = "linux")]
/// Map the two views a client works from.
///
/// The one function both sides use, so that "the control region and the arena
/// are two independent mappings with independent bases" (§4.1) is established in
/// one place rather than assumed twice.
pub fn map_views(
    fd: std::os::fd::BorrowedFd<'_>,
    a: &Attachment,
) -> sv_transport::Result<(Arc<SharedRegion>, Arc<SharedRegion>)> {
    let control = SharedRegion::map_fd(fd, a.control_offset, a.control_len as usize)?;
    let arena = SharedRegion::map_fd(fd, a.arena_offset, a.arena_len as usize)?;
    Ok((Arc::new(control), Arc::new(arena)))
}

/// Open the object the [`Attachment`] names, and map the two views from it.
///
/// The client's whole half of the handoff, in one call: **a name in, two
/// mappings out**, and no descriptor anywhere. Every attaching process in the
/// project goes through this — the ICD, `sv-gpu-server attach`, `sv-loopback` —
/// so "open it yourself" is written once.
#[cfg(target_os = "linux")]
pub fn open_and_map_views(
    a: &Attachment,
) -> sv_transport::Result<(Arc<SharedRegion>, Arc<SharedRegion>)> {
    use std::os::fd::AsFd;
    let file = sv_transport::region::open_named(&a.object)?;
    map_views(file.as_fd(), a)
}

/// A geometry small enough for a test or a container demo to finish quickly,
/// with the same shape as the real one.
pub fn demo_geometry() -> Geometry {
    Geometry {
        control_size: 1 << 20,
        arena_size: 16 << 20,
        arena_slab_size: 4 << 20,
        ring_slot_count: 8,
        ring_data_bytes: 16 << 10,
        timeline_count: 64,
    }
}

/// Publish the control region as the owner, once the views are mapped.
pub fn become_owner(
    control: Arc<SharedRegion>,
    geometry: Geometry,
    ids: ProtocolIds,
    generation: u32,
) -> sv_transport::Result<Control> {
    Control::create(control, geometry, ids, generation)
}

/// The identity the two sides must agree on.
///
/// `sv-proto`'s `PROTOCOL_HASH` is the real value (docs/design.md §4.3). This crate
/// does not depend on the 126,000-line generated crate — the transport has to be
/// testable without it — so the binaries pass their own, and the *loopback*
/// binaries pass a constant. A real `sv-icd` passes `sv_proto::PROTOCOL_HASH`
/// and the check becomes the R13 detector it is meant to be.
pub const DEMO_PROTOCOL_HASH: u64 = 0x5356_5F44_454D_4F31;

pub fn demo_ids() -> ProtocolIds {
    ProtocolIds::new(1, DEMO_PROTOCOL_HASH)
}

/// Bytes of one page, re-exported so the binaries need not depend on
/// `sv-transport::region` for a constant.
pub const PAGE_BYTES: usize = PAGE;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A run id no other test in this process, and no other run of this test
    /// binary, can be using.
    ///
    /// # This is a truncated `cargo test --workspace`, not a tidiness fix
    ///
    /// `Hostd::local` names its object `/dev/shm/sv.arena.{run_id:016x}` and
    /// creates it `O_CREAT|O_EXCL`, deliberately: the name carries the run id so
    /// two daemons cannot collide and a leftover from a crash is identifiable.
    /// Two tests here passed the **same** `run_id` of 1, and the harness runs
    /// tests in parallel threads of one process, so whichever lost the race got
    /// `EEXIST` — `open(shm, O_CREAT|O_EXCL), errno: 17`.
    ///
    /// It failed *sometimes*, which is what made it expensive. A failing test
    /// binary stops `cargo test --workspace` where it stands, so the run
    /// reported **169 tests of 1253** on the merge this was found in, and the
    /// same tree on another attempt reported the true total. A number that is
    /// either 169 or 1253 depending on thread scheduling is not a baseline, and
    /// it is the same defect §32.2 fixed in `sv-icd` arriving from a second
    /// direction.
    ///
    /// The process id is in the name as well as a per-test constant, so a run
    /// that panicked before `Drop` could unlink cannot poison every future run
    /// of the same test — which `O_EXCL` on a fixed name otherwise guarantees.
    fn unique_run_id(test: u64) -> u64 {
        (u64::from(std::process::id()) << 16) | test
    }

    #[test]
    fn a_local_daemon_describes_one_contiguous_extent() {
        let g = demo_geometry();
        let h = Hostd::local(g, unique_run_id(1), 1).unwrap();
        assert_eq!(h.runs.len(), 1);
        assert_eq!(h.runs.bytes(), h.total_len());
        let a = h.attachment();
        assert!(a.object.starts_with("/dev/shm/sv."), "the handoff is a name: {:?}", a.object);
        assert_eq!(a.control_offset, 0);
        assert_eq!(a.arena_offset, g.control_size);
        assert_eq!(a.run_count, 0, "one run is reported as the contiguous case");
        // The arena begins where the control region ends and they do not
        // overlap — §4.1's "two independent mappings", as arithmetic.
        assert_eq!(a.control_offset + a.control_len, a.arena_offset);
    }

    #[test]
    fn the_two_views_are_independent_mappings_of_one_file() {
        use std::os::fd::AsFd;
        let g = demo_geometry();
        let h = Hostd::local(g, unique_run_id(2), 1).unwrap();
        let (control, arena) = map_views(h.fd.as_fd(), &h.attachment()).unwrap();
        assert_eq!(control.len() as u64, g.control_size);
        assert_eq!(arena.len() as u64, g.arena_size);

        // Writing at offset 0 of the arena must not touch offset 0 of the
        // control region: they are different parts of the file. This is the
        // check that would have caught an off-by-one in the attachment offsets,
        // which is the kind of mistake that otherwise shows up as the ring
        // directory being overwritten by the first allocation.
        arena.write_bytes(0, b"ARENA").unwrap();
        let mut buf = [0u8; 5];
        control.read_bytes(0, &mut buf).unwrap();
        assert_eq!(&buf, b"\0\0\0\0\0");
    }

    /// docs/design.md §1a, as the daemon's clients experience it: a *name* is enough.
    ///
    /// The client here shares nothing with the daemon but a string — no
    /// descriptor, no `SCM_RIGHTS`, no `/proc` — and reaches the same pages.
    /// That is the whole of the handoff and the reason the cold path can be a
    /// virtio-serial port.
    #[test]
    fn a_client_reaches_the_same_pages_from_the_name_alone() {
        let g = demo_geometry();
        let h = Hostd::local(g, unique_run_id(3), 1).unwrap();
        let at = h.attachment();

        // Everything a client is given, as text.
        let name = at.object.clone();
        assert!(!name.is_empty());

        let (_, mine) = open_and_map_views(&at).unwrap();
        let (_, theirs) = open_and_map_views(&at).unwrap();
        mine.write_bytes(4096, b"by name").unwrap();
        let mut out = [0u8; 7];
        theirs.read_bytes(4096, &mut out).unwrap();
        assert_eq!(&out, b"by name");

        // And when the daemon goes, the name goes with it.
        drop(h);
        assert!(
            !std::path::Path::new(&name).exists(),
            "the daemon must unlink the name it created"
        );
        // …while the mappings that were already made keep working, which is why
        // unlinking early is safe once every client has opened.
        theirs.read_bytes(4096, &mut out).unwrap();
        assert_eq!(&out, b"by name");
    }

    #[test]
    fn the_owner_publishes_and_a_second_view_attaches() {
        use std::os::fd::AsFd;
        let g = demo_geometry();
        let h = Hostd::local(g, unique_run_id(4), 3).unwrap();
        let (control, _arena) = map_views(h.fd.as_fd(), &h.attachment()).unwrap();
        let owner = become_owner(Arc::clone(&control), g, demo_ids(), 3).unwrap();
        assert_eq!(owner.generation(), 3);

        // A second, entirely separate mapping of the same file — which is what
        // the peer process has — sees the published region.
        let (peer_control, _) = map_views(h.fd.as_fd(), &h.attachment()).unwrap();
        let peer = Control::attach(peer_control, demo_ids()).unwrap();
        assert_eq!(peer.generation(), 3);
    }
}
