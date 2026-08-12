//! `sv-compute-e2e` — the whole stack, with a compute shader in the middle.
//!
//! **This is the app side**, and it lives in the GPU side's crate only because
//! that crate already has `ash` and the compiled shader. It links nothing of
//! `sv-gpu-server`'s: every Vulkan call below goes through the *real* loader,
//! which loads `libsv_icd.so`, which encodes it onto a ring.
//!
//! ```text
//! VK_DRIVER_FILES=target/debug/sv_icd.json SV_SOCKET=… sv-compute-e2e
//! ```
//!
//! # What it proves that `sv-arena-probe` does not
//!
//! `sv-arena-probe` established the *memory*: a descriptor the driver exported,
//! mapped in an unrelated process, with a shader reading and writing it. It
//! reached the driver directly, with no protocol in between.
//!
//! This reaches the driver only through the wire. Every object it creates —
//! buffer, memory, shader module, descriptor set layout, pool, set, pipeline
//! layout, pipeline, command pool, command buffer, fence — is an id this process
//! minted, encoded, and sent; every `vkCmd*` is a ring packet; the submit
//! carries a `RingBarrier` past everything the device ring produced. The one
//! thing that does **not** cross is the data, which is design.md §1's invariant:
//!
//! > `vkMapMemory` on the app side returns `arena_base + slab_base +
//! > alloc_offset + offset`. A real pointer into real shared pages. It involves
//! > **no round trip and no copy**.
//!
//! So a pass here means an application wrote bytes into memory the GPU side's
//! driver owns, asked for a dispatch over the wire, and read the GPU's answer
//! back out of the same pages.

use arena_record::gpu as ref_gpu;
use ash::vk;

/// One MiB in, one MiB out. Small enough to be quick, large enough that the
/// grid-stride loop actually strides.
const HALF: u64 = 1 << 20;
const TOTAL: u64 = HALF * 2;

const ARENA_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/arena.spv"));

