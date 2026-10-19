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
use std::process::{Child, Command, ExitCode, Stdio};
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
    run_app(cfg, &section)
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
    std::fs::write(contents.join("Info.plist"), macbundle::info_plist(section, &name)).map_err(|e| e.to_string())?;
    // The host's answer, kept so the Dock menu exists before the first
    // session has asked again.
    std::fs::write(contents.join("Resources/meta.txt"), &raw).map_err(|e| e.to_string())?;
    if let Some(png) = &meta.icon_png {
        let sq = macbundle::square_png(png, 256)?;
        std::fs::write(contents.join("Resources/app.icns"), macbundle::icns(&sq, 256)?).map_err(|e| e.to_string())?;
    }
    // A hard link, so every bundle runs the one installed launcher and an
    // update is one file. The executable must be *inside* the bundle (not a
    // symlink out of it) for the process to take the bundle's identity.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = contents.join("MacOS").join(macbundle::EXECUTABLE);
    let _ = std::fs::remove_file(&target);
    if std::fs::hard_link(&exe, &target).is_err() {
        std::fs::copy(&exe, &target).map_err(|e| e.to_string())?;
    }
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
    /// This session's loopback port for its sound. Not the ini's fixed one:
    /// every running application has a session and a tunnel of its own, and a
    /// second tunnel that cannot bind the first one's port fails whole
    /// (ExitOnForwardFailure), Wayland socket included.
    audio_port: u16,
    host_app: String,
    tunnel: Mutex<Option<Child>>,
    up: Mutex<bool>,
}

fn run_app(cfg: Config, section: &str) -> ExitCode {
    let app = match cfg.app(section) {
        Ok(a) => a.clone(),
        Err(e) => {
            eprintln!("wc-launch: {e}");
            return ExitCode::from(2);
        }
    };
    let host_app = if app.host_app.is_empty() { section.to_string() } else { app.host_app.clone() };
    let id = plan::run_id(&cfg.session, std::process::id());
    let log_path = log_dir().join(format!("{section}.log"));
    let log = Log::open(&log_path);
    log.say(&format!("{section}: session {id}, host app {host_app}"));

    // The compositor's runtime directory, short enough for a Unix socket path
    // (104 bytes on macOS).
    let tmp = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    let rt = tmp.join(format!("wc-{id}"));
    let _ = std::fs::create_dir_all(&rt);
    let _ = std::fs::set_permissions(&rt, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let socket = format!("wl-{id}");
    let socket_path = rt.join(&socket);
    // SAFETY: before any thread of this process exists; the compositor reads
    // it when it binds its socket.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &rt) };
    let comp_log = log_dir().join(format!("{section}.compositor.log"));
    let _ = std::fs::remove_file(&comp_log);

    let audio_port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or_else(|_| cfg.audio_port());
    let session = Arc::new(Session {
        id: id.clone(),
        audio_port,
        host_app: host_app.clone(),
        tunnel: Mutex::new(None),
        up: Mutex::new(false),
    });

    // The Dock menu, from the answer the installer kept, and what a pick does.
    let cached = section_of_bundle()
        .and_then(|_| std::env::current_exe().ok())
        .and_then(|e| std::fs::read_to_string(e.parent()?.parent()?.join("Resources/meta.txt")).ok())
        .and_then(|raw| Meta::parse(&raw).ok());
    {
        let items = cached.as_ref().map(|m| macbundle::dock_items(m, &[])).unwrap_or_default();
        set_dock(&cfg, &session, &log, items);
    }

    let worker = {
        let (cfg, session, log, comp_log, socket_path) =
            (cfg.clone(), session.clone(), log.clone(), comp_log.clone(), socket_path.clone());
        std::thread::Builder::new()
            .name("session".into())
            .spawn(move || {
                if let Err(e) = bring_up(&cfg, &session, &log, &comp_log, &socket_path) {
                    log.say(&format!("session failed: {e}"));
                }
            })
    };
    if let Err(e) = worker {
        log.say(&format!("could not start the session thread: {e}"));
        return ExitCode::from(1);
    }

    let arena = cfg.arena_mib.max(32) * 1024 * 1024;
    let argv: Vec<String> = vec![
        "wc-launch".into(),
        "--socket".into(),
        socket,
        "--in-band".into(),
        "--arena-guest-ram".into(),
        format!("0:{arena}"),
        "--arena-outbound".into(),
        format!("0:{}", cfg.outbound),
        "--exit-when-empty".into(),
        cfg.linger_secs.clamp(5, 600).to_string(),
        "--log".into(),
        comp_log.to_string_lossy().into_owned(),
    ];
    // The application's windows, its Dock tile and its menu bar: this thread
    // is AppKit's until the last window has closed and lingered.
    let code = wc_compositor::run_from(argv);
    log.say("the compositor has ended; taking the session down");
    if let Some(mut t) = session.tunnel.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = t.kill();
        let _ = t.wait();
    }
    if *session.up.lock().unwrap_or_else(|e| e.into_inner()) {
        match ssh_verb(&cfg, &["down", &session.id]) {
            Ok(_) => log.say("down ok"),
            Err(e) => log.say(&e),
        }
    }
    let _ = std::fs::remove_dir_all(&rt);
    code
}

