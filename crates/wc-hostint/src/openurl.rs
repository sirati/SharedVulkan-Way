//! The open-URL boundary. Linux is less trusted than the host, so the host
//! checks everything Linux asks it to open, and never involves a shell.
//!
//! Adopted from the real-Mac prototype (linux-vm-agent `host/open.rs`), with
//! its tests. The rules (docs/security-model.md, "Opening links on the host"):
//! * at most [`MAX_URL_LEN`] bytes;
//! * no C0, C1 or DEL character anywhere, checked **before** parsing, because
//!   the parser silently strips tabs and newlines;
//! * parsed by a real URL parser and re-serialised, so the host opens its own
//!   spelling of the URL and not Linux's;
//! * the scheme on an allowlist (http and https by default), case-insensitive;
//! * at most 10 requests in any 10 s ([`RateLimiter`]);
//! * the opener gets the URL as one argv entry after `--` (macOS) or as the
//!   file argument of `ShellExecuteW` with an explicit verb (Windows).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use url::Url;

pub const MAX_URL_LEN: usize = 8 * 1024;

/// The schemes a fresh configuration allows.
pub fn default_schemes() -> Vec<String> {
    vec!["http".into(), "https".into()]
}

/// The re-serialised URL to hand to the opener, or why it is refused.
pub fn validate_url(raw: &str, allowed_schemes: &[String]) -> Result<String, String> {
    if raw.len() > MAX_URL_LEN {
        return Err(format!("URL longer than {MAX_URL_LEN} bytes"));
    }
    // Before parsing: the url crate silently strips tabs and newlines.
    if raw.chars().any(char::is_control) {
        return Err("URL contains control characters".into());
    }
    let url = Url::parse(raw).map_err(|e| format!("not a valid URL: {e}"))?;
    // The parser lowercases the scheme; the configuration is compared
    // case-insensitively too.
    let scheme = url.scheme();
    if !allowed_schemes.iter().any(|s| s.eq_ignore_ascii_case(scheme)) {
        return Err(format!("scheme '{scheme}' is not allowed"));
    }
    let out = url.as_str();
    if out.len() > MAX_URL_LEN || out.chars().any(char::is_control) {
        return Err("URL invalid after normalisation".into());
    }
    // A URL that starts with '-' cannot survive the parser (a scheme starts
    // with a letter), but the opener's argv must never see an option, so say
    // it here as well rather than rely on that.
    if out.starts_with('-') {
        return Err("URL starts with '-'".into());
    }
    Ok(out.to_string())
}

/// Sliding window: at most `max` events in any `window`.
pub struct RateLimiter {
    max: usize,
    window: Duration,
    events: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(max: usize, window: Duration) -> Self {
        Self { max, window, events: VecDeque::new() }
    }

    /// The host's limit for open requests: 10 in any 10 s.
    pub fn for_open() -> Self {
        Self::new(10, Duration::from_secs(10))
    }

    pub fn allow(&mut self, now: Instant) -> bool {
        while let Some(&t) = self.events.front() {
            if now.duration_since(t) >= self.window {
                self.events.pop_front();
            } else {
                break;
            }
        }
        if self.events.len() >= self.max {
            return false;
        }
        self.events.push_back(now);
        true
    }
}

