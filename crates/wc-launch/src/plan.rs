//! Every command line this launcher runs, built in one place and testable on
//! the machine that compiles it.
//!
//! **THE ARENA IS MEMORY BOTH MACHINES MAP, AND NEVER A FILE ARENA.** The
//! compositor's arena arguments are written here, in code, from the session's
//! own numbers; no configuration key and no argument reaches them. `--arena`
//! (a file), `--arena-provide` and `--shared-dir` do not appear in this file at
//! all, and `the_arena_is_ivshmem_and_never_a_file` is the test that says so.
//!
//! §82 moved the product from the guest's pinned RAM to BAR2 of an ivshmem
//! device. That is not a step towards the file arena, though the host now
//! opens a path: the path names QEMU's backing object for the BAR, the same
//! physical pages the guest maps, exactly as `/proc/<qemu-pid>/fd/<n>` named
//! the backing object of guest RAM. A file arena is a file each side maps
//! through its own filesystem, and §10.3 measured that incoherent on a
//! virtio-fs export.
//!
//! That is a hard requirement rather than a preference, and the reason is in
//! docs/waycompose-design.md §28.2: the file-backed arena is stage 10's hypervisor-free
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

/// The compositor half of `sv-guest`, and its arguments. An ivshmem arena, an
/// outbound window, one output, a socket and a log — and nothing else. In
/// particular **no `--dump-frames`**: a Firefox frame is a 2.3 MB PPM and a
/// session is not a 60-second run (§17.7).
///
/// IVSHMEM ARENA (§82). The arena used to be `--arena-guest-ram`, the guest's
/// own pinned pages, which the VM host could reach only through QEMU's monitor
/// and descriptor table — root's, on the deployment — so every session start
/// ran a locator as root. BAR2 of an ivshmem device is the same physical pages
/// on both sides and the host reaches it through a file its group may open, so
/// the product path has no root in it. `--arena-frames` went with the locate it
/// fed. The developer harnesses keep `--arena-guest-ram`; this launcher does not.
pub fn compositor_argv(cfg: &Config, socket: &str, log: &str) -> Vec<String> {
    vec![
        // `sv-guest` is one binary with two halves and the first argument picks
        // one; `sv-guest wayland …` takes exactly the arguments the old
        // `wc-compositor.exe` took (docs/one-binary.md). It is written here
        // rather than made configurable for the same reason the arena flags
        // are: there is one compositor in this design and the launcher's job is
        // to start *it*.
        "wayland".into(),
        "--socket".into(),
        socket.into(),
        "--log".into(),
        log.into(),
        "--output".into(),
        cfg.output.clone(),
        "--timeout".into(),
        cfg.session_secs.to_string(),
        // The two that make this an arena over the device's BAR: region 0 is
        // the first ivshmem device, and the outbound window is its tail.
        "--arena-ivshmem".into(),
        "0".into(),
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

/// The `user.js` the deployment writes into the application's profile.
///
/// `user.js` rather than `prefs.js` because it is re-applied on **every** start
/// and cannot be undone by the application writing its own settings out — which
/// is what "the deployment says so" has to mean. The alternative, setting the
/// pref once at install time, lasts until the first time somebody changes it in
/// the UI and then silently stops being true.
///
/// Values are rendered by shape: `true`, `false` and integers are bare, and
/// everything else is a quoted string, because `user_pref("x", true)` and
/// `user_pref("x", "true")` are different settings and Firefox takes the second
/// one as a string.
pub fn user_js(prefs: &str) -> String {
    let mut out = String::from(
        "// Written by wc-launch on every start. See docs/waycompose-design.md §28.5.\n",
    );
    for item in prefs.split(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let Some((k, v)) = item.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() {
            continue;
        }
        let bare = v == "true" || v == "false" || v.parse::<i64>().is_ok();
        if bare {
            out.push_str(&format!("user_pref(\"{k}\", {v});\n"));
        } else {
            out.push_str(&format!("user_pref(\"{k}\", \"{}\");\n", v.replace('"', "\\\"")));
        }
    }
    out
}

/// The directory an application was told to keep its profile in, if it was told
/// at all — `--profile <dir>` or `-profile <dir>`. It is the only argument this
/// launcher looks *inside* a command line for, and it is here rather than in
/// `wc-session` so that it is tested.
pub fn profile_dir(exec: &str) -> Option<String> {
    let words: Vec<&str> = exec.split_whitespace().collect();
    words
        .iter()
        .position(|w| *w == "--profile" || *w == "-profile")
        .and_then(|i| words.get(i + 1))
        .map(|s| (*s).to_string())
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
    /// spelling of "a file arena" that the compositor understands is checked
    /// for by name, so that adding one back is a red test and not a silent
    /// degrade.
    ///
    /// IVSHMEM ARENA (§82). `--arena-guest-ram` is on the list now too, for a
    /// different reason: it is not a file, but the host can only reach it as
    /// root, and the point of §82 is that the product path has no root in it.
    /// A launcher that emitted it would start a session the host's `up` refuses.
    #[test]
    fn the_arena_is_ivshmem_and_never_a_file() {
        let cfg = cfg();
        let argv = compositor_argv(&cfg, "wl-x", "log");
        assert_eq!(argv[0], "wayland", "the one binary needs its half named first");
        let i = argv.iter().position(|a| a == "--arena-ivshmem").expect("no --arena-ivshmem");
        assert_eq!(argv[i + 1], "0");
        assert!(argv.contains(&format!("0:{}", cfg.outbound)), "{argv:?}");
        for forbidden in [
            "--arena",
            "--arena-provide",
            "--shared-dir",
            "--shared-peer-prefix",
            "--arena-guest-ram",
            "--arena-frames",
        ] {
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
        for forbidden in ["shared-file", "arena-path", "arena-provide", "shared-dir", "arena-guest-ram"] {
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
        let a = ssh_exec(&cfg(), "kh", &["up", "wcl-42", "0x1b44", "ivshmem"]);
        assert_eq!(&a[a.len() - 4..], ["up", "wcl-42", "0x1b44", "ivshmem"]);
        assert_eq!(a[a.len() - 5], "<host-user>@<host-nat-ip>");
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

    /// The user asked for one thing by name: a middle click must not paste.
    #[test]
    fn the_deployment_can_state_a_preference_and_it_is_rendered_by_shape() {
        let js = user_js("middlemouse.paste=false;browser.tabs.warnOnClose=true;ui.key.x=3;a.b=hello");
        assert!(js.contains("user_pref(\"middlemouse.paste\", false);\n"), "{js}");
        assert!(js.contains("user_pref(\"browser.tabs.warnOnClose\", true);\n"), "{js}");
        assert!(js.contains("user_pref(\"ui.key.x\", 3);\n"), "{js}");
        // A string value is quoted; `false` as a string would be a different
        // setting and Firefox would take it as one.
        assert!(js.contains("user_pref(\"a.b\", \"hello\");\n"), "{js}");
        // Nothing at all is still a valid file, so the host never has to decide.
        assert!(!user_js("").contains("user_pref"));
        assert!(!user_js(" ; ; ").contains("user_pref"));
    }

    #[test]
    fn the_shipped_configuration_turns_off_the_middle_click_paste() {
        let c = cfg();
        let app = c.app("firefox").unwrap();
        assert!(app.prefs.contains("middlemouse.paste=false"), "{}", app.prefs);
        assert!(user_js(&app.prefs).contains("user_pref(\"middlemouse.paste\", false);"));
    }

    /// The codec the host CPU can afford, stated by the deployment.
    ///
    /// docs/design.md §0.17: on a VM host with no GPU, YouTube's ABR offers AV1 and
    /// Firefox decodes it in software on two cores, at **1.25× the CPU of
    /// H.264 on the medians** of eight interleaved windows at 720p60 and
    /// 1080p60 — and far less predictably. These two preferences are what
    /// leaves `avc1` as the thing the site negotiates, measured from the
    /// player's own codec line: `avc1.4d4020 (298)`, not `av01.0.08M.08 (398)`.
    ///
    /// **`media.webm.enabled` and not `media.mediasource.vp9.enabled`**, which
    /// is the correction this test exists to carry. The obvious-looking pref
    /// does nothing: with it false, `MediaSource.isTypeSupported` still answers
    /// yes for VP9 in both containers and the site still serves AV1. Only
    /// turning WebM off removes VP9 — and AV1, which YouTube also ships in
    /// WebM — so `media.av1.enabled` is belt to its braces, for the content
    /// where AV1 arrives in MP4 instead. The cost is stated rather than hidden:
    /// this takes Opus-in-WebM with it and the site falls back to AAC.
    ///
    /// It is pinned as a test rather than shipped in `wc-launch.ini`, because
    /// the *deployment* is where it belongs — a fact about the machine the
    /// browser runs on, that no protocol carries — but §0.17 also measures that
    /// it does not fix what prompted it: the dropped frames were the 216.9 ms
    /// cross-VM present, not the decoder. Across eight windows the largest drop
    /// count belongs to *H.264*, so the codec is not what that number tracks.
    /// Turning it on is one line in the `prefs` key. Losing Opus to buy 2.5% of
    /// two cores that are not the bottleneck is a choice for whoever writes
    /// that line, so the test carries the pair and not the ini.
    #[test]
    fn the_deployment_can_state_the_codec_the_host_can_afford() {
        let js = user_js("media.av1.enabled=false;media.webm.enabled=false");
        assert_eq!(
            js,
            "// Written by wc-launch on every start. See docs/waycompose-design.md §28.5.\n\
             user_pref(\"media.av1.enabled\", false);\n\
             user_pref(\"media.webm.enabled\", false);\n"
        );
    }

    #[test]
    fn the_profile_directory_is_read_off_the_command_line_or_absent() {
        assert_eq!(
            profile_dir("firefox --profile /home/s/.wc-launch/firefox about:blank").as_deref(),
            Some("/home/s/.wc-launch/firefox")
        );
        assert_eq!(profile_dir("firefox -profile /tmp/p").as_deref(), Some("/tmp/p"));
        assert_eq!(profile_dir("firefox"), None);
        assert_eq!(profile_dir("firefox --profile"), None);
    }
}
