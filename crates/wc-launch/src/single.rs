//! §102.14. An application that may have only one window (`single = on`):
//! a click finds its window and brings it to the front, a click during a
//! launch waits for that launch, and a launch that fails says so.
//!
//! On 2026-10-19 the user clicked "Windows 7 (Linux)" six times within a
//! second because nothing appeared at once; each click was a `run` on the VM
//! host, six seats started and tore each other down, and the user got nothing.
//! The host now refuses a second nested session; this side stops asking.
//!
//! Everything here is the decision, as pure functions over what the Win32 half
//! (`main.rs`'s `win` module) observed. The Win32 half only enumerates
//! windows, waits on a mutex and calls `SetForegroundWindow`.

use std::time::Duration;

/// How long a launch holds the launch lock: from the click until its window
/// appears or this passes. The real guest shows the login screen in about
/// 4.4 s cold (§102.1); this is the bound for a slow one.
pub const LAUNCH_BOUND: Duration = Duration::from_secs(90);

/// How long a second click waits for the first one's window. A little longer
/// than [`LAUNCH_BOUND`], so that the first launch gives up (and releases the
/// lock) before the second one does.
pub const WAIT_BOUND: Duration = Duration::from_secs(95);

/// What the Win32 half read off one top-level window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub pid: u32,
    pub visible: bool,
    /// Owned windows are dialogs and popups, not the application's window.
    pub owned: bool,
    /// `PKEY_AppUserModel_ID`; `None` when the shell would not say (Wine's
    /// has no property store) or the window has none.
    pub aumid: Option<String>,
    pub title: String,
}

/// How a window was recognised, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Identity,
    Title,
}

/// Is `w` this application's window in this installation's session?
///
/// It must belong to the session's compositor (`comp_pid`, from the state
/// file; 0 is "no session" and matches nothing), be visible (the compositor
/// creates a window hidden and shows it on map; a minimised one is still
/// visible) and unowned. Then its identity decides; only a window whose
/// identity cannot be read is matched by its exact title.
pub fn matches(w: &Window, comp_pid: u32, aumid: &str, title: &str) -> Option<By> {
    if comp_pid == 0 || w.pid != comp_pid || !w.visible || w.owned {
        return None;
    }
    match w.aumid.as_deref() {
        Some(a) if !a.is_empty() => (a == aumid).then_some(By::Identity),
        _ => (!title.is_empty() && w.title == title).then_some(By::Title),
    }
}

/// The first of `ws` that is the application's window.
pub fn find(ws: &[Window], comp_pid: u32, aumid: &str, title: &str) -> Option<(usize, By)> {
    ws.iter().enumerate().find_map(|(i, w)| matches(w, comp_pid, aumid, title).map(|b| (i, b)))
}

/// Whose windows this installation's are: its own compositor's (§105), or
/// with none of its own (`comp_pid` 0) the user's shared one (§107). 0 is
/// "no session", which matches nothing.
pub fn window_pid(comp_pid: u32, shared_pid: u32) -> u32 {
    if comp_pid != 0 { comp_pid } else { shared_pid }
}

/// The launch lock's name: `Local\` (per logon session), per installation
/// session name and application. A kernel object name may not contain `\`
/// after the namespace, so anything outside a safe alphabet becomes `_`.
pub fn lock_name(session: &str, app: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect()
    };
    format!("Local\\waycompose-single-{}-{}", clean(session), clean(app))
}

/// What trying the launch lock found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    /// Ours now: nobody was launching.
    Taken,
    /// Ours now, because the last holder exited without releasing it: a
    /// launcher that crashed mid-launch. Its launch is not running any more.
    Abandoned,
    /// Another click is launching.
    Held,
}

impl Lock {
    /// `WaitForSingleObject`'s answer: `WAIT_OBJECT_0`, `WAIT_ABANDONED`,
    /// anything else (`WAIT_TIMEOUT`, `WAIT_FAILED`) is somebody else's.
    pub fn from_wait(rc: u32) -> Lock {
        match rc {
            0 => Lock::Taken,
            0x80 => Lock::Abandoned,
            _ => Lock::Held,
        }
    }
    pub fn ours(self) -> bool {
        self != Lock::Held
    }
}

