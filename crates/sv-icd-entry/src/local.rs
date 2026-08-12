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
