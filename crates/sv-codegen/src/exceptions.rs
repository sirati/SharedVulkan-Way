//! The exception table: everything `vk.xml` cannot express, budgeted rather
//! than discovered.
//!
//! docs/design.md §7.4 is the specification for this file, and its closing sentence
//! is the rule it enforces: "Every exception is a data entry in one file, and an
//! unlisted case is a build failure. That is the difference between a generator
//! that is honest about its limits and one that emits plausible-looking wrong
//! code."
//!
//! # What §7.4 budgeted for, and what actually turned out to be needed
//!
//! Three of the six budgeted exceptions dissolved, because they are artefacts of
//! generating C rather than Rust. They are recorded here anyway — as notes, not
//! as tables — because "we did not need this" is a finding, and a future
//! reviewer comparing against the design deserves to know why the table is
//! shorter than the design expected.
//!
//! - **Unions.** §7.4 expected to hardcode discriminators and error on the rest,
//!   because C has no tag. Venus's generator carries a `UNION_DEFAULT_TAGS`
//!   table for exactly this. A Rust `enum` *is* the tag, so an encoded union
//!   always knows which member it holds and no default is ever guessed. The
//!   `selector=` attribute is still recorded in the model for validity checks,
//!   and a union member the generator cannot type is still a build failure — but
//!   there is nothing to hardcode.
//! - **Conditional presence** (`pTessellationState` depending on
//!   `rasterizerDiscardEnable` in a sibling struct). §7.4 proposed a table of
//!   Rust closures keyed by `(struct, field)`. Not needed for marshalling: the
//!   field is `Option<Box<…>>` and its presence is a fact of the value, not
//!   something to be recomputed. The condition matters when *lowering from the
//!   application's C structs*, which is `sv-icd-entry`'s problem, and the table
//!   belongs there when that crate is written.
//! - **`len=` vs `altlen=`.** Handled in `vk-registry` where it belongs, and
//!   cheaper than expected: every variable-length thing on our wire carries its
//!   own explicit `u64` count, so `len=` is a *cross-check* rather than a
//!   load-bearing input. A wrong `len=` in the registry therefore cannot
//!   desynchronise the stream — it can only make the generated consistency
//!   check fire. That is a direct and unplanned mitigation of R8.
//!
//! The three that survive are below.

use std::collections::BTreeMap;

/// What to do with a `void*` that is not `pNext` and has no `len=`.
///
/// docs/design.md §7.4 counts 103 of these registry-wide. Those with a `len` are
/// blobs and need no entry. The rest are listed here by `(owner, field)`, and an
/// unlisted one is a generator error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VoidPolicy {
    /// An offset into the passthrough arena.
    ///
    /// `pHostPointer` was meant to be the interesting one (§7.4): a host pointer
    /// taken to be arena memory, crossing as a `u64` offset for the GPU side to
    /// resolve against its own mapping of the same pages. No application passes
    /// arena memory there, so `VK_EXT_external_memory_host` is withdrawn instead
    /// (docs/video-decode.md §7.13.5, [`ICD_EXPANDS_ARENA_POINTER`]).
    ArenaOffset,
    /// An opaque token the app side owns and the GPU side never dereferences.
    ///
    /// `pUserData` is the canonical case: it is passed back to an app-side
    /// callback and has no meaning on the other machine. It crosses as an
    /// opaque `u64` so the app side can round-trip it, and the GPU side treats
    /// it as a number.
    OpaqueToken,
    /// A capture-replay token: null on the wire, and a non-null one is refused
    /// by name.
    ///
    /// # The class, and why it needed a policy rather than an exclusion
    ///
    /// `pShaderGroupCaptureReplayHandle` and `opaqueCaptureDescriptorData` are
    /// optional `void*` whose extent is a **device property** —
    /// `shaderGroupHandleCaptureReplaySize`,
    /// `…CaptureReplayDescriptorDataSize` — rather than a `len=`. §7.4 asks for
    /// a policy and neither `ArenaOffset` nor `OpaqueToken` is honest: the bytes
    /// are real data of a length the registry does not state.
    ///
    /// But they are only ever written by a tool *replaying a trace*, and doing
    /// that requires the device to advertise the matching feature —
    /// `rayTracingPipelineShaderGroupHandleCaptureReplay`,
    /// `descriptorBufferCaptureReplay`. So the specification already requires
    /// the member to be null, and the honest encoding is "null, and say so
    /// loudly if it is not" — which is what §0.19 item 4 predicted the answer
    /// would be and what §0.24 implemented.
    ///
    /// **This paragraph used to end "This driver advertises neither, and
    /// `sv_icd::advertise` is where that is kept", and it was not kept
    /// anywhere.** Both bits arrive inside feature structs forwarded whole from
    /// the physical device, and `withdraw_gated_features` cleared exactly one
    /// bit — `hostImageCopy` — so on a driver that supports capture-replay we
    /// advertised it and would have refused the first token an application
    /// legally passed. `sv_icd::advertise::withdraw_gated_features` clears both
    /// now, `no_capture_replay_feature_whose_token_is_a_void_pointer_is_advertised`
    /// checks it, and `crates/sv-codegen/tests/void_star.rs` enumerates the
    /// class this policy is applied to so a new member cannot join it unnoticed
    /// (docs/design.md §0.25).
    ///
    /// This is what took `VK_KHR_ray_tracing_pipeline` (§0.24) and
    /// `VK_EXT_descriptor_buffer` (§0.25) off the profile's exclusion list.
    CaptureReplayUnsupported,
    /// Not serialisable at all; a command or struct reaching one is a build
    /// failure.
    ///
    /// Venus's exclusion list, taken verbatim in spirit (docs/design.md §6.1):
    /// function pointers, pointers to mapped memory, WSI handles and external
    /// handles are not serialisable.
    NotSerialisable,
}

/// `(type or command name, member or parameter name)` → policy.
pub fn void_policy(owner: &str, field: &str) -> Option<VoidPolicy> {
    VOID_POLICIES
        .iter()
        .find(|(o, f, _)| *o == owner && *f == field)
        .map(|(_, _, p)| *p)
        // A `pUserData` is a `pUserData` wherever it occurs, and listing all
        // nineteen of them by owner would be noise that hides the two entries
        // that actually carry a decision.
        .or_else(|| BY_FIELD_NAME.iter().find(|(f, _)| *f == field).map(|(_, p)| *p))
        .or_else(|| is_capture_replay(owner, field).then_some(VoidPolicy::CaptureReplayUnsupported))
}

