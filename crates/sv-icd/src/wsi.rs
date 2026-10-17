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
/// `VK_COLOR_SPACE_PASS_THROUGH_EXT` (`VK_EXT_swapchain_colorspace`): the bytes go
/// to the window as they are, which is what this WSI does for every format.
const PASS_THROUGH: i32 = 1_000_104_013;

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
    /// Shared with the presenter, which shows frames from its own thread.
    target: std::sync::Arc<std::sync::Mutex<crate::present::Target>>,
    /// `None` presents on the calling thread (see [`queue_present`]).
    presenter: Option<Presenter>,
    /// The Wayland surface's slot in `libsv_wlshim.so` (§100). Read inside
    /// `vkQueuePresentKHR` for the frame's generation, so that call never
    /// waits for the target's lock, which the presenter holds while it shows.
    wl_slot: Option<i32>,
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
    /// Round-robin acquire cursor. An image is free once the present that used
    /// it has been shown, which a synchronous present has done by the time it
    /// returns and a pipelined one says through [`Presenter::wait_for`].
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
/// Four formats in two colour spaces, and the list is the app side's alone: the
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
        // §96: the same four again, pass-through, which Zink asks for on
        // Wayland. sRGB-nonlinear stays first: it is what everything else picks.
        (B8G8R8A8_UNORM, PASS_THROUGH),
        (B8G8R8A8_SRGB, PASS_THROUGH),
        (R8G8B8A8_UNORM, PASS_THROUGH),
        (R8G8B8A8_SRGB, PASS_THROUGH),
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
            .and_then(|d| d.universal_queue())
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

    // **A swapchain that cannot be made whole gives back what it took**
    // (waycompose-design.md §95). Each image holds a readback buffer of the
    // whole window in the arena, so a creation that failed on the second
    // image (the arena full, which is exactly when a resize asks for a new
    // swapchain beside the old one) used to keep the first image's buffer
    // for the rest of the process, and make the next attempt fail sooner.
    let mut images: Vec<SwapImage> = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let mut part = SwapImage {
            image: 0,
            memory: 0,
            buffer: 0,
            buffer_memory: 0,
            command_buffer: cmds.get(i).copied().unwrap_or(0),
            fence: 0,
            pixels: 0,
        };
        let made = (|| -> IcdResult<()> {
            (part.image, part.memory) = create_image(icd, device, desc)?;
            (part.buffer, part.buffer_memory, part.pixels) = create_readback(icd, device, desc)?;
            let r = icd.create_fence(p::VkCreateFenceArgs {
                device: p::VkDevice(device),
                p_create_info: Some(Box::new(p::VkFenceCreateInfo::default())),
                p_fence: None,
            })?;
            result_ok(r.result)?;
            part.fence = r.p_fence.ok_or(LowerError::Shape("vkCreateFence: no fence"))?.0;
            if part.command_buffer == 0 {
                return Err(LowerError::Shape("vkAllocateCommandBuffers: short"));
            }
            record_copy(icd, part.command_buffer, part.image, part.buffer, desc.width, desc.height)
        })();
        images.push(part);
        if let Err(e) = made {
            release_images(icd, device, &images, pool, &cmds);
            return Err(e);
        }
    }

    let id = IcdLocal::mint_id(icd);
    let target = crate::present::Target::open(desc.target, desc.width, desc.height);
    let wl_slot = target.wl_slot();
    icd.swapchains().insert(
        id,
        Swapchain {
            device,
            queue,
            target: std::sync::Arc::new(std::sync::Mutex::new(target)),
            presenter: Presenter::start(icd, images.len(), &desc.target, wl_slot.is_some()),
            wl_slot,
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
    let (index, queue, busy) = {
        let mut chains = icd.swapchains();
        let sc = chains
            .get_mut(&swapchain)
            .ok_or(LowerError::Shape("vkAcquireNextImageKHR: unknown swapchain"))?;
        let index = sc.next;
        sc.next = (sc.next + 1) % sc.images.len().max(1);
        (index as u32, sc.queue, sc.presenter.as_ref().map(|p| std::sync::Arc::clone(&p.busy)))
    };
    // A pipelined present of this image may still be copying out of it.
    if let Some(busy) = busy {
        Presenter::wait_for(&busy, index as usize);
    }
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
/// Reset the image's fence and submit its pre-recorded copy behind the
/// application's wait semaphores — both without waiting for an answer — and
/// then either finish here or hand the rest to the swapchain's presenter.
///
/// # Pipelined (docs/waycompose-design.md §88)
///
/// The rest is the wait for the copy and the display target's copy out of the
/// arena: on the real guest 2.0 ms and 1.2 ms of every frame, spent on
/// Firefox's renderer thread with nothing else happening. With a presenter
/// that work runs on its own thread while the application builds its next
/// frame, and `vkAcquireNextImageKHR` is what waits, for that one image, if it
/// comes round before its copy is shown.
///
/// The presenter waits on the **queue** ring. A wait on the device ring would
/// hold that ring for the length of the copy, and the application's next
/// device-ring call would queue behind it; on the queue ring it is ordered
/// after the copy it waits for by construction, and the only thing it can
/// delay is the application's next submission, which cannot be sooner than
/// the next frame. The reset before it is on the device ring, and the queue
/// ring's barrier puts the wait after it.
///
/// **Not on a Wayland surface** (docs/waycompose-design.md §98): there the
/// frame is shown with requests on the application's own `wl_surface`, and
/// those may only be made inside the application's call. See [`pipelines`].
///
/// `SV_PRESENT_SYNC=1` keeps the old order, everything on the calling thread,
/// so the two can be compared in one binary.
pub fn queue_present(
    icd: &SharedVulkan,
    queue: u64,
    swapchain: u64,
    image_index: u32,
    waits: &[u64],
) -> IcdResult<()> {
    let (job, presenter) = {
        let chains = icd.swapchains();
        let sc = chains
            .get(&swapchain)
            .ok_or(LowerError::Shape("vkQueuePresentKHR: unknown swapchain"))?;
        let img = sc
            .images
            .get(image_index as usize)
            .ok_or(LowerError::Shape("vkQueuePresentKHR: image index out of range"))?;
        (
            Job {
                swapchain,
                image_index,
                device: sc.device,
                fence: img.fence,
                pixels: img.pixels,
                len: sc.bytes(),
                width: sc.width,
                height: sc.height,
                swap_rb: sc.swap_rb,
                target: std::sync::Arc::clone(&sc.target),
                cmd: img.command_buffer,
                started: std::time::Instant::now(),
                reset: None,
                submitted: None,
                app: None,
                // Read here, inside the application's call, where the surface
                // is alive and cannot be unmapped under us (§100).
                generation: sc.wl_slot.map(wl_generation),
            },
            sc.presenter.as_ref().map(|p| (p.tx.clone(), std::sync::Arc::clone(&p.busy))),
        )
    };
    let mut job = job;

    job.started = std::time::Instant::now();
    job.app = traced_app_interval(swapchain, job.started);
    let r = icd.reset_fences(p::VkResetFencesArgs {
        device: p::VkDevice(job.device),
        fence_count: 1,
        p_fences: vec![p::VkFence(job.fence)],
    })?;
    result_ok(r.result)?;
    job.reset = Some(std::time::Instant::now());

    let mut si = p::VkSubmitInfo::default();
    si.wait_semaphore_count = waits.len() as u32;
    si.p_wait_semaphores = waits.iter().map(|w| p::VkSemaphore(*w)).collect();
    // `TRANSFER` and not `ALL_COMMANDS`: the only thing this submission does is
    // the copy, so that is the stage that has to wait for the application's
    // rendering to have finished.
    si.p_wait_dst_stage_mask =
        waits.iter().map(|_| p::VkPipelineStageFlags::VK_PIPELINE_STAGE_TRANSFER_BIT).collect();
    si.command_buffer_count = 1;
    si.p_command_buffers = vec![p::VkCommandBuffer(job.cmd)];
    let r = icd.queue_submit(p::VkQueueSubmitArgs {
        queue: p::VkQueue(queue),
        submit_count: 1,
        p_submits: vec![si],
        fence: p::VkFence(job.fence),
    })?;
    result_ok(r.result)?;
    let submitted = std::time::Instant::now();
    job.submitted = Some(submitted);

    if let Some((tx, busy)) = presenter {
        {
            let mut b = busy.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = b.get_mut(image_index as usize) {
                *slot = true;
            }
        }
        if let Err(back) = tx.send(job) {
            // The presenter is gone, which only teardown does; finish here.
            let job = back.0;
            busy.0.lock().unwrap_or_else(|e| e.into_inner())[image_index as usize] = false;
            busy.1.notify_all();
            return finish_present(icd, job, false);
        }
        if sv_transport::frametrace::enabled() {
            PRESENT_END.lock().unwrap_or_else(|e| e.into_inner()).insert(swapchain, submitted);
        }
        return Ok(());
    }
    finish_present(icd, job, false)
}

/// One present, from its copy being submitted to its frame being shown.
struct Job {
    swapchain: u64,
    image_index: u32,
    device: u64,
    fence: u64,
    cmd: u64,
    pixels: usize,
    len: usize,
    width: u32,
    height: u32,
    swap_rb: bool,
    target: std::sync::Arc<std::sync::Mutex<crate::present::Target>>,
    started: std::time::Instant,
    reset: Option<std::time::Instant>,
    submitted: Option<std::time::Instant>,
    app: Option<AppInterval>,
    /// The surface's shim generation inside this frame's `vkQueuePresentKHR`
    /// (§100); `None` when the target has no shim slot.
    generation: Option<u32>,
}

/// The images a presenter has not finished with, and the condition
/// `vkAcquireNextImageKHR` waits on.
type Busy = std::sync::Arc<(std::sync::Mutex<Vec<bool>>, std::sync::Condvar)>;

/// A swapchain's present thread (see [`queue_present`]).
pub(crate) struct Presenter {
    tx: std::sync::mpsc::Sender<Job>,
    busy: Busy,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Whether a swapchain on `target` may finish its presents on a presenter
/// thread, after `vkQueuePresentKHR` has returned. `sync` is `SV_PRESENT_SYNC`;
/// `guarded` says the Wayland surface is registered with `libsv_wlshim.so`.
///
/// # On Wayland only with the shim (docs/waycompose-design.md §98, §100)
///
/// A Wayland frame is shown with `attach`, `damage` and `commit` on the
/// application's `wl_surface`, a proxy the application owns and frees. Firefox
/// destroys that surface *before* the swapchain, on closing a window and on
/// quitting, and at quit it never destroys the swapchain at all. A presenter
/// that showed a frame after `vkQueuePresentKHR` returned marshalled on a
/// freed proxy: Firefox's crash at quit (§98). Nothing the ICD can see tells
/// it the surface is gone, and a proxy wrapper would only trade the crash for
/// a request on a deleted object id, a protocol error.
///
/// What the application does to its surface *is* visible to a library standing
/// in front of libwayland. `libsv_wlshim.so`, preloaded, marks a registered
/// surface dead before forwarding its destroy, and counts its unmaps, under
/// the lock the presenter holds around its own three requests (§100). With
/// it, a Wayland swapchain pipelines; without it, the frame is shown inside
/// the call, as Mesa's own WSI does, at the cost §99.9 measured.
///
/// X11 and headless always pipeline: an X11 frame goes out on a connection of
/// our own, by a window id whose death is an error that connection ignores.
/// `SV_PRESENT_SYNC=1` presents in the call everywhere; `SV_PRESENT_SYNC=0`
/// pipelines even an unguarded Wayland surface, which is §98's crash, kept to
/// measure what the synchronous present costs.
fn pipelines(target: &PresentTarget, sync: Option<&std::ffi::OsStr>, guarded: bool) -> bool {
    match sync {
        Some(v) if v == "0" => true,
        Some(_) => false,
        None => guarded || !matches!(target, PresentTarget::Wayland { .. }),
    }
}

impl Presenter {
    /// Only for the process's one ICD: the thread outlives the call that made
    /// it and needs an ICD that lives as long. `None` means present on the
    /// calling thread, which is what [`pipelines`] says for a Wayland surface
    /// without the shim and `SV_PRESENT_SYNC=1` asks for everywhere.
    fn start(icd: &SharedVulkan, images: usize, target: &PresentTarget, guarded: bool) -> Option<Presenter> {
        if !pipelines(target, std::env::var_os("SV_PRESENT_SYNC").as_deref(), guarded) {
            return None;
        }
        let global = crate::instance().filter(|g| std::ptr::eq(*g, icd))?;
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let busy: Busy =
            std::sync::Arc::new((std::sync::Mutex::new(vec![false; images]), std::sync::Condvar::new()));
        let theirs = std::sync::Arc::clone(&busy);
        let thread = std::thread::Builder::new()
            .name("sv-present".into())
            .spawn(move || {
                // This thread's waits are the present's own, not the
                // application's round trips (`WC_FRAME_TRACE`).
                crate::transport::untallied_on_this_thread();
                while let Ok(job) = rx.recv() {
                    let index = job.image_index as usize;
                    if let Err(e) = finish_present(global, job, true) {
                        trace!("vkQueuePresentKHR (presenter): {e:?}; that frame is dropped");
                    }
                    let mut b = theirs.0.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(slot) = b.get_mut(index) {
                        *slot = false;
                    }
                    theirs.1.notify_all();
                }
            })
            .ok()?;
        Some(Presenter { tx, busy, thread: Some(thread) })
    }

    /// Wait until `index`'s last present has been shown. Bounded: a presenter
    /// that never finishes is a GPU side that has stopped answering, and the
    /// application's next call will say so. Takes the shared state rather than
    /// `self` so the swapchain table is not held while it waits.
    fn wait_for(busy: &Busy, index: usize) {
        let (lock, cv) = &**busy;
        let b = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = cv.wait_timeout_while(b, std::time::Duration::from_secs(10), |b| {
            b.get(index).copied().unwrap_or(false)
        });
    }

    /// Finish every present already queued, then stop.
    fn stop(mut self) {
        drop(self.tx);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Wait for a present's copy and show the frame. `queue_ring` is the
/// presenter's choice of ring for the wait; the calling thread's present keeps
/// the device ring it always used.
fn finish_present(icd: &SharedVulkan, job: Job, queue_ring: bool) -> IcdResult<()> {
    let submitted = job.submitted.unwrap_or(job.started);
    let dequeued = std::time::Instant::now();
    let args = p::VkWaitForFencesArgs {
        device: p::VkDevice(job.device),
        fence_count: 1,
        p_fences: vec![p::VkFence(job.fence)],
        wait_all: true,
        // Bounded on the presenter, where nothing else would notice a copy
        // that never finished; the calling thread's wait is the application's
        // and keeps its `UINT64_MAX`.
        timeout: if queue_ring { 10_000_000_000 } else { u64::MAX },
    };
    let r = if queue_ring {
        wait_on_queue_ring(icd, &args)?
    } else {
        icd.wait_for_fences(args)?
    };
    result_ok(r.result)?;

    // SAFETY: `pixels` is the address `vkMapMemory` returned for this buffer's
    // arena allocation, whose length is at least `len` by construction in
    // `create_readback`, and the arena mapping outlives every swapchain
    // (docs/design.md §5.3). The GPU side's write to it is ordered before this read
    // by the fence just waited on, and the application cannot present this image
    // again — so the copy cannot be rewritten — until `vkAcquireNextImageKHR`
    // has seen this present finish.
    let bytes = unsafe { core::slice::from_raw_parts(job.pixels as *const u8, job.len) };
    let fenced = std::time::Instant::now();
    job.target
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .present(bytes, job.width, job.height, job.swap_rb, job.generation);
    let shown = std::time::Instant::now();
    let (swapchain, width, height, image_index) = (job.swapchain, job.width, job.height, job.image_index);
    frame_note(swapchain, bytes, width, height, fenced - job.started, shown - fenced);
    if let Some(app) = job.app {
        use sv_transport::frametrace as ft;
        // The ICD's half of the frame timeline (docs/waycompose-design.md §88).
        // `gap` is the application's own time between two presents, and
        // `rt`/`rt_us` the part of it spent waiting on the GPU side; the
        // phases after it are the present's. `queued_us` is how long a
        // pipelined present waited for its presenter.
        ft::emit_at(
            ft::at_us(job.started),
            "icd.present",
            format_args!(
                "sc={swapchain:#x} img={image_index} {width}x{height} gap_us={} rt={} rt_us={} sends={} \
                 top={} reset_us={} submit_us={} queued_us={} wait_us={} show_us={} pipelined={} end={}",
                app.gap_us,
                app.round_trips,
                app.round_trip_us,
                app.sends,
                app.top,
                ft::us(job.reset.unwrap_or(job.started) - job.started),
                ft::us(submitted - job.reset.unwrap_or(job.started)),
                ft::us(dequeued.saturating_duration_since(submitted)),
                ft::us(fenced - dequeued),
                ft::us(shown - fenced),
                queue_ring as u8,
                ft::at_us(shown),
            ),
        );
        if !queue_ring {
            PRESENT_END.lock().unwrap_or_else(|e| e.into_inner()).insert(swapchain, shown);
            // The present's own round trips are its phases above; they are
            // not the next frame's application time.
            crate::transport::ROUND_TRIPS.lock().unwrap_or_else(|e| e.into_inner()).clear();
            crate::transport::SENDS.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(())
}

/// `vkWaitForFences` as a call on the queue ring (see [`queue_present`]).
fn wait_on_queue_ring(icd: &SharedVulkan, args: &p::VkWaitForFencesArgs) -> IcdResult<p::VkWaitForFencesReply> {
    let Some(t) = icd.wire() else {
        return Ok(p::VkWaitForFencesReply { result: p::VkResult(0) });
    };
    let mut buf = vec![0u8; p::size_cmd_vk_wait_for_fences(args)];
    {
        let mut enc = sv_wire::Encoder::new(&mut buf);
        p::encode_cmd_vk_wait_for_fences(&mut enc, sv_wire::CMD_FLAG_GENERATE_REPLY, args)
            .map_err(LowerError::Wire)?;
        enc.finish().map_err(LowerError::Wire)?;
    }
    let mut reply = Vec::new();
    t.call(sv_icd_entry::RingClass::Queue, p::Opcode::VkWaitForFences as u32, &buf, &mut reply)?;
    let mut dec = sv_wire::Decoder::new(&reply);
    p::decode_reply_vk_wait_for_fences(&mut dec).map_err(LowerError::Wire)
}

/// When each swapchain's last present returned, for the gap to the next.
static PRESENT_END: std::sync::Mutex<std::collections::BTreeMap<u64, std::time::Instant>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// What the application did between its last present and this one, as the
/// transport counted it. `None` when `WC_FRAME_TRACE` is off.
struct AppInterval {
    gap_us: u64,
    round_trips: u64,
    round_trip_us: u64,
    sends: u64,
    top: String,
}

fn traced_app_interval(swapchain: u64, now: std::time::Instant) -> Option<AppInterval> {
    use std::sync::atomic::Ordering;
    if !sv_transport::frametrace::enabled() {
        return None;
    }
    let gap_us = PRESENT_END
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&swapchain)
        .map_or(0, |t| sv_transport::frametrace::us(now.saturating_duration_since(*t)));
    let mut tally = crate::transport::ROUND_TRIPS.lock().unwrap_or_else(|e| e.into_inner());
    let (round_trips, ns) = tally.total();
    let top = tally.top(4, |op| p::sv::opcode_name(op).into_owned());
    tally.clear();
    Some(AppInterval {
        gap_us,
        round_trips,
        round_trip_us: ns / 1000,
        sends: crate::transport::SENDS.swap(0, Ordering::Relaxed),
        top: if top.is_empty() { "-".into() } else { top },
    })
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
    if !n.is_multiple_of(EVERY) {
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

/// Destroy a swapchain's images, their readback buffers, the command buffers
/// and the pool: the half of [`destroy_swapchain`] that a creation which
/// failed part-way needs too.
///
/// Best-effort, for the reason `destroy_swapchain` gives. A handle that was
/// never created is 0, which every destroy here takes as `VK_NULL_HANDLE`.
fn release_images(icd: &SharedVulkan, device: u64, images: &[SwapImage], pool: u64, cmds: &[u64]) {
    for i in images {
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
    IcdLocal::free_command_buffers(icd, device, pool, cmds);
    let _ = icd.destroy_command_pool(p::VkDestroyCommandPoolArgs {
        device: p::VkDevice(device),
        command_pool: p::VkCommandPool(pool),
    });
}

/// `vkDestroySwapchainKHR`.
pub fn destroy_swapchain(icd: &SharedVulkan, device: u64, swapchain: u64) {
    let Some(mut sc) = icd.swapchains().remove(&swapchain) else { return };
    // Every queued present is finished first: they read this swapchain's
    // buffers and show on its target.
    if let Some(p) = sc.presenter.take() {
        p.stop();
    }
    // Everything below is best-effort: `vkDestroySwapchainKHR` returns void, and
    // an application that destroys a swapchain while a present is in flight has
    // already been told not to. A failed teardown leaks a GPU-side object rather
    // than corrupting one.
    let _ = icd.device_wait_idle(p::VkDeviceWaitIdleArgs { device: p::VkDevice(device) });
    let cmds: Vec<u64> = sc.images.iter().map(|i| i.command_buffer).collect();
    release_images(icd, device, &sc.images, sc.pool, &cmds);
    let _ = sc.present_target;
}

/// A Wayland surface's shim generation now (§100).
fn wl_generation(slot: i32) -> u32 {
    #[cfg(target_os = "linux")]
    {
        crate::present::wayland::guard::generation(slot)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = slot;
        0
    }
}

fn result_ok(r: p::VkResult) -> IcdResult<()> {
    if r.0 == 0 { Ok(()) } else { Err(LowerError::Result(r.0)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    const WAYLAND: PresentTarget = PresentTarget::Wayland { display: 0x1000, surface: 0x2000 };

    /// docs/waycompose-design.md §98. A pipelined Wayland present shows its
    /// frame on the application's `wl_surface` after `vkQueuePresentKHR` has
    /// returned, and Firefox destroys that surface before the swapchain: its
    /// crash at quit. Without the shim's guard that must not happen.
    #[test]
    fn an_unguarded_wayland_present_is_finished_before_vk_queue_present_returns() {
        assert!(
            !pipelines(&WAYLAND, None, false),
            "a Wayland swapchain without libsv_wlshim.so must not get a presenter: its \
             frames are shown with requests on the application's wl_surface, which the \
             application may free as soon as vkQueuePresentKHR returns"
        );
    }

    /// §100: with the shim the presenter is safe, and the frames come back.
    #[test]
    fn a_guarded_wayland_swapchain_gets_its_presenter_back() {
        assert!(pipelines(&WAYLAND, None, true));
    }

    #[test]
    fn x11_and_headless_keep_the_presenter() {
        for guarded in [false, true] {
            assert!(pipelines(&PresentTarget::X11 { window: 7 }, None, guarded));
            assert!(pipelines(&PresentTarget::Headless, None, guarded));
        }
    }

    #[test]
    fn sv_present_sync_still_chooses_for_every_target() {
        for t in [WAYLAND, PresentTarget::X11 { window: 7 }, PresentTarget::Headless] {
            for guarded in [false, true] {
                assert!(!pipelines(&t, Some(OsStr::new("1")), guarded), "{t:?}");
                assert!(pipelines(&t, Some(OsStr::new("0")), guarded), "{t:?}");
            }
        }
    }
}
