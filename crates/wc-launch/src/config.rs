//! `wc-launch.ini` — one file beside the executable, one section per
//! application.
//!
//! A second application is a second `[app.<id>]` block and a second shortcut,
//! which is the shape docs/goal-firefox.md's phase 2 asks for: the product is
//! "Linux applications on Windows", and Firefox is the first one rather than
//! the only one.
//!
//! Deliberately not TOML. The file is three lines of session settings and two
//! lines per application, it is written by the install step and read by a
//! binary that must start in single-digit milliseconds, and a parser small
//! enough to read in one sitting is worth more here than a schema.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// What the shortcut is called: "Firefox (Linux)".
    pub title: String,
    /// The icon's name in the *host's* theme. §16: the pixels live on the
    /// machine the application runs on, and there is no second copy here.
    pub icon: String,
    /// §94. The application's id **on the VM host**: a section of the host's
    /// `apps.ini` beside `wc-session`, which holds everything that decides what
    /// runs there — the executable, its fixed arguments, the profile, the
    /// preferences, the title-bar arrangement, the environment and whether the
    /// forwarded GPU applies. The section's id by default.
    ///
    /// Until §94 this block carried `exec`, `prefs` and `titlebar`, and the
    /// launcher sent the command line to the host. A key that can send a
    /// command line can run any program as the host's user
    /// (security-model.md R33), so the guest now names the application and
    /// nothing else; `exec`, `prefs` and `titlebar` are refused by name.
    pub host_app: String,
    /// §92. The application's desktop entry on the VM host, by id: `firefox`
    /// for `firefox.desktop`. On this side it is identity only: with
    /// [`App::app_id`] it is what the pin's taskbar identity is derived from
    /// before the host has answered. The section's id by default.
    pub desktop: String,
    /// §92. The Wayland app id the application's windows carry, which is what
    /// their taskbar identity is derived from (`taskbar::identity`). Empty by
    /// default, meaning the desktop entry's `StartupWMClass`, else `desktop`:
    /// the pin and the windows must agree, or a running window gets a button
    /// of its own beside its pin.
    pub app_id: String,
    /// §92.12. `taskbar-id = legacy | install`: whether this application's
    /// windows, shortcuts, pins and jump list carry the application-wide
    /// identity (`Waycompose.app.<app id>`, kept for the one installation
    /// whose existing pin has it) or this installation's own
    /// (`Waycompose.install.<installation>.<app id>`). Deploy decides once
    /// (`--taskbar-ids`) and writes it here; absent is `install`.
    pub taskbar_id: TaskbarId,
}

/// §92.12. Which form of the taskbar identity an application has here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TaskbarId {
    /// `Waycompose.install.<installation>.<app id>`: this installation's.
    #[default]
    Install,
    /// `Waycompose.app.<app id>`: the application-wide identity of §92, for
    /// the one installation deploy found the user's pin of.
    Legacy,
}

impl TaskbarId {
    pub fn parse(v: &str) -> Option<TaskbarId> {
        match v {
            "install" => Some(TaskbarId::Install),
            "legacy" => Some(TaskbarId::Legacy),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            TaskbarId::Install => "install",
            TaskbarId::Legacy => "legacy",
        }
    }
}

/// §106. `[hostint]`: how a file on this machine reaches Linux, and which
/// browser opens Linux's links here. The same keys on both hosts, checked
/// by the host's rules ([`HostInt::check_for`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInt {
    /// The folder both machines see: (this side, the VM host's side).
    /// Windows: `Z:\` and `/srv/winshare` by default, the VM's virtio-fs
    /// share. macOS: none unless configured.
    pub share: (String, String),
    /// Where a file from outside the share is copied before Linux opens
    /// it, inside the share: (this side, the VM host's side).
    pub drop: (String, String),
    /// Windows: an executable to open links from Linux with; empty is the
    /// user's default browser. macOS: an application name as `open -a`
    /// takes it, "Firefox" by default.
    pub browser: String,
    /// Windows: fixed arguments for `browser`, before the URL, one argv
    /// element each (`browser-arg`, repeated; at most [`MAX_BROWSER_ARGS`]).
    /// macOS: none (`open -a` takes no arguments for the application).
    pub browser_args: Vec<String>,
}

/// At most this many `browser-arg` lines.
pub const MAX_BROWSER_ARGS: usize = 16;
/// The longest `browser-arg`, in bytes.
pub const MAX_BROWSER_ARG_LEN: usize = 1024;

impl Default for HostInt {
    fn default() -> Self {
        HostInt::for_platform(!cfg!(target_os = "macos"))
    }
}

