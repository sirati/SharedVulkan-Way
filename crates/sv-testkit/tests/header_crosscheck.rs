//! The second independent check on the generator: our reading of `vk.xml`
//! against Khronos's, as published in `vulkan_core.h`.
//!
//! `security-model.md` R8 — "generated code trusting the registry", the only
//! risk in this project that has actually fired, and it fired twice — produces
//! one rule:
//!
//! > **every derived numeric value gets at least one assertion against an
//! > independently published number.** Round-trip tests establish that we are
//! > self-consistent; only an external fixture establishes that we are right.
//!
//! Three such assertions were written by hand after the first instance. This is
//! that rule applied to everything: every constant, every enum value, every
//! `sType`, the resolved command set of every profile, every struct member and
//! its width, every handle and every alias.
//!
//! See `crate::cheader` for why the published header is the reference and how
//! design.md §7.1's fixture-drift objection is answered (it is not a fixture: it
//! comes out of the same nix store path as `$VK_XML`).
//!
//! # It has been run against the real bug, not only against planted ones
//!
//! The negative controls below plant a fault and require the check to notice.
//! That is necessary and it is not quite sufficient, because a planted fault is
//! chosen by the same person who wrote the check. So the check was also run
//! against R8's first realised instance itself: reverting
//! `extension_enum_value`'s `!*dir` to `*dir` in `vk-registry` — the single
//! character that inverted every extension-introduced enum value in the registry
//! and that every round-trip test agreed with — produces **2,097 divergences**
//! across `EnumValue` and `StructType`, beginning
//! `VkAttachmentLoadOp::VK_ATTACHMENT_LOAD_OP_NONE: ours -1000400000,
//! vulkan_core.h 1000400000`. The historical bug is caught, in bulk, by the
//! ordinary run.

use std::sync::OnceLock;
use sv_testkit::cheader::{self, HeaderModel};
use sv_testkit::xcheck::{self, Category, XReport};
use vk_registry::{Profile, profiles};

struct Fixture {
    header: HeaderModel,
    zink: sv_testkit::Built,
    zink_profile: Profile,
    minimal: sv_testkit::Built,
    minimal_profile: Profile,
}

/// Built once: parsing `vk.xml`, resolving two profiles and reading a 26,000
/// line header is work worth doing once rather than per test.
fn fixture() -> Option<&'static Fixture> {
    static F: OnceLock<Option<Fixture>> = OnceLock::new();
    F.get_or_init(|| {
        if vk_registry::registry_path().is_err() {
            eprintln!("VK_XML unset — skipping (run inside `nix develop`)");
            return None;
        }
        let paths = cheader::header_paths().expect("locating vulkan_core.h beside the registry");
        let header = HeaderModel::load_all(&paths).expect("reading the generated headers");
        let zink_profile = profiles::zink();
        let minimal_profile = profiles::step3_minimal();
        Some(Fixture {
            header,
            zink: sv_testkit::build_for(&zink_profile).expect("building the zink plan"),
            zink_profile,
            minimal: sv_testkit::build_for(&minimal_profile).expect("building the minimal plan"),
            minimal_profile,
        })
    })
    .as_ref()
}

/// Fail with every divergence in the given categories, or pass.
fn require_clean(report: &XReport, categories: &[Category], what: &str) {
    let bad: Vec<&xcheck::Divergence> = report
        .sorted()
        .into_iter()
        .filter(|d| categories.contains(&d.category))
        .collect();
    if bad.is_empty() {
        return;
    }
    let mut msg = format!(
        "{} disagreement(s) between our generator and vulkan_core.h in {what}.\n\
         Each one means one of the two readings of vk.xml is wrong. Establish which \
         before changing either (security-model.md R8):\n",
        bad.len()
    );
    for d in bad.iter().take(60) {
        msg.push_str(&format!("  [{:?}] {}: {}\n", d.category, d.subject, d.detail));
    }
    if bad.len() > 60 {
        msg.push_str(&format!("  … and {} more\n", bad.len() - 60));
    }
    panic!("{msg}");
}

