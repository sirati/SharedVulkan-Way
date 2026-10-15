//! `VK_KHR_swapchain`, implemented entirely on the app side.
//!
//! docs/design.md §10 stage 2, with one correction to it that runs through the whole
//! file and is worth stating first.
//!
//! # The correction: presentation needs no protocol
//!
//! §10 stage 2 says "on `vkQueuePresentKHR` the app side sends `svPresentBlit`;
//! the GPU side runs `vkCmdCopyImageToBuffer` into a triple-buffered linear
//! arena buffer and signals a present timeline value". That describes a new
//! non-Vulkan command (§6.3), a new GPU-side handler, and a timeline the app
//! side waits on.
//!
//! **None of it is necessary, and building it would have been a mistake.** The
//! app side can already issue every Vulkan command there is — that is what the
//! ICD *is* — so it can create the image, create the buffer, record the copy and
//! submit it, all out of the existing wire. The GPU side is never told that any
//! of this is a swapchain and needs no code at all: `sv-gpu-server` is unchanged
//! by this stage. What §10 called `svPresentBlit` is a `vkQueueSubmit` of a
//! command buffer recorded once at swapchain creation, so a frame costs **one**
//! round trip rather than one per command.
//!
//! The general rule, which is the mirror of §0.5's "each side may rewrite
//! exactly what only it knows": **a capability that is expressible in the
//! forwarded API does not deserve a protocol extension.** The two places this
//! project *did* need one — `vkUpdateDescriptorSetWithTemplate`'s `pData` and
//! `vkCreateBuffer`'s external-memory chain — are both cases where one side held
//! information the API gives the other side no way to name. Presentation is not
//! one of those.
//!
//! # What a swapchain is here
//!
//! Per image, four objects and one mapping:
//!
//! | object | where | why |
//! |---|---|---|
//! | `VkImage` | GPU side, memory type 0 | what the application renders into |
//! | `VkDeviceMemory` | GPU side, heap 0 | §9.1: `DEVICE_LOCAL`, never mapped |
//! | `VkBuffer` | arena | where the finished frame lands |
//! | `VkCommandBuffer` | GPU side, recorded once | the copy, and its two barriers |
//! | `VkFence` | GPU side | how this side knows the copy has landed |
//!
//! The application's frame therefore crosses the boundary **once**, as a
//! `vkCmdCopyImageToBuffer` into memory the app side already has mapped. There
//! is no second copy on this side: [`crate::present`] reads the arena bytes
//! directly.
//!
//! # Why the image is not itself in the arena
//!
//! It is the obvious thing to want — a swapchain image rendered straight into
//! arena memory is visible to the app side with no copy at all — and it is
//! measurably *possible* on this hardware: RADV reports
//! `COLOR_ATTACHMENT | BLIT_SRC | TRANSFER_SRC` in `linearTilingFeatures` for
//! `VK_FORMAT_B8G8R8A8_UNORM`, so a linear, arena-backed colour attachment is
//! admissible. It is not what this does, for three reasons in increasing order
//! of weight.
//!
//! 1. **It moves the render target out of `DEVICE_LOCAL`.** §9.1 puts plain
//!    `DEVICE_LOCAL` first in the memory table precisely "so that render targets
//!    do not land in the arena". A swapchain image is the render target of every
//!    frame; making it linear host memory trades one copy per frame for every
//!    pixel write of every draw going through the slower path.
//! 2. **It drags `VkExternalMemoryImageCreateInfo` onto every forwarded image.**
//!    §0.5 item 3 spells out the consequence and calls it a load-bearing
//!    property of §9.1's ordering: an image bound into arena memory needs the
//!    same treatment `vkCreateBuffer` already gets on the GPU side
//!    (`VUID-vkBindImageMemory-memory-02728`), and it "cannot be conditional",
//!    so *every* image the application creates would become external-capable and
//!    a driver would be entitled to narrow their `memoryTypeBits`.
//! 3. **The copy is not where the time goes.** See the measurement in
//!    `docs/measured-gpu-side.md`; on this machine a 1080p `vkCmdCopyImageToBuffer`
//!    is a small fraction of the round trip that carries it.
//!
//! So the honest fallback of §10 is what is built, and the zero-copy variant is
//! recorded as measured-possible rather than assumed-impossible.

use crate::state::SharedVulkan;
use sv_icd_entry::{Icd, IcdLocal, IcdResult, LowerError, PresentTarget, SurfaceCaps, SwapchainDesc};
use sv_proto as p;

