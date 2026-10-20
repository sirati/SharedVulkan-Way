//! A minimal CUSE server: the FUSE kernel protocol over `/dev/cuse`, spoken
//! directly (docs/waycompose-design.md §104).
//!
//! The kernel side is `fs/fuse/cuse.c` + `fs/fuse/ioctl.c`; the wire structs are
//! the ones in `include/uapi/linux/fuse.h`, redefined here (all native endian,
//! no padding but what the header has). One read of the device returns exactly
//! one request; one write is exactly one reply, whose `len` must equal the bytes
//! written.
//!
//! The protocol logic is [`Server::handle`]: request bytes in, reply bytes out.
//! [`serve`] is only the loop around it, over any descriptor that keeps message
//! boundaries (`/dev/cuse`, or a `SOCK_SEQPACKET` socketpair in the tests).
//!
//! Ioctls are unrestricted (`CUSE_UNRESTRICTED_IOCTL`): the kernel does not know
//! the argument layout, so each ioctl first arrives with no data, and the
//! [`Device`] answers [`IoctlReply::Retry`] naming the user memory it wants to
//! read (`in`) and write (`out`); the kernel re-issues the request with the `in`
//! bytes, and copies the final reply's data into the `out` ranges in order.
//! Structs with embedded pointers (`DRM_IOCTL_VERSION`) take a second retry once
//! the pointers are known.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

/// The FUSE major version; the kernel refuses CUSE_INIT replies with another.
pub const FUSE_KERNEL_VERSION: u32 = 7;
/// The newest minor we claim. We reply `min(kernel's, this)`.
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 45;
/// Below 16 the retry iovecs are `struct iovec` instead of `fuse_ioctl_iovec`;
/// the kernel itself requires 11. We only speak the 16+ format.
pub const MIN_MINOR: u32 = 16;

pub const FUSE_FORGET: u32 = 2;
pub const FUSE_OPEN: u32 = 14;
pub const FUSE_READ: u32 = 15;
pub const FUSE_WRITE: u32 = 16;
pub const FUSE_RELEASE: u32 = 18;
pub const FUSE_FLUSH: u32 = 25;
pub const FUSE_INIT: u32 = 26;
pub const FUSE_INTERRUPT: u32 = 36;
pub const FUSE_DESTROY: u32 = 38;
pub const FUSE_IOCTL: u32 = 39;
pub const FUSE_POLL: u32 = 40;
pub const FUSE_BATCH_FORGET: u32 = 42;
pub const CUSE_INIT: u32 = 4096;

pub const CUSE_UNRESTRICTED_IOCTL: u32 = 1 << 0;

pub const FUSE_IOCTL_COMPAT: u32 = 1 << 0;
pub const FUSE_IOCTL_UNRESTRICTED: u32 = 1 << 1;
pub const FUSE_IOCTL_RETRY: u32 = 1 << 2;
pub const FUSE_IOCTL_32BIT: u32 = 1 << 3;
pub const FUSE_IOCTL_MAX_IOV: usize = 256;

/// The kernel's `CUSE_INIT_INFO_MAX`: the devinfo string's room.
pub const CUSE_INIT_INFO_MAX: usize = 4096;
/// `FUSE_MIN_READ_BUFFER`; our buffer is far larger (see [`READ_BUFFER`]).
pub const FUSE_MIN_READ_BUFFER: usize = 8192;
/// What we tell the kernel in `max_read`/`max_write` (it raises both to at
/// least 4096; we do neither).
pub const MAX_RW: u32 = 4096;
/// Read buffer: room for the largest ioctl input the kernel may send (it caps at
/// `max_pages` = 32 pages by default, 256 at most) plus headers.
pub const READ_BUFFER: usize = 256 * 4096 + 4096;

/// `struct fuse_in_header`, 40 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub total_extlen: u16,
    pub padding: u16,
}

/// `struct fuse_out_header`, 16 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutHeader {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

/// `struct cuse_init_in`, 16 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CuseInitIn {
    pub major: u32,
    pub minor: u32,
    pub unused: u32,
    pub flags: u32,
}

/// `struct cuse_init_out`, 72 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CuseInitOut {
    pub major: u32,
    pub minor: u32,
    pub unused: u32,
    pub flags: u32,
    pub max_read: u32,
    pub max_write: u32,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub spare: [u32; 10],
}

/// `struct fuse_open_in`, 8 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenIn {
    pub flags: u32,
    pub open_flags: u32,
}

/// `struct fuse_open_out`, 16 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenOut {
    pub fh: u64,
    pub open_flags: u32,
    pub backing_id: i32,
}

