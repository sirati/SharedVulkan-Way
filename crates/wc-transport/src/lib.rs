//! The Wayland transport socket, on Linux and on Windows.
//!
//! This crate exists so that the forked `wayland-backend` and `wayland-server`
//! in `vendor/` can say `wc_transport::Stream` where they used to say
//! `std::os::unix::net::UnixStream`, and otherwise be left alone.
//!
//! # Why AF_UNIX and not a named pipe
//!
//! The research report (`research-wayland-on-windows.md` §9.2, decision 1)
//! recommends a **named pipe** as the Windows transport, for two reasons: that
//! Windows AF_UNIX cannot pass file descriptors anyway, and that
//! `GetNamedPipeClientProcessId` gives the server the peer's PID where AF_UNIX
//! has "no `SO_PEERCRED` equivalent".
//!
//! **Both halves of that argument are wrong, and the conclusion inverts.** See
//! `docs/design.md` §2.2 for the full reasoning; in summary:
//!
//! 1. `SIO_AF_UNIX_GETPEERPID` exists. It is a documented `WSAIoctl` on Windows
//!    AF_UNIX sockets, declared in Microsoft's `afunix.h` and carried in
//!    mingw-w64's copy of it. So AF_UNIX *does* have a peer-PID mechanism, and
//!    the research's decisive argument for named pipes evaporates.
//! 2. A named pipe is not a socket, and the Windows backend of `polling` — and
//!    therefore of `calloop`, which the research itself identifies (§1.2) as
//!    the one crate in the stack that already works on Windows — is wepoll,
//!    which polls **only sockets**. Choosing a named pipe would throw away the
//!    single piece of the smithay stack that ports for free.
//!
//! So the transport is AF_UNIX on both platforms. What differs is only how an
//! `h` argument crosses, which is `wc-handle`'s problem and not this crate's.

use std::io;
use std::path::Path;

#[cfg(unix)]
mod imp {
    pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};
}

#[cfg(windows)]
mod imp {
    pub use uds_windows::{UnixListener as Listener, UnixStream as Stream};
}

pub use imp::{Listener, Stream};

/// The type a [`Stream`] or [`Listener`] borrows as, for handing to a poller.
///
/// On Unix this is a file descriptor. On Windows it is a `SOCKET`, which is the
/// one kind of Windows object wepoll can watch — the whole reason the transport
/// is a socket at all.
#[cfg(unix)]
pub type PollSource<'a> = std::os::fd::BorrowedFd<'a>;
#[cfg(windows)]
pub type PollSource<'a> = std::os::windows::io::BorrowedSocket<'a>;

/// Borrow a stream as something a poller can watch.
#[cfg(unix)]
pub fn poll_source(stream: &Stream) -> PollSource<'_> {
    use std::os::fd::AsFd;
    stream.as_fd()
}

/// Borrow a stream as something a poller can watch.
#[cfg(windows)]
pub fn poll_source(stream: &Stream) -> PollSource<'_> {
    use std::os::windows::io::AsSocket;
    stream.as_socket()
}

/// What the peer of a connection claims, or can be shown, to be.
///
/// Wayland's own use of this is thin — `wl_display` exposes it through
/// `Backend::get_client_credentials` and compositors use it for logging and for
/// `security-context`-style policy. It is reproduced here because the *shape*
/// has to survive the port even where the content cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    /// Process id of the peer, or 0 if the platform will not say.
    pub pid: u32,
    /// User id of the peer. Always 0 on Windows: there is no numeric uid, and
    /// inventing one from the token SID would be a lie in a `u32`-shaped hole.
    pub uid: u32,
    /// Group id of the peer. Always 0 on Windows, for the same reason.
    pub gid: u32,
}

/// Ask the operating system who is on the other end.
///
/// On Linux this is `SO_PEERCRED`, which is authoritative — the kernel fills it
/// in and the peer cannot influence it.
///
/// On Windows this is `SIO_AF_UNIX_GETPEERPID`. **Read the caveat**: a PID is a
/// weaker identifier than `SO_PEERCRED`'s triple, and Project Zero has shown
/// that the analogous named-pipe PID can be spoofed by a process that arranges
/// for a different process to make the connect call. Treat the result as an
/// identity *hint* good enough for logging and for correlating a client with a
/// window, and never as a security boundary. Nothing in this project currently
/// makes a trust decision on it, and nothing should start without saying so.
#[cfg(unix)]
pub fn peer_credentials(stream: &Stream) -> io::Result<PeerCredentials> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::fd::AsRawFd;
        // `UnixStream::peer_cred` is still unstable as of Rust 1.97
        // (rust-lang/rust#42839), so this is the same `getsockopt` it would
        // make. Not worth a nightly toolchain for three integers.
        let mut creds: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `creds` is a live `ucred` and `len` truthfully describes it;
        // the socket is borrowed for strictly less than the call.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut creds).cast(),
                &raw mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PeerCredentials {
            pid: creds.pid as u32,
            uid: creds.uid,
            gid: creds.gid,
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = stream;
        Ok(PeerCredentials { pid: 0, uid: 0, gid: 0 })
    }
}

