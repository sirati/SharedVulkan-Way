//! `impl sv_gpu_entry::GpuHost` — the part of a GPU side that is *not* generated.
//!
//! # What changed here, and it is most of the file
//!
//! The previous stage wrote this file as an `impl sv_proto::Server` with about
//! forty hand-written commands, and recorded the reason it could not go further:
//!
//! > The generator emits `lower_*` (C → wire, app side) and `store_*` (wire → C,
//! > app side, for replies). The GPU side needs the fourth corner — **wire → C,
//! > to call the driver** — and there is no `raise_*` pass. §7.2's taxonomy is
//! > missing it. […] For forty commands, by hand is correct. For 239 it would
//! > not be.
//!
//! That pass now exists (`sv_codegen::emit::gpu_entry`, generated into
//! `sv-gpu-entry`), so this file is what is left over when 221 of the 239
//! commands are generated: **eighteen**, and each is here because forwarding it
//! verbatim would be *wrong* rather than merely inconvenient. The list, with a
//! reason each, is `sv_codegen::exceptions::GPU_HAND_WRITTEN`.
//!
//! Four groups:
//!
//! - **the session** — the GPU side opens its driver before any application
//!   connects, because on the export arena path the arena *is* a driver
//!   allocation, so `vkCreateInstance` and `vkCreateDevice` bind an id to
//!   objects that already exist;
//! - **the arena** — `vkAllocateMemory` is a sub-range of an imported slab and
//!   never reaches a driver (design.md §5.2, §5.3);
//! - **two queries whose answer is ours** — the memory table of §9.1 is
//!   renumbered, and the device-extension list is a promise about what *we*
//!   forward rather than a report of what the driver has;
//! - **the mapping hooks** the generated passes consult.
//!
//! Everything else — buffers, images, samplers, descriptor sets, render passes,
//! dynamic rendering, pipelines, queries, synchronisation, every `vkCmd*` — is
//! generated, including its `pNext` chain, which is the thing the hand-written
//! conversions silently dropped.

// The wire structs are `Default` plus assignment throughout, for the reason
// `crate::convert` gives.
#![allow(clippy::field_reassign_with_default)]

use crate::arena::Arena;
use crate::driver::Driver;
use crate::table::{Objects, TableError};
use ash::vk;
use ash::vk::Handle as _;
use std::sync::{Arc, Mutex};
use sv_gpu_entry::{DriverFns, GpuHost};
use sv_proto::{self as p, HandleType};
use sv_wire::{Result as WireResult, WireError};

/// One session's view of the GPU side.
///
/// # What is per session and what is per device
///
/// Since design.md §0.11 this process serves **several independent sessions**,
/// one per connected client, each on its own threads. The split is by whether
/// two clients could tread on each other:
///
/// - `driver` and `fns` are **per device** and shared behind an `Arc`. They are
///   immutable once opened, so sharing them needs no lock at all.
/// - `arena` and `objects` are **per session**, and that is the isolation. Handle
///   ids are assigned by the app side from its own counter (§6.4), so two
///   clients hand out the same ids and a shared table would alias them; and the
///   arena's allocator lives on the *app side*, per client, so two clients over
///   one arena would both allocate from offset 0. Giving each session its own
///   arena makes both structural rather than a convention — see
///   `main.rs`'s `serve` for the alternative that was rejected.
/// - `queue_lock` is **per device** and is the one thing that is shared *and*
///   mutable. See its own documentation.
pub struct Shared {
    pub driver: Arc<Driver>,
    pub arena: Arena,
    pub objects: Mutex<Objects>,
    /// The driver's entry points, resolved once by name from the same `vk.xml`
    /// the wire format is generated from (design.md §7.2, pass 12).
    pub fns: Arc<DriverFns>,
    /// External synchronisation for the one `VkQueue` this device has.
    ///
    /// Vulkan requires the application to synchronise access to a `VkQueue`
    /// externally: `vkQueueSubmit`, `vkQueueWaitIdle` and `vkQueueBindSparse`
    /// are all "externally synchronized" on the queue parameter. The driver
    /// opens exactly one queue (`driver.rs`), so **every session shares it**.
    ///
    /// This lock is here for whoever hand-writes a queue call on this side, and
    /// it is deliberately not claimed to be more than that. **The generated
    /// dispatch does not take it**, and neither does anything today: the
    /// objects table is locked per lookup and released before the driver call,
    /// so two ring threads of one session could already reach `vkQueueSubmit`
    /// concurrently before any of this. Multi-session does not create that race;
    /// it widens it from "two rings of one client" to "two clients". It is
    /// written down here, next to the thing it protects, rather than in a
    /// document, and closing it is a change to pass 12 rather than to this
    /// struct.
    pub queue_lock: Arc<Mutex<()>>,
}

