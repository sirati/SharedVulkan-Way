//! Host to Linux, by type: what a Linux application claims on the host, so
//! that the host's own "Open with", a double-click and `open -a` reach it.
//!
//! * **Windows**: a per-user ProgID per application, `OpenWithProgids` under
//!   each extension it opens, registered Capabilities (so Settings' "Default
//!   apps" can choose it), and a URL protocol for any scheme the system does
//!   not already define. Everything is under HKCU, and every key or value is
//!   listed in a manifest, so uninstall removes exactly what install wrote.
//!   Nothing here takes a default away from the user's choice: Windows
//!   protects `UserChoice`, and so do we.
//! * **macOS**: `CFBundleDocumentTypes` and `CFBundleURLTypes` for the
//!   application's generated `.app` bundle (`macbundle::info_plist`).

use crate::mime;

/// What a Linux application opens, for registration on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxApp {
    /// The `[app.*]` section: the name the host asks Linux to run.
    pub section: String,
    /// The display name, "Firefox".
    pub name: String,
    pub mimes: Vec<String>,
    pub schemes: Vec<String>,
}

/// Schemes no Linux application gets on the host, whatever its desktop entry
/// claims: they belong to the host's shell, or run code.
pub const NEVER_CLAIMED: &[&str] = &[
    "file", "javascript", "vbscript", "data", "about", "blob", "res", "shell", "ms-settings", "search-ms",
    "ms-msdt", "ms-officecmd", "x-apple-systempreferences", "ms-appinstaller",
];

/// The schemes of `app` that may be registered.
pub fn claimable_schemes(app: &LinuxApp) -> Vec<String> {
    app.schemes.iter().filter(|s| !NEVER_CLAIMED.contains(&s.as_str()) && !s.starts_with("ms-")).cloned().collect()
}

// ------------------------------------------------------------------ Windows --

/// The ProgID of a Linux application: `Waycompose.Linux.<section>`, with the
/// section reduced to letters, digits and `-` (a ProgID allows little else,
/// and at most 39 characters before the version, which this stays under by
/// truncating).
pub fn progid(section: &str) -> String {
    let s: String = section.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).take(22).collect();
    format!("Waycompose.Linux.{s}")
}

/// One registry value under HKCU. `name` empty is the key's default value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegData {
    Str(String),
    /// A value whose presence is the point (`OpenWithProgids`, `URL Protocol`).
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegValue {
    /// Under HKEY_CURRENT_USER, backslash-separated.
    pub key: String,
    pub name: String,
    pub data: RegData,
}

/// What uninstall removes: a key and everything under it (one we created
/// whole), or one value in a key that is not ours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegUndo {
    Tree(String),
    Value { key: String, name: String },
}

/// What one application's registration writes, and how to take it back.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RegPlan {
    pub values: Vec<RegValue>,
    pub undo: Vec<RegUndo>,
    /// MIME types that have no extension in [`mime::TABLE`], so nothing on
    /// Windows can name them. Reported in the deploy log.
    pub unmapped: Vec<String>,
}

fn quote_arg(s: &str) -> String {
    format!("\"{s}\"")
}

