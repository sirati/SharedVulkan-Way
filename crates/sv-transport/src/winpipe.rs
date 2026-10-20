//! The cold path on Windows: a named pipe.
//!
//! docs/design.md §8's note that "a Windows app side would need a named-pipe and
//! `DuplicateHandle` implementation of exactly this file" is half right, and the
//! half that is wrong is the interesting half.
//!
//! **The named pipe is needed. `DuplicateHandle` is not.** The arena is a
//! **named** section object ([`crate::winshm`]), so the peer opens it by name
//! and nothing but text has to cross. That removes the requirement for the owner
//! to hold `PROCESS_DUP_HANDLE` on every process that attaches, which is a real
//! privilege relationship and one this design has no reason to establish.
//!
//! **This axis got the rule right first, and docs/design.md §1a is that observation
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
        pub fn CreateEventW(attrs: *mut c_void, manual: i32, initial: i32, name: *const u16) -> isize;
        pub fn GetOverlappedResult(h: isize, ov: *mut Overlapped, done: *mut u32, wait: i32) -> i32;
        pub fn GetOverlappedResultEx(h: isize, ov: *mut Overlapped, done: *mut u32, ms: u32, alertable: i32) -> i32;
        pub fn CancelIoEx(h: isize, ov: *mut Overlapped) -> i32;
    }

    pub const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    pub const ERROR_IO_PENDING: u32 = 997;
    pub const WAIT_TIMEOUT: u32 = 258;

    /// `OVERLAPPED`, x64: two `ULONG_PTR`s, the offset pair, the event.
    #[repr(C)]
    pub struct Overlapped {
        pub internal: usize,
        pub internal_high: usize,
        pub offset: u32,
        pub offset_high: u32,
        pub event: isize,
    }
}

/// One overlapped `ReadFile` or `WriteFile`, waited for.
///
/// waycompose-design.md §87. **A synchronous handle serialises every I/O on
/// it**: the I/O manager holds the file object's lock for the whole call, so
/// while the doorbell's reader sits in `ReadFile` waiting for a byte, a
/// `WriteFile` from any other thread on the same handle waits behind it — and
/// a virtio-serial port takes one opener, so there is no second handle to
/// write through. The first run with a doorbell on the Windows GPU side hung
/// exactly so: the first reply's bell blocked the ring thread behind the
/// reader, and no command after it was served. Opened overlapped, each call
/// carries its own `OVERLAPPED` and the two directions are independent.
/// The longest an overlapped write waits before it is cancelled (§99.5).
#[cfg(windows)]
const WRITE_TIMEOUT_MS: u32 = 500;

#[cfg(windows)]
fn overlapped_io(handle: isize, write: bool, buf: *mut u8, len: u32) -> core::result::Result<u32, u32> {
    // SAFETY: a fresh manual-reset event, closed after completion or retained with pending I/O.
    let event = unsafe { sys::CreateEventW(core::ptr::null_mut(), 1, 0, core::ptr::null()) };
    if event == 0 {
        return Err(unsafe { sys::GetLastError() });
    }
    let mut ov = Box::new(sys::Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event });
    let mut owned = if write { vec![0u8; len as usize] } else { Vec::new() };
    if write {
        // SAFETY: the caller supplies len readable bytes.
        unsafe { core::ptr::copy_nonoverlapping(buf, owned.as_mut_ptr(), len as usize) };
    }
    let io_buf = if write { owned.as_mut_ptr() } else { buf };
    let mut done: u32 = 0;
    // SAFETY: reads wait for completion before returning to the caller.
    // Writes own their buffer and record, retaining both if cancellation stalls.
    let ok = unsafe {
        let ovp = (&raw mut *ov).cast::<core::ffi::c_void>();
        if write {
            sys::WriteFile(handle, io_buf, len, &mut done, ovp)
        } else {
            sys::ReadFile(handle, io_buf, len, &mut done, ovp)
        }
    };
    let result = if ok != 0 {
        // Completed at once; `done` may not be set on an overlapped handle,
        // so the count is taken from the overlapped record either way.
        let mut n = 0u32;
        if unsafe { sys::GetOverlappedResult(handle, &mut ov, &mut n, 0) } != 0 {
            Ok(n)
        } else {
            Ok(done)
        }
    } else {
        let e = unsafe { sys::GetLastError() };
        if e != sys::ERROR_IO_PENDING {
            Err(e)
        } else {
            let mut n = 0u32;
            // A write waits at most WRITE_TIMEOUT_MS (§99.5): a port whose host
            // end stopped draining must not hold even the writer thread for
            // ever. It is cancelled and then waited for, because the
            // OVERLAPPED and the buffer must outlive the request.
            let ms = if write { WRITE_TIMEOUT_MS } else { u32::MAX };
            if unsafe { sys::GetOverlappedResultEx(handle, &mut ov, &mut n, ms, 0) } != 0 {
                Ok(n)
            } else {
                let e = unsafe { sys::GetLastError() };
                if write && e == sys::WAIT_TIMEOUT {
                    unsafe { sys::CancelIoEx(handle, &mut ov) };
                    if unsafe { sys::GetOverlappedResultEx(handle, &mut *ov, &mut n, WRITE_TIMEOUT_MS, 0) } == 0
                        && unsafe { core::ptr::read_volatile(&ov.internal) } == AsyncWriter::PENDING
                    {
                        // vioser can leave cancellation pending indefinitely. The kernel
                        // still owns these addresses. Quarantine them rather than block
                        // this writer forever or free memory under the driver.
                        std::mem::forget(ov);
                        std::mem::forget(owned);
                        return Err(sys::WAIT_TIMEOUT);
                    }
                    Err(sys::WAIT_TIMEOUT)
                } else {
                    Err(e)
                }
            }
        }
    };
    unsafe { sys::CloseHandle(event) };
    result
}

