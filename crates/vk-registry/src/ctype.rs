//! A C declarator parser for the type strings `vk.xml` embeds.
//!
//! `vk-parse` hands us the raw declaration text of every struct member and
//! command parameter — `const uint32_t* pCode`, `char deviceName[VK_MAX_...]`,
//! `const void* const * pParams` — and stops there. Its `src/c.rs` is a
//! character tokenizer, not a declaration parser, so the indirection depth, the
//! constness at each level and the array extents are left as a string for every
//! downstream project to re-derive. A remoting layer cannot skip that step: how
//! many stars a parameter has is precisely what decides whether it is a value, a
//! pointer to one value, or a pointer to an array (docs/design.md §7.1).
//!
//! # Why this is hand-rolled rather than built on `nom`
//!
//! docs/design.md §7.1 budgets "around 400–600 lines with `nom`". The grammar this
//! has to accept turned out to be much smaller than that estimate assumed: it
//! was measured against the whole pinned registry (see `shape_of` in
//! `examples/dump_codes.rs`) and every member and parameter declaration in
//! `vk.xml` fits
//!
//! ```text
//! decl := 'const'? 'struct'? type-name ('*' 'const'?)* name ('[' extent ']')* (':' bits)?
//! ```
//!
//! with no recursion, no parenthesised declarators, no ambiguity and no
//! backtracking. A parser-combinator library for a fixed token sequence would be
//! more code and one more dependency than a scanner, so this is a scanner. The
//! exit condition of §7.1 is honoured in the other direction: this file is well
//! under the 800-line limit, and the special-case count is reported by
//! [`Declarator::parse`] failing loudly rather than guessing.
//!
//! Function-pointer typedefs are *not* handled here. `vk-parse` already gives
//! those structured (`TypeSpec::Funcpointer`), and they are not serialisable
//! anyway (docs/design.md §6.1, Venus's exclusion list), so parsing them would be
//! work in service of nothing.

use std::fmt;

/// A C type, with constness recorded at every level of indirection.
///
/// Constness matters to the generator for one reason: it is the registry's
/// (informal but consistent) marker of direction. A `const T*` parameter is an
/// input the app side writes; a non-const `T*` is callee-filled and comes back
/// in the reply. That inference is *not* trusted on its own — `vk.xml`'s
/// `optional=` and the `returnedonly` attribute are consulted too — but it is
/// the primary signal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CType {
    /// Whether *this* level is `const`.
    pub is_const: bool,
    pub kind: CTypeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CTypeKind {
    /// `void`, only ever meaningful under a pointer.
    Void,
    /// A named type: `uint32_t`, `VkBuffer`, `char`, `StdVideoH264PictureInfo`.
    Named(String),
    /// `T*`
    Pointer(Box<CType>),
    /// `T[extent]`
    Array(Box<CType>, ArrayExtent),
}

/// The extent of a C array member.
///
/// `vk.xml` writes extents either as a literal or as the name of an API
/// constant (`VK_MAX_PHYSICAL_DEVICE_NAME_SIZE`). The constant is left
/// unresolved here because resolving it needs the constant table, which lives a
/// layer up in [`crate::model`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ArrayExtent {
    Literal(u64),
    Constant(String),
}

impl fmt::Display for ArrayExtent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArrayExtent::Literal(n) => write!(f, "{n}"),
            ArrayExtent::Constant(c) => write!(f, "{c}"),
        }
    }
}

/// A parsed declaration: a name plus the type it denotes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Declarator {
    pub name: String,
    pub ty: CType,
    /// `uint32_t mask:8`. Present only in the ray-tracing and micromap structs,
    /// which docs/design.md §7.4 puts out of scope; recorded rather than dropped so
    /// the generator can reject them by name instead of silently mis-encoding
    /// them.
    pub bitfield_bits: Option<u32>,
}

impl CType {
    pub fn named(name: impl Into<String>) -> Self {
        CType { is_const: false, kind: CTypeKind::Named(name.into()) }
    }

    /// How many `*` are between this type and its ultimate pointee.
    pub fn indirection(&self) -> usize {
        match &self.kind {
            CTypeKind::Pointer(inner) => 1 + inner.indirection(),
            _ => 0,
        }
    }

