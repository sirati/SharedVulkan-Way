//! The broker's Unix half: Linux and macOS (§111.5).
//!
//! * **Who:** `SO_PEERCRED` (Linux) or `getpeereid` and `LOCAL_PEERPID`
//!   (macOS) on the accepted socket. The uid is the user; uid 0, and the
//!   broker's own uid when it runs under a system account of its own, are
//!   admins.
//! * **Liveness:** a pidfd per process on Linux (`SO_PEERPIDFD` where the
//!   kernel has it, so the process cannot be a recycled pid; `pidfd_open`
//!   otherwise), a kqueue `EVFILT_PROC`/`NOTE_EXIT` on macOS. One thread waits
//!   on all of them.
//! * **Handles:** fds, sent with the reply as `SCM_RIGHTS`. A device is
//!   opened by the broker and its fd moved; a region's fd is a copy of the
//!   one the broker keeps.
//! * **Regions:** a sealed memfd on Linux; a POSIX shared-memory object on
//!   macOS, unlinked at once so that only fds reach it, and mapped and wired
//!   by the broker so that its pages stay where the host found them until it
//!   is destroyed (the keeper, §103.15).

use crate::core::{Peer, Pid, User};
use crate::server::{Give, Platform};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Where each device the broker may lend is: a path it opens read-write.
/// On a Linux server that is the stand-in of a VM's device (a BAR file, a
/// port's socket) or a real one; on macOS a `/dev/cu.*` port.
#[derive(Clone, Debug)]
pub struct DevicePath {
    pub name: String,
    pub path: PathBuf,
}

/// A region the broker made: its fd, and on macOS its wired mapping.
pub struct Region {
    pub fd: OwnedFd,
    map: Option<(usize, usize)>,
    /// Where in the region the host's release word is, if this platform's
    /// regions have one (the macOS boot page, §103.15).
    release_word: Option<usize>,
}

impl Region {
    pub(super) fn wired(fd: OwnedFd, len: u64, release_word: Option<usize>) -> io::Result<Self> {
        let n = usize::try_from(len).map_err(|_| io::Error::other("region too large"))?;
        let mut region = Self { fd, map: None, release_word };
        // SAFETY: a live, fixed-size shared-memory fd provided by the broker.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), n, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED, region.fd.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED { return Err(io::Error::last_os_error()); }
        region.map = Some((p as usize, n));
        // SAFETY: this process's own live mapping. Other processes' mlock
        // state is independent, so broker exit cannot unwire this mapping.
        if unsafe { libc::mlock(p, n) } != 0 {
            return Err(io::Error::other(format!("wiring the region: {}", io::Error::last_os_error())));
        }
        Ok(region)
    }

