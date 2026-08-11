//! Passes 5 and 6: `handle_map` and `transform` (design.md §7.2).
//!
//! Both are struct-graph walks over an already-decoded value, and both exist for
//! the same reason: an id or an offset that means one thing on the app side
//! means something else on the GPU side, and every place it can hide has to be
//! visited.
//!
//! > "A generated pass that walks whole struct graphs is the only tractable way
//! > to catch all of them; hand-listing the sites is how this goes wrong."
//! > — design.md §5.3
//!
//! Only types that can actually contain something to rewrite get a walker. The
//! reachability fixpoint in [`crate::plan`] includes `pNext` extenders, because a
//! `(memory, offset)` pair hidden in a chain link is exactly the case a
//! hand-written list would miss.

use crate::naming;
use crate::plan::{Plan, Scalar, Shape, StructPlan};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

pub fn emit(plan: &Plan) -> TokenStream {
    let traits = emit_traits(plan);
    let handle_walkers = plan
        .structs
        .iter()
        .filter(|s| s.has_handles && !s.is_union)
        .map(|s| emit_handle_map(plan, s));
    let handle_unions = plan
        .structs
        .iter()
        .filter(|s| s.has_handles && s.is_union)
        .map(emit_handle_map_union);
    let transform_walkers = plan
        .structs
        .iter()
        .filter(|s| s.has_device_memory && !s.is_union)
        .map(|s| emit_transform(plan, s));
    let chain = emit_chain_walkers(plan);

    quote! {
        #traits
        #(#handle_walkers)*
        #(#handle_unions)*
        #(#transform_walkers)*
        #chain
    }
}

fn emit_traits(plan: &Plan) -> TokenStream {
    let handle_variants = plan.handles.iter().map(|h| {
        let v = naming::type_ident(&h.name);
        quote!(#v,)
    });
    let handle_names = plan.handles.iter().map(|h| {
        let v = naming::type_ident(&h.name);
        let n = &h.name;
        quote!(HandleType::#v => #n,)
    });

    quote! {
        /// Which kind of object an id refers to.
        ///
        /// Passed to the mapper so the GPU side's table can be typed: an id is a
        /// `SlotMap` key and looking one up in the wrong table is the kind of
        /// mistake a tag turns into an error (design.md §6.4).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum HandleType {
            #(#handle_variants)*
        }

        impl HandleType {
            pub fn name(self) -> &'static str {
                match self { #(#handle_names)* }
            }
        }

        /// App-side id → GPU-side driver handle.
        ///
        /// design.md §6.4: what crosses is always a table index, never a
        /// pointer. The table lives on the GPU side and this is the interface
        /// the generated walkers use to consult it.
        pub trait HandleMapper {
            fn map(&mut self, ty: HandleType, id: u64) -> sv_wire::Result<u64>;
        }

        /// The `(memory, offset)` rewrite of design.md §5.3.
        ///
        /// An app-side `VkDeviceMemory` is a sub-range of a slab; the GPU side
        /// holds one real `VkDeviceMemory` per slab. Every pair that crosses is
        /// rewritten to `(slab handle, slab_offset + app_offset)`. This is
        /// gfxstream's `deviceMemoryTransform`, and it is not optional.
        pub trait MemoryTransform {
            fn rewrite(&mut self, memory: u64, offset: u64) -> sv_wire::Result<(u64, u64)>;
        }
    }
}

pub fn handle_map_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("handle_map_{}", naming::snake_case(name))
}
pub fn transform_fn(name: &str) -> proc_macro2::Ident {
    format_ident!("transform_{}", naming::snake_case(name))
}

fn emit_handle_map(plan: &Plan, s: &StructPlan) -> TokenStream {
    let ty = naming::type_ident(&s.name);
    let id = handle_map_fn(&s.name);
    let body = s.fields.iter().filter_map(|f| {
        let field = naming::ident(&f.rust_name);
        handle_walk(plan, &f.shape, &quote!(v.#field))
    });
    let doc = format!("Replace every object id inside a `{}` with its driver handle.", s.name);
    quote! {
        #[doc = #doc]
        pub fn #id(v: &mut #ty, m: &mut dyn HandleMapper) -> sv_wire::Result<()> {
            #(#body)*
            let _ = (&v, &m);
            Ok(())
        }
    }
}

fn emit_handle_map_union(s: &StructPlan) -> TokenStream {
    // A union whose members contain handles is not a shape the profiles reach;
    // emitting a walker that silently did nothing would be worse than an
    // explicit refusal, so this is a compile error in the generated crate if it
    // ever becomes reachable.
    let ty = naming::type_ident(&s.name);
    let id = handle_map_fn(&s.name);
    quote! {
        #[allow(unused)]
        pub fn #id(_v: &mut #ty, _m: &mut dyn HandleMapper) -> sv_wire::Result<()> {
            compile_error!(
                "a union containing handles reached the handle_map pass; the \
                 selector rules of design.md §7.4 must be applied before this \
                 can be generated"
            );
        }
    }
}