#[repr(C)]
#[derive(Clone, Copy)]
struct Push {
    words: u32,
    kernel: u32,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => {
            println!("[app] PASS — the dispatch crossed the wire and the bytes did not");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[app] FAIL: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("no Vulkan loader: {e}"))?;
    let app = vk::ApplicationInfo::default()
        .application_name(c"sv-compute-e2e")
        .api_version(vk::API_VERSION_1_1);
    let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }
        .map_err(|e| format!("vkCreateInstance: {e:?}"))?;

    let pds = unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| format!("vkEnumeratePhysicalDevices: {e:?}"))?;
    let &pd = pds.first().ok_or("our ICD enumerated no physical device")?;
    let props = unsafe { instance.get_physical_device_properties(pd) };
    let name = unsafe { std::ffi::CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy();
    println!("[app] the ICD reports: {name}");

    let prio = [1.0f32];
    let qi = [vk::DeviceQueueCreateInfo::default().queue_family_index(0).queue_priorities(&prio)];
    let device = unsafe { instance.create_device(pd, &vk::DeviceCreateInfo::default().queue_create_infos(&qi), None) }
        .map_err(|e| format!("vkCreateDevice: {e:?}"))?;
    let queue = unsafe { device.get_device_queue(0, 0) };

    let r = work(&instance, pd, &device, queue);
    unsafe {
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    r
}

fn work(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    d: &ash::Device,
    queue: vk::Queue,
) -> Result<(), String> {
    // -- buffers, and one allocation under both of them ---------------------
    let bci = vk::BufferCreateInfo::default()
        .size(HALF)
        .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buf_in = unsafe { d.create_buffer(&bci, None) }
        .map_err(|e| format!("vkCreateBuffer(in): {e:?}"))?;
    let buf_out = unsafe { d.create_buffer(&bci, None) }
        .map_err(|e| format!("vkCreateBuffer(out): {e:?}"))?;
    let req = unsafe { d.get_buffer_memory_requirements(buf_in) };
    println!(
        "[app] vkGetBufferMemoryRequirements: size {} align {} typeBits 0x{:x}",
        req.size, req.alignment, req.memory_type_bits
    );

    // Pick a HOST_VISIBLE type from *our* table (design.md §9.1: types 1..3 are
    // the arena, type 0 is device-local and unmappable).
    let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
    let ti = (0..mem_props.memory_type_count)
        .find(|i| {
            req.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[*i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
        })
        .ok_or("no host-visible memory type in the ICD's table")?;
    println!("[app] allocating {TOTAL} bytes from memory type {ti}");

    let mem = unsafe {
        d.allocate_memory(
            &vk::MemoryAllocateInfo::default().allocation_size(TOTAL).memory_type_index(ti),
            None,
        )
    }
    .map_err(|e| format!("vkAllocateMemory: {e:?}"))?;

    // The two binds that exercise §5.3's rewrite: offset 0 and offset HALF of
    // one app-side allocation, which the GPU side has to turn into two offsets
    // into the arena's single `VkDeviceMemory`.
    unsafe { d.bind_buffer_memory(buf_in, mem, 0) }
        .map_err(|e| format!("vkBindBufferMemory(in @0): {e:?}"))?;
    unsafe { d.bind_buffer_memory(buf_out, mem, HALF) }
        .map_err(|e| format!("vkBindBufferMemory(out @{HALF}): {e:?}"))?;

    // -- write the pattern, through a pointer into shared pages -------------
    let ptr = unsafe { d.map_memory(mem, 0, TOTAL, vk::MemoryMapFlags::empty()) }
        .map_err(|e| format!("vkMapMemory: {e:?}"))? as *mut u8;
    let stamp: u64 = 0x5356_434F_4D50_5531; // "SVCOMPU1"
    let words = (HALF / 4) as u32;
    for i in 0..words {
        let v = ref_gpu::host_fill(stamp, i);
        unsafe { ptr.add(i as usize * 4).cast::<u32>().write_unaligned(v) };
    }
    // Poison the output half, so "the GPU wrote nothing" and "the GPU wrote
    // zeros" are different answers.
    unsafe { std::ptr::write_bytes(ptr.add(HALF as usize), 0xCD, HALF as usize) };
    println!("[app] wrote {HALF} bytes through vkMapMemory's pointer");

    // -- the pipeline -------------------------------------------------------
    let code: Vec<u32> =
        ARENA_SPV.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    if code.is_empty() {
        return Err("this build has no compute shader (glslangValidator missing)".into());
    }
    let module =
        unsafe { d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None) }
            .map_err(|e| format!("vkCreateShaderModule: {e:?}"))?;

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
    .map_err(|e| format!("vkCreateDescriptorSetLayout: {e:?}"))?;

    let sizes =
        [vk::DescriptorPoolSize::default().ty(vk::DescriptorType::STORAGE_BUFFER).descriptor_count(2)];
    let pool = unsafe {
        d.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&sizes),
            None,
        )
    }
    .map_err(|e| format!("vkCreateDescriptorPool: {e:?}"))?;
    let layouts = [set_layout];
    let set = unsafe {
        d.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default().descriptor_pool(pool).set_layouts(&layouts),
        )
    }
    .map_err(|e| format!("vkAllocateDescriptorSets: {e:?}"))?[0];

    let in_info = [vk::DescriptorBufferInfo::default().buffer(buf_in).offset(0).range(HALF)];
    let out_info = [vk::DescriptorBufferInfo::default().buffer(buf_out).offset(0).range(HALF)];
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
    .map_err(|e| format!("vkCreatePipelineLayout: {e:?}"))?;
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(module)
        .name(c"main");
    let infos = [vk::ComputePipelineCreateInfo::default().stage(stage).layout(pipeline_layout)];
    let pipeline = unsafe { d.create_compute_pipelines(vk::PipelineCache::null(), &infos, None) }
        .map_err(|(_, e)| format!("vkCreateComputePipelines: {e:?}"))?[0];

    // -- record, submit, wait ------------------------------------------------
    let cmd_pool = unsafe {
        d.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(0), None)
    }
    .map_err(|e| format!("vkCreateCommandPool: {e:?}"))?;
    let cmd = unsafe {
        d.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(cmd_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }
    .map_err(|e| format!("vkAllocateCommandBuffers: {e:?}"))?[0];

    let push = Push { words, kernel: ref_gpu::KERNEL_TRANSFORM };
    unsafe {
        d.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .map_err(|e| format!("vkBeginCommandBuffer: {e:?}"))?;
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
        d.cmd_dispatch(cmd, 64, 1, 1);
        d.end_command_buffer(cmd).map_err(|e| format!("vkEndCommandBuffer: {e:?}"))?;
    }
    println!("[app] recorded {} commands, all of them ring packets", 5);

    let fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None) }
        .map_err(|e| format!("vkCreateFence: {e:?}"))?;
    let cmds = [cmd];
    let submits = [vk::SubmitInfo::default().command_buffers(&cmds)];
    // The queue-ring packet that must be ordered after every device-ring packet
    // above it — the `RingBarrier` of §4.4, emitted by `BarrierTracker`.
    unsafe { d.queue_submit(queue, &submits, fence) }
        .map_err(|e| format!("vkQueueSubmit: {e:?}"))?;
    unsafe { d.wait_for_fences(&[fence], true, 10_000_000_000) }
        .map_err(|e| format!("vkWaitForFences: {e:?}"))?;
    println!("[app] the fence signalled");

    // -- read the GPU's answer back out of our own mapping ------------------
    let mut wrong = 0u64;
    let mut untouched = 0u64;
    let mut checked = 0u64;
    let mut first_bad = None;
    for i in 0..words {
        if !ref_gpu::is_payload(i) {
            continue;
        }
        let got =
            unsafe { ptr.add(HALF as usize + i as usize * 4).cast::<u32>().read_unaligned() };
        let want = ref_gpu::transform(ref_gpu::host_fill(stamp, i), i);
        checked += 1;
        if got == 0xCDCD_CDCD {
            untouched += 1;
        } else if got != want {
            wrong += 1;
            first_bad.get_or_insert((i, got, want));
        }
    }
    println!("[app] checked {checked} payload words: {wrong} wrong, {untouched} untouched");

    unsafe {
        d.destroy_fence(fence, None);
        d.destroy_command_pool(cmd_pool, None);
        d.destroy_pipeline(pipeline, None);
        d.destroy_pipeline_layout(pipeline_layout, None);
        d.destroy_shader_module(module, None);
        d.destroy_descriptor_pool(pool, None);
        d.destroy_descriptor_set_layout(set_layout, None);
        d.unmap_memory(mem);
        d.free_memory(mem, None);
        d.destroy_buffer(buf_in, None);
        d.destroy_buffer(buf_out, None);
    }

    if untouched == checked {
        return Err("every payload word still holds the poison value: the shader ran \
                    against memory this process is not looking at"
            .into());
    }
    if wrong != 0 || untouched != 0 {
        let (i, got, want) = first_bad.unwrap_or((0, 0, 0));
        return Err(format!(
            "{wrong} wrong, {untouched} untouched of {checked}; first bad word {i}: \
             got 0x{got:08x}, expected 0x{want:08x}"
        ));
    }
    Ok(())
}