/// The registration for one application. `launcher` is the absolute path of
/// `wc-launch.exe`, `icon` that of the application's `.ico`, and `install`
/// the installation's name (so two installations register apart).
/// `scheme_exists` says whether the system already defines a URL scheme
/// (`HKEY_CLASSES_ROOT\<scheme>` with `URL Protocol`); such a scheme is only
/// offered through Capabilities, never shadowed in HKCU.
pub fn windows_plan(
    app: &LinuxApp,
    launcher: &str,
    icon: &str,
    install: &str,
    scheme_exists: &dyn Fn(&str) -> bool,
) -> RegPlan {
    let mut p = RegPlan::default();
    let pid = progid(&format!("{install}.{}", app.section));
    let classes = "Software\\Classes";
    let pkey = format!("{classes}\\{pid}");
    let label = format!("{} (Linux)", app.name);
    let set = |p: &mut RegPlan, key: &str, name: &str, data: RegData| {
        p.values.push(RegValue { key: key.into(), name: name.into(), data })
    };
    // The command line names the application section and a placeholder. The
    // launcher maps the path and asks Linux to open it with that section, as
    // the `open` verb of the control channel (the section is the only name
    // Linux acts on, §94).
    let file_cmd = format!(
        "{} --install {} {} --file \"%1\"",
        quote_arg(launcher),
        quote_arg(install),
        quote_arg(&app.section)
    );
    let url_cmd = format!(
        "{} --install {} {} --url \"%1\"",
        quote_arg(launcher),
        quote_arg(install),
        quote_arg(&app.section)
    );

    set(&mut p, &pkey, "", RegData::Str(label.clone()));
    set(&mut p, &pkey, "FriendlyTypeName", RegData::Str(label.clone()));
    set(&mut p, &format!("{pkey}\\DefaultIcon"), "", RegData::Str(format!("{icon},0")));
    set(&mut p, &format!("{pkey}\\Application"), "ApplicationName", RegData::Str(label.clone()));
    set(&mut p, &format!("{pkey}\\Application"), "ApplicationIcon", RegData::Str(format!("{icon},0")));
    set(&mut p, &format!("{pkey}\\shell\\open"), "", RegData::Str(format!("Open with {label}")));
    set(&mut p, &format!("{pkey}\\shell\\open\\command"), "", RegData::Str(file_cmd));
    p.undo.push(RegUndo::Tree(pkey.clone()));

    // Capabilities, so Settings > Default apps lists the application and a
    // person can make it the default there.
    let cap = format!("Software\\Waycompose\\{install}\\{}\\Capabilities", app.section);
    set(&mut p, &cap, "ApplicationName", RegData::Str(label.clone()));
    set(&mut p, &cap, "ApplicationDescription", RegData::Str(format!("{} from Linux, through waycompose", app.name)));
    set(&mut p, &cap, "ApplicationIcon", RegData::Str(format!("{icon},0")));
    p.undo.push(RegUndo::Tree(format!("Software\\Waycompose\\{install}\\{}", app.section)));
    let regapps = "Software\\RegisteredApplications";
    let regname = format!("Waycompose.{install}.{}", app.section);
    set(&mut p, regapps, &regname, RegData::Str(cap.clone()));
    p.undo.push(RegUndo::Value { key: regapps.into(), name: regname });

    let mut exts: Vec<&str> = Vec::new();
    for m in &app.mimes {
        let es = mime::extensions_for(m);
        if es.is_empty() {
            p.unmapped.push(m.clone());
        }
        for e in es {
            if !exts.contains(e) {
                exts.push(e);
            }
        }
    }
    for e in &exts {
        let key = format!("{classes}\\.{e}\\OpenWithProgids");
        set(&mut p, &key, &pid, RegData::Empty);
        p.undo.push(RegUndo::Value { key, name: pid.clone() });
        set(&mut p, &format!("{cap}\\FileAssociations"), &format!(".{e}"), RegData::Str(pid.clone()));
    }

    let schemes = claimable_schemes(app);
    if !schemes.is_empty() {
        // A URL ProgID of its own: Windows sends URLs to it with the URL as %1.
        let upid = format!("{pid}.URL");
        let ukey = format!("{classes}\\{upid}");
        set(&mut p, &ukey, "", RegData::Str(format!("{label} URL")));
        set(&mut p, &ukey, "URL Protocol", RegData::Empty);
        set(&mut p, &format!("{ukey}\\DefaultIcon"), "", RegData::Str(format!("{icon},0")));
        set(&mut p, &format!("{ukey}\\shell\\open\\command"), "", RegData::Str(url_cmd.clone()));
        p.undo.push(RegUndo::Tree(ukey));
        for s in &schemes {
            set(&mut p, &format!("{cap}\\URLAssociations"), s, RegData::Str(upid.clone()));
            if !scheme_exists(s) {
                // A scheme nothing on this machine handles: it becomes ours,
                // in HKCU only, and goes again at uninstall.
                let skey = format!("{classes}\\{s}");
                set(&mut p, &skey, "", RegData::Str(format!("URL:{s}")));
                set(&mut p, &skey, "URL Protocol", RegData::Empty);
                set(&mut p, &format!("{skey}\\shell\\open\\command"), "", RegData::Str(url_cmd.clone()));
                p.undo.push(RegUndo::Tree(skey));
            }
        }
    }
    p
}

/// The manifest uninstall reads: one line per undo, `tree\t<key>` or
/// `value\t<key>\t<name>`. Written next to the installation by deploy.
pub fn manifest(undo: &[RegUndo]) -> String {
    let mut s = String::from("# waycompose host-integration registry manifest; uninstall removes exactly these\n");
    for u in undo {
        match u {
            RegUndo::Tree(k) => s.push_str(&format!("tree\t{k}\n")),
            RegUndo::Value { key, name } => s.push_str(&format!("value\t{key}\t{name}\n")),
        }
    }
    s
}

