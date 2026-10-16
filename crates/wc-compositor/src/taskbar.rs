//! Which taskbar button a window belongs to.
//!
//! # The defect this exists to fix
//!
//! Every window this compositor makes is an `HWND` created by **one process**.
//! The Windows shell groups taskbar buttons by process by default, so Firefox
//! and a terminal running through us collapse into a single button that belongs
//! to `wc-compositor.exe` — and because a *grouped* button takes the
//! executable's icon rather than the window's, that button is also the generic
//! one. §16 measured exactly that from the other side: both `HICON`s were set
//! successfully, the caption showed the right icon, and the taskbar button
//! showed the generic one for a whole run.
//!
//! The shell's real grouping key is not the process. It is the window's
//! **Application User Model ID** — a string set per `HWND` through
//! `SHGetPropertyStoreForWindow` and `PKEY_AppUserModel_ID`. Windows falls back
//! to a process-wide identity only when a window has none of its own, and that
//! fallback is the bug. So this module answers one question, in arithmetic that
//! can be tested on the machine that runs `cargo test`: **given what a client
//! has told us about itself, what string goes in that property?**
//!
//! The Win32 half — the property store, the ordering against `ShowWindow`, and
//! what happens when the answer changes after the button exists — is in
//! [`crate::platform`]'s `win32` module.
//!
//! # The identity is a function of the app id, not of the window
//!
//! `xdg_toplevel.set_app_id` is Wayland's notion of application identity, and
//! it is the same string for every window of one application: Firefox's three
//! windows all say `firefox`. Deriving the AUMID from it therefore gets both
//! halves of the requirement at once — two *different* programs get two
//! buttons, and two windows of the *same* program keep sharing one, which is
//! the behaviour a user expects and the easier one to break.
//!
//! Deriving it from the window id instead would give every window its own
//! button, which is not "fixed", it is a different bug.
//!
//! # A client that never sets one
//!
//! `set_app_id` is optional and real clients skip it — §16 measured
//! `weston-presentation-shm` sending none. Those windows cannot be told apart
//! by name, so the fallback is the next-coarsest thing that is still true: the
//! **connection**. `ClientState::index` is already counted for every client
//! that connects, so two anonymous clients get two buttons and two windows of
//! one anonymous client still share one. That is exactly the guarantee the
//! app-id path gives, one level down.
//!
//! The two namespaces are kept apart (`…app.` against `…unnamed.`) so that a
//! client *called* `unnamed.c3` cannot land on connection 3's button.
//!
//! # The rules Windows imposes, and the one it does not
//!
//! An AUMID is at most [`MAX_LEN`] characters and may not contain spaces —
//! documented, and Explorer's behaviour for a longer one is not defined
//! anywhere we could rely on. App ids are not so constrained: a desktop entry
//! name may be any UTF-8 at all. So a name has to be *mapped* into the smaller
//! alphabet, and mapping is where a fix like this quietly becomes the bug it
//! was fixing: `"my app"` and `"my_app"` both sanitise to `my_app`, and two
//! different programs are back on one button.
//!
//! [`identity`] therefore appends a hash of the **original** string whenever it
//! had to change anything. Two app ids that differ at all differ in the hash,
//! so the collapse is gone; an app id that needed no mapping — which is nearly
//! all of them, `firefox` and `org.freedesktop.weston.wayland-terminal`
//! included — keeps a legible identity with nothing appended.

/// Everything this compositor mints lives under one root.
///
/// It is not a decoration. An AUMID naming an application that is really
/// installed — `Microsoft.Windows.Explorer`, say — would put our window on
/// *that* application's button, inherit its jump list and its pinning, and a
/// Wayland client picks its own app id. Prefixing every one of them keeps a
/// hostile or merely unlucky `set_app_id` inside our own namespace.
pub const PREFIX: &str = "Waycompose.";

/// The segment under [`PREFIX`] for identities derived from a client's own name.
pub const NAMED: &str = "app.";

/// The segment under [`PREFIX`] for a client that never said what it is.
///
/// A namespace of its own, so that a client which calls itself `unnamed.c3`
/// cannot land on connection 3's button.
pub const UNNAMED: &str = "unnamed.";

