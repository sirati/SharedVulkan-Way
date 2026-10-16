//! Pass 12: `sv-gpu-entry` — **wire → C**, and the driver call on the far side.
//!
//! # The pass docs/design.md §7.2 does not have, and why it is not optional
//!
//! §7.2's taxonomy has four corners of a two-by-two — *which side* by *which
//! direction* — and names three of them:
//!
//! ```text
//!                   C ──▶ wire                     wire ──▶ C
//!   app side    pass 10 `lower_*`   (icd_entry)   pass 10 `store_*` (icd_entry)
//!   GPU side    pass 10 `lower_*`   (reusable!)   ***missing***
//! ```
//!
//! The bottom-left corner needs no new code: lowering a C struct the *driver*
//! filled in is byte-for-byte the same problem as lowering one the
//! *application* filled in, and [`crate::emit::icd_entry`]'s `lower_*` does it
//! already. The bottom-right corner is genuinely new, and without it the GPU
//! side cannot call a driver at all: every `vkCreate*` needs a `const
//! VkFooCreateInfo*`, and what arrived is an owned Rust value.
//!
//! The previous stage wrote that corner by hand for about forty commands and
//! recorded, correctly, that it would not scale to 239. This pass is that
//! finding acted on.
//!
//! # Pass 4 is not dead, it moved
//!
//! docs/design.md §7.2 strikes out pass 4 (`arena_bind`) with the note that owned
//! Rust values make it a no-op — "our decoder produces owned Rust values whose
//! storage *is* their lifetime, so there is no binding step to generate". That
//! is true of *decoding* and false of *raising*, which is the operation
//! gfxstream's `deepcopy` actually serves. A C `VkGraphicsPipelineCreateInfo`
//! holds eight pointers to sub-structs that must outlive the call, and
//! producing them is exactly gfxstream's pool-allocated deep copy.
//!
//! So pass 4 exists after all, it is generated here, and its pool is
//! [`sv_gpu_entry::Raise`] — a `Vec<Box<dyn Any>>` rather than a C++ arena,
//! because `Box` is what gives a stable address in safe Rust.
//!
//! # The raise pass contains no `unsafe`, which was not obvious
//!
//! Creating a raw pointer is safe; only dereferencing one is not. Every
//! `raise_*` function here allocates owned storage and *takes its address* —
//! it never reads through a pointer. So the whole of the largest half of this
//! crate is safe Rust, and the `unsafe` is confined to the one line per command
//! that calls the driver's function pointer. That is a better ratio than the app
//! side manages, where lowering has to read the application's memory.
//!
//! # What is emitted
//!
//! ```text
//!   raise_<Struct>    sv_proto::Struct  ──▶ C Struct           (safe)
//!   raise_pnext       Vec<PNextLink>    ──▶ *const c_void      (safe)
//!   DriverFns         every command's entry point, by name     (unsafe to load)
//!   impl Server for Forward<H>          decode ▶ map ▶ raise ▶ call ▶ lower
//! ```
//!
//! and, for the commands a GPU side must give meaning to rather than forward
//! (the arena, the session, submission), a call out to [`GpuHost`], whose method
//! list is [`crate::exceptions::GPU_HAND_WRITTEN`].

use crate::abi::{AbiCommand, AbiMember, AbiPlan, AbiType, ArrayLen};
use crate::naming;
use crate::plan::{CommandPlan, FieldPlan, Plan, Scalar, Shape, StructPlan};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::collections::{BTreeMap, BTreeSet};
use vk_registry::{CType, CTypeKind, Model};

use super::icd_entry::{Ctx, c_param_type, c_type};

/// `VkBufferCreateInfo` → `raise_vk_buffer_create_info`.
fn raise_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("raise_{}", naming::snake_case(name))
}

/// `vkCreateBuffer` → the field of [`DriverFns`] holding its entry point.
fn fn_field(name: &str) -> proc_macro2::Ident {
    naming::ident(&naming::snake_case(name))
}

/// Handles a `vkDestroy*`/`vkFree*` call names but does **not** destroy.
///
/// `vkDestroyBuffer(device, buffer, …)` destroys the buffer and not the device;
/// `vkFreeCommandBuffers(device, commandPool, count, pBuffers)` frees the
/// buffers and not the pool. Without this list the generated unbind would drop
/// the device out of the table on the first `vkDestroyBuffer`, and the next
/// command would fail to resolve it — which is a failure mode worth naming
/// because it is silent until the *second* call.
const PARENT_HANDLES: &[&str] = &[
    "VkInstance",
    "VkPhysicalDevice",
    "VkDevice",
    "VkQueue",
    "VkCommandPool",
    "VkDescriptorPool",
];

/// Why a command could not be forwarded mechanically.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub command: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct Refusals {
    /// Structs with no `raise_*`, keyed by name.
    pub raise: BTreeMap<String, String>,
    pub commands: Vec<Refusal>,
}

// ---------------------------------------------------------------------------
// raising a value
// ---------------------------------------------------------------------------

