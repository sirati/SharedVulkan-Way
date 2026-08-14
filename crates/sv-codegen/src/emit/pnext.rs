//! Pass 7: the `pNext` chain walkers (docs/design.md §7.2, D6).
//!
//! # The format, and why it is not Venus's
//!
//! Venus encodes a chain as a per-parent `switch`: for each link it writes a
//! presence flag, the `sType`, the *rest of the chain recursively*, and then the
//! link's body. There is no length, so a reader that does not recognise an
//! `sType` cannot skip the link — it can only stop. That is the smaller
//! encoding, and it turns any version skew into a desynchronised stream.
//!
//! design.md D6 takes gfxstream's shape instead: each link is length-prefixed
//! and the chain ends with a zero length.
//!
//! ```text
//! link := u32 link_size   // bytes that follow, for this link only
//!         i32 sType
//!         <the link's own body, no sType, no nested chain>
//! chain := link* u32(0)
//! ```
//!
//! Flat rather than Venus's nested recursion, because the point of the length is
//! that an unknown link can be stepped over without understanding it, and a
//! nested encoding would put the rest of the chain *inside* the link you cannot
//! parse.
//!
//! This is the one place the design deliberately gives up byte-for-byte
//! comparability with Venus's generated C, and it is worth saying plainly: since
//! nearly every Vulkan struct has a `pNext`, this decision means the
//! differential test of docs/design.md §6.2 cannot be a whole-struct byte comparison.
//! What it compares instead is the `_self` body — the part that is Venus's
//! format verbatim — and the operation sequence. See `crates/sv-testkit`.
//!
//! # Asymmetric handling
//!
//! docs/design.md §6.1: the app side silently keeps an unknown link and continues;
//! the GPU side treats one as fatal. The policy travels on the decoder
//! ([`sv_wire::Decoder::lenient`]) rather than being a compile-time feature, so
//! one test binary can exercise both sides.

use crate::naming;
use crate::plan::Plan;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

pub fn allowed_fn_ident(parent: &str) -> proc_macro2::Ident {
    format_ident!("pnext_allowed_in_{}", naming::snake_case(parent))
}

pub fn emit(plan: &Plan) -> TokenStream {
    let link_enum = emit_link_enum(plan);
    let walkers = emit_walkers(plan);
    let allowed = plan.structs.iter().filter(|s| s.has_p_next).map(|s| {
        let id = allowed_fn_ident(&s.name);
        let values: Vec<i32> = s
            .extenders
            .iter()
            .filter_map(|e| plan.struct_plan(e).and_then(|p| p.s_type_value))
            .map(|v| v as i32)
            .collect();
        let doc = format!(
            "Whether `{}` may carry an `sType` in its `pNext` chain, per the \
             registry's `structextends` restricted to this build's profile.",
            s.name
        );
        if values.is_empty() {
            quote! {
                #[doc = #doc]
                pub fn #id(_s_type: i32) -> bool { false }
            }
        } else {
            quote! {
                #[doc = #doc]
                pub fn #id(s_type: i32) -> bool { matches!(s_type, #(#values)|*) }
            }
        }
    });

    quote! {
        #link_enum
        #walkers
        #(#allowed)*
    }
}

