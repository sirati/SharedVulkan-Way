//! The typed, fully-resolved intermediate representation the generator consumes.
//!
//! `vk-parse` gives a faithful transcription of `vk.xml`: attributes as
//! `Option<String>`, declarations as raw C text, aliases unresolved, extension
//! enum values left as `(extnumber, offset, dir)` triples. That is the right
//! shape for a parser and the wrong shape for a code generator, which wants to
//! ask "what is the element type of this array, and which sibling member holds
//! its length" and get an answer rather than a string to re-parse.
//!
//! This module is that answer, and it is the *only* place `vk_parse`'s types are
//! touched (docs/design.md §7.1: `vk-registry` is the only thing that reads the
//! registry). Everything downstream sees [`Model`].
//!
//! # What "resolved" means here
//!
//! - Declarations are parsed into [`crate::ctype::CType`], so indirection depth
//!   and per-level constness are facts rather than substrings.
//! - `len=` is parsed into [`LenSpec`], one term per pointer level, with
//!   `altlen=` preferred over `latexmath:` exactly as docs/design.md §7.4 requires.
//! - `optional=` is split per level into a `Vec<bool>`.
//! - Type and command aliases are recorded but *not* collapsed: the generator
//!   needs both the alias and the canonical name, because the wire protocol
//!   gives an alias the same opcode as its target while the ICD must still
//!   export both symbols.
//! - Extension enum values are computed from the registry's own formula.
//! - `structextends` is inverted into [`Model::extended_by`], which is what a
//!   `pNext` walker actually needs.
//!
//! # What is deliberately *not* resolved
//!
//! Feature and extension gating. That is a question about a *profile* — which
//! subset are we generating for — and it lives in [`crate::profile`], because
//! the answer differs per build while the model does not.

use crate::ctype::{ArrayExtent, CType, CTypeKind, Declarator, parse_proto};
use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use std::collections::BTreeMap;

/// The whole registry, in the shape the generator wants.
#[derive(Debug, Clone, Default)]
pub struct Model {
    /// Every type, keyed by name, in registry declaration order.
    pub types: IndexMap<String, TypeDef>,
    /// Every command *definition*. Aliases are in [`Model::command_aliases`].
    pub commands: IndexMap<String, Command>,
    /// `<enums name="API Constants">` — `VK_MAX_EXTENSION_NAME_SIZE` and friends.
    pub constants: IndexMap<String, Constant>,
    /// Alias name → the name it aliases, for types.
    pub type_aliases: IndexMap<String, String>,
    /// Alias name → the name it aliases, for commands.
    pub command_aliases: IndexMap<String, String>,
    /// Extended struct name → the structs that name it in `structextends`.
    ///
    /// This is the inverse of the registry's own relation and the direction a
    /// `pNext` walker needs: given a `VkImageCreateInfo`, what may legally
    /// appear in its chain.
    pub extended_by: IndexMap<String, Vec<String>>,
    /// `<feature>` blocks — the core versions.
    pub features: Vec<FeatureSet>,
    /// `<extension>` blocks.
    pub extensions: Vec<FeatureSet>,
}

/// A `<feature>` or `<extension>`: a named bundle of interface items.
///
/// Both are the same shape in the registry and both answer the same question
/// for us — "which commands and types does enabling this bring in" — so they
/// are one type here.
#[derive(Debug, Clone, Default)]
pub struct FeatureSet {
    pub name: String,
    /// `VK_VERSION_1_0` etc. have no number; extensions do.
    pub number: Option<i64>,
    /// `"instance"` or `"device"` for extensions; `None` for core versions.
    pub ext_kind: Option<String>,
    /// `"win32"`, `"android"`, … — a platform-specific extension we will not
    /// generate for unless the profile explicitly asks.
    pub platform: Option<String>,
    /// Raw `depends=` expression, kept verbatim; the profile evaluates it.
    pub depends: Option<String>,
    /// Whether the registry marks this as promoted/deprecated/obsoleted.
    pub promoted_to: Option<String>,
    pub deprecated_by: Option<String>,
    pub obsoleted_by: Option<String>,
    /// `true` when `supported=` does not include `vulkan` (e.g. `vulkansc`,
    /// `disabled`). The profile refuses to enable these.
    pub supported_for_vulkan: bool,
    pub requires: Vec<RequireBlock>,
    /// Items a core version *removes* — Vulkan 1.4 deprecates a few.
    pub removes: Vec<InterfaceRef>,
}

#[derive(Debug, Clone, Default)]
pub struct RequireBlock {
    /// `depends=` on the `<require>` itself, which gates the block.
    pub depends: Option<String>,
    /// `api=` on the `<require>`; blocks not applying to `vulkan` are dropped
    /// at build time and never appear here.
    pub items: Vec<InterfaceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterfaceRef {
    Type(String),
    Command(String),
    /// An enum constant, possibly one that extends an existing enum type.
    Enum { name: String, extends: Option<String> },
}

/// One entry of `<enums name="API Constants">`.
#[derive(Debug, Clone)]
pub struct Constant {
    pub name: String,
    /// The literal text, e.g. `256`, `(~0U)`, `1000.0F`.
    pub text: String,
    /// The value, when it is a plain integer. Array extents always are; the
    /// sentinel constants (`VK_WHOLE_SIZE`, `(~0ULL)`) are not, and are left
    /// `None` rather than guessed at.
    pub value: Option<u64>,
    /// Alias target, for `VK_LUID_SIZE_KHR = VK_LUID_SIZE`.
    pub alias: Option<String>,
}

#[derive(Debug, Clone)]
pub enum TypeDef {
    /// A C primitive the registry does not define but uses: `uint32_t`,
    /// `float`, `char`, `size_t`, `void`.
    Primitive(Primitive),
    /// `typedef uint32_t VkBool32;` — a name for something else.
    BaseType { name: String, underlying: Option<CType> },
    Handle(Handle),
    Enum(EnumType),
    Bitmask(Bitmask),
    Struct(StructType),
    FuncPointer(FuncPointer),
    /// A type from a platform header the registry only names: `HWND`,
    /// `ANativeWindow`, `xcb_connection_t`. Not serialisable, and the generator
    /// must reject any subset that reaches one (docs/design.md §6.1 exclusion list).
    External { name: String, requires: Option<String> },
}

impl TypeDef {
    pub fn name(&self) -> &str {
        match self {
            TypeDef::Primitive(p) => p.name(),
            TypeDef::BaseType { name, .. } => name,
            TypeDef::Handle(h) => &h.name,
            TypeDef::Enum(e) => &e.name,
            TypeDef::Bitmask(b) => &b.name,
            TypeDef::Struct(s) => &s.name,
            TypeDef::FuncPointer(f) => &f.name,
            TypeDef::External { name, .. } => name,
        }
    }
}

/// The C primitives that actually occur in `vk.xml`.
///
/// A closed set rather than a string, so that an unrecognised primitive is a
/// generator-time error instead of a silently mis-sized field. This is the
/// smallest and cheapest defence against R8 available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Primitive {
    Void,
    Char,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    /// `size_t`. Its width differs between the two sides in principle, which is
    /// exactly why Venus's rules pin it to 64 bits on the wire (docs/design.md §6.1).
    Size,
}

