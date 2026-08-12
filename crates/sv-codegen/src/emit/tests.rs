//! Pass 11: generated round-trip tests (design.md §7.2).
//!
//! `security-model.md` R8 is explicit that symmetry proves nothing: "marshalling
//! that is wrong in the same way on both sides … *looks* consistent while
//! corrupting memory". These tests are symmetric and therefore do **not** answer
//! R8; the differential comparison against Venus's generated C, in
//! `crates/sv-testkit`, is what answers R8.
//!
//! What they *do* answer is the other half — that the three passes agree with
//! each other — and that is worth a great deal on its own:
//!
//! - `size` and `encode` agree, because the encoder is handed exactly
//!   `size(v)` bytes and [`sv_wire::Encoder::finish`] fails if it does not fill
//!   them.
//! - `encode` and `decode` agree, because the decoded value compares equal and
//!   the decoder consumes exactly the bytes written.
//! - the `_partial` pair agrees with itself, checked by re-encoding what was
//!   decoded and comparing bytes, which is the only form of equality that means
//!   anything when the point of the encoding is that some fields are absent.
//!
//! The samples deliberately hit the cases that are easy to get wrong: nested
//! structs, `pNext` chains, arrays whose length lives in a sibling member,
//! optional pointers, fixed-extent arrays, unions, and callee-filled parameters.

use crate::naming;
use crate::plan::{LenCheck, PartialValidity, Plan, Scalar, Shape, StructPlan, partial_validity};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

pub fn emit(plan: &Plan) -> TokenStream {
    let rng = emit_rng();
    let samplers = plan.structs.iter().map(|s| emit_sampler(plan, s));
    let struct_tests = plan.structs.iter().map(emit_struct_test);
    let command_samplers = plan.commands.iter().map(|c| emit_command_sampler(plan, c));
    let command_tests = plan.commands.iter().map(emit_command_test);
    let server = emit_test_server(plan);

    quote! {
        #[cfg(test)]
        #[allow(clippy::all)]
        mod generated_roundtrip {
            use super::*;

            #rng
            #(#samplers)*
            #(#command_samplers)*
            #server
            #(#struct_tests)*
            #(#command_tests)*
        }
    }
}

/// A deterministic sampler.
///
/// Not `rand`: a generated test that fails only on some seeds is a test nobody
/// can bisect. Every run produces the same values, so a failure is reproducible
/// from the test name alone.
fn emit_rng() -> TokenStream {
    quote! {
        pub struct Rng { state: u64, depth: u32 }

        // Which draws are used depends on which types the profile contains, so
        // some are dead in a narrow profile and live in a wider one.
        #[allow(dead_code)]
        impl Rng {
            pub fn new(seed: u64) -> Rng { Rng { state: seed | 1, depth: 0 } }

            fn next(&mut self) -> u64 {
                // SplitMix64. Small, well-distributed, and identical on every
                // platform we build for — including the Windows cross target,
                // where a differently-seeded test would be a false difference.
                self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = self.state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            }

            pub fn u64(&mut self) -> u64 { self.next() }
            pub fn u32(&mut self) -> u32 { self.next() as u32 }
            pub fn i32(&mut self) -> i32 { (self.next() % 64) as i32 }
            pub fn i64(&mut self) -> i64 { self.next() as i64 }
            pub fn u16(&mut self) -> u16 { self.next() as u16 }
            pub fn i16(&mut self) -> i16 { (self.next() % 64) as i16 }
            pub fn u8(&mut self) -> u8 { self.next() as u8 }
            pub fn i8(&mut self) -> i8 { (self.next() % 64) as i8 }
            /// Finite only: a NaN would make `PartialEq` fail for reasons that
            /// have nothing to do with marshalling.
            pub fn f32(&mut self) -> f32 { (self.next() % 4096) as f32 / 16.0 }
            pub fn f64(&mut self) -> f64 { (self.next() % 4096) as f64 / 16.0 }
            pub fn bool(&mut self) -> bool { self.next() & 1 == 1 }

            /// Short but not always empty, and never zero on the first draw, so
            /// that an array's contents are actually exercised.
            pub fn len(&mut self) -> usize { (self.next() % 3) as usize + 1 }

            pub fn bytes(&mut self) -> Vec<u8> {
                let n = self.len();
                (0..n).map(|_| b'a' + (self.next() % 26) as u8).collect()
            }

            /// Bounded recursion, so a struct graph that is deep or that reaches
            /// itself through a chain cannot make the sampler diverge.
            pub fn deeper(&mut self) -> bool {
                if self.depth >= 3 { return false; }
                self.depth += 1;
                true
            }
            pub fn shallower(&mut self) { self.depth = self.depth.saturating_sub(1); }
            pub fn at_limit(&self) -> bool { self.depth >= 3 }
        }
    }
}

