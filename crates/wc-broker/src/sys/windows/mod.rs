//! The broker's Windows half (§111.8).
//!
//! * **Who:** the pipe client's process id (`GetNamedPipeClientProcessId`)
//!   and the user SID of its token (impersonating it for one query). The
//!   process is opened once and its token must have the same SID. This
//!   refuses cross-user PID reuse, but does not independently distinguish
//!   same-SID PID reuse from the original pipe peer. Mapping and retirement
//!   retain the same target process object. SYSTEM is the only admin.
//! * **Liveness:** the process handle, waited on by the thread pool
//!   (`RegisterWaitForSingleObject`); its exit is told once.
//! * **Handles:** `DuplicateHandle` into the client's process; their values
//!   are the reply's text. A device is *moved* (`DUPLICATE_CLOSE_SOURCE`):
//!   the broker keeps no handle on it, so the device's driver sees its holder
//!   close it when the holder exits, as when the holder opened it itself.
//! * **Regions:** unnamed pagefile sections, so no other process can open one
//!   by name; large-page sections when asked and the broker holds
//!   `SeLockMemoryPrivilege`. A region-init hook (the guest-RAM stamp, §108.8)
//!   runs on each new one.

pub mod acl;
pub mod bell_hub;
pub mod holder;
pub mod ident;
pub mod pipe;
pub mod service;
mod readiness;
pub mod retirement;
pub mod protected_range;
pub mod service_identity;

use crate::core::{ObjId, Peer, Pid, RegionInfo, User};
use crate::server::{Give, Platform};
use ident::Owned;
use pipe::{Listener, ServerPipe};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::{
    DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, GetLargePageMinimum, MapViewOfFile, PAGE_READWRITE, SEC_COMMIT,
    SEC_LARGE_PAGES, UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, INFINITE, RegisterWaitForSingleObject, UnregisterWaitEx, WT_EXECUTEONLYONCE,
};

/// Who may connect: SYSTEM and Administrators fully, every authenticated
/// local user with `0x12019b`: read, `FILE_WRITE_DATA` and the write of
/// attributes, but not `FILE_APPEND_DATA`, which on a pipe is
/// `FILE_CREATE_PIPE_INSTANCE` and would let a user make an instance of the
/// broker's pipe and answer in its place (so a client opens with
/// `GENERIC_READ | FILE_WRITE_DATA`, not `GENERIC_WRITE`). Authenticated
/// users, not only interactive ones: a process the task scheduler or a
/// service starts for a user carries a batch or service logon, and it is
/// that user's all the same. Remote clients are refused by
/// `PIPE_REJECT_REMOTE_CLIENTS`. What each may then *have* is decided per
/// request, by SID.
pub const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x0012019b;;;AU)";

/// What a device the broker lends is, on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceLoc {
    /// The ivshmem device in this PCI slot of the root bus (§87).
    IvshmemSlot(u8),
    /// A virtio-serial port, by name (`\\.\Global\NAME`).
    Port(String),
    /// Any path `CreateFileW` opens read-write, overlapped: a stand-in for a
    /// device in a test (a file, a named pipe).
    Path(String),
}

impl DeviceLoc {
    /// `ivshmem:0x1c`, `port:NAME`, `path:PATH`.
    pub fn parse(s: &str) -> Result<DeviceLoc, String> {
        if let Some(v) = s.strip_prefix("ivshmem:") {
            let slot = match v.strip_prefix("0x") {
                Some(h) => u8::from_str_radix(h, 16).ok(),
                None => v.parse().ok(),
            };
            return slot.filter(|s| *s < 32).map(DeviceLoc::IvshmemSlot).ok_or_else(|| format!("{v}: not a PCI slot"));
        }
        if let Some(v) = s.strip_prefix("port:").filter(|v| !v.is_empty()) {
            return Ok(DeviceLoc::Port(v.to_string()));
        }
        if let Some(v) = s.strip_prefix("path:").filter(|v| !v.is_empty()) {
            return Ok(DeviceLoc::Path(v.to_string()));
        }
        Err(format!("{s}: a device is ivshmem:SLOT, port:NAME or path:PATH"))
    }
}

#[derive(Clone, Debug)]
pub struct DeviceAt {
    pub name: String,
    pub loc: DeviceLoc,
}

