//! What the generated `unsafe` actually does, checked against handmade C
//! structures.
//!
//! This is the file to run under Miri. `sv-proto` is `#![forbid(unsafe_code)]`
//! and its round-trip tests already run clean; everything in the project that
//! reads an application's memory is generated into `sv-icd-entry`, and until
//! this test existed none of it had been executed by anything but the loader,
//! where a mistake is a fault with no line number.
//!
//! ```text
//! cargo +nightly miri test -p sv-icd-entry --test lowering
//! ```
//!
//! Miri can model all of it: the structures are ordinary Rust allocations laid
//! out `#[repr(C)]`, the pointers into them are ordinary raw pointers, and the
//! reads the generated code performs are the reads a C caller's would be. What
//! it cannot model is a genuinely foreign pointer — an application built by a C
//! compiler — and no tool can.

// A Vulkan create-info is `Default` plus the three or four fields under test;
// naming all fourteen would bury them.
#![allow(clippy::field_reassign_with_default)]

use core::ffi::c_void;
use sv_icd_entry::*;

/// Lowering the structure every application passes first.
///
/// Exercises, in one value: a nested `Optional` struct (`pApplicationInfo`), two
/// C strings inside it, an array of C strings with a sibling count
/// (`ppEnabledExtensionNames` / `enabledExtensionCount`), an empty array with a
/// zero count, and a null `pNext`.
#[test]
fn an_instance_create_info_lowers_field_for_field() {
    let app_name = c"an application";
    let engine = c"an engine";
    let mut app = VkApplicationInfo::default();
    app.s_type = VkStructureType::VK_STRUCTURE_TYPE_APPLICATION_INFO;
    app.p_application_name = app_name.as_ptr();
    app.application_version = 7;
    app.p_engine_name = engine.as_ptr();
    app.engine_version = 9;
    app.api_version = (1 << 22) | (3 << 12);

    let ext_a = c"VK_KHR_surface";
    let ext_b = c"VK_EXT_headless_surface";
    let exts = [ext_a.as_ptr(), ext_b.as_ptr()];

    let mut ci = VkInstanceCreateInfo::default();
    ci.s_type = VkStructureType::VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
    ci.p_application_info = &raw const app;
    ci.enabled_extension_count = exts.len() as u32;
    ci.pp_enabled_extension_names = exts.as_ptr();

    // SAFETY: `ci` and everything it points at are live locals.
    let w = unsafe { lower_vk_instance_create_info(&raw const ci) }.expect("lowering");

    let a = w.p_application_info.expect("pApplicationInfo is not null");
    assert_eq!(a.p_application_name, app_name.to_bytes());
    assert_eq!(a.p_engine_name, engine.to_bytes());
    assert_eq!(a.application_version, 7);
    assert_eq!(a.api_version, (1 << 22) | (3 << 12));

    assert_eq!(w.enabled_extension_count, 2);
    assert_eq!(w.pp_enabled_extension_names.len(), 2);
    assert_eq!(w.pp_enabled_extension_names[0], ext_a.to_bytes());
    assert_eq!(w.pp_enabled_extension_names[1], ext_b.to_bytes());

    // The layer array was null with a zero count and must lower to nothing
    // rather than to a read.
    assert_eq!(w.enabled_layer_count, 0);
    assert!(w.pp_enabled_layer_names.is_empty());
    assert!(w.p_next.is_empty());
}

/// A null optional pointer is `None`, not a dereference.
#[test]
fn a_null_optional_is_not_read() {
    let ci = VkInstanceCreateInfo::default();
    // SAFETY: `ci` is a live local with every pointer null.
    let w = unsafe { lower_vk_instance_create_info(&raw const ci) }.expect("lowering");
    assert!(w.p_application_info.is_none());
    assert!(w.pp_enabled_extension_names.is_empty());
}

/// An array parameter's count comes from its `len=` sibling, and a count of zero
/// means no read at all.
///
/// This is invariant 2 of the generator's header, and it is the invariant that
/// bounds a read of the *application's* memory. `sv-proto`'s wire counts cannot
/// help here — there is no wire yet.
#[test]
fn an_array_is_bounded_by_its_sibling_count() {
    let priorities = [1.0f32, 0.5, 0.25];
    let mut q = VkDeviceQueueCreateInfo::default();
    q.s_type = VkStructureType::VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
    q.queue_count = 1;
    q.queue_family_index = 0;
    // The registry says `len="queueCount"` for `pQueuePriorities`, so exactly
    // one of the three must be read even though three are there.
    q.p_queue_priorities = priorities.as_ptr();

    let mut ci = VkDeviceCreateInfo::default();
    ci.s_type = VkStructureType::VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
    ci.queue_create_info_count = 1;
    ci.p_queue_create_infos = &raw const q;

    // SAFETY: live locals; `queueCount` is 1 and the array has 3 elements.
    let w = unsafe { lower_vk_device_create_info(&raw const ci) }.expect("lowering");
    assert_eq!(w.p_queue_create_infos.len(), 1);
    assert_eq!(
        w.p_queue_create_infos[0].p_queue_priorities,
        vec![1.0f32],
        "len= is what bounds the read, not the array we happened to allocate"
    );
}

