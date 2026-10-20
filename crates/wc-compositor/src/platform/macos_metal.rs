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

use super::{DirectFrame, macmach::Surface};
use std::ffi::{c_char, c_void};

#[link(name = "Metal", kind = "framework")]
unsafe extern "C" {
    fn MTLCreateSystemDefaultDevice() -> *mut c_void;
}
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
impl Drop for Pool {
    fn drop(&mut self) {
        unsafe { objc_autoreleasePoolPop(self.0) };
    }
}
struct Owned(*mut c_void);
// Metal device, queue, pipeline and resource objects support cross-thread use.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { send_void(self.0, c"release") };
    }
}

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
unsafe fn texture(
    device: *mut c_void,
    surface: *mut c_void,
    width: usize,
    height: usize,
) -> Option<Owned> {
    let make: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        usize,
        usize,
        usize,
        bool,
    ) -> *mut c_void = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    let desc = unsafe {
        make(
            objc_getClass(c"MTLTextureDescriptor".as_ptr()),
            sel_registerName(
                c"texture2DDescriptorWithPixelFormat:width:height:mipmapped:".as_ptr(),
            ),
            80,
            width,
            height,
            false,
        )
    }; // BGRA8Unorm
    if desc.is_null() {
        return None;
    }
    let set: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // Sources and outputs are also used by the tree composition kernel.
    unsafe { set(desc, sel_registerName(c"setStorageMode:".as_ptr()), 0) };
    unsafe { set(desc, sel_registerName(c"setUsage:".as_ptr()), 1 | 2) };
    let new: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *mut c_void,
        usize,
    ) -> *mut c_void = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    let t = unsafe {
        new(
            device,
            sel_registerName(c"newTextureWithDescriptor:iosurface:plane:".as_ptr()),
            desc,
            surface,
            0,
        )
    };
    (!t.is_null()).then_some(Owned(t))
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Origin {
    x: usize,
    y: usize,
    z: usize,
}
#[repr(C)]
struct Size {
    width: usize,
    height: usize,
    depth: usize,
}

