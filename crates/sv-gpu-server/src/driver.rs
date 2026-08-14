//! The real Vulkan driver, opened once, at startup.
//!
//! # Why the driver is open before any application connects
//!
//! docs/design.md §3 draws `sv-gpu-server` with `ash → Intel ICD → iGPU` hanging off
//! the ring consumers, which reads as "open a driver when the app side says
//! `vkCreateInstance`". That is the wrong order, and the arena is what decides
//! it: on the GPU-allocated arena path (see [`crate::arena`]) the driver has to
//! allocate and export the arena **before** the app side can attach at all,
//! because the descriptor it exports is the thing the app side is attaching to.
//!
//! So the driver is opened when the process starts, and the app side's
//! `vkCreateInstance` and `vkCreateDevice` *bind an id* to the instance and
//! device that already exist rather than creating new ones. That also matches
//! what we advertise: §8 gives one physical device, and §9 a curated table, so
//! there was never going to be a second device to create.
//!
//! # Extensions
//!
//! Enabled if present, never required, and each is recorded so the session can
//! say which arena provider is available:
//!
//! | extension | what it buys |
//! |---|---|
//! | `VK_KHR_external_memory_fd` | export the arena as a descriptor the app side maps |
//! | `VK_EXT_external_memory_dma_buf` | make that descriptor a dma-buf, which an unrelated process can map |
//! | `VK_EXT_external_memory_host` | import an arena the *app* side allocated |

use ash::vk;
use std::ffi::CStr;

