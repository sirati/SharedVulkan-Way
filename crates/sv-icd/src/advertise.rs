//! What we tell the application, and where each number comes from.
//!
//! docs/design.md §9 decides this and `measured-gpu-side.md` supplies the facts. The
//! rule the whole file follows: **a number here is either measured, derived from
//! a measured number, or conservative**. R23 is the risk — "the advertised
//! memory table is a claim about another machine" — and the mitigation is that
//! every claim says which.
//!
//! # The measured GPU side, for reference
//!
//! ```text
//! heaps:
//!   [0]   6624 MiB  DEVICE_LOCAL
//! memory types:
//!   [0] heap=0  DEVICE_LOCAL
//!   [1] heap=0  DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT
//!   [2] heap=0  DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT | HOST_CACHED
//! minImportedHostPointerAlignment: 4096
//! ```
//!
//! One heap, and every type but [0] is simultaneously `DEVICE_LOCAL` and
//! `HOST_VISIBLE`, because it is an integrated GPU with unified memory. That is
//! the single biggest simplification the measurement bought: "the awkward case a
//! forwarder normally has to handle — device-local memory the application wants
//! mapped — **does not arise on this hardware**" (docs/design.md §9.1).

use sv_icd_entry::*;

/// The GPU side's real heap size, measured (`measured-gpu-side.md`).
pub const MEASURED_DEVICE_HEAP: u64 = 6624 << 20;

/// `minImportedHostPointerAlignment`, measured.
pub const IMPORTED_HOST_POINTER_ALIGNMENT: u64 = 4096;

/// Memory type indices, in the order docs/design.md §9.1 fixes them.
///
/// **The order is load-bearing and is not alphabetical or arbitrary.**
/// Applications iterate the table and take the first type whose bits satisfy
/// them, and the specification only constrains ordering where one type's flags
/// are a strict subset of another's. §9.1 uses the freedom that leaves to put
/// plain `DEVICE_LOCAL` first, so render targets do not land in the arena.
pub mod memory_type {
    /// Never mapped: render targets, optimal-tiling images.
    pub const DEVICE_LOCAL: u32 = 0;
    /// Arena-backed staging and upload.
    pub const HOST_VISIBLE: u32 = 1;
    /// Arena-backed readback.
    pub const HOST_CACHED: u32 = 2;
    /// Arena-backed, and the type modern engines look for as "ReBAR".
    pub const DEVICE_LOCAL_HOST_VISIBLE: u32 = 3;
}

/// Heap 1 is the arena. Heap 0 is what is left of the GPU side's heap.
pub mod heap {
    pub const DEVICE: u32 = 0;
    pub const ARENA: u32 = 1;
}

/// Whether a memory type index is served out of the arena.
///
/// Everything but type 0. The `vkAllocateMemory` path uses this to decide
/// whether an allocation is a sub-range of a slab or a GPU-side allocation the
/// app side never maps.
pub fn is_arena_backed(memory_type_index: u32) -> bool {
    memory_type_index != memory_type::DEVICE_LOCAL
}

/// The `memoryTypeBits` a buffer gets: every arena-backed type, which is every
/// type but 0.
///
/// This substitutes for whatever the real driver reports, and it has to: the two
/// sides number memory types differently, and docs/design.md §9.1 says we advertise
/// our four rather than mirroring the driver's. An application handed the
/// driver's mask would index our table with somebody else's numbering.
pub const ARENA_MEMORY_TYPE_BITS: u32 = 0b1110;

/// The memory table of docs/design.md §9.1.
///
/// `arena_len` is the real arena size and is reported honestly: "applications
/// budget against it and use it for `VK_EXT_memory_budget`; lying produces
/// allocation failures at bad moments" (R11).
pub fn memory_properties(arena_len: u64) -> VkPhysicalDeviceMemoryProperties {
    use VkMemoryPropertyFlags as F;
    let mut p = VkPhysicalDeviceMemoryProperties::default();

    p.memory_heap_count = 2;
    // Heap 0's size is the GPU side's real heap minus the arena, "since on this
    // hardware they are physically the same 6624 MiB" (docs/design.md §9.1).
    p.memory_heaps[heap::DEVICE as usize] = VkMemoryHeap {
        size: MEASURED_DEVICE_HEAP.saturating_sub(arena_len),
        flags: VkMemoryHeapFlags::VK_MEMORY_HEAP_DEVICE_LOCAL_BIT,
    };
    p.memory_heaps[heap::ARENA as usize] = VkMemoryHeap {
        size: arena_len,
        // The arena is guest RAM the GPU imports; it is device-local in the
        // sense that matters on unified memory, which is what the measurement
        // says (`memoryTypeBits = 0x6`, both host-visible types).
        flags: VkMemoryHeapFlags::VK_MEMORY_HEAP_DEVICE_LOCAL_BIT,
    };

    p.memory_type_count = 4;
    p.memory_types[memory_type::DEVICE_LOCAL as usize] = VkMemoryType {
        property_flags: F::VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
        heap_index: heap::DEVICE,
    };
    p.memory_types[memory_type::HOST_VISIBLE as usize] = VkMemoryType {
        property_flags: F::VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT
            | F::VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
        heap_index: heap::ARENA,
    };
    p.memory_types[memory_type::HOST_CACHED as usize] = VkMemoryType {
        property_flags: F::VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT
            | F::VK_MEMORY_PROPERTY_HOST_COHERENT_BIT
            | F::VK_MEMORY_PROPERTY_HOST_CACHED_BIT,
        heap_index: heap::ARENA,
    };
    p.memory_types[memory_type::DEVICE_LOCAL_HOST_VISIBLE as usize] = VkMemoryType {
        property_flags: F::VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT
            | F::VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT
            | F::VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
        heap_index: heap::ARENA,
    };
    p
}