/// `VK_FORMAT_B8G8R8A8_UNORM`, and the format everything here prefers.
///
/// It is first in [`surface_formats`] because it is what DXVK asks for, what
/// Zink takes when offered, and what an X11 `ZPixmap` on a 24-bit TrueColor
/// visual already is — so the app-side blit is a `memcpy` rather than a shuffle.
const B8G8R8A8_UNORM: i32 = 44;
const B8G8R8A8_SRGB: i32 = 50;
const R8G8B8A8_UNORM: i32 = 37;
const R8G8B8A8_SRGB: i32 = 43;
/// `VK_COLOR_SPACE_SRGB_NONLINEAR_KHR`, the only colour space that is core.
const SRGB_NONLINEAR: i32 = 0;

/// Bytes per pixel, for every format this implementation offers.
const BPP: u64 = 4;

/// Say what the WSI did, under `SV_DEBUG`.
///
/// Presentation is the one part of this ICD whose failures an application
/// reports in its own words rather than ours — Mesa says "zink: could not
/// create swapchain" and Wine says nothing at all — so a trace of what was
/// asked for and what came back is worth more here than anywhere else in the
/// crate.
macro_rules! trace {
    ($($arg:tt)*) => {
        sv_icd_entry::report_transport_failure(&format_args!($($arg)*))
    };
}

/// One image of a swapchain, and everything that belongs to it.
struct SwapImage {
    image: u64,
    memory: u64,
    buffer: u64,
    buffer_memory: u64,
    command_buffer: u64,
    fence: u64,
    /// The arena address of the readback buffer, as a `usize` so that [`State`]
    /// stays `Send`. It is a pointer into the arena mapping, which outlives
    /// every swapchain (docs/design.md §5.3: the arena is mapped for the process's
    /// lifetime), so it does not dangle when this struct moves.
    pixels: usize,
}

pub struct Swapchain {
    device: u64,
    queue: u64,
    target: crate::present::Target,
    /// What the surface was when this swapchain was created. Compared against
    /// the surface's size on every acquire, which is how the application is
    /// told to rebuild one (`VK_SUBOPTIMAL_KHR`).
    present_target: PresentTarget,
    width: u32,
    height: u32,
    /// `true` when the application chose an `R8G8B8A8` format, so the app side
    /// must swap red and blue on the way to the display. Zero cost when false,
    /// which is the case for every consumer measured so far.
    swap_rb: bool,
    images: Vec<SwapImage>,
    pool: u64,
    /// Round-robin acquire cursor. Present is synchronous — it waits for its own
    /// fence — so an image is free the moment the present that used it returns,
    /// and there is no in-flight set to track.
    next: usize,
}

impl Swapchain {
    fn bytes(&self) -> usize {
        self.width as usize * self.height as usize * BPP as usize
    }
}

/// `vkGetPhysicalDeviceSurfaceCapabilitiesKHR`'s `currentExtent`.
///
/// `0xFFFF_FFFF` in both fields is the specification's "the surface size will be
/// determined by the extent of a swapchain targeting the surface", which is the
/// truth for a headless surface and a lie for a window.
pub fn surface_capabilities(target: PresentTarget) -> IcdResult<SurfaceCaps> {
    let caps = match crate::present::Target::extent(target) {
        Some((w, h)) => SurfaceCaps { current_width: w, current_height: h },
        None => SurfaceCaps { current_width: u32::MAX, current_height: u32::MAX },
    };
    trace!("vkGetPhysicalDeviceSurfaceCapabilitiesKHR: {target:?} -> {caps:?}");
    Ok(caps)
}

/// `vkGetPhysicalDeviceSurfaceFormatsKHR`.
///
/// Four formats and one colour space, and the list is the app side's alone: the
/// GPU-side driver's own surface formats describe a window system it is not
/// attached to. What constrains this list is not the driver but the *blit* —
/// every entry has to be something [`crate::present`] can put in a window, which
/// on X11 means 32 bits per pixel with the channels in one of two orders.
pub fn surface_formats(_target: PresentTarget) -> Vec<(i32, i32)> {
    trace!("vkGetPhysicalDeviceSurfaceFormatsKHR: {_target:?}");
    vec![
        (B8G8R8A8_UNORM, SRGB_NONLINEAR),
        (B8G8R8A8_SRGB, SRGB_NONLINEAR),
        (R8G8B8A8_UNORM, SRGB_NONLINEAR),
        (R8G8B8A8_SRGB, SRGB_NONLINEAR),
    ]
}

