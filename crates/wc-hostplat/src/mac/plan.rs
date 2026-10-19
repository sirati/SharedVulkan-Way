//! The decisions of the macOS half, without a call into macOS: what `open`
//! is run with, which Launch Services answers become which handler offers,
//! what CoreAudio's stream configuration says about channels, and which
//! directories are application bundles. Compiled and tested on every target;
//! the FFI modules beside it only gather the inputs.

use std::path::Path;

use crate::handlers::{App, Found, Key};
use wc_hostint::wire::Target;

/// The main process's bundle identifier (§106, TCC rule 1): the only
/// process that ever opens a microphone, and the identity the user's grant
/// is kept under.
pub const HOST_BUNDLE_ID: &str = "org.waycompose.host";
/// Every bundle waycompose writes starts with this: the main process's and
/// each Linux application's (`org.waycompose.linux.<section>`). None of them
/// is ever offered to Linux as a host handler, and none is ever the target
/// of a host-side open: a Linux application opening a file "on the host"
/// that the host then hands back to Linux would loop.
pub const OWN_BUNDLE_PREFIX: &str = "org.waycompose.";
/// `open`, by absolute path: never `$PATH`.
pub const OPEN: &str = "/usr/bin/open";
/// The browser `url` requests go to unless `hostint.ini` names another.
pub const DEFAULT_BROWSER: &str = "Firefox";

/// Whether a bundle id is one of ours.
pub fn is_own_bundle(bundle_id: &str) -> bool {
    bundle_id.to_ascii_lowercase().starts_with(OWN_BUNDLE_PREFIX)
}

/// `open`'s arguments for a URL Linux asked the host's browser to show.
///
/// http and https go to the configured browser, by name (`-a`), if it is
/// installed. Anything else that passed the scheme check (`mailto:`), and
/// http(s) when the configured browser is not installed, goes to the
/// system's handler for the scheme (`open -- URL`): the URL already passed
/// the host's check, and the system's handler is the one the user chose.
/// The caller refuses that second form when the system's handler is one of
/// ours (see [`is_own_bundle`]), so a Linux browser set as the Mac's default
/// cannot bounce the URL back to Linux.
pub fn open_url_argv(url: &str, browser: Option<&str>) -> Vec<String> {
    let web = url.starts_with("http://") || url.starts_with("https://");
    match browser {
        Some(app) if web && !app.is_empty() => vec!["-a".into(), app.into(), "--".into(), url.into()],
        _ => vec!["--".into(), url.into()],
    }
}

/// `open`'s arguments for a host file, with its default application.
pub fn open_path_argv(path: &str) -> Vec<String> {
    vec!["--".into(), path.into()]
}

/// `open`'s arguments for a URL or a host file with one application, named
/// by its bundle identifier (which only the host ever held).
pub fn open_with_argv(bundle_id: &str, target: &Target) -> Vec<String> {
    let t = match target {
        Target::Url(u) => u,
        Target::HostPath(p) => p,
    };
    vec!["-b".into(), bundle_id.into(), "--".into(), t.clone()]
}

/// Extensions of directories that macOS runs or loads rather than shows.
/// A directory with one of them, or with `Contents/Info.plist` or
/// `Contents/MacOS` inside, is never opened (security-model R38): `open`
/// on an `.app` starts it.
pub const BUNDLE_EXTENSIONS: &[&str] = &[
    "app", "bundle", "framework", "plugin", "kext", "prefpane", "appex", "xpc", "workflow", "action", "service",
    "saver", "qlgenerator", "mdimporter", "osax", "component", "systemextension", "dext", "driver",
];

/// Whether a directory is an executable bundle. `exists` answers for a path
/// inside it (the caller's file system, or a test's).
pub fn is_bundle_dir(dir: &Path, exists: &dyn Fn(&Path) -> bool) -> bool {
    let ext = dir.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    if ext.as_deref().is_some_and(|e| BUNDLE_EXTENSIONS.contains(&e)) {
        return true;
    }
    exists(&dir.join("Contents/Info.plist")) || exists(&dir.join("Contents/MacOS"))
}