    pub(super) fn boot_released(&self) -> bool {
        let Some((addr, len)) = self.map else { return false };
        if len < wc_handle::arena::REGION_PAGE { return false; }
        let page0 = addr as *const u8;
        // SAFETY: the owned live mapping contains the complete boot page.
        let page = unsafe { std::slice::from_raw_parts(page0, wc_handle::arena::REGION_PAGE) };
        wc_handle::macboot::is_boot_page(page) && unsafe { wc_handle::macboot::load_released(page0) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        if let Some((addr, len)) = self.map.take() {
            // SAFETY: the mapping this struct made and owns.
            unsafe {
                libc::munlock(addr as *const libc::c_void, len);
                libc::munmap(addr as *mut libc::c_void, len);
            }
        }
    }
}

// SAFETY: the mapping is only read through volatile loads of one word, and
// unmapped once, in Drop.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

/// An fd the broker gives: a device's (moved) or a region's.
pub enum Object {
    Device(OwnedFd),
    Region(Region),
}

impl Object {
    fn fd(&self) -> RawFd {
        match self {
            Object::Device(f) => f.as_raw_fd(),
            Object::Region(r) => r.fd.as_raw_fd(),
        }
    }
}

pub struct Unix {
    /// Connections to serve: accepted on the socket, or handed in (`injector`).
    incoming: Mutex<std::sync::mpsc::Receiver<UnixStream>>,
    inject: std::sync::mpsc::Sender<UnixStream>,
    devices: Vec<DevicePath>,
    services: Vec<DevicePath>,
    /// Users with admin rights besides root: the broker's own account.
    admins: Vec<u32>,
    /// macOS: map and wire every region, and watch this word for its release.
    wire: bool,
    release_word: Option<usize>,
    /// macOS (§111.9): a region whose page 0 is a session's boot page
    /// releases its whole pool when the host's release word there is set.
    boot_release: bool,
    watcher: Watcher,
    holder: Mutex<Option<super::wired_holder::WiredHolder>>,
}

impl Unix {
    /// Listen on `path` (made 0666: who may do what is decided per request,
    /// by uid, not by who may connect).
    pub fn bind(path: impl Into<PathBuf>, devices: Vec<DevicePath>) -> io::Result<Unix> {
        let path = path.into();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        // SAFETY: no arguments.
        let me = unsafe { libc::geteuid() };
        let (inject, incoming) = std::sync::mpsc::channel();
        let accepted = inject.clone();
        std::thread::Builder::new().name("wc-broker-accept".into()).spawn(move || {
            for s in listener.incoming() {
                match s {
                    Ok(s) => {
                        if accepted.send(s).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        eprintln!("[wc-broker] accept: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }
        })?;
        Ok(Unix {
            incoming: Mutex::new(incoming),
            inject,
            devices,
            services: Vec::new(),
            admins: vec![0, me],
            wire: cfg!(target_os = "macos"),
            release_word: None,
            boot_release: false,
            watcher: Watcher::start()?,
            holder: Mutex::new(None),
        })
    }

    /// A way to hand the broker a connection it did not accept itself: one
    /// end of a socket pair a client made and something trusted forwarded
    /// (sv-drm's handoff for sandboxed processes, §112). It is served like any
    /// other, and its identity is still the kernel's: `SO_PEERCRED` on a
    /// socket pair names the process that made it, not whoever carried it.
    pub fn injector(&self) -> std::sync::mpsc::Sender<UnixStream> {
        self.inject.clone()
    }

    /// Register the same broker through sv-drm for sandboxed clients. System
    /// services may accept every uid; the kernel peer identity and existing
    /// broker policy still decide every request. No user mapping lives here.
    #[cfg(target_os = "linux")]
    pub fn register_render_node(&self, services: &std::path::Path) -> io::Result<()> {
        let inject = self.injector();
        // SAFETY: geteuid has no preconditions.
        let any_uid = unsafe { libc::geteuid() } <= sv_drm::relay::SYSTEM_UID_MAX;
        sv_drm::relay::keep_registered(services.into(), sv_drm::relay::BROKER_SERVICE.into(), any_uid, move |f| {
            if let Err(e) = inject_render_client(&inject, f) {
                eprintln!("[wc-broker] refused render-node handoff: {e}");
            }
        })
    }

    /// The local services `connect` reaches: a socket path each.
    pub fn with_services(mut self, services: Vec<DevicePath>) -> Unix {
        self.services = services;
        self
    }

    /// Regions are mapped and wired by the broker for their whole life, and
    /// an orphan is released when the word at `release_word` is nonzero.
    pub fn keep_wired(mut self, release_word: Option<usize>) -> Unix {
        self.wire = true;
        self.release_word = release_word;
        self
    }

    /// The macOS keeper (§103.15, §111.9): every region is mapped and wired
    /// by the broker for its whole life, and a pool's orphans go when the
    /// host writes the release word in the pool's boot page (page 0 of the
    /// session's ring window, `wc_handle::macboot`), which the broker finds
    /// among the pool's regions by its stamp and record.
    pub fn mac_keeper(mut self) -> Unix {
        self.wire = true;
        self.boot_release = true;
        self
    }

    /// Start an independent retention process before any host-visible grant.
    pub fn with_persistent_holder(self, executable: &std::path::Path) -> io::Result<Self> {
        *self.holder.lock().unwrap_or_else(|e| e.into_inner()) = Some(super::wired_holder::WiredHolder::start(executable)?);
        Ok(self)
    }

    /// An object for a region the embedding process made (the host role).
    pub fn object(fd: OwnedFd) -> Object {
        Object::Region(Region { fd, map: None, release_word: None })
    }

    pub fn device_object(fd: OwnedFd) -> Object {
        Object::Device(fd)
    }
}

/// Who is on the other end of `s`.
pub fn peer_of(s: &UnixStream) -> io::Result<(Pid, u32)> {
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: a live socket and a live, sized out-parameter.
        let r = unsafe {
            libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len)
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((cred.pid as Pid, cred.uid))
    }
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: a live socket and two live out-parameters.
        if unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        const SOL_LOCAL: libc::c_int = 0;
        const LOCAL_PEERPID: libc::c_int = 2;
        // SAFETY: as above.
        if unsafe { libc::getsockopt(s.as_raw_fd(), SOL_LOCAL, LOCAL_PEERPID, (&raw mut pid).cast(), &mut len) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((pid as Pid, uid))
    }
}

impl Platform for Unix {
    type Conn = UnixStream;
    type Writer = UnixStream;
    type Object = Object;

    fn accept(&self) -> io::Result<(UnixStream, UnixStream, Peer)> {
        let s = self
            .incoming
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv()
            .map_err(|_| io::Error::other("the accepting thread has stopped"))?;
        let (pid, uid) = peer_of(&s)?;
        let peer = Peer { pid, user: User::uid(uid), admin: self.admins.contains(&uid) };
        let w = s.try_clone()?;
        Ok((s, w, peer))
    }

    fn watch(&self, pid: Pid, exited: Box<dyn FnOnce() + Send>) -> bool {
        self.watcher.watch(pid, exited)
    }

    fn open_device(&self, name: &str) -> io::Result<Object> {
        let d = self
            .devices
            .iter()
            .find(|d| d.name == name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("device {name} has no path")))?;
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&d.path).or_else(|e| {
            // A port's stand-in on a Linux server is a socket: connect to it.
            if std::fs::metadata(&d.path).is_ok_and(|m| {
                use std::os::unix::fs::FileTypeExt;
                m.file_type().is_socket()
            }) {
                UnixStream::connect(&d.path).map(|s| OwnedFd::from(s).into())
            } else {
                Err(e)
            }
        })?;
        raw_if_tty(&f);
        Ok(Object::Device(OwnedFd::from(f)))
    }

    fn create_region(&self, len: u64, _large: bool) -> io::Result<Object> {
        let fd = make_region_fd(len)?;
        let region = if self.wire {
            Region::wired(fd, len, self.release_word)?
        } else {
            Region { fd, map: None, release_word: self.release_word }
        };
        Ok(Object::Region(region))
    }

    fn region_made(&self, info: &crate::core::RegionInfo, user: &User, obj: &Object) -> io::Result<Option<String>> {
        if let Object::Region(region) = obj
            && let Some(holder) = self.holder.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            holder.retain(info, user, region)?;
        }
        Ok(None)
    }