/// `struct fuse_ioctl_in`, 32 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoctlIn {
    pub fh: u64,
    pub flags: u32,
    pub cmd: u32,
    pub arg: u64,
    pub in_size: u32,
    pub out_size: u32,
}

impl IoctlIn {
    /// A 32-bit caller (compat ioctl on a 64-bit kernel, or a 32-bit kernel):
    /// pointers and `size_t` in the argument are four bytes.
    pub fn is_32bit(&self) -> bool {
        self.flags & FUSE_IOCTL_32BIT != 0
    }
}

/// `struct fuse_ioctl_out`, 16 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoctlOut {
    pub result: i32,
    pub flags: u32,
    pub in_iovs: u32,
    pub out_iovs: u32,
}

/// `struct fuse_ioctl_iovec`, 16 bytes: a range of the caller's memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoVec {
    pub base: u64,
    pub len: u64,
}

/// `struct fuse_release_in`, 24 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReleaseIn {
    pub fh: u64,
    pub flags: u32,
    pub release_flags: u32,
    pub lock_owner: u64,
}

/// `struct fuse_poll_in`, 24 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollIn {
    pub fh: u64,
    pub kh: u64,
    pub flags: u32,
    pub events: u32,
}

/// `struct fuse_poll_out`, 8 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollOut {
    pub revents: u32,
    pub padding: u32,
}

/// `struct fuse_interrupt_in`, 8 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterruptIn {
    pub unique: u64,
}

/// Little native-endian reader over a byte slice.
pub struct Rd<'a> {
    b: &'a [u8],
    o: usize,
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Rd { b, o: 0 }
    }
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let s = self.b.get(self.o..self.o + N)?;
        self.o += N;
        Some(s.try_into().unwrap())
    }
    pub fn u16(&mut self) -> Option<u16> {
        self.take().map(u16::from_ne_bytes)
    }
    pub fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_ne_bytes)
    }
    pub fn i32(&mut self) -> Option<i32> {
        self.take().map(i32::from_ne_bytes)
    }
    pub fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_ne_bytes)
    }
    pub fn rest(&self) -> &'a [u8] {
        &self.b[self.o.min(self.b.len())..]
    }
}

/// Native-endian writer.
#[derive(Default)]
pub struct Wr(pub Vec<u8>);

impl Wr {
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
}

/// Wire encoding of the protocol structs.
pub trait Wire: Sized {
    const SIZE: usize;
    fn read(r: &mut Rd<'_>) -> Option<Self>;
    fn write(&self, w: &mut Wr);
    fn parse(b: &[u8]) -> Option<Self> {
        Self::read(&mut Rd::new(b))
    }
    fn to_bytes(&self) -> Vec<u8> {
        let mut w = Wr::default();
        self.write(&mut w);
        debug_assert_eq!(w.0.len(), Self::SIZE);
        w.0
    }
}

impl Wire for InHeader {
    const SIZE: usize = 40;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(InHeader {
            len: r.u32()?,
            opcode: r.u32()?,
            unique: r.u64()?,
            nodeid: r.u64()?,
            uid: r.u32()?,
            gid: r.u32()?,
            pid: r.u32()?,
            total_extlen: r.u16()?,
            padding: r.u16()?,
        })
    }
    fn write(&self, w: &mut Wr) {
        w.u32(self.len).u32(self.opcode).u64(self.unique).u64(self.nodeid);
        w.u32(self.uid).u32(self.gid).u32(self.pid).u16(self.total_extlen).u16(self.padding);
    }
}

impl Wire for OutHeader {
    const SIZE: usize = 16;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(OutHeader { len: r.u32()?, error: r.i32()?, unique: r.u64()? })
    }
    fn write(&self, w: &mut Wr) {
        w.u32(self.len).i32(self.error).u64(self.unique);
    }
}

impl Wire for CuseInitIn {
    const SIZE: usize = 16;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(CuseInitIn { major: r.u32()?, minor: r.u32()?, unused: r.u32()?, flags: r.u32()? })
    }
    fn write(&self, w: &mut Wr) {
        w.u32(self.major).u32(self.minor).u32(self.unused).u32(self.flags);
    }
}

impl Wire for CuseInitOut {
    const SIZE: usize = 72;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        let mut o = CuseInitOut {
            major: r.u32()?,
            minor: r.u32()?,
            unused: r.u32()?,
            flags: r.u32()?,
            max_read: r.u32()?,
            max_write: r.u32()?,
            dev_major: r.u32()?,
            dev_minor: r.u32()?,
            spare: [0; 10],
        };
        for s in &mut o.spare {
            *s = r.u32()?;
        }
        Some(o)
    }
    fn write(&self, w: &mut Wr) {
        w.u32(self.major).u32(self.minor).u32(self.unused).u32(self.flags);
        w.u32(self.max_read).u32(self.max_write).u32(self.dev_major).u32(self.dev_minor);
        for s in self.spare {
            w.u32(s);
        }
    }
}

