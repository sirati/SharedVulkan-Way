//! Local Windows Wayland library boundary. Pixels stay in named sections.
//! No raw device, broker connection, VM arena or alternate allocator is used.
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use wc_handle::{Descriptor, Handle, Mapping, MappingMut};

#[cfg(windows)]
pub mod namespace;

const LIMIT: usize = 256;
const MAX_DESCRIPTOR: usize = 4096;
const MAX_SECTION: u64 = 1 << 30;

struct Table<T> {
    next: u64,
    entries: BTreeMap<u64, T>,
}
impl<T> Default for Table<T> {
    fn default() -> Self {
        Self {
            next: 1,
            entries: BTreeMap::new(),
        }
    }
}
impl<T> Table<T> {
    fn insert(&mut self, value: T) -> io::Result<u64> {
        if self.entries.len() >= LIMIT {
            return Err(io::Error::other("local resource limit"));
        }
        let id = self.next;
        self.next = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("local identities exhausted"))?;
        self.entries.insert(id, value);
        Ok(id)
    }
}
struct Connection {
    stream: wc_transport::Stream,
    #[cfg(windows)]
    _namespace: namespace::SocketPin,
}
enum View {
    Read(Mapping),
    Write(MappingMut),
}
#[derive(Clone)]
struct Capability {
    handle: Handle,
    _import: Option<Arc<Mapping>>,
}
struct Mapped {
    view: View,
    _handle: Capability,
}
#[derive(Default)]
struct State {
    connections: Table<Arc<Connection>>,
    handles: Table<Capability>,
    maps: Table<Mapped>,
}
fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}
fn lock() -> std::sync::MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}
fn missing() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "unknown local identity")
}
fn section(descriptor: &Descriptor) -> io::Result<()> {
    match descriptor {
        Descriptor::Section { name, size }
            if name.starts_with("Local\\wc-")
                && name.len() <= 256
                && !name.contains('\0')
                && !name[6..].contains(['\\', '/'])
                && *size > 0
                && *size <= MAX_SECTION =>
        {
            Ok(())
        }
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not a bounded local named section",
        )),
    }
}
fn decode(bytes: &[u8]) -> io::Result<Descriptor> {
    if bytes.is_empty() || bytes.len() > MAX_DESCRIPTOR {
        return Err(io::Error::other("descriptor bound"));
    }
    let descriptor = Descriptor::decode(bytes).map_err(io::Error::other)?;
    section(&descriptor)?;
    Ok(descriptor)
}
fn create(size: u64) -> io::Result<Handle> {
    if size == 0 || size > MAX_SECTION {
        return Err(io::Error::other("section size bound"));
    }
    let handle = Handle::create_section(size)?;
    // A process that selected a BAR allocator must never disguise it as local.
    section(handle.descriptor())?;
    Ok(handle)
}

