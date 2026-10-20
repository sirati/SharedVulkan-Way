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
const VM_PROT_READ_WRITE: i32 = 3;

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
    fn mach_make_memory_entry_64(task: Port, size: *mut u64, offset: u64,
        permission: i32, entry: *mut Port, parent: Port) -> Kr;
    fn mach_port_deallocate(task: Port, name: Port) -> Kr;
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
        if unsafe { IOObjectConformsTo(service, b"WCIVSHMEM\0".as_ptr().cast()) } == 0 {
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
        Ok(Self { connection, info: Info { bytes: words[1], page: words[2], vectors: words[3], peer_id: u16::try_from(words[4]).ok() } })
    }
    fn call(&self, selector: u32, input: &[u64]) -> io::Result<()> {
        let mut count = 0;
        check(unsafe { IOConnectCallScalarMethod(self.connection, selector, input.as_ptr(), input.len() as u32, ptr::null_mut(), &mut count) }, "WCIVSHMEM method")
    }
    /// Consume a raw connection to prevent selecting another slice under a live
    /// mapping. Core must authorize this region for its uid before calling here.
    pub fn map(self, offset: u64, len: u64) -> io::Result<Mapping> {
        let page = self.info.page;
        if len == 0 || offset % page != 0 || len % page != 0 || offset > self.info.bytes || len > self.info.bytes - offset {
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
        Ok(Mapping { device: self, address, len })
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
}
impl Mapping {
    pub fn len(&self) -> u64 { self.len }
    pub fn as_ptr(&self) -> *mut u8 { self.address as *mut u8 }
    /// Create a send right covering exactly this mapping, never its adjacent
    /// users' pages. The receiver maps it once and retains that address until
    /// its region epoch ends. This is not a copy or a POSIX shm allocation.
    pub fn memory_entry(&self) -> io::Result<MemoryEntry> {
        let mut len = self.len;
        let mut port = 0;
        check(unsafe { mach_make_memory_entry_64(task(), &mut len, self.address, VM_PROT_READ_WRITE, &mut port, 0) }, "export PCI memory entry")?;
        if len != self.len || port == 0 {
            if port != 0 { unsafe { mach_port_deallocate(task(), port); } }
            return Err(io::Error::new(io::ErrorKind::InvalidData, "memory entry geometry changed"));
        }
        Ok(MemoryEntry { port })
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        // Host owns BAR backing. Unmapping this view never unwires guest-owned
        // heap pages or returns physical guest RAM while a host GPU uses it.
        unsafe { IOConnectUnmapMemory64(self.device.connection, MEMORY, task(), self.address); }
    }
}
/// An owned Mach send right. This is NOT an fd and cannot use send_with_fds.
pub struct MemoryEntry { port: Port }
impl MemoryEntry {
    pub fn as_raw_port(&self) -> Port { self.port }
}
impl Drop for MemoryEntry {
    fn drop(&mut self) { unsafe { mach_port_deallocate(task(), self.port); } }
}
