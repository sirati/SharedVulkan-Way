//! Canonical cross-VM service control, separate from the public boot signer.
//!
//! Parsing never establishes authority. Only OS-authenticated privileged
//! services may authenticate this domain with the existing installation key.
//! Receivers must also match retained original-BAR/grant/process records before
//! changing lifetime state. A valid MAC cannot replace protected mappings or
//! independently prove a compositor mapping has ceased to exist.
use std::io;

pub const DOMAIN: &[u8] = b"wc-broker/service-control/v1\0";
pub const MAX_BYTES: usize = 4096;
const MAGIC: &[u8; 8] = b"WCBCTL01";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub vm: String,
    pub windows_device: String,
    pub host_device: u64,
    pub host_inode: u64,
    pub qemu_start: u64,
    pub driver_generation: u64,
    pub bar_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub installation: String,
    pub source: Source,
    pub roles: Roles,
    pub windows_run: u64,
    pub linux_run: u64,
    pub grant: u64,
    pub session: u64,
    pub grant_offset: u64,
    pub grant_length: u64,
    pub windows_sid: String,
    pub linux_uid: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Origin {
    Windows = 1,
    Linux = 2,
}
/// Trusted owner-selected directions, compared as part of the full binding.
/// Their appearance in a decoded message grants no endpoint authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roles {
    pub grantor: Origin,
    pub producer: Origin,
    pub consumer: Origin,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Grant = 1,
    Allocate = 2,
    Prepare = 3,
    Accept = 4,
    ProducerRetired = 5,
    ConsumerReleased = 6,
    TransferCancelled = 7,
    GrantRetired = 8,
    Resize = 9,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Payload {
    pub producer_registration: u64,
    pub recipient_registration: u64,
    pub allocation: u64,
    pub transfer: u64,
    pub consumer: u64,
    pub offset: u64,
    pub capacity: u64,
    pub length: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub binding: Binding,
    pub origin: Origin,
    pub sequence: u64,
    pub phase: Phase,
    pub payload: Payload,
}
fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
impl Message {
    pub fn validate(&self) -> io::Result<()> {
        let b = &self.binding;
        let s = &b.source;
        if b.installation.is_empty()
            || b.installation.len() > 32
            || !b
                .installation
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            || s.vm.is_empty()
            || s.vm.len() > 64
            || !s
                .vm
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            || s.windows_device.is_empty()
            || s.windows_device.len() > 1024
            || s.windows_device.chars().any(char::is_control)
            || s.host_device == 0
            || s.host_inode == 0
            || s.qemu_start == 0
            || s.driver_generation == 0
            || s.bar_bytes == 0
            || b.windows_run == 0
            || b.linux_run == 0
            || b.grant == 0
            || b.session == 0
            || b.grant_length == 0
            || !b
                .grant_offset
                .checked_add(b.grant_length)
                .is_some_and(|end| end <= s.bar_bytes)
            || b.windows_sid.len() > 184
            || !b.windows_sid.starts_with("S-1-")
            || b.windows_sid[4..]
                .split('-')
                .any(|n| n.is_empty() || !n.bytes().all(|c| c.is_ascii_digit()))
            || self.sequence == 0
        {
            return Err(bad("incomplete service control binding"));
        }
        let p = self.payload;
        let extent = p.capacity != 0
            && p.length != 0
            && p.length <= p.capacity
            && p.offset >= b.grant_offset
            && p.offset
                .checked_add(p.capacity)
                .is_some_and(|end| end <= b.grant_offset + b.grant_length);
        let no_extent = p.offset == 0 && p.capacity == 0 && p.length == 0;
        let shape = match self.phase {
            Phase::Grant | Phase::GrantRetired => p == Payload::default(),
            Phase::Allocate | Phase::Resize => {
                p.producer_registration != 0
                    && p.recipient_registration == 0
                    && p.allocation != 0
                    && p.transfer == 0
                    && p.consumer == 0
                    && extent
            }
            Phase::Prepare | Phase::TransferCancelled => {
                p.producer_registration != 0
                    && p.recipient_registration != 0
                    && p.allocation != 0
                    && p.transfer != 0
                    && p.consumer == 0
                    && no_extent
            }
            Phase::Accept => {
                p.producer_registration != 0
                    && p.recipient_registration != 0
                    && p.allocation != 0
                    && p.transfer != 0
                    && p.consumer != 0
                    && no_extent
            }
            Phase::ProducerRetired => {
                p.producer_registration != 0
                    && p.recipient_registration == 0
                    && p.allocation != 0
                    && p.transfer == 0
                    && p.consumer == 0
                    && no_extent
            }
            Phase::ConsumerReleased => {
                p.producer_registration != 0
                    && p.recipient_registration != 0
                    && p.allocation != 0
                    && p.transfer == 0
                    && p.consumer != 0
                    && no_extent
            }
        };
        if !shape {
            return Err(bad("service control phase fields differ"));
        }
        Ok(())
    }
    /// Bytes authenticated after DOMAIN. Do not authenticate an alternate text
    /// rendering, a subset of fields, or a public SignReq/boot signature.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(self.origin as u8);
        out.push(self.phase as u8);
        put64(&mut out, self.sequence);
        out.extend_from_slice(&[
            self.binding.roles.grantor as u8,
            self.binding.roles.producer as u8,
            self.binding.roles.consumer as u8,
        ]);
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
            s.bar_bytes,
            b.windows_run,
            b.linux_run,
            b.grant,
            b.session,
            b.grant_offset,
            b.grant_length,
        ] {
            put64(&mut out, n);
        }
        put_string(&mut out, &b.windows_sid);
        out.extend_from_slice(&b.linux_uid.to_le_bytes());
        let p = self.payload;
        for n in [
            p.producer_registration,
            p.recipient_registration,
            p.allocation,
            p.transfer,
            p.consumer,
            p.offset,
            p.capacity,
            p.length,
        ] {
            put64(&mut out, n);
        }
        if out.len() > MAX_BYTES {
            return Err(bad("service control message too large"));
        }
        Ok(out)
    }
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(bad("service control message too large"));
        }
        let mut r = Reader(bytes);
        if r.take(8)? != MAGIC {
            return Err(bad("unknown service control domain/version"));
        }
        let origin = match r.byte()? {
            1 => Origin::Windows,
            2 => Origin::Linux,
            _ => return Err(bad("unknown service control origin")),
        };
        let phase = match r.byte()? {
            1 => Phase::Grant,
            2 => Phase::Allocate,
            3 => Phase::Prepare,
            4 => Phase::Accept,
            5 => Phase::ProducerRetired,
            6 => Phase::ConsumerReleased,
            7 => Phase::TransferCancelled,
            8 => Phase::GrantRetired,
            9 => Phase::Resize,
            _ => return Err(bad("unknown service control phase")),
        };
        let sequence = r.u64()?;
        let roles = Roles {
            grantor: r.origin()?,
            producer: r.origin()?,
            consumer: r.origin()?,
        };
        let installation = r.string(32)?;
        let source = Source {
            vm: r.string(64)?,
            windows_device: r.string(1024)?,
            host_device: r.u64()?,
            host_inode: r.u64()?,
            qemu_start: r.u64()?,
            driver_generation: r.u64()?,
            bar_bytes: r.u64()?,
        };
        let binding = Binding {
            installation,
            source,
            roles,
            windows_run: r.u64()?,
            linux_run: r.u64()?,
            grant: r.u64()?,
            session: r.u64()?,
            grant_offset: r.u64()?,
            grant_length: r.u64()?,
            windows_sid: r.string(184)?,
            linux_uid: r.u32()?,
        };
        let payload = Payload {
            producer_registration: r.u64()?,
            recipient_registration: r.u64()?,
            allocation: r.u64()?,
            transfer: r.u64()?,
            consumer: r.u64()?,
            offset: r.u64()?,
            capacity: r.u64()?,
            length: r.u64()?,
        };
        if !r.0.is_empty() {
            return Err(bad("trailing service control bytes"));
        }
        let result = Self {
            binding,
            origin,
            sequence,
            phase,
            payload,
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
            .ok_or_else(|| bad("truncated service control message"))?;
        self.0 = tail;
        Ok(head)
    }
    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn origin(&mut self) -> io::Result<Origin> {
        match self.byte()? {
            1 => Ok(Origin::Windows),
            2 => Ok(Origin::Linux),
            _ => Err(bad("unknown service control role")),
        }
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn string(&mut self, limit: usize) -> io::Result<String> {
        let n = u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as usize;
        if n > limit {
            return Err(bad("service control string exceeds bound"));
        }
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| bad("invalid service control UTF-8"))
    }
}

