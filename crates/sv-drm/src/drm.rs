//! The DRM ioctls the node answers (docs/waycompose-design.md §104).
//!
//! Only what Mesa's GBM, Zink, libdrm and KWin ask a render node: the version,
//! the caps, and PRIME import of our tokens into per-open GEM handles. No
//! memory lives here; a GEM handle is only a name for a token's nonce, so that
//! importers that insist on a handle get a stable one.
//!
//! Layouts are the kernel's (`include/uapi/drm/drm.h`), in both the 64-bit
//! and the 32-bit (compat) ABI where they differ (`size_t` and pointers in
//! `drm_version` and `drm_unique`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use crate::cuse::{Device, InHeader, IoVec, IoctlIn, IoctlReply, OpenIn};
use crate::token::{self, Token};

/// `DRM_IOCTL_BASE`.
pub const DRM_IOCTL_BASE: u32 = b'd' as u32;

pub const DRM_VERSION: u32 = 0x00;
pub const DRM_GET_UNIQUE: u32 = 0x01;
pub const DRM_GET_MAGIC: u32 = 0x02;
pub const DRM_GEM_CLOSE: u32 = 0x09;
pub const DRM_GET_CAP: u32 = 0x0c;
pub const DRM_SET_CLIENT_CAP: u32 = 0x0d;
pub const DRM_AUTH_MAGIC: u32 = 0x11;
pub const DRM_PRIME_HANDLE_TO_FD: u32 = 0x2d;
pub const DRM_PRIME_FD_TO_HANDLE: u32 = 0x2e;
/// `DRM_IOCTL_MODE_*` live in 0xA0..=0xCF.
pub const DRM_MODE_FIRST: u32 = 0xa0;
pub const DRM_MODE_LAST: u32 = 0xcf;

pub const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
pub const DRM_CAP_PRIME: u64 = 0x5;
pub const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
pub const DRM_CAP_ADDFB2_MODIFIERS: u64 = 0x10;
pub const DRM_CAP_SYNCOBJ: u64 = 0x13;
pub const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
pub const DRM_PRIME_CAP_IMPORT: u64 = 0x1;
pub const DRM_PRIME_CAP_EXPORT: u64 = 0x2;

pub const NAME: &str = "svdrm";
pub const DATE: &str = "20261019";
pub const DESC: &str = "shared_vulkan userspace render node";
pub const VERSION: (i32, i32, i32) = (1, 0, 0);

/// `_IOC_NR`, `_IOC_TYPE`, `_IOC_SIZE` (asm-generic layout: x86, arm64).
pub fn ioc_nr(cmd: u32) -> u32 {
    cmd & 0xff
}
pub fn ioc_type(cmd: u32) -> u32 {
    (cmd >> 8) & 0xff
}
pub fn ioc_size(cmd: u32) -> u32 {
    (cmd >> 16) & 0x3fff
}
/// `_IOC(dir, 'd', nr, size)`; dir 1 = write (to the kernel), 2 = read, 3 = both.
pub const fn drm_ioc(dir: u32, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | (DRM_IOCTL_BASE << 8) | nr
}

/// What a cap query answers; `None` is EINVAL.
pub fn cap_value(cap: u64) -> Option<u64> {
    match cap {
        DRM_CAP_PRIME => Some(DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT),
        DRM_CAP_SYNCOBJ | DRM_CAP_SYNCOBJ_TIMELINE | DRM_CAP_DUMB_BUFFER | DRM_CAP_ADDFB2_MODIFIERS => Some(0),
        DRM_CAP_TIMESTAMP_MONOTONIC => Some(1),
        _ => None,
    }
}

/// The caller's pointer width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abi {
    B64,
    B32,
}

impl Abi {
    pub fn of(a: &IoctlIn) -> Abi {
        if a.is_32bit() { Abi::B32 } else { Abi::B64 }
    }
    fn word(self) -> usize {
        match self {
            Abi::B64 => 8,
            Abi::B32 => 4,
        }
    }
    fn get(self, b: &[u8], off: usize) -> u64 {
        match self {
            Abi::B64 => u64::from_ne_bytes(b[off..off + 8].try_into().unwrap()),
            Abi::B32 => u32::from_ne_bytes(b[off..off + 4].try_into().unwrap()) as u64,
        }
    }
    fn put(self, b: &mut [u8], off: usize, v: u64) {
        match self {
            Abi::B64 => b[off..off + 8].copy_from_slice(&v.to_ne_bytes()),
            Abi::B32 => b[off..off + 4].copy_from_slice(&(v as u32).to_ne_bytes()),
        }
    }
    /// `sizeof(struct drm_version)`: three ints, then three (size_t, char*).
    pub fn version_size(self) -> usize {
        match self {
            Abi::B64 => 64,
            Abi::B32 => 36,
        }
    }
    /// Offset of the `i`th (len, ptr) pair in `drm_version`.
    fn version_pair(self, i: usize) -> usize {
        let first = match self {
            Abi::B64 => 16, // 12 bytes of ints, padded to 8
            Abi::B32 => 12,
        };
        first + i * 2 * self.word()
    }
    /// `sizeof(struct drm_unique)`: (size_t, char*).
    pub fn unique_size(self) -> usize {
        2 * self.word()
    }
}

