//! Serialized, resolved native decode execution. This staging runtime requires
//! a validated independent picture after reset. Codec history and canonical
//! resource associations commit only after native completion and all target copies.
use crate::{
    native_video::{Decode, H264Reference, Picture, Reference, Session},
    native_video_image::Image,
    video_nv12::videotoolbox::{CompressedBytes, Decoder},
};
use std::{collections::HashMap, ops::Range, sync::Arc};

#[path = "native_video_dpb.rs"]
mod dpb;
#[path = "native_video_h264.rs"]
mod h264;
#[path = "native_video_lists.rs"]
mod lists;
#[path = "native_video_poc.rs"]
mod poc;

pub(crate) struct Resources {
    pub source: Arc<dyn CompressedBytes>,
    pub source_range: Range<usize>,
    pub destination: Arc<Image>,
    pub setup: Option<Arc<Image>>,
    pub references: Vec<Arc<Image>>,
    pub begin_references: Vec<Arc<Image>>,
}
#[derive(Clone)]
struct Slot {
    image: Arc<Image>,
    region: Picture,
}
struct State {
    session: Arc<Session>,
    decoder: Option<Decoder>,
    slots: HashMap<u32, u64>,
    associations: HashMap<u64, Slot>,
    codec: dpb::Dpb,
    history: poc::History,
    active_sps: Option<Vec<u8>>,
    poisoned: bool,
    // A failed accepted write cannot release allocations still used by native work.
    failed_images: Vec<Arc<Image>>,
}
#[derive(Default)]
pub(crate) struct Runtime {
    sessions: HashMap<u64, State>,
}
fn same_region(a: &Picture, b: &Picture) -> bool {
    a.layer == b.layer && a.offset == b.offset && a.extent == b.extent
}
fn same_metadata(a: &H264Reference, b: &H264Reference) -> bool {
    a.frame_num == b.frame_num && a.pic_order_cnt == b.pic_order_cnt && a.long_term == b.long_term
}
fn validate_target(image: &Image, region: &Picture) -> Result<(), String> {
    image
        .validate_bound()
        .map_err(|e| format!("unbound native decode image: {e:?}"))?;
    if region.layer != 0
        || region.offset.x != 0
        || region.offset.y != 0
        || region.extent != image.extent()
    {
        return Err("native decode requires the exact full bound image extent".into());
    }
    Ok(())
}
fn metadata(entry: &dpb::Entry) -> Result<H264Reference, String> {
    Ok(H264Reference {
        frame_num: u16::try_from(entry.long_term.unwrap_or(entry.frame_num))
            .map_err(|_| "reference frame/index exceeds StdVideo representation")?,
        pic_order_cnt: entry.poc,
        long_term: entry.long_term.is_some(),
    })
}
fn canonical_generation(
    codec: &dpb::Dpb,
    associations: &HashMap<u64, Slot>,
    reference: &Reference,
    image: &Arc<Image>,
) -> Result<u64, String> {
    validate_target(image, &reference.picture)?;
    for entry in &codec.entries {
        if let Some(known) = associations.get(&entry.generation) {
            if Arc::ptr_eq(&known.image, image)
                && same_region(&known.region, &reference.picture)
                && same_metadata(&metadata(entry)?, &reference.h264)
            {
                return Ok(entry.generation);
            }
        }
    }
    Err("native DPB identity, metadata, or generation mismatch".into())
}
// Inspect only original framing/header bytes. Stock Zink does not populate
// IdrPicFlag/is_intra, so those optional declarations cannot bootstrap VT.
#[cfg(test)]
fn actual_idr(bytes: &[u8], range: &Range<usize>, offsets: &[u32]) -> Result<bool, String> {
    let bytes = bytes
        .get(range.clone())
        .ok_or("compressed range out of bounds")?;
    let mut idr = None;
    for &offset in offsets {
        let b = bytes
            .get(offset as usize..)
            .ok_or("slice offset out of bounds")?;
        let prefix = if b.starts_with(&[0, 0, 0, 1]) {
            4
        } else if b.starts_with(&[0, 0, 1]) {
            3
        } else {
            return Err("missing Annex B prefix".into());
        };
        let header = *b.get(prefix).ok_or("missing slice header")?;
        let current = match header & 31 {
            5 => true,
            1 => false,
            _ => return Err("unsupported selected slice NAL".into()),
        };
        if header & 128 != 0 || idr.is_some_and(|old| old != current) {
            return Err("inconsistent access unit NAL types".into());
        }
        idr = Some(current);
    }
    idr.ok_or_else(|| "empty slice list".into())
}
impl Runtime {
    /// Caller has retired every submitted use of this session before removal.
    pub(crate) fn remove_session(&mut self, id: u64) {
        self.sessions.remove(&id);
    }
    /// # Safety
    /// The caller serializes decode order per session, owns every written target
    /// exclusively, and completes Vulkan release/acquire and queue synchronization.
    /// Success includes callback, original-input release and synchronous GPU copies.
    pub(crate) unsafe fn execute(
        &mut self,
        job: &Decode,
        resources: Resources,
    ) -> Result<(), String> {
        let Resources {
            source,
            source_range,
            destination,
            setup,
            references,
            begin_references,
        } = resources;
        if references.len() != job.references.len()
            || begin_references.len() != job.begin_references.len()
            || setup.is_some() != (job.h264.is_reference && job.setup.is_some())
        {
            return Err("resolved resource count mismatch".into());
        }
        if source_range.len() as u64 != job.source_range {
            return Err("resolved compressed range length mismatch".into());
        }
        let parsed = h264::parse(
            source.bytes(),
            &source_range,
            &job.slice_offsets,
            &job.configuration.sps,
            &job.configuration.pps,
        )?;
        let actual = h264::Summary {
            idr: parsed.idr,
            reference: parsed.reference,
            intra: parsed.slices.iter().all(|slice| slice.kind == 2),
        };
        if parsed.frame_num != u32::from(job.h264.frame_num)
            || parsed.pps_id != u32::from(job.h264.pps_id)
            || parsed.sequence.id != u32::from(job.h264.sps_id)
            || (job.h264.idr_pic_flag && parsed.idr_pic_id != Some(u32::from(job.h264.idr_pic_id)))
        {
            return Err(
                "original slice parameter/frame identity disagrees with decode metadata".into(),
            );
        }
        let idr = actual.idr;
        if actual.reference != job.h264.is_reference || (job.h264.is_intra && !actual.intra) {
            return Err(
                "declared picture reference/intra flags disagree with original slice".into(),
            );
        }
        if job.h264.idr_pic_flag && !idr {
            return Err("declared IDR is not an IDR NAL".into());
        }
        validate_target(&destination, &job.destination)?;
        if let (Some(image), Some(reference)) = (&setup, &job.setup) {
            validate_target(image, &reference.picture)?;
        }
        let state = self
            .sessions
            .entry(job.session.id)
            .or_insert_with(|| State {
                session: job.session.clone(),
                decoder: None,
                slots: HashMap::new(),
                associations: HashMap::new(),
                codec: dpb::Dpb::default(),
                history: poc::History::default(),
                active_sps: None,
                poisoned: false,
                failed_images: Vec::new(),
            });
        if !Arc::ptr_eq(&state.session, &job.session) {
            return Err("session ID reused with different owner".into());
        }
        if state.poisoned {
            return Err("native decode session is poisoned; retire and recreate it".into());
        }
        let reset = job.reset_before || state.decoder.is_none();
        if reset && !idr {
            if !actual.intra || !actual.reference || !job.references.is_empty() {
                return Err(
                    "native decoder reset requires an independent intra reference picture".into(),
                );
            }
            // Native intra40 proved fresh hardware decode, exact display/coded
            // target copies, and joined Vulkan sampling of this guarded path.
            h264::independent_intra(
                source.bytes(),
                &source_range,
                &job.slice_offsets,
                &job.configuration.sps,
                &job.configuration.pps,
                job.h264.frame_num,
                job.h264.pic_order_cnt,
            )?;
        }
        if !reset && !idr && state.active_sps.as_deref() != Some(job.configuration.sps.as_ref()) {
            return Err("active SPS content changed inside coded video sequence".into());
        }
        if job.references.len() > job.session.references as usize
            || job
                .setup
                .as_ref()
                .is_some_and(|r| r.slot >= job.session.dpb_slots)
            || parsed.max_refs > job.session.dpb_slots
        {
            return Err("decode references exceed session limits".into());
        }
        let mut codec = state.codec.clone();
        let mut associations = state.associations.clone();
        let mut slots = state.slots.clone();
        if job.begin_before {
            let mut snapshot = HashMap::new();
            for (reference, image) in job.begin_references.iter().zip(&begin_references) {
                if reference.slot >= job.session.dpb_slots || snapshot.contains_key(&reference.slot)
                {
                    return Err("invalid begin DPB slot".into());
                }
                let generation = canonical_generation(&codec, &associations, reference, image)?;
                snapshot.insert(reference.slot, generation);
            }
            slots = snapshot;
        }
        if reset || idr {
            if !job.references.is_empty() {
                return Err("IDR/reset cannot import reference images".into());
            }
            codec.entries.clear();
            codec.max_long_term_index = None;
            associations.clear();
            slots.clear();
        }
        let history = if reset {
            poc::History::default()
        } else {
            state.history.clone()
        };
        let history_plan = history.plan(&parsed, reset && !idr)?;
        if history_plan.poc != job.h264.pic_order_cnt {
            return Err("source-derived picture order count disagrees with decode metadata".into());
        }
        let current = dpb::Picture {
            frame_num: parsed.frame_num,
            poc: history_plan.poc,
        };
        let lists = codec.reference_lists(current, &parsed)?;
        let mut declared = Vec::new();
        let mut declared_slots = std::collections::HashSet::new();
        for (reference, image) in job.references.iter().zip(&references) {
            if reference.slot >= job.session.dpb_slots || !declared_slots.insert(reference.slot) {
                return Err("invalid decode DPB slot".into());
            }
            let generation = *slots
                .get(&reference.slot)
                .ok_or("unknown native DPB slot")?;
            if canonical_generation(&codec, &associations, reference, image)? != generation {
                return Err("active slot does not name canonical generation".into());
            }
            declared.push(generation);
        }
        for list in lists.iter().flat_map(|pair| pair.iter()) {
            if list.iter().any(|generation| !declared.contains(generation)) {
                return Err(
                    "source reference list needs an undeclared canonical generation".into(),
                );
            }
        }
        let plan = codec.plan(
            current,
            &parsed.marking,
            dpb::Limits {
                max_frame_num: parsed.max_frame_num,
                max_refs: parsed.max_refs,
            },
        )?;
        if let (Some(reference), Some(current)) = (&job.setup, &plan.current) {
            if !same_metadata(&reference.h264, &metadata(current)?) {
                return Err("setup metadata disagrees with source reference marking".into());
            }
        }
        let mut replacement = if reset {
            Some(Decoder::new_coded(
                &job.configuration.sps,
                &job.configuration.pps,
            )?)
        } else {
            None
        };
        if !reset {
            if let Err(error) = state
                .decoder
                .as_mut()
                .unwrap()
                .update_parameters(&job.configuration.sps, &job.configuration.pps)
            {
                if !idr {
                    return Err(error);
                }
                replacement = Some(Decoder::new_coded(
                    &job.configuration.sps,
                    &job.configuration.pps,
                )?);
            }
        }
        let extent = replacement
            .as_ref()
            .or(state.decoder.as_ref())
            .unwrap()
            .coded_extent;
        if [job.destination.extent.width, job.destination.extent.height] != extent
            || job.setup.as_ref().is_some_and(|r| {
                job.h264.is_reference && [r.picture.extent.width, r.picture.extent.height] != extent
            })
        {
            return Err("coded decoder output does not match exact target".into());
        }
        if let Some(replacement) = replacement {
            state.decoder = Some(replacement);
        }
        let decoder = state.decoder.as_mut().unwrap();
        // Submission errors may follow native acceptance. Fail closed even then.
        state.poisoned = true;
        state.failed_images = references;
        state.failed_images.extend(begin_references);
        state.failed_images.push(destination.clone());
        state.failed_images.extend(setup.iter().cloned());
        let submission =
            decoder.submit_annex_b_tracked(source, source_range, &job.slice_offsets)?;
        decoder.drain()?;
        let picture_result = submission
            .picture
            .recv()
            .map_err(|_| "decoder callback disconnected")?;
        submission
            .input_released
            .recv()
            .map_err(|_| "decoder input-release event disconnected")?;
        let picture = picture_result?;
        unsafe { destination.write_completed(picture.clone()) }
            .map_err(|e| format!("destination GPU copy failed: {e:?}"))?;
        if let Some(image) = &setup {
            if !Arc::ptr_eq(image, &destination) {
                unsafe { image.write_completed(picture) }
                    .map_err(|e| format!("DPB GPU copy failed: {e:?}"))?;
            }
        }
        associations.retain(|generation, slot| {
            plan.next
                .entries
                .iter()
                .any(|entry| entry.generation == *generation)
                && !Arc::ptr_eq(&slot.image, &destination)
                && !setup
                    .as_ref()
                    .is_some_and(|image| Arc::ptr_eq(image, &slot.image))
        });
        slots.retain(|_, generation| associations.contains_key(generation));
        if let Some(reference) = &job.setup {
            slots.remove(&reference.slot);
            if let (Some(image), Some(current)) = (setup, &plan.current) {
                associations.insert(
                    current.generation,
                    Slot {
                        image,
                        region: reference.picture.clone(),
                    },
                );
                slots.insert(reference.slot, current.generation);
            }
        }
        state.slots = slots;
        state.associations = associations;
        state.codec = plan.next;
        state.history = history_plan.next;
        state.active_sps = Some(job.configuration.sps.to_vec());
        state.failed_images.clear();
        state.poisoned = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idr_uses_selected_original_nals_and_nonzero_offsets() {
        let bytes = [99, 0, 0, 0, 1, 0x65, 4, 0, 0, 1, 0x65, 8];
        assert_eq!(actual_idr(&bytes, &(1..12), &[0, 6]).unwrap(), true);
        assert_eq!(
            actual_idr(&[0, 0, 1, 0x41, 9], &(0..5), &[0]).unwrap(),
            false
        );
    }
    #[test]
    fn idr_rejects_mixed_or_invalid_selected_headers() {
        assert!(actual_idr(&[0, 0, 1, 0x65, 0, 0, 1, 0x41], &(0..8), &[0, 4]).is_err());
        assert!(actual_idr(&[0, 0, 1, 0xe5], &(0..4), &[0]).is_err());
        assert!(actual_idr(&[0, 0, 1], &(0..3), &[0]).is_err());
        assert!(actual_idr(&[0, 0, 1, 0x67], &(0..4), &[0]).is_err());
        assert!(actual_idr(&[], &(0..0), &[]).is_err());
    }
    #[test]
    fn dpb_long_term_index_and_poc_are_exact_identity_metadata() {
        let a = H264Reference {
            frame_num: 3,
            pic_order_cnt: [42, 42],
            long_term: true,
        };
        assert!(same_metadata(&a, &a));
        let mut b = a.clone();
        b.frame_num = 42;
        assert!(!same_metadata(&a, &b));
        b = a.clone();
        b.long_term = false;
        assert!(!same_metadata(&a, &b));
        b = a.clone();
        b.pic_order_cnt[1] = 43;
        assert!(!same_metadata(&a, &b));
    }
}
