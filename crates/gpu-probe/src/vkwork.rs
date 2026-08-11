//! Giving the GPU real work against imported memory.
//!
//! Everything up to now has measured that the driver *accepts* our pages: the
//! import succeeds, `vkMapMemory` returns the same virtual address, and both
//! CPUs see the same bytes (`docs/measured-gpu-side.md`). An import is not a
//! submission. Nothing in that chain has exercised the GPU's own address
//! translation, and R15 says so.
//!
//! So: bind the imported memory to a `VkBuffer`, dispatch a compute shader that
//! reads every word and writes a value that depends on the word *and* its
//! index, wait on a fence, and then check the result from both CPU sides.
//!
//! This module is deliberately OS-agnostic. It takes a pointer and a size and
//! does not care whether they came from `VirtualAlloc(MEM_LARGE_PAGES)` in a
//! Windows guest or from `posix_memalign` on the machine that built it — which
//! means the whole Vulkan path, and the shader in particular, can be dry-run
//! against a local GPU before any of it is pointed at the laptop panel that
//! somebody is looking at (R1, R18).
//!
//! ## What is deliberately not here
//!
//! No secondary command buffers, no timeline semaphores, no pipeline cache, no
//! multiple queues. Every one of those is a way for a first GPU submission to
//! fail for a reason that has nothing to do with the memory it is reading.

use arena_record::gpu;
use ash::vk;
use std::ffi::{CStr, c_void};
use std::time::{Duration, Instant};

/// SPIR-V produced from `shaders/arena.comp` by the build script.
static SPV_BYTES: &[u8] = include_bytes!(env!("ARENA_SPV"));

/// Never wait forever. A GPU hang on this hardware is visible to a person
/// sitting at the laptop, and an unbounded `vkWaitForFences` would turn a
/// recoverable "we gave up" into a wedged probe holding a device open.
pub const FENCE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum Fail {
    /// Something the driver refused. Carries the call so a report can name it.
    Vk(&'static str, vk::Result),
    /// The one outcome that must stop the run rather than be retried.
    DeviceLost(&'static str),
    /// The fence did not signal inside [`FENCE_TIMEOUT`].
    Timeout,
    Other(String),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Vk(what, r) => write!(f, "{what}: {r:?}"),
            Fail::DeviceLost(what) => write!(f, "{what}: VK_ERROR_DEVICE_LOST"),
            Fail::Timeout => write!(f, "the fence did not signal within {FENCE_TIMEOUT:?}"),
            Fail::Other(s) => write!(f, "{s}"),
        }
    }
}

/// Classify a driver result so a device loss is never mistaken for an ordinary
/// error and quietly retried.
fn vkerr(what: &'static str, r: vk::Result) -> Fail {
    if r == vk::Result::ERROR_DEVICE_LOST {
        Fail::DeviceLost(what)
    } else {
        Fail::Vk(what, r)
    }
}

/// One open device, plus the facts about it a report should quote.
///
/// `entry` is held rather than read: it owns the loaded `vulkan-1.dll` /
/// `libvulkan.so`, and dropping it while an instance is live would unload the
/// loader out from under the driver.
#[allow(dead_code)]
pub struct Gpu {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub pd: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    pub name: String,
    pub has_host_import: bool,
    pub max_storage_buffer_range: u32,
    pub max_workgroup_count_x: u32,
    host_mem: Option<ash::ext::external_memory_host::Device>,
}

