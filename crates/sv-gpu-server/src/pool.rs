//! The GPU side's half of the leased arena: one pool for every session this
//! process serves, its regions as this process maps them, and the import of
//! each lease into the session's device (waycompose-design.md §97).
//!
//! # Who owns what
//!
//! [`PoolServer`] is the pool's one owner (`sv_arena::pool::Pool`, behind a
//! lock), shared by every session thread. A session holds a [`PoolBinding`] —
//! the server and its account number — in its `Objects`, and each lease it
//! takes is recorded there too, with the driver memory it was imported as, so
//! that every `(memory, offset)` the application names resolves only inside
//! leases of its own (`table::Objects::arena_slab`).
//!
//! # Two ways a lease reaches the driver
//!
//! * [`Importer::HostPointer`]: each lease is its own
//!   `VK_EXT_external_memory_host` import of the region's mapping, into the
//!   session's own device, freed when the lease is released. The guest's
//!   ivshmem path (Intel imports a BAR pointer, §87.3) and a shared file
//!   mapping on lavapipe.
//! * [`Importer::Whole`]: each region is one allocation the driver made and
//!   exported (`arena::allocate_and_export`), on a driver every session
//!   shares; a lease is a sub-range of it and nothing is imported. The
//!   development loop on RADV, which cannot import a shared mapping at all
//!   (docs/design.md §5.4).
//!
//! # No growth at run time
//!
//! The pool is the GPU ivshmem device and does not grow while the VM runs.
//! Every byte of an ivshmem BAR is pinned by vfio on testhost (QEMU's
//! `VmLck` is the guest's RAM plus both devices, to the MiB), so a region
//! added at run time would come out of the host's own RAM, which its
//! applications need; and adding one takes QEMU's monitor, which is root's.
//! The pool's size is set where the VM's RAM is set. The regions table is for
//! a second device configured the same way, not for hot-plug.

use crate::arena;
use crate::driver::Driver;
use ash::vk;
use std::sync::{Arc, Mutex, RwLock};
use sv_arena::pool::{self, Lease, MAX_REGIONS, Pool, PoolConfig, PoolError, Purpose};
use sv_transport::region::SharedRegion;

/// How leases become driver memory.
pub enum Importer {
    HostPointer,
    /// One driver allocation per region, `(memory, memory type)`.
    Whole(Vec<Option<(vk::DeviceMemory, u32)>>),
}

/// A lease as a session's device holds it.
#[derive(Debug, Clone, Copy)]
pub struct Granted {
    pub lease: Lease,
    pub memory: vk::DeviceMemory,
    pub base: u64,
    pub owned: bool,
    pub memory_type: u32,
}

/// The pool, shared by every session of this process.
pub struct PoolServer {
    pool: Mutex<Pool>,
    regions: RwLock<Vec<Option<Arc<SharedRegion>>>>,
    importer: Importer,
    /// The highest the pool's leased bytes have been logged at, in 32 MiB
    /// steps.
    logged_high: Mutex<u64>,
    next_session: std::sync::atomic::AtomicU64,
    /// §108: called with the pool unlocked when a lease does not fit; true
    /// means a region was added and the lease is worth trying again.
    grower: std::sync::OnceLock<Box<dyn Fn(u64) -> bool + Send + Sync>>,
}

/// A session's handle on the pool.
#[derive(Clone)]
pub struct PoolBinding {
    pub server: Arc<PoolServer>,
    pub session: u64,
}

impl std::fmt::Debug for PoolBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PoolBinding(session {})", self.session)
    }
}

impl PoolServer {
    /// A pool over region 0, which this process maps as `boot`.
    pub fn new(cfg: PoolConfig, boot: Arc<SharedRegion>, importer: Importer) -> Result<Arc<PoolServer>, PoolError> {
        let pool = Pool::new(cfg, boot.len() as u64)?;
        let mut regions = vec![None; MAX_REGIONS];
        regions[0] = Some(boot);
        Ok(Arc::new(PoolServer {
            pool: Mutex::new(pool),
            regions: RwLock::new(regions),
            importer,
            logged_high: Mutex::new(0),
            next_session: std::sync::atomic::AtomicU64::new(1),
            grower: std::sync::OnceLock::new(),
        }))
    }

