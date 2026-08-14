//! Run the generator.
//!
//! The generated protocol is not checked in. That is a deliberate choice with a
//! real cost — a reviewer cannot read the diff of a change to the wire format
//! from `git log` alone — and the reasons it still wins:
//!
//! - It is ~100,000 lines. Checked in, every registry bump is an unreviewable
//!   diff, and the temptation to hand-edit it becomes irresistible the first
//!   time something does not compile.
//! - The registry is pinned by nix, so the input is already reproducible. The
//!   thing a checked-in copy would protect against — "which vk.xml was this
//!   built against?" — is already prevented upstream.
//! - The *reviewable* artefact is the protocol manifest, which is written next
//!   to the source in `OUT_DIR` and is two orders of magnitude smaller. That is
//!   the file to diff when the wire format moves, and it is the file the
//!   protocol hash is computed over, so nothing can change the bytes without
//!   changing it.
//!
//! `cargo run -p sv-codegen -- --out somewhere` produces the same source for
//! reading.

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    // The registry is the only input, and it must be the pinned one.
    println!("cargo:rerun-if-env-changed=VK_XML");
    println!("cargo:rerun-if-env-changed=SV_PROTO_PROFILE");
    println!("cargo:rerun-if-changed=build.rs");

    let profile_name =
        std::env::var("SV_PROTO_PROFILE")
        .unwrap_or_else(|_| sv_codegen::DEFAULT_PROFILE.to_string());
    let profile = sv_codegen::profile_by_name(&profile_name)
        .with_context(|| format!("unknown SV_PROTO_PROFILE {profile_name:?}"))?;

    let registry = vk_registry::Registry::load().context(
        "sv-proto cannot be built outside `nix develop`: the protocol is generated \
         from $VK_XML, which the dev shell pins to the vulkan-headers in the nix \
         store. There is deliberately no fallback to a system path — see \
         crates/vk-registry/src/lib.rs.",
    )?;
    println!("cargo:rerun-if-changed={}", registry.path.display());

    let model = registry.model().context("building the typed model")?;
    let generated = sv_codegen::generate_from_model(&model, &profile)?;

    // The determinism claim, checked on every build rather than asserted in a
    // comment. It is cheap next to compiling the result.
    let again = sv_codegen::generate_from_model(&model, &profile)?;
    if again.source != generated.source || again.protocol_hash != generated.protocol_hash {
        bail!(
            "sv-codegen is not deterministic: two runs over one model produced \
             different output. The generator is a build input and its output must \
             be a function of the registry alone."
        );
    }

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("sv_proto.rs"), &generated.source)?;
    std::fs::write(out.join("protocol-manifest.txt"), &generated.manifest)?;

    // Make the hash available to the handshake without a second parse.
    println!("cargo:rustc-env=SV_PROTOCOL_HASH={}", generated.protocol_hash);
    println!("cargo:rustc-env=SV_PROTOCOL_PROFILE={}", profile.name);
    // The highest core Vulkan version the generated wire carries, which is what
    // the app side is allowed to advertise (docs/design.md §0.18). It is *not* in the
    // protocol hash and does not need to be: a profile that changes its core
    // version changes its command table, and the hash is over that.
    println!("cargo:rustc-env=SV_PROTOCOL_CORE_VERSION={}", profile.core_api_version());
    // The same number again, this time for `sv-icd`'s **build script**, which
    // has to write it into the ICD manifest — a JSON file the Vulkan loader
    // reads before a line of Rust runs, so it cannot be negotiated and cannot be
    // read out of a constant either. `links = "sv_proto"` in Cargo.toml is what
    // makes this reach a dependent build script, as `DEP_SV_PROTO_CORE_VERSION`.
    // It is the only way Cargo offers, and the alternative — a literal in
    // `sv-icd/build.rs` kept in step by a test — is the hand-maintained coupling
    // docs/design.md §0.18 exists to remove.
    println!("cargo:core_version={}", profile.core_api_version());
    println!(
        "cargo:warning=sv-proto: profile {}, {} commands, {} structs, hash {:#018x}",
        profile.name,
        generated.stats.commands,
        generated.stats.structs,
        generated.protocol_hash
    );
    Ok(())
}
