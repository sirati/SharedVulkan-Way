//! The Vulkan ICD's C boundary: `extern "system"` stubs, proc-address tables,
//! and the lowering between the application's C structures and the wire types of
//! `sv-proto`.
//!
//! **This is the only crate in the project that contains `unsafe`, and almost
//! all of it is generated.** design.md §7.3 and `security-model.md` R7 both say
//! why: the ICD boundary is where raw pointers from an application arrive, that
//! cannot be avoided, and the mitigation is to make every one of the ~250 sites
//! the same three lines so they are reviewable by pattern rather than
//! individually. The generator is `sv_codegen::emit::icd_entry`; the invariants
//! every generated block rests on are written down in its module header, once,
//! rather than restated 250 times.
//!
//! What is hand-written here is this file, `local.rs`, and nothing else:
//!
//! - the object header a dispatchable handle points at ([`DispatchableHeader`]),
//!   because the loader's contract is a fact about memory layout and not
//!   something the registry describes;
//! - the entry points that cannot be a fold over a wire reply, in
//!   [`local`] — chiefly the ones that mint or destroy a dispatchable object;
//! - the error type, and the small helpers the generated code calls.
//!
//! # The one global
//!
//! The generated stubs reach the implementation through [`icd`], a process-wide
//! `&'static dyn Icd`. That is deliberate and it is not laziness about
//! threading state: a Vulkan ICD is loaded once per process, an application may
//! create several instances, and the wire object ids in the arguments already
//! say which instance or device a call belongs to (design.md §6.4). A pointer
//! chased out of the dispatchable handle would give the same answer with more
//! moving parts. Until `sv-icd` registers itself the global is [`NullIcd`],
//! whose every method is the trait's default refusal, so a call that arrives
//! before `vkCreateInstance` fails cleanly instead of dereferencing nothing.

// The generated code names Vulkan constants exactly as the registry spells them
// (`VK_FORMAT_ASTC_6x5_UNORM_BLOCK`), because a constant that greps against the
// specification is worth more than one that satisfies a lint. The same choice
// `sv-proto` makes, for the same reason.
#![allow(non_upper_case_globals)]
// `PFN_vkVoidFunction` is the name in `vulkan_core.h`, and an application that
// greps for it should find it.
#![allow(non_camel_case_types)]
// The emitters parenthesise every substituted access so that one expression is
// correct in every position it is spliced into. Emitting context-dependent
// parentheses would be a second thing for a generator to get right, in the crate
// where a mistake dereferences an application's pointer.
#![allow(unused_parens)]
// Style lints a generator would have to emit *shape-dependent* code to satisfy.
// Every one of them asks for a different spelling depending on what a field or a
// return type happens to be, and a generator that has to decide between two
// spellings is a generator with one more thing to get wrong. `sv-proto` makes
// the same trade for the same reason, and this crate has more claim to it: a
// mistake here dereferences an application's pointer.
#![allow(clippy::needless_return)]
#![allow(clippy::unused_unit)]
#![allow(clippy::let_unit_value)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::unusual_byte_groupings)]
// `matches!(descriptorType, 6 | 7 | 8 | 9)` from `exceptions::SIBLING_SELECTED`.
// The lint wants `6..=9`, and the table is a list of *enumerants* that happen to
// be adjacent in this registry — writing it as a range would make the generated
// code depend on an adjacency Khronos never promised, to satisfy a style rule.
#![allow(clippy::manual_range_patterns)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::borrow_deref_ref)]
#![allow(clippy::explicit_auto_deref)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::double_parens)]
#![allow(clippy::useless_transmute)]
#![allow(clippy::let_and_return)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::missing_transmute_annotations)]

use core::ffi::{c_char, c_void};
use std::sync::OnceLock;

pub mod local;

// The generated crate body: the C ABI types, the lowering, the writeback, the
// stubs, the `Icd` trait and the proc-address tables.
include!(concat!(env!("OUT_DIR"), "/sv_icd_entry.rs"));

// ---------------------------------------------------------------------------
// dispatchable objects
// ---------------------------------------------------------------------------

/// `ICD_LOADER_MAGIC` from `vk_icd.h`.
///
/// The loader writes its own dispatch-table pointer over the first word of every
/// dispatchable object an ICD returns, and asserts this value is there first.
/// **The assertion is debug-only in a release loader**, so an object without it
/// is not an error message — it is the loader dereferencing whatever we happened
/// to put there (design.md §6.4, R17).
pub const ICD_LOADER_MAGIC: usize = 0x01CD_C0DE;

/// Our own marker, in the word after the loader's.
///
/// The loader overwrites `loader_data`, so it cannot be used to recognise our
/// objects. This can: [`dispatchable_id`] checks it before believing the `id`
/// beside it, which turns "the application passed a handle from another ICD"
/// from a wild read into a zero.
pub const SV_OBJECT_MAGIC: u64 = 0x5356_4F42_4A45_4331; // "SVOBJEC1"

/// The prefix of every dispatchable object `sv-icd` hands out.
///
/// # Layout is the contract
///
/// `loader_data` **must** be first and **must** be pointer-sized: the loader
/// stores its dispatch table there and every layer above us loads it from
/// offset zero. That is why this is `#[repr(C)]`, why the object is never an
/// `Arc` (whose allocation begins with two refcounts) and never a `Box<dyn
/// Trait>` (a fat pointer with no such prefix). Kazan's
/// `struct DispatchableType<T> { loader_dispatch_ptr: usize, value: T }`,
/// copied exactly, with two words added after it for our own use.
#[repr(C)]
pub struct DispatchableHeader {
    /// Written by us with [`ICD_LOADER_MAGIC`], then **overwritten by the
    /// loader** with its dispatch table pointer. Those eight bytes are gone.
    pub loader_data: *mut c_void,
    /// [`SV_OBJECT_MAGIC`].
    pub magic: u64,
    /// The wire object id (design.md §6.4, R3). This is what crosses; the
    /// pointer never does.
    pub id: u64,
    /// Which kind of object this is, for diagnostics.
    pub kind: u32,
    pub _reserved: u32,
}