/// Snapshot a complete BGRA IOSurface. The caller must first observe the
/// producing Vulkan fence. A failure leaves the source retained by `frame` and
/// must trigger the ordinary shadow fallback, not a successful presentation ack.
pub(super) fn snapshot(frame: &DirectFrame) -> Result<Surface, &'static str> {
    let (w, h) = (
        usize::try_from(frame.width).map_err(|_| "width")?,
        usize::try_from(frame.height).map_err(|_| "height")?,
    );
    if w == 0 || h == 0 || frame.offset != 0 {
        return Err("not a complete image");
    }
    let src = frame.surface.as_ptr();
    // SAFETY: DirectFrame owns the source IOSurface for this whole function.
    if unsafe {
        IOSurfaceGetWidth(src) != w
            || IOSurfaceGetHeight(src) != h
            || IOSurfaceGetBytesPerRow(src) != frame.stride as usize
            || IOSurfaceGetPixelFormat(src) != u32::from_be_bytes(*b"BGRA")
    } {
        return Err("IOSurface format, dimensions or pitch disagree with the buffer");
    }
    let output = Surface::new(frame.width, frame.height).ok_or("output IOSurface")?;
    // SAFETY: all Objective-C calls use selectors and signatures from Metal's
    // public APIs. Owned references and this pool outlive the GPU completion.
    unsafe {
        let _pool = Pool(objc_autoreleasePoolPush());
        let context = compute()?;
        let dev = &context.device;
        let queue = &context.queue;
        let source = texture(dev.0, src, w, h).ok_or("source Metal texture")?;
        let dest = texture(dev.0, output.as_ptr(), w, h).ok_or("output Metal texture")?;
        let command = send0(queue.0, c"commandBuffer");
        if command.is_null() {
            return Err("no command buffer");
        }
        let encoder = send0(command, c"blitCommandEncoder");
        if encoder.is_null() {
            return Err("no blit encoder");
        }
        let copy: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            usize,
            usize,
            Origin,
            Size,
            *mut c_void,
            usize,
            usize,
            Origin,
        ) = std::mem::transmute(objc_msgSend as *const ());
        let zero = Origin { x: 0, y: 0, z: 0 };
        copy(encoder, sel_registerName(c"copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:".as_ptr()),
            source.0, 0, 0, zero, Size { width: w, height: h, depth: 1 }, dest.0, 0, 0, zero);
        send_void(encoder, c"endEncoding");
        send_void(command, c"commit");
        send_void(command, c"waitUntilCompleted");
        let status: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        if status(command, sel_registerName(c"status".as_ptr())) != 4 {
            return Err("Metal command failed");
        }
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
    impl Drop for Locked {
        fn drop(&mut self) {
            unsafe { IOSurfaceUnlock(self.0, self.1, std::ptr::null_mut()) };
        }
    }
    fn access(
        surface: *mut c_void,
        flags: u32,
        mut pixel: impl FnMut(*mut u8),
    ) -> Result<(), &'static str> {
        // SAFETY: both call sites own a valid, complete 7x3 BGRA IOSurface.
        unsafe {
            if IOSurfaceLock(surface, flags, std::ptr::null_mut()) != 0 {
                return Err("fixture lock");
            }
            let _lock = Locked(surface, flags);
            let base = IOSurfaceGetBaseAddress(surface).cast::<u8>();
            if base.is_null() {
                return Err("fixture base");
            }
            let stride = IOSurfaceGetBytesPerRow(surface);
            for y in 0..3 {
                for x in 0..7 {
                    pixel(base.add(y * stride + x * 4));
                }
            }
        }
        Ok(())
    }
    let source = wc_handle::iosection::Surface::new(7, 3, 4, 128, u32::from_be_bytes(*b"BGRA"))
        .ok_or("fixture surface")?;
    let frame = DirectFrame {
        key: 0,
        surface: Arc::new(source),
        offset: 0,
        width: 7,
        height: 3,
        stride: 128,
        format: crate::surface::PixelFormat::Argb8888,
    };
    let fill = |value| {
        access(frame.surface.as_ptr(), 0, |p| unsafe {
            std::ptr::copy_nonoverlapping([value, 19, 47, 255].as_ptr(), p, 4);
        })
    };
    let verify = |s: &Surface, value| {
        let mut correct = true;
        access(s.as_ptr(), 1, |p| unsafe {
            correct &= std::slice::from_raw_parts(p, 4) == [value, 19, 47, 255];
        })?;
        if correct {
            Ok(())
        } else {
            Err("GPU snapshot pixels changed or were incomplete")
        }
    };
    fill(23)?;
    let first = snapshot(&frame)?;
    fill(83)?; // Legal now: snapshot waited for the source's GPU reader.
    let second = snapshot(&frame)?;
    verify(&first, 23)?;
    verify(&second, 83)?;
    probe_tree_mapping()?;
    Ok(())
}

/// One immutable per-surface picture, placed in canvas device pixels.
#[derive(Clone)]
pub(crate) struct Layer {
    pub surface: std::sync::Arc<wc_handle::iosection::Surface>,
    pub at: (i32, i32),
    pub alpha: bool,
    pub view: Option<crate::surface::View>,
}

/// Snapshot a producer while its wl_buffer is still held. The returned picture
/// owns independent pixels, so later tree composition never touches that buffer.
pub(crate) fn capture(
    frame: &DirectFrame,
) -> Result<std::sync::Arc<wc_handle::iosection::Surface>, &'static str> {
    let output = snapshot(frame)?;
    Ok(std::sync::Arc::new(unsafe {
        wc_handle::iosection::Surface::retain_raw(output.as_ptr())
    }))
}

