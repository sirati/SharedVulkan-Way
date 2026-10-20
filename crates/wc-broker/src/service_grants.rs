//! The protected service's ONE original-BAR grant namespace across sessions.
//!
//! Source-only preparation. Production must disable all compositor/Directory
//! allocation paths in protected mode and route their requests here. No whole
//! grant is released yet: uncertain history stays quarantined; durable release
//! after actual trusted completion remains a production gate on this finite BAR.
use crate::User;
use crate::bar_control::{Allocator, Consumer, Extent, GrantIdentity, Key, Process, Transfer};
use crate::bar_registry::{DurableStore, PhysicalBacking};
use crate::service_auth::{Frame, ServiceProcess, VerifiedMessage};
use crate::service_control::{Binding, Payload, Phase};
use crate::service_lifecycle::{GrantChannel, Lifecycle, ServiceContext};
use std::{
    collections::BTreeMap,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const PAGE: u64 = 4096;
fn denied(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, s)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackingGeometry {
    pub physical: PhysicalBacking,
    /// Canonical arena offset zero in the full original physical BAR.
    pub arena_base: u64,
}
/// Full-size original backing, retained by the daemon and every grant guard.
///
/// # Safety
/// Geometry must come from retained native original kernel objects and remain
/// immutable, including full backing size and header translation. A narrowed
/// exported FD, client-selected path/Directory word or a replacement mapping
/// cannot implement this authority. Dropping the object must never recycle
/// physical bytes while any original mapping/consumer can survive.
pub unsafe trait OriginalBacking: Send + Sync + 'static {
    fn geometry(&self) -> BackingGeometry;
    /// Zero ALL original grant capacity, including padding, before first
    /// exposure. Success must mean the entire exact range was initialized.
    /// No replacement mapping, deferred scrub or logical-length-only zero.
    fn initialize_zeroed(&self, offset: u64, length: u64) -> io::Result<()>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantKind {
    Pixel,
    HostOutput,
    Stream,
}
/// Root-configured physical interval available to one allocation category.
/// Every unlisted byte, including header/metadata/padding, is unavailable.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub kind: GrantKind,
    pub offset: u64,
    pub length: u64,
}
struct FreeRun {
    kind: GrantKind,
    offset: u64,
    length: u64,
}

/// Opaque identity, tied to this exact canonical owner instance.
#[derive(Clone)]
pub struct GrantId {
    id: u64,
    owner: Arc<()>,
}
#[derive(Clone)]
pub struct RegisteredProcess {
    grant: GrantId,
    process: Process,
}
#[derive(Clone)]
pub struct AllocationId {
    grant: GrantId,
    key: Key,
}
#[derive(Clone)]
pub struct PendingTransfer {
    allocation: AllocationId,
    transfer: Transfer,
}
#[derive(Clone)]
pub struct ConsumerId {
    allocation: AllocationId,
    consumer: Consumer,
}

