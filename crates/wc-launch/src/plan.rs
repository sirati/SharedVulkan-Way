//! Every command line this launcher runs, built in one place and testable on
//! the machine that compiles it.
//!
//! **THE ARENA IS MEMORY BOTH MACHINES MAP, AND NEVER A FILE ARENA.** The
//! compositor's arena arguments are written here, in code, from the session's
//! own numbers; no configuration key and no argument reaches them. `--arena`
//! (a file), `--arena-provide` and `--shared-dir` do not appear in this file at
//! all, and `the_arena_is_ivshmem_and_never_a_file` is the test that says so.
//!
//! §83 moved the product from the guest's pinned RAM to BAR2 of an ivshmem
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
/// IVSHMEM ARENA (§83). The arena used to be `--arena-guest-ram`, the guest's
/// own pinned pages, which the VM host could reach only through QEMU's monitor
/// and descriptor table — root's, on the deployment — so every session start
/// ran a locator as root. BAR2 of an ivshmem device is the same physical pages
/// on both sides and the host reaches it through a file its group may open, so
/// the product path has no root in it. `--arena-frames` went with the locate it
/// fed. The developer harnesses keep `--arena-guest-ram`; this launcher does not.
pub fn compositor_argv(cfg: &Config, socket: &str, log: &str) -> Vec<String> {
    let mut a: Vec<String> = vec![
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
        // §85. The session ends this long after its last window closes; the
        // launcher's `supervise` is waiting on the compositor and tears down
        // the rest when it goes.
        "--exit-when-empty".into(),
        cfg.linger_secs.to_string(),
        // The two that make this an arena over the device's BAR: region 0 is
        // the first ivshmem device, and the outbound window is its tail.
        "--arena-ivshmem".into(),
        "0".into(),
        "--arena-outbound".into(),
        format!("0:{}", cfg.outbound),
    ];
    // §89. The Wayland connections as rings in the region's tail, woken
    // through this port. The compositor decides whether it can (the port may
    // not exist) and says which in its log; the launcher reads that line.
    if !cfg.wayland_bell.is_empty() {
        a.extend(["--wayland-rings".into(), cfg.wayland_bell.clone()]);
    }
    a
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
    ]);
    // §86, §87. One reverse forward for the GPU process: `$D/cold.sock` on the
    // VM host reaches its port here. Each connection the host's
    // `sv-hostd ivshmem-serve` makes through it is one session, in its own slot.
    if cfg.gpu_sessions > 0 {
        a.extend(["-R".into(), format!("{}/cold.sock:127.0.0.1:{}", remote_dir(id), cfg.gpu_port())]);
    }
    // §86. The session's sound, the other way: a loopback port here that
    // reaches the PulseAudio monitor socket `wc-session` made for this session.
    if cfg.audio {
        a.extend(["-L".into(), format!("127.0.0.1:{}:{}/audio.sock", cfg.audio_port(), remote_dir(id))]);
    }
    a.push(cfg.host.clone());
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

// ------------------------------------------------- the title bar, §82 ------
//
// What the application is told about its own title bar is three things, and
// all three have to be true on the machine it runs on, because no Wayland
// protocol carries any of them:
//
// 1. `MOZ_GTK_TITLEBAR_DECORATION`: whether Firefox draws a title bar at all.
//    `client` for tabs in the caption — Firefox asks the compositor for
//    `Client`, and the compositor's default `auto` folds the caption of a
//    window whose client asked for that (§82). `none` for the opt-out.
// 2. Preferences: `ui.GtkCSD{Minimize,Maximize,Close}Button = 0`. **Measured,
//    Firefox 157:** these remove all three of Firefox's own window buttons,
//    close included — which `gtk-decoration-layout` could not do (§22.9,
//    §31.9). They are LookAndFeel overrides, so they need no GTK file and no
//    `XDG_CONFIG_HOME`.
// 3. A `userChrome.css` that reserves the corner Windows draws its buttons in,
//    and makes the tab strip exactly as tall as Windows' caption, so the two
//    title bars are one: the tabs sit in the band the buttons sit in.
//
// Windows draws minimise, maximise and close, and Firefox draws none. That is
// a decision with evidence behind it, and §82.3 has it: DWM's buttons were
// photographed on the guest (§31), they bring their own hover, theme, DPI and
// `HTMAXBUTTON` — so snap layouts — and Firefox's are GTK's, which on this
// desktop would be the only GNOME-styled controls on it and would have no
// `HTMAXBUTTON` for Windows to find.

