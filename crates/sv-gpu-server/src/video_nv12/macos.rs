//! VideoToolbox output ownership and plane import, without a Vulkan Video facade.
//! A completed CVPixelBuffer is retained through every imported plane. NV12 is
//! represented explicitly as two R8/RG8 images, not falsely as one multi-plane
//! VkImage. A future YCbCr VkImage implementation must join them in the driver.

use std::{ffi::c_void, sync::Arc};
use ash::vk;
use super::{Layout, Plane};
use crate::{driver::Driver, iosurface::{self, Texture}};

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVBufferRetain(buffer: *mut c_void) -> *mut c_void;
    fn CVBufferRelease(buffer: *mut c_void);
    fn CVPixelBufferGetPixelFormatType(buffer: *mut c_void) -> u32;
    fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetPlaneCount(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetWidthOfPlane(buffer: *mut c_void, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(buffer: *mut c_void, plane: usize) -> usize;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: *mut c_void, plane: usize) -> usize;
    fn CVPixelBufferGetIOSurface(buffer: *mut c_void) -> *mut c_void;
}
#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetPlaneCount(surface: *mut c_void) -> usize;
    fn IOSurfaceGetWidthOfPlane(surface: *mut c_void, plane: usize) -> usize;
    fn IOSurfaceGetHeightOfPlane(surface: *mut c_void, plane: usize) -> usize;
    fn IOSurfaceGetBytesPerRowOfPlane(surface: *mut c_void, plane: usize) -> usize;
    fn IOSurfaceGetBytesPerElementOfPlane(surface: *mut c_void, plane: usize) -> usize;
    fn IOSurfaceGetOffsetOfPlane(surface: *mut c_void, plane: usize) -> usize;
    fn IOSurfaceGetAllocSize(surface: *mut c_void) -> usize;
}
#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" { fn VTIsHardwareDecodeSupported(codec: u32) -> bool; }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range { Video, Full }

/// Query the platform only. This is not permission to advertise a codec, queue
/// or Vulkan Video extension before its sessions, DPB and submissions exist.
pub fn hardware_h264() -> bool {
    unsafe { VTIsHardwareDecodeSupported(u32::from_be_bytes(*b"avc1")) }
}

