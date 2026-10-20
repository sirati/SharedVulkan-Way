//! Serialized, resolved native decode execution. This staging runtime requires
//! an actual IDR after reset. It does not advertise readiness or emulate MMCO.
use crate::{
    native_video::{Decode, H264Reference, Picture, Reference, Session},
    native_video_image::Image,
    video_nv12::videotoolbox::{CompressedBytes, Decoder},
};
use std::{collections::HashMap, ops::Range, sync::Arc};

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
    metadata: H264Reference,
    generation: u64,
}
struct State {
    session: Arc<Session>,
    decoder: Option<Decoder>,
    slots: HashMap<u32, Slot>,
    generation: u64,
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
fn validate_reference(
    slots: &HashMap<u32, Slot>,
    reference: &Reference,
    image: &Arc<Image>,
    generation: u64,
) -> Result<(), String> {
    validate_target(image, &reference.picture)?;
    let slot = slots
        .get(&reference.slot)
        .ok_or("unknown native DPB slot")?;
    if !Arc::ptr_eq(&slot.image, image)
        || !same_region(&slot.region, &reference.picture)
        || !same_metadata(&slot.metadata, &reference.h264)
        || slot.generation > generation
    {
        return Err("native DPB identity, metadata, or generation mismatch".into());
    }
    Ok(())
}
// Inspect only original framing/header bytes. Stock Zink does not populate
// IdrPicFlag/is_intra, so those optional declarations cannot bootstrap VT.
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
            || setup.is_some() != job.setup.is_some()
        {
            return Err("resolved resource count mismatch".into());
        }
        if source_range.len() as u64 != job.source_range {
            return Err("resolved compressed range length mismatch".into());
        }
        let idr = actual_idr(source.bytes(), &source_range, &job.slice_offsets)?;
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
                generation: 0,
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
            return Err("native decoder reset requires actual IDR bootstrap".into());
        }
        let mut slots = state.slots.clone();
        if reset || idr {
            if !job.references.is_empty() {
                return Err("IDR/reset cannot import reference images".into());
            }
        }
        if job.begin_before {
            for (reference, image) in job.begin_references.iter().zip(&begin_references) {
                validate_reference(&slots, reference, image, state.generation)?;
            }
            slots.retain(|slot, _| job.begin_references.iter().any(|r| r.slot == *slot));
        }
        if reset || idr {
            slots.clear();
        }
        for (reference, image) in job.references.iter().zip(&references) {
            validate_reference(&slots, reference, image, state.generation)?;
        }
        if !idr && !job.h264.is_intra && job.references.is_empty() {
            return Err("dependent native decode requires explicit validated references".into());
        }
        if let Some(reference) = &job.setup {
            // Non-IDR long-term marking needs MMCO syntax validation against VT's
            // implicit DPB. Until then, only IDR long_term_reference_flag is safe.
            if job.h264.is_reference && reference.h264.long_term && !idr {
                return Err("non-IDR long-term marking is not implemented".into());
            }
            if job.h264.is_reference
                && (reference.h264.pic_order_cnt != job.h264.pic_order_cnt
                    || (!reference.h264.long_term
                        && reference.h264.frame_num != job.h264.frame_num)
                    || (idr && reference.h264.long_term && reference.h264.frame_num != 0))
            {
                return Err("setup metadata does not describe decoded picture".into());
            }
        }
        let generation = state
            .generation
            .checked_add(1)
            .ok_or("decode generation exhausted")?;
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
            || job
                .setup
                .as_ref()
                .is_some_and(|r| [r.picture.extent.width, r.picture.extent.height] != extent)
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
        slots.retain(|_, slot| {
            !Arc::ptr_eq(&slot.image, &destination)
                && !setup
                    .as_ref()
                    .is_some_and(|image| Arc::ptr_eq(image, &slot.image))
        });
        if let (Some(reference), Some(image)) = (&job.setup, setup) {
            slots.remove(&reference.slot);
            if job.h264.is_reference {
                slots.insert(
                    reference.slot,
                    Slot {
                        image,
                        region: reference.picture.clone(),
                        metadata: reference.h264.clone(),
                        generation,
                    },
                );
            }
        }
        state.slots = slots;
        state.generation = generation;
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
