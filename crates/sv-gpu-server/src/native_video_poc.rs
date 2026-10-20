//! Progressive-frame POC/source history, ITU-T H.264 7.4.3 and 8.2.1.
//! Plans are immutable until the actual native producer has completed. Frame
//! number gaps are detected and refused; non-existing pictures are not omitted
//! from an imagined reference state and never silently supplied by VT.
use super::h264::{Marking, Operation, Parsed, Sequence};
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct History {
    sequence: Option<Sequence>,
    previous_frame_num: u32,
    frame_num_offset: i64,
    previous_reference_frame_num: Option<u32>,
    reference_msb: i64,
    reference_lsb: i64,
    previous_nonreference: bool,
    maximum_picture_poc: Option<i32>,
}
pub(super) struct Plan {
    pub next: History,
    pub poc: [i32; 2],
}
fn integer(value: i64) -> Result<i32, String> {
    i32::try_from(value).map_err(|_| "H264 picture order count overflow".into())
}
pub(super) struct Synthetic {
    pub frame_num: u32,
    // POC is not inferred for type0 non-existing frames, 8.2.5.2.
    pub poc: Option<[i32; 2]>,
}
pub(super) struct CompletePlan {
    pub next: History,
    pub poc: [i32; 2],
    pub missing: Vec<Synthetic>,
}
impl History {
    /// Pure staging model. Native execution still calls the guarded `plan`.
    /// ITU-T H.264 8.2.5.2 synthesizes every intervening frame in order, even
    /// when the triggering access unit is non-reference.
    pub(super) fn plan_unproven(
        &self,
        parsed: &Parsed,
        bootstrap: bool,
    ) -> Result<CompletePlan, String> {
        let mut history = self.clone();
        let mut missing = Vec::new();
        if !parsed.idr {
            if let Some(previous) = history.previous_reference_frame_num {
                let maximum = parsed.max_frame_num;
                if !(16..=65536).contains(&maximum)
                    || !maximum.is_power_of_two()
                    || parsed.frame_num >= maximum
                {
                    return Err("invalid gap frame modulus".into());
                }
                if parsed.frame_num != previous && parsed.frame_num != (previous + 1) % maximum {
                    if !parsed.sequence.gaps_allowed {
                        return Err("frame_num gap is forbidden by SPS".into());
                    }
                    let mut frame = (previous + 1) % maximum;
                    while frame != parsed.frame_num {
                        let poc = if parsed.sequence.poc == 0 {
                            history.previous_reference_frame_num = Some(frame);
                            None
                        } else {
                            let mut synthetic = parsed.clone();
                            synthetic.frame_num = frame;
                            synthetic.reference = true;
                            synthetic.idr = false;
                            synthetic.idr_pic_id = None;
                            synthetic.poc = super::h264::Poc::default();
                            synthetic.marking = Marking::Sliding;
                            let plan = history.plan_inner(&synthetic, false, true)?;
                            history = plan.next;
                            // 8.2.1 listD includes inferred frames for POC1/2.
                            Some(plan.poc)
                        };
                        missing.push(Synthetic {
                            frame_num: frame,
                            poc,
                        });
                        frame = (frame + 1) % maximum;
                    }
                }
            }
        }
        let plan = history.plan_inner(parsed, bootstrap, true)?;
        Ok(CompletePlan {
            next: plan.next,
            poc: plan.poc,
            missing,
        })
    }
    pub(super) fn plan(&self, parsed: &Parsed, bootstrap: bool) -> Result<Plan, String> {
        self.plan_inner(parsed, bootstrap, false)
    }
    fn plan_inner(&self, parsed: &Parsed, bootstrap: bool, unproven: bool) -> Result<Plan, String> {
        let s = &parsed.sequence;
        if !(4..=16).contains(&s.frame_bits) || s.poc > 2 || parsed.frame_num >= 1 << s.frame_bits {
            return Err("invalid POC sequence/frame syntax".into());
        }
        if self.sequence.is_none() && !parsed.idr {
            if !bootstrap
                || !parsed.reference
                || (!unproven && s.poc != 0)
                || parsed.slices.iter().any(|slice| slice.kind != 2)
                || parsed.marking != Marking::Sliding
            {
                return Err(
                    "POC history requires IDR or validated independent intra bootstrap".into(),
                );
            }
        }
        let mut next = if parsed.idr {
            Self::default()
        } else {
            self.clone()
        };
        if let Some(old) = &next.sequence {
            if old != s {
                return Err("SPS sequence syntax changed without IDR/reset".into());
            }
        }
        let maximum = 1u32 << s.frame_bits;
        if parsed.max_frame_num != maximum || parsed.max_refs != s.max_refs {
            return Err("inconsistent parsed sequence limits".into());
        }
        if parsed.idr {
            if parsed.frame_num != 0 || !parsed.reference {
                return Err("invalid IDR frame identity".into());
            }
        } else if let Some(previous) = next.previous_reference_frame_num {
            if parsed.frame_num != (previous + 1) % maximum {
                return Err(if s.gaps_allowed {
                    "frame_num gaps require non-existing DPB synthesis, not yet supported"
                } else {
                    "frame_num violates progressive source history and gap flag"
                }
                .into());
            }
        }
        if s.poc == 2 && next.previous_nonreference && !parsed.reference {
            return Err("POC type2 forbids consecutive non-reference frames".into());
        }
        let frame_offset = if parsed.idr {
            0
        } else if next.previous_frame_num > parsed.frame_num {
            next.frame_num_offset
                .checked_add(i64::from(maximum))
                .ok_or("H264 FrameNumOffset overflow")?
        } else {
            next.frame_num_offset
        };
        let mut current_msb = next.reference_msb;
        let values = match s.poc {
            0 => {
                if !(4..=16).contains(&s.poc_bits) {
                    return Err("invalid POC LSB width".into());
                }
                let maximum = 1i64 << s.poc_bits;
                let lsb = i64::from(parsed.poc.lsb.ok_or("POC type0 lacks original LSB")?);
                if lsb >= maximum {
                    return Err("original POC LSB exceeds modulus".into());
                }
                if lsb < next.reference_lsb && next.reference_lsb - lsb >= maximum / 2 {
                    current_msb = next
                        .reference_msb
                        .checked_add(maximum)
                        .ok_or("H264 POC MSB overflow")?;
                } else if lsb > next.reference_lsb && lsb - next.reference_lsb > maximum / 2 {
                    current_msb = next
                        .reference_msb
                        .checked_sub(maximum)
                        .ok_or("H264 POC MSB underflow")?;
                }
                let top = current_msb.checked_add(lsb).ok_or("H264 POC overflow")?;
                [
                    top,
                    top.checked_add(i64::from(parsed.poc.bottom_delta))
                        .ok_or("H264 bottom POC overflow")?,
                ]
            }
            1 => {
                let mut absolute = if s.cycle_offsets.is_empty() {
                    0
                } else {
                    frame_offset
                        .checked_add(i64::from(parsed.frame_num))
                        .ok_or("H264 absolute frame overflow")?
                };
                if !parsed.reference && absolute > 0 {
                    absolute -= 1;
                }
                let mut expected = 0i64;
                if absolute > 0 {
                    let cycle = s.cycle_offsets.len() as i64;
                    let count = (absolute - 1) / cycle;
                    let within = (absolute - 1) % cycle;
                    let total = s.cycle_offsets.iter().try_fold(0i64, |sum, value| {
                        sum.checked_add(i64::from(*value))
                            .ok_or("H264 POC cycle sum overflow")
                    })?;
                    expected = count.checked_mul(total).ok_or("H264 POC cycle overflow")?;
                    for value in &s.cycle_offsets[..=within as usize] {
                        expected = expected
                            .checked_add(i64::from(*value))
                            .ok_or("H264 POC cycle prefix overflow")?;
                    }
                }
                if !parsed.reference {
                    expected = expected
                        .checked_add(i64::from(s.offset_non_ref))
                        .ok_or("H264 nonreference POC overflow")?;
                }
                let top = expected
                    .checked_add(i64::from(parsed.poc.delta[0]))
                    .ok_or("H264 top POC overflow")?;
                let bottom = top
                    .checked_add(i64::from(s.offset_top_bottom))
                    .and_then(|n| n.checked_add(i64::from(parsed.poc.delta[1])))
                    .ok_or("H264 bottom POC overflow")?;
                [top, bottom]
            }
            2 => {
                let value = if parsed.idr {
                    0
                } else {
                    frame_offset
                        .checked_add(i64::from(parsed.frame_num))
                        .and_then(|n| n.checked_mul(2))
                        .and_then(|n| n.checked_sub(i64::from(!parsed.reference)))
                        .ok_or("H264 POC type2 overflow")?
                };
                [value, value]
            }
            _ => unreachable!(),
        };
        // 8.2.1 bounds intermediate FrameNumOffset and PicOrderCntMsb too.
        if s.poc != 0 {
            integer(frame_offset)?;
        }
        integer(current_msb)?;
        let poc = [integer(values[0])?, integer(values[1])?];
        if parsed.idr && poc[0].min(poc[1]) != 0 {
            return Err("IDR picture order count must start at zero".into());
        }
        let mmco5 =
            matches!(&parsed.marking,Marking::Adaptive(ops) if ops.contains(&Operation::ForgetAll));
        let picture_poc = poc[0].min(poc[1]);
        if mmco5 {
            if next
                .maximum_picture_poc
                .is_some_and(|maximum| picture_poc <= maximum)
            {
                return Err("MMCO5 picture does not follow prior pictures in output order".into());
            }
            if s.poc == 0 && i64::from(parsed.poc.bottom_delta) < 1 - (1i64 << s.poc_bits) {
                return Err("MMCO5 bottom POC delta exceeds its specified bound".into());
            }
        }
        next.maximum_picture_poc = Some(if mmco5 {
            0
        } else {
            next.maximum_picture_poc
                .map_or(picture_poc, |old| old.max(picture_poc))
        });
        next.sequence = Some(s.clone());
        next.previous_frame_num = if mmco5 { 0 } else { parsed.frame_num };
        next.frame_num_offset = if mmco5 { 0 } else { frame_offset };
        next.previous_nonreference = !parsed.reference;
        if parsed.reference {
            next.previous_reference_frame_num = Some(if mmco5 { 0 } else { parsed.frame_num });
            if s.poc == 0 {
                next.reference_msb = if mmco5 { 0 } else { current_msb };
                next.reference_lsb = if mmco5 {
                    i64::from(poc[0]) - i64::from(poc[0].min(poc[1]))
                } else {
                    i64::from(parsed.poc.lsb.unwrap())
                };
            }
        }
        Ok(Plan { next, poc })
    }
}
#[cfg(test)]
mod tests {
    use super::super::h264::{Poc, Slice};
    use super::*;
    fn picture(ty: u32, frame: u32, reference: bool) -> Parsed {
        Parsed {
            sequence: Sequence {
                id: 0,
                frame_bits: 4,
                poc: ty,
                poc_bits: 4,
                max_refs: 4,
                coded_extent: [64, 64],
                ..Default::default()
            },
            idr: false,
            reference,
            frame_num: frame,
            pps_id: 0,
            idr_pic_id: None,
            poc: Poc::default(),
            max_frame_num: 16,
            max_refs: 4,
            marking: if reference {
                Marking::Sliding
            } else {
                Marking::NonReference
            },
            slices: vec![Slice {
                first_mb: 0,
                kind: 2,
                active: [0, 0],
                modifications: [vec![], vec![]],
            }],
        }
    }
    fn idr(ty: u32) -> Parsed {
        let mut p = picture(ty, 0, true);
        p.idr = true;
        p.idr_pic_id = Some(0);
        p.marking = Marking::Idr {
            no_output_prior: false,
            long_term: false,
        };
        if ty == 0 {
            p.poc.lsb = Some(0)
        }
        p
    }
    fn step(history: &mut History, p: &Parsed) -> [i32; 2] {
        let plan = history.plan(p, false).unwrap();
        *history = plan.next;
        plan.poc
    }
    #[test]
    fn poc0_wrap_and_nonreference_do_not_replace_reference_msb_lsb() {
        let mut h = History::default();
        step(&mut h, &idr(0));
        let mut p = picture(0, 1, true);
        p.poc.lsb = Some(6);
        assert_eq!(step(&mut h, &p), [6, 6]);
        p.frame_num = 2;
        p.poc.lsb = Some(12);
        assert_eq!(step(&mut h, &p), [12, 12]);
        let mut b = picture(0, 3, false);
        b.poc.lsb = Some(8);
        assert_eq!(step(&mut h, &b), [8, 8]);
        p.frame_num = 3;
        p.poc.lsb = Some(2);
        assert_eq!(step(&mut h, &p), [18, 18]);
    }
    #[test]
    fn poc0_half_modulus_boundaries_are_asymmetric() {
        let mut h = History::default();
        step(&mut h, &idr(0));
        let mut p = picture(0, 1, true);
        p.poc.lsb = Some(8);
        assert_eq!(step(&mut h, &p), [8, 8]);
        p.frame_num = 2;
        p.poc.lsb = Some(0);
        assert_eq!(step(&mut h, &p), [16, 16]);
    }
    #[test]
    fn mmco5_normalizes_ref_and_decode_history_for_next_picture() {
        let mut h = History::default();
        step(&mut h, &idr(0));
        let mut p = picture(0, 1, true);
        p.poc.lsb = Some(6);
        p.poc.bottom_delta = -2;
        p.marking = Marking::Adaptive(vec![Operation::ForgetAll]);
        assert_eq!(step(&mut h, &p), [6, 4]);
        p.frame_num = 1;
        p.poc.lsb = Some(4);
        p.poc.bottom_delta = 0;
        p.marking = Marking::Sliding;
        assert_eq!(step(&mut h, &p), [4, 4]);
    }
    #[test]
    fn poc1_uses_cycle_prefix_nonreference_offset_and_both_deltas() {
        let mut first = idr(1);
        first.sequence.cycle_offsets = vec![2, 3];
        first.sequence.offset_non_ref = -1;
        first.sequence.offset_top_bottom = 1;
        let mut h = History::default();
        assert_eq!(step(&mut h, &first), [0, 1]);
        let mut p = first.clone();
        p.idr = false;
        p.idr_pic_id = None;
        p.marking = Marking::Sliding;
        p.frame_num = 1;
        assert_eq!(step(&mut h, &p), [2, 3]);
        let mut b = p.clone();
        b.frame_num = 2;
        b.reference = false;
        b.marking = Marking::NonReference;
        b.poc.delta = [1, -1];
        assert_eq!(step(&mut h, &b), [2, 2]);
        p.frame_num = 2;
        assert_eq!(step(&mut h, &p), [5, 6]);
        p.frame_num = 3;
        assert_eq!(step(&mut h, &p), [7, 8]);
    }
    #[test]
    fn poc2_frame_num_wrap_and_nonreference_order() {
        let mut h = History::default();
        step(&mut h, &idr(2));
        for frame in 1..16 {
            assert_eq!(
                step(&mut h, &picture(2, frame, true)),
                [2 * frame as i32; 2]
            );
        }
        assert_eq!(step(&mut h, &picture(2, 0, true)), [32, 32]);
        assert_eq!(step(&mut h, &picture(2, 1, false)), [33, 33]);
        assert!(h.plan(&picture(2, 1, false), false).is_err());
        assert_eq!(step(&mut h, &picture(2, 1, true)), [34, 34]);
    }
    #[test]
    fn gaps_and_same_frame_reference_are_refused_without_mutating_history() {
        let mut h = History::default();
        step(&mut h, &idr(2));
        let before = h.clone();
        assert!(h.plan(&picture(2, 3, true), false).is_err());
        assert!(h.plan(&picture(2, 0, true), false).is_err());
        assert_eq!(h, before);
        let mut first = idr(2);
        first.sequence.gaps_allowed = true;
        let mut with_gaps = History::default();
        step(&mut with_gaps, &first);
        let mut skipped = picture(2, 3, true);
        skipped.sequence.gaps_allowed = true;
        assert!(
            with_gaps
                .plan(&skipped, false)
                .err()
                .unwrap()
                .contains("non-existing DPB synthesis")
        );
    }
    #[test]
    fn only_validated_poc0_reference_intra_can_bootstrap_without_idr() {
        let stream =
            include_bytes!("../examples/assets/videotoolbox-h264/non-idr-intra-64x50.h264");
        let p =
            super::super::h264::parse(stream, &(35..156), &[0], &stream[4..26], &stream[30..35])
                .unwrap();
        let h = History::default();
        assert!(h.plan(&p, false).is_err());
        let plan = h.plan(&p, true).unwrap();
        assert_eq!(plan.poc, [8, 8]);
        assert_eq!(plan.next.previous_reference_frame_num, Some(3));
    }
    #[test]
    fn sequence_poc_mutation_requires_new_idr_or_reset() {
        let mut h = History::default();
        step(&mut h, &idr(0));
        let p = picture(2, 1, true);
        assert!(h.plan(&p, false).is_err());
        assert_eq!(step(&mut h, &idr(2)), [0, 0]);
    }
    #[test]
    fn mmco5_cannot_reset_poc_before_previously_decoded_picture() {
        let mut h = History::default();
        step(&mut h, &idr(0));
        let mut p = picture(0, 1, true);
        p.poc.lsb = Some(6);
        step(&mut h, &p);
        p.frame_num = 2;
        p.poc.lsb = Some(4);
        p.marking = Marking::Adaptive(vec![Operation::ForgetAll]);
        assert!(h.plan(&p, false).err().unwrap().contains("output order"));
    }
    #[test]
    fn poc_overflow_is_refused_without_committing_partial_history() {
        let mut first = idr(1);
        first.sequence.cycle_offsets = vec![i32::MAX];
        let mut h = History::default();
        step(&mut h, &first);
        let mut p = first.clone();
        p.idr = false;
        p.idr_pic_id = None;
        p.frame_num = 1;
        p.marking = Marking::Sliding;
        p.poc.delta[0] = 1;
        let before = h.clone();
        assert!(h.plan(&p, false).is_err());
        assert_eq!(h, before);
    }
    #[test]
    fn unproven_fresh_reference_i_models_poc1_and_poc2_without_opening_live_gate() {
        for ty in [1, 2] {
            let mut p = picture(ty, 3, true);
            if ty == 1 {
                p.sequence.cycle_offsets = vec![2];
                p.sequence.offset_top_bottom = 1;
            }
            assert!(History::default().plan(&p, true).is_err());
            let plan = History::default().plan_unproven(&p, true).unwrap();
            assert_eq!(plan.poc, if ty == 1 { [6, 7] } else { [6, 6] });
            assert!(plan.missing.is_empty());
        }
    }
    #[test]
    fn unproven_gap_models_each_missing_frame_and_wrap() {
        for ty in [0, 1, 2] {
            let mut initial = idr(ty);
            initial.sequence.gaps_allowed = true;
            if ty == 1 {
                initial.sequence.cycle_offsets = vec![2];
            }
            let mut history = History::default().plan(&initial, false).unwrap().next;
            let mut current = picture(ty, 3, true);
            current.sequence = initial.sequence.clone();
            if ty == 0 {
                current.poc.lsb = Some(6);
            }
            assert!(history.plan(&current, false).is_err());
            let plan = history.plan_unproven(&current, false).unwrap();
            assert_eq!(
                plan.missing.iter().map(|p| p.frame_num).collect::<Vec<_>>(),
                vec![1, 2]
            );
            assert_eq!(plan.poc, [6, 6]);
            assert_eq!(
                plan.missing[0].poc,
                if ty == 0 { None } else { Some([2, 2]) }
            );
            history = plan.next;
            current.frame_num = 1;
            if ty == 0 {
                current.poc.lsb = Some(8);
            }
            let plan = history.plan_unproven(&current, false).unwrap();
            assert_eq!(plan.missing.len(), 13);
            assert_eq!(plan.missing.first().unwrap().frame_num, 4);
            assert_eq!(plan.missing.last().unwrap().frame_num, 0);
            assert_eq!(plan.poc, if ty == 0 { [8, 8] } else { [34, 34] });
        }
    }
    #[test]
    fn nonreference_gap_updates_prev_reference_to_last_inferred_frame() {
        let mut initial = idr(2);
        initial.sequence.gaps_allowed = true;
        let history = History::default().plan(&initial, false).unwrap().next;
        let mut nonref = picture(2, 3, false);
        nonref.sequence = initial.sequence.clone();
        let plan = history.plan_unproven(&nonref, false).unwrap();
        assert_eq!(plan.poc, [5, 5]);
        let mut current = picture(2, 3, true);
        current.sequence = initial.sequence;
        assert!(
            plan.next
                .plan_unproven(&current, false)
                .unwrap()
                .missing
                .is_empty()
        );
    }
    #[test]
    fn mmco5_output_order_includes_poc1_inferred_frames() {
        let mut initial = idr(1);
        initial.sequence.gaps_allowed = true;
        initial.sequence.cycle_offsets = vec![10, -9];
        let history = History::default().plan(&initial, false).unwrap().next;
        let mut current = picture(1, 2, true);
        current.sequence = initial.sequence;
        current.marking = Marking::Adaptive(vec![Operation::ForgetAll]);
        let failure = history.plan_unproven(&current, false).err().unwrap();
        assert!(failure.contains("output order"));
    }
    #[test]
    fn poc0_does_not_impose_unused_frame_offset_bound() {
        let mut history = History::default().plan(&idr(0), false).unwrap().next;
        history.previous_frame_num = 15;
        history.previous_reference_frame_num = Some(15);
        history.frame_num_offset = i64::from(i32::MAX);
        let mut current = picture(0, 0, true);
        current.poc.lsb = Some(2);
        assert_eq!(history.plan(&current, false).unwrap().poc, [2, 2]);
    }
}
