//! A Linux application's desktop entry, read on the machine it is installed on:
//! what it is called, what it can do, and the argv each of those things runs.
//! docs/waycompose-design.md §92.
//!
//! # Why this is here and not on the guest
//!
//! The Windows side shows the application as a pinned button with a jump list,
//! and both are built from this file: the name, the icon, and the `Actions=`
//! ("New Window", "New Private Window"). The file exists only on the VM host, in
//! the package's own store path on NixOS (§28.6 found the icons there for the
//! same reason), so [`icons`](crate::icons) and this module do their lookups
//! on the same machine.
//!
//! # The guest names an action, never a command
//!
//! A jump-list task on the guest runs `wc-launch.exe firefox --action
//! new-private-window`, and what crosses to the host is the application's
//! desktop id and the action's id, each checked against a small alphabet
//! ([`valid_desktop_id`], [`valid_action_id`]). The argv is built **here**,
//! from this machine's desktop entry:
//!
//! * the program must be the one the entry's own `Exec=` names, by basename,
//!   and the one the launcher was configured to run ([`Command::program`]);
//! * the arguments are the action's `Exec=` with the program removed and its
//!   field codes expanded ([`expand`]); `%u` gets at most one URL, and only
//!   from [`open_command`], which checks it with [`url_ok`];
//! * nothing the guest sent reaches a shell. `wc-session` reads the result as
//!   an argv and never `eval`s it.
//!
//! A frequent-page item is the same: the guest sends a URL, never an argv, and
//! the URL becomes one argument in the slot the entry's own `%u` marks.
//! docs/security-model.md R33 has the argument in full.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One `[Desktop Action <id>]`, or one the launcher defines (see [`builtins`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub id: String,
    /// The unlocalised `Name=`.
    pub name: String,
    /// `Name[<locale>]=`, by locale as written (`de`, `de_DE`).
    pub names: BTreeMap<String, String>,
    pub icon: Option<String>,
    /// The raw `Exec=`, value escapes already undone.
    pub exec: String,
    /// Defined by this module rather than by the desktop entry.
    pub builtin: bool,
}

/// The `[Desktop Entry]` group and its actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub names: BTreeMap<String, String>,
    pub icon: Option<String>,
    pub wm_class: Option<String>,
    pub exec: String,
    /// In `Actions=` order; an id with no group is skipped.
    pub actions: Vec<Action>,
    /// §105. `MimeType=`, as listed (file types and `x-scheme-handler/...`).
    pub mime_types: Vec<String>,
}

/// What `wc-session` runs: a program, named by basename, and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The basename of the entry's own program. `wc-session` runs the
    /// launcher's executable for the application only if its basename is this.
    pub program: String,
    pub args: Vec<String>,
}