/// # Safety
/// All pointers name live caller storage of the stated size. Returned resource
/// identities belong to this process; callers must close each alias explicitly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_local_section_create(size: u64, out: *mut u64) -> i32 {
    if out.is_null() {
        return -1;
    }
    match create(size).and_then(|h| {
        lock().handles.insert(Capability {
            handle: h,
            _import: None,
        })
    }) {
        Ok(id) => {
            unsafe { out.write(id) };
            0
        }
        Err(_) => -1,
    }
}
/// # Safety
/// out names one writable u64.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_local_section_clone(id: u64, out: *mut u64) -> i32 {
    if out.is_null() {
        return -1;
    }
    let mut state = lock();
    let Some(handle) = state.handles.entries.get(&id).cloned() else {
        return -1;
    };
    match state.handles.insert(handle) {
        Ok(id) => {
            unsafe { out.write(id) };
            0
        }
        Err(_) => -1,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn wc_local_section_close(id: u64) -> i32 {
    if lock().handles.entries.remove(&id).is_some() {
        0
    } else {
        -1
    }
}
/// Return required descriptor length, or -1. A null bytes pointer queries size.
/// No handle or transfer reservation is released by exporting a name.
/// # Safety
/// A nonnull bytes pointer names capacity writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_local_section_export(id: u64, bytes: *mut u8, capacity: u32) -> i32 {
    let state = lock();
    let Some(handle) = state.handles.entries.get(&id) else {
        return -1;
    };
    let encoded = handle.handle.descriptor().encode();
    if encoded.len() > MAX_DESCRIPTOR {
        return -1;
    }
    if !bytes.is_null() {
        if (capacity as usize) < encoded.len() {
            return -1;
        }
        unsafe { std::ptr::copy_nonoverlapping(encoded.as_ptr(), bytes, encoded.len()) };
    }
    encoded.len() as i32
}
/// # Safety
/// bytes names length readable bytes; out names one writable u64.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_local_section_import(
    bytes: *const u8,
    length: u32,
    out: *mut u64,
) -> i32 {
    if bytes.is_null() || out.is_null() || length == 0 || length as usize > MAX_DESCRIPTOR {
        return -1;
    }
    let bytes = unsafe { std::slice::from_raw_parts(bytes, length as usize) };
    let result = decode(bytes)
        .and_then(Handle::receive_descriptor)
        .and_then(|handle| {
            // Open a view now, so a disappearing/invalid name fails at import.
            let mapping = handle.map()?;
            let id = lock().handles.insert(Capability {
                handle,
                _import: Some(Arc::new(mapping)),
            })?;
            Ok(id)
        });
    match result {
        Ok(id) => {
            unsafe { out.write(id) };
            0
        }
        Err(_) => -1,
    }
}

/// # Safety
/// out_id, out_address and out_length each name one writable u64. The address
/// stays valid until wc_local_unmap, independently of capability close/clone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_local_map(
    id: u64,
    writable: i32,
    out_id: *mut u64,
    out_address: *mut u64,
    out_length: *mut u64,
) -> i32 {
    if out_id.is_null()
        || out_address.is_null()
        || out_length.is_null()
        || !matches!(writable, 0 | 1)
    {
        return -1;
    }
    let capability = { lock().handles.entries.get(&id).cloned() };
    let Some(capability) = capability else {
        return -1;
    };
    let result = if writable == 1 {
        capability.handle.map_mut().map(View::Write)
    } else {
        capability.handle.map().map(View::Read)
    };
    let Ok(mut view) = result else { return -1 };
    let (pointer, length) = match &mut view {
        View::Read(map) => (map.as_slice().as_ptr() as usize, map.as_slice().len()),
        View::Write(map) => {
            let slice = map.as_mut_slice();
            (slice.as_mut_ptr() as usize, slice.len())
        }
    };
    match lock().maps.insert(Mapped {
        view,
        _handle: capability,
    }) {
        Ok(map_id) => {
            unsafe {
                out_id.write(map_id);
                out_address.write(pointer as u64);
                out_length.write(length as u64);
            }
            0
        }
        Err(_) => -1,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn wc_local_unmap(id: u64) -> i32 {
    // Read the field to make its explicit mapping ownership visible here.
    if let Some(mapped) = lock().maps.entries.remove(&id) {
        drop(mapped.view);
        0
    } else {
        -1
    }
}

#[cfg(windows)]
fn connect_local(path: &std::path::Path) -> io::Result<Arc<Connection>> {
    let pin = namespace::SocketPin::open(path)?;
    let stream = wc_transport::connect(path)?;
    Ok(Arc::new(Connection {
        stream,
        _namespace: pin,
    }))
}
#[cfg(not(windows))]
fn connect_local(_: &std::path::Path) -> io::Result<Arc<Connection>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows local endpoint only",
    ))
}

