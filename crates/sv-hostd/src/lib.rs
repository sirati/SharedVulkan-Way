//! `sv-hostd`: the daemon that owns the arena's backing and hands it out.
//!
//! design.md §3 forced this component into existence:
//!
//! > Mapping guest RAM means opening QEMU's memfd through
//! > `/proc/<qemu-pid>/fd/<n>`, which an arbitrary application process should
//! > not be able to do and often cannot. It also centralises the GPA→offset
//! > translation, which is the fragile part (R21), and lets several application
//! > processes share one attachment. **In the same-machine development loop it
//! > degenerates to "create a memfd, `ftruncate`, hand it out"**, which is why
//! > steps 1–6 of §13 need no VM at all.
//!
//! # Two modes, one daemon, and the second is not a mock
//!
//! - [`Mode::Local`] creates the memfd itself. This serves the same-machine
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
//! **everything above the run list is identical**. The client receives a
//! descriptor and a file descriptor, maps two views, and never learns whether
//! the extent was one `mmap` or forty-seven thousand (§5.3, §5.7).
//!
//! # Why the daemon proper is Linux-only
//!
//! `sv-hostd` runs on the **app side**, and its job is to own a memfd and pass
//! descriptors over a Unix socket. Neither primitive exists on Windows, so
//! everything below that touches one is gated. What stays portable is the part
//! that is genuinely about the protocol — the geometry, the identity and the
//! publication step — because a Windows app side would reimplement the handoff
//! with a named pipe and `DuplicateHandle` and reuse the rest unchanged.
//!
//! That split is checked by `cargo check --target x86_64-pc-windows-gnu`, which
//! is why it is a `cfg` and not a comment.

#[cfg(target_os = "linux")]
use sv_arena::extent::{
    ArenaLocator, ContiguousLocator, Extent, RunList, contiguous_extent, max_map_count,
};
#[cfg(target_os = "linux")]
use sv_transport::cold::Attachment;
#[cfg(target_os = "linux")]
use sv_transport::region::memfd;
use sv_transport::control::{Control, Geometry, ProtocolIds};
use sv_transport::region::{PAGE, SharedRegion};
use std::sync::Arc;

#[cfg(target_os = "linux")]
pub mod locate;
#[cfg(target_os = "linux")]
pub mod qemu;

/// How the daemon obtained the extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The daemon created the backing itself: development loop, and the
    /// Linux/Linux topology.
    Local,
    /// The extent is guest RAM, reached through QEMU's memory backend and
    /// located by content. The topology design.md was written for.
    Qemu,
}

/// The daemon's view of the one extent it serves.
#[cfg(target_os = "linux")]
pub struct Hostd {
    pub mode: Mode,
    pub geometry: Geometry,
    pub extent: Extent,
    pub runs: RunList,
    pub fd: std::os::fd::OwnedFd,
    pub generation: u32,
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

        // `MFD_ALLOW_SEALING` is not decoration: without it this memfd can never
        // be wrapped by udmabuf, and a Linux GPU side needs a dma-buf because
        // RADV and NVIDIA both refuse a shared host pointer (sv_arena::import).
        // The flag has to be passed at creation, so a daemon that omitted it
        // would have foreclosed the AMD path before the first allocation.
        let fd = memfd("sv-arena", total)?;

        let extent = contiguous_extent(run_id, 0, control_len, arena_len)?;
        let locator = ContiguousLocator { file_offset: 0 };
        let runs = locator.locate(&extent.desc)?.coalesce();
        runs.validate(total)?;
        // §5.7: refuse loudly with the actual numbers rather than letting a
        // client discover it at the 1048577th mapping.
        runs.check_map_count(max_map_count())?;

        Ok(Hostd { mode: Mode::Local, geometry, extent, runs, fd, generation })
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
        let target = qemu::QemuTarget::discover(qmp, hint).map_err(HostdError::Io)?;
        let (map, report) = locate::scan(&target.file, target.file_len, &target.regions, desc)
            .map_err(|e| match e {
                locate::LocateError::Io(e) => HostdError::Io(e),
                other => HostdError::Io(std::io::Error::other(other.to_string())),
            })?;
        let runs = map.coalesce();
        let total = (desc.control_pages + desc.arena_pages) * PAGE as u64;
        runs.validate(total)?;
        runs.check_map_count(max_map_count())?;

        let extent = Extent { desc: desc.clone(), runs: runs.clone() };
        // The descriptor is the daemon's own view of the backend; `Hostd` holds
        // it as an `OwnedFd` so the mapping outlives the discovery.
        use std::os::fd::{AsFd, OwnedFd};
        let fd: OwnedFd = target.file.as_fd().try_clone_to_owned().map_err(HostdError::Io)?;
        let geometry = Geometry {
            control_size: desc.control_pages * PAGE as u64,
            arena_size: desc.arena_pages * PAGE as u64,
            ..Geometry::default()
        };
        Ok((
            Hostd { mode: Mode::Qemu, geometry, extent, runs, fd, generation },
            target.regions,
            report,
        ))
    }

    /// The message a client gets, alongside the descriptor.
    pub fn attachment(&self) -> Attachment {
        Attachment {
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
/// `sv-proto`'s `PROTOCOL_HASH` is the real value (design.md §4.3). This crate
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

    #[test]
    fn a_local_daemon_describes_one_contiguous_extent() {
        let g = demo_geometry();
        let h = Hostd::local(g, 0xabc, 1).unwrap();
        assert_eq!(h.runs.len(), 1);
        assert_eq!(h.runs.bytes(), h.total_len());
        let a = h.attachment();
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
        let h = Hostd::local(g, 1, 1).unwrap();
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

    #[test]
    fn the_owner_publishes_and_a_second_view_attaches() {
        use std::os::fd::AsFd;
        let g = demo_geometry();
        let h = Hostd::local(g, 1, 3).unwrap();
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
