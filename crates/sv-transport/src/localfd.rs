//! The quarantined descriptor handoff: `SCM_RIGHTS`, and the one provider that
//! still needs it.
//!
//! # This module violates docs/design.md §1a and exists anyway
//!
//! > **No file descriptor or handle ever crosses between the app side and the
//! > GPU side.** — docs/design.md §1a
//!
//! Everything in this crate obeys that except what is in this file. It is here,
//! separate, named after what it is and excluded from the cold path proper, so
//! that the violation is one grep rather than a habit — [`crate::cold`] has no
//! ancillary data in it at all any more, and every topology that has to survive
//! a real hypervisor goes through [`crate::region::open_named`] instead.
//!
//! # What still needs it, measured on 2026-08-13
//!
//! `sv_gpu_server::arena::Provider::GpuExport`: the GPU side allocates the arena
//! from the driver and exports it as a dma-buf. A dma-buf has **no name**, so
//! the only way to give one to another process is a descriptor, and there is no
//! variant of this path that a VM boundary could carry. It is therefore a
//! same-machine-only mechanism, and docs/design.md §1a says a same-machine-only
//! mechanism must not be part of the design.
//!
//! It survives for exactly one reason, and it is a hardware fact rather than a
//! preference: **on this host the only real GPU is RADV, and RADV cannot obtain
//! a named arena at all.**
//!
//! | route | measured |
//! |---|---|
//! | host pointer into a named `/dev/shm` object | `vkAllocateMemory: ERROR_INVALID_EXTERNAL_HANDLE` — structural, `AMDGPU_GEM_USERPTR_ANONONLY` |
//! | a dma-buf the GPU side makes locally with `/dev/udmabuf` | `/dev/udmabuf`: `Permission denied` — `root:kvm` 0660, this account is not in `kvm` |
//! | …and even with that group, from a *named* object | impossible: a `/dev/shm` file carries `F_SEAL_SEAL` from birth, `F_ADD_SEALS(F_SEAL_SHRINK)` is `EPERM`, and udmabuf demands `F_SEAL_SHRINK` |
//!
//! The third row is the load-bearing one and it is new: "the GPU side wraps the
//! arena in a local udmabuf" works when the object is QEMU's own sealed memfd
//! reached by path — the cross-VM topology — and **cannot** work when the object
//! is a named one, because no named object on Linux is sealable. So there is no
//! legal route to RADV on one machine, and deleting this module would take five
//! passing harnesses with it. docs/design.md §0.7 records that as a finding rather
//! than as a decision.
//!
//! # `unsafe`
//!
//! One `sendmsg` and one `recvmsg` with a hand-built `msghdr`, moved here
//! unchanged from `cold.rs`.

use crate::cold::{ColdMessage, MAX_COLD_MESSAGE};
use crate::{Error, Result};

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// The most descriptors one message may carry.
///
/// One, and it is the dma-buf. It was two before the boundary rule: the control
/// region used to be a second memfd, and it is a named object on every path now
/// — so the count fell out of the rule rather than being tuned.
pub const MAX_FDS: usize = 1;