impl Shared {
    pub fn objects(&self) -> std::sync::MutexGuard<'_, Objects> {
        // A poisoned lock is not a reason to abandon the session: what it
        // protects is a table of handles, and the panic that poisoned it has
        // already been reported. Same argument `sv_icd::state` makes.
        self.objects.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One ring's view of the session.
pub struct GpuServer {
    pub shared: std::sync::Arc<Shared>,
    /// The last table failure, so the ring can put a real diagnosis in
    /// `fatal_msg` instead of "unknown command 12".
    pub last_error: Option<String>,
}

impl GpuServer {
    pub fn new(shared: std::sync::Arc<Shared>) -> GpuServer {
        GpuServer { shared, last_error: None }
    }

    fn d(&self) -> &ash::Device {
        &self.shared.driver.device
    }

    fn note(&mut self, e: TableError) -> WireError {
        self.last_error = Some(e.to_string());
        WireError::UnknownCommand { opcode: 0 }
    }

    /// The single family we advertise, with the real properties of the family
    /// `crate::driver` opened its queue on — except `queueCount`, which is 1
    /// because that is how many queues exist.
    fn one_queue_family(&self) -> p::VkQueueFamilyProperties {
        let all = unsafe {
            self.shared
                .driver
                .instance
                .get_physical_device_queue_family_properties(self.shared.driver.pd)
        };
        let mine = all.get(self.shared.driver.queue_family as usize).copied().unwrap_or_default();
        let mut w = crate::convert::queue_family(&mine);
        w.queue_count = 1;
        w
    }
}

/// `VK_SUCCESS`.
const OK: p::VkResult = p::VkResult(0);

fn fail(e: vk::Result) -> p::VkResult {
    p::VkResult(e.as_raw())
}

/// A required `pCreateInfo` that was null.
fn no_info() -> WireError {
    WireError::Truncated { need: 1, have: 0 }
}

impl GpuHost for GpuServer {
    // -- the hooks the generated passes consult -----------------------------

    fn fns(&self) -> &DriverFns {
        &self.shared.fns
    }

    fn map_handle(&mut self, ty: HandleType, id: u64) -> WireResult<u64> {
        if id == 0 {
            return Ok(0);
        }
        // Every non-dispatchable handle is a `u64` and `ash` models them all
        // through `vk::Handle`, so `vk::Buffer` is a carrier here rather than a
        // claim about the type — the type was already checked, by `ty`.
        let found = self.shared.objects().get::<vk::Buffer>(ty, id);
        match found {
            Ok(h) => Ok(h.as_raw()),
            Err(e) => Err(self.note(e)),
        }
    }

    fn rewrite_memory(&mut self, memory: u64, offset: u64) -> WireResult<(u64, u64)> {
        let r = crate::table::resolve_memory(&self.shared.objects(), memory, offset);
        match r {
            Ok((m, at)) => Ok((m.as_raw(), at)),
            Err(e) => Err(self.note(e)),
        }
    }

    fn bind(&mut self, id: u64, ty: HandleType, driver: u64) {
        if id != 0 {
            self.shared.objects().insert_raw(id, ty, driver);
        }
    }

    fn unbind(&mut self, id: u64) {
        if id != 0 {
            self.shared.objects().remove(id);
        }
    }

    fn note_missing(&mut self, command: &'static str) {
        self.last_error =
            Some(format!("the driver has no {command}; the profile is wider than this driver"));
    }

    // -- 1. the session -----------------------------------------------------
    //
    // §3 draws the driver as opening when a command arrives. It cannot: the GPU
    // side has to have a device before the app side attaches, because on the
    // export path the arena *is* a driver allocation. See `crate::driver`.

    fn create_instance(
        &mut self,
        a: p::VkCreateInstanceArgs,
    ) -> WireResult<p::VkCreateInstanceReply> {
        let raw = self.shared.driver.instance.handle();
        if let Some(id) = a.p_instance {
            self.shared.objects().insert(id.0, HandleType::VkInstance, raw);
        }
        Ok(p::VkCreateInstanceReply { result: OK, p_instance: a.p_instance })
    }

