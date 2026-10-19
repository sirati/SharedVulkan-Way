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
    // §102. Only when asked: without it the compositor's globals are exactly
    // the ones every earlier session was measured with.
    if cfg.nested_globals {
        a.push("--nested-compositor-globals".into());
    }
    // §106. Host integration: the compositor serves the `OWNER_HOSTINT`
    // slot with this path map and browser once the session service opens it.
    a.extend(hostint_argv(&cfg.hostint));
    a
}

/// §106. The compositor's `--hostint*` flags for `[hostint]`.
pub fn hostint_argv(h: &crate::config::HostInt) -> Vec<String> {
    let mut a = vec!["--hostint".to_string()];
    let map = h.path_map();
    for (host, linux) in &map.shares {
        a.extend(["--hostint-share".into(), format!("{host}={linux}")]);
    }
    if let Some((host, linux)) = &map.drop {
        a.extend(["--hostint-drop".into(), format!("{host}={linux}")]);
    }
    if !h.browser.is_empty() {
        a.extend(["--hostint-browser".into(), h.browser.clone()]);
        for x in &h.browser_args {
            a.extend(["--hostint-browser-arg".into(), x.clone()]);
        }
    }
    a
}

/// §92. The compositor's `--relaunch-dir`: the directory this installation
/// writes each application's relaunch file into (`taskbar::Relaunch`). A
/// window whose identity has one there can be pinned as that application; any
/// other window still cannot.
/// §105. What asks the VM host for the session, now that no ssh does: the
/// compositor writes a boot record for run `id` of `installation`, signed with
/// the key in `key_file`, into the ivshmem header page, and serves this
/// launcher's requests on `control` once the host has answered. With `audio`
/// it also plays the session's sound ring.
pub fn boot_argv(cfg: &Config, id: &str, key_file: &str, control: &str) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--boot-session".into(),
        id.into(),
        "--boot-install".into(),
        cfg.installation().into(),
        "--boot-key".into(),
        key_file.into(),
        "--control-socket".into(),
        control.into(),
    ];
    if cfg.audio {
        a.extend(["--session-audio".into(), cfg.audio_volume.to_string()]);
    }
    a
}

/// §107. The named mutexes (`Local\…`) the user's one compositor and one GPU
/// process hold for their lives: a launcher that finds one held uses that
/// process instead of starting its own.
pub const COMPOSITOR_MUTEX: &str = "waycompose-compositor";
pub const GPU_MUTEX: &str = "waycompose-gpu";
/// §107. How long the user's GPU process lives with no session leasing from it.
pub const GPU_IDLE_SECS: u64 = 120;

/// §107. The user's compositor, which serves every installation: §105's
/// arguments with no session of its own, its socket for the launchers' `boot`
/// lines, and the mutex that makes it the only one. The nested-compositor
/// globals are asked for per session, in the boot (`boot_request`).
pub fn shared_compositor_argv(cfg: &Config, log: &str, user_ctl: &str) -> Vec<String> {
    let mut a = compositor_argv(cfg, "wl-shared", log);
    a.retain(|x| x != "--nested-compositor-globals");
    a.extend(["--user-control".into(), user_ctl.into(), "--singleton".into(), COMPOSITOR_MUTEX.into()]);
    // §110: the GPU pool in the same process, so the arena and the GPU device
    // are one pool. Everything after `--with-gpu` is the GPU half's.
    if one_pool_gpu(cfg) {
        a.push("--with-gpu".into());
        a.extend(gpu_argv(cfg).into_iter().skip(1));
    }
    a
}

/// §110: whether the shared compositor runs the GPU pool itself.
pub fn one_pool_gpu(cfg: &Config) -> bool {
    cfg.shared_compositor && cfg.one_pool && cfg.gpu_sessions > 0 && cfg.gpu_pool
}

