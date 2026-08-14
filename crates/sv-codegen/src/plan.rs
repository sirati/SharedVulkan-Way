//! Model → serialisation plan.
//!
//! The generator has two halves and this is the first: it turns "what does the
//! registry say" into "what bytes go on the wire, in what order", with every
//! decision already taken. The emitters that follow are then mechanical — they
//! walk a [`Shape`] and print Rust — which is what makes them reviewable.
//!
//! Keeping the plan separate is also what makes the differential test of
//! docs/design.md §6.2/§7.3 possible: Venus's generated C is a sequence of typed
//! encode calls, and so is a [`Shape`] walk, so the two can be compared without
//! either of them running.
//!
//! # The one decision that shapes everything else
//!
//! **Every variable-length thing on our wire carries its own explicit `u64`
//! count**, exactly as Venus does. The registry's `len=` is therefore *not* what
//! the decoder uses to size an array — it reads the count that is actually
//! there. `len=` is used for two things only: to tell an array pointer from a
//! pointer to one value, and to generate a consistency check.
//!
//! That is a bigger deal than it looks. R8 says a wrong `len=` produces
//! "marshalling that is wrong in the same way on both sides and therefore
//! *looks* consistent while corrupting memory". With a self-describing count it
//! cannot: the worst a wrong `len=` can do is make the app side encode the wrong
//! number of elements, and the generated check between the array's length and
//! its `xxxCount` sibling catches exactly that, on the encoding side, before any
//! bytes are written.

use crate::exceptions::{self, ConsultedExceptions, VoidPolicy};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use vk_registry::{
    ArrayExtent, CType, CTypeKind, LenTerm, Member, Model, Primitive, ResolvedProfile, TypeDef,
};

/// A scalar as it appears on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scalar {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    /// `size_t`, widened to 64 bits on the wire.
    Size,
    Bool32,
    /// A 32-bit enum, encoded as `i32`.
    Enum(String),
    /// A 32-bit `VkFlags` bitmask.
    Bitmask(String),
    /// A 64-bit `VkFlags64` bitmask.
    Bitmask64(String),
    /// An object id.
    Handle(String),
    Struct(String),
    Union(String),
    /// A `void*` that carries an arena offset or an opaque token rather than a
    /// pointer (see [`crate::exceptions`]).
    OpaqueU64(VoidPolicy),
}

impl Scalar {
    /// The number of bytes this occupies on the wire, when it is fixed.
    ///
    /// `None` for structs and unions, whose size depends on their contents.
    pub fn wire_size(&self) -> Option<usize> {
        Some(match self {
            Scalar::U8 | Scalar::I8 | Scalar::U16 | Scalar::I16 => 4,
            Scalar::U32 | Scalar::I32 | Scalar::F32 | Scalar::Bool32 => 4,
            Scalar::Enum(_) | Scalar::Bitmask(_) => 4,
            Scalar::U64 | Scalar::I64 | Scalar::F64 | Scalar::Size => 8,
            Scalar::Bitmask64(_) | Scalar::Handle(_) | Scalar::OpaqueU64(_) => 8,
            Scalar::Struct(_) | Scalar::Union(_) => return None,
        })
    }

    /// Whether an array of these is packed with no per-element padding.
    ///
    /// Venus pads an array of sub-word elements once at the end and packs
    /// everything else. `wire_size >= 4` is exactly that condition, and the
    /// distinction is only meaningful for the primitives.
    pub fn is_packable(&self) -> bool {
        matches!(
            self,
            Scalar::U8
                | Scalar::I8
                | Scalar::U16
                | Scalar::I16
                | Scalar::U32
                | Scalar::I32
                | Scalar::U64
                | Scalar::I64
                | Scalar::F32
                | Scalar::F64
        )
    }

    pub fn type_name(&self) -> Option<&str> {
        match self {
            Scalar::Enum(n)
            | Scalar::Bitmask(n)
            | Scalar::Bitmask64(n)
            | Scalar::Handle(n)
            | Scalar::Struct(n)
            | Scalar::Union(n) => Some(n),
            _ => None,
        }
    }
}

/// Where a field's element count comes from, for the generated consistency
/// check. It is never what the decoder sizes an array by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LenCheck {
    /// The count lives in a sibling field of the same struct or parameter list.
    Sibling(String),
    /// `pAllocateInfo->commandBufferCount`.
    Path { root: String, field: String },
    /// `codeSize / 4`: the count is a sibling divided by a constant.
    SiblingDiv { sibling: String, divisor: u64 },
    /// The registry gives a length we cannot express as a check. Recorded so it
    /// appears in the manifest rather than being silently dropped.
    Unchecked(String),
}

/// The wire shape of one field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// A value written inline.
    Scalar(Scalar),
    /// A C array member with a compile-time extent: `char deviceName[256]`.
    ///
    /// Venus writes a length prefix for these too, using the constant, and
    /// checks it on decode. We keep that: it costs eight bytes and it turns "the
    /// two sides were built against different values of
    /// `VK_MAX_PHYSICAL_DEVICE_NAME_SIZE`" from silent corruption into an error.
    FixedArray { elem: Box<Shape>, count: u64, extent: ArrayExtent },
    /// A pointer to exactly one value: presence flag, then the value.
    Optional(Box<Shape>),
    /// A pointer to `n` values: count, then the values.
    Array { elem: Box<Shape>, len: Option<LenCheck> },
    /// A sized `void*`: count, then the bytes, padded to 4.
    Blob { len: Option<LenCheck> },
    /// A NUL-terminated string: length including the NUL, then the bytes.
    CString,
    /// An array of NUL-terminated strings.
    CStringArray { len: Option<LenCheck> },
    /// The `pNext` chain.
    PNext,
}

