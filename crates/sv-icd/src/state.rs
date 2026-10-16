//! The app side's object tables, and the implementation of the safe half.
//!
//! # What is here and what is not
//!
//! This is the ICD as of docs/design.md §13 **step 5** — "manifest, `VK_DRIVER_FILES`,
//! dispatchable-handle magic, interface negotiation, `vulkaninfo` producing a
//! sane report" — and no further. Step 4, the end-to-end round trip against a
//! GPU side on lavapipe, needs `sv-transport` and `sv-gpu-server`, neither of
//! which is on master. So:
//!
//! - **Object ids are minted here and nothing crosses.** That is exactly what
//!   docs/design.md §6.4 specifies for the app side ("`vkCreateBuffer` encodes the id
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
//! answered locally is exactly the set docs/design.md §8, §9 and §10 name — the
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
/// *and said so at startup*. See docs/design.md §0.18, and [`crate::icd`]'s
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

/// The default arena size when nothing else says (docs/design.md §4.2's table).
pub const DEFAULT_ARENA_LEN: u64 = 256 << 20;

#[derive(Debug)]
pub struct Instance {
    pub api_version: u32,
    pub physical_devices: Vec<u64>,
}

#[derive(Debug)]
pub struct Device {
    pub physical_device: u64,
    pub queues: Vec<QueueSlot>,
}

/// One `VkQueue` an application asked for at `vkCreateDevice`: which family,
/// which index in it, and the id minted for it.
///
/// Keyed by family since docs/video-decode.md §7, when the GPU side began
/// serving more than one family. Before that every queue was family 0's and a
/// flat list indexed by `queueIndex` was the whole truth; with a decode family
/// it would hand `vkGetDeviceQueue(2, 0)` whatever was created first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueSlot {
    pub family: u32,
    pub index: u32,
    pub id: u64,
}

impl Device {
    /// The queue at `(family, index)`, if the application asked for one.
    pub fn queue(&self, family: u32, index: u32) -> Option<u64> {
        self.queues.iter().find(|q| q.family == family && q.index == index).map(|q| q.id)
    }

    /// The queue the app side's own work goes on — the swapchain's copies in
    /// `crate::wsi` — which has to be family 0's, the one family that is
    /// always `GRAPHICS | COMPUTE | TRANSFER`. The first queue created, if the
    /// application created none on family 0, which is what this returned before
    /// there were families to choose between.
    pub fn universal_queue(&self) -> Option<u64> {
        self.queue(0, 0).or_else(|| self.queues.first().map(|q| q.id))
    }
}

/// A `VkDeviceMemory` as the app side holds it.
#[derive(Debug, Clone, Copy)]
pub struct Memory {
    pub memory_type_index: u32,
    /// `None` for a heap-0 allocation, which is never arena-backed and never
    /// mappable (docs/design.md §9.1: "impossible by construction rather than by
    /// check").
    pub allocation: Option<Allocation>,
    pub size: u64,
    pub mapped: bool,
}

/// A `VkDescriptorUpdateTemplate` as the app side has to remember it.
///
/// # Why the bind point is here and was not
///
/// `vkUpdateDescriptorSetWithTemplate` needs only the entries: the descriptor
/// set it writes into is a parameter of the call. The **push** form is not like
/// that — `vkCmdPushDescriptorSetWithTemplate` names no set, because there is
/// none; the descriptors go straight into the command buffer, and the pipeline
/// bind point they belong to was fixed when the *template* was created
/// (`VkDescriptorUpdateTemplateCreateInfo::pipelineBindPoint`, meaningful
/// exactly when `templateType` is `PUSH_DESCRIPTORS`).
///
/// So expanding a push template into `vkCmdPushDescriptorSet` needs a value the
/// call does not carry, and the only place it exists is the template object.
/// docs/design.md §0.22.
#[derive(Debug, Clone)]
pub struct Template {
    pub entries: Vec<sv_proto::VkDescriptorUpdateTemplateEntry>,
    /// `pipelineBindPoint`, as the create-info gave it.
    pub bind_point: sv_proto::VkPipelineBindPoint,
}

