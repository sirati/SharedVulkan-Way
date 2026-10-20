//! One joined NV12 image backed by the completed picture's two-plane IOSurface.
//! The permitted KK import derives chroma from the same plane-0 texture. Two
//! unrelated single-plane VkImages are never presented as a decoded VkImage.
use std::{ffi::c_void, sync::Arc};
use ash::vk;
use crate::{driver::Driver, iosurface};
use super::macos::{Picture, PlaneTextures};

#[repr(C)]
struct ImportTexture {
    s_type: vk::StructureType,
    p_next: *const c_void,
    handle_type: u32,
    handle: *mut c_void,
}

pub struct Storage {
    backing: PlaneTextures,
}
impl Storage {
    pub fn picture(&self) -> &Arc<Picture> { self.backing.picture() }
}

/// Import into an existing exact NV12 image without replacing its handle.
/// Image creation must request sampled, LINEAR, dedicated MTLTEXTURE storage.
///
/// # Safety
/// `image` must be the live unbound driver's image with the exact picture
/// dimensions, NV12 format, one mip/layer/sample, no DISJOINT flag and offset0.
/// The caller owns the returned native memory and must free it before Storage,
/// after every producer/consumer has completed. This function never submits
/// work or substitutes for producer completion or external-ownership barriers.
pub unsafe fn allocate_for_image(
    driver: &Driver,
    image: vk::Image,
    picture: Arc<Picture>,
) -> Result<(vk::DeviceMemory, Storage, u32), String> {
    let backing = picture.metal_planes()?;
    let req = unsafe { driver.device.get_image_memory_requirements(image) };
    let ty = driver.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
        .ok_or("no device-local type for joined NV12 import")?;
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let import = ImportTexture {
        s_type: vk::StructureType::from_raw(1_000_602_000),
        p_next: (&raw mut dedicated).cast(),
        handle_type: iosurface::MTLTEXTURE,
        handle: backing.first_handle(),
    };
    let mut alloc = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty);
    alloc.p_next = (&raw const import).cast();
    let memory = unsafe { driver.device.allocate_memory(&alloc, None) }
        .map_err(|error| format!("joined NV12 memory import: {error:?}"))?;
    // Actual imported plane offsets/pitches are authoritative only after bind.
    // A pre-import synthetic image layout is not a CVPixelBuffer layout.
    Ok((memory, Storage { backing }, ty))
}

pub struct Image {
    driver: Arc<Driver>,
    storage: Option<Storage>,
    image: vk::Image,
    memory: vk::DeviceMemory,
}
impl Image {
    pub fn image(&self) -> vk::Image { self.image }
    pub fn picture(&self) -> &Arc<Picture> { self.storage.as_ref().unwrap().picture() }
    /// # Safety
    /// Retain this owner until every Vulkan reader finishes and issue the
    /// appropriate initial layout and external-producer visibility barriers.
    pub unsafe fn import(driver: Arc<Driver>, picture: Arc<Picture>) -> Result<Self, String> {
        let layout = picture.layout();
        let format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
        let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::from_raw(iosurface::MTLTEXTURE));
        let query = vk::PhysicalDeviceImageFormatInfo2::default().push_next(&mut ext)
            .format(format).ty(vk::ImageType::TYPE_2D).tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED);
        let mut external = vk::ExternalImageFormatProperties::default();
        let mut props = vk::ImageFormatProperties2::default().push_next(&mut external);
        unsafe { driver.instance.get_physical_device_image_format_properties2(driver.pd, &query, &mut props) }
            .map_err(|error| format!("joined NV12 import query: {error:?}"))?;
        let features = external.external_memory_properties.external_memory_features;
        let handles = external.external_memory_properties.compatible_handle_types;
        if !features.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
            || !handles.contains(vk::ExternalMemoryHandleTypeFlags::from_raw(iosurface::MTLTEXTURE))
            || props.image_format_properties.max_extent.width < layout.width as u32
            || props.image_format_properties.max_extent.height < layout.height as u32 {
            return Err("joined NV12 dimensions/import support refused".into());
        }
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::from_raw(iosurface::MTLTEXTURE));
        let info = vk::ImageCreateInfo::default().push_next(&mut external)
            .image_type(vk::ImageType::TYPE_2D).format(format)
            .extent(vk::Extent3D { width: u32::try_from(layout.width).map_err(|_| "NV12 width")?,
                height: u32::try_from(layout.height).map_err(|_| "NV12 height")?, depth: 1 })
            .mip_levels(1).array_layers(1).samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR).usage(vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::PREINITIALIZED);
        let image = unsafe { driver.device.create_image(&info, None) }
            .map_err(|error| format!("joined NV12 image creation: {error:?}"))?;
        let mut output = Self { driver, image, memory: vk::DeviceMemory::null(), storage: None };
        let (memory, storage, _) = unsafe { allocate_for_image(&output.driver, image, picture) }?;
        output.memory = memory;
        output.storage = Some(storage);
        unsafe { output.driver.device.bind_image_memory(image, memory, 0) }
            .map_err(|error| format!("joined NV12 bind: {error:?}"))?;
        let expected = output.picture().layout();
        for index in 0..2 {
            let got = unsafe { output.driver.device.get_image_subresource_layout(image,
                vk::ImageSubresource { aspect_mask: if index == 0 { vk::ImageAspectFlags::PLANE_0 }
                    else { vk::ImageAspectFlags::PLANE_1 }, mip_level: 0, array_layer: 0 }) };
            let plane = expected.planes[index];
            if got.offset != plane.offset as u64 || got.row_pitch != plane.stride as u64 {
                return Err("bound joined NV12 subresource layout differs from IOSurface".into());
            }
        }
        Ok(output)
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            self.driver.device.destroy_image(self.image, None);
            if self.memory != vk::DeviceMemory::null() { self.driver.device.free_memory(self.memory, None); }
        }
        // Storage and retained CVPixelBuffer drop only after image and memory.
    }
}
