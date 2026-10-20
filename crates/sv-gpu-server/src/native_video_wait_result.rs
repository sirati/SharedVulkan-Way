//! Cancellation observed after a real native wait wins over that slice's result.
use ash::vk;
pub(crate) fn observed_slice(
    result: Result<(), vk::Result>,
    failure: Option<vk::Result>,
) -> Result<(), vk::Result> {
    if failure.is_some() {
        Err(vk::Result::ERROR_DEVICE_LOST)
    } else {
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_after_native_success_prevents_producer_callback() {
        let mut producer_calls = 0;
        let result = observed_slice(Ok(()), Some(vk::Result::ERROR_DEVICE_LOST))
            .map(|_| producer_calls += 1);
        assert_eq!(result, Err(vk::Result::ERROR_DEVICE_LOST));
        assert_eq!(producer_calls, 0);
    }
    #[test]
    fn healthy_native_timeout_and_error_remain_observations() {
        for result in [
            Ok(()),
            Err(vk::Result::TIMEOUT),
            Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY),
        ] {
            assert_eq!(observed_slice(result, None), result);
        }
    }
}