/// A `VkDescriptorSet`, as the app side has to remember it.
///
/// Two fields, and each is here for a reason the other cannot cover — see
/// [`State::set_layouts`] for what the record is *for*.
///
/// # Why the answer and not the layout's id
///
/// The obvious record is the `VkDescriptorSetLayout` id, and it is wrong:
/// **a descriptor set layout may be destroyed while descriptor sets allocated
/// from it are still in use**, which the specification says in as many words.
/// A set holding its layout's id would then look the answer up in a table the
/// destroy has already emptied, conclude "unknown", and put the poison back on
/// the wire — the original defect, restored by an entirely ordinary sequence.
///
/// So the *answer* is captured at allocate time and shared: one `Arc` per
/// layout, however many sets are allocated from it, and the layout's own entry
/// exists only long enough to be copied out of.
#[derive(Debug, Clone)]
pub struct DescriptorSet {
    /// The bindings whose sampler comes from the layout. Shared with the layout
    /// entry it was allocated from, and outlives it.
    pub immutable: std::sync::Arc<std::collections::HashSet<u32>>,
    /// The `VkDescriptorPool` it came out of. This is what makes the record
    /// droppable: `vkResetDescriptorPool` and `vkDestroyDescriptorPool` free
    /// every set of a pool without naming one of them.
    pub pool: u64,
}

/// Everything the app side knows.
pub struct State {
    /// docs/design.md §6.4 allocates ids from a per-device counter. One counter for
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
    /// `VkDescriptorUpdateTemplate` id → what the app side has to remember
    /// about it.
    ///
    /// Kept because `vkUpdateDescriptorSetWithTemplate`'s `pData` has no
    /// describable shape without them — see
    /// [`sv_icd_entry::local::expand_descriptor_template`]. This is the only object
    /// whose *contents* the app side has to remember; everything else it only
    /// has to name.
    pub templates: HashMap<u64, Template>,
    /// `VkDescriptorSetLayout` id → the bindings of it that take their sampler
    /// from the *layout*, not from the descriptor write.
    ///
    /// # Why the app side has to remember this
    ///
    /// `VkDescriptorImageInfo::sampler` is ignored — and the specification then
    /// permits it to hold **anything at all** — for every `descriptorType`
    /// except `SAMPLER` and `COMBINED_IMAGE_SAMPLER`, and for those two as well
    /// when the binding being written was declared with `pImmutableSamplers`.
    /// CTS writes `0xDEADBEEF` there on purpose. Forwarding that value makes it
    /// an object id on the wire, the GPU side cannot resolve it, and the ring
    /// goes fatal: `no VkSampler with id 3735928559`. docs/design.md §0.26.
    ///
    /// It is the same class as `sv_codegen::exceptions::SIBLING_SELECTED` — a
    /// member the specification says is meaningless — one level deeper, and it
    /// is here rather than in the codegen for a reason that decides the shape:
    /// the selector is not a sibling member of the same struct. It is a property
    /// of the *layout the destination set was allocated with*, two commands
    /// away, and no rule expressible over `vk.xml` can reach it.
    ///
    /// Only the binding numbers are kept. Which sampler the layout names is the
    /// GPU side's business — it holds the real `VkDescriptorSetLayout` — and the
    /// only question asked here is whether the write's own sampler means
    /// anything.
    ///
    /// Read by `vkAllocateDescriptorSets` and by nothing else: the answer moves
    /// into the *set's* record there, because a layout may be destroyed while
    /// its sets live on. See [`DescriptorSet`].
    pub set_layouts: HashMap<u64, std::sync::Arc<std::collections::HashSet<u32>>>,
    /// `VkDescriptorSet` id → whether a write to it carries a sampler that means
    /// anything, and the pool that will take the record away. See
    /// [`State::set_layouts`] and [`DescriptorSet`].
    pub descriptor_sets: HashMap<u64, DescriptorSet>,
    /// `VkCommandBuffer` id → the `VkCommandPool` it was allocated from.
    ///
    /// Kept for one reason: `vkDestroyCommandPool` frees every command buffer
    /// of the pool without naming one of them, and each of those holds a block
    /// of the blob reserve. Without this the blocks of a destroyed pool were
    /// never given back — Zink destroys a pool with every context, and Firefox
    /// makes a WebGL context per page (docs/design.md §0.28).
    pub command_buffers: HashMap<u64, u64>,
    pub allocator: Box<dyn Backing>,
    /// Most arena bytes ever in use at once, and the value `SV_DEBUG` last
    /// reported ([`State::note_arena_use`]).
    pub arena_high_water: u64,
    pub arena_reported: u64,
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
            set_layouts: HashMap::new(),
            descriptor_sets: HashMap::new(),
            command_buffers: HashMap::new(),
            allocator,
            arena_high_water: 0,
            arena_reported: 0,
        }
    }

    pub fn mint(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// What the arena holds, as one clause for a diagnostic line: bytes in
    /// use of the application's share, in how many `VkDeviceMemory`, and the
    /// high-water mark.
    pub fn arena_use(&self) -> String {
        if let Some(d) = self.allocator.describe() {
            return d;
        }
        let len = self.allocator.arena_len();
        let used = len - self.allocator.free_bytes().min(len);
        let live = self.memories.values().filter(|m| m.allocation.is_some()).count();
        format!(
            "arena {} KiB of {} KiB in use in {live} allocation(s), largest free run {} KiB, \
             high water {} KiB",
            used >> 10,
            len >> 10,
            self.allocator.largest_free_run() >> 10,
            self.arena_high_water.max(used) >> 10
        )
    }

    /// Track the arena's high-water mark after an allocation or a free, and
    /// under `SV_DEBUG` print the use, with the seconds since the first arena
    /// allocation, whenever it has moved by 2 MiB either way since the last
    /// line. That series is what tells a working set that fits from one that
    /// grows until it does not.
    pub fn note_arena_use(&mut self) {
        use std::sync::OnceLock;
        static T0: OnceLock<std::time::Instant> = OnceLock::new();
        let t0 = *T0.get_or_init(std::time::Instant::now);
        let len = self.allocator.arena_len();
        let used = len - self.allocator.free_bytes().min(len);
        self.arena_high_water = self.arena_high_water.max(used);
        if used.abs_diff(self.arena_reported) >= 2 << 20 {
            self.arena_reported = used;
            if sv_icd_entry::debug_enabled() {
                eprintln!("sv-icd: t={:.1}s {}", t0.elapsed().as_secs_f64(), self.arena_use());
            }
        }
    }
}

