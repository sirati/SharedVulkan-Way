//! Trusted allocation accounting, separate from client-writable BAR metadata.
//!
//! This table is broker memory. Directory words and client-provided counters
//! are never imported. Its run and allocation ids form a separate namespace
//! from legacy directory slots. A transport must authenticate every caller and
//! retain its actual process object before issuing a Process registration.
use crate::core::User;
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Process {
    run: u64,
    registration: u64,
}
impl Process {
    pub(crate) fn registration(self) -> u64 {
        self.registration
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub run: u64,
    pub allocation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// Byte offset in the complete original physical BAR/backing object.
    pub offset: u64,
    pub capacity: u64,
    pub length: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Transfer(u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Consumer(u64);
impl Consumer {
    pub(crate) fn reference(self) -> u64 {
        self.0
    }
}

/// Private non-native remote lineage pin. It is neither a process nor a
/// native retirement proof. Moving it from pending to accepted preserves hold.
pub(crate) struct ExternalHold {
    key: Key,
    id: u64,
    identity: Arc<()>,
}
struct Allocation {
    user: User,
    producer: Option<Process>,
    extent: Extent,
    pending: BTreeMap<Transfer, Process>,
    consumers: BTreeMap<Consumer, Process>,
    external: BTreeMap<u64, Arc<()>>,
}
/// An existing compositor-owned grant delegated to this broker namespace.
/// These values are retained authority records, never read from BAR words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantIdentity {
    pub bar_run: u64,
    /// Rooted physical BAR offset of the canonical allocator's offset zero.
    /// BarPool grants use arena-relative offsets after the ivshmem header.
    pub bar_offset_base: u64,
    pub session: u8,
    pub region: u32,
    pub user: User,
}
/// Authoritative counts and original extents. The shared directory is only
/// a publication mechanism and cannot authorize a mutation of this table.
pub struct Allocator {
    run: u64,
    grant: GrantIdentity,
    _retained_grant: Arc<dyn Send + Sync>,
    start: u64,
    end: u64,
    next: u64,
    processes: BTreeMap<Process, User>,
    allocations: BTreeMap<Key, Allocation>,
}
impl Drop for Allocator {
    fn drop(&mut self) {
        if !self.allocations.is_empty() {
            // In-process control teardown is not evidence that mapped bytes
            // are unused. Keep the canonical owner's guard rather than let
            // its destructor recycle a grant with outstanding holds.
            // This does not persist state across OS process termination;
            // protected startup must separately refuse unreconciled grants.
            let retained = std::mem::replace(&mut self._retained_grant, Arc::new(()));
            std::mem::forget(retained);
        }
    }
}
fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
impl Allocator {
    /// Suballocate only within a retained, preexisting compositor grant.
    /// This never creates a second allocator over the whole device.
    ///
    /// # Safety
    /// The caller must authenticate the delegating compositor/service using
    /// its retained kernel identity, validate that `grant` is its existing
    /// grant in this exact original BAR/session, and hold the grant against
    /// release or reuse for the lifetime of `retained_grant`. Neither public
    /// role strings nor any client-writable directory word establishes this.
    /// Before changing the grant's SID/UID owner, its full capacity including
    /// padding must be scrubbed/initialized by the privileged owner after all
    /// old holds drain, or the bytes must remain partitioned to the same user.
    pub unsafe fn from_delegated_grant(
        run: u64,
        identity: GrantIdentity,
        grant: wc_handle::barpool::Grant,
        retained_grant: Arc<dyn Send + Sync>,
    ) -> io::Result<Self> {
        let start = identity
            .bar_offset_base
            .checked_add(grant.offset)
            .ok_or_else(|| io::Error::other("BAR grant base overflow"))?;
        let end = start
            .checked_add(grant.len)
            .ok_or_else(|| io::Error::other("BAR grant overflow"))?;
        if run == 0
            || identity.bar_run == 0
            || identity.session == 0
            || identity.session != grant.session
            || identity.region != grant.id
            || start >= end
            || start % 4096 != 0
            || end % 4096 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid trusted BAR grant",
            ));
        }
        Ok(Self {
            run,
            grant: identity,
            _retained_grant: retained_grant,
            start,
            end,
            next: 1,
            processes: BTreeMap::new(),
            allocations: BTreeMap::new(),
        })
    }
    #[cfg(test)]
    fn new(run: u64, start: u64, end: u64) -> io::Result<Self> {
        // SAFETY: the test fixture owns the simulated session grant.
        unsafe {
            Self::from_delegated_grant(
                run,
                GrantIdentity {
                    bar_run: 19,
                    bar_offset_base: 0,
                    session: 1,
                    region: 256,
                    user: User::sid("S-1-5-21-1"),
                },
                wc_handle::barpool::Grant {
                    id: 256,
                    session: 1,
                    region: 0,
                    offset: start,
                    len: end - start,
                },
                Arc::new(()),
            )
        }
    }
    pub fn grant(&self) -> &GrantIdentity {
        &self.grant
    }
    fn serial(&mut self) -> io::Result<u64> {
        let value = self.next;
        self.next = value
            .checked_add(1)
            .ok_or_else(|| io::Error::other("BAR identities exhausted"))?;
        Ok(value)
    }
    /// Called only after OS authentication and successful process-object watch
    /// registration. No PID or shared-memory value is a Process registration.
    pub fn register_process(&mut self, user: User) -> io::Result<Process> {
        let process = Process {
            run: self.run,
            registration: self.serial()?,
        };
        self.processes.insert(process, user);
        Ok(process)
    }
    /// Undo a registration whose watch could not be armed. It must never
    /// have participated in an allocation, transfer or consumer reference.
    #[cfg(windows)]
    pub(crate) fn cancel_unused_registration(&mut self, process: Process) -> io::Result<()> {
        if self.allocations.values().any(|a| {
            a.producer == Some(process)
                || a.pending.values().any(|p| *p == process)
                || a.consumers.values().any(|p| *p == process)
        }) {
            return Err(denied("BAR process registration already owns references"));
        }
        self.processes.remove(&process);
        Ok(())
    }
    fn user(&self, process: Process) -> io::Result<&User> {
        self.processes
            .get(&process)
            .ok_or_else(|| denied("unknown or dead BAR process"))
    }
    pub fn allocate(&mut self, process: Process, length: u64) -> io::Result<(Key, Extent)> {
        self.allocate_reserved(process, length, length)
    }
    /// Reserve address-stable capacity once. Logical pool growth stays inside
    /// this original extent and cannot move any live consumer's pointer.
    pub fn allocate_reserved(
        &mut self,
        process: Process,
        length: u64,
        reserved: u64,
    ) -> io::Result<(Key, Extent)> {
        let user = self.user(process)?.clone();
        if user != self.grant.user {
            return Err(denied("BAR grant belongs to another user"));
        }
        if length == 0 || length > reserved {
            return Err(io::Error::other("invalid BAR logical length"));
        }
        let capacity = reserved
            .checked_add(4095)
            .map(|n| n & !4095)
            .filter(|n| *n > 0)
            .ok_or_else(|| io::Error::other("invalid BAR length"))?;
        let mut used: Vec<_> = self.allocations.values().map(|a| a.extent).collect();
        used.sort_by_key(|e| e.offset);
        let mut offset = self.start;
        for e in used {
            if offset
                .checked_add(capacity)
                .is_some_and(|end| end <= e.offset)
            {
                break;
            }
            offset = e.offset + e.capacity;
        }
        if !offset
            .checked_add(capacity)
            .is_some_and(|end| end <= self.end)
        {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "trusted BAR partition is full",
            ));
        }
        let key = Key {
            run: self.run,
            allocation: self.serial()?,
        };
        let extent = Extent {
            offset,
            capacity,
            length,
        };
        self.allocations.insert(
            key,
            Allocation {
                user,
                producer: Some(process),
                extent,
                pending: BTreeMap::new(),
                consumers: BTreeMap::new(),
                external: BTreeMap::new(),
            },
        );
        Ok((key, extent))
    }
    /// Grow logical length without moving or reallocating original capacity.
    pub fn resize(&mut self, producer: Process, key: Key, length: u64) -> io::Result<Extent> {
        let a = self.owned(producer, key)?;
        if a.producer != Some(producer) || length < a.extent.length || length > a.extent.capacity {
            return Err(denied("BAR resize exceeds original producer capacity"));
        }
        let a = self.allocations.get_mut(&key).unwrap();
        a.extent.length = length;
        Ok(a.extent)
    }
    fn owned(&self, process: Process, key: Key) -> io::Result<&Allocation> {
        let user = self.user(process)?;
        let a = self
            .allocations
            .get(&key)
            .ok_or_else(|| denied("stale trusted BAR allocation"))?;
        if &a.user != user {
            return Err(denied("BAR allocation belongs to another user"));
        }
        Ok(a)
    }
    /// Reserve before a descriptor becomes observable. The transfer is bound
    /// to one authenticated recipient process, not whoever guesses its id.
    pub fn prepare(
        &mut self,
        producer: Process,
        key: Key,
        recipient: Process,
    ) -> io::Result<Transfer> {
        let a = self.owned(producer, key)?;
        if a.producer != Some(producer) || self.user(recipient)? != &a.user {
            return Err(denied("BAR transfer ownership mismatch"));
        }
        let transfer = Transfer(self.serial()?);
        self.allocations
            .get_mut(&key)
            .unwrap()
            .pending
            .insert(transfer, recipient);
        Ok(transfer)
    }
    pub fn accept(
        &mut self,
        recipient: Process,
        key: Key,
        transfer: Transfer,
    ) -> io::Result<(Consumer, Extent)> {
        let a = self.owned(recipient, key)?;
        if a.pending.get(&transfer) != Some(&recipient) {
            return Err(denied("BAR transfer recipient mismatch"));
        }
        let consumer = Consumer(self.serial()?);
        let a = self.allocations.get_mut(&key).unwrap();
        a.pending.remove(&transfer);
        a.consumers.insert(consumer, recipient);
        Ok((consumer, a.extent))
    }
    /// Pin before any remote pending descriptor can be published.
    pub(crate) fn pin_external(&mut self, producer: Process, key: Key) -> io::Result<ExternalHold> {
        if self.owned(producer, key)?.producer != Some(producer) {
            return Err(denied("not the external lineage producer"));
        }
        let id = self.serial()?;
        let identity = Arc::new(());
        self.allocations
            .get_mut(&key)
            .unwrap()
            .external
            .insert(id, identity.clone());
        Ok(ExternalHold { key, id, identity })
    }
    pub(crate) fn check_external(&self, hold: &ExternalHold) -> io::Result<()> {
        if !self
            .allocations
            .get(&hold.key)
            .and_then(|a| a.external.get(&hold.id))
            .is_some_and(|identity| Arc::ptr_eq(identity, &hold.identity))
        {
            return Err(denied("stale or foreign external lineage pin"));
        }
        Ok(())
    }
    /// # Safety
    /// Exact privileged peer transition must prove this remote lineage cannot
    /// export again: trusted consumer close or the terminal accept/cancel winner.
    /// Parsing, a native enum and a client request cannot supply that proof.
    pub(crate) unsafe fn release_external(&mut self, hold: ExternalHold) -> io::Result<()> {
        self.check_external(&hold)?;
        self.allocations.get_mut(&hold.key).unwrap().external.remove(&hold.id);
        self.reclaim(hold.key);
        Ok(())
    }
    /// Explicit close is permitted only after all local mappings are gone.
    /// The transport must enforce this lifetime; a DLL destructor is not proof.
    ///
    /// # Safety
    /// The service must have successfully revoked every corresponding kernel
    /// payload mapping or hold an equivalent trusted completion proof. A
    /// desktop close request or shared word cannot satisfy this contract.
    pub unsafe fn close_producer(&mut self, producer: Process, key: Key) -> io::Result<()> {
        if self.owned(producer, key)?.producer != Some(producer) {
            return Err(denied("not the BAR producer"));
        }
        self.allocations.get_mut(&key).unwrap().producer = None;
        self.reclaim(key);
        Ok(())
    }
    /// Retire an exact registered consumer after independent completion.
    ///
    /// # Safety
    /// The service must have successfully revoked the exact kernel mapping,
    /// or received completion from the authenticated privileged remote relay
    /// tied to that consumer's original session and process object. Client
    /// requests and shared counters do not establish consumer completion.
    pub unsafe fn release_consumer(
        &mut self,
        process: Process,
        key: Key,
        consumer: Consumer,
    ) -> io::Result<()> {
        if self.owned(process, key)?.consumers.get(&consumer) != Some(&process) {
            return Err(denied("not this BAR consumer"));
        }
        self.allocations
            .get_mut(&key)
            .unwrap()
            .consumers
            .remove(&consumer);
        self.reclaim(key);
        Ok(())
    }
    /// Privileged death path. Call only for the process registration whose
    /// retained kernel object is confirmed dead. Remote consumers and pending
    /// transfers survive producer death, including transfers not yet accepted.
    ///
    /// # Safety
    /// The caller must retain the exact authenticated process kernel object
    /// from registration until kernel-confirmed termination. Disconnect, PID
    /// reuse, elapsed time and client requests cannot satisfy this contract.
    pub unsafe fn process_dead(&mut self, process: Process) {
        self.processes.remove(&process);
        let mut empty = Vec::new();
        for (key, a) in &mut self.allocations {
            if a.producer == Some(process) {
                a.producer = None;
            }
            // Only this exact consumer process's mappings ceased to exist.
            a.consumers.retain(|_, holder| *holder != process);
            // Reservations already bound to this dead recipient cannot become
            // mappings, but producer death alone never cancels a transfer.
            a.pending.retain(|_, recipient| *recipient != process);
            if a.producer.is_none()
                && a.pending.is_empty()
                && a.consumers.is_empty()
                && a.external.is_empty()
            {
                empty.push(*key);
            }
        }
        for key in empty {
            self.allocations.remove(&key);
        }
    }
    fn reclaim(&mut self, key: Key) {
        if self.allocations.get(&key).is_some_and(|a| {
            a.producer.is_none()
                && a.pending.is_empty()
                && a.consumers.is_empty()
                && a.external.is_empty()
        }) {
            self.allocations.remove(&key);
        }
    }
    pub(crate) fn has_serial_budget(&self, required: u64) -> bool { required != 0 && u64::MAX - self.next >= required }
    // Admission bookkeeping only. Neither value is retirement authority.
    pub(crate) fn admitted_allocations(&self) -> usize { self.allocations.len() }
    pub(crate) fn admitted_references(&self) -> usize { self.allocations.values().map(|a| a.pending.len() + a.consumers.len()).sum() }
    pub fn extent(&self, key: Key) -> Option<Extent> {
        self.allocations.get(&key).map(|a| a.extent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (Allocator, Process, Process) {
        let mut a = Allocator::new(71, 4096, 12288).unwrap();
        let p = a.register_process(User::sid("S-1-5-21-1")).unwrap();
        let c = a.register_process(User::sid("S-1-5-21-1")).unwrap();
        (a, p, c)
    }
    #[test]
    fn external_pin_survives_producer_close_and_death_without_native_substitution() {
        let (mut a, p, _) = setup();
        let (key, extent) = a.allocate(p, 4096).unwrap();
        let pin = a.pin_external(p, key).unwrap();
        unsafe {
            a.close_producer(p, key).unwrap();
            a.process_dead(p);
        }
        assert_eq!(a.extent(key), Some(extent));
        a.check_external(&pin).unwrap();
        let other = a.register_process(User::sid("S-1-5-21-1")).unwrap();
        let (_, next) = a.allocate(other, 4096).unwrap();
        assert_ne!(next.offset, extent.offset);
        // A coincident numeric key/run from another table is not this pin.
        let (mut foreign, p, _) = setup();
        let (same_key, _) = foreign.allocate(p, 4096).unwrap();
        let _foreign_pin = foreign.pin_external(p, same_key).unwrap();
        assert_eq!(same_key, key);
        assert!(foreign.check_external(&pin).is_err());
    }
    #[test]
    fn canonical_header_offset_is_applied_once_and_overflow_refuses() {
        let user = User::sid("S-1-5-21-1");
        let identity = GrantIdentity {
            bar_run: 19,
            bar_offset_base: 4096,
            session: 1,
            region: 256,
            user: user.clone(),
        };
        let grant = wc_handle::barpool::Grant {
            id: 256,
            session: 1,
            region: 0,
            offset: 8192,
            len: 8192,
        };
        // SAFETY: the fixture owns the canonical arena grant and its header.
        let mut a =
            unsafe { Allocator::from_delegated_grant(71, identity.clone(), grant, Arc::new(())) }
                .unwrap();
        let p = a.register_process(user).unwrap();
        assert_eq!(a.allocate(p, 4096).unwrap().1.offset, 12288);
        let bad = GrantIdentity {
            bar_offset_base: u64::MAX,
            ..identity
        };
        // SAFETY: malformed simulated source geometry must be refused.
        assert!(unsafe { Allocator::from_delegated_grant(72, bad, grant, Arc::new(())) }.is_err());
    }
    #[test]
    fn abandoning_live_state_does_not_drop_the_canonical_grant_guard() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        // SAFETY: the fixture owns this simulated canonical grant.
        let mut a = unsafe {
            Allocator::from_delegated_grant(
                71,
                GrantIdentity {
                    bar_run: 19,
                    bar_offset_base: 0,
                    session: 1,
                    region: 256,
                    user: User::sid("S-1-5-21-1"),
                },
                wc_handle::barpool::Grant {
                    id: 256,
                    session: 1,
                    region: 0,
                    offset: 4096,
                    len: 8192,
                },
                Arc::new(Guard(Arc::clone(&released))),
            )
        }
        .unwrap();
        let p = a.register_process(User::sid("S-1-5-21-1")).unwrap();
        a.allocate(p, 1).unwrap();
        drop(a);
        assert!(!released.load(Ordering::SeqCst));
    }
    #[test]
    fn reserved_capacity_grows_in_place_with_live_consumer() {
        let (mut a, p, c) = setup();
        let (key, original) = a.allocate_reserved(p, 1, 8192).unwrap();
        let transfer = a.prepare(p, key, c).unwrap();
        let (consumer, _) = a.accept(c, key, transfer).unwrap();
        let grown = a.resize(p, key, 8192).unwrap();
        assert_eq!(grown.offset, original.offset);
        assert_eq!(grown.capacity, original.capacity);
        assert_eq!(grown.length, 8192);
        assert!(a.resize(p, key, 8193).is_err());
        assert!(a.resize(c, key, 8192).is_err());
        // SAFETY: the fixture models confirmed producer process death.
        unsafe { a.process_dead(p) };
        assert_eq!(a.extent(key), Some(grown));
        assert!(a.allocate(c, 1).is_err());
        // SAFETY: the fixture models the exact consumer's completed mapping.
        unsafe { a.release_consumer(c, key, consumer) }.unwrap();
        assert!(a.allocate(c, 8192).is_ok());
    }
    #[test]
    fn registrations_and_keys_cannot_cross_grant_runs_or_users() {
        let (mut original, p, _) = setup();
        let (old, _) = original.allocate(p, 1).unwrap();
        let mut other = Allocator::new(72, 4096, 12288).unwrap();
        let fresh = other.register_process(User::sid("S-1-5-21-1")).unwrap();
        assert!(other.allocate(p, 1).is_err());
        assert!(unsafe { other.close_producer(fresh, old) }.is_err());
        let foreign = other.register_process(User::sid("S-1-5-21-2")).unwrap();
        assert!(other.allocate(foreign, 1).is_err());
        let (key, extent) = other.allocate(fresh, 8192).unwrap();
        assert_eq!(extent.offset, 4096);
        assert_eq!(extent.capacity, 8192);
        assert_eq!(key.run, 72);
        assert!(other.allocate(fresh, 1).is_err());
    }
    #[test]
    fn dead_producer_preserves_pending_and_live_consumer() {
        let (mut a, p, c) = setup();
        let (key, extent) = a.allocate(p, 8192).unwrap();
        let pending = a.prepare(p, key, c).unwrap();
        // SAFETY: simulated kernel death in a state-machine fixture.
        unsafe { a.process_dead(p) };
        assert_eq!(a.extent(key), Some(extent));
        assert!(a.allocate(c, 1).is_err());
        let (consumer, _) = a.accept(c, key, pending).unwrap();
        assert!(a.allocate(c, 1).is_err());
        unsafe { a.release_consumer(c, key, consumer) }.unwrap();
        let (fresh, reused) = a.allocate(c, 8192).unwrap();
        assert_ne!(fresh, key);
        assert_eq!(reused.offset, extent.offset);
        assert!(unsafe { a.release_consumer(c, key, consumer) }.is_err());
    }
    #[test]
    fn completion_requires_exact_registered_recipient_even_for_same_user() {
        let (mut a, p, c) = setup();
        let other = a.register_process(User::sid("S-1-5-21-1")).unwrap();
        let foreign = a.register_process(User::sid("S-1-5-21-2")).unwrap();
        let (key, _) = a.allocate(p, 1).unwrap();
        assert!(a.prepare(p, key, foreign).is_err());
        assert!(a.prepare(other, key, c).is_err());
        let transfer = a.prepare(p, key, c).unwrap();
        assert!(a.accept(other, key, transfer).is_err());
        assert!(a.accept(foreign, key, transfer).is_err());
        let (consumer, _) = a.accept(c, key, transfer).unwrap();
        assert!(a.accept(c, key, transfer).is_err());
        assert!(unsafe { a.release_consumer(other, key, consumer) }.is_err());
        assert!(unsafe { a.close_producer(other, key) }.is_err());
        assert!(a.extent(key).is_some());
    }
    #[test]
    fn only_dead_recipient_cancels_its_pending_mapping() {
        let (mut a, p, c) = setup();
        let (key, _) = a.allocate(p, 8192).unwrap();
        let transfer = a.prepare(p, key, c).unwrap();
        unsafe { a.close_producer(p, key) }.unwrap();
        assert!(a.extent(key).is_some());
        // SAFETY: simulated kernel death in a state-machine fixture.
        unsafe { a.process_dead(c) };
        assert!(a.extent(key).is_none());
        assert!(a.accept(c, key, transfer).is_err());
    }
}