/// The retained decoder output. No locks or base addresses are used to read
/// pixels. Color matrix, primaries and transfer attachments remain on this
/// retained CVPixelBuffer; a later conversion must inspect them rather than
/// guess a matrix from resolution. Range is explicit in the NV12 fourcc.
pub struct Picture {
    buffer: *mut c_void,
    surface: *mut c_void,
    layout: Layout,
    range: Range,
}
// SAFETY: CVPixelBuffer references are thread-safe; this owner never mutates it.
unsafe impl Send for Picture {}
unsafe impl Sync for Picture {}
impl Drop for Picture { fn drop(&mut self) { unsafe { CVBufferRelease(self.buffer) }; } }
impl Picture {
    /// # Safety
    /// `buffer` must be a live, completed decoder-output CVPixelBuffer. The
    /// If the producer is VideoToolbox, its completion callback must have run. No
    /// producer may modify its pixels while this Picture or its imports live,
    /// except through an explicitly synchronized overwrite via copy_from_gpu.
    pub unsafe fn from_completed_buffer(buffer: *mut c_void) -> Result<Arc<Self>, &'static str> {
        if buffer.is_null() { return Err("null decoded picture"); }
        // SAFETY: the caller supplies a live completed CVPixelBuffer.
        unsafe {
            let range = match CVPixelBufferGetPixelFormatType(buffer) {
                x if x == u32::from_be_bytes(*b"420v") => Range::Video,
                x if x == u32::from_be_bytes(*b"420f") => Range::Full,
                _ => return Err("decoder output is not NV12"),
            };
            let surface = CVPixelBufferGetIOSurface(buffer);
            if surface.is_null() || CVPixelBufferGetPlaneCount(buffer) != 2
                || IOSurfaceGetPlaneCount(surface) != 2 { return Err("NV12 needs a two-plane IOSurface"); }
            let mut planes = [Plane { width: 0, height: 0, offset: 0, stride: 0 }; 2];
            for (i, p) in planes.iter_mut().enumerate() {
                if IOSurfaceGetBytesPerElementOfPlane(surface, i) != i + 1 { return Err("NV12 plane element size"); }
                *p = Plane { width: CVPixelBufferGetWidthOfPlane(buffer, i),
                    height: CVPixelBufferGetHeightOfPlane(buffer, i),
                    stride: CVPixelBufferGetBytesPerRowOfPlane(buffer, i),
                    offset: IOSurfaceGetOffsetOfPlane(surface, i) };
                if (p.width, p.height, p.stride) != (IOSurfaceGetWidthOfPlane(surface, i),
                    IOSurfaceGetHeightOfPlane(surface, i), IOSurfaceGetBytesPerRowOfPlane(surface, i)) {
                    return Err("CVPixelBuffer and IOSurface plane layouts disagree");
                }
            }
            let layout = Layout { width: CVPixelBufferGetWidth(buffer), height: CVPixelBufferGetHeight(buffer),
                allocation_size: IOSurfaceGetAllocSize(surface), planes }.validate()?;
            CVBufferRetain(buffer);
            Ok(Arc::new(Self { buffer, surface, layout, range }))
        }
    }

    /// The retained CVPixelBuffer, for reading color attachments. This pointer
    /// must not escape the lifetime of this Picture or be used to mutate pixels.
    pub fn layout(&self) -> Layout { self.layout }
    pub fn range(&self) -> Range { self.range }

    pub fn pixel_buffer(&self) -> *mut c_void { self.buffer }

    /// Wrap the real Metal plane extents, preserving separate chroma pitch and
    /// offset. Both textures retain this Picture through PlaneTextures.
    pub fn metal_planes(self: &Arc<Self>) -> Result<PlaneTextures, &'static str> {
        let make = |i: usize, format| {
            let p = self.layout.planes[i];
            // SAFETY: Picture retains the IOSurface, and its layout was checked.
            unsafe { iosurface::texture_plane(self.surface, format,
                u32::try_from(p.width).map_err(|_| "plane width")?,
                u32::try_from(p.height).map_err(|_| "plane height")?, i as u64, 1) }
                .ok_or("Metal rejected decoded plane")
        };
        Ok(PlaneTextures { picture: Arc::clone(self), textures: [make(0, 10)?, make(1, 30)?] })
    }
}

pub struct PlaneTextures {
    textures: [Texture; 2],
    picture: Arc<Picture>,
}

#[repr(C)]
struct ImportTexture { s_type: vk::StructureType, p_next: *const c_void, handle_type: u32, handle: *mut c_void }

/// Two separate dedicated single-plane imports. This deliberately does not
/// manufacture a Vulkan NV12 image or a sampler YCbCr conversion capability.
/// The consumer must issue the initial layout/ownership barriers before use.
/// PREINITIALIZED preserves the decoded contents through that first transition.
pub struct ImportedPlanes {
    driver: Arc<Driver>,
    textures: PlaneTextures,
    images: [vk::Image; 2],
    memories: [vk::DeviceMemory; 2],
}
impl ImportedPlanes {
    pub fn picture(&self) -> &Arc<Picture> { &self.textures.picture }
    pub fn images(&self) -> [vk::Image; 2] { self.images }
}
impl Drop for ImportedPlanes {
    fn drop(&mut self) {
        // SAFETY: import() requires the owner to retire all GPU consumers first.
        unsafe {
            for image in self.images { if image != vk::Image::null() { self.driver.device.destroy_image(image, None); } }
            for memory in self.memories { if memory != vk::DeviceMemory::null() { self.driver.device.free_memory(memory, None); } }
        }
        // Metal textures drop after Vulkan memory, Picture after the textures.
    }
}
impl PlaneTextures {
    pub fn picture(&self) -> &Arc<Picture> { &self.picture }

