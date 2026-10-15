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

/// One queue family the GPU side offers to applications, and the one queue it
/// opened on it.
///
/// # Why the index is the driver's own (docs/video-decode.md §7)
///
/// With video decode served, the families an application sees are the driver's
/// families `0..=max(compute, decode)`, **at their own indices**, each with one
/// queue. The alternative — a compact list of two with the decode family
/// renumbered to 1 — would make every `queueFamilyIndex` anywhere in the API a
/// value to translate: `VkCommandPoolCreateInfo`, `VkVideoSessionCreateInfoKHR`,
/// `pQueueFamilyIndices` of every concurrent buffer and image, and the
/// `src`/`dstQueueFamilyIndex` of every barrier inside a recorded command
/// buffer. None of those is typed as a family index in `vk.xml`, so no
/// generated pass could find them. Keeping the driver's numbering means there
/// is nothing to translate, and the price is that an application also sees the
/// families in between (RADV's async-compute family 1, NVIDIA's transfer and
/// compute families 1 and 2) — which are real queues on real families, opened
/// like the others, so seeing them is true rather than merely harmless.
#[derive(Clone, Copy, Debug)]
pub struct ServedFamily {
    /// The driver's properties for this family, with the capability bits this
    /// GPU side does not serve masked out (see [`SERVED_QUEUE_FLAGS`]) and
    /// `queueCount` set to the one queue that exists.
    pub props: vk::QueueFamilyProperties,
    /// `VkQueueFamilyVideoPropertiesKHR::videoCodecOperations`, restricted to
    /// the decode codecs whose extension this device enabled. Empty for a
    /// family that is not served for video.
    pub video_codec_operations: vk::VideoCodecOperationFlagsKHR,
    /// `VkQueueFamilyQueryResultStatusPropertiesKHR::queryResultStatusSupport`.
    pub query_result_status: bool,
    /// The queue opened on this family, index 0.
    pub queue: vk::Queue,
}

/// The queue capabilities a served family may keep. Everything else is masked:
/// `VIDEO_ENCODE` because encode is not part of this stage and an advertised
/// encode family would bring `VK_KHR_video_encode_queue` back through
/// `sv_icd::advertise::unservable_queue_requirement` with nothing measured
/// behind it; `PROTECTED` because protected content is out of scope by
/// decision (docs/design.md §0.21.4); `OPTICAL_FLOW_NV` and the rest because no
/// stage has asked for them.
pub const SERVED_QUEUE_FLAGS: vk::QueueFlags = vk::QueueFlags::from_raw(
    vk::QueueFlags::GRAPHICS.as_raw()
        | vk::QueueFlags::COMPUTE.as_raw()
        | vk::QueueFlags::TRANSFER.as_raw()
        | vk::QueueFlags::SPARSE_BINDING.as_raw()
        | vk::QueueFlags::VIDEO_DECODE_KHR.as_raw(),
);

/// The decode codec each extension stands for, so that a family's
/// `videoCodecOperations` names only codecs this device enabled. A driver whose
/// decode family does VP9 while the profile (or the driver's own extension
/// list) has no `VK_KHR_video_decode_vp9` must not tell an application it can.
pub const DECODE_CODEC_EXTENSIONS: &[(&str, vk::VideoCodecOperationFlagsKHR)] = &[
    ("VK_KHR_video_decode_h264", vk::VideoCodecOperationFlagsKHR::DECODE_H264),
    ("VK_KHR_video_decode_h265", vk::VideoCodecOperationFlagsKHR::DECODE_H265),
    ("VK_KHR_video_decode_av1", vk::VideoCodecOperationFlagsKHR::DECODE_AV1),
    ("VK_KHR_video_decode_vp9", vk::VideoCodecOperationFlagsKHR::from_raw(0x0000_0008)),
];

