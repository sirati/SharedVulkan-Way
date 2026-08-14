//! Comparing what we generate against what Khronos publishes.
//!
//! This is the second independent check `security-model.md` R8 asks for and
//! `design.md` §7.1 left outstanding. `crate::cheader` explains why the pinned
//! `vulkan_core.h` is the reference; this module is what is actually compared.
//!
//! # The order of the checks is the order of the risk
//!
//! 1. **Constants and enum values** ([`check_constants`], [`check_enum_values`],
//!    [`check_struct_types`]). This is the blind spot: the Venus differential
//!    compares member sequence, kind and width, and a constant is none of those.
//!    R8's first realised instance — the `EnumSpec::Offset` sign inversion — lived
//!    here and was caught by three hand-written assertions. These are the same
//!    assertions over every value we emit.
//! 2. **The resolved command set per profile** ([`check_command_set`]). R8's
//!    second instance was 55 commands where 239 were meant, and the header's
//!    `#define VK_VERSION_1_0 1` block is an answer to "what does 1.0 provide"
//!    arrived at without ever seeing the `apitype="internal"` sub-features that
//!    our resolver has to chase.
//! 3. **Struct member order and widths** ([`check_structs`]). Overlaps the Venus
//!    differential deliberately: that one covers 208 structs of one profile's
//!    ~700, and this one covers all of them.
//! 4. **Handles and aliases** ([`check_handles`], [`check_aliases`]).
//!
//! # What a divergence means
//!
//! Not "our generator is wrong". It means one of the two readings of `vk.xml` is
//! wrong, and which one is a question to be answered before anything is changed.
//! Every divergence is reported with both sides' answer for exactly that reason.

use crate::cheader::{HeaderDecl, HeaderModel};
use std::collections::{BTreeMap, BTreeSet};
use sv_codegen::plan::{Plan, Scalar, Shape, StructPlan};
use vk_registry::{CType, CTypeKind, Model, Profile, TypeDef};

/// Which of the four priorities a disagreement belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// An API constant: `VK_MAX_MEMORY_TYPES`, `VK_UUID_SIZE`.
    Constant,
    /// An enum or flag-bit value.
    EnumValue,
    /// A struct's `sType`.
    StructType,
    /// Which commands a profile resolves to.
    CommandSet,
    /// A struct member: presence, order, type, array extent.
    Member,
    /// A member or parameter whose declared width differs.
    Width,
    /// A command's return type or parameter list.
    Signature,
    /// A handle's dispatchability.
    Handle,
    /// An alias that does not resolve to the same thing on both sides.
    Alias,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Divergence {
    pub category: Category,
    /// What disagreed: a constant name, `VkBufferCreateInfo.pQueueFamilyIndices`,
    /// a command name.
    pub subject: String,
    /// Both sides' answer, always.
    pub detail: String,
}

/// How much was actually compared.
///
/// A cross-check that silently compared nothing is worse than none — it is a
/// green light with no information behind it — so every count here is asserted
/// against a lower bound by the tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counts {
    pub constants: usize,
    pub enum_values: usize,
    pub s_types: usize,
    pub commands_in_set: usize,
    pub structs: usize,
    pub members: usize,
    pub widths: usize,
    /// C bitfield structs compared against their header declaration, and the
    /// individual bitfields inside them. See [`check_flag_words`].
    pub flag_words: usize,
    pub flag_bits: usize,
    pub signatures: usize,
    pub params: usize,
    pub handles: usize,
    pub aliases: usize,
    /// Things deliberately not compared, with the reason, so that "compared" is
    /// never quietly padded with "looked at and skipped".
    pub skipped: BTreeMap<String, usize>,
}

impl Counts {
    fn skip(&mut self, reason: &str) {
        *self.skipped.entry(reason.to_string()).or_default() += 1;
    }
}

#[derive(Debug, Default)]
pub struct XReport {
    pub divergences: Vec<Divergence>,
    pub counts: Counts,
}

impl XReport {
    fn note(&mut self, category: Category, subject: impl Into<String>, detail: impl Into<String>) {
        self.divergences.push(Divergence {
            category,
            subject: subject.into(),
            detail: detail.into(),
        });
    }

    pub fn in_category(&self, c: Category) -> Vec<&Divergence> {
        self.divergences.iter().filter(|d| d.category == c).collect()
    }

    /// Sorted, so a failure message is the same text on every run.
    pub fn sorted(&self) -> Vec<&Divergence> {
        let mut v: Vec<&Divergence> = self.divergences.iter().collect();
        v.sort();
        v
    }
}

// ---------------------------------------------------------------------------
// 1. constants and enum values — the blind spot
// ---------------------------------------------------------------------------

