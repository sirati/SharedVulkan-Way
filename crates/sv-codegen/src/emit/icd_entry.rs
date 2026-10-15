//! Pass 10: `sv-icd-entry` — the C ABI, the `extern "system"` stubs and the
//! proc-address tables (docs/design.md §7.2, §7.3, §8).
//!
//! # Why this pass exists at all
//!
//! docs/design.md §7.3 puts it plainly: "The unavoidable `unsafe` is the ICD boundary
//! itself — `extern "system"` functions taking raw pointers from the
//! application. Those live in a separate generated crate, `sv-icd-entry`, and
//! every stub has the identical mechanical shape 'null-check, dereference, call
//! a safe inner function', so the unsafe block is one line and reviewable by
//! pattern."
//!
//! That is the whole design of this file. It emits, per command:
//!
//! ```text
//!   lower_<cmd>   *const C-struct   ──▶ sv_proto::<Cmd>Args      (unsafe, generated)
//!   Icd::<cmd>    sv_proto::<Cmd>Args ──▶ sv_proto::<Cmd>Reply   (safe, hand-written in sv-icd)
//!   store_<cmd>   sv_proto::<Cmd>Reply ──▶ *mut C-struct         (unsafe, generated)
//!   vk<Cmd>       extern "system", three lines, one unsafe block each way
//! ```
//!
//! The middle row is the only one a person writes, and it never sees a raw
//! pointer.
//!
//! # The three invariants every generated `unsafe` block rests on
//!
//! Written once, here, because they are identical in all 250 stubs and R7 asks
//! for stated invariants rather than a per-site argument:
//!
//! 1. **Every pointer the application gives us is either null or valid for
//!    reads of its declared type for the duration of the call.** This is the
//!    Vulkan specification's contract with an implementation and it is not
//!    checkable — no ICD in existence validates it. What *is* checkable is
//!    nullness, and every dereference here is guarded by a null check that
//!    turns a violation into `VK_ERROR_INITIALIZATION_FAILED` rather than a
//!    fault, for the cases where the specification permits null.
//! 2. **Every array read is bounded by the count the registry names**, resolved
//!    at generation time to a concrete sibling field ([`crate::abi::ArrayLen`]).
//!    A `len=` this generator cannot resolve is a generator error, not a guess.
//!    Note that this is *stronger* than the wire needs: docs/design.md §7.4 observes
//!    that our wire's explicit counts demote `len=` to a cross-check, and that
//!    observation stops at this boundary — here `len=` bounds a read of the
//!    application's memory.
//! 3. **A dispatchable handle is a pointer to a [`DispatchableHeader`]**, whose
//!    first word belongs to the loader and whose second is the wire object id.
//!    Every dispatchable handle we ever hand out is allocated by `sv-icd`
//!    through that type, so the read is of our own allocation.
//!
//! # What is *not* generated
//!
//! The commands the profile excludes from the wire — `vkGetInstanceProcAddr`,
//! `vkGetDeviceProcAddr`, the swapchain family, the surface queries — have no
//! `Args` type to lower into, because they are answered on the app side and
//! never cross (docs/design.md §8, §10). They get an entry in the proc tables and a
//! stub that routes to a hand-written method on [`Icd`], or, where `sv-icd` does
//! not implement them yet, a stub that fails cleanly and is listed in
//! `NOT_IMPLEMENTED`.

use crate::abi::{AbiCommand, AbiMember, AbiPlan, AbiType, ArrayLen, Level, primitive_name};
use crate::naming;
use crate::plan::{FieldPlan, Plan, PartialValidity, Scalar, Shape, StructPlan, partial_validity};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::collections::{BTreeMap, BTreeSet};
use vk_registry::{CType, CTypeKind, Model, Primitive, TypeDef};

/// Everything the emitter needs that is not in the two plans.
pub struct Ctx<'a> {
    pub model: &'a Model,
    pub abi: &'a AbiPlan,
    pub wire: &'a Plan,
    /// Types the ABI layer defines, so a name that is not one of them must be a
    /// C primitive.
    defined: BTreeSet<String>,
    /// Structs present in both plans, which are the ones that can be lowered.
    shared: BTreeSet<String>,
    dispatchable: BTreeSet<String>,
    /// Emit lowerings that read **every** member, under the name `lower_full_*`.
    ///
    /// # The correction this field is
    ///
    /// The app side's lowering of a `returnedonly` struct deliberately *skips*
    /// its scalar members and writes zeros, because on the way **out** the
    /// storage behind `pProperties` is the application's uninitialised memory
    /// and reading it is undefined behaviour plus a small information leak
    /// (docs/design.md §6.1's three-state validity, applied at the C boundary).
    ///
    /// The GPU side lowers the identical struct in the identical direction —
    /// C to wire — and for it those members are **the driver's answer**. Reusing
    /// the app side's `lower_*` there produces a `VkPhysicalDeviceProperties`
    /// whose `deviceName` is empty and whose `apiVersion` is zero, which is
    /// exactly what the first run of the generated GPU side reported.
    ///
    /// So "which direction" does not determine the conversion: **the validity
    /// state does**, and it differs between the two sides for the same struct in
    /// the same direction. docs/design.md §7.2's two-by-two is really a two-by-two-by
    /// -two, and that third axis is what this flag selects.
    lower_full: bool,
}

impl<'a> Ctx<'a> {
    pub fn new(model: &'a Model, abi: &'a AbiPlan, wire: &'a Plan) -> Ctx<'a> {
        let defined = abi.types.iter().map(|t| t.name().to_string()).collect();
        let wire_structs: BTreeSet<&str> = wire.structs.iter().map(|s| s.name.as_str()).collect();
        let shared = abi
            .types
            .iter()
            .filter_map(|t| match t {
                AbiType::Struct { name, .. } if wire_structs.contains(name.as_str()) => {
                    Some(name.clone())
                }
                _ => None,
            })
            .collect();
        let dispatchable = abi
            .types
            .iter()
            .filter_map(|t| match t {
                AbiType::Handle { name, dispatchable: true } => Some(name.clone()),
                _ => None,
            })
            .collect();
        Ctx { model, abi, wire, defined, shared, dispatchable, lower_full: false }
    }

    /// The same context emitting `lower_full_*` instead of `lower_*`. See the
    /// note on [`Ctx::lower_full`].
    pub fn full_lowering(&self) -> Ctx<'a> {
        Ctx { lower_full: true, ..self.clone_shallow() }
    }

