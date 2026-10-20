use std::ffi::{CStr, c_char, c_void};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backing {
    Broker { id: u64, offset: u64 },
    /// Only an explicitly configured disposable fixture may use this variant.
    Fixture { path: std::path::PathBuf, offset: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileMetadata {
    pub device: u64,
    pub inode: u64,
}

/// Diagnostic metadata only. Anonymous fds can share it; this is not authorization.
pub fn file_metadata(fd: &OwnedFd) -> io::Result<FileMetadata> {
    let f = std::fs::File::from(fd.try_clone()?);
    let m = f.metadata()?;
    Ok(FileMetadata { device: m.dev(), inode: m.ino() })
}

/// Compare kernel open-file descriptions, including SCM_RIGHTS/dup aliases.
/// A denied or unavailable comparison is an error, never an identity match.
#[cfg(target_os = "linux")]
pub fn same_open_file(a: &OwnedFd, b: &OwnedFd) -> io::Result<bool> {
    let me = unsafe { libc::getpid() };
    // KCMP_FILE compares the actual struct file pointers. It does not rely on
    // fstat identity, fd numbers, seek side effects, or caller-owned tokens.
    let result = unsafe { libc::syscall(libc::SYS_kcmp, me, me, 0u32, a.as_raw_fd(), b.as_raw_fd()) };
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result == 0) }
}

#[cfg(not(target_os = "linux"))]
pub fn same_open_file(_a: &OwnedFd, _b: &OwnedFd) -> io::Result<bool> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "exact native-shm fd identity requires Linux KCMP_FILE"))
}

struct Lease {
    client: wc_broker::client::Client,
    id: u64,
}
impl Drop for Lease {
    fn drop(&mut self) { let _ = self.client.close(self.id); }
}

pub struct Mapping {
    address: *mut c_void,
    mapped_len: usize,
    delta: usize,
    len: usize,
    fd: OwnedFd,
    /// Offset of the allocation in this fd, including its page-aligned part.
    fd_offset: u64,
    // Fields drop after Drop::drop unmaps. The broker lease is last.
    _lease: Option<Lease>,
}

// SAFETY: Mapping owns its address and lease and never creates Rust references
// to the shared bytes. Moving ownership between proxy threads does not change
// the address, and final unmapping still happens exactly once.
unsafe impl Send for Mapping {}

impl Mapping {
    pub fn open(backing: &Backing, len: u64) -> io::Result<Self> {
        Self::open_at(backing, len, &wc_broker::client::path())
    }

    fn open_at(backing: &Backing, len: u64, broker: &str) -> io::Result<Self> {
        if len == 0 || len > i32::MAX as u64 { return Err(invalid("invalid GTK allocation size")); }
        let (fd, offset, lease) = match backing {
            Backing::Broker { id, offset } => {
                let mut client = wc_broker::client::Client::connect(broker)?;
                client.hello("gtk-shm")?;
                let (region, fds) = client.open(*id)?;
                let lease = Lease { client, id: *id };
                let end = offset.checked_add(len).ok_or_else(|| invalid("allocation overflow"))?;
                if end > region.len { return Err(invalid("allocation exceeds the broker region")); }
                // GTK needs one ordinary fd for wl_shm. Never substitute a
                // stitched copy if the kernel's per-part limits split it.
                let selected = region.pieces.iter().zip(fds).find_map(|(piece, fd)| {
                    (piece.offset <= *offset && piece.offset.checked_add(piece.len).is_some_and(|e| e >= end))
                        .then(|| (fd, *offset - piece.offset))
                }).ok_or_else(|| invalid("GTK allocation crosses bounded broker fd parts"))?;
                (selected.0, selected.1, Some(lease))
            }
            Backing::Fixture { path, offset } => {
                let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
                let end = offset.checked_add(len).ok_or_else(|| invalid("fixture allocation overflow"))?;
                if end > file.metadata()?.len() { return Err(invalid("allocation exceeds fixture file")); }
                (OwnedFd::from(file), *offset, None)
            }
        };
        if offset.checked_add(len).is_none_or(|e| e > i32::MAX as u64) {
            return Err(invalid("wl_shm offset exceeds its protocol limit"));
        }
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page <= 0 { return Err(invalid("cannot determine mapping page size")); }
        let aligned = offset / page as u64 * page as u64;
        let delta = usize::try_from(offset - aligned).map_err(|_| invalid("mapping offset overflow"))?;
        let len = usize::try_from(len).map_err(|_| invalid("mapping size overflow"))?;
        let mapped_len = delta.checked_add(len).ok_or_else(|| invalid("mapping size overflow"))?;
        // SAFETY: a checked nonempty range of a bounded fd. No Rust reference
        // is made to these bytes; Cairo and the compositor share them.
        let address = unsafe { libc::mmap(std::ptr::null_mut(), mapped_len, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED, fd.as_raw_fd(), aligned as libc::off_t) };
        if address == libc::MAP_FAILED { return Err(io::Error::last_os_error()); }
        Ok(Self { address, mapped_len, delta, len, fd, fd_offset: offset, _lease: lease })
    }
    pub fn fd(&self) -> &OwnedFd { &self.fd }
    pub fn fd_offset(&self) -> u64 { self.fd_offset }
    pub fn len(&self) -> usize { self.len }
    pub fn is_empty(&self) -> bool { false }
    pub fn data(&self) -> *mut c_void {
        // SAFETY: delta is inside the live mapping.
        unsafe { self.address.cast::<u8>().add(self.delta).cast() }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: exactly the successful mapping this object owns.
        unsafe { libc::munmap(self.address, self.mapped_len); }
    }
}
fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

