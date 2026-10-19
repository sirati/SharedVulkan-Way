//! `sv-drm ns`: run a command where libdrm takes the CUSE node for a DRM render
//! node on bus "faux" (docs/waycompose-design.md §104).
//!
//! libdrm decides what a node is from sysfs: `/sys/dev/char/M:m/device/drm`
//! must exist, `.../device/subsystem` must be a link whose last component starts
//! with a known bus (`faux`), and `realpath(.../device)`'s basename is the faux
//! device's name (libdrm 2.4.134 `drmNodeIsDRM`, `get_subsystem_type`,
//! `drmParseFauxBusInfo`). For a CUSE device the kernel's entry points at
//! `devices/virtual/cuse/...`, so this helper, unprivileged, makes a user+mount
//! namespace, lays a tmpfs over `/sys/dev/char` with every original link plus
//! a small faux device for our node, makes it read-only, maps itself back to its
//! real uid/gid in a nested user namespace and execs the command.
//!
//! The node itself stays the real `/dev/dri/renderD<minor>` that CUSE created:
//! a device node bound into a tmpfs made inside a user namespace can be
//! stat()ed but not opened (EACCES). `--test-node` does exactly that, for
//! testing the sysfs side where no CUSE node exists.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Where libdrm looks.
pub const SYS_DEV_CHAR: &str = "/sys/dev/char";
/// X11's socket directory, made ours inside the namespace (Xwayland).
pub const X11_UNIX: &str = "/tmp/.X11-unix";
/// The faux device's name, which libdrm reports in `businfo.faux->name`.
pub const FAUX_NAME: &str = "svdrm";

/// What `sv-drm ns` was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NsConfig {
    /// The node's device number.
    pub dev: (u32, u32),
    /// The N of `/dev/dri/renderD<N>`.
    pub minor: u32,
    /// Testing only: bind this (existing) char device as `/dev/dri/renderD<minor>`
    /// over a tmpfs on `/dev/dri`.
    pub test_node: Option<PathBuf>,
    pub cmd: Vec<OsString>,
}

/// `MAJ:MIN`.
pub fn parse_dev(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.trim().split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// The `MAJ:MIN` that `sv-drm serve --state` wrote.
pub fn read_state(path: &Path) -> io::Result<(u32, u32)> {
    let s = std::fs::read_to_string(path)?;
    parse_dev(&s).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{}: not MAJ:MIN", path.display())))
}

/// One thing to create under the tmpfs, path relative to its root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Dir(PathBuf),
    File(PathBuf, String),
    Link(PathBuf, PathBuf),
}

/// What the tmpfs over `/sys/dev/char` holds: the `existing` links (name,
/// target), except one for our device number, plus the faux device.
pub fn sysfs_plan(dev: (u32, u32), minor: u32, existing: &[(OsString, PathBuf)]) -> Vec<Entry> {
    let (maj, min) = dev;
    let ours = format!("{maj}:{min}");
    let mut v: Vec<Entry> = existing
        .iter()
        .filter(|(n, _)| n.as_bytes() != ours.as_bytes() && !n.as_bytes().starts_with(b"."))
        .map(|(n, t)| Entry::Link(PathBuf::from(n), t.clone()))
        .collect();
    let p = |s: &str| PathBuf::from(s);
    let render = format!("renderD{minor}");
    v.extend([
        Entry::Dir(p(".svdrm")),
        Entry::Dir(p(".svdrm/svdrm")),
        Entry::Dir(p(".svdrm/svdrm/drm")),
        Entry::Dir(p(&format!(".svdrm/svdrm/drm/{render}"))),
        Entry::Link(p(".svdrm/svdrm/subsystem"), p("../bus/faux")),
        Entry::File(p(".svdrm/svdrm/uevent"), "DRIVER=svdrm\n".into()),
        Entry::Dir(p(".svdrm/minor")),
        Entry::Link(p(".svdrm/minor/device"), p("../svdrm")),
        Entry::File(p(".svdrm/minor/dev"), format!("{maj}:{min}\n")),
        Entry::File(p(".svdrm/minor/uevent"), format!("MAJOR={maj}\nMINOR={min}\nDEVNAME=dri/{render}\n")),
        Entry::Link(p(&ours), p(".svdrm/minor")),
    ]);
    v
}