    fn region_destroyed(&self, id: crate::core::ObjId) {
        if let Some(holder) = self.holder.lock().unwrap_or_else(|e| e.into_inner()).as_mut() { holder.remove(id); }
    }

    fn send(&self, w: &mut UnixStream, make: &dyn Fn(&[String]) -> String, give: Vec<Give<'_, Object>>) -> io::Result<()> {
        let fds: Vec<RawFd> = give
            .iter()
            .map(|g| match g {
                Give::Move(o) => o.fd(),
                Give::Copy(o) => o.fd(),
            })
            .collect();
        let texts = vec![String::from("fd"); fds.len()];
        let mut line = make(&texts);
        line.push('\n');
        send_with_fds(w, line.as_bytes(), &fds)
        // A moved object is dropped here, closing the broker's copy: the
        // client's is now the only one.
    }

    fn push(&self, w: &mut UnixStream, line: &str) -> io::Result<()> {
        let mut l = line.to_string();
        l.push('\n');
        w.write_all(l.as_bytes())
    }

    fn identity(&self, peer: &Peer) -> Option<wc_handle::macboot::Identity> {
        let uid: u32 = peer.user.0.strip_prefix("uid:")?.parse().ok()?;
        Some(wc_handle::macboot::Identity {
            os: if cfg!(target_os = "macos") { "macos" } else { "linux" }.into(),
            user: user_name(uid)?,
            id: uid.to_string(),
        })
    }