/// Read a [`manifest`] back. Only keys under `Software\Classes\Waycompose.`,
/// `Software\Waycompose\`, an extension's `OpenWithProgids`, a scheme key
/// ([`windows_plan`] wrote it), and `Software\RegisteredApplications` values
/// are accepted, so an edited manifest cannot make uninstall delete anything
/// else.
pub fn parse_manifest(text: &str) -> Vec<RegUndo> {
    let ours = |k: &str| {
        k.starts_with("Software\\Classes\\Waycompose.Linux.")
            || k.starts_with("Software\\Waycompose\\")
            || (k.starts_with("Software\\Classes\\.") && k.ends_with("\\OpenWithProgids"))
            || k == "Software\\RegisteredApplications"
            || k.strip_prefix("Software\\Classes\\").is_some_and(|s| {
                !s.is_empty()
                    && !s.contains('\\')
                    && s.starts_with(|c: char| c.is_ascii_alphabetic())
                    && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
                    && !matches!(s, "http" | "https" | "mailto" | "ftp" | "file" | "*" | "Directory" | "Folder")
            })
    };
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            match f.as_slice() {
                ["tree", k] if ours(k) && !k.ends_with("\\OpenWithProgids") && *k != "Software\\RegisteredApplications" => {
                    Some(RegUndo::Tree((*k).into()))
                }
                ["value", k, n] if ours(k) && !n.is_empty() => Some(RegUndo::Value { key: (*k).into(), name: (*n).into() }),
                _ => None,
            }
        })
        .collect()
}

