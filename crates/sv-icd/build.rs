//! Write the ICD manifest the loader reads, with an absolute `library_path`.
//!
//! docs/design.md §8: the packaged driver installs a manifest into
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
//!   why (docs/design.md §8). The version here must match what
//!   `vkEnumerateInstanceVersion` returns, and there is a test that they agree.
//! - **`is_portability_driver`.** Setting it true makes us invisible to every
//!   application that has not set
//!   `VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR`, which is almost all of
//!   them.
//!
//! # The constraint docs/design.md §0.18 had to establish, from the loader's source
//!
//! §0.18 makes the advertised Vulkan version **negotiated** — the app side
//! reports `min(what the driver behind it supports, what the wire carries)`
//! instead of a constant. This file is the part that cannot be negotiated: the
//! manifest is a file on disk and the loader parses it before a line of ours
//! runs. So what, exactly, does the loader do with the number?
//!
//! Read out of `Vulkan-Loader`'s `loader/loader.c` (main, 8253 lines; this tree
//! links vulkan-loader 1.4.350.0), the manifest's `api_version` is stored
//! verbatim as `scanned_icd->api_version` in `loader_scanned_icd_add` and is
//! **never** refreshed from a runtime query on the manifest path. It is then
//! read in exactly three places, all in `terminator_CreateInstance`:
//!
//!  1. `if (icd_term->scanned_icd->api_version >= VK_API_VERSION_1_1)` — the
//!     gate on whether the loader asks us for `vkEnumerateInstanceVersion` **at
//!     all**. Below 1.1 our runtime answer is never read and we are a 1.0 driver
//!     forever.
//!  2. the same test again, for whether to mark
//!     `khr_get_physical_device_properties2` enabled on this ICD.
//!  3. a `== 1.0` test, on Linux-sort builds, that force-enables
//!     `VK_KHR_get_physical_device_properties2`.
//!
//! Three consequences, and they are the whole of the answer:
//!
//! - **Nothing compares the manifest number with the application's requested
//!   version, and nothing clamps `VkApplicationInfo::apiVersion` down to it.**
//!   The only rewrite of the application's requested version is guarded by
//!   `icd_version_nopatch == api_version_1_0`, where `icd_version` is what *our*
//!   `vkEnumerateInstanceVersion` returned. The runtime number does the real
//!   work; the manifest only decides whether the loader bothers to ask.
//! - So **the manifest must state the ceiling — the highest version this build
//!   could ever advertise — and not the negotiated one.** Understating it is
//!   the trap above; a manifest of 1.1 with a runtime answer of 1.3 would still
//!   be asked and still work, but a manifest below the ceiling buys nothing and
//!   a manifest below 1.1 is catastrophic. Overstating it is what a lower
//!   runtime number then corrects, at no cost. There is no reading of the loader
//!   under which a per-driver manifest could carry a negotiated number anyway:
//!   one file serves every GPU the machine has.
//! - **Graceful fallback is safe.** A driver that only does 1.1 gives a runtime
//!   answer of 1.1 against this 1.3 manifest; the loader's gate passes, no
//!   demotion fires (1.1 is not 1.0), and the application's requested version
//!   reaches us untouched. Per the loader's own policy `LDP_DRIVER_10`, at
//!   interface version ≥ 5 we must **not** answer `VK_ERROR_INCOMPATIBLE_DRIVER`
//!   for a version we do not support — that check is the loader's — so the
//!   1.1 device simply reports 1.1 and the application decides.
//!
//! And **no copy of the number is maintained by hand**. A build script cannot
//! read the library's constants — it runs first, and a build-dependency on
//! `sv-proto` would compile the ~100,000 generated lines a second time for the
//! host — but it can read a value another build script published, which is what
//! `links = "sv_proto"` and `DEP_SV_PROTO_CORE_VERSION` are for. The manifest
//! this file writes is therefore the profile's own core version, whatever
//! `SV_PROTO_PROFILE` selected, and `the_manifest_agrees_with_what_we_report`
//! has become a check on the mechanism rather than on somebody's memory.

use std::path::PathBuf;

