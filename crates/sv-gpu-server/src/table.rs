//! The GPU side's object table: app-assigned id → driver handle.
//!
//! docs/design.md §6.4: "Handles crossing the boundary are **always table indices,
//! never pointers** (R3)." The app side allocates ids from a counter and encodes
//! the id it chose, so object creation costs no round trip; this is the table
//! that turns one of those ids into something a driver will accept.
//!
//! # A `SlotMap` is the wrong container, and that is a correction to §6.4
//!
//! §6.4 specifies "a `SlotMap<Key, HandleInfo>` where `HandleInfo` holds the
//! driver handle, the type tag, the dispatch pointer, ordering state and
//! deferred-destroy state — which is gfxstream's boxed-handle manager, and in
//! Rust most of it comes free".
//!
//! A slot map allocates its own keys. That is exactly what gfxstream needs,
//! because in gfxstream the *host* mints the boxed handle and hands it back. It
//! is exactly what we cannot use, because in Venus's design — which §6.4 adopts
//! in the same paragraph — the **app side** mints the id and the GPU side is
//! told what it is. A slot map would need a second map from app id to slot,
//! which is the map on its own with an extra indirection in front of it. The two
//! halves of §6.4 are each right and they do not compose; the id-assignment half
//! wins, because it is the one that removes a round trip.
//!
//! So this is a `HashMap<u64, Object>`, and the "most of it comes free" claim
//! survives intact: what gfxstream needs a manager for, Rust gets from the map.
//!
//! # One map, not one per type
//!
//! `ash` gives every Vulkan object a distinct Rust type, so a table per type
//! would be twenty maps. Instead the table stores the driver's own 64-bit handle
//! (`vk::Handle::as_raw`) plus a type tag, and hands back a typed handle on
//! lookup. The type tag is checked on every lookup, so asking for a `VkBuffer`
//! by an id that names a `VkImage` is a named error and not a driver crash —
//! which is a check gfxstream's boxed handles get for free and a bare
//! `HashMap<u64, u64>` would have thrown away.

use ash::vk;
// `Handle` is what gives every Vulkan object `as_raw`/`from_raw`. It has to be
// in scope for the one-map design to work at all: the map stores the 64-bit
// handle and hands back a typed one.
use ash::vk::Handle as _;
use std::collections::HashMap;
use sv_proto::HandleType;

/// One live object.
#[derive(Debug, Clone, Copy)]
pub struct Object {
    /// `vk::Handle::as_raw` of the driver's own handle.
    pub raw: u64,
    pub ty: HandleType,
}

/// What can go wrong looking something up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableError {
    /// No object with this id.
    ///
    /// # This used to name a cause, and the cause was wrong three times
    ///
    /// It read *"almost always the app side sending a handle it created on a
    /// ring the GPU side has not drained yet, which is R22 — a missing
    /// `RingBarrier` — rather than a bad handle"*, and the message said so too.
    /// Every occurrence traced since has been something else:
    ///
    /// * §0.16 item 2 — the `VkBuffer` shape was an out-of-bounds read of the
    ///   application's own memory.
    /// * §0.22 item 1 — the `VkDeviceMemory` shape was a missing pass.
    /// * §0.22 item 6 — the `VkPipelineLayout` shape is a **lifetime** bug:
    ///   `VK_KHR_maintenance4` lets an application destroy a pipeline layout
    ///   once the command buffer leaves the recording state, while §6.5 resolves
    ///   blob ids when the blob is replayed, so the destroy legitimately arrives
    ///   first. See §0.23.
    ///
    /// Three for three, and none was a barrier. A diagnostic that names one
    /// cause does not merely fail to help — it sends every reader to the same
    /// wrong place, and it did, repeatedly. So the message now says what is
    /// *observed* and lists what has actually been behind it, in the order
    /// those were found.
    Unknown { id: u64, want: HandleType },
    /// An id that names an object of a different type.
    WrongType { id: u64, want: HandleType, found: HandleType },
    /// A `(memory, offset)` pair that does not land inside the arena.
    NotInArena { memory: u64, offset: u64 },
}

