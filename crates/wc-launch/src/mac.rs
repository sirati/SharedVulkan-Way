//! §103.8. `wc-launch` on macOS: a Linux application as a Mac application.
//!
//! Each Linux application is an `.app` bundle in `~/Applications/Linux`
//! whose executable is a hard link to this program (`macbundle.rs`). Started
//! from a bundle, this process **is** that application: it runs the
//! compositor on its main thread, so the application's windows, its Dock tile,
//! its Dock menu and its menu bar all belong to one process with the
//! application's bundle identity, and it talks to the Linux host over the
//! forced-command ssh key exactly as the Windows launcher does (§28, §94):
//! `up` (locate the guest-RAM arena), a reverse forward of the compositor's
//! Wayland socket, `proxy`, then `run`, `action` or `open`.
//!
//! One compositor per application rather than one per user is §103.8's
//! decision: the Dock knows only processes, so the process that owns an
//! application's windows has to be that application's. The cost is one arena
//! and one proxy per running application; the gains are an exact Dock
//! identity and one application's crash not taking the others with it.
//!
//! `wc-launch --install SECTION [--pin]` writes the bundle (and with `--pin`
//! keeps it in the Dock); `wc-launch --session SECTION` runs one without a
//! bundle, for the harnesses.

use std::io::{BufRead, Write};
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

fn known_hosts() -> String {
    support_dir().join("known_hosts").to_string_lossy().into_owned()
}

