//! The safe half: `IcdLocal` and `Icd` for [`SharedVulkan`].
//!
//! Every method here takes owned wire values and returns owned wire values.
//! There is no `unsafe` in this file and there are no raw pointers in any
//! signature — that is what `sv-icd-entry`'s generated lowering bought, and it
//! is what design.md §7.3 means by "the unsafe block is one line and reviewable
//! by pattern".
//!
//! # Reading this file
//!
//! A method falls into one of three groups and each says which:
//!
//! - **Answered from what we advertise** (`crate::advertise`). These are correct
//!   as far as this stage goes and stay correct once a GPU side exists, because
//!   design.md §9 says we advertise a curated table rather than the peer's.
//! - **Answered from app-side state.** Object ids, arena allocations, mapping.
//!   These are the real implementation, not a placeholder: design.md §6.4 and
//!   §5.3 put exactly this work on the app side.
//! - **Accepted and dropped.** Anything that would need a GPU. Every one of them
//!   is marked `// NO GPU:` and returns a success the application can proceed
//!   through, because the alternative — failing — makes it impossible to see how
//!   far an application gets.
//!
//! The third group is the honest gap in this stage and it is deliberately easy
//! to grep for.

use crate::advertise;
use crate::state::{Memory, SharedVulkan, API_VERSION};
use sv_icd_entry::{Icd, IcdLocal, IcdResult, LowerError, forward};
use sv_proto as p;

/// Forward if there is a GPU side, otherwise answer with `$local`.
///
/// Every physical-device query below has this shape, and writing it out
/// thirteen times buried the one thing that differs. The two arms are both
/// correct answers to different questions: with a GPU side the honest answer is
/// the driver's, and without one it is the curated table of design.md §9 —
/// which is what makes `vulkaninfo` against a standalone driver a meaningful
/// test rather than a crash.
macro_rules! forwarded {
    ($self:ident, $fwd:ident, $args:ident, $local:block) => {
        match $self.wire() {
            Some(t) => forward::$fwd(t, $args),
            None => $local,
        }
    };
}

impl IcdLocal for SharedVulkan {
    fn api_version(&self) -> u32 {
        API_VERSION
    }

    fn transport(&self) -> Option<&dyn sv_icd_entry::Transport> {
        self.wire()
    }

    /// One id space for the process, so that `State`'s tables and the wire agree
    /// about what an id means (design.md §6.4).
    fn mint_id(&self) -> u64 {
        self.state().mint()
    }

