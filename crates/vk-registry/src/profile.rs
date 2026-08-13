//! Which part of the registry a given build generates for.
//!
//! design.md §11 scopes the project to Zink, and §13 step 3 starts smaller than
//! that. Neither is "the whole registry", so the generator needs an explicit,
//! *declared* answer to "is `vkCmdDrawIndirectCount` in?" — declared, because a
//! subset discovered by whatever the generator happened to reach is a subset
//! nobody can review and one that changes silently when the registry moves.
//!
//! A [`Profile`] is that declaration: a list of core versions and extensions,
//! plus explicit additions and removals. Resolving it produces a
//! [`ResolvedProfile`] — a closed set of commands and types where "closed"
//! means every type any included command mentions, transitively, is also
//! included. The closure is the part worth having code for; hand-maintaining it
//! is how a generator ends up emitting a struct whose member type does not
//! exist.

use crate::ctype::{CType, CTypeKind};
use crate::model::{InterfaceRef, Model, TypeDef};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// A declared generation scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// Core feature names, e.g. `VK_VERSION_1_0` … `VK_VERSION_1_3`.
    pub features: Vec<String>,
    /// Extension names.
    pub extensions: Vec<String>,
    /// Commands to include even if no listed feature or extension provides
    /// them. Kept separate from `extensions` so the reason for each is visible.
    pub extra_commands: Vec<String>,
    /// Commands to exclude even though a listed feature provides them.
    ///
    /// This is where "we do not forward presentation" (design.md §10) and "we
    /// strip `pAllocator` above the protocol" (§7.4) become facts a reviewer can
    /// read, rather than behaviour buried in the generator.
    pub excluded_commands: Vec<String>,
    /// When set, keep *only* these commands out of everything the listed
    /// features and extensions provide.
    ///
    /// An allowlist rather than a 400-entry exclusion list, so that
    /// design.md §13 step 3's "deliberately tiny command set" can be written
    /// down as the sixteen names it is.
    pub only_commands: Option<Vec<String>>,
    /// Types to exclude, with the same reasoning.
    pub excluded_types: Vec<String>,
}

/// The closed result of resolving a [`Profile`] against a [`Model`].
#[derive(Debug, Clone, Default)]
pub struct ResolvedProfile {
    pub name: String,
    /// Command definitions in scope, sorted by name.
    ///
    /// Sorted rather than in registry order because opcode assignment reads
    /// this list, and an opcode table that depends on `vk.xml`'s declaration
    /// order would shuffle whenever Khronos reorders a block.
    pub commands: Vec<String>,
    /// Command aliases in scope: alias name → the definition it shares an
    /// opcode with.
    pub command_aliases: BTreeMap<String, String>,
    /// Every type reachable from the included commands, sorted by name.
    pub types: Vec<String>,
    /// Type aliases in scope.
    pub type_aliases: BTreeMap<String, String>,
    /// Which feature or extension first brought each command in, for the
    /// generated `vkGetInstanceProcAddr` tables and for diagnostics.
    pub command_origin: BTreeMap<String, String>,
    /// Enum constants explicitly required by the listed features/extensions.
    pub enum_values: BTreeSet<String>,
}

impl ResolvedProfile {
    pub fn has_command(&self, name: &str) -> bool {
        self.commands.binary_search(&name.to_string()).is_ok()
    }
    pub fn has_type(&self, name: &str) -> bool {
        self.types.binary_search(&name.to_string()).is_ok()
    }
}

