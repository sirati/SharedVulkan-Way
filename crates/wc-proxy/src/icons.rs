//! An application id, on this side of the boundary, becomes pixels.
//!
//! # Why the resolution is here and can be nowhere else
//!
//! `xdg_toplevel.set_app_id` carries a *desktop entry name* — `"firefox"` —
//! and that is all Wayland has ever had to say about icons. What it names is a
//! `.desktop` file in `$XDG_DATA_DIRS/applications`, whose `Icon=` key names an
//! entry in an icon theme, which is a directory tree of PNGs. All three of
//! those live on the machine the *client* is on.
//!
//! In this project's topology that machine is not the one with the taskbar on
//! it. The compositor is a Windows binary on the other side of a hypervisor
//! with no `$XDG_DATA_DIRS`, no icon theme and no way to open a Linux path; the
//! only process that can answer "what does firefox look like" is this one. So
//! the lookup happens here, the result crosses as **pixels** through
//! `wc_icon_manager_v1` (see [`crate::iconlink`]), and the compositor does the
//! part that needs Win32.
//!
//! # What is implemented of the icon theme specification, and what is not
//!
//! Enough to find the file every real theme puts where the specification says,
//! and no more:
//!
//! * `$XDG_DATA_HOME` then `$XDG_DATA_DIRS` then `/usr/share`, in order.
//! * The named theme, then everything its `index.theme` `Inherits`, then
//!   `hicolor`, which the specification makes the mandatory fallback.
//! * Inside a theme, **the directory tree is read rather than `index.theme`'s
//!   `Directories` list**. `48x48/apps/firefox.png` and `apps/48/firefox.png`
//!   are both found, because the size is taken from whichever path component
//!   looks like one. Parsing `index.theme` properly would be more correct and
//!   would find nothing this does not: the sizes are in the paths because the
//!   specification tells themes to put them there.
//! * `/usr/share/pixmaps`, which predates the specification and is where a
//!   surprising number of applications still are.
//!
//! **Not implemented: SVG.** A scalable icon needs a renderer, and a renderer
//! is a dependency this crate does not have. A theme with only SVG resolves to
//! nothing, which degrades to the generic Windows icon — recorded in
//! docs/waycompose-design.md §16.4 rather than hidden here, because "Adwaita is
//! SVG-first" is exactly the kind of thing that makes a feature look broken on
//! somebody else's machine.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One decoded image, in the layout `wc_icon_manager_v1` requires: BGRA,
/// premultiplied, tightly packed, top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

/// What was found, and how — the provenance is for the log, and the log is
/// what makes a wrong icon debuggable from a photograph.
#[derive(Debug)]
pub struct Resolution {
    pub app_id: String,
    /// The `Icon=` name, or the app id if no desktop entry was found.
    pub icon_name: String,
    /// Where the desktop entry came from, if there was one.
    pub desktop: Option<PathBuf>,
    /// The files that were decoded, largest first.
    pub files: Vec<PathBuf>,
    pub images: Vec<Image>,
}

impl Resolution {
    /// One line for the log, saying which of the several ways this could have
    /// gone actually happened.
    pub fn summary(&self) -> String {
        if self.images.is_empty() {
            let why = match &self.desktop {
                Some(d) => format!(
                    "desktop entry {} names icon {:?}, and no theme has it as a PNG",
                    d.display(),
                    self.icon_name
                ),
                None => format!(
                    "no {}.desktop in any data directory, and no theme has {:?} either",
                    self.app_id, self.icon_name
                ),
            };
            return format!("app id {:?}: nothing to show ({why})", self.app_id);
        }
        format!(
            "app id {:?}: icon {:?} -> {} image(s) [{}] from {}",
            self.app_id,
            self.icon_name,
            self.images.len(),
            self.images
                .iter()
                .map(|i| format!("{}x{}", i.width, i.height))
                .collect::<Vec<_>>()
                .join(" "),
            self.files
                .first()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        )
    }
}

/// The sizes asked for, which are the sizes Windows uses: `SM_CXSMICON`,
/// `SM_CXICON`, and one bigger for a scaled display.
pub const WANTED: [u32; 3] = [16, 32, 48];