/// The `pNext` walk: known links are lowered, unknown ones are dropped, and a
/// cycle is an error rather than a hang.
#[test]
fn a_pnext_chain_is_walked_leniently_and_bounded() {
    // A link this build knows, and one it does not.
    let mut known = VkPhysicalDeviceVulkan11Features::default();
    known.s_type = VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES;
    known.multiview = 1;

    #[repr(C)]
    struct Unknown {
        s_type: VkStructureType,
        p_next: *const c_void,
        _payload: [u64; 4],
    }
    let mut unknown = Unknown {
        // A value from an extension no build of ours has heard of.
        s_type: VkStructureType(1_000_999_000),
        p_next: core::ptr::null(),
        _payload: [0; 4],
    };

    known.p_next = &raw mut unknown as *mut c_void;

    // SAFETY: a live chain of two links.
    let links = unsafe {
        lower_pnext(&raw const known as *const c_void, |s| {
            // Accept both, so that the *lowering* decides what it knows rather
            // than the membership predicate.
            s == VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES.0
                || s == 1_000_999_000
        })
    }
    .expect("the chain is walked");
    assert_eq!(links.len(), 1, "the unknown link is dropped and the walk continues");
    assert_eq!(
        links[0].s_type(),
        VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES.0
    );

    // A chain that points at itself must be an error, not a hang. The
    // specification puts no bound on chain length, so this bound is ours.
    //
    // Built through a heap allocation rather than by taking the address of a
    // local and then writing to the local: the second form makes the raw pointer
    // and the named place two aliases, and the write through the name
    // invalidates the pointer. Miri catches it, correctly — and it is the
    // *test's* bug, not the walker's.
    let mut link = VkPhysicalDeviceVulkan11Features::default();
    link.s_type = VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES;
    let cyclic = Box::into_raw(Box::new(link));
    // SAFETY: a fresh allocation this test owns; every access goes through the
    // one raw pointer.
    let r = unsafe {
        (*cyclic).p_next = cyclic as *mut c_void;
        let r = lower_pnext(cyclic as *const c_void, |_| true);
        drop(Box::from_raw(cyclic));
        r
    };
    assert_eq!(r.unwrap_err(), LowerError::ChainTooLong);

    // A null head is an empty chain.
    // SAFETY: null is the case under test.
    assert!(unsafe { lower_pnext(core::ptr::null(), |_| true) }.unwrap().is_empty());
}

/// Writeback into the application's storage, including a fixed array of structs.
#[test]
fn memory_properties_are_written_back_field_for_field() {
    let mut src = sv_proto::VkPhysicalDeviceMemoryProperties::default();
    src.memory_heap_count = 2;
    src.memory_heaps[0] =
        sv_proto::VkMemoryHeap { size: 1234, flags: sv_proto::VkMemoryHeapFlags(1) };
    src.memory_heaps[1] =
        sv_proto::VkMemoryHeap { size: 5678, flags: sv_proto::VkMemoryHeapFlags(0) };
    src.memory_type_count = 3;
    src.memory_types[1] =
        sv_proto::VkMemoryType { property_flags: sv_proto::VkMemoryPropertyFlags(6), heap_index: 1 };

    let mut dst = VkPhysicalDeviceMemoryProperties::default();
    // SAFETY: `dst` is a live local of the matching C type.
    unsafe { store_vk_physical_device_memory_properties(&src, &raw mut dst) };

    assert_eq!(dst.memory_heap_count, 2);
    assert_eq!(dst.memory_heaps[0].size, 1234);
    assert_eq!(dst.memory_heaps[0].flags.0, 1);
    assert_eq!(dst.memory_heaps[1].size, 5678);
    assert_eq!(dst.memory_type_count, 3);
    assert_eq!(dst.memory_types[1].property_flags.0, 6);
    assert_eq!(dst.memory_types[1].heap_index, 1);
}

