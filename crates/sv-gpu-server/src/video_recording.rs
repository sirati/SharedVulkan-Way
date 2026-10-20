//! Owned primary wire recordings. Ordinary commands replay once at End; mixed
//! plans retain immutable calls and split native recording only at decode cuts.
use crate::native_video::Decode;
use ash::vk;
use std::{collections::HashMap, sync::Arc};
use sv_proto as p;
#[derive(Clone, Debug)]
pub enum Barrier {
    Legacy(p::VkCmdPipelineBarrierArgs),
    Sync2(p::VkCmdPipelineBarrier2Args),
}
impl Barrier {
    pub fn command(&self) -> u64 {
        match self {
            Self::Legacy(v) => v.command_buffer.0,
            Self::Sync2(v) => v.command_buffer.0,
        }
    }
}
#[derive(Clone)]
pub enum Step {
    Barrier(Barrier),
    Decode(Decode),
    Native(Arc<sv_gpu_entry::RecordedCall>),
    Execute {
        call: Arc<sv_gpu_entry::RecordedCall>,
        secondaries: Arc<[SecondarySnapshot]>,
    },
}
/// Immutable recording generation, independent of later reset/free/rebegin.
#[derive(Clone)]
pub struct SecondarySnapshot {
    pub wire: u64,
    pub generation: u64,
    pub begin: Arc<p::VkCommandBufferBeginInfo>,
    pub steps: Arc<[Step]>,
    pub has_events: bool,
    pub unknown_state: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rejected(pub &'static str);
type Result<T> = std::result::Result<T, Rejected>;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Undecided,
    Ordinary,
    Video,
}
#[derive(Default)]
struct NativeScope {
    render: Option<u8>,
    queries: u32,
    conditional: bool,
    feedback: bool,
    unknown_state: bool,
    events: bool,
}
impl NativeScope {
    fn closed(&self) -> bool {
        self.render.is_none() && self.queries == 0 && !self.conditional && !self.feedback
    }
    fn apply(&mut self, call: &sv_gpu_entry::RecordedCall) -> Result<()> {
        let name = call.name();
        let managed = match name {
            "vkCmdBeginRenderPass" | "vkCmdBeginRenderPass2" => {
                if self.render.replace(1).is_some() {
                    return Err(Rejected("nested native rendering scope"));
                }
                true
            }
            "vkCmdBeginRendering" => {
                if self.render.replace(2).is_some() {
                    return Err(Rejected("nested native rendering scope"));
                }
                true
            }
            "vkCmdEndRenderPass" | "vkCmdEndRenderPass2" => {
                if self.render.take() != Some(1) {
                    return Err(Rejected("unmatched native render pass end"));
                }
                true
            }
            "vkCmdEndRendering" => {
                if self.render.take() != Some(2) {
                    return Err(Rejected("unmatched dynamic rendering end"));
                }
                true
            }
            "vkCmdNextSubpass" | "vkCmdNextSubpass2" => {
                if self.render != Some(1) {
                    return Err(Rejected("subpass outside native render pass"));
                }
                true
            }
            "vkCmdBeginQuery" | "vkCmdBeginQueryIndexedEXT" => {
                self.queries = self
                    .queries
                    .checked_add(1)
                    .ok_or(Rejected("query nesting overflow"))?;
                true
            }
            "vkCmdEndQuery" | "vkCmdEndQueryIndexedEXT" => {
                self.queries = self
                    .queries
                    .checked_sub(1)
                    .ok_or(Rejected("unmatched native query end"))?;
                true
            }
            "vkCmdBeginConditionalRenderingEXT" => {
                self.conditional = true;
                true
            }
            "vkCmdEndConditionalRenderingEXT" => {
                self.conditional = false;
                true
            }
            "vkCmdBeginTransformFeedbackEXT" => {
                self.feedback = true;
                true
            }
            "vkCmdEndTransformFeedbackEXT" => {
                self.feedback = false;
                true
            }
            // Labels legally span CB and submit boundaries and have no native
            // graphics state that must be reconstructed at a decode cut.
            "vkCmdBeginDebugUtilsLabelEXT"
            | "vkCmdEndDebugUtilsLabelEXT"
            | "vkCmdInsertDebugUtilsLabelEXT"
            | "vkCmdDebugMarkerBeginEXT"
            | "vkCmdDebugMarkerEndEXT"
            | "vkCmdDebugMarkerInsertEXT" => true,
            _ => false,
        };
        if !managed
            && matches!(
                call.restoration(),
                sv_gpu_entry::RecordedRestoration::UnsupportedState
            )
        {
            self.unknown_state = true;
        }
        if matches!(
            name,
            "vkCmdSetEvent"
                | "vkCmdResetEvent"
                | "vkCmdWaitEvents"
                | "vkCmdSetEvent2"
                | "vkCmdResetEvent2"
                | "vkCmdWaitEvents2"
        ) {
            // Typed event provenance lowering is the next staging increment.
            self.events = true;
        }
        Ok(())
    }
    fn cut(&self, event_lowering: bool) -> Result<()> {
        if !self.closed() {
            return Err(Rejected(
                "external decode would split an active native scope",
            ));
        }
        if self.unknown_state {
            return Err(Rejected("native state restoration is not audited"));
        }
        if self.events && !event_lowering {
            return Err(Rejected(
                "native event provenance lowering is not implemented",
            ));
        }
        Ok(())
    }
}
struct Recording {
    mode: Mode,
    video_incompatible: bool,
    scope: bool,
    ended: bool,
    poison: Option<&'static str>,
    steps: Vec<Step>,
    finished: Option<Arc<[Step]>>,
    native: NativeScope,
    generation: u64,
    begin: Option<Arc<p::VkCommandBufferBeginInfo>>,
    primary_begin: Option<p::VkCommandBufferBeginInfo>,
    snapshot: Option<SecondarySnapshot>,
}
impl Default for Recording {
    fn default() -> Self {
        Self {
            mode: Mode::Undecided,
            video_incompatible: false,
            scope: false,
            ended: false,
            poison: None,
            steps: vec![],
            finished: None,
            native: NativeScope::default(),
            generation: 0,
            begin: None,
            primary_begin: None,
            snapshot: None,
        }
    }
}
pub enum End {
    Ordinary(Vec<Step>),
    Video(Arc<[Step]>),
}
pub enum BarrierAction {
    Deferred,
    Forward(Barrier),
}
#[derive(Default)]
pub struct Plans {
    buffers: HashMap<u64, Recording>,
    allocations: HashMap<u64, u64>,
    secondary: std::collections::HashSet<u64>,
    generation: u64,
    event_lowering: bool,
}
impl Plans {
    /// Internal activation only after the queue owns complete event lowering.
    /// An ordinary End has already moved calls to native replay, so late
    /// activation rejects such event recordings rather than replaying them raw.
    pub fn enable_event_lowering(&mut self) {
        self.event_lowering = true;
        for (wire, recording) in &mut self.buffers {
            if !self.secondary.contains(wire)
                && recording.native.events
                && recording.ended
                && recording.finished.is_none()
            {
                recording.poison = Some("event lowering enabled after ordinary event replay");
            }
        }
    }
    pub fn mark_video_incompatible(&mut self, wire: u64) -> Result<()> {
        let r = self.recording(wire)?;
        if r.mode == Mode::Video {
            r.poison = Some("video recording has incompatible begin metadata");
            return Err(Rejected(r.poison.unwrap()));
        }
        r.video_incompatible = true;
        Ok(())
    }
    pub fn allocated(&mut self, pool: u64, wires: &[u64]) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        if pool == 0
            || wires
                .iter()
                .any(|w| *w == 0 || self.allocations.contains_key(w) || !seen.insert(*w))
        {
            return Err(Rejected("invalid command buffer allocation"));
        }
        for wire in wires {
            self.allocations.insert(*wire, pool);
        }
        Ok(())
    }
    pub fn allocated_secondary(&mut self, pool: u64, wires: &[u64]) -> Result<()> {
        self.allocated(pool, wires)?;
        self.secondary.extend(wires.iter().copied());
        Ok(())
    }
    pub fn allows_video(&self, wire: u64) -> bool {
        !self.secondary.contains(&wire)
    }
    pub fn contains(&self, wire: u64) -> bool {
        self.buffers.contains_key(&wire)
    }
    pub fn is_video(&self, wire: u64) -> bool {
        self.buffers.get(&wire).is_some_and(|r| {
            r.mode == Mode::Video
                || (self.event_lowering && !self.secondary.contains(&wire) && r.native.events)
        })
    }
    pub fn pool_buffers(&self, pool: u64) -> Vec<u64> {
        self.allocations
            .iter()
            .filter_map(|(wire, p)| (*p == pool).then_some(*wire))
            .collect()
    }
    pub fn reset_pool(&mut self, pool: u64) {
        for wire in self.pool_buffers(pool) {
            self.reset(wire)
        }
    }
    pub fn destroy_pool(&mut self, pool: u64) {
        let wires = self.pool_buffers(pool);
        self.free(&wires);
    }
    /// Called only after successful native allocation/begin/reset. The parent
    /// keeps submitted snapshots alive separately from this editable plan.
    pub fn begin(&mut self, wire: u64) -> Result<()> {
        if wire == 0 {
            return Err(Rejected("null command buffer"));
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(Rejected("command buffer generation exhausted"))?;
        let mut recording = Recording::default();
        recording.generation = self.generation;
        if self.secondary.contains(&wire) {
            recording.mode = Mode::Ordinary;
        }
        self.buffers.insert(wire, recording);
        Ok(())
    }
    /// Preserve exact owned begin/inheritance metadata after successful native Begin.
    /// Primary recordings move metadata without an additional Arc allocation.
    pub fn set_begin_info(&mut self, wire: u64, begin: p::VkCommandBufferBeginInfo) -> Result<()> {
        let secondary = self.secondary.contains(&wire);
        let r = self.recording(wire)?;
        if r.begin.is_some() || r.primary_begin.is_some() || !r.steps.is_empty() {
            return Err(Rejected("begin metadata set after recording started"));
        }
        if secondary {
            r.begin = Some(Arc::new(begin));
        } else {
            r.primary_begin = Some(begin);
        }
        Ok(())
    }
    /// Borrow exact metadata without allocating/cloning an ordinary primary.
    pub fn begin_info(&self, wire: u64) -> Result<&p::VkCommandBufferBeginInfo> {
        let r = self
            .buffers
            .get(&wire)
            .ok_or(Rejected("unknown command buffer"))?;
        r.primary_begin
            .as_ref()
            .or(r.begin.as_deref())
            .ok_or(Rejected("missing exact command buffer begin metadata"))
    }
    pub fn secondary_snapshot(&self, wire: u64) -> Result<SecondarySnapshot> {
        let r = self
            .buffers
            .get(&wire)
            .ok_or(Rejected("unknown secondary recording"))?;
        if !self.secondary.contains(&wire)
            || !r.ended
            || r.poison.is_some()
            || r.mode == Mode::Video
        {
            return Err(Rejected(
                "secondary recording is unfinished, poisoned or synthetic",
            ));
        }
        r.snapshot
            .clone()
            .ok_or(Rejected("secondary lacks immutable begin metadata"))
    }
    pub fn reset(&mut self, wire: u64) {
        self.buffers.remove(&wire);
    }
    pub fn free(&mut self, wires: &[u64]) {
        for wire in wires {
            self.allocations.remove(wire);
            self.secondary.remove(wire);
            self.reset(*wire)
        }
    }
    fn recording(&mut self, wire: u64) -> Result<&mut Recording> {
        let r = self
            .buffers
            .get_mut(&wire)
            .ok_or(Rejected("command buffer is not recording"))?;
        if let Some(why) = r.poison {
            return Err(Rejected(why));
        }
        if r.ended {
            return Err(Rejected("command buffer recording has ended"));
        }
        Ok(r)
    }
    pub fn poisoned(&self, wire: u64) -> Option<&'static str> {
        self.buffers.get(&wire).and_then(|r| r.poison)
    }
    pub fn executed(&mut self, wire: u64, secondaries: &[u64]) -> Result<()> {
        let mut events = false;
        for child in secondaries {
            let recording = self
                .buffers
                .get(child)
                .ok_or(Rejected("unknown secondary recording"))?;
            if !self.secondary.contains(child)
                || !recording.ended
                || recording.poison.is_some()
                || recording.mode == Mode::Video
            {
                return Err(Rejected(
                    "secondary recording is unfinished, poisoned or synthetic",
                ));
            }
            events |= recording.native.events;
        }
        let secondary = self.secondary.contains(&wire);
        let r = self.recording(wire)?;
        r.native.events |= events;
        if events && r.mode == Mode::Ordinary && !secondary {
            r.poison = Some("secondary events follow uncaptured ordinary commands");
            return Err(Rejected(r.poison.unwrap()));
        }
        Ok(())
    }
    pub fn note_ordinary_command(&mut self, wire: u64, name: &str) {
        if matches!(
            name,
            "vkCmdSetEvent"
                | "vkCmdResetEvent"
                | "vkCmdWaitEvents"
                | "vkCmdSetEvent2"
                | "vkCmdResetEvent2"
                | "vkCmdWaitEvents2"
        ) {
            if let Some(r) = self.buffers.get_mut(&wire) {
                r.native.events = true;
                if r.mode == Mode::Ordinary && !self.secondary.contains(&wire) {
                    r.poison = Some("event follows uncaptured ordinary commands");
                }
            }
        }
    }
    pub fn has_events(&self, wire: u64) -> bool {
        self.buffers.get(&wire).is_some_and(|r| r.native.events)
    }
    pub fn captures_native(&self, wire: u64) -> bool {
        self.buffers.get(&wire).is_some_and(|r| {
            (r.mode != Mode::Ordinary || self.secondary.contains(&wire))
                && !r.ended
                && r.poison.is_none()
        })
    }
    pub fn native(&mut self, call: sv_gpu_entry::RecordedCall) -> Result<()> {
        let children = if let sv_gpu_entry::RecordedCall::VkCmdExecuteCommands(args) = &call {
            if args.command_buffer_count as usize != args.p_command_buffers.len() {
                return Err(Rejected("secondary command buffer count mismatch"));
            }
            Some(
                args.p_command_buffers
                    .iter()
                    .map(|cb| self.secondary_snapshot(cb.0))
                    .collect::<Result<Vec<_>>>()?,
            )
        } else {
            None
        };
        let wire = call.command_buffer();
        if !self.captures_native(wire) {
            return Err(Rejected("native call cannot enter uncaptured recording"));
        }
        let r = self.recording(wire)?;
        if let Some(children) = &children {
            r.native.events |= children.iter().any(|child| child.has_events);
            // Ordinary secondary execution invalidates all primary state. The
            // secondary is replayed whole, so none of its bindings restore into
            // the primary. NV inheritance exceptions remain outside this path.
            r.native.unknown_state = false;
        }
        if r.scope && !call.allowed_in_video_scope() {
            return Err(Rejected("native command is not permitted in a video scope"));
        }
        r.native.apply(&call)?;
        let call = Arc::new(call);
        r.steps.push(match children {
            Some(children) => Step::Execute {
                call,
                secondaries: children.into(),
            },
            None => Step::Native(call),
        });
        Ok(())
    }
    pub fn barrier(&mut self, barrier: Barrier) -> Result<BarrierAction> {
        let secondary = self.secondary.contains(&barrier.command());
        let r = self.recording(barrier.command())?;
        if r.mode == Mode::Ordinary && !secondary {
            return Ok(BarrierAction::Forward(barrier));
        }
        r.steps.push(Step::Barrier(barrier));
        Ok(BarrierAction::Deferred)
    }
    /// Parent must replay the returned list successfully before this ordinary
    /// command is forwarded. Replay must bypass both interception hooks.
    pub fn prepare_ordinary(&mut self, wire: u64) -> Result<Vec<Barrier>> {
        if self.secondary.contains(&wire) {
            self.poison(wire, "uncaptured secondary command cannot be reconstructed");
            return Err(Rejected(
                "uncaptured secondary command cannot be reconstructed",
            ));
        }
        let r = self.recording(wire)?;
        if r.native.events {
            r.poison = Some("uncaptured command would discard event recording");
            return Err(Rejected(r.poison.unwrap()));
        }
        if r.mode == Mode::Video {
            r.poison = Some("ordinary command mixed with synthetic video");
            return Err(Rejected(r.poison.unwrap()));
        }
        if r.steps
            .iter()
            .any(|s| matches!(s, Step::Native(_) | Step::Execute { .. }))
        {
            r.poison = Some("uncaptured command cannot discard owned native recording");
            return Err(Rejected(r.poison.unwrap()));
        }
        r.mode = Mode::Ordinary;
        Ok(r.steps
            .drain(..)
            .map(|s| match s {
                Step::Barrier(b) => b,
                Step::Decode(_) | Step::Native(_) | Step::Execute { .. } => unreachable!(),
            })
            .collect())
    }
    pub fn begin_scope(&mut self, wire: u64) -> Result<()> {
        if self.secondary.contains(&wire) {
            self.poison(
                wire,
                "video scopes are forbidden in secondary command buffers",
            );
            return Err(Rejected(
                "video scopes are forbidden in secondary command buffers",
            ));
        }
        let event_lowering = self.event_lowering;
        let r = self.recording(wire)?;
        if r.scope || r.mode == Mode::Ordinary || r.video_incompatible {
            r.poison = Some("video coding scope mixed with ordinary recording");
            return Err(Rejected(r.poison.unwrap()));
        }
        r.native.cut(event_lowering)?;
        r.mode = Mode::Video;
        r.scope = true;
        Ok(())
    }
    pub fn decode(&mut self, wire: u64, decode: Decode) -> Result<()> {
        let event_lowering = self.event_lowering;
        let r = self.recording(wire)?;
        if !r.scope {
            r.poison = Some("decode outside video coding scope");
            return Err(Rejected(r.poison.unwrap()));
        }
        r.native.cut(event_lowering)?;
        r.steps.push(Step::Decode(decode));
        Ok(())
    }
    pub fn end_scope(&mut self, wire: u64) -> Result<()> {
        let r = self.recording(wire)?;
        if !r.scope {
            r.poison = Some("end without video coding scope");
            return Err(Rejected(r.poison.unwrap()));
        }
        r.scope = false;
        Ok(())
    }
    pub fn end(&mut self, wire: u64) -> Result<End> {
        let secondary = self.secondary.contains(&wire);
        let event_lowering = self.event_lowering;
        let r = self.recording(wire)?;
        if r.scope {
            r.poison = Some("command buffer ended inside video coding scope");
            return Err(Rejected(r.poison.unwrap()));
        }
        if !r.native.closed() {
            return Err(Rejected("command buffer ended inside native scope"));
        }
        if r.mode == Mode::Video && r.native.events && !event_lowering {
            r.poison = Some("native event provenance lowering is not implemented");
            return Err(Rejected(r.poison.unwrap()));
        }
        r.ended = true;
        if r.mode == Mode::Video || (!secondary && event_lowering && r.native.events) {
            if r.mode == Mode::Ordinary {
                r.poison = Some("cannot route incomplete ordinary event history");
                return Err(Rejected(r.poison.unwrap()));
            }
            let steps: Arc<[Step]> = std::mem::take(&mut r.steps).into();
            r.finished = Some(steps.clone());
            Ok(End::Video(steps))
        } else if secondary {
            let steps: Arc<[Step]> = std::mem::take(&mut r.steps).into();
            if let Some(begin) = &r.begin {
                r.snapshot = Some(SecondarySnapshot {
                    wire,
                    generation: r.generation,
                    begin: begin.clone(),
                    steps: steps.clone(),
                    has_events: r.native.events,
                    unknown_state: r.native.unknown_state,
                });
            }
            Ok(End::Ordinary(steps.iter().cloned().collect()))
        } else {
            Ok(End::Ordinary(std::mem::take(&mut r.steps)))
        }
    }
    pub fn video(&self, wire: u64) -> Result<Option<Arc<[Step]>>> {
        let r = self
            .buffers
            .get(&wire)
            .ok_or(Rejected("unknown command buffer"))?;
        if let Some(why) = r.poison {
            return Err(Rejected(why));
        }
        if !r.ended {
            return Err(Rejected("command buffer is still recording"));
        }
        Ok(r.finished.clone())
    }
    pub fn poison(&mut self, wire: u64, why: &'static str) {
        if let Some(r) = self.buffers.get_mut(&wire) {
            r.poison.get_or_insert(why);
        }
    }
}
fn stage(value: u64) -> u64 {
    let decode = vk::PipelineStageFlags2::VIDEO_DECODE_KHR.as_raw();
    if value & decode != 0 {
        (value & !decode) | vk::PipelineStageFlags2::ALL_COMMANDS.as_raw()
    } else {
        value
    }
}
fn access(value: u64) -> u64 {
    let read = vk::AccessFlags2::VIDEO_DECODE_READ_KHR.as_raw();
    let write = vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR.as_raw();
    let mut out = value & !(read | write);
    if value & read != 0 {
        out |= vk::AccessFlags2::MEMORY_READ.as_raw()
    }
    if value & write != 0 {
        out |= vk::AccessFlags2::MEMORY_WRITE.as_raw()
    }
    out
}
fn layout(
    image: u64,
    value: &mut p::VkImageLayout,
    synthetic: &impl Fn(u64) -> bool,
) -> Result<()> {
    if value.0 == vk::ImageLayout::VIDEO_DECODE_DST_KHR.as_raw()
        || value.0 == vk::ImageLayout::VIDEO_DECODE_DPB_KHR.as_raw()
    {
        if !synthetic(image) {
            return Err(Rejected("video layout on non-synthetic image"));
        }
        value.0 = vk::ImageLayout::GENERAL.as_raw();
    }
    Ok(())
}
/// Keep the original recording untouched. Opaque barrier extensions are refused
/// on the synthetic path because they may contain untranslatable synchronization.
pub fn lower(original: &Barrier, synthetic: impl Fn(u64) -> bool) -> Result<Barrier> {
    let mut b = original.clone();
    match &mut b {
        Barrier::Legacy(v) => {
            if v.memory_barrier_count as usize != v.p_memory_barriers.len()
                || v.buffer_memory_barrier_count as usize != v.p_buffer_memory_barriers.len()
                || v.image_memory_barrier_count as usize != v.p_image_memory_barriers.len()
            {
                return Err(Rejected("barrier count mismatch"));
            }
            v.src_stage_mask.0 = stage(u64::from(v.src_stage_mask.0)) as u32;
            v.dst_stage_mask.0 = stage(u64::from(v.dst_stage_mask.0)) as u32;
            for m in &mut v.p_memory_barriers {
                if !m.p_next.is_empty() {
                    return Err(Rejected("unsupported memory barrier extension"));
                }
                m.src_access_mask.0 = access(u64::from(m.src_access_mask.0)) as u32;
                m.dst_access_mask.0 = access(u64::from(m.dst_access_mask.0)) as u32;
            }
            for m in &mut v.p_buffer_memory_barriers {
                if !m.p_next.is_empty() {
                    return Err(Rejected("unsupported buffer barrier extension"));
                }
                m.src_access_mask.0 = access(u64::from(m.src_access_mask.0)) as u32;
                m.dst_access_mask.0 = access(u64::from(m.dst_access_mask.0)) as u32;
            }
            for m in &mut v.p_image_memory_barriers {
                if !m.p_next.is_empty() {
                    return Err(Rejected("unsupported image barrier extension"));
                }
                m.src_access_mask.0 = access(u64::from(m.src_access_mask.0)) as u32;
                m.dst_access_mask.0 = access(u64::from(m.dst_access_mask.0)) as u32;
                layout(m.image.0, &mut m.old_layout, &synthetic)?;
                layout(m.image.0, &mut m.new_layout, &synthetic)?;
            }
        }
        Barrier::Sync2(v) => {
            let info = v
                .p_dependency_info
                .as_deref_mut()
                .ok_or(Rejected("missing dependency info"))?;
            if !info.p_next.is_empty()
                || info.memory_barrier_count as usize != info.p_memory_barriers.len()
                || info.buffer_memory_barrier_count as usize != info.p_buffer_memory_barriers.len()
                || info.image_memory_barrier_count as usize != info.p_image_memory_barriers.len()
            {
                return Err(Rejected("unsupported dependency info"));
            }
            macro_rules! masks {
                ($m:expr) => {{
                    let m = $m;
                    if !m.p_next.is_empty() {
                        return Err(Rejected("unsupported barrier extension"));
                    }
                    m.src_stage_mask.0 = stage(m.src_stage_mask.0);
                    m.dst_stage_mask.0 = stage(m.dst_stage_mask.0);
                    m.src_access_mask.0 = access(m.src_access_mask.0);
                    m.dst_access_mask.0 = access(m.dst_access_mask.0);
                }};
            }
            for m in &mut info.p_memory_barriers {
                masks!(m)
            }
            for m in &mut info.p_buffer_memory_barriers {
                masks!(m)
            }
            for m in &mut info.p_image_memory_barriers {
                masks!(&mut *m);
                layout(m.image.0, &mut m.old_layout, &synthetic)?;
                layout(m.image.0, &mut m.new_layout, &synthetic)?;
            }
        }
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn barrier(cb: u64, marker: u64) -> Barrier {
        let mut a = p::VkCmdPipelineBarrierArgs::default();
        a.command_buffer = p::VkCommandBuffer(cb);
        a.buffer_memory_barrier_count = 1;
        a.p_buffer_memory_barriers = vec![p::VkBufferMemoryBarrier {
            offset: marker,
            ..Default::default()
        }];
        Barrier::Legacy(a)
    }
    fn marker(b: &Barrier) -> u64 {
        match b {
            Barrier::Legacy(v) => v.p_buffer_memory_barriers[0].offset,
            _ => panic!(),
        }
    }
    fn decode(id: u64) -> Decode {
        use crate::native_video::{Configuration, H264Picture, Picture, Profile, Session};
        Decode {
            begin_before: true,
            reset_before: false,
            begin_references: Arc::from([]),
            session: Arc::new(Session {
                id: 1,
                profile: Profile { idc: 100 },
                extent: vk::Extent2D {
                    width: 32,
                    height: 32,
                },
                dpb_slots: 4,
                references: 4,
            }),
            configuration: Configuration {
                sps: Arc::from([0x67]),
                pps: Arc::from([0x68]),
            },
            source_buffer: id,
            source_offset: 0,
            source_range: 1,
            slice_offsets: Arc::from([0]),
            destination: Picture {
                image_view: 9,
                layer: 0,
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 32,
                    height: 32,
                },
            },
            setup: None,
            references: Arc::from([]),
            h264: H264Picture {
                idr_pic_flag: true,
                field_pic_flag: false,
                bottom_field_flag: false,
                is_reference: true,
                is_intra: true,
                frame_num: 0,
                pic_order_cnt: [0, 0],
                idr_pic_id: 0,
                sps_id: 0,
                pps_id: 0,
            },
        }
    }
    #[test]
    fn native_scope_after_video_closes_before_next_decode_boundary() {
        let mut plans = Plans::default();
        plans.begin(7).unwrap();
        plans.begin_scope(7).unwrap();
        plans.end_scope(7).unwrap();
        let mut begin = p::VkCmdBeginRenderingArgs::default();
        begin.command_buffer = p::VkCommandBuffer(7);
        plans
            .native(sv_gpu_entry::RecordedCall::VkCmdBeginRendering(Box::new(
                begin,
            )))
            .unwrap();
        assert!(plans.begin_scope(7).is_err());
    }
    #[test]
    fn native_render_scope_and_barrier_remain_in_one_owned_recording() {
        let mut plans = Plans::default();
        plans.begin(7).unwrap();
        let mut begin = p::VkCmdBeginRenderingArgs::default();
        begin.command_buffer = p::VkCommandBuffer(7);
        plans
            .native(sv_gpu_entry::RecordedCall::VkCmdBeginRendering(Box::new(
                begin,
            )))
            .unwrap();
        plans.barrier(barrier(7, 3)).unwrap();
        let mut end = p::VkCmdEndRenderingArgs::default();
        end.command_buffer = p::VkCommandBuffer(7);
        plans
            .native(sv_gpu_entry::RecordedCall::VkCmdEndRendering(Box::new(end)))
            .unwrap();
        let End::Ordinary(steps) = plans.end(7).unwrap() else {
            panic!("ordinary scope became video");
        };
        assert!(matches!(
            steps.as_slice(),
            [Step::Native(_), Step::Barrier(_), Step::Native(_)]
        ));
    }
    #[test]
    fn mixed_snapshot_keeps_original_owned_state_after_editable_reset() {
        let mut plans = Plans::default();
        plans.begin(7).unwrap();
        let mut bind = p::VkCmdBindVertexBuffersArgs::default();
        bind.command_buffer = p::VkCommandBuffer(7);
        bind.binding_count = 1;
        bind.p_buffers = vec![p::VkBuffer(81)];
        bind.p_offsets = vec![16];
        plans
            .native(sv_gpu_entry::RecordedCall::VkCmdBindVertexBuffers(
                Box::new(bind),
            ))
            .unwrap();
        plans.begin_scope(7).unwrap();
        plans.decode(7, decode(11)).unwrap();
        plans.end_scope(7).unwrap();
        let End::Video(snapshot) = plans.end(7).unwrap() else {
            panic!("missing mixed snapshot");
        };
        plans.reset(7);
        plans.begin(7).unwrap();
        let Step::Native(call) = &snapshot[0] else {
            panic!("state order changed");
        };
        let sv_gpu_entry::RecordedCall::VkCmdBindVertexBuffers(args) = call.as_ref() else {
            panic!("state kind changed");
        };
        assert_eq!(args.p_buffers[0].0, 81);
        assert_eq!(args.p_offsets, [16]);
        assert!(matches!(&snapshot[1], Step::Decode(job) if job.source_buffer == 11));
    }
    #[test]
    fn events_after_last_decode_are_not_forwarded_to_native_noop_wait() {
        let mut plans = Plans::default();
        plans.begin(7).unwrap();
        plans.begin_scope(7).unwrap();
        plans.decode(7, decode(11)).unwrap();
        plans.end_scope(7).unwrap();
        let mut wait = p::VkCmdWaitEvents2Args::default();
        wait.command_buffer = p::VkCommandBuffer(7);
        plans
            .native(sv_gpu_entry::RecordedCall::VkCmdWaitEvents2(Box::new(wait)))
            .unwrap();
        assert!(plans.end(7).is_err());
    }
    #[test]
    fn secondary_event_history_prevents_a_later_video_cut() {
        let mut plans = Plans::default();
        plans.allocated_secondary(3, &[8]).unwrap();
        plans.begin(8).unwrap();
        plans
            .set_begin_info(8, p::VkCommandBufferBeginInfo::default())
            .unwrap();
        plans.note_ordinary_command(8, "vkCmdWaitEvents2");
        plans.end(8).unwrap();
        plans.begin(7).unwrap();
        let mut execute = p::VkCmdExecuteCommandsArgs::default();
        execute.command_buffer = p::VkCommandBuffer(7);
        execute.command_buffer_count = 1;
        execute.p_command_buffers = vec![p::VkCommandBuffer(8)];
        plans
            .native(sv_gpu_entry::RecordedCall::VkCmdExecuteCommands(Box::new(
                execute,
            )))
            .unwrap();
        assert!(plans.begin_scope(7).is_err());
    }
    #[test]
    fn secondary_execution_rejects_unfinished_and_poisoned_recordings() {
        let mut plans = Plans::default();
        plans.allocated_secondary(3, &[8]).unwrap();
        plans.begin(7).unwrap();
        plans.begin(8).unwrap();
        assert!(plans.executed(7, &[8]).is_err());
        plans.end(8).unwrap();
        plans.poison(8, "missing native implementation");
        assert!(plans.executed(7, &[8]).is_err());
    }
    #[test]
    fn nested_secondary_event_metadata_reaches_primary_cut_validation() {
        let mut plans = Plans::default();
        plans.allocated_secondary(3, &[8, 9]).unwrap();
        plans.begin(9).unwrap();
        plans.note_ordinary_command(9, "vkCmdWaitEvents2");
        plans.end(9).unwrap();
        plans.begin(8).unwrap();
        plans.executed(8, &[9]).unwrap();
        plans.end(8).unwrap();
        plans.begin(7).unwrap();
        plans.executed(7, &[8]).unwrap();
        assert!(plans.begin_scope(7).is_err());
    }
    #[test]
    fn secondary_snapshot_retains_exact_calls_begin_and_generation_after_reuse() {
        let mut plans = Plans::default();
        plans.allocated_secondary(3, &[8]).unwrap();
        plans.begin(8).unwrap();
        let mut begin = p::VkCommandBufferBeginInfo::default();
        begin.flags = p::VkCommandBufferUsageFlags(2);
        plans.set_begin_info(8, begin).unwrap();
        assert!(plans.captures_native(8));
        assert!(matches!(
            plans.barrier(barrier(8, 42)).unwrap(),
            BarrierAction::Deferred
        ));
        let mut wait = p::VkCmdWaitEvents2Args::default();
        wait.command_buffer = p::VkCommandBuffer(8);
        plans
            .native(sv_gpu_entry::RecordedCall::VkCmdWaitEvents2(Box::new(wait)))
            .unwrap();
        let End::Ordinary(replay) = plans.end(8).unwrap() else {
            panic!()
        };
        assert_eq!(replay.len(), 2);
        let snapshot = plans.secondary_snapshot(8).unwrap();
        assert_eq!(snapshot.begin.flags.0, 2);
        assert_eq!(snapshot.steps.len(), 2);
        assert!(snapshot.has_events);
        assert!(matches!(&snapshot.steps[0], Step::Barrier(b) if marker(b) == 42));
        let Step::Native(saved) = &snapshot.steps[1] else {
            panic!()
        };
        let Step::Native(replayed) = &replay[1] else {
            panic!()
        };
        assert!(Arc::ptr_eq(saved, replayed));
        plans.begin(8).unwrap();
        plans
            .set_begin_info(8, p::VkCommandBufferBeginInfo::default())
            .unwrap();
        plans.end(8).unwrap();
        assert_ne!(
            snapshot.generation,
            plans.secondary_snapshot(8).unwrap().generation
        );
        plans.free(&[8]);
        assert_eq!(snapshot.begin.flags.0, 2);
        assert_eq!(snapshot.steps.len(), 2);
    }
    #[test]
    fn nested_execute_owns_child_generation_and_event_history() {
        let mut plans = Plans::default();
        plans.allocated_secondary(3, &[8, 9]).unwrap();
        plans.begin(9).unwrap();
        plans
            .set_begin_info(9, p::VkCommandBufferBeginInfo::default())
            .unwrap();
        let mut wait = p::VkCmdWaitEvents2Args::default();
        wait.command_buffer = p::VkCommandBuffer(9);
        plans
            .native(sv_gpu_entry::RecordedCall::VkCmdWaitEvents2(Box::new(wait)))
            .unwrap();
        plans.end(9).unwrap();
        let child_generation = plans.secondary_snapshot(9).unwrap().generation;
        for (wire, child) in [(8, 9), (7, 8)] {
            plans.begin(wire).unwrap();
            plans
                .set_begin_info(wire, p::VkCommandBufferBeginInfo::default())
                .unwrap();
            let mut execute = p::VkCmdExecuteCommandsArgs::default();
            execute.command_buffer = p::VkCommandBuffer(wire);
            execute.command_buffer_count = 1;
            execute.p_command_buffers = vec![p::VkCommandBuffer(child)];
            plans
                .native(sv_gpu_entry::RecordedCall::VkCmdExecuteCommands(Box::new(
                    execute,
                )))
                .unwrap();
            if wire == 8 {
                plans.end(wire).unwrap();
            }
        }
        plans.reset(9);
        plans.reset(8);
        assert!(plans.begin_scope(7).is_err());
        let Step::Execute { secondaries, .. } = &plans.buffers[&7].steps[0] else {
            panic!()
        };
        let Step::Execute {
            secondaries: children,
            ..
        } = &secondaries[0].steps[0]
        else {
            panic!()
        };
        assert_eq!(children[0].generation, child_generation);
        assert!(children[0].has_events);
    }
    #[test]
    fn secondary_reconstruction_rejects_missing_begin_and_uncaptured_calls() {
        let mut plans = Plans::default();
        plans.allocated_secondary(3, &[8]).unwrap();
        plans.begin(8).unwrap();
        plans.end(8).unwrap();
        assert!(plans.secondary_snapshot(8).is_err());
        plans.begin(8).unwrap();
        assert!(plans.prepare_ordinary(8).is_err());
        assert!(plans.end(8).is_err());
        plans.begin(8).unwrap();
        assert!(plans.begin_scope(8).is_err());
    }
    fn record_wait(plans: &mut Plans, wire: u64) {
        let mut wait = p::VkCmdWaitEvents2Args::default();
        wait.command_buffer = p::VkCommandBuffer(wire);
        plans
            .native(sv_gpu_entry::RecordedCall::VkCmdWaitEvents2(Box::new(wait)))
            .unwrap();
    }
    #[test]
    fn event_only_primary_routes_only_after_explicit_activation() {
        for enabled in [false, true] {
            let mut plans = Plans::default();
            if enabled {
                plans.enable_event_lowering();
            }
            plans.begin(7).unwrap();
            plans
                .set_begin_info(7, p::VkCommandBufferBeginInfo::default())
                .unwrap();
            plans.mark_video_incompatible(7).unwrap();
            assert!(plans.captures_native(7));
            record_wait(&mut plans, 7);
            assert_eq!(plans.is_video(7), enabled);
            let end = plans.end(7).unwrap();
            if enabled {
                let End::Video(snapshot) = end else { panic!() };
                assert_eq!(snapshot.len(), 1);
                assert_eq!(plans.video(7).unwrap().unwrap().len(), 1);
            } else {
                assert!(matches!(end, End::Ordinary(_)));
            }
            assert!(plans.begin_info(7).is_ok());
        }
    }
    #[test]
    fn activated_event_history_can_span_decode_cuts() {
        let mut plans = Plans::default();
        plans.enable_event_lowering();
        plans.begin(7).unwrap();
        record_wait(&mut plans, 7);
        plans.begin_scope(7).unwrap();
        plans.decode(7, decode(11)).unwrap();
        plans.end_scope(7).unwrap();
        record_wait(&mut plans, 7);
        let End::Video(snapshot) = plans.end(7).unwrap() else {
            panic!()
        };
        assert!(matches!(
            &snapshot[..],
            [Step::Native(_), Step::Decode(_), Step::Native(_)]
        ));
    }
    #[test]
    fn activated_nested_secondary_events_route_event_only_primary() {
        let mut plans = Plans::default();
        plans.enable_event_lowering();
        plans.allocated_secondary(3, &[8, 9]).unwrap();
        plans.begin(9).unwrap();
        plans
            .set_begin_info(9, p::VkCommandBufferBeginInfo::default())
            .unwrap();
        record_wait(&mut plans, 9);
        assert!(matches!(plans.end(9).unwrap(), End::Ordinary(_)));
        for (wire, child) in [(8, 9), (7, 8)] {
            plans.begin(wire).unwrap();
            plans
                .set_begin_info(wire, p::VkCommandBufferBeginInfo::default())
                .unwrap();
            let mut execute = p::VkCmdExecuteCommandsArgs::default();
            execute.command_buffer = p::VkCommandBuffer(wire);
            execute.command_buffer_count = 1;
            execute.p_command_buffers = vec![p::VkCommandBuffer(child)];
            plans
                .native(sv_gpu_entry::RecordedCall::VkCmdExecuteCommands(Box::new(
                    execute,
                )))
                .unwrap();
            if wire == 8 {
                assert!(!plans.is_video(wire));
                assert!(matches!(plans.end(wire).unwrap(), End::Ordinary(_)));
            }
        }
        assert!(plans.is_video(7));
        assert!(matches!(plans.end(7).unwrap(), End::Video(_)));
    }
    #[test]
    fn incompatible_video_metadata_keeps_capture_but_rejects_video_scope() {
        let mut plans = Plans::default();
        plans.enable_event_lowering();
        plans.begin(7).unwrap();
        plans.mark_video_incompatible(7).unwrap();
        assert!(plans.captures_native(7));
        assert!(plans.begin_scope(7).is_err());
    }
    #[test]
    fn late_activation_cannot_recover_already_replayed_event_calls() {
        let mut plans = Plans::default();
        plans.begin(7).unwrap();
        record_wait(&mut plans, 7);
        assert!(matches!(plans.end(7).unwrap(), End::Ordinary(_)));
        plans.enable_event_lowering();
        assert!(plans.is_video(7));
        assert!(plans.video(7).is_err());
    }
    #[test]
    fn uncaptured_primary_prefix_cannot_later_route_events() {
        let mut plans = Plans::default();
        plans.enable_event_lowering();
        plans.begin(7).unwrap();
        plans.prepare_ordinary(7).unwrap();
        plans.note_ordinary_command(7, "vkCmdSetEvent2");
        assert!(plans.end(7).is_err());
        plans.begin(7).unwrap();
        record_wait(&mut plans, 7);
        assert!(plans.prepare_ordinary(7).is_err());
        assert!(plans.end(7).is_err());
    }
    #[test]
    fn ordinary_first_command_flushes_deferred_barriers_once_in_original_order() {
        let mut plans = Plans::default();
        plans.begin(7).unwrap();
        assert!(matches!(
            plans.barrier(barrier(7, 1)).unwrap(),
            BarrierAction::Deferred
        ));
        plans.barrier(barrier(7, 2)).unwrap();
        let flushed = plans.prepare_ordinary(7).unwrap();
        assert_eq!(flushed.iter().map(marker).collect::<Vec<_>>(), vec![1, 2]);
        assert!(plans.prepare_ordinary(7).unwrap().is_empty());
        assert!(matches!(
            plans.barrier(barrier(7, 3)).unwrap(),
            BarrierAction::Forward(_)
        ));
        assert!(plans.begin_scope(7).is_err());
        assert!(plans.poisoned(7).is_some());
        assert!(plans.end(7).is_err());
    }
    #[test]
    fn video_plan_preserves_barriers_decodes_and_multiple_scopes_across_reset() {
        let mut plans = Plans::default();
        plans.begin(7).unwrap();
        plans.barrier(barrier(7, 1)).unwrap();
        plans.begin_scope(7).unwrap();
        plans.decode(7, decode(11)).unwrap();
        plans.end_scope(7).unwrap();
        plans.barrier(barrier(7, 2)).unwrap();
        plans.begin_scope(7).unwrap();
        plans.decode(7, decode(12)).unwrap();
        plans.end_scope(7).unwrap();
        plans.barrier(barrier(7, 3)).unwrap();
        let End::Video(snapshot) = plans.end(7).unwrap() else {
            panic!()
        };
        let values: Vec<u64> = snapshot
            .iter()
            .map(|step| match step {
                Step::Barrier(b) => marker(b),
                Step::Decode(d) => d.source_buffer,
                Step::Native(_) | Step::Execute { .. } => {
                    panic!("unexpected native call in barrier-only test")
                }
            })
            .collect();
        assert_eq!(values, vec![1, 11, 2, 12, 3]);
        assert_eq!(plans.video(7).unwrap().unwrap().len(), 5);
        plans.reset(7);
        assert!(plans.video(7).is_err());
        plans.begin(7).unwrap();
        plans.free(&[7]);
        assert!(plans.end(7).is_err());
        assert_eq!(snapshot.len(), 5);
    }
    #[test]
    fn ordinary_after_video_poison_cannot_be_cleared_by_end() {
        let mut plans = Plans::default();
        plans.begin(7).unwrap();
        plans.begin_scope(7).unwrap();
        plans.end_scope(7).unwrap();
        assert!(plans.prepare_ordinary(7).is_err());
        assert!(plans.end(7).is_err());
        assert!(plans.video(7).is_err());
        plans.begin(7).unwrap();
        assert!(plans.poisoned(7).is_none());
        assert!(matches!(plans.end(7).unwrap(), End::Ordinary(_)));
    }
    #[test]
    fn lowering_keeps_originals_and_only_translates_synthetic_decode_layouts() {
        let mut info = p::VkDependencyInfo::default();
        info.image_memory_barrier_count = 1;
        info.p_image_memory_barriers = vec![p::VkImageMemoryBarrier2 {
            image: p::VkImage(9),
            src_stage_mask: p::VkPipelineStageFlags2(
                (vk::PipelineStageFlags2::VIDEO_DECODE_KHR | vk::PipelineStageFlags2::COPY)
                    .as_raw(),
            ),
            src_access_mask: p::VkAccessFlags2(
                (vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR | vk::AccessFlags2::TRANSFER_WRITE)
                    .as_raw(),
            ),
            old_layout: p::VkImageLayout(vk::ImageLayout::VIDEO_DECODE_DST_KHR.as_raw()),
            new_layout: p::VkImageLayout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL.as_raw()),
            src_queue_family_index: 3,
            dst_queue_family_index: 5,
            ..Default::default()
        }];
        let original = Barrier::Sync2(p::VkCmdPipelineBarrier2Args {
            command_buffer: p::VkCommandBuffer(7),
            p_dependency_info: Some(Box::new(info)),
        });
        assert!(lower(&original, |_| false).is_err());
        let Barrier::Sync2(result) = lower(&original, |id| id == 9).unwrap() else {
            panic!()
        };
        let image = &result
            .p_dependency_info
            .as_ref()
            .unwrap()
            .p_image_memory_barriers[0];
        assert_eq!(image.old_layout.0, vk::ImageLayout::GENERAL.as_raw());
        assert_eq!(
            image.new_layout.0,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL.as_raw()
        );
        assert_eq!(
            image.src_stage_mask.0,
            (vk::PipelineStageFlags2::ALL_COMMANDS | vk::PipelineStageFlags2::COPY).as_raw()
        );
        assert_eq!(
            image.src_access_mask.0,
            (vk::AccessFlags2::MEMORY_WRITE | vk::AccessFlags2::TRANSFER_WRITE).as_raw()
        );
        assert_eq!(
            (image.src_queue_family_index, image.dst_queue_family_index),
            (3, 5)
        );
        let Barrier::Sync2(original) = original else {
            panic!()
        };
        assert_eq!(
            original.p_dependency_info.unwrap().p_image_memory_barriers[0]
                .old_layout
                .0,
            vk::ImageLayout::VIDEO_DECODE_DST_KHR.as_raw()
        );
    }
}
#[cfg(test)]
mod pool_tests {
    use super::*;
    #[test]
    fn pool_reset_and_destroy_only_clear_their_own_command_buffers() {
        let mut p = Plans::default();
        p.allocated(10, &[1, 2]).unwrap();
        p.allocated(11, &[3]).unwrap();
        assert!(p.allocated(12, &[4, 1]).is_err());
        assert!(p.pool_buffers(12).is_empty());
        p.begin(1).unwrap();
        p.begin(2).unwrap();
        p.begin(3).unwrap();
        p.reset_pool(10);
        assert!(!p.contains(1));
        assert!(!p.contains(2));
        assert!(p.contains(3));
        assert_eq!(p.pool_buffers(10).len(), 2);
        p.begin(1).unwrap();
        p.destroy_pool(10);
        assert!(!p.contains(1));
        assert!(p.pool_buffers(10).is_empty());
        assert!(p.contains(3));
        p.free(&[3]);
        assert!(!p.contains(3));
        assert!(p.pool_buffers(11).is_empty());
    }
}
#[cfg(test)]
mod secondary_tests {
    use super::*;
    #[test]
    fn secondary_buffers_capture_native_recording_but_refuse_video_staging() {
        let mut plans = Plans::default();
        plans.allocated_secondary(10, &[7]).unwrap();
        plans.begin(7).unwrap();
        assert!(!plans.allows_video(7));
        assert!(plans.begin_scope(7).is_err());
        assert!(plans.poisoned(7).is_some());
        plans.free(&[7]);
        plans.allocated(10, &[7]).unwrap();
        plans.begin(7).unwrap();
        assert!(plans.allows_video(7));
        plans.begin_scope(7).unwrap();
    }
}
