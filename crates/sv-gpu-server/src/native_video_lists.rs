//! Progressive H.264 reference list construction, ITU-T H.264 8.2.4.2/8.2.4.3.
//! Results are stable DPB generations. No generation is an image association
//! until the caller validates it against the exact submitted resource binding.
use super::{
    dpb::{Dpb, Entry, Picture},
    h264::{Modification, Parsed},
};
fn pic_num(entry: &Entry, current: u32, maximum: u32) -> i64 {
    i64::from(entry.frame_num)
        - if entry.frame_num > current {
            i64::from(maximum)
        } else {
            0
        }
}
fn poc(entry: &Entry) -> i32 {
    entry.poc[0].min(entry.poc[1])
}
fn defaults(dpb: &Dpb, current: Picture, maximum: u32, bi: bool) -> [Vec<u64>; 2] {
    let short: Vec<_> = dpb
        .entries
        .iter()
        .filter(|e| e.long_term.is_none())
        .collect();
    let mut long: Vec<_> = dpb
        .entries
        .iter()
        .filter(|e| e.long_term.is_some())
        .collect();
    long.sort_by_key(|e| e.long_term.unwrap());
    let long: Vec<_> = long.iter().map(|e| e.generation).collect();
    if !bi {
        let mut short = short;
        short.sort_by_key(|e| std::cmp::Reverse(pic_num(e, current.frame_num, maximum)));
        let mut list: Vec<_> = short.iter().map(|e| e.generation).collect();
        list.extend(long);
        return [list, Vec::new()];
    }
    let current_poc = current.poc[0].min(current.poc[1]);
    let mut below: Vec<_> = short
        .iter()
        .copied()
        .filter(|e| poc(e) < current_poc)
        .collect();
    let mut at_or_above: Vec<_> = short
        .iter()
        .copied()
        .filter(|e| poc(e) >= current_poc)
        .collect();
    below.sort_by_key(|e| std::cmp::Reverse(poc(e)));
    at_or_above.sort_by_key(|e| poc(e));
    let mut list0: Vec<_> = below
        .iter()
        .chain(at_or_above.iter())
        .map(|e| e.generation)
        .collect();
    list0.extend(&long);
    let mut above: Vec<_> = short
        .iter()
        .copied()
        .filter(|e| poc(e) > current_poc)
        .collect();
    let mut at_or_below: Vec<_> = short
        .iter()
        .copied()
        .filter(|e| poc(e) <= current_poc)
        .collect();
    above.sort_by_key(|e| poc(e));
    at_or_below.sort_by_key(|e| std::cmp::Reverse(poc(e)));
    let mut list1: Vec<_> = above
        .iter()
        .chain(at_or_below.iter())
        .map(|e| e.generation)
        .collect();
    list1.extend(long);
    if list1.len() > 1 && list1 == list0 {
        list1.swap(0, 1)
    }
    [list0, list1]
}
fn modified(
    dpb: &Dpb,
    current: Picture,
    maximum: u32,
    initial: &[u64],
    active: u32,
    operations: &[Modification],
) -> Result<Vec<u64>, String> {
    if active > 16 || operations.len() > active as usize {
        return Err("reference modifications exceed active list bound".into());
    }
    let count = active as usize;
    let mut list: Vec<Option<u64>> = initial.iter().take(count).copied().map(Some).collect();
    list.resize(count, None);
    let mut prediction = i64::from(current.frame_num);
    for (index, operation) in operations.iter().enumerate() {
        let generation = match *operation {
            Modification::Subtract(diff) | Modification::Add(diff) => {
                if diff >= maximum {
                    return Err("reference-list difference exceeds MaxPicNum".into());
                }
                let distance = i64::from(diff) + 1;
                prediction = if matches!(operation, Modification::Subtract(_)) {
                    (prediction - distance).rem_euclid(i64::from(maximum))
                } else {
                    (prediction + distance).rem_euclid(i64::from(maximum))
                };
                let target = prediction
                    - if prediction > i64::from(current.frame_num) {
                        i64::from(maximum)
                    } else {
                        0
                    };
                dpb.entries
                    .iter()
                    .find(|e| {
                        e.long_term.is_none() && pic_num(e, current.frame_num, maximum) == target
                    })
                    .ok_or("slice selects an absent short-term reference")?
                    .generation
            }
            Modification::Long(number) => {
                dpb.entries
                    .iter()
                    .find(|e| e.long_term == Some(number))
                    .ok_or("slice selects an absent long-term reference")?
                    .generation
            }
        };
        // 8.2.4.3 temporarily extends the list by one. Remove duplicate target
        // entries only after this insertion; earlier explicit duplicates remain.
        list.insert(index, Some(generation));
        let mut tail: Vec<_> = list
            .drain(index + 1..)
            .filter(|e| *e != Some(generation))
            .collect();
        list.append(&mut tail);
        list.truncate(count);
        list.resize(count, None);
    }
    list.into_iter()
        .map(|generation| {
            generation.ok_or_else(|| "active reference list contains no-reference picture".into())
        })
        .collect()
}
impl Dpb {
    /// Run against the DPB before the current picture's marking operations.
    pub(super) fn reference_lists(
        &self,
        current: Picture,
        parsed: &Parsed,
    ) -> Result<Vec<[Vec<u64>; 2]>, String> {
        if parsed.frame_num != current.frame_num
            || !(16..=65536).contains(&parsed.max_frame_num)
            || !parsed.max_frame_num.is_power_of_two()
        {
            return Err("reference-list frame identity or limits mismatch".into());
        }
        let mut result = Vec::with_capacity(parsed.slices.len());
        for slice in &parsed.slices {
            if slice.kind == 2 {
                if slice.active != [0, 0] || slice.modifications.iter().any(|list| !list.is_empty())
                {
                    return Err("I slice cannot select reference lists".into());
                }
                result.push([Vec::new(), Vec::new()]);
                continue;
            }
            if slice.kind > 1 || self.entries.is_empty() {
                return Err("inter slice has no decoded reference state".into());
            }
            let initial = defaults(self, current, parsed.max_frame_num, slice.kind == 1);
            let list0 = modified(
                self,
                current,
                parsed.max_frame_num,
                &initial[0],
                slice.active[0],
                &slice.modifications[0],
            )?;
            let list1 = if slice.kind == 1 {
                modified(
                    self,
                    current,
                    parsed.max_frame_num,
                    &initial[1],
                    slice.active[1],
                    &slice.modifications[1],
                )?
            } else {
                if slice.active[1] != 0 || !slice.modifications[1].is_empty() {
                    return Err("P slice cannot select L1".into());
                }
                Vec::new()
            };
            result.push([list0, list1]);
        }
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::super::h264::{Marking, Poc, Slice};
    use super::*;
    fn dpb() -> Dpb {
        Dpb {
            generation: 5,
            max_long_term_index: Some(3),
            entries: vec![
                Entry {
                    generation: 1,
                    frame_num: 14,
                    poc: [0, 0],
                    long_term: None,
                },
                Entry {
                    generation: 2,
                    frame_num: 15,
                    poc: [4, 4],
                    long_term: None,
                },
                Entry {
                    generation: 3,
                    frame_num: 0,
                    poc: [8, 8],
                    long_term: None,
                },
                Entry {
                    generation: 4,
                    frame_num: 9,
                    poc: [-2, -2],
                    long_term: Some(3),
                },
                Entry {
                    generation: 5,
                    frame_num: 10,
                    poc: [-4, -4],
                    long_term: Some(0),
                },
            ],
        }
    }
    fn parsed(kind: u32, active: [u32; 2], mods: [Vec<Modification>; 2]) -> Parsed {
        Parsed {
            idr: false,
            reference: true,
            frame_num: 1,
            pps_id: 0,
            idr_pic_id: None,
            poc: Poc::default(),
            max_frame_num: 16,
            max_refs: 5,
            marking: Marking::Sliding,
            slices: vec![Slice {
                first_mb: 0,
                kind,
                active,
                modifications: mods,
            }],
        }
    }
    fn current() -> Picture {
        Picture {
            frame_num: 1,
            poc: [6, 6],
        }
    }
    #[test]
    fn p_orders_wrapped_short_refs_then_long_indices() {
        let lists = dpb()
            .reference_lists(current(), &parsed(0, [5, 0], [vec![], vec![]]))
            .unwrap();
        assert_eq!(lists[0][0], [3, 2, 1, 5, 4]);
        assert!(lists[0][1].is_empty());
    }
    #[test]
    fn b_orders_each_side_of_current_poc_then_long_indices() {
        let lists = dpb()
            .reference_lists(current(), &parsed(1, [5, 5], [vec![], vec![]]))
            .unwrap();
        assert_eq!(lists[0][0], [2, 1, 3, 5, 4]);
        assert_eq!(lists[0][1], [3, 2, 1, 5, 4]);
    }
    #[test]
    fn b_swaps_identical_defaults_before_active_truncation() {
        let lists = dpb()
            .reference_lists(
                Picture {
                    frame_num: 1,
                    poc: [10, 10],
                },
                &parsed(1, [1, 1], [vec![], vec![]]),
            )
            .unwrap();
        assert_eq!(lists[0][0], [3]);
        assert_eq!(lists[0][1], [2]);
    }
    #[test]
    fn subtract_add_and_long_modifications_select_exact_generations() {
        let lists = dpb()
            .reference_lists(
                current(),
                &parsed(
                    0,
                    [4, 0],
                    [
                        vec![
                            Modification::Subtract(2),
                            Modification::Add(1),
                            Modification::Long(3),
                        ],
                        vec![],
                    ],
                ),
            )
            .unwrap();
        assert_eq!(lists[0][0], [1, 3, 4, 2]);
    }
    #[test]
    fn explicit_repeated_ref_can_fill_initial_no_reference_entry() {
        let mut d = dpb();
        d.entries.truncate(1);
        let lists = d
            .reference_lists(
                current(),
                &parsed(
                    0,
                    [2, 0],
                    [
                        vec![Modification::Subtract(2), Modification::Subtract(15)],
                        vec![],
                    ],
                ),
            )
            .unwrap();
        assert_eq!(lists[0][0], [1, 1]);
    }
    #[test]
    fn missing_implicit_or_modified_references_are_refused() {
        let d = dpb();
        assert!(
            d.reference_lists(current(), &parsed(0, [6, 0], [vec![], vec![]]))
                .is_err()
        );
        assert!(
            d.reference_lists(
                current(),
                &parsed(0, [1, 0], [vec![Modification::Long(2)], vec![]])
            )
            .is_err()
        );
        assert!(
            d.reference_lists(
                current(),
                &parsed(0, [1, 0], [vec![Modification::Subtract(16)], vec![]])
            )
            .is_err()
        );
    }
    #[test]
    fn list_lookup_uses_prior_state_and_never_commits_marking() {
        let d = dpb();
        let before = d.clone();
        let mut p = parsed(0, [1, 0], [vec![], vec![]]);
        p.marking = Marking::Adaptive(vec![super::super::h264::Operation::ForgetAll]);
        assert_eq!(d.reference_lists(current(), &p).unwrap()[0][0], [3]);
        assert_eq!(d, before);
    }
}
