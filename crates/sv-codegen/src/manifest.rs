//! The protocol manifest and its hash.
//!
//! docs/design.md §4.3 and §6.2 deviation 2:
//!
//! > `protocol_hash` is the answer to R13. … The hash is computed by the
//! > generator over the full command and struct table it emitted — **not** over
//! > `vk.xml`, because gfxstream's own source warns that `optional=` semantics
//! > move between header releases, so the generator version matters too.
//!
//! So the manifest is a canonical text rendering of the *plan* — every command
//! with its opcode and parameter shapes, every struct with its member shapes,
//! every enum value that reaches the wire, and every exception the generator
//! consulted. Two builds agree on the wire format if and only if they agree on
//! this text.
//!
//! It is written out alongside the generated code, because a hash nobody can
//! diff is a hash nobody can debug: when two sides refuse each other, the useful
//! question is *which line* differs, and that needs the text.

use crate::plan::{
    BitmaskPlan, CommandPlan, EnumPlan, FieldPlan, HandlePlan, LenCheck, Plan, Scalar, Shape,
    StructPlan,
};
use std::fmt::Write;

/// Render the plan as canonical text.
pub fn render(plan: &Plan) -> String {
    let mut s = String::with_capacity(1 << 20);
    // Version of the *manifest format*. Bumping it changes every hash, which is
    // the right thing when the meaning of a line changes.
    let _ = writeln!(s, "manifest-version 1");
    let _ = writeln!(s, "profile {}", plan.profile);
    let _ = writeln!(s, "wire-granularity 4");
    let _ = writeln!(s, "command-header cmd_type:u32 cmd_flags:u32 cmd_size:u32");
    let _ = writeln!(s, "reply-header cmd_type:u32 reply_size:u32");
    let _ = writeln!(s, "pnext length-prefixed-flat terminator:u32(0)");

    for h in &plan.handles {
        let _ = writeln!(
            s,
            "handle {} dispatchable={} parent={}",
            h.name,
            h.dispatchable,
            h.parent.as_deref().unwrap_or("-")
        );
    }
    for e in &plan.enums {
        render_enum(&mut s, e);
    }
    for b in &plan.bitmasks {
        render_bitmask(&mut s, b);
    }
    for f in &plan.flag_words {
        render_flag_word(&mut s, f);
    }
    for st in &plan.structs {
        render_struct(&mut s, st);
    }
    for c in &plan.commands {
        render_command(&mut s, c);
    }
    for (name, s_type) in &plan.pnext_links {
        let _ = writeln!(s, "pnext-link {name} {s_type}");
    }
    for ((owner, field), policy) in &plan.exceptions.void_policies {
        let _ = writeln!(s, "exception void {owner}.{field} {policy:?}");
    }
    for name in plan.exceptions.unserialisable.keys() {
        let _ = writeln!(s, "exception unserialisable {name}");
    }
    for (alias, target) in &plan.type_aliases {
        let _ = writeln!(s, "type-alias {alias} -> {target}");
    }
    s
}

fn render_enum(s: &mut String, e: &EnumPlan) {
    let _ = writeln!(s, "enum {} width={}", e.name, e.bit_width);
    for (n, v) in &e.values {
        let _ = writeln!(s, "  {n} = {v}");
    }
}

fn render_bitmask(s: &mut String, b: &BitmaskPlan) {
    let _ = writeln!(s, "bitmask {} width={}", b.name, b.bit_width);
    for (n, v) in &b.values {
        let _ = writeln!(s, "  {n} = {v}");
    }
}

/// A flag word's bit layout is part of the protocol, so it is in the manifest.
///
/// The *wire* does not depend on the layout — the word is copied — but the two
/// sides' accessors do, and a build whose generator laid the bits out
/// differently would produce a struct the peer's driver reads wrongly with no
/// symptom at the transport. Putting the offsets in the manifest makes that a
/// handshake refusal instead (docs/design.md §4.3).
fn render_flag_word(s: &mut String, f: &crate::plan::FlagWordPlan) {
    let _ = writeln!(s, "flag-word {} width={}", f.name, f.bit_width);
    for b in &f.fields {
        let _ = writeln!(s, "  {} @{} :{}", b.name, b.offset, b.bits);
    }
}

fn render_struct(s: &mut String, st: &StructPlan) {
    let _ = writeln!(
        s,
        "{} {} stype={} returnedonly={} partial={}",
        if st.is_union { "union" } else { "struct" },
        st.name,
        st.s_type_value.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
        st.returned_only,
        st.needs_partial
    );
    for f in &st.fields {
        let _ = writeln!(s, "  {}", render_field(f));
    }
    for e in &st.extenders {
        let _ = writeln!(s, "  extends-with {e}");
    }
}