/// Ask the operating system who is on the other end. See the Unix version for
/// the caveats, which are stronger here.
#[cfg(windows)]
pub fn peer_credentials(stream: &Stream) -> io::Result<PeerCredentials> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::WSAIoctl;

    // `_WSAIOR(IOC_VENDOR, 256)` from Microsoft's afunix.h, expanded:
    //   IOC_OUT (0x4000_0000) | IOC_VENDOR (0x1800_0000) | 256
    // mingw-w64 carries the same declaration, which is where this was read
    // from rather than guessed:
    //   <mingw>/x86_64-w64-mingw32/sys-include/afunix.h:17
    const SIO_AF_UNIX_GETPEERPID: u32 = 0x4000_0000 | 0x1800_0000 | 256;

    let mut pid: u32 = 0;
    let mut returned: u32 = 0;
    // SAFETY: `pid` is a live u32 for the whole call and the size we hand the
    // kernel is its real size; the socket is borrowed for strictly less than
    // the call. The input buffer is null/zero-length, which this ioctl accepts.
    let rc = unsafe {
        WSAIoctl(
            stream.as_raw_socket() as _,
            SIO_AF_UNIX_GETPEERPID,
            std::ptr::null_mut(),
            0,
            (&raw mut pid).cast(),
            std::mem::size_of::<u32>() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if rc != 0 {
        // Deliberately not fatal. Wine does not implement this ioctl, and an
        // older Windows might not either; a compositor that refused to accept a
        // client because it could not learn a PID it does not use for anything
        // would be trading a working system for a number.
        return Ok(PeerCredentials { pid: 0, uid: 0, gid: 0 });
    }
    Ok(PeerCredentials { pid, uid: 0, gid: 0 })
}

/// Connect to a Wayland server at an absolute path.
pub fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(path)
}

/// Accept one pending connection, or `Ok(None)` if there is none.
///
/// # Why this is not just `listener.accept()`
///
/// A Wayland server does not want the peer's address — it discards it — and on
/// Windows insisting on parsing it is actively harmful.
///
/// `uds_windows::UnixListener::accept` parses the `sockaddr_un` that `accept()`
/// fills in and **fails the whole call** if `sun_family != AF_UNIX`. A Wayland
/// client never `bind()`s, so it has no address to report, and what the
/// platform writes into that structure for an unnamed peer is not something
/// this code should depend on. Measured under Wine 11.14 staging: `accept()`
/// returns `len = 110` with `sun_family = 0`, so `uds_windows` rejects every
/// connection with "file descriptor did not correspond to a Unix socket" and
/// the server accepts nobody.
///
/// That specific behaviour is a Wine artifact — real Windows is documented to
/// report `AF_UNIX` — but the *fix* is right on both platforms rather than a
/// workaround for Wine: pass null for the address, and the question never
/// arises. We do not use the value, so we should not require it to be
/// well-formed.
#[cfg(windows)]
pub fn accept(listener: &Listener) -> io::Result<Option<Stream>> {
    use std::os::windows::io::{AsRawSocket, FromRawSocket};
    use windows_sys::Win32::Networking::WinSock::{
        accept as ws_accept, WSAGetLastError, INVALID_SOCKET, WSAEWOULDBLOCK,
    };

    // SAFETY: a live listening socket borrowed for the duration of the call;
    // null address and length are the documented way to say "do not tell me
    // who it is".
    let sock = unsafe {
        ws_accept(
            listener.as_raw_socket() as _,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if sock == INVALID_SOCKET {
        // SAFETY: no arguments, no state.
        let err = unsafe { WSAGetLastError() };
        if err == WSAEWOULDBLOCK {
            return Ok(None);
        }
        return Err(io::Error::from_raw_os_error(err));
    }
    // SAFETY: `accept` returned a fresh socket that nothing else owns, so
    // taking ownership of it here is sound and it is closed exactly once, when
    // the `Stream` drops.
    Ok(Some(unsafe { Stream::from_raw_socket(sock as _) }))
}

/// Accept one pending connection, or `Ok(None)` if there is none.
///
/// See the Windows version for why the peer address is discarded; on Unix
/// `std`'s `accept` has no such problem, and this is a thin wrapper for symmetry.
#[cfg(unix)]
pub fn accept(listener: &Listener) -> io::Result<Option<Stream>> {
    match listener.accept() {
        Ok((stream, _)) => Ok(Some(stream)),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

/// Where a Wayland socket lives.
///
/// On Unix this is `$XDG_RUNTIME_DIR/<name>`, per the Wayland convention.
///
/// On Windows there is no `XDG_RUNTIME_DIR` and no `/run/user/1000`. Windows
/// AF_UNIX binds to a **real filesystem path** — not an abstract namespace, and
/// not a `\\.\pipe\` name — so the socket has to live in a real directory. We
/// use `%LOCALAPPDATA%\waycompose`, falling back to `%TEMP%`, because that is
/// per-user, is not roamed onto a network share (which would break a socket),
/// and survives having a stale socket file left in it by a crash.
pub fn socket_dir() -> io::Result<std::path::PathBuf> {
    #[cfg(unix)]
    {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if Path::new(&dir).is_absolute() => Ok(dir.into()),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "XDG_RUNTIME_DIR is not set to an absolute path",
            )),
        }
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("TEMP"))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "neither LOCALAPPDATA nor TEMP is set",
                )
            })?;
        let dir = std::path::PathBuf::from(base).join("waycompose");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn pair_round_trips() {
        let (mut a, mut b) = Stream::pair().unwrap();
        a.write_all(b"wayland").unwrap();
        let mut buf = [0u8; 7];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"wayland");
    }

    #[test]
    fn credentials_do_not_error() {
        // The *value* is platform-dependent and on some platforms is zero by
        // design; what is asserted here is only that asking never fails, since
        // the accept path calls this unconditionally.
        let (a, _b) = Stream::pair().unwrap();
        let creds = peer_credentials(&a).unwrap();
        let _ = creds.pid;
    }

    #[test]
    fn socket_dir_is_absolute_or_explains_itself() {
        match socket_dir() {
            Ok(dir) => assert!(dir.is_absolute(), "socket dir {dir:?} is not absolute"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        }
    }
}