/// Which families to serve, from the driver's family table: the pure decision,
/// separated from the Vulkan calls so that it can be tested.
///
/// Returns the driver family indices to open a queue on, in order, and the
/// decode family if one is served. Without decode it is exactly the one
/// compute family the GPU side always opened — the behaviour before this stage,
/// unchanged, so that `SV_VIDEO_DECODE=0` or a driver without decode puts back
/// what every other gate was measured against.
pub fn choose_families(
    flags: &[vk::QueueFlags],
    codec_ops: &[vk::VideoCodecOperationFlagsKHR],
    decode_wanted: bool,
) -> Option<(Vec<u32>, Option<u32>)> {
    let compute = flags.iter().position(|q| q.contains(vk::QueueFlags::COMPUTE))? as u32;
    let decode = decode_wanted
        .then(|| {
            flags.iter().zip(codec_ops).position(|(q, ops)| {
                q.contains(vk::QueueFlags::VIDEO_DECODE_KHR) && !ops.is_empty()
            })
        })
        .flatten()
        .map(|i| i as u32);
    match decode {
        // The prefix, so that every served family is at its own index.
        Some(d) => Some(((0..=compute.max(d)).collect(), Some(d))),
        None => Some((vec![compute], None)),
    }
}

/// Whether video decode is wanted at all. On unless `SV_VIDEO_DECODE=0`, so
/// that the shape before this stage stays one variable away for anyone
/// bisecting a regression to it.
pub fn decode_wanted_by_environment() -> bool {
    std::env::var("SV_VIDEO_DECODE").map(|v| v != "0").unwrap_or(true)
}

/// What [`Driver::open_inner`] opened, before the instance-level parts join it.
/// A struct because the tuple it replaces had grown to eight unnamed fields.
struct Opened {
    pd: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    served: Vec<ServedFamily>,
    decode_family: Option<u32>,
    external_fd: Option<ash::khr::external_memory_fd::Device>,
    has_dma_buf: bool,
    external_host: Option<ash::ext::external_memory_host::Device>,
    buffer_device_address: bool,
}

/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VIDEO_DECODE_VP9_FEATURES_KHR`, vulkan_core.h 1.4.350.
const STRUCTURE_TYPE_VIDEO_DECODE_VP9_FEATURES_KHR: i32 = 1_000_514_000;
/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VIDEO_MAINTENANCE_2_FEATURES_KHR`, vulkan_core.h 1.4.350.
const STRUCTURE_TYPE_VIDEO_MAINTENANCE_2_FEATURES_KHR: i32 = 1_000_586_000;

/// A feature struct of the one shape both of the above have:
/// `{ sType, pNext, VkBool32 }`. `#[repr(C)]` so that it is that shape.
#[repr(C)]
struct RawFeature {
    s_type: vk::StructureType,
    p_next: *mut std::ffi::c_void,
    /// Written by `vkGetPhysicalDeviceFeatures2` and read back by the driver
    /// at `vkCreateDevice`; nothing on this side looks at it.
    #[allow(dead_code)]
    enabled: vk::Bool32,
}

