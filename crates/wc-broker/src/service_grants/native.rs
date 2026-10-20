//! Opaque association of canonical projection holds with actual native exports.
//! Native retirement consumes only ONE projection hold. It does not retire any
//! producer, pending descriptor, consumer or durable whole-grant reservation.
use super::*;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ExportKey {
    instance: [u8; 16],
    export: u64,
}
/// Immutable facts captured from an authenticated actual native exporter.
/// This type has no safe wire/caller-tuple constructor.
pub struct NativeExportIdentity {
    key: ExportKey,
    source: PhysicalBacking,
    offset: u64,
    capacity: u64,
    access: ProjectionAccess,
}
impl NativeExportIdentity {
    /// # Safety
    /// Every field must be read from the exact retained authenticated native
    /// export/monitor, with immutable maximum rights and range validated. The
    /// instance must uniquely name that kernel exporter lifetime, export must
    /// be unique in it, and source must be the full original retained object.
    /// Caller strings, wire/shared words, file stat alone, raw monitor enum or
    /// process death cannot establish these properties. Multiple/discontiguous
    /// ranges and unknown aliases must refuse rather than fabricate this type.
    pub unsafe fn from_kernel(
        instance: [u8; 16],
        export: u64,
        source: PhysicalBacking,
        offset: u64,
        capacity: u64,
        access: ProjectionAccess,
    ) -> io::Result<Self> {
        if instance == [0; 16]
            || export == 0
            || capacity == 0
            || !offset.is_multiple_of(PAGE)
            || !capacity.is_multiple_of(PAGE)
            || !offset
                .checked_add(capacity)
                .is_some_and(|n| n <= source.bar_bytes)
        {
            return Err(denied("invalid immutable native export identity"));
        }
        Ok(Self {
            key: ExportKey { instance, export },
            source,
            offset,
            capacity,
            access,
        })
    }
}
enum Stage {
    Reserved,
    Bound(ExportKey),
    Retired(ExportKey),
}
struct State {
    stage: Stage,
}
pub(super) struct Hold {
    state: Arc<Mutex<State>>,
    binding: Binding,
    allocation: u64,
    extent: Extent,
    target: Arc<dyn ServiceProcess>,
    access: ProjectionAccess,
}
/// Non-Clone authority minted only by the canonical owner before projection.
/// It binds the exact context/grant/epochs/allocation generation/capacity/target
/// and maximum rights; applications and messages cannot construct it.
pub struct ProjectionTicket {
    id: u64,
    owner: Arc<()>,
    state: Arc<Mutex<State>>,
    exports: Arc<Mutex<BTreeMap<ExportKey, u64>>>,
    binding: Binding,
    allocation: u64,
    extent: Extent,
    target: Arc<dyn ServiceProcess>,
    access: ProjectionAccess,
}
impl ProjectionTicket {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn allocation(&self) -> u64 {
        self.allocation
    }
    pub fn extent(&self) -> Extent {
        self.extent
    }
    pub fn access(&self) -> ProjectionAccess {
        self.access
    }
    pub fn target(&self) -> &Arc<dyn ServiceProcess> {
        &self.target
    }
    /// Bind only this owner ticket to the actual native export and target.
    ///
    /// # Safety
    /// Native backend must have created this exact export ONLY for this ticket,
    /// retained its actual authenticated monitor and all lifetime/provenance
    /// guards, and verified actual kernel target equals `target`. It must keep
    /// returned authority through every projection-to-monitor transition.
    /// Errors/unknown native state must quarantine, never invent a new ticket.
    pub unsafe fn bind(
        self,
        identity: NativeExportIdentity,
        target: Arc<dyn ServiceProcess>,
    ) -> io::Result<NativeProjectionAuthority> {
        let source = &self.binding.source;
        if identity.source
            != (PhysicalBacking {
                host_device: source.host_device,
                host_inode: source.host_inode,
                bar_bytes: source.bar_bytes,
            })
            || identity.offset != self.extent.offset
            || identity.capacity != self.extent.capacity
            || identity.access != self.access
            || !Arc::ptr_eq(&target, &self.target)
        {
            return Err(denied(
                "native export differs from exact canonical projection ticket",
            ));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| denied("native projection state poisoned"))?;
        if !matches!(state.stage, Stage::Reserved) {
            return Err(denied("native projection ticket already bound"));
        }
        let mut exports = self
            .exports
            .lock()
            .map_err(|_| denied("native export namespace poisoned"))?;
        if exports.contains_key(&identity.key) {
            return Err(denied(
                "native export already associated with another ticket",
            ));
        }
        exports.insert(identity.key.clone(), self.id);
        state.stage = Stage::Bound(identity.key.clone());
        drop(exports);
        drop(state);
        Ok(NativeProjectionAuthority {
            ticket: self,
            identity,
        })
    }
}
/// Retained by the exact native projection/monitor, never by a wire receiver.
pub struct NativeProjectionAuthority {
    ticket: ProjectionTicket,
    identity: NativeExportIdentity,
}
impl NativeProjectionAuthority {
    /// Produce one proof ONLY after the exact actual export's entire alias
    /// lifetime has reached irreversible native retirement.
    ///
    /// # Safety
    /// Trusted backend must query its retained original monitor and revalidate
    /// immutable instance/export/source/range/max-rights against the identity
    /// bound above. Retirement must cover ALL dup/fork/SCM_RIGHTS FDs, VMAs,
    /// native/GPU attachments, outstanding fences and future remap capability.
    /// Query error/unknown state, client ACK, elapsed time, producer/target
    /// process death or raw Retired enum without that monitor are insufficient.
    /// This proof says nothing about other exports or logical resource holds.
    pub unsafe fn prove_retired(self) -> io::Result<RetirementProof> {
        let mut state = self
            .ticket
            .state
            .lock()
            .map_err(|_| denied("native projection state poisoned"))?;
        match &state.stage {
            Stage::Bound(key) if *key == self.identity.key => {}
            _ => return Err(denied("native export authority no longer matches ticket")),
        }
        state.stage = Stage::Retired(self.identity.key.clone());
        drop(state);
        Ok(RetirementProof {
            ticket: self.ticket,
            identity: self.identity,
        })
    }
}
/// Non-Clone, one-shot native fact. It cannot release a grant on its own.
pub struct RetirementProof {
    ticket: ProjectionTicket,
    identity: NativeExportIdentity,
}
/// Exact native adapter boundary for tracked projections.
///
/// # Safety
/// Same exact-original/max-rights/target/all-alias contract as ProjectionBackend,
/// plus successful projections MUST bind ticket to their actual immutable
/// native export and retain authority through monitor retirement. No ticket
/// may be discarded/replaced after exposure. Failure keeps the owner hold and
/// quarantines the entire grant. A stock backend lacking native monitor proof
/// must return Unsupported, never create a simulated retirement authority.
pub unsafe trait TrackedProjectionBackend<B: OriginalBacking> {
    type Projection;
    fn project_tracked(
        &mut self,
        grant: Arc<RetainedGrant<B>>,
        ticket: ProjectionTicket,
    ) -> io::Result<Self::Projection>;
}
impl<S: DurableStore, B: OriginalBacking> ProtectedGrants<S, B> {
    pub(super) fn tracked_extent<P: TrackedProjectionBackend<B>>(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        grant: &GrantId,
        allocation: u64,
        extent: Extent,
        target: Process,
        access: ProjectionAccess,
        backend: &mut P,
    ) -> io::Result<P::Projection> {
        self.record(lifecycle, grant)?;
        let record = self.records.get_mut(&grant.id).unwrap();
        let native = record
            .processes
            .get(&target)
            .ok_or_else(|| denied("unknown native projection target"))?;
        native.live()?;
        let target = native.process.clone();
        let id = self.next_projection;
        self.next_projection = id
            .checked_add(1)
            .ok_or_else(|| denied("native projection identities exhausted"))?;
        let state = Arc::new(Mutex::new(State {
            stage: Stage::Reserved,
        }));
        let binding = record.guard.binding().clone();
        record.native_holds.insert(
            id,
            Hold {
                state: state.clone(),
                binding: binding.clone(),
                allocation,
                extent,
                target: target.clone(),
                access,
            },
        );
        let ticket = ProjectionTicket {
            id,
            owner: self.identity.clone(),
            state,
            exports: self.native_exports.clone(),
            binding,
            allocation,
            extent,
            target,
            access,
        };
        let guard = record.guard.clone();
        match backend.project_tracked(guard, ticket) {
            Ok(projection) => {
                let bound = record.native_holds[&id].state.lock()
                    .map(|state| matches!(state.stage, Stage::Bound(_))).unwrap_or(false);
                if !bound {
                    record.quarantined = true;
                    return Err(denied(
                        "native backend returned without exact export association",
                    ));
                }
                Ok(projection)
            }
            Err(e) => {
                record.quarantined = true;
                Err(e)
            }
        }
    }
    pub fn project_producer_tracked<P: TrackedProjectionBackend<B>>(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        producer: &RegisteredProcess,
        allocation: &AllocationId,
        backend: &mut P,
    ) -> io::Result<P::Projection> {
        self.process_role(lifecycle, producer, ProcessRole::Producer)?;
        Self::same_grant(&producer.grant, &allocation.grant)?;
        let extent = self.allocation(lifecycle, allocation)?;
        if self.records[&allocation.grant.id]
            .producers
            .get(&allocation.key)
            != Some(&producer.process)
        {
            return Err(denied("not original projection producer"));
        }
        self.tracked_extent(
            lifecycle,
            &allocation.grant,
            allocation.key.allocation,
            extent,
            producer.process,
            ProjectionAccess::ReadWrite,
            backend,
        )
    }
    pub fn project_consumer_tracked<P: TrackedProjectionBackend<B>>(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        recipient: &RegisteredProcess,
        consumer: &ConsumerId,
        backend: &mut P,
    ) -> io::Result<P::Projection> {
        self.process_role(lifecycle, recipient, ProcessRole::Consumer)?;
        Self::same_grant(&recipient.grant, &consumer.allocation.grant)?;
        let extent = self.allocation(lifecycle, &consumer.allocation)?;
        if self.records[&recipient.grant.id]
            .consumers
            .get(&(consumer.allocation.key, consumer.consumer))
            != Some(&recipient.process)
        {
            return Err(denied("not exact accepted native consumer"));
        }
        self.tracked_extent(
            lifecycle,
            &recipient.grant,
            consumer.allocation.key.allocation,
            extent,
            recipient.process,
            ProjectionAccess::ReadOnly,
            backend,
        )
    }
    /// Retire only this exact native projection hold in the current context.
    /// Logical producer/pending/consumer and durable grant holds are unchanged.
    pub fn complete_projection(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        grant: &GrantId,
        proof: RetirementProof,
    ) -> io::Result<()> {
        self.record(lifecycle, grant)?;
        if !Arc::ptr_eq(&proof.ticket.owner, &self.identity) {
            return Err(denied(
                "retirement proof belongs to another canonical owner",
            ));
        }
        let record = self.records.get_mut(&grant.id).unwrap();
        let hold = record
            .native_holds
            .get(&proof.ticket.id)
            .ok_or_else(|| denied("native projection proof already consumed or unknown"))?;
        if !Arc::ptr_eq(&hold.state, &proof.ticket.state)
            || hold.binding != proof.ticket.binding
            || hold.binding != *record.guard.binding()
            || hold.allocation != proof.ticket.allocation
            || hold.extent != proof.ticket.extent
            || hold.access != proof.ticket.access
            || !Arc::ptr_eq(&hold.target, &proof.ticket.target)
        {
            return Err(denied(
                "native retirement differs from exact original projection hold",
            ));
        }
        let state = hold
            .state
            .lock()
            .map_err(|_| denied("native projection state poisoned"))?;
        match &state.stage {
            Stage::Retired(key) if *key == proof.identity.key => {}
            _ => return Err(denied("native retirement proof not irreversibly retired")),
        }
        drop(state);
        record.native_holds.remove(&proof.ticket.id);
        Ok(())
    }
}