/// The four devices of a Windows waycompose guest (§87, §89): the Wayland
/// arena and the GPU device, and their two doorbell ports.
pub fn default_devices() -> Vec<DeviceAt> {
    vec![
        DeviceAt { name: "arena".into(), loc: DeviceLoc::IvshmemSlot(0x1c) },
        DeviceAt { name: "gpu".into(), loc: DeviceLoc::IvshmemSlot(0x1d) },
        DeviceAt { name: "wcbell".into(), loc: DeviceLoc::Port("org.sirati.wcbell.0".into()) },
        DeviceAt { name: "svbell".into(), loc: DeviceLoc::Port("org.sirati.svbell.0".into()) },
    ]
}

/// The region-init hook (§111.8): called once on each region the broker
/// makes, before anyone else has it, with
///
/// * the whole region, mapped read-write into the broker for this call only
///   (the broker keeps the section handle, never a view);
/// * the region's id;
/// * its page size: the large-page size for a large-page region, 4096 else.
///
/// What it returns is one word (no spaces) the `ok region` reply ends with,
/// such as §108.8's stamp nonce; an error refuses the alloc. The guest-RAM
/// stamping (`sv_transport::lpstamp`) plugs in here.
pub type RegionInit = Box<dyn Fn(&mut [u8], ObjId, u64) -> io::Result<Option<String>> + Send + Sync>;

/// A handle the broker holds or gives.
pub struct Object {
    h: HANDLE,
    large: bool,
}

// SAFETY: a kernel handle.
unsafe impl Send for Object {}
unsafe impl Sync for Object {}

impl Object {
    /// Take ownership of `h` (a region the embedding process made).
    ///
    /// # Safety
    /// `h` must be an open handle owned by the caller.
    pub unsafe fn from_raw(h: HANDLE) -> Object {
        Object { h, large: false }
    }

    pub fn raw(&self) -> HANDLE {
        self.h
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        drop(Owned(self.h));
    }
}

/// The reading half of a connection.
pub struct Conn(Arc<ServerPipe>);

impl io::Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.0).read(buf)
    }
}

/// The writing half: the pipe, and the client's process to put handles into.
pub struct Writer {
    pipe: Arc<ServerPipe>,
    process: Arc<Owned>,
    user: User,
}

impl Writer {
    /// Register an authenticated producer in a trusted allocation namespace.
    /// The registration is never derived from shared directory words or PID
    /// fields supplied by a client. The watch retains this writer's exact
    /// process object even when the pipe and writer are dropped.
    pub fn register_bar_process(
        &self,
        allocator: Arc<Mutex<crate::bar_control::Allocator>>,
    ) -> io::Result<crate::bar_control::Process> {
        let process = allocator.lock().unwrap_or_else(|e| e.into_inner())
            .register_process(self.user.clone())?;
        let on_death = Arc::clone(&allocator);
        if let Err(error) = self.on_producer_death(move |_proof| {
            // SAFETY: the non-forgeable certificate is constructed only after
            // this exact retained process object is kernel-confirmed dead.
            unsafe {
                on_death.lock().unwrap_or_else(|e| e.into_inner()).process_dead(process);
            }
        }) {
            allocator.lock().unwrap_or_else(|e| e.into_inner())
                .cancel_unused_registration(process)?;
            return Err(error);
        }
        Ok(process)
    }

    /// Register retirement against this connection's retained mapping-target
    /// kernel process object. Pipe close, PID reuse and DLL teardown do not
    /// trigger retirement. Capture the broker's trusted allocation record in
    /// `retire`; shared BAR metadata is not an ownership authority.
    pub fn on_producer_death(
        &self,
        retire: impl FnOnce(retirement::ProducerDeath) + Send + 'static,
    ) -> io::Result<()> {
        retirement::watch(Arc::clone(&self.process), Box::new(retire))
    }
}

pub struct Windows {
    listener: Listener,
    devices: Vec<DeviceAt>,
    large_pages: bool,
    bell_hub: Option<Arc<bell_hub::Hub>>,
    raw_device_paths: Option<BTreeMap<String, String>>,
    protected_bar_mode: bool,
    protected_bars: BTreeMap<String, Arc<protected_range::Device>>,
    region_init: Option<RegionInit>,
    holder: Option<Arc<holder::Link>>,
    /// Process handles opened at accept, for `watch`: the process the pid
    /// named when its token was checked, never a later one. Dropped at its exit.
    opened: Arc<Mutex<BTreeMap<Pid, Arc<Owned>>>>,
}