fn sampler_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("sample_{}", naming::snake_case(name))
}

fn emit_sampler(plan: &Plan, s: &StructPlan) -> TokenStream {
    let ty = naming::type_ident(&s.name);
    let id = sampler_fn(&s.name);

    if s.is_union {
        let arms = s.fields.iter().enumerate().map(|(i, f)| {
            let v = naming::union_variant(&f.name);
            let e = sample_expr(plan, &f.shape, &s.name, None);
            let i = i as u64;
            quote!(#i => #ty::#v(#e),)
        });
        let n = s.fields.len() as u64;
        return quote! {
            pub fn #id(rng: &mut Rng) -> #ty {
                match rng.u64() % #n {
                    #(#arms)*
                    _ => <#ty as Default>::default(),
                }
            }
        };
    }

    let all: Vec<&crate::plan::FieldPlan> = s.fields.iter().collect();
    let lens = shared_lengths(&all);
    let len_decls = lens.iter().map(|name| {
        let id = len_var(name);
        quote!(let #id = rng.len();)
    });
    let fields = s.fields.iter().map(|f| {
        let fid = naming::ident(&f.rust_name);
        let e = sample_expr(plan, &f.shape, &s.name, len_of(&all, f).as_ref());
        quote!(out.#fid = #e;)
    });
    let fixups = s.fields.iter().filter_map(|f| length_fixup(&all, f, &quote!(out)));

    quote! {
        pub fn #id(rng: &mut Rng) -> #ty {
            let mut out = <#ty as Default>::default();
            if !rng.deeper() { return out; }
            // One length per `len=` sibling, drawn once and shared. Several
            // arrays routinely hang off one count — `VkSubpassDescription` has
            // three on `colorAttachmentCount` — and sampling them
            // independently produces a value the encoder is right to reject.
            #(#len_decls)*
            #(#fields)*
            rng.shallower();
            // Make the sample satisfy the registry's own `len=` relations, so
            // that the generated consistency check is exercised rather than
            // tripped over.
            #(#fixups)*
            let _ = &rng;
            out
        }
    }
}