    /// Grow on demand from now on (§108).
    pub fn set_grower(&self, g: Box<dyn Fn(u64) -> bool + Send + Sync>) {
        let _ = self.grower.set(g);
    }

    pub fn capacity_and_free(&self) -> (u64, u64) {
        let p = self.pool();
        (p.capacity(), p.free())
    }

    pub fn largest_free(&self) -> u64 {
        self.pool().largest_free()
    }

    pub fn next_region_id(&self) -> Option<u32> {
        self.pool().next_region_id()
    }

    pub fn empty_regions(&self) -> Vec<u32> {
        self.pool().empty_regions()
    }

    /// Add region `id`, which this process sees as `view`.
    pub fn add_region(&self, id: u32, view: Arc<SharedRegion>) -> Result<(), String> {
        let mut regions = self.regions.write().unwrap_or_else(|e| e.into_inner());
        let slot = regions.get_mut(id as usize).ok_or("region id out of range")?;
        self.pool().add_region(id, view.len() as u64).map_err(|e| format!("{e}"))?;
        *slot = Some(view);
        Ok(())
    }

    /// Take region `id` out of the pool, if it holds no lease. After this no
    /// lease can land in it and this process no longer reads it.
    pub fn remove_region(&self, id: u32) -> Result<(), String> {
        let mut regions = self.regions.write().unwrap_or_else(|e| e.into_inner());
        {
            let mut p = self.pool();
            p.drain(id);
            if let Err(e) = p.remove_region(id) {
                p.undrain(id);
                return Err(format!("{e}"));
            }
        }
        if let Some(s) = regions.get_mut(id as usize) {
            *s = None;
        }
        Ok(())
    }

    pub fn config(&self) -> PoolConfig {
        self.pool().config()
    }

    fn pool(&self) -> std::sync::MutexGuard<'_, Pool> {
        self.pool.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn capacity(&self) -> u64 {
        self.pool().capacity()
    }

    /// Open an account, returning the binding a session keeps.
    pub fn open_session(self: &Arc<Self>, name: &str) -> PoolBinding {
        let session = self.next_session.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.pool().open_session(session, name);
        PoolBinding { server: Arc::clone(self), session }
    }

    pub fn set_name(&self, session: u64, name: &str) {
        self.pool().set_name(session, name);
    }

    /// The pool's one log line: every region and every session's holding.
    pub fn report(&self) -> String {
        self.pool().report()
    }

    /// This process's view of region `id`.
    pub fn region(&self, id: u32) -> Option<Arc<SharedRegion>> {
        self.regions.read().unwrap_or_else(|e| e.into_inner()).get(id as usize)?.clone()
    }

    /// Read `dst.len()` bytes at pool offset `offset`.
    pub fn read_bytes(&self, offset: u64, dst: &mut [u8]) -> Result<(), String> {
        let (id, local) = pool::split(offset);
        let r = self.region(id).ok_or_else(|| format!("no region {id} in the pool"))?;
        r.read_bytes(local, dst).map_err(|e| format!("{e}"))
    }

    /// Lease without importing: what a control region needs, which the
    /// driver never sees.
    pub fn lease_raw(&self, session: u64, len: u64, purpose: Purpose) -> Result<Lease, PoolError> {
        let l = self.pool().lease(session, len, purpose)?;
        self.note_lease();
        Ok(l)
    }

    /// Lease without importing, growing the pool if it is full (§108): what a
    /// Wayland buffer in the one pool needs (§110).
    pub fn lease_growing(&self, session: u64, len: u64, purpose: Purpose) -> Result<Lease, String> {
        self.take(session, len, purpose)
    }