/// `VkPhysicalDeviceProperties`.
///
/// The limits are **conservative placeholders**, not measured, and they are the
/// largest untruth in this file. They are the specification's own minimum
/// guarantees where a minimum exists, so an application that respects them
/// cannot exceed what any conformant implementation offers — including the real
/// GPU side, once its answers are forwarded. That is the safe direction: too
/// small fails a feature check, too large fails at draw time.
pub fn device_properties(api_version: u32) -> VkPhysicalDeviceProperties {
    let mut p = VkPhysicalDeviceProperties::default();
    p.api_version = api_version;
    p.driver_version = 1;
    // 0x10005 is Khronos's "no vendor" id, which is the honest answer for a
    // software forwarder. Claiming Intel's would make applications apply Intel
    // driver workarounds to a device that is not one.
    p.vendor_id = 0x0001_0005;
    p.device_id = 0x0000_0001;
    p.device_type = VkPhysicalDeviceType::VK_PHYSICAL_DEVICE_TYPE_OTHER;
    write_c_array(&mut p.device_name, "shared_vulkan forwarded device");
    p.limits = conservative_limits();
    p.sparse_properties = VkPhysicalDeviceSparseProperties::default();
    p
}

/// The specification's minimum guarantees, used as limits until the GPU side's
/// real answers are forwarded.
fn conservative_limits() -> VkPhysicalDeviceLimits {
    let mut l = VkPhysicalDeviceLimits::default();
    l.max_image_dimension1_d = 4096;
    l.max_image_dimension2_d = 4096;
    l.max_image_dimension3_d = 256;
    l.max_image_dimension_cube = 4096;
    l.max_image_array_layers = 256;
    l.max_texel_buffer_elements = 65536;
    l.max_uniform_buffer_range = 16384;
    l.max_storage_buffer_range = 1 << 27;
    l.max_push_constants_size = 128;
    l.max_memory_allocation_count = 4096;
    l.max_sampler_allocation_count = 4000;
    l.buffer_image_granularity = 131072;
    l.max_bound_descriptor_sets = 4;
    l.max_per_stage_descriptor_samplers = 16;
    l.max_per_stage_descriptor_uniform_buffers = 12;
    l.max_per_stage_descriptor_storage_buffers = 4;
    l.max_per_stage_descriptor_sampled_images = 16;
    l.max_per_stage_descriptor_storage_images = 4;
    l.max_per_stage_descriptor_input_attachments = 4;
    l.max_per_stage_resources = 128;
    l.max_descriptor_set_samplers = 96;
    l.max_descriptor_set_uniform_buffers = 72;
    l.max_descriptor_set_uniform_buffers_dynamic = 8;
    l.max_descriptor_set_storage_buffers = 24;
    l.max_descriptor_set_storage_buffers_dynamic = 4;
    l.max_descriptor_set_sampled_images = 96;
    l.max_descriptor_set_storage_images = 24;
    l.max_descriptor_set_input_attachments = 4;
    l.max_vertex_input_attributes = 16;
    l.max_vertex_input_bindings = 16;
    l.max_vertex_input_attribute_offset = 2047;
    l.max_vertex_input_binding_stride = 2048;
    l.max_vertex_output_components = 64;
    l.max_fragment_input_components = 64;
    l.max_fragment_output_attachments = 4;
    l.max_fragment_dual_src_attachments = 1;
    l.max_fragment_combined_output_resources = 4;
    l.max_compute_shared_memory_size = 16384;
    l.max_compute_work_group_count = [65535, 65535, 65535];
    l.max_compute_work_group_invocations = 128;
    l.max_compute_work_group_size = [128, 128, 64];
    l.sub_pixel_precision_bits = 4;
    l.sub_texel_precision_bits = 4;
    l.mipmap_precision_bits = 4;
    l.max_draw_indexed_index_value = u32::MAX;
    l.max_draw_indirect_count = 1;
    l.max_sampler_lod_bias = 2.0;
    l.max_sampler_anisotropy = 1.0;
    l.max_viewports = 1;
    l.max_viewport_dimensions = [4096, 4096];
    l.viewport_bounds_range = [-8192.0, 8191.0];
    l.min_memory_map_alignment = 64;
    l.min_texel_buffer_offset_alignment = 256;
    l.min_uniform_buffer_offset_alignment = 256;
    l.min_storage_buffer_offset_alignment = 256;
    l.min_texel_offset = -8;
    l.max_texel_offset = 7;
    l.sub_pixel_interpolation_offset_bits = 4;
    l.max_framebuffer_width = 4096;
    l.max_framebuffer_height = 4096;
    l.max_framebuffer_layers = 256;
    l.max_color_attachments = 4;
    l.max_sample_mask_words = 1;
    l.max_clip_distances = 8;
    l.max_cull_distances = 8;
    l.max_combined_clip_and_cull_distances = 8;
    l.discrete_queue_priorities = 2;
    l.point_size_range = [1.0, 64.0];
    l.line_width_range = [1.0, 1.0];
    l.point_size_granularity = 0.0;
    l.line_width_granularity = 0.0;
    l.non_coherent_atom_size = 256;
    // Every `VkSampleCountFlags` limit must include at least 1 sample, and the
    // spec requires 4 for colour and depth.
    let s1_4 = VkSampleCountFlags::VK_SAMPLE_COUNT_1_BIT
        | VkSampleCountFlags::VK_SAMPLE_COUNT_4_BIT;
    l.framebuffer_color_sample_counts = s1_4;
    l.framebuffer_depth_sample_counts = s1_4;
    l.framebuffer_stencil_sample_counts = s1_4;
    l.framebuffer_no_attachments_sample_counts = s1_4;
    l.sampled_image_color_sample_counts = s1_4;
    l.sampled_image_integer_sample_counts = VkSampleCountFlags::VK_SAMPLE_COUNT_1_BIT;
    l.sampled_image_depth_sample_counts = s1_4;
    l.sampled_image_stencil_sample_counts = s1_4;
    l.storage_image_sample_counts = VkSampleCountFlags::VK_SAMPLE_COUNT_1_BIT;
    l.optimal_buffer_copy_offset_alignment = 4;
    l.optimal_buffer_copy_row_pitch_alignment = 4;
    l.timestamp_compute_and_graphics = 0;
    l.timestamp_period = 1.0;
    l.strict_lines = 0;
    l.standard_sample_locations = 1;
    l
}