fn zink_report(f: &Fixture) -> XReport {
    let mut r = XReport::default();
    xcheck::compare(&f.zink.model, &f.zink.plan, &f.zink_profile, &f.header, &mut r);
    r
}

// ---------------------------------------------------------------------------
// provenance: the two artefacts must be the same release
// ---------------------------------------------------------------------------

/// The trap design.md §7.1 names, closed.
///
/// The objection to keeping a generated model as a fixture is that the protocol
/// becomes a function of an artefact that can drift from `$VK_XML`. This
/// reference cannot: it is read out of the same nix store path. That is asserted
/// here rather than assumed, because "the same store path" is a property of how
/// `header_path` derives one from the other, and a property worth failing on if
/// it ever stops holding.
#[test]
fn the_header_and_the_registry_are_the_same_release() {
    let Some(f) = fixture() else { return };

    let xml_path = vk_registry::registry_path().unwrap();
    let header_path = cheader::header_path().unwrap();
    let xml_text = std::fs::read_to_string(&xml_path).unwrap();

    let registry_version = cheader::registry_header_version(&xml_text)
        .expect("vk.xml states a VK_HEADER_VERSION");
    let header_version = f.header.header_version.expect("vulkan_core.h defines VK_HEADER_VERSION");

    eprintln!(
        "provenance: {} and {} both at VK_HEADER_VERSION {registry_version}",
        xml_path.display(),
        header_path.display()
    );
    assert_eq!(
        registry_version, header_version,
        "the registry and the header disagree about which Vulkan version they are \
         ({registry_version} vs {header_version}). They are supposed to come out of one \
         package; a mismatch means this cross-check is comparing two different APIs."
    );

    // And the parse actually found things, so that a silently empty header model
    // cannot make every check below vacuously green.
    assert!(f.header.enums.len() > 300, "only {} enums parsed", f.header.enums.len());
    assert!(f.header.structs.len() > 1200, "only {} structs parsed", f.header.structs.len());
    assert!(f.header.commands.len() > 700, "only {} commands parsed", f.header.commands.len());
    assert!(f.header.handles.len() > 40, "only {} handles parsed", f.header.handles.len());
    assert!(f.header.defines.len() > 20, "only {} defines parsed", f.header.defines.len());
}

// ---------------------------------------------------------------------------
// priority 1: constants and enum values — the unguarded blind spot
// ---------------------------------------------------------------------------

/// Every constant and every enum value we generate, against the published
/// number.
///
/// This is the check R8's first realised instance needed. The sign inversion in
/// `EnumSpec::Offset` changed every extension-introduced value in the registry;
/// the round-trips agreed with it, the Venus differential does not look at
/// constants at all, and three hand-written assertions caught it. Here the
/// assertion covers all of them.
#[test]
fn every_constant_and_enum_value_matches_vulkan_core_h() {
    let Some(f) = fixture() else { return };
    let r = zink_report(f);
    eprintln!("{}", xcheck::summary(&r));

    // A comparison that compared nothing would be a green light with nothing
    // behind it. The bounds are the measured counts less a margin — 38, 3406 and
    // 260 for the pinned registry — so that a registry bump does not trip them
    // but a parser that silently matched nothing does.
    assert!(r.counts.constants > 30, "only {} constants compared", r.counts.constants);
    assert!(r.counts.enum_values > 3000, "only {} enum values compared", r.counts.enum_values);
    assert!(r.counts.s_types > 200, "only {} sTypes compared", r.counts.s_types);

    require_clean(
        &r,
        &[Category::Constant, Category::EnumValue, Category::StructType],
        "constants and enum values",
    );
}

// ---------------------------------------------------------------------------
// priority 2: the resolved command set — the 55-vs-239 failure
// ---------------------------------------------------------------------------

