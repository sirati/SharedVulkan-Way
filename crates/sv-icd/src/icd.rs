//! The safe half: `IcdLocal` and `Icd` for [`SharedVulkan`].
//!
//! Every method here takes owned wire values and returns owned wire values.
//! There is no `unsafe` in this file and there are no raw pointers in any
//! signature — that is what `sv-icd-entry`'s generated lowering bought, and it
//! is what docs/design.md §7.3 means by "the unsafe block is one line and reviewable
//! by pattern".
//!
//! # Reading this file
//!
//! A method falls into one of three groups and each says which:
//!
//! - **Answered from what we advertise** (`crate::advertise`). These are correct
//!   as far as this stage goes and stay correct once a GPU side exists, because
//!   docs/design.md §9 says we advertise a curated table rather than the peer's.
//! - **Answered from app-side state.** Object ids, arena allocations, mapping.
//!   These are the real implementation, not a placeholder: docs/design.md §6.4 and
//!   §5.3 put exactly this work on the app side.
//! - **Accepted and dropped.** Anything that would need a GPU, in the standalone
//!   configuration where there is no GPU to ask. These return a success the
//!   application can proceed through, because the alternative — failing — makes
//!   it impossible to see how far an application gets.
//!
//! # How to find the third group, which is **not** by grepping
//!
//! This paragraph used to say "every one of them is marked `// NO GPU:` … and it
//! is deliberately easy to grep for". That claim had rotted to one marker
//! against roughly twenty-six sites, so the grep returned 1/26 and looked
//! authoritative doing it — a worse outcome than no convention at all, because a
//! reader who finds one hit concludes there is one gap.
//!
//! The convention is replaced by a derivation. `crates/sv-icd/tests/audit.rs`
//! runs every generated `Icd` method against a driver with no GPU side and sorts
//! the profile into "forwards" and "answered here" **mechanically** — the
//! generated default refuses by its own name and no hand-written body does, so
//! the two are distinguishable without reading anything. It then requires a
//! classification for each local answer and fails if one is missing. That list
//! cannot go stale the way a comment can, and docs/design.md §0.20 is what it found.

use crate::advertise;
use crate::state::{Memory, PROTOCOL_API_VERSION, SharedVulkan};
use sv_icd_entry::{Icd, IcdLocal, IcdResult, LowerError, forward};
use sv_proto as p;

/// `VK_ERROR_TOO_MANY_OBJECTS`, vulkan_core.h.
const VK_ERROR_TOO_MANY_OBJECTS: i32 = -10;
/// `VK_ERROR_INVALID_EXTERNAL_HANDLE`, vulkan_core.h.
const VK_ERROR_INVALID_EXTERNAL_HANDLE: i32 = -1_000_072_003;

/// Forward if there is a GPU side, otherwise answer with `$local`.
///
/// Every physical-device query below has this shape, and writing it out
/// thirteen times buried the one thing that differs. The two arms are both
/// correct answers to different questions: with a GPU side the honest answer is
/// the driver's, and without one it is the curated table of docs/design.md §9 —
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