/// The title-bar arrangement for one application, as `[app.*] titlebar` names
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Titlebar {
    /// Firefox's tabs in Windows' caption, Windows' buttons in its corner. The
    /// default since §82.
    Tabs,
    /// Windows' caption above, Firefox's tab strip below, no buttons of
    /// Firefox's. The arrangement §22 shipped, and the per-application opt-out.
    None,
    /// Firefox decorates itself and nothing else is done: its own buttons, no
    /// reserved corner. The compositor still folds the caption, so this is
    /// §31's picture with Firefox's close button under Windows'. Kept as the
    /// control leg, as `--decorations client` is.
    Client,
}

impl Titlebar {
    /// `[app.*] titlebar`. `server` is accepted for `none` because it is what
    /// the compositor's flag calls the same arrangement.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tabs" => Some(Self::Tabs),
            "none" | "server" => Some(Self::None),
            "client" => Some(Self::Client),
            _ => None,
        }
    }

    /// `MOZ_GTK_TITLEBAR_DECORATION`.
    pub fn moz_env(self) -> &'static str {
        match self {
            Self::Tabs | Self::Client => "client",
            Self::None => "none",
        }
    }

    /// The preferences this arrangement adds to the application's own, in
    /// `[app.*] prefs` syntax. The application's come second, so a deployment
    /// that wants to override one of these can.
    pub fn prefs(self) -> &'static str {
        match self {
            Self::Tabs => {
                "ui.GtkCSDMinimizeButton=0;ui.GtkCSDMaximizeButton=0;ui.GtkCSDCloseButton=0;\
                 toolkit.legacyUserProfileCustomizations.stylesheets=true"
            }
            Self::None | Self::Client => "",
        }
    }
}

/// Windows' caption, measured on the guest at 100%: `folds 31 px of caption`
/// and a `146x30` button strip (§31.1, §31.5). Scaled by [`TitlebarFit::at`].
const CAPTION_AT_96: i32 = 31;
const BUTTONS_AT_96: i32 = 146;

/// How tall Firefox's tab strip must be, and how much of its right-hand end it
/// must leave empty, for Windows' caption at one DPI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TitlebarFit {
    /// The caption's height in pixels — `AdjustWindowRectExForDpi`'s top inset
    /// on Windows, which is what the compositor folds in.
    pub height: i32,
    /// The caption buttons' width in pixels.
    pub reserve: i32,
}

impl TitlebarFit {
    /// The fit at `dpi`. `caption` is the measured top inset when the caller
    /// has one (the launcher does, on Windows); otherwise it is §31's 100%
    /// number, scaled.
    ///
    /// The button strip has no API that answers without a window on screen —
    /// `DWMWA_CAPTION_BUTTON_BOUNDS` needs one — so it is §31.5's measured
    /// 146 px, scaled with `MulDiv`'s rounding. A reservation a few pixels too
    /// wide is empty tab strip; one too narrow is a chevron under a button, so
    /// the rounding is up. The compositor checks the result on every window
    /// and says in its log if the client drew under the buttons anyway.
    ///
    /// **CSS pixels are assumed to be device pixels**, which they are here: the
    /// compositor's one `wl_output` is scale 1 and the launcher sets no
    /// `layout.css.devPixelsPerPx`. A host with GTK text scaling would break
    /// the assumption, and the compositor's corner check is what would say so.
    pub fn at(dpi: u32, caption: Option<i32>) -> Self {
        let scale = |v: i32| -> i32 {
            let d = i64::from(dpi.max(1));
            ((i64::from(v) * d + 95) / 96) as i32
        };
        Self {
            height: caption.filter(|h| *h > 0).unwrap_or_else(|| scale(CAPTION_AT_96)),
            reserve: scale(BUTTONS_AT_96),
        }
    }
}

