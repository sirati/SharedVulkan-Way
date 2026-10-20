//! The `wc-broker` program on Unix (§111.9): the daemon, and a small client
//! for scripts and tests.
//!
//! ```text
//! wc-broker serve [--socket PATH] [--keys DIR] [--device NAME=PATH]... [--service NAME=PATH]... [--no-default-devices]
//!                 [--budget-mib N]
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
//!   session's boot page, with no elapsed-time release;
//! * SIGTERM (a shutdown): new allocations stop; it retains all live and
//!   orphan regions until released, then exits. No shutdown deadline.
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

/// Host-visible wired regions have no elapsed-time release. Only the host's
/// release word proves it has stopped writing into their guest RAM.
fn region_release() -> Release {
    crate::core::HOST_VISIBLE_RELEASE
}

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

/// A root-selected PCI slice, published for exactly one uid by the same core.
#[derive(Debug, PartialEq, Eq)]
pub struct PciSlice {
    pub uid:u32, pub pool:String, pub registry:u64, pub offset:u64, pub len:u64, pub epoch:u64,
}
impl PciSlice {
    fn parse(value:&str) -> Result<Self,String> {
        let parts:Vec<_> = value.split(':').collect();
        if parts.len() != 6 { return Err("--pci-slice requires UID:POOL:REGISTRY:OFFSET:LEN:EPOCH".into()); }
        let number = |s:&str| -> Result<u64,String> {
            if let Some(hex) = s.strip_prefix("0x") { u64::from_str_radix(hex,16) } else { s.parse() }
                .map_err(|_| "invalid --pci-slice integer".into())
        };
        let pool = parts[1];
        if crate::proto::Request::parse(&format!("find {pool}")).is_err() { return Err("invalid PCI pool name".into()); }
        let uid = u32::try_from(number(parts[0])?).map_err(|_| "PCI uid exceeds u32")?;
        let slice = Self { uid, pool:pool.into(), registry:number(parts[2])?, offset:number(parts[3])?, len:number(parts[4])?, epoch:number(parts[5])? };
        if slice.len == 0 || slice.offset.checked_add(slice.len).is_none() { return Err("invalid PCI slice range".into()); }
        Ok(slice)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ServeArgs {
    pub socket: String,
    pub keys: PathBuf,
    pub devices: Vec<(String, PathBuf)>,
    /// `--service NAME=PATH`: a socket `connect NAME` reaches (any user).
    pub services: Vec<(String, PathBuf)>,
    pub budget: Option<u64>,
    pub pci_slices: Vec<PciSlice>,
}

pub fn parse_serve(argv: &[String]) -> Result<ServeArgs, String> {
    let mut a = ServeArgs {
        socket: crate::client::path(),
        keys: DEFAULT_KEYS.into(),
        devices: DEFAULT_DEVICES.iter().map(|(n, p)| (n.to_string(), PathBuf::from(p))).collect(),
        services: Vec::new(),
        budget: None,
        pci_slices: Vec::new(),
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
            "--pci-slice" => a.pci_slices.push(PciSlice::parse(&v()?)?),
            "--budget-mib" => a.budget = Some(v()?.parse::<u64>().map_err(|e| format!("--budget-mib: {e}"))? << 20),
            _ => return Err(format!("unknown argument {k}")),
        }
    }
    for (index, slice) in a.pci_slices.iter().enumerate() {
        if a.pci_slices[..index].iter().any(|old|
            (old.uid == slice.uid && old.pool == slice.pool) ||
            (old.registry == slice.registry && old.offset < slice.offset + slice.len && slice.offset < old.offset + old.len)) {
            return Err("PCI slices overlap or repeat a user's pool name".into());
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
    #[cfg(not(target_os = "macos"))]
    if !a.pci_slices.is_empty() { return Err("--pci-slice requires the Intel macOS guest driver".into()); }
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
        unix = unix.mac_keeper().with_persistent_holder(&std::env::current_exe().map_err(|e| e.to_string())?)
            .map_err(|e| format!("independent wired holder: {e}"))?;
    }
    let devices = a.devices.iter().map(|(n, _)| DeviceSpec { name: n.clone(), allow: Allow::AnyUser }).collect();
    let mut rnd = [0u8; 8];
    let mut random = std::fs::File::open("/dev/urandom").map_err(|e| format!("broker run entropy: {e}"))?;
    std::io::Read::read_exact(&mut random, &mut rnd).map_err(|e| format!("broker run entropy: {e}"))?;
    let run = u64::from_le_bytes(rnd);
    let config = Config {
        devices,
        services: a.services.iter().map(|(n, _)| DeviceSpec { name: n.clone(), allow: Allow::AnyUser }).collect(),
        keys: Some(a.keys.clone()),
        release: region_release(),
        budget: a.budget,
        run,
    };
    let broker = Broker::new(unix, config);
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::sys::macos_mach::start(broker.clone()) {
        if !a.pci_slices.is_empty() { return Err(format!("Mach PCI lending unavailable: {e}")); }
        log(&format!("Mach PCI lending unavailable: {e}"));
    }
    #[cfg(target_os = "macos")]
    for slice in &a.pci_slices {
        let id = crate::sys::macos_pci::publish(&broker, crate::core::User::uid(slice.uid), &slice.pool,
            slice.epoch, slice.registry, slice.offset, slice.len).map_err(|e| format!("publish PCI pool {}: {e}", slice.pool))?;
        log(&format!("published PCI pool {} as region {id} for uid {}", slice.pool, slice.uid));
    }
    log(&format!(
        "socket {} (0666, decided per uid), keys {}, devices {}, regions held until host release",
        a.socket,
        a.keys.display(),
        a.devices.iter().map(|(n, p)| format!("{n}={}", p.display())).collect::<Vec<_>>().join(" ")
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
                log(&format!("serving ended: {e}; retaining regions until host release"));
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
    broker.quiesce();
    log("SIGTERM: new regions refused; retaining all live and orphan regions until host release");
    loop {
        broker.tick();
        if broker.stats().regions == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    log("every region released; exiting");
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
/// (`KeepAlive`). ExitTimeOut zero disables launchd's automatic SIGKILL
/// deadline: active host mappings can require an unbounded release wait.
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
  <key>MachServices</key>
  <dict><key>org.waycompose.broker.memory</key><true/></dict>
  <key>ExitTimeOut</key><integer>0</integer>
  <key>AbandonProcessGroup</key><true/>
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
        Some("__hold-wired") => crate::sys::wired_holder::run().map_err(|e| e.to_string()),
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
        Some(_) => Err("usage: wc-broker serve [--socket PATH] [--keys DIR] [--device NAME=PATH]... [--service NAME=PATH]... [--no-default-devices] \
                        [--budget-mib N] | wc-broker ctl [--hold] REQUEST... | wc-broker plist BIN"
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
        assert!(matches!(region_release(), Release::Probe { after: None }));
        assert_eq!(a.keys, PathBuf::from(DEFAULT_KEYS));
        assert_eq!(a.devices.len(), DEFAULT_DEVICES.len());
        let a = parse_serve(&s(&[
            "--socket", "/tmp/b.sock", "--no-default-devices", "--device", "wcbell=/dev/x", "--device", "wcbell=/dev/y",
            "--budget-mib", "64",
        ]))
        .unwrap();
        assert_eq!(a.socket, "/tmp/b.sock");
        assert_eq!(a.devices, vec![("wcbell".to_string(), PathBuf::from("/dev/y"))]);
        assert_eq!(a.budget, Some(64 << 20));
        for bad in [&["--device", "nopath"][..], &["--device", "a b=/x"], &["--release-after", "x"], &["--release-after", "600"], &["--fly"]] {
            assert!(parse_serve(&s(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn host_visible_orphans_survive_elapsed_time_until_release() {
        use crate::core::{Core, Peer, User};
        use std::time::Instant;
        let mut core = Core::new(vec![]);
        let now = Instant::now();
        core.connect(1, Peer { user: User::uid(501), pid: 123, admin: false });
        let ring = core.alloc(1, "session", 8192, region_release()).unwrap();
        let arena = core.alloc(1, "session", 65536, region_release()).unwrap();
        core.conn_closed(1, now);
        // Simulate a host suspended for a month. Time cannot prove release.
        core.tick(now + Duration::from_secs(30 * 24 * 3600));
        assert_eq!(core.stats().regions, 2);
        assert_eq!(core.stats().orphans, 2);
        assert!(core.region(arena.id).is_some());
        core.pool_released(ring.id);
        assert_eq!(core.stats().regions, 0);
    }

    #[test]
    fn the_launch_daemon_does_not_force_kill_retained_regions() {
        let p = launchd_plist("/Library/Application Support/waycompose/bin/wc-broker");
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert!(p.contains("<key>AbandonProcessGroup</key><true/>"), "launchd must leave independent holders alive after broker death");
        assert!(p.contains("<string>/Library/Application Support/waycompose/bin/wc-broker</string><string>serve</string>"));
        let t: u64 = p.split("<key>ExitTimeOut</key><integer>").nth(1).unwrap().split('<').next().unwrap().parse().unwrap();
        assert_eq!(t, 0, "launchd must not SIGKILL a broker retaining host-visible RAM");
        assert!(launchd_plist("/a<b").contains("/a&lt;b"));
    }
}

#[cfg(test)]
mod pci_config_tests {
    use super::*;
    fn args(values:&[&str]) -> Vec<String> { values.iter().map(|s| s.to_string()).collect() }
    #[test]
    fn pci_configuration_checks_identity_and_ranges() {
        let parsed = parse_serve(&args(&["--pci-slice", "501:gpu:0x42:0:65536:7"])).unwrap();
        assert_eq!(parsed.pci_slices[0], PciSlice { uid:501, pool:"gpu".into(), registry:66, offset:0, len:65536, epoch:7 });
        for bad in ["501:gpu:1:0:0:1", "4294967296:gpu:1:0:1:1", "501:gpu:1:18446744073709551615:1:1", "501:gpu:1:0:1", "501:bad pool:1:0:1:1"] {
            assert!(parse_serve(&args(&["--pci-slice", bad])).is_err(), "{bad}");
        }
        assert!(parse_serve(&args(&["--pci-slice", "501:a:1:0:8192:1", "--pci-slice", "502:b:1:4096:8192:1"])).is_err());
        assert!(parse_serve(&args(&["--pci-slice", "501:a:1:0:8192:1", "--pci-slice", "502:b:1:8192:8192:1"])).is_ok());
    }
}
