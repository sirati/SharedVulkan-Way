//! §92. What a Linux application looks like on the Windows taskbar: its
//! identity, the pin's relaunch command, and its jump list. The portable half,
//! tested where the launcher is compiled; `shell.rs` is the COM that writes it.
//!
//! # One identity for the pin, the shortcut and every window
//!
//! Windows puts a window on a pinned button when the window's
//! `PKEY_AppUserModel_ID` equals the pinned shortcut's `System.AppUserModel.ID`.
//! The compositor derives the window's from the Wayland app id
//! (`crate::taskbar::identity`, the compositor's own file compiled in here), so
//! the launcher derives the shortcut's the same way, from the app id the
//! application's windows will carry ([`aumid`]). For Firefox both are
//! `Waycompose.app.firefox`, the same in every session and after a reboot.
//!
//! # Where the facts come from
//!
//! The VM host: `wc-session app` reads the desktop entry (name, actions,
//! localised names, `StartupWMClass`) and sends the icons, and
//! `wc-session frequent` reads the launcher profile's history. Both are parsed
//! here ([`Meta::parse`], [`parse_frequent`]) and turned into a [`Plan`] for the
//! jump list by [`plan`].

use std::collections::{BTreeMap, BTreeSet};

use crate::plan::unb64;
use crate::taskbar;

/// One task the desktop entry (or the host, for Firefox's New Tab) offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Action {
    pub id: String,
    pub name: String,
    /// `Name[<locale>]`, keyed as the desktop entry writes the locale.
    pub names: BTreeMap<String, String>,
    /// A PNG the host drew for this task, if it has one.
    pub icon_png: Option<Vec<u8>>,
}

/// What `wc-session app` says about an application.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    pub desktop: String,
    /// The basename of the entry's own program: `firefox`.
    pub program: String,
    /// Whether the entry's `Exec=` takes a URL, which is what a page in the
    /// jump list needs.
    pub takes_url: bool,
    pub name: String,
    pub names: BTreeMap<String, String>,
    pub wm_class: Option<String>,
    pub actions: Vec<Action>,
    /// `icon NAME SIZE` and the PNG on the line after it.
    pub icon_name: String,
    pub icon_png: Option<Vec<u8>>,
}

fn text(b64: &str) -> Result<String, String> {
    String::from_utf8(unb64(b64)?).map_err(|_| "not UTF-8".to_string())
}

impl Meta {
    /// Parse `wc-session app`'s answer. Unknown lines are skipped, so a newer
    /// host can say more than this launcher reads.
    pub fn parse(out: &str) -> Result<Meta, String> {
        let mut m = Meta::default();
        let mut lines = out.lines();
        while let Some(line) = lines.next() {
            let w: Vec<&str> = line.split_whitespace().collect();
            match w.as_slice() {
                ["desktop", p] => m.desktop = (*p).into(),
                ["program", p] => m.program = (*p).into(),
                ["url", v] => m.takes_url = *v == "yes",
                ["name", b] => m.name = text(b)?,
                ["name-l", l, b] => {
                    m.names.insert((*l).into(), text(b)?);
                }
                ["wmclass", c] => m.wm_class = Some((*c).into()),
                ["action", id, b] => m.actions.push(Action {
                    id: (*id).into(),
                    name: text(b)?,
                    ..Action::default()
                }),
                ["action-l", id, l, b] => {
                    if let Some(a) = m.actions.iter_mut().find(|a| a.id == *id) {
                        a.names.insert((*l).into(), text(b)?);
                    }
                }
                ["taskicon", id, b] => {
                    let png = unb64(b)?;
                    if let Some(a) = m.actions.iter_mut().find(|a| a.id == *id) {
                        a.icon_png = Some(png);
                    }
                }
                ["icon", name, _size] => {
                    m.icon_name = (*name).into();
                    m.icon_png = Some(unb64(lines.next().unwrap_or("").trim())?);
                }
                _ => {}
            }
        }
        if m.desktop.is_empty() || m.program.is_empty() {
            return Err("the host's answer names no desktop entry".into());
        }
        Ok(m)
    }