/// The longest AUMID Windows documents as acceptable.
pub const MAX_LEN: usize = 128;

/// `_` plus sixteen hex digits.
const HASH_LEN: usize = 17;

/// How much of the app id itself survives, worst case.
///
/// The budget is the documented maximum minus the longest prefix and the hash
/// that a mapped name carries. A name that needs neither is not truncated to
/// it.
const BODY_MAX: usize = MAX_LEN - PREFIX.len() - NAMED.len() - HASH_LEN;

/// Which characters are carried through unchanged.
///
/// ASCII letters, digits, `.`, `-` and `_`. The dot is what makes an app id
/// like `org.freedesktop.weston.wayland-terminal` come out readable, and it is
/// also the separator the AUMID convention itself uses.
fn keep(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'
}

/// FNV-1a, 64-bit, over the original bytes.
///
/// Not a cryptographic choice and not pretending to be one: this is here so
/// that two app ids which sanitise to the same string do not end up on the same
/// taskbar button, and eight bytes of a decent avalanche is far more than that
/// needs. It is written out rather than pulled in so that the identity a
/// deployed binary produces does not depend on a `HashMap` seed or on anyone's
/// crate version.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The AUMID for a window, from the app id its client set and the connection it
/// arrived on.
///
/// `app_id` empty means the client has not called `set_app_id` — either it
/// never will, or it has not yet, which is the ordering problem the caller
/// handles.
pub fn identity(app_id: &str, client: u32) -> String {
    if app_id.is_empty() {
        return format!("{PREFIX}{UNNAMED}c{client}");
    }
    let mut body: String = app_id.chars().map(|c| if keep(c) { c } else { '_' }).collect();
    // Every kept character is ASCII and every replaced one became a single
    // ASCII byte, so from here bytes and characters are the same thing and
    // `truncate` cannot split anything.
    let mut mapped = body != app_id;
    if body.len() > BODY_MAX {
        body.truncate(BODY_MAX);
        mapped = true;
    }
    if mapped {
        body.push_str(&format!("_{:016x}", fnv1a(app_id.as_bytes())));
    }
    format!("{PREFIX}{NAMED}{body}")
}

// ---------------------------------------------------------------------------
// Relaunch, §92
// ---------------------------------------------------------------------------
//
// Until §92 every window carried `PKEY_AppUserModel_PreventPinning`, because
// a pin made from a window is a shortcut to whatever the shell can work out
// about it, and here that is `sv-guest.exe` rather than the Linux program the
// button stands for. A window can say what a pin should run instead
// (`PKEY_AppUserModel_RelaunchCommand`, with a display name and an icon), but
// the compositor does not know that command: the application was started on
// another machine by somebody else.
//
// The launcher does know it. It started the application and knows its own
// executable, the `[app.<id>]` section and the icon it wrote. So the launcher
// writes one small file per application into a directory it names with
// `--relaunch-dir`, keyed by the identity this module derives, and the
// compositor reads it when it stamps an identity on a window. A window with no
// file keeps `PreventPinning`, so a pin that would start the wrong program is
// still never offered.
//
// This file is compiled into `wc-launch` as well (`#[path]` in its `main.rs`),
// so the identity the launcher stamps on its shortcuts and jump list and the
// one the compositor stamps on its windows come from one function. If they
// came from two, a pinned button and its window could stop sharing a slot
// without any test failing.

/// The extension of a relaunch file: `<identity>.relaunch`.
pub const RELAUNCH_EXT: &str = "relaunch";

/// What a pin made from a window of one application should run, and wear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relaunch {
    /// The whole command line, `"C:\…\wc-launch.exe" firefox`:
    /// `PKEY_AppUserModel_RelaunchCommand`.
    pub command: String,
    /// What the pin is called: `PKEY_AppUserModel_RelaunchDisplayNameResource`.
    /// Windows ignores a relaunch command without one.
    pub name: String,
    /// `path,index` of the pin's icon: `PKEY_AppUserModel_RelaunchIconResource`.
    /// May be empty, and the pin then wears the target's icon.
    pub icon: String,
}

