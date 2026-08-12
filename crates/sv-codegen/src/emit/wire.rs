//! Passes 1–3: `size`, `encode`, `decode` (design.md §7.2).
//!
//! The `(size, encode)` split is not collapsible — you cannot reserve a ring
//! span before you know how big the message is — so every struct gets both, and
//! they are generated from the same [`Shape`] fold so that they cannot drift.
//! [`sv_wire::Encoder::finish`] then checks at run time that they agreed, which
//! turns "the two passes disagree" from a silent desynchronisation into an
//! error at the first packet.
//!
//! Pass 4, `arena_bind`, is not emitted. gfxstream needs it because its decoder
//! produces C structs full of raw pointers that must be made to point at
//! pool-allocated storage with the packet's lifetime. Our decoder produces owned
//! Rust values — `Vec`, `Option<Box<_>>` — whose storage *is* their lifetime, so
//! the pass has nothing to do. It is recorded here rather than silently dropped
//! because design.md §7.2 lists it and a reader deserves to know why the file is
//! missing.

use crate::naming;
use crate::plan::{LenCheck, PartialValidity, Plan, Scalar, Shape, StructPlan, partial_validity};
use proc_macro2::{Literal, TokenStream};
use quote::{format_ident, quote};

pub fn emit(plan: &Plan) -> TokenStream {
    let bodies = plan.structs.iter().map(|s| emit_struct(plan, s));
    quote! {
        #(#bodies)*
    }
}

// ---------------------------------------------------------------------------
// naming
// ---------------------------------------------------------------------------

pub fn size_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("size_{}", naming::snake_case(name))
}
pub fn encode_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("encode_{}", naming::snake_case(name))
}
pub fn decode_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("decode_{}", naming::snake_case(name))
}
pub fn size_self_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("size_{}_self", naming::snake_case(name))
}
pub fn encode_self_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("encode_{}_self", naming::snake_case(name))
}
pub fn decode_self_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("decode_{}_self", naming::snake_case(name))
}
pub fn size_partial_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("size_{}_partial", naming::snake_case(name))
}
pub fn encode_partial_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("encode_{}_partial", naming::snake_case(name))
}
pub fn decode_partial_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("decode_{}_partial", naming::snake_case(name))
}

/// Whether the generated encoder should walk this struct with the partial rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Full,
    Partial,
}

// ---------------------------------------------------------------------------
// the Shape fold
// ---------------------------------------------------------------------------

