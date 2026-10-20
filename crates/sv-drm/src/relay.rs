//! The render node introduces a sandboxed client to its GPU services
//! (docs/waycompose-design.md §112).
//!
//! Firefox's RDD process may open `/dev/dri/*` and issue any DRM ioctl, make a
//! socketpair and pass descriptors, but it may not `connect()` to our sockets
//! nor open the GPU pool's file by name. So the node does what a kernel driver
//! would do for its own userspace: a driver-private ioctl,
//! [`DRM_IOCTL_SVDRM_CONNECT`], hands `sv-drm serve` one end of a socketpair the
//! caller made, and `sv-drm` passes it to the service registered under the
//! name the caller asked for.
//!
//! The rules, all plain OS identity:
//!
//! * A service registers on the control socket (`sv-drm serve --services`)
//!   with one line, `register NAME` or `register NAME ANY-UID`. The first
//!   serves only its own uid (`SO_PEERCRED`); the second serves every uid and
//!   is allowed only to uid 0 and system uids (< 1000), which do their own
//!   `SO_PEERCRED` check. A registration lives as long as its connection.
//!   A second registration of a live `(NAME, scope)` is refused.
//! * For each forwarded client, `sv-drm` sends `conn UID PID\n` on that
//!   connection with the descriptor as `SCM_RIGHTS`.
//! * The ioctl forwards only to `(NAME, caller's uid)` or `(NAME, ANY-UID)`:
//!   otherwise `ENOENT` (nobody registered NAME) or `EACCES` (NAME is
//!   registered for other uids only). The descriptor is taken from the caller
//!   with `pidfd_getfd` only after that check, and only an `AF_UNIX` stream or
//!   seqpacket socket whose peer credentials carry the caller's uid is passed.
//!   `sv-drm` never opens a file or connects anywhere on anyone's behalf.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

struct Entry {
    id: u64,
    /// The registration connection, written with `conn UID PID`.
    sock: UnixStream,
}

/// The live registrations of one `sv-drm serve`.
#[derive(Default)]
pub struct Services {
    regs: Mutex<HashMap<(String, Scope), Entry>>,
    next: AtomicU64,
}

impl Services {
    pub fn new() -> Arc<Services> {
        Arc::new(Services::default())
    }

    /// Add a registration; refused while `(name, scope)` is registered by a
    /// live connection.
    pub fn add(&self, name: &str, scope: Scope, sock: UnixStream) -> Result<u64, String> {
        let mut r = self.regs.lock().unwrap_or_else(|e| e.into_inner());
        let key = (name.to_string(), scope);
        if r.contains_key(&key) {
            return Err(format!("{name} ({scope:?}) is registered already"));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        r.insert(key, Entry { id, sock });
        Ok(id)
    }

    /// Drop registration `id` (its connection closed).
    pub fn remove(&self, id: u64) {
        let mut r = self.regs.lock().unwrap_or_else(|e| e.into_inner());
        r.retain(|_, e| e.id != id);
    }

    pub fn len(&self) -> usize {
        self.regs.lock().map(|r| r.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Serve the control socket: one thread accepting, one per registration.
    pub fn listen(self: &Arc<Self>, listener: UnixListener) -> io::Result<()> {
        let me = Arc::clone(self);
        std::thread::Builder::new().name("sv-drm-services".into()).spawn(move || {
            for c in listener.incoming() {
                match c {
                    Ok(s) => {
                        let me = Arc::clone(&me);
                        let _ = std::thread::Builder::new().name("sv-drm-reg".into()).spawn(move || me.admit(s));
                    }
                    Err(e) => {
                        eprintln!("sv-drm: services: accept: {e}");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        })?;
        Ok(())
    }

    /// One control connection: its `register` line, then its life.
    pub fn admit(&self, s: UnixStream) {
        let uid = match peer_cred(&s) {
            Ok(c) => c.uid,
            Err(e) => {
                eprintln!("sv-drm: services: no peer credentials: {e}");
                return;
            }
        };
        let _ = s.set_read_timeout(Some(REGISTER_TIMEOUT));
        let line = match read_line(&s) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("sv-drm: services: uid {uid}: no registration line: {e}");
                return;
            }
        };
        let (name, scope) = match parse_register(&line, uid) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("sv-drm: services: uid {uid}: refused: {e}");
                return;
            }
        };
        let Ok(w) = s.try_clone() else { return };
        let id = match self.add(&name, scope, w) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("sv-drm: services: uid {uid}: refused: {e}");
                return;
            }
        };
        eprintln!("sv-drm: services: {name} registered by uid {uid} ({scope:?})");
        // The registration lives as long as the connection. The registrant
        // sends nothing more; anything it sends ends the registration too.
        let _ = s.set_read_timeout(None);
        let mut buf = [0u8; 1];
        let _ = (&s).read(&mut buf);
        self.remove(id);
        let _ = s.shutdown(std::net::Shutdown::Both);
        eprintln!("sv-drm: services: {name} (uid {uid}) gone");
    }
}

