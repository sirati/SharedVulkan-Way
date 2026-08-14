//! **Which implementation of Win32 this process is talking to**, decided once
//! at startup and never asked again.
//!
//! docs/waycompose-design.md §39.
//!
//! # Why this exists
//!
//! Every Windows-side run in this project has had two possible venues: a real
//! Windows kernel on the guest, and Wine on a Linux box. The second is the one
//! that is always available, so it is the one nearly every gate in `scripts/`
//! actually uses — and it is *not* Windows. §13.6 settles the tally at **four
//! recorded discrepancies**, one of which (`SetProcessDpiAwarenessContext`,
//! §10.8) was withdrawn when the cause turned out to be a session-0 window
//! station rather than Wine. §26.5 adds the correction that matters more than
//! the number: most of what this document called "Wine lied" was really "there
//! was no hypervisor in the test", which is a fact about the *topology* and not
//! about Wine.
//!
//! So the point of this module is emphatically **not** to route around Wine. It
//! is two much smaller things:
//!
//! 1. **Say which venue a run measured**, once, in a line a harness can assert
//!    on — so that a run cannot silently be evidence about the wrong platform.
//!    That failure has happened here: §33's photograph of "this compositor
//!    ignoring input" was another worktree's `weston-terminal`, and it reached
//!    `docs/` before anybody noticed.
//! 2. **Give the small number of genuine Wine defects one decision point**, so
//!    that a workaround is a named branch with a measurement attached rather
//!    than an unconditional allowance that quietly weakens the Windows run too.
//!
//! # The bar for adding a branch, which is deliberately high
//!
//! **A Wine path is only allowed where Wine is broken, never where Wine is
//! merely different.** The whole value of the Wine legs is that they are a
//! stand-in for Windows: every `if under_wine()` is one more thing the Wine run
//! stops testing about the Windows run, so the set has to stay small enough to
//! list. It is listed in §39.4, and today it is two entries, neither of which
//! changes a pixel:
//!
//! * `platform::win32::apply_frame` — Wine's `dwmapi` answers `S_OK` to
//!   `DwmExtendFrameIntoClientArea` and `DwmIsCompositionEnabled` while
//!   implementing neither, so the compositor's own log claimed a compositing
//!   arrangement that did not exist. §31.7 predicted exactly this ("a run that
//!   extends the frame and finds `dwmapi` stubbed would look exactly like
//!   `noglass.png` and would report success ... the one failure mode of this
//!   arrangement with no diagnostic") and this is the diagnostic.
//! * `scripts/probe-cursor.c` — `GetCursorInfo` is *fatal* under Wine's X11
//!   driver, which is not a Rust call site but is the same decision made the
//!   same way, from the same probe.
//!
//! Three things that look like candidates and are deliberately **not** here are
//! in §39.5, with the reason each was refused.
//!
//! # The override, and why a stale assumption must be falsifiable
//!
//! `WC_WIN32=windows|wine` replaces the probe's answer. Two uses, both real:
//! exercising a Wine path on Windows (so it is not dead code that has never
//! run where it matters), and forcing the Windows path under Wine to check that
//! a branch here is still *earning* its place. Every entry above is a claim
//! about a specific Wine version, and `WC_WIN32=windows` is how the next
//! person tests whether it has been fixed upstream without booting anything.
//!
//! An unrecognised value is a startup failure rather than a warning, for the
//! reason the argument parser gives about `--decorations`: a typo that falls
//! back to the probe is a run that silently measures the platform it was told
//! not to.

use std::sync::OnceLock;

/// Which Win32 this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Win32 {
    /// A real Windows kernel.
    Windows,
    /// Wine, with what `ntdll!wine_get_version` and `wine_get_host_version`
    /// said. The version is kept rather than discarded because every entry in
    /// §39.4 is a claim about a *particular* Wine, and the first question asked
    /// of a quirk that stops reproducing is "which one was it measured on".
    Wine { version: String, host: String },
    /// Not a Windows binary at all: the Linux build, which is what `cargo test`
    /// and the headless WLCS leg run.
    ///
    /// A variant rather than an absence so that the startup line says it out
    /// loud. A harness that meant to run the `.exe` and got this one has
    /// measured nothing about Windows, and §35 is a section about gates that
    /// reported success while something was wrong.
    None,
}