/// A doorbell writer that issues each write from the caller's own thread and
/// never waits for it (waycompose-design.md §105.14).
///
/// The detached writer it replaces handed every bell byte to a thread of its
/// own, which then waited for the write's completion. On an idle guest that
/// hand-off is a wake of a second thread, usually on a halted vCPU, and it
/// sat on the critical path of every request that found the host parked.
/// Here the caller issues the overlapped `WriteFile` itself (the virtqueue
/// kick happens inside that call) and returns; the record and its byte live
/// in an address-stable slot until the write completes. Requests that exceed
/// the write timeout are cancelled and quarantined, so a broken cancellation
/// cannot occupy the active pool forever. At most 256 quarantined requests are
/// retained; complete port failure then refuses hints without blocking callers.
#[cfg(windows)]
#[allow(clippy::vec_box)] // Each pending record must retain its address when vectors grow.
pub struct AsyncWriter {
    pipe: std::sync::Arc<Pipe>,
    slots: Vec<Box<AsyncSlot>>,
    retired: Vec<Box<AsyncSlot>>,
}

#[cfg(windows)]
struct AsyncSlot {
    ov: sys::Overlapped,
    buf: [u8; 16],
    busy: bool,
    started: std::time::Instant,
}

#[cfg(windows)]
impl Drop for AsyncSlot {
    fn drop(&mut self) {
        // Only completed slots are dropped. Outstanding requests are retained.
        unsafe { sys::CloseHandle(self.ov.event) };
    }
}

#[cfg(windows)]
unsafe impl Send for AsyncWriter {}

#[cfg(windows)]
impl AsyncWriter {
    const SLOTS: usize = 32;
    const MAX_RETIRED: usize = 256;
    /// `STATUS_PENDING`: what `Internal` holds while a request is in flight
    /// (the `HasOverlappedIoCompleted` macro).
    const PENDING: usize = 0x103;

    pub fn new(pipe: std::sync::Arc<Pipe>) -> std::io::Result<AsyncWriter> {
        let mut slots = Vec::with_capacity(Self::SLOTS);
        for _ in 0..Self::SLOTS {
            // SAFETY: a fresh manual-reset event, closed in `drop`.
            let event = unsafe { sys::CreateEventW(core::ptr::null_mut(), 1, 0, core::ptr::null()) };
            if event == 0 {
                return Err(std::io::Error::last_os_error());
            }
            slots.push(Box::new(AsyncSlot {
                ov: sys::Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event },
                buf: [0; 16],
                busy: false,
                started: std::time::Instant::now(),
            }));
        }
        Ok(AsyncWriter { pipe, slots, retired: Vec::new() })
    }

    fn reap(&mut self) {
        for s in self.slots.iter_mut().chain(self.retired.iter_mut()) {
            if s.busy && unsafe { core::ptr::read_volatile(&s.ov.internal) } != Self::PENDING {
                s.busy = false;
            }
        }
        for i in 0..self.slots.len() {
            if !self.slots[i].busy || self.slots[i].started.elapsed().as_millis() < WRITE_TIMEOUT_MS as u128 {
                continue;
            }
            unsafe { sys::CancelIoEx(self.pipe.handle, &mut self.slots[i].ov) };
            let replacement = if let Some(j) = self.retired.iter().position(|s| !s.busy) {
                self.retired.swap_remove(j)
            } else if self.retired.len() < Self::MAX_RETIRED {
                let event = unsafe { sys::CreateEventW(core::ptr::null_mut(), 1, 0, core::ptr::null()) };
                if event == 0 { continue; }
                Box::new(AsyncSlot {
                    ov: sys::Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event },
                    buf: [0; 16], busy: false, started: std::time::Instant::now(),
                })
            } else {
                continue;
            };
            self.retired.push(std::mem::replace(&mut self.slots[i], replacement));
        }
    }
}

