//! Real Icd query/allocate/bind calls across the generated wire codec. Backend
//! layout is deterministic here; no driver capability or hardware proof.
use super::*;
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct Backend {
    binds: Vec<u64>,
}
fn requirements() -> p::VkMemoryRequirements {
    p::VkMemoryRequirements {
        size: 4096,
        alignment: 4096,
        memory_type_bits: 1,
    }
}
impl p::Server for Backend {
    fn create_image(&mut self, a: p::VkCreateImageArgs) -> sv_wire::Result<p::VkCreateImageReply> {
        Ok(p::VkCreateImageReply {
            result: p::VkResult(0),
            p_image: a.p_image,
        })
    }
    fn destroy_image(&mut self, _: p::VkDestroyImageArgs) -> sv_wire::Result<()> {
        Ok(())
    }
    fn get_image_memory_requirements(
        &mut self,
        _: p::VkGetImageMemoryRequirementsArgs,
    ) -> sv_wire::Result<p::VkGetImageMemoryRequirementsReply> {
        Ok(p::VkGetImageMemoryRequirementsReply {
            p_memory_requirements: Some(Box::new(requirements())),
        })
    }
    fn get_image_memory_requirements2(
        &mut self,
        a: p::VkGetImageMemoryRequirements2Args,
    ) -> sv_wire::Result<p::VkGetImageMemoryRequirements2Reply> {
        let mut out = a.p_memory_requirements.unwrap_or_default();
        out.memory_requirements = requirements();
        Ok(p::VkGetImageMemoryRequirements2Reply {
            p_memory_requirements: Some(out),
        })
    }
    fn get_image_subresource_layout(
        &mut self,
        _: p::VkGetImageSubresourceLayoutArgs,
    ) -> sv_wire::Result<p::VkGetImageSubresourceLayoutReply> {
        Ok(p::VkGetImageSubresourceLayoutReply {
            p_layout: Some(Box::new(p::VkSubresourceLayout {
                offset: 0,
                row_pitch: 256,
                size: 4096,
                ..Default::default()
            })),
        })
    }
    fn get_image_subresource_layout2(
        &mut self,
        _: p::VkGetImageSubresourceLayout2Args,
    ) -> sv_wire::Result<p::VkGetImageSubresourceLayout2Reply> {
        Ok(p::VkGetImageSubresourceLayout2Reply {
            p_layout: Some(Box::new(p::VkSubresourceLayout2 {
                subresource_layout: p::VkSubresourceLayout {
                    offset: 0,
                    row_pitch: 256,
                    size: 4096,
                    ..Default::default()
                },
                ..Default::default()
            })),
        })
    }
    fn allocate_memory(
        &mut self,
        a: p::VkAllocateMemoryArgs,
    ) -> sv_wire::Result<p::VkAllocateMemoryReply> {
        Ok(p::VkAllocateMemoryReply {
            result: p::VkResult(0),
            p_memory: a.p_memory,
        })
    }
    fn bind_image_memory(
        &mut self,
        a: p::VkBindImageMemoryArgs,
    ) -> sv_wire::Result<p::VkBindImageMemoryReply> {
        self.binds.push(a.memory_offset);
        Ok(p::VkBindImageMemoryReply {
            result: p::VkResult(0),
        })
    }
    fn bind_image_memory2(
        &mut self,
        a: p::VkBindImageMemory2Args,
    ) -> sv_wire::Result<p::VkBindImageMemory2Reply> {
        self.binds
            .extend(a.p_bind_infos.iter().map(|i| i.memory_offset));
        Ok(p::VkBindImageMemory2Reply {
            result: p::VkResult(0),
        })
    }
}
struct Transport(Arc<Mutex<Backend>>);
impl sv_icd_entry::Transport for Transport {
    fn call(
        &self,
        _: sv_icd_entry::RingClass,
        _: u32,
        bytes: &[u8],
        reply: &mut Vec<u8>,
    ) -> IcdResult<()> {
        p::dispatch(
            &mut sv_wire::Decoder::new(bytes),
            &mut *self.0.lock().unwrap(),
            reply,
        )
        .map_err(LowerError::Wire)?;
        Ok(())
    }
    fn send(&self, class: sv_icd_entry::RingClass, op: u32, bytes: &[u8]) -> IcdResult<()> {
        self.call(class, op, bytes, &mut Vec::new())
    }
}
fn create_info() -> p::VkImageCreateInfo {
    let mut info = p::VkImageCreateInfo::default();
    info.format = p::VkFormat(16); // R8G8_UNORM, standalone NV12 chroma view.
    info.image_type = p::VkImageType(1);
    info.extent = p::VkExtent3D {
        width: 128,
        height: 16,
        depth: 1,
    };
    info.mip_levels = 1;
    info.array_layers = 1;
    info.samples = p::VkSampleCountFlagBits::VK_SAMPLE_COUNT_1_BIT;
    info.tiling = p::VkImageTiling(crate::dmabuf::TILING_DRM_FORMAT_MODIFIER);
    info.p_next = vec![p::PNextLink::VkImageDrmFormatModifierExplicitCreateInfoEXT(
        Box::new(p::VkImageDrmFormatModifierExplicitCreateInfoEXT {
            drm_format_modifier: 0,
            drm_format_modifier_plane_count: 1,
            p_plane_layouts: vec![p::VkSubresourceLayout {
                offset: 8192,
                row_pitch: 256,
                ..Default::default()
            }],
            ..Default::default()
        }),
    )];
    info
}
#[test]
fn explicit_requirements_query_allocate_bind_preserves_full_shifted_extent() {
    crate::dmabuf::test_enabled_scope(|| {
        let backend = Arc::new(Mutex::new(Backend::default()));
        let mut icd = SharedVulkan::new(1 << 20).unwrap();
        icd.set_test_transport(Box::new(Transport(backend.clone())));
        let device = p::VkDevice(1);
        let image = icd
            .create_image(p::VkCreateImageArgs {
                device,
                p_create_info: Some(Box::new(create_info())),
                p_image: None,
            })
            .unwrap()
            .p_image
            .unwrap();
        let one = icd
            .get_image_memory_requirements(p::VkGetImageMemoryRequirementsArgs {
                device,
                image,
                p_memory_requirements: None,
            })
            .unwrap()
            .p_memory_requirements
            .unwrap();
        let two = icd
            .get_image_memory_requirements2(p::VkGetImageMemoryRequirements2Args {
                device,
                p_info: Some(Box::new(p::VkImageMemoryRequirementsInfo2 {
                    image,
                    ..Default::default()
                })),
                p_memory_requirements: Some(Box::default()),
            })
            .unwrap()
            .p_memory_requirements
            .unwrap();
        let no_handle = icd
            .get_device_image_memory_requirements(p::VkGetDeviceImageMemoryRequirementsArgs {
                device,
                p_info: Some(Box::new(p::VkDeviceImageMemoryRequirements {
                    p_create_info: Some(Box::new(create_info())),
                    ..Default::default()
                })),
                p_memory_requirements: Some(Box::default()),
            })
            .unwrap()
            .p_memory_requirements
            .unwrap();
        assert_eq!(one.size, 12288);
        assert_eq!(one.alignment, 4096);
        assert_eq!(two.memory_requirements, *one);
        assert_eq!(no_handle.memory_requirements, *one);
        let allocate = |size| {
            icd.allocate_memory(p::VkAllocateMemoryArgs {
                device,
                p_allocate_info: Some(Box::new(p::VkMemoryAllocateInfo {
                    allocation_size: size,
                    memory_type_index: 0,
                    ..Default::default()
                })),
                p_memory: None,
            })
            .unwrap()
            .p_memory
            .unwrap()
        };
        let layout = icd
            .get_image_subresource_layout(p::VkGetImageSubresourceLayoutArgs {
                device,
                image,
                p_subresource: Some(Box::new(p::VkImageSubresource {
                    aspect_mask: p::VkImageAspectFlags(crate::dmabuf::ASPECT_MEMORY_PLANE_0),
                    ..Default::default()
                })),
                p_layout: Some(Box::default()),
            })
            .unwrap()
            .p_layout
            .unwrap();
        let layout2 = icd
            .get_image_subresource_layout2(p::VkGetImageSubresourceLayout2Args {
                device,
                image,
                p_subresource: Some(Box::new(p::VkImageSubresource2 {
                    image_subresource: p::VkImageSubresource {
                        aspect_mask: p::VkImageAspectFlags(crate::dmabuf::ASPECT_MEMORY_PLANE_0),
                        ..Default::default()
                    },
                    ..Default::default()
                })),
                p_layout: Some(Box::default()),
            })
            .unwrap()
            .p_layout
            .unwrap();
        assert_eq!(layout.offset, 8192);
        assert_eq!(layout2.subresource_layout.offset, 8192);
        let exact = allocate(one.size);
        assert_eq!(
            icd.bind_image_memory(p::VkBindImageMemoryArgs {
                device,
                image,
                memory: exact,
                memory_offset: 0
            })
            .unwrap()
            .result
            .0,
            0
        );
        let short = allocate(one.size - 1);
        assert_ne!(
            icd.bind_image_memory(p::VkBindImageMemoryArgs {
                device,
                image,
                memory: short,
                memory_offset: 0
            })
            .unwrap()
            .result
            .0,
            0
        );
        let second = icd
            .create_image(p::VkCreateImageArgs {
                device,
                p_create_info: Some(Box::new(create_info())),
                p_image: None,
            })
            .unwrap()
            .p_image
            .unwrap();
        let bind = |memory| p::VkBindImageMemory2Args {
            device,
            bind_info_count: 1,
            p_bind_infos: vec![p::VkBindImageMemoryInfo {
                image: second,
                memory,
                memory_offset: 0,
                ..Default::default()
            }],
        };
        assert_ne!(icd.bind_image_memory2(bind(short)).unwrap().result.0, 0);
        assert_eq!(icd.bind_image_memory2(bind(exact)).unwrap().result.0, 0);
        assert_eq!(
            backend.lock().unwrap().binds,
            vec![8192, 8192],
            "short allocations must not reach either native bind API"
        );
    });
}
