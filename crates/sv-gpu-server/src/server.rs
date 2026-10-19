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
//!   never reaches a driver (docs/design.md §5.2, §5.3);
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
/// Since docs/design.md §0.11 this process serves **several independent sessions**,
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
    /// the wire format is generated from (docs/design.md §7.2, pass 12).
    pub fns: Arc<DriverFns>,
    /// External synchronisation for the one `VkQueue` this device has.
    ///
    /// Vulkan requires the application to synchronise access to a `VkQueue`
    /// externally: `vkQueueSubmit`, `vkQueueWaitIdle` and `vkQueueBindSparse`
    /// are all "externally synchronized" on the queue parameter. The driver
    /// opens exactly one queue (`driver.rs`), so **every session shares it**.
    ///
    /// **The generated dispatch takes it**, as of the CTS stage. Pass 12 asks
    /// `vk_registry`'s `Command::externally_synchronizes_a_queue` — which reads
    /// both spellings the registry uses, `externsync=` on a `VkQueue` parameter
    /// and the `<implicitexternsyncparams>` prose that catches `vkDeviceWaitIdle`
    /// — and wraps exactly the driver call in `GpuHost::lock_queue`.
    ///
    /// Until then it was here for hand-written callers only, and the honest note
    /// in its place said so: the objects table is locked per lookup and released
    /// before the driver call, so two ring threads of *one* session could reach
    /// `vkQueueSubmit` concurrently — the race predates multi-session, which
    /// widened it from "two rings of one client" to "two clients". It is closed
    /// in the generator rather than here because that is where every call site
    /// is.
    pub queue_lock: Arc<Mutex<()>>,
    /// The device-wide command-pool lock (docs/design.md §0.23).
    ///
    /// Separate from `queue_lock` because they protect different driver
    /// objects and a submit must not wait on a recording: holding one mutex for
    /// both would serialise the two halves of every frame against each other.
    pub pool_lock: Arc<Mutex<()>>,
    /// Video decode structures at session-lifetime addresses
    /// (`crate::video_stable`, docs/video-decode.md §7.11.5).
    pub video_stable: Mutex<crate::video_stable::Stores>,
    /// Command buffers whose recording failed, and what broke each, for every
    /// ring of the session ([`Poisoned`]).
    pub poisoned: Poisoned,
}

/// Command buffers whose recording failed, and what broke each (docs/design.md
/// §0.24).
///
/// Keyed on the **driver's** handle rather than the wire object id, because
/// the generated dispatch maps handles before it looks an entry point up, so
/// that is what both the poisoning site and `vkQueueSubmit` are holding.
///
/// **Per session, not per ring** (docs/video-decode.md §7.12). A command
/// buffer is recorded on the device ring and submitted on the queue ring, so a
/// mark kept by the ring that recorded it was never seen by the submit: the
/// slot guard's first local run refused a decode, `vkEndCommandBuffer` failed
/// as it should, and the submit still reached RADV with the buffer unended
/// (`VUID-vkQueueSubmit2-commandBuffer-03874`).
///
/// A map and not a set, so the refusal can name the command that broke
/// recording rather than only the fact that something did. Empty in every run
/// where the driver has every entry point the profile carries and no video
/// command is refused, which is the common case: the submit path then costs
/// one atomic load.
#[derive(Default)]
pub struct Poisoned {
    marks: Mutex<std::collections::HashMap<u64, &'static str>>,
    count: std::sync::atomic::AtomicUsize,
}

impl Poisoned {
    fn marks(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<u64, &'static str>> {
        self.marks.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn insert(&self, command_buffer: u64, why: &'static str) {
        let mut m = self.marks();
        m.insert(command_buffer, why);
        self.count.store(m.len(), std::sync::atomic::Ordering::SeqCst);
    }

    pub fn remove(&self, command_buffer: u64) {
        if self.count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            return;
        }
        let mut m = self.marks();
        m.remove(&command_buffer);
        self.count.store(m.len(), std::sync::atomic::Ordering::SeqCst);
    }

    pub fn get(&self, command_buffer: u64) -> Option<&'static str> {
        if self.count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            return None;
        }
        self.marks().get(&command_buffer).copied()
    }
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
    /// Set by the handle-mapping pass when the command currently being
    /// dispatched named a poisoned command buffer; taken by the generated
    /// dispatch just after. Per-dispatch, which is why it is taken rather than
    /// read — a stale one would be reported against the next command.
    dispatch_poison: Option<&'static str>,
    /// Raise pools of the last `SV_VIDEO_RETAIN` video commands this ring
    /// replayed (`exceptions::GPU_RETAIN_RAISE`, docs/video-decode.md §7.11).
    /// Empty unless the variable is set.
    retained: std::collections::VecDeque<sv_gpu_entry::Raise>,
}

/// `SV_VIDEO_RETAIN`: how many video commands' raise pools a ring keeps; 0,
/// the default, keeps none.
/// The generated commands `crate::video_stable` is told about after each
/// driver call, whether or not `SV_VIDEO_LOG` is on: the sessions' limits for
/// the slot guard, and which session each parameters call's raise pool
/// belongs to (docs/video-decode.md §7.12).
const STABLE_OBSERVED: &[&str] = &[
    "vkCreateVideoSessionKHR",
    "vkDestroyVideoSessionKHR",
    "vkCreateVideoSessionParametersKHR",
    "vkUpdateVideoSessionParametersKHR",
    "vkDestroyVideoSessionParametersKHR",
];

/// One of [`STABLE_OBSERVED`] returned `result`.
///
/// # Safety
/// `args` are the call's C arguments as the driver received them
/// (`GpuHost::observe`'s contract): for each command, the device, then its
/// own parameters in order, the allocator as 0.
unsafe fn stable_observe(shared: &Shared, command: &'static str, args: &[u64], result: i32) {
    let a = |i: usize| args.get(i).copied().unwrap_or(0);
    let mut st = shared.video_stable.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY (each arm): the pointers are the call's own, valid for its
    // duration, and the output is written once the call returned success.
    match command {
        "vkCreateVideoSessionKHR" if result == 0 => unsafe {
            let ci = a(1) as *const sv_gpu_entry::VkVideoSessionCreateInfoKHR;
            let out = a(3) as *const u64;
            if let (Some(ci), false) = (ci.as_ref(), out.is_null()) {
                st.session_created(*out, ci.max_dpb_slots, ci.max_active_reference_pictures);
            }
        },
        "vkDestroyVideoSessionKHR" => st.session_destroyed(a(1)),
        "vkCreateVideoSessionParametersKHR" => unsafe {
            let ci = a(1) as *const sv_gpu_entry::VkVideoSessionParametersCreateInfoKHR;
            let out = a(3) as *const u64;
            let params = if result == 0 && !out.is_null() { *out } else { 0 };
            if let Some(ci) = ci.as_ref() {
                st.params_created(params, ci.video_session);
            }
        },
        "vkUpdateVideoSessionParametersKHR" => st.params_updated(a(1)),
        "vkDestroyVideoSessionParametersKHR" => st.params_destroyed(a(1)),
        _ => {}
    }
}

fn video_retain_limit() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("SV_VIDEO_RETAIN").ok().and_then(|v| v.parse().ok()).unwrap_or(0))
}