impl Gpu {
    pub fn open(device_index: usize) -> Result<Gpu, Fail> {
        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| Fail::Other(format!("no Vulkan loader: {e}")))?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"shared_vulkan gpu-probe")
            .api_version(vk::API_VERSION_1_1);
        let ci = vk::InstanceCreateInfo::default().application_info(&app);
        let instance = unsafe { entry.create_instance(&ci, None) }
            .map_err(|e| vkerr("vkCreateInstance", e))?;

        let pds = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| vkerr("vkEnumeratePhysicalDevices", e))?;
        let Some(&pd) = pds.get(device_index) else {
            return Err(Fail::Other(format!(
                "no physical device {device_index} ({} present)",
                pds.len()
            )));
        };
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
            .to_string_lossy()
            .to_string();

        let exts = unsafe { instance.enumerate_device_extension_properties(pd) }
            .map_err(|e| vkerr("vkEnumerateDeviceExtensionProperties", e))?;
        let has_host_import = exts.iter().any(|e| {
            (unsafe { CStr::from_ptr(e.extension_name.as_ptr()) })
                == c"VK_EXT_external_memory_host"
        });

        // Any family with COMPUTE will do. On this iGPU there is one family and
        // it is also the one the compositor uses, which is precisely why the
        // workload has to stay short.
        let qfams = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        let queue_family = qfams
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .ok_or_else(|| Fail::Other("no compute-capable queue family".into()))?
            as u32;

        let prio = [1.0f32];
        let qi = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&prio)];
        let names = [c"VK_EXT_external_memory_host".as_ptr()];
        let mut dci = vk::DeviceCreateInfo::default().queue_create_infos(&qi);
        if has_host_import {
            dci = dci.enabled_extension_names(&names);
        }
        let device = unsafe { instance.create_device(pd, &dci, None) }
            .map_err(|e| vkerr("vkCreateDevice", e))?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let host_mem = has_host_import
            .then(|| ash::ext::external_memory_host::Device::new(&instance, &device));

        Ok(Gpu {
            mem_props: unsafe { instance.get_physical_device_memory_properties(pd) },
            max_storage_buffer_range: props.limits.max_storage_buffer_range,
            max_workgroup_count_x: props.limits.max_compute_work_group_count[0],
            entry,
            instance,
            pd,
            device,
            queue,
            queue_family,
            name,
            has_host_import,
            host_mem,
        })
    }

    /// Import a host pointer as `VkDeviceMemory`.
    ///
    /// The memory-type choice follows gfxstream's defensive rewrite that
    /// `design.md` §5.4 adopts: ask the driver which types it will permit for
    /// this pointer and take one of those, rather than picking a type and
    /// hoping. Measured on this hardware the mask is always `0x6`.
    pub fn import(&self, ptr: *mut c_void, size: usize) -> Result<Memory, Fail> {
        let host_mem = self
            .host_mem
            .as_ref()
            .ok_or_else(|| Fail::Other("VK_EXT_external_memory_host is not present".into()))?;
        let mut hp = vk::MemoryHostPointerPropertiesEXT::default();
        let res = unsafe {
            (host_mem.fp().get_memory_host_pointer_properties_ext)(
                self.device.handle(),
                vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
                ptr as *const c_void,
                &mut hp,
            )
        };
        if res != vk::Result::SUCCESS {
            return Err(vkerr("vkGetMemoryHostPointerPropertiesEXT", res));
        }
        if hp.memory_type_bits == 0 {
            return Err(Fail::Other(
                "the driver permits no memory type for this pointer".into(),
            ));
        }
        let ti = self
            .host_visible_type(hp.memory_type_bits)
            .ok_or_else(|| Fail::Other("no host-visible type among the permitted ones".into()))?;

        let mut imp = vk::ImportMemoryHostPointerInfoEXT::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)
            .host_pointer(ptr);
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(size as u64)
            .memory_type_index(ti)
            .push_next(&mut imp);
        let mem = unsafe { self.device.allocate_memory(&ai, None) }
            .map_err(|e| vkerr("vkAllocateMemory(import)", e))?;
        Ok(Memory {
            mem,
            type_index: ti,
            size,
            imported: true,
            permitted_type_bits: hp.memory_type_bits,
        })
    }

    /// Ordinary driver-owned memory of a named type — the baseline the imported
    /// case is compared against.
    pub fn allocate(&self, size: usize, type_index: u32) -> Result<Memory, Fail> {
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(size as u64)
            .memory_type_index(type_index);
        let mem = unsafe { self.device.allocate_memory(&ai, None) }
            .map_err(|e| vkerr("vkAllocateMemory", e))?;
        Ok(Memory {
            mem,
            type_index,
            size,
            imported: false,
            permitted_type_bits: 1 << type_index,
        })
    }

    pub fn host_visible_type(&self, mask: u32) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|i| {
            mask & (1 << i) != 0
                && self.mem_props.memory_types[*i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
        })
    }

    /// The most device-local type there is: `DEVICE_LOCAL` and nothing else if
    /// such a type exists, otherwise whatever is device-local. On a unified iGPU
    /// this is type 0, which no application can map — the honest "what the
    /// driver would have given you" comparison.
    pub fn device_local_type(&self) -> Option<u32> {
        let types = &self.mem_props.memory_types[..self.mem_props.memory_type_count as usize];
        types
            .iter()
            .position(|t| t.property_flags == vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .or_else(|| {
                types
                    .iter()
                    .position(|t| t.property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL))
            })
            .map(|i| i as u32)
    }

    pub fn free(&self, m: Memory) {
        unsafe { self.device.free_memory(m.mem, None) };
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[allow(dead_code)] // several fields exist to be quoted in a report, not read by code
pub struct Memory {
    pub mem: vk::DeviceMemory,
    pub type_index: u32,
    pub size: usize,
    pub imported: bool,
    /// What `vkGetMemoryHostPointerPropertiesEXT` said, so a report can quote it
    /// next to what the *buffer* permits — those two masks being different is
    /// the failure mode this whole step is looking for.
    pub permitted_type_bits: u32,
}

/// What binding the two buffers to one allocation actually reported.
///
/// This is the gate. `vkGetBufferMemoryRequirements` returns its own
/// `memoryTypeBits`, and nothing guarantees that it intersects the mask the
/// *pointer* import permitted. If it does not, imported memory can be allocated
/// and mapped but never bound to a buffer, and the arena would be usable for
/// CPU sharing and useless for GPU work. Recording the two masks side by side
/// makes that visible whether or not it happens.
#[allow(dead_code)] // ditto: this struct is evidence, not control flow
pub struct BindReport {
    pub buffer_type_bits: u32,
    pub memory_type_bits: u32,
    pub alignment: u64,
    pub required_size: u64,
    pub bound_size: u64,
    pub second_buffer_offset: u64,
    pub compatible: bool,
}

pub struct Work {
    pub buf_in: vk::Buffer,
    pub buf_out: vk::Buffer,
    pub bind: BindReport,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    module: vk::ShaderModule,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    groups: u32,
}

/// Push constants, laid out exactly as `shaders/arena.comp` declares them.
#[repr(C)]
#[derive(Clone, Copy)]
struct Push {
    words: u32,
    kernel: u32,
}

impl Work {
    /// Two buffers over one allocation: input at offset 0, output at
    /// `half_bytes`. Binding the second at a non-zero offset is not incidental
    /// — a real arena hands out suballocations, so "can a buffer live part-way
    /// into imported memory?" is a question the design needs answered.
    pub fn build(gpu: &Gpu, mem: &Memory, half_bytes: usize) -> Result<Work, Fail> {
        let d = &gpu.device;
        if half_bytes as u64 > gpu.max_storage_buffer_range as u64 {
            return Err(Fail::Other(format!(
                "half the window is {half_bytes} bytes but maxStorageBufferRange is {}",
                gpu.max_storage_buffer_range
            )));
        }
        let bci = vk::BufferCreateInfo::default()
            .size(half_bytes as u64)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buf_in =
            unsafe { d.create_buffer(&bci, None) }.map_err(|e| vkerr("vkCreateBuffer(in)", e))?;
        let buf_out =
            unsafe { d.create_buffer(&bci, None) }.map_err(|e| vkerr("vkCreateBuffer(out)", e))?;

        let req = unsafe { d.get_buffer_memory_requirements(buf_in) };
        let compatible = req.memory_type_bits & (1 << mem.type_index) != 0
            && (half_bytes as u64).is_multiple_of(req.alignment);
        let bind = BindReport {
            buffer_type_bits: req.memory_type_bits,
            memory_type_bits: mem.permitted_type_bits,
            alignment: req.alignment,
            required_size: req.size,
            bound_size: half_bytes as u64,
            second_buffer_offset: half_bytes as u64,
            compatible,
        };
        if !compatible {
            unsafe {
                d.destroy_buffer(buf_in, None);
                d.destroy_buffer(buf_out, None);
            }
            return Err(Fail::Other(format!(
                "the buffer cannot live in this memory: buffer memoryTypeBits=0x{:x}, \
                 allocation is type {} (mask 0x{:x}), alignment {} vs offset {}",
                req.memory_type_bits, mem.type_index, mem.permitted_type_bits, req.alignment,
                half_bytes
            )));
        }

        unsafe { d.bind_buffer_memory(buf_in, mem.mem, 0) }
            .map_err(|e| vkerr("vkBindBufferMemory(in @0)", e))?;
        unsafe { d.bind_buffer_memory(buf_out, mem.mem, half_bytes as u64) }
            .map_err(|e| vkerr("vkBindBufferMemory(out @half)", e))?;

        // --- descriptors ---------------------------------------------------
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
        let slci = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        let set_layout = unsafe { d.create_descriptor_set_layout(&slci, None) }
            .map_err(|e| vkerr("vkCreateDescriptorSetLayout", e))?;

        let sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(2)];
        let pci = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&sizes);
        let pool = unsafe { d.create_descriptor_pool(&pci, None) }
            .map_err(|e| vkerr("vkCreateDescriptorPool", e))?;
        let layouts = [set_layout];
        let dai = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let set = unsafe { d.allocate_descriptor_sets(&dai) }
            .map_err(|e| vkerr("vkAllocateDescriptorSets", e))?[0];

        let bi_in = [vk::DescriptorBufferInfo::default()
            .buffer(buf_in)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let bi_out = [vk::DescriptorBufferInfo::default()
            .buffer(buf_out)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&bi_in),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&bi_out),
        ];
        unsafe { d.update_descriptor_sets(&writes, &[]) };

        // --- pipeline --------------------------------------------------------
        let words = spv_words()?;
        let smci = vk::ShaderModuleCreateInfo::default().code(&words);
        let module = unsafe { d.create_shader_module(&smci, None) }
            .map_err(|e| vkerr("vkCreateShaderModule", e))?;

        let ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(std::mem::size_of::<Push>() as u32)];
        let plci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&ranges);
        let pipeline_layout = unsafe { d.create_pipeline_layout(&plci, None) }
            .map_err(|e| vkerr("vkCreatePipelineLayout", e))?;

        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(c"main");
        let cpci = [vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(pipeline_layout)];
        let pipeline = unsafe { d.create_compute_pipelines(vk::PipelineCache::null(), &cpci, None) }
            .map_err(|(_, e)| vkerr("vkCreateComputePipelines", e))?[0];

        // --- command buffer ----------------------------------------------------
        let cpci2 = vk::CommandPoolCreateInfo::default()
            .queue_family_index(gpu.queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let cmd_pool = unsafe { d.create_command_pool(&cpci2, None) }
            .map_err(|e| vkerr("vkCreateCommandPool", e))?;
        let cbai = vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cmd = unsafe { d.allocate_command_buffers(&cbai) }
            .map_err(|e| vkerr("vkAllocateCommandBuffers", e))?[0];
        let fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(|e| vkerr("vkCreateFence", e))?;

        // Enough groups to cover the region, clamped to what the driver accepts.
        // The shader is a grid-stride loop, so a clamp costs iterations rather
        // than correctness.
        let total_words = (half_bytes / 4) as u32;
        let groups = total_words
            .div_ceil(256)
            .clamp(1, gpu.max_workgroup_count_x.min(65535));

        Ok(Work {
            buf_in,
            buf_out,
            bind,
            set_layout,
            pool,
            set,
            pipeline_layout,
            pipeline,
            module,
            cmd_pool,
            cmd,
            fence,
            groups,
        })
    }

    /// Record and submit one dispatch, and wait for it with a bounded timeout.
    ///
    /// Returns the wall time from just before `vkQueueSubmit` to just after the
    /// fence signals. That includes submission overhead, which is measured
    /// separately by dispatching over zero words, so the two numbers can be
    /// subtracted honestly rather than one being quoted as if it were the other.
    pub fn dispatch(&self, gpu: &Gpu, words: u32, kernel: u32) -> Result<Duration, Fail> {
        let d = &gpu.device;
        let push = Push { words, kernel };
        unsafe {
            d.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                .map_err(|e| vkerr("vkResetCommandBuffer", e))?;
            let bi = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            d.begin_command_buffer(self.cmd, &bi)
                .map_err(|e| vkerr("vkBeginCommandBuffer", e))?;

            // Host writes that happened before the submit. Vulkan makes writes
            // by *this* process's CPU visible automatically at queue submit; the
            // writer we actually care about is a Linux process on the other side
            // of the hypervisor, which is outside Vulkan's model entirely and is
            // the thing being measured. The barrier costs nothing and states the
            // intent.
            let pre = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::HOST_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)];
            d.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::HOST,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &pre,
                &[],
                &[],
            );

            d.cmd_bind_pipeline(self.cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            d.cmd_bind_descriptor_sets(
                self.cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[self.set],
                &[],
            );
            let bytes = std::slice::from_raw_parts(
                &push as *const Push as *const u8,
                std::mem::size_of::<Push>(),
            );
            d.cmd_push_constants(
                self.cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytes,
            );
            d.cmd_dispatch(self.cmd, self.groups, 1, 1);

            // Make the shader's writes visible to a host read. On this hardware
            // the memory is HOST_COHERENT so no invalidate is required, but the
            // execution dependency is: without it, "the fence signalled" does
            // not order the writes against the CPU's reads.
            let post = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)];
            d.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &post,
                &[],
                &[],
            );
            d.end_command_buffer(self.cmd)
                .map_err(|e| vkerr("vkEndCommandBuffer", e))?;

            d.reset_fences(&[self.fence])
                .map_err(|e| vkerr("vkResetFences", e))?;
            let cmds = [self.cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            let t = Instant::now();
            d.queue_submit(gpu.queue, &submit, self.fence)
                .map_err(|e| vkerr("vkQueueSubmit", e))?;
            match d.wait_for_fences(&[self.fence], true, FENCE_TIMEOUT.as_nanos() as u64) {
                Ok(()) => Ok(t.elapsed()),
                Err(vk::Result::TIMEOUT) => Err(Fail::Timeout),
                Err(e) => Err(vkerr("vkWaitForFences", e)),
            }
        }
    }

    pub fn destroy(self, gpu: &Gpu) {
        let d = &gpu.device;
        unsafe {
            d.destroy_fence(self.fence, None);
            d.destroy_command_pool(self.cmd_pool, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.pipeline_layout, None);
            d.destroy_shader_module(self.module, None);
            d.destroy_descriptor_pool(self.pool, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_buffer(self.buf_in, None);
            d.destroy_buffer(self.buf_out, None);
        }
    }
}

