//! The actual opened queue is index zero, never an alias for another request.
use super::ServedFamily;
use ash::vk;
use sv_proto as p;

pub(super) fn queue(
    served: &[ServedFamily],
    family: u32,
    index: u32,
    flags: u32,
) -> Option<vk::Queue> {
    if index != 0 || flags != 0 {
        return None;
    }
    let family = served.get(family as usize)?;
    (family.props.queue_count == 1 && family.queue != vk::Queue::null()).then_some(family.queue)
}

pub(super) fn validate_requests(
    served: &[ServedFamily],
    info: &p::VkDeviceCreateInfo,
) -> Result<(), vk::Result> {
    let invalid = vk::Result::ERROR_INITIALIZATION_FAILED;
    if info.queue_create_info_count == 0
        || info.queue_create_info_count as usize != info.p_queue_create_infos.len()
    {
        return Err(invalid);
    }
    let mut requested = std::collections::HashSet::new();
    for request in &info.p_queue_create_infos {
        if request.queue_count != 1
            || request.p_queue_priorities.len() != 1
            || !requested.insert(request.queue_family_index)
            || queue(served, request.queue_family_index, 0, request.flags.0).is_none()
            || !request.p_queue_priorities[0].is_finite()
            || !(0.0..=1.0).contains(&request.p_queue_priorities[0])
        {
            return Err(invalid);
        }
    }
    Ok(())
}

/// Mixed native/external decoding uses one actual GCT queue, not two aliases.
/// Preserve physical family indices, including inactive entries before it.
/// This is an activation condition, not a restriction on ordinary providers.
pub(super) fn single_gct(
    served: &[ServedFamily],
    selected_family: u32,
    selected_queue: vk::Queue,
) -> bool {
    let mut active = served
        .iter()
        .enumerate()
        .filter(|(_, family)| family.props.queue_count != 0);
    let Some((index, family)) = active.next() else {
        return false;
    };
    active.next().is_none()
        && index == selected_family as usize
        && family
            .props
            .queue_flags
            .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER)
        && queue(served, selected_family, 0, 0) == Some(selected_queue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash::vk::Handle;
    fn family(native: u64) -> ServedFamily {
        ServedFamily {
            props: vk::QueueFamilyProperties {
                queue_flags: vk::QueueFlags::GRAPHICS
                    | vk::QueueFlags::COMPUTE
                    | vk::QueueFlags::TRANSFER,
                queue_count: 1,
                ..Default::default()
            },
            video_codec_operations: vk::VideoCodecOperationFlagsKHR::empty(),
            query_result_status: false,
            queue: vk::Queue::from_raw(native),
        }
    }
    fn request(family: u32, count: u32) -> p::VkDeviceQueueCreateInfo {
        p::VkDeviceQueueCreateInfo {
            queue_family_index: family,
            queue_count: count,
            p_queue_priorities: vec![1.0; count as usize],
            ..Default::default()
        }
    }
    fn create(requests: Vec<p::VkDeviceQueueCreateInfo>) -> p::VkDeviceCreateInfo {
        p::VkDeviceCreateInfo {
            queue_create_info_count: requests.len() as u32,
            p_queue_create_infos: requests,
            ..Default::default()
        }
    }
    #[test]
    fn single_queue_fixture_matches_advertisement_creation_and_both_getters() {
        let served = [family(0x1234)];
        assert_eq!(served[0].props.queue_count, 1);
        assert!(single_gct(&served, 0, served[0].queue));
        assert!(validate_requests(&served, &create(vec![request(0, 1)])).is_ok());
        // Core GetDeviceQueue and GetDeviceQueue2 feed these same fields.
        let core = p::VkGetDeviceQueueArgs {
            queue_family_index: 0,
            queue_index: 0,
            ..Default::default()
        };
        let sync = p::VkDeviceQueueInfo2 {
            queue_family_index: 0,
            queue_index: 0,
            ..Default::default()
        };
        assert_eq!(
            queue(&served, core.queue_family_index, core.queue_index, 0),
            Some(served[0].queue)
        );
        assert_eq!(
            queue(
                &served,
                sync.queue_family_index,
                sync.queue_index,
                sync.flags.0
            ),
            Some(served[0].queue)
        );
        assert!(validate_requests(&served, &create(vec![request(0, 2)])).is_err());
        assert!(validate_requests(&served, &create(vec![request(0, 1), request(0, 1)])).is_err());
        for (family, index, flags) in [(0, 1, 0), (1, 0, 0), (0, 0, 1), (u32::MAX, 0, 0)] {
            assert_eq!(
                queue(&served, family, index, flags),
                None,
                "invalid request must not alias the only native queue"
            );
        }
    }
    #[test]
    fn malformed_counts_flags_and_unopened_families_are_refused() {
        let served = [family(1)];
        for requests in [vec![], vec![request(0, 0)], vec![request(1, 1)]] {
            assert!(validate_requests(&served, &create(requests)).is_err());
        }
        let mut request = request(0, 1);
        request.flags = p::VkDeviceQueueCreateFlags(1);
        assert!(validate_requests(&served, &create(vec![request])).is_err());
        let mut info = create(vec![self::request(0, 1)]);
        info.queue_create_info_count = 2;
        assert!(validate_requests(&served, &info).is_err());
        info.queue_create_info_count = 1;
        info.p_queue_create_infos[0].p_queue_priorities.clear();
        assert!(validate_requests(&served, &info).is_err());
    }
    #[test]
    fn physical_indices_are_preserved_and_multiple_real_queues_remain_ordinary() {
        let ordinary = [family(11), family(22)];
        assert!(validate_requests(&ordinary, &create(vec![request(0, 1), request(1, 1)])).is_ok());
        assert_eq!(queue(&ordinary, 1, 0, 0), Some(ordinary[1].queue));
        assert!(!single_gct(&ordinary, 0, ordinary[0].queue));
        let mut hole = family(0);
        hole.props.queue_count = 0;
        let indexed = [hole, hole, family(33)];
        assert!(single_gct(&indexed, 2, indexed[2].queue));
        assert!(!single_gct(&indexed, 0, indexed[2].queue));
        assert_eq!(queue(&indexed, 0, 0, 0), None);
        assert_eq!(queue(&indexed, 2, 0, 0), Some(indexed[2].queue));
    }
    #[test]
    fn mismatched_native_queue_capabilities_and_counts_cannot_activate_video() {
        let mut served = [family(1)];
        assert!(!single_gct(&served, 0, vk::Queue::from_raw(2)));
        served[0].props.queue_count = 2;
        assert!(!single_gct(&served, 0, served[0].queue));
        served[0].props.queue_count = 1;
        served[0].props.queue_flags &= !vk::QueueFlags::GRAPHICS;
        assert!(!single_gct(&served, 0, served[0].queue));
        served[0] = family(0);
        assert!(!single_gct(&served, 0, served[0].queue));
    }
}
