//! Normalize only submissions containing a selected synthetic-video CB.
//! Ordinary submissions return None before shape inspection and keep forwarding.
use ash::vk;
use sv_proto as p;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Semaphore {
    pub wire: u64,
    pub value: u64,
    pub stage: vk::PipelineStageFlags2,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    pub waits: Vec<Semaphore>,
    pub commands: Vec<u64>,
    pub signals: Vec<Semaphore>,
    pub fence: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmitError(pub &'static str);
type Result<T> = std::result::Result<T, SubmitError>;
fn count(n: u32, actual: usize) -> Result<()> {
    if n as usize == actual {
        Ok(())
    } else {
        Err(SubmitError("submit count/array mismatch"))
    }
}
fn legacy_extensions(info: &p::VkSubmitInfo) -> Result<(Option<&[u64]>, Option<&[u64]>)> {
    let mut timeline = None;
    let mut group = false;
    for link in &info.p_next {
        match link {
            p::PNextLink::VkTimelineSemaphoreSubmitInfo(v) if timeline.is_none() => {
                if !v.p_next.is_empty() {
                    return Err(SubmitError("nested timeline submit extension"));
                }
                count(
                    v.wait_semaphore_value_count,
                    v.p_wait_semaphore_values.len(),
                )?;
                count(
                    v.signal_semaphore_value_count,
                    v.p_signal_semaphore_values.len(),
                )?;
                count(info.wait_semaphore_count, v.p_wait_semaphore_values.len())?;
                count(
                    info.signal_semaphore_count,
                    v.p_signal_semaphore_values.len(),
                )?;
                timeline = Some((
                    v.p_wait_semaphore_values.as_slice(),
                    v.p_signal_semaphore_values.as_slice(),
                ));
            }
            p::PNextLink::VkDeviceGroupSubmitInfo(v) if !group => {
                group = true;
                if !v.p_next.is_empty() {
                    return Err(SubmitError("nested device-group submit extension"));
                }
                count(
                    v.wait_semaphore_count,
                    v.p_wait_semaphore_device_indices.len(),
                )?;
                count(
                    v.signal_semaphore_count,
                    v.p_signal_semaphore_device_indices.len(),
                )?;
                count(
                    v.command_buffer_count,
                    v.p_command_buffer_device_masks.len(),
                )?;
                if v.wait_semaphore_count != 0 {
                    count(
                        info.wait_semaphore_count,
                        v.p_wait_semaphore_device_indices.len(),
                    )?
                }
                if v.signal_semaphore_count != 0 {
                    count(
                        info.signal_semaphore_count,
                        v.p_signal_semaphore_device_indices.len(),
                    )?
                }
                if v.command_buffer_count != 0 {
                    count(
                        info.command_buffer_count,
                        v.p_command_buffer_device_masks.len(),
                    )?
                }
                if v.p_wait_semaphore_device_indices
                    .iter()
                    .chain(&v.p_signal_semaphore_device_indices)
                    .any(|&v| v != 0)
                    || v.p_command_buffer_device_masks.iter().any(|&v| v != 1)
                {
                    return Err(SubmitError("multiple-device submit unsupported"));
                }
            }
            _ => return Err(SubmitError("unknown or duplicate legacy submit extension")),
        }
    }
    Ok(timeline.map_or((None, None), |(w, s)| (Some(w), Some(s))))
}
fn finish(mut batches: Vec<Batch>, fence: u64) -> Vec<Batch> {
    if let Some(last) = batches.last_mut() {
        last.fence = fence
    }
    batches
}
pub fn legacy(
    a: &p::VkQueueSubmitArgs,
    is_video: impl Fn(u64) -> bool,
) -> Result<Option<Vec<Batch>>> {
    if !a
        .p_submits
        .iter()
        .any(|s| s.p_command_buffers.iter().any(|c| is_video(c.0)))
    {
        return Ok(None);
    }
    count(a.submit_count, a.p_submits.len())?;
    let mut batches = Vec::new();
    for info in &a.p_submits {
        count(info.wait_semaphore_count, info.p_wait_semaphores.len())?;
        count(info.wait_semaphore_count, info.p_wait_dst_stage_mask.len())?;
        count(info.signal_semaphore_count, info.p_signal_semaphores.len())?;
        count(info.command_buffer_count, info.p_command_buffers.len())?;
        let (w, s) = legacy_extensions(info)?;
        let waits = info
            .p_wait_semaphores
            .iter()
            .enumerate()
            .map(|(i, sem)| Semaphore {
                wire: sem.0,
                value: w.map_or(0, |v| v[i]),
                stage: vk::PipelineStageFlags2::from_raw(u64::from(
                    info.p_wait_dst_stage_mask[i].0,
                )),
            })
            .collect();
        let signals = info
            .p_signal_semaphores
            .iter()
            .enumerate()
            .map(|(i, sem)| Semaphore {
                wire: sem.0,
                value: s.map_or(0, |v| v[i]),
                stage: vk::PipelineStageFlags2::ALL_COMMANDS,
            })
            .collect();
        let commands = info.p_command_buffers.iter().map(|c| c.0).collect();
        batches.push(Batch {
            waits,
            commands,
            signals,
            fence: 0,
        });
    }
    Ok(Some(finish(batches, a.fence.0)))
}
fn semaphore(info: &p::VkSemaphoreSubmitInfo) -> Result<Semaphore> {
    if !info.p_next.is_empty() || info.device_index != 0 {
        return Err(SubmitError("unsupported semaphore submit metadata"));
    }
    Ok(Semaphore {
        wire: info.semaphore.0,
        value: info.value,
        stage: vk::PipelineStageFlags2::from_raw(info.stage_mask.0),
    })
}
pub fn sync2(
    a: &p::VkQueueSubmit2Args,
    is_video: impl Fn(u64) -> bool,
) -> Result<Option<Vec<Batch>>> {
    if !a.p_submits.iter().any(|s| {
        s.p_command_buffer_infos
            .iter()
            .any(|c| is_video(c.command_buffer.0))
    }) {
        return Ok(None);
    }
    count(a.submit_count, a.p_submits.len())?;
    let mut batches = Vec::new();
    for info in &a.p_submits {
        if !info.p_next.is_empty() || info.flags.0 != 0 {
            return Err(SubmitError("unsupported sync2 flags or extension"));
        }
        count(
            info.wait_semaphore_info_count,
            info.p_wait_semaphore_infos.len(),
        )?;
        count(
            info.signal_semaphore_info_count,
            info.p_signal_semaphore_infos.len(),
        )?;
        count(
            info.command_buffer_info_count,
            info.p_command_buffer_infos.len(),
        )?;
        let waits = info
            .p_wait_semaphore_infos
            .iter()
            .map(semaphore)
            .collect::<Result<Vec<_>>>()?;
        let signals = info
            .p_signal_semaphore_infos
            .iter()
            .map(semaphore)
            .collect::<Result<Vec<_>>>()?;
        let commands = info
            .p_command_buffer_infos
            .iter()
            .map(|c| {
                if !c.p_next.is_empty() || c.device_mask > 1 {
                    return Err(SubmitError("unsupported command-buffer submit metadata"));
                }
                Ok(c.command_buffer.0)
            })
            .collect::<Result<Vec<_>>>()?;
        batches.push(Batch {
            waits,
            commands,
            signals,
            fence: 0,
        });
    }
    Ok(Some(finish(batches, a.fence.0)))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn info() -> p::VkSubmitInfo {
        p::VkSubmitInfo {
            wait_semaphore_count: 2,
            p_wait_semaphores: vec![p::VkSemaphore(1), p::VkSemaphore(2)],
            p_wait_dst_stage_mask: vec![p::VkPipelineStageFlags(4), p::VkPipelineStageFlags(8)],
            command_buffer_count: 1,
            p_command_buffers: vec![p::VkCommandBuffer(9)],
            signal_semaphore_count: 2,
            p_signal_semaphores: vec![p::VkSemaphore(3), p::VkSemaphore(4)],
            ..Default::default()
        }
    }
    fn legacy_call(info: p::VkSubmitInfo) -> p::VkQueueSubmitArgs {
        p::VkQueueSubmitArgs {
            submit_count: 1,
            p_submits: vec![info],
            fence: p::VkFence(99),
            ..Default::default()
        }
    }
    #[test]
    fn binary_semaphores_keep_zero_and_future_waits_are_not_consumed() {
        let call = legacy_call(info());
        let b = legacy(&call, |c| c == 9).unwrap().unwrap();
        assert_eq!(
            b[0].waits
                .iter()
                .map(|s| (s.wire, s.value, s.stage.as_raw()))
                .collect::<Vec<_>>(),
            vec![(1, 0, 4), (2, 0, 8)]
        );
        assert_eq!(
            b[0].signals.iter().map(|s| s.value).collect::<Vec<_>>(),
            vec![0, 0]
        );
        assert_eq!(b[0].commands, vec![9]);
        assert_eq!(b[0].fence, 99);
    }
    fn timeline() -> p::PNextLink {
        p::PNextLink::VkTimelineSemaphoreSubmitInfo(Box::new(p::VkTimelineSemaphoreSubmitInfo {
            wait_semaphore_value_count: 2,
            p_wait_semaphore_values: vec![0, 100],
            signal_semaphore_value_count: 2,
            p_signal_semaphore_values: vec![0, 101],
            ..Default::default()
        }))
    }
    #[test]
    fn legacy_mixed_binary_timeline_values_and_final_batch_fence_survive() {
        let mut i = info();
        i.p_next.push(timeline());
        let mut call = legacy_call(i);
        call.submit_count = 2;
        call.p_submits.push(p::VkSubmitInfo::default());
        let b = legacy(&call, |c| c == 9).unwrap().unwrap();
        assert_eq!(b[0].waits[1].value, 100);
        assert_eq!(b[0].signals[1].value, 101);
        assert_eq!(b[0].signals[0].value, 0);
        assert_eq!(b[0].fence, 0);
        assert_eq!(b[1].fence, 99);
        assert_eq!(call.p_submits[0].p_wait_semaphores[1].0, 2);
    }
    #[test]
    fn legacy_rejects_inconsistent_timeline_counts_and_duplicates() {
        let mut i = info();
        i.p_next.push(timeline());
        let p::PNextLink::VkTimelineSemaphoreSubmitInfo(t) = &mut i.p_next[0] else {
            panic!()
        };
        t.wait_semaphore_value_count = 1;
        assert!(legacy(&legacy_call(i), |_| true).is_err());
        let mut i = info();
        i.p_next = vec![timeline(), timeline()];
        assert!(legacy(&legacy_call(i), |_| true).is_err());
    }
    #[test]
    fn sync2_keeps_binary_and_future_timeline_waits_and_signal_stage() {
        let i = p::VkSubmitInfo2 {
            wait_semaphore_info_count: 2,
            p_wait_semaphore_infos: vec![
                p::VkSemaphoreSubmitInfo {
                    semaphore: p::VkSemaphore(1),
                    stage_mask: p::VkPipelineStageFlags2(4),
                    ..Default::default()
                },
                p::VkSemaphoreSubmitInfo {
                    semaphore: p::VkSemaphore(2),
                    value: 100,
                    stage_mask: p::VkPipelineStageFlags2(8),
                    ..Default::default()
                },
            ],
            command_buffer_info_count: 1,
            p_command_buffer_infos: vec![p::VkCommandBufferSubmitInfo {
                command_buffer: p::VkCommandBuffer(9),
                device_mask: 1,
                ..Default::default()
            }],
            signal_semaphore_info_count: 1,
            p_signal_semaphore_infos: vec![p::VkSemaphoreSubmitInfo {
                semaphore: p::VkSemaphore(3),
                value: 101,
                stage_mask: p::VkPipelineStageFlags2(32),
                ..Default::default()
            }],
            ..Default::default()
        };
        let call = p::VkQueueSubmit2Args {
            submit_count: 1,
            p_submits: vec![i],
            fence: p::VkFence(99),
            ..Default::default()
        };
        let b = sync2(&call, |c| c == 9).unwrap().unwrap();
        assert_eq!(b[0].waits[0].value, 0);
        assert_eq!(b[0].waits[1].value, 100);
        assert_eq!(b[0].signals[0].stage.as_raw(), 32);
        assert_eq!(b[0].signals[0].value, 101);
        assert_eq!(b[0].fence, 99);
    }
    #[test]
    fn selected_paths_refuse_multidevice_and_unknown_metadata() {
        let mut unknown = info();
        unknown.p_next = vec![p::PNextLink::VkProtectedSubmitInfo(Box::new(
            p::VkProtectedSubmitInfo::default(),
        ))];
        assert!(legacy(&legacy_call(unknown), |_| true).is_err());
        let mut i = info();
        i.p_next = vec![p::PNextLink::VkDeviceGroupSubmitInfo(Box::new(
            p::VkDeviceGroupSubmitInfo {
                command_buffer_count: 1,
                p_command_buffer_device_masks: vec![2],
                ..Default::default()
            },
        ))];
        assert!(legacy(&legacy_call(i), |_| true).is_err());
        let i = p::VkSubmitInfo2 {
            command_buffer_info_count: 1,
            p_command_buffer_infos: vec![p::VkCommandBufferSubmitInfo {
                command_buffer: p::VkCommandBuffer(9),
                device_mask: 2,
                ..Default::default()
            }],
            ..Default::default()
        };
        let call = p::VkQueueSubmit2Args {
            submit_count: 1,
            p_submits: vec![i],
            ..Default::default()
        };
        assert!(sync2(&call, |_| true).is_err());
    }
    #[test]
    fn ordinary_path_skips_normalization_and_keeps_unknown_shapes_for_forwarding() {
        let mut i = info();
        i.p_next = vec![p::PNextLink::VkProtectedSubmitInfo(Box::new(
            p::VkProtectedSubmitInfo::default(),
        ))];
        assert!(legacy(&legacy_call(i), |_| false).unwrap().is_none());
        let call = p::VkQueueSubmit2Args {
            submit_count: 8,
            p_submits: vec![p::VkSubmitInfo2 {
                flags: p::VkSubmitFlags(1),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(sync2(&call, |_| false).unwrap().is_none());
    }
}
#[cfg(test)]
mod device_group_tests {
    use super::*;
    #[test]
    fn single_device_group_is_validated_before_discarding_redundant_indices() {
        let group = p::VkDeviceGroupSubmitInfo {
            command_buffer_count: 1,
            p_command_buffer_device_masks: vec![1],
            ..Default::default()
        };
        let info = p::VkSubmitInfo {
            command_buffer_count: 1,
            p_command_buffers: vec![p::VkCommandBuffer(9)],
            p_next: vec![p::PNextLink::VkDeviceGroupSubmitInfo(Box::new(group))],
            ..Default::default()
        };
        let mut call = p::VkQueueSubmitArgs {
            submit_count: 1,
            p_submits: vec![info],
            ..Default::default()
        };
        let batches = legacy(&call, |c| c == 9).unwrap().unwrap();
        assert_eq!(batches[0].commands, vec![9]);
        let p::PNextLink::VkDeviceGroupSubmitInfo(group) = &mut call.p_submits[0].p_next[0] else {
            panic!()
        };
        group.command_buffer_count = 2;
        assert!(legacy(&call, |c| c == 9).is_err());
    }
}
