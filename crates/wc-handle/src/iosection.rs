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
use std::sync::Mutex;

type Port = u32;
type Kr = i32;
type CFTypeRef = *const c_void;

/// The prefix every IOSurface section's name has.
pub const PREFIX: &str = "ios.";
/// The longest name a message carries.
const NAME_MAX: usize = 64;

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
}

/// A section the GPU process no longer holds.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct GoneMsg {
    h: Header,
    name_len: u32,
    name: [u8; NAME_MAX],
}

const ID_ADD: i32 = 0x5153_0001;
const ID_GONE: i32 = 0x5153_0002;
const MOVE_SEND: u32 = 17;
const COPY_SEND: u32 = 19;
const BITS_COMPLEX: u32 = 0x8000_0000;
const SEND_MSG: i32 = 1;
const RCV_MSG: i32 = 2;
const SEND_TIMEOUT: i32 = 0x10;

unsafe extern "C" {
    static bootstrap_port: Port;
    static mach_task_self_: Port;
    fn bootstrap_check_in(bp: Port, name: *const i8, sp: *mut Port) -> Kr;
    fn bootstrap_look_up(bp: Port, name: *const i8, sp: *mut Port) -> Kr;
    fn mach_port_deallocate(task: Port, name: Port) -> Kr;
    fn mach_msg(msg: *mut c_void, option: i32, send: u32, rcv: u32, port: Port, timeout: u32, notify: Port) -> Kr;
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceCreate(props: CFTypeRef) -> *mut c_void;
    fn IOSurfaceCreateMachPort(s: *mut c_void) -> Port;
    fn IOSurfaceLookupFromMachPort(p: Port) -> *mut c_void;
    fn IOSurfaceGetBaseAddress(s: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(s: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(s: *mut c_void) -> usize;
    fn IOSurfaceGetAllocSize(s: *mut c_void) -> usize;
    static kIOSurfaceWidth: CFTypeRef;
    static kIOSurfaceHeight: CFTypeRef;
    static kIOSurfaceBytesPerElement: CFTypeRef;
    static kIOSurfacePixelFormat: CFTypeRef;
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

fn release_port(p: Port) {
    // SAFETY: a right this process holds; an error is harmless here.
    unsafe { mach_port_deallocate(mach_task_self_, p) };
}

/// One IOSurface, owned (+1) by this value.
pub struct Surface(*mut c_void);

// SAFETY: IOSurfaceRef is a thread-safe CF object.
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    /// A new IOSurface of `width`×`height` elements of `bytes_per_element`,
    /// pixel format `fourcc` (e.g. `'BGRA'`).
    pub fn new(width: u32, height: u32, bytes_per_element: u32, fourcc: u32) -> Option<Surface> {
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
            let values = [width as i32, height as i32, bytes_per_element as i32, fourcc as i32];
            for (k, v) in [kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement, kIOSurfacePixelFormat]
                .into_iter()
                .zip(values)
            {
                // kCFNumberSInt32Type
                let n = CFNumberCreate(std::ptr::null(), 3, (&raw const v).cast());
                CFDictionarySetValue(d, k, n);
                CFRelease(n);
            }
            let s = IOSurfaceCreate(d);
            CFRelease(d);
            (!s.is_null()).then_some(Surface(s))
        }
    }

    /// The `IOSurfaceRef`, still owned by this value.
    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    pub fn bytes_per_row(&self) -> usize {
        // SAFETY: a live surface.
        unsafe { IOSurfaceGetBytesPerRow(self.0) }
    }

    /// Bytes from the base address the CPU may read: the whole allocation.
    pub fn len(&self) -> usize {
        // SAFETY: a live surface.
        unsafe { IOSurfaceGetAllocSize(self.0).max(IOSurfaceGetBytesPerRow(self.0) * IOSurfaceGetHeight(self.0)) }
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

/// The compositor's table: every section a GPU process sent, by name.
static TABLE: Mutex<Option<HashMap<String, Surface>>> = Mutex::new(None);

fn table<R>(f: impl FnOnce(&mut HashMap<String, Surface>) -> R) -> R {
    let mut g = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// `map_section`'s view of a section by name: a reference of its own (so a
/// `gone` while it is mapped frees nothing under it), its base address and
/// its length.
pub(crate) fn lookup(name: &str) -> Option<(Surface, *const u8, usize)> {
    table(|t| {
        t.get(name).map(|s| {
            // SAFETY: a live surface; the +1 is the returned value's.
            unsafe { CFRetain(s.0) };
            (Surface(s.0), s.base(), s.len())
        })
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
    if !name.starts_with(PREFIX) || b.len() > NAME_MAX {
        return None;
    }
    let mut out = [0u8; NAME_MAX];
    out[..b.len()].copy_from_slice(b);
    Some((b.len() as u32, out))
}

/// The GPU process: tell the compositor's service that section `name` is
/// `surface`. Its port is moved with the message.
pub fn send(service: Port, name: &str, surface: &Surface) -> Kr {
    let Some((name_len, name)) = name_field(name) else { return -1 };
    // SAFETY: a live surface; the send right made here is moved by the message.
    let port = unsafe { IOSurfaceCreateMachPort(surface.0) };
    let mut m = AddMsg {
        h: Header {
            bits: COPY_SEND | BITS_COMPLEX,
            size: std::mem::size_of::<AddMsg>() as u32,
            remote: service,
            id: ID_ADD,
            ..Default::default()
        },
        count: 1,
        surface: PortDesc { name: port, pad1: 0, bits: MOVE_SEND << 16 },
        name_len,
        name,
    };
    // SAFETY: a correctly sized, initialised message.
    let kr = unsafe { mach_msg((&raw mut m).cast(), SEND_MSG | SEND_TIMEOUT, m.h.size, 0, 0, 2000, 0) };
    if kr != 0 {
        release_port(port);
    }
    kr
}

/// The GPU process: section `name` is no longer the GPU's; the compositor
/// drops it once nothing maps it.
pub fn send_gone(service: Port, name: &str) -> Kr {
    let Some((name_len, name)) = name_field(name) else { return -1 };
    let mut m = GoneMsg {
        h: Header { bits: COPY_SEND, size: std::mem::size_of::<GoneMsg>() as u32, remote: service, id: ID_GONE, ..Default::default() },
        name_len,
        name,
    };
    // SAFETY: a correctly sized, initialised message.
    unsafe { mach_msg((&raw mut m).cast(), SEND_MSG | SEND_TIMEOUT, m.h.size, 0, 0, 2000, 0) }
}

/// The compositor: receive sections on `service` for ever, keeping them in
/// the table `map_section` reads. Run on a thread of its own.
pub fn serve(service: Port) {
    let mut buf = [0u8; 512];
    loop {
        // SAFETY: a receive into a buffer larger than either message plus its trailer.
        let kr = unsafe { mach_msg(buf.as_mut_ptr().cast(), RCV_MSG, 0, buf.len() as u32, service, 0, 0) };
        if kr != 0 {
            continue;
        }
        // SAFETY: the kernel wrote at least a header; sizes are checked before each read.
        let h: Header = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
        let name_of = |len: u32, b: &[u8; NAME_MAX]| {
            let n = (len as usize).min(NAME_MAX);
            std::str::from_utf8(&b[..n]).ok().filter(|s| s.starts_with(PREFIX)).map(str::to_string)
        };
        if h.id == ID_ADD && h.bits & BITS_COMPLEX != 0 && h.size as usize >= std::mem::size_of::<AddMsg>() {
            let m: AddMsg = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
            let port = m.surface.name;
            // SAFETY: a send right this message delivered; released after the lookup.
            let s = unsafe { IOSurfaceLookupFromMachPort(port) };
            release_port(port);
            if let (Some(name), false) = (name_of(m.name_len, &m.name), s.is_null()) {
                table(|t| t.insert(name, Surface(s)));
            } else if !s.is_null() {
                // SAFETY: the +1 the lookup returned, unused.
                unsafe { CFRelease(s) };
            }
        } else if h.id == ID_GONE && h.size as usize >= std::mem::size_of::<GoneMsg>() {
            let m: GoneMsg = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
            if let Some(name) = name_of(m.name_len, &m.name) {
                table(|t| t.remove(&name));
            }
        }
    }
}
