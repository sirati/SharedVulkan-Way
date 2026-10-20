//! The `wc-broker` program on Unix (§111.9): the daemon, and a small client
//! for scripts and tests.
//!
//! ```text
//! wc-broker serve [--socket PATH] [--keys DIR] [--device NAME=PATH]... [--service NAME=PATH]... [--no-default-devices]
//!                 [--release-after SECS] [--budget-mib N]
//! wc-broker ctl [--hold] REQUEST...
//! wc-broker plist BIN                 (the LaunchDaemon's plist, for the installer)
//! ```
//!
//! On macOS `serve` is the LaunchDaemon `org.waycompose.broker` (root, launchd
//! keeps it alive). Its defaults there:
//!
//! * the socket [`crate::client::DEFAULT_PATH`], `/var/run/waycompose/broker.sock`,
//!   mode 0666: who may do what is decided per request by the caller's uid;
//! * the keys `/Library/Application Support/waycompose/keys/<install>.key`,
//!   the directory 0700 and the keys 0600, root's: `sign` (§111.6);
//! * the devices `wcbell` and `svbell`, the VM's two legacy virtio-serial
//!   ports (`/dev/cu.org.sirati.wcbell.0`, `/dev/cu.org.sirati.svbell.0`),
//!   whose nodes it makes root's and 0600, so only it can open them;
//! * the keeper (§103.15): every region it makes is wired by it, and a
//!   session's orphans go when the host writes the release word in the
//!   session's boot page, or after `--release-after` (600 s);
//! * SIGTERM (a shutdown): it waits at most [`TERM_LIMIT`] for the hosts to
//!   release what it holds, then exits.
//!
//! `ctl` sends each REQUEST as one line and prints each reply, with the
//! number of descriptors that came with it. With `--hold` it then keeps the
//! connection (and what it owns) until its stdin ends.

use crate::core::{Allow, DeviceSpec, Release};
use crate::server::{Broker, Config};
use crate::sys::unix::{DevicePath, Unix};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long a shutdown waits for the hosts' releases (§103.15: the keeper
/// waited at most 10 s after SIGTERM; launchd's own limit is 20 s).
pub const TERM_LIMIT: Duration = Duration::from_secs(10);

/// The keeper's fallback: an orphan the host never released goes after this.
pub const RELEASE_AFTER: Duration = Duration::from_secs(600);

#[cfg(target_os = "macos")]
pub const DEFAULT_KEYS: &str = "/Library/Application Support/waycompose/keys";
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_KEYS: &str = "/etc/waycompose/keys";

/// The macOS VM's doorbell ports (§103.15), by the names the protocol lends
/// them under.
#[cfg(target_os = "macos")]
pub const DEFAULT_DEVICES: &[(&str, &str)] =
    &[("wcbell", "/dev/cu.org.sirati.wcbell.0"), ("svbell", "/dev/cu.org.sirati.svbell.0")];
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_DEVICES: &[(&str, &str)] = &[];

#[derive(Debug, PartialEq, Eq)]
pub struct ServeArgs {
    pub socket: String,
    pub keys: PathBuf,
    pub devices: Vec<(String, PathBuf)>,
    /// `--service NAME=PATH`: a socket `connect NAME` reaches (any user).
    pub services: Vec<(String, PathBuf)>,
    pub release_after: Duration,
    pub budget: Option<u64>,
}

pub fn parse_serve(argv: &[String]) -> Result<ServeArgs, String> {
    let mut a = ServeArgs {
        socket: crate::client::path(),
        keys: DEFAULT_KEYS.into(),
        devices: DEFAULT_DEVICES.iter().map(|(n, p)| (n.to_string(), PathBuf::from(p))).collect(),
        services: Vec::new(),
        release_after: RELEASE_AFTER,
        budget: None,
    };
    let mut it = argv.iter();
    while let Some(k) = it.next() {
        let mut v = || it.next().cloned().ok_or(format!("{k} needs a value"));
        match k.as_str() {
            "--socket" => a.socket = v()?,
            "--keys" => a.keys = v()?.into(),
            "--no-default-devices" => a.devices.clear(),
            "--device" => {
                let s = v()?;
                let (n, p) = s.split_once('=').ok_or(format!("--device {s}: NAME=PATH"))?;
                if crate::proto::Request::parse(&format!("device {n}")).is_err() {
                    return Err(format!("--device {s}: {n:?} is not a device name"));
                }
                a.devices.retain(|d| d.0 != n);
                a.devices.push((n.to_string(), p.into()));
            }
            "--service" => {
                let s = v()?;
                let (n, p) = s.split_once('=').ok_or(format!("--service {s}: NAME=PATH"))?;
                if crate::proto::Request::parse(&format!("connect {n}")).is_err() {
                    return Err(format!("--service {s}: {n:?} is not a service name"));
                }
                a.services.push((n.to_string(), p.into()));
            }
            "--release-after" => {
                a.release_after = Duration::from_secs(v()?.parse().map_err(|e| format!("--release-after: {e}"))?)
            }
            "--budget-mib" => a.budget = Some(v()?.parse::<u64>().map_err(|e| format!("--budget-mib: {e}"))? << 20),
            _ => return Err(format!("unknown argument {k}")),
        }
    }
    Ok(a)
}

