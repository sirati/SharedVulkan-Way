//! Native descriptor bootstrap for the headless acceptance test. No graphics or
//! audio payload uses this socket. Only the kernel's descriptor reference moves.
// Darwin libc uses u32 cmsg lengths; Linux uses usize. Keep explicit ABI casts.
#![allow(clippy::unnecessary_cast)]
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

pub fn send(sock: &UnixStream, fd: BorrowedFd<'_>) -> io::Result<()> {
    send_tag(sock, 0xfd, fd)
}

pub fn send_tag(sock: &UnixStream, tag: u8, fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut byte = tag;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    // SAFETY: zero is a valid empty msghdr; all buffers outlive sendmsg.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as _) } as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as _) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>(), fd.as_raw_fd());
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_NOSIGNAL;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        if libc::sendmsg(sock.as_raw_fd(), &msg, flags) != 1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn receive(sock: &UnixStream) -> io::Result<OwnedFd> {
    let (tag, fd) = receive_tag(sock)?;
    if tag != 0xfd {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid descriptor tag",
        ));
    }
    Ok(fd)
}

pub fn receive_tag(sock: &UnixStream) -> io::Result<(u8, OwnedFd)> {
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    // SAFETY: all supplied buffers are aligned and writable for their lengths.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, flags) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            let header = libc::CMSG_LEN(0) as usize;
            if (*cmsg).cmsg_len as usize >= header
                && (*cmsg).cmsg_level == libc::SOL_SOCKET
                && (*cmsg).cmsg_type == libc::SCM_RIGHTS
            {
                let count = ((*cmsg).cmsg_len as usize - header) / std::mem::size_of::<i32>();
                for i in 0..count {
                    let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>().add(i));
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if n != 1 || msg.msg_flags & libc::MSG_CTRUNC != 0 || fds.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected one descriptor bootstrap",
        ));
    }
    let fd = fds.pop().unwrap();
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((byte, fd))
}