/// What each profile resolves to, against the header's own feature blocks.
///
/// The header does not reproduce the `apitype="internal"` sub-feature chain that
/// R8's second instance turned on — it writes the resolved result — so agreeing
/// with it means our `depends=` evaluation agrees with Khronos's, by a route
/// that never reads a `depends=` expression.
#[test]
fn the_resolved_command_set_matches_vulkan_core_h() {
    let Some(f) = fixture() else { return };

    for (built, profile) in [(&f.zink, &f.zink_profile), (&f.minimal, &f.minimal_profile)] {
        let mut r = XReport::default();
        xcheck::check_command_set(profile, &built.plan, &f.header, &mut r);
        eprintln!(
            "profile {:?}: vulkan_core.h's blocks yield {} command(s); we generate {} \
             definition(s) plus {} alias(es)",
            profile.name,
            r.counts.commands_in_set,
            built.plan.commands.len(),
            built.plan.commands.iter().map(|c| c.aliases.len()).sum::<usize>(),
        );
        require_clean(&r, &[Category::CommandSet], &format!("the {:?} command set", profile.name));
    }

    // The bound the second instance tripped over, restated against a number that
    // is not ours: `#define VK_VERSION_1_0 1` is followed by 143 command
    // typedefs in the pinned header, and a resolver that reads
    // `<feature name="VK_VERSION_1_0">` and stops gets none of them.
    let v10 = f.header.sections.get("VK_VERSION_1_0").map(Vec::len).unwrap_or(0);
    assert!(
        v10 > 130,
        "vulkan_core.h's VK_VERSION_1_0 block yielded only {v10} commands — the section \
         parser is broken, and this check would then pass by comparing nothing"
    );
}

// ---------------------------------------------------------------------------
// priority 3: struct member order, types and widths
// ---------------------------------------------------------------------------

#[test]
fn every_struct_member_matches_vulkan_core_h() {
    let Some(f) = fixture() else { return };
    let r = zink_report(f);

    assert!(r.counts.structs > 300, "only {} structs compared", r.counts.structs);
    assert!(r.counts.members > 1500, "only {} members compared", r.counts.members);
    assert!(r.counts.widths > 1200, "only {} widths compared", r.counts.widths);

    require_clean(&r, &[Category::Member, Category::Width], "struct members and widths");
}

#[test]
fn every_command_signature_matches_vulkan_core_h() {
    let Some(f) = fixture() else { return };
    let r = zink_report(f);

    assert!(r.counts.signatures > 200, "only {} signatures compared", r.counts.signatures);
    assert!(r.counts.params > 700, "only {} parameters compared", r.counts.params);

    require_clean(&r, &[Category::Signature], "command signatures");
}

// ---------------------------------------------------------------------------
// priority 4: handles and aliases
// ---------------------------------------------------------------------------

#[test]
fn every_handle_and_alias_matches_vulkan_core_h() {
    let Some(f) = fixture() else { return };
    let r = zink_report(f);

    assert!(r.counts.handles > 25, "only {} handles compared", r.counts.handles);
    assert!(r.counts.aliases > 60, "only {} aliases compared", r.counts.aliases);

    require_clean(&r, &[Category::Handle, Category::Alias], "handles and aliases");
}

// ---------------------------------------------------------------------------
// negative controls
// ---------------------------------------------------------------------------
//
// A differential that has never failed is not evidence: it can be green because
// the two sides agree, or because the comparison would accept anything. Each
// control below breaks our side in exactly the way a real bug would and requires
// the check to notice.

/// R8's first realised instance, replayed.
///
/// The `EnumSpec::Offset` sign inversion negated every extension-introduced enum
/// value. This negates one, and requires the check to say so.
#[test]
fn a_flipped_extension_enum_value_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.zink.plan.clone();

    let target = "VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO";
    let e = plan
        .enums
        .iter_mut()
        .find(|e| e.name == "VkStructureType")
        .expect("VkStructureType is generated");
    let v = e
        .values
        .iter_mut()
        .find(|(n, _)| n == target)
        .expect("the extension-introduced value is generated");
    assert_eq!(v.1, 1_000_072_000, "the published value, before corruption");
    v.1 = -v.1;

    let mut r = XReport::default();
    xcheck::check_enum_values(&f.zink.model, &plan, &f.header, &mut r);
    assert!(
        r.in_category(Category::EnumValue).iter().any(|d| d.subject.contains(target)),
        "the cross-check did not notice an extension enum value with its sign inverted — \
         which is the exact bug R8 records as realised instance 1"
    );
}