/// Commands whose `VoidPolicy::ArenaOffset` parameter `sv-icd` expands *before*
/// anything crosses.
///
/// # The third class of over-promise, and the wild pointer that named it
///
/// `ArenaOffset` means "this `void*` is `arena_base + offset`, and the GPU side
/// resolves it against its own mapping of the same pages" (docs/design.md §5.3). That
/// is true of `pHostPointer`, where the application's pointer really is arena
/// memory. It is **not** true of a descriptor-template `pData`, which points at
/// ordinary application memory laid out by the template — and for those the
/// generated lowering emits `(p_data) as usize as u64`, handing the
/// application's own pointer over as if it were an offset.
///
/// That is only safe when something app-side turns the data into wire structs
/// first. `sv_icd::icd` overrides the generated method for
/// `vkUpdateDescriptorSetWithTemplate` and does exactly that (§0.4 item 10). For
/// `vkCmdPushDescriptorSetWithTemplate` nothing did, the raw pointer crossed,
/// and the GPU side dereferenced it — a segfault, and the leading suspect for
/// "Firefox crashes instantly" after `full` became the default profile.
///
/// **This list is what makes that checkable rather than remembered.** A command
/// with an `ArenaOffset` `void*` *parameter* and no entry here cannot lower
/// safely, so [`crate::emit::icd_entry`] puts its extension into
/// `INCOMPLETE_EXTENSIONS` — and fails the build outright if the command is
/// core, because a core command cannot be un-advertised. §0.20 item 4 predicted
/// that a mechanical widening would restore this defect and it did; a list of
/// excluded extensions is exactly what a mechanical widening reverses, and a
/// derived check is not.
///
/// Struct *members* are not covered and do not need to be: a `pNext` chain is
/// expanded by the same override. `VkImportMemoryHostPointerInfoEXT`'s
/// `pHostPointer` is a member too, and is not reached because its extension is
/// withdrawn with `vkGetMemoryHostPointerPropertiesEXT` (docs/video-decode.md
/// §7.13.5).
pub const ICD_EXPANDS_ARENA_POINTER: &[&str] = &[
    // `sv_icd::icd::update_descriptor_set_with_template` reads `pData` through
    // the template and emits `VkWriteDescriptorSet`s; the pointer never crosses.
    "vkUpdateDescriptorSetWithTemplate",
    "vkUpdateDescriptorSetWithTemplateKHR",
    "vkCmdPushDescriptorSetWithTemplate",
    "vkCmdPushDescriptorSetWithTemplateKHR",
    // The push forms, expanded the same way as of docs/design.md §0.22:
    // `sv_icd::icd::cmd_push_descriptor_set_with_template` turns `pData` into
    // `VkWriteDescriptorSet`s and forwards `vkCmdPushDescriptorSet`, so the
    // application's pointer never crosses.
    // **The `2` forms are here as documentation, not as suppression, and the
    // difference matters.** `LOWERING_INCOMPLETE` scans a command's *parameters*
    // for a `void*` with `ArenaOffset`, and these two carry `pData` inside
    // `VkPushDescriptorSetWithTemplateInfo` — a struct member — so the check
    // never sees them and removing these lines would change nothing it does.
    //
    // They are listed because a reader who finds the `2` forms carried and asks
    // "is this one safe too?" deserves the answer where the answers live, and it
    // is yes for a reason that has been measured rather than assumed:
    // `sv_icd::icd::cmd_push_descriptor_set_with_template2` unwraps the info
    // struct onto the same helper, so the pointer is expanded app-side before
    // anything crosses, and `dEQP-VK.binding_model.shader_access.*.bind2.
    // with_push_template.*` — which is the CTS group that calls the `2` entry
    // points — passes through the forwarder.
    //
    // The blind spot is real and is recorded in §0.22: a `void*` policy on a
    // struct member is invisible to this guard. Closing it means walking struct
    // members transitively, which is a change to the check rather than to this
    // list.
    "vkCmdPushDescriptorSetWithTemplate2",
    "vkCmdPushDescriptorSetWithTemplate2KHR",
    // **`vkGetMemoryHostPointerPropertiesEXT` is no longer listed, and that is
    // what withdraws `VK_EXT_external_memory_host`** (docs/video-decode.md
    // §7.13.5). It was here as "safe for the other reason": the extension's
    // pointer was taken to be arena memory, so `arena_base + offset` would be
    // what it is. That was never true of the application that uses it. ffmpeg's
    // `av_hwframe_transfer_data`, which is also Firefox's readback, imports its
    // own `malloc`ed frame as a host-pointer buffer and copies the decoded
    // picture into it. The ICD sent that pointer raw, the GPU side handed the
    // driver NULL, RADV refused NULL (so ffmpeg fell back to a staging buffer,
    // and every local proof passed), and Intel's Windows driver accepted it: the
    // copy went into memory the application never sees, and every hardware-
    // decoded frame on the guest read back as zeros, the green picture of
    // §7.13.5. Honouring a non-arena pointer needs a bounce through the arena
    // that nothing implements, so the extension is not promised.
];

/// Commands whose `VoidPolicy::NotSerialisable` **parameter** is answered on the
/// app side, and so may be dropped from the wire rather than refused.
///
/// # Why a policy needed a second list, again
///
/// `NotSerialisable` does two different things depending on where it lands. On a
/// struct member it is a refusal: [`crate::plan`] bails, the build fails, and
/// `Profile::excluded_extensions` is where the decision to live without the
/// extension gets written down. On a command *parameter* it is a **deletion** —
/// the parameter is dropped and the rest of the command is carried as if it had
/// never had one.
///
/// For `vkMapMemory` that is exactly right and is the whole design: `ppData` is
/// a pointer into our own mapping, computed app-side from `arena_base + offset`
/// (docs/design.md §5.3), and `sv_icd::icd` writes it before the command is
/// forwarded without it. Nothing is lost because nothing had to cross.
///
/// For `vkGetDescriptorSetHostMappingVALVE` the identical entry would produce a
/// command that is forwarded, returns, and never writes the one output the
/// caller asked for — the same shape of defect as §0.22's `ArenaOffset` on
/// `vkCmdPushDescriptorSetWithTemplate`, where a policy that named a treatment
/// nobody had implemented was taken as evidence that it had been. **A declared
/// policy is not an implementation**, and the only way to keep saying that is to
/// make the claim checkable: a command with a dropped `void*` parameter and no
/// entry here cannot be carried, and [`crate::plan`] refuses it by name.
///
/// An entry is therefore a statement about `sv-icd`, not about the registry —
/// "this parameter's value is produced here" — and it names the function that
/// produces it so the claim can be followed.
pub const PARAMETER_ANSWERED_APP_SIDE: &[(&str, &str)] = &[
    ("vkMapMemory", "sv_icd::icd::map_memory computes arena_base + offset and writes *ppData"),
    ("vkMapMemory2", "as vkMapMemory, through the same helper"),
    ("vkMapMemory2KHR", "as vkMapMemory, through the same helper"),
];

/// Why a command may drop a `NotSerialisable` `void*` parameter, if it may.
pub fn parameter_answered_app_side(command: &str) -> Option<&'static str> {
    PARAMETER_ANSWERED_APP_SIDE.iter().find(|(c, _)| *c == command).map(|(_, why)| *why)
}

/// Whether a `void*` belongs to the capture-replay family, by the registry's own
/// naming.
///
/// # Why a rule and not a list, after starting as a list
///
/// The list was `VkRayTracingShaderGroupCreateInfoKHR`,
/// `VkOpaqueCaptureDescriptorDataCreateInfoEXT` and the five
/// `vkGet…OpaqueCaptureDescriptorDataEXT` queries. Adding them made the profile
/// fail on `vkGetTensorOpaqueCaptureDescriptorDataARM`, which is the identical
/// shape from `VK_ARM_tensors` — and the next vendor to copy the pattern would
/// have failed it again. A list that grows once per vendor for a reason that
/// never changes is a rule that has not been written down.
///
/// Khronos names this family consistently, which is what makes the rule safe:
/// every member is `…CaptureReplay…` or `…OpaqueCapture…` in its own name or its
/// owner's. It is also *narrow* — [`void_policy`] is only consulted for a
/// `void*` with no `len=`, so `vkGetBufferOpaqueCaptureAddress`, which returns a
/// `uint64_t` and has no `void*` at all, is not reached by it.
///
/// A name that matches but is not capture-replay would be given a policy of
/// "null, and refuse a non-null one", which fails loudly at the call rather than
/// silently. That is the safe direction for a heuristic to be wrong in.
fn is_capture_replay(owner: &str, field: &str) -> bool {
    let hit = |s: &str| s.contains("CaptureReplay") || s.contains("OpaqueCapture");
    hit(owner) || hit(field)
}

