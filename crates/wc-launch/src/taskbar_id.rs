//! §92.12. Which taskbar identity each application of an installation has,
//! decided once at deploy, and what removing the installation takes back.
//!
//! The portable half: every decision is a function of what the launcher read
//! from the user's shortcuts, the claim files and the installation's own ini,
//! so it is tested on the machine that compiles it. `main.rs`'s `win` module
//! reads those facts through `shell.rs` and carries the decision out.
//!
//! # The rule
//!
//! Since §92.12 an installation's applications carry its own identity,
//! `Waycompose.install.<installation>.<app id>`. Until then every
//! installation used the application-wide `Waycompose.app.<app id>`, and the
//! user's pins, desktop shortcuts and jump lists carry it. Changing the
//! identity of a pinned shortcut under a running Explorer splits the pin and
//! its windows into two buttons until Explorer restarts, because Explorer
//! keeps its own copy of the pins. So the one installation whose pin (or,
//! with no pin anywhere, whose desktop shortcut) already carries the old
//! identity keeps it: `taskbar-id = legacy`. That installation is the one
//! the user has been clicking, so nothing the user sees changes. Every other
//! installation, and every test installation, gets its own.
//!
//! The choice is written into the installation's `wc-launch.ini` and is kept
//! from then on, so it never flips between deploys. A claim file per legacy
//! identity, `%LOCALAPPDATA%\waycompose\taskbar\<identity>.owner`, names the
//! installation that holds it, so a second installation cannot take it too;
//! the compositor refuses a second claim at boot as well
//! (`taskbar::Registry::add_session`).

use crate::config::TaskbarId;

/// Where a shortcut was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// `%APPDATA%\Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar`:
    /// what the user pinned. Never rewritten (see the module's rule).
    Pinned,
    /// The user's desktop: what `--install` wrote, and the user may have
    /// renamed or moved.
    Desktop,
    /// `Programs`: written by every launch, so not evidence of anything the
    /// user did.
    StartMenu,
}

impl Place {
    pub fn as_str(self) -> &'static str {
        match self {
            Place::Pinned => "pinned",
            Place::Desktop => "desktop",
            Place::StartMenu => "start menu",
        }
    }
}

/// One `.lnk`, as `IShellLinkW` and its property store read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub place: Place,
    pub path: String,
    /// The program it runs.
    pub target: String,
    /// `System.AppUserModel.ID`, empty if it has none. `None` when it could
    /// not be read (Wine's shell links do not implement `GetValue`). Such a
    /// link of a `wc-launch.exe` is taken to carry the application-wide
    /// identity, which every launcher wrote before §92.12, so it can only
    /// keep that identity where it is, and it is never rewritten.
    pub aumid: Option<String>,
}

/// The claim file of a legacy identity: the `wc-launch.exe` of the
/// installation that holds it, and whether that file still exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub owner: String,
    pub owner_exists: bool,
}

/// Two Windows paths name one file: case and slash direction aside.
pub fn same_path(a: &str, b: &str) -> bool {
    let n = |s: &str| s.trim().trim_matches('"').replace('/', "\\").to_lowercase();
    !a.trim().is_empty() && n(a) == n(b)
}

/// Everything one application's decision depends on.
pub struct Input<'a> {
    /// `Waycompose.app.<app id>`.
    pub legacy_aumid: &'a str,
    /// This installation's `wc-launch.exe`.
    pub own_exe: &'a str,
    /// What the installation's ini (or the one deploy replaced) already says.
    pub persisted: Option<TaskbarId>,
    /// A test installation: never the application-wide identity.
    pub test: bool,
    pub claim: Option<&'a Claim>,
    pub links: &'a [Link],
    /// Whether a program path exists: another installation's links count only
    /// while their target is still there.
    pub exists: &'a dyn Fn(&str) -> bool,
}

/// What to do with the claim file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimAction {
    None,
    /// Write it, naming this installation.
    Write,
    /// It already names this installation.
    Keep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub id: TaskbarId,
    pub why: String,
    pub claim: ClaimAction,
    /// This installation's own desktop and Start menu shortcuts that carry the
    /// application-wide identity while the decision is its own: rewritten to
    /// the installation's identity. Unpinned shortcuts only, which nobody sees
    /// the identity of.
    pub rewrite: Vec<String>,
    pub warnings: Vec<String>,
}

