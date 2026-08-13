//! The sockets, and the two directions of the pump.
//!
//! # What it is to each side
//!
//! To the local client this is an ordinary Wayland server: a `SOCK_STREAM`
//! `AF_UNIX` listener at `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`, ancillary data
//! and all. `weston-terminal` connects to it with stock libwayland and cannot
//! tell it apart from a compositor, which is the entire point — the milestone
//! is an **unmodified** third-party client.
//!
//! To the compositor this is an ordinary client of ours: the same byte stream
//! `wc-shmclient` speaks, with `h` arguments in band (docs/design.md §2.3).
//!
//! # Why `SCM_RIGHTS` on one side is not a violation of the rule
//!
//! The rule is that no descriptor crosses the boundary. `SCM_RIGHTS` here is
//! between the client and the proxy, **both on the host**, and the fd it
//! carries is created by the client and consumed by the proxy. It never reaches
//! the socket that crosses. What crosses is a name or an offset, and the peer
//! opens or resolves it itself. The two sockets are deliberately different
//! kinds of thing and this file is where the difference is enforced: the
//! upstream writer has no fd path at all, so a descriptor cannot leak across by
//! accident — there is no code that would send it.
//!
//! # Framing
//!
//! Both directions buffer until a whole message is present. That is not
//! defensive coding, it is required: a Wayland message is 8 bytes of header
//! plus arguments, a stream may deliver any prefix of it, and
//! `scripts/probe-relay.c` deliberately fragments the stream three bytes at a
//! time for exactly this reason. Descriptors are queued separately and matched
//! to `h` arguments positionally across the whole stream, as libwayland does,
//! and **not** per message — a client may batch several messages into one
//! `sendmsg` with all their fds in one control message.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::session::Session;
use crate::wire::{Header, HEADER_LEN};

/// How many descriptors one `recvmsg` will accept.
///
/// libwayland's own limit is `MAX_FDS_OUT = 28`, so a client cannot send more
/// than that in one message; the buffer is sized for twice that so a batch is
/// never split for want of space.
const MAX_FDS_IN: usize = 56;

const READ_CHUNK: usize = 64 * 1024;

/// A listener that removes its socket file when dropped.
#[derive(Debug)]
pub struct Listener {
    inner: UnixListener,
    path: PathBuf,
    _lock: Option<std::fs::File>,
}

