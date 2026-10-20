//! The existing DRM ioctl state machine, called inside hl rather than via CUSE.
//! Callbacks copy only bounded ioctl metadata. Allocation payloads stay mapped.
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use sv_drm::cuse::{Device, InHeader, IoVec, IoctlIn, IoctlReply, OpenIn};
use sv_drm::drm::{DrmNode, FdResolver};
use sv_drm::relay_api::Relay;
use sv_drm::token_wire::Token;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Callbacks {
    pub context: *mut c_void,
    pub read: unsafe extern "C" fn(*mut c_void, u64, *mut u8, u64) -> i32,
    pub write: unsafe extern "C" fn(*mut c_void, u64, *const u8, u64) -> i32,
    /// Return the immutable capability header, after validating native backing
    /// identity and runtime sealing. Returning mutable file bytes is invalid.
    pub token: unsafe extern "C" fn(*mut c_void, i32, *mut u8, u64) -> i32,
    pub relay: unsafe extern "C" fn(*mut c_void, i32, *const u8, u64) -> i32,
}
// SAFETY: callbacks are installed only while the owning node's mutex is held,
// invoked synchronously on that thread, and cleared before releasing the lock.
unsafe impl Send for Callbacks {}
struct Resolver(Arc<Mutex<Option<Callbacks>>>);
impl FdResolver for Resolver {
    fn token(&self, _pid: u32, fd: i32) -> Result<Token, i32> {
        let cb = self.0.lock().map_err(|_| libc::EIO)?.ok_or(libc::EIO)?;
        let mut bytes = [0u8; 448];
        let n = unsafe { (cb.token)(cb.context, fd, bytes.as_mut_ptr(), bytes.len() as u64) };
        if n < 0 {
            return Err(-n);
        }
        if n as usize > bytes.len() {
            return Err(libc::EINVAL);
        }
        Token::decode(&bytes[..n as usize]).map_err(|_| libc::EINVAL)
    }
}
impl Relay for Resolver {
    fn connect(&self, _uid: u32, _pid: u32, fd: i32, service: &str) -> Result<(), i32> {
        let cb = self.0.lock().map_err(|_| libc::EIO)?.ok_or(libc::EIO)?;
        let r = unsafe { (cb.relay)(cb.context, fd, service.as_ptr(), service.len() as u64) };
        if r < 0 { Err(-r) } else { Ok(()) }
    }
}
struct State {
    node: DrmNode<Resolver>,
    fh: u64,
    callbacks: Arc<Mutex<Option<Callbacks>>>,
}
pub struct Open(Mutex<State>);
fn header(uid: u32, pid: u32) -> InHeader {
    InHeader {
        uid,
        pid,
        ..Default::default()
    }
}
fn errno(f: impl FnOnce() -> Result<i32, i32>) -> i32 {
    catch_unwind(AssertUnwindSafe(f)).map_or(-libc::EIO, |r| r.unwrap_or_else(|e| -e))
}

/// # Safety
/// `out` must be writable and released once through wc_hl_drm_release.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_hl_drm_open(uid: u32, pid: u32, out: *mut *mut Open) -> i32 {
    if out.is_null() {
        return -libc::EINVAL;
    }
    unsafe {
        *out = std::ptr::null_mut();
    }
    errno(|| {
        let callbacks = Arc::new(Mutex::new(None));
        let mut node = DrmNode::with_relay(
            Resolver(callbacks.clone()),
            Arc::new(Resolver(callbacks.clone())),
        );
        let fh = node.open(&header(uid, pid), &OpenIn::default())?;
        unsafe {
            *out = Box::into_raw(Box::new(Open(Mutex::new(State {
                node,
                fh,
                callbacks,
            }))));
        }
        Ok(0)
    })
}
/// # Safety
/// `handle` is a live open handle. Callbacks must validate every guest range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_hl_drm_ioctl(
    handle: *mut Open,
    uid: u32,
    pid: u32,
    cmd: u32,
    arg: u64,
    cb: Callbacks,
) -> i32 {
    if handle.is_null() {
        return -libc::EBADF;
    }
    errno(|| {
        let mut state = unsafe { &*handle }.0.lock().map_err(|_| libc::EIO)?;
        *state.callbacks.lock().map_err(|_| libc::EIO)? = Some(cb);
        // Always clear guest context, including if a callback or core panics.
        struct Clear(Arc<Mutex<Option<Callbacks>>>);
        impl Drop for Clear {
            fn drop(&mut self) {
                if let Ok(mut c) = self.0.lock() {
                    *c = None;
                }
            }
        }
        let _clear = Clear(state.callbacks.clone());
        let mut inputs: Vec<IoVec> = Vec::new();
        let mut outputs: Vec<IoVec> = Vec::new();
        for _ in 0..8 {
            let mut data = Vec::new();
            for v in &inputs {
                let len = usize::try_from(v.len).map_err(|_| libc::EOVERFLOW)?;
                if len > 4096 || data.len() + len > 16384 {
                    return Err(libc::E2BIG);
                }
                let off = data.len();
                data.resize(off + len, 0);
                let r = unsafe { (cb.read)(cb.context, v.base, data[off..].as_mut_ptr(), v.len) };
                if r < 0 {
                    return Err(-r);
                }
            }
            let out_size = outputs
                .iter()
                .try_fold(0u32, |n, v| n.checked_add(v.len.try_into().ok()?))
                .ok_or(libc::E2BIG)?;
            if out_size > 16384 {
                return Err(libc::E2BIG);
            }
            let request = IoctlIn {
                fh: state.fh,
                cmd,
                arg,
                flags: 0,
                in_size: data.len() as u32,
                out_size,
            };
            match state.node.ioctl(&header(uid, pid), &request, &data) {
                IoctlReply::Error(e) => return Err(e),
                IoctlReply::Retry { input, output } => {
                    inputs = input;
                    outputs = output;
                }
                IoctlReply::Done { result, data } => {
                    if data.len() > out_size as usize {
                        return Err(libc::EIO);
                    }
                    let mut off = 0;
                    for v in outputs {
                        let n = (v.len as usize).min(data.len() - off);
                        let r = unsafe {
                            (cb.write)(cb.context, v.base, data[off..].as_ptr(), n as u64)
                        };
                        if r < 0 {
                            return Err(-r);
                        }
                        off += n;
                    }
                    return Ok(result);
                }
            }
        }
        Err(libc::ELOOP)
    })
}
/// # Safety
/// Handle was allocated by wc_hl_drm_open and has no concurrent users.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_hl_drm_release(handle: *mut Open) {
    if !handle.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(handle));
        }));
    }
}