/// Every API constant, against the header's `#define`.
///
/// Both directions matter. A constant whose number differs is a wrong array
/// extent; a constant the header publishes as a number and we could not resolve
/// is a `VK_MAX_…` that would silently become a zero-length array.
pub fn check_constants(model: &Model, header: &HeaderModel, report: &mut XReport) {
    for (name, c) in &model.constants {
        let Some(body) = header.defines.get(name) else {
            report.note(
                Category::Constant,
                name,
                format!("we define it as {:?}; vulkan_core.h does not define it at all", c.text),
            );
            continue;
        };
        // `#define VK_LUID_SIZE_KHR VK_LUID_SIZE` — an alias, resolved through
        // the header's own defines rather than through ours.
        let theirs = resolve_define(header, body);
        report.counts.constants += 1;
        match (c.value, theirs) {
            (Some(ours), Some(theirs)) if ours == theirs => {}
            (Some(ours), Some(theirs)) => report.note(
                Category::Constant,
                name,
                format!("ours {ours}, vulkan_core.h {theirs} (from {body:?})"),
            ),
            (Some(ours), None) => report.note(
                Category::Constant,
                name,
                format!("ours {ours}, vulkan_core.h {body:?} which is not a number"),
            ),
            (None, Some(theirs)) => report.note(
                Category::Constant,
                name,
                format!(
                    "we resolved no number from {:?}, vulkan_core.h publishes {theirs}. An \
                     unresolved constant becomes a missing array extent.",
                    c.text
                ),
            ),
            // Both agree it is a sentinel like `(~0ULL)`, which design.md
            // deliberately does not turn into a number.
            (None, None) => report.counts.skip("constant is a non-numeric sentinel on both sides"),
        }
    }
}

fn resolve_define(header: &HeaderModel, body: &str) -> Option<u64> {
    let mut cur = body.trim().to_string();
    for _ in 0..8 {
        if let Some(n) = crate::cheader::parse_c_number(&cur) {
            return Some(n);
        }
        match header.defines.get(&cur) {
            Some(next) if next != &cur => cur = next.clone(),
            _ => return None,
        }
    }
    None
}

/// Every enum and flag-bit value we generate, against the header's.
///
/// This is the check that would have caught R8's first instance on the first
/// run, over every one of the values it inverted rather than the three that
/// happened to be written down.
pub fn check_enum_values(model: &Model, plan: &Plan, header: &HeaderModel, report: &mut XReport) {
    // Our enums and bitmasks both carry `(name, value)` lists; the header keeps
    // them in one namespace, keyed by the `*FlagBits` type for a bitmask.
    //
    // `(the name we emit it under, the name the header declares it under, the
    // values)`.
    type Group<'a> = (&'a str, &'a str, &'a [(String, i64)]);
    let mut ours: Vec<Group<'_>> = Vec::new();
    for e in &plan.enums {
        ours.push((e.name.as_str(), e.name.as_str(), &e.values));
    }
    for b in &plan.bitmasks {
        // The values of a `*Flags` come from its `*FlagBits`, which is the name
        // the header declares them under.
        let group = match model.type_def(&b.name) {
            Some(TypeDef::Bitmask(bm)) => bm.bit_enum.as_deref().unwrap_or(&b.name),
            _ => b.name.as_str(),
        };
        ours.push((b.name.as_str(), group, &b.values));
    }

    // A `*Flags` bitmask and its `*FlagBits` enum carry the same value list —
    // the header declares them once, under the `*FlagBits` name — so each group
    // is compared once. Which is not the same as trusting that the two lists
    // agree: that they do is checked here, because two spellings of one flags
    // type disagreeing about a bit would be exactly R8's shape.
    /// The first spelling of a group we met, and the values it carried.
    type FirstSeen<'a> = (&'a str, &'a [(String, i64)]);
    let mut seen: BTreeMap<&str, FirstSeen<'_>> = BTreeMap::new();
    let mut work: Vec<Group<'_>> = Vec::new();
    for (our_name, group, values) in ours {
        match seen.get(group) {
            Some((first, first_values)) => {
                if *first_values != values {
                    report.note(
                        Category::EnumValue,
                        group,
                        format!(
                            "we generate two spellings of this group, `{first}` and \
                             `{our_name}`, and their value lists differ ({} vs {} entries)",
                            first_values.len(),
                            values.len()
                        ),
                    );
                    work.push((our_name, group, values));
                } else {
                    report.counts.skip("second spelling of a flags group, identical to the first");
                }
            }
            None => {
                seen.insert(group, (our_name, values));
                work.push((our_name, group, values));
            }
        }
    }

    for (our_name, group, values) in work {
        let Some(he) = header.enums.get(group).or_else(|| header.enums.get(our_name)) else {
            if !values.is_empty() {
                report.note(
                    Category::EnumValue,
                    our_name,
                    format!(
                        "we generate {} value(s) for it; vulkan_core.h declares no enum {group}",
                        values.len()
                    ),
                );
            } else {
                report.counts.skip("bitmask with no bit-values on either side");
            }
            continue;
        };

        for (name, value) in values {
            match he.value(name) {
                Some(theirs) if theirs == *value => report.counts.enum_values += 1,
                Some(theirs) => {
                    report.counts.enum_values += 1;
                    report.note(
                        Category::EnumValue,
                        format!("{group}::{name}"),
                        format!("ours {value}, vulkan_core.h {theirs}"),
                    );
                }
                None => report.note(
                    Category::EnumValue,
                    format!("{group}::{name}"),
                    format!("ours {value}; vulkan_core.h does not declare this name"),
                ),
            }
        }

        // The other direction: a value the header publishes and we do not
        // generate. That is the "quietly emitted less" shape of R8, in the part
        // of the protocol where a missing `sType` means a rejected `pNext` link.
        let mine: BTreeSet<&str> = values.iter().map(|(n, _)| n.as_str()).collect();
        for (name, _) in &he.values {
            if mine.contains(name.as_str()) {
                continue;
            }
            // `VK_…_MAX_ENUM = 0x7FFFFFFF` — and its vendor-suffixed spellings,
            // `VK_COLOR_SPACE_MAX_ENUM_KHR` — is a C-header device to pin the
            // enum's width. It is not in the registry and is not a value.
            if name.contains("_MAX_ENUM") {
                report.counts.skip("header-only *_MAX_ENUM width sentinel");
                continue;
            }
            report.note(
                Category::EnumValue,
                format!("{group}::{name}"),
                "vulkan_core.h declares it; we do not generate it".to_string(),
            );
        }
    }
}