    fn destroy_instance(&mut self, _a: p::VkDestroyInstanceArgs) -> WireResult<()> {
        // The instance outlives every app-side one: it is the process's, opened
        // at startup and destroyed in `Driver::drop`.
        Ok(())
    }

    fn enumerate_physical_devices(
        &mut self,
        a: p::VkEnumeratePhysicalDevicesArgs,
    ) -> WireResult<p::VkEnumeratePhysicalDevicesReply> {
        // One device, and it is the one this process opened. §8: "we have no PnP
        // adapter LUID and the loader's sorting is meaningless with one virtual
        // device".
        let ids = a.p_physical_devices.clone();
        for id in &ids {
            self.shared.objects().insert(id.0, HandleType::VkPhysicalDevice, self.shared.driver.pd);
        }
        Ok(p::VkEnumeratePhysicalDevicesReply {
            result: OK,
            p_physical_device_count: Some(ids.len().max(1) as u32),
            p_physical_devices: ids,
        })
    }

    fn create_device(&mut self, a: p::VkCreateDeviceArgs) -> WireResult<p::VkCreateDeviceReply> {
        // The application's requested feature set is **not** honoured, and that
        // is a real limitation rather than an oversight: the device it is asking
        // for already exists. `crate::driver` compensates by enabling every
        // extension and feature the physical device reports, so the answer to
        // "did I get what I asked for" is yes whenever the driver could have
        // said yes at all. What an application cannot do here is ask for
        // *less* — which no application does, and which costs nothing but the
        // validation layer's opinion of it.
        let raw = self.shared.driver.device.handle();
        if let Some(d) = a.p_device {
            self.shared.objects().insert(d.0, HandleType::VkDevice, raw);
        }
        Ok(p::VkCreateDeviceReply { result: OK, p_device: a.p_device })
    }

    fn destroy_device(&mut self, _a: p::VkDestroyDeviceArgs) -> WireResult<()> {
        Ok(())
    }

    fn get_device_queue(
        &mut self,
        a: p::VkGetDeviceQueueArgs,
    ) -> WireResult<p::VkGetDeviceQueueReply> {
        let q = self.shared.driver.queue;
        if let Some(id) = a.p_queue {
            self.shared.objects().insert(id.0, HandleType::VkQueue, q);
        }
        Ok(p::VkGetDeviceQueueReply { p_queue: a.p_queue })
    }

    fn get_device_queue2(
        &mut self,
        a: p::VkGetDeviceQueue2Args,
    ) -> WireResult<p::VkGetDeviceQueue2Reply> {
        let q = self.shared.driver.queue;
        if let Some(id) = a.p_queue {
            self.shared.objects().insert(id.0, HandleType::VkQueue, q);
        }
        Ok(p::VkGetDeviceQueue2Reply { p_queue: a.p_queue })
    }

    // -- 2. the arena -------------------------------------------------------

    fn allocate_memory(
        &mut self,
        a: p::VkAllocateMemoryArgs,
    ) -> WireResult<p::VkAllocateMemoryReply> {
        let info = a.p_allocate_info.as_ref().ok_or_else(no_info)?;
        // Arena-backed memory never reaches here: §9.1's heap 1 is allocated by
        // the app side's own allocator and rewritten to `ARENA_MEMORY_ID` before
        // it crosses. What does reach here is heap 0 — `DEVICE_LOCAL`, never
        // mapped — which is a real driver allocation.
        let want = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        let ti = self.shared.driver.memory_type(u32::MAX, want).unwrap_or(0);
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(info.allocation_size)
            .memory_type_index(ti);
        match unsafe { self.d().allocate_memory(&ai, None) } {
            Ok(m) => {
                if let Some(id) = a.p_memory {
                    self.shared.objects().insert_memory(
                        id.0,
                        crate::table::MemoryRange {
                            memory: m,
                            base: 0,
                            len: info.allocation_size,
                        },
                    );
                }
                Ok(p::VkAllocateMemoryReply { result: OK, p_memory: a.p_memory })
            }
            Err(e) => Ok(p::VkAllocateMemoryReply { result: fail(e), p_memory: None }),
        }
    }

