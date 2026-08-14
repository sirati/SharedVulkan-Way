//! The cases a marshalling generator gets wrong, written by hand against real
//! Vulkan structures.
//!
//! The generated round-trip tests in `sv-proto` cover every struct, but they
//! cover it with a sampler, and a sampler only exercises the shapes it knows how
//! to build. These are the specific situations `docs/api-scale.md` names:
//!
//! > Structs … nest, they chain through `pNext`, they contain pointers to arrays
//! > whose lengths live in sibling fields, and some are read-write (the callee
//! > fills them in). A generator that handles commands but treats structs
//! > naively will produce something that appears to work and then corrupts
//! > memory under a real workload.
//!
//! Each test below is one clause of that sentence.

use sv_proto::*;

fn roundtrip<T: PartialEq + std::fmt::Debug>(
    value: &T,
    size: impl Fn(&T) -> usize,
    encode: impl Fn(&mut sv_wire::Encoder<'_>, &T) -> sv_wire::Result<()>,
    decode: impl Fn(&mut sv_wire::Decoder<'_>) -> sv_wire::Result<T>,
) -> Vec<u8> {
    let n = size(value);
    let mut buf = vec![0xAAu8; n];
    let mut enc = sv_wire::Encoder::new(&mut buf);
    encode(&mut enc, value).expect("encode");
    assert_eq!(enc.finish().expect("size and encode must agree"), n);

    let mut dec = sv_wire::Decoder::new(&buf);
    let back = decode(&mut dec).expect("decode");
    assert_eq!(dec.remaining(), 0, "decode left bytes behind");
    assert_eq!(*value, back, "value did not survive the round trip");
    buf
}

/// Nested structs: a create-info holding a struct holding an array.
#[test]
fn nested_structs_survive() {
    let v = VkBufferCreateInfo {
        p_next: Vec::new(),
        flags: VkBufferCreateFlags(0),
        size: 4096,
        usage: VkBufferUsageFlags::VK_BUFFER_USAGE_TRANSFER_SRC_BIT
            | VkBufferUsageFlags::VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
        sharing_mode: VkSharingMode::VK_SHARING_MODE_CONCURRENT,
        queue_family_index_count: 3,
        p_queue_family_indices: vec![0, 1, 7],
    };
    let bytes = roundtrip(
        &v,
        size_vk_buffer_create_info,
        encode_vk_buffer_create_info,
        decode_vk_buffer_create_info,
    );

    // The layout, checked against the table in `reference/venus/NOTES.md`
    // (derived from Venus's generated C for the same struct) with our two
    // deviations applied: an empty pNext chain is a 4-byte terminator rather
    // than Venus's 8-byte simple-pointer NULL, and padding is zeroed.
    assert_eq!(&bytes[0..4], &12i32.to_le_bytes(), "sType VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO");
    assert_eq!(&bytes[4..8], &0u32.to_le_bytes(), "empty pNext chain terminator");
    assert_eq!(&bytes[8..12], &0u32.to_le_bytes(), "flags");
    assert_eq!(&bytes[12..20], &4096u64.to_le_bytes(), "size is a VkDeviceSize, 8 bytes");
    assert_eq!(&bytes[28..32], &3u32.to_le_bytes(), "queueFamilyIndexCount");
    assert_eq!(&bytes[32..40], &3u64.to_le_bytes(), "the array's own u64 count");
    assert_eq!(&bytes[40..44], &0u32.to_le_bytes());
    assert_eq!(&bytes[44..48], &1u32.to_le_bytes());
    assert_eq!(&bytes[48..52], &7u32.to_le_bytes());
    assert_eq!(bytes.len(), 52);
}

/// An array whose length lives in a sibling member, and the sibling lying.
#[test]
fn an_array_length_that_disagrees_with_its_sibling_is_refused() {
    let mut v = VkBufferCreateInfo {
        p_next: Vec::new(),
        flags: VkBufferCreateFlags(0),
        size: 1,
        usage: VkBufferUsageFlags(0),
        sharing_mode: VkSharingMode(0),
        queue_family_index_count: 2, // a lie: the array has three
        p_queue_family_indices: vec![0, 1, 2],
    };
    let mut buf = vec![0u8; size_vk_buffer_create_info(&v)];
    let mut enc = sv_wire::Encoder::new(&mut buf);
    assert!(
        matches!(
            encode_vk_buffer_create_info(&mut enc, &v),
            Err(sv_wire::WireError::LengthDisagreement { .. })
        ),
        "a xxxCount that disagrees with its array must never reach the wire"
    );

    v.queue_family_index_count = 3;
    roundtrip(
        &v,
        size_vk_buffer_create_info,
        encode_vk_buffer_create_info,
        decode_vk_buffer_create_info,
    );
}

/// An *optional* array is allowed to be absent while its count is non-zero.
///
/// `VkSubpassDescription::colorAttachmentCount` governs three members, and a
/// subpass with no resolve attachments passes NULL for `pResolveAttachments`
/// while the count still describes `pColorAttachments`. Getting this wrong makes
/// the encoder reject perfectly ordinary render passes.
#[test]
fn an_optional_array_may_be_absent_while_its_count_is_not_zero() {
    let v = VkSubpassDescription {
        flags: VkSubpassDescriptionFlags(0),
        pipeline_bind_point: VkPipelineBindPoint::VK_PIPELINE_BIND_POINT_GRAPHICS,
        input_attachment_count: 0,
        p_input_attachments: Vec::new(),
        color_attachment_count: 2,
        p_color_attachments: vec![
            VkAttachmentReference { attachment: 0, layout: VkImageLayout(2) },
            VkAttachmentReference { attachment: 1, layout: VkImageLayout(2) },
        ],
        // Absent, though the count above says two.
        p_resolve_attachments: Vec::new(),
        p_depth_stencil_attachment: None,
        preserve_attachment_count: 0,
        p_preserve_attachments: Vec::new(),
    };
    roundtrip(
        &v,
        size_vk_subpass_description,
        encode_vk_subpass_description,
        decode_vk_subpass_description,
    );
}

/// Optional pointers, both ways round.
#[test]
fn optional_pointers_encode_their_absence() {
    let absent = VkSubpassDescription {
        flags: VkSubpassDescriptionFlags(0),
        pipeline_bind_point: VkPipelineBindPoint(0),
        input_attachment_count: 0,
        p_input_attachments: Vec::new(),
        color_attachment_count: 0,
        p_color_attachments: Vec::new(),
        p_resolve_attachments: Vec::new(),
        p_depth_stencil_attachment: None,
        preserve_attachment_count: 0,
        p_preserve_attachments: Vec::new(),
    };
    let a = roundtrip(
        &absent,
        size_vk_subpass_description,
        encode_vk_subpass_description,
        decode_vk_subpass_description,
    );

    let present = VkSubpassDescription {
        p_depth_stencil_attachment: Some(Box::new(VkAttachmentReference {
            attachment: 3,
            layout: VkImageLayout(1),
        })),
        ..absent.clone()
    };
    let b = roundtrip(
        &present,
        size_vk_subpass_description,
        encode_vk_subpass_description,
        decode_vk_subpass_description,
    );

    // The presence flag is eight bytes and the pointee follows it.
    assert_eq!(b.len(), a.len() + 8, "an attachment reference is two u32s");
}

/// `pNext` chains: a link, an unknown link, and the asymmetric handling of
/// design.md §6.1.
#[test]
fn pnext_chains_carry_links_and_terminate() {
    let v = VkBufferCreateInfo {
        p_next: vec![PNextLink::VkExternalMemoryBufferCreateInfo(Box::new(
            VkExternalMemoryBufferCreateInfo {
                p_next: Vec::new(),
                handle_types: VkExternalMemoryHandleTypeFlags(0x80),
            },
        ))],
        flags: VkBufferCreateFlags(0),
        size: 64,
        usage: VkBufferUsageFlags(0),
        sharing_mode: VkSharingMode(0),
        queue_family_index_count: 0,
        p_queue_family_indices: Vec::new(),
    };
    let bytes = roundtrip(
        &v,
        size_vk_buffer_create_info,
        encode_vk_buffer_create_info,
        decode_vk_buffer_create_info,
    );

    // The link is length-prefixed, which is the whole point of preferring
    // gfxstream's encoding to Venus's (design.md D6): four bytes of length, then
    // the sType, then the body, then a zero terminator.
    let link_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    assert_eq!(link_size, 8, "sType plus one u32 of body");
    let s_type = i32::from_le_bytes(bytes[8..12].try_into().unwrap());
    assert_eq!(s_type, 1000072000, "VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO");
    assert_eq!(&bytes[12..16], &0x80u32.to_le_bytes(), "handleTypes");
    assert_eq!(&bytes[16..20], &0u32.to_le_bytes(), "chain terminator");
}

/// The app side keeps a link it does not know; the GPU side refuses it.
///
/// > gfxstream's reason was client-lenient/server-strict; ours is that a fatal
/// > abort turns a silent protocol mismatch into a loud one.
/// > — design.md §6.1
#[test]
fn an_unknown_pnext_link_is_kept_by_the_app_side_and_fatal_on_the_gpu_side() {
    // Hand-build a chain whose single link has an sType nothing knows.
    let mut buf = vec![0u8; 20];
    {
        let mut enc = sv_wire::Encoder::new(&mut buf);
        enc.u32(12).unwrap(); // link_size: sType + two u32 of body
        enc.i32(0x7FFF_0000).unwrap(); // an sType from a future extension
        enc.u32(0xDEAD_BEEF).unwrap();
        enc.u32(0x0BAD_F00D).unwrap();
        enc.u32(0).unwrap(); // terminator
        enc.finish().unwrap();
    }

    let mut lenient = sv_wire::Decoder::lenient(&buf);
    let links = decode_pnext(&mut lenient, pnext_allowed_in_vk_buffer_create_info)
        .expect("the app side keeps what it does not understand");
    assert_eq!(links.len(), 1);
    match &links[0] {
        PNextLink::Unknown { s_type, body } => {
            assert_eq!(*s_type, 0x7FFF_0000);
            assert_eq!(body.len(), 8);
        }
        other => panic!("expected an Unknown link, got {other:?}"),
    }

    // …and re-encoding it reproduces the bytes exactly, so a link travelling
    // through the app side is not damaged by passing through.
    let mut out = vec![0u8; size_pnext(&links)];
    let mut enc = sv_wire::Encoder::new(&mut out);
    encode_pnext(&mut enc, &links).unwrap();
    enc.finish().unwrap();
    assert_eq!(out, buf);

    let mut strict = sv_wire::Decoder::new(&buf);
    assert!(
        matches!(
            decode_pnext(&mut strict, pnext_allowed_in_vk_buffer_create_info),
            Err(sv_wire::WireError::UnknownPNext { s_type: 0x7FFF_0000 })
        ),
        "the GPU side must refuse a link it does not know"
    );
}

/// A read-write (callee-filled) parameter: out on the way there, full on the
/// way back.
#[test]
fn a_callee_filled_parameter_goes_out_partial_and_comes_back_whole() {
    let args = VkGetBufferMemoryRequirementsArgs {
        device: VkDevice(1),
        buffer: VkBuffer(2),
        // The app side sends storage it has not filled in.
        p_memory_requirements: Some(Box::new(VkMemoryRequirements::default())),
    };
    let n = size_cmd_vk_get_buffer_memory_requirements(&args);
    let mut buf = vec![0u8; n];
    let mut enc = sv_wire::Encoder::new(&mut buf);
    encode_cmd_vk_get_buffer_memory_requirements(
        &mut enc,
        sv_wire::CMD_FLAG_GENERATE_REPLY,
        &args,
    )
    .unwrap();
    enc.finish().unwrap();

    // `VkMemoryRequirements` is entirely implementation-filled, so its partial
    // form carries nothing but its presence flag: the command is a header, two
    // handles and that flag, and none of the uninitialised contents cross.
    assert_eq!(
        n,
        sv_wire::COMMAND_HEADER_SIZE + 8 + 8 + 8,
        "an uninitialised out-parameter must not put its contents on the wire"
    );

    let reply = VkGetBufferMemoryRequirementsReply {
        p_memory_requirements: Some(Box::new(VkMemoryRequirements {
            size: 4096,
            alignment: 256,
            memory_type_bits: 0b1011,
        })),
    };
    let n = size_reply_vk_get_buffer_memory_requirements(&reply);
    let mut buf = vec![0u8; n];
    let mut enc = sv_wire::Encoder::new(&mut buf);
    encode_reply_vk_get_buffer_memory_requirements(&mut enc, &reply).unwrap();
    enc.finish().unwrap();
    let mut dec = sv_wire::Decoder::new(&buf);
    let back = decode_reply_vk_get_buffer_memory_requirements(&mut dec).unwrap();
    assert_eq!(dec.remaining(), 0);
    assert_eq!(reply, back, "the reply carries the whole struct");
}

/// The count-then-fill idiom, which is the other read-write shape.
#[test]
fn the_count_then_fill_idiom_round_trips() {
    let args = VkEnumeratePhysicalDevicesArgs {
        instance: VkInstance(1),
        // The caller's capacity.
        p_physical_device_count: Some(4),
        p_physical_devices: vec![VkPhysicalDevice(0); 4],
    };
    let n = size_cmd_vk_enumerate_physical_devices(&args);
    let mut buf = vec![0u8; n];
    let mut enc = sv_wire::Encoder::new(&mut buf);
    encode_cmd_vk_enumerate_physical_devices(&mut enc, 0, &args).unwrap();
    enc.finish().unwrap();

    let mut dec = sv_wire::Decoder::new(&buf[sv_wire::COMMAND_HEADER_SIZE..]);
    let back = decode_cmd_vk_enumerate_physical_devices(&mut dec).unwrap();
    assert_eq!(dec.remaining(), 0);
    assert_eq!(back.p_physical_device_count, Some(4));
    assert_eq!(back.p_physical_devices.len(), 4);
}

/// A count that the peer inflates must not become an allocation.
///
/// `security-model.md` R2: bounds-check every offset and count, because "the
/// realistic failure it catches is a generator bug producing a wrong `len=`".
#[test]
fn an_inflated_count_from_the_peer_is_refused() {
    let args = VkEnumeratePhysicalDevicesArgs {
        instance: VkInstance(1),
        p_physical_device_count: Some(2),
        p_physical_devices: vec![VkPhysicalDevice(0); 2],
    };
    let n = size_cmd_vk_enumerate_physical_devices(&args);
    let mut buf = vec![0u8; n];
    let mut enc = sv_wire::Encoder::new(&mut buf);
    encode_cmd_vk_enumerate_physical_devices(&mut enc, 0, &args).unwrap();
    enc.finish().unwrap();

    // Find the array's u64 count — it follows the header, the instance handle,
    // and the presence-flag-plus-value of the count parameter — and inflate it.
    let at = sv_wire::COMMAND_HEADER_SIZE + 8 + 8 + 4;
    assert_eq!(u64::from_le_bytes(buf[at..at + 8].try_into().unwrap()), 2);
    buf[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());

    let mut dec = sv_wire::Decoder::new(&buf[sv_wire::COMMAND_HEADER_SIZE..]);
    assert!(
        decode_cmd_vk_enumerate_physical_devices(&mut dec).is_err(),
        "a count of u64::MAX must be an error, not an allocation"
    );
}

/// A string, including the case that is easy to get wrong: the terminator is
/// part of the format, not of the value.
#[test]
fn strings_carry_their_terminator_but_do_not_store_it() {
    let v = VkApplicationInfo {
        p_next: Vec::new(),
        p_application_name: b"triangle".to_vec(),
        application_version: 1,
        p_engine_name: b"".to_vec(),
        engine_version: 0,
        api_version: 0x0040_3000,
    };
    let bytes =
        roundtrip(&v, size_vk_application_info, encode_vk_application_info, decode_vk_application_info);
    // "triangle" is 8 bytes plus a NUL, padded to 12, after an 8-byte length.
    let name_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    assert_eq!(name_len, 9, "the length includes the NUL");
    assert_eq!(&bytes[16..24], b"triangle");
    assert_eq!(bytes[24], 0, "and the NUL is written");
}

/// The whole protocol, through the dispatcher, with no Vulkan anywhere.
#[test]
fn a_command_reaches_a_server_and_a_reply_comes_back() {
    struct Fake;
    impl Fake {
        fn reply() -> VkCreateBufferReply {
            VkCreateBufferReply { result: VkResult(0), p_buffer: Some(VkBuffer(0xABCD)) }
        }
    }

    let args = VkCreateBufferArgs {
        device: VkDevice(1),
        p_create_info: Some(Box::new(VkBufferCreateInfo {
            p_next: Vec::new(),
            flags: VkBufferCreateFlags(0),
            size: 1024,
            usage: VkBufferUsageFlags(1),
            sharing_mode: VkSharingMode(0),
            queue_family_index_count: 0,
            p_queue_family_indices: Vec::new(),
        })),
        // The id the app side chose, sent with the request: object creation
        // costs no round trip (design.md §6.4).
        p_buffer: Some(VkBuffer(0x1234)),
    };
    let n = size_cmd_vk_create_buffer(&args);
    let mut buf = vec![0u8; n];
    let mut enc = sv_wire::Encoder::new(&mut buf);
    encode_cmd_vk_create_buffer(&mut enc, sv_wire::CMD_FLAG_GENERATE_REPLY, &args).unwrap();
    enc.finish().unwrap();

    let mut dec = sv_wire::Decoder::new(&buf[sv_wire::COMMAND_HEADER_SIZE..]);
    let decoded = decode_cmd_vk_create_buffer(&mut dec).unwrap();
    assert_eq!(decoded.p_buffer, Some(VkBuffer(0x1234)));
    assert_eq!(decoded.p_create_info.as_ref().unwrap().size, 1024);

    let reply = Fake::reply();
    let mut rbuf = vec![0u8; size_reply_vk_create_buffer(&reply)];
    let mut enc = sv_wire::Encoder::new(&mut rbuf);
    encode_reply_vk_create_buffer(&mut enc, &reply).unwrap();
    enc.finish().unwrap();
    let mut dec = sv_wire::Decoder::new(&rbuf);
    assert_eq!(decode_reply_vk_create_buffer(&mut dec).unwrap(), reply);
}

/// The handle-map pass walks a struct's handles — and **leaves
/// `VkDeviceMemory` alone**, because that one belongs to the transform pass.
///
/// # Why the two passes have to partition the handle space
///
/// design.md §7.2 lists `handle_map` (pass 5) and `transform` (pass 6) side by
/// side and does not say which owns a `VkDeviceMemory`. Both did, and the result
/// is specific: an app-side memory id is not a table entry at all — it is either
/// `ARENA_MEMORY_ID`, a sentinel with no driver handle behind it, or an offset
/// into a slab — so the handle map fails to resolve it and takes the ring fatal
/// on the first `vkBindBufferMemory` an application makes. Zink's first buffer
/// found it.
///
/// Pass 6 rewrites the *pair*, which is the only operation that means anything
/// on a memory id: without its offset there is nothing to resolve.
#[test]
fn the_handle_map_pass_leaves_device_memory_to_the_transform_pass() {
    struct Plus(u64);
    impl HandleMapper for Plus {
        fn map(&mut self, _ty: HandleType, id: u64) -> sv_wire::Result<u64> {
            Ok(id + self.0)
        }
    }

    let mut v = VkBindBufferMemoryInfo {
        p_next: Vec::new(),
        buffer: VkBuffer(10),
        memory: VkDeviceMemory(20),
        memory_offset: 4096,
    };
    handle_map_vk_bind_buffer_memory_info(&mut v, &mut Plus(1000)).unwrap();
    assert_eq!(v.buffer, VkBuffer(1010), "an ordinary handle is mapped");
    assert_eq!(
        v.memory,
        VkDeviceMemory(20),
        "a VkDeviceMemory is not, because only the transform pass can resolve one"
    );
    assert_eq!(v.memory_offset, 4096, "and its offset is untouched with it");
}

/// The transform pass rewrites `(memory, offset)` together.
#[test]
fn the_transform_pass_rewrites_memory_and_offset_as_a_pair() {
    /// A stand-in for the slab allocator: every app-side allocation lives at
    /// offset 0x10000 of slab 7 (design.md §5.3).
    struct Slab;
    impl MemoryTransform for Slab {
        fn rewrite(&mut self, _memory: u64, offset: u64) -> sv_wire::Result<(u64, u64)> {
            Ok((7, 0x10000 + offset))
        }
    }

    let mut v = VkBindBufferMemoryInfo {
        p_next: Vec::new(),
        buffer: VkBuffer(1),
        memory: VkDeviceMemory(42),
        memory_offset: 256,
    };
    transform_vk_bind_buffer_memory_info(&mut v, &mut Slab).unwrap();
    assert_eq!(v.memory, VkDeviceMemory(7));
    assert_eq!(v.memory_offset, 0x10000 + 256);
    assert_eq!(v.buffer, VkBuffer(1), "the transform pass touches only memory");
}

/// The protocol hash is a fact about this build, and the manifest explains it.
#[test]
fn the_handshake_carries_a_hash_over_a_readable_manifest() {
    assert_ne!(sv_proto::PROTOCOL_HASH, 0);
    // Which profile, not *the* profile. `SV_PROTO_PROFILE` selects it and
    // `full` is a supported build (§0.19), so pinning the string to `zink` made
    // this fail for a reason that has nothing to do with the handshake. What is
    // worth asserting is that the manifest names the profile it was generated
    // from — a hash with no readable explanation is the thing §4.3 is against.
    assert!(
        sv_codegen::PROFILE_NAMES.contains(&sv_proto::PROTOCOL_PROFILE),
        "PROTOCOL_PROFILE {:?} is not a profile this generator knows",
        sv_proto::PROTOCOL_PROFILE
    );
    assert!(sv_proto::PROTOCOL_MANIFEST.contains("command vkCreateBuffer"));
    assert!(sv_proto::PROTOCOL_MANIFEST.contains("struct VkBufferCreateInfo"));
}
