//! Print the whole cross-check report, rather than the first sixty lines of a
//! test failure.
//!
//! `cargo run -p sv-testkit --example xcheck_report [profile]`
//!
//! The tests in `tests/header_crosscheck.rs` are the gate; this is the thing to
//! run when one of them fails, because `security-model.md` R8 asks for *every*
//! divergence to be established as one side's bug or the other's before
//! anything is changed, and a truncated panic message is not that.

use anyhow::Result;
use sv_testkit::cheader::{self, HeaderModel};
use sv_testkit::xcheck::{self, XReport};

fn main() -> Result<()> {
    let name = std::env::args().nth(1).unwrap_or_else(|| "zink".to_string());
    let profile = match name.as_str() {
        "zink" => vk_registry::profiles::zink(),
        "step3-minimal" | "minimal" => vk_registry::profiles::step3_minimal(),
        other => anyhow::bail!("unknown profile {other:?}"),
    };

    let header = HeaderModel::load_all(&cheader::header_paths()?)?;
    let built = sv_testkit::build_for(&profile)?;

    let mut report = XReport::default();
    xcheck::compare(&built.model, &built.plan, &profile, &header, &mut report);

    println!("{}", xcheck::summary(&report));
    println!();
    for (reason, n) in &report.counts.skipped {
        println!("  skipped {n:>5}  {reason}");
    }
    println!();

    let mut last = None;
    for d in report.sorted() {
        if last != Some(d.category) {
            println!("\n=== {:?} ===", d.category);
            last = Some(d.category);
        }
        println!("  {}: {}", d.subject, d.detail);
    }
    Ok(())
}
