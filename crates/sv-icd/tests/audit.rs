//! **Carried is not implemented**: which commands reach a driver, and which
//! answer here (docs/design.md §0.20).
//!
//! The user's objection is the reason this file exists:
//!
//! > also if we onlt carry 239 that means the others are all noop, thats also a
//! > nogo, those ALL need to be properly implemented
//!
//! The worry is exactly right and the word that has to be tested is *noop*. A
//! command can fail to reach a driver in four quite different ways, and only two
//! of them are the defect being described:
//!
//! 1. **Absent** — not in the profile, so there is no entry point at all and
//!    `vkGetInstanceProcAddr` returns null. The application is told, in the way
//!    Vulkan provides for telling it. Not a noop.
//! 2. **Refused** — in the profile, but the generator could not emit a complete
//!    marshalling for it, so the stub exists and fails *by name*
//!    ([`sv_icd_entry::NOT_IMPLEMENTED`]). Also not a noop: it is diagnosable,
//!    and `INCOMPLETE_EXTENSIONS` stops its extension being advertised.
//! 3. **Answered here, correctly** — the arena, the curated tables, the id
//!    space. A round trip would be wrong, not merely wasteful.
//! 4. **Answered here with a plausible value that nobody computed.** *This* is
//!    the noop, and it is worse than absent because it is not diagnosable.
//!
//! # What is derived and what is judged
//!
//! The split between "forwards" and "answered here" is **derived**, by
//! [`sv_icd_entry::AUDIT_PROBES`], and it rests on one property of the generated
//! trait: the default method with no transport returns
//! `Err(NotImplemented(<its own command name>))`, which no hand-written body
//! does. So calling all of them on a driver with no GPU side sorts the profile
//! in two without reading a line of source. Nothing here is a maintained list of
//! names — that was the mistake docs/design.md §0.18 removed from the API version and
//! it would be the same mistake here.
//!
//! What is **judged** is the classification below, because "should this have
//! been a forward?" is a question about intent that no probe can answer. The
//! judgement is checked-in data and the derived set is asserted against it, so a
//! new override cannot be added without classifying it, and a classification
//! cannot outlive the code it describes.
//!
//! # The one thing the probe cannot see, and why it does not need to
//!
//! A probe runs standalone, so it reports the *standalone* branch of an override
//! that forwards when connected. [`Where::ForwardsWhenConnected`] is that case
//! and it is the majority; calling it a noop would be false. What makes the
//! distinction safe to draw by reading rather than by probing is that it is a
//! syntactic property — the body either mentions `self.wire()` or it does not —
//! and `the_forwarding_overrides_all_mention_the_wire` below checks exactly that
//! against the source, so the judgement column cannot silently drift either.

use sv_icd_entry::AUDIT_PROBES;

/// Where a command's answer comes from, in a **connected** driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Where {
    /// The override forwards; its local arm exists only for the standalone
    /// driver, which is a supported configuration (docs/design.md §13 step 5) and not
    /// a fallback. Not a noop.
    ForwardsWhenConnected,
    /// Answered here and a round trip would be **wrong**: it is a fact about the
    /// arena, the id space or a table this side owns, not about the device.
    ClientSide,
    /// Answered here from something already fetched or already told to us.
    Cache,
    /// **A value nobody computed, returned as though somebody had.** The
    /// deliverable.
    CannedShouldForward,
    /// **An entry point that does nothing at all.** The deliverable.
    Stub,
}

