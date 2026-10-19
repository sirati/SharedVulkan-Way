//! §103.13. IOSurfaces and the mach ports that carry them between the macOS
//! main process and its faces.
//!
//! Measured on the Sonoma VM before this was written: an IOSurface's mach port
//! (`IOSurfaceCreateMachPort`) sent through a name the main process checks in
//! with its bootstrap server (`bootstrap_check_in` of an undeclared name works
//! in the per-user GUI namespace) is found again with
//! `IOSurfaceLookupFromMachPort`, and the two processes see the same pixels.
//! The deprecated global lookup by ID also worked, and is not used, because any
//! process that guesses a 32-bit ID could map a window's pixels; a mach port is
//! a capability only the face it was sent to holds, and the bootstrap name
//! exists only in this user's GUI session.

#![allow(non_upper_case_globals)]

use std::ffi::{c_void, CString};

type Port = u32;
type Kr = i32;
type CFTypeRef = *const c_void;

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
    /// `pad2:16, disposition:8, type:8`.
    bits: u32,
}

#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct HelloMsg {
    h: Header,
    count: u32,
    port: PortDesc,
    token: u64,
}

#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct SurfaceMsg {
    h: Header,
    count: u32,
    ports: [PortDesc; 2],
    id: u64,
    generation: u32,
}

const MOVE_SEND: u32 = 17;
const COPY_SEND: u32 = 19;
const MAKE_SEND: u32 = 20;
const BITS_COMPLEX: u32 = 0x8000_0000;
const SEND_MSG: i32 = 1;
const RCV_MSG: i32 = 2;
const SEND_TIMEOUT: i32 = 0x10;
const RCV_TIMEOUT: i32 = 0x100;
const RIGHT_RECEIVE: u32 = 1;