/// How to look, so that a test can point the whole thing at a directory it
/// built rather than at the machine's real theme.
#[derive(Debug, Clone)]
pub struct Search {
    /// `$XDG_DATA_HOME` and `$XDG_DATA_DIRS`, in order.
    pub data_dirs: Vec<PathBuf>,
    /// The theme to try before `hicolor`.
    pub theme: String,
}

impl Default for Search {
    fn default() -> Search {
        Search {
            data_dirs: default_data_dirs(),
            theme: std::env::var("WC_ICON_THEME").unwrap_or_else(|_| "hicolor".into()),
        }
    }
}

/// `$XDG_DATA_HOME` (or `~/.local/share`), then `$XDG_DATA_DIRS` (or the
/// specification's default), de-duplicated and kept in order.
pub fn default_data_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME") {
        out.push(PathBuf::from(home));
    } else if let Some(home) = std::env::var_os("HOME") {
        out.push(PathBuf::from(home).join(".local/share"));
    }
    let dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in dirs.split(':').filter(|s| !s.is_empty()) {
        out.push(PathBuf::from(d));
    }
    let mut seen = HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// How far up from the client's executable to look for a `share` directory.
///
/// Four is chosen against real layouts rather than as a round number:
/// `/usr/bin/foo` needs one, `/usr/lib/firefox/firefox` needs three, and a Nix
/// store path of the same shape needs three. Going further starts adding `/` to
/// the search, which would put every `.desktop` file on the machine into a
/// lookup that is already the expensive case when it misses (§16.4).
const CLIENT_PREFIX_DEPTH: usize = 4;

/// The data directories belonging to whatever `exe` was installed as.
///
/// ADDED FOR STAGE 13, and it is the fix for `0 image(s)` on the machine
/// `docs/goal-firefox.md` describes. §17.2 recorded the cause as *"the resolver
/// runs where the proxy runs, and the VM host has no icon theme carrying
/// Firefox's pixels"*. The first half is right and the second is the wrong
/// diagnosis: the proxy is a Wayland **server** reached over an AF_UNIX socket,
/// so it is always on the client's own machine, and that machine is by
/// definition the one the application is installed on. What was missing was not
/// the machine but the *directory* — Firefox's `.desktop` and its icons are in
/// its own package prefix, and on NixOS that prefix is a store path which
/// appears in `XDG_DATA_DIRS` only if somebody put the package in a profile.
/// Measured: `/run/current-system/sw/share` on the VM host has no
/// `firefox.desktop` and no `firefox.png`, and
/// `/nix/store/…-firefox-151.0.2/share` has both. See docs/waycompose-design.md §18.5.
///
/// So: walk up from the executable and take every ancestor that has a `share`.
/// That is the FHS relationship between `bin/` and `share/`, it is the same
/// relationship inside a Nix store path, inside a Flatpak, and inside anything
/// unpacked into a home directory — which is why it is preferred to a list of
/// special cases for package managers.
///
/// Root (`/`) is never a candidate: `/share` is not a thing, and adding it
/// would make a client installed at `/bin/x` search the whole filesystem root.
pub fn client_prefix_dirs(exe: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dir = exe.parent();
    for _ in 0..CLIENT_PREFIX_DEPTH {
        let Some(d) = dir else { break };
        if d.parent().is_none() {
            break;
        }
        let share = d.join("share");
        if share.is_dir() && !out.contains(&share) {
            out.push(share);
        }
        dir = d.parent();
    }
    out
}

/// The same, for a live process id.
///
/// `/proc/<pid>/exe` is the kernel's answer and not the client's: it survives
/// an `argv[0]` that lies, a wrapper script that `exec`s something else, and a
/// binary that has since been replaced on disk. Unreadable for a process
/// belonging to another user, which is a `None` and not an error — the icon is
/// cosmetic and the fallbacks below it are the whole design (§16.7).
///
/// **No descriptor crosses anything here.** This is a `readlink` on the
/// proxy's own machine, in the proxy's own namespace, and what leaves the
/// function is a path. §14.7's count is unchanged.
#[cfg(unix)]
pub fn dirs_for_pid(pid: i32) -> Vec<PathBuf> {
    match std::fs::read_link(format!("/proc/{pid}/exe")) {
        Ok(exe) => client_prefix_dirs(&exe),
        Err(_) => Vec::new(),
    }
}

