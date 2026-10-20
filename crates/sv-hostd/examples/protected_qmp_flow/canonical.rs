//! Native Linux canonical flow for a newly-created disposable QMP fixture.
//!
//! Local root endpoint, original backing, registry, allocation and projections
//! are real. The Windows bootstrap transcript is explicitly SIMULATED with a
//! freshly generated fixture key. This proves no remote OS/Windows identity.
use std::{
    io::{self, Read},
    path::Path,
    sync::Arc,
};
use sv_ram::linux_payload::{
    SealedBacking,
    protected::{ProtectedBackend, ProtectedProjection},
};
use wc_broker::{
    User,
    bar_registry::{Registry, unix::DirectoryStore},
    service_auth::{Frame, ServicePolicy},
    service_bootstrap::{BootstrapBinding, BootstrapMessage, BootstrapPhase, DOMAIN},
    service_control::{Origin, Roles, Source},
    service_grants::{
        AllocationId, GrantId, GrantKind, OriginalBacking, ProjectionAccess, ProtectedGrants,
        RegisteredProcess, Segment,
        native::{NativeLimits, RetirementProof},
    },
    service_lifecycle::{BootstrapRole, Lifecycle, ServiceContext},
};
use wc_handle::macboot::hmac_sha256;

type Context = ServiceContext<DirectoryStore, SealedBacking>;
/// Keep this alive through all projection/monitor observations. Kernel export
/// retirement is not canonical grant release and this owner never reclaims it.
pub struct FixtureOwner {
    _context: Arc<Context>,
    _lifecycle: Lifecycle<DirectoryStore, SealedBacking>,
    _grants: ProtectedGrants<DirectoryStore, SealedBacking>,
    grant: GrantId,
    producer: RegisteredProcess,
    allocation: AllocationId,
}
impl FixtureOwner {
    pub fn complete(&mut self, proof: RetirementProof) -> io::Result<()> {
        self._grants
            .complete_projection(&self._lifecycle, &self.grant, proof)
    }
    pub fn project_producer(
        &mut self,
        backend: &mut ProtectedBackend,
    ) -> io::Result<ProtectedProjection> {
        self._grants.project_producer_tracked(
            &self._lifecycle,
            &self.producer,
            &self.allocation,
            backend,
        )
    }
    pub fn assert_producer_pressure(&mut self, backend: &mut ProtectedBackend) -> io::Result<()> {
        match self.project_producer(backend) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            _ => Err(invalid("active producer exceeded native admission limit")),
        }
    }
    /// Native export retirement does not release canonical physical capacity.
    pub fn exclusions(&self) -> io::Result<Vec<(u64, u64)>> {
        self._context.exclusions()
    }
}
pub struct Flow {
    pub producer: ProtectedProjection,
    pub consumer: ProtectedProjection,
    pub owner: FixtureOwner,
    pub physical_offset: u64,
    pub capacity: u64,
    pub durable_exclusions: Vec<(u64, u64)>,
}
fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
fn random() -> io::Result<[u8; 32]> {
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    if bytes == [0; 32] {
        return Err(invalid("fixture RNG produced zero"));
    }
    Ok(bytes)
}
fn peer_frame(message: &BootstrapMessage, key: &[u8; 32]) -> io::Result<Frame> {
    let canonical = message.encode()?;
    let mut signed = DOMAIN.to_vec();
    signed.extend_from_slice(&canonical);
    Frame::from_wire(&canonical, &hmac_sha256(key, &signed))
}
/// # Safety
/// Launcher must have proven `backing` is the freshly-created original sealed
/// memfd used by this exact disposable QEMU device and that no old grants or
/// clients exist. `state_dir` is newly provisioned root0700 through protected
/// ancestors, never a reset alias for an old physical object. Backend must be
/// the reviewed private overlay with real native RO/RW max-rights proof.
/// This helper writes ONLY its new disposable grant, never family/host memory.
pub unsafe fn run(
    backing: Arc<SealedBacking>,
    backend: &mut ProtectedBackend,
    source: Source,
    state_dir: &Path,
) -> io::Result<Flow> {
    let geometry = backing.geometry();
    let physical = geometry.physical;
    if physical.host_device != source.host_device
        || physical.host_inode != source.host_inode
        || physical.bar_bytes != source.bar_bytes
        || geometry
            .arena_base
            .checked_add(16384)
            .is_none_or(|n| n > physical.bar_bytes)
    {
        return Err(invalid("fixture full original geometry differs"));
    }
    // Own native root endpoint uses backend's exact retained current-service
    // process. No PID reopening, fake Windows endpoint or remote watch exists.
    let endpoint = backend.context_endpoint(&ServicePolicy::linux_root())?;
    let store = unsafe { DirectoryStore::open(state_dir)? };
    let registry = unsafe { Registry::initialize_fresh(store, physical, Origin::Linux)? };
    let context = unsafe {
        ServiceContext::start(registry, endpoint, source.clone(), Origin::Linux, backing)?
    };
    let binding = BootstrapBinding {
        installation: "qmp-native-fixture".into(),
        source,
        roles: Roles {
            grantor: Origin::Linux,
            producer: Origin::Linux,
            consumer: Origin::Linux,
        },
        session: 1,
        windows_sid: "S-1-5-18".into(),
        linux_uid: 0,
    };
    let key = random()?;
    let mut lifecycle = unsafe {
        Lifecycle::start(
            context.clone(),
            "qmp-native-fixture",
            key,
            binding.clone(),
            BootstrapRole::Responder,
        )?
    };
    // Clearly simulated remote service transcript. Only the Linux service
    // endpoint above is a native OS authority claim in this fixture.
    let mut peer = BootstrapMessage {
        binding,
        origin: Origin::Windows,
        phase: BootstrapPhase::Hello,
        windows_epoch: 1,
        linux_epoch: 0,
        windows_challenge: random()?,
        linux_challenge: [0; 32],
    };
    let challenge = lifecycle
        .receive(&peer_frame(&peer, &key)?)?
        .ok_or_else(|| invalid("fixture expected Challenge"))?;
    peer = BootstrapMessage::decode(challenge.canonical())?;
    peer.origin = Origin::Windows;
    peer.phase = BootstrapPhase::Confirm;
    if lifecycle.receive(&peer_frame(&peer, &key)?)?.is_some() {
        return Err(invalid("fixture Confirm produced unexpected response"));
    }
    lifecycle.ready()?;
    let mut grants = unsafe {
        ProtectedGrants::start(
            context.clone(),
            vec![Segment {
                kind: GrantKind::Pixel,
                offset: geometry.arena_base,
                length: physical.bar_bytes - geometry.arena_base,
            }],
        )?
    };
    // Two simultaneous full-capacity exports, one per exact reference. Repeated
    // completed exports must return this capacity without releasing the grant.
    unsafe {
        grants.enroll_native_provider(
            backend.provider_authority()?,
            NativeLimits::new(2, 2, 1, 16384)?,
        )?;
    }
    let grant = grants.grant(&mut lifecycle, GrantKind::Pixel, 16384)?;
    let durable_exclusions = context.exclusions()?;
    if durable_exclusions != vec![(geometry.arena_base, 16384)] {
        return Err(invalid(
            "fixture grant was not durably reserved before native projection",
        ));
    }
    let producer =
        unsafe { grants.register_producer(&lifecycle, &grant, User::uid(0), backend.target())? };
    let consumer =
        unsafe { grants.register_consumer(&lifecycle, &grant, User::uid(0), backend.target())? };
    let (allocation, extent) = grants.allocate(&lifecycle, &producer, 64, 8192)?;
    if extent.offset != geometry.arena_base || extent.capacity != 8192 {
        return Err(invalid("fixture physical translation was not exact"));
    }
    let producer_projection =
        grants.project_producer_tracked(&lifecycle, &producer, &allocation, backend)?;
    let pending = grants.prepare(&lifecycle, &producer, &allocation, &consumer)?;
    let (accepted, _) = grants.accept(&lifecycle, &consumer, &pending)?;
    let consumer_projection =
        grants.project_consumer_tracked(&lifecycle, &consumer, &accepted, backend)?;
    if producer_projection.access() != ProjectionAccess::ReadWrite
        || consumer_projection.access() != ProjectionAccess::ReadOnly
    {
        return Err(invalid("fixture immutable projection rights differ"));
    }
    Ok(Flow {
        producer: producer_projection,
        consumer: consumer_projection,
        owner: FixtureOwner {
            _context: context,
            _lifecycle: lifecycle,
            _grants: grants,
            grant,
            producer,
            allocation,
        },
        physical_offset: extent.offset,
        capacity: extent.capacity,
        durable_exclusions,
    })
}
