//! Retained native/video submission segments. This remains behind readiness.
use crate::{
    native_video::Decode,
    native_video_decode::{self, Runtime},
    native_video_events::{Events, Lowered, Operation, Ticket},
    native_video_image::Image,
    native_video_source::Prepared,
    native_video_submits::Batch,
    server::Shared,
    video_recording::{self, Step},
    video_submit::{self, Work},
};
use ash::vk::{self, Handle};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use sv_proto::HandleType;
fn invalid() -> vk::Result {
    vk::Result::ERROR_INITIALIZATION_FAILED
}
struct Resolved {
    job: Decode,
    source: Mutex<Option<Prepared>>,
    destination: Arc<Image>,
    setup: Option<Arc<Image>>,
    // Logical setup descriptor validity/lifetime also applies to nonreference
    // pictures, even though that image is not a producer write target.
    _setup_descriptor: Option<Arc<Image>>,
    references: Vec<Arc<Image>>,
    begin_references: Vec<Arc<Image>>,
}
enum Action {
    Native(vk::CommandBuffer),
    Decode(Arc<Resolved>),
}
struct BatchPlan {
    waits: Vec<video_submit::Semaphore>,
    actions: Vec<Action>,
    signals: Vec<video_submit::Semaphore>,
    fence: vk::Fence,
    event_ticket: Option<usize>,
}
struct EventReservation {
    ticket: Ticket,
    committed: bool,
}
fn event_operations(steps: &[Step], operations: &mut Vec<Operation>) {
    for step in steps {
        match step {
            Step::Native(call) => {
                if let Some(op) = Operation::from_recorded(call) {
                    operations.push(op);
                }
            }
            Step::Execute { secondaries, .. } => {
                for child in secondaries.iter() {
                    event_operations(&child.steps, operations);
                }
            }
            _ => {}
        }
    }
}
fn validate_secondary_generations(
    steps: &[Step],
    plans: &video_recording::Plans,
) -> Result<(), vk::Result> {
    for step in steps {
        if let Step::Execute { secondaries, .. } = step {
            for child in secondaries.iter() {
                let current = plans
                    .secondary_snapshot(child.wire)
                    .map_err(|_| invalid())?;
                if current.generation != child.generation {
                    return Err(invalid());
                }
                validate_secondary_generations(&child.steps, plans)?;
            }
        }
    }
    Ok(())
}
pub(crate) struct Submission {
    shared: Arc<Shared>,
    runtime: Arc<Mutex<Runtime>>,
    pool: vk::CommandPool,
    batches: Vec<BatchPlan>,
    images: Vec<Arc<Image>>,
    submitted: bool,
    retired: bool,
    retained_fences: Vec<vk::Fence>,
    events: Arc<Mutex<Events>>,
    event_reservations: Vec<EventReservation>,
}
impl Submission {
    /// Resolve every allocation, synchronization handle and immutable recording
    /// before submitting any native work. No compressed payload is read here.
    pub(crate) fn prepare(
        shared: Arc<Shared>,
        queue: u64,
        batches: Vec<Batch>,
    ) -> Result<Self, vk::Result> {
        if !shared.driver.native_video_queue_contract() {
            return Err(invalid());
        }
        // Repeated GetDeviceQueue may return distinct wire aliases for the
        // same queue. Event provenance follows the actual VkQueue identity.
        let queue = shared
            .objects()
            .get::<vk::Queue>(HandleType::VkQueue, queue)
            .map_err(|_| invalid())?;
        if queue != shared.driver.queue {
            return Err(invalid());
        }
        let queue = queue.as_raw();
        let runtime = Arc::clone(
            &shared
                .native_video
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .runtime,
        );
        let events = Arc::clone(
            &shared
                .native_video
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .events,
        );
        let pool = unsafe {
            shared.driver.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(shared.driver.queue_family),
                None,
            )
        }?;
        let mut result = Self {
            shared,
            runtime,
            pool,
            batches: vec![],
            images: vec![],
            submitted: false,
            retired: false,
            retained_fences: vec![],
            events,
            event_reservations: vec![],
        };
        // Freeze all command generations first. No native producer is started.
        let frozen = {
            let state = result
                .shared
                .native_video
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            batches
                .into_iter()
                .map(|batch| {
                    let plans = batch
                        .commands
                        .iter()
                        .map(|wire| state.plans.video(*wire).map_err(|_| invalid()))
                        .collect::<Result<Vec<_>, _>>()?;
                    for plan in plans.iter().flatten() {
                        validate_secondary_generations(plan, &state.plans)?;
                    }
                    let begins = batch
                        .commands
                        .iter()
                        .map(|wire| state.plans.begin_info(*wire).cloned().ok())
                        .collect::<Vec<_>>();
                    Ok((batch, plans, begins))
                })
                .collect::<Result<Vec<_>, vk::Result>>()?
        };
        // One HOST snapshot and projected reservation transaction covers every
        // batch. Native recording starts only after all reservations succeed.
        let prepared = {
            let mut events = result.events.lock().unwrap_or_else(|e| e.into_inner());
            let mut prepared = Vec::new();
            for (batch, plans, begins) in frozen {
                let mut operations = Vec::new();
                for (plan, begin) in plans.iter().zip(&begins) {
                    if let Some(plan) = plan {
                        let before = operations.len();
                        event_operations(plan, &mut operations);
                        if operations.len() != before && begin.is_none() {
                            return Err(invalid());
                        }
                    }
                }
                let (ticket, lowered) = if operations.is_empty() {
                    (None, VecDeque::new())
                } else {
                    let plan = events.plan(queue, &operations).map_err(|_| invalid())?;
                    let ticket = events.reserve(&plan).map_err(|_| invalid())?;
                    let index = result.event_reservations.len();
                    result.event_reservations.push(EventReservation {
                        ticket,
                        committed: false,
                    });
                    (Some(index), VecDeque::from(plan.lowered))
                };
                prepared.push((batch, plans, begins, ticket, lowered));
            }
            prepared
        };
        for (batch, plans, begins, event_ticket, mut event_lowered) in prepared {
            let semaphores = |semaphores: &[crate::native_video_submits::Semaphore]| -> Result<Vec<_>,vk::Result> {
                let objects = result.shared.objects();
                semaphores.iter().map(|s| Ok(video_submit::Semaphore {
                    handle: objects.get(HandleType::VkSemaphore,s.wire).map_err(|_|invalid())?,
                    value:s.value,stage:s.stage })).collect()
            };
            let waits = semaphores(&batch.waits)?;
            let signals = semaphores(&batch.signals)?;
            let fence = if batch.fence == 0 {
                vk::Fence::null()
            } else {
                result
                    .shared
                    .objects()
                    .get(HandleType::VkFence, batch.fence)
                    .map_err(|_| invalid())?
            };
            let mut actions = vec![];
            for ((&wire, plan), begin) in batch.commands.iter().zip(plans).zip(begins) {
                if let Some(plan) = plan {
                    let begin = begin.unwrap_or_default();
                    let mut segment = None;
                    let mut state = Vec::<Arc<sv_gpu_entry::RecordedCall>>::new();
                    for step in plan.iter() {
                        match step {
                            Step::Barrier(original) => {
                                let (lowered, handles) = result.resolve_barrier(original)?;
                                let command = result.segment(&mut segment, &state, wire, &begin)?;
                                unsafe {
                                    crate::native_video_barriers::record(
                                        &result.shared.driver,
                                        command,
                                        &lowered,
                                        |ty, wire| {
                                            handles.get(&(ty, wire)).copied().ok_or_else(invalid)
                                        },
                                    )
                                }?;
                            }
                            Step::Native(call) => {
                                let command = result.segment(&mut segment, &state, wire, &begin)?;
                                result.record_call(command, call, &mut event_lowered)?;
                                // Secondary execution invalidates the primary's native state.
                                if call.name() == "vkCmdExecuteCommands" {
                                    state.clear();
                                }
                                if matches!(
                                    call.restoration(),
                                    sv_gpu_entry::RecordedRestoration::AuditedState
                                ) {
                                    state.push(Arc::clone(call));
                                }
                            }
                            Step::Execute { call, secondaries } => {
                                let command = result.segment(&mut segment, &state, wire, &begin)?;
                                result.execute_secondaries(
                                    command,
                                    call,
                                    secondaries,
                                    &mut event_lowered,
                                )?;
                                state.clear();
                            }
                            Step::Decode(job) => {
                                result.flush_segment(&mut segment, &mut actions)?;
                                let resolved = Arc::new(result.resolve_decode(job)?);
                                let mut targets = vec![Arc::clone(&resolved.destination)];
                                if let Some(setup) = &resolved.setup {
                                    if !Arc::ptr_eq(setup, &resolved.destination) {
                                        targets.push(Arc::clone(setup));
                                    }
                                }
                                actions.push(Action::Native(result.ownership(&targets, true)?));
                                actions.push(Action::Decode(resolved));
                                actions.push(Action::Native(result.ownership(&targets, false)?));
                            }
                        }
                    }
                    result.flush_segment(&mut segment, &mut actions)?;
                } else {
                    let command = result
                        .shared
                        .objects()
                        .get::<vk::CommandBuffer>(HandleType::VkCommandBuffer, wire)
                        .map_err(|_| invalid())?;
                    if result.shared.poisoned.get(command.as_raw()).is_some() {
                        return Err(invalid());
                    }
                    actions.push(Action::Native(command));
                }
            }
            if !event_lowered.is_empty() {
                return Err(invalid());
            }
            result.batches.push(BatchPlan {
                waits,
                actions,
                signals,
                fence,
                event_ticket,
            });
        }
        Ok(result)
    }
    fn execute_secondaries(
        &mut self,
        command: vk::CommandBuffer,
        call: &sv_gpu_entry::RecordedCall,
        secondaries: &[video_recording::SecondarySnapshot],
        lowered: &mut VecDeque<Lowered>,
    ) -> Result<(), vk::Result> {
        if !call.available(&self.shared.fns) {
            return Err(invalid());
        }
        let sv_gpu_entry::RecordedCall::VkCmdExecuteCommands(args) = call else {
            return Err(invalid());
        };
        if args.p_command_buffers.len() != secondaries.len()
            || args.command_buffer_count as usize != secondaries.len()
        {
            return Err(invalid());
        }
        for (wire, snapshot) in args.p_command_buffers.iter().zip(secondaries) {
            if wire.0 != snapshot.wire {
                return Err(invalid());
            }
            let child = self.secondary(snapshot, lowered)?;
            // Each occurrence receives a private recording, including repeated
            // wire IDs. Ordinary secondary execution invalidates primary state.
            unsafe {
                self.shared
                    .driver
                    .device
                    .cmd_execute_commands(command, &[child]);
            }
        }
        Ok(())
    }
    fn secondary(
        &mut self,
        snapshot: &video_recording::SecondarySnapshot,
        lowered: &mut VecDeque<Lowered>,
    ) -> Result<vk::CommandBuffer, vk::Result> {
        let command = unsafe {
            self.shared.driver.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::SECONDARY)
                    .command_buffer_count(1),
            )
        }?[0];
        let mut host = crate::server::GpuServer::new(Arc::clone(&self.shared));
        host.native_queue_worker = true;
        host.native_record_replay = Some((snapshot.wire, command.as_raw()));
        let mut forward = sv_gpu_entry::Forward::new(host);
        use sv_proto::Server;
        let reply = forward
            .begin_command_buffer(sv_proto::VkBeginCommandBufferArgs {
                command_buffer: sv_proto::VkCommandBuffer(snapshot.wire),
                p_begin_info: Some(Box::new((*snapshot.begin).clone())),
            })
            .map_err(|_| invalid())?;
        if reply.result.0 != 0 {
            return Err(invalid());
        }
        for step in snapshot.steps.iter() {
            match step {
                Step::Native(call) => self.record_call(command, call, lowered)?,
                Step::Barrier(original) => {
                    let (lowered, handles) = self.resolve_barrier(original)?;
                    unsafe {
                        crate::native_video_barriers::record(
                            &self.shared.driver,
                            command,
                            &lowered,
                            |ty, wire| handles.get(&(ty, wire)).copied().ok_or_else(invalid),
                        )
                    }?;
                }
                Step::Execute { call, secondaries } => {
                    self.execute_secondaries(command, call, secondaries, lowered)?
                }
                Step::Decode(_) => return Err(invalid()),
            }
        }
        unsafe { self.shared.driver.device.end_command_buffer(command) }?;
        Ok(command)
    }
    fn record_call(
        &mut self,
        command: vk::CommandBuffer,
        call: &sv_gpu_entry::RecordedCall,
        lowered: &mut VecDeque<Lowered>,
    ) -> Result<(), vk::Result> {
        if Operation::from_recorded(call).is_none() {
            return self.replay_native(command, call);
        }
        let (barriers, write) = match lowered.pop_front().ok_or_else(invalid)? {
            Lowered::NoOp => return Ok(()),
            Lowered::Set { before, write, .. } => (
                before,
                Some(sv_gpu_entry::RecordedCall::VkCmdSetEvent2(Box::new(write))),
            ),
            Lowered::Reset { before, write, .. } => (
                before,
                Some(sv_gpu_entry::RecordedCall::VkCmdResetEvent2(Box::new(
                    write,
                ))),
            ),
            Lowered::Wait { barriers, .. } => (barriers, None),
        };
        for barrier in barriers {
            let (barrier, handles) = self.resolve_barrier(&barrier)?;
            unsafe {
                crate::native_video_barriers::record(
                    &self.shared.driver,
                    command,
                    &barrier,
                    |ty, wire| handles.get(&(ty, wire)).copied().ok_or_else(invalid),
                )
            }?;
        }
        if let Some(write) = write {
            self.replay_native(command, &write)?;
        }
        Ok(())
    }
    fn replay_native(
        &self,
        command: vk::CommandBuffer,
        call: &sv_gpu_entry::RecordedCall,
    ) -> Result<(), vk::Result> {
        if !call.available(&self.shared.fns) {
            return Err(invalid());
        }
        if let sv_gpu_entry::RecordedCall::VkCmdExecuteCommands(args) = call {
            let state = self.shared.native_video.lock().unwrap();
            for child in &args.p_command_buffers {
                if state.plans.video(child.0).map_err(|_| invalid())?.is_some() {
                    return Err(invalid());
                }
            }
            drop(state);
            let objects = self.shared.objects();
            for child in &args.p_command_buffers {
                let native = objects
                    .get::<vk::CommandBuffer>(HandleType::VkCommandBuffer, child.0)
                    .map_err(|_| invalid())?;
                if self.shared.poisoned.get(native.as_raw()).is_some() {
                    return Err(invalid());
                }
            }
        }
        let mut host = crate::server::GpuServer::new(Arc::clone(&self.shared));
        host.native_queue_worker = true;
        host.native_record_replay = Some((call.command_buffer(), command.as_raw()));
        let mut forward = sv_gpu_entry::Forward::new(host);
        call.clone().replay(&mut forward).map_err(|_| invalid())?;
        if self.shared.poisoned.get(command.as_raw()).is_some() {
            return Err(invalid());
        }
        Ok(())
    }
    fn segment(
        &self,
        segment: &mut Option<vk::CommandBuffer>,
        state: &[Arc<sv_gpu_entry::RecordedCall>],
        wire: u64,
        begin: &sv_proto::VkCommandBufferBeginInfo,
    ) -> Result<vk::CommandBuffer, vk::Result> {
        if let Some(command) = *segment {
            return Ok(command);
        }
        let command = unsafe {
            self.shared.driver.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }?[0];
        let mut host = crate::server::GpuServer::new(Arc::clone(&self.shared));
        host.native_queue_worker = true;
        host.native_record_replay = Some((wire, command.as_raw()));
        let mut forward = sv_gpu_entry::Forward::new(host);
        use sv_proto::Server;
        let reply = forward
            .begin_command_buffer(sv_proto::VkBeginCommandBufferArgs {
                command_buffer: sv_proto::VkCommandBuffer(wire),
                p_begin_info: Some(Box::new(begin.clone())),
            })
            .map_err(|_| invalid())?;
        if reply.result.0 != 0 {
            return Err(invalid());
        }
        for call in state {
            self.replay_native(command, call)?;
        }
        *segment = Some(command);
        Ok(command)
    }
    fn flush_segment(
        &self,
        segment: &mut Option<vk::CommandBuffer>,
        actions: &mut Vec<Action>,
    ) -> Result<(), vk::Result> {
        if let Some(command) = segment.take() {
            unsafe { self.shared.driver.device.end_command_buffer(command) }?;
            actions.push(Action::Native(command));
        }
        Ok(())
    }
    fn command(&self) -> Result<vk::CommandBuffer, vk::Result> {
        let command = unsafe {
            self.shared.driver.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }?[0];
        unsafe {
            self.shared.driver.device.begin_command_buffer(
                command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }?;
        Ok(command)
    }
    fn resolve_decode(&self, job: &Decode) -> Result<Resolved, vk::Result> {
        let objects = self.shared.objects();
        let view = |picture: &crate::native_video::Picture| {
            let view = objects
                .native_video
                .views
                .get(&picture.image_view)
                .ok_or_else(invalid)?;
            if view.layer != 0
                || picture.layer != 0
                || picture.offset != vk::Offset2D::default()
                || picture.extent != view.image.extent()
            {
                return Err(invalid());
            }
            view.image.validate_bound()?;
            Ok(Arc::clone(&view.image))
        };
        let source = objects
            .native_video
            .sources
            .prepare(
                Arc::clone(&self.shared),
                &objects,
                job.source_buffer,
                job.source_offset,
                job.source_range,
            )
            .map_err(|_| invalid())?;
        let setup_descriptor = job.setup.as_ref().map(|r| view(&r.picture)).transpose()?;
        Ok(Resolved {
            job: job.clone(),
            source: Mutex::new(Some(source)),
            destination: view(&job.destination)?,
            // Non-reference setup invalidates its slot without a producer
            // write. Its supplied descriptor must still name a valid resource.
            setup: if job.h264.is_reference {
                setup_descriptor.clone()
            } else {
                None
            },
            _setup_descriptor: setup_descriptor,
            references: job
                .references
                .iter()
                .map(|r| view(&r.picture))
                .collect::<Result<_, _>>()?,
            begin_references: job
                .begin_references
                .iter()
                .map(|r| view(&r.picture))
                .collect::<Result<_, _>>()?,
        })
    }
    fn resolve_barrier(
        &mut self,
        barrier: &video_recording::Barrier,
    ) -> Result<(video_recording::Barrier, HashMap<(HandleType, u64), u64>), vk::Result> {
        let objects = self.shared.objects();
        let lowered = video_recording::lower(barrier, |wire| {
            objects.native_video.images.contains_key(&wire)
        })
        .map_err(|_| invalid())?;
        let mut handles = HashMap::new();
        let mut image = |wire: u64| -> Result<(), vk::Result> {
            let raw = objects
                .get::<vk::Image>(HandleType::VkImage, wire)
                .map_err(|_| invalid())?;
            handles.insert((HandleType::VkImage, wire), raw.as_raw());
            if let Some(owner) = objects.native_video.images.get(&wire) {
                owner.validate_bound()?;
                self.images.push(Arc::clone(owner));
            }
            Ok(())
        };
        let buffers: Vec<_> = match &lowered {
            video_recording::Barrier::Legacy(v) => {
                for b in &v.p_image_memory_barriers {
                    image(b.image.0)?;
                }
                v.p_buffer_memory_barriers
                    .iter()
                    .map(|b| b.buffer.0)
                    .collect()
            }
            video_recording::Barrier::Sync2(v) => {
                let info = v.p_dependency_info.as_deref().ok_or_else(invalid)?;
                for b in &info.p_image_memory_barriers {
                    image(b.image.0)?;
                }
                info.p_buffer_memory_barriers
                    .iter()
                    .map(|b| b.buffer.0)
                    .collect()
            }
        };
        for wire in buffers {
            let raw = objects
                .get::<vk::Buffer>(HandleType::VkBuffer, wire)
                .map_err(|_| invalid())?;
            handles.insert((HandleType::VkBuffer, wire), raw.as_raw());
        }
        Ok((lowered, handles))
    }
    fn ownership(
        &self,
        images: &[Arc<Image>],
        release: bool,
    ) -> Result<vk::CommandBuffer, vk::Result> {
        let command = self.command()?;
        let barriers: Vec<_> = images
            .iter()
            .map(|image| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(if release {
                        self.shared.driver.queue_family
                    } else {
                        vk::QUEUE_FAMILY_EXTERNAL
                    })
                    .dst_queue_family_index(if release {
                        vk::QUEUE_FAMILY_EXTERNAL
                    } else {
                        self.shared.driver.queue_family
                    })
                    .src_access_mask(if release {
                        vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE
                    } else {
                        vk::AccessFlags::empty()
                    })
                    .dst_access_mask(if release {
                        vk::AccessFlags::empty()
                    } else {
                        vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE
                    })
                    .image(image.image())
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    )
            })
            .collect();
        unsafe {
            self.shared.driver.device.cmd_pipeline_barrier(
                command,
                if release {
                    vk::PipelineStageFlags::ALL_COMMANDS
                } else {
                    vk::PipelineStageFlags::TOP_OF_PIPE
                },
                if release {
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE
                } else {
                    vk::PipelineStageFlags::ALL_COMMANDS
                },
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            self.shared.driver.device.end_command_buffer(command)
        }?;
        Ok(command)
    }
    pub(crate) unsafe fn execute(&mut self) -> Result<(), vk::Result> {
        let result = unsafe { self.execute_inner() };
        if result.is_err() {
            let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
            for event in self
                .event_reservations
                .iter()
                .filter(|event| !event.committed)
            {
                let _ = events.fail(&event.ticket);
            }
        }
        result
    }
    unsafe fn execute_inner(&mut self) -> Result<(), vk::Result> {
        let shared = Arc::clone(&self.shared);
        let _queue = shared.queue_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.submitted = true;
        for batch in &self.batches {
            let jobs: Vec<_> = batch
                .actions
                .iter()
                .filter_map(|a| match a {
                    Action::Decode(r) => Some(r),
                    _ => None,
                })
                .collect();
            let mut executor = Executor {
                runtime: &self.runtime,
                jobs,
                next: 0,
                events: &self.events,
                event: batch
                    .event_ticket
                    .map(|index| &mut self.event_reservations[index]),
            };
            let work: Vec<_> = batch
                .actions
                .iter()
                .map(|action| match action {
                    Action::Native(cb) => Work::Native(*cb),
                    Action::Decode(r) => Work::Decode(&r.job),
                })
                .collect();
            unsafe {
                video_submit::submit(
                    &shared.driver,
                    &batch.waits,
                    &work,
                    &batch.signals,
                    batch.fence,
                    &mut executor,
                    &mut self.retained_fences,
                )
            }?;
        }
        // User signals/fence remain attached to their original final batch.
        // A following internal fence retains every owner through native use.
        unsafe { video_submit::complete(&shared.driver, &mut self.retained_fences) }?;
        self.retired = true;
        Ok(())
    }
    pub(crate) fn retired_after_idle(&mut self, _proof: crate::video_worker::RetirementProof) {
        self.retired = true;
    }
}
struct Executor<'a> {
    runtime: &'a Arc<Mutex<Runtime>>,
    jobs: Vec<&'a Arc<Resolved>>,
    next: usize,
    events: &'a Arc<Mutex<Events>>,
    event: Option<&'a mut EventReservation>,
}
impl video_submit::DecodeExecutor for Executor<'_> {
    fn needs_body_completion(&self) -> bool {
        self.event.is_some()
    }
    fn body_completed(&mut self) -> Result<(), vk::Result> {
        if let Some(event) = self.event.as_mut() {
            // Real native body fence and every intervening VT/input producer
            // completed. Commit before original batch signals become visible.
            unsafe {
                self.events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .complete(&event.ticket)
            }
            .map_err(|_| invalid())?;
            event.committed = true;
        }
        Ok(())
    }
    fn decode_completed(&mut self, job: &Decode) -> Result<(), vk::Result> {
        let resolved = self.jobs.get(self.next).ok_or_else(invalid)?;
        if !std::ptr::eq(&resolved.job, job) {
            return Err(invalid());
        }
        self.next += 1;
        let prepared = resolved
            .source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(invalid)?;
        let (source, source_range) = unsafe { prepared.after_wait() };
        let resources = native_video_decode::Resources {
            source,
            source_range,
            destination: Arc::clone(&resolved.destination),
            setup: resolved.setup.clone(),
            references: resolved.references.clone(),
            begin_references: resolved.begin_references.clone(),
        };
        unsafe {
            self.runtime
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .execute(job, resources)
        }
        .map_err(|error| {
            eprintln!("native decode execution failed: {error}");
            vk::Result::ERROR_DEVICE_LOST
        })
    }
}
impl Drop for Submission {
    fn drop(&mut self) {
        // All pre-accept preparation is serialized through enqueue. Roll back
        // tail reservations in reverse order only when no native work started.
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        for event in self
            .event_reservations
            .iter()
            .rev()
            .filter(|event| !event.committed)
        {
            if !self.submitted {
                if unsafe { events.cancel_unaccepted(&event.ticket) }.is_err() {
                    let _ = events.fail(&event.ticket);
                }
            } else {
                let _ = events.fail(&event.ticket);
            }
        }
        drop(events);
        assert!(
            !self.submitted || self.retired,
            "native video owners dropped before retirement"
        );
        unsafe {
            for fence in self.retained_fences.drain(..) {
                self.shared.driver.device.destroy_fence(fence, None);
            }
            self.shared
                .driver
                .device
                .destroy_command_pool(self.pool, None);
        }
    }
}