impl HostInt {
    /// The defaults of a Windows host (`windows`) or a macOS one.
    pub fn for_platform(windows: bool) -> Self {
        if windows {
            HostInt {
                share: (r"Z:\".into(), "/srv/winshare".into()),
                drop: (r"Z:\waycompose-open".into(), "/srv/winshare/waycompose-open".into()),
                browser: String::new(),
                browser_args: Vec::new(),
            }
        } else {
            HostInt { share: Default::default(), drop: Default::default(), browser: "Firefox".into(), browser_args: Vec::new() }
        }
    }

    /// As `wc_hostint`'s path map. No share configured is no share.
    pub fn path_map(&self) -> wc_hostint::wire::PathMap {
        let set = |p: &(String, String)| !p.0.is_empty() && !p.1.is_empty();
        wc_hostint::wire::PathMap {
            shares: if set(&self.share) { vec![self.share.clone()] } else { vec![] },
            drop: set(&self.drop).then(|| self.drop.clone()),
        }
    }

    fn check(&self) -> Result<(), String> {
        self.check_for(!cfg!(target_os = "macos"))
    }

    /// Check by a Windows host's rules (`windows`) or a macOS host's.
    pub fn check_for(&self, windows: bool) -> Result<(), String> {
        let host_path = |s: &str| {
            if windows {
                let b = s.as_bytes();
                b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' && !s.split('\\').any(|c| c == "..")
            } else {
                s.starts_with('/') && !s.split('/').any(|c| c == "..")
            }
        };
        let linux = |s: &str| s.starts_with('/') && !s.split('/').any(|c| c == "..");
        let what = if windows { "a drive path (X:\\...)" } else { "an absolute path" };
        let share_set = !self.share.0.is_empty() || !self.share.1.is_empty();
        if share_set && (!host_path(&self.share.0) || !linux(&self.share.1)) {
            return Err(format!("[hostint] share-host must be {what} and share-linux an absolute path"));
        }
        if !self.drop.0.is_empty() || !self.drop.1.is_empty() {
            let hsep = if windows { '\\' } else { '/' };
            let inside = |p: &str, root: &str, sep: char| {
                let r = root.trim_end_matches(sep);
                if windows && sep == '\\' {
                    p.to_ascii_lowercase().starts_with(&format!("{}{sep}", r.to_ascii_lowercase()))
                } else {
                    p.starts_with(&format!("{r}{sep}"))
                }
            };
            if !share_set
                || !host_path(&self.drop.0)
                || !linux(&self.drop.1)
                || !inside(&self.drop.0, &self.share.0, hsep)
                || !inside(&self.drop.1, &self.share.1, '/')
            {
                return Err("[hostint] drop-host and drop-linux must be directories inside the share".into());
            }
        }
        if windows {
            if !self.browser.is_empty() && !self.browser.to_ascii_lowercase().ends_with(".exe") {
                return Err("[hostint] browser must be an .exe".into());
            }
            if !self.browser_args.is_empty() && self.browser.is_empty() {
                return Err("[hostint] browser-arg needs a browser".into());
            }
            if self.browser_args.len() > MAX_BROWSER_ARGS {
                return Err(format!("[hostint] at most {MAX_BROWSER_ARGS} browser-arg lines"));
            }
            if self.browser_args.iter().any(|a| a.is_empty() || a.len() > MAX_BROWSER_ARG_LEN || a.chars().any(char::is_control)) {
                return Err(format!(
                    "[hostint] a browser-arg must be 1..={MAX_BROWSER_ARG_LEN} bytes without control characters"
                ));
            }
        } else if !self.browser_args.is_empty() {
            return Err("[hostint] browser-arg is for a Windows host only: `open -a` takes no arguments".into());
        } else if self.browser.is_empty() || self.browser.contains('/') || self.browser.chars().any(char::is_control) {
            return Err("[hostint] browser must be an application's name, as `open -a` takes it".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// ssh target of the VM host, over the host-only virtio-net link.
    pub host: String,
    /// The private key for it. The guest holds it; the host restricts it to
    /// `wc-session`'s verbs (see `scripts/wc-session`).
    pub key: String,
    /// `ssh.exe`. The Windows one by default: the MSYS build rewrites paths
    /// that look like paths, and `-i C:\...` is one.
    pub ssh: String,
    pub compositor: String,
    pub relay: String,
    /// A name, not a path: this deployment's session, and the last component of
    /// the mutex, the state file and the run id on the host.
    pub session: String,
    /// The **startup** region, in MiB of the guest's own pinned RAM.
    ///
    /// **Chosen by [`crate::floor::Floor`], which carries the measurements.**
    /// It is not a literal here and it was one until the doc comment on this
    /// field went on explaining that "growth is armed and inert" — listing the
    /// two absences that made it so — for several commits after both had been
    /// closed. The reasoning and the number now live in one place, and
    /// `Floor::CURRENT` is the single line that moves when a guest run earns a
    /// smaller floor.
    ///
    /// The short version, because a reader here deserves one: these pages are
    /// `VirtualLock`ed, so they are non-pageable rather than merely committed,
    /// and the startup region is never reclaimed because it holds the outbound
    /// window. Its size is a floor for the whole session that release can never
    /// recover, which is why over-reserving is the more expensive mistake and
    /// why 768 MiB once reached a user.
    ///
    /// IVSHMEM ARENA (§83): **read and validated, and decides nothing** on the
    /// launcher's own path any more. The arena is BAR2 of an ivshmem device and
    /// has the size the VM host gave it. The key stays so an existing
    /// `wc-launch.ini` still parses, and because `outbound` is still checked
    /// against it.
    pub arena_mib: u64,
    /// How much of it the compositor keeps for the objects it makes for clients.
    pub outbound: u64,
    /// The one `wl_output`, `WxH`.
    pub output: String,
    /// The relay's loopback port inside the guest.
    pub port: u16,
    /// How long a session may last before the compositor's own deadline ends
    /// it. Not an idle timeout — see docs/waycompose-design.md §28.9.
    pub session_secs: u64,
    /// §85. How long the session outlives its last window, in seconds. A click
    /// within it is a warm start; after it the compositor exits and the
    /// session, its relay, tunnel and proxy, and its hold on the arena, go with
    /// it (§28.9 item 9). `linger-secs = 0` ends it as soon as the last window
    /// closes.
    pub linger_secs: u64,
    /// §86. Play the session's sound on this machine: `wc-session audio` over
    /// this launcher's own key, into waveOut. `audio = off` leaves it to the
    /// host, which then gives Firefox a null audio context.
    pub audio: bool,
    /// §86. waveOut volume for that stream, 0..=100. The device's own volume
    /// and the user's mixer still apply on top of it.
    pub audio_volume: u8,
    /// §86. The guest-side loopback port the tunnel forwards to the session's
    /// `audio.sock` on the VM host. 0 means `port + 1`.
    pub audio_port: u16,
    /// §86, §87. The forwarded GPU: 0 (the default) is none, and software
    /// rendering on the host. N > 0 starts **one** `sv-guest.exe gpu guest`
    /// process on the GPU arena's ivshmem device, which serves up to N
    /// sessions at once — as many as fit in the device, if fewer — each in its
    /// own slot of the BAR. Until §87 it was N processes with N large-page
    /// arenas that only a root broker on the host could locate.
    pub gpu_sessions: u32,
    /// §86. The GPU process's loopback port. 0 means `port + 10`.
    pub gpu_port: u16,
    /// §86. Each GPU session's arena, MiB of its slot of the BAR. Only for
    /// `gpu-arena = slots`.
    pub gpu_arena_mib: u64,
    /// §97. `true` (`gpu-arena = pool`, the default): the GPU process serves
    /// every session from one pool over the whole device, leased as each
    /// needs and given back as each frees, and `gpu-sessions` bounds how many
    /// run at once. `false` (`gpu-arena = slots`): §87's fixed slots of
    /// `gpu-arena-mib`.
    pub gpu_pool: bool,
    /// §87. The root-bus PCI slot of the GPU arena's ivshmem device, as the
    /// VM's configuration pins it (`waycomposeArena.gpu.pciAddress`).
    pub gpu_ivshmem_slot: u8,
    /// §87. The doorbell's virtio-serial port name; empty is no doorbell.
    pub gpu_bell: String,
    /// §103.15, macOS: the session's GPU server (`sv-guest`), which the main
    /// process starts and the host's session service serves; empty is none.
    pub gpu_server: String,
    /// §103.15, macOS: `VK_DRIVER_FILES` for that server (lavapipe in a VM
    /// without Metal); empty leaves the loader's own search.
    pub gpu_icd: String,
    /// §89. The Wayland rings' virtio-serial port: with it the compositor
    /// keeps the ivshmem region's tail for the session's Wayland connections
    /// and they cross as shared memory; empty, or a VM without the port, and
    /// they cross the ssh tunnel as before, which the log says loudly.
    pub wayland_bell: String,
    /// §103.17, macOS role (b): the shared arena file (a RAM-disk file the
    /// Linux guest maps through virtio-fs DAX); empty is role (a), guest RAM.
    /// `WC_MAC_ARENA_STANDIN` overrides it.
    pub arena_file: String,
    /// §102. `nested-compositor-globals = on` asks the compositor for the three
    /// globals a nested compositor needs from its host —
    /// `wp_single_pixel_buffer_manager_v1`, `wp_viewporter` and `wp_presentation`
    /// (KWin's Wayland backend refuses to start without them). Off by default,
    /// so a session that runs Firefox sees exactly the globals it was measured
    /// with: GTK uses `wp_viewporter` whenever it is offered, and that path has
    /// not been through §95's desktop checks.
    pub nested_globals: bool,
    /// §106. Host integration: the shared folder and its drop directory,
    /// and the browser for links from Linux. `[hostint]`.
    pub hostint: HostInt,
    /// §105. The installation's name, as the VM host's session service knows
    /// it (`~/.config/waycompose/installs/<name>`). Empty: the session name.
    pub installation: String,
    /// §105. The installation's key, 64 hex digits, written by deploy and
    /// readable by this user only; empty: `session.key` beside the launcher.
    pub session_key: String,
    /// §105. `debug-ssh-transport = on`: the pre-§105 path (ssh exec of the
    /// forced command, the relay and the tunnel), for debugging only, off by
    /// default and said in capitals on every step. `host`, `key`, `relay` and
    /// `port` are read only for it.
    pub debug_ssh_transport: bool,
    /// §107. `shared-compositor = on` (the default): every installation of
    /// this user is served by one compositor and one GPU process, so any
    /// number of them run at once. `off` is §105's compositor per
    /// installation, which the ivshmem driver lets run one at a time.
    pub shared_compositor: bool,
    /// §110. `one-pool = on` (the default): the shared compositor also runs
    /// the GPU pool, in its own process, so Wayland buffers can lease from the
    /// GPU device when the arena's BAR is full. `off`: the GPU pool is §107's
    /// separate per-user process.
    pub one_pool: bool,
    pub apps: BTreeMap<String, App>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: String::new(),
            key: String::new(),
            ssh: r"C:\Windows\System32\OpenSSH\ssh.exe".into(),
            compositor: String::new(),
            relay: String::new(),
            session: "wcl".into(),
            arena_mib: crate::floor::Floor::CURRENT.mib(),
            outbound: 1024 * 1024,
            output: "1600x1000".into(),
            port: 57431,
            session_secs: 43200,
            linger_secs: 60,
            audio: true,
            audio_volume: 100,
            audio_port: 0,
            gpu_sessions: 0,
            gpu_port: 0,
            gpu_arena_mib: 64,
            gpu_pool: true,
            gpu_ivshmem_slot: 0x1d,
            gpu_bell: "org.sirati.svbell.0".into(),
            gpu_server: String::new(),
            gpu_icd: String::new(),
            wayland_bell: "org.sirati.wcbell.0".into(),
            arena_file: String::new(),
            nested_globals: false,
            hostint: HostInt::default(),
            installation: String::new(),
            session_key: String::new(),
            debug_ssh_transport: false,
            shared_compositor: true,
            one_pool: true,
            apps: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let mut cfg = Config::default();
        let mut section = String::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                let Some(name) = rest.strip_suffix(']') else {
                    return Err(format!("line {}: unterminated section header", n + 1));
                };
                section = name.trim().to_string();
                if let Some(id) = section.strip_prefix("app.") {
                    if id.is_empty() {
                        return Err(format!("line {}: [app.] needs an id", n + 1));
                    }
                    cfg.apps.entry(id.to_string()).or_insert(App {
                        title: id.to_string(),
                        icon: id.to_string(),
                        host_app: id.to_string(),
                        desktop: id.to_string(),
                        app_id: String::new(),
                        taskbar_id: TaskbarId::Install,
                    });
                }
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                return Err(format!("line {}: expected key = value", n + 1));
            };
            let (k, v) = (k.trim(), v.trim());
            let num = |what: &str| -> Result<u64, String> {
                v.parse::<u64>().map_err(|e| format!("line {}: {what}: {e}", n + 1))
            };
            if let Some(id) = section.strip_prefix("app.") {
                let app = cfg.apps.get_mut(id).expect("inserted with the header");
                match k {
                    "title" => app.title = v.into(),
                    "icon" => app.icon = v.into(),
                    "host-app" => app.host_app = v.into(),
                    // §94. What runs on the VM host is the host's to say.
                    "exec" | "prefs" | "titlebar" => {
                        return Err(format!(
                            "line {}: [app.{id}] {k} is not the guest's to set since §94: the command \
                             line, profile, preferences and title bar are defined on the VM host, in \
                             apps.ini beside wc-session; name the application with host-app",
                            n + 1
                        ));
                    }
                    "desktop" => app.desktop = v.into(),
                    "app-id" => app.app_id = v.into(),
                    "taskbar-id" => {
                        app.taskbar_id = TaskbarId::parse(v)
                            .ok_or_else(|| format!("line {}: [app.{id}] taskbar-id is legacy or install, not {v}", n + 1))?
                    }
                    other => return Err(format!("line {}: [app.{id}] has no {other}", n + 1)),
                }
                continue;
            }
            match (section.as_str(), k) {
                ("session", "host") => cfg.host = v.into(),
                ("session", "key") => cfg.key = v.into(),
                ("session", "ssh") => cfg.ssh = v.into(),
                ("session", "compositor") => cfg.compositor = v.into(),
                ("session", "relay") => cfg.relay = v.into(),
                ("session", "name") => cfg.session = v.into(),
                ("session", "arena-mib") => cfg.arena_mib = num("arena-mib")?,
                ("session", "outbound") => cfg.outbound = num("outbound")?,
                ("session", "output") => cfg.output = v.into(),
                ("session", "port") => {
                    cfg.port = v.parse().map_err(|e| format!("line {}: port: {e}", n + 1))?
                }
                ("session", "session-secs") => cfg.session_secs = num("session-secs")?,
                ("session", "linger-secs") => cfg.linger_secs = num("linger-secs")?,
                ("session", "audio") => {
                    cfg.audio = match v {
                        "on" => true,
                        "off" => false,
                        _ => return Err(format!("line {}: audio is on or off, not {v}", n + 1)),
                    }
                }
                ("session", "audio-port") => {
                    cfg.audio_port = v.parse().map_err(|e| format!("line {}: audio-port: {e}", n + 1))?
                }
                ("session", "gpu-sessions") => {
                    cfg.gpu_sessions = u32::try_from(num("gpu-sessions")?)
                        .ok()
                        .filter(|v| *v <= 8)
                        .ok_or_else(|| format!("line {}: gpu-sessions is 0..=8", n + 1))?
                }
                ("session", "gpu-server") => cfg.gpu_server = v.into(),
                ("session", "gpu-icd") => cfg.gpu_icd = v.into(),
                ("session", "gpu-port") => {
                    cfg.gpu_port = v.parse().map_err(|e| format!("line {}: gpu-port: {e}", n + 1))?
                }
                ("session", "gpu-arena-mib") => cfg.gpu_arena_mib = num("gpu-arena-mib")?,
                ("session", "gpu-arena") => {
                    cfg.gpu_pool = match v {
                        "pool" => true,
                        "slots" => false,
                        _ => return Err(format!("line {}: gpu-arena is pool or slots, not {v}", n + 1)),
                    }
                }
                ("session", "gpu-ivshmem-slot") => {
                    let s = match v.strip_prefix("0x") {
                        Some(h) => u8::from_str_radix(h, 16).ok(),
                        None => v.parse::<u8>().ok(),
                    };
                    cfg.gpu_ivshmem_slot = s.filter(|s| *s < 32).ok_or_else(|| {
                        format!("line {}: gpu-ivshmem-slot is a PCI slot, 0..=0x1f", n + 1)
                    })?
                }
                ("session", "gpu-bell") => cfg.gpu_bell = v.into(),
                ("session", "wayland-bell") => cfg.wayland_bell = v.into(),
                ("session", "arena-file") => cfg.arena_file = v.into(),
                ("session", "installation") => cfg.installation = v.into(),
                ("session", "session-key") => cfg.session_key = v.into(),
                ("session", "debug-ssh-transport") => {
                    cfg.debug_ssh_transport = match v {
                        "on" => true,
                        "off" => false,
                        _ => return Err(format!("line {}: debug-ssh-transport is on or off, not {v}", n + 1)),
                    }
                }
                ("session", "one-pool") => {
                    cfg.one_pool = match v {
                        "on" => true,
                        "off" => false,
                        _ => return Err(format!("line {}: one-pool is on or off, not {v}", n + 1)),
                    }
                }
                ("session", "shared-compositor") => {
                    cfg.shared_compositor = match v {
                        "on" => true,
                        "off" => false,
                        _ => return Err(format!("line {}: shared-compositor is on or off, not {v}", n + 1)),
                    }
                }
                ("session", "nested-compositor-globals") => {
                    cfg.nested_globals = match v {
                        "on" => true,
                        "off" => false,
                        _ => {
                            return Err(format!(
                                "line {}: nested-compositor-globals is on or off, not {v}",
                                n + 1
                            ));
                        }
                    }
                }
                ("session", "audio-volume") => {
                    cfg.audio_volume = u8::try_from(num("audio-volume")?)
                        .ok()
                        .filter(|v| *v <= 100)
                        .ok_or_else(|| format!("line {}: audio-volume is 0..=100", n + 1))?
                }
                ("hostint", "share-host") => cfg.hostint.share.0 = v.into(),
                ("hostint", "share-linux") => cfg.hostint.share.1 = v.into(),
                ("hostint", "drop-host") => cfg.hostint.drop.0 = v.into(),
                ("hostint", "drop-linux") => cfg.hostint.drop.1 = v.into(),
                ("hostint", "browser") => cfg.hostint.browser = v.into(),
                // Repeated: one argv element per line, in order.
                ("hostint", "browser-arg") => cfg.hostint.browser_args.push(v.into()),
                ("hostint", other) => return Err(format!("line {}: [hostint] has no {other}", n + 1)),
                ("session", other) => {
                    return Err(format!("line {}: [session] has no {other}", n + 1));
                }
                (s, _) => return Err(format!("line {}: no section [{s}] here", n + 1)),
            }
        }
        cfg.check()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Config::parse(&text)
    }

    /// Refuse a configuration that cannot produce a run rather than one that
    /// produces a wrong one. Every field here is named in the message, because
    /// the reader of that message is looking at a shortcut that did nothing.
    fn check(&self) -> Result<(), String> {
        for (what, v) in [
            ("host", &self.host),
            ("key", &self.key),
            ("compositor", &self.compositor),
            ("relay", &self.relay),
        ] {
            // §103.15, §105: a session reaches its host through shared memory
            // only; the ssh host, key and relay are read for the opt-in debug
            // transport alone.
            if (cfg!(target_os = "macos") || !self.debug_ssh_transport) && matches!(what, "host" | "key" | "relay") {
                continue;
            }
            if v.is_empty() {
                return Err(format!("[session] {what} is required"));
            }
        }
        self.hostint.check()?;
        if !self.debug_ssh_transport && !cfg!(target_os = "macos") && self.wayland_bell.is_empty() {
            return Err("[session] wayland-bell is empty: since §105 every byte crosses on the Wayland rings, which need it (debug-ssh-transport = on is the only way without)".into());
        }
        if !self.installation.is_empty() && !installation_ok(&self.installation) {
            return Err(format!("[session] installation = {} is not a name (letters, digits, - _, at most 32)", self.installation));
        }
        if self.arena_mib == 0 {
            return Err("[session] arena-mib must not be zero".into());
        }
        if self.outbound >= self.arena_mib * 1024 * 1024 {
            return Err(format!(
                "[session] outbound {} leaves nothing of a {} MiB arena for the proxy's pools",
                self.outbound, self.arena_mib
            ));
        }
        for (id, app) in &self.apps {
            // §92. The desktop id becomes a file name on the VM host and a word in
            // $SSH_ORIGINAL_COMMAND; the host refuses anything else, and saying so
            // here names the line.
            let id_ok = |s: &str| {
                !s.starts_with('.')
                    && !s.contains("..")
                    && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
            };
            if app.desktop.is_empty() || !id_ok(&app.desktop) || !id_ok(&app.app_id) {
                return Err(format!(
                    "[app.{id}] desktop = {} and app-id = {} must be desktop ids (letters, digits, . _ - +)",
                    app.desktop, app.app_id
                ));
            }
            // §94. The host app id is a word of the request and a section name
            // in the host's apps.ini, and `wc-session` checks it with the same
            // rule ([`host_app_ok`]).
            if !host_app_ok(&app.host_app) {
                return Err(format!(
                    "[app.{id}] host-app = {} is not an application id (a letter or digit, then \
                     letters, digits, . _ - +, at most 64)",
                    app.host_app
                ));
            }
        }
        Ok(())
    }

    /// §86, §87. The GPU process's loopback port.
    pub fn gpu_port(&self) -> u16 {
        if self.gpu_port != 0 { self.gpu_port } else { self.port.wrapping_add(10) }
    }

    /// §105. The installation's name for the host's session service.
    pub fn installation(&self) -> &str {
        if self.installation.is_empty() { &self.session } else { &self.installation }
    }

    /// §86. Where the tunnel puts the session's sound on this machine.
    pub fn audio_port(&self) -> u16 {
        if self.audio_port != 0 { self.audio_port } else { self.port.wrapping_add(1) }
    }

    pub fn app(&self, id: &str) -> Result<&App, String> {
        self.apps.get(id).ok_or_else(|| {
            let known: Vec<&str> = self.apps.keys().map(String::as_str).collect();
            format!("no [app.{id}] in the configuration; it has {}", known.join(", "))
        })
    }
}

/// §94. An application id as `wc-session` accepts it: a letter or digit first
/// (so it can never be read as an option), then letters, digits, `.`, `_`,
/// `-` and `+`, no `..`, at most 64 bytes. Must agree with `check_app` in
/// `scripts/wc-session`.
pub fn host_app_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && !s.contains("..")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// The example the install step writes when there is no file yet. It is here
/// rather than in a data file so that a deployed `wc-launch.exe` can always
/// produce one.
pub const EXAMPLE: &str = r#"# wc-launch.ini -- see docs/waycompose-design.md §28.
#
# The VM host, over the host-only virtio-net link. No developer workstation is
# in this path and none may be.
[session]
host       = <host-user>@<host-nat-ip>
key        = C:\Users\<guest-user>\wclaunch\id_host
compositor = C:\Users\<guest-user>\wclaunch\sv-guest.exe
relay      = C:\Users\<guest-user>\wclaunch\probe-relay.exe
name       = wcl
arena-mib  = 64
outbound   = 1048576
output     = 1600x1000
port       = 57431
# Seconds the session outlives its last window (§85): a click within it is warm,
# and after it the session ends and gives its arena back. 0 = at once.
linger-secs = 60
# The session's sound plays here, through the default device (§86): `off`
# leaves Firefox with a null audio context on the host instead.
audio        = on
audio-volume = 100
# The loopback port the tunnel forwards to it; default port + 1.
# audio-port = 57432

# Host integration (§106). A file opened here with a Linux application is
# sent by its path in the shared folder; one from elsewhere is copied into the
# drop directory first. Links from Linux open in the default browser unless
# `browser` names one. The defaults:
# [hostint]
# share-host  = Z:\
# share-linux = /srv/winshare
# drop-host   = Z:\waycompose-open
# drop-linux  = /srv/winshare/waycompose-open
# browser     = C:\Program Files\<browser>\<browser>.exe
# browser-arg = --new-window      (repeatable: one argument per line, before the URL)

# One block per application, and a second application is a second shortcut
# rather than a second program.
#
# What runs on the VM host -- the executable, the profile, the preferences, the
# title bar -- is defined there, in apps.ini beside wc-session, and this block
# names it by `host-app` (docs/waycompose-design.md §94). The rest is how it
# looks on this desktop.
[app.firefox]
title    = Firefox (Linux)
icon     = firefox
host-app = firefox
desktop  = firefox
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(extra: &str) -> Config {
        let mut t = String::from(
            "[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n",
        );
        t.push_str(extra);
        Config::parse(&t).expect("should parse")
    }