/// The channels an `AudioBufferList` describes: the sum of its buffers'
/// `mNumberChannels`. The bytes are what `kAudioDevicePropertyStreamConfiguration`
/// returns on a 64-bit Mac: a `u32` count, padding to 8, then 16-byte
/// `AudioBuffer`s (`u32` channels, `u32` size, a pointer).
pub fn channels_of_buffer_list(b: &[u8]) -> u32 {
    let Some(n) = b.get(..4).map(|x| u32::from_ne_bytes(x.try_into().unwrap())) else { return 0 };
    let mut sum = 0u32;
    for i in 0..n as usize {
        let at = 8 + i * 16;
        match b.get(at..at + 4) {
            Some(x) => sum = sum.saturating_add(u32::from_ne_bytes(x.try_into().unwrap())),
            None => break,
        }
    }
    sum
}

/// One type's or scheme's Launch Services answer as the shared merge
/// (`crate::handlers::collect`) takes it: every bundle that opens it and the
/// default, with our own bundles left out (no loops) and bundles that are
/// not on disk any more left out too (`name` says `None`: they could not be
/// opened). A default that is one of ours is no default.
pub fn found(key: Key, all: &[String], default: Option<&str>, name: &mut dyn FnMut(&str) -> Option<String>) -> Found {
    let mut apps: Vec<App> = Vec::new();
    for bid in all.iter().map(String::as_str).chain(default) {
        if bid.is_empty() || is_own_bundle(bid) || apps.iter().any(|a| a.id.eq_ignore_ascii_case(bid)) {
            continue;
        }
        if let Some(n) = name(bid) {
            apps.push(App { id: bid.to_string(), name: n });
        }
    }
    let default = default.filter(|d| apps.iter().any(|a| a.id.eq_ignore_ascii_case(d))).map(String::from);
    Found { key, apps, default }
}

/// The MIME types of `wc_hostint::mime::TABLE` with their UTIs, in table
/// order (`public.xml` twice: `application/xml` and `text/xml`).
pub fn types() -> Vec<(&'static str, &'static str)> {
    wc_hostint::mime::TABLE.iter().map(|(m, u, _)| (*m, *u)).collect()
}

/// The display name of an application from its bundle's path:
/// `/Applications/TextEdit.app` is "TextEdit". What Finder shows for an
/// application whose bundle was not renamed.
pub fn app_name_of_path(path: &str) -> Option<String> {
    let last = path.trim_end_matches('/').rsplit('/').next()?;
    let n = last.strip_suffix(".app").unwrap_or(last);
    (!n.is_empty()).then(|| wc_hostint::wire::field(n))
}

/// The UTIs to ask Launch Services about, once each, in table order.
pub fn utis() -> Vec<&'static str> {
    let mut v: Vec<&str> = Vec::new();
    for (_, u, _) in wc_hostint::mime::TABLE {
        if !v.contains(u) {
            v.push(u);
        }
    }
    v
}

/// The URL schemes whose handlers are offered.
pub const SCHEMES: &[&str] = &["http", "https", "mailto"];

/// The microphone permission, as AVFoundation reports it
/// (`AVAuthorizationStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicAuth {
    /// Never asked: asking shows the dialog.
    Undetermined,
    /// Refused by the user, or restricted by policy: the input stays silent.
    Denied,
    Granted,
}

impl MicAuth {
    /// From `AVAuthorizationStatus`: 0 not determined, 1 restricted,
    /// 2 denied, 3 authorized. Anything else is taken as a refusal.
    pub fn from_status(s: isize) -> MicAuth {
        match s {
            0 => MicAuth::Undetermined,
            3 => MicAuth::Granted,
            _ => MicAuth::Denied,
        }
    }
}

/// What an output's queue worker is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutCtl {
    Start,
    Pause,
}