impl Win32 {
    /// The stable, greppable name. `wc-cursor.sh` and friends match on this.
    pub fn tag(&self) -> &'static str {
        match self {
            Win32::Windows => "windows",
            Win32::Wine { .. } => "wine",
            Win32::None => "none",
        }
    }
}

/// The one decision, made on first use and cached.
static WIN32: OnceLock<Win32> = OnceLock::new();

/// What this process is running on.
///
/// The `OnceLock` is the point: the probe is a `GetProcAddress` and could
/// perfectly well be repeated, but a value that can be re-read is a value two
/// call sites can disagree about, and the startup line would then be a claim
/// about only the first of them.
pub fn win32() -> &'static Win32 {
    WIN32.get_or_init(|| decide(probe(), std::env::var("WC_WIN32").ok().as_deref()).0)
}

/// The predicate the branches use. Reads as English at a call site, which is
/// most of why it exists.
pub fn under_wine() -> bool {
    matches!(win32(), Win32::Wine { .. })
}

/// What the probe found, before `WC_WIN32` has had a say.
///
/// **Each target constructs a subset of this and the tests construct all of
/// it**, which is the point of splitting the pure decision away from the FFI —
/// the same argument `wc-handle`'s Unix build makes for itself. The Linux build
/// produces only `None`; the Windows build produces only the other two. So the
/// allow is unconditional rather than `cfg`-ed either way, and a `cfg` on the
/// variants themselves would take [`decide`]'s tests off whichever target it
/// was applied to — leaving the override logic untested on the platform that
/// uses it.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probed {
    Windows,
    Wine { version: String, host: String },
    None,
}

impl Probed {
    fn into_win32(self) -> Win32 {
        match self {
            Probed::Windows => Win32::Windows,
            Probed::Wine { version, host } => Win32::Wine { version, host },
            Probed::None => Win32::None,
        }
    }

    fn tag(&self) -> &'static str {
        match self {
            Probed::Windows => "windows",
            Probed::Wine { .. } => "wine",
            Probed::None => "none",
        }
    }
}

/// Why the answer is what it is — carried separately from the answer so that
/// [`decide`] is a pure function and can be tested on the machine that builds
/// it, which is the same argument `wc-handle`'s Unix build makes for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
enum How {
    /// The probe was believed.
    Probe,
    /// `WC_WIN32` overrode it, and this is what the probe had said.
    Forced(&'static str),
    /// `WC_WIN32` held something that is not `windows` or `wine`.
    Rejected(String),
}

/// Apply the override to the probe. Pure, so it has tests.
fn decide(probed: Probed, forced: Option<&str>) -> (Win32, How) {
    let probe_tag = probed.tag();
    match forced {
        None => (probed.into_win32(), How::Probe),
        // Forcing the value the probe already found is not an error and not a
        // no-op worth complaining about: it is what a harness that always sets
        // the variable does, and it should still say the value was forced so
        // the log does not imply a measurement that was not taken.
        Some("windows") => (Win32::Windows, How::Forced(probe_tag)),
        Some("wine") => (
            // A forced Wine carries no version, because none was measured, and
            // inventing one would put a number in the log that no `ntdll`
            // returned. The word `forced` in the line is what says why.
            Win32::Wine { version: "forced".into(), host: "forced".into() },
            How::Forced(probe_tag),
        ),
        Some(other) => (probed.into_win32(), How::Rejected(other.to_owned())),
    }
}

/// Decide, print the line, and fail the run if `WC_WIN32` was nonsense.
///
/// Called from `run_from` immediately after `--log` has been redirected, so
/// this is the first line of every log a run produces and a harness reading
/// that file does not have to race the compositor for it.
///
/// Returns `Err` with the process exit code, matching how the argument parser
/// reports a bad `--decorations`.
pub fn announce() -> Result<(), u8> {
    let probed = probe();
    let forced = std::env::var("WC_WIN32").ok();
    let (win32, how) = decide(probed, forced.as_deref());

    if let How::Rejected(bad) = &how {
        eprintln!("wc-compositor: WC_WIN32={bad}: expected windows or wine");
        return Err(2);
    }

    // Whatever else happens, the cached answer is this one. `set` rather than
    // `get_or_init` so that a caller which reached `win32()` before this point
    // is a visible bug rather than a silent disagreement between the line
    // printed and the value branched on.
    if WIN32.set(win32.clone()).is_err() {
        eprintln!(
            "wc-compositor: win32 was decided before it was announced; the line below may not \
             be what the code branched on"
        );
    }

    let forced_note = match &how {
        How::Forced(probe_tag) => format!(" (forced by WC_WIN32; the probe said {probe_tag})"),
        _ => String::new(),
    };
    match &win32 {
        Win32::Windows => println!("wc-compositor: win32 is windows{forced_note}"),
        Win32::Wine { version, host } => {
            println!("wc-compositor: win32 is wine {version} (host {host}){forced_note}")
        }
        // Said in full rather than as a tag, because this line is the one a
        // person reads after wondering why a Windows assertion did not fire.
        Win32::None => println!(
            "wc-compositor: win32 is none -- this is the Linux build and nothing here is \
             evidence about Windows{forced_note}"
        ),
    }
    Ok(())
}

/// Ask `ntdll`.
///
/// `wine_get_version` in `ntdll.dll` is the probe Wine itself documents, and it
/// is a `GetProcAddress` rather than anything cleverer for the reason
/// `win32.rs` gives about `GetDpiForWindow` and `dwmapi`: an export that is
/// absent on the other platform cannot be linked, only resolved.
///
/// `GetModuleHandleW` rather than `LoadLibraryW` because `ntdll` is mapped into
/// every Windows process before `main` runs; there is nothing to load and
/// nothing to release.
#[cfg(windows)]
fn probe() -> Probed {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

    let name: Vec<u16> = "ntdll.dll".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: a NUL-terminated wide string that outlives the call. The handle
    // is borrowed, not owned — `GetModuleHandleW` does not take a reference.
    let ntdll = unsafe { GetModuleHandleW(name.as_ptr()) };
    if ntdll.is_null() {
        // Unreachable on any Windows that got this far, and said out loud
        // rather than folded into the `Windows` answer, because "we guessed"
        // and "we measured" must not print the same line.
        eprintln!("wc-compositor: ntdll.dll is not mapped; assuming windows, unmeasured");
        return Probed::Windows;
    }

    // SAFETY: a live module handle and a NUL-terminated name. A null return is
    // the documented "no such export", which here is the whole answer.
    let version = unsafe { GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()) };
    let Some(version) = version else { return Probed::Windows };

