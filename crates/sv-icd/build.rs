//! Write the ICD manifest the loader reads, with an absolute `library_path`.
//!
//! design.md §8: the packaged driver installs a manifest into
//! `/usr/share/vulkan/icd.d/`, and the development loop points `VK_DRIVER_FILES`
//! at one the build generated with an absolute path — Kazan's clean pattern.
//!
//! The manifest goes next to the `cdylib` rather than into `OUT_DIR`, because
//! `OUT_DIR` is a build-script scratch directory whose name contains a hash
//! nobody can type. `target/<profile>/sv_icd.json` is a path a person can put in
//! an environment variable.
//!
//! # The two traps this file exists to avoid
//!
//! - **`api_version` demotion.** If the JSON says < 1.1 the loader demotes us to
//!   a 1.0 driver *and rewrites every `VkApplicationInfo::apiVersion` we
//!   receive* — so a 1.3 application silently becomes a 1.0 one and nothing says
//!   why (design.md §8). The version here must match what
//!   `vkEnumerateInstanceVersion` returns, and there is a test that they agree.
//! - **`is_portability_driver`.** Setting it true makes us invisible to every
//!   application that has not set
//!   `VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR`, which is almost all of
//!   them.

use std::path::PathBuf;

/// Must equal `advertise::API_VERSION`, checked by a test in the library.
const API_VERSION: &str = "1.3.0";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // OUT_DIR is `<target>/<profile>/build/<pkg>-<hash>/out`.
    let Some(profile_dir) = out.ancestors().nth(3) else {
        println!("cargo:warning=sv-icd: cannot locate the profile directory; no manifest written");
        return;
    };

    // `CARGO_CFG_TARGET_OS`, **not** `cfg!(target_os = …)`. A build script is
    // compiled for and runs on the *host*, so `cfg!` here answers a question
    // about this machine rather than about what is being built. Cross-compiling
    // the ICD to Windows wrote a manifest naming `libsv_icd.so`, and the
    // loader's symptom for a `library_path` that does not exist is to skip the
    // driver silently — no error, no device, nothing to grep for.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    let windows = target_os == "windows";
    let lib_name = if windows { "sv_icd.dll" } else { "libsv_icd.so" };
    let lib_path = profile_dir.join(lib_name);

    // On Windows the manifest is *not* given an absolute path. The binaries are
    // built here and copied to the guest, so a path into this machine's build
    // tree is wrong the moment it arrives; the loader resolves a bare filename
    // relative to the manifest's own directory, which is exactly the property a
    // copied pair of files needs. On Linux the absolute path is kept, because
    // there the manifest is consumed where it was built.
    let library_path =
        if windows { lib_name.to_string() } else { lib_path.display().to_string() };

    let manifest = format!(
        "{{\n  \"file_format_version\": \"1.0.1\",\n  \"ICD\": {{\n    \
         \"library_path\": \"{library_path}\",\n    \"api_version\": \"{API_VERSION}\",\n    \
         \"library_arch\": \"64\",\n    \"is_portability_driver\": false\n  }}\n}}\n",
    );

    let manifest_path = profile_dir.join("sv_icd.json");
    if let Err(e) = std::fs::write(&manifest_path, manifest) {
        println!("cargo:warning=sv-icd: could not write {}: {e}", manifest_path.display());
        return;
    }
    println!("cargo:warning=sv-icd: manifest at {}", manifest_path.display());
    println!("cargo:rustc-env=SV_ICD_MANIFEST={}", manifest_path.display());
    println!("cargo:rustc-env=SV_ICD_MANIFEST_API_VERSION={API_VERSION}");

    // The absolute path of the `cdylib` the manifest names, and the profile it
    // belongs to, so that `tests/loader.rs` can tell "the driver is broken"
    // apart from "the driver has not been built yet".
    //
    // `cargo test` builds the `rlib` — the integration test links against it —
    // and *not* the `cdylib`, which nothing in the test graph depends on. So on
    // a freshly cloned tree the loader finds a manifest whose `library_path`
    // does not exist, silently skips the driver, and `vkCreateInstance` returns
    // `VK_ERROR_INCOMPATIBLE_DRIVER`. That failure is about build ordering and
    // says nothing about the code — and because a failing test binary aborts
    // `cargo test --workspace`, it also truncated the reported test total to a
    // fraction of the real one. See `tests/loader.rs`.
    println!("cargo:rustc-env=SV_ICD_LIBRARY_PATH={}", lib_path.display());
    println!(
        "cargo:rustc-env=SV_ICD_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_else(|_| "debug".into())
    );
}
