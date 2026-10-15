//! `exceptions::GPU_OBSERVED`, checked against what the generator emits.
//!
//! An entry is only useful if the generated forwarder for it calls
//! `GpuHost::observe`. A command that is hand-written (`GPU_HAND_WRITTEN`),
//! refused, or not in the profile has no generated body, so its entry would
//! silently observe nothing — the failure mode of an instrument that exists
//! for the one run where the driver dies (docs/video-decode.md §7.10).

use sv_codegen::exceptions;

#[test]
fn every_observed_command_is_generated_and_calls_the_observer() {
    let Ok(path) = vk_registry::registry_path() else {
        eprintln!("no vk.xml in this environment; skipping (VK_XML is set in nix develop)");
        return;
    };
    let reg = vk_registry::Registry::load_from(path).expect("vk.xml loads");
    let model = reg.model().expect("the model builds");
    let profile = sv_codegen::profile_by_name(sv_codegen::DEFAULT_PROFILE).expect("the default profile");
    let generated = sv_codegen::generate_gpu_entry(&model, &profile).expect("the GPU entry generates");
    let hand = exceptions::gpu_hand_written();
    // Whitespace-free, because the formatter wraps a long call differently
    // from a short one.
    let flat: String = generated.source.chars().filter(|c| !c.is_whitespace()).collect();
    for name in exceptions::GPU_OBSERVED {
        assert!(!hand.contains(name), "{name} is hand-written; it must call the observer itself");
        for phase in ["false", "true"] {
            let needle = format!("observe(\"{name}\",{phase}");
            assert!(flat.contains(&needle), "{name}: no observe(.., {phase}, ..) in its generated forwarder");
        }
    }
    // And nothing else is observed: the list is the whole of it.
    let calls = flat.matches(".observe(\"").count();
    assert_eq!(calls, 2 * exceptions::GPU_OBSERVED.len(), "one call before and one after per entry");
}
