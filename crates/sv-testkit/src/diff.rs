//! Comparing our plan against Venus's generated C.
//!
//! The comparison is deliberately about *shape*, not about our names or theirs:
//! for each struct both generators emit, walk the two member sequences in
//! parallel and require them to agree on which members exist, in what order, and
//! what kind of thing each one is.

use crate::venus::{VenusKind, VenusModel, VenusOp};
use sv_codegen::plan::{Plan, Scalar, Shape, StructPlan};

/// One disagreement between the two generators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub struct_name: String,
    pub detail: String,
}

/// What a comparison run found.
#[derive(Debug, Default)]
pub struct DiffReport {
    /// Structs both generators emit and agree about.
    pub agreed: Vec<String>,
    /// Structs both emit and disagree about. Any entry here is a bug in one of
    /// the two generators and must be explained before it is dismissed.
    pub divergences: Vec<Divergence>,
    /// Structs we emit that Venus does not, which is expected: Venus's coverage
    /// is its own curated subset and does not match ours.
    pub only_ours: Vec<String>,
    /// Structs Venus emits that we do not — likewise expected.
    pub only_venus: Vec<String>,
    /// Members skipped because the comparison cannot express them. Kept visible
    /// so that "agreed" never quietly includes "not looked at".
    pub not_compared: Vec<Divergence>,
}

/// The one structural fact per member that both generators must agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Scalar,
    Optional,
    Array,
    FixedArray,
    Blob,
    CString,
    CStringArray,
}

fn ours(shape: &Shape) -> Option<Kind> {
    Some(match shape {
        Shape::Scalar(_) => Kind::Scalar,
        Shape::Optional(_) => Kind::Optional,
        Shape::Array { .. } => Kind::Array,
        Shape::FixedArray { .. } => Kind::FixedArray,
        Shape::Blob { .. } => Kind::Blob,
        Shape::CString => Kind::CString,
        Shape::CStringArray { .. } => Kind::CStringArray,
        Shape::PNext => return None,
    })
}

fn theirs(kind: &VenusKind) -> Kind {
    match kind {
        VenusKind::Scalar(_) => Kind::Scalar,
        VenusKind::Optional(_) => Kind::Optional,
        VenusKind::Array(_) => Kind::Array,
        VenusKind::FixedArray(_) => Kind::FixedArray,
        VenusKind::Blob => Kind::Blob,
        VenusKind::CString => Kind::CString,
        VenusKind::CStringArray => Kind::CStringArray,
    }
}

/// The named type each side says a member holds, where both name one.
fn type_names(shape: &Shape, venus: &VenusKind) -> Option<(String, String)> {
    let theirs = match venus {
        VenusKind::Scalar(t) | VenusKind::Optional(t) | VenusKind::Array(t)
        | VenusKind::FixedArray(t) => t.clone(),
        _ => return None,
    };
    if theirs.is_empty() {
        return None;
    }
    let ours = leaf_type(shape)?;
    Some((ours, theirs))
}

fn leaf_type(shape: &Shape) -> Option<String> {
    match shape {
        Shape::Scalar(s) => Some(scalar_name(s)),
        Shape::Optional(inner) | Shape::Array { elem: inner, .. }
        | Shape::FixedArray { elem: inner, .. } => leaf_type(inner),
        _ => None,
    }
}

fn scalar_name(s: &Scalar) -> String {
    match s {
        Scalar::U8 => "uint8_t".into(),
        Scalar::I8 => "int8_t".into(),
        Scalar::U16 => "uint16_t".into(),
        Scalar::I16 => "int16_t".into(),
        Scalar::U32 => "uint32_t".into(),
        Scalar::I32 => "int32_t".into(),
        Scalar::U64 => "uint64_t".into(),
        Scalar::I64 => "int64_t".into(),
        Scalar::F32 => "float".into(),
        Scalar::F64 => "double".into(),
        Scalar::Size => "size_t".into(),
        Scalar::Bool32 => "VkBool32".into(),
        Scalar::Enum(n)
        | Scalar::Bitmask(n)
        | Scalar::Bitmask64(n)
        | Scalar::Handle(n)
        | Scalar::Struct(n)
        | Scalar::Union(n) => n.clone(),
        Scalar::OpaqueU64(_) => "void*".into(),
    }
}