impl Wire for OpenIn {
    const SIZE: usize = 8;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(OpenIn { flags: r.u32()?, open_flags: r.u32()? })
    }
    fn write(&self, w: &mut Wr) {
        w.u32(self.flags).u32(self.open_flags);
    }
}

impl Wire for OpenOut {
    const SIZE: usize = 16;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(OpenOut { fh: r.u64()?, open_flags: r.u32()?, backing_id: r.i32()? })
    }
    fn write(&self, w: &mut Wr) {
        w.u64(self.fh).u32(self.open_flags).i32(self.backing_id);
    }
}

impl Wire for IoctlIn {
    const SIZE: usize = 32;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(IoctlIn {
            fh: r.u64()?,
            flags: r.u32()?,
            cmd: r.u32()?,
            arg: r.u64()?,
            in_size: r.u32()?,
            out_size: r.u32()?,
        })
    }
    fn write(&self, w: &mut Wr) {
        w.u64(self.fh).u32(self.flags).u32(self.cmd).u64(self.arg).u32(self.in_size).u32(self.out_size);
    }
}

impl Wire for IoctlOut {
    const SIZE: usize = 16;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(IoctlOut { result: r.i32()?, flags: r.u32()?, in_iovs: r.u32()?, out_iovs: r.u32()? })
    }
    fn write(&self, w: &mut Wr) {
        w.i32(self.result).u32(self.flags).u32(self.in_iovs).u32(self.out_iovs);
    }
}

impl Wire for IoVec {
    const SIZE: usize = 16;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(IoVec { base: r.u64()?, len: r.u64()? })
    }
    fn write(&self, w: &mut Wr) {
        w.u64(self.base).u64(self.len);
    }
}

impl Wire for ReleaseIn {
    const SIZE: usize = 24;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(ReleaseIn { fh: r.u64()?, flags: r.u32()?, release_flags: r.u32()?, lock_owner: r.u64()? })
    }
    fn write(&self, w: &mut Wr) {
        w.u64(self.fh).u32(self.flags).u32(self.release_flags).u64(self.lock_owner);
    }
}

impl Wire for PollIn {
    const SIZE: usize = 24;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(PollIn { fh: r.u64()?, kh: r.u64()?, flags: r.u32()?, events: r.u32()? })
    }
    fn write(&self, w: &mut Wr) {
        w.u64(self.fh).u64(self.kh).u32(self.flags).u32(self.events);
    }
}

impl Wire for PollOut {
    const SIZE: usize = 8;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(PollOut { revents: r.u32()?, padding: r.u32()? })
    }
    fn write(&self, w: &mut Wr) {
        w.u32(self.revents).u32(self.padding);
    }
}

impl Wire for InterruptIn {
    const SIZE: usize = 8;
    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(InterruptIn { unique: r.u64()? })
    }
    fn write(&self, w: &mut Wr) {
        w.u64(self.unique);
    }
}

/// What a [`Device`] answers to one ioctl round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IoctlReply {
    /// Fail with this errno (positive).
    Error(i32),
    /// Ask the kernel to come back with the `input` ranges' bytes as data, and
    /// room for the `output` ranges.
    Retry { input: Vec<IoVec>, output: Vec<IoVec> },
    /// Done: the ioctl returns `result`; `data` is copied into the output ranges
    /// of the last retry, in order. Must not exceed the request's `out_size`.
    Done { result: i32, data: Vec<u8> },
}

/// The character device behind the CUSE node.
pub trait Device {
    /// An `open(2)` of the node. Returns the file handle the kernel quotes back
    /// in every later request on that open file, or an errno.
    fn open(&mut self, hdr: &InHeader, arg: &OpenIn) -> Result<u64, i32>;
    /// The last close of an open file.
    fn release(&mut self, fh: u64);
    /// One round of an ioctl; `data` is the `in_size` bytes the kernel copied
    /// from the ranges of the previous [`IoctlReply::Retry`] (empty at first).
    fn ioctl(&mut self, hdr: &InHeader, arg: &IoctlIn, data: &[u8]) -> IoctlReply;
    /// `poll(2)`: the ready events. Default: nothing is ever ready (a render
    /// node with no pending DRM events answers the same).
    fn poll(&mut self, _hdr: &InHeader, _arg: &PollIn) -> u32 {
        0
    }
}

/// What the CUSE_INIT reply announces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevInfo {
    /// `DEVNAME`, relative to /dev, e.g. `dri/renderD128`.
    pub devname: String,
    /// 0: let the kernel pick a free major.
    pub dev_major: u32,
    pub dev_minor: u32,
}

