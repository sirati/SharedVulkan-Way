//! `vkCreateDescriptorSetLayout`, and the one shape Intel's Windows driver must
//! never see (docs/design.md §0.29).
//!
//! # The fault
//!
//! Intel's Windows driver (`igvk64.dll` 101.7076, Raptor Lake-P UHD Graphics)
//! read address 0x8 inside `vkCreatePipelineLayout` at `igvk64.dll+0xdb8d3`, in
//! three of four Firefox runs on the real guest. `layout-probe` (on the
//! `deploy-guest` branch) narrowed it down, directly on that driver:
//!
//! | variation | set layouts, in order | result |
//! |---|---|---|
//! | `zink` | push (5 UBOs), empty, texel; every one with an all-zero binding-flags link | fault |
//! | `zink-no-pnext` | the same, no set with the link | ok |
//! | `push-empty` | push, empty; both with the link | fault |
//! | `push-texel` | push, texel; both with the link | fault |
//! | `empty-texel` | empty, texel; both with the link | ok |
//! | `plain-empty-texel` | an ordinary UBO set, empty, texel; all with the link | ok |
//!
//! So the fault needs a PUSH_DESCRIPTOR set layout that was created with a
//! `VkDescriptorSetLayoutBindingFlagsCreateInfo`, first in a pipeline layout
//! with another set after it. The flags were all zero. `push-only` and
//! `empty-only` never ran: the VM stopped first.
//!
//! # The transformation, and why it is exact
//!
//! **An all-zero `VkDescriptorSetLayoutBindingFlagsCreateInfo` is dropped from
//! every set layout, on every driver.** The specification says what its absence
//! means: "If `bindingCount` is zero or if this structure is not included in the
//! `pNext` chain, the `VkDescriptorBindingFlags` for each descriptor set layout
//! binding is considered to be zero." A link whose flags are all zero states
//! exactly that, so the layout the driver makes is the layout the application
//! asked for. Nothing else in the chain moves.
//!
//! Every set layout, not only the push one, because then the three-set layout
//! Firefox faulted on reaches the driver as `zink-no-pnext`, the variation
//! measured *not* to fault. Dropping the link from the push set alone would
//! leave a shape (push without, empty and texel with) that `layout-probe` never
//! made. Every driver, because it is exact, and because it is then exercised
//! on every driver this project runs on rather than only on the one it cannot
//! test on any more.
//!
//! # Non-zero flags on a push set
//!
//! `VUID-VkDescriptorSetLayoutBindingFlagsCreateInfo-flags-03003` forbids
//! UPDATE_AFTER_BIND, UPDATE_UNUSED_WHILE_PENDING and VARIABLE_DESCRIPTOR_COUNT
//! on a push-descriptor set layout, which leaves PARTIALLY_BOUND as the one
//! flag such a link may legally carry. Dropping that is **not** exact: it
//! relaxes what the application may leave invalid, and without it a statically
//! used descriptor that the application left unwritten is undefined behaviour
//! the application did not write. So on Intel's Windows driver a push set layout
//! with any non-zero binding flag is **refused** — `VK_ERROR_INITIALIZATION_FAILED`
//! and a line on stderr — because whether that shape faults is not known and
//! finding out costs the VM. On every other driver it is forwarded as it came.
//! Zink never sends it: its push layouts' flags are zeroed in a loop
//! (`descriptor_layout_create` in `zink_descriptors.c`, Mesa 26.2.4), and its only
//! non-zero flags are on the bindless layout, which is not a push one.

// The wire structs are `Default` plus assignment, for the reason `crate::convert`
// gives.
#![allow(clippy::field_reassign_with_default)]

use sv_gpu_entry::{VkAllocationCallbacks, VkDescriptorSetLayout, VkDescriptorSetLayoutCreateInfo, VkDevice};
use sv_proto as p;

/// `VK_DESCRIPTOR_SET_LAYOUT_CREATE_PUSH_DESCRIPTOR_BIT` (`_KHR`).
pub const PUSH_DESCRIPTOR: u32 = 0x1;

/// PCI vendor id of Intel.
pub const VENDOR_INTEL: u32 = 0x8086;

/// `VK_DRIVER_ID_INTEL_PROPRIETARY_WINDOWS`. The guest's driver reported it,
/// forwarded, in the deploy-guest API dump: `driverID = 5`, driverName "Intel
/// Corporation", driverInfo "101.7076".
pub const DRIVER_ID_INTEL_PROPRIETARY_WINDOWS: i32 = 5;

/// Whether this is the driver that faults on a push set layout with a
/// binding-flags link — the one where a push set layout with non-zero flags is
/// refused rather than forwarded.
///
/// Vendor and driver id both, because the same iGPU under Linux is ANV
/// (`VK_DRIVER_ID_INTEL_OPEN_SOURCE_MESA`), which is a different driver and
/// was not observed to fault.
pub fn refuses_push_binding_flags(vendor_id: u32, driver_id: i32) -> bool {
    vendor_id == VENDOR_INTEL && driver_id == DRIVER_ID_INTEL_PROPRIETARY_WINDOWS
}

