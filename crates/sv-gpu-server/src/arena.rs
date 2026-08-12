//! Where the arena comes from, and how the GPU side ends up with a
//! `VkDeviceMemory` over it.
//!
//! # The rule, and the measurement that produced it
//!
//! **Whichever side can allocate memory the other can map, allocates.**
//!
//! design.md §5.4 assumed the arena is always allocated by the side inside the
//! VM and *imported* by the driver, and put the import behind [`SlabImport`].
//! That is right for `testhost` and wrong for a Linux GPU side, because
//! `sv_arena::import` measured RADV and NVIDIA both refusing a `MAP_SHARED` host
//! pointer. The conclusion drawn from that was "we need a dma-buf, therefore we
//! need `/dev/udmabuf`, therefore we need to be in the `kvm` group" — and that
//! conclusion was wrong. There are two directions across the dma-buf boundary
//! and only one of them is privileged:
//!
//! - *userspace allocates, the kernel wraps it* — `/dev/udmabuf`, which pins
//!   arbitrary user pages on demand and is therefore `root:kvm` 0660 here;
//! - *the GPU allocates, then exports* — `vkGetMemoryFdKHR`, which is an ioctl
//!   on a render node the process already has open. `/dev/dri/renderD128` on
//!   this host is `crw-rw-rw-`. This is why CUDA and ordinary Vulkan need no
//!   privilege, and it was the evidence that the first conclusion was wrong.
//!
//! Our arena was a memfd only because `sv-hostd` created it. Inverting that for
//! the Linux/Linux topology costs nothing and removes the privilege problem.
//!
//! # Measured on this machine, by `sv-export-probe`
//!
//! | driver | app-allocated memfd, imported as a host pointer | GPU-allocated, exported as a descriptor |
//! |---|---|---|
//! | NVIDIA RTX 5090 | refused (`OUT_OF_DEVICE_MEMORY`) | **dma-buf, `mmap` works, coherent both directions** |
//! | RADV (Raphael iGPU) | refused (`INVALID_EXTERNAL_HANDLE`) | **dma-buf, `mmap` works, coherent both directions** |
//! | lavapipe | works — it adopts the pointer | **no**: no dma-buf export, and its opaque-fd export is *not the same pages* |
//! | Intel, Windows guest | works (step 0, up to 2 GiB) | n/a — a descriptor cannot cross the VM boundary |
//!
//! Two things in that table are worth stating out loud rather than leaving to be
//! read off it.
//!
//! **The two paths are complements, not alternatives.** Every row has exactly
//! one working column. There is no driver here for which both work and no driver
//! for which neither does, so the provider is *chosen by capability* and neither
//! implementation can be deleted. In particular the `testhost` topology has to
//! keep the memfd path: the GPU side is inside a VM, a descriptor cannot cross
//! that boundary, and the arena there has to be guest RAM the host locates.
//!
//! **lavapipe is a worse oracle than §5.4 said, and in a new direction.** §5.4
//! warns that lavapipe "will cheerfully import a pointer every real driver
//! rejects". The export direction fails the other way: `vkGetMemoryFdKHR` with
//! `OPAQUE_FD` *succeeds*, the `mmap` of the result *succeeds*, and the bytes
//! are not the same memory. A probe that only checked "did the call return
//! `VK_SUCCESS`" would have reported this as working. That is why
//! `sv-export-probe` writes through both mappings and reads back through the
//! other one, and why the check is in both directions.

// Both providers are Linux-only: one needs `mmap` of a descriptor and the other
// needs a descriptor to pass. A Windows GPU side keeps `Arena` and replaces the
// two constructors with `CreateFileMapping` and the host-pointer import, which
// is why the *type* is portable and the functions are not.
#[cfg(target_os = "linux")]
use crate::driver::{Driver, DriverError, Result};
use ash::vk;
use std::sync::Arc;
use sv_transport::region::SharedRegion;

/// How this session obtained the arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// The app side allocated it — `sv-hostd`'s memfd, or guest RAM it located —
    /// and the GPU side imports it with `VK_EXT_external_memory_host`.
    /// design.md §5.4's `HostPointerImport`.
    AppMemfd,
    /// The GPU side allocated it from the driver and exported a descriptor for
    /// the app side to map. Not in design.md §5.4's table, and the row that
    /// unblocks RADV and NVIDIA.
    GpuExport,
}