/// Why the loop ended.
#[derive(Debug)]
pub enum Stop {
    /// The kernel closed the channel (`ENODEV`), or the peer hung up (EOF).
    Closed,
    /// FUSE_DESTROY.
    Destroyed,
    /// CUSE_INIT refused (version), the server cannot run.
    InitFailed(String),
}

/// The protocol state: request bytes in, reply bytes out.
pub struct Server<D: Device> {
    pub dev: D,
    pub info: DevInfo,
    /// The minor version agreed in CUSE_INIT, once it was.
    pub minor: Option<u32>,
    stop: Option<Stop>,
}

pub(crate) fn errno_reply(unique: u64, errno: i32) -> Vec<u8> {
    OutHeader { len: OutHeader::SIZE as u32, error: -errno, unique }.to_bytes()
}

pub(crate) fn ok_reply(unique: u64, body: &[u8]) -> Vec<u8> {
    let mut w = Wr::default();
    OutHeader { len: (OutHeader::SIZE + body.len()) as u32, error: 0, unique }.write(&mut w);
    w.bytes(body);
    w.0
}

impl<D: Device> Server<D> {
    pub fn new(dev: D, info: DevInfo) -> Self {
        Server { dev, info, minor: None, stop: None }
    }

    /// Set once the loop must end; the reply (if any) is still sent.
    pub fn stop(&mut self) -> Option<Stop> {
        self.stop.take()
    }

    pub fn initialized(&self) -> bool {
        self.minor.is_some()
    }

    /// Handle one request; `None` for requests that take no reply (FORGET,
    /// INTERRUPT) or that are malformed beyond a header.
    pub fn handle(&mut self, msg: &[u8]) -> Option<Vec<u8>> {
        let hdr = InHeader::parse(msg)?;
        let unique = hdr.unique;
        if (hdr.len as usize) != msg.len() {
            return Some(errno_reply(unique, libc::EINVAL));
        }
        // Extensions (security contexts etc.) follow the argument; CUSE has
        // none, but skip them from the end if a kernel ever sends any.
        let ext = hdr.total_extlen as usize * 8;
        let body = msg.get(InHeader::SIZE..msg.len().saturating_sub(ext)).unwrap_or(&[]);
        match hdr.opcode {
            CUSE_INIT => Some(self.init(unique, body)),
            _ if self.minor.is_none() => Some(errno_reply(unique, libc::EIO)),
            FUSE_OPEN => {
                let Some(a) = OpenIn::parse(body) else { return Some(errno_reply(unique, libc::EINVAL)) };
                Some(match self.dev.open(&hdr, &a) {
                    Ok(fh) => ok_reply(unique, &OpenOut { fh, open_flags: 0, backing_id: 0 }.to_bytes()),
                    Err(e) => errno_reply(unique, e),
                })
            }
            FUSE_RELEASE => {
                let Some(a) = ReleaseIn::parse(body) else { return Some(errno_reply(unique, libc::EINVAL)) };
                self.dev.release(a.fh);
                Some(ok_reply(unique, &[]))
            }
            FUSE_IOCTL => {
                let Some(a) = IoctlIn::parse(body) else { return Some(errno_reply(unique, libc::EINVAL)) };
                let data = &body[IoctlIn::SIZE..];
                if data.len() != a.in_size as usize {
                    return Some(errno_reply(unique, libc::EINVAL));
                }
                Some(self.ioctl(&hdr, &a, data))
            }
            FUSE_POLL => {
                let Some(a) = PollIn::parse(body) else { return Some(errno_reply(unique, libc::EINVAL)) };
                let revents = self.dev.poll(&hdr, &a);
                Some(ok_reply(unique, &PollOut { revents, padding: 0 }.to_bytes()))
            }
            FUSE_READ | FUSE_WRITE => Some(errno_reply(unique, libc::EINVAL)),
            FUSE_FLUSH => Some(ok_reply(unique, &[])),
            // We answer every request before reading the next, so whatever an
            // interrupt names is already answered. No reply is expected.
            FUSE_INTERRUPT | FUSE_FORGET | FUSE_BATCH_FORGET => None,
            FUSE_DESTROY => {
                self.stop = Some(Stop::Destroyed);
                Some(ok_reply(unique, &[]))
            }
            _ => Some(errno_reply(unique, libc::ENOSYS)),
        }
    }