fn handle_walk(plan: &Plan, shape: &Shape, access: &TokenStream) -> Option<TokenStream> {
    match shape {
        Shape::Scalar(Scalar::Handle(name)) => {
            let ht = naming::type_ident(name);
            Some(quote!(#access.0 = m.map(HandleType::#ht, #access.0)?;))
        }
        Shape::Scalar(Scalar::Struct(name)) | Shape::Scalar(Scalar::Union(name)) => {
            let sp = plan.struct_plan(name)?;
            if !sp.has_handles {
                return None;
            }
            let f = handle_map_fn(name);
            Some(quote!(#f(&mut #access, m)?;))
        }
        Shape::Optional(inner) => {
            let e = handle_walk(plan, inner, &quote!((*inner)))?;
            Some(quote!(if let Some(inner) = &mut #access { #e }))
        }
        Shape::Array { elem, .. } | Shape::FixedArray { elem, .. } => {
            let e = handle_walk(plan, elem, &quote!((*item)))?;
            Some(quote!(for item in #access.iter_mut() { #e }))
        }
        Shape::PNext => Some(quote!(handle_map_pnext(&mut #access, m)?;)),
        _ => None,
    }
}

fn emit_transform(plan: &Plan, s: &StructPlan) -> TokenStream {
    let ty = naming::type_ident(&s.name);
    let id = transform_fn(&s.name);

    // The pair itself, if this struct holds one.
    let member_names: Vec<String> = s.fields.iter().map(|f| f.name.clone()).collect();
    let mut pair = quote!();
    for f in &s.fields {
        if !matches!(&f.shape, Shape::Scalar(Scalar::Handle(h)) if h == "VkDeviceMemory") {
            continue;
        }
        let mem = naming::ident(&f.rust_name);
        match crate::exceptions::memory_offset_sibling(&member_names, &f.name) {
            Some(off) => {
                let off_id = naming::ident(&naming::snake_case(&off));
                pair = quote! {
                    let (mem, off) = m.rewrite(v.#mem.0, v.#off_id)?;
                    v.#mem.0 = mem;
                    v.#off_id = off;
                };
            }
            None => {
                // A `VkDeviceMemory` with no offset sibling still needs the id
                // rewritten, with offset zero: `vkFreeMemory` and
                // `vkGetDeviceMemoryCommitment` are this shape.
                pair = quote! {
                    let (mem, _) = m.rewrite(v.#mem.0, 0)?;
                    v.#mem.0 = mem;
                };
            }
        }
    }

    let recurse = s.fields.iter().filter_map(|f| {
        let field = naming::ident(&f.rust_name);
        transform_walk(plan, &f.shape, &quote!(v.#field))
    });

    let doc = format!(
        "Rewrite every `(memory, offset)` pair inside a `{}` from app-side \
         allocation coordinates to GPU-side slab coordinates (design.md §5.3).",
        s.name
    );
    quote! {
        #[doc = #doc]
        pub fn #id(v: &mut #ty, m: &mut dyn MemoryTransform) -> sv_wire::Result<()> {
            #pair
            #(#recurse)*
            let _ = (&v, &m);
            Ok(())
        }
    }
}

fn transform_walk(plan: &Plan, shape: &Shape, access: &TokenStream) -> Option<TokenStream> {
    match shape {
        Shape::Scalar(Scalar::Struct(name)) | Shape::Scalar(Scalar::Union(name)) => {
            let sp = plan.struct_plan(name)?;
            if !sp.has_device_memory {
                return None;
            }
            let f = transform_fn(name);
            Some(quote!(#f(&mut #access, m)?;))
        }
        Shape::Optional(inner) => {
            let e = transform_walk(plan, inner, &quote!((*inner)))?;
            Some(quote!(if let Some(inner) = &mut #access { #e }))
        }
        Shape::Array { elem, .. } | Shape::FixedArray { elem, .. } => {
            let e = transform_walk(plan, elem, &quote!((*item)))?;
            Some(quote!(for item in #access.iter_mut() { #e }))
        }
        Shape::PNext => Some(quote!(transform_pnext(&mut #access, m)?;)),
        _ => None,
    }
}

/// Chain walkers for both passes.
///
/// The `pNext` arm is what makes these correct rather than nearly correct: a
/// chain link is a struct like any other and can hold both handles and memory
/// pairs.
fn emit_chain_walkers(plan: &Plan) -> TokenStream {
    let handle_arms = plan.pnext_links.iter().filter_map(|(name, _)| {
        let sp = plan.struct_plan(name)?;
        if !sp.has_handles {
            return None;
        }
        let v = naming::pnext_variant(name);
        let f = handle_map_fn(name);
        Some(quote!(PNextLink::#v(inner) => #f(inner, m)?,))
    });
    let transform_arms = plan.pnext_links.iter().filter_map(|(name, _)| {
        let sp = plan.struct_plan(name)?;
        if !sp.has_device_memory {
            return None;
        }
        let v = naming::pnext_variant(name);
        let f = transform_fn(name);
        Some(quote!(PNextLink::#v(inner) => #f(inner, m)?,))
    });

    quote! {
        pub fn handle_map_pnext(
            links: &mut [PNextLink],
            m: &mut dyn HandleMapper,
        ) -> sv_wire::Result<()> {
            for link in links.iter_mut() {
                match link {
                    #(#handle_arms)*
                    _ => {}
                }
            }
            let _ = m;
            Ok(())
        }

        pub fn transform_pnext(
            links: &mut [PNextLink],
            m: &mut dyn MemoryTransform,
        ) -> sv_wire::Result<()> {
            for link in links.iter_mut() {
                match link {
                    #(#transform_arms)*
                    _ => {}
                }
            }
            let _ = m;
            Ok(())
        }
    }
}
