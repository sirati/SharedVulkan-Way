//! Unactivated private-ABI composition with the reviewed kernel overlay.
//! Native retirement is one local export fact, never permission to reuse a
//! physical grant. No descriptor transfer or canonical release API is exposed.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use wc_broker::service_grants::native::{
    NativeExportIdentity, NativeProjectionAuthority, NativeProviderAuthority, ProjectionTicket,
    RetirementProof, TrackedProjectionBackend,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
struct Range {
    offset: u64,
    size: u64,
}
#[repr(C)]
struct Create {
    version: u32,
    flags: u32,
    memfd: u32,
    count: u32,
    ranges: u64,
    data_fd: i32,
    monitor_fd: i32,
    reserved: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(C)]
struct Info {
    version: u32,
    flags: u32,
    state: u32,
    count: u32,
    instance: [u8; 16],
    export_id: u64,
    source_dev: u64,
    source_ino: u64,
    source_bytes: u64,
    bytes: u64,
}
#[repr(C)]
struct QueryRange {
    index: u32,
    reserved: u32,
    range: Range,
}
const CREATE: libc::c_ulong = 0xc02875f0;
const INFO: libc::c_ulong = 0x804875f1;
const RANGE: libc::c_ulong = 0xc01875f2;
const _: () = {
    assert!(std::mem::size_of::<Create>() == 40);
    assert!(std::mem::size_of::<Info>() == 72);
    assert!(std::mem::size_of::<QueryRange>() == 24);
};

/// A kernel export state only. This cannot authorize canonical grant release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeExportState {
    Live,
    Draining,
    Retired,
    Failed,
}
fn state(n: u32) -> io::Result<NativeExportState> {
    match n {
        0 => Ok(NativeExportState::Live),
        1 => Ok(NativeExportState::Draining),
        2 => Ok(NativeExportState::Retired),
        3 => Ok(NativeExportState::Failed),
        _ => Err(denied("unknown protected export state")),
    }
}
fn query(fd: &OwnedFd) -> io::Result<Info> {
    let mut info = Info::default();
    if unsafe { libc::ioctl(fd.as_raw_fd(), INFO, &mut info) } < 0 {
        return Err(io::Error::last_os_error());
    }
    state(info.state)?;
    Ok(info)
}
fn immutable(mut info: Info) -> Info {
    info.state = 0;
    info
}

/// Actual retained raw-device/module authority. Never a pathname or instance
/// tuple. The reviewed fops owner pins its module for this open File's lifetime.
struct ProtectedProvider {
    device: File,
    identity: (u64, u64, u64),
}
impl ProtectedProvider {
    fn validate(&self) -> io::Result<()> {
        let m = self.device.metadata()?;
        let flags = unsafe { libc::fcntl(self.device.as_raw_fd(), libc::F_GETFL) };
        let descriptor = unsafe { libc::fcntl(self.device.as_raw_fd(), libc::F_GETFD) };
        if m.mode() & libc::S_IFMT != libc::S_IFCHR
            || (m.dev(), m.ino(), m.rdev()) != self.identity
            || flags < 0
            || flags & libc::O_ACCMODE != libc::O_RDWR
            || descriptor < 0
            || descriptor & libc::FD_CLOEXEC == 0
        {
            return Err(denied("retained protected provider object/access changed"));
        }
        Ok(())
    }
}
// SAFETY: created only from independently authenticated reviewed native device.
// The exact retained File pins that fops/module instance, whose CREATE IDs are
// global, strictly monotonic, nonwrapping and never reused. No discovery export
// or caller-supplied instance establishes this authority.
unsafe impl NativeProviderAuthority for ProtectedProvider {}