/// A platform projection retains this exact original grant guard, never a
/// descriptor copied from shared words. Fields are read-only outside this module.
pub struct RetainedGrant<B: OriginalBacking> {
    backing: Arc<B>,
    binding: Binding,
    kind: GrantKind,
}
impl<B: OriginalBacking> RetainedGrant<B> {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn kind(&self) -> GrantKind {
        self.kind
    }
    /// Privileged platform adapter only. Never hand this full-size authority to
    /// an application; export only the exact range supplied to project below.
    pub fn original(&self) -> &B {
        &self.backing
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionAccess { ReadOnly, ReadWrite }

/// Native projection adapter bound to an independently authenticated exact
/// target process, or protected remote export endpoint.
///
/// # Safety
/// project must expose ONLY extent's page-rounded capacity from this exact
/// retained original object to the binding's enrolled user and exact retained
/// target. It must retain the grant and actual target/mapping lifetime, refuse
/// unsupported provenance and whole-BAR FD/HANDLE aliases, and quarantine every
/// uncertain failure. A copied file is not a projection of original bytes.
/// Access must be an immutable maximum, including later mmap/mprotect/native
/// attachment paths. If ReadOnly cannot be enforced, refuse, never return RW.
/// Returning an error must not make exposed bytes eligible for reuse.
pub unsafe trait ProjectionBackend<B: OriginalBacking> {
    type Projection;
    fn project(
        &mut self,
        grant: Arc<RetainedGrant<B>>,
        extent: Extent,
        target: Arc<dyn ServiceProcess>,
        access: ProjectionAccess,
    ) -> io::Result<Self::Projection>;
}
struct NativeProcess {
    process: Arc<dyn ServiceProcess>,
    revoked: AtomicBool,
}
impl NativeProcess {
    fn live(&self) -> io::Result<()> {
        if self.revoked.load(Ordering::SeqCst) {
            return Err(denied("native process registration revoked"));
        }
        match self.process.exited() {
            Ok(false) if !self.revoked.load(Ordering::SeqCst) => Ok(()),
            _ => {
                self.revoked.store(true, Ordering::SeqCst);
                Err(denied("native process is dead or unavailable"))
            }
        }
    }
}
struct Record<B: OriginalBacking> {
    guard: Arc<RetainedGrant<B>>,
    channel: GrantChannel,
    table: Allocator,
    processes: BTreeMap<Process, NativeProcess>,
    producers: BTreeMap<Key, Process>,
    consumers: BTreeMap<(Key, Consumer), Process>,
    quarantined: bool,
}
/// Owns all categories and all sessions in one physical free list. Construction
/// claims the context exactly once; dropping it cannot start a parallel owner.
pub struct ProtectedGrants<S: DurableStore, B: OriginalBacking> {
    context: Arc<ServiceContext<S, B>>,
    identity: Arc<()>,
    free: Vec<FreeRun>,
    records: BTreeMap<u64, Record<B>>,
    geometry: BackingGeometry,
}
impl<S: DurableStore, B: OriginalBacking> ProtectedGrants<S, B> {
    /// Start the sole protected canonical allocator. The protected launcher
    /// must already have retired every legacy whole-device allocation route.
    ///
    /// # Safety
    /// All original allocation aliases (compositor pixel grants, host output,
    /// clipboard streams and any other category) must be disabled or routed to
    /// this owner. Segment boundaries must come from protected canonical layout
    /// configuration, never cooperative metadata. Unknown prior registry state
    /// must refuse startup rather than initialize a fresh ownership history.
    pub unsafe fn start(
        context: Arc<ServiceContext<S, B>>,
        mut segments: Vec<Segment>,
    ) -> io::Result<Self> {
        let geometry = context.backing().geometry();
        let source = context.source();
        if geometry.physical.host_device != source.host_device
            || geometry.physical.host_inode != source.host_inode
            || geometry.physical.bar_bytes != source.bar_bytes
            || !geometry.arena_base.is_multiple_of(PAGE)
            || geometry.arena_base >= source.bar_bytes
        {
            return Err(denied("original backing geometry differs"));
        }
        segments.sort_by_key(|s| s.offset);
        let mut end = geometry.arena_base;
        for segment in &segments {
            if segment.length == 0
                || !segment.offset.is_multiple_of(PAGE)
                || !segment.length.is_multiple_of(PAGE)
                || segment.offset < end
                || !segment
                    .offset
                    .checked_add(segment.length)
                    .is_some_and(|n| n <= source.bar_bytes)
            {
                return Err(denied("invalid or overlapping protected physical layout"));
            }
            end = segment.offset + segment.length;
        }
        if segments.is_empty() {
            return Err(denied("protected layout has no allocatable ranges"));
        }
        let exclusions = context.claim_allocator()?;
        let mut free: Vec<_> = segments
            .into_iter()
            .map(|s| FreeRun {
                kind: s.kind,
                offset: s.offset,
                length: s.length,
            })
            .collect();
        // Subtract every durable physical record, without filtering by source
        // run, session, installation or category. Partial/outside intersections
        // are handled in full physical coordinates before any first-fit search.
        for (offset, length) in exclusions {
            let end = offset
                .checked_add(length)
                .ok_or_else(|| denied("durable exclusion overflow"))?;
            let mut remaining = Vec::new();
            for run in free {
                let run_end = run.offset + run.length;
                if end <= run.offset || offset >= run_end {
                    remaining.push(run);
                    continue;
                }
                if run.offset < offset {
                    remaining.push(FreeRun {
                        kind: run.kind,
                        offset: run.offset,
                        length: offset - run.offset,
                    });
                }
                if end < run_end {
                    remaining.push(FreeRun {
                        kind: run.kind,
                        offset: end,
                        length: run_end - end,
                    });
                }
            }
            free = remaining;
        }
        Ok(Self {
            context,
            identity: Arc::new(()),
            free,
            records: BTreeMap::new(),
            geometry,
        })
    }
    fn check_lifecycle(&self, lifecycle: &Lifecycle<S, B>) -> io::Result<()> {
        if !lifecycle.belongs_to(&self.context) {
            return Err(denied("session belongs to another service context"));
        }
        lifecycle.ready()
    }
    fn record(&self, lifecycle: &Lifecycle<S, B>, id: &GrantId) -> io::Result<&Record<B>> {
        self.check_lifecycle(lifecycle)?;
        if !Arc::ptr_eq(&self.identity, &id.owner) {
            return Err(denied("foreign canonical grant owner"));
        }
        let record = self
            .records
            .get(&id.id)
            .ok_or_else(|| denied("unknown canonical grant"))?;
        if record.quarantined {
            return Err(denied("canonical grant quarantined"));
        }
        lifecycle.channel_ready(&record.channel)?;
        Ok(record)
    }
    /// Create a service-owned existing canonical grant. No descriptor, mapping
    /// or suballocator exists until the whole reservation is durable.
    pub fn grant(
        &mut self,
        lifecycle: &mut Lifecycle<S, B>,
        kind: GrantKind,
        capacity: u64,
    ) -> io::Result<GrantId> {
        self.check_lifecycle(lifecycle)?;
        let length = capacity
            .checked_add(PAGE - 1)
            .map(|n| n & !(PAGE - 1))
            .filter(|n| *n > 0)
            .ok_or_else(|| denied("invalid canonical grant size"))?;
        let i = self
            .free
            .iter()
            .position(|r| r.kind == kind && r.length >= length)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "protected physical namespace exhausted",
                )
            })?;
        let offset = self.free[i].offset;
        // Remove locally BEFORE durable reservation. Every subsequent failure
        // keeps this range unavailable; uncertain writes cannot make it free.
        self.free[i].offset += length;
        self.free[i].length -= length;
        if self.free[i].length == 0 {
            self.free.remove(i);
        }
        let id = self.context.next_grant()?;
        let binding = lifecycle.grant_binding(id, offset, length)?;
        let guard = Arc::new(RetainedGrant {
            backing: self.context.backing().clone(),
            binding: binding.clone(),
            kind,
        });
        // SAFETY: this sole canonical owner selected disjoint physical bytes
        // after importing all durable history; guard retains full original.
        let channel = unsafe { lifecycle.reserve_channel(binding.clone(), guard.clone())? };
        // Full-capacity initialization follows durable reservation. An error
        // leaves the complete physical range excluded without grant exposure.
        guard.backing.initialize_zeroed(offset, length)?;
        let region =
            u32::try_from(id).map_err(|_| denied("canonical bookkeeping IDs exhausted"))?;
        let grant = wc_handle::barpool::Grant {
            id: region,
            session: 1,
            region: 0,
            offset: offset - self.geometry.arena_base,
            len: length,
        };
        let identity = GrantIdentity {
            bar_run: binding.source.bar_run,
            bar_offset_base: self.geometry.arena_base,
            session: 1,
            region,
            user: match self.context.origin() {
                crate::service_control::Origin::Windows => User::sid(&binding.windows_sid),
                crate::service_control::Origin::Linux => User::uid(binding.linux_uid),
            },
        };
        // SAFETY: canonical record above, never cooperative Directory state.
        let table = unsafe { Allocator::from_delegated_grant(id, identity, grant, guard.clone())? };
        self.records.insert(
            id,
            Record {
                guard,
                channel,
                table,
                processes: BTreeMap::new(),
                producers: BTreeMap::new(),
                consumers: BTreeMap::new(),
                quarantined: false,
            },
        );
        Ok(GrantId {
            id,
            owner: self.identity.clone(),
        })
    }
    /// Capture only the actual enrolled user's exact native process and watch.
    ///
    /// # Safety
    /// User must be obtained from this exact connection's OS credentials and
    /// process must retain its original kernel object without PID substitution.
    /// Protected provider provenance is additionally required where applicable.
    pub unsafe fn register_process(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        grant: &GrantId,
        user: User,
        process: Arc<dyn ServiceProcess>,
    ) -> io::Result<RegisteredProcess> {
        let record = self.record(lifecycle, grant)?;
        if &user != &record.table.grant().user {
            return Err(denied("process user differs from canonical grant"));
        }
        let native = NativeProcess {
            process,
            revoked: AtomicBool::new(false),
        };
        native.live()?;
        let record = self.records.get_mut(&grant.id).unwrap();
        let registration = record.table.register_process(user)?;
        record.processes.insert(registration, native);
        Ok(RegisteredProcess {
            grant: grant.clone(),
            process: registration,
        })
    }
    fn process(&self, lifecycle: &Lifecycle<S, B>, process: &RegisteredProcess) -> io::Result<()> {
        self.record(lifecycle, &process.grant)?
            .processes
            .get(&process.process)
            .ok_or_else(|| denied("unknown native process registration"))?
            .live()
    }
    fn allocation(
        &self,
        lifecycle: &Lifecycle<S, B>,
        allocation: &AllocationId,
    ) -> io::Result<Extent> {
        self.record(lifecycle, &allocation.grant)?
            .table
            .extent(allocation.key)
            .ok_or_else(|| denied("unknown canonical allocation"))
    }
    fn same_grant(a: &GrantId, b: &GrantId) -> io::Result<()> {
        if a.id != b.id || !Arc::ptr_eq(&a.owner, &b.owner) {
            return Err(denied("resource belongs to another canonical grant"));
        }
        Ok(())
    }
    pub fn allocate(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        process: &RegisteredProcess,
        length: u64,
        capacity: u64,
    ) -> io::Result<(AllocationId, Extent)> {
        self.process(lifecycle, process)?;
        let (key, extent) = self
            .records
            .get_mut(&process.grant.id)
            .unwrap()
            .table
            .allocate_reserved(process.process, length, capacity)?;
        self.records
            .get_mut(&process.grant.id)
            .unwrap()
            .producers
            .insert(key, process.process);
        Ok((
            AllocationId {
                grant: process.grant.clone(),
                key,
            },
            extent,
        ))
    }
    pub fn resize(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        process: &RegisteredProcess,
        allocation: &AllocationId,
        length: u64,
    ) -> io::Result<Extent> {
        self.process(lifecycle, process)?;
        self.allocation(lifecycle, allocation)?;
        Self::same_grant(&process.grant, &allocation.grant)?;
        self.records
            .get_mut(&allocation.grant.id)
            .unwrap()
            .table
            .resize(process.process, allocation.key, length)
    }
    pub fn prepare(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        producer: &RegisteredProcess,
        allocation: &AllocationId,
        recipient: &RegisteredProcess,
    ) -> io::Result<PendingTransfer> {
        self.process(lifecycle, producer)?;
        self.process(lifecycle, recipient)?;
        self.allocation(lifecycle, allocation)?;
        Self::same_grant(&producer.grant, &allocation.grant)?;
        Self::same_grant(&recipient.grant, &allocation.grant)?;
        let transfer = self
            .records
            .get_mut(&allocation.grant.id)
            .unwrap()
            .table
            .prepare(producer.process, allocation.key, recipient.process)?;
        Ok(PendingTransfer {
            allocation: allocation.clone(),
            transfer,
        })
    }
    pub fn accept(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        recipient: &RegisteredProcess,
        pending: &PendingTransfer,
    ) -> io::Result<(ConsumerId, Extent)> {
        self.process(lifecycle, recipient)?;
        self.allocation(lifecycle, &pending.allocation)?;
        Self::same_grant(&recipient.grant, &pending.allocation.grant)?;
        let (consumer, extent) = self
            .records
            .get_mut(&pending.allocation.grant.id)
            .unwrap()
            .table
            .accept(recipient.process, pending.allocation.key, pending.transfer)?;
        self.records
            .get_mut(&pending.allocation.grant.id)
            .unwrap()
            .consumers
            .insert((pending.allocation.key, consumer), recipient.process);
        Ok((
            ConsumerId {
                allocation: pending.allocation.clone(),
                consumer,
            },
            extent,
        ))
    }
    /// Project only the exact producer's own currently live allocation.
    pub fn project_producer<P: ProjectionBackend<B>>(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        producer: &RegisteredProcess,
        allocation: &AllocationId,
        backend: &mut P,
    ) -> io::Result<P::Projection> {
        self.process(lifecycle, producer)?;
        Self::same_grant(&producer.grant, &allocation.grant)?;
        let record = self.record(lifecycle, &allocation.grant)?;
        if record.producers.get(&allocation.key) != Some(&producer.process) {
            return Err(denied(
                "projection target is not the original allocation producer",
            ));
        }
        self.project(lifecycle, allocation, producer.process, ProjectionAccess::ReadWrite, backend)
    }
    /// Project only an accepted consumer's exact retained native process.
    pub fn project_consumer<P: ProjectionBackend<B>>(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        recipient: &RegisteredProcess,
        consumer: &ConsumerId,
        backend: &mut P,
    ) -> io::Result<P::Projection> {
        self.process(lifecycle, recipient)?;
        Self::same_grant(&recipient.grant, &consumer.allocation.grant)?;
        let record = self.record(lifecycle, &consumer.allocation.grant)?;
        if record
            .consumers
            .get(&(consumer.allocation.key, consumer.consumer))
            != Some(&recipient.process)
        {
            return Err(denied("projection target is not this allocation consumer"));
        }
        self.project(lifecycle, &consumer.allocation, recipient.process, ProjectionAccess::ReadOnly, backend)
    }
    // Any native projection error quarantines the full canonical grant because
    // exposure may be uncertain. No subsequent descriptor or mutation escapes.
    fn project<P: ProjectionBackend<B>>(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        allocation: &AllocationId,
        target: Process,
        access: ProjectionAccess,
        backend: &mut P,
    ) -> io::Result<P::Projection> {
        let extent = self.allocation(lifecycle, allocation)?;
        let record = self.records.get(&allocation.grant.id).unwrap();
        let guard = record.guard.clone();
        let target = record
            .processes
            .get(&target)
            .ok_or_else(|| denied("missing native projection target"))?
            .process
            .clone();
        match backend.project(guard, extent, target, access) {
            Ok(projection) => Ok(projection),
            Err(e) => {
                self.records
                    .get_mut(&allocation.grant.id)
                    .unwrap()
                    .quarantined = true;
                Err(e)
            }
        }
    }
    pub fn seal(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        grant: &GrantId,
        phase: Phase,
        payload: Payload,
    ) -> io::Result<Frame> {
        self.record(lifecycle, grant)?;
        lifecycle.seal(
            &mut self.records.get_mut(&grant.id).unwrap().channel,
            phase,
            payload,
        )
    }
    /// Evidence is historical only. No resource mutation accepts it later.
    pub fn verify(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        grant: &GrantId,
        frame: &Frame,
    ) -> io::Result<VerifiedMessage> {
        self.record(lifecycle, grant)?;
        lifecycle.verify(&mut self.records.get_mut(&grant.id).unwrap().channel, frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bar_registry::Registry,
        service_auth::{ServiceEndpoint, ServicePolicy},
        service_bootstrap::BootstrapBinding,
        service_control::Origin,
        service_lifecycle::BootstrapRole,
    };
    use std::sync::{Mutex, atomic::AtomicU8};
    const MIB: u64 = 1 << 20;
    const KEY: [u8; 32] = [0x77; 32];
    #[derive(Clone, Default)]
    struct Store {
        data: Arc<Mutex<Option<Vec<u8>>>>,
        fail: Arc<AtomicBool>,
    }
    // SAFETY: isolated simulated exclusive durability for state-machine tests.
    unsafe impl DurableStore for Store {
        fn load(&mut self) -> io::Result<Vec<u8>> {
            self.data
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| io::ErrorKind::NotFound.into())
        }
        fn replace_synced(&mut self, b: &[u8]) -> io::Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(io::Error::other("injected durable failure"));
            }
            *self.data.lock().unwrap() = Some(b.to_vec());
            Ok(())
        }
    }
    struct Backing(BackingGeometry, Store, Origin, AtomicBool);
    // SAFETY: geometry fixture only, not a native original-backing claim.
    unsafe impl OriginalBacking for Backing {
        fn geometry(&self) -> BackingGeometry {
            self.0
        }
        fn initialize_zeroed(&self, offset: u64, length: u64) -> io::Result<()> {
            assert!(offset >= self.0.arena_base && offset + length <= self.0.physical.bar_bytes);
            // Verify ordering against the durable snapshot, not a local flag.
            let registry = Registry::open(self.1.clone(), self.0.physical, self.2).unwrap();
            assert!(registry.exclusions().contains(&(offset, length)));
            if self.3.load(Ordering::SeqCst) { return Err(io::Error::other("partial initialization fixture failure")); }
            Ok(()) // simulated initialization, not a native memory claim
        }
    }
    struct Watch(AtomicU8);
    // SAFETY: deterministic process query fixture only.
    unsafe impl ServiceProcess for Watch {
        fn exited(&self) -> io::Result<bool> {
            match self.0.load(Ordering::SeqCst) {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(io::Error::other("query failed")),
            }
        }
    }
    type Context = Arc<ServiceContext<Store, Backing>>;
    type Life = Lifecycle<Store, Backing>;
    type Owner = ProtectedGrants<Store, Backing>;
    fn binding() -> BootstrapBinding {
        BootstrapBinding::from(&crate::service_control::fixture().binding)
    }
    fn physical() -> PhysicalBacking {
        let s = binding().source;
        PhysicalBacking {
            host_device: s.host_device,
            host_inode: s.host_inode,
            bar_bytes: s.bar_bytes,
        }
    }
    fn context(origin: Origin, store: Store, reopen: bool) -> (Context, Arc<Watch>) {
        let registry = if reopen {
            Registry::open(store.clone(), physical(), origin).unwrap()
        } else {
            unsafe { Registry::initialize_fresh(store.clone(), physical(), origin).unwrap() }
        };
        let watch = Arc::new(Watch(AtomicU8::new(0)));
        let (policy, user) = match origin {
            Origin::Windows => (ServicePolicy::windows_system(), User::sid("S-1-5-18")),
            Origin::Linux => (ServicePolicy::linux_root(), User::uid(0)),
        };
        let endpoint =
            unsafe { ServiceEndpoint::from_current_service(&policy, user, watch.clone()).unwrap() };
        let context = unsafe {
            ServiceContext::start(
                registry,
                endpoint,
                binding().source,
                Arc::new(Backing(BackingGeometry {
                    physical: physical(), arena_base: PAGE,
                }, store, origin, AtomicBool::new(false))),
            )
            .unwrap()
        };
        (context, watch)
    }
    fn sessions(w: &Context, l: &Context, install: &str, session: u64) -> (Life, Life) {
        let mut b = binding();
        b.installation = install.into();
        b.session = session;
        let mut wl = unsafe {
            Lifecycle::start(w.clone(), install, KEY, b.clone(), BootstrapRole::Initiator).unwrap()
        };
        let mut ll = unsafe {
            Lifecycle::start(l.clone(), install, KEY, b, BootstrapRole::Responder).unwrap()
        };
        let h = wl.hello().unwrap();
        let c = ll.receive(&h).unwrap().unwrap();
        let f = wl.receive(&c).unwrap().unwrap();
        ll.receive(&f).unwrap();
        (wl, ll)
    }
    fn layout() -> Vec<Segment> {
        vec![
            Segment {
                kind: GrantKind::Pixel,
                offset: PAGE,
                length: 8 * MIB - PAGE,
            },
            Segment {
                kind: GrantKind::HostOutput,
                offset: 8 * MIB,
                length: 4 * MIB,
            },
            Segment {
                kind: GrantKind::Stream,
                offset: 12 * MIB,
                length: 4 * MIB,
            },
        ]
    }
    fn owner(c: &Context) -> Owner {
        unsafe { ProtectedGrants::start(c.clone(), layout()).unwrap() }
    }
    fn setup() -> (
        Owner,
        Life,
        Life,
        Context,
        Context,
        Store,
        Store,
        Arc<Watch>,
    ) {
        let ws = Store::default();
        let ls = Store::default();
        let (w, watch) = context(Origin::Windows, ws.clone(), false);
        let (l, _) = context(Origin::Linux, ls.clone(), false);
        let (wl, ll) = sessions(&w, &l, "test-install", 3);
        let o = owner(&w);
        (o, wl, ll, w, l, ws, ls, watch)
    }
    fn process(o: &mut Owner, l: &Life, g: &GrantId) -> (RegisteredProcess, Arc<Watch>) {
        let p = Arc::new(Watch(AtomicU8::new(0)));
        let id = unsafe {
            o.register_process(l, g, User::sid(&binding().windows_sid), p.clone())
                .unwrap()
        };
        (id, p)
    }
    fn range(o: &Owner, g: &GrantId) -> (u64, u64) {
        let b = o.records[&g.id].guard.binding();
        (b.grant_offset, b.grant_length)
    }
    #[test]
    fn one_physical_owner_serves_multiple_sessions_and_every_category() {
        let (mut o, mut a, _, w, l, _, _, _) = setup();
        assert!(unsafe { ProtectedGrants::start(w.clone(), layout()) }.is_err());
        let (mut b, _) = sessions(&w, &l, "second-install", 4);
        let ga = o.grant(&mut a, GrantKind::Pixel, 1).unwrap();
        let gb = o.grant(&mut b, GrantKind::Pixel, 1).unwrap();
        assert_eq!(range(&o, &ga), (PAGE, PAGE));
        assert_eq!(range(&o, &gb), (2 * PAGE, PAGE));
        let output = o.grant(&mut a, GrantKind::HostOutput, PAGE).unwrap();
        let stream = o.grant(&mut b, GrantKind::Stream, PAGE).unwrap();
        assert_eq!(range(&o, &output), (8 * MIB, PAGE));
        assert_eq!(range(&o, &stream), (12 * MIB, PAGE));
        assert!(
            o.seal(&b, &ga, Phase::Allocate, Payload::default())
                .is_err()
        );
        assert!(o.record(&a, &gb).is_err());
    }
    #[test]
    fn restart_imports_all_old_physical_exclusions_before_first_fit() {
        let (mut o, mut life, peer, w, l, ws, ls, _) = setup();
        for kind in [GrantKind::Pixel, GrantKind::HostOutput, GrantKind::Stream] {
            o.grant(&mut life, kind, 2 * PAGE).unwrap();
        }
        drop(o);
        drop(life);
        drop(peer);
        drop(w);
        drop(l);
        let (w, _) = context(Origin::Windows, ws, true);
        let (l, _) = context(Origin::Linux, ls, true);
        let (mut life, _) = sessions(&w, &l, "test-install", 3);
        let mut o = owner(&w);
        for (kind, offset) in [
            (GrantKind::Pixel, 3 * PAGE),
            (GrantKind::HostOutput, 8 * MIB + 2 * PAGE),
            (GrantKind::Stream, 12 * MIB + 2 * PAGE),
        ] {
            let g = o.grant(&mut life, kind, PAGE).unwrap();
            assert_eq!(range(&o, &g), (offset, PAGE));
        }
    }
    #[test]
    fn durable_failure_prevents_grant_or_projection_and_poison_is_not_reset() {
        let (mut o, mut life, _, _, _, store, _, _) = setup();
        store.fail.store(true, Ordering::SeqCst);
        assert!(o.grant(&mut life, GrantKind::Pixel, PAGE).is_err());
        assert!(o.records.is_empty());
        store.fail.store(false, Ordering::SeqCst);
        assert!(o.grant(&mut life, GrantKind::Pixel, PAGE).is_err());
        assert!(life.ready().is_err());
    }
    struct Backend {
        target: Arc<dyn ServiceProcess>,
        calls: usize,
        fail: bool,
    }
    // SAFETY: test-only projection receipt, creates no real mapping/descriptor.
    unsafe impl ProjectionBackend<Backing> for Backend {
        type Projection = (Arc<RetainedGrant<Backing>>, Extent, ProjectionAccess);
        fn project(
            &mut self,
            g: Arc<RetainedGrant<Backing>>,
            e: Extent,
            target: Arc<dyn ServiceProcess>,
            access: ProjectionAccess,
        ) -> io::Result<Self::Projection> {
            assert!(Arc::ptr_eq(&self.target, &target));
            self.calls += 1;
            if self.fail {
                return Err(io::Error::other("uncertain native projection"));
            }
            Ok((g, e, access))
        }
    }
    #[test]
    fn projection_requires_original_producer_or_exact_accepted_consumer_and_retains_full_backing() {
        let (mut o, mut life, _, _, _, _, _, _) = setup();
        let g = o.grant(&mut life, GrantKind::Pixel, 4 * PAGE).unwrap();
        let (p, watch) = process(&mut o, &life, &g);
        let (c, cwatch) = process(&mut o, &life, &g);
        let (other, _) = process(&mut o, &life, &g);
        let (a, e) = o.allocate(&life, &p, 1, 2 * PAGE).unwrap();
        assert_eq!(e.offset, PAGE);
        assert_eq!(e.capacity, 2 * PAGE);
        let mut backend = Backend {
            target: watch,
            calls: 0,
            fail: false,
        };
        assert!(o.project_producer(&life, &other, &a, &mut backend).is_err());
        assert_eq!(backend.calls, 0);
        let (projection, e, access) = o.project_producer(&life, &p, &a, &mut backend).unwrap();
        assert_eq!(access, ProjectionAccess::ReadWrite);
        assert_eq!(
            projection.original().geometry().physical.bar_bytes,
            16 * MIB
        );
        assert_eq!(projection.binding().grant_length, 4 * PAGE);
        let pending = o.prepare(&life, &p, &a, &c).unwrap();
        let (consumer, _) = o.accept(&life, &c, &pending).unwrap();
        let mut backend = Backend {
            target: cwatch,
            calls: 0,
            fail: false,
        };
        assert!(
            o.project_consumer(&life, &other, &consumer, &mut backend)
                .is_err()
        );
        let (_, _, access) = o.project_consumer(&life, &c, &consumer, &mut backend).unwrap();
        assert_eq!(access, ProjectionAccess::ReadOnly);
        assert_eq!(backend.calls, 1);
        let grown = o.resize(&life, &p, &a, 2 * PAGE).unwrap();
        assert_eq!(grown.offset, e.offset);
        assert_eq!(grown.capacity, e.capacity);
    }
    #[test]
    fn native_projection_uncertainty_quarantines_whole_grant() {
        let (mut o, mut life, _, _, _, _, _, _) = setup();
        let g = o.grant(&mut life, GrantKind::Pixel, 2 * PAGE).unwrap();
        let (p, watch) = process(&mut o, &life, &g);
        let (a, _) = o.allocate(&life, &p, 1, PAGE).unwrap();
        let mut backend = Backend {
            target: watch,
            calls: 0,
            fail: true,
        };
        assert!(o.project_producer(&life, &p, &a, &mut backend).is_err());
        assert!(o.allocate(&life, &p, 1, PAGE).is_err());
        assert!(o.resize(&life, &p, &a, PAGE).is_err());
        assert!(o.seal(&life, &g, Phase::Grant, Payload::default()).is_err());
    }
    #[test]
    fn retained_historical_evidence_never_bypasses_current_context_or_process_checks() {
        let (mut o, mut life, _, _, _, _, _, service) = setup();
        let g = o.grant(&mut life, GrantKind::Pixel, 3 * PAGE).unwrap();
        let (p, process) = process(&mut o, &life, &g);
        let (a, _) = o.allocate(&life, &p, 1, PAGE).unwrap();
        process.0.store(2, Ordering::SeqCst);
        assert!(o.resize(&life, &p, &a, PAGE).is_err());
        process.0.store(0, Ordering::SeqCst);
        assert!(o.allocate(&life, &p, 1, PAGE).is_err());
        service.0.store(1, Ordering::SeqCst);
        assert!(o.grant(&mut life, GrantKind::Pixel, PAGE).is_err());
        service.0.store(0, Ordering::SeqCst);
        assert!(o.allocate(&life, &p, 1, PAGE).is_err());
        assert!(
            o.verify(&life, &g, &Frame::from_wire(&[1], &[0; 32]).unwrap())
                .is_err()
        );
        assert_eq!(o.records.len(), 1);
    }
    #[test]
    fn initialization_failure_leaves_durable_exclusion_without_exposure() {
        let (mut o, mut life, _, context, _, _, _, _) = setup();
        context.backing().3.store(true, Ordering::SeqCst);
        assert!(o.grant(&mut life, GrantKind::Pixel, PAGE).is_err());
        assert!(o.records.is_empty());
        assert_eq!(context.exclusions().unwrap(), vec![(PAGE,PAGE)]);
        context.backing().3.store(false, Ordering::SeqCst);
        let next = o.grant(&mut life, GrantKind::Pixel, PAGE).unwrap();
        assert_eq!(range(&o,&next),(2*PAGE,PAGE));
    }
    #[test]
    fn layout_rejects_overlap_header_outside_and_unaligned_ranges() {
        for bad in [
            vec![Segment {
                kind: GrantKind::Pixel,
                offset: 0,
                length: PAGE,
            }],
            vec![Segment {
                kind: GrantKind::Pixel,
                offset: PAGE,
                length: 1,
            }],
            vec![
                Segment {
                    kind: GrantKind::Pixel,
                    offset: PAGE,
                    length: 2 * PAGE,
                },
                Segment {
                    kind: GrantKind::Stream,
                    offset: 2 * PAGE,
                    length: PAGE,
                },
            ],
            vec![Segment {
                kind: GrantKind::Pixel,
                offset: 16 * MIB,
                length: PAGE,
            }],
        ] {
            let (c, _) = context(Origin::Windows, Store::default(), false);
            assert!(unsafe { ProtectedGrants::start(c, bad) }.is_err());
        }
    }
}