#[cfg(windows)]
impl std::io::Write for AsyncWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.reap();
        let handle = self.pipe.handle;
        let Some(s) = self.slots.iter_mut().find(|s| !s.busy) else {
            // Every write still pends: the host end stopped draining. Cancel
            // them so the pool frees itself, and refuse this byte.
            for s in self.slots.iter_mut() {
                // SAFETY: the record of a request on this handle.
                unsafe { sys::CancelIoEx(handle, &mut s.ov) };
            }
            return Err(std::io::ErrorKind::WouldBlock.into());
        };
        let n = buf.len().min(s.buf.len());
        s.buf[..n].copy_from_slice(&buf[..n]);
        s.ov.internal = Self::PENDING;
        s.ov.internal_high = 0;
        let mut done = 0u32;
        // SAFETY: each slot has its own stable allocation. Pending slots
        // retain their buffer and record, including during shutdown.
        let ok = unsafe {
            sys::WriteFile(handle, s.buf.as_ptr(), n as u32, &mut done, (&raw mut s.ov).cast::<core::ffi::c_void>())
        };
        if ok == 0 {
            let e = unsafe { sys::GetLastError() };
            if e != sys::ERROR_IO_PENDING {
                s.ov.internal = 0;
                const ERROR_NO_SYSTEM_RESOURCES: u32 = 1450;
                return Err(if e == ERROR_NO_SYSTEM_RESOURCES {
                    std::io::ErrorKind::WouldBlock.into()
                } else {
                    std::io::Error::from_raw_os_error(e as i32)
                });
            }
        }
        s.busy = true;
        s.started = std::time::Instant::now();
        crate::frametrace::hop("bell.issued");
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for AsyncWriter {
    fn drop(&mut self) {
        for mut s in self.slots.drain(..).chain(self.retired.drain(..)) {
            if s.busy {
                unsafe { sys::CancelIoEx(self.pipe.handle, &mut s.ov) };
                let mut n = 0;
                if unsafe { sys::GetOverlappedResult(self.pipe.handle, &mut s.ov, &mut n, 0) } == 0
                    && unsafe { core::ptr::read_volatile(&s.ov.internal) } == Self::PENDING
                {
                    // Preserve the port too: closing it can itself wait on the
                    // same broken driver's cancellation. No shutdown wait.
                    std::mem::forget(self.pipe.clone());
                    std::mem::forget(s);
                    continue;
                }
            }
        }
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
    /// Opened with `FILE_FLAG_OVERLAPPED` (the virtio-serial ports, §87), so
    /// reads and writes go through [`overlapped_io`] and do not wait for each
    /// other.
    overlapped: bool,
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
    // SAFETY: a handle this process just opened on a port, owned by nobody else.
    open_virtio_port_handle(name, timeout_ms).map(|h| unsafe { Pipe::from_port_handle(h) })
}

/// The open half of [`open_virtio_port`]: the raw handle, opened overlapped,
/// owned by the caller (§111.8: the broker opens the port, moves the handle
/// into the process that rings it, and that process wraps it with
/// [`Pipe::from_port_handle`]).
#[cfg(windows)]
pub fn open_virtio_port_handle(name: &str, timeout_ms: u32) -> Result<isize> {
    // **Two spellings, tried in order, and the second is not a guess.**
    //
    // `\\.\Global\<name>` is the one to want: the session-0 namespace, reachable
    // from a service and from a desktop process alike, and the guest half of
    // this project has run as both.
    //
    // It is tried *first* and not *only* because on the deployment guest it
    // fails with `ERROR_ACCESS_DENIED` (5) — measured, not supposed. `vioser.sys`
    // publishes the port under both namespaces and the security descriptor on
    // the `Global\` alias is not the one on the device itself, so an
    // unprivileged process is refused there and admitted here. Falling back is
    // strictly better than requiring elevation for a doorbell.
    //
    // A `Global\` failure is therefore not reported unless the plain form fails
    // too, and the error that surfaces is the plain form's — which is the one
    // that says something about the port rather than about the alias.
    //
    // §91 corrects the reason, not the order. On testhost's guest `Global\<name>`
    // is a symbolic link to the port's PDO itself (`QueryDosDevice` gives
    // `\Device\000000NN`), and that device object carries vioser's raw-PDO
    // default, `D:P(A;;FA;;;SY)(A;;FA;;;BA)`: the refusal of an unprivileged
    // process is the device's own descriptor, under either spelling.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(u64::from(timeout_ms));
    // A `name` that is already a device path is used verbatim, so a caller that
    // has identified a port some other way can hand it over. §36.13 measured a
    // guest where neither spelling below opened an *existing* port, and the only
    // non-guessing way to identify one is to diff `Control\DeviceClasses`
    // before and after a QMP `device_add` — which yields a device path, not a
    // name.
    let paths: Vec<String> = if name.starts_with("\\\\") {
        vec![name.to_string()]
    } else {
        vec![format!("\\\\.\\Global\\{name}"), format!("\\\\.\\{name}")]
    };
    // **Access denied is not a port that has not appeared yet** (§91.3). A
    // hot-added port that vioser has not bound is ERROR_FILE_NOT_FOUND, and that
    // is what the ten seconds are for. ERROR_ACCESS_DENIED is one of two
    // things: the port's descriptor refuses this token (vioser's default admits
    // only SYSTEM and Administrators, so a medium-integrity launch from Explorer
    // is refused; since §111.8 the broker opens it and moves the handle), or another process
    // holds it (a port has one opener). Neither goes away in ten seconds, and
    // retrying for ten made every such launch ten seconds slower: §90.3's
    // `gpu ok … (10378 ms)`. One second covers a previous session that is still
    // closing its handle.
    let denied_deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
    let mut last = 0i32;
    loop {
        for path in &paths {
            let wpath = wide(path);
            // SAFETY: `wpath` outlives the call.
            let handle = unsafe {
                sys::CreateFileW(
                    wpath.as_ptr(),
                    sys::GENERIC_READ | sys::GENERIC_WRITE,
                    0,
                    core::ptr::null_mut(),
                    sys::OPEN_EXISTING,
                    sys::FILE_FLAG_OVERLAPPED,
                    0,
                )
            };
            if handle != sys::INVALID_HANDLE_VALUE {
                return Ok(handle);
            }
            last = unsafe { sys::GetLastError() } as i32;
        }
        const ERROR_ACCESS_DENIED: i32 = 5;
        let now = std::time::Instant::now();
        if last == ERROR_ACCESS_DENIED && now >= denied_deadline.min(deadline) {
            return Err(Error::Os {
                call: "CreateFileW (virtio-serial port: access denied, so either this token may \
                       not open it -- the ports admit only SYSTEM and Administrators, and the \
                       broker (sv-guest broker, waycompose-design.md §111.8) opens them for users -- \
                       or another process holds it)",
                errno: last,
            });
        }
        if now >= deadline {
            return Err(Error::Os { call: "CreateFileW (virtio-serial port)", errno: last });
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(windows)]
impl Pipe {
    /// Wrap a virtio-serial port handle opened overlapped, as
    /// [`open_virtio_port_handle`] opens one (in this process, or in the
    /// broker that moved it here, §111.8).
    ///
    /// # Safety
    /// `handle` must be an open port handle opened with
    /// `FILE_FLAG_OVERLAPPED`, owned by the caller and closed by nobody else.
    pub unsafe fn from_port_handle(handle: isize) -> Pipe {
        Pipe { handle, server: false, overlapped: true }
    }

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
        Ok(Pipe { handle, server: true, overlapped: false })
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
                return Ok(Pipe { handle, server: false, overlapped: false });
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
        let failed = if self.overlapped {
            match overlapped_io(self.handle, false, buf.as_mut_ptr(), len) {
                Ok(n) => {
                    got = n;
                    None
                }
                Err(e) => Some(e),
            }
        } else {
            // SAFETY: `buf` is valid for `len` bytes and `handle` is live.
            let ok = unsafe {
                sys::ReadFile(self.handle, buf.as_mut_ptr(), len, &mut got, core::ptr::null_mut())
            };
            (ok == 0).then(|| unsafe { sys::GetLastError() })
        };
        if let Some(e) = failed {
            // A closed pipe reads as an error, and the layer above wants EOF —
            // `stream::read_message` turns a zero-length read into "the cold
            // path closed", which is the message a human needs.
            const ERROR_BROKEN_PIPE: u32 = 109;
            const ERROR_PIPE_NOT_CONNECTED: u32 = 233;
            if e == ERROR_BROKEN_PIPE || e == ERROR_PIPE_NOT_CONNECTED {
                return Ok(0);
            }
            // **A virtio-serial port whose host end has nobody attached fails a
            // read with `ERROR_NO_SYSTEM_RESOURCES`**, which is a startling
            // spelling of "not ready". Measured on the deployment guest, not
            // guessed: the guest half necessarily opens before the host process
            // connects — a harness has to see "echoing" in the log before it is
            // safe to measure — and the first run died here instead of waiting.
            //
            // It is *not* an end. The Linux side of the same situation reads
            // EOF, which is already treated as "nothing yet"; reporting this as
            // an error would make every guest-first bring-up a failure. So it
            // becomes `WouldBlock` and the caller retries, which is what the
            // condition actually is.
            const ERROR_NO_SYSTEM_RESOURCES: u32 = 1450;
            if e == ERROR_NO_SYSTEM_RESOURCES {
                return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
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
        if self.overlapped {
            // The buffer is only read; `overlapped_io` takes `*mut` for both
            // directions.
            return overlapped_io(self.handle, true, buf.as_ptr().cast_mut(), len)
                .map(|n| n as usize)
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32));
        }
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
        // An overlapped write has completed or been retained after timeout, and
        // a flush on the handle is one more call that could queue behind the
        // reader on a driver that serialises it; there is nothing to flush.
        if self.overlapped {
            return Ok(());
        }
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

#[cfg(all(test, windows))]
mod async_writer_tests {
    use super::*;

    #[test]
    fn stalled_cancels_do_not_consume_the_active_pool() {
        // No driver receives these records. Mark them pending to model a
        // driver that never completes cancellation, without loading vioser.
        let pipe = std::sync::Arc::new(Pipe { handle: sys::INVALID_HANDLE_VALUE, server: false, overlapped: true });
        let mut writer = AsyncWriter::new(pipe).unwrap();
        for slot in &mut writer.slots {
            slot.busy = true;
            slot.ov.internal = AsyncWriter::PENDING;
            slot.started = std::time::Instant::now() - std::time::Duration::from_secs(2);
        }
        let address = &writer.slots[0].ov as *const sys::Overlapped;
        writer.reap();
        assert_eq!(writer.retired.len(), AsyncWriter::SLOTS);
        assert!(writer.slots.iter().all(|slot| !slot.busy));
        assert_eq!(address, &writer.retired[0].ov as *const sys::Overlapped);
        // Model a late completion, then require reuse of the retired slot.
        writer.retired[0].ov.internal = 0;
        writer.slots[0].busy = true;
        writer.slots[0].ov.internal = AsyncWriter::PENDING;
        writer.slots[0].started = std::time::Instant::now() - std::time::Duration::from_secs(2);
        writer.reap();
        assert_eq!(writer.retired.len(), AsyncWriter::SLOTS);
        assert_eq!(address, &writer.slots[0].ov as *const sys::Overlapped);
        // Repeated permanent failure is bounded, even when no cancel completes.
        for _ in 0..12 {
            for slot in &mut writer.slots {
                slot.busy = true;
                slot.ov.internal = AsyncWriter::PENDING;
                slot.started = std::time::Instant::now() - std::time::Duration::from_secs(2);
            }
            writer.reap();
        }
        assert_eq!(writer.retired.len(), AsyncWriter::MAX_RETIRED);
        assert!(writer.slots.iter().all(|slot| slot.busy));
        // The fake outstanding operations have no kernel owner and may be freed.
        for slot in writer.slots.iter_mut().chain(writer.retired.iter_mut()) {
            slot.busy = false;
        }
    }
}
