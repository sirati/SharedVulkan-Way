//! The ICD's C ABI surface, as a plan.
//!
//! [`crate::plan`] describes what goes *on the wire*. This describes what the
//! *application* hands us, which is a different thing and must not be conflated
//! with it — the two differ in almost every respect that matters:
//!
//! | | wire ([`crate::plan::Plan`]) | C ABI (this) |
//! |---|---|---|
//! | `VkBool32` | Rust `bool` | `u32` |
//! | `VkInstance` | a `u64` object id | `*mut VkInstance_T`, a pointer |
//! | an array | `Vec<T>` with its own count | a pointer plus a `len=` sibling |
//! | `sType` | not a field at all | member zero, and load-bearing |
//! | `pNext` | `Vec<PNextLink>` | a raw pointer to a linked list |
//! | `VkAllocationCallbacks` | excluded, not serialisable | in every creation signature |
//!
//! The last row is the one that catches people out. design.md §7.4 strips
//! `pAllocator` *above the protocol*, which is a statement about the wire; the
//! parameter is still in the C signature and an ICD that omits it corrupts its
//! caller's stack. Likewise the profile's `excluded_commands` list
//! (`vkGetInstanceProcAddr`, the swapchain family, the surface queries) names
//! commands that are **not forwarded** — every one of them is still a function
//! the ICD must export and implement locally.
//!
//! So the ABI surface is a superset of the wire profile, and the difference is
//! recorded per command as [`AbiCommand::forwarded`].
//!
//! # `len=` is load-bearing here, unlike on the wire
//!
//! [`crate::plan`]'s header explains that our wire gives every variable-length
//! thing an explicit count, which demotes the registry's `len=` from an input to
//! a cross-check — an unplanned mitigation of R8. **That demotion does not
//! extend to this plan.** Lowering `pQueueCreateInfos` out of the application's
//! memory means reading exactly `queueCreateInfoCount` elements, and the only
//! source for that number is `len=`. A wrong `len=` here is an out-of-bounds
//! read of *the application's* memory, which is worse than anything the wire can
//! do. That is why every array whose length this plan cannot resolve to a
//! concrete sibling is refused rather than guessed at ([`ArrayLen::Unresolved`]),
//! and why the commands carrying one are listed rather than silently emitted.

use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use vk_registry::{
    ArrayExtent, CType, CTypeKind, LenTerm, Member, Model, Primitive, Profile, TypeDef,
};

/// Which of the loader's three lookup tables a command belongs in.
///
/// The loader's rules, which are not symmetrical and are a classic source of
/// "the function pointer is NULL and nobody says why":
///
/// - `vkGetInstanceProcAddr(NULL, name)` answers **only** for [`Level::Global`].
/// - `vkGetInstanceProcAddr(instance, name)` answers for everything else,
///   including device-level commands (they come back as trampolines).
/// - `vkGetDeviceProcAddr(device, name)` answers **only** for [`Level::Device`].
/// - `vk_icdGetPhysicalDeviceProcAddr` answers only for
///   [`Level::PhysicalDevice`] (design.md §8: required from interface 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// No dispatchable first parameter: `vkCreateInstance`,
    /// `vkEnumerateInstance*`.
    Global,
    /// Dispatches from a `VkInstance`.
    Instance,
    /// Dispatches from a `VkPhysicalDevice`. Instance-level for the loader's
    /// purposes, but it is the set `vk_icdGetPhysicalDeviceProcAddr` answers
    /// for, so it is tracked separately.
    PhysicalDevice,
    /// Dispatches from a `VkDevice`, `VkQueue` or `VkCommandBuffer`.
    Device,
}

impl Level {
    pub fn is_device(self) -> bool {
        self == Level::Device
    }
    pub fn is_global(self) -> bool {
        self == Level::Global
    }
}