/// SPIR-V is a little-endian word stream. Reading it byte-wise rather than
/// transmuting avoids any question about the alignment of a `include_bytes!`
/// blob, and checking the magic turns "the build script produced nonsense" into
/// an error here rather than a driver crash later.
fn spv_words() -> Result<Vec<u32>, Fail> {
    if !SPV_BYTES.len().is_multiple_of(4) || SPV_BYTES.len() < 20 {
        return Err(Fail::Other(format!(
            "the compiled shader is {} bytes, which is not a SPIR-V module",
            SPV_BYTES.len()
        )));
    }
    let words: Vec<u32> = SPV_BYTES
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    if words[0] != 0x0723_0203 {
        return Err(Fail::Other(format!(
            "the compiled shader starts with 0x{:08x}, not the SPIR-V magic",
            words[0]
        )));
    }
    Ok(words)
}

// ---------------------------------------------------------------------------
// checking the answer
// ---------------------------------------------------------------------------

/// What the GPU left behind, judged against what it was required to produce.
#[derive(Default, Clone)]
pub struct Verdict {
    pub checked: u64,
    pub correct: u64,
    /// Words that still hold what was there before the dispatch. Counted
    /// separately from "wrong", because "the GPU never wrote here" and "the GPU
    /// wrote the wrong thing" are completely different failures.
    pub untouched: u64,
    pub wrong: u64,
    /// Input words the GPU was supposed to read that did not hold what the
    /// writer said it wrote. Without this, a correct-looking failure could be
    /// blamed on the GPU when the input never arrived.
    pub input_bad: u64,
    pub examples: Vec<String>,
}