/// The one queue family, for the **standalone** configuration only.
///
/// With a GPU side attached the family table is the GPU side's answer, forwarded
/// (`sv_gpu_server::driver::ServedFamily`), and since docs/video-decode.md §7 it
/// may hold a decode family beside this one. Without a GPU side there is nothing
/// to decode on, so this stays the single universal family it always was. All
/// families share the one queue *ring* (`RingClass::Queue`) — D8's ring per queue
/// was never built, and serving a second family did not need it.
pub fn queue_families() -> Vec<VkQueueFamilyProperties> {
    let mut q = VkQueueFamilyProperties::default();
    q.queue_flags = VkQueueFlags::VK_QUEUE_GRAPHICS_BIT
        | VkQueueFlags::VK_QUEUE_COMPUTE_BIT
        | VkQueueFlags::VK_QUEUE_TRANSFER_BIT;
    q.queue_count = 1;
    q.timestamp_valid_bits = 0;
    q.min_image_transfer_granularity = VkExtent3D { width: 1, height: 1, depth: 1 };
    vec![q]
}

/// The queue capabilities of the standalone family table, as one mask. The
/// connected ICD uses the GPU side's answer instead (`SharedVulkan::queue_flags`).
///
/// Derived from [`queue_families`] rather than written down beside it, so that
/// opening a second family is the only edit needed to make the extensions that
/// need it appear. The whole point of the check below is that it stops being
/// true the moment the families change, and a constant would not.
pub fn advertised_queue_flags() -> u32 {
    queue_families().iter().fold(0u32, |acc, f| acc | f.queue_flags.0)
}

