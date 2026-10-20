//! Exact native image/allocation owner for a logical Vulkan decode target.
//! The backing VkImage is sampled NV12; the server, rather than KK, executes
//! logical VIDEO_DECODE_DST/DPB operations into this allocation on Metal.
use std::sync::{Arc, Mutex};
use ash::vk;
use crate::{driver::Driver, iosurface, video_nv12::{joined, macos::{Picture, Range}}};

struct Allocation {
    memory: vk::DeviceMemory,
    storage: joined::Storage,
}
pub(crate) struct Image {
    driver: Arc<Driver>,
    image: vk::Image,
    width: u32,
    height: u32,
    allocation: Mutex<Option<Allocation>>,
    /// Color/range attachments remain on the real completed decoder picture.
    /// Allocation fourcc alone is not the interpretation of copied NV12 bytes.
    producer: Mutex<Option<Arc<Picture>>>,
}
impl Image {
    pub(crate) fn create(driver: Arc<Driver>, width: u32, height: u32) -> Result<Arc<Self>, vk::Result> {
        if width == 0 || height == 0 || width > 4096 || height > 2304 {
            return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
        }
        let format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
        let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::from_raw(iosurface::MTLTEXTURE));
        let query = vk::PhysicalDeviceImageFormatInfo2::default().push_next(&mut ext)
            .format(format).ty(vk::ImageType::TYPE_2D).tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED);
        let mut external = vk::ExternalImageFormatProperties::default();
        let mut props = vk::ImageFormatProperties2::default().push_next(&mut external);
        unsafe { driver.instance.get_physical_device_image_format_properties2(driver.pd, &query, &mut props) }?;
        let image_props = props.image_format_properties;
        if !external.external_memory_properties.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
            || !external.external_memory_properties.compatible_handle_types.contains(vk::ExternalMemoryHandleTypeFlags::from_raw(iosurface::MTLTEXTURE))
            || image_props.max_extent.width < width
            || image_props.max_extent.height < height {
            return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
        }
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::from_raw(iosurface::MTLTEXTURE));
        let info = vk::ImageCreateInfo::default().push_next(&mut external)
            .image_type(vk::ImageType::TYPE_2D).format(format)
            .extent(vk::Extent3D { width, height, depth: 1 }).mip_levels(1).array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1).tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED).initial_layout(vk::ImageLayout::PREINITIALIZED);
        let image = unsafe { driver.device.create_image(&info, None) }?;
        Ok(Arc::new(Self { driver, image, width, height,
            allocation: Mutex::new(None), producer: Mutex::new(None) }))
    }
    pub(crate) fn image(&self) -> vk::Image { self.image }
    pub(crate) fn requirements(&self) -> vk::MemoryRequirements {
        unsafe { self.driver.device.get_image_memory_requirements(self.image) }
    }
    /// Allocate once for the exact image; ownership stays here until both
    /// logical image and memory owners, and every submitted reader, release it.
    pub(crate) fn allocate(&self) -> Result<vk::DeviceMemory, vk::Result> {
        let mut allocation = self.allocation.lock().unwrap_or_else(|e| e.into_inner());
        if allocation.is_some() { return Err(vk::Result::ERROR_TOO_MANY_OBJECTS); }
        // Vulkan images have no inherent full/narrow interpretation. The source
        // stream attachments and VkSamplerYcbcrConversion carry it at use time.
        let picture = Picture::allocate_target(self.width as usize, self.height as usize, Range::Video)
            .map_err(|_| vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
        let (memory, storage, _) = unsafe { joined::allocate_for_image(&self.driver, self.image, picture) }
            .map_err(|_| vk::Result::ERROR_INVALID_EXTERNAL_HANDLE)?;
        *allocation = Some(Allocation { memory, storage });
        Ok(memory)
    }
    pub(crate) fn picture(&self) -> Result<Arc<Picture>, vk::Result> {
        self.allocation.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
            .map(|a| Arc::clone(a.storage.picture())).ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)
    }
    pub(crate) fn validate_bound(&self) -> Result<(), vk::Result> {
        let picture = self.picture()?;
        let layout = picture.layout();
        for index in 0..2 {
            let got = unsafe { self.driver.device.get_image_subresource_layout(self.image,
                vk::ImageSubresource { aspect_mask: if index == 0 { vk::ImageAspectFlags::PLANE_0 }
                    else { vk::ImageAspectFlags::PLANE_1 }, mip_level: 0, array_layer: 0 }) };
            if got.offset != layout.planes[index].offset as u64 || got.row_pitch != layout.planes[index].stride as u64 {
                return Err(vk::Result::ERROR_INVALID_EXTERNAL_HANDLE);
            }
        }
        Ok(())
    }
    /// # Safety
    /// Caller has completed the image's Vulkan release/ownership barriers and
    /// every prior reader, owns the producer role exclusively, and prevents
    /// future native readers until copy completion/acquire barriers.
    pub(crate) unsafe fn write_completed(&self, source: Arc<Picture>) -> Result<(), vk::Result> {
        let target = self.picture()?;
        unsafe { target.copy_from_gpu(&source) }.map_err(|_| vk::Result::ERROR_DEVICE_LOST)?;
        *self.producer.lock().unwrap_or_else(|e| e.into_inner()) = Some(source);
        Ok(())
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        unsafe { self.driver.device.destroy_image(self.image, None); }
        if let Some(allocation) = self.allocation.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
            unsafe { self.driver.device.free_memory(allocation.memory, None); }
            drop(allocation);
        }
    }
}