impl RawFeature {
    fn new(s_type: i32) -> RawFeature {
        RawFeature {
            s_type: vk::StructureType::from_raw(s_type),
            p_next: std::ptr::null_mut(),
            enabled: vk::FALSE,
        }
    }
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
    /// The families applications see, by the index they see them at.
    ///
    /// One entry — the compute family above, presented as family 0 — unless
    /// video decode is served, in which case it is the driver's own prefix
    /// `0..=max(compute, decode)` (see [`ServedFamily`]). `queue` and
    /// `queue_family` stay what the GPU side uses for its own work (the arena
    /// probe, `svPresentBlit`); this is what `vkGetDeviceQueue` hands out.
    pub served: Vec<ServedFamily>,
    /// The served decode family, if any, by its (driver and application) index.
    pub decode_family: Option<u32>,
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
            Ok(Opened {
                pd,
                device,
                queue,
                queue_family,
                served,
                decode_family,
                external_fd,
                has_dma_buf,
                external_host,
                buffer_device_address,
            }) => {
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
                    served,
                    decode_family,
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

    fn open_inner(entry: &ash::Entry, instance: &ash::Instance, index: usize) -> Result<Opened> {

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
        // The video half of the family table, asked only of a driver that has
        // the extension: `VkQueueFamilyVideoPropertiesKHR` in a chain the driver
        // does not know is invalid usage. Count-then-fill through the `2` form,
        // which a 1.1 instance has.
        let has_video = has(c"VK_KHR_video_queue") && has(c"VK_KHR_video_decode_queue");
        let (codec_ops, result_status): (Vec<_>, Vec<_>) = if has_video {
            let n = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
            let mut vids = vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
            let mut qrs = vec![vk::QueueFamilyQueryResultStatusPropertiesKHR::default(); n];
            let mut fams = vec![vk::QueueFamilyProperties2::default(); n];
            for ((f, v), q) in fams.iter_mut().zip(vids.iter_mut()).zip(qrs.iter_mut()) {
                *f = f.push_next(v).push_next(q);
            }
            unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut fams) };
            drop(fams);
            (
                vids.iter().map(|v| v.video_codec_operations).collect(),
                qrs.iter().map(|q| q.query_result_status_support != 0).collect(),
            )
        } else {
            (
                vec![vk::VideoCodecOperationFlagsKHR::empty(); qfams.len()],
                vec![false; qfams.len()],
            )
        };
        // Decode is served when the driver has the extensions, the profile
        // forwards them, and nobody turned it off. The profile half matters:
        // a build whose profile lacks `VK_KHR_video_decode_queue` has no
        // forwarders for its commands, and opening a family nothing can record
        // on would be §0.22's over-promise from the other end.
        let profile_has = |n: &str| sv_gpu_entry::PROFILE_DEVICE_EXTENSIONS.contains(&n);
        let decode_wanted = has_video
            && profile_has("VK_KHR_video_queue")
            && profile_has("VK_KHR_video_decode_queue")
            && decode_wanted_by_environment();
        let flags: Vec<vk::QueueFlags> = qfams.iter().map(|q| q.queue_flags).collect();
        // COMPUTE and TRANSFER, because the acceptance workload is a dispatch
        // and the present path is a copy. Any family with COMPUTE has TRANSFER
        // implicitly per the specification, but asking says what we need.
        let (open_on, decode_family) = choose_families(&flags, &codec_ops, decode_wanted)
            .ok_or(DriverError::Missing("a compute-capable queue family"))?;
        let queue_family = flags
            .iter()
            .position(|q| q.contains(vk::QueueFlags::COMPUTE))
            .ok_or(DriverError::Missing("a compute-capable queue family"))? as u32;

