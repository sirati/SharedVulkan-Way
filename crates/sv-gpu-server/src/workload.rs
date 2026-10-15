//! A compute dispatch over arena memory, on the GPU side.
//!
//! This is `gpu-probe`'s workload with its Windows half removed: two storage
//! buffers bound to *one* `VkDeviceMemory` — the input at offset 0 and the
//! output at `input_bytes` — and a grid-stride compute shader that writes
//! `arena_record::gpu::transform` of every payload word.
//!
//! Binding the second buffer at a non-zero offset is not incidental. A real
//! arena hands out suballocations, so "can a `VkBuffer` live part-way into the
//! arena's allocation?" is a question the design needs answered, and it is the
//! same question §5.3's `(memory, offset)` rewrite exists to make answerable.
//!
//! # What a pass here means, and what it does not
//!
//! It means the GPU read bytes an *unrelated process* wrote through its own
//! mapping, and that process read back bytes the GPU wrote. That is R15 and
//! §14's largest open item, in the Linux/Linux topology.
//!
//! It does not mean anything about the Windows guest topology, where the arena
//! is guest RAM rather than a driver allocation. That is step 0's experiment and
//! it was answered separately.

use crate::driver::{Driver, DriverError, Result};
use arena_record::gpu as ref_gpu;
use ash::vk;

/// Push constants, laid out exactly as `shaders/arena.comp` declares them.
#[repr(C)]
#[derive(Clone, Copy)]
struct Push {
    words: u32,
    kernel: u32,
}

/// The compiled module, in the binary.
///
/// Baked in rather than read from `OUT_DIR` at run time so the probe works
/// inside a sandbox that binds only `/nix` and the binary directory — see the
/// note in `build.rs`.
const ARENA_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/arena.spv"));

pub(crate) fn spv_words() -> Result<Vec<u32>> {
    if ARENA_SPV.is_empty() {
        return Err(DriverError::Other(
            "this build has no compute shader: glslangValidator was not on PATH \
             at build time (enter `nix develop`)"
                .into(),
        ));
    }
    if !ARENA_SPV.len().is_multiple_of(4) {
        return Err(DriverError::Other("the SPIR-V module is not a whole number of words".into()));
    }
    Ok(ARENA_SPV.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect())
}

