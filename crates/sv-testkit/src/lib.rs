//! Test-only support: the Venus differential comparison and shared fixtures.
//!
//! design.md §6.6: "The differential-test harness that compiles Venus's
//! generated C lives in a test-only crate, clearly marked." This is that crate.
//! It reads Venus's MIT-licensed generated headers as *data* — nothing from
//! Venus is compiled or vendored into the product. See `THIRD-PARTY.md`.

pub mod diff;
pub mod venus;

use anyhow::Result;
use sv_codegen::plan::Plan;
use vk_registry::{Model, Profile};

/// Build the plan a test wants to compare against, from the pinned registry.
pub fn plan_for(profile: &Profile) -> Result<Plan> {
    let registry = vk_registry::Registry::load()?;
    let model: Model = registry.model()?;
    let resolved = profile.resolve(&model)?;
    sv_codegen::plan::build(&model, &resolved)
}