impl Shape {
    /// The smallest number of bytes a value of this shape can occupy, used by
    /// the decoder's count plausibility check.
    pub fn min_wire_size(&self) -> usize {
        match self {
            Shape::Scalar(s) => s.wire_size().unwrap_or(0),
            // A count word, and nothing is required after it.
            Shape::FixedArray { elem, count, .. } => 8 + elem.min_wire_size() * (*count as usize),
            Shape::Optional(_) => 8,
            Shape::Array { .. } | Shape::Blob { .. } | Shape::CStringArray { .. } => 8,
            Shape::CString => 8,
            // A chain is at minimum its terminator.
            Shape::PNext => 4,
        }
    }

    /// Every named type this shape mentions, for reachability analysis.
    pub fn mentions(&self, out: &mut BTreeSet<String>) {
        match self {
            Shape::Scalar(s) => {
                if let Some(n) = s.type_name() {
                    out.insert(n.to_string());
                }
            }
            Shape::FixedArray { elem, .. } | Shape::Array { elem, .. } | Shape::Optional(elem) => {
                elem.mentions(out)
            }
            Shape::Blob { .. } | Shape::CString | Shape::CStringArray { .. } | Shape::PNext => {}
        }
    }
}

/// One member of a struct, or one parameter of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPlan {
    /// The registry's name, kept for the manifest, the differential test and
    /// error messages.
    pub name: String,
    /// The Rust field or binding name.
    pub rust_name: String,
    pub shape: Shape,
    /// Whether the registry marks the outermost pointer optional. Only affects
    /// validity checks; the wire is the same either way.
    pub optional: bool,
    /// `noautovalidity="true"`.
    ///
    /// The registry saying, in as many words, that its own automatic rules do
    /// not describe this member — the valid-usage prose does. For a member with
    /// a `len=` that means **the length rule does not hold unconditionally**,
    /// and applying it anyway is the R8 class of bug this project exists to
    /// avoid.
    ///
    /// The case that found it: `VkWriteDescriptorSet` gives `descriptorCount` as
    /// the `len=` of `pImageInfo`, `pBufferInfo` *and* `pTexelBufferView`, and
    /// `descriptorType` selects exactly one. All three are `noautovalidity` and
    /// none is `optional`, so a check keyed only on `optional` rejected every
    /// legal `vkUpdateDescriptorSets` — the two arrays the type did not select
    /// are empty, and the count describes the one it did.
    pub no_auto_validity: bool,
    /// True when some other field's `len=` names this one, which is what makes
    /// a member "the count of" another (design.md R13's implicit-length problem,
    /// made explicit).
    pub is_a_length: bool,
    /// Non-const outermost pointer: the callee fills it in and it comes back in
    /// the reply, not in the command.
    pub is_output: bool,
    /// `selector=` — this member is a **union**, and the named sibling of the
    /// same struct holds the discriminator that says which arm is live.
    ///
    /// Ten members in the whole registry carry it, and until docs/design.md §0.21
    /// nothing read it: `exceptions`' own header records that unions needed no
    /// exception table because "a Rust `enum` *is* the tag", which is true of
    /// the wire and stops at the C ABI — where a union has no tag and the app
    /// side was reading member [0] regardless.
    pub selector: Option<String>,
    /// `selection=` — this member belongs to a union, and these are the
    /// discriminator values that choose it. Comma-separated in the registry and
    /// split here; several arms are chosen by more than one value.
    pub selection: Vec<String>,
}

/// A struct or union.
#[derive(Debug, Clone)]
pub struct StructPlan {
    pub name: String,
    pub is_union: bool,
    pub fields: Vec<FieldPlan>,
    /// The `VK_STRUCTURE_TYPE_*` constant, written by the encoder from a
    /// constant rather than carried as a field.
    ///
    /// Dropping `sType` from the Rust type removes a whole class of bug by
    /// construction: a value cannot have the wrong `sType`, because it does not
    /// have one.
    pub s_type: Option<String>,
    pub s_type_value: Option<i64>,
    pub has_p_next: bool,
    /// `returnedonly="true"`: the implementation fills it in, so the app side
    /// sends the *partial* form (docs/design.md §6.1, Venus's three-state validity).
    pub returned_only: bool,
    /// Whether a `_partial` encoding is generated for this struct.
    pub needs_partial: bool,
    /// Whether this type transitively contains a handle, and so needs a
    /// `handle_map` walker (docs/design.md §7.2 pass 5).
    pub has_handles: bool,
    /// Whether this type transitively contains a `VkDeviceMemory`, and so needs
    /// a `transform` walker (docs/design.md §7.2 pass 6, §5.3).
    pub has_device_memory: bool,
    /// The structs this one may carry in its `pNext`, restricted to the profile.
    pub extenders: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CommandPlan {
    pub name: String,
    /// Position in the sorted command list; the wire opcode.
    pub opcode: u32,
    /// Aliases that share this opcode.
    pub aliases: Vec<String>,
    /// Parameters the app side sends.
    pub inputs: Vec<FieldPlan>,
    /// Parameters the GPU side fills in and returns.
    pub outputs: Vec<FieldPlan>,
    /// `VkResult`, or `None` for a `void` command.
    pub returns_result: bool,
    /// The command returns a value that is neither `void` nor `VkResult` — a
    /// `VkDeviceAddress` or a bare `uint64_t`. It travels in the reply's `ret`
    /// field, which exists for exactly this.
    pub returns_value: bool,
    pub return_type: Option<String>,
    /// Whether a reply is generated at all. A command with no outputs and no
    /// result is fire-and-forget, which is most of the `vkCmd*` family
    /// (docs/design.md §4.4: "Most commands generate no reply at all").
    pub generates_reply: bool,
    /// Part of the recorded `vkCmd*` subset the restricted sub-decoder accepts
    /// (docs/design.md §6.5).
    pub is_recorded: bool,
    pub origin: String,
}

#[derive(Debug, Clone)]
pub struct EnumPlan {
    pub name: String,
    pub bit_width: u32,
    /// `(constant name, value)`, sorted by name, aliases resolved away.
    pub values: Vec<(String, i64)>,
}

#[derive(Debug, Clone)]
pub struct BitmaskPlan {
    pub name: String,
    pub bit_width: u32,
    pub values: Vec<(String, i64)>,
}

/// One member of a C bitfield struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitField {
    pub name: String,
    pub rust_name: String,
    /// Bit position of the low bit, counting from the least significant bit of
    /// the storage unit. See [`FlagWordPlan`] for why that direction is assumed
    /// and what checks it.
    pub offset: u32,
    pub bits: u32,
}