/// A constant resolved to the wrong number, which is an array of the wrong
/// length everywhere it is used.
#[test]
fn a_wrong_api_constant_is_caught() {
    let Some(f) = fixture() else { return };
    let mut model = f.zink.model.clone();
    let c = model
        .constants
        .get_mut("VK_MAX_PHYSICAL_DEVICE_NAME_SIZE")
        .expect("the constant is in the model");
    assert_eq!(c.value, Some(256), "the published value, before corruption");
    c.value = Some(255);

    let mut r = XReport::default();
    xcheck::check_constants(&model, &f.header, &mut r);
    assert!(
        r.in_category(Category::Constant)
            .iter()
            .any(|d| d.subject == "VK_MAX_PHYSICAL_DEVICE_NAME_SIZE"),
        "the cross-check did not notice an API constant off by one"
    );
}

/// R8's second realised instance, replayed.
///
/// The Zink profile resolved to 55 commands instead of 239 and reported no
/// error. Dropping most of the command set must be loud.
#[test]
fn a_silently_shrunken_command_set_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.zink.plan.clone();
    let before = plan.commands.len();
    plan.commands.truncate(55);

    let mut r = XReport::default();
    xcheck::check_command_set(&f.zink_profile, &plan, &f.header, &mut r);
    let missing = r.in_category(Category::CommandSet).len();
    assert!(
        missing > 100,
        "cutting {before} commands down to 55 produced only {missing} complaint(s); this is \
         the shape of R8's second realised instance and it has to be loud"
    );
}

/// A member sequence that no longer matches the C declaration — the shape a
/// misread declarator produces.
#[test]
fn a_reordered_struct_member_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.zink.plan.clone();
    let s = plan
        .structs
        .iter_mut()
        .find(|s| s.name == "VkBufferCreateInfo")
        .expect("VkBufferCreateInfo is generated");
    let (i, j) = (1, 2);
    let names = (s.fields[i].name.clone(), s.fields[j].name.clone());
    s.fields.swap(i, j);

    let mut r = XReport::default();
    xcheck::check_structs(&plan, &f.header, &mut r);
    assert!(
        r.in_category(Category::Member).iter().any(|d| d.subject.starts_with("VkBufferCreateInfo")),
        "the cross-check did not notice {} and {} swapped in VkBufferCreateInfo",
        names.0,
        names.1
    );
}

/// A scalar at the wrong width: four bytes read where eight were written is the
/// classic silent corruption R8 is about.
#[test]
fn a_member_at_the_wrong_width_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.zink.plan.clone();
    let s = plan
        .structs
        .iter_mut()
        .find(|s| s.name == "VkBufferCreateInfo")
        .expect("VkBufferCreateInfo is generated");
    let field = s.fields.iter_mut().find(|f| f.name == "size").expect("it has a size member");
    // `VkDeviceSize size` is 64 bits. Read it as 32.
    field.shape = sv_codegen::plan::Shape::Scalar(sv_codegen::plan::Scalar::U32);

    let mut r = XReport::default();
    xcheck::check_structs(&plan, &f.header, &mut r);
    assert!(
        !r.in_category(Category::Member).is_empty() || !r.in_category(Category::Width).is_empty(),
        "the cross-check did not notice VkBufferCreateInfo.size narrowed from 64 to 32 bits"
    );
}

