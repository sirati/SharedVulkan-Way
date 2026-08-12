//! `ash` → wire, for the values the GPU side reports back.
//!
//! # Why this is written out and not transmuted
//!
//! `ash::vk::PhysicalDeviceLimits` and `sv_proto::VkPhysicalDeviceLimits`
//! describe the same 106 numbers and are both `#[repr(C)]`-shaped, so a
//! reinterpretation would work today. design.md §7.3 forbids exactly that —
//! "nothing is transmuted out of shared memory … every field is assembled from a
//! cursor" — and the argument generalises: two independently generated views of
//! one C struct agreeing about layout is a coincidence maintained by two
//! generators, and a field appended to one of them turns a passing build into
//! silent garbage. Naming every field means a mismatch is a compile error.
//!
//! The macros below are the concession to the 106: they make each line one
//! field name instead of two, so the list stays reviewable against
//! `vulkan_core.h` and still names everything.

// Clippy wants a struct literal instead of `Default` plus assignment. For a
// struct with 106 members that means naming every one of them, including the
// ones whose value is the default, which turns a reviewable list of what we
// copy into a wall nobody reads. `sv-icd` makes the same trade for the same
// structs and gives the same reason.
#![allow(clippy::field_reassign_with_default)]

use ash::vk;
use sv_proto as p;

/// Fields whose wire and `ash` types are the same.
macro_rules! same {
    ($w:ident, $c:ident, $($f:ident),* $(,)?) => { $( $w.$f = $c.$f; )* };
}
/// `VkBool32` on the way in, `bool` on the wire.
macro_rules! flag {
    ($w:ident, $c:ident, $($f:ident),* $(,)?) => { $( $w.$f = $c.$f != 0; )* };
}
/// `VkSampleCountFlags`, which is a newtype on both sides.
macro_rules! samples {
    ($w:ident, $c:ident, $($f:ident),* $(,)?) => {
        $( $w.$f = p::VkSampleCountFlags($c.$f.as_raw()); )*
    };
}

pub fn properties(c: &vk::PhysicalDeviceProperties) -> p::VkPhysicalDeviceProperties {
    let mut w = p::VkPhysicalDeviceProperties::default();
    w.api_version = c.api_version;
    w.driver_version = c.driver_version;
    w.vendor_id = c.vendor_id;
    w.device_id = c.device_id;
    w.device_type = p::VkPhysicalDeviceType(c.device_type.as_raw());
    // `device_name` is `[c_char; 256]` in `ash` and `[u8; 256]` on the wire.
    // This is the one place `vulkaninfo` looks to tell whose driver it is
    // talking to, so it is the field the whole acceptance test turns on.
    for (i, b) in c.device_name.iter().enumerate() {
        w.device_name[i] = *b as u8;
    }
    w.pipeline_cache_uuid = c.pipeline_cache_uuid;
    w.limits = limits(&c.limits);
    w.sparse_properties = sparse_properties(&c.sparse_properties);
    w
}

pub fn sparse_properties(
    c: &vk::PhysicalDeviceSparseProperties,
) -> p::VkPhysicalDeviceSparseProperties {
    let mut w = p::VkPhysicalDeviceSparseProperties::default();
    flag!(
        w,
        c,
        residency_standard2_d_block_shape,
        residency_standard2_d_multisample_block_shape,
        residency_standard3_d_block_shape,
        residency_aligned_mip_size,
        residency_non_resident_strict,
    );
    w
}

