//! Broker-owned wake fanout. Local pipes carry only coalesced RESCAN bytes.
//! They never carry BAR data or grant mapping/reclamation authority.
use super::{
    Object,
    ident::Owned,
    pipe::{Sd, wide},
};
use crate::core::User;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use sv_transport::winpipe::Pipe;
use windows_sys::Win32::Foundation::{
    ERROR_PIPE_CONNECTED, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_WRITE_DATA,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

const LIMIT: usize = 64;
const RESCAN: u8 = sv_transport::park::RESCAN_TOKEN;
static IDS: AtomicU64 = AtomicU64::new(1);

/// Configuration comes only from the service's checked physical-device policy.
#[derive(Clone)]
pub struct Source {
    pub device: String,
    pub path: String,
    pub owner: User,
}

pub struct Hub {
    sources: Vec<Source>,
    ports: Mutex<BTreeMap<String, Arc<Port>>>,
    slots: Arc<AtomicUsize>,
    stopped: AtomicBool,
}
impl Hub {
    pub fn new(sources: Vec<Source>) -> Arc<Self> {
        Arc::new(Self {
            sources,
            ports: Mutex::new(BTreeMap::new()),
            slots: Arc::new(AtomicUsize::new(0)),
            stopped: AtomicBool::new(false),
        })
    }
    pub fn subscribe(&self, device: &str, user: &User, process: Arc<Owned>) -> io::Result<Object> {
        let source = self
            .sources
            .iter()
            .find(|s| s.device == device)
            .ok_or_else(|| denied("unknown broker wake source"))?;
        if user != &source.owner {
            return Err(denied("wake source belongs to another SID"));
        }
        let mut ports = self.ports.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "broker wake hub stopped",
            ));
        }
        // A worker ending is not native IO retirement. A failed source keeps
        // its original handle and every uncertain read/write generation for
        // this hub's lifetime. Explicit reattach cannot silently reopen it.
        if !ports.contains_key(device) {
            ports.insert(device.into(), Port::open(&source.path, self.slots.clone())?);
        }
        let port = ports.get(device).unwrap().clone();
        drop(ports);
        port.subscribe(process)
    }
    /// End only this broker's wake registrations, never any BAR ownership.
    pub fn shutdown(&self) {
        let ports = self.ports.lock().unwrap_or_else(|e| e.into_inner());
        self.stopped.store(true, Ordering::Release);
        let live: Vec<_> = ports.values().cloned().collect();
        drop(ports);
        for port in live {
            port.fail();
        }
    }
    /// Called by the existing broker watch of this exact process object.
    pub fn process_exited(&self, process: &Arc<Owned>) {
        let ports: Vec<_> = self
            .ports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for port in ports {
            let ids: Vec<_> = port
                .members
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|(_, s)| Arc::ptr_eq(&s.process, process))
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                port.remove(id);
            }
        }
    }
}
impl Drop for Hub {
    fn drop(&mut self) {
        for port in self
            .ports
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            port.fail();
        }
    }
}
fn denied(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why)
}
fn cancel(raw: usize) {
    // SAFETY: every caller retains the Pipe owning this exact handle until all
    // overlapped calls return. Cancellation is not completion or handle release.
    unsafe {
        CancelIoEx(raw as HANDLE, std::ptr::null());
    }
}
struct Port {
    pipe: Arc<Pipe>,
    raw: usize,
    members: Mutex<BTreeMap<u64, Arc<Subscription>>>,
    slots: Arc<AtomicUsize>,
    failed: AtomicBool,
    dirty: Mutex<bool>,
    changed: Condvar,
    workers: AtomicUsize,
    stop: Owned,
    readiness: Mutex<Readiness>,
    read_changed: Condvar,
    ever_ready: AtomicBool,
    #[cfg(test)]
    test_probes: Mutex<std::collections::VecDeque<Probe>>,
    #[cfg(test)]
    test_attempts: AtomicUsize,
}
#[cfg(test)]
enum Probe {
    Completed1450,
    Ready1450,
    IncompleteRead,
    Gate1450 {
        entered: Arc<Owned>,
        release: Arc<Owned>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Starting,
    Ready,
    Unavailable,
}
struct Readiness {
    phase: Phase,
    waiters: usize,
}

impl Port {
    fn open(path: &str, slots: Arc<AtomicUsize>) -> io::Result<Arc<Self>> {
        // The service pins this exact ACL-checked physical interface. Do not
        // resolve a client-supplied port name or an alias at subscription time.
        let raw = super::open_path(path)?;
        // SAFETY: sole broker-owned overlapped raw port, never lent to clients.
        let pipe = Arc::new(unsafe { Pipe::from_port_handle(raw as isize) });
        Self::start(pipe, raw as usize, slots)
    }
    fn start(pipe: Arc<Pipe>, raw: usize, slots: Arc<AtomicUsize>) -> io::Result<Arc<Self>> {
        let port = Arc::new(Self {
            pipe,
            raw,
            members: Mutex::new(BTreeMap::new()),
            slots,
            failed: AtomicBool::new(false),
            dirty: Mutex::new(false),
            changed: Condvar::new(),
            workers: AtomicUsize::new(0),
            stop: event()?,
            readiness: Mutex::new(Readiness {
                phase: Phase::Idle,
                waiters: 0,
            }),
            read_changed: Condvar::new(),
            ever_ready: AtomicBool::new(false),
            #[cfg(test)]
            test_probes: Mutex::new(std::collections::VecDeque::new()),
            #[cfg(test)]
            test_attempts: AtomicUsize::new(0),
        });
        for reader in [true, false] {
            let owner = port.clone();
            port.workers.fetch_add(1, Ordering::AcqRel);
            if std::thread::Builder::new()
                .name(
                    if reader {
                        "wc-bell-raw-rx"
                    } else {
                        "wc-bell-raw-tx"
                    }
                    .into(),
                )
                .spawn(move || {
                    if reader {
                        owner.read_raw();
                    } else {
                        owner.write_raw();
                    }
                    let guard = owner.readiness.lock().unwrap_or_else(|e| e.into_inner());
                    owner.workers.fetch_sub(1, Ordering::Release);
                    owner.read_changed.notify_all();
                    drop(guard);
                })
                .is_err()
            {
                port.workers.fetch_sub(1, Ordering::Release);
                port.fail();
            }
        }
        Ok(port)
    }
    /// One actual cold read probe per explicit subscription attempt. A known
    /// completed initial 1450 has no admitted subscribers or raw writes, so a
    /// later caller may retry on this SAME retained handle, without polling.
    fn ensure_ready(&self) -> io::Result<()> {
        let mut state = self.readiness.lock().unwrap_or_else(|e| e.into_inner());
        if self.failed.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "wake source failed; native IO ownership stays retained",
            ));
        }
        if state.phase == Phase::Ready {
            return Ok(());
        }
        if matches!(state.phase, Phase::Idle | Phase::Unavailable) {
            if state.waiters != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "wake host not attached",
                ));
            }
            state.phase = Phase::Starting;
            self.read_changed.notify_all();
        }
        state.waiters += 1;
        self.read_changed.notify_all();
        while state.phase == Phase::Starting && !self.failed.load(Ordering::Acquire) {
            state = self
                .read_changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        state.waiters -= 1;
        if state.phase == Phase::Ready && !self.failed.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "wake source not attached or failed",
            ))
        }
    }
    fn admitted_read(&self) {
        // No raw writer can issue before an actual pending/successful read.
        let guard = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        self.ever_ready.store(true, Ordering::Release);
        self.changed.notify_all();
        drop(guard);
        let mut state = self.readiness.lock().unwrap_or_else(|e| e.into_inner());
        state.phase = Phase::Ready;
        self.read_changed.notify_all();
    }
    fn subscribe(self: &Arc<Self>, process: Arc<Owned>) -> io::Result<Object> {
        self.ensure_ready()?;
        if self.failed.load(Ordering::Acquire) {
            return Err(io::Error::other("wake source ended; reattach explicitly"));
        }
        self.slots
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < LIMIT).then_some(n + 1)
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "wake subscription worker bound reached",
                )
            })?;
        let (pipe, raw, client) = match pair() {
            Ok(pair) => pair,
            Err(error) => {
                self.slots.fetch_sub(1, Ordering::Release);
                return Err(error);
            }
        };
        let id = IDS.fetch_add(1, Ordering::Relaxed);
        let stop = match event() {
            Ok(stop) => stop,
            Err(error) => {
                self.slots.fetch_sub(1, Ordering::Release);
                return Err(error);
            }
        };
        let sub = Arc::new(Subscription {
            id,
            pipe,
            raw,
            stop,
            process,
            port: Arc::downgrade(self),
            slots: self.slots.clone(),
            closed: AtomicBool::new(false),
            dirty: Mutex::new(true),
            changed: Condvar::new(),
        });
        {
            let mut members = self.members.lock().unwrap_or_else(|e| e.into_inner());
            if self.failed.load(Ordering::Acquire) {
                return Err(io::Error::other("wake source ended during registration"));
            }
            // The existing process watcher and this insertion serialize through
            // members. An already-signaled exact process cannot register late.
            if unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(sub.process.0, 0)
            } != windows_sys::Win32::Foundation::WAIT_TIMEOUT
            {
                return Err(denied("wake subscriber process has exited"));
            }
            members.insert(id, sub.clone());
        }
        for reader in [true, false] {
            let registration = sub.clone();
            if let Err(error) = std::thread::Builder::new()
                .name(
                    if reader {
                        "wc-bell-local-rx"
                    } else {
                        "wc-bell-local-tx"
                    }
                    .into(),
                )
                .spawn(move || {
                    if reader {
                        registration.read_local();
                    } else {
                        registration.write_local();
                    }
                })
            {
                self.remove(id);
                return Err(error);
            }
        }
        self.broadcast();
        Ok(client)
    }
    fn remove(&self, id: u64) {
        if let Some(sub) = self
            .members
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        {
            sub.close();
        }
    }
    fn broadcast(&self) {
        let subscribers: Vec<_> = self
            .members
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for sub in subscribers {
            sub.signal();
        }
    }
    fn ring(&self) {
        // A local producer must wake same-kernel peers without a VM round trip.
        self.broadcast();
        *self.dirty.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.changed.notify_one();
    }
    fn fail(&self) {
        let guard = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        self.failed.store(true, Ordering::Release);
        unsafe {
            windows_sys::Win32::System::Threading::SetEvent(self.stop.0);
        }
        self.changed.notify_all();
        drop(guard);
        let state = self.readiness.lock().unwrap_or_else(|e| e.into_inner());
        self.read_changed.notify_all();
        drop(state);
        let members = std::mem::take(&mut *self.members.lock().unwrap_or_else(|e| e.into_inner()));
        for sub in members.into_values() {
            sub.close();
        }
        self.changed.notify_all();
        cancel(self.raw);
    }
    fn read_attempt(self: &Arc<Self>) -> io::Result<bool> {
        #[cfg(test)]
        {
            self.test_attempts.fetch_add(1, Ordering::AcqRel);
            let probe = self
                .test_probes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            match probe {
                Some(Probe::Completed1450) => return Err(io::Error::from_raw_os_error(1450)),
                Some(Probe::Ready1450) => {
                    self.admitted_read();
                    return Err(io::Error::from_raw_os_error(1450));
                }
                Some(Probe::Gate1450 { entered, release }) => {
                    unsafe {
                        windows_sys::Win32::System::Threading::SetEvent(entered.0);
                    }
                    let events = [release.0, self.stop.0];
                    let status = unsafe {
                        windows_sys::Win32::System::Threading::WaitForMultipleObjects(
                            2,
                            events.as_ptr(),
                            0,
                            windows_sys::Win32::System::Threading::INFINITE,
                        )
                    };
                    return Err(io::Error::from_raw_os_error(
                        if status == windows_sys::Win32::Foundation::WAIT_OBJECT_0 {
                            1450
                        } else {
                            995
                        },
                    ));
                }
                Some(Probe::IncompleteRead) => {
                    return read_wake_ops(
                        self.clone(),
                        self.raw,
                        self.stop.0 as usize,
                        2000,
                        || self.admitted_read(),
                        |_, _| windows_sys::Win32::Foundation::WAIT_OBJECT_0,
                        |_, ov, _| {
                            assert_eq!(unsafe { std::ptr::read_volatile(&(*ov).Internal) }, 0x103);
                            Err(io::Error::from_raw_os_error(995))
                        },
                    );
                }
                None => {}
            }
        }
        read_wake_announce(
            self.clone(),
            self.raw,
            self.stop.0 as usize,
            windows_sys::Win32::System::Threading::INFINITE,
            || self.admitted_read(),
        )
    }
    fn read_raw(self: &Arc<Self>) {
        loop {
            let mut state = self.readiness.lock().unwrap_or_else(|e| e.into_inner());
            while matches!(state.phase, Phase::Idle | Phase::Unavailable)
                && !self.failed.load(Ordering::Acquire)
            {
                state = self
                    .read_changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
            if self.failed.load(Ordering::Acquire) {
                return;
            }
            drop(state);
            let result = self.read_attempt();
            match result {
                Ok(true) => self.broadcast(),
                Err(error)
                    if error.raw_os_error() == Some(1450)
                        && !self.ever_ready.load(Ordering::Acquire) =>
                {
                    // The read is known complete before readiness, with no raw
                    // write or subscriber admitted. This includes an unusual
                    // API error whose observed pending operation then completed.
                    // Keep this handle and let only a new explicit subscription
                    // trigger another probe.
                    let mut state = self.readiness.lock().unwrap_or_else(|e| e.into_inner());
                    state.phase = Phase::Unavailable;
                    self.read_changed.notify_all();
                }
                _ => {
                    self.fail();
                    return;
                }
            }
        }
    }
    fn write_raw(&self) {
        loop {
            let mut dirty = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
            while (!*dirty || !self.ever_ready.load(Ordering::Acquire))
                && !self.failed.load(Ordering::Acquire)
            {
                dirty = self.changed.wait(dirty).unwrap_or_else(|e| e.into_inner());
            }
            if self.failed.load(Ordering::Acquire) {
                return;
            }
            *dirty = false;
            drop(dirty);
            if !matches!((&*self.pipe).write(&[RESCAN]), Ok(1)) {
                self.fail();
                return;
            }
        }
    }
}
struct Subscription {
    id: u64,
    pipe: Arc<Pipe>,
    raw: usize,
    stop: Owned,
    process: Arc<Owned>,
    port: Weak<Port>,
    slots: Arc<AtomicUsize>,
    closed: AtomicBool,
    dirty: Mutex<bool>,
    changed: Condvar,
}
impl Subscription {
    fn signal(&self) {
        *self.dirty.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.changed.notify_one();
    }
    fn close(&self) {
        let guard = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        self.closed.store(true, Ordering::Release);
        unsafe {
            windows_sys::Win32::System::Threading::SetEvent(self.stop.0);
        }
        self.changed.notify_all();
        drop(guard);
        cancel(self.raw);
        unsafe {
            windows_sys::Win32::System::Pipes::DisconnectNamedPipe(self.raw as HANDLE);
        }
    }
    fn read_local(self: &Arc<Self>) {
        while !self.closed.load(Ordering::Acquire) {
            match read_wake(
                self.clone(),
                self.raw,
                self.stop.0 as usize,
                windows_sys::Win32::System::Threading::INFINITE,
            ) {
                Ok(true) => {
                    if let Some(port) = self.port.upgrade() {
                        port.ring();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                _ => break,
            }
        }
        if let Some(port) = self.port.upgrade() {
            port.remove(self.id);
        }
    }
    fn write_local(&self) {
        loop {
            let mut dirty = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
            while !*dirty && !self.closed.load(Ordering::Acquire) {
                dirty = self.changed.wait(dirty).unwrap_or_else(|e| e.into_inner());
            }
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            *dirty = false;
            drop(dirty);
            if !matches!((&*self.pipe).write(&[RESCAN]), Ok(1)) {
                if let Some(port) = self.port.upgrade() {
                    port.remove(self.id);
                }
                return;
            }
        }
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.slots.fetch_sub(1, Ordering::Release);
    }
}

fn event() -> io::Result<Owned> {
    // SAFETY: unnamed non-inheritable manual-reset event.
    let handle = unsafe {
        windows_sys::Win32::System::Threading::CreateEventW(
            std::ptr::null(),
            1,
            0,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(Owned(handle))
    }
}

/// Own every kernel-visible read byte and OVERLAPPED until actual completion.
/// The stop event also covers cancellation requested just before ReadFile.
fn read_wake<T>(owner: Arc<T>, raw: usize, stop: usize, timeout: u32) -> io::Result<bool> {
    read_wake_announce(owner, raw, stop, timeout, || {})
}
fn read_wake_announce<T>(
    owner: Arc<T>,
    raw: usize,
    stop: usize,
    timeout: u32,
    admitted: impl FnOnce(),
) -> io::Result<bool> {
    read_wake_ops(
        owner,
        raw,
        stop,
        timeout,
        admitted,
        |events, timeout| unsafe {
            windows_sys::Win32::System::Threading::WaitForMultipleObjects(
                2,
                events.as_ptr(),
                0,
                timeout,
            )
        },
        |handle, ov, done| {
            if unsafe { windows_sys::Win32::System::IO::GetOverlappedResult(handle, ov, done, 1) }
                != 0
            {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        },
    )
}
fn read_wake_ops<T>(
    owner: Arc<T>,
    raw: usize,
    stop: usize,
    timeout: u32,
    admitted: impl FnOnce(),
    wait: impl FnOnce(&[HANDLE; 2], u32) -> u32,
    finish: impl FnOnce(
        HANDLE,
        *const windows_sys::Win32::System::IO::OVERLAPPED,
        *mut u32,
    ) -> io::Result<()>,
) -> io::Result<bool> {
    use windows_sys::Win32::{
        Foundation::{ERROR_IO_PENDING, WAIT_OBJECT_0},
        Storage::FileSystem::ReadFile,
        System::IO::OVERLAPPED,
    };
    struct ReadOp<T> {
        ov: OVERLAPPED,
        bytes: [u8; 64],
        _event: Owned,
        _owner: Arc<T>,
    }
    let done_event = event()?;
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = done_event.0;
    let mut op = Box::new(ReadOp {
        ov,
        bytes: [0; 64],
        _event: done_event,
        _owner: owner,
    });
    let mut done = 0;
    // SAFETY: address-stable record and buffer, retained owner keeps raw live.
    let ok = unsafe {
        ReadFile(
            raw as HANDLE,
            op.bytes.as_mut_ptr().cast(),
            64,
            &mut done,
            &mut op.ov,
        )
    };
    let error = (ok == 0).then(io::Error::last_os_error);
    if ok != 0
        || error
            .as_ref()
            .is_some_and(|e| e.raw_os_error() == Some(ERROR_IO_PENDING as i32))
    {
        admitted();
    }
    let pending = || unsafe { std::ptr::read_volatile(&op.ov.Internal) == 0x103 };
    if error
        .as_ref()
        .is_some_and(|e| e.raw_os_error() == Some(ERROR_IO_PENDING as i32))
        || pending()
    {
        let handles = [op.ov.hEvent, stop as HANDLE];
        // SAFETY: both retained live events; stop is checked even if close won
        // the race before this read was issued. No periodic wake interval.
        let status = wait(&handles, timeout);
        if status != WAIT_OBJECT_0 {
            unsafe {
                CancelIoEx(raw as HANDLE, &op.ov);
            }
        }
        let completion_error = finish(raw as HANDLE, &op.ov, &mut done).err();
        if unsafe { std::ptr::read_volatile(&op.ov.Internal) } == 0x103 {
            // A broken completion API cannot authorize freeing kernel-owned
            // storage. Retain the owner too, including its global worker slot.
            Box::leak(op);
            return Err(io::Error::other(
                "wake read has not actually completed; ownership retained",
            ));
        }
        if status == windows_sys::Win32::Foundation::WAIT_TIMEOUT {
            return Err(io::ErrorKind::TimedOut.into());
        }
        if let Some(error) = completion_error {
            return Err(error);
        }
    } else if let Some(error) = error {
        return Err(error);
    }
    Ok(done > 0)
}

fn pair() -> io::Result<(Arc<Pipe>, usize, Object)> {
    let name = wide(&format!(
        r"\\.\pipe\wc-bell-{}-{}",
        std::process::id(),
        IDS.fetch_add(1, Ordering::Relaxed)
    ));
    let sd = Sd::from_sddl(super::holder::HOLDER_SDDL)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    // SAFETY: unique first-instance name, broker-only ACL, non-inheritable handles.
    let server = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            &attributes,
        )
    };
    if server == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let owned_server = Owned(server);
    let client = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | FILE_WRITE_DATA,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if client == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let client = unsafe { Object::from_raw(client) };
    // Client already connected synchronously above: no pending Connect request.
    if unsafe { ConnectNamedPipe(server, std::ptr::null_mut()) } == 0
        && io::Error::last_os_error().raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32)
    {
        return Err(io::Error::last_os_error());
    }
    std::mem::forget(owned_server);
    // SAFETY: now owned by Pipe; server close disconnects its sole private client.
    Ok((
        Arc::new(unsafe { Pipe::from_port_handle(server as isize) }),
        server as usize,
        client,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    fn peer(object: Object) -> (Arc<Pipe>, usize) {
        let raw = object.raw() as usize;
        std::mem::forget(object);
        // SAFETY: owned overlapped test pipe transferred out of Object.
        (
            Arc::new(unsafe { Pipe::from_port_handle(raw as isize) }),
            raw,
        )
    }
    fn process() -> Arc<Owned> {
        Arc::new(
            super::super::ident::open_process_checked(
                std::process::id(),
                &super::super::ident::current_user().unwrap(),
            )
            .unwrap(),
        )
    }
    fn notification(pipe: &Arc<Pipe>, raw: usize) {
        let stop = event().unwrap();
        assert!(
            read_wake(pipe.clone(), raw, stop.0 as usize, 2000).unwrap(),
            "wake pipe ended"
        );
    }

    fn fixture() -> (Arc<Port>, Arc<Pipe>, usize) {
        let (server, raw, client) = pair().unwrap();
        let (host, host_raw) = peer(client);
        (
            Port::start(server, raw, Arc::new(AtomicUsize::new(0))).unwrap(),
            host,
            host_raw,
        )
    }
    fn wait_workers(port: &Port) {
        let mut state = port.readiness.lock().unwrap();
        while port.workers.load(Ordering::Acquire) != 0 {
            let (next, result) = port
                .read_changed
                .wait_timeout(state, std::time::Duration::from_secs(2))
                .unwrap();
            state = next;
            assert!(!result.timed_out(), "native workers failed to stop");
        }
    }
    fn failed_hub(port: &Arc<Port>) -> (Arc<Hub>, User) {
        let user = User::sid(&super::super::ident::current_user().unwrap());
        let hub = Hub::new(vec![Source {
            device: "fixture".into(),
            path: r"\\.\must-not-open-again".into(),
            owner: user.clone(),
        }]);
        hub.ports
            .lock()
            .unwrap()
            .insert("fixture".into(), port.clone());
        (hub, user)
    }

    #[test]
    fn failed_completion_keeps_original_raw_generation_after_workers_end() {
        let (port, _host, _) = fixture();
        port.test_probes
            .lock()
            .unwrap()
            .push_back(Probe::IncompleteRead);
        let (hub, user) = failed_hub(&port);
        // This issues a genuine pending named-pipe read. Only the completion
        // API answer is fault-injected, leaving Internal == STATUS_PENDING.
        let _ = hub.subscribe("fixture", &user, process());
        assert_eq!(
            unsafe { WaitForSingleObject(port.stop.0, 2000) },
            WAIT_OBJECT_0
        );
        wait_workers(&port);
        assert_eq!(
            hub.subscribe("fixture", &user, process())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Other
        );
        assert!(Arc::ptr_eq(
            hub.ports.lock().unwrap().get("fixture").unwrap(),
            &port
        ));
        assert_eq!(port.test_attempts.load(Ordering::Acquire), 1);
    }

    #[test]
    fn failed_raw_write_source_is_sticky_even_with_no_live_worker() {
        let (port, _host, _) = fixture();
        port.ensure_ready().unwrap();
        // A write failure can retain opaque native records in Pipe's
        // quarantine. Thread completion supplies no proof of their drain.
        port.fail();
        wait_workers(&port);
        let (hub, user) = failed_hub(&port);
        assert!(hub.subscribe("fixture", &user, process()).is_err());
        assert!(Arc::ptr_eq(
            hub.ports.lock().unwrap().get("fixture").unwrap(),
            &port
        ));
    }

    #[test]
    fn completed_cold_1450_retries_only_on_explicit_subscription_same_handle() {
        let (port, host, _) = fixture();
        port.test_probes
            .lock()
            .unwrap()
            .push_back(Probe::Completed1450);
        let raw = port.raw;
        assert_eq!(
            port.ensure_ready().err().unwrap().kind(),
            io::ErrorKind::NotConnected
        );
        assert!(!port.failed.load(Ordering::Acquire));
        assert!(!port.ever_ready.load(Ordering::Acquire));
        assert_eq!(port.test_attempts.load(Ordering::Acquire), 1);
        let (client, client_raw) = peer(port.subscribe(process()).unwrap());
        assert_eq!(port.raw, raw);
        assert_eq!(port.test_attempts.load(Ordering::Acquire), 2);
        (&*host).write_all(&[RESCAN]).unwrap();
        notification(&client, client_raw);
        port.fail();
        wait_workers(&port);
    }

    #[test]
    fn concurrent_subscribers_share_one_cold_probe_and_stop_wakes_waiters() {
        let (port, _host, _) = fixture();
        let entered = Arc::new(event().unwrap());
        let release = Arc::new(event().unwrap());
        port.test_probes.lock().unwrap().push_back(Probe::Gate1450 {
            entered: entered.clone(),
            release: release.clone(),
        });
        let first = port.clone();
        let first = std::thread::spawn(move || first.ensure_ready());
        assert_eq!(
            unsafe { WaitForSingleObject(entered.0, 2000) },
            WAIT_OBJECT_0
        );
        let second = port.clone();
        let second = std::thread::spawn(move || second.ensure_ready());
        let mut state = port.readiness.lock().unwrap();
        while state.waiters < 2 {
            let (next, timeout) = port
                .read_changed
                .wait_timeout(state, std::time::Duration::from_secs(2))
                .unwrap();
            state = next;
            assert!(
                !timeout.timed_out(),
                "second subscriber did not join the probe"
            );
        }
        drop(state);
        assert_eq!(port.test_attempts.load(Ordering::Acquire), 1);
        // The release gate remains closed. Only actual hub shutdown ends it.
        port.fail();
        assert!(first.join().unwrap().is_err());
        assert!(second.join().unwrap().is_err());
        wait_workers(&port);
    }

    #[test]
    fn pending_admission_then_1450_is_sticky_not_a_cold_retry() {
        let (port, _host, _) = fixture();
        port.test_probes.lock().unwrap().push_back(Probe::Ready1450);
        let _ = port.ensure_ready();
        assert_eq!(
            unsafe { WaitForSingleObject(port.stop.0, 2000) },
            WAIT_OBJECT_0
        );
        wait_workers(&port);
        assert!(port.ever_ready.load(Ordering::Acquire));
        assert!(port.ensure_ready().is_err());
        assert_eq!(port.test_attempts.load(Ordering::Acquire), 1);
    }

    #[test]
    fn cold_idle_worker_stops_without_attempting_a_read() {
        let (port, _host, _) = fixture();
        port.fail();
        wait_workers(&port);
        assert_eq!(port.test_attempts.load(Ordering::Acquire), 0);
        assert!(port.ensure_ready().is_err());
    }

    #[test]
    fn connection_watch_uses_its_retained_process_after_pid_map_replacement() {
        use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        fn child() -> Child {
            Child(
                std::process::Command::new("powershell.exe")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "Start-Sleep -Seconds 60",
                    ])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        }
        let mut first_child = child();
        let mut second_child = child();
        let sid = super::super::ident::current_user().unwrap();
        let first =
            Arc::new(super::super::ident::open_process_checked(first_child.0.id(), &sid).unwrap());
        let second =
            Arc::new(super::super::ident::open_process_checked(second_child.0.id(), &sid).unwrap());
        let (port, _host, _) = fixture();
        let _first_endpoint = port.subscribe(first.clone()).unwrap();
        let _second_endpoint = port.subscribe(second.clone()).unwrap();
        let first_registration = port
            .members
            .lock()
            .unwrap()
            .values()
            .find(|s| Arc::ptr_eq(&s.process, &first))
            .unwrap()
            .clone();
        let second_registration = port
            .members
            .lock()
            .unwrap()
            .values()
            .find(|s| Arc::ptr_eq(&s.process, &second))
            .unwrap()
            .clone();
        let hub = Hub::new(Vec::new());
        hub.ports
            .lock()
            .unwrap()
            .insert("fixture".into(), port.clone());
        let mut windows = super::super::Windows::bind(
            &format!(r"\\.\pipe\wc-bell-watch-{}", std::process::id()),
            super::super::holder::HOLDER_SDDL,
            Vec::new(),
        )
        .unwrap();
        windows.bell_hub = Some(hub);
        // Force exactly the production interleave: another accept replaces the
        // numeric map before this connection registers its process watch.
        windows
            .opened
            .lock()
            .unwrap()
            .insert(first_child.0.id(), second.clone());
        let (dummy_pipe, raw, _dummy_client) = pair().unwrap();
        let mut handle = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    raw as HANDLE,
                    GetCurrentProcess(),
                    &mut handle,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            },
            0
        );
        let writer = super::super::Writer {
            pipe: Arc::new(super::super::pipe::ServerPipe { handle }),
            process: first,
            user: User::sid(&sid),
        };
        let exited = Arc::new(event().unwrap());
        let signal = exited.clone();
        assert!(crate::server::Platform::watch_for(
            &windows,
            &writer,
            first_child.0.id(),
            Box::new(move || {
                unsafe {
                    windows_sys::Win32::System::Threading::SetEvent(signal.0);
                }
            })
        ));
        first_child.0.kill().unwrap();
        first_child.0.wait().unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(exited.0, 2000) },
            WAIT_OBJECT_0
        );
        assert!(first_registration.closed.load(Ordering::Acquire));
        assert!(!second_registration.closed.load(Ordering::Acquire));
        assert!(second_child.0.try_wait().unwrap().is_none());
        assert_eq!(port.members.lock().unwrap().len(), 1);
        drop(writer);
        drop(dummy_pipe);
        port.fail();
        wait_workers(&port);
    }

    /// Named pipes emulate the raw source. This excludes VM/virtio latency.
    #[test]
    #[ignore = "manual native scheduling measurement; requires real Windows"]
    fn parked_native_fanout_round_trip_latency() {
        const ROUNDS: usize = 2000;
        const WARMUP: usize = 100;
        let (port, host, host_raw) = fixture();
        let (responder, responder_raw) = peer(port.subscribe(process()).unwrap());
        let (observer, observer_raw) = peer(port.subscribe(process()).unwrap());
        let request = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let (armed_tx, armed_rx) = std::sync::mpsc::sync_channel(1);
        let request_peer = request.clone();
        let completed_peer = completed.clone();
        let responder_thread = std::thread::spawn(move || {
            let stop = event().unwrap();
            let mut seen = 0;
            while seen < ROUNDS + WARMUP {
                let wake = read_wake_ops(
                    responder.clone(),
                    responder_raw,
                    stop.0 as usize,
                    2000,
                    || {},
                    |handles, timeout| {
                        // Only announce an actual pending read entering its wait.
                        armed_tx.send(()).unwrap();
                        unsafe {
                            windows_sys::Win32::System::Threading::WaitForMultipleObjects(
                                2,
                                handles.as_ptr(),
                                0,
                                timeout,
                            )
                        }
                    },
                    |handle, ov, done| {
                        if unsafe {
                            windows_sys::Win32::System::IO::GetOverlappedResult(handle, ov, done, 1)
                        } != 0
                        {
                            Ok(())
                        } else {
                            Err(io::Error::last_os_error())
                        }
                    },
                )
                .unwrap();
                assert!(wake);
                let current = request_peer.load(Ordering::Acquire);
                if current > seen {
                    assert_eq!(current, seen + 1);
                    seen = current;
                    completed_peer.store(current, Ordering::Release);
                    (&*responder).write_all(&[RESCAN]).unwrap();
                }
            }
        });
        let observer_thread = std::thread::spawn(move || {
            let stop = event().unwrap();
            while matches!(
                read_wake(observer.clone(), observer_raw, stop.0 as usize, 2000),
                Ok(true)
            ) {}
        });
        let mut samples = Vec::with_capacity(ROUNDS);
        for sequence in 1..=ROUNDS + WARMUP {
            armed_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            request.store(sequence, Ordering::Release);
            let start = std::time::Instant::now();
            (&*host).write_all(&[RESCAN]).unwrap();
            notification(&host, host_raw);
            let elapsed = start.elapsed().as_nanos();
            assert_eq!(completed.load(Ordering::Acquire), sequence);
            if sequence > WARMUP {
                samples.push(elapsed);
            }
        }
        responder_thread.join().unwrap();
        port.fail();
        observer_thread.join().unwrap();
        wait_workers(&port);
        samples.sort_unstable();
        let mean = samples.iter().sum::<u128>() as f64 / samples.len() as f64 / 1000.0;
        println!(
            "NATIVE_PIPE_FANOUT_RTT samples={} warmup={} mean_us={:.3} p50_us={:.3} p95_us={:.3} p99_us={:.3} max_us={:.3} target_mean_us=200 physical_port=false cross_vm=false responder_pending=true",
            samples.len(),
            WARMUP,
            mean,
            samples[samples.len() / 2] as f64 / 1000.0,
            samples[samples.len() * 95 / 100] as f64 / 1000.0,
            samples[samples.len() * 99 / 100] as f64 / 1000.0,
            samples[samples.len() - 1] as f64 / 1000.0
        );
    }

    #[test]
    fn raw_and_local_wakes_broadcast_without_consuming_another_subscriber() {
        let (raw_server, raw, raw_client) = pair().unwrap();
        let (host, host_raw) = peer(raw_client);
        let port = Port::start(raw_server, raw, Arc::new(AtomicUsize::new(0))).unwrap();
        let (first, first_raw) = peer(port.subscribe(process()).unwrap());
        let (second, second_raw) = peer(port.subscribe(process()).unwrap());
        for _ in 0..64 {
            // More rounds than any initial/coalesced notifications can satisfy.
            (&*host).write_all(&[RESCAN]).unwrap();
            notification(&first, first_raw);
            notification(&second, second_raw);
            (&*first).write_all(&[17]).unwrap();
            notification(&host, host_raw);
            notification(&first, first_raw);
            notification(&second, second_raw);
        }
        port.fail();
    }

    #[test]
    fn exact_registration_close_preserves_its_peer_and_drains_blocked_reads() {
        let (raw_server, raw, raw_client) = pair().unwrap();
        let (_host, _host_raw) = peer(raw_client);
        let port = Port::start(raw_server, raw, Arc::new(AtomicUsize::new(0))).unwrap();
        let (first, _) = peer(port.subscribe(process()).unwrap());
        let first_registration = port
            .members
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        let (second, second_raw) = peer(port.subscribe(process()).unwrap());
        drop(first);
        assert_eq!(
            unsafe { WaitForSingleObject(first_registration.stop.0, 2000) },
            WAIT_OBJECT_0
        );
        assert_eq!(port.members.lock().unwrap().len(), 1);
        assert!(!port.failed.load(Ordering::Acquire));
        (&*second).write_all(&[RESCAN]).unwrap();
        notification(&second, second_raw);
        port.fail();
    }

    #[test]
    fn stopped_read_cannot_start_after_cancellation_and_strand_a_worker() {
        let (server, raw, client) = pair().unwrap();
        let _client = client;
        let stop = event().unwrap();
        unsafe {
            windows_sys::Win32::System::Threading::SetEvent(stop.0);
        }
        assert!(read_wake(server, raw, stop.0 as usize, 2000).is_err());
    }

    #[test]
    fn subscription_bound_survives_another_port_generation() {
        let slots = Arc::new(AtomicUsize::new(LIMIT));
        for _ in 0..2 {
            let (server, raw, client) = pair().unwrap();
            let _client = client;
            let port = Port::start(server, raw, slots.clone()).unwrap();
            assert_eq!(
                port.subscribe(process()).err().unwrap().kind(),
                io::ErrorKind::WouldBlock
            );
            assert!(port.members.lock().unwrap().is_empty());
            port.fail();
        }
        assert_eq!(slots.load(Ordering::Acquire), LIMIT);
    }

    #[test]
    fn wrong_sid_is_denied_before_opening_a_raw_port() {
        let hub = Hub::new(vec![Source {
            device: "wcbell".into(),
            path: r"\\.\missing-device".into(),
            owner: User::sid("S-1-5-21-111"),
        }]);
        assert_eq!(
            hub.subscribe("wcbell", &User::sid("S-1-5-21-222"), process())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(hub.ports.lock().unwrap().is_empty());
    }

    #[test]
    fn configured_hub_never_lends_a_second_raw_port_reader() {
        let windows = super::super::Windows::bind(
            &format!(r"\\.\pipe\wc-bell-no-raw-{}", std::process::id()),
            super::super::holder::HOLDER_SDDL,
            vec![super::super::DeviceAt {
                name: "wcbell".into(),
                loc: super::super::DeviceLoc::Port("missing-port".into()),
            }],
        )
        .unwrap()
        .with_bell_hub(Vec::new());
        assert_eq!(
            crate::server::Platform::open_device(&windows, "wcbell")
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn shutdown_refuses_late_subscriptions_before_opening_a_device() {
        let user = User::sid(&super::super::ident::current_user().unwrap());
        let hub = Hub::new(vec![Source {
            device: "wcbell".into(),
            path: r"\\.\missing-device".into(),
            owner: user.clone(),
        }]);
        hub.shutdown();
        assert_eq!(
            hub.subscribe("wcbell", &user, process())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::NotConnected
        );
        assert!(hub.ports.lock().unwrap().is_empty());
    }

    #[test]
    fn process_cleanup_matches_the_exact_retained_object() {
        let (server, raw, client) = pair().unwrap();
        let _client = client;
        let port = Port::start(server, raw, Arc::new(AtomicUsize::new(0))).unwrap();
        let exact = process();
        let unrelated_same_pid = process();
        let _subscription = port.subscribe(exact.clone()).unwrap();
        let hub = Hub::new(Vec::new());
        hub.ports
            .lock()
            .unwrap()
            .insert("fixture".into(), port.clone());
        hub.process_exited(&unrelated_same_pid);
        assert_eq!(port.members.lock().unwrap().len(), 1);
        let subscription = port
            .members
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        assert_eq!(
            unsafe { WaitForSingleObject(subscription.stop.0, 0) },
            WAIT_TIMEOUT
        );
        // Only the existing kernel process watcher calls this in production.
        hub.process_exited(&exact);
        assert!(port.members.lock().unwrap().is_empty());
        assert_eq!(
            unsafe { WaitForSingleObject(subscription.stop.0, 0) },
            WAIT_OBJECT_0
        );
        port.fail();
    }
}
