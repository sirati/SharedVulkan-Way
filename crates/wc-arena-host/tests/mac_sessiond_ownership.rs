//! A restarted Mac controller must not discover or tear down another harness's
//! units. The fake manager owns no real units and no VM is started.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn restart_leaves_unrelated_units_untouched() {
    let dir = std::env::temp_dir().join(format!("wc-mac-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bin = dir.join("bin");
    let runtime = dir.join("runtime");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(runtime.join("wc-mac-sessiond")).unwrap();
    let systemctl = bin.join("systemctl");
    std::fs::write(&systemctl, r#"#!/usr/bin/env bash
case " $* " in
  *' list-units '*)
    printf '%s\n' 'wc-launch-wcma-3549754-ap.service loaded active running' 'wc-launch-wcmb-3549754-bp.service loaded active running' 'wc-launch-wcmc-3549754-cp.service loaded active running' ;;
  *' is-active '*) echo inactive; exit 3 ;;
  *) exit 2 ;;
esac
"#).unwrap();
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Prove the fake manager supplies active foreign units. This is the
    // startup catalog the old controller adopted without ownership evidence.
    let catalog = Command::new(&systemctl).args(["--user", "list-units"]).output().unwrap();
    assert!(catalog.status.success());
    assert!(String::from_utf8_lossy(&catalog.stdout).contains("wc-launch-wcma-3549754-ap.service"));
    let events = dir.join("verbs");
    let session = dir.join("wc-session");
    std::fs::write(&session, "printf '%s\\n' \"$SSH_ORIGINAL_COMMAND\" >>\"$WC_TEST_EVENTS\"\necho down-ok\n").unwrap();
    // Legacy global state cannot bind a record to this install or VM.
    std::fs::write(runtime.join("wc-mac-sessiond/wcma-3549754-ap"), "invalid legacy state\n").unwrap();
    let binary = std::env::var_os("WC_MAC_SESSIOND_TEST_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_wc-arena-host").into());
    let mut child = Command::new(binary)
        .args(["mac-sessiond", "--qmp", dir.join("no-vm.sock").to_str().unwrap(), "--vm", "no-vm",
            "--session", session.to_str().unwrap(), "--sweep-secs", "0"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("WC_TEST_EVENTS", &events)
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    struct Stop<'a>(&'a mut std::process::Child);
    impl Drop for Stop<'_> {
        fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
    }
    let guard = Stop(&mut child);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains(": serving ") { let _ = ready_tx.send(()); }
        }
    });
    assert!(ready_rx.recv_timeout(Duration::from_secs(10)).is_ok(), "controller never announced readiness");
    let start = Instant::now();
    // Two controller ticks. The old startup issued three down verbs by now.
    while start.elapsed() < Duration::from_secs(5) {
        assert!(guard.0.try_wait().unwrap().is_none(), "controller exited instead of remaining available");
        assert!(!events.exists(), "restart ran an unowned session verb: {}", std::fs::read_to_string(&events).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(guard);
    assert!(runtime.join("wc-mac-sessiond/wcma-3549754-ap").exists(), "restart removed foreign state");
    let _ = std::fs::remove_dir_all(dir);
}
