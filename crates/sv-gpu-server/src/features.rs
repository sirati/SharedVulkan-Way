//! Which device features the GPU side enables, and whether that covers what an
//! application asks for.
//!
//! # Why this is a module and not a list in `driver.rs`
//!
//! The GPU side opens its device before any application connects
//! ([`crate::driver`]'s header), so the application's `VkDeviceCreateInfo`
//! arrives after the device exists and cannot change it. What the device
//! enables therefore has to cover, up front, every feature an application could
//! be told is supported, and an application is told by the driver itself:
//! `vkGetPhysicalDeviceFeatures2` is forwarded, so every struct the driver
//! knows comes back filled in.
//!
//! `driver.rs` used to chain a hand-kept list of two dozen extension feature
//! structs, and the list fell behind the profile. Zink, told that
//! `VK_EXT_extended_dynamic_state3`'s features were supported, enabled them and
//! recorded `vkCmdSetPolygonModeEXT` and its siblings into a device that had
//! never enabled one of them — `VUID-vkCmdSet*-None-09423`, about forty
//! messages per command per run (docs/video-decode.md §7.10.4), and undefined
//! behaviour on any driver. EDS3 was the one Zink happened to use; the class
//! was every feature struct of an enabled extension that nobody had typed in.
//!
//! So [`device_chain`] reads the registry's relation instead
//! (`sv_gpu_entry::DEVICE_FEATURE_STRUCTS`, generated): every struct that extends
//! both `VkPhysicalDeviceFeatures2` and `VkDeviceCreateInfo`, chained when an
//! extension that provides it is enabled. And [`not_enabled`] checks each
//! application's `VkDeviceCreateInfo` against what was enabled, so a feature
//! the device lacks is `VK_ERROR_FEATURE_NOT_PRESENT` at `vkCreateDevice` — the
//! specification's answer — rather than undefined behaviour at the first
//! command that uses it.
//!
//! # Why the device enables a superset, not exactly what was asked
//!
//! Exactly the requested set would need the device created at the
//! application's `vkCreateDevice`, and on the export arena path the arena is a
//! driver allocation made before any application attaches. Enabling a feature
//! the driver supports and nobody uses is legal; *using* one that was not
//! enabled is not, and that is the direction this module closes.

use sv_proto::PNextLink;

/// The core blocks, chained whenever the device's version has them. A struct a
/// core version up to the device's provides is covered by one of them, and
/// chaining it beside its block is invalid (`VUID-VkDeviceCreateInfo-pNext-02829`,
/// `-02830`, `-06532` and their 1.4 counterpart). The profile's core version is 1.4, and Zink
/// chains `VkPhysicalDeviceVulkan14Features` when the advertised version is.
const CORE_BLOCKS: [(&str, u32); 4] = [
    ("VkPhysicalDeviceVulkan11Features", 2),
    ("VkPhysicalDeviceVulkan12Features", 2),
    ("VkPhysicalDeviceVulkan13Features", 3),
    ("VkPhysicalDeviceVulkan14Features", 4),
];

/// Feature structs the device deliberately leaves off, and so never advertises
/// either ([`mask_to_enabled`]).
///
/// `VK_EXT_descriptor_buffer`: no application has used it through the
/// forwarder — Zink did not, while it was advertised and never enabled (a
/// descriptor-buffer command would have drawn a validation message, and the
/// 120 s soak of docs/video-decode.md §7.11 had none) — and a descriptor
/// buffer is driver-written bytes holding the GPU side's own addresses, which
/// nothing here has tested. Enabling it is also
/// `VUID-VkDeviceCreateInfo-None-08095` on RADV, beside
/// `VK_AMD_shader_fragment_mask`. Left off, Zink keeps the path every earlier
/// run measured.
pub const WITHHELD_STRUCTS: &[&str] = &["VkPhysicalDeviceDescriptorBufferFeaturesEXT"];

