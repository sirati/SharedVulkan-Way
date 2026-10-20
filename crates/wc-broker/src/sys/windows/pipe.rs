//! The broker's named pipe (§111.8): one instance per connection, opened
//! overlapped so that a push from another thread is never queued behind the
//! connection's own blocked read (a synchronous handle serialises all I/O on
//! it, §87's lesson).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::CreateEventW;

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A security descriptor made from SDDL, freed on drop.
pub(crate) struct Sd(pub PSECURITY_DESCRIPTOR);

// SAFETY: a heap block only read after it is made, freed once.
unsafe impl Send for Sd {}
unsafe impl Sync for Sd {}

impl Sd {
    pub fn from_sddl(sddl: &str) -> io::Result<Sd> {
        let w = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a live NUL-terminated string and a live out-parameter.
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), 1, &mut sd, std::ptr::null_mut()) } == 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(e.kind(), format!("SDDL {sddl:?}: {e}")));
        }
        Ok(Sd(sd))
    }
}

impl Drop for Sd {
    fn drop(&mut self) {
        // SAFETY: allocated by the conversion above, freed once.
        unsafe { LocalFree(self.0) };
    }
}

/// One server instance of the pipe, connected to one client.
pub struct ServerPipe {
    pub(crate) handle: HANDLE,
}

// SAFETY: a kernel handle; every I/O on it is overlapped with its own
// OVERLAPPED and event, so concurrent use from two threads is what it is for.
unsafe impl Send for ServerPipe {}
unsafe impl Sync for ServerPipe {}

impl Drop for ServerPipe {
    fn drop(&mut self) {
        // SAFETY: owned; disconnected and closed once.
        unsafe {
            DisconnectNamedPipe(self.handle);
            CloseHandle(self.handle);
        }
    }
}

/// The listening side: makes an instance, waits for a client.
pub struct Listener {
    name: Vec<u16>,
    sd: Sd,
    first: AtomicBool,
}

impl Listener {
    pub fn new(name: &str, sddl: &str) -> io::Result<Listener> {
        Ok(Listener { name: wide(name), sd: Sd::from_sddl(sddl)?, first: AtomicBool::new(true) })
    }

    /// Make an instance and wait for a client to connect to it.
    ///
    /// The first instance is made with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a
    /// process that made the name first (to pose as the broker) makes the
    /// broker fail to start rather than share the name with it. Clients may
    /// not make instances: the SDDL grants them read and `FILE_WRITE_DATA`,
    /// never `FILE_CREATE_PIPE_INSTANCE`.
    pub fn accept(&self) -> io::Result<ServerPipe> {
        let first = self.first.load(Ordering::Relaxed);
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd.0,
            bInheritHandle: 0,
        };
        // SAFETY: a live name and attributes for the call.
        let h = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 },
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                &sa,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(
                e.kind(),
                format!("CreateNamedPipeW{}: {e}", if first { " (first instance: is another broker running?)" } else { "" }),
            ));
        }
        self.first.store(false, Ordering::Relaxed);
        let p = ServerPipe { handle: h };
        let ev = Event::new()?;
        // SAFETY: zeroed is a valid OVERLAPPED.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = ev.0;
        // SAFETY: a live instance and OVERLAPPED, waited for below.
        let ok = unsafe { ConnectNamedPipe(p.handle, &mut ov) };
        if ok == 0 {
            // SAFETY: no arguments.
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED => {}
                ERROR_IO_PENDING => {
                    let mut n = 0u32;
                    // SAFETY: the OVERLAPPED of the pending connect.
                    if unsafe { GetOverlappedResult(p.handle, &ov, &mut n, 1) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                e => return Err(io::Error::from_raw_os_error(e as i32)),
            }
        }
        Ok(p)
    }
}

/// A manual-reset event, closed on drop.
pub(crate) struct Event(pub HANDLE);

impl Event {
    pub fn new() -> io::Result<Event> {
        // SAFETY: no name, no attributes.
        let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Event(h))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: owned, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// One overlapped read or write, waited for. A pipe the client closed reads
/// as end of file.
pub(crate) fn io(h: HANDLE, write: bool, buf: *mut u8, len: u32) -> io::Result<usize> {
    let ev = Event::new()?;
    // SAFETY: zeroed is a valid OVERLAPPED.
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = ev.0;
    let mut n = 0u32;
    // SAFETY: `buf` is live for `len` bytes until the operation is complete,
    // which is waited for below before returning.
    let ok = unsafe {
        if write { WriteFile(h, buf, len, &mut n, &mut ov) } else { ReadFile(h, buf, len, &mut n, &mut ov) }
    };
    if ok == 0 {
        // SAFETY: no arguments.
        let e = unsafe { GetLastError() };
        if e != ERROR_IO_PENDING {
            return eof_or(e);
        }
    }
    // SAFETY: the OVERLAPPED of this operation.
    if unsafe { GetOverlappedResult(h, &ov, &mut n, 1) } == 0 {
        // SAFETY: no arguments.
        return eof_or(unsafe { GetLastError() });
    }
    Ok(n as usize)
}

fn eof_or(e: u32) -> io::Result<usize> {
    if e == ERROR_BROKEN_PIPE || e == ERROR_PIPE_NOT_CONNECTED { Ok(0) } else { Err(io::Error::from_raw_os_error(e as i32)) }
}

impl io::Read for &ServerPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io(self.handle, false, buf.as_mut_ptr(), buf.len().min(1 << 20) as u32)
    }
}

impl io::Write for &ServerPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = io(self.handle, true, buf.as_ptr().cast_mut(), buf.len().min(1 << 20) as u32)?;
        if n == 0 && !buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the client closed the pipe"));
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