    // `wine_get_version` is `const char *(void)`. Wine declares it `CDECL`,
    // which on x86-64 is the one calling convention there is, so `extern "C"`
    // and `extern "system"` are the same ABI here and the choice is only
    // documentation.
    // SAFETY: the transmute is to that signature; the pointer came from
    // `GetProcAddress` for exactly that name.
    let version: unsafe extern "C" fn() -> *const i8 = unsafe { std::mem::transmute(version) };
    // SAFETY: Wine returns a pointer to a static NUL-terminated string that
    // outlives the process; it is copied out immediately regardless.
    let version = unsafe { cstr(version()) }.unwrap_or_else(|| "unknown".into());

    // SAFETY: as above. Absent on a Wine old enough not to have it, which is
    // why the host is optional and the version is not.
    let host = unsafe { GetProcAddress(ntdll, c"wine_get_host_version".as_ptr().cast()) }
        .map(|f| {
            // `void wine_get_host_version(const char **sysname, const char **release)`.
            // SAFETY: the transmute is to that signature; both out-parameters
            // are live and are only read after the call returns.
            let f: unsafe extern "C" fn(*mut *const i8, *mut *const i8) =
                unsafe { std::mem::transmute(f) };
            let (mut sys, mut rel) = (std::ptr::null(), std::ptr::null());
            unsafe { f(&mut sys, &mut rel) };
            // SAFETY: Wine writes static strings or leaves the nulls in place.
            let sys = unsafe { cstr(sys) }.unwrap_or_else(|| "?".into());
            let rel = unsafe { cstr(rel) }.unwrap_or_else(|| "?".into());
            format!("{sys} {rel}")
        })
        .unwrap_or_else(|| "unknown".into());

    Probed::Wine { version, host }
}

