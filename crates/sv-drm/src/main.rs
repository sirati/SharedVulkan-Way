//! `sv-drm`: the userspace render node (docs/waycompose-design.md §104).
//!
//! ```text
//! sv-drm serve [--minor 128] [--devname dri/renderD128] [--owner UID] [--group GID]
//!              [--mode 0660] [--state FILE]                          (root)
//! sv-drm ns (--state FILE | --dev MAJ:MIN) [--minor N] [--test-node PATH] [--hide NODE]... [--pci VVVV:DDDD] -- CMD ARGS...
//! ```

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(linux::main());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sv-drm: linux only");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use sv_drm::cuse::{self, DevInfo, Server, Stop};
    use sv_drm::drm::{DrmNode, ProcResolver};
    use sv_drm::ns::{self, NsConfig};

    const USAGE: &str = "usage:
  sv-drm serve [--minor 128] [--devname dri/renderD<minor>] [--owner UID] [--group GID] [--mode 0660] [--state FILE]
  sv-drm ns (--state FILE | --dev MAJ:MIN) [--minor N] [--test-node PATH] [--hide NODE]... [--pci VVVV:DDDD] -- CMD [ARGS...]";

    pub fn main() -> i32 {
        let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
        if args.is_empty() {
            eprintln!("{USAGE}");
            return 2;
        }
        let sub = args.remove(0);
        let r = match sub.to_str() {
            Some("serve") => serve(args),
            Some("ns") => ns_main(args),
            Some("-h" | "--help" | "help") => {
                println!("{USAGE}");
                return 0;
            }
            _ => Err(format!("unknown subcommand {sub:?}\n{USAGE}")),
        };
        match r {
            Ok(code) => code,
            Err(e) => {
                eprintln!("sv-drm: {e}");
                1
            }
        }
    }

    /// Flag pairs, and the command after `--`.
    type Parsed = (Vec<(String, String)>, Vec<OsString>);

    fn flags(args: Vec<OsString>, known: &[&str]) -> Result<Parsed, String> {
        let mut it = args.into_iter();
        let mut out = Vec::new();
        while let Some(a) = it.next() {
            let a = a.into_string().map_err(|a| format!("bad argument {a:?}"))?;
            if a == "--" {
                return Ok((out, it.collect()));
            }
            let (k, v) = match a.split_once('=') {
                Some((k, v)) if a.starts_with("--") => (k.to_string(), Some(v.to_string())),
                _ => (a.clone(), None),
            };
            if !known.contains(&k.as_str()) {
                return Err(format!("unknown flag {a}\n{USAGE}"));
            }
            let v = match v {
                Some(v) => v,
                None => it
                    .next()
                    .and_then(|v| v.into_string().ok())
                    .ok_or_else(|| format!("{k} needs a value"))?,
            };
            out.push((k, v));
        }
        Ok((out, Vec::new()))
    }

    fn num<T: std::str::FromStr>(k: &str, v: &str) -> Result<T, String> {
        v.parse().map_err(|_| format!("{k}: not a number: {v}"))
    }

    // ---- serve ----

    struct ServeArgs {
        minor: u32,
        devname: String,
        owner: Option<u32>,
        group: Option<u32>,
        mode: u32,
        state: Option<PathBuf>,
    }

    fn serve_args(args: Vec<OsString>) -> Result<ServeArgs, String> {
        let (fl, rest) = flags(args, &["--minor", "--devname", "--owner", "--group", "--mode", "--state"])?;
        if !rest.is_empty() {
            return Err("serve takes no command".into());
        }
        let mut a = ServeArgs { minor: 128, devname: String::new(), owner: None, group: None, mode: 0o660, state: None };
        for (k, v) in fl {
            match k.as_str() {
                "--minor" => a.minor = num(&k, &v)?,
                "--devname" => a.devname = v,
                "--owner" => a.owner = Some(num(&k, &v)?),
                "--group" => a.group = Some(num(&k, &v)?),
                "--mode" => {
                    a.mode = u32::from_str_radix(v.trim_start_matches("0o"), 8)
                        .ok()
                        .filter(|m| *m <= 0o777)
                        .ok_or_else(|| format!("--mode: not an octal mode: {v}"))?
                }
                "--state" => a.state = Some(PathBuf::from(v)),
                _ => unreachable!(),
            }
        }
        if !(128..=191).contains(&a.minor) {
            return Err(format!("--minor {}: a render node minor is 128..=191", a.minor));
        }
        if a.devname.is_empty() {
            a.devname = format!("dri/renderD{}", a.minor);
        }
        if !cuse::devname_ok(&a.devname) {
            return Err(format!("--devname {:?}: not a plain relative device name", a.devname));
        }
        Ok(a)
    }

    fn os_err(what: &str) -> String {
        format!("{what}: {}", io::Error::last_os_error())
    }

    /// SIGTERM/SIGINT/SIGHUP, blocked and read from a signalfd so that the
    /// loop can wait on both.
    fn signal_fd() -> Result<OwnedFd, String> {
        // SAFETY: sigset manipulation on a local; the results are checked.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                libc::sigaddset(&mut set, s);
            }
            if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                return Err(os_err("sigprocmask"));
            }
            let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC);
            if fd < 0 {
                return Err(os_err("signalfd"));
            }
            Ok(OwnedFd::from_raw_fd(fd))
        }
    }

    /// Wait until the channel has a request (true) or a signal came (false).
    fn wait(cuse: &OwnedFd, sig: &OwnedFd) -> io::Result<bool> {
        loop {
            let mut p = [
                libc::pollfd { fd: cuse.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: sig.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            // SAFETY: a valid array of two pollfds.
            let n = unsafe { libc::poll(p.as_mut_ptr(), 2, -1) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(e);
            }
            if p[1].revents & libc::POLLIN != 0 {
                // SAFETY: a zeroed siginfo buffer of the right size.
                let mut si: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
                let sz = std::mem::size_of::<libc::signalfd_siginfo>();
                // SAFETY: reading into a valid buffer.
                unsafe { libc::read(sig.as_raw_fd(), (&mut si as *mut libc::signalfd_siginfo).cast(), sz) };
                eprintln!("sv-drm: signal {}, exiting", si.ssi_signo);
                return Ok(false);
            }
            // POLLIN, or POLLERR once the kernel dropped the connection: the
            // read then says ENODEV.
            if p[0].revents != 0 {
                return Ok(true);
            }
        }
    }

    /// The `MAJ:MIN` the kernel gave the CUSE device, from its class entry
    /// (`dri/renderD128` is `dri!renderD128` in sysfs).
    fn cuse_devt(devname: &str) -> io::Result<(u32, u32)> {
        let p = format!("/sys/class/cuse/{}/dev", devname.replace('/', "!"));
        let s = std::fs::read_to_string(&p)?;
        ns::parse_dev(&s).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{p}: {s:?}")))
    }

    /// After CUSE_INIT: find the number, wait for devtmpfs to make the node,
    /// set its owner and mode, announce it.
    fn setup_node(a: &ServeArgs) -> io::Result<(u32, u32)> {
        let devt = cuse_devt(&a.devname)
            .map_err(|e| io::Error::new(e.kind(), format!("the kernel did not register the device: {e}")))?;
        let path = PathBuf::from("/dev").join(&a.devname);
        let want = libc::makedev(devt.0, devt.1);
        let start = Instant::now();
        loop {
            match std::fs::metadata(&path) {
                Ok(m) if m.file_type().is_char_device() && m.rdev() == want => break,
                Ok(m) => {
                    return Err(io::Error::other(format!(
                        "{} is not our device ({:#x}, want {}:{})",
                        path.display(),
                        m.rdev(),
                        devt.0,
                        devt.1
                    )));
                }
                Err(_) if start.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(io::Error::new(e.kind(), format!("{} did not appear: {e}", path.display()))),
            }
        }
        std::os::unix::fs::chown(&path, a.owner, a.group)
            .map_err(|e| io::Error::new(e.kind(), format!("chown {}: {e}", path.display())))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(a.mode))
            .map_err(|e| io::Error::new(e.kind(), format!("chmod {}: {e}", path.display())))?;
        eprintln!("sv-drm: serving {} ({}:{})", path.display(), devt.0, devt.1);
        if let Some(st) = &a.state {
            write_state(st, devt)?;
        }
        Ok(devt)
    }

    fn write_state(st: &Path, devt: (u32, u32)) -> io::Result<()> {
        let tmp = st.with_extension("tmp");
        std::fs::write(&tmp, format!("{}:{}\n", devt.0, devt.1))
            .and_then(|_| std::fs::rename(&tmp, st))
            .map_err(|e| io::Error::new(e.kind(), format!("state file {}: {e}", st.display())))
    }

    fn serve(args: Vec<OsString>) -> Result<i32, String> {
        let a = serve_args(args)?;
        // SAFETY: getter.
        if unsafe { libc::geteuid() } != 0 {
            return Err("serve must run as root (it opens /dev/cuse and sets the node's owner and mode)".into());
        }
        let path = PathBuf::from("/dev").join(&a.devname);
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(format!("{} exists already (a real DRM device, or another sv-drm?)", path.display()));
        }
        let c = std::ffi::CString::new("/dev/cuse").unwrap();
        // SAFETY: valid C string; checked.
        let raw = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(os_err("open /dev/cuse (is the cuse module loaded?)"));
        }
        // SAFETY: a fresh descriptor we own.
        let dev = unsafe { OwnedFd::from_raw_fd(raw) };
        let sig = signal_fd()?;
        let mut server = Server::new(
            DrmNode::new(ProcResolver),
            DevInfo { devname: a.devname.clone(), dev_major: 0, dev_minor: a.minor },
        );
        let mut announced = false;
        let r = cuse::serve(
            dev.as_fd(),
            &mut server,
            || wait(&dev, &sig),
            |s| {
                if !announced && s.initialized() {
                    announced = true;
                    setup_node(&a)?;
                }
                Ok(())
            },
        );
        if let Some(st) = &a.state {
            let _ = std::fs::remove_file(st);
        }
        match r {
            Ok(Stop::Closed) => {
                eprintln!("sv-drm: channel closed, exiting");
                Ok(0)
            }
            Ok(Stop::Destroyed) => {
                eprintln!("sv-drm: destroyed, exiting");
                Ok(0)
            }
            Ok(Stop::InitFailed(why)) => Err(format!("CUSE_INIT: {why}")),
            Err(e) => Err(format!("{e}")),
        }
    }

    // ---- ns ----

    fn ns_main(args: Vec<OsString>) -> Result<i32, String> {
        let (fl, cmd) = flags(args, &["--state", "--dev", "--minor", "--test-node", "--hide", "--pci"])?;
        let (mut dev, mut minor, mut test_node, mut state) = (None, None, None, None);
        let mut hide = Vec::new();
        let mut pci = None;
        for (k, v) in fl {
            match k.as_str() {
                "--dev" => dev = Some(ns::parse_dev(&v).ok_or_else(|| format!("--dev {v}: not MAJ:MIN"))?),
                "--minor" => minor = Some(num::<u32>(&k, &v)?),
                "--test-node" => test_node = Some(PathBuf::from(v)),
                "--state" => state = Some(PathBuf::from(v)),
                "--hide" => hide.push(PathBuf::from(v)),
                "--pci" => {
                    let (a, b) = v.split_once(':').ok_or_else(|| format!("--pci {v}: not VVVV:DDDD"))?;
                    let h = |s: &str| u16::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| format!("--pci {v}: not hex"));
                    pci = Some((h(a)?, h(b)?));
                }
                _ => unreachable!(),
            }
        }
        let dev = match (dev, state) {
            (Some(_), Some(_)) => return Err("--dev and --state are exclusive".into()),
            (Some(d), None) => d,
            (None, Some(s)) => ns::read_state(&s).map_err(|e| format!("--state: {e}"))?,
            (None, None) => {
                let n = format!("/dev/dri/renderD{}", minor.unwrap_or(128));
                let m = std::fs::metadata(&n).map_err(|e| format!("no --dev/--state, and {n}: {e}"))?;
                if !m.file_type().is_char_device() {
                    return Err(format!("{n} is not a char device"));
                }
                (libc::major(m.rdev()), libc::minor(m.rdev()))
            }
        };
        let minor = minor.unwrap_or(dev.1);
        if cmd.is_empty() {
            return Err(format!("ns needs a command after --\n{USAGE}"));
        }
        let cfg = NsConfig { dev, minor, test_node, hide, pci, cmd };
        let e = ns::run(&cfg);
        eprintln!("sv-drm: ns: {e}");
        Ok(127)
    }
}