#[cfg(test)]
pub(crate) fn fixture() -> Message {
    Message {
        binding: Binding {
            installation: "test-install".into(),
            source: Source {
                vm: "disposable".into(),
                windows_device: r"\\?\PCI#VEN_1AF4#ivshmem".into(),
                host_device: 23,
                host_inode: 47,
                qemu_start: 911,
                driver_generation: 7,
                bar_bytes: 16 << 20,
            },
            roles: Roles {
                grantor: Origin::Linux,
                producer: Origin::Windows,
                consumer: Origin::Linux,
            },
            windows_run: 71,
            linux_run: 81,
            grant: 12,
            session: 3,
            grant_offset: 128 << 10,
            grant_length: 4 << 20,
            windows_sid: "S-1-5-21-1-2-3-1001".into(),
            linux_uid: 1001,
        },
        origin: Origin::Windows,
        sequence: 1,
        phase: Phase::Allocate,
        payload: Payload {
            producer_registration: 41,
            allocation: 42,
            offset: 128 << 10,
            capacity: 65536,
            length: 32768,
            ..Payload::default()
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_frame_rejects_every_truncation_and_trailing_bytes() {
        let message = fixture();
        let bytes = message.encode().unwrap();
        assert_eq!(Message::decode(&bytes).unwrap(), message);
        for end in 0..bytes.len() {
            assert!(Message::decode(&bytes[..end]).is_err(), "prefix {end}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(Message::decode(&trailing).is_err());
        let mut alternate_version = bytes;
        alternate_version[7] = b'2';
        assert!(Message::decode(&alternate_version).is_err());
        assert!(Message::decode(&vec![0; MAX_BYTES + 1]).is_err());
    }
    #[test]
    fn phases_cannot_smuggle_extent_or_unbound_recipient_fields() {
        let mut m = fixture();
        m.phase = Phase::ProducerRetired;
        assert!(m.encode().is_err());
        m.payload = Payload {
            producer_registration: 41,
            allocation: 42,
            ..Payload::default()
        };
        assert!(m.encode().is_ok());
        m.phase = Phase::Prepare;
        m.payload.transfer = 43;
        assert!(m.encode().is_err());
        m.payload.recipient_registration = 44;
        assert!(m.encode().is_ok());
        m.phase = Phase::Accept;
        assert!(m.encode().is_err());
        m.payload.consumer = 45;
        assert!(m.encode().is_ok());
        m.phase = Phase::ConsumerReleased;
        assert!(m.encode().is_err());
        m.payload.transfer = 0;
        assert!(m.encode().is_ok());
        m.phase = Phase::GrantRetired;
        assert!(m.encode().is_err());
        m.payload = Payload::default();
        assert!(m.encode().is_ok());
    }
    #[test]
    fn original_geometry_and_identity_are_bounded_before_authentication() {
        let mut m = fixture();
        m.payload.offset = m.binding.grant_offset - 1;
        assert!(m.encode().is_err());
        m = fixture();
        m.payload.offset = u64::MAX - 1;
        assert!(m.encode().is_err());
        m = fixture();
        m.binding.grant_offset = u64::MAX;
        assert!(m.encode().is_err());
        m = fixture();
        m.binding.grant_length = m.binding.source.bar_bytes;
        assert!(m.encode().is_err());
        m = fixture();
        m.binding.source.windows_device.push('\0');
        assert!(m.encode().is_err());
        m = fixture();
        m.binding.windows_sid = "S-1-5-\n21".into();
        assert!(m.encode().is_err());
        m = fixture();
        m.binding.installation = "../other".into();
        assert!(m.encode().is_err());
        m = fixture();
        m.binding.source.windows_device = "x".repeat(1025);
        assert!(m.encode().is_err());
        m = fixture();
        m.binding.linux_run = 0;
        assert!(m.encode().is_err());
    }
}