    /// # Safety
    /// The caller must keep the returned owner alive until all Vulkan consumers
    /// have finished, including failed submissions, and issue the appropriate
    /// initial layout and external-producer visibility barriers before sampling.
    pub unsafe fn import(self, driver: Arc<Driver>) -> Result<ImportedPlanes, String> {
        let mut out = ImportedPlanes { driver, textures: self,
            images: [vk::Image::null(); 2], memories: [vk::DeviceMemory::null(); 2] };
        for i in 0..2 {
            let plane = out.textures.picture.layout.planes[i];
            let format = [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM][i];
            let mut query_ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::from_raw(iosurface::MTLTEXTURE));
            let query = vk::PhysicalDeviceImageFormatInfo2::default().push_next(&mut query_ext)
                .format(format).ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::LINEAR).usage(vk::ImageUsageFlags::SAMPLED);
            let mut external_props = vk::ExternalImageFormatProperties::default();
            let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
            unsafe { out.driver.instance.get_physical_device_image_format_properties2(out.driver.pd, &query, &mut props) }
                .map_err(|e| format!("plane {i} format query: {e:?}"))?;
            if !external_props.external_memory_properties.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE) {
                return Err(format!("plane {i}: driver does not support this texture import"));
            }
            let mut external = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::from_raw(iosurface::MTLTEXTURE));
            let info = vk::ImageCreateInfo::default().push_next(&mut external)
                .image_type(vk::ImageType::TYPE_2D).format(format)
                .extent(vk::Extent3D { width: plane.width as u32, height: plane.height as u32, depth: 1 })
                .mip_levels(1).array_layers(1).samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::LINEAR).usage(vk::ImageUsageFlags::SAMPLED)
                .initial_layout(vk::ImageLayout::PREINITIALIZED);
            // SAFETY: valid stack-owned create-info and enabled platform import.
            out.images[i] = unsafe { out.driver.device.create_image(&info, None) }.map_err(|e| format!("plane {i} image: {e:?}"))?;
            let layout = unsafe { out.driver.device.get_image_subresource_layout(out.images[i],
                vk::ImageSubresource { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, array_layer: 0 }) };
            if layout.row_pitch != plane.stride as u64 || layout.offset != 0 {
                return Err(format!("plane {i}: driver layout offset={} pitch={} differs from the imported texture's pitch={}",
                    layout.offset, layout.row_pitch, plane.stride));
            }
            let req = unsafe { out.driver.device.get_image_memory_requirements(out.images[i]) };
            let ty = out.driver.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .ok_or_else(|| format!("plane {i}: no device-local memory type"))?;
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(out.images[i]);
            let import = ImportTexture { s_type: vk::StructureType::from_raw(1_000_602_000),
                p_next: (&raw mut dedicated).cast(), handle_type: iosurface::MTLTEXTURE,
                handle: out.textures.textures[i].0 };
            let mut alloc = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty);
            alloc.p_next = (&raw const import).cast();
            out.memories[i] = unsafe { out.driver.device.allocate_memory(&alloc, None) }
                .map_err(|e| format!("plane {i} import: {e:?}"))?;
            unsafe { out.driver.device.bind_image_memory(out.images[i], out.memories[i], 0) }
                .map_err(|e| format!("plane {i} bind: {e:?}"))?;
        }
        Ok(out)
    }
}