impl Verdict {
    fn note(&mut self, s: String) {
        if self.examples.len() < 4 {
            self.examples.push(s);
        }
    }
}

/// Check the output half against `transform(host_fill(stamp, i), i)` for every
/// payload word.
///
/// Deliberately predicts from `stamp` rather than from reading the input. If it
/// compared the output against a transform of whatever the input *currently*
/// holds, then a GPU reading a stale copy of a page and a CPU reading the same
/// stale copy would agree, and the check would pass on memory that is not
/// shared at all.
pub fn verify_transform(
    input: &[u8],
    output: &[u8],
    stamp: u64,
    words: u32,
    before: Option<&[u8]>,
) -> Verdict {
    let mut v = Verdict::default();
    for i in 0..words {
        if !gpu::is_payload(i) {
            continue;
        }
        let o = i as usize * 4;
        let expect = gpu::transform(gpu::host_fill(stamp, i), i);
        let got = u32::from_le_bytes(output[o..o + 4].try_into().unwrap());
        let inp = u32::from_le_bytes(input[o..o + 4].try_into().unwrap());
        if inp != gpu::host_fill(stamp, i) {
            v.input_bad += 1;
            if v.input_bad <= 2 {
                v.note(format!(
                    "input word {i}: expected 0x{:08x}, found 0x{inp:08x} — the writer's \
                     bytes never arrived, so the GPU cannot be blamed for this one",
                    gpu::host_fill(stamp, i)
                ));
            }
        }
        v.checked += 1;
        if got == expect {
            v.correct += 1;
        } else if before.is_some_and(|b| {
            u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) == got
        }) {
            v.untouched += 1;
            if v.untouched <= 2 {
                v.note(format!(
                    "output word {i}: still 0x{got:08x}, exactly what was there before the \
                     dispatch — the GPU did not write this word"
                ));
            }
        } else {
            v.wrong += 1;
            v.note(format!(
                "output word {i}: expected 0x{expect:08x}, got 0x{got:08x} \
                 (input word was 0x{inp:08x})"
            ));
        }
    }
    v
}