impl DispatchableHeader {
    pub fn new(id: u64, kind: u32) -> DispatchableHeader {
        DispatchableHeader {
            // `without_provenance_mut` and not `as *mut c_void`: this word is a
            // magic *number* that the loader compares and then overwrites with
            // its own pointer. It is never dereferenced by us, and spelling it
            // as an exposed cast tells the compiler — and Miri — the opposite of
            // what is true.
            loader_data: core::ptr::without_provenance_mut(ICD_LOADER_MAGIC),
            magic: SV_OBJECT_MAGIC,
            id,
            kind,
            _reserved: 0,
        }
    }
}

/// A dispatchable object: the loader's word, ours, then the object's own state.
#[repr(C)]
pub struct Dispatchable<T> {
    pub header: DispatchableHeader,
    pub value: T,
}

impl<T> Dispatchable<T> {
    /// Allocate one and leak it as a raw handle.
    ///
    /// Leaked on purpose: the application owns the handle's lifetime and gives
    /// it back at `vkDestroy*`, which is where [`Dispatchable::reclaim`] runs.
    pub fn new(id: u64, kind: u32, value: T) -> *mut Dispatchable<T> {
        Box::into_raw(Box::new(Dispatchable { header: DispatchableHeader::new(id, kind), value }))
    }

    /// # Safety
    /// `p` came from [`Dispatchable::new`] with the same `T` and has not been
    /// reclaimed.
    pub unsafe fn reclaim(p: *mut Dispatchable<T>) -> Option<Box<Dispatchable<T>>> {
        if p.is_null() { None } else { Some(unsafe { Box::from_raw(p) }) }
    }

    /// # Safety
    /// As [`Dispatchable::reclaim`], without taking ownership.
    pub unsafe fn value<'a>(p: *mut Dispatchable<T>) -> Option<&'a T> {
        if p.is_null() { None } else { Some(unsafe { &(*p).value }) }
    }
}

