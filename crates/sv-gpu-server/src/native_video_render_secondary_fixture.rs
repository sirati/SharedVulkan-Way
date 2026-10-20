//! Fixed native rendering and secondary replay oracle. Feature-only child of
//! native_video_mixed_fixture. No shaders, draw loops, submits, waits or Drop.
//! Every owner survives until the existing full-graph retirement controller.
use super::*;
const RENDER_IMAGE: u64 = 200;
const RENDER_VIEW: u64 = 201;
const READBACK: u64 = 202;
const SECONDARY_POOL: u64 = 203;
const SECONDARY: u64 = 204;
const READBACK_BYTES: u64 = 64;
const PARTITION: u64 = 128 * 1024;
const MARKER: u32 = 0x3917ac65;
const INITIAL: u8 = 0xcd;
const IMAGE_ALLOCATION_LIMIT: u64 = 16 * 1024 * 1024;
type Rpc = sv_gpu_entry::Forward<GpuServer>;

pub(super) struct Prepared {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    readback: vk::Buffer,
    offset: u64,
    image_bytes: u64,
    buffer_bytes: u64,
    pool_created: bool,
    secondary_allocated: bool,
    mapped: bool,
    reserved_ids: bool,
    generation: u64,
}
impl Prepared {
    pub(super) fn prepare(rpc: &mut Rpc, previous_arena_end: u64) -> Result<Self, String> {
        let mut value = Self {
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            readback: vk::Buffer::null(),
            offset: 0,
            image_bytes: 0,
            buffer_bytes: 0,
            pool_created: false,
            secondary_allocated: false,
            mapped: false,
            reserved_ids: false,
            generation: 0,
        };
        if let Err(error) = value.prepare_inner(rpc, previous_arena_end) {
            // Preparation has never submitted a command or exposed native work.
            let cleanup = unsafe { value.cleanup_after_retirement(rpc) };
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; render/secondary preaccept cleanup: {cleanup}"),
            });
        }
        Ok(value)
    }
    fn prepare_inner(&mut self, rpc: &mut Rpc, previous_arena_end: u64) -> Result<(), String> {
        let shared = Arc::clone(&rpc.host().shared);
        let driver = &shared.driver;
        let dynamic_rendering = driver.features.p_next.iter().any(|link| match link {
            p::PNextLink::VkPhysicalDeviceVulkan13Features(features) => features.dynamic_rendering,
            p::PNextLink::VkPhysicalDeviceDynamicRenderingFeatures(features) => {
                features.dynamic_rendering
            }
            _ => false,
        });
        if !dynamic_rendering {
            return Err("render fixture requires enabled dynamicRendering".into());
        }
        for id in [
            RENDER_IMAGE,
            RENDER_VIEW,
            READBACK,
            SECONDARY_POOL,
            SECONDARY,
        ] {
            if shared.objects().raw(id).is_some() {
                return Err("render fixture wire ID collision".into());
            }
        }
        self.reserved_ids = true;
        if shared.fns.vk_cmd_begin_rendering.is_none()
            || shared.fns.vk_cmd_end_rendering.is_none()
            || shared.fns.vk_cmd_copy_image_to_buffer.is_none()
            || shared.fns.vk_cmd_execute_commands.is_none()
            || shared.fns.vk_cmd_fill_buffer.is_none()
            || shared.fns.vk_cmd_copy_buffer.is_none()
            || shared.fns.vk_cmd_pipeline_barrier.is_none()
        {
            return Err("render fixture native entrypoint unavailable".into());
        }
        let formats = unsafe {
            driver
                .instance
                .get_physical_device_format_properties(driver.pd, vk::Format::R8G8B8A8_UNORM)
        };
        if !formats.optimal_tiling_features.contains(
            vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::TRANSFER_SRC,
        ) {
            return Err("RGBA8 render/copy format unsupported".into());
        }
        self.image = native(unsafe {
            driver.device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .extent(vk::Extent3D {
                        width: 1,
                        height: 1,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                    )
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        })?;
        let req = unsafe { driver.device.get_image_memory_requirements(self.image) };
        if req.size == 0 || req.size > IMAGE_ALLOCATION_LIMIT {
            return Err("render image allocation exceeds fixed 16MiB bound".into());
        }
        self.image_bytes = req.size;
        let memory_type = driver
            .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .ok_or("render image has no device-local memory type")?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(self.image);
        self.memory = native(unsafe {
            driver.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .push_next(&mut dedicated)
                    .allocation_size(req.size)
                    .memory_type_index(memory_type),
                None,
            )
        })?;
        native(unsafe { driver.device.bind_image_memory(self.image, self.memory, 0) })?;
        self.view = native(unsafe {
            driver.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(self.image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        })?;
        let mut external = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(shared.arena.external_handle_type);
        self.readback = native(unsafe {
            driver.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .push_next(&mut external)
                    .size(READBACK_BYTES)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        })?;
        let req = unsafe { driver.device.get_buffer_memory_requirements(self.readback) };
        let align = req.alignment.max(1);
        self.offset = PARTITION
            .checked_add(align - 1)
            .map(|n| n / align * align)
            .ok_or("readback alignment overflow")?;
        self.buffer_bytes = req.size;
        self.ensure_partition_after(previous_arena_end)?;
        if req.size < READBACK_BYTES
            || req.memory_type_bits & (1 << shared.arena.memory_type) == 0
            || self
                .offset
                .checked_add(req.size)
                .is_none_or(|end| end > shared.arena.len)
        {
            return Err("render readback incompatible with fixed original arena partition".into());
        }
        // The parent supplied the actual upper bound of existing allocations.
        // Check it before binding or initializing our distinct arena partition.
        native(unsafe {
            driver
                .device
                .bind_buffer_memory(self.readback, shared.arena.memory, self.offset)
        })?;
        shared
            .arena
            .region
            .write_bytes(self.offset, &[INITIAL; READBACK_BYTES as usize])
            .map_err(|e| e.to_string())?;
        {
            let mut objects = shared.objects();
            objects.insert(RENDER_IMAGE, HandleType::VkImage, self.image);
            objects.insert(RENDER_VIEW, HandleType::VkImageView, self.view);
            objects.insert(READBACK, HandleType::VkBuffer, self.readback);
        }
        self.mapped = true;
        checked(
            wire(rpc.create_command_pool(p::VkCreateCommandPoolArgs {
                device: p::VkDevice(DEVICE),
                p_command_pool: Some(p::VkCommandPool(SECONDARY_POOL)),
                p_create_info: Some(Box::new(p::VkCommandPoolCreateInfo {
                    flags: p::VkCommandPoolCreateFlags(
                        vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER.as_raw(),
                    ),
                    queue_family_index: driver.queue_family,
                    ..Default::default()
                })),
            }))?
            .result,
        )?;
        self.pool_created = true;
        checked(
            wire(
                rpc.allocate_command_buffers(p::VkAllocateCommandBuffersArgs {
                    device: p::VkDevice(DEVICE),
                    p_allocate_info: Some(Box::new(p::VkCommandBufferAllocateInfo {
                        command_pool: p::VkCommandPool(SECONDARY_POOL),
                        level: p::VkCommandBufferLevel(vk::CommandBufferLevel::SECONDARY.as_raw()),
                        command_buffer_count: 1,
                        ..Default::default()
                    })),
                    p_command_buffers: vec![p::VkCommandBuffer(SECONDARY)],
                }),
            )?
            .result,
        )?;
        self.secondary_allocated = true;
        checked(
            wire(rpc.begin_command_buffer(p::VkBeginCommandBufferArgs {
                command_buffer: p::VkCommandBuffer(SECONDARY),
                p_begin_info: Some(Box::new(p::VkCommandBufferBeginInfo {
                    flags: p::VkCommandBufferUsageFlags(
                        vk::CommandBufferUsageFlags::SIMULTANEOUS_USE.as_raw(),
                    ),
                    p_inheritance_info: Some(Box::new(Default::default())),
                    ..Default::default()
                })),
            }))?
            .result,
        )?;
        wire(rpc.cmd_fill_buffer(p::VkCmdFillBufferArgs {
            command_buffer: p::VkCommandBuffer(SECONDARY),
            dst_buffer: p::VkBuffer(READBACK),
            dst_offset: 0,
            size: 4,
            data: MARKER,
        }))?;
        checked(
            wire(rpc.end_command_buffer(p::VkEndCommandBufferArgs {
                command_buffer: p::VkCommandBuffer(SECONDARY),
            }))?
            .result,
        )?;
        self.generation = shared
            .native_video
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .plans
            .secondary_snapshot(SECONDARY)
            .map_err(|e| format!("secondary fixture snapshot: {e:?}"))?
            .generation;
        Ok(())
    }
    /// Validate the source/mixed upper bound before any binding or initialization.
    pub(super) fn ensure_partition_after(&self, previous_end: u64) -> Result<(), String> {
        if previous_end > self.offset {
            Err("render readback overlaps existing arena allocations".into())
        } else {
            Ok(())
        }
    }
    pub(super) fn record_before_decode(&self, rpc: &mut Rpc, cb: u64) -> Result<(), String> {
        memory(
            rpc,
            cb,
            vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::TRANSFER,
            vk::AccessFlags::HOST_WRITE,
            vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::TRANSFER_WRITE,
        )?;
        self.render(rpc, cb, true)?;
        self.occurrence(rpc, cb, 0)?;
        self.occurrence(rpc, cb, 1)
    }
    pub(super) fn record_after_decode(&self, rpc: &mut Rpc, cb: u64) -> Result<(), String> {
        self.render(rpc, cb, false)?;
        self.occurrence(rpc, cb, 2)?;
        self.occurrence(rpc, cb, 3)?;
        memory(
            rpc,
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::HOST,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::HOST_READ,
        )
    }
    fn occurrence(&self, rpc: &mut Rpc, cb: u64, occurrence: u64) -> Result<(), String> {
        memory(
            rpc,
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::TRANSFER_WRITE,
        )?;
        wire(rpc.cmd_fill_buffer(p::VkCmdFillBufferArgs {
            command_buffer: p::VkCommandBuffer(cb),
            dst_buffer: p::VkBuffer(READBACK),
            dst_offset: 0,
            size: 4,
            data: 0,
        }))?;
        memory(
            rpc,
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::TRANSFER_WRITE,
        )?;
        wire(rpc.cmd_execute_commands(p::VkCmdExecuteCommandsArgs {
            command_buffer: p::VkCommandBuffer(cb),
            command_buffer_count: 1,
            p_command_buffers: vec![p::VkCommandBuffer(SECONDARY)],
        }))?;
        memory(
            rpc,
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::TRANSFER_READ,
        )?;
        wire(rpc.cmd_copy_buffer(p::VkCmdCopyBufferArgs {
            command_buffer: p::VkCommandBuffer(cb),
            src_buffer: p::VkBuffer(READBACK),
            dst_buffer: p::VkBuffer(READBACK),
            region_count: 1,
            p_regions: vec![p::VkBufferCopy {
                src_offset: 0,
                dst_offset: 4 + occurrence * 4,
                size: 4,
            }],
        }))
    }
    fn render(&self, rpc: &mut Rpc, cb: u64, first: bool) -> Result<(), String> {
        image_transition(
            rpc,
            cb,
            if first {
                vk::ImageLayout::UNDEFINED
            } else {
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL
            },
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            if first {
                vk::PipelineStageFlags::TOP_OF_PIPE
            } else {
                vk::PipelineStageFlags::TRANSFER
            },
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            if first {
                vk::AccessFlags::empty()
            } else {
                vk::AccessFlags::TRANSFER_READ
            },
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        )?;
        wire(rpc.cmd_begin_rendering(p::VkCmdBeginRenderingArgs {
            command_buffer: p::VkCommandBuffer(cb),
            p_rendering_info: Some(Box::new(p::VkRenderingInfo {
                render_area: p::VkRect2D {
                    offset: p::VkOffset2D::default(),
                    extent: p::VkExtent2D {
                        width: 1,
                        height: 1,
                    },
                },
                layer_count: 1,
                color_attachment_count: 1,
                p_color_attachments: vec![p::VkRenderingAttachmentInfo {
                    image_view: p::VkImageView(RENDER_VIEW),
                    image_layout: p::VkImageLayout(
                        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL.as_raw(),
                    ),
                    load_op: p::VkAttachmentLoadOp(vk::AttachmentLoadOp::CLEAR.as_raw()),
                    store_op: p::VkAttachmentStoreOp(vk::AttachmentStoreOp::STORE.as_raw()),
                    clear_value: p::VkClearValue::Color(p::VkClearColorValue::Float32(if first {
                        [1.0, 0.0, 0.0, 1.0]
                    } else {
                        [0.0, 1.0, 0.0, 1.0]
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            })),
        }))?;
        wire(rpc.cmd_end_rendering(p::VkCmdEndRenderingArgs {
            command_buffer: p::VkCommandBuffer(cb),
        }))?;
        image_transition(
            rpc,
            cb,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::TRANSFER,
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            vk::AccessFlags::TRANSFER_READ,
        )?;
        wire(rpc.cmd_copy_image_to_buffer(p::VkCmdCopyImageToBufferArgs {
            command_buffer: p::VkCommandBuffer(cb),
            src_image: p::VkImage(RENDER_IMAGE),
            src_image_layout: p::VkImageLayout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL.as_raw()),
            dst_buffer: p::VkBuffer(READBACK),
            region_count: 1,
            p_regions: vec![p::VkBufferImageCopy {
                buffer_offset: if first { 32 } else { 36 },
                image_subresource: p::VkImageSubresourceLayers {
                    aspect_mask: p::VkImageAspectFlags(vk::ImageAspectFlags::COLOR.as_raw()),
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_extent: p::VkExtent3D {
                    width: 1,
                    height: 1,
                    depth: 1,
                },
                ..Default::default()
            }],
        }))
    }
    /// Called after EndCommandBuffer and before submission. Four separate
    /// Execute snapshots must own the same immutable logical generation.
    pub(super) fn verify_recording(&self, shared: &Shared, primary: u64) -> Result<(), String> {
        let state = shared
            .native_video
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let plan = state
            .plans
            .video(primary)
            .map_err(|e| format!("{e:?}"))?
            .ok_or("missing mixed plan")?;
        let mut occurrences = 0;
        let mut after_decode = false;
        let mut positions = [0usize; 2];
        for step in plan.iter() {
            if matches!(step, crate::video_recording::Step::Decode(_)) {
                after_decode = true;
            }
            if let crate::video_recording::Step::Execute { secondaries, .. } = step {
                for snapshot in secondaries
                    .iter()
                    .filter(|snapshot| snapshot.wire == SECONDARY)
                {
                    if snapshot.generation != self.generation
                        || snapshot.steps.len() != 1
                        || snapshot.begin.flags.0
                            & vk::CommandBufferUsageFlags::SIMULTANEOUS_USE.as_raw()
                            == 0
                    {
                        return Err("secondary snapshot generation/content/usage differs".into());
                    }
                    match &snapshot.steps[0] {
                        crate::video_recording::Step::Native(call) => match call.as_ref() {
                            sv_gpu_entry::RecordedCall::VkCmdFillBuffer(a)
                                if a.data == MARKER
                                    && a.dst_buffer.0 == READBACK
                                    && a.dst_offset == 0
                                    && a.size == 4 => {}
                            _ => return Err("secondary snapshot marker differs".into()),
                        },
                        _ => return Err("secondary snapshot has unexpected operation".into()),
                    }
                    positions[usize::from(after_decode)] += 1;
                    occurrences += 1;
                }
            }
        }
        if occurrences != 4 || positions != [2, 2] {
            return Err(format!(
                "secondary occurrence placement differs: {positions:?}"
            ));
        }
        println!(
            "render_secondary_snapshot wire={SECONDARY} generation={} occurrences=4 before_decode=2 after_decode=2 simultaneous_use=true reset_pending=false",
            self.generation
        );
        Ok(())
    }
    /// Existing controller must observe the original user fence first.
    pub(super) fn verify(&self, shared: &Shared) -> Result<(), String> {
        let mut bytes = [0; READBACK_BYTES as usize];
        shared
            .arena
            .region
            .read_bytes(self.offset, &mut bytes)
            .map_err(|e| e.to_string())?;
        for occurrence in 0..4 {
            let offset = 4 + occurrence * 4;
            let actual = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            if actual != MARKER {
                return Err(format!(
                    "secondary occurrence{occurrence}: {actual:08x} != {MARKER:08x}"
                ));
            }
        }
        if bytes[32..36] != [255, 0, 0, 255] || bytes[36..40] != [0, 255, 0, 255] {
            return Err(format!(
                "render clear pixels differ before={:?} after={:?}",
                &bytes[32..36],
                &bytes[36..40]
            ));
        }
        if bytes[20..32]
            .iter()
            .chain(bytes[40..64].iter())
            .any(|byte| *byte != INITIAL)
        {
            return Err("render/secondary untouched readback canary differs".into());
        }
        println!(
            "render_secondary_actual dynamic_rendering_scopes=2 clear_pixels=2 extent=1x1 rgba8_before=ff0000ff rgba8_after=00ff00ff same_secondary_wire={SECONDARY} native_occurrences=4 per_occurrence_canary_reset=true distinct_marker_outputs=4 shader_invocations=0 original_arena=true"
        );
        Ok(())
    }
    pub(super) fn resource_report(&self) {
        println!(
            "render_secondary_resources images=1 image_extent=1x1 format=RGBA8 image_allocation_bytes={} image_allocation_limit={} readback_bytes={} readback_requirement_bytes={} original_arena_offset={} secondary_pools=1 source_secondary_cbs=1 secondary_occurrences=4 private_rebuild_cbs<=4 render_scopes=2 render_clear_pixels=2 extra_shader_invocations=0",
            self.image_bytes,
            IMAGE_ALLOCATION_LIMIT,
            READBACK_BYTES,
            self.buffer_bytes,
            self.offset
        );
    }
    /// # Safety
    /// No accepted work, or existing controller proved complete native/VT/input
    /// retirement and joined the operation body. No timeout-based cleanup.
    pub(super) unsafe fn cleanup_after_retirement(&self, rpc: &mut Rpc) -> Result<(), String> {
        if self.secondary_allocated {
            wire(rpc.free_command_buffers(p::VkFreeCommandBuffersArgs {
                device: p::VkDevice(DEVICE),
                command_pool: p::VkCommandPool(SECONDARY_POOL),
                command_buffer_count: 1,
                p_command_buffers: vec![p::VkCommandBuffer(SECONDARY)],
            }))?;
        }
        if self.pool_created {
            wire(rpc.destroy_command_pool(p::VkDestroyCommandPoolArgs {
                device: p::VkDevice(DEVICE),
                command_pool: p::VkCommandPool(SECONDARY_POOL),
            }))?;
        }
        let shared = Arc::clone(&rpc.host().shared);
        unsafe {
            if self.view != vk::ImageView::null() {
                shared.driver.device.destroy_image_view(self.view, None);
            }
            if self.image != vk::Image::null() {
                shared.driver.device.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                shared.driver.device.free_memory(self.memory, None);
            }
            if self.readback != vk::Buffer::null() {
                shared.driver.device.destroy_buffer(self.readback, None);
            }
        }
        let mut objects = shared.objects();
        if self.mapped {
            for id in [RENDER_VIEW, RENDER_IMAGE, READBACK] {
                objects.remove(id);
            }
        }
        if self.reserved_ids {
            // Generic creation may bind null output IDs even on native failure.
            // They were absent before preparation, and all actual pool users
            // have been freed/destroyed above or never existed.
            objects.remove(SECONDARY);
            objects.remove(SECONDARY_POOL);
        }
        Ok(())
    }
}
fn memory(
    rpc: &mut Rpc,
    cb: u64,
    src: vk::PipelineStageFlags,
    dst: vk::PipelineStageFlags,
    sa: vk::AccessFlags,
    da: vk::AccessFlags,
) -> Result<(), String> {
    wire(rpc.cmd_pipeline_barrier(p::VkCmdPipelineBarrierArgs {
        command_buffer: p::VkCommandBuffer(cb),
        src_stage_mask: p::VkPipelineStageFlags(src.as_raw()),
        dst_stage_mask: p::VkPipelineStageFlags(dst.as_raw()),
        memory_barrier_count: 1,
        p_memory_barriers: vec![p::VkMemoryBarrier {
            src_access_mask: p::VkAccessFlags(sa.as_raw()),
            dst_access_mask: p::VkAccessFlags(da.as_raw()),
            ..Default::default()
        }],
        ..Default::default()
    }))
}
fn image_transition(
    rpc: &mut Rpc,
    cb: u64,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    src: vk::PipelineStageFlags,
    dst: vk::PipelineStageFlags,
    sa: vk::AccessFlags,
    da: vk::AccessFlags,
) -> Result<(), String> {
    wire(rpc.cmd_pipeline_barrier(p::VkCmdPipelineBarrierArgs {
        command_buffer: p::VkCommandBuffer(cb),
        src_stage_mask: p::VkPipelineStageFlags(src.as_raw()),
        dst_stage_mask: p::VkPipelineStageFlags(dst.as_raw()),
        image_memory_barrier_count: 1,
        p_image_memory_barriers: vec![p::VkImageMemoryBarrier {
            src_access_mask: p::VkAccessFlags(sa.as_raw()),
            dst_access_mask: p::VkAccessFlags(da.as_raw()),
            old_layout: p::VkImageLayout(old.as_raw()),
            new_layout: p::VkImageLayout(new.as_raw()),
            src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
            dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
            image: p::VkImage(RENDER_IMAGE),
            subresource_range: p::VkImageSubresourceRange {
                aspect_mask: p::VkImageAspectFlags(vk::ImageAspectFlags::COLOR.as_raw()),
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            },
            ..Default::default()
        }],
        ..Default::default()
    }))
}
