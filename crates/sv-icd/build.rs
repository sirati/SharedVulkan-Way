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

    let lib_name = if cfg!(target_os = "windows") { "sv_icd.dll" } else { "libsv_icd.so" };
    let lib_path = profile_dir.join(lib_name);

    let manifest = format!(
        "{{\n  \"file_format_version\": \"1.0.1\",\n  \"ICD\": {{\n    \
         \"library_path\": \"{}\",\n    \"api_version\": \"{API_VERSION}\",\n    \
         \"library_arch\": \"64\",\n    \"is_portability_driver\": false\n  }}\n}}\n",
        lib_path.display()
    );

    let manifest_path = profile_dir.join("sv_icd.json");
    if let Err(e) = std::fs::write(&manifest_path, manifest) {
        println!("cargo:warning=sv-icd: could not write {}: {e}", manifest_path.display());
        return;
    }
    println!("cargo:warning=sv-icd: manifest at {}", manifest_path.display());
    println!("cargo:rustc-env=SV_ICD_MANIFEST={}", manifest_path.display());
    println!("cargo:rustc-env=SV_ICD_MANIFEST_API_VERSION={API_VERSION}");
}