/// The per-`(owner, field)` half of the table; [`BY_FIELD_NAME`] is the other.
///
/// Public so that `tests/void_star.rs` can check every entry against the
/// registry: an entry naming a member that no longer exists is a rule still
/// being applied to a question nobody asks.
pub const VOID_POLICIES: &[(&str, &str, VoidPolicy)] = &[
    // docs/design.md §5.3: an app-side `vkMapMemory` pointer is arena_base + offset,
    // and the GPU side never maps arena memory through the driver at all. The
    // import path therefore carries an offset.
    ("VkImportMemoryHostPointerInfoEXT", "pHostPointer", VoidPolicy::ArenaOffset),
    ("vkGetMemoryHostPointerPropertiesEXT", "pHostPointer", VoidPolicy::ArenaOffset),
    // `vkMapMemory`'s out-parameter is a pointer into our own mapping. It is
    // computed app-side and never crosses (docs/design.md §5.3: "no round trip and
    // no copy"), so the command is forwarded without it.
    ("vkMapMemory", "ppData", VoidPolicy::NotSerialisable),
    // `vkMapMemory2` is core 1.4's promotion of `VK_KHR_map_memory2` and its
    // out-parameter is `vkMapMemory`'s, unchanged: the same pointer into our own
    // mapping, computed app-side, never crossing. Same policy, and it is here by
    // owner rather than by field name because `ppData` elsewhere — the
    // descriptor-set host mapping, for one — is a different question.
    ("vkMapMemory2", "ppData", VoidPolicy::NotSerialisable),
    ("vkMapMemory2KHR", "ppData", VoidPolicy::NotSerialisable),
    // Explicitly out of scope: a window handle is a WSI handle.
    ("VkAndroidSurfaceCreateInfoKHR", "window", VoidPolicy::NotSerialisable),
    // The other three window handles the registry spells as a bare `void*`
    // rather than as a platform type. `VkAndroidSurfaceCreateInfoKHR::window`
    // is an `ANativeWindow*` and refuses one line above because the *type* is
    // opaque; these three are the identical thing wearing no type at all, and
    // without an entry they refuse for the uninformative reason ("no policy")
    // instead of the true one. All four are WSI handles, which is the first
    // line of Venus's exclusion list (docs/design.md §6.1), and all four belong to
    // `platform=` extensions no profile here can reach.
    ("VkViSurfaceCreateInfoNN", "window", VoidPolicy::NotSerialisable),
    ("VkIOSSurfaceCreateInfoMVK", "pView", VoidPolicy::NotSerialisable),
    ("VkMacOSSurfaceCreateInfoMVK", "pView", VoidPolicy::NotSerialisable),
    // **The CUDA argument vectors: a `void*` with no extent in any document.**
    //
    // Both structs give the *array* a length — `len="paramCount"`, and
    // `len="extraCount"` — so what refuses is the element: one `const void*`
    // per kernel argument, whose size is stated by the kernel's signature
    // inside the CUDA module. The registry cannot see that, this side does not
    // parse it, and the application is not required to know it either; the
    // driver reads the arguments using the module's own metadata. So there is
    // no length to be found rather than one we have not found, and that is the
    // difference between this group and the four in
    // `Profile::excluded_extensions` whose extent is real but is stated
    // somewhere `len=` cannot name (docs/design.md §0.25).
    ("VkCuLaunchInfoNVX", "pParams", VoidPolicy::NotSerialisable),
    ("VkCuLaunchInfoNVX", "pExtras", VoidPolicy::NotSerialisable),
    ("VkCudaLaunchInfoNV", "pParams", VoidPolicy::NotSerialisable),
    ("VkCudaLaunchInfoNV", "pExtras", VoidPolicy::NotSerialisable),
    // "NV internal use only", in the registry's own `<comment>` on the struct.
    // The specification adds nothing: `pQueriedLowLatencyData` has no size
    // member beside it, no `len=`, and no sentence anywhere stating an extent.
    // A `void*` in a `VkSemaphoreCreateInfo` chain that only NVIDIA's own
    // runtime writes and only NVIDIA's own driver reads.
    ("VkQueryLowLatencySupportNV", "pQueriedLowLatencyData", VoidPolicy::NotSerialisable),
    // A pointer into the *driver's* descriptor-set allocation, handed back to
    // the application. docs/design.md §1a: an address in the GPU side's address
    // space means nothing here and no encoding makes it mean anything.
    //
    // This is a command **parameter**, and that changes what the entry does: a
    // `NotSerialisable` parameter is *dropped* from the wire rather than
    // refused, so this line on its own would carry
    // `vkGetDescriptorSetHostMappingVALVE` as a command that returns nothing
    // and says so to nobody. [`PARAMETER_ANSWERED_APP_SIDE`] is what stops
    // that: the command is not on it, so the generator refuses instead.
    ("vkGetDescriptorSetHostMappingVALVE", "ppData", VoidPolicy::NotSerialisable),
    // The descriptor-update-template family, which is the hardest `void*` in
    // the registry and the one the generator found first.
    //
    // `pData` points at application memory laid out according to the
    // `VkDescriptorUpdateTemplate` the call also names: the template's entries
    // give the offset and stride of every descriptor within it. The registry
    // cannot express that — there is no `len=` and there could not be one — so
    // Venus and gfxstream both compute the extent in hand-written code from the
    // template object.
    //
    // Our answer falls straight out of the arena. `requirements-shared-memory.md`
    // says the command stream carries offsets and never payloads, so the ICD
    // writes the descriptor data into an arena block and the command carries the
    // offset. The GPU side already holds the template — it created it — so it
    // can compute the extent itself and read the bytes through its own mapping
    // of the same pages. No length needs to cross, and no copy happens.
    //
    // `VK_KHR_descriptor_update_template` is one of zink's five hard
    // requirements (docs/design.md §11), so this is not a case that can be excluded.
    ("vkUpdateDescriptorSetWithTemplate", "pData", VoidPolicy::ArenaOffset),
    ("vkUpdateDescriptorSetWithTemplateKHR", "pData", VoidPolicy::ArenaOffset),
    ("vkCmdPushDescriptorSetWithTemplate", "pData", VoidPolicy::ArenaOffset),
    ("vkCmdPushDescriptorSetWithTemplateKHR", "pData", VoidPolicy::ArenaOffset),
    ("vkCmdPushDescriptorSetWithTemplate2", "pData", VoidPolicy::ArenaOffset),
    ("VkPushDescriptorSetWithTemplateInfo", "pData", VoidPolicy::ArenaOffset),
    ("VkPushDescriptorSetWithTemplateInfoKHR", "pData", VoidPolicy::ArenaOffset),
    // The capture-replay class (docs/design.md §0.24). Both are optional, both have a
    // device property for a length where the registry has no `len=`, and both
    // are null unless a trace is being replayed against a device that advertises
    // the matching feature — which this one does not.
    (
        "VkRayTracingShaderGroupCreateInfoKHR",
        "pShaderGroupCaptureReplayHandle",
        VoidPolicy::CaptureReplayUnsupported,
    ),
    (
        "VkOpaqueCaptureDescriptorDataCreateInfoEXT",
        "opaqueCaptureDescriptorData",
        VoidPolicy::CaptureReplayUnsupported,
    ),
    // The five `vkGet…OpaqueCaptureDescriptorDataEXT` queries and their ARM
    // sibling need no entries: `is_capture_replay` reaches them by name. They
    // are the other half of the class — the calls a capture tool makes to
    // *obtain* the token it would later replay — and there the policy makes the
    // whole command refuse, which is right: `pData` must be non-null for the
    // call to mean anything, so every one fails cleanly rather than answering a
    // question this device may not be asked.
];

