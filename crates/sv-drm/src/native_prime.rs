//! Native PRIME registration, without a claim of Darwin kernel seals.
//!
//! The authenticated runtime registers a retained original file and canonical
//! bounded metadata. Import uses that snapshot, never a fresh mutable decode.
//! A header comparison detects mutation visible at import; it cannot prevent a
//! native writer racing or changing the file later. Guest seal enforcement is
//! the runtime's responsibility. Consumers must hold `Arc<Imported>` through
//! their actual completion, alongside the resolved allocation/IOSurface guard.
//! Only control metadata is copied here. No frame payload is read or copied.
use crate::token_wire::Token;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

pub const MAX_HEADER: usize = 448;
/// Fixed wire request: little-endian header length, then zero-padded header.
/// One SCM_RIGHTS descriptor accompanies the first byte. ACK is 0 or 1.
pub const REQUEST_LEN: usize = MAX_HEADER + 4;

fn invalid(reason: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, reason) }
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct Identity { dev: u64, ino: u64 }
fn identity(fd: BorrowedFd<'_>) -> io::Result<(Identity, u64)> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid borrowed descriptor and writable stat storage.
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 { return Err(io::Error::last_os_error()); }
    let st = unsafe { st.assume_init() };
    if st.st_mode & libc::S_IFMT != libc::S_IFREG || st.st_size < 0 { return Err(invalid("token is not a regular file")); }
    Ok((Identity { dev: st.st_dev as u64, ino: st.st_ino as u64 }, st.st_size as u64))
}
fn compare(fd: BorrowedFd<'_>, expected: &[u8]) -> io::Result<()> {
    let mut got = [0u8; MAX_HEADER];
    let mut offset = 0;
    while offset < expected.len() {
        // SAFETY: bounded writable slice, explicit offset does not alter fd position.
        let n = unsafe { libc::pread(fd.as_raw_fd(), got[offset..expected.len()].as_mut_ptr().cast(), expected.len()-offset, offset as libc::off_t) };
        if n < 0 { let e = io::Error::last_os_error(); if e.kind() == io::ErrorKind::Interrupted { continue; } return Err(e); }
        if n == 0 { return Err(invalid("truncated token header")); }
        offset += n as usize;
    }
    if &got[..expected.len()] != expected { return Err(invalid("mutated token header")); }
    Ok(())
}
/// Immutable metadata and the original retained file identity. Holding this
/// keeps the inode alive across close, dup, SCM_RIGHTS and fork aliases.
pub struct Imported { fd: OwnedFd, token: Token, header: Box<[u8]>, len: u64 }
impl Imported {
    pub fn token(&self) -> &Token { &self.token }
    pub fn validate(&self) -> io::Result<()> {
        let (_, len) = identity(self.fd())?;
        if len != self.len { return Err(invalid("token size changed")); }
        compare(self.fd(), &self.header)
    }
    pub fn fd(&self) -> BorrowedFd<'_> { self.fd.as_fd() }
}
struct Entry { imported: Arc<Imported>, owners: HashSet<u64> }
#[derive(Default)]
struct State { next: u64, entries: HashMap<Identity, Entry> }
#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<State>>);
impl Registry {
    pub fn register(&self, fd: BorrowedFd<'_>, canonical: &[u8]) -> io::Result<Owner> {
        let owner = self.owner()?;
        owner.register(fd.try_clone_to_owned()?, canonical)?;
        Ok(owner)
    }
    /// Call only after authenticating the native connection's uid and binding
    /// its lifetime to the existing broker owner/run. No guest-supplied UID.
    pub fn owner(&self) -> io::Result<Owner> {
        let mut s = self.0.lock().map_err(|_| invalid("registry poisoned"))?;
        s.next = s.next.checked_add(1).ok_or_else(|| invalid("owner generation exhausted"))?;
        Ok(Owner { registry: self.clone(), id: s.next })
    }
    pub fn import(&self, fd: BorrowedFd<'_>) -> io::Result<Arc<Imported>> {
        let (id, len) = identity(fd)?;
        let value = self.0.lock().map_err(|_| invalid("registry poisoned"))?.entries.get(&id)
            .map(|e| e.imported.clone()).ok_or_else(|| invalid("unregistered token"))?;
        if len != value.len { return Err(invalid("token size changed")); }
        compare(fd, &value.header)?;
        Ok(value)
    }
}
pub struct Owner { registry: Registry, id: u64 }
impl Owner {
    pub fn register(&self, fd: OwnedFd, header: &[u8]) -> io::Result<()> {
        if header.is_empty() || header.len() > MAX_HEADER { return Err(invalid("token header length")); }
        let token = Token::decode(header).map_err(|e| invalid(&e.to_string()))?;
        if token.encode() != header { return Err(invalid("noncanonical token header")); }
        let (id, len) = identity(fd.as_fd())?;
        let minimum = if token.record.is_empty() { header.len() as u64 } else { 8192 };
        if len != token.size.max(minimum) { return Err(invalid("token allocation length mismatch")); }
        compare(fd.as_fd(), header)?;
        let mut state = self.registry.0.lock().map_err(|_| invalid("registry poisoned"))?;
        if let Some(existing) = state.entries.get_mut(&id) {
            if existing.imported.header.as_ref() != header || existing.imported.len != len { return Err(invalid("identity already registered differently")); }
            existing.owners.insert(self.id);
        } else {
            state.entries.insert(id, Entry { imported: Arc::new(Imported { fd, token, header: header.into(), len }), owners: HashSet::from([self.id]) });
        }
        Ok(())
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if let Ok(mut s) = self.registry.0.lock() { s.entries.retain(|_, e| { e.owners.remove(&self.id); !e.owners.is_empty() }); }
    }
}

fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        if unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) } != 0 { return Err(io::Error::last_os_error()); }
        if len as usize != std::mem::size_of_val(&cred) { return Err(invalid("peer credentials length")); }
        Ok(cred.uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut uid = 0; let mut gid = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 { return Err(io::Error::last_os_error()); }
        Ok(uid)
    }
}
fn receive(stream: &mut UnixStream) -> io::Result<Option<(OwnedFd, [u8; REQUEST_LEN])>> {
    let mut bytes = [0u8; REQUEST_LEN];
    // Aligned, bounded control buffer. Adopt every received FD before checking
    // count/truncation, so malformed requests cannot leak descriptor rights.
    let mut control = [0usize; 64];
    let mut iov = libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: 1 };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov; msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast(); msg.msg_controllen = std::mem::size_of_val(&control) as _;
    let n = loop {
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted { continue; }
        break n;
    };
    if n < 0 { return Err(io::Error::last_os_error()); }
    let mut fds = Vec::new(); let mut bad = false;
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let base = libc::CMSG_LEN(0) as usize;
                let len = (*c).cmsg_len as usize;
                if len < base || (len-base) % std::mem::size_of::<libc::c_int>() != 0 { bad = true; break; }
                for i in 0..(len-base)/std::mem::size_of::<libc::c_int>() {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>().add(i))));
                }
            } else { bad = true; }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if n == 0 && fds.is_empty() { return Ok(None); }
    if bad || msg.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 || fds.len() != 1 { return Err(invalid("expected exactly one token FD")); }
    let fd = fds.pop().unwrap();
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 { return Err(io::Error::last_os_error()); }
    stream.read_exact(&mut bytes[1..])?;
    Ok(Some((fd, bytes)))
}
/// Run in the existing native server's connection worker, after broker
/// owner/run binding. Socket discovery/binding belongs to that server, not to
/// this module. EOF revokes discovery. Existing imported guards remain valid.
pub fn serve_connection(mut stream: UnixStream, expected_uid: libc::uid_t, registry: &Registry) -> io::Result<()> {
    if peer_uid(&stream)? != expected_uid { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "native PRIME peer uid")); }
    let owner = registry.owner()?;
    while let Some((fd, request)) = receive(&mut stream)? {
        let n = u32::from_le_bytes(request[..4].try_into().unwrap()) as usize;
        let result = if n == 0 || n > MAX_HEADER || request[4+n..].iter().any(|b| *b != 0) {
            Err(invalid("invalid registration frame"))
        } else { owner.register(fd, &request[4..4+n]) };
        stream.write_all(&[u8::from(result.is_err())])?;
        if result.is_err() { return result; }
    }
    Ok(())
}
/// Synchronous registration. Keep this connection alive until another owner
/// has registered its alias or the allocation is no longer discoverable.
pub fn register(stream: &mut UnixStream, fd: BorrowedFd<'_>, header: &[u8]) -> io::Result<()> {
    if header.is_empty() || header.len() > MAX_HEADER { return Err(invalid("token header length")); }
    let mut request = [0u8; REQUEST_LEN];
    request[..4].copy_from_slice(&(header.len() as u32).to_le_bytes()); request[4..4+header.len()].copy_from_slice(header);
    let mut control = [0usize; 8];
    let mut iov = libc::iovec { iov_base: request.as_mut_ptr().cast(), iov_len: 1 };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov; msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast(); msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) } as _;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET; (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>(), fd.as_raw_fd());
    }
    let n = loop {
        let n = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted { continue; }
        break n;
    };
    if n != 1 { return Err(if n < 0 { io::Error::last_os_error() } else { invalid("short registration send") }); }
    stream.write_all(&request[1..])?;
    let mut ack = [1]; stream.read_exact(&mut ack)?;
    if ack != [0] { return Err(invalid("registration rejected")); }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    fn fixture() -> (OwnedFd, Vec<u8>) {
        let token = Token { size: 16384, nonce: [7;16], name: "/tmp/sv-shared-test".into(), record: String::new() };
        let bytes = token.encode();
        let fd = unsafe { libc::memfd_create(c"native-prime-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), token.size as _) }, 0);
        assert_eq!(unsafe { libc::pwrite(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) }, bytes.len() as _);
        (fd, bytes)
    }
    #[test]
    fn retained_identity_aliases_and_completion_outlive_revocation() {
        let registry = Registry::default(); let owner = registry.owner().unwrap();
        let (fd, bytes) = fixture(); let alias = fd.try_clone().unwrap();
        assert!(registry.import(fd.as_fd()).is_err());
        owner.register(fd, &bytes).unwrap();
        let consumer = registry.import(alias.as_fd()).unwrap();
        drop(owner);
        assert!(registry.import(alias.as_fd()).is_err());
        drop(alias);
        assert_eq!(consumer.token().encode(), bytes);
        compare(consumer.fd(), &bytes).unwrap();
    }
    #[test]
    fn mutated_header_and_truncated_file_rejected() {
        let registry = Registry::default(); let owner = registry.owner().unwrap();
        let (fd, bytes) = fixture(); owner.register(fd.try_clone().unwrap(), &bytes).unwrap();
        assert_eq!(unsafe { libc::pwrite(fd.as_raw_fd(), b"X".as_ptr().cast(), 1, 0) }, 1);
        assert!(registry.import(fd.as_fd()).is_err());
        assert!(owner.register(fd.try_clone().unwrap(), &bytes).is_err());
        assert_eq!(unsafe { libc::pwrite(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) }, bytes.len() as _);
        assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 9000) }, 0);
        assert!(registry.import(fd.as_fd()).is_err());
    }
    #[test]
    fn copied_header_does_not_authorize_different_identity() {
        let registry = Registry::default(); let owner = registry.owner().unwrap();
        let (fd, bytes) = fixture(); let (other, _) = fixture();
        owner.register(fd, &bytes).unwrap(); assert!(registry.import(other.as_fd()).is_err());
        let mut extra = bytes.clone(); extra.push(0);
        assert!(owner.register(other, &extra).is_err());
    }
    #[test]
    fn scm_rights_and_multiple_owner_disconnect() {
        let registry = Registry::default(); let (fd, bytes) = fixture();
        let owner = registry.owner().unwrap(); owner.register(fd.try_clone().unwrap(), &bytes).unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let copy = registry.clone();
        let worker = std::thread::spawn(move || serve_connection(server, unsafe { libc::getuid() }, &copy));
        register(&mut client, fd.as_fd(), &bytes).unwrap();
        drop(owner);
        let consumer = registry.import(fd.as_fd()).unwrap();
        drop(client); worker.join().unwrap().unwrap();
        assert!(registry.import(fd.as_fd()).is_err());
        assert_eq!(consumer.token().encode(), bytes);
    }
    #[test]
    fn wrong_native_uid_is_rejected() {
        let registry = Registry::default(); let (_client, server) = UnixStream::pair().unwrap();
        let wrong_uid = unsafe { libc::getuid() }.wrapping_add(1);
        assert_eq!(serve_connection(server, wrong_uid, &registry).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}

/// The original token's producer-owned sync page. The runtime must forbid
/// shrink and writable header maps. Darwin itself does not provide that seal.
pub struct SyncPage { base: *mut u8, _token: Arc<Imported> }
unsafe impl Send for SyncPage {}
unsafe impl Sync for SyncPage {}
impl SyncPage {
    pub fn open(token: Arc<Imported>) -> io::Result<Option<Self>> {
        if token.token.record.is_empty() { return Ok(None); }
        compare(token.fd(), &token.header)?;
        let (_, len) = identity(token.fd())?;
        if len != token.len || len < 8192 { return Err(invalid("sync page length changed")); }
        // Map from offset zero, including on Darwin with 16 KiB native pages.
        let base = unsafe { libc::mmap(std::ptr::null_mut(), 8192, libc::PROT_READ, libc::MAP_SHARED, token.fd.as_raw_fd(), 0) };
        if base == libc::MAP_FAILED { return Err(io::Error::last_os_error()); }
        let page = Self { base: base.cast(), _token: token };
        if unsafe { std::slice::from_raw_parts(page.base.add(4096), 8) } != crate::token_wire::SYNC_MAGIC { return Err(invalid("sync page magic")); }
        Ok(Some(page))
    }
    pub fn load(&self) -> u64 {
        unsafe { &*self.base.add(4104).cast::<std::sync::atomic::AtomicU64>() }.load(std::sync::atomic::Ordering::Acquire)
    }
}
impl Drop for SyncPage {
    fn drop(&mut self) { unsafe { libc::munmap(self.base.cast(), 8192); } }
}