/// The first line of every `userChrome.css` this launcher writes. `wc-session`
/// removes a `userChrome.css` that starts with it when an application opts out,
/// and leaves alone one that does not, because that one is the user's.
pub const USER_CHROME_MARK: &str = "/* wc-launch: tabs in the title bar.";

/// The `userChrome.css` for [`Titlebar::Tabs`], or `None` for the others.
///
/// **Measured against Firefox 157 on the Linux compositor, frame dumps of the
/// geometry rectangle** (§82.4): with this sheet at 31/146 the tab strip is 31
/// rows from the top of the window to the first row of the navigation bar
/// (44 without it), and the rightmost 146 columns of those rows are one flat
/// colour (the tab-list button and Firefox's three window buttons are there
/// without it).
///
/// * `--tab-min-height` and `--tab-margin-block` are the two variables
///   Firefox's own `--tabstrip-min-height` is computed from (`tabs.css`). The
///   margin is 1 px, so the tab is `height - 2`; at 100% that is 29 px, which
///   is the value of Firefox's own compact density, so nothing is smaller than
///   a size Firefox already ships.
/// * The reservation is a `min-width` on the button box *in the tab strip
///   only*. The navigation bar has a button box of its own that Firefox hides
///   while tabs are shown, and a selector without `#TabsToolbar` brings it back
///   and pushes the hamburger menu left — which the first version of this did.
pub fn user_chrome_css(t: Titlebar, fit: TitlebarFit) -> Option<String> {
    if t != Titlebar::Tabs {
        return None;
    }
    let tab = (fit.height - 2).max(1);
    Some(format!(
        "{USER_CHROME_MARK} Written on every start; edits are lost.\n\
         \x20  See docs/waycompose-design.md §82. Windows draws minimise, maximise and close;\n\
         \x20  this makes Firefox's tab strip the caption's height ({h} px) and leaves the\n\
         \x20  {r} px Windows draws them in empty. */\n\
         :root {{\n\
         \x20 --tab-min-height: {tab}px !important;\n\
         \x20 --tab-margin-block: 1px !important;\n\
         }}\n\
         #TabsToolbar .titlebar-buttonbox-container {{\n\
         \x20 min-width: {r}px !important;\n\
         }}\n",
        h = fit.height,
        r = fit.reserve,
    ))
}

