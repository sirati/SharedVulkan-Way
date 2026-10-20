//! Allocation lifetimes inside one already mapped, fixed-size BAR.
//!
//! A transfer reserves a consumer reference before its descriptor enters the
//! wire. Receiving the descriptor converts that reservation into a consumer
//! lease. Producer close, process exit, and elapsed time cannot release a
//! consumer lease. A slot is reusable only after all three counts reach zero.
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Lease control dialect, distinct from legacy descriptor-free registry wire 1.
pub const CONTROL_WIRE_VERSION: u32 = 2;
pub const SLOTS: usize = 1024;
pub const DIRECTORY_BYTES: usize = 128 * 1024;
const MAGIC: u64 = 0x31455341454c4357;
const HEADER_WORDS: usize = 8;
const SLOT_WORDS: usize = 10;
const GENERATION: usize = 0;
const OFFSET: usize = 1;
const CAPACITY: usize = 2;
const LENGTH: usize = 3;
const OWNER: usize = 4;
const PENDING: usize = 5;
const CONSUMERS: usize = 6;
const ROLE: usize = 7;
const PRODUCER_ID: usize = 8;
const EXPOSED: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Guest = 1,
    Host = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub slot: u32,
    pub generation: u64,
}

/// The retained original mapping, not an independently opened device.
#[derive(Clone)]
pub struct Directory {
    inner: Arc<Inner>,
}
struct Inner {
    metadata: usize,
    data: usize,
    writable: usize,
    local_role: Role,
    len: usize,
    run: u64,
    host_start: usize,
    _mapping: Arc<dyn Send + Sync>,
}
// The pointers address retained shared mappings. Every metadata access uses
// atomics. Only an Owner exposes mutable bytes, within its exclusive extent.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
impl Directory {
    /// Initialize before publishing the containing BAR's session header.
    ///
    /// # Safety
    /// `metadata` is aligned, writable DIRECTORY_BYTES in the retained mapping.
    /// `data` addresses `len` readable coherent BAR bytes. `writable` addresses
    /// only the complete local_role partition, starting at DIRECTORY_BYTES for
    /// Guest or host_start for Host. The peer pixel window can remain read-only.
    /// No peer may access the directory until initialization finishes.
    pub unsafe fn initialize(
        metadata: *mut u8,
        data: *const u8,
        writable: *mut u8,
        local_role: Role,
        len: usize,
        host_start: usize,
        run: u64,
        mapping: Arc<dyn Send + Sync>,
    ) -> io::Result<Self> {
        if metadata as usize % 8 != 0
            || run == 0
            || host_start < DIRECTORY_BYTES
            || host_start >= len
        {
            return Err(bad("invalid BAR allocation directory geometry"));
        }
        // SAFETY: caller supplies the exclusive writable metadata range.
        unsafe {
            std::ptr::write_bytes(metadata, 0, DIRECTORY_BYTES);
        }
        let result = Self {
            inner: Arc::new(Inner {
                metadata: metadata as usize,
                data: data as usize,
                writable: writable as usize,
                local_role,
                len,
                run,
                host_start,
                _mapping: mapping,
            }),
        };
        result.word(2).store(run, Ordering::Relaxed);
        result.word(3).store(len as u64, Ordering::Relaxed);
        result.word(4).store(host_start as u64, Ordering::Relaxed);
        result.word(0).store(MAGIC, Ordering::Release);
        Ok(result)
    }

