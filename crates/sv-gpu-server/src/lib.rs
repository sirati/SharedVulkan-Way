//! `sv-gpu-server` — the GPU side.
//!
//! design.md §3's right-hand box: it attaches to the control region, consumes
//! command rings, decodes with the generated protocol, calls a real Vulkan
//! driver and writes replies.
//!
//! | module | what it is |
//! |---|---|
//! | [`driver`] | one open Vulkan driver, opened at startup rather than on demand |
//! | [`arena`] | where the arena comes from — the two providers, and the measurement that produced the second |
//! | [`table`] | app id → driver handle, and the GPU-side half of §5.3's `(memory, offset)` rewrite |
//! | [`server`] | `impl sv_proto::Server`: decoded arguments in, driver calls out |
//! | [`convert`] | `ash` → wire, for the values reported back |
//! | [`session`] | publish, then one thread per ring |

pub mod arena;
pub mod convert;
pub mod driver;
pub mod server;
pub mod session;
pub mod table;
pub mod workload;

/// The reserved object id for "the arena's own `VkDeviceMemory`".
///
/// design.md §5.3 makes an app-side `VkDeviceMemory` a sub-range of a slab and
/// has the app side rewrite every `(memory, offset)` pair to "(slab handle,
/// `slab_offset + app_offset`)". The app side cannot name a *driver* handle —
/// see the note on [`table::resolve_memory`] — so what it names is this
/// sentinel, and the GPU side resolves it.
///
/// A sentinel rather than a negotiated value because the app side's ids come
/// from a counter that starts at 1 (`sv_icd::state::State::mint`), so nothing it
/// mints can ever collide with `u64::MAX - 1`.
pub const ARENA_MEMORY_ID: u64 = u64::MAX - 1;

/// The wire version both sides put in the control header.
///
/// Bumped when the generated protocol's *shape* changes in a way the hash does
/// not already catch. In practice `sv_proto::PROTOCOL_HASH` is the real check
/// (§4.3, R13) and this is the coarse one in front of it.
pub const WIRE_VERSION: u32 = 1;

/// The identity the two sides must agree on before a single ring is claimed.
///
/// Unlike `sv_hostd::demo_ids`, this is the **real** one: the hash is the
/// generator's, over the command and struct table it emitted, so two builds from
/// different registries refuse to connect instead of desynchronising. That is
/// the whole of R13's mitigation and it only works if both sides pass this.
pub fn protocol_ids() -> sv_transport::control::ProtocolIds {
    sv_transport::control::ProtocolIds::new(WIRE_VERSION, sv_proto::PROTOCOL_HASH)
}
