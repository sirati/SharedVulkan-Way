//! The cold path on Windows: a named pipe.
//!
//! design.md §8's note that "a Windows app side would need a named-pipe and
//! `DuplicateHandle` implementation of exactly this file" is half right, and the
//! half that is wrong is the interesting half.
//!
//! **The named pipe is needed. `DuplicateHandle` is not.** The arena is a
//! **named** section object ([`crate::winshm`]), so the peer opens it by name
//! and nothing but text has to cross. That removes the requirement for the owner
//! to hold `PROCESS_DUP_HANDLE` on every process that attaches, which is a real
//! privilege relationship and one this design has no reason to establish.
//!
//! **This axis got the rule right first, and design.md §1a is that observation
//! generalised.** Linux used to pass a descriptor by `SCM_RIGHTS` here, on the
//! grounds that the arena was a memfd one process created and the other had no
//! other way to reach it. The answer was not a Windows equivalent of
//! `SCM_RIGHTS` but a Linux equivalent of *this*: a named object under
//! `/dev/shm` ([`crate::region::shm_create`]), and a name on the wire.
//!
//! So this module is the *transport* only, and it carries
//! [`crate::stream`]'s framing — the same framing the virtio-serial path uses,
//! for the same reason: a byte pipe with no descriptor passing. A named pipe in
//! byte mode and a virtio-serial port are the same thing to the layer above,
//! which is why there is one framing and not two.
//!
//! # Byte mode, not message mode
//!
//! Windows named pipes offer a message mode that would frame for us. It is not
//! used, and the reason is that it would be the only place in the project where
//! framing came from the transport: the Unix socket does not have it, the
//! virtio-serial port does not have it, and a `ColdMessage` that round-trips on
//! two of three paths is worse than one that round-trips on all three by the
//! same code. `PIPE_TYPE_BYTE` also means a reader that asks for less than a
//! message gets a short read instead of `ERROR_MORE_DATA`, which is the
//! behaviour [`crate::stream::read_message`] is already written against.

#[cfg(windows)]
use crate::{Error, Result};

#[cfg(windows)]
mod sys {
    use core::ffi::c_void;

    pub const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    pub const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    pub const PIPE_WAIT: u32 = 0x0000_0000;
    pub const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
    pub const GENERIC_READ: u32 = 0x8000_0000;
    pub const GENERIC_WRITE: u32 = 0x4000_0000;
    pub const OPEN_EXISTING: u32 = 3;
    pub const INVALID_HANDLE_VALUE: isize = -1;
    pub const ERROR_PIPE_CONNECTED: u32 = 535;