impl Relay for Services {
    fn connect(&self, uid: u32, pid: u32, fd: i32, service: &str) -> Result<(), i32> {
        // Decide first: an unregistered or foreign name never touches the
        // caller's descriptors.
        let (id, sock) = {
            let r = self.regs.lock().unwrap_or_else(|e| e.into_inner());
            let scope = decide(r.keys().map(|(n, s)| (n.as_str(), *s)), uid, service)?;
            let e = &r[&(service.to_string(), scope)];
            (e.id, e.sock.try_clone().map_err(|_| libc::EIO)?)
        };
        let got = take_fd(pid, fd)?;
        check_client_socket(got.as_fd(), uid)?;
        let tgid = tgid_of(pid).unwrap_or(pid);
        match send_conn(&sock, uid, tgid, got.as_fd()) {
            Ok(()) => Ok(()),
            // The service is not reading: the client may try again.
            Err(e) if e == libc::EAGAIN => Err(libc::EAGAIN),
            Err(_) => {
                self.remove(id);
                Err(libc::ENOENT)
            }
        }
    }
}

/// One `\n`-terminated line of at most [`LINE_MAX`] bytes, read a byte at a
/// time so that nothing after it is consumed.
fn read_line(s: &UnixStream) -> io::Result<String> {
    let mut v = Vec::new();
    let mut b = [0u8; 1];
    loop {
        let n = (&*s).read(&mut b)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if b[0] == b'\n' {
            break;
        }
        v.push(b[0]);
        if v.len() > LINE_MAX {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
    }
    String::from_utf8(v).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

/// `PIDFD_THREAD` (= `O_EXCL`): a pidfd for a thread that is not its group's
/// leader. The FUSE header names the calling *thread*.
const PIDFD_THREAD: libc::c_uint = libc::O_EXCL as libc::c_uint;

/// `pidfd_open` + `pidfd_getfd`: a duplicate of the caller's fd `fd`.
pub fn take_fd(pid: u32, fd: i32) -> Result<OwnedFd, i32> {
    if pid == 0 || fd < 0 {
        return Err(libc::EBADF);
    }
    let open = |flags: libc::c_uint| -> Result<OwnedFd, i32> {
        // SAFETY: a plain syscall; the result is checked.
        let r = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, flags) };
        if r < 0 {
            return Err(errno());
        }
        // SAFETY: a fresh descriptor we own.
        Ok(unsafe { OwnedFd::from_raw_fd(r as i32) })
    };
    let pidfd = match open(0) {
        Err(libc::EINVAL) => open(PIDFD_THREAD).map_err(|_| libc::ESRCH)?,
        Err(_) => return Err(libc::ESRCH),
        Ok(p) => p,
    };
    // SAFETY: a plain syscall on a live pidfd; the result is checked.
    let r = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd, 0 as libc::c_uint) };
    if r < 0 {
        return Err(errno());
    }
    // SAFETY: pidfd_getfd installs a new descriptor (O_CLOEXEC) we own.
    Ok(unsafe { OwnedFd::from_raw_fd(r as i32) })
}

/// The thread group of task `pid`, from `/proc/<pid>/status`.
pub fn tgid_of(pid: u32) -> Option<u32> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines().find_map(|l| l.strip_prefix("Tgid:")).and_then(|v| v.trim().parse().ok())
}

fn sockopt_int(fd: BorrowedFd<'_>, opt: libc::c_int) -> Result<libc::c_int, i32> {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: a valid descriptor and an out buffer of the size given.
    let r = unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, opt, (&mut v as *mut libc::c_int).cast(), &mut len) };
    if r != 0 { Err(errno()) } else { Ok(v) }
}