impl GpuServer {
    /// The slot guard of `crate::video_stable` refused a video command: it is
    /// not recorded, and the command buffer is poisoned, so its submit fails
    /// and the application falls back (docs/video-decode.md §7.12). Said on
    /// stderr the first time per process, with the numbers.
    fn refuse_video(&mut self, cb: u64, command: &'static str, r: crate::video_stable::Refusal) {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            eprintln!(
                "sv-gpu-server: {command} refused, not handed to the driver: {}; its command buffer will \
                 not submit (docs/video-decode.md §7.12)",
                r.detail
            )
        });
        crate::video_log::note(&format!("{command} cb{cb:x} REFUSED: {}", r.detail));
        self.poison_command_buffer(cb, r.why);
    }

    pub fn new(shared: std::sync::Arc<Shared>) -> GpuServer {
        GpuServer {
            shared,
            last_error: None,
            dispatch_poison: None,
            retained: Default::default(),
        }
    }

    fn d(&self) -> &ash::Device {
        &self.shared.driver.device
    }

    /// `vkDestroyImageView` on the driver, observed.
    fn destroy_view_on_driver(&mut self, device: u64, view: u64) {
        let Some(f) = self.fns().vk_destroy_image_view else {
            self.note_missing("vkDestroyImageView");
            return;
        };
        let words = [device, view, 0];
        let obs = crate::video_log::enabled();
        if obs {
            self.observe("vkDestroyImageView", false, &words, 0);
        }
        // SAFETY: both handles are the driver's, from the table or the cache.
        unsafe { f(device as usize as _, view, core::ptr::null()) };
        if obs {
            self.observe("vkDestroyImageView", true, &words, 0);
        }
    }

    /// Record a table failure and turn it into a wire error.
    ///
    /// # It used to say `UnknownCommand { opcode: 0 }`, and that was a lie
    ///
    /// Every table failure — a handle that does not resolve, a wrong type, a
    /// memory range outside the arena — came out as *"unknown command opcode
    /// 0"*, with the truth demoted to a parenthetical:
    ///
    /// ```text
    /// in a command blob: unknown command opcode 0 (no VkPipelineLayout with id 44 …)
    /// ```
    ///
    /// The opcode was fine and nothing was unknown. A reader who takes the
    /// headline at face value goes looking for a decoder that has lost
    /// alignment — a plausible bug, in a different subsystem, and §0.23's
    /// investigation spent time there before noticing the headline was
    /// fabricated.
    ///
    /// This is `TableError::Unknown`'s `RingBarrier` guess one layer out, and
    /// the same lesson: **an error must not assert something it does not
    /// know.** `WireError::Table` says a lookup failed and nothing more; the
    /// specifics are in `last_error`, which the ring prints beside it.
    fn note(&mut self, e: TableError) -> WireError {
        self.last_error = Some(e.to_string());
        WireError::Table
    }

    /// The families we advertise, by the index an application sees them at,
    /// with the real properties of each (`crate::driver::ServedFamily`):
    /// `queueCount` is 1 because that is how many queues were opened, and the
    /// capability bits this side does not serve are masked.
    fn served_queue_families(&self) -> Vec<p::VkQueueFamilyProperties> {
        self.shared.driver.served.iter().map(|f| crate::convert::queue_family(&f.props)).collect()
    }

    /// The queue `vkGetDeviceQueue(family, …)` names. A family we did not
    /// advertise gets the first queue rather than nothing — the command returns
    /// `void`, and `sv_icd::icd::get_device_queue` has already said so.
    fn queue_for_family(&self, family: u32) -> vk::Queue {
        self.shared
            .driver
            .served
            .get(family as usize)
            .map(|f| f.queue)
            .unwrap_or(self.shared.driver.queue)
    }
}

/// The two video links of a `VkQueueFamilyProperties2` reply, for one family.
///
/// Always both, whatever the request carried, and that is not an over-answer:
/// the app side's lowering sends each element of an *output array* as a
/// default with an empty chain, so the request cannot say which links the
/// application has; and `store_pnext` on the way back walks the application's
/// own chain and writes only the links whose `sType` it finds there. A link
/// the application did not chain is therefore dropped harmlessly, and one it
/// did chain is never missed. Every other link (`…GlobalPriority…`,
/// `…Checkpoint…`) stays unanswered, as it was before this stage.
///
/// Only when a decode family is served at all, so that the reply of a GPU side
/// without video is byte-for-byte what it was.
fn queue_family_video_links(
    served: &crate::driver::ServedFamily,
    decode_served: bool,
) -> Vec<p::PNextLink> {
    if !decode_served {
        return Vec::new();
    }
    let mut v = p::VkQueueFamilyVideoPropertiesKHR::default();
    v.video_codec_operations =
        p::VkVideoCodecOperationFlagsKHR(served.video_codec_operations.as_raw());
    let mut q = p::VkQueueFamilyQueryResultStatusPropertiesKHR::default();
    q.query_result_status_support = served.query_result_status;
    vec![
        p::PNextLink::VkQueueFamilyVideoPropertiesKHR(Box::new(v)),
        p::PNextLink::VkQueueFamilyQueryResultStatusPropertiesKHR(Box::new(q)),
    ]
}

/// A video session bind's driver `memoryTypeBits`, as docs/design.md §9.1's
/// table numbers it: bit 0 — our `DEVICE_LOCAL` type, and never an arena bit —
/// whenever the driver accepts any type at all.
///
/// Bit 0 is honest even when the driver's bits exclude the type `allocate_memory`
/// normally backs our type 0 with, because `bind_video_session_memory_khr`
/// re-makes a dedicated allocation from a type the driver does accept. RADV's AV1
/// session is the case that needs it: its bits are `0x678`, all host-visible
/// types, none of them RADV's type 0.
///
/// Zero is reported as zero: an application then finds no memory type and fails
/// at allocation. Not seen on RADV; said once on stderr if it is.
fn session_memory_type_bits(driver_bits: u32) -> u32 {
    if driver_bits == 0 {
        use std::sync::atomic::{AtomicBool, Ordering};
        static SAID: AtomicBool = AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            eprintln!("sv-gpu-server: a video session bind accepts no memory type at all");
        }
        return 0;
    }
    1
}