/// The application's `user.js`: the arrangement's preferences, then its own.
pub fn app_user_js(t: Titlebar, prefs: &str) -> String {
    user_js(&format!("{};{prefs}", t.prefs()))
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
    /// IVSHMEM ARENA (§83). `--arena-guest-ram` is on the list now too, for a
    /// different reason: it is not a file, but the host can only reach it as
    /// root, and the point of §83 is that the product path has no root in it.
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

    /// §89. The rings' port goes to the compositor as `--wayland-rings`, and
    /// an empty one sends nothing — the ssh path, with nothing to fall from.
    #[test]
    fn the_compositor_is_asked_for_rings_on_the_configured_port() {
        let mut c = cfg();
        let argv = compositor_argv(&c, "wl-x", "log");
        let i = argv.iter().position(|a| a == "--wayland-rings").expect("no --wayland-rings");
        assert_eq!(argv[i + 1], "org.sirati.wcbell.0");
        c.wayland_bell.clear();
        assert!(!compositor_argv(&c, "wl-x", "log").iter().any(|a| a == "--wayland-rings"));
        // The tunnel is unchanged by it: the feeds, a fallback, the GPU's cold
        // path and the sound still need it.
        assert_eq!(tunnel_argv(&c, "kh", "wcl-1"), tunnel_argv(&cfg(), "kh", "wcl-1"));
    }

    #[test]
    fn the_compositor_gets_a_session_length_and_not_a_test_length() {
        let mut c = cfg();
        c.session_secs = 43200;
        let argv = compositor_argv(&c, "s", "l");
        let i = argv.iter().position(|a| a == "--timeout").unwrap();
        assert_eq!(argv[i + 1], "43200");
    }

    /// §85. The last window closing ends the session after `linger-secs`.
    #[test]
    fn the_compositor_is_told_to_end_the_session_after_its_last_window() {
        let mut c = cfg();
        c.linger_secs = 75;
        let argv = compositor_argv(&c, "s", "l");
        let i = argv.iter().position(|a| a == "--exit-when-empty").unwrap();
        assert_eq!(argv[i + 1], "75");
        assert_eq!(Config::default().linger_secs, 60);
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

    // --- §82: the title bar ---------------------------------------------------

    #[test]
    fn titlebar_values_parse_and_server_is_none() {
        assert_eq!(Titlebar::parse("tabs"), Some(Titlebar::Tabs));
        assert_eq!(Titlebar::parse("none"), Some(Titlebar::None));
        assert_eq!(Titlebar::parse("server"), Some(Titlebar::None));
        assert_eq!(Titlebar::parse("client"), Some(Titlebar::Client));
        assert_eq!(Titlebar::parse("Tabs"), None);
        assert_eq!(Titlebar::Tabs.moz_env(), "client");
        assert_eq!(Titlebar::None.moz_env(), "none");
    }

    /// The fit at the three scales a user picks. 100% is §31's measurement;
    /// the other two are scaled up, never down, so the reservation is never
    /// narrower than the strip it reserves for.
    #[test]
    fn the_fit_scales_with_dpi_and_rounds_up() {
        assert_eq!(TitlebarFit::at(96, None), TitlebarFit { height: 31, reserve: 146 });
        assert_eq!(TitlebarFit::at(120, None), TitlebarFit { height: 39, reserve: 183 });
        assert_eq!(TitlebarFit::at(144, None), TitlebarFit { height: 47, reserve: 219 });
        // A measured caption wins over the scaled guess; a nonsense one does not.
        assert_eq!(TitlebarFit::at(144, Some(45)).height, 45);
        assert_eq!(TitlebarFit::at(144, Some(0)).height, 47);
    }

    /// The sheet carries the numbers, is scoped to the tab strip, and is
    /// marked so `wc-session` can tell it from the user's own.
    #[test]
    fn the_user_chrome_reserves_the_corner_and_fits_the_caption() {
        let css = user_chrome_css(Titlebar::Tabs, TitlebarFit::at(96, None)).unwrap();
        assert!(css.starts_with(USER_CHROME_MARK), "{css}");
        assert!(css.contains("--tab-min-height: 29px !important;"), "{css}");
        assert!(css.contains("--tab-margin-block: 1px !important;"), "{css}");
        assert!(css.contains("#TabsToolbar .titlebar-buttonbox-container {\n  min-width: 146px"), "{css}");
        let css = user_chrome_css(Titlebar::Tabs, TitlebarFit::at(144, None)).unwrap();
        assert!(css.contains("--tab-min-height: 45px") && css.contains("min-width: 219px"), "{css}");
        assert_eq!(user_chrome_css(Titlebar::None, TitlebarFit::at(96, None)), None);
        assert_eq!(user_chrome_css(Titlebar::Client, TitlebarFit::at(96, None)), None);
    }

    /// Tabs turns Firefox's three buttons off and stylesheets on; the
    /// application's own preferences follow, so they can override.
    #[test]
    fn tabs_adds_its_preferences_before_the_applications_own() {
        let js = app_user_js(Titlebar::Tabs, "middlemouse.paste=false");
        for want in [
            "user_pref(\"ui.GtkCSDMinimizeButton\", 0);",
            "user_pref(\"ui.GtkCSDMaximizeButton\", 0);",
            "user_pref(\"ui.GtkCSDCloseButton\", 0);",
            "user_pref(\"toolkit.legacyUserProfileCustomizations.stylesheets\", true);",
        ] {
            assert!(js.contains(want), "{want} missing from\n{js}");
        }
        assert!(js.find("GtkCSDCloseButton") < js.find("middlemouse"), "{js}");
        let none = app_user_js(Titlebar::None, "middlemouse.paste=false");
        assert!(!none.contains("GtkCSD"), "{none}");
        assert!(none.contains("middlemouse.paste"), "{none}");
    }
}

#[cfg(test)]
mod audio_tests {
    use super::*;

    /// §86. The tunnel carries the session's sound as a local forward to the
    /// host's per-session socket, and only when sound is on.
    #[test]
    fn the_tunnel_forwards_the_sessions_sound_socket() {
        let mut c = Config { host: "u@h".into(), ..Config::default() };
        let a = tunnel_argv(&c, "kh", "wcl-9");
        let i = a.iter().position(|x| x == "-L").expect("an -L");
        assert_eq!(a[i + 1], format!("127.0.0.1:{}:/tmp/wc-launch/wcl-9/audio.sock", c.port + 1));
        assert_eq!(a.last().unwrap(), "u@h", "the host stays last");
        c.audio = false;
        assert!(!tunnel_argv(&c, "kh", "wcl-9").iter().any(|x| x == "-L"));
    }
}

/// §86, §87. The GPU process: `sv-guest.exe gpu guest` on the GPU arena's
/// ivshmem device, one port, up to `gpu-sessions` sessions at once in slots of
/// the BAR, and the doorbell port if the ini names one. `--idle` is how long a
/// session waits for its application to claim its rings once the host has
/// asked for it, which is seconds, not the session's life: the process lives
/// as long as the launcher session, which kills it.
pub fn gpu_argv(cfg: &Config) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "gpu".into(),
        "guest".into(),
        "--port".into(),
        cfg.gpu_port().to_string(),
        "--ivshmem-slot".into(),
        format!("{:#04x}", cfg.gpu_ivshmem_slot),
        "--arena".into(),
        (cfg.gpu_arena_mib << 20).to_string(),
        "--sessions".into(),
        cfg.gpu_sessions.to_string(),
        "--idle".into(),
        "60".into(),
    ];
    if !cfg.gpu_bell.is_empty() {
        a.extend(["--bell".into(), cfg.gpu_bell.clone()]);
    }
    a
}