/// A desktop id: `firefox`, `org.gnome.Nautilus`. Letters, digits, `.`, `_`,
/// `-` and `+`, not starting with a dot, at most 128 bytes. It becomes a file
/// name under `applications/`, so no separator and no `..` can be in it.
pub fn valid_desktop_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && !s.contains("..")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// An action id, in the specification's alphabet: letters, digits and `-`.
pub fn valid_action_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Undo the escapes the specification defines for string values: `\s`, `\n`,
/// `\t`, `\r` and `\\`. Any other backslash is kept, because `Exec=` has its
/// own quoting rules on top ([`split_exec`]) and they need to see it.
fn unescape_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push_str("\\\\"),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Read a desktop entry. Groups other than `[Desktop Entry]` and
/// `[Desktop Action …]` are ignored, as are keys this module has no use for.
pub fn parse(path: &Path, text: &str) -> Result<Entry, String> {
    // group -> key -> value; the key keeps its `[locale]` suffix.
    let mut groups: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut group: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            group = rest.strip_suffix(']').map(str::to_string);
            continue;
        }
        let (Some(g), Some((k, v))) = (group.as_ref(), line.split_once('=')) else { continue };
        // The first value of a key wins; a repeated key is malformed, and
        // taking the first is what a reader that stops early would do.
        groups
            .entry(g.clone())
            .or_default()
            .entry(k.trim().to_string())
            .or_insert_with(|| unescape_value(v.trim()));
    }
    let main = groups
        .get("Desktop Entry")
        .ok_or_else(|| format!("{}: no [Desktop Entry] group", path.display()))?;
    let localised = |g: &BTreeMap<String, String>, key: &str| -> BTreeMap<String, String> {
        g.iter()
            .filter_map(|(k, v)| {
                let loc = k.strip_prefix(key)?.strip_prefix('[')?.strip_suffix(']')?;
                Some((loc.to_string(), v.clone()))
            })
            .collect()
    };
    let get = |g: &BTreeMap<String, String>, key: &str| g.get(key).filter(|v| !v.is_empty()).cloned();
    let exec = get(main, "Exec").ok_or_else(|| format!("{}: no Exec=", path.display()))?;
    let mut actions = Vec::new();
    for id in get(main, "Actions").unwrap_or_default().split(';') {
        let id = id.trim();
        if !valid_action_id(id) || actions.iter().any(|a: &Action| a.id == id) {
            continue;
        }
        let Some(g) = groups.get(&format!("Desktop Action {id}")) else { continue };
        let (Some(name), Some(aexec)) = (get(g, "Name"), get(g, "Exec")) else { continue };
        actions.push(Action {
            id: id.to_string(),
            name,
            names: localised(g, "Name"),
            icon: get(g, "Icon"),
            exec: aexec,
            builtin: false,
        });
    }
    Ok(Entry {
        path: path.to_path_buf(),
        name: get(main, "Name").unwrap_or_default(),
        names: localised(main, "Name"),
        icon: get(main, "Icon"),
        wm_class: get(main, "StartupWMClass"),
        exec,
        actions,
        mime_types: get(main, "MimeType")
            .unwrap_or_default()
            .split(';')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect(),
    })
}

/// `<dir>/applications/<id>.desktop` in the first directory that has one.
pub fn find(id: &str, dirs: &[PathBuf]) -> Result<Entry, String> {
    if !valid_desktop_id(id) {
        return Err(format!("{id:?} is not a desktop id"));
    }
    for d in dirs {
        let path = d.join("applications").join(format!("{id}.desktop"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            let mut e = parse(&path, &text)?;
            e.actions.extend(builtins(&e));
            return Ok(e);
        }
    }
    Err(format!("no {id}.desktop under any of {} data directories", dirs.len()))
}

/// Split an `Exec=` value into words, by the specification's rules: words are
/// separated by spaces, a word may be double-quoted, and inside quotes `"`,
/// `` ` ``, `$` and `\` are escaped with a backslash.
pub fn split_exec(exec: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut quoted = false;
    let mut it = exec.chars();
    while let Some(c) = it.next() {
        match (quoted, c) {
            (false, ' ' | '\t' | '\n') => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            (_, '"') => {
                quoted = !quoted;
                have = true;
            }
            (_, '\\') => match it.next() {
                Some(n) => {
                    cur.push(n);
                    have = true;
                }
                None => return Err("Exec= ends in a backslash".into()),
            },
            (_, c) => {
                cur.push(c);
                have = true;
            }
        }
    }
    if quoted {
        return Err("Exec= has an unterminated quote".into());
    }
    if have {
        out.push(cur);
    }
    if out.is_empty() {
        return Err("Exec= is empty".into());
    }
    Ok(out)
}

/// An argument `wc-session` can carry: one word, no whitespace or control
/// characters. Its `run` splits the command it is given on whitespace (into an
/// array, without globbing), so a word with a space in it would arrive as two.
pub fn safe_arg(s: &str) -> bool {
    !s.is_empty() && s.len() <= 4096 && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A URL the guest may ask the application to open.
///
/// `http`, `https` and `file` only, so the value can never begin with `-` and
/// be read as an option; no whitespace, control character, `"` or backslash,
/// so it is one word on both machines and one argument in a Windows command
/// line; at most 4096 bytes.
pub fn url_ok(u: &str) -> Result<(), String> {
    let lower = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file:///")) {
        return Err("only http://, https:// and file:/// URLs are opened".into());
    }
    if u.len() > 4096 {
        return Err("URL longer than 4096 bytes".into());
    }
    if u.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\' || !c.is_ascii()) {
        return Err("URL with whitespace, a control character, a quote, a backslash or non-ASCII".into());
    }
    Ok(())
}