const BY_FIELD_NAME: &[(&str, VoidPolicy)] = &[
    ("pUserData", VoidPolicy::OpaqueToken),
    ("pCheckpointMarker", VoidPolicy::OpaqueToken),
    ("pTag", VoidPolicy::OpaqueToken),
    // An external handle by any name. Venus excludes these and so do we
    // (docs/design.md §9.2: we do not advertise handle types we cannot honour).
    ("handle", VoidPolicy::NotSerialisable),
    ("hostAddress", VoidPolicy::ArenaOffset),
    // Every `pHostPointer` in the registry is the same thing: application memory
    // the driver reads or writes directly. docs/design.md §5.3 says what an app-side
    // pointer into mapped memory *is* in this design — `arena_base + offset` —
    // so the offset is what crosses, and the GPU side resolves it against its
    // own mapping of the same pages.
    //
    // This subsumes the two per-owner entries above; they stay because their
    // comments carry the argument. What it adds is core 1.4's host image copy
    // (`VkMemoryToImageCopy`, `VkImageToMemoryCopy`), where the pointer is not
    // guaranteed to be arena memory — an application may hand
    // `vkCopyMemoryToImage` a plain `malloc` buffer. That case needs a bounce
    // through an arena block in `sv-icd`, which is why `hostImageCopy` is
    // gated in `sv_icd::advertise` rather than advertised from the GPU side's
    // answer. The wire is right either way; what is not yet written is the copy.
    ("pHostPointer", VoidPolicy::ArenaOffset),
    ("address", VoidPolicy::NotSerialisable),
    ("pPlacedAddress", VoidPolicy::NotSerialisable),
];

/// Types the generator refuses to emit marshalling for, with the reason.
///
/// Distinct from a profile's `excluded_types`: a profile excludes something
/// because *this build* does not want it, whereas this list is about things no
/// build can have. Reaching one is a build failure with the reason attached,
/// which is the difference between "we decided not to" and "we cannot".
pub fn unserialisable_reason(type_name: &str) -> Option<&'static str> {
    UNSERIALISABLE.iter().find(|(n, _)| *n == type_name).map(|(_, r)| *r)
}

const UNSERIALISABLE: &[(&str, &str)] = &[
    (
        "VkAllocationCallbacks",
        "host allocators are not supported: the callbacks are function pointers, \
         which do not cross an address-space boundary. Stripped above the \
         protocol, as Mesa's Venus driver does (docs/design.md §7.4)",
    ),
    (
        "VkAccelerationStructureInstanceKHR",
        "bitfields the registry cannot describe, and out of scope with ray \
         tracing (docs/design.md §7.4)",
    ),
    (
        "VkAccelerationStructureMatrixMotionInstanceNV",
        "bitfields the registry cannot describe (docs/design.md §7.4)",
    ),
    (
        "VkAccelerationStructureSRTMotionInstanceNV",
        "bitfields the registry cannot describe (docs/design.md §7.4)",
    ),
    (
        "VkDebugUtilsMessengerCreateInfoEXT",
        "carries a function pointer; the reverse ring forwards messages instead \
         (docs/design.md §4.6)",
    ),
];

/// Platform typedefs that appear **by value** on the ICD's C surface, and the
/// integer each one actually is.
///
/// `vk.xml` names a platform type and stops: `<type requires="xcb/xcb.h"
/// name="xcb_visualid_t"/>` says where the definition lives and nothing about
/// it. `abi::AbiType::Opaque` renders that as a zero-sized incomplete type,
/// which is the correct and complete ABI of a type only ever seen behind a
/// pointer — and silently wrong for one used by value, where it gives the
/// parameter no storage at all.
///
/// **Two parameters on the whole surface are in that position**, and both are
/// found by `abi::build`'s refusal rather than by reading:
///
/// - `vkGetPhysicalDeviceXcbPresentationSupportKHR(…, xcb_visualid_t visual_id)`
/// - `vkGetPhysicalDeviceXlibPresentationSupportKHR(…, VisualID visualID)`
///
/// Both are on the surface for the reason `abi::SURFACE_CREATORS` gives — the
/// loader's terminator calls `abort()` for an ICD that does not export them —
/// and both are answered locally without looking at the value. So a wrong width
/// here has never had a symptom, which is exactly why it is written down rather
/// than left to be right by accident.
///
/// The widths are the platform headers', quoted:
///
/// ```c
/// /* xcb/xproto.h  */ typedef uint32_t xcb_visualid_t;
/// /* X11/X.h       */ typedef unsigned long VisualID;
/// ```
///
/// `VisualID` is `unsigned long`, so it is 64-bit on the LP64 targets this ICD
/// is built for. It is written as `uint64_t` and the entry says why, because
/// "long" is the one C width that is not a fact about the type alone. Xlib does
/// not exist on the Windows side of this project, so there is no target where
/// the other reading applies.
///
/// The second field is a **registry primitive name**, not a Rust one, so it goes
/// through the same `c_type` mapping every other declaration does.
///
/// This is a hand-written table about a C layout, which is the thing
/// `vk_registry::video_registry_path` argues against — and the difference is
/// that there is no machine-readable source to prefer. For the `StdVideo*`
/// types there is one, and it is used. Here there is not, so the table names its
/// source, states its scope, and is closed: `abi::build` refuses any *other*
/// external type used by value, so this list cannot silently grow.
pub const PLATFORM_SCALARS: &[(&str, &str)] = &[
    ("xcb_visualid_t", "uint32_t"),
    ("VisualID", "uint64_t"),
];

pub fn platform_scalar(type_name: &str) -> Option<&'static str> {
    PLATFORM_SCALARS.iter().find(|(n, _)| *n == type_name).map(|(_, t)| *t)
}

