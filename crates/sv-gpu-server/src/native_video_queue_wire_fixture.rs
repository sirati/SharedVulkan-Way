//! Wire-only creation and recording for the isolated native queue fixture.
//! No resource registry, parameters, Decode, or Plans metadata is injected.
use super::*;
use sv_gpu_entry::Forward;
const SESSION: u64 = 20;
const PARAMETERS: u64 = 21;
const POOL: u64 = 22;
const DST_IMAGE: u64 = 23;
const SETUP_IMAGE: u64 = 24;
const DST_MEMORY: u64 = 25;
const SETUP_MEMORY: u64 = 26;
type Rpc = Forward<GpuServer>;
fn profile() -> p::VkVideoProfileInfoKHR {
    p::VkVideoProfileInfoKHR {
        video_codec_operation: p::VkVideoCodecOperationFlagBitsKHR(
            vk::VideoCodecOperationFlagsKHR::DECODE_H264.as_raw() as i32,
        ),
        chroma_subsampling: p::VkVideoChromaSubsamplingFlagsKHR(
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_420.as_raw(),
        ),
        luma_bit_depth: p::VkVideoComponentBitDepthFlagsKHR(
            vk::VideoComponentBitDepthFlagsKHR::TYPE_8.as_raw(),
        ),
        chroma_bit_depth: p::VkVideoComponentBitDepthFlagsKHR(
            vk::VideoComponentBitDepthFlagsKHR::TYPE_8.as_raw(),
        ),
        p_next: vec![p::PNextLink::VkVideoDecodeH264ProfileInfoKHR(Box::new(
            p::VkVideoDecodeH264ProfileInfoKHR {
                std_profile_idc: p::StdVideoH264ProfileIdc(100),
                picture_layout: p::VkVideoDecodeH264PictureLayoutFlagBitsKHR(
                    vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE.as_raw() as i32,
                ),
                ..Default::default()
            },
        ))],
    }
}
pub(super) fn profiles() -> p::PNextLink {
    p::PNextLink::VkVideoProfileListInfoKHR(Box::new(p::VkVideoProfileListInfoKHR {
        profile_count: 1,
        p_profiles: vec![profile()],
        ..Default::default()
    }))
}
fn extent() -> p::VkExtent2D {
    p::VkExtent2D {
        width: 64,
        height: 64,
    }
}
fn range() -> p::VkImageSubresourceRange {
    p::VkImageSubresourceRange {
        aspect_mask: p::VkImageAspectFlags(vk::ImageAspectFlags::COLOR.as_raw()),
        level_count: 1,
        layer_count: 1,
        ..Default::default()
    }
}
fn picture(view: u64) -> p::VkVideoPictureResourceInfoKHR {
    p::VkVideoPictureResourceInfoKHR {
        coded_extent: extent(),
        image_view_binding: p::VkImageView(view),
        ..Default::default()
    }
}
pub(super) fn image(
    rpc: &mut Rpc,
    image: u64,
    memory: u64,
    view: u64,
    usage: vk::ImageUsageFlags,
) -> Result<(), String> {
    checked(
        wire(rpc.create_image(p::VkCreateImageArgs {
            device: p::VkDevice(DEVICE),
            p_image: Some(p::VkImage(image)),
            p_create_info: Some(Box::new(p::VkImageCreateInfo {
                p_next: vec![profiles()],
                image_type: p::VkImageType(vk::ImageType::TYPE_2D.as_raw()),
                format: p::VkFormat(vk::Format::G8_B8R8_2PLANE_420_UNORM.as_raw()),
                extent: p::VkExtent3D {
                    width: 64,
                    height: 64,
                    depth: 1,
                },
                mip_levels: 1,
                array_layers: 1,
                samples: p::VkSampleCountFlagBits(1),
                tiling: p::VkImageTiling(vk::ImageTiling::LINEAR.as_raw()),
                usage: p::VkImageUsageFlags((usage | vk::ImageUsageFlags::SAMPLED).as_raw()),
                sharing_mode: p::VkSharingMode(vk::SharingMode::EXCLUSIVE.as_raw()),
                initial_layout: p::VkImageLayout(vk::ImageLayout::UNDEFINED.as_raw()),
                ..Default::default()
            })),
        }))?
        .result,
    )?;
    let requirements = wire(rpc.get_image_memory_requirements(
        p::VkGetImageMemoryRequirementsArgs {
            device: p::VkDevice(DEVICE),
            image: p::VkImage(image),
            p_memory_requirements: Some(Box::new(Default::default())),
        },
    ))?
    .p_memory_requirements
    .ok_or("missing image requirements")?;
    if requirements.memory_type_bits & 1 == 0 {
        return Err("logical dedicated memory type0 absent".into());
    }
    checked(
        wire(rpc.allocate_memory(p::VkAllocateMemoryArgs {
            device: p::VkDevice(DEVICE),
            p_memory: Some(p::VkDeviceMemory(memory)),
            p_allocate_info: Some(Box::new(p::VkMemoryAllocateInfo {
                allocation_size: requirements.size,
                memory_type_index: 0,
                p_next: vec![p::PNextLink::VkMemoryDedicatedAllocateInfo(Box::new(
                    p::VkMemoryDedicatedAllocateInfo {
                        image: p::VkImage(image),
                        ..Default::default()
                    },
                ))],
            })),
        }))?
        .result,
    )?;
    checked(
        wire(rpc.bind_image_memory(p::VkBindImageMemoryArgs {
            device: p::VkDevice(DEVICE),
            image: p::VkImage(image),
            memory: p::VkDeviceMemory(memory),
            memory_offset: 0,
        }))?
        .result,
    )?;
    checked(
        wire(rpc.create_image_view(p::VkCreateImageViewArgs {
            device: p::VkDevice(DEVICE),
            p_view: Some(p::VkImageView(view)),
            p_create_info: Some(Box::new(p::VkImageViewCreateInfo {
                p_next: vec![p::PNextLink::VkImageViewUsageCreateInfo(Box::new(
                    p::VkImageViewUsageCreateInfo {
                        usage: p::VkImageUsageFlags(usage.as_raw()),
                        ..Default::default()
                    },
                ))],
                image: p::VkImage(image),
                view_type: p::VkImageViewType(vk::ImageViewType::TYPE_2D.as_raw()),
                format: p::VkFormat(vk::Format::G8_B8R8_2PLANE_420_UNORM.as_raw()),
                subresource_range: range(),
                ..Default::default()
            })),
        }))?
        .result,
    )?;
    Ok(())
}
fn parameters(rpc: &mut Rpc, queue_family: u32) -> Result<(), String> {
    parameters_for(
        rpc,
        queue_family,
        SESSION,
        PARAMETERS,
        crate::native_video_fixture_parameters::parameter_sets(),
    )
}
pub(super) fn parameters_for(
    rpc: &mut Rpc,
    queue_family: u32,
    session: u64,
    parameters: u64,
    add: p::VkVideoDecodeH264SessionParametersAddInfoKHR,
) -> Result<(), String> {
    let mut name = [0u8; 256];
    for (dst, src) in name
        .iter_mut()
        .zip(b"VK_STD_vulkan_video_codec_h264_decode\0")
    {
        *dst = *src;
    }
    checked(
        wire(
            rpc.create_video_session_khr(p::VkCreateVideoSessionKHRArgs {
                device: p::VkDevice(DEVICE),
                p_video_session: Some(p::VkVideoSessionKHR(session)),
                p_create_info: Some(Box::new(p::VkVideoSessionCreateInfoKHR {
                    queue_family_index: queue_family,
                    p_video_profile: Some(Box::new(profile())),
                    picture_format: p::VkFormat(vk::Format::G8_B8R8_2PLANE_420_UNORM.as_raw()),
                    reference_picture_format: p::VkFormat(
                        vk::Format::G8_B8R8_2PLANE_420_UNORM.as_raw(),
                    ),
                    max_coded_extent: extent(),
                    max_dpb_slots: 16,
                    max_active_reference_pictures: 16,
                    p_std_header_version: Some(Box::new(p::VkExtensionProperties {
                        extension_name: name,
                        spec_version: 1 << 22,
                    })),
                    ..Default::default()
                })),
            }),
        )?
        .result,
    )?;
    checked(
        wire(
            rpc.create_video_session_parameters_khr(p::VkCreateVideoSessionParametersKHRArgs {
                device: p::VkDevice(DEVICE),
                p_video_session_parameters: Some(p::VkVideoSessionParametersKHR(parameters)),
                p_create_info: Some(Box::new(p::VkVideoSessionParametersCreateInfoKHR {
                    video_session: p::VkVideoSessionKHR(session),
                    p_next: vec![
                        p::PNextLink::VkVideoDecodeH264SessionParametersCreateInfoKHR(Box::new(
                            p::VkVideoDecodeH264SessionParametersCreateInfoKHR {
                                max_std_sps_count: 1,
                                max_std_pps_count: 1,
                                p_parameters_add_info: Some(Box::new(add)),
                                ..Default::default()
                            },
                        )),
                    ],
                    ..Default::default()
                })),
            }),
        )?
        .result,
    )?;
    Ok(())
}
fn record(
    rpc: &mut Rpc,
    source_len: u64,
    mixed: Option<&mixed_compute::Prepared>,
) -> Result<(), String> {
    checked(
        wire(rpc.begin_command_buffer(p::VkBeginCommandBufferArgs {
            command_buffer: p::VkCommandBuffer(COMMAND),
            p_begin_info: Some(Box::new(Default::default())),
        }))?
        .result,
    )?;
    if let Some(mixed) = mixed {
        mixed.record_before_decode(rpc, COMMAND)?;
    }
    let barriers = vec![
        (DST_IMAGE, vk::ImageLayout::VIDEO_DECODE_DST_KHR),
        (SETUP_IMAGE, vk::ImageLayout::VIDEO_DECODE_DPB_KHR),
    ]
    .into_iter()
    .map(|(image, layout)| p::VkImageMemoryBarrier2 {
        src_stage_mask: p::VkPipelineStageFlags2(vk::PipelineStageFlags2::TOP_OF_PIPE.as_raw()),
        dst_stage_mask: p::VkPipelineStageFlags2(
            vk::PipelineStageFlags2::VIDEO_DECODE_KHR.as_raw(),
        ),
        old_layout: p::VkImageLayout(vk::ImageLayout::UNDEFINED.as_raw()),
        new_layout: p::VkImageLayout(layout.as_raw()),
        dst_access_mask: p::VkAccessFlags2(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR.as_raw()),
        src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        image: p::VkImage(image),
        subresource_range: range(),
        ..Default::default()
    })
    .collect();
    wire(rpc.cmd_pipeline_barrier2(p::VkCmdPipelineBarrier2Args {
        command_buffer: p::VkCommandBuffer(COMMAND),
        p_dependency_info: Some(Box::new(p::VkDependencyInfo {
            image_memory_barrier_count: 2,
            p_image_memory_barriers: barriers,
            ..Default::default()
        })),
    }))?;
    wire(
        rpc.cmd_begin_video_coding_khr(p::VkCmdBeginVideoCodingKHRArgs {
            command_buffer: p::VkCommandBuffer(COMMAND),
            p_begin_info: Some(Box::new(p::VkVideoBeginCodingInfoKHR {
                video_session: p::VkVideoSessionKHR(SESSION),
                video_session_parameters: p::VkVideoSessionParametersKHR(PARAMETERS),
                reference_slot_count: 1,
                p_reference_slots: vec![p::VkVideoReferenceSlotInfoKHR {
                    slot_index: -1,
                    p_picture_resource: Some(Box::new(picture(SETUP))),
                    ..Default::default()
                }],
                ..Default::default()
            })),
        }),
    )?;
    wire(
        rpc.cmd_control_video_coding_khr(p::VkCmdControlVideoCodingKHRArgs {
            command_buffer: p::VkCommandBuffer(COMMAND),
            p_coding_control_info: Some(Box::new(p::VkVideoCodingControlInfoKHR {
                flags: p::VkVideoCodingControlFlagsKHR(
                    vk::VideoCodingControlFlagsKHR::RESET.as_raw(),
                ),
                ..Default::default()
            })),
        }),
    )?;
    wire(rpc.cmd_pipeline_barrier2(p::VkCmdPipelineBarrier2Args {
        command_buffer: p::VkCommandBuffer(COMMAND),
        p_dependency_info: Some(Box::new(p::VkDependencyInfo {
            buffer_memory_barrier_count: 1,
            p_buffer_memory_barriers: vec![p::VkBufferMemoryBarrier2 {
                src_stage_mask: p::VkPipelineStageFlags2(vk::PipelineStageFlags2::HOST.as_raw()),
                src_access_mask: p::VkAccessFlags2(vk::AccessFlags2::HOST_WRITE.as_raw()),
                dst_stage_mask: p::VkPipelineStageFlags2(
                    vk::PipelineStageFlags2::VIDEO_DECODE_KHR.as_raw(),
                ),
                dst_access_mask: p::VkAccessFlags2(
                    vk::AccessFlags2::VIDEO_DECODE_READ_KHR.as_raw(),
                ),
                src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                buffer: p::VkBuffer(SOURCE),
                offset: 0,
                size: source_len,
                ..Default::default()
            }],
            ..Default::default()
        })),
    }))?;
    wire(rpc.cmd_decode_video_khr(p::VkCmdDecodeVideoKHRArgs {
        command_buffer: p::VkCommandBuffer(COMMAND),
        p_decode_info: Some(Box::new(p::VkVideoDecodeInfoKHR {
            p_next: vec![p::PNextLink::VkVideoDecodeH264PictureInfoKHR(Box::new(
                p::VkVideoDecodeH264PictureInfoKHR {
                    p_std_picture_info: Some(Box::new(
                        crate::native_video_fixture_parameters::idr_picture(),
                    )),
                    slice_count: 1,
                    p_slice_offsets: vec![0],
                    ..Default::default()
                },
            ))],
            src_buffer: p::VkBuffer(SOURCE),
            src_buffer_range: source_len,
            dst_picture_resource: picture(DST),
            p_setup_reference_slot: Some(Box::new(p::VkVideoReferenceSlotInfoKHR {
                slot_index: 0,
                p_picture_resource: Some(Box::new(picture(SETUP))),
                p_next: vec![p::PNextLink::VkVideoDecodeH264DpbSlotInfoKHR(Box::new(
                    p::VkVideoDecodeH264DpbSlotInfoKHR {
                        p_std_reference_info: Some(Box::new(Default::default())),
                        ..Default::default()
                    },
                ))],
            })),
            ..Default::default()
        })),
    }))?;
    wire(rpc.cmd_end_video_coding_khr(p::VkCmdEndVideoCodingKHRArgs {
        command_buffer: p::VkCommandBuffer(COMMAND),
        p_end_coding_info: Some(Box::new(Default::default())),
    }))?;
    if let Some(mixed) = mixed {
        mixed.record_after_decode(rpc, COMMAND)?;
    }
    checked(
        wire(rpc.end_command_buffer(p::VkEndCommandBufferArgs {
            command_buffer: p::VkCommandBuffer(COMMAND),
        }))?
        .result,
    )?;
    Ok(())
}
pub(super) fn run(sync2: bool) -> Result<(), String> {
    let fixture = prepare()?;
    submit_and_verify(
        fixture.rpc,
        sync2,
        true,
        fixture.shared,
        fixture.destination,
        fixture.setup,
        fixture.source,
        fixture.pool,
        fixture.mixed,
    )
}