/// `SO_PEERCRED` of a socket.
pub fn peer_cred_fd(fd: BorrowedFd<'_>) -> io::Result<libc::ucred> {
    let mut c = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a valid descriptor and an out buffer of the size given.
    let r = unsafe {
        libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut c as *mut libc::ucred).cast(), &mut len)
    };
    if r != 0 { Err(io::Error::last_os_error()) } else { Ok(c) }
}

/// `SO_PEERCRED` of a Unix stream.
pub fn peer_cred(s: &UnixStream) -> io::Result<libc::ucred> {
    peer_cred_fd(s.as_fd())
}

/// Only an `AF_UNIX` stream or seqpacket socket whose peer is the caller's
/// uid is passed on: what the client made with `socketpair`.
fn check_client_socket(fd: BorrowedFd<'_>, uid: u32) -> Result<(), i32> {
    // SAFETY: a zeroed out-struct for fstat on a valid descriptor.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 || st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(libc::ENOTSOCK);
    }
    if sockopt_int(fd, libc::SO_DOMAIN)? != libc::AF_UNIX {
        return Err(libc::EINVAL);
    }
    let ty = sockopt_int(fd, libc::SO_TYPE)?;
    if ty != libc::SOCK_STREAM && ty != libc::SOCK_SEQPACKET {
        return Err(libc::EINVAL);
    }
    match peer_cred_fd(fd) {
        Ok(c) if c.uid == uid => Ok(()),
        _ => Err(libc::EPERM),
    }
}

/// `sendmsg` of `bytes` with `fds` as `SCM_RIGHTS`; `flags` as given.
fn send_with_fds(sock: BorrowedFd<'_>, bytes: &[u8], fds: &[BorrowedFd<'_>], flags: libc::c_int) -> Result<usize, i32> {
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut libc::c_void, iov_len: bytes.len() };
    let n = fds.len();
    // Room for up to four descriptors, 8-aligned.
    let mut cbuf = [0u64; 8];
    // SAFETY: a zeroed msghdr filled in below with pointers to live locals.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if n > 0 {
        if n > 4 {
            return Err(libc::EINVAL);
        }
        // SAFETY: CMSG_* on a buffer large enough for n ints (checked above).
        unsafe {
            let space = libc::CMSG_SPACE((4 * n) as u32) as usize;
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((4 * n) as u32) as _;
            let data = libc::CMSG_DATA(c) as *mut i32;
            for (i, f) in fds.iter().enumerate() {
                data.add(i).write_unaligned(f.as_raw_fd());
            }
        }
    }
    // SAFETY: msg points at live locals for the duration of the call.
    let r = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, flags) };
    if r < 0 { Err(errno()) } else { Ok(r as usize) }
}

/// `conn UID PID\n` and the descriptor, in one `sendmsg` that never waits.
fn send_conn(sock: &UnixStream, uid: u32, pid: u32, fd: BorrowedFd<'_>) -> Result<(), i32> {
    let line = format!("conn {uid} {pid}\n");
    let n = send_with_fds(sock.as_fd(), line.as_bytes(), &[fd], libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL)?;
    if n != line.len() {
        // Half a line would desynchronise the registration: end it.
        return Err(libc::EPIPE);
    }
    Ok(())
}

// ---- the client: what sv-icd does in a sandbox ----

/// Open `node`, make a socketpair, and ask the node to pass one end to
/// `service`. Returns the other end. The node's errno comes back as the
/// error: `ENOENT` (nobody registered the name), `EACCES` (registered for
/// other uids), `EOPNOTSUPP` (a node without services), `ENOTTY`/`EINVAL`
/// (not an `sv-drm` node).
pub fn connect_via(node: &Path, service: &str) -> io::Result<UnixStream> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOCTTY)
        .open(node)?;
    let (ours, theirs) = UnixStream::pair()?;
    let arg = encode_connect(theirs.as_raw_fd(), service)?;
    // SAFETY: the argument is a live buffer of the size the number encodes;
    // the node only reads it.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), DRM_IOCTL_SVDRM_CONNECT as _, arg.as_ptr()) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    drop(theirs);
    Ok(ours)
}

// ---- the service: registering, and receiving clients ----

