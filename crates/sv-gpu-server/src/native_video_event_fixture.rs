//! Feature-only prefix publication probe, owned by the existing safe controller.
//! No native submission occurs during preparation. No Drop implementation frees
//! resources: the enclosing full Fixture graph owns retirement and quarantine.
use super::*;
const EVENT: u64 = 160;
const READY: u64 = 161;
const POOL: u64 = 162;
const CB: u64 = 163;

pub(super) struct Prepared {
    event: bool,
    ready: bool,
    pool: bool,
}
impl Prepared {
    pub(super) fn prepare(rpc: &mut sv_gpu_entry::Forward<GpuServer>) -> Result<Self, String> {
        let mut prepared = Self {
            event: false,
            ready: false,
            pool: false,
        };
        if let Err(error) = prepared.prepare_inner(rpc) {
            // No submit occurs in this function, so these partial objects have
            // no native/VT users. Cleanup is explicit, not a timeout inference.
            let cleanup = unsafe { prepared.cleanup_after_retirement(rpc) };
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; preaccept cleanup: {cleanup}"),
            });
        }
        Ok(prepared)
    }
    fn prepare_inner(&mut self, rpc: &mut sv_gpu_entry::Forward<GpuServer>) -> Result<(), String> {
        checked(
            wire(rpc.create_event(p::VkCreateEventArgs {
                device: p::VkDevice(DEVICE),
                p_event: Some(p::VkEvent(EVENT)),
                p_create_info: Some(Box::default()),
                ..Default::default()
            }))?
            .result,
        )?;
        self.event = true;
        checked(
            wire(rpc.create_semaphore(p::VkCreateSemaphoreArgs {
                device: p::VkDevice(DEVICE),
                p_semaphore: Some(p::VkSemaphore(READY)),
                p_create_info: Some(Box::new(p::VkSemaphoreCreateInfo {
                    p_next: vec![p::PNextLink::VkSemaphoreTypeCreateInfo(Box::new(
                        p::VkSemaphoreTypeCreateInfo {
                            semaphore_type: p::VkSemaphoreType(
                                vk::SemaphoreType::TIMELINE.as_raw(),
                            ),
                            initial_value: 0,
                            ..Default::default()
                        },
                    ))],
                    ..Default::default()
                })),
                ..Default::default()
            }))?
            .result,
        )?;
        self.ready = true;
        let family = rpc.host().shared.driver.queue_family;
        checked(
            wire(rpc.create_command_pool(p::VkCreateCommandPoolArgs {
                device: p::VkDevice(DEVICE),
                p_command_pool: Some(p::VkCommandPool(POOL)),
                p_create_info: Some(Box::new(p::VkCommandPoolCreateInfo {
                    queue_family_index: family,
                    ..Default::default()
                })),
                ..Default::default()
            }))?
            .result,
        )?;
        self.pool = true;
        checked(
            wire(
                rpc.allocate_command_buffers(p::VkAllocateCommandBuffersArgs {
                    device: p::VkDevice(DEVICE),
                    p_allocate_info: Some(Box::new(p::VkCommandBufferAllocateInfo {
                        command_pool: p::VkCommandPool(POOL),
                        level: p::VkCommandBufferLevel(vk::CommandBufferLevel::PRIMARY.as_raw()),
                        command_buffer_count: 1,
                        ..Default::default()
                    })),
                    p_command_buffers: vec![p::VkCommandBuffer(CB)],
                }),
            )?
            .result,
        )?;
        checked(
            wire(rpc.begin_command_buffer(p::VkBeginCommandBufferArgs {
                command_buffer: p::VkCommandBuffer(CB),
                p_begin_info: Some(Box::default()),
            }))?
            .result,
        )?;
        wire(rpc.cmd_set_event2(p::VkCmdSetEvent2Args {
            command_buffer: p::VkCommandBuffer(CB),
            event: p::VkEvent(EVENT),
            p_dependency_info: Some(Box::new(p::VkDependencyInfo {
                memory_barrier_count: 1,
                p_memory_barriers: vec![p::VkMemoryBarrier2 {
                    src_stage_mask: p::VkPipelineStageFlags2(
                        vk::PipelineStageFlags2::ALL_COMMANDS.as_raw(),
                    ),
                    dst_stage_mask: p::VkPipelineStageFlags2(
                        vk::PipelineStageFlags2::ALL_COMMANDS.as_raw(),
                    ),
                    ..Default::default()
                }],
                ..Default::default()
            })),
        }))?;
        checked(
            wire(rpc.end_command_buffer(p::VkEndCommandBufferArgs {
                command_buffer: p::VkCommandBuffer(CB),
            }))?
            .result,
        )?;
        Ok(())
    }
    pub(super) fn prefix_submit(&self) -> p::VkSubmitInfo {
        p::VkSubmitInfo {
            command_buffer_count: 1,
            p_command_buffers: vec![p::VkCommandBuffer(CB)],
            signal_semaphore_count: 1,
            p_signal_semaphores: vec![p::VkSemaphore(READY)],
            p_next: vec![p::PNextLink::VkTimelineSemaphoreSubmitInfo(Box::new(
                p::VkTimelineSemaphoreSubmitInfo {
                    signal_semaphore_value_count: 1,
                    p_signal_semaphore_values: vec![1],
                    ..Default::default()
                },
            ))],
            ..Default::default()
        }
    }
    pub(super) fn prefix_submit2(&self) -> p::VkSubmitInfo2 {
        p::VkSubmitInfo2 {
            command_buffer_info_count: 1,
            p_command_buffer_infos: vec![p::VkCommandBufferSubmitInfo {
                command_buffer: p::VkCommandBuffer(CB),
                ..Default::default()
            }],
            signal_semaphore_info_count: 1,
            p_signal_semaphore_infos: vec![p::VkSemaphoreSubmitInfo {
                semaphore: p::VkSemaphore(READY),
                value: 1,
                stage_mask: p::VkPipelineStageFlags2(
                    vk::PipelineStageFlags2::ALL_COMMANDS.as_raw(),
                ),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    /// Native prefix semaphore completion, then one actual event status query.
    /// No event polling and no claim of whole-job/VT retirement from this proof.
    pub(super) fn observe(&self, rpc: &mut sv_gpu_entry::Forward<GpuServer>) -> Result<(), String> {
        checked(
            wire(rpc.wait_semaphores(p::VkWaitSemaphoresArgs {
                device: p::VkDevice(DEVICE),
                timeout: 3_000_000_000,
                p_wait_info: Some(Box::new(p::VkSemaphoreWaitInfo {
                    semaphore_count: 1,
                    p_semaphores: vec![p::VkSemaphore(READY)],
                    p_values: vec![1],
                    ..Default::default()
                })),
            }))?
            .result,
        )?;
        let status = wire(rpc.get_event_status(p::VkGetEventStatusArgs {
            device: p::VkDevice(DEVICE),
            event: p::VkEvent(EVENT),
        }))?
        .result;
        if status.0 != vk::Result::EVENT_SET.as_raw() {
            return Err(format!("prefix event is not natively SET: {}", status.0));
        }
        let fence = wire(rpc.get_fence_status(p::VkGetFenceStatusArgs {
            device: p::VkDevice(DEVICE),
            fence: p::VkFence(FENCE),
        }))?
        .result;
        if fence.0 != vk::Result::NOT_READY.as_raw() {
            return Err(format!(
                "whole-job fence completed before original HOST signal: {}",
                fence.0
            ));
        }
        // READY proves the original event's final native use completed. The
        // unrelated later batch is still waiting for HOST, so these legal host
        // mutations must not be refused by delayed whole-request metadata.
        checked(
            wire(rpc.reset_event(p::VkResetEventArgs {
                device: p::VkDevice(DEVICE),
                event: p::VkEvent(EVENT),
            }))?
            .result,
        )?;
        let reset = wire(rpc.get_event_status(p::VkGetEventStatusArgs {
            device: p::VkDevice(DEVICE),
            event: p::VkEvent(EVENT),
        }))?
        .result;
        if reset.0 != vk::Result::EVENT_RESET.as_raw() {
            return Err(format!(
                "prefix host reset not reflected by native status: {}",
                reset.0
            ));
        }
        checked(
            wire(rpc.set_event(p::VkSetEventArgs {
                device: p::VkDevice(DEVICE),
                event: p::VkEvent(EVENT),
            }))?
            .result,
        )?;
        let set = wire(rpc.get_event_status(p::VkGetEventStatusArgs {
            device: p::VkDevice(DEVICE),
            event: p::VkEvent(EVENT),
        }))?
        .result;
        if set.0 != vk::Result::EVENT_SET.as_raw() {
            return Err(format!(
                "prefix host set not reflected by native status: {}",
                set.0
            ));
        }
        println!(
            "event prefix native_timeline=true actual_event_set=true overall_fence_pending=true host_reset_set_after_prefix=true event_polling=false"
        );
        Ok(())
    }
    pub(super) fn resource_report(&self) {
        println!(
            "event_prefix_resources events=1 timeline_semaphores=1 command_pools=1 primary_command_buffers=1 payload_bytes=0"
        );
    }
    /// # Safety
    /// The existing controller has proved every accepted native/VT/input owner
    /// retired, joined the body, and owns this full retained Fixture graph.
    /// A deadline, prefixReady or event SET alone never supplies this proof.
    pub(super) unsafe fn cleanup_after_retirement(
        &self,
        rpc: &mut sv_gpu_entry::Forward<GpuServer>,
    ) -> Result<(), String> {
        if self.pool {
            wire(rpc.destroy_command_pool(p::VkDestroyCommandPoolArgs {
                device: p::VkDevice(DEVICE),
                command_pool: p::VkCommandPool(POOL),
                ..Default::default()
            }))?;
        }
        if self.event {
            wire(rpc.destroy_event(p::VkDestroyEventArgs {
                device: p::VkDevice(DEVICE),
                event: p::VkEvent(EVENT),
                ..Default::default()
            }))?;
        }
        if self.ready {
            wire(rpc.destroy_semaphore(p::VkDestroySemaphoreArgs {
                device: p::VkDevice(DEVICE),
                semaphore: p::VkSemaphore(READY),
                ..Default::default()
            }))?;
        }
        Ok(())
    }
}
