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
//! docs/design.md §7.1's fixture-drift objection is answered (it is not a fixture: it
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
    /// Zink plus the three video decode extensions. It is here because it is the
    /// only profile that reaches the `StdVideo*` types, and those come from a
    /// *second* registry — `video.xml` — whose reading nothing else checks.
    video: sv_testkit::Built,
    video_profile: Profile,
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
        let video_profile = profiles::video();
        Some(Fixture {
            header,
            zink: sv_testkit::build_for(&zink_profile).expect("building the zink plan"),
            zink_profile,
            minimal: sv_testkit::build_for(&minimal_profile).expect("building the minimal plan"),
            minimal_profile,
            video: sv_testkit::build_for(&video_profile).expect("building the video plan"),
            video_profile,
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

/// The trap docs/design.md §7.1 names, closed.
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
// the video std types: a second registry, checked against its own headers
// ---------------------------------------------------------------------------

/// `video.xml`, against `vk_video/*.h`.
///
/// The generator learns the `StdVideo*` types from `video.xml` rather than from
/// the C headers, for the reasons in `vk_registry::video_registry_path`. That is
/// only defensible if the two are compared, and this is the comparison: every
/// `StdVideo*` struct the video profile carries, its member sequence, each
/// member's C type and each fixed array's extent, against the header Khronos
/// generates from the same file.
///
/// The `STD_VIDEO_*` array extents and every `StdVideo*` enum value are covered
/// too, by the ordinary constant and enum-value checks — the video headers are
/// part of `cheader::header_paths()`, so they are in the same comparison the
/// rest of the API gets rather than in a special case.
#[test]
fn the_video_std_types_match_the_vk_video_headers() {
    let Some(f) = fixture() else { return };
    let mut r = XReport::default();
    xcheck::compare(&f.video.model, &f.video.plan, &f.video_profile, &f.header, &mut r);
    eprintln!("video profile: {}", xcheck::summary(&r));

    // The counts that make this test non-vacuous, and they are about the video
    // types specifically rather than about the profile as a whole.
    let std_structs = f.video.plan.structs.iter().filter(|s| s.name.starts_with("StdVideo")).count();
    assert!(
        std_structs >= 5,
        "only {std_structs} StdVideo* struct(s) in the video profile — the merge of \
         video.xml did not happen, and every check below would then pass by comparing \
         nothing"
    );
    assert!(
        r.counts.flag_words >= 5,
        "only {} flag word(s) compared against the vk_video headers",
        r.counts.flag_words
    );
    assert!(
        r.counts.flag_bits >= 40,
        "only {} bitfield(s) compared against the vk_video headers",
        r.counts.flag_bits
    );
    // The array extents come from video.xml's own `<require>` blocks and nowhere
    // else; if `merge_video_constants` had missed them, every `StdVideo*` array
    // would have failed to resolve and the plan would not have built at all.
    assert_eq!(
        f.video.model.constants.get("STD_VIDEO_H264_SCALING_LIST_4X4_NUM_LISTS").and_then(|c| c.value),
        Some(6)
    );
    assert_eq!(
        f.video.model.constants.get("STD_VIDEO_DECODE_H264_FIELD_ORDER_COUNT_LIST_SIZE").and_then(|c| c.value),
        Some(2)
    );

    require_clean(
        &r,
        &[
            Category::Constant,
            Category::EnumValue,
            Category::StructType,
            Category::Member,
            Category::Width,
        ],
        "the video std types",
    );
}

/// The one number `docs/video-decode.md` §3 had to hand-build a table for.
///
/// `StdVideoH264LevelIdc` is an enum, not the bitstream's `level_idc` byte:
/// level 1.2 is `level_idc = 12` in the stream and enum value **2** in the
/// header. `video-probe`'s `decode::level_idc_enum` is that mapping, pinned by
/// its own test against a hand-written table. Now the generator carries the enum
/// itself, so the mapping's *target* is a published number rather than a
/// transcription — and this is the assertion that it is.
#[test]
fn the_h264_level_enum_is_the_headers_and_not_the_bitstreams() {
    let Some(f) = fixture() else { return };
    let e = f
        .video
        .plan
        .enums
        .iter()
        .find(|e| e.name == "StdVideoH264LevelIdc")
        .expect("StdVideoH264LevelIdc is generated for the video profile");
    let value = |n: &str| e.values.iter().find(|(k, _)| k == n).map(|(_, v)| *v);
    assert_eq!(value("STD_VIDEO_H264_LEVEL_IDC_1_0"), Some(0));
    assert_eq!(value("STD_VIDEO_H264_LEVEL_IDC_1_2"), Some(2), "the probe's fixture level");
    assert_eq!(value("STD_VIDEO_H264_LEVEL_IDC_5_1"), Some(14), "what Mesa 26.1.1 reports");
    assert_eq!(value("STD_VIDEO_H264_LEVEL_IDC_6_2"), Some(18), "the last one, so 52 is outside");

    // And the profile idc, which is the other by-value external type that used
    // to stop the planner.
    let p = f
        .video
        .plan
        .enums
        .iter()
        .find(|e| e.name == "StdVideoH264ProfileIdc")
        .expect("StdVideoH264ProfileIdc is generated");
    assert_eq!(
        p.values.iter().find(|(k, _)| k == "STD_VIDEO_H264_PROFILE_IDC_HIGH").map(|(_, v)| *v),
        Some(100),
        "the H.264 profile_idc of High is 100 in the bitstream and in this enum alike"
    );
}

/// All three passes generate valid Rust for the video profile.
///
/// The wire pass is checked by every build of `sv-proto`; the other two are only
/// checked by a build with `SV_PROTO_PROFILE=video`, which is not the default,
/// so nothing would notice a profile-specific emitter fault until somebody
/// tried it. One did exist and this is the test that would have found it:
/// `StdVideoH264ScalingLists::ScalingList4x4` is `uint8_t[6][16]`, the first
/// two-dimensional array any profile has reached, and the GPU side's `raise`
/// asked the whole member for its element type at both levels — emitting a loop
/// that pushed a `u8` into a `Vec<[u8; 16]>`.
///
/// `generate_*` parse their own output with `syn` before printing it, so a
/// token stream that is not valid Rust fails here rather than in a 34,000-line
/// generated file. It does not typecheck it, which is why the fault above
/// needed a compile — but it is the cheap half, and it runs every time.
#[test]
fn the_video_profile_generates_all_three_passes() {
    let Some(f) = fixture() else { return };

    let wire = sv_codegen::generate_from_model(&f.video.model, &f.video_profile)
        .expect("the wire pass generates for the video profile");
    let icd = sv_codegen::generate_icd_entry(&f.video.model, &f.video_profile)
        .expect("the ICD C surface generates for the video profile");
    let gpu = sv_codegen::generate_gpu_entry(&f.video.model, &f.video_profile)
        .expect("the GPU side generates for the video profile");
    eprintln!(
        "video profile: wire {} commands / {} structs, icd {} commands / {} types, \
         gpu {} forwarded",
        wire.stats.commands, wire.stats.structs, icd.commands, icd.types, gpu.forwarded
    );

    // Zink resolves to 239 commands and video adds the thirteen of
    // `VK_KHR_video_queue` and `VK_KHR_video_decode_queue`. Stated as a
    // difference rather than as an absolute, so a registry bump moves both.
    let zink_commands = f.zink.plan.commands.len();
    assert_eq!(
        wire.stats.commands - zink_commands,
        13,
        "the video profile should add exactly the video queue and decode-queue commands"
    );
    for name in [
        "vkCreateVideoSessionKHR",
        "vkCreateVideoSessionParametersKHR",
        "vkCmdDecodeVideoKHR",
        "vkGetPhysicalDeviceVideoCapabilitiesKHR",
    ] {
        assert!(
            f.video.plan.commands.iter().any(|c| c.name == name),
            "{name} is not in the generated video protocol"
        );
    }

    // And the generated protocol really carries the std types, by value and
    // through a pointer graph. `maxLevelIdc` is the by-value one that stopped
    // the planner; `pStdSPSs` is the deep one.
    let caps = f
        .video
        .plan
        .struct_plan("VkVideoDecodeH264CapabilitiesKHR")
        .expect("the H.264 decode capabilities struct is generated");
    let level = caps.fields.iter().find(|f| f.name == "maxLevelIdc").expect("maxLevelIdc");
    assert!(
        matches!(
            &level.shape,
            sv_codegen::plan::Shape::Scalar(sv_codegen::plan::Scalar::Enum(n))
                if n == "StdVideoH264LevelIdc"
        ),
        "maxLevelIdc should be a StdVideoH264LevelIdc enum on the wire, not {:?}",
        level.shape
    );
    let sps = f
        .video
        .plan
        .struct_plan("StdVideoH264SequenceParameterSet")
        .expect("the H.264 SPS is generated");
    // The three pointer members `docs/video-decode.md` §4 expected to have to
    // deep-copy by hand. The registry states a `len=` for one and `optional=`
    // for the other two, so the generator does it.
    let shape_of = |n: &str| sps.fields.iter().find(|f| f.name == n).map(|f| f.shape.clone());
    assert!(
        matches!(shape_of("pOffsetForRefFrame"), Some(sv_codegen::plan::Shape::Array { .. })),
        "pOffsetForRefFrame has len=num_ref_frames_in_pic_order_cnt_cycle and should be an array"
    );
    assert!(
        matches!(shape_of("pScalingLists"), Some(sv_codegen::plan::Shape::Optional(_))),
        "pScalingLists is optional=true and should be an optional value"
    );
    assert!(
        matches!(shape_of("pSequenceParameterSetVui"), Some(sv_codegen::plan::Shape::Optional(_))),
        "pSequenceParameterSetVui is optional=true and should be an optional value"
    );
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

/// A bitfield given the wrong number of bits, which moves every bit after it.
///
/// The control for `check_flag_words`. `StdVideoH264SpsFlags` is sixteen
/// one-bit fields; widening one to two silently shifts fifteen flags by a bit,
/// and a driver handed that struct decodes a different picture with no error
/// anywhere. Nothing in a round trip can see it — both sides would agree.
#[test]
fn a_bitfield_at_the_wrong_width_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.video.plan.clone();
    let fw = plan
        .flag_words
        .iter_mut()
        .find(|f| f.name == "StdVideoH264SpsFlags")
        .expect("StdVideoH264SpsFlags is a flag word of the video profile");
    assert_eq!(fw.fields[0].bits, 1, "before corruption");
    fw.fields[0].bits = 2;
    for b in fw.fields.iter_mut().skip(1) {
        b.offset += 1;
    }

    let mut r = XReport::default();
    xcheck::check_flag_words(&plan, &f.header, &mut r);
    assert!(
        !r.in_category(Category::Width).is_empty(),
        "the cross-check did not notice a one-bit flag widened to two, which shifts \
         every flag after it"
    );
}

/// Two bitfields swapped, which is the same corruption by another route.
#[test]
fn a_reordered_bitfield_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.video.plan.clone();
    let fw = plan
        .flag_words
        .iter_mut()
        .find(|f| f.name == "StdVideoDecodeH264PictureInfoFlags")
        .expect("the per-frame flags are a flag word of the video profile");
    fw.fields.swap(0, 1);

    let mut r = XReport::default();
    xcheck::check_flag_words(&plan, &f.header, &mut r);
    assert!(
        !r.in_category(Category::Member).is_empty(),
        "the cross-check did not notice two bitfields swapped"
    );
}

/// A `StdVideo*` struct whose members no longer match `video.xml`'s reading.
///
/// The video types come from a registry nothing else in the project reads, so
/// this is the control that the second reader is checked at all.
#[test]
fn a_dropped_std_video_member_is_caught() {
    let Some(f) = fixture() else { return };
    let mut plan = f.video.plan.clone();
    let s = plan
        .structs
        .iter_mut()
        .find(|s| s.name == "StdVideoH264SequenceParameterSet")
        .expect("the H.264 SPS is generated for the video profile");
    let dropped = s.fields.remove(3).name;

    let mut r = XReport::default();
    xcheck::check_structs(&plan, &f.header, &mut r);
    assert!(
        !r.in_category(Category::Member).is_empty(),
        "the cross-check did not notice {dropped} dropped from StdVideoH264SequenceParameterSet"
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

/// A command buffer whose recording failed makes its submit fail — asserted
/// over the generated source, per command, rather than described.
///
/// # Why over the source and not through a host double
///
/// The property is a *generator* decision made four different ways for four
/// kinds of command, and the thing that can go wrong is a command landing in the
/// wrong kind. A host double would exercise one command and prove nothing about
/// the other 560; reading the emitted dispatch proves it for all of them, and it
/// is the same technique `the_video_profile_generates_all_three_passes` uses one
/// test up.
///
/// The four kinds, and why each is what it is (docs/design.md §0.24):
///
/// - a recorded `vkCmd*` whose entry point the driver lacks **poisons** its
///   target, because the failure belongs to the command buffer and not to a call
///   with no reply to fail;
/// - `vkQueueSubmit` and `vkEndCommandBuffer` **refuse**, which is where the
///   failure becomes visible to an application. Since docs/video-decode.md
///   §7.12 the three of them (both submits and the end) refuse *after* a
///   reduced call reaches the driver: the end, so the driver's buffer leaves
///   the recording state; the submits without the poisoned buffers, so the
///   fence the application waits on completes;
/// - `vkBeginCommandBuffer` **clears**, because recording afresh is the only way
///   back and the specification routes every reuse through it;
/// - `vkFreeCommandBuffers` does **neither**, because a destructor that can be
///   refused is a leak.
#[test]
fn a_failed_recording_is_carried_to_the_submit_and_not_to_the_ring() {
    let Some(f) = fixture() else { return };
    let gpu = sv_codegen::generate_gpu_entry(&f.video.model, &f.video_profile)
        .expect("the GPU side generates");
    let src = &gpu.source;

    // The dispatch body for one command, from its `fn` line to the next one.
    let body = |name: &str| -> String {
        let at = src
            .find(&format!("\n    fn {name}("))
            .unwrap_or_else(|| panic!("{name} is not in the generated GPU side"));
        let rest = &src[at + 1..];
        let end = rest[1..].find("\n    fn ").map(|i| i + 1).unwrap_or(rest.len());
        rest[..end].to_string()
    };
    let refuses_before = |n: &str| body(n).contains("if let Some(__why) = __h.take_poisoned_command_buffer()");
    // Refused after a reduced driver call: the mark is taken before the call
    // and reported after it.
    let refuses_after = |n: &str| {
        let b = body(n);
        let (Some(take), Some(call), Some(report)) = (
            b.find("let __end_poison = __h.take_poisoned_command_buffer()"),
            b.find("unsafe { __f("),
            b.rfind("__h.note_poisoned_submit("),
        ) else {
            return false;
        };
        take < call && call < report
    };
    let refuses = |n: &str| refuses_before(n) || refuses_after(n);
    let poisons = |n: &str| body(n).contains("__h.poison_command_buffer(");
    let clears = |n: &str| body(n).contains("__h.unpoison_command_buffer(");

    // The one that matters. `vkQueueSubmit`'s command buffers are a member of
    // `VkSubmitInfo`, not a parameter, so a check derived from the wire
    // signature misses it — which the first version of this did. The traversal
    // is the handle-mapping pass, which visits every handle at any depth.
    assert!(refuses("queue_submit"), "vkQueueSubmit must refuse a poisoned command buffer");
    assert!(refuses("queue_submit2"), "vkQueueSubmit2 must too, two levels down");
    // The specification's own answer: recording failed, so ending fails.
    assert!(refuses("end_command_buffer"), "vkEndCommandBuffer must report a failed recording");
    // ... after the driver has ended its buffer, and the submits after the
    // driver has their fence (§7.12).
    for n in ["queue_submit", "queue_submit2", "end_command_buffer"] {
        assert!(refuses_after(n), "{n} must refuse after a reduced driver call");
    }
    assert!(body("queue_submit").contains("strip_poisoned_submit("), "vkQueueSubmit strips poisoned buffers");
    assert!(body("queue_submit2").contains("strip_poisoned_submit2("), "vkQueueSubmit2 keeps only its fence");

    // Recording afresh forgives, and must not refuse the call that forgives.
    assert!(clears("begin_command_buffer"), "vkBeginCommandBuffer must clear the mark");
    assert!(!refuses("begin_command_buffer"), "the call that clears must not be refused");

    // A destructor is never refused: freeing an invalid command buffer is
    // exactly what an application does about one.
    assert!(!refuses("free_command_buffers"), "vkFreeCommandBuffers must never be refused");

    // Every recorded command poisons rather than returning a silent `Ok(())`.
    // Counted rather than spot-checked, because "most of them" is how the one
    // that matters gets missed.
    let recorded: Vec<&str> = src
        .lines()
        .filter_map(|l| l.trim().strip_prefix("fn cmd_"))
        .filter_map(|l| l.split('(').next())
        .collect();
    assert!(recorded.len() > 80, "only {} vkCmd* found; the sweep did not run", recorded.len());
    for c in &recorded {
        let name = format!("cmd_{c}");
        // A hand-written recorded command (`GPU_HAND_WRITTEN`: the video
        // coding scope and decode, docs/video-decode.md §7.11.5) appears here
        // only as its `GpuHost` declaration, with no body; its poisoning is in
        // `sv_gpu_server::server`, written the same way.
        if !body(&name).contains('{') {
            continue;
        }
        assert!(
            poisons(&name),
            "vk{c} returns silently when the driver lacks it instead of poisoning its \
             command buffer"
        );
        assert!(!refuses(&name), "a recorded command must not refuse; the submit does");
    }
}