/// A C struct whose members are *all* bitfields of one `uint32_t`.
///
/// # Why this shape exists at all
///
/// docs/design.md §7.4 put bitfields out of scope and the planner refused them
/// outright, because `vk.xml`'s only bitfield struct is
/// `VkAccelerationStructureInstanceKHR`, which is ray tracing and out of scope
/// anyway. `video.xml` is nothing like that: **293 of its members are
/// `uint32_t x : 1`**, and every `StdVideo*Flags` type on the H.264 decode
/// surface is one of these and nothing else. Refusing them refuses video decode.
///
/// So the exception is budgeted rather than discovered, in §7.4's sense, and it
/// is deliberately the narrowest rule that admits the video types:
///
/// - every member is a bitfield, and
/// - every member's declared type is `uint32_t`, and
/// - the widths sum to at most 32.
///
/// A struct that mixes bitfields with ordinary members is still refused, so
/// `VkAccelerationStructureInstanceKHR` — bitfields *and* a
/// `VkTransformMatrixKHR` — stays out and its exclusion in
/// `abi::surface_profile` stays a live tripwire.
///
/// # What crosses, and why the bit order is not a wire question
///
/// The wire form is **one `u32`, copied**. Neither side ever takes the word
/// apart: the ICD reads the application's struct as a word and sends it; the GPU
/// side writes that word into the struct it hands the driver. So the protocol
/// does not depend on which end of the word `constraint_set0_flag` lives at —
/// only on the two C compilers agreeing, which they must anyway for the
/// application and the driver to agree on the same machine.
///
/// The generated *accessors* do depend on it. They assume the allocation order
/// every C ABI this project targets uses — first member at bit 0, ascending —
/// which is what the x86-64 System V ABI and MSVC on x64 both specify for
/// consecutive same-type bitfields. That assumption is checked rather than
/// asserted: `sv-testkit`'s video header cross-check reads the widths and order
/// out of `vk_video/*.h` independently, and `docs/video-decode.md` records that
/// the same layout, via bindgen, decoded byte-identically on RADV, NVIDIA and
/// the guest Intel driver — three drivers across both ABIs.
#[derive(Debug, Clone)]
pub struct FlagWordPlan {
    pub name: String,
    /// Always 32 today; carried so a future 16-bit flag word is a planner change
    /// rather than a silent widening.
    pub bit_width: u32,
    pub fields: Vec<BitField>,
}

#[derive(Debug, Clone)]
pub struct HandlePlan {
    pub name: String,
    pub dispatchable: bool,
    pub parent: Option<String>,
}

/// Everything the emitters need, with every decision already made.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub profile: String,
    pub commands: Vec<CommandPlan>,
    pub structs: Vec<StructPlan>,
    pub enums: Vec<EnumPlan>,
    pub bitmasks: Vec<BitmaskPlan>,
    /// C bitfield structs, which are `Scalar::Bitmask` on the wire and a
    /// `#[repr(transparent)]` `u32` newtype in Rust. See [`FlagWordPlan`].
    pub flag_words: Vec<FlagWordPlan>,
    pub handles: Vec<HandlePlan>,
    /// Type aliases in the profile: alias → target.
    pub type_aliases: BTreeMap<String, String>,
    /// Every struct that appears in some `pNext` chain, with its `sType`.
    pub pnext_links: Vec<(String, i64)>,
    pub exceptions: ConsultedExceptions,
}

impl Plan {
    pub fn struct_plan(&self, name: &str) -> Option<&StructPlan> {
        self.structs.iter().find(|s| s.name == name)
    }
}

