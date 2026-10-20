//! Service-only authentication for cross-VM control, not a local-client token.
//!
//! The existing installation key remains inside privileged services. No public
//! boot-signing request reaches this API. An authenticated message establishes
//! its exact service/binding/sequence, never that a resource has completed.
use crate::core::User;
use crate::service_control::{Binding, DOMAIN, MAX_BYTES, Message, Origin, Payload, Phase};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use wc_handle::macboot::{KEY_LEN, hmac_sha256};

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
fn opposite(origin: Origin) -> Origin {
    match origin {
        Origin::Windows => Origin::Linux,
        Origin::Linux => Origin::Windows,
    }
}

/// Root-configured local service policy. This is never read from a message.
pub struct ServicePolicy {
    origin: Origin,
    user: User,
}
impl ServicePolicy {
    pub fn windows_system() -> Self {
        Self {
            origin: Origin::Windows,
            user: User::sid("S-1-5-18"),
        }
    }
    pub fn linux_root() -> Self {
        Self {
            origin: Origin::Linux,
            user: User::uid(0),
        }
    }
    /// Select a dedicated service account from protected host configuration.
    ///
    /// # Safety
    /// The UID must belong exclusively to the protected broker/provider service,
    /// not a desktop account or a caller-selected UID. Its code, configuration
    /// and control must not be writable by applications. UID alone cannot prove
    /// those properties; the privileged launcher/receiver must establish them.
    pub unsafe fn dedicated_linux_uid(uid: u32) -> io::Result<Self> {
        if uid == 0 || uid == u32::MAX {
            return Err(denied("invalid dedicated service UID"));
        }
        Ok(Self {
            origin: Origin::Linux,
            user: User::uid(uid),
        })
    }
}

/// Exact retained process object, supplied only by the native service boundary.
///
/// # Safety
/// Implementations must query the retained kernel process object, never a PID,
/// socket state, timer or client word. `Ok(true)` means kernel-confirmed death.
/// Errors must not be converted into evidence of liveness or completion.
pub unsafe trait ServiceProcess: Send + Sync {
    fn exited(&self) -> io::Result<bool>;
}

/// Sealed authority from an OS-authenticated privileged process object.
/// It is not serializable, and no PID/SID/role string in a message creates it.
pub struct ServiceEndpoint {
    origin: Origin,
    _user: User,
    process: Arc<dyn ServiceProcess>,
    revoked: Arc<AtomicBool>,
}
impl ServiceEndpoint {
    pub(crate) fn origin(&self) -> Origin {
        self.origin
    }
    // Restricted sharing preserves irreversible revocation across lifecycle
    // and every grant stream. It does not permit a new bootstrap ticket.
    pub(crate) fn share(&self) -> Self {
        Self {
            origin: self.origin,
            _user: self._user.clone(),
            process: self.process.clone(),
            revoked: self.revoked.clone(),
        }
    }
    pub(crate) fn ensure_live(&self) -> io::Result<()> {
        if self.revoked.load(Ordering::SeqCst) {
            return Err(denied("service endpoint authority was revoked"));
        }
        match self.process.exited() {
            Ok(false) if !self.revoked.load(Ordering::SeqCst) => Ok(()),
            Ok(false) => Err(denied("service endpoint authority was revoked")),
            Ok(true) | Err(_) => {
                self.revoked.store(true, Ordering::SeqCst);
                Err(denied(
                    "service process is dead or its kernel identity is unavailable",
                ))
            }
        }
    }
    /// Capture the native receiver's exact peer and retain its kernel object.
    ///
    /// # Safety
    /// `actual_user` must come from the actual connection's OS credentials,
    /// as seen by the privileged receiver. `retained_process` must hold its
    /// original authenticated Windows process HANDLE or exact Linux peer
    /// pidfd, captured without PID-reuse substitution. The receiver must apply
    /// root-owned policy and enforce protected service provenance. Neither
    /// client fields, a reopened PID, nor arbitrary same-UID code satisfies
    /// this contract. A failed/unsupported kernel identity lookup must refuse.
    pub unsafe fn from_kernel_peer(
        policy: &ServicePolicy,
        actual_user: User,
        retained_process: Arc<dyn ServiceProcess>,
    ) -> io::Result<Self> {
        if actual_user != policy.user {
            return Err(denied(
                "control endpoint is not the configured privileged service",
            ));
        }
        if retained_process.exited()? {
            return Err(denied("service process already exited"));
        }
        Ok(Self {
            origin: policy.origin,
            _user: actual_user,
            process: retained_process,
            revoked: Arc::new(AtomicBool::new(false)),
        })
    }
    /// Capture the privileged service's own identity without a PID lookup.
    ///
    /// # Safety
    /// Same protected service policy as `from_kernel_peer`, but `actual_user`
    /// must come from the current OS primary process credentials and the
    /// retained object must name this exact current service process. The
    /// Windows adapter duplicates the current-process pseudo HANDLE; Linux
    /// may use an exact retained native service object. Client input and a
    /// thread impersonation token cannot establish own-service identity.
    pub unsafe fn from_current_service(
        policy: &ServicePolicy,
        actual_user: User,
        retained_process: Arc<dyn ServiceProcess>,
    ) -> io::Result<Self> {
        // SAFETY: the caller establishes the equivalent own-process kernel
        // identity contract above instead of a peer connection's credentials.
        unsafe { Self::from_kernel_peer(policy, actual_user, retained_process) }
    }
}

