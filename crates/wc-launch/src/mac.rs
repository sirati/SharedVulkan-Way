//! §103.8. `wc-launch` on macOS: a Linux application as a Mac application.
//!
//! Each Linux application is an `.app` bundle in `~/Applications/Linux`
//! whose executable is a copy of this program (`macbundle.rs`). Started from a
//! bundle, this process is that application's *face* (§103.13): its Dock
//! tile, menu bar and windows. One *main* process per user runs the
//! compositor and the session. The session starts with no ssh and no socket
//! (§103.15): the compositor writes a boot record into page 0 of its ring
//! window, the host's session service finds it, locates both windows, starts
//! the proxy and answers in the same page; everything after that is rings in
//! guest RAM (§103.14).
//!
//! `wc-launch --install SECTION... [--pin] [--meta DIR]` writes the bundles
//! (with `--pin` keeps them in the Dock) from the host's `wc-session app`
//! answers, which the install harness puts in DIR as `<host-app>.meta`;
//! `wc-launch --session SECTION` runs a face without a bundle, for the
//! harnesses.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::appmeta::Meta;
use crate::config::Config;
use crate::{macbundle, plan};

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn support_dir() -> PathBuf {
    home().join("Library/Application Support/waycompose")
}

fn log_dir() -> PathBuf {
    home().join("Library/Logs/waycompose")
}

/// The launcher's own log lines, beside the compositor's.
#[derive(Clone)]
struct Log(Arc<Mutex<Option<std::fs::File>>>);

impl Log {
    fn open(path: &Path) -> Log {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
        Log(Arc::new(Mutex::new(f)))
    }
    fn say(&self, msg: &str) {
        if let Some(f) = self.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = writeln!(f, "[launch {}] {msg}", std::process::id());
        }
        eprintln!("wc-launch: {msg}");
    }
}

fn load_config() -> Result<Config, String> {
    let path = support_dir().join("wc-launch.ini");
    let mut cfg = Config::load(&path)?;
    // The Windows default names ssh.exe; on a Mac it is the system's.
    if cfg.ssh.contains('\\') || cfg.ssh.is_empty() {
        cfg.ssh = "/usr/bin/ssh".into();
    }
    if cfg.key.is_empty() {
        cfg.key = support_dir().join("id_ed25519").to_string_lossy().into_owned();
    }
    Ok(cfg)
}

pub fn main(args: Vec<String>) -> ExitCode {
    let _ = std::fs::create_dir_all(support_dir());
    let _ = std::fs::create_dir_all(log_dir());
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wc-launch: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(i) = args.iter().position(|a| a == "--install") {
        let pin = args.iter().any(|a| a == "--pin");
        let meta_dir = args.iter().position(|a| a == "--meta").and_then(|m| args.get(m + 1)).map(PathBuf::from);
        let sections: Vec<String> = args[i + 1..]
            .iter()
            .filter(|a| !a.starts_with("--") && Some(Path::new(a)) != meta_dir.as_deref())
            .cloned()
            .collect();
        let Some(meta_dir) = meta_dir else {
            eprintln!("wc-launch: --install needs --meta DIR, the host's `wc-session app` answers (§103.15)");
            return ExitCode::from(2);
        };
        let mut ok = true;
        for s in &sections {
            match install(&cfg, s, pin, &meta_dir) {
                Ok(p) => println!("installed {s}: {}", p.display()),
                Err(e) => {
                    eprintln!("wc-launch: {s}: {e}");
                    ok = false;
                }
            }
        }
        return if ok && !sections.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) };
    }
    if args.iter().any(|a| a == "--main") {
        return run_main(cfg);
    }
    let section = if let Some(i) = args.iter().position(|a| a == "--session") {
        args.get(i + 1).cloned()
    } else {
        section_of_bundle()
    };
    let Some(section) = section else {
        eprintln!(
            "wc-launch: not started from an application bundle; use --install SECTION [--pin] \
             or --session SECTION"
        );
        return ExitCode::from(2);
    };
    run_face_mode(&cfg, &section)
}

/// The `[app.*]` section this bundle starts, from its own `Info.plist`.
fn section_of_bundle() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let contents = exe.parent()?.parent()?;
    let plist = std::fs::read_to_string(contents.join("Info.plist")).ok()?;
    macbundle::section_from_plist(&plist)
}