/// # Safety
/// `path`, when non-null, is a live NUL-terminated C string. The caller must
/// keep the returned mapping alive until every Cairo use has ended.
/// Fixture paths are accepted only when the library was explicitly enabled
/// for fixtures. Production calls always use broker-backed bounded fds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_native_shm_open(kind: u32, id: u64, path: *const c_char, offset: u64, len: u64) -> *mut Mapping {
    let result = std::panic::catch_unwind(|| {
        let backing = match kind {
            1 => Backing::Broker { id, offset },
            2 if std::env::var_os("WC_NATIVE_SHM_FIXTURE").is_some_and(|v| v == "1") && !path.is_null() => {
                // SAFETY: the C ABI requires a live NUL-terminated path.
                let p = unsafe { CStr::from_ptr(path) }.to_str().map_err(|_| invalid("fixture path is not UTF-8"))?;
                Backing::Fixture { path: p.into(), offset }
            }
            _ => return Err(invalid("unsupported native-shm backing")),
        };
        Mapping::open(&backing, len)
    });
    match result {
        Ok(Ok(mapping)) => Box::into_raw(Box::new(mapping)),
        Ok(Err(error)) => { eprintln!("wc-native-shm: allocation refused: {error}"); std::ptr::null_mut() }
        Err(_) => { eprintln!("wc-native-shm: allocation panicked"); std::ptr::null_mut() }
    }
}