impl Listener {
    /// Bind `$dir/$name`, refusing to take over a socket somebody else is
    /// serving.
    ///
    /// The lock file is libwayland's own convention (`<name>.lock`), and using
    /// it means a stray proxy and a real compositor cannot both think they own
    /// the display — which otherwise presents as a client connecting to the
    /// wrong one and no error anywhere.
    pub fn bind(dir: &Path, name: &str) -> io::Result<Listener> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(name);
        let lock_path = dir.join(format!("{name}.lock"));
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        // SAFETY: a live fd owned by `lock`.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{} is locked; another server owns this display", lock_path.display()),
            ));
        }
        let _ = std::fs::remove_file(&path);
        let inner = UnixListener::bind(&path)?;
        Ok(Listener { inner, path, _lock: Some(lock) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Wait up to `timeout` for the one client this stage serves.
    pub fn accept_one(&self, timeout: std::time::Duration) -> io::Result<UnixStream> {
        self.inner.set_nonblocking(true)?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.inner.accept() {
                Ok((s, _)) => {
                    s.set_nonblocking(true)?;
                    return Ok(s);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "no client connected",
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Run the pump until either side closes or `idle_timeout` passes with no
/// traffic.
///
/// Returns why it stopped, for the log — a proxy that exits silently is
/// indistinguishable from one that crashed, and the harness has to be able to
/// tell "the client finished" from "the compositor went away".
pub fn pump(
    session: &mut Session,
    client: UnixStream,
    upstream: UnixStream,
    icons: Option<&mut crate::iconlink::IconLink>,
    idle_timeout: std::time::Duration,
    log: &mut dyn FnMut(&str),
) -> io::Result<&'static str> {
    upstream.set_nonblocking(true)?;
    client.set_nonblocking(true)?;

    let mut from_client = Vec::<u8>::new();
    let mut from_server = Vec::<u8>::new();
    let mut fds = VecDeque::<OwnedFd>::new();
    let mut to_server = Vec::<u8>::new();
    let mut to_client = Vec::<u8>::new();
    let mut to_client_fds = Vec::<OwnedFd>::new();
    let mut last_traffic = std::time::Instant::now();
    let mut icons = icons;

    loop {
        // Three descriptors at most, and the third is this program's *own*
        // connection to the compositor rather than anything a client gave it —
        // see `iconlink` for why the icon feed cannot ride the first one.
        let mut pollfds = [
            libc::pollfd { fd: client.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: upstream.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd {
                fd: icons.as_ref().map_or(-1, |l| l.fd()),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if !to_server.is_empty() {
            pollfds[1].events |= libc::POLLOUT;
        }
        if !to_client.is_empty() {
            pollfds[0].events |= libc::POLLOUT;
        }
        if icons.as_ref().is_some_and(|l| l.wants_write()) {
            pollfds[2].events |= libc::POLLOUT;
        }
        // A negative fd in a `pollfd` is ignored by `poll`, which is how the
        // no-icons case costs nothing and needs no second loop.
        // SAFETY: a live array of three `pollfd`s whose length is passed
        // truthfully.
        let n = unsafe { libc::poll(pollfds.as_mut_ptr(), 3, 20) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }

        // --- client -> compositor -------------------------------------------
        if pollfds[0].revents & libc::POLLIN != 0 {
            match recv_with_fds(client.as_raw_fd(), &mut from_client, &mut fds)? {
                0 => return Ok("the client closed the connection"),
                _ => last_traffic = std::time::Instant::now(),
            }
            drain(&mut from_client, |h, body| {
                match session.client_message(h, body, &mut fds) {
                    Ok(bytes) => {
                        to_server.extend_from_slice(&bytes);
                        Ok(())
                    }
                    Err(e) => Err(format!("{e}")),
                }
            })
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("client: {e}")))?;
            // STAGE 16. Events the session invented for its own client, which
            // today is `wl_display.delete_id` for objects the proxy destroyed
            // or whose destructor it absorbed. They ride the ordinary client
            // write path and never carry a descriptor — see
            // `Session::take_client_bytes`.
            to_client.extend_from_slice(&session.take_client_bytes());
            for note in session.take_notes() {
                log(&note);
            }
            // The app ids this batch carried, resolved to pixels and published
            // on the other connection. After the forward, always: an icon is
            // never worth delaying a client's message by a directory walk.
            if let Some(link) = icons.as_deref_mut() {
                for app_id in session.take_app_ids() {
                    link.note_app_id(&app_id, log);
                }
            }
        }

        // --- compositor -> client -------------------------------------------
        if pollfds[1].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; READ_CHUNK];
            // SAFETY: a live buffer whose length is passed truthfully.
            let n = unsafe {
                libc::read(upstream.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
            };
            if n == 0 {
                return Ok("the compositor closed the connection");
            }
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::WouldBlock {
                    return Err(e);
                }
            } else {
                last_traffic = std::time::Instant::now();
                from_server.extend_from_slice(&buf[..n as usize]);
            }
            drain(&mut from_server, |h, body| match session.server_message(h, body) {
                Ok((bytes, mut new_fds)) => {
                    to_client.extend_from_slice(&bytes);
                    to_client_fds.append(&mut new_fds);
                    Ok(())
                }
                Err(e) => Err(format!("{e}")),
            })
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("compositor: {e}")))?;
            to_client.extend_from_slice(&session.take_client_bytes());
            for note in session.take_notes() {
                log(&note);
            }
        }

        // --- writes ---------------------------------------------------------
        if !to_server.is_empty() {
            // Never with a control message. There is no code path here that
            // could send a descriptor upstream, which is the rule enforced by
            // construction rather than by review.
            match write_some(upstream.as_raw_fd(), &to_server) {
                Ok(n) => drop(to_server.drain(..n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    return Ok("the compositor stopped reading");
                }
                Err(e) => return Err(e),
            }
        }
        if !to_client.is_empty() {
            match send_with_fds(client.as_raw_fd(), &to_client, &to_client_fds) {
                Ok(n) => {
                    drop(to_client.drain(..n));
                    to_client_fds.clear();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    return Ok("the client stopped reading");
                }
                Err(e) => return Err(e),
            }
        }

        // --- the icon feed ---------------------------------------------------
        //
        // Deliberately last, and deliberately not counted as traffic: a run
        // whose client has gone quiet should still time out, and the icon
        // connection is chatty exactly once, at the start.
        if let Some(link) = icons.as_deref_mut() {
            if pollfds[2].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                link.on_readable(log)?;
            }
            if link.wants_write() {
                link.on_writable()?;
            }
        }

        if last_traffic.elapsed() > idle_timeout {
            return Ok("idle timeout");
        }
    }
}

/// Consume every complete message in `buf`, leaving any partial one behind.
fn drain(
    buf: &mut Vec<u8>,
    mut each: impl FnMut(Header, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut at = 0usize;
    while let Some(h) = Header::parse(&buf[at..]) {
        if h.size < HEADER_LEN {
            return Err(format!("message on object {} claims {} bytes", h.object, h.size));
        }
        if buf.len() - at < h.size {
            break;
        }
        each(h, &buf[at + HEADER_LEN..at + h.size])?;
        at += h.size;
    }
    buf.drain(..at);
    Ok(())
}

fn write_some(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: a live fd and a buffer whose length is passed truthfully.
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// `recvmsg` with `MSG_CMSG_CLOEXEC`, appending bytes to `buf` and descriptors
/// to `fds`.
fn recv_with_fds(
    fd: RawFd,
    buf: &mut Vec<u8>,
    fds: &mut VecDeque<OwnedFd>,
) -> io::Result<usize> {
    let mut data = [0u8; READ_CHUNK];
    // Space for MAX_FDS_IN descriptors plus one cmsghdr.
    let mut cbuf = [0u8; 16 + 4 * MAX_FDS_IN];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = cbuf.len() as _;

    // SAFETY: `msg` points at live buffers whose lengths are passed truthfully.
    // `MSG_CMSG_CLOEXEC` so a descriptor cannot leak into a child between here
    // and the `OwnedFd` that takes ownership of it below.
    let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            return Ok(usize::MAX);
        }
        return Err(e);
    }

    // SAFETY: `msg` was just filled in by `recvmsg`; `CMSG_FIRSTHDR` and
    // `CMSG_NXTHDR` walk only within `msg_control`/`msg_controllen`.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let payload = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = payload / 4;
                let base = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for i in 0..count {
                    fds.push_back(OwnedFd::from_raw_fd(base.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    if n == 0 {
        return Ok(0);
    }
    buf.extend_from_slice(&data[..n as usize]);
    Ok(n as usize)
}

/// `sendmsg`, with `fds` in a single `SCM_RIGHTS` control message.
///
/// Only ever called on the **client** socket. See this module's header.
fn send_with_fds(fd: RawFd, buf: &[u8], fds: &[OwnedFd]) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let mut iov = libc::iovec {
        iov_base: buf.as_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;

    let mut cbuf = vec![0u8; if fds.is_empty() { 0 } else { unsafe { libc::CMSG_SPACE(4 * fds.len() as u32) as usize } }];
    if !fds.is_empty() {
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = cbuf.len() as _;
        // SAFETY: `msg_control` points at `cbuf`, which is exactly
        // `CMSG_SPACE(4 * fds.len())` bytes, so one header and `fds.len()`
        // four-byte descriptors fit.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(4 * fds.len() as u32) as _;
            let base = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            for (i, f) in fds.iter().enumerate() {
                base.add(i).write_unaligned(f.as_raw_fd());
            }
        }
    }

    // SAFETY: `msg` points at live buffers whose lengths are passed truthfully.
    let n = unsafe { libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_leaves_a_partial_message_behind() {
        // Two whole messages and three bytes of a third, which is exactly what
        // `probe-relay.c --chunk 3` produces and what stage 1 found nothing had
        // ever generated before.
        let mut buf = Vec::new();
        buf.extend_from_slice(&crate::wire::message(1, 0, &[1, 2, 3, 4]));
        buf.extend_from_slice(&crate::wire::message(2, 1, &[]));
        buf.extend_from_slice(&[7, 0, 0]);
        let mut seen = Vec::new();
        drain(&mut buf, |h, body| {
            seen.push((h.object, h.opcode, body.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [(1, 0, 4), (2, 1, 0)]);
        assert_eq!(buf, [7, 0, 0]);
    }

    #[test]
    fn drain_holds_a_message_whose_body_has_not_arrived() {
        let whole = crate::wire::message(3, 2, &[9; 16]);
        for n in 0..whole.len() {
            let mut buf = whole[..n].to_vec();
            let mut count = 0;
            drain(&mut buf, |_, _| {
                count += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(count, 0, "a {n}-byte prefix was dispatched");
            assert_eq!(buf.len(), n, "a {n}-byte prefix was consumed");
        }
    }

    #[test]
    fn an_impossible_size_is_refused_rather_than_looping() {
        // A size below the header length would make `at` advance by less than
        // one message and spin forever. It is a hostile stream, not a short
        // read, and it is named as such.
        let mut buf = vec![1, 0, 0, 0, 0, 0, 4, 0];
        assert!(drain(&mut buf, |_, _| Ok(())).is_err());
    }

    /// Descriptors and bytes really do cross a socket pair together, and the
    /// received fd refers to the same object. This is the one thing in this
    /// file that cannot be checked without a kernel.
    #[test]
    fn scm_rights_round_trips_over_a_socket_pair() {
        let (a, b) = UnixStream::pair().unwrap();
        let memfd = {
            // SAFETY: a NUL-terminated literal name and a valid flag.
            let raw = unsafe { libc::memfd_create(c"wc-test".as_ptr(), 0) };
            assert!(raw >= 0);
            // SAFETY: freshly returned by `memfd_create` and owned here.
            unsafe { OwnedFd::from_raw_fd(raw) }
        };
        use std::io::Write as _;
        let mut f = std::fs::File::from(memfd.try_clone().unwrap());
        f.write_all(b"marker").unwrap();

        let sent = send_with_fds(a.as_raw_fd(), b"hello", &[memfd]).unwrap();
        assert_eq!(sent, 5);

        let mut buf = Vec::new();
        let mut fds = VecDeque::new();
        let n = recv_with_fds(b.as_raw_fd(), &mut buf, &mut fds).unwrap();
        assert_eq!(n, 5);
        assert_eq!(buf, b"hello");
        assert_eq!(fds.len(), 1);

        use std::io::{Read as _, Seek as _};
        let mut got = std::fs::File::from(fds.pop_front().unwrap());
        got.rewind().unwrap();
        let mut s = String::new();
        got.read_to_string(&mut s).unwrap();
        assert_eq!(s, "marker");
    }

    #[test]
    fn a_listener_refuses_to_take_over_a_locked_display() {
        let dir = std::env::temp_dir().join(format!("wc-proxy-lock-{}", std::process::id()));
        let _first = Listener::bind(&dir, "wl-test").unwrap();
        let second = Listener::bind(&dir, "wl-test");
        assert_eq!(second.unwrap_err().kind(), io::ErrorKind::AddrInUse);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