/// Where the count of an array parameter or member comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArrayLen {
    /// A sibling in the same struct or parameter list.
    Sibling(String),
    /// `pAllocateInfo->commandBufferCount`: a member of another parameter.
    Path { root: String, field: String },
    /// `codeSize / 4`.
    SiblingDiv { sibling: String, divisor: u64 },
    /// `(rasterizationSamples + 31) / 32` — `VkPipelineMultisampleStateCreateInfo`
    /// `pSampleMask`, the only occurrence on this surface, and one the ICD does
    /// have to read correctly because the array is the application's.
    SiblingCeilDiv { sibling: String, add: u64, divisor: u64 },
    /// A `len=` this plan will not guess at. Any command or struct reaching one
    /// is refused rather than emitted with an invented bound.
    Unresolved(String),
}

/// One member of a `#[repr(C)]` type, or one parameter of a C function.
#[derive(Debug, Clone)]
pub struct AbiMember {
    pub name: String,
    pub rust_name: String,
    pub ty: CType,
    /// Length of the outermost pointer level, when it is an array.
    pub len: Option<ArrayLen>,
    /// Length of the *second* pointer level, for `const char* const*`.
    pub inner_len: Option<ArrayLen>,
    /// `optional=` at the outermost level.
    pub optional: bool,
    /// Non-const outermost pointer: the callee fills it in.
    pub is_output: bool,
    /// Some other member's `len=` names this one.
    pub is_a_length: bool,
    /// `null-terminated` at the outermost pointer level: a C string.
    pub is_c_string: bool,
}

/// A type as C sees it.
#[derive(Debug, Clone)]
pub enum AbiType {
    /// `typedef uint32_t VkBool32;`
    Alias { name: String, underlying: CType },
    Handle { name: String, dispatchable: bool },
    /// An `enum` or a `VkFlags`, as a `#[repr(transparent)]` newtype for the
    /// reason `crate::emit::types` gives: a value from a newer driver that is
    /// not one of the declared variants is undefined behaviour in a Rust `enum`
    /// and merely an integer in a newtype.
    ///
    /// A C bitfield struct is also one of these — see [`build`]'s handling of
    /// `plan::flag_word_of`. `struct { uint32_t a:1; uint32_t b:1; }` and
    /// `#[repr(transparent)] struct T(u32)` have the same size, alignment and
    /// argument-passing class, and the ICD never takes the word apart.
    Newtype { name: String, bit_width: u32, bitflags: bool, values: Vec<(String, i64)> },
    Struct { name: String, is_union: bool, members: Vec<AbiMember> },
    FuncPointer { name: String, return_type: CType, params: Vec<(String, CType)> },
    /// A type from a platform header, emitted as a zero-sized incomplete type.
    ///
    /// **Only legal behind a pointer.** `HWND` and `xcb_connection_t*` are the
    /// cases this exists for and both are pointers; a zero-sized stand-in used
    /// *by value* would silently give a struct member no storage and shift every
    /// member after it.
    ///
    /// That was not a hypothetical. `VkVideoDecodeH264ProfileInfoKHR::stdProfileIdc`
    /// is a `StdVideoH264ProfileIdc` by value, and `StdVideoH264ProfileIdc` was
    /// an external type until `video.xml` was merged
    /// (`vk_registry::video_registry_path`). Nothing reached it, so nothing
    /// broke — which is exactly the shape of bug this project keeps finding.
    /// [`build`] now refuses a by-value use rather than emitting one, so the
    /// next such type is a generator error and not a wrong offset.
    Opaque { name: String },
}