impl std::fmt::Display for TableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TableError::Unknown { id, want } => write!(
                f,
                "no {} with id {id} — it was never created, was already destroyed, \
                 or was destroyed while a recorded blob still named it (design.md \
                 §0.23); it may also be a wrong id read out of application memory. \
                 Check the last create/destroy of this id before assuming a \
                 missing RingBarrier",
                want.name()
            ),
            TableError::WrongType { id, want, found } => {
                write!(f, "id {id} is a {}, not a {}", found.name(), want.name())
            }
            TableError::NotInArena { memory, offset } => {
                write!(f, "memory {memory} + {offset} is not inside the arena")
            }
        }
    }
}

impl std::error::Error for TableError {}

/// A `VkDeviceMemory` as the GPU side holds it.
///
/// The arena is one allocation and every app-side arena allocation is a
/// sub-range of it, so an app-side memory id resolves to *the same*
/// `VkDeviceMemory` with a different base. Non-arena memory — heap 0 of §9.1,
/// which is never host-visible — is its own allocation with base zero.
#[derive(Debug, Clone, Copy)]
pub struct MemoryRange {
    pub memory: vk::DeviceMemory,
    pub base: u64,
    pub len: u64,
}

/// Every object this session knows about.
pub struct Objects {
    map: HashMap<u64, Object>,
    memory: HashMap<u64, MemoryRange>,
    /// The whole-arena allocation, under [`crate::ARENA_MEMORY_ID`].
    arena: Option<MemoryRange>,
    /// One entry per slab; `None` until `svImportArenaSlab` names it (§5.2).
    slabs: Vec<Option<vk::DeviceMemory>>,
    slab_size: u64,
}

impl Default for Objects {
    fn default() -> Self {
        Objects::new()
    }
}

impl Objects {
    pub fn new() -> Objects {
        Objects {
            map: HashMap::new(),
            memory: HashMap::new(),
            arena: None,
            slabs: Vec::new(),
            slab_size: 1,
        }
    }

    /// Publish the arena as **one** imported slab covering all of it.
    ///
    /// This is what the GPU-allocated provider does and always will: there the
    /// arena *is* a `vkAllocateMemory` this process made and then exported, so
    /// there is nothing to import lazily — the memory exists before the app side
    /// has attached, let alone allocated. docs/design.md §5.2's lazy import is a
    /// property of the *import* providers, and [`Objects::set_lazy_arena`] is
    /// the other half.
    pub fn set_arena(&mut self, memory: vk::DeviceMemory, len: u64) {
        let r = MemoryRange { memory, base: 0, len };
        self.arena = Some(r);
        self.slab_size = len.max(1);
        self.slabs = vec![Some(memory)];
        self.memory.insert(crate::ARENA_MEMORY_ID, r);
    }

    /// Publish an arena whose slabs will arrive one `svImportArenaSlab` at a
    /// time (docs/design.md §5.2).
    ///
    /// > Slabs are **imported** lazily. When the app side needs a slab it picks
    /// > a free region of the arena, writes a slab-directory entry, and sends
    /// > `svImportArenaSlab`. The GPU side imports it and replies.
    pub fn set_lazy_arena(&mut self, len: u64, slab_size: u64) {
        self.arena = Some(MemoryRange { memory: vk::DeviceMemory::null(), base: 0, len });
        self.slab_size = slab_size.max(1);
        self.slabs = vec![None; len.div_ceil(slab_size.max(1)) as usize];
        self.memory.insert(
            crate::ARENA_MEMORY_ID,
            MemoryRange { memory: vk::DeviceMemory::null(), base: 0, len },
        );
    }

    /// Has any slab been imported yet? The first one is where the arena's
    /// memory type comes into existence on the lazy path.
    pub fn any_slab_imported(&self) -> bool {
        self.slabs.iter().any(|s| s.is_some())
    }

    /// Record a slab the GPU side has just imported.
    pub fn set_slab(&mut self, slab: u32, memory: vk::DeviceMemory) {
        if let Some(s) = self.slabs.get_mut(slab as usize) {
            *s = Some(memory);
        }
    }

    /// The `VkDeviceMemory` covering an arena offset, and the offset inside it.
    ///
    /// `None` means the slab has not been imported, which is a protocol error
    /// rather than a resource shortage: the app side must not use a slab it has
    /// not asked for.
    pub fn arena_slab(&self, offset: u64) -> Option<(vk::DeviceMemory, u64)> {
        let slab = (offset / self.slab_size) as usize;
        let m = (*self.slabs.get(slab)?)?;
        Some((m, offset % self.slab_size))
    }

