//! The real Vulkan driver, opened once, at startup.
//!
//! # Why the driver is open before any application connects
//!
//! design.md §3 draws `sv-gpu-server` with `ash → Intel ICD → iGPU` hanging off
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
    pub name: String,
    /// `VK_KHR_external_memory_fd`, and its dispatch table.
    pub external_fd: Option<ash::khr::external_memory_fd::Device>,
    pub has_dma_buf: bool,
    /// `VK_EXT_external_memory_host`, and its dispatch table.
    pub external_host: Option<ash::ext::external_memory_host::Device>,
}

impl Driver {
    /// Open device `index`, or the first one, and enable whatever it offers.
    pub fn open(index: usize) -> Result<Driver> {
        let entry =
            unsafe { ash::Entry::load() }.map_err(|e| DriverError::NoLoader(e.to_string()))?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"sv-gpu-server")
            // 1.1 is the floor the design needs: timeline semaphores and the
            // external-memory queries are 1.1-era, and asking for more than the
            // driver has makes `vkCreateInstance` fail outright.
            .api_version(vk::API_VERSION_1_1);
        let ci = vk::InstanceCreateInfo::default().application_info(&app);
        let instance =
            unsafe { entry.create_instance(&ci, None) }.map_err(vkerr("vkCreateInstance"))?;

        let opened = Driver::open_on(entry, instance, index);
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

    fn open_on(entry: ash::Entry, instance: ash::Instance, index: usize) -> Result<Driver> {
        let r = Driver::open_inner(&entry, &instance, index);
        match r {
            Ok((pd, device, queue, queue_family, external_fd, has_dma_buf, external_host)) => {
                let props = unsafe { instance.get_physical_device_properties(pd) };
                let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
                let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
                Ok(Driver {
                    entry,
                    instance,
                    pd,
                    device,
                    queue,
                    queue_family,
                    props,
                    mem_props,
                    name,
                    external_fd,
                    has_dma_buf,
                    external_host,
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
        let mut names: Vec<*const std::ffi::c_char> = Vec::new();
        if has_fd {
            names.push(c"VK_KHR_external_memory_fd".as_ptr());
        }
        if has_dma_buf {
            names.push(c"VK_EXT_external_memory_dma_buf".as_ptr());
        }
        if has_host {
            names.push(c"VK_EXT_external_memory_host".as_ptr());
        }
        let dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qi)
            .enabled_extension_names(&names);
        let device =
            unsafe { instance.create_device(pd, &dci, None) }.map_err(vkerr("vkCreateDevice"))?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        let external_fd =
            has_fd.then(|| ash::khr::external_memory_fd::Device::new(instance, &device));
        let external_host =
            has_host.then(|| ash::ext::external_memory_host::Device::new(instance, &device));
        Ok((pd, device, queue, queue_family, external_fd, has_dma_buf, external_host))
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

    /// The first memory type satisfying `want`, or `None`.
    pub fn memory_type(&self, mask: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|i| {
            mask & (1 << i) != 0
                && self.mem_props.memory_types[*i as usize].property_flags.contains(want)
        })
    }
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