impl AbiType {
    pub fn name(&self) -> &str {
        match self {
            AbiType::Alias { name, .. }
            | AbiType::Handle { name, .. }
            | AbiType::Newtype { name, .. }
            | AbiType::Struct { name, .. }
            | AbiType::FuncPointer { name, .. }
            | AbiType::Opaque { name } => name,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AbiCommand {
    pub name: String,
    /// Names that are the same entry point: `vkTrimCommandPoolKHR` and
    /// `vkTrimCommandPool`. Every one of them must be answerable by
    /// `vkGetInstanceProcAddr`, because an application that enabled the
    /// extension will ask for the suffixed spelling.
    pub aliases: Vec<String>,
    pub params: Vec<AbiMember>,
    pub return_type: CType,
    pub level: Level,
    /// Whether this command has a wire opcode in `sv-proto`.
    ///
    /// `false` for the ICD's own: `vkGetInstanceProcAddr`, `vkGetDeviceProcAddr`,
    /// the swapchain family and the surface queries. Those are implemented on
    /// the app side and never cross (design.md §8, §10).
    pub forwarded: bool,
    pub returns_result: bool,
    pub returns_void: bool,
}

impl AbiCommand {
    /// Every spelling of this entry point, canonical first.
    pub fn all_names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.aliases.iter().map(String::as_str))
    }
}

#[derive(Debug, Clone, Default)]
pub struct AbiPlan {
    pub profile: String,
    pub commands: Vec<AbiCommand>,
    /// Types in dependency-free order (Rust does not care) but sorted by name so
    /// the output is a function of the registry alone.
    pub types: Vec<AbiType>,
    /// API constants with a numeric value, for the ICD's own use.
    pub constants: Vec<(String, u64)>,
    /// Structs that carry an `sType`, with its value: the `pNext` chain walker
    /// needs the mapping in the C direction as well as the wire one.
    pub s_types: BTreeMap<String, i64>,
    /// `parent → structs that may extend it`, restricted to the surface.
    pub extenders: BTreeMap<String, Vec<String>>,
}

impl AbiPlan {
    pub fn type_named(&self, name: &str) -> Option<&AbiType> {
        self.types.iter().find(|t| t.name() == name)
    }
    pub fn command(&self, name: &str) -> Option<&AbiCommand> {
        self.commands.iter().find(|c| c.name == name)
    }
}

/// The profile the ICD's C surface is generated from.
///
/// The wire profile with its command exclusions lifted: those commands are not
/// forwarded, but they are still exported. `VkAllocationCallbacks` comes back
/// too, for the reason in this module's header.
pub fn surface_profile(wire: &Profile) -> Profile {
    let mut p = wire.clone();
    p.name = format!("{}-icd", wire.name);
    p.excluded_commands = SURFACE_CREATORS.iter().map(|s| s.to_string()).collect();
    // The one type that stays excluded is the one the registry genuinely cannot
    // describe: `VkAccelerationStructureInstanceKHR` is bitfields (design.md
    // §7.4). It is unreachable from this surface; the exclusion is a tripwire
    // rather than a workaround, and reaching it is a generator error.
    //
    // The two X11 create-info structs are excluded for the opposite reason:
    // they are perfectly describable and they hold a pointer into a library
    // nobody here links. They are unreachable once [`SURFACE_CREATORS`] is out,
    // and reaching one would mean a create-surface command had come back.
    p.excluded_types = vec![
        "VkAccelerationStructureInstanceKHR".to_string(),
        "VkXcbSurfaceCreateInfoKHR".to_string(),
        "VkXlibSurfaceCreateInfoKHR".to_string(),
    ];
    p
}

/// The commands that are not on the C surface either, and the only ones.
///
/// design.md §8: "We opt *out* of interface-3 surface creation by not exposing
/// `vkCreateXXXSurfaceKHR`, so the loader hands us a `VkIcdSurfaceXXX` from
/// `vk_icd.h` which we cast." That decision is what makes advertising
/// `VK_KHR_xcb_surface` cost nothing in platform headers, and it is enforced
/// here rather than left to the emitter, so that a generated `extern "system"`
/// signature naming `xcb_connection_t` cannot come back by accident.
///
/// Note what is **not** in this list: the two
/// `vkGetPhysicalDevice…PresentationSupportKHR` queries take the same
/// `xcb_connection_t*`/`Display*` and are on the surface anyway, because the
/// loader's terminator calls `abort()` for an ICD that does not export them.
/// They ignore the argument, so a `*mut c_void` in the signature is enough.
/// `vkCreateHeadlessSurfaceEXT` is in the list for a second reason, and it is
/// the one that decided the shape of the ICD's WSI: **if any one surface
/// creator is exported, the ICD sees two kinds of `VkSurfaceKHR`.** The loader
/// keeps its own `VkIcdSurface` either way, and passes the ICD's own handle to
/// every terminator only for the surfaces the ICD created. Exporting exactly
/// none of them makes `VkSurfaceKHR` uniformly "a pointer to a
/// `VkIcdSurfaceBase`", which is a thing that can be read; exporting some makes
/// it "either that or one of my ids", which is a thing that can only be
/// guessed at.
pub const SURFACE_CREATORS: &[&str] =
    &["vkCreateXlibSurfaceKHR", "vkCreateXcbSurfaceKHR", "vkCreateHeadlessSurfaceEXT"];

/// Build the ABI plan.
///
/// `forwarded` is the set of command names that have a wire opcode — everything
/// else is the ICD's own.
pub fn build(model: &Model, profile: &Profile, forwarded: &BTreeSet<String>) -> Result<AbiPlan> {
    let resolved = profile.resolve(model)?;

    let mut aliases_by_target: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (alias, target) in &resolved.command_aliases {
        aliases_by_target.entry(target.as_str()).or_default().push(alias.clone());
    }

    let mut plan = AbiPlan { profile: profile.name.clone(), ..Default::default() };
    let mut wanted: BTreeSet<String> = BTreeSet::new();

    let mut names: Vec<&String> = resolved.commands.iter().collect();
    names.sort();
    for name in names {
        let cmd = model
            .commands
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("{name} is in the profile but not in the model"))?;

        let lengths = length_members(&cmd.params);
        let mut params = Vec::new();
        for p in &cmd.params {
            params.push(member(model, name, p, &lengths, &mut wanted)?);
        }
        note_type(model, &cmd.return_type, &mut wanted);

        let return_type = cmd.return_type.clone();
        let returns_result = return_type.base_name() == Some("VkResult");
        let returns_void = matches!(return_type.kind, CTypeKind::Void);

        plan.commands.push(AbiCommand {
            name: name.clone(),
            aliases: {
                let mut a = aliases_by_target.get(name.as_str()).cloned().unwrap_or_default();
                a.sort();
                a
            },
            level: level_of(model, cmd.params.first()),
            params,
            return_type,
            forwarded: forwarded.contains(name),
            returns_result,
            returns_void,
        });
    }

