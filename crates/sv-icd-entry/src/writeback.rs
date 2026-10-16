//! The output links the generator will not write back, written back by hand.
//!
//! waycompose-design.md §96. The generator refuses a `store_*` for a struct with
//! a pointer-valued member, and it is right to: it cannot know how much storage
//! is behind the pointer. Its refusal used to mean "leave the link exactly as the
//! application left it", which is the conservative direction for a link nobody
//! needs. Three of the refused links are not that:
//!
//! * **`VkPhysicalDeviceVulkan14Properties`** is where a Vulkan 1.4 application
//!   reads `maxPushDescriptors`, `defaultRobustness*` and a dozen other limits.
//!   Left untouched it reads zeros, and a zero `maxPushDescriptors` with push
//!   descriptors advertised is a contradiction the validation layer reports on
//!   every push-descriptor layout Zink makes
//!   (`VUID-VkDescriptorSetLayoutCreateInfo-flags-00281`).
//! * **`VkPhysicalDeviceLayeredApiPropertiesListKHR`** is how Zink tells which
//!   driver it is on. It passes a capacity of one, and a link left as it was
//!   says "one layered API" whose properties are all zero; Zink then takes that
//!   zero as the driver id, calls the renderer "zink Vulkan 1.4(… (Driver
//!   Unknown))", and — the part that matters — applies none of its per-driver
//!   workarounds, RADV's or Intel's.
//! * **`VkPhysicalDeviceHostImageCopyProperties`** has the same two arrays as
//!   the 1.4 struct and nothing else.
//!
//! What the pointers point at is the two-call idiom: the application says how
//! many elements it has room for and the implementation says how many there
//! are. The GPU side was asked with no room (the lowering sends every array
//! empty), so the reply carries the counts and no elements. Copy layouts
//! matter only to `hostImageCopy`, which the forwarder does not advertise, so
//! their counts are reported as zero rather than as a number of elements this
//! side cannot supply. The layered-API list is written with its count and the
//! top-level fields of each entry the application made room for.

use core::ffi::c_void;

/// The default arm of the generated `store_pnext`: a reply link whose struct
/// the generator did not write back.
///
/// # Safety
/// As `store_pnext`: `p` is a live link of the application's output chain, of
/// the struct `link`'s `sType` names, valid for writes.
pub(crate) unsafe fn store_refused_link(link: &sv_proto::PNextLink, p: *mut c_void) {
    use sv_proto::PNextLink as L;
    match link {
        L::VkPhysicalDeviceVulkan14Properties(s) => unsafe {
            let d = p as *mut crate::VkPhysicalDeviceVulkan14Properties;
            (*d).line_sub_pixel_precision_bits = s.line_sub_pixel_precision_bits;
            (*d).max_vertex_attrib_divisor = s.max_vertex_attrib_divisor;
            (*d).supports_non_zero_first_instance = b(s.supports_non_zero_first_instance);
            (*d).max_push_descriptors = s.max_push_descriptors;
            (*d).dynamic_rendering_local_read_depth_stencil_attachments =
                b(s.dynamic_rendering_local_read_depth_stencil_attachments);
            (*d).dynamic_rendering_local_read_multisampled_attachments =
                b(s.dynamic_rendering_local_read_multisampled_attachments);
            (*d).early_fragment_multisample_coverage_after_sample_counting =
                b(s.early_fragment_multisample_coverage_after_sample_counting);
            (*d).early_fragment_sample_mask_test_before_sample_counting =
                b(s.early_fragment_sample_mask_test_before_sample_counting);
            (*d).depth_stencil_swizzle_one_support = b(s.depth_stencil_swizzle_one_support);
            (*d).polygon_mode_point_size = b(s.polygon_mode_point_size);
            (*d).non_strict_single_pixel_wide_lines_use_parallelogram =
                b(s.non_strict_single_pixel_wide_lines_use_parallelogram);
            (*d).non_strict_wide_lines_use_parallelogram = b(s.non_strict_wide_lines_use_parallelogram);
            (*d).block_texel_view_compatible_multiple_layers =
                b(s.block_texel_view_compatible_multiple_layers);
            (*d).max_combined_image_sampler_descriptor_count =
                s.max_combined_image_sampler_descriptor_count;
            (*d).fragment_shading_rate_clamp_combiner_inputs =
                b(s.fragment_shading_rate_clamp_combiner_inputs);
            (*d).default_robustness_storage_buffers =
                crate::VkPipelineRobustnessBufferBehavior(s.default_robustness_storage_buffers.0);
            (*d).default_robustness_uniform_buffers =
                crate::VkPipelineRobustnessBufferBehavior(s.default_robustness_uniform_buffers.0);
            (*d).default_robustness_vertex_inputs =
                crate::VkPipelineRobustnessBufferBehavior(s.default_robustness_vertex_inputs.0);
            (*d).default_robustness_images =
                crate::VkPipelineRobustnessImageBehavior(s.default_robustness_images.0);
            // See the header: no elements to give, so no elements claimed.
            (*d).copy_src_layout_count = 0;
            (*d).copy_dst_layout_count = 0;
            (*d).optimal_tiling_layout_uuid = s.optimal_tiling_layout_uuid;
            (*d).identical_memory_type_requirements = b(s.identical_memory_type_requirements);
        },
        L::VkPhysicalDeviceHostImageCopyProperties(s) => unsafe {
            let d = p as *mut crate::VkPhysicalDeviceHostImageCopyProperties;
            (*d).copy_src_layout_count = 0;
            (*d).copy_dst_layout_count = 0;
            (*d).optimal_tiling_layout_uuid = s.optimal_tiling_layout_uuid;
            (*d).identical_memory_type_requirements = b(s.identical_memory_type_requirements);
        },
        L::VkPhysicalDeviceLayeredApiPropertiesListKHR(s) => unsafe {
            let d = p as *mut crate::VkPhysicalDeviceLayeredApiPropertiesListKHR;
            let there = s.layered_api_count;
            let out = (*d).p_layered_apis;
            if out.is_null() {
                (*d).layered_api_count = there;
                return;
            }
            let room = (*d).layered_api_count;
            let n = room.min(there).min(s.p_layered_apis.len() as u32);
            for (i, e) in s.p_layered_apis.iter().take(n as usize).enumerate() {
                let o = out.add(i);
                (*o).vendor_id = e.vendor_id;
                (*o).device_id = e.device_id;
                (*o).layered_api = crate::VkPhysicalDeviceLayeredApiKHR(e.layered_api.0);
                // NUL-terminated on both sides; the copy stops at the first NUL
                // and always leaves one.
                let len = e.device_name.iter().position(|c| *c == 0).unwrap_or(e.device_name.len());
                let len = len.min((*o).device_name.len() - 1);
                for j in 0..len {
                    (*o).device_name[j] = e.device_name[j] as _;
                }
                (*o).device_name[len] = 0;
            }
            (*d).layered_api_count = n;
        },
        // Every other refusal is a link nobody has needed back yet, and stays
        // as the application left it.
        _ => {}
    }
}

fn b(v: bool) -> u32 {
    u32::from(v)
}