impl Drop for Windows {
    fn drop(&mut self) {
        // Process watches may retain the hub after the service stops. End its
        // wake endpoints now; this never retires mappings or grant resources.
        if let Some(hub) = &self.bell_hub { hub.shutdown(); }
    }
}

impl Windows {
    /// Serve on pipe `name` with descriptor `sddl` ([`PIPE_SDDL`]).
    pub fn bind(name: &str, sddl: &str, devices: Vec<DeviceAt>) -> io::Result<Windows> {
        let large_pages = ident::enable_privilege("SeLockMemoryPrivilege").unwrap_or(false);
        Ok(Windows {
            listener: Listener::new(name, sddl)?,
            devices,
            large_pages,
            bell_hub: None,
            raw_device_paths: None,
            protected_bar_mode: false,
            protected_bars: BTreeMap::new(),
            region_init: None,
            holder: None,
            opened: Arc::default(),
        })
    }

    /// Enable broker-owned raw ports and wake-only per-process subscriptions.
    pub fn with_bell_hub(mut self, sources: Vec<bell_hub::Source>) -> Self {
        self.bell_hub = Some(bell_hub::Hub::new(sources));
        self
    }

    /// Pin service raw-device opens to the exact interface whose durable
    /// owner was checked at startup. Aliases cannot select a different BAR.
    pub fn with_raw_device_paths(mut self, paths: BTreeMap<String, String>) -> Self {
        self.raw_device_paths = Some(paths);
        self
    }

    /// Reserve every BAR/path lending route before protected initialization.
    /// A failed capability query never re-enables the legacy lending path.
    /// This alone does not activate a pool or authorize client mappings.
    pub fn reserve_bars_for_protected_mode(mut self) -> Self {
        self.protected_bar_mode = true;
        self
    }

    /// Retain the original service-owned protected device. Only service code
    /// can use it; no request returns its raw HANDLE to a desktop process.
    pub fn with_protected_bar(mut self, name: &str,
        device: Arc<protected_range::Device>) -> io::Result<Self> {
        if !self.protected_bar_mode || self.protected_bars.contains_key(name)
            || !self.devices.iter().any(|d| d.name == name && matches!(d.loc, DeviceLoc::IvshmemSlot(_))) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "invalid protected BAR registration"));
        }
        self.protected_bars.insert(name.to_string(), device);
        Ok(self)
    }

    pub fn protected_bar(&self, name: &str) -> Option<&Arc<protected_range::Device>> {
        self.protected_bars.get(name)
    }

    /// Run `f` on every new region (§108.8's stamp plugs in here).
    pub fn with_region_init(mut self, f: RegionInit) -> Windows {
        self.region_init = Some(f);
        self
    }

    /// Keep a copy of every region in a holder process on pipe `pipe`,
    /// started with `spawn` if it is not running, and take back the regions
    /// a previous run left there (§111.8).
    pub fn with_holder(mut self, pipe: &str, spawn: holder::Spawn) -> io::Result<Windows> {
        self.holder = Some(holder::Link::start(pipe, spawn)?);
        Ok(self)
    }

    /// The holder's process id, when there is one.
    pub fn holder_pid(&self) -> Option<u32> {
        self.holder.as_ref()?.holder_pid()
    }

    pub fn large_pages(&self) -> bool {
        self.large_pages
    }

    fn identify(&self, pipe: &ServerPipe) -> io::Result<(Peer, Arc<Owned>)> {
        let (pid, sid) = ident::pipe_client(pipe.handle)?;
        let process = Arc::new(if self.protected_bar_mode {
            ident::open_protected_process_checked(pid, &sid)?
        } else {
            ident::open_process_checked(pid, &sid)?
        });
        let admin = sid == ident::SYSTEM_SID;
        Ok((Peer { pid, user: User::sid(&sid), admin }, process))
    }

}

