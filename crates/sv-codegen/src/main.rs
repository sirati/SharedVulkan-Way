//! `sv-codegen` — run the protocol generator by hand.
//!
//! `sv-proto`'s build script calls the library directly; this binary exists so
//! that a person can look at the output, diff two manifests, and check the
//! determinism claim without a build in the way.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut profile_name = "zink".to_string();
    let mut out_dir: Option<PathBuf> = None;
    let mut check_determinism = false;
    let mut stats_only = false;
    let mut icd_entry = false;
    let mut gpu_entry = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" | "-p" => {
                profile_name = args.next().context("--profile needs a name")?;
            }
            "--out" | "-o" => {
                out_dir = Some(PathBuf::from(args.next().context("--out needs a directory")?));
            }
            "--icd-entry" => icd_entry = true,
            "--gpu-entry" => gpu_entry = true,
            "--check-determinism" => check_determinism = true,
            "--stats" => stats_only = true,
            "--list-profiles" => {
                for p in sv_codegen::PROFILE_NAMES {
                    println!("{p}");
                }
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "sv-codegen [--profile NAME] [--out DIR] [--stats] [--check-determinism]\n\
                     \n\
                     Reads $VK_XML (set by `nix develop`) and emits the protocol.\n\
                     Profiles: {}",
                    sv_codegen::PROFILE_NAMES.join(", ")
                );
                return Ok(());
            }
            other => bail!("unrecognised argument {other:?}; try --help"),
        }
    }

    let profile = sv_codegen::profile_by_name(&profile_name).with_context(|| {
        format!("unknown profile {profile_name:?}; try one of {:?}", sv_codegen::PROFILE_NAMES)
    })?;

    if gpu_entry {
        let registry = vk_registry::Registry::load()?;
        let model = registry.model()?;
        let g = sv_codegen::generate_gpu_entry(&model, &profile)?;
        eprintln!("forwarded generically {} commands", g.forwarded);
        eprintln!("hand-written GPU side {} commands", g.hand_written);
        eprintln!("generated lines       {}", g.generated_lines);
        eprintln!("refused commands      {}", g.refusals.commands.len());
        for r in &g.refusals.commands {
            eprintln!("  {:<48} {}", r.command, r.reason);
        }
        eprintln!("refused raises        {}", g.refusals.raise.len());
        for (n, why) in &g.refusals.raise {
            eprintln!("  {n:<48} {why}");
        }
        match out_dir {
            Some(dir) => {
                std::fs::create_dir_all(&dir)?;
                let rs = dir.join("sv_gpu_entry.rs");
                std::fs::write(&rs, &g.source)?;
                eprintln!("wrote                 {}", rs.display());
            }
            None => print!("{}", g.source),
        }
        return Ok(());
    }

    if icd_entry {
        let registry = vk_registry::Registry::load()?;
        let model = registry.model()?;
        let g = sv_codegen::generate_icd_entry(&model, &profile)?;
        eprintln!("icd surface           {} commands, {} types", g.commands, g.types);
        eprintln!("generated lines       {}", g.generated_lines);
        eprintln!("refused commands      {}", g.refusals.commands.len());
        for r in &g.refusals.commands {
            eprintln!("  {:<48} {}", r.command, r.reason);
        }
        eprintln!("refused lowerings     {}", g.refusals.lower.len());
        for (n, why) in &g.refusals.lower {
            eprintln!("  {n:<48} {why}");
        }
        eprintln!("refused writebacks    {}", g.refusals.store.len());
        for (n, why) in &g.refusals.store {
            eprintln!("  {n:<48} {why}");
        }
        match out_dir {
            Some(dir) => {
                std::fs::create_dir_all(&dir)?;
                let rs = dir.join("sv_icd_entry.rs");
                std::fs::write(&rs, &g.source)?;
                eprintln!("wrote                 {}", rs.display());
            }
            None => print!("{}", g.source),
        }
        return Ok(());
    }

    let generated = sv_codegen::generate(&profile)?;
    let s = &generated.stats;

    eprintln!("profile               {}", profile.name);
    eprintln!("protocol_hash         {:#018x}", generated.protocol_hash);
    eprintln!("commands              {} (+{} aliases)", s.commands, s.command_aliases);
    eprintln!("structs               {}", s.structs);
    eprintln!("unions                {}", s.unions);
    eprintln!("enums / bitmasks      {} / {}", s.enums, s.bitmasks);
    eprintln!("handles               {}", s.handles);
    eprintln!("pNext link types      {}", s.pnext_links);
    eprintln!("needing _partial      {}", s.structs_needing_partial);
    eprintln!("with handles          {}", s.structs_with_handles);
    eprintln!("with VkDeviceMemory   {}", s.structs_with_device_memory);
    eprintln!("exceptions consulted  {}", s.exceptions_consulted);
    eprintln!("generated lines       {}", s.generated_lines);

    if check_determinism {
        let again = sv_codegen::generate(&profile)?;
        if again.source != generated.source {
            bail!(
                "the generator is not deterministic: two runs over the same registry \
                 produced different source. This breaks the build-input contract and \
                 would make the protocol hash meaningless."
            );
        }
        if again.protocol_hash != generated.protocol_hash {
            bail!("the protocol hash is not deterministic");
        }
        eprintln!("determinism           ok (two runs, byte-identical)");
    }

    if stats_only {
        return Ok(());
    }

    match out_dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;
            let rs = dir.join("sv_proto.rs");
            let mf = dir.join("protocol-manifest.txt");
            std::fs::write(&rs, &generated.source)
                .with_context(|| format!("writing {}", rs.display()))?;
            std::fs::write(&mf, &generated.manifest)
                .with_context(|| format!("writing {}", mf.display()))?;
            eprintln!("wrote                 {}", rs.display());
            eprintln!("wrote                 {}", mf.display());
        }
        None => print!("{}", generated.source),
    }
    Ok(())
}