    #[test]
    fn the_example_is_a_valid_configuration() {
        let cfg = Config::parse(EXAMPLE).expect("the shipped example must parse");
        assert_eq!(cfg.apps.len(), 1);
        let app = cfg.app("firefox").unwrap();
        assert_eq!(app.title, "Firefox (Linux)");
        assert_eq!(app.host_app, "firefox");
        // §94: nothing in the guest's file says what runs on the host.
        for k in ["exec", "prefs", "titlebar", "--profile", "/nix/store/"] {
            assert!(!EXAMPLE.lines().any(|l| !l.starts_with('#') && l.contains(k)), "{k}");
        }
    }

    #[test]
    fn defaults_and_overrides() {
        let cfg = ok("");
        // The default is `Floor::CURRENT`, not a literal -- see `crate::floor`.
        assert_eq!(cfg.arena_mib, crate::floor::Floor::CURRENT.mib());
        assert_eq!(cfg.port, 57431);
        let cfg = ok("arena-mib=64\nport=1\noutput=800x600\n");
        assert_eq!((cfg.arena_mib, cfg.port, cfg.output.as_str()), (64, 1, "800x600"));
    }

    #[test]
    fn a_missing_required_field_is_named() {
        let e = Config::parse("[session]\nhost=u@h\n").unwrap_err();
        assert!(e.contains("compositor"), "{e}");
        // §105: the ssh host and key are required only by the opt-in debug transport.
        let e = Config::parse("[session]\ncompositor=C\ndebug-ssh-transport=on\n").unwrap_err();
        assert!(e.contains("host"), "{e}");
        let c = Config::parse("[session]\ncompositor=C\n").unwrap();
        assert!(!c.debug_ssh_transport && c.host.is_empty() && c.key.is_empty(), "no ssh by default");
        assert_eq!(c.installation(), "wcl");
        assert!(Config::parse("[session]\ncompositor=C\ninstallation=a b\n").is_err());
    }

