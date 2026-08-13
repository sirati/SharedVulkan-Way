//! Where the arena comes from, and how the GPU side ends up with a
//! `VkDeviceMemory` over it.
//!
//! # The rule that overrides everything below
//!
//! > **No file descriptor or handle ever crosses between the app side and the
//! > GPU side.** A descriptor created *locally*, out of memory that side can
//! > already reach, is fine; a *name* crossing, which the peer opens itself, is
//! > the mechanism. — design.md §1a
//!
//! [`Provider::AppNamedShm`] obeys it. [`Provider::GpuExport`] does not, cannot
//! be made to — a dma-buf has no name — and survives only because of a measured
//! dead end recorded in design.md §0.7 and in `sv_transport::localfd`. Read the
//! next section knowing that its conclusion ("inverting the direction costs
//! nothing") was reached before the rule was stated, and that the rule does not
//! accept the trade.
//!
//! # The older rule, and the measurement that produced it
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
//! | NVIDIA RTX 5090 | **works** — corrected 2026-08-12, see below | **dma-buf, `mmap` works, coherent both directions** |
//! | RADV (Raphael iGPU) | refused (`INVALID_EXTERNAL_HANDLE`), structurally | **dma-buf, `mmap` works, coherent both directions** |
//! | lavapipe | works — it adopts the pointer | **no**: no dma-buf export, and its opaque-fd export is *not the same pages* |
//! | Intel, Windows guest | works (step 0, up to 2 GiB) | n/a — a descriptor cannot cross the VM boundary |
//!
//! **The NVIDIA row is a correction, and it matters for the reverse direction.**
//! The `OUT_OF_DEVICE_MEMORY` this table used to record was measured against
//! `gpu-probe --file /tmp/window`, and `/tmp` is ext4. `dmabuf-probe` isolated
//! the variable: NVIDIA imports a `MAP_SHARED` mapping of **shmem** — a memfd or
//! a tmpfs file — at 4 MiB and 128 MiB, `memoryTypeBits = 0xc`, coherent in both
//! directions, and refuses only a page-cache-backed regular file. The arena is a
//! memfd. So the discrete card *can* serve an app side that cannot receive a
//! descriptor, which is exactly the reverse-direction topology. RADV's refusal
//! is structural by contrast — libdrm passes `AMDGPU_GEM_USERPTR_ANONONLY`
//! unconditionally, so no choice of backing helps it.
//!
//! Two things in that table are worth stating out loud rather than leaving to be
//! read off it.
//!
//! **Every driver has at least one working column, and the choice is by
//! capability rather than policy.** Neither implementation can be deleted: the
//! `testhost` topology has to keep the memfd path, because the GPU side is
//! inside a VM, a descriptor cannot cross that boundary, and the arena there has
//! to be guest RAM the host locates. What changed with the NVIDIA correction is
//! that the columns are no longer *exclusive* — NVIDIA can do both — so
//! [`Provider::for_driver`] is now "the better one where there is a choice" and
//! [`Provider::for_driver_across_a_vm`] is "the one that is possible at all when
//! no descriptor can cross".
//!
//! **lavapipe is a worse oracle than §5.4 said, and in a new direction.** §5.4
//! warns that lavapipe "will cheerfully import a pointer every real driver
//! rejects". The export direction fails the other way: `vkGetMemoryFdKHR` with
//! `OPAQUE_FD` *succeeds*, the `mmap` of the result *succeeds*, and the bytes
//! are not the same memory. A probe that only checked "did the call return
//! `VK_SUCCESS`" would have reported this as working. That is why
//! `sv-export-probe` writes through both mappings and reads back through the
//! other one, and why the check is in both directions.

// `allocate_and_export` is Linux-only, because a dma-buf descriptor is. The
// host-pointer *import* is not — it takes a pointer and nothing else — and a
// Windows GPU side needs exactly it, over a section object rather than a memfd.
// So `import_whole` is built on both and only the export half is gated.
use crate::driver::{Driver, DriverError, Result};
use ash::vk;
use std::sync::Arc;
use sv_transport::region::SharedRegion;