    // The two structs the `pNext` walkers cast to. They are core 1.1 and so are
    // in every profile this project will have, but naming them here means a
    // profile that somehow did not carry them fails at generation rather than in
    // a generated file that will not compile.
    for base in ["VkBaseInStructure", "VkBaseOutStructure"] {
        if !model.types.contains_key(base) {
            bail!("{base} is not in the registry, and the pNext walkers need it");
        }
        wanted.insert(base.to_string());
    }

    // Seed with the profile's own closed type set as well as with what the
    // command signatures reach. Those are not the same set and the difference is
    // the whole `pNext` universe: `VkPhysicalDeviceVulkan12Features` appears in
    // no command signature anywhere — it is only ever reached by walking a
    // chain — so a closure seeded from parameters alone silently omits every
    // extending struct, and an ICD that cannot name a chain link cannot decline
    // it either. Seeding from parameters *only* was the first attempt and it
    // produced 357 types where the profile has 700.
    for t in &resolved.types {
        wanted.insert(model.canonical_type(t));
    }

    // Transitive closure over struct members. A worklist rather than recursion
    // because the type graph is cyclic through `pNext` extenders.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    while let Some(name) = wanted.iter().next().cloned() {
        wanted.remove(&name);
        if !seen.insert(name.clone()) {
            continue;
        }
        if profile.excluded_types.contains(&name) {
            bail!(
                "the ICD's C surface reaches {name}, which is excluded because the \
                 registry cannot describe it (design.md §7.4). Either the profile \
                 gained an extension it should not have, or the exclusion is wrong."
            );
        }
        let Some(def) = model.types.get(&name) else { continue };
        if let Some(t) = abi_type(model, def, &mut wanted)? {
            plan.types.push(t);
        }
    }

    // `pNext` chains, in the C direction: which sType may appear under which
    // parent, and what each sType names.
    for t in &plan.types {
        if let AbiType::Struct { name, .. } = t
            && let Some(s) = model.struct_def(name)
            && let Some(c) = &s.s_type
            && let Some(v) = enum_value(model, "VkStructureType", c)
        {
            plan.s_types.insert(name.clone(), v);
        }
    }
    let present: BTreeSet<&str> = plan.types.iter().map(AbiType::name).collect();
    for (parent, exts) in &model.extended_by {
        if !present.contains(parent.as_str()) {
            continue;
        }
        let mut kept: Vec<String> = exts
            .iter()
            .filter(|e| present.contains(e.as_str()) && plan.s_types.contains_key(*e))
            .cloned()
            .collect();
        kept.sort();
        if !kept.is_empty() {
            plan.extenders.insert(parent.clone(), kept);
        }
    }

    let mut constants: Vec<(String, u64)> = model
        .constants
        .iter()
        .filter(|(_, c)| c.alias.is_none())
        .filter_map(|(n, c)| c.value.map(|v| (n.clone(), v)))
        .collect();
    constants.sort();
    plan.constants = constants;

    plan.types.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(plan)
}

