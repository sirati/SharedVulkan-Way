//! The app side's object tables, and the implementation of the safe half.
//!
//! # What is here and what is not
//!
//! This is the ICD as of design.md §13 **step 5** — "manifest, `VK_DRIVER_FILES`,
//! dispatchable-handle magic, interface negotiation, `vulkaninfo` producing a
//! sane report" — and no further. Step 4, the end-to-end round trip against a
//! GPU side on lavapipe, needs `sv-transport` and `sv-gpu-server`, neither of
//! which is on master. So:
//!
//! - **Object ids are minted here and nothing crosses.** That is exactly what
//!   design.md §6.4 specifies for the app side ("`vkCreateBuffer` encodes the id
//!   it chose and returns immediately — object creation costs no round trip"),
//!   so the ids are real and the encoding is real; what is missing is the ring
//!   that would carry them.
//! - **Arena memory is real.** `vkAllocateMemory` takes a sub-range of a real
//!   shared mapping and `vkMapMemory` returns a pointer into it. Writing through
//!   that pointer writes the memfd. What has not happened is the GPU side
//!   importing the same pages.
//! - **Everything that would need an answer from a GPU is answered locally**,
//!   from the measured table in [`crate::advertise`] or with a conservative
//!   constant, and every such place says so. Nothing here pretends to have run
//!   on a GPU.
//!
//! **That is no longer the state of this crate.** `sv-transport` and
//! `sv-gpu-server` exist, [`crate::transport::RingTransport`] implements the
//! seam, and when `SV_SOCKET` names a live GPU side every one of the ~239
//! forwarded commands crosses instead of being answered here. What is still
//! answered locally is exactly the set design.md §8, §9 and §10 name — the
//! memory table, the extension lists, the WSI family, and `vkMapMemory` — plus,
//! when there is *no* GPU side, everything, so that `vulkaninfo` against a
//! standalone driver still produces a report.
//!
//! The seam is [`sv_icd_entry::Transport`], and it lives there rather than here
//! because the generated forwarders have to name it and they cannot depend on
//! the crate that depends on them.

use crate::advertise;
use crate::arena::{Allocation, Backing, Mapping, Placeholder};
use std::collections::HashMap;
use std::sync::Mutex;
use sv_icd_entry::{IcdResult, LowerError};

/// **The ceiling**: the highest Vulkan version this build could ever advertise.
///
/// `VK_MAKE_API_VERSION(0, 1, 3, 0)` today, and *derived* rather than written
/// down: it is [`sv_proto::CORE_API_VERSION`], which the generator computed from
/// the wire profile's `VK_VERSION_*` features. A constant here could disagree
/// with the command table; this cannot.
///
/// It is a ceiling and not an answer. What an application is told is
/// [`negotiate`]'s output — `min(what the driver behind us reports, this)` — so
/// that a 1.1 device is advertised as 1.1 and a 1.4 device is advertised as 1.3
/// *and said so at startup*. See design.md §0.18, and [`crate::icd`]'s
/// `clamp_api_version` for the two GL versions that claiming more than this
/// cost the last time it was claimed.
///
/// It must also agree with the manifest, which is a file the loader reads before
/// any of this runs; `the_manifest_agrees_with_what_we_report` is that check and
/// `build.rs` is where the constraint is written out.
pub const PROTOCOL_API_VERSION: u32 = sv_proto::CORE_API_VERSION;

/// What [`negotiate`] decided, and why — so the reason can be printed once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiated {
    /// No GPU side, or one that did not answer `svDriverVersions`. The ceiling
    /// is the honest answer: it is exactly what the standalone driver
    /// implements.
    Unknown,
    /// The driver reports the same version the wire carries. Nothing to say
    /// beyond "negotiation ran".
    Exact,
    /// The driver is **older** than the ceiling, and we advertise the driver's
    /// number. This is the graceful-fallback case the whole exercise is for: a
    /// GPU that only does 1.1 still works, and the application is told 1.1 so
    /// that its own version check makes the right decision.
    Fallback { driver: u32 },
    /// The driver is **newer** than the ceiling, and we advertise the ceiling.
    ///
    /// This is a demotion and it must never be silent — a silent demotion is
    /// precisely the defect §0.18 exists to fix. The application would otherwise
    /// never attempt anything the extra version enables and nothing anywhere
    /// would say why.
    Clamped { driver: u32 },
}

