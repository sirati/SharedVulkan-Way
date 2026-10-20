//! Fail-closed privileged service bootstrap and retained per-grant streams.
//!
//! This module is not wired to production allocation. The canonical owner must
//! enforce registry exclusions on EVERY allocation route before activation.
//! Epoch changes never prove old mappings dead and never release old bytes.
//! A ready instance refuses rebootstrap, even after peer restart. Recovery
//! currently requires fresh own tickets/lifecycles on BOTH sides, while all old
//! grants remain excluded. No automatic replay-counter reset is provided.
use crate::bar_registry::{DurableStore, EpochTicket, Registry};
use crate::service_auth::{Authenticator, Frame, ServiceEndpoint, VerifiedMessage};
use crate::service_bootstrap::{BootstrapBinding, BootstrapMessage, BootstrapPhase, DOMAIN};
use crate::service_control::{Binding, Message, Origin, Payload, Phase, Source};
use std::{
    io,
    sync::{Arc, Mutex, MutexGuard},
};
use wc_handle::macboot::{KEY_LEN, hmac_sha256};

fn denied(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, s)
}
fn opposite(o: Origin) -> Origin {
    match o {
        Origin::Windows => Origin::Linux,
        Origin::Linux => Origin::Windows,
    }
}

/// Selected by protected service configuration, never by the peer's frame.
#[derive(Clone, Copy)]
pub enum BootstrapRole {
    Initiator,
    Responder,
}
#[derive(PartialEq)]
enum Stage {
    Initial,
    HelloSent,
    ChallengeSent,
    Ready,
}

/// A retained stream with no public constructor or reset. All application
/// operations go through its owning Lifecycle, including after reconnects.
pub struct GrantChannel {
    auth: Authenticator,
    owner: Arc<()>,
}

struct ServiceState<S: DurableStore> {
    registry: Registry<S>,
    sessions: Vec<(String, u64)>,
    allocator_claimed: bool,
    next_grant: u64,
}
/// One protected daemon incarnation for one original physical backing. This
/// owns the durable registry and own service epoch ABOVE all installations and
/// sessions. Starting another per-binding handshake never issues another epoch.
pub struct ServiceContext<S: DurableStore, B: Send + Sync + 'static> {
    state: Mutex<ServiceState<S>>,
    ticket: EpochTicket,
    endpoint: ServiceEndpoint,
    source: Source,
    grantor: Origin,
    original_backing: Arc<B>,
}
impl<S: DurableStore, B: Send + Sync + 'static> ServiceContext<S, B> {
    /// Create exactly one canonical service context for the original backing.
    ///
    /// # Safety
    /// Endpoint must name this protected daemon's own exact native process.
    /// Source and registry must already be rooted to the actual retained full
    /// original backing/device. Every alias and allocator route must use this
    /// context, never a second store/context. Grantor must be the one protected
    /// source-wide canonical allocator origin, identical on both machines and
    /// immutable while this original backing lives. No session can choose it.
    /// No valid older snapshot may be restored while original bytes live.
    /// No session metadata is authority.
    pub unsafe fn start(
        mut registry: Registry<S>,
        endpoint: ServiceEndpoint,
        source: Source,
        grantor: Origin,
        original_backing: Arc<B>,
    ) -> io::Result<Arc<Self>> {
        let physical = registry.backing();
        if source.host_device != physical.host_device
            || source.host_inode != physical.host_inode
            || source.bar_bytes != physical.bar_bytes
        {
            return Err(denied(
                "service source differs from canonical registry backing",
            ));
        }
        endpoint.ensure_live()?;
        let ticket = registry.issue_next_service_epoch(endpoint.origin())?;
        Ok(Arc::new(Self {
            state: Mutex::new(ServiceState {
                registry,
                sessions: Vec::new(),
                allocator_claimed: false,
                next_grant: 1,
            }),
            ticket,
            endpoint,
            source,
            grantor,
            original_backing,
        }))
    }
    fn state(&self) -> io::Result<MutexGuard<'_, ServiceState<S>>> {
        self.state
            .lock()
            .map_err(|_| denied("canonical service state poisoned"))
    }
    pub(crate) fn live(&self) -> io::Result<()> {
        self.endpoint.ensure_live()?;
        if !self.state()?.registry.owns_current_ticket(&self.ticket) {
            return Err(denied("service local epoch is no longer current"));
        }
        Ok(())
    }
    /// Includes every old physical grant, regardless of installation/category.
    pub fn exclusions(&self) -> io::Result<Vec<(u64, u64)>> {
        self.live()?;
        Ok(self.state()?.registry.exclusions())
    }
    pub(crate) fn backing(&self) -> &Arc<B> {
        &self.original_backing
    }
    pub(crate) fn origin(&self) -> Origin {
        self.endpoint.origin()
    }
    pub(crate) fn grantor(&self) -> Origin { self.grantor }
    pub(crate) fn source(&self) -> &Source {
        &self.source
    }
    pub(crate) fn claim_allocator(&self) -> io::Result<Vec<(u64, u64)>> {
        self.live()?;
        let mut state = self.state()?;
        if state.allocator_claimed {
            return Err(denied("canonical grant owner already claimed"));
        }
        state.allocator_claimed = true;
        Ok(state.registry.exclusions())
    }
    pub(crate) fn next_grant(&self) -> io::Result<u64> {
        self.live()?;
        let mut state = self.state()?;
        let id = state.next_grant;
        state.next_grant = id
            .checked_add(1)
            .ok_or_else(|| denied("canonical grant IDs exhausted"))?;
        Ok(id)
    }
}

