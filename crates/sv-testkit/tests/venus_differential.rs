//! The differential test of design.md §6.2 and §7.3.
//!
//! > differential-test generated marshalling round-trips rather than trusting
//! > symmetry. Stronger: compare byte-for-byte against Venus's MIT-licensed
//! > generated C on the shared subset, which is independent evidence rather
//! > than self-agreement.
//! > — `security-model.md` R8
//!
//! See `crates/sv-testkit/src/venus.rs` for what is and is not compared, and why
//! byte-for-byte was not achievable given design decision D6.

use sv_testkit::{diff, venus};

fn run() -> Option<(diff::DiffReport, usize)> {
    if vk_registry::registry_path().is_err() {
        eprintln!("VK_XML unset — skipping (run inside `nix develop`)");
        return None;
    }
    let dir = venus::reference_dir();
    if !dir.is_dir() {
        eprintln!("reference/venus is absent — skipping the differential test");
        return None;
    }
    let venus_model = venus::VenusModel::load(&dir).expect("reading the reference headers");
    let plan = sv_testkit::plan_for(&vk_registry::profiles::zink()).expect("building the plan");
    let n = venus_model.structs.len();
    Some((diff::compare(&plan, &venus_model), n))
}

#[test]
fn our_encoding_agrees_with_venus_on_every_shared_struct() {
    let Some((report, venus_total)) = run() else { return };

    eprintln!(
        "venus differential: {} structs parsed from the reference headers; \
         {} shared with our profile and agreeing; {} only ours; {} only Venus's; \
         {} members not compared",
        venus_total,
        report.agreed.len(),
        report.only_ours.len(),
        report.only_venus.len(),
        report.not_compared.len(),
    );

    // The comparison is only evidence if it actually compared something. A
    // parser that silently matched nothing would produce a green test and no
    // information, which is the failure mode this whole exercise exists to
    // avoid.
    assert!(
        report.agreed.len() > 150,
        "only {} structs were compared — the reference parser is probably broken, \
         and a differential test that compares nothing is worse than none",
        report.agreed.len()
    );

    if !report.divergences.is_empty() {
        let mut msg = format!(
            "{} struct(s) encode differently from Venus. Each one is a bug in \
             one of the two generators (security-model.md R8):\n",
            report.divergences.len()
        );
        for d in report.divergences.iter().take(40) {
            msg.push_str(&format!("  {}: {}\n", d.struct_name, d.detail));
        }
        if report.divergences.len() > 40 {
            msg.push_str(&format!("  … and {} more\n", report.divergences.len() - 40));
        }
        panic!("{msg}");
    }
}

/// A negative control.
///
/// A differential test can pass because the two things agree, or because the
/// comparison is broken and would accept anything. This deliberately corrupts
/// one struct and requires the comparison to notice — so a green run above is
/// evidence rather than a formality.
#[test]
fn the_comparison_detects_a_planted_disagreement() {
    let Some(_) = run() else { return };

    let dir = venus::reference_dir();
    let venus_model = venus::VenusModel::load(&dir).unwrap();
    let mut plan = sv_testkit::plan_for(&vk_registry::profiles::zink()).unwrap();

    // Find a struct both sides know and that has at least one array member,
    // then read that array as a single value — the exact mistake a wrong `len=`
    // would cause.
    let target = plan
        .structs
        .iter_mut()
        .find(|s| {
            venus_model.structs.contains_key(&s.name)
                && s.fields.iter().any(|f| matches!(f.shape, sv_codegen::plan::Shape::Array { .. }))
        })
        .expect("some shared struct has an array member");
    let name = target.name.clone();
    for f in &mut target.fields {
        if let sv_codegen::plan::Shape::Array { elem, .. } = f.shape.clone() {
            f.shape = sv_codegen::plan::Shape::Optional(elem);
            break;
        }
    }

    let report = diff::compare(&plan, &venus_model);
    assert!(
        report.divergences.iter().any(|d| d.struct_name == name),
        "the comparison failed to notice an array turned into a single value in {name}; \
         it is not testing anything"
    );
}
