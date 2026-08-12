//! `impl sv_proto::Server` — decoded arguments in, real driver calls out.
//!
//! # What is implemented, and why not all 239
//!
//! design.md §13 sequences the GPU side deliberately: step 4 is a byte-pattern
//! round trip, step 5 is `vulkaninfo` through the real loader, step 6 is "widen
//! to Zink's requirement set … mostly generator coverage plus GPU-side semantic
//! glue". This file is steps 4 and 5 and the compute workload §13 step 7 puts
//! first, which is:
//!
//! - the physical-device queries `vulkaninfo` asks, answered from the **real
//!   driver** rather than from `sv_icd::advertise`;
//! - enough of the device to run a compute dispatch over arena memory: buffers,
//!   memory binding, shader modules, descriptors, a compute pipeline, a command
//!   pool and buffer, recording, submit, and a fence to wait on.
//!
//! Everything else takes `Server`'s default, which refuses by name with its own
//! opcode. That default is new — see the note on the trait — and it is what
//! makes "enough commands for that path" expressible at all.
//!
//! # The pass design.md §7.2 does not have
//!
//! The generator emits `lower_*` (C → wire, app side) and `store_*` (wire → C,
//! app side, for replies). The GPU side needs the fourth corner — **wire → C, to
//! call the driver** — and there is no `raise_*` pass. §7.2's taxonomy is
//! missing it.
//!
//! It is not an oversight that can be fixed by adding an emitter in an
//! afternoon, which is why this file writes the conversions by hand. A C
//! `VkDescriptorSetLayoutCreateInfo` holds a `const VkDescriptorSetLayoutBinding
//! *pBindings`, so raising an owned wire value means producing storage that
//! outlives the call and threading its lifetime through every nested struct. The
//! app-side `lower_` pass has the mirror-image problem and solves it by *reading*
//! borrowed C memory rather than producing it. Written by hand against `ash`'s
//! builders the lifetimes are the borrow checker's problem and cost nothing;
//! generated, they would be a new category of thing for the generator to get
//! right, in the crate where a mistake is a driver dereferencing a dangling
//! pointer.
//!
//! For forty commands, by hand is correct. For 239 it would not be, and that is
//! the real finding: **the GPU side's cost does not scale the way the app
//! side's does**, because the app side got a generated pass and the GPU side
//! does not have one.

// The wire structs are `Default` plus assignment throughout, for the reason
// `crate::convert` gives: naming every member of a 106-member struct to satisfy
// this lint would bury the handful that carry meaning. Same trade `sv-icd`
// makes over the same types.
#![allow(clippy::field_reassign_with_default)]

use crate::arena::Arena;
use crate::driver::Driver;
use crate::table::{Objects, TableError, resolve_memory};
use ash::vk;
use std::sync::Mutex;
use sv_proto::{self as p, HandleType};
use sv_wire::{Result as WireResult, WireError};

/// Everything the ring threads share.
pub struct Shared {
    pub driver: Driver,
    pub arena: Arena,
    pub objects: Mutex<Objects>,
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

    /// Resolve one id, recording the diagnosis if it fails.
    ///
    /// The lookup is finished — and the guard dropped — before `note` runs,
    /// because `note` takes `&mut self` and the guard borrows `self.shared`.
    fn h<H: vk::Handle>(&mut self, ty: HandleType, id: u64) -> WireResult<H> {
        let found = self.shared.objects().get::<H>(ty, id);
        match found {
            Ok(h) => Ok(h),
            Err(e) => Err(self.note(e)),
        }
    }

    fn note(&mut self, e: TableError) -> WireError {
        let w = WireError::UnknownCommand { opcode: 0 };
        self.last_error = Some(e.to_string());
        w
    }

    fn bind<H: vk::Handle>(&mut self, id: Option<u64>, ty: HandleType, handle: H) {
        if let Some(id) = id {
            self.shared.objects().insert(id, ty, handle);
        }
    }
}

