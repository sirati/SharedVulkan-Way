//! Reading Venus's generated C, so our generator can be checked against
//! somebody else's.
//!
//! # Why this exists
//!
//! `security-model.md` R8 is the sharpest thing in that document:
//!
//! > Wrong `len=`/`optional=` metadata, or a generator bug reading it, produces
//! > marshalling that is wrong in the same way on both sides and therefore
//! > *looks* consistent while corrupting memory.
//!
//! Every round-trip test in `sv-proto` is symmetric and therefore proves nothing
//! about that. What proves something is comparing against an *independent*
//! implementation of the same wire format, which docs/design.md §6.2 and §7.3 name:
//! Venus's MIT-licensed generated C.
//!
//! # What is compared, and what is not
//!
//! **Not** byte-for-byte, and the reason is a decision the design had already
//! taken before this test was written. design.md D6 replaces Venus's per-parent
//! `pNext` switch with gfxstream's length-prefixed links, and §6.2 deviation 1
//! adds a `u32` length to every command header. Nearly every Vulkan struct has a
//! `pNext`, so those two decisions between them change the bytes of almost
//! everything. §6.2's claim that "deviating further would mean losing the
//! ability to differentially test our encoder against Venus's generated C" is
//! therefore slightly optimistic about the deviations it had already made — see
//! the report accompanying this work.
//!
//! What *is* compared is the **structure of each struct's `_self` body**: the
//! ordered sequence of members, and for each one whether it is a scalar of a
//! given width, an array, a pointer to one value, a blob or a string. That is
//! precisely the information the generator derives from `len=`, `optional=` and
//! the C declarator — the three things R8 is about — and comparing it catches
//! the whole class:
//!
//! - a member one generator emits and the other does not,
//! - a member ordering difference,
//! - an array read as a single value or the reverse (a wrong `len=`),
//! - a scalar encoded at the wrong width.
//!
//! It is stronger than a byte comparison in one respect and weaker in another.
//! Stronger: a byte comparison over one sample value can pass while the `len=`
//! handling is wrong for every other value, whereas a structural comparison
//! holds for all values. Weaker: it does not check the primitive encodings
//! themselves, which are checked instead against the quoted C in
//! `reference/venus/NOTES.md` by the unit tests in `sv-wire`.
//!
//! # Provenance
//!
//! The headers under `reference/venus/` are Mesa's, MIT-licensed, fetched from
//! tag `mesa-26.2.0` and from `virglrenderer-1.3.0`. See `THIRD-PARTY.md`. They
//! are read as data by this test crate and no Venus code is compiled or
//! vendored into the product.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One operation in a Venus `_self` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenusOp {
    pub member: String,
    pub kind: VenusKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VenusKind {
    /// `vn_sizeof_<ty>(&val->m)` — one value of a named type.
    Scalar(String),
    /// `vn_sizeof_simple_pointer(val->m)` then the pointee: a pointer to one
    /// value.
    Optional(String),
    /// A count, then `vn_sizeof_<ty>_array(...)` or a per-element loop.
    Array(String),
    /// `vn_sizeof_<ty>_array(val->m, <constant>)` with no preceding
    /// `array_size(val->…)`: a fixed-extent C array member.
    FixedArray(String),
    /// A sized `void*`.
    Blob,
    /// A NUL-terminated string.
    CString,
    /// An array of NUL-terminated strings.
    CStringArray,
}

/// Every `_self` body found in the reference headers, keyed by struct name.
#[derive(Debug, Default)]
pub struct VenusModel {
    pub structs: BTreeMap<String, Vec<VenusOp>>,
    /// Structs Venus refuses to serialise at all, which it marks by emitting a
    /// body containing `assert(false)`.
    pub refused: Vec<String>,
}

