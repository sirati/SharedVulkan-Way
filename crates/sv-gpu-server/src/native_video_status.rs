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
