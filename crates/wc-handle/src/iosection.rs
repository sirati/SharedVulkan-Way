//! §103.19: shared sections as IOSurfaces, on a Mac hosting the Linux VM.
//!
//! On macOS the GPU (KosmicKrisp, Vulkan on Metal) can render into an image
//! only in GPU memory, and the one way to give such memory to another process
//! without copying it is an IOSurface. So a §104 section the GPU process makes
//! for an exportable image is an IOSurface it renders into, and the compositor
//! (the main process) gets it as a mach port: `send` here, `serve` there. A
//! port is a capability only the receiving process holds; the IOSurface's
//! global id is never used, since any process guessing it could read a
//! window's pixels (macmach.rs measured the same and chose the same).
//!
//! The section's name (`ios.<pid>.<n>.<hex>`) is what the app side's token
//! carries, as on the other platforms; `map_section` finds the IOSurface by it
//! in this process's table.

#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::sync::{Mutex, OnceLock};
use crate::iosection_policy::{self as policy, Message, Registry, Section, Sender};
pub use crate::iosection_policy::Owner;

/// Strict metadata validation for typed NV12 registration. Ordinary sections
/// and CPU mapping retain their existing nonplanar contract.
#[path = "iosection_nv12_policy.rs"]
pub mod nv12_geometry;

type Port = u32;
type Kr = i32;
type CFTypeRef = *const c_void;

/// The prefix every IOSurface section's name has.
pub const PREFIX: &str = "ios.";
/// The longest name a message carries.
const NAME_MAX: usize = policy::NAME_MAX;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Header {
    bits: u32,
    size: u32,
    remote: Port,
    local: Port,
    voucher: Port,
    id: i32,
}

#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct PortDesc {
    name: Port,
    pad1: u32,
    bits: u32,
}

/// A section that now exists: its name and its IOSurface.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct AddMsg {
    h: Header,
    count: u32,
    surface: PortDesc,
    name_len: u32,
    name: [u8; NAME_MAX],
    owner: Owner,
}

/// A section the GPU process no longer holds.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct GoneMsg {
    h: Header,
    name_len: u32,
    name: [u8; NAME_MAX],
    owner: Owner,
}

const ID_ADD: i32 = policy::ADD;
const ID_ADD_NV12: i32 = policy::ADD_NV12;
const ID_GONE: i32 = policy::GONE;
const MOVE_SEND: u32 = 17;
const COPY_SEND: u32 = 19;
const BITS_COMPLEX: u32 = 0x8000_0000;
const SEND_MSG: i32 = 1;
const RCV_MSG: i32 = 2;
const SEND_TIMEOUT: i32 = 0x10;
const RCV_TIMEOUT: i32 = 0x100;
const MAKE_SEND_ONCE: u32 = 21;
const MOVE_SEND_ONCE: u32 = 18;
const AUDIT_TRAILER: i32 = 3 << 24;
#[repr(C, align(8))]
struct Receive([u8; 512]);
#[repr(C)]
struct Ack { h: Header, status: i32 }
struct MessageGuard(*mut c_void);
impl Drop for MessageGuard { fn drop(&mut self) { unsafe { mach_msg_destroy(self.0) }; } }
struct ReceiveRight(Port);
impl Drop for ReceiveRight {
    fn drop(&mut self) {
        // SAFETY: exactly the receive right allocated by send().
        unsafe { mach_port_mod_refs(mach_task_self_, self.0, 1, -1) };
    }
}