/// `vkGetPhysicalDeviceSurfacePresentModesKHR`.
///
/// All three of the modes that do not need a refresh clock, and they are
/// currently the same thing: presentation here is synchronous and unqueued, so
/// `IMMEDIATE`, `MAILBOX` and `FIFO` describe a queue of length zero. `FIFO`
/// must be in the list — the specification requires every surface to support it
/// — and the other two are honest rather than aspirational, because a present
/// that has already completed by the time the call returns never tears and never
/// drops a frame.
pub fn surface_present_modes(_target: PresentTarget) -> Vec<i32> {
    trace!("vkGetPhysicalDeviceSurfacePresentModesKHR: {_target:?}");
    vec![2 /* FIFO */, 0 /* IMMEDIATE */, 1 /* MAILBOX */]
}

/// `vkCreateSwapchainKHR`.
pub fn create_swapchain(icd: &SharedVulkan, device: u64, desc: &SwapchainDesc) -> IcdResult<u64> {
    trace!(
        "vkCreateSwapchainKHR: {:?} {}x{} format {} usage {:#x} images {} mode {}",
        desc.target,
        desc.width,
        desc.height,
        desc.format,
        desc.image_usage,
        desc.min_image_count,
        desc.present_mode
    );
    if icd.wire().is_none() {
        // A swapchain image is a real `VkImage` on a real device. With no GPU
        // side there is nothing to create it on, and refusing here is much
        // better than a swapchain whose images are ids nothing backs.
        return Err(LowerError::NotImplemented("vkCreateSwapchainKHR without a GPU side"));
    }
    if desc.width == 0 || desc.height == 0 {
        return Err(LowerError::Shape("vkCreateSwapchainKHR: zero extent"));
    }
    let queue = {
        let s = icd.state();
        s.devices
            .get(&device)
            .and_then(|d| d.queues.first().copied())
            .ok_or(LowerError::Shape("vkCreateSwapchainKHR: unknown device"))?
    };
    let count = desc
        .min_image_count
        .clamp(sv_icd_entry::MIN_SWAPCHAIN_IMAGES, sv_icd_entry::MAX_SWAPCHAIN_IMAGES);

    // One pool for the swapchain's copy command buffers. `RESET_COMMAND_BUFFER`
    // is not set: each buffer is recorded exactly once, at creation, and
    // re-recording it every frame is the cost this design exists to avoid.
    let pool = {
        let mut ci = p::VkCommandPoolCreateInfo::default();
        ci.queue_family_index = 0;
        let r = icd.create_command_pool(p::VkCreateCommandPoolArgs {
            device: p::VkDevice(device),
            p_create_info: Some(Box::new(ci)),
            p_command_pool: None,
        })?;
        result_ok(r.result)?;
        r.p_command_pool.ok_or(LowerError::Shape("vkCreateCommandPool: no pool"))?.0
    };

    let mut alloc = p::VkCommandBufferAllocateInfo::default();
    alloc.command_pool = p::VkCommandPool(pool);
    alloc.level = p::VkCommandBufferLevel(0); // PRIMARY
    alloc.command_buffer_count = count;
    let cmds = IcdLocal::allocate_command_buffers(icd, device, &alloc)?;

    let mut images = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let img = create_image(icd, device, desc)?;
        let (buffer, buffer_memory, pixels) = create_readback(icd, device, desc)?;
        let fence = {
            let r = icd.create_fence(p::VkCreateFenceArgs {
                device: p::VkDevice(device),
                p_create_info: Some(Box::new(p::VkFenceCreateInfo::default())),
                p_fence: None,
            })?;
            result_ok(r.result)?;
            r.p_fence.ok_or(LowerError::Shape("vkCreateFence: no fence"))?.0
        };
        let command_buffer =
            *cmds.get(i).ok_or(LowerError::Shape("vkAllocateCommandBuffers: short"))?;
        record_copy(icd, command_buffer, img.0, buffer, desc.width, desc.height)?;
        images.push(SwapImage {
            image: img.0,
            memory: img.1,
            buffer,
            buffer_memory,
            command_buffer,
            fence,
            pixels,
        });
    }

    let id = IcdLocal::mint_id(icd);
    icd.swapchains().insert(
        id,
        Swapchain {
            device,
            queue,
            target: crate::present::Target::open(desc.target, desc.width, desc.height),
            present_target: desc.target,
            width: desc.width,
            height: desc.height,
            swap_rb: desc.format == R8G8B8A8_UNORM || desc.format == R8G8B8A8_SRGB,
            images,
            pool,
            next: 0,
        },
    );
    Ok(id)
}