/// The first of each of these failures is said unconditionally, once.
///
/// Each one ends, two layers up, in a `GL_OUT_OF_MEMORY` or a lost device with
/// nothing in the log that names this ICD (waycompose-design.md §95): Zink's
/// "vkCreateBuffer failed" carries no result, and the GPU side never learns
/// that an arena allocation was refused because it never sees one. Once, not
/// per occurrence, so that a failure in a loop cannot become the output; and
/// a flag per kind rather than `report_always`'s per-message set, because each
/// message carries numbers and would otherwise never repeat.
pub static ARENA_EXHAUSTED_SAID: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub static SLAB_IMPORT_SAID: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub static CREATE_BUFFER_FAILED_SAID: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// The leased arena's one line at attach (waycompose-design.md §97).
pub static POOL_SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Print `msg` the first time `flag` is seen clear.
pub fn say_once(flag: &std::sync::atomic::AtomicBool, msg: &dyn std::fmt::Display) {
    if !flag.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!("sv-icd: {msg}");
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
    /// constant `advertise::DEVICE_EXTENSIONS` used to be. docs/design.md §9.2 says
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
    /// The OR of the queue families the GPU side serves, asked once at connect.
    ///
    /// What `crate::advertise::unservable_queue_requirement` reads. It was the
    /// constant of the standalone family table, which was true while the GPU
    /// side served exactly that one family; since docs/video-decode.md §7 it may
    /// also serve a decode family, and a gate reading a constant would go on
    /// refusing the decode extensions the GPU side can now serve — the same
    /// "right about the condition, wrong about the set it reads" shape §0.22
    /// records, from the other direction.
    queue_flags: u32,
    /// Live swapchains, by id (docs/design.md §10, [`crate::wsi`]).
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
    /// **The ICD owns these**, which docs/design.md §8 says it does not: see
    /// `sv_codegen::emit::icd_entry::EXTRA_LOCAL`. A `VkSurfaceKHR` that reaches
    /// any entry point of this driver is a key of this map, and one that is not
    /// is a surface some other ICD created.
    surfaces: Mutex<HashMap<u64, sv_icd_entry::PresentTarget>>,
}