/// What [`lower`] did to a create info.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lowered {
    /// Nothing to change.
    Unchanged,
    /// An all-zero binding-flags link was taken out of the chain.
    DroppedZeroBindingFlags,
    /// Not to be forwarded; the reason is for stderr.
    Refused(String),
}

fn zero(l: &p::PNextLink) -> Option<bool> {
    match l {
        p::PNextLink::VkDescriptorSetLayoutBindingFlagsCreateInfo(x) => {
            Some(x.p_binding_flags.iter().all(|f| f.0 == 0))
        }
        _ => None,
    }
}

/// Shape a set layout's create info for the driver, in place. See the module
/// documentation for what and why.
///
/// `refuse_push_flags` is [`refuses_push_binding_flags`] of the open driver.
pub fn lower(info: &mut p::VkDescriptorSetLayoutCreateInfo, refuse_push_flags: bool) -> Lowered {
    let mut dropped = false;
    let mut i = 0;
    while i < info.p_next.len() {
        if zero(&info.p_next[i]) == Some(true) {
            // The wire's chain is flat (sv_codegen::emit::pnext), so a link's
            // own `p_next` is empty; if it ever is not, what it held stays
            // where it was rather than going with the link.
            if let p::PNextLink::VkDescriptorSetLayoutBindingFlagsCreateInfo(x) = info.p_next.remove(i) {
                let rest = x.p_next;
                let n = rest.len();
                info.p_next.splice(i..i, rest);
                i += n;
            }
            dropped = true;
        } else {
            i += 1;
        }
    }
    let push = info.flags.0 & PUSH_DESCRIPTOR != 0;
    if push
        && refuse_push_flags
        && let Some(p::PNextLink::VkDescriptorSetLayoutBindingFlagsCreateInfo(x)) =
            info.p_next.iter().find(|l| zero(l) == Some(false))
    {
        let flags: Vec<String> = x.p_binding_flags.iter().map(|f| format!("{:#x}", f.0)).collect();
        return Lowered::Refused(format!(
            "a PUSH_DESCRIPTOR set layout with binding flags [{}]: Intel's Windows driver \
             faults in vkCreatePipelineLayout on a push set layout created with binding \
             flags, and dropping non-zero ones is not exact (docs/design.md §0.29)",
            flags.join(", ")
        ));
    }
    if dropped {
        Lowered::DroppedZeroBindingFlags
    } else {
        Lowered::Unchanged
    }
}

/// The driver's `vkCreateDescriptorSetLayout`, as `sv_gpu_entry::DriverFns` types it.
pub type CreateFn = unsafe extern "system" fn(
    VkDevice,
    *const VkDescriptorSetLayoutCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkDescriptorSetLayout,
) -> sv_gpu_entry::VkResult;

/// Lower `info`, then call the driver with it unless it was refused.
///
/// Returns the `VkResult` and the driver's handle (0 if none). Separate from
/// `GpuServer` so that a test can hand it a driver of its own and look at
/// exactly what a driver would have been given.
pub fn create(
    f: CreateFn,
    device: u64,
    info: &mut p::VkDescriptorSetLayoutCreateInfo,
    refuse_push_flags: bool,
) -> sv_wire::Result<(i32, u64)> {
    if let Lowered::Refused(why) = lower(info, refuse_push_flags) {
        eprintln!("[gpu] vkCreateDescriptorSetLayout refused: {why}");
        return Ok((VK_ERROR_INITIALIZATION_FAILED, 0));
    }
    let mut pool = sv_gpu_entry::Raise::default();
    let ci = sv_gpu_entry::raise_vk_descriptor_set_layout_create_info(info, &mut pool)?;
    let mut layout: VkDescriptorSetLayout = 0;
    // SAFETY: `ci` and every pointer inside it are owned by `pool`, which
    // outlives the call; `layout` is a live local; a null allocator is always
    // valid; `device` is the driver handle the caller's map produced, and `f`
    // is that driver's entry point (or, in a test, a function of that type).
    let r = unsafe { f(device as usize as VkDevice, &ci, core::ptr::null(), &mut layout) };
    Ok((r.0, layout))
}