/// Why this extension cannot be served, if it cannot: the first command
/// requirement no advertised queue family satisfies.
///
/// # The defect, and why `INCOMPLETE_EXTENSIONS` could not see it
///
/// That table asks whether the *generator* produced a working stub. For
/// `VK_KHR_video_queue` and `VK_KHR_video_decode_queue` it did — every entry
/// point marshals — so when `full` became the default profile a forwarded device
/// began advertising sixteen video extensions. It also advertises **one queue
/// family**, `GRAPHICS | COMPUTE | TRANSFER` (§0.8 item 7's synthesised family),
/// and `vkCmdDecodeVideoKHR` is `queues="VK_QUEUE_VIDEO_DECODE_BIT_KHR"`. There
/// was no family an application could record it on.
///
/// Firefox 153 selects a device on exactly those two extensions, and its
/// first-frame failure comes back as `NS_ERROR_DOM_MEDIA_FATAL_ERR` — which its
/// hardware-to-software recovery path does not admit. One pref away from the
/// default that was broken playback, not slow playback.
///
/// # Why it is derived and not a list
///
/// The underlying fact is "we advertise a family we did not open", and both
/// halves are already data: the registry states `queues=` per command
/// (`sv_icd_entry::EXTENSION_QUEUE_REQUIREMENTS`), and [`queue_families`] states
/// what we offer. Adding `VK_KHR_video_queue` to an exclusion list would have
/// fixed today and said nothing about `VK_KHR_video_encode_queue`,
/// `VK_NV_optical_flow` or `VK_ARM_data_graph`, which fail for the identical
/// reason and are caught here without being named.
///
/// **An honest refusal, not a placeholder.** A gate with nothing behind it is
/// worse than a documented gap: if we cannot serve a video queue we must not
/// claim one. `have` is the OR of the families actually served — the GPU side's
/// answer when connected — so when the GPU side serves a decode family
/// (docs/video-decode.md §7) this returns `None` for the decode extensions on its
/// own, and keeps refusing `VK_KHR_video_encode_queue`, whose family is not served.
pub fn unservable_queue_requirement(extension: &str, have: u32) -> Option<u32> {
    let reqs = sv_icd_entry::EXTENSION_QUEUE_REQUIREMENTS
        .binary_search_by_key(&extension, |(name, _)| name)
        .ok()
        .map(|i| sv_icd_entry::EXTENSION_QUEUE_REQUIREMENTS[i].1)?;
    // Every command's mask must intersect what we advertise; the first that does
    // not is the reason, and is worth returning rather than a bool so the
    // diagnostic can say which capability was missing.
    reqs.iter().copied().find(|mask| mask & have == 0)
}

/// Features, curated rather than mirrored (docs/design.md §9.2).
///
/// **`bufferDeviceAddress` is deliberately absent** and is not an oversight: an
/// application that writes a raw 64-bit address into arena memory for a shader
/// to dereference has done something the `transform` pass cannot see. A device
/// that does not advertise the feature makes DXVK ≥ 2.7 and vkd3d-proton fail
/// device creation cleanly and loudly, which is a much better outcome than one
/// that advertises it and renders garbage (§9.2, R12).
pub fn features() -> VkPhysicalDeviceFeatures {
    VkPhysicalDeviceFeatures::default()
}

/// The instance extensions we advertise.
pub const INSTANCE_EXTENSIONS: &[(&str, u32)] = &[
    // docs/design.md §10. Headless is stage 1 and is what Wine maps
    // `VK_KHR_win32_surface` onto when there is no display (§0.5); the two X11
    // extensions are stage 2, and are what Mesa's `zink_kopper` asks for by
    // name when a GL application draws to a window.
    //
    // **Advertising an X surface extension obliges us to export its
    // `vkGetPhysicalDevice…PresentationSupportKHR`**, because the loader's
    // terminator for that family calls `abort()` rather than answering
    // `VK_FALSE` when the selected ICD does not have it. It does *not* oblige us
    // to export `vkCreateXcbSurfaceKHR`: not exporting that is how the loader
    // knows to hand us its own `VkIcdSurfaceXcb` instead, which is design.md
    // §8's decision and is what keeps `libxcb` out of this crate's link line.
    ("VK_KHR_surface", 25),
    ("VK_EXT_headless_surface", 1),
    ("VK_KHR_xlib_surface", 6),
    ("VK_KHR_xcb_surface", 6),
    // **The one that decides whether Firefox renders in hardware.** Without it
    // `zink_kopper` cannot create a `VkSurfaceKHR` on the Wayland platform, so
    // Mesa loads the *swrast* DRI driver with Zink behind it and Firefox's
    // hardware WebRender rejects the EGL configs that arrangement produces
    // (`docs/firefox-accel.md`). With it, Mesa takes the same kopper path it
    // takes on a real RADV against a `wl_shm`-only compositor, which is
    // measured to satisfy Firefox.
    //
    // Advertising it obliges the same two things the X11 pair oblige — a
    // `vkGetPhysicalDeviceWaylandPresentationSupportKHR`, or the loader
    // `abort()`s, and a `vkCreateWaylandSurfaceKHR`, or every surface query
    // silently answers "not supported" — and one thing they do not: the ICD
    // must actually speak `wl_shm` on the application's connection, because a
    // `wl_surface` has no id a second connection could name. See
    // `crate::present::wayland`.
    ("VK_KHR_wayland_surface", 6),
    // **Wine asks for these three on every instance it creates**, whatever the
    // application asked for, and `vkCreateInstance` fails with
    // `VK_ERROR_EXTENSION_NOT_PRESENT` if the host ICD does not have them. That
    // is the entire reason DXVK could not start against this driver — not a
    // feature, not `bufferDeviceAddress`, an instance extension list.
    //
    // They cost nothing to advertise and they do not weaken §9.2. All three are
    // **query** extensions, promoted to core 1.1, whose commands this profile
    // already forwards; what §9.2 declines to advertise is external *handle
    // types*, and these extensions are the mechanism by which a device says
    // which handle types it supports. Saying "none" through them is more honest
    // than not being askable. See `external_semaphore_properties` and its two
    // siblings in `icd.rs`, which answer exactly that rather than forwarding
    // the GPU-side driver's much longer list.
    ("VK_KHR_external_memory_capabilities", 1),
    ("VK_KHR_external_semaphore_capabilities", 1),
    ("VK_KHR_external_fence_capabilities", 1),
    // Same story, and the same zero cost: core 1.1, every command already
    // forwarded, and Wine puts it on the host instance whenever the client
    // instance is 1.0 — which is what its own helper processes create.
    ("VK_KHR_get_physical_device_properties2", 2),
];