/// The wire id behind a dispatchable handle, or 0 for `VK_NULL_HANDLE`.
///
/// Called by every generated lowering that meets a dispatchable parameter, and
/// it is the third of the three invariants in the generator's header: what a
/// dispatchable handle points at is always one of our own allocations, so the
/// read is of memory we made.
///
/// # Safety
/// `p` is null, or a handle this ICD returned and the application has not yet
/// destroyed.
pub unsafe fn dispatchable_id(p: *const c_void) -> u64 {
    if p.is_null() {
        return 0;
    }
    let h = p as *const DispatchableHeader;
    // SAFETY: the caller's contract. The magic check is not a substitute for it
    // — a freed object may still read as ours — but it does catch the two
    // realistic mistakes: a handle from a different ICD, and a non-dispatchable
    // handle passed where a dispatchable one belongs.
    unsafe {
        if (*h).magic != SV_OBJECT_MAGIC {
            return 0;
        }
        (*h).id
    }
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Why a call did not reach the wire, or did not come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowerError {
    /// The command has an entry point but no implementation. Carries the
    /// command name, so a `VK_LOADER_DEBUG=all` run says which.
    NotImplemented(&'static str),
    /// A required pointer was null, or an array's shape did not fit.
    Shape(&'static str),
    /// A `pNext` chain longer than [`MAX_PNEXT_LINKS`], which in practice means
    /// a cycle.
    ChainTooLong,
    /// The transport is not connected.
    NotConnected,
    OutOfHostMemory,
    OutOfDeviceMemory,
    DeviceLost,
    /// A specific `VkResult` chosen by the implementation.
    Result(i32),
    /// The wire itself failed: a truncated reply, a length disagreement, a
    /// command the GPU side does not implement. Kept as a `WireError` rather
    /// than flattened, because R13's whole point is that the *first* mismatch is
    /// reportable with its own opcode.
    Wire(sv_wire::WireError),
}

/// A wire failure, as a `LowerError`.
///
/// One function so that every one of the ~239 generated forwarders spells the
/// conversion identically. A `From` impl would be tidier and is deliberately not
/// used: `?` on a `WireError` inside a generated body would then convert
/// silently, and this crate's rule is that a generated block does nothing the
/// generator did not write out.
pub fn wire_failed(e: sv_wire::WireError) -> LowerError {
    LowerError::Wire(e)
}

/// Which ring a forwarded command belongs on (design.md §4.4).
///
/// The generated forwarders pass one of these with every packet; the transport
/// decides what a ring is. Note that this is a *class*, not a ring id: the app
/// side has one instance ring but a device ring per `VkDevice` and a queue ring
/// per `VkQueue`, and the id inside the command's own arguments is what picks
/// between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingClass {
    /// Everything before a device exists.
    Instance,
    /// Object creation and destruction, descriptor updates, command recording.
    Device,
    /// `vkQueueSubmit`, `vkQueueWaitIdle`, present.
    Queue,
}

/// Where a forwarded command goes.
///
/// design.md called this seam "`state::Transport`" and put it in `sv-icd`; it
/// belongs here instead, because the *generated* forwarders have to name it and
/// they cannot depend on the crate that depends on them. The signature is
/// `(ring, opcode, bytes) -> bytes` and nothing more: the typing has already
/// happened in `sv-proto`, and a transport that knew about Vulkan would be a
/// transport with a reason to be regenerated.
pub trait Transport: Send + Sync {
    /// Send a command and wait for its reply.
    fn call(
        &self,
        ring: RingClass,
        opcode: u32,
        command: &[u8],
        reply: &mut Vec<u8>,
    ) -> IcdResult<()>;

    /// Send a command that generates no reply (design.md §4.4, D15).
    fn send(&self, ring: RingClass, opcode: u32, command: &[u8]) -> IcdResult<()>;

    /// Append a recorded `vkCmd*` to `command_buffer`'s blob (design.md §6.5).
    ///
    /// > Each `VkCommandBuffer` owns an arena block; recording encodes straight
    /// > into it; `vkEndCommandBuffer` publishes `(offset, len)`.
    ///
    /// The default forwards to [`Transport::send`] on the device ring, which is
    /// what every transport did before blobs existed and is exactly right for
    /// one that has no arena — `sv-loopback` and the test doubles. A transport
    /// that overrides this owes [`Transport::publish_blob`] as well: the two are
    /// the two halves of one mechanism and overriding one without the other
    /// records commands nobody ever executes.
    ///
    /// **`opcode` is passed unchanged and the ordering is the caller's.** A
    /// blob is append-only and the sub-decoder replays it in order, which is
    /// what makes this safe under §4.4's rule that nothing may reorder `vkCmd*`
    /// within a command buffer.
    fn record(&self, command_buffer: u64, opcode: u32, command: &[u8]) -> IcdResult<()> {
        let _ = command_buffer;
        self.send(RingClass::Device, opcode, command)
    }

    /// Hand `command_buffer`'s accumulated blob to the GPU side and reset it.
    ///
    /// Called from `vkEndCommandBuffer`. The default does nothing, which pairs
    /// with the default [`Transport::record`] having already sent every command
    /// individually.
    fn publish_blob(&self, command_buffer: u64) -> IcdResult<()> {
        let _ = command_buffer;
        Ok(())
    }

    /// Forget `command_buffer`'s blob without executing it.
    ///
    /// `vkResetCommandBuffer`, `vkResetCommandPool` and `vkFreeCommandBuffers`.
    fn discard_blob(&self, command_buffer: u64) {
        let _ = command_buffer;
    }
}

pub type LowerResult<T> = Result<T, LowerError>;
/// What an [`Icd`] method returns.
pub type IcdResult<T> = Result<T, LowerError>;

/// A chain this long is a cycle, not a chain.
///
/// The specification puts no bound on `pNext` length, so this is our bound and
/// not Vulkan's. It exists because the alternative to a bound is a hang inside
/// an application's own call, which is the least debuggable failure available.
pub const MAX_PNEXT_LINKS: usize = 256;

/// Map a lowering failure onto a `VkResult`.
pub fn lower_failed(e: LowerError) -> VkResult {
    report_failure(e);
    match e {
        LowerError::OutOfHostMemory => VkResult::VK_ERROR_OUT_OF_HOST_MEMORY,
        LowerError::OutOfDeviceMemory => VkResult::VK_ERROR_OUT_OF_DEVICE_MEMORY,
        LowerError::DeviceLost => VkResult::VK_ERROR_DEVICE_LOST,
        LowerError::Result(r) => VkResult(r),
        // `VK_ERROR_INITIALIZATION_FAILED` rather than `VK_ERROR_UNKNOWN`
        // because it is a result almost every command is allowed to return,
        // and an application that handles errors at all handles it.
        _ => VkResult::VK_ERROR_INITIALIZATION_FAILED,
    }
}

/// Say what went wrong, on the channel the loader's own diagnostics use.
///
/// Silent unless `VK_LOADER_DEBUG` or `SV_DEBUG` is set, because an ICD that
/// writes to stderr in the ordinary case is an ICD people uninstall.
pub fn report_failure(e: LowerError) {
    if debug_enabled() {
        eprintln!("sv-icd: {e:?}");
    }
}

/// Say what the transport said, since [`LowerError`] cannot carry it.
///
/// `LowerError` is `Copy` and stringless by design — it is returned from every
/// generated lowering, so it has to be free — which means a transport error's
/// own message has nowhere to go. It goes here.
pub fn report_transport_failure(e: &dyn core::fmt::Display) {
    if debug_enabled() {
        eprintln!("sv-icd: transport: {e}");
    }
}

/// Say something that must be said whether or not anyone asked for debugging.
///
/// The reserved use is **data this ICD is about to discard**. Everything else
/// belongs behind `SV_DEBUG`, because an ICD that chatters is an ICD people
/// silence; but a descriptor write that does not arrive produces no error, no
/// `VkResult` and no validation message, and the next thing that happens is the
/// GPU reading uninitialised memory. `expand_descriptor_template` dropped 64
/// inline-uniform-block writes per CTS test for exactly as long as its
/// complaint was gated (design.md §0.16), and the cost was a device loss whose
/// diagnosis started three layers away.
///
/// Once per distinct message, so a per-descriptor fault cannot become the
/// output.
pub fn report_always(e: &dyn core::fmt::Display) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SAID: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let msg = e.to_string();
    let mut g = SAID.lock().unwrap_or_else(|p| p.into_inner());
    if g.get_or_insert_with(HashSet::new).insert(msg.clone()) {
        eprintln!("sv-icd: {msg}");
    }
}

/// Note that an unimplemented entry point was called.
pub fn not_implemented(command: &'static str) {
    if debug_enabled() {
        eprintln!("sv-icd: {command} is not implemented in this build");
    }
}

fn debug_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("SV_DEBUG").is_some() || std::env::var_os("VK_LOADER_DEBUG").is_some()
    })
}

// ---------------------------------------------------------------------------
// the implementation, and the one global
// ---------------------------------------------------------------------------