    /// Attach the original mapping after validating the containing BAR header.
    ///
    /// # Safety
    /// The same pointer/lifetime requirements as initialize apply. The metadata
    /// must have been initialized by that function, not ordinary file bytes.
    pub unsafe fn attach(
        metadata: *mut u8,
        data: *const u8,
        writable: *mut u8,
        local_role: Role,
        len: usize,
        host_start: usize,
        run: u64,
        mapping: Arc<dyn Send + Sync>,
    ) -> io::Result<Self> {
        if metadata as usize % 8 != 0
            || run == 0
            || host_start < DIRECTORY_BYTES
            || host_start >= len
        {
            return Err(bad("invalid BAR allocation directory geometry"));
        }
        let result = Self {
            inner: Arc::new(Inner {
                metadata: metadata as usize,
                data: data as usize,
                writable: writable as usize,
                local_role,
                len,
                run,
                host_start,
                _mapping: mapping,
            }),
        };
        result.validate()?;
        Ok(result)
    }
    fn word(&self, index: usize) -> &AtomicU64 {
        // SAFETY: indices are constants or checked slot indices and fit inside
        // DIRECTORY_BYTES. Attach requires alignment and a retained mapping.
        unsafe { &*((self.inner.metadata as *const AtomicU64).add(index)) }
    }
    fn slot(&self, slot: usize, field: usize) -> &AtomicU64 {
        self.word(HEADER_WORDS + slot * SLOT_WORDS + field)
    }
    fn validate(&self) -> io::Result<()> {
        if self.word(0).load(Ordering::Acquire) != MAGIC
            || self.word(2).load(Ordering::Relaxed) != self.inner.run
            || self.word(3).load(Ordering::Relaxed) != self.inner.len as u64
            || self.word(4).load(Ordering::Relaxed) != self.inner.host_start as u64
        {
            return Err(bad("BAR allocation directory session identity differs"));
        }
        Ok(())
    }
    fn lock(&self) -> io::Result<Guard<'_>> {
        self.validate()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        while self
            .word(1)
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            if std::time::Instant::now() >= deadline {
                // A dead peer may own this lock. Never steal it or infer
                // consumer completion: retain every count and fail closed.
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "BAR allocation metadata is busy or its writer stopped",
                ));
            }
            std::thread::yield_now();
        }
        Ok(Guard(self))
    }
    fn checked(&self, key: Key) -> io::Result<usize> {
        let slot = key.slot as usize;
        if slot >= SLOTS
            || key.generation == 0
            || self.slot(slot, GENERATION).load(Ordering::Relaxed) != key.generation
            || self.slot(slot, CAPACITY).load(Ordering::Relaxed) == 0
        {
            return Err(bad("stale BAR allocation generation"));
        }
        let offset = self.slot(slot, OFFSET).load(Ordering::Relaxed) as usize;
        let capacity = self.slot(slot, CAPACITY).load(Ordering::Relaxed) as usize;
        let length = self.slot(slot, LENGTH).load(Ordering::Relaxed) as usize;
        let role = self.slot(slot, ROLE).load(Ordering::Relaxed);
        let (start, limit) = match role {
            1 => (DIRECTORY_BYTES, self.inner.host_start),
            2 => (self.inner.host_start, self.inner.len),
            _ => return Err(bad("invalid BAR allocation owner partition")),
        };
        if length == 0
            || length > capacity
            || offset < start
            || !offset.checked_add(capacity).is_some_and(|end| end <= limit)
        {
            return Err(bad("BAR allocation extent exceeds its owner partition"));
        }
        Ok(slot)
    }
    fn reclaim(&self, slot: usize) {
        if [OWNER, PENDING, CONSUMERS]
            .iter()
            .all(|field| self.slot(slot, *field).load(Ordering::Relaxed) == 0)
        {
            self.slot(slot, CAPACITY).store(0, Ordering::Relaxed);
        }
    }
    /// Reserve address-stable capacity. Only logical length is initialized.
    /// Capacity never moves or grows beyond the original fixed BAR.
    pub fn allocate(&self, role: Role, length: usize, capacity: usize) -> io::Result<Owner> {
        if role != self.inner.local_role {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "BAR allocation belongs to the other peer partition",
            ));
        }
        if length == 0 || capacity < length {
            return Err(bad("invalid BAR allocation length"));
        }
        let capacity = capacity
            .checked_add(65535)
            .ok_or_else(|| bad("capacity overflow"))?
            & !65535;
        let _guard = self.lock()?;
        let free = (0..SLOTS)
            .find(|slot| self.slot(*slot, CAPACITY).load(Ordering::Relaxed) == 0)
            .ok_or_else(|| io::Error::other("BAR allocation directory is full"))?;
        let (start, limit) = match role {
            Role::Guest => (DIRECTORY_BYTES, self.inner.host_start),
            Role::Host => (self.inner.host_start, self.inner.len),
        };
        let mut extents: Vec<_> = (0..SLOTS)
            .filter_map(|slot| {
                let len = self.slot(slot, CAPACITY).load(Ordering::Relaxed) as usize;
                let offset = self.slot(slot, OFFSET).load(Ordering::Relaxed) as usize;
                (len != 0 && offset >= start && offset < limit).then_some((offset, len))
            })
            .collect();
        extents.sort_unstable();
        let mut offset = start;
        for (at, span) in extents {
            if offset.checked_add(capacity).is_some_and(|end| end <= at) {
                break;
            }
            offset = offset.max(at.checked_add(span).ok_or_else(|| bad("extent overflow"))?);
        }
        if !offset.checked_add(capacity).is_some_and(|end| end <= limit) {
            return Err(io::Error::other(
                "fixed BAR address-stable capacity exhausted",
            ));
        }
        let generation = self
            .slot(free, GENERATION)
            .load(Ordering::Relaxed)
            .checked_add(1)
            .ok_or_else(|| bad("BAR allocation generation exhausted"))?;
        for (field, value) in [
            (GENERATION, generation),
            (OFFSET, offset as u64),
            (LENGTH, length as u64),
            (OWNER, 1),
            (PENDING, 0),
            (CONSUMERS, 0),
            (ROLE, role as u64),
            (PRODUCER_ID, 0),
            (EXPOSED, 0),
        ] {
            self.slot(free, field).store(value, Ordering::Relaxed);
        }
        // Initialize only newly exposed bytes, directly in the BAR.
        // SAFETY: lock proved the complete reserved extent is exclusively free.
        unsafe {
            std::ptr::write_bytes(self.owner_pointer(offset), 0, length);
        }
        self.slot(free, CAPACITY)
            .store(capacity as u64, Ordering::Relaxed);
        Ok(Owner {
            directory: self.clone(),
            key: Key {
                slot: free as u32,
                generation,
            },
            mapped: std::sync::atomic::AtomicBool::new(false),
        })
    }
    /// Acquire a reference already reserved by the sender, exactly once.
    pub fn accept(&self, key: Key) -> io::Result<Consumer> {
        let _guard = self.lock()?;
        let slot = self.checked(key)?;
        let pending = self.slot(slot, PENDING).load(Ordering::Relaxed);
        if pending == 0 {
            return Err(bad("BAR descriptor has no pending transfer"));
        }
        let readers = self
            .slot(slot, CONSUMERS)
            .load(Ordering::Relaxed)
            .checked_add(1)
            .ok_or_else(|| bad("BAR consumer count overflow"))?;
        self.slot(slot, PENDING)
            .store(pending - 1, Ordering::Relaxed);
        self.slot(slot, CONSUMERS).store(readers, Ordering::Relaxed);
        Ok(Consumer {
            directory: self.clone(),
            key,
        })
    }
    fn owner_pointer(&self, offset: usize) -> *mut u8 {
        let start = match self.inner.local_role {
            Role::Guest => DIRECTORY_BYTES,
            Role::Host => self.inner.host_start,
        };
        (self.inner.writable as *mut u8).wrapping_add(offset - start)
    }
    /// Cooperative counter simulation for isolated tests only.
    /// Client-writable counters cannot authorize privileged retirement.
    ///
    /// # Safety
    /// The test must exclusively control every simulated participant. This
    /// helper must never be exposed by a production broker or recovery path.
    #[cfg(test)]
    unsafe fn simulate_cooperative_producer_close(
        &self,
        key: Key,
        producer_id: u64,
        offset: usize,
        capacity: usize,
    ) -> io::Result<bool> {
        let _guard = self.lock()?;
        let slot = self.checked(key)?;
        if producer_id == 0
            || self.slot(slot, PRODUCER_ID).load(Ordering::Relaxed) != producer_id
            || self.slot(slot, OFFSET).load(Ordering::Relaxed) != offset as u64
            || self.slot(slot, CAPACITY).load(Ordering::Relaxed) != capacity as u64
        {
            return Err(bad(
                "BAR producer retirement does not match trusted registration",
            ));
        }
        if self.slot(slot, OWNER).load(Ordering::Relaxed) == 0 {
            return Ok(false);
        }
        self.slot(slot, OWNER).store(0, Ordering::Relaxed);
        self.reclaim(slot);
        Ok(true)
    }
    pub fn extent(&self, key: Key) -> io::Result<(usize, usize, usize)> {
        let _guard = self.lock()?;
        let slot = self.checked(key)?;
        Ok((
            self.slot(slot, OFFSET).load(Ordering::Relaxed) as usize,
            self.slot(slot, LENGTH).load(Ordering::Relaxed) as usize,
            self.slot(slot, CAPACITY).load(Ordering::Relaxed) as usize,
        ))
    }
}
struct Guard<'a>(&'a Directory);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.word(1).store(0, Ordering::Release);
    }
}

