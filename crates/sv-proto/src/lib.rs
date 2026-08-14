//! The generated shared_vulkan protocol.
//!
//! Everything in this crate is emitted by `sv-codegen` from the registry
//! `$VK_XML` names. Nothing here is hand-written except this file and [`sv`],
//! which carries docs/design.md §6.3's non-Vulkan commands — "hand-written, not
//! generated, and … the only place the protocol is ours rather than Vulkan's".
//! They live here rather than in `sv-wire` because they are protocol and not
//! primitives, and beside the generated protocol because both sides need them
//! and the assertion that keeps their two opcode ranges apart has to be able to
//! see `Opcode::COUNT`.
//!
//! # `#![forbid(unsafe_code)]`
//!
//! docs/design.md §7.3 makes this the crate's central claim, and it applies to all of
//! it — encode, decode, pnext, transform, handle map, sub-decoder:
//!
//! > The generated marshalling is the largest body of code in the project and
//! > the one where a mistake is silent corruption rather than a crash.
//!
//! Three properties make it achievable rather than aspirational:
//!
//! - **Nothing is transmuted out of shared memory.** The peer can mutate those
//!   bytes at any moment, so a `&VkFoo` over them would violate the aliasing
//!   model no matter how careful the rest of the code was, and struct padding
//!   would make it unsound even single-threaded. Every field is assembled from a
//!   cursor.
//! - **Every read is bounds-checked**, and returns a [`sv_wire::WireError`]
//!   rather than reading past the end.
//! - **Counts come from the wire, never from a pointer difference or a
//!   registry-derived length applied to peer bytes**, and are checked against
//!   what remains before anything is allocated.
//!
//! The unavoidable `unsafe` is the ICD boundary — `extern "system"` functions
//! taking raw pointers from the application — and it lives in a different crate
//! (`sv-icd-entry`) that has not been written yet.
//!
//! # What this crate does *not* do
//!
//! It has no I/O, no transport, no Vulkan. It turns values into bytes and bytes
//! into values, and it defines the [`Server`] trait that the GPU side
//! implements. That is why it can be tested with no driver present, which is
//! what docs/design.md §13 steps 1–6 depend on.

#![forbid(unsafe_code)]
// The generated code names Vulkan constants exactly as the registry spells
// them, because a constant that greps against the specification is worth more
// than one that satisfies a lint.
#![allow(non_upper_case_globals)]
// The emitters substitute a field access into many positions and parenthesise
// it so that `(*item).0` and `enc.u32(*item)` are both correct without the
// emitter having to know which context it is in. Emitting context-dependent
// parentheses would be a second thing to get right in generated code, to
// silence a lint about a redundant pair of brackets.
#![allow(unused_parens)]
// Style lints that a generator would have to emit *shape-dependent* code to
// satisfy. Every one of them asks for a different spelling depending on what a
// field happens to be, and a generator that has to decide between two spellings
// is a generator with one more thing to get wrong. That trade is not worth
// making in the crate where a mistake is silent memory corruption, so the
// spelling stays uniform and the lints are turned off here, once, in the open.
#![allow(clippy::field_reassign_with_default)] // decode assigns in wire order, using `?`
#![allow(clippy::borrow_deref_ref)] // `&(*inner)` from the uniform access form
#![allow(clippy::needless_borrow)]
#![allow(clippy::explicit_auto_deref)]
#![allow(clippy::derivable_impls)] // `Default` is written out; `derive` stops at [T; 32]
#![allow(clippy::collapsible_if)]
#![allow(clippy::match_single_binding)]
#![allow(clippy::double_parens)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::identity_op)]
#![allow(clippy::let_and_return)]
#![allow(clippy::needless_late_init)]
#![allow(clippy::unnecessary_cast)]

include!(concat!(env!("OUT_DIR"), "/sv_proto.rs"));

pub mod sv;

/// The `protocol_hash` this build carries into the handshake (docs/design.md §4.3).
///
/// Computed by the generator over the manifest it emitted — the full command and
/// struct table, plus the exceptions it consulted — and *not* over `vk.xml`,
/// because a change to the generator moves the wire format just as a change to
/// the registry does.
pub const PROTOCOL_HASH: u64 = {
    // `env!` gives a string; parsing it in a const context keeps the value a
    // compile-time constant rather than a lazily-parsed static.
    const S: &str = env!("SV_PROTOCOL_HASH");
    let bytes = S.as_bytes();
    let mut acc: u64 = 0;
    let mut i = 0;
    while i < bytes.len() {
        acc = acc * 10 + (bytes[i] - b'0') as u64;
        i += 1;
    }
    acc
};

/// Which profile this build was generated for.
pub const PROTOCOL_PROFILE: &str = env!("SV_PROTOCOL_PROFILE");

/// The highest core Vulkan version this wire can carry — `sv-icd`'s **ceiling**.
///
/// `vk_registry::profile::Profile::core_api_version` over the profile the
/// generator ran on, so it cannot drift from the command table: adding
/// `VK_VERSION_1_4` to the profile raises this and brings the fourteen commands
/// and forty-four types it names onto the wire in the same edit.
///
/// An ICD's `vkEnumerateInstanceVersion` is a *promise* that the entry points
/// and structures of that version exist, and the side that has to keep it is the
/// app side, whose whole vocabulary is this profile. So the app side advertises
/// `min(what the driver reports, this)` — never a constant, and never the
/// driver's number unclamped. docs/design.md §0.18 has the measurement of what
/// claiming more than this costs (two GL versions, through Zink).
pub const CORE_API_VERSION: u32 = {
    const S: &str = env!("SV_PROTOCOL_CORE_VERSION");
    let bytes = S.as_bytes();
    let mut acc: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        acc = acc * 10 + (bytes[i] - b'0') as u32;
        i += 1;
    }
    acc
};

/// The canonical protocol manifest, for diagnosing a handshake refusal.
///
/// When two sides disagree, the hash says *that* they disagree and this says
/// *how*: dump both and diff them.
pub const PROTOCOL_MANIFEST: &str =
    include_str!(concat!(env!("OUT_DIR"), "/protocol-manifest.txt"));

#[cfg(test)]
mod support_tests {
    use super::*;

    #[test]
    fn the_protocol_hash_is_not_zero_and_the_manifest_is_not_empty() {
        assert_ne!(PROTOCOL_HASH, 0);
        assert!(PROTOCOL_MANIFEST.starts_with("manifest-version 1"));
        assert!(PROTOCOL_MANIFEST.lines().count() > 1000);
    }

    #[test]
    fn every_opcode_round_trips_through_its_number() {
        for i in 0..Opcode::COUNT as u32 {
            let op = Opcode::from_u32(i).expect("opcodes are dense from zero");
            assert_eq!(op as u32, i);
            assert!(op.name().starts_with("vk"));
        }
        assert!(Opcode::from_u32(Opcode::COUNT as u32).is_none());
    }
}
