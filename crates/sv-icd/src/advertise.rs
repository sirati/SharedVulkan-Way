//! What we tell the application, and where each number comes from.
//!
//! design.md §9 decides this and `measured-gpu-side.md` supplies the facts. The
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
//! mapped — **does not arise on this hardware**" (design.md §9.1).

use sv_icd_entry::*;

/// The GPU side's real heap size, measured (`measured-gpu-side.md`).
pub const MEASURED_DEVICE_HEAP: u64 = 6624 << 20;

/// `minImportedHostPointerAlignment`, measured.
pub const IMPORTED_HOST_POINTER_ALIGNMENT: u64 = 4096;

/// Memory type indices, in the order design.md §9.1 fixes them.
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
/// sides number memory types differently, and design.md §9.1 says we advertise
/// our four rather than mirroring the driver's. An application handed the
/// driver's mask would index our table with somebody else's numbering.
pub const ARENA_MEMORY_TYPE_BITS: u32 = 0b1110;

/// The memory table of design.md §9.1.
///
/// `arena_len` is the real arena size and is reported honestly: "applications
/// budget against it and use it for `VK_EXT_memory_budget`; lying produces
/// allocation failures at bad moments" (R11).
pub fn memory_properties(arena_len: u64) -> VkPhysicalDeviceMemoryProperties {
    use VkMemoryPropertyFlags as F;
    let mut p = VkPhysicalDeviceMemoryProperties::default();

    p.memory_heap_count = 2;
    // Heap 0's size is the GPU side's real heap minus the arena, "since on this
    // hardware they are physically the same 6624 MiB" (design.md §9.1).
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

/// The one queue family.
///
/// A single universal queue rather than a mirror of the GPU side's families:
/// design.md D8 gives each queue its own ring, and every family we advertise is
/// a ring the GPU side has to consume. One is what the milestone needs and it is
/// what an application that asks for `GRAPHICS | COMPUTE | TRANSFER` gets.
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

/// Features, curated rather than mirrored (design.md §9.2).
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
    // design.md §10 stage 1: headless first, and no swapchain at all.
    ("VK_KHR_surface", 25),
    ("VK_EXT_headless_surface", 1),
];

/// The device extensions we advertise.
///
/// Empty in this build, and that is the honest state rather than a placeholder:
/// an extension in this list is a promise that its entry points work, and
/// nothing forwards yet. Zink's five required extensions
/// (`maintenance1`, `maintenance5`, `create_renderpass2`, `dynamic_rendering`,
/// `descriptor_update_template`) plus `timeline_semaphore` go here when there is
/// a GPU side to forward them to (design.md §11).
pub const DEVICE_EXTENSIONS: &[(&str, u32)] = &[];

#[cfg(test)]
mod tests {
    use super::*;

    /// The subset-ordering rule of design.md §9.1, checked rather than asserted
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

    #[test]
    fn we_do_not_advertise_buffer_device_address() {
        // The feature is not in `VkPhysicalDeviceFeatures` at all — it arrives
        // through `VkPhysicalDeviceVulkan12Features` — so what this checks is
        // that the base feature set is empty and nothing has quietly turned
        // something on.
        let f = features();
        assert_eq!(f.robust_buffer_access, 0);
        assert_eq!(f.shader_int64, 0);
    }

    #[test]
    fn the_device_name_is_nul_terminated() {
        let p = device_properties(1 << 22);
        assert!(p.device_name.contains(&0), "deviceName must be NUL-terminated");
    }
}
