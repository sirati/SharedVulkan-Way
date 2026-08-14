//! The `void*` family, asked of the registry rather than of the table.
//!
//! docs/design.md §7.4 makes an unlisted `void*` a build failure, which is a
//! strong rule with one weakness: it only fires for the `void*` a *profile*
//! reaches. Everything here asks the whole registry instead, so a policy that is
//! wrong — or right for a reason that has stopped being true — is found by the
//! test suite rather than by the next profile widening.
//!
//! docs/design.md §0.25 is what these check.

use sv_codegen::exceptions::{self, VoidPolicy};
use vk_registry::model::{Model, TypeDef};

fn model() -> Option<Model> {
    let path = vk_registry::registry_path().ok()?;
    let reg = vk_registry::Registry::load_from(path).ok()?;
    reg.model().ok()
}

/// Every `(owner, member)` in the registry that is a `void*` with no `len=` —
/// the exact population `exceptions::void_policy` exists for.
fn every_void_star(m: &Model) -> Vec<(String, String, Option<VoidPolicy>)> {
    let mut out = Vec::new();
    let mut note = |owner: &str, member: &vk_registry::model::Member| {
        // A `void**` is a `void*` one level down; both reach the policy table.
        if !member.ty.base().is_void() || member.ty.indirection() == 0 {
            return;
        }
        if member.name == "pNext" {
            return;
        }
        // A length at *any* level makes it a blob rather than a policy case at
        // that level; the planner asks per level and so does this.
        if member.len.as_ref().is_some_and(|l| l.term(member.ty.indirection() - 1).is_some()) {
            return;
        }
        out.push((
            owner.to_string(),
            member.name.clone(),
            exceptions::void_policy(owner, &member.name),
        ));
    };

    for (name, def) in &m.types {
        if let TypeDef::Struct(s) = def {
            for member in &s.members {
                note(name, member);
            }
        }
    }
    for (name, cmd) in &m.commands {
        for p in &cmd.params {
            note(name, p);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// **A `NotSerialisable` command parameter is deleted, not refused, and deleting
/// an output is how a command becomes a no-op that nobody notices.**
///
/// The `ArenaOffset` version of this defect is docs/design.md §0.22's: a policy
/// naming a treatment that nothing implemented, read by the next person as
/// evidence that something did. This is the same shape one policy over, and the
/// answer is the same — the claim is a list, and the list is checked.
#[test]
fn a_not_serialisable_void_parameter_is_never_silently_dropped() {
    let Some(m) = model() else {
        eprintln!("VK_XML unset — skipping (run inside `nix develop`)");
        return;
    };
    let full = vk_registry::profiles::full().resolve(&m).expect("the full profile resolves");

    for (owner, member, policy) in every_void_star(&m) {
        if policy != Some(VoidPolicy::NotSerialisable) || !m.commands.contains_key(&owner) {
            continue;
        }
        if exceptions::parameter_answered_app_side(&owner).is_some() {
            continue;
        }
        // Not answered app-side, so dropping the parameter would answer nobody.
        // `plan::build` refuses such a command outright; this is the same
        // invariant stated where it can be read, with the profile that would
        // have to change named in it.
        assert!(
            !full.has_command(&owner),
            "the full profile carries {owner}, whose {member} is a \
             VoidPolicy::NotSerialisable parameter with nothing app-side to produce it. \
             That combination forwards a command which never answers its caller — either \
             add {owner} to PARAMETER_ANSWERED_APP_SIDE with what answers it, or leave the \
             extension in Profile::excluded_extensions with the reason."
        );
    }
}

/// Every command named in `PARAMETER_ANSWERED_APP_SIDE` still has the parameter
/// the entry is about.
///
/// The list is a claim about `sv-icd`; this is the half of it the registry can
/// check. An entry whose command has no dropped `void*` any more is a stale
/// permission, and a stale permission is exactly what would let the next one
/// through unnoticed.
#[test]
fn every_answered_parameter_entry_still_has_a_parameter_to_answer() {
    let Some(m) = model() else {
        eprintln!("VK_XML unset — skipping (run inside `nix develop`)");
        return;
    };
    let all = every_void_star(&m);
    for (command, why) in exceptions::PARAMETER_ANSWERED_APP_SIDE {
        // Aliases live in `Model::command_aliases`, not in `commands`;
        // `vkMapMemory2KHR` is one and is a real entry, not a stale one.
        let canonical = m.canonical_command(command);
        assert!(
            all.iter().any(|(owner, _, policy)| {
                *owner == canonical && *policy == Some(VoidPolicy::NotSerialisable)
            }),
            "PARAMETER_ANSWERED_APP_SIDE says {command} answers a dropped void* app-side \
             ({why}), but it has no NotSerialisable void* parameter in this registry"
        );
    }
}

/// The capture-replay class, enumerated.
///
/// `exceptions::is_capture_replay` is a *rule* over Khronos's naming rather than
/// a list, which is what keeps it from growing once per vendor
/// (`VK_ARM_tensors` copied the pattern and the list would have missed it). The
/// cost of a rule is that nobody can see what it matches, so this test is the
/// list — regenerated by reading, not by hand — and it fails when the rule
/// starts or stops matching something.
///
/// A new member appearing here is not a defect; it is the class growing, and the
/// two things to check when it does are that the member really is a
/// capture-replay token and that `sv_icd::advertise::withdraw_gated_features`
/// withdraws whatever feature would make an application pass one.
#[test]
fn the_capture_replay_class_is_what_the_naming_rule_matches() {
    let Some(m) = model() else {
        eprintln!("VK_XML unset — skipping (run inside `nix develop`)");
        return;
    };
    let matched: Vec<String> = every_void_star(&m)
        .into_iter()
        .filter(|(_, _, p)| *p == Some(VoidPolicy::CaptureReplayUnsupported))
        .map(|(owner, member, _)| format!("{owner}.{member}"))
        .collect();

    assert_eq!(
        matched,
        vec![
            "VkOpaqueCaptureDescriptorDataCreateInfoEXT.opaqueCaptureDescriptorData",
            "VkRayTracingShaderGroupCreateInfoKHR.pShaderGroupCaptureReplayHandle",

            "vkGetAccelerationStructureOpaqueCaptureDescriptorDataEXT.pData",
            "vkGetBufferOpaqueCaptureDescriptorDataEXT.pData",
            "vkGetImageOpaqueCaptureDescriptorDataEXT.pData",
            "vkGetImageViewOpaqueCaptureDescriptorDataEXT.pData",
            "vkGetSamplerOpaqueCaptureDescriptorDataEXT.pData",
            "vkGetTensorOpaqueCaptureDescriptorDataARM.pData",
            "vkGetTensorViewOpaqueCaptureDescriptorDataARM.pData",
        ],
        "the capture-replay class changed; see the doc comment on this test"
    );
}

/// A `void*` with a policy has a *reason* recorded beside it, and the reasons are
/// what this file is really guarding.
///
/// The weak version of this test would assert that every `void*` in the registry
/// has a policy — which is false on purpose: §7.4's build failure is how an
/// unread one is found, and `Profile::excluded_extensions` is where living
/// without it is written down. What can be asserted is the other direction: no
/// entry names something the registry does not have, so the table cannot quietly
/// keep answering a question that has gone away.
#[test]
fn no_policy_entry_names_a_member_the_registry_does_not_have() {
    let Some(m) = model() else {
        eprintln!("VK_XML unset — skipping (run inside `nix develop`)");
        return;
    };
    // The entries that are documentation rather than rules — nothing consults
    // them, and each is here because reading it beside its neighbours is what
    // makes a class visible. Named individually so a *third* cannot join them
    // by accident, which is the only way this exemption stays honest. The owner
    // must still exist.
    const DOCUMENTARY: &[(&str, &str, &str)] = &[
        // An `ANativeWindow*`, not a `void*`: it refuses one step earlier as an
        // opaque platform type and `void_policy` is never asked. Kept beside
        // `VkViSurfaceCreateInfoNN::window` and the two `pView`s, which the
        // registry does spell `void*`, because the four are one class —
        // docs/design.md §6.1's WSI handles.
        ("VkAndroidSurfaceCreateInfoKHR", "window", "an ANativeWindow*, refused as a platform type"),
        // The `2` forms carry `pData` inside `VkPushDescriptorSetWithTemplateInfo`
        // and so have no parameter of that name; `ICD_EXPANDS_ARENA_POINTER`
        // says the same of its own `2` entries and for the same reason.
        (
            "vkCmdPushDescriptorSetWithTemplate2",
            "pData",
            "the member is in VkPushDescriptorSetWithTemplateInfo, not in the command",
        ),
    ];

    let all = every_void_star(&m);
    for (owner, member, _) in exceptions::VOID_POLICIES {
        if let Some((_, _, why)) = DOCUMENTARY.iter().find(|(o, f, _)| o == owner && f == member) {
            assert!(
                m.types.contains_key(*owner)
                    || m.commands.contains_key(*owner)
                    || m.command_aliases.contains_key(*owner),
                "VOID_POLICIES names {owner} ({why}), which this registry does not define at all"
            );
            continue;
        }
        // Both halves of the name are resolved through the registry's own
        // aliasing, because `vkMapMemory2KHR` and `VkPushDescriptorSetWithTemplateInfoKHR`
        // are entries about the promoted names.
        let owner = if m.commands.contains_key(*owner) || m.command_aliases.contains_key(*owner) {
            m.canonical_command(owner)
        } else {
            m.canonical_type(owner)
        };
        assert!(
            all.iter().any(|(o, f, _)| *o == owner && f == member),
            "VOID_POLICIES names {owner}.{member}, which is not a `void*` without a `len=` \
             in this registry — so nothing consults the entry and its reason is no longer \
             attached to anything"
        );
    }
}
