//! Generate pass 12 (docs/design.md §7.2, as corrected) into `OUT_DIR`.
//!
//! Same rules as `sv-icd-entry`'s build script: the pinned registry is the only
//! input, the output is not checked in, and determinism is asserted rather than
//! assumed.

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    println!("cargo:rerun-if-env-changed=VK_XML");
    println!("cargo:rerun-if-env-changed=SV_PROTO_PROFILE");
    println!("cargo:rerun-if-changed=build.rs");

    let profile_name = std::env::var("SV_PROTO_PROFILE")
        .unwrap_or_else(|_| sv_codegen::DEFAULT_PROFILE.to_string());
    let profile = sv_codegen::profile_by_name(&profile_name)
        .with_context(|| format!("unknown SV_PROTO_PROFILE {profile_name:?}"))?;

    let registry = vk_registry::Registry::load().context(
        "sv-gpu-entry cannot be built outside `nix develop`: the C signatures it \
         calls a driver with are generated from $VK_XML.",
    )?;
    println!("cargo:rerun-if-changed={}", registry.path.display());

    let model = registry.model().context("building the typed model")?;
    let generated = sv_codegen::generate_gpu_entry(&model, &profile)?;

    let again = sv_codegen::generate_gpu_entry(&model, &profile)?;
    if again.source != generated.source {
        bail!("sv-codegen's GPU entry pass is not deterministic");
    }

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("sv_gpu_entry.rs"), &generated.source)?;

    let mut report = String::new();
    report.push_str("# commands with no generated forwarding\n");
    for r in &generated.refusals.commands {
        report.push_str(&format!("{}\t{}\n", r.command, r.reason));
    }
    report.push_str("\n# structs with no raise_*\n");
    for (n, why) in &generated.refusals.raise {
        report.push_str(&format!("{n}\t{why}\n"));
    }
    std::fs::write(out.join("gpu-entry-refusals.txt"), &report)?;

    println!(
        "cargo:warning=sv-gpu-entry: {} forwarded, {} hand-written, {} refused, {} lines",
        generated.forwarded,
        generated.hand_written,
        generated.refusals.commands.len(),
        generated.generated_lines
    );
    Ok(())
}