    /// Firefox, by its program. The jump list then follows Windows' Firefox
    /// rather than the desktop entry: its three tasks, in its order, with its
    /// own words (see [`mozilla_label`]).
    pub fn is_mozilla(&self) -> bool {
        self.program == "firefox" || self.program.starts_with("firefox-")
    }
}

/// The identity of an application's pin, shortcut, jump list and windows.
///
/// The app id is the one its windows will carry: the `[app.<id>]` section's
/// `app-id` if it names one, else the desktop entry's `StartupWMClass`, which
/// exists to say exactly that, else the desktop id.
pub fn aumid(app_id: &str, desktop: &str, meta: Option<&Meta>) -> String {
    let id = if !app_id.is_empty() {
        app_id
    } else if let Some(w) = meta.and_then(|m| m.wm_class.as_deref()).filter(|w| !w.is_empty()) {
        w
    } else {
        desktop
    };
    taskbar::identity(id, 0)
}

// ------------------------------------------------------------ frequent ---

/// Firefox's own jump-list switches, as `wc-session frequent` read them from
/// the profile (`browser.taskbar.lists.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JumpPrefs {
    pub lists: bool,
    pub frequent: bool,
    pub recent: bool,
    pub tasks: bool,
    pub max: usize,
    /// `browser.taskbar.lists.refreshInSeconds`: how often a running session
    /// rebuilds the list.
    pub refresh_secs: u64,
}

impl Default for JumpPrefs {
    /// Firefox's defaults.
    fn default() -> Self {
        JumpPrefs { lists: true, frequent: true, recent: false, tasks: true, max: 7, refresh_secs: 120 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Frequent,
    Recent,
}

/// One page from the profile's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub kind: PageKind,
    pub url: String,
    pub title: String,
    /// PNG or ICO bytes of its favicon.
    pub icon: Option<Vec<u8>>,
}

/// Parse `wc-session frequent`. A line that does not parse is dropped rather
/// than failing the list: one page with an odd title should not cost the rest.
pub fn parse_frequent(out: &str) -> (JumpPrefs, Vec<Page>) {
    let mut prefs = JumpPrefs::default();
    let mut pages = Vec::new();
    for line in out.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w.as_slice() {
            ["prefs", rest @ ..] => {
                for kv in rest {
                    let Some((k, v)) = kv.split_once('=') else { continue };
                    let b = v == "true";
                    match k {
                        "lists" => prefs.lists = b,
                        "frequent" => prefs.frequent = b,
                        "recent" => prefs.recent = b,
                        "tasks" => prefs.tasks = b,
                        "max" => prefs.max = v.parse().unwrap_or(7).min(20),
                        "refresh" => prefs.refresh_secs = v.parse().unwrap_or(120).clamp(30, 3600),
                        _ => {}
                    }
                }
            }
            ["item", kind, url, title, icon] => {
                let kind = match *kind {
                    "frequent" => PageKind::Frequent,
                    "recent" => PageKind::Recent,
                    _ => continue,
                };
                let Ok(url) = text(url) else { continue };
                let title = if *title == "-" { String::new() } else { text(title).unwrap_or_default() };
                let icon = if *icon == "-" { None } else { unb64(icon).ok() };
                pages.push(Page { kind, url, title, icon });
            }
            _ => {}
        }
    }
    (prefs, pages)
}

/// The guest's half of the host's `url_ok`: a URL that can be one argument of
/// a Windows command line and that the host will accept. The host checks
/// again, and its check is the one that matters (security-model.md R33).
pub fn url_ok(u: &str) -> bool {
    let lower = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file:///"))
        // An `IShellLink`'s arguments are cut at 1024 characters, and a
        // truncated URL is a different page.
        && u.len() <= 1000
        && !u.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\' || !c.is_ascii())
}

// -------------------------------------------------------- localisation ---