/// Round for a struct without pointers: ask for it (and room to write it back
/// when `out`), then run `f` on its bytes. `f` returns the bytes to write back.
fn fixed(a: &IoctlIn, data: &[u8], size: usize, out: bool, f: impl FnOnce(&[u8]) -> Result<Vec<u8>, i32>) -> IoctlReply {
    let want_out = if out { size } else { 0 };
    if data.len() != size || a.out_size as usize != want_out {
        let v = IoVec { base: a.arg, len: size as u64 };
        return IoctlReply::Retry { input: vec![v], output: if out { vec![v] } else { vec![] } };
    }
    match f(data) {
        Ok(d) => {
            debug_assert_eq!(d.len(), want_out);
            IoctlReply::Done { result: 0, data: d }
        }
        Err(e) => IoctlReply::Error(e),
    }
}

/// `DRM_IOCTL_VERSION`: the struct, then the three strings, each cut to the
/// caller's buffer length; the `*_len` fields always say the full length (the
/// kernel's `drm_copy_field`). Two retries: one to read the struct, one, once
/// the buffer pointers are known, to write the strings.
pub fn version(a: &IoctlIn, data: &[u8]) -> IoctlReply {
    let abi = Abi::of(a);
    let size = abi.version_size();
    let whole = IoVec { base: a.arg, len: size as u64 };
    if data.len() != size {
        return IoctlReply::Retry { input: vec![whole], output: vec![whole] };
    }
    let mut st = data.to_vec();
    st[0..4].copy_from_slice(&VERSION.0.to_ne_bytes());
    st[4..8].copy_from_slice(&VERSION.1.to_ne_bytes());
    st[8..12].copy_from_slice(&VERSION.2.to_ne_bytes());
    let mut output = vec![whole];
    let mut strings = Vec::new();
    for (i, s) in [NAME, DATE, DESC].iter().enumerate() {
        let off = abi.version_pair(i);
        let ulen = abi.get(data, off);
        let ptr = abi.get(data, off + abi.word());
        let n = ulen.min(s.len() as u64) as usize;
        abi.put(&mut st, off, s.len() as u64);
        if n > 0 {
            if ptr == 0 {
                return IoctlReply::Error(libc::EFAULT);
            }
            output.push(IoVec { base: ptr, len: n as u64 });
            strings.extend_from_slice(&s.as_bytes()[..n]);
        }
    }
    let total: u64 = output.iter().map(|v| v.len).sum();
    if a.out_size as u64 != total {
        return IoctlReply::Retry { input: vec![whole], output };
    }
    st.extend_from_slice(&strings);
    IoctlReply::Done { result: 0, data: st }
}

/// Resolves a caller's fd to the token behind it.
pub trait FdResolver {
    /// The token behind fd `fd` of task `pid`; errno on failure.
    fn token(&self, pid: u32, fd: i32) -> Result<Token, i32>;
}

/// `TMPFS_MAGIC`: memfds live on the kernel's internal shmem mount.
const TMPFS_MAGIC: i64 = 0x0102_1994;

/// Resolves through `/proc/<pid>/fd/<fd>`, where `pid` is the task that issued
/// the ioctl (from the FUSE header, in our pid namespace).
///
/// Never blocks and never opens anything but a memfd: the link is first opened
/// `O_PATH` (which runs no driver `open`, so a client cannot point us at a FIFO,
/// at a FUSE file it serves itself, or at this very node and deadlock the
/// loop), checked to be a regular file on tmpfs, and only then reopened for
/// reading through `/proc/self/fd`, which names that same inode.
pub struct ProcResolver;

