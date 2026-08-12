//! The GPU side's object table: app-assigned id → driver handle.
//!
//! design.md §6.4: "Handles crossing the boundary are **always table indices,
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
    /// No object with this id. Almost always the app side sending a handle it
    /// created on a ring the GPU side has not drained yet, which is R22 — a
    /// missing `RingBarrier` — rather than a bad handle.
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
                "no {} with id {id} — if the app side created it on another ring, \
                 the packet needed a RingBarrier (R22)",
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
    /// has attached, let alone allocated. design.md §5.2's lazy import is a
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
    /// time (design.md §5.2).
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