    #[test]
    fn an_outbound_window_larger_than_the_arena_is_refused() {
        let e = Config::parse(
            "[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\narena-mib=1\noutbound=2097152\n",
        )
        .unwrap_err();
        assert!(e.contains("leaves nothing"), "{e}");
    }

    /// §94. The three keys that used to say what runs on the VM host are
    /// refused by name, so an ini from before §94 fails at the line that
    /// needs changing rather than sending a command line nobody reads.
    #[test]
    fn a_command_line_preferences_or_a_title_bar_in_the_guest_s_file_are_refused() {
        for (k, v) in [("exec", "/bin/firefox --profile /tmp/p"), ("prefs", "a.b=1"), ("titlebar", "none")] {
            let e = Config::parse(&format!("[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n[app.x]\n{k}={v}\n"))
                .unwrap_err();
            assert!(e.contains(&format!("[app.x] {k} is not the guest's")) && e.contains("apps.ini"), "{e}");
        }
    }

    /// §94. The host app id defaults to the section's and is held to the
    /// alphabet `wc-session` accepts: a leading `-` would be an option.
    #[test]
    fn the_host_app_id_defaults_to_the_section_and_is_checked() {
        let base = "[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n[app.ff]\n";
        assert_eq!(Config::parse(base).unwrap().app("ff").unwrap().host_app, "ff");
        let c = Config::parse(&format!("{base}host-app=firefox-esr\n")).unwrap();
        assert_eq!(c.app("ff").unwrap().host_app, "firefox-esr");
        for bad in ["-x", "--dirs", ".x", "a..b", "a/b", "a b", "a;b", "$(id)", &"a".repeat(65)] {
            assert!(Config::parse(&format!("{base}host-app={bad}\n")).is_err(), "{bad}");
            assert!(!host_app_ok(bad), "{bad}");
        }
        assert!(host_app_ok("org.mozilla.firefox") && host_app_ok("7zip+x_y-z"));
    }