/// One independently challenged, pre-enrolled installation/session binding.
/// Keep it and its grant streams across reconnects. The shared context issues
/// the daemon epoch only once and retains every sibling session's streams.
pub struct Lifecycle<S: DurableStore, B: Send + Sync + 'static> {
    context: Arc<ServiceContext<S, B>>,
    binding: BootstrapBinding,
    key: [u8; KEY_LEN],
    role: BootstrapRole,
    stage: Stage,
    local_challenge: [u8; 32],
    peer: Option<(u64, [u8; 32])>,
    channel_owner: Arc<()>,
}
impl<S: DurableStore, B: Send + Sync + 'static> Lifecycle<S, B> {
    /// Start one enrolled binding without changing the daemon service epoch.
    ///
    /// # Safety
    /// Key must be the existing protected key for exactly installation. The
    /// complete binding and SID-to-UID policy must be pre-enrolled by the
    /// canonical source owner, never learned from Hello/client input. The
    /// same installation/session may be started only once in this context;
    /// reconnect must retain its existing lifecycle and replay counters.
    pub unsafe fn start(
        context: Arc<ServiceContext<S, B>>,
        installation: &str,
        key: [u8; KEY_LEN],
        binding: BootstrapBinding,
        role: BootstrapRole,
    ) -> io::Result<Self> {
        Self::start_with_challenge(
            context,
            (installation, key),
            binding,
            role,
            random_challenge()?,
        )
    }
    fn start_with_challenge(
        context: Arc<ServiceContext<S, B>>,
        installation_key: (&str, [u8; KEY_LEN]),
        binding: BootstrapBinding,
        role: BootstrapRole,
        local_challenge: [u8; 32],
    ) -> io::Result<Self> {
        let (installation, key) = installation_key;
        binding.validate()?;
        if installation != binding.installation
            || binding.source != context.source
            || binding.roles.grantor != context.grantor
            || local_challenge == [0; 32]
        {
            return Err(denied(
                "invalid rooted bootstrap installation/source/challenge",
            ));
        }
        context.live()?;
        let session = (binding.installation.clone(), binding.session);
        {
            let mut state = context.state()?;
            if state.sessions.contains(&session) {
                return Err(denied(
                    "session lifecycle already started; retain replay state",
                ));
            }
            state.sessions.push(session);
        }
        Ok(Self {
            context,
            binding,
            key,
            role,
            stage: Stage::Initial,
            local_challenge,
            peer: None,
            channel_owner: Arc::new(()),
        })
    }
    fn live(&self) -> io::Result<()> {
        self.context.live()
    }
    pub fn exclusions(&self) -> io::Result<Vec<(u64, u64)>> {
        self.context.exclusions()
    }
    pub fn ready(&self) -> io::Result<()> {
        self.live()?;
        let peer = self
            .peer
            .ok_or_else(|| denied("service pair not confirmed"))?
            .0;
        if self.stage != Stage::Ready
            || !self
                .context
                .state()?
                .registry
                .confirmed_pair(&self.context.ticket, peer)
        {
            return Err(denied("service pair not durably confirmed"));
        }
        Ok(())
    }
    pub(crate) fn belongs_to(&self, context: &Arc<ServiceContext<S, B>>) -> bool {
        Arc::ptr_eq(&self.context, context)
    }
    pub(crate) fn grant_binding(
        &self,
        grant: u64,
        offset: u64,
        length: u64,
    ) -> io::Result<Binding> {
        self.ready()?;
        let local = self.context.ticket.value();
        let peer = self.peer.unwrap().0;
        let (windows_run, linux_run) = match self.context.endpoint.origin() {
            Origin::Windows => (local, peer),
            Origin::Linux => (peer, local),
        };
        Ok(Binding {
            installation: self.binding.installation.clone(),
            source: self.binding.source.clone(),
            roles: self.binding.roles,
            windows_run,
            linux_run,
            grant,
            session: self.binding.session,
            grant_offset: offset,
            grant_length: length,
            windows_sid: self.binding.windows_sid.clone(),
            linux_uid: self.binding.linux_uid,
        })
    }
    /// Authenticate a newly offered grant before the peer mirror can reserve
    /// it. This proves canonical service origin, not historical apply authority.
    pub(crate) fn incoming_grant(&self, frame: &Frame) -> io::Result<Binding> {
        self.ready()?;
        if self.context.endpoint.origin() == self.context.grantor {
            return Err(denied("canonical grantor cannot accept peer allocation authority"));
        }
        let mut signed = DOMAIN.to_vec(); signed.extend_from_slice(frame.canonical());
        let expected = hmac_sha256(&self.key, &signed);
        if expected.iter().zip(frame.mac()).fold(0u8,|v,(a,b)|v|(a^b)) != 0 {
            return Err(denied("invalid canonical grant authentication"));
        }
        let message = Message::decode(frame.canonical())?;
        let b = &message.binding;
        if message.origin != self.context.grantor || message.phase != Phase::Grant || message.sequence != 1
            || *b != self.grant_binding(b.grant,b.grant_offset,b.grant_length)? {
            return Err(denied("offered grant differs from enrolled canonical source/pair"));
        }
        Ok(message.binding)
    }
    /// The initiator may emit Hello exactly once for this ticket.
    pub fn hello(&mut self) -> io::Result<Frame> {
        self.live()?;
        if !matches!(self.role, BootstrapRole::Initiator) || self.stage != Stage::Initial {
            return Err(denied("bootstrap Hello cannot restart this lifecycle"));
        }
        let frame = self.seal_bootstrap(BootstrapPhase::Hello)?;
        self.stage = Stage::HelloSent;
        Ok(frame)
    }
    /// Respond to the next exact phase. Invalid frames do not advance state.
    /// A successful initiator returns Confirm only after durable enrollment;
    /// the responder becomes ready only after authenticating that Confirm.
    pub fn receive(&mut self, frame: &Frame) -> io::Result<Option<Frame>> {
        self.live()?;
        let mut bytes = DOMAIN.to_vec();
        bytes.extend_from_slice(frame.canonical());
        let expected = hmac_sha256(&self.key, &bytes);
        if expected
            .iter()
            .zip(frame.mac())
            .fold(0u8, |n, (a, b)| n | (a ^ b))
            != 0
        {
            return Err(denied("invalid bootstrap authentication"));
        }
        let m = BootstrapMessage::decode(frame.canonical())?;
        if m.binding != self.binding || m.origin != opposite(self.context.endpoint.origin()) {
            return Err(denied("bootstrap enrolled source or direction differs"));
        }
        let (local_epoch, local_challenge, peer_epoch, peer_challenge) =
            match self.context.endpoint.origin() {
                Origin::Windows => (
                    m.windows_epoch,
                    m.windows_challenge,
                    m.linux_epoch,
                    m.linux_challenge,
                ),
                Origin::Linux => (
                    m.linux_epoch,
                    m.linux_challenge,
                    m.windows_epoch,
                    m.windows_challenge,
                ),
            };
        match (self.role, &self.stage, m.phase) {
            (BootstrapRole::Responder, Stage::Initial, BootstrapPhase::Hello) => {
                // Hello contains no local fields. The responder's challenge is
                // already fixed by the OS RNG, never chosen by this message.
                if peer_challenge == self.local_challenge {
                    return Err(denied("bootstrap challenges coincide"));
                }
                self.peer = Some((peer_epoch, peer_challenge));
                let result = self.seal_bootstrap(BootstrapPhase::Challenge)?;
                self.stage = Stage::ChallengeSent;
                Ok(Some(result))
            }
            (BootstrapRole::Initiator, Stage::HelloSent, BootstrapPhase::Challenge) => {
                if local_epoch != self.context.ticket.value()
                    || local_challenge != self.local_challenge
                {
                    return Err(denied(
                        "bootstrap challenge does not answer fresh local Hello",
                    ));
                }
                self.peer = Some((peer_epoch, peer_challenge));
                // Build first so codec errors cannot leave readiness without a
                // corresponding Confirm. Persistence still precedes exposure.
                let result = self.seal_bootstrap(BootstrapPhase::Confirm)?;
                self.enroll_peer(peer_epoch)?;
                Ok(Some(result))
            }
            (BootstrapRole::Responder, Stage::ChallengeSent, BootstrapPhase::Confirm) => {
                if local_epoch != self.context.ticket.value()
                    || local_challenge != self.local_challenge
                    || self.peer != Some((peer_epoch, peer_challenge))
                {
                    return Err(denied(
                        "bootstrap Confirm differs from both live challenges",
                    ));
                }
                self.enroll_peer(peer_epoch)?;
                Ok(None)
            }
            _ => Err(denied("bootstrap phase replay or unexpected transition")),
        }
    }
    fn enroll_peer(&mut self, peer_epoch: u64) -> io::Result<()> {
        self.live()?;
        // SAFETY: only the two exact mutually challenged branches above reach
        // here. Their MAC, full pre-enrollment and fresh local ticket match.
        {
            let mut state = self.context.state()?;
            if !state
                .registry
                .confirmed_pair(&self.context.ticket, peer_epoch)
            {
                unsafe {
                    state
                        .registry
                        .confirm_peer_epoch(&self.context.ticket, peer_epoch)?;
                }
            }
        }
        self.stage = Stage::Ready;
        self.ready()
    }
    fn seal_bootstrap(&self, phase: BootstrapPhase) -> io::Result<Frame> {
        self.live()?;
        let (peer_epoch, peer_challenge) = self.peer.unwrap_or((0, [0; 32]));
        let (windows_epoch, windows_challenge, linux_epoch, linux_challenge) =
            match self.context.endpoint.origin() {
                Origin::Windows => (
                    self.context.ticket.value(),
                    self.local_challenge,
                    peer_epoch,
                    peer_challenge,
                ),
                Origin::Linux => (
                    peer_epoch,
                    peer_challenge,
                    self.context.ticket.value(),
                    self.local_challenge,
                ),
            };
        let canonical = BootstrapMessage {
            binding: self.binding.clone(),
            origin: self.context.endpoint.origin(),
            phase,
            windows_epoch,
            linux_epoch,
            windows_challenge,
            linux_challenge,
        }
        .encode()?;
        let mut signed = DOMAIN.to_vec();
        signed.extend_from_slice(&canonical);
        Frame::from_wire(&canonical, &hmac_sha256(&self.key, &signed))
    }
    /// Reserve whole original grant durably before constructing its stream.
    ///
    /// # Safety
    /// The caller is the canonical original allocator. `binding` must describe
    /// its actual retained grant and physical geometry including header base;
    /// `retained_grant` must prevent that exact grant's release/reuse. Every
    /// original allocation/alias route must honor all persistent exclusions.
    /// A wire grant, shared-directory counter or placeholder Arc is no proof.
    pub unsafe fn reserve_channel<G: Send + Sync + 'static>(
        &mut self,
        binding: Binding,
        retained_grant: Arc<G>,
    ) -> io::Result<GrantChannel> {
        self.ready()?;
        if BootstrapBinding::from(&binding) != self.binding {
            return Err(denied("grant differs from enrolled bootstrap source"));
        }
        // SAFETY: canonical allocator and retained grant contract is required
        // of this method's caller; the registry verifies epochs and overlap.
        let reservation = unsafe {
            self.context
                .state()?
                .registry
                .reserve(&self.context.ticket, binding)?
        };
        let auth = unsafe {
            Authenticator::new(
                self.context.endpoint.share(),
                &self.binding.installation,
                self.key,
                reservation.binding().clone(),
                retained_grant,
            )?
        };
        Ok(GrantChannel {
            auth,
            owner: self.channel_owner.clone(),
        })
    }
    pub(crate) fn channel_ready(&self, channel: &GrantChannel) -> io::Result<()> {
        self.ready()?;
        if !Arc::ptr_eq(&self.channel_owner, &channel.owner) {
            return Err(denied(
                "grant stream belongs to a different service lifecycle",
            ));
        }
        Ok(())
    }
    pub fn seal(
        &self,
        channel: &mut GrantChannel,
        phase: Phase,
        payload: Payload,
    ) -> io::Result<Frame> {
        self.channel_ready(channel)?;
        channel.auth.seal(phase, payload)
    }
    /// Returns historical authentication evidence only. Before delayed use,
    /// the canonical resource owner must recheck this lifecycle's current
    /// readiness/channel and exact pending resource state at mutation time.
    /// This foundation provides no allocator apply/completion operation.
    pub fn verify(&self, channel: &mut GrantChannel, frame: &Frame) -> io::Result<VerifiedMessage> {
        self.channel_ready(channel)?;
        channel.auth.verify(frame)
    }
}