/// Whether two type names denote the same wire encoding.
///
/// Venus resolves some spellings and not others: it writes `vn_sizeof_VkFlags`
/// for every 32-bit bitmask regardless of which one, and `vn_sizeof_VkDeviceSize`
/// rather than `uint64_t`. Neither is a disagreement about bytes, so the
/// comparison is over equivalence classes rather than strings.
fn same_type(ours: &str, theirs: &str, plan: &Plan) -> bool {
    if ours == theirs {
        return true;
    }
    // `VkFlags` is Venus's spelling of every 32-bit bitmask; `VkFlags64` of
    // every 64-bit one.
    if theirs == "VkFlags" && plan.bitmasks.iter().any(|b| b.name == ours && b.bit_width == 32) {
        return true;
    }
    if theirs == "VkFlags64" && plan.bitmasks.iter().any(|b| b.name == ours && b.bit_width == 64) {
        return true;
    }
    // A `*FlagBits` member: we keep the enum type, Venus widens to the flags
    // typedef. Same four bytes.
    if theirs == "VkFlags" && ours.contains("FlagBits") {
        return true;
    }
    // A `char[N]` member. We store it as `[u8; N]` because a Rust `char` is a
    // Unicode scalar value and a Vulkan `char` is a byte; the wire is the same
    // N bytes either way.
    if ours == "uint8_t" && theirs == "char" {
        return true;
    }
    // Base types Venus leaves as their Vulkan name.
    matches!(
        (ours, theirs),
        ("uint64_t", "VkDeviceSize")
            | ("uint64_t", "VkDeviceAddress")
            | ("uint32_t", "VkSampleMask")
            | ("uint32_t", "VkFlags")
            | ("uint64_t", "VkFlags64")
            | ("uint32_t", "VkBool32")
            | ("VkBool32", "uint32_t")
    )
}

/// Compare one struct.
pub fn compare_struct(plan: &Plan, s: &StructPlan, venus: &[VenusOp], report: &mut DiffReport) {
    // `sType` is not a field of ours and Venus skips it in `_self` too, so both
    // sequences start at the same place. `pNext` is ours-only in the field list
    // because our chain is a value; Venus handles it in the wrapper.
    let mine: Vec<&sv_codegen::plan::FieldPlan> =
        s.fields.iter().filter(|f| !matches!(f.shape, Shape::PNext)).collect();

    if mine.len() != venus.len() {
        report.divergences.push(Divergence {
            struct_name: s.name.clone(),
            detail: format!(
                "member count differs: ours {} [{}], Venus {} [{}]",
                mine.len(),
                mine.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", "),
                venus.len(),
                venus.iter().map(|o| o.member.as_str()).collect::<Vec<_>>().join(", "),
            ),
        });
        return;
    }

    let mut ok = true;
    for (f, op) in mine.iter().zip(venus.iter()) {
        if f.name != op.member {
            report.divergences.push(Divergence {
                struct_name: s.name.clone(),
                detail: format!("member order differs: ours `{}`, Venus `{}`", f.name, op.member),
            });
            ok = false;
            continue;
        }
        let Some(k) = ours(&f.shape) else {
            report.not_compared.push(Divergence {
                struct_name: s.name.clone(),
                detail: format!("`{}` is a pNext chain, which the formats differ on by design", f.name),
            });
            continue;
        };
        let t = theirs(&op.kind);
        // Venus writes a length prefix for a fixed-extent C array too, so the
        // two spellings of "array" are the same bytes when the count matches;
        // what must not differ is array-vs-single-value.
        let equivalent = k == t
            || matches!((k, t), (Kind::Array, Kind::FixedArray) | (Kind::FixedArray, Kind::Array))
            || matches!((k, t), (Kind::Blob, Kind::Array) | (Kind::Array, Kind::Blob))
            || matches!((k, t), (Kind::CString, Kind::Blob) | (Kind::Blob, Kind::CString));
        if !equivalent {
            report.divergences.push(Divergence {
                struct_name: s.name.clone(),
                detail: format!("`{}`: ours is {k:?}, Venus is {t:?}", f.name),
            });
            ok = false;
            continue;
        }
        if let Some((a, b)) = type_names(&f.shape, &op.kind)
            && !same_type(&a, &b, plan) {
                report.divergences.push(Divergence {
                    struct_name: s.name.clone(),
                    detail: format!("`{}`: ours is `{a}`, Venus is `{b}`", f.name),
                });
                ok = false;
            }
    }
    if ok {
        report.agreed.push(s.name.clone());
    }
}

/// Compare every struct both generators emit.
pub fn compare(plan: &Plan, venus: &VenusModel) -> DiffReport {
    let mut report = DiffReport::default();
    for s in &plan.structs {
        if s.is_union {
            // Venus's union encoding is a hardcoded tag; ours is a Rust enum
            // discriminant. The two are the same four bytes only when the tag
            // choice agrees, which is not something this comparison can see.
            continue;
        }
        match venus.structs.get(&s.name) {
            Some(ops) => compare_struct(plan, s, ops, &mut report),
            None => report.only_ours.push(s.name.clone()),
        }
    }
    for name in venus.structs.keys() {
        if plan.struct_plan(name).is_none() {
            report.only_venus.push(name.clone());
        }
    }
    report
}
