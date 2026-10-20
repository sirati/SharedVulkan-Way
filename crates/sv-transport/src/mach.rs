//! Owned public Mach memory-entry rights and shared receiver mappings.
//! No allocation fallback is permitted when exporting an I/O mapping fails.
use std::{io, mem::ManuallyDrop};
pub type Port = u32;
unsafe extern "C" {
    static mach_task_self_: Port;
    fn mach_port_deallocate(task: Port, name: Port) -> i32;
    fn mach_make_memory_entry_64(
        task: Port,
        size: &mut u64,
        offset: u64,
        permission: i32,
        entry: &mut Port,
        parent: Port,
    ) -> i32;
    fn mach_vm_map(
        task: Port,
        address: &mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        entry: Port,
        offset: u64,
        copy: i32,
        protection: i32,
        maximum: i32,
        inherit: i32,
    ) -> i32;
    fn mach_vm_deallocate(task: Port, address: u64, size: u64) -> i32;
}
pub fn task() -> Port {
    unsafe { mach_task_self_ }
}
pub fn check(code: i32, call: &str) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{call}: Mach error {:#x}",
            code as u32
        )))
    }
}
/// Owns one send-right reference in the current task.
#[derive(Debug)]
pub struct SendRight(Port);
impl SendRight {
    /// # Safety
    /// `port` must name one live, owned send-right reference in this task.
    pub unsafe fn from_raw(port: Port) -> Self {
        Self(port)
    }
    pub fn as_raw(&self) -> Port {
        self.0
    }
    pub fn into_raw(self) -> Port {
        ManuallyDrop::new(self).0
    }
}
impl Drop for SendRight {
    fn drop(&mut self) {
        unsafe {
            mach_port_deallocate(task(), self.0);
        }
    }
}
/// Export exactly an existing VM mapping. This may refuse PCI-backed memory.
/// # Safety
/// The caller must own a writable, page-aligned mapping of `len` bytes at
/// `address` and keep its backing valid for every use of the returned right.
pub unsafe fn memory_entry(address: u64, len: u64) -> io::Result<SendRight> {
    if address == 0 || len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty memory-entry mapping",
        ));
    }
    let mut size = len;
    let mut port = 0;
    let result = unsafe { mach_make_memory_entry_64(task(), &mut size, address, 3, &mut port, 0) };
    // Some failure paths can still return a right. Always balance it.
    let right = (port != 0).then(|| unsafe { SendRight::from_raw(port) });
    check(result, "mach_make_memory_entry_64")?;
    if size != len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory entry geometry changed",
        ));
    }
    right.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "memory entry returned a null right",
        )
    })
}
/// Retains both the received right and its shared, non-inherited VM view.
pub struct Mapping {
    address: u64,
    len: usize,
    _right: SendRight,
}
// The mapping remains stable until Drop. Synchronization belongs to the shared
// memory protocol; these types expose no Rust references into the mapping.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}
impl Mapping {
    pub fn new(right: SendRight, len: u64) -> io::Result<Self> {
        let size = usize::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mapping too large"))?;
        if size == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty mapping"));
        }
        let mut address = 0;
        // VM_FLAGS_ANYWHERE, copy=false, read-write, VM_INHERIT_NONE.
        check(
            unsafe {
                mach_vm_map(
                    task(),
                    &mut address,
                    len,
                    0,
                    1,
                    right.as_raw(),
                    0,
                    0,
                    3,
                    3,
                    2,
                )
            },
            "mach_vm_map",
        )?;
        if address == 0 {
            unsafe {
                mach_vm_deallocate(task(), address, len);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Mach mapped a null address",
            ));
        }
        Ok(Self {
            address,
            len: size,
            _right: right,
        })
    }
    pub fn as_ptr(&self) -> *mut u8 {
        self.address as *mut u8
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            mach_vm_deallocate(task(), self.address, self.len as u64);
        }
    }
}
