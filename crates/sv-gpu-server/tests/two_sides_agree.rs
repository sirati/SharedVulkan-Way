//! The constants the two sides must agree on, compared rather than asserted.
//!
//! `sv-icd` and `sv-gpu-server` are separate processes and, in the `testhost`
//! topology, separate operating systems, so neither may depend on the other.
//! That leaves every shared value declared twice — and a value declared twice is
//! a value that can drift. This test binary links both and compares them, which
//! is the only place in the tree where that is allowed.
//!
//! The failure each of these guards against is silent rather than loud, which is
//! why they are worth the dev-dependency:
//!
//! - a mismatched `ARENA_MEMORY_ID` makes every `vkBindBufferMemory` name an
//!   object the GPU side has never heard of, one call *after* the allocation
//!   that looked fine;
//! - a mismatched `WIRE_VERSION` or protocol hash is caught at handshake, which
//!   is the good case — but only if both sides compute it the same way, and
//!   *that* is what this checks.

#[test]
fn the_arena_memory_id_is_the_same_number_on_both_sides() {
    assert_eq!(
        sv_gpu_server::ARENA_MEMORY_ID,
        sv_icd::state::ARENA_MEMORY_ID,
        "the app side rewrites (memory, offset) to this id and the GPU side resolves it; \
         if they disagree, every bind names an unknown object"
    );
    // And it must be unreachable by minting. `sv_icd::state::State::mint` counts
    // up from 1, so the sentinel has to be at the far end of the range.
    const _: () = {
        assert!(sv_gpu_server::ARENA_MEMORY_ID > u64::MAX / 2);
        assert!(sv_gpu_server::ARENA_MEMORY_ID != 0, "0 is VK_NULL_HANDLE");
    };
}

#[test]
fn both_sides_put_the_same_identity_in_the_handshake() {
    let gpu = sv_gpu_server::protocol_ids();
    let app = sv_icd::protocol_ids();
    assert_eq!(gpu.layout_version, app.layout_version);
    assert_eq!(gpu.wire_version, app.wire_version);
    assert_eq!(
        gpu.protocol_hash, app.protocol_hash,
        "both must be sv_proto::PROTOCOL_HASH, or the R13 detector fires on every \
         connection between two correct builds"
    );
    // The hash has to be the generator's, not a placeholder: `sv-hostd`'s demo
    // constant exists for the loopback binaries and must never reach here.
    assert_eq!(gpu.protocol_hash, sv_proto::PROTOCOL_HASH);
    assert_ne!(gpu.protocol_hash, sv_hostd::DEMO_PROTOCOL_HASH);
}