/// # Safety
/// `mapping` is null or a live result of wc_native_shm_open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_native_shm_data(mapping: *const Mapping) -> *mut c_void {
    unsafe { mapping.as_ref() }.map_or(std::ptr::null_mut(), Mapping::data)
}
/// # Safety
/// `mapping` is null or a live result of wc_native_shm_open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_native_shm_fd(mapping: *const Mapping) -> i32 {
    unsafe { mapping.as_ref() }.map_or(-1, |m| m.fd.as_raw_fd())
}
/// # Safety
/// `mapping` is null or a live result of wc_native_shm_open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_native_shm_offset(mapping: *const Mapping) -> u64 {
    unsafe { mapping.as_ref() }.map_or(0, Mapping::fd_offset)
}
/// # Safety
/// `mapping` is null or a live result, closed exactly once, after Cairo's use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_native_shm_close(mapping: *mut Mapping) {
    if !mapping.is_null() { drop(unsafe { Box::from_raw(mapping) }); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    #[test]
    #[cfg(target_os = "linux")]
    fn exact_file_comparison_rejects_foreign_anonymous_fds_with_identical_stat_metadata() {
        use std::os::fd::FromRawFd;
        let make = || {
            let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
            assert!(fd >= 0);
            unsafe { OwnedFd::from_raw_fd(fd) }
        };
        let a = make(); let foreign = make(); let duplicate = a.try_clone().unwrap();
        // Linux eventfd is a concrete example of the anon-inode collision.
        assert_eq!(file_metadata(&a).unwrap(), file_metadata(&foreign).unwrap());
        assert!(same_open_file(&a, &duplicate).unwrap());
        assert!(!same_open_file(&a, &foreign).unwrap());
    }

    #[test]
    fn scm_rights_preserves_the_painted_backing_after_the_creator_mapping_closes() {
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;
        use sv_transport::localfd::{send_with_fds, recv_with_fds};
        let p = std::env::temp_dir().join(format!("native-shm-rights-{}", std::process::id()));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&p).unwrap();
        file.set_len(8192).unwrap();
        let m = Mapping::open(&Backing::Fixture { path: p.clone(), offset: 0 }, 8192).unwrap();
        let (tx, rx) = UnixStream::pair().unwrap();
        let expected = file_metadata(m.fd()).unwrap();
        send_with_fds(&tx, b"original-backing", &[m.fd().as_fd()]).unwrap();
        let mut message = Vec::new();
        let mut fds = recv_with_fds(&rx, &mut message).unwrap();
        assert_eq!(message, b"original-backing");
        let fd = fds.remove(0);
        assert_eq!(file_metadata(&fd).unwrap(), expected);
        #[cfg(target_os = "linux")]
        assert!(same_open_file(m.fd(), &fd).unwrap());
        let peer = unsafe { libc::mmap(std::ptr::null_mut(), 8192, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        assert_ne!(peer, libc::MAP_FAILED);
        unsafe { std::ptr::write_volatile(m.data().cast::<u64>(), 0x1122_3344_5566_7788); }
        assert_eq!(unsafe { std::ptr::read_volatile(peer.cast::<u64>()) }, 0x1122_3344_5566_7788);
        drop(m);
        // The receiver still holds the same original pages after Cairo closes.
        unsafe { std::ptr::write_volatile(peer.cast::<u64>(), 0x8877_6655_4433_2211); }
        let mut bytes = [0;8];
        use std::os::unix::fs::FileExt;
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(u64::from_ne_bytes(bytes), 0x8877_6655_4433_2211);
        unsafe { libc::munmap(peer, 8192); }
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn broker_retirement_waits_for_the_cairo_mapping_lease() {
        use wc_broker::core::{Release, User};
        use wc_broker::server::{Broker, Config};
        use wc_broker::sys::unix::Unix;
        let dir = std::env::temp_dir().join(format!("native-shm-broker-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("broker.sock");
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(dir.join("pages")).unwrap();
        file.set_len(8192).unwrap();
        let broker = std::sync::Arc::new(Broker::new(Unix::bind(&path, Vec::new()).unwrap(), Config {
            devices: Vec::new(), services: Vec::new(), keys: None, release: Release::OwnerFree, budget: None, run: 7,
        }));
        let uid = unsafe { libc::geteuid() };
        let id = broker.publish(User::uid(uid), "gtk-test", 9, vec![(Unix::object(file.into()), 0, 8192)]);
        let serving = broker.clone();
        std::thread::spawn(move || { let _ = serving.serve(); });
        let (tx, rx) = std::sync::mpsc::channel();
        broker.on_drained(Box::new(move |id, epoch| { tx.send((id, epoch)).unwrap(); }));
        let m = Mapping::open_at(&Backing::Broker { id, offset: 0 }, 8192, path.to_str().unwrap()).unwrap();
        assert_eq!(broker.holders(id), vec![std::process::id()]);
        broker.retire(id);
        assert!(rx.try_recv().is_err(), "retirement released live Cairo pages");
        unsafe { std::ptr::write_volatile(m.data().cast::<u64>(), 0x12345678); }
        assert!(Mapping::open_at(&Backing::Broker { id, offset: 0 }, 8192, path.to_str().unwrap()).is_err());
        drop(m);
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), (id, 9));
        assert!(broker.holders(id).is_empty());
        broker.unpublish(id);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn paints_the_original_pages_with_an_unaligned_allocation_offset() {
        let p = std::env::temp_dir().join(format!("native-shm-original-{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&p).unwrap();
        file.set_len(16384).unwrap();
        file.seek(SeekFrom::Start(1024)).unwrap(); file.write_all(b"before").unwrap();
        let m = Mapping::open(&Backing::Fixture { path: p.clone(), offset: 1024 }, 4096).unwrap();
        unsafe { std::ptr::copy_nonoverlapping(b"paint!".as_ptr(), m.data().cast(), 6); }
        let fd = m.fd.try_clone().unwrap();
        drop(m);
        file.seek(SeekFrom::Start(1024)).unwrap(); let mut actual = [0;6]; file.read_exact(&mut actual).unwrap();
        assert_eq!(&actual, b"paint!");
        assert_eq!(file_metadata(&fd).unwrap().inode, file.metadata().unwrap().ino());
        assert!(Mapping::open(&Backing::Fixture { path: p.clone(), offset: 16384 }, 1).is_err());
        std::fs::remove_file(p).unwrap();
    }
}