/// The `VkImage` the application renders into, plus its heap-0 memory.
///
/// `TRANSFER_SRC` is added to whatever the application asked for, because the
/// present path copies out of this image and the application has no reason to
/// know that. `vkGetPhysicalDeviceSurfaceCapabilitiesKHR` advertises the bit as
/// supported, so an application that echoes `supportedUsageFlags` back — as
/// several do — asks for it anyway.
fn create_image(icd: &SharedVulkan, device: u64, desc: &SwapchainDesc) -> IcdResult<(u64, u64)> {
    let mut ci = p::VkImageCreateInfo::default();
    ci.image_type = p::VkImageType(1); // 2D
    ci.format = p::VkFormat(desc.format);
    ci.extent = p::VkExtent3D { width: desc.width, height: desc.height, depth: 1 };
    ci.mip_levels = 1;
    ci.array_layers = 1;
    ci.samples = p::VkSampleCountFlagBits::VK_SAMPLE_COUNT_1_BIT;
    ci.tiling = p::VkImageTiling(0); // OPTIMAL
    ci.usage = p::VkImageUsageFlags(desc.image_usage)
        | p::VkImageUsageFlags::VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
    ci.sharing_mode = p::VkSharingMode(0); // EXCLUSIVE
    ci.initial_layout = p::VkImageLayout::VK_IMAGE_LAYOUT_UNDEFINED;

    let r = icd.create_image(p::VkCreateImageArgs {
        device: p::VkDevice(device),
        p_create_info: Some(Box::new(ci)),
        p_image: None,
    })?;
    result_ok(r.result)?;
    let image = r.p_image.ok_or(LowerError::Shape("vkCreateImage: no image"))?.0;

    let req = icd.get_image_memory_requirements(p::VkGetImageMemoryRequirementsArgs {
        device: p::VkDevice(device),
        image: p::VkImage(image),
        p_memory_requirements: None,
    })?;
    let size = req
        .p_memory_requirements
        .map(|m| m.size)
        .filter(|s| *s > 0)
        .unwrap_or(desc.width as u64 * desc.height as u64 * BPP);

    // Memory type 0 — `DEVICE_LOCAL`, never mapped — and chosen by us rather
    // than read out of `memoryTypeBits`. §9.1 is a table this side invented, and
    // the mask that came back describes the *driver's* numbering; the two agree
    // only by luck. Here there is no need to consult it at all, because there is
    // exactly one right answer for a render target.
    let mut ai = p::VkMemoryAllocateInfo::default();
    ai.allocation_size = size;
    ai.memory_type_index = crate::advertise::memory_type::DEVICE_LOCAL;
    let m = icd.allocate_memory(p::VkAllocateMemoryArgs {
        device: p::VkDevice(device),
        p_allocate_info: Some(Box::new(ai)),
        p_memory: None,
    })?;
    result_ok(m.result)?;
    let memory = m.p_memory.ok_or(LowerError::Shape("vkAllocateMemory: no memory"))?.0;

    let b = icd.bind_image_memory(p::VkBindImageMemoryArgs {
        device: p::VkDevice(device),
        image: p::VkImage(image),
        memory: p::VkDeviceMemory(memory),
        memory_offset: 0,
    })?;
    result_ok(b.result)?;
    Ok((image, memory))
}

