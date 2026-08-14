//! `SlabImport`: how a slab becomes a `VkDeviceMemory` on the GPU side.
//!
//! docs/design.md §5.4 puts this behind a trait, and gives the reason plainly:
//!
//! > RADV rejects *every* shared mapping through this path — libdrm passes
//! > `AMDGPU_GEM_USERPTR_ANONONLY` unconditionally and Linux gives
//! > `MAP_SHARED|MAP_ANONYMOUS` a backing file via `shmem_zero_setup()` — so a
//! > shared arena is unimportable on AMD/Linux by construction.
//!
//! That has since been **measured on this machine**: RADV refused a `MAP_SHARED`
//! mapping with `INVALID_EXTERNAL_HANDLE` and NVIDIA with
//! `OUT_OF_DEVICE_MEMORY`, while both accepted an anonymous one. So on a Linux
//! GPU side `VK_EXT_external_memory_host` is **not** the import path, and the
//! Linux-guest topology the project now supports is the first configuration
//! that actually needs the second implementation.
//!
//! # The finding this module exists to record
//!
//! **A memfd is not a dma-buf, and no amount of trait plumbing makes it one.**
//! `VkImportMemoryFdInfoKHR` with `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT`
//! wants a dma-buf descriptor; handing it a memfd fails. docs/design.md §5.4 treats
//! the choice between `HostPointerImport` and `DmaBufImport` as a property of
//! the *importing* side — "`SlabImport` is unchanged by the fragmentation
//! finding … the importing side never learns of it" — and for the host-pointer
//! path that is true. For dma-buf it is not: the arena has to have been
//! **created as a dma-buf in the first place**, which is a constraint on
//! [`crate::extent::ArenaProvider`], the other side of the design, and a
//! decision that has to be taken before a single byte is allocated.
//!
//! Two ways to satisfy it on Linux, neither of which is "share a memfd":
//!
//! 1. **udmabuf** (`/dev/udmabuf`) wraps a memfd that has been sealed with
//!    `F_SEAL_SHRINK` and hands back a dma-buf fd for it. This is the mechanism
//!    that makes the memfd arena importable on AMD, and it is why
//!    [`sv_transport::region::memfd`] passes `MFD_ALLOW_SEALING` — without that
//!    flag the memfd can never be sealed and udmabuf will refuse it, and by then
//!    the arena is committed. **On this host `/dev/udmabuf` exists but is
//!    `root:kvm` mode 0660 and the developer account is not in `kvm`**, so this
//!    path is presently unreachable here without a group change — the same
//!    shape of problem as `SeLockMemoryPrivilege` on the Windows side (R25), and
//!    worth recording next to it.
//! 2. **Let the GPU allocate it** and export with
//!    `VK_KHR_external_memory_fd`, then have the app side map the exported
//!    dma-buf. This inverts D20 — the side inside the VM no longer owns the
//!    region — so it is a different topology rather than a different import.
//!
//! Neither is implemented here. What is implemented is the boundary, so that
//! the next stage picks one rather than discovering the choice exists.
//!
//! # `unsafe`
//!
//! None. This module names resources; it does not dereference them.

use crate::alloc::Allocation;
use crate::{Error, Result};

/// What an import implementation needs to be handed.
///
/// The two variants are the reason this is not simply `(offset, size)`: a host
/// pointer and a dma-buf descriptor are different resources, obtained
/// differently, and an arena that can supply one may be unable to supply the
/// other. Making that visible in the type is the whole point — docs/design.md §5.4's
/// signature hides it, and hiding it is how a Linux GPU side gets built on a
/// path RADV will refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingKind {
    /// `VK_EXT_external_memory_host`, `HOST_ALLOCATION_BIT_EXT`. Measured
    /// working on the Windows/Intel target for four allocators and up to 2 GiB.
    /// Measured **refused** by RADV and NVIDIA on Linux for a `MAP_SHARED`
    /// mapping.
    HostPointer,
    /// `VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf`. The Linux
    /// GPU side's path, and it constrains how the arena was created.
    DmaBuf,
    /// lavapipe adopts the pointer. "A great harness and a dangerous oracle"
    /// (§5.4): it will import a pointer every real driver rejects, so a pass
    /// here is never evidence the memory mechanism works.
    Noop,
}

/// Which import path a GPU side should use, given what it is.
///
/// Deliberately a function of the *driver*, not of the arena: the arena's job is
/// to satisfy whatever this returns, and it has to know before it commits.
pub fn path_for_driver(driver: &str) -> BackingKind {
    match driver {
        // **RADV refuses every shared mapping, whatever backs it.** libdrm
        // passes `AMDGPU_GEM_USERPTR_ANONONLY` unconditionally, so this is
        // structural rather than incidental and no choice of backing helps.
        // udmabuf is the only route (`measured-gpu-side.md`, "The reverse
        // direction").
        "radv" | "amdgpu" => BackingKind::DmaBuf,
        // **NVIDIA takes a shared mapping of shmem** — measured 2026-08-12,
        // `memoryTypeBits = 0xc`, coherent in both directions, at 4 MiB and
        // 128 MiB. The earlier "NVIDIA refuses MAP_SHARED" result was measured
        // against a file on ext4; the boundary is shmem versus page cache, and
        // the arena is a memfd, which is shmem. So the discrete card can serve
        // a guest with no udmabuf and no group change.
        "nvidia" => BackingKind::HostPointer,
        "lavapipe" | "llvmpipe" | "swiftshader" => BackingKind::Noop,
        // The measured-working target, and the conservative default: a driver
        // we have not tried gets the path that is known to work somewhere real
        // rather than the one with the extra prerequisite.
        _ => BackingKind::HostPointer,
    }
}