/// Every command the probe finds answering locally, classified.
///
/// Asserted below to be exactly the probe's set — no more, no fewer.
const CLASSIFIED: &[(&str, Where, &str)] = &[
    // -- the physical-device queries: forwarded, with a curated standalone answer
    ("vkGetPhysicalDeviceProperties", Where::ForwardsWhenConnected,
     "forwards; apiVersion clamped to the wire's ceiling afterwards (§0.18)"),
    ("vkGetPhysicalDeviceProperties2", Where::ForwardsWhenConnected,
     "as vkGetPhysicalDeviceProperties"),
    ("vkGetPhysicalDeviceFeatures", Where::ForwardsWhenConnected,
     "forwards; standalone answers all-false, which is the conservative direction"),
    ("vkGetPhysicalDeviceFeatures2", Where::ForwardsWhenConnected,
     "as vkGetPhysicalDeviceFeatures"),
    ("vkGetPhysicalDeviceQueueFamilyProperties", Where::ForwardsWhenConnected,
     "forwards; standalone answers advertise::queue_families()"),
    ("vkGetPhysicalDeviceQueueFamilyProperties2", Where::ForwardsWhenConnected,
     "as vkGetPhysicalDeviceQueueFamilyProperties"),
    ("vkGetPhysicalDeviceFormatProperties", Where::ForwardsWhenConnected,
     "forwards; standalone answers the empty set, and says so at the site"),

    // -- the arena: a round trip here would be wrong, not slow
    ("vkAllocateMemory", Where::ClientSide,
     "§5.2: arena memory is a sub-range of one imported VkDeviceMemory; only a \
      heap-0 allocation forwards. Per-allocation round trips are what the arena \
      exists to remove"),
    ("vkFreeMemory", Where::ClientSide,
     "as vkAllocateMemory: the sub-range returns to our allocator"),
    ("vkGetPhysicalDeviceMemoryProperties", Where::ClientSide,
     "§9.1: the table describes *our* arena's heap and types, not the driver's. \
      Forwarding it would describe memory the application cannot allocate from"),
    ("vkGetPhysicalDeviceMemoryProperties2", Where::ClientSide,
     "as vkGetPhysicalDeviceMemoryProperties: the same arena table, wrapped. Its \
      pNext chain is left as the application left it rather than filled in, which \
      is the honest answer for links we have nothing to put in"),
    ("vkGetBufferMemoryRequirements", Where::Cache,
     "size and alignment are recomputed from what vkCreateBuffer was told plus \
      the arena's granularity; memoryTypeBits is forwarded"),

    // -- external handle properties: all-zero is the specification's "no"
    ("vkGetPhysicalDeviceExternalBufferProperties", Where::ClientSide,
     "§1a/§6.1: no handle type is serialisable across the boundary, so 'not \
      supported' is the true answer and forwarding would produce a promise we \
      could not keep"),
    ("vkGetPhysicalDeviceExternalSemaphoreProperties", Where::ClientSide,
     "as vkGetPhysicalDeviceExternalBufferProperties"),
    ("vkGetPhysicalDeviceExternalFenceProperties", Where::ClientSide,
     "as vkGetPhysicalDeviceExternalBufferProperties"),
    // -- file descriptors: refused, because forwarded they are the GPU side's
    //    descriptor numbers (docs/video-decode.md §7)
    ("vkGetMemoryFdKHR", Where::ClientSide,
     "§1a: a forwarded answer is a descriptor number open in the GPU side's \
      process; VK_ERROR_TOO_MANY_OBJECTS is the true answer"),
    ("vkGetSemaphoreFdKHR", Where::ClientSide, "as vkGetMemoryFdKHR: a sync_fd is a descriptor too"),
    ("vkGetFenceFdKHR", Where::ClientSide, "as vkGetMemoryFdKHR: a fence fd is a descriptor too"),
    ("vkGetMemoryFdPropertiesKHR", Where::ClientSide,
     "§1a: no descriptor of this process names anything the GPU side has"),
    ("vkImportSemaphoreFdKHR", Where::ClientSide,
     "§1a: forwarded, the GPU side's driver would take ownership of, and close, \
      its own descriptor of that number"),
    ("vkImportFenceFdKHR", Where::ClientSide, "as vkImportSemaphoreFdKHR, for a fence instead"),

    // -- object creation: the id is ours (§6.4), the object is the driver's
    ("vkCreateBuffer", Where::ForwardsWhenConnected, "forwards; the id is minted here (§6.4)"),
    ("vkDestroyBuffer", Where::ForwardsWhenConnected, "forwards; drops our size record"),
    ("vkBindBufferMemory", Where::ForwardsWhenConnected,
     "forwards, rewriting (memory, offset) to the arena's reserved id"),
    ("vkCreateCommandPool", Where::ForwardsWhenConnected, "forwards; id minted here"),
    ("vkDestroyCommandPool", Where::ForwardsWhenConnected, "forwards"),
    ("vkCreateFence", Where::ForwardsWhenConnected, "forwards; id minted here"),
    ("vkDestroyFence", Where::ForwardsWhenConnected, "forwards"),
    ("vkCreateSemaphore", Where::ForwardsWhenConnected, "forwards; id minted here"),
    ("vkDestroySemaphore", Where::ForwardsWhenConnected, "forwards"),
    ("vkCreateDescriptorUpdateTemplate", Where::ForwardsWhenConnected,
     "forwards; the entries are also kept here because pData has no describable \
      shape without them"),
    ("vkDestroyDescriptorUpdateTemplate", Where::ForwardsWhenConnected, "forwards"),
    ("vkUpdateDescriptorSetWithTemplate", Where::ForwardsWhenConnected,
     "forwards, expanded into vkUpdateDescriptorSets from the kept entries"),
    ("vkCmdPushDescriptorSetWithTemplate", Where::ForwardsWhenConnected,
     "forwards, expanded into vkCmdPushDescriptorSet from the kept entries plus \
      the template's pipelineBindPoint, which this call does not carry. Before \
      §0.22 it was neither forwarded nor expanded: pData crossed as an arena \
      offset it never was, and the GPU side segfaulted reading a descriptor \
      layout from wherever that number landed"),
    ("vkCmdPushDescriptorSetWithTemplate2", Where::ForwardsWhenConnected,
     "as vkCmdPushDescriptorSetWithTemplate; unwraps the info struct and \
      delegates, so there is one expansion and not two"),

    // -- fences and idles: ordinary forwards
    ("vkGetFenceStatus", Where::ForwardsWhenConnected, "forwards"),
    ("vkResetFences", Where::CannedShouldForward,
     "**sent unanswered** (waycompose-design.md §88): the packet reaches the \
      driver and VK_SUCCESS is synthesised, as for vkQueueSubmit. Its only \
      failures are the out-of-memory codes, which make the ring fatal and \
      surface as VK_ERROR_DEVICE_LOST on the next call. The rings keep its \
      order: later waits share its device ring, later submits carry a barrier"),
    ("vkResetCommandPool", Where::CannedShouldForward,
     "as vkResetFences: sent unanswered, VK_SUCCESS synthesised (§88). Zink \
      resets two pools a frame and each answer cost 0.8 ms on the guest"),
    ("vkWaitForFences", Where::ForwardsWhenConnected, "forwards"),
    ("vkDeviceWaitIdle", Where::ForwardsWhenConnected, "forwards"),
    ("vkQueueWaitIdle", Where::ForwardsWhenConnected, "forwards"),

    // -- the command-buffer blob (§6.5), and the one synthesised result
    ("vkBeginCommandBuffer", Where::ForwardsWhenConnected,
     "recorded into the command buffer's arena blob rather than sent as a packet, \
      which is §6.5's design and still reaches the driver"),
    ("vkEndCommandBuffer", Where::ForwardsWhenConnected,
     "as vkBeginCommandBuffer, and publishes the blob"),
    ("vkResetCommandBuffer", Where::ForwardsWhenConnected, "forwards; discards the blob"),
    ("vkQueueSubmit", Where::CannedShouldForward,
     "**the packet is sent, the VkResult is not the driver's.** Submission is \
      fire-and-forget by design (§4.4, D15) so no reply is waited for, and \
      VK_SUCCESS is synthesised. The work happens; what an application cannot \
      learn from the return value is whether it was accepted. A ring that goes \
      fatal surfaces as VK_ERROR_DEVICE_LOST on the *next* call, not this one"),
];