unsafe extern "C" {
    static bootstrap_port: Port;
    static mach_task_self_: Port;
    fn bootstrap_check_in(bp: Port, name: *const i8, sp: *mut Port) -> Kr;
    fn bootstrap_look_up(bp: Port, name: *const i8, sp: *mut Port) -> Kr;
    fn mach_msg_destroy(msg: *mut c_void);
    fn mach_port_allocate(task: Port, right: u32, name: *mut Port) -> Kr;
    fn mach_port_insert_right(task: Port, name: Port, poly: Port, disposition: u32) -> Kr;
    fn mach_port_mod_refs(task: Port, name: Port, right: u32, delta: i32) -> Kr;
    fn mach_msg(msg: *mut c_void, option: i32, send: u32, rcv: u32, port: Port, timeout: u32, notify: Port) -> Kr;
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceCreate(props: CFTypeRef) -> *mut c_void;
    fn IOSurfaceCreateMachPort(s: *mut c_void) -> Port;
    fn IOSurfaceLookupFromMachPort(p: Port) -> *mut c_void;
    fn IOSurfaceLock(s: *mut c_void, flags: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(s: *mut c_void, flags: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBytesPerElement(s: *mut c_void) -> usize;
    fn IOSurfaceGetPlaneCount(s: *mut c_void) -> usize;
    fn IOSurfaceGetWidthOfPlane(s: *mut c_void, plane: usize) -> usize;
    fn IOSurfaceGetHeightOfPlane(s: *mut c_void, plane: usize) -> usize;
    fn IOSurfaceGetBytesPerElementOfPlane(s: *mut c_void, plane: usize) -> usize;
    fn IOSurfaceGetBytesPerRowOfPlane(s: *mut c_void, plane: usize) -> usize;
    fn IOSurfaceGetOffsetOfPlane(s: *mut c_void, plane: usize) -> usize;
    fn IOSurfaceGetPixelFormat(s: *mut c_void) -> u32;
    fn IOSurfaceGetWidth(s: *mut c_void) -> usize;
    fn IOSurfaceGetBaseAddress(s: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(s: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(s: *mut c_void) -> usize;
    fn IOSurfaceGetAllocSize(s: *mut c_void) -> usize;
    static kIOSurfaceWidth: CFTypeRef;
    static kIOSurfaceHeight: CFTypeRef;
    static kIOSurfaceBytesPerElement: CFTypeRef;
    static kIOSurfacePixelFormat: CFTypeRef;
    static kIOSurfaceBytesPerRow: CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryCreateMutable(a: CFTypeRef, cap: isize, k: *const c_void, v: *const c_void) -> *mut c_void;
    fn CFDictionarySetValue(d: *mut c_void, k: CFTypeRef, v: CFTypeRef);
    fn CFNumberCreate(a: CFTypeRef, t: isize, v: *const c_void) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

/// One IOSurface, owned (+1) by this value.
pub struct Surface(*mut c_void);

// SAFETY: IOSurfaceRef is a thread-safe CF object.
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    /// # Safety
    /// `surface` is a live IOSurfaceRef. The returned owner retains it.
    pub unsafe fn retain_raw(surface: *mut c_void) -> Surface {
        assert!(!surface.is_null());
        unsafe { CFRetain(surface) };
        Surface(surface)
    }

    /// Read a completed immutable compositor snapshot only on shadow fallback.
    /// # Safety
    /// GPU producers must have completed and no writer may mutate the surface
    /// during the closure.
    pub unsafe fn read_snapshot<R>(&self, f: impl FnOnce(&[u8], usize, usize, usize) -> R) -> Option<R> {
        struct Locked(*mut c_void);
        impl Drop for Locked { fn drop(&mut self) { unsafe { IOSurfaceUnlock(self.0, 1, std::ptr::null_mut()) }; } }
        // SAFETY: a live surface; the read lock and reference cover the closure.
        unsafe {
            if IOSurfaceLock(self.0, 1, std::ptr::null_mut()) != 0 { return None; }
            let _lock = Locked(self.0);
            let ptr = IOSurfaceGetBaseAddress(self.0).cast::<u8>();
            if ptr.is_null() || !self.valid_geometry() { return None; }
            Some(f(std::slice::from_raw_parts(ptr, self.len()),
                IOSurfaceGetWidth(self.0), IOSurfaceGetHeight(self.0), IOSurfaceGetBytesPerRow(self.0)))
        }
    }
    /// A new IOSurface of `width`×`height` elements of `bytes_per_element`,
    /// `bytes_per_row` apart, pixel format `fourcc` (e.g. `'BGRA'`). The pitch is
    /// the caller's, so that the app side can be told it before the surface exists.
    pub fn new(width: u32, height: u32, bytes_per_element: u32, bytes_per_row: u32, fourcc: u32) -> Option<Surface> {
        let dims = [i32::try_from(width).ok()?, i32::try_from(height).ok()?,
            i32::try_from(bytes_per_element).ok()?, i32::try_from(bytes_per_row).ok()?];
        let row = (width as usize).checked_mul(bytes_per_element as usize)?;
        if width == 0 || height == 0 || bytes_per_element == 0 || (bytes_per_row as usize) < row { return None; }
        let needed = (bytes_per_row as usize).checked_mul(height as usize - 1)?.checked_add(row)?;
        if needed > isize::MAX as usize { return None; }
        // SAFETY: CF calls with valid arguments; every object made here is
        // released here except the surface, which this value owns.
        unsafe {
            let d = CFDictionaryCreateMutable(
                std::ptr::null(),
                0,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            );
            if d.is_null() {
                return None;
            }
            let values = [dims[0], dims[1], dims[2], dims[3], fourcc as i32];
            for (k, v) in [kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfacePixelFormat]
                .into_iter()
                .zip(values)
            {
                // kCFNumberSInt32Type
                let n = CFNumberCreate(std::ptr::null(), 3, (&raw const v).cast());
                if n.is_null() { CFRelease(d); return None; }
                CFDictionarySetValue(d, k, n);
                CFRelease(n);
            }
            let s = IOSurfaceCreate(d);
            CFRelease(d);
            if s.is_null() { return None; }
            let surface = Surface(s);
            if !surface.valid_geometry() || IOSurfaceGetWidth(s) != width as usize
                || IOSurfaceGetHeight(s) != height as usize
                || IOSurfaceGetBytesPerRow(s) != bytes_per_row as usize
                || IOSurfaceGetBytesPerElement(s) != bytes_per_element as usize { return None; }
            Some(surface)
        }
    }

    /// The `IOSurfaceRef`, still owned by this value.
    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Another reference to the same surface.
    pub fn retain(&self) -> Surface {
        // SAFETY: a live surface; the +1 is the returned value's.
        unsafe { CFRetain(self.0) };
        Surface(self.0)
    }

    pub fn width(&self) -> usize { unsafe { IOSurfaceGetWidth(self.0) } }
    pub fn height(&self) -> usize { unsafe { IOSurfaceGetHeight(self.0) } }
    pub fn bytes_per_element(&self) -> usize { unsafe { IOSurfaceGetBytesPerElement(self.0) } }
    pub fn pixel_format(&self) -> u32 { unsafe { IOSurfaceGetPixelFormat(self.0) } }
    pub fn plane_count(&self) -> usize { unsafe { IOSurfaceGetPlaneCount(self.0) } }

    /// Read original plane metadata without locking or accessing pixel storage.
    pub fn plane_metadata(&self, plane: usize) -> Option<nv12_geometry::Plane> {
        if plane >= self.plane_count() { return None; }
        // SAFETY: this owner retains the live IOSurface and the index is bounded.
        Some(unsafe { nv12_geometry::Plane {
            width: IOSurfaceGetWidthOfPlane(self.0, plane),
            height: IOSurfaceGetHeightOfPlane(self.0, plane),
            bytes_per_element: IOSurfaceGetBytesPerElementOfPlane(self.0, plane),
            bytes_per_row: IOSurfaceGetBytesPerRowOfPlane(self.0, plane),
            offset: IOSurfaceGetOffsetOfPlane(self.0, plane),
        } })
    }
    /// Validate metadata for the original joined 420v target. This does not
    /// authorize broker publication or relax the existing nonplanar protocol.
    pub fn nv12_geometry(&self) -> Result<nv12_geometry::Nv12Geometry, nv12_geometry::Rejected> {
        let plane_count = self.plane_count();
        if plane_count != 2 { return Err(nv12_geometry::Rejected::PlaneCount); }
        let y = self.plane_metadata(0).ok_or(nv12_geometry::Rejected::PlaneCount)?;
        let uv = self.plane_metadata(1).ok_or(nv12_geometry::Rejected::PlaneCount)?;
        nv12_geometry::Nv12Geometry::validate(nv12_geometry::Surface {
            width: self.width(), height: self.height(), pixel_format: self.pixel_format(),
            allocation_size: self.len(), plane_count, planes: [y, uv],
        })
    }

    pub fn bytes_per_row(&self) -> usize {
        // SAFETY: a live surface.
        unsafe { IOSurfaceGetBytesPerRow(self.0) }
    }

    /// Bytes from the base address the CPU may read: the whole allocation.
    pub fn len(&self) -> usize {
        // SAFETY: a live surface.
        unsafe { IOSurfaceGetAllocSize(self.0) }
    }

    fn valid_geometry(&self) -> bool {
        // This section protocol currently carries non-planar surfaces only.
        unsafe { IOSurfaceGetPlaneCount(self.0) == 0 && policy::geometry(
            IOSurfaceGetWidth(self.0), IOSurfaceGetHeight(self.0),
            IOSurfaceGetBytesPerElement(self.0), IOSurfaceGetBytesPerRow(self.0), self.len()).is_some() }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn base(&self) -> *const u8 {
        // SAFETY: a live surface; the address stays valid while it does.
        unsafe { IOSurfaceGetBaseAddress(self.0).cast::<u8>().cast_const() }
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IOSurface({:p})", self.0)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: the +1 this value owns.
        unsafe { CFRelease(self.0) };
    }
}

/// Native names are retained only for a live broker connection generation.
#[derive(Default)]
struct Table { registry: Registry<Section<Surface>>, identities: HashMap<Owner, Sender> }
static TABLE: Mutex<Option<Table>> = Mutex::new(None);
fn table<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    let mut g = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Table::default))
}

/// The caller binds a kernel-audited sender to the shared broker and starts
/// observing its connection generation before returning success. Revocations
/// call revoke_owner(), including broker disconnect. No keeper is created here.
pub type OwnerObserver = Box<dyn Fn(Owner, Sender) -> std::io::Result<()> + Send + Sync>;
static OBSERVER: OnceLock<OwnerObserver> = OnceLock::new();
pub fn set_owner_observer(observer: OwnerObserver) -> bool { OBSERVER.set(observer).is_ok() }
pub fn revoke_owner(owner: Owner) {
    table(|t| { t.registry.revoke(owner); t.identities.remove(&owner); });
}
fn register(owner: Owner, sender: Sender, name: String, surface: Section<Surface>) -> bool {
    table(|t| {
        if let Some(known) = t.identities.get(&owner) {
            if *known != sender { return false; }
        } else {
            let Some(observe) = OBSERVER.get() else { return false };
            // Hold the table lock across binding. A concurrent revocation can
            // only run after insertion, so death racing ADD cannot resurrect it.
            if observe(owner, sender).is_err() { return false; }
            t.identities.insert(owner, sender);
            t.registry.add_owner(owner);
        }
        t.registry.insert(owner, name, surface)
    })
}

/// Retain an imported GPU section without mapping or reading its pixels.
pub fn surface(name: &str) -> Option<Surface> {
    table(|t| t.registry.get(name).and_then(Section::ordinary).map(Surface::retain))
}
/// Retain only an explicitly registered NV12 section. This does not prove
/// producer completion, grant CPU access, or retire any producer/consumer job.
pub fn nv12_surface(name: &str) -> Option<Surface> {
    table(|t| t.registry.get(name).and_then(Section::nv12).map(Surface::retain))
}
pub(crate) fn lookup(name: &str) -> Option<(Surface, *const u8, usize)> {
    table(|t| {
        let s = t.registry.get(name)?.ordinary()?;
        if !s.valid_geometry() || s.base().is_null() { return None; }
        Some((s.retain(), s.base(), s.len()))
    })
}

/// Check in the compositor's service (an undeclared name in this user's GUI
/// bootstrap namespace, as macmach.rs does for the faces).
pub fn check_in(name: &str) -> Result<Port, Kr> {
    let c = CString::new(name).map_err(|_| -1)?;
    let mut p: Port = 0;
    // SAFETY: valid name and out-parameter.
    let kr = unsafe { bootstrap_check_in(bootstrap_port, c.as_ptr(), &mut p) };
    if kr == 0 { Ok(p) } else { Err(kr) }
}

/// The GPU process: the compositor's service by name.
pub fn look_up(name: &str) -> Result<Port, Kr> {
    let c = CString::new(name).map_err(|_| -1)?;
    let mut p: Port = 0;
    // SAFETY: valid name and out-parameter.
    let kr = unsafe { bootstrap_look_up(bootstrap_port, c.as_ptr(), &mut p) };
    if kr == 0 { Ok(p) } else { Err(kr) }
}

fn name_field(name: &str) -> Option<(u32, [u8; NAME_MAX])> {
    let b = name.as_bytes();
    if !policy::valid_name(b) { return None; }
    let mut out = [0u8; NAME_MAX]; out[..b.len()].copy_from_slice(b);
    Some((b.len() as u32, out))
}

/// Register a section under a live shared-broker connection generation. The
/// synchronous acknowledgment prevents an exported token from outrunning ADD.
pub fn send(service: Port, owner: Owner, name: &str, surface: &Surface) -> Kr {
    if !surface.valid_geometry() { return -1; }
    send_add(service, owner, name, surface, ID_ADD)
}

/// Register the exact retained two-plane 64x64 420v surface under the same
/// audited owner protocol. The acknowledgment proves registration only;
/// callers must separately retain/synchronize all GPU producers and readers.
pub fn send_nv12(service: Port, owner: Owner, name: &str, surface: &Surface) -> Kr {
    if surface.nv12_geometry().is_err() { return -1; }
    send_add(service, owner, name, surface, ID_ADD_NV12)
}
fn send_add(service: Port, owner: Owner, name: &str, surface: &Surface, id: i32) -> Kr {
    let Some((name_len, name)) = name_field(name) else { return -1 };
    let mut reply = 0;
    let kr = unsafe { mach_port_allocate(mach_task_self_, 1, &mut reply) };
    if kr != 0 { return kr; }
    let reply = ReceiveRight(reply);
    let port = unsafe { IOSurfaceCreateMachPort(surface.0) };
    if port == 0 { return -1; }
    let mut m = AddMsg {
        h: Header { bits: COPY_SEND | (MAKE_SEND_ONCE << 8) | BITS_COMPLEX,
            size: std::mem::size_of::<AddMsg>() as u32, remote: service, local: reply.0,
            id, ..Default::default() },
        count: 1, surface: PortDesc { name: port, pad1: 0, bits: MOVE_SEND << 16 },
        name_len, name, owner,
    };
    let kr = unsafe { mach_msg((&raw mut m).cast(), SEND_MSG | SEND_TIMEOUT, m.h.size, 0, 0, 2000, 0) };
    if kr != 0 {
        // Preserve the borrowed destination and our receive right. Destroy any
        // returned/moved descriptor right using the kernel-updated message.
        m.h.remote = 0; m.h.local = 0;
        unsafe { mach_msg_destroy((&raw mut m).cast()) };
        return kr;
    }
    let mut bytes = Receive([0; 512]);
    let kr = unsafe { mach_msg(bytes.0.as_mut_ptr().cast(), RCV_MSG | RCV_TIMEOUT,
        0, bytes.0.len() as u32, reply.0, 2000, 0) };
    if kr != 0 { return kr; }
    let _owned = MessageGuard(bytes.0.as_mut_ptr().cast());
    let h: Header = unsafe { std::ptr::read_unaligned(bytes.0.as_ptr().cast()) };
    if h.id != policy::ACK || h.size != std::mem::size_of::<Ack>() as u32
        || h.bits != MOVE_SEND_ONCE << 8 || h.remote != 0 || h.local != reply.0 { return -1; }
    unsafe { std::ptr::read_unaligned(bytes.0.as_ptr().add(24).cast::<i32>()) }
}

pub fn send_gone(service: Port, owner: Owner, name: &str) -> Kr {
    let Some((name_len, name)) = name_field(name) else { return -1 };
    let mut m = GoneMsg { h: Header { bits: COPY_SEND, size: std::mem::size_of::<GoneMsg>() as u32,
        remote: service, id: ID_GONE, ..Default::default() }, name_len, name, owner };
    unsafe { mach_msg((&raw mut m).cast(), SEND_MSG | SEND_TIMEOUT, m.h.size, 0, 0, 2000, 0) }
}

fn acknowledge(bytes: &mut Receive, status: i32) {
    let h: Header = unsafe { std::ptr::read_unaligned(bytes.0.as_ptr().cast()) };
    if h.remote == 0 || h.bits & 0xff != MOVE_SEND_ONCE { return; }
    // Transfer the reply right from the received-message guard to this outgoing
    // message. On failure, destroy the kernel-restored outgoing right exactly once.
    bytes.0[8..12].copy_from_slice(&0u32.to_ne_bytes());
    let mut ack = Ack { h: Header { bits: MOVE_SEND_ONCE, size: std::mem::size_of::<Ack>() as u32,
        remote: h.remote, id: policy::ACK, ..Default::default() }, status };
    let kr = unsafe { mach_msg((&raw mut ack).cast(), SEND_MSG | SEND_TIMEOUT,
        ack.h.size, 0, 0, 2000, 0) };
    if kr != 0 { unsafe { mach_msg_destroy((&raw mut ack).cast()) }; }
}

/// Allocate an unregistered private receive right in this task. The caller
/// keeps the returned name and may pass it directly to `serve` in the same
/// process; no bootstrap registration or launchd/global service is used.
#[cfg(target_os = "macos")]
pub fn private_service_port() -> Result<u32, Kr> {
    let mut port = 0;
    let kr = unsafe { mach_port_allocate(mach_task_self_, 1, &mut port) };
    if kr != 0 { return Err(kr); }
    /* Install a send right in this same task. The numeric name is only
     * meaningful with this capability; no bootstrap lookup is involved. */
    let kr = unsafe { mach_port_insert_right(mach_task_self_, port, port, MOVE_SEND) };
    if kr == 0 { Ok(port) } else { Err(kr) }
}

/// Receive kernel-validated Mach messages with audit trailers. Every successful
/// receive, accepted or rejected, destroys all unadopted rights. A persistent
/// receive error terminates this endpoint; it never busy-loops.
pub fn serve(service: Port) {
    const { assert!(std::mem::size_of::<AddMsg>() == policy::ADD_SIZE); }
    const { assert!(std::mem::size_of::<GoneMsg>() == policy::GONE_SIZE); }
    let mut bytes = Receive([0; 512]);
    loop {
        bytes.0.fill(0);
        let kr = unsafe { mach_msg(bytes.0.as_mut_ptr().cast(), RCV_MSG | AUDIT_TRAILER,
            0, bytes.0.len() as u32, service, 0, 0) };
        if kr != 0 {
            eprintln!("IOSurface endpoint receive failed: {kr}");
            table(|t| *t = Table::default());
            return;
        }
        let _owned = MessageGuard(bytes.0.as_mut_ptr().cast());
        let sender = policy::audit(&bytes.0).filter(|s| s.uid == unsafe { libc::geteuid() });
        let mut status = -1;
        if let Some(sender) = sender {
            match policy::parse(&bytes.0) {
                Some(Message::Add { name, port, owner }) => {
                    // Lookup creates an independent CF reference. The delivered
                    // Mach descriptor still belongs to the message guard.
                    let raw = unsafe { IOSurfaceLookupFromMachPort(port) };
                    if !raw.is_null() {
                        let s = Surface(raw);
                        if s.valid_geometry() && register(owner, sender, name, Section::Ordinary(s)) { status = 0; }
                    }
                }
                Some(Message::AddNv12 { name, port, owner }) => {
                    // The same delivered capability and broker-authenticated
                    // owner flow, with independent strict planar validation.
                    let raw = unsafe { IOSurfaceLookupFromMachPort(port) };
                    if !raw.is_null() {
                        let s = Surface(raw);
                        if s.nv12_geometry().is_ok()
                            && register(owner, sender, name, Section::Nv12(s)) { status = 0; }
                    }
                }
                Some(Message::Gone { name, owner }) => {
                    table(|t| {
                        if t.identities.get(&owner) == Some(&sender) { t.registry.remove(owner, &name); }
                    });
                    status = 0;
                }
                None => {}
            }
        }
        acknowledge(&mut bytes, status);
    }
}
