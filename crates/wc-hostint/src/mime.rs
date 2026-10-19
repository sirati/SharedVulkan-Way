//! Types, three ways: Linux's MIME types, macOS's UTIs and Windows' file
//! extensions. One table, so a Linux application that opens `text/html`
//! claims `public.html` in its bundle and `.html`/`.htm` in the registry,
//! and a host handler for `.html` becomes a Linux handler for `text/html`.
//!
//! The table holds the types people double-click. A MIME type that is not in
//! it still works on macOS through a dynamic UTI declaration
//! ([`uti_for`] returns `None` and the bundle lists the MIME type itself
//! under `CFBundleTypeMIMETypes`), and on Windows not at all: Windows
//! associates extensions, and a MIME type with no known extension has nothing
//! to associate. Such types are reported, not guessed.

/// (MIME type, UTI, extensions). The first extension is the canonical one.
pub const TABLE: &[(&str, &str, &[&str])] = &[
    ("text/plain", "public.plain-text", &["txt", "text", "log"]),
    ("text/html", "public.html", &["html", "htm"]),
    ("application/xhtml+xml", "public.xhtml", &["xhtml", "xht"]),
    ("text/css", "public.css", &["css"]),
    ("text/csv", "public.comma-separated-values-text", &["csv"]),
    ("text/markdown", "net.daringfireball.markdown", &["md", "markdown"]),
    ("text/x-csrc", "public.c-source", &["c"]),
    ("text/x-chdr", "public.c-header", &["h"]),
    ("text/x-c++src", "public.c-plus-plus-source", &["cpp", "cxx", "cc"]),
    ("text/x-python", "public.python-script", &["py"]),
    ("text/x-shellscript", "public.shell-script", &["sh"]),
    ("text/rust", "org.rust-lang.rust-source", &["rs"]),
    ("application/json", "public.json", &["json"]),
    ("application/xml", "public.xml", &["xml"]),
    ("text/xml", "public.xml", &["xml"]),
    ("application/x-yaml", "public.yaml", &["yaml", "yml"]),
    ("application/pdf", "com.adobe.pdf", &["pdf"]),
    ("application/rtf", "public.rtf", &["rtf"]),
    ("image/png", "public.png", &["png"]),
    ("image/jpeg", "public.jpeg", &["jpg", "jpeg"]),
    ("image/gif", "com.compuserve.gif", &["gif"]),
    ("image/webp", "org.webmproject.webp", &["webp"]),
    ("image/svg+xml", "public.svg-image", &["svg"]),
    ("image/bmp", "com.microsoft.bmp", &["bmp"]),
    ("image/tiff", "public.tiff", &["tif", "tiff"]),
    ("audio/mpeg", "public.mp3", &["mp3"]),
    ("audio/flac", "org.xiph.flac", &["flac"]),
    ("audio/x-wav", "com.microsoft.waveform-audio", &["wav"]),
    ("audio/ogg", "org.xiph.ogg-audio", &["oga", "ogg"]),
    ("video/mp4", "public.mpeg-4", &["mp4", "m4v"]),
    ("video/webm", "org.webmproject.webm", &["webm"]),
    ("video/x-matroska", "org.matroska.mkv", &["mkv"]),
    ("application/zip", "public.zip-archive", &["zip"]),
    ("application/x-tar", "public.tar-archive", &["tar"]),
    ("application/gzip", "org.gnu.gnu-zip-archive", &["gz"]),
    ("application/vnd.oasis.opendocument.text", "org.oasis-open.opendocument.text", &["odt"]),
    ("application/vnd.oasis.opendocument.spreadsheet", "org.oasis-open.opendocument.spreadsheet", &["ods"]),
    ("application/vnd.openxmlformats-officedocument.wordprocessingml.document", "org.openxmlformats.wordprocessingml.document", &["docx"]),
    ("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "org.openxmlformats.spreadsheetml.sheet", &["xlsx"]),
];

/// The UTI for a MIME type, if the table knows it.
pub fn uti_for(mime: &str) -> Option<&'static str> {
    let m = mime.to_ascii_lowercase();
    TABLE.iter().find(|(t, _, _)| *t == m).map(|(_, u, _)| *u)
}

/// The extensions (without the dot) for a MIME type.
pub fn extensions_for(mime: &str) -> &'static [&'static str] {
    let m = mime.to_ascii_lowercase();
    TABLE.iter().find(|(t, _, _)| *t == m).map_or(&[], |(_, _, e)| e)
}

/// The MIME type for a file extension (with or without the dot).
pub fn mime_for_extension(ext: &str) -> Option<&'static str> {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    TABLE.iter().find(|(_, _, es)| es.contains(&e.as_str())).map(|(t, _, _)| *t)
}

/// The MIME type for a UTI.
pub fn mime_for_uti(uti: &str) -> Option<&'static str> {
    TABLE.iter().find(|(_, u, _)| u.eq_ignore_ascii_case(uti)).map(|(t, _, _)| *t)
}

