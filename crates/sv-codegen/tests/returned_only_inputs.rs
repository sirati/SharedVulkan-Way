//! `exceptions::RETURNED_ONLY_READ_AS_INPUT`, asked of the registry.
//!
//! Each entry says "this struct is `returnedonly`, and this member takes it as
//! an input". Both halves are facts about `vk.xml`, so both are checked against
//! it: an entry whose struct stopped being `returnedonly` is a rule applied for
//! a reason that is gone, and one whose member stopped being an input is a
//! struct whose `lower_*` now reads storage nobody initialised.

use sv_codegen::exceptions;
use vk_registry::model::Model;

fn model() -> Option<Model> {
    let path = vk_registry::registry_path().ok()?;
    let reg = vk_registry::Registry::load_from(path).ok()?;
    reg.model().ok()
}

/// The member a reason cites, as `Owner::member`.
fn cited(reason: &str) -> (&str, &str) {
    let site = reason.split_whitespace().next().expect("a reason names its site");
    site.split_once("::").expect("the site is Owner::member")
}

#[test]
fn every_returned_only_input_is_returned_only_and_is_an_input() {
    let Some(m) = model() else {
        eprintln!("no vk.xml in this environment; skipping (VK_XML is set in nix develop)");
        return;
    };
    for (name, reason) in exceptions::RETURNED_ONLY_READ_AS_INPUT {
        let s = m.struct_def(name).unwrap_or_else(|| panic!("{name}: not a struct"));
        assert!(s.returned_only, "{name} is no longer returnedonly; the entry has no reason left");

        let (owner, member) = cited(reason);
        let o = m.struct_def(owner).unwrap_or_else(|| panic!("{owner}: not a struct"));
        let mem = o.member(member).unwrap_or_else(|| panic!("{owner}::{member}: no such member"));
        assert!(
            format!("{:?}", mem.ty).contains(name),
            "{owner}::{member} is not a {name}"
        );
        assert!(
            !mem.is_output_pointer(),
            "{owner}::{member} is an output, and an output must keep lower_out_*"
        );
        assert!(!o.returned_only, "{owner} is itself returnedonly, so its member is not an input");
    }
}
