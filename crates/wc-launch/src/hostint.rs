//! §106 in the launcher: a Linux application registered with Windows for
//! the types its desktop entry opens (`--install` writes it, and
//! `--uninstall-assoc` removes it through the manifest), and a file this
//! machine asks Linux to open (`<app> --file PATH`, which the registration's
//! command line runs).
//!
//! The registration is `wc_hostint::assoc::windows_plan`, written under
//! HKCU by `wc_hostplat::win::registry`, and listed in
//! `apps\<section>.assoc` beside the launcher, which uninstall reads. A file
//! becomes the `open` request a jump-list page already is
//! (`Request::Open`): the `file:///` URL of its path on the VM host, after a
//! copy into the share's drop directory if it was not in the share.

use std::path::{Path, PathBuf};

use wc_hostint::assoc::LinuxApp;
use wc_hostint::mime::scheme_of;

use crate::appmeta::Meta;
use crate::config::App;

/// What the registration says about an application: its name from the
/// host's desktop entry (the shortcut's title until the host has answered),
/// its file types and its URL schemes.
pub fn linux_app(section: &str, app: &App, meta: Option<&Meta>) -> LinuxApp {
    // The shortcut's title is this installation's name for the application
    // ("Firefox (Linux)", "Firefox (Linux HI test)"), so "Open with" tells two
    // installations apart; the desktop entry's name only when there is none.
    let title = app.title.trim();
    let name = if !title.is_empty() {
        title
    } else {
        meta.map(|m| m.name.trim()).filter(|n| !n.is_empty()).unwrap_or("Linux application")
    };
    let mut mimes = Vec::new();
    let mut schemes = Vec::new();
    for t in meta.map(|m| m.mimes.as_slice()).unwrap_or_default() {
        match scheme_of(t) {
            Some(s) => schemes.push(s),
            None => mimes.push(t.clone()),
        }
    }
    LinuxApp { section: section.into(), name: name.into(), mimes, schemes }
}

/// Where an application's registration manifest lives.
pub fn manifest_path(apps: &Path, section: &str) -> PathBuf {
    apps.join(format!("{section}.assoc"))
}

/// A path `--file` accepts: absolute on a drive, no control characters,
/// no alternate data stream.
pub fn file_arg_ok(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 4
        && b.len() <= 4096
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/')
        && !p[2..].contains(':')
        && !p.chars().any(char::is_control)
}

#[cfg(windows)]
pub use win::{deploy, file_url, uninstall};

#[cfg(windows)]
mod win {
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    use wc_hostint::assoc::{manifest, parse_manifest, windows_plan};
    use wc_hostplat::share::{FilePlan, plan, unique_dir};
    use wc_hostplat::win::registry;

    use super::*;
    use crate::Log;
    use crate::config::Config;

    /// Register `[app.<section>]` for its types. Never fatal: what fails is
    /// logged. A previous registration of the same application is removed
    /// first, so a type the application no longer opens goes too.
    pub fn deploy(cfg: &Config, apps: &Path, launcher: &str, icon: Option<&str>, section: &str, meta: Option<&Meta>, log: &mut Log) {
        let Ok(app) = cfg.app(section) else { return };
        let la = linux_app(section, app, meta);
        if la.mimes.is_empty() && la.schemes.is_empty() {
            log.say(&format!("assoc: {section}: the host named no types for it, so nothing is registered"));
            remove_manifest(apps, section, log);
            return;
        }
        remove_manifest(apps, section, log);
        let icon = icon.unwrap_or(launcher);
        let p = windows_plan(&la, launcher, icon, &cfg.session, &registry::scheme_exists);
        let mpath = manifest_path(apps, section);
        // The manifest first: a registration that stops half-way is still
        // removed by uninstall.
        let _ = std::fs::create_dir_all(apps);
        if let Err(e) = std::fs::write(&mpath, manifest(&p.undo)) {
            log.say(&format!("assoc: {}: {e}; nothing registered", mpath.display()));
            return;
        }
        match registry::apply(&p) {
            Ok(n) => log.say(&format!(
                "assoc: {section}: {} type(s), {} scheme(s), {n} value(s) under HKCU{}",
                la.mimes.len(),
                la.schemes.len(),
                if p.unmapped.is_empty() {
                    String::new()
                } else {
                    format!("; no Windows extension for {}", p.unmapped.join(", "))
                }
            )),
            Err(e) => log.say(&format!("assoc: {section}: {e}")),
        }
    }

    fn remove_manifest(apps: &Path, section: &str, log: &mut Log) -> bool {
        let mpath = manifest_path(apps, section);
        let Ok(text) = std::fs::read_to_string(&mpath) else { return true };
        let undo = parse_manifest(&text);
        match registry::remove(&undo) {
            Ok(n) => {
                log.say(&format!("assoc: {section}: removed {n} item(s)"));
                let _ = std::fs::remove_file(&mpath);
                true
            }
            Err(errs) => {
                log.say(&format!("assoc: {section}: {} item(s) could not be removed: {}", errs.len(), errs.join("; ")));
                false
            }
        }
    }