/// The whole lookup: app id in, pixels out.
pub fn resolve(app_id: &str, search: &Search) -> Resolution {
    let (desktop, icon_name) = match desktop_entry(app_id, search) {
        Some((path, name)) => (Some(path), name),
        // No desktop entry is not the end of it. An application's icon is very
        // often installed under its own app id whether or not anything ships a
        // `.desktop` for it, and looking is free.
        None => (None, app_id.to_owned()),
    };

    let candidates = icon_files(&icon_name, search);
    let mut files = Vec::new();
    let mut images = Vec::new();
    for want in WANTED {
        let Some((size, path)) = pick(&candidates, want) else {
            continue;
        };
        // The same file answers two sizes often enough — a theme with only
        // 48x48 answers all three — and decoding it twice would be wasted work
        // and a duplicate on the wire.
        if files.contains(path) {
            continue;
        }
        match decode_png(path) {
            Ok(image) => {
                let _ = size;
                files.push(path.clone());
                images.push(image);
            }
            Err(e) => eprintln!("[proxy] icon {}: {e}", path.display()),
        }
    }
    images.sort_by_key(|i| std::cmp::Reverse(i.width.max(i.height)));
    files.sort();
    Resolution {
        app_id: app_id.to_owned(),
        icon_name,
        desktop,
        files,
        images,
    }
}

/// Find `<app_id>.desktop` and read its `Icon=`.
///
/// Three tries, in the order a taskbar would: the name as given, the name
/// lower-cased, and finally any entry whose `StartupWMClass` is this app id —
/// which is the key that exists precisely because an application's window class
/// and its desktop file often disagree, and which is what a Linux panel uses to
/// match a window to a launcher.
fn desktop_entry(app_id: &str, search: &Search) -> Option<(PathBuf, String)> {
    if app_id.is_empty() || app_id.contains('/') {
        return None;
    }
    let names = [format!("{app_id}.desktop"), format!("{}.desktop", app_id.to_lowercase())];
    for dir in &search.data_dirs {
        let apps = dir.join("applications");
        for name in &names {
            let path = apps.join(name);
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Some(icon) = entry_key(&text, "Icon") {
                    return Some((path, icon));
                }
        }
    }
    for dir in &search.data_dirs {
        let Ok(entries) = std::fs::read_dir(dir.join("applications")) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if entry_key(&text, "StartupWMClass").as_deref() == Some(app_id)
                && let Some(icon) = entry_key(&text, "Icon") {
                    return Some((path, icon));
                }
        }
    }
    None
}

/// Read one key out of a desktop entry's `[Desktop Entry]` group.
///
/// Deliberately not a full INI parser: later groups (`[Desktop Action new-window]`
/// and friends) have their own `Icon=` and taking the first match in the file
/// regardless of group is how a launcher ends up with the "New Private Window"
/// icon. Firefox's own desktop file on the machine this was written on has
/// three such groups.
pub fn entry_key(text: &str, key: &str) -> Option<String> {
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        // `Icon[de]=` is a localised value; the unlocalised one is what an icon
        // theme is keyed by, so anything with a bracket is skipped.
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == key {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        }
    }
    None
}

/// Every PNG in every theme that could be this icon, as (size, path).
///
/// A `/` or a leading `.` in the name is refused outright: the name comes off
/// the wire from a client, and joining it onto a directory would otherwise let
/// `../../etc/something.png` be an icon.
fn icon_files(name: &str, search: &Search) -> Vec<(u32, PathBuf)> {
    let mut out = Vec::new();
    if name.is_empty() {
        return out;
    }
    // An absolute path is legal in a desktop entry's `Icon=` and means exactly
    // itself.
    let as_path = Path::new(name);
    if as_path.is_absolute() {
        if as_path.extension().and_then(|e| e.to_str()) == Some("png") && as_path.is_file() {
            out.push((0, as_path.to_owned()));
        }
        return out;
    }
    if name.contains('/') || name.starts_with('.') {
        return out;
    }

    let file = format!("{name}.png");
    for theme in theme_chain(&search.theme, search) {
        for dir in &search.data_dirs {
            let root = dir.join("icons").join(&theme);
            collect(&root, &file, 0, &mut out);
        }
    }
    for dir in &search.data_dirs {
        let p = dir.join("pixmaps").join(&file);
        if p.is_file() {
            // Size unknown: `pixmaps` has no size in its layout, so it is
            // treated as a last resort by [`pick`] rather than as a size 0.
            out.push((0, p));
        }
    }
    out
}