/// Array members that are only *valid to read* when a sibling holds one of a
/// set of values — a union in everything but the registry's spelling.
///
/// # The one that cost a device loss
///
/// `VkWriteDescriptorSet` carries `pImageInfo`, `pBufferInfo` and
/// `pTexelBufferView`. Exactly one of the three is used and `descriptorType`
/// says which; **the other two are ignored and the specification permits them to
/// hold anything at all** — an uninitialised stack slot, a stale pointer, a
/// pointer to an array of a different element type or a shorter length. The
/// registry marks all three `noautovalidity="true"`, which is its way of saying
/// "a sibling decides this and I cannot express how".
///
/// A lowering that reads whichever pointers happen to be non-null is therefore
/// making an out-of-bounds read of the *application's* memory — docs/design.md §0.3
/// item 2's hazard exactly, met for the second time and this time live. What it
/// produced was a `VkBuffer` id of `0xFFFFFFFF00000001` on the wire and
/// `no VkBuffer with id …` on the GPU side; and that is the **lucky** outcome,
/// because an id that fails to map takes the ring fatal and says so. An id that
/// happens to collide with a live object maps successfully, the driver is given
/// a descriptor pointing at the wrong resource, and the GPU hangs with no
/// diagnostic anywhere — which is what the CTS device losses were.
///
/// # Why no harness had caught it
///
/// Zink uses descriptor update *templates* unconditionally in its default mode
/// (docs/design.md §0.4 item 10), and this project expands those app-side into wire
/// structs directly, never touching this lowering. DXVK leans on templates too.
/// So the direct `vkUpdateDescriptorSets` path — the one every ordinary Vulkan
/// application uses — had never been exercised by anything in this tree until
/// the conformance suite ran. §0.15's whole argument for owning a CTS harness,
/// in one struct.
///
/// # Why a table and not a rule
///
/// There is nothing in `vk.xml` to derive this from: `noautovalidity` says only
/// *that* a sibling decides, never *which* sibling or *which* values. The
/// mapping below is the specification's, transcribed, and the table is closed —
/// `VkWriteDescriptorSet` is the only struct on this surface with the shape.
/// A member listed here reads as an empty array when the selector does not
/// choose it, which is what the wire should carry: the callee must not look at
/// it either.
///
/// Values are `VkDescriptorType` enumerants, quoted from `vulkan_core.h`:
/// SAMPLER 0, COMBINED_IMAGE_SAMPLER 1, SAMPLED_IMAGE 2, STORAGE_IMAGE 3,
/// UNIFORM_TEXEL_BUFFER 4, STORAGE_TEXEL_BUFFER 5, UNIFORM_BUFFER 6,
/// STORAGE_BUFFER 7, UNIFORM_BUFFER_DYNAMIC 8, STORAGE_BUFFER_DYNAMIC 9,
/// INPUT_ATTACHMENT 10.
pub const SIBLING_SELECTED: &[(&str, &str, &str, &[i64])] = &[
    ("VkWriteDescriptorSet", "pImageInfo", "descriptorType", &[0, 1, 2, 3, 10]),
    ("VkWriteDescriptorSet", "pBufferInfo", "descriptorType", &[6, 7, 8, 9]),
    ("VkWriteDescriptorSet", "pTexelBufferView", "descriptorType", &[4, 5]),
];

/// The selector and the values that make `struct.member` valid to read.
pub fn sibling_selected(owner: &str, member: &str) -> Option<(&'static str, &'static [i64])> {
    SIBLING_SELECTED
        .iter()
        .find(|(s, m, _, _)| *s == owner && *m == member)
        .map(|(_, _, sel, vals)| (*sel, *vals))
}

/// Members whose value is a `(memory, offset)` pair the `transform` pass must
/// rewrite (docs/design.md §5.3).
///
/// Derived, not listed: any `VkDeviceMemory`-typed member, paired with the
/// sibling that holds its offset. The pairing rule is here rather than in the
/// walker so that the one place it could be wrong is visible.
///
/// > "A generated pass that walks whole struct graphs is the only tractable way
/// > to catch all of them; hand-listing the sites is how this goes wrong."
/// > — docs/design.md §5.3
pub fn memory_offset_sibling(members: &[String], memory_member: &str) -> Option<String> {
    // `memory` → `memoryOffset`; `memory` → `offset`; `pMemory` → `pOffset`.
    let base = memory_member.trim_start_matches('p');
    let candidates = [
        format!("{memory_member}Offset"),
        format!("{}Offset", lower_first(base)),
        "offset".to_string(),
        "memoryOffset".to_string(),
    ];
    for c in candidates {
        if let Some(found) = members.iter().find(|m| **m == c) {
            return Some(found.clone());
        }
    }
    None
}

/// `returnedonly` structs that a command also takes as an **input**, and whose
/// app-side `lower_*` must therefore read every member.
///
/// # The one that dropped a name
///
/// `returnedonly` means "the implementation fills this in", and the app side's
/// `lower_*` honours it by writing zeros instead of reading — right for an
/// output, whose storage is the caller's uninitialised memory. But output
/// parameters now take `lower_out_*` (see `emit::icd_entry`), and in the
/// generated `sv_icd_entry.rs` of the `full` profile `lower_vk_extension_properties`
/// has exactly two callers: `lower_vk_video_capabilities_khr`, which nothing
/// calls, and the input below. So for this struct the zeros landed only where it
/// is *passed in*:
/// `VkVideoSessionCreateInfoKHR::pStdHeaderVersion` is a
/// `const VkExtensionProperties*` the application copies out of
/// `VkVideoCapabilitiesKHR::stdHeaderVersion` and hands back. It crossed as
/// `{ "", 0 }`, and the GPU side's validation layer said so —
/// `VUID-VkVideoSessionCreateInfoKHR-pStdHeaderVersion-07190`, "extensionName ''
/// is an unsupported Video Std header name" (docs/video-decode.md §7). RADV
/// accepted it anyway; a driver that checks the version would refuse the
/// session.
///
/// A list rather than a rule because the rule — "a `returnedonly` struct
/// reachable from a non-output member" — would also flip every `returnedonly`
/// struct nested inside another `returnedonly` struct, whose `lower_*` must
/// keep zeroing for as long as anything could call it on output storage. The
/// list is closed and `emit::icd_entry`'s test checks each entry is still
/// `returnedonly` in the registry, so an entry cannot outlive its reason.
pub const RETURNED_ONLY_READ_AS_INPUT: &[(&str, &str)] = &[(
    "VkExtensionProperties",
    "VkVideoSessionCreateInfoKHR::pStdHeaderVersion is the application's copy of \
     VkVideoCapabilitiesKHR::stdHeaderVersion, passed back in",
)];

/// Whether `name` is on [`RETURNED_ONLY_READ_AS_INPUT`].
pub fn returned_only_read_as_input(name: &str) -> bool {
    RETURNED_ONLY_READ_AS_INPUT.iter().any(|(n, _)| *n == name)
}

/// Array members whose length is a member of an **enclosing** struct, not of
/// their own: `(owner, member, key)`.
///
/// # The one that took H.265 down
///
/// `video.xml` gives `StdVideoH265HrdParameters::pSubLayerHrdParametersNal` and
/// `…Vcl` the length `len="*_max_sub_layers_minus1 + 1"`, and its comment says
/// what the `*` stands for: `sps_max_sub_layers_minus1 + 1` or
/// `vps_max_sub_layers_minus1 + 1`, "depending on whether the HRD parameters
/// are part of the SPS or VPS". No sibling holds it, so the app side refused to
/// lower the HRD struct — and with it the VPS, the SPS (through its VUI), and
/// every H.265 session-parameters and inline-parameters structure. Those links
/// then fell out of their `pNext` chains without a word: H.265 session
/// parameters reached the driver empty, the first decode lost the device, and
/// RADV took the GPU side down with it (docs/video-decode.md §7).
///
/// The answer is the registry's own: the enclosing struct that has the count
/// publishes it under `key` while its members are lowered
/// ([`LEN_CONTEXT_PROVIDERS`]), and the array reads it back. A lowering that
/// reaches the array with no provider above it fails with `Shape` rather than
/// guessing a length — the bound is application memory we did not allocate.
pub const LEN_FROM_CONTEXT: &[(&str, &str, &str)] = &[
    ("StdVideoH265HrdParameters", "pSubLayerHrdParametersNal", "h265_max_sub_layers"),
    ("StdVideoH265HrdParameters", "pSubLayerHrdParametersVcl", "h265_max_sub_layers"),
];

