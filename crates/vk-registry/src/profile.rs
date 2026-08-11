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
    /// profile names as required. `VK_KHR_swapchain` is deliberately absent:
    /// design.md §10 puts presentation outside the protocol, and §13 step 5
    /// targets `VK_EXT_headless_surface` first.
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
                "VK_EXT_robustness2".into(),
                // The reverse ring forwards validation-layer output to the app
                // side, which needs the messenger types (design.md §4.6).
                "VK_EXT_debug_utils".into(),
                // Headless first (design.md §10 stage 1).
                "VK_EXT_headless_surface".into(),
                "VK_KHR_surface".into(),
            ],
            extra_commands: vec![],
            excluded_commands: excluded_commands(),
            excluded_types: excluded_types(),
            only_commands: None,
        }
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
            // Surface queries are answered app-side from what the ICD
            // advertises, not asked of a GPU side that has no window system.
            "vkDestroySurfaceKHR",
            "vkCreateHeadlessSurfaceEXT",
            "vkGetPhysicalDeviceSurfaceSupportKHR",
            "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
            "vkGetPhysicalDeviceSurfaceFormatsKHR",
            "vkGetPhysicalDeviceSurfacePresentModesKHR",
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
        Model::build(&reg.raw).ok()
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

    #[test]
    fn excluded_commands_really_are_excluded() {
        let Some(m) = model() else { return };
        let p = profiles::zink().resolve(&m).unwrap();
        assert!(!p.has_command("vkQueuePresentKHR"));
        assert!(!p.has_command("vkGetInstanceProcAddr"));
        assert!(!p.has_type("VkAllocationCallbacks"));
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
