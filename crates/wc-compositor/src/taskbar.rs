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
//!
//! # One identity per installation, not per application (§92.12)
//!
//! [`identity`] is a function of the app id alone, so every installation that
//! runs Firefox put its windows, shortcuts, pins and jump list under
//! `Waycompose.app.firefox`. A jump list is keyed by that string and the last
//! writer wins, so a test installation's run replaced the user's list, and
//! the windows of two installations shared one button and relaunched one of
//! them. [`installed_identity`] puts the installation's name in the string:
//! `Waycompose.install.<install>.<app id>`. A [`Scope`] says, per
//! installation, which of its applications keep the old form because the
//! user's existing pin carries it; deploy decides that once and the
//! installation's `wc-launch.ini` keeps it.

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
// One identity per installation, §92.12
// ---------------------------------------------------------------------------

/// The segment under [`PREFIX`] for an identity scoped to one installation:
/// `Waycompose.install.<install>.<app id>`.
///
/// A segment of its own, like [`NAMED`] and [`UNNAMED`], so that an
/// installation called `app` cannot produce `Waycompose.app.firefox`, and a
/// client calling itself `install.x.y` stays under `Waycompose.app.`.
pub const INSTALLED: &str = "install.";

/// The longest installation name the session service accepts
/// (`wc_handle::macboot::valid_installation`).
const INSTALL_MAX: usize = 32;

/// The characters an installation-scoped identity carries through unchanged:
/// lower-case ASCII letters, digits, `.`, `-`, `_`.
///
/// Lower case only. Windows does not document whether it compares AUMIDs with
/// regard to case, so the scoped form never relies on case to tell two apart:
/// an upper-case letter is lowered and the name counts as mapped, which
/// appends the hash of the original.
fn keep_lower(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' || c == '_'
}