impl Profile {
    /// The highest core Vulkan version this profile declares, as
    /// `VK_MAKE_API_VERSION(0, major, minor, 0)`.
    ///
    /// **This is the number the app side may advertise**, and deriving it here
    /// rather than writing it down twice is the point: an ICD's
    /// `vkEnumerateInstanceVersion` is a promise that the entry points and
    /// structures of that version exist, and the only thing that can keep the
    /// promise is the wire profile. `sv-icd` calls this its *ceiling* and
    /// advertises `min(what the driver supports, this)`; see design.md §0.18.
    ///
    /// A profile with no `VK_VERSION_*` feature carries no core Vulkan at all,
    /// which is the §13-step-3 tiny profile, and 1.0 is the honest answer for
    /// it: an ICD cannot report less.
    pub fn core_api_version(&self) -> u32 {
        let mut best = (1u32, 0u32);
        for f in &self.features {
            let Some(rest) = f.strip_prefix("VK_VERSION_") else { continue };
            let Some((major, minor)) = rest.split_once('_') else { continue };
            let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>()) else {
                continue;
            };
            if (major, minor) > best {
                best = (major, minor);
            }
        }
        (best.0 << 22) | (best.1 << 12)
    }

    /// Resolve the profile: gather what the listed features and extensions
    /// require, then close over type dependencies.
    pub fn resolve(&self, model: &Model) -> Result<ResolvedProfile> {
        let mut out = ResolvedProfile { name: self.name.clone(), ..Default::default() };

        let excluded_cmds: BTreeSet<&str> =
            self.excluded_commands.iter().map(String::as_str).collect();
        let excluded_types: BTreeSet<&str> =
            self.excluded_types.iter().map(String::as_str).collect();

        let mut commands: BTreeSet<String> = BTreeSet::new();
        let mut seed_types: BTreeSet<String> = BTreeSet::new();

        // -- 1. what the listed features and extensions require ---------------
        //
        // A core version is not a leaf. Since `vulkan-headers 1.4.34x` the
        // registry splits each one into `apitype="internal"` sub-features
        // chained by `depends=`, and `<feature name="VK_VERSION_1_0">` itself
        // contains no commands at all — see `crate::depends`. So enabling a
        // feature means enabling its whole dependency chain.
        let mut enabled_names: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        for name in &self.features {
            if !model.features.iter().any(|f| &f.name == name) {
                bail!("profile {:?} lists unknown feature {name:?}", self.name);
            }
            queue.push_back(name.clone());
        }
        while let Some(name) = queue.pop_front() {
            if !enabled_names.insert(name.clone()) {
                continue;
            }
            let Some(f) = model.features.iter().find(|f| f.name == name) else { continue };
            if !f.supported_for_vulkan {
                bail!(
                    "profile {:?} reaches feature {name:?}, which the registry does not \
                     support for the `vulkan` API",
                    self.name
                );
            }
            if let Some(dep) = &f.depends {
                for n in crate::depends::names(dep) {
                    queue.push_back(n);
                }
            }
        }

        let mut enabled: Vec<&crate::model::FeatureSet> = model
            .features
            .iter()
            .filter(|f| enabled_names.contains(&f.name))
            .collect();

        for name in &self.extensions {
            match model.extensions.iter().find(|e| &e.name == name) {
                Some(e) if e.supported_for_vulkan => {
                    enabled.push(e);
                    enabled_names.insert(name.clone());
                }
                Some(_) => bail!(
                    "profile {:?} lists extension {name:?}, which the registry does not \
                     support for the `vulkan` API",
                    self.name
                ),
                None => bail!("profile {:?} lists unknown extension {name:?}", self.name),
            }
        }

        // An extension whose own `depends=` is not met would be enabled against
        // a Vulkan version that does not provide the types it uses. That is a
        // profile bug, and a loud one is better than a protocol quietly missing
        // half a struct.
        let is_enabled = |n: &str| enabled_names.contains(n);
        for name in &self.extensions {
            let e = model.extensions.iter().find(|e| &e.name == name).unwrap();
            if let Some(dep) = &e.depends
                && !crate::depends::satisfied(dep, &is_enabled) {
                    bail!(
                        "profile {:?} enables {name:?}, whose depends= expression {dep:?} \
                         is not satisfied by the rest of the profile",
                        self.name
                    );
                }
        }

        for fs in &enabled {
            for block in &fs.requires {
                // A `<require depends="…">` block is conditional on other
                // features or extensions. Including it unconditionally would
                // pull in commands this profile does not actually have.
                if let Some(dep) = &block.depends
                    && !crate::depends::satisfied(dep, &is_enabled) {
                        continue;
                    }
                for item in &block.items {
                    match item {
                        InterfaceRef::Command(c) => {
                            commands.insert(c.clone());
                            out.command_origin.entry(c.clone()).or_insert(fs.name.clone());
                        }
                        InterfaceRef::Type(t) => {
                            seed_types.insert(t.clone());
                        }
                        InterfaceRef::Enum { name, .. } => {
                            out.enum_values.insert(name.clone());
                        }
                    }
                }
            }
        }

        // A core version's `<remove>` block is a real deprecation: 1.4 removes
        // a handful of 1.0 entry points. Honour it, or we generate stubs for
        // things the profile says are gone.
        for fs in &enabled {
            for item in &fs.removes {
                match item {
                    InterfaceRef::Command(c) => {
                        commands.remove(c);
                    }
                    InterfaceRef::Type(t) => {
                        seed_types.remove(t);
                    }
                    InterfaceRef::Enum { .. } => {}
                }
            }
        }

        if let Some(only) = &self.only_commands {
            let keep: BTreeSet<&str> = only.iter().map(String::as_str).collect();
            for name in only {
                if !commands.contains(name) {
                    bail!(
                        "profile {:?} restricts to {name:?}, which none of its features or \
                         extensions provides",
                        self.name
                    );
                }
            }
            commands.retain(|c| keep.contains(c.as_str()));
        }

        for c in &self.extra_commands {
            commands.insert(c.clone());
            out.command_origin.entry(c.clone()).or_insert_with(|| "profile".to_string());
        }
        for c in &excluded_cmds {
            commands.remove(*c);
        }

        // -- 2. resolve aliases, keeping both names --------------------------
        let mut definitions: BTreeSet<String> = BTreeSet::new();
        for c in &commands {
            if model.commands.contains_key(c) {
                definitions.insert(c.clone());
            } else {
                let target = model.canonical_command(c);
                if !model.commands.contains_key(&target) {
                    bail!("profile {:?} requires command {c:?}, which the registry does not define", self.name);
                }
                if excluded_cmds.contains(target.as_str()) {
                    continue;
                }
                definitions.insert(target.clone());
                out.command_aliases.insert(c.clone(), target);
            }
        }

        // -- 3. close over types ---------------------------------------------
        let mut reached: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        // A `<require>` block names more than types: `VK_API_VERSION` and the
        // other `category="define"` entries, `<type name="vk_platform"/>`
        // includes, and enum group names all appear as `<type>` references. Only
        // the ones the model actually defines are worth chasing; the rest carry
        // no marshalling. A type reached from a *declarator*, by contrast, must
        // resolve — that check is below and stays fatal.
        for t in seed_types {
            if model.types.contains_key(&model.canonical_type(&t)) {
                queue.push_back(t);
            }
        }
        for c in &definitions {
            let cmd = &model.commands[c];
            collect_type_names(&cmd.return_type, &mut queue);
            for p in &cmd.params {
                collect_type_names(&p.ty, &mut queue);
            }
        }
        // `pNext` extenders are reachable only through the chain, never through
        // a member's declared type. A generator that skips them emits a chain
        // walker that cannot encode anything, which is the sort of hole that
        // shows up as "the extension silently did nothing".
        let mut pending_extenders = true;

        while let Some(name) = queue.pop_front() {
            let canonical = model.canonical_type(&name);
            if canonical != name {
                out.type_aliases.insert(name.clone(), canonical.clone());
            }
            if excluded_types.contains(canonical.as_str()) || !reached.insert(canonical.clone()) {
                continue;
            }
            let Some(def) = model.types.get(&canonical) else {
                bail!(
                    "profile {:?}: type {name:?} is referenced but the registry does not define it",
                    self.name
                );
            };
            match def {
                TypeDef::Struct(s) => {
                    for m in &s.members {
                        collect_type_names(&m.ty, &mut queue);
                    }
                    // The `sType` constant and any enum a member names.
                    if let Some(st) = &s.s_type {
                        out.enum_values.insert(st.clone());
                    }
                }
                TypeDef::BaseType { underlying: Some(u), .. } => collect_type_names(u, &mut queue),
                TypeDef::Bitmask(b) => {
                    if let Some(e) = &b.bit_enum {
                        queue.push_back(e.clone());
                    }
                }
                TypeDef::FuncPointer(f) => {
                    collect_type_names(&f.return_type, &mut queue);
                    for (_, t) in &f.params {
                        collect_type_names(t, &mut queue);
                    }
                }
                TypeDef::Handle(h) => {
                    if let Some(p) = &h.parent {
                        // `parent=` may name several alternatives.
                        for one in p.split(',') {
                            queue.push_back(one.trim().to_string());
                        }
                    }
                }
                _ => {}
            }

            if queue.is_empty() && pending_extenders {
                pending_extenders = false;
                // Only extenders whose own gating is in the profile: an
                // extension struct from an extension we did not enable has no
                // business in our chain walkers.
                let mut providers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
                for fs in model.features.iter().chain(model.extensions.iter()) {
                    if !enabled_names.contains(&fs.name) {
                        continue;
                    }
                    for block in &fs.requires {
                        for item in &block.items {
                            if let InterfaceRef::Type(t) = item {
                                providers
                                    .entry(model.canonical_type(t))
                                    .or_default()
                                    .insert(fs.name.clone());
                            }
                        }
                    }
                }
                let targets: Vec<String> = reached.iter().cloned().collect();
                for target in targets {
                    let Some(exts) = model.extended_by.get(&target) else { continue };
                    for e in exts {
                        if providers.contains_key(e) && !excluded_types.contains(e.as_str()) {
                            queue.push_back(e.clone());
                        }
                    }
                }
            }
        }

        out.commands = definitions.into_iter().collect();
        out.types = reached.into_iter().collect();
        Ok(out)
    }
}