/// The driver memory type a session bind is re-made from: the first
/// `DEVICE_LOCAL` type the driver accepts, else the first type it accepts.
fn session_memory_type(driver: &Driver, driver_bits: u32) -> Option<u32> {
    driver
        .memory_type(driver_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
        .or_else(|| driver.memory_type(driver_bits, vk::MemoryPropertyFlags::empty()))
}

/// An image's driver `memoryTypeBits` in §9.1's numbering.
///
/// Bit 0 exactly when the driver's bits include our type 0's driver type (the
/// first `DEVICE_LOCAL`, as `allocate_memory` picks it). Bits 1–3, the arena's three
/// types, exactly when the driver's bits include the type the arena was
/// actually allocated from — which is what binding an image there would mean.
/// On RADV this reproduces almost exactly what the verbatim forward said (its
/// type 0 is `DEVICE_LOCAL` and the arena is its type 2); what it changes is
/// NVIDIA, whose type 0 has no flags.
fn app_image_memory_type_bits(shared: &Shared, driver_bits: u32) -> u32 {
    let dl = shared
        .driver
        .memory_type(u32::MAX, vk::MemoryPropertyFlags::DEVICE_LOCAL)
        .map_or(0, |dl| u32::from(driver_bits & (1 << dl) != 0));
    let arena = if driver_bits & (1 << shared.arena.memory_type) != 0 { 0b1110 } else { 0 };
    dl | arena
}

/// The requirements of an image bound to a shared section (§104): our type 0
/// only, which is what `svAllocShared` imports as, and a dedicated allocation —
/// one section per image, which is what makes it one dma-buf.
fn shared_image_requirements(r: &mut p::VkMemoryRequirements2) {
    r.memory_requirements.memory_type_bits = 1;
    for l in r.p_next.iter_mut() {
        if let p::PNextLink::VkMemoryDedicatedRequirements(d) = l {
            d.prefers_dedicated_allocation = true;
            d.requires_dedicated_allocation = true;
        }
    }
}

/// `VK_SUCCESS`.
const OK: p::VkResult = p::VkResult(0);

fn fail(e: vk::Result) -> p::VkResult {
    p::VkResult(e.as_raw())
}

/// Say that the driver returned a lost device or an out-of-memory, once per
/// command and result (waycompose-design.md §95).
///
/// Those are the results that end an application, and the reply that carries
/// them says nothing about *which side* produced them: on the guest a Firefox
/// died of `VK_ERROR_DEVICE_LOST` twice and this side's log was empty, so
/// nobody could tell the driver's loss from the transport's. Other negative
/// results are routine answers (`VK_ERROR_FORMAT_NOT_SUPPORTED` from a format
/// probe, `VK_ERROR_OUT_OF_POOL_MEMORY` from a descriptor pool an application
/// grows on demand) and stay quiet.
pub(crate) fn note_driver_error(command: &'static str, result: i32) {
    let what = match result {
        -1 => "VK_ERROR_OUT_OF_HOST_MEMORY",
        -2 => "VK_ERROR_OUT_OF_DEVICE_MEMORY",
        -4 => "VK_ERROR_DEVICE_LOST",
        _ => return,
    };
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SAID: Mutex<Option<HashSet<(&'static str, i32)>>> = Mutex::new(None);
    let first = SAID
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert((command, result));
    if first {
        eprintln!("sv-gpu-server: the driver returned {what} from {command}");
    }
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
            Ok(h) => {
                // **This pass is the command-buffer traversal** (design.md
                // §0.24). It already visits every handle anywhere in the
                // argument graph — nested inside `VkSubmitInfo`, inside
                // `VkCommandBufferSubmitInfo`, at any depth — so noting the
                // poisoned ones here means the check needs no walker of its own
                // and cannot miss the one command that matters. A walker written
                // against the wire signature did miss it: `vkQueueSubmit`'s
                // buffers are a member of `VkSubmitInfo`, not a parameter.
                if ty == HandleType::VkCommandBuffer && self.dispatch_poison.is_none() {
                    self.dispatch_poison = self.shared.poisoned.get(h.as_raw());
                }
                Ok(h.as_raw())
            }
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

    /// The driver has no entry point for a command the profile carries.
    ///
    /// # Why this says something out loud, and why that is not the whole fix
    ///
    /// `last_error` alone reaches nobody here. It is consumed only where a
    /// command returns `Err` (`session.rs`, the blob decoder and the ring's
    /// fatal path), and the generated caller deliberately does **not**: a
    /// missing entry point is a normal configuration, because the profile is a
    /// superset of any one driver's extensions, and failing the ring over
    /// `vkSetDebugUtilsObjectNameEXT` would lose the device over a debug label.
    ///
    /// For a command that generates a reply, the trade is sound — the caller
    /// gets `VK_ERROR_EXTENSION_NOT_PRESENT` and can act on it. For a **recorded
    /// `vkCmd*` whose entire output is a side effect** it is not:
    /// `vkCmdDecodeVideoKHR` returned `Ok(())`, nothing was recorded, no reply
    /// existed to carry the fact, and no caller checking a return value could
    /// have noticed. That is the silent class §0.19 item 1 counts, on the GPU
    /// side rather than the app side, and it is the sharpest argument that
    /// *carried* and *implemented* have to stay separate claims.
    ///
    /// Reported once per command name, always rather than under a variable,
    /// exactly as `sv_icd_entry::not_implemented` is and for the same reason:
    /// when the only evidence of a silent failure is whether we said anything,
    /// gating it on an environment variable nobody sets makes the worst failure
    /// mode the quietest one.
    ///
    /// **What this deliberately does not do** is turn a missing recorded command
    /// into a ring fatal. That would be the Vulkan-correct outcome — a command
    /// buffer that could not record a command is invalid, and its submit should
    /// fail — but the blob decoder's `Err` path sets `fatal` on the ring and
    /// kills the session, which is far too blunt for a driver that merely lacks
    /// an optional extension. Poisoning the *command buffer* instead, so that
    /// its submit fails and nothing else does, is the right shape and is not
    /// written. §0.22 states it as an open item rather than pretending the
    /// message closes it.
    ///
    /// The primary defence is upstream and does hold: a command belonging to an
    /// extension whose queue family this ICD cannot offer is no longer
    /// advertised at all (`sv_icd::advertise::unservable_queue_requirement`), so
    /// a conforming application never reaches here.
    fn note_missing(&mut self, command: &'static str) {
        use std::collections::HashSet;
        use std::sync::Mutex;
        static SAID: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
        let first = {
            let mut g = SAID.lock().unwrap_or_else(|p| p.into_inner());
            g.get_or_insert_with(HashSet::new).insert(command)
        };
        if first {
            eprintln!(
                "sv-gpu-server: the driver has no {command}; the profile is wider \
                 than this driver."
            );
        }
        self.last_error =
            Some(format!("the driver has no {command}; the profile is wider than this driver"));
    }

    /// §0.24: a recorded command that could not be recorded invalidates its
    /// command buffer, and the submit is where that becomes visible.
    ///
    /// This is the answer §0.22 item 5 left open. `Ok(())` was silent and `Err`
    /// took the ring fatal, and neither was right because the failure is not the
    /// `vkCmd*`'s to report — Vulkan says a command buffer whose recording
    /// failed is invalid, and this is that state, kept where the only thing that
    /// can act on it lives.
    fn poison_command_buffer(&mut self, command_buffer: u64, why: &'static str) {
        self.shared.poisoned.insert(command_buffer, why);
    }

    fn strip_poisoned_submit(&mut self, a: &mut p::VkQueueSubmitArgs) -> bool {
        // A device-group chain carries one mask per command buffer, so a batch
        // with any extension struct is not taken apart.
        if a.p_submits.iter().any(|s| !s.p_next.is_empty()) {
            return false;
        }
        for s in &mut a.p_submits {
            s.p_command_buffers.retain(|cb| self.shared.poisoned.get(cb.0).is_none());
            s.command_buffer_count = s.p_command_buffers.len() as u32;
        }
        true
    }

    fn strip_poisoned_submit2(&mut self, a: &mut p::VkQueueSubmit2Args) -> bool {
        a.p_submits.clear();
        a.submit_count = 0;
        true
    }

    fn settle_stripped_submit(&mut self, fence: u64) {
        if fence == 0 {
            return;
        }
        let f = vk::Fence::from_raw(fence);
        // SAFETY: the fence is the driver's, from the table, and the submit
        // that names it has just returned. A second at most: the batch is
        // empty, so it completes once earlier work on the queue has.
        let _ = unsafe { self.shared.driver.device.wait_for_fences(&[f], true, 1_000_000_000) };
    }

    fn unpoison_command_buffer(&mut self, command_buffer: u64) {
        self.shared.poisoned.remove(command_buffer);
    }

    fn observing(&self, command: &'static str) -> bool {
        crate::video_log::enabled() || STABLE_OBSERVED.contains(&command)
    }

    fn observe(&self, command: &'static str, after: bool, args: &[u64], result: i32) {
        if after && STABLE_OBSERVED.contains(&command) {
            // SAFETY: as below; `stable_observe` reads only what each
            // command's signature says the words are.
            unsafe { stable_observe(&self.shared, command, args, result) };
        }
        if crate::video_log::enabled() {
            // SAFETY: the generated dispatch calls this with the arguments it
            // is about to pass, or has just passed, to the driver —
            // `GpuHost::observe`'s contract, which is `video_log::observe`'s.
            unsafe { crate::video_log::observe(&self.shared, command, after, args, result) }
        }
    }

    fn note_driver_error(&self, command: &'static str, result: i32) {
        note_driver_error(command, result);
    }

    fn retain_raise(&mut self, command: &'static str, pool: sv_gpu_entry::Raise) {
        if matches!(command, "vkCreateVideoSessionParametersKHR" | "vkUpdateVideoSessionParametersKHR") {
            self.shared.video_stable.lock().unwrap_or_else(|e| e.into_inner()).keep_params(pool);
            return;
        }
        let keep = video_retain_limit();
        if keep == 0 {
            return;
        }
        self.retained.push_back(pool);
        while self.retained.len() > keep {
            self.retained.pop_front();
        }
    }

    fn take_poisoned_command_buffer(&mut self) -> Option<&'static str> {
        self.dispatch_poison.take()
    }

    fn note_poisoned_submit(&mut self, command: &'static str, why: &'static str) {
        let msg = format!(
            "{command} names a command buffer whose recording failed: the driver \
             has no {why}"
        );
        // Always, and once per pair. A refused submit is the *good* outcome here
        // — it is the failure being reported instead of swallowed — but a
        // caller sees only an error code, and the code cannot say which command
        // could not be recorded three hundred calls ago.
        use std::collections::HashSet;
        use std::sync::Mutex;
        static SAID: Mutex<Option<HashSet<(&'static str, &'static str)>>> = Mutex::new(None);
        let first = {
            let mut g = SAID.lock().unwrap_or_else(|p| p.into_inner());
            g.get_or_insert_with(HashSet::new).insert((command, why))
        };
        if first {
            eprintln!("sv-gpu-server: {msg}");
        }
        self.last_error = Some(msg);
    }

    fn lock_queue(&self) -> sv_gpu_entry::QueueGuard<'_> {
        sv_gpu_entry::QueueGuard::held(&self.shared.queue_lock)
    }

    fn lock_command_pool(&self) -> sv_gpu_entry::QueueGuard<'_> {
        sv_gpu_entry::QueueGuard::held(&self.shared.pool_lock)
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
        //
        // What it must not do is ask for a feature the device lacks and be
        // told yes: every command using it would then be undefined behaviour
        // on the driver (Zink and `VK_EXT_extended_dynamic_state3`,
        // docs/video-decode.md §7.11). That is the specification's
        // `VK_ERROR_FEATURE_NOT_PRESENT`, and the members are named.
        if let Some(info) = a.p_create_info.as_deref() {
            let driver = &self.shared.driver;
            let missing = crate::features::not_enabled(info, &driver.features);
            if !missing.is_empty() {
                eprintln!(
                    "sv-gpu-server: vkCreateDevice asks for {} feature(s) this device did not \
                     enable, refused with VK_ERROR_FEATURE_NOT_PRESENT: {}",
                    missing.len(),
                    missing.join(", ")
                );
                return Ok(p::VkCreateDeviceReply {
                    result: fail(vk::Result::ERROR_FEATURE_NOT_PRESENT),
                    p_device: None,
                });
            }
            // Extensions are reported, not refused: the app side implements some
            // itself (WSI), and the GPU side drops some it advertises when their
            // instance-level dependency is absent (`crate::driver`).
            for e in &info.pp_enabled_extension_names {
                let e = String::from_utf8_lossy(e);
                let e = e.trim_end_matches('\0');
                if !driver.enabled_extensions.iter().any(|x| x == e) {
                    eprintln!("sv-gpu-server: vkCreateDevice enables {e}, which this device did not");
                }
            }
        }
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
        let q = self.queue_for_family(a.queue_family_index);
        if let Some(id) = a.p_queue {
            self.shared.objects().insert(id.0, HandleType::VkQueue, q);
        }
        Ok(p::VkGetDeviceQueueReply { p_queue: a.p_queue })
    }

    fn get_device_queue2(
        &mut self,
        a: p::VkGetDeviceQueue2Args,
    ) -> WireResult<p::VkGetDeviceQueue2Reply> {
        let family = a.p_queue_info.as_ref().map(|i| i.queue_family_index).unwrap_or(0);
        let q = self.queue_for_family(family);
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
        // **The dedicated-allocation link is carried; nothing else in the chain
        // is.** A driver may *require* an image's memory to be dedicated to it
        // (`VkMemoryDedicatedRequirements::requiresDedicatedAllocation`, which
        // RADV reports for ffmpeg's decode pictures), and the GPU side's
        // validation layer reported `VUID-VkBindImageMemoryInfo-image-01445` on
        // every one of them while this link was being dropped (docs/video-decode.md
        // §7.5). The image or buffer it names is mapped to the driver's handle;
        // an id that does not resolve fails the call rather than dedicating the
        // memory to nothing.
        let dedicated = info.p_next.iter().find_map(|l| match l {
            p::PNextLink::VkMemoryDedicatedAllocateInfo(d) => Some((d.image.0, d.buffer.0)),
            _ => None,
        });
        let mut ded = vk::MemoryDedicatedAllocateInfo::default();
        if let Some((image, buffer)) = dedicated {
            ded.image = vk::Image::from_raw(self.map_handle(HandleType::VkImage, image)?);
            ded.buffer = vk::Buffer::from_raw(self.map_handle(HandleType::VkBuffer, buffer)?);
        }
        // **And the allocate flags** (waycompose-design.md §96). Since buffers
        // may live in type 0, a buffer created for `SHADER_DEVICE_ADDRESS` can
        // be bound here, and its memory must have been allocated with
        // `DEVICE_ADDRESS` (`VUID-vkBindBufferMemory-bufferDeviceAddress-03339`,
        // reported for Zink's buffers the first time they left the arena).
        // The flags are the application's; the device mask is meaningful only
        // with `DEVICE_MASK`, which a single-device GPU side keeps as given.
        let flags = info.p_next.iter().find_map(|l| match l {
            p::PNextLink::VkMemoryAllocateFlagsInfo(f) => Some((f.flags.0, f.device_mask)),
            _ => None,
        });
        let mut fl = vk::MemoryAllocateFlagsInfo::default();
        if let Some((bits, mask)) = flags {
            fl = fl.flags(vk::MemoryAllocateFlags::from_raw(bits)).device_mask(mask);
        }
        let mut ai = vk::MemoryAllocateInfo::default()
            .allocation_size(info.allocation_size)
            .memory_type_index(ti);
        if dedicated.is_some() {
            ai = ai.push_next(&mut ded);
        }
        if flags.is_some() {
            ai = ai.push_next(&mut fl);
        }
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
            Err(e) => {
                note_driver_error("vkAllocateMemory", e.as_raw());
                Ok(p::VkAllocateMemoryReply { result: fail(e), p_memory: None })
            }
        }
    }

    /// Forwarded, with one `pNext` link the application did not write.
    ///
    /// **The only command on this surface the GPU side alters rather than
    /// reproduces**, and the alteration is about the arena rather than about
    /// buffers. The arena's `VkDeviceMemory` is external memory on every
    /// provider of docs/design.md §5.4 — exported as a dma-buf here, imported as a
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
    /// **It cannot be done by the app side.** docs/design.md §0.4 says "the app side
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
        crate::video_log::buffer_created(buffer as usize as u64, ci.usage.0, ci.size, r.0);
        note_driver_error("vkCreateBuffer", r.0);
        if let Some(id) = a.p_buffer {
            self.bind(id.0, HandleType::VkBuffer, buffer as usize as u64);
        }
        Ok(p::VkCreateBufferReply { result: p::VkResult(r.0), p_buffer: a.p_buffer })
    }

    // -- video images and their views (docs/video-decode.md §7.11) -----------
    //
    // Forwarded as the generated code would, plus `crate::video_views`: a video
    // image's views are created once per description and destroyed with the
    // image, because Intel's Windows driver reads a view after ffmpeg has
    // destroyed it. The observer sees exactly the calls the driver sees.

    fn create_image(&mut self, a: p::VkCreateImageArgs) -> WireResult<p::VkCreateImageReply> {
        let mut a = a;
        a.device.0 = self.map_handle(HandleType::VkDevice, a.device.0)?;
        let info = a.p_create_info.as_mut().ok_or_else(no_info)?;
        {
            let mut ad = sv_gpu_entry::Mapping(&mut *self);
            let m: &mut dyn p::HandleMapper = &mut ad;
            p::handle_map_vk_image_create_info(info, m)?;
        }
        // §104: the app side marks an image it will bind to a shared section
        // (its dma-buf) with `VkExternalMemoryImageCreateInfo{HOST_ALLOCATION}`.
        // It is LINEAR, names that one handle type, and is created only if the
        // driver said such an image can be bound to imported host memory: on
        // the guest a driver fault can stop the VM, so a combination the
        // driver did not promise is never sent.
        let host_alloc = info.p_next.iter_mut().find_map(|l| match l {
            p::PNextLink::VkExternalMemoryImageCreateInfo(x) if x.handle_types.0 & crate::shared::HOST_ALLOCATION != 0 => {
                Some(x)
            }
            _ => None,
        });
        let shared_image = if let Some(x) = host_alloc {
            x.handle_types.0 = crate::shared::HOST_ALLOCATION;
            info.tiling = p::VkImageTiling(vk::ImageTiling::LINEAR.as_raw());
            if let Err(why) = crate::shared::image_importable(&self.shared.driver, info) {
                eprintln!(
                    "sv-gpu-server: vkCreateImage of a shared image refused, format {} {}x{} usage {:#x}: {why}",
                    info.format.0, info.extent.width, info.extent.height, info.usage.0
                );
                return Ok(p::VkCreateImageReply {
                    result: fail(vk::Result::ERROR_FORMAT_NOT_SUPPORTED),
                    p_image: None,
                });
            }
            true
        } else {
            false
        };
        let Some(f) = self.fns().vk_create_image else {
            self.note_missing("vkCreateImage");
            return Ok(p::VkCreateImageReply { result: p::VkResult(-3), p_image: None });
        };
        let mut pool = sv_gpu_entry::Raise::default();
        let ci = sv_gpu_entry::raise_vk_image_create_info(info, &mut pool)?;
        let mut image = Default::default();
        let device = a.device.0 as usize as _;
        let words = [a.device.0, (&raw const ci) as u64, 0, (&raw mut image) as u64];
        let obs = crate::video_log::enabled();
        if obs {
            self.observe("vkCreateImage", false, &words, 0);
        }
        // SAFETY: `ci` and everything it points to live in `pool`.
        let r = unsafe { f(device, &ci, core::ptr::null(), &mut image) };
        note_driver_error("vkCreateImage", r.0);
        if obs {
            self.observe("vkCreateImage", true, &words, r.0);
        }
        if r.0 == 0 {
            self.shared.objects().video_views.image_created(image as usize as u64, ci.usage.0);
            if shared_image {
                self.shared.objects().shared_images.insert(image as usize as u64);
            }
        }
        if let Some(id) = a.p_image {
            self.bind(id.0, HandleType::VkImage, image as usize as u64);
        }
        Ok(p::VkCreateImageReply { result: p::VkResult(r.0), p_image: a.p_image })
    }

    fn create_image_view(
        &mut self,
        a: p::VkCreateImageViewArgs,
    ) -> WireResult<p::VkCreateImageViewReply> {
        let mut a = a;
        a.device.0 = self.map_handle(HandleType::VkDevice, a.device.0)?;
        let info = a.p_create_info.as_mut().ok_or_else(no_info)?;
        {
            let mut ad = sv_gpu_entry::Mapping(&mut *self);
            let m: &mut dyn p::HandleMapper = &mut ad;
            p::handle_map_vk_image_view_create_info(info, m)?;
        }
        use crate::video_views::Create;
        let plan = if crate::video_views::enabled() {
            self.shared.objects().video_views.lookup(info)
        } else {
            Create::Forward
        };
        if let Create::Reuse(view) = plan {
            if let Some(id) = a.p_view {
                self.bind(id.0, HandleType::VkImageView, view);
            }
            return Ok(p::VkCreateImageViewReply { result: OK, p_view: a.p_view });
        }
        let Some(f) = self.fns().vk_create_image_view else {
            self.note_missing("vkCreateImageView");
            return Ok(p::VkCreateImageViewReply { result: p::VkResult(-3), p_view: None });
        };
        let mut pool = sv_gpu_entry::Raise::default();
        let ci = sv_gpu_entry::raise_vk_image_view_create_info(info, &mut pool)?;
        let mut view = Default::default();
        let device = a.device.0 as usize as _;
        let words = [a.device.0, (&raw const ci) as u64, 0, (&raw mut view) as u64];
        let obs = crate::video_log::enabled();
        if obs {
            self.observe("vkCreateImageView", false, &words, 0);
        }
        // SAFETY: `ci` and everything it points to live in `pool`.
        let r = unsafe { f(device, &ci, core::ptr::null(), &mut view) };
        if obs {
            self.observe("vkCreateImageView", true, &words, r.0);
        }
        let view = view as usize as u64;
        if r.0 == 0 && plan == Create::CreateAndKeep {
            self.shared.objects().video_views.keep((**info).clone(), view);
        }
        if let Some(id) = a.p_view {
            self.bind(id.0, HandleType::VkImageView, view);
        }
        Ok(p::VkCreateImageViewReply { result: p::VkResult(r.0), p_view: a.p_view })
    }

    fn destroy_image_view(&mut self, a: p::VkDestroyImageViewArgs) -> WireResult<()> {
        let id = a.image_view.0;
        let device = self.map_handle(HandleType::VkDevice, a.device.0)?;
        let view = self.map_handle(HandleType::VkImageView, id)?;
        self.unbind(id);
        if view == 0 || self.shared.objects().video_views.is_kept(view) {
            return Ok(());
        }
        self.destroy_view_on_driver(device, view);
        Ok(())
    }

    fn destroy_image(&mut self, a: p::VkDestroyImageArgs) -> WireResult<()> {
        let id = a.image.0;
        let device = self.map_handle(HandleType::VkDevice, a.device.0)?;
        let image = self.map_handle(HandleType::VkImage, id)?;
        self.unbind(id);
        if image == 0 {
            return Ok(());
        }
        self.shared.objects().shared_images.remove(&image);
        let kept = self.shared.objects().video_views.image_destroyed(image);
        for view in kept {
            self.destroy_view_on_driver(device, view);
        }
        let Some(f) = self.fns().vk_destroy_image else {
            self.note_missing("vkDestroyImage");
            return Ok(());
        };
        let words = [device, image, 0];
        let obs = crate::video_log::enabled();
        if obs {
            self.observe("vkDestroyImage", false, &words, 0);
        }
        // SAFETY: both handles are the driver's, from the table.
        unsafe { f(device as usize as _, image, core::ptr::null()) };
        if obs {
            self.observe("vkDestroyImage", true, &words, 0);
        }
        Ok(())
    }

    // -- the coding scope and the decode (docs/video-decode.md §7.11.5) ------
    //
    // Forwarded as the generated code would, except that the structures the
    // driver is handed live in `crate::video_stable`'s per-slot stores rather
    // than in this call's raise pool: Intel's Windows driver follows pointers
    // from earlier calls. Observed (`SV_VIDEO_LOG`) with exactly what the driver
    // receives.

    fn cmd_begin_video_coding_khr(&mut self, a: p::VkCmdBeginVideoCodingKHRArgs) -> WireResult<()> {
        let mut a = a;
        a.command_buffer.0 = self.map_handle(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        if let Some(info) = a.p_begin_info.as_mut() {
            let mut ad = sv_gpu_entry::Mapping(&mut *self);
            let m: &mut dyn p::HandleMapper = &mut ad;
            p::handle_map_vk_video_begin_coding_info_khr(info, m)?;
        }
        let _ = self.take_poisoned_command_buffer();
        let Some(f) = self.fns().vk_cmd_begin_video_coding_khr else {
            self.note_missing("vkCmdBeginVideoCodingKHR");
            self.poison_command_buffer(a.command_buffer.0, "vkCmdBeginVideoCodingKHR");
            return Ok(());
        };
        let mut pool = sv_gpu_entry::Raise::default();
        let info: *const sv_gpu_entry::VkVideoBeginCodingInfoKHR = match &a.p_begin_info {
            Some(x) => {
                let v = sv_gpu_entry::raise_vk_video_begin_coding_info_khr(x, &mut pool)?;
                pool.one(v) as _
            }
            None => std::ptr::null(),
        };
        let cb = a.command_buffer.0;
        // SAFETY: `info` and its array were raised into `pool` for this call.
        let verdict = unsafe { self.shared.video_stable.lock().unwrap_or_else(|e| e.into_inner()).begin(cb, info) };
        if let Err(r) = verdict {
            self.refuse_video(cb, "vkCmdBeginVideoCodingKHR", r);
            return Ok(());
        }
        let words = [cb, info as u64];
        let obs = crate::video_log::enabled();
        if obs {
            self.observe("vkCmdBeginVideoCodingKHR", false, &words, 0);
        }
        // SAFETY: the handles are the driver's; every pointer is in `pool` or
        // in the stores, which outlive the call.
        unsafe { f(cb as usize as _, info) };
        if obs {
            self.observe("vkCmdBeginVideoCodingKHR", true, &words, 0);
        }
        self.retain_raise("vkCmdBeginVideoCodingKHR", pool);
        Ok(())
    }

    fn cmd_decode_video_khr(&mut self, a: p::VkCmdDecodeVideoKHRArgs) -> WireResult<()> {
        let mut a = a;
        a.command_buffer.0 = self.map_handle(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        if let Some(info) = a.p_decode_info.as_mut() {
            let mut ad = sv_gpu_entry::Mapping(&mut *self);
            let m: &mut dyn p::HandleMapper = &mut ad;
            p::handle_map_vk_video_decode_info_khr(info, m)?;
        }
        let _ = self.take_poisoned_command_buffer();
        let Some(f) = self.fns().vk_cmd_decode_video_khr else {
            self.note_missing("vkCmdDecodeVideoKHR");
            self.poison_command_buffer(a.command_buffer.0, "vkCmdDecodeVideoKHR");
            return Ok(());
        };
        let mut pool = sv_gpu_entry::Raise::default();
        let raised: *const sv_gpu_entry::VkVideoDecodeInfoKHR = match &a.p_decode_info {
            Some(x) => {
                let v = sv_gpu_entry::raise_vk_video_decode_info_khr(x, &mut pool)?;
                pool.one(v) as _
            }
            None => std::ptr::null(),
        };
        let cb = a.command_buffer.0;
        // A command buffer the slot guard refused earlier in this recording is
        // never submitted; nothing more is recorded into it at the driver.
        // Intel parses a decode at record time, and this one may lie outside
        // any scope the driver saw begun.
        if self.shared.poisoned.get(cb).is_some() {
            return Ok(());
        }
        // SAFETY: `raised` and everything it points to are in `pool`.
        let verdict = unsafe {
            self.shared.video_stable.lock().unwrap_or_else(|e| e.into_inner()).decode(cb, raised)
        };
        let info = match verdict {
            crate::video_stable::Decode::Stable(p) => p,
            crate::video_stable::Decode::Refused(r) => {
                self.refuse_video(cb, "vkCmdDecodeVideoKHR", r);
                return Ok(());
            }
            crate::video_stable::Decode::Raised => {
                if !raised.is_null() {
                    static SAID: std::sync::Once = std::sync::Once::new();
                    SAID.call_once(|| {
                        eprintln!(
                            "sv-gpu-server: a vkCmdDecodeVideoKHR this side cannot keep at stable addresses \
                             (not H.264, H.265 or AV1, or outside a scope it saw); forwarded as raised \
                             (docs/video-decode.md §7.12)"
                        )
                    });
                }
                raised
            }
        };
        let words = [cb, info as u64];
        let obs = crate::video_log::enabled();
        if obs {
            self.observe("vkCmdDecodeVideoKHR", false, &words, 0);
        }
        // SAFETY: as in `cmd_begin_video_coding_khr`.
        unsafe { f(cb as usize as _, info) };
        if obs {
            self.observe("vkCmdDecodeVideoKHR", true, &words, 0);
        }
        self.retain_raise("vkCmdDecodeVideoKHR", pool);
        Ok(())
    }

    fn cmd_end_video_coding_khr(&mut self, a: p::VkCmdEndVideoCodingKHRArgs) -> WireResult<()> {
        let cb = self.map_handle(HandleType::VkCommandBuffer, a.command_buffer.0)?;
        let _ = self.take_poisoned_command_buffer();
        let Some(f) = self.fns().vk_cmd_end_video_coding_khr else {
            self.note_missing("vkCmdEndVideoCodingKHR");
            self.poison_command_buffer(cb, "vkCmdEndVideoCodingKHR");
            return Ok(());
        };
        let mut pool = sv_gpu_entry::Raise::default();
        let info: *const sv_gpu_entry::VkVideoEndCodingInfoKHR = match &a.p_end_coding_info {
            Some(x) => {
                let v = sv_gpu_entry::raise_vk_video_end_coding_info_khr(x, &mut pool)?;
                pool.one(v) as _
            }
            None => std::ptr::null(),
        };
        // The scope's begin was refused (the slot guard): nothing was begun at
        // the driver, so nothing is ended there either.
        if !self.shared.video_stable.lock().unwrap_or_else(|e| e.into_inner()).end(cb)
            && self.shared.poisoned.get(cb).is_some()
        {
            return Ok(());
        }
        let words = [cb, info as u64];
        let obs = crate::video_log::enabled();
        if obs {
            self.observe("vkCmdEndVideoCodingKHR", false, &words, 0);
        }
        // SAFETY: the handle is the driver's; `info` is in `pool`.
        unsafe { f(cb as usize as _, info) };
        if obs {
            self.observe("vkCmdEndVideoCodingKHR", true, &words, 0);
        }
        Ok(())
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
        // A shared section's view goes after its import (§104).
        let section = {
            let mut o = self.shared.objects();
            o.remove(id);
            o.shared_imports.remove(&id)
        };
        drop(section);
        Ok(())
    }

    fn map_memory(&mut self, a: p::VkMapMemoryArgs) -> WireResult<p::VkMapMemoryReply> {
        // Nothing crosses, and the wire reply has nowhere to put an address —
        // `ppData` is on §6.1's exclusion list ("pointers to mapped memory
        // regions … are **not serialisable**"), so `VkMapMemoryReply` carries a
        // result and nothing else. The app side's `vkMapMemory` returns a
        // pointer into its own mapping of the arena (docs/design.md §5.3); this
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
        // Reported for diagnostics only: docs/design.md §9.1 says the app side
        // advertises its own four-type table and does not mirror this one, and
        // `sv-icd` answers the command locally so this reply is never consulted.
        Ok(p::VkGetPhysicalDeviceMemoryPropertiesReply {
            p_memory_properties: Some(Box::new(crate::convert::memory_properties(
                &self.shared.driver.mem_props,
            ))),
        })
    }

    /// The driver's answer, masked to what this device enabled
    /// (`crate::features::mask_to_enabled`): an application told a feature is
    /// supported asks for it, and the device it gets already exists.
    fn get_physical_device_features2(
        &mut self,
        a: p::VkGetPhysicalDeviceFeatures2Args,
    ) -> WireResult<p::VkGetPhysicalDeviceFeatures2Reply> {
        let driver = &self.shared.driver;
        let template = a.p_features.map(|f| *f).unwrap_or_default();
        let mut f = crate::features::query_driver(&driver.instance, driver.pd, &template)
            .map_err(|e| {
                eprintln!("sv-gpu-server: vkGetPhysicalDeviceFeatures2: {e}");
                WireError::UnknownCommand { opcode: 0 }
            })?;
        crate::features::mask_to_enabled(&mut f, &driver.features);
        Ok(p::VkGetPhysicalDeviceFeatures2Reply { p_features: Some(Box::new(f)) })
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

    /// The queue families this GPU side serves, each described from the queue
    /// it actually opened.
    ///
    /// Not the driver's whole table. `crate::driver` opens one queue per served
    /// family, and an application handed RADV's five families would ask for
    /// family 3 and be given another family's queue with no diagnostic. Without
    /// video decode that is one family, presented as family 0, exactly as
    /// before; with it, it is the driver's prefix up to and including the
    /// decode family, at the driver's own indices (`driver::ServedFamily` says
    /// why the indices are not renumbered). The flags are the real ones, minus
    /// the capabilities we do not serve — it is the *selection* that is ours,
    /// as with the memory table of §9.1.
    fn get_physical_device_queue_family_properties(
        &mut self,
        a: p::VkGetPhysicalDeviceQueueFamilyPropertiesArgs,
    ) -> WireResult<p::VkGetPhysicalDeviceQueueFamilyPropertiesReply> {
        let all = self.served_queue_families();
        let n = all.len() as u32;
        let want = a.p_queue_family_properties.len();
        Ok(p::VkGetPhysicalDeviceQueueFamilyPropertiesReply {
            p_queue_family_property_count: Some(if want == 0 { n } else { n.min(want as u32) }),
            p_queue_family_properties: all.into_iter().take(want).collect(),
        })
    }

    /// As the plain form, and it carries the two video links of the chain.
    ///
    /// `VkQueueFamilyVideoPropertiesKHR` is how an application finds out *which
    /// codecs* a decode family does — the flag alone says only that it decodes —
    /// and ffmpeg's `hwcontext_vulkan` reads it to choose its decode queue. Both
    /// links are answered from what `crate::driver` recorded at open, and the
    /// codec mask is the one restricted to extensions this device enabled.
    fn get_physical_device_queue_family_properties2(
        &mut self,
        a: p::VkGetPhysicalDeviceQueueFamilyProperties2Args,
    ) -> WireResult<p::VkGetPhysicalDeviceQueueFamilyProperties2Reply> {
        let served = &self.shared.driver.served;
        let decode_served = self.shared.driver.decode_family.is_some();
        let n = served.len() as u32;
        let want = a.p_queue_family_properties.len();
        let out: Vec<p::VkQueueFamilyProperties2> = a
            .p_queue_family_properties
            .into_iter()
            .zip(served.iter())
            .map(|(mut w, f)| {
                w.queue_family_properties = crate::convert::queue_family(&f.props);
                w.p_next = queue_family_video_links(f, decode_served);
                w
            })
            .collect();
        Ok(p::VkGetPhysicalDeviceQueueFamilyProperties2Reply {
            p_queue_family_property_count: Some(if want == 0 { n } else { n.min(want as u32) }),
            p_queue_family_properties: out,
        })
    }

    /// Forwarded for the numbers, renumbered for the mask.
    ///
    /// `memoryTypeBits` comes back in the driver's numbering and is read by the
    /// application against docs/design.md §9.1's table, so it is translated —
    /// the same argument `sv_icd::icd::get_buffer_memory_requirements` makes for
    /// buffers, made here because only this side knows which driver type our
    /// type 0 is. See [`session_memory_type_bits`].
    ///
    /// **Session memory never goes in the arena**, and the mask says so: it is
    /// memory only the video engine touches, which is docs/design.md §0.21.2's
    /// decision 1 applied to the session as well as to the pictures.
    fn get_video_session_memory_requirements_khr(
        &mut self,
        a: p::VkGetVideoSessionMemoryRequirementsKHRArgs,
    ) -> WireResult<p::VkGetVideoSessionMemoryRequirementsKHRReply> {
        let session = self.map_handle(HandleType::VkVideoSessionKHR, a.video_session.0)?;
        let Some(vq) = self.shared.driver.video_queue.as_ref() else {
            self.note_missing("vkGetVideoSessionMemoryRequirementsKHR");
            let mut r = p::VkGetVideoSessionMemoryRequirementsKHRReply::default();
            r.result = p::VkResult(vk::Result::ERROR_EXTENSION_NOT_PRESENT.as_raw());
            return Ok(r);
        };
        let session = vk::VideoSessionKHR::from_raw(session);
        let device = self.shared.driver.device.handle();
        let fp = vq.fp().get_video_session_memory_requirements_khr;
        let mut n = 0u32;
        // SAFETY: a count query; the session handle is the driver's own, mapped
        // above, and the device is the one it was created on.
        let r = unsafe { fp(device, session, &mut n, std::ptr::null_mut()) };
        let capacity = a.p_memory_requirements.len();
        if r != vk::Result::SUCCESS || capacity == 0 {
            return Ok(p::VkGetVideoSessionMemoryRequirementsKHRReply {
                result: p::VkResult(r.as_raw()),
                p_memory_requirements_count: Some(n),
                p_memory_requirements: Vec::new(),
            });
        }
        let mut got = vec![vk::VideoSessionMemoryRequirementsKHR::default(); capacity];
        let mut m = capacity as u32;
        // SAFETY: `got` holds `m` elements, each a valid output structure.
        let r = unsafe { fp(device, session, &mut m, got.as_mut_ptr()) };
        got.truncate(m as usize);
        let out = got
            .iter()
            .map(|g| {
                let mut w = p::VkVideoSessionMemoryRequirementsKHR::default();
                w.memory_bind_index = g.memory_bind_index;
                w.memory_requirements.size = g.memory_requirements.size;
                w.memory_requirements.alignment = g.memory_requirements.alignment;
                w.memory_requirements.memory_type_bits =
                    session_memory_type_bits(g.memory_requirements.memory_type_bits);
                w
            })
            .collect();
        Ok(p::VkGetVideoSessionMemoryRequirementsKHRReply {
            result: p::VkResult(r.as_raw()),
            p_memory_requirements_count: Some(m),
            p_memory_requirements: out,
        })
    }

    /// Forwarded, after making sure each dedicated type-0 allocation is of a
    /// driver type the session accepts for that bind index.
    ///
    /// [`session_memory_type_bits`] tells the application "our type 0" for any
    /// bind the driver can serve at all, and our type 0 is normally the driver's
    /// first `DEVICE_LOCAL` type. Where the driver wants something else — RADV's
    /// AV1 session, whose bits are all host-visible types — the allocation is
    /// re-made here, the first moment the session and bind index are known.
    ///
    /// **Only a dedicated allocation is re-made**: offset 0 and the whole
    /// allocation bound. That is what ffmpeg and `video-probe` do, and it is
    /// the only case where nothing else could already be bound into the memory
    /// being replaced. Anything else is passed through and the driver answers.
    fn bind_video_session_memory_khr(
        &mut self,
        a: p::VkBindVideoSessionMemoryKHRArgs,
    ) -> WireResult<p::VkBindVideoSessionMemoryKHRReply> {
        let session = vk::VideoSessionKHR::from_raw(
            self.map_handle(HandleType::VkVideoSessionKHR, a.video_session.0)?,
        );
        let Some((get_reqs, bind)) = self
            .shared
            .driver
            .video_queue
            .as_ref()
            .map(|vq| (vq.fp().get_video_session_memory_requirements_khr, vq.fp().bind_video_session_memory_khr))
        else {
            self.note_missing("vkBindVideoSessionMemoryKHR");
            return Ok(p::VkBindVideoSessionMemoryKHRReply {
                result: p::VkResult(vk::Result::ERROR_EXTENSION_NOT_PRESENT.as_raw()),
            });
        };
        let device = self.shared.driver.device.handle();
        // The driver's requirements, by bind index.
        let reqs = {
            let fp = get_reqs;
            let mut n = 0u32;
            // SAFETY: a count query on the driver's own session and device.
            // A failure leaves `n` at 0 and the bind below finds no
            // requirement to re-make against, which is the driver's answer.
            let _ = unsafe { fp(device, session, &mut n, std::ptr::null_mut()) };
            let mut v = vec![vk::VideoSessionMemoryRequirementsKHR::default(); n as usize];
            // SAFETY: `v` holds `n` output structures.
            let _ = unsafe { fp(device, session, &mut n, v.as_mut_ptr()) };
            v.truncate(n as usize);
            v
        };
        let usual = self.shared.driver.memory_type(u32::MAX, vk::MemoryPropertyFlags::DEVICE_LOCAL);
        let mut binds = Vec::with_capacity(a.p_bind_session_memory_infos.len());
        for b in &a.p_bind_session_memory_infos {
            let (memory, offset) = self.rewrite_memory(b.memory.0, b.memory_offset)?;
            let mut memory = vk::DeviceMemory::from_raw(memory);
            let want = reqs
                .iter()
                .find(|r| r.memory_bind_index == b.memory_bind_index)
                .map(|r| r.memory_requirements.memory_type_bits);
            let range = self.shared.objects().memory(b.memory.0);
            if let (Some(want), Some(range), Some(usual)) = (want, range, usual)
                && b.memory.0 != crate::ARENA_MEMORY_ID
                && want & (1 << usual) == 0
                && b.memory_offset == 0
                && b.memory_size == range.len
                && let Some(ty) = session_memory_type(&self.shared.driver, want)
            {
                let ai = vk::MemoryAllocateInfo::default()
                    .allocation_size(range.len)
                    .memory_type_index(ty);
                // SAFETY: an ordinary allocation on the driver's own device.
                match unsafe { self.d().allocate_memory(&ai, None) } {
                    Ok(m) => {
                        // SAFETY: the replaced allocation is dedicated to this
                        // bind (offset 0, whole size) and so has nothing bound.
                        unsafe { self.d().free_memory(range.memory, None) };
                        self.shared.objects().insert_memory(
                            b.memory.0,
                            crate::table::MemoryRange { memory: m, base: 0, len: range.len },
                        );
                        memory = m;
                    }
                    Err(e) => {
                        return Ok(p::VkBindVideoSessionMemoryKHRReply { result: fail(e) });
                    }
                }
            }
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(b.memory_bind_index)
                    .memory(memory)
                    .memory_offset(offset)
                    .memory_size(b.memory_size),
            );
        }
        // SAFETY: `binds` outlives the call; every handle in it is the driver's.
        let r = unsafe {
            bind(device, session, binds.len() as u32, binds.as_ptr())
        };
        Ok(p::VkBindVideoSessionMemoryKHRReply { result: p::VkResult(r.as_raw()) })
    }

    /// Forwarded for the numbers, renumbered for the mask, as above.
    ///
    /// Images had gone through verbatim since the first stage. That is right
    /// only where the driver's numbering happens to agree with docs/design.md
    /// §9.1's, which on RADV it nearly does (its type 0 is `DEVICE_LOCAL`) and
    /// on NVIDIA it does not (its type 0 has no flags) — see
    /// [`app_image_memory_type_bits`].
    fn get_image_memory_requirements(
        &mut self,
        a: p::VkGetImageMemoryRequirementsArgs,
    ) -> WireResult<p::VkGetImageMemoryRequirementsReply> {
        let image = vk::Image::from_raw(self.map_handle(HandleType::VkImage, a.image.0)?);
        // SAFETY: the driver's own image, mapped above, on the device it was
        // created on.
        let r = unsafe { self.d().get_image_memory_requirements(image) };
        let mut w = p::VkMemoryRequirements::default();
        w.size = r.size;
        w.alignment = r.alignment;
        w.memory_type_bits = app_image_memory_type_bits(&self.shared, r.memory_type_bits);
        // §104: an image bound to a shared section takes our type 0, which is
        // what `svAllocShared` imports as.
        if self.shared.objects().shared_images.contains(&image.as_raw()) {
            w.memory_type_bits = 1;
        }
        Ok(p::VkGetImageMemoryRequirementsReply { p_memory_requirements: Some(Box::new(w)) })
    }

    /// The `2` form: the generated path — it carries `VkImagePlaneMemoryRequirementsInfo`
    /// in and `VkMemoryDedicatedRequirements` out — with the mask renumbered on
    /// the way back.
    fn get_image_memory_requirements2(
        &mut self,
        a: p::VkGetImageMemoryRequirements2Args,
    ) -> WireResult<p::VkGetImageMemoryRequirements2Reply> {
        let mut a = a;
        {
            let mut ad = sv_gpu_entry::Mapping(&mut *self);
            let m: &mut dyn p::HandleMapper = &mut ad;
            if let Some(inner) = &mut a.p_info {
                p::handle_map_vk_image_memory_requirements_info2(inner, m)?;
            }
        }
        let Some(f) = self.fns().vk_get_image_memory_requirements2 else {
            self.note_missing("vkGetImageMemoryRequirements2");
            return Err(WireError::UnknownCommand { opcode: 0 });
        };
        let mut pool = sv_gpu_entry::Raise::default();
        let info: *const sv_gpu_entry::VkImageMemoryRequirementsInfo2 = match &a.p_info {
            Some(x) => {
                let v = sv_gpu_entry::raise_vk_image_memory_requirements_info2(x, &mut pool)?;
                pool.one(v)
            }
            None => std::ptr::null(),
        };
        let mut out = match &a.p_memory_requirements {
            Some(v) => sv_gpu_entry::raise_vk_memory_requirements2(v, &mut pool)?,
            None => Default::default(),
        };
        // SAFETY: `info` and the chain of `out` are owned by `pool`, which
        // outlives the call; the device is the driver's own.
        unsafe { f(self.d().handle().as_raw() as usize as _, info, &mut out) };
        // SAFETY: `out` was just written by the driver and its chain is `pool`'s.
        let mut reply = unsafe { sv_gpu_entry::lower_full_vk_memory_requirements2(&raw const out) }
            .map_err(|_| WireError::UnknownCommand { opcode: 0 })?;
        let bits = &mut reply.memory_requirements.memory_type_bits;
        *bits = app_image_memory_type_bits(&self.shared, *bits);
        let image = a.p_info.as_ref().map_or(0, |i| i.image.0);
        if self.shared.objects().shared_images.contains(&image) {
            shared_image_requirements(&mut reply);
        }
        Ok(p::VkGetImageMemoryRequirements2Reply { p_memory_requirements: Some(Box::new(reply)) })
    }

    /// As the `2` form, for an image that does not exist yet.
    fn get_device_image_memory_requirements(
        &mut self,
        a: p::VkGetDeviceImageMemoryRequirementsArgs,
    ) -> WireResult<p::VkGetDeviceImageMemoryRequirementsReply> {
        let mut a = a;
        {
            let mut ad = sv_gpu_entry::Mapping(&mut *self);
            let m: &mut dyn p::HandleMapper = &mut ad;
            if let Some(inner) = &mut a.p_info {
                p::handle_map_vk_device_image_memory_requirements(inner, m)?;
            }
        }
        let Some(f) = self.fns().vk_get_device_image_memory_requirements else {
            self.note_missing("vkGetDeviceImageMemoryRequirements");
            return Err(WireError::UnknownCommand { opcode: 0 });
        };
        let mut pool = sv_gpu_entry::Raise::default();
        let info: *const sv_gpu_entry::VkDeviceImageMemoryRequirements = match &a.p_info {
            Some(x) => {
                let v = sv_gpu_entry::raise_vk_device_image_memory_requirements(x, &mut pool)?;
                pool.one(v)
            }
            None => std::ptr::null(),
        };
        let mut out = match &a.p_memory_requirements {
            Some(v) => sv_gpu_entry::raise_vk_memory_requirements2(v, &mut pool)?,
            None => Default::default(),
        };
        // SAFETY: as `get_image_memory_requirements2`.
        unsafe { f(self.d().handle().as_raw() as usize as _, info, &mut out) };
        // SAFETY: as `get_image_memory_requirements2`.
        let mut reply = unsafe { sv_gpu_entry::lower_full_vk_memory_requirements2(&raw const out) }
            .map_err(|_| WireError::UnknownCommand { opcode: 0 })?;
        let bits = &mut reply.memory_requirements.memory_type_bits;
        *bits = app_image_memory_type_bits(&self.shared, *bits);
        let host_alloc = a.p_info.as_ref().and_then(|i| i.p_create_info.as_ref()).is_some_and(|ci| {
            ci.p_next.iter().any(|l| {
                matches!(l, p::PNextLink::VkExternalMemoryImageCreateInfo(x)
                    if x.handle_types.0 & crate::shared::HOST_ALLOCATION != 0)
            })
        });
        if host_alloc {
            shared_image_requirements(&mut reply);
        }
        Ok(p::VkGetDeviceImageMemoryRequirementsReply {
            p_memory_requirements: Some(Box::new(reply)),
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

    // -- 6. a shape one driver must never see -------------------------------

    /// Forwarded with an all-zero binding-flags link dropped, which is exact,
    /// and on Intel's Windows driver refused for a push set layout with
    /// non-zero binding flags, which could not be made exact. The argument is
    /// `crate::set_layout`'s, and docs/design.md §0.29's.
    fn create_descriptor_set_layout(
        &mut self,
        a: p::VkCreateDescriptorSetLayoutArgs,
    ) -> WireResult<p::VkCreateDescriptorSetLayoutReply> {
        let mut a = a;
        a.device.0 = self.map_handle(HandleType::VkDevice, a.device.0)?;
        let info = a.p_create_info.as_mut().ok_or_else(no_info)?;
        // The immutable samplers, which are handles: what the generated
        // forwarder's mapping pass would have done.
        p::handle_map_vk_descriptor_set_layout_create_info(info, &mut sv_gpu_entry::Mapping(&mut *self))?;
        let Some(f) = self.fns().vk_create_descriptor_set_layout else {
            self.note_missing("vkCreateDescriptorSetLayout");
            let mut r = p::VkCreateDescriptorSetLayoutReply::default();
            r.result = p::VkResult(crate::set_layout::VK_ERROR_INITIALIZATION_FAILED);
            return Ok(r);
        };
        let refuse = self.shared.driver.refuses_push_binding_flags();
        let (r, layout) = crate::set_layout::create(f, a.device.0, info, refuse)?;
        if r != 0 {
            let mut reply = p::VkCreateDescriptorSetLayoutReply::default();
            reply.result = p::VkResult(r);
            return Ok(reply);
        }
        if let Some(id) = a.p_set_layout {
            self.bind(id.0, HandleType::VkDescriptorSetLayout, layout);
        }
        Ok(p::VkCreateDescriptorSetLayoutReply { result: OK, p_set_layout: a.p_set_layout })
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