/// The half of the implementation the generator cannot describe.
///
/// Every method here corresponds to an entry point in
/// `sv_codegen::emit::icd_entry::HAND_WRITTEN`, and they are hand-declared for
/// the reason that list gives: a dispatchable handle is a pointer to an
/// allocation only the ICD can make, so no fold over a wire reply produces one.
/// The methods therefore speak in **wire object ids**, and [`local`] does the
/// pointer work on both sides of them.
///
/// Everything here has a default that refuses, so `sv-icd` can implement a
/// subset and the rest still fails cleanly (design.md §13 step 5 is about the
/// loader accepting us, not about a complete driver).
pub trait IcdLocal {
    /// The Vulkan version this driver implements.
    ///
    /// Ship a real one. design.md §8: if the manifest says < 1.1, or
    /// `vkEnumerateInstanceVersion` is missing, or it returns < 1.1, or it
    /// returns anything but `VK_SUCCESS`, the loader **demotes us to a 1.0
    /// driver and rewrites every `VkApplicationInfo::apiVersion` we receive**.
    fn api_version(&self) -> u32 {
        // VK_MAKE_API_VERSION(0, 1, 3, 0)
        (1 << 22) | (3 << 12)
    }

    /// The instance extensions we advertise: `(name, spec version)`.
    ///
    /// Curated, never mirrored from the GPU side. The loader has
    /// `MAX_NUM_UNKNOWN_EXTS = 250` hand-generated trampolines and its
    /// known-extension list depends on its own header version, so reflecting
    /// everything the peer reports is how an ICD becomes unloadable on an older
    /// loader (design.md §8, R16).
    fn instance_extensions(&self) -> &[(&'static str, u32)] {
        &[]
    }

    /// The device extensions we advertise, for the same reason.
    fn device_extensions(&self) -> &[(&'static str, u32)] {
        &[]
    }

    /// A fresh wire object id.
    ///
    /// design.md §6.4's counter. Every generated [`Icd`] default that forwards a
    /// command with an output handle calls this *before* encoding, because the
    /// id is what the command carries — the application's `*pBuffer` holds
    /// nothing until we have returned.
    ///
    /// The default is a process-wide counter so that even an implementation
    /// which overrides nothing produces unique, non-zero ids. `sv-icd` overrides
    /// it with its own `State::mint`, so that there is exactly one id space and
    /// its tables can be keyed by it.
    fn mint_id(&self) -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// The transport to the GPU side, if this build has one connected.
    ///
    /// This is the hinge of the whole app side. Every generated [`Icd`] method
    /// consults it: with a transport the command is encoded and sent, without
    /// one it refuses by name. An implementation therefore does not have to
    /// write 239 forwarders — it writes the handful design.md §8 and §10 say are
    /// answered app-side, and the rest cross by default.
    ///
    /// `None` is a legitimate steady state and not only a startup window: a
    /// `vulkaninfo` run against a driver with no GPU side behind it still
    /// enumerates, because the commands that answer it are the overridden ones.
    fn transport(&self) -> Option<&dyn Transport> {
        None
    }

    /// `vkGetInstanceProcAddr`, which cannot be forwarded because its answer is
    /// a pointer (design.md §6.1's exclusion list).
    ///
    /// `instance` is `None` for the loader's NULL-instance global lookup.
    fn instance_proc_addr(&self, instance: Option<u64>, name: &str) -> Option<*const c_void> {
        let _ = (instance, name);
        None
    }

    /// `vkGetDeviceProcAddr`.
    fn device_proc_addr(&self, device: u64, name: &str) -> Option<*const c_void> {
        let _ = (device, name);
        None
    }

    fn create_instance(&self, info: &sv_proto::VkInstanceCreateInfo) -> IcdResult<u64> {
        let _ = info;
        Err(LowerError::NotImplemented("vkCreateInstance"))
    }
    fn destroy_instance(&self, instance: u64) {
        let _ = instance;
    }
    fn enumerate_physical_devices(&self, instance: u64) -> IcdResult<Vec<u64>> {
        let _ = instance;
        Err(LowerError::NotImplemented("vkEnumeratePhysicalDevices"))
    }
    fn create_device(
        &self,
        physical_device: u64,
        info: &sv_proto::VkDeviceCreateInfo,
    ) -> IcdResult<u64> {
        let _ = (physical_device, info);
        Err(LowerError::NotImplemented("vkCreateDevice"))
    }
    fn destroy_device(&self, device: u64) {
        let _ = device;
    }
    fn get_device_queue(&self, device: u64, family: u32, index: u32) -> IcdResult<u64> {
        let _ = (device, family, index);
        Err(LowerError::NotImplemented("vkGetDeviceQueue"))
    }
    fn allocate_command_buffers(
        &self,
        device: u64,
        info: &sv_proto::VkCommandBufferAllocateInfo,
    ) -> IcdResult<Vec<u64>> {
        let _ = (device, info);
        Err(LowerError::NotImplemented("vkAllocateCommandBuffers"))
    }
    fn free_command_buffers(&self, device: u64, pool: u64, buffers: &[u64]) {
        let _ = (device, pool, buffers);
    }

    /// `vkMapMemory`, and the whole reason the project exists.
    ///
    /// design.md §5.3: the answer is `arena_base + slab_base + alloc_offset +
    /// offset`, a real pointer into real shared pages, with **no round trip and
    /// no copy**. The arena is mapped for the process's lifetime, so this and
    /// `vkUnmapMemory` are pure app-side bookkeeping.
    fn map_memory(
        &self,
        device: u64,
        memory: u64,
        offset: u64,
        size: u64,
    ) -> IcdResult<*mut c_void> {
        let _ = (device, memory, offset, size);
        Err(LowerError::NotImplemented("vkMapMemory"))
    }
    fn unmap_memory(&self, device: u64, memory: u64) {
        let _ = (device, memory);
    }

    // -- presentation (design.md §10) ---------------------------------------
    //
    // Eleven methods, none of which has a wire opcode. The whole of
    // `VK_KHR_swapchain` is implemented on this side, out of ordinary forwarded
    // commands: a swapchain image is a `vkCreateImage` the ICD made on the
    // application's behalf, and `vkQueuePresentKHR` is a `vkQueueSubmit` of a
    // command buffer the ICD recorded. The GPU side is not told that any of it
    // is a swapchain and does not have to be.
    //
    // They speak in [`PresentTarget`], [`SurfaceCaps`] and [`SwapchainDesc`]
    // rather than in `VkIcdSurface*` and `VkSwapchainCreateInfoKHR*`, which is
    // the same division of labour as everything above: the pointer stops at
    // `local.rs`.

    /// `vkCreateXxxSurfaceKHR`, for every `Xxx` this ICD answers for.
    ///
    /// **The ICD owns its surfaces**, which design.md §8 says it does not. See
    /// `sv_codegen::emit::icd_entry::EXTRA_LOCAL`: at loader interface ≥ 3 the
    /// loader creates the driver's surface lazily by calling this, and a driver
    /// that does not export it is silently told it cannot present.
    fn create_surface(&self, target: PresentTarget) -> IcdResult<u64> {
        let _ = target;
        Err(LowerError::NotImplemented("vkCreateXxxSurfaceKHR"))
    }

    fn destroy_surface(&self, surface: u64) {
        let _ = surface;
    }

    /// What a `VkSurfaceKHR` of ours names.
    fn surface_target(&self, surface: u64) -> Option<PresentTarget> {
        let _ = surface;
        None
    }

    /// `vkGetPhysicalDeviceSurfaceCapabilitiesKHR`.
    fn surface_capabilities(&self, target: PresentTarget) -> IcdResult<SurfaceCaps> {
        let _ = target;
        Err(LowerError::NotImplemented("vkGetPhysicalDeviceSurfaceCapabilitiesKHR"))
    }

    /// `vkGetPhysicalDeviceSurfaceFormatsKHR`, as `(VkFormat, VkColorSpaceKHR)`.
    fn surface_formats(&self, target: PresentTarget) -> Vec<(i32, i32)> {
        let _ = target;
        Vec::new()
    }

    /// `vkGetPhysicalDeviceSurfacePresentModesKHR`, as `VkPresentModeKHR`.
    fn surface_present_modes(&self, target: PresentTarget) -> Vec<i32> {
        let _ = target;
        Vec::new()
    }

    /// `vkCreateSwapchainKHR`. Returns the swapchain's app-side id.
    fn create_swapchain(&self, device: u64, desc: &SwapchainDesc) -> IcdResult<u64> {
        let _ = (device, desc);
        Err(LowerError::NotImplemented("vkCreateSwapchainKHR"))
    }

    fn destroy_swapchain(&self, device: u64, swapchain: u64) {
        let _ = (device, swapchain);
    }

    /// `vkGetSwapchainImagesKHR`: the `VkImage` ids, in acquire order.
    fn swapchain_images(&self, swapchain: u64) -> IcdResult<Vec<u64>> {
        let _ = swapchain;
        Err(LowerError::NotImplemented("vkGetSwapchainImagesKHR"))
    }

    /// `vkAcquireNextImageKHR`. Returns the image index.
    ///
    /// `semaphore` and `fence` are zero for "not requested"; if either is given
    /// it **must** be signalled before the application may use the image, and
    /// the only way to signal an object that lives on the GPU side is to submit
    /// something that signals it.
    fn acquire_next_image(
        &self,
        device: u64,
        swapchain: u64,
        timeout: u64,
        semaphore: u64,
        fence: u64,
    ) -> IcdResult<u32> {
        let _ = (device, swapchain, timeout, semaphore, fence);
        Err(LowerError::NotImplemented("vkAcquireNextImageKHR"))
    }

    /// `vkQueuePresentKHR`, for one `(swapchain, image index)` pair.
    ///
    /// `waits` are the application's wait semaphores and belong to the *first*
    /// pair only, exactly as the specification's own "the presentation engine
    /// waits once" wording implies.
    fn queue_present(
        &self,
        queue: u64,
        swapchain: u64,
        image_index: u32,
        waits: &[u64],
    ) -> IcdResult<()> {
        let _ = (queue, swapchain, image_index, waits);
        Err(LowerError::NotImplemented("vkQueuePresentKHR"))
    }
}

/// Where a presented frame goes, once the app side holds its pixels.
///
/// **This crate never dereferences any of it**, and that is the whole of the
/// rule it keeps. It reads the fields out of the loader's `VkIcdSurface`,
/// carries them as integers, and hands them on; what the display target does
/// with them is `sv_icd::present`'s business, and it is that module — not this
/// one — that `dlopen`s a window-system library.
///
/// The X11 variant is an id and could not be anything else: X window ids are
/// global to the display, so `sv_icd::present::x11` opens a connection of its
/// own. The Wayland variant *is* two pointers, and cannot be an id, because
/// **a `wl_surface` is a proxy private to one connection**. There is no name for
/// it that a second connection could resolve, so the only way to put a frame on
/// the application's surface is to speak on the application's own display —
/// which is what `sv_icd::present::wayland` does, on an event queue of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentTarget {
    /// `VK_EXT_headless_surface`, which is what Wine maps `VK_KHR_win32_surface`
    /// onto when there is no display (design.md §0.5). The frame stops in the
    /// arena; `SV_PRESENT_DUMP` is how a harness reads it out.
    Headless,
    /// An X11 window, by XID.
    X11 { window: u64 },
    /// A `wl_display*` and a `wl_surface*`, as `usize` so that this stays
    /// `Copy`, `Send` and free of raw pointers in a safe signature. Both belong
    /// to the application and are valid for as long as the surface is.
    Wayland { display: usize, surface: usize },
}

/// The parts of `VkSurfaceCapabilitiesKHR` that are not constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceCaps {
    /// `currentExtent`. `0xFFFF_FFFF` in either field means "the surface has no
    /// size of its own and the swapchain decides", which is the honest answer
    /// for a headless surface.
    pub current_width: u32,
    pub current_height: u32,
}