/// The decision for one application of this installation.
pub fn decide(i: &Input) -> Decision {
    let with_legacy: Vec<&Link> =
        i.links.iter().filter(|l| l.aumid.as_deref().is_none_or(|a| a.eq_ignore_ascii_case(i.legacy_aumid))).collect();
    let ours = |l: &Link| same_path(&l.target, i.own_exe);
    // Whose link it is: this installation's (`true`), or another's that is
    // still installed (`false`).
    let find = |p: Place, mine: bool| {
        with_legacy
            .iter()
            .copied()
            .find(|l| l.place == p && if mine { ours(l) } else { !ours(l) && (i.exists)(&l.target) })
    };
    let claim_ours = i.claim.is_some_and(|c| same_path(&c.owner, i.own_exe));
    let claim_other = i.claim.filter(|c| !same_path(&c.owner, i.own_exe) && c.owner_exists);
    let mut warnings = Vec::new();

    let (id, why) = if let Some(p) = i.persisted {
        if p == TaskbarId::Legacy && let Some(c) = claim_other {
            warnings.push(format!(
                "the ini says legacy but {} holds the claim on {}; kept, and the compositor gives the identity to whichever session boots first",
                c.owner, i.legacy_aumid
            ));
        }
        (p, "kept from the installation's ini".to_string())
    } else if i.test {
        (TaskbarId::Install, "a test installation".to_string())
    } else if claim_ours {
        (TaskbarId::Legacy, "this installation holds the claim".to_string())
    } else if let Some(c) = claim_other {
        (TaskbarId::Install, format!("{} holds the claim", c.owner))
    } else {
        let desktop_ours = find(Place::Desktop, true).is_some();
        let desktop_other = find(Place::Desktop, false);
        if find(Place::Pinned, true).is_some() {
            (TaskbarId::Legacy, "the user's pin carries it and runs this installation".to_string())
        } else if let Some(l) = find(Place::Pinned, false) {
            (TaskbarId::Install, format!("the user's pin carries it and runs {}", l.target))
        } else if desktop_ours && desktop_other.is_none() {
            (TaskbarId::Legacy, "no pin; the desktop shortcut carries it and runs this installation".to_string())
        } else if let Some(l) = desktop_other.filter(|_| desktop_ours) {
            (TaskbarId::Install, format!("no pin, and desktop shortcuts of this installation and of {} carry it", l.target))
        } else {
            (TaskbarId::Install, "no pin or desktop shortcut of this installation carries it".to_string())
        }
    };

    let claim = match (id, claim_ours) {
        (TaskbarId::Legacy, true) => ClaimAction::Keep,
        (TaskbarId::Legacy, false) if claim_other.is_none() => ClaimAction::Write,
        _ => ClaimAction::None,
    };
    let mut rewrite = Vec::new();
    if id == TaskbarId::Install {
        for l in with_legacy.iter().copied().filter(|l| ours(l)) {
            if l.place == Place::Pinned {
                warnings.push(format!(
                    "{} is pinned and carries {}; it is left alone (Explorer keeps its own copy of a pin), \
                     so this installation's windows get a button beside it until it is pinned again",
                    l.path, i.legacy_aumid
                ));
            } else if l.aumid.is_some() {
                rewrite.push(l.path.clone());
            } else {
                warnings.push(format!("{}: its identity could not be read, so it is left as it is", l.path));
            }
        }
    }
    Decision { id, why, claim, rewrite, warnings }
}

/// What removing an installation's taskbar presence takes back, for one
/// application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    /// The jump list to delete (`ICustomDestinationList::DeleteList`): the
    /// installation's own, never another's.
    pub delete_list: Option<String>,
    /// Remove the claim file of the legacy identity.
    pub remove_claim: bool,
    /// This installation's desktop and Start menu shortcuts.
    pub remove_links: Vec<String>,
    /// Its pins, which only the user can take off the taskbar.
    pub pins_left: Vec<String>,
}