/// The concurrency check: while the writer churns the input between two stamps,
/// a copy kernel writes what it read. Every output word must therefore be one
/// of the two values the writer ever wrote.
///
/// A word that is neither is the only thing an incoherent or torn read could
/// produce, and it is a positive finding rather than an inference. A count of
/// zero is *not* proof of coherency under load — it is one bounded observation,
/// at 4-byte granularity, over whatever interleaving actually happened.
pub fn verify_concurrent_copy(output: &[u8], stamps: (u64, u64), words: u32) -> ConcurrentVerdict {
    let mut v = ConcurrentVerdict::default();
    for i in 0..words {
        if !gpu::is_payload(i) {
            continue;
        }
        let o = i as usize * 4;
        let got = u32::from_le_bytes(output[o..o + 4].try_into().unwrap());
        v.checked += 1;
        if got == gpu::host_fill(stamps.0, i) {
            v.saw_a += 1;
        } else if got == gpu::host_fill(stamps.1, i) {
            v.saw_b += 1;
        } else {
            v.neither += 1;
            if v.examples.len() < 4 {
                v.examples.push(format!(
                    "word {i}: 0x{got:08x} is neither A=0x{:08x} nor B=0x{:08x}",
                    gpu::host_fill(stamps.0, i),
                    gpu::host_fill(stamps.1, i)
                ));
            }
        }
    }
    v
}