/// `VkSurfaceCapabilitiesKHR::minImageCount`.
///
/// Two, because FIFO needs one image on screen and one to draw into, and
/// because a swapchain image here costs a GPU-side allocation plus an arena
/// buffer of the same size — so the floor should be the real floor.
pub const MIN_SWAPCHAIN_IMAGES: u32 = 2;

/// `VkSurfaceCapabilitiesKHR::maxImageCount`.
pub const MAX_SWAPCHAIN_IMAGES: u32 = 8;

/// `VkSwapchainCreateInfoKHR`, reduced to what the app side acts on.
#[derive(Clone, Copy, Debug)]
pub struct SwapchainDesc {
    /// Resolved from `VkSwapchainCreateInfoKHR::surface` in `local.rs`, so that
    /// the loader's `VkIcdSurface*` never reaches a safe method.
    pub target: PresentTarget,
    pub min_image_count: u32,
    pub format: i32,
    pub color_space: i32,
    pub width: u32,
    pub height: u32,
    pub image_usage: u32,
    pub present_mode: i32,
    pub old_swapchain: u64,
}

/// The implementation of last resort: every method is the trait's default.
///
/// Not a placeholder to be removed. It is what makes a call that arrives before
/// `vkCreateInstance`, or after a failed one, return an error instead of finding
/// a null pointer where the implementation should be.
pub struct NullIcd;
impl IcdLocal for NullIcd {}
impl Icd for NullIcd {}

