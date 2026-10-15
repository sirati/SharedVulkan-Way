//! The `pNext` walk has to fit on the thread an application calls it from.
//!
//! docs/design.md §0.27. Firefox calls `vkCreateDevice` — through Mesa's Zink,
//! from `kopper_init_screen` — on WebRender's `Renderer` thread, an `nsThread`
//! with **256 KiB** of stack. In a debug build `lower_pnext` alone was a 143 KB
//! frame, and Zink's device chain (`VkDeviceCreateInfo` → `VkPhysicalDevice-
//! Features2` → the feature structs) put two of them on the stack at once: the
//! second one's stack probe faulted ~254 KiB below the thread's first frame.
//!
//! These tests run the same shape of chain on a thread with a quarter of that,
//! in whatever profile `cargo test` builds — which is the debug one, the profile
//! every deployment script ships. Before §0.27 they did not fail: they
//! overflowed, and the Rust runtime aborted the whole test binary, which is why
//! they live in a file of their own.
//!
//! What is measured is the walker, not Firefox: the budget leaves the 256 KiB
//! thread three quarters of its stack for Gecko, Mesa and the loader, which
//! used about 12 KiB of it in the core §0.27 read.

#![allow(clippy::field_reassign_with_default)]

use core::ffi::c_void;
use sv_icd_entry::*;

/// A quarter of a Firefox `nsThread`'s stack.
const STACK: usize = 64 * 1024;

fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name("sv-small-stack".into())
        .stack_size(STACK)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("the walk returned rather than unwinding")
}

/// The links of a Zink-shaped `vkCreateDevice` chain, owned so that a thread
/// can build and lower it. Boxed, so the addresses stay put while linked.
struct ZinkChain {
    create: Box<VkDeviceCreateInfo>,
    _features2: Box<VkPhysicalDeviceFeatures2>,
    _v11: Box<VkPhysicalDeviceVulkan11Features>,
    _v12: Box<VkPhysicalDeviceVulkan12Features>,
    _v13: Box<VkPhysicalDeviceVulkan13Features>,
    _clip: Box<VkPhysicalDeviceDepthClipEnableFeaturesEXT>,
}

/// The sTypes of [`ZinkChain`]'s links, in chain order.
const ORDER: [VkStructureType; 5] = [
    VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
    VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES,
    VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
    VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
    VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DEPTH_CLIP_ENABLE_FEATURES_EXT,
];

/// `VkDeviceCreateInfo` → `VkPhysicalDeviceFeatures2` → three feature structs
/// it is the parent of. The second link being a *parent* of the rest is what
/// made the old walk nest; the depth-clip link is the one the §0.24 fault was
/// lowering when it died.
fn zink_chain() -> ZinkChain {
    let mut clip = Box::new(VkPhysicalDeviceDepthClipEnableFeaturesEXT::default());
    clip.s_type = ORDER[4];
    clip.depth_clip_enable = 1;

    let mut v13 = Box::new(VkPhysicalDeviceVulkan13Features::default());
    v13.s_type = ORDER[3];
    v13.dynamic_rendering = 1;
    v13.p_next = &raw mut *clip as *mut c_void;

    let mut v12 = Box::new(VkPhysicalDeviceVulkan12Features::default());
    v12.s_type = ORDER[2];
    v12.timeline_semaphore = 1;
    v12.p_next = &raw mut *v13 as *mut c_void;

    let mut v11 = Box::new(VkPhysicalDeviceVulkan11Features::default());
    v11.s_type = ORDER[1];
    v11.multiview = 1;
    v11.p_next = &raw mut *v12 as *mut c_void;

    let mut features2 = Box::new(VkPhysicalDeviceFeatures2::default());
    features2.s_type = ORDER[0];
    features2.features.geometry_shader = 1;
    features2.p_next = &raw mut *v11 as *mut c_void;

    let mut create = Box::new(VkDeviceCreateInfo::default());
    create.s_type = VkStructureType::VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
    create.p_next = &raw const *features2 as *const c_void;

    ZinkChain { create, _features2: features2, _v11: v11, _v12: v12, _v13: v13, _clip: clip }
}