    pub fn arena(&self) -> Option<MemoryRange> {
        self.arena
    }

    /// The arena offset of `offset` bytes into the driver allocation `memory`,
    /// if `memory` is one of this session's arena slabs — the inverse of
    /// [`Objects::arena_slab`], for `sv_gpu_server::video_log`, which has a
    /// driver handle and wants the bytes behind it.
    pub fn arena_offset_of(&self, memory: u64, offset: u64) -> Option<u64> {
        if memory == 0 || offset >= self.slab_size {
            return None;
        }
        let slab = self.slabs.iter().position(|s| s.is_some_and(|m| m.as_raw() == memory))?;
        Some(slab as u64 * self.slab_size + offset)
    }

    pub fn slab_size(&self) -> u64 {
        self.slab_size
    }

    /// Record `id` as naming `handle`.
    pub fn insert<H: vk::Handle>(&mut self, id: u64, ty: HandleType, handle: H) {
        if id != 0 {
            self.map.insert(id, Object { raw: handle.as_raw(), ty });
        }
    }

    /// Record `id` as naming the raw 64-bit handle `raw`.
    ///
    /// The generated forwarding pass has already turned the driver's answer into
    /// a `u64` — a dispatchable handle is a pointer and a non-dispatchable one is
    /// a number, and both fit — so it has nothing typed to hand over. The type
    /// tag comes from the registry rather than from the value, which is the same
    /// place [`insert`](Self::insert) gets it.
    pub fn insert_raw(&mut self, id: u64, ty: HandleType, raw: u64) {
        if id != 0 {
            self.map.insert(id, Object { raw, ty });
        }
    }

    /// The driver handle behind `id`, whatever type it is.
    ///
    /// For the one site where the *caller* states the type and the registry does
    /// not: `VkDebugUtilsObjectNameInfoEXT::objectHandle`, whose type comes from
    /// its `objectType` sibling. Checking the tag here would mean maintaining a
    /// `VkObjectType` → [`HandleType`] table to check something the caller has
    /// already told the driver, and getting *that* wrong would refuse valid
    /// calls. The driver checks it for real.
    pub fn raw(&self, id: u64) -> Option<u64> {
        if id == 0 {
            return Some(0);
        }
        self.map.get(&id).map(|o| o.raw)
    }

    /// Look `id` up as a handle of type `H`.
    ///
    /// `VK_NULL_HANDLE` is 0 on the wire and 0 in the driver, and passing it
    /// through unchanged is correct: every command that accepts a null handle
    /// accepts it on both sides, and refusing here would break
    /// `vkCmdBindDescriptorSets` with no dynamic offsets before it started.
    pub fn get<H: vk::Handle>(&self, ty: HandleType, id: u64) -> Result<H, TableError> {
        if id == 0 {
            return Ok(H::from_raw(0));
        }
        match self.map.get(&id) {
            Some(o) if o.ty == ty => Ok(H::from_raw(o.raw)),
            Some(o) => Err(TableError::WrongType { id, want: ty, found: o.ty }),
            None => Err(TableError::Unknown { id, want: ty }),
        }
    }

    /// Forget `id`, and say whether it was there.
    pub fn remove(&mut self, id: u64) -> Option<Object> {
        self.memory.remove(&id);
        self.map.remove(&id)
    }

    pub fn insert_memory(&mut self, id: u64, r: MemoryRange) {
        self.memory.insert(id, r);
        self.map.insert(id, Object { raw: r.memory.as_raw(), ty: HandleType::VkDeviceMemory });
    }

