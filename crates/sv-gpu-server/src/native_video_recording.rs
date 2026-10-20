//! macOS recording hooks. Original owned wire calls replay at ordinary End or
//! remain in an immutable mixed plan for private native-segment preparation.
use crate::{
    server::GpuServer,
    video_recording::{Barrier, BarrierAction, End, Step},
};
use ash::vk;
use std::sync::Arc;
use sv_gpu_entry::{Forward, GpuHost};
use sv_proto::{self as p, Server};
use sv_wire::Result;
fn failed() -> p::VkResult {
    p::VkResult(vk::Result::ERROR_INITIALIZATION_FAILED.as_raw())
}
impl GpuServer {
    fn native_recording_enabled(&self) -> bool {
        // Keep interception after an asynchronous error so synthetic plans can
        // never fall back to direct submission. Initial readiness stays false.
        self.shared.native_video.lock().unwrap().video.available()
    }
    fn replay_recording<T>(
        &mut self,
        call: impl FnOnce(&mut Forward<GpuServer>) -> Result<T>,
    ) -> Result<T> {
        let mut host = GpuServer::new(Arc::clone(&self.shared));
        host.native_queue_worker = true;
        let mut forward = Forward::new(host);
        let result = call(&mut forward);
        if let Some(error) = forward.host().last_error.take() {
            self.last_error = Some(error)
        }
        result
    }
    fn recording_error(&mut self, wire: u64, why: &'static str) -> Result<()> {
        self.shared
            .native_video
            .lock()
            .unwrap()
            .plans
            .poison(wire, why);
        let actual = self.map_handle(p::HandleType::VkCommandBuffer, wire)?;
        self.poison_command_buffer(actual, why);
        self.last_error = Some(why.into());
        Err(sv_wire::WireError::UnknownCommand { opcode: 0 })
    }
    pub(crate) fn native_captures_recorded_commands(&self, wire: u64) -> bool {
        self.native_recording_enabled()
            && self
                .shared
                .native_video
                .lock()
                .unwrap()
                .plans
                .captures_native(wire)
    }
    pub(crate) fn native_capture_recorded_command(
        &mut self,
        call: sv_gpu_entry::RecordedCall,
    ) -> Result<()> {
        let wire = call.command_buffer();
        if !call.available(&self.shared.fns) {
            return self.recording_error(wire, "captured command has no native implementation");
        }
        let result = self.shared.native_video.lock().unwrap().plans.native(call);
        match result {
            Ok(()) => Ok(()),
            Err(error) => self.recording_error(wire, error.0),
        }
    }
    fn replay_steps(&mut self, wire: u64, steps: Vec<Step>) -> Result<()> {
        for step in steps {
            match step {
                Step::Barrier(barrier) => self.replay_barriers(wire, vec![barrier])?,
                Step::Native(call) | Step::Execute { call, .. } => {
                    let call = Arc::try_unwrap(call).unwrap_or_else(|call| (*call).clone());
                    self.replay_recording(|f| call.replay(f))?;
                }
                Step::Decode(_) => {
                    return self.recording_error(wire, "decode in ordinary native replay");
                }
            }
        }
        Ok(())
    }
    fn replay_barriers(&mut self, wire: u64, barriers: Vec<Barrier>) -> Result<()> {
        for barrier in barriers {
            let lowered = {
                let objects = self.shared.objects();
                crate::video_recording::lower(&barrier, |wire| {
                    objects.native_video.images.contains_key(&wire)
                })
            };
            let barrier = match lowered {
                Ok(value) => value,
                Err(error) => return self.recording_error(wire, error.0),
            };
            let result = match barrier {
                Barrier::Legacy(args) => self.replay_recording(|f| f.cmd_pipeline_barrier(args)),
                Barrier::Sync2(args) => self.replay_recording(|f| f.cmd_pipeline_barrier2(args)),
            };
            if result.is_err() {
                return self.recording_error(wire, "failed to replay deferred native barrier");
            }
            let actual = self.map_handle(p::HandleType::VkCommandBuffer, wire)?;
            if self.shared.poisoned.get(actual).is_some() {
                return self.recording_error(wire, "deferred barrier has no native implementation");
            }
        }
        Ok(())
    }
    pub(crate) fn native_prepare_recorded_command(
        &mut self,
        name: &'static str,
        wire: u64,
    ) -> Result<()> {
        if !self.native_recording_enabled() {
            return Ok(());
        }
        let barriers = {
            let mut state = self.shared.native_video.lock().unwrap();
            if !state.plans.contains(wire) {
                return Ok(());
            }
            state.plans.note_ordinary_command(wire, name);
            state.plans.prepare_ordinary(wire)
        };
        match barriers {
            Ok(barriers) => self.replay_barriers(wire, barriers),
            Err(error) => self.recording_error(wire, error.0),
        }
    }
    pub(crate) fn native_record_allocate(
        &mut self,
        a: &p::VkAllocateCommandBuffersArgs,
    ) -> Option<Result<p::VkAllocateCommandBuffersReply>> {
        if !self.native_recording_enabled() {
            return None;
        }
        let info = a.p_allocate_info.as_deref()?;
        let ids: Vec<_> = a.p_command_buffers.iter().map(|v| v.0).collect();
        if info.command_buffer_count as usize != ids.len() {
            return Some(Ok(p::VkAllocateCommandBuffersReply {
                result: failed(),
                p_command_buffers: vec![],
            }));
        }
        let reserved = {
            let mut state = self.shared.native_video.lock().unwrap();
            if info.level.0 == vk::CommandBufferLevel::PRIMARY.as_raw() {
                state.plans.allocated(info.command_pool.0, &ids)
            } else if info.level.0 == vk::CommandBufferLevel::SECONDARY.as_raw() {
                state.plans.allocated_secondary(info.command_pool.0, &ids)
            } else {
                return Some(Ok(p::VkAllocateCommandBuffersReply {
                    result: failed(),
                    p_command_buffers: vec![],
                }));
            }
        };
        if reserved.is_err() {
            return Some(Ok(p::VkAllocateCommandBuffersReply {
                result: failed(),
                p_command_buffers: vec![],
            }));
        }
        let result = self.replay_recording(|f| f.allocate_command_buffers(a.clone()));
        if !matches!(&result,Ok(reply) if reply.result.0==0) {
            self.shared.native_video.lock().unwrap().plans.free(&ids)
        }
        Some(result)
    }
    pub(crate) fn native_record_begin(
        &mut self,
        a: &p::VkBeginCommandBufferArgs,
    ) -> Option<Result<p::VkBeginCommandBufferReply>> {
        if !self.native_recording_enabled() {
            return None;
        }
        let result = self.replay_recording(|f| f.begin_command_buffer(a.clone()));
        if matches!(&result,Ok(reply) if reply.result.0==0) {
            let mut state = self.shared.native_video.lock().unwrap();
            state.video.reset_command(a.command_buffer.0);
            if state.plans.begin(a.command_buffer.0).is_err() {
                drop(state);
                let _ = self.recording_error(a.command_buffer.0, "invalid native recording begin");
                return Some(Ok(p::VkBeginCommandBufferReply { result: failed() }));
            }
            let video_compatible = state.plans.allows_video(a.command_buffer.0)
                && a.p_begin_info.as_deref().is_some_and(|info| {
                    info.p_next.is_empty()
                        && info.p_inheritance_info.is_none()
                        && info.flags.0 & vk::CommandBufferUsageFlags::RENDER_PASS_CONTINUE.as_raw()
                            == 0
                });
            if let Some(info) = a.p_begin_info.as_deref() {
                if state
                    .plans
                    .set_begin_info(a.command_buffer.0, info.clone())
                    .is_err()
                {
                    drop(state);
                    let _ = self.recording_error(
                        a.command_buffer.0,
                        "failed to retain native begin metadata",
                    );
                    return Some(Ok(p::VkBeginCommandBufferReply { result: failed() }));
                }
            }
            if !video_compatible && state.plans.allows_video(a.command_buffer.0) {
                let _ = state.plans.mark_video_incompatible(a.command_buffer.0);
            }
        }
        Some(result)
    }
    pub(crate) fn native_record_end(
        &mut self,
        a: &p::VkEndCommandBufferArgs,
    ) -> Option<Result<p::VkEndCommandBufferReply>> {
        if !self.native_recording_enabled() {
            return None;
        }
        let ended = self
            .shared
            .native_video
            .lock()
            .unwrap()
            .plans
            .end(a.command_buffer.0);
        let prepared = match ended {
            Ok(End::Ordinary(steps)) => self.replay_steps(a.command_buffer.0, steps),
            Ok(End::Video(_)) => Ok(()),
            Err(error) => self.recording_error(a.command_buffer.0, error.0),
        };
        // Even an invalid logical recording must close the real driver CB.
        let mut result = self.replay_recording(|f| f.end_command_buffer(a.clone()));
        if prepared.is_err() {
            if let Ok(reply) = &mut result {
                if reply.result.0 == 0 {
                    reply.result = failed()
                }
            }
        }
        if !matches!(&result,Ok(reply) if reply.result.0==0) {
            self.shared
                .native_video
                .lock()
                .unwrap()
                .plans
                .poison(a.command_buffer.0, "native command buffer end failed")
        }
        Some(result)
    }
    pub(crate) fn native_record_reset(
        &mut self,
        a: &p::VkResetCommandBufferArgs,
    ) -> Option<Result<p::VkResetCommandBufferReply>> {
        if !self.native_recording_enabled() {
            return None;
        }
        let result = self.replay_recording(|f| f.reset_command_buffer(a.clone()));
        if matches!(&result,Ok(reply) if reply.result.0==0) {
            let mut state = self.shared.native_video.lock().unwrap();
            state.plans.reset(a.command_buffer.0);
            state.video.reset_command(a.command_buffer.0)
        }
        Some(result)
    }
    pub(crate) fn native_record_free(
        &mut self,
        a: &p::VkFreeCommandBuffersArgs,
    ) -> Option<Result<()>> {
        if !self.native_recording_enabled() {
            return None;
        }
        let result = self.replay_recording(|f| f.free_command_buffers(a.clone()));
        if result.is_ok() {
            let mut state = self.shared.native_video.lock().unwrap();
            let ids: Vec<_> = a.p_command_buffers.iter().map(|v| v.0).collect();
            for id in &ids {
                state.video.reset_command(*id)
            }
            state.plans.free(&ids)
        }
        Some(result)
    }
    pub(crate) fn native_record_reset_pool(
        &mut self,
        a: &p::VkResetCommandPoolArgs,
    ) -> Option<Result<p::VkResetCommandPoolReply>> {
        if !self.native_recording_enabled() {
            return None;
        }
        let result = self.replay_recording(|f| f.reset_command_pool(a.clone()));
        if matches!(&result,Ok(reply) if reply.result.0==0) {
            let mut state = self.shared.native_video.lock().unwrap();
            for id in state.plans.pool_buffers(a.command_pool.0) {
                state.video.reset_command(id)
            }
            state.plans.reset_pool(a.command_pool.0)
        }
        Some(result)
    }
    pub(crate) fn native_record_destroy_pool(
        &mut self,
        a: &p::VkDestroyCommandPoolArgs,
    ) -> Option<Result<()>> {
        if !self.native_recording_enabled() {
            return None;
        }
        let result = self.replay_recording(|f| f.destroy_command_pool(a.clone()));
        if result.is_ok() {
            let mut state = self.shared.native_video.lock().unwrap();
            for id in state.plans.pool_buffers(a.command_pool.0) {
                state.video.reset_command(id)
            }
            state.plans.destroy_pool(a.command_pool.0)
        }
        Some(result)
    }
    fn native_record_barrier(&mut self, barrier: Barrier) -> Option<Result<()>> {
        if !self.native_recording_enabled() {
            return None;
        }
        let wire = barrier.command();
        let action = self
            .shared
            .native_video
            .lock()
            .unwrap()
            .plans
            .barrier(barrier);
        match action {
            Ok(BarrierAction::Deferred) => Some(Ok(())),
            Ok(BarrierAction::Forward(_)) => None,
            Err(error) => Some(self.recording_error(wire, error.0)),
        }
    }
    pub(crate) fn native_record_barrier_legacy(
        &mut self,
        a: &p::VkCmdPipelineBarrierArgs,
    ) -> Option<Result<()>> {
        self.native_record_barrier(Barrier::Legacy(a.clone()))
    }
    pub(crate) fn native_record_barrier_sync2(
        &mut self,
        a: &p::VkCmdPipelineBarrier2Args,
    ) -> Option<Result<()>> {
        self.native_record_barrier(Barrier::Sync2(a.clone()))
    }
    pub(crate) fn native_record_execute(
        &mut self,
        a: &p::VkCmdExecuteCommandsArgs,
    ) -> Option<Result<()>> {
        if !self.native_recording_enabled() {
            return None;
        }
        let children: Vec<_> = a.p_command_buffers.iter().map(|cb| cb.0).collect();
        let tracked = self
            .shared
            .native_video
            .lock()
            .unwrap()
            .plans
            .executed(a.command_buffer.0, &children);
        if let Err(error) = tracked {
            return Some(self.recording_error(a.command_buffer.0, error.0));
        }
        let poisoned = {
            let objects = self.shared.objects();
            children.iter().any(|wire| {
                match objects.get::<vk::CommandBuffer>(p::HandleType::VkCommandBuffer, *wire) {
                    Ok(command) => {
                        use ash::vk::Handle;
                        self.shared.poisoned.get(command.as_raw()).is_some()
                    }
                    Err(_) => true,
                }
            })
        };
        if poisoned {
            Some(self.recording_error(
                a.command_buffer.0,
                "secondary native command buffer is poisoned or unmapped",
            ))
        } else {
            None
        }
    }
}
