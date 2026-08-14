//! The honest end-to-end test: drive this ICD **through the real Vulkan
//! loader**.
//!
//! docs/design.md §13 step 5 asks for "manifest, `VK_DRIVER_FILES`,
//! dispatchable-handle magic, interface negotiation, `vulkaninfo` producing a
//! sane report". `vulkaninfo` is a person running a command; this is the same
//! path as a test, and it is the only test in the workspace that exercises:
//!
//! - the loader finding and accepting the manifest,
//! - `vk_icdNegotiateLoaderICDInterfaceVersion` and the export set,
//! - **the dispatchable-handle contract**, which nothing else can check. The
//!   loader overwrites the first word of every handle we return with its own
//!   dispatch table pointer and then dereferences it on the next call. Getting
//!   that wrong is a fault inside the loader with our frame nowhere in the
//!   backtrace (docs/design.md §6.4, R17), and the loader's own assertion is
//!   debug-only.
//!
//! The loader is `dlopen`ed rather than linked, so the test skips cleanly on a
//! machine that has no `libvulkan.so.1` instead of failing to build. It also
//! builds the `cdylib` it drives if `cargo test` has not — see
//! [`ensure_driver_artefact`], which is the difference between this file
//! reporting a defect and reporting the order cargo happened to build things
//! in.
//!
//! Everything below uses `sv_icd_entry`'s C ABI types, which is a second thing
//! this test checks by construction: if those layouts were wrong, the loader
//! and this test would disagree about where a field is and the calls would fail
//! in ways no amount of Rust-level care would explain.

// Miri cannot `dlopen` a real loader, and a mocked one would be testing this
// file rather than the loader's contract. Everything it *can* usefully model —
// the generated lowering, the writeback and the dispatchable header — is in
// `sv-icd-entry`'s own tests, which do run under it.
#![cfg(all(unix, not(miri)))]
#![allow(non_snake_case)]
// A Vulkan create-info is `Default` plus the three fields that matter; naming
// all fourteen would bury them.
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::missing_transmute_annotations)]

use core::ffi::{c_char, c_void};
use sv_icd_entry::*;

// ---------------------------------------------------------------------------
// the driver artefact, which `cargo test` does not build on its own
// ---------------------------------------------------------------------------

/// Make sure the `cdylib` the manifest names actually exists, building it if it
/// does not, and say why if it still does not.
///
/// # The gate this replaces reported the wrong thing
///
/// `sv-icd` is `crate-type = ["cdylib", "rlib"]`. An integration test links the
/// `rlib`, so `cargo test` builds *only* the `rlib`: nothing in the test's
/// dependency graph needs the `cdylib`, and cargo builds what is needed. On a
/// freshly cloned tree the manifest therefore names a `libsv_icd.so` that has
/// never been produced, the loader skips a driver whose `library_path` does not
/// resolve without logging anything, and `vkCreateInstance` returns
/// `VK_ERROR_INCOMPATIBLE_DRIVER` (-9).
///
/// That is a fact about build ordering, not about the code — and it was worse
/// than a false red, because a test binary that *fails* stops `cargo test
/// --workspace` where it stands. The run reported 166 tests instead of 1207,
/// and twice an agent nearly published the truncated number as a baseline.
/// Cargo has no stable way to say "this test depends on that `cdylib`"
/// (artifact dependencies are still unstable), so the test arranges it itself.
///
/// `env!("CARGO")` is the cargo that compiled this file, so the nested build
/// uses the same toolchain rather than whatever is first on `PATH`. If it
/// cannot be run — no cargo, a read-only target directory, a custom profile
/// whose artefacts land somewhere this file cannot predict — the test *skips*
/// with the reason printed. Set `SV_ICD_REQUIRE_LOADER=1` to turn that skip
/// into a failure, which is what a release gate should do: a check that can
/// only ever skip is not a check.
fn ensure_driver_artefact() -> Result<(), String> {
    let lib = std::path::Path::new(sv_icd::LIBRARY_PATH);
    if lib.exists() {
        return Ok(());
    }

    let mut cmd = std::process::Command::new(env!("CARGO"));
    cmd.arg("build")
        .arg("--offline")
        .arg("--lib")
        .arg("--manifest-path")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
    if sv_icd::PROFILE == "release" {
        cmd.arg("--release");
    }
    let attempt = match cmd.output() {
        Ok(o) => o,
        Err(e) => return Err(format!("could not run {}: {e}", env!("CARGO"))),
    };
    if lib.exists() {
        return Ok(());
    }
    Err(format!(
        "{} does not exist and `cargo build --lib` did not produce it{}\n{}",
        sv_icd::LIBRARY_PATH,
        if attempt.status.success() { " either" } else { "; the build failed" },
        String::from_utf8_lossy(&attempt.stderr).trim_end(),
    ))
}