/// `VK_VERSION_1_x` → `x`, and the same for vk.xml 1.4.350's split of each
/// core version into `VK_BASE_VERSION_1_x`, `VK_COMPUTE_VERSION_1_x` and
/// `VK_GRAPHICS_VERSION_1_x`, which is where most promoted structs now live.
fn core_minor(provider: &str) -> Option<u32> {
    let rest = provider.strip_prefix("VK_")?;
    let (_, minor) = rest.split_once("VERSION_1_")?;
    minor.parse().ok()
}

/// The feature structs a device of core version `1.minor` with `extensions`
/// enabled is created with, every member false, ready for
/// `vkGetPhysicalDeviceFeatures2` to fill in.
///
/// One link per `sType` (an extension struct promoted to a KHR or core name
/// keeps its `sType`, and the registry lists it once).
pub fn device_chain(minor: u32, extensions: &[&str]) -> Vec<PNextLink> {
    let mut chain = Vec::new();
    let mut block_names = Vec::new();
    for (name, from) in CORE_BLOCKS {
        if minor >= from {
            block_names.push(name);
        }
    }
    let covered_by_core = |providers: &[&str]| {
        minor >= 2 && providers.iter().filter_map(|p| core_minor(p)).any(|m| (1..=minor).contains(&m))
    };
    let mut seen = std::collections::BTreeSet::new();
    for s in &sv_gpu_entry::DEVICE_FEATURE_STRUCTS {
        let chained = if WITHHELD_STRUCTS.contains(&s.name) {
            false
        } else if block_names.contains(&s.name) {
            true
        } else if covered_by_core(s.providers) {
            false
        } else {
            s.providers.iter().any(|p| core_minor(p).is_none() && extensions.contains(p))
        };
        if chained
            && seen.insert(s.s_type)
            && let Some(link) = sv_gpu_entry::device_feature_link(s.s_type)
        {
            chain.push(link);
        }
    }
    chain
}

/// Pairs of features the specification forbids enabling together, as
/// `(keep, drop)`: when the driver supports both, the device keeps the first.
///
/// `VK_KHR_fragment_shading_rate` against `VK_NV_shading_rate_image` and
/// `VK_EXT_fragment_density_map` (`VUID-VkDeviceCreateInfo-shadingRateImage-04478`
/// to `-04480`, `-fragmentDensityMap-04481` to `-04483`). The KHR one is kept
/// because it is the one Zink uses.
const EXCLUSIVE: &[(&str, &str)] = &[
    ("pipelineFragmentShadingRate", "shadingRateImage"),
    ("primitiveFragmentShadingRate", "shadingRateImage"),
    ("attachmentFragmentShadingRate", "shadingRateImage"),
    ("pipelineFragmentShadingRate", "fragmentDensityMap"),
    ("primitiveFragmentShadingRate", "fragmentDensityMap"),
    ("attachmentFragmentShadingRate", "fragmentDensityMap"),
];

/// Turn off the second of each [`EXCLUSIVE`] pair the driver answered true for
/// both of. Returns the names turned off.
pub fn resolve_exclusive(chain: &mut [PNextLink]) -> Vec<&'static str> {
    let on = |chain: &[PNextLink], want: &str| {
        chain.iter().any(|l| {
            sv_gpu_entry::feature_bits(l)
                .is_some_and(|b| b.iter().any(|(n, v)| *n == want && *v))
        })
    };
    let mut dropped = Vec::new();
    for &(keep, drop) in EXCLUSIVE {
        if on(chain, keep) && on(chain, drop) {
            for l in chain.iter_mut() {
                sv_gpu_entry::set_feature_bit(l, drop, false);
            }
            if !dropped.contains(&drop) {
                dropped.push(drop);
            }
        }
    }
    dropped
}