    #[test]
    fn an_unknown_app_lists_the_known_ones() {
        let cfg = ok("[app.firefox]\n[app.gimp]\n");
        let e = cfg.app("inkscape").unwrap_err();
        assert!(e.contains("firefox, gimp"), "{e}");
    }

    #[test]
    fn a_typo_is_a_refusal_and_not_a_default() {
        let e = Config::parse("[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\narena_mib=64\n")
            .unwrap_err();
        assert!(e.contains("arena_mib"), "{e}");
    }

    /// §92. The desktop id defaults to the section's, the app id to nothing
    /// (meaning the entry's StartupWMClass), and neither may leave the
    /// alphabet the host accepts.
    #[test]
    fn the_desktop_id_and_app_id_default_and_are_checked() {
        let base = "[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n[app.firefox]\n";
        let c = Config::parse(base).unwrap();
        let a = c.app("firefox").unwrap();
        assert_eq!((a.desktop.as_str(), a.app_id.as_str()), ("firefox", ""));
        let c = Config::parse(&format!("{base}desktop=org.mozilla.firefox\napp-id=firefox-esr\n")).unwrap();
        let a = c.app("firefox").unwrap();
        assert_eq!((a.desktop.as_str(), a.app_id.as_str()), ("org.mozilla.firefox", "firefox-esr"));
        for bad in ["../x", "a b", "a/b", ".x", "a;b"] {
            assert!(Config::parse(&format!("{base}desktop={bad}\n")).is_err(), "{bad}");
            assert!(Config::parse(&format!("{base}app-id={bad}\n")).is_err(), "{bad}");
        }
    }
}