/// The structs that publish a [`LEN_FROM_CONTEXT`] key while their members are
/// lowered: `(struct, member, key, addend)` — the value is `member + addend`,
/// which is the `+ 1` of the registry's `len=`.
pub const LEN_CONTEXT_PROVIDERS: &[(&str, &str, &str, usize)] = &[
    ("StdVideoH265VideoParameterSet", "vps_max_sub_layers_minus1", "h265_max_sub_layers", 1),
    ("StdVideoH265SequenceParameterSet", "sps_max_sub_layers_minus1", "h265_max_sub_layers", 1),
];

/// The context key an array member reads its length from, if it is one.
pub fn len_from_context(owner: &str, member: &str) -> Option<&'static str> {
    LEN_FROM_CONTEXT.iter().find(|(o, m, _)| *o == owner && *m == member).map(|(_, _, k)| *k)
}

/// The `(member, key, addend)` a struct publishes, if it publishes one.
pub fn len_context_provider(owner: &str) -> Option<(&'static str, &'static str, usize)> {
    LEN_CONTEXT_PROVIDERS.iter().find(|(o, ..)| *o == owner).map(|(_, m, k, a)| (*m, *k, *a))
}

/// The two commands that frame a blob and are therefore inside it.
///
/// docs/design.md §6.5 gives the sub-decoder "only the `vkCmd*` opcode subset". That
/// is one command short at each end. The GPU side replays a blob when it
/// *arrives*, not when the application recorded it, so a `vkBeginCommandBuffer`
/// that crossed as an ordinary round trip would open the driver's command buffer
/// at a moment unrelated to the recording it belongs to — and with two command
/// buffers being recorded at once, which Vulkan permits, the `vkCmd*` of one
/// would land in the other. Putting both framing commands in the blob makes it
/// self-describing and removes two round trips per command buffer.
///
/// It is a list of two and it is closed. Anything longer would be the
/// restriction §6.5 asks for quietly dissolving.
pub const BLOB_FRAMING: &[&str] = &["vkBeginCommandBuffer", "vkEndCommandBuffer"];

/// The commands that return a command buffer to a recordable state, and so
/// clear the "recording failed" mark of docs/design.md §0.24.
///
/// # Why two names and not a rule
///
/// There is no attribute in `vk.xml` that distinguishes these from
/// `vkEndCommandBuffer`, which is the third command taking a `VkCommandBuffer`
/// as its first parameter without recording into it. So this is a list, and it
/// is stated as one rather than dressed up as a derivation.
///
/// **`vkBeginCommandBuffer` alone would be enough**, and that is the useful
/// fact. The specification requires it before recording into a command buffer in
/// any state — initial, executable or invalid — so no path exists from "failed
/// to record" back to "being recorded" that does not pass through it. That is
/// why `vkResetCommandPool` needs no entry despite resetting buffers this list
/// never sees: a buffer it reset is in the initial state, and the next thing
/// that may legally happen to it is a `vkBeginCommandBuffer`.
///
/// `vkResetCommandBuffer` is here anyway because it is the same state
/// transition, and a mark that outlived an explicit reset would be surprising to
/// read in a trace even though nothing could observe it.
pub const CLEARS_COMMAND_BUFFER_POISON: &[&str] =
    &["vkBeginCommandBuffer", "vkResetCommandBuffer"];

