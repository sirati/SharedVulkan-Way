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
    /// The arena, in MiB of the guest's own pinned RAM.
    pub arena_mib: u64,
    /// How much of it the compositor keeps for the objects it makes for clients.
    pub outbound: u64,
    /// The one `wl_output`, `WxH`.
    pub output: String,
    /// The relay's loopback port inside the guest.
    pub port: u16,
    /// How long a session may last before the compositor's own deadline ends
    /// it. Not an idle timeout — see docs/design.md §19.6.
    pub session_secs: u64,
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
            arena_mib: 256,
            outbound: 1024 * 1024,
            output: "1600x1000".into(),
            port: 57431,
            session_secs: 43200,
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
        }
        Ok(())
    }

    pub fn app(&self, id: &str) -> Result<&App, String> {
        self.apps.get(id).ok_or_else(|| {
            let known: Vec<&str> = self.apps.keys().map(String::as_str).collect();
            format!("no [app.{id}] in the configuration; it has {}", known.join(", "))
        })
    }

    pub fn arena_bytes(&self) -> u64 {
        self.arena_mib * 1024 * 1024
    }
}

/// The example the install step writes when there is no file yet. It is here
/// rather than in a data file so that a deployed `wc-launch.exe` can always
/// produce one.
pub const EXAMPLE: &str = r#"# wc-launch.ini -- see docs/design.md §19.
#
# The VM host, over the host-only virtio-net link. No developer workstation is
# in this path and none may be.
[session]
host       = <host-user>@<host-nat-ip>
key        = C:\Users\<guest-user>\wclaunch\id_host
compositor = C:\Users\<guest-user>\wclaunch\wc-compositor.exe
relay      = C:\Users\<guest-user>\wclaunch\probe-relay.exe
name       = wcl
arena-mib  = 256
outbound   = 1048576
output     = 1600x1000
port       = 57431

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
        assert_eq!(cfg.arena_mib, 256);
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
}