    /// Forwarded, with one `pNext` link the application did not write.
    ///
    /// **The only command on this surface the GPU side alters rather than
    /// reproduces**, and the alteration is about the arena rather than about
    /// buffers. The arena's `VkDeviceMemory` is external memory on every
    /// provider of design.md §5.4 — exported as a dma-buf here, imported as a
    /// host pointer on the Windows path — and a buffer bound into external
    /// memory must have been *created* naming at least one of the same handle
    /// types. Without it RADV renders correctly and the validation layer says
    /// `VUID-vkBindBufferMemory-memory-02726` twelve times a frame; a stricter
    /// driver is entitled to fail the bind.
    ///
    /// Three things are worth saying about why it lands here and not elsewhere.
    ///
    /// **It cannot be conditional**, because at `vkCreateBuffer` nobody knows
    /// which memory the buffer will be bound to: the application has not called
    /// `vkGetBufferMemoryRequirements`, let alone chosen a type. So every
    /// forwarded buffer is created external-capable, including the ones that
    /// end up in §9.1's type 0, which is an ordinary driver allocation. That is
    /// legal — naming a handle type permits an external binding, it does not
    /// require one — and its real cost is that the driver may narrow
    /// `memoryTypeBits`. `Driver::assert_arena_buffers_are_exportable` checks
    /// at startup that it does not narrow it past the arena.
    ///
    /// **It cannot be done by the app side.** design.md §0.4 says "the app side
    /// cannot add that chain — it does not know the handle type", which is true
    /// and understates it: the handle type is chosen by whichever §5.4 provider
    /// this side used, and on the app-allocated path the `Attach` that carries
    /// the arena to the ICD comes from `sv-hostd`, which does not know it
    /// either. Nothing outside this process holds the value.
    ///
    /// **It merges rather than appends.** Two `pNext` links of the same `sType`
    /// is invalid usage and the second is ignored, so an application that
    /// supplied its own `VkExternalMemoryBufferCreateInfo` — as one importing a
    /// dma-buf legitimately would — has ours OR-ed into it instead.
    fn create_buffer(&mut self, a: p::VkCreateBufferArgs) -> WireResult<p::VkCreateBufferReply> {
        let mut a = a;
        a.device.0 = self.map_handle(HandleType::VkDevice, a.device.0)?;
        let kind = self.shared.arena.external_handle_type.as_raw();
        let info = a.p_create_info.as_mut().ok_or_else(no_info)?;
        if kind != 0 {
            let existing = info.p_next.iter_mut().find_map(|l| match l {
                p::PNextLink::VkExternalMemoryBufferCreateInfo(x) => Some(x),
                _ => None,
            });
            match existing {
                Some(x) => x.handle_types.0 |= kind,
                None => {
                    let mut x = p::VkExternalMemoryBufferCreateInfo::default();
                    x.handle_types = p::VkExternalMemoryHandleTypeFlags(kind);
                    info.p_next.push(p::PNextLink::VkExternalMemoryBufferCreateInfo(Box::new(x)));
                }
            }
        }

        let Some(f) = self.fns().vk_create_buffer else {
            self.note_missing("vkCreateBuffer");
            let mut r = p::VkCreateBufferReply::default();
            r.result = p::VkResult(-3);
            return Ok(r);
        };
        let mut pool = sv_gpu_entry::Raise::default();
        let ci = sv_gpu_entry::raise_vk_buffer_create_info(info, &mut pool)?;
        let mut buffer = Default::default();
        // SAFETY: `ci` and every pointer inside it are owned by `pool`, which
        // outlives the call; `device` is the driver handle the map produced.
        let r = unsafe {
            f(a.device.0 as usize as _, &ci, core::ptr::null(), &mut buffer)
        };
        if let Some(id) = a.p_buffer {
            self.bind(id.0, HandleType::VkBuffer, buffer as usize as u64);
        }
        Ok(p::VkCreateBufferReply { result: p::VkResult(r.0), p_buffer: a.p_buffer })
    }

    fn free_memory(&mut self, a: p::VkFreeMemoryArgs) -> WireResult<()> {
        let id = a.memory.0;
        if id == crate::ARENA_MEMORY_ID {
            // The arena is freed when the session ends, never by an application.
            return Ok(());
        }
        let r = self.shared.objects().memory(id);
        if let Some(r) = r {
            unsafe { self.d().free_memory(r.memory, None) };
        }
        self.shared.objects().remove(id);
        Ok(())
    }

