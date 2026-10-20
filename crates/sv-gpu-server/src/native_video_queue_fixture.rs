//! Isolated real-driver queue fixture. Readiness is enabled only on its Shared.
//! Decode metadata is installed manually. This does not prove stock wire
//! session/parameter parsing, product activation, or valid application reset
//! of a pending command buffer. The snapshot probe mutates fixture metadata.
use crate::{
    arena,
    driver::Driver,
    native_video::{
        Configuration, Decode, H264Picture, H264Reference, NativeVideo, Picture, Profile, Ready,
        Reference, Session,
    },
    native_video_dispatch::DispatchState,
    native_video_image::Image,
    native_video_resources::View,
    server::{GpuServer, Shared},
    table::Objects,
};
use ash::vk;
use std::sync::{Arc, Mutex};
use sv_proto::{self as p, HandleType, Server};
use sv_transport::region::SharedRegion;

struct CompletionGuard(bool);
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if self.0 {
            eprintln!("fixture native completion unproven; exiting before owner release");
            std::process::exit(1);
        }
    }
}

const STREAM: &[u8] = include_bytes!("../examples/assets/videotoolbox-h264/progressive-64x50.h264");
const DEVICE: u64 = 1;
const QUEUE: u64 = 2;
const COMMAND: u64 = 3;
const ORDINARY: u64 = 4;
const SOURCE: u64 = 5;
const TIMELINE: u64 = 6;
const BINARY: u64 = 7;
const FENCE: u64 = 8;
const DST: u64 = 9;
const SETUP: u64 = 10;
fn checked(result: p::VkResult) -> Result<(), String> {
    if result.0 == 0 {
        Ok(())
    } else {
        Err(format!("wire result {}", result.0))
    }
}
fn wire<T>(result: sv_wire::Result<T>) -> Result<T, String> {
    result.map_err(|e| format!("{e:?}"))
}
fn native<T>(result: Result<T, vk::Result>) -> Result<T, String> {
    result.map_err(|e| format!("{e:?}"))
}
fn install(state: &mut DispatchState, job: Decode) -> Result<(), String> {
    let plans = &mut state.plans;
    plans.begin(COMMAND).map_err(|e| format!("{e:?}"))?;
    plans.begin_scope(COMMAND).map_err(|e| format!("{e:?}"))?;
    plans.decode(COMMAND, job).map_err(|e| format!("{e:?}"))?;
    plans.end_scope(COMMAND).map_err(|e| format!("{e:?}"))?;
    plans.end(COMMAND).map_err(|e| format!("{e:?}"))?;
    Ok(())
}
fn first_picture() -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), String> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= STREAM.len() {
        let size = if STREAM[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if STREAM[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        starts.push((i, size));
        i += size;
    }
    let mut sps = None;
    let mut pps = None;
    let mut slice = None;
    for (index, &(start, size)) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(STREAM.len(), |s| s.0);
        let payload = &STREAM[start + size..end];
        match payload[0] & 31 {
            7 if sps.is_none() => sps = Some(payload.to_vec()),
            8 if pps.is_none() => pps = Some(payload.to_vec()),
            5 if slice.is_none() => slice = Some(STREAM[start..end].to_vec()),
            _ => {}
        }
    }
    Ok((
        sps.ok_or("no SPS")?,
        pps.ok_or("no PPS")?,
        slice.ok_or("no IDR")?,
    ))
}
/// Run one queue API variant. Requires the static kk_nv12_fixture diagnostic
/// library used by macos-videotoolbox-decode, linked only for this feature.
/// This function owns its isolated Shared and never changes product readiness.
pub fn run(sync2: bool) -> Result<(), String> {
    let (sps, pps, payload) = first_picture()?;
    let driver = Arc::new(Driver::open(0).map_err(|e| e.to_string())?);
    let region = Arc::new(SharedRegion::heap(1 << 20).map_err(|e| e.to_string())?);
    region.write_bytes(0, &payload).map_err(|e| e.to_string())?;
    // The canonical source path reads the original arena. No replacement
    // payload Vec is passed to the decoder or worker.
    let arena = arena::import_whole(&driver, Arc::clone(&region)).map_err(|e| e.to_string())?;
    let mut external =
        vk::ExternalMemoryBufferCreateInfo::default().handle_types(arena.external_handle_type);
    let source_buffer = native(unsafe {
        driver.device.create_buffer(
            &vk::BufferCreateInfo::default()
                .push_next(&mut external)
                .size(payload.len() as u64)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE),
            None,
        )
    })?;
    let requirements = unsafe { driver.device.get_buffer_memory_requirements(source_buffer) };
    if requirements.size > arena.len
        || requirements.memory_type_bits & (1 << arena.memory_type) == 0
    {
        return Err("source buffer incompatible with original arena".into());
    }
    native(unsafe {
        driver
            .device
            .bind_buffer_memory(source_buffer, arena.memory, 0)
    })?;
    let mut state = DispatchState::default();
    state.video = NativeVideo::new(Ready {
        hardware_h264: true,
        joined_nv12_import: true,
        native_queue_bound_decode: true,
    });
    state
        .plans
        .allocated(100, &[COMMAND])
        .map_err(|e| format!("{e:?}"))?;
    let destination = native(Image::create(Arc::clone(&driver), 64, 64))?;
    let setup = native(Image::create(Arc::clone(&driver), 64, 64))?;
    for image in [&destination, &setup] {
        let memory = native(image.allocate())?;
        native(image.bind(memory, 0))?;
        native(image.validate_bound())?;
    }
    assert!(!Arc::ptr_eq(&destination, &setup));
    let picture = |image_view| Picture {
        image_view,
        layer: 0,
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 64,
            height: 64,
        },
    };
    let job = Decode {
        begin_before: true,
        reset_before: true,
        begin_references: Arc::from([]),
        h264: H264Picture {
            idr_pic_flag: true,
            field_pic_flag: false,
            bottom_field_flag: false,
            is_reference: true,
            is_intra: true,
            frame_num: 0,
            pic_order_cnt: [0, 0],
            idr_pic_id: 0,
            sps_id: 0,
            pps_id: 0,
        },
        session: Arc::new(Session {
            id: 1,
            profile: Profile { idc: 100 },
            extent: vk::Extent2D {
                width: 64,
                height: 64,
            },
            dpb_slots: 16,
            references: 16,
        }),
        configuration: Configuration {
            sps: Arc::from(sps),
            pps: Arc::from(pps),
        },
        source_buffer: SOURCE,
        source_offset: 0,
        source_range: payload.len() as u64,
        slice_offsets: Arc::from([0]),
        destination: picture(DST),
        setup: Some(Reference {
            slot: 0,
            picture: picture(SETUP),
            h264: H264Reference {
                frame_num: 0,
                pic_order_cnt: [0, 0],
                long_term: false,
            },
        }),
        references: Arc::from([]),
    };
    install(&mut state, job.clone())?;
    let pool = native(unsafe {
        driver.device.create_command_pool(
            &vk::CommandPoolCreateInfo::default().queue_family_index(driver.queue_family),
            None,
        )
    })?;
    let command = native(unsafe {
        driver.device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .command_buffer_count(1),
        )
    })?[0];
    native(unsafe {
        driver
            .device
            .begin_command_buffer(command, &vk::CommandBufferBeginInfo::default())
    })?;
    let barriers: Vec<_> = [&destination, &setup]
        .iter()
        .map(|image| {
            vk::ImageMemoryBarrier::default()
                .image(image.image())
                .old_layout(vk::ImageLayout::PREINITIALIZED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )
        })
        .collect();
    unsafe {
        driver.device.cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barriers,
        )
    };
    native(unsafe { driver.device.end_command_buffer(command) })?;
    let mut timeline_type =
        vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
    let timeline = native(unsafe {
        driver.device.create_semaphore(
            &vk::SemaphoreCreateInfo::default().push_next(&mut timeline_type),
            None,
        )
    })?;
    let binary = native(unsafe {
        driver
            .device
            .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
    })?;
    let fence = native(unsafe {
        driver
            .device
            .create_fence(&vk::FenceCreateInfo::default(), None)
    })?;
    let mut objects = Objects::new();
    objects.set_arena(arena.memory, arena.len);
    objects.insert(SOURCE, HandleType::VkBuffer, source_buffer);
    objects.insert(DEVICE, HandleType::VkDevice, driver.device.handle());
    objects.insert(QUEUE, HandleType::VkQueue, driver.queue);
    objects.insert(ORDINARY, HandleType::VkCommandBuffer, command);
    objects.insert(TIMELINE, HandleType::VkSemaphore, timeline);
    objects.insert(BINARY, HandleType::VkSemaphore, binary);
    objects.insert(FENCE, HandleType::VkFence, fence);
    for (id, image) in [(DST, &destination), (SETUP, &setup)] {
        let view = native(unsafe {
            driver.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image.image())
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        })?;
        objects.insert(id, HandleType::VkImageView, view);
        objects.native_video.views.insert(
            id,
            View {
                image: Arc::clone(image),
                layer: 0,
                native: Some(view),
            },
        );
    }
    objects.native_video.sources.created(
        SOURCE,
        payload.len() as u64,
        vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR,
    );
    objects
        .native_video
        .sources
        .bound(SOURCE, crate::ARENA_MEMORY_ID, 0);
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
    let mut rpc = sv_gpu_entry::Forward::new(GpuServer::new(Arc::clone(&shared)));
    let mut completion = CompletionGuard(true);
    // Earlier ordinary native work signals the binary once. It also supplies
    // the actual initial layout transitions before video ownership transfers.
    checked(
        wire(rpc.queue_submit(p::VkQueueSubmitArgs {
            queue: p::VkQueue(QUEUE),
            submit_count: 1,
            p_submits: vec![p::VkSubmitInfo {
                command_buffer_count: 1,
                p_command_buffers: vec![p::VkCommandBuffer(ORDINARY)],
                signal_semaphore_count: 1,
                p_signal_semaphores: vec![p::VkSemaphore(BINARY)],
                ..Default::default()
            }],
            fence: p::VkFence(0),
        }))?
        .result,
    )?;
    if sync2 {
        let semaphore = |id, value| p::VkSemaphoreSubmitInfo {
            semaphore: p::VkSemaphore(id),
            value,
            stage_mask: p::VkPipelineStageFlags2(vk::PipelineStageFlags2::ALL_COMMANDS.as_raw()),
            ..Default::default()
        };
        checked(
            wire(rpc.queue_submit2(p::VkQueueSubmit2Args {
                queue: p::VkQueue(QUEUE),
                submit_count: 1,
                p_submits: vec![p::VkSubmitInfo2 {
                    wait_semaphore_info_count: 2,
                    p_wait_semaphore_infos: vec![semaphore(TIMELINE, 1), semaphore(BINARY, 0)],
                    command_buffer_info_count: 1,
                    p_command_buffer_infos: vec![p::VkCommandBufferSubmitInfo {
                        command_buffer: p::VkCommandBuffer(COMMAND),
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
                    p_next: vec![p::PNextLink::VkTimelineSemaphoreSubmitInfo(Box::new(
                        p::VkTimelineSemaphoreSubmitInfo {
                            wait_semaphore_value_count: 2,
                            p_wait_semaphore_values: vec![1, 0],
                            ..Default::default()
                        },
                    ))],
                    wait_semaphore_count: 2,
                    p_wait_semaphores: vec![p::VkSemaphore(TIMELINE), p::VkSemaphore(BINARY)],
                    p_wait_dst_stage_mask: vec![
                        p::VkPipelineStageFlags(
                            vk::PipelineStageFlags::ALL_COMMANDS.as_raw()
                        );
                        2
                    ],
                    command_buffer_count: 1,
                    p_command_buffers: vec![p::VkCommandBuffer(COMMAND)],
                    ..Default::default()
                }],
                fence: p::VkFence(FENCE),
            }))?
            .result,
        )?;
    }
    // Submit has returned while HOST timeline remains zero. Prove it cannot
    // have signaled the final user fence before the future HOST signal.
    if native(unsafe { driver.device.get_fence_status(fence) })? {
        return Err("user fence signaled before future HOST timeline".into());
    }
    {
        let mut state = shared.native_video.lock().unwrap();
        // Test-only metadata replacement, not a legal app reset-pending claim.
        // Accepted Submission owns its old immutable Arc, resources and source.
        state.plans.reset(COMMAND);
        let mut replacement = job;
        replacement.source_buffer = u64::MAX;
        install(&mut state, replacement)?;
    }
    checked(
        wire(rpc.signal_semaphore(p::VkSignalSemaphoreArgs {
            device: p::VkDevice(DEVICE),
            p_signal_info: Some(Box::new(p::VkSemaphoreSignalInfo {
                semaphore: p::VkSemaphore(TIMELINE),
                value: 1,
                ..Default::default()
            })),
        }))?
        .result,
    )?;
    native(unsafe { driver.device.wait_for_fences(&[fence], true, 5_000_000_000) })?;
    // The user fence follows decode and ownership acquire. Sampling these exact
    // images must observe both writes, without copying into a different target.
    for (role, image) in [("destination", &destination), ("setup", &setup)] {
        let _queue = shared.queue_lock.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { sample(&driver, image.image(), role) }?;
    }
    checked(
        wire(rpc.queue_wait_idle(p::VkQueueWaitIdleArgs {
            queue: p::VkQueue(QUEUE),
        }))?
        .result,
    )?;
    if let Some(error) = shared.native_video.lock().unwrap().failure() {
        return Err(format!("worker failure {error:?}"));
    }
    completion.0 = false;
    println!(
        "queue fixture sync2={sync2} original_arena=true distinct_setup=true future_host_signal=true binary_wait=true user_fence=true accepted_snapshot=true pixels=32"
    );
    // All native work is retired before these fixture-only handles are freed.
    unsafe {
        for view in shared.objects().native_video.views.values() {
            if let Some(view) = view.native {
                driver.device.destroy_image_view(view, None);
            }
        }
        driver.device.destroy_buffer(source_buffer, None);
        driver.device.free_memory(shared.arena.memory, None);
        driver.device.destroy_fence(fence, None);
        driver.device.destroy_semaphore(binary, None);
        driver.device.destroy_semaphore(timeline, None);
        driver.device.destroy_command_pool(pool, None);
    }
    Ok(())
}

// Same ABI and diagnostic shader as examples/support/vt_joined_sample.rs.
// Linked only when the explicit native-video-fixture feature is built.
#[repr(C)]
struct Info {
    get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
    device: vk::Device,
    queue: vk::Queue,
    queue_family: u32,
    src_queue_family: u32,
    dst_queue_family: u32,
    memory_properties: *const vk::PhysicalDeviceMemoryProperties,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    src_stage: vk::PipelineStageFlags,
    src_access: vk::AccessFlags,
    model: vk::SamplerYcbcrModelConversion,
    range: vk::SamplerYcbcrRange,
    x_chroma: vk::ChromaLocation,
    y_chroma: vk::ChromaLocation,
}
#[link(name = "kk_nv12_fixture", kind = "static")]
unsafe extern "C" {
    fn kk_nv12_fixture_start(info: *const Info, job: *mut *mut std::ffi::c_void) -> vk::Result;
    fn kk_nv12_fixture_read(job: *mut std::ffi::c_void, timeout: u64, rgba: *mut f32)
    -> vk::Result;
    fn kk_nv12_fixture_destroy(job: *mut std::ffi::c_void) -> vk::Result;
}
unsafe fn sample(driver: &Driver, image: vk::Image, role: &str) -> Result<(), String> {
    let info = Info {
        get_device_proc_addr: driver.instance.fp_v1_0().get_device_proc_addr,
        device: driver.device.handle(),
        queue: driver.queue,
        queue_family: driver.queue_family,
        src_queue_family: vk::QUEUE_FAMILY_IGNORED,
        dst_queue_family: vk::QUEUE_FAMILY_IGNORED,
        memory_properties: &driver.mem_props,
        image,
        old_layout: vk::ImageLayout::GENERAL,
        src_stage: vk::PipelineStageFlags::ALL_COMMANDS,
        src_access: vk::AccessFlags::MEMORY_WRITE,
        model: vk::SamplerYcbcrModelConversion::YCBCR_601,
        range: vk::SamplerYcbcrRange::ITU_NARROW,
        x_chroma: vk::ChromaLocation::MIDPOINT,
        y_chroma: vk::ChromaLocation::MIDPOINT,
    };
    let mut job = std::ptr::null_mut();
    let result = unsafe { kk_nv12_fixture_start(&info, &mut job) };
    if result != vk::Result::SUCCESS {
        return Err(format!("{role} sample submit: {result:?}"));
    }
    let mut rgba = [0.0f32; 64];
    let result = unsafe { kk_nv12_fixture_read(job, 5_000_000_000, rgba.as_mut_ptr()) };
    if result != vk::Result::SUCCESS {
        return Err(format!("{role} sample fence: {result:?}"));
    }
    let result = unsafe { kk_nv12_fixture_destroy(job) };
    if result != vk::Result::SUCCESS {
        return Err(format!("{role} sample destroy: {result:?}"));
    }
    let mut max_error = 0.0f32;
    for row in 0..4 {
        for col in 0..4 {
            let x = (col * 2 + 1) * 64 / 8;
            let y = (row * 2 + 1) * 64 / 8;
            let luma = (40 + (x / 16) * 8 + (y.min(49) / 10) * 6) as f32;
            let luma = (luma - 16.0) / 219.0;
            let cb = (64.0 - 128.0) / 224.0;
            let cr = (192.0 - 128.0) / 224.0;
            let expected = [
                luma + 1.402 * cr,
                luma - 0.344136 * cb - 0.714136 * cr,
                luma + 1.772 * cb,
                1.0,
            ];
            for channel in 0..4 {
                let actual = rgba[(row * 4 + col) * 4 + channel];
                if !actual.is_finite() {
                    return Err(format!("{role} nonfinite sample"));
                }
                max_error = max_error.max((actual - expected[channel]).abs());
            }
        }
    }
    if max_error > 0.025 {
        return Err(format!("{role} max RGB error {max_error} exceeds 0.025"));
    }
    println!("{role} actual_joined_image={image:?} sampled_pixels=16 max_rgb_error={max_error}");
    Ok(())
}
