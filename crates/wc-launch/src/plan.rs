//! Every command line this launcher runs, built in one place and testable on
//! the machine that compiles it.
//!
//! **THE ARENA IS THE GUEST'S OWN RAM AND THERE IS NO OTHER KIND.** The
//! compositor's arena arguments are written here, in code, from the session's
//! own numbers; no configuration key and no argument reaches them. `--arena`
//! (a file), `--arena-provide` and `--shared-dir` do not appear in this file at
//! all, and `guest_ram_is_the_only_transport` is the test that says so.
//!
//! That is a hard requirement rather than a preference, and the reason is in
//! docs/design.md §19.2: the file-backed arena is stage 10's hypervisor-free
//! twin, a fixture for the Wine leg, and a launcher that could fall back to it
//! would produce a window that works and pixels that tear, with nothing in any
//! log saying which of the two ran.

use crate::config::Config;

/// The run directory on the VM host is derived from the session name and the
/// owning process, so two launchers on one guest cannot share one.
pub fn run_id(session: &str, pid: u32) -> String {
    format!("{session}-{pid}")
}

/// Where `wc-session` puts that run's files. Must agree with `scripts/wc-session`.
pub fn remote_dir(id: &str) -> String {
    format!("/tmp/wc-launch/{id}")
}

pub fn remote_upstream(id: &str) -> String {
    format!("{}/up.sock", remote_dir(id))
}

/// `wc-compositor.exe`'s arguments. Guest RAM, an outbound window, one output,
/// a socket and a log — and nothing else. In particular **no `--dump-frames`**:
/// a Firefox frame is a 2.3 MB PPM and a session is not a 60-second run
/// (§17.7).
pub fn compositor_argv(cfg: &Config, socket: &str, log: &str) -> Vec<String> {
    vec![
        "--socket".into(),
        socket.into(),
        "--log".into(),
        log.into(),
        "--output".into(),
        cfg.output.clone(),
        "--timeout".into(),
        cfg.session_secs.to_string(),
        // The two that make this an arena over the guest's own pinned pages.
        "--arena-guest-ram".into(),
        format!("0:{}", cfg.arena_bytes()),
        "--arena-outbound".into(),
        format!("0:{}", cfg.outbound),
    ]
}

/// The byte pipe from the guest's loopback into the compositor's AF_UNIX
/// socket. `--timeout 0` is "for as long as the session lasts"; every other
/// caller of this program is a test with a deadline.
pub fn relay_argv(cfg: &Config, socket_path: &str) -> Vec<String> {
    vec![cfg.port.to_string(), socket_path.into(), "--timeout".into(), "0".into()]
}

/// Options shared by every ssh this launcher runs.
///
/// `BatchMode` because there is nobody at the keyboard — a launcher that
/// blocked on a passphrase prompt would be a shortcut that does nothing — and
/// `IdentitiesOnly` because the one key that works is named on the line.
pub fn ssh_options(cfg: &Config, known_hosts: &str) -> Vec<String> {
    vec![
        "-i".into(),
        cfg.key.clone(),
        "-o".into(),
        "IdentitiesOnly=yes".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        format!("UserKnownHostsFile={known_hosts}"),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
    ]
}

/// One verb of `wc-session`, on the VM host. The words arrive there as
/// `$SSH_ORIGINAL_COMMAND`, because the key is restricted to a forced command.
pub fn ssh_exec(cfg: &Config, known_hosts: &str, verb: &[&str]) -> Vec<String> {
    let mut a = ssh_options(cfg, known_hosts);
    a.push(cfg.host.clone());
    a.extend(verb.iter().map(|s| (*s).to_string()));
    a
}

/// The tunnel: a Unix socket **on the VM host** that forwards into the guest's
/// relay port. It runs from the guest because the guest is where the click is;
/// stage 12's harness made the same connection in the other direction from a
/// workstation that is no longer in this picture.
///
/// `ExitOnForwardFailure` so that a tunnel which did not bind is a process that
/// exits rather than a session that hangs on the first pixel, and
/// `StreamLocalBindUnlink` so a socket left by a killed session is replaced
/// rather than inherited.
pub fn tunnel_argv(cfg: &Config, known_hosts: &str, id: &str) -> Vec<String> {
    let mut a = ssh_options(cfg, known_hosts);
    a.extend([
        "-N".into(),
        "-T".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "StreamLocalBindUnlink=yes".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-R".into(),
        format!("{}:127.0.0.1:{}", remote_upstream(id), cfg.port),
        cfg.host.clone(),
    ]);
    a
}

