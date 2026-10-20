//! Failed native execution is served-device loss, even when an unsignaled
//! native fence or semaphore would otherwise report NOT_READY or wait forever.
use ash::vk;
use sv_proto as p;

pub(super) trait LostReply: Default {
    fn set_result(&mut self, result: p::VkResult);
}
macro_rules! replies {
    ($($reply:ty),* $(,)?) => {$(
        impl LostReply for $reply {
            fn set_result(&mut self,result:p::VkResult) { self.result=result; }
        }
    )*};
}
replies!(
    p::VkGetFenceStatusReply,
    p::VkWaitForFencesReply,
    p::VkGetSemaphoreCounterValueReply,
    p::VkWaitSemaphoresReply
);

/// None leaves every healthy ordinary call on its existing forwarding path.
/// Error replies contain no fabricated output values and require no native call.
fn failed<T: LostReply>(
    driver_failure: Option<vk::Result>,
    session_failure: Option<vk::Result>,
) -> Option<sv_wire::Result<T>> {
    driver_failure.or(session_failure).map(|_| {
        let mut reply = T::default();
        reply.set_result(p::VkResult(vk::Result::ERROR_DEVICE_LOST.as_raw()));
        Ok(reply)
    })
}
#[cfg(target_os = "macos")]
impl crate::server::GpuServer {
    pub(super) fn native_failed_status<T: LostReply>(&self) -> Option<sv_wire::Result<T>> {
        let driver_failure = self.shared.driver.native_submission_failure();
        let session_failure = self
            .shared
            .native_video
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure();
        failed(driver_failure, session_failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn healthy_status_and_wait_calls_keep_the_native_forwarding_path() {
        assert!(failed::<p::VkGetFenceStatusReply>(None, None).is_none());
        assert!(failed::<p::VkWaitForFencesReply>(None, None).is_none());
        assert!(failed::<p::VkGetSemaphoreCounterValueReply>(None, None).is_none());
        assert!(failed::<p::VkWaitSemaphoresReply>(None, None).is_none());
    }
    #[test]
    fn every_latched_failure_is_device_lost_without_output_values() {
        for cause in [
            vk::Result::ERROR_DEVICE_LOST,
            vk::Result::ERROR_INITIALIZATION_FAILED,
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        ] {
            for (driver, session) in [(Some(cause), None), (None, Some(cause))] {
                let fence = failed::<p::VkGetFenceStatusReply>(driver, session)
                    .unwrap()
                    .unwrap();
                let fences = failed::<p::VkWaitForFencesReply>(driver, session)
                    .unwrap()
                    .unwrap();
                let counter = failed::<p::VkGetSemaphoreCounterValueReply>(driver, session)
                    .unwrap()
                    .unwrap();
                let semaphores = failed::<p::VkWaitSemaphoresReply>(driver, session)
                    .unwrap()
                    .unwrap();
                for result in [
                    fence.result,
                    fences.result,
                    counter.result,
                    semaphores.result,
                ] {
                    assert_eq!(result.0, vk::Result::ERROR_DEVICE_LOST.as_raw());
                }
                assert_eq!(counter.p_value, None);
            }
        }
    }
}

const WAIT_SLICE_NS: u64 = 50_000_000;
/// A TIMEOUT is only an observation. This loop never retires or destroys work.
fn bounded_wait(
    timeout: u64,
    mut elapsed: impl FnMut() -> u64,
    mut failure: impl FnMut() -> bool,
    mut wait: impl FnMut(u64) -> vk::Result,
) -> vk::Result {
    let mut first = true;
    loop {
        if failure() {
            return vk::Result::ERROR_DEVICE_LOST;
        }
        let elapsed = elapsed();
        if !first && timeout != u64::MAX && elapsed >= timeout {
            return vk::Result::TIMEOUT;
        }
        let remaining = if timeout == u64::MAX {
            u64::MAX
        } else {
            timeout.saturating_sub(elapsed)
        };
        let result = wait(remaining.min(WAIT_SLICE_NS));
        if failure() {
            return vk::Result::ERROR_DEVICE_LOST;
        }
        if result != vk::Result::TIMEOUT || timeout == 0 {
            return result;
        }
        first = false;
    }
}
#[cfg(target_os = "macos")]
fn failed_shared(shared: &crate::server::Shared) -> bool {
    let driver = shared.driver.native_submission_failure();
    let local = shared
        .native_video
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .failure();
    driver.is_some() || local.is_some()
}
#[cfg(target_os = "macos")]
fn elapsed_ns(start: std::time::Instant) -> u64 {
    start.elapsed().as_nanos().min(u64::MAX as u128) as u64
}
#[cfg(target_os = "macos")]
impl crate::server::GpuServer {
    fn native_wait_interception(&self) -> bool {
        self.shared
            .native_video
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .video
            .available()
    }
    pub(super) fn native_wait_for_fences(
        &mut self,
        a: &p::VkWaitForFencesArgs,
    ) -> Option<sv_wire::Result<p::VkWaitForFencesReply>> {
        use sv_gpu_entry::GpuHost;
        if let Some(reply) = self.native_failed_status() {
            return Some(reply);
        }
        if !self.native_wait_interception() {
            return None;
        }
        // A missing function cannot block. Keep the ordinary missing-entrypoint
        // diagnostic and result rather than manufacturing another path.
        let native = self.shared.fns.vk_wait_for_fences?;
        let shared = std::sync::Arc::clone(&self.shared);
        Some((|| {
            if a.fence_count as usize != a.p_fences.len() {
                return Err(sv_wire::WireError::Truncated {
                    need: a.fence_count as usize,
                    have: a.p_fences.len(),
                });
            }
            let device: sv_gpu_entry::VkDevice =
                self.map_handle(p::HandleType::VkDevice, a.device.0)? as usize as _;
            let handles: Vec<sv_gpu_entry::VkFence> = a
                .p_fences
                .iter()
                .map(|f| self.map_handle(p::HandleType::VkFence, f.0))
                .collect::<sv_wire::Result<_>>()?;
            let wait_all = if a.wait_all { 1 } else { 0 };
            let start = std::time::Instant::now();
            // Exact handles and Shared outlive every native call. No table,
            // queue or native-video lock remains held while the driver waits.
            let result = bounded_wait(
                a.timeout,
                || elapsed_ns(start),
                || failed_shared(&shared),
                |slice| {
                    let result = vk::Result::from_raw(
                        unsafe { native(device, a.fence_count, handles.as_ptr(), wait_all, slice) }
                            .0,
                    );
                    #[cfg(all(target_os = "macos", feature = "native-video-fixture"))]
                    crate::native_video_queue_fixture::frontend_wait_slice(true, result);
                    result
                },
            );
            Ok(p::VkWaitForFencesReply {
                result: p::VkResult(result.as_raw()),
            })
        })())
    }
    pub(super) fn native_wait_semaphores(
        &mut self,
        a: &p::VkWaitSemaphoresArgs,
    ) -> Option<sv_wire::Result<p::VkWaitSemaphoresReply>> {
        use sv_gpu_entry::GpuHost;
        if let Some(reply) = self.native_failed_status() {
            return Some(reply);
        }
        if !self.native_wait_interception() {
            return None;
        }
        let native = self.shared.fns.vk_wait_semaphores?;
        let shared = std::sync::Arc::clone(&self.shared);
        Some((|| {
            let device: sv_gpu_entry::VkDevice =
                self.map_handle(p::HandleType::VkDevice, a.device.0)? as usize as _;
            let mut info = a
                .p_wait_info
                .as_deref()
                .cloned()
                .ok_or(sv_wire::WireError::Truncated { need: 1, have: 0 })?;
            for actual in [info.p_semaphores.len(), info.p_values.len()] {
                if info.semaphore_count as usize != actual {
                    return Err(sv_wire::WireError::Truncated {
                        need: info.semaphore_count as usize,
                        have: actual,
                    });
                }
            }
            {
                let mut mapping = sv_gpu_entry::Mapping(&mut *self);
                // Map once, including any supported extension chain. Raise
                // retains the exact arrays, values, flags and chain for all slices.
                p::handle_map_vk_semaphore_wait_info(&mut info, &mut mapping)?;
            }
            let mut pool = sv_gpu_entry::Raise::default();
            let info = sv_gpu_entry::raise_vk_semaphore_wait_info(&info, &mut pool)?;
            let start = std::time::Instant::now();
            let result = bounded_wait(
                a.timeout,
                || elapsed_ns(start),
                || failed_shared(&shared),
                |slice| {
                    let result = vk::Result::from_raw(unsafe { native(device, &info, slice) }.0);
                    #[cfg(all(target_os = "macos", feature = "native-video-fixture"))]
                    crate::native_video_queue_fixture::frontend_wait_slice(false, result);
                    result
                },
            );
            Ok(p::VkWaitSemaphoresReply {
                result: p::VkResult(result.as_raw()),
            })
        })())
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn cancellation_before_wait_never_calls_native() {
        assert_eq!(
            bounded_wait(
                u64::MAX,
                || 0,
                || true,
                |_| panic!("native wait after cancellation")
            ),
            vk::Result::ERROR_DEVICE_LOST
        );
    }
    #[test]
    fn infinite_wait_observes_cancellation_after_one_bounded_slice() {
        let cancelled = Cell::new(false);
        let mut slices = Vec::new();
        let result = bounded_wait(
            u64::MAX,
            || 0,
            || cancelled.get(),
            |slice| {
                slices.push(slice);
                cancelled.set(true);
                vk::Result::TIMEOUT
            },
        );
        assert_eq!(result, vk::Result::ERROR_DEVICE_LOST);
        assert_eq!(slices, [WAIT_SLICE_NS]);
    }
    #[test]
    fn cancellation_wins_over_concurrent_native_success() {
        let cancelled = Cell::new(false);
        assert_eq!(
            bounded_wait(
                u64::MAX,
                || 0,
                || cancelled.get(),
                |_| {
                    cancelled.set(true);
                    vk::Result::SUCCESS
                }
            ),
            vk::Result::ERROR_DEVICE_LOST
        );
    }
    #[test]
    fn finite_timeout_preserves_deadline_and_shortens_final_slice() {
        let elapsed = Cell::new(0);
        let mut slices = Vec::new();
        let result = bounded_wait(
            120_000_000,
            || elapsed.get(),
            || false,
            |slice| {
                slices.push(slice);
                elapsed.set(elapsed.get() + slice);
                vk::Result::TIMEOUT
            },
        );
        assert_eq!(result, vk::Result::TIMEOUT);
        assert_eq!(slices, [50_000_000, 50_000_000, 20_000_000]);
        assert_eq!(elapsed.get(), 120_000_000);
    }
    #[test]
    fn zero_timeout_is_one_native_poll_and_native_errors_are_preserved() {
        for expected in [
            vk::Result::SUCCESS,
            vk::Result::TIMEOUT,
            vk::Result::ERROR_OUT_OF_HOST_MEMORY,
        ] {
            let mut slices = Vec::new();
            let actual = bounded_wait(
                0,
                || 0,
                || false,
                |slice| {
                    slices.push(slice);
                    expected
                },
            );
            assert_eq!(actual, expected);
            assert_eq!(slices, [0]);
        }
    }
    #[test]
    fn elapsed_native_overrun_does_not_start_another_wait() {
        let elapsed = Cell::new(0);
        let mut calls = 0;
        assert_eq!(
            bounded_wait(
                55_000_000,
                || elapsed.get(),
                || false,
                |_| {
                    calls += 1;
                    elapsed.set(70_000_000);
                    vk::Result::TIMEOUT
                }
            ),
            vk::Result::TIMEOUT
        );
        assert_eq!(calls, 1);
    }
}
