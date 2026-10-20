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
    usage: vk::ImageUsageFlags,
    flags: vk::ImageCreateFlags,
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

/// Extra LINEAR features are offered only when the real driver accepts the
/// corresponding dedicated MTLTexture import and its optimal format has them.
pub fn linear_extra(driver: &crate::driver::Driver, format: vk::Format) -> u64 {
    if !format_ok(format) { return 0; }
    let native = unsafe { driver.instance.get_physical_device_format_properties(driver.pd, format) };
    let mut extra = 0;
    for (feature, usage) in [
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, vk::ImageUsageFlags::COLOR_ATTACHMENT),
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND, vk::ImageUsageFlags::COLOR_ATTACHMENT),
        (vk::FormatFeatureFlags::BLIT_DST, vk::ImageUsageFlags::TRANSFER_DST),
    ] {
        if !native.optimal_tiling_features.contains(feature)
            || crate::metal_image::texture_usage(format.as_raw(), usage.as_raw(), 0).is_none() { continue; }
        let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::from_raw(MTLTEXTURE));
        let info = vk::PhysicalDeviceImageFormatInfo2::default().format(format).ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::LINEAR).usage(usage).push_next(&mut ext);
        let mut external = vk::ExternalImageFormatProperties::default();
        let mut props = vk::ImageFormatProperties2::default().push_next(&mut external);
        let accepted = unsafe { driver.instance.get_physical_device_image_format_properties2(driver.pd, &info, &mut props) }.is_ok();
        let samples = props.image_format_properties.sample_counts.as_raw();
        if accepted && crate::metal_image::importable(
            external.external_memory_properties.external_memory_features.as_raw(),
            external.external_memory_properties.compatible_handle_types.as_raw(), samples) {
            extra |= u64::from(feature.as_raw());
        }
    }
    extra
}

