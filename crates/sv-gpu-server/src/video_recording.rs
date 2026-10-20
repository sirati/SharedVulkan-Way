//! Owned wire recordings. Parent replays ordinary barriers before forwarding
//! the first ordinary command, and lowers video barriers only at submission.
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
struct Recording {
    mode: Mode,
    scope: bool,
    ended: bool,
    poison: Option<&'static str>,
    steps: Vec<Step>,
    finished: Option<Arc<[Step]>>,
}
impl Default for Recording {
    fn default() -> Self {
        Self {
            mode: Mode::Undecided,
            scope: false,
            ended: false,
            poison: None,
            steps: vec![],
            finished: None,
        }
    }
}
pub enum End {
    Ordinary(Vec<Barrier>),
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
}
impl Plans {
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
    pub fn contains(&self, wire: u64) -> bool {
        self.buffers.contains_key(&wire)
    }
    pub fn is_video(&self, wire: u64) -> bool {
        self.buffers
            .get(&wire)
            .is_some_and(|r| r.mode == Mode::Video)
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
        self.buffers.insert(wire, Recording::default());
        Ok(())
    }
    pub fn reset(&mut self, wire: u64) {
        self.buffers.remove(&wire);
    }
    pub fn free(&mut self, wires: &[u64]) {
        for wire in wires {
            self.allocations.remove(wire);
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
    pub fn barrier(&mut self, barrier: Barrier) -> Result<BarrierAction> {
        let r = self.recording(barrier.command())?;
        if r.mode == Mode::Ordinary {
            return Ok(BarrierAction::Forward(barrier));
        }
        r.steps.push(Step::Barrier(barrier));
        Ok(BarrierAction::Deferred)
    }
    /// Parent must replay the returned list successfully before this ordinary
    /// command is forwarded. Replay must bypass both interception hooks.
    pub fn prepare_ordinary(&mut self, wire: u64) -> Result<Vec<Barrier>> {
        let r = self.recording(wire)?;
        if r.mode == Mode::Video {
            r.poison = Some("ordinary command mixed with synthetic video");
            return Err(Rejected(r.poison.unwrap()));
        }
        r.mode = Mode::Ordinary;
        Ok(r.steps
            .drain(..)
            .map(|s| match s {
                Step::Barrier(b) => b,
                Step::Decode(_) => unreachable!(),
            })
            .collect())
    }
    pub fn begin_scope(&mut self, wire: u64) -> Result<()> {
        let r = self.recording(wire)?;
        if r.scope || r.mode == Mode::Ordinary {
            r.poison = Some("video coding scope mixed with ordinary recording");
            return Err(Rejected(r.poison.unwrap()));
        }
        r.mode = Mode::Video;
        r.scope = true;
        Ok(())
    }
    pub fn decode(&mut self, wire: u64, decode: Decode) -> Result<()> {
        let r = self.recording(wire)?;
        if !r.scope {
            r.poison = Some("decode outside video coding scope");
            return Err(Rejected(r.poison.unwrap()));
        }
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
        let r = self.recording(wire)?;
        if r.scope {
            r.poison = Some("command buffer ended inside video coding scope");
            return Err(Rejected(r.poison.unwrap()));
        }
        r.ended = true;
        if r.mode == Mode::Video {
            let steps: Arc<[Step]> = std::mem::take(&mut r.steps).into();
            r.finished = Some(steps.clone());
            Ok(End::Video(steps))
        } else {
            Ok(End::Ordinary(
                r.steps
                    .drain(..)
                    .map(|s| match s {
                        Step::Barrier(b) => b,
                        Step::Decode(_) => unreachable!(),
                    })
                    .collect(),
            ))
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
