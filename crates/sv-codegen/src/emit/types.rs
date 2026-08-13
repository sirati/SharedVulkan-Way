//! Pass: types, enums and constants (design.md §7.5's first row).
//!
//! Handles, enums and bitmasks are `#[repr(transparent)]` newtypes rather than
//! Rust `enum`s. That is not stylistic: a Vulkan enum received from the peer can
//! legitimately hold a value this build has never heard of — a newer driver, a
//! newer extension — and a Rust `enum` with an out-of-range discriminant is
//! instant undefined behaviour. A newtype holds it, compares it and passes it on
//! without ever claiming it is one of the variants. R7 in one design decision.

use crate::naming;
use crate::plan::{FlagWordPlan, Plan, Scalar, Shape, StructPlan};
use proc_macro2::TokenStream;
use quote::quote;

pub fn emit(plan: &Plan) -> TokenStream {
    let handles = plan.handles.iter().map(emit_handle);
    let enums = plan.enums.iter().map(|e| {
        emit_newtype(&e.name, e.bit_width, &e.values, /* bitflags */ false)
    });
    let bitmasks = plan.bitmasks.iter().map(|b| {
        emit_newtype(&b.name, b.bit_width, &b.values, /* bitflags */ true)
    });
    let flag_words = plan.flag_words.iter().map(emit_flag_word);
    let structs = plan.structs.iter().map(|s| emit_struct(plan, s));

    quote! {
        #(#handles)*
        #(#enums)*
        #(#bitmasks)*
        #(#flag_words)*
        #(#structs)*
    }
}

/// A C bitfield struct: one `u32`, with a getter and a setter per field.
///
/// Rust has no `#[repr(C)]` bitfield, so the storage unit is the type and the
/// fields are methods. That is not a compromise here — it is closer to what the
/// wire wants than a struct would be, because the word is copied end to end and
/// never taken apart in transit (see `plan::FlagWordPlan`).
///
/// The accessors shift and mask from the least significant bit upwards, in
/// declaration order, which is what the x86-64 System V ABI and MSVC x64 both do
/// for consecutive same-type bitfields. The generated `bit_layout` doc comment
/// states the offsets so a reader can check them against the header without
/// reading this generator.
fn emit_flag_word(f: &FlagWordPlan) -> TokenStream {
    let ty = naming::type_ident(&f.name);

    let accessors = f.fields.iter().map(|b| {
        let get = naming::ident(&b.rust_name);
        let set = naming::ident(&format!("set_{}", b.rust_name));
        let with = naming::ident(&format!("with_{}", b.rust_name));
        let offset = b.offset;
        let mask: u32 = if b.bits >= 32 { u32::MAX } else { (1u32 << b.bits) - 1 };
        let doc = format!(
            "`{}`: {} bit{} at offset {}.",
            b.name,
            b.bits,
            if b.bits == 1 { "" } else { "s" },
            b.offset
        );
        quote! {
            #[doc = #doc]
            pub fn #get(self) -> u32 { (self.0 >> #offset) & #mask }
            #[doc = #doc]
            pub fn #set(&mut self, value: u32) {
                self.0 = (self.0 & !(#mask << #offset)) | ((value & #mask) << #offset);
            }
            #[doc = #doc]
            pub fn #with(mut self, value: u32) -> Self { self.#set(value); self }
        }
    });

    let layout = f
        .fields
        .iter()
        .map(|b| format!("`{}` [{}..{})", b.name, b.offset, b.offset + b.bits))
        .collect::<Vec<_>>()
        .join(", ");
    let doc = format!(
        "`{}`, a C struct of `uint32_t` bitfields.\n\n\
         Carried as the one 32-bit storage unit it occupies, and copied without \
         being taken apart: the wire form does not depend on the bit order, only \
         on the two sides' C compilers agreeing about it — which they must \
         anyway, because each side's compiler has to agree with its own driver.\n\n\
         Bit layout, least significant bit first: {layout}.",
        f.name
    );

    quote! {
        #[doc = #doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        #[repr(transparent)]
        pub struct #ty(pub u32);

        impl #ty {
            #(#accessors)*
        }
    }
}

fn emit_handle(h: &crate::plan::HandlePlan) -> TokenStream {
    let name = naming::type_ident(&h.name);
    let doc = format!(
        "A Vulkan `{}` as it crosses the boundary: an app-side-assigned object \
         id, never a pointer.\n\n\
         design.md §6.4 assigns ids on the app side from a per-device counter, so \
         object creation costs no round trip, and `security-model.md` R3 requires \
         that what crosses is always a table index. The two sides do not share an \
         address space, so this is correctness before it is safety.{}",
        h.name,
        if h.dispatchable {
            "\n\nThis handle is *dispatchable*. On the app side the value the \
             application sees is a pointer to a `#[repr(C)]` struct whose first \
             word the loader overwrites (design.md §6.4, R17); that is the ICD's \
             problem, not the protocol's, and only the id appears here."
        } else {
            ""
        }
    );
    quote! {
        #[doc = #doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        #[repr(transparent)]
        pub struct #name(pub u64);

        impl #name {
            /// `VK_NULL_HANDLE`.
            pub const NULL: Self = Self(0);
            pub fn is_null(self) -> bool { self.0 == 0 }
        }
    }
}