/// `queue_family_index_count = p_queue_family_indices.len()`, and the `altlen`
/// variant.
///
/// The count is not always a plain scalar. In the count-then-fill idiom —
/// `vkEnumeratePhysicalDevices(instance, pPhysicalDeviceCount, pPhysicalDevices)`
/// — the count is itself a `uint32_t*`, so its Rust type is `Option<u32>` and
/// the assignment has to be wrapped. Finding that out from a compile error in
/// generated code is the cheap version of finding it out from a corrupted
/// stream.
fn length_fixup(
    fields: &[&crate::plan::FieldPlan],
    f: &crate::plan::FieldPlan,
    binding: &TokenStream,
) -> Option<TokenStream> {
    let len = match &f.shape {
        Shape::Array { len, .. } | Shape::Blob { len } | Shape::CStringArray { len } => {
            len.as_ref()?
        }
        _ => return None,
    };
    let array = naming::ident(&f.rust_name);
    let (sibling, factor) = match len {
        LenCheck::Sibling(s) => (s.clone(), 1u64),
        LenCheck::SiblingDiv { sibling, divisor } => (sibling.clone(), *divisor),
        _ => return None,
    };
    let target = fields.iter().find(|c| c.name == sibling)?;
    let count = naming::ident(&target.rust_name);
    let value = quote!((#binding.#array.len() as u64 * #factor));
    match &target.shape {
        Shape::Scalar(_) => Some(quote!(#binding.#count = #value as _;)),
        Shape::Optional(inner) if matches!(**inner, Shape::Scalar(_)) => {
            Some(quote!(#binding.#count = Some(#value as _);))
        }
        _ => None,
    }
}

/// The `len=` sibling a field's length must agree with, if any.
fn len_of(fields: &[&crate::plan::FieldPlan], f: &crate::plan::FieldPlan) -> Option<String> {
    let len = match &f.shape {
        Shape::Array { len, .. } | Shape::Blob { len } | Shape::CStringArray { len } => {
            len.as_ref()?
        }
        _ => return None,
    };
    let sibling = match len {
        LenCheck::Sibling(s) => s.clone(),
        LenCheck::SiblingDiv { sibling, .. } => sibling.clone(),
        _ => return None,
    };
    let target = fields.iter().find(|c| c.name == sibling)?;
    match &target.shape {
        Shape::Scalar(_) => Some(sibling),
        Shape::Optional(inner) if matches!(**inner, Shape::Scalar(_)) => Some(sibling),
        _ => None,
    }
}

fn shared_lengths(fields: &[&crate::plan::FieldPlan]) -> Vec<String> {
    let mut out: Vec<String> = fields.iter().filter_map(|f| len_of(fields, f)).collect();
    out.sort();
    out.dedup();
    out
}

fn len_var(sibling: &str) -> proc_macro2::Ident {
    format_ident!("len_{}", naming::snake_case(sibling))
}

fn sample_expr(
    plan: &Plan,
    shape: &Shape,
    owner: &str,
    shared_len: Option<&String>,
) -> TokenStream {
    let n_expr = match shared_len {
        Some(s) => {
            let id = len_var(s);
            quote!(#id)
        }
        None => quote!(rng.len()),
    };
    match shape {
        Shape::Scalar(s) => sample_scalar(s),
        Shape::FixedArray { elem, count, .. } => {
            let inner = sample_expr(plan, elem, owner, None);
            let n = *count as usize;
            // `from_fn` rather than `[expr; N]`, which would need the element to
            // be `Copy` and would evaluate the expression once.
            quote!(core::array::from_fn::<_, #n, _>(|_| #inner))
        }
        Shape::Optional(inner) => {
            let e = sample_expr(plan, inner, owner, None);
            if super::types::is_aggregate(inner) {
                quote!(if rng.bool() { Some(Box::new(#e)) } else { None })
            } else {
                quote!(if rng.bool() { Some(#e) } else { None })
            }
        }
        Shape::Array { elem, .. } => {
            let e = sample_expr(plan, elem, owner, None);
            quote!({ let n = #n_expr; (0..n).map(|_| #e).collect::<Vec<_>>() })
        }
        Shape::Blob { .. } => quote!({
            let n = #n_expr;
            (0..n).map(|_| rng.u8()).collect::<Vec<u8>>()
        }),
        Shape::CString => quote!(rng.bytes()),
        Shape::CStringArray { .. } => {
            quote!({ let n = #n_expr; (0..n).map(|_| rng.bytes()).collect::<Vec<_>>() })
        }
        Shape::PNext => sample_pnext(plan, owner),
    }
}

fn sample_scalar(s: &Scalar) -> TokenStream {
    match s {
        Scalar::U8 => quote!(rng.u8()),
        Scalar::I8 => quote!(rng.i8()),
        Scalar::U16 => quote!(rng.u16()),
        Scalar::I16 => quote!(rng.i16()),
        Scalar::U32 => quote!(rng.u32()),
        Scalar::I32 => quote!(rng.i32()),
        Scalar::U64 | Scalar::Size | Scalar::OpaqueU64(_) => quote!(rng.u64()),
        Scalar::I64 => quote!(rng.i64()),
        Scalar::F32 => quote!(rng.f32()),
        Scalar::F64 => quote!(rng.f64()),
        Scalar::Bool32 => quote!(rng.bool()),
        Scalar::Enum(n) => {
            let t = naming::type_ident(n);
            quote!(#t(rng.i32()))
        }
        Scalar::Bitmask(n) => {
            let t = naming::type_ident(n);
            quote!(#t(rng.u32()))
        }
        Scalar::Bitmask64(n) => {
            let t = naming::type_ident(n);
            quote!(#t(rng.u64()))
        }
        Scalar::Handle(n) => {
            let t = naming::type_ident(n);
            quote!(#t(rng.u64()))
        }
        Scalar::Struct(n) | Scalar::Union(n) => {
            let f = sampler_fn(n);
            quote!(#f(rng))
        }
    }
}

/// Sample a chain from the extenders the parent actually permits.
fn sample_pnext(plan: &Plan, owner: &str) -> TokenStream {
    let Some(parent) = plan.struct_plan(owner) else { return quote!(Vec::new()) };
    if parent.extenders.is_empty() {
        return quote!(Vec::new());
    }
    let arms = parent.extenders.iter().enumerate().filter_map(|(i, e)| {
        plan.struct_plan(e)?;
        let v = naming::pnext_variant(e);
        let f = sampler_fn(e);
        let i = i as u64;
        Some(quote!(#i => Some(PNextLink::#v(Box::new(#f(rng)))),))
    });
    let arms: Vec<_> = arms.collect();
    if arms.is_empty() {
        return quote!(Vec::new());
    }
    let n = parent.extenders.len() as u64;
    quote!({
        if rng.at_limit() {
            Vec::new()
        } else {
            let count = rng.u64() % 3;
            (0..count)
                .filter_map(|_| match rng.u64() % #n {
                    #(#arms)*
                    _ => None,
                })
                .collect::<Vec<_>>()
        }
    })
}

fn emit_struct_test(s: &StructPlan) -> TokenStream {
    let ty = naming::type_ident(&s.name);
    let sample = sampler_fn(&s.name);
    let size = super::wire::size_fn(&s.name);
    let encode = super::wire::encode_fn(&s.name);
    let decode = super::wire::decode_fn(&s.name);
    let test_name = format_ident!("roundtrip_{}", naming::snake_case(&s.name));
    let name = &s.name;

    let partial_case = if s.needs_partial {
        let sp = super::wire::size_partial_fn(&s.name);
        let ep = super::wire::encode_partial_fn(&s.name);
        let dp = super::wire::decode_partial_fn(&s.name);
        quote! {
            // The partial form drops fields on purpose, so value equality is
            // meaningless. Byte idempotence is not: encoding what was decoded
            // must reproduce the same bytes, which fails the moment the two
            // halves disagree about which fields are present.
            let n = #sp(&v);
            let mut buf = vec![0u8; n];
            let mut enc = sv_wire::Encoder::new(&mut buf);
            #ep(&mut enc, &v).expect("partial encode");
            enc.finish().expect("partial size and encode must agree");

            let mut dec = sv_wire::Decoder::new(&buf);
            let back = #dp(&mut dec).expect("partial decode");
            assert_eq!(dec.remaining(), 0, "{} partial decode left bytes behind", #name);

            let n2 = #sp(&back);
            let mut buf2 = vec![0u8; n2];
            let mut enc2 = sv_wire::Encoder::new(&mut buf2);
            #ep(&mut enc2, &back).expect("partial re-encode");
            enc2.finish().unwrap();
            assert_eq!(buf, buf2, "{} partial encoding is not idempotent", #name);
        }
    } else {
        quote!()
    };

    quote! {
        #[test]
        fn #test_name() {
            for seed in 1u64..6 {
                let mut rng = Rng::new(seed.wrapping_mul(0x1234_5678_9ABC_DEF1));
                let v: #ty = #sample(&mut rng);

                let n = #size(&v);
                let mut buf = vec![0u8; n];
                let mut enc = sv_wire::Encoder::new(&mut buf);
                #encode(&mut enc, &v).expect("encode");
                enc.finish().expect("size and encode must agree exactly");

                let mut dec = sv_wire::Decoder::new(&buf);
                let back = #decode(&mut dec).expect("decode");
                assert_eq!(dec.remaining(), 0, "{} decode left bytes behind", #name);
                assert_eq!(v, back, "{} did not survive a round trip", #name);

                #partial_case
            }
        }
    }
}

fn emit_command_sampler(plan: &Plan, c: &crate::plan::CommandPlan) -> TokenStream {
    let args = format_ident!("{}Args", naming::opcode_variant(&c.name));
    let id = format_ident!("sample_args_{}", naming::snake_case(&c.name));
    let all: Vec<&crate::plan::FieldPlan> = c.inputs.iter().chain(c.outputs.iter()).collect();
    let lens = shared_lengths(&all);
    let len_decls = lens.iter().map(|name| {
        let id = len_var(name);
        quote!(let #id = rng.len();)
    });
    let fields = all.iter().map(|f| {
        let fid = naming::ident(&f.rust_name);
        // A command's parameters are not members of a struct, so a `pNext`
        // parameter (there are none in practice) has no parent to draw
        // extenders from.
        let e = sample_expr(plan, &f.shape, "", len_of(&all, f).as_ref());
        quote!(out.#fid = #e;)
    });
    let fixups = all.iter().filter_map(|f| length_fixup(&all, f, &quote!(out)));
    quote! {
        pub fn #id(rng: &mut Rng) -> #args {
            let mut out = <#args as Default>::default();
            #(#len_decls)*
            #(#fields)*
            #(#fixups)*
            let _ = &rng;
            out
        }
    }
}

/// Whether every parameter of this command is encoded in full.
///
/// A callee-filled parameter is encoded with the partial rules — Venus's
/// three-state validity, adopted by design.md §6.1 — which means the scalar
/// fields of, say, `VkPhysicalDeviceProperties` are deliberately *not* on the
/// wire. Comparing the decoded value to what was sampled would then fail for
/// exactly the reason the encoding is correct. Byte idempotence is the check
/// that means something there, and it is applied to every command regardless.
fn all_parameters_are_full(c: &crate::plan::CommandPlan) -> bool {
    c.outputs.iter().all(|f| partial_validity(f) == PartialValidity::Full)
}

fn emit_command_test(c: &crate::plan::CommandPlan) -> TokenStream {
    let args_ty = format_ident!("{}Args", naming::opcode_variant(&c.name));
    let sample = format_ident!("sample_args_{}", naming::snake_case(&c.name));
    let size = format_ident!("size_cmd_{}", naming::snake_case(&c.name));
    let encode = format_ident!("encode_cmd_{}", naming::snake_case(&c.name));
    let decode = format_ident!("decode_cmd_{}", naming::snake_case(&c.name));
    let test_name = format_ident!("roundtrip_cmd_{}", naming::snake_case(&c.name));
    let name = &c.name;
    let opcode = c.opcode;

    let value_equality = if all_parameters_are_full(c) {
        quote!(assert_eq!(a, back, "{} did not survive a round trip", #name);)
    } else {
        quote!(let _ = &a;)
    };

    let reply_case = if c.generates_reply {
        let reply_ty = format_ident!("{}Reply", naming::opcode_variant(&c.name));
        let sr = format_ident!("size_reply_{}", naming::snake_case(&c.name));
        let er = format_ident!("encode_reply_{}", naming::snake_case(&c.name));
        let dr = format_ident!("decode_reply_{}", naming::snake_case(&c.name));
        quote! {
            let r = <#reply_ty as Default>::default();
            let n = #sr(&r);
            let mut buf = vec![0u8; n];
            let mut enc = sv_wire::Encoder::new(&mut buf);
            #er(&mut enc, &r).expect("encode reply");
            enc.finish().expect("reply size and encode must agree");
            let mut dec = sv_wire::Decoder::new(&buf);
            let back = #dr(&mut dec).expect("decode reply");
            assert_eq!(dec.remaining(), 0);
            assert_eq!(r, back, "{} reply did not survive a round trip", #name);
        }
    } else {
        quote!()
    };

    quote! {
        #[test]
        fn #test_name() {
            for seed in 1u64..4 {
                let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
                let a: #args_ty = #sample(&mut rng);

                let n = #size(&a);
                let mut buf = vec![0u8; n];
                let mut enc = sv_wire::Encoder::new(&mut buf);
                #encode(&mut enc, 0, &a).expect("encode command");
                enc.finish().expect("command size and encode must agree");

                // The header the encoder wrote must describe the bytes it wrote.
                let mut hdr = sv_wire::Decoder::new(&buf);
                let header = sv_wire::CommandHeader::decode(&mut hdr).unwrap();
                assert_eq!(header.cmd_type, #opcode);
                assert_eq!(header.cmd_size as usize, n, "{} header length is wrong", #name);

                let mut dec = sv_wire::Decoder::new(&buf[sv_wire::COMMAND_HEADER_SIZE..]);
                let back = #decode(&mut dec).expect("decode command");
                assert_eq!(dec.remaining(), 0, "{} decode left bytes behind", #name);
                #value_equality

                // Re-encoding what was decoded must reproduce the same bytes.
                // This holds whether or not any parameter is encoded partially,
                // and it fails the moment the encode and decode halves disagree
                // about which fields are present.
                let mut buf2 = vec![0u8; #size(&back)];
                let mut enc2 = sv_wire::Encoder::new(&mut buf2);
                #encode(&mut enc2, 0, &back).expect("re-encode command");
                enc2.finish().unwrap();
                assert_eq!(buf, buf2, "{} encoding is not idempotent", #name);

                #reply_case
            }
        }
    }
}

/// A server that answers every command with a default reply.
///
/// Its real value is not what it does but that it exists: implementing the
/// generated `Server` trait for a type is what proves the trait is
/// *implementable*, and it lets `dispatch` and `subdecode` be exercised with no
/// Vulkan driver anywhere near the test — which is the whole point of
/// design.md §13's ordering.
fn emit_test_server(plan: &Plan) -> TokenStream {
    let methods = plan.commands.iter().map(|c| {
        let m = naming::command_method(&c.name);
        let args = format_ident!("{}Args", naming::opcode_variant(&c.name));
        if c.generates_reply {
            let reply = format_ident!("{}Reply", naming::opcode_variant(&c.name));
            quote! {
                fn #m(&mut self, _args: #args) -> sv_wire::Result<#reply> {
                    self.calls += 1;
                    Ok(<#reply as Default>::default())
                }
            }
        } else {
            quote! {
                fn #m(&mut self, _args: #args) -> sv_wire::Result<()> {
                    self.calls += 1;
                    Ok(())
                }
            }
        }
    });

    // One representative command per shape, for the dispatcher test.
    let first_recorded = plan.commands.iter().find(|c| c.is_recorded);
    let dispatch_test = match plan.commands.first() {
        Some(c) => {
            let sample = format_ident!("sample_args_{}", naming::snake_case(&c.name));
            let size = format_ident!("size_cmd_{}", naming::snake_case(&c.name));
            let encode = format_ident!("encode_cmd_{}", naming::snake_case(&c.name));
            let opcode = naming::opcode_variant(&c.name);
            quote! {
                #[test]
                fn dispatch_reaches_the_server() {
                    let mut rng = Rng::new(7);
                    let a = #sample(&mut rng);
                    let mut buf = vec![0u8; #size(&a)];
                    let mut enc = sv_wire::Encoder::new(&mut buf);
                    #encode(&mut enc, sv_wire::CMD_FLAG_GENERATE_REPLY, &a).unwrap();
                    enc.finish().unwrap();

                    let mut srv = TestServer::default();
                    let mut reply = Vec::new();
                    let mut dec = sv_wire::Decoder::new(&buf);
                    let op = dispatch(&mut dec, &mut srv, &mut reply).expect("dispatch");
                    assert_eq!(op, Opcode::#opcode);
                    assert_eq!(srv.calls, 1);
                }

                /// A command whose declared length disagrees with its contents
                /// must be caught at once, not carried forward. This is the R13
                /// detector — the reason design.md §6.2 spends four bytes per
                /// command — and it is worth an explicit test rather than a
                /// comment.
                #[test]
                fn a_wrong_command_length_is_detected_immediately() {
                    let mut rng = Rng::new(11);
                    let a = #sample(&mut rng);
                    let n = #size(&a);
                    let mut buf = vec![0u8; n];
                    let mut enc = sv_wire::Encoder::new(&mut buf);
                    #encode(&mut enc, 0, &a).unwrap();
                    enc.finish().unwrap();

                    // Claim four bytes more than were written.
                    let bad = (n as u32) + 4;
                    buf[8..12].copy_from_slice(&bad.to_le_bytes());
                    let mut srv = TestServer::default();
                    let mut reply = Vec::new();
                    let mut dec = sv_wire::Decoder::new(&buf);
                    assert!(
                        dispatch(&mut dec, &mut srv, &mut reply).is_err(),
                        "a command claiming more bytes than it has must be rejected"
                    );
                }

                #[test]
                fn an_unknown_opcode_is_rejected() {
                    let mut buf = vec![0u8; sv_wire::COMMAND_HEADER_SIZE];
                    let mut enc = sv_wire::Encoder::new(&mut buf);
                    sv_wire::CommandHeader {
                        cmd_type: u32::MAX,
                        cmd_flags: 0,
                        cmd_size: sv_wire::COMMAND_HEADER_SIZE as u32,
                    }
                    .encode(&mut enc)
                    .unwrap();
                    enc.finish().unwrap();
                    let mut srv = TestServer::default();
                    let mut reply = Vec::new();
                    let mut dec = sv_wire::Decoder::new(&buf);
                    assert!(matches!(
                        dispatch(&mut dec, &mut srv, &mut reply),
                        Err(sv_wire::WireError::UnknownCommand { .. })
                    ));
                }
            }
        }
        None => quote!(),
    };

    let subdecode_test = match first_recorded {
        Some(c) => {
            let sample = format_ident!("sample_args_{}", naming::snake_case(&c.name));
            let size = format_ident!("size_cmd_{}", naming::snake_case(&c.name));
            let encode = format_ident!("encode_cmd_{}", naming::snake_case(&c.name));
            quote! {
                /// The restricted sub-decoder accepts a blob of `vkCmd*` and
                /// nothing else (design.md §6.5).
                #[test]
                fn subdecode_accepts_recorded_commands_and_refuses_the_rest() {
                    let mut rng = Rng::new(3);
                    let a = #sample(&mut rng);
                    let n = #size(&a);
                    let mut blob = vec![0u8; n * 3];
                    {
                        let mut enc = sv_wire::Encoder::new(&mut blob);
                        for _ in 0..3 { #encode(&mut enc, 0, &a).unwrap(); }
                        enc.finish().unwrap();
                    }
                    let mut srv = TestServer::default();
                    let mut dec = sv_wire::Decoder::new(&blob);
                    assert_eq!(subdecode(&mut dec, &mut srv).unwrap(), 3);
                    assert_eq!(srv.calls, 3);

                    // An opcode outside the blob subset is refused. The subset
                    // is `is_blob_allowed`, which is the recorded commands plus
                    // the two that frame them — so the sample has to be chosen
                    // against *that*, not against `is_recorded`, or this test
                    // passes while checking the wrong list.
                    let mut bad = blob.clone();
                    let outside = (0..Opcode::COUNT as u32)
                        .filter_map(Opcode::from_u32)
                        .find(|o| !o.is_blob_allowed());
                    if let Some(op) = outside {
                        bad[0..4].copy_from_slice(&(op as u32).to_le_bytes());
                        let mut srv = TestServer::default();
                        let mut dec = sv_wire::Decoder::new(&bad);
                        assert!(subdecode(&mut dec, &mut srv).is_err());
                    }
                }

                /// The blob subset is the `vkCmd*` opcodes **plus the two that
                /// frame them**, and it is exactly those.
                ///
                /// design.md §6.5 says "only the `vkCmd*` opcode subset"; the
                /// blob turned out to have to carry its own
                /// `vkBeginCommandBuffer` and `vkEndCommandBuffer`, because the
                /// GPU side replays a blob when it arrives rather than when the
                /// application recorded it. That widening is a two-entry list
                /// and this is what keeps it a two-entry list.
                #[test]
                fn the_blob_subset_is_the_recorded_commands_plus_exactly_two() {
                    let extra: Vec<&'static str> = (0..Opcode::COUNT as u32)
                        .filter_map(Opcode::from_u32)
                        .filter(|o| o.is_blob_allowed() && !o.is_recorded())
                        .map(|o| o.name())
                        .collect();
                    assert_eq!(
                        extra,
                        vec!["vkBeginCommandBuffer", "vkEndCommandBuffer"],
                        "the restricted sub-decoder has grown a new opcode"
                    );
                }
            }
        }
        None => quote!(),
    };

    quote! {
        #[derive(Default)]
        pub struct TestServer { pub calls: usize }

        impl Server for TestServer {
            #(#methods)*
        }

        #dispatch_test
        #subdecode_test
    }
}
