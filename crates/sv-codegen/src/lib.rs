//! The shared_vulkan protocol generator.
//!
//! 747 commands and 1701 structs (`docs/api-scale.md`) is five figures of
//! mechanical Rust that would be wrong in a hundred places if typed by hand, and
//! stale the day the registry moves. So the wire format is generated, and the
//! generator is the first real component of the project rather than a
//! convenience added later (design.md §7).
//!
//! # Shape of the thing
//!
//! ```text
//! $VK_XML ──vk-parse──▶ vk_registry::Model ──Profile::resolve──▶ ResolvedProfile
//!                                                                      │
//!                                                                  plan::build
//!                                                                      ▼
//!                                                                    Plan
//!                                                     ┌────────────────┼────────────────┐
//!                                                  emit::*        manifest::render   (differential
//!                                                     ▼                ▼              test input)
//!                                                 sv_proto.rs      manifest.txt
//!                                                                      ▼
//!                                                              protocol_hash (§4.3)
//! ```
//!
//! # Determinism
//!
//! The generator is a build input, so the same registry must produce
//! byte-identical Rust. Three things enforce that and all three are load-bearing:
//!
//! 1. Every map the generator iterates is ordered — `IndexMap` where insertion
//!    order is meaningful, `BTreeMap`/`BTreeSet` where it is not. No `HashMap`
//!    reaches an emitter.
//! 2. Opcodes are assigned from the *sorted* command list, so a registry that
//!    reorders a block does not renumber the protocol.
//! 3. Output goes through `prettyplease`, which is a pure function of the token
//!    stream.
//!
//! [`generate`] returns the hash of its own output alongside the source, and
//! `sv-proto`'s build script asserts that generating twice gives the same bytes.

pub mod abi;
pub mod emit;
pub mod exceptions;
pub mod manifest;
pub mod naming;
pub mod plan;

use anyhow::{Context, Result};
use vk_registry::{Model, Profile, Registry};

/// Everything one run of the generator produces.
pub struct Generated {
    /// The Rust source of the generated protocol crate.
    pub source: String,
    /// The canonical protocol manifest (design.md §4.3).
    pub manifest: String,
    /// FNV-1a of the manifest: the `protocol_hash` of the handshake header.
    pub protocol_hash: u64,
    /// Counts, for the build log and for checking the design's estimates.
    pub stats: Stats,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub commands: usize,
    pub command_aliases: usize,
    pub structs: usize,
    pub unions: usize,
    pub enums: usize,
    pub bitmasks: usize,
    pub handles: usize,
    pub pnext_links: usize,
    pub structs_needing_partial: usize,
    pub structs_with_handles: usize,
    pub structs_with_device_memory: usize,
    pub exceptions_consulted: usize,
    pub generated_lines: usize,
}

/// Load `$VK_XML`, build the model, resolve the profile, and emit.
pub fn generate(profile: &Profile) -> Result<Generated> {
    let registry = Registry::load().context("loading the pinned registry")?;
    let model = registry.model().context("building the typed model")?;
    generate_from_model(&model, profile)
}

pub fn generate_from_model(model: &Model, profile: &Profile) -> Result<Generated> {
    let resolved = profile
        .resolve(model)
        .with_context(|| format!("resolving profile {:?}", profile.name))?;
    let plan = plan::build(model, &resolved)
        .with_context(|| format!("planning profile {:?}", profile.name))?;

    let manifest = manifest::render(&plan);
    let protocol_hash = manifest::hash(&manifest);

    let tokens = emit::emit_all(&plan);
    // Parsing before printing is not a formality: it is the generator's own
    // syntax check, and it fails here with the offending tokens rather than in
    // a 100,000-line file whose line numbers mean nothing.
    let file: syn::File = syn::parse2(tokens.clone()).map_err(|e| {
        anyhow::anyhow!(
            "the generated token stream is not valid Rust: {e}\n\
             This is a generator bug, not a registry problem."
        )
    })?;
    let body = prettyplease::unparse(&file);

    let source = format!("{}\n{body}", preamble(profile, protocol_hash, &plan));
    let generated_lines = source.lines().count();

    let stats = Stats {
        commands: plan.commands.len(),
        command_aliases: plan.commands.iter().map(|c| c.aliases.len()).sum(),
        structs: plan.structs.iter().filter(|s| !s.is_union).count(),
        unions: plan.structs.iter().filter(|s| s.is_union).count(),
        enums: plan.enums.len(),
        bitmasks: plan.bitmasks.len(),
        handles: plan.handles.len(),
        pnext_links: plan.pnext_links.len(),
        structs_needing_partial: plan.structs.iter().filter(|s| s.needs_partial).count(),
        structs_with_handles: plan.structs.iter().filter(|s| s.has_handles).count(),
        structs_with_device_memory: plan.structs.iter().filter(|s| s.has_device_memory).count(),
        exceptions_consulted: plan.exceptions.len(),
        generated_lines,
    };

    Ok(Generated { source, manifest, protocol_hash, stats })
}

