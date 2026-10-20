//! Durable exclusions for original BAR bytes. This is not an allocator or
//! proof of canonical grant ownership. No record is ever released here.
//!
//! Unactivated until every original allocator consults these exclusions. The
//! Unix store is implemented; a Windows protected durable-store adapter is
//! still required before Windows-side activation. Neither store detects a
//! privileged restoration of an older valid filesystem image: canonical
//! provisioning/recovery must never roll back this state while backing lives.
use crate::service_control::{Binding, Message, Origin, Payload, Phase};
use std::io;

/// Whole original backing file at byte zero. Enrollment must derive dev/ino
/// and exact file length from its retained privileged fd, not a path or wire
/// message. Nonzero backend offsets/subfile aliases are unsupported and must
/// be refused, since this schema cannot identify their physical aliasing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalBacking {
    pub host_device: u64,
    pub host_inode: u64,
    pub bar_bytes: u64,
}

/// Exclusive protected storage, retained for the registry's whole lifetime.
///
/// # Safety
/// The implementation must exclude concurrent readers/writers using the same
/// canonical physical-backing store, reject untrusted ownership/permissions,
/// and never create missing state on load. replace_synced must atomically
/// replace the complete snapshot and flush both contents and namespace before
/// success. Any uncertain write outcome must return an error. The production
/// owner must select one canonical store for the retained original backing;
/// aliases and service/VM epochs must not select a fresh store.
pub unsafe trait DurableStore {
    fn load(&mut self) -> io::Result<Vec<u8>>;
    fn replace_synced(&mut self, bytes: &[u8]) -> io::Result<()>;
}

pub struct EpochTicket {
    backing: PhysicalBacking,
    origin: Origin,
    value: u64,
}
impl EpochTicket {
    pub fn value(&self) -> u64 {
        self.value
    }
    pub fn origin(&self) -> Origin {
        self.origin
    }
}
/// Issued only after the complete reservation snapshot is durable. This does
/// not establish that the original compositor actually delegated the grant.
pub struct Reservation {
    binding: Binding,
}
impl Reservation {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
}

