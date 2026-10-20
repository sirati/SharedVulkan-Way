//! Platform-independent render-node relay ABI and authorization rules.
use std::io;
use std::time::Duration;

/// `DRM_COMMAND_BASE`: the driver-private ioctl numbers start here.
pub const DRM_COMMAND_BASE: u32 = 0x40;
/// `DRM_SVDRM_CONNECT`, the first driver-private number.
pub const DRM_SVDRM_CONNECT: u32 = DRM_COMMAND_BASE;
/// `char service[64]`: a NUL-terminated name of at most 63 bytes.
pub const SERVICE_LEN: usize = 64;
/// `sizeof(struct drm_svdrm_connect) = { __s32 fd; char service[64]; }`.
pub const CONNECT_SIZE: usize = 4 + SERVICE_LEN;
/// `DRM_IOW(DRM_COMMAND_BASE + 0, struct drm_svdrm_connect)` = `0x40446440`.
pub const DRM_IOCTL_SVDRM_CONNECT: u32 = crate::drm::drm_ioc(1, DRM_SVDRM_CONNECT, CONNECT_SIZE as u32);

/// The highest uid that counts as a system uid for `ANY-UID`.
pub const SYSTEM_UID_MAX: u32 = 999;
/// The longest registration line read (the name is at most 63 bytes).
pub const LINE_MAX: usize = 128;
/// How long a new control connection has to say `register …`.
pub const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);

/// The GPU service: `sv-gpu:` + `SV_SOCKET` (see [`service_name`]).
pub const GPU_PREFIX: &str = "sv-gpu:";
/// The doorbell hub: `sv-bell:` + `SV_BELL`.
pub const BELL_PREFIX: &str = "sv-bell:";
/// The system broker, whose normal per-user policy applies after forwarding.
pub const BROKER_SERVICE: &str = "waycompose-broker";

/// A service name: 1..=63 bytes of printable ASCII without space, so that it
/// fits the ioctl's field with its NUL and is one word on the control line.
pub fn name_ok(n: &str) -> bool {
    !n.is_empty() && n.len() < SERVICE_LEN && n.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// The name a service registers and a client asks for, derived from the
/// socket path both sides already know (`SV_SOCKET`, `SV_BELL`, byte for byte
/// as given): `prefix + path` when that is a valid name, otherwise
/// `prefix + "#" + 16 hex digits` of the path's 64-bit FNV-1a hash. Paths are
/// absolute, so the two forms cannot collide.
pub fn service_name(prefix: &str, path: &str) -> String {
    let direct = format!("{prefix}{path}");
    if name_ok(&direct) {
        return direct;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{prefix}#{h:016x}")
}

/// The ioctl's argument as the caller lays it out.
pub fn encode_connect(fd: i32, service: &str) -> io::Result<[u8; CONNECT_SIZE]> {
    if !name_ok(service) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a service name: {service:?}")));
    }
    let mut b = [0u8; CONNECT_SIZE];
    b[0..4].copy_from_slice(&fd.to_ne_bytes());
    b[4..4 + service.len()].copy_from_slice(service.as_bytes());
    Ok(b)
}

/// The ioctl's argument as `sv-drm` reads it: `(fd, service)`, or `EINVAL`.
pub fn decode_connect(b: &[u8]) -> Result<(i32, String), i32> {
    if b.len() != CONNECT_SIZE {
        return Err(libc::EINVAL);
    }
    let fd = i32::from_ne_bytes(b[0..4].try_into().unwrap());
    let field = &b[4..];
    let end = field.iter().position(|&c| c == 0).ok_or(libc::EINVAL)?;
    let name = std::str::from_utf8(&field[..end]).map_err(|_| libc::EINVAL)?;
    if !name_ok(name) {
        return Err(libc::EINVAL);
    }
    Ok((fd, name.to_string()))
}

/// Whom a registration serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Only clients of this uid (the registrant's own).
    Uid(u32),
    /// Every uid; the service checks `SO_PEERCRED` itself.
    Any,
}

/// The registration rule, as a pure function: the line (without its newline)
/// and the registrant's `SO_PEERCRED` uid give the name and the scope, or why
/// not.
pub fn parse_register(line: &str, peer_uid: u32) -> Result<(String, Scope), String> {
    let w: Vec<&str> = line.split(' ').collect();
    let (name, any) = match w.as_slice() {
        ["register", n] => (*n, false),
        ["register", n, "ANY-UID"] => (*n, true),
        _ => return Err(format!("not `register NAME [ANY-UID]`: {line:?}")),
    };
    if !name_ok(name) {
        return Err(format!("not a service name: {name:?}"));
    }
    if !any {
        return Ok((name.to_string(), Scope::Uid(peer_uid)));
    }
    if peer_uid == 0 || peer_uid <= SYSTEM_UID_MAX {
        Ok((name.to_string(), Scope::Any))
    } else {
        Err(format!("ANY-UID is for uid 0 and system uids (< {}), not uid {peer_uid}", SYSTEM_UID_MAX + 1))
    }
}

/// The forwarding rule, as a pure function over the live registrations: the
/// scope to forward to, `ENOENT` when nobody registered `name`, `EACCES` when
/// `name` is registered for other uids only.
pub fn decide<'a>(regs: impl IntoIterator<Item = (&'a str, Scope)>, uid: u32, name: &str) -> Result<Scope, i32> {
    let mut any = false;
    let mut other = false;
    for (n, s) in regs {
        if n != name {
            continue;
        }
        match s {
            Scope::Uid(u) if u == uid => return Ok(s),
            Scope::Uid(_) => other = true,
            Scope::Any => any = true,
        }
    }
    if any {
        Ok(Scope::Any)
    } else if other {
        Err(libc::EACCES)
    } else {
        Err(libc::ENOENT)
    }
}

/// What the node asks of its relay.
pub trait Relay: Send + Sync {
    /// Forward fd `fd` of task `pid` (uid `uid`) to the service `service`.
    fn connect(&self, uid: u32, pid: u32, fd: i32, service: &str) -> Result<(), i32>;
}