pub struct Owner {
    directory: Directory,
    key: Key,
    mapped: std::sync::atomic::AtomicBool,
}
impl Owner {
    pub(crate) fn claim_mapping(&self) -> io::Result<()> {
        self.mapped
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "BAR owner already has a writable mapping",
                )
            })
    }
    pub(crate) fn write_at(&self, offset: usize, bytes: &[u8]) -> io::Result<()> {
        self.claim_mapping()?;
        struct Permit<'a>(&'a Owner);
        impl Drop for Permit<'_> {
            fn drop(&mut self) {
                self.0.release_mapping();
            }
        }
        let _permit = Permit(self);
        let (_, len, _) = self.extent()?;
        if !offset
            .checked_add(bytes.len())
            .is_some_and(|end| end <= len)
        {
            return Err(bad("BAR write exceeds logical length"));
        }
        // SAFETY: exclusive producer write capability and checked live range.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.pointer()?.add(offset), bytes.len());
        }
        Ok(())
    }
    pub(crate) fn release_mapping(&self) {
        self.mapped.store(false, Ordering::Release);
    }
    pub fn key(&self) -> Key {
        self.key
    }
    pub fn extent(&self) -> io::Result<(usize, usize, usize)> {
        self.directory.extent(self.key)
    }
    pub fn pointer(&self) -> io::Result<*mut u8> {
        let _guard = self.directory.lock()?;
        let slot = self.directory.checked(self.key)?;
        self.directory
            .slot(slot, EXPOSED)
            .store(1, Ordering::Relaxed);
        let offset = self.directory.slot(slot, OFFSET).load(Ordering::Relaxed) as usize;
        Ok(self.directory.owner_pointer(offset))
    }
    /// Stamp the broker registration before any pointer or wire exposure.
    /// Zero remains the safe default: an unregistered owner cannot be retired
    /// through the kernel-death API.
    pub fn register_producer(&self, producer_id: u64) -> io::Result<(Key, usize, usize)> {
        if producer_id == 0 {
            return Err(bad("zero BAR producer registration"));
        }
        let _guard = self.directory.lock()?;
        let slot = self.directory.checked(self.key)?;
        if self.directory.slot(slot, EXPOSED).load(Ordering::Relaxed) != 0
            || self
                .directory
                .slot(slot, PRODUCER_ID)
                .load(Ordering::Relaxed)
                != 0
        {
            return Err(bad("BAR producer registration must precede exposure"));
        }
        self.directory
            .slot(slot, PRODUCER_ID)
            .store(producer_id, Ordering::Relaxed);
        Ok((
            self.key,
            self.directory.slot(slot, OFFSET).load(Ordering::Relaxed) as usize,
            self.directory.slot(slot, CAPACITY).load(Ordering::Relaxed) as usize,
        ))
    }
    pub fn grow(&self, length: usize) -> io::Result<()> {
        let _guard = self.directory.lock()?;
        let slot = self.directory.checked(self.key)?;
        let old = self.directory.slot(slot, LENGTH).load(Ordering::Relaxed) as usize;
        let capacity = self.directory.slot(slot, CAPACITY).load(Ordering::Relaxed) as usize;
        if length < old || length > capacity {
            return Err(bad("BAR pool resize exceeds reserved capacity"));
        }
        let offset = self.directory.slot(slot, OFFSET).load(Ordering::Relaxed) as usize;
        // SAFETY: owner retains exclusive production rights in this capacity;
        // existing consumers only mapped the old logical range.
        unsafe {
            std::ptr::write_bytes(
                self.directory.owner_pointer(offset).add(old),
                0,
                length - old,
            );
        }
        self.directory
            .slot(slot, LENGTH)
            .store(length as u64, Ordering::Relaxed);
        Ok(())
    }
    pub fn publish(&self) -> io::Result<Transfer> {
        let _guard = self.directory.lock()?;
        let slot = self.directory.checked(self.key)?;
        let count = self
            .directory
            .slot(slot, PENDING)
            .load(Ordering::Relaxed)
            .checked_add(1)
            .ok_or_else(|| bad("BAR transfer count overflow"))?;
        self.directory
            .slot(slot, PENDING)
            .store(count, Ordering::Relaxed);
        self.directory
            .slot(slot, EXPOSED)
            .store(1, Ordering::Relaxed);
        Ok(Transfer {
            directory: self.directory.clone(),
            key: self.key,
            committed: false,
        })
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if let Ok(_guard) = self.directory.lock() {
            if let Ok(slot) = self.directory.checked(self.key) {
                self.directory.slot(slot, OWNER).store(0, Ordering::Relaxed);
                self.directory.reclaim(slot);
            }
        }
    }
}
/// Roll back an unsent descriptor. After commit only its consumer can release
/// it; disconnect and producer destruction cannot guess that it was drained.
pub struct Transfer {
    directory: Directory,
    key: Key,
    committed: bool,
}
impl Transfer {
    pub fn key(&self) -> Key {
        self.key
    }
    pub fn commit(mut self) -> Key {
        self.committed = true;
        self.key
    }
}
impl Drop for Transfer {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(_guard) = self.directory.lock() {
            if let Ok(slot) = self.directory.checked(self.key) {
                let count = self.directory.slot(slot, PENDING).load(Ordering::Relaxed);
                self.directory
                    .slot(slot, PENDING)
                    .store(count.saturating_sub(1), Ordering::Relaxed);
                self.directory.reclaim(slot);
            }
        }
    }
}
pub struct Consumer {
    directory: Directory,
    key: Key,
}
impl Consumer {
    pub fn key(&self) -> Key {
        self.key
    }
    pub fn extent(&self) -> io::Result<(usize, usize, usize)> {
        self.directory.extent(self.key)
    }
    pub fn pointer(&self) -> io::Result<*const u8> {
        let (offset, _, _) = self.extent()?;
        Ok((self.directory.inner.data as *const u8).wrapping_add(offset))
    }
}
impl Drop for Consumer {
    fn drop(&mut self) {
        if let Ok(_guard) = self.directory.lock() {
            if let Ok(slot) = self.directory.checked(self.key) {
                let count = self.directory.slot(slot, CONSUMERS).load(Ordering::Relaxed);
                self.directory
                    .slot(slot, CONSUMERS)
                    .store(count.saturating_sub(1), Ordering::Relaxed);
                self.directory.reclaim(slot);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Memory {
        pointer: usize,
        layout: std::alloc::Layout,
    }
    unsafe impl Send for Memory {}
    unsafe impl Sync for Memory {}
    impl Drop for Memory {
        fn drop(&mut self) {
            // SAFETY: this uniquely owned allocation uses the original layout.
            unsafe {
                std::alloc::dealloc(self.pointer as *mut u8, self.layout);
            }
        }
    }
    fn fixture() -> Directory {
        let len = 4 * 1024 * 1024;
        let layout = std::alloc::Layout::from_size_align(len, 65536).unwrap();
        // SAFETY: valid layout; test retains the allocation through Memory.
        let pointer = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!pointer.is_null());
        let memory = Arc::new(Memory {
            pointer: pointer as usize,
            layout,
        });
        // SAFETY: exclusive aligned allocation and no peer before publication.
        unsafe {
            Directory::initialize(
                pointer,
                pointer,
                pointer.add(DIRECTORY_BYTES),
                Role::Guest,
                len,
                len / 2,
                42,
                memory,
            )
        }
        .unwrap()
    }
    #[test]
    fn allocation_control_rejects_legacy_wire_version() {
        use sv_transport::control::{Control, Geometry, ProtocolIds};
        use sv_transport::region::SharedRegion;
        let geometry = Geometry::small();
        let region = Arc::new(SharedRegion::heap(geometry.control_size as usize).unwrap());
        let _owner = Control::create(
            region.clone(),
            geometry,
            ProtocolIds::new(CONTROL_WIRE_VERSION, 42),
            1,
        )
        .unwrap();
        assert!(Control::attach(region.clone(), ProtocolIds::new(1, 42)).is_err());
        assert!(Control::attach(region, ProtocolIds::new(CONTROL_WIRE_VERSION, 42)).is_ok());
    }
    #[test]
    fn producer_drop_never_reclaims_pending_or_consumed_extent() {
        let directory = fixture();
        let owner = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        let key = owner.publish().unwrap().commit();
        let original = owner.extent().unwrap().0;
        drop(owner);
        let other = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        assert_ne!(other.extent().unwrap().0, original);
        let consumer = directory.accept(key).unwrap();
        assert!(
            directory.accept(key).is_err(),
            "a transfer is consumed once"
        );
        drop(other);
        let next = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        assert_ne!(next.extent().unwrap().0, original);
        drop(consumer);
        let recycled = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        assert_eq!(recycled.extent().unwrap().0, original);
        assert_ne!(recycled.key(), key);
        assert!(
            directory.accept(key).is_err(),
            "old generation cannot acquire new bytes"
        );
    }
    #[test]
    fn growth_preserves_address_and_pixels_and_never_exposes_capacity() {
        let directory = fixture();
        let owner = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        let pointer = owner.pointer().unwrap();
        // SAFETY: test owns this live logical extent.
        unsafe {
            pointer.write(0xa5);
        }
        let key = owner.publish().unwrap().commit();
        let consumer = directory.accept(key).unwrap();
        owner.grow(32768).unwrap();
        assert_eq!(owner.pointer().unwrap(), pointer);
        assert_eq!(consumer.extent().unwrap().1, 32768);
        // SAFETY: both offsets are in the live initialized extent.
        unsafe {
            assert_eq!(pointer.read(), 0xa5);
            assert_eq!(pointer.add(8192).read(), 0);
        }
        assert!(owner.grow(65537).is_err());
        assert!(owner.grow(1024).is_err());
    }
    #[test]
    fn cancelled_transfer_and_many_lifetimes_reuse_without_timeout() {
        let directory = fixture();
        for _ in 0..(SLOTS * 2) {
            let owner = directory.allocate(Role::Guest, 1, 65536).unwrap();
            let transfer = owner.publish().unwrap();
            let key = transfer.key();
            drop(transfer);
            assert!(directory.accept(key).is_err());
            drop(owner);
        }
    }
    #[test]
    fn handle_and_mapping_hold_actual_remote_completion() {
        let directory = fixture();
        let arena = 0xb101;
        register(arena, directory.clone()).unwrap();
        let producer = crate::Handle::create_bar(arena, Role::Guest, 4096, 65536).unwrap();
        producer.write_at(7, &[0xa5]).unwrap();
        let prepared = producer.prepare_transfer().unwrap();
        let descriptor = crate::Descriptor::decode(prepared.bytes()).unwrap();
        let key = match descriptor {
            crate::Descriptor::BarLease {
                slot, generation, ..
            } => Key { slot, generation },
            _ => unreachable!(),
        };
        prepared.commit();
        let original = directory.extent(key).unwrap().0;
        drop(producer);
        let consumer = crate::Handle::receive_descriptor(descriptor.clone()).unwrap();
        assert!(!consumer.is_owner());
        assert!(consumer.map_mut().is_err());
        assert!(consumer.write_at(0, &[1]).is_err());
        let mapping = consumer.map().unwrap();
        assert_eq!(mapping.as_slice()[7], 0xa5);
        drop(consumer);
        assert_eq!(
            directory
                .slot(key.slot as usize, CONSUMERS)
                .load(Ordering::Relaxed),
            1
        );
        let blocked = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        assert_ne!(blocked.extent().unwrap().0, original);
        drop(mapping);
        let reused = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        assert_eq!(reused.extent().unwrap().0, original);
        assert!(crate::Handle::receive_descriptor(descriptor.clone()).is_err());
        let forged = crate::Handle::from_descriptor(descriptor);
        assert!(forged.map().is_err());
        assert!(forged.prepare_transfer().is_err());
    }
    #[test]
    fn writable_maps_are_exclusive_and_retain_owner_after_handle_close() {
        let directory = fixture();
        let arena = 0xb102;
        register(arena, directory.clone()).unwrap();
        let owner = crate::Handle::create_bar(arena, Role::Guest, 4096, 65536).unwrap();
        let mut map = owner.map_mut().unwrap();
        assert!(owner.map_mut().is_err());
        assert!(owner.write_at(0, &[1]).is_err());
        drop(owner);
        map.as_mut_slice()[0] = 0x35;
        let next = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        assert_ne!(next.pointer().unwrap(), map.as_mut_slice().as_mut_ptr());
        drop(map);
        assert_eq!(
            directory
                .allocate(Role::Guest, 4096, 65536)
                .unwrap()
                .extent()
                .unwrap()
                .0,
            DIRECTORY_BYTES
        );
    }
    #[test]
    fn kernel_retirement_preserves_remote_pending_and_checks_registration() {
        let directory = fixture();
        let owner = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        let (key, offset, capacity) = owner.register_producer(42).unwrap();
        let transfer = owner.publish().unwrap();
        transfer.commit();
        // Unit simulation of a trusted supervisor. Production callers must
        // satisfy the kernel-death and trusted-record safety contract.
        unsafe {
            assert!(
                directory
                    .simulate_cooperative_producer_close(key, 43, offset, capacity)
                    .is_err()
            );
            assert!(
                directory
                    .simulate_cooperative_producer_close(key, 42, offset + 1, capacity)
                    .is_err()
            );
            assert!(
                directory
                    .simulate_cooperative_producer_close(key, 42, offset, capacity)
                    .unwrap()
            );
        }
        assert_eq!(
            directory
                .slot(key.slot as usize, OWNER)
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            directory
                .slot(key.slot as usize, PENDING)
                .load(Ordering::Relaxed),
            1
        );
        assert_ne!(
            directory
                .allocate(Role::Guest, 4096, 65536)
                .unwrap()
                .extent()
                .unwrap()
                .0,
            offset
        );
        let consumer = directory.accept(key).unwrap();
        assert_eq!(
            directory
                .slot(key.slot as usize, CONSUMERS)
                .load(Ordering::Relaxed),
            1
        );
        drop(consumer);
        assert_eq!(
            directory
                .slot(key.slot as usize, CAPACITY)
                .load(Ordering::Relaxed),
            0
        );
    }
    #[test]
    fn broker_registration_must_precede_every_pointer_or_wire_exposure() {
        let directory = fixture();
        let owner = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        assert!(owner.register_producer(0).is_err());
        owner.pointer().unwrap();
        assert!(owner.register_producer(42).is_err());
        let other = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        let prepared = other.publish().unwrap();
        drop(prepared);
        assert!(other.register_producer(43).is_err());
    }
    #[test]
    fn abandoned_metadata_lock_fails_without_releasing_owner_or_consumer() {
        let directory = fixture();
        let owner = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        let key = owner.key();
        directory.word(1).store(1, Ordering::Release);
        let start = std::time::Instant::now();
        assert!(
            matches!(directory.allocate(Role::Guest, 1, 65536), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        drop(owner);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(
            directory
                .slot(key.slot as usize, OWNER)
                .load(Ordering::Relaxed),
            1
        );
        assert_ne!(
            directory
                .slot(key.slot as usize, CAPACITY)
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(directory.word(1).load(Ordering::Relaxed), 1);
    }
    #[test]
    fn independent_sender_windows_never_overlap() {
        let directory = fixture();
        let guest = directory.allocate(Role::Guest, 4096, 65536).unwrap();
        let host_directory = unsafe {
            Directory::attach(
                directory.inner.metadata as *mut u8,
                directory.inner.data as *const u8,
                (directory.inner.data + directory.inner.host_start) as *mut u8,
                Role::Host,
                directory.inner.len,
                directory.inner.host_start,
                directory.inner.run,
                directory.inner._mapping.clone(),
            )
        }
        .unwrap();
        assert!(directory.allocate(Role::Host, 4096, 65536).is_err());
        let host = host_directory.allocate(Role::Host, 4096, 65536).unwrap();
        let (offset, _, capacity) = guest.extent().unwrap();
        assert!(offset + capacity <= directory.inner.host_start);
        assert!(host.extent().unwrap().0 >= directory.inner.host_start);
    }
}

fn registry() -> &'static std::sync::Mutex<std::collections::HashMap<u32, Directory>> {
    static REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u32, Directory>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}
/// Register only after the transport authenticated the original BAR mapping.
/// An arena id cannot be replaced while any old descriptors might exist.
pub fn register(arena: u32, directory: Directory) -> io::Result<()> {
    let mut entries = registry()
        .lock()
        .map_err(|_| bad("BAR directory registry poisoned"))?;
    if entries.contains_key(&arena) {
        return Err(bad("BAR directory already registered"));
    }
    entries.insert(arena, directory);
    Ok(())
}
pub fn registered(arena: u32) -> io::Result<Directory> {
    registry()
        .lock()
        .map_err(|_| bad("BAR directory registry poisoned"))?
        .get(&arena)
        .cloned()
        .ok_or_else(|| bad("BAR directory is not attached"))
}

impl Consumer {
    pub fn publish(&self) -> io::Result<Transfer> {
        let _guard = self.directory.lock()?;
        let slot = self.directory.checked(self.key)?;
        let count = self
            .directory
            .slot(slot, PENDING)
            .load(Ordering::Relaxed)
            .checked_add(1)
            .ok_or_else(|| bad("BAR transfer count overflow"))?;
        self.directory
            .slot(slot, PENDING)
            .store(count, Ordering::Relaxed);
        Ok(Transfer {
            directory: self.directory.clone(),
            key: self.key,
            committed: false,
        })
    }
}