    /// The type reached by stripping every pointer and array level.
    pub fn base(&self) -> &CType {
        match &self.kind {
            CTypeKind::Pointer(inner) | CTypeKind::Array(inner, _) => inner.base(),
            _ => self,
        }
    }

    /// The name of the ultimate pointee, if it is a named type.
    pub fn base_name(&self) -> Option<&str> {
        match &self.base().kind {
            CTypeKind::Named(n) => Some(n.as_str()),
            _ => None,
        }
    }

    pub fn is_void(&self) -> bool {
        matches!(self.kind, CTypeKind::Void)
    }

    pub fn as_pointer(&self) -> Option<&CType> {
        match &self.kind {
            CTypeKind::Pointer(inner) => Some(inner),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<(&CType, &ArrayExtent)> {
        match &self.kind {
            CTypeKind::Array(inner, ext) => Some((inner, ext)),
            _ => None,
        }
    }
}

impl fmt::Display for CType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Constness binds to the left in C, so a const *pointee* is written
        // before the type and a const *pointer* after the star. Getting that
        // backwards would make this `Display` disagree with the input it was
        // parsed from, which is exactly what the round-trip test checks.
        match &self.kind {
            CTypeKind::Void | CTypeKind::Named(_) if self.is_const => write!(f, "const ")?,
            _ => {}
        }
        match &self.kind {
            CTypeKind::Void => write!(f, "void"),
            CTypeKind::Named(n) => write!(f, "{n}"),
            CTypeKind::Pointer(inner) => {
                write!(f, "{inner}*")?;
                if self.is_const {
                    write!(f, " const")?;
                }
                Ok(())
            }
            CTypeKind::Array(inner, ext) => write!(f, "{inner}[{ext}]"),
        }
    }
}

/// A declaration this parser refuses to guess at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub input: String,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot parse C declaration {:?}: {}", self.input, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Number(u64),
    Star,
    LBracket,
    RBracket,
    Colon,
    Semi,
}

fn tokenize(input: &str) -> Result<Vec<Tok>, ParseError> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len() && {
                let c = bytes[i] as char;
                c.is_ascii_alphanumeric() || c == '_'
            } {
                i += 1;
            }
            out.push(Tok::Ident(input[start..i].to_string()));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                i += 1;
            }
            let text = &input[start..i];
            let n = text.parse::<u64>().map_err(|_| ParseError {
                input: input.to_string(),
                message: format!("integer literal {text} does not fit in u64"),
            })?;
            out.push(Tok::Number(n));
        } else {
            out.push(match c {
                '*' => Tok::Star,
                '[' => Tok::LBracket,
                ']' => Tok::RBracket,
                ':' => Tok::Colon,
                ';' => Tok::Semi,
                other => {
                    return Err(ParseError {
                        input: input.to_string(),
                        message: format!("unexpected character {other:?}"),
                    });
                }
            });
            i += 1;
        }
    }
    Ok(out)
}