/// The device extensions we advertise **with no GPU side**.
///
/// Empty, and that is the honest state rather than a placeholder: an extension
/// in this list is a promise that its entry points work, and a standalone driver
/// forwards nothing.
///
/// With a GPU side the list is **not a constant** — it is asked of the peer at
/// connect and cached in [`crate::state::SharedVulkan::device_extensions`],
/// because the only side that can keep §9.2's promise is the one holding the
/// driver. That is a correction to docs/design.md §9.2, which describes the list as
/// something the app side curates: it cannot, because the intersection with the
/// driver's own extensions is not knowable here.
pub const DEVICE_EXTENSIONS: &[(&str, u32)] = &[];

/// Features the driver over there has and this side will not promise.
///
/// # Why this exists at all, given `INCOMPLETE_EXTENSIONS`
///
/// Extensions and features are not the same lever, and core 1.4 is where the
/// difference stops being academic. `sv_icd_entry::INCOMPLETE_EXTENSIONS` can
/// withdraw `VK_EXT_host_image_copy` — but the same capability is *also* core
/// 1.4's `hostImageCopy`, reachable through `VkPhysicalDeviceVulkan14Features`
/// with no extension string to decline. Withdrawing the extension and leaving
/// the feature bit set advertises the same thing twice and denies it once.
///
/// So this walks the reply's `pNext` chain and clears the bit wherever it
/// appears: in `VkPhysicalDeviceVulkan14Features`, in the promoted
/// `VkPhysicalDeviceHostImageCopyFeatures`, and in the `…EXT` form. All three
/// are the same bit and an application may ask through any of them.
///
/// # Why `hostImageCopy` in particular
///
/// The wire is right: `sv_codegen::exceptions` gives `pHostPointer` the
/// `ArenaOffset` policy, which is what docs/design.md §5.3 says a host pointer *is*
/// here — `arena_base + offset`. What is not written is the case where the
/// application's pointer is **not** arena memory. `vkCopyMemoryToImage` accepts
/// a plain `malloc` buffer, and honouring that needs a bounce through an arena
/// block whose extent must be computed from `imageExtent`, `imageSubresource`,
/// `memoryRowLength` and the image's format. That is real work and it is not
/// done, so the feature is withdrawn rather than half-kept.
///
/// This is the seam docs/design.md §0.19 item 3 asks for: carry everything, advertise
/// what works. The three commands stay on the wire and in the proc tables — they
/// are core 1.4 and cannot be made to disappear — and a conforming application
/// that respects the feature bit never calls them.
/// # Why the capture-replay bits, and what was asserting itself before
///
/// `sv_codegen::exceptions::VoidPolicy::CaptureReplayUnsupported` encodes
/// `pShaderGroupCaptureReplayHandle` and `opaqueCaptureDescriptorData` as null
/// on the wire and refuses a non-null one by name. That is honest **only
/// because the specification requires those members to be null unless the
/// device advertises the matching feature** — and the policy's own comment said
/// so: "This driver advertises neither, and `sv_icd::advertise` is where that is
/// kept."
///
/// It was not kept anywhere. These features arrive in
/// `VkPhysicalDeviceRayTracingPipelineFeaturesKHR` and
/// `VkPhysicalDeviceDescriptorBufferFeaturesEXT`, which are forwarded whole from
/// the physical device, so on a driver that supports capture-replay — RADV and
/// NVIDIA both do — we advertised it, an application could legally enable it,
/// and the first replay token it passed would meet a refusal. Exactly the shape
/// of docs/design.md §0.16 item 3, where §9.2's claim about `bufferDeviceAddress`
/// rested on a test about a different structure and `vulkaninfo` printed the
/// opposite all along.
///
/// So the claim is made here, where it is true by construction, and
/// `sv_codegen`'s tests enumerate the class the claim is about
/// (`the_capture_replay_class_is_what_the_naming_rule_matches`). docs/design.md
/// §0.25.
///
/// **`bufferDeviceAddressCaptureReplay` is deliberately not withdrawn.** Its
/// token is a `uint64_t` — `VkBufferOpaqueCaptureAddressCreateInfo::opaqueCaptureAddress`
/// — not a `void*`, `vkGetBufferOpaqueCaptureAddress` is forwarded and returns
/// the GPU side's real value (§0.20), and a `u64` crosses. The line between the
/// two is the encoding, not the words "capture replay".
pub fn withdraw_gated_features(chain: &mut [sv_proto::PNextLink]) {
    use sv_proto::PNextLink as L;
    for link in chain.iter_mut() {
        match link {
            L::VkPhysicalDeviceVulkan14Features(f) => f.host_image_copy = false,
            L::VkPhysicalDeviceHostImageCopyFeatures(f) => f.host_image_copy = false,
            L::VkPhysicalDeviceRayTracingPipelineFeaturesKHR(f) => {
                f.ray_tracing_pipeline_shader_group_handle_capture_replay = false;
                f.ray_tracing_pipeline_shader_group_handle_capture_replay_mixed = false;
            }
            L::VkPhysicalDeviceDescriptorBufferFeaturesEXT(f) => {
                f.descriptor_buffer_capture_replay = false;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The subset-ordering rule of docs/design.md §9.1, checked rather than asserted
    /// in prose.
    ///
    /// The specification requires that if one memory type's property flags are a
    /// strict subset of another's, the subset precedes it. An application that
    /// takes the first match therefore gets the least-capable type that works,
    /// which is what it wants.
    #[test]
    fn the_memory_table_satisfies_the_subset_ordering_rule() {
        let p = memory_properties(2 << 30);
        let n = p.memory_type_count as usize;
        assert_eq!(n, 4);
        for i in 0..n {
            for j in 0..n {
                let a = p.memory_types[i].property_flags.0;
                let b = p.memory_types[j].property_flags.0;
                if a != b && a & b == a {
                    assert!(
                        i < j,
                        "type {i} ({a:#x}) is a strict subset of type {j} ({b:#x}) and must \
                         come first"
                    );
                }
            }
        }
    }

    /// Every arena-backed type must be host-visible, or an application will not
    /// take the fast path — which is the entire point of the project.
    #[test]
    fn arena_types_are_host_visible_and_device_types_are_not() {
        let p = memory_properties(2 << 30);
        for i in 0..p.memory_type_count as usize {
            let hv = p.memory_types[i]
                .property_flags
                .contains(VkMemoryPropertyFlags::VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT);
            assert_eq!(
                hv,
                is_arena_backed(i as u32),
                "type {i}: host-visible and arena-backed must agree"
            );
            assert_eq!(
                p.memory_types[i].heap_index,
                if is_arena_backed(i as u32) { heap::ARENA } else { heap::DEVICE }
            );
        }
    }

    /// Heap 1's size is the real arena size (R11), and heap 0 does not double
    /// count it.
    #[test]
    fn the_heaps_do_not_lie_about_size() {
        let arena = 2u64 << 30;
        let p = memory_properties(arena);
        assert_eq!(p.memory_heap_count, 2);
        assert_eq!(p.memory_heaps[heap::ARENA as usize].size, arena);
        assert_eq!(
            p.memory_heaps[heap::DEVICE as usize].size,
            MEASURED_DEVICE_HEAP - arena,
            "the two heaps are physically the same memory on this hardware"
        );
    }

    /// The 1.0 feature set is the fallback with no GPU side, and it stays empty.
    ///
    /// **This test used to be called `we_do_not_advertise_buffer_device_address`
    /// and it never checked that.** `bufferDeviceAddress` is not in
    /// `VkPhysicalDeviceFeatures` at all — it arrives through
    /// `VkPhysicalDeviceVulkan12Features`, which is forwarded whole from the
    /// physical device — so the assertion below was true, the name was false,
    /// and docs/design.md §9.2's claim that we do not advertise the feature rested on
    /// a test about a different structure. `vulkaninfo` printed
    /// `bufferDeviceAddress = true` throughout (docs/design.md §0.16 item 3).
    ///
    /// The name is corrected rather than the assertion, because the feature is
    /// now **implemented** — all three of `vkGetBufferDeviceAddress`,
    /// `vkGetBufferOpaqueCaptureAddress` and
    /// `vkGetDeviceMemoryOpaqueCaptureAddress` forward and return the GPU side's
    /// real address (§0.20) — so advertising it is honest and the check that
    /// belongs here is the one the body actually makes.
    #[test]
    fn the_ten_zero_feature_set_is_empty() {
        let f = features();
        assert_eq!(f.robust_buffer_access, 0);
        assert_eq!(f.shader_int64, 0);
    }

    /// We do not claim an extension whose commands need a queue family we do
    /// not open — stated as the invariant, not as a list of names.
    ///
    /// The property is checkable in both directions from data that already
    /// exists, which is the point of deriving it: `EXTENSION_QUEUE_REQUIREMENTS`
    /// is generated from the registry's `queues=`, and [`queue_families`] is
    /// what we advertise. A name-based test would have to be edited when the
    /// ICD grows a video family; this one starts passing for a different reason
    /// and keeps asserting the same thing.
    #[test]
    fn no_extension_is_advertised_that_needs_a_queue_family_we_do_not_open() {
        let have = advertised_queue_flags();
        assert_ne!(have, 0, "we advertise at least one family");

        let mut refused = 0usize;
        for (ext, masks) in sv_icd_entry::EXTENSION_QUEUE_REQUIREMENTS {
            let unservable = masks.iter().any(|m| m & have == 0);
            assert_eq!(
                unservable,
                unservable_queue_requirement(ext, have).is_some(),
                "{ext}: the gate disagrees with the table it reads"
            );
            if unservable {
                refused += 1;
            }
        }
        assert!(
            refused > 0,
            "with one GRAPHICS|COMPUTE|TRANSFER family, something must be \
             refused — if nothing is, the table is empty and the gate is a \
             no-op dressed as a check"
        );

        // The case that motivated it, named once. Not as the assertion — that is
        // the loop above — but because a regression here is broken video
        // playback in Firefox and the next reader should meet the example.
        // `VK_KHR_video_queue` introduces `vkCmdBeginVideoCodingKHR`, which is
        // `queues="VK_QUEUE_VIDEO_DECODE_BIT_KHR,VK_QUEUE_VIDEO_ENCODE_BIT_KHR"`
        // and cannot be recorded on any family this driver offers.
        assert!(
            unservable_queue_requirement("VK_KHR_video_queue", have).is_some(),
            "VK_KHR_video_queue needs a video family and we open none"
        );
        assert!(
            unservable_queue_requirement("VK_KHR_video_decode_queue", have).is_some(),
            "VK_KHR_video_decode_queue needs a video family and we open none"
        );
        // And the control: an extension whose commands run on graphics must not
        // be caught by the same rule, or the gate is refusing everything.
        assert!(
            unservable_queue_requirement("VK_EXT_transform_feedback", have).is_none(),
            "VK_EXT_transform_feedback records on graphics and must be servable"
        );
    }

    /// With a decode family served the gate opens for decode, and only for
    /// decode. `VK_QUEUE_VIDEO_DECODE_BIT_KHR` is `0x20`; the encode bit `0x40`
    /// is masked on the GPU side (`sv_gpu_server::driver::SERVED_QUEUE_FLAGS`),
    /// so `VK_KHR_video_encode_queue` must stay refused — and
    /// `VK_KHR_video_queue`, whose begin/end/control commands are
    /// `queues="…DECODE…,…ENCODE…"`, a disjunction, must become servable.
    #[test]
    fn a_served_decode_family_opens_the_decode_extensions_and_not_encode() {
        let have = advertised_queue_flags() | 0x20;
        for ext in ["VK_KHR_video_queue", "VK_KHR_video_decode_queue"] {
            assert_eq!(unservable_queue_requirement(ext, have), None, "{ext}");
        }
        assert_eq!(
            unservable_queue_requirement("VK_KHR_video_encode_queue", have),
            Some(0x40),
            "encode is not served and must not be advertised"
        );
    }

    /// A feature the driver has and we do not keep is withdrawn on *every* path
    /// an application can ask through.
    ///
    /// The bug this guards is not "we forgot to clear it" — it is "we cleared it
    /// in one of the three places". `hostImageCopy` is core 1.4's feature, the
    /// promoted extension's feature and the `…EXT` alias's feature, and an
    /// application that chains `VkPhysicalDeviceVulkan14Features` sees a
    /// different answer from one that chains
    /// `VkPhysicalDeviceHostImageCopyFeatures` unless both are handled.
    ///
    /// (This comment was two tests above its function, behind a stray `#[test]`
    /// that made `rustc` warn about a duplicated attribute; moved rather than
    /// left, because a doc comment on the wrong test is a claim about the wrong
    /// assertion.)
    #[test]
    fn a_gated_feature_is_withdrawn_wherever_it_is_asked_for() {
        let mut chain = vec![
            sv_proto::PNextLink::VkPhysicalDeviceVulkan14Features(Box::new(
                sv_proto::VkPhysicalDeviceVulkan14Features {
                    host_image_copy: true,
                    // A neighbour that must survive: the walk clears one bit,
                    // not a struct.
                    push_descriptor: true,
                    ..Default::default()
                },
            )),
            sv_proto::PNextLink::VkPhysicalDeviceHostImageCopyFeatures(Box::new(
                sv_proto::VkPhysicalDeviceHostImageCopyFeatures {
                    host_image_copy: true,
                    ..Default::default()
                },
            )),
        ];
        withdraw_gated_features(&mut chain);
        for link in &chain {
            match link {
                sv_proto::PNextLink::VkPhysicalDeviceVulkan14Features(f) => {
                    assert!(!f.host_image_copy, "core 1.4's bit is still set");
                    assert!(f.push_descriptor, "the walk cleared a bit it was not asked to");
                }
                sv_proto::PNextLink::VkPhysicalDeviceHostImageCopyFeatures(f) => {
                    assert!(!f.host_image_copy, "the promoted extension's bit is still set");
                }
                _ => unreachable!(),
            }
        }
    }

    /// The feature that makes a `CaptureReplayUnsupported` `void*` legal to be
    /// non-null is never advertised, on any of the paths it arrives by.
    ///
    /// This is the half of docs/design.md §0.24's argument that was written down
    /// and not built. The policy is honest exactly while these bits are false,
    /// so the bits are what the test is about — an application that respects
    /// them never reaches the refusal, and one that does not gets a named error
    /// instead of a wrong address (§0.25).
    #[test]
    fn no_capture_replay_feature_whose_token_is_a_void_pointer_is_advertised() {
        let mut chain = vec![
            sv_proto::PNextLink::VkPhysicalDeviceRayTracingPipelineFeaturesKHR(Box::new(
                sv_proto::VkPhysicalDeviceRayTracingPipelineFeaturesKHR {
                    ray_tracing_pipeline: true,
                    ray_tracing_pipeline_shader_group_handle_capture_replay: true,
                    ray_tracing_pipeline_shader_group_handle_capture_replay_mixed: true,
                    ..Default::default()
                },
            )),
            sv_proto::PNextLink::VkPhysicalDeviceDescriptorBufferFeaturesEXT(Box::new(
                sv_proto::VkPhysicalDeviceDescriptorBufferFeaturesEXT {
                    descriptor_buffer: true,
                    descriptor_buffer_capture_replay: true,
                    ..Default::default()
                },
            )),
            // The one that stays: its token is a `uint64_t` and it works.
            sv_proto::PNextLink::VkPhysicalDeviceBufferDeviceAddressFeatures(Box::new(
                sv_proto::VkPhysicalDeviceBufferDeviceAddressFeatures {
                    buffer_device_address: true,
                    buffer_device_address_capture_replay: true,
                    ..Default::default()
                },
            )),
        ];
        withdraw_gated_features(&mut chain);
        for link in &chain {
            match link {
                sv_proto::PNextLink::VkPhysicalDeviceRayTracingPipelineFeaturesKHR(f) => {
                    assert!(!f.ray_tracing_pipeline_shader_group_handle_capture_replay);
                    assert!(!f.ray_tracing_pipeline_shader_group_handle_capture_replay_mixed);
                    assert!(
                        f.ray_tracing_pipeline,
                        "ray tracing itself is carried; only replay is withdrawn"
                    );
                }
                sv_proto::PNextLink::VkPhysicalDeviceDescriptorBufferFeaturesEXT(f) => {
                    assert!(!f.descriptor_buffer_capture_replay);
                    assert!(f.descriptor_buffer, "the extension's own feature survives");
                }
                sv_proto::PNextLink::VkPhysicalDeviceBufferDeviceAddressFeatures(f) => {
                    assert!(
                        f.buffer_device_address_capture_replay,
                        "a u64 capture-replay token crosses and must not be withdrawn"
                    );
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn the_device_name_is_nul_terminated() {
        let p = device_properties(1 << 22);
        assert!(p.device_name.contains(&0), "deviceName must be NUL-terminated");
    }
}
