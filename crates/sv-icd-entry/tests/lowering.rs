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

/// A stand-in for `sv-icd`'s arena allocator: ids 100 and up are "arena-backed"
/// and rewrite to `(ARENA, 0x1000 * id + offset)`; ids below 100 are heap-0 and
/// cross unchanged; anything at or above 900 is unknown and must fail.
const ARENA: u64 = 0xFFFF_FFFF_FFFF_FFFE;

impl IcdLocal for TestIcd {
    fn rewrite_memory(&self, memory: u64, offset: u64) -> IcdResult<(u64, u64)> {
        match memory {
            m if m >= 900 => Err(LowerError::Shape("no such VkDeviceMemory")),
            m if m >= 100 => Ok((ARENA, 0x1000 * m + offset)),
            m => Ok((m, offset)),
        }
    }
}

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

/// **The arm `descriptorType` does not select must not be read at all.**
///
/// `VkWriteDescriptorSet` has three array pointers — `pImageInfo`,
/// `pBufferInfo`, `pTexelBufferView` — of which exactly one is used, and the
/// specification permits the other two to hold *anything*: an uninitialised
/// stack slot, a stale pointer, a pointer to an array of a different element
/// type or a shorter length. The registry marks all three
/// `noautovalidity="true"`, which is its way of saying a sibling decides and it
/// cannot express how.
///
/// The lowering read whichever were non-null, which is an out-of-bounds read of
/// the application's own memory. Live, not theoretical: it put a `VkBuffer` id
/// of `0xFFFFFFFF00000001` on the wire, and an id that fails to map is the
/// *lucky* case — one that collides with a live object hands the driver a
/// descriptor pointing at the wrong resource and hangs the GPU with no
/// diagnostic anywhere (design.md §0.16).
///
/// This is the negative control. The two unselected pointers here address a
/// single-element array while `descriptorCount` is 4, so a lowering that reads
/// them runs off the end — under Miri that is a hard error, and without Miri the
/// assertions below still fail, because the wire value would carry the poison
/// pattern instead of an empty array.
#[test]
fn an_unselected_descriptor_arm_is_never_read() {
    const POISON: u64 = 0xFFFF_FFFF_0000_0001;

    let images = [VkDescriptorImageInfo { sampler: 11, image_view: 12, image_layout: VkImageLayout(1) }; 4];
    // One element each, deliberately shorter than `descriptorCount`.
    let stale_buffer = [VkDescriptorBufferInfo { buffer: POISON, offset: 0, range: 0 }];
    let stale_view = [POISON];

    let mut w = VkWriteDescriptorSet::default();
    w.s_type = VkStructureType::VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
    w.dst_set = 5;
    w.descriptor_count = 4;
    w.descriptor_type = VkDescriptorType::VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE;
    w.p_image_info = images.as_ptr();
    w.p_buffer_info = stale_buffer.as_ptr();
    w.p_texel_buffer_view = stale_view.as_ptr();

    // SAFETY: `w` is a live local. `pImageInfo` addresses four elements, which
    // is what `descriptorCount` claims; the other two are the hazard under test
    // and a correct lowering does not dereference them.
    let lowered = unsafe { local::lower_write_descriptor_set_for_test(&raw const w) }
        .expect("a SAMPLED_IMAGE write lowers");
    assert_eq!(lowered.p_image_info.len(), 4, "the selected arm carries every element");
    assert!(lowered.p_buffer_info.is_empty(), "pBufferInfo is not selected by SAMPLED_IMAGE");
    assert!(lowered.p_texel_buffer_view.is_empty(), "pTexelBufferView is not selected either");

    // And the mirror: a buffer type reads the buffer arm and leaves the images.
    let mut w2 = VkWriteDescriptorSet::default();
    w2.s_type = VkStructureType::VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
    w2.descriptor_count = 1;
    w2.descriptor_type = VkDescriptorType::VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
    w2.p_image_info = images.as_ptr();
    w2.p_buffer_info = stale_buffer.as_ptr();
    w2.p_texel_buffer_view = stale_view.as_ptr();
    // SAFETY: as above; `pBufferInfo` addresses the one element it claims.
    let l2 = unsafe { local::lower_write_descriptor_set_for_test(&raw const w2) }
        .expect("a STORAGE_BUFFER write lowers");
    assert_eq!(l2.p_buffer_info.len(), 1);
    assert_eq!(l2.p_buffer_info[0].buffer.0, POISON, "the *selected* arm is read verbatim");
    assert!(l2.p_image_info.is_empty(), "pImageInfo is not selected by STORAGE_BUFFER");
}

