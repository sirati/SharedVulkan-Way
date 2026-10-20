//! Physical barrier recording for an owned video submission segment. Wire
//! resources are resolved against retained owners before native recording.
use crate::{driver::Driver, video_recording::Barrier};
use ash::vk::{self, Handle};
use sv_proto::{self as p, HandleType};
fn subresource(r: &p::VkImageSubresourceRange) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::from_raw(r.aspect_mask.0),
        base_mip_level: r.base_mip_level,
        level_count: r.level_count,
        base_array_layer: r.base_array_layer,
        layer_count: r.layer_count,
    }
}
/// # Safety
/// The command buffer is recording on this driver's queue family. `barrier`
/// has passed video_recording::lower and all resolved resources remain alive
/// through the final native completion fence. No logical VIDEO layout reaches
/// this function. The mapper returns actual handles from that retained plan.
pub(crate) unsafe fn record(
    driver: &Driver,
    command: vk::CommandBuffer,
    barrier: &Barrier,
    map: impl Fn(HandleType, u64) -> Result<u64, vk::Result>,
) -> Result<(), vk::Result> {
    match barrier {
        Barrier::Legacy(v) => {
            let memories: Vec<_> = v
                .p_memory_barriers
                .iter()
                .map(|b| {
                    vk::MemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::from_raw(b.src_access_mask.0))
                        .dst_access_mask(vk::AccessFlags::from_raw(b.dst_access_mask.0))
                })
                .collect();
            let buffers: Vec<_> = v
                .p_buffer_memory_barriers
                .iter()
                .map(|b| {
                    Ok(vk::BufferMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::from_raw(b.src_access_mask.0))
                        .dst_access_mask(vk::AccessFlags::from_raw(b.dst_access_mask.0))
                        .src_queue_family_index(b.src_queue_family_index)
                        .dst_queue_family_index(b.dst_queue_family_index)
                        .buffer(vk::Buffer::from_raw(map(HandleType::VkBuffer, b.buffer.0)?))
                        .offset(b.offset)
                        .size(b.size))
                })
                .collect::<Result<_, vk::Result>>()?;
            let images: Vec<_> = v
                .p_image_memory_barriers
                .iter()
                .map(|b| {
                    Ok(vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::from_raw(b.src_access_mask.0))
                        .dst_access_mask(vk::AccessFlags::from_raw(b.dst_access_mask.0))
                        .old_layout(vk::ImageLayout::from_raw(b.old_layout.0))
                        .new_layout(vk::ImageLayout::from_raw(b.new_layout.0))
                        .src_queue_family_index(b.src_queue_family_index)
                        .dst_queue_family_index(b.dst_queue_family_index)
                        .image(vk::Image::from_raw(map(HandleType::VkImage, b.image.0)?))
                        .subresource_range(subresource(&b.subresource_range)))
                })
                .collect::<Result<_, vk::Result>>()?;
            unsafe {
                driver.device.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::from_raw(v.src_stage_mask.0),
                    vk::PipelineStageFlags::from_raw(v.dst_stage_mask.0),
                    vk::DependencyFlags::from_raw(v.dependency_flags.0),
                    &memories,
                    &buffers,
                    &images,
                );
            }
        }
        Barrier::Sync2(v) => {
            let info = v
                .p_dependency_info
                .as_deref()
                .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
            let memories: Vec<_> = info
                .p_memory_barriers
                .iter()
                .map(|b| {
                    vk::MemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::from_raw(b.src_stage_mask.0))
                        .dst_stage_mask(vk::PipelineStageFlags2::from_raw(b.dst_stage_mask.0))
                        .src_access_mask(vk::AccessFlags2::from_raw(b.src_access_mask.0))
                        .dst_access_mask(vk::AccessFlags2::from_raw(b.dst_access_mask.0))
                })
                .collect();
            let buffers: Vec<_> = info
                .p_buffer_memory_barriers
                .iter()
                .map(|b| {
                    Ok(vk::BufferMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::from_raw(b.src_stage_mask.0))
                        .dst_stage_mask(vk::PipelineStageFlags2::from_raw(b.dst_stage_mask.0))
                        .src_access_mask(vk::AccessFlags2::from_raw(b.src_access_mask.0))
                        .dst_access_mask(vk::AccessFlags2::from_raw(b.dst_access_mask.0))
                        .src_queue_family_index(b.src_queue_family_index)
                        .dst_queue_family_index(b.dst_queue_family_index)
                        .buffer(vk::Buffer::from_raw(map(HandleType::VkBuffer, b.buffer.0)?))
                        .offset(b.offset)
                        .size(b.size))
                })
                .collect::<Result<_, vk::Result>>()?;
            let images: Vec<_> = info
                .p_image_memory_barriers
                .iter()
                .map(|b| {
                    Ok(vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::from_raw(b.src_stage_mask.0))
                        .dst_stage_mask(vk::PipelineStageFlags2::from_raw(b.dst_stage_mask.0))
                        .src_access_mask(vk::AccessFlags2::from_raw(b.src_access_mask.0))
                        .dst_access_mask(vk::AccessFlags2::from_raw(b.dst_access_mask.0))
                        .old_layout(vk::ImageLayout::from_raw(b.old_layout.0))
                        .new_layout(vk::ImageLayout::from_raw(b.new_layout.0))
                        .src_queue_family_index(b.src_queue_family_index)
                        .dst_queue_family_index(b.dst_queue_family_index)
                        .image(vk::Image::from_raw(map(HandleType::VkImage, b.image.0)?))
                        .subresource_range(subresource(&b.subresource_range)))
                })
                .collect::<Result<_, vk::Result>>()?;
            let dependency = vk::DependencyInfo::default()
                .dependency_flags(vk::DependencyFlags::from_raw(info.dependency_flags.0))
                .memory_barriers(&memories)
                .buffer_memory_barriers(&buffers)
                .image_memory_barriers(&images);
            unsafe {
                driver.device.cmd_pipeline_barrier2(command, &dependency);
            }
        }
    }
    Ok(())
}