/// Send a message, passing up to [`MAX_FDS`] descriptors with it.
///
/// # Safety
///
/// One `sendmsg` with a hand-built `msghdr` and one `SCM_RIGHTS` control
/// message. The invariants:
///
/// - The `iovec` points at `framed`, which outlives the call.
/// - The control buffer is `CMSG_SPACE(4 * MAX_FDS)` bytes and `cmsg_len` is set
///   to `CMSG_LEN(4 * n)`, so the kernel reads exactly `n` `int`s from it.
/// - The descriptors are *borrowed*: the kernel duplicates them into the
///   receiver, and this side's ownership is unaffected.
///
/// The layout of `msghdr`, `iovec` and `cmsghdr` is transcribed for 64-bit
/// Linux, which is asserted at compile time below rather than assumed.
pub fn send_with_fds(sock: &UnixStream, payload: &[u8], fds: &[BorrowedFd<'_>]) -> Result<()> {
    if fds.len() > MAX_FDS {
        return Err(Error::BadColdMessage("too many descriptors for one cold-path message"));
    }
    let mut framed = Vec::with_capacity(payload.len() + 4);
    framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    framed.extend_from_slice(payload);

    if fds.is_empty() {
        sock.try_clone()
            .and_then(|mut s| s.write_all(&framed))
            .map_err(|e| Error::Os { call: "write", errno: e.raw_os_error().unwrap_or(0) })?;
        return Ok(());
    }

    let mut iov = sys::Iovec { base: framed.as_ptr() as *mut u8, len: framed.len() };
    let mut cbuf = [0u8; sys::CMSG_SPACE_MAX_FDS];
    // cmsg_len, cmsg_level, cmsg_type, then the descriptors.
    let cmsg_len = sys::CMSG_LEN_HDR + 4 * fds.len();
    cbuf[0..8].copy_from_slice(&(cmsg_len as u64).to_le_bytes());
    cbuf[8..12].copy_from_slice(&sys::SOL_SOCKET.to_le_bytes());
    cbuf[12..16].copy_from_slice(&sys::SCM_RIGHTS.to_le_bytes());
    for (i, f) in fds.iter().enumerate() {
        let at = 16 + 4 * i;
        cbuf[at..at + 4].copy_from_slice(&f.as_raw_fd().to_le_bytes());
    }

    let mut msg = sys::MsgHdr {
        name: core::ptr::null_mut(),
        namelen: 0,
        _pad0: 0,
        iov: &mut iov,
        iovlen: 1,
        control: cbuf.as_mut_ptr() as *mut core::ffi::c_void,
        controllen: sys::cmsg_space(fds.len()),
        flags: 0,
        _pad1: 0,
    };
    // SAFETY: as documented above. `sendmsg` reads through the pointers for the
    // duration of the call only, and all three buffers are live locals.
    let n = unsafe { sys::sendmsg(sock.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(Error::Os { call: "sendmsg", errno: crate::region::sys::errno() });
    }
    // A short send of the header would desynchronise the stream, so it is an
    // error rather than something to loop over.
    if (n as usize) != framed.len() {
        return Err(Error::BadColdMessage("short sendmsg"));
    }
    Ok(())
}

/// Receive one message, and every descriptor that came with it.
///
/// # Safety
///
/// One `recvmsg`, with `MSG_CMSG_CLOEXEC` so a descriptor cannot leak through an
/// `exec` between arriving and being wrapped. Each returned fd is wrapped in an
/// [`OwnedFd`] immediately, so it is closed exactly once.
///
/// The first `recvmsg` asks for the four-byte length prefix only. On a
/// `SOCK_STREAM` Unix socket the ancillary data is delivered with the read that
/// consumes the first byte it was attached to, and the sender attaches it to the
/// same `sendmsg` as the prefix — so the descriptors arrive here or not at all.
pub fn recv_with_fds(sock: &UnixStream, out: &mut Vec<u8>) -> Result<Vec<OwnedFd>> {
    let mut lenbuf = [0u8; 4];
    let mut iov = sys::Iovec { base: lenbuf.as_mut_ptr(), len: 4 };
    let mut cbuf = [0u8; sys::CMSG_SPACE_MAX_FDS];
    let mut msg = sys::MsgHdr {
        name: core::ptr::null_mut(),
        namelen: 0,
        _pad0: 0,
        iov: &mut iov,
        iovlen: 1,
        control: cbuf.as_mut_ptr() as *mut core::ffi::c_void,
        controllen: sys::CMSG_SPACE_MAX_FDS,
        flags: 0,
        _pad1: 0,
    };
    // SAFETY: as documented. All buffers are live locals; the kernel writes at
    // most `iov.len` payload bytes and at most `controllen` control bytes.
    let n = unsafe { sys::recvmsg(sock.as_raw_fd(), &mut msg, sys::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(Error::Os { call: "recvmsg", errno: crate::region::sys::errno() });
    }
    if n == 0 {
        return Err(Error::BadColdMessage("peer closed the cold path"));
    }

    let mut fds = Vec::new();
    if msg.controllen >= sys::CMSG_LEN_HDR + 4 {
        let cmsg_len = u64::from_le_bytes(cbuf[0..8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(cbuf[8..12].try_into().unwrap());
        let ty = i32::from_le_bytes(cbuf[12..16].try_into().unwrap());
        if level == sys::SOL_SOCKET && ty == sys::SCM_RIGHTS {
            // How many descriptors the kernel actually delivered, from the
            // header it wrote — not from how many we hoped for.
            let n = cmsg_len.saturating_sub(sys::CMSG_LEN_HDR) / 4;
            for i in 0..n.min(MAX_FDS) {
                let at = 16 + 4 * i;
                let raw = i32::from_le_bytes(cbuf[at..at + 4].try_into().unwrap());
                // SAFETY: the kernel installed `raw` in this process's table and
                // nobody else has it; wrapping it here is the unique owner.
                fds.push(unsafe { OwnedFd::from_raw_fd(raw) });
            }
        }
    }

    let mut have = n as usize;
    let mut s = sock;
    while have < 4 {
        let got = s
            .read(&mut lenbuf[have..])
            .map_err(|e| Error::Os { call: "read", errno: e.raw_os_error().unwrap_or(0) })?;
        if got == 0 {
            return Err(Error::BadColdMessage("truncated cold-path length"));
        }
        have += got;
    }
    let len = u32::from_le_bytes(lenbuf) as usize;
    if len > MAX_COLD_MESSAGE {
        return Err(Error::BadColdMessage("cold-path message is implausibly long"));
    }
    out.clear();
    out.resize(len, 0);
    s.read_exact(out)
        .map_err(|e| Error::Os { call: "read_exact", errno: e.raw_os_error().unwrap_or(0) })?;
    Ok(fds)
}

/// Receive a [`ColdMessage`] and every descriptor it carried.
pub fn recv_all(sock: &UnixStream) -> Result<(ColdMessage, Vec<OwnedFd>)> {
    let mut buf = Vec::new();
    let fds = recv_with_fds(sock, &mut buf)?;
    Ok((ColdMessage::decode(&buf)?, fds))
}

/// The three structures `sendmsg` needs, transcribed for 64-bit Linux.
pub(crate) mod sys {
    use core::ffi::c_void;

    pub const SOL_SOCKET: i32 = 1;
    pub const SCM_RIGHTS: i32 = 1;
    pub const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;

    /// The `cmsghdr` itself: `cmsg_len` (8), `cmsg_level` (4), `cmsg_type` (4).
    pub const CMSG_LEN_HDR: usize = 16;
    /// `CMSG_SPACE(sizeof(int) * MAX_FDS)`. `CMSG_SPACE` rounds the data up to
    /// 8, so one `int` occupies the same 24 bytes two used to.
    pub const CMSG_SPACE_MAX_FDS: usize = 16 + 8;

    /// `CMSG_SPACE(sizeof(int) * n)`.
    pub const fn cmsg_space(n: usize) -> usize {
        CMSG_LEN_HDR + (4 * n).div_ceil(8) * 8
    }

    #[repr(C)]
    pub struct Iovec {
        pub base: *mut u8,
        pub len: usize,
    }

    #[repr(C)]
    pub struct MsgHdr {
        pub name: *mut c_void,
        pub namelen: u32,
        pub _pad0: u32,
        pub iov: *mut Iovec,
        pub iovlen: usize,
        pub control: *mut c_void,
        pub controllen: usize,
        pub flags: i32,
        pub _pad1: i32,
    }

    // The layout is the ABI's, not ours. If either of these ever fails, the
    // transcription above is wrong for the target and the fd handoff would
    // corrupt memory rather than fail — so it is a compile error.
    const _: () = assert!(core::mem::size_of::<MsgHdr>() == 56);
    const _: () = assert!(core::mem::size_of::<Iovec>() == 16);
    const _: () = assert!(core::mem::size_of::<usize>() == 8);

    unsafe extern "C" {
        pub fn sendmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
        pub fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cold::{Attachment, ColdMessage};

    /// The one thing this module is still for: a descriptor for an object that
    /// has no name, on one machine, to one peer.
    #[test]
    #[cfg_attr(miri, ignore = "socketpair and sendmsg are syscalls Miri does not implement")]
    fn one_nameless_descriptor_crosses_and_names_the_same_memory() {
        use crate::region::{SharedRegion, memfd};
        use std::os::fd::AsFd;

        let (a, b) = UnixStream::pair().unwrap();
        let fd = memfd("sv-cold-test", 8192).unwrap();
        let msg = ColdMessage::Attach(Attachment {
            object: "/dev/shm/sv.control".into(),
            // Empty: "the arena has no name, and a descriptor is coming".
            arena_object: String::new(),
            control_offset: 0,
            control_len: 4096,
            arena_offset: 4096,
            arena_len: 4096,
            generation: 1,
            run_count: 0,
        });
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        send_with_fds(&a, &buf, &[fd.as_fd()]).unwrap();

        let (got, fds) = recv_all(&b).unwrap();
        assert_eq!(got, msg);
        assert_eq!(fds.len(), 1);

        // The proof that it is the *same* object and not merely a descriptor: a
        // write through the sender's mapping is visible through the receiver's.
        let here = SharedRegion::map_fd(fd.as_fd(), 0, 4096).unwrap();
        let there = SharedRegion::map_fd(fds[0].as_fd(), 0, 4096).unwrap();
        here.write_bytes(128, b"crossed").unwrap();
        let mut out = [0u8; 7];
        there.read_bytes(128, &mut out).unwrap();
        assert_eq!(&out, b"crossed");
    }

    #[test]
    fn the_control_buffer_holds_exactly_one_descriptor() {
        assert_eq!(sys::cmsg_space(1), sys::CMSG_SPACE_MAX_FDS);
        // Two would still fit in the 24 bytes `CMSG_SPACE` rounds to, and it is
        // `MAX_FDS` rather than the buffer that says one is the limit now.
        assert_eq!(MAX_FDS, 1);
        assert!(sys::cmsg_space(3) > sys::CMSG_SPACE_MAX_FDS);
    }
}