/// The generated `sv-icd-entry` crate body, plus what it refused to emit.
pub struct GeneratedIcd {
    pub source: String,
    pub refusals: emit::icd_entry::Refusals,
    pub commands: usize,
    pub types: usize,
    pub generated_lines: usize,
}

/// Generate pass 10 (design.md §7.2): the ICD's C ABI, its `extern "system"`
/// stubs and its proc-address tables.
///
/// Takes the *wire* profile and derives the C surface from it
/// ([`abi::surface_profile`]), because the two are not the same set and the
/// difference — the commands the ICD answers itself — is exactly what would be
/// lost by generating from one profile.
pub fn generate_icd_entry(model: &Model, wire_profile: &Profile) -> Result<GeneratedIcd> {
    let resolved = wire_profile.resolve(model)?;
    let wire = plan::build(model, &resolved)?;
    let forwarded: std::collections::BTreeSet<String> =
        resolved.commands.iter().cloned().collect();
    let surface = abi::surface_profile(wire_profile);
    let abi = abi::build(model, &surface, &forwarded)?;

    let cx = emit::icd_entry::Ctx::new(model, &abi, &wire);
    let (sections, refusals) = emit::icd_entry::emit_all(&cx);

    let mut tokens = proc_macro2::TokenStream::new();
    for (_, t) in &sections {
        tokens.extend(t.clone());
    }
    let file: syn::File = match syn::parse2(tokens) {
        Ok(f) => f,
        Err(_) => {
            // Re-parse section by section. The whole stream is over a megabyte on
            // one line, so a span into it is not evidence a person can act on;
            // the section label is.
            for (label, t) in &sections {
                if let Err(e) = syn::parse2::<syn::File>(t.clone()) {
                    let dump = std::path::PathBuf::from("sv-icd-entry-unparsed.rs");
                    let _ = std::fs::write(&dump, t.to_string());
                    anyhow::bail!(
                        "the generated ICD entry section {label:?} is not valid Rust: {e}\n\
                         This is a generator bug, not a registry problem.\n\
                         The section is in {}",
                        dump.display()
                    );
                }
            }
            anyhow::bail!(
                "the generated ICD entry stream does not parse as a whole although \
                 every section does, which means two sections collide"
            );
        }
    };
    let body = prettyplease::unparse(&file);
    let header = format!(
        "// GENERATED by sv-codegen (pass 10, design.md §7.2). Do not edit.\n\
         //\n\
         // C surface:  {} commands, {} types\n\
         // Refused:    {} commands, {} struct lowerings, {} struct writebacks\n\
         //\n\
         // This is the only generated crate that permits `unsafe`, and every\n\
         // block in it has the same shape (design.md §7.3, security-model.md R7).\n",
        abi.commands.len(),
        abi.types.len(),
        refusals.commands.len(),
        refusals.lower.len(),
        refusals.store.len(),
    );
    let source = format!("{header}\n{body}");
    let generated_lines = source.lines().count();
    Ok(GeneratedIcd {
        source,
        refusals,
        commands: abi.commands.len(),
        types: abi.types.len(),
        generated_lines,
    })
}

/// The generated `sv-gpu-entry` crate body, plus what it refused to emit.
pub struct GeneratedGpu {
    pub source: String,
    pub refusals: emit::gpu_entry::Refusals,
    /// Commands the generator forwards without a line of hand-written code.
    pub forwarded: usize,
    /// Commands the GPU side must give meaning to itself
    /// ([`exceptions::GPU_HAND_WRITTEN`]).
    pub hand_written: usize,
    pub generated_lines: usize,
}

