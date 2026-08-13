//! The entry points that cannot be generated.
//!
//! Seventeen of the C surface's 250, and each is here for one of three reasons
//! stated in `sv_codegen::emit::icd_entry::HAND_WRITTEN`:
//!
//! 1. **It mints or destroys a dispatchable object.** What the application gets
//!    back from `vkCreateInstance` is a pointer to an allocation whose first
//!    word the loader owns (design.md §6.4). A fold over a wire reply cannot
//!    produce one: the reply carries a `u64` id, and turning an id into a live
//!    object is bookkeeping, not marshalling. Vulkan has five dispatchable
//!    types, so the list is short and closed.
//! 2. **Its answer is a pointer.** `vkGetInstanceProcAddr` and
//!    `vkGetDeviceProcAddr`; design.md §6.1's exclusion list.
//! 3. **It is answered app-side by design.** The extension enumerations, because
//!    we advertise a curated set rather than mirroring the peer's 148 (§8, R16);
//!    and `vkMapMemory`/`vkUnmapMemory`, because the arena is already mapped and
//!    the answer is arithmetic (§5.3).
//!
//! Everything here has the same shape as the generated stubs — check, lower,
//! call a safe method, write back — and the same three invariants apply.

use crate::*;
use core::ffi::{c_char, c_void};

/// Object kinds, for [`DispatchableHeader::kind`]. Diagnostics only.
pub const KIND_INSTANCE: u32 = 1;
pub const KIND_PHYSICAL_DEVICE: u32 = 2;
pub const KIND_DEVICE: u32 = 3;
pub const KIND_QUEUE: u32 = 4;
pub const KIND_COMMAND_BUFFER: u32 = 5;

/// A dispatchable object with no state of its own beyond its id.
///
/// Every dispatchable object this ICD returns is one of these. The state lives
/// in `sv-icd`'s tables, keyed by the id, because the id is what crosses and
/// because a table lookup is checkable where a pointer chase is not (R3).
pub type Object = Dispatchable<()>;

fn new_object(id: u64, kind: u32) -> *mut Object {
    Dispatchable::new(id, kind, ())
}

// ---------------------------------------------------------------------------
// count-then-fill, once
// ---------------------------------------------------------------------------

/// The `vkEnumerate*` idiom, written once rather than four times.
///
/// # Safety
/// `count` is null or writable; `out` is null or writable for `*count`
/// elements.
unsafe fn fill<T: Copy>(items: &[T], count: *mut u32, out: *mut T) -> VkResult {
    if count.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: the caller's contract.
    unsafe {
        if out.is_null() {
            *count = items.len() as u32;
            return VkResult::VK_SUCCESS;
        }
        let n = core::cmp::min(*count as usize, items.len());
        for (i, item) in items.iter().take(n).enumerate() {
            *out.add(i) = *item;
        }
        *count = n as u32;
        if n < items.len() { VkResult::VK_INCOMPLETE } else { VkResult::VK_SUCCESS }
    }
}

