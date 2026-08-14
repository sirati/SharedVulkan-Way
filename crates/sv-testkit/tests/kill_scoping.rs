//! No harness kills by a name the whole machine shares.
//!
//! # Why this is a test and not just a script
//!
//! An over-broad `pkill -9 -f weston` killed the desktop session hosting
//! 1Password, taking the machine's SSH agent and commit-signing key with it for
//! the rest of the session (§59). The user's instruction was that it must never
//! happen again, and §59.4 item 1 named the gap in the fix:
//!
//! > Nothing forces a harness to call `proc_kill`. The tests assert that the
//! > refusal works, not that anybody calls it.
//!
//! `scripts/wc-killcheck.sh` closes it. This puts it behind
//! `cargo test --workspace`, because a rule nobody runs is the same shape as
//! the defect it prevents — which is the argument §54 made for the citation
//! checker, and the reason that one has caught real breakage on four merges
//! since.
//!
//! # The rule it encodes
//!
//! A pattern is allowed only when it names something **only this run could have
//! produced** — in practice, one interpolating a directory, socket or port the
//! run owns. A literal cannot be run-unique: two concurrent runs of a harness
//! share every literal in it. Bare program names are refused.

use std::path::Path;
use std::process::Command;

fn root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn no_harness_kills_by_a_shared_name() {
    let root = root();
    let script = root.join("scripts/wc-killcheck.sh");
    assert!(
        script.is_file(),
        "{} is missing: the kill-scoping rule cannot run, and passing without it \
         would be a green result that means nothing",
        script.display()
    );

    let out = Command::new("bash")
        .arg(&script)
        .current_dir(&root)
        .output()
        .expect("run wc-killcheck.sh");

    assert!(
        out.status.success(),
        "a harness can kill a process this run did not start.\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// The gate must be able to fail, and must fail for the right reason.
///
/// Two fixtures, in that order, which is §55.4's shape: a control whose only
/// kill is correctly scoped, where the checker must **succeed** — if it does
/// not, the checker is broken in this environment and this test says so rather
/// than claiming a result about kill scoping — and then a bare-name kill, which
/// it must reject. Without the control, a checker that had grown an unmet
/// dependency would fail the second fixture and be reported as working.
#[test]
fn the_kill_gate_can_fail_and_for_the_right_reason() {
    let root = root();
    let script = root.join("scripts/wc-killcheck.sh");
    if !script.is_file() {
        return; // the test above reports this
    }

    let tmp = std::env::temp_dir().join(format!("sv-killgate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("scripts")).expect("temp dirs");
    std::fs::copy(&script, tmp.join("scripts/wc-killcheck.sh")).expect("copy the checker");

    let run = || {
        Command::new("bash")
            .arg(tmp.join("scripts/wc-killcheck.sh"))
            .current_dir(&tmp)
            .output()
            .expect("run the checker against the fixture")
    };

    // Control: a kill scoped to the run's own directory. Must pass.
    std::fs::write(
        tmp.join("scripts/good.sh"),
        "pkill -f \"^$HOST_DIR/wc-proxy\" 2>/dev/null\n",
    )
    .expect("write");
    let control = run();
    if !control.status.success() {
        let so = String::from_utf8_lossy(&control.stdout).to_string();
        let _ = std::fs::remove_dir_all(&tmp);
        panic!(
            "the kill gate rejected a fixture whose only kill is correctly scoped to \
             the run's own directory, so this test cannot say anything about its ability \
             to catch an unscoped one:\n{so}"
        );
    }

    // The shape that caused the outage. Assembled rather than written literally:
    // the checker scans `scripts/`, and this file is not in it, but the same
    // habit has already saved two other checkers from flagging their own prose.
    std::fs::write(
        tmp.join("scripts/bad.sh"),
        format!("pkill -9 -f {} 2>/dev/null\n", "weston"),
    )
    .expect("write");
    let out = run();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        !out.status.success(),
        "the kill gate accepted a bare-name kill -- the exact shape that took out \
         this machine's SSH agent. It cannot fail, so the test above proves nothing.\n{stdout}"
    );
    assert!(
        stdout.contains("bad.sh"),
        "the gate failed but did not name the offending file, so its output cannot \
         be acted on:\n{stdout}"
    );
    assert!(
        stdout.contains("proc_kill"),
        "the gate refused without saying what to use instead; a refusal that does not \
         name the alternative gets worked around:\n{stdout}"
    );
}