/// Ask the driver to fill in `template`'s chain: the GPU side's own
/// `vkGetPhysicalDeviceFeatures2`, through the generated raise and lower.
pub fn query_driver(
    instance: &ash::Instance,
    pd: ash::vk::PhysicalDevice,
    template: &sv_proto::VkPhysicalDeviceFeatures2,
) -> Result<sv_proto::VkPhysicalDeviceFeatures2, String> {
    let mut pool = sv_gpu_entry::Raise::default();
    let mut c = sv_gpu_entry::raise_vk_physical_device_features2(template, &mut pool)
        .map_err(|e| format!("raising the feature chain: {e:?}"))?;
    // `sv_gpu_entry`'s `VkPhysicalDeviceFeatures2` is the registry's layout,
    // which is `ash`'s, and every link of its chain lives in `pool`.
    unsafe { (instance.fp_v1_1().get_physical_device_features2)(pd, (&raw mut c).cast()) };
    unsafe { sv_gpu_entry::lower_full_vk_physical_device_features2(&raw const c) }
        .map_err(|e| format!("reading the feature chain back: {e:?}"))
}

/// Clear every member of `reported` that `enabled` does not have, so that what
/// an application is told is supported is what this device enabled.
///
/// The other half of [`not_enabled`]. The driver's answer is a superset of the
/// device's wherever the device deliberately left something off
/// ([`WITHHELD_STRUCTS`], [`EXCLUSIVE`]) or did not enable the extension a
/// struct belongs to, and an application told *supported* asks for it and is
/// refused at `vkCreateDevice` — or, before that check existed, used it. A
/// struct the device chained is matched by `sType`; one it did not (a promoted
/// struct its core block covers) member by member, by registry name.
/// `VkPhysicalDeviceFeatures` is left as reported: the device enables all of it.
pub fn mask_to_enabled(
    reported: &mut sv_proto::VkPhysicalDeviceFeatures2,
    enabled: &sv_proto::VkPhysicalDeviceFeatures2,
) {
    let any_on = |member: &str| {
        enabled.p_next.iter().any(|l| {
            sv_gpu_entry::feature_bits(l).is_some_and(|b| b.iter().any(|(n, v)| *n == member && *v))
        })
    };
    for link in reported.p_next.iter_mut() {
        let Some(bits) = sv_gpu_entry::feature_bits(link) else { continue };
        let s_type = link.s_type();
        let same = enabled.p_next.iter().find(|l| l.s_type() == s_type).and_then(sv_gpu_entry::feature_bits);
        for (member, on) in bits {
            if !on {
                continue;
            }
            let have = match &same {
                Some(b) => b.iter().any(|(n, v)| *n == member && *v),
                None => any_on(member),
            };
            if !have {
                sv_gpu_entry::set_feature_bit(link, member, false);
            }
        }
    }
}

