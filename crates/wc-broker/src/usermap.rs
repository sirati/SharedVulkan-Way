//! Which Linux user a server-side user's session runs as (§111.10).
//!
//! The client role's host (the Linux machine that runs the applications)
//! reads a root-owned table, `/etc/waycompose/users.conf`, one mapping a line:
//!
//! ```text
//! # SERVER   SERVER-USER   LINUX-USER     [installs=a,b] [id=SID-or-uid]
//! vm-guest   <guest-user>  <desktop-user> installs=wcivs,wcmc
//! vm-guest   <guest-user>  wctest         installs=wctest1   id=S-1-5-21-...-1001
//! vm-guest   -             <desktop-user> installs=wcivs     (transitional, see below)
//! ```
//!
//! * `SERVER` is the machine whose sessions a daemon serves (one VM). A
//!   daemon is told its own (`--server`) and reads only those lines.
//! * `SERVER-USER` is the account name of the boot record's identity
//!   (`wc_handle::macboot::Identity::user`), compared without regard to ASCII
//!   case when the identity's os is `windows` (Windows' own rule) and exactly
//!   otherwise. `id=` pins the account's stable id as well (a SID, a uid).
//! * `LINUX-USER` is the account the session's processes run as.
//! * `installs=` limits the line to those installations; without it, any.
//!
//! The first line that matches wins, so a narrower line goes first. A record
//! no line matches is refused.
//!
//! **`-` as SERVER-USER is transitional.** It matches a record that carries
//! no identity at all (§107's, from a guest whose privileged service is not
//! yet deployed), only when the daemon was started with
//! `--accept-unidentified`, and it must name `installs=`. Remove those lines
//! and the flag once every guest installation boots through its service.

use std::collections::BTreeSet;

/// One line of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub server: String,
    /// `None`: the transitional `-`, a record without identity.
    pub server_user: Option<String>,
    pub linux_user: String,
    pub installs: Option<BTreeSet<String>>,
    pub id: Option<String>,
    /// For messages: where it came from.
    pub line: usize,
}

/// The whole table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserMap {
    pub mappings: Vec<Mapping>,
}

/// What a boot record says about who asked, as far as the map cares.
#[derive(Clone, Copy, Debug)]
pub struct Asker<'a> {
    pub os: &'a str,
    pub user: &'a str,
    pub id: &'a str,
}

fn word_ok(w: &str) -> bool {
    !w.is_empty() && w.len() <= 80 && w.bytes().all(|b| b.is_ascii_graphic() && b != b'=' && b != b',' && b != b'#')
}