/// `VK_ERROR_INITIALIZATION_FAILED`, the answer this side gives for a command it
/// will not forward (the same one `note_missing`'s callers give).
pub const VK_ERROR_INITIALIZATION_FAILED: i32 = -3;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use sv_gpu_entry::{VkBaseInStructure, VkDescriptorSetLayoutBindingFlagsCreateInfo, VkResult};

    const BINDING_FLAGS_STYPE: i32 = 1000161000;
    const PARTIALLY_BOUND: u32 = 0x4;
    const UNIFORM_BUFFER: i32 = 6;
    const UNIFORM_TEXEL_BUFFER: i32 = 4;

    /// What the mock driver was handed, one entry per call.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Seen {
        flags: u32,
        bindings: Vec<(u32, i32, u32)>,
        chain: Vec<i32>,
        binding_flags: Option<Vec<u32>>,
    }

    thread_local! {
        static CALLS: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    }

    /// A driver that records the create info it was given, read the way a
    /// driver reads it: through the C pointers.
    unsafe extern "system" fn mock_driver(
        _device: VkDevice,
        ci: *const VkDescriptorSetLayoutCreateInfo,
        _alloc: *const VkAllocationCallbacks,
        out: *mut VkDescriptorSetLayout,
    ) -> VkResult {
        let seen = unsafe {
            let ci = &*ci;
            let bindings = (0..ci.binding_count as usize)
                .map(|i| {
                    let b = &*ci.p_bindings.add(i);
                    (b.binding, b.descriptor_type.0, b.descriptor_count)
                })
                .collect();
            let mut chain = Vec::new();
            let mut binding_flags = None;
            let mut l = ci.p_next as *const VkBaseInStructure;
            while !l.is_null() {
                chain.push((*l).s_type.0);
                if (*l).s_type.0 == BINDING_FLAGS_STYPE {
                    let bf = &*(l as *const VkDescriptorSetLayoutBindingFlagsCreateInfo);
                    binding_flags =
                        Some((0..bf.binding_count as usize).map(|i| (*bf.p_binding_flags.add(i)).0).collect());
                }
                l = (*l).p_next;
            }
            Seen { flags: ci.flags.0, bindings, chain, binding_flags }
        };
        CALLS.with(|c| c.borrow_mut().push(seen));
        unsafe { *out = 0x5e7 };
        VkResult(0)
    }

    fn take_calls() -> Vec<Seen> {
        CALLS.with(|c| std::mem::take(&mut *c.borrow_mut()))
    }

    fn binding(n: u32, ty: i32, stage: u32) -> p::VkDescriptorSetLayoutBinding {
        let mut b = p::VkDescriptorSetLayoutBinding::default();
        b.binding = n;
        b.descriptor_type = p::VkDescriptorType(ty);
        b.descriptor_count = 1;
        b.stage_flags = p::VkShaderStageFlags(stage);
        b
    }

    fn flags_link(flags: &[u32]) -> p::PNextLink {
        let mut x = p::VkDescriptorSetLayoutBindingFlagsCreateInfo::default();
        x.binding_count = flags.len() as u32;
        x.p_binding_flags = flags.iter().map(|f| p::VkDescriptorBindingFlags(*f)).collect();
        p::PNextLink::VkDescriptorSetLayoutBindingFlagsCreateInfo(Box::new(x))
    }

    fn layout(flags: u32, bindings: Vec<p::VkDescriptorSetLayoutBinding>, link: Option<&[u32]>) -> p::VkDescriptorSetLayoutCreateInfo {
        let mut ci = p::VkDescriptorSetLayoutCreateInfo::default();
        ci.flags = p::VkDescriptorSetLayoutCreateFlags(flags);
        ci.binding_count = bindings.len() as u32;
        ci.p_bindings = bindings;
        if let Some(f) = link {
            ci.p_next.push(flags_link(f));
        }
        ci
    }

    /// The three set layouts Firefox's Zink created before the faulting
    /// `vkCreatePipelineLayout` on the guest, as the deploy-guest API dump
    /// records them (handles 0xc, 0xe, 0x6f).
    fn firefox_sets() -> Vec<p::VkDescriptorSetLayoutCreateInfo> {
        let push = layout(
            PUSH_DESCRIPTOR,
            (0..5).map(|i| binding(i, UNIFORM_BUFFER, 1 << i)).collect(),
            Some(&[0, 0, 0, 0, 0]),
        );
        let empty = layout(0, vec![], Some(&[]));
        let texel = layout(0, vec![binding(128, UNIFORM_TEXEL_BUFFER, 0x10)], Some(&[0]));
        vec![push, empty, texel]
    }

    #[test]
    fn firefox_s_pipeline_layout_reaches_the_driver_as_the_variation_that_did_not_fault() {
        // On Intel's Windows driver, where the refusal is armed too.
        for mut ci in firefox_sets() {
            let (r, h) = create(mock_driver, 1, &mut ci, true).unwrap();
            assert_eq!((r, h), (0, 0x5e7));
        }
        let calls = take_calls();
        assert_eq!(calls.len(), 3, "every set layout reached the driver");
        for c in &calls {
            assert!(c.chain.is_empty(), "no set layout carries a pNext chain: {c:?}");
            assert_eq!(c.binding_flags, None);
        }
        // `zink-no-pnext`: push first with its five UBOs, then the empty set,
        // then the texel set, none of them with a binding-flags link.
        assert_eq!(calls[0].flags, PUSH_DESCRIPTOR);
        assert_eq!(calls[0].bindings, (0..5).map(|i| (i, UNIFORM_BUFFER, 1)).collect::<Vec<_>>());
        assert_eq!((calls[1].flags, calls[1].bindings.len()), (0, 0));
        assert_eq!(calls[2].bindings, vec![(128, UNIFORM_TEXEL_BUFFER, 1)]);
    }

    #[test]
    fn the_same_on_every_other_driver() {
        for mut ci in firefox_sets() {
            create(mock_driver, 1, &mut ci, false).unwrap();
        }
        assert!(take_calls().iter().all(|c| c.chain.is_empty()));
    }

    #[test]
    fn a_push_set_with_partially_bound_never_reaches_intel_s_windows_driver() {
        let mut ci = layout(PUSH_DESCRIPTOR, vec![binding(0, UNIFORM_BUFFER, 1), binding(1, UNIFORM_BUFFER, 16)], Some(&[0, PARTIALLY_BOUND]));
        let (r, h) = create(mock_driver, 1, &mut ci, true).unwrap();
        assert_eq!((r, h), (VK_ERROR_INITIALIZATION_FAILED, 0));
        assert!(take_calls().is_empty(), "the driver was not called");
    }

    #[test]
    fn a_push_set_with_partially_bound_is_forwarded_unchanged_elsewhere() {
        let mut ci = layout(PUSH_DESCRIPTOR, vec![binding(0, UNIFORM_BUFFER, 1), binding(1, UNIFORM_BUFFER, 16)], Some(&[0, PARTIALLY_BOUND]));
        let (r, _) = create(mock_driver, 1, &mut ci, false).unwrap();
        assert_eq!(r, 0);
        let calls = take_calls();
        assert_eq!(calls[0].chain, vec![BINDING_FLAGS_STYPE]);
        assert_eq!(calls[0].binding_flags, Some(vec![0, PARTIALLY_BOUND]));
    }

    #[test]
    fn non_zero_flags_on_an_ordinary_set_are_forwarded_on_intel_too() {
        // Zink's bindless layout: UPDATE_AFTER_BIND | UPDATE_UNUSED_WHILE_PENDING
        // | PARTIALLY_BOUND, and not a push set. Not the measured fault's shape,
        // and not this workaround's business.
        let mut ci = layout(0x2, vec![binding(0, 1, 0x3f)], Some(&[7]));
        let (r, _) = create(mock_driver, 1, &mut ci, true).unwrap();
        assert_eq!(r, 0);
        let calls = take_calls();
        assert_eq!(calls[0].binding_flags, Some(vec![7]));
    }

    #[test]
    fn other_links_keep_their_place() {
        let mut ci = layout(PUSH_DESCRIPTOR, vec![binding(0, UNIFORM_BUFFER, 1)], None);
        ci.p_next.push(p::PNextLink::VkMutableDescriptorTypeCreateInfoEXT(Box::default()));
        ci.p_next.push(flags_link(&[0]));
        ci.p_next.push(p::PNextLink::VkMutableDescriptorTypeCreateInfoEXT(Box::default()));
        assert_eq!(lower(&mut ci, true), Lowered::DroppedZeroBindingFlags);
        assert_eq!(ci.p_next.len(), 2);
        assert!(ci.p_next.iter().all(|l| matches!(l, p::PNextLink::VkMutableDescriptorTypeCreateInfoEXT(_))));
    }

    #[test]
    fn a_layout_without_the_link_is_unchanged() {
        let mut ci = layout(PUSH_DESCRIPTOR, vec![binding(0, UNIFORM_BUFFER, 1)], None);
        assert_eq!(lower(&mut ci, true), Lowered::Unchanged);
    }

    #[test]
    fn a_duplicate_link_cannot_smuggle_flags_past_the_refusal() {
        // Two links of one sType is invalid usage; the second must not reach
        // Intel's driver either.
        let mut ci = layout(PUSH_DESCRIPTOR, vec![binding(0, UNIFORM_BUFFER, 1)], Some(&[0]));
        ci.p_next.push(flags_link(&[PARTIALLY_BOUND]));
        assert!(matches!(lower(&mut ci, true), Lowered::Refused(_)));
    }

    #[test]
    fn only_intel_s_windows_driver_is_singled_out() {
        assert!(refuses_push_binding_flags(0x8086, 5));
        assert!(!refuses_push_binding_flags(0x8086, 6), "ANV");
        assert!(!refuses_push_binding_flags(0x1002, 3), "RADV");
        assert!(!refuses_push_binding_flags(0x10de, 4), "NVIDIA");
    }
}
