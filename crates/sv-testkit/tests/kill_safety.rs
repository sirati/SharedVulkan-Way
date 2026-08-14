//! `proc_kill` refuses anything that is not a pid.
//!
//! # Why this is a test
//!
//! An over-broad `pkill -9 -f weston`, meant for one harness's leftovers,
//! matched the desktop session hosting 1Password and killed it — taking the SSH
//! agent for two machines and the commit-signing key with it. That is the
//! fourth instance in one session of a process-name match catching more than it
//! meant to, and the first to cost anything outside its own run.
//!
//! `scripts/wc-binaries.sh` grew `proc_kill`/`proc_kill_pidfile` in response:
//! they take a pid or a pidfile and refuse a name or a pattern. A refusal that
//! nobody exercises is a refusal that can be softened by accident, so it is
//! checked here — the same argument §54 made for the citation checker, which
//! also lived in a script nothing ran until it was put behind `cargo test`.
//!
//! The positive path (actually killing a process) is exercised by the harnesses
//! themselves; what is asserted here is the part that has to hold even when
//! somebody is in a hurry: **a name is not a pid, and the function says so.**

use std::process::Command;

fn run_snippet(body: &str) -> (bool, String) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf();
    let script = format!(
        "set -uo pipefail\nROOT={root}\n. {root}/scripts/wc-binaries.sh\n{body}\n",
        root = root.display()
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(script)
        .output()
        .expect("run bash");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

/// The exact shape that caused the outage: a bare program name.
#[test]
fn a_program_name_is_not_a_pid_and_is_refused() {
    for victim in ["weston", "qemu", "firefox", "sv-hostd", ""] {
        let (_, out) = run_snippet(&format!(
            "if proc_kill '{victim}' 'the thing'; then echo ACCEPTED; else echo REFUSED; fi"
        ));
        assert!(
            out.contains("REFUSED"),
            "proc_kill accepted {victim:?} as a pid. A name matches processes this run \
             did not start, which is how an SSH agent and commit signing were lost.\n{out}"
        );
    }
}

/// pid 1 is not ours to stop, and neither is anything below it.
#[test]
fn init_is_refused() {
    let (_, out) = run_snippet("if proc_kill 1 'init'; then echo ACCEPTED; else echo REFUSED; fi");
    assert!(out.contains("REFUSED"), "proc_kill accepted pid 1:\n{out}");
}

/// A pidfile that does not hold a pid is left alone rather than guessed at —
/// a truncated or half-written pidfile is a pattern one indirection along.
#[test]
fn a_pidfile_without_a_pid_is_refused_and_kept() {
    let (_, out) = run_snippet(
        "f=$(mktemp); echo weston > \"$f\"; \
         proc_kill_pidfile \"$f\" 'junk' || true; \
         [ -f \"$f\" ] && echo KEPT; rm -f \"$f\"",
    );
    assert!(
        out.contains("KEPT"),
        "a pidfile holding a name was consumed rather than refused:\n{out}"
    );
}