#[cfg(test)]
mod audio_tests {
    use super::*;

    fn parse(extra: &str) -> Result<Config, String> {
        Config::parse(&format!("[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n{extra}"))
    }

    /// §86. Sound is on unless the ini says off, and the two words are the
    /// only two.
    #[test]
    fn audio_is_on_by_default_and_spelled_on_or_off() {
        let c = parse("").unwrap();
        assert!(c.audio);
        assert_eq!(c.audio_volume, 100);
        assert!(!parse("audio = off\n").unwrap().audio);
        assert!(parse("audio = yes\n").is_err());
        assert_eq!(parse("audio-volume = 5\n").unwrap().audio_volume, 5);
        assert!(parse("audio-volume = 101\n").is_err());
        assert!(parse("audio-volume = 300\n").is_err());
        assert_eq!(c.audio_port(), c.port + 1);
        assert_eq!(parse("audio-port = 50000\n").unwrap().audio_port(), 50000);
    }
}

#[cfg(test)]
mod gpu_tests {
    use super::*;

    /// §86, §87. No forwarded GPU unless the ini asks, at most eight sessions,
    /// one port, and the ivshmem slot and doorbell port of testhost's VM
    /// unless the ini names others.
    #[test]
    fn gpu_sessions_are_off_by_default_and_bounded() {
        let p = |e: &str| Config::parse(&format!("[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n{e}"));
        let c = p("").unwrap();
        assert_eq!(c.gpu_sessions, 0);
        assert_eq!(c.gpu_port(), c.port + 10);
        assert_eq!(p("gpu-sessions = 4\ngpu-port = 50000\n").unwrap().gpu_port(), 50000);
        assert!(p("gpu-sessions = 9\n").is_err());
        assert_eq!(c.gpu_arena_mib, 64);
        assert!(c.gpu_pool, "the pool is the default (§97)");
        assert!(!p("gpu-arena = slots\n").unwrap().gpu_pool);
        assert!(p("gpu-arena = pool\n").unwrap().gpu_pool);
        assert!(p("gpu-arena = fixed\n").is_err());
        assert_eq!((c.gpu_ivshmem_slot, c.gpu_bell.as_str()), (0x1d, "org.sirati.svbell.0"));
        let o = p("gpu-ivshmem-slot = 0x1e\ngpu-bell =\n").unwrap();
        assert_eq!((o.gpu_ivshmem_slot, o.gpu_bell.as_str()), (0x1e, ""));
        assert_eq!(p("gpu-ivshmem-slot = 29\n").unwrap().gpu_ivshmem_slot, 0x1d);
        assert!(p("gpu-ivshmem-slot = 0x20\n").is_err());
    }