/// A forwarded client.
#[derive(Debug)]
pub struct Forwarded {
    /// The caller's uid, as the node saw it.
    pub uid: u32,
    /// The caller's process (thread group) id, in `sv-drm`'s pid namespace.
    pub pid: u32,
    /// The socket the client made; its `SO_PEERCRED` is the client's.
    pub stream: UnixStream,
}

/// A live registration: one control connection.
pub struct Registration {
    sock: UnixStream,
    buf: Vec<u8>,
    fds: std::collections::VecDeque<OwnedFd>,
}

impl Registration {
    /// Connect to `services` and send `register NAME[ ANY-UID]`.
    pub fn register(services: &Path, name: &str, any_uid: bool) -> io::Result<Registration> {
        if !name_ok(name) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a service name: {name:?}")));
        }
        let mut sock = UnixStream::connect(services)?;
        let line = if any_uid { format!("register {name} ANY-UID\n") } else { format!("register {name}\n") };
        sock.write_all(line.as_bytes())?;
        Ok(Registration { sock, buf: Vec::new(), fds: Default::default() })
    }

    /// Over an existing connection (tests).
    pub fn from_stream(sock: UnixStream) -> Registration {
        Registration { sock, buf: Vec::new(), fds: Default::default() }
    }

    /// The next forwarded client. `UnexpectedEof` when `sv-drm` closed the
    /// registration (refused, or `sv-drm` ended).
    pub fn accept(&mut self) -> io::Result<Forwarded> {
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                let line = String::from_utf8_lossy(&line[..i]).into_owned();
                let fd = self
                    .fds
                    .pop_front()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{line:?} came without a descriptor")))?;
                let w: Vec<&str> = line.split(' ').collect();
                let (uid, pid) = match w.as_slice() {
                    ["conn", u, p] => (
                        u.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, line.clone()))?,
                        p.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, line.clone()))?,
                    ),
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("not `conn UID PID`: {line:?}"))),
                };
                return Ok(Forwarded { uid, pid, stream: UnixStream::from(fd) });
            }
            let mut data = [0u8; 256];
            let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
            let mut cbuf = [0u64; 8];
            // SAFETY: a zeroed msghdr pointing at live locals.
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
            // SAFETY: as above; the kernel writes within the given lengths.
            let n = unsafe { libc::recvmsg(self.sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            // SAFETY: walking the control messages the kernel wrote.
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&msg);
                while !c.is_null() {
                    if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                        let k = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                        let p = libc::CMSG_DATA(c) as *const i32;
                        for i in 0..k {
                            self.fds.push_back(OwnedFd::from_raw_fd(p.add(i).read_unaligned()));
                        }
                    }
                    c = libc::CMSG_NXTHDR(&msg, c);
                }
            }
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.buf.extend_from_slice(&data[..n as usize]);
            if self.buf.len() > LINE_MAX * 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "an overlong line from sv-drm"));
            }
        }
    }
}

