//! Every `<doc>.md §N` citation in the tree resolves to exactly one section.
//!
//! # Why this is a test and not just a script
//!
//! §48 built `scripts/wc-xref.sh`, which found 49 citations naming a document
//! that does not contain the section they cite — the Wayland side pointing at
//! the Vulkan document. All 49 were fixed. §48.6 item 2 then said the obvious
//! thing about the result:
//!
//! > Nothing runs `wc-xref.sh` automatically. It is a script that must be
//! > invoked, which is exactly the property that let the citations rot.
//!
//! A checker nobody runs is a checker that reports the state of the tree on the
//! day somebody remembered it. This tree is edited by a dozen agents and rename
//! detection has moved sections between the two documents more than once, so
//! "somebody remembered" is not a schedule. `cargo test --workspace` is the gate
//! every agent is told to run before committing; this puts the citation check
//! behind it.
//!
//! # Why here
//!
//! The workspace is a virtual manifest, so there is no root package to hold a
//! repo-wide test. `sv-testkit` is the crate whose entire purpose is test-only
//! support, which makes it the least arbitrary of the available homes for a
//! check that belongs to no single crate.
//!
//! # What a failure means
//!
//! The message is the script's own output, which names every broken citation
//! with its file and line. A failure is never ambiguous about what to do: the
//! cited section does not exist in the cited document, so either the section
//! moved or the document is wrong, and both are one-line fixes.
//!
//! # What this does *not* check
//!
//! Bare `§N` with no document — 2275 of them — which means "this file's own
//! document" and would need a file-to-document map that does not exist. The
//! unqualified form is gone as of §55 and the script now fails on it; what it
//! still only reports is the ambiguous form, for §42's
//! reason that a check nobody can pass gets disabled -- 103 citations name a
//! section their document defines twice, and only de-duplicating the document
//! can fix that — no edit to the citation itself will.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The repository root, from this crate's manifest directory.
///
/// `CARGO_MANIFEST_DIR` is `…/crates/sv-testkit`, so the root is two levels up.
/// Derived rather than searched for: a walk upwards looking for `.git` finds the
/// wrong thing inside a git worktree, where `.git` is a *file*, and finds the
/// enclosing repository if this crate is ever vendored.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/sv-testkit is two levels below the workspace root")
        .to_path_buf()
}

#[test]
fn every_design_document_citation_resolves() {
    let root = repo_root();
    let script = root.join("scripts/wc-xref.sh");

    // A missing script is a failure, not a skip. It is checked in, and a run
    // that silently passed because the checker had been deleted or renamed
    // would be the §42 defect this whole line of work is about — the gate
    // reporting success for the one reason that makes it meaningless.
    assert!(
        script.is_file(),
        "{} is missing: the citation check cannot run, and passing without it \
         would be a green result that means nothing",
        script.display()
    );

    let out = Command::new("bash")
        .arg(&script)
        .current_dir(&root)
        .output()
        .unwrap_or_else(|e| panic!("could not run {}: {e}", script.display()));

    if !out.status.success() {
        // The script prints its findings on stdout and only diagnostics on
        // stderr, so both go into the message: the assertion is only useful if
        // it says *which* citations broke.
        panic!(
            "design-document citations do not resolve.\n\
             Run `scripts/wc-xref.sh --list` for the full report.\n\
             \n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

/// The check must be able to fail, which is the property §42 is about.
///
/// A citation checker that passed on anything would be worse than none: it
/// would be counted as coverage. This feeds the script a document naming a
/// section that cannot exist and requires it to say so — so a future change
/// that made the scan silently match nothing (a broken regex, a `grep` treating
/// a file as binary after a bad byte — §44.7 — or a path that no longer
/// resolves) fails here rather than turning the test above into a no-op.
#[test]
fn the_citation_check_can_fail() {
    let root = repo_root();
    let script = root.join("scripts/wc-xref.sh");
    if !script.is_file() {
        return; // the test above is the one that reports this
    }

    let tmp = std::env::temp_dir().join(format!("sv-xref-negative-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("docs")).expect("temp dirs");
    std::fs::create_dir_all(tmp.join("crates")).expect("temp dirs");
    std::fs::create_dir_all(tmp.join("scripts")).expect("temp dirs");
    std::fs::copy(&script, tmp.join("scripts/wc-xref.sh")).expect("copy the checker");

    // Two documents with one section each, and a source file citing a section
    // that neither defines.
    std::fs::write(tmp.join("docs/design.md"), "## 1. only section\n").expect("write");
    std::fs::write(tmp.join("docs/waycompose-design.md"), "## 1. only section\n").expect("write");
    // **The bait is assembled at run time, and that is not fussiness.** Written
    // as a literal it is itself a citation, in a `.rs` file the checker scans, so
    // the negative fixture broke the real check on this test's first run. The
    // alternative was an exclusion for test files, which is how a checker starts
    // having exceptions. Both fixtures below are built the same way.
    //

    // **A control fixture first, so a failure here cannot be misread.** §54.3
    // named this: the negative test copies only the script into a temporary
    // tree, so if the checker ever grows a dependency on something outside its
    // own file — a config, a second script, a helper it sources — it would fail
    // on the fixture below for that reason and be reported as "the checker
    // correctly rejected a bad citation". A negative test that passes for the
    // wrong reason is the §42 defect wearing the opposite mask.
    //
    // So the same tree is run twice: once with only sound citations, where the
    // checker must *succeed*, and then with the bait. If the control fails, the
    // checker is broken or unsatisfied in this environment and the message says
    // so instead of claiming a result about citations.
    let run = |dir: &Path| {
        Command::new("bash")
            .arg(dir.join("scripts/wc-xref.sh"))
            .current_dir(dir)
            .output()
            .expect("run the checker against the fixture")
    };

    std::fs::write(
        tmp.join("crates/sound.rs"),
        format!("// see {}/design.md {}1 for the argument\n", "docs", '\u{a7}'),
    )
    .expect("write");
    let control = run(&tmp);
    if !control.status.success() {
        let so = String::from_utf8_lossy(&control.stdout).to_string();
        let se = String::from_utf8_lossy(&control.stderr).to_string();
        let _ = std::fs::remove_dir_all(&tmp);
        panic!(
            "the citation checker rejected a fixture whose only citation is sound, so this \
             test cannot say anything about its ability to detect a bad one. The usual cause \
             is the checker having grown a dependency that the copied-into-/tmp fixture does \
             not carry.\n--- stdout ---\n{so}\n--- stderr ---\n{se}"
        );
    }

    std::fs::write(
        tmp.join("crates/bait.rs"),
        format!("// see {}/design.md {}99.99 for the argument\n", "docs", '\u{a7}'),
    )
    .expect("write");
    let out = run(&tmp);

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        !out.status.success(),
        "the citation checker passed a document citing §99.99 of a file that \
         defines only §1. It cannot fail, so the test above proves nothing.\n{stdout}"
    );
    assert!(
        stdout.contains("99.99"),
        "the checker failed but did not name the broken citation, so its output \
         cannot be acted on:\n{stdout}"
    );
}
