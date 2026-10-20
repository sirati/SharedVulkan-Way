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

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetNamedPipeClientProcessId(pipe: isize, pid: *mut u32) -> i32;
    fn DuplicateHandle(src_proc: isize, src: isize, dst_proc: isize, dst: *mut isize, access: u32, inherit: i32, options: u32) -> i32;
    fn GetCurrentProcess() -> isize;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(p: isize, access: u32, tok: *mut isize) -> i32;
    fn GetTokenInformation(tok: isize, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, out: *mut *mut u16) -> i32;
}

/// The process at the other end of a pipe this process serves.
pub fn pipe_client_pid(pipe: &std::fs::File) -> std::io::Result<u32> {
    use std::os::windows::io::AsRawHandle;
    let mut pid = 0u32;
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle() as isize, &mut pid) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pid)
}

/// The user SID of process `pid`, as a string (`S-1-5-21-…`).
pub fn process_sid(pid: u32) -> std::io::Result<String> {
    // PROCESS_QUERY_LIMITED_INFORMATION = 0x1000, TOKEN_QUERY = 8, TokenUser = 1
    let p = unsafe { OpenProcess(0x1000, 0, pid) };
    if p == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut tok = 0isize;
    let ok = unsafe { OpenProcessToken(p, 8, &mut tok) };
    unsafe { CloseHandle(p) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buf = vec![0u64; 64];
    let mut ret = 0u32;
    let ok = unsafe { GetTokenInformation(tok, 1, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut ret) };
    unsafe { CloseHandle(tok) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // TOKEN_USER { SID_AND_ATTRIBUTES { PSID Sid; DWORD Attributes } }
    let sid = buf[0] as *mut c_void;
    let mut s: *mut u16 = core::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut n = 0;
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) });
    unsafe { LocalFree(s.cast()) };
    Ok(out)
}

/// Duplicate `handle` (of this process) into process `pid`, with the same
/// access; returns the handle's value in that process. Nothing else can open
/// an unnamed section, so this is the only way into it.
pub fn duplicate_into(pid: u32, handle: isize) -> std::io::Result<isize> {
    // PROCESS_DUP_HANDLE = 0x40, DUPLICATE_SAME_ACCESS = 2
    let p = unsafe { OpenProcess(0x40, 0, pid) };
    if p == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut out = 0isize;
    let ok = unsafe { DuplicateHandle(GetCurrentProcess(), handle, p, &mut out, 0, 0, 2) };
    unsafe { CloseHandle(p) };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(out) }
}