/// Retains a trusted private-overlay device and exact current service target.
/// There is no stock-ABI or pathname fallback.
pub struct ProtectedBackend {
    local: ServiceLocalBackend,
    provider: Arc<ProtectedProvider>,
    authority: Arc<dyn NativeProviderAuthority>,
}
impl ProtectedBackend {
    /// # Safety
    /// Protected launcher must independently authenticate this retained device
    /// as the reviewed overlay instance, its loaded source/build and native
    /// proof, including fops module ownership, one immutable instance per module
    /// lifetime, strictly monotonic nonwrapping CREATE IDs, and irreversible
    /// all-alias/native-hold/fence retirement with no later remap. The retained
    /// raw File must pin that module lifetime. An ioctl number or caller pathname
    /// does not authenticate a module.
    /// `local` must obey its protected-service enrollment/fork contract.
    pub unsafe fn from_verified_device(
        local: ServiceLocalBackend,
        device: File,
    ) -> io::Result<Self> {
        let m = device.metadata()?;
        let provider = Arc::new(ProtectedProvider {
            device,
            identity: (m.dev(), m.ino(), m.rdev()),
        });
        provider.validate()?;
        let authority: Arc<dyn NativeProviderAuthority> = provider.clone();
        Ok(Self {
            local,
            provider,
            authority,
        })
    }
    /// Enroll this exact retained device authority with the canonical owner.
    /// Its first fresh tracked export pins the actual kernel instance at bind.
    pub fn provider_authority(&self) -> io::Result<Arc<dyn NativeProviderAuthority>> {
        self.local.local.current()?;
        self.provider.validate()?;
        Ok(self.authority.clone())
    }
    pub fn target(&self) -> Arc<dyn ServiceProcess> {
        self.local.target()
    }
    /// Admit the same retained actual service process into its canonical
    /// context. This issues no epoch and supplies no synthetic remote process.
    pub fn context_endpoint(&self, policy: &ServicePolicy) -> io::Result<ServiceEndpoint> {
        self.local.local.current()?;
        // SAFETY: protected construction already proved own service provenance;
        // current() rechecks actual primary credentials and exact live pidfd.
        unsafe {
            ServiceEndpoint::from_current_service(
                policy,
                wc_broker::User::uid(self.local.local.uid),
                self.local.target(),
            )
        }
    }
}
// SAFETY: original/grant checks precede export; only exact local native target
// is admitted. Immutable access/range/source metadata is checked on the retained
// monitor; no descriptor escapes and retirement never mutates allocation state.
unsafe impl ProjectionBackend<SealedBacking> for ProtectedBackend {
    type Projection = ProtectedProjection;
    fn project(
        &mut self,
        grant: Arc<RetainedGrant<SealedBacking>>,
        extent: Extent,
        target: Arc<dyn ServiceProcess>,
        access: ProjectionAccess,
    ) -> io::Result<Self::Projection> {
        self.local.local.current()?;
        self.provider.validate()?;
        if !Arc::ptr_eq(&target, &self.local.target) {
            return Err(denied(
                "protected export target differs from retained local service",
            ));
        }
        let b = grant.original();
        let binding = grant.binding();
        b.bounds(extent.offset, extent.capacity)?;
        if extent.length == 0
            || extent.length > extent.capacity
            || extent.offset < binding.grant_offset
            || !extent
                .offset
                .checked_add(extent.capacity)
                .is_some_and(|end| {
                    binding
                        .grant_offset
                        .checked_add(binding.grant_length)
                        .is_some_and(|limit| end <= limit)
                })
            || b.geometry.physical.host_device != binding.source.host_device
            || b.geometry.physical.host_inode != binding.source.host_inode
            || b.geometry.physical.bar_bytes != binding.source.bar_bytes
        {
            return Err(denied(
                "protected projection differs from canonical physical grant",
            ));
        }
        let (mapping, data, monitor, info) =
            create_checked(&self.provider.device, b, extent, access)?;
        Ok(ProtectedProjection {
            mapping,
            data,
            monitor,
            expected: immutable(info),
            grant,
            local: self.local.local.clone(),
            logical: extent.length,
            access,
            extent,
            provider: self.provider.clone(),
            authority: None,
        })
    }
}
// SAFETY: this callback performs one fresh synchronous CREATE/immutable INFO+
// RANGE validation/bind under the owner's serialized gate. Only this exact
// retained provider and native target are admitted. Authority stays attached to
// the actual monitor and cannot turn an untracked export into a tracked proof.
unsafe impl TrackedProjectionBackend<SealedBacking> for ProtectedBackend {
    type Projection = ProtectedProjection;
    fn project_tracked(
        &mut self,
        grant: Arc<RetainedGrant<SealedBacking>>,
        ticket: ProjectionTicket,
    ) -> io::Result<Self::Projection> {
        if !Arc::ptr_eq(ticket.provider(), &self.authority) {
            return Err(denied(
                "projection ticket belongs to another retained provider",
            ));
        }
        if ticket.binding() != grant.binding() {
            return Err(denied(
                "projection ticket differs from exact retained grant",
            ));
        }
        if grant.original().live_owner.is_none() {
            return Err(unsupported(
                "tracked projection requires requeryable original source owner",
            ));
        }
        let extent = ticket.extent();
        let access = ticket.access();
        let target = ticket.target().clone();
        let mut projection = <Self as ProjectionBackend<SealedBacking>>::project(
            self,
            grant,
            extent,
            target.clone(),
            access,
        )?;
        let info = projection.expected;
        // SAFETY: actual retained monitor's immutable facts and exact one-range
        // source were verified before this view can be returned or transferred.
        let identity = unsafe {
            NativeExportIdentity::from_kernel(
                info.instance,
                info.export_id,
                wc_broker::bar_registry::PhysicalBacking {
                    host_device: info.source_dev,
                    host_inode: info.source_ino,
                    bar_bytes: info.source_bytes,
                },
                extent.offset,
                extent.capacity,
                access,
            )
        }?;
        // SAFETY: fresh export belongs only to this ticket and exact native
        // target. The returned authority is retained in this view/monitor.
        projection.authority = Some(unsafe { ticket.bind(identity, target) }?);
        Ok(projection)
    }
}
fn query_range(fd: &OwnedFd) -> io::Result<Range> {
    let mut query = QueryRange {
        index: 0,
        reserved: 0,
        range: Range { offset: 0, size: 0 },
    };
    if unsafe { libc::ioctl(fd.as_raw_fd(), RANGE, &mut query) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if query.index != 0 || query.reserved != 0 {
        return Err(denied("native range query identity changed"));
    }
    Ok(query.range)
}
fn create_checked(
    device: &File,
    b: &SealedBacking,
    extent: Extent,
    access: ProjectionAccess,
) -> io::Result<(Mapping, OwnedFd, OwnedFd, Info)> {
    b.bounds(extent.offset, extent.capacity)?;
    let flags = u32::from(access == ProjectionAccess::ReadOnly);
    let range = Range {
        offset: extent.offset,
        size: extent.capacity,
    };
    let mut create = Create {
        version: 1,
        flags,
        memfd: b.original.as_raw_fd() as u32,
        count: 1,
        ranges: (&range as *const Range) as u64,
        data_fd: -1,
        monitor_fd: -1,
        reserved: 0,
    };
    if unsafe { libc::ioctl(device.as_raw_fd(), CREATE, &mut create) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if create.data_fd < 0 || create.monitor_fd < 0 || create.data_fd == create.monitor_fd {
        // A trusted reviewed kernel cannot return this shape. Refuse and
        // retain any ambiguous descriptors rather than close unrelated FDs.
        return Err(denied("invalid protected kernel descriptor result"));
    }
    let data = unsafe { OwnedFd::from_raw_fd(create.data_fd) };
    let monitor = unsafe { OwnedFd::from_raw_fd(create.monitor_fd) };
    let actual_access = unsafe { libc::fcntl(data.as_raw_fd(), libc::F_GETFL) };
    let wanted_access = if flags == 0 {
        libc::O_RDWR
    } else {
        libc::O_RDONLY
    };
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if actual_access < 0
        || actual_access & libc::O_ACCMODE != wanted_access
        || unsafe { libc::fstat(data.as_raw_fd(), &mut stat) } != 0
        || stat.st_size < 0
        || stat.st_size as u64 != extent.capacity
        || [&data, &monitor].iter().any(|fd| {
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            flags < 0 || flags & libc::FD_CLOEXEC == 0
        })
    {
        return Err(denied("protected kernel descriptor access/size differs"));
    }
    let info = query(&monitor)?;
    let expected = b.geometry.physical;
    if info.version != 1
        || info.flags != flags
        || info.state != 0
        || info.count != 1
        || info.instance == [0; 16]
        || info.export_id == 0
        || info.source_dev != expected.host_device
        || info.source_ino != expected.host_inode
        || info.source_bytes != expected.bar_bytes
        || info.bytes != extent.capacity
    {
        return Err(denied("protected kernel source/access identity differs"));
    }
    let range = query_range(&monitor)?;
    if range.offset != extent.offset || range.size != extent.capacity {
        return Err(denied("protected kernel physical extent differs"));
    }
    let length = usize::try_from(extent.capacity)
        .map_err(|_| denied("protected mapping size unsupported"))?;
    let prot = libc::PROT_READ | if flags == 0 { libc::PROT_WRITE } else { 0 };
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            prot,
            libc::MAP_SHARED,
            data.as_raw_fd(),
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok((Mapping { base, length }, data, monitor, info))
}
/// Owns the exact native monitor and original grant. No raw FD/desktop export.
pub struct ProtectedProjection {
    mapping: Mapping,
    data: OwnedFd,
    monitor: OwnedFd,
    expected: Info,
    grant: Arc<RetainedGrant<SealedBacking>>,
    local: Arc<LocalProcess>,
    logical: u64,
    access: ProjectionAccess,
    extent: Extent,
    provider: Arc<ProtectedProvider>,
    authority: Option<NativeProjectionAuthority>,
}
impl ProtectedProjection {
    /// A const pointer for both access modes. Writes require the separately
    /// checked RW maximum and the caller's shared-memory synchronization.
    pub fn address(&self) -> io::Result<*const u8> {
        self.local.current()?;
        Ok(self.mapping.base.cast())
    }
    pub fn access(&self) -> ProjectionAccess {
        self.access
    }
    pub fn length(&self) -> u64 {
        self.logical
    }
    pub fn capacity(&self) -> u64 {
        self.mapping.length as u64
    }
    /// Close this CPU view and descriptor while retaining the exact monitor
    /// and original owner. Other aliases and native fences may still be live.
    pub fn into_monitor(self) -> ProtectedMonitor {
        let Self {
            mapping,
            data,
            monitor,
            expected,
            grant,
            local,
            extent,
            provider,
            authority,
            ..
        } = self;
        drop(mapping);
        drop(data);
        ProtectedMonitor {
            monitor,
            expected,
            _grant: grant,
            local,
            extent,
            provider,
            authority,
            poisoned: AtomicBool::new(false),
        }
    }
}
/// Exact retained kernel monitor only. A Retired result supplies no canonical
/// release authority, cross-VM completion or GPU correctness proof.
pub struct ProtectedMonitor {
    monitor: OwnedFd,
    expected: Info,
    _grant: Arc<RetainedGrant<SealedBacking>>,
    local: Arc<LocalProcess>,
    extent: Extent,
    provider: Arc<ProtectedProvider>,
    authority: Option<NativeProjectionAuthority>,
    poisoned: AtomicBool,
}
impl ProtectedMonitor {
    fn checked_state(&self) -> io::Result<NativeExportState> {
        if self.poisoned.load(Ordering::SeqCst) {
            return Err(denied("native monitor permanently quarantined"));
        }
        let result = (|| {
            self.local.current()?;
            self.provider.validate()?;
            self._grant.original().validate()?;
            let current = query(&self.monitor)?;
            if immutable(current) != self.expected {
                return Err(denied("retained native monitor identity changed"));
            }
            let range = query_range(&self.monitor)?;
            if range.offset != self.extent.offset || range.size != self.extent.capacity {
                return Err(denied("retained native monitor range changed"));
            }
            state(current.state)
        })();
        if result.is_err() || matches!(result, Ok(NativeExportState::Failed)) {
            self.poisoned.store(true, Ordering::SeqCst);
        }
        result
    }
    /// Local native fact only; never a canonical release or proof constructor.
    pub fn native_state(&self) -> io::Result<NativeExportState> {
        self.checked_state()
    }
    /// Produce one owner-bound proof only after this exact export's irreversible
    /// kernel all-alias/fence retirement. LIVE/DRAINING retains its authority;
    /// any uncertain native/source/provider query permanently quarantines it.
    pub fn try_retirement(&mut self) -> io::Result<Option<RetirementProof>> {
        if self.authority.is_none() {
            return Err(unsupported(
                "untracked or already consumed native authority",
            ));
        }
        match self.checked_state()? {
            NativeExportState::Live | NativeExportState::Draining => Ok(None),
            NativeExportState::Failed => Err(denied("native export failed, authority retained")),
            NativeExportState::Retired => {
                let authority = self.authority.take().unwrap();
                // SAFETY: exact retained reviewed monitor, immutable source/
                // access/range and current provider/source were revalidated.
                // RETIRED is published only after all exporter aliases, native
                // holds and captured fences have ended. No process exit or
                // caller enum supplies this authority. This private backend
                // offers no hardware GPU/desktop import route.
                match unsafe { authority.prove_retired() } {
                    Ok(proof) => Ok(Some(proof)),
                    Err(error) => {
                        self.poisoned.store(true, Ordering::SeqCst);
                        Err(error)
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_abi_sizes_and_unknown_retirement_fail_closed() {
        assert_eq!(
            (
                std::mem::size_of::<Create>(),
                std::mem::size_of::<Info>(),
                std::mem::size_of::<QueryRange>()
            ),
            (40, 72, 24)
        );
        assert!(state(4).is_err());
        assert!(state(u32::MAX).is_err());
        let info = Info {
            state: 2,
            export_id: 7,
            ..Info::default()
        };
        assert_eq!(immutable(info).export_id, 7);
        assert_eq!(immutable(info).state, 0);
        assert_ne!(
            immutable(info),
            immutable(Info {
                source_bytes: 4096,
                ..info
            })
        );
    }
    #[test]
    #[ignore = "requires private matching reviewed protected-overlay kernel fixture"]
    fn native_private_overlay_mapping_and_monitor() {
        let local = unsafe {
            ServiceLocalBackend::for_current_service(&ServicePolicy::linux_root()).unwrap()
        };
        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/udmabuf-protected-test")
            .unwrap();
        let backend = unsafe { ProtectedBackend::from_verified_device(local, device).unwrap() };
        let (file, geometry) = super::super::tests::memfd(super::super::tests::stable());
        file.write_all_at(&[0x91; 16384], 0).unwrap();
        let b = unsafe { SealedBacking::from_original(file, geometry, Arc::new(())).unwrap() };
        let extent = Extent {
            offset: 4096,
            length: 4096,
            capacity: 4096,
        };
        for access in [ProjectionAccess::ReadOnly, ProjectionAccess::ReadWrite] {
            let (mapping, data, monitor, info) =
                create_checked(&backend.provider.device, &b, extent, access).unwrap();
            assert_eq!(query(&monitor).unwrap(), info);
            assert_eq!(
                unsafe { std::ptr::read_volatile(mapping.base.cast::<u8>()) },
                0x91
            );
            assert_ne!(
                unsafe {
                    libc::mprotect(
                        mapping.base,
                        mapping.length,
                        libc::PROT_READ | libc::PROT_EXEC,
                    )
                },
                0
            );
            if access == ProjectionAccess::ReadOnly {
                assert_ne!(
                    unsafe {
                        libc::mprotect(
                            mapping.base,
                            mapping.length,
                            libc::PROT_READ | libc::PROT_WRITE,
                        )
                    },
                    0
                );
            } else {
                unsafe {
                    std::ptr::write_volatile(mapping.base.cast::<u8>(), 0x3d);
                }
                let mut byte = [0];
                b.original.read_exact_at(&mut byte, 4096).unwrap();
                assert_eq!(byte, [0x3d]);
            }
            drop(data);
            assert_eq!(
                query(&monitor).unwrap().state,
                0,
                "VMA still retains actual export"
            );
            drop(mapping);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let current = query(&monitor).unwrap();
                assert_eq!(immutable(current), immutable(info));
                if current.state == 2 {
                    break;
                }
                assert!(current.state == 0 || current.state == 1);
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        let mut header = [0; 4096];
        let mut adjacent = [0; 8192];
        b.original.read_exact_at(&mut header, 0).unwrap();
        b.original.read_exact_at(&mut adjacent, 8192).unwrap();
        assert!(header.iter().chain(adjacent.iter()).all(|v| *v == 0x91));
        println!(
            "PRIVATE OVERLAY: exact original geometry/access/range monitor, RO write/exec refusal, RW original write, VMA-held LIVE, final native RETIRED; no canonical release"
        );
    }
}