/// Where the reference headers live, relative to the workspace root.
pub fn reference_dir() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/sv-testkit`.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../reference/venus")
}

impl VenusModel {
    /// Parse every `vn_sizeof_*_self` body under `dir`.
    pub fn load(dir: &Path) -> std::io::Result<VenusModel> {
        let mut m = VenusModel::default();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("vn_protocol_driver") && n.ends_with(".h"))
                    .unwrap_or(false)
            })
            .collect();
        entries.sort();
        for path in entries {
            let text = std::fs::read_to_string(&path)?;
            m.parse_file(&text);
        }
        Ok(m)
    }

    fn parse_file(&mut self, text: &str) {
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let Some(name) = self_body_name(lines[i]) else {
                i += 1;
                continue;
            };
            // Skip to the opening brace, then collect until the matching close.
            let mut body = Vec::new();
            let mut j = i + 1;
            while j < lines.len() && !lines[j].starts_with('{') {
                j += 1;
            }
            j += 1;
            while j < lines.len() && !lines[j].starts_with('}') {
                body.push(lines[j]);
                j += 1;
            }
            if body.iter().any(|l| l.contains("assert(false)")) {
                self.refused.push(name);
            } else {
                self.structs.insert(name, parse_body(&body));
            }
            i = j;
        }
    }
}

/// `vn_sizeof_VkBufferCreateInfo_self(const VkBufferCreateInfo *val)` → the name.
fn self_body_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("vn_sizeof_")?;
    let (name, tail) = rest.split_once("_self(")?;
    if !tail.starts_with("const ") {
        return None;
    }
    // `_self_partial` bodies are a second wire shape and are not compared here;
    // ours is derived from the same validity rules but the comparison would need
    // its own mapping.
    if name.ends_with("_partial") {
        return None;
    }
    Some(name.to_string())
}

fn parse_body(lines: &[&str]) -> Vec<VenusOp> {
    let mut ops: Vec<VenusOp> = Vec::new();
    // `array_size` marks the start of an array; the operation that follows names
    // the element type and the member.
    let mut pending_array = false;
    let mut pending_pointer: Option<String> = None;

    for raw in lines {
        let line = raw.trim();
        if line.starts_with("/*") || line.is_empty() || line.starts_with("return") {
            continue;
        }

        if line.contains("vn_sizeof_array_size(") {
            // `vn_sizeof_array_size(0)` is the NULL branch of an array that was
            // already opened; only the first one matters.
            if !line.contains("vn_sizeof_array_size(0)") {
                pending_array = true;
            }
            continue;
        }

        if let Some(member) = capture(line, "vn_sizeof_simple_pointer(val->", ")") {
            pending_pointer = Some(member);
            continue;
        }

        // `size += vn_sizeof_<ty>_array(val-><member>, <count>);`
        if let Some((ty, member)) = capture_array(line) {
            let kind = if pending_array {
                pending_array = false;
                VenusKind::Array(ty)
            } else {
                VenusKind::FixedArray(ty)
            };
            push(&mut ops, member, kind);
            continue;
        }

        // A per-element loop: `size += vn_sizeof_<ty>(&val-><member>[i]);`
        if let Some((ty, member)) = capture_element(line) {
            pending_array = false;
            push(&mut ops, member, VenusKind::Array(ty));
            continue;
        }

        if let Some(member) = capture(line, "vn_sizeof_blob_array(val->", ",") {
            pending_array = false;
            push(&mut ops, member, VenusKind::Blob);
            continue;
        }

        // `size += vn_sizeof_<ty>(&val-><member>);`
        if let Some((ty, member)) = capture_scalar(line) {
            match pending_pointer.take() {
                Some(p) if p == member => push(&mut ops, member, VenusKind::Optional(ty)),
                other => {
                    pending_pointer = other;
                    push(&mut ops, member, VenusKind::Scalar(ty));
                }
            }
            continue;
        }

        // `size += vn_sizeof_<ty>(val-><member>);` — the pointee of a simple
        // pointer is passed by value, not by address.
        if let Some((ty, member)) = capture_scalar_by_value(line) {
            match pending_pointer.take() {
                Some(p) if p == member => push(&mut ops, member, VenusKind::Optional(ty)),
                other => {
                    pending_pointer = other;
                    push(&mut ops, member, VenusKind::Scalar(ty));
                }
            }
            continue;
        }

        // A string: `strlen(val-><member>) + 1` appears in the length line. An
        // *array* of strings indexes it: `strlen(val->ppEnabledLayerNames[i])`,
        // which is how `VkInstanceCreateInfo` spells its two extension lists.
        if let Some(raw_member) = capture_until_close(line, "strlen(val->") {
            pending_array = false;
            let (member, indexed) = match raw_member.strip_suffix("[i]") {
                Some(m) => (m.to_string(), true),
                None => (raw_member, false),
            };
            let kind = if indexed { VenusKind::CStringArray } else { VenusKind::CString };
            push(&mut ops, member, kind);
            continue;
        }
    }

    // A pointer whose pointee was written by a helper we did not match still
    // counts as a pointer.
    if let Some(p) = pending_pointer {
        push(&mut ops, p, VenusKind::Optional(String::new()));
    }
    ops
}

fn push(ops: &mut Vec<VenusOp>, member: String, kind: VenusKind) {
    // Venus emits both branches of an `if (val->p)`, and a string's length and
    // its bytes are two lines about one member. Keep the first mention.
    if ops.last().map(|o| o.member == member).unwrap_or(false) {
        return;
    }
    ops.push(VenusOp { member, kind });
}

/// Everything between `prefix` and the next `)`, allowing an `[i]` subscript.
fn capture_until_close(line: &str, prefix: &str) -> Option<String> {
    let at = line.find(prefix)? + prefix.len();
    let rest = &line[at..];
    let end = rest.find(')')?;
    let name = &rest[..end];
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '[' || c == ']')
    {
        return None;
    }
    Some(name.to_string())
}

fn capture(line: &str, prefix: &str, terminator: &str) -> Option<String> {
    let at = line.find(prefix)? + prefix.len();
    let rest = &line[at..];
    let end = rest.find(terminator)?;
    let name = &rest[..end];
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(name.to_string())
}

/// `vn_sizeof_<ty>(&val-><member>)`
fn capture_scalar(line: &str) -> Option<(String, String)> {
    let at = line.find("vn_sizeof_")? + "vn_sizeof_".len();
    let rest = &line[at..];
    let (ty, tail) = rest.split_once("(&val->")?;
    if ty.ends_with("_array") || ty.is_empty() {
        return None;
    }
    let member = tail.split(')').next()?;
    if member.contains('[') || !member.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((ty.to_string(), member.to_string()))
}

/// `vn_sizeof_<ty>(val-><member>)` — used for the pointee of a simple pointer.
fn capture_scalar_by_value(line: &str) -> Option<(String, String)> {
    let at = line.find("vn_sizeof_")? + "vn_sizeof_".len();
    let rest = &line[at..];
    let (ty, tail) = rest.split_once("(val->")?;
    if ty.ends_with("_array") || ty == "simple_pointer" || ty == "array_size" || ty.is_empty() {
        return None;
    }
    let member = tail.split(')').next()?;
    if member.contains('[') || !member.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((ty.to_string(), member.to_string()))
}

/// `vn_sizeof_<ty>_array(val-><member>, …)`
fn capture_array(line: &str) -> Option<(String, String)> {
    let at = line.find("vn_sizeof_")? + "vn_sizeof_".len();
    let rest = &line[at..];
    let (ty, tail) = rest.split_once("_array(val->")?;
    if ty == "blob" || ty.is_empty() {
        return None;
    }
    let member = tail.split(',').next()?.split(')').next()?;
    if !member.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((ty.to_string(), member.to_string()))
}

/// `size += vn_sizeof_<ty>(&val-><member>[i]);`
fn capture_element(line: &str) -> Option<(String, String)> {
    if !line.contains("[i]") {
        return None;
    }
    let at = line.find("vn_sizeof_")? + "vn_sizeof_".len();
    let rest = &line[at..];
    let (ty, tail) = rest.split_once("(&val->")?;
    let member = tail.split('[').next()?;
    if !member.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((ty.to_string(), member.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_buffer_create_info_body() {
        let body = [
            "    size_t size = 0;",
            "    /* skip val->{sType,pNext} */",
            "    size += vn_sizeof_VkFlags(&val->flags);",
            "    size += vn_sizeof_VkDeviceSize(&val->size);",
            "    size += vn_sizeof_VkFlags(&val->usage);",
            "    size += vn_sizeof_VkSharingMode(&val->sharingMode);",
            "    size += vn_sizeof_uint32_t(&val->queueFamilyIndexCount);",
            "    if (val->sharingMode == VK_SHARING_MODE_CONCURRENT) {",
            "        size += vn_sizeof_array_size(val->queueFamilyIndexCount);",
            "        size += vn_sizeof_uint32_t_array(val->pQueueFamilyIndices, \
             val->queueFamilyIndexCount);",
            "    } else {",
            "        size += vn_sizeof_array_size(0);",
            "    }",
            "    return size;",
        ];
        let ops = parse_body(&body);
        let names: Vec<&str> = ops.iter().map(|o| o.member.as_str()).collect();
        assert_eq!(
            names,
            ["flags", "size", "usage", "sharingMode", "queueFamilyIndexCount", "pQueueFamilyIndices"]
        );
        assert_eq!(ops[0].kind, VenusKind::Scalar("VkFlags".into()));
        assert_eq!(ops[5].kind, VenusKind::Array("uint32_t".into()));
    }

    #[test]
    fn parses_a_per_element_array_loop() {
        let body = [
            "    size_t size = 0;",
            "    if (val->pImageInfo) {",
            "        size += vn_sizeof_array_size(val->descriptorCount);",
            "        for (uint32_t i = 0; i < val->descriptorCount; i++)",
            "            size += vn_sizeof_VkDescriptorImageInfo(&val->pImageInfo[i]);",
            "    } else {",
            "        size += vn_sizeof_array_size(0);",
            "    }",
            "    return size;",
        ];
        let ops = parse_body(&body);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].member, "pImageInfo");
        assert_eq!(ops[0].kind, VenusKind::Array("VkDescriptorImageInfo".into()));
    }

    #[test]
    fn recognises_a_simple_pointer() {
        let body = [
            "    size_t size = 0;",
            "    size += vn_sizeof_simple_pointer(val->pTessellationState);",
            "    if (val->pTessellationState)",
            "        size += vn_sizeof_VkPipelineTessellationStateCreateInfo(\
             val->pTessellationState);",
            "    return size;",
        ];
        let ops = parse_body(&body);
        assert_eq!(ops.len(), 1);
        assert_eq!(
            ops[0].kind,
            VenusKind::Optional("VkPipelineTessellationStateCreateInfo".into())
        );
    }
}