/// A `OnceLock` and not an `AtomicPtr` to a leaked `Box`.
///
/// The first version leaked the two-word fat pointer on purpose — once per
/// process, never freed — and Miri's leak check flagged it, correctly. A
/// `OnceLock<&'static dyn Icd>` stores both words inline, so there is nothing to
/// leak and `icd()` is still one acquire load and a branch on the path of every
/// Vulkan call.
static ICD: OnceLock<&'static dyn Icd> = OnceLock::new();

/// Register the implementation. Called once, by `sv-icd`.
///
/// A second call with a different implementation is ignored rather than
/// panicking: an ICD is loaded once per process, so the only way to reach it is
/// a test, and a test that has already installed one does not want its process
/// aborted.
pub fn set_icd(icd: &'static dyn Icd) {
    let _ = ICD.set(icd);
}

/// The registered implementation, or [`NullIcd`].
pub fn icd() -> &'static dyn Icd {
    static NULL: NullIcd = NullIcd;
    match ICD.get() {
        Some(i) => *i,
        None => &NULL,
    }
}

// ---------------------------------------------------------------------------
// small helpers the generated code calls
// ---------------------------------------------------------------------------

/// The bytes of a NUL-terminated C string, terminator excluded.
///
/// # Safety
/// `p` is null or points at a NUL-terminated string that outlives the call.
/// There is no length to check against — that is what "NUL-terminated" means,
/// and it is the one place invariant 2 of the generator's header cannot help.
pub unsafe fn c_string_bytes(p: *const c_char) -> Vec<u8> {
    if p.is_null() {
        return Vec::new();
    }
    let mut n = 0usize;
    // SAFETY: the caller's contract.
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
        core::slice::from_raw_parts(p as *const u8, n).to_vec()
    }
}

/// One row of a generated proc-address table.
///
/// A named type rather than a tuple because a `static` holding a raw pointer
/// must be `Sync`, and a tuple cannot be given that promise. The promise is
/// sound: `pfn` is always the address of an `extern "system"` function in this
/// crate's own text, which is immutable, never freed and identical in every
/// thread.
#[derive(Clone, Copy)]
pub struct ProcEntry {
    pub name: &'static str,
    pub pfn: *const c_void,
}

// SAFETY: see above — the pointer is a function address in `.text`.
unsafe impl Sync for ProcEntry {}
// SAFETY: as above.
unsafe impl Send for ProcEntry {}

/// Look a name up in one of the generated tables.
pub fn lookup(table: &[ProcEntry], name: &str) -> Option<*const c_void> {
    table.iter().find(|e| e.name == name).map(|e| e.pfn)
}