impl SharedVulkan {
    /// A standalone driver: its own memfd, no GPU side.
    ///
    /// Not a fallback to be removed. `vulkaninfo` against this configuration is
    /// what docs/design.md §13 step 5 tests, the loader test in `tests/loader.rs`
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
            queue_flags: crate::advertise::advertised_queue_flags(),
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
        let attached = connect_waiting_for_slot(socket)?;
        let arena = Mapping::attach(&attached.transport.arena, attached.arena_len);
        // `CanaryPolicy::None`: R26's canary detects a *guest page* moving
        // underneath a `VirtualLock`ed window, which is a Windows-in-a-VM
        // failure. Neither Linux provider can exhibit it — a memfd's pages do
        // not migrate under a `MAP_SHARED` mapping, and driver memory is the
        // driver's — so the canary here would cost a page per slab to detect
        // something that cannot happen.
        // A leased arena (waycompose-design.md §97) has no window to carve:
        // the allocator starts with nothing and leases as it needs.
        let allocator: Box<dyn Backing> = match attached.transport.pool.as_ref() {
            Some(p) => Box::new(crate::arena::Leased::new(p.granule)),
            None => Box::new(
                sv_arena::alloc::ArenaAllocator::new(
                    attached.arena_len,
                    attached.slab_size,
                    sv_arena::canary::CanaryPolicy::None,
                )
                .map_err(|e| format!("{e}"))?,
            ),
        };
        // **Before anything else is asked of the ring.** The loader may call
        // `vkEnumerateInstanceVersion` before `vkCreateInstance` — it is a global
        // command — so the answer has to exist by the time this constructor
        // returns, and the only side that knows it is the one holding the
        // driver.
        let (driver_instance, driver_device) =
            attached.transport.driver_versions().unwrap_or((0, 0));
        // **The instance number is bounded by the device's**, and that is a
        // consequence of docs/design.md §8 rather than of Vulkan: an ICD may in
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
            state: Mutex::new(State::new(allocator)),
            transport: Some(attached.transport),
            api_version,
            device_api_version,
            device_extensions: Vec::new(),
            queue_flags: crate::advertise::advertised_queue_flags(),
            swapchains: Mutex::new(HashMap::new()),
            surfaces: Mutex::new(HashMap::new()),
        };
        icd.queue_flags = icd.ask_queue_flags();
        icd.device_extensions = icd.ask_device_extensions();
        Ok(icd)
    }

    /// Ask the GPU side which queue capabilities its families serve, as one mask.
    ///
    /// Count-then-fill, two round trips at startup like the extension query
    /// beside it. A failure falls back to the standalone table's mask, which is
    /// the conservative direction: it can only *withhold* the decode extensions,
    /// never claim them.
    #[cfg(any(target_os = "linux", windows))]
    fn ask_queue_flags(&self) -> u32 {
        let fallback = crate::advertise::advertised_queue_flags();
        let Some(t) = self.wire() else { return fallback };
        let ask = |capacity: usize| {
            sv_icd_entry::forward::get_physical_device_queue_family_properties(
                t,
                sv_proto::VkGetPhysicalDeviceQueueFamilyPropertiesArgs {
                    physical_device: sv_proto::VkPhysicalDevice(0),
                    p_queue_family_property_count: Some(capacity as u32),
                    p_queue_family_properties: vec![
                        sv_proto::VkQueueFamilyProperties::default();
                        capacity
                    ],
                },
            )
        };
        let Ok(count) = ask(0) else { return fallback };
        let n = count.p_queue_family_property_count.unwrap_or(0) as usize;
        if n == 0 {
            return fallback;
        }
        let Ok(full) = ask(n) else { return fallback };
        let flags = full.p_queue_family_properties.iter().fold(0u32, |a, f| a | f.queue_flags.0);
        if flags == 0 { fallback } else { flags }
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
        // is a correction to docs/design.md §0.4 item 8 rather than an exception to
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
        // works", and docs/design.md §9.2 asks for the second.
        //
        // Concretely: this is what stops `VK_KHR_buffer_device_address` being
        // advertised while `vkGetBufferDeviceAddress` returns a silent zero.
        // Nothing here decides *whether* to implement it — `sv-worktrees/cts2`
        // owns that — it decides not to promise it until something does.
        out.retain(|(n, _)| sv_icd_entry::INCOMPLETE_EXTENSIONS.binary_search(n).is_err());

        // The fourth term, and the one that has nothing to do with entry points.
        //
        // The filter above asks whether the *generator* kept §9.2's promise. It
        // cannot ask whether the *device* can: an extension whose commands must
        // be recorded on a `VIDEO_DECODE` queue is unservable here no matter how
        // well its marshalling was generated, unless the GPU side serves a
        // family with that bit. `self.queue_flags` is the GPU side's own answer
        // (docs/video-decode.md §7): with a decode family it lets the decode
        // extensions through and still stops encode, whose family is not served.
        //
        // This went live the moment `full` became the default profile: sixteen
        // video extensions appeared in `vulkaninfo` through the forwarder, and
        // Firefox 153 selects a device on two of them. Recorded as a future risk
        // in §0.8 item 7 while the profile was `zink`, with nothing connecting
        // that note to the profile change — see §0.22.
        out.retain(|(n, _)| {
            match crate::advertise::unservable_queue_requirement(n, self.queue_flags) {
                None => true,
                Some(missing) => {
                    // Always, not under `SV_DEBUG`: an extension silently
                    // missing from the advertised set is the kind of thing that
                    // gets diagnosed as "the GPU does not support it".
                    sv_icd_entry::report_always(&format_args!(
                        "{n} needs a queue family with flags {missing:#x} and this \
                         driver advertises {:#x}; not advertising it",
                        self.queue_flags
                    ));
                    false
                }
            }
        });

        // **`VK_KHR_swapchain` is added here rather than intersected**, and that
        // is a correction to docs/design.md §0.4 item 8 rather than an exception to
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

        // Close the set under `depends=`.
        //
        // The queue check withdrew the three video extensions that *have*
        // commands. Ten codec and maintenance ones — `VK_KHR_video_decode_h264`,
        // `…_av1`, `VK_KHR_video_maintenance1` — add only structs and enums, so
        // they constrain no queue and nothing above could see them. Each depends
        // on one of the three that just went, and §9.2's promise is about the
        // whole extension: "H.264 decode, but not the decode queue you would
        // decode on" is not something an implementation may say.
        //
        // A fixpoint rather than one pass, for the reason
        // `Profile::include_portable_extensions` needs one: dependency chains
        // are deeper than one link, and a single pass keeps whatever depended on
        // what it just removed.
        //
        // **`have` is three sets, not one, and getting that wrong cost
        // `VK_KHR_swapchain` for the second time in this file.** A device
        // extension's `depends=` routinely names an *instance* extension —
        // `VK_KHR_swapchain` depends on `VK_KHR_surface`,
        // `VK_KHR_external_fence` on `VK_KHR_external_fence_capabilities`,
        // `VK_KHR_shader_untyped_pointers` on
        // `VK_KHR_get_physical_device_properties2` — and none of those is in
        // `out`, which is the *device* list. A closure that consulted only `out`
        // withdrew swapchain, present_id, present_wait, incremental_present,
        // hdr_metadata and eleven more, and `vulkaninfo` through the forwarder
        // went from 218 device extensions to 192.
        //
        // It is the same mistake as the ordering bug §0.19 item 6 records, in a
        // different disguise: a check that is right about the extension it was
        // written for and wrong about the set it reads. The diff caught it
        // again, which is the argument for the diff being the gate.
        //
        // Core versions satisfy a `depends=` on themselves — an extension
        // promoted into 1.1 may say `VK_VERSION_1_1,VK_KHR_…`, and this is a 1.4
        // device.
        loop {
            let have = |n: &str| {
                if let Some(minor) =
                    n.strip_prefix("VK_VERSION_1_").and_then(|m| m.parse::<u32>().ok())
                {
                    return minor <= 4;
                }
                out.iter().any(|(o, _)| *o == n)
                    || crate::advertise::INSTANCE_EXTENSIONS.iter().any(|(o, _)| *o == n)
            };
            let drop: Vec<&str> = out
                .iter()
                .map(|(n, _)| *n)
                .filter(|n| !sv_icd_entry::depends_satisfied(n, &have))
                .collect();
            if drop.is_empty() {
                break;
            }
            for d in &drop {
                sv_icd_entry::report_always(&format_args!(
                    "{d} depends on something this driver does not advertise; \
                     not advertising it either"
                ));
            }
            out.retain(|(n, _)| !drop.contains(n));
        }
        out
    }

    /// What this build advertises to an application.
    pub fn device_extensions(&self) -> &[(&'static str, u32)] {
        &self.device_extensions
    }

    /// The swapchain table (docs/design.md §10).
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

    /// The arena heap's size as an application should see it: the arena's
    /// length, or on a leased arena (waycompose-design.md §97) the most this
    /// session may lease — the pool's size or the session's cap, whichever is
    /// less. Not `arena_len`, which on a leased arena is the address space
    /// reserved for every region the pool could have: reported as a heap it
    /// took the device heap's whole size away, and Zink refused every
    /// device-local allocation ("can't allocate … from heap that's only 0
    /// bytes").
    pub fn heap_len(&self) -> u64 {
        match self.transport.as_ref().and_then(|t| t.pool.as_ref()) {
            Some(p) => p.session_cap.min(p.capacity),
            None => self.arena_len,
        }
    }

    /// docs/design.md §5.3's rewrite, app-side half: allocation coordinates become
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
             see docs/design.md §0.18 for what that costs.",
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
    advertise::memory_properties(icd.heap_len())
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

    /// docs/video-decode.md §7.13.5: the forwarder cannot import an
    /// application's own memory, so `VK_EXT_external_memory_host` is not
    /// promised. While it was, ffmpeg's `av_hwframe_transfer_data` (Firefox's
    /// readback) imported its `malloc`ed frame, Intel's driver accepted the NULL
    /// the GPU side passed on, and every hardware-decoded picture on the guest
    /// read back as zeros. The withdrawal is derived (the pointer's command is
    /// not in `ICD_EXPANDS_ARENA_POINTER`); this pins it.
    #[test]
    fn the_host_pointer_extension_is_not_promised() {
        assert!(
            sv_icd_entry::INCOMPLETE_EXTENSIONS.binary_search(&"VK_EXT_external_memory_host").is_ok(),
            "VK_EXT_external_memory_host must not be advertised"
        );
        assert!(
            sv_icd_entry::LOWERING_INCOMPLETE
                .contains(&("VK_EXT_external_memory_host", "vkGetMemoryHostPointerPropertiesEXT")),
            "and for the wild host pointer, not for some other reason"
        );
    }
}

