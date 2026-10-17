//! systemd's file-descriptor store, for the doorbell hub (waycompose-design.md
//! §99.6): the few calls of `sd_notify(3)` and `sd_listen_fds(3)` it needs,
//! written against the documented wire format rather than linking libsystemd.
//!
//! * [`take`] returns the descriptors systemd handed this process at start
//!   (`LISTEN_FDS`, `LISTEN_FDNAMES`, `LISTEN_PID`), by name.
//! * [`store`] gives systemd a copy of a descriptor under a name
//!   (`FDSTORE=1`), replacing any it already holds under that name.
//! * [`remove`] drops a named descriptor from the store (`FDSTOREREMOVE=1`).
//! * [`notify`] sends a plain state line (`READY=1`).
//!
//! Every call is a no-op without `$NOTIFY_SOCKET` / `$LISTEN_PID`: the hub
//! run by hand, or in a test, works exactly as before, it just keeps nothing
//! across a restart.
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const LISTEN_FDS_START: RawFd = 3;

/// The descriptors systemd passed this process, by name. The variables are
/// removed so a child cannot mistake them for its own, and every descriptor
/// is marked close-on-exec.
pub fn take() -> HashMap<String, OwnedFd> {
    let mut out = HashMap::new();
    let pid_ok = std::env::var("LISTEN_PID").ok().and_then(|p| p.parse::<u32>().ok()) == Some(std::process::id());
    let n: i32 = std::env::var("LISTEN_FDS").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    let names: Vec<String> =
        std::env::var("LISTEN_FDNAMES").map(|s| s.split(':').map(str::to_string).collect()).unwrap_or_default();
    // SAFETY: removing our own environment variables at start, before any
    // other thread of ours exists.
    unsafe {
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
    }
    if !pid_ok {
        return out;
    }
    for i in 0..n {
        let fd = LISTEN_FDS_START + i;
        // SAFETY: systemd's contract: fds 3.. 3+LISTEN_FDS-1 are ours and open.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        let name = names.get(i as usize).cloned().unwrap_or_else(|| format!("unnamed{i}"));
        // SAFETY: as above; each is taken exactly once.
        out.insert(name, unsafe { OwnedFd::from_raw_fd(fd) });
    }
    out
}

fn send(msg: &str, fd: Option<RawFd>) -> std::io::Result<bool> {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else { return Ok(false) };
    let path = path.to_string_lossy().into_owned();
    // SAFETY: plain socket(2); closed below on every path.
    let s = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if s < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        unsafe { libc::close(s) };
        return Err(std::io::Error::other("NOTIFY_SOCKET path too long"));
    }
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i] = *b as libc::c_char;
    }
    // An abstract socket is written with a leading '@'.
    if bytes.first() == Some(&b'@') {
        addr.sun_path[0] = 0;
    }
    let addr_len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len()) as libc::socklen_t;
    let mut iov = libc::iovec { iov_base: msg.as_ptr() as *mut libc::c_void, iov_len: msg.len() };
    let mut cbuf = [0u8; 64];
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_name = (&raw mut addr).cast();
    hdr.msg_namelen = addr_len;
    hdr.msg_iov = &raw mut iov;
    hdr.msg_iovlen = 1;
    if let Some(fd) = fd {
        // SAFETY: one SCM_RIGHTS control message carrying one fd, built in
        // a buffer larger than CMSG_SPACE(sizeof(int)).
        unsafe {
            let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
            hdr.msg_control = cbuf.as_mut_ptr().cast();
            hdr.msg_controllen = space as _;
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
        }
    }
    // SAFETY: `hdr` points at live locals for the duration of the call.
    let r = unsafe { libc::sendmsg(s, &hdr, libc::MSG_NOSIGNAL) };
    let e = std::io::Error::last_os_error();
    unsafe { libc::close(s) };
    if r < 0 { Err(e) } else { Ok(true) }
}

/// `READY=1` and the like. Returns whether there was anybody to tell.
pub fn notify(state: &str) -> std::io::Result<bool> {
    send(state, None)
}

/// Keep a copy of `fd` in systemd's store under `name`, replacing any held
/// under that name.
pub fn store(name: &str, fd: &impl AsRawFd) -> std::io::Result<bool> {
    let _ = remove(name);
    send(&format!("FDSTORE=1\nFDNAME={name}"), Some(fd.as_raw_fd()))
}

/// Drop the descriptor stored under `name`.
pub fn remove(name: &str) -> std::io::Result<bool> {
    send(&format!("FDSTOREREMOVE=1\nFDNAME={name}"), None)
}
