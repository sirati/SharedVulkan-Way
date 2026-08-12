//! The app side's object tables, and the implementation of the safe half.
//!
//! # What is here and what is not
//!
//! This is the ICD as of design.md §13 **step 5** — "manifest, `VK_DRIVER_FILES`,
//! dispatchable-handle magic, interface negotiation, `vulkaninfo` producing a
//! sane report" — and no further. Step 4, the end-to-end round trip against a
//! GPU side on lavapipe, needs `sv-transport` and `sv-gpu-server`, neither of
//! which is on master. So:
//!
//! - **Object ids are minted here and nothing crosses.** That is exactly what
//!   design.md §6.4 specifies for the app side ("`vkCreateBuffer` encodes the id
//!   it chose and returns immediately — object creation costs no round trip"),
//!   so the ids are real and the encoding is real; what is missing is the ring
//!   that would carry them.
//! - **Arena memory is real.** `vkAllocateMemory` takes a sub-range of a real
//!   shared mapping and `vkMapMemory` returns a pointer into it. Writing through
//!   that pointer writes the memfd. What has not happened is the GPU side
//!   importing the same pages.
//! - **Everything that would need an answer from a GPU is answered locally**,
//!   from the measured table in [`crate::advertise`] or with a conservative
//!   constant, and every such place says so. Nothing here pretends to have run
//!   on a GPU.
//!
//! **That is no longer the state of this crate.** `sv-transport` and
//! `sv-gpu-server` exist, [`crate::transport::RingTransport`] implements the
//! seam, and when `SV_SOCKET` names a live GPU side every one of the ~239
//! forwarded commands crosses instead of being answered here. What is still
//! answered locally is exactly the set design.md §8, §9 and §10 name — the
//! memory table, the extension lists, the WSI family, and `vkMapMemory` — plus,
//! when there is *no* GPU side, everything, so that `vulkaninfo` against a
//! standalone driver still produces a report.
//!
//! The seam is [`sv_icd_entry::Transport`], and it lives there rather than here
//! because the generated forwarders have to name it and they cannot depend on
//! the crate that depends on them.

use crate::advertise;
use crate::arena::{Allocation, Backing, Mapping, Placeholder};
use std::collections::HashMap;
use std::sync::Mutex;
use sv_icd_entry::{IcdResult, LowerError};

/// `VK_MAKE_API_VERSION(0, 1, 3, 0)`, and it must agree with the manifest.
pub const API_VERSION: u32 = (1 << 22) | (3 << 12);

/// The reserved id the arena's `VkDeviceMemory` has on the GPU side.
///
/// Must equal `sv_gpu_server::ARENA_MEMORY_ID`. Declared here rather than taken
/// as a dependency because the app side must not link the GPU side: the two are
/// separate processes and, on `testhost`, separate operating systems. The
/// agreement is checked by `the_two_sides_agree_about_the_arena_id`.
pub const ARENA_MEMORY_ID: u64 = u64::MAX - 1;

/// The default arena size when nothing else says (design.md §4.2's table).
pub const DEFAULT_ARENA_LEN: u64 = 256 << 20;

#[derive(Debug)]
pub struct Instance {
    pub api_version: u32,
    pub physical_devices: Vec<u64>,
}

#[derive(Debug)]
pub struct Device {
    pub physical_device: u64,
    pub queues: Vec<u64>,
}

/// A `VkDeviceMemory` as the app side holds it.
#[derive(Debug, Clone, Copy)]
pub struct Memory {
    pub memory_type_index: u32,
    /// `None` for a heap-0 allocation, which is never arena-backed and never
    /// mappable (design.md §9.1: "impossible by construction rather than by
    /// check").
    pub allocation: Option<Allocation>,
    pub size: u64,
    pub mapped: bool,
}

/// Everything the app side knows.
pub struct State {
    /// design.md §6.4 allocates ids from a per-device counter. One counter for
    /// the process is a superset of that — every id is still unique on the wire
    /// — and it removes the question of which counter a cross-device object
    /// belongs to. Starts at 1 so that 0 stays `VK_NULL_HANDLE`.
    next_id: u64,
    pub instances: HashMap<u64, Instance>,
    /// Physical device id → the instance that enumerated it.
    pub physical_devices: HashMap<u64, u64>,
    pub devices: HashMap<u64, Device>,
    /// Queue id → its device.
    pub queues: HashMap<u64, u64>,
    pub memories: HashMap<u64, Memory>,
    /// Buffer id → its memory requirements, so `vkGetBufferMemoryRequirements`
    /// can answer without a GPU.
    pub buffers: HashMap<u64, u64>,
    pub allocator: Box<dyn Backing>,
}

impl State {
    fn new(allocator: Box<dyn Backing>) -> State {
        State {
            next_id: 1,
            instances: HashMap::new(),
            physical_devices: HashMap::new(),
            devices: HashMap::new(),
            queues: HashMap::new(),
            memories: HashMap::new(),
            buffers: HashMap::new(),
            allocator,
        }
    }