/// `name` in `keep`'s alphabet, cut to `max`, and whether anything changed.
fn map_name(name: &str, max: usize, keep: fn(char) -> bool) -> (String, bool) {
    let mut s: String = name
        .chars()
        .map(|c| if keep(c) { c } else if c.is_ascii_uppercase() && keep(c.to_ascii_lowercase()) { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let mut mapped = s != name;
    if s.len() > max {
        s.truncate(max);
        mapped = true;
    }
    (s, mapped)
}

/// The installation's segment: its name in lower case, with eight hex digits
/// of the original's hash when that changed it. Never contains a dot, so the
/// segment after it is unambiguously the application.
pub fn install_segment(install: &str) -> String {
    let (s, mapped) = map_name(install, INSTALL_MAX, |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    let s = if s.is_empty() { "_".to_string() } else { s };
    if mapped { format!("{s}-{:08x}", fnv1a(install.as_bytes()) as u32) } else { s }
}

/// The identity of `app_id`'s windows in installation `install`:
/// `Waycompose.install.wcivs.firefox`.
///
/// A client that never named itself gets the same [`UNNAMED`] identity as
/// everywhere: the connection number is unique in the compositor, so it is
/// already one installation's, and such a window cannot be pinned.
pub fn installed_identity(install: &str, app_id: &str, client: u32) -> String {
    if app_id.is_empty() {
        return format!("{PREFIX}{UNNAMED}c{client}");
    }
    let head = format!("{PREFIX}{INSTALLED}{}.", install_segment(install));
    let (mut body, mapped) = map_name(app_id, MAX_LEN - head.len() - HASH_LEN, keep_lower);
    if mapped {
        body.push_str(&format!("_{:016x}", fnv1a(app_id.as_bytes())));
    }
    format!("{head}{body}")
}

/// Whether `aumid` is an application's identity in either form, which is what
/// a pin, a jump list or a window's successor can be keyed by.
pub fn is_named(aumid: &str) -> bool {
    aumid.starts_with(&format!("{PREFIX}{NAMED}")) || aumid.starts_with(&format!("{PREFIX}{INSTALLED}"))
}

/// An application id as `[app.*] app-id` and the desktop id allow it.
pub fn app_id_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// How one installation's windows are named on the taskbar: its name, and the
/// app ids whose windows keep the application-wide identity because the
/// installation is the one whose pin already carries it (§92.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub install: String,
    pub legacy: Vec<String>,
}

impl Scope {
    /// `INSTALL` or `INSTALL:APP[,APP…]`, the boot line's `taskbar=` and the
    /// compositor's `--taskbar-scope`.
    pub fn parse(spec: &str) -> Result<Scope, String> {
        let (install, legacy) = match spec.split_once(':') {
            Some((i, l)) => (i, l.split(',').map(str::to_string).collect::<Vec<_>>()),
            None => (spec, Vec::new()),
        };
        if install.is_empty()
            || install.len() > INSTALL_MAX
            || !install.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(format!("taskbar scope {spec:?}: {install:?} is not an installation name"));
        }
        if let Some(bad) = legacy.iter().find(|a| !app_id_ok(a)) {
            return Err(format!("taskbar scope {spec:?}: {bad:?} is not an app id"));
        }
        Ok(Scope { install: install.to_string(), legacy })
    }

    /// The other half of [`Scope::parse`].
    pub fn render(&self) -> String {
        if self.legacy.is_empty() { self.install.clone() } else { format!("{}:{}", self.install, self.legacy.join(",")) }
    }

    /// The identity of `app_id`'s windows in this installation.
    pub fn identity(&self, app_id: &str, client: u32) -> String {
        if !app_id.is_empty() && self.legacy.iter().any(|a| a == app_id) {
            identity(app_id, client)
        } else {
            installed_identity(&self.install, app_id, client)
        }
    }

    /// Whether `aumid` names one of this installation's applications.
    pub fn owns(&self, aumid: &str) -> bool {
        let head = format!("{PREFIX}{INSTALLED}{}.", install_segment(&self.install));
        aumid.starts_with(&head) || self.legacy.iter().any(|a| identity(a, 0) == aumid)
    }
}

/// A window's identity: in its installation's scope, or the application-wide
/// form for a client of no known installation (a socket client, a session
/// whose launcher predates §92.12).
pub fn scoped_identity(scope: Option<&Scope>, app_id: &str, client: u32) -> String {
    match scope {
        Some(s) => s.identity(app_id, client),
        None => identity(app_id, client),
    }
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

/// One session's installation, as the compositor knows it (§107, §92.12).
#[derive(Debug, Clone)]
struct Entry {
    session: u8,
    scope: Option<Scope>,
    relaunch: Option<std::path::PathBuf>,
}

/// Which installation each session is, and where each keeps its relaunch
/// files. One per compositor process; a value of its own so the tests can
/// make as many as they like.
#[derive(Debug, Default)]
pub struct Registry {
    /// `--taskbar-scope`: the one-session compositor's installation, for the
    /// clients of session 0.
    default: Option<Scope>,
    /// `--relaunch-dir`: the one-session compositor's directory.
    dirs: Vec<std::path::PathBuf>,
    sessions: Vec<Entry>,
}

impl Registry {
    /// `--relaunch-dir`.
    pub fn set_relaunch_dir(&mut self, dir: std::path::PathBuf) {
        if !self.dirs.contains(&dir) {
            self.dirs.push(dir);
        }
    }

    /// `--taskbar-scope`.
    pub fn set_default(&mut self, scope: Scope) {
        self.default = Some(scope);
    }

    /// A session came up. Returns the legacy app ids it was **not** given
    /// because another live session already holds them: two installations
    /// must never carry one identity, and deploy's claim (§92.12) is what
    /// should have prevented it, so the compositor says so and keeps the
    /// first.
    pub fn add_session(&mut self, session: u8, scope: Option<Scope>, relaunch: Option<std::path::PathBuf>) -> Vec<String> {
        self.sessions.retain(|e| e.session != session);
        let mut refused = Vec::new();
        let scope = scope.map(|mut s| {
            let held = |a: &String| {
                self.sessions.iter().filter_map(|e| e.scope.as_ref()).chain(self.default.iter()).any(|o| o.install != s.install && o.legacy.contains(a))
            };
            refused = s.legacy.iter().filter(|a| held(a)).cloned().collect();
            s.legacy.retain(|a| !refused.contains(a));
            s
        });
        self.sessions.push(Entry { session, scope, relaunch });
        refused
    }

    /// The session ended.
    pub fn remove_session(&mut self, session: u8) {
        self.sessions.retain(|e| e.session != session);
    }

    fn scope_of(&self, session: u8) -> Option<&Scope> {
        let own = self.sessions.iter().find(|e| e.session == session).and_then(|e| e.scope.as_ref());
        own.or(if session == 0 { self.default.as_ref() } else { None })
    }

    /// The identity of a window of `app_id`, from connection `client` of
    /// session `session`.
    pub fn identity(&self, app_id: &str, client: u32, session: u8) -> String {
        scoped_identity(self.scope_of(session), app_id, client)
    }

    /// The directories a window of `aumid` may find its relaunch file in: the
    /// installation's that owns the identity, and no other. Another
    /// installation's directory may still hold a file of the same name from
    /// before §92.12, and its command would start the wrong installation. An
    /// identity no scope owns (a launcher before §92.12) is looked up in the
    /// directories of the sessions without a scope, as before.
    pub fn relaunch_dirs(&self, aumid: &str) -> Vec<std::path::PathBuf> {
        let mut owned: Vec<std::path::PathBuf> = Vec::new();
        let mut found = false;
        for e in &self.sessions {
            if e.scope.as_ref().is_some_and(|s| s.owns(aumid)) {
                found = true;
                match &e.relaunch {
                    Some(d) => owned.push(d.clone()),
                    None => owned.extend(self.dirs.iter().cloned()),
                }
            }
        }
        if self.default.as_ref().is_some_and(|s| s.owns(aumid)) {
            found = true;
            owned.extend(self.dirs.iter().cloned());
        }
        if !found {
            owned = self.sessions.iter().filter(|e| e.scope.is_none()).filter_map(|e| e.relaunch.clone()).collect();
            owned.extend(self.dirs.iter().cloned());
        }
        let mut out = Vec::new();
        for d in owned {
            if !out.contains(&d) {
                out.push(d);
            }
        }
        out
    }

    /// The relaunch details for a window's identity, if its launcher left any.
    pub fn relaunch_for(&self, aumid: &str) -> Option<Relaunch> {
        let name = relaunch_file_name(aumid)?;
        self.relaunch_dirs(aumid).iter().find_map(|d| Relaunch::parse(&std::fs::read_to_string(d.join(&name)).ok()?))
    }
}

/// The process's registry.
static REGISTRY: std::sync::Mutex<Registry> =
    std::sync::Mutex::new(Registry { default: None, dirs: Vec::new(), sessions: Vec::new() });

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// `--relaunch-dir`: where the launcher keeps the files above.
pub fn set_relaunch_dir(dir: std::path::PathBuf) {
    registry().set_relaunch_dir(dir);
}

/// `--taskbar-scope`: the one-session compositor's installation (§92.12).
pub fn set_default_scope(scope: Scope) {
    registry().set_default(scope);
}

/// §107, §92.12: a session's installation and relaunch directory, for as
/// long as it runs. See [`Registry::add_session`] for what it returns.
pub fn add_session(session: u8, scope: Option<Scope>, relaunch: Option<std::path::PathBuf>) -> Vec<String> {
    registry().add_session(session, scope, relaunch)
}

/// §107: the installation's session ended.
pub fn remove_session(session: u8) {
    registry().remove_session(session);
}

/// The identity of a window of `app_id` from connection `client` of session
/// `session`: what the compositor stamps on it.
pub fn identity_in(app_id: &str, client: u32, session: u8) -> String {
    registry().identity(app_id, client, session)
}

/// The relaunch details for a window's identity, if a launcher left any.
///
/// Read at each stamp rather than cached, so a launcher that refreshes the
/// file between two windows is seen by the second. It is a few hundred bytes,
/// read when a window is created or renamed.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn relaunch_for(aumid: &str) -> Option<Relaunch> {
    registry().relaunch_for(aumid)
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
mod install_tests {
    use super::*;

    #[test]
    fn an_installation_s_identity_is_legible_and_its_own() {
        assert_eq!(installed_identity("wcivs", "firefox", 1), "Waycompose.install.wcivs.firefox");
        assert_eq!(installed_identity("wclinux", "atp-nested", 1), "Waycompose.install.wclinux.atp-nested");
        // The bug: two installations of one application, one identity.
        assert_ne!(installed_identity("wclinux", "atp-nested", 1), installed_identity("wcnossh", "atp-nested", 1));
        // And neither is the application-wide one.
        assert_ne!(installed_identity("app", "firefox", 1), identity("firefox", 1));
        // Two windows of one application in one installation share it.
        assert_eq!(installed_identity("wcivs", "firefox", 1), installed_identity("wcivs", "firefox", 9));
    }

    /// Case is never what tells two identities apart, and every mapped name
    /// carries the hash of its original.
    #[test]
    fn names_are_lowered_and_mapped_names_stay_distinct() {
        let a = installed_identity("WCivs", "firefox", 1);
        let b = installed_identity("wcivs", "firefox", 1);
        assert_ne!(a, b);
        assert!(a.starts_with("Waycompose.install.wcivs-"), "{a}");
        let up = installed_identity("wcivs", "org.gnome.Nautilus", 1);
        let low = installed_identity("wcivs", "org.gnome.nautilus", 1);
        assert_ne!(up, low);
        for id in [&a, &up] {
            let body = id.strip_prefix(PREFIX).unwrap();
            assert!(!body.chars().any(|c| c.is_ascii_uppercase()), "{id}");
        }
        assert_ne!(installed_identity("w", "my app", 1), installed_identity("w", "my_app", 1));
        assert_ne!(installed_identity("a_b", "x", 1), installed_identity("a-b", "x", 1));
    }

    #[test]
    fn every_scoped_identity_fits_what_windows_documents() {
        let installs = ["wcivs", "a", &"x".repeat(32), &"Y".repeat(40), "a b/c", ""];
        let apps = ["firefox", "org.freedesktop.weston.wayland-terminal", &"z".repeat(500), &"☃".repeat(80), "a b"];
        let mut seen = std::collections::BTreeSet::new();
        for i in installs {
            for a in apps {
                let id = installed_identity(i, a, 3);
                assert!(id.len() <= MAX_LEN, "{} chars: {id}", id.len());
                assert!(!id.contains(' ') && id.is_ascii(), "{id:?}");
                assert!(id.starts_with("Waycompose.install."), "{id}");
                assert!(relaunch_file_name(&id).is_some(), "{id}");
                assert!(is_named(&id));
                assert!(seen.insert(id.clone()), "two (install, app) pairs gave {id}");
            }
        }
    }

    /// The installation segment has no dot, so the next segment is the app.
    #[test]
    fn the_install_segment_has_no_dot() {
        for i in ["a.b", "wcivs", "Ä", "..", "x.y.z"] {
            assert!(!install_segment(i).contains('.'), "{i}");
        }
        assert_ne!(install_segment("a.b"), install_segment("a_b"));
    }

    #[test]
    fn a_scope_round_trips_and_refuses_what_is_not_a_name() {
        let s = Scope::parse("wcivs:firefox,org.x.Y").unwrap();
        assert_eq!(s, Scope { install: "wcivs".into(), legacy: vec!["firefox".into(), "org.x.Y".into()] });
        assert_eq!(Scope::parse(&s.render()).unwrap(), s);
        assert_eq!(Scope::parse("wcpins").unwrap().legacy, Vec::<String>::new());
        assert_eq!(Scope::parse("wcpins").unwrap().render(), "wcpins");
        for bad in ["", ":firefox", "a b", "a.b", &"x".repeat(33), "w:", "w:a,,b", "w:a b", "w:a\tb"] {
            assert!(Scope::parse(bad).is_err(), "{bad:?}");
        }
    }

    /// The primary installation keeps the application-wide identity for the
    /// applications deploy found pinned; everything else is its own.
    #[test]
    fn a_scope_keeps_the_legacy_identity_only_where_it_says() {
        let s = Scope::parse("wcivs:firefox").unwrap();
        assert_eq!(s.identity("firefox", 1), "Waycompose.app.firefox");
        assert_eq!(s.identity("thunderbird", 1), "Waycompose.install.wcivs.thunderbird");
        assert_eq!(s.identity("", 4), "Waycompose.unnamed.c4");
        assert!(s.owns("Waycompose.app.firefox"));
        assert!(s.owns("Waycompose.install.wcivs.thunderbird"));
        assert!(!s.owns("Waycompose.app.thunderbird"));
        assert!(!s.owns("Waycompose.install.wcivs2.firefox"));
        assert_eq!(scoped_identity(None, "firefox", 1), identity("firefox", 1));
    }

    /// §107 + §92.12: one compositor, three installations, one application.
    #[test]
    fn a_shared_compositor_names_each_session_s_windows_by_its_installation() {
        let mut r = Registry::default();
        assert!(r.add_session(1, Some(Scope::parse("wcivs:firefox").unwrap()), None).is_empty());
        assert!(r.add_session(2, Some(Scope::parse("wcpins").unwrap()), None).is_empty());
        r.add_session(3, None, None);
        assert_eq!(r.identity("firefox", 10, 1), "Waycompose.app.firefox");
        assert_eq!(r.identity("firefox", 11, 2), "Waycompose.install.wcpins.firefox");
        // A launcher from before §92.12 sends no scope: the old identity.
        assert_eq!(r.identity("firefox", 12, 3), "Waycompose.app.firefox");
        // A socket client, and a session that never booted.
        assert_eq!(r.identity("firefox", 13, 0), "Waycompose.app.firefox");
        assert_eq!(r.identity("firefox", 14, 9), "Waycompose.app.firefox");
        r.remove_session(2);
        assert_eq!(r.identity("firefox", 11, 2), "Waycompose.app.firefox");
    }

    /// Two installations may never end up with one identity, even if deploy's
    /// claim was bypassed: the second's legacy claim is refused.
    #[test]
    fn a_second_legacy_claim_is_refused() {
        let mut r = Registry::default();
        r.add_session(1, Some(Scope::parse("wcivs:firefox").unwrap()), None);
        let refused = r.add_session(2, Some(Scope::parse("wctest:firefox,atp-nested").unwrap()), None);
        assert_eq!(refused, ["firefox"]);
        assert_eq!(r.identity("firefox", 1, 2), "Waycompose.install.wctest.firefox");
        assert_eq!(r.identity("atp-nested", 1, 2), "Waycompose.app.atp-nested");
        // The same installation booting again is not a second claim.
        assert!(r.add_session(3, Some(Scope::parse("wcivs:firefox").unwrap()), None).is_empty());
    }

    /// The one-session compositor: `--taskbar-scope` names session 0.
    #[test]
    fn the_one_session_compositor_uses_its_default_scope() {
        let mut r = Registry::default();
        r.set_default(Scope::parse("wclinux").unwrap());
        r.add_session(0, None, None);
        assert_eq!(r.identity("atp-nested", 1, 0), "Waycompose.install.wclinux.atp-nested");
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wc-taskbar-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(d: &std::path::Path, aumid: &str, command: &str) {
        let r = Relaunch { command: command.into(), name: "N".into(), icon: String::new() };
        std::fs::write(d.join(relaunch_file_name(aumid).unwrap()), r.render()).unwrap();
    }

    /// The relaunch file is looked up in the installation that owns the
    /// identity, so a stale file of the same name in another installation's
    /// directory never gives a window the wrong command.
    #[test]
    fn a_relaunch_file_is_found_in_its_own_installation_only() {
        let (ivs, pins, old) = (dir("ivs"), dir("pins"), dir("old"));
        // wcpins wrote the application-wide file before §92.12.
        write(&pins, "Waycompose.app.firefox", "pins.exe firefox");
        write(&pins, "Waycompose.install.wcpins.firefox", "pins.exe firefox (own)");
        write(&ivs, "Waycompose.app.firefox", "ivs.exe firefox");
        write(&old, "Waycompose.app.minecraft", "old.exe minecraft");
        let mut r = Registry::default();
        // wcpins booted first, so it is first in boot order.
        r.add_session(1, Some(Scope::parse("wcpins").unwrap()), Some(pins.clone()));
        r.add_session(2, Some(Scope::parse("wcivs:firefox").unwrap()), Some(ivs.clone()));
        r.add_session(3, None, Some(old.clone()));
        assert_eq!(r.relaunch_for("Waycompose.app.firefox").unwrap().command, "ivs.exe firefox");
        assert_eq!(r.relaunch_for("Waycompose.install.wcpins.firefox").unwrap().command, "pins.exe firefox (own)");
        assert_eq!(r.relaunch_dirs("Waycompose.install.wcpins.firefox"), [pins.as_path()]);
        // An unscoped launcher's identity: the unscoped sessions' directories.
        assert_eq!(r.relaunch_for("Waycompose.app.minecraft").unwrap().command, "old.exe minecraft");
        assert_eq!(r.relaunch_dirs("Waycompose.app.minecraft"), [old.as_path()]);
        // An installation without its file: nothing, not another's.
        assert_eq!(r.relaunch_for("Waycompose.install.wcivs.thunderbird"), None);
        r.remove_session(2);
        // With wcivs gone, nobody owns the legacy id; the stale wcpins file is
        // still not offered, because wcpins is scoped.
        assert_eq!(r.relaunch_for("Waycompose.app.firefox"), None);
        for d in [ivs, pins, old] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// One session, `--relaunch-dir` and `--taskbar-scope` on the command line.
    #[test]
    fn the_one_session_compositor_finds_its_file_in_relaunch_dir() {
        let d = dir("one");
        write(&d, "Waycompose.install.wclinux.atp-nested", "wclinux.exe atp-nested");
        let mut r = Registry::default();
        r.set_relaunch_dir(d.clone());
        r.set_default(Scope::parse("wclinux").unwrap());
        r.add_session(0, None, None);
        let id = r.identity("atp-nested", 1, 0);
        assert_eq!(r.relaunch_for(&id).unwrap().command, "wclinux.exe atp-nested");
        let _ = std::fs::remove_dir_all(d);
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