/// What a click on a single application does first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum First {
    /// A window exists: bring it to the front, send nothing to the host.
    Focus,
    /// No window and nobody launching: this click launches, holding the lock.
    Launch,
    /// Another click is launching: wait for its window.
    Wait,
}

/// The window is looked for before the lock is tried, so a window that
/// exists is always focused rather than launched again.
pub fn first(window: bool, lock: impl FnOnce() -> Lock) -> First {
    if window {
        return First::Focus;
    }
    if lock().ours() { First::Launch } else { First::Wait }
}

/// One turn of a second click's wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiting {
    Focus,
    /// The first launch let go of the lock and there is no window: it failed,
    /// and it has told the user. This click goes quietly.
    FirstFailed,
    /// Neither a window nor a release within [`WAIT_BOUND`]: go quietly.
    GaveUp,
    Again,
}

/// `window` is looked at after `released`, so a launch that releases the lock
/// the moment its window appears is a `Focus`, not a `FirstFailed`.
pub fn waiting(window: bool, released: bool, waited: Duration) -> Waiting {
    if window {
        Waiting::Focus
    } else if released {
        Waiting::FirstFailed
    } else if waited >= WAIT_BOUND {
        Waiting::GaveUp
    } else {
        Waiting::Again
    }
}

/// One turn of the launching click's wait for its own window, after the
/// host accepted the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launching {
    Up,
    NoWindow,
    Again,
}

pub fn launching(window: bool, held: Duration) -> Launching {
    if window {
        Launching::Up
    } else if held >= LAUNCH_BOUND {
        Launching::NoWindow
    } else {
        Launching::Again
    }
}

/// Why a launch of a single application failed, as the user is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The session (compositor, arena, proxy) could not start.
    Session(String),
    /// The host refused the run.
    Refused(String),
    /// The run was accepted and no window came within the bound.
    NoWindow,
}

/// The user's language, from `GetUserPreferredUILanguages` as
/// `appmeta::lang_candidates` splits it: German, else English, as the jump
/// list's labels (`appmeta::mozilla_label`).
pub fn german(langs: &[String]) -> bool {
    langs.iter().any(|l| l == "de" || l.starts_with("de-") || l.starts_with("de_"))
}

/// The host's or the session's reason, one line and short, for a dialog.
fn reason(s: &str) -> String {
    let l = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut out: String = l.chars().take(200).collect();
    if l.chars().count() > 200 {
        out.push('…');
    }
    out
}

/// The host's lock (§102.14): another nested session is running.
fn already_running(s: &str) -> bool {
    s.contains("already running")
}