/// The links in `/sys/dev/char` now.
pub fn list_links(dir: &Path) -> io::Result<Vec<(OsString, PathBuf)>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if let Ok(t) = std::fs::read_link(e.path()) {
            v.push((e.file_name(), t));
        }
    }
    v.sort();
    Ok(v)
}

/// Create the plan under `root`.
pub fn apply(root: &Path, plan: &[Entry]) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for e in plan {
        match e {
            Entry::Dir(p) => {
                let p = root.join(p);
                std::fs::create_dir(&p)?;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
            }
            Entry::File(p, s) => {
                let p = root.join(p);
                std::fs::write(&p, s)?;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444))?;
            }
            Entry::Link(p, t) => std::os::unix::fs::symlink(t, root.join(p))?,
        }
    }
    Ok(())
}

fn cstr(p: impl AsRef<OsStr>) -> io::Result<CString> {
    CString::new(p.as_ref().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

fn ctx<T>(r: io::Result<T>, what: impl FnOnce() -> String) -> io::Result<T> {
    r.map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", what())))
}

fn check(r: libc::c_int, what: impl FnOnce() -> String) -> io::Result<()> {
    if r < 0 { ctx(Err(io::Error::last_os_error()), what) } else { Ok(()) }
}

fn mount(src: Option<&str>, target: &str, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> io::Result<()> {
    let src = src.map(cstr).transpose()?;
    let target_c = cstr(target)?;
    let fstype = fstype.map(cstr).transpose()?;
    let data = data.map(cstr).transpose()?;
    let ptr = |c: &Option<CString>| c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    // SAFETY: valid C strings or NULL.
    let r = unsafe { libc::mount(ptr(&src), target_c.as_ptr(), ptr(&fstype), flags, ptr(&data).cast()) };
    check(r, || format!("mount {target}"))
}

fn write_proc(path: &str, s: &str) -> io::Result<()> {
    // One write: the kernel takes a map in a single write(2).
    ctx(std::fs::write(path, s), || format!("write {path} ({:?})", s.trim()))
}

const TMPFS_FLAGS: libc::c_ulong = libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV;

/// A read-only tmpfs over `dir` holding `plan`.
fn overlay(dir: &str, plan: &[Entry]) -> io::Result<()> {
    mount(Some("svdrm"), dir, Some("tmpfs"), TMPFS_FLAGS, Some("mode=0755,size=1m"))?;
    ctx(apply(Path::new(dir), plan), || format!("populate {dir}"))?;
    mount(None, dir, None, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | TMPFS_FLAGS, None)
}

/// `--test-node`: a tmpfs on /dev/dri with `node` bound as renderD<minor>.
fn test_node(node: &Path, minor: u32) -> io::Result<()> {
    let node_s = node.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "test node path"))?;
    mount(Some("svdrm"), "/dev/dri", Some("tmpfs"), libc::MS_NOSUID | libc::MS_NOEXEC, Some("mode=0755,size=64k"))?;
    let target = format!("/dev/dri/renderD{minor}");
    ctx(std::fs::write(&target, b""), || format!("create {target}"))?;
    mount(Some(node_s), &target, None, libc::MS_BIND, None)?;
    mount(None, "/dev/dri", None, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NOEXEC, None)
}

/// Set up the namespace and exec `cfg.cmd`. Returns only on failure. Must be
/// called while the process is single-threaded (unshare(CLONE_NEWUSER)).
pub fn run(cfg: &NsConfig) -> io::Error {
    match setup(cfg) {
        Ok(()) => exec(&cfg.cmd),
        Err(e) => e,
    }
}

/// Everything up to the exec.
pub fn setup(cfg: &NsConfig) -> io::Result<()> {
    if cfg.cmd.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no command"));
    }
    // SAFETY: getters without failure modes.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let existing = ctx(list_links(Path::new(SYS_DEV_CHAR)), || format!("list {SYS_DEV_CHAR}"))?;
    let plan = sysfs_plan(cfg.dev, cfg.minor, &existing);

    // 1: a user namespace where we are root, and our own mount namespace.
    // SAFETY: plain syscall.
    check(unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) }, || {
        "unshare(CLONE_NEWUSER|CLONE_NEWNS) (unprivileged user namespaces disabled?)".into()
    })?;
    write_proc("/proc/self/setgroups", "deny")?;
    write_proc("/proc/self/uid_map", &format!("0 {uid} 1\n"))?;
    write_proc("/proc/self/gid_map", &format!("0 {gid} 1\n"))?;
    mount(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)?;

    // 2: the faux device.
    overlay(SYS_DEV_CHAR, &plan)?;
    if let Some(n) = &cfg.test_node {
        test_node(n, cfg.minor)?;
    }
    // 2b: X11's socket directory, ours. In this namespace the host's
    // root-owned /tmp/.X11-unix shows as the overflow uid, and
    // kwin_wayland_wrapper refuses to make Xwayland's sockets in a directory
    // "not owned by root or us" — the nested session had no X11 at all
    // (waycompose-design.md §104). A tmpfs made here belongs to us; X11
    // clients outside the namespace still reach Xwayland through its abstract
    // socket, which the mount namespace does not hide. Skipped when there is
    // no such directory.
    if Path::new(X11_UNIX).is_dir() {
        mount(Some("svdrm-x11"), X11_UNIX, Some("tmpfs"), TMPFS_FLAGS, Some("mode=1777,size=64k"))?;
    }

    // 3: back to the real ids, in a user namespace nested in the first. We are
    // uid 0 there, which is our real uid, so the one-line map "<uid> 0 1" is
    // one an unprivileged process may write for itself; setgroups stays "deny"
    // (inherited), which the gid map requires.
    // SAFETY: plain syscall.
    check(unsafe { libc::unshare(libc::CLONE_NEWUSER) }, || "unshare(CLONE_NEWUSER), second".into())?;
    let _ = std::fs::write("/proc/self/setgroups", "deny");
    write_proc("/proc/self/uid_map", &format!("{uid} 0 1\n"))?;
    write_proc("/proc/self/gid_map", &format!("{gid} 0 1\n"))?;
    // SAFETY: getters.
    let (u2, g2) = unsafe { (libc::getuid(), libc::getgid()) };
    if (u2, g2) != (uid, gid) {
        return Err(io::Error::other(format!("ids after the remap are {u2}:{g2}, not {uid}:{gid}")));
    }
    // The exec drops the capabilities this namespace gave us (uid != 0 in it).
    Ok(())
}