impl Declarator {
    /// Parse one member or parameter declaration.
    ///
    /// Everything this cannot express is an error rather than a best guess,
    /// because a declarator misread by one star produces marshalling that is
    /// wrong in the same way on both sides and therefore looks consistent —
    /// which is exactly R8 in `security-model.md`.
    pub fn parse(input: &str) -> Result<Declarator, ParseError> {
        let toks = tokenize(input)?;
        let err = |m: &str| ParseError { input: input.to_string(), message: m.to_string() };
        let mut i = 0;

        // --- specifier-qualifier list -----------------------------------
        // `const` and `struct` may both appear, in that order, and `struct` is
        // a pure elaborated-type-specifier here: `const struct Foo* p`.
        let mut base_const = false;
        loop {
            match toks.get(i) {
                Some(Tok::Ident(w)) if w == "const" => {
                    base_const = true;
                    i += 1;
                }
                Some(Tok::Ident(w)) if w == "struct" => {
                    i += 1;
                }
                _ => break,
            }
        }
        let base_name = match toks.get(i) {
            Some(Tok::Ident(w)) => {
                i += 1;
                w.clone()
            }
            _ => return Err(err("expected a type name")),
        };
        let mut ty = CType {
            is_const: base_const,
            kind: if base_name == "void" {
                CTypeKind::Void
            } else {
                CTypeKind::Named(base_name)
            },
        };

        // --- pointer chain ----------------------------------------------
        // A `const` *after* a star qualifies the pointer itself
        // (`const void* const * p`), so it belongs to the level being built,
        // not to the pointee.
        while matches!(toks.get(i), Some(Tok::Star)) {
            i += 1;
            let mut ptr_const = false;
            if let Some(Tok::Ident(w)) = toks.get(i)
                && w == "const" {
                    ptr_const = true;
                    i += 1;
                }
            ty = CType { is_const: ptr_const, kind: CTypeKind::Pointer(Box::new(ty)) };
        }

        // --- the declared name ------------------------------------------
        let name = match toks.get(i) {
            Some(Tok::Ident(w)) => {
                i += 1;
                w.clone()
            }
            _ => return Err(err("expected a declarator name")),
        };

        // --- array extents ----------------------------------------------
        // Written outermost-first in C (`float matrix[3][4]` is 3 rows of 4),
        // so they are collected in source order and applied in reverse.
        let mut extents = Vec::new();
        while matches!(toks.get(i), Some(Tok::LBracket)) {
            i += 1;
            let ext = match toks.get(i) {
                Some(Tok::Number(n)) => ArrayExtent::Literal(*n),
                Some(Tok::Ident(c)) => ArrayExtent::Constant(c.clone()),
                _ => return Err(err("expected an array extent")),
            };
            i += 1;
            if !matches!(toks.get(i), Some(Tok::RBracket)) {
                return Err(err("unterminated array extent"));
            }
            i += 1;
            extents.push(ext);
        }
        for ext in extents.into_iter().rev() {
            ty = CType { is_const: false, kind: CTypeKind::Array(Box::new(ty), ext) };
        }

        // --- bitfield ----------------------------------------------------
        let mut bitfield_bits = None;
        if matches!(toks.get(i), Some(Tok::Colon)) {
            i += 1;
            match toks.get(i) {
                Some(Tok::Number(n)) => {
                    bitfield_bits = Some(*n as u32);
                    i += 1;
                }
                _ => return Err(err("expected a bitfield width")),
            }
        }

        // A trailing `;` is present on some members and is not information.
        if matches!(toks.get(i), Some(Tok::Semi)) {
            i += 1;
        }
        if i != toks.len() {
            return Err(err(&format!("{} trailing token(s)", toks.len() - i)));
        }

        Ok(Declarator { name, ty, bitfield_bits })
    }
}