pub fn limits(c: &vk::PhysicalDeviceLimits) -> p::VkPhysicalDeviceLimits {
    let mut w = p::VkPhysicalDeviceLimits::default();
    same!(
        w,
        c,
        max_image_dimension1_d,
        max_image_dimension2_d,
        max_image_dimension3_d,
        max_image_dimension_cube,
        max_image_array_layers,
        max_texel_buffer_elements,
        max_uniform_buffer_range,
        max_storage_buffer_range,
        max_push_constants_size,
        max_memory_allocation_count,
        max_sampler_allocation_count,
        buffer_image_granularity,
        sparse_address_space_size,
        max_bound_descriptor_sets,
        max_per_stage_descriptor_samplers,
        max_per_stage_descriptor_uniform_buffers,
        max_per_stage_descriptor_storage_buffers,
        max_per_stage_descriptor_sampled_images,
        max_per_stage_descriptor_storage_images,
        max_per_stage_descriptor_input_attachments,
        max_per_stage_resources,
        max_descriptor_set_samplers,
        max_descriptor_set_uniform_buffers,
        max_descriptor_set_uniform_buffers_dynamic,
        max_descriptor_set_storage_buffers,
        max_descriptor_set_storage_buffers_dynamic,
        max_descriptor_set_sampled_images,
        max_descriptor_set_storage_images,
        max_descriptor_set_input_attachments,
        max_vertex_input_attributes,
        max_vertex_input_bindings,
        max_vertex_input_attribute_offset,
        max_vertex_input_binding_stride,
        max_vertex_output_components,
        max_tessellation_generation_level,
        max_tessellation_patch_size,
        max_tessellation_control_per_vertex_input_components,
        max_tessellation_control_per_vertex_output_components,
        max_tessellation_control_per_patch_output_components,
        max_tessellation_control_total_output_components,
        max_tessellation_evaluation_input_components,
        max_tessellation_evaluation_output_components,
        max_geometry_shader_invocations,
        max_geometry_input_components,
        max_geometry_output_components,
        max_geometry_output_vertices,
        max_geometry_total_output_components,
        max_fragment_input_components,
        max_fragment_output_attachments,
        max_fragment_dual_src_attachments,
        max_fragment_combined_output_resources,
        max_compute_shared_memory_size,
        max_compute_work_group_count,
        max_compute_work_group_invocations,
        max_compute_work_group_size,
        sub_pixel_precision_bits,
        sub_texel_precision_bits,
        mipmap_precision_bits,
        max_draw_indexed_index_value,
        max_draw_indirect_count,
        max_sampler_lod_bias,
        max_sampler_anisotropy,
        max_viewports,
        max_viewport_dimensions,
        viewport_bounds_range,
        viewport_sub_pixel_bits,
        min_texel_buffer_offset_alignment,
        min_uniform_buffer_offset_alignment,
        min_storage_buffer_offset_alignment,
        min_texel_offset,
        max_texel_offset,
        min_texel_gather_offset,
        max_texel_gather_offset,
        min_interpolation_offset,
        max_interpolation_offset,
        sub_pixel_interpolation_offset_bits,
        max_framebuffer_width,
        max_framebuffer_height,
        max_framebuffer_layers,
        max_color_attachments,
        max_sample_mask_words,
        timestamp_period,
        max_clip_distances,
        max_cull_distances,
        max_combined_clip_and_cull_distances,
        discrete_queue_priorities,
        point_size_range,
        line_width_range,
        point_size_granularity,
        line_width_granularity,
        optimal_buffer_copy_offset_alignment,
        optimal_buffer_copy_row_pitch_alignment,
        non_coherent_atom_size,
    );
    // `size_t` in C, so `usize` in `ash` and `u64` on the wire — §6.1's
    // "`size_t` as `u64`". The one field in the struct that needs a cast, and it
    // needs one precisely because the wire refuses to have a pointer width.
    w.min_memory_map_alignment = c.min_memory_map_alignment as u64;
    samples!(
        w,
        c,
        framebuffer_color_sample_counts,
        framebuffer_depth_sample_counts,
        framebuffer_stencil_sample_counts,
        framebuffer_no_attachments_sample_counts,
        sampled_image_color_sample_counts,
        sampled_image_integer_sample_counts,
        sampled_image_depth_sample_counts,
        sampled_image_stencil_sample_counts,
        storage_image_sample_counts,
    );
    flag!(w, c, timestamp_compute_and_graphics, strict_lines, standard_sample_locations);
    w
}