/// `execvp`; returns the error.
pub fn exec(cmd: &[OsString]) -> io::Error {
    let args: Vec<CString> = match cmd.iter().map(cstr).collect() {
        Ok(a) => a,
        Err(e) => return e,
    };
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    // SAFETY: a NULL-terminated argv of valid C strings.
    unsafe { libc::execvp(argv[0], argv.as_ptr()) };
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("exec {}: {e}", cmd[0].to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_numbers() {
        assert_eq!(parse_dev("511:128\n"), Some((511, 128)));
        assert_eq!(parse_dev("x:1"), None);
        assert_eq!(parse_dev("1"), None);
    }

    #[test]
    fn the_plan_keeps_the_links_and_replaces_ours() {
        let existing = vec![
            (OsString::from("1:3"), PathBuf::from("../../devices/virtual/mem/null")),
            (OsString::from("511:128"), PathBuf::from("../../devices/virtual/cuse/dri!renderD128")),
        ];
        let plan = sysfs_plan((511, 128), 128, &existing);
        assert!(plan.contains(&Entry::Link("1:3".into(), "../../devices/virtual/mem/null".into())));
        assert!(plan.contains(&Entry::Link("511:128".into(), ".svdrm/minor".into())));
        assert!(!plan.iter().any(|e| matches!(e, Entry::Link(_, t) if t.to_str().unwrap().contains("cuse"))));
        assert!(plan.contains(&Entry::File(".svdrm/minor/dev".into(), "511:128\n".into())));
        assert!(plan.contains(&Entry::File(
            ".svdrm/minor/uevent".into(),
            "MAJOR=511\nMINOR=128\nDEVNAME=dri/renderD128\n".into()
        )));
    }

    /// The plan, applied to a scratch dir, answers libdrm's three questions.
    #[test]
    fn the_plan_resolves_like_libdrm_expects() {
        let root = std::env::temp_dir().join(format!("sv-drm-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        apply(&root, &sysfs_plan((511, 129), 129, &[])).unwrap();
        let dev = root.join("511:129/device");
        assert!(dev.join("drm").is_dir());
        let sub = std::fs::read_link(dev.join("subsystem")).unwrap();
        assert!(sub.file_name().unwrap().to_str().unwrap().starts_with("faux"));
        assert_eq!(std::fs::canonicalize(&dev).unwrap().file_name().unwrap(), FAUX_NAME);
        assert!(dev.join("drm/renderD129").is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