    unsafe extern "system" {
        pub fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer: u32,
            in_buffer: u32,
            default_timeout: u32,
            attrs: *mut c_void,
        ) -> isize;
        pub fn ConnectNamedPipe(pipe: isize, overlapped: *mut c_void) -> i32;
        pub fn DisconnectNamedPipe(pipe: isize) -> i32;
        pub fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            creation: u32,
            flags: u32,
            template: isize,
        ) -> isize;
        pub fn WaitNamedPipeW(name: *const u16, timeout: u32) -> i32;
        pub fn ReadFile(
            h: isize,
            buf: *mut u8,
            len: u32,
            read: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        pub fn WriteFile(
            h: isize,
            buf: *const u8,
            len: u32,
            written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        pub fn FlushFileBuffers(h: isize) -> i32;
        pub fn CloseHandle(h: isize) -> i32;
        pub fn GetLastError() -> u32;
    }
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

/// One end of a named pipe, as a `Read + Write` byte channel.
#[cfg(windows)]
pub struct Pipe {
    handle: isize,
    /// Servers disconnect before closing so the next `ConnectNamedPipe` on a
    /// reused instance behaves; clients simply close.
    server: bool,
}

#[cfg(windows)]
unsafe impl Send for Pipe {}

/// Open the guest end of a **virtio-serial port** on Windows.
///
/// waycompose-design.md §36.6. This is the Windows twin of
/// [`crate::stream::open_virtio_port`], and it lives in this module rather than
/// beside it because the mechanism is this module's: `vioser.sys` exposes a port
/// as a **named-pipe-shaped device object** at `\\.\Global\<name>`, and opening
/// it is the same `CreateFileW(OPEN_EXISTING)` that [`Pipe::connect`] does. The
/// returned [`Pipe`] is `Read + Write` and is a byte channel with no framing,
/// which is exactly what the doorbell wants.
///
/// # Not `Pipe::connect`, and the difference is one call
///
/// `Pipe::connect` retries around `WaitNamedPipeW`, which is meaningful for a
/// real named pipe whose instances can all be busy. A virtio-serial port is not
/// a named pipe and has no instances: `WaitNamedPipeW` on it fails with
/// `ERROR_INVALID_NAME` rather than waiting, so the retry would be a busy loop
/// dressed as patience. What *is* worth retrying is the port not existing yet —
/// the device object appears when the guest's `vioser.sys` binds the port, which
/// on a hot-added port is some milliseconds after the QMP `device_add` returns —
/// so this sleeps between attempts instead.
///
/// # One opener, and it is a platform rule rather than a convention
///
/// A virtio-serial port permits exactly one opener. That is why `docs/one-binary.md`
/// §1.6 says the doorbell cannot be shared between the GPU half and the
/// compositor half while they are separate processes, and it is the same
/// constraint [`crate::stream::open_virtio_port`] records for Linux ("two
/// descriptors on the same port is a way to deadlock"). Read and write both go
/// through the one handle this returns.
#[cfg(windows)]
pub fn open_virtio_port(name: &str, timeout_ms: u32) -> Result<Pipe> {
    // `\\.\Global\` rather than `\\.\`: the port has to be reachable from a
    // service and from a session-0 process, and the session-local namespace is
    // not. The guest half of this project has run as both.
    let path = format!("\\\\.\\Global\\{name}");
    let wpath = wide(&path);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(u64::from(timeout_ms));
    loop {
        // SAFETY: `wpath` outlives the call.
        let handle = unsafe {
            sys::CreateFileW(
                wpath.as_ptr(),
                sys::GENERIC_READ | sys::GENERIC_WRITE,
                0,
                core::ptr::null_mut(),
                sys::OPEN_EXISTING,
                0,
                0,
            )
        };
        if handle != sys::INVALID_HANDLE_VALUE {
            return Ok(Pipe { handle, server: false });
        }
        let errno = unsafe { sys::GetLastError() } as i32;
        if std::time::Instant::now() >= deadline {
            return Err(Error::Os { call: "CreateFileW (virtio-serial port)", errno });
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(windows)]
impl Pipe {
    /// Create the pipe and block until a client connects.
    ///
    /// One instance, because one session has one peer. `PIPE_REJECT_REMOTE_
    /// CLIENTS` is not decoration: without it a named pipe is reachable over SMB
    /// from another machine, which is a network-facing surface this design does
    /// not want and `security-model.md` does not account for.
    pub fn serve(name: &str) -> Result<Pipe> {
        let wname = wide(name);
        // SAFETY: `wname` outlives the call; a null attribute pointer asks for
        // the default descriptor, which is the creating user only.
        let handle = unsafe {
            sys::CreateNamedPipeW(
                wname.as_ptr(),
                sys::PIPE_ACCESS_DUPLEX,
                sys::PIPE_TYPE_BYTE | sys::PIPE_WAIT | sys::PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                64 * 1024,
                0,
                core::ptr::null_mut(),
            )
        };
        if handle == sys::INVALID_HANDLE_VALUE {
            return Err(Error::Os {
                call: "CreateNamedPipeW",
                errno: unsafe { sys::GetLastError() } as i32,
            });
        }
        // SAFETY: `handle` is a live pipe instance this function owns.
        let ok = unsafe { sys::ConnectNamedPipe(handle, core::ptr::null_mut()) };
        // A client that connected between Create and Connect makes Connect fail
        // with ERROR_PIPE_CONNECTED, which is success. Treating it as failure is
        // the classic named-pipe race and it fires under load, not in testing.
        if ok == 0 && unsafe { sys::GetLastError() } != sys::ERROR_PIPE_CONNECTED {
            let e = unsafe { sys::GetLastError() } as i32;
            unsafe { sys::CloseHandle(handle) };
            return Err(Error::Os { call: "ConnectNamedPipe", errno: e });
        }
        Ok(Pipe { handle, server: true })
    }

    /// Connect to a pipe the peer is serving, waiting up to `timeout_ms` for it
    /// to exist.
    ///
    /// The wait is mandatory rather than convenient: the two processes are
    /// started by a script and there is no ordering between them, and R4 says
    /// every wait is bounded.
    pub fn connect(name: &str, timeout_ms: u32) -> Result<Pipe> {
        let wname = wide(name);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
        loop {
            // SAFETY: `wname` outlives the call.
            let handle = unsafe {
                sys::CreateFileW(
                    wname.as_ptr(),
                    sys::GENERIC_READ | sys::GENERIC_WRITE,
                    0,
                    core::ptr::null_mut(),
                    sys::OPEN_EXISTING,
                    0,
                    0,
                )
            };
            if handle != sys::INVALID_HANDLE_VALUE {
                return Ok(Pipe { handle, server: false });
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::Os {
                    call: "CreateFileW (named pipe)",
                    errno: unsafe { sys::GetLastError() } as i32,
                });
            }
            // `WaitNamedPipe` fails immediately if the pipe does not exist at
            // all, so a sleep is still needed for the "server not started yet"
            // case; the wait covers "exists but all instances busy".
            unsafe { sys::WaitNamedPipeW(wname.as_ptr(), 50) };
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

#[cfg(windows)]
impl std::io::Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self).read(buf)
    }
}

/// `&Pipe` is `Read` and `Write` so that one open handle can serve a reader
/// thread and a writer thread at once. Neither operation mutates anything in
/// `Pipe` — both are a syscall on `handle` — and a virtio-serial port permits
/// exactly one opener, so sharing the handle is not a convenience but the only
/// way a doorbell can exist at all: a doorbell is by construction a thing you
/// write while something else is blocked reading. See `vdoorbell::attach`.
#[cfg(windows)]
impl std::io::Read for &Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut got: u32 = 0;
        let len = buf.len().min(u32::MAX as usize) as u32;
        // SAFETY: `buf` is valid for `len` bytes and `handle` is live.
        let ok = unsafe {
            sys::ReadFile(self.handle, buf.as_mut_ptr(), len, &mut got, core::ptr::null_mut())
        };
        if ok == 0 {
            let e = unsafe { sys::GetLastError() };
            // A closed pipe reads as an error, and the layer above wants EOF —
            // `stream::read_message` turns a zero-length read into "the cold
            // path closed", which is the message a human needs.
            const ERROR_BROKEN_PIPE: u32 = 109;
            const ERROR_PIPE_NOT_CONNECTED: u32 = 233;
            if e == ERROR_BROKEN_PIPE || e == ERROR_PIPE_NOT_CONNECTED {
                return Ok(0);
            }
            return Err(std::io::Error::from_raw_os_error(e as i32));
        }
        Ok(got as usize)
    }
}