    fn note_lease(&self) {
        let used = {
            let p = self.pool();
            p.capacity() - p.free()
        };
        let mut high = self.logged_high.lock().unwrap_or_else(|e| e.into_inner());
        let step = 32 << 20;
        if used >= *high + step {
            *high = used / step * step;
            eprintln!("[gpu] pool high water: {}", self.report());
        }
    }

    /// Lease `len` bytes to `session` and import them into `driver`'s device.
    /// A refusal is logged with the pool's state, once per call.
    pub fn lease(&self, session: u64, len: u64, purpose: Purpose, driver: &Driver) -> Result<Granted, String> {
        let lease = self.take(session, len, purpose)?;
        match self.import(lease, driver) {
            Ok(g) => Ok(g),
            Err(e) => {
                let _ = self.pool().release(lease.offset);
                eprintln!("[gpu] lease at {:#x} ({} KiB) could not be imported: {e}", lease.offset, lease.len >> 10);
                Err(e)
            }
        }
    }

    /// The pool's half of [`PoolServer::lease`]: the range, or the refusal,
    /// logged with the pool's whole state.
    fn take(&self, session: u64, len: u64, purpose: Purpose) -> Result<Lease, String> {
        // Bound first, so that the lock is released before the refusal below
        // asks for the report: a guard in a `match` scrutinee lives to the end
        // of the `match`, and `report` takes the same lock (measured: the
        // first refused lease hung the whole process).
        let mut leased = self.pool().lease(session, len, purpose);
        // §108: the pool, not the session, ran out. Grow (with the pool
        // unlocked) and try once more.
        if matches!(leased, Err(PoolError::NoSpace { .. })) && purpose != Purpose::Control {
            if let Some(grow) = self.grower.get() {
                if grow(len) {
                    leased = self.pool().lease(session, len, purpose);
                }
            }
        }
        let lease = match leased {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[gpu] lease of {} KiB ({purpose:?}) refused: {e}; {}", len >> 10, self.report());
                return Err(format!("{e}"));
            }
        };
        self.note_lease();
        Ok(lease)
    }

    fn import(&self, lease: Lease, driver: &Driver) -> Result<Granted, String> {
        let (id, local) = pool::split(lease.offset);
        match &self.importer {
            Importer::HostPointer => {
                let region = self.region(id).ok_or("a lease in a region this process has not mapped")?;
                let (memory, ty) = arena::import_slab(driver, &region, local, lease.len).map_err(|e| format!("{e}"))?;
                Ok(Granted { lease, memory, base: 0, owned: true, memory_type: ty })
            }
            Importer::Whole(m) => {
                let (memory, ty) = m.get(id as usize).copied().flatten().ok_or("a region with no driver allocation")?;
                Ok(Granted { lease, memory, base: local, owned: false, memory_type: ty })
            }
        }
    }

    /// Give back a lease the session's device has already let go of.
    pub fn release(&self, offset: u64) -> Result<Lease, String> {
        self.pool().release(offset).map_err(|e| format!("{e}"))
    }

    /// Close an account; returns whatever it still held, already given back.
    pub fn close_session(&self, session: u64) -> Vec<Lease> {
        self.pool().close_session(session)
    }

    /// Keep a session's leases for ever (its driver may still use them).
    pub fn quarantine(&self, session: u64) {
        self.pool().quarantine(session);
    }
}

impl PoolBinding {
    pub fn lease(&self, len: u64, purpose: Purpose, driver: &Driver) -> Result<Granted, String> {
        self.server.lease(self.session, len, purpose, driver)
    }
}