/// Report a reason this test cannot run, and decide whether that is fatal.
///
/// Skipping is the default because the two reasons — no `libvulkan.so.1`, no
/// buildable `cdylib` — are both properties of the machine rather than of the
/// code. `SV_ICD_REQUIRE_LOADER=1` makes them fatal for anyone who wants to
/// know that this gate actually ran.
fn skip(reason: &str) {
    if std::env::var_os("SV_ICD_REQUIRE_LOADER").is_some_and(|v| v != "0") {
        panic!("SV_ICD_REQUIRE_LOADER is set and the loader test cannot run: {reason}");
    }
    eprintln!("SKIP: the real-loader test cannot run: {reason}");
}

// ---------------------------------------------------------------------------
// the loader, opened by hand
// ---------------------------------------------------------------------------

struct Loader {
    handle: *mut c_void,
    get_instance_proc_addr:
        unsafe extern "system" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction,
}

impl Loader {
    fn open() -> Option<Loader> {
        // Before anything else: the driver the manifest names has to exist. A
        // missing `cdylib` and a broken one both come back from the loader as
        // `VK_ERROR_INCOMPATIBLE_DRIVER`, and only one of them is a defect.
        if let Err(why) = ensure_driver_artefact() {
            skip(&why);
            return None;
        }

        // The manifest must be visible before the loader scans for drivers, and
        // the loader scans once per `vkCreateInstance`, so setting it here is in
        // time. `VK_DRIVER_FILES` rather than the deprecated
        // `VK_ICD_FILENAMES`: the loader ignores the latter when both are set
        // (docs/design.md §8).
        //
        // SAFETY: single-threaded at this point; no other thread reads the
        // environment.
        unsafe {
            std::env::set_var("VK_DRIVER_FILES", sv_icd::MANIFEST_PATH);
            // Layers would work, but they change which entry points the loader
            // asks us for, and this test is about the driver.
            std::env::remove_var("VK_INSTANCE_LAYERS");
            std::env::remove_var("VK_LOADER_LAYERS_ENABLE");
        }

        for name in [c"libvulkan.so.1", c"libvulkan.so"] {
            // SAFETY: a NUL-terminated library name.
            let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if h.is_null() {
                continue;
            }
            // SAFETY: `h` is a live handle and the symbol has the signature the
            // Vulkan specification gives it.
            let sym = unsafe { libc::dlsym(h, c"vkGetInstanceProcAddr".as_ptr()) };
            if sym.is_null() {
                continue;
            }
            return Some(Loader {
                handle: h,
                // SAFETY: as above.
                get_instance_proc_addr: unsafe { std::mem::transmute(sym) },
            });
        }
        skip("no libvulkan.so.1 on this machine");
        None
    }

    /// # Safety
    /// `T` is the signature the specification gives `name`.
    unsafe fn get<T>(&self, instance: VkInstance, name: &std::ffi::CStr) -> Option<T> {
        // SAFETY: the caller's contract, plus a NUL-terminated name.
        unsafe {
            let f = (self.get_instance_proc_addr)(instance, name.as_ptr())?;
            Some(std::mem::transmute_copy::<_, T>(&f))
        }
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        // Deliberately not `dlclose`d: unloading the loader unloads this ICD,
        // and its `OnceLock`s and leaked objects would go with it while the
        // test process is still holding pointers to them.
        let _ = self.handle;
    }
}

fn check(r: VkResult, what: &str) {
    assert_eq!(r, VkResult::VK_SUCCESS, "{what} returned {r:?}");
}

// ---------------------------------------------------------------------------
// the sequence
// ---------------------------------------------------------------------------