/// A 64-bit bitmask read as 32 bits.
///
/// This is the control for the *width* comparison specifically, rather than for
/// the type-name comparison: the name still matches — it is still
/// `VkPipelineStageFlags2` on both sides — and only the number of bytes differs.
/// The bug it stands in for is real and one line wide: `vk-registry` decides a
/// bitmask's width by looking for `VkFlags64` in the typedef's code, and half of
/// a 64-bit mask silently dropped is exactly R8's kind of quiet.
#[test]
fn a_sixty_four_bit_mask_read_as_thirty_two_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.zink.plan.clone();

    let mut corrupted = None;
    'outer: for s in plan.structs.iter_mut() {
        for field in s.fields.iter_mut() {
            if let sv_codegen::plan::Shape::Scalar(sv_codegen::plan::Scalar::Bitmask64(n)) =
                field.shape.clone()
            {
                field.shape = sv_codegen::plan::Shape::Scalar(sv_codegen::plan::Scalar::Bitmask(n));
                corrupted = Some(format!("{}.{}", s.name, field.name));
                break 'outer;
            }
        }
    }
    let corrupted = corrupted.expect("the zink profile has a VkFlags64 member");

    let mut r = XReport::default();
    xcheck::check_structs(&plan, &f.header, &mut r);
    assert!(
        r.in_category(Category::Width).iter().any(|d| d.subject == corrupted),
        "the width comparison did not notice {corrupted} narrowed from 64 bits to 32 while \
         keeping the same type name — which means the width comparison is not testing anything"
    );
}

/// A fixed array whose extent came out wrong — `char deviceName[256]` read as
/// something else.
#[test]
fn a_wrong_array_extent_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.zink.plan.clone();
    let mut corrupted = None;
    'outer: for s in plan.structs.iter_mut() {
        for field in s.fields.iter_mut() {
            if let sv_codegen::plan::Shape::FixedArray { count, .. } = &mut field.shape
                && *count > 1
            {
                *count -= 1;
                corrupted = Some(format!("{}.{}", s.name, field.name));
                break 'outer;
            }
        }
    }
    let corrupted = corrupted.expect("some struct has a fixed-extent array");

    let mut r = XReport::default();
    xcheck::check_structs(&plan, &f.header, &mut r);
    assert!(
        r.in_category(Category::Member).iter().any(|d| d.subject == corrupted),
        "the cross-check did not notice a fixed array extent off by one in {corrupted}"
    );
}

/// A handle on the wrong side of the dispatchable line.
#[test]
fn a_mislabelled_handle_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.zink.plan.clone();
    let h = plan
        .handles
        .iter_mut()
        .find(|h| h.name == "VkBuffer")
        .expect("VkBuffer is generated");
    assert!(!h.dispatchable, "VkBuffer is non-dispatchable, before corruption");
    h.dispatchable = true;

    let mut r = XReport::default();
    xcheck::check_handles(&plan, &f.header, &mut r);
    assert!(
        r.in_category(Category::Handle).iter().any(|d| d.subject == "VkBuffer"),
        "the cross-check did not notice VkBuffer claimed as a dispatchable handle"
    );
}

/// An alias pointed at the wrong target, which puts two different types or
/// commands on one opcode.
#[test]
fn a_misresolved_alias_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.zink.plan.clone();
    let (alias, _) = plan
        .type_aliases
        .iter()
        .find(|(a, _)| f.header.typedefs.contains_key(*a))
        .map(|(a, t)| (a.clone(), t.clone()))
        .expect("some type alias is a typedef in the header");
    plan.type_aliases.insert(alias.clone(), "VkBuffer".to_string());

    let mut r = XReport::default();
    xcheck::check_aliases(&plan, &f.header, &mut r);
    assert!(
        r.in_category(Category::Alias).iter().any(|d| d.subject == alias),
        "the cross-check did not notice {alias} repointed at VkBuffer"
    );
}

// ---------------------------------------------------------------------------
// determinism
// ---------------------------------------------------------------------------

/// The same registry and the same header produce the same report, every time.
///
/// A check whose output depends on iteration order cannot be diffed, and a
/// divergence list nobody can diff is a list nobody reads.
#[test]
fn the_cross_check_is_deterministic() {
    let Some(f) = fixture() else { return };
    let a = zink_report(f);
    let b = zink_report(f);
    assert_eq!(a.counts, b.counts, "the counts moved between two runs over one input");
    assert_eq!(
        a.sorted(),
        b.sorted(),
        "the divergence list moved between two runs over one input"
    );
}