impl Primitive {
    pub fn from_c_name(name: &str) -> Option<Primitive> {
        Some(match name {
            "void" => Primitive::Void,
            "char" => Primitive::Char,
            "int8_t" => Primitive::I8,
            "uint8_t" => Primitive::U8,
            "int16_t" => Primitive::I16,
            "uint16_t" => Primitive::U16,
            "int32_t" | "int" => Primitive::I32,
            "uint32_t" => Primitive::U32,
            "int64_t" => Primitive::I64,
            "uint64_t" => Primitive::U64,
            "float" => Primitive::F32,
            "double" => Primitive::F64,
            "size_t" => Primitive::Size,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Primitive::Void => "void",
            Primitive::Char => "char",
            Primitive::I8 => "int8_t",
            Primitive::U8 => "uint8_t",
            Primitive::I16 => "int16_t",
            Primitive::U16 => "uint16_t",
            Primitive::I32 => "int32_t",
            Primitive::U32 => "uint32_t",
            Primitive::I64 => "int64_t",
            Primitive::U64 => "uint64_t",
            Primitive::F32 => "float",
            Primitive::F64 => "double",
            Primitive::Size => "size_t",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Handle {
    pub name: String,
    /// `true` for `VK_DEFINE_HANDLE` (dispatchable), `false` for
    /// `VK_DEFINE_NON_DISPATCHABLE_HANDLE`.
    ///
    /// The distinction survives into our design: a dispatchable handle the app
    /// side returns must be a pointer to a `#[repr(C)]` struct whose first word
    /// the loader overwrites (docs/design.md §6.4), while a non-dispatchable one is
    /// just an id. On the wire both are a u64 id.
    pub dispatchable: bool,
    /// `parent=` — `VkDevice` for `VkBuffer`, etc. Used by the GPU-side object
    /// table to scope ids and to destroy children with their parent.
    pub parent: Option<String>,
    /// `objtypeenum=` — the `VkObjectType` value, needed for debug-utils
    /// forwarding over the reverse ring.
    pub obj_type_enum: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnumType {
    pub name: String,
    /// 32 unless the registry says otherwise.
    pub bit_width: u32,
    pub values: Vec<EnumValue>,
}

#[derive(Debug, Clone)]
pub struct Bitmask {
    pub name: String,
    /// The `VkFlags`/`VkFlags64` typedef underneath.
    pub bit_width: u32,
    /// `requires=`/`bitvalues=` — the `*FlagBits` enum holding the bit names.
    pub bit_enum: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnumValue {
    pub name: String,
    pub value: i64,
    /// Which feature or extension introduced it, for gating.
    pub provenance: Option<String>,
    /// Set when this name is an alias of another value of the same enum.
    pub alias_of: Option<String>,
    pub deprecated: bool,
}

#[derive(Debug, Clone)]
pub struct StructType {
    pub name: String,
    pub is_union: bool,
    pub members: Vec<Member>,
    /// `returnedonly="true"` — the struct is only ever filled in by the
    /// implementation. Venus's three-state output validity keys off this
    /// (docs/design.md §6.1).
    pub returned_only: bool,
    /// `structextends=` — the structs this one may appear in the `pNext` of.
    pub extends: Vec<String>,
    /// The `VK_STRUCTURE_TYPE_*` constant from the `values=` attribute on
    /// `sType`, when there is one.
    pub s_type: Option<String>,
    pub allow_duplicate: bool,
}

impl StructType {
    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }
    /// Whether this struct participates in `pNext` chains at all.
    pub fn has_p_next(&self) -> bool {
        self.members.iter().any(|m| m.name == "pNext")
    }
}

/// A struct member, or — the same shape, and deliberately the same type — a
/// command parameter. Everything the generator asks about a member it also asks
/// about a parameter.
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub ty: CType,
    pub bitfield_bits: Option<u32>,
    /// The parsed `len=`/`altlen=` pair, one term per pointer level.
    pub len: Option<LenSpec>,
    /// `optional=`, split per level. `optional[0]` is about the outermost
    /// pointer. Missing levels are `false`.
    pub optional: Vec<bool>,
    /// `externsync=` verbatim. Not used by marshalling; used by the GPU side to
    /// know which objects a command mutates, which is what the deferred-destroy
    /// bookkeeping needs (docs/design.md §5.5).
    pub externsync: Option<String>,
    /// `selector=` — for a union member, the sibling holding the discriminator.
    pub selector: Option<String>,
    /// `selection=` — for a member *of* a union, the discriminator values that
    /// choose it.
    pub selection: Vec<String>,
    /// `stride=` — the array this member points at is spaced by the named
    /// sibling's value in **bytes**, not by `size_of` its element type.
    ///
    /// Two parameters in the whole registry carry it, both multi-draw, and
    /// until docs/design.md §0.24 nothing read it — so a strided array was
    /// marshalled as though tightly packed, and the GPU side handed the driver a
    /// repacked array together with the *application's* stride, which walks off
    /// the end of it.
    pub stride: Option<String>,
    /// `values=` — the fixed `sType` value.
    pub values: Vec<String>,
    /// `noautovalidity="true"`, which usually marks a member whose presence is
    /// decided by something the registry cannot express. docs/design.md §7.4's
    /// "conditional presence" cases all carry it.
    pub no_auto_validity: bool,
    /// `objecttype=` — for a sibling member naming the `VkObjectType` of an
    /// untyped handle.
    pub object_type: Option<String>,
    pub deprecated: Option<String>,
    /// `api=` — a member that exists only for `vulkansc` is dropped at build
    /// time and never appears here.
    pub api: Option<String>,
}

impl Member {
    pub fn is_optional(&self, level: usize) -> bool {
        self.optional.get(level).copied().unwrap_or(false)
    }
    /// Whether the outermost pointer is to non-const storage — the registry's
    /// marker for a callee-filled parameter.
    pub fn is_output_pointer(&self) -> bool {
        match self.ty.as_pointer() {
            Some(inner) => !inner.is_const,
            None => false,
        }
    }
}

/// The `len=` attribute, one term per level of indirection.
///
/// `len="enabledExtensionCount,null-terminated"` on a `const char* const*` says
/// "an array of `enabledExtensionCount` pointers, each to a NUL-terminated
/// string". The comma-separated list is therefore positional, outermost first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LenSpec(pub Vec<LenTerm>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LenTerm {
    /// `null-terminated` — a C string, and the only case whose length is not
    /// known before walking it.
    NullTerminated,
    /// A literal count, which in practice is only ever `1`.
    Literal(u64),
    /// The name of a sibling member or parameter.
    Sibling(String),
    /// `pAllocateInfo->commandBufferCount` — a member of another parameter.
    Path { root: String, field: String },
    /// A C expression from `altlen=`, e.g. `codeSize / 4`.
    ///
    /// docs/design.md §7.4 is explicit: read `altlen`, never `latexmath:`. There are
    /// four of these in the whole registry and all four are simple arithmetic,
    /// so they are carried as text and evaluated by the generator's expression
    /// mapper rather than by a parser nobody needs.
    Expr(String),
}

impl LenSpec {
    pub fn term(&self, level: usize) -> Option<&LenTerm> {
        self.0.get(level)
    }
}

#[derive(Debug, Clone)]
pub struct FuncPointer {
    pub name: String,
    pub return_type: CType,
    pub params: Vec<(String, CType)>,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub return_type: CType,
    pub params: Vec<Member>,
    pub success_codes: Vec<String>,
    pub error_codes: Vec<String>,
    /// `cmdbufferlevel=` — present exactly on the `vkCmd*` family, which is what
    /// the restricted sub-decoder of docs/design.md §6.5 is allowed to accept.
    pub cmd_buffer_level: Option<String>,
    pub queues: Option<String>,
    pub render_pass: Option<String>,
    /// Parameters the registry marks as implicitly externally synchronised.
    pub implicit_externsync: Vec<String>,
}

impl Command {
    /// Whether this belongs to the recorded-command-buffer subset.
    ///
    /// Name prefix *and* `cmdbufferlevel`, because either alone is a guess:
    /// `vkCmdExecuteCommands` is a `vkCmd*` that the sub-decoder must handle
    /// specially, and the registry marks the family properly.
    pub fn is_command_buffer_recorded(&self) -> bool {
        self.cmd_buffer_level.is_some() && self.name.starts_with("vkCmd")
    }
    /// The object the command dispatches from — its first parameter, when that
    /// is a handle. Used to decide which ring a call belongs on.
    pub fn dispatch_param(&self) -> Option<&Member> {
        self.params.first()
    }

    /// Whether the specification requires the caller to externally synchronise
    /// a `VkQueue` for this command.
    ///
    /// Two spellings in the registry mean the same obligation and both have to
    /// be read, which is why this is a method rather than a grep:
    ///
    /// - an explicit `externsync=` on a parameter whose type is `VkQueue` —
    ///   `vkQueueSubmit`, `vkQueueSubmit2`, `vkQueueWaitIdle`,
    ///   `vkQueueBindSparse`, the debug-utils label family;
    /// - an `<implicitexternsyncparams>` entry naming "all sname:VkQueue objects
    ///   created from pname:device" — `vkDeviceWaitIdle` and `vkDestroyDevice`,
    ///   which have no queue parameter at all and are therefore invisible to the
    ///   first rule.
    ///
    /// This is what pass 12 consults to decide which generated dispatches take
    /// the device's queue lock. A GPU side opens **one** queue and every session
    /// shares it (docs/design.md §0.12 item 4), so the application's own
    /// synchronisation cannot discharge the obligation: two clients that each
    /// use their queue correctly still reach one driver `VkQueue` at once.
    pub fn externally_synchronizes_a_queue(&self) -> bool {
        let explicit = self
            .params
            .iter()
            .any(|p| p.externsync.is_some() && p.ty.base_name() == Some("VkQueue"));
        // The implicit list is prose, and prose is what the registry gives — the
        // entries are asciidoc, `ifdef::` blocks and all. Matching on the type
        // name is the only stable thing in them.
        let implicit = self.implicit_externsync.iter().any(|p| p.contains("VkQueue"));
        explicit || implicit
    }

    /// Whether the specification requires the caller to externally synchronise a
    /// `VkCommandPool` or a `VkCommandBuffer` for this command.
    ///
    /// The same two spellings as [`Self::externally_synchronizes_a_queue`], and
    /// the same argument for why the application's own synchronisation cannot
    /// discharge the obligation on our behalf — one step further out.
    ///
    /// Vulkan lets an application record two command buffers at once provided it
    /// synchronises each buffer and its pool. **A well-behaved application does
    /// that on its own threads, and then this design replays its recordings on
    /// *ours*** (docs/design.md §6.5: a `vkCmd*` is appended to a blob and the
    /// blob is decoded by whichever ring thread receives it). The pairing
    /// between "the thread that recorded" and "the thread that executes" is
    /// broken by construction, so the obligation lands here.
    ///
    /// Measured before it was derived: `gl-probe` under Zink crashed roughly one
    /// run in two inside `radv_amdgpu_cs_finalize`, reached from
    /// `vkEndCommandBuffer` in a blob replay — and **not at all under the
    /// validation layer**, whose own locking hid it. docs/design.md §0.23.
    pub fn externally_synchronizes_a_command_pool(&self) -> bool {
        let is_pool = |n: Option<&str>| matches!(n, Some("VkCommandPool" | "VkCommandBuffer"));
        let explicit =
            self.params.iter().any(|p| p.externsync.is_some() && is_pool(p.ty.base_name()));
        let implicit = self
            .implicit_externsync
            .iter()
            .any(|p| p.contains("VkCommandPool") || p.contains("VkCommandBuffer"));
        explicit || implicit
    }
}

// ---------------------------------------------------------------------------
// Building the model
// ---------------------------------------------------------------------------

/// The API this model is built for. `vk.xml` also describes Vulkan SC, whose
/// declarations differ; mixing the two is a silent-corruption class of error, so
/// the filter is applied once, here, rather than at every use.
const API: &str = "vulkan";

fn api_matches(attr: Option<&str>) -> bool {
    match attr {
        None => true,
        Some(list) => list.split(',').any(|a| a.trim() == API),
    }
}

impl Model {
    /// Build the model from a parsed registry.
    pub fn build(raw: &vk_parse::Registry) -> Result<Model> {
        let mut m = Model::default();

        // Primitives are not in the registry; it just uses them. Seed them so
        // that every name a declarator mentions resolves to something.
        for p in [
            Primitive::Void,
            Primitive::Char,
            Primitive::I8,
            Primitive::U8,
            Primitive::I16,
            Primitive::U16,
            Primitive::I32,
            Primitive::U32,
            Primitive::I64,
            Primitive::U64,
            Primitive::F32,
            Primitive::F64,
            Primitive::Size,
        ] {
            m.types.insert(p.name().to_string(), TypeDef::Primitive(p));
        }
        // `int` is a distinct spelling of the same thing and does occur.
        m.types.insert("int".into(), TypeDef::Primitive(Primitive::I32));

        // Enum values arrive in two places — the `<enums>` block and every
        // `<require>` that extends an enum — so they are collected separately
        // and merged in at the end.
        let mut extra_values: BTreeMap<String, Vec<EnumValue>> = BTreeMap::new();

        for child in &raw.0 {
            match child {
                vk_parse::RegistryChild::Types(types) => m.build_types(types)?,
                vk_parse::RegistryChild::Enums(enums) => m.build_enums(enums)?,
                vk_parse::RegistryChild::Commands(cmds) => m.build_commands(cmds)?,
                vk_parse::RegistryChild::Feature(f) => {
                    let fs = m.build_feature_set(f, &mut extra_values)?;
                    m.features.push(fs);
                }
                vk_parse::RegistryChild::Extensions(exts) => {
                    for e in &exts.children {
                        let fs = m.build_extension(e, &mut extra_values)?;
                        m.extensions.push(fs);
                    }
                }
                _ => {}
            }
        }

        m.merge_extension_enum_values(extra_values);
        m.build_extended_by();
        m.resolve_constant_aliases();
        Ok(m)
    }

    /// Merge Khronos's *video std* registry — `video.xml` — into this model.
    ///
    /// `vk.xml` names the `StdVideo*` types and does not define them (see
    /// [`crate::video_registry_path`] for the whole argument). `video.xml` is
    /// the machine-readable definition the `vk_video/*.h` headers are generated
    /// from, in the same schema, so this is the same three passes the main
    /// registry gets and not a second reader.
    ///
    /// Three things it deliberately does **not** do.
    ///
    /// - **It does not add `video.xml`'s `<extension>` blocks to
    ///   [`Model::extensions`].** They are named `vulkan_video_codec_h264std`
    ///   and so on: header guards, not Vulkan extensions. A profile must never
    ///   be able to list one, and `VK_KHR_video_decode_h264` in `vk.xml` already
    ///   pulls in every type it needs by name.
    /// - **It does not touch `extended_by`.** No `StdVideo*` struct carries an
    ///   `sType` or a `pNext`; they are plain C aggregates that Vulkan structs
    ///   point at. That is also why nothing generic can walk them, and why the
    ///   registry's `len=`/`optional=` on their pointer members is the only
    ///   thing that says how deep a copy has to go.
    /// - **It does not overwrite a primitive.** `video.xml` re-declares
    ///   `uint32_t` and friends the way `vk.xml` does; `build_types` already
    ///   refuses to replace a seeded primitive with an opaque placeholder.
    ///
    /// What it *does* overwrite, on purpose, is the `TypeDef::External`
    /// placeholder `vk.xml` leaves for each `StdVideo*` name. After this call
    /// `StdVideoH264LevelIdc` is an enum with nineteen values, and
    /// `StdVideoH264SequenceParameterSet` is a struct with three pointer members
    /// whose lengths and optionality the registry states.
    pub fn merge_video(&mut self, raw: &vk_parse::Registry) -> Result<()> {
        for child in &raw.0 {
            match child {
                vk_parse::RegistryChild::Types(types) => self.build_types(types)?,
                vk_parse::RegistryChild::Enums(enums) => self.build_enums(enums)?,
                // `video.xml` declares no commands, and if a future release does
                // they are not Vulkan entry points and must not become opcodes.
                vk_parse::RegistryChild::Extensions(exts) => {
                    self.merge_video_constants(exts);
                }
                _ => {}
            }
        }

        // The array extents of the video structs are named constants —
        // `STD_VIDEO_H264_SCALING_LIST_4X4_NUM_LISTS` and eleven others — and an
        // extent that does not resolve to a number is a generator error, not a
        // zero-length array. Say so here, where the reason is legible, rather
        // than three layers away in `shape_of`.
        for def in self.types.values() {
            let TypeDef::Struct(s) = def else { continue };
            if !s.name.starts_with("StdVideo") {
                continue;
            }
            for m in &s.members {
                let mut ty = &m.ty;
                while let CTypeKind::Array(inner, ext) = &ty.kind {
                    if self.extent_value(ext).is_none() {
                        bail!(
                            "{}.{}: array extent {ext} is not a number video.xml \
                             defines. Every StdVideo array extent is an <enum> in a \
                             <require> block of video.xml's own <extensions>; a \
                             missing one means `merge_video_constants` did not see \
                             that block.",
                            s.name,
                            m.name
                        );
                    }
                    ty = inner;
                }
            }
        }

        // Deterministic emission order, as `merge_extension_enum_values` does
        // for the main registry.
        for def in self.types.values_mut() {
            if let TypeDef::Enum(et) = def {
                et.values.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
        Ok(())
    }

    /// `video.xml` states its API constants inside `<extension>/<require>`
    /// rather than in an `<enums name="API Constants">` block:
    ///
    /// ```xml
    /// <enum name="STD_VIDEO_H264_SCALING_LIST_4X4_NUM_LISTS" value="6" type="uint32_t"/>
    /// ```
    ///
    /// They are still API constants — every one is an array extent of a struct
    /// in the same file — so they go where `vk.xml`'s do. The string-valued ones
    /// (`…_EXTENSION_NAME`) come through with `value: None`, exactly as
    /// `VK_WHOLE_SIZE` does, rather than being guessed at.
    fn merge_video_constants(&mut self, exts: &vk_parse::Extensions) {
        for e in &exts.children {
            for child in &e.children {
                let vk_parse::ExtensionChild::Require { api, items, .. } = child else {
                    continue;
                };
                if !api_matches(api.as_deref()) {
                    continue;
                }
                for item in items {
                    let vk_parse::InterfaceItem::Enum(en) = item else { continue };
                    if !api_matches(en.api.as_deref()) {
                        continue;
                    }
                    // Only free-standing values. An `extends=` here would be a
                    // video enum extending a Vulkan enum, which does not occur
                    // and which this path is not the place to handle.
                    let (text, alias) = match &en.spec {
                        vk_parse::EnumSpec::Value { value, extends: None, .. } => {
                            (value.clone(), None)
                        }
                        vk_parse::EnumSpec::Alias { alias, extends: None, .. } => {
                            (alias.clone(), Some(alias.clone()))
                        }
                        _ => continue,
                    };
                    let value = parse_c_integer(&text);
                    self.constants.entry(en.name.clone()).or_insert(Constant {
                        name: en.name.clone(),
                        text,
                        value,
                        alias,
                    });
                }
            }
        }
        self.resolve_constant_aliases();
    }

    fn build_types(&mut self, types: &vk_parse::Types) -> Result<()> {
        for child in &types.children {
            let vk_parse::TypesChild::Type(ty) = child else { continue };
            if !api_matches(ty.api.as_deref()) {
                continue;
            }
            let Some(name) = type_name_of(ty) else { continue };

            if let Some(alias) = &ty.alias {
                self.type_aliases.insert(name, alias.clone());
                continue;
            }

            let def = match ty.category.as_deref() {
                Some("basetype") => {
                    let underlying = match &ty.spec {
                        vk_parse::TypeSpec::Code(c) => parse_typedef_target(&c.code),
                        _ => None,
                    };
                    TypeDef::BaseType { name: name.clone(), underlying }
                }
                Some("handle") => {
                    let code = match &ty.spec {
                        vk_parse::TypeSpec::Code(c) => c.code.as_str(),
                        _ => "",
                    };
                    TypeDef::Handle(Handle {
                        name: name.clone(),
                        dispatchable: code.contains("VK_DEFINE_HANDLE"),
                        parent: ty.parent.clone(),
                        obj_type_enum: ty.objtypeenum.clone(),
                    })
                }
                Some("enum") => TypeDef::Enum(EnumType {
                    name: name.clone(),
                    bit_width: 32,
                    values: Vec::new(),
                }),
                Some("bitmask") => {
                    let code = match &ty.spec {
                        vk_parse::TypeSpec::Code(c) => c.code.as_str(),
                        _ => "",
                    };
                    TypeDef::Bitmask(Bitmask {
                        name: name.clone(),
                        bit_width: if code.contains("VkFlags64") { 64 } else { 32 },
                        bit_enum: ty.bitvalues.clone().or_else(|| ty.requires.clone()),
                    })
                }
                Some(cat @ ("struct" | "union")) => {
                    let mut members = Vec::new();
                    if let vk_parse::TypeSpec::Members(ms) = &ty.spec {
                        for mm in ms {
                            let vk_parse::TypeMember::Definition(d) = mm else { continue };
                            if !api_matches(d.api.as_deref()) {
                                continue;
                            }
                            members.push(
                                build_member_from_type_member(d)
                                    .with_context(|| format!("in struct {name}"))?,
                            );
                        }
                    }
                    let s_type = members
                        .iter()
                        .find(|mm| mm.name == "sType")
                        .and_then(|mm| mm.values.first().cloned());
                    TypeDef::Struct(StructType {
                        name: name.clone(),
                        is_union: cat == "union",
                        members,
                        returned_only: ty.returnedonly.as_deref() == Some("true"),
                        extends: split_list(ty.structextends.as_deref()),
                        s_type,
                        allow_duplicate: ty.allowduplicate.as_deref() == Some("true"),
                    })
                }
                Some("funcpointer") => {
                    let vk_parse::TypeSpec::Funcpointer(fp) = &ty.spec else {
                        bail!("funcpointer {name} has no funcpointer spec");
                    };
                    let return_type = parse_funcpointer_return(&fp.proto.code)
                        .with_context(|| format!("return type of {name}"))?;
                    let mut params = Vec::new();
                    for p in &fp.params {
                        let d = Declarator::parse(&p.code)
                            .map_err(|e| anyhow::anyhow!("{e}"))
                            .with_context(|| format!("parameter of {name}"))?;
                        params.push((d.name, d.ty));
                    }
                    TypeDef::FuncPointer(FuncPointer { name: name.clone(), return_type, params })
                }
                // `<type requires="windows.h" name="HWND"/>` and the like: a
                // name the registry borrows and never defines. The same shape
                // is used for the C primitives (`<type name="uint32_t"/>`),
                // which are already seeded above and must not be overwritten
                // with an opaque placeholder.
                None => {
                    if matches!(self.types.get(&name), Some(TypeDef::Primitive(_))) {
                        continue;
                    }
                    TypeDef::External { name: name.clone(), requires: ty.requires.clone() }
                }
                Some(_) => continue,
            };
            self.types.insert(name, def);
        }
        Ok(())
    }

    fn build_enums(&mut self, enums: &vk_parse::Enums) -> Result<()> {
        let Some(group) = enums.name.clone() else { return Ok(()) };

        if group == "API Constants" {
            for c in &enums.children {
                let vk_parse::EnumsChild::Enum(e) = c else { continue };
                if !api_matches(e.api.as_deref()) {
                    continue;
                }
                let (text, alias) = match &e.spec {
                    vk_parse::EnumSpec::Value { value, .. } => (value.clone(), None),
                    vk_parse::EnumSpec::Alias { alias, .. } => (alias.clone(), Some(alias.clone())),
                    _ => continue,
                };
                let value = parse_c_integer(&text);
                self.constants.insert(
                    e.name.clone(),
                    Constant { name: e.name.clone(), text, value, alias },
                );
            }
            return Ok(());
        }

        let bit_width = enums.bitwidth.unwrap_or(32);
        let mut values = Vec::new();
        for c in &enums.children {
            let vk_parse::EnumsChild::Enum(e) = c else { continue };
            if !api_matches(e.api.as_deref()) {
                continue;
            }
            let (value, alias_of) = match &e.spec {
                vk_parse::EnumSpec::Value { value, .. } => match parse_c_signed(value) {
                    Some(v) => (v, None),
                    // A non-numeric value in a real enum would be a registry
                    // shape we do not understand; skipping it silently is how a
                    // generator emits a table with a hole in it.
                    None => bail!("enum {} has non-numeric value {value:?}", e.name),
                },
                vk_parse::EnumSpec::Bitpos { bitpos, .. } => (1i64 << bitpos, None),
                vk_parse::EnumSpec::Alias { alias, .. } => (0, Some(alias.clone())),
                _ => continue,
            };
            values.push(EnumValue {
                name: e.name.clone(),
                value,
                provenance: None,
                alias_of,
                deprecated: e.deprecated.is_some(),
            });
        }

        match self.types.get_mut(&group) {
            Some(TypeDef::Enum(et)) => {
                et.bit_width = bit_width;
                et.values.extend(values);
            }
            // `<enums>` blocks exist for `*FlagBits` names that are declared as
            // `enum` types; if the type was not declared at all, declare it now
            // rather than dropping the values.
            _ => {
                self.types.insert(
                    group.clone(),
                    TypeDef::Enum(EnumType { name: group, bit_width, values }),
                );
            }
        }
        Ok(())
    }

    fn build_commands(&mut self, cmds: &vk_parse::Commands) -> Result<()> {
        for c in &cmds.children {
            match c {
                vk_parse::Command::Alias { name, alias } => {
                    self.command_aliases.insert(name.clone(), alias.clone());
                }
                vk_parse::Command::Definition(d) => {
                    if !api_matches(d.api.as_deref()) {
                        continue;
                    }
                    let (name, return_type) = parse_proto(&d.proto.code)
                        .with_context(|| format!("prototype {:?}", d.proto.code))?;
                    let mut params = Vec::new();
                    for p in &d.params {
                        if !api_matches(p.api.as_deref()) {
                            continue;
                        }
                        params.push(
                            build_member_from_command_param(p)
                                .with_context(|| format!("in command {name}"))?,
                        );
                    }
                    self.commands.insert(
                        name.clone(),
                        Command {
                            name,
                            return_type,
                            params,
                            success_codes: split_list(d.successcodes.as_deref()),
                            error_codes: split_list(d.errorcodes.as_deref()),
                            cmd_buffer_level: d.cmdbufferlevel.clone(),
                            queues: d.queues.clone(),
                            render_pass: d.renderpass.clone(),
                            implicit_externsync: d.implicitexternsyncparams.clone(),
                        },
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn build_feature_set(
        &mut self,
        f: &vk_parse::Feature,
        extra: &mut BTreeMap<String, Vec<EnumValue>>,
    ) -> Result<FeatureSet> {
        let mut fs = FeatureSet {
            name: f.name.clone(),
            supported_for_vulkan: api_matches(Some(&f.api)),
            depends: f.depends.clone(),
            ..Default::default()
        };
        let supported = fs.supported_for_vulkan;
        collect_children(&f.children, &f.name, None, &mut fs, extra, supported);
        Ok(fs)
    }

    fn build_extension(
        &mut self,
        e: &vk_parse::Extension,
        extra: &mut BTreeMap<String, Vec<EnumValue>>,
    ) -> Result<FeatureSet> {
        let mut fs = FeatureSet {
            name: e.name.clone(),
            number: e.number,
            ext_kind: e.ext_type.clone(),
            platform: e.platform.clone(),
            depends: e.depends.clone(),
            promoted_to: e.promotedto.clone(),
            deprecated_by: e.deprecatedby.clone(),
            obsoleted_by: e.obsoletedby.clone(),
            supported_for_vulkan: e
                .supported
                .as_deref()
                .map(|s| s.split(',').any(|a| a.trim() == API))
                .unwrap_or(false),
            ..Default::default()
        };
        let supported = fs.supported_for_vulkan;
        collect_children(&e.children, &e.name, e.number, &mut fs, extra, supported);
        Ok(fs)
    }

    fn merge_extension_enum_values(&mut self, extra: BTreeMap<String, Vec<EnumValue>>) {
        for (group, values) in extra {
            if let Some(TypeDef::Enum(et)) = self.types.get_mut(&group) {
                for v in values {
                    if !et.values.iter().any(|e| e.name == v.name) {
                        et.values.push(v);
                    }
                }
            }
        }
        // Deterministic emission order, independent of the order the registry
        // happened to introduce extension values in.
        for def in self.types.values_mut() {
            if let TypeDef::Enum(et) = def {
                et.values.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
    }

    fn build_extended_by(&mut self) {
        let mut rel: IndexMap<String, Vec<String>> = IndexMap::new();
        for def in self.types.values() {
            let TypeDef::Struct(s) = def else { continue };
            for target in &s.extends {
                rel.entry(target.clone()).or_default().push(s.name.clone());
            }
        }
        for v in rel.values_mut() {
            v.sort();
        }
        rel.sort_keys();
        self.extended_by = rel;
    }

    fn resolve_constant_aliases(&mut self) {
        let resolved: Vec<(String, Option<u64>)> = self
            .constants
            .values()
            .filter_map(|c| {
                let target = c.alias.as_ref()?;
                let v = self.constants.get(target)?.value;
                Some((c.name.clone(), v))
            })
            .collect();
        for (name, v) in resolved {
            if let Some(c) = self.constants.get_mut(&name) {
                c.value = v;
            }
        }
    }

    // -- queries the generator asks ------------------------------------------

    /// Follow `type_aliases` to the name that actually has a definition.
    pub fn canonical_type(&self, name: &str) -> String {
        let mut cur = name.to_string();
        for _ in 0..16 {
            match self.type_aliases.get(&cur) {
                Some(next) => cur = next.clone(),
                None => break,
            }
        }
        cur
    }

    pub fn canonical_command(&self, name: &str) -> String {
        let mut cur = name.to_string();
        for _ in 0..16 {
            match self.command_aliases.get(&cur) {
                Some(next) => cur = next.clone(),
                None => break,
            }
        }
        cur
    }

    pub fn type_def(&self, name: &str) -> Option<&TypeDef> {
        self.types.get(&self.canonical_type(name))
    }

    pub fn struct_def(&self, name: &str) -> Option<&StructType> {
        match self.type_def(name) {
            Some(TypeDef::Struct(s)) => Some(s),
            _ => None,
        }
    }

    /// Resolve an array extent to a number, following API constants.
    pub fn extent_value(&self, ext: &ArrayExtent) -> Option<u64> {
        match ext {
            ArrayExtent::Literal(n) => Some(*n),
            ArrayExtent::Constant(c) => self.constants.get(c).and_then(|k| k.value),
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn type_name_of(ty: &vk_parse::Type) -> Option<String> {
    if let Some(n) = &ty.name {
        return Some(n.clone());
    }
    match &ty.spec {
        vk_parse::TypeSpec::Code(c) => c.markup.iter().find_map(|m| match m {
            vk_parse::TypeCodeMarkup::Name(n) => Some(n.clone()),
            _ => None,
        }),
        // A `funcpointer` carries its name on the proto, not on the element, and
        // dropping it here is how `PFN_*` types come to be "referenced but not
        // defined" three layers away.
        vk_parse::TypeSpec::Funcpointer(fp) => Some(fp.proto.name.clone()),
        _ => None,
    }
}

fn split_list(s: Option<&str>) -> Vec<String> {
    s.map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

/// `typedef uint32_t VkBool32;` → the `uint32_t` half.
fn parse_typedef_target(code: &str) -> Option<CType> {
    let code = code.trim().trim_end_matches(';');
    let rest = code.strip_prefix("typedef ")?;
    // The declarator parser handles the rest verbatim: `uint32_t VkBool32`.
    Declarator::parse(rest).ok().map(|d| d.ty)
}

/// The return type of a function-pointer typedef.
///
/// `vk-parse` reassembles the proto as `void* (VKAPI_PTR *PFN_vkAllocationFunction)`,
/// so everything before the first parenthesis is the return type. Giving it a
/// synthetic declarator name reuses the one declarator parser rather than
/// growing a second, subtly different one.
fn parse_funcpointer_return(code: &str) -> Result<CType> {
    let head = code.split('(').next().unwrap_or("").trim();
    if head.is_empty() {
        bail!("funcpointer proto {code:?} has no return type");
    }
    Ok(Declarator::parse(&format!("{head} _ret"))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("funcpointer return type from {code:?}"))?
        .ty)
}

fn build_member_from_type_member(d: &vk_parse::TypeMemberDefinition) -> Result<Member> {
    let decl = Declarator::parse(&d.code).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Member {
        name: decl.name,
        ty: decl.ty,
        bitfield_bits: decl.bitfield_bits,
        len: parse_len(d.len.as_deref(), d.altlen.as_deref()),
        optional: parse_optional(d.optional.as_deref()),
        externsync: d.externsync.clone(),
        // `stride=` is a command-parameter attribute in this registry version;
        // no struct member carries one, and both uses are multi-draw params.
        stride: None,
        selector: d.selector.clone(),
        selection: split_list(d.selection.as_deref()),
        values: split_list(d.values.as_deref()),
        no_auto_validity: d.noautovalidity.as_deref() == Some("true"),
        object_type: d.objecttype.clone(),
        deprecated: d.deprecated.clone(),
        api: d.api.clone(),
    })
}

fn build_member_from_command_param(p: &vk_parse::CommandParam) -> Result<Member> {
    let decl = Declarator::parse(&p.definition.code).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Member {
        name: decl.name,
        ty: decl.ty,
        bitfield_bits: decl.bitfield_bits,
        len: parse_len(p.len.as_deref(), p.altlen.as_deref()),
        optional: parse_optional(p.optional.as_deref()),
        externsync: p.externsync.clone(),
        stride: p.stride.clone(),
        selector: None,
        selection: Vec::new(),
        values: Vec::new(),
        no_auto_validity: p.noautovalidity.as_deref() == Some("true"),
        object_type: p.objecttype.clone(),
        deprecated: None,
        api: p.api.clone(),
    })
}

/// Parse `len=` into one term per pointer level, preferring `altlen=`.
///
/// docs/design.md §7.4: "read `altlen`; there are only 5 latexmath cases in the
/// registry and `altlen` exists to give the plain-C equivalent. Do not build a
/// LaTeX parser."
fn parse_len(len: Option<&str>, altlen: Option<&str>) -> Option<LenSpec> {
    let len = len?;
    let terms = len
        .split(',')
        .map(|t| {
            let t = t.trim();
            if t == "null-terminated" {
                LenTerm::NullTerminated
            } else if t.starts_with("latexmath:") {
                // The registry pairs every latexmath len with an altlen giving
                // the same thing in C. If it ever does not, the generator sees
                // an Expr it cannot map and fails loudly.
                LenTerm::Expr(altlen.unwrap_or(t).to_string())
            } else if let Ok(n) = t.parse::<u64>() {
                LenTerm::Literal(n)
            } else if let Some((root, field)) = t.split_once("->") {
                LenTerm::Path { root: root.trim().to_string(), field: field.trim().to_string() }
            } else if t.chars().all(|c| c.is_alphanumeric() || c == '_') {
                LenTerm::Sibling(t.to_string())
            } else {
                LenTerm::Expr(t.to_string())
            }
        })
        .collect();
    Some(LenSpec(terms))
}

/// `optional="true,false"` → `[true, false]`.
fn parse_optional(s: Option<&str>) -> Vec<bool> {
    s.map(|s| s.split(',').map(|t| t.trim() == "true").collect()).unwrap_or_default()
}

/// Parse the integer literals the registry actually writes: `256`, `0`,
/// `(~0U)`, `(~0ULL)`, `1000.0F`. Only plain decimals become values; the rest
/// stay `None` on purpose, because a sentinel silently truncated to a number is
/// worse than an absent one.
fn parse_c_integer(text: &str) -> Option<u64> {
    let t = text.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(hex.trim_end_matches(['U', 'u', 'L', 'l']), 16).ok();
    }
    t.parse::<u64>().ok()
}

fn parse_c_signed(text: &str) -> Option<i64> {
    let t = text.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(hex.trim_end_matches(['U', 'u', 'L', 'l']), 16).ok()
            .map(|v| v as i64);
    }
    t.parse::<i64>().ok()
}

/// Walk a `<feature>`/`<extension>`'s children, filling in the require blocks
/// and computing every extension enum value it introduces.
///
/// `supported` is whether the registry supports this feature or extension for
/// the `vulkan` API, and it gates the enum values — not the require blocks,
/// which stay recorded so that [`crate::profile`] can still name and refuse an
/// unsupported extension by name.
///
/// **Why that gate exists** (2026-08-12, found by the `vulkan_core.h`
/// cross-check in `sv-testkit`): `api_matches` is applied to each `<enum>`'s own
/// `api=` attribute, but a *whole extension* declared `supported="vulkansc"` or
/// `supported="disabled"` carries no `api=` on its individual enums — the
/// attribute lives on the extension. Without this gate the values of 225
/// disabled extensions and every Vulkan SC extension were merged into our enum
/// tables: `VK_STRUCTURE_TYPE_APPLICATION_PARAMETERS_EXT`,
/// `VK_MEMORY_HEAP_SEU_SAFE_BIT`, `VK_PIPELINE_CACHE_CREATE_READ_ONLY_BIT` and
/// 100-odd more, none of which are Vulkan.
///
/// This module's own header says mixing the two APIs "is a silent-corruption
/// class of error, so the filter is applied once, here". It was applied on two
/// of the three paths an enum value arrives by. This is the third — the same
/// shape as `security-model.md` R8's two realised instances: a filter that looks
/// applied, is not, and produces a plausible answer either way.
fn collect_children(
    children: &[vk_parse::ExtensionChild],
    owner: &str,
    owner_number: Option<i64>,
    fs: &mut FeatureSet,
    extra: &mut BTreeMap<String, Vec<EnumValue>>,
    supported: bool,
) {
    for child in children {
        match child {
            vk_parse::ExtensionChild::Require { api, depends, items, .. } => {
                if !api_matches(api.as_deref()) {
                    continue;
                }
                let mut block =
                    RequireBlock { depends: depends.clone(), items: Vec::with_capacity(items.len()) };
                for item in items {
                    match item {
                        vk_parse::InterfaceItem::Type { name, .. } => {
                            block.items.push(InterfaceRef::Type(name.clone()));
                        }
                        vk_parse::InterfaceItem::Command { name, .. } => {
                            block.items.push(InterfaceRef::Command(name.clone()));
                        }
                        vk_parse::InterfaceItem::Enum(e) => {
                            if !api_matches(e.api.as_deref()) {
                                continue;
                            }
                            let extends = enum_extends(&e.spec).cloned();
                            block.items.push(InterfaceRef::Enum {
                                name: e.name.clone(),
                                extends: extends.clone(),
                            });
                            if let (true, Some(group), Some(v)) =
                                (supported, extends, extension_enum_value(e, owner_number))
                            {
                                extra.entry(group).or_default().push(EnumValue {
                                    name: e.name.clone(),
                                    value: v.0,
                                    provenance: Some(owner.to_string()),
                                    alias_of: v.1,
                                    deprecated: e.deprecated.is_some(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
                fs.requires.push(block);
            }
            vk_parse::ExtensionChild::Remove { api, items, .. } => {
                if !api_matches(api.as_deref()) {
                    continue;
                }
                for item in items {
                    match item {
                        vk_parse::InterfaceItem::Type { name, .. } => {
                            fs.removes.push(InterfaceRef::Type(name.clone()));
                        }
                        vk_parse::InterfaceItem::Command { name, .. } => {
                            fs.removes.push(InterfaceRef::Command(name.clone()));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn enum_extends(spec: &vk_parse::EnumSpec) -> Option<&String> {
    match spec {
        vk_parse::EnumSpec::Alias { extends, .. } => extends.as_ref(),
        vk_parse::EnumSpec::Offset { extends, .. } => Some(extends),
        vk_parse::EnumSpec::Bitpos { extends, .. } => extends.as_ref(),
        vk_parse::EnumSpec::Value { extends, .. } => extends.as_ref(),
        // `vk_parse::EnumSpec` is `#[non_exhaustive]`. A spec kind this build
        // does not know is not an enum value we can place, so it is dropped —
        // and dropping it is safe here because an unplaced value simply does not
        // reach the generated tables, rather than reaching them with a wrong
        // number.
        _ => None,
    }
}

/// The registry's own formula for an extension-introduced enum value:
///
/// ```text
/// value = 1000000000 + (extnumber - 1) * 1000 + offset,  negated if dir="-"
/// ```
///
/// Getting this wrong would give every extension struct the wrong `sType`,
/// which the GPU side would reject as an unknown `pNext` link — loudly, thanks
/// to §6.1's asymmetric handling, but only after a confusing afternoon.
pub fn extension_enum_number(ext_number: i64, offset: i64, negative: bool) -> i64 {
    let v = 1_000_000_000 + (ext_number - 1) * 1000 + offset;
    if negative { -v } else { v }
}

fn extension_enum_value(e: &vk_parse::Enum, owner_number: Option<i64>) -> Option<(i64, Option<String>)> {
    match &e.spec {
        vk_parse::EnumSpec::Offset { offset, extnumber, dir, .. } => {
            let n = extnumber.or(owner_number)?;
            // `vk_parse`'s `dir` field is named after the registry attribute but
            // holds the *opposite* sense: it is set to `false` when the XML says
            // `dir="-"` and left `true` otherwise. Reading it as "is negative"
            // negates every extension-introduced enum value in the registry —
            // which is what the first version of this code did, and which no
            // symmetric round-trip test can possibly notice, because both sides
            // agree on the wrong number. It was caught by asserting a constant
            // against the registry (see `sType_values_match_the_registry`), and
            // it is exactly the failure mode `security-model.md` R8 describes.
            Some((extension_enum_number(n, *offset, !*dir), None))
        }
        vk_parse::EnumSpec::Bitpos { bitpos, .. } => Some((1i64 << bitpos, None)),
        vk_parse::EnumSpec::Value { value, .. } => parse_c_signed(value).map(|v| (v, None)),
        vk_parse::EnumSpec::Alias { alias, .. } => Some((0, Some(alias.clone()))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn len_splits_per_level() {
        let l = parse_len(Some("enabledExtensionCount,null-terminated"), None).unwrap();
        assert_eq!(
            l.0,
            vec![LenTerm::Sibling("enabledExtensionCount".into()), LenTerm::NullTerminated]
        );
    }

    #[test]
    fn len_prefers_altlen_over_latexmath() {
        let l = parse_len(
            Some("latexmath:[\\textrm{codeSize} \\over 4]"),
            Some("codeSize / 4"),
        )
        .unwrap();
        assert_eq!(l.0, vec![LenTerm::Expr("codeSize / 4".into())]);
    }

    #[test]
    fn len_path_form() {
        let l = parse_len(Some("pAllocateInfo->commandBufferCount"), None).unwrap();
        assert_eq!(
            l.0,
            vec![LenTerm::Path {
                root: "pAllocateInfo".into(),
                field: "commandBufferCount".into()
            }]
        );
    }

    #[test]
    fn optional_splits_per_level() {
        assert_eq!(parse_optional(Some("false,true")), vec![false, true]);
        assert_eq!(parse_optional(Some("true")), vec![true]);
        assert!(parse_optional(None).is_empty());
    }

    #[test]
    fn typedef_target() {
        assert_eq!(
            parse_typedef_target("typedef uint32_t VkBool32;"),
            Some(CType::named("uint32_t"))
        );
    }

    /// The registry formula, checked against a value everyone can look up:
    /// `VK_KHR_swapchain` is extension 2, and
    /// `VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR` is offset 0 → 1000001000.
    /// Enum values checked against facts that do not come from this code.
    ///
    /// Every other test in this crate compares the model to itself or to the
    /// same registry it was built from. These constants are published, stable
    /// and independently checkable, which is why this test is the one that found
    /// the sign inversion in `extension_enum_value` — a bug that inverted every
    /// extension-introduced enum value in the registry and that no round-trip
    /// test could see.
    #[test]
    fn extension_enum_values_match_the_published_constants() {
        let Ok(path) = crate::registry_path() else {
            eprintln!("VK_XML unset — skipping (run inside `nix develop`)");
            return;
        };
        let reg = crate::Registry::load_from(path).unwrap();
        let m = reg.model().unwrap();

        let value = |enum_name: &str, constant: &str| -> i64 {
            let Some(TypeDef::Enum(e)) = m.type_def(enum_name) else {
                panic!("{enum_name} is not an enum in the model")
            };
            let mut name = constant;
            for _ in 0..8 {
                let v = e
                    .values
                    .iter()
                    .find(|v| v.name == name)
                    .unwrap_or_else(|| panic!("{enum_name} has no value {name}"));
                match &v.alias_of {
                    Some(t) => name = t,
                    None => return v.value,
                }
            }
            panic!("alias chain for {constant} did not terminate")
        };

        // Core values, which the registry states directly.
        assert_eq!(value("VkStructureType", "VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO"), 12);
        assert_eq!(value("VkResult", "VK_SUCCESS"), 0);
        assert_eq!(value("VkResult", "VK_ERROR_OUT_OF_HOST_MEMORY"), -1);

        // Extension values, which the registry states as `(extnumber, offset,
        // dir)` and which this crate has to compute. These are the numbers in
        // `vulkan_core.h`.
        assert_eq!(
            value("VkStructureType", "VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO"),
            1_000_072_000
        );
        assert_eq!(
            value("VkStructureType", "VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR"),
            1_000_001_000
        );
        // The 1.1/1.2 block runs 49, 50, 51, 52 in `vulkan_core.h`.
        assert_eq!(
            value("VkStructureType", "VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES"),
            49
        );
        assert_eq!(
            value("VkStructureType", "VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES"),
            51
        );
        // The one direction that must be negative: an error code.
        assert_eq!(value("VkResult", "VK_ERROR_SURFACE_LOST_KHR"), -1_000_000_000);
        assert_eq!(value("VkResult", "VK_ERROR_OUT_OF_POOL_MEMORY"), -1_000_069_000);
        // …and a positive success code from the same mechanism.
        assert_eq!(value("VkResult", "VK_SUBOPTIMAL_KHR"), 1_000_001_003);

        // Not every extension value is negative, and not every one is positive:
        // if the sign logic were inverted, or dropped, one of these would fail.
        let Some(TypeDef::Enum(results)) = m.type_def("VkResult") else { unreachable!() };
        assert!(results.values.iter().any(|v| v.value > 1_000_000_000));
        assert!(results.values.iter().any(|v| v.value < -1_000_000_000));
    }

    /// Vulkan SC is a different API and its values are not ours.
    ///
    /// Found by the `vulkan_core.h` cross-check in `sv-testkit` on 2026-08-12:
    /// `api_matches` was applied to each `<enum>`'s own `api=` attribute, but an
    /// extension declared `supported="vulkansc"` or `supported="disabled"`
    /// carries that fact on the *extension*, and its individual enums have no
    /// `api=` at all. Every one of their values was therefore merged into our
    /// tables. This module's header says mixing the two APIs is a
    /// silent-corruption error; this is the assertion that it is not happening.
    #[test]
    fn values_from_unsupported_extensions_do_not_leak() {
        let Ok(path) = crate::registry_path() else {
            eprintln!("VK_XML unset — skipping (run inside `nix develop`)");
            return;
        };
        let reg = crate::Registry::load_from(path).unwrap();
        let m = reg.model().unwrap();

        let has = |enum_name: &str, constant: &str| -> bool {
            match m.type_def(enum_name) {
                Some(TypeDef::Enum(e)) => e.values.iter().any(|v| v.name == constant),
                _ => panic!("{enum_name} is not an enum in the model"),
            }
        };

        // Vulkan SC, from extensions marked `supported="vulkansc"`.
        for (group, name) in [
            ("VkStructureType", "VK_STRUCTURE_TYPE_APPLICATION_PARAMETERS_EXT"),
            ("VkStructureType", "VK_STRUCTURE_TYPE_EXPORT_MEMORY_SCI_BUF_INFO_NV"),
            ("VkMemoryHeapFlagBits", "VK_MEMORY_HEAP_SEU_SAFE_BIT"),
            ("VkPipelineCacheCreateFlagBits", "VK_PIPELINE_CACHE_CREATE_READ_ONLY_BIT"),
            ("VkResult", "VK_ERROR_INVALID_PIPELINE_CACHE_DATA"),
        ] {
            assert!(!has(group, name), "{name} is Vulkan SC's and must not be in our {group}");
        }

        // Placeholder extensions marked `supported="disabled"`, which reserve a
        // bit position under a name that will change when the extension ships.
        assert!(
            !has("VkAccessFlagBits2", "VK_ACCESS_2_RESERVED_28_BIT_AMD"),
            "a reserved bit from a disabled extension must not reach our tables"
        );

        // …while the real values are all still there.
        assert!(has("VkStructureType", "VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO"));
        assert!(has("VkStructureType", "VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR"));
        assert!(has("VkMemoryHeapFlagBits", "VK_MEMORY_HEAP_DEVICE_LOCAL_BIT"));
    }

    #[test]
    fn extension_enum_arithmetic() {
        // VK_KHR_swapchain is extension 2 and
        // VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR is its offset 0.
        assert_eq!(extension_enum_number(2, 0, false), 1_000_001_000);
        // VK_KHR_surface is extension 1; VK_ERROR_SURFACE_LOST_KHR is offset 0
        // with dir="-".
        assert_eq!(extension_enum_number(1, 0, true), -1_000_000_000);
    }
}