/// How this session obtained the arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// The app side allocated it — a **named** object `sv-hostd` created, or
    /// guest RAM it located — and the GPU side imports it with
    /// `VK_EXT_external_memory_host`. design.md §5.4's `HostPointerImport`.
    ///
    /// The name is historical: since design.md §1a the object is *named* shared
    /// memory rather than a memfd, because a memfd can only be handed over as a
    /// descriptor and a descriptor may not cross the boundary. What the GPU side
    /// does with it is unchanged — it opens the name, maps it, and imports the
    /// pointer.
    AppNamedShm,
    /// The GPU side allocated it from the driver and exported a **descriptor**
    /// for the app side to map.
    ///
    /// # This provider violates design.md §1a and is kept for a measured reason
    ///
    /// A dma-buf has no name, so this is a same-machine-only mechanism: it
    /// cannot exist across a hypervisor in either direction. §1a says such a
    /// mechanism does not belong in the design, and the honest conclusion would
    /// be to delete it.
    ///
    /// It survives because on this host **RADV cannot obtain a named arena at
    /// all**, and the alternatives are closed for reasons that are facts about
    /// Linux rather than about this code — measured 2026-08-13, see
    /// design.md §0.7 and `sv_transport::localfd`. Every harness that judges
    /// this project against a real GPU runs through here.
    GpuExport,
}

impl Provider {
    /// Which provider a driver can actually serve, from the measurements above.
    ///
    /// Deliberately a function of the driver rather than a flag, for the same
    /// reason `sv_arena::import::path_for_driver` is: the arena has to know
    /// before it commits, and a wrong guess is discovered at the first
    /// allocation rather than at startup.
    /// **The rule-compliant provider wherever the driver allows one.** This used
    /// to prefer `GpuExport` for NVIDIA "because it needs no shmem-shaped arena
    /// and puts the pages in memory the driver chose". Those are optimisations;
    /// §1a is not, and NVIDIA was measured importing a `MAP_SHARED` mapping of
    /// shmem — which is exactly what a `/dev/shm` object is — so it takes the
    /// named path now. RADV is the only entry left on the wrong side of the rule
    /// and it is there because nothing else works, not because it is better.
    pub fn for_driver(key: &str) -> Provider {
        match key {
            // Refuses a shared host pointer whatever backs it
            // (`AMDGPU_GEM_USERPTR_ANONONLY`), and cannot wrap a named object in
            // a local dma-buf either, because a named object cannot be sealed
            // and udmabuf requires `F_SEAL_SHRINK`. design.md §0.7.
            "radv" => Provider::GpuExport,
            // Imports a shared mapping of shmem; measured 2026-08-12.
            "nvidia" => Provider::AppNamedShm,
            // Adopts a host pointer; its export is not the same pages.
            "lavapipe" => Provider::AppNamedShm,
            // The measured-working Windows target, and the conservative default:
            // an untried driver gets the path that obeys the rule.
            _ => Provider::AppNamedShm,
        }
    }