pub struct Registry<S: DurableStore> {
    store: S,
    backing: PhysicalBacking,
    local_origin: Origin,
    local_issued: bool,
    pair_ready: bool,
    next_epoch: u64,
    windows_epoch: u64,
    linux_epoch: u64,
    records: Vec<Binding>,
    poisoned: bool,
}
fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
const MAGIC: &[u8] = b"WCBARREG01";
const END: &[u8] = b"END-WCBARREG01";
const MAX: usize = 16 * 1024 * 1024;
impl<S: DurableStore> Registry<S> {
    /// Establish an explicitly fresh original backing, never recover missing
    /// state by assuming it is fresh.
    ///
    /// # Safety
    /// The privileged canonical owner must prove this is the first enrollment
    /// of these exact retained backing bytes, with no old mappings, grants or
    /// writers. The store must be newly provisioned, protected and exclusive.
    /// A new service epoch, VM run, path or driver generation is not that proof.
    pub unsafe fn initialize_fresh(
        mut store: S,
        backing: PhysicalBacking,
        local_origin: Origin,
    ) -> io::Result<Self> {
        validate_backing(backing)?;
        // Even privileged initialization must not overwrite an existing store.
        match store.load() {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(invalid("BAR registry already exists")),
            Err(e) => return Err(e),
        }
        let mut result = Self {
            store,
            backing,
            local_origin,
            local_issued: false,
            pair_ready: false,
            next_epoch: 1,
            windows_epoch: 0,
            linux_epoch: 0,
            records: Vec::new(),
            poisoned: false,
        };
        result.persist()?;
        Ok(result)
    }
    pub fn open(mut store: S, expected: PhysicalBacking, local_origin: Origin) -> io::Result<Self> {
        validate_backing(expected)?;
        let bytes = store.load()?;
        if bytes.len() > MAX {
            return Err(invalid("BAR registry too large"));
        }
        if bytes.len() < 8 {
            return Err(invalid("truncated BAR registry checksum"));
        }
        let (body, checksum) = bytes.split_at(bytes.len() - 8);
        if crc64(body).to_le_bytes() != checksum {
            return Err(invalid("corrupt BAR registry checksum"));
        }
        let mut rd = Cursor(body);
        rd.expect(MAGIC)?;
        let recorded_origin = match rd.u64()? {
            1 => Origin::Windows,
            2 => Origin::Linux,
            _ => return Err(invalid("invalid registry origin")),
        };
        if recorded_origin != local_origin {
            return Err(invalid("registry belongs to another local service origin"));
        }
        let backing = PhysicalBacking {
            host_device: rd.u64()?,
            host_inode: rd.u64()?,
            bar_bytes: rd.u64()?,
        };
        if backing != expected {
            return Err(invalid("BAR registry original backing differs"));
        }
        let next_epoch = rd.u64()?;
        let windows_epoch = rd.u64()?;
        let linux_epoch = rd.u64()?;
        let local_epoch = match local_origin {
            Origin::Windows => windows_epoch,
            Origin::Linux => linux_epoch,
        };
        if next_epoch == 0 || local_epoch >= next_epoch {
            return Err(invalid("invalid durable service epochs"));
        }
        let count = rd.u64()?;
        if count > (MAX / 64) as u64 {
            return Err(invalid("too many BAR reservations"));
        }
        let mut records = Vec::new();
        for _ in 0..count {
            let n = usize::try_from(rd.u64()?).map_err(|_| invalid("record length overflow"))?;
            let message = Message::decode(rd.take(n)?)?;
            if message.phase != Phase::Grant
                || message.origin != Origin::Windows
                || message.sequence != 1
            {
                return Err(invalid("noncanonical BAR reservation"));
            }
            check_binding(backing, &message.binding)?;
            if message.binding.windows_run > windows_epoch
                || message.binding.linux_run > linux_epoch
            {
                return Err(invalid("reservation has unissued service epoch"));
            }
            check_overlap(&records, &message.binding)?;
            records.push(message.binding);
        }
        rd.expect(END)?;
        if !rd.0.is_empty() {
            return Err(invalid("trailing BAR registry bytes"));
        }
        Ok(Self {
            store,
            backing,
            local_origin,
            local_issued: false,
            pair_ready: false,
            next_epoch,
            windows_epoch,
            linux_epoch,
            records,
            poisoned: false,
        })
    }
    /// The ticket is still this live registry instance's issued local epoch.
    /// This establishes no peer authority or pair readiness.
    pub fn owns_current_ticket(&self, ticket: &EpochTicket) -> bool {
        let active = match self.local_origin {
            Origin::Windows => self.windows_epoch,
            Origin::Linux => self.linux_epoch,
        };
        !self.poisoned
            && self.local_issued
            && ticket.backing == self.backing
            && ticket.origin == self.local_origin
            && ticket.value == active
    }
    /// Exact durably confirmed pair for the current live local ticket.
    pub fn confirmed_pair(&self, local: &EpochTicket, peer_epoch: u64) -> bool {
        let peer = match self.local_origin {
            Origin::Windows => self.linux_epoch,
            Origin::Linux => self.windows_epoch,
        };
        self.owns_current_ticket(local) && self.pair_ready && peer_epoch != 0 && peer_epoch == peer
    }
    /// All physical extents, including prior epochs and uncertain old grants.
    /// Offsets are full original BAR offsets, including its header prefix.
    pub fn exclusions(&self) -> Vec<(u64, u64)> {
        self.records
            .iter()
            .map(|b| (b.grant_offset, b.grant_length))
            .collect()
    }
    pub fn issue_next_service_epoch(&mut self, origin: Origin) -> io::Result<EpochTicket> {
        self.live()?;
        if origin != self.local_origin {
            return Err(invalid("cannot issue a peer service epoch"));
        }
        self.pair_ready = false;
        let value = self.next_epoch;
        self.next_epoch = value
            .checked_add(1)
            .ok_or_else(|| invalid("service epochs exhausted"))?;
        match origin {
            Origin::Windows => self.windows_epoch = value,
            Origin::Linux => self.linux_epoch = value,
        }
        self.persist()?;
        self.local_issued = true;
        Ok(EpochTicket {
            backing: self.backing,
            origin,
            value,
        })
    }
    /// Persist the peer epoch proved by the live lifecycle handshake.
    ///
    /// # Safety
    /// Only the lifecycle owning this exact freshly issued local ticket may
    /// call this after BOTH live challenges have been authenticated against
    /// the pre-enrolled exact source, installation, physical device and role.
    /// The peer must acknowledge this local epoch and its own current epoch.
    /// A wire number, ordering against the local counter, old frame or old
    /// journal pair is not proof. Same-epoch live reconnect must retain the
    /// identical lifecycle/authenticator, never reset replay counters.
    pub unsafe fn confirm_peer_epoch(
        &mut self,
        local: &EpochTicket,
        peer_epoch: u64,
    ) -> io::Result<()> {
        self.live()?;
        let active = match self.local_origin {
            Origin::Windows => self.windows_epoch,
            Origin::Linux => self.linux_epoch,
        };
        if !self.local_issued
            || self.pair_ready
            || local.backing != self.backing
            || local.origin != self.local_origin
            || local.value != active
            || peer_epoch == 0
        {
            return Err(invalid(
                "peer confirmation does not match fresh local ticket",
            ));
        }
        let prior = match self.local_origin {
            Origin::Windows => self.linux_epoch,
            Origin::Linux => self.windows_epoch,
        };
        if peer_epoch < prior {
            return Err(invalid("peer service epoch moved backwards"));
        }
        match self.local_origin {
            Origin::Windows => self.linux_epoch = peer_epoch,
            Origin::Linux => self.windows_epoch = peer_epoch,
        }
        self.persist()?;
        self.pair_ready = true;
        Ok(())
    }
    /// Reserve a real original owner's canonical grant before any mapping,
    /// publication or authenticated Grant frame can expose its bytes.
    ///
    /// # Safety
    /// The caller must retain the actual original backing/kernel device and
    /// canonical grant guard, derive physical offsets with the arena header
    /// base included, and enroll every allocator route under these exclusions.
    /// A wire Binding, shared directory or journal record is not delegation.
    pub unsafe fn reserve(
        &mut self,
        ticket: &EpochTicket,
        binding: Binding,
    ) -> io::Result<Reservation> {
        self.live()?;
        let active = match ticket.origin {
            Origin::Windows => self.windows_epoch,
            Origin::Linux => self.linux_epoch,
        };
        if !self.pair_ready
            || !self.local_issued
            || ticket.origin != self.local_origin
            || ticket.backing != self.backing
            || ticket.value != active
            || active == 0
            || binding.windows_run != self.windows_epoch
            || binding.linux_run != self.linux_epoch
        {
            return Err(invalid("stale or mismatched service epoch ticket"));
        }
        check_binding(self.backing, &binding)?;
        check_overlap(&self.records, &binding)?;
        self.records.push(binding.clone());
        self.persist()?;
        Ok(Reservation { binding })
    }
    fn live(&self) -> io::Result<()> {
        if self.poisoned {
            Err(invalid(
                "BAR registry write outcome uncertain; reopening required",
            ))
        } else {
            Ok(())
        }
    }
    fn persist(&mut self) -> io::Result<()> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(self.local_origin as u64).to_le_bytes());
        for n in [
            self.backing.host_device,
            self.backing.host_inode,
            self.backing.bar_bytes,
            self.next_epoch,
            self.windows_epoch,
            self.linux_epoch,
            self.records.len() as u64,
        ] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        for binding in &self.records {
            let encoded = Message {
                binding: binding.clone(),
                origin: Origin::Windows,
                sequence: 1,
                phase: Phase::Grant,
                payload: Payload::default(),
            }
            .encode()?;
            bytes.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
            bytes.extend_from_slice(&encoded);
        }
        bytes.extend_from_slice(END);
        bytes.extend_from_slice(&crc64(&bytes).to_le_bytes());
        if bytes.len() > MAX {
            self.poisoned = true;
            return Err(invalid("BAR registry full; grants retained"));
        }
        if let Err(e) = self.store.replace_synced(&bytes) {
            self.poisoned = true;
            return Err(e);
        }
        Ok(())
    }
}
fn validate_backing(b: PhysicalBacking) -> io::Result<()> {
    if b.host_device == 0 || b.host_inode == 0 || b.bar_bytes == 0 {
        Err(invalid("missing original backing identity"))
    } else {
        Ok(())
    }
}
fn check_binding(backing: PhysicalBacking, b: &Binding) -> io::Result<()> {
    Message {
        binding: b.clone(),
        origin: Origin::Windows,
        sequence: 1,
        phase: Phase::Grant,
        payload: Payload::default(),
    }
    .validate()?;
    if b.source.host_device != backing.host_device
        || b.source.host_inode != backing.host_inode
        || b.source.bar_bytes != backing.bar_bytes
        || !b.grant_offset.is_multiple_of(4096)
        || !b.grant_length.is_multiple_of(4096)
    {
        return Err(invalid("grant differs from retained physical BAR geometry"));
    }
    Ok(())
}
fn check_overlap(records: &[Binding], b: &Binding) -> io::Result<()> {
    let end = b
        .grant_offset
        .checked_add(b.grant_length)
        .ok_or_else(|| invalid("grant overflow"))?;
    if records
        .iter()
        .any(|old| b.grant_offset < old.grant_offset + old.grant_length && old.grant_offset < end)
    {
        return Err(invalid(
            "physical BAR extent overlaps retained prior reservation",
        ));
    }
    Ok(())
}
// Detect accidental snapshot corruption. This is not authentication or a
// replacement for protected ownership and original-owner enrollment.
fn crc64(bytes: &[u8]) -> u64 {
    let mut crc = 0u64;
    for byte in bytes {
        crc ^= u64::from(*byte) << 56;
        for _ in 0..8 {
            crc = if crc & (1 << 63) != 0 {
                (crc << 1) ^ 0x42f0_e1eb_a9ea_3693
            } else {
                crc << 1
            };
        }
    }
    crc
}
struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if n > self.0.len() {
            return Err(invalid("truncated BAR registry"));
        }
        let (out, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(out)
    }
    fn expect(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.take(bytes.len())? != bytes {
            return Err(invalid("BAR registry format differs"));
        }
        Ok(())
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

#[cfg(unix)]
pub mod unix;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_control::{Roles, Source};
    use std::sync::{Arc, Mutex};
    #[derive(Clone, Default)]
    struct Memory {
        bytes: Arc<Mutex<Option<Vec<u8>>>>,
        failure: Arc<Mutex<u8>>,
    }
    // SAFETY: tests serialize calls, simulate both atomic outcomes, and never
    // expose real physical memory. Permission validation is tested separately.
    unsafe impl DurableStore for Memory {
        fn load(&mut self) -> io::Result<Vec<u8>> {
            self.bytes
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
        fn replace_synced(&mut self, bytes: &[u8]) -> io::Result<()> {
            let failure = *self.failure.lock().unwrap();
            if failure == 1 {
                return Err(io::Error::other("crash before rename"));
            }
            *self.bytes.lock().unwrap() = Some(bytes.to_vec());
            if failure == 2 {
                return Err(io::Error::other(
                    "crash after rename before directory flush",
                ));
            }
            Ok(())
        }
    }
    pub(super) fn backing() -> PhysicalBacking {
        PhysicalBacking {
            host_device: 7,
            host_inode: 9,
            bar_bytes: 1024 * 1024,
        }
    }
    pub(super) fn binding(w: u64, l: u64, offset: u64) -> Binding {
        Binding {
            installation: "test".into(),
            source: Source {
                vm: "private".into(),
                windows_device: "PCI\\PRIVATE".into(),
                host_device: 7,
                host_inode: 9,
                qemu_start: 11,
                driver_generation: 12,
                bar_run: 13,
                bar_bytes: backing().bar_bytes,
            },
            roles: Roles {
                grantor: Origin::Windows,
                producer: Origin::Windows,
                consumer: Origin::Linux,
            },
            windows_run: w,
            linux_run: l,
            grant: 1,
            session: 1,
            grant_offset: offset,
            grant_length: 4096,
            windows_sid: "S-1-5-21-1".into(),
            linux_uid: 1000,
        }
    }
    fn fresh(memory: Memory) -> Registry<Memory> {
        unsafe { Registry::initialize_fresh(memory, backing(), Origin::Windows).unwrap() }
    }
    #[test]
    fn restart_and_all_generation_changes_keep_original_byte_exclusions() {
        let memory = Memory::default();
        let mut registry = fresh(memory.clone());
        let w = registry.issue_next_service_epoch(Origin::Windows).unwrap();
        let l = 101;
        unsafe {
            registry.confirm_peer_epoch(&w, l).unwrap();
        }
        unsafe {
            registry.reserve(&w, binding(w.value(), l, 4096)).unwrap();
        }
        drop(registry);
        let mut registry = Registry::open(memory, backing(), Origin::Windows).unwrap();
        let w2 = registry.issue_next_service_epoch(Origin::Windows).unwrap();
        let l2 = 102;
        unsafe {
            registry.confirm_peer_epoch(&w2, l2).unwrap();
        }
        assert!(w2.value() > w.value());
        let mut changed = binding(w2.value(), l2, 4096);
        changed.source.bar_run += 1;
        changed.source.driver_generation += 1;
        changed.source.qemu_start += 1;
        changed.source.windows_device = "PCI\\RENAMED".into();
        changed.source.vm = "renamed".into();
        changed.session = 2;
        changed.grant = 2;
        changed.windows_sid = "S-1-5-21-2".into();
        changed.linux_uid = 1001;
        assert!(unsafe { registry.reserve(&w2, changed.clone()) }.is_err());
        changed.grant_offset = 8192;
        assert!(unsafe { registry.reserve(&w, changed.clone()) }.is_err());
        unsafe {
            registry.reserve(&w2, changed).unwrap();
        }
        assert_eq!(registry.exclusions(), vec![(4096, 4096), (8192, 4096)]);
    }
    #[test]
    fn uncertain_commit_never_returns_authority_and_poisoned_instance_cannot_continue() {
        for failure in [1, 2] {
            let memory = Memory::default();
            let mut registry = fresh(memory.clone());
            let w = registry.issue_next_service_epoch(Origin::Windows).unwrap();
            let l = 101;
            unsafe {
                registry.confirm_peer_epoch(&w, l).unwrap();
            }
            *memory.failure.lock().unwrap() = failure;
            assert!(unsafe { registry.reserve(&w, binding(w.value(), l, 4096)) }.is_err());
            assert!(registry.issue_next_service_epoch(Origin::Windows).is_err());
            drop(registry);
            *memory.failure.lock().unwrap() = 0;
            let registry = Registry::open(memory, backing(), Origin::Windows).unwrap();
            assert_eq!(registry.exclusions().len(), usize::from(failure == 2));
        }
    }
    #[test]
    fn local_and_peer_epochs_are_independent_and_pairs_require_fresh_confirmation() {
        let memory = Memory::default();
        let mut registry = fresh(memory.clone());
        assert!(registry.issue_next_service_epoch(Origin::Linux).is_err());
        let w = registry.issue_next_service_epoch(Origin::Windows).unwrap();
        assert!(registry.owns_current_ticket(&w));
        assert!(!registry.confirmed_pair(&w, 500));
        assert!(unsafe { registry.reserve(&w, binding(w.value(), 500, 4096)) }.is_err());
        unsafe {
            registry.confirm_peer_epoch(&w, 500).unwrap();
        }
        assert!(registry.confirmed_pair(&w, 500));
        assert!(!registry.confirmed_pair(&w, 499));
        assert!(unsafe { registry.confirm_peer_epoch(&w, 500) }.is_err());
        unsafe {
            registry.reserve(&w, binding(w.value(), 500, 4096)).unwrap();
        }
        let w2 = registry.issue_next_service_epoch(Origin::Windows).unwrap();
        assert_eq!(w2.value(), 2); // Peer ordering cannot issue our epoch.
        assert!(!registry.owns_current_ticket(&w));
        assert!(unsafe { registry.reserve(&w2, binding(w2.value(), 500, 8192)) }.is_err());
        assert!(unsafe { registry.confirm_peer_epoch(&w, 501) }.is_err());
        assert!(unsafe { registry.confirm_peer_epoch(&w2, 499) }.is_err());
        unsafe {
            registry.confirm_peer_epoch(&w2, 500).unwrap();
        }
        drop(registry);
        let mut registry = Registry::open(memory, backing(), Origin::Windows).unwrap();
        assert!(!registry.owns_current_ticket(&w2));
        assert!(unsafe { registry.confirm_peer_epoch(&w2, 500) }.is_err());
        assert!(unsafe { registry.reserve(&w2, binding(w2.value(), 500, 8192)) }.is_err());
        assert_eq!(registry.exclusions(), vec![(4096, 4096)]);
    }

    #[test]
    fn peer_confirmation_is_durable_before_admission_and_never_resumes_on_open() {
        for failure in [1, 2] {
            let memory = Memory::default();
            let mut registry = fresh(memory.clone());
            let local = registry.issue_next_service_epoch(Origin::Windows).unwrap();
            *memory.failure.lock().unwrap() = failure;
            assert!(unsafe { registry.confirm_peer_epoch(&local, 500) }.is_err());
            assert!(!registry.confirmed_pair(&local, 500));
            assert!(
                unsafe { registry.reserve(&local, binding(local.value(), 500, 4096)) }.is_err()
            );
            drop(registry);
            *memory.failure.lock().unwrap() = 0;
            let mut registry = Registry::open(memory, backing(), Origin::Windows).unwrap();
            assert!(!registry.confirmed_pair(&local, 500));
            assert!(unsafe { registry.confirm_peer_epoch(&local, 500) }.is_err());
            let new_local = registry.issue_next_service_epoch(Origin::Windows).unwrap();
            if failure == 2 {
                assert!(unsafe { registry.confirm_peer_epoch(&new_local, 499) }.is_err());
            }
            unsafe {
                registry.confirm_peer_epoch(&new_local, 500).unwrap();
            }
            assert!(registry.confirmed_pair(&new_local, 500));
        }
    }

    #[test]
    fn missing_truncated_and_wrong_original_backing_fail_closed() {
        assert!(Registry::open(Memory::default(), backing(), Origin::Windows).is_err());
        let memory = Memory::default();
        let mut original = fresh(memory.clone());
        let ticket = original.issue_next_service_epoch(Origin::Windows).unwrap();
        unsafe {
            original.confirm_peer_epoch(&ticket, 101).unwrap();
        }
        unsafe {
            original
                .reserve(&ticket, binding(ticket.value(), 101, 4096))
                .unwrap();
        }
        drop(original);
        assert!(
            unsafe { Registry::initialize_fresh(memory.clone(), backing(), Origin::Windows) }
                .is_err()
        );
        let mut wrong = backing();
        wrong.host_inode += 1;
        assert!(Registry::open(memory.clone(), wrong, Origin::Windows).is_err());
        let bytes = memory.bytes.lock().unwrap().clone().unwrap();
        for n in 0..bytes.len() {
            *memory.bytes.lock().unwrap() = Some(bytes[..n].to_vec());
            assert!(
                Registry::open(memory.clone(), backing(), Origin::Windows).is_err(),
                "prefix {n}"
            );
            let mut damaged = bytes.clone();
            damaged[n] ^= 1;
            *memory.bytes.lock().unwrap() = Some(damaged);
            assert!(
                Registry::open(memory.clone(), backing(), Origin::Windows).is_err(),
                "bit corruption {n}"
            );
        }
    }
    #[test]
    fn epoch_flush_failures_and_exhaustion_never_issue_reusable_tickets() {
        for failure in [1, 2] {
            let memory = Memory::default();
            let mut registry = fresh(memory.clone());
            *memory.failure.lock().unwrap() = failure;
            assert!(registry.issue_next_service_epoch(Origin::Windows).is_err());
            assert!(registry.issue_next_service_epoch(Origin::Linux).is_err());
            drop(registry);
            *memory.failure.lock().unwrap() = 0;
            let mut registry = Registry::open(memory, backing(), Origin::Windows).unwrap();
            let ticket = registry.issue_next_service_epoch(Origin::Windows).unwrap();
            assert_eq!(ticket.value(), if failure == 1 { 1 } else { 2 });
            registry.next_epoch = u64::MAX;
            assert!(registry.issue_next_service_epoch(Origin::Windows).is_err());
        }
    }
}

#[cfg(all(test, unix))]
mod file_tests {
    use super::tests::{backing, binding};
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn directory() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "wc-bar-registry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    #[test]
    fn crash_worker() {
        let Some(path) = std::env::var_os("WC_BAR_REGISTRY_CRASH_WORKER") else {
            return;
        };
        let store = unsafe { unix::DirectoryStore::open(std::path::Path::new(&path)).unwrap() };
        let mut registry =
            unsafe { Registry::initialize_fresh(store, backing(), Origin::Windows).unwrap() };
        let w = registry.issue_next_service_epoch(Origin::Windows).unwrap();
        let l = 101;
        unsafe {
            registry.confirm_peer_epoch(&w, l).unwrap();
        }
        unsafe {
            registry.reserve(&w, binding(w.value(), l, 4096)).unwrap();
        }
        // Bypass all destructors, as service termination would. The parent
        // must still retain the exact persisted physical-byte exclusion.
        unsafe { libc::_exit(0) }
    }
    #[test]
    fn actual_service_crash_retains_exclusion_and_file_lock() {
        let path = directory();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bar_registry::file_tests::crash_worker",
                "--nocapture",
            ])
            .env("WC_BAR_REGISTRY_CRASH_WORKER", &path)
            .status()
            .unwrap();
        assert!(status.success());
        let store = unsafe { unix::DirectoryStore::open(&path).unwrap() };
        assert!(unsafe { unix::DirectoryStore::open(&path) }.is_err());
        let mut registry = Registry::open(store, backing(), Origin::Windows).unwrap();
        assert_eq!(registry.exclusions(), vec![(4096, 4096)]);
        let w = registry.issue_next_service_epoch(Origin::Windows).unwrap();
        let l = 101;
        unsafe {
            registry.confirm_peer_epoch(&w, l).unwrap();
        }
        assert!(unsafe { registry.reserve(&w, binding(w.value(), l, 4096)) }.is_err());
        unsafe {
            registry.reserve(&w, binding(w.value(), l, 8192)).unwrap();
        }
        drop(registry);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn symlinks_permissions_pending_and_lost_state_fail_closed() {
        let path = directory();
        let mut store = unsafe { unix::DirectoryStore::open(&path).unwrap() };
        assert_eq!(store.load().unwrap_err().kind(), io::ErrorKind::NotFound);
        std::os::unix::fs::symlink("other", path.join("state")).unwrap();
        assert!(Registry::open(store, backing(), Origin::Windows).is_err());
        std::fs::remove_file(path.join("state")).unwrap();
        let store = unsafe { unix::DirectoryStore::open(&path).unwrap() };
        let registry =
            unsafe { Registry::initialize_fresh(store, backing(), Origin::Windows).unwrap() };
        drop(registry);
        std::fs::set_permissions(path.join("state"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        let store = unsafe { unix::DirectoryStore::open(&path).unwrap() };
        assert!(Registry::open(store, backing(), Origin::Windows).is_err());
        std::fs::set_permissions(path.join("state"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        let store = unsafe { unix::DirectoryStore::open(&path).unwrap() };
        let mut registry = Registry::open(store, backing(), Origin::Windows).unwrap();
        std::fs::write(path.join("pending"), b"interrupted write").unwrap();
        std::fs::set_permissions(path.join("pending"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        registry.issue_next_service_epoch(Origin::Windows).unwrap();
        assert!(!path.join("pending").exists());
        std::fs::remove_file(path.join("state")).unwrap();
        assert!(registry.issue_next_service_epoch(Origin::Windows).is_err());
        assert!(!path.join("state").exists());
        drop(registry);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(unsafe { unix::DirectoryStore::open(&path) }.is_err());
        std::fs::remove_dir_all(path).unwrap();
    }
}