const COMPOSE_SHADER: &std::ffi::CStr = c"#include <metal_stdlib>\nusing namespace metal;\nstruct Params { int2 at; uint2 dst; float4 src; uint alpha; uint first; uint2 pad; };\nkernel void compose(texture2d<float,access::read> src [[texture(0)]], texture2d<float,access::read_write> dst [[texture(1)]], constant Params& p [[buffer(0)]], uint2 q [[thread_position_in_grid]]) {\n if (q.x >= dst.get_width() || q.y >= dst.get_height()) return;\n float4 color = p.first ? float4(0.0) : dst.read(q);\n long2 s = long2(q) - long2(p.at);\n if (all(s >= long2(0)) && s.x < long(p.dst.x) && s.y < long(p.dst.y)) {\n  float2 position = p.src.xy + (float2(s) + 0.5) * p.src.zw / float2(p.dst);\n  uint2 sample = uint2(clamp(floor(position),float2(0.0),float2(src.get_width()-1,src.get_height()-1)));\n  float4 pixel = src.read(sample);\n  if (!p.alpha) pixel.a = 1.0;\n  color = pixel + color * (1.0 - pixel.a);\n }\n dst.write(color,q);\n}\nstruct BufferParams { ulong offset; uint stride; uint width; uint height; uint pad; };\nkernel void capture_buffer(device const uchar* source [[buffer(0)]], constant BufferParams& p [[buffer(1)]], texture2d<float,access::write> output [[texture(0)]], uint2 q [[thread_position_in_grid]]) {\n if (q.x >= p.width || q.y >= p.height) return;\n ulong at = p.offset + ulong(q.y) * p.stride + ulong(q.x) * 4;\n output.write(float4(source[at+2],source[at+1],source[at],source[at+3])/255.0,q);\n}\n";

