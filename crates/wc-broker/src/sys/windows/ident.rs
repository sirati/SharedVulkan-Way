//! Who is asking (§111.8): the pipe client's token, the process behind it,
//! and the account a SID names.

use std::io;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LUID, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupAccountSidW,
    LookupPrivilegeValueW, PSID, RevertToSelf, SE_PRIVILEGE_ENABLED, SID_NAME_USE, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, ImpersonateNamedPipeClient};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcess, OpenProcessToken, OpenThreadToken, PROCESS_DUP_HANDLE,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

use super::pipe::wide;

/// The one account that is an admin of the broker (§111.8): SYSTEM. Not
/// Administrators: an administrator's process is a user's like any other
/// here, and gets its own user's regions only.
pub const SYSTEM_SID: &str = "S-1-5-18";

/// A handle closed on drop.
pub struct Owned(pub HANDLE);

// SAFETY: a kernel handle.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owned, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn os(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// The string form of a SID.
///
/// # Safety
/// `sid` must point at a valid SID.
unsafe fn sid_text(sid: PSID) -> io::Result<String> {
    let mut s: *mut u16 = std::ptr::null_mut();
    // SAFETY: the caller's SID and a live out-parameter.
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
        return Err(os("ConvertSidToStringSidW"));
    }
    let mut n = 0;
    // SAFETY: a NUL-terminated string the call allocated.
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `n` u16s just counted.
    let t = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) });
    // SAFETY: allocated by the call, freed once.
    unsafe { LocalFree(s.cast()) };
    Ok(t)
}

/// The user SID of a token, as text.
pub fn token_user(token: HANDLE) -> io::Result<String> {
    let mut need = 0u32;
    // SAFETY: the size query.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut need) };
    if need == 0 {
        return Err(os("GetTokenInformation(TokenUser) size"));
    }
    let mut buf = vec![0u64; (need as usize).div_ceil(8)];
    // SAFETY: a live buffer of at least `need` bytes, 8-aligned.
    if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), need, &mut need) } == 0 {
        return Err(os("GetTokenInformation(TokenUser)"));
    }
    // SAFETY: the call wrote a TOKEN_USER at the start of the buffer.
    let tu = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    // SAFETY: its SID points into the same live buffer.
    unsafe { sid_text(tu.User.Sid) }
}

/// The process and the user SID on the other end of a connected pipe
/// instance: the pid from the pipe, the SID from the client's own token by
/// impersonating it for the length of one query.
pub fn pipe_client(pipe: HANDLE) -> io::Result<(u32, String)> {
    let mut pid = 0u32;
    // SAFETY: a live, connected server instance.
    if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) } == 0 {
        return Err(os("GetNamedPipeClientProcessId"));
    }
    // SAFETY: as above.
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(os("ImpersonateNamedPipeClient"));
    }
    let mut tok: HANDLE = std::ptr::null_mut();
    // SAFETY: this thread's impersonation token, opened as the broker itself
    // (`OpenAsSelf`), so an identification-level token is enough.
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut tok) };
    let err = if opened == 0 { Some(os("OpenThreadToken")) } else { None };
    // SAFETY: always revert before anything else runs on this thread.
    if unsafe { RevertToSelf() } == 0 {
        // A thread that cannot stop impersonating must not go on serving.
        eprintln!("[wc-broker] RevertToSelf failed ({}); exiting", io::Error::last_os_error());
        std::process::exit(3);
    }
    if let Some(e) = err {
        return Err(e);
    }
    let tok = Owned(tok);
    Ok((pid, token_user(tok.0)?))
}

/// Open process `pid` for what the broker does with it (watch it, put
/// handles into it), and check that its own token's user is `sid`: the
/// pipe told the pid, and a pid can be reused between that and this open, so
/// the process must be shown to be the same user's.
pub fn open_process_checked(pid: u32, sid: &str) -> io::Result<Owned> {
    // SAFETY: plain call; checked below.
    let h = unsafe { OpenProcess(PROCESS_DUP_HANDLE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return Err(os(&format!("OpenProcess({pid})")));
    }
    let p = Owned(h);
    let mut tok: HANDLE = std::ptr::null_mut();
    // SAFETY: a live process handle with query access.
    if unsafe { OpenProcessToken(p.0, TOKEN_QUERY, &mut tok) } == 0 {
        return Err(os(&format!("OpenProcessToken({pid})")));
    }
    let tok = Owned(tok);
    let owner = token_user(tok.0)?;
    if owner != sid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("process {pid} runs as {owner}, not as the pipe's client {sid}: refused"),
        ));
    }
    Ok(p)
}

/// The user SID of this process.
pub fn current_user() -> io::Result<String> {
    let mut tok: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) } == 0 {
        return Err(os("OpenProcessToken(self)"));
    }
    let tok = Owned(tok);
    token_user(tok.0)
}

/// The account name of `sid` (`LookupAccountSidW`), or `None`.
pub fn account_name(sid: &str) -> Option<String> {
    let w = wide(sid);
    let mut p: PSID = std::ptr::null_mut();
    // SAFETY: a live string and out-parameter.
    if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut p) } == 0 {
        return None;
    }
    let mut name = [0u16; 256];
    let mut dom = [0u16; 256];
    let (mut nn, mut dn) = (name.len() as u32, dom.len() as u32);
    let mut use_: SID_NAME_USE = 0;
    // SAFETY: the SID just made and live buffers of the sizes given.
    let ok = unsafe {
        LookupAccountSidW(std::ptr::null(), p, name.as_mut_ptr(), &mut nn, dom.as_mut_ptr(), &mut dn, &mut use_)
    };
    // SAFETY: allocated by the conversion, freed once.
    unsafe { LocalFree(p) };
    (ok != 0).then(|| String::from_utf16_lossy(&name[..nn as usize]))
}


/// Enable privilege `name` (`SeLockMemoryPrivilege`) in this process's
/// token. Ok(false): the token does not hold it.
pub fn enable_privilege(name: &str) -> io::Result<bool> {
    let mut tok: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut tok) } == 0 {
        return Err(os("OpenProcessToken"));
    }
    let tok = Owned(tok);
    let mut luid = LUID { LowPart: 0, HighPart: 0 };
    let w = wide(name);
    // SAFETY: live arguments.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), w.as_ptr(), &mut luid) } == 0 {
        return Err(os("LookupPrivilegeValueW"));
    }
    let tp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
    };
    // SAFETY: a live token and privilege set.
    if unsafe { AdjustTokenPrivileges(tok.0, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
        return Err(os("AdjustTokenPrivileges"));
    }
    const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;
    // SAFETY: no arguments.
    Ok(unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED)
}