/// Push every named type mentioned by a C type onto the work queue.
fn collect_type_names(ty: &CType, queue: &mut VecDeque<String>) {
    match &ty.kind {
        CTypeKind::Void => {}
        CTypeKind::Named(n) => queue.push_back(n.clone()),
        CTypeKind::Pointer(inner) | CTypeKind::Array(inner, _) => collect_type_names(inner, queue),
    }
}

/// The profiles this project ships.
///
/// They are data, in code, rather than a config file, for one reason: a profile
/// that lists an extension the registry does not support must fail the *build*,
/// and a checked-in Rust value is checked by the same compiler that builds the
/// generator. Growing the subset is editing this function, which is exactly the
/// "declared input" design.md §13 asks for.
pub mod profiles {
    use super::Profile;

    /// design.md §13 step 3: "a deliberately tiny command set".
    ///
    /// Its purpose is to be small enough to read the generated output of, by
    /// hand, in full. Everything the generator can get wrong it can get wrong
    /// here, at a size where a person can see it.
    pub fn step3_minimal() -> Profile {
        Profile {
            name: "step3-minimal".into(),
            features: vec!["VK_VERSION_1_0".into()],
            extensions: vec![],
            extra_commands: vec![],
            excluded_commands: excluded_commands(),
            excluded_types: excluded_types(),
            only_commands: Some(STEP3_COMMANDS.iter().map(|s| s.to_string()).collect()),
        }
    }

