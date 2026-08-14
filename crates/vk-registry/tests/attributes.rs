//! **Every attribute the registry puts on a member or a parameter is either
//! handled or listed as deliberately ignored — and a new one fails the build.**
//!
//! # Why this test exists
//!
//! Three times in this project a `vk.xml` attribute turned out to be
//! load-bearing and unread, and each was found by a crash rather than by the
//! compiler:
//!
//! - **`noautovalidity=`** (docs/design.md §0.16) — the registry saying "a
//!   sibling decides this member's validity". Unread, so
//!   `VkWriteDescriptorSet`'s unselected arms were dereferenced out of the
//!   application's stack. Cost: a GPU hang, found by CTS.
//! - **`selector=` / `selection=`** (§0.21) — which arm of a union is live.
//!   Unread, so the app side marshalled member [0] whatever the application
//!   meant. Cost: 52 commands held out of every profile behind a
//!   `compile_error!` nobody could safely remove.
//! - **`stride=`** (§0.24) — the spacing of an array that is not tightly
//!   packed. Unread, so a strided array was read at `size_of` spacing and the
//!   driver was handed a repacked array with the application's original stride.
//!   Cost: latent only because Zink passes `size_of`.
//!
//! Three is a pattern, and the pattern has a fixed point: the *fourth* one
//! should be found by a failing build. So this walks the registry text, collects
//! every attribute name that appears on a `<member>` or `<param>`, and compares
//! it against a list that says, for each, which of the two things is true —
//! *the generator reads it*, or *the generator ignores it and here is why*.
//!
//! It is deliberately a text scan rather than a walk over `vk_parse`'s typed
//! fields. `vk_parse` only surfaces attributes it models, so a genuinely new one
//! would be invisible in the typed view — which is precisely the case this test
//! is for.

use std::collections::BTreeSet;

/// Attributes the generator reads, with where.
const HANDLED: &[(&str, &str)] = &[
    ("len", "vk_registry::model::LenSpec; bounds every array"),
    ("altlen", "the C spelling of a latexmath len, preferred over it"),
    ("optional", "per-level, into Member::optional"),
    ("externsync", "the queue lock and the command-pool lock (§0.12, §0.23)"),
    ("selector", "which union arm is live, at the enclosing struct (§0.21)"),
    ("selection", "the discriminator values choosing a union arm (§0.21)"),
    ("values", "the fixed sType, written from a constant rather than carried"),
    ("noautovalidity", "exceptions::SIBLING_SELECTED and the len rules (§0.16)"),
    ("objecttype", "the sibling naming an untyped handle's VkObjectType"),
    ("stride", "the byte spacing of an array that is not tightly packed (§0.24)"),
    ("api", "vulkan vs vulkansc; the non-vulkan declarations are dropped"),
    ("deprecated", "recorded; the profile decides"),
];

/// Attributes the generator ignores **on purpose**, each with the reason.
///
/// An entry here is a decision, not an oversight, and that is the whole
/// difference this test is trying to preserve.
const IGNORED: &[(&str, &str)] = &[
    ("limittype", "a property's aggregation rule for physical-device selection; \
                   nothing this generator marshals depends on it"),
    ("featurelink", "documentation cross-reference"),
    ("comment", "prose"),
    // **The fourth, and this test found it on its first run** — which is the
    // whole argument for the test, so it is recorded here rather than quietly
    // added to `HANDLED`.
    //
    // One use in the entire registry: `vkGetPipelinePropertiesEXT`'s
    // `pPipelineProperties` is a `VkBaseOutStructure*` and `validstructs=` names
    // the one type it may actually be —
    // `VkPipelinePropertiesIdentifierEXT`. It is the same *kind* of statement as
    // `selector=`: the registry saying which concrete thing a
    // deliberately-vague declaration stands for.
    //
    // Ignored rather than read, and safely so, for a reason that is checkable:
    // `VK_EXT_pipeline_properties` is in no profile, so the command is not
    // carried and the parameter is never marshalled. **If it is ever added, this
    // attribute is what the lowering has to consult** — a `VkBaseOutStructure*`
    // marshalled as its declared type would carry a two-field header and drop
    // the identifier the caller asked for.
    (
        "validstructs",
        "the concrete type behind a VkBaseOutStructure*; one use, in \
         VK_EXT_pipeline_properties, which no profile carries. Read it before \
         carrying that extension",
    ),
];

#[test]
fn every_registry_attribute_is_handled_or_listed() {
    let path = std::env::var("VK_XML")
        .expect("VK_XML is unset — run inside `nix develop`, which pins it");
    let xml = std::fs::read_to_string(&path).expect("VK_XML names a readable file");

    // Attribute names inside `<member …>` and `<param …>` open tags. Crude on
    // purpose: a real XML walk would need the whole document model to answer a
    // question about twenty tag heads, and being crude here fails *safe* — a
    // name this misses is a name the generator also never saw.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for tag in ["<member", "<param"] {
        let mut at = 0usize;
        while let Some(i) = xml[at..].find(tag) {
            let start = at + i + tag.len();
            let Some(end) = xml[start..].find('>') else { break };
            let head = &xml[start..start + end];
            at = start + end;
            // `name="x"` pairs in the tag head.
            for (k, part) in head.split('=').enumerate() {
                if k == 0 {
                    continue;
                }
                // The attribute name is the last identifier before this `=`.
                let prev = head.split('=').nth(k - 1).unwrap_or("");
                let name: String = prev
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let _ = part;
                if !name.is_empty() {
                    seen.insert(name);
                }
            }
        }
    }

    assert!(
        seen.len() > 5,
        "the scan found only {seen:?}, which cannot be right for a registry this \
         size — the parser, not the registry, is what changed"
    );

    let known: BTreeSet<&str> =
        HANDLED.iter().chain(IGNORED).map(|(n, _)| *n).collect();
    let unknown: Vec<&String> = seen.iter().filter(|n| !known.contains(n.as_str())).collect();

    assert!(
        unknown.is_empty(),
        "the registry carries attribute(s) this generator has never been told \
         about: {unknown:?}\n\
         \n\
         Three times an unread attribute has been load-bearing — `noautovalidity` \
         cost a GPU hang, `selector`/`selection` held 52 commands out of every \
         profile, `stride` handed the driver an array with the wrong spacing — \
         and each was found by a crash. This is where the fourth is supposed to \
         be found instead.\n\
         \n\
         Decide which it is and say so in `HANDLED` or `IGNORED` in this file: \
         either the generator reads it, or it ignores it *and the entry gives \
         the reason*. Adding a name without a reason is how the first three got \
         here."
    );
}