struct Compute {
    device: Owned,
    queue: Owned,
    compose: Owned,
    mapped: Owned,
    _functions: Vec<Owned>,
    _library: Owned,
}
fn compute() -> Result<&'static Compute, &'static str> {
    static COMPUTE: std::sync::OnceLock<Result<Compute, &'static str>> = std::sync::OnceLock::new();
    COMPUTE
        .get_or_init(|| unsafe {
            let _pool = Pool(objc_autoreleasePoolPush());
            let device = Owned(MTLCreateSystemDefaultDevice());
            if device.0.is_null() {
                return Err("Metal device");
            }
            let queue = Owned(send0(device.0, c"newCommandQueue"));
            if queue.0.is_null() {
                return Err("Metal queue");
            }
            let string: unsafe extern "C" fn(
                *mut c_void,
                *mut c_void,
                *const c_char,
            ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
            let source = string(
                objc_getClass(c"NSString".as_ptr()),
                sel_registerName(c"stringWithUTF8String:".as_ptr()),
                COMPOSE_SHADER.as_ptr(),
            );
            if source.is_null() {
                return Err("shader source");
            }
            let library: unsafe extern "C" fn(
                *mut c_void,
                *mut c_void,
                *mut c_void,
                *mut c_void,
                *mut *mut c_void,
            ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
            let mut error = std::ptr::null_mut();
            let library = Owned(library(
                device.0,
                sel_registerName(c"newLibraryWithSource:options:error:".as_ptr()),
                source,
                std::ptr::null_mut(),
                &mut error,
            ));
            if library.0.is_null() {
                return Err("Metal shader compilation");
            }
            let mut functions = Vec::new();
            let mut pipeline = |name: &std::ffi::CStr| -> Result<Owned, &'static str> {
                let name = string(
                    objc_getClass(c"NSString".as_ptr()),
                    sel_registerName(c"stringWithUTF8String:".as_ptr()),
                    name.as_ptr(),
                );
                let named: unsafe extern "C" fn(
                    *mut c_void,
                    *mut c_void,
                    *mut c_void,
                ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
                let function = Owned(named(
                    library.0,
                    sel_registerName(c"newFunctionWithName:".as_ptr()),
                    name,
                ));
                if function.0.is_null() {
                    return Err("Metal function");
                }
                let make: unsafe extern "C" fn(
                    *mut c_void,
                    *mut c_void,
                    *mut c_void,
                    *mut *mut c_void,
                ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
                let result = Owned(make(
                    device.0,
                    sel_registerName(c"newComputePipelineStateWithFunction:error:".as_ptr()),
                    function.0,
                    &mut error,
                ));
                if result.0.is_null() {
                    return Err("Metal pipeline");
                }
                functions.push(function);
                Ok(result)
            };
            let compose = pipeline(c"compose")?;
            let mapped = pipeline(c"capture_buffer")?;
            Ok(Compute {
                device,
                queue,
                compose,
                mapped,
                _functions: functions,
                _library: library,
            })
        })
        .as_ref()
        .map_err(|e| *e)
}

/// Composite the actual ordered surface tree with premultiplied alpha, clipping
/// and opaque XRGB normalization. All sources are immutable owned snapshots.
/// Completion retires every GPU reader before any source texture is dropped.
pub(crate) fn compose(
    width: i32,
    height: i32,
    layers: &[Layer],
) -> Result<std::sync::Arc<wc_handle::iosection::Surface>, &'static str> {
    let (w, h) = (
        usize::try_from(width).map_err(|_| "canvas width")?,
        usize::try_from(height).map_err(|_| "canvas height")?,
    );
    if w == 0 || h == 0 || layers.is_empty() {
        return Err("empty canvas");
    }
    let output = Surface::new(width, height).ok_or("composition surface")?;
    unsafe {
        let _pool = Pool(objc_autoreleasePoolPush());
        let context = compute()?;
        let dev = &context.device;
        let queue = &context.queue;
        let pipeline = &context.compose;
        let destination = texture(dev.0, output.as_ptr(), w, h).ok_or("canvas texture")?;
        let command = send0(queue.0, c"commandBuffer");
        if command.is_null() {
            return Err("composition command");
        }
        let mut sources = Vec::with_capacity(layers.len());
        #[repr(C, align(16))]
        struct Params {
            x: i32,
            y: i32,
            width: u32,
            height: u32,
            src: [f32; 4],
            alpha: u32,
            first: u32,
            pad: [u32; 2],
        }
        for (index, layer) in layers.iter().enumerate() {
            let ptr = layer.surface.as_ptr();
            if layer.surface.plane_count() != 0
                || layer.surface.pixel_format() != u32::from_be_bytes(*b"BGRA")
            {
                return Err("layer format");
            }
            let source = texture(dev.0, ptr, layer.surface.width(), layer.surface.height())
                .ok_or("layer texture")?;
            let encoder = send0(command, c"computeCommandEncoder");
            if encoder.is_null() {
                return Err("compute encoder");
            }
            let object: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) =
                std::mem::transmute(objc_msgSend as *const ());
            object(
                encoder,
                sel_registerName(c"setComputePipelineState:".as_ptr()),
                pipeline.0,
            );
            let bind: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, usize) =
                std::mem::transmute(objc_msgSend as *const ());
            bind(
                encoder,
                sel_registerName(c"setTexture:atIndex:".as_ptr()),
                source.0,
                0,
            );
            bind(
                encoder,
                sel_registerName(c"setTexture:atIndex:".as_ptr()),
                destination.0,
                1,
            );
            let view = layer.view.unwrap_or(crate::surface::View {
                src: (
                    0.0,
                    0.0,
                    layer.surface.width() as f64,
                    layer.surface.height() as f64,
                ),
                dst: (layer.surface.width() as i32, layer.surface.height() as i32),
            });
            if view.dst.0 <= 0
                || view.dst.1 <= 0
                || ![view.src.0, view.src.1, view.src.2, view.src.3]
                    .iter()
                    .all(|x| x.is_finite())
                || view.src.0 < 0.0
                || view.src.1 < 0.0
                || view.src.2 <= 0.0
                || view.src.3 <= 0.0
                || view.src.0 + view.src.2 > layer.surface.width() as f64
                || view.src.1 + view.src.3 > layer.surface.height() as f64
            {
                return Err("layer viewport bounds");
            }
            let params = Params {
                x: layer.at.0,
                y: layer.at.1,
                width: view.dst.0 as u32,
                height: view.dst.1 as u32,
                src: [
                    view.src.0 as f32,
                    view.src.1 as f32,
                    view.src.2 as f32,
                    view.src.3 as f32,
                ],
                alpha: u32::from(layer.alpha),
                first: u32::from(index == 0),
                pad: [0; 2],
            };
            let bytes: unsafe extern "C" fn(*mut c_void, *mut c_void, *const Params, usize, usize) =
                std::mem::transmute(objc_msgSend as *const ());
            bytes(
                encoder,
                sel_registerName(c"setBytes:length:atIndex:".as_ptr()),
                &params,
                std::mem::size_of::<Params>(),
                0,
            );
            let dispatch: unsafe extern "C" fn(*mut c_void, *mut c_void, Size, Size) =
                std::mem::transmute(objc_msgSend as *const ());
            dispatch(
                encoder,
                sel_registerName(c"dispatchThreads:threadsPerThreadgroup:".as_ptr()),
                Size {
                    width: w,
                    height: h,
                    depth: 1,
                },
                Size {
                    width: 8,
                    height: 8,
                    depth: 1,
                },
            );
            send_void(encoder, c"endEncoding");
            sources.push(source);
        }
        send_void(command, c"commit");
        send_void(command, c"waitUntilCompleted");
        let status: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        if status(command, sel_registerName(c"status".as_ptr())) != 4 {
            return Err("GPU composition failed");
        }
    }
    Ok(std::sync::Arc::new(unsafe {
        wc_handle::iosection::Surface::retain_raw(output.as_ptr())
    }))
}

