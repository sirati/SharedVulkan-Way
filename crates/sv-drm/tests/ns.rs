//! `sv-drm ns` for real: an unprivileged user+mount namespace with the faux
//! sysfs entry, checked from inside by a shell probe. Skips (passes with a note)
//! where unprivileged user namespaces are disabled.
#![cfg(target_os = "linux")]

use std::process::Command;

fn ns(args: &[&str], script: &str) -> Option<std::process::Output> {
    let out = Command::new(env!("CARGO_BIN_EXE_sv-drm"))
        .arg("ns")
        .args(args)
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    if String::from_utf8_lossy(&out.stderr).contains("unshare(CLONE_NEWUSER") {
        eprintln!("skipped: no unprivileged user namespaces here");
        return None;
    }
    Some(out)
}

/// A major nothing uses (the dynamic range tops out far below).
const MAJ: &str = "511";

#[test]
fn the_namespace_shows_a_faux_render_node_and_drops_back_to_the_real_ids() {
    // SAFETY: getters.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let script = format!(
        r#"set -e
d=/sys/dev/char/{MAJ}:128/device
test -d $d/drm
case "$(readlink $d/subsystem)" in */faux) ;; *) echo bad-subsystem; exit 1;; esac
test "$(basename "$(realpath $d)")" = svdrm
test -d $d/drm/renderD128
test "$(cat /sys/dev/char/{MAJ}:128/dev)" = {MAJ}:128
grep -qx DEVNAME=dri/renderD128 /sys/dev/char/{MAJ}:128/uevent
test "$(realpath /sys/dev/char/1:3)" = /sys/devices/virtual/mem/null
test "$(ls /sys/dev/char | wc -l)" -gt 10
if touch /sys/dev/char/x 2>/dev/null; then echo writable; exit 1; fi
grep -q '^CapEff:\s*0000000000000000$' /proc/self/status
echo "ids $(id -u) $(id -g)"
"#
    );
    let Some(out) = ns(&["--dev", &format!("{MAJ}:128"), "--minor", "128"], &script) else { return };
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout: {so}\nstderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(so.contains(&format!("ids {uid} {gid}")), "{so}");
}

#[test]
fn a_state_file_names_the_device_and_a_test_node_is_a_char_device() {
    let st = std::env::temp_dir().join(format!("sv-drm-state-{}", std::process::id()));
    std::fs::write(&st, format!("{MAJ}:129\n")).unwrap();
    let script = format!(
        "set -e; test -d /sys/dev/char/{MAJ}:129/device/drm/renderD129; test -c /dev/dri/renderD129; echo ok"
    );
    let r = ns(&["--state", st.to_str().unwrap(), "--test-node", "/dev/null"], &script);
    std::fs::remove_file(&st).unwrap();
    let Some(out) = r else { return };
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
}

#[test]
fn bad_invocations_fail_clearly() {
    let run = |a: &[&str]| Command::new(env!("CARGO_BIN_EXE_sv-drm")).args(a).output().unwrap();
    let o = run(&["ns", "--dev", "x", "--", "true"]);
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("--dev"));
    let o = run(&["ns", "--dev", "511:128"]);
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("command"));
    let o = run(&["serve", "--minor", "5"]);
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("128..=191"));
    // SAFETY: getter.
    if unsafe { libc::geteuid() } != 0 {
        let o = run(&["serve"]);
        let e = String::from_utf8_lossy(&o.stderr);
        assert!(!o.status.success() && e.starts_with("sv-drm: serve must run as root"), "{e}");
    }
}