impl Relaunch {
    /// One `key=value` per line. Anything else is ignored, so a later
    /// launcher may add keys an older compositor does not read.
    ///
    /// `None` unless both the command and the name are there: a relaunch
    /// command without a display name is one Windows ignores, and the window
    /// then pins as `sv-guest.exe`, which is exactly the pin this exists to
    /// prevent.
    pub fn parse(text: &str) -> Option<Relaunch> {
        let mut r = Relaunch { command: String::new(), name: String::new(), icon: String::new() };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "command" => r.command = v.trim().to_string(),
                "name" => r.name = v.trim().to_string(),
                "icon" => r.icon = v.trim().to_string(),
                _ => {}
            }
        }
        (!r.command.is_empty() && !r.name.is_empty()).then_some(r)
    }

    /// The file's text. A value cannot contain a line break, so one that does
    /// is cut at it rather than allowed to start a second key.
    ///
    /// Written by `wc-launch`, which compiles this file too; the compositor only
    /// reads.
    #[allow(dead_code)]
    pub fn render(&self) -> String {
        let one = |s: &str| s.lines().next().unwrap_or("").trim().to_string();
        format!(
            "command={}\nname={}\nicon={}\n",
            one(&self.command),
            one(&self.name),
            one(&self.icon)
        )
    }
}

/// The relaunch file's name for an identity, or `None` for an identity this
/// module could not have produced.
///
/// An identity is ASCII from [`keep`]'s alphabet under [`PREFIX`] by
/// construction, which is also a safe file name; the check is here so that
/// the rule is stated where the path is built rather than inherited from
/// another function.
pub fn relaunch_file_name(aumid: &str) -> Option<String> {
    let ok = aumid.starts_with(PREFIX)
        && aumid.len() <= MAX_LEN
        && aumid.chars().all(keep);
    ok.then(|| format!("{aumid}.{RELAUNCH_EXT}"))
}

static RELAUNCH_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// `--relaunch-dir`: where the launcher keeps the files above. Set once, at
/// argument parsing; a second call is ignored.
pub fn set_relaunch_dir(dir: std::path::PathBuf) {
    let _ = RELAUNCH_DIR.set(dir);
}

/// The relaunch details for a window's identity, if a launcher left any.
///
/// Read at each stamp rather than cached, so a launcher that refreshes the
/// file between two windows is seen by the second. It is a few hundred bytes,
/// read when a window is created or renamed.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn relaunch_for(aumid: &str) -> Option<Relaunch> {
    let dir = RELAUNCH_DIR.get()?;
    let name = relaunch_file_name(aumid)?;
    Relaunch::parse(&std::fs::read_to_string(dir.join(name)).ok()?)
}

#[cfg(test)]
mod relaunch_tests {
    use super::*;

    #[test]
    fn a_relaunch_file_round_trips() {
        let r = Relaunch {
            command: r#""C:\Users\u\wcivs\wc-launch.exe" firefox"#.into(),
            name: "Firefox (Linux)".into(),
            icon: r"C:\Users\u\wcivs\firefox.ico,0".into(),
        };
        assert_eq!(Relaunch::parse(&r.render()), Some(r));
    }

    /// Windows ignores a relaunch command with no display name, and the pin
    /// would then be `sv-guest.exe`: no file is better than half of one.
    #[test]
    fn a_relaunch_without_a_name_or_command_is_none() {
        assert_eq!(Relaunch::parse("command=x.exe\nicon=a.ico,0\n"), None);
        assert_eq!(Relaunch::parse("name=X\n"), None);
        assert_eq!(Relaunch::parse(""), None);
    }

    #[test]
    fn a_line_break_cannot_add_a_key() {
        let r = Relaunch { command: "a.exe\nname=evil".into(), name: "N".into(), icon: String::new() };
        let back = Relaunch::parse(&r.render()).unwrap();
        assert_eq!(back.command, "a.exe");
        assert_eq!(back.name, "N");
    }

