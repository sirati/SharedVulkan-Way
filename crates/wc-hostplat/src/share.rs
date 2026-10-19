//! Host to Linux, a file: what URL Linux is sent for a host file.
//!
//! A file inside a share (Windows `Z:\` <-> `/srv/winshare`) is sent as the
//! `file:///` URL of its Linux path. A file outside every share is first
//! copied into a directory of its own under the share's drop directory, so
//! Linux sees it under its own name and two opens of files with the same
//! name never collide. The copy is the host's; Linux never reads outside the
//! share.

use wc_hostint::linux::host_to_linux;
use wc_hostint::wire::PathMap;

/// A Linux path as a `file:///` URL. Every byte outside RFC 3986's unreserved
/// set and `/` is percent-encoded, so the URL is printable ASCII with no
/// space, quote or backslash, which is what the `open` verb accepts
/// (wc-proxy `url_ok`).
pub fn file_url(linux_path: &str) -> String {
    let mut s = String::from("file://");
    for b in linux_path.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// What to do to send a host file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePlan {
    /// Already in a share: send this URL.
    Shared { url: String },
    /// Copy `from` to `host_dest` (creating `host_dir`), then send `url`.
    Copy { host_dir: String, host_dest: String, url: String },
}

/// The last component of a Windows or POSIX path.
fn base_name(p: &str) -> &str {
    p.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or("")
}

/// A file name that is safe in both systems' directories: no separators,
/// no control characters, no Windows-reserved characters, not `.`/`..`,
/// at most 200 bytes. Empty becomes `file`.
pub fn safe_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    while s.len() > 200 {
        s.pop();
    }
    let s2 = s.trim_end_matches(['.', ' ']).trim_start_matches(' ');
    if s2.is_empty() || s2 == "." || s2 == ".." {
        return "file".into();
    }
    s = s2.into();
    s
}

/// Plan sending `host_path` (absolute). `unique` names the copy's own
/// directory (see [`unique_dir`]). `None` if the file is outside every
/// share and there is no drop directory, or the drop directory itself does
/// not map.
pub fn plan(host_path: &str, map: &PathMap, unique: &str) -> Option<FilePlan> {
    if let Some(l) = host_to_linux(host_path, map) {
        return Some(FilePlan::Shared { url: file_url(&l) });
    }
    let (drop_host, drop_linux) = map.drop.as_ref()?;
    let name = safe_name(base_name(host_path));
    let win = drop_host.as_bytes().get(1) == Some(&b':');
    let sep = if win { '\\' } else { '/' };
    let host_dir = format!("{}{sep}{unique}", drop_host.trim_end_matches(['\\', '/']));
    let host_dest = format!("{host_dir}{sep}{name}");
    // The drop directory's Linux side is given, and the copy is under it.
    let linux = format!("{}/{unique}/{name}", drop_linux.trim_end_matches('/'));
    Some(FilePlan::Copy { host_dir, host_dest, url: file_url(&linux) })
}

/// A directory name unique to this process and moment: seconds since the
/// epoch, the pid and a counter, in hex.
pub fn unique_dir(epoch_secs: u64, pid: u32, n: u32) -> String {
    format!("{epoch_secs:x}-{pid:x}-{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win() -> PathMap {
        PathMap {
            shares: vec![("Z:\\".into(), "/srv/winshare".into())],
            drop: Some(("Z:\\waycompose-open".into(), "/srv/winshare/waycompose-open".into())),
        }
    }

    #[test]
    fn file_urls_are_percent_encoded() {
        assert_eq!(file_url("/srv/winshare/a b.txt"), "file:///srv/winshare/a%20b.txt");
        assert_eq!(file_url("/x/Ünï\"'\\%#?.pdf"), "file:///x/%C3%9Cn%C3%AF%22%27%5C%25%23%3F.pdf");
        assert_eq!(
            wc_hostint::linux::file_url_path(&file_url("/srv/winshare/ä ö/x#1.txt")).as_deref(),
            Some("/srv/winshare/ä ö/x#1.txt"),
            "round trip through the Linux side's decoder"
        );
    }

    #[test]
    fn a_shared_file_is_sent_by_its_linux_path() {
        assert_eq!(
            plan("Z:\\Docs\\a b.txt", &win(), "u"),
            Some(FilePlan::Shared { url: "file:///srv/winshare/Docs/a%20b.txt".into() })
        );
        assert_eq!(
            plan("z:/docs/x.pdf", &win(), "u"),
            Some(FilePlan::Shared { url: "file:///srv/winshare/docs/x.pdf".into() })
        );
    }

    #[test]
    fn a_host_only_file_is_copied_into_its_own_drop_directory() {
        let d = unique_dir(0x6700_0000, 0x1234, 1);
        assert_eq!(d, "67000000-1234-1");
        assert_eq!(
            plan("C:\\Users\\u\\Desktop\\report 1.pdf", &win(), &d),
            Some(FilePlan::Copy {
                host_dir: "Z:\\waycompose-open\\67000000-1234-1".into(),
                host_dest: "Z:\\waycompose-open\\67000000-1234-1\\report 1.pdf".into(),
                url: "file:///srv/winshare/waycompose-open/67000000-1234-1/report%201.pdf".into(),
            })
        );
        let no_drop = PathMap { drop: None, ..win() };
        assert_eq!(plan("C:\\a.txt", &no_drop, &d), None);
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_name("a:b|c"), "a_b_c");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name("x. "), "x");
        assert_eq!(safe_name(""), "file");
        assert!(safe_name(&"é".repeat(300)).len() <= 200);
    }
}