// ------------------------------------------------------------- install ---

fn install(cfg: &Config, section: &str, pin: bool, meta_dir: &Path) -> Result<PathBuf, String> {
    let app = cfg.app(section)?;
    let host_app = if app.host_app.is_empty() { section } else { &app.host_app };
    // The host's answer to `wc-session app HOST-APP light`, fetched by the
    // install harness on the host: installing needs no channel to it.
    let meta_path = meta_dir.join(format!("{host_app}.meta"));
    let raw = std::fs::read_to_string(&meta_path).map_err(|e| format!("{}: {e}", meta_path.display()))?;
    let meta = Meta::parse(&raw)?;
    // The host's desktop name, unless the ini gave a title of its own (the
    // parser defaults a missing title to the section id).
    let titled = !app.title.is_empty() && app.title != section;
    let name = macbundle::bundle_name(if titled { &app.title } else { &meta.name });
    let bundle = home().join(macbundle::APPS_DIR).join(format!("{name}.app"));
    let contents = bundle.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS")).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(contents.join("Resources")).map_err(|e| e.to_string())?;
    std::fs::write(contents.join("Info.plist"), macbundle::info_plist(section, &name)).map_err(|e| e.to_string())?;
    // The host's answer, kept so the Dock menu exists before the first
    // session has asked again.
    std::fs::write(contents.join("Resources/meta.txt"), &raw).map_err(|e| e.to_string())?;
    if let Some(png) = &meta.icon_png {
        let sq = macbundle::square_png(png, 256)?;
        std::fs::write(contents.join("Resources/app.icns"), macbundle::icns(&sq, 256)?).map_err(|e| e.to_string())?;
    }
    // A copy, not a hard link. With every bundle's executable one inode,
    // LaunchServices took a second application's launch for the first one
    // already running and only activated it (§103.13, measured: Console did
    // not start while Firefox ran, and did with a copy). The executable must
    // also be *inside* the bundle for the process to take its identity.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = contents.join("MacOS").join(macbundle::EXECUTABLE);
    let tmp = target.with_extension("new");
    std::fs::copy(&exe, &tmp).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
    // LaunchServices learns the bundle now rather than at the next login.
    let _ = Command::new(
        "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
    )
    .args(["-f", &bundle.to_string_lossy()])
    .status();
    if pin {
        pin_in_dock(&bundle)?;
    }
    Ok(bundle)
}

/// "Keep in Dock", done for the user: the same entry the Dock writes when a
/// person chooses it from the tile's menu.
fn pin_in_dock(bundle: &Path) -> Result<(), String> {
    let url = format!("file://{}/", bundle.to_string_lossy().replace(' ', "%20"));
    let have = Command::new("defaults").args(["read", "com.apple.dock", "persistent-apps"]).output();
    if let Ok(o) = have
        && String::from_utf8_lossy(&o.stdout).contains(&url)
    {
        return Ok(());
    }
    let entry = format!(
        "<dict><key>tile-data</key><dict><key>file-data</key><dict><key>_CFURLString</key>\
         <string>{url}</string><key>_CFURLStringType</key><integer>15</integer></dict></dict>\
         <key>tile-type</key><string>file-tile</string></dict>"
    );
    let st = Command::new("defaults")
        .args(["write", "com.apple.dock", "persistent-apps", "-array-add", &entry])
        .status()
        .map_err(|e| e.to_string())?;
    if !st.success() {
        return Err("defaults write com.apple.dock failed".into());
    }
    let _ = Command::new("killall").arg("Dock").status();
    Ok(())
}

// ------------------------------------------------------------- session ---

struct Session {
    id: String,
    /// The control channel (§103.14), once the rings are up.
    ctl: Mutex<Option<sv_transport::wlbytes::ByteChannel>>,
    up: Mutex<bool>,
    /// Requests that arrived before the session was up: `(section, key)`.
    pending: Mutex<Vec<(String, String)>>,
}

/// §103.13: the installed launcher, which the main process runs from: not a
/// copy inside some application's bundle, whose identity it would take.
fn installed_exe() -> PathBuf {
    let p = support_dir().join("bin/wc-launch");
    if p.is_file() { p } else { std::env::current_exe().unwrap_or(p) }
}

