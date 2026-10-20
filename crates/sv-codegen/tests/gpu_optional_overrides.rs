//! Optional interceptors must see wire handles before ordinary forwarding.
#[test]
fn optional_hooks_precede_mapping_and_support_explicit_worker_bypass() {
    let path =
        vk_registry::registry_path().expect("VK_XML is required for this generator regression");
    let registry = vk_registry::Registry::load_from(path).unwrap();
    let model = registry.model().unwrap();
    let profile = sv_codegen::profile_by_name(sv_codegen::DEFAULT_PROFILE).unwrap();
    let generated = sv_codegen::generate_gpu_entry(&model, &profile).unwrap();
    let flat: String = generated
        .source
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    for method in [
        "bind_image_memory",
        "bind_image_memory2",
        "queue_submit",
        "queue_submit2",
        "queue_wait_idle",
        "device_wait_idle",
        "get_physical_device_video_capabilities_khr",
        "create_video_session_khr",
        "destroy_video_session_khr",
        "create_video_session_parameters_khr",
        "update_video_session_parameters_khr",
        "destroy_video_session_parameters_khr",
    ] {
        let start = flat.find(&format!("fn{method}(")).expect(method);
        let body = &flat[start..];
        let end = body[2..].find("fn").map_or(body.len(), |v| v + 2);
        let body = &body[..end];
        let hook = body
            .find(&format!("__h.override_{method}(&a)"))
            .expect(method);
        let mapping = body.find("crate::Mapping(").expect(method);
        assert!(hook < mapping, "{method} intercepted after mapping");
        assert!(
            body[..hook].contains("if!__h.bypass_optional_overrides()"),
            "{method} has no worker bypass"
        );
        let trait_hook = format!("fnoverride_{method}(");
        let trait_start = flat.find(&trait_hook).unwrap();
        assert!(
            flat[trait_start..]
                .split('}')
                .next()
                .unwrap()
                .contains("{None")
        );
    }
}

#[test]
fn ordinary_recording_preparation_precedes_mapping_and_follows_typed_override() {
    let path =
        vk_registry::registry_path().expect("VK_XML is required for this generator regression");
    let registry = vk_registry::Registry::load_from(path).unwrap();
    let model = registry.model().unwrap();
    let profile = sv_codegen::profile_by_name(sv_codegen::DEFAULT_PROFILE).unwrap();
    let generated = sv_codegen::generate_gpu_entry(&model, &profile).unwrap();
    let flat: String = generated
        .source
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    for method in [
        "cmd_draw",
        "cmd_copy_buffer",
        "cmd_execute_commands",
        "cmd_pipeline_barrier",
        "cmd_pipeline_barrier2",
    ] {
        let start = flat.find(&format!("fn{method}(")).unwrap();
        let body = &flat[start..];
        let end = body[2..].find("fn").map_or(body.len(), |v| v + 2);
        let body = &body[..end];
        let preparation = body.find("__h.prepare_recorded_command(").unwrap();
        let mapping = body.find("crate::Mapping(").unwrap();
        assert!(preparation < mapping);
        assert!(body[..preparation].contains("if!__h.bypass_optional_overrides()"));
        if method.contains("pipeline_barrier") || method == "cmd_execute_commands" {
            let typed = body.find(&format!("__h.override_{method}(&a)")).unwrap();
            assert!(typed < preparation);
        }
    }
}