    fn open_service(&self, name: &str) -> io::Result<Object> {
        let s = self
            .services
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("service {name} has no socket")))?;
        Ok(Object::Device(OwnedFd::from(UnixStream::connect(&s.path)?)))
    }

    fn released(&self, obj: &Object) -> bool {
        match obj {
            Object::Region(Region { map: Some((addr, len)), release_word: Some(off), .. }) if off + 4 <= *len => {
                // SAFETY: inside the live mapping; the host writes this word.
                let v = unsafe { std::ptr::read_volatile((addr + off) as *const u32) };
                v != 0
            }
            _ => false,
        }
    }

    fn released_pool(&self, obj: &Object) -> bool {
        match obj {
            Object::Region(region) if self.boot_release => region.boot_released(),
            _ => false,
        }
    }

    fn end(&self, pid: Pid) {
        // Only a process still watched: an exit already seen means the pid
        // may be another process's by now.
        if self.watcher.watching(pid) {
            // SAFETY: a signal to a pid; no memory is involved.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// A doorbell port that is a serial device (macOS: a virtio console's `cu.*`
/// node) is put in raw mode before it is moved, as
/// `PortHandle::guest_port` does for a port it opens itself: the holder gets
/// `fd:N`, which takes the descriptor as it is.
fn raw_if_tty(f: &std::fs::File) {
    let fd = f.as_raw_fd();
    // SAFETY: a live descriptor; `t` is a zeroed, valid termios.
    unsafe {
        if libc::isatty(fd) == 1 {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) == 0 {
                libc::cfmakeraw(&mut t);
                libc::tcsetattr(fd, libc::TCSANOW, &t);
            }
        }
    }
}

/// Validate the carrier's metadata against the socket before it reaches
/// the broker's normal SO_PEERCRED authentication and request policy.
#[cfg(target_os = "linux")]
fn inject_render_client(inject: &std::sync::mpsc::Sender<UnixStream>, f: sv_drm::relay::Forwarded) -> io::Result<()> {
    let peer = sv_drm::relay::peer_cred(&f.stream)?;
    if peer.uid != f.uid || peer.pid <= 0 || peer.pid as u32 != f.pid {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "render-node peer uid/pid differs from its handoff"));
    }
    inject.send(f.stream).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "broker stopped"))
}

/// The account name of `uid`, from the password database.
pub fn user_name(uid: u32) -> Option<String> {
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: live out-parameters and a live, sized buffer.
    let r = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if r != 0 || out.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r set pw_name to a NUL-terminated string in `buf`.
    let name = unsafe { std::ffi::CStr::from_ptr(pw.pw_name) };
    name.to_str().ok().map(str::to_string)
}

#[cfg(target_os = "linux")]
fn make_region_fd(len: u64) -> io::Result<OwnedFd> {
    let name = c"wc-broker-region";
    // SAFETY: a static name; flags only.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: a live fd.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Its size is fixed: a holder must not shrink pages from under another.
    // SAFETY: a live memfd.
    unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL);
    }
    Ok(fd)
}