/// Parse a command's `<proto>` text, `"VkResult vkCreateBuffer"`.
///
/// Shares the declarator grammar exactly — a prototype is a declaration whose
/// name happens to be the function's — so it is not a second parser.
pub fn parse_proto(input: &str) -> Result<(String, CType), ParseError> {
    let d = Declarator::parse(input)?;
    if d.bitfield_bits.is_some() {
        return Err(ParseError {
            input: input.to_string(),
            message: "a prototype cannot be a bitfield".into(),
        });
    }
    Ok((d.name, d.ty))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Declarator {
        Declarator::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn plain_scalar() {
        let d = p("uint32_t width");
        assert_eq!(d.name, "width");
        assert_eq!(d.ty, CType::named("uint32_t"));
        assert_eq!(d.ty.indirection(), 0);
    }

    #[test]
    fn const_pointer_to_array() {
        let d = p("const uint32_t*                pCode");
        assert_eq!(d.name, "pCode");
        assert_eq!(d.ty.indirection(), 1);
        assert!(!d.ty.is_const, "the pointer itself is not const");
        assert!(d.ty.as_pointer().unwrap().is_const, "the pointee is const");
        assert_eq!(d.ty.base_name(), Some("uint32_t"));
    }

    #[test]
    fn mutable_pointer_is_callee_filled() {
        let d = p("VkBuffer* pBuffer");
        assert_eq!(d.ty.indirection(), 1);
        assert!(!d.ty.as_pointer().unwrap().is_const);
        assert_eq!(d.ty.base_name(), Some("VkBuffer"));
    }

    #[test]
    fn double_pointer() {
        let d = p("void** ppData");
        assert_eq!(d.name, "ppData");
        assert_eq!(d.ty.indirection(), 2);
        assert!(d.ty.base().is_void());
    }

    /// `const char* const*` — the shape of `ppEnabledExtensionNames`, and the
    /// one place the *pointer* is const rather than the pointee.
    #[test]
    fn array_of_strings() {
        let d = p("const char* const*      ppEnabledExtensionNames");
        assert_eq!(d.ty.indirection(), 2);
        let outer = d.ty.as_pointer().unwrap();
        assert!(outer.is_const, "the inner pointers are const");
        assert!(outer.as_pointer().unwrap().is_const, "the chars are const");
        assert_eq!(d.ty.base_name(), Some("char"));
    }

    /// `const void* const *`, with the space before the star, appears verbatim
    /// in the registry (`pParams`, `pExtras`).
    #[test]
    fn const_void_const_star_with_space() {
        let d = p("const void* const *    pParams");
        assert_eq!(d.ty.indirection(), 2);
        assert!(d.ty.base().is_void());
    }

    #[test]
    fn fixed_char_array_with_named_extent() {
        let d = p("char        deviceName[VK_MAX_PHYSICAL_DEVICE_NAME_SIZE]");
        let (elem, ext) = d.ty.as_array().unwrap();
        assert_eq!(*elem, CType::named("char"));
        assert_eq!(*ext, ArrayExtent::Constant("VK_MAX_PHYSICAL_DEVICE_NAME_SIZE".into()));
    }

    #[test]
    fn fixed_array_with_literal_extent() {
        let d = p("float x[4]");
        let (_, ext) = d.ty.as_array().unwrap();
        assert_eq!(*ext, ArrayExtent::Literal(4));
    }

    /// `float matrix[3][4]` is 3 arrays of 4 floats, not 4 of 3.
    #[test]
    fn two_dimensional_array_nests_outermost_first() {
        let d = p("float matrix[3][4]");
        let (row, outer) = d.ty.as_array().unwrap();
        assert_eq!(*outer, ArrayExtent::Literal(3));
        let (elem, inner) = row.as_array().unwrap();
        assert_eq!(*inner, ArrayExtent::Literal(4));
        assert_eq!(*elem, CType::named("float"));
    }

    #[test]
    fn bitfield_is_recorded_not_dropped() {
        let d = p("uint32_t mask:8");
        assert_eq!(d.bitfield_bits, Some(8));
        assert_eq!(d.ty, CType::named("uint32_t"));
    }

    #[test]
    fn elaborated_struct_specifier() {
        let d = p("const struct AHardwareBuffer* buffer");
        assert_eq!(d.ty.indirection(), 1);
        assert_eq!(d.ty.base_name(), Some("AHardwareBuffer"));
    }

    #[test]
    fn proto_is_the_same_grammar() {
        let (name, ret) = parse_proto("VkResult vkCreateBuffer").unwrap();
        assert_eq!(name, "vkCreateBuffer");
        assert_eq!(ret, CType::named("VkResult"));
        let (name, ret) = parse_proto("void vkDestroyBuffer").unwrap();
        assert_eq!(name, "vkDestroyBuffer");
        assert!(ret.is_void());
    }

    #[test]
    fn rejects_rather_than_guesses() {
        for bad in ["", "*", "const", "uint32_t", "uint32_t x[", "uint32_t x]", "uint32_t x y z"] {
            assert!(Declarator::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn display_round_trips_the_interesting_shapes() {
        for s in ["const uint32_t*", "const char* const*", "void**", "const void* const*"] {
            let d = Declarator::parse(&format!("{s} x")).unwrap();
            assert_eq!(d.ty.to_string().replace(' ', ""), s.replace(' ', ""));
        }
        let d = Declarator::parse("char deviceName[4]").unwrap();
        assert_eq!(d.ty.to_string(), "char[4]");
    }
}