fn os(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// Open a stand-in device by path, overlapped, exclusively (as a port is).
fn open_path(path: &str) -> io::Result<HANDLE> {
    let w = pipe::wide(path);
    // SAFETY: a live path; no attributes, no template.
    let h = unsafe {
        CreateFileW(w.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, std::ptr::null_mut())
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(os(&format!("opening {path}")));
    }
    Ok(h)
}

struct Watch {
    exited: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    process: Arc<Owned>,
    wait: Mutex<HANDLE>,
}

// SAFETY: the wait handle is only touched under its mutex.
unsafe impl Send for Watch {}
unsafe impl Sync for Watch {}

/// Watches whose process has exited, to be unregistered outside the
/// callback (a wait cannot be unregistered blockingly from its own callback).
static DONE: Mutex<Vec<usize>> = Mutex::new(Vec::new());

unsafe extern "system" fn on_exit(ctx: *mut core::ffi::c_void, _timed_out: bool) {
    // SAFETY: the Watch registered with this callback, alive until it is
    // reaped from DONE, which happens only after this callback has returned.
    let w = unsafe { &*(ctx as *const Watch) };
    let f = w.exited.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(f) = f {
        f();
    }
    DONE.lock().unwrap_or_else(|e| e.into_inner()).push(ctx as usize);
}

/// Unregister and free the watches whose process has exited.
fn reap() {
    let done: Vec<usize> = std::mem::take(&mut *DONE.lock().unwrap_or_else(|e| e.into_inner()));
    for p in done {
        // SAFETY: a Watch leaked in `watch`, reaped once.
        let w = unsafe { Box::from_raw(p as *mut Watch) };
        let wait = *w.wait.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the registered wait; INVALID_HANDLE_VALUE waits for its
        // callback to have returned, which it has (it queued this).
        unsafe { UnregisterWaitEx(wait, INVALID_HANDLE_VALUE) };
        drop(w);
    }
}

/// Retain the exact process object rather than reopening a process by PID.
fn watch_process(process: Arc<Owned>, exited: Box<dyn FnOnce() + Send>) -> bool {
    let w = Box::into_raw(Box::new(Watch {
        exited: Mutex::new(Some(exited)),
        process,
        wait: Mutex::new(std::ptr::null_mut()),
    }));
    // SAFETY: `w` is live until reaped.
    let wref = unsafe { &*w };
    // The wait handle is stored before the callback can read it: the
    // callback does not read it, and `reap` takes this lock.
    let mut slot = wref.wait.lock().unwrap_or_else(|e| e.into_inner());
    let mut wait: HANDLE = std::ptr::null_mut();
    // SAFETY: a live process handle (kept by the Watch) and context.
    let ok = unsafe {
        RegisterWaitForSingleObject(&mut wait, wref.process.0, Some(on_exit), w.cast(), INFINITE, WT_EXECUTEONLYONCE)
    };
    if ok == 0 {
        drop(slot);
        // SAFETY: never registered; freed here once.
        drop(unsafe { Box::from_raw(w) });
        return false;
    }
    *slot = wait;
    true
}

impl Platform for Windows {
    type Conn = Conn;
    type Writer = Writer;
    type Object = Object;

    fn accept(&self) -> io::Result<(Conn, Writer, Peer)> {
        reap();
        let pipe = Arc::new(self.listener.accept()?);
        let (peer, process) = match self.identify(&pipe) {
            Ok(x) => x,
            Err(e) => {
                let _ = (&*pipe).write_all(format!("err {e}\n").as_bytes());
                return Err(e);
            }
        };
        self.opened.lock().unwrap_or_else(|e| e.into_inner()).insert(peer.pid, Arc::clone(&process));
        Ok((Conn(Arc::clone(&pipe)), Writer { pipe, process, user: peer.user.clone() }, peer))
    }

    fn watch(&self, pid: Pid, exited: Box<dyn FnOnce() + Send>) -> bool {
        // Every connection of one process watches the same handle; the entry
        // goes when that process has exited (unless a new process of the
        // same pid has replaced it meanwhile).
        let Some(process) = self.opened.lock().unwrap_or_else(|e| e.into_inner()).get(&pid).cloned() else {
            return false;
        };
        let (opened, mine) = (Arc::clone(&self.opened), Arc::clone(&process));
        let bell_hub = self.bell_hub.clone();
        let exited: Box<dyn FnOnce() + Send> = Box::new(move || {
            {
                let mut m = opened.lock().unwrap_or_else(|e| e.into_inner());
                if m.get(&pid).is_some_and(|p| Arc::ptr_eq(p, &mine)) {
                    m.remove(&pid);
                }
            }
            if let Some(hub) = bell_hub { hub.process_exited(&mine); }
            exited();
        });
        watch_process(process, exited)
    }

    fn open_device(&self, name: &str) -> io::Result<Object> {
        let d = self
            .devices
            .iter()
            .find(|d| d.name == name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("device {name} has no location here")))?;
        if self.protected_bar_mode && !matches!(d.loc, DeviceLoc::Port(_)) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied,
                "protected mode never lends a raw BAR or path alias"));
        }
        if self.bell_hub.is_some() && matches!(d.loc, DeviceLoc::Port(_)) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "raw doorbell ports stay broker-owned; use wake subscription"));
        }
        let h = match &d.loc {
            DeviceLoc::IvshmemSlot(s) => match &self.raw_device_paths {
                Some(paths) => {
                    let path = paths.get(name).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "raw device has no trusted durable owner",
                        )
                    })?;
                    wc_handle::open_ivshmem_path(path.clone())?.into_raw()
                }
                None => wc_handle::open_ivshmem_slot(*s)?.into_raw(),
            },
            DeviceLoc::Port(p) => sv_transport::winpipe::open_virtio_port_handle(p, 500)
                .map_err(|e| io::Error::other(format!("port {p}: {e}")))? as HANDLE,
            DeviceLoc::Path(p) => open_path(p)?,
        };
        Ok(Object { h, large: false })
    }

    fn open_service_for(&self, writer: &Writer, peer: &Peer, name: &str) -> io::Result<Object> {
        let device = name.strip_prefix("bell:").ok_or_else(|| io::Error::other("unknown Windows local service"))?;
        if writer.user != peer.user { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "wake subscriber identity mismatch")); }
        self.bell_hub.as_ref().ok_or_else(|| io::Error::other("broker wake hub is unavailable"))?
            .subscribe(device, &writer.user, Arc::clone(&writer.process))
    }

    fn create_region(&self, len: u64, large: bool) -> io::Result<Object> {
        let mut flags = PAGE_READWRITE | SEC_COMMIT;
        if large {
            if !self.large_pages {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "large pages need SeLockMemoryPrivilege, which this broker's account does not hold",
                ));
            }
            // SAFETY: no arguments.
            let unit = unsafe { GetLargePageMinimum() } as u64;
            if unit == 0 || !len.is_multiple_of(unit) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("a large-page region is a whole number of {unit}-byte pages, not {len} bytes"),
                ));
            }
            // Keep the guest's working memory available, as the old keeper
            // did. A broker budget alone does not account for other apps.
            let (total, available) = sv_transport::winram::memory_status();
            let reserve = std::env::var("WC_RAM_RESERVE_MIB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map_or((1u64 << 30).max(total / 100 * 15), |m| {
                    m.saturating_mul(1 << 20)
                });
            if available < len.saturating_add(reserve) {
                return Err(io::Error::other(format!(
                    "budget: {} MiB asked, {} MiB available, {} MiB kept for Windows",
                    len >> 20,
                    available >> 20,
                    reserve >> 20
                )));
            }
            flags |= SEC_LARGE_PAGES;
        }
        // SAFETY: a pagefile-backed section; no attributes and NO NAME, so
        // only handles reach it.
        let h = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), flags, (len >> 32) as u32, len as u32, std::ptr::null())
        };
        if h.is_null() {
            return Err(os(&format!("CreateFileMappingW({len} bytes{})", if large { ", large pages" } else { "" })));
        }
        Ok(Object { h, large })
    }

    fn send(&self, w: &mut Writer, make: &dyn Fn(&[String]) -> String, give: Vec<Give<'_, Object>>) -> io::Result<()> {
        // SAFETY: the pseudo-handle of this process.
        let me = unsafe { GetCurrentProcess() };
        let mut out: Vec<HANDLE> = Vec::new();
        let mut failed = None;
        for g in give {
            let (src, close) = match &g {
                Give::Move(o) => (o.h, true),
                Give::Copy(o) => (o.h, false),
            };
            let mut dst: HANDLE = std::ptr::null_mut();
            let opts = DUPLICATE_SAME_ACCESS | if close { DUPLICATE_CLOSE_SOURCE } else { 0 };
            // SAFETY: a live source handle of this process and a live target
            // process handle opened with PROCESS_DUP_HANDLE.
            let ok = unsafe { DuplicateHandle(me, src, w.process.0, &mut dst, 0, 0, opts) };
            if let Give::Move(o) = g {
                // DUPLICATE_CLOSE_SOURCE closed the broker's handle, whether
                // or not the duplicate was made.
                std::mem::forget(o);
            }
            if ok == 0 {
                failed = Some(os("DuplicateHandle into the client"));
                break;
            }
            out.push(dst);
        }
        let texts: Vec<String> = out.iter().map(|h| format!("{:#x}", *h as usize)).collect();
        let r = match failed {
            Some(e) => Err(e),
            None => {
                let mut line = make(&texts);
                line.push('\n');
                (&*w.pipe).write_all(line.as_bytes())
            }
        };
        if r.is_err() {
            // The client never learned of them: close them in its process.
            for h in out {
                // SAFETY: a handle this broker just put into the client.
                unsafe { DuplicateHandle(w.process.0, h, std::ptr::null_mut(), std::ptr::null_mut(), 0, 0, DUPLICATE_CLOSE_SOURCE) };
            }
        }
        r
    }

    fn push(&self, w: &mut Writer, line: &str) -> io::Result<()> {
        let mut l = line.to_string();
        l.push('\n');
        (&*w.pipe).write_all(l.as_bytes())
    }

    fn region_made(&self, info: &RegionInfo, user: &User, obj: &Object) -> io::Result<Option<String>> {
        let mut note = None;
        if let Some(init) = &self.region_init {
            let len = usize::try_from(info.len).map_err(|_| io::Error::other("region too large to map"))?;
            // SAFETY: a fresh section of `len` bytes, mapped for this call only.
            let v = unsafe { MapViewOfFile(obj.h, FILE_MAP_ALL_ACCESS, 0, 0, len) };
            if v.Value.is_null() {
                return Err(os("MapViewOfFile (region init)"));
            }
            // SAFETY: the view just made, `len` bytes, nobody else's yet.
            let bytes = unsafe { std::slice::from_raw_parts_mut(v.Value.cast::<u8>(), len) };
            // SAFETY: no arguments.
            let unit = if obj.large { (unsafe { GetLargePageMinimum() }) as u64 } else { 4096 };
            let r = init(bytes, info.id, unit);
            // SAFETY: the view made above, unmapped once.
            unsafe { UnmapViewOfFile(v) };
            note = r?;
        }
        if let Some(h) = &self.holder
            && let Err(e) = h.add(info, user, obj.h)
        {
            // Link::add registers the raw handle before transferring it.
            // The failed allocation drops that handle, so the restart
            // watcher must not retain it and later duplicate a reused value.
            h.del(info.id);
            return Err(e);
        }

        Ok(note)
    }

    fn region_destroyed(&self, id: ObjId) {
        if let Some(h) = &self.holder {
            h.del(id);
        }
    }

    fn restore(&self) -> Vec<(RegionInfo, User, Object)> {
        let Some(h) = &self.holder else { return Vec::new() };
        h.restore().into_iter().map(|(info, user, raw)| (info, user, Object { h: raw, large: false })).collect()
    }

    fn identity(&self, peer: &Peer) -> Option<wc_handle::macboot::Identity> {
        let sid = peer.user.0.strip_prefix("sid:")?;
        let name = ident::account_name(sid).unwrap_or_else(|| sid.to_string());
        Some(wc_handle::macboot::Identity { os: "windows".into(), user: crate::sys::identity_word(&name), id: sid.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_mode_refuses_raw_bar_and_path_alias_lending() {
        let path = format!(r"\\.\pipe\wc-protected-denial-{}", std::process::id());
        let devices = vec![
            DeviceAt { name: "arena".into(), loc: DeviceLoc::IvshmemSlot(0x1c) },
            DeviceAt { name: "alias".into(), loc: DeviceLoc::Path("must-never-be-opened".into()) },
        ];
        let platform = Windows::bind(&path, PIPE_SDDL, devices).unwrap()
            .reserve_bars_for_protected_mode();
        for name in ["arena", "alias"] {
            let error = match platform.open_device(name) {
                Err(error) => error,
                Ok(_) => panic!("protected mode lent a raw device"),
            };
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn a_device_location_parses_from_its_three_spellings() {
        assert_eq!(DeviceLoc::parse("ivshmem:0x1c"), Ok(DeviceLoc::IvshmemSlot(0x1c)));
        assert_eq!(DeviceLoc::parse("ivshmem:29"), Ok(DeviceLoc::IvshmemSlot(29)));
        assert!(DeviceLoc::parse("ivshmem:0x20").is_err());
        assert_eq!(DeviceLoc::parse("port:org.sirati.wcbell.0"), Ok(DeviceLoc::Port("org.sirati.wcbell.0".into())));
        assert_eq!(DeviceLoc::parse(r"path:\\.\pipe\x"), Ok(DeviceLoc::Path(r"\\.\pipe\x".into())));
        assert!(DeviceLoc::parse("port:").is_err());
        assert!(DeviceLoc::parse("disk:c").is_err());
    }
}