#[derive(Default, Clone)]
pub struct ConcurrentVerdict {
    pub checked: u64,
    pub saw_a: u64,
    pub saw_b: u64,
    /// The number that matters. Anything above zero is a real result.
    pub neither: u64,
    pub examples: Vec<String>,
}

/// Fill the input half as the host would, for runs with no host in the loop.
pub fn fill_input(input: &mut [u8], stamp: u64, words: u32) {
    for i in 0..words {
        if !gpu::is_payload(i) {
            continue;
        }
        let o = i as usize * 4;
        input[o..o + 4].copy_from_slice(&gpu::host_fill(stamp, i).to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The build script produced a real module. Cheap, and it catches a
    /// glslangValidator that "succeeded" into an empty file.
    #[test]
    fn the_compiled_shader_is_spirv() {
        let w = spv_words().expect("the build script must produce SPIR-V");
        assert_eq!(w[0], 0x0723_0203);
        assert!(w.len() > 40, "a module of {} words is too small", w.len());
    }

    /// The checker must fail loudly on memory the GPU never touched. If a page
    /// of zeroes passed, every negative result in this experiment would be
    /// worthless.
    #[test]
    fn untouched_output_does_not_pass() {
        let words = 2048u32; // two pages
        let mut input = vec![0u8; words as usize * 4];
        fill_input(&mut input, 0x1234, words);
        let output = vec![0u8; words as usize * 4];
        let v = verify_transform(&input, &output, 0x1234, words, Some(&output));
        assert!(v.checked > 0);
        assert_eq!(v.correct, 0, "zeroed output must not be accepted");
        assert_eq!(v.untouched, v.checked);
        assert_eq!(v.input_bad, 0, "the input was written correctly");
    }

    /// ...and it must pass on the answer the shader is required to produce, or
    /// a real success would be reported as a failure.
    #[test]
    fn the_reference_answer_passes() {
        let words = 2048u32;
        let mut input = vec![0u8; words as usize * 4];
        fill_input(&mut input, 0xABCD, words);
        let mut output = vec![0u8; words as usize * 4];
        for i in 0..words {
            if !gpu::is_payload(i) {
                continue;
            }
            let o = i as usize * 4;
            let v = u32::from_le_bytes(input[o..o + 4].try_into().unwrap());
            output[o..o + 4].copy_from_slice(&gpu::transform(v, i).to_le_bytes());
        }
        let v = verify_transform(&input, &output, 0xABCD, words, None);
        assert_eq!(v.wrong, 0);
        assert_eq!(v.correct, v.checked);
        assert_eq!(v.checked, 2 * (1024 - 64));
    }

    /// A GPU that read the right values but got the index wrong — the
    /// off-by-a-page failure R21 predicts — must not pass.
    #[test]
    fn an_index_shifted_answer_does_not_pass() {
        let words = 2048u32;
        let mut input = vec![0u8; words as usize * 4];
        fill_input(&mut input, 7, words);
        let mut output = vec![0u8; words as usize * 4];
        for i in 0..words {
            if !gpu::is_payload(i) {
                continue;
            }
            let o = i as usize * 4;
            let v = u32::from_le_bytes(input[o..o + 4].try_into().unwrap());
            // right value, wrong index
            output[o..o + 4].copy_from_slice(&gpu::transform(v, i ^ 1).to_le_bytes());
        }
        let v = verify_transform(&input, &output, 7, words, None);
        assert_eq!(v.correct, 0);
        assert_eq!(v.wrong, v.checked);
    }

    /// The concurrency checker has to accept either writer value and reject a
    /// third one, or a torn read would be invisible.
    #[test]
    fn the_concurrency_checker_accepts_two_values_and_rejects_a_third() {
        let words = 1024u32;
        let (a, b) = (0x1111_1111u64, 0x2222_2222u64);
        let mut out = vec![0u8; words as usize * 4];
        for i in 0..words {
            if !gpu::is_payload(i) {
                continue;
            }
            let o = i as usize * 4;
            let v = if i % 2 == 0 {
                gpu::host_fill(a, i)
            } else {
                gpu::host_fill(b, i)
            };
            out[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
        let v = verify_concurrent_copy(&out, (a, b), words);
        assert_eq!(v.neither, 0);
        assert_eq!(v.saw_a + v.saw_b, v.checked);

        // Now corrupt one word the way a torn read would.
        out[64 * 4..64 * 4 + 4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let v = verify_concurrent_copy(&out, (a, b), words);
        assert_eq!(v.neither, 1, "a third value must be caught");
    }

    /// An input that never arrived must be attributed to the writer, not to the
    /// GPU — otherwise a host-side failure reads as a GPU failure.
    #[test]
    fn a_missing_input_is_attributed_to_the_writer() {
        let words = 1024u32;
        let input = vec![0u8; words as usize * 4];
        let output = vec![0u8; words as usize * 4];
        let v = verify_transform(&input, &output, 0x99, words, None);
        assert_eq!(v.input_bad, v.checked);
        assert!(v.examples.iter().any(|e| e.contains("never arrived")));
    }
}