#[test]
fn a_trivial_application_gets_through_the_real_loader() {
    let Some(loader) = Loader::open() else { return };

    // -- vkCreateInstance ---------------------------------------------------
    type PfnCreateInstance = unsafe extern "system" fn(
        *const VkInstanceCreateInfo,
        *const VkAllocationCallbacks,
        *mut VkInstance,
    ) -> VkResult;
    // SAFETY: the specification's signature, and a NULL instance is the global
    // lookup this command must be answerable by.
    let create_instance: PfnCreateInstance = unsafe {
        loader
            .get(core::ptr::null_mut(), c"vkCreateInstance")
            .expect("the loader must resolve vkCreateInstance")
    };

    let mut app = VkApplicationInfo::default();
    app.s_type = VkStructureType::VK_STRUCTURE_TYPE_APPLICATION_INFO;
    app.api_version = (1 << 22) | (3 << 12);
    let name = c"sv-icd loader test";
    app.p_application_name = name.as_ptr();
    app.p_engine_name = name.as_ptr();

    let mut ci = VkInstanceCreateInfo::default();
    ci.s_type = VkStructureType::VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
    ci.p_application_info = &raw const app;

    let mut instance: VkInstance = core::ptr::null_mut();
    // SAFETY: every pointer is a live local, and the arrays are empty with a
    // zero count.
    check(unsafe { create_instance(&raw const ci, core::ptr::null(), &raw mut instance) },
        "vkCreateInstance");
    assert!(!instance.is_null());

    // -- vkEnumeratePhysicalDevices ----------------------------------------
    type PfnEnumerate =
        unsafe extern "system" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
    // SAFETY: the specification's signature.
    let enumerate: PfnEnumerate =
        unsafe { loader.get(instance, c"vkEnumeratePhysicalDevices").unwrap() };

    let mut count = 0u32;
    // SAFETY: the count-then-fill idiom, with a null array on the first call.
    check(unsafe { enumerate(instance, &raw mut count, core::ptr::null_mut()) },
        "vkEnumeratePhysicalDevices (count)");
    assert_eq!(count, 1, "docs/design.md §8: one virtual device");

    let mut devices = vec![core::ptr::null_mut::<VkPhysicalDevice_T>(); count as usize];
    // SAFETY: `devices` has `count` elements.
    check(unsafe { enumerate(instance, &raw mut count, devices.as_mut_ptr()) },
        "vkEnumeratePhysicalDevices (fill)");
    let phys = devices[0];
    assert!(!phys.is_null());

    // Calling it twice must give the same handle: an application is entitled to
    // compare physical devices, and a fresh allocation per call would both leak
    // and lie.
    let mut again = vec![core::ptr::null_mut::<VkPhysicalDevice_T>(); count as usize];
    // SAFETY: as above.
    check(unsafe { enumerate(instance, &raw mut count, again.as_mut_ptr()) }, "enumerate twice");
    assert_eq!(again[0], phys, "physical device handles must be stable");

    // -- the memory table, which is the point of docs/design.md §9.1 --------------
    type PfnMemProps =
        unsafe extern "system" fn(VkPhysicalDevice, *mut VkPhysicalDeviceMemoryProperties);
    // SAFETY: the specification's signature.
    let mem_props: PfnMemProps =
        unsafe { loader.get(instance, c"vkGetPhysicalDeviceMemoryProperties").unwrap() };
    let mut mp = VkPhysicalDeviceMemoryProperties::default();
    // SAFETY: `mp` is a live local of the right type.
    unsafe { mem_props(phys, &raw mut mp) };
    assert_eq!(mp.memory_heap_count, 2);
    assert_eq!(mp.memory_type_count, 4);
    // The arena heap must be host-visible or an application will never take the
    // fast path, which is the whole project.
    let arena_type = 1usize;
    assert!(
        mp.memory_types[arena_type]
            .property_flags
            .contains(VkMemoryPropertyFlags::VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)
    );
    let arena_heap = mp.memory_types[arena_type].heap_index as usize;
    assert!(mp.memory_heaps[arena_heap].size > 0);

    // -- vkCreateDevice -----------------------------------------------------
    type PfnCreateDevice = unsafe extern "system" fn(
        VkPhysicalDevice,
        *const VkDeviceCreateInfo,
        *const VkAllocationCallbacks,
        *mut VkDevice,
    ) -> VkResult;
    // SAFETY: the specification's signature.
    let create_device: PfnCreateDevice =
        unsafe { loader.get(instance, c"vkCreateDevice").unwrap() };

    let priority = 1.0f32;
    let mut qci = VkDeviceQueueCreateInfo::default();
    qci.s_type = VkStructureType::VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
    qci.queue_family_index = 0;
    qci.queue_count = 1;
    qci.p_queue_priorities = &raw const priority;

    let mut dci = VkDeviceCreateInfo::default();
    dci.s_type = VkStructureType::VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
    dci.queue_create_info_count = 1;
    dci.p_queue_create_infos = &raw const qci;

    let mut device: VkDevice = core::ptr::null_mut();
    // SAFETY: every pointer is a live local and the counts match the arrays.
    check(unsafe { create_device(phys, &raw const dci, core::ptr::null(), &raw mut device) },
        "vkCreateDevice");
    assert!(!device.is_null());

    // -- the queue ----------------------------------------------------------
    type PfnGetDeviceProcAddr =
        unsafe extern "system" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction;
    // SAFETY: the specification's signature.
    let gdpa: PfnGetDeviceProcAddr =
        unsafe { loader.get(instance, c"vkGetDeviceProcAddr").unwrap() };

    // SAFETY: `gdpa` has the signature above and the names are NUL-terminated.
    let (get_queue, queue_submit, device_wait_idle, allocate_memory, map_memory, unmap_memory,
         free_memory, destroy_device) = unsafe {
        let g = |n: &std::ffi::CStr| gdpa(device, n.as_ptr()).expect("device entry point");
        (
            std::mem::transmute::<_, unsafe extern "system" fn(VkDevice, u32, u32, *mut VkQueue)>(
                g(c"vkGetDeviceQueue"),
            ),
            std::mem::transmute::<
                _,
                unsafe extern "system" fn(VkQueue, u32, *const VkSubmitInfo, VkFence) -> VkResult,
            >(g(c"vkQueueSubmit")),
            std::mem::transmute::<_, unsafe extern "system" fn(VkDevice) -> VkResult>(
                g(c"vkDeviceWaitIdle"),
            ),
            std::mem::transmute::<
                _,
                unsafe extern "system" fn(
                    VkDevice,
                    *const VkMemoryAllocateInfo,
                    *const VkAllocationCallbacks,
                    *mut VkDeviceMemory,
                ) -> VkResult,
            >(g(c"vkAllocateMemory")),
            std::mem::transmute::<
                _,
                unsafe extern "system" fn(
                    VkDevice,
                    VkDeviceMemory,
                    VkDeviceSize,
                    VkDeviceSize,
                    VkMemoryMapFlags,
                    *mut *mut c_void,
                ) -> VkResult,
            >(g(c"vkMapMemory")),
            std::mem::transmute::<_, unsafe extern "system" fn(VkDevice, VkDeviceMemory)>(
                g(c"vkUnmapMemory"),
            ),
            std::mem::transmute::<
                _,
                unsafe extern "system" fn(VkDevice, VkDeviceMemory, *const VkAllocationCallbacks),
            >(g(c"vkFreeMemory")),
            std::mem::transmute::<
                _,
                unsafe extern "system" fn(VkDevice, *const VkAllocationCallbacks),
            >(g(c"vkDestroyDevice")),
        )
    };

    let mut queue: VkQueue = core::ptr::null_mut();
    // SAFETY: `queue` is a live local.
    unsafe { get_queue(device, 0, 0, &raw mut queue) };
    assert!(!queue.is_null(), "vkGetDeviceQueue must return a dispatchable handle");

    // -- a submit and a wait ------------------------------------------------
    // An empty submit: nothing executes, and design.md D15 makes submit
    // asynchronous anyway, so what this checks is that the call reaches us with
    // its arguments intact and comes back.
    let mut submit = VkSubmitInfo::default();
    submit.s_type = VkStructureType::VK_STRUCTURE_TYPE_SUBMIT_INFO;
    // SAFETY: one `VkSubmitInfo` and a null fence, which is legal.
    check(unsafe { queue_submit(queue, 1, &raw const submit, 0) }, "vkQueueSubmit");
    // SAFETY: a live device.
    check(unsafe { device_wait_idle(device) }, "vkDeviceWaitIdle");

    // -- arena memory, and a pointer into it --------------------------------
    const SIZE: VkDeviceSize = 64 * 1024;
    let mut ai = VkMemoryAllocateInfo::default();
    ai.s_type = VkStructureType::VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
    ai.allocation_size = SIZE;
    ai.memory_type_index = arena_type as u32;

    let mut memory: VkDeviceMemory = 0;
    // SAFETY: live locals.
    check(unsafe { allocate_memory(device, &raw const ai, core::ptr::null(), &raw mut memory) },
        "vkAllocateMemory");
    assert_ne!(memory, 0);

    let mut p: *mut c_void = core::ptr::null_mut();
    // SAFETY: live locals; `VK_WHOLE_SIZE` is the documented spelling for "the
    // rest of the allocation".
    check(unsafe { map_memory(device, memory, 0, u64::MAX, VkMemoryMapFlags(0), &raw mut p) },
        "vkMapMemory");
    assert!(!p.is_null(), "docs/design.md §5.3: vkMapMemory returns a real pointer into the arena");

    // Writing through the mapping writes the shared file. That is the property
    // the whole design rests on, and here it is only checked as far as this
    // process can see — the GPU side importing the same pages is step 4 and
    // needs `sv-gpu-server`.
    // SAFETY: `p` is valid for `SIZE` bytes, per the call that returned it.
    unsafe {
        let bytes = core::slice::from_raw_parts_mut(p as *mut u8, SIZE as usize);
        bytes.fill(0xAB);
        bytes[0] = 0x5C;
        assert_eq!(bytes[0], 0x5C);
        assert_eq!(bytes[SIZE as usize - 1], 0xAB);
    }

    // A second allocation must not overlap the first.
    let mut memory2: VkDeviceMemory = 0;
    // SAFETY: live locals.
    check(unsafe { allocate_memory(device, &raw const ai, core::ptr::null(), &raw mut memory2) },
        "vkAllocateMemory (second)");
    let mut p2: *mut c_void = core::ptr::null_mut();
    // SAFETY: live locals.
    check(unsafe { map_memory(device, memory2, 0, u64::MAX, VkMemoryMapFlags(0), &raw mut p2) },
        "vkMapMemory (second)");
    let (a, b) = (p as usize, p2 as usize);
    assert!(
        a + SIZE as usize <= b || b + SIZE as usize <= a,
        "two allocations must not overlap in the arena"
    );
    // SAFETY: `p2` is valid for `SIZE` bytes.
    unsafe {
        (p2 as *mut u8).write(0x11);
        assert_eq!((p as *mut u8).read(), 0x5C, "the second allocation overwrote the first");
    }

    // -- teardown -----------------------------------------------------------
    // SAFETY: live handles, destroyed once each, in the order the specification
    // requires.
    unsafe {
        unmap_memory(device, memory);
        unmap_memory(device, memory2);
        free_memory(device, memory, core::ptr::null());
        free_memory(device, memory2, core::ptr::null());
        destroy_device(device, core::ptr::null());
    }

    type PfnDestroyInstance =
        unsafe extern "system" fn(VkInstance, *const VkAllocationCallbacks);
    // SAFETY: the specification's signature.
    let destroy_instance: PfnDestroyInstance =
        unsafe { loader.get(instance, c"vkDestroyInstance").unwrap() };
    // SAFETY: a live instance, destroyed once.
    unsafe { destroy_instance(instance, core::ptr::null()) };
}

/// The loader must not be able to get a device-level entry point out of a NULL
/// instance.
///
/// If it can, the loader has a pointer it will eventually call with no dispatch
/// table behind it, and the fault will be somewhere else entirely.
#[test]
fn a_null_instance_answers_only_for_global_commands() {
    let Some(loader) = Loader::open() else { return };
    let null: VkInstance = core::ptr::null_mut();
    // SAFETY: a NULL instance is exactly what this call is specified to accept.
    unsafe {
        assert!(loader.get::<*const c_void>(null, c"vkCreateInstance").is_some());
        assert!(loader.get::<*const c_void>(null, c"vkEnumerateInstanceVersion").is_some());
        assert!(
            loader.get::<*const c_void>(null, c"vkEnumerateInstanceExtensionProperties").is_some()
        );
        assert!(loader.get::<*const c_void>(null, c"vkCmdDraw").is_none());
        assert!(loader.get::<*const c_void>(null, c"vkCreateDevice").is_none());
        assert!(loader.get::<*const c_void>(null, c"vkNotARealFunction").is_none());
    }
}
