//! Protected original-page projection, restricted to this privileged service.
//!
//! Stock udmabuf exports O_RDWR and has no revocation/final-alias census. Thus
//! this adapter refuses consumer RO and all transferable desktop exports. It
//! never exposes original memfd/device/export FDs. The canonical owner retains
//! allocations with no death-based reclaim. This is unactivated preparation.
use crate::udma::{self, Limits, Part};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::Arc;
use wc_broker::bar_control::Extent;
use wc_broker::service_auth::{ServiceEndpoint, ServicePolicy, ServiceProcess};
use wc_broker::service_grants::{
    BackingGeometry, OriginalBacking, ProjectionAccess, ProjectionBackend, RetainedGrant,
};

fn denied(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, s)
}
fn unsupported(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, s)
}
fn page() -> io::Result<u64> {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n == 4096 {
        Ok(n as u64)
    } else {
        Err(unsupported(
            "protected payload requires native 4096-byte pages",
        ))
    }
}
/// Full original sealed memfd, never an exported/narrowed descriptor or a copy.
pub struct SealedBacking {
    original: File,
    geometry: BackingGeometry,
    _original_owner: Arc<dyn Send + Sync>,
}
impl SealedBacking {
    /// Enroll the independently retained original backend, not a caller path.
    ///
    /// # Safety
    /// The canonical privileged service must validate QMP/VM/kernel source
    /// identity and retain its actual original owner. These exact whole-file
    /// bytes at offset zero must be immutable in size and exclusively allocated
    /// through the canonical protected owner. No active legacy whole-BAR writer
    /// may overlap new grants. A matching inode or valid seal is not delegation.
    pub unsafe fn from_original(
        original: File,
        geometry: BackingGeometry,
        original_owner: Arc<dyn Send + Sync>,
    ) -> io::Result<Self> {
        let result = Self {
            original,
            geometry,
            _original_owner: original_owner,
        };
        result.validate()?;
        Ok(result)
    }
    fn validate(&self) -> io::Result<()> {
        let page = page()?;
        let m = self.original.metadata()?;
        let expected = self.geometry.physical;
        if !m.is_file()
            || m.dev() != expected.host_device
            || m.ino() != expected.host_inode
            || m.len() != expected.bar_bytes
            || m.len() == 0
            || !m.len().is_multiple_of(page)
            || !self.geometry.arena_base.is_multiple_of(page)
            || self.geometry.arena_base >= m.len()
        {
            return Err(denied("original sealed backing identity/geometry differs"));
        }
        let fd = self.original.as_raw_fd();
        let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
        let wanted = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        if seals < 0
            || seals & wanted != wanted
            || seals & (libc::F_SEAL_WRITE | libc::F_SEAL_FUTURE_WRITE) != 0
        {
            return Err(unsupported(
                "original backing lacks immutable-size seals or denies stock udmabuf writes",
            ));
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || flags & libc::O_ACCMODE != libc::O_RDWR {
            return Err(denied(
                "original backend lacks required retained read/write authority",
            ));
        }
        Ok(())
    }
    fn bounds(&self, offset: u64, length: u64) -> io::Result<()> {
        self.validate()?;
        let page = page()?;
        if length == 0
            || !offset.is_multiple_of(page)
            || !length.is_multiple_of(page)
            || offset < self.geometry.arena_base
            || !offset
                .checked_add(length)
                .is_some_and(|end| end <= self.geometry.physical.bar_bytes)
        {
            return Err(denied(
                "payload extent crosses original physical boundaries",
            ));
        }
        Ok(())
    }
}
// SAFETY: constructor retains the independently authenticated original owner;
// fstat/seals prevent substitution/resizing, and all offsets use physical bytes.
unsafe impl OriginalBacking for SealedBacking {
    fn geometry(&self) -> BackingGeometry {
        self.geometry
    }
    fn initialize_zeroed(&self, offset: u64, length: u64) -> io::Result<()> {
        self.bounds(offset, length)?;
        // Called by canonical owner only AFTER durable disjoint reservation.
        // Never use this to clear a prior uncertain or still-mapped grant.
        let zeros = [0u8; 65536];
        let mut at = offset;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| denied("zero extent overflow"))?;
        while at < end {
            let n = (end - at).min(zeros.len() as u64) as usize;
            self.original.write_all_at(&zeros[..n], at)?;
            at += n as u64;
        }
        Ok(())
    }
}
struct LocalProcess {
    object: OwnedFd,
    pid: u32,
    uid: u32,
}
impl LocalProcess {
    fn current(&self) -> io::Result<()> {
        if unsafe { libc::getpid() } as u32 != self.pid
            || unsafe { libc::getuid() } != self.uid
            || unsafe { libc::geteuid() } != self.uid
        {
            return Err(denied(
                "projection belongs to another service process or credentials",
            ));
        }
        if self.exited()? {
            return Err(denied("local service process exited"));
        }
        Ok(())
    }
}
// SAFETY: poll queries this exact retained current-process pidfd. Connection
// close, elapsed time and client PID values are never death evidence.
unsafe impl ServiceProcess for LocalProcess {
    fn exited(&self) -> io::Result<bool> {
        let mut p = libc::pollfd {
            fd: self.object.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut p, 1, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if p.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(denied("local service pidfd unavailable"));
        }
        Ok(p.revents & (libc::POLLIN | libc::POLLHUP) != 0)
    }
}
/// The only supported exposure is a local mapping inside this privileged
/// service. There is intentionally no desktop FD-transfer implementation.
pub struct ServiceLocalBackend {
    device: File,
    limits: Limits,
    local: Arc<LocalProcess>,
    target: Arc<dyn ServiceProcess>,
    _endpoint: ServiceEndpoint,
}
impl ServiceLocalBackend {
    /// Capture actual own credentials/pidfd and open udmabuf only here.
    ///
    /// # Safety
    /// The caller must be the protected root/configured dedicated service,
    /// with privileged code/configuration and no desktop-controlled execution.
    /// It must not fork inherited payload mappings into application processes.
    /// No memory is projected by this constructor, and policy comes from the
    /// protected launcher, never a client-selected UID.
    pub unsafe fn for_current_service(policy: &ServicePolicy) -> io::Result<Self> {
        page()?;
        let pid = unsafe { libc::getpid() } as u32;
        let uid = unsafe { libc::getuid() };
        if uid != unsafe { libc::geteuid() } {
            return Err(denied("ambiguous service credentials"));
        }
        // Self cannot be PID-reused while this constructor executes.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let local = Arc::new(LocalProcess {
            object: unsafe { OwnedFd::from_raw_fd(fd) },
            pid,
            uid,
        });
        let target: Arc<dyn ServiceProcess> = local.clone();
        let endpoint = unsafe {
            ServiceEndpoint::from_current_service(policy, wc_broker::User::uid(uid), target.clone())
        }?;
        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/udmabuf")?;
        let m = device.metadata()?;
        if m.mode() & libc::S_IFMT != libc::S_IFCHR {
            return Err(denied("udmabuf device is not a retained character device"));
        }
        Ok(Self {
            device,
            limits: Limits::read(),
            local,
            target,
            _endpoint: endpoint,
        })
    }
    fn authorize(
        &self,
        target: &Arc<dyn ServiceProcess>,
        access: ProjectionAccess,
    ) -> io::Result<()> {
        self.local.current()?;
        if !Arc::ptr_eq(&self.target, target) {
            return Err(denied(
                "projection target is not this exact retained service process",
            ));
        }
        if access != ProjectionAccess::ReadWrite {
            return Err(unsupported(
                "stock udmabuf exports O_RDWR; immutable read-only projection unavailable",
            ));
        }
        Ok(())
    }
    /// Register this exact native service object with the canonical owner.
    pub fn target(&self) -> Arc<dyn ServiceProcess> {
        self.target.clone()
    }
    /// Explicitly refuse transferable aliases until a kernel final-reference
    /// mechanism is available. PID death cannot retire dma-buf descendants.
    pub fn desktop_export(&self) -> io::Result<()> {
        Err(unsupported(
            "stock udmabuf has no immutable RO/final-alias retirement; desktop export disabled",
        ))
    }
}
// SAFETY: only exact own-service target, exact opaque retained grant/range,
// no exported descriptors and no death/Drop-based canonical reclamation.
unsafe impl ProjectionBackend<SealedBacking> for ServiceLocalBackend {
    type Projection = LocalProjection;
    fn project(
        &mut self,
        grant: Arc<RetainedGrant<SealedBacking>>,
        extent: Extent,
        target: Arc<dyn ServiceProcess>,
        access: ProjectionAccess,
    ) -> io::Result<LocalProjection> {
        self.authorize(&target, access)?;
        let binding = grant.binding();
        let b = grant.original();
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
                        .is_some_and(|grant_end| end <= grant_end)
                })
            || b.geometry.physical.host_device != binding.source.host_device
            || b.geometry.physical.host_inode != binding.source.host_inode
            || b.geometry.physical.bar_bytes != binding.source.bar_bytes
        {
            return Err(denied("projection differs from canonical physical grant"));
        }
        let parts = udma::create_with_device(
            self.device.as_fd(),
            b.original.as_fd(),
            &[(extent.offset, extent.capacity)],
            self.limits,
        )?;
        let mapping = Mapping::new(&parts, extent.capacity)?;
        Ok(LocalProjection {
            mapping,
            _parts: parts,
            _grant: grant,
            local: self.local.clone(),
            logical: extent.length,
        })
    }
}
struct Mapping {
    base: *mut libc::c_void,
    length: usize,
}
impl Mapping {
    fn new(parts: &[Part], length: u64) -> io::Result<Self> {
        let length = usize::try_from(length).map_err(|_| denied("projection size unsupported"))?;
        if length == 0 {
            return Err(denied("empty projection"));
        }
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let result = Self { base, length };
        let mut at = 0u64;
        for part in parts {
            let flags = unsafe { libc::fcntl(part.fd.as_raw_fd(), libc::F_GETFL) };
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if flags < 0
                || flags & libc::O_ACCMODE != libc::O_RDWR
                || unsafe { libc::fstat(part.fd.as_raw_fd(), &mut stat) } != 0
                || stat.st_size < 0
                || stat.st_size as u64 != part.len
                || part.offset != at
                || part.len == 0
                || !at
                    .checked_add(part.len)
                    .is_some_and(|end| end <= length as u64)
            {
                return Err(denied("kernel projection extent/access differs"));
            }
            let address = unsafe { base.cast::<u8>().add(at as usize) }.cast();
            let mapped = unsafe {
                libc::mmap(
                    address,
                    part.len as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    part.fd.as_raw_fd(),
                    0,
                )
            };
            if mapped == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            at += part.len;
        }
        if at != length as u64 {
            return Err(denied("kernel projection does not cover exact capacity"));
        }
        Ok(result)
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base, self.length);
        }
    }
}
/// Private FD/mapping ownership. Closing this local view is not a grant release.
/// Canonical allocator currently retains every grant without a reclaim API.
pub struct LocalProjection {
    mapping: Mapping,
    _parts: Vec<Part>,
    _grant: Arc<RetainedGrant<SealedBacking>>,
    local: Arc<LocalProcess>,
    logical: u64,
}
impl LocalProjection {
    /// Local service address only. Use volatile/shared-memory-safe access and
    /// actual producer/consumer synchronization; no Rust exclusive slice is
    /// constructed over potentially concurrently written original pages.
    pub fn address(&self) -> io::Result<*mut u8> {
        self.local.current()?;
        Ok(self.mapping.base.cast())
    }
    pub fn length(&self) -> u64 {
        self.logical
    }
    pub fn capacity(&self) -> u64 {
        self.mapping.length as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wc_broker::bar_registry::PhysicalBacking;
    fn memfd(seals: i32) -> (File, BackingGeometry) {
        let name = std::ffi::CString::new("protected-payload-fixture").unwrap();
        let fd = unsafe {
            libc::memfd_create(name.as_ptr(), libc::MFD_ALLOW_SEALING | libc::MFD_CLOEXEC)
        };
        assert!(fd >= 0);
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(16384).unwrap();
        if seals != 0 {
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) }, 0);
        }
        let m = file.metadata().unwrap();
        let geometry = BackingGeometry {
            physical: PhysicalBacking {
                host_device: m.dev(),
                host_inode: m.ino(),
                bar_bytes: m.len(),
            },
            arena_base: 4096,
        };
        (file, geometry)
    }
    fn stable() -> i32 {
        libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL
    }
    #[test]
    fn original_size_identity_and_seals_are_not_inferred_or_repaired() {
        for seals in [
            0,
            libc::F_SEAL_SHRINK,
            stable() | libc::F_SEAL_WRITE,
            stable() | libc::F_SEAL_FUTURE_WRITE,
        ] {
            let (file, geometry) = memfd(seals);
            assert!(unsafe { SealedBacking::from_original(file, geometry, Arc::new(())) }.is_err());
        }
        let (file, mut geometry) = memfd(stable());
        geometry.physical.host_inode += 1;
        assert!(unsafe { SealedBacking::from_original(file, geometry, Arc::new(())) }.is_err());
        let (file, mut geometry) = memfd(stable());
        geometry.physical.bar_bytes -= 4096;
        assert!(unsafe { SealedBacking::from_original(file, geometry, Arc::new(())) }.is_err());
        let (file, mut geometry) = memfd(stable());
        geometry.arena_base = 4097;
        assert!(unsafe { SealedBacking::from_original(file, geometry, Arc::new(())) }.is_err());
    }
    #[test]
    fn physical_header_and_full_page_capacity_are_bounded() {
        let (file, geometry) = memfd(stable());
        file.write_all_at(&[0x91; 16384], 0).unwrap();
        let original =
            unsafe { SealedBacking::from_original(file, geometry, Arc::new(())).unwrap() };
        for (offset, length) in [
            (0, 4096),
            (4097, 4096),
            (4096, 4097),
            (16384, 4096),
            (u64::MAX - 4095, 8192),
            (4096, 0),
        ] {
            assert!(original.initialize_zeroed(offset, length).is_err());
        }
        original.initialize_zeroed(4096, 8192).unwrap();
        let mut bytes = [0; 16384];
        original.original.read_exact_at(&mut bytes, 0).unwrap();
        assert!(bytes[..4096].iter().all(|b| *b == 0x91));
        assert!(bytes[4096..12288].iter().all(|b| *b == 0));
        assert!(bytes[12288..].iter().all(|b| *b == 0x91));
        assert!(original.original.set_len(8192).is_err());
        assert!(original.original.set_len(32768).is_err());
    }
    /// Must run explicitly as the protected root service in the private kernel
    /// fixture. Absence of udmabuf/pidfd is a failure, never a skipped success.
    #[test]
    #[ignore = "requires private Linux kernel fixture with root /dev/udmabuf"]
    fn native_original_range_and_maximum_access() {
        let backend = unsafe {
            ServiceLocalBackend::for_current_service(&ServicePolicy::linux_root()).unwrap()
        };
        let target = backend.target();
        backend
            .authorize(&target, ProjectionAccess::ReadWrite)
            .unwrap();
        assert!(
            backend
                .authorize(&target, ProjectionAccess::ReadOnly)
                .is_err()
        );
        assert!(backend.desktop_export().is_err());
        let other = unsafe {
            ServiceLocalBackend::for_current_service(&ServicePolicy::linux_root()).unwrap()
        };
        assert!(
            backend
                .authorize(&other.target(), ProjectionAccess::ReadWrite)
                .is_err()
        );
        let (file, geometry) = memfd(stable());
        file.write_all_at(&[0xA9; 16384], 0).unwrap();
        let original =
            unsafe { SealedBacking::from_original(file, geometry, Arc::new(())).unwrap() };
        original.initialize_zeroed(4096, 8192).unwrap();
        let parts = udma::create_with_device(
            backend.device.as_fd(),
            original.original.as_fd(),
            &[(4096, 8192)],
            Limits {
                list_limit: 1,
                size_limit: 4096,
            },
        )
        .unwrap();
        assert_eq!(parts.len(), 2);
        let mapping = Mapping::new(&parts, 8192).unwrap();
        unsafe {
            std::ptr::write_volatile(mapping.base.cast::<u8>(), 0x3D);
            std::ptr::write_volatile(mapping.base.cast::<u8>().add(4096 + 100), 0x7B);
        }
        let mut byte = [0];
        original.original.read_exact_at(&mut byte, 4096).unwrap();
        assert_eq!(byte, [0x3D]);
        original
            .original
            .read_exact_at(&mut byte, 8192 + 100)
            .unwrap();
        assert_eq!(byte, [0x7B]);
        for part in &parts {
            let past = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    8192,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    part.fd.as_raw_fd(),
                    0,
                )
            };
            assert_eq!(
                past,
                libc::MAP_FAILED,
                "export reached outside its exact original range"
            );
            let offset = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    4096,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    part.fd.as_raw_fd(),
                    4096,
                )
            };
            assert_eq!(
                offset,
                libc::MAP_FAILED,
                "export accepted out-of-range file offset"
            );
        }
        let ro = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_SHARED,
                parts[0].fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ro, libc::MAP_FAILED);
        assert_eq!(
            unsafe { libc::mprotect(ro, 4096, libc::PROT_READ | libc::PROT_WRITE) },
            0,
            "stock maximum authority assumption changed; re-audit RO policy"
        );
        unsafe {
            libc::munmap(ro, 4096);
        }
        let mut before = [0; 4096];
        let mut after = [0; 4096];
        original.original.read_exact_at(&mut before, 0).unwrap();
        original.original.read_exact_at(&mut after, 12288).unwrap();
        assert!(before.iter().chain(after.iter()).all(|b| *b == 0xA9));
        drop(mapping);
        drop(parts);
        println!(
            "ORIGINAL RANGE:8192 bytes; header/adjacent pages unchanged; two exact kernel exports; maximumRW upgrade proved; RO/desktop/foreign-target refused"
        );
    }
}