fn level_of(model: &Model, first: Option<&Member>) -> Level {
    let Some(p) = first else { return Level::Global };
    // Only a *by-value* handle dispatches. `vkEnumerateInstanceExtension-
    // Properties(const char* pLayerName, …)` has a first parameter and is still
    // global.
    if p.ty.indirection() != 0 {
        return Level::Global;
    }
    let Some(base) = p.ty.base_name() else { return Level::Global };
    match model.canonical_type(base).as_str() {
        "VkDevice" | "VkQueue" | "VkCommandBuffer" => Level::Device,
        "VkPhysicalDevice" => Level::PhysicalDevice,
        "VkInstance" => Level::Instance,
        _ => Level::Global,
    }
}

/// Which members are named by some other member's `len=`.
fn length_members(members: &[Member]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for m in members {
        let Some(len) = &m.len else { continue };
        for term in &len.0 {
            match term {
                LenTerm::Sibling(s) => {
                    out.insert(s.clone());
                }
                LenTerm::Expr(e) => {
                    for tok in e.split(|c: char| !c.is_alphanumeric() && c != '_') {
                        if !tok.is_empty() && !tok.chars().all(|c| c.is_ascii_digit()) {
                            out.insert(tok.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn member(
    model: &Model,
    owner: &str,
    m: &Member,
    lengths: &BTreeSet<String>,
    wanted: &mut BTreeSet<String>,
) -> Result<AbiMember> {
    if let Some(bits) = m.bitfield_bits {
        bail!(
            "{owner}.{}: a {bits}-bit bitfield in a struct that is not a flag word. \
             Rust has no `#[repr(C)]` bitfield; a struct whose members are *all* \
             `uint32_t` bitfields is carried as the one word it occupies \
             (plan::FlagWordPlan), and a struct that mixes the two has no \
             C-compatible Rust definition at all (design.md §7.4).",
            m.name
        );
    }
    // A platform type with no definition has no size, so it cannot be a value.
    // See `AbiType::Opaque` and `exceptions::PLATFORM_SCALARS`.
    if m.ty.indirection() == 0
        && let Some(base) = m.ty.base_name()
        && matches!(model.type_def(base), Some(TypeDef::External { .. }))
        && crate::exceptions::platform_scalar(base).is_none()
    {
        bail!(
            "{owner}.{}: {base} is a platform type the registry only names, used *by \
             value*. It is emitted as a zero-sized incomplete type, which is right \
             behind a pointer and silently wrong here — every member after this one \
             would be at the wrong offset. Either the type belongs in a registry the \
             generator reads (as the StdVideo* types now do, via video.xml), or this \
             surface should not reach it.",
            m.name
        );
    }
    note_type(model, &m.ty, wanted);
    let term = |level: usize| m.len.as_ref().and_then(|l| l.term(level)).cloned();
    Ok(AbiMember {
        rust_name: crate::naming::snake_case(&m.name),
        is_c_string: matches!(term(0), Some(LenTerm::NullTerminated))
            || (m.ty.indirection() == 1 && m.ty.base_name() == Some("char") && m.len.is_none()),
        len: term(0).as_ref().and_then(array_len),
        inner_len: term(1).as_ref().and_then(array_len),
        optional: m.is_optional(0),
        is_output: m.is_output_pointer(),
        is_a_length: lengths.contains(&m.name),
        name: m.name.clone(),
        ty: m.ty.clone(),
    })
}

fn array_len(term: &LenTerm) -> Option<ArrayLen> {
    match term {
        LenTerm::Sibling(s) => Some(ArrayLen::Sibling(s.clone())),
        LenTerm::Path { root, field } => {
            Some(ArrayLen::Path { root: root.clone(), field: field.clone() })
        }
        LenTerm::Expr(e) => Some(parse_len_expr(e)),
        // A literal `1` is a pointer to one value, not an array; a
        // `null-terminated` is a string. Neither is a count.
        LenTerm::Literal(_) | LenTerm::NullTerminated => None,
    }
}

/// The four `altlen=` expressions in the registry are `codeSize / 4`,
/// `(samples + 31) / 32`, `(rasterizationSamples + 31) / 32` and
/// `2*VK_UUID_SIZE`. The wire plan records the last three unchecked, because
/// there the count comes from the wire and `len=` is only a cross-check. Here
/// the expression *is* the bound on a read of the application's memory, so the
/// two ceiling-division forms are parsed rather than shrugged at.
fn parse_len_expr(e: &str) -> ArrayLen {
    let compact: String = e.chars().filter(|c| !c.is_whitespace()).collect();
    if let Some((lhs, rhs)) = compact.split_once('/')
        && let Ok(divisor) = rhs.parse::<u64>()
        && divisor != 0
    {
        if lhs.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return ArrayLen::SiblingDiv { sibling: lhs.to_string(), divisor };
        }
        // `(name+31)`
        if let Some(inner) = lhs.strip_prefix('(').and_then(|s| s.strip_suffix(')'))
            && let Some((sibling, add)) = inner.split_once('+')
            && sibling.chars().all(|c| c.is_alphanumeric() || c == '_')
            && let Ok(add) = add.parse::<u64>()
        {
            return ArrayLen::SiblingCeilDiv {
                sibling: sibling.to_string(),
                add,
                divisor,
            };
        }
    }
    ArrayLen::Unresolved(e.to_string())
}

/// Record every named type a C type mentions, so the closure picks it up.
fn note_type(model: &Model, ty: &CType, wanted: &mut BTreeSet<String>) {
    match &ty.kind {
        CTypeKind::Void => {}
        CTypeKind::Named(n) => {
            let canonical = model.canonical_type(n);
            if !matches!(model.types.get(&canonical), Some(TypeDef::Primitive(_))) {
                wanted.insert(canonical);
            }
        }
        CTypeKind::Pointer(inner) => note_type(model, inner, wanted),
        CTypeKind::Array(inner, _) => note_type(model, inner, wanted),
    }
}

fn abi_type(
    model: &Model,
    def: &TypeDef,
    wanted: &mut BTreeSet<String>,
) -> Result<Option<AbiType>> {
    Ok(Some(match def {
        TypeDef::Primitive(_) => return Ok(None),
        TypeDef::BaseType { name, underlying } => match underlying {
            Some(u) => {
                note_type(model, u, wanted);
                AbiType::Alias { name: name.clone(), underlying: u.clone() }
            }
            // An opaque platform typedef with no underlying type. Only legal
            // behind a pointer, which is exactly what `Opaque` encodes.
            None => AbiType::Opaque { name: name.clone() },
        },
        TypeDef::Handle(h) => {
            AbiType::Handle { name: h.name.clone(), dispatchable: h.dispatchable }
        }
        TypeDef::Enum(e) => AbiType::Newtype {
            name: e.name.clone(),
            bit_width: e.bit_width,
            bitflags: false,
            values: enum_values(e),
        },
        TypeDef::Bitmask(b) => {
            let values = match b.bit_enum.as_ref().and_then(|n| model.type_def(n)) {
                Some(TypeDef::Enum(e)) => enum_values(e),
                _ => Vec::new(),
            };
            AbiType::Newtype {
                name: b.name.clone(),
                bit_width: b.bit_width,
                bitflags: true,
                values,
            }
        }
        TypeDef::Struct(s) => {
            // A C bitfield struct is one `uint32_t` and is passed like one, so
            // it gets the same `#[repr(transparent)]` newtype a `VkFlags` does.
            // The single-bit members become named constants; a multi-bit one
            // (`reserved : 30`, which H.265 and AV1 have and H.264 does not) is
            // not a bit and gets none, rather than a constant that is a lie.
            if let Some(fw) = crate::plan::flag_word_of(model, s)? {
                let values = fw
                    .fields
                    .iter()
                    .filter(|b| b.bits == 1)
                    .map(|b| (b.name.clone(), 1i64 << b.offset))
                    .collect();
                return Ok(Some(AbiType::Newtype {
                    name: s.name.clone(),
                    bit_width: fw.bit_width,
                    bitflags: true,
                    values,
                }));
            }
            let lengths = length_members(&s.members);
            let mut members = Vec::new();
            for m in &s.members {
                members.push(member(model, &s.name, m, &lengths, wanted)?);
            }
            AbiType::Struct { name: s.name.clone(), is_union: s.is_union, members }
        }
        TypeDef::FuncPointer(f) => {
            note_type(model, &f.return_type, wanted);
            for (_, t) in &f.params {
                note_type(model, t, wanted);
            }
            AbiType::FuncPointer {
                name: f.name.clone(),
                return_type: f.return_type.clone(),
                params: f.params.clone(),
            }
        }
        // A platform type used by value is an integer with a width, and the
        // width is written down once, in `exceptions::PLATFORM_SCALARS`, with
        // the header that states it. Everything else stays incomplete, which is
        // the whole ABI of a type only seen behind a pointer.
        TypeDef::External { name, .. } => match crate::exceptions::platform_scalar(name) {
            Some(prim) => AbiType::Alias {
                name: name.clone(),
                underlying: CType::named(prim),
            },
            None => AbiType::Opaque { name: name.clone() },
        },
    }))
}

fn enum_values(e: &vk_registry::EnumType) -> Vec<(String, i64)> {
    let direct: BTreeMap<&str, i64> = e
        .values
        .iter()
        .filter(|v| v.alias_of.is_none())
        .map(|v| (v.name.as_str(), v.value))
        .collect();
    let mut out: BTreeMap<String, i64> =
        direct.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    for v in &e.values {
        let Some(first) = &v.alias_of else { continue };
        let mut target = first.as_str();
        for _ in 0..8 {
            if let Some(value) = direct.get(target) {
                out.insert(v.name.clone(), *value);
                break;
            }
            match e.values.iter().find(|x| x.name == target).and_then(|x| x.alias_of.as_deref()) {
                Some(next) => target = next,
                None => break,
            }
        }
    }
    out.into_iter().collect()
}

fn enum_value(model: &Model, enum_name: &str, constant: &str) -> Option<i64> {
    let TypeDef::Enum(e) = model.type_def(enum_name)? else { return None };
    let mut name = constant;
    for _ in 0..8 {
        let v = e.values.iter().find(|v| v.name == name)?;
        match &v.alias_of {
            Some(target) => name = target,
            None => return Some(v.value),
        }
    }
    None
}

/// Resolve an array extent, for `[T; N]`.
pub fn extent(model: &Model, ext: &ArrayExtent) -> Result<u64> {
    model
        .extent_value(ext)
        .ok_or_else(|| anyhow::anyhow!("array extent {ext} does not resolve to a number"))
}

/// The Rust primitive a C primitive is, at the ABI.
///
/// `size_t` is `usize` here and `u64` on the wire, and that difference is the
/// whole point of keeping the two plans apart: the ABI must match the compiler
/// that built the application, and the wire must match the peer.
pub fn primitive_name(p: Primitive) -> &'static str {
    match p {
        Primitive::Void => "core::ffi::c_void",
        Primitive::Char => "core::ffi::c_char",
        Primitive::I8 => "i8",
        Primitive::U8 => "u8",
        Primitive::I16 => "i16",
        Primitive::U16 => "u16",
        Primitive::I32 => "i32",
        Primitive::U32 => "u32",
        Primitive::I64 => "i64",
        Primitive::U64 => "u64",
        Primitive::F32 => "f32",
        Primitive::F64 => "f64",
        Primitive::Size => "usize",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Option<Model> {
        let path = vk_registry::registry_path().ok()?;
        let reg = vk_registry::Registry::load_from(path).ok()?;
        reg.model().ok()
    }

    fn plan() -> Option<AbiPlan> {
        let m = model()?;
        let wire = vk_registry::profiles::zink();
        let forwarded: BTreeSet<String> =
            wire.resolve(&m).unwrap().commands.iter().cloned().collect();
        Some(build(&m, &surface_profile(&wire), &forwarded).expect("abi plan"))
    }

    /// The C surface is not the wire profile, and the difference is exactly the
    /// commands the ICD implements itself.
    #[test]
    fn the_surface_is_a_superset_of_the_wire() {
        let Some(p) = plan() else { return };
        eprintln!("abi commands {} types {}", p.commands.len(), p.types.len());
        let gipa = p.command("vkGetInstanceProcAddr").expect("vkGetInstanceProcAddr");
        assert!(!gipa.forwarded, "proc-address lookup cannot be forwarded");
        assert_eq!(gipa.level, Level::Instance);
        let ci = p.command("vkCreateInstance").expect("vkCreateInstance");
        assert!(ci.forwarded);
        assert_eq!(ci.level, Level::Global);
        assert_eq!(ci.params.len(), 3, "pAllocator is part of the C signature");
        assert_eq!(
            p.command("vkGetPhysicalDeviceMemoryProperties").unwrap().level,
            Level::PhysicalDevice
        );
        assert_eq!(p.command("vkCmdDraw").unwrap().level, Level::Device);
        assert!(
            p.type_named("VkAllocationCallbacks").is_some(),
            "stripped from the wire, still in every C signature"
        );
    }

    /// Every `len=` on the surface either resolves to a sibling or is named.
    ///
    /// This is the R8 check that the wire plan gets for free and this one does
    /// not: here `len=` is what bounds a read of the application's memory.
    #[test]
    fn unresolved_lengths_are_named_not_guessed() {
        let Some(p) = plan() else { return };
        let mut bad = Vec::new();
        for c in &p.commands {
            for m in &c.params {
                if let Some(ArrayLen::Unresolved(e)) = &m.len {
                    bad.push(format!("{}.{} len={e}", c.name, m.name));
                }
            }
        }
        for t in &p.types {
            if let AbiType::Struct { name, members, .. } = t {
                for m in members {
                    if let Some(ArrayLen::Unresolved(e)) = &m.len {
                        bad.push(format!("{name}.{} len={e}", m.name));
                    }
                }
            }
        }
        assert!(
            bad.is_empty(),
            "every `len=` on the C surface must resolve to a concrete count, because \
             here it bounds a read of the application's memory rather than being the \
             cross-check design.md §7.4 demotes it to on the wire: {bad:#?}"
        );
    }
}