impl Provider {
    /// Which provider a driver can actually serve, from the measurements above.
    ///
    /// Deliberately a function of the driver rather than a flag, for the same
    /// reason `sv_arena::import::path_for_driver` is: the arena has to know
    /// before it commits, and a wrong guess is discovered at the first
    /// allocation rather than at startup.
    pub fn for_driver(key: &str) -> Provider {
        match key {
            // Both refuse a shared host pointer and both export a dma-buf.
            "radv" | "nvidia" => Provider::GpuExport,
            // Adopts a host pointer; its export is not the same pages.
            "lavapipe" => Provider::AppMemfd,
            // The measured-working Windows target, and the conservative default:
            // an untried driver gets the path that is proven somewhere real and
            // has no extra prerequisite.
            _ => Provider::AppMemfd,
        }
    }
}

/// The arena, as the GPU side holds it.
///
/// Both providers converge here, and that convergence is the point: above this
/// type nothing knows which one produced it. `region` is the GPU side's own
/// mapping — the one it reads command data through, at the offsets the wire
/// carries (§5.3: "the GPU side reads through *its own* mapping of the arena") —
/// and `memory` is one `VkDeviceMemory` covering the whole of it at offset 0.
pub struct Arena {
    pub provider: Provider,
    pub region: Arc<SharedRegion>,
    pub memory: vk::DeviceMemory,
    pub len: u64,
    /// The memory type the driver ended up using, for the report.
    pub memory_type: u32,
    /// Held for as long as the arena lives on the export path: the app side has
    /// its own duplicate, but dropping ours would be a surprise waiting to
    /// happen if the app side ever reconnects.
    #[cfg(target_os = "linux")]
    pub exported_fd: Option<std::os::fd::OwnedFd>,
}

impl Arena {
    /// design.md §5.2's slab count, for a report. One import covers all of them
    /// in this stage; see the note on lazy import in [`import_whole`].
    pub fn slab_count(&self, slab_size: u64) -> u64 {
        self.len.div_ceil(slab_size.max(1))
    }
}

/// The GPU-allocated path: allocate, export, map the descriptor ourselves.
///
/// Returns the arena and the descriptor to hand the app side. The GPU side maps
/// the descriptor rather than calling `vkMapMemory`, so that **both sides reach
/// the arena the same way** — through an ordinary `mmap` of the same object.
/// That is not merely tidy: lavapipe segfaults in `vkFreeMemory` when an
/// allocation has been both exported and `vkMapMemory`'d, and while lavapipe
/// does not take this path, a driver that is fragile about one combination is
/// evidence that the combination is worth avoiding.
#[cfg(target_os = "linux")]
pub fn allocate_and_export(driver: &Driver, len: u64) -> Result<Arena> {
    use std::os::fd::FromRawFd;

    let ext = driver
        .external_fd
        .as_ref()
        .ok_or(DriverError::Missing("VK_KHR_external_memory_fd"))?;
    // dma-buf if the driver has it, because that is the descriptor an unrelated
    // process can map. OPAQUE_FD is the fallback and is not equivalent: on
    // lavapipe it produced a descriptor that mapped to different memory.
    let kind = if driver.has_dma_buf {
        vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT
    } else {
        vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD
    };

    let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let type_index = driver
        .memory_type(u32::MAX, want)
        .ok_or(DriverError::Missing("a HOST_VISIBLE|HOST_COHERENT memory type"))?;

    let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(kind);
    let ai = vk::MemoryAllocateInfo::default()
        .allocation_size(len)
        .memory_type_index(type_index)
        .push_next(&mut export);
    let memory = unsafe { driver.device.allocate_memory(&ai, None) }
        .map_err(|e| DriverError::Vk("vkAllocateMemory(export)", e))?;

    let gi = vk::MemoryGetFdInfoKHR::default().memory(memory).handle_type(kind);
    let raw = match unsafe { ext.get_memory_fd(&gi) } {
        Ok(fd) => fd,
        Err(e) => {
            unsafe { driver.device.free_memory(memory, None) };
            return Err(DriverError::Vk("vkGetMemoryFdKHR", e));
        }
    };
    // SAFETY: `vkGetMemoryFdKHR` transfers ownership of the descriptor to the
    // application, which is us; wrapping it here makes that exactly once.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };

    let region = match SharedRegion::map_fd(std::os::fd::AsFd::as_fd(&fd), 0, len as usize) {
        Ok(r) => r,
        Err(e) => {
            unsafe { driver.device.free_memory(memory, None) };
            return Err(DriverError::Other(format!(
                "the driver exported a descriptor this process cannot mmap ({e}); \
                 the exporter does not implement mmap for this memory"
            )));
        }
    };

    Ok(Arena {
        provider: Provider::GpuExport,
        region: Arc::new(region),
        memory,
        len,
        memory_type: type_index,
        exported_fd: Some(fd),
    })
}