// ---------------------------------------------------------------------------
// pass 6, app-side half: every memory id that crosses is rewritten
// ---------------------------------------------------------------------------

/// A transport that keeps the bytes instead of sending them.
///
/// The reply is this command's own opcode in a `ReplyHeader` followed by zeros:
/// the header has to match or `decode_reply_*` refuses it as somebody else's
/// answer, which is R13 doing its job. Zeros after it are `VK_SUCCESS` — enough
/// for a forwarder to return, and this test is about what went *out*.
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<u8>>);

impl Transport for Recorder {
    fn call(
        &self,
        _ring: RingClass,
        opcode: u32,
        command: &[u8],
        reply: &mut Vec<u8>,
    ) -> IcdResult<()> {
        *self.0.lock().unwrap() = command.to_vec();
        reply.resize(64, 0);
        let mut enc = sv_wire::Encoder::new(&mut reply[..8]);
        sv_wire::ReplyHeader { cmd_type: opcode, reply_size: 64 }
            .encode(&mut enc)
            .expect("a reply header fits in eight bytes");
        Ok(())
    }
    fn send(&self, _ring: RingClass, _opcode: u32, command: &[u8]) -> IcdResult<()> {
        *self.0.lock().unwrap() = command.to_vec();
        Ok(())
    }
}

fn sent(r: &Recorder) -> Vec<u8> {
    r.0.lock().unwrap().clone()
}

/// design.md §5.3's rewrite happens for **every** command that carries a memory
/// id, not for the one it was hand-written in.
///
/// `vkBindBufferMemory` had the rewrite and `vkBindImageMemory` did not, so an
/// image bound to host-visible memory sent an app-side id the GPU side has never
/// heard of; it answered `no VkDeviceMemory with id 8`, took the ring fatal, and
/// the device was lost (design.md §0.17). The two commands are asserted
/// together here precisely because their being different was the bug.
#[test]
fn every_forwarded_memory_pair_is_rewritten() {
    set_icd(&TEST_ICD);
    let t = Recorder::default();

    // The shape that was broken.
    forward::bind_image_memory(
        &t,
        sv_proto::VkBindImageMemoryArgs {
            device: sv_proto::VkDevice(1),
            image: sv_proto::VkImage(2),
            memory: sv_proto::VkDeviceMemory(100),
            memory_offset: 0x40,
        },
    )
    .expect("a bind of arena-backed memory forwards");
    let cmd = sent(&t);
    let mut d = sv_wire::Decoder::new(&cmd);
    sv_wire::CommandHeader::decode(&mut d).expect("a header");
    let a = sv_proto::decode_cmd_vk_bind_image_memory(&mut d).expect("the command decodes");
    assert_eq!(a.memory.0, ARENA, "the app-side id never reaches the wire");
    assert_eq!(a.memory_offset, 0x1000 * 100 + 0x40, "the offset is arena-absolute");

    // The shape that already worked, so that the two cannot drift apart again.
    forward::bind_buffer_memory(
        &t,
        sv_proto::VkBindBufferMemoryArgs {
            device: sv_proto::VkDevice(1),
            buffer: sv_proto::VkBuffer(3),
            memory: sv_proto::VkDeviceMemory(101),
            memory_offset: 8,
        },
    )
    .expect("a buffer bind forwards");
    let cmd = sent(&t);
    let mut d = sv_wire::Decoder::new(&cmd);
    sv_wire::CommandHeader::decode(&mut d).expect("a header");
    let a = sv_proto::decode_cmd_vk_bind_buffer_memory(&mut d).expect("the command decodes");
    assert_eq!(a.memory.0, ARENA);
    assert_eq!(a.memory_offset, 0x1000 * 101 + 8);

    // Heap-0 memory is a real GPU-side allocation under the app's own id and
    // must cross untouched — a rewrite that "helpfully" renamed it would break
    // the one case that was never broken.
    forward::bind_image_memory(
        &t,
        sv_proto::VkBindImageMemoryArgs {
            device: sv_proto::VkDevice(1),
            image: sv_proto::VkImage(2),
            memory: sv_proto::VkDeviceMemory(7),
            memory_offset: 0x40,
        },
    )
    .expect("a bind of device-local memory forwards");
    let cmd = sent(&t);
    let mut d = sv_wire::Decoder::new(&cmd);
    sv_wire::CommandHeader::decode(&mut d).expect("a header");
    let a = sv_proto::decode_cmd_vk_bind_image_memory(&mut d).expect("the command decodes");
    assert_eq!(a.memory.0, 7);
    assert_eq!(a.memory_offset, 0x40);
}