/// Copy a C string Wine owns into one we do.
///
/// # Safety
/// `p` is null or points at a NUL-terminated string valid for the call.
#[cfg(windows)]
unsafe fn cstr(p: *const i8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller's contract.
    Some(unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// There is no Win32 here to ask.
///
/// Not a stub that answers `Windows`: this build has no `ntdll`, and a probe
/// that returned a plausible answer on the platform where the question is
/// meaningless is precisely §13.6's mistake — an answer taken in a place the
/// code will not run.
#[cfg(not(windows))]
fn probe() -> Probed {
    Probed::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wine() -> Probed {
        Probed::Wine { version: "11.14".into(), host: "Linux 6.18.33".into() }
    }

    #[test]
    fn an_unset_variable_leaves_the_probe_alone() {
        assert_eq!(decide(Probed::Windows, None), (Win32::Windows, How::Probe));
        assert_eq!(decide(Probed::None, None), (Win32::None, How::Probe));
        let (w, how) = decide(wine(), None);
        assert_eq!(how, How::Probe);
        assert_eq!(w.tag(), "wine");
        // The version survives the trip, because §39.4's entries are claims
        // about a particular Wine and a log that dropped the number would make
        // the next person re-measure to find out which.
        assert_eq!(w, Win32::Wine { version: "11.14".into(), host: "Linux 6.18.33".into() });
    }

    #[test]
    fn the_override_wins_in_both_directions() {
        // Forcing the Windows path under Wine: how a branch here is checked
        // for still earning its place, without booting anything.
        assert_eq!(
            decide(wine(), Some("windows")),
            (Win32::Windows, How::Forced("wine"))
        );
        // And the Wine path on Windows, so it is not code that has only ever
        // run where it does not matter.
        let (w, how) = decide(Probed::Windows, Some("wine"));
        assert_eq!(how, How::Forced("windows"));
        assert!(matches!(w, Win32::Wine { .. }));
    }

    #[test]
    fn a_forced_wine_does_not_invent_a_version() {
        // The alternative — carrying the probe's version through an override —
        // would put a number in the log that no `ntdll` on this machine
        // returned, which is the kind of plausible-and-wrong evidence §35 is
        // about.
        let (w, _) = decide(Probed::Windows, Some("wine"));
        assert_eq!(w, Win32::Wine { version: "forced".into(), host: "forced".into() });
    }

    #[test]
    fn forcing_what_the_probe_already_said_is_still_reported_as_forced() {
        // A harness that always sets the variable gets a line saying so, rather
        // than one implying a measurement it did not take.
        assert_eq!(
            decide(Probed::Windows, Some("windows")),
            (Win32::Windows, How::Forced("windows"))
        );
    }

    #[test]
    fn a_typo_is_refused_rather_than_ignored() {
        // The failure this prevents: `WC_WIN32=Wine` silently falling back to
        // the probe, so a run told to exercise the Wine path measures the
        // Windows one and passes.
        let (w, how) = decide(wine(), Some("Wine"));
        assert_eq!(how, How::Rejected("Wine".into()));
        // The value is still the probe's, but `announce` refuses to start, so
        // nothing ever branches on it.
        assert!(matches!(w, Win32::Wine { .. }));
        assert!(matches!(decide(Probed::Windows, Some("")).1, How::Rejected(_)));
        assert!(matches!(decide(Probed::Windows, Some("linux")).1, How::Rejected(_)));
    }

    #[test]
    fn the_tags_are_the_strings_the_harnesses_grep_for() {
        // Pinned because `scripts/wc-cursor.sh` matches on them, and a rename
        // here would turn every Windows-only assertion there into a silent
        // skip rather than a failure.
        assert_eq!(Win32::Windows.tag(), "windows");
        assert_eq!(Win32::None.tag(), "none");
        assert_eq!(wine().into_win32().tag(), "wine");
    }

    /// The Linux build must not answer a Windows question plausibly.
    #[test]
    #[cfg(not(windows))]
    fn the_linux_build_says_none_rather_than_guessing() {
        assert_eq!(probe(), Probed::None);
    }

    /// On the Windows target this is the real thing, and it is the assertion
    /// that catches a broken transmute or a mistyped export name: run under
    /// Wine it must say `wine`, and `WC_WIN32` is not consulted here.
    #[test]
    #[cfg(windows)]
    fn the_probe_agrees_with_the_binary_it_is_running_under() {
        match probe() {
            Probed::Wine { version, host } => {
                assert!(!version.is_empty(), "wine_get_version returned nothing");
                assert!(!host.is_empty(), "wine_get_host_version returned nothing");
            }
            // Also correct: this test binary can be run on a real guest.
            Probed::Windows => {}
            Probed::None => panic!("the windows build probed `none`"),
        }
    }
}
