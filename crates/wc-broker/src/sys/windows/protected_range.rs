//! SYSTEM service client for the protected ivshmem driver overlay.
//!
//! Unsupported QUERY fails closed. This API never falls back to lending the
//! raw device, and never returns a raw device HANDLE to a desktop client.
use super::{Writer, ident::Owned};
use std::io;
use std::sync::Arc;
use windows_sys::Win32::System::IO::DeviceIoControl;

const QUERY: u32 = 0x0022_6080;
const MAP: u32 = 0x0022_e084;
const UNMAP: u32 = 0x0022_e088;
const CONFIGURE: u32 = 0x0022_e08c;
const POLICY_QUERY: u32 = 0x0022_6090;
const VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Caps {
    pub version: u32,
    pub reserved: u32,
    pub generation: u64,
    pub bar_bytes: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    pub version: u32,
    pub reserved: u32,
    pub generation: u64,
    pub write_offset: u64,
    pub write_length: u64,
    pub read_offset: u64,
    pub read_length: u64,
}
#[repr(C)]
#[derive(Default)]
struct PolicyStatus {
    configured: u32,
    reserved: u32,
    policy: Policy,
}
#[repr(C)]
struct MapRequest {
    version: u32,
    flags: u32,
    target_handle: u64,
    generation: u64,
    offset: u64,
    length: u64,
}
#[repr(C)]
#[derive(Default)]
struct Reply {
    generation: u64,
    mapping: u64,
    address: u64,
    length: u64,
}
#[repr(C)]
struct Release {
    version: u32,
    reserved: u32,
    generation: u64,
    mapping: u64,
}