/// The same pass, reached through a struct and through an array of them.
///
/// `vkBindImageMemory2` and `vkQueueBindSparse` carry the pair inside
/// `VkBindImageMemoryInfo` and `VkSparseMemoryBind`, where no top-level rule can
/// see it, and `VK_NULL_HANDLE` in a sparse bind means "unbind this range" and
/// must survive the walk.
#[test]
fn a_memory_pair_inside_a_struct_is_rewritten_too() {
    set_icd(&TEST_ICD);
    let t = Recorder::default();

    let mut info = sv_proto::VkBindImageMemoryInfo::default();
    info.image = sv_proto::VkImage(2);
    info.memory = sv_proto::VkDeviceMemory(102);
    info.memory_offset = 0x20;
    forward::bind_image_memory2(
        &t,
        sv_proto::VkBindImageMemory2Args {
            device: sv_proto::VkDevice(1),
            bind_info_count: 1,
            p_bind_infos: vec![info],
        },
    )
    .expect("a bind2 forwards");
    let cmd = sent(&t);
    let mut d = sv_wire::Decoder::new(&cmd);
    sv_wire::CommandHeader::decode(&mut d).expect("a header");
    let a = sv_proto::decode_cmd_vk_bind_image_memory2(&mut d).expect("the command decodes");
    assert_eq!(a.p_bind_infos[0].memory.0, ARENA);
    assert_eq!(a.p_bind_infos[0].memory_offset, 0x1000 * 102 + 0x20);

    let mut null_bind = sv_proto::VkSparseMemoryBind::default();
    null_bind.memory = sv_proto::VkDeviceMemory(0);
    null_bind.memory_offset = 0;
    let mut bound = sv_proto::VkSparseMemoryBind::default();
    bound.memory = sv_proto::VkDeviceMemory(103);
    bound.memory_offset = 0x10;
    let mut buf = sv_proto::VkSparseBufferMemoryBindInfo::default();
    buf.buffer = sv_proto::VkBuffer(4);
    buf.bind_count = 2;
    buf.p_binds = vec![null_bind, bound];
    let mut bind = sv_proto::VkBindSparseInfo::default();
    bind.buffer_bind_count = 1;
    bind.p_buffer_binds = vec![buf];
    forward::queue_bind_sparse(
        &t,
        sv_proto::VkQueueBindSparseArgs {
            queue: sv_proto::VkQueue(1),
            bind_info_count: 1,
            p_bind_info: vec![bind],
            fence: sv_proto::VkFence(0),
        },
    )
    .expect("a sparse bind forwards");
    let cmd = sent(&t);
    let mut d = sv_wire::Decoder::new(&cmd);
    sv_wire::CommandHeader::decode(&mut d).expect("a header");
    let a = sv_proto::decode_cmd_vk_queue_bind_sparse(&mut d).expect("the command decodes");
    let binds = &a.p_bind_info[0].p_buffer_binds[0].p_binds;
    assert_eq!(binds[0].memory.0, 0, "VK_NULL_HANDLE means unbind, and is not looked up");
    assert_eq!(binds[1].memory.0, ARENA);
    assert_eq!(binds[1].memory_offset, 0x1000 * 103 + 0x10);
}

/// An id the app side never handed out fails **here**, with the id in the error.
///
/// The alternative is what this project measured: the raw id crosses, the GPU
/// side cannot resolve it, the ring goes fatal and the application sees
/// `VK_ERROR_DEVICE_LOST` from an unrelated `vkWaitForFences` several calls
/// later.
#[test]
fn an_unknown_memory_id_fails_before_it_crosses() {
    set_icd(&TEST_ICD);
    let t = Recorder::default();
    let e = forward::bind_image_memory(
        &t,
        sv_proto::VkBindImageMemoryArgs {
            device: sv_proto::VkDevice(1),
            image: sv_proto::VkImage(2),
            memory: sv_proto::VkDeviceMemory(999),
            memory_offset: 0,
        },
    )
    .expect_err("an unknown memory id does not cross");
    assert_eq!(e, LowerError::Wire(sv_wire::WireError::NoSuchMemory { id: 999 }));
    assert!(sent(&t).is_empty(), "and nothing was sent");
}