/// The commands a GPU side must give meaning to rather than forward verbatim.
///
/// This is the honest measure of how much of a GPU side is hand-written: pass 12
/// generates the other ~210, and every entry here is a place where "call the
/// driver with the same arguments" is the *wrong* answer, with the reason
/// alongside. An entry that is merely inconvenient does not belong.
///
/// Four groups, and no others:
///
/// 1. **The session.** The GPU side opens its driver before any application
///    connects (`sv_gpu_server::driver`), because the arena has to be allocated
///    and exported before the app side can attach at all. So `vkCreateInstance`
///    and `vkCreateDevice` *bind an id* to objects that already exist rather
///    than creating new ones, and `vkDestroyDevice` must not destroy the device
///    the arena lives in.
/// 2. **The arena.** `vkAllocateMemory` is a sub-range of a slab the GPU side
///    already imported (docs/design.md §5.2, §5.3); it never reaches a driver.
/// 3. **Submission.** `vkQueueSubmit` carries `svExecuteCommandBlob`
///    descriptors, not a driver call (docs/design.md §6.5).
/// 4. **Two queries whose answer is ours rather than the driver's**: the memory
///    table of docs/design.md §9.1, which is renumbered, and the extension list,
///    which is a promise about what *we* forward.
pub const GPU_HAND_WRITTEN: &[(&str, &str)] = &[
    // --- 1. the session ---------------------------------------------------
    ("vkCreateInstance", "binds an id to the instance the GPU side already opened"),
    ("vkDestroyInstance", "the GPU side's instance outlives any one client"),
    ("vkEnumeratePhysicalDevices", "docs/design.md §8 advertises exactly one device"),
    ("vkCreateDevice", "binds an id to the device the arena was allocated from"),
    ("vkDestroyDevice", "the arena lives in this device"),
    ("vkGetDeviceQueue", "the queue was chosen at startup, with the arena"),
    ("vkGetDeviceQueue2", "as vkGetDeviceQueue"),
    // --- 2. the arena -----------------------------------------------------
    ("vkAllocateMemory", "a sub-range of an imported slab (docs/design.md §5.2)"),
    ("vkFreeMemory", "returns a sub-range to the allocator, and must not free the slab"),
    ("vkMapMemory", "the mapping is the app side's; nothing crosses"),
    ("vkUnmapMemory", "as vkMapMemory"),
    ("vkFlushMappedMemoryRanges", "ranges are in arena coordinates"),
    ("vkInvalidateMappedMemoryRanges", "as vkFlushMappedMemoryRanges"),
    // The first command on this surface the GPU side has to **alter** rather
    // than reproduce, and the reason is the arena rather than the command.
    //
    // The arena's `VkDeviceMemory` is external memory on every provider of
    // docs/design.md §5.4 — exported as a dma-buf on the GPU-allocated path,
    // imported as a host pointer on the app-allocated one — and Vulkan requires
    // that every buffer bound into such memory was *created* naming at least
    // one of the same handle types (`VUID-vkBindBufferMemory-memory-02726` and
    // `-02727`). Neither side can supply that from where it stands: the
    // application never knows its memory is external, and the app side never
    // knows *which* handle type, because that is decided by whichever provider
    // the GPU side chose at startup. Only this side holds the value, and
    // `vkCreateBuffer` is where it has to be applied — before the buffer
    // exists, and long before anyone knows which memory it will be bound to.
    //
    // A generated forwarder cannot do it. The generated path raises the wire
    // struct into C and calls the driver; there is no seam at which a `pNext`
    // link can be added, and adding one is not a shape any pass in §7.2 has —
    // every pass converts what it was given, and none of them invents.
    ("vkCreateBuffer", "the arena is external memory, so every buffer bound into it must say so"),
    // --- 3. answers that are ours -----------------------------------------
    //
    // `vkQueueSubmit` is deliberately **not** here, although docs/design.md §6.5
    // implies it must be. Its blob form (`svExecuteCommandBlob`) is a change to
    // what the *app side* sends, not to what the GPU side does with a
    // `VkSubmitInfo`, and until the app side records blobs the generic
    // forwarding is strictly better than the hand-written version it replaced:
    // it carries the `pNext` chain, so a timeline-semaphore submit works.
    (
        "vkGetPhysicalDeviceMemoryProperties",
        "docs/design.md §9.1's table is renumbered; the driver's would index ours wrongly",
    ),
    ("vkGetPhysicalDeviceMemoryProperties2", "as vkGetPhysicalDeviceMemoryProperties"),
    (
        "vkEnumerateDeviceExtensionProperties",
        "an extension we advertise is a promise that we forward its entry points",
    ),
    // The same argument as the memory table, and it was not obvious until an
    // application acted on the forwarded answer: a GPU side has **one** queue,
    // on one family, chosen at startup. Passing the driver's whole family table
    // through has the application select family 3 and `vkGetDeviceQueue` hand it
    // the queue from family 0, silently. One synthesised family, describing the
    // queue that actually exists, is the only answer that is true.
    (
        "vkGetPhysicalDeviceQueueFamilyProperties",
        "a GPU side has one queue on one family; the driver's table is not ours to pass on",
    ),
    ("vkGetPhysicalDeviceQueueFamilyProperties2", "as vkGetPhysicalDeviceQueueFamilyProperties"),
    // The same argument for features (docs/video-decode.md §7.11). The device
    // exists before the application asks, so what it is told is supported must
    // be what that device *enabled*: forwarded verbatim, the driver said yes to
    // `VK_EXT_extended_dynamic_state3` while the device had it off, and Zink
    // used it.
    (
        "vkGetPhysicalDeviceFeatures2",
        "the answer is masked to the features the already-open device enabled",
    ),
    // The memory table again, met from the other end (docs/video-decode.md §7).
    // The driver answers `memoryTypeBits` in *its* numbering, and an application
    // reads it against docs/design.md §9.1's four types. Session memory is
    // device-local by every driver measured, and this side allocates §9.1's
    // type 0 as the driver's first `DEVICE_LOCAL` type — so the honest answer is
    // bit 0 exactly when the driver's bits include that type. Forwarded
    // verbatim, NVIDIA's session memory (driver type 1, `DEVICE_LOCAL`) would
    // read as our type 1, which is the arena.
    (
        "vkGetVideoSessionMemoryRequirementsKHR",
        "memoryTypeBits is in the driver's numbering and §9.1's table is ours",
    ),
    // And the same for images, which had been forwarded verbatim since the
    // first stage and were right only where the driver's numbering happens to
    // agree with ours. On RADV it nearly does — its type 0 is `DEVICE_LOCAL`, so
    // an optimal image's bit 0 lands on our type 0. On NVIDIA it does not: the
    // driver's type 0 has no flags and type 1 is `DEVICE_LOCAL`, so a DPB whose
    // bits are the driver's `0x2` reads as our type 1 — the arena.
    // Session memory a driver will not take from our type 0's driver type. RADV's
    // AV1 session wants a host-visible type (driver bits `0x678`, none of them
    // its type 0), and §9.1 has exactly one non-arena type to offer. The bind
    // is the first moment this side knows which session and bind index a
    // `VkDeviceMemory` is for, so a dedicated allocation is re-made there from a
    // type the driver accepts.
    (
        "vkBindVideoSessionMemoryKHR",
        "a dedicated type-0 allocation is re-made from a driver type the session accepts",
    ),
    ("vkGetImageMemoryRequirements", "as vkGetVideoSessionMemoryRequirementsKHR"),
    ("vkGetImageMemoryRequirements2", "as vkGetVideoSessionMemoryRequirementsKHR"),
    ("vkGetDeviceImageMemoryRequirements", "as vkGetVideoSessionMemoryRequirementsKHR"),
    // §103.19: on a Mac a shared image is an IOSurface texture, so the
    // host-allocation question is asked of the driver as an MTLTEXTURE one.
    ("vkGetPhysicalDeviceImageFormatProperties", "uses the same native image storage contract as the 2 form"),
    ("vkGetPhysicalDeviceImageFormatProperties2", "on macOS a host-allocation image query is asked as MTLTEXTURE"),
    ("vkGetPhysicalDeviceFormatProperties", "on macOS LINEAR gains what an IOSurface-backed image allows"),
    ("vkGetPhysicalDeviceFormatProperties2", "on macOS LINEAR gains what an IOSurface-backed image allows"),
    // --- 5. a handle the registry does not type as one --------------------
    //
    // `VkDebugUtilsObjectNameInfoEXT::objectHandle` is a `uint64_t` carrying
    // `objecttype="objectType"` — the registry says, in an attribute nothing in
    // this generator reads, that its type is decided at run time by a *sibling
    // member*. The generated `handle_map` pass walks types, so it cannot see it,
    // and the app side's object id therefore reaches the driver unmapped. RADV
    // dereferences it and segfaults, which is how this was found.
    //
    // Mapping it needs a `VkObjectType` → `HandleType` switch at run time, which
    // is a different shape from every other handle site. Two commands, and they
    // are the only ones on this surface.
    (
        "vkSetDebugUtilsObjectNameEXT",
        "objectHandle is a handle only `objecttype=` identifies; no type-directed pass sees it",
    ),
    ("vkSetDebugUtilsObjectTagEXT", "as vkSetDebugUtilsObjectNameEXT"),
    // --- 6. a shape one driver must never see -----------------------------
    //
    // Intel's Windows driver (`igvk64.dll` 101.7076) faults inside
    // `vkCreatePipelineLayout` when the first set layout is a PUSH_DESCRIPTOR
    // one created with a `VkDescriptorSetLayoutBindingFlagsCreateInfo`, which
    // is what Zink makes for every set layout. On a passed-through iGPU the
    // fault stopped the VM (docs/design.md §0.29). The layout is shaped where
    // it is created, because by `vkCreatePipelineLayout` it is a handle.
    (
        "vkCreateDescriptorSetLayout",
        "an all-zero binding-flags link is dropped, exactly; Intel's Windows driver faults on it",
    ),
    // --- 7. a lifetime one driver gets wrong ----------------------------------
    //
    // Intel's Windows driver reads a decode picture's image view after the
    // application has destroyed it, and faulted inside `vkCmdDecodeVideoKHR` on
    // the guest twice (docs/video-decode.md §7.11). A video image's views are
    // therefore made once per description and destroyed with the image
    // (`sv_gpu_server::video_views`), which needs all four commands: the image's
    // usage at creation, the view's description, and both destroys.
    ("vkCreateImage", "records which images are video images, for vkCreateImageView"),
    ("vkCreateImageView", "a video image's view is made once per description and kept"),
    ("vkDestroyImageView", "a kept view is not destroyed until its image is"),
    ("vkDestroyImage", "destroys the image's kept views first"),
    // The fix itself (docs/video-decode.md §7.11.5): the driver is handed the
    // decode structures at per-slot, session-lifetime addresses
    // (`sv_gpu_server::video_stable`), because Intel's Windows driver follows
    // pointers from earlier calls. The end closes the command buffer's scope.
    ("vkCmdBeginVideoCodingKHR", "reference slots point into the session's per-slot stores"),
    ("vkCmdDecodeVideoKHR", "the decode structures are handed over from per-slot stores"),
    ("vkCmdEndVideoCodingKHR", "closes the command buffer's scope in the stores"),
];