#[cfg(target_os = "macos")]
fn make_region_fd(len: u64) -> io::Result<OwnedFd> {
    let mut rnd = [0u8; 8];
    // SAFETY: a live buffer.
    unsafe { libc::arc4random_buf(rnd.as_mut_ptr().cast(), 8) };
    let name = std::ffi::CString::new(format!("/wcb.{:016x}", u64::from_le_bytes(rnd))).map_err(io::Error::other)?;
    // SAFETY: a live name; O_EXCL, so never someone else's object.
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Unlinked at once: from now on only fds reach it.
    // SAFETY: the name just created.
    unsafe { libc::shm_unlink(name.as_ptr()) };
    // SAFETY: a fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: a live fd.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// `sendmsg` with `fds` as `SCM_RIGHTS`.
pub fn send_with_fds(s: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
    if fds.is_empty() {
        return (&*s).write_all(bytes);
    }
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut libc::c_void, iov_len: bytes.len() };
    // SAFETY: zeroed is a valid msghdr; every pointer set below is live for the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer is sized for exactly this header and payload.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<RawFd>(), fds.len());
    }
    // SAFETY: as above.
    let n = unsafe { libc::sendmsg(s.as_raw_fd(), &msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // The fds went with the first byte; the rest of a long line is plain.
    if (n as usize) < bytes.len() {
        (&*s).write_all(&bytes[n as usize..])?;
    }
    Ok(())
}

/// Read one line from `s`, collecting any fds that came with it.
pub fn recv_line(s: &UnixStream, buf: &mut Vec<u8>, fds: &mut Vec<OwnedFd>) -> io::Result<Option<String>> {
    loop {
        if let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            return Ok(Some(String::from_utf8_lossy(&line[..i]).into_owned()));
        }
        let mut chunk = [0u8; 4096];
        let space = unsafe { libc::CMSG_SPACE((16 * std::mem::size_of::<RawFd>()) as u32) } as usize;
        let mut cbuf = vec![0u8; space];
        let mut iov = libc::iovec { iov_base: chunk.as_mut_ptr().cast(), iov_len: chunk.len() };
        // SAFETY: zeroed is a valid msghdr; the pointers are live for the call.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        // SAFETY: as above.
        let n = unsafe { libc::recvmsg(s.as_raw_fd(), &mut msg, flags) };
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
                    let payload = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    let count = payload / std::mem::size_of::<RawFd>();
                    let p = libc::CMSG_DATA(c).cast::<RawFd>();
                    for i in 0..count {
                        let fd = std::ptr::read_unaligned(p.add(i));
                        #[cfg(not(target_os = "linux"))]
                        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                        fds.push(OwnedFd::from_raw_fd(fd));
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
}

// ------------------------------------------------------------- liveness

type Exited = Box<dyn FnOnce() + Send>;
type Waiting = Arc<Mutex<BTreeMap<Pid, (Option<OwnedFd>, Vec<Exited>)>>>;

/// One thread waiting for every watched process's exit.
struct Watcher {
    #[cfg(target_os = "linux")]
    epoll: OwnedFd,
    #[cfg(target_os = "macos")]
    kq: OwnedFd,
    waiting: Waiting,
}

impl Watcher {
    fn start() -> io::Result<Watcher> {
        let waiting: Waiting = Arc::default();
        #[cfg(target_os = "linux")]
        {
            // SAFETY: flags only.
            let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
            if ep < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a fresh fd we own.
            let epoll = unsafe { OwnedFd::from_raw_fd(ep) };
            let (w, e) = (Arc::clone(&waiting), epoll.as_raw_fd());
            std::thread::Builder::new().name("wc-broker-exits".into()).spawn(move || {
                let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 16];
                loop {
                    // SAFETY: a live epoll fd and a live event array.
                    let n = unsafe { libc::epoll_wait(e, evs.as_mut_ptr(), evs.len() as i32, -1) };
                    if n < 0 {
                        continue;
                    }
                    for ev in &evs[..n as usize] {
                        let pid = ev.u64 as Pid;
                        let gone = w.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
                        if let Some((fd, calls)) = gone {
                            if let Some(fd) = &fd {
                                // SAFETY: removing a registered fd.
                                unsafe { libc::epoll_ctl(e, libc::EPOLL_CTL_DEL, fd.as_raw_fd(), std::ptr::null_mut()) };
                            }
                            for c in calls {
                                c();
                            }
                        }
                    }
                }
            })?;
            Ok(Watcher { epoll, waiting })
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: no arguments.
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a fresh fd we own.
            let kq = unsafe { OwnedFd::from_raw_fd(kq) };
            let (w, k) = (Arc::clone(&waiting), kq.as_raw_fd());
            std::thread::Builder::new().name("wc-broker-exits".into()).spawn(move || {
                loop {
                    // SAFETY: zeroed is a valid kevent.
                    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
                    // SAFETY: a live kqueue and one live event.
                    let n = unsafe { libc::kevent(k, std::ptr::null(), 0, &mut ev, 1, std::ptr::null()) };
                    if n != 1 {
                        continue;
                    }
                    let pid = ev.ident as Pid;
                    let gone = w.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
                    for c in gone.map(|g| g.1).unwrap_or_default() {
                        c();
                    }
                }
            })?;
            Ok(Watcher { kq, waiting })
        }
    }

    fn watching(&self, pid: Pid) -> bool {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&pid)
    }

    fn watch(&self, pid: Pid, exited: Exited) -> bool {
        let mut w = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = w.get_mut(&pid) {
            entry.1.push(exited);
            return true;
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: the raw syscall; pidfd_open(pid, 0).
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as RawFd;
            if fd < 0 {
                return false;
            }
            // SAFETY: a fresh fd we own.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let mut ev = libc::epoll_event { events: libc::EPOLLIN as u32, u64: u64::from(pid) };
            // SAFETY: a live epoll fd, a live pidfd, a live event.
            if unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), libc::EPOLL_CTL_ADD, fd.as_raw_fd(), &mut ev) } != 0 {
                return false;
            }
            w.insert(pid, (Some(fd), vec![exited]));
            true
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: zeroed is a valid kevent.
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            ev.ident = pid as usize;
            ev.filter = libc::EVFILT_PROC;
            ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
            ev.fflags = libc::NOTE_EXIT;
            // SAFETY: a live kqueue and one live change.
            if unsafe { libc::kevent(self.kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
                return false;
            }
            w.insert(pid, (None, vec![exited]));
            true
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod render_node_tests {
    use super::*;
    use crate::proto::Request;
    use crate::server::{Broker, Config};
    use crate::{Allow, DeviceSpec, Release};
    use sv_drm::relay::Forwarded;

    fn forwarded(stream: UnixStream) -> Forwarded {
        let c = sv_drm::relay::peer_cred(&stream).unwrap();
        Forwarded { uid: c.uid, pid: c.pid as u32, stream }
    }

    #[test]
    fn forged_render_node_uid_or_pid_never_enters_the_broker() {
        let (send, recv) = std::sync::mpsc::channel();
        for forge_uid in [true, false] {
            let (_client, stream) = UnixStream::pair().unwrap();
            let mut f = forwarded(stream);
            if forge_uid {
                f.uid = f.uid.wrapping_add(1);
            } else {
                f.pid = f.pid.wrapping_add(1);
            }
            assert_eq!(inject_render_client(&send, f).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert!(matches!(recv.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        }
    }

    #[test]
    fn render_node_registration_reaches_a_service_that_starts_late() {
        let dir = std::env::temp_dir().join(format!("wc-broker-late-drm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let services_path = dir.join("services.sock");
        let _ = std::fs::remove_file(&services_path);
        let unix = Unix::bind(dir.join("broker.sock"), Vec::new()).unwrap();
        assert!(!services_path.exists());
        // This returns without the node's service socket being present.
        unix.register_render_node(&services_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let services = sv_drm::relay::Services::new();
        services.listen(UnixListener::bind(&services_path).unwrap()).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while services.is_empty() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(services.len(), 1, "broker registration did not retry after the node service appeared");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn broker_render_node_handoff_applies_the_mapped_user_service_policy() {
        for mapped in [true, false] {
            let dir = std::env::temp_dir().join(format!("wc-broker-render-{}-{mapped}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let hub_path = dir.join("hub.sock");
            let _ = std::fs::remove_file(&hub_path);
            let hub = UnixListener::bind(&hub_path).unwrap();
            let mut unix = Unix::bind(dir.join("broker.sock"), Vec::new()).unwrap()
                .with_services(vec![DevicePath { name: "sv-hub".into(), path: hub_path }]);
            // Treat the test process as a desktop client, rather than the
            // system account that is creating this broker in the test.
            unix.admins.clear();
            let inject = unix.injector();
            let (mut client, stream) = UnixStream::pair().unwrap();
            client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let f = forwarded(stream);
            let uid = f.uid;
            let config = Config {
                devices: Vec::new(),
                services: vec![DeviceSpec {
                    name: "sv-hub".into(),
                    allow: Allow::Users(vec![User::uid(if mapped { uid } else { uid.wrapping_add(1) })]),
                }],
                keys: None, release: Release::OwnerFree, budget: None, run: 1,
            };
            let broker = Broker::new(unix, config);
            std::thread::spawn(move || broker.serve());
            inject_render_client(&inject, f).unwrap();
            let mut buf = Vec::new();
            let mut fds = Vec::new();
            writeln!(client, "{}", Request::Hello { role: "rdd-test".into() }.line()).unwrap();
            let hello = recv_line(&client, &mut buf, &mut fds).unwrap().unwrap();
            assert!(hello.starts_with("ok hello "), "{hello}");
            assert!(hello.contains(&format!(" uid:{uid} ")), "the relay changed the kernel user: {hello}");
            assert!(fds.is_empty());
            writeln!(client, "{}", Request::Connect { name: "sv-hub".into() }.line()).unwrap();
            let reply = recv_line(&client, &mut buf, &mut fds).unwrap().unwrap();
            if mapped {
                assert!(reply.starts_with("ok connect sv-hub "), "{reply}");
                assert_eq!(fds.len(), 1);
                let mut connected = UnixStream::from(fds.remove(0));
                connected.write_all(b"mapped").unwrap();
                let (mut server, _) = hub.accept().unwrap();
                let mut bytes = [0u8; 6];
                std::io::Read::read_exact(&mut server, &mut bytes).unwrap();
                assert_eq!(&bytes, b"mapped");
            } else {
                assert!(reply.starts_with("err service sv-hub is not for "), "{reply}");
                assert!(fds.is_empty());
            }
            drop(client);
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