/// Everything that can go wrong talking to a driver.
#[derive(Debug)]
pub enum DriverError {
    NoLoader(String),
    Vk(&'static str, vk::Result),
    Missing(&'static str),
    Other(String),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::NoLoader(e) => write!(f, "no Vulkan loader: {e}"),
            DriverError::Vk(what, r) => write!(f, "{what}: {r:?}"),
            DriverError::Missing(what) => write!(f, "the driver does not offer {what}"),
            DriverError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for DriverError {}

pub type Result<T> = std::result::Result<T, DriverError>;

fn vkerr(what: &'static str) -> impl Fn(vk::Result) -> DriverError {
    move |r| DriverError::Vk(what, r)
}

/// One open driver: instance, physical device, device, queue.
pub struct Driver {
    /// Held rather than read: it owns the loaded `libvulkan.so`, and dropping it
    /// while an instance is live would unload the loader out from under the
    /// driver. `gpu-probe` learned the same thing.
    #[allow(dead_code)]
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub pd: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    pub props: vk::PhysicalDeviceProperties,
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    /// What the loader on *this* side answers `vkEnumerateInstanceVersion` with,
    /// before [`Driver::open`] clamps it down to what the profile uses.
    ///
    /// Kept unclamped and reported unclamped over `svDriverVersions`, because
    /// this side's job is to say what the driver is, not what the app side may
    /// advertise. The clamp is `sv_gpu_server`'s business only for the instance
    /// it creates for itself; the *advertised* number is negotiated on the app
    /// side, once, in `sv_icd::state::negotiate` — see docs/design.md §0.18.
    pub instance_api_version: u32,
    pub name: String,
    /// `VK_KHR_external_memory_fd`, and its dispatch table.
    pub external_fd: Option<ash::khr::external_memory_fd::Device>,
    pub has_dma_buf: bool,
    /// `VK_EXT_external_memory_host`, and its dispatch table.
    pub external_host: Option<ash::ext::external_memory_host::Device>,
    /// `VK_EXT_debug_utils`, for the one command whose handle argument the
    /// generated passes cannot see (`server::set_debug_utils_object_name_ext`).
    pub debug_utils: Option<ash::ext::debug_utils::Device>,
    /// Whether `VkPhysicalDeviceVulkan12Features::bufferDeviceAddress` came back
    /// true and was therefore enabled on this device.
    ///
    /// Read by [`crate::arena`], and this is the whole reason it is a field. A
    /// buffer whose address a shader may take must be bound to memory allocated
    /// with `VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT`
    /// (`VUID-VkMemoryAllocateInfo-flags-03331` in the other direction:
    /// `VUID-VkBufferDeviceAddressInfo-buffer-02601`), and **the arena is one
    /// allocation made before any application connects**, so the decision has to
    /// be taken here or not at all. Setting the bit unconditionally is invalid
    /// usage on a driver without the feature, so it is conditional on this.
    pub buffer_device_address: bool,
}

impl Driver {
    /// Open device `index`, or the first one, and enable whatever it offers.
    pub fn open(index: usize) -> Result<Driver> {
        let entry =
            unsafe { ash::Entry::load() }.map_err(|e| DriverError::NoLoader(e.to_string()))?;
        // 1.1 was the floor the arena needed and it is **not** enough for Zink:
        // `scalarBlockLayout` and `timelineSemaphore` live in
        // `VkPhysicalDeviceVulkan12Features` and `dynamicRendering` in the 1.3
        // block, and a 1.1 instance cannot even name those structures. Ask for
        // what the loader says is available, capped at what the wire carries —
        // asking for more than the driver has makes `vkCreateInstance` fail
        // outright, and asking for more than the profile knows buys nothing.
        //
        // The cap used to be the literal 1.3 and is now
        // [`sv_proto::CORE_API_VERSION`], which *is* 1.3 today and is derived
        // from the profile. The difference matters the day the profile gains
        // `VK_VERSION_1_4`: this instance rises with it in the same edit rather
        // than being the one place that was forgotten. `max` against 1.1 because
        // a profile below that is a profile this side cannot serve at all, and
        // `clamp` panics rather than saturating when its bounds cross.
        let available = unsafe { entry.try_enumerate_instance_version() }
            .ok()
            .flatten()
            .unwrap_or(vk::API_VERSION_1_0);
        let ceiling = sv_proto::CORE_API_VERSION.max(vk::API_VERSION_1_1);
        let want = available.clamp(vk::API_VERSION_1_1, ceiling);
        let app = vk::ApplicationInfo::default()
            .application_name(c"sv-gpu-server")
            .api_version(want);
        // `VK_EXT_debug_utils` if the loader has it. Zink names every object it
        // creates, and without the extension enabled here
        // `vkSetDebugUtilsObjectNameEXT` resolves to null on this side and every
        // one of those calls is a forwarded command with nothing behind it.
        // It is an *instance* extension whose device entry points the loader
        // itself provides, so enabling it costs one string and nothing else.
        let have_debug_utils = unsafe { entry.enumerate_instance_extension_properties(None) }
            .map(|v| {
                v.iter().any(|e| {
                    let name = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) };
                    name == c"VK_EXT_debug_utils"
                })
            })
            .unwrap_or(false);
        // `VK_KHR_surface`, if the loader has it, for one reason and not for
        // presentation: `VK_KHR_swapchain` is in the profile (docs/design.md §10) so
        // that the device enables it and
        // `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR` exists, and a device extension may
        // only be enabled when its instance-level dependency is. This side
        // creates no surface and never will — the window is on the *other* side
        // of the boundary — so the extension is enabled purely to make the
        // layout legal.
        let have_surface = unsafe { entry.enumerate_instance_extension_properties(None) }
            .map(|v| {
                v.iter().any(|e| {
                    let name = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) };
                    name == c"VK_KHR_surface"
                })
            })
            .unwrap_or(false);
        let mut inst_exts: Vec<*const std::ffi::c_char> = Vec::new();
        if have_debug_utils {
            inst_exts.push(c"VK_EXT_debug_utils".as_ptr());
        }
        if have_surface {
            inst_exts.push(c"VK_KHR_surface".as_ptr());
        }
        let ci = vk::InstanceCreateInfo::default()
            .application_info(&app)
            .enabled_extension_names(&inst_exts);
        let instance =
            unsafe { entry.create_instance(&ci, None) }.map_err(vkerr("vkCreateInstance"))?;