#[cfg(windows)]
impl std::io::Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&*self).flush()
    }
}

#[cfg(windows)]
impl std::io::Write for &Pipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut put: u32 = 0;
        let len = buf.len().min(u32::MAX as usize) as u32;
        // SAFETY: `buf` is valid for `len` bytes and `handle` is live.
        let ok = unsafe {
            sys::WriteFile(self.handle, buf.as_ptr(), len, &mut put, core::ptr::null_mut())
        };
        if ok == 0 {
            return Err(std::io::Error::from_raw_os_error(unsafe { sys::GetLastError() } as i32));
        }
        Ok(put as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // SAFETY: `handle` is live.
        unsafe { sys::FlushFileBuffers(self.handle) };
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: `handle` was established by `serve` or `connect` and is
        // released exactly once.
        unsafe {
            if self.server {
                sys::DisconnectNamedPipe(self.handle);
            }
            sys::CloseHandle(self.handle);
        }
    }
}

/// The pipe name for one session.
///
/// One function so the two sides cannot spell it differently, exactly as
/// [`crate::winshm::section_name`]. Computable on Linux too, because the app
/// side of a Windows pair may be built anywhere.
pub fn pipe_name(session: &str) -> String {
    format!(r"\\.\pipe\shared_vulkan.{session}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_pipe_name_is_local_and_stable() {
        assert_eq!(super::pipe_name("abc"), r"\\.\pipe\shared_vulkan.abc");
    }

    /// The two names are derived from one session string, so a script that
    /// starts both sides has exactly one thing to pass them.
    #[test]
    fn one_session_string_names_both_the_pipe_and_the_section() {
        let s = "e2e-1234";
        assert!(super::pipe_name(s).ends_with(s));
        assert!(crate::winshm::section_name(s).ends_with(s));
    }
}