/// Expand the field codes in an argv whose program has already been removed.
///
/// `%u %U %f %F` take `url` if there is one and vanish otherwise; `%i` is
/// `--icon <Icon=>` when the entry has an icon; `%c` the name; `%k` the entry's
/// path; `%%` a percent sign; the deprecated `%d %D %n %N %v %m` vanish. A
/// field code inside a longer word other than `%%` is refused, as is any other
/// letter: the specification calls both invalid, and a refused action is one
/// the guest does not offer rather than one that runs something unexpected.
pub fn expand(words: &[String], entry: &Entry, url: Option<&str>) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut url_used = false;
    for w in words {
        match w.as_str() {
            "%u" | "%U" | "%f" | "%F" => {
                if let Some(u) = url
                    && !url_used
                {
                    out.push(u.to_string());
                    url_used = true;
                }
                continue;
            }
            "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => continue,
            "%i" => {
                if let Some(i) = &entry.icon {
                    out.push("--icon".into());
                    out.push(i.clone());
                }
                continue;
            }
            "%c" => {
                out.push(entry.name.clone());
                continue;
            }
            "%k" => {
                out.push(entry.path.display().to_string());
                continue;
            }
            _ => {}
        }
        let mut s = String::with_capacity(w.len());
        let mut it = w.chars();
        while let Some(c) = it.next() {
            if c != '%' {
                s.push(c);
                continue;
            }
            match it.next() {
                Some('%') => s.push('%'),
                Some(o) => return Err(format!("field code %{o} inside the word {w:?}")),
                None => return Err(format!("a lone % at the end of {w:?}")),
            }
        }
        out.push(s);
    }
    if url.is_some() && !url_used {
        return Err("this application's Exec= takes no URL (%u, %U, %f or %F)".into());
    }
    for a in &out {
        if !safe_arg(a) {
            return Err(format!("argument {a:?} is not one word"));
        }
    }
    Ok(out)
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// The command for an `Exec=` value, with the program checked against the
/// entry's own.
fn command(entry: &Entry, exec: &str, url: Option<&str>) -> Result<Command, String> {
    let main = split_exec(&entry.exec)?;
    let program = basename(&main[0]).to_string();
    let words = split_exec(exec)?;
    if basename(&words[0]) != program {
        return Err(format!(
            "runs {:?}, not the application's own {program:?}",
            basename(&words[0])
        ));
    }
    Ok(Command { program, args: expand(&words[1..], entry, url)? })
}

/// The argv tail for one action, by id. Only the entry's own actions and
/// [`builtins`] have ids.
pub fn action_command(entry: &Entry, id: &str) -> Result<Command, String> {
    if !valid_action_id(id) {
        return Err(format!("{id:?} is not an action id"));
    }
    let a = entry
        .actions
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| format!("{} has no action {id:?}", entry.path.display()))?;
    command(entry, &a.exec, None)
}

/// The argv tail that opens `url`: the entry's main `Exec=`, with the URL in
/// the slot its `%u` (or `%U`, `%f`, `%F`) marks.
pub fn open_command(entry: &Entry, url: &str) -> Result<Command, String> {
    url_ok(url)?;
    command(entry, &entry.exec, Some(url))
}

