//! Test-only support: the Venus differential comparison and shared fixtures.
//!
//! docs/design.md §6.6: "The differential-test harness that compiles Venus's
//! generated C lives in a test-only crate, clearly marked." This is that crate.
//! It reads Venus's MIT-licensed generated headers as *data* — nothing from
//! Venus is compiled or vendored into the product. See `THIRD-PARTY.md`.

pub mod cheader;
pub mod diff;
pub mod venus;
pub mod xcheck;

use anyhow::Result;
use sv_codegen::plan::Plan;
use vk_registry::{Model, Profile, ResolvedProfile};

/// Build the plan a test wants to compare against, from the pinned registry.
pub fn plan_for(profile: &Profile) -> Result<Plan> {
    Ok(build_for(profile)?.plan)
}

/// Everything a cross-check needs: the model, the resolved profile and the plan.
///
/// The cross-check of [`xcheck`] compares all three against `vulkan_core.h` —
/// the model because that is where the C declarators are read, the plan because
/// that is what generates code — so it needs them together rather than one at a
/// time.
pub struct Built {
    pub model: Model,
    pub resolved: ResolvedProfile,
    pub plan: Plan,
}

pub fn build_for(profile: &Profile) -> Result<Built> {
    let registry = vk_registry::Registry::load()?;
    let model: Model = registry.model()?;
    let resolved = profile.resolve(&model)?;
    let plan = sv_codegen::plan::build(&model, &resolved)?;
    Ok(Built { model, resolved, plan })
}