/// The generated commands whose driver call `GpuHost::observe` sees, before and
/// after (docs/video-decode.md §7.10).
///
/// The guest's Intel driver faulted inside `vkCmdDecodeVideoKHR` replayed from a
/// command blob after a thousand good frames (waycompose-design.md §87.12), and
/// nothing on either side had recorded what that call was given: a recorded
/// command has no reply, and the fault handler can only name the ring. So the
/// video path's lifecycle is observable — sessions, their parameters, the
/// images and views a DPB is made of, the coding scope and the decode itself,
/// and the submits that run them — and `sv_gpu_server::video_log` writes one
/// line per call *before* the driver sees it when `SV_VIDEO_LOG` is set.
///
/// Buffers are here for their lifetimes: the log only keeps the ones a video
/// command uses, and it is the order of their destroys against the decodes that
/// name them that the log exists to show. Images and views were too, until
/// they became hand-written for the view cache (docs/video-decode.md §7.11);
/// they report themselves now. Every entry must be a generated command; a
/// hand-written one calls the observer itself.
pub const GPU_OBSERVED: &[&str] = &[
    "vkGetPhysicalDeviceVideoCapabilitiesKHR",
    "vkCreateVideoSessionKHR",
    "vkDestroyVideoSessionKHR",
    "vkCreateVideoSessionParametersKHR",
    "vkUpdateVideoSessionParametersKHR",
    "vkDestroyVideoSessionParametersKHR",
    "vkDestroyBuffer",
    "vkBindBufferMemory",
    "vkCmdControlVideoCodingKHR",
    "vkQueueSubmit",
    "vkQueueSubmit2",
];

/// The generated commands whose raise pool `GpuHost::retain_raise` is handed
/// after the driver call, instead of dropping it (docs/video-decode.md §7.11).
///
/// Since §7.11.5 begin and decode are hand-written and call the hook
/// themselves. Since §7.12 the two parameters calls are here as well: the GPU
/// side keeps their pools (the SPS, PPS, VPS and AV1 sequence header arrays)
/// as long as the session, as ffmpeg keeps its H.265 parameter-set arrays for
/// the decoder's life (`sv_gpu_server::video_stable`).
///
/// The guest's Intel driver faulted in `vkCmdDecodeVideoKHR` three times,
/// each time by following a stale pointer at an IDR. In the last run the
/// memory that pointer named had been reused by this side's own raise
/// allocations. One explanation is that the driver keeps a pointer into a
/// structure it was handed by an earlier call. Native ffmpeg keeps those
/// structures in pooled per-picture memory, which would hide that; the
/// forwarder frees each call's copies when the call returns. Keeping the pools
/// (`SV_VIDEO_RETAIN`) is the experiment that tells the two explanations apart.
pub const GPU_RETAIN_RAISE: &[&str] = &[
    "vkCmdControlVideoCodingKHR",
    "vkCreateVideoSessionParametersKHR",
    "vkUpdateVideoSessionParametersKHR",
];

/// Just the names, for the generator's set membership.
pub fn gpu_hand_written() -> Vec<&'static str> {
    GPU_HAND_WRITTEN.iter().map(|(n, _)| *n).collect()
}

fn lower_first(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) => c.to_lowercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}

/// Every exception the generator actually consulted, for the build log and for
/// the manifest that the protocol hash is computed over.
///
/// docs/design.md §4.3: the hash is "computed by the generator over the full command
/// and struct table it emitted — not over `vk.xml`, because … the generator
/// version matters too". An exception table that changed without the registry
/// changing is exactly the kind of generator-version change that must move the
/// hash, so consulted exceptions are recorded and hashed.
#[derive(Debug, Default, Clone)]
pub struct ConsultedExceptions {
    pub void_policies: BTreeMap<(String, String), VoidPolicy>,
    pub unserialisable: BTreeMap<String, &'static str>,
}

impl ConsultedExceptions {
    pub fn note_void(&mut self, owner: &str, field: &str, policy: VoidPolicy) {
        self.void_policies.insert((owner.to_string(), field.to_string()), policy);
    }
    pub fn note_unserialisable(&mut self, name: &str, reason: &'static str) {
        self.unserialisable.insert(name.to_string(), reason);
    }
    pub fn len(&self) -> usize {
        self.void_policies.len() + self.unserialisable.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_interesting_void_pointers_have_the_policies_the_design_names() {
        assert_eq!(
            void_policy("VkImportMemoryHostPointerInfoEXT", "pHostPointer"),
            Some(VoidPolicy::ArenaOffset)
        );
        assert_eq!(
            void_policy("VkDebugUtilsMessengerCreateInfoEXT", "pUserData"),
            Some(VoidPolicy::OpaqueToken)
        );
    }

    #[test]
    fn an_unlisted_void_pointer_has_no_policy_and_will_fail_the_build() {
        assert_eq!(void_policy("VkSomethingNew", "pMystery"), None);
    }

    #[test]
    fn memory_offset_pairing_covers_the_registry_spellings() {
        let members: Vec<String> =
            ["memory", "memoryOffset"].iter().map(|s| s.to_string()).collect();
        assert_eq!(memory_offset_sibling(&members, "memory"), Some("memoryOffset".into()));

        let members: Vec<String> = ["memory", "offset", "size"].iter().map(|s| s.to_string()).collect();
        assert_eq!(memory_offset_sibling(&members, "memory"), Some("offset".into()));

        let members: Vec<String> = ["memory"].iter().map(|s| s.to_string()).collect();
        assert_eq!(memory_offset_sibling(&members, "memory"), None);
    }

    #[test]
    fn the_allocation_callbacks_exclusion_states_its_reason() {
        assert!(unserialisable_reason("VkAllocationCallbacks").unwrap().contains("function pointers"));
    }
}

/// Optional typed interception before wire handle mapping. Defaults preserve
/// driver forwarding; queue workers can replay with the explicit bypass hook.
pub const GPU_OPTIONAL_OVERRIDES: &[&str] = &[
    "vkBindImageMemory", "vkBindImageMemory2", "vkBindBufferMemory", "vkBindBufferMemory2",
    "vkCmdPipelineBarrier", "vkCmdPipelineBarrier2", "vkCmdControlVideoCodingKHR",
    "vkBeginCommandBuffer", "vkEndCommandBuffer", "vkResetCommandBuffer",
    "vkAllocateCommandBuffers", "vkFreeCommandBuffers",
    "vkResetCommandPool", "vkDestroyCommandPool", "vkCmdExecuteCommands",
    "vkGetPhysicalDeviceVideoCapabilitiesKHR",
    "vkGetPhysicalDeviceVideoFormatPropertiesKHR",
    "vkCreateVideoSessionKHR",
    "vkDestroyVideoSessionKHR", "vkCreateVideoSessionParametersKHR",
    "vkUpdateVideoSessionParametersKHR", "vkDestroyVideoSessionParametersKHR",
    "vkGetVideoSessionMemoryRequirementsKHR", "vkBindVideoSessionMemoryKHR",
    "vkQueueSubmit", "vkQueueSubmit2", "vkQueueWaitIdle", "vkDeviceWaitIdle",
    "vkGetFenceStatus", "vkWaitForFences", "vkGetSemaphoreCounterValue", "vkWaitSemaphores",
];
