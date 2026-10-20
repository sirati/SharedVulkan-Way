//! GPU composition into a fresh compositor-owned IOSurface.
//!
//! `waitUntilCompleted` is the source-retirement contract: Metal documents
//! completion as completion of all encoded GPU work. Both textures and the
//! source IOSurface remain owned until that wait returns, including GPU errors.
//! Only then may the caller acknowledge its source buffer. The output is never
//! rewritten or pooled. Core Animation retains the immutable output as its
//! contents, so display retirement cannot race reuse of the application's
//! allocation. This is a GPU blit, not direct CALayer scanout. No CPU pixels are
//! read. A future asynchronous implementation must preserve the same ordering.

use std::ffi::{c_char, c_void};
use super::{macmach::Surface, DirectFrame};

#[link(name = "Metal", kind = "framework")]
unsafe extern "C" { fn MTLCreateSystemDefaultDevice() -> *mut c_void; }
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}
#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetWidth(s: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(s: *mut c_void) -> usize;
    fn IOSurfaceGetBytesPerRow(s: *mut c_void) -> usize;
    fn IOSurfaceGetPixelFormat(s: *mut c_void) -> u32;
}

struct Pool(*mut c_void);
impl Drop for Pool { fn drop(&mut self) { unsafe { objc_autoreleasePoolPop(self.0) }; } }
struct Owned(*mut c_void);
impl Drop for Owned { fn drop(&mut self) { unsafe { send_void(self.0, c"release") }; } }

// Objective-C selectors below have the exact signatures used here.
unsafe fn send0(object: *mut c_void, name: &std::ffi::CStr) -> *mut c_void {
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { f(object, sel_registerName(name.as_ptr())) }
}
unsafe fn send_void(object: *mut c_void, name: &std::ffi::CStr) {
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { f(object, sel_registerName(name.as_ptr())) };
}
unsafe fn texture(device: *mut c_void, surface: *mut c_void, width: usize, height: usize) -> Option<Owned> {
    let make: unsafe extern "C" fn(*mut c_void, *mut c_void, usize, usize, usize, bool) -> *mut c_void =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    let desc = unsafe { make(objc_getClass(c"MTLTextureDescriptor".as_ptr()),
        sel_registerName(c"texture2DDescriptorWithPixelFormat:width:height:mipmapped:".as_ptr()),
        80, width, height, false) }; // BGRA8Unorm
    if desc.is_null() { return None; }
    let set: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // Shared storage and no shader usage: this texture is used only by a blit.
    unsafe { set(desc, sel_registerName(c"setStorageMode:".as_ptr()), 0) };
    let new: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, usize) -> *mut c_void =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    let t = unsafe { new(device, sel_registerName(c"newTextureWithDescriptor:iosurface:plane:".as_ptr()), desc, surface, 0) };
    (!t.is_null()).then_some(Owned(t))
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Origin { x: usize, y: usize, z: usize }
#[repr(C)]
struct Size { width: usize, height: usize, depth: usize }