/// The GPU side's import of one slab. docs/design.md §5.4.
///
/// Generic over the driver's memory handle so that this crate needs no Vulkan
/// dependency — `sv-arena` is testable with no driver present, which is what
/// docs/design.md §13 steps 1–6 depend on.
pub trait SlabImport {
    /// Whatever the driver calls a `VkDeviceMemory`.
    type Memory;

    /// What this implementation needs the arena to be.
    fn backing_kind(&self) -> BackingKind;

    /// gfxstream's defensive memory-type rewrite (§5.4): call
    /// `vkGetMemoryHostPointerPropertiesEXT` first, and if the preferred type
    /// index is not in the returned `memoryTypeBits`, pick the lowest set bit
    /// instead.
    fn memory_type_for(&self, arena_offset: u64, size: u64) -> Result<u32>;

    /// Import `[arena_offset, arena_offset + size)` as one `VkDeviceMemory`.
    fn import(&mut self, arena_offset: u64, size: u64) -> Result<Self::Memory>;
}

/// gfxstream's memory-type rewrite, extracted so it can be tested without a
/// driver.
///
/// > if the preferred type index is not in the returned `memoryTypeBits`, pick
/// > the lowest set bit instead. A `memoryTypeBits` mask that excludes your
/// > choice is a normal condition to handle, not a failure. — docs/design.md §5.4
///
/// On the measured target the mask is `0x6` and the choice is index 1, so the
/// rewrite never fires; it costs one branch and it is kept because the mask is
/// the driver's business.
pub fn choose_memory_type(preferred: u32, memory_type_bits: u32) -> Result<u32> {
    if memory_type_bits == 0 {
        return Err(Error::BadGeometry("the driver offers no memory type for this pointer"));
    }
    if memory_type_bits & (1 << preferred) != 0 {
        return Ok(preferred);
    }
    Ok(memory_type_bits.trailing_zeros())
}

/// An import that does nothing, for the development loop.
///
/// It records what it was asked to import so a test can assert the *sequence* of
/// imports — which is the part of lazy import (§5.2) that is easy to get wrong
/// and that no driver is needed to check.
#[derive(Debug, Default)]
pub struct NoopImport {
    pub imported: Vec<(u64, u64)>,
}

impl SlabImport for NoopImport {
    type Memory = usize;

    fn backing_kind(&self) -> BackingKind {
        BackingKind::Noop
    }

    fn memory_type_for(&self, _arena_offset: u64, _size: u64) -> Result<u32> {
        // lavapipe returns `memoryTypeBits = 1` unconditionally.
        choose_memory_type(1, 1)
    }

    fn import(&mut self, arena_offset: u64, size: u64) -> Result<Self::Memory> {
        self.imported.push((arena_offset, size));
        Ok(self.imported.len() - 1)
    }
}

/// Import every slab an allocation touches that has not been imported yet.
///
/// docs/design.md §5.2: "Slabs are **imported** lazily. When the app side needs a
/// slab it picks a free region of the arena, writes a slab-directory entry, and
/// sends `svImportArenaSlab`." This is the app side's half of that decision,
/// with the round trip left to the caller.
pub fn slabs_to_import(alloc: &crate::alloc::ArenaAllocator, a: Allocation) -> Vec<u32> {
    if alloc.slab_needs_import(a.slab) { vec![a.slab] } else { Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::ArenaAllocator;
    use crate::canary::CanaryPolicy;

    #[test]
    fn the_measured_drivers_get_the_path_that_works_for_them() {
        // The correction to docs/design.md §5.4's table, measured on this machine.
        assert_eq!(path_for_driver("radv"), BackingKind::DmaBuf);
        // Corrected 2026-08-12: NVIDIA imports a shared mapping of *shmem*, and
        // the arena is a memfd. What it refuses is a file on a real filesystem,
        // which is what the contradicted measurement used.
        assert_eq!(path_for_driver("nvidia"), BackingKind::HostPointer);
        assert_eq!(path_for_driver("lavapipe"), BackingKind::Noop);
        // The Windows/Intel target, and the default for anything untried.
        assert_eq!(path_for_driver("intel"), BackingKind::HostPointer);
    }

    #[test]
    fn the_memory_type_rewrite_fires_only_when_it_has_to() {
        // The measured target: mask 0x6, preferred index 1, no rewrite.
        assert_eq!(choose_memory_type(1, 0x6).unwrap(), 1);
        // A mask that excludes the preference: take the lowest set bit.
        assert_eq!(choose_memory_type(0, 0x6).unwrap(), 1);
        assert_eq!(choose_memory_type(3, 0b1010_0000).unwrap(), 5);
        // lavapipe's unconditional 1.
        assert_eq!(choose_memory_type(1, 1).unwrap(), 0);
        // No type at all is a failure, not a silent zero.
        assert!(choose_memory_type(0, 0).is_err());
    }

    #[test]
    fn lazy_import_asks_for_each_slab_once() {
        let mut a = ArenaAllocator::new(4 << 20, 1 << 20, CanaryPolicy::None).unwrap();
        let mut imp = NoopImport::default();
        let mut asked = Vec::new();
        // Allocate enough to spill into a second slab.
        for _ in 0..6 {
            let x = a.alloc(256 * 1024).unwrap();
            for slab in slabs_to_import(&a, x) {
                let (base, len) = a.slab_span(slab).unwrap();
                imp.import(base, len).unwrap();
                a.mark_imported(slab);
                asked.push(slab);
            }
        }
        assert_eq!(asked, vec![0, 1], "each slab imported once, in order");
        assert_eq!(imp.imported.len(), 2);
        assert_eq!(imp.imported[0], (0, 1 << 20));
        assert_eq!(imp.imported[1], (1 << 20, 1 << 20));
        assert_eq!(imp.backing_kind(), BackingKind::Noop);
    }
}