/// The main process (§103.13): the compositor with the router platform, one
/// session for every application, and the faces' requests.
fn run_main(cfg: Config) -> ExitCode {
    let id = plan::run_id(&cfg.session, std::process::id());
    let log = Log::open(&log_dir().join("main.log"));
    log.say(&format!("main process: session {id}"));
    let tmp = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    let rt = tmp.join(format!("wc-{id}"));
    let _ = std::fs::create_dir_all(&rt);
    let _ = std::fs::set_permissions(&rt, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let socket = format!("wl-{id}");
    // SAFETY: before any thread of this process exists.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &rt);
        std::env::set_var("WC_MAC_ROLE", "main");
        std::env::set_var("WC_FACE_LATENCY", "1");
    }
    let comp_log = log_dir().join("main.compositor.log");
    let _ = std::fs::remove_file(&comp_log);
    let session = Arc::new(Session {
        id: id.clone(),
        ctl: Mutex::new(None),
        up: Mutex::new(false),
        pending: Mutex::new(Vec::new()),
    });
    {
        let (c1, s1, l1) = (cfg.clone(), session.clone(), log.clone());
        let l2 = log.clone();
        wc_compositor::set_face_hooks(wc_compositor::FaceHooks {
            on_request: Box::new(move |section: &str, key: &str| request(&c1, &s1, &l1, section, key)),
            on_face_lost: Box::new(move |section: &str| {
                // Started again in the background; it asks for its windows
                // back when it says hello (macos_router).
                l2.say(&format!("face {section} crashed; starting it again"));
                let (bid, l3) = (macbundle::bundle_id(section), l2.clone());
                // LaunchServices may still list the dead process for a moment.
                std::thread::spawn(move || {
                    for attempt in 1..=5u64 {
                        std::thread::sleep(Duration::from_millis(500 * attempt));
                        let ok = Command::new("/usr/bin/open").args(["-g", "-b", &bid]).status().is_ok_and(|s| s.success());
                        if ok {
                            l3.say(&format!("{bid} started again (attempt {attempt})"));
                            return;
                        }
                    }
                    l3.say(&format!("{bid} could not be started again"));
                });
            }),
        });
    }
    let worker = {
        let (cfg, session, log) = (cfg.clone(), session.clone(), log.clone());
        std::thread::Builder::new().name("session".into()).spawn(move || {
            match bring_up(&cfg, &session, &log) {
                Ok(()) => {
                    *session.up.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    let queued: Vec<(String, String)> =
                        std::mem::take(&mut *session.pending.lock().unwrap_or_else(|e| e.into_inner()));
                    for (section, key) in queued {
                        request(&cfg, &session, &log, &section, &key);
                    }
                }
                Err(e) => log.say(&format!("session failed: {e}")),
            }
        })
    };
    if let Err(e) = worker {
        log.say(&format!("could not start the session thread: {e}"));
        return ExitCode::from(1);
    }
    // The arena backend (§103.13): one arena per main process, so one per
    // user session, shared by every application. Guest RAM when macOS is the
    // VM (role (a)); with WC_MAC_ARENA_STANDIN=<file>, a file the Linux guest
    // maps too (role (b): a Mac hosting the Linux VM, the file on a RAM disk
    // the hypervisor shares), which the compositor maps as its "ivshmem"
    // device and the host side checks with `up ID RUNID ivshmem`.
    let standin = std::env::var("WC_MAC_ARENA_STANDIN").ok().filter(|s| !s.is_empty());
    let arena = cfg.arena_mib.max(32) * 1024 * 1024;
    // §103.14: the Wayland connections, the feeds, the control and the sound
    // ride rings in a second window of this process's RAM.
    let mut argv: Vec<String> =
        vec!["wc-launch".into(), "--socket".into(), socket, "--in-band".into(), "--wayland-rings-guest-ram".into()];
    // §103.15: the rings' doorbell, a legacy virtio-serial port's `cu.*` node
    // (or an `fd:R,W` pair a parent opened). Without one the rings are polled
    // and the host's service finds the boot record by its periodic sweep.
    let bell = std::env::var("WC_MAC_BELL").ok().unwrap_or_else(|| cfg.wayland_bell.clone());
    if !bell.is_empty() {
        argv.extend(["--wayland-rings".into(), bell]);
    }
    // §103.15: the session's GPU server, started here rather than by a
    // harness's ssh. Its cold page carries the session's label; the host's
    // session service serves it once the boot record says so.
    let gpu = start_gpu_server(&cfg, &id, &log);
    wc_compositor::set_boot_session(&id, cfg.outbound, gpu.is_some());
    match &standin {
        Some(file) => {
            // SAFETY: before the compositor's threads exist.
            unsafe { std::env::set_var("WC_IVSHMEM_STANDIN", file) };
            argv.extend(["--arena-ivshmem".into(), "0".into()]);
        }
        None => argv.extend(["--arena-guest-ram".into(), format!("0:{arena}")]),
    }
    argv.extend([
        "--arena-outbound".into(),
        format!("0:{}", cfg.outbound),
        "--exit-when-empty".into(),
        cfg.linger_secs.clamp(5, 600).to_string(),
        "--log".into(),
        comp_log.to_string_lossy().into_owned(),
    ]);
    let code = wc_compositor::run_from(argv);
    log.say("the compositor has ended; taking the session down");
    if *session.up.lock().unwrap_or_else(|e| e.into_inner()) {
        match control_request(&session, "down") {
            Ok(r) => log.say(&format!("down over the control ring: {r}")),
            Err(e) => log.say(&format!("down: {e}")),
        }
    }
    if let Some(mut g) = gpu {
        let _ = g.kill();
        let _ = g.wait();
    }
    let _ = std::fs::remove_dir_all(&rt);
    code
}