    /// Which provider is *possible* when the two sides are on opposite sides of
    /// a hypervisor.
    ///
    /// A descriptor cannot cross a VM boundary, so `GpuExport` is unavailable
    /// however capable the driver is. The arena must be memory both sides can
    /// map, which is guest RAM, which the GPU side imports as a host pointer.
    ///
    /// Returning `None` is the honest answer for RADV, and the reason is a
    /// *permission* rather than a design limit — which is a distinction §1a
    /// makes it worth being precise about. libdrm's
    /// `AMDGPU_GEM_USERPTR_ANONONLY` refuses every shared mapping, so the only
    /// route is a dma-buf; across a hypervisor that dma-buf is one the GPU side
    /// makes **locally**, with `/dev/udmabuf`, out of QEMU's own memfd, which it
    /// already maps. Nothing crosses, so the rule permits it — see design.md
    /// §0.7 — and it is blocked here only because `/dev/udmabuf` is `root:kvm`
    /// 0660 and this account is not in `kvm`. Saying so in the type stops a
    /// caller from discovering it as an `INVALID_EXTERNAL_HANDLE` three layers
    /// down.
    pub fn for_driver_across_a_vm(key: &str) -> Option<Provider> {
        match key {
            // Structural refusal of host pointers. The legal answer is a local
            // udmabuf over QEMU's sealed memfd; `/dev/udmabuf` is `root:kvm`
            // 0660 and this account is not in the group, measured 2026-08-13.
            "radv" | "amdgpu" => None,
            // Corrected 2026-08-12: imports a shared mapping of shmem, which is
            // what QEMU's `memory-backend-memfd` is.
            "nvidia" => Some(Provider::AppNamedShm),
            _ => Some(Provider::AppNamedShm),
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
    /// The external handle type the arena's `VkDeviceMemory` was allocated
    /// **with** — exported on one provider, imported on the other — or zero if
    /// this arena is not external memory at all.
    ///
    /// This is not diagnostic information. Vulkan requires that a buffer bound
    /// into memory that was exported or imported have been *created* naming at
    /// least one of the same handle types
    /// (`VUID-vkBindBufferMemory-memory-02726` for export, `-02727` for
    /// import), and this field is the only place that value exists. It is a
    /// property of the arena, decided by whichever of §5.4's providers built
    /// it, and it is why `vkCreateBuffer` is hand-written on this side: see
    /// `GpuServer::create_buffer`.
    pub external_handle_type: vk::ExternalMemoryHandleTypeFlags,
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
        external_handle_type: kind,
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
/// Import one slab of an already-mapped arena (design.md §5.2, `svImportArenaSlab`).
///
/// The lazy half of [`import_whole`]: the same call over a sub-range of the same
/// mapping. §5.4's claim that "`import` is granularity-blind, allocator-blind
/// and — up to 2 GiB — size-blind" is what makes this a parameter change rather
/// than a second implementation.
///
/// # What it is worth, measured
///
/// Importing the whole arena at startup costs about **70 ms per GiB** on this
/// machine past the first quarter-gigabyte: 17 ms to have a 64 MiB or 256 MiB
/// arena ready to serve, 72 ms at 1 GiB, 141 ms at 2 GiB. That is startup an
/// application pays for memory it may never touch, and it is what §5.2 means by
/// "lazy import bounds the number of driver imports and the driver's own
/// pinning". A 64 MiB slab is about 4.4 ms, paid once, when something first
/// allocates in it.
pub fn import_slab(
    driver: &Driver,
    region: &SharedRegion,
    arena_offset: u64,
    len: u64,
) -> Result<(vk::DeviceMemory, u32)> {
    let ext = driver
        .external_host
        .as_ref()
        .ok_or(DriverError::Missing("VK_EXT_external_memory_host"))?;
    if arena_offset.checked_add(len).is_none_or(|e| e > region.len() as u64) {
        return Err(DriverError::Other(format!(
            "a slab at {arena_offset}+{len} is outside the {}-byte arena",
            region.len()
        )));
    }
    // SAFETY: as `import_whole` — the base of a live mapping this process owns,
    // offset inside it by the check above, handed to the driver rather than
    // dereferenced. The mapping outlives the import because `Arena` holds the
    // `Arc` and slabs are never released while the process lives (§5.2).
    let ptr = unsafe { region.base_ptr().add(arena_offset as usize) } as *mut std::ffi::c_void;

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
        .map_err(|e| DriverError::Vk("vkAllocateMemory(import slab)", e))?;
    Ok((memory, type_index))
}

/// An arena that is mapped but not yet imported: every slab arrives on demand.
///
/// Nothing is asked of the driver here, which is the point — this is the
/// constructor whose whole content is the *absence* of the `vkAllocateMemory`
/// that [`import_whole`] makes.
pub fn map_lazy(region: Arc<SharedRegion>) -> Arena {
    let len = region.len() as u64;
    Arena {
        provider: Provider::AppNamedShm,
        region,
        memory: vk::DeviceMemory::null(),
        len,
        // Not known until the first slab is imported. It is only read for the
        // startup report and by `check_external_buffers_keep_the_arena`, which
        // runs after the first import on this path.
        memory_type: u32::MAX,
        external_handle_type: vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
        #[cfg(target_os = "linux")]
        exported_fd: None,
    }
}

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
        provider: Provider::AppNamedShm,
        region,
        memory,
        len,
        memory_type: type_index,
        // The mirror of the export path, and the reason the field is named for
        // "external" rather than for "export": an *imported* allocation carries
        // the same obligation on every buffer bound into it
        // (`VUID-vkBindBufferMemory-memory-02727`), naming the handle type it
        // was imported with. design.md §0.4 recorded this as a consequence of
        // the export provider; it is a consequence of the arena being external
        // memory at all, which both providers make it.
        external_handle_type: vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
        // The import path never exports anything, on either OS; the field only
        // exists where a descriptor does.
        #[cfg(target_os = "linux")]
        exported_fd: None,
    })
}

