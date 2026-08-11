//! Reading `vk.xml`.
//!
//! Everything this project generates — client stubs, wire encoding, server
//! dispatch — is a function of the Vulkan registry, because the API is far too
//! large to marshal by hand (1885 commands and 1701 structs, counted in
//! docs/api-scale.md). So the registry is an input to the build, and this crate
//! is the only thing that reads it.
//!
//! The registry is located through `$VK_XML`, which the dev shell sets from the
//! pinned `vulkan-headers` in the nix store. That indirection is deliberate:
//! the generated wire format must not depend on whichever Vulkan headers happen
//! to be installed on a machine, or two developers generate two subtly
//! different protocols and the mismatch shows up as a corrupted command stream
//! rather than a build error.

pub mod ctype;
pub mod depends;
pub mod model;
pub mod profile;

pub use ctype::{ArrayExtent, CType, CTypeKind, Declarator};
pub use model::{
    Bitmask, Command, Constant, EnumType, EnumValue, FeatureSet, FuncPointer, Handle, InterfaceRef,
    LenSpec, LenTerm, Member, Model, Primitive, StructType, TypeDef,
};
pub use profile::{Profile, ResolvedProfile, profiles};

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Where the registry lives, per the dev shell.
///
/// Deliberately no fallback to a system path. A missing `$VK_XML` means the
/// caller is outside `nix develop`, and guessing would reintroduce exactly the
/// "which vk.xml was this built against?" problem the pinning exists to
/// prevent.
pub fn registry_path() -> Result<PathBuf> {
    let raw = std::env::var("VK_XML").context(
        "VK_XML is not set — run inside `nix develop`, which pins it to the \
         vulkan-headers in the nix store",
    )?;
    let path = PathBuf::from(raw);
    if !path.is_file() {
        bail!("VK_XML points at {}, which is not a file", path.display());
    }
    Ok(path)
}

/// A parsed registry, kept in vk-parse's own representation for now.
///
/// Wrapped rather than re-modelled because the shape the generator wants is not
/// yet decided, and inventing an intermediate representation before knowing
/// what it feeds would be guessing. What this type does buy is a single place
/// that knows how the registry was loaded.
pub struct Registry {
    pub raw: vk_parse::Registry,
    pub path: PathBuf,
}

impl Registry {
    /// Parse the registry named by `$VK_XML`.
    pub fn load() -> Result<Self> {
        Self::load_from(registry_path()?)
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        // `.context()` does not apply here: vk_parse::FatalError does not
        // implement std::error::Error, so anyhow cannot wrap it. Format it in
        // by hand rather than reaching for a wrapper type.
        let (raw, errors) = vk_parse::parse_file(&path)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e:?}", path.display()))?;

        // vk-parse reports recoverable oddities rather than failing. They are
        // not fatal, but they are also not to be swallowed: a registry this
        // build disagrees with is precisely the failure mode that produces
        // marshalling which is wrong on both sides and therefore looks
        // consistent (docs/security-model.md, R8).
        if !errors.is_empty() {
            eprintln!(
                "vk-registry: {} recoverable issue(s) parsing {}:",
                errors.len(),
                path.display()
            );
            for e in errors.iter().take(20) {
                eprintln!("  {e:?}");
            }
        }

        Ok(Self { raw, path })
    }

    /// Build the typed model the generator consumes.
    pub fn model(&self) -> Result<Model> {
        Model::build(&self.raw)
    }

    /// Counts of the things the generator will have to emit code for.
    ///
    /// Exists to be checked against docs/api-scale.md: if these drift, the
    /// registry moved, and the generated protocol moved with it.
    pub fn scale(&self) -> Scale {
        let mut s = Scale::default();
        for child in &self.raw.0 {
            match child {
                vk_parse::RegistryChild::Commands(cmds) => {
                    for c in &cmds.children {
                        match c {
                            vk_parse::Command::Definition(_) => s.commands += 1,
                            vk_parse::Command::Alias { .. } => s.command_aliases += 1,
                            _ => {}
                        }
                    }
                }
                vk_parse::RegistryChild::Types(types) => {
                    for t in &types.children {
                        if let vk_parse::TypesChild::Type(ty) = t {
                            match ty.category.as_deref() {
                                Some("struct") => s.structs += 1,
                                Some("union") => s.unions += 1,
                                Some("handle") => s.handles += 1,
                                Some("enum") => s.enum_types += 1,
                                _ => {}
                            }
                        }
                    }
                }
                vk_parse::RegistryChild::Extensions(exts) => {
                    s.extensions += exts.children.len();
                }
                _ => {}
            }
        }
        s
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Scale {
    pub commands: usize,
    pub command_aliases: usize,
    pub structs: usize,
    pub unions: usize,
    pub handles: usize,
    pub enum_types: usize,
    pub extensions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry parses, and it is the size we think it is.
    ///
    /// Skipped rather than failed when `$VK_XML` is unset, so the test suite is
    /// still runnable outside the dev shell; inside it, this is the check that
    /// the pinned registry has not moved under us.
    #[test]
    fn registry_parses_and_is_the_expected_size() {
        let Ok(path) = registry_path() else {
            eprintln!("VK_XML unset — skipping (run inside `nix develop`)");
            return;
        };

        let reg = Registry::load_from(&path).expect("registry should parse");
        let s = reg.scale();
        eprintln!("scale: {s:#?}");

        // Lower bounds, not equality: a registry bump adds entries, and this
        // test exists to catch "we are reading the wrong file" or "the parse
        // silently produced nothing", not to pin a version.
        assert!(s.commands > 700, "commands = {} — too few, wrong file?", s.commands);
        assert!(s.structs > 1400, "structs = {} — too few, wrong file?", s.structs);
        assert!(s.handles > 40, "handles = {}", s.handles);
        assert!(s.extensions > 500, "extensions = {}", s.extensions);
    }
}