    fn clone_shallow(&self) -> Ctx<'a> {
        Ctx {
            model: self.model,
            abi: self.abi,
            wire: self.wire,
            defined: self.defined.clone(),
            shared: self.shared.clone(),
            dispatchable: self.dispatchable.clone(),
            lower_full: self.lower_full,
        }
    }

    pub fn is_dispatchable(&self, name: &str) -> bool {
        self.dispatchable.contains(&self.model.canonical_type(name))
    }

    /// Whether a wire struct also exists on the C surface, so a conversion
    /// between the two can be emitted at all.
    pub fn shared_struct(&self, name: &str) -> bool {
        self.shared.contains(name)
    }

    /// Close the shared set under "this struct can be raised", for
    /// [`super::gpu_entry`].
    ///
    /// The same fixpoint [`close`] runs for the app side's two directions, and
    /// for the same reason: a struct whose conversion was refused makes every
    /// struct containing it unconvertible, and emitting a call to a function
    /// that was never emitted is a generated crate that does not compile.
    pub fn close_raise<F>(&self, emit: F) -> (BTreeMap<String, String>, Ctx<'a>)
    where
        F: Fn(&Ctx<'_>, &StructPlan) -> Result<TokenStream, String>,
    {
        close(self, |_| true, emit)
    }

    /// Close the shared set under a caller-chosen "wanted" predicate.
    pub fn close_with<F, W>(&self, wanted: W, emit: F) -> (BTreeMap<String, String>, Ctx<'a>)
    where
        F: Fn(&Ctx<'_>, &StructPlan) -> Result<TokenStream, String>,
        W: Fn(&StructPlan) -> bool,
    {
        close(self, wanted, emit)
    }

    /// Whether this context emits the full lowering.
    pub fn is_full_lowering(&self) -> bool {
        self.lower_full
    }

    /// `lower_<Struct>`, or `lower_full_<Struct>` in a full-lowering context.
    pub fn lower_fn(&self, name: &str) -> proc_macro2::Ident {
        if self.lower_full {
            format_ident!("lower_full_{}", naming::snake_case(name))
        } else {
            format_ident!("lower_{}", naming::snake_case(name))
        }
    }

    /// `lower_pnext`, or `lower_full_pnext`.
    pub fn lower_pnext_fn(&self) -> proc_macro2::Ident {
        if self.lower_full {
            format_ident!("lower_full_pnext")
        } else {
            format_ident!("lower_pnext")
        }
    }

    /// The same context with a narrowed set of lowerable structs.
    fn without(&self, refused: &BTreeMap<String, String>) -> Ctx<'a> {
        Ctx {
            model: self.model,
            abi: self.abi,
            wire: self.wire,
            defined: self.defined.clone(),
            shared: self.shared.iter().filter(|n| !refused.contains_key(*n)).cloned().collect(),
            dispatchable: self.dispatchable.clone(),
            lower_full: self.lower_full,
        }
    }
}

// ---------------------------------------------------------------------------
// C types
// ---------------------------------------------------------------------------

/// A C type as Rust spells it at the ABI.
pub fn c_type(cx: &Ctx<'_>, ty: &CType) -> TokenStream {
    match &ty.kind {
        CTypeKind::Void => quote!(core::ffi::c_void),
        CTypeKind::Named(n) => {
            let canonical = cx.model.canonical_type(n);
            if cx.defined.contains(&canonical) {
                let id = naming::type_ident(&canonical);
                return quote!(#id);
            }
            // Not a type the ABI layer defines, so it is a C primitive.
            let p = match cx.model.types.get(&canonical) {
                Some(TypeDef::Primitive(p)) => *p,
                _ => Primitive::from_c_name(&canonical).unwrap_or(Primitive::U32),
            };
            let t: TokenStream = primitive_name(p).parse().expect("a primitive name parses");
            quote!(#t)
        }
        CTypeKind::Pointer(inner) => {
            let t = c_type(cx, inner);
            if inner.is_const { quote!(*const #t) } else { quote!(*mut #t) }
        }
        CTypeKind::Array(inner, ext) => {
            let t = c_type(cx, inner);
            let n = crate::abi::extent(cx.model, ext).unwrap_or(0) as usize;
            quote!([#t; #n])
        }
    }
}

/// A C type in *parameter* position, where an array is not an array.
///
/// C decays an array parameter to a pointer: `void vkCmdSetBlendConstants(
/// VkCommandBuffer, const float blendConstants[4])` takes a `const float*`, and
/// four floats are **not** pushed by value. Rust's `[f32; 4]` in an
/// `extern "system"` signature is a different calling convention, and the
/// mismatch is silent — the compiler warns about FFI safety and nothing else.
/// Getting this wrong corrupts the argument registers of every call to the
/// affected command.
///
/// Only the outermost level decays; `char deviceName[256]` inside a struct is a
/// genuine array and [`c_type`] handles it.
pub fn c_param_type(cx: &Ctx<'_>, ty: &CType) -> TokenStream {
    match &ty.kind {
        CTypeKind::Array(inner, _) => {
            let t = c_type(cx, inner);
            if inner.is_const { quote!(*const #t) } else { quote!(*mut #t) }
        }
        _ => c_type(cx, ty),
    }
}

// ---------------------------------------------------------------------------
// the type layer
// ---------------------------------------------------------------------------

fn emit_types(cx: &Ctx<'_>) -> TokenStream {
    let items = cx.abi.types.iter().map(|t| emit_type(cx, t));
    let consts = cx.abi.constants.iter().map(|(n, v)| {
        let id = naming::ident(n);
        quote!(pub const #id: u64 = #v;)
    });
    quote! {
        #(#items)*
        /// The registry's numeric API constants.
        pub mod consts {
            #(#consts)*
        }
    }
}

fn emit_type(cx: &Ctx<'_>, t: &AbiType) -> TokenStream {
    match t {
        AbiType::Alias { name, underlying } => {
            let id = naming::type_ident(name);
            let ty = c_type(cx, underlying);
            let doc = format!("`typedef {underlying} {name};`");
            quote! {
                #[doc = #doc]
                pub type #id = #ty;
            }
        }
        AbiType::Opaque { name } => {
            let id = naming::type_ident(name);
            let doc = format!(
                "`{name}`, a type this build only ever sees behind a pointer. An \
                 incomplete type is the whole of its ABI."
            );
            quote! {
                #[doc = #doc]
                #[repr(C)]
                pub struct #id {
                    _opaque: [u8; 0],
                    _not_send: core::marker::PhantomData<*mut u8>,
                }
            }
        }
        AbiType::Handle { name, dispatchable } => emit_handle(name, *dispatchable),
        AbiType::Newtype { name, bit_width, bitflags, values } => {
            emit_newtype(name, *bit_width, *bitflags, values)
        }
        AbiType::Struct { name, is_union, members } => {
            emit_struct(cx, name, *is_union, members)
        }
        AbiType::FuncPointer { name, return_type, params } => {
            let id = naming::type_ident(name);
            let ret = if matches!(return_type.kind, CTypeKind::Void) {
                quote!()
            } else {
                let r = c_type(cx, return_type);
                quote!(-> #r)
            };
            let ps = params.iter().map(|(_, t)| c_type(cx, t));
            let doc = format!("`{name}`");
            quote! {
                #[doc = #doc]
                pub type #id = Option<unsafe extern "system" fn(#(#ps),*) #ret>;
            }
        }
    }
}

fn emit_handle(name: &str, dispatchable: bool) -> TokenStream {
    let id = naming::type_ident(name);
    if dispatchable {
        let opaque = format_ident!("{}_T", name);
        let doc = format!(
            "`{name}`, a **dispatchable** handle.\n\n\
             At the ABI it is a pointer, and the loader requires the first \
             pointer-sized word of what it points at to be its own dispatch table \
             pointer (docs/design.md §6.4, R17). Everything `sv-icd` returns through \
             one of these is a [`crate::DispatchableHeader`] followed by the \
             object's own state; the loader's assertion is debug-only, so getting \
             this wrong is a fault inside the loader rather than an error."
        );
        quote! {
            #[doc = #doc]
            #[repr(C)]
            pub struct #opaque {
                _opaque: [u8; 0],
                _not_send: core::marker::PhantomData<*mut u8>,
            }
            #[doc = #doc]
            pub type #id = *mut #opaque;
        }
    } else {
        let doc = format!(
            "`{name}`, a non-dispatchable handle: a bare 64-bit value, and in this \
             implementation it *is* the wire object id (docs/design.md §6.4, R3). \
             Nothing dereferences it on either side."
        );
        quote! {
            #[doc = #doc]
            pub type #id = u64;
        }
    }
}

fn emit_newtype(name: &str, bit_width: u32, bitflags: bool, values: &[(String, i64)]) -> TokenStream {
    let ty = naming::type_ident(name);
    let repr: TokenStream = if bit_width == 64 {
        quote!(u64)
    } else if bitflags {
        quote!(u32)
    } else {
        quote!(i32)
    };
    let consts = values.iter().map(|(n, v)| {
        let cn = naming::ident(n);
        let lit = if bit_width == 64 {
            let u = *v as u64;
            quote!(#u)
        } else if bitflags {
            let u = *v as u32;
            quote!(#u)
        } else {
            let i = *v as i32;
            quote!(#i)
        };
        quote!(pub const #cn: Self = Self(#lit);)
    });
    let ops = if bitflags {
        quote! {
            impl core::ops::BitOr for #ty {
                type Output = Self;
                fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
            }
            impl core::ops::BitOrAssign for #ty {
                fn bitor_assign(&mut self, rhs: Self) { self.0 |= rhs.0; }
            }
            impl #ty {
                pub fn contains(self, other: Self) -> bool { self.0 & other.0 == other.0 }
            }
        }
    } else {
        quote!()
    };
    let doc = format!(
        "`{name}` at the C ABI.\n\n\
         A `#[repr(transparent)]` newtype rather than a Rust `enum`, for the \
         reason `crate::emit::types` gives for the wire types: an application may \
         legitimately pass a value this build has never heard of, and a Rust \
         `enum` holding an undeclared discriminant is undefined behaviour before \
         we have even looked at it."
    );
    quote! {
        #[doc = #doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        #[repr(transparent)]
        pub struct #ty(pub #repr);
        impl #ty {
            #(#consts)*
        }
        #ops
    }
}

fn emit_struct(cx: &Ctx<'_>, name: &str, is_union: bool, members: &[AbiMember]) -> TokenStream {
    let id = naming::type_ident(name);
    let fields = members.iter().map(|m| {
        let f = naming::ident(&m.rust_name);
        let ty = c_type(cx, &m.ty);
        let doc = format!("`{}` — `{}`", m.name, m.ty);
        quote! {
            #[doc = #doc]
            pub #f: #ty,
        }
    });
    let doc = format!("`{name}`, laid out exactly as the C header does.");
    // `Default` is written rather than derived: `#[derive(Default)]` does not
    // reach `[T; N]` for N > 32, and `char deviceName[256]` is in a struct every
    // application reads. Zero is the correct default for every member of every
    // one of these types — a null pointer, a zero enum, a zero handle.
    let default_impl = quote! {
        impl Default for #id {
            fn default() -> Self {
                // SAFETY: every member is a raw pointer, an integer, a
                // `#[repr(transparent)]` newtype over an integer, a fixed array
                // of those, or a `#[repr(C)]` union of those. All-zero is a
                // valid bit pattern for all of them and none has a niche.
                unsafe { core::mem::zeroed() }
            }
        }
    };
    if is_union {
        quote! {
            #[doc = #doc]
            #[derive(Clone, Copy)]
            #[repr(C)]
            pub union #id {
                #(#fields)*
            }
            #default_impl
        }
    } else {
        quote! {
            #[doc = #doc]
            #[derive(Clone, Copy)]
            #[repr(C)]
            pub struct #id {
                #(#fields)*
            }
            #default_impl
        }
    }
}

// ---------------------------------------------------------------------------
// lowering: C ──▶ wire
// ---------------------------------------------------------------------------

/// The wire type of a shape, qualified with `sv_proto::`.
///
/// [`crate::emit::rust_type`] emits bare names, which is right inside `sv-proto`
/// and wrong here: this crate defines a *different* type of the same name for
/// every one of them. `VkFormat` unqualified in this crate is the C ABI's
/// `VkFormat`, and letting the two be spelled the same way is how a lowering
/// silently type-checks against the wrong side.
fn wire_type(shape: &Shape) -> TokenStream {
    match shape {
        Shape::Scalar(s) => wire_scalar_type(s),
        Shape::FixedArray { elem, count, .. } => {
            let inner = wire_type(elem);
            let n = *count as usize;
            quote!([#inner; #n])
        }
        Shape::Optional(inner) => {
            let t = wire_type(inner);
            if matches!(**inner, Shape::Scalar(Scalar::Struct(_) | Scalar::Union(_))) {
                quote!(Option<Box<#t>>)
            } else {
                quote!(Option<#t>)
            }
        }
        Shape::Array { elem, .. } => {
            let t = wire_type(elem);
            quote!(Vec<#t>)
        }
        Shape::Blob { .. } | Shape::CString => quote!(Vec<u8>),
        Shape::CStringArray { .. } => quote!(Vec<Vec<u8>>),
        Shape::PNext => quote!(Vec<sv_proto::PNextLink>),
    }
}

fn wire_scalar_type(s: &Scalar) -> TokenStream {
    match s {
        Scalar::U8 => quote!(u8),
        Scalar::I8 => quote!(i8),
        Scalar::U16 => quote!(u16),
        Scalar::I16 => quote!(i16),
        Scalar::U32 => quote!(u32),
        Scalar::I32 => quote!(i32),
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
            quote!(sv_proto::#id)
        }
    }
}

/// A default wire value, qualified. The counterpart of
/// [`crate::emit::default_expr`], for the same reason as [`wire_type`].
fn wire_default(shape: &Shape) -> TokenStream {
    match shape {
        Shape::Scalar(s) => match s {
            Scalar::Bool32 => quote!(false),
            Scalar::F32 | Scalar::F64 => quote!(0.0),
            Scalar::U8
            | Scalar::I8
            | Scalar::U16
            | Scalar::I16
            | Scalar::U32
            | Scalar::I32
            | Scalar::U64
            | Scalar::I64
            | Scalar::Size
            | Scalar::OpaqueU64(_) => quote!(0),
            Scalar::Enum(n) | Scalar::Bitmask(n) | Scalar::Bitmask64(n) | Scalar::Handle(n) => {
                let id = naming::type_ident(n);
                quote!(sv_proto::#id(0))
            }
            Scalar::Struct(n) | Scalar::Union(n) => {
                let id = naming::type_ident(n);
                quote!(<sv_proto::#id as Default>::default())
            }
        },
        Shape::FixedArray { elem, count, .. } => {
            let inner = wire_default(elem);
            let n = *count as usize;
            if matches!(**elem, Shape::Scalar(Scalar::Struct(_) | Scalar::Union(_))) {
                quote!(core::array::from_fn::<_, #n, _>(|_| #inner))
            } else if matches!(**elem, Shape::Scalar(_)) {
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

fn lower_struct_fn(name: &str) -> proc_macro2::Ident {
    // Kept for the app side; a full-lowering context goes through `Ctx::lower_fn`.
    format_ident!("lower_{}", naming::snake_case(name))
}
fn lower_out_struct_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("lower_out_{}", naming::snake_case(name))
}
fn store_struct_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("store_{}", naming::snake_case(name))
}

/// Why a command could not be given a mechanical stub.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub command: String,
    pub reason: String,
}

/// Whether a member's C type is one of the `#[repr(transparent)]` newtypes, so
/// that reading it as a number needs a `.0`.
///
/// `VkPipelineMultisampleStateCreateInfo::pSampleMask` has
/// `altlen="(rasterizationSamples + 31) / 32"`, and `rasterizationSamples` is a
/// `VkSampleCountFlagBits` — an enum, not an integer. The registry's length
/// expressions are C, where that distinction does not exist.
fn is_newtype(cx: &Ctx<'_>, ty: &CType) -> bool {
    let Some(base) = ty.base_name() else { return false };
    matches!(
        cx.abi.type_named(&cx.model.canonical_type(base)),
        Some(AbiType::Newtype { .. })
    )
}

/// Build a "read this sibling as a count" accessor from a value accessor.
///
/// Two things have to happen between "the registry named a sibling" and "a
/// number":
///
/// - **A count parameter is often a pointer.** `vkGetPhysicalDeviceQueue-
///   FamilyProperties` has `len="pQueueFamilyPropertyCount"` on its array and
///   `uint32_t* pQueueFamilyPropertyCount` as the parameter, so the sibling is
///   the *address* of the count. Casting that address to a length is not a
///   subtle bug: it asks for a `Vec` of about 2^47 elements, and the first
///   `vulkaninfo` run against this ICD aborted on exactly that. The registry
///   makes no distinction because C dereferences it in the caller's head.
/// - **A count may be an enum.** See [`is_newtype`].
fn count_accessor<'f>(
    cx: &'f Ctx<'_>,
    members: BTreeMap<String, CType>,
    access: Box<dyn Fn(&str) -> TokenStream + 'f>,
) -> impl Fn(&str) -> TokenStream + 'f {
    move |sibling: &str| {
        let a = access(sibling);
        let Some(ty) = members.get(sibling) else { return a };
        if ty.as_pointer().is_some() {
            // A null count with a non-null array is not a call the specification
            // permits, but it costs one comparison to make it a zero-length read
            // rather than a fault.
            return quote!((if (#a).is_null() { 0 } else { *(#a) }));
        }
        if is_newtype(cx, ty) {
            return quote!((#a).0);
        }
        a
    }
}

/// Read the count that bounds an array, as an expression over `base`.
///
/// `base` is the token for the owning value: `(*p)` for a struct member,
/// `params` for a command parameter list (where each parameter is a local).
fn count_expr(len: &ArrayLen, member_access: &dyn Fn(&str) -> TokenStream) -> Option<TokenStream> {
    Some(match len {
        ArrayLen::Sibling(s) => {
            let a = member_access(s);
            quote!((#a) as usize)
        }
        ArrayLen::SiblingDiv { sibling, divisor } => {
            let a = member_access(sibling);
            quote!(((#a) as usize) / (#divisor as usize))
        }
        ArrayLen::SiblingCeilDiv { sibling, add, divisor } => {
            let a = member_access(sibling);
            quote!((((#a) as usize) + (#add as usize)) / (#divisor as usize))
        }
        // The bound is arithmetic the generator already did: nothing to read.
        ArrayLen::Constant(n) => {
            let n = *n as usize;
            quote!(#n)
        }
        // A path length (`pAllocateInfo->commandBufferCount`) is only ever a
        // command parameter, and the parameter accessor handles it.
        ArrayLen::Path { .. } | ArrayLen::Unresolved(_) => return None,
    })
}

/// The lowering expression for one wire field, reading from `access`.
///
/// `access` is an *lvalue expression* of the C type, e.g. `(*p).p_queue_infos`.
fn lower_expr(
    cx: &Ctx<'_>,
    shape: &Shape,
    abi: &AbiMember,
    access: &TokenStream,
    member_access: &dyn Fn(&str) -> TokenStream,
    owner: &str,
) -> Result<TokenStream, String> {
    Ok(match shape {
        Shape::Scalar(s) => lower_scalar(cx, s, access)?,
        Shape::FixedArray { elem, count, .. } => {
            let n = *count as usize;
            let i = fixed_index(elem);
            let inner = lower_expr(cx, elem, abi, &quote!((#access)[#i]), member_access, owner)?;
            let ty = wire_type(elem);
            quote!({
                let mut __v: Vec<#ty> = Vec::with_capacity(#n);
                for #i in 0..#n { __v.push(#inner); }
                match <[#ty; #n]>::try_from(__v) {
                    Ok(a) => a,
                    Err(_) => return Err(crate::LowerError::Shape(#owner)),
                }
            })
        }
        Shape::Optional(inner) => {
            let deref = quote!((*(#access)));
            let e = lower_expr(cx, inner, abi, &deref, member_access, owner)?;
            let boxed = matches!(**inner, Shape::Scalar(Scalar::Struct(_) | Scalar::Union(_)));
            let some = if boxed { quote!(Some(Box::new(#e))) } else { quote!(Some(#e)) };
            quote!(if (#access).is_null() { None } else { #some })
        }
        Shape::Array { elem, .. } => {
            let len = abi
                .len
                .as_ref()
                .ok_or_else(|| format!("{owner}.{}: an array with no resolvable len=", abi.name))?;
            // A length an enclosing struct holds (`exceptions::LEN_FROM_CONTEXT`)
            // is read from the lowering context it published, and a missing
            // one is a refusal of this call, not a guess.
            let n = match (count_expr(len, member_access), crate::exceptions::len_from_context(owner, &abi.name)) {
                (Some(n), _) => n,
                (None, Some(key)) => quote!(crate::lowering_context::get(#key)?),
                (None, None) => {
                    return Err(format!("{owner}.{}: len={len:?} is not a sibling", abi.name));
                }
            };
            // **`stride=`: the application's array may not be tightly packed.**
            //
            // `(#access).add(__i)` strides by `size_of` the element, which is
            // what a C array does and *not* what the registry says here: two
            // parameters carry `stride=`, both multi-draw, and both let the
            // application space its entries by any number of bytes it likes —
            // `VkMultiDrawInfoEXT` interleaved into a larger struct of its own,
            // for instance. Reading them at `size_of` spacing returns the wrong
            // elements and walks off the end of a sparse array.
            //
            // Neither `vk-registry` nor this generator mentioned `stride`
            // before docs/design.md §0.24, which makes it the third
            // load-bearing registry attribute nobody read, after `selector=`
            // (§0.21) and `noautovalidity=` (§0.16). It was latent only because
            // Zink passes `size_of`.
            //
            // The wire form is always tightly packed, so the *stride does not
            // cross*: `gpu_entry` substitutes `size_of` for the driver's copy of
            // the parameter, which is the other half of this fix.
            let step = match abi.stride.as_deref() {
                Some(s) => {
                    let a = member_access(s);
                    Some(quote!((#a) as usize))
                }
                None => None,
            };
            let elem_access = match &step {
                Some(step) => quote!(
                    *((#access).cast::<u8>().add(__i * #step).cast::<_>())
                ),
                None => quote!(*(#access).add(__i)),
            };
            let inner = lower_expr(cx, elem, abi, &elem_access, member_access, owner)?;
            let ty = wire_type(elem);
            // A pointer that a *sibling* decides the validity of. Non-null is
            // not permission to read it — see `exceptions::SIBLING_SELECTED`,
            // which is entirely about `VkWriteDescriptorSet` and cost a GPU
            // hang. The guard comes first so that a `descriptorType` naming a
            // different arm makes this one an empty array without the pointer
            // ever being dereferenced.
            let n = match crate::exceptions::sibling_selected(owner, &abi.name) {
                Some((sel, vals)) => {
                    let s = member_access(sel);
                    let vals = vals.iter().copied();
                    // `member_access` already unwraps a newtyped enum to its
                    // integer, so this is the value and not the wrapper.
                    quote!(if !matches!((#s) as i64, #(#vals)|*) { 0usize } else { #n })
                }
                None => n,
            };
            quote!({
                let __n = if (#access).is_null() { 0usize } else { #n };
                let mut __v: Vec<#ty> = Vec::with_capacity(__n);
                for __i in 0..__n { __v.push(#inner); }
                __v
            })
        }
        Shape::Blob { .. } => {
            let len = abi
                .len
                .as_ref()
                .ok_or_else(|| format!("{owner}.{}: a blob with no resolvable len=", abi.name))?;
            let n = count_expr(len, member_access)
                .ok_or_else(|| format!("{owner}.{}: len={len:?} is not a sibling", abi.name))?;
            quote!({
                let __n = if (#access).is_null() { 0usize } else { #n };
                if __n == 0 {
                    Vec::new()
                } else {
                    core::slice::from_raw_parts((#access) as *const u8, __n).to_vec()
                }
            })
        }
        Shape::CString => quote!(crate::c_string_bytes(#access as *const core::ffi::c_char)),
        Shape::CStringArray { .. } => {
            let len = abi.len.as_ref().ok_or_else(|| {
                format!("{owner}.{}: a string array with no resolvable len=", abi.name)
            })?;
            let n = count_expr(len, member_access)
                .ok_or_else(|| format!("{owner}.{}: len={len:?} is not a sibling", abi.name))?;
            quote!({
                let __n = if (#access).is_null() { 0usize } else { #n };
                let mut __v: Vec<Vec<u8>> = Vec::with_capacity(__n);
                for __i in 0..__n {
                    __v.push(crate::c_string_bytes(
                        *(#access).add(__i) as *const core::ffi::c_char,
                    ));
                }
                __v
            })
        }
        Shape::PNext => {
            let allowed = crate::emit::pnext::allowed_fn_ident(owner);
            let walker = cx.lower_pnext_fn();
            quote!(#walker(#access as *const core::ffi::c_void, sv_proto::#allowed)?)
        }
    })
}

fn lower_scalar(cx: &Ctx<'_>, s: &Scalar, access: &TokenStream) -> Result<TokenStream, String> {
    Ok(match s {
        Scalar::U8 | Scalar::U16 | Scalar::U32 | Scalar::U64 => quote!((#access) as _),
        Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64 => quote!((#access) as _),
        Scalar::F32 | Scalar::F64 => quote!(#access),
        Scalar::Size => quote!((#access) as u64),
        Scalar::Bool32 => quote!((#access) != 0),
        // A capture-replay member is **refused by name if it is not null**,
        // rather than carried. docs/design.md §0.24: we do not advertise
        // `…CaptureReplay` in any feature struct, so the specification requires
        // the application to leave these null, and a non-null one is a usage
        // error rather than data we lost. Refusing it loudly is the difference
        // between "we do not support capture-replay" and "we silently ignored
        // your replay token and gave you a different address".
        Scalar::OpaqueU64(crate::exceptions::VoidPolicy::CaptureReplayUnsupported) => {
            quote!({
                if !(#access).is_null() {
                    return Err(crate::LowerError::CaptureReplayUnsupported);
                }
                0u64
            })
        }
        Scalar::OpaqueU64(_) => quote!((#access) as usize as u64),
        Scalar::Enum(n) | Scalar::Bitmask(n) | Scalar::Bitmask64(n) => {
            let t = naming::type_ident(n);
            quote!(sv_proto::#t((#access).0))
        }
        Scalar::Handle(n) => {
            let t = naming::type_ident(n);
            if cx.is_dispatchable(n) {
                if cx.lower_full {
                    // A dispatchable handle the *driver* returned is the
                    // driver's own pointer, and `dispatchable_id` reads a
                    // `DispatchableHeader` only `sv-icd` ever writes. Reading one
                    // out of a driver's object would be a wild dereference, so
                    // this is refused rather than converted; the commands that
                    // return such a handle are handled by name in pass 12, which
                    // binds the app side's id to it instead.
                    return Err(format!(
                        "a dispatchable {n} inside a struct cannot be lowered on the GPU side"
                    ));
                }
                quote!(sv_proto::#t(crate::dispatchable_id(#access as *const core::ffi::c_void)))
            } else {
                quote!(sv_proto::#t(#access))
            }
        }
        Scalar::Struct(n) => {
            if !cx.shared.contains(n) {
                return Err(format!("{n} has no wire counterpart"));
            }
            let f = cx.lower_fn(n);
            quote!(#f(&(#access))?)
        }
        Scalar::Union(n) => {
            if !cx.shared.contains(n) {
                return Err(format!("{n} has no wire counterpart"));
            }
            // **A union whose arm changes what the bytes mean cannot be reached
            // from here**, because here is the generic path and it has no
            // sibling to read. `emit_lower_struct` handles the selector case, so
            // arriving in this arm means the union is a member of something that
            // does not annotate it — a registry gap — and the honest answer is
            // to refuse the whole command by name rather than take member [0]
            // and hope. See `union_needs_selector`.
            if union_needs_selector(cx, n) {
                return Err(format!(
                    "{n} is a union carrying a handle or a (memory, offset) pair, so which arm \
                     is live decides what crosses — and it is being lowered somewhere with no \
                     `selector=` sibling in scope. gfxstream's member-[0] rule is not safe here \
                     (docs/design.md §0.21)"
                ));
            }
            let f = cx.lower_fn(n);
            quote!(#f(&(#access))?)
        }
    })
}

/// Whether reading the wrong arm of this union would be a *semantic* error
/// rather than a renaming.
///
/// gfxstream's rule — always marshal member [0] — is safe exactly when every arm
/// is plain bytes of the same extent, because the peer then reconstructs the
/// bytes the application wrote and only the *name* is lost. `VkClearValue` and
/// `VkPerformanceCounterResultKHR` are that shape and stay on the fast path.
///
/// It stops being safe the moment an arm holds something the boundary has to
/// *translate*: an object id that the handle-map pass must turn into a driver
/// handle, or a `(memory, offset)` pair the transform pass must rebase. Reading
/// the wrong arm then does not mis-name the bytes, it hands the driver a
/// descriptor built from a number that was never a handle — docs/design.md §0.16
/// item 2's hazard, which cost this project a GPU hang the first time it was met
/// one level in.
fn union_needs_selector(cx: &Ctx<'_>, name: &str) -> bool {
    cx.wire
        .struct_plan(name)
        .is_some_and(|s| s.is_union && (s.has_handles || s.has_device_memory))
}

/// `lower_<Struct>(p: *const abi::Struct) -> Result<sv_proto::Struct>`.
pub fn emit_lower_struct(cx: &Ctx<'_>, s: &StructPlan) -> Result<TokenStream, String> {
    let name = &s.name;
    let abi_members: BTreeMap<&str, &AbiMember> = match cx.abi.type_named(name) {
        Some(AbiType::Struct { members, .. }) => {
            members.iter().map(|m| (m.name.as_str(), m)).collect()
        }
        _ => return Err(format!("{name} is not a struct on the C surface")),
    };
    let f = cx.lower_fn(name);
    let ty = naming::type_ident(name);
    let member_access = |sibling: &str| {
        let id = naming::ident(&naming::snake_case(sibling));
        quote!((*p).#id)
    };
    let member_types: BTreeMap<String, CType> =
        abi_members.iter().map(|(n, m)| ((*n).to_string(), m.ty.clone())).collect();
    let count_access = count_accessor(
        cx,
        member_types,
        Box::new(|s: &str| {
            let id = naming::ident(&naming::snake_case(s));
            quote!((*p).#id)
        }),
    );

    if s.is_union && union_needs_selector(cx, name) {
        // **The selector-driven form, and it takes the discriminator as a
        // parameter so that it cannot be called without one.**
        //
        // The registry states both halves of this and nothing read either: the
        // enclosing struct's member carries `selector="geometryType"`, and each
        // arm of the union carries `selection="VK_GEOMETRY_TYPE_TRIANGLES_KHR"`
        // — ten annotations across the whole of `vk.xml`, parsed into
        // `Member::selector` and `Member::selection` since the registry crate
        // was written. `exceptions`' header concluded that unions needed no
        // exception table because "a Rust `enum` *is* the tag", which is true of
        // the wire and **stops at the C ABI**, where there is no tag and this
        // function is the thing that has to invent one.
        //
        // Making the selector an argument rather than something read from an
        // enclosing pointer is what makes the property structural: there is no
        // way to emit a call to this function that has not gone and found the
        // sibling. A caller that cannot is a compile error in the generator, not
        // a wrong answer at run time.
        let mut arms = Vec::new();
        for field in &s.fields {
            if field.selection.is_empty() {
                return Err(format!(
                    "{name}.{}: a union arm carrying a handle or memory pair with no \
                     `selection=` in the registry — there is nothing to select it by, and \
                     guessing is what docs/design.md §0.21 exists to stop",
                    field.name
                ));
            }
            let variant = naming::union_variant(&field.name);
            let abi = abi_members
                .get(field.name.as_str())
                .ok_or_else(|| format!("{name}.{}: no C member", field.name))?;
            let access = member_access(&field.name);
            let e = lower_expr(cx, &field.shape, abi, &access, &count_access, name)?;
            // Resolved to numbers here rather than emitted as associated
            // constants, because the C-layer enums are newtypes and a `match`
            // pattern over an associated const depends on structural-equality
            // rules the generator should not have an opinion about.
            let mut pats = Vec::new();
            for c in &field.selection {
                let v = cx
                    .wire
                    .enums
                    .iter()
                    .find_map(|e| e.values.iter().find(|(n, _)| n == c).map(|(_, v)| *v))
                    .ok_or_else(|| {
                        format!("{name}.{}: selection={c} is not a value of any enum in this profile", field.name)
                    })?;
                let v = v as i32;
                pats.push(quote!(#v));
            }
            arms.push(quote! {
                #(#pats)|* => sv_proto::#ty::#variant(#e),
            });
        }
        let cname = name.to_string();
        let doc = format!(
            "Lower a C `{name}`, choosing the arm its `selector=` sibling names.\n\n\
             The discriminator is a parameter rather than something read out of \
             an enclosing pointer, so a caller that has not found the sibling \
             cannot compile. An unrecognised value refuses the call by name \
             instead of falling back to a member (docs/design.md §0.21)."
        );
        return Ok(quote! {
            #[doc = #doc]
            #[allow(unused_unsafe, unused_variables)]
            pub unsafe fn #f(
                p: *const #ty,
                __sel: i32,
            ) -> crate::LowerResult<sv_proto::#ty> {
                unsafe {
                    Ok(match __sel {
                        #(#arms)*
                        _ => return Err(crate::LowerError::Shape(#cname)),
                    })
                }
            }
        });
    }

    if s.is_union {
        // docs/design.md §7.4: a C union carries no tag and only ten `selector=`
        // annotations exist in the whole registry. gfxstream always marshals
        // member [0]; Venus hardcodes three discriminators and refuses the rest.
        // We take gfxstream's rule *and* check the thing that makes it safe: the
        // generator refuses a union whose member [0] is not the one every other
        // member fits inside, so the bytes an application wrote are never
        // truncated, only reinterpreted.
        //
        // **It is reached only for a union that is plain bytes**
        // (`union_needs_selector` above): every arm the same storage, nothing
        // the boundary has to translate, so reinterpreting loses the name and
        // nothing else.
        let first = s
            .fields
            .first()
            .ok_or_else(|| format!("{name}: a union with no members"))?;
        let variant = naming::union_variant(&first.name);
        let abi = abi_members
            .get(first.name.as_str())
            .ok_or_else(|| format!("{name}.{}: no C member", first.name))?;
        let access = member_access(&first.name);
        let e = lower_expr(cx, &first.shape, abi, &access, &count_access, name)?;
        let doc = format!(
            "Lower a C `{name}`.\n\n\
             A C union has no tag, so member `{}` is read — gfxstream's rule \
             (docs/design.md §7.4). Every member of this union occupies the same \
             storage and the encoding is bit-preserving, so what the peer \
             reconstructs is the bytes the application wrote; what is lost is \
             only the *name* the application used for them.",
            first.name
        );
        return Ok(quote! {
            #[doc = #doc]
            #[allow(unused_unsafe, unused_variables)]
            pub unsafe fn #f(p: *const #ty) -> crate::LowerResult<sv_proto::#ty> {
                unsafe { Ok(sv_proto::#ty::#variant(#e)) }
            }
        });
    }

    let mut inits = Vec::new();
    for field in &s.fields {
        let id = naming::ident(&field.rust_name);
        let abi = abi_members
            .get(field.name.as_str())
            .ok_or_else(|| format!("{name}.{}: no C member", field.name))?;
        // A `returnedonly` struct is filled in by the implementation, so its
        // contents on the way *out* are whatever the caller's uninitialised
        // memory held. Venus's three-state validity, applied at the C boundary
        // where it matters most: reading those bytes would be reading
        // uninitialised memory, which is undefined behaviour and a small
        // information leak besides (docs/design.md §6.1).
        // `returnedonly` means "the *implementation* fills this in". On the app
        // side we are the caller, so those members are uninitialised on the way
        // out and must not be read. On the GPU side we are the implementation's
        // proxy and those same members are the driver's answer — the only part
        // of the struct that carries information. See [`Ctx::lower_full`].
        let validity = if s.returned_only
            && !cx.lower_full
            && !crate::exceptions::returned_only_read_as_input(name)
        {
            partial_validity(field)
        } else {
            PartialValidity::Full
        };
        let access = member_access(&field.name);
        let e = match validity {
            // **An output chain gives up its tags and nothing else.** `Partial`
            // on a `pNext` means "the links are there, their contents are the
            // caller's uninitialised memory" — which the member-by-member rules
            // above already say for every other field and the chain walk did
            // not (docs/design.md §0.24).
            PartialValidity::Partial if matches!(field.shape, Shape::PNext) => {
                let allowed = crate::emit::pnext::allowed_fn_ident(name);
                quote!(lower_pnext_tags(
                    #access as *const core::ffi::c_void,
                    sv_proto::#allowed,
                )?)
            }
            PartialValidity::Skip => wire_default(&field.shape),
            // **The `selector=` call site.** This is the only place in the
            // generator that has both the union and the sibling that decides
            // it, so it is the only place that can call the selector-driven
            // lowering — which is why that function takes the discriminator as
            // an argument rather than reaching for it.
            _ => match (&field.selector, &field.shape) {
                (Some(sel), Shape::Scalar(Scalar::Union(u))) if union_needs_selector(cx, u) => {
                    if !s.fields.iter().any(|f| &f.name == sel) {
                        return Err(format!(
                            "{name}.{}: selector={sel} names no sibling of this struct",
                            field.name
                        ));
                    }
                    let f = cx.lower_fn(u);
                    let sib = member_access(sel);
                    quote!(#f(&(#access), (#sib).0 as i32)?)
                }
                _ => lower_expr(cx, &field.shape, abi, &access, &count_access, name)?,
            },
        };
        inits.push(quote!(#id: #e,));
    }
    let doc = format!("Lower a C `{name}` into its wire form.");
    // A struct that holds a length its descendants need publishes it for the
    // duration of its own lowering (`exceptions::LEN_CONTEXT_PROVIDERS`).
    let body = quote!(unsafe { Ok(sv_proto::#ty { #(#inits)* }) });
    let body = match crate::exceptions::len_context_provider(name) {
        Some((member, key, add)) if !cx.lower_full => {
            let access = member_access(member);
            quote!({
                let __len = unsafe { (#access) as usize } + #add;
                crate::lowering_context::with(#key, __len, || #body)
            })
        }
        _ => body,
    };
    Ok(quote! {
        #[doc = #doc]
        #[allow(unused_unsafe, unused_variables)]
        pub unsafe fn #f(p: *const #ty) -> crate::LowerResult<sv_proto::#ty> {
            let _ = p;
            #body
        }
    })
}

// ---------------------------------------------------------------------------
// writeback: wire ──▶ C
// ---------------------------------------------------------------------------

/// Statements that write one wire value into C storage at `dst` (an lvalue).
fn store_stmt(
    cx: &Ctx<'_>,
    shape: &Shape,
    src: &TokenStream,
    dst: &TokenStream,
    owner: &str,
) -> Result<TokenStream, String> {
    Ok(match shape {
        Shape::Scalar(s) => store_scalar(cx, s, src, dst)?,
        Shape::FixedArray { elem, count, .. } => {
            let n = *count as usize;
            let i = fixed_index(elem);
            let inner = store_stmt(
                cx,
                elem,
                &quote!((#src)[#i]),
                &quote!((#dst)[#i]),
                owner,
            )?;
            quote!(for #i in 0..#n { #inner })
        }
        // A returned struct whose members are pointers to caller storage cannot
        // be filled in without knowing the caller's capacity, and no struct on
        // this surface does it. Refusing is better than writing through a
        // pointer we did not bound.
        Shape::Optional(_)
        | Shape::Array { .. }
        | Shape::Blob { .. }
        | Shape::CString
        | Shape::CStringArray { .. } => {
            return Err(format!(
                "{owner}: a pointer-valued member cannot be written back into \
                 caller storage of unknown capacity"
            ));
        }
        Shape::PNext => quote!(store_pnext(&(#src), (#dst) as *mut core::ffi::c_void);),
    })
}

fn store_scalar(
    cx: &Ctx<'_>,
    s: &Scalar,
    src: &TokenStream,
    dst: &TokenStream,
) -> Result<TokenStream, String> {
    Ok(match s {
        Scalar::U8
        | Scalar::I8
        | Scalar::U16
        | Scalar::I16
        | Scalar::U32
        | Scalar::I32
        | Scalar::U64
        | Scalar::I64
        | Scalar::Size => quote!(#dst = (#src) as _;),
        Scalar::F32 | Scalar::F64 => quote!(#dst = #src;),
        Scalar::Bool32 => quote!(#dst = if #src { 1 } else { 0 };),
        Scalar::OpaqueU64(_) => quote!(#dst = (#src) as usize as _;),
        Scalar::Enum(n) | Scalar::Bitmask(n) | Scalar::Bitmask64(n) => {
            let t = naming::type_ident(n);
            quote!(#dst = #t((#src).0);)
        }
        Scalar::Handle(n) => {
            if cx.is_dispatchable(n) {
                // A dispatchable handle cannot be reconstructed from an id: it is
                // a pointer to an object only `sv-icd` can allocate. Commands
                // that return one are handled by name, not by this fold.
                return Err(format!("a dispatchable {n} cannot be written back generically"));
            }
            quote!(#dst = (#src).0;)
        }
        Scalar::Struct(n) | Scalar::Union(n) => {
            if !cx.shared.contains(n) {
                return Err(format!("{n} has no wire counterpart"));
            }
            let f = store_struct_fn(n);
            quote!(#f(&(#src), &raw mut (#dst));)
        }
    })
}

fn emit_store_struct(cx: &Ctx<'_>, s: &StructPlan) -> Result<TokenStream, String> {
    let name = &s.name;
    if s.is_union {
        return Err(format!("{name}: a union is never written back"));
    }
    let f = store_struct_fn(name);
    let ty = naming::type_ident(name);
    let mut stmts = Vec::new();
    for field in &s.fields {
        let id = naming::ident(&field.rust_name);
        let stmt = store_stmt(
            cx,
            &field.shape,
            &quote!(src.#id),
            &quote!((*dst).#id),
            name,
        )?;
        stmts.push(stmt);
    }
    let doc = format!(
        "Write a wire `{name}` back into the application's storage.\n\n\
         `sType` is deliberately not written: the application set it, it is how \
         the application recognises its own chain link, and overwriting it with \
         a value derived from the peer's reply would be the one way this \
         direction could corrupt a caller."
    );
    Ok(quote! {
        #[doc = #doc]
        #[allow(unused_unsafe, unused_variables)]
        pub unsafe fn #f(src: &sv_proto::#ty, dst: *mut #ty) {
            unsafe { #(#stmts)* }
        }
    })
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn stub_ident(name: &str) -> proc_macro2::Ident {
    naming::fn_ident("sv", name)
}

/// Entry points `sv-icd-entry` writes by hand instead of generating.
///
/// All but the last two are here for one reason, and it is a structural one
/// rather than a shortcut: **a dispatchable handle cannot be generated.** What
/// the application gets back from `vkCreateInstance` is a pointer to an
/// allocation whose first word the loader owns (docs/design.md §6.4), and no fold
/// over a wire reply can mint one — the reply carries a `u64` id, and turning an
/// id into a live object is the ICD's own bookkeeping. Every command that
/// *returns* a dispatchable object, or destroys one, is therefore hand-written,
/// and the list is short because Vulkan has only five dispatchable types.
///
/// The rest are here because design.md says they are answered app-side:
/// proc-address lookup (§6.1: its answer is a pointer), the extension
/// enumerations (§8: "extension filtering, not mirroring" — we advertise a
/// curated set, not the 148 the GPU side reports, R16), and
/// `vkMapMemory`/`vkUnmapMemory` (§5.3: "pure app-side bookkeeping", no round
/// trip and no copy — the whole point of the arena).
pub const HAND_WRITTEN: &[&str] = &[
    "vkGetInstanceProcAddr",
    "vkGetDeviceProcAddr",
    "vkCreateInstance",
    "vkDestroyInstance",
    "vkEnumeratePhysicalDevices",
    "vkCreateDevice",
    "vkDestroyDevice",
    "vkGetDeviceQueue",
    "vkAllocateCommandBuffers",
    "vkFreeCommandBuffers",
    "vkEnumerateInstanceExtensionProperties",
    "vkEnumerateInstanceLayerProperties",
    "vkEnumerateInstanceVersion",
    "vkEnumerateDeviceExtensionProperties",
    "vkEnumerateDeviceLayerProperties",
    "vkMapMemory",
    "vkUnmapMemory",
    // Core 1.4's promotion of the same two, and they are here for the same
    // reason and one sharper one. `ppData` is `VoidPolicy::NotSerialisable`,
    // which drops it from the wire — right for a command answered app-side,
    // and *silently wrong* for a generated one, because the stub would forward
    // the info struct and return `VK_SUCCESS` having never written `*ppData`.
    // A void policy states what a member means; only this list states which
    // side answers. See `crate::local::sv_vk_map_memory2`.
    "vkMapMemory2",
    "vkUnmapMemory2",
    // docs/design.md §10. Presentation is the ICD's, entirely: a swapchain image is
    // a GPU-side `VkImage` the app side created out of ordinary forwarded
    // commands, and `vkQueuePresentKHR` is a copy into the arena followed by
    // whatever the app side's display target does with the pixels. None of
    // these commands has a wire opcode, so none of them can be a fold over a
    // reply; and their C parameters are `VkIcdSurfaceXXX` pointers the registry
    // does not describe at all.
    "vkDestroySurfaceKHR",
    "vkCreateHeadlessSurfaceEXT",
    "vkCreateXlibSurfaceKHR",
    "vkCreateXcbSurfaceKHR",
    "vkCreateWaylandSurfaceKHR",
    "vkGetPhysicalDeviceSurfaceSupportKHR",
    "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
    "vkGetPhysicalDeviceSurfaceFormatsKHR",
    "vkGetPhysicalDeviceSurfacePresentModesKHR",
    "vkGetPhysicalDeviceXlibPresentationSupportKHR",
    "vkGetPhysicalDeviceXcbPresentationSupportKHR",
    "vkGetPhysicalDeviceWaylandPresentationSupportKHR",
    "vkCreateSwapchainKHR",
    "vkDestroySwapchainKHR",
    "vkGetSwapchainImagesKHR",
    "vkAcquireNextImageKHR",
    "vkQueuePresentKHR",
    // **`VK_EXT_debug_utils`'s messenger, which the loader reaches whatever we
    // advertise.** These three were generated refusals — the generator has
    // nothing to forward them to, because a messenger is an app-side object
    // holding an app-side function pointer, and a `PFN_` cannot cross a process
    // boundary (docs/design.md §6.1's exclusion list). "Cannot be forwarded" was
    // then treated as "cannot be implemented", which is a different claim: it is
    // answered *here*, like the swapchain above it.
    //
    // Refusing cost the browser. Zink creates a messenger while building its
    // screen and takes the failure as fatal, so `vkCreateDebugUtilsMessengerEXT`
    // returning `VK_ERROR_INITIALIZATION_FAILED` meant no physical device, no
    // screen, no GL, and Firefox exiting at once (docs/design.md §0.22).
    // `crate::local` implements all three for real: the object is stored,
    // destroyed on request, and `vkSubmitDebugUtilsMessageEXT` delivers to it.
    "vkCreateDebugUtilsMessengerEXT",
    "vkDestroyDebugUtilsMessengerEXT",
    "vkSubmitDebugUtilsMessageEXT",
];

fn emit_command(
    lower_cx: &Ctx<'_>,
    store_cx: &Ctx<'_>,
    c: &AbiCommand,
) -> (TokenStream, Option<Refusal>) {
    let cx = lower_cx;
    let stub = stub_ident(&c.name);
    // A parameter the wire command does not name is one the protocol strips —
    // `pAllocator`, always (docs/design.md §7.4). It is still in the C signature and
    // must still occupy its argument slot; it is simply never read.
    let used: BTreeSet<&str> = lower_cx
        .wire
        .commands
        .iter()
        .find(|w| w.name == c.name)
        .map(|w| {
            w.inputs.iter().chain(w.outputs.iter()).map(|f| f.name.as_str()).collect()
        })
        .unwrap_or_default();
    let idents: Vec<proc_macro2::Ident> = c
        .params
        .iter()
        .map(|p| {
            if used.contains(p.name.as_str()) || used.is_empty() {
                naming::ident(&p.rust_name)
            } else {
                naming::ident(&format!("_{}", p.rust_name))
            }
        })
        .collect();
    let params: Vec<TokenStream> = c
        .params
        .iter()
        .zip(&idents)
        .map(|(p, id)| {
            let ty = c_param_type(cx, &p.ty);
            quote!(#id: #ty)
        })
        .collect();
    let ret = if c.returns_void {
        quote!()
    } else {
        let r = c_type(cx, &c.return_type);
        quote!(-> #r)
    };

    let body = match stub_body(lower_cx, store_cx, c) {
        Ok(b) => b,
        Err(reason) => {
            let refusal = Refusal { command: c.name.clone(), reason: reason.clone() };
            let doc = format!(
                "**Not implemented.** {reason}\n\nThe entry point exists so the \
                 loader's tables are complete and an application gets a clean \
                 failure rather than a null function pointer."
            );
            let unused = idents.iter().map(|id| quote!(let _ = #id;));
            let fail = if c.returns_void {
                quote!()
            } else if c.returns_result {
                quote!(VkResult::VK_ERROR_INITIALIZATION_FAILED)
            } else {
                quote!(Default::default())
            };
            let cname = c.name.as_str();
            let f = quote! {
                #[doc = #doc]
                #[allow(unused_unsafe)]
                pub unsafe extern "system" fn #stub(#(#params),*) #ret {
                    #(#unused)*
                    crate::not_implemented(#cname);
                    #fail
                }
            };
            return (f, Some(refusal));
        }
    };

    let doc = format!("`{}`", c.name);
    (
        quote! {
            #[doc = #doc]
            #[allow(unused_unsafe)]
            pub unsafe extern "system" fn #stub(#(#params),*) #ret {
                #body
            }
        },
        None,
    )
}

fn stub_body(
    lower_cx: &Ctx<'_>,
    store_cx: &Ctx<'_>,
    c: &AbiCommand,
) -> Result<TokenStream, String> {
    let cx = lower_cx;
    if !c.forwarded {
        return Err(format!(
            "`{}` is answered on the app side rather than forwarded (docs/design.md §8, \
             §10), and `sv-icd` does not implement it yet",
            c.name
        ));
    }
    let wire = cx
        .wire
        .commands
        .iter()
        .find(|w| w.name == c.name)
        .ok_or_else(|| format!("{} has no wire command", c.name))?;

    let abi_by_name: BTreeMap<&str, &AbiMember> =
        c.params.iter().map(|p| (p.name.as_str(), p)).collect();

    // Command parameters are locals, and a path length names another parameter's
    // member.
    let member_access = |name: &str| {
        let id = naming::ident(&naming::snake_case(name));
        quote!(#id)
    };
    let path_access = |root: &str, field: &str| {
        let r = naming::ident(&naming::snake_case(root));
        let f = naming::ident(&naming::snake_case(field));
        quote!((*#r).#f)
    };
    let param_types: BTreeMap<String, CType> =
        c.params.iter().map(|p| (p.name.clone(), p.ty.clone())).collect();
    let count_access = count_accessor(
        cx,
        param_types,
        Box::new(|s: &str| {
            let id = naming::ident(&naming::snake_case(s));
            quote!(#id)
        }),
    );

    // --- lower ------------------------------------------------------------
    let mut inits = Vec::new();
    for field in wire.inputs.iter().chain(wire.outputs.iter()) {
        let id = naming::ident(&field.rust_name);
        let abi = abi_by_name
            .get(field.name.as_str())
            .ok_or_else(|| format!("{}.{}: no C parameter", c.name, field.name))?;
        let access = member_access(&field.name);

        // Output parameters follow Venus's three-state validity, and the reason
        // is sharper here than on the wire: the storage behind `pBuffer` is
        // uninitialised when the application calls us, so reading it is
        // undefined behaviour. What *is* meaningful on an output parameter is
        // its count — `*pPhysicalDeviceCount` is the caller's capacity, an input
        // in everything but constness — and that is exactly the case
        // `partial_validity` marks `Full`.
        let validity =
            if field.is_output { partial_validity(field) } else { PartialValidity::Full };

        let e = match (&field.shape, validity) {
            // An output **blob** — a sized `void*` — carries the caller's
            // capacity and no values, and it has to be matched here, *above*
            // the `Skip` arm, because `partial_validity` calls a blob `Skip`
            // and `Skip` means "send nothing at all".
            //
            // Sending nothing is right about the *contents* — the caller's
            // buffer is uninitialised memory and reading it is undefined — and
            // wrong about the length, which is the one thing an output
            // parameter genuinely carries inwards. The GPU side sizes the
            // buffer it hands the driver from the length of the array it
            // received, so a zero-length blob became a **null `pData` with a
            // non-zero size**, and the driver did what a driver does with a
            // null output pointer: nothing.
            //
            // Two commands on this surface. `vkGetPipelineCacheData` is the one
            // that was observed: it returned `VK_INCOMPLETE` on every run,
            // which Zink prints and survives, and which is what led here. The
            // other is `vkGetQueryPoolResults`, and it is the reason this is
            // worth fixing rather than silencing — there the same path returns
            // `VK_SUCCESS` with the caller's buffer *untouched*, so a query
            // reads whatever was already in that memory and nothing anywhere
            // reports a problem. Zink does not take that path for occlusion
            // queries (it copies results into a buffer with
            // `vkCmdCopyQueryPoolResults` and reads the buffer), so it had not
            // fired; an application that calls `vkGetQueryPoolResults`
            // directly, as the specification entirely permits, would have got
            // silent nonsense.
            //
            // It is docs/design.md §0.4 item 5's mistake in the other direction:
            // there the *count* was derived from the array's length on the way
            // back, here the array's length is dropped on the way out.
            (Shape::Blob { .. }, _) if field.is_output => {
                let len = abi.len.as_ref().ok_or_else(|| {
                    format!("{}.{}: an output blob with no len=", c.name, field.name)
                })?;
                let n = match len {
                    ArrayLen::Path { root, field } => {
                        let a = path_access(root, field);
                        let r = naming::ident(&naming::snake_case(root));
                        quote!(if #r.is_null() { 0usize } else { (#a) as usize })
                    }
                    other => count_expr(other, &count_access).ok_or_else(|| {
                        format!("{}.{}: len={other:?} is not readable", c.name, field.name)
                    })?,
                };
                quote!({
                    let __n = if #access.is_null() { 0usize } else { #n };
                    vec![0u8; __n]
                })
            }
            // **An output array is never skipped, whatever its element is.**
            //
            // `Skip` means "do not read the caller's memory here", and for an
            // output array that is right about the *contents* and wrong about
            // the *capacity*: the count-then-fill idiom needs the peer to know
            // how many elements it may write, and the wire carries that as the
            // length of the array it is sent (docs/design.md §6.1). The arm
            // below sends `__n` default elements, which reads nothing of the
            // caller's and states the capacity exactly.
            //
            // Without this guard `partial_validity` decided by *element type*:
            // an array of structs came back `Partial` and reached that arm, an
            // array of plain scalars came back `Skip` and became `Vec::new()`.
            // So `vkGetPhysicalDeviceCalibrateableTimeDomainsKHR` sent capacity
            // zero however much the application had allocated, the GPU side
            // dutifully asked the driver for nothing, and the reply came back
            // `VK_INCOMPLETE` for ever. Zink asks for the time domains while
            // building its screen and treats the failure as fatal — this is the
            // second half of why Firefox never got a window (docs/design.md
            // §0.22), and the same struct-shaped blind spot as the GPU-side
            // `Out::Array` arm it mirrors.
            (Shape::Array { .. }, PartialValidity::Skip) if field.is_output => {
                let ty = wire_type(match &field.shape {
                    Shape::Array { elem, .. } => elem,
                    other => other,
                });
                let d = wire_default(match &field.shape {
                    Shape::Array { elem, .. } => elem,
                    other => other,
                });
                let len = abi.len.as_ref().ok_or_else(|| {
                    format!("{}.{}: an output array with no len=", c.name, field.name)
                })?;
                let n = match len {
                    ArrayLen::Path { root, field } => {
                        let a = path_access(root, field);
                        let r = naming::ident(&naming::snake_case(root));
                        quote!(if #r.is_null() { 0usize } else { (#a) as usize })
                    }
                    other => count_expr(other, &count_access).ok_or_else(|| {
                        format!("{}.{}: len={other:?} is not readable", c.name, field.name)
                    })?,
                };
                quote!({
                    let __n = if #access.is_null() { 0usize } else { #n };
                    let mut __v: Vec<#ty> = Vec::with_capacity(__n);
                    for _ in 0..__n { __v.push(#d); }
                    __v
                })
            }
            (_, PartialValidity::Skip) => wire_default(&field.shape),
            // The decayed array of `c_param_type`: the wire shape is a fixed
            // array but what arrived is a pointer, so it is indexed as one.
            (Shape::FixedArray { elem, count, .. }, _)
                if matches!(abi.ty.kind, CTypeKind::Array(..)) =>
            {
                let n = *count as usize;
                let ty = wire_type(elem);
                let inner = lower_expr(
                    cx,
                    elem,
                    abi,
                    &quote!(*(#access).add(__i)),
                    &count_access,
                    &c.name,
                )?;
                let cname = c.name.as_str();
                quote!({
                    if (#access).is_null() {
                        return Err(crate::LowerError::Shape(#cname));
                    }
                    let mut __v: Vec<#ty> = Vec::with_capacity(#n);
                    for __i in 0..#n { __v.push(#inner); }
                    match <[#ty; #n]>::try_from(__v) {
                        Ok(a) => a,
                        Err(_) => return Err(crate::LowerError::Shape(#cname)),
                    }
                })
            }
            // An output array: send the caller's capacity as the element count,
            // with no values. Venus's `_partial` array, and it is what lets the
            // peer clamp its reply against our buffer (docs/design.md §6.1).
            (Shape::Array { elem, .. }, _) if field.is_output => {
                let ty = wire_type(elem);
                let d = wire_default(elem);
                let len = abi.len.as_ref().ok_or_else(|| {
                    format!("{}.{}: an output array with no len=", c.name, field.name)
                })?;
                let n = match len {
                    ArrayLen::Path { root, field } => {
                        let a = path_access(root, field);
                        let r = naming::ident(&naming::snake_case(root));
                        quote!(if #r.is_null() { 0usize } else { (#a) as usize })
                    }
                    other => count_expr(other, &count_access).ok_or_else(|| {
                        format!("{}.{}: len={other:?} is not readable", c.name, field.name)
                    })?,
                };
                quote!({
                    let __n = if #access.is_null() { 0usize } else { #n };
                    let mut __v: Vec<#ty> = Vec::with_capacity(__n);
                    for _ in 0..__n { __v.push(#d); }
                    __v
                })
            }
            // **An output struct parameter gives up its chain's tags and
            // nothing else — decided here, by direction, not by the type.**
            //
            // Venus's three-state validity is a statement about *which way a
            // parameter travels*: on the way out, a structure the implementation
            // fills is the caller's uninitialised memory and must not be read.
            // This build derived that from `returnedonly` on the struct, which
            // is a property of the type — and a type can travel both ways.
            // `VkPhysicalDeviceFeatures2` is an input to `vkCreateDevice` and an
            // output of `vkGetPhysicalDeviceFeatures2`, so it carries no flag,
            // so its chain was walked in full and Firefox faulted lowering a
            // link out of the loader's uninitialised storage (docs/design.md
            // §0.24, the fourth fault in that chain).
            //
            // The call site is the only place that knows the direction, so the
            // call site decides. What the peer needs from an output structure is
            // the *shape of its chain* — which links are there for it to fill —
            // and nothing else; `lower_out_*` is that, and it subsumes the
            // `returnedonly` case rather than sitting beside it.
            (Shape::Optional(inner), _)
                if field.is_output
                    && matches!(&**inner, Shape::Scalar(Scalar::Struct(n))
                        if cx.shared.contains(n.as_str())) =>
            {
                let Shape::Scalar(Scalar::Struct(n)) = &**inner else { unreachable!() };
                let f = lower_out_struct_fn(n);
                quote!(if #access.is_null() { None } else { Some(Box::new(#f(#access)?)) })
            }
            (Shape::Optional(inner), _) if field.is_output => {
                // `uint32_t* pCount` on the way in carries the caller's capacity.
                let deref = quote!((*#access));
                let e = lower_expr(cx, inner, abi, &deref, &count_access, &c.name)?;
                let some = if matches!(**inner, Shape::Scalar(Scalar::Struct(_) | Scalar::Union(_)))
                {
                    quote!(Some(Box::new(#e)))
                } else {
                    quote!(Some(#e))
                };
                quote!(if #access.is_null() { None } else { #some })
            }
            _ => {
                // A path length is the one shape whose count lives in another
                // parameter's memory rather than in a sibling local.
                if let Some(ArrayLen::Path { root, field: fld }) = &abi.len {
                    let ty = wire_type(match &field.shape {
                        Shape::Array { elem, .. } => elem,
                        other => other,
                    });
                    let elem = match &field.shape {
                        Shape::Array { elem, .. } => elem,
                        other => {
                            return Err(format!(
                                "{}.{}: a path len= on a {other:?}",
                                c.name, field.name
                            ));
                        }
                    };
                    let r = naming::ident(&naming::snake_case(root));
                    let a = path_access(root, fld);
                    let inner = lower_expr(
                        cx,
                        elem,
                        abi,
                        &quote!(*(#access).add(__i)),
                        &count_access,
                        &c.name,
                    )?;
                    quote!({
                        let __n = if #r.is_null() || #access.is_null() {
                            0usize
                        } else {
                            (#a) as usize
                        };
                        let mut __v: Vec<#ty> = Vec::with_capacity(__n);
                        for __i in 0..__n { __v.push(#inner); }
                        __v
                    })
                } else {
                    lower_expr(cx, &field.shape, abi, &access, &count_access, &c.name)?
                }
            }
        };
        inits.push(quote!(#id: #e,));
    }

    let args_ty = format_ident!("{}Args", naming::opcode_variant(&c.name));
    let method = naming::command_method(&c.name);
    // What a lowering failure returns. A `void` command has nowhere to put an
    // error, so it reports and returns; a `VkResult` command maps the error onto
    // one of the results the specification allows an implementation to invent.
    let on_err = if c.returns_void {
        quote!({ crate::report_failure(__e); return; })
    } else if wire.returns_value {
        // There is no `VkResult` to fail with and the specification gives these
        // three no error path at all — `vkGetBufferDeviceAddress` "returns the
        // address" and that is the whole contract. Zero is the one value that
        // cannot be a valid device address, so it is the failure, and it is
        // *reported* as well so that a run under `SV_DEBUG` says why rather than
        // leaving an application to dereference a null address in a shader.
        quote!({ crate::report_failure(__e); return 0 as _; })
    } else {
        quote!(return crate::lower_failed(__e))
    };

    // --- call and write back ---------------------------------------------
    let call = if wire.generates_reply {
        let store = emit_writeback(store_cx, c, wire, &abi_by_name)?;
        // A value that is neither `void` nor `VkResult` rides the reply's `ret`
        // field (docs/design.md §0.3 item 5, closed). It is read out *before* the
        // writeback, because the writeback consumes the reply, and cast back to
        // whatever the C header spells the return type — `VkDeviceAddress` here,
        // a bare `uint64_t` for the two opaque-capture-address commands.
        let (take_ret, result) = if c.returns_result {
            (quote!(), quote!(__r))
        } else if c.returns_void {
            (quote!(), quote!())
        } else if wire.returns_value {
            let cret = c_type(lower_cx, &c.return_type);
            (quote!(let __ret = __reply.ret as #cret;), quote!(__ret))
        } else {
            return Err(format!(
                "{} returns {:?}, which is neither void nor VkResult",
                c.name, c.return_type
            ));
        };
        quote! {
            let __reply = match crate::icd().#method(__args) {
                Ok(r) => r,
                Err(__e) => #on_err,
            };
            #take_ret
            let __r = #store;
            #result
        }
    } else {
        // Fire and forget: the whole `vkCmd*` family, plus the destroy calls.
        let result = if c.returns_result {
            quote!(VkResult::VK_SUCCESS)
        } else if c.returns_void {
            quote!()
        } else {
            return Err(format!("{} has no reply but returns a value", c.name));
        };
        quote! {
            if let Err(__e) = crate::icd().#method(__args) {
                #on_err;
            }
            #result
        }
    };

    // The lowering runs inside a closure so that `?` has somewhere to go: a
    // stub returns `VkResult`, not `Result`, and the alternative — threading a
    // match through every nested array element — is exactly the hand-typed
    // repetition this pass exists to avoid.
    Ok(quote! {
        let __build = || -> crate::LowerResult<sv_proto::#args_ty> {
            unsafe { Ok(sv_proto::#args_ty { #(#inits)* }) }
        };
        let __args = match __build() {
            Ok(a) => a,
            Err(__e) => #on_err,
        };
        unsafe { #call }
    })
}

/// Where an output array's capacity comes from.
enum Capacity {
    /// `uint32_t* pCount`: the caller's capacity on the way in, the answer on
    /// the way out.
    OutPointer(proc_macro2::Ident),
    /// `uint32_t createInfoCount`, or `pAllocateInfo->descriptorSetCount`: the
    /// caller stated it and there is nothing to write back.
    Input(TokenStream),
}

fn capacity(
    _cx: &Ctx<'_>,
    c: &AbiCommand,
    wire: &crate::plan::CommandPlan,
    abi_by_name: &BTreeMap<&str, &AbiMember>,
    field: &FieldPlan,
) -> Result<Capacity, String> {
    let len = abi_by_name
        .get(field.name.as_str())
        .and_then(|a| a.len.as_ref())
        .ok_or_else(|| format!("{}.{}: an output array with no len=", c.name, field.name))?;
    Ok(match len {
        ArrayLen::Sibling(s) => {
            if wire.outputs.iter().any(|f| &f.name == s) {
                Capacity::OutPointer(naming::ident(&naming::snake_case(s)))
            } else {
                let id = naming::ident(&naming::snake_case(s));
                Capacity::Input(quote!(#id))
            }
        }
        ArrayLen::Path { root, field: fld } => {
            let r = naming::ident(&naming::snake_case(root));
            let f = naming::ident(&naming::snake_case(fld));
            Capacity::Input(quote!(if (#r).is_null() { 0 } else { (*#r).#f }))
        }
        other => {
            return Err(format!("{}.{}: an output array with len={other:?}", c.name, field.name));
        }
    })
}

/// Statements that write a reply into the caller's output parameters, ending in
/// an expression of type `VkResult` (or `()`).
fn emit_writeback(
    cx: &Ctx<'_>,
    c: &AbiCommand,
    wire: &crate::plan::CommandPlan,
    abi_by_name: &BTreeMap<&str, &AbiMember>,
) -> Result<TokenStream, String> {
    let mut stmts = Vec::new();
    let mut incomplete = Vec::new();

    // Which output parameters are the count of another output array or blob.
    // They are written by the array's own arm, from what was actually copied,
    // and must not also be written from the reply.
    let count_of: BTreeSet<&str> = wire
        .outputs
        .iter()
        .filter(|f| matches!(f.shape, Shape::Array { .. } | Shape::Blob { .. }))
        .filter_map(|f| match abi_by_name.get(f.name.as_str()).and_then(|a| a.len.as_ref()) {
            Some(ArrayLen::Sibling(s)) => Some(s.as_str()),
            _ => None,
        })
        .collect();

    for field in &wire.outputs {
        let id = naming::ident(&field.rust_name);
        let ptr = naming::ident(&naming::snake_case(&field.name));

        if count_of.contains(field.name.as_str()) {
            continue;
        }

        match &field.shape {
            Shape::Array { .. } | Shape::Blob { .. } => {
                let store_one = match &field.shape {
                    Shape::Array { elem, .. } => store_stmt(
                        cx,
                        elem,
                        &quote!(__reply.#id[__i]),
                        &quote!(*(#ptr).add(__i)),
                        &c.name,
                    )?,
                    // A sized `void*` is bytes, and its C pointer is not a value
                    // type at all, so the writes go through a `*mut u8` view.
                    _ => quote!(*((#ptr) as *mut u8).add(__i) = __reply.#id[__i];),
                };

                match capacity(cx, c, wire, abi_by_name, field)? {
                    // The count-then-fill idiom: the caller's capacity arrives in
                    // `*pCount` and the answer is written back to it. Venus's
                    // clamp (docs/design.md §6.1) — an unclamped write here is a
                    // buffer overflow in *our* code the moment either side has a
                    // bug, and the clamp is one comparison.
                    Capacity::OutPointer(count_ptr) => {
                        // **The count comes from the reply's count field, not
                        // from the length of the reply's array**, and the
                        // difference is the whole count-then-fill idiom.
                        //
                        // On the first call an application passes `pProperties =
                        // NULL` and asks only "how many?". A GPU side answering
                        // that honestly sends back the number and *no array* —
                        // there is nowhere to put one and nobody asked. Deriving
                        // `*pCount` from the array's length then writes zero,
                        // the application allocates nothing, and the query comes
                        // back empty with no error anywhere.
                        //
                        // It looked right for as long as the GPU side happened
                        // to return the full array on every call, which the
                        // hand-written half did. The first generated forwarder
                        // that did the sane thing turned every enumeration in
                        // the API into "count = 0", and `vulkaninfo` printed an
                        // empty queue-family list while the validation layer
                        // indexed the empty vector and died.
                        let want = count_ptr.to_string();
                        let count_id = naming::ident(
                            &wire
                                .outputs
                                .iter()
                                .find(|f| f.name == want || naming::snake_case(&f.name) == want)
                                .map(|f| f.rust_name.clone())
                                .unwrap_or(want),
                        );
                        stmts.push(quote! {
                            let __have = __reply.#id.len();
                            // What the peer says the real count is. Falls back to
                            // what it actually sent, so a reply with no count
                            // field still behaves.
                            let __said = __reply.#count_id.map(|v| v as usize).unwrap_or(__have);
                            let __cap = if (#count_ptr).is_null() {
                                0usize
                            } else {
                                (*(#count_ptr)) as usize
                            };
                            if (#ptr).is_null() {
                                if !(#count_ptr).is_null() {
                                    *(#count_ptr) = __said as _;
                                }
                            } else {
                                let __n = core::cmp::min(__cap, __have);
                                for __i in 0..__n { #store_one }
                                if !(#count_ptr).is_null() {
                                    *(#count_ptr) = __n as _;
                                }
                                if __n < __said { __incomplete = true; }
                            }
                        });
                        incomplete.push(());
                    }
                    // The caller stated the capacity as an ordinary input:
                    // `vkCreateGraphicsPipelines` writes exactly
                    // `createInfoCount` handles. There is nowhere to report a
                    // short answer, so a short answer is clamped and nothing
                    // else; `VK_INCOMPLETE` is not one of this command's
                    // documented results.
                    Capacity::Input(n) => {
                        stmts.push(quote! {
                            if !(#ptr).is_null() {
                                let __n = core::cmp::min((#n) as usize, __reply.#id.len());
                                for __i in 0..__n { #store_one }
                            }
                        });
                    }
                }
            }
            Shape::Optional(inner) => {
                let store = store_stmt(cx, inner, &quote!((*__v)), &quote!(*(#ptr)), &c.name)?;
                stmts.push(quote! {
                    if !(#ptr).is_null()
                        && let Some(__v) = __reply.#id.as_ref() {
                        #store
                    }
                });
            }
            other => {
                return Err(format!("{}.{}: cannot write back a {other:?}", c.name, field.name));
            }
        }
    }

    let result = if wire.returns_result {
        if incomplete.is_empty() {
            quote!(VkResult(__reply.result.0))
        } else {
            quote!(if __incomplete && __reply.result.0 == 0 {
                VkResult::VK_INCOMPLETE
            } else {
                VkResult(__reply.result.0)
            })
        }
    } else {
        quote!(())
    };

    Ok(quote! {{
        #[allow(unused_mut, unused_variables)]
        let mut __incomplete = false;
        #(#stmts)*
        #result
    }})
}

// ---------------------------------------------------------------------------
// the trait, the tables and the whole crate
// ---------------------------------------------------------------------------

/// Which ring a command's packet belongs on (docs/design.md §4.4).
///
/// Derived from the command's first parameter, which is the only thing that
/// decides it: a `VkQueue` command is a queue-ring command, anything naming a
/// `VkDevice` or an object owned by one is a device-ring command, and everything
/// before a device exists goes on the instance ring.
///
/// This is generated rather than hand-listed for the reason §5.3 gives about the
/// `transform` pass: hand-listing the sites is how it goes wrong, and the
/// registry already knows the answer.
fn ring_class(c: &crate::plan::CommandPlan) -> TokenStream {
    let first = c.inputs.first().map(|f| &f.shape);
    match first {
        Some(Shape::Scalar(Scalar::Handle(h))) => match h.as_str() {
            "VkQueue" => quote!(crate::RingClass::Queue),
            "VkInstance" | "VkPhysicalDevice" => quote!(crate::RingClass::Instance),
            _ => quote!(crate::RingClass::Device),
        },
        // No handle at all: `vkEnumerateInstanceVersion` and friends, which are
        // answered before an instance exists.
        _ => quote!(crate::RingClass::Instance),
    }
}

/// The app side's forwarding functions: `Args` in, ring out, `Reply` back.
///
/// One free function per forwarded command, each the exact dual of the arm
/// `sv_proto::dispatch` generates for the same opcode. They are free functions
/// rather than methods on a trait so that nothing here has to be object-safe and
/// so that `sv-icd` can call one directly when it wants to forward a command it
/// otherwise answers locally.
/// Pass 6, app-side half: the `(memory, offset)` rewrite of docs/design.md §5.3,
/// applied to one command's inputs just before they are encoded.
///
/// # Why here and not in the C stub
///
/// The stub lowers the application's arguments and then calls the [`Icd`] trait,
/// whose method the app side is entitled to *answer itself*: `vkAllocateMemory`,
/// `vkMapMemory` and `vkFreeMemory` are answered out of the app-side allocator
/// and never cross at all. Rewriting in the stub would hand those overrides
/// arena coordinates in place of the ids their own tables are keyed by. This
/// module is the narrower seam — everything that reaches it is about to become
/// bytes on a ring — so "every memory id that crosses is rewritten" is stated
/// exactly where it is true, and nothing that does not cross is touched.
///
/// It is the same fold `gpu_entry` runs on the way in, sharing
/// [`super::passes::transform_walk_pub`] and
/// [`crate::exceptions::memory_offset_sibling`] so that the two sides cannot
/// disagree about what a pair is.
fn memory_rewrite_pre(cx: &Ctx<'_>, c: &crate::plan::CommandPlan) -> TokenStream {
    // A `(memory, offset)` pair at the top level of a parameter list, which no
    // struct walker can see: `vkBindImageMemory(device, image, memory,
    // memoryOffset)` is exactly this shape.
    let param_names: Vec<String> = c.inputs.iter().map(|f| f.name.clone()).collect();
    let mut pairs = Vec::new();
    for f in &c.inputs {
        if !matches!(&f.shape, Shape::Scalar(Scalar::Handle(h)) if h == "VkDeviceMemory") {
            continue;
        }
        let mem = naming::ident(&f.rust_name);
        match crate::exceptions::memory_offset_sibling(&param_names, &f.name) {
            Some(off) => {
                let off_id = naming::ident(&naming::snake_case(&off));
                pairs.push(quote! {
                    let (__mm, __oo) = sv_proto::MemoryTransform::rewrite(
                        &mut crate::MemoryRewrite, args.#mem.0, args.#off_id,
                    ).map_err(crate::wire_failed)?;
                    args.#mem.0 = __mm;
                    args.#off_id = __oo;
                });
            }
            None => pairs.push(quote! {
                let (__mm, _) = sv_proto::MemoryTransform::rewrite(
                    &mut crate::MemoryRewrite, args.#mem.0, 0,
                ).map_err(crate::wire_failed)?;
                args.#mem.0 = __mm;
            }),
        }
    }

    // And every pair reachable through a struct, an array of structs or a
    // `pNext` link: `VkBindImageMemoryInfo`, `VkMappedMemoryRange`,
    // `VkSparseMemoryBind`.
    let mut walks = Vec::new();
    for f in &c.inputs {
        let id = naming::ident(&f.rust_name);
        if let Some(stmt) = super::passes::transform_walk_pub(cx.wire, &f.shape, &quote!(args.#id)) {
            walks.push(stmt);
        }
    }
    if pairs.is_empty() && walks.is_empty() {
        return quote!();
    }
    // `__t` is deliberately shadowed inside the block: the walkers were written
    // to be emitted next to a `&mut dyn MemoryTransform` of that name, and the
    // enclosing function's `__t` is the transport. Nothing inside the block
    // wants the transport, and the shadow ends with it.
    // The walkers speak `sv_wire::Result` and this function returns
    // `IcdResult`, and `sv_icd_entry` deliberately has no `From<WireError>` —
    // its own words: "`?` on a `WireError` inside a generated body would then
    // convert silently, and this crate's rule is that a generated block does
    // nothing the generator did not write out". So the conversion is written
    // out, once, around the whole walk.
    let walks = if walks.is_empty() {
        quote!()
    } else {
        quote! {
            {
                let mut __ad = crate::MemoryRewrite;
                let __t: &mut dyn sv_proto::MemoryTransform = &mut __ad;
                let mut __walk = || -> sv_wire::Result<()> {
                    #(#walks)*
                    Ok(())
                };
                __walk().map_err(crate::wire_failed)?;
            }
        }
    };
    quote! {
        let mut args = args;
        #(#pairs)*
        #walks
    }
}

fn emit_forward_module(cx: &Ctx<'_>) -> TokenStream {
    let fns = cx.wire.commands.iter().map(|c| {
        let m = naming::command_method(&c.name);
        let snake = naming::snake_case(&c.name);
        let args = format_ident!("{}Args", naming::opcode_variant(&c.name));
        let size_cmd = format_ident!("size_cmd_{}", snake);
        let encode_cmd = format_ident!("encode_cmd_{}", snake);
        let opcode = c.opcode;
        let ring = ring_class(c);
        let doc = format!("Forward `{}` to the GPU side.", c.name);
        let mem_pre = memory_rewrite_pre(cx, c);

        // The command bytes are identical in both directions; only the flag and
        // what happens afterwards differ.
        let encode = quote! {
            #mem_pre
            let mut __cmd = vec![0u8; sv_proto::#size_cmd(&args)];
            {
                let mut __enc = sv_wire::Encoder::new(&mut __cmd);
                sv_proto::#encode_cmd(&mut __enc, __flags, &args).map_err(crate::wire_failed)?;
                __enc.finish().map_err(crate::wire_failed)?;
            }
        };

        if c.generates_reply {
            let reply = format_ident!("{}Reply", naming::opcode_variant(&c.name));
            let decode_reply = format_ident!("decode_reply_{}", snake);
            quote! {
                #[doc = #doc]
                pub fn #m(
                    __t: &dyn crate::Transport,
                    args: sv_proto::#args,
                ) -> crate::IcdResult<sv_proto::#reply> {
                    let __flags = sv_wire::CMD_FLAG_GENERATE_REPLY;
                    #encode
                    let mut __reply = Vec::new();
                    __t.call(#ring, #opcode, &__cmd, &mut __reply)?;
                    let mut __dec = sv_wire::Decoder::new(&__reply);
                    sv_proto::#decode_reply(&mut __dec).map_err(crate::wire_failed)
                }
            }
        } else if c.is_recorded {
            // docs/design.md §6.5: "`vkCmd*` calls are **not** sent individually.
            // Each `VkCommandBuffer` owns an arena block; recording encodes
            // straight into it."
            //
            // The command buffer is the first parameter of every `vkCmd*` in
            // the registry — that is what makes one a `vkCmd*` — and it is the
            // only thing the transport needs in order to know which blob these
            // bytes belong to. Asserting it rather than assuming it, because a
            // recorded command whose first parameter were something else would
            // otherwise route its bytes into whichever blob that id happened to
            // name.
            let cb = c
                .inputs
                .first()
                .filter(|f| matches!(&f.shape, Shape::Scalar(Scalar::Handle(h)) if h == "VkCommandBuffer"))
                .map(|f| naming::ident(&f.rust_name));
            let Some(cb) = cb else {
                return quote! {
                    compile_error!(concat!(
                        "a recorded command whose first parameter is not a VkCommandBuffer: ",
                        #doc
                    ));
                };
            };
            quote! {
                #[doc = #doc]
                pub fn #m(
                    __t: &dyn crate::Transport,
                    args: sv_proto::#args,
                ) -> crate::IcdResult<()> {
                    let __flags = 0u32;
                    let __cb = args.#cb.0;
                    #encode
                    __t.record(__cb, #opcode, &__cmd)
                }
            }
        } else {
            quote! {
                #[doc = #doc]
                pub fn #m(
                    __t: &dyn crate::Transport,
                    args: sv_proto::#args,
                ) -> crate::IcdResult<()> {
                    let __flags = 0u32;
                    #encode
                    __t.send(#ring, #opcode, &__cmd)
                }
            }
        }
    });

    quote! {
        /// Encode a command, hand it to the transport, decode the reply.
        ///
        /// The dual of `sv_proto::dispatch`, and the thing that turns
        /// [`crate::Icd`]'s default method from a refusal into a round trip.
        pub mod forward {
            #(#fns)*
        }
    }
}

/// Statements that fill every output handle of a command with a fresh id.
///
/// # Why this has to be generated, and cannot be a plain forward
///
/// docs/design.md §6.4: "the app side allocates ids from a per-device 64-bit counter,
/// so `vkCreateBuffer` encodes the id it chose and returns immediately". The id
/// is therefore an **input** to the command even though C calls it an output
/// parameter — and the lowering pass, faithfully, fills it from the
/// application's `*pBuffer`, which before the call holds nothing.
///
/// A default that forwarded the lowered arguments unchanged therefore sent
/// `pImage = 0` for every command `sv-icd` had not overridden, the GPU side
/// dutifully recorded "object 0 is this image", and the *next* call arrived with
/// a null handle. RADV segfaulted inside
/// `vkGetImageMemoryRequirements2` — no error, no diagnostic, just a dead GPU
/// side and `VK_ERROR_DEVICE_LOST` on the app side one call later.
///
/// So minting belongs in the default. It also means `sv-icd` does not have to
/// override a command merely to give it an id, which was the shape of every
/// create-command override this crate would otherwise need.
fn emit_mint_outputs(c: &crate::plan::CommandPlan) -> Vec<TokenStream> {
    let mut stmts = Vec::new();
    for f in &c.outputs {
        let name = format_ident!("{}", f.rust_name);
        match &f.shape {
            Shape::Optional(inner) => {
                if let Shape::Scalar(Scalar::Handle(h)) = &**inner {
                    let ty = format_ident!("{}", h);
                    stmts.push(quote! {
                        args.#name = Some(sv_proto::#ty(crate::IcdLocal::mint_id(self)));
                    });
                }
            }
            Shape::Array { elem, .. } => {
                if let Shape::Scalar(Scalar::Handle(h)) = &**elem {
                    let ty = format_ident!("{}", h);
                    // The length comes from the lowering, which read the
                    // application's own count. Each slot gets its own id.
                    stmts.push(quote! {
                        for __slot in args.#name.iter_mut() {
                            *__slot = sv_proto::#ty(crate::IcdLocal::mint_id(self));
                        }
                    });
                }
            }
            Shape::Scalar(Scalar::Handle(h)) => {
                let ty = format_ident!("{}", h);
                stmts.push(quote! {
                    args.#name = sv_proto::#ty(crate::IcdLocal::mint_id(self));
                });
            }
            _ => {}
        }
    }
    stmts
}

fn emit_icd_trait(cx: &Ctx<'_>) -> TokenStream {
    let methods = cx.wire.commands.iter().map(|c| {
        let m = naming::command_method(&c.name);
        let args = format_ident!("{}Args", naming::opcode_variant(&c.name));
        let cname = c.name.as_str();
        let mint = emit_mint_outputs(c);
        let mut_args = if mint.is_empty() { quote!() } else { quote!(mut) };
        let doc = format!(
            "`{}`.\n\n**The default forwards.** If [`crate::IcdLocal::transport`] \
             returns a connected transport this encodes the command, sends it to \
             the GPU side and decodes the reply; with no transport it refuses by \
             name. An implementation overrides this method only for the commands \
             docs/design.md §8 and §10 say are answered app-side.",
            c.name
        );
        if HAND_WRITTEN.contains(&c.name.as_str()) {
            // Its method lives on `IcdLocal`, hand-declared and speaking in
            // object ids. Generating a second one of the same name here makes
            // every call site ambiguous, since `Icd: IcdLocal`.
            return quote!();
        }
        let ret = if c.generates_reply {
            let reply = format_ident!("{}Reply", naming::opcode_variant(&c.name));
            quote!(sv_proto::#reply)
        } else {
            quote!(())
        };
        quote! {
            #[doc = #doc]
            fn #m(&self, #mut_args args: sv_proto::#args) -> crate::IcdResult<#ret> {
                match crate::IcdLocal::transport(self) {
                    Some(__t) => {
                        #(#mint)*
                        crate::forward::#m(__t, args)
                    }
                    None => {
                        let _ = args;
                        Err(crate::LowerError::NotImplemented(#cname))
                    }
                }
            }
        }
    });
    quote! {
        /// What `sv-icd` implements: one safe method per forwarded command.
        ///
        /// Every method takes decoded, owned arguments and returns a decoded
        /// reply. Nothing on this trait is `unsafe` and nothing on it is a raw
        /// pointer — the lowering above has already happened. That is the whole
        /// point of docs/design.md §7.3's "the unsafe block is one line and reviewable
        /// by pattern": this trait is the line it calls.
        ///
        /// It is the app-side dual of `sv_proto::Server`, and the two are
        /// deliberately the same shape.
        pub trait Icd: crate::IcdLocal + Send + Sync + 'static {
            #(#methods)*
        }
    }
}

/// One callable probe per forwarded command, for the carried-vs-implemented audit.
///
/// # The question this exists to answer mechanically
///
/// The profile says which commands are *carried*. It does not say which are
/// *implemented*, and the two are not the same claim: a command whose generated
/// forwarder exists but whose `Icd` method `sv-icd` overrides with a canned
/// value never reaches a driver, and returns something plausible while doing so.
/// That is worse than a missing command, because a missing one is diagnosable.
///
/// Reading 239 method bodies answers it and does not stay answered. This does,
/// and it rests on one property of [`emit_icd_trait`] above: **the generated
/// default, with no transport, returns
/// `Err(LowerError::NotImplemented(<this command's own name>))` and nothing
/// else does.** So calling every method on a driver with no GPU side sorts the
/// set exactly in two:
///
/// - that precise error ⇒ the default is in force ⇒ the command *forwards* when
///   there is a GPU side, and no hand-written code stands in its way;
/// - anything else — a success, a different error, a panic ⇒ `sv-icd` overrode
///   it and answered locally, and it needs classifying by hand.
///
/// The second set is small enough to read. The first is the one that would
/// otherwise be taken on trust, and it is the larger by an order of magnitude.
///
/// `Default::default()` for the arguments is deliberate and is not a weakness of
/// the probe: a zeroed `Args` names object id 0, which is `VK_NULL_HANDLE`, so
/// nothing here can resolve to a real object and no probe can have an effect
/// beyond minting an id. What is being measured is *which branch the
/// implementation takes*, and that is decided before any argument is looked at.
fn emit_audit_probes(cx: &Ctx<'_>) -> TokenStream {
    let probes = cx.wire.commands.iter().filter_map(|c| {
        if HAND_WRITTEN.contains(&c.name.as_str()) {
            // On `IcdLocal`, with a hand-written signature that does not take an
            // `Args`. There is nothing mechanical to call.
            return None;
        }
        let m = naming::command_method(&c.name);
        let cname = c.name.as_str();
        Some(quote! {
            crate::AuditProbe {
                command: #cname,
                run: |__icd| {
                    match crate::Icd::#m(__icd, Default::default()) {
                        Ok(_) => crate::Answered::Locally,
                        // The command's own name, matched as a pattern rather
                        // than compared in a guard: only the generated default
                        // refuses with exactly this string, and a pattern says
                        // so where a guard merely tests it.
                        Err(crate::LowerError::NotImplemented(#cname)) => {
                            crate::Answered::ByTheGeneratedDefault
                        }
                        Err(__e) => crate::Answered::LocallyWithError(__e),
                    }
                },
            }
        })
    });
    quote! {
        /// Every forwarded command, callable, for `sv-icd`'s audit test.
        ///
        /// See `sv_codegen::emit::icd_entry::emit_audit_probes` for what the
        /// outcomes mean and why a zeroed `Args` is the right argument.
        pub static AUDIT_PROBES: &[crate::AuditProbe] = &[#(#probes),*];
    }
}

/// Entry points that are hand-written **and not on the generated C surface**,
/// which have to be put in the proc tables by name.
///
/// Four, all surface creators, and they are here for two facts that only make
/// sense together.
///
/// **The registry cannot give them a signature.** `vkCreateXcbSurfaceKHR` takes
/// a `VkXcbSurfaceCreateInfoKHR`, whose `connection` member is an
/// `xcb_connection_t*` — a type that comes from a platform header this project
/// does not include and would not link. `sv_codegen::abi::SURFACE_CREATORS`
/// therefore keeps them, and the two structs, off the generated surface.
///
/// **The loader nonetheless requires them.** This is a correction to design.md
/// §8, which says "We opt *out* of interface-3 surface creation by not exposing
/// `vkCreateXXXSurfaceKHR`, so the loader hands us a `VkIcdSurfaceXXX` from
/// `vk_icd.h` which we cast." **That opt-out no longer exists.** In
/// Vulkan-Loader 1.4.350 `wsi_unwrap_icd_surface` allocates a per-ICD surface
/// slot for every driver that enables `VK_KHR_surface` at interface ≥ 3, and
/// creates the ICD's surface *lazily* on first use by calling the ICD's own
/// `vkCreateXxxSurfaceKHR`. A driver that does not export one gets
/// `VK_ERROR_EXTENSION_NOT_PRESENT` from that lazy creation, which the
/// terminators turn into `*pSupported = false` and `VK_ERROR_SURFACE_LOST_KHR`
/// **with no diagnostic of any kind**. The observed symptom was Mesa printing
/// "zink: could not create swapchain" and this ICD's own trace showing that not
/// one of its surface entry points had been called.
///
/// So they are written by hand, with the create-info structs declared by hand
/// beside them, and the platform pointer is read as an opaque `usize`.
const EXTRA_LOCAL: &[(&str, &str)] = &[
    ("vkCreateHeadlessSurfaceEXT", "sv_vk_create_headless_surface_ext"),
    ("vkCreateXlibSurfaceKHR", "sv_vk_create_xlib_surface_khr"),
    ("vkCreateXcbSurfaceKHR", "sv_vk_create_xcb_surface_khr"),
    ("vkCreateWaylandSurfaceKHR", "sv_vk_create_wayland_surface_khr"),
];

fn emit_tables(cx: &Ctx<'_>, refused: &BTreeSet<String>) -> TokenStream {
    let mut global = Vec::new();
    let mut instance = Vec::new();
    let mut phys = Vec::new();
    let mut device = Vec::new();

    // The hand-written instance-level entry points the C surface does not carry.
    for (name, f) in EXTRA_LOCAL {
        let f = format_ident!("{}", f);
        instance.push(quote! {
            crate::ProcEntry {
                name: #name,
                pfn: crate::local::#f as *const core::ffi::c_void,
            },
        });
    }

    for c in &cx.abi.commands {
        let hand = HAND_WRITTEN.contains(&c.name.as_str());
        let stub = stub_ident(&c.name);
        let path = if hand { quote!(crate::local::#stub) } else { quote!(#stub) };
        for name in c.all_names() {
            let entry = quote! {
                crate::ProcEntry {
                    name: #name,
                    pfn: #path as *const core::ffi::c_void,
                },
            };
            match c.level {
                Level::Global => global.push(entry),
                Level::Instance => instance.push(entry),
                Level::PhysicalDevice => {
                    phys.push(entry.clone());
                    instance.push(entry);
                }
                Level::Device => device.push(entry),
            }
        }
    }

    let not_implemented: Vec<&str> = refused.iter().map(String::as_str).collect();

    // Extensions this build must not advertise, worked out here rather than
    // guessed at runtime.
    //
    // docs/design.md §9.2 says an extension in the advertised list is a promise its
    // entry points work. Until now that promise was kept by *intersecting* the
    // profile with what the GPU side's driver reports, which answers "does the
    // hardware have it" and leaves "did we generate a working stub for it"
    // unasked — which is how `bufferDeviceAddress` came to be advertised with
    // three of its entry points in `NOT_IMPLEMENTED`, and how an application
    // could call one and get a silent zero back.
    //
    // An extension appears here when **any** command it introduces is refused.
    // Not "all": an extension is one promise, and half of it working is a driver
    // that fails where the specification says it must not.
    //
    // Core versions are deliberately absent. A refused core command cannot be
    // un-advertised — the version number is what advertises it — so it stays a
    // `NOT_IMPLEMENTED` entry and nothing else. That is the honest split, and
    // the API version is `sv-worktrees/vers`'s question, not this table's.
    let mut incomplete: BTreeSet<(&str, String)> = BTreeSet::new();
    for e in &cx.model.extensions {
        if !e.supported_for_vulkan {
            continue;
        }
        // Through the alias, not just the name. `VK_KHR_buffer_device_address`
        // requires `vkGetBufferDeviceAddressKHR`, which is an *alias* of the
        // core `vkGetBufferDeviceAddress` — one entry point under two names, and
        // it is the core name the generator refuses. Comparing names alone let
        // the extension through, which is the exact bug this table exists to
        // stop, one indirection down.
        // **The refused command is kept, not just the fact that there was one.**
        // Without it the only thing a test can check about this table is that it
        // is sorted, and the interesting property — that every entry is here
        // *because of* a named stub — has to be asserted by writing the answer
        // down. It was, and it went stale the moment the stub it named became a
        // real implementation: `no_extension_is_advertised_with_a_stubbed_entry_-
        // point` hard-coded `VK_KHR_buffer_device_address`, and when
        // §0.20 implemented `vkGetBufferDeviceAddress` the assertion was left
        // demanding that a fixed bug still be present. Carrying the reason makes
        // the pairing derivable on both sides, so the test checks the invariant
        // instead of remembering an example.
        let because = e.requires.iter().flat_map(|b| b.items.iter()).find_map(|it| match it {
            vk_registry::InterfaceRef::Command(c) => {
                // Through the alias, not just the name.
                // `VK_KHR_buffer_device_address` requires
                // `vkGetBufferDeviceAddressKHR`, which is an *alias* of the core
                // `vkGetBufferDeviceAddress` — one entry point under two names,
                // and it is the core name the generator refuses. Comparing names
                // alone let the extension through, which is the exact bug this
                // table exists to stop, one indirection down. The name reported
                // is the one that is actually in `NOT_IMPLEMENTED`, so a reader
                // can find it and a test can look it up.
                if refused.contains(c) {
                    return Some(c.clone());
                }
                let canon = cx.model.canonical_command(c);
                refused.contains(&canon).then(|| canon.to_string())
            }
            _ => None,
        });
        if let Some(cmd) = because {
            incomplete.insert((e.name.as_str(), cmd));
        }
    }
    let incomplete_reasons: Vec<TokenStream> = incomplete
        .iter()
        .map(|(e, c)| {
            let c = c.as_str();
            quote!((#e, #c))
        })
        .collect();
    // The two classes share one output list: an extension is withheld whether

    // Commands whose app-side lowering would hand the application's own pointer
    // to the GPU side as an arena offset (docs/design.md §0.24 item 6).
    //
    // The third class of over-promise, and the one neither `NOT_IMPLEMENTED` nor
    // the queue check can see: the entry point exists, marshals and dispatches,
    // and the *value it marshals is wrong*. `INCOMPLETE_EXTENSIONS` asks "did
    // the generator emit a working stub"; here it did, and the stub writes a
    // wild pointer onto the wire.
    let mut lowering_incomplete: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut core_wild: Vec<String> = Vec::new();
    for c in &cx.wire.commands {
        let needs_expansion = cx.model.commands.get(&c.name).is_some_and(|cmd| {
            cmd.params.iter().any(|p| {
                p.ty.base().is_void()
                    && crate::exceptions::void_policy(&c.name, &p.name)
                        == Some(crate::exceptions::VoidPolicy::ArenaOffset)
            })
        });
        if !needs_expansion
            || crate::exceptions::ICD_EXPANDS_ARENA_POINTER.contains(&c.name.as_str())
        {
            continue;
        }
        let requires_it = |f: &vk_registry::FeatureSet| {
            f.requires.iter().flat_map(|b| b.items.iter()).any(|it| {
                matches!(it, vk_registry::InterfaceRef::Command(x)
                    if *x == c.name || cx.model.canonical_command(x) == c.name)
            })
        };

        // **Core first, and this ordering is the whole correctness of the
        // check.** A command can be required by an extension *and* by a core
        // version — `vkCmdPushDescriptorSetWithTemplate` is required by
        // `VK_KHR_push_descriptor` and by Vulkan 1.4 — and finding the extension
        // first makes the answer "withdraw the extension", which does nothing:
        // the core command is still carried and a 1.4 application still calls
        // it. The first run of this check did exactly that, and the wild pointer
        // it was written to stop survived it.
        //
        // There is nothing to withdraw for a core command, so it is a build
        // failure. That is the point of the check: it converts a mechanical
        // widening from a segfault into something that will not compile.
        if cx.model.features.iter().any(|f| f.supported_for_vulkan && requires_it(f)) {
            core_wild.push(c.name.clone());
            continue;
        }
        match cx.model.extensions.iter().filter(|e| e.supported_for_vulkan).find(|e| requires_it(e))
        {
            Some(e) => {
                lowering_incomplete.insert((e.name.as_str(), c.name.as_str()));
            }
            None => core_wild.push(c.name.clone()),
        }
    }
    let core_wild = if core_wild.is_empty() {
        quote!()
    } else {
        let msg = format!(
            "these commands lower a `void*` as an arena offset but nothing app-side \
             expands it, so the application's own pointer would cross as an offset — \
             and they are core, so there is no extension to stop advertising. Either \
             implement the expansion and add them to \
             `sv_codegen::exceptions::ICD_EXPANDS_ARENA_POINTER`, or exclude them in \
             the profile: {}",
            core_wild.join(", ")
        );
        quote!(compile_error!(#msg);)
    };
    let lowering_incomplete_pairs: Vec<TokenStream> = lowering_incomplete
        .iter()
        .map(|(e, c)| quote!((#e, #c)))
        .collect();
    let lowering_incomplete_exts: BTreeSet<&str> =
        lowering_incomplete.iter().map(|(e, _)| *e).collect();

    // its entry point is missing or its lowering is wrong, because from an
    // application's side those are the same broken promise.
    let incomplete: Vec<&str> = incomplete
        .iter()
        .map(|(e, _)| *e)
        .chain(lowering_incomplete_exts.iter().copied())
        .collect::<BTreeSet<&str>>()
        .into_iter()
        .collect();
    // What queue an extension's commands have to be recorded on, from `queues=`.
    //
    // # The defect this exists for, and why the other table could not catch it
    //
    // `INCOMPLETE_EXTENSIONS` asks "did the generator emit a working stub". It
    // answered yes for `VK_KHR_video_queue` and `VK_KHR_video_decode_queue` —
    // every one of their entry points marshals correctly — and the extensions
    // were advertised the moment `full` became the default profile. But the ICD
    // advertises **one queue family**, `GRAPHICS | COMPUTE | TRANSFER`, and
    // `vkCmdDecodeVideoKHR` is `queues="VK_QUEUE_VIDEO_DECODE_BIT_KHR"`. There
    // is no family an application can record it on. Firefox 153 selects a device
    // on exactly those two extensions and reports the failure as
    // `NS_ERROR_DOM_MEDIA_FATAL_ERR`, which its hardware-to-software recovery
    // path does not admit: one pref away from the default, that was broken
    // playback rather than slow playback.
    //
    // A table of stubbed entry points cannot see this, because nothing about a
    // *queue family* is expressible in one. The fact is "we advertise a family
    // we did not open", and the registry states the other half of it.
    //
    // # Why the shape is a list of masks
    //
    // `queues=` is a **disjunction** — `queues="A,B"` means the command may be
    // recorded from a family with A *or* B. So one mask per command that has a
    // requirement, and an extension is servable when *every* mask intersects
    // what we advertise. Folding them together with `|` would say
    // `VK_KHR_video_queue` is fine because some other command of it runs on
    // graphics, which is how this class of bug survives a summary.
    //
    // Extensions with no queue requirement at all are omitted, which is most of
    // them: the table is the exceptions, and `sv-icd` treats an absent entry as
    // "nothing to check".
    let queue_flag_bits: BTreeMap<&str, i64> = cx
        .model
        .types
        .values()
        .find_map(|t| match t {
            vk_registry::TypeDef::Enum(e) if e.name == "VkQueueFlagBits" => Some(e),
            _ => None,
        })
        .map(|e| e.values.iter().map(|v| (v.name.as_str(), v.value)).collect())
        .unwrap_or_default();

    let mut unknown_queue_bits: BTreeSet<String> = BTreeSet::new();
    let mut queue_reqs: BTreeMap<&str, BTreeSet<u32>> = BTreeMap::new();
    for e in &cx.model.extensions {
        if !e.supported_for_vulkan {
            continue;
        }
        for it in e.requires.iter().flat_map(|b| b.items.iter()) {
            let vk_registry::InterfaceRef::Command(c) = it else { continue };
            let canon = cx.model.canonical_command(c);
            let Some(cmd) = cx.model.commands.get(&canon) else { continue };
            let Some(q) = cmd.queues.as_deref() else { continue };
            // `queues="true"` is the registry's older spelling for "any queue",
            // which constrains nothing.
            if q == "true" {
                continue;
            }
            let mut mask = 0u32;
            for name in q.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                match queue_flag_bits.get(name) {
                    Some(v) => mask |= *v as u32,
                    // A bit the registry names in `queues=` but does not define
                    // as a `VkQueueFlagBits` value is a registry the generator
                    // does not understand, and guessing zero would silently mean
                    // "no requirement" — the permissive direction, which is the
                    // wrong one to guess in.
                    // A bit named in `queues=` that this registry does not
                    // define as a `VkQueueFlagBits` value would silently mean
                    // "no requirement" if skipped — the permissive direction,
                    // which is the wrong one to guess in. `compile_error!` is
                    // how this file already refuses what it cannot know, and it
                    // fails the build rather than the playback.
                    None => {
                        unknown_queue_bits.insert(format!("{canon} queues={q:?}: {name}"));
                    }
                }
            }
            if mask != 0 {
                queue_reqs.entry(e.name.as_str()).or_default().insert(mask);
            }
        }
    }
    let queue_reqs: Vec<TokenStream> = queue_reqs
        .iter()
        .map(|(e, masks)| {
            let masks = masks.iter().copied().collect::<Vec<u32>>();
            quote!((#e, &[#(#masks),*] as &[u32]))
        })
        .collect();
    let unknown_queue_bits = if unknown_queue_bits.is_empty() {
        quote!()
    } else {
        let msg = format!(
            "queues= names a VkQueueFlagBits this registry does not define, so \
             EXTENSION_QUEUE_REQUIREMENTS cannot say what family these need and \
             would under-constrain them: {}",
            unknown_queue_bits.iter().cloned().collect::<Vec<_>>().join("; ")
        );
        quote!(compile_error!(#msg);)
    };

    // `depends=`, compiled to a boolean expression over the advertised set.
    //
    // The queue check above withdrew `VK_KHR_video_queue`,
    // `VK_KHR_video_decode_queue` and `VK_KHR_video_encode_queue` — every video
    // extension that *has* commands. Ten remained: `VK_KHR_video_decode_h264`,
    // `…_av1`, `VK_KHR_video_maintenance1` and the rest are codec and
    // maintenance extensions that add only structs and enums, so they constrain
    // no queue and the check had nothing to say about them.
    //
    // Advertising them is still a claim we cannot keep. Each `depends=` on one
    // of the three that just went away, and §9.2's promise is about the
    // extension as a whole: "we support H.264 decode, but not the decode queue
    // you would decode on" is not a thing an implementation may say. So the
    // advertised set is closed under `depends=` as well — by fixpoint, because
    // `VK_KHR_video_maintenance2` depends on `VK_KHR_video_queue` through
    // nothing else and a single pass would keep whatever depended on *it*.
    //
    // Lowered to code here rather than evaluated at runtime because `sv-icd` is
    // a `cdylib` and the alternative was linking `vk-parse` into a driver for
    // one function, or writing a second evaluator that could drift from
    // `vk_registry::depends`. `Expr` is that module's own AST, so there is still
    // exactly one parser.
    fn lower_depends(e: &vk_registry::depends::Expr) -> TokenStream {
        match e {
            vk_registry::depends::Expr::Name(n) => quote!(__have(#n)),
            vk_registry::depends::Expr::Any(v) => {
                let parts = v.iter().map(lower_depends);
                quote!((false #(|| #parts)*))
            }
            vk_registry::depends::Expr::All(v) => {
                let parts = v.iter().map(lower_depends);
                quote!((true #(&& #parts)*))
            }
        }
    }
    let depends_arms: Vec<TokenStream> = cx
        .model
        .extensions
        .iter()
        .filter(|e| e.supported_for_vulkan)
        .filter_map(|e| {
            let expr = e.depends.as_deref()?;
            let name = e.name.as_str();
            // An expression this parser cannot read must read as *unsatisfied*,
            // the conservative direction — the same rule
            // `vk_registry::depends::satisfied` states.
            match vk_registry::depends::compile(expr) {
                Some(ast) => {
                    let body = lower_depends(&ast);
                    Some(quote!(#name => #body,))
                }
                None => Some(quote!(#name => false,)),
            }
        })
        .collect();

    let n_impl = cx.abi.commands.len() - refused.len();
    let total = cx.abi.commands.len();
    let hand_written = HAND_WRITTEN;
    let extra_local: Vec<&str> = EXTRA_LOCAL.iter().map(|(n, _)| *n).collect();

    quote! {
        /// Commands answerable by `vkGetInstanceProcAddr(NULL, …)`.
        pub static GLOBAL_COMMANDS: &[crate::ProcEntry] = &[#(#global)*];
        /// Commands answerable by `vkGetInstanceProcAddr(instance, …)` and not by
        /// `vkGetDeviceProcAddr`.
        pub static INSTANCE_COMMANDS: &[crate::ProcEntry] = &[#(#instance)*];
        /// The set `vk_icdGetPhysicalDeviceProcAddr` answers for: commands whose
        /// first parameter is a `VkPhysicalDevice` (docs/design.md §8).
        pub static PHYSICAL_DEVICE_COMMANDS: &[crate::ProcEntry] = &[#(#phys)*];
        /// Commands answerable by `vkGetDeviceProcAddr`.
        pub static DEVICE_COMMANDS: &[crate::ProcEntry] = &[#(#device)*];

        /// Entry points that exist and refuse.
        ///
        /// Checked by a test rather than left as a comment, because "which
        /// commands are stubbed" is the single most useful thing to be able to
        /// read off a partial implementation, and a list that drifts from the
        /// code is worse than no list.
        pub static NOT_IMPLEMENTED: &[&str] = &[#(#not_implemented,)*];

        /// Extensions at least one of whose entry points is in
        /// [`NOT_IMPLEMENTED`], and which must therefore **not** be advertised.
        ///
        /// `sv_icd` subtracts this from whatever the GPU side reports, so that
        /// the advertised set is the intersection of three things and not two:
        /// what the profile carries, what the device supports, and what this
        /// build actually generated a working stub for.
        ///
        /// Sorted, so `binary_search` is the lookup.
        pub static INCOMPLETE_EXTENSIONS: &[&str] = &[#(#incomplete,)*];

        #unknown_queue_bits

        /// What queue an extension's commands must be recorded on, from the
        /// registry's `queues=`.
        ///
        /// One entry per extension that constrains anything, and **one mask per
        /// command** rather than one per extension: `queues=` is a disjunction,
        /// so a command is satisfiable when its mask intersects a family we
        /// advertise, and the *extension* is satisfiable when every one of its
        /// masks is. Folding them together with `|` would report
        /// `VK_KHR_video_queue` as servable because some other command of it
        /// runs on graphics, which is precisely how the defect this table exists
        /// for survived being looked at.
        ///
        /// `sv_icd::advertise::unservable_queue_requirement` is the consumer.
        /// Extensions absent from this list constrain nothing.
        ///
        /// Sorted by extension name, so `binary_search_by_key` is the lookup.
        pub static EXTENSION_QUEUE_REQUIREMENTS: &[(&str, &[u32])] = &[#(#queue_reqs,)*];

        #core_wild

        /// Extensions withheld because a command's app-side *lowering* is
        /// incomplete — the entry point exists and marshals, and the value it
        /// marshals would be the application's own pointer sent as an arena
        /// offset (docs/design.md §0.24 item 6).
        ///
        /// Folded into [`INCOMPLETE_EXTENSIONS`]; kept separately with the
        /// offending command so a reader can tell this class from a stubbed
        /// entry point, which needs a different fix.
        pub static LOWERING_INCOMPLETE: &[(&str, &str)] = &[#(#lowering_incomplete_pairs,)*];

        /// Whether an extension's `depends=` is satisfied by `__have`.
        ///
        /// The registry's expression, compiled: `,` became `||`, `+` became
        /// `&&`, and a name became a call. An extension the registry gives no
        /// `depends=` — most of them — answers `true`.
        ///
        /// `sv_icd` uses it to close the advertised set under dependency, so
        /// that withdrawing `VK_KHR_video_decode_queue` also withdraws
        /// `VK_KHR_video_decode_h264`, which adds no commands of its own and so
        /// no other check can see.
        pub fn depends_satisfied(extension: &str, __have: &dyn Fn(&str) -> bool) -> bool {
            match extension {
                #(#depends_arms)*
                _ => true,
            }
        }

        /// Why each entry of [`INCOMPLETE_EXTENSIONS`] is there: the extension
        /// paired with one entry point of its own that is in
        /// [`NOT_IMPLEMENTED`].
        ///
        /// The name is the one the *generator refused*, which for a promoted
        /// extension is the core spelling rather than the suffixed alias —
        /// `VK_KHR_buffer_device_address` would appear here against
        /// `vkGetBufferDeviceAddress`, not against `vkGetBufferDeviceAddressKHR`
        /// — so it can be found in `NOT_IMPLEMENTED` and looked up by a test.
        ///
        /// This exists because the alternative was a test that named one
        /// extension by hand, and a hand-named case is a fact with an expiry
        /// date: it asserted `VK_KHR_buffer_device_address` must be excluded,
        /// docs/design.md §0.20 implemented the three entry points, and the assertion
        /// then failed for the best possible reason — demanding that a fixed bug
        /// still be present.
        pub static INCOMPLETE_EXTENSION_REASONS: &[(&str, &str)] = &[#(#incomplete_reasons,)*];

        /// Entry points `sv-icd-entry` writes by hand — see the generator's
        /// `HAND_WRITTEN` for why each one cannot be a fold over a wire reply.
        pub static HAND_WRITTEN: &[&str] = &[#(#hand_written,)*];

        /// Entry points that exist only by hand: the registry cannot give them
        /// a signature, and the loader requires them anyway. See the
        /// generator's `EXTRA_LOCAL`.
        pub static EXTRA_LOCAL: &[&str] = &[#(#extra_local,)*];

        /// How many of the C surface's entry points do something.
        pub const IMPLEMENTED_COUNT: usize = #n_impl;
        /// How many entry points the C surface has.
        pub const COMMAND_COUNT: usize = #total;
    }
}

fn emit_pnext_walkers(lower_cx: &Ctx<'_>, store_cx: &Ctx<'_>) -> TokenStream {
    let cx = lower_cx;
    // **One `#[inline(never)]` helper per link, for the stack.**
    //
    // The same defect as `sv_proto::decode_pnext`'s (docs/design.md §0.24) and
    // it survived that fix because it is a different crate: as
    // `#val => PNextLink::V(Box::new(lower_x(__p as *const T)?)),` every arm
    // materialises its struct as a temporary **in `lower_pnext`'s own frame**
    // before the `Box` moves it to the heap, and a debug build does not overlap
    // the slots of mutually exclusive match arms — so the frame is the *sum* of
    // a thousand link types rather than the largest.
    //
    // It is worse here than on the decode side, because this one **nests**: a
    // chain parent that is itself a link — `VkPhysicalDeviceFeatures2` inside
    // `VkDeviceCreateInfo`'s chain, which is exactly what Zink sends — puts two
    // of these frames on the stack at once. Firefox reaches it on a Mesa thread
    // with twenty frames already below.
    let lower_helpers = cx.wire.pnext_links.iter().filter_map(|(name, _)| {
        if !lower_cx.shared.contains(name) {
            return None;
        }
        let v = naming::pnext_variant(name);
        let f = lower_struct_fn(name);
        let ty = naming::type_ident(name);
        let helper = format_ident!("lower_link_{}", naming::snake_case(name));
        let doc = format!("One `{name}` link, lowered in its own frame.");
        Some(quote! {
            #[doc = #doc]
            #[inline(never)]
            unsafe fn #helper(
                __p: *const core::ffi::c_void,
            ) -> crate::LowerResult<sv_proto::PNextLink> {
                unsafe { Ok(sv_proto::PNextLink::#v(Box::new(#f(__p as *const #ty)?))) }
            }
        })
    });
    let lower_arms = cx.wire.pnext_links.iter().filter_map(|(name, value)| {
        if !lower_cx.shared.contains(name) {
            return None;
        }
        let helper = format_ident!("lower_link_{}", naming::snake_case(name));
        let val = *value as i32;
        Some(quote!(#val => #helper(__p)?,))
    });
    let store_arms = cx.wire.pnext_links.iter().filter_map(|(name, value)| {
        if !store_cx.shared.contains(name) {
            return None;
        }
        let v = naming::pnext_variant(name);
        let f = store_struct_fn(name);
        let ty = naming::type_ident(name);
        let val = *value as i32;
        Some(quote! {
            (#val, sv_proto::PNextLink::#v(__inner)) => #f(__inner, __p as *mut #ty),
        })
    });

    // The tag-only arms: the same links, with no contents read.
    let tag_arms = cx.wire.pnext_links.iter().filter_map(|(name, value)| {
        if !lower_cx.shared.contains(name) {
            return None;
        }
        let v = naming::pnext_variant(name);
        let val = *value as i32;
        Some(quote!(#val => sv_proto::PNextLink::#v(Box::new(Default::default())),))
    });

    quote! {
        #(#lower_helpers)*

        /// Walk a `pNext` chain that is **output storage**, taking the tags and
        /// nothing else.
        ///
        /// # Venus's three-state validity, applied to a chain
        ///
        /// docs/design.md §6.1 says a `returnedonly` structure's members are the
        /// *caller's uninitialised memory* on the way out, so the app side sends
        /// the partial form and does not read them. `emit_lower_struct` honours
        /// that member by member — and then handed the whole `pNext` chain to
        /// [`lower_pnext`], which lowers every link it recognises **in full**.
        /// The tags are real; everything hanging off them is whatever was on the
        /// caller's stack.
        ///
        /// That is docs/design.md §0.3 item 2's hazard for the third time, and
        /// it is what crashed Firefox on `vkGetPhysicalDeviceProperties2`,
        /// reached from the loader's `linux_read_sorted_physical_devices`
        /// (§0.24). What the peer needs from an output chain is only *which
        /// links are there*, so that it can fill them; the contents are its
        /// answer, not the caller's question.
        ///
        /// # Safety
        /// As [`lower_pnext`]: `head` is null or a chain of
        /// `VkBaseInStructure`-prefixed structs living for the call. This
        /// function reads strictly less than that contract allows — two fields
        /// of each link and no further.
        pub unsafe fn lower_pnext_tags(
            head: *const core::ffi::c_void,
            allowed: fn(i32) -> bool,
        ) -> crate::LowerResult<Vec<sv_proto::PNextLink>> {
            let mut out = Vec::new();
            let mut __p = head;
            let mut __guard = 0usize;
            while !__p.is_null() {
                __guard += 1;
                if __guard > crate::MAX_PNEXT_LINKS {
                    return Err(crate::LowerError::ChainTooLong);
                }
                let __base = __p as *const VkBaseInStructure;
                let (__s_type, __next) = unsafe { ((*__base).s_type.0, (*__base).p_next) };
                if allowed(__s_type) {
                    let link = match __s_type {
                        #(#tag_arms)*
                        _ => { __p = __next as *const core::ffi::c_void; continue; }
                    };
                    out.push(link);
                }
                __p = __next as *const core::ffi::c_void;
            }
            Ok(out)
        }

        /// Walk a C `pNext` chain into the wire's flat, length-prefixed form.
        ///
        /// `allowed` is the generated per-parent membership predicate. A link
        /// whose `sType` the parent does not accept, or that this build does not
        /// know, is **dropped and the walk continues** — docs/design.md §6.1's
        /// asymmetry: "the app side silently drops unknown links and continues
        /// down `pNext`; the GPU side treats one as fatal".
        ///
        /// # Safety
        /// `head` is null or points at a chain of `VkBaseInStructure`-prefixed
        /// structs, each of which lives for the duration of the call. That is the
        /// Vulkan specification's contract; the loop bound below is not part of
        /// it, and exists so that a cyclic chain is an error rather than a hang.
        pub unsafe fn lower_pnext(
            head: *const core::ffi::c_void,
            allowed: fn(i32) -> bool,
        ) -> crate::LowerResult<Vec<sv_proto::PNextLink>> {
            let mut out = Vec::new();
            let mut __p = head;
            let mut __guard = 0usize;
            while !__p.is_null() {
                __guard += 1;
                if __guard > crate::MAX_PNEXT_LINKS {
                    return Err(crate::LowerError::ChainTooLong);
                }
                let __base = __p as *const VkBaseInStructure;
                let (__s_type, __next) = unsafe { ((*__base).s_type.0, (*__base).p_next) };
                if allowed(__s_type) {
                    let link = unsafe {
                        match __s_type {
                            #(#lower_arms)*
                            _ => {
                                __p = __next as *const core::ffi::c_void;
                                continue;
                            }
                        }
                    };
                    out.push(link);
                }
                __p = __next as *const core::ffi::c_void;
            }
            Ok(out)
        }

        /// Fill in the application's own `pNext` chain from a reply.
        ///
        /// The C chain is the authority on what to write: we walk *it*, and for
        /// each link look for a reply link of the same `sType`. A reply link the
        /// application did not ask for has nowhere to go and is dropped; a chain
        /// link the peer did not answer is left exactly as the application left
        /// it. Both are the conservative direction.
        ///
        /// # Safety
        /// As [`lower_pnext`], with writes: each chain link is valid for writes
        /// of the struct its `sType` names. That is the specification's
        /// contract for an output chain.
        pub unsafe fn store_pnext(
            links: &[sv_proto::PNextLink],
            head: *mut core::ffi::c_void,
        ) {
            let mut __p = head;
            let mut __guard = 0usize;
            while !__p.is_null() {
                __guard += 1;
                if __guard > crate::MAX_PNEXT_LINKS {
                    return;
                }
                let __base = __p as *mut VkBaseOutStructure;
                let (__s_type, __next) = unsafe { ((*__base).s_type.0, (*__base).p_next) };
                if let Some(__link) = links.iter().find(|l| l.s_type() == __s_type) {
                    unsafe {
                        match (__s_type, __link) {
                            #(#store_arms)*
                            _ => {}
                        }
                    }
                }
                __p = __next as *mut core::ffi::c_void;
            }
        }
    }
}

/// What the generator refused to emit, and why.
#[derive(Debug, Clone, Default)]
pub struct Refusals {
    /// Structs with no `lower_*`, keyed by name.
    pub lower: BTreeMap<String, String>,
    /// Structs with no `store_*`.
    pub store: BTreeMap<String, String>,
    /// Commands with a stub that refuses.
    pub commands: Vec<Refusal>,
}

/// Not a refusal: a struct that simply never travels in this direction.
const NOT_NEEDED: &str = "not needed in this direction";

/// Close a refusal set under containment.
///
/// A struct whose lowering was refused makes every struct that contains it
/// unlowerable too. Iterating to a fixpoint is what makes it impossible for the
/// emitted code to call a function that was never emitted — the alternative is a
/// generated crate that does not compile, which is the failure mode
/// docs/design.md §7.3 spends `syn::parse2` on avoiding one layer up.
fn close<'a, F, W>(cx: &Ctx<'a>, wanted: W, emit: F) -> (BTreeMap<String, String>, Ctx<'a>)
where
    F: Fn(&Ctx<'_>, &StructPlan) -> Result<TokenStream, String>,
    W: Fn(&StructPlan) -> bool,
{
    let mut refused: BTreeMap<String, String> = BTreeMap::new();
    for s in &cx.wire.structs {
        if cx.shared.contains(&s.name) && !wanted(s) {
            refused.insert(s.name.clone(), NOT_NEEDED.to_string());
        }
    }
    loop {
        let narrowed = cx.without(&refused);
        let mut round = Vec::new();
        for s in &cx.wire.structs {
            if !narrowed.shared.contains(&s.name) {
                continue;
            }
            if let Err(e) = emit(&narrowed, s) {
                round.push((s.name.clone(), e));
            }
        }
        if round.is_empty() {
            return (refused, narrowed);
        }
        refused.extend(round);
    }
}

/// Emit the whole `sv-icd-entry` crate body, as labelled sections.
pub fn emit_all(cx: &Ctx<'_>) -> (Vec<(String, TokenStream)>, Refusals) {
    let types = emit_types(cx);

    let (lower_refused, lower_cx) = close(cx, |_| true, emit_lower_struct);
    // Only structs the implementation can fill in need a writeback. That set is
    // exactly `needs_partial`: docs/design.md §6.1's three-state validity marks a
    // struct partial when it is `returnedonly` or reachable from a command
    // output, which is the same question as "could this ever come back". Without
    // the filter the refusal list is 82 entries long and every one of them is a
    // creation struct nobody would ever write back, which buries the two that
    // matter.
    let (store_refused, store_cx) = close(cx, |s| s.needs_partial, emit_store_struct);

    let lowers: Vec<TokenStream> = cx
        .wire
        .structs
        .iter()
        .filter(|s| lower_cx.shared.contains(&s.name))
        .map(|s| emit_lower_struct(&lower_cx, s).expect("closed above"))
        .collect();
    // The out-parameter form of every struct the app side can lower: the chain's
    // tags and nothing else. See the `field.is_output` arm of `emit_lowering`
    // for why direction and not `returnedonly` decides which is used.
    let lower_outs: Vec<TokenStream> = cx
        .wire
        .structs
        .iter()
        .filter(|s| lower_cx.shared.contains(&s.name) && !s.is_union)
        .map(|s| {
            let f = lower_out_struct_fn(&s.name);
            let ty = naming::type_ident(&s.name);
            let inits = s.fields.iter().map(|field| {
                let id = naming::ident(&field.rust_name);
                if matches!(field.shape, Shape::PNext) {
                    let allowed = crate::emit::pnext::allowed_fn_ident(&s.name);
                    quote!(#id: lower_pnext_tags(
                        (*p).#id as *const core::ffi::c_void,
                        sv_proto::#allowed,
                    )?,)
                } else {
                    let d = wire_default(&field.shape);
                    quote!(#id: #d,)
                }
            });
            let doc = format!(
                "Lower a C `{}` that the implementation is about to **fill**.\n\n\
                 Every member but the `pNext` chain is the caller's \
                 uninitialised memory on the way out (docs/design.md §6.1), so \
                 every member but the chain is a default. The chain itself \
                 yields its tags, which is the one thing the peer needs: which \
                 links are there for it to write into.",
                s.name
            );
            quote! {
                #[doc = #doc]
                #[allow(unused_unsafe, unused_variables)]
                pub unsafe fn #f(p: *const #ty) -> crate::LowerResult<sv_proto::#ty> {
                    let _ = p;
                    unsafe { Ok(sv_proto::#ty { #(#inits)* }) }
                }
            }
        })
        .collect();
    let stores: Vec<TokenStream> = cx
        .wire
        .structs
        .iter()
        .filter(|s| store_cx.shared.contains(&s.name))
        .map(|s| emit_store_struct(&store_cx, s).expect("closed above"))
        .collect();

    let mut command_items = Vec::new();
    let mut commands = Vec::new();
    for c in &cx.abi.commands {
        if HAND_WRITTEN.contains(&c.name.as_str()) {
            // The tables point at `crate::local::…` for these; generating a
            // second stub of the same name would shadow nothing and confuse
            // everything.
            continue;
        }
        let (item, refusal) = emit_command(&lower_cx, &store_cx, c);
        command_items.push(item);
        if let Some(r) = refusal {
            commands.push(r);
        }
    }
    let refused_names: BTreeSet<String> = commands.iter().map(|r| r.command.clone()).collect();

    let pnext = emit_pnext_walkers(&lower_cx, &store_cx);
    let forward = emit_forward_module(cx);
    let icd_trait = emit_icd_trait(cx);
    let tables = emit_tables(cx, &refused_names);
    let probes = emit_audit_probes(cx);

    // Sections are kept separate rather than concatenated, because when the
    // whole stream does not parse the only useful question is "which item", and
    // a 1.7 MB single line answers it very slowly. `generate_icd_entry` re-parses
    // section by section on failure.
    let mut sections = vec![("types".to_string(), types)];
    for (s, t) in cx.wire.structs.iter().filter(|s| lower_cx.shared.contains(&s.name)).zip(lowers) {
        sections.push((format!("lower {}", s.name), t));
    }
    for (s, t) in cx
        .wire
        .structs
        .iter()
        .filter(|s| lower_cx.shared.contains(&s.name) && !s.is_union)
        .zip(lower_outs)
    {
        sections.push((format!("lower_out {}", s.name), t));
    }
    for (s, t) in cx.wire.structs.iter().filter(|s| store_cx.shared.contains(&s.name)).zip(stores) {
        sections.push((format!("store {}", s.name), t));
    }
    sections.push(("pnext".to_string(), pnext));
    for (c, t) in cx
        .abi
        .commands
        .iter()
        .filter(|c| !HAND_WRITTEN.contains(&c.name.as_str()))
        .zip(command_items)
    {
        sections.push((format!("stub {}", c.name), t));
    }
    sections.push(("forward".to_string(), forward));
    sections.push(("trait".to_string(), icd_trait));
    sections.push(("tables".to_string(), tables));
    sections.push(("audit probes".to_string(), probes));

    let keep = |m: BTreeMap<String, String>| -> BTreeMap<String, String> {
        m.into_iter().filter(|(_, why)| why != NOT_NEEDED).collect()
    };
    (sections, Refusals { lower: keep(lower_refused), store: keep(store_refused), commands })
}

/// The loop index for one level of a fixed-size array, distinct per level.
///
/// # The two-dimensional array, met a third time
///
/// `StdVideoH264ScalingLists::ScalingList4x4` is `uint8_t[6][16]`. Every
/// fixed-array loop was `for __i in 0..n`, so the inner loop's `__i` shadowed
/// the outer one and the element read was `ScalingList4x4[__i][__i]` with the
/// inner index — out of bounds at 6 on the first dimension. It was found by
/// ffmpeg's Vulkan H.264 decoder through the forwarder (a stream with scaling
/// matrices reaches it; `video-probe`'s fixtures never set them), as a panic in
/// `sv_vk_cmd_decode_video_khr` (docs/video-decode.md §7). The same shadowing
/// was in the GPU side's `raise` and in the app side's writeback, and all
/// three use this.
///
/// Named by how many fixed-array levels lie *below* this one (`__f0` for the
/// innermost), and never `__i`, which the pointer-array loops use: an array of
/// fixed arrays would otherwise shadow its outer `.add(__i)` the same way.
pub(crate) fn fixed_index(elem: &Shape) -> proc_macro2::Ident {
    fn below(s: &Shape) -> usize {
        match s {
            Shape::FixedArray { elem, .. } => 1 + below(elem),
            _ => 0,
        }
    }
    format_ident!("__f{}", below(elem))
}