        let opened = Driver::open_on(entry, instance, index, available);
        if opened.is_err() {
            // Nothing else holds the instance if device creation failed, and
            // leaking it would keep the driver's own threads alive for the life
            // of the process.
            //
            // (`open_on` takes the instance by value and destroys it on its own
            // error paths; this branch exists only to say so.)
        }
        opened
    }

    fn open_on(
        entry: ash::Entry,
        instance: ash::Instance,
        index: usize,
        instance_api_version: u32,
    ) -> Result<Driver> {
        let r = Driver::open_inner(&entry, &instance, index);
        match r {
            Ok((pd, device, queue, queue_family, external_fd, has_dma_buf, external_host, buffer_device_address)) => {
                let props = unsafe { instance.get_physical_device_properties(pd) };
                let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
                let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
                // Instance-level extension, device-level entry points, provided
                // by the loader rather than by the driver — so it is available
                // whenever the instance enabled it, which `open` does.
                let debug_utils = Some(ash::ext::debug_utils::Device::new(&instance, &device));
                Ok(Driver {
                    entry,
                    instance,
                    pd,
                    device,
                    queue,
                    queue_family,
                    props,
                    mem_props,
                    instance_api_version,
                    name,
                    external_fd,
                    has_dma_buf,
                    external_host,
                    debug_utils,
                    buffer_device_address,
                })
            }
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn open_inner(
        entry: &ash::Entry,
        instance: &ash::Instance,
        index: usize,
    ) -> Result<(
        vk::PhysicalDevice,
        ash::Device,
        vk::Queue,
        u32,
        Option<ash::khr::external_memory_fd::Device>,
        bool,
        Option<ash::ext::external_memory_host::Device>,
        bool,
    )> {
        let _ = entry;
        let pds =
            unsafe { instance.enumerate_physical_devices() }.map_err(vkerr("vkEnumeratePhysicalDevices"))?;
        let &pd = pds.get(index).ok_or_else(|| {
            DriverError::Other(format!("no physical device {index} ({} present)", pds.len()))
        })?;

        let exts = unsafe { instance.enumerate_device_extension_properties(pd) }
            .map_err(vkerr("vkEnumerateDeviceExtensionProperties"))?;
        let has = |want: &CStr| {
            exts.iter().any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == want)
        };
        let has_fd = has(c"VK_KHR_external_memory_fd");
        let has_dma_buf = has_fd && has(c"VK_EXT_external_memory_dma_buf");
        let has_host = has(c"VK_EXT_external_memory_host");
        let _ = &has;

        let qfams = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        // COMPUTE and TRANSFER, because the acceptance workload is a dispatch
        // and the present path is a copy. Any family with COMPUTE has TRANSFER
        // implicitly per the specification, but asking says what we need.
        let queue_family = qfams
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .ok_or(DriverError::Missing("a compute-capable queue family"))? as u32;

        let prio = [1.0f32];
        let qi = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&prio)];

        // Every extension the profile forwards that this driver actually has,
        // plus the three the arena needs.
        //
        // # Why everything, rather than what the application asked for
        //
        // The GPU side opens its device at startup, before any application
        // connects, because on the export arena path the arena *is* a driver
        // allocation (see this module's header). So by the time
        // `vkCreateDevice` arrives the device exists and its enabled set is
        // fixed. The only way for the answer to "did I get what I asked for" to
        // be yes is to have enabled everything the driver could have said yes
        // to. This is a real limitation of opening early, and it is recorded in
        // `server::create_device` from the other end.
        let mut owned: Vec<std::ffi::CString> = Vec::new();
        for want in sv_gpu_entry::PROFILE_DEVICE_EXTENSIONS {
            let Ok(c) = std::ffi::CString::new(want) else { continue };
            if has(c.as_c_str()) {
                owned.push(c);
            }
        }
        for want in [
            c"VK_KHR_external_memory_fd",
            c"VK_EXT_external_memory_dma_buf",
            c"VK_EXT_external_memory_host",
        ] {
            if has(want) && !owned.iter().any(|o| o.as_c_str() == want) {
                owned.push(want.to_owned());
            }
        }
        let names: Vec<*const std::ffi::c_char> = owned.iter().map(|c| c.as_ptr()).collect();

        // The features likewise: ask the driver what it supports and enable all
        // of it. `VkPhysicalDeviceFeatures2` with the 1.1/1.2/1.3 blocks chained
        // is the whole set that matters to Zink — `robustBufferAccess`,
        // `logicOp`, `scalarBlockLayout`, `timelineSemaphore`,
        // `dynamicRendering`, `nullDescriptor` and the rest all live in one of
        // these four structures or in an extension one, and an extension feature
        // struct is enabled by default when its extension is.
        let mut f11 = vk::PhysicalDeviceVulkan11Features::default();
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
        let mut rb2 = vk::PhysicalDeviceRobustness2FeaturesEXT::default();
        let mut xfb = vk::PhysicalDeviceTransformFeedbackFeaturesEXT::default();
        let mut cond = vk::PhysicalDeviceConditionalRenderingFeaturesEXT::default();
        let mut prov = vk::PhysicalDeviceProvokingVertexFeaturesEXT::default();
        let mut line = vk::PhysicalDeviceLineRasterizationFeaturesEXT::default();
        let mut vdiv = vk::PhysicalDeviceVertexAttributeDivisorFeaturesEXT::default();
        let mut eds = vk::PhysicalDeviceExtendedDynamicStateFeaturesEXT::default();
        let mut eds2 = vk::PhysicalDeviceExtendedDynamicState2FeaturesEXT::default();
        let mut dclip = vk::PhysicalDeviceDepthClipEnableFeaturesEXT::default();
        let mut bcs = vk::PhysicalDeviceBorderColorSwizzleFeaturesEXT::default();
        let mut cbc = vk::PhysicalDeviceCustomBorderColorFeaturesEXT::default();
        let mut v2d3 = vk::PhysicalDeviceImage2DViewOf3DFeaturesEXT::default();
        let mut md = vk::PhysicalDeviceMultiDrawFeaturesEXT::default();
        let mut cwe = vk::PhysicalDeviceColorWriteEnableFeaturesEXT::default();
        let mut nscm = vk::PhysicalDeviceNonSeamlessCubeMapFeaturesEXT::default();
        let mut pgq = vk::PhysicalDevicePrimitivesGeneratedQueryFeaturesEXT::default();
        let mut m5 = vk::PhysicalDeviceMaintenance5FeaturesKHR::default();
        // The six extensions docs/design.md §0.20 added to the profile that carry a
        // feature struct. Without the struct chained the feature comes back
        // false and is *enabled* false, so an application that asks for
        // `VK_EXT_graphics_pipeline_library` finds the extension present and the
        // feature off — which is a worse answer than not advertising it, because
        // it fails at `vkCreateGraphicsPipelines` instead of at
        // `vkCreateDevice`. Adding the extension string and forgetting this is
        // the shape of mistake `chain_if!` exists to make cheap to avoid.
        let mut fsr = vk::PhysicalDeviceFragmentShadingRateFeaturesKHR::default();
        let mut gpl = vk::PhysicalDeviceGraphicsPipelineLibraryFeaturesEXT::default();
        let mut sucf = vk::PhysicalDeviceShaderSubgroupUniformControlFlowFeaturesKHR::default();
        let mut bary = vk::PhysicalDeviceFragmentShaderBarycentricFeaturesKHR::default();
        let mut ai64 = vk::PhysicalDeviceShaderImageAtomicInt64FeaturesEXT::default();
        let mut dbc = vk::PhysicalDeviceDepthBiasControlFeaturesEXT::default();
        let enabled = |name: &CStr| owned.iter().any(|o| o.as_c_str() == name);
        let mut f2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut f11)
            .push_next(&mut f12)
            .push_next(&mut f13);
        // Only chain a feature struct whose extension was enabled: a driver is
        // permitted to reject `vkGetPhysicalDeviceFeatures2` chains it does not
        // recognise, and enabling a feature from an extension that is not in
        // `ppEnabledExtensionNames` is invalid usage the validation layer will
        // fail the call for.
        macro_rules! chain_if {
            ($name:expr, $s:expr) => {
                if enabled($name) {
                    f2 = f2.push_next(&mut $s);
                }
            };
        }
        chain_if!(c"VK_EXT_robustness2", rb2);
        chain_if!(c"VK_EXT_transform_feedback", xfb);
        chain_if!(c"VK_EXT_conditional_rendering", cond);
        chain_if!(c"VK_EXT_provoking_vertex", prov);
        chain_if!(c"VK_EXT_line_rasterization", line);
        chain_if!(c"VK_EXT_vertex_attribute_divisor", vdiv);
        chain_if!(c"VK_EXT_extended_dynamic_state", eds);
        chain_if!(c"VK_EXT_extended_dynamic_state2", eds2);
        chain_if!(c"VK_EXT_depth_clip_enable", dclip);
        chain_if!(c"VK_EXT_border_color_swizzle", bcs);
        chain_if!(c"VK_EXT_custom_border_color", cbc);
        chain_if!(c"VK_EXT_image_2d_view_of_3d", v2d3);
        chain_if!(c"VK_EXT_multi_draw", md);
        chain_if!(c"VK_EXT_color_write_enable", cwe);
        chain_if!(c"VK_EXT_non_seamless_cube_map", nscm);
        chain_if!(c"VK_EXT_primitives_generated_query", pgq);
        chain_if!(c"VK_KHR_maintenance5", m5);
        chain_if!(c"VK_KHR_fragment_shading_rate", fsr);
        chain_if!(c"VK_EXT_graphics_pipeline_library", gpl);
        chain_if!(c"VK_KHR_shader_subgroup_uniform_control_flow", sucf);
        chain_if!(c"VK_KHR_fragment_shader_barycentric", bary);
        chain_if!(c"VK_EXT_shader_image_atomic_int64", ai64);
        chain_if!(c"VK_EXT_depth_bias_control", dbc);
        unsafe { instance.get_physical_device_features2(pd, &mut f2) };

        let dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qi)
            .enabled_extension_names(&names)
            .push_next(&mut f2);
        let device =
            unsafe { instance.create_device(pd, &dci, None) }.map_err(vkerr("vkCreateDevice"))?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        // Read out of the *answer* `vkCreateDevice` was just given, so the two
        // cannot disagree: §0.4 item 9 hands the physical device's own features
        // straight back as the enabled set, which is what makes "reported" and
        // "enabled" the same word here. Recording it is what lets `crate::arena`
        // allocate memory a shader may take the address of, and what turns
        // docs/design.md §0.16 item 3's "advertised and not implemented" into
        // implemented rather than into a retraction.
        //
        // After `vkCreateDevice` rather than before it, because `f12` is
        // mutably borrowed by the chain until then, and the borrow checker is
        // right: reading a `pNext` link while the chain it is in is still lent
        // out is exactly the aliasing this project forbids everywhere else.
        let bda = f12.buffer_device_address != 0;

        let external_fd =
            has_fd.then(|| ash::khr::external_memory_fd::Device::new(instance, &device));
        let external_host =
            has_host.then(|| ash::ext::external_memory_host::Device::new(instance, &device));
        Ok((pd, device, queue, queue_family, external_fd, has_dma_buf, external_host, bda))
    }

    /// The driver's own name, as `sv_arena::import::path_for_driver` spells it.
    ///
    /// Matched on the device name because `VkPhysicalDeviceDriverProperties` is a
    /// 1.2 query and this instance is 1.1. The names are the ones that module's
    /// table already knows.
    pub fn driver_key(&self) -> &'static str {
        let n = self.name.to_ascii_lowercase();
        if n.contains("llvmpipe") || n.contains("lavapipe") {
            "lavapipe"
        } else if n.contains("radv") {
            "radv"
        } else if n.contains("nvidia") {
            "nvidia"
        } else if n.contains("intel") {
            "intel"
        } else {
            "unknown"
        }
    }

    /// Resolve every entry point of the profile from this driver.
    ///
    /// Instance-level names come from `vkGetInstanceProcAddr` and device-level
    /// ones from `vkGetDeviceProcAddr`. The distinction is not cosmetic: the
    /// loader's instance table answers device commands with a *trampoline* that
    /// dispatches on the handle, and we are calling a driver directly with a
    /// handle the loader has never seen.
    pub fn load_fns(&self) -> sv_gpu_entry::DriverFns {
        let inst_fp = self.entry.static_fn().get_instance_proc_addr;
        let dev_fp = self.instance.fp_v1_0().get_device_proc_addr;
        let inst_handle = self.instance.handle();
        let dev_handle = self.device.handle();
        let by_instance = |name: &str| -> Option<unsafe extern "system" fn()> {
            let c = std::ffi::CString::new(name).ok()?;
            unsafe { inst_fp(inst_handle, c.as_ptr()) }
        };
        let by_device = |name: &str| -> Option<unsafe extern "system" fn()> {
            let c = std::ffi::CString::new(name).ok()?;
            // A device-level name the driver does not have comes back null from
            // `vkGetDeviceProcAddr`, and so does one whose extension was not
            // enabled — which is the same thing from our point of view, since
            // `open_inner` enables everything the driver offers.
            match unsafe { dev_fp(dev_handle, c.as_ptr()) } {
                Some(f) => Some(f),
                None => by_instance(name),
            }
        };
        sv_gpu_entry::DriverFns::load(&by_instance, &by_device)
    }

    /// The first memory type satisfying `want`, or `None`.
    pub fn memory_type(&self, mask: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|i| {
            mask & (1 << i) != 0
                && self.mem_props.memory_types[*i as usize].property_flags.contains(want)
        })
    }
}

/// The device extensions this GPU side may advertise: the profile's, restricted
/// to what the driver actually has, with the driver's own `specVersion`.
///
/// Both halves of the intersection are load-bearing and for different reasons.
/// The profile half is docs/design.md §9.2's rule — "an extension in this list is a
/// promise that its entry points work", and an entry point works only if pass 12
/// emitted a forwarder. The driver half is obvious and is the one a hand-written
/// list would have got right anyway.
pub fn forwardable_device_extensions(d: &Driver) -> Vec<(String, u32)> {
    let exts = match unsafe { d.instance.enumerate_device_extension_properties(d.pd) } {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for want in sv_gpu_entry::PROFILE_DEVICE_EXTENSIONS {
        for e in &exts {
            let name = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) };
            if name.to_bytes() == want.as_bytes() {
                out.push(((*want).to_string(), e.spec_version));
                break;
            }
        }
    }
    out
}

impl Drop for Driver {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