/// Format a packed `VK_MAKE_API_VERSION` the way a person writes it.
pub fn version_string(v: u32) -> String {
    format!("{}.{}.{}", (v >> 22) & 0x7f, (v >> 12) & 0x3ff, v & 0xfff)
}

/// Decide what to advertise, given what the driver reports and what we carry.
///
/// `driver` is `0` for "not known", which is what a standalone driver and an
/// unanswered `svDriverVersions` both produce. Pure, and separate from the
/// connect path, because the interesting cases are the ones no GPU on this
/// machine can produce: a device *older* than the ceiling needs a test as much
/// as a newer one does, and there is no 1.1-only GPU here to get it from.
pub fn negotiate(driver: u32, ceiling: u32) -> (u32, Negotiated) {
    // Compare on major.minor and ignore the patch. A driver at 1.3.290 and a
    // ceiling of 1.3.0 are the same *contract* — the patch of a core version
    // says which header revision the driver was built against and promises no
    // entry point — so treating 1.3.290 as "newer than we can carry" would print
    // a demotion notice on every ordinary run and mean nothing by it.
    let nopatch = |v: u32| v & !0xfff;
    match driver {
        0 => (ceiling, Negotiated::Unknown),
        d if nopatch(d) == nopatch(ceiling) => {
            // Keep the *driver's* patch digits. They are the only part of the
            // number that distinguishes two drivers of one minor version, and
            // `vulkaninfo` prints them, so throwing them away is what makes the
            // forwarded report differ from the direct one by a visible digit.
            (d, Negotiated::Exact)
        }
        d if nopatch(d) < nopatch(ceiling) => (d, Negotiated::Fallback { driver: d }),
        d => (ceiling, Negotiated::Clamped { driver: d }),
    }
}

/// The reserved id the arena's `VkDeviceMemory` has on the GPU side.
///
/// Must equal `sv_gpu_server::ARENA_MEMORY_ID`. Declared here rather than taken
/// as a dependency because the app side must not link the GPU side: the two are
/// separate processes and, on `testhost`, separate operating systems. The
/// agreement is checked by `the_two_sides_agree_about_the_arena_id`.
pub const ARENA_MEMORY_ID: u64 = u64::MAX - 1;

/// The default arena size when nothing else says (design.md §4.2's table).
pub const DEFAULT_ARENA_LEN: u64 = 256 << 20;

#[derive(Debug)]
pub struct Instance {
    pub api_version: u32,
    pub physical_devices: Vec<u64>,
}

#[derive(Debug)]
pub struct Device {
    pub physical_device: u64,
    pub queues: Vec<u64>,
}

/// A `VkDeviceMemory` as the app side holds it.
#[derive(Debug, Clone, Copy)]
pub struct Memory {
    pub memory_type_index: u32,
    /// `None` for a heap-0 allocation, which is never arena-backed and never
    /// mappable (design.md §9.1: "impossible by construction rather than by
    /// check").
    pub allocation: Option<Allocation>,
    pub size: u64,
    pub mapped: bool,
}

