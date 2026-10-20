//! Feature-only controller for the ordinary and wire recording proofs.
//! An error never turns a timeout into retirement. In particular a native
//! completion fence alone does not retire an already started VT producer.
use super::*;
use std::{
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc,
    },
    time::Duration,
};
static FINISHED: OnceLock<Mutex<Option<mpsc::Sender<Result<(), vk::Result>>>>> = OnceLock::new();
pub(super) fn native_job_finished(fence: u64, result: Result<(), vk::Result>) {
    // Prefix/event-only Jobs do not prove the selected decode producer retired.
    if fence != FENCE {
        return;
    }
    if let Some(tx) = FINISHED
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        let _ = tx.send(result);
    }
}
#[derive(Default)]
pub(super) struct Progress {
    pub attempted: AtomicBool,
    pub cancelled: AtomicBool,
    // 0: no HOST signal attempted, 1: attempted, 2: controller closed gate.
    pub signal: AtomicU8,
    pub sampler_live: AtomicBool,
}
impl Progress {
    pub(super) fn check(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err("fixture operation cancelled".into())
        } else {
            Ok(())
        }
    }
}
unsafe fn cleanup(fixture: &Fixture, binary: vk::Semaphore) {
    let driver = &fixture.shared.driver;
    unsafe {
        for view in fixture.shared.objects().native_video.views.values() {
            if let Some(view) = view.native {
                driver.device.destroy_image_view(view, None);
            }
        }
        driver.device.destroy_buffer(fixture.source, None);
        driver.device.free_memory(fixture.shared.arena.memory, None);
        driver.device.destroy_fence(fixture.user_fence, None);
        driver.device.destroy_semaphore(binary, None);
        driver.device.destroy_semaphore(fixture.timeline, None);
        driver.device.destroy_command_pool(fixture.pool, None);
    }
}
pub(super) fn run(
    fixture: Fixture,
    sync2: bool,
    wire_recording: bool,
    job: Decode,
    binary: vk::Semaphore,
) -> Result<(), String> {
    cancellation::resource_report(&fixture, "normal_prepared");
    let (finished_tx, finished_rx) = mpsc::channel();
    *FINISHED
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(finished_tx);
    let progress = Arc::new(Progress::default());
    let body_progress = Arc::clone(&progress);
    let shared = Arc::clone(&fixture.shared);
    let destination = Arc::clone(&fixture.destination);
    let setup = Arc::clone(&fixture.setup);
    let fence = fixture.user_fence;
    let event_prefix = fixture.event_prefix.clone();
    let mixed = fixture.mixed.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let body = std::thread::Builder::new()
        .name("fixture-verification".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            // Forward owns thread-local wire scratch. Construct it on this thread.
            let rpc = sv_gpu_entry::Forward::new(GpuServer::new(Arc::clone(&shared)));
            let result = verification_body(
                rpc,
                sync2,
                shared,
                destination,
                setup,
                job,
                fence,
                body_progress,
                event_prefix,
                mixed,
            );
            let _ = done_tx.send(result);
        })
        .map_err(|e| format!("fixture operation spawn: {e}"))?;
    let mut body_done = false;
    let result = match done_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(result) => {
            body_done = true;
            result
        }
        Err(error) => Err(format!("bounded fixture operation did not finish: {error}")),
    };
    if result.is_err() {
        progress.cancelled.store(true, Ordering::SeqCst);
        // A successful transition proves that the body can never signal later.
        let never_signaled = progress
            .signal
            .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        fixture.shared.cancel_native_submissions();
        // Never race an in-flight original HOST call. If the gate was closed,
        // cancellation's post-native-wait check prevents starting VT on rescue.
        if !body_done && !never_signaled {
            match done_rx.recv_timeout(Duration::from_secs(3)) {
                Ok(_) => body_done = true,
                Err(_) => cancellation::parked(
                    fixture,
                    "original HOST call or operation not quiescent; rescue would race producer input",
                ),
            }
        }
        if progress.attempted.load(Ordering::SeqCst) {
            if let Err(error) = cancellation::signal_original_timeline(&fixture, true) {
                cancellation::parked(fixture, &error);
            }
        }
        if !body_done {
            match done_rx.recv_timeout(Duration::from_secs(8)) {
                Ok(_) => body_done = true,
                Err(_) => cancellation::parked(
                    fixture,
                    "fixture operation still active after diagnostic HOST rescue",
                ),
            }
        }
        if progress.sampler_live.load(Ordering::SeqCst) {
            cancellation::parked(
                fixture,
                "sampler ownership not disposed; retaining its bounded diagnostic allocation",
            );
        }
        if progress.attempted.load(Ordering::SeqCst) {
            // Wait for selected Job disposition before appending a native fence.
            // A quarantine error is not evidence of external producer retirement.
            let finished = finished_rx.recv_timeout(Duration::from_secs(3));
            if finished.is_err() {
                cancellation::parked(fixture, "selected video Job disposition unproven");
            }
            let user_signaled = match cancellation::retire_native(&fixture) {
                Ok(value) => value,
                Err(error) => cancellation::parked(fixture, &error),
            };
            if !never_signaled && finished != Ok(Ok(())) && !user_signaled {
                cancellation::parked(
                    fixture,
                    "native fence completed but started VT producer retirement remains unproven",
                );
            }
        } else if let Some(fence) = fixture.diagnostic_fence {
            unsafe {
                fixture.shared.driver.device.destroy_fence(fence, None);
            }
        }
    } else {
        if let Err(error) = cancellation::retire_native(&fixture) {
            cancellation::parked(fixture, &error);
        }
        println!(
            "queue fixture sync2={sync2} wire_recording={wire_recording} original_arena=true distinct_setup=true future_host_signal=true binary_wait=true user_fence=true accepted_snapshot=true pixels=32"
        );
    }
    // No operation can issue a sampler or queue call after this join.
    if body_done {
        let _ = body.join();
    }
    FINISHED
        .get()
        .unwrap()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(prefix) = &fixture.event_prefix {
        let mut rpc = sv_gpu_entry::Forward::new(GpuServer::new(Arc::clone(&fixture.shared)));
        if let Err(error) = unsafe { prefix.cleanup_after_retirement(&mut rpc) } {
            cancellation::parked(fixture, &format!("retired prefix cleanup failed: {error}"));
        }
    }
    if let Some(mixed) = &fixture.mixed {
        let mut rpc = sv_gpu_entry::Forward::new(GpuServer::new(Arc::clone(&fixture.shared)));
        if let Err(error) = unsafe { mixed.cleanup_after_retirement(&mut rpc) } {
            cancellation::parked(fixture, &format!("retired mixed cleanup failed: {error}"));
        }
    }
    unsafe {
        cleanup(&fixture, binary);
    }
    result
}