/// The app-allocated path: import the mapping we were handed.
///
/// design.md §5.4's `HostPointerImport`, with gfxstream's defensive memory-type
/// rewrite, and one deviation from §5.2 stated plainly:
///
/// **The whole arena is imported as one `VkDeviceMemory`, not one per slab, and
/// not lazily.** §5.2 makes a slab "a lazy-import unit and a failure unit, and
/// nothing else", and lazy import needs `svImportArenaSlab` — one of the
/// thirteen non-Vulkan commands of §6.3, none of which is generated and none of
/// which exists. Writing one was not what this stage was for, and §0.1 item 4
/// measured a single 2 GiB import as legal on the real target, so the whole
/// arena in one import is a smaller lie than a hand-rolled thirteenth command
/// would have been. What it costs is that the driver pins the configured arena
/// up front instead of tracking demand. What it does not cost is any of §5.3:
/// the `(memory, offset)` rewrite is unchanged, with one slab.
#[cfg(target_os = "linux")]
pub fn import_whole(driver: &Driver, region: Arc<SharedRegion>) -> Result<Arena> {
    let ext = driver
        .external_host
        .as_ref()
        .ok_or(DriverError::Missing("VK_EXT_external_memory_host"))?;
    let len = region.len() as u64;
    // SAFETY: the pointer is the base of a live mapping this process owns, and
    // it is handed to the driver rather than dereferenced here. The mapping
    // outlives the import because `Arena` holds the `Arc`.
    let ptr = unsafe { region.base_ptr() } as *mut std::ffi::c_void;

    let mut hp = vk::MemoryHostPointerPropertiesEXT::default();
    let r = unsafe {
        (ext.fp().get_memory_host_pointer_properties_ext)(
            driver.device.handle(),
            vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
            ptr,
            &mut hp,
        )
    };
    if r != vk::Result::SUCCESS {
        return Err(DriverError::Vk("vkGetMemoryHostPointerPropertiesEXT", r));
    }
    // The one place §5.4's "copy gfxstream's defensive memory-type rewrite"
    // lives. It has never fired on measured hardware and it costs one branch.
    let preferred = driver
        .memory_type(hp.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE)
        .unwrap_or(0);
    let type_index = sv_arena::import::choose_memory_type(preferred, hp.memory_type_bits)
        .map_err(|e| DriverError::Other(format!("{e}")))?;

    let mut imp = vk::ImportMemoryHostPointerInfoEXT::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)
        .host_pointer(ptr);
    let ai = vk::MemoryAllocateInfo::default()
        .allocation_size(len)
        .memory_type_index(type_index)
        .push_next(&mut imp);
    let memory = unsafe { driver.device.allocate_memory(&ai, None) }
        .map_err(|e| DriverError::Vk("vkAllocateMemory(import host pointer)", e))?;

    Ok(Arena {
        provider: Provider::AppMemfd,
        region,
        memory,
        len,
        memory_type: type_index,
        exported_fd: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability table above, as an assertion, so a driver moving between
    /// columns fails a test rather than a session.
    #[test]
    fn each_measured_driver_gets_the_provider_that_works_for_it() {
        assert_eq!(Provider::for_driver("radv"), Provider::GpuExport);
        assert_eq!(Provider::for_driver("nvidia"), Provider::GpuExport);
        assert_eq!(Provider::for_driver("lavapipe"), Provider::AppMemfd);
        assert_eq!(Provider::for_driver("intel"), Provider::AppMemfd);
        assert_eq!(Provider::for_driver("unknown"), Provider::AppMemfd);
    }

    /// Every driver has exactly one working column, so the two providers are
    /// complements. If this ever stops being true the choice becomes a policy
    /// rather than a capability, and that is a different design.
    #[test]
    fn the_two_providers_do_not_overlap_on_any_measured_driver() {
        for (key, import_path) in [
            ("radv", sv_arena::import::BackingKind::DmaBuf),
            ("nvidia", sv_arena::import::BackingKind::DmaBuf),
            ("lavapipe", sv_arena::import::BackingKind::Noop),
            ("intel", sv_arena::import::BackingKind::HostPointer),
        ] {
            let host_pointer_works = import_path != sv_arena::import::BackingKind::DmaBuf;
            assert_eq!(
                Provider::for_driver(key) == Provider::AppMemfd,
                host_pointer_works,
                "{key}: the provider and the import path must agree about whether \
                 a host pointer is acceptable"
            );
        }
    }
}