/// # Safety
/// path names length UTF-8 bytes without NUL; out names one writable u64.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_local_connect(path: *const u8, length: u32, out: *mut u64) -> i32 {
    if path.is_null() || out.is_null() || length == 0 || length > 1024 {
        return -1;
    }
    let bytes = unsafe { std::slice::from_raw_parts(path, length as usize) };
    let Ok(path) = std::str::from_utf8(bytes) else {
        return -1;
    };
    // Local selection must never be redirected to the BAR facade by env.
    if path.contains('\0')
        || path == "broker-bar"
        || std::env::var_os("WC_WAYLAND_RING").is_some_and(|v| v == "1")
    {
        return -1;
    }
    let result =
        connect_local(std::path::Path::new(path)).and_then(|c| lock().connections.insert(c));
    match result {
        Ok(id) => {
            unsafe { out.write(id) };
            0
        }
        Err(_) => -1,
    }
}
fn connection(id: u64) -> io::Result<Arc<Connection>> {
    lock()
        .connections
        .entries
        .get(&id)
        .cloned()
        .ok_or_else(missing)
}
/// # Safety
/// bytes names capacity writable bytes, with capacity between 1 and 65536.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_local_read(id: u64, bytes: *mut u8, capacity: u32) -> i32 {
    if bytes.is_null() || capacity == 0 || capacity > 65536 {
        return -1;
    }
    let Ok(c) = connection(id) else { return -1 };
    let buffer = unsafe { std::slice::from_raw_parts_mut(bytes, capacity as usize) };
    (&c.stream)
        .read(buffer)
        .map(|n| n as i32)
        .unwrap_or_else(failed)
}
/// # Safety
/// bytes names length readable bytes, with length between 1 and 65536.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_local_write(id: u64, bytes: *const u8, length: u32) -> i32 {
    if bytes.is_null() || length == 0 || length > 65536 {
        return -1;
    }
    let Ok(c) = connection(id) else { return -1 };
    let buffer = unsafe { std::slice::from_raw_parts(bytes, length as usize) };
    (&c.stream)
        .write(buffer)
        .map(|n| n as i32)
        .unwrap_or_else(failed)
}
#[unsafe(no_mangle)]
pub extern "C" fn wc_local_disconnect(id: u64) -> i32 {
    let c = lock().connections.entries.remove(&id);
    if let Some(c) = c {
        let _ = c.stream.shutdown(std::net::Shutdown::Both);
        0
    } else {
        -1
    }
}

thread_local! { static LAST_ERROR: std::cell::Cell<i32> = const { std::cell::Cell::new(0) }; }
fn failed(error: io::Error) -> i32 {
    LAST_ERROR.with(|code| {
        code.set(match error.kind() {
            io::ErrorKind::WouldBlock => 11,
            io::ErrorKind::Interrupted => 4,
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset => 32,
            io::ErrorKind::PermissionDenied => 13,
            io::ErrorKind::NotFound => 9,
            _ => 5,
        })
    });
    -1
}
#[unsafe(no_mangle)]
pub extern "C" fn wc_local_error() -> i32 {
    LAST_ERROR.with(std::cell::Cell::get)
}
#[unsafe(no_mangle)]
pub extern "C" fn wc_local_nonblocking(id: u64, enabled: i32) -> i32 {
    connection(id)
        .and_then(|c| c.stream.set_nonblocking(enabled != 0))
        .map(|_| 0)
        .unwrap_or_else(failed)
}
/// Readiness bits are portable: 1 readable, 2 writable, 4 terminal/error.
#[unsafe(no_mangle)]
pub extern "C" fn wc_local_wait(id: u64, events: i32, timeout_ms: i32) -> i32 {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            POLLERR, POLLHUP, POLLNVAL, POLLRDNORM, POLLWRNORM, WSAPOLLFD, WSAPoll,
        };
        let Ok(c) = connection(id) else {
            return failed(missing());
        };
        let mut fd = WSAPOLLFD {
            fd: c.stream.as_raw_socket() as _,
            events: if events & 1 != 0 { POLLRDNORM } else { 0 }
                | if events & 2 != 0 { POLLWRNORM } else { 0 },
            revents: 0,
        };
        if unsafe { WSAPoll(&mut fd, 1, timeout_ms) } < 0 {
            return failed(io::Error::other("local socket poll failed"));
        }
        (if fd.revents & POLLRDNORM != 0 { 1 } else { 0 })
            | (if fd.revents & POLLWRNORM != 0 { 2 } else { 0 })
            | (if fd.revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
                4
            } else {
                0
            })
    }
    #[cfg(not(windows))]
    {
        let _ = (id, events, timeout_ms);
        failed(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows local endpoint only",
        ))
    }
}

