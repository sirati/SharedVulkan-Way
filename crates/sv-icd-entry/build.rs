//! Generate pass 10 (docs/design.md §7.2) into `OUT_DIR`.
//!
//! The same rules as `sv-proto`'s build script and for the same reasons: the
//! registry is the only input, it must be the pinned one, and the output is not
//! checked in because it is tens of thousands of lines nobody would review as a
//! diff.
//!
//! Unlike `sv-proto` this crate's output is *not* part of the protocol hash. The
//! C ABI is a contract with the application, not with the peer, and two builds
//! of `sv-icd` that disagree about it do not desynchronise a wire — they fail to
//! load. The thing that would make them disagree is the registry, which nix
//! pins.

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    println!("cargo:rerun-if-env-changed=VK_XML");
    println!("cargo:rerun-if-env-changed=SV_PROTO_PROFILE");
    println!("cargo:rerun-if-changed=build.rs");

    let profile_name = std::env::var("SV_PROTO_PROFILE")
        .unwrap_or_else(|_| sv_codegen::DEFAULT_PROFILE.to_string());
    let profile = sv_codegen::profile_by_name(&profile_name)
        .with_context(|| format!("unknown SV_PROTO_PROFILE {profile_name:?}"))?;
    // Which profile this build was generated from, readable from inside the
    // crate. The tests need it: "these are exactly the stubbed entry points" is
    // a true and useful assertion, and it is a *different* list per profile.
    println!("cargo:rustc-env=SV_ICD_ENTRY_PROFILE={profile_name}");

    let registry = vk_registry::Registry::load().context(
        "sv-icd-entry cannot be built outside `nix develop`: its C ABI is \
         generated from $VK_XML, which the dev shell pins to the vulkan-headers \
         in the nix store.",
    )?;
    println!("cargo:rerun-if-changed={}", registry.path.display());

    let model = registry.model().context("building the typed model")?;
    let generated = sv_codegen::generate_icd_entry(&model, &profile)?;

    // The same determinism contract `sv-proto` asserts: the ABI a `cdylib`
    // exports must be a function of the registry alone, or two builds of the
    // same source disagree about the layout of a struct an application passes.
    let again = sv_codegen::generate_icd_entry(&model, &profile)?;
    if again.source != generated.source {
        bail!("sv-codegen's ICD entry pass is not deterministic");
    }

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("sv_icd_entry.rs"), &generated.source)?;

    // A machine-readable record of what the generator refused, next to the
    // source, so "what is stubbed" is a file rather than a memory.
    let mut report = String::new();
    report.push_str("# entry points with a stub that refuses\n");
    for r in &generated.refusals.commands {
        report.push_str(&format!("{}\t{}\n", r.command, r.reason));
    }
    report.push_str("\n# structs with no lower_*\n");
    for (n, why) in &generated.refusals.lower {
        report.push_str(&format!("{n}\t{why}\n"));
    }
    report.push_str("\n# structs with no store_*\n");
    for (n, why) in &generated.refusals.store {
        report.push_str(&format!("{n}\t{why}\n"));
    }
    std::fs::write(out.join("icd-entry-refusals.txt"), &report)?;

    println!(
        "cargo:warning=sv-icd-entry: {} commands, {} types, {} refused, {} lines",
        generated.commands,
        generated.types,
        generated.refusals.commands.len(),
        generated.generated_lines
    );
    Ok(())
}