fn set_dock(cfg: &Config, session: &Arc<Session>, log: &Log, items: Vec<(String, String)>) {
    let (cfg, session, log) = (cfg.clone(), session.clone(), log.clone());
    let items = items
        .into_iter()
        .map(|(title, key)| wc_compositor::DockItem { title, key })
        .collect();
    wc_compositor::set_dock_menu(
        items,
        Box::new(move |key: &str| {
            let (cfg, session, log, key) = (cfg.clone(), session.clone(), log.clone(), key.to_string());
            std::thread::spawn(move || {
                if !*session.up.lock().unwrap_or_else(|e| e.into_inner()) {
                    log.say("a Dock request before the session is up; ignored");
                    return;
                }
                let verb = plan::launch_verb(&session.id, &session.host_app, macbundle::request_for(&key), 96, None);
                let words: Vec<&str> = verb.iter().map(String::as_str).collect();
                match ssh_verb(&cfg, &words) {
                    Ok(_) => log.say(&format!("Dock request {:?} ok", if key.is_empty() { "run" } else { &key })),
                    Err(e) => log.say(&e),
                }
            });
        }),
    );
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

fn bring_up(cfg: &Config, s: &Session, log: &Log, comp_log: &Path, socket_path: &Path) -> Result<(), String> {
    let t0 = Instant::now();
    let arena = wait_line(comp_log, "provided from guest RAM: run", Duration::from_secs(30))
        .ok_or("the compositor never provided its arena")?;
    let (run, pages) = macbundle::arena_of(&arena).ok_or_else(|| format!("cannot read the arena line: {arena}"))?;
    wait_line(comp_log, "listening on", Duration::from_secs(30)).ok_or("the compositor never listened")?;
    log.say(&format!("compositor up in {:?}: run {run}, {pages} pages", t0.elapsed()));

    let out = cfg.outbound.to_string();
    let up = ssh_verb(cfg, &["up", &s.id, &run, &pages, &out])?;
    *s.up.lock().unwrap_or_else(|e| e.into_inner()) = true;
    log.say(&format!("up: {}", up.trim().replace('\n', "; ")));

    // The Wayland socket, forwarded to the host's run directory: a reverse
    // Unix-socket forward, so nothing listens on a TCP port in either machine.
    let mut a = plan::ssh_options(cfg, &known_hosts());
    a.extend([
        "-N".into(),
        "-T".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "StreamLocalBindUnlink=yes".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-R".into(),
        format!("{}:{}", plan::remote_upstream(&s.id), socket_path.to_string_lossy()),
    ]);
    // §86's sound, the other way: a loopback port here that reaches the
    // PulseAudio monitor `wc-session proxy` makes for this session.
    if cfg.audio {
        a.extend(["-L".into(), format!("127.0.0.1:{}:{}/audio.sock", s.audio_port, plan::remote_dir(&s.id))]);
    }
    a.push(cfg.host.clone());
    let child = Command::new(&cfg.ssh)
        .args(&a)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("tunnel: {e}"))?;
    *s.tunnel.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);

    let mut last = String::new();
    let mut proxied = false;
    for _ in 0..60 {
        match ssh_verb(cfg, &["proxy", &s.id]) {
            Ok(o) => {
                log.say(&format!("proxy: {}", o.trim().replace('\n', "; ")));
                proxied = true;
                if cfg.audio && o.contains("audio pulse") {
                    start_audio(s.audio_port, cfg.audio_volume, log);
                }
                break;
            }
            Err(e) => {
                last = e;
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    if !proxied {
        return Err(format!("the proxy did not start: {last}"));
    }
    let verb = plan::launch_verb(&s.id, &s.host_app, crate::Request::Run, 96, None);
    let words: Vec<&str> = verb.iter().map(String::as_str).collect();
    let o = ssh_verb(cfg, &words)?;
    log.say(&format!("run: {} ({:?} after start)", o.trim().replace('\n', "; "), t0.elapsed()));
    Ok(())
}