/// Keep `name` registered on `services` for as long as the process lives,
/// re-registering a second after the registration ends (sv-drm restarted, or
/// it refused), and hand every forwarded client to `serve`. Returns at once.
pub fn keep_registered(
    services: PathBuf,
    name: String,
    any_uid: bool,
    serve: impl Fn(Forwarded) + Send + 'static,
) -> io::Result<()> {
    std::thread::Builder::new().name("sv-drm-registration".into()).spawn(move || {
        let mut said_down = false;
        loop {
            match Registration::register(&services, &name, any_uid) {
                Ok(mut r) => {
                    eprintln!("sv-drm relay: registered {name} on {}", services.display());
                    said_down = false;
                    loop {
                        match r.accept() {
                            Ok(f) => serve(f),
                            Err(e) => {
                                eprintln!("sv-drm relay: the registration of {name} ended: {e}");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    if !said_down {
                        eprintln!("sv-drm relay: cannot register {name} on {} ({e}); retrying every second", services.display());
                        said_down = true;
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    })?;
    Ok(())
}

/// Bind the control socket at `path`, mode 0666: any user may register (for
/// its own uid). The parent directory is made 0755 if absent; a stale socket
/// is replaced.
pub fn bind_services(path: &Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
        && !dir.exists()
    {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ioctl_number_and_struct() {
        assert_eq!(DRM_IOCTL_SVDRM_CONNECT, 0x4044_6440);
        assert_eq!(crate::drm::ioc_nr(DRM_IOCTL_SVDRM_CONNECT), 0x40);
        assert_eq!(crate::drm::ioc_size(DRM_IOCTL_SVDRM_CONNECT), 68);
        let b = encode_connect(7, "sv-gpu:/run/x.sock").unwrap();
        assert_eq!(decode_connect(&b), Ok((7, "sv-gpu:/run/x.sock".into())));
        let mut full = [b'a'; CONNECT_SIZE];
        full[0..4].copy_from_slice(&3i32.to_ne_bytes());
        assert_eq!(decode_connect(&full), Err(libc::EINVAL), "no NUL in the field");
        let mut sp = encode_connect(1, "a").unwrap();
        sp[4] = b' ';
        assert_eq!(decode_connect(&sp), Err(libc::EINVAL));
        assert_eq!(decode_connect(&b[..67]), Err(libc::EINVAL));
        assert!(encode_connect(1, &"x".repeat(64)).is_err());
        assert!(encode_connect(1, "").is_err());
    }

    #[test]
    fn service_names() {
        assert_eq!(service_name(GPU_PREFIX, "/run/user/1000/sv.sock"), "sv-gpu:/run/user/1000/sv.sock");
        let long = format!("/run/user/1000/{}/icd.sock", "d".repeat(60));
        let n = service_name(GPU_PREFIX, &long);
        assert!(n.starts_with("sv-gpu:#") && n.len() == 8 + 16 && name_ok(&n), "{n}");
        assert_eq!(n, service_name(GPU_PREFIX, &long), "stable");
        assert_ne!(n, service_name(GPU_PREFIX, &format!("{long}x")));
        let spaced = service_name(BELL_PREFIX, "/tmp/a b/sv.sock");
        assert!(spaced.starts_with("sv-bell:#") && name_ok(&spaced));
    }

    #[test]
    fn the_registration_rule() {
        assert_eq!(parse_register("register sv-gpu:/x", 1000), Ok(("sv-gpu:/x".into(), Scope::Uid(1000))));
        assert_eq!(parse_register("register waycompose-broker ANY-UID", 0), Ok(("waycompose-broker".into(), Scope::Any)));
        assert_eq!(parse_register("register waycompose-broker ANY-UID", 999), Ok(("waycompose-broker".into(), Scope::Any)));
        assert!(parse_register("register waycompose-broker ANY-UID", 1000).is_err(), "a user cannot serve everyone");
        assert!(parse_register("register waycompose-broker ANY-UID", 65534).is_err());
        for bad in ["", "register", "register  x", "register x y", "register x ANY-UID extra", "regster x", "register x any-uid"] {
            assert!(parse_register(bad, 0).is_err(), "{bad:?}");
        }
        assert!(parse_register(&format!("register {}", "n".repeat(64)), 0).is_err());
    }

    #[test]
    fn the_forwarding_rule() {
        let regs = [("a", Scope::Uid(1000)), ("b", Scope::Uid(1001)), ("c", Scope::Any), ("d", Scope::Uid(1000)), ("d", Scope::Any)];
        assert_eq!(decide(regs, 1000, "a"), Ok(Scope::Uid(1000)));
        assert_eq!(decide(regs, 1001, "a"), Err(libc::EACCES), "another uid's service");
        assert_eq!(decide(regs, 0, "b"), Err(libc::EACCES), "root is not special either");
        assert_eq!(decide(regs, 1000, "nobody"), Err(libc::ENOENT));
        assert_eq!(decide(regs, 4242, "c"), Ok(Scope::Any));
        assert_eq!(decide(regs, 1000, "d"), Ok(Scope::Uid(1000)), "one's own first");
        assert_eq!(decide(regs, 1002, "d"), Ok(Scope::Any));
    }

    #[test]
    fn a_live_name_is_not_taken_twice() {
        let s = Services::new();
        let (a, _a2) = UnixStream::pair().unwrap();
        let (b, _b2) = UnixStream::pair().unwrap();
        let id = s.add("x", Scope::Uid(5), a).unwrap();
        assert!(s.add("x", Scope::Uid(5), b.try_clone().unwrap()).is_err());
        assert!(s.add("x", Scope::Uid(6), b.try_clone().unwrap()).is_ok(), "per uid");
        s.remove(id);
        assert!(s.add("x", Scope::Uid(5), b).is_ok(), "free again once the first is gone");
    }

    /// Registration over a real control socket, and a forwarded descriptor
    /// whose `SO_PEERCRED` is the process that made the socketpair.
    #[test]
    fn register_forward_and_peer_credentials() {
        let dir = std::env::temp_dir().join(format!("sv-drm-relay-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("services.sock");
        let s = Services::new();
        s.listen(bind_services(&path).unwrap()).unwrap();
        let mut reg = Registration::register(&path, "svc", false).unwrap();
        let me = std::process::id();
        // SAFETY: getters.
        let uid = unsafe { libc::geteuid() };
        let t0 = std::time::Instant::now();
        while s.is_empty() {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        let (mut client, far) = UnixStream::pair().unwrap();
        assert_eq!(s.connect(uid, me, far.as_raw_fd(), "nope"), Err(libc::ENOENT));
        assert_eq!(s.connect(uid.wrapping_add(1), me, far.as_raw_fd(), "svc"), Err(libc::EACCES));
        let f = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(s.connect(uid, me, f.as_raw_fd(), "svc"), Err(libc::ENOTSOCK), "only a socket");
        s.connect(uid, me, far.as_raw_fd(), "svc").unwrap();
        drop(far);
        let mut got = reg.accept().unwrap();
        assert_eq!((got.uid, got.pid), (uid, me));
        let c = peer_cred(&got.stream).unwrap();
        assert_eq!((c.uid, c.pid as u32), (uid, me));
        client.write_all(b"ping").unwrap();
        let mut b = [0u8; 4];
        got.stream.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"ping");
        // The registration ends with its connection.
        drop(reg);
        let t0 = std::time::Instant::now();
        while !s.is_empty() {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        let (_c2, far2) = UnixStream::pair().unwrap();
        assert_eq!(s.connect(uid, me, far2.as_raw_fd(), "svc"), Err(libc::ENOENT));
        // A user cannot register ANY-UID (unless the tests run as root).
        if uid >= 1000 {
            let mut r = Registration::register(&path, "everyone", true).unwrap();
            assert_eq!(r.accept().unwrap_err().kind(), io::ErrorKind::UnexpectedEof, "refused: the connection is closed");
            assert!(s.is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The socketpair made by **another process**: the service sees that
    /// process's pid and uid in `SO_PEERCRED` and in the `conn` line, and the
    /// two talk. The child is ours, so `pidfd_getfd` passes Yama's scope 1.
    #[test]
    fn a_child_process_socket_reaches_the_service_with_its_credentials() {
        let s = Services::new();
        let (reg_ours, reg_theirs) = UnixStream::pair().unwrap();
        // SAFETY: getter.
        let uid = unsafe { libc::geteuid() };
        s.add("svc", Scope::Uid(uid), reg_theirs).unwrap();
        let mut reg = Registration::from_stream(reg_ours);
        let mut p = [0; 2];
        // SAFETY: pipe into a valid array.
        assert_eq!(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: the child only makes async-signal-safe calls and _exits.
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            unsafe {
                let mut sv = [0; 2];
                if libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
                    libc::_exit(2);
                }
                libc::write(p[1], sv.as_ptr().add(1).cast(), 4);
                let mut b = [0u8; 3];
                let n = libc::read(sv[0], b.as_mut_ptr().cast(), 3);
                libc::write(sv[0], b"ok:".as_ptr().cast(), 3);
                libc::write(sv[0], b.as_ptr().cast(), n.max(0) as usize);
                libc::_exit(0);
            }
        }
        unsafe { libc::close(p[1]) };
        let mut fdb = [0u8; 4];
        // SAFETY: reading into a valid buffer.
        assert_eq!(unsafe { libc::read(p[0], fdb.as_mut_ptr().cast(), 4) }, 4);
        unsafe { libc::close(p[0]) };
        let child_fd = i32::from_ne_bytes(fdb);
        assert_eq!(s.connect(uid, child as u32, child_fd, "svc"), Ok(()));
        let mut got = reg.accept().unwrap();
        assert_eq!(got.pid, child as u32);
        let c = peer_cred(&got.stream).unwrap();
        assert_eq!((c.pid, c.uid), (child, uid), "the client's own credentials");
        got.stream.write_all(b"abc").unwrap();
        let mut b = [0u8; 6];
        got.stream.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"ok:abc");
        let mut st = 0;
        // SAFETY: waiting for our child.
        unsafe { libc::waitpid(child, &mut st, 0) };
        assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0);
    }
}
