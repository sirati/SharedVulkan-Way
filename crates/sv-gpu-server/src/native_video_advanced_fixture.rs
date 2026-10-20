//! Exact owned Forward descriptors for the original advanced PCM matrix.
//! Diagnostic only. This module never enables product Runtime gates.
#[path = "native_video_fixture_stream.rs"]
mod stream;
use ash::vk;
use stream::{Frame, Metadata, PcmCase};
use sv_proto as p;

pub(super) struct FrameDescriptors {
    pub begin: p::VkVideoBeginCodingInfoKHR,
    pub decode: p::VkVideoDecodeInfoKHR,
}
fn bits<const N: usize>(v: &vk::native::__BindgenBitfieldUnit<[u8; N]>) -> u32 {
    assert!(N > 0 && N <= 4);
    v.get(0, (N * 8) as u8) as u32
}
fn picture(view: u64) -> p::VkVideoPictureResourceInfoKHR {
    p::VkVideoPictureResourceInfoKHR {
        coded_extent: p::VkExtent2D {
            width: 64,
            height: 64,
        },
        image_view_binding: p::VkImageView(view),
        ..Default::default()
    }
}
fn reference(slot: i32, view: u64, metadata: &Metadata) -> p::VkVideoReferenceSlotInfoKHR {
    let mut flags: vk::native::StdVideoDecodeH264ReferenceInfoFlags = unsafe { std::mem::zeroed() };
    flags.set_used_for_long_term_reference(u32::from(metadata.long_term));
    p::VkVideoReferenceSlotInfoKHR {
        slot_index: slot,
        p_picture_resource: Some(Box::new(picture(view))),
        p_next: vec![p::PNextLink::VkVideoDecodeH264DpbSlotInfoKHR(Box::new(
            p::VkVideoDecodeH264DpbSlotInfoKHR {
                p_std_reference_info: Some(Box::new(p::StdVideoDecodeH264ReferenceInfo {
                    flags: p::StdVideoDecodeH264ReferenceInfoFlags(bits(&flags._bitfield_1)),
                    frame_num: metadata.frame_num,
                    pic_order_cnt: metadata.poc,
                    ..Default::default()
                })),
                ..Default::default()
            },
        ))],
    }
}
/// Fixed four DPB resources, with view IDs starting at base. Reuse a resource
/// only after its previous producer's final reference use in this exact stream.
/// Native submission completion is required before advancing to the next frame.
fn slots(frames: &[Frame]) -> Result<Vec<usize>, String> {
    let mut result = Vec::new();
    let mut owners = [None; 4];
    for (index, frame) in frames.iter().enumerate() {
        let needed = |producer| {
            frames[index..]
                .iter()
                .any(|f| f.references.iter().any(|r| r.frame_index == producer))
        };
        let chosen = owners
            .iter()
            .position(|owner| owner.is_none_or(|old| !needed(old)))
            .ok_or("four fixed DPB resources cannot hold source lifetime")?;
        // Even if it dies after this picture, an input cannot alias this picture's setup.
        if frame
            .references
            .iter()
            .any(|r| owners[chosen] == Some(r.frame_index))
        {
            return Err("setup resource aliases active input".into());
        }
        if frame.setup.is_some() {
            owners[chosen] = Some(index);
        }
        result.push(chosen);
    }
    Ok(result)
}
fn descriptors(
    frame: &Frame,
    frame_index: usize,
    assignments: &[usize],
    base_view: u64,
    destination: u64,
    source: u64,
    session: u64,
    parameters: u64,
) -> Result<FrameDescriptors, String> {
    let refs = frame
        .references
        .iter()
        .map(|prior| {
            let slot = *assignments
                .get(prior.frame_index)
                .ok_or("reference producer outside stream")?;
            Ok(reference(
                slot as i32,
                base_view + slot as u64,
                &prior.metadata,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let setup_slot = *assignments.get(frame_index).ok_or("setup outside stream")?;
    let mut begin_refs = refs.clone();
    // All four resources are bound at Begin. The distinct decode destination
    // is not a DPB resource. A slotIndex -1 binds a resource without importing it.
    for slot in 0..4 {
        if !refs.iter().any(|r| r.slot_index == slot) {
            begin_refs.push(p::VkVideoReferenceSlotInfoKHR {
                slot_index: -1,
                p_picture_resource: Some(Box::new(picture(base_view + slot as u64))),
                ..Default::default()
            });
        }
    }
    let mut flags: vk::native::StdVideoDecodeH264PictureInfoFlags = unsafe { std::mem::zeroed() };
    flags.set_is_reference(u32::from(frame.is_reference));
    flags.set_is_intra(u32::from(frame.is_intra));
    flags.set_IdrPicFlag(u32::from(frame.idr));
    Ok(FrameDescriptors {
        begin: p::VkVideoBeginCodingInfoKHR {
            video_session: p::VkVideoSessionKHR(session),
            video_session_parameters: p::VkVideoSessionParametersKHR(parameters),
            reference_slot_count: begin_refs.len() as u32,
            p_reference_slots: begin_refs,
            ..Default::default()
        },
        decode: p::VkVideoDecodeInfoKHR {
            src_buffer: p::VkBuffer(source),
            src_buffer_offset: frame.range.start as u64,
            src_buffer_range: frame.range.len() as u64,
            dst_picture_resource: picture(destination),
            p_next: vec![p::PNextLink::VkVideoDecodeH264PictureInfoKHR(Box::new(
                p::VkVideoDecodeH264PictureInfoKHR {
                    p_std_picture_info: Some(Box::new(p::StdVideoDecodeH264PictureInfo {
                        flags: p::StdVideoDecodeH264PictureInfoFlags(bits(&flags._bitfield_1)),
                        frame_num: frame.frame_num,
                        pic_order_cnt: frame.poc,
                        idr_pic_id: frame.idr_pic_id.unwrap_or(0) as u16,
                        ..Default::default()
                    })),
                    slice_count: frame.slice_offsets.len() as u32,
                    p_slice_offsets: frame.slice_offsets.clone(),
                    ..Default::default()
                },
            ))],
            p_setup_reference_slot: frame.setup.as_ref().map(|metadata| {
                Box::new(reference(
                    setup_slot as i32,
                    base_view + setup_slot as u64,
                    metadata,
                ))
            }),
            reference_slot_count: refs.len() as u32,
            p_reference_slots: refs,
            ..Default::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_matrix_uses_exact_ranges_four_resources_and_postmarking_mmco5() {
        let mut count = 0;
        for case in (0..3)
            .flat_map(|ty| {
                [
                    PcmCase::Marking(ty),
                    PcmCase::Gap(ty),
                    PcmCase::Bootstrap(ty),
                ]
            })
            .chain([PcmCase::BSkip])
        {
            let stream = stream::pcm_original(case).unwrap();
            let assignments = slots(&stream.frames).unwrap();
            for (index, frame) in stream.frames.iter().enumerate() {
                let d = descriptors(frame, index, &assignments, 100, 9, 5, 20, 21).unwrap();
                assert_eq!(d.begin.reference_slot_count, 4);
                assert_eq!(d.decode.src_buffer_offset, frame.range.start as u64);
                assert_eq!(d.decode.src_buffer_range, frame.range.len() as u64);
                assert_eq!(d.decode.p_reference_slots.len(), frame.references.len());
                for r in &d.decode.p_reference_slots {
                    assert_ne!(
                        r.p_picture_resource.as_ref().unwrap().image_view_binding.0,
                        9
                    );
                    if let Some(setup) = &d.decode.p_setup_reference_slot {
                        assert_ne!(
                            r.p_picture_resource.as_ref().unwrap().image_view_binding.0,
                            setup
                                .p_picture_resource
                                .as_ref()
                                .unwrap()
                                .image_view_binding
                                .0
                        );
                    }
                }
                if matches!(case, PcmCase::Marking(_)) && index == 3 {
                    let p::PNextLink::VkVideoDecodeH264PictureInfoKHR(pic) = &d.decode.p_next[0]
                    else {
                        panic!()
                    };
                    let pic = pic.p_std_picture_info.as_ref().unwrap();
                    assert_eq!((pic.frame_num, pic.pic_order_cnt), (3, [6, 6]));
                    let setup = d.decode.p_setup_reference_slot.as_ref().unwrap();
                    let p::PNextLink::VkVideoDecodeH264DpbSlotInfoKHR(dp) = &setup.p_next[0] else {
                        panic!()
                    };
                    let dp = dp.p_std_reference_info.as_ref().unwrap();
                    assert_eq!((dp.frame_num, dp.pic_order_cnt, dp.flags.0), (0, [0, 0], 0));
                }
                count += 1;
            }
        }
        assert_eq!(count, 37);
    }
}

#[cfg(target_os = "macos")]
pub(super) use hardware::run;
#[cfg(target_os = "macos")]
mod hardware {
    use super::*;
    use crate::native_video_queue_fixture::{self as fixture, wire_recording};
    use fixture::{
        DispatchState, Driver, GpuServer, Image, NativeVideo, Objects, Ready, Shared, arena,
        checked, wire,
    };
    use std::{
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };
    use sv_gpu_entry::Forward;
    use sv_proto::{HandleType, Server};
    use sv_transport::region::SharedRegion;
    const DEVICE: u64 = 1;
    const QUEUE: u64 = 2;
    const SOURCE: u64 = 5;
    const FENCE: u64 = 8;
    const DST: u64 = 9;
    const BASE: u64 = 100;
    const POOL: u64 = 22;
    use std::sync::atomic::{AtomicBool, Ordering};
    type Rpc = Forward<GpuServer>;
    struct Case {
        stream: stream::Stream,
        base: usize,
        kind: PcmCase,
    }
    struct Graph {
        shared: Arc<Shared>,
        images: Vec<Arc<Image>>,
        source: vk::Buffer,
        pool: vk::CommandPool,
        fence: vk::Fence,
    }
    fn allocate(rpc: &mut Rpc, shared: &Shared, bytes: usize) -> Result<(), String> {
        checked(
            wire(rpc.create_buffer(p::VkCreateBufferArgs {
                device: p::VkDevice(DEVICE),
                p_buffer: Some(p::VkBuffer(SOURCE)),
                p_create_info: Some(Box::new(p::VkBufferCreateInfo {
                    p_next: vec![wire_recording::profiles()],
                    size: bytes as u64,
                    usage: p::VkBufferUsageFlags(
                        vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR.as_raw(),
                    ),
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
        for index in 0..5u64 {
            wire_recording::image(
                rpc,
                200 + index,
                300 + index,
                if index == 0 { DST } else { BASE + index - 1 },
                if index == 0 {
                    vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
                } else {
                    vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR
                },
            )?;
        }
        checked(
            wire(rpc.create_command_pool(p::VkCreateCommandPoolArgs {
                device: p::VkDevice(DEVICE),
                p_command_pool: Some(p::VkCommandPool(POOL)),
                p_create_info: Some(Box::new(p::VkCommandPoolCreateInfo {
                    queue_family_index: shared.driver.queue_family,
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
                        command_buffer_count: 37,
                        ..Default::default()
                    })),
                    p_command_buffers: (0..37).map(|i| p::VkCommandBuffer(1000 + i)).collect(),
                }),
            )?
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
        Ok(())
    }
    fn barriers(rpc: &mut Rpc, cb: u64, first: bool) -> Result<(), String> {
        let barriers = (0..5u64)
            .map(|i| p::VkImageMemoryBarrier2 {
                src_stage_mask: p::VkPipelineStageFlags2(
                    if first {
                        vk::PipelineStageFlags2::TOP_OF_PIPE
                    } else {
                        vk::PipelineStageFlags2::ALL_COMMANDS
                    }
                    .as_raw(),
                ),
                src_access_mask: p::VkAccessFlags2(
                    if first {
                        vk::AccessFlags2::empty()
                    } else {
                        vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE
                    }
                    .as_raw(),
                ),
                dst_stage_mask: p::VkPipelineStageFlags2(
                    vk::PipelineStageFlags2::VIDEO_DECODE_KHR.as_raw(),
                ),
                dst_access_mask: p::VkAccessFlags2(
                    (vk::AccessFlags2::VIDEO_DECODE_READ_KHR
                        | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
                        .as_raw(),
                ),
                old_layout: p::VkImageLayout(
                    if first {
                        vk::ImageLayout::UNDEFINED
                    } else {
                        vk::ImageLayout::GENERAL
                    }
                    .as_raw(),
                ),
                new_layout: p::VkImageLayout(
                    if i == 0 {
                        vk::ImageLayout::VIDEO_DECODE_DST_KHR
                    } else {
                        vk::ImageLayout::VIDEO_DECODE_DPB_KHR
                    }
                    .as_raw(),
                ),
                src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                image: p::VkImage(200 + i),
                subresource_range: p::VkImageSubresourceRange {
                    aspect_mask: p::VkImageAspectFlags(vk::ImageAspectFlags::COLOR.as_raw()),
                    level_count: 1,
                    layer_count: 1,
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect::<Vec<_>>();
        wire(rpc.cmd_pipeline_barrier2(p::VkCmdPipelineBarrier2Args {
            command_buffer: p::VkCommandBuffer(cb),
            p_dependency_info: Some(Box::new(p::VkDependencyInfo {
                image_memory_barrier_count: 5,
                p_image_memory_barriers: barriers,
                ..Default::default()
            })),
        }))?;
        Ok(())
    }
    fn record(
        rpc: &mut Rpc,
        cb: u64,
        d: FrameDescriptors,
        reset: bool,
        first: bool,
    ) -> Result<(), String> {
        checked(
            wire(rpc.begin_command_buffer(p::VkBeginCommandBufferArgs {
                command_buffer: p::VkCommandBuffer(cb),
                p_begin_info: Some(Box::new(Default::default())),
            }))?
            .result,
        )?;
        barriers(rpc, cb, first)?;
        wire(
            rpc.cmd_begin_video_coding_khr(p::VkCmdBeginVideoCodingKHRArgs {
                command_buffer: p::VkCommandBuffer(cb),
                p_begin_info: Some(Box::new(d.begin)),
            }),
        )?;
        if reset {
            wire(
                rpc.cmd_control_video_coding_khr(p::VkCmdControlVideoCodingKHRArgs {
                    command_buffer: p::VkCommandBuffer(cb),
                    p_coding_control_info: Some(Box::new(p::VkVideoCodingControlInfoKHR {
                        flags: p::VkVideoCodingControlFlagsKHR(
                            vk::VideoCodingControlFlagsKHR::RESET.as_raw(),
                        ),
                        ..Default::default()
                    })),
                }),
            )?;
        }
        wire(rpc.cmd_decode_video_khr(p::VkCmdDecodeVideoKHRArgs {
            command_buffer: p::VkCommandBuffer(cb),
            p_decode_info: Some(Box::new(d.decode)),
        }))?;
        wire(rpc.cmd_end_video_coding_khr(p::VkCmdEndVideoCodingKHRArgs {
            command_buffer: p::VkCommandBuffer(cb),
            p_end_coding_info: Some(Box::new(Default::default())),
        }))?;
        checked(
            wire(rpc.end_command_buffer(p::VkEndCommandBufferArgs {
                command_buffer: p::VkCommandBuffer(cb),
            }))?
            .result,
        )
    }
    fn submit(rpc: &mut Rpc, cb: u64, sync2: bool) -> Result<(), String> {
        checked(
            wire(rpc.reset_fences(p::VkResetFencesArgs {
                device: p::VkDevice(DEVICE),
                fence_count: 1,
                p_fences: vec![p::VkFence(FENCE)],
            }))?
            .result,
        )?;
        if sync2 {
            checked(
                wire(rpc.queue_submit2(p::VkQueueSubmit2Args {
                    queue: p::VkQueue(QUEUE),
                    submit_count: 1,
                    p_submits: vec![p::VkSubmitInfo2 {
                        command_buffer_info_count: 1,
                        p_command_buffer_infos: vec![p::VkCommandBufferSubmitInfo {
                            command_buffer: p::VkCommandBuffer(cb),
                            device_mask: 1,
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    fence: p::VkFence(FENCE),
                }))?
                .result,
            )?;
        } else {
            checked(
                wire(rpc.queue_submit(p::VkQueueSubmitArgs {
                    queue: p::VkQueue(QUEUE),
                    submit_count: 1,
                    p_submits: vec![p::VkSubmitInfo {
                        command_buffer_count: 1,
                        p_command_buffers: vec![p::VkCommandBuffer(cb)],
                        ..Default::default()
                    }],
                    fence: p::VkFence(FENCE),
                }))?
                .result,
            )?;
        }
        checked(
            wire(rpc.wait_for_fences(p::VkWaitForFencesArgs {
                device: p::VkDevice(DEVICE),
                fence_count: 1,
                p_fences: vec![p::VkFence(FENCE)],
                wait_all: true,
                timeout: 5_000_000_000,
            }))?
            .result,
        )
    }
    fn body(
        shared: Arc<Shared>,
        cases: Vec<Case>,
        images: Vec<Arc<Image>>,
        sync2: bool,
        cancelled: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let check = || {
            if cancelled.load(Ordering::SeqCst) {
                Err("advanced controller cancelled further work".to_string())
            } else {
                Ok(())
            }
        };
        let mut rpc = Forward::new(GpuServer::new(Arc::clone(&shared)));
        let mut ordinal = 0;
        for (case_index, case) in cases.into_iter().enumerate() {
            check()?;
            let session = 500 + case_index as u64 * 2;
            let parameters = session + 1;
            wire_recording::parameters_for(
                &mut rpc,
                shared.driver.queue_family,
                session,
                parameters,
                case.kind.parameter_sets()?,
            )?;
            let assignment = slots(&case.stream.frames)?;
            for (index, frame) in case.stream.frames.iter().enumerate() {
                check()?;
                let cb = 1000 + ordinal as u64;
                let mut d = descriptors(
                    frame,
                    index,
                    &assignment,
                    BASE,
                    DST,
                    SOURCE,
                    session,
                    parameters,
                )?;
                d.decode.src_buffer_offset += case.base as u64;
                record(&mut rpc, cb, d, index == 0, ordinal == 0)?;
                check()?;
                submit(&mut rpc, cb, sync2)?;
                // Runtime success precedes the real user fence and includes
                // callback plus input_released.recv(). Overwrite only this
                // completed original AU before sampling its original targets.
                shared
                    .arena
                    .region
                    .write_bytes(
                        (case.base + frame.range.start) as u64,
                        &vec![0xa5; frame.range.len()],
                    )
                    .map_err(|e| e.to_string())?;
                let _queue = shared.queue_lock.lock().unwrap_or_else(|e| e.into_inner());
                let y = frame
                    .expected_y
                    .ok_or("advanced frame missing fixed pixel oracle")?;
                check()?;
                unsafe {
                    fixture::sample_expected(
                        &shared.driver,
                        images[0].image(),
                        "advanced_destination",
                        Some(y),
                    )?;
                }
                if frame.setup.is_some() {
                    check()?;
                    unsafe {
                        fixture::sample_expected(
                            &shared.driver,
                            images[assignment[index] + 1].image(),
                            "advanced_setup",
                            Some(y),
                        )?;
                    }
                }
                println!(
                    "advanced case={:?} index={index} ordinal={ordinal} range={:?} frame_num={} current_poc={:?} setup={:?} slot={} true_user_fence=true input_release_before_original_au_overwrite=true",
                    case.kind,
                    frame.range,
                    frame.frame_num,
                    frame.poc,
                    frame.setup,
                    assignment[index]
                );
                ordinal += 1;
            }
            check()?;
            wire(rpc.destroy_video_session_parameters_khr(
                p::VkDestroyVideoSessionParametersKHRArgs {
                    device: p::VkDevice(DEVICE),
                    video_session_parameters: p::VkVideoSessionParametersKHR(parameters),
                },
            ))?;
            wire(
                rpc.destroy_video_session_khr(p::VkDestroyVideoSessionKHRArgs {
                    device: p::VkDevice(DEVICE),
                    video_session: p::VkVideoSessionKHR(session),
                }),
            )?;
        }
        checked(
            wire(rpc.queue_wait_idle(p::VkQueueWaitIdleArgs {
                queue: p::VkQueue(QUEUE),
            }))?
            .result,
        )?;
        if let Some(error) = shared.native_video.lock().unwrap().failure() {
            return Err(format!("advanced worker failure {error:?}"));
        }
        if ordinal != 37 {
            return Err("advanced frame count differs".into());
        }
        Ok(())
    }
    pub(crate) fn run(sync2: bool) -> Result<(), String> {
        let mut cases = Vec::new();
        let mut payload = Vec::new();
        for kind in (0..3)
            .flat_map(|ty| {
                [
                    PcmCase::Marking(ty),
                    PcmCase::Gap(ty),
                    PcmCase::Bootstrap(ty),
                ]
            })
            .chain([PcmCase::BSkip])
        {
            let stream = stream::pcm_original(kind)?;
            payload.resize(payload.len().next_multiple_of(256), 0);
            let base = payload.len();
            payload.extend_from_slice(stream.bytes);
            cases.push(Case { stream, base, kind });
        }
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
        state.runtime.lock().unwrap().enable_fixture_staging();
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
        allocate(&mut rpc, &shared, payload.len())?;
        let graph = {
            let objects = shared.objects();
            Graph {
                shared: Arc::clone(&shared),
                images: (0..5)
                    .map(|i| Arc::clone(&objects.native_video.images[&(200 + i)]))
                    .collect(),
                source: objects
                    .get::<vk::Buffer>(HandleType::VkBuffer, SOURCE)
                    .map_err(|e| format!("{e:?}"))?,
                pool: objects
                    .get::<vk::CommandPool>(HandleType::VkCommandPool, POOL)
                    .map_err(|e| format!("{e:?}"))?,
                fence: objects
                    .get::<vk::Fence>(HandleType::VkFence, FENCE)
                    .map_err(|e| format!("{e:?}"))?,
            }
        };
        println!(
            "advanced prepared pid={} source_bytes={} bound_images=5 bound_bytes={} bounded_frames=37 product_gates=false",
            std::process::id(),
            payload.len(),
            graph
                .images
                .iter()
                .map(|i| i.requirements().size)
                .sum::<u64>()
        );
        let (tx, rx) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let worker_images = graph.images.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::Builder::new()
            .name("advanced37-forward".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                let result = body(worker_shared, cases, worker_images, sync2, worker_cancelled);
                let _ = tx.send(result);
            })
            .map_err(|e| e.to_string())?;
        let result = rx
            .recv_timeout(Duration::from_secs(120))
            .map_err(|e| format!("advanced operation incomplete: {e}"))
            .and_then(|r| r);
        if let Err(error) = result {
            cancelled.store(true, Ordering::SeqCst);
            eprintln!(
                "RETAINED advanced37 pid={} bound_images=5 source_bytes={} reason={error}",
                std::process::id(),
                payload.len()
            );
            // Parent retains the complete Shared, runtime, original arena, images,
            // native handles and worker. Never release an uncertain native owner.
            let _ = Box::leak(Box::new((graph, rpc, worker)));
            loop {
                std::thread::park();
            }
        }
        worker
            .join()
            .map_err(|_| "advanced worker panicked after result")?;
        unsafe {
            for view in graph.shared.objects().native_video.views.values() {
                if let Some(native) = view.native {
                    driver.device.destroy_image_view(native, None);
                }
            }
            driver.device.destroy_buffer(graph.source, None);
            driver.device.destroy_fence(graph.fence, None);
            driver.device.destroy_command_pool(graph.pool, None);
            driver.device.free_memory(graph.shared.arena.memory, None);
        }
        println!(
            "advanced37 complete true_native_vt_input_retirement=true sync2={sync2} product_gates=false"
        );
        Ok(())
    }
}
