//! The few Win32 calls this crate needs beyond `sv_transport::winram`,
//! declared by hand as the rest of the project does.
#![allow(non_snake_case, clippy::upper_case_acronyms)]

use core::ffi::c_void;
use std::os::windows::io::FromRawHandle;

const PIPE_ACCESS_DUPLEX: u32 = 3;
const PIPE_TYPE_BYTE: u32 = 0;
const PIPE_READMODE_BYTE: u32 = 0;
const PIPE_WAIT: u32 = 0;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 8;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const INVALID_HANDLE_VALUE: isize = -1;

#[repr(C)]
struct SECURITY_ATTRIBUTES {
    len: u32,
    sd: *mut c_void,
    inherit: i32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(name: *const u16, open: u32, mode: u32, max: u32, out: u32, inb: u32, timeout: u32, sa: *mut SECURITY_ATTRIBUTES) -> isize;
    fn ConnectNamedPipe(p: isize, ov: *mut c_void) -> i32;
    fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *mut c_void, disp: u32, flags: u32, tmpl: isize) -> isize;
    fn WaitNamedPipeW(name: *const u16, ms: u32) -> i32;
    fn CloseHandle(h: isize) -> i32;
    fn GetLastError() -> u32;
    fn LocalFree(p: *mut c_void) -> *mut c_void;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn TerminateProcess(h: isize, code: u32) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(s: *const u16, rev: u32, sd: *mut *mut c_void, len: *mut u32) -> i32;
}
#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(alg: *mut c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
}
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(ms: u32) -> u32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

/// `N` bytes from the system's preferred RNG.
pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG = 2
    let st = unsafe { BCryptGenRandom(core::ptr::null_mut(), b.as_mut_ptr(), N as u32, 2) };
    assert_eq!(st, 0, "BCryptGenRandom failed: {st:#x}");
    b
}

/// 1 ms scheduler ticks, so a `sleep(1 ms)` is about 1 ms and not 15.6.
pub fn fine_timer() {
    unsafe { timeBeginPeriod(1) };
}

/// Create one more instance of the pipe and wait for a client on it.
pub fn pipe_accept(name: &str, sddl: &str) -> std::io::Result<std::fs::File> {
    let w = wide(name);
    let s = wide(sddl);
    let mut sd: *mut c_void = core::ptr::null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(s.as_ptr(), 1, &mut sd, core::ptr::null_mut()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut sa = SECURITY_ATTRIBUTES { len: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, sd, inherit: 0 };
    let h = unsafe {
        CreateNamedPipeW(
            w.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 << 10,
            64 << 10,
            0,
            &mut sa,
        )
    };
    unsafe { LocalFree(sd) };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { ConnectNamedPipe(h, core::ptr::null_mut()) } == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
        let e = std::io::Error::last_os_error();
        unsafe { CloseHandle(h) };
        return Err(e);
    }
    Ok(unsafe { std::fs::File::from_raw_handle(h as *mut c_void) })
}

/// Open the pipe, waiting up to `timeout_ms` for it to exist or for an
/// instance to come free.
pub fn pipe_connect(name: &str, timeout_ms: u32) -> std::io::Result<std::fs::File> {
    let w = wide(name);
    let t0 = std::time::Instant::now();
    loop {
        let h = unsafe { CreateFileW(w.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, core::ptr::null_mut(), OPEN_EXISTING, 0, 0) };
        if h != INVALID_HANDLE_VALUE {
            return Ok(unsafe { std::fs::File::from_raw_handle(h as *mut c_void) });
        }
        let e = unsafe { GetLastError() };
        if t0.elapsed().as_millis() as u32 >= timeout_ms {
            return Err(std::io::Error::from_raw_os_error(e as i32));
        }
        match e {
            ERROR_PIPE_BUSY => unsafe {
                WaitNamedPipeW(w.as_ptr(), 100);
            },
            ERROR_FILE_NOT_FOUND => std::thread::sleep(std::time::Duration::from_millis(50)),
            _ => return Err(std::io::Error::from_raw_os_error(e as i32)),
        }
    }
}

/// `TerminateProcess`: the crash the keeper exists for, on demand (tests).
pub fn kill(pid: u32) -> std::io::Result<()> {
    // PROCESS_TERMINATE = 1
    let h = unsafe { OpenProcess(1, 0, pid) };
    if h == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ok = unsafe { TerminateProcess(h, 99) };
    unsafe { CloseHandle(h) };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}