fn open_c(path: &str, flags: libc::c_int) -> Result<OwnedFd, i32> {
    let p = CString::new(path).map_err(|_| libc::EINVAL)?;
    // SAFETY: valid C string; the result is checked.
    let fd = unsafe { libc::open(p.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
    }
    // SAFETY: a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl FdResolver for ProcResolver {
    fn token(&self, pid: u32, fd: i32) -> Result<Token, i32> {
        if pid == 0 || fd < 0 {
            return Err(libc::EBADF);
        }
        let path = open_c(&format!("/proc/{pid}/fd/{fd}"), libc::O_PATH | libc::O_CLOEXEC).map_err(|_| libc::EBADF)?;
        // SAFETY: plain syscalls into zeroed out-structs.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(path.as_raw_fd(), &mut st) } != 0 || st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(libc::EINVAL);
        }
        let mut sf: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(path.as_raw_fd(), &mut sf) } != 0 || sf.f_type as i64 != TMPFS_MAGIC {
            return Err(libc::EINVAL);
        }
        let f = open_c(
            &format!("/proc/self/fd/{}", path.as_raw_fd()),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY,
        )
        .map_err(|_| libc::EINVAL)?;
        token::read(f.as_fd()).map_err(|_| libc::EINVAL)
    }
}

/// One open file description of the node.
#[derive(Debug, Default)]
pub struct OpenFile {
    /// Who opened it (for the log).
    pub pid: u32,
    /// GEM handle -> the token it names.
    pub handles: BTreeMap<u32, Token>,
    /// Token nonce -> GEM handle: the same allocation imports to the same handle.
    pub by_nonce: HashMap<[u8; 16], u32>,
    next_handle: u32,
}

impl OpenFile {
    /// The handle for `t`, made on first import.
    pub fn import(&mut self, t: Token) -> u32 {
        if let Some(&h) = self.by_nonce.get(&t.nonce) {
            return h;
        }
        loop {
            self.next_handle = self.next_handle.wrapping_add(1).max(1);
            if !self.handles.contains_key(&self.next_handle) {
                break;
            }
        }
        let h = self.next_handle;
        self.by_nonce.insert(t.nonce, h);
        self.handles.insert(h, t);
        h
    }

    /// Drop a handle; false if there was none.
    pub fn close(&mut self, h: u32) -> bool {
        match self.handles.remove(&h) {
            Some(t) => {
                self.by_nonce.remove(&t.nonce);
                true
            }
            None => false,
        }
    }
}

/// The render node: per-open state and the ioctl table.
pub struct DrmNode<R: FdResolver> {
    pub files: HashMap<u64, OpenFile>,
    next_fh: u64,
    resolver: R,
    logged: HashSet<u32>,
}

impl<R: FdResolver> DrmNode<R> {
    pub fn new(resolver: R) -> Self {
        DrmNode { files: HashMap::new(), next_fh: 0, resolver, logged: HashSet::new() }
    }

    fn log_once(&mut self, nr: u32, cmd: u32, pid: u32, what: &str) {
        if self.logged.insert(nr) {
            eprintln!("sv-drm: ioctl nr {nr:#04x} (cmd {cmd:#010x}) from pid {pid}: {what}");
        }
    }
}

impl<R: FdResolver> Device for DrmNode<R> {
    fn open(&mut self, hdr: &InHeader, _a: &OpenIn) -> Result<u64, i32> {
        self.next_fh += 1;
        self.files.insert(self.next_fh, OpenFile { pid: hdr.pid, ..Default::default() });
        Ok(self.next_fh)
    }

    fn release(&mut self, fh: u64) {
        self.files.remove(&fh);
    }