/// The languages to try for a Windows UI language, most specific first:
/// `de-DE` gives `de-DE`, `de_DE`, `de`.
pub fn lang_candidates(ui: &str) -> Vec<String> {
    let mut out = Vec::new();
    for l in ui.split(['\0', ';', ',']).map(str::trim).filter(|l| !l.is_empty()) {
        for c in [l.to_string(), l.replace('-', "_"), l.split(['-', '_']).next().unwrap_or(l).to_string()] {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// Windows' Firefox's own words for its jump list, which the Linux package
/// does not ship: they are in `browser/taskbar.ftl`, built only for Windows.
/// English and German; any other language gets the English.
pub fn mozilla_label(key: &str, langs: &[String]) -> &'static str {
    let de = crate::single::german(langs);
    let pick = |en: &'static str, de_s: &'static str| if de { de_s } else { en };
    match key {
        "frequent" => pick("Frequent", "Häufig"),
        "recent" => pick("Recent", "Kürzlich"),
        "new-tab" => pick("Open new tab", "Neuen Tab öffnen"),
        "new-window" => pick("Open new window", "Neues Fenster öffnen"),
        "new-private-window" => pick("Open new private window", "Neues privates Fenster öffnen"),
        "new-tab.d" => pick("Open a new browser tab.", "Einen neuen Tab öffnen."),
        "new-window.d" => pick("Open a new browser window.", "Ein neues Browserfenster öffnen."),
        "new-private-window.d" => {
            pick("Open a new window in private browsing mode.", "Ein neues Fenster im Privaten Modus öffnen.")
        }
        _ => "",
    }
}

fn localised<'a>(name: &'a str, names: &'a BTreeMap<String, String>, langs: &[String]) -> &'a str {
    langs.iter().find_map(|l| names.get(l)).map(String::as_str).unwrap_or(name)
}

// ---------------------------------------------------------------- plan ---

/// Which icon a link wears; `shell.rs` maps it to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconRef {
    App,
    /// The task icon the host drew for this action id.
    Task(String),
    /// The favicon of the `n`th page given to [`plan`].
    Page(usize),
}

/// One jump-list entry: `wc-launch.exe <args>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub title: String,
    pub args: Vec<String>,
    pub icon: IconRef,
    pub description: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Custom categories, in order: "Frequent", then "Recent" if asked for.
    pub categories: Vec<(String, Vec<Link>)>,
    pub tasks: Vec<Link>,
}

/// The jump list for `[app.<section>]`.
///
/// Firefox's matches Windows' Firefox: a "Frequent" category of the profile's
/// most visited pages, then the tasks Open new tab, Open new window and Open
/// new private window, named as Windows' Firefox names them in the UI's
/// language. Its Profile Manager action is left out: the launcher pins
/// Firefox's profile with `--profile`, so a profile manager could only start a
/// second Firefox on another profile, beside the session.
///
/// Any other application gets its desktop entry's actions in `Actions=` order,
/// named by `Name[<lang>]` where the entry has one.
///
/// `removed` holds URLs the user removed from the list; Windows refuses a list
/// that puts one back, and the user asked for it to be gone.
pub fn plan(
    section: &str,
    meta: &Meta,
    prefs: &JumpPrefs,
    pages: &[Page],
    langs: &[String],
    removed: &BTreeSet<String>,
) -> Plan {
    let mut p = Plan::default();
    let mozilla = meta.is_mozilla();
    if prefs.tasks {
        let ids: Vec<&str> = if mozilla {
            ["new-tab", "new-window", "new-private-window"]
                .into_iter()
                .filter(|id| meta.actions.iter().any(|a| a.id == *id))
                .collect()
        } else {
            meta.actions.iter().map(|a| a.id.as_str()).collect()
        };
        for id in ids {
            let a = meta.actions.iter().find(|a| a.id == id).expect("filtered above");
            let (title, description) = if mozilla {
                (mozilla_label(id, langs).to_string(), mozilla_label(&format!("{id}.d"), langs).to_string())
            } else {
                (localised(&a.name, &a.names, langs).to_string(), String::new())
            };
            p.tasks.push(Link {
                title,
                args: vec![section.into(), "--action".into(), id.into()],
                icon: if a.icon_png.is_some() { IconRef::Task(id.into()) } else { IconRef::App },
                description,
            });
        }
    }
    if prefs.lists && meta.takes_url {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (kind, on, key) in [
            (PageKind::Frequent, prefs.frequent, "frequent"),
            (PageKind::Recent, prefs.recent, "recent"),
        ] {
            if !on {
                continue;
            }
            let mut links = Vec::new();
            for (i, page) in pages.iter().enumerate() {
                if links.len() >= prefs.max {
                    break;
                }
                if page.kind != kind || !url_ok(&page.url) || removed.contains(&page.url) {
                    continue;
                }
                if !seen.insert(page.url.as_str()) {
                    continue;
                }
                let title = if page.title.trim().is_empty() { page.url.clone() } else { page.title.clone() };
                links.push(Link {
                    title,
                    args: vec![section.into(), "--url".into(), page.url.clone()],
                    icon: if page.icon.is_some() { IconRef::Page(i) } else { IconRef::App },
                    description: page.url.clone(),
                });
            }
            if !links.is_empty() {
                let name = if mozilla { mozilla_label(key, langs).to_string() } else { key.to_string() };
                p.categories.push((name, links));
            }
        }
    }
    p
}