    /// The commands design.md §13 step 3 names, kept as the positive list the
    /// exclusion above is derived from.
    pub const STEP3_COMMANDS: &[&str] = &[
        "vkCreateInstance",
        "vkDestroyInstance",
        "vkEnumeratePhysicalDevices",
        "vkGetPhysicalDeviceProperties",
        "vkGetPhysicalDeviceMemoryProperties",
        "vkGetPhysicalDeviceQueueFamilyProperties",
        "vkCreateDevice",
        "vkDestroyDevice",
        "vkAllocateMemory",
        "vkFreeMemory",
        "vkMapMemory",
        "vkUnmapMemory",
        "vkCreateBuffer",
        "vkDestroyBuffer",
        "vkGetBufferMemoryRequirements",
        "vkBindBufferMemory",
    ];

    /// design.md §11: Zink's requirement set, which is the v1 scope.
    ///
    /// Core 1.0–1.3 plus the extensions zink's own `VP_ZINK_requirements.json`
    /// profile names as required, plus the WSI family of design.md §10 stage 2.
    ///
    /// **`VK_KHR_swapchain` is in this list and none of its commands are on the
    /// wire.** That is not a contradiction: an extension appears here so that
    /// its *types* exist on the C surface and so that its *name* reaches the GPU
    /// side's `vkCreateDevice`, and `excluded_commands` decides separately which
    /// of its commands cross. Presentation is implemented in the ICD (§10), so
    /// none of them do — but the GPU-side driver still has to have the extension
    /// enabled, because the application will transition a swapchain image to
    /// `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR` and that layout does not exist on a
    /// device that never enabled it.
    pub fn zink() -> Profile {
        Profile {
            name: "zink".into(),
            features: vec![
                "VK_VERSION_1_0".into(),
                "VK_VERSION_1_1".into(),
                "VK_VERSION_1_2".into(),
                "VK_VERSION_1_3".into(),
            ],
            extensions: vec![
                // Zink's five hard requirements (design.md §11).
                "VK_KHR_maintenance1".into(),
                // Advertised for its *name*, not for anything it adds. Every
                // type and command it defines is core since 1.1 and so already
                // in this profile; what is not already here is the string in
                // `VkPhysicalDeviceProperties`' extension list, and Zink gates
                // the whole tessellation stage on finding it there —
                // `zink_init_shader_caps` skips `MESA_SHADER_TESS_CTRL`/`_EVAL`
                // unless `feats.features.tessellationShader &&
                // have_KHR_maintenance2` (`zink_screen.c`), and it reads
                // `have_KHR_maintenance2` from the extension list rather than
                // from the version, promoted or not. Without it Mesa's state
                // tracker sees zero tessellation-control instructions, drops
                // `GL_ARB_tessellation_shader`, and GL stops at 3.3.
                "VK_KHR_maintenance2".into(),
                "VK_KHR_maintenance5".into(),
                "VK_KHR_create_renderpass2".into(),
                "VK_KHR_dynamic_rendering".into(),
                "VK_KHR_descriptor_update_template".into(),
                "VK_KHR_timeline_semaphore".into(),
                // The ladder to GL 4.6 that VP_ZINK_gl21_baseline and its
                // successors add, restricted to things that do not put raw
                // device addresses in application data (design.md §9.2).
                "VK_EXT_transform_feedback".into(),
                "VK_EXT_conditional_rendering".into(),
                "VK_EXT_provoking_vertex".into(),
                "VK_EXT_line_rasterization".into(),
                "VK_EXT_vertex_attribute_divisor".into(),
                "VK_EXT_extended_dynamic_state".into(),
                "VK_EXT_extended_dynamic_state2".into(),
                "VK_EXT_depth_clip_enable".into(),
                "VK_EXT_border_color_swizzle".into(),
                "VK_EXT_custom_border_color".into(),
                "VK_EXT_image_2d_view_of_3d".into(),
                "VK_EXT_multi_draw".into(),
                "VK_EXT_color_write_enable".into(),
                "VK_EXT_non_seamless_cube_map".into(),
                "VK_EXT_primitives_generated_query".into(),
                "VK_KHR_shader_draw_parameters".into(),
                // Likewise name-only: `vkCmdDrawIndirectCount` and
                // `vkCmdDrawIndexedIndirectCount` are core since 1.2 and were
                // already forwarded, but Zink sets
                // `PIPE_CAP_MULTI_DRAW_INDIRECT_PARAMS` from
                // `have_KHR_draw_indirect_count` alone, and Mesa gates
                // `GL_ARB_indirect_parameters` — the last extension on the way
                // to GL 4.6 — on that cap. Adding the string also brings the
                // two `…KHR` aliases onto the exported ABI, which is what Zink
                // resolves once it believes the extension is present.
                "VK_KHR_draw_indirect_count".into(),
                "VK_EXT_robustness2".into(),
                // The reverse ring forwards validation-layer output to the app
                // side, which needs the messenger types (design.md §4.6).
                "VK_EXT_debug_utils".into(),
                // Headless first (design.md §10 stage 1).
                "VK_EXT_headless_surface".into(),
                "VK_KHR_surface".into(),
                // §10 stage 2. The ICD implements every command of this
                // extension; see the note above for why it is nonetheless in the
                // profile. Advertising it is also what DXVK ≤ 2.6.2 was waiting
                // for — it marks `VK_KHR_swapchain` *required* even for a device
                // that will never present (§0.5).
                "VK_KHR_swapchain".into(),
                // The three surface extensions Mesa's `zink_kopper` asks for by
                // name. None carries a window-system dependency into this
                // crate: the loader's `VkIcdSurfaceXlib`/`VkIcdSurfaceXcb`/
                // `VkIcdSurfaceWayland` are what an ICD actually receives, and
                // what the ICD does with the pointers inside them is
                // `sv_icd::present`'s business — Xlib and `libwayland-client`
                // are both reached through `dlopen`.
                //
                // **`VK_KHR_wayland_surface` used to say "deliberately not
                // here — phase 2", and that sentence is what stopped Firefox.**
                // Without it `zink_kopper` cannot make a `VkSurfaceKHR` on the
                // Wayland platform, so Mesa loads the *swrast* DRI driver with
                // Zink merely behind it as the gallium driver, and Firefox's
                // hardware WebRender rejects the resulting EGL configs
                // (`docs/firefox-accel.md`, "testhost, reached"). The control
                // that pins it is RADV on a `wl_shm`-only weston, where the one
                // difference is that RADV has this extension.
                "VK_KHR_xlib_surface".into(),
                "VK_KHR_xcb_surface".into(),
                "VK_KHR_wayland_surface".into(),
            ],
            extra_commands: vec![],
            excluded_commands: excluded_commands(),
            excluded_types: excluded_types(),
            only_commands: None,
        }
    }

