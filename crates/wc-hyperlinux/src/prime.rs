//! Bounded native PRIME capability registration. The compositor retains the
//! original file and immutable header; frame pages never cross this socket.
use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;
use sv_drm::token_wire::Token;

fn validate(bytes: &[u8]) -> io::Result<Token> {
    let token = Token::decode(bytes).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    if bytes.len() > 448 || token.encode() != bytes {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(token)
}
/// # Safety
/// Bytes point to a readable metadata buffer of the supplied bounded length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_hl_token_validate(bytes: *const u8, len: u64) -> i32 {
    if bytes.is_null() || len > 448 {
        return -libc::EINVAL;
    }
    catch_unwind(AssertUnwindSafe(|| {
        validate(unsafe { std::slice::from_raw_parts(bytes, len as usize) })
    }))
    .map_or(-libc::EIO, |r| {
        r.map_or_else(|e| -e.raw_os_error().unwrap_or(libc::EIO), |_| 0)
    })
}
/// # Safety
/// fd is live, bytes readable, and out points to one writable opaque slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_hl_prime_register(
    fd: i32,
    bytes: *const u8,
    len: u64,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() || bytes.is_null() || len > 448 {
        return -libc::EINVAL;
    }
    unsafe {
        *out = std::ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
        let bytes = unsafe { std::slice::from_raw_parts(bytes, len as usize) };
        let token = validate(bytes)?;
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if st.st_uid != unsafe { libc::geteuid() }
            || st.st_mode & libc::S_IFMT != libc::S_IFREG
            || st.st_size < 0
            || (st.st_size as u64) < token.size
        {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let path = std::env::var_os("WC_HL_PRIME")
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        #[cfg(target_os = "macos")]
        {
            let (mut uid, mut gid) = (0, 0);
            if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0
                || uid != unsafe { libc::geteuid() }
            {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
        }
        let mut request = [0u8; 452];
        request[..4].copy_from_slice(&(len as u32).to_le_bytes());
        request[4..4 + bytes.len()].copy_from_slice(bytes);
        crate::local_fd::send_tag(&stream, request[0], fd)?;
        stream.write_all(&request[1..])?;
        let mut ack = [1u8];
        stream.read_exact(&mut ack)?;
        if ack[0] != 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // Opaque owner is independently cloneable for descriptor and mapping
        // lifetimes. The last owner closes the registration connection.
        unsafe {
            *out = Box::into_raw(Box::new(Arc::new(stream))).cast();
        }
        Ok(())
    }));
    result.map_or(-libc::EIO, |r| {
        r.map_or_else(|e| -e.raw_os_error().unwrap_or(libc::EIO), |_| 0)
    })
}
/// # Safety
/// Handle is null or points to a live registration owner.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_hl_prime_clone(handle: *const c_void) -> *mut c_void {
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    catch_unwind(AssertUnwindSafe(|| {
        let owner = unsafe { &*handle.cast::<Arc<UnixStream>>() };
        Box::into_raw(Box::new(owner.clone())).cast()
    }))
    .unwrap_or(std::ptr::null_mut())
}
/// # Safety
/// Handle is null or is one uniquely owned registration reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_hl_prime_release(handle: *mut c_void) {
    if !handle.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(handle.cast::<Arc<UnixStream>>()));
        }));
    }
}