    #[test]
    fn only_our_identities_name_a_file() {
        assert_eq!(
            relaunch_file_name(&identity("firefox", 1)).as_deref(),
            Some("Waycompose.app.firefox.relaunch")
        );
        assert!(relaunch_file_name(&identity("", 4)).is_some());
        assert_eq!(relaunch_file_name("Microsoft.Windows.Explorer"), None);
        assert_eq!(relaunch_file_name("Waycompose.app/../x"), None);
        // Whatever a client calls itself, the file name stays in the directory.
        for c in ["../../evil", "a\\b", "C:x", "☃"] {
            let f = relaunch_file_name(&identity(c, 1)).unwrap();
            assert!(!f.contains(['/', '\\', ':']), "{f}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The requirement, in one test: two different programs, two identities.
    #[test]
    fn two_different_apps_do_not_share_an_identity() {
        assert_ne!(
            identity("firefox", 1),
            identity("org.freedesktop.weston.wayland-terminal", 2)
        );
    }

    /// And the half that is easy to break while fixing the other: two windows
    /// of one application, on one connection or on two, keep one identity.
    #[test]
    fn two_windows_of_one_app_share_an_identity() {
        assert_eq!(identity("firefox", 1), identity("firefox", 1));
        assert_eq!(identity("firefox", 1), identity("firefox", 7));
    }

    #[test]
    fn a_legible_app_id_is_carried_through_legibly() {
        assert_eq!(identity("firefox", 1), "Waycompose.app.firefox");
        assert_eq!(
            identity("org.freedesktop.weston.wayland-terminal", 1),
            "Waycompose.app.org.freedesktop.weston.wayland-terminal"
        );
    }

    /// A client that never said what it is gets one identity per *connection*,
    /// so two anonymous programs are still two buttons.
    #[test]
    fn anonymous_clients_are_told_apart_by_connection() {
        assert_ne!(identity("", 1), identity("", 2));
        assert_eq!(identity("", 3), identity("", 3));
        assert_eq!(identity("", 3), "Waycompose.unnamed.c3");
    }

    /// The collision the sanitiser would otherwise introduce. `"my app"` and
    /// `"my_app"` are different programs and must stay on different buttons.
    #[test]
    fn sanitising_does_not_merge_two_different_names() {
        let space = identity("my app", 1);
        let under = identity("my_app", 1);
        assert_ne!(space, under);
        assert!(!space.contains(' '), "an AUMID may not contain a space: {space}");
        // The one that needed no mapping carries no hash.
        assert_eq!(under, "Waycompose.app.my_app");
    }

    /// Non-ASCII is a name too. It must map, and it must stay distinct.
    #[test]
    fn a_non_ascii_app_id_maps_and_stays_distinct() {
        let a = identity("café", 1);
        let b = identity("cafe_", 1);
        assert_ne!(a, b);
        assert!(a.is_ascii(), "{a}");
    }

    /// Two long app ids that agree for the first hundred characters are two
    /// programs, and truncation alone would merge them.
    #[test]
    fn truncation_does_not_merge_two_long_names() {
        let stem = "a".repeat(200);
        let a = identity(&format!("{stem}one"), 1);
        let b = identity(&format!("{stem}two"), 1);
        assert_ne!(a, b);
        assert!(a.len() <= MAX_LEN, "{} chars", a.len());
        assert!(b.len() <= MAX_LEN, "{} chars", b.len());
    }

    /// Windows' own two limits, over everything above.
    #[test]
    fn every_identity_fits_what_windows_documents() {
        let cases = [
            String::new(),
            "firefox".into(),
            "org.freedesktop.weston.wayland-terminal".into(),
            "a name with spaces and /slashes/ and \\backslashes\\".into(),
            "☃".repeat(60),
            "x".repeat(500),
        ];
        for c in cases {
            let id = identity(&c, 4);
            assert!(id.len() <= MAX_LEN, "{} chars for {c:?}", id.len());
            assert!(!id.contains(' '), "space in {id:?}");
            assert!(id.is_ascii(), "non-ascii in {id:?}");
            assert!(id.starts_with(PREFIX), "{id:?} escaped the namespace");
        }
    }

    /// A client cannot name itself onto another connection's button, and cannot
    /// name itself onto an application that is really installed.
    #[test]
    fn the_namespaces_cannot_be_forged() {
        assert_ne!(identity("unnamed.c3", 9), identity("", 3));
        let id = identity("Microsoft.Windows.Explorer", 1);
        assert!(id.starts_with(&format!("{PREFIX}{NAMED}")), "{id}");
    }
}