/// The **ceiling**, as a string, taken from `sv-proto`'s build script.
///
/// `DEP_SV_PROTO_CORE_VERSION` is `vk_registry::profile::Profile::core_api_version`
/// over the profile the generator actually ran on, delivered here by
/// `links = "sv_proto"`. So `SV_PROTO_PROFILE=full` — core 1.0–1.4 — writes a
/// 1.4.0 manifest and a 1.4.0 `vkEnumerateInstanceVersion` in the same build,
/// with nothing to remember and nothing to move by hand.
///
/// This used to be the literal `"1.3.0"` with a test holding it in step. The
/// test is still there, because it also checks the ≥ 1.1 rule that the module
/// docs above explain — but it can no longer *fail* on the profile moving, which
/// is the point: docs/design.md §0.18 removed every hand-maintained copy of this
/// number and this was the last one.
fn manifest_api_version() -> String {
    // Missing means the dependency's build script did not run, which cannot
    // happen for a `links` dependency — but panicking with the reason beats a
    // manifest that silently says 0.0.0 and gets the driver demoted to 1.0.
    let raw = std::env::var("DEP_SV_PROTO_CORE_VERSION")
        .expect("DEP_SV_PROTO_CORE_VERSION: sv-proto declares links = \"sv_proto\"");
    let v: u32 = raw.parse().expect("DEP_SV_PROTO_CORE_VERSION is a packed u32");
    format!("{}.{}.{}", (v >> 22) & 0x7f, (v >> 12) & 0x3ff, v & 0xfff)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest_api_version = manifest_api_version();

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
         \"library_path\": \"{library_path}\",\n    \"api_version\": \"{manifest_api_version}\",\n    \
         \"library_arch\": \"64\",\n    \"is_portability_driver\": false\n  }}\n}}\n",
    );

    let manifest_path = profile_dir.join("sv_icd.json");
    if let Err(e) = std::fs::write(&manifest_path, manifest) {
        println!("cargo:warning=sv-icd: could not write {}: {e}", manifest_path.display());
        return;
    }
    println!("cargo:warning=sv-icd: manifest at {}", manifest_path.display());
    println!("cargo:rustc-env=SV_ICD_MANIFEST={}", manifest_path.display());
    println!("cargo:rustc-env=SV_ICD_MANIFEST_API_VERSION={manifest_api_version}");

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
    // `libsv_wlshim.so` (docs/waycompose-design.md §100), the LD_PRELOAD that
    // lets a Wayland present finish after vkQueuePresentKHR. C, because it
    // must define libwayland's variadic entry points, which stable Rust
    // cannot. Linux targets only; beside the ICD, where deploy and the
    // harnesses look for it.
    if target_os == "linux" {
        println!("cargo:rerun-if-changed=shim/sv_wlshim.c");
        // Not `CC`: the dev shell sets that to the mingw compiler, for the
        // Windows binaries. This is built for the Linux machine running cargo.
        println!("cargo:rerun-if-env-changed=CC_x86_64_unknown_linux_gnu");
        println!("cargo:rerun-if-env-changed=CC_FOR_BUILD");
        let cc = std::env::var("CC_x86_64_unknown_linux_gnu")
            .or_else(|_| std::env::var("CC_FOR_BUILD"))
            .unwrap_or_else(|_| "gcc".into());
        let shim = profile_dir.join("libsv_wlshim.so");
        let status = std::process::Command::new(&cc)
            .args(["-shared", "-fPIC", "-O2", "-Wall", "-Wextra", "-Werror", "-fvisibility=hidden", "-o"])
            .arg(&shim)
            .arg("shim/sv_wlshim.c")
            .arg("-lpthread")
            .status();
        match status {
            Ok(s) if s.success() => println!("cargo:rustc-env=SV_WLSHIM_PATH={}", shim.display()),
            Ok(s) => panic!("sv-icd: building {} failed ({s})", shim.display()),
            Err(e) => panic!("sv-icd: cannot run {cc} to build the Wayland shim: {e}"),
        }
    }

    println!("cargo:rustc-env=SV_ICD_LIBRARY_PATH={}", lib_path.display());
    println!(
        "cargo:rustc-env=SV_ICD_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_else(|_| "debug".into())
    );
}
