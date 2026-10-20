//! A test harness, not a render node: the node's [`Device`] behind one file of
//! an unprivileged FUSE mount (`sv-drm fuse-node`, docs/waycompose-design.md
//! §112).
//!
//! `/dev/cuse` is root's, so a test without root cannot make the real node.
//! A FUSE file answers ioctls too: in restricted mode the kernel copies the
//! argument in and out by the size and direction encoded in the number, which
//! is all [`crate::relay::DRM_IOCTL_SVDRM_CONNECT`] needs. So the client runs
//! its real code (open, socketpair, `ioctl`) and `sv-drm` its real handler;
//! what differs from `serve` is the file's type (a regular file, not a
//! character device, so the dma-buf path stays off) and that `pidfd_getfd`
//! runs without root, which Yama's scope 1 allows only for descendants:
//! `fuse-node` therefore runs the clients as its children.
//!
//! The filesystem has a root directory and one file. Everything else
//! (`OPEN`, `IOCTL`, `RELEASE`, …) is the CUSE [`Server`]'s.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::cuse::{
    Device, FUSE_INIT, FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, Handler, InHeader, MIN_MINOR, Server, Stop,
    Wire, Wr, errno_reply, ok_reply,
};

pub const FUSE_LOOKUP: u32 = 1;
pub const FUSE_GETATTR: u32 = 3;
pub const FUSE_STATFS: u32 = 17;
pub const FUSE_OPENDIR: u32 = 27;
pub const FUSE_READDIR: u32 = 28;
pub const FUSE_RELEASEDIR: u32 = 29;
pub const FUSE_ACCESS: u32 = 34;

const ROOT: u64 = 1;
const FILE: u64 = 2;

/// The filesystem around a CUSE [`Server`].
pub struct FsNode<D: Device> {
    pub server: Server<D>,
    /// The one file's name.
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    stop: Option<Stop>,
}

impl<D: Device> FsNode<D> {
    pub fn new(server: Server<D>, name: &str, uid: u32, gid: u32) -> Self {
        FsNode { server, name: name.to_string(), uid, gid, stop: None }
    }

    /// `struct fuse_attr`, 88 bytes.
    fn attr(&self, node: u64) -> Vec<u8> {
        let (mode, nlink) = if node == ROOT { (libc::S_IFDIR | 0o700, 2) } else { (libc::S_IFREG | 0o600, 1) };
        let mut w = Wr::default();
        w.u64(node).u64(0).u64(0).u64(0).u64(0).u64(0); // ino size blocks atime mtime ctime
        w.u32(0).u32(0).u32(0); // nsecs
        w.u32(mode).u32(nlink).u32(self.uid).u32(self.gid).u32(0).u32(4096).u32(0);
        debug_assert_eq!(w.0.len(), 88);
        w.0
    }

    fn init(&mut self, unique: u64, body: &[u8]) -> Vec<u8> {
        let mut r = crate::cuse::Rd::new(body);
        let (Some(major), Some(minor), Some(readahead)) = (r.u32(), r.u32(), r.u32()) else {
            return errno_reply(unique, libc::EINVAL);
        };
        if major != FUSE_KERNEL_VERSION || minor < MIN_MINOR {
            self.stop = Some(Stop::InitFailed(format!("kernel offers FUSE {major}.{minor}")));
            return errno_reply(unique, libc::EPROTO);
        }
        let minor = minor.min(FUSE_KERNEL_MINOR_VERSION);
        self.server.minor = Some(minor);
        // `struct fuse_init_out`, 64 bytes.
        let mut w = Wr::default();
        w.u32(FUSE_KERNEL_VERSION).u32(minor).u32(readahead).u32(0);
        w.u16(16).u16(12).u32(4096).u32(1); // max_background, congestion, max_write, time_gran
        w.u16(0).u16(0).u32(0).u32(0).u16(0); // max_pages, map_alignment, flags2, max_stack_depth, timeout
        w.bytes(&[0; 22]);
        debug_assert_eq!(w.0.len(), 64);
        ok_reply(unique, &w.0)
    }
}

impl<D: Device> Handler for FsNode<D> {
    fn handle(&mut self, msg: &[u8]) -> Option<Vec<u8>> {
        let hdr = InHeader::parse(msg)?;
        let unique = hdr.unique;
        if hdr.len as usize != msg.len() {
            return Some(errno_reply(unique, libc::EINVAL));
        }
        let ext = hdr.total_extlen as usize * 8;
        let body = msg.get(InHeader::SIZE..msg.len().saturating_sub(ext)).unwrap_or(&[]);
        match hdr.opcode {
            FUSE_INIT => Some(self.init(unique, body)),
            _ if self.server.minor.is_none() => Some(errno_reply(unique, libc::EIO)),
            FUSE_LOOKUP => {
                let name = body.split(|&b| b == 0).next().unwrap_or(&[]);
                if hdr.nodeid != ROOT || name != self.name.as_bytes() {
                    return Some(errno_reply(unique, libc::ENOENT));
                }
                // `struct fuse_entry_out`: nodeid, generation, entry_valid,
                // attr_valid, their nsecs, then the attributes.
                let mut w = Wr::default();
                w.u64(FILE).u64(1).u64(1).u64(1).u32(0).u32(0);
                w.bytes(&self.attr(FILE));
                Some(ok_reply(unique, &w.0))
            }
            FUSE_GETATTR => {
                if hdr.nodeid != ROOT && hdr.nodeid != FILE {
                    return Some(errno_reply(unique, libc::ENOENT));
                }
                // `struct fuse_attr_out`: attr_valid, its nsec, dummy, attr.
                let mut w = Wr::default();
                w.u64(1).u32(0).u32(0);
                w.bytes(&self.attr(hdr.nodeid));
                Some(ok_reply(unique, &w.0))
            }
            crate::cuse::FUSE_OPEN if hdr.nodeid != FILE => Some(errno_reply(unique, libc::EISDIR)),
            FUSE_OPENDIR => {
                let mut w = Wr::default();
                w.u64(0).u32(0).i32(0);
                Some(ok_reply(unique, &w.0))
            }
            FUSE_READDIR => Some(ok_reply(unique, &[])),
            FUSE_RELEASEDIR | FUSE_ACCESS => Some(ok_reply(unique, &[])),
            FUSE_STATFS => Some(errno_reply(unique, libc::ENOSYS)),
            _ => self.server.handle(msg),
        }
    }