// No clock, PID, token helper or weak fallback may create these challenges.
fn random_challenge() -> io::Result<[u8; 32]> {
    let mut bytes = [0; 32];
    #[cfg(unix)]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    }
    #[cfg(windows)]
    {
        #[link(name = "bcrypt")]
        unsafe extern "system" {
            fn BCryptGenRandom(
                algorithm: *mut std::ffi::c_void,
                buffer: *mut u8,
                len: u32,
                flags: u32,
            ) -> i32;
        }
        // SAFETY: system RNG writes exactly the provided live 32-byte buffer.
        if unsafe { BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), 32, 2) } != 0 {
            return Err(io::Error::other("system bootstrap RNG failed"));
        }
    }
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no system bootstrap RNG",
    ));
    if bytes == [0; 32] {
        return Err(io::Error::other(
            "system bootstrap RNG returned zero challenge",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bar_registry::PhysicalBacking,
        core::User,
        service_auth::{ServicePolicy, ServiceProcess},
    };
    use std::sync::{
        Mutex,
        atomic::{AtomicU8, Ordering},
    };
    const KEY: [u8; 32] = [0x55; 32];
    #[derive(Clone, Default)]
    struct MemoryStore {
        data: Arc<Mutex<Option<Vec<u8>>>>,
        fail: Arc<AtomicU8>,
    }
    // SAFETY: isolated test-only exclusive in-memory store; not production durability.
    unsafe impl DurableStore for MemoryStore {
        fn load(&mut self) -> io::Result<Vec<u8>> {
            self.data
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| io::ErrorKind::NotFound.into())
        }
        fn replace_synced(&mut self, bytes: &[u8]) -> io::Result<()> {
            let fail = self.fail.load(Ordering::SeqCst);
            if fail == 1 {
                return Err(io::Error::other("before write"));
            }
            *self.data.lock().unwrap() = Some(bytes.to_vec());
            if fail == 2 {
                return Err(io::Error::other("after uncertain write"));
            }
            Ok(())
        }
    }
    struct Process(AtomicU8);
    // SAFETY: deterministic native-query fixture only.
    unsafe impl ServiceProcess for Process {
        fn exited(&self) -> io::Result<bool> {
            match self.0.load(Ordering::SeqCst) {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(io::Error::other("kernel query unavailable")),
            }
        }
    }
    type Life = Lifecycle<MemoryStore, ()>;
    fn binding() -> BootstrapBinding {
        BootstrapBinding::from(&crate::service_control::fixture().binding)
    }
    fn backing() -> PhysicalBacking {
        let s = binding().source;
        PhysicalBacking {
            host_device: s.host_device,
            host_inode: s.host_inode,
            bar_bytes: s.bar_bytes,
        }
    }
    fn endpoint(origin: Origin, process: Arc<Process>) -> ServiceEndpoint {
        let (p, u) = match origin {
            Origin::Windows => (ServicePolicy::windows_system(), User::sid("S-1-5-18")),
            Origin::Linux => (ServicePolicy::linux_root(), User::uid(0)),
        };
        // SAFETY: synthetic service authority only, never a native identity claim.
        unsafe { ServiceEndpoint::from_current_service(&p, u, process).unwrap() }
    }
    fn initial(origin: Origin) -> (Life, MemoryStore, Arc<Process>) {
        let store = MemoryStore::default();
        // SAFETY: test's independently empty store and simulated backing.
        let registry =
            unsafe { Registry::initialize_fresh(store.clone(), backing(), origin).unwrap() };
        make(origin, registry, store, 1)
    }
    fn make(
        origin: Origin,
        registry: Registry<MemoryStore>,
        store: MemoryStore,
        challenge: u8,
    ) -> (Life, MemoryStore, Arc<Process>) {
        let process = Arc::new(Process(AtomicU8::new(0)));
        let role = match origin {
            Origin::Windows => BootstrapRole::Initiator,
            Origin::Linux => BootstrapRole::Responder,
        };
        let nonce = match origin {
            Origin::Windows => [challenge; 32],
            Origin::Linux => [challenge + 100; 32],
        };
        let context = unsafe {
            ServiceContext::start(
                registry,
                endpoint(origin, process.clone()),
                binding().source,
                binding().roles.grantor,
                Arc::new(()),
            )
            .unwrap()
        };
        let life =
            Lifecycle::start_with_challenge(context, ("test-install", KEY), binding(), role, nonce)
                .unwrap();
        (life, store, process)
    }
    fn pair() -> (Life, Life) {
        (initial(Origin::Windows).0, initial(Origin::Linux).0)
    }
    fn handshake(w: &mut Life, l: &mut Life) {
        let hello = w.hello().unwrap();
        let challenge = l.receive(&hello).unwrap().unwrap();
        assert!(l.ready().is_err());
        let confirm = w.receive(&challenge).unwrap().unwrap();
        w.ready().unwrap();
        assert!(l.receive(&confirm).unwrap().is_none());
        l.ready().unwrap();
    }
    fn grant(w: &Life) -> Binding {
        let mut b = crate::service_control::fixture().binding;
        b.windows_run = w.context.ticket.value();
        b.linux_run = w.peer.unwrap().0;
        b
    }
    fn signed(m: &BootstrapMessage, key: &[u8]) -> Frame {
        let bytes = m.encode().unwrap();
        let mut input = DOMAIN.to_vec();
        input.extend_from_slice(&bytes);
        Frame::from_wire(&bytes, &hmac_sha256(key, &input)).unwrap()
    }
    #[test]
    fn mutual_handshake_gates_persistence_and_stream_replay() {
        let (mut w, mut l) = pair();
        assert!(w.ready().is_err());
        assert!(l.hello().is_err());
        handshake(&mut w, &mut l);
        assert!(w.hello().is_err());
        let b = grant(&w);
        // SAFETY: simulated canonical original grants for state-machine testing.
        let mut wc = unsafe { w.reserve_channel(b.clone(), Arc::new(())).unwrap() };
        let mut lc = unsafe { l.reserve_channel(b, Arc::new(())).unwrap() };
        let payload = crate::service_control::fixture().payload;
        let frame = w.seal(&mut wc, Phase::Allocate, payload).unwrap();
        assert_eq!(l.verify(&mut lc, &frame).unwrap().message().sequence, 1);
        assert!(l.verify(&mut lc, &frame).is_err());
        // Reconnect retains these same objects and their sequence state.
        // A late/replayed peer Hello cannot reset this ready lifecycle,
        // stream replay counters, or its durable physical exclusions.
        let exclusions = l.exclusions().unwrap();
        let late_hello = BootstrapMessage {
            binding: binding(),
            origin: Origin::Windows,
            phase: BootstrapPhase::Hello,
            windows_epoch: w.context.ticket.value(),
            linux_epoch: 0,
            windows_challenge: [9; 32],
            linux_challenge: [0; 32],
        };
        assert!(l.receive(&signed(&late_hello, &KEY)).is_err());
        l.ready().unwrap();
        assert_eq!(l.exclusions().unwrap(), exclusions);
        assert!(l.verify(&mut lc, &frame).is_err());
        let frame2 = w.seal(&mut wc, Phase::Allocate, payload).unwrap();
        assert_eq!(l.verify(&mut lc, &frame2).unwrap().message().sequence, 2);
        let (mut other, mut other_l) = pair();
        handshake(&mut other, &mut other_l);
        assert!(other.seal(&mut wc, Phase::Allocate, payload).is_err());
    }
    #[test]
    fn old_transcript_cannot_bootstrap_a_fresh_epoch() {
        let (mut w, ws, _) = initial(Origin::Windows);
        let (mut l, ls, _) = initial(Origin::Linux);
        let h = w.hello().unwrap();
        let c = l.receive(&h).unwrap().unwrap();
        let f = w.receive(&c).unwrap().unwrap();
        l.receive(&f).unwrap();
        drop(w);
        drop(l);
        let wr = Registry::open(ws.clone(), backing(), Origin::Windows).unwrap();
        let lr = Registry::open(ls.clone(), backing(), Origin::Linux).unwrap();
        let (mut w, _, _) = make(Origin::Windows, wr, ws, 2);
        let (mut l, _, _) = make(Origin::Linux, lr, ls, 2);
        assert!(w.ready().is_err());
        assert!(l.ready().is_err());
        assert!(w.receive(&c).is_err());
        w.hello().unwrap();
        assert!(w.receive(&c).is_err());
        // A replayed Hello gets only a fresh Challenge, never readiness.
        assert!(l.receive(&h).unwrap().is_some());
        assert!(l.receive(&f).is_err());
        assert!(l.ready().is_err());
    }
    #[test]
    fn authenticated_but_wrong_binding_direction_and_challenges_are_refused() {
        let (mut w, mut l) = pair();
        let h = w.hello().unwrap();
        let mut m = BootstrapMessage::decode(h.canonical()).unwrap();
        for field in 0..3 {
            let mut changed = m.clone();
            match field {
                0 => changed.binding.source.host_inode += 1,
                1 => changed.binding.installation = "other".into(),
                _ => changed.binding.source.bar_run += 1,
            }
            assert!(l.receive(&signed(&changed, &KEY)).is_err());
        }
        assert!(l.receive(&signed(&m, &[2; 32])).is_err());
        assert!(w.receive(&h).is_err());
        let c = l.receive(&h).unwrap().unwrap();
        assert!(l.receive(&h).is_err());
        m = BootstrapMessage::decode(c.canonical()).unwrap();
        m.windows_challenge = [3; 32];
        assert!(w.receive(&signed(&m, &KEY)).is_err());
        m = BootstrapMessage::decode(c.canonical()).unwrap();
        m.windows_epoch += 1;
        assert!(w.receive(&signed(&m, &KEY)).is_err());
        let f = w.receive(&c).unwrap().unwrap();
        assert!(w.receive(&c).is_err());
        m = BootstrapMessage::decode(f.canonical()).unwrap();
        m.linux_challenge = [5; 32];
        assert!(l.receive(&signed(&m, &KEY)).is_err());
        l.receive(&f).unwrap();
        assert!(l.receive(&f).is_err());
    }
    #[test]
    fn uncertain_enrollment_write_never_exposes_confirm_or_ready() {
        for failure in [1, 2] {
            let (mut w, ws, _) = initial(Origin::Windows);
            let (mut l, _, _) = initial(Origin::Linux);
            let h = w.hello().unwrap();
            let c = l.receive(&h).unwrap().unwrap();
            ws.fail.store(failure, Ordering::SeqCst);
            assert!(w.receive(&c).is_err());
            assert!(w.ready().is_err());
            ws.fail.store(0, Ordering::SeqCst);
            assert!(w.receive(&c).is_err());
            assert!(l.ready().is_err());
        }
    }
    #[test]
    fn new_epoch_invalidates_live_channels_and_keeps_all_old_exclusions() {
        let (mut w, ws, _) = initial(Origin::Windows);
        let (mut l, _, _) = initial(Origin::Linux);
        handshake(&mut w, &mut l);
        let b = grant(&w);
        let mut channel = unsafe { w.reserve_channel(b.clone(), Arc::new(())).unwrap() };
        let expected = w.exclusions().unwrap();
        assert_eq!(expected, vec![(b.grant_offset, b.grant_length)]);
        // In-process ownership prevents outside issuance. Exercise the internal
        // defense too, as if a future owner adds another lifecycle operation.
        w.context
            .state()
            .unwrap()
            .registry
            .issue_next_service_epoch(Origin::Windows)
            .unwrap();
        assert!(w.ready().is_err());
        assert!(
            w.seal(
                &mut channel,
                Phase::Allocate,
                crate::service_control::fixture().payload
            )
            .is_err()
        );
        drop(w);
        let registry = Registry::open(ws.clone(), backing(), Origin::Windows).unwrap();
        let (w, _, _) = make(Origin::Windows, registry, ws, 3);
        assert!(w.ready().is_err());
        assert_eq!(w.exclusions().unwrap(), expected);
    }
    #[test]
    fn process_death_and_query_failure_revoke_every_shared_stream_permanently() {
        for event in [1, 2] {
            let (mut w, _, process) = initial(Origin::Windows);
            let (mut l, _, _) = initial(Origin::Linux);
            handshake(&mut w, &mut l);
            let mut channel = unsafe { w.reserve_channel(grant(&w), Arc::new(())).unwrap() };
            process.0.store(event, Ordering::SeqCst);
            assert!(w.ready().is_err());
            process.0.store(0, Ordering::SeqCst);
            assert!(w.ready().is_err());
            // The stream's own endpoint shares revocation, too.
            assert!(
                channel
                    .auth
                    .seal(Phase::Allocate, crate::service_control::fixture().payload)
                    .is_err()
            );
        }
    }
    #[test]
    fn sibling_installations_share_one_epoch_without_resetting_live_streams() {
        let (mut w, mut l) = pair();
        handshake(&mut w, &mut l);
        let epoch = w.context.ticket.value();
        let mut channel = unsafe { w.reserve_channel(grant(&w), Arc::new(())).unwrap() };
        let payload = crate::service_control::fixture().payload;
        assert_eq!(
            crate::service_control::Message::decode(
                w.seal(&mut channel, Phase::Allocate, payload)
                    .unwrap()
                    .canonical()
            )
            .unwrap()
            .sequence,
            1
        );
        let mut b = binding();
        b.installation = "second-install".into();
        b.session += 1;
        let mut sibling_w = Lifecycle::start_with_challenge(
            w.context.clone(),
            ("second-install", [7; 32]),
            b.clone(),
            BootstrapRole::Initiator,
            [10; 32],
        )
        .unwrap();
        let mut sibling_l = Lifecycle::start_with_challenge(
            l.context.clone(),
            ("second-install", [7; 32]),
            b,
            BootstrapRole::Responder,
            [11; 32],
        )
        .unwrap();
        assert!(sibling_w.ready().is_err());
        handshake(&mut sibling_w, &mut sibling_l);
        assert_eq!(sibling_w.context.ticket.value(), epoch);
        w.ready().unwrap();
        l.ready().unwrap();
        assert_eq!(
            crate::service_control::Message::decode(
                w.seal(&mut channel, Phase::Allocate, payload)
                    .unwrap()
                    .canonical()
            )
            .unwrap()
            .sequence,
            2
        );
        assert!(
            Lifecycle::start_with_challenge(
                w.context.clone(),
                ("test-install", KEY),
                binding(),
                BootstrapRole::Initiator,
                [12; 32]
            )
            .is_err()
        );
        assert!(
            sibling_w
                .seal(&mut channel, Phase::Allocate, payload)
                .is_err()
        );
        let mut wrong_grantor = binding();
        wrong_grantor.installation = "third-install".into();
        wrong_grantor.session += 2;
        wrong_grantor.roles.grantor = opposite(w.context.grantor);
        assert!(Lifecycle::start_with_challenge(w.context.clone(), ("third-install", KEY),
            wrong_grantor, BootstrapRole::Initiator, [13;32]).is_err());
    }
    #[test]
    fn os_challenges_are_nonzero_and_independent() {
        let a = random_challenge().unwrap();
        let b = random_challenge().unwrap();
        assert_ne!(a, [0; 32]);
        assert_ne!(b, [0; 32]);
        assert_ne!(a, b);
    }
}