/// `vkCreateImage` made a shared image.
pub fn remember(image: vk::Image, format: vk::Format, width: u32, height: u32,
    usage: vk::ImageUsageFlags, flags: vk::ImageCreateFlags) {
    shapes(|s| s.insert(ash::vk::Handle::as_raw(image), Shape { format, width, height, usage, flags }));
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
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
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
fn texture(surface: &Surface, mtl_format: u64, width: u32, height: u32, usage: u64) -> Option<Texture> {
    // SAFETY: the reference owns a live IOSurface throughout this call.
    unsafe { texture_plane(surface.as_ptr(), mtl_format, width, height, 0, usage) }
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
        struct Pool(*mut c_void);
        impl Drop for Pool { fn drop(&mut self) { unsafe { objc_autoreleasePoolPop(self.0) }; } }
        // Rust CLI workers have no implicit AppKit autorelease pool. The
        // descriptor is autoreleased; the returned new texture owns its +1.
        let _pool = Pool(objc_autoreleasePoolPush());
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
    if layout.offset != 0 { return Err(format!("IOSurface image layout requires unsupported offset {}", layout.offset)); }
    let pitch = u32::try_from(layout.row_pitch).map_err(|_| "row pitch beyond 4 GiB")?;
    let surface = Surface::new(sh.width, sh.height, 4, pitch, fourcc)
        .ok_or_else(|| format!("IOSurfaceCreate {}x{} failed", sh.width, sh.height))?;
    import(driver, image, want, surface)
}

/// Import an existing section's `surface` as `image`'s dedicated memory (the
/// GPU process importing a token it made: `svImportShared`).
pub fn import(driver: &crate::driver::Driver, image: vk::Image, want: u32, surface: Surface) -> Result<(Section, vk::DeviceMemory, u32), String> {
    let sh = shapes(|s| s.get(&ash::vk::Handle::as_raw(image)).copied()).ok_or("not a shared image this process made")?;
    let (mtl, fourcc) = formats(sh.format).ok_or_else(|| format!("format {:?} cannot be an IOSurface", sh.format))?;
    let sub = vk::ImageSubresource { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, array_layer: 0 };
    let layout = unsafe { driver.device.get_image_subresource_layout(image, sub) };
    let actual = crate::metal_image::SurfaceLayout {
        width: surface.width(), height: surface.height(), pitch: surface.bytes_per_row(),
        bpp: surface.bytes_per_element(), fourcc: surface.pixel_format(), allocation: surface.len(),
    };
    if surface.plane_count() != 0 || !crate::metal_image::compatible_surface(actual, sh.width, sh.height,
        fourcc, layout.offset, layout.row_pitch, layout.size) {
        return Err(format!("IOSurface geometry/format does not match target image layout {layout:?}"));
    }
    let usage = crate::metal_image::texture_usage(sh.format.as_raw(), sh.usage.as_raw(), sh.flags.as_raw())
        .ok_or("unsupported Metal texture format/usage/view combination")?;
    let texture = texture(&surface, mtl, sh.width, sh.height, usage).ok_or("newTextureWithDescriptor:iosurface:plane: failed")?;
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

/// Fail closed if the retained service send right has become a dead name.
/// This checks endpoint lifetime, not the receiver identity established by its
/// capability handoff. It never looks up a replacement service after failure.
pub(crate) fn nv12_endpoint_alive() -> bool {
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn mach_port_type(task: u32, name: u32, kind: *mut u32) -> i32;
    }
    let Some(port) = service() else { return false; };
    let mut kind = 0;
    // SAFETY: the cached capability belongs to this task; kind is writable.
    unsafe { mach_port_type(mach_task_self_, port, &mut kind) == 0 }
        && kind & (1 << 16) != 0 && kind & (1 << 20) == 0
}

/// Keep the authenticated owner connection open for this GPU process lifetime.
/// A separate atomic broker observation makes revocation permanent locally too.
struct OwnerConnection {
    _client: wc_broker::client::Client,
    owner: wc_handle::iosection::Owner,
    validity: std::sync::Arc<crate::native_video_shared_policy::Revocation>,
}
fn owner_connection() -> Result<(wc_handle::iosection::Owner,
    std::sync::Arc<crate::native_video_shared_policy::Revocation>), String> {
    static CONNECTION: Mutex<Option<OwnerConnection>> = Mutex::new(None);
    let mut g = CONNECTION.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        let mut client = wc_broker::client::Client::connect(&wc_broker::client::path()).map_err(|e| e.to_string())?;
        client.hello("gpu").map_err(|e| e.to_string())?;
        let owner = client.owner_token().map_err(|e| e.to_string())?;
        let mut observer = wc_broker::client::Client::connect(&wc_broker::client::path()).map_err(|e| e.to_string())?;
        observer.hello("iosurface-observer").map_err(|e| e.to_string())?;
        observer.observe_owner(owner, std::process::id()).map_err(|e| e.to_string())?;
        let validity = std::sync::Arc::new(crate::native_video_shared_policy::Revocation::default());
        let watched = std::sync::Arc::clone(&validity);
        std::thread::Builder::new().name("nv12-export-owner".into()).spawn(move || {
            let ended = format!("!owner-ended {}", owner.connection);
            loop {
                match observer.next_event() {
                    Ok(event) if event != ended => continue,
                    _ => break,
                }
            }
            watched.revoke();
        }).map_err(|e| e.to_string())?;
        *g = Some(OwnerConnection { _client: client, owner, validity });
    }
    let connection = g.as_ref().expect("initialized");
    if !connection.validity.active() { return Err("IOSurface broker owner was revoked".into()); }
    Ok((connection.owner, std::sync::Arc::clone(&connection.validity)))
}
fn owner() -> Result<wc_handle::iosection::Owner, String> {
    owner_connection().map(|(owner, _)| owner)
}

/// Publish only after the compositor has acknowledged an authenticated ADD.
pub fn announce(name: &str, surface: &Surface) -> Result<(), String> {
    let s = service().ok_or("no compositor IOSurface service")?;
    let owner = owner()?;
    let kr = wc_handle::iosection::send(s, owner, name, surface);
    if kr != 0 {
        // ADD may have arrived before its reply was lost. Withdraw that name.
        let _ = wc_handle::iosection::send_gone(s, owner, name);
        return Err(format!("registering IOSurface {name}: {kr}"));
    }
    Ok(())
}

/// Publish the exact joined NV12 surface through the typed audited endpoint.
/// Acknowledgment proves registration, never producer or consumer completion.
pub(crate) fn announce_nv12(name: &str, surface: &Surface) -> Result<std::sync::Arc<crate::native_video_shared_policy::Revocation>, String> {
    let s = service().ok_or("no compositor IOSurface service")?;
    let (owner, validity) = owner_connection()?;
    let kr = wc_handle::iosection::send_nv12(s, owner, name, surface);
    if kr != 0 {
        // The receiver may have adopted the exact surface before ACK was lost.
        let _ = wc_handle::iosection::send_gone(s, owner, name);
        return Err(format!("registering original NV12 IOSurface {name}: {kr}"));
    }
    if !validity.active() || !nv12_endpoint_alive() {
        let _ = wc_handle::iosection::send_gone(s, owner, name);
        return Err("NV12 owner or endpoint was revoked during registration".into());
    }
    Ok(validity)
}

pub fn retire(name: &str) {
    if let (Some(s), Ok(owner)) = (service(), owner()) {
        let _ = wc_handle::iosection::send_gone(s, owner, name);
    }
}