/// Generate pass 12 (design.md §7.2, as corrected): the GPU side's wire → C
/// raise pass, the driver's function table, and the forwarding `Server` impl.
///
/// Takes the same two plans `generate_icd_entry` does, because the GPU side's
/// problem is the app side's in mirror image and both need the wire shapes and
/// the C shapes side by side.
pub fn generate_gpu_entry(model: &Model, wire_profile: &Profile) -> Result<GeneratedGpu> {
    let resolved = wire_profile.resolve(model)?;
    let wire = plan::build(model, &resolved)?;
    let forwarded_set: std::collections::BTreeSet<String> =
        resolved.commands.iter().cloned().collect();
    let surface = abi::surface_profile(wire_profile);
    let abi = abi::build(model, &surface, &forwarded_set)?;

    let cx = emit::icd_entry::Ctx::new(model, &abi, &wire);
    let hand = exceptions::gpu_hand_written();
    let (sections, refusals) =
        emit::gpu_entry::emit_all(&cx, &hand, &wire_profile.extensions);

    let mut tokens = proc_macro2::TokenStream::new();
    for (_, t) in &sections {
        tokens.extend(t.clone());
    }
    let file: syn::File = match syn::parse2(tokens) {
        Ok(f) => f,
        Err(_) => {
            for (label, t) in &sections {
                if let Err(e) = syn::parse2::<syn::File>(t.clone()) {
                    let dump = std::path::PathBuf::from("sv-gpu-entry-unparsed.rs");
                    let _ = std::fs::write(&dump, t.to_string());
                    anyhow::bail!(
                        "the generated GPU entry section {label:?} is not valid Rust: {e}\n\
                         This is a generator bug, not a registry problem.\n\
                         The section is in {}",
                        dump.display()
                    );
                }
            }
            anyhow::bail!(
                "the generated GPU entry stream does not parse as a whole although \
                 every section does, which means two sections collide"
            );
        }
    };
    let body = prettyplease::unparse(&file);
    let forwarded = wire.commands.len() - hand.len() - refusals.commands.len();
    let header = format!(
        "// GENERATED by sv-codegen (pass 12, design.md §7.2 as corrected). Do not edit.\n\
         //\n\
         // Forwarded generically: {forwarded} commands\n\
         // Hand-written GPU side:  {} commands (exceptions::GPU_HAND_WRITTEN)\n\
         // Refused:                {} commands, {} struct raises\n\
         //\n\
         // The `raise_*` half of this file contains no `unsafe`: it allocates\n\
         // owned storage and takes its address, and taking an address is safe.\n\
         // The `unsafe` is the one line per command that calls the driver.\n",
        hand.len(),
        refusals.commands.len(),
        refusals.raise.len(),
    );
    let source = format!("{header}\n{body}");
    let generated_lines = source.lines().count();
    Ok(GeneratedGpu {
        source,
        refusals,
        forwarded,
        hand_written: hand.len(),
        generated_lines,
    })
}

fn preamble(profile: &Profile, hash: u64, plan: &plan::Plan) -> String {
    format!(
        "// GENERATED by sv-codegen. Do not edit.\n\
         //\n\
         // Profile:       {}\n\
         // protocol_hash: {hash:#018x}\n\
         // Commands:      {}\n\
         // Structs:       {}\n\
         //\n\
         // The hash above is computed over the protocol manifest, not over\n\
         // vk.xml, because the generator version matters as much as the\n\
         // registry version (design.md §4.3, security-model.md R13). Two builds\n\
         // whose hashes differ refuse each other at handshake rather than\n\
         // desynchronising.\n\
         //\n\
         // Regenerate with:  cargo run -p sv-codegen -- --profile {}\n",
        profile.name,
        plan.commands.len(),
        plan.structs.len(),
        profile.name,
    )
}

/// The profiles the CLI and the build script know by name.
pub fn profile_by_name(name: &str) -> Option<Profile> {
    match name {
        "zink" => Some(vk_registry::profiles::zink()),
        "step3-minimal" | "step3" => Some(vk_registry::profiles::step3_minimal()),
        _ => None,
    }
}

pub const PROFILE_NAMES: &[&str] = &["zink", "step3-minimal"];