// ---------------------------------------------------------------------------
// a whole stub, end to end, with no loader in the way
// ---------------------------------------------------------------------------

struct TestIcd;

impl IcdLocal for TestIcd {}

impl Icd for TestIcd {
    fn get_physical_device_memory_properties(
        &self,
        args: sv_proto::VkGetPhysicalDeviceMemoryPropertiesArgs,
    ) -> IcdResult<sv_proto::VkGetPhysicalDeviceMemoryPropertiesReply> {
        // The id the stub lowered out of the dispatchable handle is the whole
        // point of R3: what crosses is a table index, never a pointer.
        assert_eq!(args.physical_device.0, 0x1234, "the handle lowered to its wire id");
        let mut m = sv_proto::VkPhysicalDeviceMemoryProperties::default();
        m.memory_heap_count = 1;
        m.memory_heaps[0] =
            sv_proto::VkMemoryHeap { size: 42, flags: sv_proto::VkMemoryHeapFlags(1) };
        m.memory_type_count = 1;
        m.memory_types[0] = sv_proto::VkMemoryType {
            property_flags: sv_proto::VkMemoryPropertyFlags(6),
            heap_index: 0,
        };
        Ok(sv_proto::VkGetPhysicalDeviceMemoryPropertiesReply {
            p_memory_properties: Some(Box::new(m)),
        })
    }

    fn get_physical_device_queue_family_properties(
        &self,
        _args: sv_proto::VkGetPhysicalDeviceQueueFamilyPropertiesArgs,
    ) -> IcdResult<sv_proto::VkGetPhysicalDeviceQueueFamilyPropertiesReply> {
        let mut q = sv_proto::VkQueueFamilyProperties::default();
        q.queue_count = 3;
        Ok(sv_proto::VkGetPhysicalDeviceQueueFamilyPropertiesReply {
            p_queue_family_property_count: Some(2),
            p_queue_family_properties: vec![q.clone(), q],
        })
    }
}

static TEST_ICD: TestIcd = TestIcd;

/// The generated stub, the lowering, the trait call and the writeback, in one
/// call, with a real dispatchable handle.
#[test]
fn a_generated_stub_goes_all_the_way_round() {
    set_icd(&TEST_ICD);
    let handle = Dispatchable::new(0x1234, 2, ());
    let mut props = VkPhysicalDeviceMemoryProperties::default();
    // SAFETY: a handle this test allocated and a live output struct.
    unsafe {
        sv_vk_get_physical_device_memory_properties(
            handle as VkPhysicalDevice,
            &raw mut props,
        );
    }
    assert_eq!(props.memory_heap_count, 1);
    assert_eq!(props.memory_heaps[0].size, 42);
    assert_eq!(props.memory_types[0].property_flags.0, 6);

    // SAFETY: allocated above, reclaimed once.
    unsafe {
        let _ = Dispatchable::reclaim(handle);
    }
}

/// Venus's clamp (design.md §6.1), in the generated writeback.
///
/// "an unclamped write is a buffer overflow in our own code the moment either
/// side has a bug, and the clamp is one comparison". This checks the comparison
/// exists: the implementation offers two families and the caller has room for
/// one.
#[test]
fn the_count_then_fill_idiom_clamps_to_the_callers_buffer() {
    set_icd(&TEST_ICD);
    let handle = Dispatchable::new(0x1234, 2, ());

    // First call: null array, and the count comes back.
    let mut count = 0u32;
    // SAFETY: a live count and a null array, which is the documented first call.
    unsafe {
        sv_vk_get_physical_device_queue_family_properties(
            handle as VkPhysicalDevice,
            &raw mut count,
            core::ptr::null_mut(),
        );
    }
    assert_eq!(count, 2);

    // Second call with room for one. The second element must not be touched.
    let mut one = [VkQueueFamilyProperties::default(); 2];
    one[1].queue_count = 0xDEAD;
    let mut cap = 1u32;
    // SAFETY: `one` has two elements and we declare capacity for one, so a
    // correct implementation writes exactly one.
    unsafe {
        sv_vk_get_physical_device_queue_family_properties(
            handle as VkPhysicalDevice,
            &raw mut cap,
            one.as_mut_ptr(),
        );
    }
    assert_eq!(cap, 1, "the count that comes back is what was written");
    assert_eq!(one[0].queue_count, 3);
    assert_eq!(one[1].queue_count, 0xDEAD, "the caller's second element was written past");

    // SAFETY: allocated above, reclaimed once.
    unsafe {
        let _ = Dispatchable::reclaim(handle);
    }
}
