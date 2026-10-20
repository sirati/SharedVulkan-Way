//! Transactional progressive-frame H.264 decoded-reference marking model.
//! ITU-T H.264 (08/2021), 7.4.3.3 and 8.2.5. This owns codec metadata only.
//! A caller must reconcile every generation with exact bound resources before
//! native submission, then commit `Plan::next` only after successful completion.
//! This marking model does not synthesize gap pictures or derive full POC.
//! Authoritative edition PDF SHA256:
//! d6e09bcc94e5d095e1b2d6d16bf96d82a593d67915a963336ada082a775e9cdf.
use super::h264::{Marking, Operation};
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub generation: u64,
    /// FrameNum after MMCO5 normalization. LT metadata uses `long_term` instead.
    pub frame_num: u32,
    pub poc: [i32; 2],
    pub long_term: Option<u32>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Dpb {
    pub entries: Vec<Entry>,
    pub max_long_term_index: Option<u32>,
    pub generation: u64,
}
#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub max_frame_num: u32,
    pub max_refs: u32,
}
#[derive(Clone, Copy)]
pub(super) struct Picture {
    pub frame_num: u32,
    pub poc: [i32; 2],
}
pub(super) struct Plan {
    pub next: Dpb,
    pub current: Option<Entry>,
    pub removed: Vec<u64>,
    /// Surviving prior generations whose LT association changed by MMCO3.
    pub changed: Vec<Entry>,
}
fn wrap(entry: &Entry, current: u32, maximum: u32) -> i64 {
    i64::from(entry.frame_num)
        - if entry.frame_num > current {
            i64::from(maximum)
        } else {
            0
        }
}
fn short_index(
    entries: &[Entry],
    current: u32,
    maximum: u32,
    difference_minus1: u32,
) -> Result<usize, String> {
    let target = i64::from(current) - (i64::from(difference_minus1) + 1);
    entries
        .iter()
        .position(|e| e.long_term.is_none() && wrap(e, current, maximum) == target)
        .ok_or_else(|| "MMCO names an absent short-term reference".into())
}
fn check_long(index: u32, maximum: Option<u32>) -> Result<(), String> {
    if !maximum.is_some_and(|max| index <= max) {
        return Err("MMCO long-term index exceeds current bound".into());
    }
    Ok(())
}
impl Dpb {
    pub(super) fn plan(
        &self,
        picture: Picture,
        marking: &Marking,
        limits: Limits,
    ) -> Result<Plan, String> {
        if !(16..=65536).contains(&limits.max_frame_num)
            || !limits.max_frame_num.is_power_of_two()
            || limits.max_refs > 16
            || picture.frame_num >= limits.max_frame_num
        {
            return Err("invalid progressive H264 DPB limits or frame_num".into());
        }
        let capacity = limits.max_refs.max(1) as usize;
        let mut next = self.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or("DPB generation overflow")?;
        let mut current = Entry {
            generation: next.generation,
            frame_num: picture.frame_num,
            poc: picture.poc,
            long_term: None,
        };
        let retain_current = !matches!(marking, Marking::NonReference);
        match marking {
            Marking::NonReference => {}
            Marking::Idr { long_term, .. } => {
                if picture.frame_num != 0 {
                    return Err("IDR frame_num must be zero".into());
                }
                next.entries.clear();
                next.max_long_term_index = long_term.then_some(0);
                current.long_term = long_term.then_some(0);
            }
            Marking::Sliding => {
                if next.entries.len() > capacity {
                    return Err("DPB already exceeds SPS reference bound".into());
                }
                if next.entries.len() == capacity {
                    let oldest = next
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.long_term.is_none())
                        .min_by_key(|(_, e)| wrap(e, picture.frame_num, limits.max_frame_num))
                        .map(|(i, _)| i)
                        .ok_or("sliding window has no short-term picture to evict")?;
                    next.entries.remove(oldest);
                }
            }
            Marking::Adaptive(operations) => {
                if operations.len() > 64 {
                    return Err("too many MMCO operations".into());
                }
                // 7.4.3.3 explicitly forbids these combinations, independent of
                // whether their hypothetical effects happen to cancel out.
                let mut counts = [0u32; 3];
                let reset = operations
                    .iter()
                    .any(|op| matches!(op, Operation::ForgetAll));
                if reset
                    && operations.iter().any(|op| {
                        matches!(
                            op,
                            Operation::ForgetShort { .. }
                                | Operation::ForgetLong { .. }
                                | Operation::Promote { .. }
                        )
                    })
                {
                    return Err("MMCO5 cannot coexist with MMCO1-3".into());
                }
                for operation in operations {
                    match *operation {
                        Operation::ForgetShort { difference_minus1 } => {
                            let index = short_index(
                                &next.entries,
                                picture.frame_num,
                                limits.max_frame_num,
                                difference_minus1,
                            )?;
                            next.entries.remove(index);
                        }
                        Operation::ForgetLong { pic_num } => {
                            if current.long_term == Some(pic_num) {
                                return Err("MMCO2 cannot unmark the current picture".into());
                            }
                            let index = next
                                .entries
                                .iter()
                                .position(|e| e.long_term == Some(pic_num))
                                .ok_or("MMCO names an absent long-term reference")?;
                            next.entries.remove(index);
                        }
                        Operation::Promote {
                            difference_minus1,
                            index,
                        } => {
                            check_long(index, next.max_long_term_index)?;
                            if current.long_term == Some(index) {
                                return Err("MMCO3 cannot replace the current picture".into());
                            }
                            let target = short_index(
                                &next.entries,
                                picture.frame_num,
                                limits.max_frame_num,
                                difference_minus1,
                            )?;
                            let generation = next.entries[target].generation;
                            next.entries.retain(|e| e.long_term != Some(index));
                            next.entries
                                .iter_mut()
                                .find(|e| e.generation == generation)
                                .unwrap()
                                .long_term = Some(index);
                        }
                        Operation::LimitLong { max_index_plus1 } => {
                            counts[0] += 1;
                            if counts[0] > 1 || max_index_plus1 > limits.max_refs {
                                return Err("invalid or repeated MMCO4".into());
                            }
                            let maximum = max_index_plus1.checked_sub(1);
                            if current
                                .long_term
                                .is_some_and(|index| !maximum.is_some_and(|max| index <= max))
                            {
                                return Err("MMCO4 cannot unmark the current picture".into());
                            }
                            next.max_long_term_index = maximum;
                            next.entries.retain(|e| {
                                e.long_term
                                    .is_none_or(|index| maximum.is_some_and(|max| index <= max))
                            });
                        }
                        Operation::ForgetAll => {
                            counts[1] += 1;
                            if counts[1] > 1 || current.long_term.is_some() {
                                return Err("invalid or repeated MMCO5 ordering".into());
                            }
                            next.entries.clear();
                            next.max_long_term_index = None;
                            current.frame_num = 0;
                            let poc = current.poc[0].min(current.poc[1]);
                            current.poc = [
                                current.poc[0]
                                    .checked_sub(poc)
                                    .ok_or("MMCO5 POC overflow")?,
                                current.poc[1]
                                    .checked_sub(poc)
                                    .ok_or("MMCO5 POC overflow")?,
                            ];
                        }
                        Operation::CurrentLong { index } => {
                            counts[2] += 1;
                            if counts[2] > 1 {
                                return Err("repeated MMCO6".into());
                            }
                            check_long(index, next.max_long_term_index)?;
                            next.entries.retain(|e| e.long_term != Some(index));
                            current.long_term = Some(index);
                            // 8.2.5.4.6 imposes the reference bound immediately,
                            // so a later MMCO1 cannot repair an early overflow.
                            if next.entries.len() + 1 > capacity {
                                return Err("MMCO6 immediately exceeds reference bound".into());
                            }
                        }
                    }
                }
            }
        }
        if retain_current {
            if current.long_term.is_none()
                && next
                    .entries
                    .iter()
                    .any(|e| e.long_term.is_none() && e.frame_num == current.frame_num)
            {
                return Err("duplicate short-term frame_num after marking".into());
            }
            next.entries.push(current.clone());
        }
        if next.entries.len() > capacity {
            return Err("marked DPB exceeds SPS reference bound".into());
        }
        let removed = self
            .entries
            .iter()
            .filter(|old| {
                !next
                    .entries
                    .iter()
                    .any(|new| new.generation == old.generation)
            })
            .map(|e| e.generation)
            .collect();
        let changed = next
            .entries
            .iter()
            .filter(|new| {
                self.entries
                    .iter()
                    .any(|old| old.generation == new.generation && old != *new)
            })
            .cloned()
            .collect();
        Ok(Plan {
            next,
            current: retain_current.then_some(current),
            removed,
            changed,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const LIMITS: Limits = Limits {
        max_frame_num: 16,
        max_refs: 3,
    };
    fn picture(frame_num: u32) -> Picture {
        Picture {
            frame_num,
            poc: [2 * frame_num as i32; 2],
        }
    }
    fn step(dpb: &mut Dpb, n: u32, marking: Marking) -> Plan {
        let plan = dpb.plan(picture(n), &marking, LIMITS).unwrap();
        *dpb = plan.next.clone();
        plan
    }
    #[test]
    fn sliding_evicts_smallest_wrapped_frame_num() {
        let mut d = Dpb::default();
        for n in [14, 15, 0] {
            step(&mut d, n, Marking::Sliding);
        }
        let p = step(&mut d, 1, Marking::Sliding);
        assert_eq!(p.removed, vec![1]);
        assert_eq!(
            d.entries.iter().map(|e| e.frame_num).collect::<Vec<_>>(),
            [15, 0, 1]
        );
    }
    #[test]
    fn mmco1_removes_exact_short_picture_and_failure_is_transactional() {
        let mut d = Dpb::default();
        step(&mut d, 0, Marking::Sliding);
        step(&mut d, 1, Marking::Sliding);
        let p = step(
            &mut d,
            2,
            Marking::Adaptive(vec![Operation::ForgetShort {
                difference_minus1: 1,
            }]),
        );
        assert_eq!(p.removed, vec![1]);
        let before = d.clone();
        assert!(
            d.plan(
                picture(3),
                &Marking::Adaptive(vec![Operation::ForgetShort {
                    difference_minus1: 10
                }]),
                LIMITS
            )
            .is_err()
        );
        assert_eq!(d, before);
    }
    #[test]
    fn mmco2_forgets_idr_long_term_picture() {
        let mut d = Dpb::default();
        step(
            &mut d,
            0,
            Marking::Idr {
                no_output_prior: false,
                long_term: true,
            },
        );
        let p = step(
            &mut d,
            1,
            Marking::Adaptive(vec![Operation::ForgetLong { pic_num: 0 }]),
        );
        assert_eq!(p.removed, vec![1]);
        assert!(d.entries.iter().all(|e| e.long_term.is_none()));
    }
    #[test]
    fn mmco3_promotes_exact_generation_and_replaces_same_long_index() {
        let mut d = Dpb::default();
        step(
            &mut d,
            0,
            Marking::Idr {
                no_output_prior: false,
                long_term: true,
            },
        );
        step(&mut d, 1, Marking::Sliding);
        let p = step(
            &mut d,
            2,
            Marking::Adaptive(vec![Operation::Promote {
                difference_minus1: 0,
                index: 0,
            }]),
        );
        assert_eq!(p.removed, vec![1]);
        assert_eq!(p.changed.len(), 1);
        assert_eq!(p.changed[0].generation, 2);
        assert_eq!(p.changed[0].long_term, Some(0));
    }
    #[test]
    fn mmco4_reduces_bound_and_retires_higher_long_indices() {
        let mut d = Dpb::default();
        step(&mut d, 0, Marking::Sliding);
        step(
            &mut d,
            1,
            Marking::Adaptive(vec![
                Operation::LimitLong { max_index_plus1: 3 },
                Operation::CurrentLong { index: 2 },
            ]),
        );
        let p = step(
            &mut d,
            2,
            Marking::Adaptive(vec![Operation::LimitLong { max_index_plus1: 1 }]),
        );
        assert_eq!(p.removed, vec![2]);
        assert_eq!(d.max_long_term_index, Some(0));
    }
    #[test]
    fn mmco5_clears_refs_and_normalizes_current_frame_and_poc() {
        let mut d = Dpb::default();
        step(&mut d, 0, Marking::Sliding);
        let p = d
            .plan(
                Picture {
                    frame_num: 7,
                    poc: [17, 19],
                },
                &Marking::Adaptive(vec![Operation::ForgetAll]),
                LIMITS,
            )
            .unwrap();
        assert_eq!(p.removed, vec![1]);
        let current = p.current.unwrap();
        assert_eq!(current.frame_num, 0);
        assert_eq!(current.poc, [0, 2]);
        assert_eq!(p.next.max_long_term_index, None);
    }
    #[test]
    fn mmco6_replaces_existing_long_index_and_keeps_current_generation() {
        let mut d = Dpb::default();
        step(
            &mut d,
            0,
            Marking::Idr {
                no_output_prior: false,
                long_term: true,
            },
        );
        let p = step(
            &mut d,
            1,
            Marking::Adaptive(vec![Operation::CurrentLong { index: 0 }]),
        );
        assert_eq!(p.removed, vec![1]);
        assert_eq!(p.current.unwrap().long_term, Some(0));
        assert_eq!(d.entries[0].generation, 2);
    }
    #[test]
    fn reset_then_limit_then_current_long_is_legal() {
        let d = Dpb::default();
        let p = d
            .plan(
                picture(5),
                &Marking::Adaptive(vec![
                    Operation::ForgetAll,
                    Operation::LimitLong { max_index_plus1: 2 },
                    Operation::CurrentLong { index: 1 },
                ]),
                LIMITS,
            )
            .unwrap();
        assert_eq!(p.current.unwrap().long_term, Some(1));
    }
    #[test]
    fn forbidden_mmco_orders_and_unknown_long_bounds_are_atomic() {
        let d = Dpb::default();
        for ops in [
            vec![Operation::CurrentLong { index: 0 }],
            vec![
                Operation::ForgetAll,
                Operation::ForgetShort {
                    difference_minus1: 0,
                },
            ],
            vec![
                Operation::LimitLong { max_index_plus1: 1 },
                Operation::LimitLong { max_index_plus1: 1 },
            ],
            vec![
                Operation::LimitLong { max_index_plus1: 1 },
                Operation::CurrentLong { index: 0 },
                Operation::ForgetAll,
            ],
            vec![
                Operation::LimitLong { max_index_plus1: 1 },
                Operation::CurrentLong { index: 0 },
                Operation::ForgetLong { pic_num: 0 },
            ],
        ] {
            assert!(d.plan(picture(1), &Marking::Adaptive(ops), LIMITS).is_err());
            assert_eq!(d, Dpb::default());
        }
    }
    #[test]
    fn nonreference_has_no_dpb_write_but_advances_job_generation() {
        let mut d = Dpb::default();
        step(&mut d, 0, Marking::Sliding);
        let p = d.plan(picture(1), &Marking::NonReference, LIMITS).unwrap();
        assert_eq!(p.next.entries, d.entries);
        assert!(p.current.is_none());
        assert!(p.removed.is_empty());
        assert_eq!(p.next.generation, 2);
    }
    #[test]
    fn mmco6_capacity_is_checked_before_later_removal() {
        let mut d = Dpb::default();
        for n in 0..3 {
            step(&mut d, n, Marking::Sliding);
        }
        let bound = Operation::LimitLong { max_index_plus1: 1 };
        let insert = Operation::CurrentLong { index: 0 };
        let remove = Operation::ForgetShort {
            difference_minus1: 2,
        };
        assert!(
            d.plan(
                picture(3),
                &Marking::Adaptive(vec![bound.clone(), insert.clone(), remove.clone()]),
                LIMITS
            )
            .err()
            .unwrap()
            .contains("immediately")
        );
        let plan = d
            .plan(
                picture(3),
                &Marking::Adaptive(vec![bound, remove, insert]),
                LIMITS,
            )
            .unwrap();
        assert_eq!(plan.next.entries.len(), 3);
    }
    #[test]
    fn current_long_picture_cannot_be_replaced_or_limited_away() {
        let mut d = Dpb::default();
        step(&mut d, 0, Marking::Sliding);
        let first = vec![
            Operation::LimitLong { max_index_plus1: 1 },
            Operation::CurrentLong { index: 0 },
        ];
        for forbidden in [
            Operation::Promote {
                difference_minus1: 0,
                index: 0,
            },
            Operation::LimitLong { max_index_plus1: 0 },
            Operation::CurrentLong { index: 0 },
            Operation::ForgetLong { pic_num: 0 },
        ] {
            let mut ops = first.clone();
            ops.push(forbidden);
            assert!(d.plan(picture(1), &Marking::Adaptive(ops), LIMITS).is_err());
        }
    }
    #[test]
    fn sliding_uses_at_least_one_slot_but_cannot_evict_only_long_picture() {
        let limits = Limits {
            max_frame_num: 16,
            max_refs: 0,
        };
        let d = Dpb::default();
        let plan = d.plan(picture(0), &Marking::Sliding, limits).unwrap();
        let next = plan
            .next
            .plan(picture(1), &Marking::Sliding, limits)
            .unwrap();
        assert_eq!(next.next.entries.len(), 1);
        assert_eq!(next.removed, vec![1]);
        let long = d
            .plan(
                picture(0),
                &Marking::Idr {
                    no_output_prior: false,
                    long_term: true,
                },
                limits,
            )
            .unwrap();
        assert!(
            long.next
                .plan(picture(1), &Marking::Sliding, limits)
                .is_err()
        );
    }
    #[test]
    fn idr_retires_all_prior_generations_without_reusing_identity() {
        let mut d = Dpb::default();
        step(&mut d, 0, Marking::Sliding);
        step(&mut d, 1, Marking::Sliding);
        let plan = step(
            &mut d,
            0,
            Marking::Idr {
                no_output_prior: true,
                long_term: false,
            },
        );
        assert_eq!(plan.removed, vec![1, 2]);
        assert_eq!(plan.current.unwrap().generation, 3);
        assert_eq!(d.entries.len(), 1);
        assert_eq!(d.max_long_term_index, None);
    }
}