/// Read an original wl_shm mapping on the GPU without copying its bytes into a
/// shadow or staging buffer. The buffer-read kernel accepts arbitrary row pitch
/// and byte offset, so a texture-buffer alignment restriction does not change
/// the application's layout.
///
/// # Safety
/// `base` must describe `mapped_len` bytes of live, page-aligned shared VM pages.
/// `logical_len` is the readable pool size, at most mapped_len. The caller must
/// retain the original mapping/backing and prevent buffer reuse until return.
/// Only the logical buffer range is read; rounded trailing VM bytes are not read.
pub(crate) unsafe fn capture_mapping(
    base: *const u8,
    mapped_len: usize,
    logical_len: usize,
    offset: usize,
    spec: crate::surface::BufferSpec,
) -> Result<std::sync::Arc<wc_handle::iosection::Surface>, &'static str> {
    let need = spec.required_len().ok_or("mapped buffer geometry")?;
    let end = offset.checked_add(need).ok_or("mapped buffer overflow")?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = usize::try_from(page).map_err(|_| "host VM page size")?;
    if page == 0
        || base.is_null()
        || (base as usize) % page != 0
        || mapped_len == 0
        || mapped_len % page != 0
        || logical_len > mapped_len
        || end > logical_len
        || mapped_len > isize::MAX as usize
    {
        return Err("mapped buffer page alignment or bounds");
    }
    let output = Surface::new(spec.width, spec.height).ok_or("mapped snapshot surface")?;
    unsafe {
        let _pool = Pool(objc_autoreleasePoolPush());
        let context = compute()?;
        let make: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            usize,
            usize,
            *mut c_void,
        ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
        let buffer = Owned(make(
            context.device.0,
            sel_registerName(c"newBufferWithBytesNoCopy:length:options:deallocator:".as_ptr()),
            base.cast_mut().cast(),
            mapped_len,
            0,
            std::ptr::null_mut(),
        ));
        if buffer.0.is_null() {
            return Err("Metal cannot import these original shared VM pages");
        }
        let destination = texture(
            context.device.0,
            output.as_ptr(),
            spec.width as usize,
            spec.height as usize,
        )
        .ok_or("mapped output texture")?;
        let command = send0(context.queue.0, c"commandBuffer");
        if command.is_null() {
            return Err("mapped command");
        }
        let encoder = send0(command, c"computeCommandEncoder");
        if encoder.is_null() {
            return Err("mapped encoder");
        }
        let object: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        object(
            encoder,
            sel_registerName(c"setComputePipelineState:".as_ptr()),
            context.mapped.0,
        );
        let bind: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, usize, usize) =
            std::mem::transmute(objc_msgSend as *const ());
        bind(
            encoder,
            sel_registerName(c"setBuffer:offset:atIndex:".as_ptr()),
            buffer.0,
            0,
            0,
        );
        let tex: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, usize) =
            std::mem::transmute(objc_msgSend as *const ());
        tex(
            encoder,
            sel_registerName(c"setTexture:atIndex:".as_ptr()),
            destination.0,
            0,
        );
        #[repr(C)]
        struct Params {
            offset: u64,
            stride: u32,
            width: u32,
            height: u32,
            pad: u32,
        }
        let params = Params {
            offset: offset as u64,
            stride: spec.stride as u32,
            width: spec.width as u32,
            height: spec.height as u32,
            pad: 0,
        };
        let bytes: unsafe extern "C" fn(*mut c_void, *mut c_void, *const Params, usize, usize) =
            std::mem::transmute(objc_msgSend as *const ());
        bytes(
            encoder,
            sel_registerName(c"setBytes:length:atIndex:".as_ptr()),
            &params,
            std::mem::size_of::<Params>(),
            1,
        );
        let dispatch: unsafe extern "C" fn(*mut c_void, *mut c_void, Size, Size) =
            std::mem::transmute(objc_msgSend as *const ());
        dispatch(
            encoder,
            sel_registerName(c"dispatchThreads:threadsPerThreadgroup:".as_ptr()),
            Size {
                width: spec.width as usize,
                height: spec.height as usize,
                depth: 1,
            },
            Size {
                width: 8,
                height: 8,
                depth: 1,
            },
        );
        send_void(encoder, c"endEncoding");
        send_void(command, c"commit");
        send_void(command, c"waitUntilCompleted");
        let status: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        if status(command, sel_registerName(c"status".as_ptr())) != 4 {
            return Err("mapped capture GPU failure");
        }
    }
    Ok(std::sync::Arc::new(unsafe {
        wc_handle::iosection::Surface::retain_raw(output.as_ptr())
    }))
}

