//! Compile the compute shader, and refuse to build if it has drifted from the
//! Rust definition of the same arithmetic.
//!
//! The shader is GLSL in the tree rather than a checked-in `.spv` blob, because
//! the whole value of the measurement is that a reader can see what the GPU was
//! asked to compute. `glslangValidator` comes from the dev shell (`flake.nix`),
//! so this needs no network and no ambient toolchain.
//!
//! The constant check below is crude on purpose. The real invariant — that
//! `transform` in GLSL and `transform` in Rust are the same function — cannot
//! be proved by a build script. What it *can* do is catch the realistic
//! failure: somebody edits one of the two multipliers or the shift, the two
//! sides silently disagree, and every page comes back "wrong" in a way that
//! reads as a GPU or coherency fault rather than as a typo. So every magic
//! number the two share is required to appear in both files.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let src = manifest.join("shaders/arena.comp");
    let rust = manifest.join("../arena-record/src/gpu.rs");
    let spv = out.join("arena.spv");

    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-changed={}", rust.display());

    check_constants_agree(&src, &rust);

    // Vulkan 1.1 accepts SPIR-V 1.3; asking for it rather than the default
    // keeps the module inside what the target actually promises to consume.
    let status = Command::new("glslangValidator")
        .args(["--target-env", "vulkan1.1", "-o"])
        .arg(&spv)
        .arg(&src)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("glslangValidator failed ({s}) on {}", src.display()),
        Err(e) => panic!(
            "could not run glslangValidator: {e}\n\
             It comes from the dev shell — build inside `nix develop`."
        ),
    }
    println!("cargo:rustc-env=ARENA_SPV={}", spv.display());
}

/// Every constant the shader and the Rust share has to be present in both.
fn check_constants_agree(glsl_path: &Path, rust_path: &Path) {
    let glsl = std::fs::read_to_string(glsl_path).expect("read the shader");
    let rust = std::fs::read_to_string(rust_path).expect("read arena-record/src/gpu.rs");
    // (in the shader, in the Rust) — spelt differently in the two languages,
    // which is exactly why this cannot be a plain string comparison.
    let pairs = [
        ("0x2545F491u", "0x2545_F491"),
        ("0x9E3779B1u", "0x9E37_79B1"),
        ("0x85EBCA6Bu", "0x85EB_CA6B"),
        // The seed that stops zero being a fixed point. If this ever vanishes
        // from one side, an untouched page starts passing on the other.
        ("0x27D4EB2Fu", "0x27D4_EB2F"),
        // The header skip. 1023 and 64 in the shader; the Rust states it as
        // WORDS_PER_PAGE/HEADER_WORDS and pins the bitwise form in a test.
        ("1023u", "1023"),
    ];
    for (g, r) in pairs {
        assert!(
            glsl.contains(g),
            "{} no longer contains {g}: the shader and arena_record::gpu have drifted",
            glsl_path.display()
        );
        assert!(
            rust.contains(r),
            "{} no longer contains {r}: the shader and arena_record::gpu have drifted",
            rust_path.display()
        );
    }
    // The shifts, which a typo would change without changing any constant above.
    for s in [">> 15", ">> 13", ">> 16"] {
        assert!(glsl.contains(s), "the shader lost the shift {s}");
        assert!(rust.contains(s), "arena_record::gpu lost the shift {s}");
    }
}