    fn map_memory(&mut self, a: p::VkMapMemoryArgs) -> WireResult<p::VkMapMemoryReply> {
        // Nothing crosses, and the wire reply has nowhere to put an address —
        // `ppData` is on §6.1's exclusion list ("pointers to mapped memory
        // regions … are **not serialisable**"), so `VkMapMemoryReply` carries a
        // result and nothing else. The app side's `vkMapMemory` returns a
        // pointer into its own mapping of the arena (design.md §5.3); this
        // exists so that an app side which asks anyway gets a success rather
        // than a refusal.
        let _ = a;
        Ok(p::VkMapMemoryReply { result: OK })
    }

    fn unmap_memory(&mut self, _a: p::VkUnmapMemoryArgs) -> WireResult<()> {
        Ok(())
    }

    fn flush_mapped_memory_ranges(
        &mut self,
        _a: p::VkFlushMappedMemoryRangesArgs,
    ) -> WireResult<p::VkFlushMappedMemoryRangesReply> {
        // §5.6: the arena is host-coherent on both providers, so a flush is a
        // no-op rather than an unimplemented command. Forwarding it would name a
        // `VkMappedMemoryRange` in *arena* coordinates against a driver
        // allocation the driver never mapped.
        Ok(p::VkFlushMappedMemoryRangesReply { result: OK })
    }

    fn invalidate_mapped_memory_ranges(
        &mut self,
        _a: p::VkInvalidateMappedMemoryRangesArgs,
    ) -> WireResult<p::VkInvalidateMappedMemoryRangesReply> {
        Ok(p::VkInvalidateMappedMemoryRangesReply { result: OK })
    }

    // -- 3. answers that are ours ------------------------------------------

    fn get_physical_device_memory_properties(
        &mut self,
        _a: p::VkGetPhysicalDeviceMemoryPropertiesArgs,
    ) -> WireResult<p::VkGetPhysicalDeviceMemoryPropertiesReply> {
        // Reported for diagnostics only: design.md §9.1 says the app side
        // advertises its own four-type table and does not mirror this one, and
        // `sv-icd` answers the command locally so this reply is never consulted.
        Ok(p::VkGetPhysicalDeviceMemoryPropertiesReply {
            p_memory_properties: Some(Box::new(crate::convert::memory_properties(
                &self.shared.driver.mem_props,
            ))),
        })
    }

    fn get_physical_device_memory_properties2(
        &mut self,
        _a: p::VkGetPhysicalDeviceMemoryProperties2Args,
    ) -> WireResult<p::VkGetPhysicalDeviceMemoryProperties2Reply> {
        let mut out = p::VkPhysicalDeviceMemoryProperties2::default();
        out.memory_properties = crate::convert::memory_properties(&self.shared.driver.mem_props);
        Ok(p::VkGetPhysicalDeviceMemoryProperties2Reply {
            p_memory_properties: Some(Box::new(out)),
        })
    }

    /// The one queue family this GPU side has, described from the queue it
    /// actually opened.
    ///
    /// Not the driver's table. design.md §8 gives one physical device and
    /// `crate::driver` opens one queue on one family; an application handed
    /// RADV's five families would ask for family 3 and be given family 0's
    /// queue with no diagnostic. The flags are the real ones for the family we
    /// chose, so an application that checks `VK_QUEUE_GRAPHICS_BIT` gets a true
    /// answer — it is the *indexing* that is ours, exactly as with the memory
    /// table of §9.1.
    fn get_physical_device_queue_family_properties(
        &mut self,
        a: p::VkGetPhysicalDeviceQueueFamilyPropertiesArgs,
    ) -> WireResult<p::VkGetPhysicalDeviceQueueFamilyPropertiesReply> {
        let one = self.one_queue_family();
        let want = a.p_queue_family_properties.len();
        Ok(p::VkGetPhysicalDeviceQueueFamilyPropertiesReply {
            p_queue_family_property_count: Some(1),
            p_queue_family_properties: if want == 0 { Vec::new() } else { vec![one] },
        })
    }

    fn get_physical_device_queue_family_properties2(
        &mut self,
        a: p::VkGetPhysicalDeviceQueueFamilyProperties2Args,
    ) -> WireResult<p::VkGetPhysicalDeviceQueueFamilyProperties2Reply> {
        let mut w = p::VkQueueFamilyProperties2::default();
        w.queue_family_properties = self.one_queue_family();
        let want = a.p_queue_family_properties.len();
        Ok(p::VkGetPhysicalDeviceQueueFamilyProperties2Reply {
            p_queue_family_property_count: Some(1),
            p_queue_family_properties: if want == 0 { Vec::new() } else { vec![w] },
        })
    }

