//! Plan → Rust.
//!
//! Every emitter here is a fold over [`crate::plan::Shape`]. That is the whole
//! trick: because the plan already took the decisions, an emitter is a
//! structural recursion with no registry knowledge in it, which is what makes it
//! possible to read one and believe it.
//!
//! Output goes through `quote` and `prettyplease`, so a generated file that does
//! not parse fails *here*, with the offending token stream, rather than failing
//! `cargo build` in a 100,000-line file with no line numbers worth having.

pub mod dispatch;
pub mod gpu_entry;
pub mod icd_entry;
pub mod passes;
pub mod pnext;
pub mod tests;
pub mod types;
pub mod wire;

use crate::naming;
use crate::plan::{Plan, Scalar, Shape};
use proc_macro2::TokenStream;
use quote::quote;

/// The Rust type a shape is stored as.
///
/// # Why these are owned rather than borrowed
///
/// The obvious alternative is `Cow<'a, [T]>` everywhere, which would let the app
/// side encode straight out of the application's own arrays with no copy. It was
/// considered and rejected for v1, and the reasoning is worth writing down
/// because the project's whole thesis is about not copying:
///
/// The requirement that forbids copying is about the *arena* — vertex data,
/// textures, mapped memory (`requirements-shared-memory.md`). Command parameters
/// are a different thing: they are serialised no matter what, by every design
/// including Venus's, because they have to change representation to cross. What
/// an owned type costs is one extra copy of a *parameter graph* — a
/// `VkBufferCreateInfo`, a list of four queue family indices — between the
/// application's memory and the ring. The largest instance is a SPIR-V module at
/// pipeline-creation time, which is not a hot path.
///
/// What it buys is a lifetime-free generated crate: no `<'a>` on 700 types, no
/// variance puzzles in the `pNext` enum, and a decode side that is the same type
/// as the encode side so a round-trip test can simply compare with `==`. Given
/// that the generated crate is the largest body of code in the project and the
/// one where a mistake is silent corruption, that trade is worth taking once and
/// revisiting with a profile in hand.
pub fn rust_type(shape: &Shape) -> TokenStream {
    match shape {
        Shape::Scalar(s) => scalar_type(s),
        Shape::FixedArray { elem, count, .. } => {
            let inner = rust_type(elem);
            let n = *count as usize;
            quote!([#inner; #n])
        }
        Shape::Optional(inner) => {
            let t = rust_type(inner);
            // Boxed only where it is needed to keep a struct's size sane and to
            // break the type-level cycle a chain would otherwise create.
            if boxes(inner) {
                quote!(Option<Box<#t>>)
            } else {
                quote!(Option<#t>)
            }
        }
        Shape::Array { elem, .. } => {
            let t = rust_type(elem);
            quote!(Vec<#t>)
        }
        // Blobs and strings are both bytes. A string's terminator is part of the
        // *format*, not of the value, so it is never stored.
        Shape::Blob { .. } | Shape::CString => quote!(Vec<u8>),
        Shape::CStringArray { .. } => quote!(Vec<Vec<u8>>),
        Shape::PNext => quote!(Vec<PNextLink>),
    }
}

fn boxes(shape: &Shape) -> bool {
    matches!(shape, Shape::Scalar(Scalar::Struct(_)) | Shape::Scalar(Scalar::Union(_)))
}

pub fn scalar_type(s: &Scalar) -> TokenStream {
    match s {
        Scalar::U8 => quote!(u8),
        Scalar::I8 => quote!(i8),
        Scalar::U16 => quote!(u16),
        Scalar::I16 => quote!(i16),
        Scalar::U32 => quote!(u32),
        Scalar::I32 => quote!(i32),
        // `size_t` is stored as `u64`, not `usize`. The two sides may be
        // different machines and are certainly different processes; a type whose
        // width depends on which side compiled it has no business in a wire
        // struct, and this crate cross-compiles to Windows precisely so that
        // assumption gets tested.
        Scalar::U64 | Scalar::Size => quote!(u64),
        Scalar::I64 => quote!(i64),
        Scalar::F32 => quote!(f32),
        Scalar::F64 => quote!(f64),
        Scalar::Bool32 => quote!(bool),
        Scalar::OpaqueU64(_) => quote!(u64),
        Scalar::Enum(n)
        | Scalar::Bitmask(n)
        | Scalar::Bitmask64(n)
        | Scalar::Handle(n)
        | Scalar::Struct(n)
        | Scalar::Union(n) => {
            let id = naming::type_ident(n);
            quote!(#id)
        }
    }
}

/// A default value expression for a shape.
///
/// Generated rather than derived because `#[derive(Default)]` does not reach
/// `[T; N]` for `N > 32`, and `char deviceName[VK_MAX_PHYSICAL_DEVICE_NAME_SIZE]`
/// is a 256-element array that occurs in a struct every application reads.
pub fn default_expr(shape: &Shape) -> TokenStream {
    match shape {
        Shape::Scalar(s) => match s {
            Scalar::Bool32 => quote!(false),
            Scalar::F32 | Scalar::F64 => quote!(0.0),
            Scalar::U8 | Scalar::I8 | Scalar::U16 | Scalar::I16 => quote!(0),
            Scalar::U32 | Scalar::I32 | Scalar::U64 | Scalar::I64 | Scalar::Size => quote!(0),
            Scalar::OpaqueU64(_) => quote!(0),
            Scalar::Enum(n) | Scalar::Bitmask(n) | Scalar::Bitmask64(n) | Scalar::Handle(n) => {
                let id = naming::type_ident(n);
                quote!(#id(0))
            }
            Scalar::Struct(n) | Scalar::Union(n) => {
                let id = naming::type_ident(n);
                quote!(<#id as Default>::default())
            }
        },
        Shape::FixedArray { elem, count, .. } => {
            let inner = default_expr(elem);
            let n = *count as usize;
            // `[expr; N]` requires the element to be `Copy`, which a struct is
            // not — and `VkPhysicalDeviceMemoryProperties::memoryTypes` is a
            // 32-element array of `VkMemoryType`. `from_fn` has neither the
            // `Copy` bound nor the 32-element ceiling that `#[derive(Default)]`
            // would have hit anyway.
            if matches!(**elem, Shape::Scalar(_)) && !boxes(elem) {
                quote!([#inner; #n])
            } else {
                quote!(core::array::from_fn::<_, #n, _>(|_| #inner))
            }
        }
        Shape::Optional(_) => quote!(None),
        Shape::Array { .. }
        | Shape::Blob { .. }
        | Shape::CString
        | Shape::CStringArray { .. }
        | Shape::PNext => quote!(Vec::new()),
    }
}

/// The full generated crate source, as one module tree.
///
/// One file rather than several because `include!` from `OUT_DIR` is the
/// mechanism, and a single `include!` keeps the crate root honest about
/// `#![forbid(unsafe_code)]` covering all of it.
pub fn emit_all(plan: &Plan) -> TokenStream {
    let types = types::emit(plan);
    let pnext = pnext::emit(plan);
    let wire = wire::emit(plan);
    let passes = passes::emit(plan);
    let dispatch = dispatch::emit(plan);
    let tests = tests::emit(plan);

    quote! {
        #types
        #pnext
        #wire
        #passes
        #dispatch
        #tests
    }
}
