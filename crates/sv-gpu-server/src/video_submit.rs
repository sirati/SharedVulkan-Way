//! Ordered native Vulkan/VideoToolbox submission on the existing single queue.
//! Ordinary submissions keep their generated path. This executor is used only
//! after the caller has validated a supported video recording and retained its
//! exact source/target allocations. It never performs decode at record time.
use crate::{driver::Driver, native_video::Decode};
use ash::vk;

#[derive(Clone, Copy)]
pub(crate) struct Semaphore {
    pub handle: vk::Semaphore,
    /// Binary semaphore submissions use zero, as required by synchronization2.
    pub value: u64,
    pub stage: vk::PipelineStageFlags2,
}
pub(crate) enum Work<'a> {
    /// An ordered native segment, including its original image barriers.
    Native(vk::CommandBuffer),
    Decode(&'a Decode),
}

/// The queue is idle at this callback, including all earlier submitted waits
/// and native segments. A successful return means decoder and Metal writes to
/// the exact bound destination have completed. The callback must retain its
/// input allocation and every output/reference allocation through completion.
pub(crate) trait DecodeExecutor {
    fn decode_completed(&mut self, job: &Decode) -> Result<(), vk::Result>;
    /// Event metadata commits require proof of the real final native body.
    fn needs_body_completion(&self) -> bool {
        false
    }
    /// Called only after final native fence completion, before user signals.
    fn body_completed(&mut self) -> Result<(), vk::Result> {
        Ok(())
    }
}