/// Zink's device chain lowers on a quarter of a WebRender thread's stack, and
/// it lowers **flat**: one list in chain order, no link carrying a tail of its
/// own. The flatness is the wire's contract (links encode with their `_self`
/// form) and it is also what keeps the walk from nesting.
#[test]
fn zinks_device_chain_lowers_flat_on_a_quarter_of_a_webrender_stack() {
    let wire = on_small_stack(|| {
        let chain = zink_chain();
        // SAFETY: a live chain of `#[repr(C)]` structs this closure owns.
        unsafe { lower_vk_device_create_info(&raw const *chain.create) }
            .expect("the chain lowers")
    });
    let tags: Vec<i32> = wire.p_next.iter().map(|l| l.s_type()).collect();
    assert_eq!(tags, ORDER.map(|s| s.0).to_vec(), "every link, in chain order");

    for link in &wire.p_next {
        let tail = match link {
            sv_proto::PNextLink::VkPhysicalDeviceFeatures2(l) => {
                assert!(l.features.geometry_shader);
                l.p_next.len()
            }
            sv_proto::PNextLink::VkPhysicalDeviceVulkan11Features(l) => {
                assert!(l.multiview);
                l.p_next.len()
            }
            sv_proto::PNextLink::VkPhysicalDeviceVulkan12Features(l) => {
                assert!(l.timeline_semaphore);
                l.p_next.len()
            }
            sv_proto::PNextLink::VkPhysicalDeviceVulkan13Features(l) => {
                assert!(l.dynamic_rendering);
                l.p_next.len()
            }
            sv_proto::PNextLink::VkPhysicalDeviceDepthClipEnableFeaturesEXT(l) => {
                assert!(l.depth_clip_enable);
                l.p_next.len()
            }
            other => panic!("a link nobody put in the chain: {other:?}"),
        };
        assert_eq!(tail, 0, "{:#x} carries a tail of its own", link.s_type());
    }
}

/// The same chain's wire form decodes on the same budget — `decode_pnext` is
/// the frame §0.24 measured at 180 KB and this build measured at 145 KB again.
#[test]
fn the_chain_round_trips_through_the_wire_on_the_same_budget() {
    let chain = zink_chain();
    // SAFETY: as above.
    let wire = unsafe { lower_vk_device_create_info(&raw const *chain.create) }.unwrap();
    let links = wire.p_next;

    let back = on_small_stack(move || {
        let mut buf = vec![0u8; sv_proto::size_pnext(&links)];
        let mut enc = sv_wire::Encoder::new(&mut buf);
        sv_proto::encode_pnext(&mut enc, &links).expect("encodes");
        enc.finish().expect("exactly the size computed");
        let mut dec = sv_wire::Decoder::new(&buf);
        let back = sv_proto::decode_pnext(&mut dec, sv_proto::pnext_allowed_in_vk_device_create_info)
            .expect("decodes");
        (links, back)
    });
    assert_eq!(back.0, back.1);
}

/// The output direction: `vkGetPhysicalDeviceFeatures2` takes the same chain as
/// storage, and the tag-only walk was a 64 KB frame of its own.
#[test]
fn the_chain_as_output_storage_gives_up_its_tags_on_the_same_budget() {
    let tags = on_small_stack(|| {
        let chain = zink_chain();
        // SAFETY: as above; the tag walk reads two fields of each link.
        unsafe {
            lower_pnext_tags(
                chain.create.p_next,
                sv_proto::pnext_allowed_in_vk_device_create_info,
            )
        }
        .unwrap()
        .iter()
        .map(|l| l.s_type())
        .collect::<Vec<_>>()
    });
    assert_eq!(tags, ORDER.map(|s| s.0).to_vec());
}