/// Every struct's `sType`, against the header.
///
/// Separate from [`check_enum_values`] because it checks a different link: not
/// "is `VK_STRUCTURE_TYPE_FOO` 1000072000" but "is `VkFoo`'s `sType`
/// `VK_STRUCTURE_TYPE_FOO`". Getting *that* wrong is a `pNext` link the peer
/// rejects, which is where the sign inversion would have surfaced.
pub fn check_struct_types(plan: &Plan, header: &HeaderModel, report: &mut XReport) {
    let Some(st) = header.enums.get("VkStructureType") else {
        report.note(
            Category::StructType,
            "VkStructureType",
            "vulkan_core.h declares no VkStructureType — the header parse is broken".to_string(),
        );
        return;
    };
    for s in &plan.structs {
        let (Some(name), Some(value)) = (&s.s_type, s.s_type_value) else {
            continue;
        };
        report.counts.s_types += 1;
        match st.value(name) {
            Some(theirs) if theirs == value => {}
            Some(theirs) => report.note(
                Category::StructType,
                format!("{}.sType", s.name),
                format!("{name}: ours {value}, vulkan_core.h {theirs}"),
            ),
            None => report.note(
                Category::StructType,
                format!("{}.sType", s.name),
                format!("{name} is not a VkStructureType value in vulkan_core.h"),
            ),
        }
        // …and that the header's own struct agrees this is its first member.
        if let Some(hs) = header.structs.get(&s.name)
            && hs.members.first().map(|m| m.name.as_str()) != Some("sType")
        {
            report.note(
                Category::StructType,
                format!("{}.sType", s.name),
                "we write an sType for it, but vulkan_core.h's struct does not begin with one"
                    .to_string(),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 2. the resolved command set — the 55-vs-239 failure
// ---------------------------------------------------------------------------

/// What a profile resolves to, against what the header's feature and extension
/// blocks declare.
///
/// The header is the right reference for this one specifically because it does
/// **not** reproduce the registry's `apitype="internal"` sub-feature chain: it
/// writes the resolved result, so `#define VK_VERSION_1_0 1` … is followed by
/// every 1.0 command whether the registry reaches it directly or through
/// `depends=`. Agreeing with it means our `depends=` evaluation agrees with
/// Khronos's.
pub fn check_command_set(
    profile: &Profile,
    plan: &Plan,
    header: &HeaderModel,
    report: &mut XReport,
) {
    let mut expected: BTreeSet<String> = BTreeSet::new();
    for name in profile.features.iter().chain(profile.extensions.iter()) {
        match header.sections.get(name) {
            Some(cmds) => expected.extend(cmds.iter().cloned()),
            None => report.note(
                Category::CommandSet,
                name,
                "the profile lists it, but vulkan_core.h has no block for it".to_string(),
            ),
        }
    }

    // The profile's declared deviations, which are reviewable data in
    // `vk_registry::profiles` rather than behaviour hidden in the generator.
    for c in &profile.excluded_commands {
        expected.remove(c);
    }
    if let Some(only) = &profile.only_commands {
        let keep: BTreeSet<&str> = only.iter().map(String::as_str).collect();
        expected.retain(|c| keep.contains(c.as_str()));
    }

    let mut ours: BTreeSet<String> = BTreeSet::new();
    // A command that is only reachable as the *definition* an in-scope alias
    // points at. `VK_KHR_maintenance5` requires `vkCmdBindIndexBuffer2KHR`,
    // which `vk.xml` declares as an alias of the Vulkan 1.4 entry point
    // `vkCmdBindIndexBuffer2`; the wire carries one opcode under the definition's
    // name and the ICD exports both. The header, having no notion of an alias,
    // simply puts the two names in two blocks — so a definition whose alias is in
    // scope is agreement, not a divergence. That the two really are the same
    // function is not taken on trust: `check_aliases` requires the header to
    // declare both with identical signatures.
    let mut alias_targets: BTreeMap<&str, &str> = BTreeMap::new();
    for c in &plan.commands {
        ours.insert(c.name.clone());
        for a in &c.aliases {
            ours.insert(a.clone());
            alias_targets.insert(c.name.as_str(), a.as_str());
        }
    }

    report.counts.commands_in_set = expected.len();

    for name in expected.difference(&ours) {
        report.note(
            Category::CommandSet,
            name,
            format!(
                "vulkan_core.h declares it in block {:?}, and the profile enables that block, \
                 but our resolver did not include it",
                header.commands.get(name).map(|c| c.section.as_str()).unwrap_or("?")
            ),
        );
    }
    for name in ours.difference(&expected) {
        if let Some(alias) = alias_targets.get(name.as_str())
            && expected.contains(*alias)
        {
            report.counts.skip("command reached as the definition an in-scope alias names");
            continue;
        }
        report.note(
            Category::CommandSet,
            name,
            format!(
                "we generate it; vulkan_core.h puts it in block {:?}, which this profile does \
                 not enable",
                header.commands.get(name).map(|c| c.section.as_str()).unwrap_or("nowhere")
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// 3. struct members, and command signatures
// ---------------------------------------------------------------------------

/// What the header says a declaration is, reduced to the facts both sides state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Value,
    Pointer,
    PointerToPointer,
    FixedArray,
}

fn header_kind(d: &HeaderDecl) -> Kind {
    if !d.arrays.is_empty() {
        Kind::FixedArray
    } else {
        match d.pointer_depth {
            0 => Kind::Value,
            1 => Kind::Pointer,
            _ => Kind::PointerToPointer,
        }
    }
}

/// Our shape, reduced to the same facts.
///
/// The header carries no `len=`, so it cannot tell a pointer to one value from a
/// pointer to an array — that distinction is what the Venus differential exists
/// for, and the two checks are complements. What this *can* see is a pointer
/// read as a value, an array extent that resolved to the wrong number, a member
/// in the wrong place, and a member that is not there at all.
fn our_kind(shape: &Shape) -> Kind {
    match shape {
        Shape::Scalar(Scalar::OpaqueU64(_)) => Kind::Pointer,
        Shape::Scalar(_) => Kind::Value,
        Shape::FixedArray { .. } => Kind::FixedArray,
        Shape::Optional(_) | Shape::Array { .. } | Shape::Blob { .. } | Shape::CString => {
            Kind::Pointer
        }
        Shape::CStringArray { .. } => Kind::PointerToPointer,
        Shape::PNext => Kind::Pointer,
    }
}

fn leaf_scalar(shape: &Shape) -> Option<&Scalar> {
    match shape {
        Shape::Scalar(s) => Some(s),
        Shape::FixedArray { elem, .. } | Shape::Array { elem, .. } | Shape::Optional(elem) => {
            leaf_scalar(elem)
        }
        _ => None,
    }
}

fn our_type_name(shape: &Shape) -> Option<String> {
    Some(match shape {
        Shape::CString | Shape::CStringArray { .. } => "char".into(),
        Shape::Blob { .. } => "void".into(),
        Shape::PNext => "void".into(),
        _ => match leaf_scalar(shape)? {
            Scalar::U8 => "uint8_t".into(),
            Scalar::I8 => "int8_t".into(),
            Scalar::U16 => "uint16_t".into(),
            Scalar::I16 => "int16_t".into(),
            Scalar::U32 => "uint32_t".into(),
            Scalar::I32 => "int32_t".into(),
            Scalar::U64 => "uint64_t".into(),
            Scalar::I64 => "int64_t".into(),
            Scalar::F32 => "float".into(),
            Scalar::F64 => "double".into(),
            Scalar::Size => "size_t".into(),
            Scalar::Bool32 => "VkBool32".into(),
            Scalar::OpaqueU64(_) => "void".into(),
            Scalar::Enum(n)
            | Scalar::Bitmask(n)
            | Scalar::Bitmask64(n)
            | Scalar::Handle(n)
            | Scalar::Struct(n)
            | Scalar::Union(n) => n.clone(),
        },
    })
}

/// Whether the two spellings denote the same C type.
///
/// Our plan resolves the registry's `basetype` typedefs down to the primitive
/// underneath — `VkDeviceSize` is a `uint64_t` and that is what goes on the wire
/// — so the header's spelling is resolved through *the header's own* typedef
/// chain before comparing. That keeps the resolution on the reference's side of
/// the comparison rather than ours: if `vulkan_core.h` were to typedef
/// `VkDeviceSize` to something other than `uint64_t`, this would fire.
fn same_type(header: &HeaderModel, ours: &str, theirs: &str) -> bool {
    if ours == theirs {
        return true;
    }
    let resolved = header.resolve(theirs);
    if ours == resolved {
        return true;
    }
    matches!(
        (ours, resolved.as_str()),
        // A Vulkan `char` is a byte and a Rust `char` is a Unicode scalar
        // value, so a `char[N]` member is `[u8; N]` for us. Same N bytes.
        ("uint8_t", "char")
    )
}

/// The width in bytes our generator believes a scalar is declared at.
///
/// *Declared*, not encoded: `sv-wire` widens `u8` and `u16` to four bytes on the
/// wire (Venus's rule), which is a property of the encoding rather than a claim
/// about the C type. What must agree with the header is the claim about C.
fn our_declared_width(s: &Scalar) -> Option<usize> {
    Some(match s {
        Scalar::U8 | Scalar::I8 => 1,
        Scalar::U16 | Scalar::I16 => 2,
        Scalar::U32 | Scalar::I32 | Scalar::F32 | Scalar::Bool32 => 4,
        Scalar::Enum(_) | Scalar::Bitmask(_) => 4,
        Scalar::U64 | Scalar::I64 | Scalar::F64 | Scalar::Size => 8,
        Scalar::Bitmask64(_) | Scalar::Handle(_) | Scalar::OpaqueU64(_) => 8,
        Scalar::Struct(_) | Scalar::Union(_) => return None,
    })
}

/// Every C bitfield struct we carry as one word, against the header's own
/// declaration of it.
///
/// This is the check that makes `plan::FlagWordPlan` something other than an
/// assertion. The planner reads `video.xml` and lays the fields out in
/// declaration order from bit 0; the header states the same struct in C, with
/// its own `: N` widths, read by a parser that has never seen `video.xml`. What
/// is compared is the member sequence, each member's width in bits, and the
/// offset that follows from it — so a member dropped, reordered, or given the
/// wrong number of bits is a failure here.
///
/// What it cannot see is the *direction* a C compiler allocates in, because the
/// header does not say and no reading of it can. That is stated as an assumption
/// in `plan::FlagWordPlan`, with the evidence for it: the same layout, via
/// bindgen, decoded byte-identically on three drivers across both ABIs
/// (`docs/video-decode.md`). It is also the one thing the wire does not depend
/// on — the word is copied, never taken apart.
pub fn check_flag_words(plan: &Plan, header: &HeaderModel, report: &mut XReport) {
    for f in &plan.flag_words {
        let Some(hs) = header.structs.get(&f.name) else {
            report.note(
                Category::Member,
                &f.name,
                "we carry it as a 32-bit flag word; no published header declares it".to_string(),
            );
            continue;
        };
        report.counts.flag_words += 1;

        if hs.members.len() != f.fields.len() {
            report.note(
                Category::Member,
                &f.name,
                format!(
                    "we lay out {} bitfield(s); the header declares {}",
                    f.fields.len(),
                    hs.members.len()
                ),
            );
            continue;
        }
        let mut offset = 0u32;
        for (ours, theirs) in f.fields.iter().zip(hs.members.iter()) {
            report.counts.flag_bits += 1;
            if ours.name != theirs.name {
                report.note(
                    Category::Member,
                    format!("{}.{}", f.name, ours.name),
                    format!(
                        "we have {:?} at this position; the header has {:?}",
                        ours.name, theirs.name
                    ),
                );
                break;
            }
            let Some(bits) = theirs.bitfield else {
                report.note(
                    Category::Member,
                    format!("{}.{}", f.name, ours.name),
                    "we read it as a bitfield; the header declares it as an ordinary member"
                        .to_string(),
                );
                break;
            };
            if !same_type(header, "uint32_t", &theirs.base_type) {
                report.note(
                    Category::Width,
                    format!("{}.{}", f.name, ours.name),
                    format!(
                        "we assume a `uint32_t` storage unit; the header declares {}",
                        theirs.base_type
                    ),
                );
            }
            if ours.bits != bits || ours.offset != offset {
                report.note(
                    Category::Width,
                    format!("{}.{}", f.name, ours.name),
                    format!(
                        "we place {} bit(s) at offset {}; the header declares {bits} bit(s), \
                         which in declaration order falls at offset {offset}",
                        ours.bits, ours.offset
                    ),
                );
            }
            offset += bits;
        }
    }
}

/// Every struct we generate, against the header's declaration of it.
pub fn check_structs(plan: &Plan, header: &HeaderModel, report: &mut XReport) {
    for s in &plan.structs {
        let Some(hs) = header.structs.get(&s.name) else {
            report.note(
                Category::Member,
                &s.name,
                "we generate it; vulkan_core.h does not declare it".to_string(),
            );
            continue;
        };
        report.counts.structs += 1;
        compare_struct(s, hs.members.as_slice(), header, report);
    }
}

fn compare_struct(
    s: &StructPlan,
    members: &[HeaderDecl],
    header: &HeaderModel,
    report: &mut XReport,
) {
    // `sType` is not a field of ours by construction — a value cannot carry the
    // wrong `sType` if it does not have one (plan.rs) — and its value is checked
    // separately by `check_struct_types`.
    let theirs: Vec<&HeaderDecl> = members.iter().filter(|m| m.name != "sType").collect();

    if s.fields.len() != theirs.len() {
        report.note(
            Category::Member,
            &s.name,
            format!(
                "member count differs: ours {} [{}], vulkan_core.h {} [{}]",
                s.fields.len(),
                s.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", "),
                theirs.len(),
                theirs.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "),
            ),
        );
        return;
    }

    for (f, h) in s.fields.iter().zip(theirs.iter()) {
        let subject = format!("{}.{}", s.name, f.name);
        if f.name != h.name {
            report.note(
                Category::Member,
                &subject,
                format!("member order differs: ours `{}`, vulkan_core.h `{}`", f.name, h.name),
            );
            continue;
        }
        report.counts.members += 1;

        if let Some(bits) = h.bitfield {
            // docs/design.md §7.4: bitfields the registry cannot describe are out of
            // scope, and `plan::build` refuses them outright, so reaching one
            // here would mean the profile let one through.
            report.note(
                Category::Member,
                &subject,
                format!(
                    "vulkan_core.h declares it as a :{bits} bitfield, which docs/design.md §7.4 \
                     puts out of scope — it should not be in the plan at all"
                ),
            );
            continue;
        }

        let (k, t) = (our_kind(&f.shape), header_kind(h));
        if k != t {
            report.note(
                Category::Member,
                &subject,
                format!("ours is {k:?}, vulkan_core.h declares {t:?} (`{}`)", spell(h)),
            );
            continue;
        }

        // Array extents: this is where an API constant we resolved wrongly
        // shows up as a buffer of the wrong length.
        //
        // Compared level by level rather than as a product, because a C array
        // can have more than one dimension and a product would call
        // `uint8_t[6][16]` and `uint8_t[96]` the same thing. They are not: the
        // first is a `[[u8; 16]; 6]` and the second a `[u8; 96]`, and the wire
        // writes a length prefix per level. `video.xml` has two of these —
        // `StdVideoH264ScalingLists::ScalingList4x4` and `…8x8` — and they are
        // the first multi-dimensional arrays any profile has reached.
        if matches!(f.shape, Shape::FixedArray { .. }) && !h.arrays.is_empty() {
            let ours = our_extents(&f.shape);
            let theirs: Option<Vec<u64>> = h
                .arrays
                .iter()
                .map(|a| crate::cheader::parse_c_number(a).or_else(|| header.define_value(a)))
                .collect();
            match theirs {
                Some(t) if t == ours => {}
                Some(t) => report.note(
                    Category::Member,
                    &subject,
                    format!(
                        "array extent differs: ours {ours:?}, vulkan_core.h {t:?} (from [{}])",
                        h.arrays.join("][")
                    ),
                ),
                None => report.counts.skip("array extent vulkan_core.h does not state as a number"),
            }
        }

        // `pNext` is a chain for us and a `struct VkBaseInStructure*` or a
        // `const void*` for the header, depending on the struct. Both are the
        // link, and the chain's own encoding is checked by the round-trips and
        // by the Venus differential; what matters here is that it is a pointer,
        // which the kind comparison above has already established.
        if matches!(f.shape, Shape::PNext) {
            continue;
        }

        match (our_type_name(&f.shape), h.base_type.as_str()) {
            (Some(a), b) if same_type(header, &a, b) => {
                // The width, resolved independently on each side.
                if let Some(scalar) = leaf_scalar(&f.shape)
                    && let (Some(ours), Some(theirs)) =
                        (our_declared_width(scalar), header.width_of(b))
                {
                    report.counts.widths += 1;
                    if ours != theirs {
                        report.note(
                            Category::Width,
                            &subject,
                            format!("`{b}`: we encode it as {ours} byte(s), vulkan_core.h resolves it to {theirs}"),
                        );
                    }
                }
            }
            (Some(a), b) => report.note(
                Category::Member,
                &subject,
                format!("type differs: ours `{a}`, vulkan_core.h `{b}`"),
            ),
            (None, _) => report.counts.skip("member shape names no single type"),
        }
    }
}

fn spell(d: &HeaderDecl) -> String {
    let mut s = d.base_type.clone();
    for _ in 0..d.pointer_depth {
        s.push('*');
    }
    for a in &d.arrays {
        s.push_str(&format!("[{a}]"));
    }
    s
}

/// Our array extents, outermost first, to compare against the header's list.
fn our_extents(shape: &Shape) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur = shape;
    while let Shape::FixedArray { elem, count, .. } = cur {
        out.push(*count);
        cur = elem;
    }
    out
}

/// Every command's signature, against the header's `PFN_vk…` typedef.
///
/// Compared against the *model* rather than the plan, because the model is where
/// the C declarator is read and the plan is derived from it: a parameter misread
/// by one star is a model fact, and it is the fact this catches. The plan is
/// then checked separately for having dropped nothing (see below).
pub fn check_signatures(model: &Model, plan: &Plan, header: &HeaderModel, report: &mut XReport) {
    for c in &plan.commands {
        let Some(cmd) = model.commands.get(&c.name) else { continue };
        let Some(hc) = header.commands.get(&c.name) else {
            report.note(
                Category::Signature,
                &c.name,
                "we generate it; vulkan_core.h declares no PFN for it".to_string(),
            );
            continue;
        };
        report.counts.signatures += 1;

        let ours_ret = ctype_decl(&cmd.return_type);
        if ours_ret.0 != hc.return_type || ours_ret.1 != hc.return_pointer_depth {
            report.note(
                Category::Signature,
                &c.name,
                format!(
                    "return type differs: ours `{}`{}, vulkan_core.h `{}`{}",
                    ours_ret.0,
                    "*".repeat(ours_ret.1),
                    hc.return_type,
                    "*".repeat(hc.return_pointer_depth),
                ),
            );
        }

        if cmd.params.len() != hc.params.len() {
            report.note(
                Category::Signature,
                &c.name,
                format!(
                    "parameter count differs: ours {} [{}], vulkan_core.h {} [{}]",
                    cmd.params.len(),
                    cmd.params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "),
                    hc.params.len(),
                    hc.params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "),
                ),
            );
            continue;
        }

        for (p, hp) in cmd.params.iter().zip(hc.params.iter()) {
            report.counts.params += 1;
            let (base, depth) = ctype_decl(&p.ty);
            if p.name != hp.name {
                report.note(
                    Category::Signature,
                    format!("{}({})", c.name, p.name),
                    format!("parameter order differs: ours `{}`, vulkan_core.h `{}`", p.name, hp.name),
                );
                continue;
            }
            if base != hp.base_type || depth != hp.pointer_depth {
                report.note(
                    Category::Signature,
                    format!("{}({})", c.name, p.name),
                    format!(
                        "ours `{base}`{}, vulkan_core.h `{}`{}",
                        "*".repeat(depth),
                        hp.base_type,
                        "*".repeat(hp.pointer_depth),
                    ),
                );
            }
        }

        // Nothing may be dropped silently. The two strips that *are* deliberate
        // are named in docs/design.md §7.4 and in the exception table, and both are
        // listed here so that a third one cannot appear without this failing.
        let plan_params: BTreeSet<&str> = c
            .inputs
            .iter()
            .chain(c.outputs.iter())
            .map(|f| f.name.as_str())
            .collect();
        for p in &cmd.params {
            if plan_params.contains(p.name.as_str()) {
                continue;
            }
            if p.ty.base_name() == Some("VkAllocationCallbacks") {
                report.counts.skip("pAllocator, stripped above the protocol (docs/design.md §7.4)");
                continue;
            }
            if sv_codegen::exceptions::void_policy(&c.name, &p.name).is_some() {
                report.counts.skip("void* parameter with a declared policy entry");
                continue;
            }
            report.note(
                Category::Signature,
                format!("{}({})", c.name, p.name),
                "vulkan_core.h declares this parameter and our plan carries neither an input \
                 nor an output for it, and it is not one of the declared strips"
                    .to_string(),
            );
        }
    }
}

/// A `CType` reduced to the two facts the header states: the base name and how
/// many stars.
fn ctype_decl(ty: &CType) -> (String, usize) {
    let mut depth = 0;
    let mut cur = ty;
    loop {
        match &cur.kind {
            CTypeKind::Pointer(inner) => {
                depth += 1;
                cur = inner;
            }
            CTypeKind::Array(inner, _) => cur = inner,
            CTypeKind::Void => return ("void".to_string(), depth),
            CTypeKind::Named(n) => return (n.clone(), depth),
        }
    }
}

// ---------------------------------------------------------------------------
// 4. handles and aliases
// ---------------------------------------------------------------------------

/// Dispatchable versus non-dispatchable, against the header's two macros.
///
/// It matters beyond bookkeeping: docs/design.md §6.4 makes a dispatchable handle a
/// pointer to a `#[repr(C)]` struct whose first word the loader overwrites, and
/// a non-dispatchable one a plain id. Getting the flag wrong is a loader crash
/// at the ICD boundary, not a wire-format bug — but it is derived from the same
/// registry reading, so it belongs to the same risk.
pub fn check_handles(plan: &Plan, header: &HeaderModel, report: &mut XReport) {
    for h in &plan.handles {
        let Some(&dispatchable) = header.handles.get(&h.name) else {
            report.note(
                Category::Handle,
                &h.name,
                "we generate it; vulkan_core.h declares no handle of that name".to_string(),
            );
            continue;
        };
        report.counts.handles += 1;
        if dispatchable != h.dispatchable {
            report.note(
                Category::Handle,
                &h.name,
                format!(
                    "ours says dispatchable = {}, vulkan_core.h uses {}",
                    h.dispatchable,
                    if dispatchable {
                        "VK_DEFINE_HANDLE"
                    } else {
                        "VK_DEFINE_NON_DISPATCHABLE_HANDLE"
                    }
                ),
            );
        }
    }
}

/// Type and command aliases, against the header.
///
/// A type alias is a `typedef` the header states outright. A *command* alias is
/// not written as an alias in C at all — the header simply declares two
/// prototypes — so what is checked is the consequence: the two signatures must
/// be identical. If they are not, the alias resolution put two different
/// commands on one opcode.
pub fn check_aliases(plan: &Plan, header: &HeaderModel, report: &mut XReport) {
    for (alias, target) in &plan.type_aliases {
        // Only the ones the header knows: a `*FlagBits`/`*Flags` pair, an enum
        // group, and the `category="define"` entries are aliases in the
        // registry's sense without being typedefs in C.
        let Some(theirs) = header.typedefs.get(alias) else {
            report.counts.skip("type alias vulkan_core.h does not spell as a typedef");
            continue;
        };
        report.counts.aliases += 1;
        // The header may typedef through an intermediate name; resolve both
        // sides to the end of their own chains before comparing.
        let theirs_end = header.resolve(theirs);
        let ours_end = header.resolve(target);
        if theirs_end != ours_end {
            report.note(
                Category::Alias,
                alias,
                format!(
                    "ours resolves to `{target}` (`{ours_end}`), vulkan_core.h typedefs it to \
                     `{theirs}` (`{theirs_end}`)"
                ),
            );
        }
    }

    for c in &plan.commands {
        let Some(target) = header.commands.get(&c.name) else { continue };
        for alias in &c.aliases {
            let Some(a) = header.commands.get(alias) else {
                report.note(
                    Category::Alias,
                    alias,
                    format!("we give it {}'s opcode; vulkan_core.h declares no such command", c.name),
                );
                continue;
            };
            report.counts.aliases += 1;
            let sig = |cmd: &crate::cheader::HeaderCommand| {
                (
                    cmd.return_type.clone(),
                    cmd.return_pointer_depth,
                    cmd.params
                        .iter()
                        .map(|p| (p.base_type.clone(), p.pointer_depth))
                        .collect::<Vec<_>>(),
                )
            };
            if sig(a) != sig(target) {
                report.note(
                    Category::Alias,
                    alias,
                    format!(
                        "we give it {}'s opcode, but vulkan_core.h declares the two with \
                         different signatures — one wire message cannot carry both",
                        c.name
                    ),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the whole thing
// ---------------------------------------------------------------------------

/// Run every check.
pub fn compare(
    model: &Model,
    plan: &Plan,
    profile: &Profile,
    header: &HeaderModel,
    report: &mut XReport,
) {
    check_constants(model, header, report);
    check_enum_values(model, plan, header, report);
    check_struct_types(plan, header, report);
    check_command_set(profile, plan, header, report);
    check_structs(plan, header, report);
    check_flag_words(plan, header, report);
    check_signatures(model, plan, header, report);
    check_handles(plan, header, report);
    check_aliases(plan, header, report);
}

/// A one-line summary of a run, for the test's stderr.
pub fn summary(report: &XReport) -> String {
    let c = &report.counts;
    format!(
        "cross-check against vulkan_core.h: {} constants, {} enum values, {} sTypes, \
         {} commands in the profile's resolved set, {} structs / {} members / {} widths, \
         {} flag words / {} bitfields, \
         {} signatures / {} parameters, {} handles, {} aliases; {} divergence(s)",
        c.constants,
        c.enum_values,
        c.s_types,
        c.commands_in_set,
        c.structs,
        c.members,
        c.widths,
        c.flag_words,
        c.flag_bits,
        c.signatures,
        c.params,
        c.handles,
        c.aliases,
        report.divergences.len(),
    )
}