/// Reinterpret a table entry as the loader's `PFN_vkVoidFunction`.
///
/// The one transmute in the hand-written half, and it is the transmute the
/// loader's own headers describe: `vkGetInstanceProcAddr` is specified to return
/// a `PFN_vkVoidFunction` that the caller casts back to the real signature. The
/// two-step is because `Option<*const c_void>` is two words — a raw pointer has
/// no null niche — while `Option<fn()>` is one.
pub fn as_pfn(p: Option<*const c_void>) -> PFN_vkVoidFunction {
    let p = p?;
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` came from a `ProcEntry`, so it is the address of an
    // `extern "system"` function, and both types are one non-null word.
    Some(unsafe { core::mem::transmute::<*const c_void, unsafe extern "system" fn()>(p) })
}

/// The name of an entry point, from the `const char*` the loader hands us.
///
/// # Safety
/// `p` is null or a NUL-terminated string.
pub unsafe fn proc_name<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller's contract.
    let bytes = unsafe {
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
        }
        core::slice::from_raw_parts(p as *const u8, n)
    };
    core::str::from_utf8(bytes).ok()
}

/// Write a Rust string into a fixed C character array, NUL-terminated and
/// truncated rather than overflowing.
pub fn write_c_array(dst: &mut [c_char], s: &str) {
    let n = core::cmp::min(s.len(), dst.len().saturating_sub(1));
    for (i, b) in s.as_bytes()[..n].iter().enumerate() {
        dst[i] = *b as c_char;
    }
    if !dst.is_empty() {
        dst[n] = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loader reads the first pointer-sized word of a dispatchable object
    /// and nothing else about our layout. If this ever fails, every dispatchable
    /// handle this ICD returns is a fault inside the loader.
    #[test]
    fn the_loaders_word_is_first_and_pointer_sized() {
        assert_eq!(core::mem::offset_of!(DispatchableHeader, loader_data), 0);
        assert_eq!(
            core::mem::size_of::<*mut c_void>(),
            core::mem::size_of::<usize>(),
            "the loader's dispatch table pointer must fit the first word"
        );
        let d = Dispatchable::new(7, 1, 42u32);
        assert_eq!(core::mem::offset_of!(Dispatchable<u32>, header), 0);
        // SAFETY: just allocated.
        unsafe {
            assert_eq!(dispatchable_id(d as *const c_void), 7);
            // The loader overwrites the first word; the id must survive that.
            (*d).header.loader_data = core::ptr::without_provenance_mut(0xdead_beef);
            assert_eq!(dispatchable_id(d as *const c_void), 7);
            let _ = Dispatchable::reclaim(d);
        }
    }

    #[test]
    fn a_foreign_handle_is_not_believed() {
        let mut not_ours = [0u64; 8];
        let p = not_ours.as_mut_ptr() as *const c_void;
        // SAFETY: readable for the size of a header.
        assert_eq!(unsafe { dispatchable_id(p) }, 0);
        assert_eq!(unsafe { dispatchable_id(core::ptr::null()) }, 0);
    }

    /// The C ABI is not the wire, and these are the differences that would be
    /// silent if they were wrong.
    #[test]
    fn the_c_abi_is_not_the_wire() {
        assert_eq!(core::mem::size_of::<VkBool32>(), 4, "not Rust's one-byte bool");
        assert_eq!(core::mem::size_of::<VkInstance>(), core::mem::size_of::<*mut c_void>());
        assert_eq!(core::mem::size_of::<VkBuffer>(), 8);
        assert_eq!(core::mem::size_of::<VkResult>(), 4);
        // sType is a member here and is not one on the wire.
        assert_eq!(core::mem::offset_of!(VkInstanceCreateInfo, s_type), 0);
        assert_eq!(
            core::mem::offset_of!(VkInstanceCreateInfo, p_next),
            core::mem::size_of::<*const c_void>()
        );
    }

    /// Sizes of the structs whose layout an application depends on most.
    ///
    /// These are the numbers `vulkan_core.h` produces on the same target, and
    /// they are asserted rather than trusted for the reason design.md §7.3 gives
    /// after the sign-inversion bug: round-trip tests check that we agree with
    /// ourselves, and only an external number checks that we are right.
    #[test]
    fn struct_sizes_match_the_c_header() {
        // VkExtent3D: three uint32_t.
        assert_eq!(core::mem::size_of::<VkExtent3D>(), 12);
        // VkOffset2D: two int32_t.
        assert_eq!(core::mem::size_of::<VkOffset2D>(), 8);
        // VkMemoryType: VkMemoryPropertyFlags + uint32_t.
        assert_eq!(core::mem::size_of::<VkMemoryType>(), 8);
        // VkMemoryHeap: VkDeviceSize + VkMemoryHeapFlags, padded to 16.
        assert_eq!(core::mem::size_of::<VkMemoryHeap>(), 16);
        // VkPhysicalDeviceMemoryProperties: 4 + 32*8 + 4(+4 pad) + 16*16 = 520.
        assert_eq!(core::mem::size_of::<VkPhysicalDeviceMemoryProperties>(), 520);
        assert_eq!(core::mem::align_of::<VkPhysicalDeviceMemoryProperties>(), 8);
    }

    #[test]
    fn the_tables_are_complete_and_disjoint_where_they_must_be() {
        assert!(lookup(GLOBAL_COMMANDS, "vkCreateInstance").is_some());
        assert!(lookup(GLOBAL_COMMANDS, "vkEnumerateInstanceVersion").is_some());
        assert!(lookup(GLOBAL_COMMANDS, "vkEnumerateInstanceExtensionProperties").is_some());
        // A device command must not be answerable with a NULL instance.
        assert!(lookup(GLOBAL_COMMANDS, "vkCreateDevice").is_none());
        assert!(lookup(GLOBAL_COMMANDS, "vkCmdDraw").is_none());
        assert!(lookup(INSTANCE_COMMANDS, "vkCreateDevice").is_some());
        assert!(lookup(DEVICE_COMMANDS, "vkCmdDraw").is_some());
        // `vkGetDeviceProcAddr` answers only for device-level commands.
        assert!(lookup(DEVICE_COMMANDS, "vkCreateDevice").is_none());
        // The physical-device table is "every command whose first parameter is a
        // `VkPhysicalDevice`", which is the definition
        // `vk_icdGetPhysicalDeviceProcAddr` is specified against. The loader only
        // ever asks it about entry points it does not itself know, so the
        // presence of `vkCreateDevice` — which does take a physical device
        // first — costs nothing and its absence would be a lie about the
        // predicate.
        assert!(lookup(PHYSICAL_DEVICE_COMMANDS, "vkGetPhysicalDeviceMemoryProperties").is_some());
        assert!(lookup(PHYSICAL_DEVICE_COMMANDS, "vkCreateDevice").is_some());
        assert!(lookup(PHYSICAL_DEVICE_COMMANDS, "vkCmdDraw").is_none());
        assert!(lookup(PHYSICAL_DEVICE_COMMANDS, "vkCreateInstance").is_none());
        assert!(lookup(PHYSICAL_DEVICE_COMMANDS, "vkDestroyInstance").is_none());
    }

    /// An extension's promoted alias must resolve too: an application that
    /// enabled `VK_KHR_maintenance1` asks for `vkTrimCommandPoolKHR`, and the
    /// core name is not what it will look up.
    #[test]
    fn aliases_are_answerable() {
        assert!(lookup(DEVICE_COMMANDS, "vkTrimCommandPool").is_some());
        assert!(lookup(DEVICE_COMMANDS, "vkTrimCommandPoolKHR").is_some());
    }

    /// What is stubbed, asserted rather than described.
    ///
    /// The brief for this stage asks for "a documented list of what is stubbed",
    /// and a list in prose drifts from the code within a week. This *is* the
    /// list: `NOT_IMPLEMENTED` is generated from what the generator refused, and
    /// this test pins it. A new refusal fails here with its name, and a refusal
    /// that goes away fails here too — which is the direction that matters, since
    /// nothing else would notice an entry point quietly becoming real.
    #[test]
    fn the_stubbed_entry_points_are_exactly_these() {
        let expected: &[&str] = &[
            // The debug-utils messenger, whose GPU-side half is the reverse
            // ring (design.md §4.6) rather than a forwarded command.
            "vkCreateDebugUtilsMessengerEXT",
            "vkDestroyDebugUtilsMessengerEXT",
            "vkSubmitDebugUtilsMessageEXT",
            // The device-group half `VK_KHR_swapchain` absorbed when 1.1
            // promoted `VK_KHR_device_group`. We advertise one physical device
            // in one group, so there is nothing for these to say, and the GPU
            // side has no surface to ask about even if there were (§10).
            "vkAcquireNextImage2KHR",
            "vkGetDeviceGroupPresentCapabilitiesKHR",
            "vkGetDeviceGroupSurfacePresentModesKHR",
            "vkGetPhysicalDevicePresentRectanglesKHR",
            // Returns dispatchable objects, so it belongs with the hand-written
            // set and has not been written.
            "vkEnumeratePhysicalDeviceGroups",
            "vkGetDeviceQueue2",
            // design.md §0.3 finding 5: a command returning a value that is
            // neither `void` nor `VkResult` has no reply to carry it. Latent —
            // §9.2 does not advertise `bufferDeviceAddress` — and a hole in the
            // protocol rather than in this crate.
            "vkGetBufferDeviceAddress",
            "vkGetBufferOpaqueCaptureAddress",
            "vkGetDeviceMemoryOpaqueCaptureAddress",
        ];
        let mut got: Vec<&str> = NOT_IMPLEMENTED.to_vec();
        got.sort_unstable();
        let mut want: Vec<&str> = expected.to_vec();
        want.sort_unstable();
        assert_eq!(got, want);
        assert_eq!(IMPLEMENTED_COUNT + NOT_IMPLEMENTED.len(), COMMAND_COUNT);
    }

    /// Every entry point on the C surface is answerable from some table.
    ///
    /// A command with a stub and no table entry is a function nobody can reach,
    /// which is the same as not having written it — and it fails silently.
    #[test]
    fn every_command_is_in_exactly_one_of_the_three_lookup_tables() {
        let total = GLOBAL_COMMANDS.len() + INSTANCE_COMMANDS.len() + DEVICE_COMMANDS.len();
        // The physical-device table is a *subset* of the instance one, not a
        // fourth partition, so it does not count towards the total.
        assert!(total >= COMMAND_COUNT, "{total} table entries for {COMMAND_COUNT} commands");
        for e in PHYSICAL_DEVICE_COMMANDS {
            assert!(
                lookup(INSTANCE_COMMANDS, e.name).is_some(),
                "{} is physical-device-level but not instance-level",
                e.name
            );
        }
        // No name in two partitions.
        for e in DEVICE_COMMANDS {
            assert!(lookup(GLOBAL_COMMANDS, e.name).is_none(), "{} is in two tables", e.name);
            assert!(lookup(INSTANCE_COMMANDS, e.name).is_none(), "{} is in two tables", e.name);
        }
    }

    /// The hand-written list and the generated tables must agree, or a name in
    /// `HAND_WRITTEN` silently resolves to nothing.
    #[test]
    fn every_hand_written_entry_point_is_in_a_table() {
        for name in HAND_WRITTEN {
            let found = lookup(GLOBAL_COMMANDS, name).is_some()
                || lookup(INSTANCE_COMMANDS, name).is_some()
                || lookup(DEVICE_COMMANDS, name).is_some();
            assert!(found, "{name} is hand-written but in no table");
        }
    }

    /// The surface creators are reachable, and the loader will `abort()` if the
    /// presentation-support queries are not.
    ///
    /// Both halves of this are things the loader punishes silently or fatally
    /// rather than with a diagnostic, which is why they are asserted here rather
    /// than left to a run:
    ///
    /// - a missing `vkCreateXxxSurfaceKHR` makes `wsi_unwrap_icd_surface` fail
    ///   and every surface query answer "not supported" with no message
    ///   (`sv_codegen::emit::icd_entry::EXTRA_LOCAL`);
    /// - a missing `vkGetPhysicalDeviceXcbPresentationSupportKHR` makes the
    ///   loader's terminator call `abort()`.
    #[test]
    fn the_whole_wsi_family_is_reachable() {
        let must: &[&str] = &[
            "vkCreateHeadlessSurfaceEXT",
            "vkCreateXlibSurfaceKHR",
            "vkCreateXcbSurfaceKHR",
            "vkDestroySurfaceKHR",
            "vkGetPhysicalDeviceSurfaceSupportKHR",
            "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
            "vkGetPhysicalDeviceSurfaceFormatsKHR",
            "vkGetPhysicalDeviceSurfacePresentModesKHR",
            "vkCreateWaylandSurfaceKHR",
            "vkGetPhysicalDeviceXlibPresentationSupportKHR",
            "vkGetPhysicalDeviceXcbPresentationSupportKHR",
            "vkGetPhysicalDeviceWaylandPresentationSupportKHR",
            "vkCreateSwapchainKHR",
            "vkDestroySwapchainKHR",
            "vkGetSwapchainImagesKHR",
            "vkAcquireNextImageKHR",
            "vkQueuePresentKHR",
        ];
        for name in must {
            let found = lookup(INSTANCE_COMMANDS, name).is_some()
                || lookup(DEVICE_COMMANDS, name).is_some();
            assert!(found, "{name} must be exported once the WSI is advertised");
            assert!(
                !NOT_IMPLEMENTED.contains(name),
                "{name} is exported but refuses, which the loader cannot tell from working"
            );
        }
        for name in EXTRA_LOCAL {
            assert!(
                lookup(INSTANCE_COMMANDS, name).is_some(),
                "{name} is hand-written off the C surface and must still be in a table"
            );
        }
    }
}