fn log(s: &str) {
    eprintln!("[wc-broker] {s}");
}

static TERMED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_sig: libc::c_int) {
    TERMED.store(true, Ordering::SeqCst);
}

/// Make `path` root's and 0600: the ports are the broker's to open (§111.9).
/// devfs gives a serial node 0666 when the device attaches.
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(m) = std::fs::metadata(path) else { return };
    if m.uid() == 0 && m.mode() & 0o077 == 0 {
        return;
    }
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else { return };
    // SAFETY: a NUL-terminated path; uid and gid 0.
    let owned = unsafe { libc::chown(c.as_ptr(), 0, 0) } == 0;
    let moded = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).is_ok();
    log(&format!(
        "{}: {}",
        path.display(),
        if owned && moded { "now root's, 0600: only the broker opens it".to_string() } else { format!("could not make it root's only ({})", std::io::Error::last_os_error()) }
    ));
}

pub fn serve(a: ServeArgs) -> Result<(), String> {
    if let Some(dir) = std::path::Path::new(&a.socket).parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    // The keys are root's: only the broker reads them (§111.6).
    if std::fs::create_dir_all(&a.keys).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&a.keys, std::fs::Permissions::from_mode(0o700));
    }
    for (_, p) in &a.devices {
        restrict(p);
    }
    let paths: Vec<DevicePath> = a.devices.iter().map(|(n, p)| DevicePath { name: n.clone(), path: p.clone() }).collect();
    let mut unix = Unix::bind(&a.socket, paths).map_err(|e| format!("{}: {e}", a.socket))?;
    unix = unix.with_services(a.services.iter().map(|(n, p)| DevicePath { name: n.clone(), path: p.clone() }).collect());
    if cfg!(target_os = "macos") {
        unix = unix.mac_keeper();
    }
    let devices = a.devices.iter().map(|(n, _)| DeviceSpec { name: n.clone(), allow: Allow::AnyUser }).collect();
    let mut rnd = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut rnd);
    }
    let run = u64::from_le_bytes(rnd);
    let config = Config {
        devices,
        services: a.services.iter().map(|(n, _)| DeviceSpec { name: n.clone(), allow: Allow::AnyUser }).collect(),
        keys: Some(a.keys.clone()),
        release: Release::Probe { after: Some(a.release_after) },
        budget: a.budget,
        run,
    };
    let broker = Broker::new(unix, config);
    log(&format!(
        "socket {} (0666, decided per uid), keys {}, devices {}, an orphan's limit {:?}",
        a.socket,
        a.keys.display(),
        a.devices.iter().map(|(n, p)| format!("{n}={}", p.display())).collect::<Vec<_>>().join(" "),
        a.release_after
    ));
    // SAFETY: the handler only stores an atomic. SIGHUP is ignored: nothing
    // of a login session's end may take the keeper's regions with it.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let b2 = broker.clone();
    std::thread::Builder::new()
        .name("wc-broker-serve".into())
        .spawn(move || {
            if let Err(e) = b2.serve() {
                log(&format!("serving ended: {e}"));
                std::process::exit(1);
            }
        })
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    while !TERMED.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
        n += 1;
        // A port node that came back (a device re-attached) is made the
        // broker's again.
        if n.is_multiple_of(100) {
            for (_, p) in &a.devices {
                restrict(p);
            }
        }
    }
    let held = broker.stats().orphans;
    log(&format!("SIGTERM: {held} orphan region(s) held; waiting at most {TERM_LIMIT:?} for their release"));
    let left = broker.drain_orphans(TERM_LIMIT);
    log(&if left == 0 { "every orphan released; exiting".to_string() } else { format!("{left} orphan(s) NOT released in {TERM_LIMIT:?}; exiting anyway") });
    let _ = std::fs::remove_file(&a.socket);
    Ok(())
}