/// Everything the app side knows.
pub struct State {
    /// design.md §6.4 allocates ids from a per-device counter. One counter for
    /// the process is a superset of that — every id is still unique on the wire
    /// — and it removes the question of which counter a cross-device object
    /// belongs to. Starts at 1 so that 0 stays `VK_NULL_HANDLE`.
    next_id: u64,
    pub instances: HashMap<u64, Instance>,
    /// Physical device id → the instance that enumerated it.
    pub physical_devices: HashMap<u64, u64>,
    pub devices: HashMap<u64, Device>,
    /// Queue id → its device.
    pub queues: HashMap<u64, u64>,
    pub memories: HashMap<u64, Memory>,
    /// Buffer id → its memory requirements, so `vkGetBufferMemoryRequirements`
    /// can answer without a GPU.
    pub buffers: HashMap<u64, u64>,
    /// `VkDescriptorUpdateTemplate` id → the entries it was created with.
    ///
    /// Kept because `vkUpdateDescriptorSetWithTemplate`'s `pData` has no
    /// describable shape without them — see
    /// [`sv_icd_entry::local::expand_descriptor_template`]. This is the only object
    /// whose *contents* the app side has to remember; everything else it only
    /// has to name.
    pub templates: HashMap<u64, Vec<sv_proto::VkDescriptorUpdateTemplateEntry>>,
    pub allocator: Box<dyn Backing>,
}

impl State {
    fn new(allocator: Box<dyn Backing>) -> State {
        State {
            next_id: 1,
            instances: HashMap::new(),
            physical_devices: HashMap::new(),
            devices: HashMap::new(),
            queues: HashMap::new(),
            memories: HashMap::new(),
            buffers: HashMap::new(),
            templates: HashMap::new(),
            allocator,
        }
    }