/// §107. The `boot` line for the user's compositor: tab-separated `key=value`
/// (a Windows path may hold spaces and never a tab). The compositor's
/// `session_svc::Config::parse_request` reads it; the test below pins the
/// line both sides agree on.
pub fn boot_request(cfg: &Config, id: &str, key_file: &str, control: &str, gpu: bool, relaunch: &str, taskbar: Option<&str>) -> String {
    let mut f = vec![
        "boot".to_string(),
        format!("session={id}"),
        format!("install={}", cfg.installation()),
        format!("key={key_file}"),
        format!("control={control}"),
    ];
    if gpu {
        f.push("gpu=1".into());
    }
    if cfg.audio {
        f.push(format!("audio={}", cfg.audio_volume));
    }
    if cfg.nested_globals {
        f.push("nested=1".into());
    }
    f.push(format!("relaunch={relaunch}"));
    // §92.12: the installation's taskbar scope. `None` only for the retry
    // against a running compositor from before §92.12, which refuses the field.
    if let Some(t) = taskbar {
        f.push(format!("taskbar={t}"));
    }
    f.join("\t")
}

/// §107. The user's GPU process: §97's pool, held by its mutex, ending after
/// [`GPU_IDLE_SECS`] with no session rather than with any installation's.
pub fn shared_gpu_argv(cfg: &Config) -> Vec<String> {
    let mut a = gpu_argv(cfg);
    a.extend(["--singleton".into(), GPU_MUTEX.into(), "--exit-idle".into(), GPU_IDLE_SECS.to_string()]);
    a
}

/// §105. A request as the control ring takes it: the host puts this session's
/// id after the verb itself, so the id the ssh path sent second is dropped.
pub fn without_id(verb: &[String]) -> Vec<String> {
    let mut v = verb.to_vec();
    if v.len() > 1 {
        v.remove(1);
    }
    v
}

pub fn relaunch_argv(dir: &str) -> Vec<String> {
    vec!["--relaunch-dir".into(), dir.into()]
}

/// §92.12. The one-session compositor's `--taskbar-scope`: this installation,
/// so its windows carry this installation's taskbar identity.
pub fn taskbar_argv(spec: &str) -> Vec<String> {
    vec!["--taskbar-scope".into(), spec.into()]
}

/// §92.12. This installation's taskbar scope: its name, and the window app
/// ids of the applications whose `taskbar-id` is `legacy`. `window_id` gives
/// an application's window app id from its section and block (the launcher
/// reads the host's cached answer for `StartupWMClass`). An id the scope
/// cannot carry could not match a window either, and is left out.
pub fn taskbar_scope(cfg: &Config, window_id: impl Fn(&str, &crate::config::App) -> String) -> crate::taskbar::Scope {
    let mut legacy: Vec<String> = Vec::new();
    for (section, app) in &cfg.apps {
        if app.taskbar_id == crate::config::TaskbarId::Legacy {
            let id = window_id(section, app);
            if crate::taskbar::app_id_ok(&id) && !legacy.contains(&id) {
                legacy.push(id);
            }
        }
    }
    crate::taskbar::Scope { install: cfg.installation().to_string(), legacy }
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
    // No forward for the GPU process: its cold path is the pool's mailboxes
    // (`sv_transport::poolcold`), read by the VM host through the device.
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
    /// §103.8: tabs in a macOS window's title bar, the Mac way round: AppKit
    /// keeps its own close, minimise and zoom buttons at the top *left* (as
    /// Windows keeps its caption buttons with [`Titlebar::Tabs`]), so the tab
    /// strip leaves that corner empty and GTK draws no buttons of its own.
    MacTabs,
}

