//! Synthetic Vulkan Video objects for the hardware H.264 path.
//!
//! This module never forwards synthetic handles to a Vulkan driver. The caller
//! owns handle-table translation and queue synchronization. Recorded jobs retain
//! all session/configuration state after application handles are destroyed.
use ash::vk;
use std::{collections::HashMap, sync::Arc};

pub type VideoResult<T> = Result<T, vk::Result>;
const FORMAT: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
fn unsupported<T>() -> VideoResult<T> {
    Err(vk::Result::ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR)
}
fn invalid<T>() -> VideoResult<T> {
    Err(vk::Result::ERROR_INITIALIZATION_FAILED)
}

#[derive(Clone, Copy, Default)]
pub struct Ready {
    pub hardware_h264: bool,
    /// Set only after the joined NV12 import and transfer path is usable.
    pub joined_nv12_import: bool,
}
impl Ready {
    pub fn available(self) -> bool {
        self.hardware_h264 && self.joined_nv12_import
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub idc: u32,
}
impl Profile {
    pub fn validate(
        base: &vk::VideoProfileInfoKHR<'_>,
        h264: &vk::VideoDecodeH264ProfileInfoKHR<'_>,
    ) -> VideoResult<Self> {
        if base.video_codec_operation != vk::VideoCodecOperationFlagsKHR::DECODE_H264
            || base.chroma_subsampling != vk::VideoChromaSubsamplingFlagsKHR::TYPE_420
            || base.luma_bit_depth != vk::VideoComponentBitDepthFlagsKHR::TYPE_8
            || base.chroma_bit_depth != vk::VideoComponentBitDepthFlagsKHR::TYPE_8
            || h264.picture_layout != vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE
        {
            return unsupported();
        }
        let idc = h264.std_profile_idc;
        if ![66, 77, 100].contains(&idc) {
            return unsupported();
        }
        Ok(Self { idc })
    }
}

#[derive(Debug)]
pub struct Session {
    pub id: u64,
    pub profile: Profile,
    pub extent: vk::Extent2D,
    pub dpb_slots: u32,
    pub references: u32,
}
#[derive(Clone)]
struct Sps {
    nal: Arc<[u8]>,
    profile: u32,
}
#[derive(Clone)]
struct Pps {
    nal: Arc<[u8]>,
    sps: u8,
}
#[derive(Clone, Default)]
struct ParameterSets {
    sps: HashMap<u8, Sps>,
    pps: HashMap<u8, Pps>,
}
struct Parameters {
    session: Arc<Session>,
    sets: Arc<ParameterSets>,
    sequence: u32,
    initialized: bool,
    max_sps: usize,
    max_pps: usize,
}
#[derive(Clone)]
pub struct Configuration {
    pub sps: Arc<[u8]>,
    pub pps: Arc<[u8]>,
}
/// Parent resolves these stable resource IDs against its object table, retaining
/// the corresponding buffer/image allocations until queue completion.
#[derive(Clone, Debug)]
pub struct Picture {
    pub image_view: u64,
    pub layer: u32,
    pub offset: vk::Offset2D,
    pub extent: vk::Extent2D,
}
#[derive(Clone, Debug)]
pub struct Reference {
    pub h264: H264Reference,
    pub slot: u32,
    pub picture: Picture,
}
#[derive(Clone)]
pub struct Decode {
    pub h264: H264Picture,
    pub reset_before: bool,
    pub begin_references: Arc<[Reference]>,
    pub session: Arc<Session>,
    pub configuration: Configuration,
    pub source_buffer: u64,
    pub source_offset: u64,
    pub source_range: u64,
    pub slice_offsets: Arc<[u32]>,
    pub destination: Picture,
    pub setup: Option<Reference>,
    pub references: Arc<[Reference]>,
}
struct Scope {
    reset: bool,
    begin_references: Arc<[Reference]>,
    session: Arc<Session>,
    sets: Arc<ParameterSets>,
    jobs: Vec<Decode>,
}
#[derive(Default)]
pub struct NativeVideo {
    ready: Ready,
    next: u64,
    sessions: HashMap<u64, Arc<Session>>,
    parameters: HashMap<u64, Parameters>,
    scopes: HashMap<u64, Scope>,
    recordings: HashMap<u64, Arc<[Decode]>>,
}
impl NativeVideo {
    pub fn new(ready: Ready) -> Self {
        Self {
            ready,
            ..Self::default()
        }
    }
    pub fn available(&self) -> bool {
        self.ready.available()
    }
    fn id(&mut self) -> VideoResult<u64> {
        self.next = self
            .next
            .checked_add(1)
            .ok_or(vk::Result::ERROR_TOO_MANY_OBJECTS)?;
        Ok(self.next)
    }
    pub fn capabilities(
        &self,
        _profile: Profile,
    ) -> VideoResult<vk::VideoCapabilitiesKHR<'static>> {
        if !self.available() || ![66, 77, 100].contains(&_profile.idc) {
            return unsupported();
        }
        Ok(vk::VideoCapabilitiesKHR::default()
            .flags(vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES)
            .min_bitstream_buffer_offset_alignment(1)
            .min_bitstream_buffer_size_alignment(1)
            .picture_access_granularity(vk::Extent2D {
                width: 16,
                height: 16,
            })
            .min_coded_extent(vk::Extent2D {
                width: 16,
                height: 16,
            })
            .max_coded_extent(vk::Extent2D {
                width: 4096,
                height: 4096,
            })
            .max_dpb_slots(16)
            .max_active_reference_pictures(16))
    }
    pub fn create_session(
        &mut self,
        profile: Profile,
        info: &vk::VideoSessionCreateInfoKHR<'_>,
    ) -> VideoResult<u64> {
        self.capabilities(profile)?;
        if !info.flags.is_empty()
            || info.picture_format != FORMAT
            || info.reference_picture_format != FORMAT
            || !extent_valid(info.max_coded_extent)
            || info.max_dpb_slots > 16
            || info.max_active_reference_pictures > 16
            || info.max_active_reference_pictures > info.max_dpb_slots
        {
            return unsupported();
        }
        let id = self.id()?;
        self.sessions.insert(
            id,
            Arc::new(Session {
                id,
                profile,
                extent: info.max_coded_extent,
                dpb_slots: info.max_dpb_slots,
                references: info.max_active_reference_pictures,
            }),
        );
        Ok(id)
    }
    /// VideoToolbox allocates internal memory. Vulkan reports zero requirements.
    pub fn memory_requirements(
        &self,
        id: u64,
    ) -> VideoResult<&'static [vk::VideoSessionMemoryRequirementsKHR<'static>]> {
        self.sessions
            .get(&id)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        Ok(&[])
    }
    pub fn bind_memory(
        &self,
        id: u64,
        binds: &[vk::BindVideoSessionMemoryInfoKHR<'_>],
    ) -> VideoResult<()> {
        self.memory_requirements(id)?;
        if binds.is_empty() { Ok(()) } else { invalid() }
    }
    pub fn session(&self, id: u64) -> VideoResult<Arc<Session>> {
        self.sessions
            .get(&id)
            .cloned()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)
    }
    pub fn destroy_session(&mut self, id: u64) {
        self.sessions.remove(&id);
    }
    pub fn destroy_parameters(&mut self, id: u64) {
        self.parameters.remove(&id);
    }
    pub fn create_parameters(
        &mut self,
        session: u64,
        template: Option<u64>,
        max_sps: usize,
        max_pps: usize,
    ) -> VideoResult<u64> {
        if max_sps == 0 || max_sps > 32 || max_pps == 0 || max_pps > 256 {
            return invalid();
        }
        let session = self
            .sessions
            .get(&session)
            .cloned()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let sets = if let Some(id) = template {
            let old = self
                .parameters
                .get(&id)
                .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
            if old.session.id != session.id {
                return invalid();
            }
            old.sets.clone()
        } else {
            Arc::default()
        };
        if sets.sps.len() > max_sps || sets.pps.len() > max_pps {
            return invalid();
        }
        let id = self.id()?;
        self.parameters.insert(
            id,
            Parameters {
                session,
                sets,
                sequence: 0,
                initialized: false,
                max_sps,
                max_pps,
            },
        );
        Ok(id)
    }
    /// All pointed-to StdVideo data must be readable for this call. NAL encoding
    /// completes before return, so no borrowed StdVideo pointers are retained.
    pub unsafe fn initialize_parameters(
        &mut self,
        id: u64,
        sps: &[vk::native::StdVideoH264SequenceParameterSet],
        pps: &[vk::native::StdVideoH264PictureParameterSet],
    ) -> VideoResult<()> {
        if self
            .parameters
            .get(&id)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?
            .initialized
        {
            return invalid();
        }
        unsafe {
            self.update_parameters(id, 1, sps, pps)?;
        }
        self.parameters.get_mut(&id).unwrap().sequence = 0;
        Ok(())
    }
    pub unsafe fn update_parameters(
        &mut self,
        id: u64,
        sequence: u32,
        sps: &[vk::native::StdVideoH264SequenceParameterSet],
        pps: &[vk::native::StdVideoH264PictureParameterSet],
    ) -> VideoResult<()> {
        let p = self
            .parameters
            .get_mut(&id)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        if sequence
            != p.sequence
                .checked_add(1)
                .ok_or(vk::Result::ERROR_TOO_MANY_OBJECTS)?
        {
            return invalid();
        }
        let mut sets = (*p.sets).clone();
        for s in sps {
            if sets.sps.contains_key(&s.seq_parameter_set_id)
                || s.profile_idc != p.session.profile.idc
                || u32::from(s.max_num_ref_frames) > p.session.dpb_slots
                || s.flags.frame_mbs_only_flag() == 0
                || s.chroma_format_idc != 1
                || s.bit_depth_luma_minus8 != 0
                || s.bit_depth_chroma_minus8 != 0
            {
                return unsupported();
            }
            let width = s
                .pic_width_in_mbs_minus1
                .checked_add(1)
                .and_then(|v| v.checked_mul(16))
                .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
            let height = s
                .pic_height_in_map_units_minus1
                .checked_add(1)
                .and_then(|v| v.checked_mul(16))
                .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
            if width > p.session.extent.width || height > p.session.extent.height {
                return unsupported();
            }
            let nal = unsafe { crate::video_nv12::h264_parameters::sps_nal(s) }
                .map_err(|_| vk::Result::ERROR_INITIALIZATION_FAILED)?;
            sets.sps.insert(
                s.seq_parameter_set_id,
                Sps {
                    nal: nal.into(),
                    profile: s.profile_idc,
                },
            );
        }
        for v in pps {
            if sets.pps.contains_key(&v.pic_parameter_set_id) {
                return invalid();
            }
            let s = sets
                .sps
                .get(&v.seq_parameter_set_id)
                .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
            // pps_nal reads only these two SPS fields, never SPS pointers.
            let mut metadata: vk::native::StdVideoH264SequenceParameterSet =
                unsafe { std::mem::zeroed() };
            metadata.seq_parameter_set_id = v.seq_parameter_set_id;
            metadata.profile_idc = s.profile;
            let nal = unsafe { crate::video_nv12::h264_parameters::pps_nal(v, &metadata) }
                .map_err(|_| vk::Result::ERROR_INITIALIZATION_FAILED)?;
            sets.pps.insert(
                v.pic_parameter_set_id,
                Pps {
                    nal: nal.into(),
                    sps: v.seq_parameter_set_id,
                },
            );
        }
        if sets.sps.len() > p.max_sps || sets.pps.len() > p.max_pps {
            return invalid();
        }
        p.sets = Arc::new(sets);
        p.sequence = sequence;
        p.initialized = true;
        Ok(())
    }
    pub fn begin(&mut self, command: u64, session: u64, parameters: u64) -> VideoResult<()> {
        if self.scopes.contains_key(&command) || self.recordings.contains_key(&command) {
            return invalid();
        }
        let p = self
            .parameters
            .get(&parameters)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        if p.session.id != session || !self.sessions.contains_key(&session) {
            return invalid();
        }
        self.scopes.insert(
            command,
            Scope {
                reset: false,
                begin_references: Arc::from([]),
                session: p.session.clone(),
                sets: p.sets.clone(),
                jobs: vec![],
            },
        );
        Ok(())
    }
    pub fn configuration(&self, command: u64, sps: u8, pps: u8) -> VideoResult<Configuration> {
        let scope = self
            .scopes
            .get(&command)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let p = scope
            .sets
            .pps
            .get(&pps)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        if p.sps != sps {
            return invalid();
        }
        let s = scope
            .sets
            .sps
            .get(&sps)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        Ok(Configuration {
            sps: s.nal.clone(),
            pps: p.nal.clone(),
        })
    }
    pub fn record(&mut self, command: u64, mut job: Decode, sps: u8, pps: u8) -> VideoResult<()> {
        job.h264.validate()?;
        if job.h264.sps_id != sps || job.h264.pps_id != pps {
            return invalid();
        }
        job.configuration = self.configuration(command, sps, pps)?;
        let scope = self
            .scopes
            .get_mut(&command)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        if job.session.id != scope.session.id
            || job.source_buffer == 0
            || job.source_range == 0
            || job.source_offset.checked_add(job.source_range).is_none()
            || job.slice_offsets.is_empty()
            || job
                .slice_offsets
                .iter()
                .any(|&v| u64::from(v) >= job.source_range)
            || job.slice_offsets.windows(2).any(|v| v[0] >= v[1])
            || job.references.len() > scope.session.references as usize
        {
            return invalid();
        }
        validate_picture(&job.destination, &scope.session)?;
        let mut slots = std::collections::HashSet::new();
        for r in job.references.iter() {
            if r.slot >= scope.session.dpb_slots || !slots.insert(r.slot) {
                return invalid();
            }
            validate_picture(&r.picture, &scope.session)?;
        }
        if let Some(r) = &job.setup {
            if r.slot >= scope.session.dpb_slots {
                return invalid();
            }
            validate_picture(&r.picture, &scope.session)?;
        }
        job.reset_before = scope.reset;
        job.begin_references = scope.begin_references.clone();
        scope.reset = false;
        scope.jobs.push(job);
        Ok(())
    }
    pub fn end(&mut self, command: u64) -> VideoResult<()> {
        let scope = self
            .scopes
            .remove(&command)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        self.recordings.insert(command, scope.jobs.into());
        Ok(())
    }
    /// Clone on submit, including resubmission; reset/free alone discards recording.
    pub fn recording(&self, command: u64) -> VideoResult<Arc<[Decode]>> {
        if self.scopes.contains_key(&command) {
            return invalid();
        }
        self.recordings
            .get(&command)
            .cloned()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)
    }
    pub fn reset_command(&mut self, command: u64) {
        self.scopes.remove(&command);
        self.recordings.remove(&command);
    }
}
fn extent_valid(e: vk::Extent2D) -> bool {
    e.width >= 16
        && e.height >= 16
        && e.width <= 4096
        && e.height <= 4096
        && e.width % 2 == 0
        && e.height % 2 == 0
}
fn validate_picture(p: &Picture, s: &Session) -> VideoResult<()> {
    if p.image_view == 0
        || p.offset.x != 0
        || p.offset.y != 0
        || !extent_valid(p.extent)
        || p.extent.width > s.extent.width
        || p.extent.height > s.extent.height
    {
        return unsupported();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ready() -> NativeVideo {
        NativeVideo::new(Ready {
            hardware_h264: true,
            joined_nv12_import: true,
        })
    }
    fn info() -> vk::VideoSessionCreateInfoKHR<'static> {
        vk::VideoSessionCreateInfoKHR::default()
            .picture_format(FORMAT)
            .reference_picture_format(FORMAT)
            .max_coded_extent(vk::Extent2D {
                width: 1920,
                height: 1088,
            })
            .max_dpb_slots(4)
            .max_active_reference_pictures(4)
    }
    #[test]
    fn readiness_requires_both_native_components() {
        for (hardware_h264, joined_nv12_import) in [(false, false), (true, false), (false, true)] {
            let mut v = NativeVideo::new(Ready {
                hardware_h264,
                joined_nv12_import,
            });
            assert!(v.create_session(Profile { idc: 100 }, &info()).is_err());
        }
    }
    #[test]
    fn retained_session_survives_handle_destruction() {
        let mut v = ready();
        let id = v.create_session(Profile { idc: 100 }, &info()).unwrap();
        let p = v.create_parameters(id, None, 1, 1).unwrap();
        v.begin(42, id, p).unwrap();
        let retained = v.scopes.get(&42).unwrap().session.clone();
        v.destroy_session(id);
        v.destroy_parameters(p);
        assert_eq!(retained.id, id);
        v.end(42).unwrap();
        assert!(v.recording(42).unwrap().is_empty());
        assert!(v.recording(42).is_ok());
        v.reset_command(42);
        assert!(v.recording(42).is_err());
    }
    #[test]
    fn refuses_format_reference_overflow_and_allocations() {
        let mut v = ready();
        let mut i = info();
        i.picture_format = vk::Format::R8G8B8A8_UNORM;
        assert!(v.create_session(Profile { idc: 100 }, &i).is_err());
        i = info();
        i.max_active_reference_pictures = 5;
        assert!(v.create_session(Profile { idc: 100 }, &i).is_err());
        let id = v.create_session(Profile { idc: 100 }, &info()).unwrap();
        assert!(v.memory_requirements(id).unwrap().is_empty());
        assert!(v.bind_memory(id, &[]).is_ok());
        assert!(
            v.bind_memory(id, &[vk::BindVideoSessionMemoryInfoKHR::default()])
                .is_err()
        );
    }
    #[test]
    fn failed_update_preserves_sequence_and_snapshot() {
        let mut v = ready();
        let id = v.create_session(Profile { idc: 100 }, &info()).unwrap();
        let p = v.create_parameters(id, None, 1, 1).unwrap();
        assert!(unsafe { v.update_parameters(p, 2, &[], &[]) }.is_err());
        assert_eq!(v.parameters[&p].sequence, 0);
        assert!(unsafe { v.update_parameters(p, 1, &[], &[]) }.is_ok());
        assert!(unsafe { v.update_parameters(p, 1, &[], &[]) }.is_err());
        assert_eq!(v.parameters[&p].sequence, 1);
    }
}