/// How long a process waits for a GPU session slot before it gives up.
///
/// A slot comes back when the session in it ends, and the GPU side ends one
/// either when its application says so at exit or, for one that did not, when
/// its heartbeat has been silent for `APP_GONE_AFTER` (5 s) and its leftovers
/// are destroyed. Ten seconds covers the second case with room.
#[cfg(any(target_os = "linux", windows))]
const SLOT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// When this process stops waiting for a slot, as milliseconds of the wall
/// clock: the first call starts the budget and every later one shares it.
///
/// **Per process, not per load.** The loader loads an ICD several times in one
/// process (once for the pre-instance queries, again for the instance), and a
/// refused load leaves nothing loaded, so a static here starts fresh each
/// time: with every slot really taken, three loads would wait thirty seconds.
/// The budget is kept in a file named by the pid in the temporary directory,
/// which outlives an unload; a file left by an earlier process of the same pid
/// holds a deadline in the past, which only means that process does not wait.
#[cfg(any(target_os = "linux", windows))]
fn slot_wait_deadline_ms() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("sv-icd-slot-wait-{}", std::process::id()));
    if let Some(d) = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse().ok()) {
        return d;
    }
    let d = now + SLOT_WAIT.as_millis() as u64;
    let _ = std::fs::write(&path, d.to_string());
    d
}

