//! Typed wire submissions owned by the device executor. Wire metadata is Send;
//! Raise pools stay on the executing thread. Nothing here copies pixel payload.
use crate::{
    server::{GpuServer, Shared},
    video_worker,
};
use ash::vk;
use std::sync::{Arc, mpsc};
use sv_proto::{self as p, Server};

pub(crate) type Fatal = Arc<dyn Fn(vk::Result) + Send + Sync>;
enum Call {
    Submit(p::VkQueueSubmitArgs),
    Submit2(p::VkQueueSubmit2Args),
    QueueIdle(p::VkQueueWaitIdleArgs),
    DeviceIdle(p::VkDeviceWaitIdleArgs),
    RetireSession(u64),
}
struct Job {
    shared: Arc<Shared>,
    call: Option<Call>,
    fatal: Option<Fatal>,
    done: Option<mpsc::Sender<Result<(), vk::Result>>>,
    video: Option<crate::native_video_queue::Submission>,
    native_started: bool,
    ordinary_fence: Option<vk::Fence>,
    ordinary_queue: Option<vk::Queue>,
    ordinary_retired: bool,
}
impl video_worker::Submission for Job {
    fn execute(&mut self) -> Result<(), vk::Result> {
        if let Some(error) = self
            .shared
            .native_video
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure()
        {
            let _ = error;
            return Err(vk::Result::ERROR_DEVICE_LOST);
        }
        if let Some(Call::RetireSession(id)) = self.call.as_ref() {
            let runtime = Arc::clone(
                &self
                    .shared
                    .native_video
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .runtime,
            );
            runtime
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove_session(*id);
            self.call.take();
            return Ok(());
        }
        self.native_started = true;
        if let Some(video) = self.video.as_mut() {
            return unsafe { video.execute() };
        }
        let mut host = GpuServer::new(Arc::clone(&self.shared));
        host.native_queue_worker = true;
        let mut forward = sv_gpu_entry::Forward::new(host);
        let asynchronous = matches!(self.call, Some(Call::Submit(_) | Call::Submit2(_)));
        let result = match self.call.take().ok_or(vk::Result::ERROR_DEVICE_LOST)? {
            Call::Submit(args) => forward.queue_submit(args).map(|r| r.result.0),
            Call::Submit2(args) => forward.queue_submit2(args).map(|r| r.result.0),
            Call::QueueIdle(args) => forward.queue_wait_idle(args).map(|r| r.result.0),
            Call::DeviceIdle(args) => forward.device_wait_idle(args).map(|r| r.result.0),
            Call::RetireSession(_) => unreachable!("session cleanup runs before native dispatch"),
        }
        .map_err(|_| vk::Result::ERROR_DEVICE_LOST)?;
        if result == 0 {
            if asynchronous {
                let _queue = self
                    .shared
                    .queue_lock
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let device = &self.shared.driver.device;
                let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;
                self.ordinary_fence = Some(fence);
                // Retain successful ordinary submits asynchronously. An appended
                // fence proves native retirement without waiting on graphics.
                unsafe {
                    device.queue_submit2(
                        self.ordinary_queue
                            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?,
                        &[vk::SubmitInfo2::default()],
                        fence,
                    )
                }?;
            }
            Ok(())
        } else {
            Err(vk::Result::from_raw(result))
        }
    }
    fn retire_failed(&mut self) -> bool {
        // A rejected job never reached native dispatch and may release owners.
        // Once native work started, an error or cancellation is not retirement.
        // The canonical executor quarantines this entire Job instead of freeing
        // uncertain resources, waiting idle behind a future signal, or aborting.
        !self.native_started
    }

    fn completed(&mut self) -> bool {
        let Some(fence) = self.ordinary_fence else {
            return true;
        };
        match unsafe { self.shared.driver.device.get_fence_status(fence) } {
            Ok(true) | Err(vk::Result::ERROR_DEVICE_LOST) => {
                self.ordinary_retired = true;
                true
            }
            _ => false,
        }
    }