/// An output's next queue command, if any: start once `prebuffer` bytes are
/// queued, pause once it has had nothing to play for `idle_pause`. The
/// command itself runs on the output's own worker, never on the ring's
/// thread or under the state lock: CoreAudio can hold an AudioQueue start
/// for as long as a TCC dialog is up.
pub fn output_control(running: bool, queued: usize, prebuffer: usize, idle_for: std::time::Duration, idle_pause: std::time::Duration) -> Option<OutCtl> {
    if !running && queued >= prebuffer {
        Some(OutCtl::Start)
    } else if running && queued == 0 && idle_for > idle_pause {
        Some(OutCtl::Pause)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_argv_never_lets_a_url_be_an_option() {
        assert_eq!(open_url_argv("https://a/", Some("Firefox")), ["-a", "Firefox", "--", "https://a/"]);
        assert_eq!(open_url_argv("https://a/", None), ["--", "https://a/"]);
        // mailto goes to the system's mail handler, not to the browser.
        assert_eq!(open_url_argv("mailto:x@y", Some("Firefox")), ["--", "mailto:x@y"]);
        assert_eq!(open_path_argv("-a"), ["--", "-a"]);
        assert_eq!(
            open_with_argv("com.apple.TextEdit", &Target::HostPath("/Users/q/a.txt".into())),
            ["-b", "com.apple.TextEdit", "--", "/Users/q/a.txt"]
        );
        assert_eq!(open_with_argv("org.mozilla.firefox", &Target::Url("https://x/".into()))[3], "https://x/");
    }

    #[test]
    fn bundles_are_recognised_by_extension_or_contents() {
        let none = |_: &Path| false;
        assert!(is_bundle_dir(Path::new("/Users/q/Calc.app"), &none));
        assert!(is_bundle_dir(Path::new("/Users/q/X.APP"), &none));
        assert!(is_bundle_dir(Path::new("/Users/q/t.workflow"), &none));
        assert!(!is_bundle_dir(Path::new("/Users/q/Photos"), &none));
        // A renamed bundle is still one.
        let plist = |p: &Path| p.ends_with("Contents/Info.plist");
        assert!(is_bundle_dir(Path::new("/Users/q/notes"), &plist));
        let macos = |p: &Path| p.ends_with("Contents/MacOS");
        assert!(is_bundle_dir(Path::new("/Users/q/notes"), &macos));
    }

    #[test]
    fn buffer_lists_sum_their_channels() {
        let mut b = vec![0u8; 8 + 2 * 16];
        b[..4].copy_from_slice(&2u32.to_ne_bytes());
        b[8..12].copy_from_slice(&2u32.to_ne_bytes());
        b[24..28].copy_from_slice(&6u32.to_ne_bytes());
        assert_eq!(channels_of_buffer_list(&b), 8);
        // No streams in this direction: the device is not offered for it.
        assert_eq!(channels_of_buffer_list(&0u32.to_ne_bytes()), 0);
        // A short answer never reads past the end.
        let mut s = vec![0u8; 12];
        s[..4].copy_from_slice(&5u32.to_ne_bytes());
        s[8..12].copy_from_slice(&1u32.to_ne_bytes());
        assert_eq!(channels_of_buffer_list(&s), 1);
        assert_eq!(channels_of_buffer_list(&[]), 0);
    }

    fn names(bid: &str) -> Option<String> {
        match bid {
            "com.apple.TextEdit" | "com.apple.textedit" => Some("TextEdit".into()),
            "com.apple.Safari" => Some("Safari".into()),
            "org.mozilla.firefox" => Some("Firefox".into()),
            "com.apple.mail" => Some("Mail".into()),
            _ => None,
        }
    }

    #[test]
    fn launch_services_answers_become_offers_without_our_own_bundles() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let found = vec![
            found(
                Key::Mime("text/plain".into()),
                &s(&["com.apple.TextEdit", "org.waycompose.linux.text-editor"]),
                Some("com.apple.TextEdit"),
                &mut names,
            ),
            found(Key::Mime("application/xml".into()), &s(&["com.apple.textedit", "com.gone.App"]), None, &mut names),
            found(
                Key::Mime("text/html".into()),
                &s(&["com.apple.Safari", "org.mozilla.firefox"]),
                Some("org.waycompose.linux.firefox"),
                &mut names,
            ),
            found(
                Key::Scheme("https".into()),
                &s(&["com.apple.Safari", "org.mozilla.firefox", "org.waycompose.linux.firefox"]),
                Some("com.apple.Safari"),
                &mut names,
            ),
            found(Key::Scheme("mailto".into()), &s(&["com.apple.mail"]), Some("com.apple.mail"), &mut names),
        ];
        // A gone bundle and our own are not there; a default of ours is none.
        assert_eq!(found[1].apps.len(), 1);
        assert_eq!(found[2].default, None);
        assert!(found.iter().flat_map(|f| &f.apps).all(|a| !is_own_bundle(&a.id)));
        let o = crate::handlers::collect(&found);
        let ns: Vec<&str> = o.iter().map(|h| h.offer.name.as_str()).collect();
        assert_eq!(ns, ["Firefox", "Mail", "Safari", "TextEdit"]);
        let te = o.iter().find(|h| h.offer.name == "TextEdit").unwrap();
        assert_eq!(te.app, "com.apple.TextEdit", "one TextEdit, whatever the spelling");
        assert_eq!(te.offer.mimes, ["text/plain", "application/xml"]);
        assert_eq!(te.offer.default_for, ["text/plain"]);
        let sa = o.iter().find(|h| h.offer.name == "Safari").unwrap();
        assert_eq!(sa.offer.schemes, ["https"]);
        assert_eq!(sa.offer.default_for, ["x-scheme-handler/https"]);
        assert!(o.iter().all(|h| !h.offer.default_for.iter().any(|d| d == "text/html")));
        // Tokens are opaque: no bundle id crosses.
        for h in &o {
            assert!(wc_hostint::wire::token_ok(&h.offer.token));
            assert!(!h.offer.token.contains("apple"));
        }
    }

    #[test]
    fn the_microphone_status_and_the_output_commands() {
        use std::time::Duration;
        assert_eq!(MicAuth::from_status(0), MicAuth::Undetermined);
        assert_eq!(MicAuth::from_status(1), MicAuth::Denied);
        assert_eq!(MicAuth::from_status(2), MicAuth::Denied);
        assert_eq!(MicAuth::from_status(3), MicAuth::Granted);
        assert_eq!(MicAuth::from_status(7), MicAuth::Denied);
        let (s, idle) = (Duration::from_millis(10), Duration::from_secs(3));
        assert_eq!(output_control(false, 100, 200, s, idle), None, "prebuffering");
        assert_eq!(output_control(false, 200, 200, s, idle), Some(OutCtl::Start));
        assert_eq!(output_control(true, 500, 200, s, idle), None, "playing");
        assert_eq!(output_control(true, 0, 200, s, idle), None, "a short gap is not idle");
        assert_eq!(output_control(true, 0, 200, Duration::from_secs(4), idle), Some(OutCtl::Pause));
        assert_eq!(output_control(true, 1, 200, Duration::from_secs(4), idle), None, "still draining");
    }

    #[test]
    fn names_and_types() {
        assert_eq!(app_name_of_path("/Applications/TextEdit.app/").as_deref(), Some("TextEdit"));
        assert_eq!(app_name_of_path("/Applications/Utilities/Script Editor.app").as_deref(), Some("Script Editor"));
        assert_eq!(app_name_of_path("/"), None);
        let t = types();
        assert_eq!(t.iter().filter(|(_, u)| *u == "public.xml").count(), 2);
        assert!(t.contains(&("text/plain", "public.plain-text")));
    }
}