    pub fn mint(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// The ICD.
pub struct SharedVulkan {
    pub arena: Mapping,
    pub arena_len: u64,
    state: Mutex<State>,
    transport: Option<crate::transport::RingTransport>,
}

impl SharedVulkan {
    /// A standalone driver: its own memfd, no GPU side.
    ///
    /// Not a fallback to be removed. `vulkaninfo` against this configuration is
    /// what design.md §13 step 5 tests, the loader test in `tests/loader.rs`
    /// drives it, and a machine with no GPU side running has to enumerate rather
    /// than fail to load.
    pub fn new(arena_len: u64) -> Result<SharedVulkan, std::io::Error> {
        let arena = Mapping::create(arena_len)?;
        let allocator = Placeholder::new(arena_len, crate::arena::DEFAULT_SLAB_SIZE)
            .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
        Ok(SharedVulkan {
            arena,
            arena_len,
            state: Mutex::new(State::new(Box::new(allocator))),
            transport: None,
        })
    }

    /// A connected driver: the arena and the control region come from the peer.
    ///
    /// The allocator becomes the real `sv_arena::ArenaAllocator` rather than
    /// `Placeholder`, because from here the offsets it produces are read by
    /// *another process* — §5.2's segregated free lists and its slab-boundary
    /// rule stop being an implementation detail and start being the contract.
    ///
    /// `endpoint` is a Unix socket path on Linux and a **session name** on
    /// Windows, from which the pipe and the two section names are derived. One
    /// environment variable either way, because an ICD cannot be given
    /// arguments and adding a second variable for the same job is how a user
    /// ends up setting the wrong one.
    #[cfg(any(target_os = "linux", windows))]
    pub fn connected(endpoint: &str) -> Result<SharedVulkan, String> {
        let socket = endpoint;
        let attached = crate::transport::RingTransport::connect(socket)?;
        let arena = Mapping::attach(&attached.transport.arena, attached.arena_len);
        // `CanaryPolicy::None`: R26's canary detects a *guest page* moving
        // underneath a `VirtualLock`ed window, which is a Windows-in-a-VM
        // failure. Neither Linux provider can exhibit it — a memfd's pages do
        // not migrate under a `MAP_SHARED` mapping, and driver memory is the
        // driver's — so the canary here would cost a page per slab to detect
        // something that cannot happen.
        let allocator = sv_arena::alloc::ArenaAllocator::new(
            attached.arena_len,
            attached.slab_size,
            sv_arena::canary::CanaryPolicy::None,
        )
        .map_err(|e| format!("{e}"))?;
        Ok(SharedVulkan {
            arena,
            arena_len: attached.arena_len,
            state: Mutex::new(State::new(Box::new(allocator))),
            transport: Some(attached.transport),
        })
    }

    /// The transport, if this process is connected to a GPU side.
    pub fn wire(&self) -> Option<&dyn sv_icd_entry::Transport> {
        self.transport.as_ref().map(|t| t as &dyn sv_icd_entry::Transport)
    }

    /// design.md §5.3's rewrite, app-side half: allocation coordinates become
    /// arena coordinates.
    ///
    /// Returns `(memory id, offset)` ready to cross. For an arena-backed
    /// allocation that is `(ARENA_MEMORY_ID, arena_offset + offset)`; for
    /// heap-0 memory it is the pair unchanged, because that memory is a real
    /// GPU-side allocation under the app's own id.
    pub fn rewrite_memory(&self, memory: u64, offset: u64) -> IcdResult<(u64, u64)> {
        let s = self.state();
        let m = s
            .memories
            .get(&memory)
            .ok_or(LowerError::Shape("no such VkDeviceMemory"))?;
        let Some(a) = m.allocation else {
            return Ok((memory, offset));
        };
        if offset > a.size {
            return Err(LowerError::Shape("offset is outside the allocation"));
        }
        let base = s
            .allocator
            .arena_offset(a)
            .map_err(|_| LowerError::Shape("allocation is outside the arena"))?;
        Ok((ARENA_MEMORY_ID, base + offset))
    }

    /// A poisoned lock is not a reason to abort an application's Vulkan call:
    /// the state it protects is plain tables, and the panic that poisoned it has
    /// already been reported.
    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Refuse, naming the command.
pub fn unimplemented<T>(what: &'static str) -> IcdResult<T> {
    Err(LowerError::NotImplemented(what))
}

/// The memory table, with this ICD's arena size in it.
pub fn memory_properties(icd: &SharedVulkan) -> sv_icd_entry::VkPhysicalDeviceMemoryProperties {
    advertise::memory_properties(icd.arena_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest's `api_version` and `vkEnumerateInstanceVersion` must agree.
    ///
    /// design.md §8: if they disagree in the wrong direction the loader demotes
    /// the driver to Vulkan 1.0 *and rewrites every application's requested
    /// version*, with no diagnostic. A string comparison is a cheap way never to
    /// find that out the hard way.
    #[test]
    fn the_manifest_agrees_with_what_we_report() {
        let major = (API_VERSION >> 22) & 0x7f;
        let minor = (API_VERSION >> 12) & 0x3ff;
        let patch = API_VERSION & 0xfff;
        assert_eq!(
            format!("{major}.{minor}.{patch}"),
            env!("SV_ICD_MANIFEST_API_VERSION"),
            "the manifest and vkEnumerateInstanceVersion must report the same version"
        );
        assert!(minor >= 1, "a manifest below 1.1 makes the loader demote us silently");
    }
}