/// A host path Linux asked to open: it must be an absolute path inside one
/// of the shared roots the host exported (its home on macOS, the mapped
/// folder on Windows), with no `..` component, no control characters, and
/// no more than [`MAX_URL_LEN`] bytes. Returns the path unchanged when it
/// passes. Purely lexical: the host then opens it with its own API, which
/// resolves symlinks inside the share the same way the user's own file
/// manager would.
pub fn validate_host_path(path: &str, roots: &[String], windows: bool) -> Result<String, String> {
    if path.len() > MAX_URL_LEN {
        return Err(format!("path longer than {MAX_URL_LEN} bytes"));
    }
    if path.chars().any(char::is_control) {
        return Err("path contains control characters".into());
    }
    let sep = |c: char| c == '/' || (windows && c == '\\');
    if path.split(sep).any(|c| c == "..") {
        return Err("path contains '..'".into());
    }
    if windows {
        // A drive path only: no UNC (\\server), no device path (\\?\, \\.\),
        // no alternate data stream (a second ':').
        let b = path.as_bytes();
        let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
        if !drive || path[2..].contains(':') {
            return Err("not an absolute drive path".into());
        }
    } else if !path.starts_with('/') {
        return Err("not an absolute path".into());
    }
    let norm = |s: &str| -> String {
        let s = if windows { s.replace('/', "\\").to_ascii_lowercase() } else { s.to_string() };
        s.trim_end_matches(['/', '\\']).to_string()
    };
    let p = norm(path);
    let sepc = if windows { '\\' } else { '/' };
    let inside = roots.iter().map(|r| norm(r)).any(|r| !r.is_empty() && (p == r || p.starts_with(&format!("{r}{sepc}"))));
    if !inside {
        return Err("path is outside the shared folders".into());
    }
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web() -> Vec<String> {
        default_schemes()
    }

    #[test]
    fn accepts_web_urls() {
        assert_eq!(validate_url("https://example.org/a?b=c#d", &web()).unwrap(), "https://example.org/a?b=c#d");
        assert_eq!(validate_url("http://example.org", &web()).unwrap(), "http://example.org/");
        assert_eq!(validate_url("HTTPS://Example.ORG/X", &web()).unwrap(), "https://example.org/X");
        // The configuration is case-insensitive too.
        assert!(validate_url("https://example.org/", &["HTTPS".into()]).is_ok());
        // Spaces get percent-encoded by the re-serialisation.
        assert_eq!(validate_url("https://example.org/a b", &web()).unwrap(), "https://example.org/a%20b");
        // Quotes and shell metacharacters survive only percent-encoded or
        // inert: the opener never sees a shell.
        assert_eq!(validate_url("https://example.org/\"a\"", &web()).unwrap(), "https://example.org/%22a%22");
        // IDN hosts become punycode.
        assert_eq!(validate_url("https://bücher.example/", &web()).unwrap(), "https://xn--bcher-kva.example/");
    }

    #[test]
    fn rejects_bad_schemes() {
        for u in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "file:///etc/passwd",
            "ftp://example.org/",
            "x-apple-systempreferences:com.apple.preference",
            "data:text/html,<script>",
            "ms-settings:privacy",
            "search-ms:query=x",
            "vbscript:msgbox(1)",
        ] {
            let err = validate_url(u, &web()).unwrap_err();
            assert!(err.contains("not allowed"), "{u}: {err}");
        }
    }

    #[test]
    fn rejects_malformed() {
        assert!(validate_url("/etc/passwd", &web()).is_err());
        assert!(validate_url("", &web()).is_err());
        assert!(validate_url("https://exa mple.org/", &web()).is_err());
        assert!(validate_url("-a Terminal", &web()).is_err());
        assert!(validate_url("C:\\Windows\\system32\\calc.exe", &web()).is_err());
    }

    #[test]
    fn rejects_control_characters() {
        for u in [
            "https://example.org/\n",
            "https://ex\tample.org/",
            "https://example.org/\u{7f}",
            "https://example.org/\u{85}",
            "https://example.org/\u{9b}",
            "https://example.org/\0",
            "https://example.org/\r\nX: y",
        ] {
            assert!(validate_url(u, &web()).unwrap_err().contains("control"), "{u:?}");
        }
    }

    #[test]
    fn rejects_long() {
        let long = format!("https://example.org/{}", "a".repeat(9 * 1024));
        assert!(validate_url(&long, &web()).unwrap_err().contains("longer"));
        let ok = format!("https://example.org/{}", "a".repeat(MAX_URL_LEN - 20));
        assert_eq!(ok.len(), MAX_URL_LEN);
        assert!(validate_url(&ok, &web()).is_ok());
        // Within the limit before, over it after percent-encoding.
        let grows = format!("https://example.org/{}x", "a ".repeat(4000));
        assert!(grows.len() <= MAX_URL_LEN);
        assert!(validate_url(&grows, &web()).unwrap_err().contains("normalisation"));
    }

    #[test]
    fn rate_limiter_window() {
        let mut rl = RateLimiter::for_open();
        let t0 = Instant::now();
        for i in 0..10 {
            assert!(rl.allow(t0 + Duration::from_millis(i)));
        }
        assert!(!rl.allow(t0 + Duration::from_secs(5)));
        assert!(rl.allow(t0 + Duration::from_secs(10)));
        // Only the oldest slot has expired.
        assert!(!rl.allow(t0 + Duration::from_secs(10)));
        assert!(rl.allow(t0 + Duration::from_millis(10_001)));
    }

    #[test]
    fn host_paths_inside_the_share_only() {
        let mac = vec!["/Users/alice".to_string()];
        assert!(validate_host_path("/Users/alice/Documents/a b.txt", &mac, false).is_ok());
        assert!(validate_host_path("/Users/alice", &mac, false).is_ok());
        assert!(validate_host_path("/Users/alicex/a", &mac, false).is_err());
        assert!(validate_host_path("/Users/alice/../bob/a", &mac, false).unwrap_err().contains(".."));
        assert!(validate_host_path("/etc/passwd", &mac, false).unwrap_err().contains("outside"));
        assert!(validate_host_path("Users/alice/a", &mac, false).is_err());
        assert!(validate_host_path("/Users/alice/a\nb", &mac, false).unwrap_err().contains("control"));
        assert!(validate_host_path("/x", &[], false).is_err());

        let win = vec!["C:\\Users\\Alice".to_string()];
        assert!(validate_host_path("C:\\Users\\Alice\\Desktop\\a.txt", &win, true).is_ok());
        assert!(validate_host_path("c:/users/alice/a.txt", &win, true).is_ok(), "drive paths are case-insensitive");
        assert!(validate_host_path("C:\\Users\\Alice\\..\\Bob\\a", &win, true).is_err());
        assert!(validate_host_path("C:\\Users\\Alice\\a.txt:stream", &win, true).is_err());
        assert!(validate_host_path("\\\\server\\share\\a", &win, true).is_err());
        assert!(validate_host_path("\\\\?\\C:\\Users\\Alice\\a", &win, true).is_err());
        assert!(validate_host_path("C:\\Windows\\notepad.exe", &win, true).unwrap_err().contains("outside"));
    }
}