    fn instance_extensions(&self) -> &[(&'static str, u32)] {
        advertise::INSTANCE_EXTENSIONS
    }

    fn device_extensions(&self) -> &[(&'static str, u32)] {
        self.device_extensions()
    }

    fn instance_proc_addr(
        &self,
        instance: Option<u64>,
        name: &str,
    ) -> Option<*const core::ffi::c_void> {
        // The rule that produces the classic silent failure: with a NULL
        // instance the loader is doing its global lookup and **only** the global
        // commands may be answered. Answering a device command here hands the
        // loader a pointer it will call with no dispatch table behind it.
        if instance.is_none() {
            return sv_icd_entry::lookup(sv_icd_entry::GLOBAL_COMMANDS, name);
        }
        // `vkGetInstanceProcAddr` must be able to return itself, and it is not
        // in any generated table because the profile excludes it from the wire.
        if name == "vkGetInstanceProcAddr" {
            return Some(
                sv_icd_entry::local::sv_vk_get_instance_proc_addr as *const core::ffi::c_void,
            );
        }
        sv_icd_entry::lookup(sv_icd_entry::GLOBAL_COMMANDS, name)
            .or_else(|| sv_icd_entry::lookup(sv_icd_entry::INSTANCE_COMMANDS, name))
            // A non-NULL instance may also be asked for device-level commands;
            // the loader turns them into trampolines.
            .or_else(|| sv_icd_entry::lookup(sv_icd_entry::DEVICE_COMMANDS, name))
    }

    fn device_proc_addr(&self, _device: u64, name: &str) -> Option<*const core::ffi::c_void> {
        if name == "vkGetDeviceProcAddr" {
            return Some(
                sv_icd_entry::local::sv_vk_get_device_proc_addr as *const core::ffi::c_void,
            );
        }
        sv_icd_entry::lookup(sv_icd_entry::DEVICE_COMMANDS, name)
    }

    /// The ids are minted here whether or not there is a GPU side, which is
    /// design.md §6.4 exactly: "the app side allocates ids from a counter, so
    /// `vkCreateBuffer` encodes the id it chose and returns immediately". What
    /// the round trip adds is not the id — it is the GPU side *learning* the id,
    /// so that everything referencing this instance afterwards resolves.
    fn create_instance(&self, info: &p::VkInstanceCreateInfo) -> IcdResult<u64> {
        let (id, phys) = {
            let mut s = self.state();
            let id = s.mint();
            // One physical device, minted with the instance. design.md §8: we do
            // not export `vk_icdEnumerateAdapterPhysicalDevices`, because we
            // have no PnP adapter LUID and its only benefit — the loader sorting
            // physical devices for drivers at interface ≥ 6 — is meaningless
            // with one device.
            let phys = s.mint();
            s.physical_devices.insert(phys, id);
            s.instances.insert(
                id,
                crate::state::Instance { api_version: API_VERSION, physical_devices: vec![phys] },
            );
            (id, phys)
        };
        if let Some(t) = self.wire() {
            let reply = forward::create_instance(
                t,
                p::VkCreateInstanceArgs {
                    p_create_info: Some(Box::new(info.clone())),
                    p_instance: Some(p::VkInstance(id)),
                },
            )?;
            if reply.result.0 != 0 {
                return Err(LowerError::Result(reply.result.0));
            }
            // The GPU side has an instance now; tell it about the physical
            // device the loader is about to ask us for, so the id resolves
            // before the first query arrives on the same ring.
            let r = forward::enumerate_physical_devices(
                t,
                p::VkEnumeratePhysicalDevicesArgs {
                    instance: p::VkInstance(id),
                    p_physical_device_count: Some(1),
                    p_physical_devices: vec![p::VkPhysicalDevice(phys)],
                },
            )?;
            if r.result.0 != 0 {
                return Err(LowerError::Result(r.result.0));
            }
        }
        Ok(id)
    }

    fn destroy_instance(&self, instance: u64) {
        if let Some(t) = self.wire() {
            let _ = forward::destroy_instance(
                t,
                p::VkDestroyInstanceArgs { instance: p::VkInstance(instance) },
            );
        }
        let mut s = self.state();
        if let Some(i) = s.instances.remove(&instance) {
            for p in i.physical_devices {
                s.physical_devices.remove(&p);
            }
        }
    }

    fn enumerate_physical_devices(&self, instance: u64) -> IcdResult<Vec<u64>> {
        let s = self.state();
        s.instances
            .get(&instance)
            .map(|i| i.physical_devices.clone())
            .ok_or(LowerError::Shape("vkEnumeratePhysicalDevices: unknown instance"))
    }

    fn create_device(&self, physical_device: u64, info: &p::VkDeviceCreateInfo) -> IcdResult<u64> {
        let mut s = self.state();
        if !s.physical_devices.contains_key(&physical_device) {
            return Err(LowerError::Shape("vkCreateDevice: unknown physical device"));
        }
        // An extension the application asks for that we do not advertise is
        // `VK_ERROR_EXTENSION_NOT_PRESENT`, which is the spec's answer and is
        // what makes a curated extension list honest rather than a filter that
        // silently drops things (design.md §9.2, R16).
        for want in &info.pp_enabled_extension_names {
            let want = String::from_utf8_lossy(want).to_string();
            if !self.device_extensions().iter().any(|(n, _)| *n == want) {
                return Err(LowerError::Result(
                    sv_icd_entry::VkResult::VK_ERROR_EXTENSION_NOT_PRESENT.0,
                ));
            }
        }
        let id = s.mint();
        // One queue object per requested queue, across all requested families.
        // We advertise one family with one queue, so an application that asks
        // for more than that has been told it cannot have them.
        let wanted: usize = info
            .p_queue_create_infos
            .iter()
            .map(|q| q.queue_count.max(1) as usize)
            .sum::<usize>()
            .max(1);
        let mut queue_ids = Vec::with_capacity(wanted);
        for _ in 0..wanted {
            let q = s.mint();
            s.queues.insert(q, id);
            queue_ids.push(q);
        }
        s.devices.insert(id, crate::state::Device { physical_device, queues: queue_ids });
        drop(s);

        if let Some(t) = self.wire() {
            let reply = forward::create_device(
                t,
                p::VkCreateDeviceArgs {
                    physical_device: p::VkPhysicalDevice(physical_device),
                    p_create_info: Some(Box::new(info.clone())),
                    p_device: Some(p::VkDevice(id)),
                },
            )?;
            if reply.result.0 != 0 {
                return Err(LowerError::Result(reply.result.0));
            }
        }
        Ok(id)
    }

    fn destroy_device(&self, device: u64) {
        if let Some(t) = self.wire() {
            let _ =
                forward::destroy_device(t, p::VkDestroyDeviceArgs { device: p::VkDevice(device) });
        }
        let mut s = self.state();
        if let Some(d) = s.devices.remove(&device) {
            for q in d.queues {
                s.queues.remove(&q);
            }
        }
    }

    /// `vkGetDeviceQueue` — one family, and a family index we do not have is
    /// *reported* rather than refused.
    ///
    /// The command returns `void`. There is nowhere to put an error, so refusing
    /// meant writing nothing through `pQueue` and leaving the application with
    /// whatever its uninitialised memory held — which Zink then passes to
    /// `vkQueueSubmit`. An application asking for a family we did not advertise
    /// has made a mistake; handing it the one queue that exists makes that
    /// mistake harmless instead of a wild pointer, and `SV_DEBUG` says so.
    ///
    /// (Zink asks for `sparse_queue`, which it leaves at `UINT32_MAX` when no
    /// family advertises `VK_QUEUE_SPARSE_BINDING_BIT` — so this is not a
    /// hypothetical application error, it is the one in front of us.)
    fn get_device_queue(&self, device: u64, family: u32, index: u32) -> IcdResult<u64> {
        if family != 0 {
            sv_icd_entry::report_transport_failure(&format_args!(
                "vkGetDeviceQueue: queue family {family} was never advertised; \
                 giving the one queue this device has"
            ));
        }
        let index = if family == 0 { index } else { 0 };
        let id = {
            let s = self.state();
            s.devices
                .get(&device)
                .and_then(|d| d.queues.get(index as usize).copied())
                .ok_or(LowerError::Shape("vkGetDeviceQueue: no such queue"))?
        };
        if let Some(t) = self.wire() {
            forward::get_device_queue(
                t,
                p::VkGetDeviceQueueArgs {
                    device: p::VkDevice(device),
                    queue_family_index: family,
                    queue_index: index,
                    p_queue: Some(p::VkQueue(id)),
                },
            )?;
        }
        Ok(id)
    }

    fn allocate_command_buffers(
        &self,
        device: u64,
        info: &p::VkCommandBufferAllocateInfo,
    ) -> IcdResult<Vec<u64>> {
        let ids: Vec<u64> = {
            let mut s = self.state();
            if !s.devices.contains_key(&device) {
                return Err(LowerError::Shape("vkAllocateCommandBuffers: unknown device"));
            }
            // design.md §6.5 gives each `VkCommandBuffer` an arena block that
            // recording encodes straight into, and `vkQueueSubmit` then sends
            // `svExecuteCommandBlob` descriptors. **That is not built** — see
            // the note in `sv_gpu_server::server` — so no block is reserved and
            // each `vkCmd*` crosses as its own device-ring packet instead.
            (0..info.command_buffer_count.max(1)).map(|_| s.mint()).collect()
        };
        if let Some(t) = self.wire() {
            let reply = forward::allocate_command_buffers(
                t,
                p::VkAllocateCommandBuffersArgs {
                    device: p::VkDevice(device),
                    p_allocate_info: Some(Box::new(info.clone())),
                    p_command_buffers: ids.iter().map(|i| p::VkCommandBuffer(*i)).collect(),
                },
            )?;
            if reply.result.0 != 0 {
                return Err(LowerError::Result(reply.result.0));
            }
        }
        Ok(ids)
    }

    fn free_command_buffers(&self, device: u64, pool: u64, buffers: &[u64]) {
        if let Some(t) = self.wire() {
            let _ = forward::free_command_buffers(
                t,
                p::VkFreeCommandBuffersArgs {
                    device: p::VkDevice(device),
                    command_pool: p::VkCommandPool(pool),
                    command_buffer_count: buffers.len() as u32,
                    p_command_buffers: buffers.iter().map(|b| p::VkCommandBuffer(*b)).collect(),
                },
            );
        }
    }

    /// design.md §5.3, and the whole point of the project.
    fn map_memory(
        &self,
        _device: u64,
        memory: u64,
        offset: u64,
        size: u64,
    ) -> IcdResult<*mut core::ffi::c_void> {
        let mut s = self.state();
        let Some(alloc) = s.memories.get(&memory).map(|m| m.allocation) else {
            return Err(LowerError::Shape("vkMapMemory: unknown memory"));
        };
        let Some(alloc) = alloc else {
            // Heap 0 is never host-visible, so this cannot be reached through a
            // legal call — the application would have had to allocate from a
            // type whose flags say it cannot be mapped.
            return Err(LowerError::Shape("vkMapMemory: memory is not host-visible"));
        };
        let whole = size == u64::MAX; // VK_WHOLE_SIZE
        let len = if whole { alloc.size.saturating_sub(offset) } else { size };
        if offset.saturating_add(len) > alloc.size {
            return Err(LowerError::Shape("vkMapMemory: range is outside the allocation"));
        }
        let base = s
            .allocator
            .arena_offset(alloc)
            .map_err(|_| LowerError::Shape("vkMapMemory: allocation is outside the arena"))?;
        // R2: bounds-checked against the region length, not merely against the
        // allocation, because the two checks catch different bugs — this one
        // catches ours.
        let p = self
            .arena
            .pointer_at(base + offset, len.max(1))
            .ok_or(LowerError::Shape("vkMapMemory: outside the arena mapping"))?;
        if let Some(m) = s.memories.get_mut(&memory) {
            m.mapped = true;
        }
        Ok(p as *mut core::ffi::c_void)
    }

    fn unmap_memory(&self, _device: u64, memory: u64) {
        // Pure bookkeeping: the arena is mapped for the process's lifetime, so
        // there is nothing to unmap (design.md §5.3).
        let mut s = self.state();
        if let Some(m) = s.memories.get_mut(&memory) {
            m.mapped = false;
        }
    }
}

impl Icd for SharedVulkan {
    // -- what we advertise --------------------------------------------------

    /// **The acceptance criterion of this stage.** With a GPU side this reports
    /// the *driver's* properties — its `deviceName`, its limits — rather than
    /// design.md §9's synthesised table, which is how `vulkaninfo` tells the two
    /// configurations apart from the outside.
    fn get_physical_device_properties(
        &self,
        args: p::VkGetPhysicalDevicePropertiesArgs,
    ) -> IcdResult<p::VkGetPhysicalDevicePropertiesReply> {
        forwarded!(self, get_physical_device_properties, args, {
            Ok(p::VkGetPhysicalDevicePropertiesReply {
                p_properties: Some(Box::new(device_properties())),
            })
        })
    }

    fn get_physical_device_properties2(
        &self,
        args: p::VkGetPhysicalDeviceProperties2Args,
    ) -> IcdResult<p::VkGetPhysicalDeviceProperties2Reply> {
        forwarded!(self, get_physical_device_properties2, args, {
            let mut out = p::VkPhysicalDeviceProperties2::default();
            out.properties = device_properties();
            // The application's own chain links are left exactly as it left
            // them: answering a link we have no answer for would be inventing
            // data. See `store_pnext` in `sv-icd-entry` for the direction that
            // fills them in once there is something to fill them with.
            Ok(p::VkGetPhysicalDeviceProperties2Reply { p_properties: Some(Box::new(out)) })
        })
    }

    fn get_physical_device_memory_properties(
        &self,
        _args: p::VkGetPhysicalDeviceMemoryPropertiesArgs,
    ) -> IcdResult<p::VkGetPhysicalDeviceMemoryPropertiesReply> {
        Ok(p::VkGetPhysicalDeviceMemoryPropertiesReply {
            p_memory_properties: Some(Box::new(memory_properties(self))),
        })
    }

    fn get_physical_device_memory_properties2(
        &self,
        _args: p::VkGetPhysicalDeviceMemoryProperties2Args,
    ) -> IcdResult<p::VkGetPhysicalDeviceMemoryProperties2Reply> {
        let mut out = p::VkPhysicalDeviceMemoryProperties2::default();
        out.memory_properties = memory_properties(self);
        Ok(p::VkGetPhysicalDeviceMemoryProperties2Reply {
            p_memory_properties: Some(Box::new(out)),
        })
    }

    fn get_physical_device_features(
        &self,
        args: p::VkGetPhysicalDeviceFeaturesArgs,
    ) -> IcdResult<p::VkGetPhysicalDeviceFeaturesReply> {
        forwarded!(self, get_physical_device_features, args, {
            Ok(p::VkGetPhysicalDeviceFeaturesReply {
                p_features: Some(Box::new(p::VkPhysicalDeviceFeatures::default())),
            })
        })
    }

    fn get_physical_device_features2(
        &self,
        args: p::VkGetPhysicalDeviceFeatures2Args,
    ) -> IcdResult<p::VkGetPhysicalDeviceFeatures2Reply> {
        forwarded!(self, get_physical_device_features2, args, {
            Ok(p::VkGetPhysicalDeviceFeatures2Reply {
                p_features: Some(Box::new(p::VkPhysicalDeviceFeatures2::default())),
            })
        })
    }

    fn get_physical_device_queue_family_properties(
        &self,
        args: p::VkGetPhysicalDeviceQueueFamilyPropertiesArgs,
    ) -> IcdResult<p::VkGetPhysicalDeviceQueueFamilyPropertiesReply> {
        forwarded!(self, get_physical_device_queue_family_properties, args, {
            let families = queue_families();
            Ok(p::VkGetPhysicalDeviceQueueFamilyPropertiesReply {
                p_queue_family_property_count: Some(families.len() as u32),
                p_queue_family_properties: families,
            })
        })
    }

    /// The `2` form, forwarded like the plain one.
    ///
    /// It was answered locally, and that was a bug the moment the plain form
    /// started forwarding: an application that asks both — Zink asks the plain
    /// one, the validation layer asks both — was told two different things about
    /// the same device. The local answer stays behind it for the standalone
    /// configuration, exactly as everywhere else.
    fn get_physical_device_queue_family_properties2(
        &self,
        args: p::VkGetPhysicalDeviceQueueFamilyProperties2Args,
    ) -> IcdResult<p::VkGetPhysicalDeviceQueueFamilyProperties2Reply> {
        if let Some(t) = self.wire() {
            return forward::get_physical_device_queue_family_properties2(t, args);
        }
        let families: Vec<p::VkQueueFamilyProperties2> = queue_families()
            .into_iter()
            .map(|q| {
                let mut w = p::VkQueueFamilyProperties2::default();
                w.queue_family_properties = q;
                w
            })
            .collect();
        Ok(p::VkGetPhysicalDeviceQueueFamilyProperties2Reply {
            p_queue_family_property_count: Some(families.len() as u32),
            p_queue_family_properties: families,
        })
    }

    fn get_physical_device_format_properties(
        &self,
        args: p::VkGetPhysicalDeviceFormatPropertiesArgs,
    ) -> IcdResult<p::VkGetPhysicalDeviceFormatPropertiesReply> {
        // §9's "curated, not mirrored" does *not* extend to this table, and the
        // previous stage said so: it is "the one place a forwarded answer is
        // strictly required before Zink can run, because Zink chooses formats
        // from exactly this table". With a GPU side it is now forwarded. Without
        // one the conservative empty set stays, and an application that respects
        // it will not try anything.
        forwarded!(self, get_physical_device_format_properties, args, {
            Ok(p::VkGetPhysicalDeviceFormatPropertiesReply {
                p_format_properties: Some(Box::new(p::VkFormatProperties::default())),
            })
        })
    }

    // -- app-side state -----------------------------------------------------

    fn allocate_memory(
        &self,
        args: p::VkAllocateMemoryArgs,
    ) -> IcdResult<p::VkAllocateMemoryReply> {
        let Some(info) = args.p_allocate_info.as_ref() else {
            return Err(LowerError::Shape("vkAllocateMemory: no VkMemoryAllocateInfo"));
        };
        let mut s = self.state();
        let id = s.mint();
        let type_index = info.memory_type_index;
        if type_index >= 4 {
            return Err(LowerError::Shape("vkAllocateMemory: no such memory type"));
        }

        // Arena-backed memory is allocated **entirely on the app side** and does
        // not cross: design.md §5.2 puts the allocator here ("it is the side
        // that receives the allocation requests, and keeping the free lists in
        // private memory means the GPU side cannot be corrupted by an allocator
        // bug"), and §5.3 makes the resulting `VkDeviceMemory` a sub-range of a
        // slab the GPU side has already imported. There is nothing to ask for.
        let allocation = if advertise::is_arena_backed(type_index) {
            match s.allocator.alloc(info.allocation_size) {
                Ok(a) => Some(a),
                Err(_) => {
                    // R11: the arena is a fixed pre-negotiated window, and
                    // exhaustion is a normal condition an application handles.
                    return Ok(p::VkAllocateMemoryReply {
                        result: p::VkResult(
                            sv_icd_entry::VkResult::VK_ERROR_OUT_OF_DEVICE_MEMORY.0,
                        ),
                        p_memory: None,
                    });
                }
            }
        } else {
            // Heap 0 is `DEVICE_LOCAL` and never mapped, so it *is* a GPU-side
            // allocation and it does cross.
            None
        };
        let size = allocation.map(|a| a.size).unwrap_or(info.allocation_size);
        s.memories.insert(
            id,
            Memory { memory_type_index: type_index, allocation, size, mapped: false },
        );
        drop(s);

        if allocation.is_none()
            && let Some(t) = self.wire()
        {
            let reply = forward::allocate_memory(
                t,
                p::VkAllocateMemoryArgs {
                    device: args.device,
                    p_allocate_info: args.p_allocate_info,
                    p_memory: Some(p::VkDeviceMemory(id)),
                },
            )?;
            if reply.result.0 != 0 {
                self.state().memories.remove(&id);
                return Ok(reply);
            }
        }
        Ok(p::VkAllocateMemoryReply {
            result: p::VkResult(0),
            p_memory: Some(p::VkDeviceMemory(id)),
        })
    }

    fn free_memory(&self, args: p::VkFreeMemoryArgs) -> IcdResult<()> {
        let arena_backed = {
            let mut s = self.state();
            match s.memories.remove(&args.memory.0) {
                Some(m) => match m.allocation {
                    Some(a) => {
                        // design.md §5.5 defers this until the timelines an
                        // allocation could be referenced by have passed. The
                        // timeline mirror is not wired to submissions yet, so
                        // this is still immediate — the honest gap, and the one
                        // `ArenaAllocator::free_deferred` is waiting for.
                        let _ = s.allocator.free_now(a);
                        true
                    }
                    None => false,
                },
                None => return Ok(()),
            }
        };
        // Arena memory has no GPU-side object to free: it is a sub-range of the
        // one arena allocation, which outlives every application. Heap-0 memory
        // does, and it is a real driver allocation there.
        if !arena_backed && let Some(t) = self.wire() {
            forward::free_memory(t, args)?;
        }
        Ok(())
    }

    fn get_buffer_memory_requirements(
        &self,
        args: p::VkGetBufferMemoryRequirementsArgs,
    ) -> IcdResult<p::VkGetBufferMemoryRequirementsReply> {
        // Forwarded for the *numbers* and patched for the *mask*. The driver
        // knows the real size and alignment for this buffer and we do not; it
        // also numbers memory types its own way, and design.md §9.1 says we
        // advertise ours. Passing its `memoryTypeBits` through would have an
        // application select a type from our four-entry table by an index that
        // means something else.
        if let Some(t) = self.wire() {
            let mut reply = forward::get_buffer_memory_requirements(t, args)?;
            if let Some(r) = reply.p_memory_requirements.as_mut() {
                r.alignment = r.alignment.max(crate::arena::GRANULARITY);
                r.size = crate::arena::round_up(r.size.max(1));
                r.memory_type_bits = advertise::ARENA_MEMORY_TYPE_BITS;
            }
            return Ok(reply);
        }
        let s = self.state();
        let size = s.buffers.get(&args.buffer.0).copied().unwrap_or(0);
        let mut r = p::VkMemoryRequirements::default();
        // Rounded to the allocator's granularity and reported honestly, because
        // `VUID-VkMemoryAllocateInfo-allocationSize-01745` requires the size to
        // be a multiple of the alignment too (design.md §5.2).
        r.size = crate::arena::round_up(size.max(1));
        r.alignment = crate::arena::GRANULARITY;
        r.memory_type_bits = advertise::ARENA_MEMORY_TYPE_BITS;
        Ok(p::VkGetBufferMemoryRequirementsReply { p_memory_requirements: Some(Box::new(r)) })
    }

    fn create_buffer(&self, args: p::VkCreateBufferArgs) -> IcdResult<p::VkCreateBufferReply> {
        let Some(info) = args.p_create_info.as_ref() else {
            return Err(LowerError::Shape("vkCreateBuffer: no VkBufferCreateInfo"));
        };
        let size = info.size;
        let id = {
            let mut s = self.state();
            let id = s.mint();
            s.buffers.insert(id, size);
            id
        };
        // §6.4: the id is chosen here and the command carries it, so creation
        // costs no round trip in the sense that matters — the app side never
        // waits to learn a handle. It still waits for the *result*, because
        // `VK_ERROR_OUT_OF_DEVICE_MEMORY` from `vkCreateBuffer` is something an
        // application is entitled to see.
        if let Some(t) = self.wire() {
            let reply = forward::create_buffer(
                t,
                p::VkCreateBufferArgs {
                    device: args.device,
                    p_create_info: args.p_create_info,
                    p_buffer: Some(p::VkBuffer(id)),
                },
            )?;
            if reply.result.0 != 0 {
                self.state().buffers.remove(&id);
                return Ok(reply);
            }
            return Ok(p::VkCreateBufferReply {
                result: p::VkResult(0),
                p_buffer: Some(p::VkBuffer(id)),
            });
        }
        Ok(p::VkCreateBufferReply { result: p::VkResult(0), p_buffer: Some(p::VkBuffer(id)) })
    }

    fn destroy_buffer(&self, args: p::VkDestroyBufferArgs) -> IcdResult<()> {
        self.state().buffers.remove(&args.buffer.0);
        if let Some(t) = self.wire() {
            forward::destroy_buffer(t, args)?;
        }
        Ok(())
    }

    /// design.md §5.3's rewrite, app-side half.
    ///
    /// The memory id becomes [`sv_gpu_server::ARENA_MEMORY_ID`]'s value and the
    /// offset becomes the allocation's arena offset plus the application's. §5.3
    /// says the id becomes "the slab's GPU-side handle"; it cannot — the app
    /// side knows no driver handle and §6.4 depends on it not knowing one — so
    /// what crosses is a well-known id and a number this side already owns, and
    /// the GPU side finishes the translation. See `sv_gpu_server::table`.
    fn bind_buffer_memory(
        &self,
        args: p::VkBindBufferMemoryArgs,
    ) -> IcdResult<p::VkBindBufferMemoryReply> {
        let Some(t) = self.wire() else {
            // NO GPU: nothing to bind to.
            return Ok(p::VkBindBufferMemoryReply { result: p::VkResult(0) });
        };
        let (memory, offset) = self.rewrite_memory(args.memory.0, args.memory_offset)?;
        forward::bind_buffer_memory(
            t,
            p::VkBindBufferMemoryArgs {
                device: args.device,
                buffer: args.buffer,
                memory: p::VkDeviceMemory(memory),
                memory_offset: offset,
            },
        )
    }

    // -- descriptor update templates ----------------------------------------
    //
    // Zink's default descriptor path uses templates unconditionally, so these
    // two are on the critical path for OpenGL and not a corner.

    /// Remember the entries, then forward as usual.
    ///
    /// The template object itself crosses and is created on the far side like
    /// any other; what stays here is a *copy of its entries*, because they are
    /// the only description of `pData`'s layout that exists anywhere.
    fn create_descriptor_update_template(
        &self,
        args: p::VkCreateDescriptorUpdateTemplateArgs,
    ) -> IcdResult<p::VkCreateDescriptorUpdateTemplateReply> {
        // **Mint the id.** `emit_mint_outputs` puts this in the *trait default*,
        // so overriding a create-command silently opts out of it and every
        // object of that type gets id 0 — which is `VK_NULL_HANDLE`, so nothing
        // complains, they all alias, and the last one created wins every lookup.
        // Two descriptor update templates both numbered 0 is how this was found:
        // the second overwrote the first's entries and a descriptor write went
        // to a binding the other set's layout does not have.
        let mut args = args;
        args.p_descriptor_update_template =
            Some(p::VkDescriptorUpdateTemplate(sv_icd_entry::IcdLocal::mint_id(self)));
        let entries = args
            .p_create_info
            .as_ref()
            .map(|c| c.p_descriptor_update_entries.clone())
            .unwrap_or_default();
        let id = args.p_descriptor_update_template.map(|t| t.0);
        let reply = match self.wire() {
            Some(t) => forward::create_descriptor_update_template(t, args)?,
            None => p::VkCreateDescriptorUpdateTemplateReply {
                result: p::VkResult(0),
                p_descriptor_update_template: args.p_descriptor_update_template,
            },
        };
        if reply.result.0 == 0
            && let Some(id) = id
        {
            self.state().templates.insert(id, entries);
        }
        Ok(reply)
    }

    fn destroy_descriptor_update_template(
        &self,
        args: p::VkDestroyDescriptorUpdateTemplateArgs,
    ) -> IcdResult<()> {
        self.state().templates.remove(&args.descriptor_update_template.0);
        if let Some(t) = self.wire() {
            forward::destroy_descriptor_update_template(t, args)?;
        }
        Ok(())
    }

    /// Rewritten into `vkUpdateDescriptorSets`, which is a shape the wire has.
    ///
    /// `pData` is a pointer whose *layout* lives in the template object and
    /// whose *bytes* live in the application's memory. It cannot cross, and it
    /// does not have to: expanding it here produces exactly the same descriptor
    /// writes in a form the registry describes. See
    /// [`sv_icd_entry::local::expand_descriptor_template`] for why this is the only
    /// parameter on the surface that needs it.
    fn update_descriptor_set_with_template(
        &self,
        args: p::VkUpdateDescriptorSetWithTemplateArgs,
    ) -> IcdResult<()> {
        let Some(t) = self.wire() else { return Ok(()) };
        let entries = {
            let s = self.state();
            match s.templates.get(&args.descriptor_update_template.0) {
                Some(e) => e.clone(),
                // A template we never saw created. Refusing is right: silently
                // doing nothing would leave the descriptor set holding whatever
                // it held, which is a wrong picture rather than a failed call.
                None => {
                    return Err(LowerError::Shape(
                        "vkUpdateDescriptorSetWithTemplate: unknown template",
                    ));
                }
            }
        };
        // SAFETY: `p_data` is the application's own `pData` for this call, read
        // in the application's process, with the entries it created the template
        // with. The specification's contract; see the function's own note.
        let writes = unsafe {
            sv_icd_entry::local::expand_descriptor_template(
                args.p_data,
                args.descriptor_set.0,
                &entries,
            )
        };
        if writes.is_empty() {
            return Ok(());
        }
        forward::update_descriptor_sets(
            t,
            p::VkUpdateDescriptorSetsArgs {
                device: args.device,
                descriptor_write_count: writes.len() as u32,
                p_descriptor_writes: writes,
                descriptor_copy_count: 0,
                p_descriptor_copies: Vec::new(),
            },
        )
    }

    // -- accepted and dropped ------------------------------------------------

    // -- forwarded, with a standalone answer behind them ---------------------
    //
    // Each of these was "accepted and dropped" in the previous stage. They are
    // now round trips whenever there is a GPU side; the local answer stays
    // because a driver with no GPU side still has to let `vulkaninfo` and the
    // loader test walk create-instance → create-device → submit → allocate →
    // map without failing, which is design.md §13 step 5.
    //
    // The object id is minted here in every case (§6.4), so the *only*
    // difference between the two arms is whether anything learns of it.

    fn create_command_pool(
        &self,
        args: p::VkCreateCommandPoolArgs,
    ) -> IcdResult<p::VkCreateCommandPoolReply> {
        let id = self.state().mint();
        let args = p::VkCreateCommandPoolArgs {
            p_command_pool: Some(p::VkCommandPool(id)),
            ..args
        };
        forwarded!(self, create_command_pool, args, {
            Ok(p::VkCreateCommandPoolReply {
                result: p::VkResult(0),
                p_command_pool: Some(p::VkCommandPool(id)),
            })
        })
    }

    fn destroy_command_pool(&self, args: p::VkDestroyCommandPoolArgs) -> IcdResult<()> {
        forwarded!(self, destroy_command_pool, args, { Ok(()) })
    }

    fn begin_command_buffer(
        &self,
        args: p::VkBeginCommandBufferArgs,
    ) -> IcdResult<p::VkBeginCommandBufferReply> {
        forwarded!(self, begin_command_buffer, args, {
            Ok(p::VkBeginCommandBufferReply { result: p::VkResult(0) })
        })
    }

    fn end_command_buffer(
        &self,
        args: p::VkEndCommandBufferArgs,
    ) -> IcdResult<p::VkEndCommandBufferReply> {
        forwarded!(self, end_command_buffer, args, {
            Ok(p::VkEndCommandBufferReply { result: p::VkResult(0) })
        })
    }

    /// design.md D15 makes submit asynchronous and non-negotiable — "if submit
    /// is synchronous the design is dead on latency grounds regardless of
    /// transport".
    ///
    /// **This one is synchronous, and it is the clearest debt in the app side.**
    /// `vkQueueSubmit` generates a reply, so the generated forwarder waits for
    /// it. Making it asynchronous needs `svQueueSubmitAsync` (§6.3) and the
    /// timeline mirror wired to it (§9.3), neither of which exists; without them
    /// there is nothing to tell the application the submission was accepted. It
    /// is correct and it is slow, and the fix is a protocol command rather than
    /// a change here.
    fn queue_submit(&self, args: p::VkQueueSubmitArgs) -> IcdResult<p::VkQueueSubmitReply> {
        forwarded!(self, queue_submit, args, {
            Ok(p::VkQueueSubmitReply { result: p::VkResult(0) })
        })
    }

    fn device_wait_idle(
        &self,
        args: p::VkDeviceWaitIdleArgs,
    ) -> IcdResult<p::VkDeviceWaitIdleReply> {
        // §9.3 makes this a timeline wait against the mirrored counters. The
        // mirror is not wired to submissions, so it is a round trip to a real
        // `vkDeviceWaitIdle` instead — correct, and one round trip rather than
        // an acquire load.
        forwarded!(self, device_wait_idle, args, {
            Ok(p::VkDeviceWaitIdleReply { result: p::VkResult(0) })
        })
    }

    fn queue_wait_idle(&self, args: p::VkQueueWaitIdleArgs) -> IcdResult<p::VkQueueWaitIdleReply> {
        forwarded!(self, queue_wait_idle, args, {
            Ok(p::VkQueueWaitIdleReply { result: p::VkResult(0) })
        })
    }

    fn create_fence(&self, args: p::VkCreateFenceArgs) -> IcdResult<p::VkCreateFenceReply> {
        let id = self.state().mint();
        let args = p::VkCreateFenceArgs { p_fence: Some(p::VkFence(id)), ..args };
        forwarded!(self, create_fence, args, {
            Ok(p::VkCreateFenceReply { result: p::VkResult(0), p_fence: Some(p::VkFence(id)) })
        })
    }

    fn destroy_fence(&self, args: p::VkDestroyFenceArgs) -> IcdResult<()> {
        forwarded!(self, destroy_fence, args, { Ok(()) })
    }

    fn get_fence_status(
        &self,
        args: p::VkGetFenceStatusArgs,
    ) -> IcdResult<p::VkGetFenceStatusReply> {
        forwarded!(self, get_fence_status, args, {
            Ok(p::VkGetFenceStatusReply { result: p::VkResult(0) })
        })
    }

    fn reset_fences(&self, args: p::VkResetFencesArgs) -> IcdResult<p::VkResetFencesReply> {
        forwarded!(self, reset_fences, args, {
            Ok(p::VkResetFencesReply { result: p::VkResult(0) })
        })
    }

    fn wait_for_fences(
        &self,
        args: p::VkWaitForFencesArgs,
    ) -> IcdResult<p::VkWaitForFencesReply> {
        forwarded!(self, wait_for_fences, args, {
            Ok(p::VkWaitForFencesReply { result: p::VkResult(0) })
        })
    }

    fn create_semaphore(
        &self,
        args: p::VkCreateSemaphoreArgs,
    ) -> IcdResult<p::VkCreateSemaphoreReply> {
        let id = self.state().mint();
        let args = p::VkCreateSemaphoreArgs { p_semaphore: Some(p::VkSemaphore(id)), ..args };
        forwarded!(self, create_semaphore, args, {
            Ok(p::VkCreateSemaphoreReply {
                result: p::VkResult(0),
                p_semaphore: Some(p::VkSemaphore(id)),
            })
        })
    }

    fn destroy_semaphore(&self, args: p::VkDestroySemaphoreArgs) -> IcdResult<()> {
        forwarded!(self, destroy_semaphore, args, { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// the advertised values, converted from the C ABI types to the wire types
// ---------------------------------------------------------------------------
//
// `crate::advertise` builds the C ABI structs because that is what its numbers
// describe and what a test can compare against `vulkan_core.h`. The wire types
// are a different set with the same field names, so the conversion is
// field-by-field and mechanical. It is written by hand here rather than
// generated because it is four structs, and a generated "C to wire for a value
// we made up ourselves" pass would be a pass with no other user.

fn device_properties() -> p::VkPhysicalDeviceProperties {
    let c = advertise::device_properties(API_VERSION);
    let mut w = p::VkPhysicalDeviceProperties::default();
    w.api_version = c.api_version;
    w.driver_version = c.driver_version;
    w.vendor_id = c.vendor_id;
    w.device_id = c.device_id;
    w.device_type = p::VkPhysicalDeviceType(c.device_type.0);
    for (i, b) in c.device_name.iter().enumerate() {
        w.device_name[i] = *b as u8;
    }
    w.pipeline_cache_uuid = c.pipeline_cache_uuid;
    w.limits = limits(&c.limits);
    w
}

fn limits(c: &sv_icd_entry::VkPhysicalDeviceLimits) -> p::VkPhysicalDeviceLimits {
    // The struct has 110 members and every one is a scalar or a small array, so
    // a byte-for-byte reinterpretation would work and is exactly what this
    // project does not do (design.md §7.3: "never transmute"). The fields that
    // matter to an application checking whether it can run are set; the rest
    // keep the zero the wire type's `Default` gives them.
    let mut w = p::VkPhysicalDeviceLimits::default();
    w.max_image_dimension1_d = c.max_image_dimension1_d;
    w.max_image_dimension2_d = c.max_image_dimension2_d;
    w.max_image_dimension3_d = c.max_image_dimension3_d;
    w.max_image_dimension_cube = c.max_image_dimension_cube;
    w.max_image_array_layers = c.max_image_array_layers;
    w.max_texel_buffer_elements = c.max_texel_buffer_elements;
    w.max_uniform_buffer_range = c.max_uniform_buffer_range;
    w.max_storage_buffer_range = c.max_storage_buffer_range;
    w.max_push_constants_size = c.max_push_constants_size;
    w.max_memory_allocation_count = c.max_memory_allocation_count;
    w.max_sampler_allocation_count = c.max_sampler_allocation_count;
    w.buffer_image_granularity = c.buffer_image_granularity;
    w.max_bound_descriptor_sets = c.max_bound_descriptor_sets;
    w.max_per_stage_resources = c.max_per_stage_resources;
    w.max_vertex_input_attributes = c.max_vertex_input_attributes;
    w.max_vertex_input_bindings = c.max_vertex_input_bindings;
    w.max_compute_work_group_count = c.max_compute_work_group_count;
    w.max_compute_work_group_invocations = c.max_compute_work_group_invocations;
    w.max_compute_work_group_size = c.max_compute_work_group_size;
    w.max_compute_shared_memory_size = c.max_compute_shared_memory_size;
    w.max_draw_indexed_index_value = c.max_draw_indexed_index_value;
    w.max_draw_indirect_count = c.max_draw_indirect_count;
    w.max_viewports = c.max_viewports;
    w.max_viewport_dimensions = c.max_viewport_dimensions;
    w.viewport_bounds_range = c.viewport_bounds_range;
    w.min_memory_map_alignment = c.min_memory_map_alignment as u64;
    w.min_texel_buffer_offset_alignment = c.min_texel_buffer_offset_alignment;
    w.min_uniform_buffer_offset_alignment = c.min_uniform_buffer_offset_alignment;
    w.min_storage_buffer_offset_alignment = c.min_storage_buffer_offset_alignment;
    w.max_framebuffer_width = c.max_framebuffer_width;
    w.max_framebuffer_height = c.max_framebuffer_height;
    w.max_framebuffer_layers = c.max_framebuffer_layers;
    w.max_color_attachments = c.max_color_attachments;
    w.max_sample_mask_words = c.max_sample_mask_words;
    w.discrete_queue_priorities = c.discrete_queue_priorities;
    w.point_size_range = c.point_size_range;
    w.line_width_range = c.line_width_range;
    w.non_coherent_atom_size = c.non_coherent_atom_size;
    w.timestamp_period = c.timestamp_period;
    w.framebuffer_color_sample_counts =
        p::VkSampleCountFlags(c.framebuffer_color_sample_counts.0);
    w.framebuffer_depth_sample_counts =
        p::VkSampleCountFlags(c.framebuffer_depth_sample_counts.0);
    w.framebuffer_stencil_sample_counts =
        p::VkSampleCountFlags(c.framebuffer_stencil_sample_counts.0);
    w.framebuffer_no_attachments_sample_counts =
        p::VkSampleCountFlags(c.framebuffer_no_attachments_sample_counts.0);
    w.sampled_image_color_sample_counts =
        p::VkSampleCountFlags(c.sampled_image_color_sample_counts.0);
    w.sampled_image_integer_sample_counts =
        p::VkSampleCountFlags(c.sampled_image_integer_sample_counts.0);
    w.sampled_image_depth_sample_counts =
        p::VkSampleCountFlags(c.sampled_image_depth_sample_counts.0);
    w.sampled_image_stencil_sample_counts =
        p::VkSampleCountFlags(c.sampled_image_stencil_sample_counts.0);
    w.storage_image_sample_counts = p::VkSampleCountFlags(c.storage_image_sample_counts.0);
    w.standard_sample_locations = c.standard_sample_locations != 0;
    w.optimal_buffer_copy_offset_alignment = c.optimal_buffer_copy_offset_alignment;
    w.optimal_buffer_copy_row_pitch_alignment = c.optimal_buffer_copy_row_pitch_alignment;
    w
}

fn memory_properties(icd: &SharedVulkan) -> p::VkPhysicalDeviceMemoryProperties {
    let c = advertise::memory_properties(icd.arena_len);
    let mut w = p::VkPhysicalDeviceMemoryProperties::default();
    w.memory_type_count = c.memory_type_count;
    for i in 0..c.memory_type_count as usize {
        w.memory_types[i] = p::VkMemoryType {
            property_flags: p::VkMemoryPropertyFlags(c.memory_types[i].property_flags.0),
            heap_index: c.memory_types[i].heap_index,
        };
    }
    w.memory_heap_count = c.memory_heap_count;
    for i in 0..c.memory_heap_count as usize {
        w.memory_heaps[i] = p::VkMemoryHeap {
            size: c.memory_heaps[i].size,
            flags: p::VkMemoryHeapFlags(c.memory_heaps[i].flags.0),
        };
    }
    w
}

fn queue_families() -> Vec<p::VkQueueFamilyProperties> {
    advertise::queue_families()
        .into_iter()
        .map(|q| {
            let mut w = p::VkQueueFamilyProperties::default();
            w.queue_flags = p::VkQueueFlags(q.queue_flags.0);
            w.queue_count = q.queue_count;
            w.timestamp_valid_bits = q.timestamp_valid_bits;
            w.min_image_transfer_granularity = p::VkExtent3D {
                width: q.min_image_transfer_granularity.width,
                height: q.min_image_transfer_granularity.height,
                depth: q.min_image_transfer_granularity.depth,
            };
            w
        })
        .collect()
}