/// Bytes this shape occupies, as an expression over `access`.
pub fn size_expr(shape: &Shape, access: &TokenStream, mode: Mode) -> TokenStream {
    match shape {
        Shape::Scalar(s) => scalar_size(s, access, mode),
        Shape::FixedArray { elem, count, .. } => {
            let n = *count as usize;
            let body = match packed_kind(elem) {
                Some(PackedKind::Bytes) => quote!(sv_wire::pad4(#n)),
                Some(PackedKind::U16) => quote!(sv_wire::pad4(#n * 2)),
                Some(PackedKind::Wide(w)) => quote!(#n * #w),
                None => {
                    let e = size_expr(elem, &quote!(item), mode);
                    quote!(#access.iter().map(|item| #e).sum::<usize>())
                }
            };
            quote!(8usize + #body)
        }
        Shape::Optional(inner) => {
            let e = size_expr(inner, &quote!(inner), mode);
            // The `let _` is not noise: for a fixed-size pointee the size
            // expression is a constant that never mentions the binding, and 96
            // dead-variable warnings in generated code are 96 warnings nobody
            // will ever read.
            quote!(8usize + match &#access { Some(inner) => { let _ = &inner; #e }, None => 0 })
        }
        Shape::Array { elem, .. } => {
            let body = match packed_kind(elem) {
                Some(PackedKind::Bytes) => quote!(sv_wire::pad4(#access.len())),
                Some(PackedKind::U16) => quote!(sv_wire::pad4(#access.len() * 2)),
                Some(PackedKind::Wide(w)) => quote!(#access.len() * #w),
                None => {
                    let e = size_expr(elem, &quote!(item), mode);
                    quote!(#access.iter().map(|item| #e).sum::<usize>())
                }
            };
            quote!(8usize + #body)
        }
        Shape::Blob { .. } => quote!(8usize + sv_wire::pad4(#access.len())),
        Shape::CString => quote!(8usize + sv_wire::pad4(#access.len() + 1)),
        Shape::CStringArray { .. } => quote!(
            8usize + #access.iter().map(|s| 8usize + sv_wire::pad4(s.len() + 1)).sum::<usize>()
        ),
        Shape::PNext => match mode {
            Mode::Full => quote!(size_pnext(&#access)),
            Mode::Partial => quote!(size_pnext_partial(&#access)),
        },
    }
}

fn scalar_size(s: &Scalar, access: &TokenStream, mode: Mode) -> TokenStream {
    if let Some(n) = s.wire_size() {
        let lit = Literal::usize_suffixed(n);
        return quote!(#lit);
    }
    match s {
        Scalar::Struct(n) | Scalar::Union(n) => {
            let f = match mode {
                Mode::Full => size_fn(n),
                Mode::Partial => size_partial_fn(n),
            };
            quote!(#f(&#access))
        }
        _ => unreachable!("every other scalar has a fixed wire size"),
    }
}

/// Statements that write this shape.
pub fn encode_stmt(shape: &Shape, access: &TokenStream, mode: Mode) -> TokenStream {
    match shape {
        Shape::Scalar(s) => scalar_encode(s, access, mode),
        Shape::FixedArray { elem, count, .. } => {
            let n = *count;
            let body = match packed_kind(elem) {
                Some(PackedKind::Bytes) => quote!(enc.blob(&#access)?;),
                Some(PackedKind::U16) => quote!(enc.packed_u16(&#access)?;),
                _ => {
                    let e = encode_stmt(elem, &quote!((*item)), mode);
                    quote!(for item in #access.iter() { #e })
                }
            };
            quote! {
                enc.array_size(#n)?;
                #body
            }
        }
        Shape::Optional(inner) => {
            let e = encode_stmt(inner, &quote!((*inner)), mode);
            quote! {
                match &#access {
                    Some(inner) => { enc.presence(true)?; #e }
                    None => { enc.presence(false)?; }
                }
            }
        }
        Shape::Array { elem, .. } => {
            let body = match packed_kind(elem) {
                Some(PackedKind::Bytes) => quote!(enc.blob(&#access)?;),
                Some(PackedKind::U16) => quote!(enc.packed_u16(&#access)?;),
                _ => {
                    let e = encode_stmt(elem, &quote!((*item)), mode);
                    quote!(for item in #access.iter() { #e })
                }
            };
            quote! {
                enc.array_size(#access.len() as u64)?;
                #body
            }
        }
        Shape::Blob { .. } => quote! {
            enc.array_size(#access.len() as u64)?;
            enc.blob(&#access)?;
        },
        Shape::CString => quote!(enc.c_string(&#access)?;),
        Shape::CStringArray { .. } => quote! {
            enc.array_size(#access.len() as u64)?;
            for s in #access.iter() { enc.c_string(s)?; }
        },
        Shape::PNext => match mode {
            Mode::Full => quote!(encode_pnext(enc, &#access)?;),
            Mode::Partial => quote!(encode_pnext_partial(enc, &#access)?;),
        },
    }
}

fn scalar_encode(s: &Scalar, access: &TokenStream, mode: Mode) -> TokenStream {
    match s {
        Scalar::U8 => quote!(enc.u8(#access)?;),
        Scalar::I8 => quote!(enc.i8(#access)?;),
        Scalar::U16 => quote!(enc.u16(#access)?;),
        Scalar::I16 => quote!(enc.i16(#access)?;),
        Scalar::U32 => quote!(enc.u32(#access)?;),
        Scalar::I32 => quote!(enc.i32(#access)?;),
        Scalar::U64 | Scalar::Size => quote!(enc.u64(#access)?;),
        Scalar::I64 => quote!(enc.i64(#access)?;),
        Scalar::F32 => quote!(enc.f32(#access)?;),
        Scalar::F64 => quote!(enc.f64(#access)?;),
        Scalar::Bool32 => quote!(enc.bool32(#access)?;),
        Scalar::OpaqueU64(_) => quote!(enc.u64(#access)?;),
        Scalar::Enum(_) => quote!(enc.i32(#access.0)?;),
        Scalar::Bitmask(_) => quote!(enc.u32(#access.0)?;),
        Scalar::Bitmask64(_) => quote!(enc.u64(#access.0)?;),
        Scalar::Handle(_) => quote!(enc.handle(#access.0)?;),
        Scalar::Struct(n) | Scalar::Union(n) => {
            let f = match mode {
                Mode::Full => encode_fn(n),
                Mode::Partial => encode_partial_fn(n),
            };
            quote!(#f(enc, &#access)?;)
        }
    }
}

/// An expression producing the decoded value, with an optional upper bound on
/// the outermost array's element count taken from its `len=` sibling.
///
/// See [`sv_wire::Decoder::array_size_bounded`] for why the bound is needed at
/// all: a partially-encoded element can occupy zero bytes, and a byte-based
/// plausibility check has nothing to bite on.
pub fn decode_expr_bounded(shape: &Shape, mode: Mode, bound: Option<&TokenStream>) -> TokenStream {
    let Some(bound) = bound else { return decode_expr(shape, mode) };
    match shape {
        Shape::Array { elem, .. } if packed_kind(elem).is_none() => {
            let ty = super::rust_type(elem);
            let e = decode_expr(elem, mode);
            let min = elem.min_wire_size();
            quote!({
                let n = dec.array_size_bounded(#min, #bound)?;
                let mut v: Vec<#ty> = Vec::with_capacity(n);
                for _ in 0..n { v.push(#e); }
                v
            })
        }
        _ => decode_expr(shape, mode),
    }
}

/// An expression producing the decoded value. Valid inside a function returning
/// [`sv_wire::Result`], because it uses `?`.
pub fn decode_expr(shape: &Shape, mode: Mode) -> TokenStream {
    match shape {
        Shape::Scalar(s) => scalar_decode(s, mode),
        Shape::FixedArray { elem, count, .. } => {
            let n = *count as usize;
            let n64 = *count;
            let ty = super::rust_type(elem);
            match packed_kind(elem) {
                Some(PackedKind::Bytes) => quote!({
                    dec.fixed_array_size(#n64)?;
                    let bytes = dec.blob(#n)?;
                    let mut out = [0u8; #n];
                    out.copy_from_slice(bytes);
                    out
                }),
                Some(PackedKind::U16) => quote!({
                    dec.fixed_array_size(#n64)?;
                    let v = dec.packed_u16(#n)?;
                    let out: [u16; #n] = v.try_into().map_err(|_| {
                        sv_wire::WireError::ArrayLenMismatch { expected: #n64, found: 0 }
                    })?;
                    out
                }),
                _ => {
                    let e = decode_expr(elem, mode);
                    quote!({
                        dec.fixed_array_size(#n64)?;
                        let mut v: Vec<#ty> = Vec::with_capacity(#n);
                        for _ in 0..#n { v.push(#e); }
                        let out: [#ty; #n] = v.try_into().map_err(|_| {
                            sv_wire::WireError::ArrayLenMismatch { expected: #n64, found: 0 }
                        })?;
                        out
                    })
                }
            }
        }
        Shape::Optional(inner) => {
            let e = decode_expr(inner, mode);
            if super::types::is_aggregate(inner) {
                quote!(if dec.presence()? { Some(Box::new(#e)) } else { None })
            } else {
                quote!(if dec.presence()? { Some(#e) } else { None })
            }
        }
        Shape::Array { elem, .. } => {
            let ty = super::rust_type(elem);
            match packed_kind(elem) {
                Some(PackedKind::Bytes) => quote!({
                    let n = dec.array_size(1)?;
                    dec.blob(n)?.to_vec()
                }),
                Some(PackedKind::U16) => quote!({
                    let n = dec.array_size(2)?;
                    dec.packed_u16(n)?
                }),
                _ => {
                    let e = decode_expr(elem, mode);
                    // `max(1)` so that a struct whose minimum encoding is empty
                    // still bounds the count by the bytes that remain. Without
                    // it a count of `u64::MAX` would pass the plausibility check
                    // and then be handed to `Vec::with_capacity`.
                    let min = elem.min_wire_size().max(1);
                    quote!({
                        let n = dec.array_size(#min)?;
                        let mut v: Vec<#ty> = Vec::with_capacity(n);
                        for _ in 0..n { v.push(#e); }
                        v
                    })
                }
            }
        }
        Shape::Blob { .. } => quote!({
            let n = dec.array_size(1)?;
            dec.blob(n)?.to_vec()
        }),
        Shape::CString => quote!(dec.c_string()?.to_vec()),
        Shape::CStringArray { .. } => quote!({
            let n = dec.array_size(8)?;
            let mut v: Vec<Vec<u8>> = Vec::with_capacity(n);
            for _ in 0..n { v.push(dec.c_string()?.to_vec()); }
            v
        }),
        Shape::PNext => quote!(Vec::new()),
    }
}

fn scalar_decode(s: &Scalar, mode: Mode) -> TokenStream {
    match s {
        Scalar::U8 => quote!(dec.u8()?),
        Scalar::I8 => quote!(dec.i8()?),
        Scalar::U16 => quote!(dec.u16()?),
        Scalar::I16 => quote!(dec.i16()?),
        Scalar::U32 => quote!(dec.u32()?),
        Scalar::I32 => quote!(dec.i32()?),
        Scalar::U64 | Scalar::Size => quote!(dec.u64()?),
        Scalar::I64 => quote!(dec.i64()?),
        Scalar::F32 => quote!(dec.f32()?),
        Scalar::F64 => quote!(dec.f64()?),
        Scalar::Bool32 => quote!(dec.bool32()?),
        Scalar::OpaqueU64(_) => quote!(dec.u64()?),
        Scalar::Enum(n) => {
            let t = naming::type_ident(n);
            quote!(#t(dec.i32()?))
        }
        Scalar::Bitmask(n) => {
            let t = naming::type_ident(n);
            quote!(#t(dec.u32()?))
        }
        Scalar::Bitmask64(n) => {
            let t = naming::type_ident(n);
            quote!(#t(dec.u64()?))
        }
        Scalar::Handle(n) => {
            let t = naming::type_ident(n);
            quote!(#t(dec.handle()?))
        }
        Scalar::Struct(n) | Scalar::Union(n) => {
            let f = match mode {
                Mode::Full => decode_fn(n),
                Mode::Partial => decode_partial_fn(n),
            };
            quote!(#f(dec)?)
        }
    }
}

/// How an array of this element type is laid out.
enum PackedKind {
    /// One byte per element, padded once at the end.
    Bytes,
    /// Two bytes per element, padded once at the end.
    U16,
    /// `n` bytes per element, no padding.
    Wide(usize),
}

fn packed_kind(shape: &Shape) -> Option<PackedKind> {
    let Shape::Scalar(s) = shape else { return None };
    match s {
        Scalar::U8 => Some(PackedKind::Bytes),
        Scalar::U16 => Some(PackedKind::U16),
        Scalar::I8 | Scalar::I16 => None,
        _ => s.wire_size().map(PackedKind::Wide),
    }
}

// ---------------------------------------------------------------------------
// struct bodies
// ---------------------------------------------------------------------------

fn emit_struct(plan: &Plan, s: &StructPlan) -> TokenStream {
    if s.is_union {
        return emit_union(s);
    }
    let full = emit_struct_mode(plan, s, Mode::Full);
    let partial = if s.needs_partial {
        emit_struct_mode(plan, s, Mode::Partial)
    } else {
        quote!()
    };
    quote! {
        #full
        #partial
    }
}

fn emit_struct_mode(_plan: &Plan, s: &StructPlan, mode: Mode) -> TokenStream {
    let ty = naming::type_ident(&s.name);
    let (size_id, encode_id, decode_id) = match mode {
        Mode::Full => (size_fn(&s.name), encode_fn(&s.name), decode_fn(&s.name)),
        Mode::Partial => {
            (size_partial_fn(&s.name), encode_partial_fn(&s.name), decode_partial_fn(&s.name))
        }
    };
    let (size_self_id, encode_self_id, decode_self_id) = match mode {
        Mode::Full => (size_self_fn(&s.name), encode_self_fn(&s.name), decode_self_fn(&s.name)),
        Mode::Partial => (
            format_ident!("{}_partial", size_self_fn(&s.name)),
            format_ident!("{}_partial", encode_self_fn(&s.name)),
            format_ident!("{}_partial", decode_self_fn(&s.name)),
        ),
    };

    // -- the `self` body: every member but `sType` and `pNext` --------------
    let body_fields: Vec<_> = s
        .fields
        .iter()
        .filter(|f| !matches!(f.shape, Shape::PNext))
        .filter(|f| mode == Mode::Full || partial_validity(f) != PartialValidity::Skip)
        .collect();

    let field_mode = |f: &&crate::plan::FieldPlan| match mode {
        Mode::Full => Mode::Full,
        Mode::Partial => match partial_validity(f) {
            PartialValidity::Full => Mode::Full,
            _ => Mode::Partial,
        },
    };

    let size_parts = body_fields.iter().map(|f| {
        let id = naming::ident(&f.rust_name);
        let e = size_expr(&f.shape, &quote!(v.#id), field_mode(f));
        quote!(n += #e;)
    });
    let encode_parts = body_fields.iter().map(|f| {
        let id = naming::ident(&f.rust_name);
        encode_stmt(&f.shape, &quote!(v.#id), field_mode(f))
    });
    let decode_parts = body_fields.iter().map(|f| {
        let id = naming::ident(&f.rust_name);
        let bound = count_bound(&s.fields, f, &quote!(out));
        let e = decode_expr_bounded(&f.shape, field_mode(f), bound.as_ref());
        quote!(out.#id = #e;)
    });

    // -- length cross-checks -------------------------------------------------
    //
    // Run on both sides: on encode so a malformed value never reaches the wire,
    // and on decode so a peer that disagrees with us about a `len=` is caught
    // rather than believed.
    let encode_checks: Vec<TokenStream> = if mode == Mode::Full {
        s.fields.iter().filter_map(|f| length_check(&s.fields, f, &quote!(v))).collect()
    } else {
        Vec::new()
    };
    let decode_checks: Vec<TokenStream> = if mode == Mode::Full {
        s.fields.iter().filter_map(|f| length_check(&s.fields, f, &quote!(out))).collect()
    } else {
        Vec::new()
    };

    // -- the sType / pNext wrapper ------------------------------------------
    let has_pnext_field = s.fields.iter().any(|f| matches!(f.shape, Shape::PNext));
    let pnext_field = s
        .fields
        .iter()
        .find(|f| matches!(f.shape, Shape::PNext))
        .map(|f| naming::ident(&f.rust_name));
    let allowed_fn = pnext::allowed_fn_ident(&s.name);

    let (wrapper_size, wrapper_encode, wrapper_decode) = match (s.s_type_value, has_pnext_field) {
        (Some(v), true) => {
            let sv = v as i32;
            let pf = pnext_field.clone().unwrap();
            let (size_chain, enc_chain, dec_chain) = match mode {
                Mode::Full => (
                    quote!(size_pnext(&v.#pf)),
                    quote!(encode_pnext(enc, &v.#pf)?;),
                    quote!(decode_pnext(dec, #allowed_fn)?),
                ),
                Mode::Partial => (
                    quote!(size_pnext_partial(&v.#pf)),
                    quote!(encode_pnext_partial(enc, &v.#pf)?;),
                    quote!(decode_pnext_partial(dec, #allowed_fn)?),
                ),
            };
            (
                quote!(4usize + #size_chain + #size_self_id(v)),
                quote! {
                    enc.i32(#sv)?;
                    #enc_chain
                    #encode_self_id(enc, v)?;
                    Ok(())
                },
                quote! {
                    let s_type = dec.i32()?;
                    if s_type != #sv {
                        return Err(sv_wire::WireError::UnknownPNext { s_type });
                    }
                    let chain = #dec_chain;
                    let mut out = #decode_self_id(dec)?;
                    out.#pf = chain;
                    Ok(out)
                },
            )
        }
        (Some(v), false) => {
            let sv = v as i32;
            (
                quote!(4usize + #size_self_id(v)),
                quote! {
                    enc.i32(#sv)?;
                    #encode_self_id(enc, v)?;
                    Ok(())
                },
                quote! {
                    let s_type = dec.i32()?;
                    if s_type != #sv {
                        return Err(sv_wire::WireError::UnknownPNext { s_type });
                    }
                    #decode_self_id(dec)
                },
            )
        }
        (None, _) => (
            quote!(#size_self_id(v)),
            quote!(#encode_self_id(enc, v)),
            quote!(#decode_self_id(dec)),
        ),
    };

    // A struct with no encoded members — `VkBaseInStructure` is one — would
    // otherwise get a `let mut` that is never assigned to. Generated code that
    // warns is generated code nobody reads the warnings of.
    let mut_n = if body_fields.is_empty() { quote!() } else { quote!(mut) };
    let mut_out = if body_fields.is_empty() && decode_checks.is_empty() {
        quote!()
    } else {
        quote!(mut)
    };

    let size_doc = format!("Bytes `{}` occupies on the wire.", s.name);
    let enc_doc = format!("Write a `{}`.", s.name);
    let dec_doc = format!("Read a `{}`.", s.name);

    quote! {
        #[doc = #size_doc]
        pub fn #size_id(v: &#ty) -> usize { #wrapper_size }

        #[doc = #enc_doc]
        pub fn #encode_id(enc: &mut sv_wire::Encoder<'_>, v: &#ty) -> sv_wire::Result<()> {
            #wrapper_encode
        }

        #[doc = #dec_doc]
        pub fn #decode_id(dec: &mut sv_wire::Decoder<'_>) -> sv_wire::Result<#ty> {
            #wrapper_decode
        }

        pub fn #size_self_id(v: &#ty) -> usize {
            let #mut_n n = 0usize;
            #(#size_parts)*
            let _ = v;
            n
        }

        pub fn #encode_self_id(enc: &mut sv_wire::Encoder<'_>, v: &#ty) -> sv_wire::Result<()> {
            #(#encode_checks)*
            #(#encode_parts)*
            let _ = (&enc, v);
            Ok(())
        }

        pub fn #decode_self_id(dec: &mut sv_wire::Decoder<'_>) -> sv_wire::Result<#ty> {
            let #mut_out out = <#ty as Default>::default();
            #(#decode_parts)*
            #(#decode_checks)*
            let _ = &dec;
            Ok(out)
        }
    }
}

/// The generated cross-check between a `xxxCount` member and the array it is
/// declared to be the length of.
///
/// This is the mitigation R8 asks for, made concrete: the wire carries its own
/// count, so a wrong `len=` cannot desynchronise the stream — it can only make
/// this check fire.
/// An upper bound on an array's element count, from the `len=` sibling that has
/// already been decoded.
///
/// Only when the sibling really does come first in declaration order. Vulkan
/// puts `xxxCount` before `pXxx` without exception, but "without exception" is
/// a claim about today's registry, and reading a field that has not been
/// assigned yet would silently bound the array by zero.
pub fn count_bound(
    fields: &[crate::plan::FieldPlan],
    f: &crate::plan::FieldPlan,
    binding: &TokenStream,
) -> Option<TokenStream> {
    let len = match &f.shape {
        Shape::Array { len, .. } => len.as_ref()?,
        _ => return None,
    };
    let (sibling, divisor) = match len {
        LenCheck::Sibling(s) => (s.clone(), 1u64),
        LenCheck::SiblingDiv { sibling, divisor } => (sibling.clone(), *divisor),
        _ => return None,
    };
    let here = fields.iter().position(|c| c.name == f.name)?;
    let there = fields.iter().position(|c| c.name == sibling)?;
    if there >= here {
        return None;
    }
    let target = &fields[there];
    let count = naming::ident(&target.rust_name);
    match &target.shape {
        Shape::Scalar(_) => Some(quote!(((#binding.#count as u64) / #divisor) as usize)),
        Shape::Optional(inner) if matches!(**inner, Shape::Scalar(_)) => {
            Some(quote!((#binding.#count.unwrap_or(0) as u64 / #divisor) as usize))
        }
        _ => None,
    }
}

fn length_check(
    fields: &[crate::plan::FieldPlan],
    f: &crate::plan::FieldPlan,
    binding: &TokenStream,
) -> Option<TokenStream> {
    let len = match &f.shape {
        // `pAllocateInfo->commandBufferCount` names a field of a *different*
        // parameter, so it is a command-level check, not a struct-level one.
        // `Unchecked` is a length the registry states in a form we deliberately
        // do not parse (design.md §7.4: do not build a LaTeX parser).
        Shape::Array { len, .. } | Shape::Blob { len } | Shape::CStringArray { len } => {
            len.as_ref()?
        }
        _ => return None,
    };
    let (sibling, divisor) = match len {
        LenCheck::Sibling(s) => (s.clone(), 1u64),
        LenCheck::SiblingDiv { sibling, divisor } => (sibling.clone(), *divisor),
        LenCheck::Path { .. } | LenCheck::Unchecked(_) => return None,
    };
    // The sibling must exist here and be a plain scalar. In the count-then-fill
    // idiom it is a pointer instead (`uint32_t* pPhysicalDeviceCount`), where
    // the outbound value is the caller's *capacity* rather than the array's
    // length and comparing them would be wrong, not merely awkward.
    let target = fields.iter().find(|c| c.name == sibling)?;
    if !matches!(target.shape, Shape::Scalar(_)) {
        return None;
    }
    let array = naming::ident(&f.rust_name);
    let count = naming::ident(&target.rust_name);
    let label = if divisor == 1 {
        format!("{sibling} (the len= of {})", f.name)
    } else {
        format!("{sibling} / {divisor} (the altlen= of {})", f.name)
    };

    // An array that may be absent *while its count is non-zero* must not trip
    // the check, and this is not a corner case. Two registry markers say it:
    //
    // - `optional="true"`. `VkSubpassDescription` has `colorAttachmentCount`
    //   governing `pColorAttachments`, `pResolveAttachments` and
    //   `pDepthStencilAttachment`, and a subpass with no resolve attachments
    //   passes NULL for the second while the count describes the first.
    //
    // - `noautovalidity="true"`, which the registry uses for a member whose
    //   validity is decided by *another member's value* rather than by its own
    //   metadata. `VkWriteDescriptorSet` is the case that matters:
    //   `descriptorCount` is the `len=` of `pImageInfo`, `pBufferInfo` and
    //   `pTexelBufferView`, and `descriptorType` picks one. None of the three is
    //   `optional`, so keying only on that rejected **every legal
    //   `vkUpdateDescriptorSets`** — which is how this was found, by a compute
    //   dispatch whose descriptors are buffers and whose `pImageInfo` is
    //   therefore empty.
    //
    // Venus encodes a NULL array as a count of zero, so "empty" and "absent" are
    // the same thing on the wire, and the relaxation costs nothing: the wire
    // carries its own count for every array (§7.4), so this check is a verifier
    // and never an input. What is given up is catching a *populated* array whose
    // count disagrees — and that is still caught, because the guard only skips
    // the empty case.
    let guard = if f.optional || f.no_auto_validity {
        quote!(!#binding.#array.is_empty() &&)
    } else {
        quote!()
    };

    Some(quote! {
        if #guard (#binding.#count as u64) / #divisor != #binding.#array.len() as u64 {
            return Err(sv_wire::WireError::LengthDisagreement {
                field: #label,
                declared: (#binding.#count as u64) / #divisor,
                actual: #binding.#array.len() as u64,
            });
        }
    })
}

fn emit_union(s: &StructPlan) -> TokenStream {
    let ty = naming::type_ident(&s.name);
    let size_id = size_fn(&s.name);
    let encode_id = encode_fn(&s.name);
    let decode_id = decode_fn(&s.name);

    let size_arms = s.fields.iter().map(|f| {
        let v = naming::union_variant(&f.name);
        let e = size_expr(&f.shape, &quote!((*inner)), Mode::Full);
        quote!(#ty::#v(inner) => { let _ = &inner; #e },)
    });
    let encode_arms = s.fields.iter().enumerate().map(|(i, f)| {
        let v = naming::union_variant(&f.name);
        let tag = i as u32;
        let e = encode_stmt(&f.shape, &quote!((*inner)), Mode::Full);
        quote! {
            #ty::#v(inner) => { enc.u32(#tag)?; #e }
        }
    });
    let decode_arms = s.fields.iter().enumerate().map(|(i, f)| {
        let v = naming::union_variant(&f.name);
        let tag = i as u32;
        let e = decode_expr(&f.shape, Mode::Full);
        quote!(#tag => #ty::#v(#e),)
    });

    // A union that is `returnedonly` still needs a partial form, because it may
    // sit inside one. Its partial encoding is its full one: the tag is the only
    // thing the caller could have got wrong, and it is not optional.
    let partial = if s.needs_partial {
        let sp = size_partial_fn(&s.name);
        let ep = encode_partial_fn(&s.name);
        let dp = decode_partial_fn(&s.name);
        quote! {
            pub fn #sp(v: &#ty) -> usize { #size_id(v) }
            pub fn #ep(enc: &mut sv_wire::Encoder<'_>, v: &#ty) -> sv_wire::Result<()> {
                #encode_id(enc, v)
            }
            pub fn #dp(dec: &mut sv_wire::Decoder<'_>) -> sv_wire::Result<#ty> {
                #decode_id(dec)
            }
        }
    } else {
        quote!()
    };

    quote! {
        pub fn #size_id(v: &#ty) -> usize {
            4usize + match v { #(#size_arms)* }
        }

        pub fn #encode_id(enc: &mut sv_wire::Encoder<'_>, v: &#ty) -> sv_wire::Result<()> {
            match v { #(#encode_arms)* }
            Ok(())
        }

        pub fn #decode_id(dec: &mut sv_wire::Decoder<'_>) -> sv_wire::Result<#ty> {
            let tag = dec.u32()?;
            Ok(match tag {
                #(#decode_arms)*
                _ => return Err(sv_wire::WireError::UnknownUnionTag { tag }),
            })
        }

        #partial
    }
}

use super::pnext;