pub(super) fn prepare() -> Result<Fixture, String> {
    let (_, _, payload) = first_picture()?;
    let driver = Arc::new(Driver::open(0).map_err(|e| e.to_string())?);
    let region = Arc::new(SharedRegion::heap(1 << 20).map_err(|e| e.to_string())?);
    region.write_bytes(0, &payload).map_err(|e| e.to_string())?;
    let arena = arena::import_whole(&driver, region).map_err(|e| e.to_string())?;
    let mut state = DispatchState::default();
    state.video = NativeVideo::new(Ready {
        hardware_h264: true,
        joined_nv12_import: true,
        native_queue_bound_decode: true,
    });
    let mut objects = Objects::new();
    objects.set_arena(arena.memory, arena.len);
    objects.insert(DEVICE, HandleType::VkDevice, driver.device.handle());
    objects.insert(QUEUE, HandleType::VkQueue, driver.queue);
    let shared = Arc::new(Shared {
        native_video: Mutex::new(state),
        fns: Arc::new(driver.load_fns()),
        driver: Arc::clone(&driver),
        arena,
        objects: Mutex::new(objects),
        queue_lock: Arc::new(Mutex::new(())),
        pool_lock: Arc::new(Mutex::new(())),
        video_stable: Mutex::new(Default::default()),
        poisoned: Default::default(),
    });
    let mut rpc = Forward::new(GpuServer::new(Arc::clone(&shared)));
    checked(
        wire(rpc.create_buffer(p::VkCreateBufferArgs {
            device: p::VkDevice(DEVICE),
            p_buffer: Some(p::VkBuffer(SOURCE)),
            p_create_info: Some(Box::new(p::VkBufferCreateInfo {
                p_next: vec![profiles()],
                size: payload.len() as u64,
                usage: p::VkBufferUsageFlags(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR.as_raw()),
                sharing_mode: p::VkSharingMode(vk::SharingMode::EXCLUSIVE.as_raw()),
                ..Default::default()
            })),
        }))?
        .result,
    )?;
    checked(
        wire(rpc.bind_buffer_memory(p::VkBindBufferMemoryArgs {
            device: p::VkDevice(DEVICE),
            buffer: p::VkBuffer(SOURCE),
            memory: p::VkDeviceMemory(crate::ARENA_MEMORY_ID),
            memory_offset: 0,
        }))?
        .result,
    )?;
    image(
        &mut rpc,
        DST_IMAGE,
        DST_MEMORY,
        DST,
        vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
    )?;
    image(
        &mut rpc,
        SETUP_IMAGE,
        SETUP_MEMORY,
        SETUP,
        vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR,
    )?;
    parameters(&mut rpc, driver.queue_family)?;
    checked(
        wire(rpc.create_command_pool(p::VkCreateCommandPoolArgs {
            device: p::VkDevice(DEVICE),
            p_command_pool: Some(p::VkCommandPool(POOL)),
            p_create_info: Some(Box::new(p::VkCommandPoolCreateInfo {
                queue_family_index: driver.queue_family,
                ..Default::default()
            })),
        }))?
        .result,
    )?;
    checked(
        wire(
            rpc.allocate_command_buffers(p::VkAllocateCommandBuffersArgs {
                device: p::VkDevice(DEVICE),
                p_allocate_info: Some(Box::new(p::VkCommandBufferAllocateInfo {
                    command_pool: p::VkCommandPool(POOL),
                    level: p::VkCommandBufferLevel(vk::CommandBufferLevel::PRIMARY.as_raw()),
                    command_buffer_count: 2,
                    ..Default::default()
                })),
                p_command_buffers: vec![p::VkCommandBuffer(COMMAND), p::VkCommandBuffer(ORDINARY)],
            }),
        )?
        .result,
    )?;
    // Ordinary empty native CB supplies the earlier binary signal. All actual
    // image and source barriers remain in the wire-recorded video plan.
    checked(
        wire(rpc.begin_command_buffer(p::VkBeginCommandBufferArgs {
            command_buffer: p::VkCommandBuffer(ORDINARY),
            p_begin_info: Some(Box::new(Default::default())),
        }))?
        .result,
    )?;
    checked(
        wire(rpc.end_command_buffer(p::VkEndCommandBufferArgs {
            command_buffer: p::VkCommandBuffer(ORDINARY),
        }))?
        .result,
    )?;
    let mixed = if std::env::args().any(|a| a == "--mixed-compute") {
        let mixed = Arc::new(mixed_compute::Prepared::prepare(&mut rpc)?);
        mixed.resource_report();
        Some(mixed)
    } else {
        None
    };
    record(&mut rpc, payload.len() as u64, mixed.as_deref())?;
    // Test diagnostics resolve canonical owners only after wire recording has
    // completed. These lookups never mutate Plans or resource metadata.
    let (destination, setup, source, pool) = {
        let objects = shared.objects();
        (
            Arc::clone(&objects.native_video.images[&DST_IMAGE]),
            Arc::clone(&objects.native_video.images[&SETUP_IMAGE]),
            objects
                .get::<vk::Buffer>(HandleType::VkBuffer, SOURCE)
                .map_err(|e| format!("{e:?}"))?,
            objects
                .get::<vk::CommandPool>(HandleType::VkCommandPool, POOL)
                .map_err(|e| format!("{e:?}"))?,
        )
    };
    if Arc::ptr_eq(&destination, &setup) {
        return Err("wire-created targets alias".into());
    }
    // Synchronization handles are created through the same RPC as the resource
    // and coding calls. The common verifier signals HOST through this protocol.
    checked(
        wire(rpc.create_semaphore(p::VkCreateSemaphoreArgs {
            device: p::VkDevice(DEVICE),
            p_semaphore: Some(p::VkSemaphore(TIMELINE)),
            p_create_info: Some(Box::new(p::VkSemaphoreCreateInfo {
                p_next: vec![p::PNextLink::VkSemaphoreTypeCreateInfo(Box::new(
                    p::VkSemaphoreTypeCreateInfo {
                        semaphore_type: p::VkSemaphoreType(vk::SemaphoreType::TIMELINE.as_raw()),
                        initial_value: 0,
                        ..Default::default()
                    },
                ))],
                ..Default::default()
            })),
        }))?
        .result,
    )?;
    checked(
        wire(rpc.create_semaphore(p::VkCreateSemaphoreArgs {
            device: p::VkDevice(DEVICE),
            p_semaphore: Some(p::VkSemaphore(BINARY)),
            p_create_info: Some(Box::new(Default::default())),
        }))?
        .result,
    )?;
    checked(
        wire(rpc.create_fence(p::VkCreateFenceArgs {
            device: p::VkDevice(DEVICE),
            p_fence: Some(p::VkFence(FENCE)),
            p_create_info: Some(Box::new(Default::default())),
        }))?
        .result,
    )?;
    println!("wire fixture resources/session/Std parameters/coding recorded through Forward");
    let timeline = shared
        .objects()
        .get::<vk::Semaphore>(HandleType::VkSemaphore, TIMELINE)
        .map_err(|e| format!("{e:?}"))?;
    let image_requirement_bytes = [destination.requirements().size, setup.requirements().size];
    let source_bytes = payload.len() as u64;
    let user_fence = shared
        .objects()
        .get::<vk::Fence>(HandleType::VkFence, FENCE)
        .map_err(|e| format!("{e:?}"))?;
    Ok(Fixture {
        event_prefix: None,
        mixed,
        diagnostic_fence: None,
        user_fence,
        timeline,
        image_requirement_bytes,
        source_bytes,
        rpc,
        shared,
        destination,
        setup,
        source,
        pool,
    })
}