/// Build the plan for a resolved profile.
pub fn build(model: &Model, profile: &ResolvedProfile) -> Result<Plan> {
    let mut plan = Plan { profile: profile.name.clone(), ..Default::default() };
    let mut exceptions = ConsultedExceptions::default();

    // -- types -----------------------------------------------------------
    for name in &profile.types {
        if let Some(reason) = exceptions::unserialisable_reason(name) {
            exceptions.note_unserialisable(name, reason);
            continue;
        }
        let Some(def) = model.types.get(name) else { continue };
        match def {
            TypeDef::Enum(e) => {
                // `*FlagBits` enums are emitted as types *as well as* being
                // folded into their `*Flags` bitmask's constants. The first
                // attempt skipped them, on the theory that a flags value should
                // have one type rather than two — and the generated crate did
                // not compile, because the registry declares plenty of members
                // with the `FlagBits` type directly (`VkSampleCountFlagBits
                // rasterizationSamples`, `VkResolveModeFlagBits resolveMode`).
                // Both spellings are real and both get a type.
                plan.enums.push(EnumPlan {
                    name: name.clone(),
                    bit_width: e.bit_width,
                    values: enum_values(model, e),
                });
            }
            TypeDef::Bitmask(b) => {
                let values = match b.bit_enum.as_ref().and_then(|n| model.type_def(n)) {
                    Some(TypeDef::Enum(e)) => enum_values(model, e),
                    _ => Vec::new(),
                };
                plan.bitmasks.push(BitmaskPlan {
                    name: name.clone(),
                    bit_width: b.bit_width,
                    values,
                });
            }
            TypeDef::Handle(h) => plan.handles.push(HandlePlan {
                name: name.clone(),
                dispatchable: h.dispatchable,
                parent: h.parent.clone(),
            }),
            TypeDef::Struct(s) => {
                if let Some(fw) = flag_word_of(model, s)? {
                    plan.flag_words.push(fw);
                    continue;
                }
                let mut fields = Vec::new();
                let member_names: Vec<String> =
                    s.members.iter().map(|m| m.name.clone()).collect();
                let lengths = length_members(&s.members);
                for m in &s.members {
                    if let Some(bits) = m.bitfield_bits {
                        bail!(
                            "{name}.{}: a {bits}-bit bitfield in a struct that is not a flag \
                             word. The planner carries a struct whose members are *all* \
                             `uint32_t` bitfields summing to at most 32 bits, as one `u32` \
                             (see plan::FlagWordPlan); a struct that mixes bitfields with \
                             ordinary members has no layout the registry states and none this \
                             generator will invent (docs/design.md §7.4). Exclude the type in the \
                             profile if this is intentional.",
                            m.name
                        );
                    }
                    if m.name == "sType" {
                        continue;
                    }
                    fields.push(field_plan(model, name, m, &lengths, &mut exceptions)?);
                }
                let _ = member_names;
                let s_type_value = s
                    .s_type
                    .as_ref()
                    .and_then(|c| lookup_enum_value(model, "VkStructureType", c));
                if s.s_type.is_some() && s_type_value.is_none() {
                    bail!(
                        "{name} declares sType {:?} but the registry has no such \
                         VkStructureType value — a wrong sType is a pNext link the peer will \
                         reject, so this is a build failure rather than a warning",
                        s.s_type
                    );
                }
                plan.structs.push(StructPlan {
                    name: name.clone(),
                    is_union: s.is_union,
                    fields,
                    s_type: s.s_type.clone(),
                    s_type_value,
                    has_p_next: s.has_p_next(),
                    returned_only: s.returned_only,
                    needs_partial: false,
                    has_handles: false,
                    has_device_memory: false,
                    extenders: Vec::new(),
                });
            }
            _ => {}
        }
    }

    // -- commands ---------------------------------------------------------
    let mut aliases_by_target: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (alias, target) in &profile.command_aliases {
        aliases_by_target.entry(target.as_str()).or_default().push(alias.clone());
    }

    for (index, name) in profile.commands.iter().enumerate() {
        let cmd = &model.commands[name];
        let lengths = length_members(&cmd.params);
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        for p in &cmd.params {
            // docs/design.md §7.4: `pAllocator` is stripped above the protocol.
            if p.ty.base_name() == Some("VkAllocationCallbacks") {
                exceptions.note_unserialisable(
                    "VkAllocationCallbacks",
                    exceptions::unserialisable_reason("VkAllocationCallbacks").unwrap(),
                );
                continue;
            }
            if let Some(VoidPolicy::NotSerialisable) =
                p.ty.base().is_void().then(|| exceptions::void_policy(name, &p.name)).flatten()
            {
                exceptions.note_void(name, &p.name, VoidPolicy::NotSerialisable);
                continue;
            }
            let f = field_plan(model, name, p, &lengths, &mut exceptions)?;
            if f.is_output {
                outputs.push(f);
            } else {
                inputs.push(f);
            }
        }

        let return_type = cmd.return_type.base_name().map(str::to_string);
        let returns_result = return_type.as_deref() == Some("VkResult");
        // **A reply exists whenever the command returns anything at all**, which
        // is docs/design.md §0.3 item 5's correction to this line and the hole it
        // named. `vkGetBufferDeviceAddress`, `vkGetBufferOpaqueCaptureAddress`
        // and `vkGetDeviceMemoryOpaqueCaptureAddress` return a
        // `VkDeviceAddress` with no output parameter and no `VkResult`, so the
        // rule as first written — `returns_result || !outputs.is_empty()` —
        // generated no reply for them, the emitter had nowhere to put the value,
        // and all three came out in `NOT_IMPLEMENTED`.
        //
        // §0.3 called that "latent rather than live" because §9.2 does not
        // advertise `bufferDeviceAddress`. §0.16 item 3 falsified it: the
        // feature is mirrored from the physical device, so `vulkaninfo` says
        // true, and CTS called all three on the first afternoon anyone asked.
        let returns_value = return_type.is_some() && !returns_result;
        // A `vkCmd*` recorded into a blob never generates a reply: its result is
        // the state of the command buffer, and docs/design.md §6.5 sends the whole
        // blob as one `svExecuteCommandBlob`.
        let is_recorded = cmd.is_command_buffer_recorded();
        let generates_reply =
            !is_recorded && (returns_result || returns_value || !outputs.is_empty());

        plan.commands.push(CommandPlan {
            name: name.clone(),
            opcode: index as u32,
            aliases: aliases_by_target.get(name.as_str()).cloned().unwrap_or_default(),
            inputs,
            outputs,
            returns_result,
            returns_value,
            return_type,
            generates_reply,
            is_recorded,
            origin: profile.command_origin.get(name).cloned().unwrap_or_default(),
        });
    }

    plan.type_aliases = profile.type_aliases.clone();
    plan.exceptions = exceptions;

    annotate_pnext(model, profile, &mut plan);
    annotate_reachability(&mut plan);
    annotate_partial(&mut plan);

    plan.enums.sort_by(|a, b| a.name.cmp(&b.name));
    plan.bitmasks.sort_by(|a, b| a.name.cmp(&b.name));
    plan.handles.sort_by(|a, b| a.name.cmp(&b.name));
    plan.structs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(plan)
}

