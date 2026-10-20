//! Bounded first-contact messages for privileged cross-VM service bootstrap.
//!
//! These messages carry no grant and do not authorize exposure or reclamation.
//! Authentication must compare the complete already-enrolled expected binding
//! from the canonical source owner. A peer's Hello cannot fill wildcard source
//! fields. Fresh durable epochs and independent OS-CSPRNG challenges belong to
//! the service lifecycle, not this data codec or any local-client request.
use crate::service_control::{Binding, Message, Origin, Payload, Phase, Roles, Source};
pub use crate::service_control::{DOMAIN, MAX_BYTES};
use std::io;
const MAGIC: &[u8; 8] = b"WCBBOOT1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapBinding {
    pub installation: String,
    pub source: Source,
    pub roles: Roles,
    pub session: u64,
    pub windows_sid: String,
    pub linux_uid: u32,
}
impl From<&Binding> for BootstrapBinding {
    fn from(b: &Binding) -> Self {
        Self {
            installation: b.installation.clone(),
            source: b.source.clone(),
            roles: b.roles,
            session: b.session,
            windows_sid: b.windows_sid.clone(),
            linux_uid: b.linux_uid,
        }
    }
}
impl BootstrapBinding {
    pub fn validate(&self) -> io::Result<()> {
        // Reuse the canonical source/user/installation checks. These constants
        // are validation-only and never appear in the bootstrap wire format.
        Message {
            binding: Binding {
                installation: self.installation.clone(),
                source: self.source.clone(),
                roles: self.roles,
                windows_run: 1,
                linux_run: 1,
                grant: 1,
                session: self.session,
                grant_offset: 0,
                grant_length: self.source.bar_bytes,
                windows_sid: self.windows_sid.clone(),
                linux_uid: self.linux_uid,
            },
            origin: self.roles.grantor,
            sequence: 1,
            phase: Phase::Grant,
            payload: Payload::default(),
        }
        .validate()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BootstrapPhase {
    Hello = 1,
    Challenge = 2,
    Confirm = 3,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapMessage {
    pub binding: BootstrapBinding,
    pub origin: Origin,
    pub phase: BootstrapPhase,
    pub windows_epoch: u64,
    pub linux_epoch: u64,
    pub windows_challenge: [u8; 32],
    pub linux_challenge: [u8; 32],
}
fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
impl BootstrapMessage {
    pub fn validate(&self) -> io::Result<()> {
        self.binding.validate()?;
        let windows = self.windows_epoch != 0 && self.windows_challenge != [0; 32];
        let linux = self.linux_epoch != 0 && self.linux_challenge != [0; 32];
        let valid = match self.phase {
            BootstrapPhase::Hello => match self.origin {
                Origin::Windows => {
                    windows && self.linux_epoch == 0 && self.linux_challenge == [0; 32]
                }
                Origin::Linux => {
                    linux && self.windows_epoch == 0 && self.windows_challenge == [0; 32]
                }
            },
            BootstrapPhase::Challenge | BootstrapPhase::Confirm => {
                windows && linux && self.windows_challenge != self.linux_challenge
            }
        };
        if !valid {
            return Err(bad("invalid service bootstrap phase fields"));
        }
        Ok(())
    }
    /// Authenticate DOMAIN followed by these exact bytes. The distinct magic
    /// prevents any bootstrap frame from parsing as a granted control message.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let mut out = MAGIC.to_vec();
        out.push(self.origin as u8);
        out.push(self.phase as u8);
        put64(&mut out, self.windows_epoch);
        put64(&mut out, self.linux_epoch);
        out.extend_from_slice(&self.windows_challenge);
        out.extend_from_slice(&self.linux_challenge);
        let b = &self.binding;
        let s = &b.source;
        put_string(&mut out, &b.installation);
        put_string(&mut out, &s.vm);
        put_string(&mut out, &s.windows_device);
        for n in [
            s.host_device,
            s.host_inode,
            s.qemu_start,
            s.driver_generation,
            s.bar_run,
            s.bar_bytes,
        ] {
            put64(&mut out, n);
        }
        out.extend_from_slice(&[
            b.roles.grantor as u8,
            b.roles.producer as u8,
            b.roles.consumer as u8,
        ]);
        put64(&mut out, b.session);
        put_string(&mut out, &b.windows_sid);
        out.extend_from_slice(&b.linux_uid.to_le_bytes());
        if out.len() > MAX_BYTES {
            return Err(bad("service bootstrap message too large"));
        }
        Ok(out)
    }
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(bad("service bootstrap message too large"));
        }
        let mut r = Reader(bytes);
        if r.take(8)? != MAGIC {
            return Err(bad("unknown service bootstrap domain/version"));
        }
        let origin = r.origin()?;
        let phase = match r.byte()? {
            1 => BootstrapPhase::Hello,
            2 => BootstrapPhase::Challenge,
            3 => BootstrapPhase::Confirm,
            _ => return Err(bad("unknown bootstrap phase")),
        };
        let windows_epoch = r.u64()?;
        let linux_epoch = r.u64()?;
        let windows_challenge = r.take(32)?.try_into().unwrap();
        let linux_challenge = r.take(32)?.try_into().unwrap();
        let installation = r.string(32)?;
        let source = Source {
            vm: r.string(64)?,
            windows_device: r.string(1024)?,
            host_device: r.u64()?,
            host_inode: r.u64()?,
            qemu_start: r.u64()?,
            driver_generation: r.u64()?,
            bar_run: r.u64()?,
            bar_bytes: r.u64()?,
        };
        let roles = Roles {
            grantor: r.origin()?,
            producer: r.origin()?,
            consumer: r.origin()?,
        };
        let binding = BootstrapBinding {
            installation,
            source,
            roles,
            session: r.u64()?,
            windows_sid: r.string(184)?,
            linux_uid: r.u32()?,
        };
        if !r.0.is_empty() {
            return Err(bad("trailing bootstrap bytes"));
        }
        let result = Self {
            binding,
            origin,
            phase,
            windows_epoch,
            linux_epoch,
            windows_challenge,
            linux_challenge,
        };
        result.validate()?;
        Ok(result)
    }
}
fn put64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let (head, tail) = self
            .0
            .split_at_checked(n)
            .ok_or_else(|| bad("truncated bootstrap message"))?;
        self.0 = tail;
        Ok(head)
    }
    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn origin(&mut self) -> io::Result<Origin> {
        match self.byte()? {
            1 => Ok(Origin::Windows),
            2 => Ok(Origin::Linux),
            _ => Err(bad("unknown bootstrap origin")),
        }
    }
    fn string(&mut self, limit: usize) -> io::Result<String> {
        let n = u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as usize;
        if n > limit {
            return Err(bad("bootstrap string exceeds bound"));
        }
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| bad("invalid bootstrap UTF-8"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn hello() -> BootstrapMessage {
        BootstrapMessage {
            binding: BootstrapBinding::from(&crate::service_control::fixture().binding),
            origin: Origin::Windows,
            phase: BootstrapPhase::Hello,
            windows_epoch: 71,
            linux_epoch: 0,
            windows_challenge: [0x71; 32],
            linux_challenge: [0; 32],
        }
    }
    #[test]
    fn first_contact_has_no_grant_and_rejects_all_truncation_and_extra_bytes() {
        let m = hello();
        let bytes = m.encode().unwrap();
        assert_eq!(BootstrapMessage::decode(&bytes).unwrap(), m);
        assert!(Message::decode(&bytes).is_err());
        assert!(
            BootstrapMessage::decode(&crate::service_control::fixture().encode().unwrap()).is_err()
        );
        for n in 0..bytes.len() {
            assert!(BootstrapMessage::decode(&bytes[..n]).is_err());
        }
        let mut extra = bytes;
        extra.push(0);
        assert!(BootstrapMessage::decode(&extra).is_err());
    }
    #[test]
    fn phase_shapes_require_independent_nonzero_challenges_and_epochs() {
        let mut m = hello();
        m.linux_epoch = 1;
        assert!(m.encode().is_err());
        m.linux_epoch = 0;
        m.linux_challenge = [1; 32];
        assert!(m.encode().is_err());
        m.phase = BootstrapPhase::Challenge;
        m.linux_epoch = 81;
        assert!(m.encode().is_ok());
        m.phase = BootstrapPhase::Confirm;
        assert!(m.encode().is_ok());
        m.linux_challenge = m.windows_challenge;
        assert!(m.encode().is_err());
        m.linux_challenge = [0; 32];
        assert!(m.encode().is_err());
        m = hello();
        m.origin = Origin::Linux;
        assert!(m.encode().is_err());
    }
    #[test]
    fn bootstrap_never_accepts_missing_or_wildcard_source_identity() {
        let mut m = hello();
        m.binding.source.bar_run = 0;
        assert!(m.encode().is_err());
        m = hello();
        m.binding.source.windows_device.clear();
        assert!(m.encode().is_err());
        m = hello();
        m.binding.source.host_inode = 0;
        assert!(m.encode().is_err());
        m = hello();
        m.binding.installation = "..".into();
        assert!(m.encode().is_err());
    }
}