/// The features `info` asks for that `enabled` (the device's own
/// `VkPhysicalDeviceFeatures2`, as `vkGetPhysicalDeviceFeatures2` answered and
/// [`resolve_exclusive`] left it) does not have, as `struct.member`.
///
/// A requested struct is matched by `sType` first. One the device did not
/// chain — a 1.2 struct such as `VkPhysicalDevice8BitStorageFeatures`, which
/// the device enables through `VkPhysicalDeviceVulkan12Features` — is matched
/// member by member against every chained struct, by registry name, which is
/// how the core blocks name the promoted members.
pub fn not_enabled(
    info: &sv_proto::VkDeviceCreateInfo,
    enabled: &sv_proto::VkPhysicalDeviceFeatures2,
) -> Vec<String> {
    let mut missing = Vec::new();
    let core_on = sv_gpu_entry::core_feature_bits(&enabled.features);
    let check_core = |asked: &sv_proto::VkPhysicalDeviceFeatures, missing: &mut Vec<String>| {
        for ((name, want), (_, have)) in sv_gpu_entry::core_feature_bits(asked).iter().zip(&core_on) {
            if *want && !*have {
                missing.push(format!("VkPhysicalDeviceFeatures.{name}"));
            }
        }
    };
    if let Some(f) = &info.p_enabled_features {
        check_core(f, &mut missing);
    }
    let any_on = |member: &str| {
        enabled.p_next.iter().any(|l| {
            sv_gpu_entry::feature_bits(l).is_some_and(|b| b.iter().any(|(n, v)| *n == member && *v))
        })
    };
    for link in &info.p_next {
        if let PNextLink::VkPhysicalDeviceFeatures2(f2) = link {
            check_core(&f2.features, &mut missing);
            continue;
        }
        let Some(asked) = sv_gpu_entry::feature_bits(link) else { continue };
        let s_type = link.s_type();
        let name = sv_gpu_entry::DEVICE_FEATURE_STRUCTS
            .iter()
            .find(|s| s.s_type == s_type)
            .map_or("?", |s| s.name);
        let same = enabled.p_next.iter().find(|l| l.s_type() == s_type);
        for (member, want) in asked {
            if !want {
                continue;
            }
            let have = match same.and_then(sv_gpu_entry::feature_bits) {
                Some(b) => b.iter().any(|(n, v)| *n == member && *v),
                None => any_on(member),
            };
            if !have {
                missing.push(format!("{name}.{member}"));
            }
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(chain: &[PNextLink]) -> Vec<&'static str> {
        chain
            .iter()
            .map(|l| {
                let s = l.s_type();
                sv_gpu_entry::DEVICE_FEATURE_STRUCTS.iter().find(|d| d.s_type == s).unwrap().name
            })
            .collect()
    }

    /// The regression this module exists for: an enabled extension's feature
    /// struct is chained without anyone naming it — EDS3 above all, which the
    /// hand-kept list in `driver.rs` lacked.
    #[test]
    fn every_enabled_extensions_feature_struct_is_chained() {
        let exts = [
            "VK_EXT_extended_dynamic_state3",
            "VK_EXT_extended_dynamic_state",
            "VK_KHR_video_maintenance1",
            "VK_EXT_robustness2",
        ];
        let chain = device_chain(3, &exts);
        let n = names(&chain);
        for want in [
            "VkPhysicalDeviceVulkan11Features",
            "VkPhysicalDeviceVulkan12Features",
            "VkPhysicalDeviceVulkan13Features",
            "VkPhysicalDeviceExtendedDynamicState3FeaturesEXT",
            "VkPhysicalDeviceExtendedDynamicStateFeaturesEXT",
            "VkPhysicalDeviceVideoMaintenance1FeaturesKHR",
        ] {
            assert!(n.contains(&want), "{want} is not chained: {n:?}");
        }
        // And nothing whose extension is not enabled.
        assert!(!n.contains(&"VkPhysicalDeviceExtendedDynamicState2FeaturesEXT"), "{n:?}");
    }

    /// Every device feature struct of every extension in the profile is
    /// reachable: enabling all of them chains one link per struct that some
    /// extension provides and no core version up to 1.3 covers.
    #[test]
    fn the_whole_profile_chains_every_extension_struct_once() {
        let exts: Vec<&str> = sv_gpu_entry::PROFILE_DEVICE_EXTENSIONS.to_vec();
        let chain = device_chain(3, &exts);
        let mut s_types: Vec<i32> = chain.iter().map(|l| l.s_type()).collect();
        let len = s_types.len();
        s_types.sort();
        s_types.dedup();
        assert_eq!(len, s_types.len(), "a struct is chained twice");
        for s in &sv_gpu_entry::DEVICE_FEATURE_STRUCTS {
            let by_ext = s.providers.iter().any(|p| exts.contains(p));
            let by_core = s.providers.iter().filter_map(|p| core_minor(p)).any(|m| m <= 3);
            if by_ext && !by_core && !WITHHELD_STRUCTS.contains(&s.name) {
                assert!(s_types.contains(&s.s_type), "{} is provided by an enabled extension and not chained", s.name);
            }
        }
    }

    /// A struct a core version covers is never chained beside its block —
    /// `VkPhysicalDevice8BitStorageFeatures` with `VkPhysicalDeviceVulkan12Features`
    /// is `VUID-VkDeviceCreateInfo-pNext-02830`.
    #[test]
    fn promoted_structs_are_left_to_their_core_block() {
        let chain = device_chain(3, &["VK_KHR_8bit_storage", "VK_KHR_maintenance4"]);
        let n = names(&chain);
        assert!(!n.contains(&"VkPhysicalDevice8BitStorageFeatures"), "{n:?}");
        assert!(!n.contains(&"VkPhysicalDeviceMaintenance4Features"), "{n:?}");
        assert_eq!(n.len(), 3, "{n:?}");
    }

    fn eds3(polygon_mode: bool) -> PNextLink {
        let mut l = sv_gpu_entry::device_feature_link(
            sv_gpu_entry::DEVICE_FEATURE_STRUCTS
                .iter()
                .find(|s| s.name == "VkPhysicalDeviceExtendedDynamicState3FeaturesEXT")
                .unwrap()
                .s_type,
        )
        .unwrap();
        assert!(sv_gpu_entry::set_feature_bit(&mut l, "extendedDynamicState3PolygonMode", polygon_mode));
        l
    }

    /// What Zink did: ask for an EDS3 member the device did not enable. That
    /// is named, struct and member, and a member it did enable is not.
    #[test]
    fn a_requested_feature_the_device_lacks_is_named() {
        let info = sv_proto::VkDeviceCreateInfo { p_next: vec![eds3(true)], ..Default::default() };
        let without = sv_proto::VkPhysicalDeviceFeatures2::default();
        assert_eq!(
            not_enabled(&info, &without),
            ["VkPhysicalDeviceExtendedDynamicState3FeaturesEXT.extendedDynamicState3PolygonMode"]
        );
        let with = sv_proto::VkPhysicalDeviceFeatures2 { p_next: vec![eds3(true)], ..Default::default() };
        assert!(not_enabled(&info, &with).is_empty());
        let off = sv_proto::VkDeviceCreateInfo { p_next: vec![eds3(false)], ..Default::default() };
        assert!(not_enabled(&off, &without).is_empty());
    }

    /// A promoted struct the application chains is satisfied by the core block
    /// the device enabled it through, member by member.
    #[test]
    fn a_promoted_struct_is_matched_against_its_core_block() {
        let link = |name: &str, member: &str| {
            let s = sv_gpu_entry::DEVICE_FEATURE_STRUCTS.iter().find(|s| s.name == name).unwrap();
            let mut l = sv_gpu_entry::device_feature_link(s.s_type).unwrap();
            assert!(sv_gpu_entry::set_feature_bit(&mut l, member, true), "{name}.{member}");
            l
        };
        let info = sv_proto::VkDeviceCreateInfo {
            p_next: vec![link("VkPhysicalDevice8BitStorageFeatures", "storageBuffer8BitAccess")],
            ..Default::default()
        };
        let enabled = sv_proto::VkPhysicalDeviceFeatures2 {
            p_next: vec![link("VkPhysicalDeviceVulkan12Features", "storageBuffer8BitAccess")],
            ..Default::default()
        };
        assert!(not_enabled(&info, &enabled).is_empty());
        let core = sv_proto::VkPhysicalDeviceFeatures { wide_lines: true, ..Default::default() };
        let info = sv_proto::VkDeviceCreateInfo { p_enabled_features: Some(Box::new(core)), ..Default::default() };
        assert_eq!(not_enabled(&info, &enabled), ["VkPhysicalDeviceFeatures.wideLines"]);
    }

    /// The whole regression on a real driver, the way Zink meets it: ask the
    /// driver about every device feature struct of every extension the device
    /// enabled (the app side's `vkGetPhysicalDeviceFeatures2` is forwarded, so
    /// this is what an application is told), ask for all of it at
    /// `vkCreateDevice`, and require that the device has every member. Before
    /// the chain came from the registry, `VK_EXT_extended_dynamic_state3`'s
    /// members failed this on RADV. Skipped where there is no Vulkan loader.
    #[test]
    fn everything_the_driver_reports_for_an_enabled_extension_is_enabled() {
        let driver = match crate::driver::Driver::open(0) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("no Vulkan driver here ({e}); skipped");
                return;
            }
        };
        let exts: Vec<&str> = driver.enabled_extensions.iter().map(String::as_str).collect();
        // Every struct an enabled extension or core version provides,
        // core-covered ones too: an application may chain
        // `VkPhysicalDevice8BitStorageFeatures`.
        let mut asked = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for s in &sv_gpu_entry::DEVICE_FEATURE_STRUCTS {
            let by_core = s.providers.iter().filter_map(|p| core_minor(p)).any(|m| m <= 4);
            if (by_core || s.providers.iter().any(|p| exts.contains(p))) && seen.insert(s.s_type) {
                asked.push(sv_gpu_entry::device_feature_link(s.s_type).unwrap());
            }
        }
        let template = sv_proto::VkPhysicalDeviceFeatures2 { p_next: asked, ..Default::default() };
        let raw = query_driver(&driver.instance, driver.pd, &template).unwrap();
        let ask_all = |f: &sv_proto::VkPhysicalDeviceFeatures2| sv_proto::VkDeviceCreateInfo {
            p_next: f.p_next.clone(),
            p_enabled_features: Some(Box::new(f.features.clone())),
            ..Default::default()
        };
        // Unmasked, the only members the device lacks are the ones it withheld
        // on purpose — and, on a device below 1.4, 1.4's own block.
        let withheld: Vec<String> = WITHHELD_STRUCTS
            .iter()
            .filter_map(|n| sv_gpu_entry::DEVICE_FEATURE_STRUCTS.iter().find(|s| s.name == *n))
            .filter_map(|s| sv_gpu_entry::device_feature_link(s.s_type))
            .flat_map(|l| sv_gpu_entry::feature_bits(&l).unwrap())
            .map(|(n, _)| n.to_string())
            .collect();
        let excl: Vec<&str> = EXCLUSIVE.iter().map(|(_, d)| *d).collect();
        let unexpected: Vec<String> = not_enabled(&ask_all(&raw), &driver.features)
            .into_iter()
            .filter(|m| {
                let member = m.rsplit('.').next().unwrap();
                !withheld.iter().any(|w| w == member)
                    && !excl.contains(&member)
                    && !m.starts_with("VkPhysicalDeviceVulkan14Features.")
            })
            .collect();
        assert!(unexpected.is_empty(), "{}: reported and not enabled: {unexpected:?}", driver.name);
        // Masked, as an application is told, everything is enabled.
        let mut told = raw.clone();
        mask_to_enabled(&mut told, &driver.features);
        let missing = not_enabled(&ask_all(&told), &driver.features);
        assert!(missing.is_empty(), "{}: advertised and not enabled: {missing:?}", driver.name);
    }

    #[test]
    fn of_two_exclusive_features_the_khr_one_is_kept() {
        let link = |name: &str, members: &[&str]| {
            let s = sv_gpu_entry::DEVICE_FEATURE_STRUCTS.iter().find(|s| s.name == name)?;
            let mut l = sv_gpu_entry::device_feature_link(s.s_type)?;
            for m in members {
                assert!(sv_gpu_entry::set_feature_bit(&mut l, m, true));
            }
            Some(l)
        };
        let (Some(fsr), Some(fdm)) = (
            link("VkPhysicalDeviceFragmentShadingRateFeaturesKHR", &["pipelineFragmentShadingRate"]),
            link("VkPhysicalDeviceFragmentDensityMapFeaturesEXT", &["fragmentDensityMap"]),
        ) else {
            return; // a profile without one of the two has nothing to resolve
        };
        let mut chain = vec![fsr, fdm];
        assert_eq!(resolve_exclusive(&mut chain), ["fragmentDensityMap"]);
        let on: Vec<_> = chain
            .iter()
            .flat_map(|l| sv_gpu_entry::feature_bits(l).unwrap())
            .filter(|(_, v)| *v)
            .map(|(n, _)| n)
            .collect();
        assert_eq!(on, ["pipelineFragmentShadingRate"]);
    }
}