/// [`crate::transport::RingTransport::connect`], retried while the guest says
/// every GPU session slot is taken (waycompose-design.md §95.10).
///
/// Refusing at once is right for a GPU side that is absent and wrong for one
/// that is busy for a few seconds more: Firefox's browser process asks for its
/// slot moments after `glxtest` has exited from one, and with two slots it was
/// refused and fell back to llvmpipe for the life of the process. The refusal
/// reaches this side as text through `sv-hostd`, so it is matched by the
/// guest's own words, `GPU session slots are taken`; anything else fails as
/// before.
#[cfg(any(target_os = "linux", windows))]
fn connect_waiting_for_slot(socket: &str) -> Result<crate::transport::Attached, String> {
    let started = std::time::Instant::now();
    let mut deadline = None;
    let mut said = false;
    loop {
        match crate::transport::RingTransport::connect(socket) {
            Err(e) if e.contains("GPU session slots are taken") => {
                let d = *deadline.get_or_insert_with(slot_wait_deadline_ms);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|t| t.as_millis() as u64)
                    .unwrap_or(u64::MAX);
                if now >= d {
                    return Err(e);
                }
                if !said {
                    said = true;
                    eprintln!(
                        "sv-icd: {e}; waiting up to {:.1} s for one to come back",
                        (d - now) as f64 / 1000.0
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Ok(a) => {
                if deadline.is_some() {
                    eprintln!("sv-icd: got a GPU session slot after {:.1?}", started.elapsed());
                }
                return Ok(a);
            }
            Err(e) => return Err(e),
        }
    }
}