/// Snapshot a complete BGRA IOSurface. The caller must first observe the
/// producing Vulkan fence. A failure leaves the source retained by `frame` and
/// must trigger the ordinary shadow fallback, not a successful presentation ack.
pub(super) fn snapshot(frame: &DirectFrame) -> Result<Surface, &'static str> {
    let (w, h) = (usize::try_from(frame.width).map_err(|_| "width")?,
                  usize::try_from(frame.height).map_err(|_| "height")?);
    if w == 0 || h == 0 || frame.offset != 0 { return Err("not a complete image"); }
    let src = frame.surface.as_ptr();
    // SAFETY: DirectFrame owns the source IOSurface for this whole function.
    if unsafe { IOSurfaceGetWidth(src) != w || IOSurfaceGetHeight(src) != h
        || IOSurfaceGetBytesPerRow(src) != frame.stride as usize
        || IOSurfaceGetPixelFormat(src) != u32::from_be_bytes(*b"BGRA") } {
        return Err("IOSurface format, dimensions or pitch disagree with the buffer");
    }
    let output = Surface::new(frame.width, frame.height).ok_or("output IOSurface")?;
    // SAFETY: all Objective-C calls use selectors and signatures from Metal's
    // public APIs. Owned references and this pool outlive the GPU completion.
    unsafe {
        let _pool = Pool(objc_autoreleasePoolPush());
        let dev = Owned(MTLCreateSystemDefaultDevice());
        if dev.0.is_null() { return Err("no Metal device"); }
        let queue = Owned(send0(dev.0, c"newCommandQueue"));
        if queue.0.is_null() { return Err("no Metal queue"); }
        let source = texture(dev.0, src, w, h).ok_or("source Metal texture")?;
        let dest = texture(dev.0, output.as_ptr(), w, h).ok_or("output Metal texture")?;
        let command = send0(queue.0, c"commandBuffer");
        if command.is_null() { return Err("no command buffer"); }
        let encoder = send0(command, c"blitCommandEncoder");
        if encoder.is_null() { return Err("no blit encoder"); }
        let copy: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, usize, usize,
            Origin, Size, *mut c_void, usize, usize, Origin) =
            std::mem::transmute(objc_msgSend as *const ());
        let zero = Origin { x: 0, y: 0, z: 0 };
        copy(encoder, sel_registerName(c"copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:".as_ptr()),
            source.0, 0, 0, zero, Size { width: w, height: h, depth: 1 }, dest.0, 0, 0, zero);
        send_void(encoder, c"endEncoding");
        send_void(command, c"commit");
        send_void(command, c"waitUntilCompleted");
        let status: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        if status(command, sel_registerName(c"status".as_ptr())) != 4 { return Err("Metal command failed"); }
    }
    Ok(output)
}

/// Headless acceptance probe. CPU access here creates and verifies a fixture;
/// the product snapshot path above never reads the source on the CPU. It checks
/// padding, completion before source reuse, and immutability of an older output.
pub fn probe_retirement() -> Result<(), &'static str> {
    use std::sync::Arc;
    #[link(name = "IOSurface", kind = "framework")]
    unsafe extern "C" {
        fn IOSurfaceLock(s: *mut c_void, flags: u32, seed: *mut u32) -> i32;
        fn IOSurfaceUnlock(s: *mut c_void, flags: u32, seed: *mut u32) -> i32;
        fn IOSurfaceGetBaseAddress(s: *mut c_void) -> *mut c_void;
    }
    // This guard also unlocks when a fixture assertion fails.
    struct Locked(*mut c_void, u32);
    impl Drop for Locked { fn drop(&mut self) { unsafe { IOSurfaceUnlock(self.0, self.1, std::ptr::null_mut()) }; } }
    fn access(surface: *mut c_void, flags: u32, mut pixel: impl FnMut(*mut u8)) -> Result<(), &'static str> {
        // SAFETY: both call sites own a valid, complete 7x3 BGRA IOSurface.
        unsafe {
            if IOSurfaceLock(surface, flags, std::ptr::null_mut()) != 0 { return Err("fixture lock"); }
            let _lock = Locked(surface, flags);
            let base = IOSurfaceGetBaseAddress(surface).cast::<u8>();
            if base.is_null() { return Err("fixture base"); }
            let stride = IOSurfaceGetBytesPerRow(surface);
            for y in 0..3 { for x in 0..7 { pixel(base.add(y * stride + x * 4)); } }
        }
        Ok(())
    }
    let source = wc_handle::iosection::Surface::new(7, 3, 4, 128, u32::from_be_bytes(*b"BGRA"))
        .ok_or("fixture surface")?;
    let frame = DirectFrame { key: 0, surface: Arc::new(source), offset: 0,
        width: 7, height: 3, stride: 128, format: crate::surface::PixelFormat::Argb8888 };
    let fill = |value| access(frame.surface.as_ptr(), 0, |p| unsafe {
        std::ptr::copy_nonoverlapping([value, 19, 47, 255].as_ptr(), p, 4);
    });
    let verify = |s: &Surface, value| {
        let mut correct = true;
        access(s.as_ptr(), 1, |p| unsafe { correct &= std::slice::from_raw_parts(p, 4) == [value, 19, 47, 255]; })?;
        if correct { Ok(()) } else { Err("GPU snapshot pixels changed or were incomplete") }
    };
    fill(23)?;
    let first = snapshot(&frame)?;
    fill(83)?; // Legal now: snapshot waited for the source's GPU reader.
    let second = snapshot(&frame)?;
    verify(&first, 23)?;
    verify(&second, 83)?;
    Ok(())
}