fn extension_properties(list: &[(&'static str, u32)]) -> Vec<VkExtensionProperties> {
    list.iter()
        .map(|(name, version)| {
            let mut p = VkExtensionProperties::default();
            write_c_array(&mut p.extension_name, name);
            p.spec_version = *version;
            p
        })
        .collect()
}

// ---------------------------------------------------------------------------
// proc address
// ---------------------------------------------------------------------------

/// `vkGetInstanceProcAddr`.
///
/// The rule that catches people out: with a **NULL instance** only the global
/// commands may be answered, and with a non-NULL one everything else may be —
/// including device-level commands, which come back as loader trampolines.
/// Answering a device command for a NULL instance is how an ICD gets a call with
/// no dispatch table behind it.
pub unsafe extern "system" fn sv_vk_get_instance_proc_addr(
    instance: VkInstance,
    p_name: *const c_char,
) -> PFN_vkVoidFunction {
    // SAFETY: the loader passes a NUL-terminated string.
    let name = unsafe { proc_name(p_name) }?;
    let id = if instance.is_null() {
        None
    } else {
        // SAFETY: a handle this ICD returned.
        Some(unsafe { dispatchable_id(instance as *const c_void) })
    };
    let p = icd().instance_proc_addr(id, name);
    // SAFETY: every pointer in the generated tables is an `extern "system"` fn
    // of this crate's own, and `PFN_vkVoidFunction` is the type the loader
    // immediately casts back to the right signature.
    as_pfn(p)
}

/// `vkGetDeviceProcAddr`. Device-level commands only.
pub unsafe extern "system" fn sv_vk_get_device_proc_addr(
    device: VkDevice,
    p_name: *const c_char,
) -> PFN_vkVoidFunction {
    // SAFETY: as above.
    let name = unsafe { proc_name(p_name) }?;
    let id = unsafe { dispatchable_id(device as *const c_void) };
    let p = icd().device_proc_addr(id, name);
    // SAFETY: as above.
    as_pfn(p)
}

// ---------------------------------------------------------------------------
// global queries
// ---------------------------------------------------------------------------

pub unsafe extern "system" fn sv_vk_enumerate_instance_version(
    p_api_version: *mut u32,
) -> VkResult {
    if p_api_version.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: the loader passes writable storage for one `uint32_t`.
    unsafe { *p_api_version = icd().api_version() };
    VkResult::VK_SUCCESS
}

pub unsafe extern "system" fn sv_vk_enumerate_instance_extension_properties(
    p_layer_name: *const c_char,
    p_property_count: *mut u32,
    p_properties: *mut VkExtensionProperties,
) -> VkResult {
    // A driver reports no extensions for any layer but its own (which is none).
    if !p_layer_name.is_null() {
        // SAFETY: writable if non-null.
        return unsafe { fill::<VkExtensionProperties>(&[], p_property_count, p_properties) };
    }
    let props = extension_properties(icd().instance_extensions());
    // SAFETY: the caller's storage, bounded by `*p_property_count`.
    unsafe { fill(&props, p_property_count, p_properties) }
}

/// An ICD has no layers, and saying so is not a stub.
pub unsafe extern "system" fn sv_vk_enumerate_instance_layer_properties(
    p_property_count: *mut u32,
    p_properties: *mut VkLayerProperties,
) -> VkResult {
    // SAFETY: the caller's storage.
    unsafe { fill::<VkLayerProperties>(&[], p_property_count, p_properties) }
}

pub unsafe extern "system" fn sv_vk_enumerate_device_extension_properties(
    _physical_device: VkPhysicalDevice,
    p_layer_name: *const c_char,
    p_property_count: *mut u32,
    p_properties: *mut VkExtensionProperties,
) -> VkResult {
    if !p_layer_name.is_null() {
        // SAFETY: writable if non-null.
        return unsafe { fill::<VkExtensionProperties>(&[], p_property_count, p_properties) };
    }
    let props = extension_properties(icd().device_extensions());
    // SAFETY: the caller's storage.
    unsafe { fill(&props, p_property_count, p_properties) }
}

pub unsafe extern "system" fn sv_vk_enumerate_device_layer_properties(
    _physical_device: VkPhysicalDevice,
    p_property_count: *mut u32,
    p_properties: *mut VkLayerProperties,
) -> VkResult {
    // SAFETY: the caller's storage.
    unsafe { fill::<VkLayerProperties>(&[], p_property_count, p_properties) }
}

// ---------------------------------------------------------------------------
// the dispatchable frontier
// ---------------------------------------------------------------------------

pub unsafe extern "system" fn sv_vk_create_instance(
    p_create_info: *const VkInstanceCreateInfo,
    _p_allocator: *const VkAllocationCallbacks,
    p_instance: *mut VkInstance,
) -> VkResult {
    if p_create_info.is_null() || p_instance.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: non-null and valid for the call, per the specification.
    let info = match unsafe { lower_vk_instance_create_info(p_create_info) } {
        Ok(i) => i,
        Err(e) => return lower_failed(e),
    };
    match icd().create_instance(&info) {
        Ok(id) => {
            // SAFETY: the loader passes writable storage for one handle.
            unsafe { *p_instance = new_object(id, KIND_INSTANCE) as VkInstance };
            VkResult::VK_SUCCESS
        }
        Err(e) => lower_failed(e),
    }
}

pub unsafe extern "system" fn sv_vk_destroy_instance(
    instance: VkInstance,
    _p_allocator: *const VkAllocationCallbacks,
) {
    if instance.is_null() {
        return;
    }
    // SAFETY: a handle this ICD returned and has not yet reclaimed.
    unsafe {
        icd().destroy_instance(dispatchable_id(instance as *const c_void));
        let _ = Dispatchable::reclaim(instance as *mut Object);
    }
}

pub unsafe extern "system" fn sv_vk_enumerate_physical_devices(
    instance: VkInstance,
    p_physical_device_count: *mut u32,
    p_physical_devices: *mut VkPhysicalDevice,
) -> VkResult {
    // SAFETY: a handle this ICD returned.
    let id = unsafe { dispatchable_id(instance as *const c_void) };
    let ids = match icd().enumerate_physical_devices(id) {
        Ok(v) => v,
        Err(e) => return lower_failed(e),
    };
    if p_physical_device_count.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: the caller's storage, bounded by `*p_physical_device_count`.
    unsafe {
        if p_physical_devices.is_null() {
            *p_physical_device_count = ids.len() as u32;
            return VkResult::VK_SUCCESS;
        }
        let n = core::cmp::min(*p_physical_device_count as usize, ids.len());
        for (i, id) in ids.iter().take(n).enumerate() {
            // A physical device is enumerated repeatedly and never destroyed, so
            // a fresh allocation per call would leak one object per
            // `vkEnumeratePhysicalDevices`. `sv-icd` therefore owns the objects
            // and hands back the same pointers; see `Icd::physical_device_handle`.
            *p_physical_devices.add(i) = physical_device_handle(*id);
        }
        *p_physical_device_count = n as u32;
        if n < ids.len() { VkResult::VK_INCOMPLETE } else { VkResult::VK_SUCCESS }
    }
}

/// The stable dispatchable object for a physical device id.
///
/// Physical devices are enumerated as often as an application likes and are
/// never destroyed, so their objects are cached rather than allocated per call.
/// The alternative — a fresh allocation each time — leaks, and worse, gives an
/// application two handles it is entitled to compare with `==`.
fn physical_device_handle(id: u64) -> VkPhysicalDevice {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<u64, usize>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    let p = *guard
        .entry(id)
        .or_insert_with(|| new_object(id, KIND_PHYSICAL_DEVICE) as usize);
    p as VkPhysicalDevice
}

pub unsafe extern "system" fn sv_vk_create_device(
    physical_device: VkPhysicalDevice,
    p_create_info: *const VkDeviceCreateInfo,
    _p_allocator: *const VkAllocationCallbacks,
    p_device: *mut VkDevice,
) -> VkResult {
    if p_create_info.is_null() || p_device.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: handles and pointers the specification says are valid here.
    let phys = unsafe { dispatchable_id(physical_device as *const c_void) };
    let info = match unsafe { lower_vk_device_create_info(p_create_info) } {
        Ok(i) => i,
        Err(e) => return lower_failed(e),
    };
    match icd().create_device(phys, &info) {
        Ok(id) => {
            // SAFETY: writable storage for one handle.
            unsafe { *p_device = new_object(id, KIND_DEVICE) as VkDevice };
            VkResult::VK_SUCCESS
        }
        Err(e) => lower_failed(e),
    }
}

pub unsafe extern "system" fn sv_vk_destroy_device(
    device: VkDevice,
    _p_allocator: *const VkAllocationCallbacks,
) {
    if device.is_null() {
        return;
    }
    // SAFETY: a handle this ICD returned.
    unsafe {
        icd().destroy_device(dispatchable_id(device as *const c_void));
        let _ = Dispatchable::reclaim(device as *mut Object);
    }
}

pub unsafe extern "system" fn sv_vk_get_device_queue(
    device: VkDevice,
    queue_family_index: u32,
    queue_index: u32,
    p_queue: *mut VkQueue,
) {
    if p_queue.is_null() {
        return;
    }
    // SAFETY: a handle this ICD returned.
    let dev = unsafe { dispatchable_id(device as *const c_void) };
    match icd().get_device_queue(dev, queue_family_index, queue_index) {
        Ok(id) => {
            // As with physical devices, a queue is fetched repeatedly and never
            // destroyed, and the specification lets an application compare the
            // handles it gets back.
            // SAFETY: writable storage for one handle.
            unsafe { *p_queue = queue_handle(id) };
        }
        Err(e) => {
            report_failure(e);
            // SAFETY: writable storage for one handle.
            unsafe { *p_queue = core::ptr::null_mut() };
        }
    }
}

fn queue_handle(id: u64) -> VkQueue {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<u64, usize>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    let p = *guard.entry(id).or_insert_with(|| new_object(id, KIND_QUEUE) as usize);
    p as VkQueue
}

pub unsafe extern "system" fn sv_vk_allocate_command_buffers(
    device: VkDevice,
    p_allocate_info: *const VkCommandBufferAllocateInfo,
    p_command_buffers: *mut VkCommandBuffer,
) -> VkResult {
    if p_allocate_info.is_null() || p_command_buffers.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: valid for the call.
    let dev = unsafe { dispatchable_id(device as *const c_void) };
    let info = match unsafe { lower_vk_command_buffer_allocate_info(p_allocate_info) } {
        Ok(i) => i,
        Err(e) => return lower_failed(e),
    };
    let want = info.command_buffer_count as usize;
    let ids = match icd().allocate_command_buffers(dev, &info) {
        Ok(v) => v,
        Err(e) => return lower_failed(e),
    };
    if ids.len() < want {
        return VkResult::VK_ERROR_OUT_OF_DEVICE_MEMORY;
    }
    // SAFETY: the caller allocated `commandBufferCount` handles, which is what
    // the registry's `len=` says and what invariant 2 bounds this loop by.
    unsafe {
        for (i, id) in ids.iter().take(want).enumerate() {
            *p_command_buffers.add(i) = new_object(*id, KIND_COMMAND_BUFFER) as VkCommandBuffer;
        }
    }
    VkResult::VK_SUCCESS
}

pub unsafe extern "system" fn sv_vk_free_command_buffers(
    device: VkDevice,
    command_pool: VkCommandPool,
    command_buffer_count: u32,
    p_command_buffers: *const VkCommandBuffer,
) {
    if p_command_buffers.is_null() {
        return;
    }
    // SAFETY: `commandBufferCount` handles, per the registry's `len=`.
    let (dev, ids) = unsafe {
        let dev = dispatchable_id(device as *const c_void);
        let mut ids = Vec::with_capacity(command_buffer_count as usize);
        for i in 0..command_buffer_count as usize {
            let h = *p_command_buffers.add(i);
            if h.is_null() {
                continue;
            }
            ids.push(dispatchable_id(h as *const c_void));
            let _ = Dispatchable::reclaim(h as *mut Object);
        }
        (dev, ids)
    };
    icd().free_command_buffers(dev, command_pool, &ids);
}

// ---------------------------------------------------------------------------
// mapping
// ---------------------------------------------------------------------------

/// `vkMapMemory` — the whole point of the project, and it never leaves the
/// process.
///
/// design.md §5.3: "`vkMapMemory` on the app side returns `arena_base +
/// slab_base + alloc_offset + offset`. A real pointer into real shared pages. It
/// involves **no round trip and no copy**." That the view is stitched from
/// hundreds or thousands of `mmap` calls (§5.7) is invisible here and must stay
/// invisible.
pub unsafe extern "system" fn sv_vk_map_memory(
    device: VkDevice,
    memory: VkDeviceMemory,
    offset: VkDeviceSize,
    size: VkDeviceSize,
    _flags: VkMemoryMapFlags,
    pp_data: *mut *mut c_void,
) -> VkResult {
    if pp_data.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: a handle this ICD returned.
    let dev = unsafe { dispatchable_id(device as *const c_void) };
    match icd().map_memory(dev, memory, offset, size) {
        Ok(p) => {
            // SAFETY: writable storage for one pointer.
            unsafe { *pp_data = p };
            VkResult::VK_SUCCESS
        }
        Err(e) => lower_failed(e),
    }
}

pub unsafe extern "system" fn sv_vk_unmap_memory(device: VkDevice, memory: VkDeviceMemory) {
    // SAFETY: a handle this ICD returned.
    let dev = unsafe { dispatchable_id(device as *const c_void) };
    icd().unmap_memory(dev, memory);
}

// ---------------------------------------------------------------------------
// descriptor update templates
// ---------------------------------------------------------------------------

/// Expand a `vkUpdateDescriptorSetWithTemplate` into the `VkWriteDescriptorSet`
/// list it means.
///
/// # Why this is here and not in the generator
///
/// `pData` is a `const void*` with **no `len=`, no `altlen=` and no sibling that
/// bounds it**. Its layout is described by the *template object* — a handle —
/// and by nothing in the registry. It is the one parameter on this surface whose
/// size is a property of another object's contents, so no fold over `vk.xml`
/// can marshal it and design.md §7.4's `void*` policy table can only classify it
/// as opaque. Venus and gfxstream both special-case it; so do we.
///
/// # Why translating is better than forwarding
///
/// The pointer is the *application's* and this code runs in the application's
/// process, so the data is readable here and meaningless anywhere else. Reading
/// it now turns a parameter that cannot cross into one that never has to:
/// `vkUpdateDescriptorSets` carries the same information in a shape the registry
/// does describe, and the GPU side needs no special case at all. The template
/// object still crosses and is still created on the far side — it is only the
/// *update* that is rewritten.
///
/// This is also what makes Zink work: Zink's descriptor path uses templates
/// unconditionally in its default mode, so without this every descriptor update
/// is a null pointer handed to a driver.
///
/// # Safety
///
/// `p_data` is the application's `pData` for a call made with `entries`, so for
/// each entry `e` and each `j < e.descriptor_count` the bytes at
/// `p_data + e.offset + j * e.stride` are a valid value of the type `e
/// .descriptor_type` selects. That is the Vulkan specification's contract for
/// this command and it is not checkable — the whole point of the parameter is
/// that its shape lives in the template.
pub unsafe fn expand_descriptor_template(
    p_data: u64,
    dst_set: u64,
    entries: &[sv_proto::VkDescriptorUpdateTemplateEntry],
) -> Vec<sv_proto::VkWriteDescriptorSet> {
    let mut out = Vec::with_capacity(entries.len());
    if p_data == 0 {
        return out;
    }
    for e in entries {
        let n = e.descriptor_count as usize;
        let mut w = sv_proto::VkWriteDescriptorSet {
            p_next: Vec::new(),
            dst_set: sv_proto::VkDescriptorSet(dst_set),
            dst_binding: e.dst_binding,
            dst_array_element: e.dst_array_element,
            descriptor_count: e.descriptor_count,
            descriptor_type: e.descriptor_type,
            p_image_info: Vec::new(),
            p_buffer_info: Vec::new(),
            p_texel_buffer_view: Vec::new(),
        };
        let at = |j: usize| -> *const u8 {
            (p_data as *const u8).wrapping_add(e.offset as usize + j * e.stride as usize)
        };
        match e.descriptor_type.0 {
            // SAMPLER, COMBINED_IMAGE_SAMPLER, SAMPLED_IMAGE, STORAGE_IMAGE,
            // INPUT_ATTACHMENT
            0 | 1 | 2 | 3 | 10 => {
                for j in 0..n {
                    let p = at(j) as *const VkDescriptorImageInfo;
                    let c = unsafe { core::ptr::read_unaligned(p) };
                    w.p_image_info.push(sv_proto::VkDescriptorImageInfo {
                        sampler: sv_proto::VkSampler(c.sampler),
                        image_view: sv_proto::VkImageView(c.image_view),
                        image_layout: sv_proto::VkImageLayout(c.image_layout.0),
                    });
                }
            }
            // UNIFORM_TEXEL_BUFFER, STORAGE_TEXEL_BUFFER
            4 | 5 => {
                for j in 0..n {
                    let p = at(j) as *const VkBufferView;
                    let c = unsafe { core::ptr::read_unaligned(p) };
                    w.p_texel_buffer_view.push(sv_proto::VkBufferView(c));
                }
            }
            // UNIFORM_BUFFER, STORAGE_BUFFER, and the two dynamic forms
            6..=9 => {
                for j in 0..n {
                    let p = at(j) as *const VkDescriptorBufferInfo;
                    let c = unsafe { core::ptr::read_unaligned(p) };
                    w.p_buffer_info.push(sv_proto::VkDescriptorBufferInfo {
                        buffer: sv_proto::VkBuffer(c.buffer),
                        offset: c.offset,
                        range: c.range,
                    });
                }
            }
            // An inline uniform block's "count" is a byte length and its data is
            // carried in a `VkWriteDescriptorSetInlineUniformBlock` chain link
            // rather than in an array. Not in this profile's feature set;
            // dropped rather than guessed at, and said so.
            _ => {
                report_transport_failure(&format_args!(
                    "vkUpdateDescriptorSetWithTemplate: descriptor type {} is not expandable",
                    e.descriptor_type.0
                ));
                continue;
            }
        }
        out.push(w);
    }
    out
}

// ---------------------------------------------------------------------------
// presentation (design.md §10)
// ---------------------------------------------------------------------------
//
// Thirteen entry points, all hand-written for the same two reasons: none of
// them has a wire opcode, so there is no reply to fold; and the one parameter
// that matters is a `VkSurfaceKHR`, which is not a handle at all but a pointer
// into the *loader's* memory whose layout `vk_icd.h` fixes and `vk.xml` does
// not mention.
//
// What each of them does is in `sv_icd::wsi`. What is here is the pointer work.

/// The two surface create-info structs the registry will not give us.
///
/// `VkXcbSurfaceCreateInfoKHR::connection` is an `xcb_connection_t*` and
/// `VkXlibSurfaceCreateInfoKHR::dpy` is a `Display*`, both from platform headers
/// this project does not include, so `sv_codegen::abi::SURFACE_CREATORS` keeps
/// the two commands and the two structs off the generated C surface and they are
/// declared here instead — which costs nine lines and no build dependency.
///
/// **Neither pointer is ever dereferenced.** All this ICD takes from an X
/// surface is the window id; see `sv_icd::present` for why it opens a connection
/// of its own rather than using the application's.
mod surface_ci {
    /// `{ VkStructureType sType; const void* pNext; VkFlags flags;
    /// xcb_connection_t* connection; xcb_window_t window; }`
    #[repr(C)]
    pub struct Xcb {
        pub s_type: i32,
        pub p_next: *const core::ffi::c_void,
        pub flags: u32,
        pub connection: *mut core::ffi::c_void,
        pub window: u32,
    }

    /// `{ VkStructureType sType; const void* pNext; VkFlags flags;
    /// Display* dpy; Window window; }`. `Window` is an X11 `XID`, an
    /// `unsigned long`.
    #[repr(C)]
    pub struct Xlib {
        pub s_type: i32,
        pub p_next: *const core::ffi::c_void,
        pub flags: u32,
        pub dpy: *mut core::ffi::c_void,
        pub window: core::ffi::c_ulong,
    }

    /// `{ VkStructureType sType; const void* pNext; VkFlags flags;
    /// struct wl_display* display; struct wl_surface* surface; }`.
    ///
    /// The one whose pointers are not merely carried: see [`PresentTarget`] for
    /// why a `wl_surface` has no name a second connection could resolve, and
    /// `sv_icd::present::wayland` for what is done with them.
    #[repr(C)]
    pub struct Wayland {
        pub s_type: i32,
        pub p_next: *const core::ffi::c_void,
        pub flags: u32,
        pub display: *mut core::ffi::c_void,
        pub surface: *mut core::ffi::c_void,
    }
}

/// The `VkSurfaceKHR` a terminator handed us, resolved to what it points at.
fn read_surface(surface: VkSurfaceKHR) -> Option<PresentTarget> {
    if surface == 0 {
        return None;
    }
    let t = icd().surface_target(surface);
    if t.is_none() {
        report_transport_failure(&format_args!(
            "VkSurfaceKHR {surface:#x} is not one this ICD created"
        ));
    }
    t
}

fn create_surface(target: PresentTarget, p_surface: *mut VkSurfaceKHR) -> VkResult {
    if p_surface.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    match icd().create_surface(target) {
        Ok(id) => {
            // SAFETY: the loader passes writable storage for one
            // non-dispatchable handle.
            unsafe { *p_surface = id };
            VkResult::VK_SUCCESS
        }
        Err(e) => lower_failed(e),
    }
}

/// `vkCreateHeadlessSurfaceEXT`.
///
/// See `sv_codegen::emit::icd_entry::EXTRA_LOCAL` for why an ICD that would
/// rather not implement surface creation has to implement it anyway.
pub unsafe extern "system" fn sv_vk_create_headless_surface_ext(
    _instance: VkInstance,
    _p_create_info: *const VkHeadlessSurfaceCreateInfoEXT,
    _p_allocator: *const VkAllocationCallbacks,
    p_surface: *mut VkSurfaceKHR,
) -> VkResult {
    create_surface(PresentTarget::Headless, p_surface)
}

pub unsafe extern "system" fn sv_vk_create_xlib_surface_khr(
    _instance: VkInstance,
    p_create_info: *const c_void,
    _p_allocator: *const VkAllocationCallbacks,
    p_surface: *mut VkSurfaceKHR,
) -> VkResult {
    if p_create_info.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: the loader passes the application's `VkXlibSurfaceCreateInfoKHR`,
    // valid for reads for the duration of the call.
    let ci = unsafe { &*(p_create_info as *const surface_ci::Xlib) };
    create_surface(PresentTarget::X11 { window: ci.window as u64 }, p_surface)
}

pub unsafe extern "system" fn sv_vk_create_xcb_surface_khr(
    _instance: VkInstance,
    p_create_info: *const c_void,
    _p_allocator: *const VkAllocationCallbacks,
    p_surface: *mut VkSurfaceKHR,
) -> VkResult {
    if p_create_info.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: as above, for `VkXcbSurfaceCreateInfoKHR`.
    let ci = unsafe { &*(p_create_info as *const surface_ci::Xcb) };
    create_surface(PresentTarget::X11 { window: ci.window as u64 }, p_surface)
}

/// `vkCreateWaylandSurfaceKHR`.
///
/// A null `display` or `surface` is refused rather than carried, because a
/// Wayland target with either missing is not a headless surface that shows
/// nothing — it is a pointer that something downstream would dereference.
pub unsafe extern "system" fn sv_vk_create_wayland_surface_khr(
    _instance: VkInstance,
    p_create_info: *const c_void,
    _p_allocator: *const VkAllocationCallbacks,
    p_surface: *mut VkSurfaceKHR,
) -> VkResult {
    if p_create_info.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: as above, for `VkWaylandSurfaceCreateInfoKHR`.
    let ci = unsafe { &*(p_create_info as *const surface_ci::Wayland) };
    if ci.display.is_null() || ci.surface.is_null() {
        report_transport_failure(&format_args!(
            "vkCreateWaylandSurfaceKHR: display {:?} surface {:?} — one of them is null",
            ci.display, ci.surface
        ));
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    create_surface(
        PresentTarget::Wayland { display: ci.display as usize, surface: ci.surface as usize },
        p_surface,
    )
}

/// `vkDestroySurfaceKHR`.
pub unsafe extern "system" fn sv_vk_destroy_surface_khr(
    _instance: VkInstance,
    surface: VkSurfaceKHR,
    _p_allocator: *const VkAllocationCallbacks,
) {
    if surface != 0 {
        icd().destroy_surface(surface);
    }
}

/// `vkGetPhysicalDeviceSurfaceSupportKHR`.
///
/// True for family 0 — the one family design.md §9 advertises — and for any
/// surface whose platform we can read. Presentation needs nothing of the GPU
/// side, so there is no family that could fail to support it.
pub unsafe extern "system" fn sv_vk_get_physical_device_surface_support_khr(
    _physical_device: VkPhysicalDevice,
    queue_family_index: u32,
    surface: VkSurfaceKHR,
    p_supported: *mut VkBool32,
) -> VkResult {
    if p_supported.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: a surface from the loader.
    let ok = read_surface(surface).is_some() && queue_family_index == 0;
    report_transport_failure(&format_args!(
        "vkGetPhysicalDeviceSurfaceSupportKHR: family {queue_family_index} -> {ok}"
    ));
    // SAFETY: writable storage for one `VkBool32`.
    unsafe { *p_supported = u32::from(ok) };
    VkResult::VK_SUCCESS
}

/// `vkGetPhysicalDeviceXlibPresentationSupportKHR`, and its XCB twin below.
///
/// **These exist because the loader `abort()`s without them.** Its terminator
/// for the `…PresentationSupportKHR` family does not fall back to a default
/// when the selected ICD does not export the entry point — it logs "ICD for
/// selected physical device does not export …" and aborts the process. So
/// advertising `VK_KHR_xcb_surface` at all obliges us to answer, and the answer
/// is yes: presentation here is a memcpy from the arena into a window, and any
/// X visual can receive one.
pub unsafe extern "system" fn sv_vk_get_physical_device_xlib_presentation_support_khr(
    _physical_device: VkPhysicalDevice,
    queue_family_index: u32,
    _dpy: *mut c_void,
    _visual_id: core::ffi::c_ulong,
) -> VkBool32 {
    u32::from(queue_family_index == 0)
}

pub unsafe extern "system" fn sv_vk_get_physical_device_xcb_presentation_support_khr(
    _physical_device: VkPhysicalDevice,
    queue_family_index: u32,
    _connection: *mut c_void,
    _visual_id: u32,
) -> VkBool32 {
    u32::from(queue_family_index == 0)
}

/// `vkGetPhysicalDeviceWaylandPresentationSupportKHR`.
///
/// Same obligation as the two above, and the same answer, but the reason it is
/// *true* is different and worth one line: on Wayland the presentation engine
/// really is this driver — a `wl_shm` buffer attached to the application's own
/// surface — so there is no compositor-side capability that could say no. What
/// this must not do is dereference the `wl_display`: the loader calls it before
/// any surface exists, from whatever thread the application asked on.
pub unsafe extern "system" fn sv_vk_get_physical_device_wayland_presentation_support_khr(
    _physical_device: VkPhysicalDevice,
    queue_family_index: u32,
    _display: *mut c_void,
) -> VkBool32 {
    u32::from(queue_family_index == 0)
}

/// `vkGetPhysicalDeviceSurfaceCapabilitiesKHR`.
///
/// Everything but `currentExtent` is a constant of this implementation rather
/// than a property of a window, because the presentation engine *is* this
/// implementation: there is no compositor to impose a transform, no alpha
/// blending we do not do ourselves, and no minimum image count beyond what FIFO
/// needs.
pub unsafe extern "system" fn sv_vk_get_physical_device_surface_capabilities_khr(
    _physical_device: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    p_capabilities: *mut VkSurfaceCapabilitiesKHR,
) -> VkResult {
    if p_capabilities.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: a surface from the loader.
    let Some(t) = read_surface(surface) else {
        return VkResult::VK_ERROR_SURFACE_LOST_KHR;
    };
    let caps = match icd().surface_capabilities(t) {
        Ok(c) => c,
        Err(e) => return lower_failed(e),
    };
    let mut out = VkSurfaceCapabilitiesKHR::default();
    out.min_image_count = MIN_SWAPCHAIN_IMAGES;
    out.max_image_count = MAX_SWAPCHAIN_IMAGES;
    out.current_extent = VkExtent2D { width: caps.current_width, height: caps.current_height };
    out.min_image_extent = VkExtent2D { width: 1, height: 1 };
    out.max_image_extent = VkExtent2D { width: 16384, height: 16384 };
    out.max_image_array_layers = 1;
    out.supported_transforms =
        VkSurfaceTransformFlagsKHR::VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR;
    out.current_transform =
        VkSurfaceTransformFlagBitsKHR::VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR;
    out.supported_composite_alpha =
        VkCompositeAlphaFlagsKHR::VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR;
    // `TRANSFER_SRC` is not a courtesy to the application: it is what the ICD
    // itself needs, because presenting is `vkCmdCopyImageToBuffer` out of the
    // image the application rendered into.
    out.supported_usage_flags = VkImageUsageFlags::VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT
        | VkImageUsageFlags::VK_IMAGE_USAGE_TRANSFER_SRC_BIT
        | VkImageUsageFlags::VK_IMAGE_USAGE_TRANSFER_DST_BIT
        | VkImageUsageFlags::VK_IMAGE_USAGE_SAMPLED_BIT
        | VkImageUsageFlags::VK_IMAGE_USAGE_STORAGE_BIT;
    // SAFETY: writable storage for one struct.
    unsafe { *p_capabilities = out };
    VkResult::VK_SUCCESS
}

pub unsafe extern "system" fn sv_vk_get_physical_device_surface_formats_khr(
    _physical_device: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    p_surface_format_count: *mut u32,
    p_surface_formats: *mut VkSurfaceFormatKHR,
) -> VkResult {
    // SAFETY: a surface from the loader.
    let Some(t) = read_surface(surface) else {
        return VkResult::VK_ERROR_SURFACE_LOST_KHR;
    };
    let formats: Vec<VkSurfaceFormatKHR> = icd()
        .surface_formats(t)
        .into_iter()
        .map(|(f, c)| VkSurfaceFormatKHR { format: VkFormat(f), color_space: VkColorSpaceKHR(c) })
        .collect();
    // SAFETY: the count-then-fill contract, once, as everywhere else here.
    unsafe { fill(&formats, p_surface_format_count, p_surface_formats) }
}

pub unsafe extern "system" fn sv_vk_get_physical_device_surface_present_modes_khr(
    _physical_device: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    p_present_mode_count: *mut u32,
    p_present_modes: *mut VkPresentModeKHR,
) -> VkResult {
    // SAFETY: a surface from the loader.
    let Some(t) = read_surface(surface) else {
        return VkResult::VK_ERROR_SURFACE_LOST_KHR;
    };
    let modes: Vec<VkPresentModeKHR> =
        icd().surface_present_modes(t).into_iter().map(VkPresentModeKHR).collect();
    // SAFETY: as above.
    unsafe { fill(&modes, p_present_mode_count, p_present_modes) }
}

/// `vkCreateSwapchainKHR`.
///
/// The `VkSurfaceKHR` is resolved here and the rest of the structure reduced to
/// a [`SwapchainDesc`], so that `sv-icd` never sees the loader's pointer. The
/// `pNext` chain is *dropped* rather than refused: every link that can legally
/// appear on this structure comes from an extension we do not advertise, so an
/// application cannot have written one that means anything.
pub unsafe extern "system" fn sv_vk_create_swapchain_khr(
    device: VkDevice,
    p_create_info: *const VkSwapchainCreateInfoKHR,
    _p_allocator: *const VkAllocationCallbacks,
    p_swapchain: *mut VkSwapchainKHR,
) -> VkResult {
    if p_create_info.is_null() || p_swapchain.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: a handle this ICD returned, and a structure the specification
    // says is valid for reads for the duration of the call.
    let dev = unsafe { dispatchable_id(device as *const c_void) };
    let ci = unsafe { &*p_create_info };
    // SAFETY: a surface from the loader.
    let Some(target) = read_surface(ci.surface) else {
        return VkResult::VK_ERROR_SURFACE_LOST_KHR;
    };
    let desc = SwapchainDesc {
        target,
        min_image_count: ci.min_image_count,
        format: ci.image_format.0,
        color_space: ci.image_color_space.0,
        width: ci.image_extent.width,
        height: ci.image_extent.height,
        image_usage: ci.image_usage.0,
        present_mode: ci.present_mode.0,
        old_swapchain: ci.old_swapchain,
    };
    match icd().create_swapchain(dev, &desc) {
        Ok(id) => {
            // SAFETY: writable storage for one non-dispatchable handle.
            unsafe { *p_swapchain = id };
            VkResult::VK_SUCCESS
        }
        Err(e) => lower_failed(e),
    }
}

pub unsafe extern "system" fn sv_vk_destroy_swapchain_khr(
    device: VkDevice,
    swapchain: VkSwapchainKHR,
    _p_allocator: *const VkAllocationCallbacks,
) {
    if device.is_null() || swapchain == 0 {
        return;
    }
    // SAFETY: a handle this ICD returned.
    let dev = unsafe { dispatchable_id(device as *const c_void) };
    icd().destroy_swapchain(dev, swapchain);
}

pub unsafe extern "system" fn sv_vk_get_swapchain_images_khr(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    p_swapchain_image_count: *mut u32,
    p_swapchain_images: *mut VkImage,
) -> VkResult {
    let images = match icd().swapchain_images(swapchain) {
        Ok(v) => v,
        Err(e) => return lower_failed(e),
    };
    // SAFETY: the count-then-fill contract.
    unsafe { fill(&images, p_swapchain_image_count, p_swapchain_images) }
}

pub unsafe extern "system" fn sv_vk_acquire_next_image_khr(
    device: VkDevice,
    swapchain: VkSwapchainKHR,
    timeout: u64,
    semaphore: VkSemaphore,
    fence: VkFence,
    p_image_index: *mut u32,
) -> VkResult {
    if p_image_index.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: a handle this ICD returned.
    let dev = unsafe { dispatchable_id(device as *const c_void) };
    match icd().acquire_next_image(dev, swapchain, timeout, semaphore, fence) {
        Ok(i) => {
            // SAFETY: writable storage for one index.
            unsafe { *p_image_index = i };
            VkResult::VK_SUCCESS
        }
        Err(e) => lower_failed(e),
    }
}

/// `vkQueuePresentKHR`.
///
/// The wait semaphores go to the **first** swapchain's copy submission and to
/// no other. That is what "the presentation engine waits once" means for an
/// implementation that presents by submitting: a binary semaphore's signal can
/// only be waited on once, so handing the same list to a second submission
/// would deadlock rather than double-wait.
pub unsafe extern "system" fn sv_vk_queue_present_khr(
    queue: VkQueue,
    p_present_info: *const VkPresentInfoKHR,
) -> VkResult {
    if p_present_info.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: a handle this ICD returned, and the application's own structure.
    let q = unsafe { dispatchable_id(queue as *const c_void) };
    let info = unsafe { &*p_present_info };
    if info.p_swapchains.is_null() || info.p_image_indices.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: `pWaitSemaphores` has `waitSemaphoreCount` elements — invariant 2
    // of the generator's header, applied by hand because this stub is by hand.
    let waits: Vec<u64> = if info.p_wait_semaphores.is_null() {
        Vec::new()
    } else {
        unsafe {
            core::slice::from_raw_parts(info.p_wait_semaphores, info.wait_semaphore_count as usize)
        }
        .to_vec()
    };
    let n = info.swapchain_count as usize;
    // SAFETY: both arrays have `swapchainCount` elements.
    let (chains, indices) = unsafe {
        (
            core::slice::from_raw_parts(info.p_swapchains, n),
            core::slice::from_raw_parts(info.p_image_indices, n),
        )
    };
    let mut worst = VkResult::VK_SUCCESS;
    for i in 0..n {
        let r =
            match icd().queue_present(q, chains[i], indices[i], if i == 0 { &waits } else { &[] }) {
                Ok(()) => VkResult::VK_SUCCESS,
                Err(e) => lower_failed(e),
            };
        if !info.p_results.is_null() {
            // SAFETY: `pResults` is null or has `swapchainCount` elements.
            unsafe { *info.p_results.add(i) = r };
        }
        if r.0 < worst.0 {
            worst = r;
        }
    }
    worst
}
