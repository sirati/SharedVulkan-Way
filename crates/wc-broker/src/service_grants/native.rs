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
/// Protected admission policy. Limits count outstanding tickets, including
/// failed or unknown exports. Completion returns admission only for its hold.
#[derive(Clone, Copy, Debug)]
pub struct NativeLimits {
    total: usize,
    per_grant: usize,
    per_reference: usize,
    capacity_bytes: u64,
}
impl NativeLimits {
    pub fn new(
        total: usize,
        per_grant: usize,
        per_reference: usize,
        capacity_bytes: u64,
    ) -> io::Result<Self> {
        if per_reference == 0
            || per_reference > per_grant
            || per_grant > total
            || capacity_bytes == 0
        {
            return Err(denied("invalid native admission limits"));
        }
        Ok(Self {
            total,
            per_grant,
            per_reference,
            capacity_bytes,
        })
    }
}
/// Retained authenticated native device authority, enrolled by the launcher.
///
/// # Safety
/// Implementors must retain the exact authenticated provider/device for the
/// complete owner lifetime and guarantee fresh monotonic nonwrapping export IDs.
/// Every tracked backend must compare this exact Arc before CREATE, validate
/// immutable INFO from that retained device and serialize CREATE through bind.
/// A type name, caller string, path or supplied instance tuple is insufficient.
pub unsafe trait NativeProviderAuthority: Send + Sync {
    /// Revalidate the exact retained raw device/module authority. Does not
    /// validate any original source, which has its own native owner guard.
    fn validate_live_provider(&self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no native provider liveness validator"))
    }
}
pub(super) struct Provider {
    instance: Option<[u8; 16]>,
    authority: Arc<dyn NativeProviderAuthority>,
    gate: Arc<Mutex<()>>,
    pub(super) high_water: u64,
    pub(super) limits: NativeLimits,
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
/// Logical reference is owner-derived inside the opaque ticket. Merely
/// constructing this value grants no authority or completion proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectionReference {
    Producer { registration: u64 },
    Consumer { registration: u64, consumer: u64 },
    RemoteConsumer { registration: u64, consumer: u64 },
}
pub(super) struct Hold {
    state: Arc<Mutex<State>>,
    binding: Binding,
    allocation: u64,
    reference: ProjectionReference,
    extent: Extent,
    target: Arc<dyn ServiceProcess>,
    access: ProjectionAccess,
}
impl Hold {
    pub(super) fn matches_reference(
        &self,
        allocation: u64,
        reference: ProjectionReference,
    ) -> bool {
        self.allocation == allocation && self.reference == reference
    }
}
/// Non-Clone authority minted only by the canonical owner before projection.
/// It binds the exact context/grant/epochs/allocation generation/capacity/target
/// and maximum rights; applications and messages cannot construct it.
pub struct ProjectionTicket {
    id: u64,
    owner: Arc<()>,
    state: Arc<Mutex<State>>,
    exports: Arc<Mutex<Option<Provider>>>,
    provider: Arc<dyn NativeProviderAuthority>,
    binding: Binding,
    allocation: u64,
    reference: ProjectionReference,
    extent: Extent,
    target: Arc<dyn ServiceProcess>,
    access: ProjectionAccess,
}
impl ProjectionTicket {
    /// Actual retained provider authority enrolled by the protected launcher.
    /// Backend must compare this exact Arc with its own device guard before CREATE.
    pub fn provider(&self) -> &Arc<dyn NativeProviderAuthority> {
        &self.provider
    }

    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn allocation(&self) -> u64 {
        self.allocation
    }
    pub fn reference(&self) -> ProjectionReference {
        self.reference
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
    /// This must remain synchronous inside the owner callback: fresh CREATE,
    /// immutable validation and bind are serialized across every backend for
    /// this owner. Export IDs must obey the enrolled monotonic contract.
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
        let provider = exports
            .as_mut()
            .ok_or_else(|| denied("native provider not enrolled"))?;
        if provider
            .instance
            .is_some_and(|instance| instance != identity.key.instance)
            || identity.key.export <= provider.high_water
        {
            return Err(denied(
                "native provider changed or export identity is not fresh",
            ));
        }
        provider.instance = Some(identity.key.instance);
        provider.high_water = identity.key.export;
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
pub(super) struct Request {
    pub(super) allocation: u64,
    pub(super) reference: ProjectionReference,
    pub(super) extent: Extent,
    pub(super) target: Process,
    pub(super) access: ProjectionAccess,
}
impl<S: DurableStore, B: OriginalBacking> ProtectedGrants<S, B> {
    /// Enroll one independently authenticated native exporter for this owner.
    /// It cannot be replaced or cleared in this context, even after draining.
    /// A changed provider requires complete two-service drain and fresh context.
    ///
    /// # Safety
    /// The protected launcher must authenticate and retain the exact original
    /// exporter device authority before enrollment. The first immutable INFO
    /// pins its nonzero instance once; it can never reset in this context.
    /// Every fresh CREATE must issue strictly increasing
    /// nonzero IDs without wrap or reuse for that instance, including failures.
    /// Every backend must serialize fresh CREATE through bind synchronously in
    /// this owner's callback. Arbitrary existing exports cannot be enrolled.
    pub unsafe fn enroll_native_provider(
        &mut self,
        authority: Arc<dyn NativeProviderAuthority>,
        limits: NativeLimits,
    ) -> io::Result<()> {
        let mut provider = self
            .native_exports
            .lock()
            .map_err(|_| denied("native provider poisoned"))?;
        if provider.is_some() {
            return Err(denied("native provider already enrolled"));
        }
        *provider = Some(Provider {
            instance: None,
            authority,
            gate: Arc::new(Mutex::new(())),
            high_water: 0,
            limits,
        });
        Ok(())
    }
    pub(super) fn tracked_extent<P: TrackedProjectionBackend<B>>(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        grant: &GrantId,
        request: Request,
        backend: &mut P,
    ) -> io::Result<P::Projection> {
        self.record(lifecycle, grant)?;
        let Request {
            allocation,
            reference,
            extent,
            target,
            access,
        } = request;
        if self.records[&grant.id]
            .closing
            .contains_key(&(allocation, reference))
        {
            return Err(denied("exact projection reference closing or retired"));
        }
        let (limits, provider, gate) = {
            let enrolled = self
                .native_exports
                .lock()
                .map_err(|_| denied("native provider poisoned"))?;
            let enrolled = enrolled.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "native provider not enrolled")
            })?;
            (
                enrolled.limits,
                enrolled.authority.clone(),
                enrolled.gate.clone(),
            )
        };
        let _provider_serial = gate
            .lock()
            .map_err(|_| denied("native provider admission gate poisoned"))?;
        // Bounded active ledger, not a completion census. No native callback
        // or ticket escapes until every admission dimension has capacity.
        let mut total = 0usize;
        let mut bytes = 0u64;
        for r in self.records.values() {
            total = total
                .checked_add(r.native_holds.len())
                .ok_or_else(|| denied("native admission overflow"))?;
            for h in r.native_holds.values() {
                bytes = bytes
                    .checked_add(h.extent.capacity)
                    .ok_or_else(|| denied("native capacity overflow"))?;
            }
        }
        let current = &self.records[&grant.id];
        let references = current
            .native_holds
            .values()
            .filter(|h| h.allocation == allocation && h.reference == reference)
            .count();
        if total >= limits.total
            || current.native_holds.len() >= limits.per_grant
            || references >= limits.per_reference
            || !bytes
                .checked_add(extent.capacity)
                .is_some_and(|n| n <= limits.capacity_bytes)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "native projection admission saturated",
            ));
        }
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
                reference,
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
            provider,
            binding,
            allocation,
            reference,
            extent,
            target,
            access,
        };
        let guard = record.guard.clone();
        match backend.project_tracked(guard, ticket) {
            Ok(projection) => {
                let bound = record.native_holds[&id]
                    .state
                    .lock()
                    .map(|state| matches!(state.stage, Stage::Bound(_)))
                    .unwrap_or(false);
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
            Request {
                allocation: allocation.key.allocation,
                reference: ProjectionReference::Producer {
                    registration: producer.process.registration(),
                },
                extent,
                target: producer.process,
                access: ProjectionAccess::ReadWrite,
            },
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
            Request {
                allocation: consumer.allocation.key.allocation,
                reference: ProjectionReference::Consumer {
                    registration: recipient.process.registration(),
                    consumer: consumer.consumer.reference(),
                },
                extent,
                target: recipient.process,
                access: ProjectionAccess::ReadOnly,
            },
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
            || hold.reference != proof.ticket.reference
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
        record.bump_lineage()?;
        record.note_terminal(
            proof.ticket.allocation,
            proof.ticket.reference,
            Some((proof.identity.key.instance, proof.identity.key.export)),
        );
        record.native_holds.remove(&proof.ticket.id);
        Ok(())
    }
}