/// Owned picture syntax used to compare explicit Vulkan DPB state with the
/// decoder's implicit reference state at submission time.
#[derive(Clone, Debug)]
pub struct H264Picture {
    pub idr_pic_flag: bool,
    pub field_pic_flag: bool,
    pub bottom_field_flag: bool,
    pub is_reference: bool,
    pub is_intra: bool,
    pub frame_num: u16,
    pub pic_order_cnt: [i32; 2],
    pub idr_pic_id: u16,
    pub sps_id: u8,
    pub pps_id: u8,
}
impl H264Picture {
    pub fn from_std(p: &vk::native::StdVideoDecodeH264PictureInfo) -> VideoResult<Self> {
        if p.flags.complementary_field_pair() != 0
            || p.flags._bitfield_1.get(6, 2) != 0
            || p.reserved1 != 0
            || p.reserved2 != 0
        {
            return unsupported();
        }
        let result = Self {
            idr_pic_flag: p.flags.IdrPicFlag() != 0,
            field_pic_flag: p.flags.field_pic_flag() != 0,
            bottom_field_flag: p.flags.bottom_field_flag() != 0,
            is_reference: p.flags.is_reference() != 0,
            is_intra: p.flags.is_intra() != 0,
            frame_num: p.frame_num,
            pic_order_cnt: p.PicOrderCnt,
            idr_pic_id: p.idr_pic_id,
            sps_id: p.seq_parameter_set_id,
            pps_id: p.pic_parameter_set_id,
        };
        result.validate()?;
        Ok(result)
    }
    fn validate(&self) -> VideoResult<()> {
        if self.field_pic_flag
            || self.bottom_field_flag
            || self.sps_id > 31
            || (self.idr_pic_flag && (!self.is_intra || !self.is_reference || self.frame_num != 0))
        {
            return unsupported();
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct H264Reference {
    pub frame_num: u16,
    pub pic_order_cnt: [i32; 2],
    pub long_term: bool,
}
impl H264Reference {
    pub fn from_std(r: &vk::native::StdVideoDecodeH264ReferenceInfo) -> VideoResult<Self> {
        if r.flags.top_field_flag() != 0
            || r.flags.bottom_field_flag() != 0
            || r.flags.is_non_existing() != 0
            || r.flags._bitfield_1.get(4, 4) != 0
            || r.reserved != 0
        {
            return unsupported();
        }
        Ok(Self {
            frame_num: r.FrameNum,
            pic_order_cnt: r.PicOrderCnt,
            long_term: r.flags.used_for_long_term_reference() != 0,
        })
    }
}
impl NativeVideo {
    pub fn begin_references(
        &mut self,
        command: u64,
        references: Arc<[Reference]>,
    ) -> VideoResult<()> {
        let scope = self
            .scopes
            .get_mut(&command)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        if !scope.jobs.is_empty() || references.len() > scope.session.dpb_slots as usize {
            return invalid();
        }
        let mut slots = std::collections::HashSet::new();
        for r in references.iter() {
            if r.slot >= scope.session.dpb_slots || !slots.insert(r.slot) {
                return invalid();
            }
            validate_picture(&r.picture, &scope.session)?;
        }
        scope.begin_references = references;
        Ok(())
    }
    pub fn reset_scope(&mut self, command: u64) -> VideoResult<()> {
        let scope = self
            .scopes
            .get_mut(&command)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        scope.reset = true;
        scope.begin_references = Arc::from([]);
        Ok(())
    }
}
#[cfg(test)]
mod tests_syntax {
    use super::*;
    #[test]
    fn progressive_picture_metadata_is_owned_and_fields_are_refused() {
        let mut p: vk::native::StdVideoDecodeH264PictureInfo = unsafe { std::mem::zeroed() };
        p.flags.set_IdrPicFlag(1);
        p.flags.set_is_intra(1);
        p.flags.set_is_reference(1);
        p.PicOrderCnt = [4, 4];
        let owned = H264Picture::from_std(&p).unwrap();
        p.PicOrderCnt = [9, 9];
        assert_eq!(owned.pic_order_cnt, [4, 4]);
        p.flags.set_field_pic_flag(1);
        assert!(H264Picture::from_std(&p).is_err());
        p.flags.set_field_pic_flag(0);
        p.flags.set_complementary_field_pair(1);
        assert!(H264Picture::from_std(&p).is_err());
    }
}