/// base64, because the application's command line crosses in
/// `$SSH_ORIGINAL_COMMAND` and a quoting bug in that path would be a shell
/// injection on the VM host rather than a wrong window.
pub fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn unb64(text: &str) -> Result<Vec<u8>, String> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for ch in text.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            other => return Err(format!("base64: {:?} is not a digit", other as char)),
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EXAMPLE};

    fn cfg() -> Config {
        Config::parse(EXAMPLE).unwrap()
    }

    /// The hard requirement, as a test rather than as a paragraph. Every
    /// spelling of "not the guest's RAM" that the compositor understands is
    /// checked for by name, so that adding one back is a red test and not a
    /// silent degrade.
    #[test]
    fn guest_ram_is_the_only_transport() {
        let cfg = cfg();
        let argv = compositor_argv(&cfg, "wl-x", "log");
        assert!(argv.iter().any(|a| a == "--arena-guest-ram"), "{argv:?}");
        assert!(argv.contains(&format!("0:{}", 256 * 1024 * 1024)));
        for forbidden in ["--arena", "--arena-provide", "--shared-dir", "--shared-peer-prefix"] {
            assert!(
                !argv.iter().any(|a| a == forbidden),
                "{forbidden} reached the compositor's command line: {argv:?}"
            );
        }
        // And the whole file, comments excepted: no *code* here spells a
        // file-backed transport, so no argument can reach one. The comments do
        // spell them, which is the point of the comments.
        let src = include_str!("plan.rs");
        let code: String = src
            .split("mod tests")
            .next()
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["shared-file", "arena-path", "arena-provide", "shared-dir"] {
            assert!(!code.contains(forbidden), "{forbidden} appears in plan.rs's code");
        }
    }

    #[test]
    fn the_compositor_gets_a_session_length_and_not_a_test_length() {
        let mut c = cfg();
        c.session_secs = 43200;
        let argv = compositor_argv(&c, "s", "l");
        let i = argv.iter().position(|a| a == "--timeout").unwrap();
        assert_eq!(argv[i + 1], "43200");
    }

    #[test]
    fn the_relay_does_not_expire() {
        let argv = relay_argv(&cfg(), r"C:\x\wl-y");
        assert_eq!(argv[0], "57431");
        assert_eq!(argv[1], r"C:\x\wl-y");
        assert_eq!(argv[3], "0");
    }

    #[test]
    fn the_tunnel_binds_the_hosts_end_of_the_run_directory() {
        let a = tunnel_argv(&cfg(), "kh", "wcl-42");
        let i = a.iter().position(|x| x == "-R").unwrap();
        assert_eq!(a[i + 1], "/tmp/wc-launch/wcl-42/up.sock:127.0.0.1:57431");
        assert!(a.iter().any(|x| x == "ExitOnForwardFailure=yes"));
        assert_eq!(a.last().unwrap(), "<host-user>@<host-nat-ip>");
    }

    #[test]
    fn a_verb_is_the_last_thing_on_the_ssh_line() {
        let a = ssh_exec(&cfg(), "kh", &["up", "wcl-42", "0x1b44", "65536", "1048576"]);
        assert_eq!(&a[a.len() - 5..], ["up", "wcl-42", "0x1b44", "65536", "1048576"]);
        assert_eq!(a[a.len() - 6], "<host-user>@<host-nat-ip>");
    }

    #[test]
    fn base64_round_trips_a_command_line_with_spaces_and_quotes() {
        for s in [
            "",
            "a",
            "ab",
            "abc",
            "/run/current-system/sw/bin/firefox --profile /home/s/.wc-launch/firefox",
            "x 'y' \"z\" $(rm -rf /) `id`",
        ] {
            let e = b64(s.as_bytes());
            assert!(!e.contains(' '), "{e}");
            assert_eq!(unb64(&e).unwrap(), s.as_bytes(), "{s}");
        }
    }

    #[test]
    fn base64_agrees_with_the_usual_answer() {
        assert_eq!(b64(b"firefox"), "ZmlyZWZveA==");
        assert_eq!(b64(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn the_run_directory_is_the_same_string_on_both_sides() {
        let id = run_id("wcl", 1234);
        assert_eq!(id, "wcl-1234");
        assert_eq!(remote_dir(&id), "/tmp/wc-launch/wcl-1234");
        assert_eq!(remote_upstream(&id), "/tmp/wc-launch/wcl-1234/up.sock");
    }
}