unsafe extern "C" {
    static bootstrap_port: Port;
    static mach_task_self_: Port;
    fn bootstrap_check_in(bp: Port, name: *const i8, sp: *mut Port) -> Kr;
    fn bootstrap_look_up(bp: Port, name: *const i8, sp: *mut Port) -> Kr;
    fn mach_port_allocate(task: Port, right: u32, name: *mut Port) -> Kr;
    fn mach_port_deallocate(task: Port, name: Port) -> Kr;
    fn mach_msg(msg: *mut c_void, option: i32, send: u32, rcv: u32, port: Port, timeout: u32, notify: Port) -> Kr;
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceCreate(props: CFTypeRef) -> *mut c_void;
    fn IOSurfaceCreateMachPort(s: *mut c_void) -> Port;
    fn IOSurfaceLookupFromMachPort(p: Port) -> *mut c_void;
    fn IOSurfaceLock(s: *mut c_void, options: u32, seed: *mut u32) -> Kr;
    fn IOSurfaceUnlock(s: *mut c_void, options: u32, seed: *mut u32) -> Kr;
    fn IOSurfaceGetBaseAddress(s: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(s: *mut c_void) -> usize;
    fn IOSurfaceGetWidth(s: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(s: *mut c_void) -> usize;
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
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

fn task() -> Port {
    // SAFETY: a process-lifetime global the mach runtime initialises.
    unsafe { mach_task_self_ }
}

/// One IOSurface, BGRA, owned (+1) by this value.
pub struct Surface(*mut c_void);

// SAFETY: IOSurfaceRef is a thread-safe CF object; access to its pixels is
// bracketed by IOSurfaceLock.
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: the +1 reference this value owns.
        unsafe { CFRelease(self.0) };
    }
}

impl Surface {
    pub fn new(w: i32, h: i32) -> Option<Surface> {
        if w <= 0 || h <= 0 {
            return None;
        }
        let num = |v: i32| -> CFTypeRef {
            // SAFETY: kCFNumberSInt32Type (3) from a live i32.
            unsafe { CFNumberCreate(std::ptr::null(), 3, (&raw const v).cast()) }
        };
        // SAFETY: a CF dictionary with CF callbacks, filled with CF numbers it
        // retains; our references are released after.
        unsafe {
            let d = CFDictionaryCreateMutable(
                std::ptr::null(),
                0,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            let vals = [num(w), num(h), num(4), num(i32::from_be_bytes(*b"BGRA"))];
            for (k, v) in [kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement, kIOSurfacePixelFormat]
                .into_iter()
                .zip(vals)
            {
                CFDictionarySetValue(d, k, v);
                CFRelease(v);
            }
            let s = IOSurfaceCreate(d);
            CFRelease(d);
            (!s.is_null()).then_some(Surface(s))
        }
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    pub fn size(&self) -> (i32, i32) {
        // SAFETY: a live surface.
        unsafe { (IOSurfaceGetWidth(self.0) as i32, IOSurfaceGetHeight(self.0) as i32) }
    }

    /// Copy tightly packed BGRA rows (`w * 4` stride) into the surface.
    pub fn copy_from(&self, src: &[u8], w: i32, h: i32) {
        let (sw, sh) = self.size();
        let (w, h) = (w.min(sw).max(0) as usize, h.min(sh).max(0) as usize);
        if src.len() < w * h * 4 {
            return;
        }
        // SAFETY: the surface is locked for the copy; rows are bounded by the
        // surface's own size and stride and by `src`'s length.
        unsafe {
            IOSurfaceLock(self.0, 0, std::ptr::null_mut());
            let base = IOSurfaceGetBaseAddress(self.0).cast::<u8>();
            let bpr = IOSurfaceGetBytesPerRow(self.0);
            for y in 0..h {
                std::ptr::copy_nonoverlapping(src.as_ptr().add(y * w * 4), base.add(y * bpr), w * 4);
            }
            IOSurfaceUnlock(self.0, 0, std::ptr::null_mut());
        }
    }
}

/// Check in (or create) the main process's service. `Err` if another main
/// process of this user's session already holds it.
pub fn check_in(name: &str) -> Result<Port, Kr> {
    let c = CString::new(name).expect("name");
    let mut p: Port = 0;
    // SAFETY: valid name and out-parameter.
    let kr = unsafe { bootstrap_check_in(bootstrap_port, c.as_ptr(), &mut p) };
    if kr == 0 { Ok(p) } else { Err(kr) }
}

pub fn look_up(name: &str) -> Result<Port, Kr> {
    let c = CString::new(name).expect("name");
    let mut p: Port = 0;
    // SAFETY: valid name and out-parameter.
    let kr = unsafe { bootstrap_look_up(bootstrap_port, c.as_ptr(), &mut p) };
    if kr == 0 { Ok(p) } else { Err(kr) }
}

/// A fresh receive right.
pub fn receive_port() -> Option<Port> {
    let mut p: Port = 0;
    // SAFETY: out-parameter.
    (unsafe { mach_port_allocate(task(), RIGHT_RECEIVE, &mut p) } == 0).then_some(p)
}

/// Drop a send right (or the receive right, for a receive port).
pub fn release(p: Port) {
    // SAFETY: a right this process holds; errors are harmless here.
    unsafe {
        mach_port_deallocate(task(), p);
    }
}

fn desc(name: Port, disposition: u32) -> PortDesc {
    PortDesc { name, pad1: 0, bits: (disposition << 16) /* | MACH_MSG_PORT_DESCRIPTOR(0) << 24 */ }
}

/// Face: tell the main process's service which port to send surfaces to.
pub fn send_hello(service: Port, mine: Port, token: u64) -> Kr {
    let mut m = HelloMsg {
        h: Header { bits: COPY_SEND | BITS_COMPLEX, size: std::mem::size_of::<HelloMsg>() as u32, remote: service, ..Default::default() },
        count: 1,
        port: desc(mine, MAKE_SEND),
        token,
    };
    // SAFETY: a correctly sized, initialised message.
    unsafe { mach_msg((&raw mut m).cast(), SEND_MSG | SEND_TIMEOUT, m.h.size, 0, 0, 2000, 0) }
}

/// Main: the next hello, as `(token, send right to the face's port)`.
pub fn recv_hello(service: Port, timeout_ms: u32) -> Option<(u64, Port)> {
    let mut buf = [0u8; 256];
    // SAFETY: a receive into a buffer larger than the message plus trailer.
    let kr = unsafe {
        mach_msg(buf.as_mut_ptr().cast(), RCV_MSG | RCV_TIMEOUT, 0, buf.len() as u32, service, timeout_ms, 0)
    };
    if kr != 0 {
        return None;
    }
    // SAFETY: the kernel wrote at least a header; the size is checked first.
    let h: Header = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    if (h.size as usize) < std::mem::size_of::<HelloMsg>() || h.bits & BITS_COMPLEX == 0 {
        return None;
    }
    let m: HelloMsg = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    Some((m.token, m.port.name))
}

/// Main: send a window's two surfaces to a face. The ports are moved.
pub fn send_surfaces(face: Port, id: u64, generation: u32, s: [&Surface; 2]) -> Kr {
    // SAFETY: live surfaces; each call makes a send right this message moves.
    let ports = unsafe { [IOSurfaceCreateMachPort(s[0].as_ptr()), IOSurfaceCreateMachPort(s[1].as_ptr())] };
    let mut m = SurfaceMsg {
        h: Header { bits: COPY_SEND | BITS_COMPLEX, size: std::mem::size_of::<SurfaceMsg>() as u32, remote: face, ..Default::default() },
        count: 2,
        ports: [desc(ports[0], MOVE_SEND), desc(ports[1], MOVE_SEND)],
        id,
        generation,
    };
    // SAFETY: a correctly sized, initialised message.
    let kr = unsafe { mach_msg((&raw mut m).cast(), SEND_MSG | SEND_TIMEOUT, m.h.size, 0, 0, 2000, 0) };
    if kr != 0 {
        release(ports[0]);
        release(ports[1]);
    }
    kr
}

/// Face: the next surface pair that arrived, as `(id, generation, surfaces)`.
pub fn recv_surfaces(port: Port, timeout_ms: u32) -> Option<(u64, u32, [Surface; 2])> {
    let mut buf = [0u8; 256];
    // SAFETY: as in `recv_hello`.
    let kr = unsafe {
        mach_msg(buf.as_mut_ptr().cast(), RCV_MSG | RCV_TIMEOUT, 0, buf.len() as u32, port, timeout_ms, 0)
    };
    if kr != 0 {
        return None;
    }
    let h: Header = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    if (h.size as usize) < std::mem::size_of::<SurfaceMsg>() || h.bits & BITS_COMPLEX == 0 {
        return None;
    }
    let m: SurfaceMsg = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    let look = |p: Port| {
        // SAFETY: a send right this message delivered; released after lookup.
        let s = unsafe { IOSurfaceLookupFromMachPort(p) };
        release(p);
        (!s.is_null()).then_some(Surface(s))
    };
    let ports = m.ports;
    let (a, b) = (look(ports[0].name)?, look(ports[1].name)?);
    Some((m.id, m.generation, [a, b]))
}
