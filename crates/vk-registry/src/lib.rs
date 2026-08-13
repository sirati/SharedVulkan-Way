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

/// Where the *video std* registry lives: `video.xml`, beside `vk.xml`.
///
/// # Why a second XML file, and why this one
///
/// `vk.xml` does not define the `StdVideo*` types. It names them, and only
/// names them:
///
/// ```xml
/// <type requires="vk_video/vulkan_video_codec_h264std.h" name="StdVideoH264LevelIdc"/>
/// ```
///
/// which the model has no choice but to read as [`TypeDef::External`] — a
/// platform type like `HWND`, whose layout we cannot know. That is what stops
/// the video profile in `sv-codegen`'s planner, and it is *not* a missing case
/// in the planner: the file it reads genuinely does not say.
///
/// The definitions are in `vk_video/vulkan_video_codec_h264std.h`, and the top
/// of that header says where *it* came from:
///
/// > This header is generated from the Khronos Vulkan XML API Registry.
///
/// That registry is `video.xml`, and it ships in the same `vulkan-headers`
/// package, in the same directory as `vk.xml`. So the answer to "how should the
/// generator learn about `vk_video`" is not "parse C" and not "hand-write a
/// table of layouts". It is: **read the other XML file that Khronos already
/// publishes for exactly this purpose**, in the same schema, with the same
/// parser, from the same pinned store path.
///
/// Four properties, which are the same four `sv_testkit::cheader` gives for
/// reading `vulkan_core.h`:
///
/// 1. It is the *authoritative* source — the headers are generated from it, not
///    the other way round.
/// 2. It cannot drift from `$VK_XML`: it is derived from that path, and a
///    missing file is an error rather than a fallback.
/// 3. It needs no new parser. `vk-parse` reads it; `Model::merge_video` reuses
///    the same `build_types`/`build_enums` this crate already has.
/// 4. It puts **no second source of truth about a C layout** into the project.
///    design.md's video-decode note listed "teach the pipeline about
///    `vk_video`" as an option with the objection that it would do exactly
///    that; using Khronos's own machine-readable definition is the version of
///    the option the objection does not apply to.
///
/// The headers are still read, by `sv-testkit`'s cross-check, and they are read
/// as a *check* rather than as an input — the same relationship `vulkan_core.h`
/// has to `vk.xml`. If the two disagree, that test fails.
pub fn video_registry_path() -> Result<PathBuf> {
    let xml = registry_path()?;
    let path = xml.with_file_name("video.xml");
    if !path.is_file() {
        bail!(
            "no video.xml beside the pinned registry: expected {} next to {}. \
             The video std types (StdVideoH264*, StdVideoDecode*) are defined \
             there and nowhere else in machine-readable form; vk.xml only names \
             them. A vulkan-headers package without it cannot generate the video \
             profile.",
            path.display(),
            xml.display()
        );
    }
    Ok(path)
}

/// A parsed registry, kept in vk-parse's own representation for now.
///
/// Wrapped rather than re-modelled because the shape the generator wants is not
/// yet decided, and inventing an intermediate representation before knowing
/// what it feeds would be guessing. What this type does buy is a single place
/// that knows how the registry was loaded.
///
/// It holds *two* parsed files, because the API is described by two:
/// `vk.xml` and, for the video std types, `video.xml`. See
/// [`video_registry_path`].
pub struct Registry {
    pub raw: vk_parse::Registry,
    pub path: PathBuf,
    /// `video.xml`, when the package ships one. `None` only for a registry
    /// loaded from an explicit path that has no sibling video registry, which
    /// is a shape the tests use and the build never sees.
    pub video: Option<vk_parse::Registry>,
    pub video_path: Option<PathBuf>,
}

impl Registry {
    /// Parse the registry named by `$VK_XML`, and the `video.xml` beside it.
    pub fn load() -> Result<Self> {
        let mut r = Self::load_from(registry_path()?)?;
        let vpath = video_registry_path()?;
        r.video = Some(Self::parse_one(&vpath)?);
        r.video_path = Some(vpath);
        Ok(r)
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let raw = Self::parse_one(&path)?;
        // The sibling video registry, when there is one. Loading it here rather
        // than only in `load()` means every path that reads a registry from the
        // store — including the tests, which call `load_from(registry_path())` —
        // gets the same model, and a model that silently differs between the
        // build and the tests is worse than one that fails.
        let vpath = path.with_file_name("video.xml");
        let (video, video_path) = if vpath.is_file() {
            (Some(Self::parse_one(&vpath)?), Some(vpath))
        } else {
            (None, None)
        };

        Ok(Self { raw, path, video, video_path })
    }

    fn parse_one(path: &Path) -> Result<vk_parse::Registry> {
        // `.context()` does not apply here: vk_parse::FatalError does not
        // implement std::error::Error, so anyhow cannot wrap it. Format it in
        // by hand rather than reaching for a wrapper type.
        let (raw, errors) = vk_parse::parse_file(path)
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
        Ok(raw)
    }

    /// Build the typed model the generator consumes.
    pub fn model(&self) -> Result<Model> {
        let mut m = Model::build(&self.raw)?;
        if let Some(v) = &self.video {
            m.merge_video(v)?;
        }
        Ok(m)
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
