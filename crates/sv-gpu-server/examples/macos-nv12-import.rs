//! Cross-build on the build host; run headlessly on hardware only when allowed.
//! This tests an IOSurface-backed NV12 fixture, not VideoToolbox decode itself.
#[cfg(not(target_os = "macos"))]
fn main() { eprintln!("this probe requires macOS"); }

#[cfg(target_os = "macos")]
fn main() {
    use std::{ffi::c_void, sync::Arc};
    use sv_gpu_server::{driver::Driver, video_nv12::macos::{Picture, hardware_h264}};
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDictionaryCreateMutable(a: *const c_void, capacity: isize, k: *const c_void, v: *const c_void) -> *mut c_void;
        fn CFDictionarySetValue(d: *mut c_void, k: *const c_void, v: *const c_void);
        fn CFRelease(p: *const c_void);
        static kCFBooleanTrue: *const c_void;
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
    }
    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        fn CVPixelBufferCreate(a: *const c_void, width: usize, height: usize, format: u32,
            attributes: *const c_void, buffer: *mut *mut c_void) -> i32;
        fn CVBufferRelease(buffer: *mut c_void);
        static kCVPixelBufferIOSurfacePropertiesKey: *const c_void;
        static kCVPixelBufferMetalCompatibilityKey: *const c_void;
    }
    let picture = unsafe {
        let dict = || CFDictionaryCreateMutable(std::ptr::null(), 0,
            (&raw const kCFTypeDictionaryKeyCallBacks).cast(), (&raw const kCFTypeDictionaryValueCallBacks).cast());
        let attributes = dict(); let surface = dict();
        assert!(!attributes.is_null() && !surface.is_null());
        CFDictionarySetValue(attributes, kCVPixelBufferIOSurfacePropertiesKey, surface);
        CFDictionarySetValue(attributes, kCVPixelBufferMetalCompatibilityKey, kCFBooleanTrue);
        let mut buffer = std::ptr::null_mut();
        let status = CVPixelBufferCreate(std::ptr::null(), 128, 64, u32::from_be_bytes(*b"420v"), attributes, &mut buffer);
        CFRelease(surface); CFRelease(attributes);
        assert_eq!(status, 0, "NV12 fixture allocation");
        // This fixture has no asynchronous producer; it is ready at creation.
        let picture = Picture::from_completed_buffer(buffer);
        CVBufferRelease(buffer);
        picture.expect("validate NV12 fixture planes")
    };
    println!("hardware_h264={} layout={:?} range={:?}", hardware_h264(), picture.layout(), picture.range());
    let planes = picture.metal_planes().expect("wrap R8 and RG8 Metal planes");
    let driver = Arc::new(Driver::open(0).expect("open native Vulkan driver"));
    // No GPU submission occurs, so all consumers are retired when this drops.
    let imported = unsafe { planes.import(driver) }.expect("import both dedicated planes");
    println!("imported two plane images {:?}; no multi-plane VkImage advertised", imported.images());
}