/// The theme, what it inherits, and `hicolor` last.
fn theme_chain(theme: &str, search: &Search) -> Vec<String> {
    let mut chain = Vec::new();
    let mut queue = vec![theme.to_owned()];
    while let Some(next) = queue.pop() {
        if next.is_empty() || chain.contains(&next) || chain.len() > 8 {
            continue;
        }
        chain.push(next.clone());
        for dir in &search.data_dirs {
            let index = dir.join("icons").join(&next).join("index.theme");
            let Ok(text) = std::fs::read_to_string(&index) else {
                continue;
            };
            if let Some(parents) = ini_key(&text, "Inherits") {
                for p in parents.split(',').rev() {
                    queue.push(p.trim().to_owned());
                }
            }
            break;
        }
    }
    if !chain.iter().any(|t| t == "hicolor") {
        chain.push("hicolor".into());
    }
    chain
}

/// The first `key=` in a file, whatever group it is in. Only used for
/// `index.theme`'s `Inherits`, which is always in `[Icon Theme]` and first.
fn ini_key(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_owned())
}

/// Walk a theme directory looking for `file`, taking the size from whichever
/// path component looks like one (`48x48`, `48`, `scalable`).
fn collect(dir: &Path, file: &str, depth: usize, out: &mut Vec<(u32, PathBuf)>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            collect(&path, file, depth + 1, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some(file) {
            out.push((size_from_path(&path), path));
        }
    }
}

/// Pull a pixel size out of a path like `.../hicolor/48x48/apps/firefox.png`.
/// 0 means "no size in the path", which [`pick`] treats as a last resort.
fn size_from_path(path: &Path) -> u32 {
    for part in path.iter().rev().skip(1) {
        let Some(part) = part.to_str() else { continue };
        let head = part.split(['x', '@']).next().unwrap_or("");
        if let Ok(n) = head.parse::<u32>()
            && (8..=1024).contains(&n) {
                return n;
            }
    }
    0
}

/// The best file for a target size: exact, else the smallest that is bigger,
/// else the biggest that is smaller, else an unsized one.
///
/// The same order [`crate::iconlink`]'s peer uses on the far side, and for the
/// same reason: scaling an icon down keeps its shape and scaling one up does
/// not.
fn pick(candidates: &[(u32, PathBuf)], want: u32) -> Option<(u32, &PathBuf)> {
    let sized: Vec<&(u32, PathBuf)> = candidates.iter().filter(|(s, _)| *s > 0).collect();
    let exact = sized.iter().find(|(s, _)| *s == want);
    let bigger = sized.iter().filter(|(s, _)| *s > want).min_by_key(|(s, _)| *s);
    let smaller = sized.iter().filter(|(s, _)| *s < want).max_by_key(|(s, _)| *s);
    let chosen = exact.or(bigger).or(smaller);
    match chosen {
        Some((s, p)) => Some((*s, p)),
        None => candidates.first().map(|(s, p)| (*s, p)),
    }
}

