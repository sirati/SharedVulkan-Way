//! The exception table: everything `vk.xml` cannot express, budgeted rather
//! than discovered.
//!
//! design.md §7.4 is the specification for this file, and its closing sentence
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
/// design.md §7.4 counts 103 of these registry-wide. Those with a `len` are
/// blobs and need no entry. The rest are listed here by `(owner, field)`, and an
/// unlisted one is a generator error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoidPolicy {
    /// An offset into the passthrough arena.
    ///
    /// `pHostPointer` is the interesting one, exactly as §7.4 says: it is
    /// `VK_EXT_external_memory_host`, and in our design a host pointer *is* an
    /// arena offset (design.md §5.3). It crosses as a `u64` offset, and the GPU
    /// side resolves it against its own mapping of the same pages.
    ArenaOffset,
    /// An opaque token the app side owns and the GPU side never dereferences.
    ///
    /// `pUserData` is the canonical case: it is passed back to an app-side
    /// callback and has no meaning on the other machine. It crosses as an
    /// opaque `u64` so the app side can round-trip it, and the GPU side treats
    /// it as a number.
    OpaqueToken,
    /// Not serialisable at all; a command or struct reaching one is a build
    /// failure.
    ///
    /// Venus's exclusion list, taken verbatim in spirit (design.md §6.1):
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
}

const VOID_POLICIES: &[(&str, &str, VoidPolicy)] = &[
    // design.md §5.3: an app-side `vkMapMemory` pointer is arena_base + offset,
    // and the GPU side never maps arena memory through the driver at all. The
    // import path therefore carries an offset.
    ("VkImportMemoryHostPointerInfoEXT", "pHostPointer", VoidPolicy::ArenaOffset),
    ("vkGetMemoryHostPointerPropertiesEXT", "pHostPointer", VoidPolicy::ArenaOffset),
    // `vkMapMemory`'s out-parameter is a pointer into our own mapping. It is
    // computed app-side and never crosses (design.md §5.3: "no round trip and
    // no copy"), so the command is forwarded without it.
    ("vkMapMemory", "ppData", VoidPolicy::NotSerialisable),
    // Explicitly out of scope: a window handle is a WSI handle.
    ("VkAndroidSurfaceCreateInfoKHR", "window", VoidPolicy::NotSerialisable),
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
    // requirements (design.md §11), so this is not a case that can be excluded.
    ("vkUpdateDescriptorSetWithTemplate", "pData", VoidPolicy::ArenaOffset),
    ("vkUpdateDescriptorSetWithTemplateKHR", "pData", VoidPolicy::ArenaOffset),
    ("vkCmdPushDescriptorSetWithTemplate", "pData", VoidPolicy::ArenaOffset),
    ("vkCmdPushDescriptorSetWithTemplateKHR", "pData", VoidPolicy::ArenaOffset),
    ("vkCmdPushDescriptorSetWithTemplate2", "pData", VoidPolicy::ArenaOffset),
    ("VkPushDescriptorSetWithTemplateInfo", "pData", VoidPolicy::ArenaOffset),
    ("VkPushDescriptorSetWithTemplateInfoKHR", "pData", VoidPolicy::ArenaOffset),
];

const BY_FIELD_NAME: &[(&str, VoidPolicy)] = &[
    ("pUserData", VoidPolicy::OpaqueToken),
    ("pCheckpointMarker", VoidPolicy::OpaqueToken),
    ("pTag", VoidPolicy::OpaqueToken),
    // An external handle by any name. Venus excludes these and so do we
    // (design.md §9.2: we do not advertise handle types we cannot honour).
    ("handle", VoidPolicy::NotSerialisable),
    ("hostAddress", VoidPolicy::ArenaOffset),
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
         protocol, as Mesa's Venus driver does (design.md §7.4)",
    ),
    (
        "VkAccelerationStructureInstanceKHR",
        "bitfields the registry cannot describe, and out of scope with ray \
         tracing (design.md §7.4)",
    ),
    (
        "VkAccelerationStructureMatrixMotionInstanceNV",
        "bitfields the registry cannot describe (design.md §7.4)",
    ),
    (
        "VkAccelerationStructureSRTMotionInstanceNV",
        "bitfields the registry cannot describe (design.md §7.4)",
    ),
    (
        "VkDebugUtilsMessengerCreateInfoEXT",
        "carries a function pointer; the reverse ring forwards messages instead \
         (design.md §4.6)",
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

/// Members whose value is a `(memory, offset)` pair the `transform` pass must
/// rewrite (design.md §5.3).
///
/// Derived, not listed: any `VkDeviceMemory`-typed member, paired with the
/// sibling that holds its offset. The pairing rule is here rather than in the
/// walker so that the one place it could be wrong is visible.
///
/// > "A generated pass that walks whole struct graphs is the only tractable way
/// > to catch all of them; hand-listing the sites is how this goes wrong."
/// > — design.md §5.3
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

/// The two commands that frame a blob and are therefore inside it.
///
/// design.md §6.5 gives the sub-decoder "only the `vkCmd*` opcode subset". That
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
///    already imported (design.md §5.2, §5.3); it never reaches a driver.
/// 3. **Submission.** `vkQueueSubmit` carries `svExecuteCommandBlob`
///    descriptors, not a driver call (design.md §6.5).
/// 4. **Two queries whose answer is ours rather than the driver's**: the memory
///    table of design.md §9.1, which is renumbered, and the extension list,
///    which is a promise about what *we* forward.
pub const GPU_HAND_WRITTEN: &[(&str, &str)] = &[
    // --- 1. the session ---------------------------------------------------
    ("vkCreateInstance", "binds an id to the instance the GPU side already opened"),
    ("vkDestroyInstance", "the GPU side's instance outlives any one client"),
    ("vkEnumeratePhysicalDevices", "design.md §8 advertises exactly one device"),
    ("vkCreateDevice", "binds an id to the device the arena was allocated from"),
    ("vkDestroyDevice", "the arena lives in this device"),
    ("vkGetDeviceQueue", "the queue was chosen at startup, with the arena"),
    ("vkGetDeviceQueue2", "as vkGetDeviceQueue"),
    // --- 2. the arena -----------------------------------------------------
    ("vkAllocateMemory", "a sub-range of an imported slab (design.md §5.2)"),
    ("vkFreeMemory", "returns a sub-range to the allocator, and must not free the slab"),
    ("vkMapMemory", "the mapping is the app side's; nothing crosses"),
    ("vkUnmapMemory", "as vkMapMemory"),
    ("vkFlushMappedMemoryRanges", "ranges are in arena coordinates"),
    ("vkInvalidateMappedMemoryRanges", "as vkFlushMappedMemoryRanges"),
    // The first command on this surface the GPU side has to **alter** rather
    // than reproduce, and the reason is the arena rather than the command.
    //
    // The arena's `VkDeviceMemory` is external memory on every provider of
    // design.md §5.4 — exported as a dma-buf on the GPU-allocated path,
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
    // `vkQueueSubmit` is deliberately **not** here, although design.md §6.5
    // implies it must be. Its blob form (`svExecuteCommandBlob`) is a change to
    // what the *app side* sends, not to what the GPU side does with a
    // `VkSubmitInfo`, and until the app side records blobs the generic
    // forwarding is strictly better than the hand-written version it replaced:
    // it carries the `pNext` chain, so a timeline-semaphore submit works.
    (
        "vkGetPhysicalDeviceMemoryProperties",
        "design.md §9.1's table is renumbered; the driver's would index ours wrongly",
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
/// design.md §4.3: the hash is "computed by the generator over the full command
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
