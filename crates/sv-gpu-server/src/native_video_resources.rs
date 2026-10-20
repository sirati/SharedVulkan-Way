//! Canonical session ownership of logical video resources. The object table
//! uses the real image/memory handles, but this owner destroys each exactly
//! once after application handles and accepted submissions release them.
use crate::native_video_image::Image;
use ash::vk;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

pub(crate) struct View {
    pub image: Arc<Image>,
    pub layer: u32,
    pub native: Option<vk::ImageView>,
}
#[derive(Default)]
pub(crate) struct Resources {
    pub sources: crate::native_video_source::SourceRegistry,
    pub images: HashMap<u64, Arc<Image>>,
    pub memories: HashMap<u64, Arc<Image>>,
    pub views: HashMap<u64, View>,
    pub exportable: HashSet<u64>,
    pub leases: HashMap<u64, crate::native_video_shared::Lease>,
}
impl Resources {
    pub fn owns_image(&self, raw: vk::Image) -> bool {
        self.images
            .values()
            .chain(self.memories.values())
            .any(|v| v.image() == raw)
            || self.views.values().any(|v| v.image.image() == raw)
    }
    pub fn owns_memory(&self, raw: vk::DeviceMemory) -> bool {
        self.memories
            .values()
            .any(|v| v.allocated_memory() == Some(raw))
    }
    /// Called by Objects only after native views are destroyed and the device
    /// is idle. Dropping these references preserves separately retained jobs.
    pub fn clear(&mut self) {
        self.sources.clear();
        self.exportable.clear();
        self.leases.clear();
        self.views.clear();
        self.images.clear();
        self.memories.clear();
    }
}