    fn take_stop(&mut self) -> Option<Stop> {
        self.stop.take().or_else(|| self.server.stop())
    }
}

/// Mount a FUSE filesystem at `mountpoint` with `fusermount3` (setuid, so no
/// root here), `auto_unmount`: it is unmounted when this process ends.
/// Returns `/dev/fuse`'s descriptor and the connection `fusermount3` watches,
/// which must stay open as long as the mount should.
pub fn mount(mountpoint: &Path) -> io::Result<(OwnedFd, UnixStream)> {
    use std::os::unix::process::CommandExt;
    let (ours, theirs) = UnixStream::pair()?;
    let raw = theirs.as_raw_fd();
    // NixOS keeps the setuid copy in /run/wrappers; elsewhere PATH has it.
    let wrapped = Path::new("/run/wrappers/bin/fusermount3");
    let helper = if wrapped.exists() { wrapped } else { Path::new("fusermount3") };
    let mut cmd = std::process::Command::new(helper);
    cmd.env("_FUSE_COMMFD", raw.to_string())
        .args(["-o", "fsname=svdrm,subtype=svdrm,auto_unmount", "--"])
        .arg(mountpoint);
    // SAFETY: only fcntl between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::fcntl(raw, libc::F_SETFD, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // With auto_unmount, fusermount3 stays to watch the connection, so it is
    // not waited for here; a thread reaps it when it ends.
    let mut child = cmd.spawn()?;
    drop(theirs);
    // The descriptor comes back as SCM_RIGHTS with one byte of data.
    let mut data = [0u8; 1];
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: 1 };
    let mut cbuf = [0u64; 4];
    // SAFETY: a zeroed msghdr pointing at live locals.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    // SAFETY: as above.
    let n = unsafe { libc::recvmsg(ours.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n <= 0 {
        let e = io::Error::last_os_error();
        let st = child.wait()?;
        return Err(io::Error::other(format!("fusermount3 sent no descriptor ({st}, {e})")));
    }
    std::thread::spawn(move || child.wait());
    // SAFETY: reading the control message the kernel wrote.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            let fd = (libc::CMSG_DATA(c) as *const i32).read_unaligned();
            return Ok((OwnedFd::from_raw_fd(fd), ours));
        }
    }
    Err(io::Error::other("fusermount3 sent no descriptor"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cuse::tests::req;
    use crate::cuse::{DevInfo, OutHeader};
    use crate::drm::{DrmNode, ProcResolver};

    fn fs() -> FsNode<DrmNode<ProcResolver>> {
        let s = Server::new(DrmNode::new(ProcResolver), DevInfo { devname: "x".into(), dev_major: 0, dev_minor: 128 });
        FsNode::new(s, "renderD128", 1000, 100)
    }

    #[test]
    fn init_lookup_getattr_open() {
        let mut f = fs();
        let mut init = Wr::default();
        init.u32(7).u32(41).u32(65536).u32(0).bytes(&[0; 48]);
        let r = f.handle(&req(FUSE_INIT, 1, 0, &init.0)).unwrap();
        assert_eq!(r.len(), 16 + 64);
        assert_eq!(OutHeader::parse(&r).unwrap().error, 0);
        assert_eq!(u32::from_ne_bytes(r[20..24].try_into().unwrap()), 41);

        let mut lk = req(FUSE_LOOKUP, 2, 5, b"renderD128\0");
        lk[16..24].copy_from_slice(&ROOT.to_ne_bytes());
        let r = f.handle(&lk).unwrap();
        assert_eq!(r.len(), 16 + 40 + 88);
        assert_eq!(u64::from_ne_bytes(r[16..24].try_into().unwrap()), FILE);
        let mode = u32::from_ne_bytes(r[16 + 40 + 60..16 + 40 + 64].try_into().unwrap());
        assert_eq!(mode & libc::S_IFMT, libc::S_IFREG);

        let mut lk = req(FUSE_LOOKUP, 3, 5, b"other\0");
        lk[16..24].copy_from_slice(&ROOT.to_ne_bytes());
        assert_eq!(OutHeader::parse(&f.handle(&lk).unwrap()).unwrap().error, -libc::ENOENT);

        let mut ga = req(FUSE_GETATTR, 4, 5, &[0; 16]);
        ga[16..24].copy_from_slice(&ROOT.to_ne_bytes());
        assert_eq!(f.handle(&ga).unwrap().len(), 16 + 16 + 88);

        let mut op = req(crate::cuse::FUSE_OPEN, 5, 5, &[0; 8]);
        op[16..24].copy_from_slice(&FILE.to_ne_bytes());
        let r = f.handle(&op).unwrap();
        assert_eq!(OutHeader::parse(&r).unwrap().error, 0, "OPEN is the node's");
        assert_eq!(f.server.dev.files.len(), 1);
    }
}
