//! Native host backing for Hyper Linux. Linux ELF libraries stay in the guest.
//! No guest payload is copied, and no worker or helper process is started.
#![cfg(unix)]

use std::ffi::c_void;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;
use std::{io, panic};
pub mod local_fd;
pub mod drm;
pub mod prime;

/// One independently owned MAP_SHARED view. Closing or replacing the guest fd
/// cannot revoke this mapping. A fork child imports a duplicated fd, never a pointer.
pub struct Backing {
    fd: OwnedFd,
    base: NonNull<c_void>,
    mapped_len: usize,
    delta: usize,
    offset: u64,
    len: usize,
}

impl Backing {
    pub fn map(fd: BorrowedFd<'_>, offset: u64, len: usize, writable: bool) -> io::Result<Self> {
        if len == 0 || offset & 4095 != 0 || offset > i64::MAX as u64 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // SAFETY: sysconf has no pointer arguments or ownership effects.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page < 4096 || !(page as usize).is_power_of_two() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let page = page as usize;
        let delta = (offset % page as u64) as usize;
        let mapped_len = delta
            .checked_add(len)
            .and_then(|n| n.checked_add(page - 1))
            .map(|n| n & !(page - 1))
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        let file_offset = offset - delta as u64;
        let end = offset
            .checked_add(len as u64)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        let fd = fd.try_clone_to_owned()?;
        // SAFETY: fstat initializes the supplied writable stat object.
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstat succeeded.
        let st = unsafe { st.assume_init() };
        // Mapping devices requires a different, audited import contract.
        if st.st_mode & libc::S_IFMT != libc::S_IFREG || st.st_size < 0 || end > st.st_size as u64 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let prot = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
        // SAFETY: fd is retained, file extent was checked, no fixed address is requested.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mapped_len,
                prot,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                file_offset as libc::off_t,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            base: NonNull::new(base).expect("mmap returned null"),
            mapped_len,
            delta,
            offset,
            len,
        })
    }

    pub fn info(&self) -> Mapping {
        Mapping {
            base: self.base.as_ptr(),
            mapped_len: self.mapped_len as u64,
            data_offset: self.delta as u64,
            len: self.len as u64,
            file_offset: self.offset,
        }
    }

    pub fn export_fd(&self) -> io::Result<OwnedFd> {
        self.fd.try_clone()
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        // SAFETY: this exact view belongs to this Backing and no other object unmaps it.
        unsafe {
            libc::munmap(self.base.as_ptr(), self.mapped_len);
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Mapping {
    pub base: *mut c_void,
    pub mapped_len: u64,
    pub data_offset: u64,
    pub len: u64,
    pub file_offset: u64,
}

// All exported calls catch panics. Handles and output pointers are caller-owned
// references as specified in include/wc_hyperlinux.h; Rust owns allocation/free.
fn boundary(f: impl FnOnce() -> io::Result<()>) -> i32 {
    match panic::catch_unwind(panic::AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => -e.raw_os_error().unwrap_or(libc::EIO),
        Err(_) => -libc::EIO,
    }
}

/// # Safety
/// `out` and `info` must be writable. `fd` must remain valid during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_hl_backing_map(
    fd: i32,
    offset: u64,
    len: u64,
    writable: i32,
    out: *mut *mut Backing,
    info: *mut Mapping,
) -> i32 {
    if out.is_null() || info.is_null() {
        return -libc::EINVAL;
    }
    // SAFETY: checked nonnull, caller guarantees writable storage.
    unsafe {
        *out = std::ptr::null_mut();
    }
    boundary(|| {
        let len =
            usize::try_from(len).map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        if fd < 0 {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        // SAFETY: borrowed only during this call; Backing duplicates it.
        let b = Box::new(Backing::map(
            unsafe { BorrowedFd::borrow_raw(fd) },
            offset,
            len,
            writable != 0,
        )?);
        unsafe {
            *info = b.info();
            *out = Box::into_raw(b);
        }
        Ok(())
    })
}

/// # Safety
/// Handle must be live and exclusively released exactly once, after HVF unmap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_hl_backing_release(handle: *mut Backing) {
    let _ = panic::catch_unwind(|| {
        if !handle.is_null() {
            // SAFETY: caller transfers the one Box ownership back to Rust.
            drop(unsafe { Box::from_raw(handle) });
        }
    });
}

/// # Safety
/// Handle must be live; output receives an independently owned CLOEXEC fd.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_hl_backing_export_fd(handle: *const Backing, out: *mut i32) -> i32 {
    use std::os::fd::IntoRawFd;
    if handle.is_null() || out.is_null() {
        return -libc::EINVAL;
    }
    boundary(|| {
        // SAFETY: caller holds a live handle and writable out pointer.
        unsafe {
            *out = (*handle).export_fd()?.into_raw_fd();
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn independent_shared_views_survive_descriptor_close_and_unlink() {
        let path = std::env::temp_dir().join(format!("wc-hl-{}", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.set_len(65536).unwrap();
        let a = Backing::map(file.as_fd(), 4096, 8192, true).unwrap();
        let inherited = a.export_fd().unwrap();
        let b = Backing::map(inherited.as_fd(), 4096, 8192, true).unwrap();
        drop(file);
        drop(inherited);
        std::fs::remove_file(path).unwrap();
        fn at(b: &Backing) -> &AtomicU32 {
            unsafe {
                &*((b.info().base as *mut u8).add(b.info().data_offset as usize)
                    as *const AtomicU32)
            }
        }
        at(&a).store(0x1234, Ordering::Release);
        assert_eq!(at(&b).load(Ordering::Acquire), 0x1234);
        drop(a);
        at(&b).store(0xabcd, Ordering::Release);
        assert_eq!(at(&b).load(Ordering::Acquire), 0xabcd);
    }

    #[test]
    fn ffi_rejects_invalid_extents_and_initializes_failed_handle() {
        let mut handle = std::ptr::dangling_mut::<Backing>();
        let mut info = Mapping {
            base: std::ptr::null_mut(),
            mapped_len: 0,
            data_offset: 0,
            len: 0,
            file_offset: 0,
        };
        assert_eq!(
            unsafe { wc_hl_backing_map(-1, 0, 4096, 1, &mut handle, &mut info) },
            -libc::EBADF
        );
        assert!(handle.is_null());
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(Backing::map(file.as_fd(), 0, 4096, true).is_err());
        assert!(Backing::map(file.as_fd(), u64::MAX, 4096, true).is_err());
    }
}