/// A C-valued expression producing `shape` from the wire lvalue `access`.
///
/// `access` is a *place* expression of the wire type (`v.p_bindings`,
/// `(*inner)`), and the result is a value of the corresponding C type. `__a` is
/// the [`Raise`] pool in scope.
fn raise_expr(
    cx: &Ctx<'_>,
    shape: &Shape,
    abi: &AbiMember,
    access: &TokenStream,
    owner: &str,
) -> Result<TokenStream, String> {
    Ok(match shape {
        Shape::Scalar(s) => raise_scalar(cx, s, access, owner)?,
        Shape::FixedArray { elem, count, .. } => {
            let n = *count as usize;
            // The element type is the C one, and it is not always the wire one:
            // `char deviceName[256]` is `[i8; 256]` in C and `[u8; 256]` on the
            // wire. Building through a `Vec` and converting keeps the `raise`
            // of each element in one place, and `try_from` cannot fail because
            // the loop runs exactly `n` times.
            //
            // The recursion peels one array level off the *member's own C type*
            // as it descends, rather than asking the same member for its
            // element type at every depth. That distinction only shows up with
            // more than one dimension, and `StdVideoH264ScalingLists` —
            // `uint8_t ScalingList4x4[6][16]` — is the first member in any
            // profile that has two. Asking the whole member gave both levels
            // `[u8; 16]`, so the inner loop pushed a `u8` into a
            // `Vec<[u8; 16]>`: caught by the compiler, but only once a profile
            // reached it.
            let inner_abi = AbiMember { ty: peel_array(&abi.ty)?, ..abi.clone() };
            let i = super::icd_entry::fixed_index(elem);
            let inner = raise_expr(cx, elem, &inner_abi, &quote!((#access)[#i]), owner)?;
            let cty = fixed_elem_c_type(cx, &abi.ty)?;
            quote!({
                let mut __t: Vec<#cty> = Vec::with_capacity(#n);
                for #i in 0..#n { __t.push(#inner); }
                match <[#cty; #n]>::try_from(__t) {
                    Ok(__x) => __x,
                    // Unreachable: the loop pushed exactly #n elements.
                    Err(_) => return Err(sv_wire::WireError::Truncated { need: #n, have: 0 }),
                }
            })
        }
        // Everything below produces a pointer, and the C member it lands in may
        // be `*const T` or `*mut T` depending on whether the struct is
        // `returnedonly`. `as _` reconciles the two; it is a constness cast and
        // nothing else, since the pointee type is derived from the same member.
        // **These two peel the member's C type as they descend, for the reason
        // `FixedArray` above gives about `StdVideoH264ScalingLists` — and they
        // did not, which is a one-dimension-out bug that only a doubly indirect
        // member can reach.** `const VkAccelerationStructureGeometryKHR* const*
        // ppGeometries` is `Pointer(Pointer(T))`, so the `Array` arm handed its
        // element the *whole* member and the element's `Optional` arm peeled one
        // level to `*const T` where the value is a `T` — `let __v: *const T =
        // raise_…()` against a function returning `T`.
        //
        // Nothing in any profile was doubly indirect until
        // `VK_KHR_acceleration_structure` came in behind the union walkers
        // (docs/design.md §0.21), which is why a pass that has marshalled half a
        // million lines had never met it. The fix is the same one line
        // `FixedArray` already carries, and the lesson is that it was written as
        // a special case for two-dimensional *arrays* when it is really the rule
        // for every level of indirection.
        Shape::Optional(inner) => {
            let deref = quote!(*__x);
            let inner_abi = AbiMember { ty: peel_array(&abi.ty)?, ..abi.clone() };
            let e = raise_expr(cx, inner, &inner_abi, &deref, owner)?;
            let cty = pointee_c_type(cx, &abi.ty)?;
            // A pointer to one, allocated in the pool so it outlives the call.
            quote!(match &(#access) {
                Some(__x) => { let __v: #cty = #e; __a.one(__v) as _ }
                None => core::ptr::null_mut::<#cty>() as _,
            })
        }
        Shape::Array { elem, .. } => {
            let inner_abi = AbiMember { ty: peel_array(&abi.ty)?, ..abi.clone() };
            let inner = raise_expr(cx, elem, &inner_abi, &quote!(*__x), owner)?;
            let cty = pointee_c_type(cx, &abi.ty)?;
            // An empty array becomes a **null** pointer rather than a dangling
            // one. Vulkan permits null whenever the count is zero, and a driver
            // that null-checks before consulting the count — several do — reads
            // the dangling address otherwise.
            quote!({
                let mut __t: Vec<#cty> = Vec::with_capacity((#access).len());
                for __x in (#access).iter() { __t.push(#inner); }
                if __t.is_empty() {
                    core::ptr::null_mut::<#cty>() as _
                } else {
                    __a.slice(__t) as _
                }
            })
        }
        // A blob's C type is whatever the header says — `void*` for
        // `pInitialData`, `const uint32_t*` for SPIR-V. The bytes are the same
        // either way; the pool gives eight-byte alignment, which covers every
        // element type the registry uses behind a sized `void*`.
        Shape::Blob { .. } => quote!(__a.blob(&(#access)) as _),
        Shape::CString => quote!(__a.cstring(&(#access)) as _),
        Shape::CStringArray { .. } => quote!({
            let mut __t: Vec<*const core::ffi::c_char> = Vec::with_capacity((#access).len());
            for __x in (#access).iter() { __t.push(__a.cstring(__x)); }
            if __t.is_empty() {
                core::ptr::null_mut::<*const core::ffi::c_char>() as _
            } else {
                __a.slice(__t) as _
            }
        }),
        Shape::PNext => quote!(raise_pnext(&(#access), &mut *__a)? as _),
    })
}

fn raise_scalar(
    cx: &Ctx<'_>,
    s: &Scalar,
    access: &TokenStream,
    owner: &str,
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
        | Scalar::Size => quote!((#access) as _),
        Scalar::F32 | Scalar::F64 => quote!(#access),
        Scalar::Bool32 => quote!(if #access { 1 } else { 0 }),
        // A `void*` the policy table calls opaque: on the wire it is a `u64`,
        // and the only meaning it can have on this side is "not a pointer we
        // are permitted to reconstruct". Zero it rather than casting a number
        // out of another address space into a pointer.
        Scalar::OpaqueU64(_) => quote!({ let _ = #access; Default::default() }),
        Scalar::Enum(n) | Scalar::Bitmask(n) | Scalar::Bitmask64(n) => {
            let t = naming::type_ident(n);
            quote!(#t((#access).0 as _))
        }
        Scalar::Handle(n) => {
            // By this point the handle map has already replaced the app side's
            // object id with the driver's own handle, so what `.0` holds is a
            // driver handle — a `u64` for a non-dispatchable one and a pointer
            // for a dispatchable one, both stored as `u64` in the table.
            if cx.is_dispatchable(n) {
                quote!((#access).0 as usize as _)
            } else {
                quote!((#access).0)
            }
        }
        Scalar::Struct(n) | Scalar::Union(n) => {
            if !raisable(cx, n) {
                return Err(format!("{owner}: {n} has no C counterpart"));
            }
            let f = raise_fn(n);
            quote!(#f(&(#access), &mut *__a)?)
        }
    })
}

/// Whether a wire struct has a C counterpart this pass may emit a `raise_*` for.
fn raisable(cx: &Ctx<'_>, name: &str) -> bool {
    cx.shared_struct(name)
}

/// The C element type behind a pointer member, for sizing pool storage.
fn pointee_c_type(cx: &Ctx<'_>, ty: &CType) -> Result<TokenStream, String> {
    match &ty.kind {
        CTypeKind::Pointer(inner) => Ok(c_type(cx, inner)),
        _ => Err(format!("expected a pointer, got {ty:?}")),
    }
}

/// The C element type of a fixed array member.
/// One array (or decayed-array pointer) level off a C type.
///
/// The type-level companion of [`fixed_elem_c_type`], which produces the
/// *rendered* element type; this produces the `CType` the next level of the
/// recursion should ask its questions of.
fn peel_array(ty: &CType) -> Result<CType, String> {
    match &ty.kind {
        CTypeKind::Array(inner, _) | CTypeKind::Pointer(inner) => Ok((**inner).clone()),
        _ => Err(format!("expected an array, got {ty:?}")),
    }
}

fn fixed_elem_c_type(cx: &Ctx<'_>, ty: &CType) -> Result<TokenStream, String> {
    match &ty.kind {
        CTypeKind::Array(inner, _) => Ok(c_type(cx, inner)),
        // A decayed array *parameter*: `const float blendConstants[4]` arrives
        // as `const float*`. Raising still produces four floats and takes their
        // address, so the element type is the pointee.
        CTypeKind::Pointer(inner) => Ok(c_type(cx, inner)),
        _ => Err(format!("expected an array, got {ty:?}")),
    }
}

/// `raise_<Struct>(v: &sv_proto::Struct, __a: &mut Raise) -> Result<CStruct>`.
fn emit_raise_struct(cx: &Ctx<'_>, s: &StructPlan) -> Result<TokenStream, String> {
    let name = &s.name;
    let abi_members: BTreeMap<&str, &AbiMember> = match cx.abi.type_named(name) {
        Some(AbiType::Struct { members, .. }) => {
            members.iter().map(|m| (m.name.as_str(), m)).collect()
        }
        _ => return Err(format!("{name} is not a struct on the C surface")),
    };
    let f = raise_fn(name);
    let cty = naming::type_ident(name);
    let wty = naming::type_ident(name);

    if s.is_union {
        // A C union is written through whichever member the wire value names.
        // The Rust enum carries the tag the C type does not, so unlike the
        // lowering direction there is nothing to guess: the value says which
        // member it is.
        let mut arms = Vec::new();
        for field in &s.fields {
            let v = naming::union_variant(&field.name);
            let abi = abi_members
                .get(field.name.as_str())
                .ok_or_else(|| format!("{name}.{}: no C member", field.name))?;
            let id = naming::ident(&field.rust_name);
            let e = raise_expr(cx, &field.shape, abi, &quote!(*__x), name)?;
            arms.push(quote! {
                sv_proto::#wty::#v(__x) => { __c.#id = #e; }
            });
        }
        let doc = format!(
            "Raise a wire `{name}` into its C form.\n\n\
             Unlike lowering, which has to pick a member because a C union \
             carries no tag, this direction is exact: the Rust enum *is* the \
             tag (docs/design.md §7.4)."
        );
        return Ok(quote! {
            #[doc = #doc]
            #[allow(unused_variables, clippy::needless_update)]
            pub fn #f(v: &sv_proto::#wty, __a: &mut crate::Raise) -> sv_wire::Result<#cty> {
                let mut __c = <#cty as Default>::default();
                match v { #(#arms)* }
                Ok(__c)
            }
        });
    }

    let wire_fields: BTreeMap<&str, &FieldPlan> =
        s.fields.iter().map(|f| (f.name.as_str(), f)).collect();

    let mut stmts = Vec::new();
    // The ABI's member order is the C header's, which is the order a reader
    // checks against `vulkan_core.h`.
    for (mname, abi) in &abi_members {
        let cid = naming::ident(&abi.rust_name);
        if *mname == "sType" {
            // `sType` is not a wire field at all — dropping it from the Rust
            // type is what makes "a value with the wrong sType" unrepresentable
            // (docs/design.md §7.2). It is reconstructed here from the registry.
            let Some(v) = s.s_type_value else {
                return Err(format!("{name} has an sType member but no value in the registry"));
            };
            let v = v as i32;
            stmts.push(quote!(__c.#cid = VkStructureType(#v);));
            continue;
        }
        let Some(field) = wire_fields.get(mname) else {
            return Err(format!("{name}.{mname}: no wire field"));
        };
        let access = {
            let id = naming::ident(&field.rust_name);
            quote!(v.#id)
        };
        let e = raise_expr(cx, &field.shape, abi, &access, name)?;
        stmts.push(quote!(__c.#cid = #e;));
    }

    let doc = format!(
        "Raise a wire `{name}` into the C struct a driver can be handed.\n\n\
         Pointer-valued members are allocated in `__a` and live as long as it \
         does, which is docs/design.md §7.2's pass 4 doing the job it was struck out \
         for not having."
    );
    Ok(quote! {
        #[doc = #doc]
        #[allow(unused_variables, clippy::needless_update)]
        pub fn #f(v: &sv_proto::#wty, __a: &mut crate::Raise) -> sv_wire::Result<#cty> {
            let mut __c = <#cty as Default>::default();
            #(#stmts)*
            Ok(__c)
        }
    })
}

/// The `pNext` chain walker, wire → C.
///
/// Built back to front so each link's `pNext` is known when the link itself is
/// allocated. A link this build does not know is dropped, which is the only
/// thing it can be — an unknown link has no C type to allocate.
fn emit_raise_pnext(cx: &Ctx<'_>) -> TokenStream {
    // **One `#[inline(never)]` helper per link, and arms that are tail calls**
    // (docs/design.md §0.27). As `let mut __l: T = raise_x(..)?;` inside the
    // `match`, every arm's C struct and `?` had its own slot in `raise_pnext`'s
    // frame in a debug build: 204 KB, on the thread that serves a session.
    let raisable_links: Vec<&(String, i64)> =
        cx.wire.pnext_links.iter().filter(|(name, _)| raisable(cx, name)).collect();
    let helpers = raisable_links.iter().map(|(name, _)| {
        let f = raise_fn(name);
        let ty = naming::type_ident(name);
        let helper = format_ident!("raise_link_{}", naming::snake_case(name));
        quote! {
            #[inline(never)]
            fn #helper(
                __inner: &sv_proto::#ty,
                __next: *const core::ffi::c_void,
                __a: &mut crate::Raise,
            ) -> sv_wire::Result<*const core::ffi::c_void> {
                let mut __l: #ty = #f(__inner, &mut *__a)?;
                // `pNext` is `const void*` in an input struct and `void*` in a
                // `returnedonly` one, and both occur in the same chain.
                __l.p_next = __next as _;
                Ok(__a.one::<#ty>(__l) as *const core::ffi::c_void)
            }
        }
    });
    let arms = raisable_links.iter().map(|(name, _)| {
        let v = naming::pnext_variant(name);
        let helper = format_ident!("raise_link_{}", naming::snake_case(name));
        quote!(sv_proto::PNextLink::#v(__inner) => #helper(__inner, __next, __a),)
    });
    quote! {
        #(#helpers)*

        /// One link, raised in front of `__next`. Every arm is a tail call.
        fn raise_link(
            __link: &sv_proto::PNextLink,
            __next: *const core::ffi::c_void,
            __a: &mut crate::Raise,
        ) -> sv_wire::Result<*const core::ffi::c_void> {
            match __link {
                #(#arms)*
                _ => Ok(__next),
            }
        }

        /// Rebuild a C `pNext` chain from the wire's flat, length-prefixed form.
        ///
        /// The chain is built **back to front**: a C link holds a pointer to the
        /// next one, so the tail has to exist before the head can point at it.
        ///
        /// A link whose `sType` this build does not know is skipped rather than
        /// forwarded. That is not the app side's leniency (docs/design.md §6.1 —
        /// "the app side silently drops unknown links") reappearing on the wrong
        /// side: an unknown link is *already* fatal at the decoder here, so
        /// anything reaching this function is known to the protocol and merely
        /// has no `raise_*`, which the generator's own closure makes
        /// unreachable.
        pub fn raise_pnext(
            links: &[sv_proto::PNextLink],
            __a: &mut crate::Raise,
        ) -> sv_wire::Result<*const core::ffi::c_void> {
            let mut __next: *const core::ffi::c_void = core::ptr::null();
            for __link in links.iter().rev() {
                __next = raise_link(__link, __next, &mut *__a)?;
            }
            Ok(__next)
        }
    }
}

// ---------------------------------------------------------------------------
// the driver's function table
// ---------------------------------------------------------------------------

/// The type of one driver entry point, as an `extern "system"` fn pointer.
fn fn_ptr_type(cx: &Ctx<'_>, c: &AbiCommand) -> TokenStream {
    let params = c.params.iter().map(|p| c_param_type(cx, &p.ty));
    if c.returns_void {
        quote!(unsafe extern "system" fn(#(#params),*))
    } else {
        let r = c_type(cx, &c.return_type);
        quote!(unsafe extern "system" fn(#(#params),*) -> #r)
    }
}

fn emit_driver_fns(cx: &Ctx<'_>, forwardable: &BTreeSet<String>) -> TokenStream {
    let cmds: Vec<&AbiCommand> =
        cx.abi.commands.iter().filter(|c| forwardable.contains(&c.name)).collect();

    let fields = cmds.iter().map(|c| {
        let id = fn_field(&c.name);
        let ty = fn_ptr_type(cx, c);
        let doc = format!("`{}`", c.name);
        quote! { #[doc = #doc] pub #id: Option<#ty>, }
    });
    let defaults = cmds.iter().map(|c| {
        let id = fn_field(&c.name);
        quote!(#id: None,)
    });
    let loads = cmds.iter().map(|c| {
        let id = fn_field(&c.name);
        let ty = fn_ptr_type(cx, c);
        let name = c.name.as_str();
        // A device-level command is looked up through `vkGetDeviceProcAddr`
        // when there is a device: the loader's own documentation is emphatic
        // that the device table is the one without a trampoline, and on a
        // multi-device driver the instance answer is a dispatch stub.
        let getter = if c.level.is_device() {
            quote!(__dev)
        } else {
            quote!(__inst)
        };
        quote! {
            #id: #getter(#name).map(|__p| unsafe {
                core::mem::transmute::<
                    unsafe extern "system" fn(),
                    #ty,
                >(__p)
            }),
        }
    });
    let names = cmds.iter().map(|c| {
        let id = fn_field(&c.name);
        let n = c.name.as_str();
        quote!(if self.#id.is_none() { __missing.push(#n); })
    });

    quote! {
        /// Every entry point the GPU side may need from the real driver.
        ///
        /// Loaded by name rather than through `ash`'s typed wrappers, and that
        /// is deliberate: the whole point of this crate is that the C signature
        /// comes from the same `vk.xml` the wire format does, so the argument
        /// list a driver is called with is derived from the registry rather than
        /// from a second hand-maintained binding. A disagreement between the two
        /// would be a stack corruption with no diagnostic.
        #[allow(non_snake_case)]
        pub struct DriverFns {
            #(#fields)*
        }

        impl Default for DriverFns {
            fn default() -> Self {
                Self { #(#defaults)* }
            }
        }

        impl DriverFns {
            /// Resolve every entry point.
            ///
            /// `__inst` answers instance- and physical-device-level names,
            /// `__dev` device-level ones. Both return `None` for a name the
            /// driver does not have, which is normal — the profile is a superset
            /// of any one driver's extensions.
            pub fn load(
                __inst: &dyn Fn(&str) -> Option<unsafe extern "system" fn()>,
                __dev: &dyn Fn(&str) -> Option<unsafe extern "system" fn()>,
            ) -> DriverFns {
                DriverFns { #(#loads)* }
            }

            /// Which entry points the driver did not supply.
            ///
            /// Reported once at startup rather than discovered as a refusal per
            /// command, because "this driver has no `vkCmdBeginRendering`" is a
            /// fact about the configuration and an application should be told
            /// before it has drawn nothing for ten seconds.
            pub fn missing(&self) -> Vec<&'static str> {
                let mut __missing = Vec::new();
                #(#names)*
                __missing
            }
        }
    }
}

// ---------------------------------------------------------------------------
// per-command forwarding
// ---------------------------------------------------------------------------

/// How an output parameter is served.
enum Out {
    /// `VkBuffer* pBuffer`: one value, and if it is a handle the app side's id
    /// must be bound to it afterwards.
    One { c_ty: TokenStream, handle: Option<String> },
    /// `VkPhysicalDevice* pDevices` with `uint32_t* pCount`: an array whose
    /// capacity the caller stated and whose real length comes back.
    Array { c_ty: TokenStream, handle: Option<String>, count: Option<String> },
    /// `uint64_t* pValue`: one number, not a struct. `vkGetSemaphoreCounterValue`
    /// and four others.
    Scalar { c_ty: TokenStream, back: Scalar },
    /// A blob, held as bytes. `count` is the output sibling holding the driver's
    /// answer in bytes (`vkGetPipelineCacheData`); `None` when the size is an
    /// *input* the caller fixed (`vkGetQueryPoolResults`), in which case the
    /// whole buffer comes back.
    Blob { count: Option<String> },
    /// `uint32_t* pCount` itself, when some other output names it.
    Count { c_ty: TokenStream },
}

fn handle_name(shape: &Shape) -> Option<String> {
    match shape {
        Shape::Scalar(Scalar::Handle(n)) => Some(n.clone()),
        Shape::Optional(inner) | Shape::Array { elem: inner, .. } => handle_name(inner),
        _ => None,
    }
}

fn classify_output(
    cx: &Ctx<'_>,
    c: &AbiCommand,
    wire: &CommandPlan,
    abi: &AbiMember,
    field: &FieldPlan,
) -> Result<Out, String> {
    let counts: BTreeSet<&str> = wire
        .outputs
        .iter()
        .chain(wire.inputs.iter())
        .filter(|f| f.is_a_length)
        .map(|f| f.name.as_str())
        .collect();
    if counts.contains(field.name.as_str()) {
        // The C type is the authority on the width: `uint32_t*` for most
        // enumerations, `size_t*` for `vkGetPipelineCacheData`, and guessing
        // from the wire's `u64` would corrupt the caller's stack on one of them.
        return Ok(Out::Count { c_ty: pointee_c_type(cx, &abi.ty)? });
    }
    let count_sibling = match &abi.len {
        Some(ArrayLen::Sibling(s)) if wire.outputs.iter().any(|f| &f.name == s) => {
            Some(s.clone())
        }
        _ => None,
    };
    Ok(match &field.shape {
        // A scalar behind an out-pointer that is *not* somebody's length: one
        // number, and the only thing to do with it is convert it back.
        Shape::Optional(inner)
            if matches!(&**inner, Shape::Scalar(s)
                if !matches!(s, Scalar::Struct(_) | Scalar::Union(_) | Scalar::Handle(_))) =>
        {
            let Shape::Scalar(s) = &**inner else { unreachable!() };
            Out::Scalar { c_ty: pointee_c_type(cx, &abi.ty)?, back: s.clone() }
        }
        Shape::Optional(inner) => Out::One {
            c_ty: pointee_c_type(cx, &abi.ty)?,
            handle: handle_name(inner),
        },
        Shape::Array { elem, .. } => Out::Array {
            c_ty: pointee_c_type(cx, &abi.ty)?,
            handle: handle_name(elem),
            count: count_sibling,
        },
        // An output blob is `void* pData`, and `c_void` is neither `Default`
        // nor sized in a way `resize_with` can use. Bytes are what a blob is.
        Shape::Blob { .. } => Out::Blob { count: count_sibling },
        other => {
            return Err(format!("{}.{}: an output shaped {other:?}", c.name, field.name));
        }
    })
}

/// The whole body of one forwarded `Server` method.
///
/// `cx` is the *raise* context (wire → C) and `lower_cx` the *full lowering* one
/// (C → wire, reading every member). They are different closures over the same
/// plans and one command's body needs both — which is itself the finding: the
/// two directions do not have the same set of convertible structs.
fn emit_forward_command(
    cx: &Ctx<'_>,
    lower_cx: &Ctx<'_>,
    c: &AbiCommand,
    wire: &CommandPlan,
) -> Result<TokenStream, String> {
    let abi_by_name: BTreeMap<&str, &AbiMember> =
        c.params.iter().map(|p| (p.name.as_str(), p)).collect();

    // -- 0. remember what a destroy call is about to invalidate -------------
    //
    // Captured *before* the handle map, because the map overwrites the app
    // side's id with the driver's handle and the table is keyed by the id.
    let mut unbind = Vec::new();
    // The object a `vkDestroyX` destroys is an `X`, **even when `X` is on the
    // parent list** (waycompose-design.md §87). `VkCommandPool` and
    // `VkDescriptorPool` are there because `vkFreeCommandBuffers` and
    // `vkFreeDescriptorSets` name them as the parent of what they free — and
    // the same exclusion kept `vkDestroyCommandPool` and
    // `vkDestroyDescriptorPool` from ever unbinding the pool itself. Every
    // destroyed pool stayed in the table under its stale driver handle, which
    // was harmless until §87 began destroying what a session left behind: the
    // guest's Intel driver faulted on the second `vkDestroyCommandPool` of a
    // pool `video-probe` had already destroyed. The instance and the device are
    // excepted because they are this process's, not the application's.
    let destroyed = c
        .name
        .strip_prefix("vkDestroy")
        .map(|s| format!("Vk{s}"))
        .filter(|t| t != "VkInstance" && t != "VkDevice");
    if c.name.starts_with("vkDestroy") || c.name.starts_with("vkFree") {
        for f in &wire.inputs {
            let id = naming::ident(&f.rust_name);
            match &f.shape {
                Shape::Scalar(Scalar::Handle(h))
                    if !PARENT_HANDLES.contains(&h.as_str())
                        || destroyed.as_deref() == Some(h.as_str()) =>
                {
                    unbind.push(quote!(__gone.push(a.#id.0);));
                }
                Shape::Array { elem, .. } => {
                    if let Shape::Scalar(Scalar::Handle(h)) = &**elem
                        && !PARENT_HANDLES.contains(&h.as_str())
                    {
                        unbind.push(quote!(
                            for __x in a.#id.iter() { __gone.push(__x.0); }
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    let reply_ty = format_ident!("{}Reply", naming::opcode_variant(&c.name));
    let unbind_block = if unbind.is_empty() {
        quote!()
    } else {
        quote! {
            let mut __gone: Vec<u64> = Vec::new();
            #(#unbind)*
        }
    };
    let unbind_after = if unbind.is_empty() {
        quote!()
    } else {
        quote!(for __x in __gone { __b.unbind(__x); })
    };

    // -- 1. handle map and memory transform, over the inputs ----------------
    //
    // Both run on the wire value in place, before anything is raised, because
    // raising reads `.0` and expects it to already be a driver handle.
    let mut handle_pre = Vec::new();
    for f in &wire.inputs {
        let id = naming::ident(&f.rust_name);
        if let Some(stmt) = super::passes::handle_walk_pub(cx.wire, &f.shape, &quote!(a.#id)) {
            handle_pre.push(stmt);
        }
    }
    // A `(memory, offset)` pair at the top level of a parameter list, which the
    // struct walkers cannot see: `vkBindBufferMemory(device, buffer, memory,
    // memoryOffset)` is exactly this shape and it is not a struct.
    let param_names: Vec<String> = wire.inputs.iter().map(|f| f.name.clone()).collect();
    let mut mem_pre = Vec::new();
    for f in &wire.inputs {
        if !matches!(&f.shape, Shape::Scalar(Scalar::Handle(h)) if h == "VkDeviceMemory") {
            continue;
        }
        let mem = naming::ident(&f.rust_name);
        match crate::exceptions::memory_offset_sibling(&param_names, &f.name) {
            Some(off) => {
                let off_id = naming::ident(&naming::snake_case(&off));
                mem_pre.push(quote! {
                    let (__mm, __oo) = __h.rewrite_memory(a.#mem.0, a.#off_id)?;
                    a.#mem.0 = __mm;
                    a.#off_id = __oo;
                });
            }
            None => mem_pre.push(quote! {
                let (__mm, _) = __h.rewrite_memory(a.#mem.0, 0)?;
                a.#mem.0 = __mm;
            }),
        }
    }
    let mut transform_pre = Vec::new();
    for f in &wire.inputs {
        let id = naming::ident(&f.rust_name);
        if let Some(stmt) = super::passes::transform_walk_pub(cx.wire, &f.shape, &quote!(a.#id)) {
            transform_pre.push(stmt);
        }
    }
    let has_pre = !handle_pre.is_empty() || !mem_pre.is_empty() || !transform_pre.is_empty();
    // The two generated walkers want a `&mut dyn HandleMapper` and a
    // `&mut dyn MemoryTransform`, and one host cannot lend both at once. They
    // are used in separate statements, so two short-lived adapters over the same
    // host is the whole of the reconciliation.
    let handle_pre = if handle_pre.is_empty() {
        quote!()
    } else {
        quote! {
            {
                let mut __ad = crate::Mapping(&mut *__h);
                let __m: &mut dyn sv_proto::HandleMapper = &mut __ad;
                #(#handle_pre)*
            }
        }
    };
    let transform_pre = if transform_pre.is_empty() {
        quote!()
    } else {
        quote! {
            {
                let mut __ad = crate::Mapping(&mut *__h);
                let __t: &mut dyn sv_proto::MemoryTransform = &mut __ad;
                #(#transform_pre)*
            }
        }
    };
    let pre = quote! {
        #handle_pre
        #(#mem_pre)*
        #transform_pre
    };

    // -- 2. raise the inputs into C locals ----------------------------------
    let mut locals = Vec::new();
    for f in &wire.inputs {
        let abi = abi_by_name
            .get(f.name.as_str())
            .ok_or_else(|| format!("{}.{}: no C parameter", c.name, f.name))?;
        let id = naming::ident(&f.rust_name);
        let ty = c_param_type(cx, &abi.ty);
        // **A `stride=` parameter is rewritten, not forwarded.**
        //
        // The wire carries every array tightly packed, so the application's
        // stride describes a layout that no longer exists by the time the driver
        // sees it. Passing it through hands RADV a repacked array *and* the
        // original spacing, and it strides straight off the end — the app side
        // reads at the application's stride (`icd_entry`), this side tells the
        // driver the only stride that is now true (docs/design.md §0.24).
        //
        // `size_of` the C element type rather than the wire one, because it is
        // the C array the driver walks.
        let strided_by = c.params.iter().find(|p| p.stride.as_deref() == Some(f.name.as_str()));
        let e = match strided_by {
            Some(arr) => {
                let elem = pointee_c_type(cx, &arr.ty)?;
                quote!(core::mem::size_of::<#elem>() as _)
            }
            None => raise_expr(cx, &f.shape, abi, &quote!(a.#id), &c.name)?,
        };
        // An array *parameter* decays to a pointer and an array *member* does
        // not — docs/design.md §0.3's finding 4, met from the other side.
        // `vkCmdSetBlendConstants(const float blendConstants[4])` takes four
        // floats by address, so the raised `[f32; 4]` has to be given one.
        let e = if matches!(abi.ty.kind, CTypeKind::Array(..)) {
            quote!({ let __v = #e; __a.one(__v) as _ })
        } else {
            e
        };
        locals.push(quote!(let #id: #ty = #e;));
    }

    // -- 3. output storage ---------------------------------------------------
    let mut outs: Vec<(&FieldPlan, Out)> = Vec::new();
    for f in &wire.outputs {
        let abi = abi_by_name
            .get(f.name.as_str())
            .ok_or_else(|| format!("{}.{}: no C parameter", c.name, f.name))?;
        outs.push((f, classify_output(cx, c, wire, abi, f)?));
    }

    let mut out_decls = Vec::new();
    // waycompose-design.md §87: every output array gets canary bytes in its
    // spare capacity before the call and is checked after it, so a driver that
    // writes past the count it was given is reported instead of corrupting the
    // heap (`sv_gpu_entry::guard_spare`).
    let mut guard_checks = Vec::new();
    let cname_for_guard = c.name.as_str();
    for (f, kind) in &outs {
        let id = naming::ident(&f.rust_name);
        match kind {
            Out::Count { c_ty } => {
                // The caller's capacity on the way in, the answer on the way
                // out — Venus's `_partial` count, and the reason an output
                // parameter travels in *both* directions (docs/design.md §7.2).
                out_decls.push(quote!(let mut #id: #c_ty = a.#id.unwrap_or(0) as #c_ty;));
            }
            // # An output struct's `sType` and `pNext` are *inputs*
            //
            // This is Venus's three-state validity read the right way round, and
            // getting it wrong is what the first run of this pass did.
            // `vkGetPhysicalDeviceFeatures2(pFeatures)` fills in
            // `pFeatures->features`, but `pFeatures->sType` and
            // `pFeatures->pNext` are the *caller's*: the chain is how the caller
            // says which extension features it wants, and a zeroed `sType` makes
            // the call invalid usage. So an output struct is **raised from the
            // wire value**, exactly like an input, and only its body is
            // overwritten by the driver.
            //
            // That is also what makes `vkGetPhysicalDeviceFeatures2` work at
            // all across the boundary, which is the single query Zink's startup
            // turns on: the chain goes out, the driver fills every link, and
            // `lower_full_pnext` brings all of them back.
            Out::One { c_ty, handle } => {
                let init = match (handle, output_struct(f)) {
                    (None, Some(n)) if cx.shared_struct(n) => {
                        let r = raise_fn(n);
                        quote!(match &a.#id {
                            Some(__v) => #r(__v, &mut *__a)?,
                            None => <#c_ty as Default>::default(),
                        })
                    }
                    _ => quote!(<#c_ty as Default>::default()),
                };
                out_decls.push(quote!(let mut #id: #c_ty = #init;));
            }
            Out::Scalar { c_ty, .. } => {
                out_decls.push(quote!(let mut #id: #c_ty = <#c_ty as Default>::default();));
            }
            Out::Array { c_ty, handle, .. } => {
                let init = match (handle, output_struct(f)) {
                    (None, Some(n)) if cx.shared_struct(n) => {
                        let r = raise_fn(n);
                        quote! {
                            for __e in a.#id.iter() {
                                let __v = #r(__e, &mut *__a)?;
                                #id.push(__v);
                            }
                        }
                    }
                    _ => quote!(#id.resize_with(a.#id.len(), <#c_ty as Default>::default);),
                };
                out_decls.push(quote! {
                    let mut #id: Vec<#c_ty> = Vec::with_capacity(a.#id.len());
                    #init
                    crate::guard_spare(&mut #id);
                });
                let pname = f.name.as_str();
                guard_checks.push(quote!(crate::check_spare(&#id, #cname_for_guard, #pname);));
            }
            // Over-aligned deliberately: `vkGetQueryPoolResults` writes
            // `uint64_t` when `VK_QUERY_RESULT_64_BIT` is set, and the caller's
            // capacity in bytes says nothing about the alignment the driver
            // wants. A `Vec<u64>` sized in words is the cheapest way to be
            // right for both widths.
            Out::Blob { .. } => out_decls.push(quote! {
                let mut #id: Vec<u64> = vec![0u64; a.#id.len().div_ceil(8)];
            }),
        }
    }

    // -- 4. the call ---------------------------------------------------------
    let mut args = Vec::new();
    for p in &c.params {
        let id = naming::ident(&p.rust_name);
        if wire.inputs.iter().any(|f| f.name == p.name) {
            args.push(quote!(#id));
            continue;
        }
        if let Some((f, kind)) = outs.iter().find(|(f, _)| f.name == p.name) {
            let id = naming::ident(&f.rust_name);
            args.push(match kind {
                Out::Count { .. } | Out::One { .. } | Out::Scalar { .. } => quote!(&mut #id),
                Out::Array { .. } => quote!(
                    if #id.is_empty() { core::ptr::null_mut() } else { #id.as_mut_ptr() }
                ),
                Out::Blob { .. } => quote!(
                    if #id.is_empty() {
                        core::ptr::null_mut()
                    } else {
                        #id.as_mut_ptr() as *mut _
                    }
                ),
            });
            continue;
        }
        // The only parameter in neither list is `pAllocator`, which design.md
        // §7.4 strips above the protocol. Host allocators are not supported and
        // that is legal; null is what "no callbacks" means.
        if p.name == "pAllocator" {
            args.push(quote!(core::ptr::null()));
            continue;
        }
        return Err(format!("{}.{}: a parameter in neither direction", c.name, p.name));
    }

    let field = fn_field(&c.name);
    let cname = c.name.as_str();
    let call = if c.returns_void {
        quote!(unsafe { __f(#(#args),*) };)
    } else {
        quote!(let __r = unsafe { __f(#(#args),*) };)
    };

    // # An observer, for the few commands `exceptions::GPU_OBSERVED` names
    //
    // `GpuHost::observe` sees the arguments **as the driver receives them** —
    // raised, mapped, pointers into this call's own pool — immediately before
    // the call and again after it. It exists for docs/video-decode.md §7.10:
    // a driver that faults inside a recorded command leaves no reply and no
    // trace, and the only record of what it was given is one written before it
    // was given it. Each argument crosses as a `u64` (a pointer's address, a
    // handle's raw value, an integer); the observer knows each command's
    // signature. Off, it costs one call to `observing`.
    // # A raise pool the host may keep, for `exceptions::GPU_RETAIN_RAISE`
    //
    // The pool owns every C structure the driver was handed. Dropped here, as
    // it is for every other command, a pointer the driver kept past the call
    // points into freed memory; handed to `GpuHost::retain_raise`, it lives as
    // long as the host says (docs/video-decode.md §7.11).
    let retain = if crate::exceptions::GPU_RETAIN_RAISE.contains(&cname) {
        quote!(__h.retain_raise(#cname, __pool);)
    } else {
        quote!()
    };

    let call = if crate::exceptions::GPU_OBSERVED.contains(&cname) {
        let words: Vec<TokenStream> = args
            .iter()
            .zip(&c.params)
            .map(|(a, p)| {
                let is_out_one = outs.iter().any(|(f, k)| {
                    f.name == p.name && matches!(k, Out::Count { .. } | Out::One { .. } | Out::Scalar { .. })
                });
                if p.name == "pAllocator" {
                    quote!(0u64)
                } else if is_out_one {
                    let id = outs
                        .iter()
                        .find(|(f, _)| f.name == p.name)
                        .map(|(f, _)| naming::ident(&f.rust_name))
                        .expect("found above");
                    quote!((&raw mut #id) as usize as u64)
                } else {
                    quote!((#a) as usize as u64)
                }
            })
            .collect();
        let n = words.len();
        let result = if c.returns_void { quote!(0i32) } else { quote!(__r.0 as i32) };
        quote! {
            let __obs = __h.observing(#cname);
            let __ow: [u64; #n] = if __obs { [#(#words),*] } else { [0u64; #n] };
            if __obs {
                __h.observe(#cname, false, &__ow, 0);
            }
            #call
            if __obs {
                __h.observe(#cname, true, &__ow, #result);
            }
        }
    } else {
        call
    };

    // # External synchronisation of the one `VkQueue` (docs/design.md §0.12 item 4)
    //
    // Vulkan makes `vkQueueSubmit`, `vkQueueWaitIdle`, `vkQueueBindSparse` and
    // `vkDeviceWaitIdle` externally synchronised on the queue, which means *the
    // caller* must not make two of them concurrently. Across this boundary the
    // caller is not one application:
    //
    // - a GPU side opens **one** queue, and since §0.12 every session shares it,
    //   so two well-behaved clients still reach one driver `VkQueue` at once;
    // - within one session there are three ring threads (instance, device,
    //   queue), and `vkDeviceWaitIdle` rides the device ring while
    //   `vkQueueSubmit` rides the queue one — so the race predates multi-session
    //   and §0.12 recorded it as such.
    //
    // The lock is per *device* and lives on the hand-written half, because what
    // it protects is the driver's queue rather than the session's table. It is
    // taken as narrowly as the call itself: raising the parameters above it
    // touches nothing shared, and widening the lock to cover marshalling would
    // serialise every submit's memcpy for no benefit.
    //
    // `drop` is explicit rather than left to the end of the block because
    // `__b = __h` reborrows the host mutably immediately afterwards, and a guard
    // holding a shared borrow across that line does not compile. That is the
    // borrow checker enforcing the narrowness this comment claims.
    let call = if c.externsync_queue {
        quote! {
            let __qg = __h.lock_queue();
            #call
            drop(__qg);
        }
    } else {
        call
    };

    // # The command-pool lock, and why it is **not** taken per recorded command
    //
    // `VkCommandBuffer` and `VkCommandPool` are externally synchronised, and the
    // pairing between the thread that recorded and the thread that executes is
    // broken by construction here (docs/design.md §6.5, and
    // `Command::externally_synchronizes_a_command_pool` for the argument).
    //
    // But a recording is a *sequence*, not a set of independent calls: two blobs
    // for two command buffers of one pool must not interleave, and a lock taken
    // and released around each `vkCmd*` permits exactly that interleaving while
    // looking like synchronisation. So everything inside a blob — every
    // `is_recorded` command and the two `BLOB_FRAMING` commands that bracket it
    // — is deliberately **not** locked here; `sv_gpu_server::session` holds the
    // lock across the whole replay instead, which is the granularity the
    // obligation actually has.
    //
    // Taking it in both places would deadlock on the first `vkCmd*`, because the
    // guard is a plain `Mutex` and Rust's is not reentrant. That is a good
    // property rather than an inconvenience: it makes the double-lock a hang in
    // the first test rather than a subtle scope error.
    let in_blob =
        wire.is_recorded || crate::exceptions::BLOB_FRAMING.contains(&c.name.as_str());
    let call = if c.externsync_pool && !in_blob {
        quote! {
            let __pg = __h.lock_command_pool();
            #call
            drop(__pg);
        }
    } else {
        call
    };

    // # The driver's own errors, told to the host (waycompose-design.md §95)
    //
    // A `VkResult` the driver returns is forwarded unchanged, which is right,
    // and was also all that happened to it: a GPU side whose driver lost its
    // device or ran out of memory said nothing, and the application's log
    // could not say which side failed. After the locks above are released,
    // so the host may take its own.
    let call = if wire.returns_result {
        quote! {
            #call
            if __r.0 < 0 {
                __h.note_driver_error(#cname, __r.0);
            }
        }
    } else {
        call
    };

    // -- 5. bind created objects, and lower the outputs back ----------------
    let mut post = Vec::new();
    let mut reply_fields = Vec::new();
    if wire.returns_result {
        reply_fields.push(quote!(result: sv_proto::VkResult(__r.0),));
    } else if wire.returns_value {
        // The driver's own answer, widened to the wire's `u64`. This is the GPU
        // side of docs/design.md §0.3 item 5: `vkGetBufferDeviceAddress` returns a
        // real device address on a real device, and §11's caveat holds it
        // together — we never relocate, so the number stays true for as long as
        // the buffer does.
        reply_fields.push(quote!(ret: __r as u64,));
    }
    for (f, kind) in &outs {
        let id = naming::ident(&f.rust_name);
        match kind {
            Out::Count { .. } => {
                reply_fields.push(quote!(#id: Some(#id as _),));
            }
            Out::Scalar { back, .. } => {
                let e = lower_scalar_back(back, &quote!(#id));
                reply_fields.push(quote!(#id: Some(#e),));
            }
            Out::One { handle: Some(h), .. } => {
                let ht = naming::type_ident(h);
                // The reply carries the id the *app side* chose, not the
                // driver's handle: object creation costs no round trip
                // precisely because the app already knows the id (design.md
                // §6.4). What crosses back is only whether it worked.
                post.push(quote! {
                    if let Some(__id) = a.#id {
                        __b.bind(__id.0, sv_proto::HandleType::#ht, #id as usize as u64);
                    }
                });
                reply_fields.push(quote!(#id: a.#id,));
            }
            Out::One { handle: None, .. } => {
                let n = output_struct(f)
                    .ok_or_else(|| format!("{}: an output that is not a struct", f.name))?;
                if !lower_cx.shared_struct(n) {
                    return Err(format!("{}: {n} has no full lowering", f.name));
                }
                let lower = lower_cx.lower_fn(n);
                post.push(quote! {
                    let #id = unsafe { #lower(&raw const #id) }
                        .map_err(|_| sv_wire::WireError::UnknownCommand { opcode: 0 })?;
                });
                reply_fields.push(quote!(#id: Some(Box::new(#id)),));
            }
            Out::Array { handle: Some(h), count, .. } => {
                let ht = naming::type_ident(h);
                let n = match count {
                    Some(s) => {
                        let cid = naming::ident(&naming::snake_case(s));
                        quote!((#cid as usize).min(#id.len()))
                    }
                    None => quote!(#id.len()),
                };
                // §96. **An element the driver left `VK_NULL_HANDLE` goes back
                // as `VK_NULL_HANDLE`**, not as the id the app side minted for
                // it. A batch create can fail per element, and one way is not a
                // failure at all: `vkCreateGraphicsPipelines` with
                // `FAIL_ON_PIPELINE_COMPILE_REQUIRED` answers
                // `VK_PIPELINE_COMPILE_REQUIRED` and a null pipeline, and the
                // caller tests the *handle* to decide whether to compile. Zink's
                // fast-link path does exactly that; given the minted id back it
                // bound a pipeline that existed nowhere, this side translated it
                // to a null handle, and RADV dereferenced it in
                // `vkCmdBindPipeline` (waycompose-design.md §96).
                let out = naming::ident(&format!("__out_{}", f.rust_name));
                post.push(quote! {
                    let mut #out = a.#id.clone();
                    for __i in 0..#n {
                        if let Some(__id) = a.#id.get(__i) {
                            if #id[__i] as usize as u64 == 0 {
                                #out[__i].0 = 0;
                            } else {
                                __b.bind(
                                    __id.0,
                                    sv_proto::HandleType::#ht,
                                    #id[__i] as usize as u64,
                                );
                            }
                        }
                    }
                });
                reply_fields.push(quote!(#id: #out,));
            }
            // **An output array of plain scalars**, which this arm used to
            // refuse with "an output that is not a struct".
            //
            // Two commands in the registry are this shape and both belong to
            // `VK_KHR_calibrated_timestamps`: `pTimeDomains` is an array of
            // `VkTimeDomainKHR` and `pTimestamps` an array of `uint64_t`. The
            // refusal meant the GPU side generated no forwarder for either, so
            // `sv_proto::Server`'s default answered
            // `VK_ERROR_INITIALIZATION_FAILED` — while the app side advertised
            // the extension and forwarded the call perfectly happily.
            //
            // **That is what was killing Firefox** (docs/design.md §0.22). Zink calls
            // `vkGetPhysicalDeviceCalibrateableTimeDomainsKHR` while building
            // its screen, takes the failure as fatal, and tears the device down
            // before rendering a frame: `DRI2: failed to create screen`,
            // `eglInitialize failed`, no GL at all. An array of numbers coming
            // back is the same fold as the single-number `Out::Scalar` arm above
            // with a loop around it, and it is emitted here rather than refused.
            Out::Array { handle: None, count, .. } if output_struct(f).is_none() => {
                let elem = match &f.shape {
                    Shape::Array { elem, .. } | Shape::Optional(elem) => match &**elem {
                        Shape::Scalar(s) => s.clone(),
                        other => {
                            return Err(format!(
                                "{}: an output array of {other:?}, which is neither a struct \
                                 nor a scalar",
                                f.name
                            ));
                        }
                    },
                    other => {
                        return Err(format!("{}: an output array shaped {other:?}", f.name));
                    }
                };
                let n = match count {
                    Some(s) => {
                        let cid = naming::ident(&naming::snake_case(s));
                        quote!((#cid as usize).min(#id.len()))
                    }
                    None => quote!(#id.len()),
                };
                let back = lower_scalar_back(&elem, &quote!(#id[__i]));
                post.push(quote! {
                    let mut __v = Vec::with_capacity(#n);
                    for __i in 0..#n { __v.push(#back); }
                    let #id = __v;
                });
                reply_fields.push(quote!(#id: #id,));
            }
            Out::Array { handle: None, count, .. } => {
                let sname = output_struct(f)
                    .ok_or_else(|| format!("{}: an output that is not a struct", f.name))?;
                if !lower_cx.shared_struct(sname) {
                    return Err(format!("{}: {sname} has no full lowering", f.name));
                }
                let lower = lower_cx.lower_fn(sname);
                let n = match count {
                    Some(s) => {
                        let cid = naming::ident(&naming::snake_case(s));
                        quote!((#cid as usize).min(#id.len()))
                    }
                    None => quote!(#id.len()),
                };
                post.push(quote! {
                    let mut __v = Vec::with_capacity(#n);
                    for __i in 0..#n {
                        __v.push(
                            unsafe { #lower(&raw const #id[__i]) }
                                .map_err(|_| sv_wire::WireError::UnknownCommand { opcode: 0 })?,
                        );
                    }
                    let #id = __v;
                });
                reply_fields.push(quote!(#id: #id,));
            }
            Out::Blob { count, .. } => {
                // The count is in **bytes**, whatever the element type of the
                // storage is, and it is the driver's answer rather than the
                // capacity. Clamping against our own buffer is Venus's rule
                // (docs/design.md §6.1) applied in the direction the reply travels.
                let n = match count {
                    Some(s) => {
                        let cid = naming::ident(&naming::snake_case(s));
                        quote!((#cid as usize).min(__bytes.len()))
                    }
                    // The size was an input the caller fixed, so the whole
                    // buffer is the answer: `vkGetQueryPoolResults`.
                    None => quote!(__bytes.len()),
                };
                post.push(quote! {
                    let #id: Vec<u8> = {
                        let __bytes: &[u8] = unsafe {
                            core::slice::from_raw_parts(
                                #id.as_ptr() as *const u8,
                                core::mem::size_of_val(&#id[..]),
                            )
                        };
                        __bytes[..#n].to_vec()
                    };
                });
                reply_fields.push(quote!(#id: #id,));
            }
        }
    }


    let tail = if wire.generates_reply {
        quote! {
            #(#post)*
            #unbind_after
            Ok(sv_proto::#reply_ty { #(#reply_fields)* })
        }
    } else {
        quote! {
            #(#post)*
            #unbind_after
            Ok(())
        }
    };

    // What a command answers when the driver does not have it. Deliberately the
    // same three cases `emit_server_trait`'s default refusal uses.
    let missing_answer = if wire.generates_reply && wire.returns_result {
        quote!({
            let mut __r = <sv_proto::#reply_ty as Default>::default();
            __r.result = sv_proto::VkResult(-3);
            Ok(__r)
        })
    } else if wire.generates_reply {
        quote!(Err(sv_wire::WireError::UnknownCommand { opcode: 0 }))
    } else {
        quote!(Ok(()))
    };

    // Recording is starting again, so whatever broke last time is forgiven.
    let clears = crate::exceptions::CLEARS_COMMAND_BUFFER_POISON.contains(&cname);
    let unpoison =
        if clears { quote!(__h.unpoison_command_buffer(a.command_buffer.0);) } else { quote!() };

    // Refuse a call that names a command buffer whose recording failed.
    //
    // # Where the set of command buffers comes from, and why not from here
    //
    // The first version of this walked the wire signature for `VkCommandBuffer`
    // fields. It covered `vkCmdExecuteCommands` and `vkFreeCommandBuffers` and
    // **missed `vkQueueSubmit`**, which is the only one that matters:
    // `vkQueueSubmit` takes `VkSubmitInfo[]` and the buffers are a member of
    // *that*, one level down, and `vkQueueSubmit2` puts them two levels down
    // inside `VkCommandBufferSubmitInfo`. A shallow walk finds the commands that
    // do not need checking and not the one that does.
    //
    // The right traversal already exists and runs immediately above: the
    // **handle-mapping pass** visits every handle anywhere in the argument
    // graph, nested or not, and calls `map_handle` with its `HandleType`. So the
    // host notes command buffers as it maps them and this asks what it saw. No
    // second walker, no depth limit, and a command added to the registry
    // tomorrow with buffers three structs deep is covered by construction.
    //
    // # Which commands ask
    //
    // Not recorded ones: a `vkCmd*` recording into an already-poisoned buffer is
    // harmless, the buffer is invalid either way, and refusing here would report
    // the failure against whichever command happened to come next rather than
    // against the submit.
    //
    // Not the ones that clear the mark — `vkBeginCommandBuffer` maps its target
    // before `unpoison` runs, so it would refuse the very call that makes the
    // buffer usable again.
    //
    // Not the ones that *destroy*, derived from the fact that they unbind
    // handles: `vkFreeCommandBuffers` on an invalid buffer must free it, and a
    // destructor that can be refused is a leak. This is why the check is keyed
    // on `unbind.is_empty()` rather than on a list of names.
    //
    // What is left is `vkQueueSubmit`, `vkQueueSubmit2` and
    // `vkEndCommandBuffer` — and `vkEndCommandBuffer` refusing is not a side
    // effect but the specification: a command buffer whose recording failed
    // returns an error from `vkEndCommandBuffer`.
    //
    // `vkEndCommandBuffer` still reaches the driver first (docs/video-decode.md
    // §7.12). Refused before the call, the driver's buffer stayed in the
    // recording state, and the application's next `vkBeginCommandBuffer` on it
    // was then a begin of a buffer still recording. Ended, it is merely a
    // buffer with a command missing, which is never submitted (the submit is
    // refused) and is reset by the next begin like any other.
    let asks_about_poison = !wire.is_recorded && !clears && unbind.is_empty();
    let end_after_call = asks_about_poison && cname == "vkEndCommandBuffer";
    // A `vkQueueSubmit` naming a poisoned command buffer still reaches the
    // driver, without that buffer (docs/video-decode.md §7.12). The ICD sends
    // that one without waiting for a reply and has already told the
    // application `VK_SUCCESS`, so the application counts on the batch's
    // semaphore signals and fence; dropped whole, the next wait on them would
    // never return. The error still goes back and takes the ring fatal, as any
    // refused asynchronous submit does: the application sees
    // `VK_ERROR_DEVICE_LOST` on its next call.
    //
    // `vkQueueSubmit2` is a round trip, so the application reads the error and
    // counts no semaphore signal; signalling them anyway put its timeline
    // values behind the driver's (`VUID-VkSubmitInfo2-semaphore-03882`). But
    // its fence still has to complete: ffmpeg's next `ff_vk_exec_start` on that
    // execution context waits for it with no timeout, and refused whole, that
    // wait never returned. So the driver gets the fence and no batch at all.
    let strip_hook = match cname {
        "vkQueueSubmit" => Some(quote!(strip_poisoned_submit)),
        "vkQueueSubmit2" => Some(quote!(strip_poisoned_submit2)),
        _ => None,
    };
    let submit_after_call = asks_about_poison && strip_hook.is_some();
    let settle = if cname == "vkQueueSubmit2" {
        quote!(__h.settle_stripped_submit(a.fence.0);)
    } else {
        quote!()
    };
    let poison_after = if end_after_call || submit_after_call {
        quote! {
            if let Some(__why) = __end_poison {
                let _ = &__r;
                #settle
                __h.note_poisoned_submit(#cname, __why);
                return #missing_answer;
            }
        }
    } else {
        quote!()
    };
    let poison_check = if end_after_call {
        quote!(let __end_poison = __h.take_poisoned_command_buffer();)
    } else if submit_after_call {
        let hook = strip_hook.expect("checked above");
        quote! {
            let __end_poison = __h.take_poisoned_command_buffer();
            if let Some(__why) = __end_poison
                && !__h.#hook(&mut a)
            {
                // The batch could not be taken apart: refused whole, as before.
                __h.note_poisoned_submit(#cname, __why);
                return #missing_answer;
            }
        }
    } else if asks_about_poison {
        quote! {
            if let Some(__why) = __h.take_poisoned_command_buffer() {
                // Failing *this* call is the whole point: it is detectable, it
                // names the command that broke recording, and it does not touch
                // the ring.
                __h.note_poisoned_submit(#cname, __why);
                return #missing_answer;
            }
        }
    } else {
        // Still has to be drained, or a poisoned buffer seen by a recorded
        // command would be reported against the next unrelated call that asks.
        quote!(let _ = __h.take_poisoned_command_buffer();)
    };

    // A recorded command whose entry point the driver lacks poisons its target.
    //
    // `Ok(())` here was the silent-no-op §0.22 item 5 named and did not close:
    // nothing is recorded, there is no reply to carry the fact, and a caller
    // checking a return value cannot notice. Returning `Err` instead would take
    // the ring fatal, which is far too blunt for a driver that merely lacks an
    // optional extension — so neither answer was right, because the failure does
    // not belong to *this* call. It belongs to the command buffer, and Vulkan
    // already has a word for that: a command buffer whose recording failed is
    // invalid, and submitting it must fail.
    //
    // The handle mapping above has already run, so `a.command_buffer.0` is the
    // driver's handle rather than the wire id — which is also what
    // `vkQueueSubmit` will be holding when it checks.
    let missing_arm = if wire.is_recorded {
        quote! {
            __h.note_missing(#cname);
            __h.poison_command_buffer(a.command_buffer.0, #cname);
            return #missing_answer;
        }
    } else {
        quote! {
            __h.note_missing(#cname);
            return #missing_answer;
        }
    };

    let mut_a = if has_pre || submit_after_call { quote!(mut) } else { quote!() };
    Ok(quote! {
        let #mut_a a = a;
        let __h = &mut self.0;
        #unbind_block
        #pre
        #unpoison
        #poison_check
        let __maybe = __h.fns().#field;
        let __f = match __maybe {
            Some(__f) => __f,
            None => {
                // **Fail the call, not the session.** The profile is a superset
                // of any one driver's extensions, so a missing entry point is a
                // normal configuration and not a protocol error — and returning
                // `Err` here would propagate out of `dispatch`, produce no reply
                // at all, and take the ring fatal. That is exactly what
                // `sv_proto::Server`'s own default refusal was changed to avoid
                // (docs/design.md §7.2's note on the trait), and the same argument
                // applies one layer down: `vkSetDebugUtilsObjectNameEXT` on a
                // driver without the extension must not lose the device.
                //
                // For a *recorded* command there is a third answer, and it is
                // the right one: poison the command buffer, so the submit that
                // would have executed it fails instead. See `missing_arm`.
                #missing_arm
            }
        };
        let mut __pool = crate::Raise::default();
        let __a = &mut __pool;
        #(#locals)*
        #(#out_decls)*
        #call
        #poison_after
        #(#guard_checks)*
        #retain
        let __b = __h;
        #tail
    })
}

/// C → wire for one scalar: the inverse of [`raise_scalar`], for the handful of
/// commands whose output is a bare number rather than a struct.
///
/// It is a *different* function from the app side's `lower_scalar`, and only
/// because that one is private; the conversions are the same three lines and a
/// test asserts the round trip rather than the reader having to compare them.
fn lower_scalar_back(s: &Scalar, access: &TokenStream) -> TokenStream {
    match s {
        Scalar::Bool32 => quote!((#access) != 0),
        Scalar::F32 | Scalar::F64 => quote!(#access),
        Scalar::Enum(n) | Scalar::Bitmask(n) | Scalar::Bitmask64(n) => {
            let t = naming::type_ident(n);
            quote!(sv_proto::#t((#access).0 as _))
        }
        Scalar::OpaqueU64(_) => quote!((#access) as usize as u64),
        _ => quote!((#access) as _),
    }
}

/// The struct a command output is made of.
fn output_struct(f: &FieldPlan) -> Option<&str> {
    fn base(shape: &Shape) -> Option<&str> {
        match shape {
            Shape::Scalar(Scalar::Struct(n)) | Shape::Scalar(Scalar::Union(n)) => Some(n),
            Shape::Optional(i) | Shape::Array { elem: i, .. } => base(i),
            _ => None,
        }
    }
    base(&f.shape)
}

/// The full-lowering `pNext` walker: C chain → wire links, reading every member.
///
/// Emitted by the same function as `sv-icd-entry`'s `lower_pnext` and a different
/// function in substance, because the links it calls are the `lower_full_*` ones:
/// which of them is correct depends on which side is asking, not on the
/// direction (see [`Ctx::lower_full`]).
fn emit_lower_full_pnext(cx: &Ctx<'_>) -> TokenStream {
    // The same walker as the app side's, from the same emitter: a table rather
    // than a `match`, and each link lowered with its `pNext` cut so the walk does
    // not recurse (docs/design.md §0.27). In a debug build this side's copy was
    // a 136 KB frame.
    let walker = super::icd_entry::emit_lower_walker(cx);
    quote! {
        /// One link's lowering, as a `pNext` dispatch-table entry.
        type LowerLinkFn =
            unsafe fn(*const core::ffi::c_void) -> crate::LowerResult<sv_proto::PNextLink>;
        #walker
    }
}

// ---------------------------------------------------------------------------
// the whole crate
// ---------------------------------------------------------------------------

fn emit_host_trait(cx: &Ctx<'_>, hand: &[&AbiCommand]) -> TokenStream {
    let methods = hand.iter().filter_map(|c| {
        let wire = cx.wire.commands.iter().find(|w| w.name == c.name)?;
        let m = naming::command_method(&c.name);
        let args = format_ident!("{}Args", naming::opcode_variant(&c.name));
        let ret = if wire.generates_reply {
            let r = format_ident!("{}Reply", naming::opcode_variant(&c.name));
            quote!(sv_proto::#r)
        } else {
            quote!(())
        };
        let doc = format!(
            "`{}` — hand-written, because forwarding it verbatim would be wrong.",
            c.name
        );
        Some(quote! {
            #[doc = #doc]
            fn #m(&mut self, args: sv_proto::#args) -> sv_wire::Result<#ret>;
        })
    });

    quote! {
        /// What a GPU side supplies so that the generated forwarding can run.
        ///
        /// Everything here is either a table the generated code consults or a
        /// command whose meaning is *ours* rather than the driver's — the arena,
        /// the session, submission. The list is short and closed, and it is the
        /// honest measure of how much of a GPU side is hand-written: everything
        /// not named here is generated (docs/design.md §7.2, pass 12).
        pub trait GpuHost {
            /// The driver's entry points.
            fn fns(&self) -> &DriverFns;
            /// App-assigned object id → the driver's own handle (docs/design.md §6.4).
            fn map_handle(
                &mut self,
                ty: sv_proto::HandleType,
                id: u64,
            ) -> sv_wire::Result<u64>;
            /// Arena coordinates → a driver allocation and an offset into it
            /// (docs/design.md §5.3, GPU-side half).
            fn rewrite_memory(&mut self, memory: u64, offset: u64)
                -> sv_wire::Result<(u64, u64)>;
            /// Record the app side's id for an object the driver just created.
            fn bind(&mut self, id: u64, ty: sv_proto::HandleType, driver: u64);
            /// Forget an object the driver just destroyed.
            fn unbind(&mut self, id: u64);
            /// Record that the driver has no such entry point, for one clear
            /// diagnosis rather than a silent refusal.
            fn note_missing(&mut self, command: &'static str);
            /// Mark a command buffer as having failed to record, naming the
            /// command that could not be recorded.
            ///
            /// Vulkan already has the concept: a command buffer whose recording
            /// failed is *invalid*, and submitting an invalid command buffer
            /// must not succeed. That is the answer to the question §0.22 item 5
            /// left open — a recorded `vkCmd*` whose driver entry point is
            /// missing has no reply to fail and must not fail the ring, because
            /// the failure is not this call's. It is the command buffer's.
            ///
            /// Called with the **driver's** handle, because the generated
            /// dispatch maps handles before it looks the entry point up, and
            /// because that is what `vkQueueSubmit` will be holding when it
            /// checks.
            fn poison_command_buffer(&mut self, command_buffer: u64, why: &'static str);
            /// Remove every poisoned command buffer from a `vkQueueSubmit`'s
            /// batches, keeping their semaphores and the fence; false if the
            /// batches cannot be taken apart, and the submit is then refused
            /// whole (docs/video-decode.md §7.12).
            fn strip_poisoned_submit(&mut self, a: &mut sv_proto::VkQueueSubmitArgs) -> bool {
                let _ = a;
                false
            }
            /// The same for `vkQueueSubmit2`, whose error the application
            /// reads: every batch is removed and only the fence is left.
            fn strip_poisoned_submit2(&mut self, a: &mut sv_proto::VkQueueSubmit2Args) -> bool {
                let _ = a;
                false
            }
            /// After a `vkQueueSubmit2` stripped to its fence: wait for that
            /// fence, which the application believes was never submitted and
            /// may destroy at once (`VUID-vkDestroyFence-fence-01120`).
            fn settle_stripped_submit(&mut self, fence: u64) {
                let _ = fence;
            }
            /// Clear a command buffer's poison: it is being recorded afresh.
            ///
            /// Without this a buffer reused after a failed recording stays
            /// invalid forever, which turns one missing extension into a device
            /// that never submits again.
            fn unpoison_command_buffer(&mut self, command_buffer: u64);
            /// Take the reason, if the handle-mapping pass just mapped a command
            /// buffer whose recording had failed.
            ///
            /// The mapping pass is the traversal: it visits every handle
            /// anywhere in the argument graph, so this needs no walker of its
            /// own and cannot miss a `VkCommandBuffer` nested inside a
            /// `VkSubmitInfo`. Taking rather than peeking, because the flag is
            /// per-dispatch and a stale one would be reported against the next
            /// command.
            fn take_poisoned_command_buffer(&mut self) -> Option<&'static str>;
            /// Report that a call was refused because it names a command buffer
            /// whose recording failed.
            fn note_poisoned_submit(&mut self, command: &'static str, why: &'static str);
            /// Hold the device's queue lock for the duration of one driver call
            /// the specification marks externally synchronised on a `VkQueue`.
            ///
            /// The generated dispatch takes this around — and only around — the
            /// call itself, for the commands
            /// `vk_registry::model::Command::externally_synchronizes_a_queue`
            /// selects. A GPU side has one queue and every session shares it, so
            /// the obligation cannot be discharged by any one client behaving
            /// (docs/design.md §0.12 item 4).
            fn lock_queue(&self) -> crate::QueueGuard<'_>;
            /// Hold the device's command-pool lock, for the commands
            /// `vk_registry::model::Command::externally_synchronizes_a_command_pool`
            /// selects and which are *not* replayed inside a blob.
            ///
            /// Everything inside a blob is covered once, by
            /// `sv_gpu_server::session`, because a recording is a sequence and
            /// per-command locking would let two recordings interleave while
            /// looking synchronised (docs/design.md §0.23).
            fn lock_command_pool(&self) -> crate::QueueGuard<'_>;
            /// Whether [`GpuHost::observe`] wants to be called at all for this
            /// call of `command`, one of the commands `exceptions::GPU_OBSERVED`
            /// names.
            fn observing(&self, command: &'static str) -> bool {
                let _ = command;
                false
            }
            /// The driver call of a command `exceptions::GPU_OBSERVED` names,
            /// just before it (`after` false) and just after it (`after` true,
            /// with the `VkResult`, or 0 for a `void` command).
            ///
            /// `args` holds the C arguments exactly as the driver receives
            /// them, each widened to a `u64`: a pointer's address, a handle's
            /// raw value, an integer. Every pointer is valid for the duration
            /// of this call and no longer — it points into the dispatch's own
            /// raise pool or output storage — and an output is only written
            /// once `after` is true.
            fn observe(&self, command: &'static str, after: bool, args: &[u64], result: i32) {
                let _ = (command, after, args, result);
            }
            /// The driver returned a negative `VkResult` from `command`. The
            /// reply carries it either way; this is so that the side holding
            /// the driver can say so (waycompose-design.md §95).
            fn note_driver_error(&self, command: &'static str, result: i32) {
                let _ = (command, result);
            }
            /// The raise pool of a command `exceptions::GPU_RETAIN_RAISE` names,
            /// after its driver call: every C structure the driver was handed.
            /// Dropped by default, as every other command's is.
            fn retain_raise(&mut self, command: &'static str, pool: crate::Raise) {
                let _ = command;
                drop(pool);
            }

            #(#methods)*
        }
    }
}

fn emit_server_impl(
    cx: &Ctx<'_>,
    forwardable: &BTreeMap<String, TokenStream>,
    hand: &BTreeSet<String>,
    refused: &BTreeMap<String, String>,
) -> TokenStream {
    let methods = cx.wire.commands.iter().map(|w| {
        let m = naming::command_method(&w.name);
        let args = format_ident!("{}Args", naming::opcode_variant(&w.name));
        let ret = if w.generates_reply {
            let r = format_ident!("{}Reply", naming::opcode_variant(&w.name));
            quote!(sv_proto::#r)
        } else {
            quote!(())
        };
        if hand.contains(&w.name) {
            let doc = format!("`{}` — delegated to the hand-written half.", w.name);
            return quote! {
                #[doc = #doc]
                fn #m(&mut self, args: sv_proto::#args) -> sv_wire::Result<#ret> {
                    self.0.#m(args)
                }
            };
        }
        if let Some(body) = forwardable.get(&w.name) {
            let doc = format!("`{}` — forwarded to the driver, generated.", w.name);
            return quote! {
                #[doc = #doc]
                #[allow(unused_mut, unused_variables, clippy::let_and_return)]
                fn #m(&mut self, a: sv_proto::#args) -> sv_wire::Result<#ret> {
                    #body
                }
            };
        }
        let why = refused.get(&w.name).cloned().unwrap_or_else(|| "not forwardable".into());
        let doc = format!(
            "`{}` — **not forwarded**: {why}\n\nThe trait default answers, which \
             fails the call rather than the session.",
            w.name
        );
        quote! { #[doc = #doc] }
    });

    quote! {
        /// A [`GpuHost`] wearing the protocol's [`sv_proto::Server`] trait.
        ///
        /// A newtype rather than a blanket `impl<H: GpuHost> Server for H`,
        /// which the orphan rule forbids — `Server` is `sv-proto`'s and `H` is
        /// nobody's.
        pub struct Forward<H: GpuHost>(pub H);

        impl<H: GpuHost> Forward<H> {
            pub fn new(host: H) -> Forward<H> { Forward(host) }
            pub fn host(&mut self) -> &mut H { &mut self.0 }
        }

        impl<H: GpuHost> sv_proto::Server for Forward<H> {
            #(#methods)*
        }
    }
}

/// The device's feature structs, from the registry rather than from a list.
///
/// `sv_gpu_server::driver` opens its device before any application connects,
/// and so has to enable, up front, every feature an application could later
/// ask for. It used to chain a hand-kept list of two dozen extension feature
/// structs into that device, and the list was short: Zink recorded
/// `VK_EXT_extended_dynamic_state3` commands whose features the device had
/// never enabled (`VUID-vkCmdSet*-None-09423`, docs/video-decode.md §7.10.4),
/// because the app side forwards `vkGetPhysicalDeviceFeatures2` to the driver
/// and is told *supported* for every struct the driver knows, while the device
/// enabled only the structs the list named. What was missing was a class —
/// every feature struct of an enabled extension that nobody had typed in — so
/// the answer is the registry's own relation: every struct that extends both
/// `VkPhysicalDeviceFeatures2` and `VkDeviceCreateInfo`, with the core
/// versions and extensions of this profile that provide it.
///
/// Also emitted: a default link per struct (so the GPU side can build the chain
/// from an `sType`), and each struct's `VkBool32` members by registry name (so
/// it can check an application's `VkDeviceCreateInfo` against what it enabled).
fn emit_device_features(cx: &Ctx<'_>, wanted: &[String]) -> TokenStream {
    let canonical = |n: &str| -> String {
        let mut n = n.to_string();
        for _ in 0..8 {
            match cx.model.type_aliases.get(&n) {
                Some(t) => n = t.clone(),
                None => break,
            }
        }
        n
    };
    let extends = |parent: &str| -> BTreeSet<String> {
        cx.wire
            .struct_plan(parent)
            .map(|p| p.extenders.iter().map(|e| canonical(e)).collect())
            .unwrap_or_default()
    };
    let in_features2 = extends("VkPhysicalDeviceFeatures2");
    let in_create = extends("VkDeviceCreateInfo");
    let wanted: BTreeSet<&str> = wanted.iter().map(|s| s.as_str()).collect();
    // Core versions and this profile's extensions, each with the canonical
    // names of the types it requires.
    let providers_of = |s: &str| -> Vec<String> {
        let mut out: Vec<String> = cx
            .model
            .features
            .iter()
            .chain(cx.model.extensions.iter().filter(|e| wanted.contains(e.name.as_str())))
            .filter(|f| {
                f.requires.iter().any(|r| {
                    r.items.iter().any(|i| {
                        matches!(i, vk_registry::model::InterfaceRef::Type(t) if canonical(t) == s)
                    })
                })
            })
            .map(|f| f.name.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    };
    let bools = |s: &StructPlan| -> Vec<(String, proc_macro2::Ident)> {
        s.fields
            .iter()
            .filter(|f| matches!(f.shape, Shape::Scalar(Scalar::Bool32)))
            .map(|f| (f.name.clone(), naming::ident(&f.rust_name)))
            .collect()
    };

    let mut table = Vec::new();
    let mut defaults = Vec::new();
    let mut bit_arms = Vec::new();
    let mut set_arms = Vec::new();
    for (name, s_type) in &cx.wire.pnext_links {
        if !in_features2.contains(name) || !in_create.contains(name) {
            continue;
        }
        let Some(sp) = cx.wire.struct_plan(name) else { continue };
        let providers = providers_of(name);
        let s_type = *s_type as i32;
        let v = naming::pnext_variant(name);
        let t = naming::type_ident(name);
        table.push(quote! {
            DeviceFeatureStruct { name: #name, s_type: #s_type, providers: &[#(#providers),*] },
        });
        defaults.push(quote!(#s_type => sv_proto::PNextLink::#v(Box::default()),));
        let fields = bools(sp);
        let names: Vec<&String> = fields.iter().map(|(n, _)| n).collect();
        let idents: Vec<&proc_macro2::Ident> = fields.iter().map(|(_, i)| i).collect();
        bit_arms.push(quote! {
            sv_proto::PNextLink::#v(__s) => Some(vec![#((#names, __s.#idents)),*]),
        });
        set_arms.push(quote! {
            sv_proto::PNextLink::#v(__s) => {
                let __s: &mut sv_proto::#t = __s;
                match __name { #(#names => { __s.#idents = __on; true })* _ => false }
            }
        });
    }
    let core = cx.wire.struct_plan("VkPhysicalDeviceFeatures");
    let core_fields = core.map(bools).unwrap_or_default();
    let core_names: Vec<&String> = core_fields.iter().map(|(n, _)| n).collect();
    let core_idents: Vec<&proc_macro2::Ident> = core_fields.iter().map(|(_, i)| i).collect();
    let n = table.len();
    quote! {
        /// A feature struct a device may be created with: one that extends both
        /// `VkPhysicalDeviceFeatures2` and `VkDeviceCreateInfo` in the registry.
        #[derive(Debug, Clone, Copy)]
        pub struct DeviceFeatureStruct {
            pub name: &'static str,
            pub s_type: i32,
            /// The core versions (`VK_VERSION_1_x`) and this profile's
            /// extensions that bring the struct in. Empty only for a struct no
            /// enabled thing provides, which a device must not chain.
            pub providers: &'static [&'static str],
        }

        /// Every device feature struct this profile's wire carries.
        pub const DEVICE_FEATURE_STRUCTS: [DeviceFeatureStruct; #n] = [#(#table)*];

        /// A link of the struct with this `sType`, every member false.
        pub fn device_feature_link(s_type: i32) -> Option<sv_proto::PNextLink> {
            Some(match s_type {
                #(#defaults)*
                _ => return None,
            })
        }

        /// A device feature link's `VkBool32` members by registry name, or `None`
        /// for a link that is not a device feature struct.
        pub fn feature_bits(link: &sv_proto::PNextLink) -> Option<Vec<(&'static str, bool)>> {
            match link {
                #(#bit_arms)*
                _ => None,
            }
        }

        /// Set one member of a device feature link by registry name; false if
        /// the link has no such member.
        pub fn set_feature_bit(link: &mut sv_proto::PNextLink, __name: &str, __on: bool) -> bool {
            match link {
                #(#set_arms)*
                _ => false,
            }
        }

        /// `VkPhysicalDeviceFeatures`'s members by registry name.
        pub fn core_feature_bits(f: &sv_proto::VkPhysicalDeviceFeatures) -> Vec<(&'static str, bool)> {
            vec![#((#core_names, f.#core_idents)),*]
        }
    }
}

/// The profile's *device* extensions, as a constant the GPU side can filter the
/// driver's list against.
///
/// docs/design.md §9.2's rule, made checkable: "an extension in this list is a
/// promise that its entry points work". An entry point works only if pass 12
/// emitted a forwarder for it, so the set a GPU side may advertise is exactly
/// the profile's device extensions intersected with the driver's — and the first
/// half of that intersection has to come from the generator, because the
/// alternative is a hand-kept list beside `vk_registry::profiles::zink()` that
/// silently stops matching it.
fn emit_profile_extensions(cx: &Ctx<'_>, wanted: &[String]) -> TokenStream {
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
    // An expression the parser cannot read reads as *unsatisfied*, which is the
    // conservative direction and the rule `vk_registry::depends::satisfied`
    // states.
    let depends_arms: Vec<TokenStream> = cx
        .model
        .extensions
        .iter()
        .filter(|e| e.supported_for_vulkan)
        .filter_map(|e| {
            let expr = e.depends.as_deref()?;
            let name = e.name.as_str();
            match vk_registry::depends::compile(expr) {
                Some(ast) => {
                    let body = lower_depends(&ast);
                    Some(quote!(#name => #body,))
                }
                None => Some(quote!(#name => false,)),
            }
        })
        .collect();
    let names: Vec<&str> = wanted
        .iter()
        .filter(|n| {
            cx.model
                .extensions
                .iter()
                .find(|e| &e.name == *n)
                .and_then(|e| e.ext_kind.as_deref())
                == Some("device")
        })
        .map(|s| s.as_str())
        .collect();
    let n = names.len();
    quote! {
        /// The device extensions this build's profile forwards, sorted as the
        /// profile lists them.
        ///
        /// A GPU side advertises the intersection of this with what its driver
        /// actually has; an app side advertises what the GPU side answered.
        /// Neither may advertise anything else, because an advertised extension
        /// is a promise that its entry points cross (docs/design.md §9.2).
        pub const PROFILE_DEVICE_EXTENSIONS: [&str; #n] = [#(#names),*];

        /// Whether an extension's `depends=` is satisfied by `__have`.
        ///
        /// The same compiled registry expression `sv_icd_entry` carries, emitted
        /// here as well because the GPU side has the same obligation and a
        /// *different* set to check it against: its own instance's extensions
        /// and its own driver's device extensions.
        ///
        /// It was missing, and the validation layer said so:
        /// `VUID-vkCreateDevice-ppEnabledExtensionNames-01387` —
        /// `VK_EXT_display_control` enabled without `VK_EXT_display_surface_counter`,
        /// `VK_EXT_present_timing` without `VK_KHR_get_surface_capabilities2`,
        /// `VK_EXT_swapchain_maintenance1` without `VK_EXT_surface_maintenance1`.
        /// **Every one of those dependencies is an *instance* extension**, which
        /// is why enabling "every profile extension the driver has" looked
        /// complete and was not: the device list can never satisfy them
        /// (docs/design.md §0.23).
        pub fn depends_satisfied(extension: &str, __have: &dyn Fn(&str) -> bool) -> bool {
            match extension {
                #(#depends_arms)*
                _ => true,
            }
        }
    }
}

/// Emit the whole `sv-gpu-entry` crate body, as labelled sections.
pub fn emit_all(
    cx: &Ctx<'_>,
    hand_written: &[&str],
    profile_extensions: &[String],
) -> (Vec<(String, TokenStream)>, Refusals) {
    // Close the raisable set under containment, exactly as the app side closes
    // its lowerable set: a struct whose raise was refused makes every struct
    // containing it unraisable too, and emitting a call to a function that was
    // never emitted is a generated crate that does not compile.
    let (raise_refused, raise_cx) = cx.close_raise(emit_raise_struct);

    // And the same for the *other* direction. Not `sv-icd-entry`'s `lower_*`,
    // although it is the same direction and the same structs: a `returnedonly`
    // struct's members are uninitialised application memory on the app side and
    // the driver's answer here, so the app side's lowering zeroes exactly the
    // members this side exists to carry. See `Ctx::lower_full`.
    //
    // Restricted to `needs_partial`, which docs/design.md §6.1 defines as
    // "`returnedonly` or reachable from a command output" — precisely the
    // structs that can come back.
    let full_cx = cx.full_lowering();
    let (_, lower_cx) =
        full_cx.close_with(|s| s.needs_partial, super::icd_entry::emit_lower_struct);

    let full_lowers: Vec<(String, TokenStream)> = cx
        .wire
        .structs
        .iter()
        .filter(|s| lower_cx.shared_struct(&s.name))
        .map(|s| {
            (
                format!("lower_full {}", s.name),
                super::icd_entry::emit_lower_struct(&lower_cx, s).expect("closed above"),
            )
        })
        .collect();

    let raises: Vec<(String, TokenStream)> = cx
        .wire
        .structs
        .iter()
        .filter(|s| raise_cx.shared_struct(&s.name))
        .map(|s| {
            (
                format!("raise {}", s.name),
                emit_raise_struct(&raise_cx, s).expect("closed above"),
            )
        })
        .collect();

    let hand: BTreeSet<String> = hand_written.iter().map(|s| s.to_string()).collect();
    let mut forwardable = BTreeMap::new();
    let mut refused: BTreeMap<String, String> = BTreeMap::new();
    let mut refusals = Vec::new();
    for c in &cx.abi.commands {
        if !c.forwarded || hand.contains(&c.name) {
            continue;
        }
        let Some(wire) = cx.wire.commands.iter().find(|w| w.name == c.name) else {
            continue;
        };
        match emit_forward_command(&raise_cx, &lower_cx, c, wire) {
            Ok(body) => {
                forwardable.insert(c.name.clone(), body);
            }
            Err(reason) => {
                refused.insert(c.name.clone(), reason.clone());
                refusals.push(Refusal { command: c.name.clone(), reason });
            }
        }
    }

    let hand_cmds: Vec<&AbiCommand> =
        cx.abi.commands.iter().filter(|c| hand.contains(&c.name)).collect();

    let mut sections = vec![
        ("extensions".to_string(), emit_profile_extensions(cx, profile_extensions)),
        ("device features".to_string(), emit_device_features(cx, profile_extensions)),
        ("raise pnext".to_string(), emit_raise_pnext(&raise_cx)),
        ("lower_full pnext".to_string(), emit_lower_full_pnext(&lower_cx)),
    ];
    sections.extend(full_lowers);
    sections.extend(raises);
    // The generated forwarders **and** the hand-written ones, because
    // `DriverFns` is "every entry point the GPU side may need" and a command
    // being hand-written says nothing about whether it ends up calling the
    // driver. It said the wrong thing here for as long as every hand-written
    // command either answered locally (the session, the memory table) or went
    // through `ash` (`vkAllocateMemory`). `vkCreateBuffer` is the first that
    // must call the driver with the *registry's* signature — it raises the
    // registry's `VkBufferCreateInfo`, and reaching it through `ash` would mean
    // handing one binding's struct to a function typed by another, which is the
    // stack corruption this table exists to prevent.
    let table: BTreeSet<String> = forwardable
        .keys()
        .cloned()
        .chain(hand_cmds.iter().filter(|c| c.forwarded).map(|c| c.name.clone()))
        .collect();
    sections.push(("fns".to_string(), emit_driver_fns(cx, &table)));
    sections.push(("host".to_string(), emit_host_trait(cx, &hand_cmds)));
    sections.push((
        "server".to_string(),
        emit_server_impl(cx, &forwardable, &hand, &refused),
    ));

    (sections, Refusals { raise: raise_refused, commands: refusals })
}

/// Everything the GPU-side emitter needs from the model that is not in a plan.
pub fn contexts<'a>(
    model: &'a Model,
    abi: &'a AbiPlan,
    wire: &'a Plan,
) -> Ctx<'a> {
    Ctx::new(model, abi, wire)
}
