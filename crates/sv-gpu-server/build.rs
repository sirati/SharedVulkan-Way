//! Compile the compute shader the arena probe dispatches.
//!
//! **The shader is `gpu-probe`'s, by path, not a copy.** `gpu-probe/build.rs`
//! already checks that every constant in `shaders/arena.comp` also appears in
//! `arena-record/src/gpu.rs`, so the GLSL and the Rust reference cannot drift.
//! Copying the file here would produce a third party to that agreement whose
//! drift nothing checks — and the whole point of the workload is that three
//! independent implementations of one function agree, so a silent fourth is
//! exactly the wrong thing to add.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let src = manifest.join("../gpu-probe/shaders/arena.comp");
    let spv = out.join("arena.spv");

    println!("cargo:rerun-if-changed={}", src.display());

    // `glslangValidator` comes from the dev shell. If it is missing the probe
    // binary should still build — it is one binary of several in this crate and
    // the ring consumers do not need a shader — so this warns rather than
    // panics, and the probe reports the absence at run time.
    let built = match Command::new("glslangValidator")
        .args(["--target-env", "vulkan1.1", "-o"])
        .arg(&spv)
        .arg(&src)
        .status()
    {
        Ok(s) if s.success() => true,
        Ok(s) => {
            println!("cargo:warning=glslangValidator failed ({s}) on {}", src.display());
            false
        }
        Err(e) => {
            println!(
                "cargo:warning=glslangValidator not found ({e}); build inside `nix develop` \
                 for the arena probe's shader"
            );
            false
        }
    };
    // The module is `include_bytes!`d rather than read at run time, and that is
    // not a micro-optimisation: the probe runs inside a bubblewrap sandbox that
    // binds `/nix` and the binary directory and nothing else, so an absolute
    // path into `target/debug/build/…/out` does not exist there. Reading it at
    // run time worked on the host and failed in the container with `ENOENT`
    // three frames from the cause. A file that must exist for the include to
    // compile is written either way.
    if !built {
        std::fs::write(&spv, []).expect("write an empty module placeholder");
    }
}