#[cfg(test)]
mod gpu_tests {
    use super::*;

    #[test]
    fn the_gpu_process_has_one_port_one_forward_and_its_device() {
        let mut c = Config { host: "u@h".into(), ..Config::default() };
        assert!(!tunnel_argv(&c, "kh", "wcl-1").iter().any(|x| x.contains("cold")));
        c.gpu_sessions = 3;
        let a = tunnel_argv(&c, "kh", "wcl-1");
        let colds: Vec<&String> = a.iter().filter(|x| x.contains("/cold")).collect();
        assert_eq!(colds, [&format!("/tmp/wc-launch/wcl-1/cold.sock:127.0.0.1:{}", c.port + 10)]);
        let g = gpu_argv(&c);
        assert_eq!(&g[..6], ["gpu", "guest", "--port", &(c.port + 10).to_string(), "--ivshmem-slot", "0x1d"]);
        let after = |k: &str| g.iter().position(|x| x == k).map(|i| g[i + 1].as_str());
        assert_eq!(after("--sessions"), Some("3"));
        assert_eq!(after("--bell"), Some("org.sirati.svbell.0"));
        assert!(!g.iter().any(|x| x.contains("small-pages")), "the launcher never asks for small pages");
        c.gpu_bell.clear();
        assert!(!gpu_argv(&c).iter().any(|x| x == "--bell"));
        assert_eq!(a.last().unwrap(), "u@h");
    }
}