/// Run only on the device submission worker, never in the submitting RPC.
/// Execute supported video work in order, between the original wait and signal
/// operations. Each native segment completes before the next external producer
/// starts. Each external producer completes before subsequent native work can
/// acquire/read its target. There is no device-wide idle, frame polling or
/// independent reclamation worker.
///
/// # Safety
/// The caller holds the driver's existing queue_lock throughout this function,
/// passes only live mapped handles from its session, and retains all referenced
/// allocations/recordings until completion. Native segments must preserve their
/// original order and contain no hidden decode commands. Unsupported mixed
/// recording must be refused before calling this function. On failure no user
/// completion signal is submitted; the caller must fail the submission/session
/// and keep resources alive until the driver's teardown retires failed work.
pub(crate) unsafe fn submit(
    driver: &Driver,
    waits: &[Semaphore],
    work: &[Work<'_>],
    signals: &[Semaphore],
    completion: vk::Fence,
    executor: &mut impl DecodeExecutor,
    retained_fences: &mut Vec<vk::Fence>,
) -> Result<(), vk::Result> {
    let waits: Vec<_> = waits
        .iter()
        .map(|s| {
            vk::SemaphoreSubmitInfo::default()
                .semaphore(s.handle)
                .value(s.value)
                // Conservative ordering also covers the external decoder producer.
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        })
        .collect();
    // Waiting on this internal fence observes the real Vulkan waits and all
    // prior queue work before any bytes are exposed to VideoToolbox or Metal.
    unsafe { native_completed(driver, &waits, &[], retained_fences) }?;
    let mut native = Vec::new();
    for item in work {
        match item {
            Work::Native(command) => {
                native.push(vk::CommandBufferSubmitInfo::default().command_buffer(*command))
            }
            Work::Decode(job) => {
                if !native.is_empty() {
                    unsafe { native_completed(driver, &[], &native, retained_fences) }?;
                    native.clear();
                }
                executor.decode_completed(job)?;
            }
        }
    }
    complete_body(executor, &mut native, |commands| unsafe {
        native_completed(driver, &[], commands, retained_fences)
    })?;
    let signals: Vec<_> = signals
        .iter()
        .map(|s| {
            vk::SemaphoreSubmitInfo::default()
                .semaphore(s.handle)
                .value(s.value)
                .stage_mask(
                    if s.stage
                        .intersects(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                    {
                        vk::PipelineStageFlags2::ALL_COMMANDS
                    } else {
                        s.stage
                    },
                )
        })
        .collect();
    let last = vk::SubmitInfo2::default()
        .command_buffer_infos(&native)
        .signal_semaphore_infos(&signals);
    // Without a body-completion hook, final native segments stay asynchronous.
    // With one, this signal-only batch follows the completed body and commit.
    unsafe {
        driver
            .device
            .queue_submit2(driver.queue, &[last], completion)
    }
}

/// Keep the completion proof and metadata commit before constructing user
/// signals. Even an empty final body needs a fence for prior queue prefixes.
fn complete_body<T>(
    executor: &mut impl DecodeExecutor,
    native: &mut Vec<T>,
    complete: impl FnOnce(&[T]) -> Result<(), vk::Result>,
) -> Result<(), vk::Result> {
    if executor.needs_body_completion() {
        complete(native)?;
        native.clear();
        executor.body_completed()?;
    }
    Ok(())
}

unsafe fn native_completed(
    driver: &Driver,
    waits: &[vk::SemaphoreSubmitInfo<'_>],
    commands: &[vk::CommandBufferSubmitInfo<'_>],
    retained_fences: &mut Vec<vk::Fence>,
) -> Result<(), vk::Result> {
    let fence = unsafe {
        driver
            .device
            .create_fence(&vk::FenceCreateInfo::default(), None)
    }?;
    retained_fences.push(fence);
    let batch = vk::SubmitInfo2::default()
        .wait_semaphore_infos(waits)
        .command_buffer_infos(commands);
    let submitted = unsafe { driver.device.queue_submit2(driver.queue, &[batch], fence) };
    #[cfg(all(target_os = "macos", feature = "native-video-fixture"))]
    if submitted.is_ok() && !waits.is_empty() {
        crate::native_video_queue_fixture::native_wait_submitted();
    }
    let result = match submitted {
        Ok(()) => loop {
            if driver.native_submission_failure().is_some() {
                break Err(vk::Result::ERROR_DEVICE_LOST);
            }
            let slice = unsafe { driver.device.wait_for_fences(&[fence], true, 50_000_000) };
            let slice = crate::native_video_wait_result::observed_slice(
                slice,
                driver.native_submission_failure(),
            );
            match slice {
                Err(vk::Result::TIMEOUT) => continue,
                result => break result,
            }
        },
        Err(error) => Err(error),
    };
    if result.is_ok() {
        unsafe { driver.device.destroy_fence(fence, None) };
        retained_fences.retain(|&retained| retained != fence);
    }
    result
}

/// # Safety
/// Called on the device worker under its queue lock; accepted owners remain
/// alive until this fence observes all preceding native commands and signals.
pub(crate) unsafe fn complete(
    driver: &Driver,
    retained_fences: &mut Vec<vk::Fence>,
) -> Result<(), vk::Result> {
    unsafe { native_completed(driver, &[], &[], retained_fences) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    struct Executor {
        required: bool,
        commit: Result<(), vk::Result>,
        order: Rc<RefCell<Vec<&'static str>>>,
    }
    impl DecodeExecutor for Executor {
        fn decode_completed(&mut self, _: &Decode) -> Result<(), vk::Result> {
            Ok(())
        }
        fn needs_body_completion(&self) -> bool {
            self.required
        }
        fn body_completed(&mut self) -> Result<(), vk::Result> {
            self.order.borrow_mut().push("commit");
            self.commit
        }
    }
    #[test]
    fn body_fence_precedes_commit_and_signal_for_nonempty_and_empty_suffixes() {
        for body in [vec![17, 23], vec![]] {
            let order = Rc::new(RefCell::new(Vec::new()));
            let mut executor = Executor {
                required: true,
                commit: Ok(()),
                order: order.clone(),
            };
            let mut native = body.clone();
            complete_body(&mut executor, &mut native, |commands| {
                assert_eq!(commands, body);
                order.borrow_mut().push("fence");
                Ok(())
            })
            .unwrap();
            assert!(native.is_empty());
            order.borrow_mut().push("signal");
            assert_eq!(*order.borrow(), ["fence", "commit", "signal"]);
        }
    }
    #[test]
    fn fence_or_commit_failure_stops_before_signal_and_preserves_pending_body() {
        for fence_failure in [false, true] {
            let order = Rc::new(RefCell::new(Vec::new()));
            let mut executor = Executor {
                required: true,
                commit: Err(vk::Result::ERROR_DEVICE_LOST),
                order: order.clone(),
            };
            let mut native = vec![17];
            let result = complete_body(&mut executor, &mut native, |_| {
                order.borrow_mut().push("fence");
                if fence_failure {
                    Err(vk::Result::ERROR_DEVICE_LOST)
                } else {
                    Ok(())
                }
            });
            if result.is_ok() {
                order.borrow_mut().push("signal");
            }
            assert_eq!(result, Err(vk::Result::ERROR_DEVICE_LOST));
            if fence_failure {
                assert_eq!(native, [17]);
                assert_eq!(*order.borrow(), ["fence"]);
            } else {
                assert!(native.is_empty());
                assert_eq!(*order.borrow(), ["fence", "commit"]);
            }
        }
    }
    #[test]
    fn ordinary_executor_keeps_final_native_body_asynchronous() {
        struct Ordinary;
        impl DecodeExecutor for Ordinary {
            fn decode_completed(&mut self, _: &Decode) -> Result<(), vk::Result> {
                Ok(())
            }
        }
        let mut native = vec![17];
        complete_body(&mut Ordinary, &mut native, |_| {
            panic!("unexpected final body wait")
        })
        .unwrap();
        assert_eq!(native, [17]);
    }
}