    fn init(&mut self, unique: u64, body: &[u8]) -> Vec<u8> {
        let Some(a) = CuseInitIn::parse(body) else { return errno_reply(unique, libc::EINVAL) };
        if a.major != FUSE_KERNEL_VERSION || a.minor < MIN_MINOR {
            self.stop = Some(Stop::InitFailed(format!(
                "kernel offers FUSE {}.{}, need {}.{}+",
                a.major, a.minor, FUSE_KERNEL_VERSION, MIN_MINOR
            )));
            return errno_reply(unique, libc::EPROTO);
        }
        if a.flags & CUSE_UNRESTRICTED_IOCTL == 0 {
            self.stop = Some(Stop::InitFailed("kernel does not offer unrestricted ioctls".into()));
            return errno_reply(unique, libc::EPROTO);
        }
        let minor = a.minor.min(FUSE_KERNEL_MINOR_VERSION);
        self.minor = Some(minor);
        let out = CuseInitOut {
            major: FUSE_KERNEL_VERSION,
            minor,
            unused: 0,
            flags: CUSE_UNRESTRICTED_IOCTL,
            max_read: MAX_RW,
            max_write: MAX_RW,
            dev_major: self.info.dev_major,
            dev_minor: self.info.dev_minor,
            spare: [0; 10],
        };
        let mut body = out.to_bytes();
        body.extend_from_slice(&devinfo_bytes(&self.info.devname));
        ok_reply(unique, &body)
    }

    fn ioctl(&mut self, hdr: &InHeader, a: &IoctlIn, data: &[u8]) -> Vec<u8> {
        let unique = hdr.unique;
        match self.dev.ioctl(hdr, a, data) {
            IoctlReply::Error(e) => errno_reply(unique, e),
            IoctlReply::Retry { input, output } => {
                if a.flags & FUSE_IOCTL_UNRESTRICTED == 0 {
                    return errno_reply(unique, libc::EIO);
                }
                if input.len() + output.len() > FUSE_IOCTL_MAX_IOV {
                    return errno_reply(unique, libc::ENOMEM);
                }
                let out = IoctlOut {
                    result: 0,
                    flags: FUSE_IOCTL_RETRY,
                    in_iovs: input.len() as u32,
                    out_iovs: output.len() as u32,
                };
                let mut w = Wr::default();
                out.write(&mut w);
                for v in input.iter().chain(output.iter()) {
                    v.write(&mut w);
                }
                ok_reply(unique, &w.0)
            }
            IoctlReply::Done { result, data } => {
                if data.len() > a.out_size as usize {
                    eprintln!(
                        "sv-drm: ioctl {:#x}: reply of {} bytes exceeds out_size {} (bug)",
                        a.cmd,
                        data.len(),
                        a.out_size
                    );
                    return errno_reply(unique, libc::EIO);
                }
                let mut body = IoctlOut { result, flags: 0, in_iovs: 0, out_iovs: 0 }.to_bytes();
                body.extend_from_slice(&data);
                ok_reply(unique, &body)
            }
        }
    }
}

/// The CUSE_INIT devinfo: NUL-separated `KEY=VALUE` strings, NUL-terminated.
pub fn devinfo_bytes(devname: &str) -> Vec<u8> {
    let mut v = format!("DEVNAME={devname}").into_bytes();
    v.push(0);
    debug_assert!(v.len() <= CUSE_INIT_INFO_MAX);
    v
}

/// A devname the kernel accepts and devtmpfs places where we expect: relative,
/// no empty or dot components, nothing odd.
pub fn devname_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./".contains(&b))
        && name.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Request bytes in, reply bytes out: [`Server`], or a filesystem around one
/// ([`crate::fusenode`]).
pub trait Handler {
    fn handle(&mut self, msg: &[u8]) -> Option<Vec<u8>>;
    /// Set once the loop must end.
    fn take_stop(&mut self) -> Option<Stop>;
}

impl<D: Device> Handler for Server<D> {
    fn handle(&mut self, msg: &[u8]) -> Option<Vec<u8>> {
        Server::handle(self, msg)
    }
    fn take_stop(&mut self) -> Option<Stop> {
        self.stop()
    }
}