/// A driver result as a wire `VkResult`.
fn res(r: std::result::Result<(), vk::Result>) -> p::VkResult {
    match r {
        Ok(()) => p::VkResult(0),
        Err(e) => p::VkResult(e.as_raw()),
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

impl p::Server for GpuServer {
    // -- physical device: the real driver's answers, not ours ---------------
    //
    // This is the acceptance criterion of the stage: `vulkaninfo` reporting the
    // *driver's* properties rather than `sv_icd::advertise`'s synthesised ones.

    fn get_physical_device_properties(
        &mut self,
        _a: p::VkGetPhysicalDevicePropertiesArgs,
    ) -> WireResult<p::VkGetPhysicalDevicePropertiesReply> {
        let c = self.shared.driver.props;
        Ok(p::VkGetPhysicalDevicePropertiesReply {
            p_properties: Some(Box::new(crate::convert::properties(&c))),
        })
    }

    fn get_physical_device_properties2(
        &mut self,
        _a: p::VkGetPhysicalDeviceProperties2Args,
    ) -> WireResult<p::VkGetPhysicalDeviceProperties2Reply> {
        let c = self.shared.driver.props;
        let mut out = p::VkPhysicalDeviceProperties2::default();
        out.properties = crate::convert::properties(&c);
        Ok(p::VkGetPhysicalDeviceProperties2Reply { p_properties: Some(Box::new(out)) })
    }

    fn get_physical_device_features(
        &mut self,
        _a: p::VkGetPhysicalDeviceFeaturesArgs,
    ) -> WireResult<p::VkGetPhysicalDeviceFeaturesReply> {
        let f = unsafe {
            self.shared.driver.instance.get_physical_device_features(self.shared.driver.pd)
        };
        Ok(p::VkGetPhysicalDeviceFeaturesReply {
            p_features: Some(Box::new(crate::convert::features(&f))),
        })
    }

    fn get_physical_device_features2(
        &mut self,
        _a: p::VkGetPhysicalDeviceFeatures2Args,
    ) -> WireResult<p::VkGetPhysicalDeviceFeatures2Reply> {
        let f = unsafe {
            self.shared.driver.instance.get_physical_device_features(self.shared.driver.pd)
        };
        let mut out = p::VkPhysicalDeviceFeatures2::default();
        out.features = crate::convert::features(&f);
        Ok(p::VkGetPhysicalDeviceFeatures2Reply { p_features: Some(Box::new(out)) })
    }

    fn get_physical_device_queue_family_properties(
        &mut self,
        _a: p::VkGetPhysicalDeviceQueueFamilyPropertiesArgs,
    ) -> WireResult<p::VkGetPhysicalDeviceQueueFamilyPropertiesReply> {
        let q = unsafe {
            self.shared
                .driver
                .instance
                .get_physical_device_queue_family_properties(self.shared.driver.pd)
        };
        let v: Vec<_> = q.iter().map(crate::convert::queue_family).collect();
        Ok(p::VkGetPhysicalDeviceQueueFamilyPropertiesReply {
            p_queue_family_property_count: Some(v.len() as u32),
            p_queue_family_properties: v,
        })
    }

    fn get_physical_device_format_properties(
        &mut self,
        a: p::VkGetPhysicalDeviceFormatPropertiesArgs,
    ) -> WireResult<p::VkGetPhysicalDeviceFormatPropertiesReply> {
        let f = unsafe {
            self.shared.driver.instance.get_physical_device_format_properties(
                self.shared.driver.pd,
                vk::Format::from_raw(a.format.0),
            )
        };
        let mut w = p::VkFormatProperties::default();
        w.linear_tiling_features = p::VkFormatFeatureFlags(f.linear_tiling_features.as_raw());
        w.optimal_tiling_features = p::VkFormatFeatureFlags(f.optimal_tiling_features.as_raw());
        w.buffer_features = p::VkFormatFeatureFlags(f.buffer_features.as_raw());
        Ok(p::VkGetPhysicalDeviceFormatPropertiesReply { p_format_properties: Some(Box::new(w)) })
    }

    fn get_physical_device_format_properties2(
        &mut self,
        a: p::VkGetPhysicalDeviceFormatProperties2Args,
    ) -> WireResult<p::VkGetPhysicalDeviceFormatProperties2Reply> {
        let f = unsafe {
            self.shared.driver.instance.get_physical_device_format_properties(
                self.shared.driver.pd,
                vk::Format::from_raw(a.format.0),
            )
        };
        let mut out = p::VkFormatProperties2::default();
        out.format_properties.linear_tiling_features =
            p::VkFormatFeatureFlags(f.linear_tiling_features.as_raw());
        out.format_properties.optimal_tiling_features =
            p::VkFormatFeatureFlags(f.optimal_tiling_features.as_raw());
        out.format_properties.buffer_features = p::VkFormatFeatureFlags(f.buffer_features.as_raw());
        Ok(p::VkGetPhysicalDeviceFormatProperties2Reply {
            p_format_properties: Some(Box::new(out)),
        })
    }

    fn get_physical_device_queue_family_properties2(
        &mut self,
        _a: p::VkGetPhysicalDeviceQueueFamilyProperties2Args,
    ) -> WireResult<p::VkGetPhysicalDeviceQueueFamilyProperties2Reply> {
        let q = unsafe {
            self.shared
                .driver
                .instance
                .get_physical_device_queue_family_properties(self.shared.driver.pd)
        };
        let v: Vec<p::VkQueueFamilyProperties2> = q
            .iter()
            .map(|f| {
                let mut w = p::VkQueueFamilyProperties2::default();
                w.queue_family_properties = crate::convert::queue_family(f);
                w
            })
            .collect();
        Ok(p::VkGetPhysicalDeviceQueueFamilyProperties2Reply {
            p_queue_family_property_count: Some(v.len() as u32),
            p_queue_family_properties: v,
        })
    }

    /// `vulkaninfo` asks this for every format it knows, and losing the device
    /// on the first one is how the first end-to-end run failed.
    fn get_physical_device_image_format_properties(
        &mut self,
        a: p::VkGetPhysicalDeviceImageFormatPropertiesArgs,
    ) -> WireResult<p::VkGetPhysicalDeviceImageFormatPropertiesReply> {
        let r = unsafe {
            self.shared.driver.instance.get_physical_device_image_format_properties(
                self.shared.driver.pd,
                vk::Format::from_raw(a.format.0),
                vk::ImageType::from_raw(a.type_.0),
                vk::ImageTiling::from_raw(a.tiling.0),
                vk::ImageUsageFlags::from_raw(a.usage.0),
                vk::ImageCreateFlags::from_raw(a.flags.0),
            )
        };
        match r {
            Ok(f) => Ok(p::VkGetPhysicalDeviceImageFormatPropertiesReply {
                result: OK,
                p_image_format_properties: Some(Box::new(crate::convert::image_format(&f))),
            }),
            // `VK_ERROR_FORMAT_NOT_SUPPORTED` is the normal answer for most of
            // the formats `vulkaninfo` asks about, and it is a result rather
            // than a failure.
            Err(e) => Ok(p::VkGetPhysicalDeviceImageFormatPropertiesReply {
                result: fail(e),
                p_image_format_properties: None,
            }),
        }
    }

    fn get_physical_device_memory_properties(
        &mut self,
        _a: p::VkGetPhysicalDeviceMemoryPropertiesArgs,
    ) -> WireResult<p::VkGetPhysicalDeviceMemoryPropertiesReply> {
        // Reported for diagnostics only: design.md §9.1 says the app side
        // advertises its own four-type table and does not mirror this one, and
        // `sv-icd` overrides the command so this reply is never consulted in
        // practice.
        Ok(p::VkGetPhysicalDeviceMemoryPropertiesReply {
            p_memory_properties: Some(Box::new(crate::convert::memory_properties(
                &self.shared.driver.mem_props,
            ))),
        })
    }

    // -- instance and device: bind an id to what already exists -------------
    //
    // §3 draws the driver as opening when a command arrives. It cannot: the GPU
    // side has to have a device before the app side attaches, because on the
    // export path the arena *is* a driver allocation. See `crate::driver`.

    fn create_instance(
        &mut self,
        a: p::VkCreateInstanceArgs,
    ) -> WireResult<p::VkCreateInstanceReply> {
        let id = a.p_instance.map(|i| i.0);
        let raw = self.shared.driver.instance.handle();
        if let Some(id) = id {
            self.shared.objects().insert(id, HandleType::VkInstance, raw);
        }
        Ok(p::VkCreateInstanceReply { result: OK, p_instance: a.p_instance })
    }

    fn destroy_instance(&mut self, _a: p::VkDestroyInstanceArgs) -> WireResult<()> {
        // The instance outlives every app-side one: it is the process's, opened
        // at startup and destroyed in `Driver::drop`. Forgetting the id is the
        // whole of the work.
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

    // -- memory --------------------------------------------------------------

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

    // -- buffers -------------------------------------------------------------

    fn create_buffer(&mut self, a: p::VkCreateBufferArgs) -> WireResult<p::VkCreateBufferReply> {
        let info = a.p_create_info.as_ref().ok_or_else(no_info)?;
        let ci = vk::BufferCreateInfo::default()
            .flags(vk::BufferCreateFlags::from_raw(info.flags.0))
            .size(info.size)
            .usage(vk::BufferUsageFlags::from_raw(info.usage.0))
            .sharing_mode(vk::SharingMode::from_raw(info.sharing_mode.0));
        match unsafe { self.d().create_buffer(&ci, None) } {
            Ok(b) => {
                self.bind(a.p_buffer.map(|h| h.0), HandleType::VkBuffer, b);
                Ok(p::VkCreateBufferReply { result: OK, p_buffer: a.p_buffer })
            }
            Err(e) => Ok(p::VkCreateBufferReply { result: fail(e), p_buffer: None }),
        }
    }

    fn destroy_buffer(&mut self, a: p::VkDestroyBufferArgs) -> WireResult<()> {
        let b: vk::Buffer = self.h(HandleType::VkBuffer, a.buffer.0)?;
        unsafe { self.d().destroy_buffer(b, None) };
        self.shared.objects().remove(a.buffer.0);
        Ok(())
    }

    fn get_buffer_memory_requirements(
        &mut self,
        a: p::VkGetBufferMemoryRequirementsArgs,
    ) -> WireResult<p::VkGetBufferMemoryRequirementsReply> {
        let b: vk::Buffer = self.h(HandleType::VkBuffer, a.buffer.0)?;
        let r = unsafe { self.d().get_buffer_memory_requirements(b) };
        let mut w = p::VkMemoryRequirements::default();
        w.size = r.size;
        w.alignment = r.alignment;
        // The driver's own mask, forwarded unchanged. The app side *replaces* it
        // with the §9.1 table's bits, because the two sides number memory types
        // differently and an application picking by the driver's numbering would
        // pick a type our table does not have. Sending the real one anyway is
        // what makes that substitution visible rather than invented.
        w.memory_type_bits = r.memory_type_bits;
        Ok(p::VkGetBufferMemoryRequirementsReply { p_memory_requirements: Some(Box::new(w)) })
    }

    fn bind_buffer_memory(
        &mut self,
        a: p::VkBindBufferMemoryArgs,
    ) -> WireResult<p::VkBindBufferMemoryReply> {
        let b: vk::Buffer = self.h(HandleType::VkBuffer, a.buffer.0)?;
        // The GPU-side half of §5.3's rewrite. The app side turned its own
        // allocation coordinates into arena coordinates; this turns arena
        // coordinates into a driver allocation and an offset into it.
        let resolved = resolve_memory(&self.shared.objects(), a.memory.0, a.memory_offset);
        let (mem, at) = match resolved {
            Ok(v) => v,
            Err(e) => return Err(self.note(e)),
        };
        let r = unsafe { self.d().bind_buffer_memory(b, mem, at) };
        Ok(p::VkBindBufferMemoryReply { result: res(r) })
    }

    // -- images --------------------------------------------------------------
    //
    // Not needed by the compute workload; needed by `vulkaninfo`, which creates
    // an image per format to ask what its memory requirements are.

    fn create_image(&mut self, a: p::VkCreateImageArgs) -> WireResult<p::VkCreateImageReply> {
        let info = a.p_create_info.as_ref().ok_or_else(no_info)?;
        let ci = vk::ImageCreateInfo::default()
            .flags(vk::ImageCreateFlags::from_raw(info.flags.0))
            .image_type(vk::ImageType::from_raw(info.image_type.0))
            .format(vk::Format::from_raw(info.format.0))
            .extent(vk::Extent3D {
                width: info.extent.width,
                height: info.extent.height,
                depth: info.extent.depth,
            })
            .mip_levels(info.mip_levels)
            .array_layers(info.array_layers)
            .samples(vk::SampleCountFlags::from_raw(info.samples.0 as u32))
            .tiling(vk::ImageTiling::from_raw(info.tiling.0))
            .usage(vk::ImageUsageFlags::from_raw(info.usage.0))
            .sharing_mode(vk::SharingMode::from_raw(info.sharing_mode.0))
            .initial_layout(vk::ImageLayout::from_raw(info.initial_layout.0));
        match unsafe { self.d().create_image(&ci, None) } {
            Ok(i) => {
                self.bind(a.p_image.map(|h| h.0), HandleType::VkImage, i);
                Ok(p::VkCreateImageReply { result: OK, p_image: a.p_image })
            }
            Err(e) => Ok(p::VkCreateImageReply { result: fail(e), p_image: None }),
        }
    }

    fn destroy_image(&mut self, a: p::VkDestroyImageArgs) -> WireResult<()> {
        let i: vk::Image = self.h(HandleType::VkImage, a.image.0)?;
        unsafe { self.d().destroy_image(i, None) };
        self.shared.objects().remove(a.image.0);
        Ok(())
    }

    fn get_image_memory_requirements(
        &mut self,
        a: p::VkGetImageMemoryRequirementsArgs,
    ) -> WireResult<p::VkGetImageMemoryRequirementsReply> {
        let i: vk::Image = self.h(HandleType::VkImage, a.image.0)?;
        let r = unsafe { self.d().get_image_memory_requirements(i) };
        let mut w = p::VkMemoryRequirements::default();
        w.size = r.size;
        w.alignment = r.alignment;
        w.memory_type_bits = r.memory_type_bits;
        Ok(p::VkGetImageMemoryRequirementsReply { p_memory_requirements: Some(Box::new(w)) })
    }

    fn bind_image_memory(
        &mut self,
        a: p::VkBindImageMemoryArgs,
    ) -> WireResult<p::VkBindImageMemoryReply> {
        let i: vk::Image = self.h(HandleType::VkImage, a.image.0)?;
        let resolved = resolve_memory(&self.shared.objects(), a.memory.0, a.memory_offset);
        let (mem, at) = match resolved {
            Ok(v) => v,
            Err(e) => return Err(self.note(e)),
        };
        let r = unsafe { self.d().bind_image_memory(i, mem, at) };
        Ok(p::VkBindImageMemoryReply { result: res(r) })
    }

    // -- shaders, descriptors, pipelines -------------------------------------

    fn create_shader_module(
        &mut self,
        a: p::VkCreateShaderModuleArgs,
    ) -> WireResult<p::VkCreateShaderModuleReply> {
        let info = a.p_create_info.as_ref().ok_or_else(no_info)?;
        let ci = vk::ShaderModuleCreateInfo::default().code(&info.p_code);
        match unsafe { self.d().create_shader_module(&ci, None) } {
            Ok(m) => {
                self.bind(a.p_shader_module.map(|h| h.0), HandleType::VkShaderModule, m);
                Ok(p::VkCreateShaderModuleReply { result: OK, p_shader_module: a.p_shader_module })
            }
            Err(e) => {
                Ok(p::VkCreateShaderModuleReply { result: fail(e), p_shader_module: None })
            }
        }
    }

    fn destroy_shader_module(&mut self, a: p::VkDestroyShaderModuleArgs) -> WireResult<()> {
        let m: vk::ShaderModule = self.h(HandleType::VkShaderModule, a.shader_module.0)?;
        unsafe { self.d().destroy_shader_module(m, None) };
        self.shared.objects().remove(a.shader_module.0);
        Ok(())
    }

    fn create_descriptor_set_layout(
        &mut self,
        a: p::VkCreateDescriptorSetLayoutArgs,
    ) -> WireResult<p::VkCreateDescriptorSetLayoutReply> {
        let info = a.p_create_info.as_ref().ok_or_else(no_info)?;
        let bindings: Vec<vk::DescriptorSetLayoutBinding> = info
            .p_bindings
            .iter()
            .map(|b| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b.binding)
                    .descriptor_type(vk::DescriptorType::from_raw(b.descriptor_type.0))
                    .descriptor_count(b.descriptor_count)
                    .stage_flags(vk::ShaderStageFlags::from_raw(b.stage_flags.0))
            })
            .collect();
        let ci = vk::DescriptorSetLayoutCreateInfo::default()
            .flags(vk::DescriptorSetLayoutCreateFlags::from_raw(info.flags.0))
            .bindings(&bindings);
        match unsafe { self.d().create_descriptor_set_layout(&ci, None) } {
            Ok(l) => {
                self.bind(
                    a.p_set_layout.map(|h| h.0),
                    HandleType::VkDescriptorSetLayout,
                    l,
                );
                Ok(p::VkCreateDescriptorSetLayoutReply {
                    result: OK,
                    p_set_layout: a.p_set_layout,
                })
            }
            Err(e) => Ok(p::VkCreateDescriptorSetLayoutReply {
                result: fail(e),
                p_set_layout: None,
            }),
        }
    }

    fn destroy_descriptor_set_layout(
        &mut self,
        a: p::VkDestroyDescriptorSetLayoutArgs,
    ) -> WireResult<()> {
        let l: vk::DescriptorSetLayout =
            self.h(HandleType::VkDescriptorSetLayout, a.descriptor_set_layout.0)?;
        unsafe { self.d().destroy_descriptor_set_layout(l, None) };
        self.shared.objects().remove(a.descriptor_set_layout.0);
        Ok(())
    }

    fn create_descriptor_pool(
        &mut self,
        a: p::VkCreateDescriptorPoolArgs,
    ) -> WireResult<p::VkCreateDescriptorPoolReply> {
        let info = a.p_create_info.as_ref().ok_or_else(no_info)?;
        let sizes: Vec<vk::DescriptorPoolSize> = info
            .p_pool_sizes
            .iter()
            .map(|s| {
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::from_raw(s.type_.0))
                    .descriptor_count(s.descriptor_count)
            })
            .collect();
        let ci = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::from_raw(info.flags.0))
            .max_sets(info.max_sets)
            .pool_sizes(&sizes);
        match unsafe { self.d().create_descriptor_pool(&ci, None) } {
            Ok(pool) => {
                self.bind(a.p_descriptor_pool.map(|h| h.0), HandleType::VkDescriptorPool, pool);
                Ok(p::VkCreateDescriptorPoolReply {
                    result: OK,
                    p_descriptor_pool: a.p_descriptor_pool,
                })
            }
            Err(e) => Ok(p::VkCreateDescriptorPoolReply {
                result: fail(e),
                p_descriptor_pool: None,
            }),
        }
    }

    fn destroy_descriptor_pool(
        &mut self,
        a: p::VkDestroyDescriptorPoolArgs,
    ) -> WireResult<()> {
        let pool: vk::DescriptorPool =
            self.h(HandleType::VkDescriptorPool, a.descriptor_pool.0)?;
        unsafe { self.d().destroy_descriptor_pool(pool, None) };
        self.shared.objects().remove(a.descriptor_pool.0);
        Ok(())
    }

    fn allocate_descriptor_sets(
        &mut self,
        a: p::VkAllocateDescriptorSetsArgs,
    ) -> WireResult<p::VkAllocateDescriptorSetsReply> {
        let info = a.p_allocate_info.as_ref().ok_or_else(no_info)?;
        let pool: vk::DescriptorPool =
            self.h(HandleType::VkDescriptorPool, info.descriptor_pool.0)?;
        let mut layouts = Vec::with_capacity(info.p_set_layouts.len());
        for l in &info.p_set_layouts {
            layouts.push(self.h::<vk::DescriptorSetLayout>(
                HandleType::VkDescriptorSetLayout,
                l.0,
            )?);
        }
        let ai = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        match unsafe { self.d().allocate_descriptor_sets(&ai) } {
            Ok(sets) => {
                let ids = a.p_descriptor_sets.clone();
                for (id, s) in ids.iter().zip(sets.iter()) {
                    self.shared.objects().insert(id.0, HandleType::VkDescriptorSet, *s);
                }
                Ok(p::VkAllocateDescriptorSetsReply { result: OK, p_descriptor_sets: ids })
            }
            Err(e) => Ok(p::VkAllocateDescriptorSetsReply {
                result: fail(e),
                p_descriptor_sets: Vec::new(),
            }),
        }
    }

    fn update_descriptor_sets(
        &mut self,
        a: p::VkUpdateDescriptorSetsArgs,
    ) -> WireResult<()> {
        // The buffer infos have to outlive the call, so they are built first and
        // borrowed second — the lifetime problem a generated `raise_` pass would
        // have had to solve in general and that `ash`'s builders make the borrow
        // checker's business here.
        let mut buffer_infos: Vec<Vec<vk::DescriptorBufferInfo>> = Vec::new();
        let mut sets = Vec::new();
        for w in &a.p_descriptor_writes {
            let mut infos = Vec::with_capacity(w.p_buffer_info.len());
            for b in &w.p_buffer_info {
                let buf: vk::Buffer = self.h(HandleType::VkBuffer, b.buffer.0)?;
                infos.push(
                    vk::DescriptorBufferInfo::default()
                        .buffer(buf)
                        .offset(b.offset)
                        .range(b.range),
                );
            }
            buffer_infos.push(infos);
            sets.push(self.h::<vk::DescriptorSet>(HandleType::VkDescriptorSet, w.dst_set.0)?);
        }
        let mut writes = Vec::with_capacity(a.p_descriptor_writes.len());
        for (i, w) in a.p_descriptor_writes.iter().enumerate() {
            let mut dw = vk::WriteDescriptorSet::default()
                .dst_set(sets[i])
                .dst_binding(w.dst_binding)
                .dst_array_element(w.dst_array_element)
                .descriptor_type(vk::DescriptorType::from_raw(w.descriptor_type.0));
            if !buffer_infos[i].is_empty() {
                dw = dw.buffer_info(&buffer_infos[i]);
            }
            writes.push(dw);
        }
        unsafe { self.d().update_descriptor_sets(&writes, &[]) };
        Ok(())
    }

    fn create_pipeline_layout(
        &mut self,
        a: p::VkCreatePipelineLayoutArgs,
    ) -> WireResult<p::VkCreatePipelineLayoutReply> {
        let info = a.p_create_info.as_ref().ok_or_else(no_info)?;
        let mut layouts = Vec::with_capacity(info.p_set_layouts.len());
        for l in &info.p_set_layouts {
            layouts.push(self.h::<vk::DescriptorSetLayout>(
                HandleType::VkDescriptorSetLayout,
                l.0,
            )?);
        }
        let ranges: Vec<vk::PushConstantRange> = info
            .p_push_constant_ranges
            .iter()
            .map(|r| {
                vk::PushConstantRange::default()
                    .stage_flags(vk::ShaderStageFlags::from_raw(r.stage_flags.0))
                    .offset(r.offset)
                    .size(r.size)
            })
            .collect();
        let ci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&ranges);
        match unsafe { self.d().create_pipeline_layout(&ci, None) } {
            Ok(l) => {
                self.bind(a.p_pipeline_layout.map(|h| h.0), HandleType::VkPipelineLayout, l);
                Ok(p::VkCreatePipelineLayoutReply {
                    result: OK,
                    p_pipeline_layout: a.p_pipeline_layout,
                })
            }
            Err(e) => Ok(p::VkCreatePipelineLayoutReply {
                result: fail(e),
                p_pipeline_layout: None,
            }),
        }
    }

    fn destroy_pipeline_layout(
        &mut self,
        a: p::VkDestroyPipelineLayoutArgs,
    ) -> WireResult<()> {
        let l: vk::PipelineLayout =
            self.h(HandleType::VkPipelineLayout, a.pipeline_layout.0)?;
        unsafe { self.d().destroy_pipeline_layout(l, None) };
        self.shared.objects().remove(a.pipeline_layout.0);
        Ok(())
    }

    fn create_compute_pipelines(
        &mut self,
        a: p::VkCreateComputePipelinesArgs,
    ) -> WireResult<p::VkCreateComputePipelinesReply> {
        // `pName` is a NUL-terminated blob on the wire; `ash` wants a `&CStr`,
        // and the storage has to outlive the call.
        let mut names: Vec<std::ffi::CString> = Vec::new();
        let mut modules = Vec::new();
        let mut layouts = Vec::new();
        for ci in &a.p_create_infos {
            modules.push(self.h::<vk::ShaderModule>(
                HandleType::VkShaderModule,
                ci.stage.module.0,
            )?);
            layouts.push(self.h::<vk::PipelineLayout>(
                HandleType::VkPipelineLayout,
                ci.layout.0,
            )?);
            let raw: Vec<u8> =
                ci.stage.p_name.iter().copied().take_while(|b| *b != 0).collect();
            names.push(std::ffi::CString::new(raw).unwrap_or_default());
        }
        let infos: Vec<vk::ComputePipelineCreateInfo> = a
            .p_create_infos
            .iter()
            .enumerate()
            .map(|(i, ci)| {
                vk::ComputePipelineCreateInfo::default()
                    .flags(vk::PipelineCreateFlags::from_raw(ci.flags.0))
                    .stage(
                        vk::PipelineShaderStageCreateInfo::default()
                            // `VkShaderStageFlagBits` is an enum on the wire, so
                            // `i32`; the flags type it feeds is `u32`. The bits
                            // are the same and the cast is the whole conversion.
                            .stage(vk::ShaderStageFlags::from_raw(ci.stage.stage.0 as u32))
                            .module(modules[i])
                            .name(&names[i]),
                    )
                    .layout(layouts[i])
            })
            .collect();
        let r = unsafe {
            self.d().create_compute_pipelines(vk::PipelineCache::null(), &infos, None)
        };
        match r {
            Ok(pipes) => {
                let ids = a.p_pipelines.clone();
                for (id, pipe) in ids.iter().zip(pipes.iter()) {
                    self.shared.objects().insert(id.0, HandleType::VkPipeline, *pipe);
                }
                Ok(p::VkCreateComputePipelinesReply { result: OK, p_pipelines: ids })
            }
            Err((_, e)) => Ok(p::VkCreateComputePipelinesReply {
                result: fail(e),
                p_pipelines: Vec::new(),
            }),
        }
    }

    fn destroy_pipeline(&mut self, a: p::VkDestroyPipelineArgs) -> WireResult<()> {
        let pipe: vk::Pipeline = self.h(HandleType::VkPipeline, a.pipeline.0)?;
        unsafe { self.d().destroy_pipeline(pipe, None) };
        self.shared.objects().remove(a.pipeline.0);
        Ok(())
    }

    // -- command buffers -----------------------------------------------------

    fn create_command_pool(
        &mut self,
        a: p::VkCreateCommandPoolArgs,
    ) -> WireResult<p::VkCreateCommandPoolReply> {
        let info = a.p_create_info.as_ref().ok_or_else(no_info)?;
        let ci = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::from_raw(info.flags.0))
            // The app side advertises one queue family (§9.1's table has one),
            // and it is not necessarily the driver's index for it. Substituting
            // ours is the same class of translation as the memory-type table.
            .queue_family_index(self.shared.driver.queue_family);
        match unsafe { self.d().create_command_pool(&ci, None) } {
            Ok(pool) => {
                self.bind(a.p_command_pool.map(|h| h.0), HandleType::VkCommandPool, pool);
                Ok(p::VkCreateCommandPoolReply { result: OK, p_command_pool: a.p_command_pool })
            }
            Err(e) => {
                Ok(p::VkCreateCommandPoolReply { result: fail(e), p_command_pool: None })
            }
        }
    }

    fn destroy_command_pool(&mut self, a: p::VkDestroyCommandPoolArgs) -> WireResult<()> {
        let pool: vk::CommandPool = self.h(HandleType::VkCommandPool, a.command_pool.0)?;
        unsafe { self.d().destroy_command_pool(pool, None) };
        self.shared.objects().remove(a.command_pool.0);
        Ok(())
    }

    fn allocate_command_buffers(
        &mut self,
        a: p::VkAllocateCommandBuffersArgs,
    ) -> WireResult<p::VkAllocateCommandBuffersReply> {
        let info = a.p_allocate_info.as_ref().ok_or_else(no_info)?;
        let pool: vk::CommandPool = self.h(HandleType::VkCommandPool, info.command_pool.0)?;
        let ai = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::from_raw(info.level.0))
            .command_buffer_count(info.command_buffer_count.max(1));
        match unsafe { self.d().allocate_command_buffers(&ai) } {
            Ok(cbs) => {
                let ids = a.p_command_buffers.clone();
                for (id, cb) in ids.iter().zip(cbs.iter()) {
                    self.shared.objects().insert(id.0, HandleType::VkCommandBuffer, *cb);
                }
                Ok(p::VkAllocateCommandBuffersReply { result: OK, p_command_buffers: ids })
            }
            Err(e) => Ok(p::VkAllocateCommandBuffersReply {
                result: fail(e),
                p_command_buffers: Vec::new(),
            }),
        }
    }

    fn free_command_buffers(&mut self, a: p::VkFreeCommandBuffersArgs) -> WireResult<()> {
        let pool: vk::CommandPool = self.h(HandleType::VkCommandPool, a.command_pool.0)?;
        let mut cbs = Vec::new();
        for c in &a.p_command_buffers {
            cbs.push(self.h::<vk::CommandBuffer>(HandleType::VkCommandBuffer, c.0)?);
        }
        unsafe { self.d().free_command_buffers(pool, &cbs) };
        for c in &a.p_command_buffers {
            self.shared.objects().remove(c.0);
        }
        Ok(())
    }

    fn begin_command_buffer(
        &mut self,
        a: p::VkBeginCommandBufferArgs,
    ) -> WireResult<p::VkBeginCommandBufferReply> {
        let cb: vk::CommandBuffer = self.h(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        let flags = a
            .p_begin_info
            .as_ref()
            .map(|i| vk::CommandBufferUsageFlags::from_raw(i.flags.0))
            .unwrap_or_default();
        let bi = vk::CommandBufferBeginInfo::default().flags(flags);
        let r = unsafe { self.d().begin_command_buffer(cb, &bi) };
        Ok(p::VkBeginCommandBufferReply { result: res(r) })
    }

    fn end_command_buffer(
        &mut self,
        a: p::VkEndCommandBufferArgs,
    ) -> WireResult<p::VkEndCommandBufferReply> {
        let cb: vk::CommandBuffer = self.h(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        let r = unsafe { self.d().end_command_buffer(cb) };
        Ok(p::VkEndCommandBufferReply { result: res(r) })
    }

    fn reset_command_buffer(
        &mut self,
        a: p::VkResetCommandBufferArgs,
    ) -> WireResult<p::VkResetCommandBufferReply> {
        let cb: vk::CommandBuffer = self.h(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        let r = unsafe {
            self.d().reset_command_buffer(cb, vk::CommandBufferResetFlags::from_raw(a.flags.0))
        };
        Ok(p::VkResetCommandBufferReply { result: res(r) })
    }

    // -- recording -----------------------------------------------------------
    //
    // **These are forwarded one at a time, and §6.5 says they should not be.**
    // That section makes each `VkCommandBuffer` own an arena block, has
    // recording encode straight into it, and hands `(offset, len)` to the GPU
    // side as `svExecuteCommandBlob` for a restricted sub-decoder to walk —
    // genuinely zero-copy from record to execute. None of that is built, for the
    // same reason lazy slab import is not: it needs one of §6.3's thirteen
    // non-Vulkan commands, and none of them is generated.
    //
    // Forwarding individually is *correct* rather than merely expedient — the
    // device ring is ordered, so `vkBegin…`, every `vkCmd*` and `vkEnd…` arrive
    // in the order the application issued them, and the queue ring's barrier
    // orders the submit after all of it. What it costs is one ring packet and
    // one round of decode per `vkCmd*` instead of one per command buffer, which
    // is exactly the cost §6.5 exists to remove. It is a throughput decision
    // deferred, not a correctness one.

    fn cmd_bind_pipeline(&mut self, a: p::VkCmdBindPipelineArgs) -> WireResult<()> {
        let cb: vk::CommandBuffer = self.h(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        let pipe: vk::Pipeline = self.h(HandleType::VkPipeline, a.pipeline.0)?;
        unsafe {
            self.d().cmd_bind_pipeline(
                cb,
                vk::PipelineBindPoint::from_raw(a.pipeline_bind_point.0),
                pipe,
            )
        };
        Ok(())
    }

    fn cmd_bind_descriptor_sets(
        &mut self,
        a: p::VkCmdBindDescriptorSetsArgs,
    ) -> WireResult<()> {
        let cb: vk::CommandBuffer = self.h(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        let layout: vk::PipelineLayout = self.h(HandleType::VkPipelineLayout, a.layout.0)?;
        let mut sets = Vec::with_capacity(a.p_descriptor_sets.len());
        for s in &a.p_descriptor_sets {
            sets.push(self.h::<vk::DescriptorSet>(HandleType::VkDescriptorSet, s.0)?);
        }
        unsafe {
            self.d().cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::from_raw(a.pipeline_bind_point.0),
                layout,
                a.first_set,
                &sets,
                &a.p_dynamic_offsets,
            )
        };
        Ok(())
    }

    fn cmd_dispatch(&mut self, a: p::VkCmdDispatchArgs) -> WireResult<()> {
        let cb: vk::CommandBuffer = self.h(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        unsafe { self.d().cmd_dispatch(cb, a.group_count_x, a.group_count_y, a.group_count_z) };
        Ok(())
    }

    fn cmd_push_constants(&mut self, a: p::VkCmdPushConstantsArgs) -> WireResult<()> {
        let cb: vk::CommandBuffer = self.h(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        let layout: vk::PipelineLayout = self.h(HandleType::VkPipelineLayout, a.layout.0)?;
        unsafe {
            self.d().cmd_push_constants(
                cb,
                layout,
                vk::ShaderStageFlags::from_raw(a.stage_flags.0),
                a.offset,
                &a.p_values,
            )
        };
        Ok(())
    }

    fn cmd_fill_buffer(&mut self, a: p::VkCmdFillBufferArgs) -> WireResult<()> {
        let cb: vk::CommandBuffer = self.h(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        let b: vk::Buffer = self.h(HandleType::VkBuffer, a.dst_buffer.0)?;
        unsafe { self.d().cmd_fill_buffer(cb, b, a.dst_offset, a.size, a.data) };
        Ok(())
    }

    fn cmd_copy_buffer(&mut self, a: p::VkCmdCopyBufferArgs) -> WireResult<()> {
        let cb: vk::CommandBuffer = self.h(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        let src: vk::Buffer = self.h(HandleType::VkBuffer, a.src_buffer.0)?;
        let dst: vk::Buffer = self.h(HandleType::VkBuffer, a.dst_buffer.0)?;
        let regions: Vec<vk::BufferCopy> = a
            .p_regions
            .iter()
            .map(|r| {
                vk::BufferCopy::default()
                    .src_offset(r.src_offset)
                    .dst_offset(r.dst_offset)
                    .size(r.size)
            })
            .collect();
        unsafe { self.d().cmd_copy_buffer(cb, src, dst, &regions) };
        Ok(())
    }

    fn cmd_pipeline_barrier(&mut self, a: p::VkCmdPipelineBarrierArgs) -> WireResult<()> {
        let cb: vk::CommandBuffer = self.h(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        let mut buffers = Vec::with_capacity(a.p_buffer_memory_barriers.len());
        for b in &a.p_buffer_memory_barriers {
            let buf: vk::Buffer = self.h(HandleType::VkBuffer, b.buffer.0)?;
            buffers.push(
                vk::BufferMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::from_raw(b.src_access_mask.0))
                    .dst_access_mask(vk::AccessFlags::from_raw(b.dst_access_mask.0))
                    .src_queue_family_index(b.src_queue_family_index)
                    .dst_queue_family_index(b.dst_queue_family_index)
                    .buffer(buf)
                    .offset(b.offset)
                    .size(b.size),
            );
        }
        let memory: Vec<vk::MemoryBarrier> = a
            .p_memory_barriers
            .iter()
            .map(|m| {
                vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::from_raw(m.src_access_mask.0))
                    .dst_access_mask(vk::AccessFlags::from_raw(m.dst_access_mask.0))
            })
            .collect();
        unsafe {
            self.d().cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::from_raw(a.src_stage_mask.0),
                vk::PipelineStageFlags::from_raw(a.dst_stage_mask.0),
                vk::DependencyFlags::from_raw(a.dependency_flags.0),
                &memory,
                &buffers,
                &[],
            )
        };
        Ok(())
    }

    // -- submit and wait -----------------------------------------------------

    fn queue_submit(&mut self, a: p::VkQueueSubmitArgs) -> WireResult<p::VkQueueSubmitReply> {
        let q: vk::Queue = self.h(HandleType::VkQueue, a.queue.0)?;
        let fence: vk::Fence = self.h(HandleType::VkFence, a.fence.0)?;

        let mut all_cbs: Vec<Vec<vk::CommandBuffer>> = Vec::new();
        let mut all_wait: Vec<Vec<vk::Semaphore>> = Vec::new();
        let mut all_signal: Vec<Vec<vk::Semaphore>> = Vec::new();
        for s in &a.p_submits {
            let mut cbs = Vec::with_capacity(s.p_command_buffers.len());
            for c in &s.p_command_buffers {
                cbs.push(self.h::<vk::CommandBuffer>(HandleType::VkCommandBuffer, c.0)?);
            }
            all_cbs.push(cbs);
            let mut w = Vec::with_capacity(s.p_wait_semaphores.len());
            for x in &s.p_wait_semaphores {
                w.push(self.h::<vk::Semaphore>(HandleType::VkSemaphore, x.0)?);
            }
            all_wait.push(w);
            let mut g = Vec::with_capacity(s.p_signal_semaphores.len());
            for x in &s.p_signal_semaphores {
                g.push(self.h::<vk::Semaphore>(HandleType::VkSemaphore, x.0)?);
            }
            all_signal.push(g);
        }
        let stages: Vec<Vec<vk::PipelineStageFlags>> = a
            .p_submits
            .iter()
            .map(|s| {
                s.p_wait_dst_stage_mask
                    .iter()
                    .map(|f| vk::PipelineStageFlags::from_raw(f.0))
                    .collect()
            })
            .collect();

        let submits: Vec<vk::SubmitInfo> = (0..a.p_submits.len())
            .map(|i| {
                let mut s = vk::SubmitInfo::default().command_buffers(&all_cbs[i]);
                if !all_wait[i].is_empty() {
                    s = s.wait_semaphores(&all_wait[i]).wait_dst_stage_mask(&stages[i]);
                }
                if !all_signal[i].is_empty() {
                    s = s.signal_semaphores(&all_signal[i]);
                }
                s
            })
            .collect();
        let r = unsafe { self.d().queue_submit(q, &submits, fence) };
        Ok(p::VkQueueSubmitReply { result: res(r) })
    }

    fn queue_wait_idle(
        &mut self,
        a: p::VkQueueWaitIdleArgs,
    ) -> WireResult<p::VkQueueWaitIdleReply> {
        let q: vk::Queue = self.h(HandleType::VkQueue, a.queue.0)?;
        let r = unsafe { self.d().queue_wait_idle(q) };
        Ok(p::VkQueueWaitIdleReply { result: res(r) })
    }

    fn device_wait_idle(
        &mut self,
        _a: p::VkDeviceWaitIdleArgs,
    ) -> WireResult<p::VkDeviceWaitIdleReply> {
        let r = unsafe { self.d().device_wait_idle() };
        Ok(p::VkDeviceWaitIdleReply { result: res(r) })
    }

    fn create_fence(&mut self, a: p::VkCreateFenceArgs) -> WireResult<p::VkCreateFenceReply> {
        let flags = a
            .p_create_info
            .as_ref()
            .map(|i| vk::FenceCreateFlags::from_raw(i.flags.0))
            .unwrap_or_default();
        let ci = vk::FenceCreateInfo::default().flags(flags);
        match unsafe { self.d().create_fence(&ci, None) } {
            Ok(f) => {
                self.bind(a.p_fence.map(|h| h.0), HandleType::VkFence, f);
                Ok(p::VkCreateFenceReply { result: OK, p_fence: a.p_fence })
            }
            Err(e) => Ok(p::VkCreateFenceReply { result: fail(e), p_fence: None }),
        }
    }

    fn destroy_fence(&mut self, a: p::VkDestroyFenceArgs) -> WireResult<()> {
        let f: vk::Fence = self.h(HandleType::VkFence, a.fence.0)?;
        unsafe { self.d().destroy_fence(f, None) };
        self.shared.objects().remove(a.fence.0);
        Ok(())
    }

    fn wait_for_fences(
        &mut self,
        a: p::VkWaitForFencesArgs,
    ) -> WireResult<p::VkWaitForFencesReply> {
        let mut fences = Vec::with_capacity(a.p_fences.len());
        for f in &a.p_fences {
            fences.push(self.h::<vk::Fence>(HandleType::VkFence, f.0)?);
        }
        let r = unsafe { self.d().wait_for_fences(&fences, a.wait_all, a.timeout) };
        // A timeout is `VK_TIMEOUT`, which is a success code the application has
        // to see: mapping it onto an error would turn a poll into a failure.
        Ok(p::VkWaitForFencesReply {
            result: match r {
                Ok(()) => OK,
                Err(vk::Result::TIMEOUT) => p::VkResult(vk::Result::TIMEOUT.as_raw()),
                Err(e) => fail(e),
            },
        })
    }

    fn reset_fences(&mut self, a: p::VkResetFencesArgs) -> WireResult<p::VkResetFencesReply> {
        let mut fences = Vec::with_capacity(a.p_fences.len());
        for f in &a.p_fences {
            fences.push(self.h::<vk::Fence>(HandleType::VkFence, f.0)?);
        }
        let r = unsafe { self.d().reset_fences(&fences) };
        Ok(p::VkResetFencesReply { result: res(r) })
    }

    fn get_fence_status(
        &mut self,
        a: p::VkGetFenceStatusArgs,
    ) -> WireResult<p::VkGetFenceStatusReply> {
        let f: vk::Fence = self.h(HandleType::VkFence, a.fence.0)?;
        let r = unsafe { self.d().get_fence_status(f) };
        Ok(p::VkGetFenceStatusReply {
            result: match r {
                Ok(true) => OK,
                Ok(false) => p::VkResult(vk::Result::NOT_READY.as_raw()),
                Err(e) => fail(e),
            },
        })
    }

    fn create_semaphore(
        &mut self,
        a: p::VkCreateSemaphoreArgs,
    ) -> WireResult<p::VkCreateSemaphoreReply> {
        let ci = vk::SemaphoreCreateInfo::default();
        match unsafe { self.d().create_semaphore(&ci, None) } {
            Ok(s) => {
                self.bind(a.p_semaphore.map(|h| h.0), HandleType::VkSemaphore, s);
                Ok(p::VkCreateSemaphoreReply { result: OK, p_semaphore: a.p_semaphore })
            }
            Err(e) => Ok(p::VkCreateSemaphoreReply { result: fail(e), p_semaphore: None }),
        }
    }

    fn destroy_semaphore(&mut self, a: p::VkDestroySemaphoreArgs) -> WireResult<()> {
        let s: vk::Semaphore = self.h(HandleType::VkSemaphore, a.semaphore.0)?;
        unsafe { self.d().destroy_semaphore(s, None) };
        self.shared.objects().remove(a.semaphore.0);
        Ok(())
    }

    // -- coherency -----------------------------------------------------------
    //
    // §5.6 decides write-back and coherent on both sides, and Venus's vtest
    // `bo_flush`/`bo_invalidate` are literal no-ops for the same reason. Both
    // measured providers give `HOST_COHERENT` memory, so these are no-ops here
    // too — but they are *implemented* no-ops rather than refusals, because an
    // application that calls them must not be told the driver cannot.

    fn flush_mapped_memory_ranges(
        &mut self,
        _a: p::VkFlushMappedMemoryRangesArgs,
    ) -> WireResult<p::VkFlushMappedMemoryRangesReply> {
        Ok(p::VkFlushMappedMemoryRangesReply { result: OK })
    }

    fn invalidate_mapped_memory_ranges(
        &mut self,
        _a: p::VkInvalidateMappedMemoryRangesArgs,
    ) -> WireResult<p::VkInvalidateMappedMemoryRangesReply> {
        Ok(p::VkInvalidateMappedMemoryRangesReply { result: OK })
    }
}