/// `sv-guest gpu guest --label <session>`, for the life of the main process.
fn start_gpu_server(cfg: &Config, id: &str, log: &Log) -> Option<std::process::Child> {
    if cfg.gpu_server.is_empty() {
        return None;
    }
    let label = wc_handle::macboot::gpu_label(id);
    let out = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(log_dir().join("gpu.log"));
    let mut c = Command::new(&cfg.gpu_server);
    c.args(["gpu", "guest", "--label", label, "--arena"])
        .arg((cfg.gpu_arena_mib.max(16) * 1024 * 1024).to_string())
        .args(["--idle", "43200"])
        .stdin(Stdio::null());
    // Its rings' doorbell (§103.15): the second legacy virtio-serial port,
    // when the VM has it; without it the GPU rings poll.
    let bell = if cfg.gpu_bell.is_empty() { None } else { Some(format!("/dev/{}", cfg.gpu_bell)) };
    if let Some(dev) = bell.as_deref().filter(|d| Path::new(d).exists()) {
        c.args(["--bell", &format!("dev:{}", &dev[5..])]);
    }
    if !cfg.gpu_icd.is_empty() {
        c.env("VK_DRIVER_FILES", &cfg.gpu_icd);
    }
    if let Ok(f) = out {
        if let Ok(f2) = f.try_clone() {
            c.stdout(f2);
        }
        c.stderr(f);
    }
    match c.spawn() {
        Ok(child) => {
            log.say(&format!("gpu server {} started (label {label})", cfg.gpu_server));
            Some(child)
        }
        Err(e) => {
            log.say(&format!("NO GPU: {}: {e}", cfg.gpu_server));
            None
        }
    }
}

/// A face's request (§103.13): run its application, an action or a URL, in
/// the one session. Queued until the session is up.
fn request(cfg: &Config, s: &Arc<Session>, log: &Log, section: &str, key: &str) {
    if !*s.up.lock().unwrap_or_else(|e| e.into_inner()) {
        s.pending.lock().unwrap_or_else(|e| e.into_inner()).push((section.to_string(), key.to_string()));
        return;
    }
    let host_app = match cfg.app(section) {
        Ok(a) if !a.host_app.is_empty() => a.host_app.clone(),
        Ok(_) => section.to_string(),
        Err(e) => {
            log.say(&format!("request from face {section}: {e}"));
            return;
        }
    };
    let (s, log, key, section) = (s.clone(), log.clone(), key.to_string(), section.to_string());
    std::thread::spawn(move || {
        // The words wc-session takes, less the session id, which the host's
        // end of the channel puts in itself (macsvc.rs).
        let verb = plan::launch_verb("ID", &host_app, macbundle::request_for(&key), 96, None);
        let words: Vec<&str> = verb.iter().map(String::as_str).filter(|w| *w != "ID").collect();
        let line = words.join(" ");
        match control_request(&s, &line) {
            Ok(r) => log.say(&format!("{section} {:?}: {r}", if key.is_empty() { "run" } else { &key })),
            Err(e) => log.say(&e),
        }
    });
}