/// Bounded unauthenticated wire input. Construction does not establish trust.
pub struct Frame {
    canonical: Vec<u8>,
    mac: [u8; 32],
}
impl Frame {
    pub fn from_wire(canonical: &[u8], mac: &[u8]) -> io::Result<Self> {
        if canonical.is_empty() || canonical.len() > MAX_BYTES || mac.len() != 32 {
            return Err(denied("invalid service control frame size"));
        }
        let mut tag = [0; 32];
        tag.copy_from_slice(mac);
        Ok(Self {
            canonical: canonical.to_vec(),
            mac: tag,
        })
    }
    pub fn canonical(&self) -> &[u8] {
        &self.canonical
    }
    pub fn mac(&self) -> &[u8; 32] {
        &self.mac
    }
}

/// Only verification constructs this evidence. It is not a completion proof.
/// The allocator must separately match its actual pending/resource records and
/// kernel/privileged-provider completion before retiring any hold.
pub struct VerifiedMessage {
    message: Message,
}
impl VerifiedMessage {
    pub fn message(&self) -> &Message {
        &self.message
    }
}

/// One retained grant and both service epochs, with independent strict streams.
/// Keep this object across transport reconnects. Restart requires a new rooted
/// binding/epoch; no shared word or client request may reset these counters.
pub struct Authenticator {
    endpoint: ServiceEndpoint,
    key: [u8; KEY_LEN],
    binding: Binding,
    _retained_grant: Arc<dyn Send + Sync>,
    next_send: Option<u64>,
    next_receive: Option<u64>,
}
impl Authenticator {
    /// Construct only inside the authenticated existing privileged service.
    ///
    /// # Safety
    /// `key` must be the existing protected key for exactly `installation`,
    /// never a key supplied by a client. `binding` and its roles must come from
    /// the authenticated canonical grant owner and protected VM/SID-to-UID
    /// configuration. Both service epochs must be fresh, not caller choices.
    /// Production callers must use the durable lifecycle and one stream per
    /// exact binding. Reconstructing this object for a prior binding would
    /// reset replay protection and violates the freshness contract.
    /// `retained_grant` must keep that exact original BAR/grant alive against
    /// release/reuse. A public boot MAC is not service endpoint authority.
    pub unsafe fn new(
        endpoint: ServiceEndpoint,
        installation: &str,
        key: [u8; KEY_LEN],
        binding: Binding,
        retained_grant: Arc<dyn Send + Sync>,
    ) -> io::Result<Self> {
        if binding.installation != installation {
            return Err(denied("service control installation key mismatch"));
        }
        Message {
            binding: binding.clone(),
            origin: endpoint.origin,
            sequence: 1,
            phase: Phase::Grant,
            payload: Payload::default(),
        }
        .validate()?;
        Ok(Self {
            endpoint,
            key,
            binding,
            _retained_grant: retained_grant,
            next_send: Some(1),
            next_receive: Some(1),
        })
    }
    /// Authenticate only this rooted binding, local role and next sequence.
    /// There is no arbitrary-byte/domain signing oracle and no caller sequence.
    pub fn seal(&mut self, phase: Phase, payload: Payload) -> io::Result<Frame> {
        self.ensure_live()?;
        self.check_role(self.endpoint.origin, phase)?;
        let sequence = self
            .next_send
            .ok_or_else(|| denied("service control send sequence exhausted"))?;
        let message = Message {
            binding: self.binding.clone(),
            origin: self.endpoint.origin,
            sequence,
            phase,
            payload,
        };
        let canonical = message.encode()?;
        let mac = self.tag(&canonical);
        self.next_send = sequence.checked_add(1);
        Ok(Frame { canonical, mac })
    }
    /// Verify the full canonical message before advancing replay state.
    /// A valid but semantically inapplicable message still needs refusal by the
    /// owning resource state machine; authentication alone never releases it.
    pub fn verify(&mut self, frame: &Frame) -> io::Result<VerifiedMessage> {
        self.ensure_live()?;
        if frame.canonical.is_empty() || frame.canonical.len() > MAX_BYTES {
            return Err(denied("invalid service control frame size"));
        }
        let expected = self.tag(&frame.canonical);
        let mismatch = expected
            .iter()
            .zip(frame.mac.iter())
            .fold(0u8, |v, (a, b)| v | (a ^ b));
        if mismatch != 0 {
            return Err(denied("invalid service control authentication"));
        }
        let message = Message::decode(&frame.canonical)?;
        if message.binding != self.binding || message.origin != opposite(self.endpoint.origin) {
            return Err(denied("service control binding or direction differs"));
        }
        self.check_role(message.origin, message.phase)?;
        if Some(message.sequence) != self.next_receive {
            return Err(denied("service control replay or sequence gap"));
        }
        self.next_receive = message.sequence.checked_add(1);
        Ok(VerifiedMessage { message })
    }
    fn ensure_live(&mut self) -> io::Result<()> {
        self.endpoint.ensure_live()
    }
    fn check_role(&self, origin: Origin, phase: Phase) -> io::Result<()> {
        let roles = self.binding.roles;
        let required = match phase {
            Phase::Grant
            | Phase::GrantRetired
            | Phase::GrantClosing
            | Phase::GrantReleaseCommit => roles.grantor,
            Phase::GrantQuiescent => opposite(roles.grantor),
            Phase::RegisterProducer
            | Phase::Allocate
            | Phase::Resize
            | Phase::Prepare
            | Phase::ProducerRetired
            | Phase::TransferCancelled => roles.producer,
            Phase::RegisterConsumer
            | Phase::Accept
            | Phase::ConsumerReleased
            | Phase::TransferCancelAck
            | Phase::PrepareDeclined => roles.consumer,
        };
        if origin != required {
            return Err(denied("service control phase is not this endpoint's role"));
        }
        Ok(())
    }
    fn tag(&self, canonical: &[u8]) -> [u8; 32] {
        let mut input = Vec::with_capacity(DOMAIN.len() + canonical.len());
        input.extend_from_slice(DOMAIN);
        input.extend_from_slice(canonical);
        hmac_sha256(&self.key, &input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_control::fixture;
    const KEY: [u8; KEY_LEN] = [0x71; KEY_LEN];
    struct FixtureProcess;
    // SAFETY: state-machine fixture only, not an OS process identity proof.
    unsafe impl ServiceProcess for FixtureProcess {
        fn exited(&self) -> io::Result<bool> {
            Ok(false)
        }
    }
    fn endpoint(origin: Origin) -> ServiceEndpoint {
        let (policy, user) = match origin {
            Origin::Windows => (ServicePolicy::windows_system(), User::sid("S-1-5-18")),
            Origin::Linux => (ServicePolicy::linux_root(), User::uid(0)),
        };
        // SAFETY: state-machine fixture only, not an OS authentication claim.
        unsafe {
            ServiceEndpoint::from_kernel_peer(&policy, user, Arc::new(FixtureProcess)).unwrap()
        }
    }
    fn auth(origin: Origin, binding: Binding) -> Authenticator {
        let installation = binding.installation.clone();
        // SAFETY: test-only installation key and retained simulated grant.
        unsafe {
            Authenticator::new(endpoint(origin), &installation, KEY, binding, Arc::new(())).unwrap()
        }
    }
    fn pair() -> (Authenticator, Authenticator) {
        (
            auth(Origin::Windows, fixture().binding),
            auth(Origin::Linux, fixture().binding),
        )
    }
    fn raw_frame(message: &Message, key: &[u8], domain: &[u8]) -> Frame {
        let canonical = message.encode().unwrap();
        let mut signed = domain.to_vec();
        signed.extend_from_slice(&canonical);
        Frame::from_wire(&canonical, &hmac_sha256(key, &signed)).unwrap()
    }
    #[test]
    fn closing_and_cancel_ack_roles_are_current_binding_directions() {
        let (windows, linux) = pair();
        for phase in [Phase::GrantClosing, Phase::GrantReleaseCommit] {
            assert!(linux.check_role(Origin::Linux, phase).is_ok());
            assert!(windows.check_role(Origin::Windows, phase).is_err());
        }
        assert!(
            windows
                .check_role(Origin::Windows, Phase::GrantQuiescent)
                .is_ok()
        );
        assert!(
            linux
                .check_role(Origin::Linux, Phase::GrantQuiescent)
                .is_err()
        );
        assert!(
            linux
                .check_role(Origin::Linux, Phase::TransferCancelAck)
                .is_ok()
        );
        assert!(
            windows
                .check_role(Origin::Windows, Phase::TransferCancelAck)
                .is_err()
        );
        let mut sender = auth(Origin::Linux, fixture().binding);
        let mut receiver = auth(Origin::Windows, fixture().binding);
        let frame = sender
            .seal(
                Phase::GrantClosing,
                Payload {
                    close_generation: 7,
                    ..Payload::default()
                },
            )
            .unwrap();
        let mut changed = Message::decode(&frame.canonical).unwrap();
        changed.payload.close_generation += 1;
        let changed = Frame::from_wire(&changed.encode().unwrap(), &frame.mac).unwrap();
        assert!(receiver.verify(&changed).is_err());
        assert!(receiver.verify(&frame).is_ok());
    }
    #[test]
    fn ordinary_user_cannot_create_service_authority() {
        // SAFETY: intentionally rejected credential fixtures; no authority produced.
        assert!(
            unsafe {
                ServiceEndpoint::from_kernel_peer(
                    &ServicePolicy::windows_system(),
                    User::sid("S-1-5-21-1-2-3-1001"),
                    Arc::new(FixtureProcess),
                )
            }
            .is_err()
        );
        assert!(
            unsafe {
                ServiceEndpoint::from_kernel_peer(
                    &ServicePolicy::linux_root(),
                    User::uid(1000),
                    Arc::new(FixtureProcess),
                )
            }
            .is_err()
        );
        // SAFETY: synthetic root-configured dedicated service identity.
        let dedicated = unsafe { ServicePolicy::dedicated_linux_uid(441).unwrap() };
        assert!(
            unsafe {
                ServiceEndpoint::from_kernel_peer(
                    &dedicated,
                    User::uid(1000),
                    Arc::new(FixtureProcess),
                )
            }
            .is_err()
        );
    }
    #[test]
    fn valid_control_is_bound_and_replay_is_refused() {
        let (mut sender, mut receiver) = pair();
        let frame = sender.seal(Phase::Allocate, fixture().payload).unwrap();
        let verified = receiver.verify(&frame).unwrap();
        assert_eq!(verified.message(), &fixture());
        assert!(receiver.verify(&frame).is_err());
        let second = sender.seal(Phase::Allocate, fixture().payload).unwrap();
        assert_eq!(receiver.verify(&second).unwrap().message().sequence, 2);
    }
    #[test]
    fn public_boot_signature_and_wrong_domain_cannot_authenticate_control() {
        let (_, mut receiver) = pair();
        let m = fixture();
        for domain in [b"wc-broker/boot/v1\0".as_slice(), b"".as_slice()] {
            assert!(receiver.verify(&raw_frame(&m, &KEY, domain)).is_err());
        }
        let rec = wc_handle::macboot::Record {
            session: "fixture".into(),
            arena_run: 71,
            arena_pages: 4096,
            outbound: 4096,
            rings_run: 81,
            rings_pages: 1,
            gpu: false,
        };
        let identity = wc_handle::macboot::Identity {
            os: "windows".into(),
            user: "fixture-user".into(),
            id: m.binding.windows_sid.clone(),
        };
        let boot = wc_handle::macboot::Auth::sign_identity(
            &rec,
            1,
            Some(&identity),
            "test-install",
            1,
            &KEY,
        );
        let forged = Frame::from_wire(&m.encode().unwrap(), &boot.mac).unwrap();
        assert!(receiver.verify(&forged).is_err());
        assert!(receiver.verify(&raw_frame(&m, &KEY, DOMAIN)).is_ok());
    }
    #[test]
    fn changed_binding_is_refused_even_with_valid_installation_mac() {
        let original = fixture();
        for which in 0..18 {
            let mut m = original.clone();
            match which {
                0 => m.binding.installation = "other-install".into(),
                1 => m.binding.source.vm = "other-vm".into(),
                2 => m.binding.source.windows_device.push('x'),
                3 => m.binding.source.host_device += 1,
                4 => m.binding.source.host_inode += 1,
                5 => m.binding.source.qemu_start += 1,
                6 => m.binding.source.driver_generation += 1,
                7 => m.binding.source.bar_bytes += 4096,
                8 => m.binding.windows_run += 1,
                9 => m.binding.linux_run += 1,
                10 => m.binding.grant += 1,
                11 => m.binding.session += 1,
                12 => m.binding.grant_offset += 4096,
                13 => m.binding.grant_length += 4096,
                14 => m.binding.windows_sid.push('2'),
                15 => m.binding.linux_uid += 1,
                16 => m.binding.roles.producer = Origin::Linux,
                _ => m.binding.source.bar_run += 1,
            }
            if which == 12 {
                m.payload.offset += 4096;
            }
            let (_, mut receiver) = pair();
            assert!(
                receiver.verify(&raw_frame(&m, &KEY, DOMAIN)).is_err(),
                "binding field {which}"
            );
            assert!(receiver.verify(&raw_frame(&original, &KEY, DOMAIN)).is_ok());
        }
    }
    #[test]
    fn tampered_phase_recipient_and_mac_do_not_advance_sequence() {
        let (mut sender, mut receiver) = pair();
        let good = sender.seal(Phase::Allocate, fixture().payload).unwrap();
        let mut wrong_phase = good.canonical.clone();
        wrong_phase[9] = Phase::ProducerRetired as u8;
        assert!(
            receiver
                .verify(&Frame::from_wire(&wrong_phase, &good.mac).unwrap())
                .is_err()
        );
        let mut prepare = fixture();
        prepare.phase = Phase::Prepare;
        prepare.payload = Payload {
            producer_registration: 41,
            recipient_registration: 52,
            allocation: 42,
            transfer: 61,
            ..Payload::default()
        };
        let prepared = raw_frame(&prepare, &KEY, DOMAIN);
        let mut wrong_recipient = prepared.canonical.clone();
        let recipient_byte = wrong_recipient.len() - 64 + 8;
        wrong_recipient[recipient_byte] ^= 1;
        assert!(
            receiver
                .verify(&Frame::from_wire(&wrong_recipient, &prepared.mac).unwrap())
                .is_err()
        );
        for byte in 0..32 {
            let mut tag = good.mac;
            tag[byte] ^= 1;
            assert!(
                receiver
                    .verify(&Frame::from_wire(&good.canonical, &tag).unwrap())
                    .is_err()
            );
        }
        assert!(receiver.verify(&good).is_ok());
    }
    #[test]
    fn reflection_wrong_service_role_gap_and_old_epoch_are_refused() {
        let (mut sender, mut receiver) = pair();
        assert!(
            sender
                .seal(Phase::ConsumerReleased, Payload::default())
                .is_err()
        );
        let reflected = sender.seal(Phase::Allocate, fixture().payload).unwrap();
        assert!(sender.verify(&reflected).is_err());
        let mut gap = fixture();
        gap.sequence = 2;
        assert!(receiver.verify(&raw_frame(&gap, &KEY, DOMAIN)).is_err());
        let mut fake_completion = fixture();
        fake_completion.phase = Phase::ConsumerReleased;
        fake_completion.payload = Payload {
            producer_registration: 41,
            recipient_registration: 52,
            allocation: 42,
            consumer: 61,
            ..Payload::default()
        };
        assert!(
            receiver
                .verify(&raw_frame(&fake_completion, &KEY, DOMAIN))
                .is_err()
        );
        let mut restarted = fixture().binding;
        restarted.linux_run += 1;
        assert!(auth(Origin::Linux, restarted).verify(&reflected).is_err());
        assert!(receiver.verify(&reflected).is_ok());
    }
    #[test]
    fn exhausted_stream_never_wraps_or_resets() {
        let (mut sender, mut receiver) = pair();
        sender.next_send = Some(u64::MAX);
        receiver.next_receive = Some(u64::MAX);
        let frame = sender.seal(Phase::Allocate, fixture().payload).unwrap();
        assert!(receiver.verify(&frame).is_ok());
        assert!(sender.seal(Phase::Allocate, fixture().payload).is_err());
        assert!(receiver.verify(&frame).is_err());
    }
    #[test]
    fn bounded_frame_and_key_label_refuse_before_use() {
        assert!(Frame::from_wire(&[], &[0; 32]).is_err());
        assert!(Frame::from_wire(&vec![0; MAX_BYTES + 1], &[0; 32]).is_err());
        assert!(Frame::from_wire(&[1], &[0; 31]).is_err());
        // SAFETY: deliberately mismatched synthetic key label, rejected.
        assert!(
            unsafe {
                Authenticator::new(
                    endpoint(Origin::Windows),
                    "other-install",
                    KEY,
                    fixture().binding,
                    Arc::new(()),
                )
            }
            .is_err()
        );
    }
    #[test]
    fn kernel_death_or_query_failure_irrevocably_revokes_endpoint() {
        use std::sync::atomic::{AtomicU8, Ordering};
        struct Watched(AtomicU8);
        // SAFETY: injected process-watch results for state-machine testing only.
        unsafe impl ServiceProcess for Watched {
            fn exited(&self) -> io::Result<bool> {
                match self.0.load(Ordering::SeqCst) {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(io::Error::other("fixture query failure")),
                }
            }
        }
        for event in [1, 2] {
            let process = Arc::new(Watched(AtomicU8::new(0)));
            // SAFETY: fixture-only retained privileged process and key/grant.
            let endpoint = unsafe {
                ServiceEndpoint::from_kernel_peer(
                    &ServicePolicy::windows_system(),
                    User::sid("S-1-5-18"),
                    process.clone(),
                )
                .unwrap()
            };
            let mut sender = unsafe {
                Authenticator::new(
                    endpoint,
                    "test-install",
                    KEY,
                    fixture().binding,
                    Arc::new(()),
                )
                .unwrap()
            };
            assert!(sender.seal(Phase::Allocate, fixture().payload).is_ok());
            process.0.store(event, Ordering::SeqCst);
            assert!(sender.seal(Phase::Allocate, fixture().payload).is_err());
            process.0.store(0, Ordering::SeqCst);
            assert!(sender.seal(Phase::Allocate, fixture().payload).is_err());
            assert!(sender.verify(&raw_frame(&fixture(), &KEY, DOMAIN)).is_err());
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn actual_unix_peer_credentials_cannot_claim_root_service() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let (peer, _other) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&cred) as libc::socklen_t;
        // SAFETY: actual connected kernel socket and sized credential output.
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    peer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut cred as *mut libc::ucred).cast(),
                    &mut size,
                )
            },
            0
        );
        assert_eq!(cred.pid, unsafe { libc::getpid() });
        // This test process is still alive, so this self-pid cannot be reused.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, cred.pid, 0) as i32 };
        assert!(raw >= 0);
        struct KernelProcess(OwnedFd);
        // SAFETY: the test retains its exact live process pidfd and uses only
        // kernel POLLIN exit evidence; socket closure is never consulted.
        unsafe impl ServiceProcess for KernelProcess {
            fn exited(&self) -> io::Result<bool> {
                let mut poll = libc::pollfd {
                    fd: self.0.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut poll, 1, 0) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                if poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                    return Err(io::Error::other("invalid process pidfd"));
                }
                Ok(poll.revents & libc::POLLIN != 0)
            }
        }
        let retained = Arc::new(KernelProcess(unsafe { OwnedFd::from_raw_fd(raw) }));
        // SAFETY: actual socket credentials and retained exact live test process.
        let result = unsafe {
            ServiceEndpoint::from_kernel_peer(
                &ServicePolicy::linux_root(),
                User::uid(cred.uid),
                retained,
            )
        };
        assert_eq!(result.is_ok(), cred.uid == 0);
    }
}