/// Snapshot live connection, capability and mapping counts for lifecycle checks.
/// # Safety
/// out names three writable u32 objects.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_local_counts(out: *mut u32) -> i32 {
    if out.is_null() {
        return -1;
    }
    let state = lock();
    unsafe {
        out.write(state.connections.entries.len() as u32);
        out.add(1).write(state.handles.entries.len() as u32);
        out.add(2).write(state.maps.entries.len() as u32);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn table_never_reuses_identity_and_refuses_full_or_exhausted() {
        let mut table = Table::default();
        let first = table.insert(()).unwrap();
        table.entries.remove(&first);
        assert_ne!(table.insert(()).unwrap(), first);
        while table.entries.len() < LIMIT {
            table.insert(()).unwrap();
        }
        assert!(table.insert(()).is_err());
        table.entries.clear();
        table.next = u64::MAX;
        assert!(table.insert(()).is_err());
    }
    #[test]
    fn local_descriptor_policy_refuses_other_transports_and_bad_shapes() {
        let good = Descriptor::Section {
            name: "Local\\wc-0123".into(),
            size: 4096,
        };
        assert_eq!(decode(&good.encode()).unwrap(), good);
        for descriptor in [
            Descriptor::Section {
                name: "Global\\wc-other".into(),
                size: 4096,
            },
            Descriptor::Section {
                name: "Local\\wc-empty".into(),
                size: 0,
            },
            Descriptor::Section {
                name: "Local\\wc-large".into(),
                size: MAX_SECTION + 1,
            },
            Descriptor::SharedFile {
                path: "foo".into(),
                size: 4096,
            },
        ] {
            assert!(decode(&descriptor.encode()).is_err());
        }
        assert!(decode(&vec![0; MAX_DESCRIPTOR + 1]).is_err());
        assert!(decode(&good.encode()[..8]).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn native_section_aliases_maps_and_import_retain_original_object() {
        unsafe {
            let mut producer = 0;
            assert_eq!(wc_local_section_create(4096, &mut producer), 0);
            let mut alias = 0;
            assert_eq!(wc_local_section_clone(producer, &mut alias), 0);
            let mut map = 0;
            let mut address = 0;
            let mut length = 0;
            assert_eq!(
                wc_local_map(producer, 1, &mut map, &mut address, &mut length),
                0
            );
            assert_eq!(length, 4096);
            (address as *mut u8).write(0x73);
            let n = wc_local_section_export(alias, std::ptr::null_mut(), 0);
            assert!(n > 0);
            let mut descriptor = vec![0u8; n as usize];
            assert_eq!(
                wc_local_section_export(alias, descriptor.as_mut_ptr(), n as u32),
                n
            );
            assert_eq!(wc_local_section_close(producer), 0);
            assert_eq!(wc_local_section_close(alias), 0);
            // No producer capability remains. The map retains its original Handle.
            let mut consumer = 0;
            assert_eq!(
                wc_local_section_import(descriptor.as_ptr(), n as u32, &mut consumer),
                0
            );
            let mut read_map = 0;
            let mut read_address = 0;
            assert_eq!(
                wc_local_map(consumer, 0, &mut read_map, &mut read_address, &mut length),
                0
            );
            assert_eq!((read_address as *const u8).read(), 0x73);
            let mut denied = 0;
            assert_eq!(
                wc_local_map(consumer, 1, &mut denied, &mut read_address, &mut length),
                -1
            );
            assert_eq!(wc_local_section_close(consumer), 0);
            assert_eq!(wc_local_unmap(map), 0);
            assert_eq!((read_address as *const u8).read(), 0x73);
            assert_eq!(wc_local_unmap(read_map), 0);
            assert_eq!(wc_local_section_close(consumer), -1);
            assert_eq!(wc_local_unmap(read_map), -1);
        }
    }
}