/// One `wc-session` verb; its stdout, or the reason it failed.
fn ssh_verb(cfg: &Config, verb: &[&str]) -> Result<String, String> {
    let out = Command::new(&cfg.ssh)
        .args(plan::ssh_exec(cfg, &known_hosts(), verb))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", cfg.ssh))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{} failed ({}): {}",
            verb.first().copied().unwrap_or("?"),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

pub fn main(args: Vec<String>) -> ExitCode {
    // §105: --hostint-probe, --render-test, --capture-test (mac_hostint.rs).
    if let Some(code) = crate::mac_hostint::cli(&args) {
        return code;
    }
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
        let sections: Vec<String> = args[i + 1..].iter().filter(|a| !a.starts_with("--")).cloned().collect();
        let mut ok = true;
        for s in &sections {
            match install(&cfg, s, pin) {
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

fn install(cfg: &Config, section: &str, pin: bool) -> Result<PathBuf, String> {
    let app = cfg.app(section)?;
    let host_app = if app.host_app.is_empty() { section } else { &app.host_app };
    let raw = ssh_verb(cfg, &["app", host_app, "light"])?;
    let meta = Meta::parse(&raw)?;
    // The host's desktop name, unless the ini gave a title of its own (the
    // parser defaults a missing title to the section id).
    let titled = !app.title.is_empty() && app.title != section;
    let name = macbundle::bundle_name(if titled { &app.title } else { &meta.name });
    let bundle = home().join(macbundle::APPS_DIR).join(format!("{name}.app"));
    let contents = bundle.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS")).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(contents.join("Resources")).map_err(|e| e.to_string())?;
    // §105: with the desktop entry's document and URL types.
    std::fs::write(contents.join("Info.plist"), macbundle::info_plist_for(section, &name, &meta.mimes)).map_err(|e| e.to_string())?;
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
    // §105: the main process's own bundle, org.waycompose.host (TCC rule 1).
    crate::mac_hostint::install_host_bundle(&exe)?;
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

/// Where a main process records its session, so the next one can clean up
/// after a crash: `<session id> <tunnel pid>`.
fn session_record() -> PathBuf {
    wc_compositor::socket_path().with_file_name("session")
}

/// A main process that died without its goodbye (SIGKILL, a panic) leaves
/// its ssh tunnel running here and its proxy and sound on the host. The next
/// main process ends both, by the pid and the session id it recorded.
fn clean_stale_session(cfg: &Config, log: &Log) {
    let Ok(text) = std::fs::read_to_string(session_record()) else { return };
    let mut w = text.split_whitespace();
    let (Some(old), pid) = (w.next(), w.next().and_then(|p| p.parse::<u32>().ok())) else { return };
    if let Some(pid) = pid {
        // Only if that pid is still an ssh: pids are reused.
        let comm = Command::new("/bin/ps").args(["-p", &pid.to_string(), "-o", "comm="]).output();
        if comm.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim().ends_with("ssh")) {
            let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
        }
    }
    match ssh_verb(cfg, &["down", old]) {
        Ok(_) => log.say(&format!("cleaned up session {old}, left by a main process that did not end cleanly")),
        Err(e) => log.say(&format!("stale session {old}: {e}")),
    }
    let _ = std::fs::remove_file(session_record());
}

/// The main process (§103.13): the compositor with the router platform, one
/// session for every application, and the faces' requests.
fn run_main(cfg: Config) -> ExitCode {
    let id = plan::run_id(&cfg.session, std::process::id());
    let log = Log::open(&log_dir().join("main.log"));
    log.say(&format!("main process: session {id}"));
    clean_stale_session(&cfg, &log);
    let tmp = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    let rt = tmp.join(format!("wc-{id}"));
    let _ = std::fs::create_dir_all(&rt);
    let _ = std::fs::set_permissions(&rt, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let socket = format!("wl-{id}");
    let socket_path = rt.join(&socket);
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
        let (cfg, session, log, comp_log, socket_path) =
            (cfg.clone(), session.clone(), log.clone(), comp_log.clone(), socket_path.clone());
        std::thread::Builder::new().name("session".into()).spawn(move || {
            match bring_up(&cfg, &session, &log, &comp_log, &socket_path) {
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
    let _ = std::fs::remove_dir_all(&rt);
    let _ = std::fs::remove_file(session_record());
    code
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
    // §105: a document from the Mac goes to Linux by its Linux path.
    let key = match crate::mac_hostint::map_open_key(cfg, key) {
        Ok(k) => k,
        Err(e) => return log.say(&format!("{section}: cannot open in Linux: {e}")),
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
            Ok(_) if t0.elapsed() < Duration::from_secs(60) => std::thread::sleep(Duration::from_millis(1)),
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
        if let Some(c) = wc_compositor::shared_control() {
            if let Ok(live) = ByteChannel::live_connections(&c) {
                for slot in live {
                    if c.ring_owner(slot).ok() == Some(owner)
                        && let Ok(ch) = ByteChannel::open(&c, slot, Endpoint::Peer)
                    {
                        return Some(ch);
                    }
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
                Ok(_) => std::thread::sleep(Duration::from_millis(1)),
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
                // §105: from its own bundle, through Launch Services (TCC rule 1).
                if crate::mac_hostint::start_main_in_host_bundle(&log_dir().join("main.stderr.log"), &|m| log.say(m)) {
                    continue;
                }
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

/// Wait for a line in the compositor's log and return it.
fn wait_line(path: &Path, needle: &str, timeout: Duration) -> Option<String> {
    let t0 = Instant::now();
    while t0.elapsed() < timeout {
        if let Ok(f) = std::fs::File::open(path) {
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                if line.contains(needle) {
                    return Some(line);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Play the session's sound (§86) through CoreAudio, for as long as the
/// forward delivers it.
fn start_audio(port: u16, vol: u8, log: &Log) {
    let log = log.clone();
    let _ = std::thread::Builder::new().name("audio".into()).spawn(move || {
        let mut conn = None;
        for _ in 0..40 {
            if let Ok(c) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                conn = Some(c);
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let Some(c) = conn else {
            log.say(&format!("audio: nothing on 127.0.0.1:{port}"));
            return;
        };
        log.say(&format!("audio: playing s16le 48000 Hz stereo through CoreAudio, from 127.0.0.1:{port}"));
        match crate::audio_mac::play(c, vol) {
            Ok(st) => log.say(&st.line()),
            Err(e) => log.say(&format!("audio: {e}")),
        }
    });
}

fn bring_up(cfg: &Config, s: &Arc<Session>, log: &Log, comp_log: &Path, _socket_path: &Path) -> Result<(), String> {
    let t0 = Instant::now();
    let up = if std::env::var_os("WC_IVSHMEM_STANDIN").is_some() {
        let line = wait_line(comp_log, "header published: run", Duration::from_secs(30))
            .ok_or("the compositor never published its arena header")?;
        let run = line.split("run ").nth(1).and_then(|r| r.split(',').next()).unwrap_or("").to_string();
        wait_line(comp_log, "listening on", Duration::from_secs(30)).ok_or("the compositor never listened")?;
        log.say(&format!("compositor up in {:?}: shared-file arena, run {run}", t0.elapsed()));
        ssh_verb(cfg, &["up", &s.id, &run, "ivshmem"])?
    } else {
        let arena = wait_line(comp_log, "provided from guest RAM: run", Duration::from_secs(30))
            .ok_or("the compositor never provided its arena")?;
        let (run, pages) = macbundle::arena_of(&arena).ok_or_else(|| format!("cannot read the arena line: {arena}"))?;
        wait_line(comp_log, "listening on", Duration::from_secs(30)).ok_or("the compositor never listened")?;
        log.say(&format!("compositor up in {:?}: run {run}, {pages} pages", t0.elapsed()));
        let out = cfg.outbound.to_string();
        ssh_verb(cfg, &["up", &s.id, &run, &pages, &out])?
    };
    log.say(&format!("up: {}", up.trim().replace('\n', "; ")));
    // For the next main process, should this one die without its goodbye.
    let _ = std::fs::write(session_record(), format!("{}\n", s.id));

    // §103.14: the ring region, located once (the bootstrap's second and last
    // locate), then made into the control region; then the proxy, which
    // attaches it. These three ssh execs are the whole of ssh's part: no
    // tunnel, no forward, nothing after them.
    let rings = wait_line(comp_log, "wayland rings in guest RAM: run", Duration::from_secs(30))
        .ok_or("the compositor kept no guest-RAM rings")?;
    let rrun = rings.split("run ").nth(1).and_then(|r| r.split(',').next()).unwrap_or("").to_string();
    let rpages = rings.split(", ").nth(1).and_then(|p| p.split(' ').next()).unwrap_or("").to_string();
    let r = ssh_verb(cfg, &["rings", &s.id, &rrun, &rpages])?;
    log.say(&format!("rings: {}", r.trim().replace('\n', "; ")));
    wc_compositor::guest_ram_rings_located();
    let o = ssh_verb(cfg, &["proxy", &s.id])?;
    log.say(&format!("proxy: {}", o.trim().replace('\n', "; ")));

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
    // §105 host integration: BEGIN (the rest is in mac_hostint.rs)
    {
        let (cfg2, log2) = (cfg.clone(), log.clone());
        std::thread::Builder::new().name("hostint-open".into()).spawn(move || {
            match service_channel(sv_transport::wlbytes::OWNER_HOSTINT, Duration::from_secs(20)) {
                Some(ch) => crate::mac_hostint::start(ch, &cfg2, Box::new(move |m: &str| log2.say(m))),
                None => log2.say("hostint: the host opened no host-integration ring"),
            }
        }).ok();
    }
    // §105 host integration: END
    log.say(&format!("session up {:?} after start", t0.elapsed()));
    Ok(())
}