/// Send one request on the control ring and wait for its reply line.
fn control_request(s: &Session, line: &str) -> Result<String, String> {
    use sv_transport::wlbytes::ReadOutcome;
    let mut g = s.ctl.lock().unwrap_or_else(|e| e.into_inner());
    let ch = g.as_mut().ok_or("no control channel")?;
    let t0 = Instant::now();
    let mut out = format!("{line}\n").into_bytes();
    while !out.is_empty() {
        match ch.write(&out) {
            Ok(n) if n > 0 => {
                out.drain(..n);
            }
            Ok(_) if t0.elapsed() < Duration::from_secs(2) => std::thread::sleep(Duration::from_millis(1)),
            Ok(_) => return Err("the control ring is full".into()),
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut reply = Vec::new();
    let mut buf = [0u8; 4096];
    while !reply.ends_with(b"\n") {
        match ch.read(&mut buf) {
            Ok(ReadOutcome::Bytes(n)) => reply.extend_from_slice(&buf[..n]),
            Ok(ReadOutcome::Eof) => return Err("the control ring ended".into()),
            Ok(_) if t0.elapsed() < Duration::from_secs(60) => {
                let _ = ch.wait_readable(wc_compositor::ring_wait_slice());
            }
            Ok(_) => return Err("no reply on the control ring".into()),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(format!("{} ({:.1} ms round trip)", String::from_utf8_lossy(&reply).trim(), t0.elapsed().as_secs_f64() * 1000.0))
}

/// The ring slot the host claimed for a session service, once it has.
fn service_channel(owner: u64, wait: Duration) -> Option<sv_transport::wlbytes::ByteChannel> {
    use sv_transport::wlbytes::{ByteChannel, Endpoint};
    let t0 = Instant::now();
    while t0.elapsed() < wait {
        if let Some(c) = wc_compositor::shared_control()
            && let Ok(live) = ByteChannel::live_connections(&c)
        {
            for slot in live {
                if c.ring_owner(slot).ok() == Some(owner)
                    && let Ok(ch) = ByteChannel::open(&c, slot, Endpoint::Peer)
                {
                    return Some(ch);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// The sound channel as a `Read`, for the CoreAudio player.
struct RingReader(sv_transport::wlbytes::ByteChannel);

impl std::io::Read for RingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use sv_transport::wlbytes::ReadOutcome;
        loop {
            match self.0.read(buf) {
                Ok(ReadOutcome::Bytes(n)) => return Ok(n),
                Ok(ReadOutcome::Eof) => return Ok(0),
                Ok(_) => {
                    let _ = self.0.wait_readable(wc_compositor::ring_wait_slice());
                }
                Err(e) => return Err(std::io::Error::other(e.to_string())),
            }
        }
    }
}

/// A face (§103.13): the application's Dock tile, menu bar and windows, as a
/// small process that asks the main process for everything else, starting
/// it first if there is none.
fn run_face_mode(cfg: &Config, section: &str) -> ExitCode {
    wc_compositor::face_early();
    let log = Log::open(&log_dir().join(format!("{section}.log")));
    if let Err(e) = cfg.app(section) {
        log.say(&e);
        return ExitCode::from(2);
    }
    let path = wc_compositor::socket_path();
    let t0 = Instant::now();
    let mut started = false;
    let stream = loop {
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(s) => break s,
            Err(_) if !started => {
                started = true;
                log.say("no main process; starting one");
                let out = std::fs::OpenOptions::new().create(true).append(true).open(log_dir().join("main.stderr.log"));
                let mut c = Command::new(installed_exe());
                c.arg("--main").stdin(Stdio::null());
                if let Ok(f) = out {
                    if let Ok(f2) = f.try_clone() {
                        c.stdout(f2);
                    }
                    c.stderr(f);
                }
                // Its own process group, so it outlives this face and no
                // signal for this face's group reaches it.
                std::os::unix::process::CommandExt::process_group(&mut c, 0);
                if let Err(e) = c.spawn() {
                    log.say(&format!("could not start the main process: {e}"));
                    return ExitCode::from(1);
                }
            }
            Err(e) if t0.elapsed() > Duration::from_secs(20) => {
                log.say(&format!("the main process never listened on {}: {e}", path.display()));
                return ExitCode::from(1);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    // The answer the installer kept: the Dock menu, and the ids this face's
    // windows will carry.
    let meta = section_of_bundle()
        .and_then(|_| std::env::current_exe().ok())
        .and_then(|e| std::fs::read_to_string(e.parent()?.parent()?.join("Resources/meta.txt")).ok())
        .and_then(|raw| Meta::parse(&raw).ok());
    let host_app = cfg.app(section).map(|a| a.host_app.clone()).unwrap_or_default();
    let ids = wc_compositor::face_ids(
        section,
        &host_app,
        meta.as_ref().map_or("", |m| m.desktop.as_str()),
        meta.as_ref().and_then(|m| m.wm_class.as_deref()),
    );
    let dock = meta
        .as_ref()
        .map(|m| macbundle::dock_items(m, &[]))
        .unwrap_or_default()
        .into_iter()
        .map(|(title, key)| wc_compositor::DockItem { title, key })
        .collect();
    log.say(&format!("face {section}: connected in {:?}, answers to {ids:?}", t0.elapsed()));
    let code = wc_compositor::run_face(stream, section, ids, dock);
    log.say(&format!("face {section}: ends ({code})"));
    ExitCode::from(code as u8)
}

/// §103.15: wait for the host's session service to answer the boot record,
/// then open the control channel it claimed, hold the lease, and play the
/// sound. Nothing here reaches the host but through guest RAM.
fn bring_up(cfg: &Config, s: &Arc<Session>, log: &Log) -> Result<(), String> {
    let t0 = Instant::now();
    if std::env::var_os("WC_IVSHMEM_STANDIN").is_some() {
        // Role (b)'s boot (a shared-file arena) is the real Mac's launcher's.
        return Err("WC_MAC_ARENA_STANDIN: this launcher boots guest-RAM sessions only (§103.15)".into());
    }
    let wait = Duration::from_secs(std::env::var("WC_MAC_BOOT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60));
    loop {
        match wc_compositor::boot_status() {
            Some(Ok(())) => break,
            Some(Err(why)) => return Err(format!("the host's session service refused session {}: {why}", s.id)),
            None if t0.elapsed() > wait => {
                return Err(format!(
                    "no answer to the boot record in {wait:?}: the host's session service \
                     (wc-mac-sessiond) is not running, or cannot see this VM"
                ));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    log.say(&format!("session {} located and started by the host in {:?}", s.id, t0.elapsed()));
    let ctl = service_channel(sv_transport::wlbytes::OWNER_CONTROL, Duration::from_secs(20))
        .ok_or("the host never opened the control ring")?;
    *s.ctl.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctl);
    {
        // The lease the host's proxy holds this process to (macsvc.rs): a
        // ping every half second, or it stops touching this RAM.
        let s2 = Arc::clone(s);
        std::thread::Builder::new().name("lease".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            let mut g = s2.ctl.lock().unwrap_or_else(|e| e.into_inner());
            let Some(ch) = g.as_mut() else { return };
            if ch.write(b"ping\n").is_err() {
                return;
            }
        }).ok();
    }
    if cfg.audio {
        let (vol, log2) = (cfg.audio_volume, log.clone());
        std::thread::Builder::new().name("audio".into()).spawn(move || {
            match service_channel(sv_transport::wlbytes::OWNER_AUDIO, Duration::from_secs(20)) {
                Some(ch) => {
                    log2.say("audio: playing the session's sound from its ring through CoreAudio");
                    match crate::audio_mac::play(RingReader(ch), vol) {
                        Ok(st) => log2.say(&st.line()),
                        Err(e) => log2.say(&format!("audio: {e}")),
                    }
                }
                None => log2.say("audio: the host opened no sound ring"),
            }
        }).ok();
    }
    log.say(&format!("session up {:?} after start", t0.elapsed()));
    Ok(())
}