/// Actions the launcher adds to an entry that lacks them.
///
/// One, today: **New Tab** for Firefox. Windows' own Firefox puts "Open new
/// tab" first in its jump list and the Linux desktop entry has no such action,
/// so a pinned Linux Firefox would be one task short of the native one. It is
/// defined here, on the host, as an argv in code, so it is still an id the
/// guest names and never a command it sends. Firefox's `--new-tab` opens its
/// argument in a new tab of the running window, and `about:newtab` is the page
/// Ctrl+T opens.
pub fn builtins(entry: &Entry) -> Vec<Action> {
    let Ok(main) = split_exec(&entry.exec) else { return Vec::new() };
    let program = basename(&main[0]).to_string();
    let mozilla = program == "firefox" || program.starts_with("firefox-");
    if !mozilla || entry.actions.iter().any(|a| a.id == "new-tab") {
        return Vec::new();
    }
    vec![Action {
        id: "new-tab".into(),
        name: "New Tab".into(),
        names: BTreeMap::new(),
        icon: None,
        exec: format!("{program} --new-tab about:newtab"),
        builtin: true,
    }]
}

// ---------------------------------------------------------------------------
// `wc-proxy desktop …`, the verb `wc-session` calls
// ---------------------------------------------------------------------------

fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            out.push(if i <= c.len() { A[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

/// What `info` prints, one fact per line, free text in base64 so that a name
/// with a space or a newline is still one word.
///
/// ```text
/// desktop /nix/store/…-firefox-157.0.1/share/applications/firefox.desktop
/// program firefox
/// url yes
/// name RmlyZWZveA==
/// name-l de RmlyZWZveA==
/// wmclass firefox
/// action new-private-window TmV3IFByaXZhdGUgV2luZG93
/// action-l new-private-window de …
/// action-icon new-private-window <Icon=>
/// unsupported profile-manager-window <reason, base64>
/// mime text/html
/// ```
pub fn info_lines(e: &Entry) -> Vec<String> {
    let mut out = vec![format!("desktop {}", e.path.display())];
    let main = split_exec(&e.exec).unwrap_or_default();
    if let Some(p) = main.first() {
        out.push(format!("program {}", basename(p)));
    }
    let takes_url = main.iter().any(|w| matches!(w.as_str(), "%u" | "%U" | "%f" | "%F"));
    out.push(format!("url {}", if takes_url { "yes" } else { "no" }));
    out.push(format!("name {}", b64(e.name.as_bytes())));
    for (l, n) in &e.names {
        if valid_action_id(&l.replace(['_', '@', '.'], "-")) {
            out.push(format!("name-l {l} {}", b64(n.as_bytes())));
        }
    }
    if let Some(w) = e.wm_class.as_ref().filter(|w| safe_arg(w)) {
        out.push(format!("wmclass {w}"));
    }
    // §105: what the guest registers the application for. One word each,
    // printable ASCII; the guest checks each again.
    for t in e.mime_types.iter().filter(|t| t.len() <= 255 && t.contains('/') && safe_arg(t)) {
        out.push(format!("mime {t}"));
    }
    for a in &e.actions {
        if let Err(why) = action_command(e, &a.id) {
            out.push(format!("unsupported {} {}", a.id, b64(why.as_bytes())));
            continue;
        }
        out.push(format!("action {} {}", a.id, b64(a.name.as_bytes())));
        for (l, n) in &a.names {
            if valid_action_id(&l.replace(['_', '@', '.'], "-")) {
                out.push(format!("action-l {} {l} {}", a.id, b64(n.as_bytes())));
            }
        }
        if let Some(i) = a.icon.as_ref().filter(|i| safe_arg(i)) {
            out.push(format!("action-icon {} {i}", a.id));
        }
    }
    out
}

/// `wc-proxy desktop [--dirs A:B] info|action|open …`, split into the
/// directories and the words.
///
/// §94: **options come first, and the first word that is not one ends them**
/// (as does `--`). Until §94 `--dirs` was looked for anywhere, so a word in a
/// positional slot that read `--dirs` — the URL of `open`, say — was taken as
/// the option and the next word as its value. `wc-session` passes the URL as
/// one argument and checks its scheme first, so this was not reachable from
/// the guest; it is closed here so that it does not depend on that.
pub fn split_args(args: &[String]) -> Result<(Vec<PathBuf>, Vec<String>), String> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "--dirs" => {
                let v = args.get(i + 1).ok_or("--dirs needs a value")?;
                dirs.extend(v.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
                i += 2;
            }
            "--" => {
                i += 1;
                break;
            }
            _ => break,
        }
    }
    Ok((dirs, args[i..].to_vec()))
}

/// `wc-proxy desktop --dirs A:B info ID`, `… action ID ACTION` and `… open ID
/// URL`. Exit status 1 with the reason on stderr when the answer is no.
pub fn cli(args: &[String]) -> std::process::ExitCode {
    let fail = |m: String| {
        eprintln!("wc-proxy desktop: {m}");
        std::process::ExitCode::from(1)
    };
    let (dirs, words) = match split_args(args) {
        Ok(v) => v,
        Err(e) => return fail(e.to_string()),
    };
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let entry = match words.get(1).map(|id| find(id, &dirs)) {
        Some(Ok(e)) => e,
        Some(Err(e)) => return fail(e),
        None => return fail("usage: desktop [--dirs A:B] info|action|open ID [ACTION|URL]".into()),
    };
    let cmd = match (words.first().copied(), words.get(2)) {
        (Some("info"), None) => {
            for l in info_lines(&entry) {
                println!("{l}");
            }
            return std::process::ExitCode::SUCCESS;
        }
        (Some("action"), Some(id)) => action_command(&entry, id),
        (Some("open"), Some(url)) => open_command(&entry, url),
        _ => return fail("usage: desktop [--dirs A:B] info|action|open ID [ACTION|URL]".into()),
    };
    match cmd {
        Ok(c) => {
            println!("program {}", c.program);
            println!("args {}", c.args.join(" "));
            std::process::ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Firefox's entry as nixpkgs ships it (firefox-157.0.1, testhost).
    const FIREFOX: &str = "[Desktop Entry]
Actions=new-private-window;new-window;profile-manager-window
Categories=Network;WebBrowser
Exec=firefox --name firefox %U
GenericName=Web Browser
Icon=firefox
MimeType=text/html;text/xml;x-scheme-handler/https;bad type;
Name=Firefox
Name[de]=Firefox
StartupWMClass=firefox
Terminal=false
Type=Application

[Desktop Action new-private-window]
Exec=firefox --private-window %U
Name=New Private Window
Name[de]=Neues privates Fenster

[Desktop Action new-window]
Exec=firefox --new-window %U
Name=New Window

[Desktop Action profile-manager-window]
Exec=firefox --ProfileManager
Name=Profile Manager
";

    fn firefox() -> Entry {
        let mut e = parse(Path::new("/s/firefox.desktop"), FIREFOX).unwrap();
        e.actions.extend(builtins(&e));
        e
    }

    #[test]
    fn firefox_s_entry_reads_as_its_name_class_and_actions() {
        let e = firefox();
        assert_eq!(e.name, "Firefox");
        assert_eq!(e.wm_class.as_deref(), Some("firefox"));
        assert_eq!(e.icon.as_deref(), Some("firefox"));
        let ids: Vec<&str> = e.actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["new-private-window", "new-window", "profile-manager-window", "new-tab"]);
        assert_eq!(e.actions[0].names.get("de").map(String::as_str), Some("Neues privates Fenster"));
        // The action groups' own keys never leak into the entry's.
        assert_eq!(e.names.get("de").map(String::as_str), Some("Firefox"));
    }

    /// The jump list's tasks, as argv tails: the action's own arguments with
    /// `%U` gone, because a task opens no file.
    #[test]
    fn each_action_becomes_its_own_arguments() {
        let e = firefox();
        let c = action_command(&e, "new-private-window").unwrap();
        assert_eq!(c, Command { program: "firefox".into(), args: vec!["--private-window".into()] });
        assert_eq!(action_command(&e, "new-window").unwrap().args, ["--new-window"]);
        assert_eq!(action_command(&e, "profile-manager-window").unwrap().args, ["--ProfileManager"]);
        assert_eq!(action_command(&e, "new-tab").unwrap().args, ["--new-tab", "about:newtab"]);
    }

    /// The whitelist: an id that is not in this machine's entry runs nothing,
    /// and an id outside the alphabet is refused before it is looked up.
    #[test]
    fn only_the_entry_s_own_action_ids_run() {
        let e = firefox();
        assert!(action_command(&e, "rm-rf").is_err());
        for bad in ["", "new-window;id", "../x", "new window", "new_window", "a$(id)", "x\n"] {
            assert!(action_command(&e, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_url_goes_into_the_slot_the_entry_marks() {
        let e = firefox();
        let c = open_command(&e, "https://example.org/a?b=1&c=$x#d").unwrap();
        assert_eq!(c.args, ["--name", "firefox", "https://example.org/a?b=1&c=$x#d"]);
        assert_eq!(open_command(&e, "file:///home/u/a.html").unwrap().args[2], "file:///home/u/a.html");
    }

    /// No option injection: a URL cannot start with `-`, and nothing that is
    /// not one word, one Windows argument and one of three schemes passes.
    #[test]
    fn urls_are_checked_strictly() {
        let e = firefox();
        for bad in [
            "--headless",
            "-P",
            "javascript:alert(1)",
            "data:text/html,x",
            "chrome://browser/content",
            "about:config",
            "file://host/share",
            "https://a b",
            "https://a\"b",
            "https://a\\b",
            "https://a\nb",
            "https://é.example",
            "",
        ] {
            assert!(open_command(&e, bad).is_err(), "{bad:?}");
        }
        assert!(open_command(&e, &format!("https://{}", "a".repeat(5000))).is_err());
        assert!(url_ok("HTTPS://EXAMPLE.ORG/").is_ok());
    }

    /// An entry whose `Exec=` takes no URL cannot be asked to open one.
    #[test]
    fn an_entry_without_a_url_slot_refuses_a_url() {
        let e = parse(Path::new("/x.desktop"), "[Desktop Entry]\nName=X\nExec=xterm\n").unwrap();
        assert!(open_command(&e, "https://example.org/").is_err());
    }

    #[test]
    fn field_codes_expand_by_the_specification() {
        let e = parse(
            Path::new("/a/app.desktop"),
            "[Desktop Entry]\nName=App\nIcon=app-icon\nExec=app %i %c %k 100%% %d %f\n",
        )
        .unwrap();
        let words = split_exec(&e.exec).unwrap();
        assert_eq!(
            expand(&words[1..], &e, None).unwrap(),
            ["--icon", "app-icon", "App", "/a/app.desktop", "100%"]
        );
        let bad = split_exec("app --x=%u").unwrap();
        assert!(expand(&bad[1..], &e, None).is_err());
        let bad = split_exec("app %z").unwrap();
        assert!(expand(&bad[1..], &e, None).is_err());
    }

    /// The specification's quoting, and the two value escapes that matter.
    #[test]
    fn exec_splits_by_the_specification_s_quoting() {
        assert_eq!(split_exec(r#"a "b c" d"#).unwrap(), ["a", "b c", "d"]);
        assert_eq!(split_exec(r#"a "x\"y\\z\$""#).unwrap(), ["a", r#"x"y\z$"#]);
        assert_eq!(split_exec(r#"a """#).unwrap(), ["a", ""]);
        assert!(split_exec(r#"a "b"#).is_err());
        assert!(split_exec("   ").is_err());
        assert_eq!(unescape_value(r"a\sb\\c"), r"a b\\c");
    }

    /// A word with a space in it would be split by `wc-session`, so an action
    /// that needs one is not offered rather than run wrong.
    #[test]
    fn an_action_whose_arguments_are_not_single_words_is_unsupported() {
        let e = parse(
            Path::new("/a/x.desktop"),
            "[Desktop Entry]\nName=X\nExec=x %U\nActions=a;b;c\n\
             [Desktop Action a]\nName=A\nExec=x \"two words\"\n\
             [Desktop Action b]\nName=B\nExec=/usr/bin/other --x\n\
             [Desktop Action c]\nName=C\nExec=/opt/x/bin/x --c\n",
        )
        .unwrap();
        assert!(action_command(&e, "a").is_err());
        assert!(action_command(&e, "b").is_err(), "a different program");
        assert_eq!(action_command(&e, "c").unwrap().args, ["--c"]);
        let info = info_lines(&e).join("\n");
        assert!(info.contains("unsupported a "), "{info}");
        assert!(info.contains("unsupported b "), "{info}");
        assert!(info.contains("action c "), "{info}");
    }

    #[test]
    fn info_says_what_the_guest_needs() {
        let info = info_lines(&firefox());
        assert!(info.contains(&"program firefox".to_string()));
        assert!(info.contains(&"url yes".to_string()));
        assert!(info.contains(&"wmclass firefox".to_string()));
        assert!(info.contains(&format!("action new-window {}", b64(b"New Window"))));
        assert!(info.contains(&format!("action-l new-private-window de {}", b64("Neues privates Fenster".as_bytes()))));
        assert!(info.contains(&format!("action new-tab {}", b64(b"New Tab"))));
        // §105: the types, one per line; one with a space is not passed on.
        let mimes: Vec<&String> = info.iter().filter(|l| l.starts_with("mime ")).collect();
        assert_eq!(mimes, ["mime text/html", "mime text/xml", "mime x-scheme-handler/https"]);
    }

    #[test]
    fn desktop_ids_cannot_leave_the_applications_directory() {
        assert!(valid_desktop_id("firefox"));
        assert!(valid_desktop_id("org.gnome.Nautilus"));
        for bad in ["", "../firefox", "a/b", ".hidden", "a..b", "a b", "a\\b"] {
            assert!(!valid_desktop_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn find_reads_the_first_directory_that_has_the_entry() {
        let root = std::env::temp_dir().join(format!("wc-desktop-test-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(b.join("applications")).unwrap();
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(b.join("applications/firefox.desktop"), FIREFOX).unwrap();
        let e = find("firefox", &[a, b.clone()]).unwrap();
        assert_eq!(e.path, b.join("applications/firefox.desktop"));
        assert!(e.actions.iter().any(|x| x.id == "new-tab"));
        assert!(find("../b/applications/firefox", std::slice::from_ref(&b)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// §94. An option is only an option before the first word, so a URL or
    /// an id in its slot that reads `--dirs` stays a word.
    #[test]
    fn options_end_at_the_first_word() {
        let v = |a: &[&str]| split_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let (d, w) = v(&["--dirs", "/a:/b", "open", "firefox", "--dirs", "/evil"]).unwrap();
        assert_eq!(d, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(w, ["open", "firefox", "--dirs", "/evil"]);
        let (d, w) = v(&["info", "--dirs", "/evil"]).unwrap();
        assert!(d.is_empty());
        assert_eq!(w, ["info", "--dirs", "/evil"]);
        assert_eq!(v(&["--dirs", "/a", "--", "--dirs"]).unwrap().1, ["--dirs"]);
        assert!(v(&["--dirs"]).is_err());
    }
}