pub struct Device {
    original: Owned,
    caps: Caps,
}
impl Device {
    /// Consume the service's original device object, opened after durable raw
    /// device identity/ACL validation. A stock whole-BAR-only driver refuses
    /// QUERY, and no protected-mode object is created.
    pub fn from_original(device: wc_handle::IvshmemDevice) -> io::Result<Arc<Self>> {
        let mut result = Self {
            original: Owned(device.into_raw()),
            caps: Caps::default(),
        };
        let mut caps = Caps::default();
        result.ioctl(QUERY, &[], bytes_mut(&mut caps))?;
        if caps.version != VERSION
            || caps.reserved != 0
            || caps.generation == 0
            || caps.bar_bytes == 0
        {
            return Err(io::Error::other("invalid protected BAR capabilities"));
        }
        result.caps = caps;
        Ok(Arc::new(result))
    }
    pub fn caps(&self) -> Caps {
        self.caps
    }
    /// Read the actual immutable kernel policy for this retained device.
    /// None means only that the current generation is unconfigured. This is
    /// not a mapping census or evidence that old grant bytes are unused.
    /// Unknown/unsupported queries and generation changes fail closed.
    pub fn policy(&self) -> io::Result<Option<Policy>> {
        let mut status = PolicyStatus::default();
        self.ioctl(POLICY_QUERY, &[], bytes_mut(&mut status))?;
        let policy = status.policy;
        if status.configured > 1
            || status.reserved != 0
            || policy.version != VERSION
            || policy.reserved != 0
            || policy.generation != self.caps.generation
        {
            return Err(io::Error::other("invalid protected BAR policy status"));
        }
        if status.configured == 0 {
            if policy.write_offset != 0
                || policy.write_length != 0
                || policy.read_offset != 0
                || policy.read_length != 0
            {
                return Err(io::Error::other("unconfigured BAR carries payload policy"));
            }
            return Ok(None);
        }
        let within = |offset: u64, length: u64| {
            offset % 4096 == 0
                && length % 4096 == 0
                && offset
                    .checked_add(length)
                    .is_some_and(|end| end <= self.caps.bar_bytes)
        };
        if policy.version != VERSION
            || policy.reserved != 0
            || policy.generation != self.caps.generation
            || policy.write_length == 0
            || !within(policy.write_offset, policy.write_length)
            || !within(policy.read_offset, policy.read_length)
            || (policy.read_length != 0
                && policy.write_offset < policy.read_offset + policy.read_length
                && policy.read_offset < policy.write_offset + policy.write_length)
        {
            return Err(io::Error::other("invalid protected BAR policy reply"));
        }
        Ok(Some(policy))
    }
    /// Install immutable payload windows. Everything outside them, including
    /// control, directory and ring metadata, remains unmappable by clients.
    ///
    /// # Safety
    /// The service must derive these ranges from its retained original BAR
    /// geometry and allocator grants, never from client-writable metadata.
    pub unsafe fn configure(&self, write: (u64, u64), read: (u64, u64)) -> io::Result<()> {
        let policy = Policy {
            version: VERSION,
            reserved: 0,
            generation: self.caps.generation,
            write_offset: write.0,
            write_length: write.1,
            read_offset: read.0,
            read_length: read.1,
        };
        self.ioctl(CONFIGURE, bytes(&policy), &mut [])
    }
    /// Map a trusted allocation into the exact authenticated producer process.
    /// The target is this Writer's retained process object, never a client PID.
    ///
    /// # Safety
    /// The service must retain the original grant and authoritative allocation
    /// record for `offset,length`, authorize this writer's user and producer
    /// registration, and keep all producer/transfer/consumer holds until their
    /// independent trusted completion. A directory tuple is not that record.
    pub unsafe fn map_payload(
        self: &Arc<Self>,
        writer: &Writer,
        offset: u64,
        length: u64,
        writable: bool,
    ) -> io::Result<Mapping> {
        let input = MapRequest {
            version: VERSION,
            flags: u32::from(writable),
            target_handle: writer.process.0 as usize as u64,
            generation: self.caps.generation,
            offset,
            length,
        };
        let mut out = Reply::default();
        self.ioctl(MAP, bytes(&input), bytes_mut(&mut out))?;
        if out.generation != self.caps.generation
            || out.mapping == 0
            || out.address == 0
            || out.length != length
        {
            // Do not invent an unmap id from a malformed reply. The kernel
            // retains its actual mapping until this process exits or service
            // recovery reconciles authoritative records.
            return Err(io::Error::other("invalid protected BAR mapping reply"));
        }
        Ok(Mapping {
            device: Arc::clone(self),
            _process: Arc::clone(&writer.process),
            reply: out,
        })
    }
    fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<()> {
        let mut got = 0;
        // SAFETY: retained synchronous original device handle and valid fixed
        // ABI buffers, kept live for this synchronous buffered IOCTL call.
        if unsafe {
            DeviceIoControl(
                self.original.0,
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if got as usize != output.len() {
            return Err(io::Error::other("protected BAR reply size differs"));
        }
        Ok(())
    }
}
/// Broker-owned mapping record. Dropping it never means producer completion.
/// The driver independently retains the target and releases its mapping only
/// on privileged revoke or kernel-confirmed termination.
pub struct Mapping {
    device: Arc<Device>,
    _process: Arc<Owned>,
    reply: Reply,
}
impl Mapping {
    pub fn address(&self) -> u64 {
        self.reply.address
    }
    pub fn length(&self) -> u64 {
        self.reply.length
    }
    /// Privileged revocation after trusted local unmap or remote completion.
    /// The allocation's holds may be changed only after this succeeds.
    pub fn revoke(&self) -> io::Result<()> {
        let input = Release {
            version: VERSION,
            reserved: 0,
            generation: self.reply.generation,
            mapping: self.reply.mapping,
        };
        self.device.ioctl(UNMAP, bytes(&input), &mut [])
    }
}
fn bytes<T>(value: &T) -> &[u8] {
    // SAFETY: every caller uses only padding-free fixed integer ABI structs.
    unsafe { std::slice::from_raw_parts((value as *const T).cast(), std::mem::size_of::<T>()) }
}
fn bytes_mut<T>(value: &mut T) -> &mut [u8] {
    // SAFETY: as above, output is entirely initialized by the driver.
    unsafe { std::slice::from_raw_parts_mut((value as *mut T).cast(), std::mem::size_of::<T>()) }
}