fn probe_local() -> Vec<&'static str> {
    let icd = sv_icd::state::SharedVulkan::new(1 << 20).expect("a 1 MiB arena");
    let mut out: Vec<&'static str> = AUDIT_PROBES
        .iter()
        .filter(|p| (p.run)(&icd).is_local())
        .map(|p| p.command)
        .collect();
    out.sort_unstable();
    out
}

/// The derived set and the judged set must be the same set.
///
/// This is the assertion that stops the classification rotting. Adding an
/// override without a line above fails here, and so does classifying a command
/// that no longer answers locally.
#[test]
fn every_locally_answered_command_is_classified() {
    let probed = probe_local();
    let mut judged: Vec<&str> = CLASSIFIED.iter().map(|(n, _, _)| *n).collect();
    judged.sort_unstable();

    let unclassified: Vec<&&str> = probed.iter().filter(|n| !judged.contains(n)).collect();
    assert!(
        unclassified.is_empty(),
        "these commands answer locally and are not classified in CLASSIFIED: {unclassified:?}\n\
         Add a line saying which of the five kinds each is, and why."
    );
    let stale: Vec<&&str> = judged.iter().filter(|n| !probed.contains(n)).collect();
    assert!(
        stale.is_empty(),
        "these are classified but no longer answer locally: {stale:?}\n\
         Delete the lines; the code has moved on."
    );
    assert_eq!(probed.len(), CLASSIFIED.len(), "one line each, no duplicates");
}