fn emit_newtype(name: &str, bit_width: u32, values: &[(String, i64)], bitflags: bool) -> TokenStream {
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
        quote! { pub const #cn: Self = Self(#lit); }
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
            impl core::ops::BitAnd for #ty {
                type Output = Self;
                fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
            }
            impl #ty {
                pub fn contains(self, other: Self) -> bool { self.0 & other.0 == other.0 }
            }
        }
    } else {
        quote!()
    };

    let doc = format!(
        "`{name}`.\n\n\
         A newtype rather than a Rust `enum`, because a value that arrives from \
         the peer may legitimately be one this build has never heard of, and a \
         Rust `enum` holding an undeclared discriminant is undefined behaviour. \
         Holding it, comparing it and passing it on costs nothing and cannot be \
         unsound."
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

fn emit_struct(plan: &Plan, s: &StructPlan) -> TokenStream {
    if s.is_union {
        return emit_union(s);
    }
    let name = naming::type_ident(&s.name);

    let fields = s.fields.iter().map(|f| {
        let id = naming::ident(&f.rust_name);
        let ty = super::rust_type(&f.shape);
        let mut notes = Vec::new();
        if f.name != f.rust_name {
            notes.push(format!("Registry name: `{}`.", f.name));
        }
        if f.is_a_length {
            notes.push(
                "This member is another member's `len=`. It is carried on the \
                 wire for fidelity, and the generated decoder checks it against \
                 the array it describes — the wire's own count is what sizes the \
                 array, so a disagreement is reported rather than obeyed."
                    .into(),
            );
        }
        if f.optional {
            notes.push("`optional=\"true\"` in the registry.".into());
        }
        // An empty `#[doc = ""]` is a lint and a lie; omit the attribute when
        // there is nothing to say.
        let doc = if notes.is_empty() {
            quote!()
        } else {
            let text = notes.join(" ");
            quote!(#[doc = #text])
        };
        quote! {
            #doc
            pub #id: #ty,
        }
    });

    let defaults = s.fields.iter().map(|f| {
        let id = naming::ident(&f.rust_name);
        let d = super::default_expr(&f.shape);
        quote!(#id: #d,)
    });

    let s_type_doc = match (&s.s_type, s.s_type_value) {
        (Some(n), Some(v)) => format!(
            "\n\nIts `sType` is `{n}` ({v}) and is **not** a field: the encoder \
             writes it from a constant. A value of this type therefore cannot \
             carry the wrong `sType`."
        ),
        _ => String::new(),
    };
    let extends_doc = if s.extenders.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nIts `pNext` chain may carry: {}.",
            s.extenders.iter().map(|e| format!("`{e}`")).collect::<Vec<_>>().join(", ")
        )
    };
    let partial_doc = if s.needs_partial {
        "\n\nA `_partial` encoding is generated for this type, because it is \
         filled in by the implementation and the app side must not send \
         uninitialised contents (design.md §6.1, Venus's three-state output \
         validity)."
    } else {
        ""
    };
    let doc = format!("`{}`.{s_type_doc}{extends_doc}{partial_doc}", s.name);

    // PartialEq but not Eq: several structs hold floats.
    let _ = plan;
    quote! {
        #[doc = #doc]
        #[derive(Debug, Clone, PartialEq)]
        pub struct #name {
            #(#fields)*
        }

        impl Default for #name {
            fn default() -> Self {
                Self { #(#defaults)* }
            }
        }
    }
}

/// A union becomes a Rust `enum`, which is the whole reason design.md §7.4's
/// union exception did not need a table.
///
/// > "Venus hardcodes three and refuses to serialise structs containing any
/// > other; gfxstream always marshals member `[0]`."  — design.md §7.4
///
/// Both are working around C having no tag. A Rust `enum` carries one, so the
/// encoder always knows which member it holds and never has to guess. The
/// `selector=` attribute is still recorded in the model, for the validity rules
/// the ICD will need; it is not needed to serialise.
fn emit_union(s: &StructPlan) -> TokenStream {
    let name = naming::type_ident(&s.name);
    let variants = s.fields.iter().map(|f| {
        let v = naming::union_variant(&f.name);
        let ty = super::rust_type(&f.shape);
        let doc = format!("`{}`", f.name);
        quote! {
            #[doc = #doc]
            #v(#ty),
        }
    });
    let first = s.fields.first().expect("a union with no members is not a union");
    let first_variant = naming::union_variant(&first.name);
    let first_default = super::default_expr(&first.shape);
    let doc = format!(
        "`{}`, as a Rust `enum`.\n\n\
         The wire form is Venus's: a `u32` member index, then the member. The \
         index is the variant, so unlike a C generator this one never has to \
         hardcode a discriminator for a union the registry does not annotate \
         with `selector=` (design.md §7.4).",
        s.name
    );
    quote! {
        #[doc = #doc]
        #[derive(Debug, Clone, PartialEq)]
        pub enum #name {
            #(#variants)*
        }

        impl Default for #name {
            fn default() -> Self { Self::#first_variant(#first_default) }
        }
    }
}

/// Which member index a union variant occupies, for the encoder.
pub fn union_tags(s: &StructPlan) -> Vec<(TokenStream, u32)> {
    s.fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let v = naming::union_variant(&f.name);
            (quote!(#v), i as u32)
        })
        .collect()
}

/// Whether a shape needs a `Box` in the Rust representation.
pub fn is_aggregate(shape: &Shape) -> bool {
    matches!(shape, Shape::Scalar(Scalar::Struct(_)) | Shape::Scalar(Scalar::Union(_)))
}