    pub fn mint(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// The ICD.
pub struct SharedVulkan {
    pub arena: Mapping,
    pub arena_len: u64,
    state: Mutex<State>,
    transport: Option<crate::transport::RingTransport>,
    /// What `vkEnumerateInstanceVersion` answers and what every instance this
    /// driver creates is stamped with.
    ///
    /// Settled once, at construction, by [`negotiate`] over what the GPU side
    /// reported. A field rather than a constant because it is a fact about the
    /// *pair* — this build's wire and that machine's driver — and the whole
    /// point of §0.18 is that neither half decides it alone.
    api_version: u32,
    /// The negotiated `VkPhysicalDeviceProperties::apiVersion`, kept apart from
    /// [`Self::api_version`] because Vulkan keeps them apart: a driver may
    /// support a higher instance version than a particular device does. With a
    /// GPU side this is a cross-check on the forwarded number rather than the
    /// source of it — the forwarded properties carry the device's own answer and
    /// `clamp_api_version` bounds it — and without one it is what the local
    /// table reports.
    device_api_version: u32,
    /// What we tell an application `vkEnumerateDeviceExtensionProperties`
    /// returns, and what `vkCreateDevice` will accept.
    ///
    /// **Asked of the GPU side once, at connect**, rather than being the
    /// constant `advertise::DEVICE_EXTENSIONS` used to be. design.md §9.2 says
    /// an extension in this list is a promise its entry points work, and the
    /// only side that can keep that promise is the one holding the driver: it
    /// answers the intersection of the profile with what its driver has
    /// (`sv_gpu_server::driver::forwardable_device_extensions`). With no GPU
    /// side the list stays empty, which is the honest answer for a driver that
    /// cannot forward anything.
    ///
    /// Leaked to `'static` because an ICD is a process-lifetime singleton — the
    /// loader `dlopen`s it once — and the alternative is a lifetime on the
    /// generated `Icd` trait for a `Vec` that is never freed anyway.
    device_extensions: Vec<(&'static str, u32)>,
    /// Live swapchains, by id (design.md §10, [`crate::wsi`]).
    ///
    /// **A second lock rather than a field of [`State`]**, and the lock order is
    /// swapchain-then-state, never the reverse. Every step of
    /// `vkQueuePresentKHR` is an ordinary forwarded Vulkan command, and every
    /// one of those takes `State` — so holding `State` across a present would
    /// deadlock on the first `vkQueueSubmit`. Keeping the two apart makes that
    /// impossible rather than merely avoided.
    swapchains: Mutex<HashMap<u64, crate::wsi::Swapchain>>,
    /// Live surfaces, by id.
    ///
    /// **The ICD owns these**, which design.md §8 says it does not: see
    /// `sv_codegen::emit::icd_entry::EXTRA_LOCAL`. A `VkSurfaceKHR` that reaches
    /// any entry point of this driver is a key of this map, and one that is not
    /// is a surface some other ICD created.
    surfaces: Mutex<HashMap<u64, sv_icd_entry::PresentTarget>>,
}

impl SharedVulkan {
    /// A standalone driver: its own memfd, no GPU side.
    ///
    /// Not a fallback to be removed. `vulkaninfo` against this configuration is
    /// what design.md §13 step 5 tests, the loader test in `tests/loader.rs`
    /// drives it, and a machine with no GPU side running has to enumerate rather
    /// than fail to load.
    pub fn new(arena_len: u64) -> Result<SharedVulkan, std::io::Error> {
        let arena = Mapping::create(arena_len)?;
        let allocator = Placeholder::new(arena_len, crate::arena::DEFAULT_SLAB_SIZE)
            .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
        Ok(SharedVulkan {
            arena,
            arena_len,
            state: Mutex::new(State::new(Box::new(allocator))),
            transport: None,
            // No driver behind us, so there is nothing to negotiate against and
            // the ceiling is the honest answer: a standalone driver implements
            // exactly the profile and no more.
            api_version: PROTOCOL_API_VERSION,
            device_api_version: PROTOCOL_API_VERSION,
            device_extensions: Vec::new(),
            swapchains: Mutex::new(HashMap::new()),
            surfaces: Mutex::new(HashMap::new()),
        })
    }

    /// A connected driver: the arena and the control region come from the peer.
    ///
    /// The allocator becomes the real `sv_arena::ArenaAllocator` rather than
    /// `Placeholder`, because from here the offsets it produces are read by
    /// *another process* — §5.2's segregated free lists and its slab-boundary
    /// rule stop being an implementation detail and start being the contract.
    ///
    /// `endpoint` is a Unix socket path on Linux and a **session name** on
    /// Windows, from which the pipe and the two section names are derived. One
    /// environment variable either way, because an ICD cannot be given
    /// arguments and adding a second variable for the same job is how a user
    /// ends up setting the wrong one.
    #[cfg(any(target_os = "linux", windows))]
    pub fn connected(endpoint: &str) -> Result<SharedVulkan, String> {
        let socket = endpoint;
        let attached = crate::transport::RingTransport::connect(socket)?;
        let arena = Mapping::attach(&attached.transport.arena, attached.arena_len);
        // `CanaryPolicy::None`: R26's canary detects a *guest page* moving
        // underneath a `VirtualLock`ed window, which is a Windows-in-a-VM
        // failure. Neither Linux provider can exhibit it — a memfd's pages do
        // not migrate under a `MAP_SHARED` mapping, and driver memory is the
        // driver's — so the canary here would cost a page per slab to detect
        // something that cannot happen.
        let allocator = sv_arena::alloc::ArenaAllocator::new(
            attached.arena_len,
            attached.slab_size,
            sv_arena::canary::CanaryPolicy::None,
        )
        .map_err(|e| format!("{e}"))?;
        // **Before anything else is asked of the ring.** The loader may call
        // `vkEnumerateInstanceVersion` before `vkCreateInstance` — it is a global
        // command — so the answer has to exist by the time this constructor
        // returns, and the only side that knows it is the one holding the
        // driver.
        let (driver_instance, driver_device) =
            attached.transport.driver_versions().unwrap_or((0, 0));
        // **The instance number is bounded by the device's**, and that is a
        // consequence of design.md §8 rather than of Vulkan: an ICD may in
        // general offer more instance-level version than any of its devices
        // supports, but this one has exactly one physical device, so an
        // application told "instance 1.3" and then handed a 1.1 device learns
        // nothing from the first number that the second does not contradict.
        // The GPU side's instance number is also its *loader's*, which on a
        // machine with a new loader and an old driver overstates the driver by
        // an entire minor version; taking the smaller of the two is what stops
        // that from becoming our answer. Zero means "not reported" and must not
        // win a `min`.
        let both = match (driver_instance, driver_device) {
            (0, d) => d,
            (i, 0) => i,
            (i, d) => i.min(d),
        };
        let (api_version, how) = negotiate(both, PROTOCOL_API_VERSION);
        let (device_api_version, _) = negotiate(driver_device, PROTOCOL_API_VERSION);
        announce(how, api_version, driver_device);

        let mut icd = SharedVulkan {
            arena,
            arena_len: attached.arena_len,
            state: Mutex::new(State::new(Box::new(allocator))),
            transport: Some(attached.transport),
            api_version,
            device_api_version,
            device_extensions: Vec::new(),
            swapchains: Mutex::new(HashMap::new()),
            surfaces: Mutex::new(HashMap::new()),
        };
        icd.device_extensions = icd.ask_device_extensions();
        Ok(icd)
    }

    /// Ask the GPU side which device extensions it will forward.
    ///
    /// Two round trips at startup — count, then fill — and they are the
    /// count-then-fill idiom the protocol was built for. Failure is not fatal:
    /// an empty list means an application that asks for an extension is told
    /// `VK_ERROR_EXTENSION_NOT_PRESENT`, which is a clean refusal rather than a
    /// driver that loads and then misbehaves.
    // Same cfg as its one caller, `connected`. It was linux-only while the
    // Windows transport did not exist; now that it does, a windows-gnu build
    // fails to compile without this — the caller is compiled and the callee
    // is not.
    #[cfg(any(target_os = "linux", windows))]
    fn ask_device_extensions(&self) -> Vec<(&'static str, u32)> {
        let Some(t) = self.wire() else { return Vec::new() };
        let ask = |capacity: usize| {
            sv_icd_entry::forward::enumerate_device_extension_properties(
                t,
                sv_proto::VkEnumerateDeviceExtensionPropertiesArgs {
                    // The GPU side answers for the one physical device it has
                    // and does not read this; zero is `VK_NULL_HANDLE` and is
                    // the honest thing to send before anything is enumerated.
                    physical_device: sv_proto::VkPhysicalDevice(0),
                    p_layer_name: Vec::new(),
                    p_property_count: Some(capacity as u32),
                    p_properties: vec![
                        sv_proto::VkExtensionProperties::default();
                        capacity
                    ],
                },
            )
        };
        let Ok(count) = ask(0) else { return Vec::new() };
        let n = count.p_property_count.unwrap_or(0) as usize;
        if n == 0 {
            return Vec::new();
        }
        let Ok(full) = ask(n) else { return Vec::new() };
        let mut out: Vec<(&'static str, u32)> = full
            .p_properties
            .iter()
            .filter_map(|e| {
                let end = e.extension_name.iter().position(|b| *b == 0).unwrap_or(0);
                let name = std::str::from_utf8(&e.extension_name[..end]).ok()?;
                if name.is_empty() {
                    return None;
                }
                Some((&*Box::leak(name.to_string().into_boxed_str()), e.spec_version))
            })
            .collect();
        // **`VK_KHR_swapchain` is added here rather than intersected**, and that
        // is a correction to design.md §0.4 item 8 rather than an exception to
        // it. §0.4's rule — the list is "the profile's extensions intersected
        // with what the driver actually has, and only the side holding the
        // driver knows the second half" — is right for every extension whose
        // commands are *forwarded*. This one's commands are not: the whole of
        // `VK_KHR_swapchain` is implemented in [`crate::wsi`], out of ordinary
        // Vulkan the GPU side never learns is a swapchain. §9.2 says an
        // extension in this list is a promise its entry points work, and the
        // side that keeps that promise here is this one.
        //
        // The GPU-side driver still has to *enable* it — an application
        // transitions a swapchain image to `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR`,
        // and that layout does not exist on a device that did not — which is
        // why it is in the profile the GPU side enables from
        // (`vk_registry::profile::zink`). If the driver over there lacks it
        // entirely, that is a driver we cannot present through, and it will say
        // so at the first barrier rather than here.
        // The third term of the intersection, and the one that was missing.
        //
        // The GPU side answers "the profile, intersected with what my driver
        // has". Both halves of that can be true of an extension whose entry
        // points this build never generated — the generator refuses a command
        // whose reply it cannot shape, and the refusal lands in
        // `sv_icd_entry::NOT_IMPLEMENTED` rather than in the profile. So the
        // answer arriving over the wire is "the device supports it", not "it
        // works", and design.md §9.2 asks for the second.
        //
        // Concretely: this is what stops `VK_KHR_buffer_device_address` being
        // advertised while `vkGetBufferDeviceAddress` returns a silent zero.
        // Nothing here decides *whether* to implement it — `sv-worktrees/cts2`
        // owns that — it decides not to promise it until something does.
        out.retain(|(n, _)| sv_icd_entry::INCOMPLETE_EXTENSIONS.binary_search(n).is_err());

        // **`VK_KHR_swapchain` is added here rather than intersected**, and that
        // is a correction to design.md §0.4 item 8 rather than an exception to
        // it. §0.4's rule — the list is "the profile's extensions intersected
        // with what the driver actually has, and only the side holding the
        // driver knows the second half" — is right for every extension whose
        // commands are *forwarded*. This one's commands are not: the whole of
        // `VK_KHR_swapchain` is implemented in [`crate::wsi`], out of ordinary
        // Vulkan the GPU side never learns is a swapchain. §9.2 says an
        // extension in this list is a promise its entry points work, and the
        // side that keeps that promise here is this one.
        //
        // The GPU-side driver still has to *enable* it — an application
        // transitions a swapchain image to `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR`,
        // and that layout does not exist on a device that did not — which is
        // why it is in the profile the GPU side enables from
        // (`vk_registry::profile::zink`). If the driver over there lacks it
        // entirely, that is a driver we cannot present through, and it will say
        // so at the first barrier rather than here.
        //
        // **It goes after the filter, not before, and that ordering is the whole
        // of a regression this section caused and caught.** `VK_KHR_swapchain`
        // is in `INCOMPLETE_EXTENSIONS` — `vkAcquireNextImage2KHR` and the three
        // device-group present queries are stubs, and have always been — so
        // adding it first and filtering second removed presentation from the
        // advertised set. `vulkaninfo` through the forwarder went from 218
        // device extensions to 217 and the missing one was the extension every
        // windowed application requires. The filter answers "did the *generator*
        // keep the promise"; this line answers "did `crate::wsi`", and those are
        // different questions about different code.
        if !out.iter().any(|(n, _)| *n == "VK_KHR_swapchain") {
            out.push(("VK_KHR_swapchain", 70));
        }
        out
    }

    /// What this build advertises to an application.
    pub fn device_extensions(&self) -> &[(&'static str, u32)] {
        &self.device_extensions
    }

    /// The swapchain table (design.md §10).
    pub fn swapchains(&self) -> std::sync::MutexGuard<'_, HashMap<u64, crate::wsi::Swapchain>> {
        self.swapchains.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The surface table.
    pub fn surfaces(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<u64, sv_icd_entry::PresentTarget>> {
        self.surfaces.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The transport, if this process is connected to a GPU side.
    pub fn wire(&self) -> Option<&dyn sv_icd_entry::Transport> {
        self.transport.as_ref().map(|t| t as &dyn sv_icd_entry::Transport)
    }

    /// The transport as itself rather than as the trait.
    ///
    /// One caller: `vkFreeCommandBuffers`, which returns a command blob's arena
    /// block. That is not on [`sv_icd_entry::Transport`] because it is not
    /// something a *generated* forwarder ever needs — only this crate knows the
    /// difference between a command buffer being reset and one being freed.
    pub fn ring_transport(&self) -> Option<&crate::transport::RingTransport> {
        self.transport.as_ref()
    }

    /// design.md §5.3's rewrite, app-side half: allocation coordinates become
    /// arena coordinates.
    ///
    /// Returns `(memory id, offset)` ready to cross. For an arena-backed
    /// allocation that is `(ARENA_MEMORY_ID, arena_offset + offset)`; for
    /// heap-0 memory it is the pair unchanged, because that memory is a real
    /// GPU-side allocation under the app's own id.
    pub fn rewrite_memory(&self, memory: u64, offset: u64) -> IcdResult<(u64, u64)> {
        let s = self.state();
        let m = s
            .memories
            .get(&memory)
            .ok_or(LowerError::Shape("no such VkDeviceMemory"))?;
        let Some(a) = m.allocation else {
            return Ok((memory, offset));
        };
        if offset > a.size {
            return Err(LowerError::Shape("offset is outside the allocation"));
        }
        let base = s
            .allocator
            .arena_offset(a)
            .map_err(|_| LowerError::Shape("allocation is outside the arena"))?;
        Ok((ARENA_MEMORY_ID, base + offset))
    }

    /// What `vkEnumerateInstanceVersion` answers: the negotiated version.
    pub fn advertised_api_version(&self) -> u32 {
        self.api_version
    }

    /// What a physical device this driver reports is allowed to claim.
    pub fn advertised_device_api_version(&self) -> u32 {
        self.device_api_version
    }

    /// A poisoned lock is not a reason to abort an application's Vulkan call:
    /// the state it protects is plain tables, and the panic that poisoned it has
    /// already been reported.
    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Say, on the loader's diagnostic channel, what was negotiated and why.
///
/// **On the line that always prints, not behind a debug flag.** The bug §0.18
/// fixes was not that the wrong version was reported — it was that the wrong
/// version was reported *without anything saying so*, which is why it survived
/// long enough to cost two GL versions through Zink. A demotion nobody can see
/// is the same defect wearing different clothes, so the one case that must never
/// be quiet is [`Negotiated::Clamped`].
fn announce(how: Negotiated, advertised: u32, driver_device: u32) {
    let adv = version_string(advertised);
    match how {
        Negotiated::Unknown => eprintln!(
            "sv-icd: the GPU side did not report a driver version; advertising this build's \
             ceiling, Vulkan {adv}"
        ),
        Negotiated::Exact => eprintln!(
            "sv-icd: Vulkan {adv}, which is both what the driver offers and what this wire \
             carries (device {})",
            version_string(driver_device)
        ),
        Negotiated::Fallback { driver } => eprintln!(
            "sv-icd: the driver offers Vulkan {}, below this build's ceiling {}; advertising \
             {adv} — what the GPU supports, forwarded",
            version_string(driver),
            version_string(PROTOCOL_API_VERSION),
        ),
        Negotiated::Clamped { driver } => eprintln!(
            "sv-icd: the driver offers Vulkan {}, but this wire carries only {} (profile {}); \
             advertising {adv}.\n\
             sv-icd: applications will not attempt anything above {adv} through this driver. \
             Raising the ceiling means adding the missing core version to the wire profile — \
             see design.md §0.18 for what that costs.",
            version_string(driver),
            version_string(PROTOCOL_API_VERSION),
            sv_proto::PROTOCOL_PROFILE,
        ),
    }
}

// `pub fn unimplemented<T>(what) -> IcdResult<T>` used to live here, unused by
// anything in the workspace and unused for long enough that the audit of §0.20
// found it by looking for the opposite thing. It is deleted rather than wired
// up: a helper whose name announces a convention, with no callers, reads as
// evidence that refusals are marked when they are not. The one place that does
// refuse spells it out — `LowerError::NotImplemented("vkCreateSwapchainKHR
// without a GPU side")` in `crate::wsi` — and a spelled-out refusal carrying its
// own reason is better than a shared helper that carries none.

/// The memory table, with this ICD's arena size in it.
pub fn memory_properties(icd: &SharedVulkan) -> sv_icd_entry::VkPhysicalDeviceMemoryProperties {
    advertise::memory_properties(icd.arena_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest must state the **ceiling**, and the ceiling is derived.
    ///
    /// This is the drift detector for the one coupling §0.18 could not remove:
    /// the manifest is a file on disk, written by `build.rs`, and the loader
    /// reads it before a line of this crate runs, so it cannot be negotiated.
    /// `build.rs` explains what the loader does with the number and why "state
    /// the ceiling" is the right answer; this checks that the file says it.
    ///
    /// The failure it prevents is silent in both directions. Below 1.1 the
    /// loader stops asking us for a runtime version at all and treats us as a
    /// 1.0 driver forever, rewriting every application's requested version on
    /// the way past (loader.c, `terminator_CreateInstance`). Above the ceiling
    /// the JSON claims something the wire cannot carry, and a person reading the
    /// two files gets the wrong answer about which one is authoritative.
    #[test]
    fn the_manifest_agrees_with_what_we_report() {
        assert_eq!(
            version_string(PROTOCOL_API_VERSION),
            env!("SV_ICD_MANIFEST_API_VERSION"),
            "the manifest states the ceiling; the ceiling comes from the wire profile \
             (sv_proto::CORE_API_VERSION), so a profile that gains a core version needs \
             build.rs's MANIFEST_API_VERSION moved with it"
        );
        let minor = (PROTOCOL_API_VERSION >> 12) & 0x3ff;
        assert!(minor >= 1, "a manifest below 1.1 makes the loader demote us silently");
    }

    /// The four outcomes, including the two no GPU on this machine can produce.
    #[test]
    fn we_advertise_what_the_driver_supports_up_to_what_we_carry() {
        let mk = |minor: u32, patch: u32| (1u32 << 22) | (minor << 12) | patch;
        let ceiling = mk(3, 0);

        // The testhost guest's Intel UHD, and this machine's RADV: newer than
        // the wire. Demoted to the ceiling, and *said so*.
        for driver in [mk(4, 323), mk(4, 348), mk(4, 0)] {
            assert_eq!(negotiate(driver, ceiling), (ceiling, Negotiated::Clamped { driver }));
        }

        // The case with no hardware here to produce it, and the one the
        // requirement is actually about: a GPU that only does 1.1 must still
        // work, and must be advertised as 1.1 rather than as our ceiling.
        for driver in [mk(1, 0), mk(1, 128), mk(2, 0), mk(0, 0) | (1 << 22)] {
            let (advertised, how) = negotiate(driver, ceiling);
            assert_eq!(advertised, driver, "a driver below the ceiling is forwarded, not raised");
            assert_eq!(how, Negotiated::Fallback { driver });
        }

        // Equal on major.minor. The driver's patch survives, because that is
        // what `vulkaninfo` prints and the acceptance test is that the forwarded
        // report and the direct one are the same text.
        assert_eq!(negotiate(mk(3, 290), ceiling), (mk(3, 290), Negotiated::Exact));
        assert_eq!(negotiate(ceiling, ceiling), (ceiling, Negotiated::Exact));

        // No answer: the ceiling, which is what a standalone driver implements.
        assert_eq!(negotiate(0, ceiling), (ceiling, Negotiated::Unknown));

        // Whatever happens, we never advertise above what the wire carries.
        for driver in [0, mk(1, 0), mk(3, 999), mk(4, 323), mk(9, 9), u32::MAX] {
            let (advertised, _) = negotiate(driver, ceiling);
            assert!(
                advertised & !0xfff <= ceiling,
                "negotiate({driver:#x}) advertised {advertised:#x}, above the ceiling"
            );
        }
    }

    /// A standalone driver reports the ceiling and nothing else.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn with_no_gpu_side_the_ceiling_is_what_is_advertised() {
        let icd = SharedVulkan::new(1 << 20).expect("a 1 MiB arena");
        assert_eq!(icd.advertised_api_version(), PROTOCOL_API_VERSION);
        assert_eq!(icd.advertised_device_api_version(), PROTOCOL_API_VERSION);
    }

    #[test]
    fn a_version_is_printed_the_way_a_person_writes_it() {
        assert_eq!(version_string((1 << 22) | (4 << 12) | 323), "1.4.323");
        assert_eq!(version_string((1 << 22) | (3 << 12)), "1.3.0");
    }
}