/// The memory type a host-pointer import of `region` would get on `driver`,
/// asked without importing anything, so that a pooled session can answer
/// requirement queries before its first lease.
pub fn host_pointer_memory_type(driver: &Driver, region: &SharedRegion, offset: u64) -> Result<u32, String> {
    let ext = driver.external_host.as_ref().ok_or("VK_EXT_external_memory_host is missing")?;
    region.check(offset, 4096).map_err(|e| format!("{e}"))?;
    // SAFETY: a pointer inside a live mapping this process owns, by the check
    // above; the driver only queries it.
    let ptr = unsafe { region.base_ptr().add(offset as usize) } as *mut std::ffi::c_void;
    let mut hp = vk::MemoryHostPointerPropertiesEXT::default();
    let r = unsafe {
        (ext.fp().get_memory_host_pointer_properties_ext)(
            driver.device.handle(),
            vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
            ptr,
            &mut hp,
        )
    };
    if r != vk::Result::SUCCESS {
        return Err(format!("vkGetMemoryHostPointerPropertiesEXT: {r:?}"));
    }
    let preferred = driver.memory_type(hp.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE).unwrap_or(0);
    sv_arena::import::choose_memory_type(preferred, hp.memory_type_bits).map_err(|e| format!("{e}"))
}

/// Free every lease a session's device holds and close its account: the end
/// of a pooled session whose device is idle. Imports are freed before the
/// ranges go back, so no range is another session's while this device still
/// has it.
///
/// # Safety
/// `device` must be idle and must be the device the owned leases were
/// imported into.
pub unsafe fn end_session(binding: &PoolBinding, device: &ash::Device, leases: Vec<(u64, crate::table::LeaseMem)>) -> usize {
    let n = leases.len();
    for (_, l) in &leases {
        if l.owned {
            unsafe { device.free_memory(l.memory, None) };
        }
    }
    binding.server.close_session(binding.session);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u64 = 1 << 20;

    /// The pool logic without a driver: a control lease, a read through the
    /// region, a release, and an account that closes to nothing.
    #[test]
    fn leases_read_through_their_region_and_come_back() {
        let boot = Arc::new(SharedRegion::heap((64 * M) as usize).unwrap());
        let s = PoolServer::new(PoolConfig::for_capacity(64 * M), Arc::clone(&boot), Importer::HostPointer).unwrap();
        let b = s.open_session("test");
        let l = s.lease_raw(b.session, 16 * M, Purpose::Control).unwrap();
        assert_eq!(l.region(), 0);
        boot.write_bytes(pool::split(l.offset).1, b"in region 0").unwrap();
        let mut got = [0u8; 11];
        s.read_bytes(l.offset, &mut got).unwrap();
        assert_eq!(&got, b"in region 0");
        assert!(s.read_bytes(pool::pool_offset(5, 0), &mut got).is_err());
        let c = s.lease_raw(b.session, 8 * M, Purpose::Chunk).unwrap();
        s.release(c.offset).unwrap();
        assert!(s.release(c.offset).is_err());
        assert_eq!(s.close_session(b.session).len(), 1, "the control region");
        assert_eq!(s.capacity(), 64 * M);
        assert!(s.report().contains("region 0"));
    }

    /// A refusal returns, with the pool's state logged, rather than waiting on
    /// the lock it holds: the first version re-locked the pool to build its
    /// log line and hung the whole GPU process on the first full pool.
    #[test]
    fn a_refused_lease_returns_instead_of_hanging() {
        let boot = Arc::new(SharedRegion::heap((64 * M) as usize).unwrap());
        let s = PoolServer::new(PoolConfig::for_capacity(64 * M), boot, Importer::HostPointer).unwrap();
        let b = s.open_session("greedy");
        let (tx, rx) = std::sync::mpsc::channel();
        let s2 = Arc::clone(&s);
        std::thread::spawn(move || {
            let mut got = 0;
            while s2.take(b.session, 8 * M, Purpose::Chunk).is_ok() {
                got += 1;
            }
            let _ = tx.send(got);
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("a refused lease hung");
        assert!(got > 0 && got * 8 * M <= s.config().session_cap);
    }
}