    fn finished(&mut self, result: Result<(), vk::Result>) {
        if let Err(error) = result {
            self.shared
                .native_video
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .fail(error);
            if let Some(fatal) = &self.fatal {
                fatal(error);
            }
        }
        if let Some(done) = self.done.take() {
            let _ = done.send(result);
        }
    }
}
impl GpuServer {
    fn enqueue_native(&self, call: Call, wait: bool) -> Result<(), vk::Result> {
        if self.shared.driver.native_submission_failure().is_some() {
            return Err(vk::Result::ERROR_DEVICE_LOST);
        }
        let batches = {
            let state = self
                .shared
                .native_video
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(error) = state.failure() {
                let _ = error;
                return Err(vk::Result::ERROR_DEVICE_LOST);
            }
            match &call {
                Call::Submit(args) => {
                    crate::native_video_submits::legacy(args, |wire| state.plans.is_video(wire))
                }
                Call::Submit2(args) => {
                    crate::native_video_submits::sync2(args, |wire| state.plans.is_video(wire))
                }
                _ => Ok(None),
            }
        }
        .map_err(|_| vk::Result::ERROR_INITIALIZATION_FAILED)?;
        // Freeze recordings, parameters, original mapping ranges and exact
        // target allocations before QueueSubmit reports acceptance. Preparation
        // records native barriers but never waits or reads compressed payload.
        let video = batches
            .map(|batches| {
                crate::native_video_queue::Submission::prepare(Arc::clone(&self.shared), batches)
            })
            .transpose()?;
        let (done, receiver) = if wait {
            let (sender, receiver) = mpsc::channel();
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        let ordinary_queue = match &call {
            Call::Submit(args) => Some(args.queue.0),
            Call::Submit2(args) => Some(args.queue.0),
            _ => None,
        }
        .map(|wire| {
            self.shared
                .objects()
                .get(sv_proto::HandleType::VkQueue, wire)
                .map_err(|_| vk::Result::ERROR_INITIALIZATION_FAILED)
        })
        .transpose()?;
        let job = Job {
            shared: Arc::clone(&self.shared),
            call: Some(call),
            fatal: self.native_fatal.clone(),
            done,
            video,
            native_started: false,
            ordinary_fence: None,
            ordinary_queue,
            ordinary_retired: false,
        };
        self.shared
            .driver
            .native_submission_queue()?
            .enqueue(Box::new(job))?;
        if let Some(receiver) = receiver {
            loop {
                match receiver.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(result) => break result,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        break Err(vk::Result::ERROR_DEVICE_LOST);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if self.shared.driver.native_submission_failure().is_some() {
                            break Err(vk::Result::ERROR_DEVICE_LOST);
                        }
                    }
                }
            }
        } else {
            Ok(())
        }
    }
    pub(crate) fn native_queue_retire_session(&self, id: u64) -> sv_wire::Result<()> {
        self.enqueue_native(Call::RetireSession(id), false)
            .map_err(|error| {
                self.shared
                    .native_video
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .fail(error);
                if let Some(fatal) = &self.native_fatal {
                    fatal(error);
                }
                sv_wire::WireError::UnknownCommand { opcode: 0 }
            })
    }
    fn native_queue_enabled(&self) -> bool {
        if self.shared.driver.native_submission_failure().is_some() { return true; }
        let state = self.shared.native_video.lock().unwrap_or_else(|e| e.into_inner());
        state.failure().is_some() || state.video.available()
    }
    pub(crate) fn native_queue_submit(
        &self,
        a: &p::VkQueueSubmitArgs,
    ) -> Option<sv_wire::Result<p::VkQueueSubmitReply>> {
        self.native_queue_enabled().then(|| {
            Ok(p::VkQueueSubmitReply {
                result: p::VkResult(
                    self.enqueue_native(Call::Submit(a.clone()), false)
                        .err()
                        .map_or(0, |e| e.as_raw()),
                ),
            })
        })
    }
    pub(crate) fn native_queue_submit2(
        &self,
        a: &p::VkQueueSubmit2Args,
    ) -> Option<sv_wire::Result<p::VkQueueSubmit2Reply>> {
        self.native_queue_enabled().then(|| {
            Ok(p::VkQueueSubmit2Reply {
                result: p::VkResult(
                    self.enqueue_native(Call::Submit2(a.clone()), false)
                        .err()
                        .map_or(0, |e| e.as_raw()),
                ),
            })
        })
    }
    pub(crate) fn native_queue_idle(
        &self,
        a: &p::VkQueueWaitIdleArgs,
    ) -> Option<sv_wire::Result<p::VkQueueWaitIdleReply>> {
        self.native_queue_enabled().then(|| {
            Ok(p::VkQueueWaitIdleReply {
                result: p::VkResult(
                    self.enqueue_native(Call::QueueIdle(a.clone()), true)
                        .err()
                        .map_or(0, |e| e.as_raw()),
                ),
            })
        })
    }
    pub(crate) fn native_device_idle(
        &self,
        a: &p::VkDeviceWaitIdleArgs,
    ) -> Option<sv_wire::Result<p::VkDeviceWaitIdleReply>> {
        self.native_queue_enabled().then(|| {
            Ok(p::VkDeviceWaitIdleReply {
                result: p::VkResult(
                    self.enqueue_native(Call::DeviceIdle(a.clone()), true)
                        .err()
                        .map_or(0, |e| e.as_raw()),
                ),
            })
        })
    }
}

impl Shared {
    /// Cancellation changes admission/control only. It does not retire native
    /// GPU ownership or release any mapping, allocation, or driver object.
    pub(crate) fn cancel_native_submissions(&self) {
        let cancelled = self.driver.cancel_native_submissions();
        let mut state = self.native_video.lock().unwrap_or_else(|e| e.into_inner());
        // A wait may precede the first QueueSubmit and hence queue creation.
        // Latch local cancellation so that such a wait also returns to control.
        if cancelled || state.video.available() {
            state.fail(vk::Result::ERROR_DEVICE_LOST);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if let Some(fence) = self.ordinary_fence {
            assert!(
                self.ordinary_retired,
                "submitted native fence requires retirement or quarantine"
            );
            unsafe { self.shared.driver.device.destroy_fence(fence, None) };
        }
    }
}