impl Titlebar {
    /// `[app.*] titlebar`. `server` is accepted for `none` because it is what
    /// the compositor's flag calls the same arrangement.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tabs" => Some(Self::Tabs),
            "none" | "server" => Some(Self::None),
            "client" => Some(Self::Client),
            "mac-tabs" => Some(Self::MacTabs),
            _ => None,
        }
    }

    /// `MOZ_GTK_TITLEBAR_DECORATION`.
    pub fn moz_env(self) -> &'static str {
        match self {
            Self::Tabs | Self::Client | Self::MacTabs => "client",
            Self::None => "none",
        }
    }

    /// The preferences this arrangement adds to the application's own, in
    /// `[app.*] prefs` syntax. The application's come second, so a deployment
    /// that wants to override one of these can.
    pub fn prefs(self) -> &'static str {
        match self {
            Self::Tabs | Self::MacTabs => {
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

/// The traffic lights' corner at 1x: three 12 pt buttons, 8 pt apart, 20 pt
/// in from the edge, and a margin before the first tab.
pub const MAC_BUTTONS_AT_1X: i32 = 78;

/// The `userChrome.css` for [`Titlebar::Tabs`] and [`Titlebar::MacTabs`], or `None` for the others.
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
    if t == Titlebar::MacTabs {
        let tab = (fit.height - 2).max(1);
        return Some(format!(
            "{USER_CHROME_MARK} Written on every start; edits are lost.\n\
             \x20  See docs/waycompose-design.md §103.8. macOS draws close, minimise and zoom\n\
             \x20  in the top-left {r} px; the tab strip starts after them and has the\n\
             \x20  caption's height ({h} px). */\n\
             :root {{\n\
             \x20 --tab-min-height: {tab}px !important;\n\
             \x20 --tab-margin-block: 1px !important;\n\
             }}\n\
             #TabsToolbar {{\n\
             \x20 padding-inline-start: {r}px !important;\n\
             }}\n\
             #TabsToolbar .titlebar-buttonbox-container {{\n\
             \x20 display: none !important;\n\
             }}\n",
            h = fit.height,
            r = MAC_BUTTONS_AT_1X,
        ));
    }
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

/// The first line of the GTK sheet this launcher writes (§91.8).
pub const GTK_CSS_MARK: &str = "/* wc-launch: GTK windows under Windows' caption.";

/// The `gtk.css` for [`Titlebar::Tabs`], or `None` for the others.
///
/// §91.8. The compositor folds Windows' caption into every window whose client
/// draws its own title bar, and that includes the GTK dialogs Firefox opens
/// (the file chooser, Save As, Print): their header bar is the caption. Only
/// Firefox's main window had its right-hand corner reserved, by
/// [`user_chrome_css`], so a dialog's rightmost buttons (the file chooser's
/// *Open*) sat under Windows' minimise, maximise and close. This pads every
/// header bar's end by the same reservation. `headerbar` is the node name in
/// GTK 3.20 and later and in GTK 4. The top corners are squared because the
/// window is opaque to GDI, so the transparent pixels outside a rounded corner
/// were drawn black.
///
/// Measured with Firefox 157's GTK3 file chooser on the Linux compositor
/// (frame dumps): without the sheet, the rightmost 146 columns of the header
/// bar hold Search and Open; with it, Open ends 146 px from the edge and those
/// columns are header-bar background.
pub fn gtk_css(t: Titlebar, fit: TitlebarFit) -> Option<String> {
    if t == Titlebar::MacTabs {
        return Some(format!(
            "{GTK_CSS_MARK} Written on every start; edits are lost.\n\
             \x20  See docs/waycompose-design.md §103.8. macOS draws close, minimise and zoom\n\
             \x20  in the leftmost {r} px of a folded title bar; a header bar leaves them empty\n\
             \x20  and draws no buttons of its own. */\n\
             headerbar {{\n\
             \x20 padding-left: {r}px;\n\
             \x20 border-top-left-radius: 0;\n\
             \x20 border-top-right-radius: 0;\n\
             }}\n\
             headerbar windowcontrols, headerbar .titlebutton {{\n\
             \x20 opacity: 0;\n\
             }}\n",
            r = MAC_BUTTONS_AT_1X,
        ));
    }
    if t != Titlebar::Tabs {
        return None;
    }
    Some(format!(
        "{GTK_CSS_MARK} Written on every start; edits are lost.\n\
         \x20  See docs/waycompose-design.md §91.8. Windows draws minimise, maximise and close\n\
         \x20  in the rightmost {r} px of a folded caption; a header bar leaves them empty. */\n\
         headerbar {{\n\
         \x20 padding-right: {r}px;\n\
         \x20 border-top-left-radius: 0;\n\
         \x20 border-top-right-radius: 0;\n\
         }}\n",
        r = fit.reserve,
    ))
}

/// The application's `user.js`: the arrangement's preferences, then its own.
pub fn app_user_js(t: Titlebar, prefs: &str) -> String {
    user_js(&format!("{};{prefs}", t.prefs()))
}

// ------------------------------------------- what a launch sends, §94 ------
//
// Until §94 a launch sent `run ID CMD PROFILE PREFS TITLEBAR CHROME GTK`: the
// command line, the profile directory, the rendered `user.js`, the title-bar
// environment value and both style sheets, all base64'd. The command line was
// the hole security-model.md R33 records: whoever holds the guest's key could
// name any program on the VM host. Every one of those words is now the host's,
// in `apps.ini` beside `wc-session`, and the files are rendered there by this
// same code ([`host_files`], run as `wc-launch --render-app-files`). What is
// left for the guest to say is which application, which action or URL, and
// two numbers only the guest can measure.

/// The desktop DPI the host accepts: Windows' 100% is 96 and its largest
/// scale, 500%, is 480. Anything outside is refused on both sides.
pub const DPI_MIN: u32 = 48;
pub const DPI_MAX: u32 = 960;
/// The caption height the host accepts, in pixels; 0 means "not measured",
/// and the host scales §31's 100% number by the DPI instead.
pub const CAPTION_MAX: u32 = 512;

/// The verb of one launch: `run ID APP DPI CAPTION`, `action ID APP ACTION DPI
/// CAPTION` or `open ID APP B64URL DPI CAPTION`. `APP` is the host's id for
/// the application (`[app.*] host-app`). The URL is base64'd because it is the
/// one word with characters `wc-session`'s whitelist refuses (`?`, `&`, `%`).
pub fn launch_verb(id: &str, app: &str, req: crate::Request, dpi: u32, caption: Option<i32>) -> Vec<String> {
    let dpi = dpi.clamp(DPI_MIN, DPI_MAX);
    let caption = caption.filter(|h| *h > 0).map_or(0, |h| (h as u32).min(CAPTION_MAX));
    let mut v: Vec<String> = match req {
        crate::Request::Run => vec!["run".into(), id.into(), app.into()],
        crate::Request::Action(a) => vec!["action".into(), id.into(), app.into(), a.into()],
        crate::Request::Open(u) => vec!["open".into(), id.into(), app.into(), b64(u.as_bytes())],
    };
    v.push(dpi.to_string());
    v.push(caption.to_string());
    v
}

/// What the host writes for one launch of an application, rendered from its
/// `apps.ini` definition and the guest's two numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFiles {
    /// `user.js`: the arrangement's preferences, then the definition's.
    pub user_js: String,
    /// `chrome/userChrome.css`, for [`Titlebar::Tabs`] only.
    pub user_chrome: Option<String>,
    /// The GTK sheet of §91.8, for [`Titlebar::Tabs`] only.
    pub gtk_css: Option<String>,
    /// The value of `MOZ_GTK_TITLEBAR_DECORATION`.
    pub moz_env: &'static str,
    pub fit: TitlebarFit,
}

/// §94. Render [`HostFiles`] on the VM host. `titlebar` and `prefs` are the
/// host's definition; `dpi` and `caption` are the guest's, and are refused
/// outside [`DPI_MIN`]..=[`DPI_MAX`] and 0..=[`CAPTION_MAX`] (0: not measured).
pub fn host_files(titlebar: &str, dpi: u32, caption: u32, prefs: &str) -> Result<HostFiles, String> {
    let t = Titlebar::parse(titlebar).ok_or_else(|| format!("titlebar {titlebar:?} is not one of tabs, none, client"))?;
    if !(DPI_MIN..=DPI_MAX).contains(&dpi) {
        return Err(format!("dpi {dpi} is outside {DPI_MIN}..={DPI_MAX}"));
    }
    if caption > CAPTION_MAX {
        return Err(format!("caption {caption} is outside 0..={CAPTION_MAX}"));
    }
    let fit = TitlebarFit::at(dpi, Some(caption as i32).filter(|h| *h > 0));
    Ok(HostFiles {
        user_js: app_user_js(t, prefs),
        user_chrome: user_chrome_css(t, fit),
        gtk_css: gtk_css(t, fit),
        moz_env: t.moz_env(),
        fit,
    })
}

/// base64, for the words that cross in `$SSH_ORIGINAL_COMMAND` with
/// characters `wc-session` refuses there: a jump-list page's URL, and the
/// desktop entry's free text coming back.
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

    #[test]
    fn the_compositor_gets_the_hostint_path_map() {
        let h = crate::config::HostInt::for_platform(true);
        assert_eq!(
            hostint_argv(&h),
            ["--hostint", "--hostint-share", r"Z:\=/srv/winshare", "--hostint-drop", r"Z:\waycompose-open=/srv/winshare/waycompose-open"]
        );
        let mut b = h.clone();
        b.browser = r"C:\b\browser.exe".into();
        assert_eq!(hostint_argv(&b).last().map(String::as_str), Some(r"C:\b\browser.exe"));
        b.browser_args = vec!["--new-window".into(), "--user-data-dir=C:\\T\\p q".into()];
        assert_eq!(
            hostint_argv(&b)[5..],
            ["--hostint-browser", r"C:\b\browser.exe", "--hostint-browser-arg", "--new-window", "--hostint-browser-arg", r"--user-data-dir=C:\T\p q"]
        );
    }

    fn cfg() -> Config {
        Config::parse(EXAMPLE).unwrap()
    }

    /// §107. The user's compositor: no session of its own, the user's socket,
    /// the mutex; the nested globals go per session in the boot line.
    #[test]
    fn the_shared_compositor_boots_sessions_on_its_socket() {
        let mut c = cfg();
        c.nested_globals = true;
        let a = shared_compositor_argv(&c, r"C:\L\wc-compositor.log", r"C:\L\wc-compositor.ctl");
        let after = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].as_str());
        assert_eq!(after("--user-control"), Some(r"C:\L\wc-compositor.ctl"));
        assert_eq!(after("--singleton"), Some(COMPOSITOR_MUTEX));
        assert!(!a.iter().any(|x| x.starts_with("--boot-") || x == "--control-socket" || x == "--nested-compositor-globals"), "{a:?}");
        assert_eq!(after("--arena-ivshmem"), Some("0"));
        // The line the compositor's `Config::parse_request` reads (its test
        // pins the same shape): tabs between fields, spaces kept in paths.
        let line = boot_request(&c, "wcl-42", r"C:\U\First Last\session.key", r"C:\L\wc-launch-wcl.ctl", true, r"C:\U\First Last\apps", Some("wcl:firefox"));
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f[0], "boot");
        assert!(f.contains(&"session=wcl-42") && f.contains(&r"key=C:\U\First Last\session.key"));
        assert!(f.contains(&"gpu=1") && f.contains(&"nested=1") && f.contains(&r"relaunch=C:\U\First Last\apps"));
        assert!(f.iter().any(|x| x.starts_with("install=")));
        assert!(f.contains(&"taskbar=wcl:firefox"));
        assert!(!boot_request(&c, "i", "k", "c", false, "r", None).contains("taskbar="), "the retry for an older compositor");
        assert!(!line.contains('\n'));
        let g = shared_gpu_argv(&c);
        let after = |k: &str| g.iter().position(|x| x == k).map(|i| g[i + 1].as_str());
        assert_eq!(after("--singleton"), Some(GPU_MUTEX));
        assert_eq!(after("--exit-idle"), Some("120"));
        // §110: with GPU sessions, the GPU pool runs in the compositor.
        c.gpu_sessions = 2;
        let a = shared_compositor_argv(&c, "L", "C");
        let i = a.iter().position(|x| x == "--with-gpu").expect("--with-gpu");
        assert_eq!(&a[i + 1..i + 3], ["guest", "--ivshmem-slot"]);
        c.one_pool = false;
        assert!(!shared_compositor_argv(&c, "L", "C").iter().any(|x| x == "--with-gpu"));
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

    /// §102. `nested-compositor-globals = on` is the one way the flag reaches
    /// the compositor; the default argv does not carry it.
    #[test]
    fn the_nested_compositor_globals_are_passed_only_when_configured() {
        let mut c = cfg();
        let flag = "--nested-compositor-globals";
        assert!(!compositor_argv(&c, "s", "l").iter().any(|a| a == flag));
        c.nested_globals = true;
        assert!(compositor_argv(&c, "s", "l").iter().any(|a| a == flag));
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

    /// §94: the preference is the deployment's, so it is in the host's
    /// `apps.ini` that the deploy step writes, and the guest's file has none.
    #[test]
    fn the_deployment_turns_off_the_middle_click_paste_on_the_host() {
        let deploy = include_str!("../../../scripts/wc-launch.sh");
        let apps = deploy.split("<<APPS").nth(1).and_then(|t| t.split("\nAPPS\n").next()).expect("an apps.ini heredoc");
        assert!(apps.contains("prefs    = middlemouse.paste=false"), "{apps}");
        assert!(user_js("middlemouse.paste=false").contains("user_pref(\"middlemouse.paste\", false);"));
        assert!(!EXAMPLE.contains("middlemouse"));
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

    /// §94. A launch names the application and nothing the host runs: no
    /// path, no profile, no file content. Two numbers and, for a page, its
    /// URL in base64.
    #[test]
    fn a_launch_sends_an_app_id_and_two_numbers_and_never_a_command_line() {
        assert_eq!(launch_verb("wcl-7", "firefox", crate::Request::Run, 144, Some(45)), ["run", "wcl-7", "firefox", "144", "45"]);
        assert_eq!(
            launch_verb("wcl-7", "firefox", crate::Request::Action("new-private-window"), 96, None),
            ["action", "wcl-7", "firefox", "new-private-window", "96", "0"]
        );
        let o = launch_verb("wcl-7", "firefox", crate::Request::Open("https://e.org/?a=1&b"), 96, Some(-3));
        assert_eq!(o[..3], ["open", "wcl-7", "firefox"]);
        assert_eq!(unb64(&o[3]).unwrap(), b"https://e.org/?a=1&b");
        assert_eq!(o[4..], ["96", "0"]);
        // Out of range is clamped here and refused on the host.
        assert_eq!(launch_verb("i", "a", crate::Request::Run, 5000, Some(9000))[3..], ["960", "512"]);
        // Every word is in the alphabet `wc-session` admits before it looks
        // at a verb at all.
        for v in [launch_verb("i", "firefox", crate::Request::Run, 96, None), o] {
            assert!(v.iter().all(|w| w.chars().all(|c| c.is_ascii_alphanumeric() || "+/=._-".contains(c))), "{v:?}");
        }
    }

    /// §94. The host renders what the guest used to send, from its own
    /// definition and the guest's two numbers, and refuses numbers outside
    /// their range.
    #[test]
    fn the_host_renders_the_profile_files_and_refuses_numbers_out_of_range() {
        let f = host_files("tabs", 144, 45, "middlemouse.paste=false").unwrap();
        assert_eq!(f.moz_env, "client");
        assert_eq!(f.fit, TitlebarFit { height: 45, reserve: 219 });
        assert!(f.user_js.contains("user_pref(\"middlemouse.paste\", false);"));
        assert!(f.user_chrome.as_deref().unwrap().contains("min-width: 219px"));
        assert!(f.gtk_css.as_deref().unwrap().contains("padding-right: 219px"));
        assert_eq!(host_files("tabs", 96, 0, "").unwrap().fit, TitlebarFit { height: 31, reserve: 146 });
        let n = host_files("none", 96, 0, "").unwrap();
        assert_eq!((n.moz_env, n.user_chrome, n.gtk_css), ("none", None, None));
        for (t, d, c) in [("tab", 96, 0), ("tabs", 47, 0), ("tabs", 961, 0), ("tabs", 96, 513)] {
            assert!(host_files(t, d, c, "").is_err(), "{t} {d} {c}");
        }
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
        assert_eq!(Titlebar::parse("mac-tabs"), Some(Titlebar::MacTabs));
        assert_eq!(Titlebar::MacTabs.moz_env(), "client");
        let mac = user_chrome_css(Titlebar::MacTabs, TitlebarFit::at(96, None)).unwrap();
        assert!(mac.contains("padding-inline-start: 78px"), "{mac}");
        assert!(mac.contains("display: none"), "{mac}");
        assert!(gtk_css(Titlebar::MacTabs, TitlebarFit::at(96, None)).unwrap().contains("padding-left: 78px"));
        assert!(Titlebar::MacTabs.prefs().contains("GtkCSDCloseButton=0"));
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

    /// §91.8: the GTK sheet pads every header bar by the same reservation as
    /// the tab strip's, at each of §82's three scales.
    #[test]
    fn the_gtk_sheet_pads_the_header_bar_by_the_caption_buttons() {
        for (dpi, px) in [(96, 146), (120, 183), (144, 219)] {
            let css = gtk_css(Titlebar::Tabs, TitlebarFit::at(dpi, None)).unwrap();
            assert!(css.starts_with(GTK_CSS_MARK), "{css}");
            assert!(css.contains(&format!("headerbar {{\n  padding-right: {px}px;")), "{dpi}: {css}");
        }
        assert_eq!(gtk_css(Titlebar::None, TitlebarFit::at(96, None)), None);
        assert_eq!(gtk_css(Titlebar::Client, TitlebarFit::at(96, None)), None);
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

/// §86, §87, §97. The GPU process: `sv-guest.exe gpu guest` on the GPU arena's
/// ivshmem device, up to `gpu-sessions` sessions at once — leasing from one
/// pool over the whole device (`gpu-arena = pool`, the default) or in §87's
/// fixed slots of the BAR — and the doorbell port if the ini names one.
/// `--idle` is how long a session waits for its application to claim its
/// rings once the host has asked for it, which is seconds, not the session's
/// life: the process lives as long as the launcher session, which kills it.
///
/// A pool has no port: its cold path is the mailboxes in the pool's header
/// granule (`sv_transport::poolcold`), which the VM host's
/// `sv-hostd ivshmem-serve --cold pool` reads through the same device. §87's
/// slots still speak TCP (`--port`), and nothing forwards it any more.
pub fn gpu_argv(cfg: &Config) -> Vec<String> {
    let mut a: Vec<String> = vec!["gpu".into(), "guest".into()];
    if cfg.gpu_pool {
        a.extend(["--ivshmem-slot".into(), format!("{:#04x}", cfg.gpu_ivshmem_slot), "--pool".into()]);
    } else {
        a.extend([
            "--port".into(),
            cfg.gpu_port().to_string(),
            "--ivshmem-slot".into(),
            format!("{:#04x}", cfg.gpu_ivshmem_slot),
            "--arena".into(),
            (cfg.gpu_arena_mib << 20).to_string(),
        ]);
    }
    a.extend([
        "--sessions".into(),
        cfg.gpu_sessions.to_string(),
        "--idle".into(),
        "60".into(),
    ]);
    if !cfg.gpu_bell.is_empty() {
        a.extend(["--bell".into(), cfg.gpu_bell.clone()]);
    }
    a
}

#[cfg(test)]
mod gpu_tests {
    use super::*;

    #[test]
    fn the_gpu_process_has_no_port_no_forward_and_its_device() {
        let mut c = Config { host: "u@h".into(), ..Config::default() };
        c.gpu_sessions = 3;
        let a = tunnel_argv(&c, "kh", "wcl-1");
        assert!(!a.iter().any(|x| x.contains("cold")), "no forward for the GPU cold path: {a:?}");
        let g = gpu_argv(&c);
        assert_eq!(&g[..5], ["gpu", "guest", "--ivshmem-slot", "0x1d", "--pool"]);
        assert!(!g.iter().any(|x| x == "--port"), "the pool's cold path is its mailboxes: {g:?}");
        let after = |k: &str| g.iter().position(|x| x == k).map(|i| g[i + 1].as_str());
        assert_eq!(after("--sessions"), Some("3"));
        assert!(g.iter().any(|x| x == "--pool") && !g.iter().any(|x| x == "--arena"), "the pool by default (§97)");
        c.gpu_pool = false;
        let s = gpu_argv(&c);
        assert!(!s.iter().any(|x| x == "--pool"));
        assert_eq!(s.iter().position(|x| x == "--arena").map(|i| s[i + 1].as_str()), Some("67108864"));
        c.gpu_pool = true;
        assert_eq!(after("--bell"), Some("org.sirati.svbell.0"));
        assert!(!g.iter().any(|x| x.contains("small-pages")), "the launcher never asks for small pages");
        c.gpu_bell.clear();
        assert!(!gpu_argv(&c).iter().any(|x| x == "--bell"));
        assert_eq!(a.last().unwrap(), "u@h");
    }
}

#[cfg(test)]
mod boot_tests {
    use super::*;

    #[test]
    fn the_compositor_is_asked_to_boot_the_session_with_no_ssh() {
        let mut c = Config { session: "wcns".into(), ..Config::default() };
        let a = boot_argv(&c, "wcns-42", r"C:\w\session.key", r"C:\s\wc-launch-wcns.ctl");
        let after = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].as_str());
        assert_eq!(after("--boot-session"), Some("wcns-42"));
        assert_eq!(after("--boot-install"), Some("wcns"), "the session name by default");
        assert_eq!(after("--boot-key"), Some(r"C:\w\session.key"));
        assert_eq!(after("--control-socket"), Some(r"C:\s\wc-launch-wcns.ctl"));
        assert_eq!(after("--session-audio"), Some("100"));
        c.installation = "wclinux".into();
        c.audio = false;
        let a = boot_argv(&c, "wcns-42", "k", "s");
        assert!(a.iter().any(|x| x == "wclinux"));
        assert!(!a.iter().any(|x| x == "--session-audio"));
        // Nothing of ssh anywhere in what starts a session.
        assert!(!compositor_argv(&c, "wl-x", "log").iter().chain(&a).any(|x| x.contains("ssh")));
    }

    #[test]
    fn a_request_on_the_ring_carries_no_session_id() {
        let v = launch_verb("wcns-42", "firefox", crate::Request::Run, 96, Some(30));
        assert_eq!(v[1], "wcns-42");
        let w = without_id(&v);
        assert_eq!(w[0], "run");
        assert!(!w.iter().any(|x| x == "wcns-42"));
        assert_eq!(&w[1..], &v[2..]);
    }
}

#[cfg(test)]
mod taskbar_scope_tests {
    use super::*;
    use crate::config::TaskbarId;

    fn cfg() -> Config {
        Config::parse(
            "[session]\ncompositor=C\ninstallation=wcivs\n[app.firefox]\ntaskbar-id = legacy\n\
             [app.tb]\ndesktop = thunderbird\n[app.ff2]\ndesktop = firefox\ntaskbar-id = legacy\n",
        )
        .unwrap()
    }

    /// §92.12: the boot carries the installation and its legacy app ids, in
    /// the form the compositor's `Scope::parse` reads, and only those.
    #[test]
    fn the_scope_names_the_installation_and_its_legacy_window_ids() {
        let c = cfg();
        let s = taskbar_scope(&c, |_, a| a.desktop.clone());
        assert_eq!(s.install, "wcivs");
        assert_eq!(s.legacy, ["firefox"], "two sections of one window id are one entry");
        assert_eq!(crate::taskbar::Scope::parse(&s.render()).unwrap(), s);
        // The window app id is the launcher's, StartupWMClass included.
        let s = taskbar_scope(&c, |sec, _| if sec == "firefox" { "Navigator".into() } else { "firefox".into() });
        assert_eq!(s.legacy, ["firefox", "Navigator"]);
        // An id the scope could not carry is left out rather than refused.
        assert!(taskbar_scope(&c, |_, _| "a b".into()).legacy.is_empty());
        let mut none = c.clone();
        for a in none.apps.values_mut() {
            a.taskbar_id = TaskbarId::Install;
        }
        assert_eq!(taskbar_scope(&none, |_, a| a.desktop.clone()).render(), "wcivs");
        assert_eq!(taskbar_argv("wcivs:firefox"), ["--taskbar-scope", "wcivs:firefox"]);
    }
}
