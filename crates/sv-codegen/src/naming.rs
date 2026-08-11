//! Registry names → Rust identifiers.
//!
//! Kept in one place and kept boring. Every generated name is a pure function of
//! a registry name, because the protocol hash and the byte-identical-output
//! requirement both depend on the generator having no state that could make the
//! same input produce two outputs.

use proc_macro2::{Ident, Span};

/// `pQueueFamilyIndices` → `p_queue_family_indices`, `sType` → `s_type`.
///
/// The Vulkan type name is kept verbatim (`VkBufferCreateInfo` is already
/// idiomatic Rust for a type) but field names are converted, because
/// `#![allow(non_snake_case)]` over 100,000 generated lines would also silence
/// it in the small amount of hand-written glue that shares the crate.
pub fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower = i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_ascii_lowercase();
            let prev_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
            if i > 0 && (prev_lower || (prev_upper && next_lower)) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    // A leading `p`/`pp` Hungarian prefix survives as `p_`/`pp_`, which is
    // deliberate: it is the registry's own marker for "this is a pointer", and
    // keeping it means a generated field name still greps against the spec.
    escape_keyword(&out)
}

/// Rust keywords that occur as Vulkan member names (`type`, `ref`, `box`).
fn escape_keyword(s: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "as", "box", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
        "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "priv", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true",
        "type", "unsafe", "use", "where", "while", "abstract", "become", "final", "macro",
        "override", "typeof", "unsized", "virtual", "yield", "async", "await", "try", "gen",
    ];
    if KEYWORDS.contains(&s) { format!("{s}_") } else { s.to_string() }
}

/// A `proc_macro2::Ident` from a name known to be a valid identifier.
///
/// Panics rather than returning an error: every caller has already validated
/// the name against the registry, so a failure here is a generator bug and
/// should stop the build, not produce a file with a hole in it.
pub fn ident(name: &str) -> Ident {
    Ident::new(name, Span::call_site())
}

/// The Rust type name for a registry type.
pub fn type_ident(name: &str) -> Ident {
    ident(name)
}

/// The variant name for a struct inside the global `pNext` link enum.
pub fn pnext_variant(struct_name: &str) -> Ident {
    ident(struct_name)
}

/// `VkClearColorValue` member `float32` → variant `Float32`.
pub fn union_variant(member: &str) -> Ident {
    let mut out = String::new();
    let mut upper = true;
    for c in member.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    ident(&escape_keyword(&out))
}

/// The opcode enum variant for a command: `vkCreateBuffer` → `VkCreateBuffer`.
pub fn opcode_variant(command: &str) -> Ident {
    let mut cs = command.chars();
    let first = cs.next().unwrap_or('x').to_ascii_uppercase();
    ident(&format!("{first}{}", cs.as_str()))
}

/// `vkCreateBuffer` → `create_buffer`, for the server trait's method names.
pub fn command_method(command: &str) -> Ident {
    ident(&snake_case(command.strip_prefix("vk").unwrap_or(command)))
}

/// `vkCreateBuffer` → `size_vk_create_buffer` and friends.
pub fn fn_ident(prefix: &str, name: &str) -> Ident {
    ident(&format!("{prefix}_{}", snake_case(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_handles_the_shapes_the_registry_uses() {
        assert_eq!(snake_case("sType"), "s_type");
        assert_eq!(snake_case("pNext"), "p_next");
        assert_eq!(snake_case("pQueueFamilyIndices"), "p_queue_family_indices");
        assert_eq!(snake_case("queueFamilyIndexCount"), "queue_family_index_count");
        assert_eq!(snake_case("size"), "size");
        assert_eq!(snake_case("ppEnabledExtensionNames"), "pp_enabled_extension_names");
        // Acronym runs: `maxDescriptorSetUniformBuffersDynamic` has none, but
        // `pfnUserCallback` and `apiVersion` do the two interesting things.
        assert_eq!(snake_case("apiVersion"), "api_version");
        assert_eq!(snake_case("pfnUserCallback"), "pfn_user_callback");
        assert_eq!(snake_case("subgroupSize"), "subgroup_size");
        assert_eq!(snake_case("VRSTileSize"), "vrs_tile_size");
    }

    #[test]
    fn keywords_are_escaped() {
        assert_eq!(snake_case("type"), "type_");
        assert_eq!(snake_case("ref"), "ref_");
    }

    #[test]
    fn union_variants_are_camel() {
        assert_eq!(union_variant("float32").to_string(), "Float32");
        assert_eq!(union_variant("deviceAddress").to_string(), "DeviceAddress");
        assert_eq!(union_variant("hostAddress").to_string(), "HostAddress");
    }

    #[test]
    fn opcode_variants_capitalise_the_vk() {
        assert_eq!(opcode_variant("vkCreateBuffer").to_string(), "VkCreateBuffer");
    }

    #[test]
    fn command_methods_drop_the_vk() {
        assert_eq!(command_method("vkCreateBuffer").to_string(), "create_buffer");
        assert_eq!(command_method("vkCmdDraw").to_string(), "cmd_draw");
    }
}