/// Run the transform over `[0, split.input_bytes())` of `memory`, writing the
/// result at `split.input_bytes()`.
pub fn run(driver: &Driver, memory: vk::DeviceMemory, split: ref_gpu::Split) -> Result<()> {
    let d = &driver.device;
    let half = split.input_bytes() as u64;
    let code = spv_words()?;

    let bci = vk::BufferCreateInfo::default()
        .size(half)
        .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buf_in = unsafe { d.create_buffer(&bci, None) }
        .map_err(|e| DriverError::Vk("vkCreateBuffer(in)", e))?;
    let buf_out = unsafe { d.create_buffer(&bci, None) }
        .map_err(|e| DriverError::Vk("vkCreateBuffer(out)", e))?;

    // The gate `gpu-probe` records: `vkGetBufferMemoryRequirements` returns its
    // own `memoryTypeBits`, and nothing guarantees it intersects the mask the
    // *allocation* has. If it does not, arena memory is mappable and useless for
    // GPU work, which is the failure this whole step is looking for.
    let req = unsafe { d.get_buffer_memory_requirements(buf_in) };
    if !half.is_multiple_of(req.alignment) {
        unsafe {
            d.destroy_buffer(buf_in, None);
            d.destroy_buffer(buf_out, None);
        }
        return Err(DriverError::Other(format!(
            "the second buffer's offset {half} is not a multiple of the required \
             alignment {}",
            req.alignment
        )));
    }

    unsafe { d.bind_buffer_memory(buf_in, memory, 0) }
        .map_err(|e| DriverError::Vk("vkBindBufferMemory(in @0)", e))?;
    unsafe { d.bind_buffer_memory(buf_out, memory, half) }
        .map_err(|e| DriverError::Vk("vkBindBufferMemory(out @half)", e))?;

    let bindings = [
        vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::COMPUTE),
        vk::DescriptorSetLayoutBinding::default()
            .binding(1)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::COMPUTE),
    ];
    let set_layout = unsafe {
        d.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )
    }
    .map_err(|e| DriverError::Vk("vkCreateDescriptorSetLayout", e))?;

    let sizes = [vk::DescriptorPoolSize::default()
        .ty(vk::DescriptorType::STORAGE_BUFFER)
        .descriptor_count(2)];
    let pool = unsafe {
        d.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&sizes),
            None,
        )
    }
    .map_err(|e| DriverError::Vk("vkCreateDescriptorPool", e))?;
    let layouts = [set_layout];
    let set = unsafe {
        d.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(pool)
                .set_layouts(&layouts),
        )
    }
    .map_err(|e| DriverError::Vk("vkAllocateDescriptorSets", e))?[0];

    let in_info = [vk::DescriptorBufferInfo::default().buffer(buf_in).offset(0).range(half)];
    let out_info = [vk::DescriptorBufferInfo::default().buffer(buf_out).offset(0).range(half)];
    let writes = [
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&in_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&out_info),
    ];
    unsafe { d.update_descriptor_sets(&writes, &[]) };

    let module = unsafe {
        d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
    }
    .map_err(|e| DriverError::Vk("vkCreateShaderModule", e))?;

    let ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
        .offset(0)
        .size(std::mem::size_of::<Push>() as u32)];
    let pipeline_layout = unsafe {
        d.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&layouts)
                .push_constant_ranges(&ranges),
            None,
        )
    }
    .map_err(|e| DriverError::Vk("vkCreatePipelineLayout", e))?;

    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(module)
        .name(c"main");
    let infos =
        [vk::ComputePipelineCreateInfo::default().stage(stage).layout(pipeline_layout)];
    let pipeline = unsafe { d.create_compute_pipelines(vk::PipelineCache::null(), &infos, None) }
        .map_err(|(_, e)| DriverError::Vk("vkCreateComputePipelines", e))?[0];

    let cmd_pool = unsafe {
        d.create_command_pool(
            &vk::CommandPoolCreateInfo::default().queue_family_index(driver.queue_family),
            None,
        )
    }
    .map_err(|e| DriverError::Vk("vkCreateCommandPool", e))?;
    let cmd = unsafe {
        d.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(cmd_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }
    .map_err(|e| DriverError::Vk("vkAllocateCommandBuffers", e))?[0];

    // A workgroup count the driver certainly accepts whatever the window size
    // is; the shader's grid-stride loop covers the rest. `gpu-probe` makes the
    // same choice and gives the reason: the caller should not have to know
    // `maxComputeWorkGroupCount` and get it right at 1 GiB.
    let groups = 256u32.min(driver.props.limits.max_compute_work_group_count[0]).max(1);
    let push = Push { words: split.words(), kernel: ref_gpu::KERNEL_TRANSFORM };

    unsafe {
        d.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .map_err(|e| DriverError::Vk("vkBeginCommandBuffer", e))?;
        d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline);
        d.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            pipeline_layout,
            0,
            &[set],
            &[],
        );
        let bytes = std::slice::from_raw_parts(
            (&push as *const Push) as *const u8,
            std::mem::size_of::<Push>(),
        );
        d.cmd_push_constants(cmd, pipeline_layout, vk::ShaderStageFlags::COMPUTE, 0, bytes);
        d.cmd_dispatch(cmd, groups, 1, 1);
        d.end_command_buffer(cmd).map_err(|e| DriverError::Vk("vkEndCommandBuffer", e))?;
    }

    let fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None) }
        .map_err(|e| DriverError::Vk("vkCreateFence", e))?;
    let cmds = [cmd];
    let submits = [vk::SubmitInfo::default().command_buffers(&cmds)];
    unsafe { d.queue_submit(driver.queue, &submits, fence) }
        .map_err(|e| DriverError::Vk("vkQueueSubmit", e))?;
    // Ten seconds is generous for a megabyte and it is bounded, which matters:
    // this GPU may be driving somebody's screen (R18).
    let r = unsafe { d.wait_for_fences(&[fence], true, 10_000_000_000) };

    unsafe {
        d.destroy_fence(fence, None);
        d.destroy_command_pool(cmd_pool, None);
        d.destroy_pipeline(pipeline, None);
        d.destroy_pipeline_layout(pipeline_layout, None);
        d.destroy_shader_module(module, None);
        d.destroy_descriptor_pool(pool, None);
        d.destroy_descriptor_set_layout(set_layout, None);
        d.destroy_buffer(buf_in, None);
        d.destroy_buffer(buf_out, None);
    }
    r.map_err(|e| DriverError::Vk("vkWaitForFences", e))
}
