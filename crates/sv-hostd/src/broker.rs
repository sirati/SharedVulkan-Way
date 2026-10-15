//! The launcher's way to a located GPU session without being root itself
//! (waycompose-design.md §86).
//!
//! Locating a GPU side's arena needs QEMU's monitor and its descriptor table,
//! which are root's on testhost. The developer harness gets root through
//! `sudokey`, which needs a person's 1Password agent; the desktop shortcut has
//! no person behind it. So a small root daemon, `sv-hostd broker`, listens on a
//! socket only its group can reach. A request names a session directory and a
//! count; the broker checks that the directory and the cold sockets in it
//! belong to the **peer** (`SO_PEERCRED`), then runs
//! `sv-hostd qemu-serve … --drop-to <peer uid>:<peer gid>`, which locates as
//! root and serves as the peer. One request at a time, because QMP serves one
//! client.
//!
//! What it does not reduce: once dropped, the serving process holds QEMU's
//! memory backend, so the peer can open all of guest RAM through
//! `/proc/<pid>/fd/<n>`. Membership of the broker's group is therefore
//! equivalent to read and write access to the VM's memory, which is more than
//! `wc-arena`'s 128 MiB. security-model.md has to say so before the broker's
//! group is anybody's but the operator's.
//!
//! The checks are here, pure, so they are tested without root.



/// The most GPU sessions one request may ask for. Firefox uses three or four.
pub const MAX_SESSIONS: u32 = 8;

/// A parsed request: `serve <dir> <n>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub dir: String,
    pub sessions: u32,
}

pub fn parse_request(line: &str) -> Result<Request, String> {
    let w: Vec<&str> = line.split_whitespace().collect();
    match w.as_slice() {
        ["serve", dir, n] => {
            let sessions: u32 = n.parse().map_err(|_| format!("bad session count {n:?}"))?;
            if sessions == 0 || sessions > MAX_SESSIONS {
                return Err(format!("{sessions} session(s): 1..={MAX_SESSIONS}"));
            }
            Ok(Request { dir: (*dir).to_string(), sessions })
        }
        _ => Err(format!("not a request: {line:?} (want: serve <dir> <n>)")),
    }
}

/// The directory must be absolute, lexically clean (no `.` or `..`), strictly
/// under `root`, and a single path component below it — `wc-session`'s
/// `/tmp/wc-launch/<id>` and nothing else.
pub fn check_dir_name(dir: &str, root: &str) -> Result<(), String> {
    // Exactly `root/<id>`, compared as strings: `Path::components` quietly drops
    // `.` components, so a path-based check accepts `root/./x`.
    let id = dir
        .strip_prefix(root)
        .and_then(|r| r.strip_prefix('/'))
        .ok_or_else(|| format!("{dir} is not one session directory below {root}"))?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(format!("{dir} is not one session directory below {root}"));
    }
    if !root.starts_with('/') {
        return Err(format!("{root} is not absolute"));
    }
    Ok(())
}

/// What `lstat` said about one path, reduced to what the broker decides on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    pub kind: Kind,
    pub uid: u32,
    pub mode: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    Socket,
    Symlink,
    Other,
}

/// The session directory: a real directory (not a link), the peer's, and not
/// writable by anybody else.
pub fn check_dir(seen: Seen, peer_uid: u32, dir: &str) -> Result<(), String> {
    if seen.kind != Kind::Dir {
        return Err(format!("{dir} is not a directory ({:?})", seen.kind));
    }
    if seen.uid != peer_uid {
        return Err(format!("{dir} belongs to uid {}, the request came from uid {peer_uid}", seen.uid));
    }
    if seen.mode & 0o022 != 0 {
        return Err(format!("{dir} is writable by others (mode {:o})", seen.mode & 0o7777));
    }
    Ok(())
}

/// One cold socket: a socket (not a link to one), the peer's.
pub fn check_cold(seen: Seen, peer_uid: u32, path: &str) -> Result<(), String> {
    if seen.kind != Kind::Socket {
        return Err(format!("{path} is not a socket ({:?})", seen.kind));
    }
    if seen.uid != peer_uid {
        return Err(format!("{path} belongs to uid {}, the request came from uid {peer_uid}", seen.uid));
    }
    Ok(())
}

/// The cold sockets a request for `n` sessions in `dir` names, in order.
pub fn cold_paths(dir: &str, n: u32) -> Vec<String> {
    (0..n).map(|i| format!("{dir}/cold{i}.sock")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_one_line_with_a_bounded_count() {
        assert_eq!(parse_request("serve /tmp/wc-launch/a 4\n").unwrap(), Request { dir: "/tmp/wc-launch/a".into(), sessions: 4 });
        assert!(parse_request("serve /tmp/wc-launch/a 0").is_err());
        assert!(parse_request("serve /tmp/wc-launch/a 9").is_err());
        assert!(parse_request("serve /tmp/wc-launch/a").is_err());
        assert!(parse_request("serve /tmp/wc-launch/a 2 --qmp /x").is_err());
    }

    #[test]
    fn only_one_session_directory_below_the_root_is_accepted() {
        let r = "/tmp/wc-launch";
        assert!(check_dir_name("/tmp/wc-launch/wcl-9528", r).is_ok());
        for bad in [
            "wcl-1",
            "/tmp/wc-launch",
            "/tmp/wc-launch/",
            "/tmp/wc-launch/../etc",
            "/tmp/wc-launch/./x",
            "/tmp/wc-launch/a/b",
            "/tmp/other/a",
            "/tmp/wc-launchx/a",
            "/tmp/wc-launch/a b",
        ] {
            assert!(check_dir_name(bad, r).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn ownership_and_kind_are_the_peers_or_the_request_is_refused() {
        let d = |kind, uid, mode| Seen { kind, uid, mode };
        assert!(check_dir(d(Kind::Dir, 1000, 0o700), 1000, "d").is_ok());
        assert!(check_dir(d(Kind::Dir, 1001, 0o700), 1000, "d").is_err());
        assert!(check_dir(d(Kind::Symlink, 1000, 0o777), 1000, "d").is_err());
        assert!(check_dir(d(Kind::Dir, 1000, 0o770), 1000, "d").is_err());
        assert!(check_cold(d(Kind::Socket, 1000, 0o600), 1000, "s").is_ok());
        assert!(check_cold(d(Kind::Symlink, 1000, 0o777), 1000, "s").is_err());
        assert!(check_cold(d(Kind::Socket, 0, 0o600), 1000, "s").is_err());
        assert_eq!(cold_paths("/r/a", 2), ["/r/a/cold0.sock", "/r/a/cold1.sock"]);
    }
}