use crate::server::GpuServer;
use ash::vk::Handle;
use sv_proto::{self as p, HandleType};
impl GpuServer {
    pub(crate) fn native_create_video_image(
        &mut self,
        a: &p::VkCreateImageArgs,
    ) -> Option<sv_wire::Result<p::VkCreateImageReply>> {
        let c = a.p_create_info.as_deref()?;
        let video =
            vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR;
        let import_only = c.usage.0 & video.as_raw() == 0
            && c.format.0 == vk::Format::G8_B8R8_2PLANE_420_UNORM.as_raw()
            && c.p_next.iter().any(|link| {
                matches!(link,
                p::PNextLink::VkExternalMemoryImageCreateInfo(info)
                if info.handle_types.0 == crate::shared::HOST_ALLOCATION)
            });
        if c.usage.0 & video.as_raw() == 0 && !import_only {
            return None;
        }
        if !self
            .shared
            .native_video
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .available()
        {
            return None;
        }
        let result = (|| -> Result<(), vk::Result> {
            let supported = video
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST;
            if (c.flags.0 != 0 && c.flags.0 != vk::ImageCreateFlags::MUTABLE_FORMAT.as_raw())
                || c.image_type.0 != vk::ImageType::TYPE_2D.as_raw()
                || c.format.0 != vk::Format::G8_B8R8_2PLANE_420_UNORM.as_raw()
                || c.extent.depth != 1
                || c.mip_levels != 1
                || c.array_layers != 1
                || c.samples.0 != vk::SampleCountFlags::TYPE_1.as_raw() as i32
                || c.tiling.0 != vk::ImageTiling::LINEAR.as_raw()
                || !supported.contains(vk::ImageUsageFlags::from_raw(c.usage.0))
                || c.sharing_mode.0 != vk::SharingMode::EXCLUSIVE.as_raw()
                || c.queue_family_index_count != 0
                || !c.p_queue_family_indices.is_empty()
                || c.initial_layout.0 != vk::ImageLayout::UNDEFINED.as_raw()
            {
                return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
            }
            let mut profiles = None;
            let mut formats = None;
            let mut external = false;
            let mut exportable = false;
            for link in &c.p_next {
                match link {
                    p::PNextLink::VkVideoProfileListInfoKHR(list) if profiles.is_none() => {
                        profiles = Some(list);
                    }
                    p::PNextLink::VkImageFormatListCreateInfo(list)
                        if formats.is_none()
                            && list.p_next.is_empty()
                            && list.view_format_count as usize == list.p_view_formats.len() =>
                    {
                        formats = Some(
                            list.p_view_formats
                                .iter()
                                .map(|f| vk::Format::from_raw(f.0))
                                .collect::<Vec<_>>(),
                        );
                    }
                    p::PNextLink::VkExternalMemoryImageCreateInfo(info)
                        if !external
                            && info.p_next.is_empty()
                            && (info.handle_types.0 == 0
                                || info.handle_types.0 == crate::shared::HOST_ALLOCATION
                                || info.handle_types.0
                                    == vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT.as_raw()) =>
                    {
                        external = true;
                        exportable = info.handle_types.0 != 0;
                    }
                    _ => return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED),
                }
            }
            if let Some(list) = profiles {
                if !list.p_next.is_empty() || list.profile_count != 1 || list.p_profiles.len() != 1
                {
                    return Err(vk::Result::ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR);
                }
                let profile = crate::native_video_dispatch::profile(&list.p_profiles[0])?;
                let state = self
                    .shared
                    .native_video
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let _ = state.video.capabilities(profile)?;
            } else if !import_only {
                return Err(vk::Result::ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR);
            }
            {
                let state = self
                    .shared
                    .native_video
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !state.video.supports_image_extent(vk::Extent2D {
                    width: c.extent.width,
                    height: c.extent.height,
                }) {
                    return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
                }
            }
            let wire = a.p_image.ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?.0;
            if wire == 0
                || self
                    .shared
                    .objects()
                    .native_video
                    .images
                    .contains_key(&wire)
            {
                return Err(vk::Result::ERROR_INITIALIZATION_FAILED);
            }
            let image = if c.flags.0 == vk::ImageCreateFlags::MUTABLE_FORMAT.as_raw() {
                Image::create_config(
                    Arc::clone(&self.shared.driver),
                    c.extent.width,
                    c.extent.height,
                    vk::ImageCreateFlags::from_raw(c.flags.0),
                    vk::ImageUsageFlags::from_raw(c.usage.0) & !video,
                    formats
                        .as_deref()
                        .ok_or(vk::Result::ERROR_FORMAT_NOT_SUPPORTED)?,
                )?
            } else {
                if formats.is_some()
                    || c.usage.0 & !(video | vk::ImageUsageFlags::SAMPLED).as_raw() != 0
                {
                    return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
                }
                Image::create(
                    Arc::clone(&self.shared.driver),
                    c.extent.width,
                    c.extent.height,
                )?
            };
            let mut objects = self.shared.objects();
            objects.insert(wire, HandleType::VkImage, image.image());
            objects.shared_images.insert(image.image().as_raw());
            objects.native_video.images.insert(wire, image);
            if exportable {
                objects.native_video.exportable.insert(wire);
            }
            Ok(())
        })();
        Some(Ok(p::VkCreateImageReply {
            result: p::VkResult(result.as_ref().err().map_or(0, |e| e.as_raw())),
            p_image: result.ok().and(a.p_image),
        }))
    }
    pub(crate) fn native_allocate_video_image(
        &mut self,
        a: &p::VkAllocateMemoryArgs,
    ) -> Option<sv_wire::Result<p::VkAllocateMemoryReply>> {
        let c = a.p_allocate_info.as_deref()?;
        let dedicated = c.p_next.iter().find_map(|link| match link {
            p::PNextLink::VkMemoryDedicatedAllocateInfo(d) => Some(d),
            _ => None,
        })?;
        let image = self
            .shared
            .objects()
            .native_video
            .images
            .get(&dedicated.image.0)
            .cloned()?;
        let result = (|| -> Result<(), vk::Result> {
            let wire = a.p_memory.ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?.0;
            if wire == 0
                || dedicated.buffer.0 != 0
                || !dedicated.p_next.is_empty()
                || !c.p_next.iter().all(|link| match link {
                    p::PNextLink::VkMemoryDedicatedAllocateInfo(info) => {
                        std::ptr::eq(info, dedicated)
                    }
                    p::PNextLink::VkExportMemoryAllocateInfo(info) => {
                        info.p_next.is_empty() && info.handle_types.0 == 0
                    }
                    _ => false,
                })
                || c.p_next
                    .iter()
                    .filter(|link| matches!(link, p::PNextLink::VkExportMemoryAllocateInfo(_)))
                    .count()
                    > 1
                || c.memory_type_index != 0
                || c.allocation_size < image.requirements().size
                || self.shared.objects().memory(wire).is_some()
            {
                return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
            }
            let memory = image.allocate()?;
            let mut objects = self.shared.objects();
            objects.insert_memory(
                wire,
                crate::table::MemoryRange {
                    memory,
                    base: 0,
                    len: c.allocation_size,
                },
            );
            objects.native_video.memories.insert(wire, image);
            Ok(())
        })();
        Some(Ok(p::VkAllocateMemoryReply {
            result: p::VkResult(result.as_ref().err().map_or(0, |e| e.as_raw())),
            p_memory: result.ok().and(a.p_memory),
        }))
    }
}
impl GpuServer {
    pub(crate) fn native_free_video_memory(
        &mut self,
        a: &p::VkFreeMemoryArgs,
    ) -> Option<sv_wire::Result<()>> {
        let owner = {
            let mut objects = self.shared.objects();
            let owner = objects.native_video.memories.remove(&a.memory.0)?;
            let lease = objects.native_video.leases.remove(&a.memory.0);
            objects.remove(a.memory.0);
            (owner, lease)
        };
        drop(owner);
        Some(Ok(()))
    }
    pub(crate) fn native_destroy_video_image(
        &mut self,
        a: &p::VkDestroyImageArgs,
    ) -> Option<sv_wire::Result<()>> {
        let owner = {
            let mut objects = self.shared.objects();
            let owner = objects.native_video.images.remove(&a.image.0)?;
            objects.native_video.exportable.remove(&a.image.0);
            objects.shared_images.remove(&owner.image().as_raw());
            objects.remove(a.image.0);
            owner
        };
        drop(owner);
        Some(Ok(()))
    }
    fn bind_video_image(
        &self,
        image_wire: u64,
        memory_wire: u64,
        offset: u64,
    ) -> Option<Result<(), vk::Result>> {
        let objects = self.shared.objects();
        let image = objects.native_video.images.get(&image_wire).cloned()?;
        let memory = objects.native_video.memories.get(&memory_wire).cloned();
        drop(objects);
        Some((|| {
            let memory = memory.ok_or(vk::Result::ERROR_INVALID_EXTERNAL_HANDLE)?;
            if !Arc::ptr_eq(&image, &memory) {
                return Err(vk::Result::ERROR_INVALID_EXTERNAL_HANDLE);
            }
            image.bind(
                memory
                    .allocated_memory()
                    .ok_or(vk::Result::ERROR_INVALID_EXTERNAL_HANDLE)?,
                offset,
            )?;
            image.validate_bound()
        })())
    }
    pub(crate) fn native_bind_video_image(
        &self,
        a: &p::VkBindImageMemoryArgs,
    ) -> Option<sv_wire::Result<p::VkBindImageMemoryReply>> {
        self.bind_video_image(a.image.0, a.memory.0, a.memory_offset)
            .map(|result| {
                Ok(p::VkBindImageMemoryReply {
                    result: p::VkResult(result.err().map_or(0, |e| e.as_raw())),
                })
            })
    }
    pub(crate) fn native_bind_video_images(
        &self,
        a: &p::VkBindImageMemory2Args,
    ) -> Option<sv_wire::Result<p::VkBindImageMemory2Reply>> {
        let objects = self.shared.objects();
        if !a
            .p_bind_infos
            .iter()
            .any(|i| objects.native_video.images.contains_key(&i.image.0))
        {
            return None;
        }
        // Mixed native/owned batches need one validated atomic dispatch plan.
        // Do not silently bind an owned subset and forward the rest.
        let valid = a.bind_info_count as usize == a.p_bind_infos.len()
            && a.p_bind_infos.iter().all(|i| {
                i.p_next.is_empty()
                    && i.memory_offset == 0
                    && objects
                        .native_video
                        .images
                        .get(&i.image.0)
                        .zip(objects.native_video.memories.get(&i.memory.0))
                        .is_some_and(|(image, memory)| {
                            Arc::ptr_eq(image, memory) && !image.is_bound()
                        })
            });
        drop(objects);
        let result = if !valid {
            Err(vk::Result::ERROR_INVALID_EXTERNAL_HANDLE)
        } else {
            a.p_bind_infos.iter().try_for_each(|i| {
                self.bind_video_image(i.image.0, i.memory.0, i.memory_offset)
                    .unwrap_or(Err(vk::Result::ERROR_INVALID_EXTERNAL_HANDLE))
            })
        };
        Some(Ok(p::VkBindImageMemory2Reply {
            result: p::VkResult(result.err().map_or(0, |e| e.as_raw())),
        }))
    }
}
impl GpuServer {
    pub(crate) fn native_create_video_view(
        &mut self,
        a: &p::VkCreateImageViewArgs,
    ) -> Option<sv_wire::Result<p::VkCreateImageViewReply>> {
        if self.native_queue_worker {
            return None;
        }
        let c = a.p_create_info.as_deref()?;
        let image = self
            .shared
            .objects()
            .native_video
            .images
            .get(&c.image.0)
            .cloned()?;
        let wire = match a.p_view {
            Some(v) if v.0 != 0 => v.0,
            _ => {
                return Some(Ok(p::VkCreateImageViewReply {
                    result: p::VkResult(vk::Result::ERROR_INITIALIZATION_FAILED.as_raw()),
                    p_view: None,
                }));
            }
        };
        let mut args = a.clone();
        let mut usage = None;
        let info = args.p_create_info.as_deref_mut().unwrap();
        for link in &mut info.p_next {
            if let p::PNextLink::VkImageViewUsageCreateInfo(v) = link {
                if usage.is_some() || !v.p_next.is_empty() {
                    return Some(Ok(p::VkCreateImageViewReply {
                        result: p::VkResult(vk::Result::ERROR_FORMAT_NOT_SUPPORTED.as_raw()),
                        p_view: None,
                    }));
                }
                usage = Some(v.usage.0);
                let video = (vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
                    | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR)
                    .as_raw();
                v.usage.0 &= !video;
            }
        }
        let layer = info.subresource_range.base_array_layer;
        let video = (vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
            | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR)
            .as_raw();
        let video_only = usage.is_some_and(|v| v != 0 && v & !video == 0);
        if video_only {
            if info.flags.0 != 0
                || info.view_type.0 != vk::ImageViewType::TYPE_2D.as_raw()
                || info.format.0 != vk::Format::G8_B8R8_2PLANE_420_UNORM.as_raw()
                || info.subresource_range.aspect_mask.0 != vk::ImageAspectFlags::COLOR.as_raw()
                || layer != 0
                || info.subresource_range.layer_count != 1
                || info.subresource_range.base_mip_level != 0
                || info.subresource_range.level_count != 1
                || info
                    .p_next
                    .iter()
                    .any(|l| !matches!(l, p::PNextLink::VkImageViewUsageCreateInfo(_)))
            {
                return Some(Ok(p::VkCreateImageViewReply {
                    result: p::VkResult(vk::Result::ERROR_FORMAT_NOT_SUPPORTED.as_raw()),
                    p_view: None,
                }));
            }
            self.shared.objects().native_video.views.insert(
                wire,
                View {
                    image,
                    layer,
                    native: None,
                },
            );
            return Some(Ok(p::VkCreateImageViewReply {
                result: p::VkResult(0),
                p_view: a.p_view,
            }));
        }
        if info.flags.0 != 0
            || info.view_type.0 != vk::ImageViewType::TYPE_2D.as_raw()
            || layer != 0
            || info.subresource_range.layer_count != 1
            || info.subresource_range.base_mip_level != 0
            || info.subresource_range.level_count != 1
            || !image.contract().allows_view(
                vk::Format::from_raw(info.format.0),
                vk::ImageAspectFlags::from_raw(info.subresource_range.aspect_mask.0),
            )
        {
            return Some(Ok(p::VkCreateImageViewReply {
                result: p::VkResult(vk::Result::ERROR_FORMAT_NOT_SUPPORTED.as_raw()),
                p_view: None,
            }));
        }
        // The original wire metadata and YCbCr conversion survive. Only the
        // logical video usage is removed before the real sampled view call.
        use sv_proto::Server;
        let mut replay = GpuServer::new(Arc::clone(&self.shared));
        replay.native_queue_worker = true;
        let result = sv_gpu_entry::Forward::new(replay).create_image_view(args);
        Some(result.and_then(|reply| {
            if reply.result.0 == 0 {
                let mut objects = self.shared.objects();
                let native = objects
                    .get::<vk::ImageView>(HandleType::VkImageView, wire)
                    .map_err(|_| sv_wire::WireError::UnknownCommand { opcode: 0 })?;
                objects.native_video.views.insert(
                    wire,
                    View {
                        image,
                        layer,
                        native: Some(native),
                    },
                );
            }
            Ok(reply)
        }))
    }
    pub(crate) fn native_destroy_video_view(
        &mut self,
        a: &p::VkDestroyImageViewArgs,
    ) -> Option<sv_wire::Result<()>> {
        let view = {
            let mut objects = self.shared.objects();
            let view = objects.native_video.views.remove(&a.image_view.0)?;
            objects.remove(a.image_view.0);
            view
        };
        if let Some(native) = view.native {
            unsafe {
                self.shared.driver.device.destroy_image_view(native, None);
            }
        }
        drop(view);
        Some(Ok(()))
    }
}