pub fn features(c: &vk::PhysicalDeviceFeatures) -> p::VkPhysicalDeviceFeatures {
    let mut w = p::VkPhysicalDeviceFeatures::default();
    flag!(
        w,
        c,
        robust_buffer_access,
        full_draw_index_uint32,
        image_cube_array,
        independent_blend,
        geometry_shader,
        tessellation_shader,
        sample_rate_shading,
        dual_src_blend,
        logic_op,
        multi_draw_indirect,
        draw_indirect_first_instance,
        depth_clamp,
        depth_bias_clamp,
        fill_mode_non_solid,
        depth_bounds,
        wide_lines,
        large_points,
        alpha_to_one,
        multi_viewport,
        sampler_anisotropy,
        texture_compression_etc2,
        texture_compression_astc_ldr,
        texture_compression_bc,
        occlusion_query_precise,
        pipeline_statistics_query,
        vertex_pipeline_stores_and_atomics,
        fragment_stores_and_atomics,
        shader_tessellation_and_geometry_point_size,
        shader_image_gather_extended,
        shader_storage_image_extended_formats,
        shader_storage_image_multisample,
        shader_storage_image_read_without_format,
        shader_storage_image_write_without_format,
        shader_uniform_buffer_array_dynamic_indexing,
        shader_sampled_image_array_dynamic_indexing,
        shader_storage_buffer_array_dynamic_indexing,
        shader_storage_image_array_dynamic_indexing,
        shader_clip_distance,
        shader_cull_distance,
        shader_float64,
        shader_int64,
        shader_int16,
        shader_resource_residency,
        shader_resource_min_lod,
        sparse_binding,
        sparse_residency_buffer,
        sparse_residency_image2_d,
        sparse_residency_image3_d,
        sparse_residency2_samples,
        sparse_residency4_samples,
        sparse_residency8_samples,
        sparse_residency16_samples,
        sparse_residency_aliased,
        variable_multisample_rate,
        inherited_queries,
    );
    w
}

pub fn image_format(c: &vk::ImageFormatProperties) -> p::VkImageFormatProperties {
    let mut w = p::VkImageFormatProperties::default();
    w.max_extent = p::VkExtent3D {
        width: c.max_extent.width,
        height: c.max_extent.height,
        depth: c.max_extent.depth,
    };
    same!(w, c, max_mip_levels, max_array_layers, max_resource_size);
    w.sample_counts = p::VkSampleCountFlags(c.sample_counts.as_raw());
    w
}

pub fn memory_properties(
    c: &vk::PhysicalDeviceMemoryProperties,
) -> p::VkPhysicalDeviceMemoryProperties {
    let mut w = p::VkPhysicalDeviceMemoryProperties::default();
    w.memory_type_count = c.memory_type_count;
    for i in 0..c.memory_type_count as usize {
        w.memory_types[i] = p::VkMemoryType {
            property_flags: p::VkMemoryPropertyFlags(c.memory_types[i].property_flags.as_raw()),
            heap_index: c.memory_types[i].heap_index,
        };
    }
    w.memory_heap_count = c.memory_heap_count;
    for i in 0..c.memory_heap_count as usize {
        w.memory_heaps[i] = p::VkMemoryHeap {
            size: c.memory_heaps[i].size,
            flags: p::VkMemoryHeapFlags(c.memory_heaps[i].flags.as_raw()),
        };
    }
    w
}

pub fn queue_family(c: &vk::QueueFamilyProperties) -> p::VkQueueFamilyProperties {
    let mut w = p::VkQueueFamilyProperties::default();
    w.queue_flags = p::VkQueueFlags(c.queue_flags.as_raw());
    w.queue_count = c.queue_count;
    w.timestamp_valid_bits = c.timestamp_valid_bits;
    w.min_image_transfer_granularity = p::VkExtent3D {
        width: c.min_image_transfer_granularity.width,
        height: c.min_image_transfer_granularity.height,
        depth: c.min_image_transfer_granularity.depth,
    };
    w
}