// -------------------------------------------------------------------- macOS --

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The `CFBundleDocumentTypes` and `CFBundleURLTypes` keys for a Linux
/// application's bundle, as plist XML to put inside its top-level `<dict>`.
/// Empty when it opens nothing. The rank is `Alternate`: the bundle appears
/// in "Open With" and can be chosen as the default, and never takes a type
/// from the application the user already has for it.
pub fn plist_types(app: &LinuxApp, bundle_id: &str) -> String {
    let mut utis: Vec<&str> = Vec::new();
    let mut other: Vec<&str> = Vec::new();
    for m in &app.mimes {
        match mime::uti_for(m) {
            Some(u) if !utis.contains(&u) => utis.push(u),
            Some(_) => {}
            None => other.push(m),
        }
    }
    let mut s = String::new();
    if !utis.is_empty() || !other.is_empty() {
        s.push_str("\t<key>CFBundleDocumentTypes</key>\n\t<array>\n");
        if !utis.is_empty() {
            s.push_str("\t\t<dict>\n\t\t\t<key>CFBundleTypeName</key>\n");
            s.push_str(&format!("\t\t\t<string>{} document</string>\n", xml(&app.name)));
            s.push_str("\t\t\t<key>CFBundleTypeRole</key>\n\t\t\t<string>Editor</string>\n");
            s.push_str("\t\t\t<key>LSHandlerRank</key>\n\t\t\t<string>Alternate</string>\n");
            s.push_str("\t\t\t<key>LSItemContentTypes</key>\n\t\t\t<array>\n");
            for u in &utis {
                s.push_str(&format!("\t\t\t\t<string>{}</string>\n", xml(u)));
            }
            s.push_str("\t\t\t</array>\n\t\t</dict>\n");
        }
        if !other.is_empty() {
            // No UTI known: the MIME types themselves. Launch Services maps a
            // MIME type to a dynamic UTI and matches it.
            s.push_str("\t\t<dict>\n\t\t\t<key>CFBundleTypeName</key>\n");
            s.push_str(&format!("\t\t\t<string>{} file</string>\n", xml(&app.name)));
            s.push_str("\t\t\t<key>CFBundleTypeRole</key>\n\t\t\t<string>Editor</string>\n");
            s.push_str("\t\t\t<key>LSHandlerRank</key>\n\t\t\t<string>Alternate</string>\n");
            s.push_str("\t\t\t<key>CFBundleTypeMIMETypes</key>\n\t\t\t<array>\n");
            for m in &other {
                s.push_str(&format!("\t\t\t\t<string>{}</string>\n", xml(m)));
            }
            s.push_str("\t\t\t</array>\n\t\t</dict>\n");
        }
        s.push_str("\t</array>\n");
    }
    let schemes = claimable_schemes(app);
    if !schemes.is_empty() {
        s.push_str("\t<key>CFBundleURLTypes</key>\n\t<array>\n\t\t<dict>\n");
        s.push_str(&format!("\t\t\t<key>CFBundleURLName</key>\n\t\t\t<string>{}.url</string>\n", xml(bundle_id)));
        s.push_str("\t\t\t<key>CFBundleTypeRole</key>\n\t\t\t<string>Viewer</string>\n");
        s.push_str("\t\t\t<key>CFBundleURLSchemes</key>\n\t\t\t<array>\n");
        for sc in &schemes {
            s.push_str(&format!("\t\t\t\t<string>{}</string>\n", xml(sc)));
        }
        s.push_str("\t\t\t</array>\n\t\t</dict>\n\t</array>\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn firefox() -> LinuxApp {
        LinuxApp {
            section: "firefox".into(),
            name: "Firefox".into(),
            mimes: vec!["text/html".into(), "application/xhtml+xml".into(), "application/vnd.mozilla.xul+xml".into()],
            schemes: vec!["http".into(), "https".into(), "javascript".into()],
        }
    }

    #[test]
    fn progids() {
        assert_eq!(progid("firefox"), "Waycompose.Linux.firefox");
        assert_eq!(progid("wclinux.gnome text"), "Waycompose.Linux.wclinux-gnome-text");
        assert!(progid(&"x".repeat(80)).len() <= 39);
    }

    #[test]
    fn windows_registration_of_firefox() {
        let p = windows_plan(&firefox(), "C:\\wc\\wc-launch.exe", "C:\\wc\\icons\\firefox.ico", "wc", &|s| {
            s == "http" || s == "https"
        });
        let pid = "Waycompose.Linux.wc-firefox";
        let find = |k: &str, n: &str| p.values.iter().find(|v| v.key == k && v.name == n).map(|v| v.data.clone());
        assert_eq!(
            find(&format!("Software\\Classes\\{pid}\\shell\\open\\command"), ""),
            Some(RegData::Str(
                "\"C:\\wc\\wc-launch.exe\" --install \"wc\" \"firefox\" --file \"%1\"".into()
            ))
        );
        assert_eq!(find("Software\\Classes\\.html\\OpenWithProgids", pid), Some(RegData::Empty));
        assert_eq!(find("Software\\Classes\\.htm\\OpenWithProgids", pid), Some(RegData::Empty));
        assert_eq!(find("Software\\Classes\\.xhtml\\OpenWithProgids", pid), Some(RegData::Empty));
        // Never the extension's default value, never UserChoice.
        assert!(p.values.iter().all(|v| !(v.key == "Software\\Classes\\.html" && v.name.is_empty())));
        assert!(p.values.iter().all(|v| !v.key.contains("UserChoice")));
        // http/https exist on the system: offered through Capabilities only.
        assert!(p.values.iter().all(|v| v.key != "Software\\Classes\\http" && v.key != "Software\\Classes\\https"));
        assert_eq!(
            find("Software\\Waycompose\\wc\\firefox\\Capabilities\\URLAssociations", "https"),
            Some(RegData::Str(format!("{pid}.URL")))
        );
        // javascript: is never claimed.
        assert!(p.values.iter().all(|v| v.name != "javascript" && !v.key.ends_with("\\javascript")));
        assert_eq!(p.unmapped, ["application/vnd.mozilla.xul+xml"]);
        // Everything written is undone.
        for v in &p.values {
            let covered = p.undo.iter().any(|u| match u {
                RegUndo::Tree(k) => v.key == *k || v.key.starts_with(&format!("{k}\\")),
                RegUndo::Value { key, name } => *key == v.key && *name == v.name,
            });
            assert!(covered, "{v:?} is not undone");
        }
    }

    #[test]
    fn an_unknown_scheme_gets_a_key_of_its_own() {
        let mut a = firefox();
        a.schemes = vec!["irc".into()];
        let p = windows_plan(&a, "C:\\l.exe", "C:\\i.ico", "wc", &|_| false);
        assert!(p.values.iter().any(|v| v.key == "Software\\Classes\\irc" && v.name == "URL Protocol"));
        assert!(p.undo.contains(&RegUndo::Tree("Software\\Classes\\irc".into())));
    }

    #[test]
    fn manifest_round_trip_and_its_guard() {
        let p = windows_plan(&firefox(), "C:\\l.exe", "C:\\i.ico", "wc", &|_| true);
        assert_eq!(parse_manifest(&manifest(&p.undo)), p.undo);
        let evil = "tree\tSoftware\\Microsoft\\Windows\ntree\tSoftware\\Classes\\http\ntree\tSoftware\\Classes\\.txt\\OpenWithProgids\n\
                    value\tSoftware\\Classes\\.txt\\OpenWithProgids\tWaycompose.Linux.wc-x\ntree\tSoftware\\Classes\\Directory\n";
        assert_eq!(
            parse_manifest(evil),
            [RegUndo::Value { key: "Software\\Classes\\.txt\\OpenWithProgids".into(), name: "Waycompose.Linux.wc-x".into() }]
        );
    }

    #[test]
    fn plist_types_of_firefox() {
        let s = plist_types(&firefox(), "org.waycompose.linux.firefox");
        assert!(s.contains("<string>public.html</string>"));
        assert!(s.contains("<string>public.xhtml</string>"));
        assert!(s.contains("<key>CFBundleTypeMIMETypes</key>"));
        assert!(s.contains("<string>application/vnd.mozilla.xul+xml</string>"));
        assert!(s.contains("<string>Alternate</string>"));
        assert!(s.contains("<string>org.waycompose.linux.firefox.url</string>"));
        assert!(s.contains("<string>https</string>"));
        assert!(!s.contains("javascript"));
        let none = LinuxApp { section: "kgx".into(), name: "Console".into(), mimes: vec![], schemes: vec![] };
        assert_eq!(plist_types(&none, "x"), "");
    }
}