/// One global link enum rather than one per parent.
///
/// Venus generates a `_pnext` walker per parent struct, which is ~200 near
/// identical functions and, in Rust, would be ~200 near identical enums. A
/// single link type plus a generated per-parent membership predicate says the
/// same thing in a fraction of the code, and — because our links are
/// length-prefixed — the predicate is a *validity* check rather than the thing
/// that makes parsing possible.
fn emit_link_enum(plan: &Plan) -> TokenStream {
    let variants = plan.pnext_links.iter().map(|(name, _)| {
        let v = naming::pnext_variant(name);
        let t = naming::type_ident(name);
        let doc = format!("`{name}`");
        quote! {
            #[doc = #doc]
            #v(Box<#t>),
        }
    });
    let s_type_arms = plan.pnext_links.iter().map(|(name, value)| {
        let v = naming::pnext_variant(name);
        let val = *value as i32;
        quote!(PNextLink::#v(_) => #val,)
    });

    quote! {
        /// One link of a `pNext` chain.
        ///
        /// The payloads are boxed: the largest extending struct is several
        /// hundred bytes and an unboxed enum would be that size for every link.
        #[derive(Debug, Clone, PartialEq)]
        pub enum PNextLink {
            #(#variants)*
            /// A link this build does not know.
            ///
            /// Kept verbatim so the app side can carry it through unchanged
            /// (docs/design.md §6.1: "the app side silently drops unknown links and
            /// continues"), which also makes a decode/encode round trip exact
            /// even across a version skew. The GPU side never constructs one —
            /// its decoder refuses first.
            Unknown { s_type: i32, body: Vec<u8> },
        }

        impl PNextLink {
            pub fn s_type(&self) -> i32 {
                match self {
                    #(#s_type_arms)*
                    PNextLink::Unknown { s_type, .. } => *s_type,
                }
            }
        }
    }
}

fn emit_walkers(plan: &Plan) -> TokenStream {
    let size_arms = plan.pnext_links.iter().map(|(name, _)| {
        let v = naming::pnext_variant(name);
        let f = super::wire::size_self_fn(name);
        quote!(PNextLink::#v(inner) => #f(inner),)
    });
    let size_partial_arms = plan.pnext_links.iter().map(|(name, _)| {
        let v = naming::pnext_variant(name);
        let has_partial = plan.struct_plan(name).map(|s| s.needs_partial).unwrap_or(false);
        let f = if has_partial {
            format_ident!("{}_partial", super::wire::size_self_fn(name))
        } else {
            super::wire::size_self_fn(name)
        };
        quote!(PNextLink::#v(inner) => #f(inner),)
    });
    let encode_arms = plan.pnext_links.iter().map(|(name, _)| {
        let v = naming::pnext_variant(name);
        let f = super::wire::encode_self_fn(name);
        quote!(PNextLink::#v(inner) => #f(enc, inner)?,)
    });
    let encode_partial_arms = plan.pnext_links.iter().map(|(name, _)| {
        let v = naming::pnext_variant(name);
        let has_partial = plan.struct_plan(name).map(|s| s.needs_partial).unwrap_or(false);
        let f = if has_partial {
            format_ident!("{}_partial", super::wire::encode_self_fn(name))
        } else {
            super::wire::encode_self_fn(name)
        };
        quote!(PNextLink::#v(inner) => #f(enc, inner)?,)
    });
    let decode_arms = plan.pnext_links.iter().map(|(name, value)| {
        let v = naming::pnext_variant(name);
        let f = super::wire::decode_self_fn(name);
        let val = *value as i32;
        quote!(#val => PNextLink::#v(Box::new(#f(&mut body)?)),)
    });
    let decode_partial_arms = plan.pnext_links.iter().map(|(name, value)| {
        let v = naming::pnext_variant(name);
        let has_partial = plan.struct_plan(name).map(|s| s.needs_partial).unwrap_or(false);
        let f = if has_partial {
            format_ident!("{}_partial", super::wire::decode_self_fn(name))
        } else {
            super::wire::decode_self_fn(name)
        };
        let val = *value as i32;
        quote!(#val => PNextLink::#v(Box::new(#f(&mut body)?)),)
    });

    quote! {
        /// Bytes a chain occupies, terminator included.
        pub fn size_pnext(links: &[PNextLink]) -> usize {
            let mut n = 4usize; // the terminator
            for link in links {
                n += 4 /* link_size */ + 4 /* sType */ + size_pnext_body(link);
            }
            n
        }

        pub fn size_pnext_partial(links: &[PNextLink]) -> usize {
            let mut n = 4usize;
            for link in links {
                n += 4 + 4 + size_pnext_body_partial(link);
            }
            n
        }

        fn size_pnext_body(link: &PNextLink) -> usize {
            match link {
                #(#size_arms)*
                PNextLink::Unknown { body, .. } => body.len(),
            }
        }

        fn size_pnext_body_partial(link: &PNextLink) -> usize {
            match link {
                #(#size_partial_arms)*
                PNextLink::Unknown { body, .. } => body.len(),
            }
        }

        /// Write a chain.
        ///
        /// The length of each link is patched in after the body is written
        /// rather than taken from the size pass, so that the two cannot disagree
        /// about a single link even if they disagree about the total — which
        /// [`sv_wire::Encoder::finish`] would then catch.
        pub fn encode_pnext(
            enc: &mut sv_wire::Encoder<'_>,
            links: &[PNextLink],
        ) -> sv_wire::Result<()> {
            for link in links {
                let patch = enc.reserve_u32()?;
                let mark = enc.position();
                enc.i32(link.s_type())?;
                match link {
                    #(#encode_arms)*
                    PNextLink::Unknown { body, .. } => enc.packed_bytes(body)?,
                }
                let written = enc.since(mark) as u32;
                enc.patch_u32(patch, written);
            }
            enc.u32(sv_wire::PNEXT_TERMINATOR)
        }

        pub fn encode_pnext_partial(
            enc: &mut sv_wire::Encoder<'_>,
            links: &[PNextLink],
        ) -> sv_wire::Result<()> {
            for link in links {
                let patch = enc.reserve_u32()?;
                let mark = enc.position();
                enc.i32(link.s_type())?;
                match link {
                    #(#encode_partial_arms)*
                    PNextLink::Unknown { body, .. } => enc.packed_bytes(body)?,
                }
                let written = enc.since(mark) as u32;
                enc.patch_u32(patch, written);
            }
            enc.u32(sv_wire::PNEXT_TERMINATOR)
        }

        /// Read a chain, checking each link against what the parent permits.
        pub fn decode_pnext(
            dec: &mut sv_wire::Decoder<'_>,
            allowed: fn(i32) -> bool,
        ) -> sv_wire::Result<Vec<PNextLink>> {
            let mut out = Vec::new();
            loop {
                let link_size = dec.u32()?;
                if link_size == sv_wire::PNEXT_TERMINATOR {
                    return Ok(out);
                }
                if link_size < 4 {
                    return Err(sv_wire::WireError::PNextLengthMismatch {
                        declared: link_size,
                        consumed: 4,
                    });
                }
                let mut link = dec.sub(link_size as usize)?;
                let s_type = link.i32()?;
                let mut body = link;
                if !allowed(s_type) {
                    out.push(unknown_or_fatal(dec.unknown_pnext_policy(), s_type, &mut body)?);
                    continue;
                }
                let decoded = match s_type {
                    #(#decode_arms)*
                    _ => {
                        out.push(unknown_or_fatal(
                            dec.unknown_pnext_policy(), s_type, &mut body,
                        )?);
                        continue;
                    }
                };
                if body.remaining() != 0 {
                    return Err(sv_wire::WireError::PNextLengthMismatch {
                        declared: link_size,
                        consumed: link_size as usize - body.remaining(),
                    });
                }
                out.push(decoded);
            }
        }

        pub fn decode_pnext_partial(
            dec: &mut sv_wire::Decoder<'_>,
            allowed: fn(i32) -> bool,
        ) -> sv_wire::Result<Vec<PNextLink>> {
            let mut out = Vec::new();
            loop {
                let link_size = dec.u32()?;
                if link_size == sv_wire::PNEXT_TERMINATOR {
                    return Ok(out);
                }
                if link_size < 4 {
                    return Err(sv_wire::WireError::PNextLengthMismatch {
                        declared: link_size,
                        consumed: 4,
                    });
                }
                let mut link = dec.sub(link_size as usize)?;
                let s_type = link.i32()?;
                let mut body = link;
                if !allowed(s_type) {
                    out.push(unknown_or_fatal(dec.unknown_pnext_policy(), s_type, &mut body)?);
                    continue;
                }
                let decoded = match s_type {
                    #(#decode_partial_arms)*
                    _ => {
                        out.push(unknown_or_fatal(
                            dec.unknown_pnext_policy(), s_type, &mut body,
                        )?);
                        continue;
                    }
                };
                if body.remaining() != 0 {
                    return Err(sv_wire::WireError::PNextLengthMismatch {
                        declared: link_size,
                        consumed: link_size as usize - body.remaining(),
                    });
                }
                out.push(decoded);
            }
        }

        /// The asymmetry of docs/design.md §6.1, in one place.
        fn unknown_or_fatal(
            policy: sv_wire::UnknownPNextPolicy,
            s_type: i32,
            body: &mut sv_wire::Decoder<'_>,
        ) -> sv_wire::Result<PNextLink> {
            match policy {
                sv_wire::UnknownPNextPolicy::Skip => {
                    let n = body.remaining();
                    Ok(PNextLink::Unknown { s_type, body: body.packed_bytes(n)?.to_vec() })
                }
                sv_wire::UnknownPNextPolicy::Fatal => {
                    Err(sv_wire::WireError::UnknownPNext { s_type })
                }
            }
        }
    }
}
