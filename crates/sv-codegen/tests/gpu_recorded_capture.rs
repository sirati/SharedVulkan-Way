//! Owned capture must cover the complete served native recording surface.
use sv_codegen::{exceptions, naming};

#[test]
fn every_served_native_recording_moves_wire_arguments_before_mapping() {
    let registry = vk_registry::Registry::load().expect("VK_XML is required");
    let model = registry.model().unwrap();
    let profile = sv_codegen::profile_by_name(sv_codegen::DEFAULT_PROFILE).unwrap();
    let wire = sv_codegen::plan::build(&model, &profile.resolve(&model).unwrap()).unwrap();
    let generated = sv_codegen::generate_gpu_entry(&model, &profile).unwrap();
    let flat: String = generated
        .source
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .replace(",)", ")")
        .replace("=>{", "=>");
    let parsed = syn::parse_file(&generated.source).unwrap();
    let mut bodies = std::collections::BTreeMap::new();
    for item in parsed.items {
        let syn::Item::Impl(implementation) = item else {
            continue;
        };
        for item in implementation.items {
            let syn::ImplItem::Fn(method) = item else {
                continue;
            };
            let block = method.block;
            let body = quote::quote!(#block)
                .to_string()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
                .replace(",)", ")");
            bodies.insert(method.sig.ident.to_string(), body);
        }
    }
    let hand = exceptions::gpu_hand_written();
    let video = [
        "vkCmdBeginVideoCodingKHR",
        "vkCmdControlVideoCodingKHR",
        "vkCmdDecodeVideoKHR",
        "vkCmdEndVideoCodingKHR",
    ];
    let mut covered = 0;
    for command in wire.commands.iter().filter(|c| c.is_recorded) {
        let variant = naming::opcode_variant(&command.name);
        if video.contains(&command.name.as_str()) || hand.contains(&command.name.as_str()) {
            assert!(!flat.contains(&format!("{variant}(Box<sv_proto::{variant}Args>)")));
            continue;
        }
        if generated
            .refusals
            .commands
            .iter()
            .any(|r| r.command == command.name)
        {
            assert!(!flat.contains(&format!("{variant}(Box<sv_proto::{variant}Args>)")));
            continue;
        }
        covered += 1;
        assert!(!command.generates_reply);
        assert!(
            flat.contains(&format!("{variant}(Box<sv_proto::{variant}Args>)")),
            "{} missing owned variant",
            command.name
        );
        let method = naming::command_method(&command.name);
        let body = bodies.get(&method.to_string()).unwrap();
        let capture = body
            .find(&format!(
                "return__h.capture_recorded_command(RecordedCall::{variant}(Box::new(a)))"
            ))
            .unwrap_or_else(|| panic!("{} missing consuming capture", command.name));
        assert!(capture < body.find("__h.prepare_recorded_command(").unwrap());
        assert!(capture < body.find("crate::Mapping(").unwrap());
        assert!(body[..capture].contains(
            "!__h.bypass_optional_overrides()&&__h.capture_recorded_commands(a.command_buffer.0)"
        ));
        assert!(
            !body[..capture].contains(".clone()"),
            "ordinary capture test must not clone {}",
            command.name
        );
        if exceptions::GPU_OPTIONAL_OVERRIDES.contains(&command.name.as_str()) {
            assert!(body.find(&format!("__h.override_{method}(&a)")).unwrap() < capture);
        }
        assert!(flat.contains(&format!("Self::{variant}(a)=>a.command_buffer.0")));
        assert!(flat.contains(&format!(
            "Self::{variant}(a)=>sv_proto::Server::{method}(forward,*a)"
        )));
        let scope = model
            .commands
            .get(&command.name)
            .and_then(|c| c.video_coding.as_deref());
        let allowed = matches!(scope, Some("both" | "inside"));
        assert!(flat.contains(&format!("Self::{variant}(_)=>{allowed}")));
        let native_field = naming::snake_case(&command.name);
        assert!(flat.contains(&format!("Self::{variant}(_)=>fns.{native_field}.is_some()")));
        for alias in &command.aliases {
            let alias_variant = naming::opcode_variant(alias);
            assert!(
                !flat.contains(&format!("{alias_variant}(Box<")),
                "alias must share canonical variant"
            );
        }
    }
    assert!(
        covered > 100,
        "regression must check the full recorded surface"
    );
    assert!(flat.contains("Self::VkCmdBindPipeline(_)=>RecordedRestoration::AuditedState"));
    assert!(flat.contains("Self::VkCmdDraw(_)=>RecordedRestoration::Other"));
    for (names, classification) in [
        (exceptions::GPU_RECORDED_RESTORABLE_STATE, "AuditedState"),
        (exceptions::GPU_RECORDED_SCOPE_LOCAL_STATE, "ScopeLocal"),
        (
            exceptions::GPU_RECORDED_UNAUDITED_STATE_EFFECTS,
            "UnsupportedState",
        ),
    ] {
        for name in names {
            let variant = naming::opcode_variant(name);
            assert!(
                flat.contains(&format!(
                    "Self::{variant}(_)=>RecordedRestoration::{classification}"
                )),
                "missing explicit classification for {name}"
            );
        }
    }
    for name in [
        "VkCmdSetEvent",
        "VkCmdSetPerformanceMarkerINTEL",
        "VkCmdBindDescriptorBuffersEXT",
        "VkCmdBindTransformFeedbackBuffersEXT",
    ] {
        let classification = if name == "VkCmdSetEvent" {
            "Other"
        } else {
            "UnsupportedState"
        };
        assert!(flat.contains(&format!(
            "Self::{name}(_)=>RecordedRestoration::{classification}"
        )));
    }
    for method in [
        "cmd_set_performance_marker_intel",
        "cmd_set_performance_override_intel",
        "cmd_set_performance_stream_marker_intel",
    ] {
        let body = bodies.get(&method.to_string()).unwrap();
        let failed = body.find("if__r.0<0").unwrap();
        assert!(body[failed..].contains("__h.poison_command_buffer(a.command_buffer.0,"));
        assert!(
            body[failed..].contains(
                "if__h.bypass_optional_overrides(){returnErr(sv_wire::WireError::Table);}"
            )
        );
    }
}