/// The arena buffer a finished frame lands in, and its mapped address.
fn create_readback(
    icd: &SharedVulkan,
    device: u64,
    desc: &SwapchainDesc,
) -> IcdResult<(u64, u64, usize)> {
    let len = desc.width as u64 * desc.height as u64 * BPP;
    let mut ci = p::VkBufferCreateInfo::default();
    ci.size = len;
    ci.usage = p::VkBufferUsageFlags::VK_BUFFER_USAGE_TRANSFER_DST_BIT;
    ci.sharing_mode = p::VkSharingMode(0);
    let r = icd.create_buffer(p::VkCreateBufferArgs {
        device: p::VkDevice(device),
        p_create_info: Some(Box::new(ci)),
        p_buffer: None,
    })?;
    result_ok(r.result)?;
    let buffer = r.p_buffer.ok_or(LowerError::Shape("vkCreateBuffer: no buffer"))?.0;

    let req = icd.get_buffer_memory_requirements(p::VkGetBufferMemoryRequirementsArgs {
        device: p::VkDevice(device),
        buffer: p::VkBuffer(buffer),
        p_memory_requirements: None,
    })?;
    let size = req.p_memory_requirements.map(|m| m.size).filter(|s| *s >= len).unwrap_or(len);

    // Type 2 — `HOST_VISIBLE | HOST_COHERENT | HOST_CACHED` — because this
    // buffer is written by the GPU and read by the CPU exactly once per frame,
    // which is the readback case §9.1 puts that type there for.
    let mut ai = p::VkMemoryAllocateInfo::default();
    ai.allocation_size = size;
    ai.memory_type_index = crate::advertise::memory_type::HOST_CACHED;
    let m = icd.allocate_memory(p::VkAllocateMemoryArgs {
        device: p::VkDevice(device),
        p_allocate_info: Some(Box::new(ai)),
        p_memory: None,
    })?;
    result_ok(m.result)?;
    let memory = m.p_memory.ok_or(LowerError::Shape("vkAllocateMemory: no memory"))?.0;

    let b = icd.bind_buffer_memory(p::VkBindBufferMemoryArgs {
        device: p::VkDevice(device),
        buffer: p::VkBuffer(buffer),
        memory: p::VkDeviceMemory(memory),
        memory_offset: 0,
    })?;
    result_ok(b.result)?;

    // The whole point of the project, used by the project itself: the presented
    // frame is readable here with no copy and no round trip (docs/design.md §5.3).
    let ptr = IcdLocal::map_memory(icd, device, memory, 0, len)?;
    Ok((buffer, memory, ptr as usize))
}