    // -- 5. a handle the registry does not type as one ---------------------

    /// `vkSetDebugUtilsObjectNameEXT`, with `objectHandle` mapped by hand.
    ///
    /// The generated `handle_map` pass walks *types*, and this member's type is
    /// `uint64_t`; what makes it a handle is `objecttype="objectType"`, an
    /// attribute naming a sibling member that decides the type at run time.
    /// Nothing type-directed can see that, so the app side's id would reach the
    /// driver unmapped — and a dispatchable one is a pointer, which RADV
    /// dereferences.
    ///
    /// Naming is cosmetic, so an unresolvable handle is dropped rather than
    /// failed: an application that names an object we have no record of is
    /// asking for a label, not for correctness.
    fn set_debug_utils_object_name_ext(
        &mut self,
        a: p::VkSetDebugUtilsObjectNameEXTArgs,
    ) -> WireResult<p::VkSetDebugUtilsObjectNameEXTReply> {
        let Some(mut info) = a.p_name_info.clone() else {
            return Ok(p::VkSetDebugUtilsObjectNameEXTReply { result: OK });
        };
        let Some(raw) = self.shared.objects().raw(info.object_handle) else {
            return Ok(p::VkSetDebugUtilsObjectNameEXTReply { result: OK });
        };
        info.object_handle = raw;
        let mut name: Vec<u8> = info.p_object_name.clone();
        name.push(0);
        let Ok(name) = std::ffi::CStr::from_bytes_with_nul(&name) else {
            return Ok(p::VkSetDebugUtilsObjectNameEXTReply { result: OK });
        };
        let mut ci = vk::DebugUtilsObjectNameInfoEXT::default().object_name(name);
        // `ash`'s builder types `objectHandle` as one of its `Handle` types,
        // which is precisely the typing the registry declines to give it: what
        // decides the type here is the `objectType` sibling, at run time. The
        // fields are assigned directly rather than through the builder for that
        // reason and no other.
        ci.object_type = vk::ObjectType::from_raw(info.object_type.0);
        ci.object_handle = info.object_handle;
        let r = match &self.shared.driver.debug_utils {
            Some(d) => unsafe { d.set_debug_utils_object_name(&ci) },
            None => Ok(()),
        };
        Ok(p::VkSetDebugUtilsObjectNameEXTReply {
            result: match r {
                Ok(()) => OK,
                Err(e) => fail(e),
            },
        })
    }

    fn set_debug_utils_object_tag_ext(
        &mut self,
        _a: p::VkSetDebugUtilsObjectTagEXTArgs,
    ) -> WireResult<p::VkSetDebugUtilsObjectTagEXTReply> {
        // As above, and nothing in the GL stack sets tags; accepted and dropped
        // rather than forwarded with an unmapped handle.
        Ok(p::VkSetDebugUtilsObjectTagEXTReply { result: OK })
    }

    fn enumerate_device_extension_properties(
        &mut self,
        a: p::VkEnumerateDeviceExtensionPropertiesArgs,
    ) -> WireResult<p::VkEnumerateDeviceExtensionPropertiesReply> {
        // The *intersection* of what the driver has with what the profile
        // forwards, and the direction of the filter is the point: an extension
        // we advertise is a promise that its entry points work, and an entry
        // point works only if the generator emitted a forwarder for it. Handing
        // the driver's list through unfiltered would advertise
        // `VK_KHR_acceleration_structure` on a machine whose protocol has never
        // heard of it.
        let have = crate::driver::forwardable_device_extensions(&self.shared.driver);
        let capacity = a.p_properties.len();
        let mut props: Vec<p::VkExtensionProperties> = Vec::new();
        for (name, version) in have.iter().take(if capacity == 0 { 0 } else { capacity }) {
            let mut e = p::VkExtensionProperties::default();
            let bytes = name.as_bytes();
            let n = bytes.len().min(e.extension_name.len() - 1);
            e.extension_name[..n].copy_from_slice(&bytes[..n]);
            e.spec_version = *version;
            props.push(e);
        }
        let complete = capacity == 0 || capacity >= have.len();
        Ok(p::VkEnumerateDeviceExtensionPropertiesReply {
            // `VK_INCOMPLETE` when the caller's buffer was too small, which is a
            // result and not a failure — the count-then-fill idiom depends on it.
            result: if complete { OK } else { p::VkResult(5) },
            p_property_count: Some(have.len() as u32),
            p_properties: props,
        })
    }
}