pub fn plan_removal(id: TaskbarId, own_aumid: &str, legacy_aumid: &str, own_exe: &str, claim: Option<&Claim>, links: &[Link]) -> Removal {
    let claim_ours = claim.is_some_and(|c| same_path(&c.owner, own_exe));
    let (delete_list, remove_claim) = match id {
        TaskbarId::Install => (Some(own_aumid.to_string()), false),
        // The application-wide list is this installation's only while it
        // holds the claim; otherwise it is another's, or nobody knows whose.
        TaskbarId::Legacy if claim_ours => (Some(legacy_aumid.to_string()), true),
        TaskbarId::Legacy => (None, false),
    };
    let mine = |l: &&Link| {
        same_path(&l.target, own_exe)
            && l.aumid.as_deref().is_none_or(|a| a.eq_ignore_ascii_case(own_aumid) || a.eq_ignore_ascii_case(legacy_aumid))
    };
    let mut r = Removal { delete_list, remove_claim, remove_links: Vec::new(), pins_left: Vec::new() };
    for l in links.iter().filter(mine) {
        if l.place == Place::Pinned { r.pins_left.push(l.path.clone()) } else { r.remove_links.push(l.path.clone()) }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: &str = "Waycompose.app.firefox";
    const IVS: &str = r"C:\Users\<guest-user>\wcivs\wc-launch.exe";
    const PINS: &str = r"C:\Users\<guest-user>\wcpins\wc-launch.exe";
    const GONE: &str = r"C:\Users\<guest-user>\wclaunch\wc-launch.exe";

    fn link(place: Place, path: &str, target: &str, aumid: &str) -> Link {
        Link { place, path: path.into(), target: target.into(), aumid: Some(aumid.into()) }
    }

    fn unread(place: Place, path: &str, target: &str) -> Link {
        Link { place, path: path.into(), target: target.into(), aumid: None }
    }

    /// A shortcut whose identity cannot be read counts as the old identity,
    /// so it can keep it where it is and is never rewritten: what Wine shows,
    /// and the safe reading of a failure on Windows.
    #[test]
    fn an_unreadable_identity_only_ever_keeps_the_old_one() {
        let links = vec![
            unread(Place::Pinned, "p", IVS),
            unread(Place::Desktop, "d1", IVS),
            unread(Place::Desktop, "d2", PINS),
            unread(Place::StartMenu, "s2", PINS),
        ];
        let d = decide(&input(IVS, &links, None));
        assert_eq!(d.id, TaskbarId::Legacy, "{}", d.why);
        let d = decide(&input(PINS, &links, None));
        assert_eq!(d.id, TaskbarId::Install, "{}", d.why);
        assert!(d.rewrite.is_empty(), "nothing unreadable is rewritten");
        assert_eq!(d.warnings.len(), 2);
        let r = plan_removal(TaskbarId::Install, "own", L, PINS, None, &links);
        assert_eq!(r.remove_links, ["d2", "s2"]);
    }

    fn exists(p: &str) -> bool {
        !same_path(p, GONE)
    }

    fn input<'a>(own: &'a str, links: &'a [Link], claim: Option<&'a Claim>) -> Input<'a> {
        Input {
            legacy_aumid: L,
            own_exe: own,
            persisted: None,
            test: false,
            claim,
            links,
            exists: &exists,
        }
    }

    /// The user's situation: `Firefox (Linux)` pinned from `wcivs`, a test
    /// installation's Start menu entry with the same identity.
    fn user() -> Vec<Link> {
        vec![
            link(Place::Pinned, r"P\Firefox (Linux).lnk", IVS, L),
            link(Place::Desktop, r"D\Firefox (Linux).lnk", IVS, L),
            link(Place::StartMenu, r"S\Firefox (Linux).lnk", IVS, L),
            link(Place::StartMenu, r"S\Firefox (Linux, pins test).lnk", PINS, L),
        ]
    }

    #[test]
    fn the_installation_whose_pin_carries_the_identity_keeps_it() {
        let links = user();
        let d = decide(&input(IVS, &links, None));
        assert_eq!(d.id, TaskbarId::Legacy, "{}", d.why);
        assert_eq!(d.claim, ClaimAction::Write);
        assert!(d.rewrite.is_empty() && d.warnings.is_empty());
        // Every other installation gets its own, and its own unpinned
        // shortcut moves with it.
        let d = decide(&input(PINS, &links, None));
        assert_eq!(d.id, TaskbarId::Install, "{}", d.why);
        assert_eq!(d.claim, ClaimAction::None);
        assert_eq!(d.rewrite, [r"S\Firefox (Linux, pins test).lnk"]);
    }

    /// Whatever order the installations are deployed in, at most one keeps it.
    #[test]
    fn two_installations_never_both_keep_it() {
        let cases: Vec<Vec<Link>> = vec![
            user(),
            vec![link(Place::Desktop, "a", IVS, L), link(Place::Desktop, "b", PINS, L)],
            vec![link(Place::Desktop, "a", IVS, L), link(Place::Pinned, "b", PINS, L)],
            vec![link(Place::Pinned, "a", IVS, L), link(Place::Pinned, "b", PINS, L)],
            vec![link(Place::StartMenu, "a", IVS, L), link(Place::StartMenu, "b", PINS, L)],
            vec![],
        ];
        for links in cases {
            // Either order: the first deploy may write the claim the second reads.
            for (first, second) in [(IVS, PINS), (PINS, IVS)] {
                let a = decide(&input(first, &links, None));
                let claim = (a.claim == ClaimAction::Write).then(|| Claim { owner: first.into(), owner_exists: true });
                let b = decide(&input(second, &links, claim.as_ref()));
                assert!(
                    !(a.id == TaskbarId::Legacy && b.id == TaskbarId::Legacy),
                    "both legacy for {links:?}: {} / {}",
                    a.why,
                    b.why
                );
            }
        }
    }

    #[test]
    fn a_test_installation_never_keeps_it() {
        let links = vec![link(Place::Pinned, "p", PINS, L)];
        let mut i = input(PINS, &links, None);
        i.test = true;
        let d = decide(&i);
        assert_eq!(d.id, TaskbarId::Install);
        assert_eq!(d.claim, ClaimAction::None);
        // Its pin is not rewritten; the deploy log says why.
        assert!(d.rewrite.is_empty());
        assert_eq!(d.warnings.len(), 1);
    }

    /// The decision is kept: a later deploy reads it and never flips it, even
    /// when the evidence has changed since.
    #[test]
    fn a_persisted_decision_never_flips() {
        let mut i = input(IVS, &[], None);
        i.persisted = Some(TaskbarId::Legacy);
        let d = decide(&i);
        assert_eq!((d.id, d.claim), (TaskbarId::Legacy, ClaimAction::Write));
        let links = user();
        let mut i = input(IVS, &links, None);
        i.persisted = Some(TaskbarId::Install);
        let d = decide(&i);
        assert_eq!(d.id, TaskbarId::Install);
        assert_eq!(d.rewrite, [r"D\Firefox (Linux).lnk", r"S\Firefox (Linux).lnk"]);
    }

    /// A claim decides before the shortcuts do; one whose owner is gone does
    /// not.
    #[test]
    fn the_claim_file_decides_and_a_stale_one_does_not() {
        let links = user();
        let other = Claim { owner: PINS.into(), owner_exists: true };
        let d = decide(&input(IVS, &links, Some(&other)));
        assert_eq!(d.id, TaskbarId::Install, "{}", d.why);
        let mine = Claim { owner: IVS.to_uppercase().replace('\\', "/"), owner_exists: true };
        let d = decide(&input(IVS, &[], Some(&mine)));
        assert_eq!((d.id, d.claim), (TaskbarId::Legacy, ClaimAction::Keep));
        let stale = Claim { owner: GONE.into(), owner_exists: false };
        let d = decide(&input(IVS, &links, Some(&stale)));
        assert_eq!((d.id, d.claim), (TaskbarId::Legacy, ClaimAction::Write));
    }

    /// A pin of an installation that is gone is no evidence for anyone.
    #[test]
    fn links_of_a_removed_installation_are_not_evidence() {
        let links = vec![link(Place::Pinned, "p", GONE, L), link(Place::Desktop, "d", IVS, L)];
        assert_eq!(decide(&input(IVS, &links, None)).id, TaskbarId::Legacy);
        // Nor is a Start menu entry: every launch writes one.
        let links = vec![link(Place::StartMenu, "s", IVS, L)];
        let d = decide(&input(IVS, &links, None));
        assert_eq!(d.id, TaskbarId::Install);
        assert_eq!(d.rewrite, ["s"]);
        // Nor a shortcut with some other identity.
        let links = vec![link(Place::Pinned, "p", IVS, "Waycompose.app.thunderbird")];
        assert_eq!(decide(&input(IVS, &links, None)).id, TaskbarId::Install);
    }

    #[test]
    fn paths_compare_as_windows_compares_them() {
        assert!(same_path(r"C:\U\x\wc-launch.exe", "c:/u/X/WC-LAUNCH.EXE"));
        assert!(same_path(r#""C:\U\x\wc-launch.exe""#, r"C:\U\x\wc-launch.exe"));
        assert!(!same_path(r"C:\U\x\wc-launch.exe", r"C:\U\y\wc-launch.exe"));
        assert!(!same_path("", ""));
    }

    /// Removal takes back what is the installation's and nothing else.
    #[test]
    fn removal_deletes_only_the_installation_s_own() {
        let links = user();
        let r = plan_removal(TaskbarId::Install, "Waycompose.install.wcpins.firefox", L, PINS, None, &links);
        assert_eq!(r.delete_list.as_deref(), Some("Waycompose.install.wcpins.firefox"));
        assert!(!r.remove_claim);
        assert_eq!(r.remove_links, [r"S\Firefox (Linux, pins test).lnk"]);
        assert!(r.pins_left.is_empty());
        // The legacy holder deletes the application-wide list only while its
        // claim stands.
        let mine = Claim { owner: IVS.into(), owner_exists: true };
        let r = plan_removal(TaskbarId::Legacy, "Waycompose.install.wcivs.firefox", L, IVS, Some(&mine), &links);
        assert_eq!(r.delete_list.as_deref(), Some(L));
        assert!(r.remove_claim);
        assert_eq!(r.remove_links, [r"D\Firefox (Linux).lnk", r"S\Firefox (Linux).lnk"]);
        assert_eq!(r.pins_left, [r"P\Firefox (Linux).lnk"]);
        let theirs = Claim { owner: PINS.into(), owner_exists: true };
        let r = plan_removal(TaskbarId::Legacy, "x", L, IVS, Some(&theirs), &links);
        assert_eq!(r.delete_list, None);
        assert!(!r.remove_claim);
        assert_eq!(plan_removal(TaskbarId::Legacy, "x", L, IVS, None, &links).delete_list, None);
    }
}