/// Record the copy, once, at swapchain creation.
///
/// `PRESENT_SRC_KHR` on both sides is not decoration. The specification requires
/// the application to have transitioned the image to that layout before it
/// presents, so it is the correct `oldLayout`; and transitioning back means the
/// next frame's barrier is the same one, so the buffer can be recorded once and
/// replayed forever. It is also the reason `VK_KHR_swapchain` is in the profile
/// (`vk_registry::profile::zink`): the layout does not exist on a GPU-side
/// device that never enabled the extension.
fn record_copy(
    icd: &SharedVulkan,
    cmd: u64,
    image: u64,
    buffer: u64,
    width: u32,
    height: u32,
) -> IcdResult<()> {
    let r = icd.begin_command_buffer(p::VkBeginCommandBufferArgs {
        command_buffer: p::VkCommandBuffer(cmd),
        p_begin_info: Some(Box::new(p::VkCommandBufferBeginInfo::default())),
    })?;
    result_ok(r.result)?;

    let range = p::VkImageSubresourceRange {
        aspect_mask: p::VkImageAspectFlags::VK_IMAGE_ASPECT_COLOR_BIT,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    let barrier = |old: i32, new: i32, src: u32, dst: u32| {
        let mut b = p::VkImageMemoryBarrier::default();
        b.src_access_mask = p::VkAccessFlags(src);
        b.dst_access_mask = p::VkAccessFlags(dst);
        b.old_layout = p::VkImageLayout(old);
        b.new_layout = p::VkImageLayout(new);
        b.src_queue_family_index = u32::MAX; // VK_QUEUE_FAMILY_IGNORED
        b.dst_queue_family_index = u32::MAX;
        b.image = p::VkImage(image);
        b.subresource_range = range.clone();
        b
    };
    let present_src = p::VkImageLayout::VK_IMAGE_LAYOUT_PRESENT_SRC_KHR.0;
    let transfer_src = p::VkImageLayout::VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL.0;
    let memory_read = p::VkAccessFlags::VK_ACCESS_MEMORY_READ_BIT.0;
    let transfer_read = p::VkAccessFlags::VK_ACCESS_TRANSFER_READ_BIT.0;
    let all_commands = p::VkPipelineStageFlags::VK_PIPELINE_STAGE_ALL_COMMANDS_BIT;
    let transfer = p::VkPipelineStageFlags::VK_PIPELINE_STAGE_TRANSFER_BIT;

    icd.cmd_pipeline_barrier(p::VkCmdPipelineBarrierArgs {
        command_buffer: p::VkCommandBuffer(cmd),
        src_stage_mask: all_commands,
        dst_stage_mask: transfer,
        dependency_flags: p::VkDependencyFlags(0),
        memory_barrier_count: 0,
        p_memory_barriers: Vec::new(),
        buffer_memory_barrier_count: 0,
        p_buffer_memory_barriers: Vec::new(),
        image_memory_barrier_count: 1,
        p_image_memory_barriers: vec![barrier(
            present_src,
            transfer_src,
            memory_read,
            transfer_read,
        )],
    })?;

    let region = p::VkBufferImageCopy {
        buffer_offset: 0,
        // Zero means "tightly packed to `imageExtent.width`", which is the
        // third of docs/design.md §10's improvements over sharedgl: the stride is
        // the actual width rather than the configured maximum.
        buffer_row_length: 0,
        buffer_image_height: 0,
        image_subresource: p::VkImageSubresourceLayers {
            aspect_mask: p::VkImageAspectFlags::VK_IMAGE_ASPECT_COLOR_BIT,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        },
        image_offset: p::VkOffset3D { x: 0, y: 0, z: 0 },
        image_extent: p::VkExtent3D { width, height, depth: 1 },
    };
    icd.cmd_copy_image_to_buffer(p::VkCmdCopyImageToBufferArgs {
        command_buffer: p::VkCommandBuffer(cmd),
        src_image: p::VkImage(image),
        src_image_layout: p::VkImageLayout(transfer_src),
        dst_buffer: p::VkBuffer(buffer),
        region_count: 1,
        p_regions: vec![region],
    })?;

    icd.cmd_pipeline_barrier(p::VkCmdPipelineBarrierArgs {
        command_buffer: p::VkCommandBuffer(cmd),
        src_stage_mask: transfer,
        dst_stage_mask: all_commands,
        dependency_flags: p::VkDependencyFlags(0),
        memory_barrier_count: 0,
        p_memory_barriers: Vec::new(),
        buffer_memory_barrier_count: 0,
        p_buffer_memory_barriers: Vec::new(),
        image_memory_barrier_count: 1,
        p_image_memory_barriers: vec![barrier(
            transfer_src,
            present_src,
            transfer_read,
            memory_read,
        )],
    })?;

    let r = icd.end_command_buffer(p::VkEndCommandBufferArgs {
        command_buffer: p::VkCommandBuffer(cmd),
    })?;
    result_ok(r.result)
}

/// `vkGetSwapchainImagesKHR`.
pub fn swapchain_images(icd: &SharedVulkan, swapchain: u64) -> IcdResult<Vec<u64>> {
    let chains = icd.swapchains();
    let sc = chains
        .get(&swapchain)
        .ok_or(LowerError::Shape("vkGetSwapchainImagesKHR: unknown swapchain"))?;
    Ok(sc.images.iter().map(|i| i.image).collect())
}

/// `vkAcquireNextImageKHR`.
///
/// **Acquire is about timing, not about data**, and the timing it has to
/// express is "this image is yours now". Every image is free the instant the
/// present that used it returned — [`queue_present`] waits for its own fence —
/// so the index is a round-robin cursor and the only work is *signalling*.
///
/// Signalling is the part that has to cross. The semaphore and the fence the
/// application passes are GPU-side objects, and the only way to signal one from
/// here is to submit something that signals it. An empty `vkQueueSubmit` — no
/// command buffers, no waits — is exactly that, and it is what every WSI
/// implementation that does not own the presentation engine does.
pub fn acquire_next_image(
    icd: &SharedVulkan,
    device: u64,
    swapchain: u64,
    _timeout: u64,
    semaphore: u64,
    fence: u64,
) -> IcdResult<u32> {
    let (index, queue) = {
        let mut chains = icd.swapchains();
        let sc = chains
            .get_mut(&swapchain)
            .ok_or(LowerError::Shape("vkAcquireNextImageKHR: unknown swapchain"))?;
        let index = sc.next;
        sc.next = (sc.next + 1) % sc.images.len().max(1);
        (index as u32, sc.queue)
    };
    let _ = device;
    if semaphore != 0 || fence != 0 {
        let mut si = p::VkSubmitInfo::default();
        if semaphore != 0 {
            si.signal_semaphore_count = 1;
            si.p_signal_semaphores = vec![p::VkSemaphore(semaphore)];
        }
        let r = icd.queue_submit(p::VkQueueSubmitArgs {
            queue: p::VkQueue(queue),
            submit_count: 1,
            p_submits: vec![si],
            fence: p::VkFence(fence),
        })?;
        result_ok(r.result)?;
    }
    Ok(index)
}

/// `vkQueuePresentKHR`.
///
/// Four steps and one round trip: reset the fence, submit the pre-recorded copy
/// behind the application's wait semaphores, wait for it, and hand the arena
/// bytes to the display target.
///
/// The wait is what makes this synchronous, and it is deliberate at this stage
/// rather than an oversight. The alternative — return immediately and blit on a
/// later frame — needs the timeline mirror of §9.3, which is not wired to
/// submissions yet; and `vkQueueSubmit` on this side is itself still synchronous
/// (§0.3, `icd::queue_submit`), so a present that did not wait would still cost
/// a round trip and would additionally be able to show a half-written frame.
pub fn queue_present(
    icd: &SharedVulkan,
    queue: u64,
    swapchain: u64,
    image_index: u32,
    waits: &[u64],
) -> IcdResult<()> {
    let (device, cmd, fence, pixels, len, width, height, swap_rb) = {
        let chains = icd.swapchains();
        let sc = chains
            .get(&swapchain)
            .ok_or(LowerError::Shape("vkQueuePresentKHR: unknown swapchain"))?;
        let img = sc
            .images
            .get(image_index as usize)
            .ok_or(LowerError::Shape("vkQueuePresentKHR: image index out of range"))?;
        (
            sc.device,
            img.command_buffer,
            img.fence,
            img.pixels,
            sc.bytes(),
            sc.width,
            sc.height,
            sc.swap_rb,
        )
    };

    let started = std::time::Instant::now();
    let r = icd.reset_fences(p::VkResetFencesArgs {
        device: p::VkDevice(device),
        fence_count: 1,
        p_fences: vec![p::VkFence(fence)],
    })?;
    result_ok(r.result)?;

    let mut si = p::VkSubmitInfo::default();
    si.wait_semaphore_count = waits.len() as u32;
    si.p_wait_semaphores = waits.iter().map(|w| p::VkSemaphore(*w)).collect();
    // `TRANSFER` and not `ALL_COMMANDS`: the only thing this submission does is
    // the copy, so that is the stage that has to wait for the application's
    // rendering to have finished.
    si.p_wait_dst_stage_mask =
        waits.iter().map(|_| p::VkPipelineStageFlags::VK_PIPELINE_STAGE_TRANSFER_BIT).collect();
    si.command_buffer_count = 1;
    si.p_command_buffers = vec![p::VkCommandBuffer(cmd)];
    let r = icd.queue_submit(p::VkQueueSubmitArgs {
        queue: p::VkQueue(queue),
        submit_count: 1,
        p_submits: vec![si],
        fence: p::VkFence(fence),
    })?;
    result_ok(r.result)?;

    let r = icd.wait_for_fences(p::VkWaitForFencesArgs {
        device: p::VkDevice(device),
        fence_count: 1,
        p_fences: vec![p::VkFence(fence)],
        wait_all: true,
        timeout: u64::MAX,
    })?;
    result_ok(r.result)?;

    // SAFETY: `pixels` is the address `vkMapMemory` returned for this buffer's
    // arena allocation, whose length is at least `len` by construction in
    // `create_readback`, and the arena mapping outlives every swapchain
    // (docs/design.md §5.3). The GPU side's write to it is ordered before this read
    // by the fence just waited on.
    let bytes = unsafe { core::slice::from_raw_parts(pixels as *const u8, len) };
    let fenced = std::time::Instant::now();
    {
        let mut chains = icd.swapchains();
        if let Some(sc) = chains.get_mut(&swapchain) {
            sc.target.present(bytes, width, height, swap_rb);
        }
    }
    let shown = std::time::Instant::now();
    frame_note(swapchain, bytes, width, height, fenced - started, shown - fenced);
    Ok(())
}

/// Say, occasionally and only under `SV_DEBUG`, whether the frame about to be
/// displayed has anything in it.
///
/// **This exists because "presented" and "visible" are different claims and
/// this project has now confused them twice.** A swapchain whose every entry
/// point returns `VK_SUCCESS`, whose fence signals and whose blit runs, still
/// shows a black window if the application rendered nothing into the image —
/// and from the outside that is indistinguishable from a broken display target.
/// One number tells the two apart, and it is the *only* number that does
/// without a screenshot.
///
/// Sampled rather than summed: a 4K frame is 33 MB and this runs sixty times a
/// second, so it reads one pixel per 4 KiB — enough to tell "all zero" from
/// "an image" and cheap enough to leave in.
///
/// It also says what the present itself cost, in two parts (docs/design.md
/// §0.28): `fence`, from `vkResetFences` to the end of `vkWaitForFences` — three
/// forwarded calls, the GPU side's `vkCmdCopyImageToBuffer` into the arena, and
/// waiting for the application's own rendering, which the copy waits on — and
/// `copy`, the display target's CPU copy out of the arena and its commit. The
/// second is what a zero-copy present would remove; the first it would not.
fn frame_note(
    swapchain: u64,
    bytes: &[u8],
    width: u32,
    height: u32,
    fence: std::time::Duration,
    copy: std::time::Duration,
) {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    const EVERY: u64 = 60;
    static COUNT: AtomicU64 = AtomicU64::new(0);
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    static FENCE_NS: AtomicU64 = AtomicU64::new(0);
    static COPY_NS: AtomicU64 = AtomicU64::new(0);

    FENCE_NS.fetch_add(fence.as_nanos() as u64, Ordering::Relaxed);
    COPY_NS.fetch_add(copy.as_nanos() as u64, Ordering::Relaxed);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n % EVERY != 0 {
        return;
    }
    let frames = if n == 0 { 1.0 } else { EVERY as f64 };
    let fence_ms = FENCE_NS.swap(0, Ordering::Relaxed) as f64 / frames / 1e6;
    let copy_ms = COPY_NS.swap(0, Ordering::Relaxed) as f64 / frames / 1e6;
    // The rate is the other half, and it is what turns "Firefox gave up" into a
    // number that can be attributed. It is measured between notes rather than
    // around one present, so it includes everything a frame costs on this side:
    // the submit, the fence wait, the round trip that carries them, and the
    // display target's own work.
    let now = std::time::Instant::now();
    let per_frame = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let d = last.map(|t| now.duration_since(t).as_secs_f64() / EVERY as f64);
        *last = Some(now);
        d
    };
    let mut nonzero = 0usize;
    let mut sampled = 0usize;
    for px in bytes.chunks_exact(4).step_by(1024) {
        sampled += 1;
        if px[0] | px[1] | px[2] != 0 {
            nonzero += 1;
        }
    }
    match per_frame {
        Some(s) => trace!(
            "vkQueuePresentKHR: swapchain {swapchain:#x} frame {n} {width}x{height} — \
             {nonzero}/{sampled} sampled pixels are not black, {:.1} ms/frame ({:.1} fps); \
             present: fence {fence_ms:.2} ms, copy {copy_ms:.2} ms",
            s * 1e3,
            1.0 / s
        ),
        None => trace!(
            "vkQueuePresentKHR: swapchain {swapchain:#x} frame {n} {width}x{height} — \
             {nonzero}/{sampled} sampled pixels are not black, first frame"
        ),
    }
}