impl Picture {
    /// Copy a completed decoder picture into this already imported allocation.
    /// This preserves the bound VkImage and never maps or reads pixel bytes.
    ///
    /// # Safety
    /// The caller must retire every earlier GPU/display reader of `self`, hold
    /// the target image's exclusive producer ownership, and prevent all other
    /// writers. No later reader may start until this function returns success.
    /// On failure, the target's contents are undefined and must not be signaled
    /// as a completed decode. Both pictures remain retained through completion.
    pub unsafe fn copy_from_gpu(self: &Arc<Self>, source: &Arc<Self>) -> Result<(), String> {
        if self.surface == source.surface { return Ok(()); }
        if self.layout.width != source.layout.width || self.layout.height != source.layout.height
            || self.range != source.range {
            return Err("decoded picture and bound target dimensions/range differ".into());
        }
        // Pitch and offsets may differ. Metal copies actual plane texels, not
        // inferred NV12 byte ranges, after each picture validated its layout.
        let make = |picture: &Picture, index: usize| {
            let p = picture.layout.planes[index];
            unsafe { iosurface::texture_plane(picture.surface, if index == 0 { 10 } else { 30 },
                u32::try_from(p.width).map_err(|_| "copy plane width")?,
                u32::try_from(p.height).map_err(|_| "copy plane height")?, index as u64, 0) }
                .ok_or("Metal rejected NV12 blit plane")
        };
        let src = [make(source, 0)?, make(source, 1)?];
        let dst = [make(self, 0)?, make(self, 1)?];
        #[repr(C)] #[derive(Clone, Copy)] struct Three { x: usize, y: usize, z: usize }
        #[link(name = "objc")]
        unsafe extern "C" {
            fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
            fn objc_msgSend();
            fn objc_autoreleasePoolPush() -> *mut c_void;
            fn objc_autoreleasePoolPop(pool: *mut c_void);
        }
        unsafe fn object(object: *mut c_void, selector: &std::ffi::CStr) -> *mut c_void {
            let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
                unsafe { std::mem::transmute(objc_msgSend as *const ()) };
            unsafe { send(object, sel_registerName(selector.as_ptr())) }
        }
        unsafe fn call(object: *mut c_void, selector: &std::ffi::CStr) {
            let send: unsafe extern "C" fn(*mut c_void, *mut c_void) =
                unsafe { std::mem::transmute(objc_msgSend as *const ()) };
            unsafe { send(object, sel_registerName(selector.as_ptr())) }
        }
        struct Pool(*mut c_void);
        impl Drop for Pool { fn drop(&mut self) { unsafe { objc_autoreleasePoolPop(self.0) } } }
        struct Queue(*mut c_void);
        impl Drop for Queue { fn drop(&mut self) { unsafe { call(self.0, c"release") } } }
        unsafe {
            let _pool = Pool(objc_autoreleasePoolPush());
            let device = object(dst[0].0, c"device");
            if device.is_null() { return Err("NV12 blit device unavailable".into()); }
            let queue = Queue(object(device, c"newCommandQueue"));
            if queue.0.is_null() { return Err("NV12 blit queue unavailable".into()); }
            let command = object(queue.0, c"commandBuffer");
            if command.is_null() { return Err("NV12 blit command unavailable".into()); }
            let encoder = object(command, c"blitCommandEncoder");
            if encoder.is_null() { return Err("NV12 blit encoder unavailable".into()); }
            let copy: unsafe extern "C" fn(*mut c_void, *mut c_void,
                *mut c_void, usize, usize, Three, Three, *mut c_void, usize, usize, Three) =
                std::mem::transmute(objc_msgSend as *const ());
            for index in 0..2 {
                let p = self.layout.planes[index];
                copy(encoder, sel_registerName(c"copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:".as_ptr()),
                    src[index].0, 0, 0, Three { x: 0, y: 0, z: 0 },
                    Three { x: p.width, y: p.height, z: 1 }, dst[index].0, 0, 0,
                    Three { x: 0, y: 0, z: 0 });
            }
            call(encoder, c"endEncoding");
            call(command, c"commit");
            call(command, c"waitUntilCompleted");
            let status: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize =
                std::mem::transmute(objc_msgSend as *const ());
            if status(command, sel_registerName(c"status".as_ptr())) != 4 {
                return Err("NV12 GPU copy did not complete".into());
            }
        }
        Ok(())
    }
}