/// The URL scheme an `x-scheme-handler/<scheme>` names, lowercased, if it is
/// a scheme (RFC 3986: a letter, then letters, digits, `+`, `-`, `.`).
pub fn scheme_of(mime: &str) -> Option<String> {
    let s = mime.strip_prefix("x-scheme-handler/")?.to_ascii_lowercase();
    let mut cs = s.chars();
    let ok = cs.next().is_some_and(|c| c.is_ascii_alphabetic())
        && cs.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    ok.then_some(s)
}

/// Whether a MIME type string is plausible: `type/subtype`, printable ASCII,
/// no spaces, at most 255 bytes. A desktop entry is Linux's, and its types
/// end up in the host's registry and Info.plist.
pub fn is_mime(s: &str) -> bool {
    s.len() <= 255
        && s.split_once('/').is_some_and(|(a, b)| {
            a.starts_with(|c: char| c.is_ascii_alphabetic())
                && b.starts_with(|c: char| c.is_ascii_alphanumeric())
                && !b.contains('/')
                && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-._/".contains(c))
        })
}

/// What a desktop entry says it opens: its `MimeType=` list, split into file
/// types and URL schemes, each checked with [`is_mime`] / [`scheme_of`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Opens {
    pub mimes: Vec<String>,
    pub schemes: Vec<String>,
}

/// Parse the `MimeType=` key of a desktop entry's `[Desktop Entry]` group.
pub fn opens_of_desktop_entry(text: &str) -> Opens {
    let mut group = "";
    let mut out = Opens::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            group = line;
            continue;
        }
        if group != "[Desktop Entry]" {
            continue;
        }
        let Some(v) = line.strip_prefix("MimeType=") else { continue };
        for m in v.split(';').map(str::trim).filter(|m| !m.is_empty()) {
            if let Some(s) = scheme_of(m) {
                if !out.schemes.contains(&s) {
                    out.schemes.push(s);
                }
            } else if is_mime(m) {
                let m = m.to_ascii_lowercase();
                if !out.mimes.contains(&m) {
                    out.mimes.push(m);
                }
            }
        }
    }
    out
}

/// The desktop entry's `%f`/`%F`/`%u`/`%U` field code: whether it takes files
/// or URLs at all. `None` when its `Exec=` takes neither, and then the
/// application cannot be handed a document.
pub fn exec_takes(text: &str) -> Option<char> {
    let exec = text.lines().map(str::trim).find_map(|l| l.strip_prefix("Exec="))?;
    ['u', 'U', 'f', 'F'].into_iter().find(|c| exec.contains(&format!("%{c}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_ways() {
        assert_eq!(uti_for("text/html"), Some("public.html"));
        assert_eq!(uti_for("TEXT/PLAIN"), Some("public.plain-text"));
        assert_eq!(extensions_for("text/html"), ["html", "htm"]);
        assert_eq!(mime_for_extension(".HTM"), Some("text/html"));
        assert_eq!(mime_for_extension("txt"), Some("text/plain"));
        assert_eq!(mime_for_uti("com.adobe.pdf"), Some("application/pdf"));
        assert_eq!(uti_for("application/x-unknown"), None);
        assert!(extensions_for("application/x-unknown").is_empty());
    }

    #[test]
    fn the_table_is_consistent() {
        for (m, u, es) in TABLE {
            assert!(is_mime(m), "{m}");
            assert!(!es.is_empty(), "{m} has no extension");
            assert!(u.contains('.'), "{u}");
            for e in *es {
                assert!(e.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()), "{e}");
            }
        }
    }

    #[test]
    fn schemes() {
        assert_eq!(scheme_of("x-scheme-handler/https").as_deref(), Some("https"));
        assert_eq!(scheme_of("x-scheme-handler/MAILTO").as_deref(), Some("mailto"));
        assert_eq!(scheme_of("x-scheme-handler/web+app").as_deref(), Some("web+app"));
        assert_eq!(scheme_of("x-scheme-handler/1http"), None);
        assert_eq!(scheme_of("x-scheme-handler/a b"), None);
        assert_eq!(scheme_of("text/html"), None);
    }

    #[test]
    fn firefox_desktop_entry() {
        // Trimmed from nixpkgs' firefox.desktop.
        let e = "[Desktop Entry]\nName=Firefox\nExec=firefox --name firefox %U\n\
                 MimeType=text/html;text/xml;application/xhtml+xml;application/vnd.mozilla.xul+xml;x-scheme-handler/http;x-scheme-handler/https;\n\
                 [Desktop Action new-window]\nMimeType=image/png;\nExec=firefox --new-window %U\n";
        let o = opens_of_desktop_entry(e);
        assert_eq!(o.mimes, ["text/html", "text/xml", "application/xhtml+xml", "application/vnd.mozilla.xul+xml"]);
        assert_eq!(o.schemes, ["http", "https"]);
        assert_eq!(exec_takes(e), Some('U'));
        assert_eq!(exec_takes("[Desktop Entry]\nExec=kgx\n"), None);
    }

    #[test]
    fn hostile_types_are_dropped() {
        let e = "[Desktop Entry]\nMimeType=text/plain;a b/c;\"x/y\";../..;text/plain;x-scheme-handler/;\n";
        let o = opens_of_desktop_entry(e);
        assert_eq!(o.mimes, ["text/plain"]);
        assert!(o.schemes.is_empty());
    }
}
