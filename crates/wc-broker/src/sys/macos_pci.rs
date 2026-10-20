//! Raw PCI access for the root broker, through the WCIVSHMEM IOKit userclient.
//!
//! This is deliberately below the broker's uid/lease policy. A new connection
//! selects one immutable BAR2 slice. It never exports BAR0 or the whole BAR.
//! Mach memory-entry rights still need an authenticated broker handoff before
//! desktop clients can use this backend; SCM_RIGHTS cannot carry Mach rights.
use std::{ffi::c_void, io, ptr};

type Port = u32;
type Kr = i32;
const OPEN_TYPE: u32 = 0x57435043;
const MEMORY: u32 = 1;
const INFO: u32 = 0;
const SELECT: u32 = 1;
const RING: u32 = 2;


#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegistryEntryIDMatching(id: u64) -> *mut c_void;
    fn IOServiceGetMatchingService(main: Port, matching: *mut c_void) -> Port;
    fn IOServiceOpen(service: Port, task: Port, kind: u32, connection: *mut Port) -> Kr;
    fn IOServiceClose(connection: Port) -> Kr;
    fn IOObjectRelease(object: Port) -> Kr;
    fn IOObjectConformsTo(object: Port, class: *const std::ffi::c_char) -> u32;
    fn IOConnectCallScalarMethod(connection: Port, selector: u32,
        input: *const u64, input_count: u32, output: *mut u64, output_count: *mut u32) -> Kr;
    fn IOConnectMapMemory64(connection: Port, memory: u32, task: Port,
        address: *mut u64, size: *mut u64, options: u32) -> Kr;
    fn IOConnectUnmapMemory64(connection: Port, memory: u32, task: Port, address: u64) -> Kr;
}
unsafe extern "C" {
    static mach_task_self_: Port;

}
fn check(kr: Kr, operation: &str) -> io::Result<()> {
    if kr == 0 { Ok(()) } else {
        Err(io::Error::other(format!("{operation}: Mach/IOKit error {:#x}", kr as u32)))
    }
}
fn task() -> Port { unsafe { mach_task_self_ } }

#[derive(Clone, Copy, Debug)]
pub struct Info {
    pub bytes: u64,
    pub page: u64,
    pub vectors: u64,
    pub peer_id: Option<u16>,
}

/// Only wc-broker's platform backend may open this raw resource. Registry IDs
/// identify devices across enumeration without accepting arbitrary driver paths.
pub struct Device {
    registry: u64,
    connection: Port,
    info: Info,
}
impl Device {
    pub fn info(&self) -> Info { self.info }
    pub fn open(registry_id: u64) -> io::Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "PCI access belongs to the root broker"));
        }
        let matching = unsafe { IORegistryEntryIDMatching(registry_id) };
        if matching.is_null() { return Err(io::Error::other("IORegistryEntryIDMatching failed")); }
        // IOServiceGetMatchingService consumes the matching dictionary.
        let service = unsafe { IOServiceGetMatchingService(0, matching) };
        if service == 0 { return Err(io::Error::new(io::ErrorKind::NotFound, "PCI registry entry missing")); }
        if unsafe { IOObjectConformsTo(service, c"WCIVSHMEM".as_ptr()) } == 0 {
            unsafe { IOObjectRelease(service); }
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "registry entry is not WCIVSHMEM"));
        }
        let mut connection = 0;
        let kr = unsafe { IOServiceOpen(service, task(), OPEN_TYPE, &mut connection) };
        unsafe { IOObjectRelease(service); }
        check(kr, "open WCIVSHMEM")?;
        let mut words = [0u64; 5];
        let mut count = words.len() as u32;
        let kr = unsafe { IOConnectCallScalarMethod(connection, INFO, ptr::null(), 0, words.as_mut_ptr(), &mut count) };
        if kr != 0 || count != 5 || words[0] != 1 || !words[2].is_power_of_two() || words[1] == 0 || words[3] > 2048 {
            unsafe { IOServiceClose(connection); }
            check(kr, "WCIVSHMEM info")?;
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported WCIVSHMEM ABI or device geometry"));
        }
        Ok(Self { registry:registry_id, connection, info: Info { bytes: words[1], page: words[2], vectors: words[3], peer_id: u16::try_from(words[4]).ok() } })
    }
    fn call(&self, selector: u32, input: &[u64]) -> io::Result<()> {
        let mut count = 0;
        check(unsafe { IOConnectCallScalarMethod(self.connection, selector, input.as_ptr(), input.len() as u32, ptr::null_mut(), &mut count) }, "WCIVSHMEM method")
    }
    /// Consume a raw connection to prevent selecting another slice under a live
    /// mapping. Core must authorize this region for its uid before calling here.
    pub fn map(self, offset: u64, len: u64) -> io::Result<Mapping> {
        let page = self.info.page;
        if len == 0 || !offset.is_multiple_of(page) || !len.is_multiple_of(page) || offset > self.info.bytes || len > self.info.bytes - offset {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "PCI slice is not a page-aligned BAR2 range"));
        }
        self.call(SELECT, &[offset, len])?;
        let mut address = 0;
        let mut mapped = 0;
        check(unsafe { IOConnectMapMemory64(self.connection, MEMORY, task(), &mut address, &mut mapped, 0) }, "map PCI slice")?;
        if mapped != len || address == 0 {
            unsafe { IOConnectUnmapMemory64(self.connection, MEMORY, task(), address); }
            return Err(io::Error::new(io::ErrorKind::InvalidData, "driver returned an unexpected slice"));
        }
        Ok(Mapping { device: self, address, len, irq:std::sync::Mutex::new(None) })
    }
    pub fn ring(&self, peer: u16, vector: u16) -> io::Result<()> {
        if u64::from(vector) >= self.info.vectors {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "doorbell vector outside MSI-X table"));
        }
        self.call(RING, &[peer.into(), vector.into()])
    }
}
impl Drop for Device {
    fn drop(&mut self) { unsafe { IOServiceClose(self.connection); } }
}