/// Run `server` over `fd` until the channel closes, the kernel destroys the
/// connection, or `wait` says to stop. `wait` blocks until `fd` is readable
/// (`Ok(true)`) or the server should stop (`Ok(false)`, e.g. on SIGTERM).
/// `after` runs after each answered request (the CUSE_INIT reply is what makes
/// the kernel create the node, so node setup goes there); its error ends the
/// loop.
pub fn serve<H: Handler>(
    fd: BorrowedFd<'_>,
    server: &mut H,
    mut wait: impl FnMut() -> io::Result<bool>,
    mut after: impl FnMut(&mut H) -> io::Result<()>,
) -> io::Result<Stop> {
    let mut buf = vec![0u8; READ_BUFFER];
    loop {
        if !wait()? {
            return Ok(Stop::Closed);
        }
        // SAFETY: the buffer is valid for its length.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                // Interrupted, or the request was interrupted before we read it.
                Some(libc::EINTR) | Some(libc::EAGAIN) | Some(libc::ENOENT) => continue,
                Some(libc::ENODEV) => return Ok(Stop::Closed),
                _ => return Err(e),
            }
        }
        if n == 0 {
            return Ok(Stop::Closed);
        }
        if let Some(reply) = server.handle(&buf[..n as usize]) {
            // SAFETY: the reply is valid for its length.
            let w = unsafe { libc::write(fd.as_raw_fd(), reply.as_ptr().cast(), reply.len()) };
            if w < 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    // The request was aborted or interrupted meanwhile: nothing
                    // is waiting for this reply any more.
                    Some(libc::ENOENT) | Some(libc::EINTR) => {}
                    Some(libc::ENODEV) => return Ok(Stop::Closed),
                    _ => return Err(e),
                }
            }
        }
        if let Some(s) = server.take_stop() {
            return Ok(s);
        }
        after(server)?;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A request as the kernel lays it out.
    pub fn req(opcode: u32, unique: u64, pid: u32, body: &[u8]) -> Vec<u8> {
        req_as(opcode, unique, pid, 1000, body)
    }

    /// [`req`] from a caller of uid `uid`.
    pub fn req_as(opcode: u32, unique: u64, pid: u32, uid: u32, body: &[u8]) -> Vec<u8> {
        let h = InHeader {
            len: (InHeader::SIZE + body.len()) as u32,
            opcode,
            unique,
            nodeid: 0,
            uid,
            gid: 100,
            pid,
            total_extlen: 0,
            padding: 0,
        };
        let mut v = h.to_bytes();
        v.extend_from_slice(body);
        v
    }

    pub fn init_req(unique: u64, minor: u32) -> Vec<u8> {
        req(
            CUSE_INIT,
            unique,
            0,
            &CuseInitIn { major: 7, minor, unused: 0, flags: CUSE_UNRESTRICTED_IOCTL }.to_bytes(),
        )
    }

    pub fn ioctl_req(unique: u64, pid: u32, fh: u64, cmd: u32, arg: u64, data: &[u8], out_size: u32) -> Vec<u8> {
        let a = IoctlIn {
            fh,
            flags: FUSE_IOCTL_UNRESTRICTED,
            cmd,
            arg,
            in_size: data.len() as u32,
            out_size,
        };
        let mut body = a.to_bytes();
        body.extend_from_slice(data);
        req(FUSE_IOCTL, unique, pid, &body)
    }

    /// A decoded ioctl reply.
    #[derive(Debug, PartialEq, Eq)]
    pub enum Got {
        Err(i32),
        Retry(Vec<IoVec>, Vec<IoVec>),
        Done(i32, Vec<u8>),
    }

    pub fn decode_ioctl_reply(b: &[u8], unique: u64) -> Got {
        let h = OutHeader::parse(b).unwrap();
        assert_eq!(h.len as usize, b.len(), "len matches the bytes written");
        assert_eq!(h.unique, unique);
        if h.error != 0 {
            assert_eq!(b.len(), OutHeader::SIZE, "an error reply is the header alone");
            return Got::Err(-h.error);
        }
        let o = IoctlOut::parse(&b[16..]).unwrap();
        let rest = &b[32..];
        if o.flags & FUSE_IOCTL_RETRY != 0 {
            assert_eq!(rest.len(), (o.in_iovs + o.out_iovs) as usize * 16, "kernel insists on the exact size");
            let v: Vec<IoVec> = rest.chunks(16).map(|c| IoVec::parse(c).unwrap()).collect();
            let (i, o2) = v.split_at(o.in_iovs as usize);
            Got::Retry(i.to_vec(), o2.to_vec())
        } else {
            Got::Done(o.result, rest.to_vec())
        }
    }

    struct Echo {
        opened: Vec<u64>,
        released: Vec<u64>,
    }

    impl Device for Echo {
        fn open(&mut self, hdr: &InHeader, _a: &OpenIn) -> Result<u64, i32> {
            self.opened.push(hdr.pid as u64);
            Ok(hdr.pid as u64 + 100)
        }
        fn release(&mut self, fh: u64) {
            self.released.push(fh);
        }
        fn ioctl(&mut self, _h: &InHeader, a: &IoctlIn, data: &[u8]) -> IoctlReply {
            if data.is_empty() {
                IoctlReply::Retry {
                    input: vec![IoVec { base: a.arg, len: 4 }],
                    output: vec![IoVec { base: a.arg, len: 4 }],
                }
            } else {
                IoctlReply::Done { result: 0, data: data.iter().map(|b| b + 1).collect() }
            }
        }
    }

    fn server() -> Server<Echo> {
        Server::new(
            Echo { opened: vec![], released: vec![] },
            DevInfo { devname: "dri/renderD128".into(), dev_major: 0, dev_minor: 128 },
        )
    }

    #[test]
    fn struct_sizes_match_fuse_h() {
        assert_eq!(InHeader::default().to_bytes().len(), 40);
        assert_eq!(OutHeader::default().to_bytes().len(), 16);
        assert_eq!(CuseInitIn::default().to_bytes().len(), 16);
        assert_eq!(CuseInitOut::default().to_bytes().len(), 72);
        assert_eq!(OpenIn::default().to_bytes().len(), 8);
        assert_eq!(OpenOut::default().to_bytes().len(), 16);
        assert_eq!(IoctlIn::default().to_bytes().len(), 32);
        assert_eq!(IoctlOut::default().to_bytes().len(), 16);
        assert_eq!(IoVec::default().to_bytes().len(), 16);
        assert_eq!(ReleaseIn::default().to_bytes().len(), 24);
        assert_eq!(PollIn::default().to_bytes().len(), 24);
        assert_eq!(PollOut::default().to_bytes().len(), 8);
        assert_eq!(InterruptIn::default().to_bytes().len(), 8);
        let i = IoctlIn { fh: 1, flags: 2, cmd: 3, arg: 4, in_size: 5, out_size: 6 };
        assert_eq!(IoctlIn::parse(&i.to_bytes()), Some(i));
    }

    #[test]
    fn init_negotiates_the_minor_and_announces_the_device() {
        for (kernel, agreed) in [(45, 45), (31, 31), (99, FUSE_KERNEL_MINOR_VERSION)] {
            let mut s = server();
            let r = s.handle(&init_req(1, kernel)).unwrap();
            let h = OutHeader::parse(&r).unwrap();
            assert_eq!((h.len as usize, h.error, h.unique), (r.len(), 0, 1));
            let o = CuseInitOut::parse(&r[16..]).unwrap();
            assert_eq!((o.major, o.minor), (7, agreed));
            assert_eq!(o.flags, CUSE_UNRESTRICTED_IOCTL);
            assert_eq!((o.dev_major, o.dev_minor), (0, 128));
            assert!(o.max_read >= 4096 && o.max_write >= 4096);
            assert_eq!(&r[88..], b"DEVNAME=dri/renderD128\0");
            assert!(s.initialized());
        }
    }

    #[test]
    fn an_old_or_foreign_kernel_is_refused() {
        let mut s = server();
        let r = s.handle(&init_req(1, 15)).unwrap();
        assert_eq!(OutHeader::parse(&r).unwrap().error, -libc::EPROTO);
        assert!(matches!(s.stop(), Some(Stop::InitFailed(_))));
        let mut s = server();
        let body = CuseInitIn { major: 8, minor: 0, unused: 0, flags: 1 }.to_bytes();
        s.handle(&req(CUSE_INIT, 1, 0, &body)).unwrap();
        assert!(!s.initialized());
    }

    #[test]
    fn requests_before_init_fail() {
        let mut s = server();
        let r = s.handle(&req(FUSE_OPEN, 3, 42, &OpenIn::default().to_bytes())).unwrap();
        assert_eq!(OutHeader::parse(&r).unwrap().error, -libc::EIO);
    }

    #[test]
    fn open_ioctl_retry_release() {
        let mut s = server();
        s.handle(&init_req(1, 45)).unwrap();
        let r = s.handle(&req(FUSE_OPEN, 2, 42, &OpenIn { flags: 2, open_flags: 0 }.to_bytes())).unwrap();
        assert_eq!(r.len(), 32);
        assert_eq!(OpenOut::parse(&r[16..]).unwrap().fh, 142);

        let r = s.handle(&ioctl_req(3, 42, 142, 0xc004_6400, 0x1000, &[], 0)).unwrap();
        assert_eq!(
            decode_ioctl_reply(&r, 3),
            Got::Retry(vec![IoVec { base: 0x1000, len: 4 }], vec![IoVec { base: 0x1000, len: 4 }])
        );
        let r = s.handle(&ioctl_req(4, 42, 142, 0xc004_6400, 0x1000, &[1, 2, 3, 4], 4)).unwrap();
        assert_eq!(decode_ioctl_reply(&r, 4), Got::Done(0, vec![2, 3, 4, 5]));

        let rel = ReleaseIn { fh: 142, flags: 2, release_flags: 0, lock_owner: 0 }.to_bytes();
        let r = s.handle(&req(FUSE_RELEASE, 5, 42, &rel)).unwrap();
        assert_eq!(r, ok_reply(5, &[]));
        assert_eq!(s.dev.released, vec![142]);
    }

    #[test]
    fn a_reply_larger_than_out_size_is_not_sent() {
        let mut s = server();
        s.handle(&init_req(1, 45)).unwrap();
        // in data present, but no room for output.
        let r = s.handle(&ioctl_req(3, 42, 1, 0xc004_6400, 0x1000, &[1, 2, 3, 4], 0)).unwrap();
        assert_eq!(decode_ioctl_reply(&r, 3), Got::Err(libc::EIO));
    }

    #[test]
    fn a_length_mismatch_is_refused() {
        let mut s = server();
        s.handle(&init_req(1, 45)).unwrap();
        let mut m = ioctl_req(3, 42, 1, 0xc004_6400, 0x1000, &[1, 2, 3, 4], 4);
        m.truncate(m.len() - 1);
        let r = s.handle(&m).unwrap();
        assert_eq!(OutHeader::parse(&r).unwrap().error, -libc::EINVAL);
    }

    #[test]
    fn misc_opcodes() {
        let mut s = server();
        s.handle(&init_req(1, 45)).unwrap();
        let rd = [0u8; 40];
        for op in [FUSE_READ, FUSE_WRITE] {
            let r = s.handle(&req(op, 9, 1, &rd)).unwrap();
            assert_eq!(OutHeader::parse(&r).unwrap().error, -libc::EINVAL);
        }
        assert_eq!(s.handle(&req(FUSE_FLUSH, 10, 1, &[0; 24])).unwrap(), ok_reply(10, &[]));
        assert_eq!(s.handle(&req(FUSE_INTERRUPT, 11, 1, &InterruptIn { unique: 9 }.to_bytes())), None);
        let p = s.handle(&req(FUSE_POLL, 12, 1, &PollIn::default().to_bytes())).unwrap();
        assert_eq!(p, ok_reply(12, &PollOut::default().to_bytes()));
        let r = s.handle(&req(12345, 13, 1, &[])).unwrap();
        assert_eq!(OutHeader::parse(&r).unwrap().error, -libc::ENOSYS);
        assert_eq!(s.handle(&req(FUSE_DESTROY, 14, 0, &[])).unwrap(), ok_reply(14, &[]));
        assert!(matches!(s.stop(), Some(Stop::Destroyed)));
    }

    #[test]
    fn devnames() {
        assert!(devname_ok("dri/renderD128"));
        for bad in ["", "/dri/x", "dri//x", "dri/../x", "a b", "dri/x\0"] {
            assert!(!devname_ok(bad), "{bad:?}");
        }
    }

    /// The loop over a descriptor that keeps message boundaries, as /dev/cuse.
    #[test]
    fn serve_over_a_seqpacket_socketpair() {
        use std::os::fd::{AsFd, FromRawFd, OwnedFd};
        let mut sv = [0; 2];
        // SAFETY: socketpair into a valid array.
        let r = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr()) };
        assert_eq!(r, 0);
        // SAFETY: fresh descriptors we own.
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) };
        let t = std::thread::spawn(move || {
            let mut s = server();
            let stop = serve(b.as_fd(), &mut s, || Ok(true), |_| Ok(())).unwrap();
            (stop, s.dev.opened)
        });
        let send = |m: &[u8]| {
            // SAFETY: valid buffer.
            assert_eq!(unsafe { libc::write(a.as_raw_fd(), m.as_ptr().cast(), m.len()) }, m.len() as isize);
        };
        let recv = || {
            let mut buf = vec![0u8; 8192];
            // SAFETY: valid buffer.
            let n = unsafe { libc::read(a.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            assert!(n > 0);
            buf.truncate(n as usize);
            buf
        };
        send(&init_req(1, 45));
        assert_eq!(CuseInitOut::parse(&recv()[16..]).unwrap().minor, 45);
        send(&req(FUSE_OPEN, 2, 77, &OpenIn::default().to_bytes()));
        assert_eq!(OpenOut::parse(&recv()[16..]).unwrap().fh, 177);
        send(&req(FUSE_INTERRUPT, 3, 77, &InterruptIn { unique: 2 }.to_bytes()));
        send(&ioctl_req(4, 77, 177, 0, 8, &[], 0));
        assert!(matches!(decode_ioctl_reply(&recv(), 4), Got::Retry(..)), "the interrupt got no reply");
        drop(a);
        let (stop, opened) = t.join().unwrap();
        assert!(matches!(stop, Stop::Closed));
        assert_eq!(opened, vec![77]);
    }
}