/// One argument of a Windows command line, quoted by the rules
/// `CommandLineToArgvW` and the C runtime undo: quotes only when needed, and
/// backslashes doubled only where they precede a quote.
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0usize;
    for c in a.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

/// A command line from its arguments.
pub fn command_line(args: &[String]) -> String {
    args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ")
}

/// The relaunch file for an application: what a pin made from one of its
/// windows runs, is called and wears.
pub fn relaunch(exe: &str, section: &str, title: &str, icon: Option<&str>) -> taskbar::Relaunch {
    taskbar::Relaunch {
        command: command_line(&[exe.to_string(), section.to_string()]),
        name: title.to_string(),
        icon: icon.map(|i| format!("{i},0")).unwrap_or_default(),
    }
}

/// The removed-items file: one URL per line, kept in the user's own app data.
pub fn parse_removed(text: &str) -> BTreeSet<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

/// The URL a jump-list link's arguments carry, for a link Windows says the
/// user removed: `<section> --url <URL>`, as [`plan`] writes it.
pub fn url_of_args(args: &str) -> Option<String> {
    let mut words = args.split_whitespace();
    while let Some(w) = words.next() {
        if w == "--url" {
            return words.next().map(|u| u.trim_matches('"').to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::b64;

    /// What `wc-session app firefox` printed on testhost, with the PNGs cut
    /// to a valid header.
    fn firefox_meta() -> Meta {
        let png = b64(&[0x89, b'P', b'N', b'G']);
        let out = format!(
            "desktop /nix/store/x-firefox-157.0.1/share/applications/firefox.desktop\n\
             program firefox\nurl yes\nname {}\nwmclass firefox\n\
             action new-private-window {}\naction-l new-private-window de {}\n\
             action new-window {}\naction profile-manager-window {}\naction new-tab {}\n\
             unsupported weird {}\n\
             taskicon new-tab {png}\ntaskicon new-private-window {png}\n\
             icon firefox 128x128\n{png}\n",
            b64(b"Firefox"),
            b64(b"New Private Window"),
            b64("Neues privates Fenster".as_bytes()),
            b64(b"New Window"),
            b64(b"Profile Manager"),
            b64(b"New Tab"),
            b64(b"why"),
        );
        Meta::parse(&out).unwrap()
    }

    fn de() -> Vec<String> {
        lang_candidates("de-DE")
    }

    #[test]
    fn the_host_s_answer_parses() {
        let m = firefox_meta();
        assert_eq!(m.name, "Firefox");
        assert_eq!(m.wm_class.as_deref(), Some("firefox"));
        assert!(m.takes_url && m.is_mozilla());
        assert_eq!(m.actions.len(), 4, "the unsupported one is not an action");
        assert_eq!(m.actions[0].names.get("de").map(String::as_str), Some("Neues privates Fenster"));
        assert!(m.actions.iter().find(|a| a.id == "new-tab").unwrap().icon_png.is_some());
        assert!(m.actions.iter().find(|a| a.id == "new-window").unwrap().icon_png.is_none());
        assert_eq!(m.icon_name, "firefox");
        assert_eq!(m.icon_png.as_deref(), Some(&[0x89, b'P', b'N', b'G'][..]));
        assert!(Meta::parse("program x\n").is_err());
    }

    /// The requirement that makes a pin and its window share a slot: the
    /// launcher's identity for Firefox is the compositor's for Firefox's
    /// windows, which call themselves `firefox`.
    #[test]
    fn the_pin_s_identity_is_the_window_s() {
        let m = firefox_meta();
        assert_eq!(aumid("", "firefox", Some(&m)), taskbar::identity("firefox", 1));
        assert_eq!(aumid("", "firefox", Some(&m)), "Waycompose.app.firefox");
        // Without the host's answer, the desktop id; with an app-id, that.
        assert_eq!(aumid("", "firefox", None), "Waycompose.app.firefox");
        assert_eq!(aumid("org.x.Y", "y", Some(&m)), "Waycompose.app.org.x.Y");
        // Stable: no session, pid or connection in it.
        assert_eq!(aumid("", "firefox", Some(&m)), aumid("", "firefox", None));
    }

    /// Windows' Firefox's three tasks, in its order and its words, and not the
    /// profile manager.
    #[test]
    fn firefox_s_tasks_match_windows_firefox_in_german() {
        let p = plan("firefox", &firefox_meta(), &JumpPrefs::default(), &[], &de(), &BTreeSet::new());
        let titles: Vec<&str> = p.tasks.iter().map(|l| l.title.as_str()).collect();
        assert_eq!(titles, ["Neuen Tab öffnen", "Neues Fenster öffnen", "Neues privates Fenster öffnen"]);
        assert_eq!(p.tasks[2].args, ["firefox", "--action", "new-private-window"]);
        assert_eq!(p.tasks[0].icon, IconRef::Task("new-tab".into()));
        assert_eq!(p.tasks[1].icon, IconRef::App, "no task icon was sent for new-window");
        let en = plan("firefox", &firefox_meta(), &JumpPrefs::default(), &[], &lang_candidates("en-US"), &BTreeSet::new());
        assert_eq!(en.tasks[0].title, "Open new tab");
        assert_eq!(en.tasks[2].title, "Open new private window");
        let fr = plan("firefox", &firefox_meta(), &JumpPrefs::default(), &[], &lang_candidates("fr-FR"), &BTreeSet::new());
        assert_eq!(fr.tasks[1].title, "Open new window", "English is the fallback");
    }

    #[test]
    fn another_app_s_tasks_are_its_desktop_actions_localised() {
        let out = format!(
            "desktop /a/x.desktop\nprogram xed\nurl no\nname {}\naction a {}\naction-l a de {}\naction b {}\n",
            b64(b"X"),
            b64(b"Alpha"),
            b64(b"Alpha DE"),
            b64(b"Beta")
        );
        let m = Meta::parse(&out).unwrap();
        let p = plan("x", &m, &JumpPrefs::default(), &[], &de(), &BTreeSet::new());
        let titles: Vec<&str> = p.tasks.iter().map(|l| l.title.as_str()).collect();
        assert_eq!(titles, ["Alpha DE", "Beta"]);
        assert!(p.categories.is_empty(), "an Exec= without %u has no pages");
    }

    fn page(kind: PageKind, url: &str, title: &str, icon: bool) -> Page {
        Page { kind, url: url.into(), title: title.into(), icon: icon.then(|| vec![1]) }
    }

    #[test]
    fn the_frequent_category_follows_firefox_s_prefs() {
        let pages: Vec<Page> = (0..10)
            .map(|i| page(PageKind::Frequent, &format!("https://e{i}.example/"), &format!("T{i}"), i % 2 == 0))
            .chain([
                page(PageKind::Frequent, "javascript:alert(1)", "bad", false),
                page(PageKind::Frequent, "https://a b/", "bad", false),
                page(PageKind::Recent, "https://r.example/", "", false),
            ])
            .collect();
        let removed: BTreeSet<String> = ["https://e1.example/".to_string()].into();
        let p = plan("firefox", &firefox_meta(), &JumpPrefs::default(), &pages, &de(), &removed);
        assert_eq!(p.categories.len(), 1);
        let (name, links) = &p.categories[0];
        assert_eq!(name, "Häufig");
        assert_eq!(links.len(), 7, "maxListItemCount");
        assert!(links.iter().all(|l| l.args[1] == "--url" && url_ok(&l.args[2])));
        assert!(!links.iter().any(|l| l.args[2] == "https://e1.example/"), "removed by the user");
        assert_eq!(links[0].icon, IconRef::Page(0));
        assert_eq!(links[1].icon, IconRef::Page(2), "e1 was removed, so e2 is second");
        assert_eq!(links[2].icon, IconRef::App, "e3 has no favicon");

        let recent = JumpPrefs { recent: true, ..JumpPrefs::default() };
        let p = plan("firefox", &firefox_meta(), &recent, &pages, &de(), &removed);
        assert_eq!(p.categories[1].0, "Kürzlich");
        assert_eq!(p.categories[1].1[0].title, "https://r.example/", "the URL stands in for no title");

        for off in [
            JumpPrefs { lists: false, ..JumpPrefs::default() },
            JumpPrefs { frequent: false, ..JumpPrefs::default() },
        ] {
            assert!(plan("firefox", &firefox_meta(), &off, &pages, &de(), &removed).categories.is_empty());
        }
        let no_tasks = JumpPrefs { tasks: false, ..JumpPrefs::default() };
        assert!(plan("firefox", &firefox_meta(), &no_tasks, &pages, &de(), &removed).tasks.is_empty());
    }

    #[test]
    fn the_host_s_frequent_list_parses() {
        let out = format!(
            "prefs lists=true frequent=true recent=false tasks=false max=3 refresh=60\n\
             item frequent {} {} {}\nitem recent {} - -\nitem odd x y z\ngarbage\n",
            b64(b"https://example.org/?a=1&b=2"),
            b64("Ein Titel mit Leerzeichen".as_bytes()),
            b64(&[0x89, b'P']),
            b64(b"file:///tmp/a.html"),
        );
        let (prefs, pages) = parse_frequent(&out);
        assert_eq!(prefs, JumpPrefs { lists: true, frequent: true, recent: false, tasks: false, max: 3, refresh_secs: 60 });
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].title, "Ein Titel mit Leerzeichen");
        assert_eq!(pages[1].kind, PageKind::Recent);
        assert!(pages[1].icon.is_none());
        assert_eq!(parse_frequent("prefs none\n"), (JumpPrefs::default(), vec![]));
    }

    #[test]
    fn windows_command_lines_quote_by_the_crt_s_rules() {
        assert_eq!(quote_arg("firefox"), "firefox");
        assert_eq!(quote_arg(r"C:\Program Files\x.exe"), r#""C:\Program Files\x.exe""#);
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_arg(r"a b\"), r#""a b\\""#);
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(
            command_line(&[r"C:\Users\u\wc ivs\wc-launch.exe".into(), "firefox".into()]),
            r#""C:\Users\u\wc ivs\wc-launch.exe" firefox"#
        );
    }

    #[test]
    fn the_relaunch_file_runs_the_launcher_with_the_section() {
        let r = relaunch(r"C:\Users\u\wcivs\wc-launch.exe", "firefox", "Firefox (Linux)", Some(r"C:\Users\u\wcivs\firefox.ico"));
        assert_eq!(r.command, r"C:\Users\u\wcivs\wc-launch.exe firefox");
        assert_eq!(r.icon, r"C:\Users\u\wcivs\firefox.ico,0");
        assert_eq!(taskbar::Relaunch::parse(&r.render()), Some(r));
    }

    #[test]
    fn removed_links_give_back_their_urls() {
        assert_eq!(url_of_args(r#"firefox --url "https://x.example/?a=1""#).as_deref(), Some("https://x.example/?a=1"));
        assert_eq!(url_of_args("firefox --url https://x.example/").as_deref(), Some("https://x.example/"));
        assert_eq!(url_of_args("firefox --action new-tab"), None);
        assert_eq!(parse_removed("a\n\n b \n").into_iter().collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn languages_are_tried_most_specific_first() {
        assert_eq!(lang_candidates("de-DE"), ["de-DE", "de_DE", "de"]);
        assert_eq!(lang_candidates("de-DE\0en-US\0"), ["de-DE", "de_DE", "de", "en-US", "en_US", "en"]);
    }
}