fn render_command(s: &mut String, c: &CommandPlan) {
    let _ = writeln!(
        s,
        "command {} opcode={} reply={} recorded={} result={}",
        c.name,
        c.opcode,
        c.generates_reply,
        c.is_recorded,
        c.return_type.as_deref().unwrap_or("void")
    );
    for a in &c.aliases {
        let _ = writeln!(s, "  alias {a}");
    }
    for f in &c.inputs {
        let _ = writeln!(s, "  in  {}", render_field(f));
    }
    for f in &c.outputs {
        let _ = writeln!(s, "  out {}", render_field(f));
    }
}

fn render_field(f: &FieldPlan) -> String {
    format!(
        "{}: {}{}{}",
        f.name,
        render_shape(&f.shape),
        if f.optional { " optional" } else { "" },
        if f.is_a_length { " is-a-length" } else { "" }
    )
}

/// The wire shape, spelled out.
///
/// Deliberately verbose: this is the text two machines are compared by, so
/// anything that changes the bytes must change this string, and nothing that
/// does not change the bytes should.
pub fn render_shape(shape: &Shape) -> String {
    match shape {
        Shape::Scalar(s) => render_scalar(s),
        Shape::FixedArray { elem, count, extent } => {
            format!("array[{count}={extent}] of {}", render_shape(elem))
        }
        Shape::Optional(inner) => format!("optional {}", render_shape(inner)),
        Shape::Array { elem, len } => {
            format!("array(count:u64) of {} len={}", render_shape(elem), render_len(len))
        }
        Shape::Blob { len } => format!("blob(count:u64) len={}", render_len(len)),
        Shape::CString => "cstring(count:u64)".into(),
        Shape::CStringArray { len } => format!("cstring-array(count:u64) len={}", render_len(len)),
        Shape::PNext => "pnext-chain".into(),
    }
}

fn render_len(len: &Option<LenCheck>) -> String {
    match len {
        None => "-".into(),
        Some(LenCheck::Sibling(s)) => s.clone(),
        Some(LenCheck::Path { root, field }) => format!("{root}->{field}"),
        Some(LenCheck::SiblingDiv { sibling, divisor }) => format!("{sibling}/{divisor}"),
        Some(LenCheck::Unchecked(e)) => format!("unchecked({e})"),
    }
}

fn render_scalar(s: &Scalar) -> String {
    match s {
        Scalar::U8 => "u8:4".into(),
        Scalar::I8 => "i8:4".into(),
        Scalar::U16 => "u16:4".into(),
        Scalar::I16 => "i16:4".into(),
        Scalar::U32 => "u32:4".into(),
        Scalar::I32 => "i32:4".into(),
        Scalar::U64 => "u64:8".into(),
        Scalar::I64 => "i64:8".into(),
        Scalar::F32 => "f32:4".into(),
        Scalar::F64 => "f64:8".into(),
        Scalar::Size => "size_t:8".into(),
        Scalar::Bool32 => "bool32:4".into(),
        Scalar::Enum(n) => format!("enum:4 {n}"),
        Scalar::Bitmask(n) => format!("flags:4 {n}"),
        Scalar::Bitmask64(n) => format!("flags64:8 {n}"),
        Scalar::Handle(n) => format!("handle:8 {n}"),
        Scalar::Struct(n) => format!("struct {n}"),
        Scalar::Union(n) => format!("union {n}"),
        Scalar::OpaqueU64(p) => format!("opaque:8 {p:?}"),
    }
}

/// The 64-bit hash carried in the handshake header.
///
/// FNV-1a over the manifest text. Not a cryptographic hash and it does not need
/// to be: R13 is about two builds of *our own* code silently disagreeing, not
/// about an adversary constructing a collision, and `security-model.md` is
/// explicit that this is not a security boundary. What it does need to be is
/// stable across compilers, architectures and Rust releases — which
/// `DefaultHasher` is explicitly not, and which twelve lines of FNV are.
pub fn hash(manifest: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in manifest.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Unused parameters kept so the signature reads the way the design describes.
#[allow(dead_code)]
fn _unused(_: &HandlePlan) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_a_pinned_constant_not_whatever_the_stdlib_does_today() {
        // FNV-1a of the empty string is the offset basis, and of "a" is one
        // round. Both are external facts, so a change to this function fails
        // here rather than silently changing every protocol hash.
        assert_eq!(hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash("a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn shape_rendering_distinguishes_the_things_that_change_bytes() {
        let u32 = Shape::Scalar(Scalar::U32);
        let u64 = Shape::Scalar(Scalar::U64);
        assert_ne!(render_shape(&u32), render_shape(&u64));

        let arr = Shape::Array { elem: Box::new(u32.clone()), len: None };
        let opt = Shape::Optional(Box::new(u32.clone()));
        assert_ne!(render_shape(&arr), render_shape(&opt));

        // …and does not distinguish things that do not. A length the registry
        // states differently but that yields the same count is still a
        // different check, so it *does* show up — which is the conservative
        // direction to be wrong in.
        assert_eq!(render_shape(&u32), render_shape(&Shape::Scalar(Scalar::U32)));
    }
}