    /// Zink's set plus Vulkan Video's H.264 decode path.
    ///
    /// **This profile used not to resolve, and that was what it was for.** It
    /// failed in `plan.rs` on the first `StdVideo*` type it reached, because
    /// `vk.xml` does not define the video std types — it names them, with
    /// `requires="vk_video/vulkan_video_codec_h264std.h"` — so the registry
    /// modelled them as `TypeDef::External` and the wire planner refused a type
    /// whose layout it could not know. Six were on the H.264 decode surface and
    /// two were passed by value, which also broke the C ABI pass's assumption
    /// that an external type is only ever seen behind a pointer.
    ///
    /// It resolves now, and nothing about the planner's rule changed: the
    /// generator was given the other registry Khronos publishes.
    /// `crate::video_registry_path` is the whole argument; the short version is
    /// that `video.xml` ships beside `vk.xml` in the same package, is what the
    /// `vk_video/*.h` headers are generated *from*, and is read by the same
    /// parser. `sv-testkit`'s `the_video_std_types_match_the_vk_video_headers`
    /// is what makes that something other than a claim.
    ///
    /// What it carries, and what it does not:
    ///
    /// - 252 commands against Zink's 239 — the thirteen of `VK_KHR_video_queue`
    ///   and `VK_KHR_video_decode_queue`.
    /// - The `StdVideo*` types by value and by pointer, including
    ///   `StdVideoH264SequenceParameterSet`'s three pointer members, which the
    ///   generator deep-copies because the registry states their `len=` and
    ///   `optional=`. `docs/video-decode.md` §4 expected to have to hand-write
    ///   that; it did not.
    /// - **H.264 only.** H.265 and AV1 are surveyed by `video-probe` and are not
    ///   in the profile; their per-frame structures have pointers where H.264's
    ///   have none, and adding them is a measurement rather than a line.
    pub fn video() -> Profile {
        let mut p = zink();
        p.name = "video".into();
        p.extensions.extend([
            "VK_KHR_video_queue".into(),
            "VK_KHR_video_decode_queue".into(),
            "VK_KHR_video_decode_h264".into(),
        ]);
        p
    }