        let prio = [1.0f32];
        let qi: Vec<vk::DeviceQueueCreateInfo> = open_on
            .iter()
            .map(|&f| {
                vk::DeviceQueueCreateInfo::default().queue_family_index(f).queue_priorities(&prio)
            })
            .collect();

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
        // **Close the enabled set under `depends=`**, against the two sets that
        // can satisfy one: the device extensions about to be enabled, and the
        // *instance* extensions this instance actually enabled above.
        //
        // Enabling "every profile extension the driver has" is not the same as
        // enabling a legal set, and the validation layer said so —
        // `VUID-vkCreateDevice-ppEnabledExtensionNames-01387`, three times:
        // `VK_EXT_display_control` without `VK_EXT_display_surface_counter`,
        // `VK_EXT_present_timing` without `VK_KHR_get_surface_capabilities2`,
        // `VK_EXT_swapchain_maintenance1` without `VK_EXT_surface_maintenance1`.
        // Every one of those dependencies is an **instance** extension, so no
        // amount of looking at the device list could have found them; this is
        // the same "right about the extension, wrong about the set it reads"
        // shape `sv_icd`'s own closure records (docs/design.md §0.23).
        //
        // A fixpoint rather than one pass, because dependency chains are deeper
        // than one link and a single pass keeps whatever depended on what it
        // just removed.
        // The two `Driver::open` enables, re-derived by the same rule it used —
        // "available on this loader" — rather than threaded through, so the two
        // places cannot say different things about one instance.
        let instance_enabled: Vec<&str> = {
            let avail = unsafe { entry.enumerate_instance_extension_properties(None) }
                .unwrap_or_default();
            let has_inst = |want: &CStr| {
                avail
                    .iter()
                    .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == want)
            };
            let mut v = Vec::new();
            if has_inst(c"VK_EXT_debug_utils") {
                v.push("VK_EXT_debug_utils");
            }
            if has_inst(c"VK_KHR_surface") {
                v.push("VK_KHR_surface");
            }
            v
        };
        loop {
            let present: Vec<String> =
                owned.iter().filter_map(|c| c.to_str().ok().map(str::to_string)).collect();
            let have = |n: &str| {
                // A core version satisfies a `depends=` on itself; this device
                // is created against the instance's own version, capped at 1.3.
                if let Some(minor) =
                    n.strip_prefix("VK_VERSION_1_").and_then(|m| m.parse::<u32>().ok())
                {
                    return minor <= 3;
                }
                present.iter().any(|o| o == n) || instance_enabled.contains(&n)
            };
            let drop: Vec<String> = present
                .iter()
                .filter(|n| !sv_gpu_entry::depends_satisfied(n, &have))
                .cloned()
                .collect();
            if drop.is_empty() {
                break;
            }
            owned.retain(|c| !c.to_str().is_ok_and(|s| drop.iter().any(|d| d == s)));
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
        // The video extensions with a feature struct. Same argument as the six
        // above, and sharper: `VK_KHR_video_decode_vp9`'s whole capability is
        // its one feature bit, so the extension enabled with the feature off is
        // an extension that refuses every VP9 profile.
        let mut vm1 = vk::PhysicalDeviceVideoMaintenance1FeaturesKHR::default();
        chain_if!(c"VK_KHR_video_maintenance1", vm1);
        // Two that `ash` 0.38 (1.3.281) predates. Each is `sType`, `pNext` and
        // one `VkBool32` (vulkan_core.h, 1.4.350), and they are spliced onto the
        // front of the chain by hand rather than through `push_next`, which
        // only takes the types `ash` knows.
        let mut vp9 = RawFeature::new(STRUCTURE_TYPE_VIDEO_DECODE_VP9_FEATURES_KHR);
        let mut vm2 = RawFeature::new(STRUCTURE_TYPE_VIDEO_MAINTENANCE_2_FEATURES_KHR);
        if enabled(c"VK_KHR_video_decode_vp9") {
            vp9.p_next = f2.p_next;
            f2.p_next = (&raw mut vp9).cast();
        }
        if enabled(c"VK_KHR_video_maintenance2") {
            vm2.p_next = f2.p_next;
            f2.p_next = (&raw mut vm2).cast();
        }
        unsafe { instance.get_physical_device_features2(pd, &mut f2) };

        let dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qi)
            .enabled_extension_names(&names)
            .push_next(&mut f2);
        let device =
            unsafe { instance.create_device(pd, &dci, None) }.map_err(vkerr("vkCreateDevice"))?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        // The codecs this device enabled, for masking the decode family's
        // `videoCodecOperations` — an application told "VP9" by the family and
        // refused `VK_KHR_video_decode_vp9` by the extension list has been told
        // two different things.
        let codecs = DECODE_CODEC_EXTENSIONS
            .iter()
            .filter(|(n, _)| owned.iter().any(|o| o.to_bytes() == n.as_bytes()))
            .fold(vk::VideoCodecOperationFlagsKHR::empty(), |acc, (_, op)| acc | *op);
        let served: Vec<ServedFamily> = open_on
            .iter()
            .map(|&f| {
                let mut props = qfams[f as usize];
                props.queue_count = 1;
                let decode = Some(f) == decode_family;
                if decode_family.is_some() {
                    props.queue_flags &= SERVED_QUEUE_FLAGS;
                }
                ServedFamily {
                    props,
                    video_codec_operations: if decode {
                        codec_ops[f as usize] & codecs
                    } else {
                        vk::VideoCodecOperationFlagsKHR::empty()
                    },
                    query_result_status: decode && result_status[f as usize],
                    queue: unsafe { device.get_device_queue(f, 0) },
                }
            })
            .collect();
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
        Ok(Opened {
            pd,
            device,
            queue,
            queue_family,
            served,
            decode_family,
            external_fd,
            has_dma_buf,
            external_host,
            buffer_device_address: bda,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use vk::QueueFlags as Q;
    use vk::VideoCodecOperationFlagsKHR as Op;

    const G: Q = Q::from_raw(Q::GRAPHICS.as_raw() | Q::COMPUTE.as_raw() | Q::TRANSFER.as_raw());

    /// RADV's family table as docs/video-decode.md §1 measured it.
    fn radv() -> (Vec<Q>, Vec<Op>) {
        let none = Op::empty();
        (
            vec![G, Q::COMPUTE | Q::TRANSFER, Q::VIDEO_DECODE_KHR, Q::VIDEO_ENCODE_KHR, Q::SPARSE_BINDING],
            vec![none, none, Op::DECODE_H264 | Op::DECODE_H265 | Op::DECODE_AV1, Op::ENCODE_H264, none],
        )
    }

    #[test]
    fn without_decode_the_one_compute_family_is_served_exactly_as_before() {
        let (f, o) = radv();
        assert_eq!(choose_families(&f, &o, false), Some((vec![0], None)));
    }

    #[test]
    fn with_decode_the_served_families_are_the_drivers_own_prefix() {
        // RADV: decode is family 2, so 0, 1 and 2 are served at their own
        // indices and the encode family after it is not.
        let (f, o) = radv();
        assert_eq!(choose_families(&f, &o, true), Some((vec![0, 1, 2], Some(2))));
        // NVIDIA: decode is family 3, after transfer and compute families.
        let n = vec![G, Q::TRANSFER, Q::COMPUTE | Q::TRANSFER, Q::TRANSFER | Q::VIDEO_DECODE_KHR];
        let no = vec![Op::empty(), Op::empty(), Op::empty(), Op::DECODE_H264];
        assert_eq!(choose_families(&n, &no, true), Some((vec![0, 1, 2, 3], Some(3))));
        // The guest Intel: decode is family 1 and the prefix is the whole table.
        let i = vec![G, Q::VIDEO_DECODE_KHR];
        let io = vec![Op::empty(), Op::DECODE_H264];
        assert_eq!(choose_families(&i, &io, true), Some((vec![0, 1], Some(1))));
    }

    #[test]
    fn a_decode_family_that_names_no_codec_is_not_a_decode_family() {
        let f = vec![G, Q::VIDEO_DECODE_KHR];
        let o = vec![Op::empty(), Op::empty()];
        assert_eq!(choose_families(&f, &o, true), Some((vec![0], None)));
    }

    #[test]
    fn no_compute_family_is_no_device() {
        assert_eq!(choose_families(&[Q::VIDEO_DECODE_KHR], &[Op::DECODE_H264], true), None);
    }

    #[test]
    fn encode_and_protected_are_not_served_capabilities() {
        assert!(!SERVED_QUEUE_FLAGS.contains(Q::VIDEO_ENCODE_KHR));
        assert!(!SERVED_QUEUE_FLAGS.contains(Q::PROTECTED));
        assert!(SERVED_QUEUE_FLAGS.contains(Q::VIDEO_DECODE_KHR | G));
    }
}