    /// `--uninstall-assoc [SECTION]`: one application's registration, or
    /// every one this installation wrote.
    pub fn uninstall(apps: &Path, section: Option<&str>, log: &mut Log) -> Result<(), String> {
        let sections: Vec<String> = match section {
            Some(s) => vec![s.to_string()],
            None => std::fs::read_dir(apps)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".assoc").map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let mut ok = true;
        for s in &sections {
            ok &= remove_manifest(apps, s, log);
        }
        log.say(&format!("assoc: uninstalled {} application(s)", sections.len()));
        if ok { Ok(()) } else { Err("some registry entries could not be removed; see the log".into()) }
    }

    static N: AtomicU32 = AtomicU32::new(0);

    /// The URL Linux is sent for a file on this machine, copying it into
    /// the share's drop directory first if it is not in the share.
    pub fn file_url(cfg: &Config, path: &str, log: &mut Log) -> Result<String, String> {
        if !file_arg_ok(path) {
            return Err("--file wants an absolute path on a drive".into());
        }
        let abs = std::path::absolute(path).map_err(|e| format!("--file: {e}"))?;
        let meta = std::fs::metadata(&abs).map_err(|e| format!("--file: {e}"))?;
        if !meta.is_file() {
            return Err("--file names something that is not a file".into());
        }
        let abs = abs.display().to_string();
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let unique = unique_dir(secs, std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        match plan(&abs, &cfg.hostint.path_map(), &unique) {
            Some(FilePlan::Shared { url }) => Ok(url),
            Some(FilePlan::Copy { host_dir, host_dest, url }) => {
                std::fs::create_dir_all(&host_dir).map_err(|e| format!("{host_dir}: {e}"))?;
                let n = std::fs::copy(&abs, &host_dest).map_err(|e| format!("copy into {host_dir}: {e}"))?;
                // The file's name is the user's; only its size is logged.
                log.say(&format!("file: copied {n} bytes into the share's drop directory"));
                Ok(url)
            }
            None => Err("the file is outside the share and no drop directory is configured".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App {
            title: "Firefox (Linux)".into(),
            icon: "firefox".into(),
            host_app: "firefox".into(),
            desktop: "firefox".into(),
            app_id: String::new(),
            single: false,
        }
    }

    #[test]
    fn the_registration_follows_the_desktop_entry() {
        let meta = Meta::parse(
            "desktop /d/firefox.desktop\nprogram firefox\nname RmlyZWZveA==\nmime text/html\nmime x-scheme-handler/https\nmime x-scheme-handler/http\n",
        )
        .unwrap();
        let la = linux_app("firefox", &app(), Some(&meta));
        assert_eq!(la.name, "Firefox (Linux)", "the shortcut's title, this installation's name");
        assert_eq!(la.mimes, ["text/html"]);
        assert_eq!(la.schemes, ["https", "http"]);
        let p = wc_hostint::assoc::windows_plan(&la, "C:\\wc\\wc-launch.exe", "C:\\wc\\firefox.ico", "wcl", &|_| true);
        let cmd = p.values.iter().find(|v| v.key.ends_with("\\shell\\open\\command")).unwrap();
        assert_eq!(
            cmd.data,
            wc_hostint::assoc::RegData::Str("\"C:\\wc\\wc-launch.exe\" --session-name \"wcl\" \"firefox\" --file \"%1\"".into())
        );
        // Before the host answered: the title, and nothing to register.
        let la = linux_app("firefox", &app(), None);
        assert_eq!(la.name, "Firefox (Linux)");
        // A second installation's title keeps it apart in "Open with".
        let mut a2 = app();
        a2.title = "Firefox (Linux HI test)".into();
        let la2 = linux_app("ffhi", &a2, Some(&meta));
        let p2 = wc_hostint::assoc::windows_plan(&la2, "C:\\l.exe", "C:\\i.ico", "wchi", &|_| true);
        assert!(p2.values.iter().any(|v| v.name.is_empty() && v.data == wc_hostint::assoc::RegData::Str("Firefox (Linux HI test)".into())));
        assert!(la.mimes.is_empty() && la.schemes.is_empty());
    }

    #[test]
    fn file_arguments_are_drive_paths() {
        assert!(file_arg_ok("C:\\Users\\u\\a b.pdf"));
        assert!(file_arg_ok("Z:/x.txt"));
        assert!(!file_arg_ok("\\\\server\\share\\x"));
        assert!(!file_arg_ok("C:\\x.txt:stream"));
        assert!(!file_arg_ok("relative.txt"));
        assert!(!file_arg_ok("C:\\a\nb"));
        assert_eq!(manifest_path(Path::new("apps"), "firefox"), Path::new("apps").join("firefox.assoc"));
    }
}