fn linux_name_ok(w: &str) -> bool {
    // useradd's rule, roughly: what a NixOS account name can be.
    !w.is_empty()
        && w.len() <= 32
        && w.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && w.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn install_ok(w: &str) -> bool {
    !w.is_empty() && w.len() <= 32 && w.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl UserMap {
    /// Parse the table. Any line that does not parse fails the whole table
    /// (a daemon that half-read its policy would map someone it should not).
    pub fn parse(text: &str) -> Result<UserMap, String> {
        let mut mappings: Vec<Mapping> = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let n = i + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let w: Vec<&str> = line.split_whitespace().collect();
            if w.len() < 3 {
                return Err(format!("users.conf line {n}: SERVER SERVER-USER LINUX-USER [installs=a,b] [id=ID]"));
            }
            let (server, su, lu) = (w[0], w[1], w[2]);
            if !word_ok(server) {
                return Err(format!("users.conf line {n}: bad server {server:?}"));
            }
            let server_user = match su {
                "-" => None,
                s if word_ok(s) => Some(s.to_string()),
                s => return Err(format!("users.conf line {n}: bad server user {s:?}")),
            };
            if !linux_name_ok(lu) || lu == "root" {
                return Err(format!("users.conf line {n}: {lu:?} is not a Linux account a session may run as"));
            }
            let mut installs = None;
            let mut id = None;
            for opt in &w[3..] {
                match opt.split_once('=') {
                    Some(("installs", v)) if installs.is_none() => {
                        let set: BTreeSet<String> = v.split(',').map(str::to_string).collect();
                        if set.iter().any(|s| !install_ok(s)) {
                            return Err(format!("users.conf line {n}: bad installs {v:?}"));
                        }
                        installs = Some(set);
                    }
                    Some(("id", v)) if id.is_none() && word_ok(v) => id = Some(v.to_string()),
                    _ => return Err(format!("users.conf line {n}: unknown or repeated option {opt:?}")),
                }
            }
            if server_user.is_none() {
                if id.is_some() {
                    return Err(format!("users.conf line {n}: `-` (no identity) cannot be pinned by id="));
                }
                let Some(set) = &installs else {
                    return Err(format!("users.conf line {n}: `-` (no identity) must name its installs="));
                };
                // One Linux user per installation for records without identity:
                // there is nothing else to tell two such lines apart by.
                for m in mappings.iter().filter(|m| m.server == server && m.server_user.is_none()) {
                    if let Some(other) = &m.installs
                        && let Some(both) = other.intersection(set).next()
                    {
                        return Err(format!(
                            "users.conf line {n}: installation {both} already has a no-identity user on line {}",
                            m.line
                        ));
                    }
                }
            }
            mappings.push(Mapping {
                server: server.to_string(),
                server_user,
                linux_user: lu.to_string(),
                installs,
                id,
                line: n,
            });
        }
        Ok(UserMap { mappings })
    }

    pub fn load(path: &std::path::Path) -> Result<UserMap, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        UserMap::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The mapping for a record with an identity, or why there is none.
    pub fn resolve(&self, server: &str, asker: Asker<'_>, install: &str) -> Result<&Mapping, String> {
        let same_user = |a: &str, b: &str| if asker.os == "windows" { a.eq_ignore_ascii_case(b) } else { a == b };
        let mut user_known = false;
        for m in self.mappings.iter().filter(|m| m.server == server) {
            let Some(su) = &m.server_user else { continue };
            if !same_user(su, asker.user) {
                continue;
            }
            if m.id.as_deref().is_some_and(|id| id != asker.id) {
                continue;
            }
            user_known = true;
            if m.installs.as_ref().is_none_or(|s| s.contains(install)) {
                return Ok(m);
            }
        }
        if user_known {
            Err(format!(
                "{} user {} ({}) may not start installation {install} on this host (users.conf of {server})",
                asker.os, asker.user, asker.id
            ))
        } else {
            Err(format!(
                "{} user {} ({}) is not mapped to any Linux user on this host (users.conf of {server})",
                asker.os, asker.user, asker.id
            ))
        }
    }

    /// The transitional mapping for a record with no identity (`-`).
    pub fn resolve_unidentified(&self, server: &str, install: &str) -> Option<&Mapping> {
        self.mappings
            .iter()
            .filter(|m| m.server == server && m.server_user.is_none())
            .find(|m| m.installs.as_ref().is_some_and(|s| s.contains(install)))
    }

    /// Every Linux user any line of `server` maps to.
    pub fn linux_users(&self, server: &str) -> BTreeSet<&str> {
        self.mappings.iter().filter(|m| m.server == server).map(|m| m.linux_user.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONF: &str = "\
# a comment
vm  guest   desk installs=wcivs,wcmc,wclinux   # the desktop user
vm  guest   wctest  installs=wct1
vm  alice   alice   id=S-1-5-21-1-2-3-1002
vm  -       desk installs=wcivs,wcmc
other guest  admin
";

    fn win<'a>(user: &'a str, id: &'a str) -> Asker<'a> {
        Asker { os: "windows", user, id }
    }

    #[test]
    fn a_mapped_identity_resolves_per_installation() {
        let m = UserMap::parse(CONF).unwrap();
        assert_eq!(m.resolve("vm", win("guest", "S-1"), "wcivs").unwrap().linux_user, "desk");
        assert_eq!(m.resolve("vm", win("GUEST", "S-1"), "wclinux").unwrap().linux_user, "desk", "Windows names ignore case");
        assert_eq!(m.resolve("vm", win("guest", "S-1"), "wct1").unwrap().linux_user, "wctest");
        assert_eq!(m.resolve("other", win("guest", "S-1"), "anything").unwrap().linux_user, "admin");
        assert_eq!(m.linux_users("vm").into_iter().collect::<Vec<_>>(), vec!["alice", "desk", "wctest"]);
    }

    #[test]
    fn unmapped_unpinned_and_out_of_scope_are_refused_by_name() {
        let m = UserMap::parse(CONF).unwrap();
        let e = m.resolve("vm", win("mallory", "S-9"), "wcivs").unwrap_err();
        assert!(e.contains("not mapped"), "{e}");
        let e = m.resolve("vm", win("guest", "S-1"), "wcother").unwrap_err();
        assert!(e.contains("may not start installation wcother"), "{e}");
        assert!(m.resolve("vm", win("alice", "S-1-5-21-1-2-3-1002"), "x").is_ok());
        let e = m.resolve("vm", win("alice", "S-1-5-21-1-2-3-9999"), "x").unwrap_err();
        assert!(e.contains("not mapped"), "the id pins the account: {e}");
        // Case matters off Windows.
        let lin = Asker { os: "linux", user: "Guest", id: "1000" };
        assert!(m.resolve("vm", lin, "wcivs").is_err());
        // Another server's lines are not this one's.
        assert!(m.resolve("nope", win("guest", "S-1"), "wcivs").is_err());
    }

    #[test]
    fn the_transitional_line_is_per_installation_and_only_for_no_identity() {
        let m = UserMap::parse(CONF).unwrap();
        assert_eq!(m.resolve_unidentified("vm", "wcmc").unwrap().linux_user, "desk");
        assert!(m.resolve_unidentified("vm", "wclinux").is_none());
        assert!(m.resolve_unidentified("other", "wcmc").is_none());
        // `-` never matches a record that has an identity.
        assert!(m.resolve("vm", win("-", "S-1"), "wcivs").is_err());
    }

    #[test]
    fn a_bad_table_fails_whole() {
        for bad in [
            "vm guest",
            "vm guest root",
            "vm guest <desktop-user>",
            "vm guest desk installs=a,../b",
            "vm guest desk flavour=x",
            "vm guest desk id=1 id=2",
            "vm - desk",
            "vm - desk installs=a id=S-1",
            "vm - a installs=x,y\nvm - b installs=y",
        ] {
            assert!(UserMap::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(UserMap::parse("\n# nothing\n").unwrap(), UserMap::default());
    }
}