    /// §89. The rings' port is on by default — a VM without it costs a loud
    /// line and the ssh path, not a failure — and an empty value turns it off.
    #[test]
    fn the_wayland_rings_port_is_on_by_default_and_can_be_turned_off() {
        let p = |e: &str| Config::parse(&format!("[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n{e}"));
        assert_eq!(p("").unwrap().wayland_bell, "org.sirati.wcbell.0");
        // §105: empty only with the debug transport; the rings are the only way.
        assert!(p("wayland-bell =\n").unwrap_err().contains("wayland-bell"));
        assert_eq!(p("wayland-bell =\ndebug-ssh-transport = on\n").unwrap().wayland_bell, "");
        assert_eq!(p("wayland-bell = org.example.x.0\n").unwrap().wayland_bell, "org.example.x.0");
        assert_eq!(p("").unwrap().arena_file, "");
        assert_eq!(p("arena-file = /Volumes/shm/arena\n").unwrap().arena_file, "/Volumes/shm/arena");
    }

    /// §102. The nested-compositor globals are off unless the ini asks.
    #[test]
    fn nested_compositor_globals_are_off_by_default() {
        let p = |e: &str| Config::parse(&format!("[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n{e}"));
        assert!(!p("").unwrap().nested_globals);
        assert!(p("").unwrap().shared_compositor, "§107: one compositor per user by default");
        assert!(!p("shared-compositor = off\n").unwrap().shared_compositor);
        assert!(p("shared-compositor = maybe\n").is_err());
        assert!(p("").unwrap().one_pool, "§110: one pool by default");
        assert!(!p("one-pool = off\n").unwrap().one_pool);
        assert!(p("nested-compositor-globals = on\n").unwrap().nested_globals);
        assert!(!p("nested-compositor-globals = off\n").unwrap().nested_globals);
        assert!(p("nested-compositor-globals = yes\n").is_err());
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn hostint_paths_default_to_the_share_and_are_checked() {
        let ok = |e: &str| Config::parse(&format!("[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n{e}")).expect("should parse");
        let c = ok("");
        let m = c.hostint.path_map();
        assert_eq!(m.shares, [(r"Z:\".to_string(), "/srv/winshare".to_string())]);
        assert_eq!(m.drop.as_ref().map(|d| d.0.as_str()), Some(r"Z:\waycompose-open"));
        let c = ok("[hostint]\nshare-host = Y:\\\nshare-linux = /srv/s\ndrop-host = Y:\\in\ndrop-linux = /srv/s/in\nbrowser = C:\\b\\b.exe\n");
        assert_eq!(c.hostint.share.0, "Y:\\");
        assert_eq!(c.hostint.browser, "C:\\b\\b.exe");
        let bad = |extra: &str| {
            Config::parse(&format!("[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n[hostint]\n{extra}")).is_err()
        };
        assert!(bad("drop-host = C:\\elsewhere\n"), "the drop directory is inside the share");
        assert!(bad("share-linux = srv\n"));
        assert!(bad("browser = C:\\x\\b.bat\n"));
        assert!(bad("colour = red\n"));
    }

    /// The macOS rules, checked here too: POSIX paths, an application name.
    #[test]
    fn hostint_on_a_mac_host() {
        let d = HostInt::for_platform(false);
        assert_eq!(d.check_for(false), Ok(()));
        assert!(d.path_map().shares.is_empty(), "no share unless configured");
        assert_eq!(d.browser, "Firefox");
        let h = |s: (&str, &str), dr: (&str, &str), b: &str| HostInt {
            share: (s.0.into(), s.1.into()),
            drop: (dr.0.into(), dr.1.into()),
            browser: b.into(),
            browser_args: vec![],
        };
        let good = h(("/Users/u", "/Users/u"), ("/Users/u/.wc-open", "/Users/u/.wc-open"), "Safari");
        assert_eq!(good.check_for(false), Ok(()));
        assert_eq!(good.path_map().shares.len(), 1);
        assert!(good.check_for(true).is_err(), "not a Windows configuration");
        assert!(h(("/Users/u", "/Users/u"), ("/tmp/x", "/Users/u/x"), "Firefox").check_for(false).is_err());
        assert!(h(("/Users/u/../v", "/Users/u"), ("", ""), "Firefox").check_for(false).is_err());
        assert!(h(("", ""), ("", ""), "/Applications/Firefox.app").check_for(false).is_err());
        assert!(h(("", ""), ("", ""), "").check_for(false).is_err());
        assert!(h(("", ""), ("/a/b", "/a/b"), "Firefox").check_for(false).is_err(), "a drop needs a share");
        // And the Windows defaults pass the Windows rules.
        assert_eq!(HostInt::for_platform(true).check_for(true), Ok(()));
    }

    /// `browser-arg`: repeated, in order, Windows only, and bounded.
    #[test]
    fn browser_args_are_ordered_bounded_and_windows_only() {
        let mut h = HostInt::for_platform(true);
        h.browser = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe".into();
        h.browser_args = vec!["--user-data-dir=C:\\Temp\\wc edge".into(), "--no-first-run".into(), "--new-window".into()];
        assert_eq!(h.check_for(true), Ok(()));
        let mut bad = h.clone();
        bad.browser_args.push("a\nb".into());
        assert!(bad.check_for(true).is_err(), "a control character");
        let mut bad = h.clone();
        bad.browser_args = vec!["x".into(); MAX_BROWSER_ARGS + 1];
        assert!(bad.check_for(true).is_err(), "too many");
        let mut bad = h.clone();
        bad.browser_args = vec!["x".repeat(MAX_BROWSER_ARG_LEN + 1)];
        assert!(bad.check_for(true).is_err(), "too long");
        let mut bad = h.clone();
        bad.browser.clear();
        assert!(bad.check_for(true).is_err(), "arguments for no browser");
        let mut mac = HostInt::for_platform(false);
        assert_eq!(mac.check_for(false), Ok(()));
        mac.browser_args = vec!["--new-window".into()];
        assert!(mac.check_for(false).is_err(), "not on macOS");
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn browser_arg_lines_are_parsed_in_order() {
        let c = Config::parse(
            "[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n[hostint]\nbrowser = C:\\e\\msedge.exe\n\
             browser-arg = --user-data-dir=C:\\Temp\\p q\nbrowser-arg = --no-first-run\n",
        )
        .unwrap();
        assert_eq!(c.hostint.browser_args, ["--user-data-dir=C:\\Temp\\p q", "--no-first-run"]);
    }
}

/// §105. An installation name as the VM host's session service accepts it
/// (`wc_handle::macboot::valid_installation`, which is not a dependency of
/// this crate on Linux): 1 to 32 of letters, digits, `-` and `_`.
pub fn installation_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ------------------------------------------------------------ §92.12 ---

/// §92.12. The `taskbar-id` of each `[app.*]` section that has one, read
/// from an ini's text without parsing the rest: deploy reads the previous
/// ini with it, which may be of an older launcher.
pub fn taskbar_ids(text: &str) -> BTreeMap<String, TaskbarId> {
    let mut out = BTreeMap::new();
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        if let (Some(id), Some((k, v))) = (section.strip_prefix("app."), line.split_once('='))
            && k.trim() == "taskbar-id"
            && let Some(t) = TaskbarId::parse(v.trim())
        {
            out.insert(id.to_string(), t);
        }
    }
    out
}

/// §92.12. `text` with `[app.<section>] taskbar-id = <value>`: the line
/// replaced where the section has one, else added as the section's first
/// key. Every other byte is kept, comments and line endings included.
pub fn set_taskbar_id(text: &str, section: &str, value: TaskbarId) -> Result<String, String> {
    let header = format!("app.{section}");
    let mut out = String::with_capacity(text.len() + 32);
    let mut in_section = false;
    let mut found_section = false;
    let mut done = false;
    for raw in text.split_inclusive('\n') {
        let eol = if raw.ends_with("\r\n") { "\r\n" } else { "\n" };
        let line = raw.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            in_section = name.trim() == header;
            out.push_str(raw);
            if in_section && !done {
                found_section = true;
                if !raw.ends_with('\n') {
                    out.push_str(eol);
                }
                out.push_str(&format!("taskbar-id = {}{eol}", value.as_str()));
                done = true;
            }
            continue;
        }
        if in_section && line.split_once('=').is_some_and(|(k, _)| k.trim() == "taskbar-id") {
            // The value written above under the header replaces this one.
            continue;
        }
        out.push_str(raw);
    }
    if !found_section {
        return Err(format!("no [app.{section}] in the configuration"));
    }
    Ok(out)
}

#[cfg(test)]
mod taskbar_id_tests {
    use super::*;

    const INI: &str = "[session]\ncompositor=C\n\n[app.firefox]\ntitle = Firefox (Linux)\n# a comment\n\n[app.atp-nested]\ntitle = W\ntaskbar-id = legacy\n";

    #[test]
    fn taskbar_id_is_parsed_defaults_to_install_and_is_checked() {
        let c = Config::parse(INI).unwrap();
        assert_eq!(c.apps["firefox"].taskbar_id, TaskbarId::Install);
        assert_eq!(c.apps["atp-nested"].taskbar_id, TaskbarId::Legacy);
        assert!(Config::parse("[session]\ncompositor=C\n[app.x]\ntaskbar-id = mine\n").unwrap_err().contains("legacy or install"));
    }

    #[test]
    fn the_ids_are_read_from_any_ini_text() {
        let ids = taskbar_ids(INI);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids["atp-nested"], TaskbarId::Legacy);
        // An ini an older launcher would refuse is still read for this.
        let ids = taskbar_ids("[session]\nnot a line\n[app.f]\nexec = x\ntaskbar-id = install\n");
        assert_eq!(ids["f"], TaskbarId::Install);
        assert!(taskbar_ids("[session]\ntaskbar-id = legacy\n").is_empty(), "only [app.*]");
    }

    #[test]
    fn setting_an_id_changes_that_line_and_nothing_else() {
        let t = set_taskbar_id(INI, "firefox", TaskbarId::Legacy).unwrap();
        assert_eq!(t, INI.replace("[app.firefox]\n", "[app.firefox]\ntaskbar-id = legacy\n"));
        let t = set_taskbar_id(&t, "atp-nested", TaskbarId::Install).unwrap();
        assert!(t.ends_with("[app.atp-nested]\ntaskbar-id = install\ntitle = W\n"), "{t}");
        assert_eq!(taskbar_ids(&t).len(), 2);
        let c = Config::parse(&t).unwrap();
        assert_eq!(c.apps["firefox"].taskbar_id, TaskbarId::Legacy);
        assert_eq!(c.apps["atp-nested"].taskbar_id, TaskbarId::Install);
        // Twice is once.
        assert_eq!(set_taskbar_id(&t, "firefox", TaskbarId::Legacy).unwrap(), t);
        assert!(set_taskbar_id(INI, "thunderbird", TaskbarId::Legacy).is_err());
        let crlf = INI.replace('\n', "\r\n");
        let t = set_taskbar_id(&crlf, "firefox", TaskbarId::Legacy).unwrap();
        assert!(t.contains("[app.firefox]\r\ntaskbar-id = legacy\r\ntitle"), "{t:?}");
        assert!(!t.replace("\r\n", "").contains('\n'));
    }
}