    fn ioctl(&mut self, hdr: &InHeader, a: &IoctlIn, data: &[u8]) -> IoctlReply {
        let (cmd, nr, pid) = (a.cmd, ioc_nr(a.cmd), hdr.pid);
        if !self.files.contains_key(&a.fh) {
            return IoctlReply::Error(libc::EBADF);
        }
        if ioc_type(cmd) != DRM_IOCTL_BASE {
            if self.logged.insert(0x1_0000 | (cmd & 0xffff)) {
                eprintln!("sv-drm: non-DRM ioctl cmd {cmd:#010x} from pid {pid}: ENOTTY");
            }
            return IoctlReply::Error(libc::ENOTTY);
        }
        let abi = Abi::of(a);
        let expect = match nr {
            DRM_VERSION => Some(abi.version_size()),
            DRM_GET_UNIQUE => Some(abi.unique_size()),
            DRM_GET_CAP => Some(16),
            DRM_GEM_CLOSE => Some(8),
            DRM_PRIME_FD_TO_HANDLE => Some(12),
            _ => None,
        };
        if let Some(s) = expect
            && ioc_size(cmd) as usize != s
        {
            self.log_once(nr, cmd, pid, "unexpected argument size: EINVAL");
            return IoctlReply::Error(libc::EINVAL);
        }
        match nr {
            DRM_VERSION => version(a, data),
            DRM_GET_UNIQUE => fixed(a, data, abi.unique_size(), true, |d| {
                // An empty bus id: unique_len = 0, nothing copied.
                let mut d = d.to_vec();
                abi.put(&mut d, 0, 0);
                Ok(d)
            }),
            DRM_GET_MAGIC | DRM_AUTH_MAGIC => IoctlReply::Error(libc::EACCES),
            DRM_SET_CLIENT_CAP => IoctlReply::Error(libc::EINVAL),
            DRM_GET_CAP => fixed(a, data, 16, true, |d| {
                let cap = u64::from_ne_bytes(d[0..8].try_into().unwrap());
                let v = cap_value(cap).ok_or(libc::EINVAL)?;
                let mut d = d.to_vec();
                d[8..16].copy_from_slice(&v.to_ne_bytes());
                Ok(d)
            }),
            DRM_GEM_CLOSE => {
                let file = self.files.get_mut(&a.fh).unwrap();
                fixed(a, data, 8, false, |d| {
                    let h = u32::from_ne_bytes(d[0..4].try_into().unwrap());
                    let pad = u32::from_ne_bytes(d[4..8].try_into().unwrap());
                    if pad != 0 || !file.close(h) {
                        return Err(libc::EINVAL);
                    }
                    Ok(vec![])
                })
            }
            DRM_PRIME_FD_TO_HANDLE => {
                let resolver = &self.resolver;
                let file = self.files.get_mut(&a.fh).unwrap();
                fixed(a, data, 12, true, |d| {
                    let fd = i32::from_ne_bytes(d[8..12].try_into().unwrap());
                    let t = resolver.token(pid, fd)?;
                    let h = file.import(t);
                    let mut d = d.to_vec();
                    d[0..4].copy_from_slice(&h.to_ne_bytes());
                    Ok(d)
                })
            }
            DRM_PRIME_HANDLE_TO_FD => {
                // The kernel turns ENOSYS from a FUSE server into ENOTTY.
                self.log_once(nr, cmd, pid, "PRIME_HANDLE_TO_FD: a CUSE server cannot hand out fds (ENOSYS)");
                IoctlReply::Error(libc::ENOSYS)
            }
            DRM_MODE_FIRST..=DRM_MODE_LAST => {
                self.log_once(nr, cmd, pid, "KMS on a render node: EOPNOTSUPP");
                IoctlReply::Error(libc::EOPNOTSUPP)
            }
            _ => {
                self.log_once(nr, cmd, pid, "unhandled: EINVAL");
                IoctlReply::Error(libc::EINVAL)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cuse::tests::{Got, decode_ioctl_reply, init_req, req};
    use crate::cuse::{
        DevInfo, FUSE_IOCTL_32BIT, FUSE_IOCTL_UNRESTRICTED, FUSE_OPEN, FUSE_RELEASE, OutHeader, ReleaseIn, Server,
        Wire,
    };

    const IOCTL_VERSION: u32 = drm_ioc(3, DRM_VERSION, 64);
    const IOCTL_VERSION32: u32 = drm_ioc(3, DRM_VERSION, 36);
    const IOCTL_GET_UNIQUE: u32 = drm_ioc(3, DRM_GET_UNIQUE, 16);
    const IOCTL_GET_MAGIC: u32 = drm_ioc(2, DRM_GET_MAGIC, 4);
    const IOCTL_GET_CAP: u32 = drm_ioc(3, DRM_GET_CAP, 16);
    const IOCTL_SET_CLIENT_CAP: u32 = drm_ioc(1, DRM_SET_CLIENT_CAP, 16);
    const IOCTL_GEM_CLOSE: u32 = drm_ioc(1, DRM_GEM_CLOSE, 8);
    const IOCTL_FD_TO_HANDLE: u32 = drm_ioc(3, DRM_PRIME_FD_TO_HANDLE, 12);
    const IOCTL_HANDLE_TO_FD: u32 = drm_ioc(3, DRM_PRIME_HANDLE_TO_FD, 12);
    const IOCTL_MODE_GETRESOURCES: u32 = drm_ioc(3, 0xa0, 64);
    const IOCTL_SYNCOBJ_CREATE: u32 = drm_ioc(3, 0xbf, 16); // in the 0xA0..=0xCF range
    const IOCTL_I915_GETPARAM: u32 = drm_ioc(3, 0x46, 16);

    #[test]
    fn the_ioctl_numbers_are_libdrms() {
        // As libdrm/xf86drm.h computes them on x86_64.
        assert_eq!(IOCTL_VERSION, 0xc040_6400);
        assert_eq!(IOCTL_GET_CAP, 0xc010_640c);
        assert_eq!(IOCTL_FD_TO_HANDLE, 0xc00c_642e);
        assert_eq!(IOCTL_HANDLE_TO_FD, 0xc00c_642d);
        assert_eq!(IOCTL_GEM_CLOSE, 0x4008_6409);
        assert_eq!(IOCTL_GET_UNIQUE, 0xc010_6401);
        assert_eq!(IOCTL_GET_MAGIC, 0x8004_6402);
    }

    #[test]
    fn caps() {
        assert_eq!(cap_value(DRM_CAP_PRIME), Some(3));
        assert_eq!(cap_value(DRM_CAP_SYNCOBJ), Some(0));
        assert_eq!(cap_value(DRM_CAP_SYNCOBJ_TIMELINE), Some(0));
        assert_eq!(cap_value(DRM_CAP_DUMB_BUFFER), Some(0));
        assert_eq!(cap_value(DRM_CAP_TIMESTAMP_MONOTONIC), Some(1));
        assert_eq!(cap_value(DRM_CAP_ADDFB2_MODIFIERS), Some(0));
        assert_eq!(cap_value(0x8), None); // DRM_CAP_CURSOR_WIDTH
    }

    /// A resolver over a fixed table, for tests that need no real fds.
    struct Table(HashMap<(u32, i32), Token>);
    impl FdResolver for Table {
        fn token(&self, pid: u32, fd: i32) -> Result<Token, i32> {
            self.0.get(&(pid, fd)).cloned().ok_or(libc::EINVAL)
        }
    }

    fn tok(n: u8) -> Token {
        Token { size: 4096, nonce: [n; 16], name: format!("/dev/shm/sv-{n}") }
    }

    /// A server after CUSE_INIT and one OPEN by `pid`; returns it and the fh.
    fn opened<R: FdResolver>(r: R, pid: u32) -> (Server<DrmNode<R>>, u64) {
        let mut s =
            Server::new(DrmNode::new(r), DevInfo { devname: "dri/renderD128".into(), dev_major: 0, dev_minor: 128 });
        s.handle(&init_req(1, 45)).unwrap();
        let r = s.handle(&req(FUSE_OPEN, 2, pid, &OpenIn { flags: 2, open_flags: 0 }.to_bytes())).unwrap();
        let fh = u64::from_ne_bytes(r[16..24].try_into().unwrap());
        (s, fh)
    }

    /// Drive one ioctl through the kernel's retry loop against simulated user
    /// memory, as fs/fuse/ioctl.c does: returns the result (or -errno), the
    /// number of rounds, and the memory after.
    fn drive<D: Device>(s: &mut Server<D>, pid: u32, fh: u64, cmd: u32, flags: u32, mem: &mut UserMem) -> (i32, usize) {
        let (mut ins, mut outs): (Vec<IoVec>, Vec<IoVec>) = (vec![], vec![]);
        for round in 1..10 {
            let data: Vec<u8> = ins.iter().flat_map(|v| mem.read(v.base, v.len)).collect();
            let out_size: u64 = outs.iter().map(|v| v.len).sum();
            let a = IoctlIn {
                fh,
                flags: FUSE_IOCTL_UNRESTRICTED | flags,
                cmd,
                arg: mem.arg,
                in_size: data.len() as u32,
                out_size: out_size as u32,
            };
            let mut body = a.to_bytes();
            body.extend_from_slice(&data);
            let unique = 100 + round as u64;
            let r = s.handle(&req(crate::cuse::FUSE_IOCTL, unique, pid, &body)).unwrap();
            match decode_ioctl_reply(&r, unique) {
                Got::Err(e) => return (-e, round),
                Got::Retry(i, o) => (ins, outs) = (i, o),
                Got::Done(res, d) => {
                    assert!(d.len() as u64 <= out_size);
                    let mut off = 0;
                    for v in &outs {
                        let n = (v.len as usize).min(d.len() - off);
                        mem.write(v.base, &d[off..off + n]);
                        off += n;
                    }
                    return (res, round);
                }
            }
        }
        panic!("no end to the retries");
    }

    /// The caller's address space: a few disjoint regions.
    struct UserMem {
        arg: u64,
        regions: Vec<(u64, Vec<u8>)>,
    }
    impl UserMem {
        fn new(arg: u64, st: Vec<u8>) -> Self {
            UserMem { arg, regions: vec![(arg, st)] }
        }
        fn add(&mut self, base: u64, len: usize) {
            self.regions.push((base, vec![0xee; len]));
        }
        fn find(&mut self, base: u64, len: u64) -> &mut [u8] {
            for (b, v) in &mut self.regions {
                if base >= *b && base + len <= *b + v.len() as u64 {
                    let o = (base - *b) as usize;
                    return &mut v[o..o + len as usize];
                }
            }
            panic!("EFAULT at {base:#x}+{len}");
        }
        fn read(&mut self, base: u64, len: u64) -> Vec<u8> {
            self.find(base, len).to_vec()
        }
        fn write(&mut self, base: u64, d: &[u8]) {
            self.find(base, d.len() as u64).copy_from_slice(d);
        }
        fn st(&self) -> &[u8] {
            &self.regions[0].1
        }
        fn region(&self, i: usize) -> &[u8] {
            &self.regions[i].1
        }
    }

    fn u64_at(b: &[u8], o: usize) -> u64 {
        u64::from_ne_bytes(b[o..o + 8].try_into().unwrap())
    }
    fn u32_at(b: &[u8], o: usize) -> u32 {
        u32::from_ne_bytes(b[o..o + 4].try_into().unwrap())
    }

    #[test]
    fn get_cap_takes_one_retry() {
        let (mut s, fh) = opened(Table(HashMap::new()), 42);
        let mut st = DRM_CAP_PRIME.to_ne_bytes().to_vec();
        st.extend_from_slice(&0u64.to_ne_bytes());
        let mut m = UserMem::new(0x7000, st);
        assert_eq!(drive(&mut s, 42, fh, IOCTL_GET_CAP, 0, &mut m), (0, 2));
        assert_eq!(u64_at(m.st(), 8), 3);

        let mut st = 0x8u64.to_ne_bytes().to_vec();
        st.extend_from_slice(&[0; 8]);
        let mut m = UserMem::new(0x7000, st);
        assert_eq!(drive(&mut s, 42, fh, IOCTL_GET_CAP, 0, &mut m).0, -libc::EINVAL);
    }

    #[test]
    fn version_with_buffers_takes_two_retries() {
        let (mut s, fh) = opened(Table(HashMap::new()), 42);
        // drmGetVersion's second call: buffers of the lengths the first returned,
        // here deliberately one short for the description.
        let mut st = vec![0u8; 64];
        let (name, date, desc) = (0x9000u64, 0xa000u64, 0xb000u64);
        for (i, (l, p)) in [(5u64, name), (8, date), (DESC.len() as u64 - 1, desc)].iter().enumerate() {
            st[16 + i * 16..24 + i * 16].copy_from_slice(&l.to_ne_bytes());
            st[24 + i * 16..32 + i * 16].copy_from_slice(&p.to_ne_bytes());
        }
        let mut m = UserMem::new(0x8000, st);
        m.add(name, 16);
        m.add(date, 16);
        m.add(desc, 64);
        assert_eq!(drive(&mut s, 42, fh, IOCTL_VERSION, 0, &mut m), (0, 3), "first, struct, strings");
        let st = m.st();
        assert_eq!((u32_at(st, 0), u32_at(st, 4), u32_at(st, 8)), (1, 0, 0));
        assert_eq!(u64_at(st, 16), 5);
        assert_eq!(u64_at(st, 32), 8);
        assert_eq!(u64_at(st, 48), DESC.len() as u64, "the full length, not the copied one");
        assert_eq!((u64_at(st, 24), u64_at(st, 40), u64_at(st, 56)), (name, date, desc), "pointers untouched");
        assert_eq!(&m.region(1)[..6], b"svdrm\xee");
        assert_eq!(&m.region(2)[..9], b"20261019\xee");
        let d = m.region(3);
        assert_eq!(&d[..DESC.len() - 1], &DESC.as_bytes()[..DESC.len() - 1]);
        assert_eq!(d[DESC.len() - 1], 0xee, "never more than the caller's length");
    }

    #[test]
    fn version_with_zero_lengths_takes_one_retry() {
        let (mut s, fh) = opened(Table(HashMap::new()), 42);
        let mut m = UserMem::new(0x8000, vec![0u8; 64]);
        assert_eq!(drive(&mut s, 42, fh, IOCTL_VERSION, 0, &mut m), (0, 2));
        let st = m.st();
        assert_eq!((u64_at(st, 16), u64_at(st, 32), u64_at(st, 48)), (5, 8, DESC.len() as u64));
    }

    #[test]
    fn version_from_a_32bit_caller() {
        let (mut s, fh) = opened(Table(HashMap::new()), 42);
        let mut st = vec![0u8; 36];
        st[12..16].copy_from_slice(&64u32.to_ne_bytes()); // name_len
        st[16..20].copy_from_slice(&0x9000u32.to_ne_bytes()); // name
        let mut m = UserMem::new(0x8000, st);
        m.add(0x9000, 64);
        assert_eq!(drive(&mut s, 42, fh, IOCTL_VERSION32, FUSE_IOCTL_32BIT, &mut m), (0, 3));
        assert_eq!(u32_at(m.st(), 12), 5);
        assert_eq!(u32_at(m.st(), 20), 8);
        assert_eq!(&m.region(1)[..5], b"svdrm");
        // The 64-bit number from a 32-bit caller is a size mismatch.
        let mut m = UserMem::new(0x8000, vec![0; 64]);
        assert_eq!(drive(&mut s, 42, fh, IOCTL_VERSION, FUSE_IOCTL_32BIT, &mut m).0, -libc::EINVAL);
    }

    #[test]
    fn version_with_a_null_buffer_faults() {
        let (mut s, fh) = opened(Table(HashMap::new()), 42);
        let mut st = vec![0u8; 64];
        st[16..24].copy_from_slice(&5u64.to_ne_bytes());
        let mut m = UserMem::new(0x8000, st);
        assert_eq!(drive(&mut s, 42, fh, IOCTL_VERSION, 0, &mut m).0, -libc::EFAULT);
    }

    #[test]
    fn get_unique_is_empty() {
        let (mut s, fh) = opened(Table(HashMap::new()), 42);
        let mut st = 32u64.to_ne_bytes().to_vec();
        st.extend_from_slice(&0x9000u64.to_ne_bytes());
        let mut m = UserMem::new(0x8000, st);
        m.add(0x9000, 32);
        assert_eq!(drive(&mut s, 42, fh, IOCTL_GET_UNIQUE, 0, &mut m), (0, 2));
        assert_eq!(u64_at(m.st(), 0), 0);
        assert_eq!(m.region(1), &[0xee; 32][..]);
    }

    #[test]
    fn refusals_come_without_a_retry() {
        let (mut s, fh) = opened(Table(HashMap::new()), 42);
        for (cmd, e) in [
            (IOCTL_GET_MAGIC, libc::EACCES),
            (drm_ioc(1, DRM_AUTH_MAGIC, 4), libc::EACCES),
            (IOCTL_SET_CLIENT_CAP, libc::EINVAL),
            (IOCTL_HANDLE_TO_FD, libc::ENOSYS),
            (IOCTL_MODE_GETRESOURCES, libc::EOPNOTSUPP),
            (IOCTL_SYNCOBJ_CREATE, libc::EOPNOTSUPP),
            (IOCTL_I915_GETPARAM, libc::EINVAL),
            (drm_ioc(3, 0xd0, 8), libc::EINVAL),
            (0x5401, libc::ENOTTY), // TCGETS
        ] {
            let mut m = UserMem::new(0x8000, vec![0; 64]);
            assert_eq!(drive(&mut s, 42, fh, cmd, 0, &mut m), (-e, 1), "cmd {cmd:#x}");
        }
        // An ioctl on an fh the kernel never opened.
        let mut m = UserMem::new(0x8000, vec![0; 16]);
        assert_eq!(drive(&mut s, 42, fh + 1, IOCTL_GET_CAP, 0, &mut m).0, -libc::EBADF);
    }

    fn prime(fd: i32) -> Vec<u8> {
        let mut st = vec![0u8; 12];
        st[0..4].copy_from_slice(&0xdead_beefu32.to_ne_bytes());
        st[8..12].copy_from_slice(&fd.to_ne_bytes());
        st
    }

    fn gem_close(h: u32) -> Vec<u8> {
        let mut st = h.to_ne_bytes().to_vec();
        st.extend_from_slice(&[0; 4]);
        st
    }

    #[test]
    fn prime_import_gem_close_release() {
        let table = Table(HashMap::from([((42, 5), tok(1)), ((42, 6), tok(1)), ((42, 7), tok(2)), ((43, 5), tok(3))]));
        let (mut s, fh) = opened(table, 42);
        let import = |s: &mut Server<DrmNode<Table>>, pid: u32, fd: i32| {
            let mut m = UserMem::new(0x8000, prime(fd));
            let (r, rounds) = drive(s, pid, fh, IOCTL_FD_TO_HANDLE, 0, &mut m);
            assert!(r < 0 || rounds == 2);
            if r < 0 { Err(-r) } else { Ok(u32_at(m.st(), 0)) }
        };
        let h1 = import(&mut s, 42, 5).unwrap();
        assert_ne!(h1, 0);
        assert_eq!(import(&mut s, 42, 6), Ok(h1), "same allocation, same handle");
        let h2 = import(&mut s, 42, 7).unwrap();
        assert_ne!(h1, h2);
        // fd 5 of another pid is a different file: the resolver is asked with
        // the pid of the ioctl, never the opener's.
        let h3 = import(&mut s, 43, 5).unwrap();
        assert!(h3 != h1 && h3 != h2);
        assert_eq!(import(&mut s, 42, 99), Err(libc::EINVAL));
        assert_eq!(s.dev.files[&fh].handles.len(), 3);

        let mut m = UserMem::new(0x8000, gem_close(h1));
        assert_eq!(drive(&mut s, 42, fh, IOCTL_GEM_CLOSE, 0, &mut m), (0, 2));
        let mut m = UserMem::new(0x8000, gem_close(h1));
        assert_eq!(drive(&mut s, 42, fh, IOCTL_GEM_CLOSE, 0, &mut m).0, -libc::EINVAL, "already closed");
        let mut m = UserMem::new(0x8000, gem_close(12345));
        assert_eq!(drive(&mut s, 42, fh, IOCTL_GEM_CLOSE, 0, &mut m).0, -libc::EINVAL);
        let h1b = import(&mut s, 42, 5).unwrap();
        assert!(h1b != h2 && h1b != h3, "a reimport gets a live-unique handle");

        let rel = ReleaseIn { fh, flags: 2, release_flags: 0, lock_owner: 0 }.to_bytes();
        let r = s.handle(&req(FUSE_RELEASE, 50, 42, &rel)).unwrap();
        assert_eq!(OutHeader::parse(&r).unwrap().error, 0);
        assert!(s.dev.files.is_empty(), "release drops the open's handles");
    }

    #[test]
    fn handles_are_per_open() {
        let table = Table(HashMap::from([((42, 5), tok(1))]));
        let (mut s, fh1) = opened(table, 42);
        let r = s.handle(&req(FUSE_OPEN, 3, 42, &OpenIn::default().to_bytes())).unwrap();
        let fh2 = u64::from_ne_bytes(r[16..24].try_into().unwrap());
        assert_ne!(fh1, fh2);
        for fh in [fh1, fh2] {
            let mut m = UserMem::new(0x8000, prime(5));
            assert_eq!(drive(&mut s, 42, fh, IOCTL_FD_TO_HANDLE, 0, &mut m).0, 0);
            assert_eq!(u32_at(m.st(), 0), 1);
        }
        let mut m = UserMem::new(0x8000, gem_close(1));
        assert_eq!(drive(&mut s, 42, fh1, IOCTL_GEM_CLOSE, 0, &mut m).0, 0);
        assert_eq!(s.dev.files[&fh2].handles.len(), 1);
    }

    #[test]
    fn the_proc_resolver_reads_real_tokens_of_the_calling_pid() {
        use std::os::fd::AsRawFd;
        let t = tok(9);
        let fd = token::create(&t).unwrap();
        let pid = std::process::id();
        assert_eq!(ProcResolver.token(pid, fd.as_raw_fd()), Ok(t.clone()));

        // Through the whole node.
        let (mut s, fh) = opened(ProcResolver, pid);
        let mut m = UserMem::new(0x8000, prime(fd.as_raw_fd()));
        assert_eq!(drive(&mut s, pid, fh, IOCTL_FD_TO_HANDLE, 0, &mut m), (0, 2));
        assert_eq!(s.dev.files[&fh].handles[&u32_at(m.st(), 0)], t);

        // Not a token: a plain file, a pipe (would block if opened), /dev/null, nothing.
        let f = std::fs::File::open("/proc/self/exe").unwrap();
        assert_eq!(ProcResolver.token(pid, f.as_raw_fd()), Err(libc::EINVAL));
        let mut p = [0; 2];
        // SAFETY: pipe into a valid array; closed below.
        assert_eq!(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        assert_eq!(ProcResolver.token(pid, p[0]), Err(libc::EINVAL));
        // SAFETY: our pipe fds.
        unsafe {
            libc::close(p[0]);
            libc::close(p[1]);
        }
        let n = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(ProcResolver.token(pid, n.as_raw_fd()), Err(libc::EINVAL));
        assert_eq!(ProcResolver.token(pid, 100_000), Err(libc::EBADF));
        assert_eq!(ProcResolver.token(pid, -1), Err(libc::EBADF));
        assert_eq!(ProcResolver.token(0, 3), Err(libc::EBADF));
        // An unsealed memfd with a valid header.
        let name = CString::new("x").unwrap();
        // SAFETY: test-only syscalls on a fresh memfd.
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        let h = t.encode();
        unsafe { libc::pwrite(raw, h.as_ptr().cast(), h.len(), 0) };
        assert_eq!(ProcResolver.token(pid, raw), Err(libc::EINVAL));
        unsafe { libc::close(raw) };
    }
}