/// Does naming the arena's handle type on a buffer cost us the arena?
///
/// `GpuServer::create_buffer` adds a `VkExternalMemoryBufferCreateInfo` to every
/// forwarded buffer, because a buffer bound into the arena must carry one and
/// nothing at create time knows which buffers those will be. The price of doing
/// it unconditionally is that a driver is entitled to answer
/// `vkGetBufferMemoryRequirements` with a *narrower* `memoryTypeBits` for an
/// external-capable buffer — and if the narrowing excluded the arena's own
/// memory type, every buffer we created would become unbindable to the arena.
///
/// That is not a thing to discover as a failed bind in the middle of a frame,
/// and it is not a thing to assume either: it is one query, asked once, of the
/// driver that is actually here. Two identical buffers, one with the link and
/// one without, with the usage bits Zink asks for; the check is that the arena's
/// memory type survives.
///
/// Measured on RADV (RAPHAEL_MENDOCINO, Mesa 26.1.1): the mask is unchanged.
pub fn check_external_buffers_keep_the_arena(driver: &Driver, arena: &Arena) -> Result<()> {
    check_external_buffers(driver, arena.external_handle_type, arena.memory_type)
}

/// The same check against a memory type that is known separately.
///
/// Lazy import (§5.2) means the arena's memory type does not exist until the
/// first `svImportArenaSlab` succeeds, so on that path this runs *then* rather
/// than at startup. Late is much better than not at all: the failure it catches
/// — a driver that narrows `memoryTypeBits` for external-capable buffers past
/// the arena's own type — makes every subsequent `vkBindBufferMemory` illegal,
/// and diagnosing that from `VUID-vkBindBufferMemory-memory-01035` is an
/// afternoon.
pub fn check_external_buffers(
    driver: &Driver,
    kind: vk::ExternalMemoryHandleTypeFlags,
    memory_type: u32,
) -> Result<()> {
    if kind.is_empty() || memory_type == u32::MAX {
        return Ok(());
    }
    // Named `arena` so the body below reads unchanged; it is only the two fields
    // the check actually consults.
    struct ProbeArena {
        memory_type: u32,
    }
    let arena = ProbeArena { memory_type };
    // Everything Zink binds into host-visible memory: vertex, index, uniform,
    // storage and staging.
    let usage = vk::BufferUsageFlags::TRANSFER_SRC
        | vk::BufferUsageFlags::TRANSFER_DST
        | vk::BufferUsageFlags::UNIFORM_BUFFER
        | vk::BufferUsageFlags::STORAGE_BUFFER
        | vk::BufferUsageFlags::INDEX_BUFFER
        | vk::BufferUsageFlags::VERTEX_BUFFER;
    let bits = |external: bool| -> Result<u32> {
        let mut ext = vk::ExternalMemoryBufferCreateInfo::default().handle_types(kind);
        let mut ci = vk::BufferCreateInfo::default()
            .size(4096)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        if external {
            ci = ci.push_next(&mut ext);
        }
        let b = unsafe { driver.device.create_buffer(&ci, None) }
            .map_err(|e| DriverError::Vk("vkCreateBuffer(probe)", e))?;
        let req = unsafe { driver.device.get_buffer_memory_requirements(b) };
        unsafe { driver.device.destroy_buffer(b, None) };
        Ok(req.memory_type_bits)
    };
    let plain = bits(false)?;
    let external = bits(true)?;
    if external & (1 << arena.memory_type) == 0 {
        return Err(DriverError::Other(format!(
            "naming {kind:?} on a buffer narrows memoryTypeBits from {plain:#x} to {external:#x}, \
             which excludes the arena's memory type {}; buffers could be created or bound to the \
             arena but not both",
            arena.memory_type
        )));
    }
    if plain != external {
        eprintln!(
            "[gpu] external-capable buffers have memoryTypeBits {external:#x} rather than \
             {plain:#x}; the arena's type {} survives",
            arena.memory_type
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability table above, as an assertion, so a driver moving between
    /// columns fails a test rather than a session.
    #[test]
    fn each_measured_driver_gets_the_provider_that_works_for_it() {
        // The only driver on the wrong side of design.md §1a, and the only one
        // with no alternative: §0.7.
        assert_eq!(Provider::for_driver("radv"), Provider::GpuExport);
        // Moved to the named path when the rule was stated: it can, so it must.
        assert_eq!(Provider::for_driver("nvidia"), Provider::AppNamedShm);
        assert_eq!(Provider::for_driver("lavapipe"), Provider::AppNamedShm);
        assert_eq!(Provider::for_driver("intel"), Provider::AppNamedShm);
        assert_eq!(Provider::for_driver("unknown"), Provider::AppNamedShm);
    }

    /// The two tables have to agree about whether a host pointer is acceptable,
    /// because one of them is what the arena commits to and the other is what
    /// the import actually does. They are separate crates and drift silently.
    #[test]
    fn the_provider_and_the_import_path_agree_about_host_pointers() {
        for (key, import_path) in [
            ("radv", sv_arena::import::BackingKind::DmaBuf),
            ("nvidia", sv_arena::import::BackingKind::HostPointer),
            ("lavapipe", sv_arena::import::BackingKind::Noop),
            ("intel", sv_arena::import::BackingKind::HostPointer),
        ] {
            assert_eq!(
                sv_arena::import::path_for_driver(key),
                import_path,
                "{key}: sv-arena's import table moved"
            );
            let host_pointer_works = import_path != sv_arena::import::BackingKind::DmaBuf;
            assert_eq!(
                Provider::for_driver_across_a_vm(key).is_some(),
                host_pointer_works,
                "{key}: across a VM only a host-pointer import is possible, so the two \
                 tables must agree about whether one works"
            );
        }
    }

    /// The reverse direction, as a table. `None` for RADV is the finding, not an
    /// omission: it is blocked on `/dev/udmabuf` being `root:kvm` 0660, and
    /// nothing in this codebase can change that.
    #[test]
    fn across_a_vm_only_a_host_pointer_import_is_possible() {
        assert_eq!(Provider::for_driver_across_a_vm("nvidia"), Some(Provider::AppNamedShm));
        assert_eq!(Provider::for_driver_across_a_vm("intel"), Some(Provider::AppNamedShm));
        assert_eq!(Provider::for_driver_across_a_vm("lavapipe"), Some(Provider::AppNamedShm));
        assert_eq!(Provider::for_driver_across_a_vm("radv"), None);
        // …even though on one machine RADV is perfectly capable, which is the
        // point: the topology removes an option the driver has.
        assert_eq!(Provider::for_driver("radv"), Provider::GpuExport);
    }
}