    /// Commands excluded from every profile, with the reason each is out.
    fn excluded_commands() -> Vec<String> {
        [
            // design.md §10: presentation is outside the protocol. The ICD
            // implements the swapchain; it is never forwarded.
            "vkCreateSwapchainKHR",
            "vkDestroySwapchainKHR",
            "vkGetSwapchainImagesKHR",
            "vkAcquireNextImageKHR",
            "vkQueuePresentKHR",
            // The rest of `VK_KHR_swapchain`, which is the device-group half it
            // absorbed when 1.1 promoted `VK_KHR_device_group`. We advertise one
            // physical device in one group, so there is nothing for these to
            // say; they exist on the C surface and refuse, and nothing on the
            // wire could answer them anyway because the GPU side has no surface.
            "vkGetDeviceGroupPresentCapabilitiesKHR",
            "vkGetDeviceGroupSurfacePresentModesKHR",
            "vkGetPhysicalDevicePresentRectanglesKHR",
            "vkAcquireNextImage2KHR",
            // Surface queries are answered app-side from what the ICD
            // advertises, not asked of a GPU side that has no window system.
            "vkDestroySurfaceKHR",
            // Also off the C surface (`sv_codegen::abi::SURFACE_CREATORS`), and
            // for a sharper reason than the X11 pair: an ICD that exports *any*
            // surface creator is handed its own handle for those surfaces and
            // the loader's `VkIcdSurface*` for the rest, and there is no way to
            // tell one 64-bit value from the other. Exporting none makes every
            // `VkSurfaceKHR` a `VkIcdSurfaceBase*` whose first field says which
            // window system it is.
            "vkCreateHeadlessSurfaceEXT",
            "vkGetPhysicalDeviceSurfaceSupportKHR",
            "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
            "vkGetPhysicalDeviceSurfaceFormatsKHR",
            "vkGetPhysicalDeviceSurfacePresentModesKHR",
            // Surface creation, likewise. **The ICD implements these rather
            // than declining them**, which is a change from design.md §8's "we
            // opt out of interface-3 surface creation by not exposing
            // `vkCreateXXXSurfaceKHR`": an ICD that does not export
            // `vkGetPhysicalDeviceXcbPresentationSupportKHR` makes the loader
            // `abort()` rather than fall back, so once the extension is
            // advertised at all the whole family has to be there.
            // §8's opt-out, and the only two commands that stay off the *C*
            // surface as well (`sv_codegen::abi::SURFACE_CREATORS`). Their one
            // interesting parameter is an `xcb_connection_t*` or a `Display*`,
            // which comes from a platform header, so neither the wire nor a
            // generated C signature can name it — and neither has to: an ICD
            // that does not export them is handed the loader's own
            // `VkIcdSurfaceXcb`/`VkIcdSurfaceXlib` instead, which is the
            // documented fallback and carries the same two fields.
            "vkCreateXlibSurfaceKHR",
            "vkCreateXcbSurfaceKHR",
            "vkCreateWaylandSurfaceKHR",
            // These three, by contrast, **must** be exported once the extension
            // is advertised: the loader's terminator calls `abort()` when the
            // selected ICD does not have them, rather than answering VK_FALSE.
            // They are hand-written and ignore their connection argument.
            //
            // Excluded from the *wire* rather than from the C surface, and the
            // distinction is the whole reason they are in this list twice over:
            // `Display*`, `xcb_connection_t*` and `struct wl_display*` are
            // platform types the wire planner refuses to serialise — correctly,
            // since a pointer into another process's libwayland means nothing
            // in the guest — while a generated C signature can name them
            // perfectly well as an opaque pointer, which is what
            // `sv_codegen::abi::AbiType::Opaque` is for.
            "vkGetPhysicalDeviceXlibPresentationSupportKHR",
            "vkGetPhysicalDeviceXcbPresentationSupportKHR",
            "vkGetPhysicalDeviceWaylandPresentationSupportKHR",
            // The debug-utils messenger lives on the app side; its GPU-side
            // half is the reverse ring, which is hand-written (design.md §4.6,
            // §6.3), not a forwarded command.
            "vkCreateDebugUtilsMessengerEXT",
            "vkDestroyDebugUtilsMessengerEXT",
            "vkSubmitDebugUtilsMessageEXT",
            // Proc-address lookup is the ICD's own job and cannot be forwarded:
            // the answer is a pointer, and pointers do not cross (design.md
            // §6.1 exclusion list).
            "vkGetInstanceProcAddr",
            "vkGetDeviceProcAddr",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Types excluded from every profile, with the reason each is out.
    fn excluded_types() -> Vec<String> {
        [
            // design.md §7.4: not serialisable. Stripped above the protocol, as
            // Mesa's own Venus driver does.
            "VkAllocationCallbacks",
            // Ray tracing is out of scope, and this struct is bitfields the
            // registry cannot describe (design.md §7.4).
            "VkAccelerationStructureInstanceKHR",
            // Carries `PFN_vkDebugUtilsMessengerCallbackEXT`, a function
            // pointer, which cannot cross an address-space boundary. The app
            // side registers the messenger locally and the GPU side forwards
            // messages over the reverse ring instead (design.md §4.6). Excluded
            // here rather than left to fail in the generator so that the reason
            // is next to the decision.
            "VkDebugUtilsMessengerCreateInfoEXT",
            // The two X11 surface-creation structs. Each holds a pointer into a
            // library this project does not link — `xcb_connection_t*`,
            // `Display*` — so they are not serialisable in exactly the sense
            // design.md §6.1 means, and they are not on the C surface either,
            // because the two commands that take them are the ones §8 declines
            // to export. What the ICD is handed instead is the loader's
            // `VkIcdSurfaceXcb`/`VkIcdSurfaceXlib`, a `vk_icd.h` struct with the
            // same two fields and no registry entry at all.
            "VkXcbSurfaceCreateInfoKHR",
            "VkXlibSurfaceCreateInfoKHR",
            // And the Wayland one, for the same reason and one more. `struct
            // wl_display*` and `struct wl_surface*` are pointers into
            // `libwayland-client`, which this project also does not link — and
            // unlike the X11 pair, these two pointers are **used**: `sv_icd`'s
            // Wayland target attaches a `wl_shm` buffer to that very surface on
            // that very connection (design.md §10, `sv_icd::present::wayland`).
            // Used through `dlopen`, though, so the crate still links nothing;
            // what the struct has to be here is the loader's
            // `VkIcdSurfaceWayland`, which has the same two fields.
            "VkWaylandSurfaceCreateInfoKHR",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Option<Model> {
        let path = crate::registry_path().ok()?;
        let reg = crate::Registry::load_from(path).ok()?;
        reg.model().ok()
    }

    #[test]
    fn zink_profile_resolves_and_is_closed() {
        let Some(m) = model() else {
            eprintln!("VK_XML unset — skipping (run inside `nix develop`)");
            return;
        };
        let p = profiles::zink().resolve(&m).expect("zink profile should resolve");
        // Lower bounds, not equalities: a registry bump adds entries. The point
        // of the bound is to catch the resolver silently producing *nothing*,
        // which is exactly what happened when the core versions were split into
        // `apitype="internal"` sub-features (see `crate::depends`) and this
        // assertion was the only thing that noticed.
        assert!(p.commands.len() > 200, "zink subset has {} commands", p.commands.len());
        assert!(p.types.len() > 500, "zink subset has {} types", p.types.len());
        // Vulkan 1.0 core must be in there.
        for must in ["vkCreateInstance", "vkCreateDevice", "vkAllocateMemory", "vkCmdDraw"] {
            assert!(p.has_command(must), "{must} is missing from the zink subset");
        }

        // Closure: every type any included struct mentions is itself included.
        for name in &p.types {
            if let Some(TypeDef::Struct(s)) = m.types.get(name) {
                for mem in &s.members {
                    if let Some(base) = mem.ty.base_name() {
                        let canonical = m.canonical_type(base);
                        assert!(
                            p.has_type(&canonical),
                            "{name}.{} refers to {canonical}, which is not in the profile",
                            mem.name
                        );
                    }
                }
            }
        }
    }

    /// The ceiling `sv-icd` advertises comes from here and from nowhere else.
    ///
    /// Needs no registry: it reads the declaration, not the resolution, which is
    /// what makes it usable from a build script.
    #[test]
    fn the_core_version_is_the_highest_one_declared() {
        // The real one. Bump this line and the ICD's manifest together — that is
        // the whole coupling, and `sv_icd::state`'s manifest test enforces it.
        assert_eq!(profiles::zink().core_api_version(), (1 << 22) | (3 << 12), "zink is 1.0-1.3");

        let mk = |minor: u32| (1u32 << 22) | (minor << 12);
        let with = |features: &[&str]| Profile {
            features: features.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
        .core_api_version();

        // Out of order, because `features` is a declaration and not a ladder.
        assert_eq!(with(&["VK_VERSION_1_3", "VK_VERSION_1_0", "VK_VERSION_1_2"]), mk(3));
        assert_eq!(with(&["VK_VERSION_1_4"]), mk(4));
        // Two digits must not sort as text: 1.10 is above 1.9, and a profile
        // that compared strings would say the opposite.
        assert_eq!(with(&["VK_VERSION_1_9", "VK_VERSION_1_10"]), mk(10));
        // A profile with no core version at all is still a 1.0 ICD; there is no
        // lower thing an ICD may report.
        assert_eq!(with(&[]), mk(0));
        assert_eq!(with(&["VK_KHR_swapchain"]), mk(0));
    }

    #[test]
    fn excluded_commands_really_are_excluded() {
        let Some(m) = model() else { return };
        let p = profiles::zink().resolve(&m).unwrap();
        assert!(!p.has_command("vkQueuePresentKHR"));
        assert!(!p.has_command("vkGetInstanceProcAddr"));
        assert!(!p.has_type("VkAllocationCallbacks"));
    }

    /// The set pass 12 locks the device queue around, as data.
    ///
    /// Asserted as an **equality** rather than a containment, because both
    /// mistakes matter and they fail differently: a command missing from this
    /// list is a race the generator will not close, and one added spuriously is
    /// a driver call needlessly serialised across every session. It also checks
    /// that [`crate::model::Command::externally_synchronizes_a_queue`] reads
    /// both spellings — the first two entries have no `VkQueue` parameter and
    /// are reached only through `<implicitexternsyncparams>`.
    #[test]
    fn the_commands_that_externally_synchronize_a_queue_are_these_nine() {
        let Some(m) = model() else { return };
        let p = profiles::zink().resolve(&m).unwrap();
        let mut got: Vec<&str> = p
            .commands
            .iter()
            .filter(|n| {
                m.commands.get(n.as_str()).is_some_and(|c| c.externally_synchronizes_a_queue())
            })
            .map(String::as_str)
            .collect();
        got.sort_unstable();
        assert_eq!(
            got,
            [
                // `vkDestroyDevice` is the one entry pass 12 emits no lock for,
                // because it is hand-written on the GPU side — the device
                // outlives every client and the command is a no-op there
                // (`sv_codegen::exceptions::GPU_HAND_WRITTEN`). So this list has
                // nine names and the generated file has eight `lock_queue`
                // sites, and the difference is not an omission.
                "vkDestroyDevice",
                "vkDeviceWaitIdle",
                // The debug-utils label family is externally synchronised on the
                // queue for the same reason submission is: the label stack is
                // the queue's.
                "vkQueueBeginDebugUtilsLabelEXT",
                "vkQueueBindSparse",
                "vkQueueEndDebugUtilsLabelEXT",
                "vkQueueInsertDebugUtilsLabelEXT",
                "vkQueueSubmit",
                "vkQueueSubmit2",
                "vkQueueWaitIdle",
            ]
        );
    }

    #[test]
    fn aliases_are_kept_alongside_their_targets() {
        let Some(m) = model() else { return };
        let p = profiles::zink().resolve(&m).unwrap();
        // VK_KHR_maintenance1 aliases vkTrimCommandPoolKHR onto the 1.1 core
        // vkTrimCommandPool; both names must survive, because the ICD exports
        // both while the wire carries one opcode.
        assert!(p.has_command("vkTrimCommandPool"));
        assert_eq!(
            p.command_aliases.get("vkTrimCommandPoolKHR").map(String::as_str),
            Some("vkTrimCommandPool")
        );
    }
}