/// `vkDestroySwapchainKHR`.
pub fn destroy_swapchain(icd: &SharedVulkan, device: u64, swapchain: u64) {
    let Some(sc) = icd.swapchains().remove(&swapchain) else { return };
    // Everything below is best-effort: `vkDestroySwapchainKHR` returns void, and
    // an application that destroys a swapchain while a present is in flight has
    // already been told not to. A failed teardown leaks a GPU-side object rather
    // than corrupting one.
    let _ = icd.device_wait_idle(p::VkDeviceWaitIdleArgs { device: p::VkDevice(device) });
    for i in &sc.images {
        let _ = icd.destroy_fence(p::VkDestroyFenceArgs {
            device: p::VkDevice(device),
            fence: p::VkFence(i.fence),
        });
        let _ = icd.destroy_image(p::VkDestroyImageArgs {
            device: p::VkDevice(device),
            image: p::VkImage(i.image),
        });
        let _ = icd.free_memory(p::VkFreeMemoryArgs {
            device: p::VkDevice(device),
            memory: p::VkDeviceMemory(i.memory),
        });
        let _ = icd.destroy_buffer(p::VkDestroyBufferArgs {
            device: p::VkDevice(device),
            buffer: p::VkBuffer(i.buffer),
        });
        let _ = icd.free_memory(p::VkFreeMemoryArgs {
            device: p::VkDevice(device),
            memory: p::VkDeviceMemory(i.buffer_memory),
        });
    }
    let cmds: Vec<u64> = sc.images.iter().map(|i| i.command_buffer).collect();
    IcdLocal::free_command_buffers(icd, device, sc.pool, &cmds);
    let _ = icd.destroy_command_pool(p::VkDestroyCommandPoolArgs {
        device: p::VkDevice(device),
        command_pool: p::VkCommandPool(sc.pool),
    });
    let _ = sc.present_target;
}

fn result_ok(r: p::VkResult) -> IcdResult<()> {
    if r.0 == 0 { Ok(()) } else { Err(LowerError::Result(r.0)) }
}
