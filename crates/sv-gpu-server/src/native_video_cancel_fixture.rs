//! Feature-only cancellation proof. Rendezvous observe actual native submission
//! and completed healthy wait slices. They never claim native retirement.
//!
//! This fixture uses local SharedRegion::heap backing imported into a real Arena.
//! It has no VM or guest mapping and does not test cross-process source leasing.
//! Calls use direct Forward and the canonical Shared cancellation API; no socket
//! disconnect, ring join or VM teardown is synthesized or claimed.
//! Real bound NV12 owners and the complete original Shared graph remain live.
//! Backing is fixed at 1 MiB plus a precreated 4 KiB cold Shared, with two 64x64
//! NV12 targets. Fixture threads use 256 KiB stacks: two frontend observers, one
//! bounded assertion thread, one canonical executor, and at most one independent
//! host-control thread at a time.
//! Native requirement sizes and actual process RSS are logged. Normal rendezvous and
//! cleanup attempts are bounded. A missed quarantine deadline may use a separate
//! native HOST-control thread to signal only this fixture's own test timeline.
//! If quarantine or the appended native completion fence cannot be proved after
//! that diagnostic rescue, the fixture retains its owners in place and disables process exit. An external controller timeout in
//! that state is a failed proof, not safe teardown. Native launch requires the
//! controller to accept that failure policy within its exclusive target window.
use super::*;
use std::{
    sync::{OnceLock, mpsc},
    time::Duration,
};
type Rpc = sv_gpu_entry::Forward<GpuServer>;
struct WaitObserver {
    entered: mpsc::Sender<vk::Result>,
    resume: mpsc::Receiver<()>,
}
#[derive(Default)]
struct Observers {
    submitted: Option<mpsc::Sender<()>>,
    fence: Option<WaitObserver>,
    semaphore: Option<WaitObserver>,
}
static OBSERVERS: OnceLock<Mutex<Observers>> = OnceLock::new();
fn observers() -> &'static Mutex<Observers> {
    OBSERVERS.get_or_init(|| Mutex::new(Observers::default()))
}
pub(super) fn native_wait_submitted() {
    let observer = observers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .submitted
        .take();
    if let Some(observer) = observer {
        let _ = observer.send(());
    }
}
pub(super) fn frontend_wait_slice(fence: bool, result: vk::Result) {
    let observer = {
        let mut state = observers().lock().unwrap_or_else(|e| e.into_inner());
        if fence {
            state.fence.take()
        } else {
            state.semaphore.take()
        }
    };
    if let Some(observer) = observer {
        let _ = observer.entered.send(result);
        if result == vk::Result::TIMEOUT {
            // Dropping the controller's sender also releases this rendezvous.
            // A controller failure cannot leave our own test hook unbounded.
            let _ = observer.resume.recv_timeout(Duration::from_secs(3));
        }
    }
}
struct Control {
    fence_resume: Option<mpsc::Sender<()>>,
    semaphore_resume: Option<mpsc::Sender<()>>,
}
impl Drop for Control {
    fn drop(&mut self) {
        self.fence_resume.take();
        self.semaphore_resume.take();
        *observers().lock().unwrap_or_else(|e| e.into_inner()) = Observers::default();
    }
}
fn expected(result: p::VkResult, want: vk::Result, stage: &str) -> Result<(), String> {
    if result.0 == want.as_raw() {
        Ok(())
    } else {
        Err(format!("{stage}: expected {want:?}, got {}", result.0))
    }
}
fn fence_wait(timeout: u64) -> p::VkWaitForFencesArgs {
    p::VkWaitForFencesArgs {
        device: p::VkDevice(DEVICE),
        fence_count: 1,
        p_fences: vec![p::VkFence(FENCE)],
        wait_all: true,
        timeout,
    }
}
fn semaphore_wait(timeout: u64) -> p::VkWaitSemaphoresArgs {
    p::VkWaitSemaphoresArgs {
        device: p::VkDevice(DEVICE),
        timeout,
        p_wait_info: Some(Box::new(p::VkSemaphoreWaitInfo {
            semaphore_count: 1,
            p_semaphores: vec![p::VkSemaphore(TIMELINE)],
            p_values: vec![1],
            flags: p::VkSemaphoreWaitFlags(vk::SemaphoreWaitFlags::ANY.as_raw()),
            ..Default::default()
        })),
    }
}
fn submit(rpc: &mut Rpc, sync2: bool) -> Result<(), String> {
    if sync2 {
        checked(
            wire(rpc.queue_submit2(p::VkQueueSubmit2Args {
                queue: p::VkQueue(QUEUE),
                submit_count: 1,
                fence: p::VkFence(FENCE),
                p_submits: vec![p::VkSubmitInfo2 {
                    wait_semaphore_info_count: 1,
                    p_wait_semaphore_infos: vec![p::VkSemaphoreSubmitInfo {
                        semaphore: p::VkSemaphore(TIMELINE),
                        value: 1,
                        stage_mask: p::VkPipelineStageFlags2(
                            vk::PipelineStageFlags2::ALL_COMMANDS.as_raw(),
                        ),
                        ..Default::default()
                    }],
                    command_buffer_info_count: 1,
                    p_command_buffer_infos: vec![p::VkCommandBufferSubmitInfo {
                        command_buffer: p::VkCommandBuffer(COMMAND),
                        device_mask: 1,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            }))?
            .result,
        )
    } else {
        checked(
            wire(rpc.queue_submit(p::VkQueueSubmitArgs {
                queue: p::VkQueue(QUEUE),
                submit_count: 1,
                fence: p::VkFence(FENCE),
                p_submits: vec![p::VkSubmitInfo {
                    p_next: vec![p::PNextLink::VkTimelineSemaphoreSubmitInfo(Box::new(
                        p::VkTimelineSemaphoreSubmitInfo {
                            wait_semaphore_value_count: 1,
                            p_wait_semaphore_values: vec![1],
                            ..Default::default()
                        },
                    ))],
                    wait_semaphore_count: 1,
                    p_wait_semaphores: vec![p::VkSemaphore(TIMELINE)],
                    p_wait_dst_stage_mask: vec![p::VkPipelineStageFlags(
                        vk::PipelineStageFlags::ALL_COMMANDS.as_raw(),
                    )],
                    command_buffer_count: 1,
                    p_command_buffers: vec![p::VkCommandBuffer(COMMAND)],
                    ..Default::default()
                }],
            }))?
            .result,
        )
    }
}
fn assert_failed_calls(rpc: &mut Rpc) -> Result<(), String> {
    let lost = vk::Result::ERROR_DEVICE_LOST;
    expected(
        wire(rpc.get_fence_status(p::VkGetFenceStatusArgs {
            device: p::VkDevice(DEVICE),
            fence: p::VkFence(FENCE),
        }))?
        .result,
        lost,
        "subsequent GetFenceStatus",
    )?;
    expected(
        wire(rpc.wait_for_fences(fence_wait(1_000_000)))?.result,
        lost,
        "subsequent WaitForFences",
    )?;
    let counter = wire(
        rpc.get_semaphore_counter_value(p::VkGetSemaphoreCounterValueArgs {
            device: p::VkDevice(DEVICE),
            semaphore: p::VkSemaphore(TIMELINE),
            p_value: Some(0),
        }),
    )?;
    expected(counter.result, lost, "subsequent GetSemaphoreCounterValue")?;
    if counter.p_value.is_some() {
        return Err("failed counter query fabricated a value".into());
    }
    expected(
        wire(rpc.wait_semaphores(semaphore_wait(1_000_000)))?.result,
        lost,
        "subsequent WaitSemaphores",
    )?;
    expected(
        wire(rpc.queue_wait_idle(p::VkQueueWaitIdleArgs {
            queue: p::VkQueue(QUEUE),
        }))?
        .result,
        lost,
        "subsequent QueueWaitIdle",
    )?;
    expected(
        wire(rpc.device_wait_idle(p::VkDeviceWaitIdleArgs {
            device: p::VkDevice(DEVICE),
        }))?
        .result,
        lost,
        "subsequent DeviceWaitIdle",
    )?;
    expected(
        wire(rpc.queue_submit(p::VkQueueSubmitArgs {
            queue: p::VkQueue(QUEUE),
            submit_count: 1,
            p_submits: vec![Default::default()],
            fence: p::VkFence(0),
        }))?
        .result,
        lost,
        "new QueueSubmit",
    )?;
    expected(
        wire(rpc.queue_submit2(p::VkQueueSubmit2Args {
            queue: p::VkQueue(QUEUE),
            submit_count: 1,
            p_submits: vec![Default::default()],
            fence: p::VkFence(0),
        }))?
        .result,
        lost,
        "new QueueSubmit2",
    )?;
    Ok(())
}
struct PendingAssertions {
    thread: std::thread::JoinHandle<()>,
    done: mpsc::Receiver<Result<(), String>>,
}
fn assert_failed_async(
    shared: Arc<Shared>,
    stage: &str,
    pending: &mut Vec<PendingAssertions>,
) -> Result<(), String> {
    let (done_tx, done) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("fixture-failed-call-check".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut rpc = sv_gpu_entry::Forward::new(GpuServer::new(shared));
            let _ = done_tx.send(assert_failed_calls(&mut rpc));
        })
        .map_err(|e| format!("{stage} assertion spawn: {e}"))?;
    match done.recv_timeout(Duration::from_secs(2)) {
        Ok(result) => {
            let _ = thread.join();
            result.map_err(|e| format!("{stage}: {e}"))
        }
        Err(error) => {
            pending.push(PendingAssertions { thread, done });
            Err(format!(
                "{stage} assertion did not finish within control deadline: {error}"
            ))
        }
    }
}
fn cold_shared(original: &Arc<Shared>) -> Result<Arc<Shared>, String> {
    // Precreated while healthy. This independent small local region is never
    // imported by the driver and is not a guest/VM mapping. Borrowed status
    // handles belong to the same device; the original Fixture retains owners.
    let region = Arc::new(SharedRegion::heap(4096).map_err(|e| e.to_string())?);
    let mut objects = Objects::new();
    {
        let source = original.objects();
        objects.insert(
            DEVICE,
            HandleType::VkDevice,
            source
                .get::<vk::Device>(HandleType::VkDevice, DEVICE)
                .map_err(|e| format!("{e:?}"))?,
        );
        objects.insert(
            QUEUE,
            HandleType::VkQueue,
            source
                .get::<vk::Queue>(HandleType::VkQueue, QUEUE)
                .map_err(|e| format!("{e:?}"))?,
        );
        objects.insert(
            FENCE,
            HandleType::VkFence,
            source
                .get::<vk::Fence>(HandleType::VkFence, FENCE)
                .map_err(|e| format!("{e:?}"))?,
        );
        objects.insert(
            TIMELINE,
            HandleType::VkSemaphore,
            source
                .get::<vk::Semaphore>(HandleType::VkSemaphore, TIMELINE)
                .map_err(|e| format!("{e:?}"))?,
        );
    }
    Ok(Arc::new(Shared {
        driver: Arc::clone(&original.driver),
        fns: Arc::clone(&original.fns),
        arena: arena::map_lazy(region),
        objects: Mutex::new(objects),
        native_video: Mutex::new(DispatchState::default()),
        queue_lock: Arc::clone(&original.queue_lock),
        pool_lock: Arc::clone(&original.pool_lock),
        video_stable: Mutex::new(Default::default()),
        poisoned: Default::default(),
    }))
}
pub(super) fn resource_report(fixture: &Fixture, stage: &str) {
    let pid = std::process::id();
    let rss = std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| output.trim().parse::<u64>().ok());
    eprintln!(
        "fixture resources stage={stage} pid={pid} rss_kib={rss:?} backing=local_heap arena_bytes={} source_bytes={} target_images=2 target_extents=64x64 image_requirement_bytes=[{},{}] fixture_command_pools<=3 fixture_command_buffers<=7 fixture_semaphores=2 fixture_fences<=4 cold_backing_bytes<=4096 frontend_threads<=2 assertion_threads<=1 operation_threads<=1 host_control_threads<=1 executor_threads=1 sampler_jobs<=1 vm_maps=0 guest_maps=0",
        fixture.shared.arena.len,
        fixture.source_bytes,
        fixture.image_requirement_bytes[0],
        fixture.image_requirement_bytes[1]
    );
}
pub(super) fn parked(fixture: Fixture, message: &str) -> ! {
    eprintln!(
        "FAIL native queue fixture: {message}; retirement unproven, retaining exactly one idle diagnostic process and original owner graph; NOT clean bounded teardown"
    );
    resource_report(&fixture, "retained_unproven");
    // No watchdog or process abort may release uncertain native allocations.
    // There is no retrying native work, native decode, allocation growth or busy
    // polling here. The exclusive controller must preserve this failed process.
    let _ = Box::leak(Box::new(fixture));
    loop {
        std::thread::park_timeout(Duration::from_secs(1));
    }
}
pub(super) fn signal_original_timeline(fixture: &Fixture, rescue: bool) -> Result<(), String> {
    let timeline = fixture.timeline;
    let shared = Arc::clone(&fixture.shared);
    let (done_tx, done_rx) = mpsc::channel();
    // A separate real HOST-control thread owns its Shared clone. It does not
    // enter Forward, the cancelled executor, queue_lock, or any wait rendezvous.
    // This is legal test input for this fixture's OWN timeline, not recovery or
    // retirement evidence for product work.
    let control = std::thread::Builder::new()
        .name("fixture-native-host-signal".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let result = (|| {
                if native(unsafe { shared.driver.device.get_semaphore_counter_value(timeline) })?
                    >= 1
                {
                    return Ok(());
                }
                native(unsafe {
                    shared.driver.device.signal_semaphore(
                        &vk::SemaphoreSignalInfo::default()
                            .semaphore(timeline)
                            .value(1),
                    )
                })
            })();
            let _ = done_tx.send(result);
        })
        .map_err(|e| format!("native HOST-control spawn: {e}"))?;
    let result = done_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|e| format!("independent native HOST signal did not finish: {e}"))?;
    result?;
    let _ = control.join();
    println!(
        "cancellation diagnostic_native_host_signal=true own_test_timeline=true rescue={rescue} product_admission_unchanged=true retirement_claim=false"
    );
    Ok(())
}
fn native_retirement_control(
    shared: Arc<Shared>,
    completion: vk::Fence,
    user_fence: vk::Fence,
) -> Result<bool, String> {
    let driver = &shared.driver;
    let start = std::time::Instant::now();
    loop {
        match shared.queue_lock.try_lock() {
            Ok(_queue) => {
                native(unsafe {
                    driver.device.queue_submit2(
                        driver.queue,
                        &[vk::SubmitInfo2::default()],
                        completion,
                    )
                })?;
                break;
            }
            Err(std::sync::TryLockError::Poisoned(error)) => {
                let _queue = error.into_inner();
                native(unsafe {
                    driver.device.queue_submit2(
                        driver.queue,
                        &[vk::SubmitInfo2::default()],
                        completion,
                    )
                })?;
                break;
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if start.elapsed() >= Duration::from_secs(2) {
                    return Err(
                        "native queue remained owned after bounded cancellation/rescue".into(),
                    );
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    native(unsafe {
        driver
            .device
            .wait_for_fences(&[completion], true, 5_000_000_000)
    })?;
    unsafe { driver.device.destroy_fence(completion, None) };
    println!("fixture cleanup appended_native_completion_fence=true native_retirement_proven=true");
    native(unsafe { driver.device.get_fence_status(user_fence) })
}
pub(super) fn retire_native(fixture: &Fixture) -> Result<bool, String> {
    let shared = Arc::clone(&fixture.shared);
    let completion = fixture
        .diagnostic_fence
        .ok_or("missing precreated diagnostic fence")?;
    let user_fence = fixture.user_fence;
    let (done_tx, done_rx) = mpsc::channel();
    let control = std::thread::Builder::new()
        .name("fixture-native-retirement".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let result = native_retirement_control(shared, completion, user_fence);
            let _ = done_tx.send(result);
        })
        .map_err(|e| format!("retirement-control spawn: {e}"))?;
    let result = done_rx
        .recv_timeout(Duration::from_secs(8))
        .map_err(|e| format!("bounded native retirement control did not finish: {e}"))?;
    let _ = control.join();
    result
}
pub(super) fn run(sync2: bool) -> Result<(), String> {
    let mut fixture = wire_recording::prepare()?;
    let cold = cold_shared(&fixture.shared)?;
    fixture.diagnostic_fence = Some(native(unsafe {
        fixture
            .shared
            .driver
            .device
            .create_fence(&vk::FenceCreateInfo::default(), None)
    })?);
    resource_report(&fixture, "prepared");
    let mut pending_assertions = Vec::new();
    let (submitted_tx, submitted_rx) = mpsc::channel();
    let (fence_entered_tx, fence_entered_rx) = mpsc::channel();
    let (semaphore_entered_tx, semaphore_entered_rx) = mpsc::channel();
    let (fence_resume_tx, fence_resume_rx) = mpsc::channel();
    let (semaphore_resume_tx, semaphore_resume_rx) = mpsc::channel();
    *observers().lock().unwrap_or_else(|e| e.into_inner()) = Observers {
        submitted: Some(submitted_tx),
        fence: Some(WaitObserver {
            entered: fence_entered_tx,
            resume: fence_resume_rx,
        }),
        semaphore: Some(WaitObserver {
            entered: semaphore_entered_tx,
            resume: semaphore_resume_rx,
        }),
    };
    let mut control = Control {
        fence_resume: Some(fence_resume_tx),
        semaphore_resume: Some(semaphore_resume_tx),
    };
    let (failed_tx, failed_rx) = mpsc::channel();
    fixture.rpc.host().native_fatal = Some(Arc::new(move |error| {
        let _ = failed_tx.send(error);
    }));
    let mut waiters = Vec::new();
    let mut cancelled = false;
    let mut accepted = false;
    let assertions = (|| -> Result<(), String> {
        submit(&mut fixture.rpc, sync2)?;
        accepted = true;
        // The accepted Job owns its cloned callback. Later refused RPCs must
        // not emit that same callback and pretend the original Job quarantined.
        fixture.rpc.host().native_fatal = None;
        submitted_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|e| format!("native wait submission observer: {e}"))?;
        // The original future timeline remains zero through both first slices.
        // Native decode cannot begin until that real queue wait completes.
        expected(
            wire(fixture.rpc.get_fence_status(p::VkGetFenceStatusArgs {
                device: p::VkDevice(DEVICE),
                fence: p::VkFence(FENCE),
            }))?
            .result,
            vk::Result::NOT_READY,
            "healthy GetFenceStatus",
        )?;
        let counter = wire(fixture.rpc.get_semaphore_counter_value(
            p::VkGetSemaphoreCounterValueArgs {
                device: p::VkDevice(DEVICE),
                semaphore: p::VkSemaphore(TIMELINE),
                p_value: Some(0),
            },
        ))?;
        checked(counter.result)?;
        if counter.p_value != Some(0) {
            return Err("original future timeline changed before cancellation".into());
        }
        for fence in [true, false] {
            let shared = Arc::clone(&fixture.shared);
            let (done_tx, done_rx) = mpsc::channel();
            let thread = std::thread::Builder::new()
                .name(
                    if fence {
                        "fixture-fence-wait"
                    } else {
                        "fixture-semaphore-wait"
                    }
                    .into(),
                )
                .stack_size(256 * 1024)
                .spawn(move || {
                    let mut rpc = sv_gpu_entry::Forward::new(GpuServer::new(shared));
                    let result = if fence {
                        wire(rpc.wait_for_fences(fence_wait(u64::MAX))).map(|v| v.result.0)
                    } else {
                        wire(rpc.wait_semaphores(semaphore_wait(u64::MAX))).map(|v| v.result.0)
                    };
                    let _ = done_tx.send(result);
                })
                .map_err(|e| format!("frontend wait spawn: {e}"))?;
            waiters.push((thread, done_rx));
        }
        for (name, entered) in [
            ("fence", &fence_entered_rx),
            ("semaphore", &semaphore_entered_rx),
        ] {
            let result = entered
                .recv_timeout(Duration::from_secs(2))
                .map_err(|e| format!("{name} healthy wait entry: {e}"))?;
            if result != vk::Result::TIMEOUT {
                return Err(format!("{name} first healthy native slice was {result:?}"));
            }
        }
        fixture.shared.cancel_native_submissions();
        cancelled = true;
        control.fence_resume.take();
        control.semaphore_resume.take();
        for (_, done) in &waiters {
            let result = done
                .recv_timeout(Duration::from_secs(2))
                .map_err(|e| format!("existing frontend wait did not exit: {e}"))??;
            if result != vk::Result::ERROR_DEVICE_LOST.as_raw() {
                return Err(format!(
                    "existing healthy wait returned {result}, expected DEVICE_LOST"
                ));
            }
        }
        assert_failed_async(
            Arc::clone(&fixture.shared),
            "original Shared",
            &mut pending_assertions,
        )?;
        assert_failed_async(
            Arc::clone(&cold),
            "cold Shared readiness=false local_failure=None",
            &mut pending_assertions,
        )?;
        Ok(())
    })();
    if !accepted {
        drop(control);
        return assertions;
    }
    if !cancelled {
        fixture.shared.cancel_native_submissions();
    }
    // Always release either rendezvous, including assertion and mapping errors.
    drop(control);
    // Normal cleanup waits for the original Job's unique quarantine callback.
    // A missed bounded deadline triggers only a separate fixture rescue path.
    let mut rescue = false;
    let quarantined = match failed_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(vk::Result::ERROR_DEVICE_LOST) => true,
        other => {
            eprintln!(
                "FAIL bounded quarantine callback: {other:?}; attempting independent diagnostic rescue"
            );
            rescue = true;
            if let Err(error) = signal_original_timeline(&fixture, true) {
                parked(fixture, &error);
            }
            matches!(
                failed_rx.recv_timeout(Duration::from_secs(2)),
                Ok(vk::Result::ERROR_DEVICE_LOST)
            )
        }
    };
    if !rescue {
        if let Err(error) = signal_original_timeline(&fixture, false) {
            parked(fixture, &error);
        }
    }
    // A regressed idle RPC may be inside a native queue host call. The HOST
    // signal can release it independently; require its return before the
    // controller submits on that queue, preserving host queue synchronization.
    for pending in pending_assertions.drain(..) {
        if pending.done.recv_timeout(Duration::from_secs(2)).is_err() {
            parked(
                fixture,
                "failed-call assertion still holds native queue access after HOST signal",
            );
        }
        let _ = pending.thread.join();
    }
    if let Err(error) = retire_native(&fixture) {
        parked(fixture, &format!("native cleanup fence failed: {error}"));
    }
    if !quarantined {
        parked(
            fixture,
            "native completion observed, but original worker quarantine remains unproven after rescue",
        );
    }
    // A native completion fence has now proven the earlier real GPU wait is
    // retired. An assertion failure may be returned safely without any abort
    // while GPU use is outstanding. Product admission remains failed.
    if fixture.shared.driver.native_submission_failure().is_none()
        || fixture
            .shared
            .native_video
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure()
            .is_none()
    {
        return Err("diagnostic cleanup changed sticky failure state".into());
    }
    for (thread, _) in waiters {
        if thread.is_finished() {
            let _ = thread.join();
        }
        // A faulty unbounded frontend may still be in a native host wait. Its
        // own Shared clone remains live; GPU retirement was proved above.
    }
    assertions?;
    if rescue {
        return Err("diagnostic rescue proved retirement, but bounded cancellation failed; this is not product recovery".into());
    }
    println!(
        "cancellation sync2={sync2} healthy_fence_and_semaphore_entered=true cold_shared_readiness_false_device_lost=true original_native_wait_submitted=true existing_waits_device_lost=true subsequent_calls_device_lost=true new_submissions_refused=true"
    );
    Ok(())
}
