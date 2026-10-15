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
    /// The command line to run on the VM host, as written.
    pub exec: String,
    /// Preferences the *deployment* sets on the application's profile, as
    /// `key=value` separated by `;`.
    ///
    /// This is the same class of thing as `MOZ_GTK_TITLEBAR_DECORATION=none` in
    /// `wc-session run` and it exists for the same reason: a setting that has to
    /// be true on the client's machine, that no protocol carries, and that the
    /// launcher is the only thing in a position to state. `middlemouse.paste` is
    /// the one that made it necessary — on Linux a middle click pastes the
    /// primary selection, and the window this launcher opens is on a Windows
    /// desktop where a middle click does not.
    pub prefs: String,
    /// What the application does with its own title bar: `tabs`, `none` or
    /// `client`. See [`crate::plan::Titlebar`].
    ///
    /// **`tabs` by default since §82**, which is the goal clause
    /// `docs/goal-firefox.md` states: Firefox's tab strip drawn *inside*
    /// Windows' caption, Windows' own minimise, maximise and close in its
    /// corner. The compositor's half has existed since §31 and is now its
    /// default (`auto` folds the caption of a client that asked to decorate
    /// itself); this half is the three things only the client's machine can
    /// be told — `MOZ_GTK_TITLEBAR_DECORATION=client`, preferences that turn
    /// Firefox's own window buttons off, and a `userChrome.css` that reserves
    /// the buttons' corner and fits the tab strip to the caption's height.
    ///
    /// `none` is the per-application opt-out: Windows' caption, the tab strip
    /// under it — §22's arrangement, unchanged. Until §82 it was the only
    /// value, because the comment here said the compositor's half "does not
    /// exist yet"; it had existed for months.
    pub titlebar: String,
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
                        exec: String::new(),
                        prefs: String::new(),
                        titlebar: "tabs".into(),
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
                    "exec" => app.exec = v.into(),
                    "prefs" => app.prefs = v.into(),
                    "titlebar" => app.titlebar = v.into(),
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
                ("session", "audio-volume") => {
                    cfg.audio_volume = u8::try_from(num("audio-volume")?)
                        .ok()
                        .filter(|v| *v <= 100)
                        .ok_or_else(|| format!("line {}: audio-volume is 0..=100", n + 1))?
                }
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
            if v.is_empty() {
                return Err(format!("[session] {what} is required"));
            }
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
            if app.exec.trim().is_empty() {
                return Err(format!("[app.{id}] exec is required"));
            }
            // §82. A typo here would otherwise reach the VM host as an
            // environment variable Firefox does not recognise, and Firefox
            // answers an unknown value by decorating itself — a window that
            // looks almost right, which is the worst kind.
            if crate::plan::Titlebar::parse(&app.titlebar).is_none() {
                return Err(format!(
                    "[app.{id}] titlebar = {} is not one of tabs, none, client",
                    app.titlebar
                ));
            }
        }
        Ok(())
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

# One block per application, and a second application is a second shortcut
# rather than a second program.
#
# `exec` is resolved on the VM host, and `/run/current-system/sw/bin/firefox` is
# a symlink into that machine's current system rather than a store path: a store
# path is correct for exactly one rebuild.
[app.firefox]
title = Firefox (Linux)
icon  = firefox
exec  = firefox --profile ~/.wc-launch/firefox
# A middle click on Windows does not paste, so it must not paste here either.
prefs = middlemouse.paste=false
# Firefox's tabs in Windows' caption, with Windows' own minimise, maximise and
# close in its corner. `none` opts this application out: Windows' caption above,
# the tab strip below. See docs/waycompose-design.md §82.
titlebar = tabs
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
        // The store path changes on every rebuild of the host; a launcher that
        // hard-coded one would work until the next `nixos-rebuild`.
        assert!(!app.exec.contains("/nix/store/"), "{}", app.exec);
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
        assert!(e.contains("key"), "{e}");
    }

    #[test]
    fn an_outbound_window_larger_than_the_arena_is_refused() {
        let e = Config::parse(
            "[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\narena-mib=1\noutbound=2097152\n",
        )
        .unwrap_err();
        assert!(e.contains("leaves nothing"), "{e}");
    }

    #[test]
    fn an_app_without_an_exec_is_refused() {
        let e = Config::parse("[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n[app.x]\n")
            .unwrap_err();
        assert!(e.contains("[app.x] exec"), "{e}");
    }

    #[test]
    fn an_unknown_app_lists_the_known_ones() {
        let cfg = ok("[app.firefox]\nexec=/bin/firefox\n[app.gimp]\nexec=/bin/gimp\n");
        let e = cfg.app("inkscape").unwrap_err();
        assert!(e.contains("firefox, gimp"), "{e}");
    }

    #[test]
    fn a_typo_is_a_refusal_and_not_a_default() {
        let e = Config::parse("[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\narena_mib=64\n")
            .unwrap_err();
        assert!(e.contains("arena_mib"), "{e}");
    }

    /// §82: Firefox's tabs in the caption unless a block says otherwise, and
    /// the shipped example says so in as many words.
    #[test]
    fn titlebar_defaults_to_tabs_and_each_app_can_opt_out() {
        let cfg = ok("[app.firefox]\nexec=/bin/firefox\n[app.other]\nexec=/bin/firefox\ntitlebar=none\n");
        assert_eq!(cfg.app("firefox").unwrap().titlebar, "tabs");
        assert_eq!(cfg.app("other").unwrap().titlebar, "none");
        assert_eq!(Config::parse(EXAMPLE).unwrap().app("firefox").unwrap().titlebar, "tabs");
    }

    #[test]
    fn a_titlebar_typo_is_refused_by_name() {
        let e = Config::parse(
            "[session]\nhost=u@h\nkey=K\ncompositor=C\nrelay=R\n[app.x]\nexec=/bin/x\ntitlebar=tab\n",
        )
        .unwrap_err();
        assert!(e.contains("[app.x] titlebar = tab"), "{e}");
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