    pub fn memory(&self, id: u64) -> Option<MemoryRange> {
        self.memory.get(&id).copied()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Destroy what the application left behind, children before parents, on a
    /// device it can no longer reach (waycompose-design.md §87).
    ///
    /// The first Firefox on the ivshmem path measured why this exists: its
    /// `glxtest` process exits with a buffer, eight command pools, four set
    /// layouts, a pipeline layout, two samplers and a semaphore still alive, so
    /// every Firefox start would cost a session slot for the life of the GPU
    /// process. Destroying a valid object on an idle device is ordinary Vulkan —
    /// the application would have made exactly these calls — so the slot can
    /// be reused and the device destroyed with no children, as the
    /// specification requires.
    ///
    /// Only types whose destroy call is core Vulkan are handled. If anything
    /// else is left (a video session, an acceleration structure, …) nothing is
    /// destroyed and the types are returned, and the caller keeps the device.
    /// `arena` is the session's arena import: memory objects that are slices of
    /// it are not the application's to free.
    ///
    /// # Safety
    ///
    /// `device` must be the device every handle here was created on, idle
    /// (`vkDeviceWaitIdle` returned), and no thread may use any of them again.
    pub unsafe fn destroy_leftovers(
        &mut self,
        device: &ash::Device,
        arena: vk::DeviceMemory,
    ) -> Result<usize, Vec<String>> {
        use HandleType as H;
        // Dependency order: what uses a thing goes before the thing.
        const ORDER: &[HandleType] = &[
            H::VkPipeline,
            H::VkPipelineLayout,
            H::VkPipelineCache,
            H::VkDescriptorPool,
            H::VkDescriptorUpdateTemplate,
            H::VkDescriptorSetLayout,
            H::VkFramebuffer,
            H::VkRenderPass,
            H::VkImageView,
            H::VkBufferView,
            H::VkSampler,
            H::VkSamplerYcbcrConversion,
            H::VkImage,
            H::VkBuffer,
            H::VkShaderModule,
            H::VkCommandPool,
            H::VkQueryPool,
            H::VkEvent,
            H::VkFence,
            H::VkSemaphore,
            H::VkDeviceMemory,
        ];
        const OWNED_ELSEWHERE: &[HandleType] = &[
            H::VkInstance,
            H::VkPhysicalDevice,
            H::VkDevice,
            H::VkQueue,
            H::VkCommandBuffer,
            H::VkDescriptorSet,
        ];
        let unhandled: Vec<String> = self
            .map
            .values()
            .filter(|o| !OWNED_ELSEWHERE.contains(&o.ty) && !ORDER.contains(&o.ty))
            .map(|o| format!("{:?}", o.ty))
            .collect();
        if !unhandled.is_empty() {
            return Err(unhandled);
        }
        let mut n = 0usize;
        for &ty in ORDER {
            let raws: Vec<u64> = self.map.values().filter(|o| o.ty == ty).map(|o| o.raw).collect();
            for raw in raws {
                // SAFETY: the caller's contract; each handle was created on
                // `device` by this session and is destroyed exactly once,
                // because the table is cleared below.
                unsafe {
                    match ty {
                        H::VkPipeline => device.destroy_pipeline(vk::Pipeline::from_raw(raw), None),
                        H::VkPipelineLayout => {
                            device.destroy_pipeline_layout(vk::PipelineLayout::from_raw(raw), None)
                        }
                        H::VkPipelineCache => {
                            device.destroy_pipeline_cache(vk::PipelineCache::from_raw(raw), None)
                        }
                        H::VkDescriptorPool => {
                            device.destroy_descriptor_pool(vk::DescriptorPool::from_raw(raw), None)
                        }
                        H::VkDescriptorUpdateTemplate => device.destroy_descriptor_update_template(
                            vk::DescriptorUpdateTemplate::from_raw(raw),
                            None,
                        ),
                        H::VkDescriptorSetLayout => device.destroy_descriptor_set_layout(
                            vk::DescriptorSetLayout::from_raw(raw),
                            None,
                        ),
                        H::VkFramebuffer => device.destroy_framebuffer(vk::Framebuffer::from_raw(raw), None),
                        H::VkRenderPass => device.destroy_render_pass(vk::RenderPass::from_raw(raw), None),
                        H::VkImageView => device.destroy_image_view(vk::ImageView::from_raw(raw), None),
                        H::VkBufferView => device.destroy_buffer_view(vk::BufferView::from_raw(raw), None),
                        H::VkSampler => device.destroy_sampler(vk::Sampler::from_raw(raw), None),
                        H::VkSamplerYcbcrConversion => device.destroy_sampler_ycbcr_conversion(
                            vk::SamplerYcbcrConversion::from_raw(raw),
                            None,
                        ),
                        H::VkImage => device.destroy_image(vk::Image::from_raw(raw), None),
                        H::VkBuffer => device.destroy_buffer(vk::Buffer::from_raw(raw), None),
                        H::VkShaderModule => device.destroy_shader_module(vk::ShaderModule::from_raw(raw), None),
                        H::VkCommandPool => device.destroy_command_pool(vk::CommandPool::from_raw(raw), None),
                        H::VkQueryPool => device.destroy_query_pool(vk::QueryPool::from_raw(raw), None),
                        H::VkEvent => device.destroy_event(vk::Event::from_raw(raw), None),
                        H::VkFence => device.destroy_fence(vk::Fence::from_raw(raw), None),
                        H::VkSemaphore => device.destroy_semaphore(vk::Semaphore::from_raw(raw), None),
                        H::VkDeviceMemory => {
                            // A host-visible allocation is a slice of the
                            // arena and names the arena's memory; that one is
                            // the session's, freed by the caller.
                            if raw == arena.as_raw() {
                                continue;
                            }
                            device.free_memory(vk::DeviceMemory::from_raw(raw), None)
                        }
                        _ => unreachable!("only ORDER's types reach here"),
                    }
                }
                n += 1;
            }
        }
        self.map.retain(|_, o| OWNED_ELSEWHERE.contains(&o.ty));
        self.memory.retain(|id, _| *id == crate::ARENA_MEMORY_ID);
        Ok(n)
    }

    /// [`Objects::leftovers`] one by one: `(type, app id, driver handle)`.
    pub fn leftover_handles(&self) -> Vec<(String, u64, u64)> {
        let mut v: Vec<(String, u64, u64)> = self
            .map
            .iter()
            .filter(|(_, o)| {
                !matches!(
                    o.ty,
                    HandleType::VkInstance
                        | HandleType::VkPhysicalDevice
                        | HandleType::VkDevice
                        | HandleType::VkQueue
                        | HandleType::VkCommandBuffer
                        | HandleType::VkDescriptorSet
                )
            })
            .map(|(id, o)| (format!("{:?}", o.ty), *id, o.raw))
            .collect();
        v.sort();
        v
    }

    /// What the application created and did not destroy, by type
    /// (waycompose-design.md §87).
    ///
    /// Leaves out what this side owns rather than the application — the
    /// instance, physical device, device and queues, which `create_*` hand out
    /// and `destroy_*` deliberately do not remove — and what Vulkan frees with
    /// its parent: command buffers with their pool, descriptor sets with
    /// theirs. Everything else here is an object the driver still holds for a
    /// session that has ended, and a process that serves another session on
    /// the same arena slot must know that before it destroys the device under
    /// it.
    pub fn leftovers(&self) -> std::collections::BTreeMap<String, usize> {
        let mut out = std::collections::BTreeMap::new();
        for o in self.map.values() {
            if matches!(
                o.ty,
                HandleType::VkInstance
                    | HandleType::VkPhysicalDevice
                    | HandleType::VkDevice
                    | HandleType::VkQueue
                    | HandleType::VkCommandBuffer
                    | HandleType::VkDescriptorSet
            ) {
                continue;
            }
            *out.entry(format!("{:?}", o.ty)).or_insert(0) += 1;
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// The GPU side's `(memory, offset)` rewrite.
///
/// # Where this belongs, and why design.md puts it in the wrong place
///
/// §5.3 says the rewrite is done by "the generated `transform` pass" on the app
/// side, turning the app's memory id into "the slab's GPU-side handle". It
/// cannot be, and the reason is §6.4 two sections later: **the app side does not
/// know any GPU-side handle, and the whole design depends on it not knowing
/// one.** Ids are app-assigned so that creation costs no round trip; a rewrite
/// that substituted a driver handle would have to have asked for it.
///
/// What the app side *can* do — and what `sv-icd` does — is rewrite its own
/// allocation coordinates into *arena* coordinates: `(memory, offset)` becomes
/// `(ARENA_MEMORY_ID, arena_offset + offset)`. That carries no driver handle and
/// no address, only a well-known id and a number the app side already owns. The
/// second half of the rewrite, arena offset to `VkDeviceMemory` plus offset,
/// happens here, on the side that has the table.
///
/// So the pass is split rather than moved, and the generated
/// `sv_proto::MemoryTransform` walkers serve the app-side half. The GPU-side
/// half is this function, and it is one addition and one bounds check.
pub fn resolve_memory(
    objects: &Objects,
    memory: u64,
    offset: u64,
) -> Result<(vk::DeviceMemory, u64), TableError> {
    let r = objects.memory(memory).ok_or(TableError::Unknown {
        id: memory,
        want: HandleType::VkDeviceMemory,
    })?;
    let at = r.base.checked_add(offset).ok_or(TableError::NotInArena { memory, offset })?;
    // R2, and the realistic thing it catches is our own arithmetic: an app side
    // whose arena offset was computed against a different slab size.
    if at > r.base.saturating_add(r.len) {
        return Err(TableError::NotInArena { memory, offset });
    }
    if memory == crate::ARENA_MEMORY_ID {
        // §5.3 makes an app-side allocation "a sub-range of *one* slab", so this
        // never has to span two — and a lookup that lands in a slab nobody
        // imported is a protocol error rather than a bounds error, because the
        // app side is only supposed to allocate out of slabs it asked for.
        return objects.arena_slab(at).ok_or(TableError::NotInArena { memory, offset });
    }
    Ok((r.memory, at))
}

/// `sv_proto::HandleMapper` over an [`Objects`].
///
/// The generated walkers consult this to replace every id inside a struct with
/// its driver handle. A miss is a wire error rather than a panic, so a packet
/// that arrived before the object it names fails that packet and names the
/// object, instead of taking the session down.
pub struct Mapper<'a> {
    pub objects: &'a Objects,
    /// The first id that could not be resolved, kept so the caller can put it in
    /// the ring's `fatal_msg` rather than losing it to a generic wire error.
    pub missed: Option<TableError>,
}

impl<'a> Mapper<'a> {
    pub fn new(objects: &'a Objects) -> Mapper<'a> {
        Mapper { objects, missed: None }
    }
}

impl sv_proto::HandleMapper for Mapper<'_> {
    fn map(&mut self, ty: HandleType, id: u64) -> sv_wire::Result<u64> {
        if id == 0 {
            return Ok(0);
        }
        match self.objects.get::<vk::Buffer>(ty, id) {
            // Every non-dispatchable Vulkan handle is a `u64` and `ash` models
            // them all through `vk::Handle`, so borrowing `vk::Buffer` as the
            // carrier here is a cast and not a claim about the type — the type
            // was already checked, by `ty`, inside `get`.
            Ok(h) => Ok(h.as_raw()),
            Err(e) => {
                self.missed.get_or_insert(e);
                // The opcode is not available this deep in a struct walk;
                // `UnknownCommand` with the handle type is the closest the wire
                // error set gets, and `missed` carries the real diagnosis.
                Err(sv_wire::WireError::UnknownCommand { opcode: ty as u32 })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    /// §87, on a real driver: every type `destroy_leftovers` handles, made the
    /// way an application makes it and left behind, is destroyed on an idle
    /// device, the arena's memory is not, and an unhandled type makes it touch
    /// nothing. Run against whatever `Driver::open(0)` finds — lavapipe in the
    /// dev shell, RADV with `VK_ICD_FILENAMES` naming it — and skipped where
    /// there is no Vulkan loader at all.
    #[test]
    fn leftovers_of_every_handled_type_are_destroyed_on_a_real_driver() {
        use crate::driver::Driver;
        let driver = match Driver::open(0) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("no Vulkan driver here ({e}); skipped");
                return;
            }
        };
        let d = &driver.device;
        let mut objs = Objects::new();
        let mut id = 100u64;
        let mut add = |o: &mut Objects, ty: HandleType, raw: u64| {
            id += 1;
            o.insert_raw(id, ty, raw);
        };
        let local = |bits: u32| driver.memory_type(bits, vk::MemoryPropertyFlags::empty()).unwrap();
        unsafe {
            // The arena stand-in, which must survive.
            let arena = d
                .allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(1 << 16).memory_type_index(local(u32::MAX)), None)
                .unwrap();
            add(&mut objs, HandleType::VkDeviceMemory, arena.as_raw());
            add(&mut objs, HandleType::VkDevice, d.handle().as_raw());
            add(&mut objs, HandleType::VkInstance, driver.instance.handle().as_raw());

            let buffer = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(1 << 16)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::UNIFORM_TEXEL_BUFFER),
                    None,
                )
                .unwrap();
            let req = d.get_buffer_memory_requirements(buffer);
            let mem = d
                .allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(local(req.memory_type_bits)), None)
                .unwrap();
            d.bind_buffer_memory(buffer, mem, 0).unwrap();
            add(&mut objs, HandleType::VkBuffer, buffer.as_raw());
            add(&mut objs, HandleType::VkDeviceMemory, mem.as_raw());
            let bview = d
                .create_buffer_view(
                    &vk::BufferViewCreateInfo::default().buffer(buffer).format(vk::Format::R32_UINT).range(vk::WHOLE_SIZE),
                    None,
                )
                .unwrap();
            add(&mut objs, HandleType::VkBufferView, bview.as_raw());

            let image = d
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .extent(vk::Extent3D { width: 16, height: 16, depth: 1 })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::COLOR_ATTACHMENT),
                    None,
                )
                .unwrap();
            let ireq = d.get_image_memory_requirements(image);
            let imem = d
                .allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(ireq.size).memory_type_index(local(ireq.memory_type_bits)), None)
                .unwrap();
            d.bind_image_memory(image, imem, 0).unwrap();
            add(&mut objs, HandleType::VkImage, image.as_raw());
            add(&mut objs, HandleType::VkDeviceMemory, imem.as_raw());
            let iview = d
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            level_count: 1,
                            layer_count: 1,
                            ..Default::default()
                        }),
                    None,
                )
                .unwrap();
            add(&mut objs, HandleType::VkImageView, iview.as_raw());
            let sampler = d.create_sampler(&vk::SamplerCreateInfo::default(), None).unwrap();
            add(&mut objs, HandleType::VkSampler, sampler.as_raw());

            let attach = [vk::AttachmentDescription::default()
                .format(vk::Format::R8G8B8A8_UNORM)
                .samples(vk::SampleCountFlags::TYPE_1)
                .final_layout(vk::ImageLayout::GENERAL)];
            let cref = [vk::AttachmentReference { attachment: 0, layout: vk::ImageLayout::GENERAL }];
            let sub = [vk::SubpassDescription::default().color_attachments(&cref)];
            let rp = d
                .create_render_pass(&vk::RenderPassCreateInfo::default().attachments(&attach).subpasses(&sub), None)
                .unwrap();
            add(&mut objs, HandleType::VkRenderPass, rp.as_raw());
            let views = [iview];
            let fb = d
                .create_framebuffer(
                    &vk::FramebufferCreateInfo::default().render_pass(rp).attachments(&views).width(16).height(16).layers(1),
                    None,
                )
                .unwrap();
            add(&mut objs, HandleType::VkFramebuffer, fb.as_raw());

            let bindings = [0, 1].map(|b| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
            });
            let dsl = d
                .create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None)
                .unwrap();
            add(&mut objs, HandleType::VkDescriptorSetLayout, dsl.as_raw());
            let sizes = [vk::DescriptorPoolSize::default().ty(vk::DescriptorType::STORAGE_BUFFER).descriptor_count(2)];
            let dpool = d
                .create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&sizes), None)
                .unwrap();
            add(&mut objs, HandleType::VkDescriptorPool, dpool.as_raw());
            let set = d
                .allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(dpool).set_layouts(&[dsl]))
                .unwrap()[0];
            add(&mut objs, HandleType::VkDescriptorSet, set.as_raw());
            let layouts = [dsl];
            let pl = d
                .create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts), None)
                .unwrap();
            add(&mut objs, HandleType::VkPipelineLayout, pl.as_raw());
            let cache = d.create_pipeline_cache(&vk::PipelineCacheCreateInfo::default(), None).unwrap();
            add(&mut objs, HandleType::VkPipelineCache, cache.as_raw());
            if let Ok(code) = crate::workload::spv_words() {
                let sm = d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None).unwrap();
                add(&mut objs, HandleType::VkShaderModule, sm.as_raw());
                let stage = vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::COMPUTE)
                    .module(sm)
                    .name(c"main");
                let p = d
                    .create_compute_pipelines(cache, &[vk::ComputePipelineCreateInfo::default().stage(stage).layout(pl)], None)
                    .unwrap()[0];
                add(&mut objs, HandleType::VkPipeline, p.as_raw());
            }

            let cpool = d
                .create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(driver.queue_family), None)
                .unwrap();
            add(&mut objs, HandleType::VkCommandPool, cpool.as_raw());
            let cb = d
                .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(cpool).command_buffer_count(1))
                .unwrap()[0];
            add(&mut objs, HandleType::VkCommandBuffer, cb.as_raw());
            let qp = d
                .create_query_pool(&vk::QueryPoolCreateInfo::default().query_type(vk::QueryType::OCCLUSION).query_count(1), None)
                .unwrap();
            add(&mut objs, HandleType::VkQueryPool, qp.as_raw());
            add(&mut objs, HandleType::VkEvent, d.create_event(&vk::EventCreateInfo::default(), None).unwrap().as_raw());
            add(&mut objs, HandleType::VkFence, d.create_fence(&vk::FenceCreateInfo::default(), None).unwrap().as_raw());
            add(&mut objs, HandleType::VkSemaphore, d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).unwrap().as_raw());

            let left = objs.leftovers();
            assert!(!left.contains_key("VkDevice") && !left.contains_key("VkCommandBuffer"), "{left:?}");

            // An unhandled type: nothing is destroyed and the type is named.
            objs.insert_raw(9999, HandleType::VkVideoSessionKHR, 1);
            let before = objs.len();
            assert_eq!(objs.destroy_leftovers(d, arena).unwrap_err(), vec!["VkVideoSessionKHR".to_string()]);
            assert_eq!(objs.len(), before);
            objs.remove(9999);

            d.device_wait_idle().unwrap();
            let n = objs.destroy_leftovers(d, arena).unwrap();
            let pipeline_objects = if crate::workload::spv_words().is_ok() { 2 } else { 0 };
            assert_eq!(n, 18 + pipeline_objects, "everything but the arena, the device, the instance, the set and the command buffer");
            assert!(objs.leftovers().is_empty(), "{:?}", objs.leftovers());
            // The arena survived: freeing it now is legal only because it was not freed above.
            d.free_memory(arena, None);
            eprintln!("destroyed {n} leftover objects on {}", driver.name);
        }
    }
    #[test]
    fn a_type_confusion_is_named_rather_than_dereferenced() {
        let mut o = Objects::new();
        o.insert(7, HandleType::VkBuffer, vk::Buffer::from_raw(0xBEEF));
        assert_eq!(o.get::<vk::Buffer>(HandleType::VkBuffer, 7).unwrap().as_raw(), 0xBEEF);
        assert_eq!(
            o.get::<vk::Image>(HandleType::VkImage, 7).unwrap_err(),
            TableError::WrongType {
                id: 7,
                want: HandleType::VkImage,
                found: HandleType::VkBuffer
            }
        );
        assert_eq!(
            o.get::<vk::Buffer>(HandleType::VkBuffer, 8).unwrap_err(),
            TableError::Unknown { id: 8, want: HandleType::VkBuffer }
        );
    }

    /// `VK_NULL_HANDLE` is 0 on both sides and must survive the table untouched.
    #[test]
    fn the_null_handle_passes_through() {
        let o = Objects::new();
        assert_eq!(o.get::<vk::Buffer>(HandleType::VkBuffer, 0).unwrap(), vk::Buffer::null());
    }

    #[test]
    fn an_arena_offset_becomes_a_driver_offset_and_is_bounded() {
        let mut o = Objects::new();
        o.set_arena(vk::DeviceMemory::from_raw(0x1234), 4096);
        let (m, at) = resolve_memory(&o, crate::ARENA_MEMORY_ID, 512).unwrap();
        assert_eq!(m.as_raw(), 0x1234);
        assert_eq!(at, 512);
        // R2: past the end is refused rather than handed to a driver.
        assert_eq!(
            resolve_memory(&o, crate::ARENA_MEMORY_ID, 8192).unwrap_err(),
            TableError::NotInArena { memory: crate::ARENA_MEMORY_ID, offset: 8192 }
        );
        assert!(resolve_memory(&o, 99, 0).is_err());
    }
}
