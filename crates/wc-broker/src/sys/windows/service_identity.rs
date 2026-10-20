//! Native identity for control authentication inside the installed SYSTEM service.
//! This does not use the pipe's PID/SID lookup as an original-process proof.
use super::ident::{self, Owned};
use crate::core::User;
use crate::service_auth::{ServiceEndpoint, ServicePolicy, ServiceProcess};
use std::io;
use std::sync::Arc;
use windows_sys::Win32::Foundation::{DuplicateHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

struct CurrentServiceProcess(Owned);
// SAFETY: the only constructor below duplicates the actual current process
// object, without a PID lookup, and retains its owned synchronize HANDLE.
unsafe impl ServiceProcess for CurrentServiceProcess {
    fn exited(&self) -> io::Result<bool> {
        // SAFETY: the retained process HANDLE, zero-time kernel state query.
        match unsafe { WaitForSingleObject(self.0.0, 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

/// Capture the actual current SYSTEM service's process object and token SID.
/// No public boot-signing or user-client request invokes this endpoint.
///
/// # Safety
/// The caller must establish the installed service's protected executable,
/// configuration and SCM provenance before constructing control authentication.
/// This function checks kernel identity; SYSTEM identity alone is not an
/// attestation of the caller's service configuration or resource completion.
pub unsafe fn current_endpoint() -> io::Result<ServiceEndpoint> {
    let sid = ident::current_user()?;
    if sid != ident::SYSTEM_SID {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control signing requires the SYSTEM service",
        ));
    }
    let mut handle = std::ptr::null_mut();
    // SAFETY: duplicate our actual current process pseudo-handle into our own
    // handle table with only synchronization rights. No client PID is used.
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &mut handle,
            PROCESS_SYNCHRONIZE,
            0,
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let retained = Arc::new(CurrentServiceProcess(Owned(handle)));
    // SAFETY: exact self-process object and kernel primary token obtained
    // above; protected service provenance is the caller's explicit contract.
    unsafe {
        ServiceEndpoint::from_current_service(
            &ServicePolicy::windows_system(),
            User::sid(&sid),
            retained,
        )
    }
}