/// Recognise a C bitfield struct, and lay its members out.
///
/// Returns `Ok(None)` for anything that is not one — the common case, and the
/// answer for every struct in `vk.xml` except `VkAccelerationStructureInstanceKHR`.
/// Returns an error only for the shapes that *look* like a flag word and are
/// not, because those are the ones where guessing would produce a plausible
/// wrong answer.
///
/// See [`FlagWordPlan`] for the rule and for what the bit order rests on.
pub fn flag_word_of(model: &Model, s: &vk_registry::StructType) -> Result<Option<FlagWordPlan>> {
    if s.is_union || s.members.is_empty() || !s.members.iter().all(|m| m.bitfield_bits.is_some()) {
        return Ok(None);
    }
    let mut fields = Vec::new();
    let mut offset = 0u32;
    for m in &s.members {
        let bits = m.bitfield_bits.unwrap_or(0);
        let base = m.ty.base_name().unwrap_or("");
        if model.canonical_type(base) != "uint32_t" || m.ty.indirection() != 0 {
            bail!(
                "{}.{}: a bitfield of {base}. The planner carries `uint32_t` bitfields \
                 only, because that is the only spelling `video.xml` uses and because a \
                 narrower storage unit changes both the wire width and the packing rule \
                 (plan::FlagWordPlan).",
                s.name,
                m.name
            );
        }
        if bits == 0 || offset + bits > 32 {
            bail!(
                "{}.{}: {bits} bits at offset {offset} does not fit one 32-bit storage \
                 unit. A C compiler would start a second word here and this generator \
                 would silently drop it.",
                s.name,
                m.name
            );
        }
        fields.push(BitField {
            name: m.name.clone(),
            rust_name: crate::naming::snake_case(&m.name),
            offset,
            bits,
        });
        offset += bits;
    }
    Ok(Some(FlagWordPlan { name: s.name.clone(), bit_width: 32, fields }))
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
                    // `codeSize / 4` names `codeSize`.
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

fn field_plan(
    model: &Model,
    owner: &str,
    m: &Member,
    lengths: &BTreeSet<String>,
    exceptions: &mut ConsultedExceptions,
) -> Result<FieldPlan> {
    let shape = shape_of(model, owner, m, 0, &m.ty, exceptions)?;
    Ok(FieldPlan {
        name: m.name.clone(),
        rust_name: crate::naming::snake_case(&m.name),
        shape,
        optional: m.is_optional(0),
        no_auto_validity: m.no_auto_validity,
        is_a_length: lengths.contains(&m.name),
        is_output: m.is_output_pointer(),
        selector: m.selector.clone(),
        selection: m.selection.clone(),
    })
}

/// The recursive core: a C type plus the registry's metadata becomes a wire
/// shape.
///
/// `level` is the indirection level being described, which is what indexes into
/// `len=` and `optional=`. Getting that indexing wrong is the single most
/// likely way to produce marshalling that is wrong on both sides at once, so it
/// is one parameter threaded explicitly rather than inferred.
fn shape_of(
    model: &Model,
    owner: &str,
    m: &Member,
    level: usize,
    ty: &CType,
    exceptions: &mut ConsultedExceptions,
) -> Result<Shape> {
    match &ty.kind {
        CTypeKind::Array(inner, extent) => {
            let count = model.extent_value(extent).ok_or_else(|| {
                anyhow::anyhow!(
                    "{owner}.{}: array extent {extent} does not resolve to a number",
                    m.name
                )
            })?;
            let elem = shape_of(model, owner, m, level, inner, exceptions)?;
            Ok(Shape::FixedArray { elem: Box::new(elem), count, extent: extent.clone() })
        }
        CTypeKind::Pointer(pointee) => {
            if m.name == "pNext" {
                return Ok(Shape::PNext);
            }
            let len_term = m.len.as_ref().and_then(|l| l.term(level)).cloned();

            // A `void*`: a blob if it has a length, otherwise a policy entry.
            if pointee.is_void() {
                return match &len_term {
                    Some(LenTerm::NullTerminated) => Ok(Shape::CString),
                    Some(t) => Ok(Shape::Blob { len: len_check(t) }),
                    None => {
                        let policy = exceptions::void_policy(owner, &m.name).ok_or_else(|| {
                            anyhow::anyhow!(
                                "{owner}.{}: a `void*` with no `len=` and no policy entry. \
                                 docs/design.md §7.4 requires every one of these to be a data entry \
                                 in crates/sv-codegen/src/exceptions.rs; add one, with the \
                                 reason.",
                                m.name
                            )
                        })?;
                        exceptions.note_void(owner, &m.name, policy);
                        if policy == VoidPolicy::NotSerialisable {
                            bail!(
                                "{owner}.{}: {:?} is not serialisable (docs/design.md §6.1 \
                                 exclusion list)",
                                m.name,
                                m.name
                            );
                        }
                        Ok(Shape::Scalar(Scalar::OpaqueU64(policy)))
                    }
                };
            }

            // `char*` with `len="null-terminated"`, and `const char* const*`.
            if pointee.base_name() == Some("char") && pointee.indirection() == 0 {
                return match &len_term {
                    Some(LenTerm::NullTerminated) | None => Ok(Shape::CString),
                    Some(t) => Ok(Shape::Blob { len: len_check(t) }),
                };
            }
            if pointee.indirection() == 1 && pointee.base_name() == Some("char") {
                return Ok(Shape::CStringArray { len: len_term.as_ref().and_then(len_check) });
            }

            let inner = shape_of(model, owner, m, level + 1, pointee, exceptions)?;
            match len_term {
                // No `len=` at this level means a pointer to exactly one value.
                None => Ok(Shape::Optional(Box::new(inner))),
                Some(LenTerm::Literal(1)) => Ok(Shape::Optional(Box::new(inner))),
                Some(LenTerm::NullTerminated) => Ok(Shape::CString),
                Some(t) => Ok(Shape::Array { elem: Box::new(inner), len: len_check(&t) }),
            }
        }
        CTypeKind::Void => bail!("{owner}.{}: a bare `void` is not a value", m.name),
        CTypeKind::Named(name) => Ok(Shape::Scalar(scalar_of(model, owner, &m.name, name)?)),
    }
}

fn len_check(term: &LenTerm) -> Option<LenCheck> {
    match term {
        LenTerm::Sibling(s) => Some(LenCheck::Sibling(s.clone())),
        LenTerm::Path { root, field } => {
            Some(LenCheck::Path { root: root.clone(), field: field.clone() })
        }
        LenTerm::Expr(e) => Some(parse_len_expr(e)),
        LenTerm::Literal(_) | LenTerm::NullTerminated => None,
    }
}

/// The four `altlen=` expressions in the whole registry are
/// `codeSize / 4`, `(samples + 31) / 32`, `(rasterizationSamples + 31) / 32`
/// and `2*VK_UUID_SIZE`. Only the first is a length we can check against a
/// sibling; the rest are recorded unchecked rather than mis-parsed.
fn parse_len_expr(e: &str) -> LenCheck {
    let compact: String = e.chars().filter(|c| !c.is_whitespace()).collect();
    if let Some((lhs, rhs)) = compact.split_once('/')
        && lhs.chars().all(|c| c.is_alphanumeric() || c == '_')
            && let Ok(divisor) = rhs.parse::<u64>() {
                return LenCheck::SiblingDiv { sibling: lhs.to_string(), divisor };
            }
    LenCheck::Unchecked(e.to_string())
}

fn scalar_of(model: &Model, owner: &str, field: &str, name: &str) -> Result<Scalar> {
    let canonical = model.canonical_type(name);
    let Some(def) = model.types.get(&canonical) else {
        bail!("{owner}.{field}: type {name} is not in the registry");
    };
    Ok(match def {
        TypeDef::Primitive(p) => match p {
            Primitive::Void => bail!("{owner}.{field}: a bare `void` is not a value"),
            Primitive::Char | Primitive::U8 => Scalar::U8,
            Primitive::I8 => Scalar::I8,
            Primitive::U16 => Scalar::U16,
            Primitive::I16 => Scalar::I16,
            Primitive::U32 => Scalar::U32,
            Primitive::I32 => Scalar::I32,
            Primitive::U64 => Scalar::U64,
            Primitive::I64 => Scalar::I64,
            Primitive::F32 => Scalar::F32,
            Primitive::F64 => Scalar::F64,
            Primitive::Size => Scalar::Size,
        },
        TypeDef::BaseType { name: n, underlying } => {
            if n == "VkBool32" {
                Scalar::Bool32
            } else {
                match underlying {
                    Some(u) => {
                        let base = u.base_name().unwrap_or("uint32_t");
                        scalar_of(model, owner, field, base)?
                    }
                    None => bail!(
                        "{owner}.{field}: {n} is an opaque platform type and is not \
                         serialisable (docs/design.md §6.1)"
                    ),
                }
            }
        }
        TypeDef::Handle(h) => Scalar::Handle(h.name.clone()),
        TypeDef::Enum(e) => {
            if e.bit_width == 64 {
                Scalar::Bitmask64(e.name.clone())
            } else {
                Scalar::Enum(e.name.clone())
            }
        }
        TypeDef::Bitmask(b) => {
            if b.bit_width == 64 {
                Scalar::Bitmask64(b.name.clone())
            } else {
                Scalar::Bitmask(b.name.clone())
            }
        }
        TypeDef::Struct(s) => {
            if s.is_union {
                Scalar::Union(s.name.clone())
            } else if flag_word_of(model, s)?.is_some() {
                // A flag word is one `u32` on the wire, which is what
                // `Scalar::Bitmask` already means. Reusing it rather than adding
                // a variant keeps the encoders, decoders, samplers and the
                // manifest renderer untouched — the wire really is identical,
                // and the difference is only in how the *type* is declared.
                Scalar::Bitmask(s.name.clone())
            } else {
                Scalar::Struct(s.name.clone())
            }
        }
        TypeDef::FuncPointer(f) => bail!(
            "{owner}.{field}: {} is a function pointer, which is not serialisable \
             (docs/design.md §6.1, Venus's exclusion list)",
            f.name
        ),
        TypeDef::External { name: n, .. } => bail!(
            "{owner}.{field}: {n} comes from a platform header and is not serialisable \
             (docs/design.md §6.1)"
        ),
    })
}

fn enum_values(model: &Model, e: &vk_registry::EnumType) -> Vec<(String, i64)> {
    let direct: BTreeMap<&str, i64> =
        e.values.iter().filter(|v| v.alias_of.is_none()).map(|v| (v.name.as_str(), v.value)).collect();
    let mut out: BTreeMap<String, i64> =
        direct.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    // Aliases become ordinary constants with the target's value, so that a name
    // the application may legitimately use resolves without a second table.
    //
    // The chain is followed rather than stepped once, because the registry has
    // aliases *of* aliases: `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VARIABLE_POINTER_FEATURES_KHR`
    // names `…_VARIABLE_POINTERS_FEATURES_KHR`, which in turn names the 1.1 core
    // `…_VARIABLE_POINTERS_FEATURES`. Resolving one hop dropped that name from
    // the generated table silently — a constant an application may legitimately
    // pass, missing, with nothing to notice it. Found by the `vulkan_core.h`
    // cross-check (2026-08-12), which is R8's rule doing its job: only an
    // external source says what we *should* have emitted.
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
    let _ = model;
    out.into_iter().collect()
}

fn lookup_enum_value(model: &Model, enum_name: &str, constant: &str) -> Option<i64> {
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

/// Fill in `extenders` and the global link table.
fn annotate_pnext(model: &Model, profile: &ResolvedProfile, plan: &mut Plan) {
    let in_plan: BTreeSet<&str> = plan.structs.iter().map(|s| s.name.as_str()).collect();
    let mut links: BTreeMap<String, i64> = BTreeMap::new();

    let mut extenders_by_parent: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (parent, exts) in &model.extended_by {
        if !profile.has_type(parent) {
            continue;
        }
        let mut kept: Vec<String> = exts
            .iter()
            .filter(|e| in_plan.contains(e.as_str()) && profile.has_type(e))
            .cloned()
            .collect();
        kept.sort();
        if !kept.is_empty() {
            for e in &kept {
                if let Some(sp) = plan.structs.iter().find(|s| &s.name == e)
                    && let Some(v) = sp.s_type_value {
                        links.insert(e.clone(), v);
                    }
            }
            extenders_by_parent.insert(parent.clone(), kept);
        }
    }
    for s in &mut plan.structs {
        if let Some(e) = extenders_by_parent.remove(&s.name) {
            s.extenders = e;
        }
    }
    plan.pnext_links = links.into_iter().collect();
}

/// Fixpoint over "contains a handle" and "contains a `VkDeviceMemory`".
///
/// A fixpoint rather than a single pass because struct graphs are deep and, with
/// `pNext`, cyclic in the type graph if not in any value. Passes 5 and 6 of
/// docs/design.md §7.2 are emitted only for types that can actually contain
/// something to rewrite, which is what keeps them from being two more full
/// walks over 700 structs.
fn annotate_reachability(plan: &mut Plan) {
    let index: BTreeMap<String, usize> =
        plan.structs.iter().enumerate().map(|(i, s)| (s.name.clone(), i)).collect();

    // Direct evidence first.
    let mut handles: Vec<bool> = Vec::with_capacity(plan.structs.len());
    let mut memory: Vec<bool> = Vec::with_capacity(plan.structs.len());
    for s in &plan.structs {
        let mut names = BTreeSet::new();
        for f in &s.fields {
            f.shape.mentions(&mut names);
        }
        handles.push(s.fields.iter().any(|f| shape_has_handle(&f.shape)));
        memory.push(names.contains("VkDeviceMemory"));
    }

    loop {
        let mut changed = false;
        for (i, s) in plan.structs.iter().enumerate() {
            let mut names = BTreeSet::new();
            for f in &s.fields {
                f.shape.mentions(&mut names);
            }
            // A struct's chain can carry any extender, so a chain member
            // inherits every extender's properties. Missing that is how a
            // `(memory, offset)` pair hidden in a `pNext` extender escapes the
            // transform pass — the exact failure docs/design.md §5.3 warns about.
            let reachable: Vec<&String> = names.iter().chain(s.extenders.iter()).collect();
            for n in reachable {
                let Some(&j) = index.get(n.as_str()) else { continue };
                if handles[j] && !handles[i] {
                    handles[i] = true;
                    changed = true;
                }
                if memory[j] && !memory[i] {
                    memory[i] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    for (i, s) in plan.structs.iter_mut().enumerate() {
        s.has_handles = handles[i];
        s.has_device_memory = memory[i];
    }
}

fn shape_has_handle(shape: &Shape) -> bool {
    match shape {
        Shape::Scalar(Scalar::Handle(_)) => true,
        Shape::Scalar(_) => false,
        Shape::FixedArray { elem, .. } | Shape::Array { elem, .. } | Shape::Optional(elem) => {
            shape_has_handle(elem)
        }
        _ => false,
    }
}

/// Decide which structs need a `_partial` encoding.
///
/// Venus's rule, taken verbatim (docs/design.md §6.1 adopts its three-state output
/// validity): a struct that is `returnedonly` and appears as an output is
/// encoded partially on the way *to* the GPU side, because its contents are
/// whatever the caller's uninitialised memory held. Any struct reachable from
/// one of those, by value or through a chain, needs the partial form too.
fn annotate_partial(plan: &mut Plan) {
    let index: BTreeMap<String, usize> =
        plan.structs.iter().enumerate().map(|(i, s)| (s.name.clone(), i)).collect();

    let mut needs: Vec<bool> = plan.structs.iter().map(|s| s.returned_only).collect();

    // Anything a command names as an output also needs it, even if the registry
    // does not mark the struct `returnedonly`.
    for c in &plan.commands {
        for f in &c.outputs {
            let mut names = BTreeSet::new();
            f.shape.mentions(&mut names);
            for n in names {
                if let Some(&i) = index.get(&n) {
                    needs[i] = true;
                }
            }
        }
    }

    loop {
        let mut changed = false;
        for (i, s) in plan.structs.iter().enumerate() {
            if !needs[i] {
                continue;
            }
            let mut names = BTreeSet::new();
            for f in &s.fields {
                f.shape.mentions(&mut names);
            }
            for n in names.iter().chain(s.extenders.iter()) {
                let Some(&j) = index.get(n.as_str()) else { continue };
                if !needs[j] {
                    needs[j] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    for (i, s) in plan.structs.iter_mut().enumerate() {
        s.needs_partial = needs[i];
    }
}

/// Whether a member of a partially-encoded struct is itself encoded.
///
/// Venus's `_get_variable_validity`, reimplemented:
///
/// - a member that is *the length of* another member is fully encoded, because
///   the caller supplies it — this is the count-then-fill idiom;
/// - a handle or a struct is *partially* encoded, because the caller supplies
///   the id (ours) or the nested `sType`/`pNext`;
/// - everything else is not encoded at all, because its value is uninitialised
///   memory and sending it would be both wasteful and a small information leak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialValidity {
    Full,
    Partial,
    Skip,
}

pub fn partial_validity(field: &FieldPlan) -> PartialValidity {
    if field.is_a_length {
        return PartialValidity::Full;
    }
    match &field.shape {
        Shape::PNext => PartialValidity::Partial,
        Shape::Scalar(Scalar::Handle(_)) => PartialValidity::Full,
        Shape::Scalar(Scalar::Struct(_)) | Shape::Scalar(Scalar::Union(_)) => {
            PartialValidity::Partial
        }
        Shape::Optional(inner) | Shape::Array { elem: inner, .. } => {
            match partial_validity(&FieldPlan { shape: (**inner).clone(), ..field.clone() }) {
                PartialValidity::Skip => PartialValidity::Skip,
                _ => PartialValidity::Partial,
            }
        }
        Shape::FixedArray { elem, .. } => {
            match partial_validity(&FieldPlan { shape: (**elem).clone(), ..field.clone() }) {
                PartialValidity::Skip => PartialValidity::Skip,
                _ => PartialValidity::Partial,
            }
        }
        _ => PartialValidity::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_wire_sizes_match_venus() {
        assert_eq!(Scalar::U8.wire_size(), Some(4));
        assert_eq!(Scalar::U16.wire_size(), Some(4));
        assert_eq!(Scalar::U32.wire_size(), Some(4));
        assert_eq!(Scalar::U64.wire_size(), Some(8));
        assert_eq!(Scalar::Size.wire_size(), Some(8));
        assert_eq!(Scalar::F64.wire_size(), Some(8));
        assert_eq!(Scalar::Handle("VkBuffer".into()).wire_size(), Some(8));
        assert_eq!(Scalar::Enum("VkFormat".into()).wire_size(), Some(4));
        assert_eq!(Scalar::Bitmask64("VkPipelineStageFlags2".into()).wire_size(), Some(8));
        assert_eq!(Scalar::Struct("VkExtent2D".into()).wire_size(), None);
    }

    /// An alias of an alias still names a number.
    ///
    /// The registry has them: `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VARIABLE_POINTER_FEATURES_KHR`
    /// names `…_VARIABLE_POINTERS_FEATURES_KHR`, which names the 1.1 core
    /// `…_VARIABLE_POINTERS_FEATURES`. Stepping the chain once dropped the name
    /// from the generated table without a word — found by the `vulkan_core.h`
    /// cross-check, which is the only thing that could have: nothing internal
    /// knows what we *failed* to emit.
    #[test]
    fn an_alias_of_an_alias_resolves() {
        use vk_registry::{EnumType, EnumValue};
        let v = |name: &str, value: i64, alias: Option<&str>| EnumValue {
            name: name.into(),
            value,
            provenance: None,
            alias_of: alias.map(str::to_string),
            deprecated: false,
        };
        let e = EnumType {
            name: "VkStructureType".into(),
            bit_width: 32,
            values: vec![
                v("VK_ONE", 1_000_120_000, None),
                v("VK_ONE_KHR", 0, Some("VK_ONE")),
                v("VK_LEGACY_KHR", 0, Some("VK_ONE_KHR")),
            ],
        };
        let out: BTreeMap<String, i64> = enum_values(&Model::default(), &e).into_iter().collect();
        assert_eq!(out.get("VK_ONE"), Some(&1_000_120_000));
        assert_eq!(out.get("VK_ONE_KHR"), Some(&1_000_120_000));
        assert_eq!(
            out.get("VK_LEGACY_KHR"),
            Some(&1_000_120_000),
            "an alias whose target is itself an alias must still resolve"
        );
    }

    #[test]
    fn altlen_expressions_become_checks_where_they_can() {
        assert_eq!(
            parse_len_expr("codeSize / 4"),
            LenCheck::SiblingDiv { sibling: "codeSize".into(), divisor: 4 }
        );
        // The registry's other three are recorded rather than guessed at.
        assert!(matches!(parse_len_expr("(samples + 31) / 32"), LenCheck::Unchecked(_)));
        assert!(matches!(parse_len_expr("2*VK_UUID_SIZE"), LenCheck::Unchecked(_)));
    }
}