/// Encode a device-ring command without asking for a reply, and send it.
///
/// For commands whose only possible failures are out-of-memory codes and whose
/// ordering the rings already keep (docs/waycompose-design.md §88): the GPU
/// side makes a refusal nobody asked about fatal, so it is never silent.
fn send_unanswered(
    t: &dyn sv_icd_entry::Transport,
    opcode: p::Opcode,
    size: usize,
    encode: impl FnOnce(&mut sv_wire::Encoder<'_>) -> sv_wire::Result<()>,
) -> IcdResult<()> {
    let mut buf = vec![0u8; size];
    {
        let mut enc = sv_wire::Encoder::new(&mut buf);
        encode(&mut enc).map_err(LowerError::Wire)?;
        enc.finish().map_err(LowerError::Wire)?;
    }
    t.send(sv_icd_entry::RingClass::Device, opcode as u32, &buf)
}

/// Encode one command straight into a command buffer's blob (docs/design.md §6.5).
///
/// The generated forwarders do this for every `vkCmd*`; this is for the two
/// framing commands, which the generator emits as replying calls because that is
/// what their signatures say and which travel in the blob anyway.
fn record_into_blob(
    t: &dyn sv_icd_entry::Transport,
    command_buffer: u64,
    opcode: p::Opcode,
    size: usize,
    encode: impl FnOnce(&mut sv_wire::Encoder<'_>) -> sv_wire::Result<()>,
) -> IcdResult<()> {
    let mut buf = vec![0u8; size];
    {
        let mut enc = sv_wire::Encoder::new(&mut buf);
        encode(&mut enc).map_err(LowerError::Wire)?;
        enc.finish().map_err(LowerError::Wire)?;
    }
    t.record(command_buffer, opcode as u32, &buf)
}

impl IcdLocal for SharedVulkan {
    /// `vkEnumerateInstanceVersion`, and it is the negotiated number rather than
    /// a constant (docs/design.md §0.18).
    fn api_version(&self) -> u32 {
        self.advertised_api_version()
    }

    fn transport(&self) -> Option<&dyn sv_icd_entry::Transport> {
        self.wire()
    }

    /// One id space for the process, so that `State`'s tables and the wire agree
    /// about what an id means (docs/design.md §6.4).
    fn mint_id(&self) -> u64 {
        self.state().mint()
    }

    /// docs/design.md §5.3's rewrite, app-side half. The generated forwarders call
    /// this for every `(memory, offset)` pair that crosses; the arena allocator
    /// that answers it lives here.
    fn rewrite_memory(&self, memory: u64, offset: u64) -> IcdResult<(u64, u64)> {
        SharedVulkan::rewrite_memory(self, memory, offset)
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
    /// docs/design.md §6.4 exactly: "the app side allocates ids from a counter, so
    /// `vkCreateBuffer` encodes the id it chose and returns immediately". What
    /// the round trip adds is not the id — it is the GPU side *learning* the id,
    /// so that everything referencing this instance afterwards resolves.
    fn create_instance(&self, info: &p::VkInstanceCreateInfo) -> IcdResult<u64> {
        let (id, phys) = {
            let mut s = self.state();
            let id = s.mint();
            // One physical device, minted with the instance. docs/design.md §8: we do
            // not export `vk_icdEnumerateAdapterPhysicalDevices`, because we
            // have no PnP adapter LUID and its only benefit — the loader sorting
            // physical devices for drivers at interface ≥ 6 — is meaningless
            // with one device.
            let phys = s.mint();
            s.physical_devices.insert(phys, id);
            s.instances.insert(
                id,
                crate::state::Instance {
                    api_version: self.advertised_api_version(),
                    physical_devices: vec![phys],
                },
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
        // silently drops things (docs/design.md §9.2, R16).
        for want in &info.pp_enabled_extension_names {
            let want = String::from_utf8_lossy(want).to_string();
            if !self.device_extensions().iter().any(|(n, _)| *n == want) {
                return Err(LowerError::Result(
                    sv_icd_entry::VkResult::VK_ERROR_EXTENSION_NOT_PRESENT.0,
                ));
            }
        }
        let id = s.mint();
        // One queue object per requested queue, recorded with its family and
        // index so `vkGetDeviceQueue` can find it again. Every family we
        // advertise has one queue, so an application that asks for more than
        // that has been told it cannot have them; and one that asks for none
        // still gets family 0's, which is what `crate::wsi` records on.
        let mut wanted: Vec<(u32, u32)> = info
            .p_queue_create_infos
            .iter()
            .flat_map(|q| (0..q.queue_count.max(1)).map(move |i| (q.queue_family_index, i)))
            .collect();
        if wanted.is_empty() {
            wanted.push((0, 0));
        }
        let mut queue_ids = Vec::with_capacity(wanted.len());
        for (family, index) in wanted {
            let q = s.mint();
            s.queues.insert(q, id);
            queue_ids.push(crate::state::QueueSlot { family, index, id: q });
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
                s.queues.remove(&q.id);
            }
        }
    }

    /// `vkGetDeviceQueue` — the queue created for `(family, index)`, and one
    /// that was never created is *reported* rather than refused.
    ///
    /// The command returns `void`. There is nowhere to put an error, so refusing
    /// meant writing nothing through `pQueue` and leaving the application with
    /// whatever its uninitialised memory held — which Zink then passes to
    /// `vkQueueSubmit`. An application asking for a queue it did not create
    /// has made a mistake; handing it family 0's queue makes that mistake
    /// harmless instead of a wild pointer, and `SV_DEBUG` says so.
    ///
    /// Since docs/video-decode.md §7 the GPU side may serve several families
    /// (the decode family among them), each at the driver's own index, so the
    /// lookup is by family as well as index and the family crosses unchanged.
    ///
    /// (Zink asks for `sparse_queue`, which it leaves at `UINT32_MAX` when no
    /// family advertises `VK_QUEUE_SPARSE_BINDING_BIT` — so this is not a
    /// hypothetical application error, it is the one in front of us.)
    fn get_device_queue(&self, device: u64, family: u32, index: u32) -> IcdResult<u64> {
        let (id, family, index) = {
            let s = self.state();
            let d = s
                .devices
                .get(&device)
                .ok_or(LowerError::Shape("vkGetDeviceQueue: unknown device"))?;
            match d.queue(family, index) {
                Some(id) => (id, family, index),
                None => {
                    // Never created, which for the families we advertise means
                    // the application did not ask for it at `vkCreateDevice`.
                    sv_icd_entry::report_transport_failure(&format_args!(
                        "vkGetDeviceQueue: queue {index} of family {family} was never \
                         created; giving family 0's queue"
                    ));
                    let id = d
                        .universal_queue()
                        .ok_or(LowerError::Shape("vkGetDeviceQueue: no such queue"))?;
                    (id, 0, 0)
                }
            }
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
            // docs/design.md §6.5 gives each `VkCommandBuffer` an arena block that
            // recording encodes straight into, and `vkQueueSubmit` then sends
            // `svExecuteCommandBlob` descriptors. **That is not built** — see
            // the note in `sv_gpu_server::server` — so no block is reserved and
            // each `vkCmd*` crosses as its own device-ring packet instead.
            let ids: Vec<u64> = (0..info.command_buffer_count.max(1)).map(|_| s.mint()).collect();
            for id in &ids {
                s.command_buffers.insert(*id, info.command_pool.0);
            }
            ids
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
        {
            let mut s = self.state();
            for b in buffers {
                s.command_buffers.remove(b);
            }
        }
        // After the free has been queued, not before: `free_blob` waits for the
        // GPU side to finish with the block, and the packet it is waiting past
        // has to be on the ring for that wait to end.
        if let Some(t) = self.ring_transport() {
            for b in buffers {
                t.free_blob(*b);
            }
        }
    }

    /// docs/design.md §5.3, and the whole point of the project.
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
        // there is nothing to unmap (docs/design.md §5.3).
        let mut s = self.state();
        if let Some(m) = s.memories.get_mut(&memory) {
            m.mapped = false;
        }
    }

    // -- presentation (docs/design.md §10) ---------------------------------------
    //
    // Delegations only. The implementation is [`crate::wsi`], which is long
    // enough to want its own file and is the one place in this crate that
    // *issues* Vulkan commands rather than answering them.

    fn create_surface(&self, target: sv_icd_entry::PresentTarget) -> IcdResult<u64> {
        let id = self.state().mint();
        self.surfaces().insert(id, target);
        Ok(id)
    }

    fn destroy_surface(&self, surface: u64) {
        self.surfaces().remove(&surface);
    }

    fn surface_target(&self, surface: u64) -> Option<sv_icd_entry::PresentTarget> {
        self.surfaces().get(&surface).copied()
    }

    fn surface_capabilities(
        &self,
        target: sv_icd_entry::PresentTarget,
    ) -> IcdResult<sv_icd_entry::SurfaceCaps> {
        crate::wsi::surface_capabilities(target)
    }

    fn surface_formats(&self, target: sv_icd_entry::PresentTarget) -> Vec<(i32, i32)> {
        crate::wsi::surface_formats(target)
    }

    fn surface_present_modes(&self, target: sv_icd_entry::PresentTarget) -> Vec<i32> {
        crate::wsi::surface_present_modes(target)
    }

    fn create_swapchain(
        &self,
        device: u64,
        desc: &sv_icd_entry::SwapchainDesc,
    ) -> IcdResult<u64> {
        crate::wsi::create_swapchain(self, device, desc)
    }

    fn destroy_swapchain(&self, device: u64, swapchain: u64) {
        crate::wsi::destroy_swapchain(self, device, swapchain)
    }

    fn swapchain_images(&self, swapchain: u64) -> IcdResult<Vec<u64>> {
        crate::wsi::swapchain_images(self, swapchain)
    }

    fn acquire_next_image(
        &self,
        device: u64,
        swapchain: u64,
        timeout: u64,
        semaphore: u64,
        fence: u64,
    ) -> IcdResult<u32> {
        crate::wsi::acquire_next_image(self, device, swapchain, timeout, semaphore, fence)
    }

    fn queue_present(
        &self,
        queue: u64,
        swapchain: u64,
        image_index: u32,
        waits: &[u64],
    ) -> IcdResult<()> {
        crate::wsi::queue_present(self, queue, swapchain, image_index, waits)
    }
}

impl Icd for SharedVulkan {
    // -- what we advertise --------------------------------------------------

    /// **The acceptance criterion of this stage.** With a GPU side this reports
    /// the *driver's* properties — its `deviceName`, its limits — rather than
    /// docs/design.md §9's synthesised table, which is how `vulkaninfo` tells the two
    /// configurations apart from the outside.
    ///
    /// Every field but one is the driver's answer. `apiVersion` is **ours**, for
    /// the reason [`clamp_api_version`] gives.
    fn get_physical_device_properties(
        &self,
        args: p::VkGetPhysicalDevicePropertiesArgs,
    ) -> IcdResult<p::VkGetPhysicalDevicePropertiesReply> {
        if let Some(t) = self.wire() {
            let mut reply = forward::get_physical_device_properties(t, args)?;
            if let Some(p) = reply.p_properties.as_mut() {
                clamp_api_version(self, &mut p.api_version);
            }
            return Ok(reply);
        }
        Ok(p::VkGetPhysicalDevicePropertiesReply {
            p_properties: Some(Box::new(device_properties(self))),
        })
    }

    fn get_physical_device_properties2(
        &self,
        args: p::VkGetPhysicalDeviceProperties2Args,
    ) -> IcdResult<p::VkGetPhysicalDeviceProperties2Reply> {
        if let Some(t) = self.wire() {
            let mut reply = forward::get_physical_device_properties2(t, args)?;
            if let Some(p) = reply.p_properties.as_mut() {
                clamp_api_version(self, &mut p.properties.api_version);
            }
            return Ok(reply);
        }
        let mut out = p::VkPhysicalDeviceProperties2::default();
        out.properties = device_properties(self);
        // The application's own chain links are left exactly as it left
        // them: answering a link we have no answer for would be inventing
        // data. See `store_pnext` in `sv-icd-entry` for the direction that
        // fills them in once there is something to fill them with.
        Ok(p::VkGetPhysicalDeviceProperties2Reply { p_properties: Some(Box::new(out)) })
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
        let mut reply = forwarded!(self, get_physical_device_features2, args, {
            Ok(p::VkGetPhysicalDeviceFeatures2Reply {
                p_features: Some(Box::new(p::VkPhysicalDeviceFeatures2::default())),
            })
        })?;
        if let Some(f) = reply.p_features.as_mut() {
            crate::advertise::withdraw_gated_features(&mut f.p_next);
        }
        Ok(reply)
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

    /// "No external handle type is supported", answered here rather than
    /// forwarded.
    ///
    /// The three `vkGetPhysicalDeviceExternal*Properties` queries are the only
    /// commands on this surface where the *driver's* honest answer is our
    /// dishonest one. RADV can export a `VkBuffer` as a dma-buf; we cannot pass
    /// a dma-buf to the application, because the application is on the other
    /// side of a wire that docs/design.md §6.1 puts external handles explicitly
    /// outside of ("WSI handles and external handles are **not
    /// serialisable**"). Forwarding the reply would advertise an export path
    /// that fails at the first `vkGetMemoryFdKHR`.
    ///
    /// docs/design.md §9.2 already says we "do not advertise external
    /// memory/semaphore/fence handle types we cannot honour"; before these
    /// overrides that sentence was true only because the *extensions* were not
    /// advertised, which is not the same thing — the queries are core 1.1 and an
    /// application can make them regardless. All-zero
    /// `externalMemoryProperties`/`externalSemaphoreProperties` is the
    /// specification's way of spelling "not supported for this handle type".
    fn get_physical_device_external_buffer_properties(
        &self,
        _args: p::VkGetPhysicalDeviceExternalBufferPropertiesArgs,
    ) -> IcdResult<p::VkGetPhysicalDeviceExternalBufferPropertiesReply> {
        Ok(p::VkGetPhysicalDeviceExternalBufferPropertiesReply {
            p_external_buffer_properties: Some(Box::default()),
        })
    }

    fn get_physical_device_external_semaphore_properties(
        &self,
        _args: p::VkGetPhysicalDeviceExternalSemaphorePropertiesArgs,
    ) -> IcdResult<p::VkGetPhysicalDeviceExternalSemaphorePropertiesReply> {
        Ok(p::VkGetPhysicalDeviceExternalSemaphorePropertiesReply {
            p_external_semaphore_properties: Some(Box::default()),
        })
    }

    fn get_physical_device_external_fence_properties(
        &self,
        _args: p::VkGetPhysicalDeviceExternalFencePropertiesArgs,
    ) -> IcdResult<p::VkGetPhysicalDeviceExternalFencePropertiesReply> {
        Ok(p::VkGetPhysicalDeviceExternalFencePropertiesReply {
            p_external_fence_properties: Some(Box::default()),
        })
    }

    /// **A file descriptor is refused here, never forwarded** (docs/design.md
    /// §1a), and the six commands that would carry one say so with an error.
    ///
    /// Forwarded, `vkGetMemoryFdKHR` returned the number of a descriptor that
    /// is open in the *GPU side's* process: an integer that means nothing here,
    /// or worse, names an unrelated descriptor of this process that the
    /// application will now import and eventually close. The import direction
    /// was the mirror image — the GPU side's driver taking ownership of, and
    /// closing, whatever its own descriptor of that number was (its socket,
    /// the arena). Neither had happened because nothing called them; Firefox's
    /// Vulkan decoder does, on its first frame, once the forwarder serves a
    /// decode queue (docs/video-decode.md §7). The external-memory queries
    /// above already answer "no handle type is supported", so an application
    /// that asks first never gets here; this is for the ones that do not ask.
    ///
    /// `VK_ERROR_TOO_MANY_OBJECTS` is one of the two results the specification
    /// allows the getters; `VK_ERROR_INVALID_EXTERNAL_HANDLE` is the importers'.
    fn get_memory_fd_khr(&self, _args: p::VkGetMemoryFdKHRArgs) -> IcdResult<p::VkGetMemoryFdKHRReply> {
        Ok(p::VkGetMemoryFdKHRReply { result: p::VkResult(VK_ERROR_TOO_MANY_OBJECTS), p_fd: None })
    }

    fn get_semaphore_fd_khr(
        &self,
        _args: p::VkGetSemaphoreFdKHRArgs,
    ) -> IcdResult<p::VkGetSemaphoreFdKHRReply> {
        Ok(p::VkGetSemaphoreFdKHRReply { result: p::VkResult(VK_ERROR_TOO_MANY_OBJECTS), p_fd: None })
    }

    fn get_fence_fd_khr(&self, _args: p::VkGetFenceFdKHRArgs) -> IcdResult<p::VkGetFenceFdKHRReply> {
        Ok(p::VkGetFenceFdKHRReply { result: p::VkResult(VK_ERROR_TOO_MANY_OBJECTS), p_fd: None })
    }

    fn get_memory_fd_properties_khr(
        &self,
        _args: p::VkGetMemoryFdPropertiesKHRArgs,
    ) -> IcdResult<p::VkGetMemoryFdPropertiesKHRReply> {
        Ok(p::VkGetMemoryFdPropertiesKHRReply {
            result: p::VkResult(VK_ERROR_INVALID_EXTERNAL_HANDLE),
            p_memory_fd_properties: None,
        })
    }

    fn import_semaphore_fd_khr(
        &self,
        _args: p::VkImportSemaphoreFdKHRArgs,
    ) -> IcdResult<p::VkImportSemaphoreFdKHRReply> {
        Ok(p::VkImportSemaphoreFdKHRReply { result: p::VkResult(VK_ERROR_INVALID_EXTERNAL_HANDLE) })
    }

    fn import_fence_fd_khr(
        &self,
        _args: p::VkImportFenceFdKHRArgs,
    ) -> IcdResult<p::VkImportFenceFdKHRReply> {
        Ok(p::VkImportFenceFdKHRReply { result: p::VkResult(VK_ERROR_INVALID_EXTERNAL_HANDLE) })
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
        // not cross: docs/design.md §5.2 puts the allocator here ("it is the side
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
                    // It is also the one this side can explain and nobody else
                    // can: Zink says only "couldn't allocate memory: heap=N size=M" and
                    // WebRender only OutOfMemory, and the GPU side never sees the call.
                    // So the first one is said, always, with the numbers.
                    crate::state::say_once(
                        &crate::state::ARENA_EXHAUSTED_SAID,
                        &format_args!(
                            "vkAllocateMemory: the arena is exhausted: {} bytes asked for, {}",
                            info.allocation_size,
                            s.arena_use()
                        ),
                    );
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
        let slab_geometry = allocation.map(|a| (a.slab, s.allocator.slab_size()));
        s.memories.insert(
            id,
            Memory { memory_type_index: type_index, allocation, size, mapped: false },
        );
        if allocation.is_some() {
            s.note_arena_use();
        }
        drop(s);

        // docs/design.md §5.2: "When the app side needs a slab it picks a free region
        // of the arena, writes a slab-directory entry, and sends
        // `svImportArenaSlab`. The GPU side imports it and replies. A failed
        // import is a hard error surfaced as `VK_ERROR_OUT_OF_DEVICE_MEMORY`
        // from the triggering `vkAllocateMemory`."
        //
        // The allocator has just told us which slab this landed in, and the slab
        // directory says whether that slab exists on the other side yet. On a
        // provider that imported the whole arena up front — which is every
        // GPU-allocated one — the directory says so at attach and this is one
        // acquire load.
        if let (Some((slab, slab_size)), Some(t)) = (slab_geometry, self.ring_transport())
            && let Err(e) = t.ensure_slab(slab, u64::from(slab) * slab_size, slab_size)
        {
            crate::state::say_once(
                &crate::state::SLAB_IMPORT_SAID,
                &format_args!(
                    "vkAllocateMemory: the GPU side did not import slab {slab}: {e:?}; \
                     returning VK_ERROR_OUT_OF_DEVICE_MEMORY"
                ),
            );
            // The application gets no handle, so it can never free this one:
            // give the range back here or it is lost for the session.
            let mut s = self.state();
            if let Some(Memory { allocation: Some(a), .. }) = s.memories.remove(&id) {
                let _ = s.allocator.free_now(a);
            }
            return Ok(p::VkAllocateMemoryReply {
                result: p::VkResult(sv_icd_entry::VkResult::VK_ERROR_OUT_OF_DEVICE_MEMORY.0),
                p_memory: None,
            });
        }

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

    /// Arena memory is given back here; heap-0 memory is forwarded, and
    /// **forwarded before it is forgotten** (docs/design.md §0.28).
    ///
    /// The generated `forward::free_memory` rewrites the id through
    /// `MemoryRewrite`, which looks it up in the very table this removes it
    /// from. Removing first made every heap-0 free fail on this side with
    /// `NoSuchMemory` — reported only under `SV_DEBUG`, and only as a line —
    /// so the GPU side never freed a single device-local allocation: 416 to
    /// 439 per five-minute Firefox run, each a real driver allocation kept
    /// until the session ended.
    fn free_memory(&self, args: p::VkFreeMemoryArgs) -> IcdResult<()> {
        let id = args.memory.0;
        let arena_backed = match self.state().memories.get(&id) {
            Some(m) => m.allocation.is_some(),
            None => return Ok(()),
        };
        // Arena memory has no GPU-side object to free: it is a sub-range of the
        // one arena allocation, which outlives every application. Heap-0 memory
        // does, and it is a real driver allocation there.
        let forwarded = match (arena_backed, self.wire()) {
            (false, Some(t)) => forward::free_memory(t, args),
            _ => Ok(()),
        };
        let mut s = self.state();
        if let Some(Memory { allocation: Some(a), .. }) = s.memories.remove(&id) {
            // docs/design.md §5.5 defers this until the timelines an
            // allocation could be referenced by have passed. The timeline
            // mirror is not wired to submissions yet, so this is still
            // immediate — the honest gap, and the one
            // `ArenaAllocator::free_deferred` is waiting for.
            let _ = s.allocator.free_now(a);
            s.note_arena_use();
        }
        forwarded
    }

    fn get_buffer_memory_requirements(
        &self,
        args: p::VkGetBufferMemoryRequirementsArgs,
    ) -> IcdResult<p::VkGetBufferMemoryRequirementsReply> {
        // Forwarded for the *numbers* and patched for the *mask*. The driver
        // knows the real size and alignment for this buffer and we do not; it
        // also numbers memory types its own way, and docs/design.md §9.1 says we
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
        // be a multiple of the alignment too (docs/design.md §5.2).
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
        let usage = info.usage.0;
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
                // The GPU side's answer, forwarded. Zink prints "vkCreateBuffer
                // failed" with no result and no size, so this is the only line
                // that can say whose failure it was.
                let mut s = self.state();
                crate::state::say_once(
                    &crate::state::CREATE_BUFFER_FAILED_SAID,
                    &format_args!(
                        "vkCreateBuffer: the GPU side returned {} for a {size}-byte \
                         buffer (usage {:#x}); {}",
                        reply.result.0,
                        usage,
                        s.arena_use()
                    ),
                );
                s.buffers.remove(&id);
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

    /// Bind, or say nothing was bound.
    ///
    /// **The `(memory, offset)` rewrite is no longer here.** It used to be, and
    /// being here was the bug: §5.3's app-side half was one hand-written line in
    /// this one command, so the six other commands that carry a memory id across
    /// — `vkBindImageMemory` first among them — sent the app-side id raw and the
    /// GPU side took a ring fatal on it (docs/design.md §0.20). It is now generated
    /// for every forwarded command, in `sv_icd_entry::forward`, and this
    /// override is left only for the property the generated default does not
    /// have: with no GPU side there is nothing to bind to, and saying so is
    /// better than refusing by name.
    fn bind_buffer_memory(
        &self,
        args: p::VkBindBufferMemoryArgs,
    ) -> IcdResult<p::VkBindBufferMemoryReply> {
        let Some(t) = self.wire() else {
            // NO GPU: nothing to bind to.
            return Ok(p::VkBindBufferMemoryReply { result: p::VkResult(0) });
        };
        forward::bind_buffer_memory(t, args)
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
        let template = crate::state::Template {
            entries: args
                .p_create_info
                .as_ref()
                .map(|c| c.p_descriptor_update_entries.clone())
                .unwrap_or_default(),
            // Meaningful only for a `PUSH_DESCRIPTORS` template, and harmless
            // to keep for the others: it is the one thing
            // `vkCmdPushDescriptorSetWithTemplate` needs and does not carry.
            bind_point: args
                .p_create_info
                .as_ref()
                .map(|c| c.pipeline_bind_point)
                .unwrap_or(p::VkPipelineBindPoint(0)),
        };
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
            self.state().templates.insert(id, template);
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
                Some(t) => t.entries.clone(),
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
        let mut writes = unsafe {
            sv_icd_entry::local::expand_descriptor_template(
                args.p_data,
                args.descriptor_set.0,
                &entries,
            )
        };
        if writes.is_empty() {
            return Ok(());
        }
        // The expanded writes are `vkUpdateDescriptorSets` writes and carry the
        // same ignored member; a template entry says which `descriptorType` it
        // is writing and nothing about the layout's immutable samplers, so this
        // path needs the same pass. docs/design.md §0.26.
        self.drop_ignored_samplers(&mut writes);
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

    // -- descriptor sets: which writes carry a sampler that means anything ---
    //
    // docs/design.md §0.26. Seven commands, of which only the last changes a
    // packet; the other six exist to keep the record it reads. Each keeps the
    // generated default's refusal-by-name in its standalone arm, so the audit's
    // derivation still sorts all seven as "forwards".

    /// Remember which of this layout's bindings carry immutable samplers.
    ///
    /// That is the fact `update_descriptor_sets` cannot get any other way; see
    /// [`crate::state::State::set_layouts`].
    fn create_descriptor_set_layout(
        &self,
        mut args: p::VkCreateDescriptorSetLayoutArgs,
    ) -> IcdResult<p::VkCreateDescriptorSetLayoutReply> {
        let Some(t) = self.wire() else {
            return Err(LowerError::NotImplemented("vkCreateDescriptorSetLayout"));
        };
        // **Mint the id here.** `emit_mint_outputs` puts it in the *trait
        // default*, so overriding a create-command opts out of it silently and
        // every object of that type gets id 0 — see
        // `create_descriptor_update_template` for the day that cost.
        let id = IcdLocal::mint_id(self);
        args.p_set_layout = Some(p::VkDescriptorSetLayout(id));
        // Collected before the call, because `args` moves into it.
        let immutable: std::sync::Arc<std::collections::HashSet<u32>> = std::sync::Arc::new(
            args.p_create_info
                .as_ref()
                .map(|c| {
                    c.p_bindings
                        .iter()
                        .filter(|b| !b.p_immutable_samplers.is_empty())
                        .map(|b| b.binding)
                        .collect()
                })
                .unwrap_or_default(),
        );
        let reply = forward::create_descriptor_set_layout(t, args)?;
        if reply.result.0 == 0 {
            self.state().set_layouts.insert(id, immutable);
        }
        Ok(reply)
    }

    /// Drop the layout's entry — and *only* the layout's.
    ///
    /// A descriptor set layout may be destroyed while sets allocated from it are
    /// still in use, so this must not take the answer away from those sets. It
    /// does not: they took their own copy at allocate time
    /// ([`crate::state::DescriptorSet`]).
    fn destroy_descriptor_set_layout(
        &self,
        args: p::VkDestroyDescriptorSetLayoutArgs,
    ) -> IcdResult<()> {
        self.state().set_layouts.remove(&args.descriptor_set_layout.0);
        let Some(t) = self.wire() else {
            return Err(LowerError::NotImplemented("vkDestroyDescriptorSetLayout"));
        };
        forward::destroy_descriptor_set_layout(t, args)
    }

    /// Take each set's copy of its layout's answer, before the layout can go.
    fn allocate_descriptor_sets(
        &self,
        mut args: p::VkAllocateDescriptorSetsArgs,
    ) -> IcdResult<p::VkAllocateDescriptorSetsReply> {
        let Some(t) = self.wire() else {
            return Err(LowerError::NotImplemented("vkAllocateDescriptorSets"));
        };
        for slot in args.p_descriptor_sets.iter_mut() {
            *slot = p::VkDescriptorSet(IcdLocal::mint_id(self));
        }
        let pool = args.p_allocate_info.as_ref().map(|a| a.descriptor_pool.0).unwrap_or(0);
        // `pSetLayouts` is parallel to `pDescriptorSets`; a short one is an
        // application error the driver will refuse, and zipping simply records
        // nothing for the sets it does not describe. A layout this side never
        // saw created records nothing, which leaves its sets unclassifiable and
        // therefore untouched — the safe direction, as in `drop_ignored_samplers`.
        let record: Vec<(u64, crate::state::DescriptorSet)> = {
            let s = self.state();
            args.p_allocate_info
                .as_ref()
                .map(|a| {
                    args.p_descriptor_sets
                        .iter()
                        .zip(a.p_set_layouts.iter())
                        .filter_map(|(set, l)| {
                            let immutable = s.set_layouts.get(&l.0)?;
                            let rec = crate::state::DescriptorSet {
                                immutable: std::sync::Arc::clone(immutable),
                                pool,
                            };
                            Some((set.0, rec))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let reply = forward::allocate_descriptor_sets(t, args)?;
        if reply.result.0 == 0 {
            let mut s = self.state();
            for (set, rec) in record {
                s.descriptor_sets.insert(set, rec);
            }
        }
        Ok(reply)
    }

    fn free_descriptor_sets(
        &self,
        args: p::VkFreeDescriptorSetsArgs,
    ) -> IcdResult<p::VkFreeDescriptorSetsReply> {
        {
            let mut s = self.state();
            for set in &args.p_descriptor_sets {
                s.descriptor_sets.remove(&set.0);
            }
        }
        let Some(t) = self.wire() else {
            return Err(LowerError::NotImplemented("vkFreeDescriptorSets"));
        };
        forward::free_descriptor_sets(t, args)
    }

    /// A pool reset frees every set in it without naming one of them, which is
    /// why [`crate::state::DescriptorSet`] carries the pool.
    fn reset_descriptor_pool(
        &self,
        args: p::VkResetDescriptorPoolArgs,
    ) -> IcdResult<p::VkResetDescriptorPoolReply> {
        self.forget_pool(args.descriptor_pool.0);
        let Some(t) = self.wire() else {
            return Err(LowerError::NotImplemented("vkResetDescriptorPool"));
        };
        forward::reset_descriptor_pool(t, args)
    }

    fn destroy_descriptor_pool(&self, args: p::VkDestroyDescriptorPoolArgs) -> IcdResult<()> {
        self.forget_pool(args.descriptor_pool.0);
        let Some(t) = self.wire() else {
            return Err(LowerError::NotImplemented("vkDestroyDescriptorPool"));
        };
        forward::destroy_descriptor_pool(t, args)
    }

    /// Drop every `VkDescriptorImageInfo::sampler` the specification ignores,
    /// then forward.
    ///
    /// This is the whole of docs/design.md §0.26 on the wire. Everything above
    /// exists so that this can answer "does this write's sampler mean
    /// anything?" — and the answer has to be *no* by default for three of the
    /// five descriptor types that select `pImageInfo`, and no for the other two
    /// whenever the destination binding was declared with immutable samplers.
    fn update_descriptor_sets(&self, mut args: p::VkUpdateDescriptorSetsArgs) -> IcdResult<()> {
        self.drop_ignored_samplers(&mut args.p_descriptor_writes);
        let Some(t) = self.wire() else {
            return Err(LowerError::NotImplemented("vkUpdateDescriptorSets"));
        };
        forward::update_descriptor_sets(t, args)
    }

    /// The push form, rewritten into `vkCmdPushDescriptorSet`.
    ///
    /// # The defect this closes, which was predicted and then landed anyway
    ///
    /// `exceptions::void_policy` declares `VoidPolicy::ArenaOffset` for this
    /// command's `pData`, with a paragraph on why it is the hardest `void*` in
    /// the registry — **and nothing implemented it.** A policy is a statement
    /// about what the bytes *are*; it does not move them. So the generated
    /// lowering took the application's own `pData` pointer and put it on the
    /// wire as though it were an offset into our arena, and the GPU side read a
    /// descriptor layout from wherever in the arena that number happened to
    /// land. Measured: two device losses, a ring fatal, and **a segmentation
    /// fault inside `sv-gpu-server`** — a wrong pointer lowered as an offset is
    /// the worst blast radius this design has.
    ///
    /// docs/design.md §0.20 item 4 found it, excluded `VK_KHR_push_descriptor` from
    /// `zink` for exactly this, and wrote that *"a mechanical widening that
    /// restores it without the expansion restores the segmentation fault with
    /// it."* Making `full` the default did precisely that. The warning was
    /// right, the exclusion was right, and neither survived a profile generated
    /// from a rule — which is the argument for exclusions carrying their reasons
    /// rather than only their names.
    ///
    /// # Why it is a rewrite and not a forward
    ///
    /// The same reason `vkUpdateDescriptorSetWithTemplate` is (§0.4 item 10):
    /// `pData`'s *layout* lives in the template object and its *bytes* live in
    /// the application's memory, so it cannot cross and does not have to.
    /// Expanding it here produces the identical descriptor writes in a shape the
    /// registry describes. The one thing this form needs and the call does not
    /// carry is `pipelineBindPoint`, which is why [`crate::state::Template`]
    /// keeps it.
    ///
    /// `dstSet` is zero because a push descriptor writes into no set — the
    /// specification requires the member to be ignored, and sending anything
    /// else would be inventing a handle.
    fn cmd_push_descriptor_set_with_template(
        &self,
        args: p::VkCmdPushDescriptorSetWithTemplateArgs,
    ) -> IcdResult<()> {
        let Some(t) = self.wire() else { return Ok(()) };
        self.push_with_template(
            t,
            args.command_buffer,
            args.descriptor_update_template,
            args.layout,
            args.set,
            args.p_data,
        )
    }

    /// The `2` form, which is the same call with its parameters in a struct.
    ///
    /// `VkPushDescriptorSetWithTemplateInfo` carries the template, layout, set
    /// and `pData`; there is nothing in it the form above does not handle, so
    /// both unwrap onto one helper rather than expanding twice.
    fn cmd_push_descriptor_set_with_template2(
        &self,
        args: p::VkCmdPushDescriptorSetWithTemplate2Args,
    ) -> IcdResult<()> {
        let Some(t) = self.wire() else { return Ok(()) };
        let Some(info) = args.p_push_descriptor_set_with_template_info else {
            return Err(LowerError::Shape(
                "vkCmdPushDescriptorSetWithTemplate2: no VkPushDescriptorSetWithTemplateInfo",
            ));
        };
        self.push_with_template(
            t,
            args.command_buffer,
            info.descriptor_update_template,
            info.layout,
            info.set,
            info.p_data,
        )
    }

    // -- accepted and dropped ------------------------------------------------

    // -- forwarded, with a standalone answer behind them ---------------------
    //
    // Each of these was "accepted and dropped" in the previous stage. They are
    // now round trips whenever there is a GPU side; the local answer stays
    // because a driver with no GPU side still has to let `vulkaninfo` and the
    // loader test walk create-instance → create-device → submit → allocate →
    // map without failing, which is docs/design.md §13 step 5.
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

    /// Destroying a pool frees every command buffer allocated from it, and so
    /// every blob block they hold (docs/design.md §0.28).
    ///
    /// The blocks go back after the destroy is queued, for the reason
    /// `free_command_buffers` gives: `free_blob` waits for the GPU side to
    /// finish reading, and the packets it waits past must be on the ring.
    fn destroy_command_pool(&self, args: p::VkDestroyCommandPoolArgs) -> IcdResult<()> {
        let pool = args.command_pool.0;
        let r = forwarded!(self, destroy_command_pool, args, { Ok(()) });
        let freed: Vec<u64> = {
            let mut s = self.state();
            let freed: Vec<u64> =
                s.command_buffers.iter().filter(|(_, p)| **p == pool).map(|(cb, _)| *cb).collect();
            for cb in &freed {
                s.command_buffers.remove(cb);
            }
            freed
        };
        if let Some(t) = self.ring_transport() {
            for cb in freed {
                t.free_blob(cb);
            }
        }
        r
    }

    /// docs/design.md §6.5, and the correction to it that the blob forced.
    ///
    /// §6.5 leaves `vkBeginCommandBuffer` as an ordinary forwarded command and
    /// only `vkCmd*` in the blob. That cannot work: the GPU side replays a blob
    /// when it *arrives*, which is at `vkEndCommandBuffer`, so a `Begin` that
    /// crossed when the application called it would open the driver's command
    /// buffer at an unrelated moment — and with two command buffers being
    /// recorded concurrently, which Vulkan explicitly permits, the second
    /// `Begin` would close the first's recording and every `vkCmd*` replayed
    /// afterwards would land in the wrong one.
    ///
    /// So both framing commands go **inside** the blob
    /// (`sv_codegen::exceptions::BLOB_FRAMING`), the blob is self-describing,
    /// and a command buffer's whole recording is one packet instead of two round
    /// trips and N sends.
    ///
    /// The result is answered locally, which is the same optimism
    /// `vkQueueSubmit` now takes: a `Begin` that the driver refuses surfaces as
    /// the ring going fatal with the opcode in `fatal_msg`, and the application
    /// sees `VK_ERROR_DEVICE_LOST` on its next call. That is a worse diagnostic
    /// than a returned `VkResult` and it is the only failure this trades away —
    /// `vkBeginCommandBuffer` may return `VK_ERROR_OUT_OF_*_MEMORY` and nothing
    /// else.
    fn begin_command_buffer(
        &self,
        args: p::VkBeginCommandBufferArgs,
    ) -> IcdResult<p::VkBeginCommandBufferReply> {
        let Some(t) = self.wire() else {
            return Ok(p::VkBeginCommandBufferReply { result: p::VkResult(0) });
        };
        let cb = args.command_buffer.0;
        // Begin implicitly resets a command buffer that was already recorded
        // (VUID-vkBeginCommandBuffer-commandBuffer-00049 permits it when the
        // pool allows individual reset, and requires the application not to do
        // it otherwise), so the blob starts empty here whatever was in it.
        t.discard_blob(cb);
        record_into_blob(
            t,
            cb,
            p::Opcode::VkBeginCommandBuffer,
            p::size_cmd_vk_begin_command_buffer(&args),
            |enc| p::encode_cmd_vk_begin_command_buffer(enc, 0, &args),
        )?;
        Ok(p::VkBeginCommandBufferReply { result: p::VkResult(0) })
    }

    /// `vkResetCommandBuffer` throws away the recording on both sides.
    ///
    /// The forwarded command still crosses — the GPU side's driver command
    /// buffer is a real object and resetting it is what the application asked
    /// for — and the blob is emptied without giving its block back, because a
    /// reset command buffer is almost always about to be recorded into again and
    /// returning the block only to ask for the same size a moment later is how a
    /// first-fit allocator fragments.
    fn reset_command_buffer(
        &self,
        args: p::VkResetCommandBufferArgs,
    ) -> IcdResult<p::VkResetCommandBufferReply> {
        if let Some(t) = self.wire() {
            t.discard_blob(args.command_buffer.0);
        }
        forwarded!(self, reset_command_buffer, args, {
            Ok(p::VkResetCommandBufferReply { result: p::VkResult(0) })
        })
    }

    fn end_command_buffer(
        &self,
        args: p::VkEndCommandBufferArgs,
    ) -> IcdResult<p::VkEndCommandBufferReply> {
        let Some(t) = self.wire() else {
            return Ok(p::VkEndCommandBufferReply { result: p::VkResult(0) });
        };
        let cb = args.command_buffer.0;
        record_into_blob(
            t,
            cb,
            p::Opcode::VkEndCommandBuffer,
            p::size_cmd_vk_end_command_buffer(&args),
            |enc| p::encode_cmd_vk_end_command_buffer(enc, 0, &args),
        )?;
        t.publish_blob(cb)?;
        Ok(p::VkEndCommandBufferReply { result: p::VkResult(0) })
    }

    /// design.md D15: submit is asynchronous, and it is now.
    ///
    /// > if submit is synchronous the design is dead on latency grounds
    /// > regardless of transport
    ///
    /// # It needed no new opcode, and that is a correction to §6.3
    ///
    /// §6.3 budgets `svQueueSubmitAsync` — "submit with no reply; timeline
    /// values are the completion signal" — as one of thirteen non-Vulkan
    /// commands. It does not exist and does not need to: *asynchronous* is a
    /// property of the **flag**, not of the command. `vkQueueSubmit` sent
    /// without `CMD_FLAG_GENERATE_REPLY` is exactly `svQueueSubmitAsync`, the
    /// GPU side's generated arm already respects `wants_reply()`, and the
    /// timeline mirror §9.3 wanted turns out not to be needed either: what an
    /// application uses to learn a submission has completed is a fence or a
    /// semaphore, and both of those are objects the GPU side already owns.
    ///
    /// This is §0.6 item 1's rule — "a capability that is expressible in the
    /// forwarded API does not deserve a protocol extension" — applied to the
    /// second-largest opcode on that list.
    ///
    /// # What it costs, stated plainly
    ///
    /// The `VkResult` is not read. `VK_SUCCESS` is returned optimistically, and
    /// a submission the driver refuses shows up as the ring going fatal with
    /// `vkQueueSubmit` and the driver's own code in `fatal_msg`, which the
    /// application sees as `VK_ERROR_DEVICE_LOST` on its next call
    /// (`sv_gpu_server::session::drain` does that check, and it exists for this).
    /// The results `vkQueueSubmit` may return are the two out-of-memory codes
    /// and `VK_ERROR_DEVICE_LOST`; an application that would have handled the
    /// first two now sees the third.
    ///
    /// # The ordering it breaks, and the barrier that puts it back
    ///
    /// A blocking submit is an accidental cross-ring barrier — nothing else is
    /// issued until the GPU side has answered. Removing it exposes
    /// `vkDeviceWaitIdle`, which the ring rule puts on the *device* ring and
    /// which would otherwise be able to run before the submission it is supposed
    /// to be waiting for. [`crate::transport::Barriers`] is the answer and
    /// carries the argument.
    fn queue_submit(&self, args: p::VkQueueSubmitArgs) -> IcdResult<p::VkQueueSubmitReply> {
        let Some(t) = self.wire() else {
            return Ok(p::VkQueueSubmitReply { result: p::VkResult(0) });
        };
        let mut buf = vec![0u8; p::size_cmd_vk_queue_submit(&args)];
        {
            let mut enc = sv_wire::Encoder::new(&mut buf);
            p::encode_cmd_vk_queue_submit(&mut enc, 0, &args).map_err(LowerError::Wire)?;
            enc.finish().map_err(LowerError::Wire)?;
        }
        t.send(sv_icd_entry::RingClass::Queue, p::Opcode::VkQueueSubmit as u32, &buf)?;
        Ok(p::VkQueueSubmitReply { result: p::VkResult(0) })
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

    /// `vkResetFences` goes out without waiting for its answer, as
    /// `vkQueueSubmit` does (D15), and for the reason docs/waycompose-design.md
    /// §88 measured: on the guest a round trip is about 0.7 ms of crossing for
    /// a command the driver runs in microseconds, and Zink resets a fence every
    /// frame. The results it can return are the two out-of-memory codes; one
    /// the driver did return makes the ring fatal with this opcode, and the
    /// application sees `VK_ERROR_DEVICE_LOST` on its next call.
    ///
    /// Order is kept by the rings, not by the wait: a later wait or status
    /// query is on this same device ring, FIFO, and a later submission naming
    /// the fence is on the queue ring with a barrier past this packet
    /// ([`crate::transport`]'s `Barriers`).
    fn reset_fences(&self, args: p::VkResetFencesArgs) -> IcdResult<p::VkResetFencesReply> {
        if let Some(t) = self.wire() {
            send_unanswered(
                t,
                p::Opcode::VkResetFences,
                p::size_cmd_vk_reset_fences(&args),
                |enc| p::encode_cmd_vk_reset_fences(enc, 0, &args),
            )?;
        }
        Ok(p::VkResetFencesReply { result: p::VkResult(0) })
    }

    /// `vkResetCommandPool`, unanswered for the same reason as
    /// [`Self::reset_fences`]: Zink resets two pools a frame, and on the guest
    /// each answer cost 0.8 ms of a 21 ms frame (§88). Recording into the
    /// pool's buffers afterwards travels in blobs on this same device ring,
    /// behind it.
    fn reset_command_pool(
        &self,
        args: p::VkResetCommandPoolArgs,
    ) -> IcdResult<p::VkResetCommandPoolReply> {
        if let Some(t) = self.wire() {
            send_unanswered(
                t,
                p::Opcode::VkResetCommandPool,
                p::size_cmd_vk_reset_command_pool(&args),
                |enc| p::encode_cmd_vk_reset_command_pool(enc, 0, &args),
            )?;
        }
        Ok(p::VkResetCommandPoolReply { result: p::VkResult(0) })
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

/// Hold the forwarded `apiVersion` down to the version this ICD implements.
///
/// **The one field of `VkPhysicalDeviceProperties` that is not the driver's to
/// answer.** Everything else in that struct describes the hardware, and the
/// whole point of forwarding it is that the application learns about the real
/// device. `apiVersion` does not describe hardware: it is a promise about which
/// entry points and which structures exist, and the side that has to keep that
/// promise is this one. The wire profile is core 1.0–1.3 (`vk_registry::
/// profile::zink`), the manifest says 1.3 and `vkEnumerateInstanceVersion`
/// says 1.3 — so a physical device reporting the driver's 1.4 was reporting a
/// version its own ICD does not implement.
///
/// **The bound is [`PROTOCOL_API_VERSION`] and it is derived, not written
/// down** (docs/design.md §0.18). What changed when §0.18 was done is not this
/// function — clamping *is* the right thing and the paragraphs below are why —
/// but three facts around it: the bound now comes from the wire profile so it
/// cannot disagree with the command table; the driver's number is learned over
/// `svDriverVersions` rather than assumed; and a clamp that actually fires is
/// printed at startup by `sv_icd::state::announce`. A demotion that nobody can
/// see is how this defect survived long enough to cost the two GL versions
/// described below.
///
/// Note what this is *not*: it is not a floor. A driver reporting **less** than
/// the bound passes through untouched and the application is told the driver's
/// smaller number — which is the whole of "graceful fallback", and it needs no
/// code here because `min` already does it.
///
/// **The bound is the negotiated device version, not the bare ceiling**, and the
/// difference is not cosmetic. The two numbers are the same on every ordinary
/// run — the negotiated one *is* `min(driver, ceiling)` and the forwarded reply
/// carries that same driver — so the clamp is unchanged in practice. Where they
/// part is where it matters: a driver that answered `svDriverVersions` with 1.1
/// and this reply with 1.4 would otherwise produce an ICD whose
/// `vkEnumerateInstanceVersion` says 1.1 and whose physical device says 1.3,
/// which is a driver contradicting itself one call apart. Settling the number
/// once at connect and bounding every later answer by it is what makes the
/// driver's report a single consistent claim. It is also what makes
/// `SV_DRIVER_VERSION_MAX` on the GPU side a faithful stand-in for an old
/// device, and therefore what makes the fallback path testable at all.
///
/// It is not a cosmetic inconsistency, and the cost was two GL versions. Mesa's
/// Zink takes `screen->vk_version` from exactly this field, and its device-info
/// pass stops chaining an extension's feature struct into
/// `vkGetPhysicalDeviceFeatures2` once the version says that extension is core —
/// it reads the feature out of the core aggregate instead
/// (`zink_device_info.py`, `features_promoted`). At 1.4,
/// `vertexAttributeInstanceRateDivisor` is read from
/// `VkPhysicalDeviceVulkan14Features`, which is not a struct this profile knows,
/// so it came back false, `PIPE_CAP_VERTEX_ELEMENT_INSTANCE_DIVISOR` was 0,
/// `GL_ARB_instanced_arrays` was absent and GL stopped at 3.2 — while the
/// `VkPhysicalDeviceVertexAttributeDivisorFeaturesEXT` we *do* answer, and which
/// says `true`, went unasked. Claiming a version above what you implement makes
/// an honest answer unreachable rather than wrong.
fn clamp_api_version(icd: &SharedVulkan, v: &mut u32) {
    // `min` of the two rather than the negotiated number outright: if this
    // driver ever reported *less* here than it did at connect, the smaller
    // answer is still the honest one and raising it would be inventing support.
    *v = (*v).min(icd.advertised_device_api_version()).min(PROTOCOL_API_VERSION);
}

fn device_properties(icd: &SharedVulkan) -> p::VkPhysicalDeviceProperties {
    let c = advertise::device_properties(icd.advertised_device_api_version());
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
    // project does not do (docs/design.md §7.3: "never transmute"). The fields that
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

impl SharedVulkan {
    /// Forget every descriptor set that came out of `pool`.
    ///
    /// `vkResetDescriptorPool` and `vkDestroyDescriptorPool` free all of a
    /// pool's sets and name none of them, so this is a scan. It is the only
    /// linear pass in the record and it runs on two commands an application
    /// calls a handful of times, against a map that holds one small entry per
    /// live descriptor set — which is also what keeps the record from growing
    /// without bound, since most applications free sets exactly this way and
    /// never call `vkFreeDescriptorSets` at all.
    fn forget_pool(&self, pool: u64) {
        self.state().descriptor_sets.retain(|_, s| s.pool != pool);
    }

    /// Zero every `VkDescriptorImageInfo::sampler` that the specification says
    /// is ignored — docs/design.md §0.26.
    ///
    /// # The rule, transcribed
    ///
    /// `VkDescriptorImageInfo::sampler` is used only for descriptor types
    /// `SAMPLER` (0) and `COMBINED_IMAGE_SAMPLER` (1), and for those two only
    /// *"if the binding being updated does not use immutable samplers"*
    /// (VUID-VkWriteDescriptorSet-descriptorType-00325). `SAMPLED_IMAGE` (2),
    /// `STORAGE_IMAGE` (3) and `INPUT_ATTACHMENT` (10) also read `pImageInfo`
    /// and never read its sampler. Ignored means ignored: the application is
    /// entitled to leave anything there, and CTS leaves `0xDEADBEEF`.
    ///
    /// # Why zero rather than "leave it alone"
    ///
    /// On this wire a handle **is** an object id, so a value that means nothing
    /// to the specification still gets looked up on the GPU side. The lucky
    /// outcome is `no VkSampler with id 3735928559` and a fatal ring; the
    /// unlucky one is an id that happens to name a live sampler, which resolves,
    /// and the driver is handed a descriptor built from a value the application
    /// never chose. `VK_NULL_HANDLE` is what the far side must be given here,
    /// and [`sv_gpu_server::table::Objects::get`] already passes 0 through
    /// unchanged.
    ///
    /// # What an unknown set means, and why it is left alone
    ///
    /// A `dstSet` this side never saw allocated cannot be classified, and the
    /// two mistakes are not symmetric: keeping a sampler that is ignored costs
    /// at worst the fatal that already happens and says so, while dropping one
    /// that is *used* silently binds the wrong sampler and no diagnostic exists
    /// anywhere. So an unknown set keeps its samplers.
    fn drop_ignored_samplers(&self, writes: &mut [p::VkWriteDescriptorSet]) {
        // `VkDescriptorType`, quoted from `vulkan_core.h`.
        const SAMPLER: i64 = 0;
        const COMBINED_IMAGE_SAMPLER: i64 = 1;
        // Cheap out before taking the lock: most writes are buffers, whose
        // `pImageInfo` the lowering has already left empty
        // (`sv_codegen::exceptions::SIBLING_SELECTED`).
        if !writes.iter().any(|w| !w.p_image_info.is_empty()) {
            return;
        }
        let s = self.state();
        for w in writes.iter_mut() {
            if w.p_image_info.is_empty() {
                continue;
            }
            let ty = i64::from(w.descriptor_type.0);
            let used = if ty == SAMPLER || ty == COMBINED_IMAGE_SAMPLER {
                match s.descriptor_sets.get(&w.dst_set.0) {
                    Some(d) => !d.immutable.contains(&w.dst_binding),
                    // Never saw the set allocated: see the note above.
                    None => true,
                }
            } else {
                false
            };
            if !used {
                for info in w.p_image_info.iter_mut() {
                    info.sampler = p::VkSampler(0);
                }
            }
        }
    }

    /// The shared body of the two `vkCmdPushDescriptorSetWithTemplate` forms.
    ///
    /// One expansion, reached from two trait methods, so the `2` form cannot
    /// drift from the form it is a restatement of. See
    /// `Icd::cmd_push_descriptor_set_with_template` for what this is closing and
    /// why it is a rewrite rather than a forward (docs/design.md §0.22).
    #[allow(clippy::too_many_arguments)]
    fn push_with_template(
        &self,
        t: &dyn sv_icd_entry::Transport,
        command_buffer: p::VkCommandBuffer,
        template: p::VkDescriptorUpdateTemplate,
        layout: p::VkPipelineLayout,
        set: u32,
        p_data: u64,
    ) -> IcdResult<()> {
        let (entries, bind_point) = {
            let s = self.state();
            match s.templates.get(&template.0) {
                Some(t) => (t.entries.clone(), t.bind_point),
                // As in the update form: a template we never saw created is
                // refused rather than dropped, because dropping it leaves the
                // command buffer holding a wrong picture instead of a failed
                // call.
                None => {
                    return Err(LowerError::Shape(
                        "vkCmdPushDescriptorSetWithTemplate: unknown template",
                    ));
                }
            }
        };
        // SAFETY: `p_data` is the application's own `pData` for this call, read
        // in the application's process, with the entries it created the template
        // with. The specification's contract, exactly as for the update form.
        let writes =
            unsafe { sv_icd_entry::local::expand_descriptor_template(p_data, 0, &entries) };
        if writes.is_empty() {
            return Ok(());
        }
        forward::cmd_push_descriptor_set(
            t,
            p::VkCmdPushDescriptorSetArgs {
                command_buffer,
                pipeline_bind_point: bind_point,
                layout,
                set,
                descriptor_write_count: writes.len() as u32,
                p_descriptor_writes: writes,
            },
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DescriptorSet;

    /// One write of `count` image infos, each with the same sampler id.
    fn write(set: u64, binding: u32, ty: i32, sampler: u64) -> p::VkWriteDescriptorSet {
        let mut w = p::VkWriteDescriptorSet::default();
        w.dst_set = p::VkDescriptorSet(set);
        w.dst_binding = binding;
        w.descriptor_count = 1;
        w.descriptor_type = p::VkDescriptorType(ty);
        let mut info = p::VkDescriptorImageInfo::default();
        info.sampler = p::VkSampler(sampler);
        info.image_view = p::VkImageView(12);
        w.p_image_info = vec![info];
        w
    }

    fn samplers(writes: &[p::VkWriteDescriptorSet]) -> Vec<u64> {
        writes.iter().flat_map(|w| w.p_image_info.iter().map(|i| i.sampler.0)).collect()
    }

    /// docs/design.md §0.26, all four answers of the rule at once.
    ///
    /// `0xDEADBEEF` is the literal value CTS writes into the member it is
    /// entitled to leave uninitialised, and it is the value that lost a device:
    /// on this wire a handle *is* an object id, so an ignored one is still
    /// looked up on the GPU side and the ring goes fatal on it.
    #[test]
    fn an_ignored_sampler_does_not_reach_the_wire() {
        const POISON: u64 = 0xDEAD_BEEF;
        const SAMPLER: i32 = 0;
        const COMBINED: i32 = 1;
        const SAMPLED_IMAGE: i32 = 2;

        let icd = SharedVulkan::new(1 << 20).expect("a standalone driver");
        {
            let mut s = icd.state();
            // Layout 100: binding 1 has immutable samplers, binding 2 does not.
            // Set 10 is allocated from it and takes its own copy of the answer,
            // which is what makes the layout's own entry disposable.
            let immutable = std::sync::Arc::new([1u32].into_iter().collect());
            s.set_layouts.insert(100, std::sync::Arc::clone(&immutable));
            s.descriptor_sets.insert(10, DescriptorSet { immutable, pool: 7 });
        }

        // 1. Immutable binding: the write's sampler means nothing.
        let mut w = vec![write(10, 1, COMBINED, POISON)];
        icd.drop_ignored_samplers(&mut w);
        assert_eq!(samplers(&w), vec![0], "an immutable binding's sampler must not cross");

        // 2. Same layout, a binding it did not declare immutable: the sampler is
        //    the application's and must survive untouched.
        let mut w = vec![write(10, 2, COMBINED, 15)];
        icd.drop_ignored_samplers(&mut w);
        assert_eq!(samplers(&w), vec![15], "a used sampler must not be dropped");

        // 3. A type that never reads the member, immutable or not.
        let mut w = vec![write(10, 2, SAMPLED_IMAGE, POISON)];
        icd.drop_ignored_samplers(&mut w);
        assert_eq!(samplers(&w), vec![0], "SAMPLED_IMAGE never reads pImageInfo's sampler");

        // 4. A set this side never saw allocated cannot be classified, and the
        //    two mistakes are not symmetric: keeping is the safe direction.
        let mut w = vec![write(999, 0, SAMPLER, 15)];
        icd.drop_ignored_samplers(&mut w);
        assert_eq!(samplers(&w), vec![15], "an unknown set keeps its samplers");

        // 5. Destroying the layout must not disarm the sets allocated from it:
        //    the specification permits exactly this order, and a set that lost
        //    its answer here would put the poison back on the wire.
        icd.state().set_layouts.remove(&100);
        let mut w = vec![write(10, 1, COMBINED, POISON)];
        icd.drop_ignored_samplers(&mut w);
        assert_eq!(samplers(&w), vec![0], "a set outlives its layout with its answer");

        // 6. A pool reset does forget its sets, so the same write is no longer
        //    classifiable and the sampler is kept rather than silently dropped.
        icd.forget_pool(7);
        let mut w = vec![write(10, 1, COMBINED, POISON)];
        icd.drop_ignored_samplers(&mut w);
        assert_eq!(samplers(&w), vec![POISON], "a reset pool's sets are forgotten");
    }
}
