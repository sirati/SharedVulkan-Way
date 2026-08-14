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
    let mut gap = false;
    let mut audit = false;

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
            "--gap" | "gap" => gap = true,
            "--audit" | "audit" => audit = true,
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

    // What the profile does not carry, as data rather than as prose.
    //
    // docs/design.md §11 declares the scope and `vk_registry::profiles::zink` *is*
    // the declaration, but until now the only way to ask "which commands are
    // out?" was to read the list and subtract it from the registry in your head.
    // That is the question `goal.md`'s "a lot of functions are still missing"
    // asks, and it deserves a number, an ordering, and a reason per line.
    //
    // The grouping is by the extension or core version that *introduces* each
    // missing command, because that is the unit a profile is edited in: nobody
    // adds `vkCmdTraceRaysKHR`, they add `VK_KHR_ray_tracing_pipeline`. A flat
    // list of 500 names would be true and useless.
    // Which extensions the generator can actually carry, asked one at a time.
    //
    // Widening a profile is cheap when it works and opaque when it does not:
    // adding thirty extensions at once and getting one error tells you about
    // one of them. This asks the question per extension — resolve, plan the
    // wire, plan the C ABI — and prints a verdict per line, so the profile can
    // be edited from a list of facts rather than by bisecting a build failure.
    //
    // Emission is deliberately skipped. Every failure this project has hit came
    // from `plan::build` (a type whose layout the registry does not state) or
    // from `abi::build` (an `Opaque` used by value); nothing has ever failed for
    // the first time inside `prettyplease`, and skipping it turns a ten-minute
    // audit into a twenty-second one.
    if audit {
        let registry = vk_registry::Registry::load()?;
        let model = registry.model()?;
        let already: std::collections::BTreeSet<&str> =
            profile.extensions.iter().map(String::as_str).collect();

        let mut ok = Vec::new();
        let mut bad: Vec<(String, String)> = Vec::new();
        for ext in &model.extensions {
            if !ext.supported_for_vulkan || already.contains(ext.name.as_str()) {
                continue;
            }
            let mut p = profile.clone();
            p.name = format!("{}+{}", profile.name, ext.name);
            p.extensions.push(ext.name.clone());
            let verdict = (|| -> Result<()> {
                let resolved = p.resolve(&model)?;
                let wire = sv_codegen::plan::build(&model, &resolved)?;
                let _ = wire;
                let forwarded: std::collections::BTreeSet<String> =
                    resolved.commands.iter().cloned().collect();
                let surface = sv_codegen::abi::surface_profile(&p);
                sv_codegen::abi::build(&model, &surface, &forwarded)?;
                Ok(())
            })();
            let platform = ext.platform.clone().unwrap_or_default();
            match verdict {
                Ok(()) => ok.push((ext.name.clone(), platform)),
                Err(e) => {
                    // One line. The chain's root is the fact; the context above
                    // it is just "resolving profile zink+X", which the name
                    // already says.
                    let root = e.chain().last().map(|c| c.to_string()).unwrap_or_default();
                    bad.push((ext.name.clone(), root.replace('\n', " ")));
                }
            }
        }
        println!("base profile          {}", profile.name);
        println!("carryable             {}", ok.len());
        println!("not carryable         {}", bad.len());
        println!();
        for (name, platform) in &ok {
            let p = if platform.is_empty() { String::new() } else { format!(" [{platform}]") };
            println!("ok    {name}{p}");
        }
        for (name, why) in &bad {
            println!("FAIL  {name}: {why}");
        }
        return Ok(());
    }

    if gap {
        let registry = vk_registry::Registry::load()?;
        let model = registry.model()?;
        let resolved = profile.resolve(&model)?;

        // Every command the registry defines for `vulkan`, and where it first
        // appears. A command can be required by several features; the first
        // wins, and the sort makes "first" mean the same thing on every run.
        let mut origin: std::collections::BTreeMap<&str, &str> = Default::default();
        let sets = model.features.iter().chain(model.extensions.iter());
        for f in sets.filter(|f| f.supported_for_vulkan) {
            for b in &f.requires {
                for it in &b.items {
                    if let vk_registry::InterfaceRef::Command(c) = it {
                        origin.entry(c.as_str()).or_insert(f.name.as_str());
                    }
                }
            }
        }

        let defined: Vec<&str> = model.commands.keys().map(String::as_str).collect();
        let carried: Vec<&str> =
            defined.iter().copied().filter(|c| resolved.has_command(c)).collect();
        let mut missing_by_set: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        let mut unreachable = 0usize;
        for c in defined.iter().copied() {
            if resolved.has_command(c) {
                continue;
            }
            match origin.get(c) {
                Some(set) => missing_by_set.entry(set).or_default().push(c),
                // Defined but required by nothing this registry supports for
                // `vulkan` — a Vulkan SC command, or one behind a `disabled`
                // extension. Counted, not listed: no profile can reach it.
                None => unreachable += 1,
            }
        }

        println!("profile               {}", profile.name);
        println!("registry commands     {}", defined.len());
        println!("carried by profile    {}", carried.len());
        println!(
            "not carried           {} ({} of them unreachable by any profile)",
            defined.len() - carried.len(),
            unreachable
        );
        println!();
        println!("what would have to be added, by the thing that introduces it:");
        let mut rows: Vec<(&str, Vec<&str>)> = missing_by_set.into_iter().collect();
        // Largest first — the ordering a person editing the profile wants.
        rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
        for (set, mut cmds) in rows {
            cmds.sort_unstable();
            let platform = model
                .extensions
                .iter()
                .chain(model.features.iter())
                .find(|f| f.name == set)
                .and_then(|f| f.platform.clone())
                .map(|p| format!(" [{p}]"))
                .unwrap_or_default();
            println!("  {:>3}  {set}{platform}", cmds.len());
            println!("       {}", cmds.join(", "));
        }
        return Ok(());
    }

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