/// `ctl`: one line per REQUEST, each reply printed.
pub fn ctl(argv: &[String]) -> Result<(), String> {
    use crate::sys::unix::{recv_line, send_with_fds};
    let hold = argv.iter().any(|a| a == "--hold");
    let path = crate::client::path();
    let s = std::os::unix::net::UnixStream::connect(&path).map_err(|e| format!("the broker at {path}: {e}"))?;
    let (mut buf, mut fds) = (Vec::new(), Vec::new());
    for req in argv.iter().filter(|a| *a != "--hold") {
        send_with_fds(&s, format!("{req}\n").as_bytes(), &[]).map_err(|e| e.to_string())?;
        loop {
            let line = recv_line(&s, &mut buf, &mut fds).map_err(|e| e.to_string())?.ok_or("the broker closed the connection")?;
            let n = std::mem::take(&mut fds).len();
            println!("{line}{}", if n > 0 { format!("  [{n} fd]") } else { String::new() });
            if !line.starts_with('!') {
                break;
            }
        }
    }
    if hold {
        println!("holding; end stdin to let go");
        let mut s = String::new();
        while std::io::stdin().read_line(&mut s).is_ok_and(|n| n > 0) {
            s.clear();
        }
    }
    Ok(())
}

/// The LaunchDaemon's label and where its plist goes (§111.9).
pub const LAUNCHD_LABEL: &str = "org.waycompose.broker";
pub const LAUNCHD_PLIST: &str = "/Library/LaunchDaemons/org.waycompose.broker.plist";

/// The LaunchDaemon's plist for the program at `bin`: root (a daemon's
/// default), started at boot, restarted by launchd whenever it ends
/// (`KeepAlive`), and given 15 s after SIGTERM, more than the
/// [`TERM_LIMIT`] it waits for the hosts' releases.
pub fn launchd_plist(bin: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ExitTimeOut</key><integer>15</integer>
  <key>StandardErrorPath</key><string>/Library/Logs/waycompose/broker.log</string>
  <key>StandardOutPath</key><string>/Library/Logs/waycompose/broker.log</string>
</dict>
</plist>
"#,
        esc(bin)
    )
}

pub fn main(args: Vec<String>) -> std::process::ExitCode {
    let r = match args.first().map(String::as_str) {
        Some("ctl") => ctl(&args[1..]),
        Some("plist") => match args.get(1) {
            Some(bin) => {
                print!("{}", launchd_plist(bin));
                Ok(())
            }
            None => Err("wc-broker plist BIN".into()),
        },
        Some("serve") => parse_serve(&args[1..]).and_then(serve),
        None => parse_serve(&[]).and_then(serve),
        Some(_) => Err("usage: wc-broker serve [--socket PATH] [--keys DIR] [--device NAME=PATH]... [--no-default-devices] \
                        [--release-after SECS] [--budget-mib N] | wc-broker ctl [--hold] REQUEST... | wc-broker plist BIN"
            .into()),
    };
    match r {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            log(&e);
            std::process::ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn serve_takes_its_defaults_and_overrides() {
        let a = parse_serve(&[]).unwrap();
        assert_eq!(a.release_after, RELEASE_AFTER);
        assert_eq!(a.keys, PathBuf::from(DEFAULT_KEYS));
        assert_eq!(a.devices.len(), DEFAULT_DEVICES.len());
        let a = parse_serve(&s(&[
            "--socket", "/tmp/b.sock", "--no-default-devices", "--device", "wcbell=/dev/x", "--device", "wcbell=/dev/y",
            "--release-after", "5", "--budget-mib", "64",
        ]))
        .unwrap();
        assert_eq!(a.socket, "/tmp/b.sock");
        assert_eq!(a.devices, vec![("wcbell".to_string(), PathBuf::from("/dev/y"))]);
        assert_eq!((a.release_after, a.budget), (Duration::from_secs(5), Some(64 << 20)));
        for bad in [&["--device", "nopath"][..], &["--device", "a b=/x"], &["--release-after", "x"], &["--fly"]] {
            assert!(parse_serve(&s(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_launch_daemon_is_kept_alive_and_outlasts_its_own_shutdown_wait() {
        let p = launchd_plist("/Library/Application Support/waycompose/bin/wc-broker");
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert!(p.contains("<string>/Library/Application Support/waycompose/bin/wc-broker</string><string>serve</string>"));
        let t: u64 = p.split("<key>ExitTimeOut</key><integer>").nth(1).unwrap().split('<').next().unwrap().parse().unwrap();
        assert!(Duration::from_secs(t) > TERM_LIMIT);
        assert!(launchd_plist("/a<b").contains("/a&lt;b"));
    }
}
