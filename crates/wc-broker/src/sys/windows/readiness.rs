//! Installed-service readiness: kernel server identity and bounded pipe I/O.
use std::io;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, FILE_WRITE_DATA, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, ReadFile, WriteFile};
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Services::{CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS_PROCESS};
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use super::{ident, pipe};

fn service_pid() -> io::Result<u32> {
    // SAFETY: local SCM with read-only service query rights.
    let scm = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
    if scm.is_null() { return Err(io::Error::last_os_error()); }
    let name = pipe::wide(super::service::SERVICE_NAME);
    // SAFETY: a live manager handle and NUL-terminated name.
    let service = unsafe { OpenServiceW(scm, name.as_ptr(), SERVICE_QUERY_STATUS) };
    if service.is_null() {
        let error = io::Error::last_os_error();
        unsafe { CloseServiceHandle(scm); }
        return Err(error);
    }
    let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut needed = 0;
    // SAFETY: a writable, correctly sized status buffer; both handles close below.
    let ok = unsafe { QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, (&mut status as *mut SERVICE_STATUS_PROCESS).cast(), std::mem::size_of_val(&status) as u32, &mut needed) };
    let error = io::Error::last_os_error();
    unsafe { CloseServiceHandle(service); CloseServiceHandle(scm); }
    if ok == 0 { return Err(error); }
    if status.dwCurrentState != SERVICE_RUNNING || status.dwProcessId == 0 {
        return Err(io::Error::other("installed broker service is not running"));
    }
    Ok(status.dwProcessId)
}

fn trusted_pipe(path: &str, expected_pid: u32, expected_sid: &str) -> io::Result<(ident::Owned, ident::Owned)> {
    let name = pipe::wide(path);
    // SAFETY: opens a fresh overlapped client handle with identification-only SQOS.
    let handle = unsafe { CreateFileW(name.as_ptr(), GENERIC_READ | FILE_WRITE_DATA, 0, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION, std::ptr::null_mut()) };
    if handle == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
    let handle = ident::Owned(handle);
    let mut pid = 0;
    // SAFETY: the connected pipe and writable PID result.
    if unsafe { GetNamedPipeServerProcessId(handle.0, &mut pid) } == 0 { return Err(io::Error::last_os_error()); }
    if pid != expected_pid { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "pipe server is not the installed SCM service process")); }
    // Keep the verified process handle alive throughout the exchange. This
    // checks its kernel token SID and prevents trusting a recycled PID.
    let process = ident::open_process_checked(pid, expected_sid)?;
    if unsafe { WaitForSingleObject(process.0, 0) } != WAIT_TIMEOUT { return Err(io::Error::other("pipe server process has exited")); }
    Ok((handle, process))
}

fn transfer(handle: HANDLE, writing: bool, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
    let event = pipe::Event::new()?;
    let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
    operation.hEvent = event.0;
    let mut count = 0;
    // SAFETY: buffers and OVERLAPPED stay alive until completion, including
    // cancellation below. Only this operation accesses its event and buffer.
    let ok = unsafe {
        if writing { WriteFile(handle, bytes.as_ptr(), bytes.len() as u32, &mut count, &mut operation) }
        else { ReadFile(handle, bytes.as_mut_ptr(), bytes.len() as u32, &mut count, &mut operation) }
    };
    if ok == 0 && io::Error::last_os_error().raw_os_error() != Some(ERROR_IO_PENDING as i32) { return Err(io::Error::last_os_error()); }
    let remaining = deadline.saturating_duration_since(Instant::now()).as_millis().min(1000) as u32;
    if unsafe { WaitForSingleObject(event.0, remaining) } != WAIT_OBJECT_0 {
        // Cancel and drain the operation before releasing its stack buffers.
        // This is control-pipe I/O only; no memory region is retired or freed.
        unsafe { CancelIoEx(handle, &operation); GetOverlappedResult(handle, &operation, &mut count, 1); }
        return Err(io::Error::new(io::ErrorKind::TimedOut, "broker readiness I/O deadline"));
    }
    if unsafe { GetOverlappedResult(handle, &operation, &mut count, 0) } == 0 { return Err(io::Error::last_os_error()); }
    Ok(count as usize)
}

fn exchange(handle: HANDLE) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    for (request, prefix) in [(b"hello installer-ready\n".as_slice(), "ok hello "), (b"stats\n".as_slice(), "ok stats ")] {
        let mut request = request.to_vec();
        let mut sent = 0;
        while sent < request.len() {
            let n = transfer(handle, true, &mut request[sent..], deadline)?;
            if n == 0 { return Err(io::Error::other("readiness write closed")); }
            sent += n;
        }
        let mut reply = Vec::new();
        loop {
            let mut bytes = [0u8; 512];
            let n = transfer(handle, false, &mut bytes, deadline)?;
            if n == 0 { return Err(io::Error::other("readiness reply closed")); }
            reply.extend_from_slice(&bytes[..n]);
            if reply.len() > 4096 { return Err(io::Error::other("readiness reply too long")); }
            if reply.contains(&b'\n') { break; }
        }
        if !String::from_utf8_lossy(&reply).starts_with(prefix) { return Err(io::Error::other("invalid readiness reply")); }
    }
    Ok(())
}

pub(super) fn installed(path: &str) -> io::Result<()> {
    let pid = service_pid()?;
    let (handle, _process) = trusted_pipe(path, pid, ident::SYSTEM_SID)?;
    exchange(handle.0)?;
    if service_pid()? != pid { return Err(io::Error::other("broker service changed during readiness")); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn connect_private(name: &str, pid: u32, sid: &str) -> io::Result<(ident::Owned, ident::Owned)> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            match trusted_pipe(name, pid, sid) {
                Err(e) if e.raw_os_error() == Some(2) && Instant::now() < until => std::thread::yield_now(),
                result => return result,
            }
        }
    }

    #[test]
    fn readiness_refuses_a_pipe_owned_by_another_process_before_accepting_replies() {
        let name = format!(r"\\.\pipe\wc-ready-spoof-{}", std::process::id());
        let endpoint = name.clone();
        let (end, wait) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let listener = pipe::Listener::new(&endpoint, super::super::PIPE_SDDL).unwrap();
            let _pipe = listener.accept().unwrap();
            let _ = wait.recv();
        });
        let error = connect_private(&name, std::process::id().wrapping_add(1), ident::SYSTEM_SID).err().expect("spoofed process accepted");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("SCM service process"));
        end.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn readiness_stalled_verified_private_server_has_a_bounded_io_deadline() {
        let name = format!(r"\\.\pipe\wc-ready-stall-{}", std::process::id());
        let endpoint = name.clone();
        let (end, wait) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let listener = pipe::Listener::new(&endpoint, super::super::PIPE_SDDL).unwrap();
            let _pipe = listener.accept().unwrap();
            let _ = wait.recv();
        });
        // Only this private fixture supplies the test account's SID. The
        // installed-service entry always requires SCM PID and SYSTEM.
        let sid = ident::current_user().unwrap();
        let (pipe, _process) = connect_private(&name, std::process::id(), &sid).unwrap();
        let start = Instant::now();
        let error = exchange(pipe.0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(3));
        end.send(()).unwrap();
        server.join().unwrap();
    }
}