pub struct Mapping {
    device: Device,
    address: u64,
    len: u64,
    irq: std::sync::Mutex<Option<std::sync::Arc<Irq>>>,
}
impl Mapping {
    pub fn len(&self) -> u64 { self.len }
    pub fn is_empty(&self) -> bool { self.len == 0 }
    pub fn wait_irq(&self, seen:u32) -> io::Result<u32> {
        let owner = {
            let mut irq = self.irq.lock().unwrap_or_else(|e| e.into_inner());
            if irq.is_none() { *irq = Some(Irq::get(self.device.registry)?); }
            std::sync::Arc::clone(irq.as_ref().unwrap())
        };
        owner.wait(seen)
    }
    pub fn as_ptr(&self) -> *mut u8 { self.address as *mut u8 }
    pub fn ring(&self, peer: u16, vector: u16) -> io::Result<()> { self.device.ring(peer, vector) }
    /// Create a send right covering exactly this mapping, never its adjacent
    /// users' pages. The receiver maps it once and retains that address until
    /// its region epoch ends. This is not a copy or a POSIX shm allocation.
    pub fn memory_entry(&self) -> io::Result<sv_transport::mach::SendRight> {
        // The driver owns this immutable slice for this mapping's lifetime.
        unsafe { sv_transport::mach::memory_entry(self.address, self.len) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        // Host owns BAR backing. Unmapping this view never unwires guest-owned
        // heap pages or returns physical guest RAM while a host GPU uses it.
        unsafe { IOConnectUnmapMemory64(self.device.connection, MEMORY, task(), self.address); }
    }
}

// The driver has one IRQ owner per PCI device, not one per slice. The broker
// shares that owner across its published slices and fans out counted hints.
struct IrqState {
    stop: std::sync::atomic::AtomicBool,
    completed: std::sync::atomic::AtomicBool,
    kernel_seen: std::sync::atomic::AtomicU64,
    event: std::sync::Mutex<(u32, bool)>,
    cv: std::sync::Condvar,
}
struct Irq {
    state: std::sync::Arc<IrqState>,
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl Drop for Irq {
    fn drop(&mut self) {
        self.state.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(worker) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() { let _ = worker.join(); }
    }
}
unsafe extern "C" fn irq_callback(context:*mut c_void, result:i32, sequence:*mut c_void, _time:*mut c_void) {
    // The worker retains this state until after its notification port is
    // destroyed. The only send right to that port is passed to IOKit.
    let state = unsafe { &*context.cast::<IrqState>() };
    // IOKit error completions carry no result arguments. Only a successful
    // two-argument completion supplies the counted kernel sequence.
    if result == 0 {
        state.kernel_seen.store(sequence as usize as u64, std::sync::atomic::Ordering::Release);
    }
    let mut event = state.event.lock().unwrap_or_else(|e| e.into_inner());
    event.0 = event.0.wrapping_add(1);
    event.1 |= result != 0;
    state.completed.store(true, std::sync::atomic::Ordering::Release);
    state.cv.notify_all();
}
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IONotificationPortCreate(main:Port) -> *mut c_void;
    fn IONotificationPortGetMachPort(port:*mut c_void) -> Port;
    fn IONotificationPortDestroy(port:*mut c_void);
    fn IODispatchCalloutFromMessage(unused:*mut c_void, message:*mut c_void, reference:*mut c_void);
    fn IOConnectCallAsyncScalarMethod(connection:Port, selector:u32, wake:Port, reference:*mut u64, reference_count:u32,
        input:*const u64, input_count:u32, output:*mut u64, output_count:*mut u32) -> Kr;
    fn mach_msg(message:*mut super::machwire::Header, options:u32, send_size:u32, receive_size:u32, receive:Port, timeout:u32, notify:Port) -> Kr;
}
impl Irq {
    fn get(registry:u64) -> io::Result<std::sync::Arc<Self>> {
        use std::sync::{Arc, Mutex, OnceLock, Weak};
        static OWNERS: OnceLock<Mutex<std::collections::BTreeMap<u64, Weak<Irq>>>> = OnceLock::new();
        let mut owners = OWNERS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if let Some(owner) = owners.get(&registry).and_then(Weak::upgrade) { return Ok(owner); }
        let state = Arc::new(IrqState { stop:false.into(), completed:false.into(), kernel_seen:0.into(), event:Mutex::new((0,false)), cv:std::sync::Condvar::new() });
        let thread_state = Arc::clone(&state);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new().name("wc-pci-irq".into()).spawn(move || {
            let result = (|| {
                let device = Device::open(registry)?;
                let port = unsafe { IONotificationPortCreate(0) };
                if port.is_null() { return Err(io::Error::other("create IOKit IRQ notification port")); }
                struct Notify(*mut c_void);
                impl Drop for Notify { fn drop(&mut self) { unsafe { IONotificationPortDestroy(self.0); } } }
                let port = Notify(port);
                let wake = unsafe { IONotificationPortGetMachPort(port.0) };
                let arm = || {
                    let mut reference = [0u64;8];
                    reference[1] = irq_callback as *const () as usize as u64;
                    reference[2] = Arc::as_ptr(&thread_state) as usize as u64;
                    let seen = thread_state.kernel_seen.load(std::sync::atomic::Ordering::Acquire);
                    let mut count = 0;
                    check(unsafe { IOConnectCallAsyncScalarMethod(device.connection, 3, wake, reference.as_mut_ptr(), 8, &seen, 1, ptr::null_mut(), &mut count) }, "arm PCI IRQ")
                };
                arm()?;
                let _ = ready_tx.send(Ok(()));
                while !thread_state.stop.load(std::sync::atomic::Ordering::Acquire) {
                    let mut buffer = [0u64;128];
                    let code = unsafe { mach_msg(buffer.as_mut_ptr().cast(), 2 | 0x100, 0, 1024, wake, 100, 0) };
                    if code == 0 {
                        unsafe { IODispatchCalloutFromMessage(ptr::null_mut(), buffer.as_mut_ptr().cast(), port.0); }
                        if thread_state.completed.swap(false, std::sync::atomic::Ordering::AcqRel) {
                            if thread_state.event.lock().unwrap_or_else(|e| e.into_inner()).1 { break; }
                            arm()?;
                        }
                    } else if code != 0x10004003 { return Err(io::Error::other(format!("receive PCI IRQ: {code:#x}"))); }
                }
                Ok(())
            })();
            if let Err(error) = result { let _ = ready_tx.send(Err(error)); }
            let mut event = thread_state.event.lock().unwrap_or_else(|e| e.into_inner());
            event.1 = true;
            thread_state.cv.notify_all();
        })?;
        let owner = Arc::new(Self { state, worker:Mutex::new(Some(worker)) });
        ready_rx.recv().map_err(|_| io::Error::other("PCI IRQ worker ended during startup"))??;
        owners.insert(registry, Arc::downgrade(&owner));
        Ok(owner)
    }
    fn wait(&self, seen:u32) -> io::Result<u32> {
        let state = self.state.event.lock().unwrap_or_else(|e| e.into_inner());
        let (state, _) = self.state.cv.wait_timeout_while(state, std::time::Duration::from_millis(750), |s| s.0 == seen && !s.1)
            .unwrap_or_else(|e| e.into_inner());
        if state.1 { return Err(io::Error::other("PCI IRQ source stopped")); }
        Ok(state.0)
    }
}

/// Publish one administrator-selected slice through the existing broker core.
/// Export is probed before publication; refusal never substitutes new backing.
pub fn publish(broker:&crate::server::Broker<super::unix::Unix>, user:crate::core::User, pool:&str,
    epoch:u64, registry:u64, offset:u64, len:u64) -> io::Result<crate::core::ObjId> {
    super::pci_owner::load()?.authorize(&user)
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied,e))?;
    let mapping = std::sync::Arc::new(Device::open(registry)?.map(offset, len)?);
    drop(mapping.memory_entry()?);
    Ok(broker.publish(user, pool, epoch, vec![(super::unix::Object::MachRegion(mapping), 0, len)]))
}