/// No name appears twice, and no reason is left blank.
#[test]
fn the_classification_is_well_formed() {
    let mut seen = std::collections::BTreeSet::new();
    for (name, kind, why) in CLASSIFIED {
        assert!(seen.insert(*name), "{name} is classified twice");
        assert!(!why.is_empty(), "{name} needs a reason");
        // "forwards" really is the whole reason for a destroy that forwards.
        // For every other kind the claim is that a round trip is *not* wanted,
        // and that is never self-evident.
        if *kind != Where::ForwardsWhenConnected {
            assert!(
                why.len() > 40,
                "{name} is classified {kind:?}, which is a claim that needs an argument"
            );
        }
    }
}

/// The numbers docs/design.md §0.20 quotes, so the prose cannot drift from the code.
#[test]
fn the_headline_numbers_are_what_the_document_says() {
    let probed = probe_local();
    let forwards = AUDIT_PROBES.len() - probed.len();
    let count = |w: Where| CLASSIFIED.iter().filter(|(_, k, _)| *k == w).count();

    // Only meaningful for the profile the document was written against. A
    // different profile is a different audit and must not fail this build.
    if sv_proto::PROTOCOL_PROFILE != "zink" {
        eprintln!("profile {} is not zink — skipping the quoted numbers", sv_proto::PROTOCOL_PROFILE);
        return;
    }

    // These moved once already, and the move is the reason to keep asserting
    // them: `cts-close` implemented the three buffer-device-address entry points
    // and two more besides, so 224 became 229, 188 became 193 and
    // NOT_IMPLEMENTED went 12 -> 9 — while the locally-answered 36 did not
    // change at all. A test that only checked the classification would have said
    // nothing about any of that.
    assert_eq!(AUDIT_PROBES.len(), 229, "commands with a generated Icd method");
    assert_eq!(forwards, 193, "untouched generated defaults, which forward");
    assert_eq!(probed.len(), 36, "overridden, and answered here in the standalone case");
    assert_eq!(count(Where::ForwardsWhenConnected), 27);
    assert_eq!(count(Where::ClientSide), 7);
    assert_eq!(count(Where::Cache), 1);
    assert_eq!(count(Where::CannedShouldForward), 1);
    assert_eq!(count(Where::Stub), 0, "an outright stub among the carried commands");

    // The refused set is the other half of the honest answer: an entry point
    // that exists and fails by name. It must never grow silently.
    assert_eq!(sv_icd_entry::NOT_IMPLEMENTED.len(), 9);
    assert_eq!(
        sv_icd_entry::IMPLEMENTED_COUNT + sv_icd_entry::NOT_IMPLEMENTED.len(),
        sv_icd_entry::COMMAND_COUNT
    );
}

/// Every override classified as forwarding must actually mention the wire.
///
/// The judgement column says "this one forwards when connected", and a probe
/// run standalone cannot check that claim. This can: an override that reaches
/// the GPU side names `self.wire()`, `forwarded!` or `forward::` somewhere in
/// its body, and one that does not, cannot. It is a coarse check and it is
/// exactly as strong as the claim it guards.
#[test]
fn the_forwarding_overrides_all_mention_the_wire() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/icd.rs"))
        .expect("crates/sv-icd/src/icd.rs");
    // Method bodies, split on the fn header. Crude on purpose: a parser here
    // would be a second thing to get wrong, and the property is coarse.
    for (name, kind, _) in CLASSIFIED {
        if *kind != Where::ForwardsWhenConnected {
            continue;
        }
        let method = to_snake(name);
        let Some(start) = src.find(&format!("fn {method}(")) else {
            panic!("{name}: no `fn {method}(` in icd.rs — has it been renamed?");
        };
        let body = &src[start..];
        let end = body[1..].find("\n    fn ").map(|i| i + 1).unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.contains("self.wire()") || body.contains("forwarded!") || body.contains("forward::"),
            "{name} is classified ForwardsWhenConnected but its body never reaches the wire"
        );
    }
}

/// `vkGetPhysicalDeviceProperties2` -> `get_physical_device_properties2`.
///
/// A trailing digit stays attached to its word, which is `naming::command_method`'s
/// rule for commands — and deliberately *not* the rule for struct members, where
/// `maxImageDimension1D` becomes `max_image_dimension1_d`. Two conventions in one
/// generator is a thing to know rather than to guess at.
fn to_snake(vk: &str) -> String {
    let name = vk.strip_prefix("vk").unwrap_or(vk);
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower = i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_ascii_lowercase();
            let prev_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
            if i > 0 && (prev_lower || (prev_upper && next_lower)) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}