/// Headless original-page import and ordered-tree fixture. CPU accesses create
/// and verify the fixture only. The product mapped capture and tree path use
/// GPU reads/writes and wait for retirement before source reuse.
fn probe_tree_mapping() -> Result<(), &'static str> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let name = std::ffi::CString::new(format!(
        "/wcm.{:x}.{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "fixture clock")?
            .subsec_nanos()
    ))
    .map_err(|_| "fixture name")?;
    let raw = unsafe {
        libc::shm_open(
            name.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
    };
    if raw < 0 {
        return Err("fixture shm_open");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    unsafe {
        libc::shm_unlink(name.as_ptr());
    }
    let (width, height, stride, offset) = (7, 3, 36, 13usize);
    let spec = crate::surface::BufferSpec {
        width,
        height,
        stride,
        format: crate::surface::PixelFormat::Argb8888,
    };
    let logical = offset
        .checked_add(spec.required_len().ok_or("fixture geometry")?)
        .ok_or("fixture range")?;
    if unsafe { libc::ftruncate(fd.as_raw_fd(), logical as libc::off_t) } != 0 {
        return Err("fixture size");
    }
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .map_err(|_| "fixture page")?;
    let mapped = logical.checked_add(page - 1).ok_or("fixture extent")? / page * page;
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mapped,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err("fixture mmap");
    }
    struct Map(*mut c_void, usize);
    impl Drop for Map {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.0, self.1);
            }
        }
    }
    let map = Map(ptr, mapped);
    let fill = |color: [u8; 4]| unsafe {
        for y in 0..height as usize {
            for x in 0..width as usize {
                std::ptr::copy_nonoverlapping(
                    color.as_ptr(),
                    map.0.cast::<u8>().add(offset + y * stride as usize + x * 4),
                    4,
                );
            }
        }
    };
    fill([20, 40, 60, 0]);
    let bottom = unsafe { capture_mapping(map.0.cast(), mapped, logical, offset, spec) }?;
    fill([30, 0, 0, 128]); // Original mapping is reusable only after capture waited.
    let top = unsafe { capture_mapping(map.0.cast(), mapped, logical, offset, spec) }?;
    fill([255, 255, 255, 255]);
    let layers = [
        Layer {
            surface: bottom,
            at: (1, 1),
            alpha: false,
            view: None,
        },
        Layer {
            surface: top,
            at: (-2, 0),
            alpha: true,
            view: None,
        },
    ];
    let output = compose(9, 5, &layers)?;
    let correct = unsafe {
        output.read_snapshot(|bytes, _, _, pitch| {
            [
                (8, 4, [0u8, 0, 0, 0]),
                (7, 3, [20, 40, 60, 255]),
                (0, 0, [30, 0, 0, 128]),
                (2, 1, [40, 20, 30, 255]),
            ]
            .into_iter()
            .all(|(x, y, expected)| {
                bytes[y * pitch + x * 4..y * pitch + x * 4 + 4]
                    .iter()
                    .zip(expected)
                    .all(|(actual, want)| i16::from(*actual).abs_diff(i16::from(want)) <= 1)
            })
        })
    }
    .ok_or("tree fixture readback")?;
    if !correct {
        return Err("mapped source reuse, alpha normalization, order or clipping differs");
    }
    unsafe {
        for y in 0..height as usize {
            for x in 0..width as usize {
                std::ptr::copy_nonoverlapping(
                    [(x + y * 10) as u8, 0, 0, 255].as_ptr(),
                    map.0.cast::<u8>().add(offset + y * stride as usize + x * 4),
                    4,
                );
            }
        }
    }
    let patterned = unsafe { capture_mapping(map.0.cast(), mapped, logical, offset, spec) }?;
    let mut layer = Layer {
        surface: patterned,
        at: (0, 0),
        alpha: false,
        view: Some(crate::surface::View {
            src: (1.0, 1.0, 2.0, 1.0),
            dst: (4, 2),
        }),
    };
    let scaled = compose(4, 2, &[layer.clone()])?;
    let correct = unsafe {
        scaled.read_snapshot(|bytes, _, _, pitch| {
            (0..2).all(|y| (0..4).all(|x| bytes[y * pitch + x * 4] == if x < 2 { 11 } else { 12 }))
        })
    }
    .ok_or("viewport fixture readback")?;
    if !correct {
        return Err("GPU crop/nearest viewport scaling differs");
    }
    // A metadata-only viewport change reuses the owned source, not client pages.
    layer.view = Some(crate::surface::View {
        src: (2.0, 0.0, 1.0, 1.0),
        dst: (2, 1),
    });
    let changed = compose(2, 1, &[layer])?;
    let correct = unsafe { changed.read_snapshot(|bytes, _, _, _| bytes[0] == 2 && bytes[4] == 2) }
        .ok_or("metadata-only viewport readback")?;
    if !correct {
        return Err("metadata-only GPU viewport update differs");
    }
    Ok(())
}