/// The dialog's text. Its caption is the application's title.
pub fn message(f: &Failure, title: &str, de: bool) -> String {
    let secs = LAUNCH_BOUND.as_secs();
    match (f, de) {
        (Failure::Refused(r), true) if already_running(r) => format!(
            "{title} läuft bereits oder wird gerade beendet.\n\nBitte warten Sie einen Moment und versuchen Sie es dann erneut."
        ),
        (Failure::Refused(r), false) if already_running(r) => format!(
            "{title} is already running or is shutting down.\n\nPlease wait a moment and try again."
        ),
        (Failure::Refused(r), true) => {
            format!("{title} konnte nicht gestartet werden: Der Linux-Host hat abgelehnt.\n\n{}", reason(r))
        }
        (Failure::Refused(r), false) => {
            format!("{title} could not be started: the Linux host refused.\n\n{}", reason(r))
        }
        (Failure::Session(r), true) => {
            format!("{title} konnte nicht gestartet werden: Die Sitzung ließ sich nicht starten.\n\n{}", reason(r))
        }
        (Failure::Session(r), false) => {
            format!("{title} could not be started: the session did not start.\n\n{}", reason(r))
        }
        (Failure::NoWindow, true) => {
            format!("{title} wurde gestartet, aber nach {secs} Sekunden ist kein Fenster erschienen.")
        }
        (Failure::NoWindow, false) => format!("{title} was started, but no window appeared within {secs} seconds."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "Waycompose.app.atp-nested";
    const TITLE: &str = "Windows 7 (Linux)";

    fn w(pid: u32, aumid: Option<&str>, title: &str) -> Window {
        Window { pid, visible: true, owned: false, aumid: aumid.map(String::from), title: title.into() }
    }

    #[test]
    fn a_window_is_matched_by_its_identity() {
        assert_eq!(matches(&w(7, Some(ID), "KWin"), 7, ID, TITLE), Some(By::Identity));
    }

    #[test]
    fn another_identity_is_not_matched_even_with_the_title() {
        assert_eq!(matches(&w(7, Some("Waycompose.app.firefox"), TITLE), 7, ID, TITLE), None);
        // The Windows 11 shortcut's window is not the Windows 7 one's.
        assert_eq!(matches(&w(7, Some("Waycompose.app.atp-nested-win11"), TITLE), 7, ID, TITLE), None);
    }

    #[test]
    fn without_an_identity_the_exact_title_decides() {
        assert_eq!(matches(&w(7, None, TITLE), 7, ID, TITLE), Some(By::Title));
        assert_eq!(matches(&w(7, Some(""), TITLE), 7, ID, TITLE), Some(By::Title));
        assert_eq!(matches(&w(7, None, "Windows 7 (Linux) "), 7, ID, TITLE), None);
        assert_eq!(matches(&w(7, None, "windows 7 (linux)"), 7, ID, TITLE), None);
        assert_eq!(matches(&w(7, None, ""), 7, ID, ""), None);
    }

    #[test]
    fn only_the_sessions_compositor_counts() {
        assert_eq!(matches(&w(8, Some(ID), TITLE), 7, ID, TITLE), None);
        // No session: no compositor pid, nothing matches, not even pid 0's.
        assert_eq!(matches(&w(0, Some(ID), TITLE), 0, ID, TITLE), None);
    }

    #[test]
    fn hidden_and_owned_windows_are_not_the_application() {
        let mut x = w(7, Some(ID), TITLE);
        x.visible = false;
        assert_eq!(matches(&x, 7, ID, TITLE), None);
        let mut x = w(7, Some(ID), TITLE);
        x.owned = true;
        assert_eq!(matches(&x, 7, ID, TITLE), None);
    }

    #[test]
    fn a_shared_compositors_windows_are_looked_for_when_there_is_no_own_one() {
        assert_eq!(window_pid(10, 20), 10);
        assert_eq!(window_pid(0, 20), 20);
        assert_eq!(window_pid(0, 0), 0);
        // Another installation's window in the same shared compositor, with
        // another identity, is not this application's.
        let ws = [w(20, Some("Waycompose.app.firefox"), "Firefox"), w(20, Some(ID), "x")];
        assert_eq!(find(&ws, window_pid(0, 20), ID, TITLE).map(|(i, _)| i), Some(1));
    }

    #[test]
    fn find_returns_the_first_match() {
        let ws = [w(9, Some(ID), TITLE), w(7, Some("other"), "x"), w(7, None, TITLE), w(7, Some(ID), "y")];
        assert_eq!(find(&ws, 7, ID, TITLE), Some((2, By::Title)));
        assert_eq!(find(&ws[..2], 7, ID, TITLE), None);
    }

    #[test]
    fn lock_names_are_per_session_and_app_and_stay_one_name() {
        assert_eq!(lock_name("wclinux", "linux-desktop"), "Local\\waycompose-single-wclinux-linux-desktop");
        assert_ne!(lock_name("wclinux", "linux-desktop"), lock_name("wclinux", "linux-desktop-win11"));
        assert_ne!(lock_name("a", "linux-desktop"), lock_name("b", "linux-desktop"));
        let n = lock_name("a\\b", "c/d e");
        assert_eq!(n.matches('\\').count(), 1, "{n}");
        assert_eq!(n, "Local\\waycompose-single-a_b-c_d_e");
    }

    #[test]
    fn wait_results_map_to_lock_states() {
        assert_eq!(Lock::from_wait(0), Lock::Taken);
        assert_eq!(Lock::from_wait(0x80), Lock::Abandoned);
        assert_eq!(Lock::from_wait(0x102), Lock::Held); // WAIT_TIMEOUT
        assert_eq!(Lock::from_wait(0xffff_ffff), Lock::Held); // WAIT_FAILED
        assert!(Lock::Abandoned.ours());
    }

    #[test]
    fn a_window_wins_and_the_lock_is_not_even_tried() {
        assert_eq!(first(true, || panic!("the lock must not be taken")), First::Focus);
        assert_eq!(first(false, || Lock::Taken), First::Launch);
        assert_eq!(first(false, || Lock::Abandoned), First::Launch);
        assert_eq!(first(false, || Lock::Held), First::Wait);
    }

    #[test]
    fn a_second_click_waits_then_focuses_or_goes_quietly() {
        let s = Duration::from_secs;
        assert_eq!(waiting(false, false, s(1)), Waiting::Again);
        assert_eq!(waiting(true, false, s(1)), Waiting::Focus);
        // The first launch released the lock as its window appeared.
        assert_eq!(waiting(true, true, s(5)), Waiting::Focus);
        assert_eq!(waiting(false, true, s(5)), Waiting::FirstFailed);
        assert_eq!(waiting(false, false, WAIT_BOUND), Waiting::GaveUp);
        assert_eq!(waiting(true, false, WAIT_BOUND), Waiting::Focus);
    }

    #[test]
    fn the_launch_holds_the_lock_until_its_window_or_the_bound() {
        let s = Duration::from_secs;
        assert_eq!(launching(false, s(4)), Launching::Again);
        assert_eq!(launching(true, s(4)), Launching::Up);
        assert_eq!(launching(false, LAUNCH_BOUND), Launching::NoWindow);
        assert_eq!(launching(true, LAUNCH_BOUND), Launching::Up);
        // The second click outlasts the first, so it sees the release.
        assert!(WAIT_BOUND > LAUNCH_BOUND);
    }

    #[test]
    fn the_language_is_german_or_english() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(german(&l(&["de-DE", "de_DE", "de"])));
        assert!(german(&l(&["en-US", "de"])));
        assert!(!german(&l(&["en-US", "en"])));
        assert!(!german(&l(&[])));
        assert!(!german(&l(&["dek"])));
    }

    #[test]
    fn messages_name_the_application_and_the_reason() {
        let m = message(&Failure::Refused("wc-session: no such app\nmore".into()), TITLE, true);
        assert!(m.starts_with("Windows 7 (Linux) konnte nicht gestartet werden"), "{m}");
        assert!(m.ends_with("wc-session: no such app"), "{m}");
        let m = message(&Failure::Refused("run 1: a nested session is already running".into()), TITLE, true);
        assert!(m.contains("läuft bereits"), "{m}");
        let m = message(&Failure::Refused("a nested session is already running".into()), TITLE, false);
        assert!(m.contains("already running"), "{m}");
        let m = message(&Failure::Session("the compositor never published".into()), TITLE, false);
        assert!(m.contains("session did not start") && m.contains("never published"), "{m}");
        let m = message(&Failure::NoWindow, TITLE, true);
        assert!(m.contains("90 Sekunden"), "{m}");
        let m = message(&Failure::NoWindow, TITLE, false);
        assert!(m.contains("90 seconds"), "{m}");
    }

    #[test]
    fn a_long_reason_is_cut() {
        let long = "x".repeat(500);
        let m = message(&Failure::Session(long), TITLE, false);
        assert!(m.chars().count() < 320 && m.ends_with('…'), "{}", m.len());
    }
}