/// Decode a PNG into premultiplied BGRA.
///
/// Two conversions, both of which are silent bugs if they are missed:
///
/// * **RGBA to BGRA.** A blue Firefox logo coming out orange is the tell.
/// * **Straight alpha to premultiplied.** PNG stores colour independent of
///   alpha; `wl_shm`'s ARGB8888, this project's wire format and Windows'
///   `AlphaBlend` all want it multiplied through. Missing it shows as a pale
///   fringe on the anti-aliased edge rather than as an error, which is why it
///   is asserted in a test rather than left to a photograph.
pub fn decode_png(path: &Path) -> Result<Image, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{e}"))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    // Palette, grey and 16-bit all become 8-bit RGB or RGBA here, so the match
    // below has four cases instead of the format's dozen.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| format!("{e}"))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "image is too large to decode".to_string())?;
    if size > 8 << 20 {
        return Err(format!("{size} bytes is not an icon"));
    }
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("{e}"))?;
    let (w, h) = (info.width as usize, info.height as usize);
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        other => return Err(format!("colour type {other:?} is not handled")),
    };
    let mut pixels = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = &buf[y * info.line_size..(y + 1) * info.line_size];
        for x in 0..w {
            let px = &row[x * channels..(x + 1) * channels];
            let (r, g, b, a) = match channels {
                4 => (px[0], px[1], px[2], px[3]),
                3 => (px[0], px[1], px[2], 0xFF),
                2 => (px[0], px[0], px[0], px[1]),
                _ => (px[0], px[0], px[0], 0xFF),
            };
            let mul = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
            pixels.extend_from_slice(&[mul(b), mul(g), mul(r), a]);
        }
    }
    Ok(Image {
        width: info.width as i32,
        height: info.height as i32,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_entry_group_is_the_only_one_read() {
        // Firefox's own file has `[Desktop Action new-window]` groups with
        // their own keys; a parser that took the first `Icon=` anywhere would
        // give a window the "New Private Window" icon on some machines and the
        // right one on others.
        let text = "[Desktop Entry]\nName=Firefox\nIcon=firefox\n\
                    \n[Desktop Action new-private-window]\nName=Private\nIcon=firefox-private\n";
        assert_eq!(entry_key(text, "Icon").as_deref(), Some("firefox"));
        assert_eq!(entry_key(text, "Name").as_deref(), Some("Firefox"));
        assert_eq!(entry_key(text, "Nothing"), None);
    }

    #[test]
    fn a_size_is_taken_from_whichever_path_component_has_one() {
        assert_eq!(size_from_path(Path::new("/a/hicolor/48x48/apps/f.png")), 48);
        assert_eq!(size_from_path(Path::new("/a/hicolor/apps/32/f.png")), 32);
        assert_eq!(size_from_path(Path::new("/a/hicolor/64x64@2/apps/f.png")), 64);
        assert_eq!(size_from_path(Path::new("/a/hicolor/scalable/apps/f.png")), 0);
        assert_eq!(size_from_path(Path::new("/usr/share/pixmaps/f.png")), 0);
    }

    #[test]
    fn picking_prefers_exact_then_bigger_then_smaller() {
        let c = |sizes: &[u32]| -> Vec<(u32, PathBuf)> {
            sizes.iter().map(|s| (*s, PathBuf::from(format!("{s}.png")))).collect()
        };
        let set = c(&[16, 32, 128]);
        assert_eq!(pick(&set, 32).unwrap().0, 32);
        assert_eq!(pick(&set, 48).unwrap().0, 128, "bigger beats smaller");
        assert_eq!(pick(&set, 256).unwrap().0, 128, "nothing bigger exists");
        assert!(pick(&[], 32).is_none());
        // An unsized file is a last resort and not a size-0 match.
        let mixed = vec![(0u32, PathBuf::from("pixmaps.png")), (48, PathBuf::from("48.png"))];
        assert_eq!(pick(&mixed, 16).unwrap().0, 48);
    }

    #[test]
    fn hicolor_is_always_in_the_chain_even_if_the_theme_is_not_there() {
        let search = Search {
            data_dirs: vec![PathBuf::from("/nonexistent")],
            theme: "Adwaita".into(),
        };
        let chain = theme_chain(&search.theme, &search);
        assert_eq!(chain, vec!["Adwaita".to_string(), "hicolor".to_string()]);
    }

    #[test]
    fn a_name_that_is_a_path_traversal_finds_nothing() {
        let search = Search {
            data_dirs: vec![PathBuf::from("/usr/share")],
            theme: "hicolor".into(),
        };
        assert!(icon_files("../../etc/passwd", &search).is_empty());
        assert!(icon_files(".hidden", &search).is_empty());
        assert!(icon_files("", &search).is_empty());
    }

    /// The end-to-end lookup, against a theme built in a temporary directory —
    /// so it asserts the same code path the guest runs without depending on
    /// what happens to be installed on the machine running the tests.
    /// STAGE 13. The three layouts this has to work on, in one test, because
    /// the walk depth is the only thing that separates them and a depth chosen
    /// by eye is a depth that is wrong for the fourth one.
    ///
    /// `/usr/bin/foo` is FHS, `/usr/lib/firefox/firefox` is how a distribution
    /// actually ships a browser, and the Nix store path is the case that
    /// produced the bug — Firefox's own prefix, three levels above the binary
    /// the kernel reports for the process.
    #[test]
    fn the_client_prefix_is_found_from_bin_and_from_lib() {
        let root = std::env::temp_dir().join(format!("wc-prefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for p in [
            "usr/bin",
            "usr/share",
            "usr/lib/firefox",
            "store/firefox-151.0.2/lib/firefox",
            "store/firefox-151.0.2/share",
        ] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }

        // FHS, and `/usr/lib/firefox/firefox` reaches the same `share` from
        // one level deeper.
        assert_eq!(
            client_prefix_dirs(&root.join("usr/bin/foo")),
            vec![root.join("usr/share")]
        );
        assert_eq!(
            client_prefix_dirs(&root.join("usr/lib/firefox/firefox")),
            vec![root.join("usr/share")]
        );
        // The store path: the prefix is three levels up and it is the only
        // place `firefox.desktop` exists on the machine this was measured on.
        assert_eq!(
            client_prefix_dirs(&root.join("store/firefox-151.0.2/lib/firefox/firefox")),
            vec![root.join("store/firefox-151.0.2/share")]
        );
        // A binary with no `share` anywhere above it contributes nothing,
        // rather than contributing a directory that does not exist.
        assert!(client_prefix_dirs(&root.join("store/nothing/bin/x")).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_desktop_entry_and_a_theme_become_pixels() {
        let root = std::env::temp_dir().join(format!("wc-icons-{}", std::process::id()));
        let apps = root.join("applications");
        let theme = root.join("icons/hicolor/32x32/apps");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::create_dir_all(&theme).unwrap();
        std::fs::write(
            apps.join("wc-test-app.desktop"),
            "[Desktop Entry]\nName=Test\nIcon=wc-test-icon\n",
        )
        .unwrap();
        // A 2x1 PNG with one opaque and one half-transparent pixel, written by
        // hand so the test does not need an encoder.
        std::fs::write(theme.join("wc-test-icon.png"), tiny_png()).unwrap();

        let search = Search {
            data_dirs: vec![root.clone()],
            theme: "hicolor".into(),
        };
        let r = resolve("wc-test-app", &search);
        assert_eq!(r.icon_name, "wc-test-icon");
        assert!(r.desktop.is_some(), "the entry was found");
        assert_eq!(r.images.len(), 1, "one file answers all three sizes: {}", r.summary());
        let img = &r.images[0];
        assert_eq!((img.width, img.height), (2, 1));
        // Red, opaque -> BGRA 0,0,255,255. Then red at half alpha,
        // premultiplied -> 0,0,128,128 rather than 0,0,255,128.
        assert_eq!(&img.pixels[0..4], &[0, 0, 255, 255]);
        assert_eq!(img.pixels[7], 128, "alpha survives");
        assert_eq!(img.pixels[6], 128, "and the colour is premultiplied by it");

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A 2x1 RGBA PNG: red opaque, red at alpha 128. Built here rather than
    /// checked in as a binary blob so that what it contains is readable.
    fn tiny_png() -> Vec<u8> {
        fn chunk(kind: &[u8], data: &[u8]) -> Vec<u8> {
            let mut out = (data.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let mut crc = 0xFFFF_FFFFu32;
            for b in kind.iter().chain(data) {
                crc ^= *b as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
                }
            }
            out.extend_from_slice(&(!crc).to_be_bytes());
            out
        }
        // zlib with stored (uncompressed) deflate blocks, so no encoder is
        // needed: one filter byte then two RGBA pixels.
        let raw = [0u8, 255, 0, 0, 255, 255, 0, 0, 128];
        let mut z = vec![0x78, 0x01, 0x01, raw.len() as u8, 0x00, !(raw.len() as u8), 0xFF];
        z.extend_from_slice(&raw);
        let (mut a, mut b) = (1u32, 0u32);
        for byte in raw {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());

        let mut ihdr = 2u32.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);

        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend(chunk(b"IHDR", &ihdr));
        png.extend(chunk(b"IDAT", &z));
        png.extend(chunk(b"IEND", &[]));
        png
    }
}
