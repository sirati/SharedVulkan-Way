//! Feature-only mixed compute/decode/copy probe on the original imported arena.
//! Shader provenance: examples/assets/mixed-video/bounded.comp, compiled only
//! for native-video-fixture. Literal bounds and no shader loop keep work fixed
//! even if push-constant restoration fails. No product payload copy.
use super::*;
#[path = "native_video_render_secondary_fixture.rs"]
mod render_secondary;
const BUFFERS: [u64; 3] = [180, 181, 182];
const SHADER: u64 = 183;
const SET_LAYOUT: u64 = 184;
const PIPE_LAYOUT: u64 = 185;
const DESCRIPTOR_POOL: u64 = 186;
const SET: u64 = 187;
const PIPELINE: u64 = 188;
const OCCLUSION: u64 = 189;
const TIMESTAMPS: u64 = 190;
const BYTES: u64 = 4096;
const SENTINEL: u32 = 0x73569b21;
const CANARY: u32 = 0xa17e5eed;
static SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bounded-mixed.spv"));
pub(super) struct Prepared {
    offsets: [u64; 3],
    buffers: usize,
    shader: bool,
    set_layout: bool,
    pipe_layout: bool,
    descriptor_pool: bool,
    pipeline: bool,
    queries: bool,
    query_pools: usize,
    render_secondary: Option<render_secondary::Prepared>,
}
impl Prepared {
    pub(super) fn prepare(rpc: &mut sv_gpu_entry::Forward<GpuServer>) -> Result<Self, String> {
        let mut value = Self {
            offsets: [0; 3],
            buffers: 0,
            shader: false,
            set_layout: false,
            pipe_layout: false,
            descriptor_pool: false,
            pipeline: false,
            queries: std::env::args().any(|a| a == "--mixed-queries"),
            query_pools: 0,
            render_secondary: None,
        };
        if let Err(error) = value.prepare_inner(rpc) {
            let cleanup = unsafe { value.cleanup_after_retirement(rpc) };
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; mixed preaccept cleanup: {cleanup}"),
            });
        }
        Ok(value)
    }
    fn prepare_inner(&mut self, rpc: &mut sv_gpu_entry::Forward<GpuServer>) -> Result<(), String> {
        if SPIRV.len() < 20 || SPIRV.len() % 4 != 0 {
            return Err("bounded arena shader missing or misaligned".into());
        }
        let code: Vec<u32> = SPIRV
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        if code[0] != 0x07230203 || code[1] > 0x00010300 || code[1] < 0x00010000 {
            return Err("arena shader is not supported SPIR-V 1.0-1.3".into());
        }
        let shared = Arc::clone(&rpc.host().shared);
        let limits = unsafe {
            shared
                .driver
                .instance
                .get_physical_device_properties(shared.driver.pd)
        }
        .limits;
        if limits.max_storage_buffer_range < u32::try_from(BYTES).unwrap()
            || limits.max_compute_work_group_count[0] < 4
            || limits.max_compute_work_group_size[0] < 256
            || limits.max_compute_work_group_invocations < 256
            || limits.max_push_constants_size < 8
        {
            return Err("mixed fixture shader exceeds native device limits".into());
        }
        let source = shared
            .objects()
            .get::<vk::Buffer>(HandleType::VkBuffer, SOURCE)
            .map_err(|e| format!("{e:?}"))?;
        let source_bytes =
            unsafe { shared.driver.device.get_buffer_memory_requirements(source) }.size;
        let mut next = 64 * 1024u64;
        if source_bytes > next {
            return Err("mixed output partition would overlap original compressed source".into());
        }
        for (index, wire_id) in BUFFERS.iter().enumerate() {
            checked(
                wire(
                    rpc.create_buffer(p::VkCreateBufferArgs {
                        device: p::VkDevice(DEVICE),
                        p_buffer: Some(p::VkBuffer(*wire_id)),
                        p_create_info: Some(Box::new(p::VkBufferCreateInfo {
                            p_next: vec![p::PNextLink::VkExternalMemoryBufferCreateInfo(Box::new(
                                p::VkExternalMemoryBufferCreateInfo {
                                    handle_types: p::VkExternalMemoryHandleTypeFlags(
                                        shared.arena.external_handle_type.as_raw(),
                                    ),
                                    ..Default::default()
                                },
                            ))],
                            size: BYTES,
                            usage: p::VkBufferUsageFlags(
                                (vk::BufferUsageFlags::STORAGE_BUFFER
                                    | vk::BufferUsageFlags::TRANSFER_SRC
                                    | vk::BufferUsageFlags::TRANSFER_DST)
                                    .as_raw(),
                            ),
                            sharing_mode: p::VkSharingMode(vk::SharingMode::EXCLUSIVE.as_raw()),
                            ..Default::default()
                        })),
                        ..Default::default()
                    }),
                )?
                .result,
            )?;
            self.buffers += 1;
            let actual = shared
                .objects()
                .get::<vk::Buffer>(HandleType::VkBuffer, *wire_id)
                .map_err(|e| format!("{e:?}"))?;
            let req = unsafe { shared.driver.device.get_buffer_memory_requirements(actual) };
            let align = req
                .alignment
                .max(limits.min_storage_buffer_offset_alignment)
                .max(1);
            next = next
                .checked_add(align - 1)
                .map(|n| n / align * align)
                .ok_or("mixed buffer alignment overflow")?;
            if req.size < BYTES {
                return Err("mixed buffer requirement smaller than declared size".into());
            }
            let end = next
                .checked_add(req.size)
                .ok_or("mixed buffer range overflow")?;
            if end > shared.arena.len || req.memory_type_bits & (1 << shared.arena.memory_type) == 0
            {
                return Err("mixed buffer incompatible with original arena".into());
            }
            self.offsets[index] = next;
            checked(
                wire(rpc.bind_buffer_memory(p::VkBindBufferMemoryArgs {
                    device: p::VkDevice(DEVICE),
                    buffer: p::VkBuffer(*wire_id),
                    memory: p::VkDeviceMemory(crate::ARENA_MEMORY_ID),
                    memory_offset: next,
                }))?
                .result,
            )?;
            let mut initial = vec![0; BYTES as usize];
            for word in initial.chunks_exact_mut(4) {
                word.copy_from_slice(&CANARY.to_le_bytes());
            }
            shared
                .arena
                .region
                .write_bytes(next, &initial)
                .map_err(|e| e.to_string())?;
            next = end;
        }
        checked(
            wire(rpc.create_shader_module(p::VkCreateShaderModuleArgs {
                device: p::VkDevice(DEVICE),
                p_shader_module: Some(p::VkShaderModule(SHADER)),
                p_create_info: Some(Box::new(p::VkShaderModuleCreateInfo {
                    code_size: SPIRV.len() as u64,
                    p_code: code,
                    ..Default::default()
                })),
                ..Default::default()
            }))?
            .result,
        )?;
        self.shader = true;
        checked(
            wire(
                rpc.create_descriptor_set_layout(p::VkCreateDescriptorSetLayoutArgs {
                    device: p::VkDevice(DEVICE),
                    p_set_layout: Some(p::VkDescriptorSetLayout(SET_LAYOUT)),
                    p_create_info: Some(Box::new(p::VkDescriptorSetLayoutCreateInfo {
                        binding_count: 2,
                        p_bindings: (0..2)
                            .map(|binding| p::VkDescriptorSetLayoutBinding {
                                binding,
                                descriptor_type: p::VkDescriptorType(
                                    vk::DescriptorType::STORAGE_BUFFER.as_raw(),
                                ),
                                descriptor_count: 1,
                                stage_flags: p::VkShaderStageFlags(
                                    vk::ShaderStageFlags::COMPUTE.as_raw(),
                                ),
                                ..Default::default()
                            })
                            .collect(),
                        ..Default::default()
                    })),
                    ..Default::default()
                }),
            )?
            .result,
        )?;
        self.set_layout = true;
        checked(
            wire(rpc.create_pipeline_layout(p::VkCreatePipelineLayoutArgs {
                device: p::VkDevice(DEVICE),
                p_pipeline_layout: Some(p::VkPipelineLayout(PIPE_LAYOUT)),
                p_create_info: Some(Box::new(p::VkPipelineLayoutCreateInfo {
                    set_layout_count: 1,
                    p_set_layouts: vec![p::VkDescriptorSetLayout(SET_LAYOUT)],
                    push_constant_range_count: 1,
                    p_push_constant_ranges: vec![p::VkPushConstantRange {
                        stage_flags: p::VkShaderStageFlags(vk::ShaderStageFlags::COMPUTE.as_raw()),
                        offset: 0,
                        size: 8,
                    }],
                    ..Default::default()
                })),
                ..Default::default()
            }))?
            .result,
        )?;
        self.pipe_layout = true;
        checked(
            wire(rpc.create_descriptor_pool(p::VkCreateDescriptorPoolArgs {
                device: p::VkDevice(DEVICE),
                p_descriptor_pool: Some(p::VkDescriptorPool(DESCRIPTOR_POOL)),
                p_create_info: Some(Box::new(p::VkDescriptorPoolCreateInfo {
                    max_sets: 1,
                    pool_size_count: 1,
                    p_pool_sizes: vec![p::VkDescriptorPoolSize {
                        type_: p::VkDescriptorType(vk::DescriptorType::STORAGE_BUFFER.as_raw()),
                        descriptor_count: 2,
                    }],
                    ..Default::default()
                })),
                ..Default::default()
            }))?
            .result,
        )?;
        self.descriptor_pool = true;
        checked(
            wire(
                rpc.allocate_descriptor_sets(p::VkAllocateDescriptorSetsArgs {
                    device: p::VkDevice(DEVICE),
                    p_allocate_info: Some(Box::new(p::VkDescriptorSetAllocateInfo {
                        descriptor_pool: p::VkDescriptorPool(DESCRIPTOR_POOL),
                        descriptor_set_count: 1,
                        p_set_layouts: vec![p::VkDescriptorSetLayout(SET_LAYOUT)],
                        ..Default::default()
                    })),
                    p_descriptor_sets: vec![p::VkDescriptorSet(SET)],
                }),
            )?
            .result,
        )?;
        wire(
            rpc.update_descriptor_sets(p::VkUpdateDescriptorSetsArgs {
                device: p::VkDevice(DEVICE),
                descriptor_write_count: 2,
                p_descriptor_writes: (0..2)
                    .map(|binding| p::VkWriteDescriptorSet {
                        dst_set: p::VkDescriptorSet(SET),
                        dst_binding: binding,
                        descriptor_count: 1,
                        descriptor_type: p::VkDescriptorType(
                            vk::DescriptorType::STORAGE_BUFFER.as_raw(),
                        ),
                        p_buffer_info: vec![p::VkDescriptorBufferInfo {
                            buffer: p::VkBuffer(BUFFERS[binding as usize]),
                            offset: 0,
                            range: BYTES,
                        }],
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
        )?;
        checked(
            wire(
                rpc.create_compute_pipelines(p::VkCreateComputePipelinesArgs {
                    device: p::VkDevice(DEVICE),
                    create_info_count: 1,
                    p_create_infos: vec![p::VkComputePipelineCreateInfo {
                        stage: p::VkPipelineShaderStageCreateInfo {
                            stage: p::VkShaderStageFlagBits(
                                vk::ShaderStageFlags::COMPUTE.as_raw() as i32
                            ),
                            module: p::VkShaderModule(SHADER),
                            p_name: b"main\0".to_vec(),
                            ..Default::default()
                        },
                        layout: p::VkPipelineLayout(PIPE_LAYOUT),
                        base_pipeline_index: -1,
                        ..Default::default()
                    }],
                    p_pipelines: vec![p::VkPipeline(PIPELINE)],
                    ..Default::default()
                }),
            )?
            .result,
        )?;
        self.pipeline = true;
        if self.queries {
            let families = unsafe {
                shared
                    .driver
                    .instance
                    .get_physical_device_queue_family_properties(shared.driver.pd)
            };
            let family = families
                .get(shared.driver.queue_family as usize)
                .ok_or("missing mixed query family")?;
            if !family.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                || family.timestamp_valid_bits != 64
                || limits.timestamp_compute_and_graphics == 0
            {
                return Err(
                    "mixed queries require graphics and full 64-bit compute timestamps".into(),
                );
            }
            for (id, ty, count) in [
                (OCCLUSION, vk::QueryType::OCCLUSION, 1),
                (TIMESTAMPS, vk::QueryType::TIMESTAMP, 2),
            ] {
                checked(
                    wire(rpc.create_query_pool(p::VkCreateQueryPoolArgs {
                        device: p::VkDevice(DEVICE),
                        p_query_pool: Some(p::VkQueryPool(id)),
                        p_create_info: Some(Box::new(p::VkQueryPoolCreateInfo {
                            query_type: p::VkQueryType(ty.as_raw()),
                            query_count: count,
                            ..Default::default()
                        })),
                    }))?
                    .result,
                )?;
                self.query_pools += 1;
            }
        }
        if std::env::args().any(|a| a == "--mixed-render-secondary") {
            self.render_secondary = Some(render_secondary::Prepared::prepare(
                rpc,
                next.max(source_bytes),
            )?);
        }
        Ok(())
    }
    pub(super) fn record_before_decode(
        &self,
        rpc: &mut sv_gpu_entry::Forward<GpuServer>,
        cb: u64,
    ) -> Result<(), String> {
        if let Some(render) = &self.render_secondary {
            render.record_before_decode(rpc, cb)?;
        }
        if self.queries {
            for (id, count) in [(OCCLUSION, 1), (TIMESTAMPS, 2)] {
                wire(rpc.cmd_reset_query_pool(p::VkCmdResetQueryPoolArgs {
                    command_buffer: p::VkCommandBuffer(cb),
                    query_pool: p::VkQueryPool(id),
                    first_query: 0,
                    query_count: count,
                }))?;
            }
            wire(rpc.cmd_begin_query(p::VkCmdBeginQueryArgs {
                command_buffer: p::VkCommandBuffer(cb),
                query_pool: p::VkQueryPool(OCCLUSION),
                query: 0,
                flags: p::VkQueryControlFlags(0),
            }))?;
        }
        wire(rpc.cmd_fill_buffer(p::VkCmdFillBufferArgs {
            command_buffer: p::VkCommandBuffer(cb),
            dst_buffer: p::VkBuffer(BUFFERS[0]),
            dst_offset: 0,
            size: BYTES,
            data: SENTINEL,
        }))?;
        barrier(
            rpc,
            cb,
            vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::AccessFlags::HOST_WRITE | vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
        )?;
        if self.queries {
            wire(rpc.cmd_end_query(p::VkCmdEndQueryArgs {
                command_buffer: p::VkCommandBuffer(cb),
                query_pool: p::VkQueryPool(OCCLUSION),
                query: 0,
            }))?;
            timestamp(rpc, cb, 0)?;
        }
        wire(rpc.cmd_bind_pipeline(p::VkCmdBindPipelineArgs {
            command_buffer: p::VkCommandBuffer(cb),
            pipeline_bind_point: p::VkPipelineBindPoint(vk::PipelineBindPoint::COMPUTE.as_raw()),
            pipeline: p::VkPipeline(PIPELINE),
        }))?;
        wire(
            rpc.cmd_bind_descriptor_sets(p::VkCmdBindDescriptorSetsArgs {
                command_buffer: p::VkCommandBuffer(cb),
                pipeline_bind_point: p::VkPipelineBindPoint(
                    vk::PipelineBindPoint::COMPUTE.as_raw(),
                ),
                layout: p::VkPipelineLayout(PIPE_LAYOUT),
                first_set: 0,
                descriptor_set_count: 1,
                p_descriptor_sets: vec![p::VkDescriptorSet(SET)],
                ..Default::default()
            }),
        )?;
        wire(rpc.cmd_push_constants(p::VkCmdPushConstantsArgs {
            command_buffer: p::VkCommandBuffer(cb),
            layout: p::VkPipelineLayout(PIPE_LAYOUT),
            stage_flags: p::VkShaderStageFlags(vk::ShaderStageFlags::COMPUTE.as_raw()),
            offset: 0,
            size: 8,
            p_values: [1024u32.to_le_bytes(), 1u32.to_le_bytes()].concat(),
        }))?;
        Ok(())
    }
    pub(super) fn record_after_decode(
        &self,
        rpc: &mut sv_gpu_entry::Forward<GpuServer>,
        cb: u64,
    ) -> Result<(), String> {
        if self.queries {
            timestamp(rpc, cb, 1)?;
        }
        // No pipeline, descriptors or push constants are rebound after decode.
        wire(rpc.cmd_dispatch(p::VkCmdDispatchArgs {
            command_buffer: p::VkCommandBuffer(cb),
            group_count_x: 4,
            group_count_y: 1,
            group_count_z: 1,
        }))?;
        barrier(
            rpc,
            cb,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::TRANSFER,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::TRANSFER_READ,
        )?;
        wire(rpc.cmd_copy_buffer(p::VkCmdCopyBufferArgs {
            command_buffer: p::VkCommandBuffer(cb),
            src_buffer: p::VkBuffer(BUFFERS[1]),
            dst_buffer: p::VkBuffer(BUFFERS[2]),
            region_count: 1,
            p_regions: vec![p::VkBufferCopy {
                src_offset: 0,
                dst_offset: 0,
                size: BYTES,
            }],
        }))?;
        barrier(
            rpc,
            cb,
            vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::HOST,
            vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::HOST_READ,
        )?;
        if let Some(render) = &self.render_secondary {
            render.record_after_decode(rpc, cb)?;
        }
        Ok(())
    }
    pub(super) fn verify_recording(&self, shared: &Shared, primary: u64) -> Result<(), String> {
        if let Some(render) = &self.render_secondary {
            render.verify_recording(shared, primary)?;
        }
        Ok(())
    }
    /// Call only after the existing controller observes the original user fence.
    pub(super) fn verify(&self, shared: &Shared) -> Result<(), String> {
        for (index, &offset) in self.offsets.iter().enumerate() {
            let mut bytes = vec![0; BYTES as usize];
            shared
                .arena
                .region
                .read_bytes(offset, &mut bytes)
                .map_err(|e| e.to_string())?;
            for (word, bytes) in bytes.chunks_exact(4).enumerate() {
                let actual = u32::from_le_bytes(bytes.try_into().unwrap());
                let expected = if index == 0 || word >= 64 {
                    SENTINEL
                } else {
                    CANARY
                };
                if actual != expected {
                    return Err(format!(
                        "mixed original arena buffer{index} word{word}: {actual:08x} != {expected:08x}"
                    ));
                }
            }
        }
        if let Some(render) = &self.render_secondary {
            render.verify(shared)?;
        }
        println!(
            "mixed compute_after_decode=true rebind_after_decode=false dispatches=1 workgroups=4 shader_loops=0 words=1024 payload_words=960 copy_bytes=4096 original_arena=true header_canaries=true"
        );
        Ok(())
    }
    /// Query result retrieval happens after the original user fence, never
    /// during pre-accept recording or from a timeout cleanup path.
    pub(super) fn verify_queries(
        &self,
        rpc: &mut sv_gpu_entry::Forward<GpuServer>,
    ) -> Result<(), String> {
        if !self.queries {
            return Ok(());
        }
        let mut values = Vec::new();
        let mut availability = Vec::new();
        for (id, count) in [(OCCLUSION, 1), (TIMESTAMPS, 2)] {
            let reply = wire(
                rpc.get_query_pool_results(p::VkGetQueryPoolResultsArgs {
                    device: p::VkDevice(DEVICE),
                    query_pool: p::VkQueryPool(id),
                    first_query: 0,
                    query_count: count,
                    data_size: u64::from(count) * 16,
                    stride: 16,
                    flags: p::VkQueryResultFlags(
                        (vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WITH_AVAILABILITY)
                            .as_raw(),
                    ),
                    p_data: vec![0; count as usize * 16],
                }),
            )?;
            checked(reply.result)?;
            if reply.p_data.len() != count as usize * 16 {
                return Err("mixed query result size differs".into());
            }
            for record in reply.p_data.chunks_exact(16) {
                let value = u64::from_le_bytes(record[..8].try_into().unwrap());
                let available = u64::from_le_bytes(record[8..].try_into().unwrap());
                if available == 0 {
                    return Err("mixed completed query remains unavailable".into());
                }
                values.push(value);
                availability.push(available);
            }
        }
        if values[0] != 0 || values[1] == 0 || values[2] <= values[1] {
            return Err(format!("mixed query oracle differs: {values:?}"));
        }
        println!(
            "mixed_queries occlusion={} availability={availability:?} timestamp_before={} timestamp_after={} timestamp_bits=64 timestamp_strictly_increasing=true native_reset=true render_draw=false",
            values[0], values[1], values[2]
        );
        Ok(())
    }
    pub(super) fn resource_report(&self) {
        if let Some(render) = &self.render_secondary {
            render.resource_report();
        }
        println!(
            "mixed_query_resources query_pools={} occlusion_queries={} timestamp_queries={}",
            self.query_pools,
            usize::from(self.queries),
            usize::from(self.queries) * 2
        );
        println!(
            "mixed_resources original_arena_buffers=3 buffer_bytes=12288 shader_modules=1 pipelines=1 descriptor_sets=1 descriptor_pools=1"
        );
    }
    /// # Safety
    /// Existing full-graph controller proved native/VT/input retirement and
    /// joined the body. Pre-accept prepare cleanup has no native users at all.
    pub(super) unsafe fn cleanup_after_retirement(
        &self,
        rpc: &mut sv_gpu_entry::Forward<GpuServer>,
    ) -> Result<(), String> {
        if let Some(render) = &self.render_secondary {
            unsafe {
                render.cleanup_after_retirement(rpc)?;
            }
        }
        for &id in &[OCCLUSION, TIMESTAMPS][..self.query_pools] {
            wire(rpc.destroy_query_pool(p::VkDestroyQueryPoolArgs {
                device: p::VkDevice(DEVICE),
                query_pool: p::VkQueryPool(id),
                ..Default::default()
            }))?;
        }
        if self.pipeline {
            wire(rpc.destroy_pipeline(p::VkDestroyPipelineArgs {
                device: p::VkDevice(DEVICE),
                pipeline: p::VkPipeline(PIPELINE),
                ..Default::default()
            }))?;
        }
        if self.descriptor_pool {
            wire(rpc.destroy_descriptor_pool(p::VkDestroyDescriptorPoolArgs {
                device: p::VkDevice(DEVICE),
                descriptor_pool: p::VkDescriptorPool(DESCRIPTOR_POOL),
                ..Default::default()
            }))?;
        }
        if self.pipe_layout {
            wire(rpc.destroy_pipeline_layout(p::VkDestroyPipelineLayoutArgs {
                device: p::VkDevice(DEVICE),
                pipeline_layout: p::VkPipelineLayout(PIPE_LAYOUT),
                ..Default::default()
            }))?;
        }
        if self.set_layout {
            wire(
                rpc.destroy_descriptor_set_layout(p::VkDestroyDescriptorSetLayoutArgs {
                    device: p::VkDevice(DEVICE),
                    descriptor_set_layout: p::VkDescriptorSetLayout(SET_LAYOUT),
                    ..Default::default()
                }),
            )?;
        }
        if self.shader {
            wire(rpc.destroy_shader_module(p::VkDestroyShaderModuleArgs {
                device: p::VkDevice(DEVICE),
                shader_module: p::VkShaderModule(SHADER),
                ..Default::default()
            }))?;
        }
        for &buffer in &BUFFERS[..self.buffers] {
            wire(rpc.destroy_buffer(p::VkDestroyBufferArgs {
                device: p::VkDevice(DEVICE),
                buffer: p::VkBuffer(buffer),
                ..Default::default()
            }))?;
        }
        Ok(())
    }
}
fn barrier(
    rpc: &mut sv_gpu_entry::Forward<GpuServer>,
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

fn timestamp(
    rpc: &mut sv_gpu_entry::Forward<GpuServer>,
    cb: u64,
    query: u32,
) -> Result<(), String> {
    wire(rpc.cmd_write_timestamp2(p::VkCmdWriteTimestamp2Args {
        command_buffer: p::VkCommandBuffer(cb),
        stage: p::VkPipelineStageFlags2(vk::PipelineStageFlags2::ALL_COMMANDS.as_raw()),
        query_pool: p::VkQueryPool(TIMESTAMPS),
        query,
    }))
}
