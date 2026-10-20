//! §103.19: on a Mac, an exportable image's memory is an IOSurface.
//!
//! KosmicKrisp renders only into GPU memory; the one way to give that to the
//! compositor (another process) without a copy is an IOSurface. So where the
//! other platforms bind a shared image to an imported host-pointer section
//! (`shared.rs`), the Mac creates the image for `MTLTEXTURE` import, makes an
//! IOSurface of its size, wraps it in a Metal texture and imports that as the
//! image's dedicated memory. The compositor gets the IOSurface as a mach port
//! (`wc_handle::iosection`) under the section's name.
//!
//! The image stays LINEAR, as the app side asked, so its layout is the
//! driver's own answer; the IOSurface is made with exactly that row pitch, so
//! what the app side describes in its dma-buf is what the compositor reads.

use ash::vk;
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::Mutex;
use wc_handle::iosection::Surface;

/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLTEXTURE_BIT_EXT`.
pub const MTLTEXTURE: u32 = 0x0002_0000;
/// `VK_STRUCTURE_TYPE_IMPORT_MEMORY_METAL_HANDLE_INFO_EXT`.
const IMPORT_MEMORY_METAL_HANDLE_INFO: i32 = 1_000_602_000;

/// `VkImportMemoryMetalHandleInfoEXT` (`vulkan_metal.h`, not in ash).
#[repr(C)]
struct ImportMemoryMetalHandleInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    handle_type: u32,
    handle: *mut c_void,
}

/// A shared image's format and size, from its `vkCreateImage`.
#[derive(Clone, Copy)]
struct Shape {
    format: vk::Format,
    width: u32,
    height: u32,
}

static SHAPES: Mutex<Option<HashMap<u64, Shape>>> = Mutex::new(None);

fn shapes<R>(f: impl FnOnce(&mut HashMap<u64, Shape>) -> R) -> R {
    let mut g = SHAPES.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// The pixel formats a shared image may have here: Metal's, and the IOSurface's.
fn formats(f: vk::Format) -> Option<(u64, u32)> {
    // MTLPixelFormat values; IOSurface fourccs.
    Some(match f {
        vk::Format::B8G8R8A8_UNORM => (80, u32::from_be_bytes(*b"BGRA")),
        vk::Format::B8G8R8A8_SRGB => (81, u32::from_be_bytes(*b"BGRA")),
        vk::Format::R8G8B8A8_UNORM => (70, u32::from_be_bytes(*b"RGBA")),
        vk::Format::R8G8B8A8_SRGB => (71, u32::from_be_bytes(*b"RGBA")),
        _ => return None,
    })
}

/// Whether a shared image of this format can be an IOSurface.
pub fn format_ok(f: vk::Format) -> bool {
    formats(f).is_some()
}

/// What a LINEAR image of an IOSurface format can do here beyond the
/// driver's own `linearTilingFeatures`: every exported (dma-buf) image is
/// LINEAR and is made on an imported IOSurface texture, which the driver lets
/// an application render, blend and blit into (KosmicKrisp with the MTLTEXTURE
/// import). `VkFormatFeatureFlags` and `VkFormatFeatureFlags2` agree on these
/// bits.
pub fn linear_extra(format: vk::Format) -> u64 {
    if !format_ok(format) {
        return 0;
    }
    (vk::FormatFeatureFlags::COLOR_ATTACHMENT
        | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
        | vk::FormatFeatureFlags::BLIT_DST)
        .as_raw()
        .into()
}

/// `vkCreateImage` made a shared image.
pub fn remember(image: vk::Image, format: vk::Format, width: u32, height: u32) {
    shapes(|s| s.insert(ash::vk::Handle::as_raw(image), Shape { format, width, height }));
}

/// `vkDestroyImage`.
pub fn forget(image: vk::Image) {
    shapes(|s| s.remove(&ash::vk::Handle::as_raw(image)));
}

/// Whether `image` is a shared image this process made.
pub fn is_shared(image: vk::Image) -> bool {
    shapes(|s| s.contains_key(&ash::vk::Handle::as_raw(image)))
}

// -- Metal, through the Objective-C runtime -----------------------------------

#[link(name = "Metal", kind = "framework")]
unsafe extern "C" {
    fn MTLCreateSystemDefaultDevice() -> *mut c_void;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
}

/// One Metal texture on an IOSurface, owned (+1).
pub struct Texture(pub(crate) *mut c_void);

// SAFETY: Metal textures may be used and released from any thread.
unsafe impl Send for Texture {}
unsafe impl Sync for Texture {}

impl Drop for Texture {
    fn drop(&mut self) {
        // SAFETY: `release` on the +1 this value owns.
        unsafe {
            let send: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(objc_msgSend as *const ());
            send(self.0, sel_registerName(c"release".as_ptr()));
        }
    }
}

/// The system's Metal device: the one KosmicKrisp uses on a Mac with one GPU.
fn device() -> *mut c_void {
    static DEV: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // SAFETY: a plain C call; the device lives for the process.
    *DEV.get_or_init(|| unsafe { MTLCreateSystemDefaultDevice() } as usize) as *mut c_void
}

/// `[device newTextureWithDescriptor:d iosurface:surface plane:0]`, render
/// target and shader read/write.
fn texture(surface: &Surface, mtl_format: u64, width: u32, height: u32) -> Option<Texture> {
    // SAFETY: the reference owns a live IOSurface throughout this call.
    unsafe { texture_plane(surface.as_ptr(), mtl_format, width, height, 0, 1 | 2 | 4 | 0x10) }
}

/// Wrap one actual IOSurface plane. Caller owns the live surface and supplies
/// the plane's true dimensions and a Metal format matching its element size.
pub(crate) unsafe fn texture_plane(surface: *mut c_void, mtl_format: u64,
    width: u32, height: u32, plane: u64, usage: u64) -> Option<Texture> {
    let dev = device();
    if dev.is_null() {
        return None;
    }
    // SAFETY: messages to Metal classes and objects with the argument types
    // their selectors take; every object here is autoreleased or owned.
    unsafe {
        let class = objc_getClass(c"MTLTextureDescriptor".as_ptr());
        let make: unsafe extern "C" fn(*mut c_void, *mut c_void, u64, u64, u64, bool) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let desc = make(
            class,
            sel_registerName(c"texture2DDescriptorWithPixelFormat:width:height:mipmapped:".as_ptr()),
            mtl_format,
            u64::from(width),
            u64::from(height),
            false,
        );
        if desc.is_null() {
            return None;
        }
        let set: unsafe extern "C" fn(*mut c_void, *mut c_void, u64) = std::mem::transmute(objc_msgSend as *const ());
        // MTLTextureUsageShaderRead | ShaderWrite | RenderTarget | PixelFormatView
        // (an SRGB or UNORM view of the other, which Zink makes).
        set(desc, sel_registerName(c"setUsage:".as_ptr()), usage);
        // MTLStorageModeShared: an IOSurface's memory is the CPU's too.
        set(desc, sel_registerName(c"setStorageMode:".as_ptr()), 0);
        let new: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, u64) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let t = new(dev, sel_registerName(c"newTextureWithDescriptor:iosurface:plane:".as_ptr()), desc, surface, plane);
        (!t.is_null()).then_some(Texture(t))
    }
}

/// An IOSurface section: the surface the compositor shows, and the Metal
/// texture the GPU's import of it holds.
pub struct Section {
    pub surface: Surface,
    _texture: Texture,
}

/// Make `image`'s IOSurface and import it as the image's dedicated memory,
/// of a type in `want`. Returns the section, the memory and its type.
pub fn make(driver: &crate::driver::Driver, image: vk::Image, want: u32) -> Result<(Section, vk::DeviceMemory, u32), String> {
    let sh = shapes(|s| s.get(&ash::vk::Handle::as_raw(image)).copied()).ok_or("not a shared image this process made")?;
    let (_, fourcc) = formats(sh.format).ok_or_else(|| format!("format {:?} cannot be an IOSurface", sh.format))?;
    let sub = vk::ImageSubresource { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, array_layer: 0 };
    // SAFETY: the driver's own LINEAR image, not yet bound; its layout is
    // what the app side is told, so the surface is made to match it.
    let layout = unsafe { driver.device.get_image_subresource_layout(image, sub) };
    let pitch = u32::try_from(layout.row_pitch).map_err(|_| "row pitch beyond 4 GiB")?;
    let surface = Surface::new(sh.width, sh.height, 4, pitch, fourcc)
        .ok_or_else(|| format!("IOSurfaceCreate {}x{} failed", sh.width, sh.height))?;
    import(driver, image, want, surface)
}

/// Import an existing section's `surface` as `image`'s dedicated memory (the
/// GPU process importing a token it made: `svImportShared`).
pub fn import(driver: &crate::driver::Driver, image: vk::Image, want: u32, surface: Surface) -> Result<(Section, vk::DeviceMemory, u32), String> {
    let sh = shapes(|s| s.get(&ash::vk::Handle::as_raw(image)).copied()).ok_or("not a shared image this process made")?;
    let (mtl, _) = formats(sh.format).ok_or_else(|| format!("format {:?} cannot be an IOSurface", sh.format))?;
    let texture = texture(&surface, mtl, sh.width, sh.height).ok_or("newTextureWithDescriptor:iosurface:plane: failed")?;
    // SAFETY: the driver's own image.
    let req = unsafe { driver.device.get_image_memory_requirements(image) };
    let bits = req.memory_type_bits & want & ((1u64 << driver.mem_props.memory_type_count) - 1) as u32;
    let device_local = (0..driver.mem_props.memory_type_count).find(|&i| {
        bits & (1 << i) != 0
            && driver.mem_props.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    });
    let ty = device_local.or_else(|| (bits != 0).then(|| bits.trailing_zeros())).ok_or("no memory type for the image")?;
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let metal = ImportMemoryMetalHandleInfo {
        s_type: vk::StructureType::from_raw(IMPORT_MEMORY_METAL_HANDLE_INFO),
        p_next: (&raw mut dedicated).cast(),
        handle_type: MTLTEXTURE,
        handle: texture.0,
    };
    let mut ai = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty);
    ai.p_next = (&raw const metal).cast();
    // SAFETY: a valid chain (the metal import, then the dedicated image) on
    // the stack for the call; the texture outlives the memory (`Section`).
    let memory = unsafe { driver.device.allocate_memory(&ai, None) }
        .map_err(|e| format!("vkAllocateMemory(import an IOSurface texture): {e:?}"))?;
    Ok((Section { surface, _texture: texture }, memory, ty))
}

/// The compositor's section service, named by the main process.
fn service() -> Option<u32> {
    static S: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *S.get_or_init(|| {
        let name = std::env::var("WC_GPU_SECTIONS").ok()?;
        match wc_handle::iosection::look_up(&name) {
            Ok(p) => Some(p),
            Err(kr) => {
                eprintln!("[gpu] no section service {name} ({kr}): the compositor cannot show exported images");
                None
            }
        }
    })
}

/// Tell the compositor section `name` is `surface`.
pub fn announce(name: &str, surface: &Surface) {
    if let Some(s) = service() {
        let kr = wc_handle::iosection::send(s, name, surface);
        if kr != 0 {
            eprintln!("[gpu] sending section {name} to the compositor: {kr}");
        }
    }
}

/// Section `name` is gone from this process.
pub fn retire(name: &str) {
    if let Some(s) = service() {
        let _ = wc_handle::iosection::send_gone(s, name);
    }
}
