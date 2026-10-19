// The launcher is a *product*, so it has no console: a shortcut that flashes a
// black rectangle on every double-click is a shortcut nobody uses, and §16.6
// found that a console window belonging to one of our processes is not merely
// ugly — it takes the taskbar button and the foreground with it. Everything
// this program would have printed goes to its log beside the compositor's.
#![cfg_attr(windows, windows_subsystem = "windows")]
// On Linux this binary is only the tests underneath it: the configuration, the
// command lines and the .ico wrapper are checked on the machine that compiles
// the program, and everything that calls Win32 is compiled out. So the halves
// that reach it are unused here, which is not dead code — it is code for
// another machine, and the Windows gate is where it has to be alive.
#![cfg_attr(not(windows), allow(dead_code))]
//! `wc-launch` — double-click an icon on the Windows desktop, and a Linux
//! application appears in a window, over memory both machines map.
//!
//! Read docs/waycompose-design.md §28 and §83. In one paragraph: the compositor runs here,
//! in the guest, on the interactive desktop, and provides an arena out of BAR2
//! of the VM's ivshmem device; the VM host checks the header the compositor
//! wrote at the front of that region, runs `wc-proxy` on it **as the user**,
//! and runs the application; the Wayland bytes cross on a loopback relay and an
//! ssh remote forward. Until §83 the arena was the guest's pinned RAM and the
//! host had to locate it as root; the shape of this program — **the session is
//! warm, and a click is one ssh exec** — was the answer to that cost and is
//! kept because a cold start is still a compositor and a proxy to start.
//!
//! The first instance to run takes a named mutex and becomes the session's
//! owner; every later one finds the mutex taken, reads the session's state file
//! and launches its application against the session that already exists. Two
//! double-clicks cannot produce two compositors, which is the point of the
//! mutex rather than a happy accident of it.

mod floor;
mod appmeta;
mod audio;
mod config;
mod ico;
// §103.8: the macOS bundle's contents (pure, tested everywhere) and the macOS
// launcher itself.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod macbundle;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
mod audio_mac;
mod plan;
#[cfg(windows)]
mod shell;
/// §92. The compositor's own taskbar-identity module, compiled in here too, so
/// that the identity on a pinned shortcut and the identity on the
/// application's windows come from one function and cannot drift apart.
#[path = "../../wc-compositor/src/taskbar.rs"]
#[allow(dead_code)]
mod taskbar;

/// §92. What a click asks the host for: the application as configured, one of
/// its desktop entry's actions (a jump-list task), or a URL (a frequent page).
#[derive(Debug, Clone, Copy)]
pub enum Request<'a> {
    Run,
    Action(&'a str),
    Open(&'a str),
}
mod token;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use config::Config;

// ---------------------------------------------------------------- logging ---

/// One log per session, appended by every instance, with the pid on each line —
/// because a click that did nothing and a click that did something in another
/// process look identical from the desktop.
struct Log(Option<std::fs::File>, u32);

impl Log {
    fn open(path: &Path) -> Log {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
        Log(f, std::process::id())
    }
    fn say(&mut self, msg: &str) {
        use std::io::Write;
        if let Some(f) = self.0.as_mut() {
            let _ = writeln!(f, "[{}] {msg}", self.1);
            let _ = f.flush();
        }
    }
}

// ------------------------------------------------------------- the session ---

/// What an instance that is *not* the owner needs in order to launch an
/// application into a session it did not start.
#[derive(Default, Debug)]
struct State {
    id: String,
    comp_log: String,
    comp_pid: u32,
    ready: bool,
}

impl State {
    fn parse(text: &str) -> State {
        let mut s = State::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k {
                "id" => s.id = v.into(),
                "comp-log" => s.comp_log = v.into(),
                "comp-pid" => s.comp_pid = v.parse().unwrap_or(0),
                "ready" => s.ready = v == "1",
                _ => {}
            }
        }
        s
    }
    fn render(&self) -> String {
        format!(
            "id={}\ncomp-log={}\ncomp-pid={}\nready={}\n",
            self.id,
            self.comp_log,
            self.comp_pid,
            u8::from(self.ready)
        )
    }
    /// Written to a neighbour and renamed, because a reader is a second click
    /// and half a state file is worse than none.
    fn write(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("state.tmp");
        std::fs::write(&tmp, self.render())?;
        std::fs::rename(&tmp, path)
    }
}

// ------------------------------------------------------------------ paths ---

struct Paths {
    exe_dir: PathBuf,
    ini: PathBuf,
    state_dir: PathBuf,
    log: PathBuf,
    state: PathBuf,
    comp_log: PathBuf,
    known_hosts: PathBuf,
    /// §92. `<exe dir>\apps`: the relaunch files the compositor reads
    /// (`--relaunch-dir`) and the host's last answer about each application.
    /// Beside the executable, because a relaunch command names this
    /// installation's `wc-launch.exe`.
    apps: PathBuf,
    /// §92. `%LOCALAPPDATA%\waycompose\jumplist`: favicons, task icons and the
    /// list of pages the user removed. History-derived, so in the user's own
    /// app data and never beside the program.
    jump: PathBuf,
}

fn paths(session: &str) -> Result<Paths, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let exe_dir = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
    // The same directory the compositor's AF_UNIX sockets live in, which is the
    // one place on this guest that is already this project's.
    let state_dir = match std::env::var_os("LOCALAPPDATA") {
        Some(v) => PathBuf::from(v).join("waycompose"),
        None => exe_dir.join("state"),
    };
    let _ = std::fs::create_dir_all(&state_dir);
    Ok(Paths {
        ini: exe_dir.join("wc-launch.ini"),
        log: state_dir.join(format!("wc-launch-{session}.log")),
        state: state_dir.join(format!("wc-launch-{session}.state")),
        comp_log: state_dir.join(format!("wc-launch-{session}-comp.log")),
        known_hosts: exe_dir.join("known_hosts"),
        apps: exe_dir.join("apps"),
        jump: state_dir.join("jumplist"),
        state_dir,
        exe_dir,
    })
}

// ------------------------------------------------------------------- main ---

// §103.8: on macOS `main` hands over to `mac::main` before the Windows path.
#[cfg_attr(target_os = "macos", allow(unreachable_code, unused_variables))]
fn main() -> ExitCode {
    let t0 = Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();

    // §82. `--write-firefox-profile TITLEBAR DPI DIR [PREFS]`: write the
    // `user.js` and `chrome/userChrome.css` a launch would send, into DIR, and
    // nothing else. On every platform and before any configuration is read,
    // because its callers are the harnesses that run Firefox against this
    // compositor on Linux and under Wine — and a harness that wrote its own copy
    // of the sheet would be testing the copy.
    if let Some(i) = args.iter().position(|a| a == "--write-firefox-profile") {
        return write_firefox_profile(&args[i + 1..]);
    }
    // §94. `--render-app-files TITLEBAR DPI CAPTION OUTDIR [PREFS]`: what
    // `wc-session` runs **on the VM host** for every launch, from the host's
    // own definition of the application and the two numbers the guest sent.
    // It writes OUTDIR/user.js, and for `tabs` OUTDIR/userChrome.css and
    // OUTDIR/gtk.css, and prints `moz-env VALUE`. Until §94 these were
    // rendered here on the guest and sent as text; a key that sends file
    // contents is a key that writes a profile's preferences.
    if let Some(i) = args.iter().position(|a| a == "--render-app-files") {
        return render_app_files(&args[i + 1..]);
    }
    // §103.8: on a Mac this process is the Linux application's own (its
    // bundle's executable), and everything below is the Windows launcher.
    #[cfg(target_os = "macos")]
    return mac::main(args);

    // The session name is needed before the configuration can be read, because
    // the log the configuration's errors go into is named after it. It is the
    // default unless `--session-name` says otherwise.
    let session = args
        .iter()
        .position(|a| a == "--session-name")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "wcl".into());

    let p = match paths(&session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("wc-launch: {e}");
            return ExitCode::from(2);
        }
    };
    let mut log = Log::open(&p.log);

    if args.iter().any(|a| a == "--write-example-config") {
        return match std::fs::write(&p.ini, config::EXAMPLE) {
            Ok(()) => {
                log.say(&format!("wrote an example configuration to {}", p.ini.display()));
                ExitCode::SUCCESS
            }
            Err(e) => {
                log.say(&format!("could not write {}: {e}", p.ini.display()));
                ExitCode::from(2)
            }
        };
    }

    let cfg = match Config::load(&p.ini) {
        Ok(c) => c,
        Err(e) => {
            log.say(&format!("configuration: {e}"));
            return ExitCode::from(2);
        }
    };

    let rc = run(&args, &cfg, &p, &mut log, t0);
    match rc {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log.say(&format!("failed: {e}"));
            ExitCode::from(1)
        }
    }
}

fn write_firefox_profile(a: &[String]) -> ExitCode {
    let (Some(t), Some(dpi), Some(dir)) = (a.first(), a.get(1), a.get(2)) else {
        eprintln!("wc-launch: --write-firefox-profile TITLEBAR DPI DIR [PREFS]");
        return ExitCode::from(2);
    };
    let Some(t) = plan::Titlebar::parse(t) else {
        eprintln!("wc-launch: titlebar {t:?} is not one of tabs, none, client");
        return ExitCode::from(2);
    };
    let Ok(dpi) = dpi.parse::<u32>() else {
        eprintln!("wc-launch: dpi {dpi:?} is not a number");
        return ExitCode::from(2);
    };
    let dir = std::path::Path::new(dir);
    let fit = plan::TitlebarFit::at(dpi, None);
    let prefs = a.get(3).map(String::as_str).unwrap_or("");
    let wrote = std::fs::create_dir_all(dir.join("chrome"))
        .and_then(|()| std::fs::write(dir.join("user.js"), plan::app_user_js(t, prefs)))
        .and_then(|()| match plan::user_chrome_css(t, fit) {
            Some(css) => std::fs::write(dir.join("chrome/userChrome.css"), css),
            None => Ok(()),
        });
    match wrote {
        Ok(()) => {
            println!(
                "wc-launch: wrote {:?}'s profile for {} at {dpi} dpi: caption {} px, buttons {} px, env MOZ_GTK_TITLEBAR_DECORATION={}",
                t,
                dir.display(),
                fit.height,
                fit.reserve,
                t.moz_env()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wc-launch: {}: {e}", dir.display());
            ExitCode::from(2)
        }
    }
}

fn render_app_files(a: &[String]) -> ExitCode {
    let usage = || {
        eprintln!("wc-launch: --render-app-files TITLEBAR DPI CAPTION OUTDIR [PREFS]");
        ExitCode::from(2)
    };
    let (Some(t), Some(dpi), Some(cap), Some(dir)) = (a.first(), a.get(1), a.get(2), a.get(3)) else {
        return usage();
    };
    let (Ok(dpi), Ok(cap)) = (dpi.parse::<u32>(), cap.parse::<u32>()) else {
        return usage();
    };
    let f = match plan::host_files(t, dpi, cap, a.get(4).map(String::as_str).unwrap_or("")) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("wc-launch: {e}");
            return ExitCode::from(2);
        }
    };
    let dir = Path::new(dir);
    let mut wrote = std::fs::write(dir.join("user.js"), &f.user_js);
    for (name, body) in [("userChrome.css", &f.user_chrome), ("gtk.css", &f.gtk_css)] {
        if let (Ok(()), Some(b)) = (&wrote, body) {
            wrote = std::fs::write(dir.join(name), b);
        }
    }
    match wrote {
        Ok(()) => {
            println!("moz-env {}", f.moz_env);
            println!("fit {} {}", f.fit.height, f.fit.reserve);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wc-launch: {}: {e}", dir.display());
            ExitCode::from(2)
        }
    }
}

#[cfg(not(windows))]
fn run(_a: &[String], _c: &Config, _p: &Paths, log: &mut Log, _t0: Instant) -> Result<(), String> {
    // The Linux build exists so that the configuration, the command lines and
    // the .ico wrapper are tested by `cargo test` on the machine that compiles
    // this. Everything below that line is Win32 and a guest.
    log.say("wc-launch is a Windows program; this build only carries its tests");
    Err("not Windows".into())
}

#[cfg(windows)]
fn run(
    args: &[String],
    cfg: &Config,
    p: &Paths,
    log: &mut Log,
    t0: Instant,
) -> Result<(), String> {
    let mut app: Option<&str> = None;
    let mut install: Option<&str> = None;
    let mut session_only = false;
    let mut stop = false;
    let mut status = false;
    let mut ping = false;
    let mut request = Request::Run;
    let mut refresh_only: Option<&str> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => session_only = true,
            "--stop" => stop = true,
            "--status" => status = true,
            "--ping" => ping = true,
            "--install" => install = Some(it.next().map(String::as_str).unwrap_or("")),
            // §92. Rebuild an application's Start menu entry, relaunch file and
            // jump list from the host's answer, without launching anything.
            "--refresh" => refresh_only = Some(it.next().map(String::as_str).unwrap_or("")),
            // §92. A jump-list task names an action of the desktop entry, and a
            // frequent page a URL. Both are checked again on the VM host, where
            // the check is the one that counts (security-model.md R33).
            "--action" => {
                let id = it.next().map(String::as_str).unwrap_or("");
                if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    return Err(format!("--action {id:?} is not an action id"));
                }
                request = Request::Action(id);
            }
            "--url" => {
                let url = it.next().map(String::as_str).unwrap_or("");
                if url.is_empty() || url.len() > 4096 || url.contains(char::is_whitespace) {
                    return Err("--url wants one URL".into());
                }
                request = Request::Open(url);
            }
            "--session-name" => {
                it.next();
            }
            other if other.starts_with("--") => return Err(format!("unknown option {other}")),
            other => app = Some(other),
        }
    }

    if ping {
        return win::ping(cfg, p, log);
    }
    if status {
        return win::status(cfg, p, log);
    }
    if stop {
        return win::stop(cfg, p, log);
    }
    if let Some(id) = install {
        return win::install(cfg, p, log, id);
    }
    if let Some(id) = refresh_only {
        cfg.app(id)?;
        win::refresh(cfg, p, log, id, win::Refresh::Full);
        return Ok(());
    }

    let (app, section) = match (app, session_only) {
        (Some(a), _) => (Some(cfg.app(a)?), a),
        (None, true) => (None, ""),
        (None, false) => return Err("no application named, and no --session".into()),
    };
    // §91: which route this launch came by. A double-click from Explorer is
    // medium and not elevated; a harness task at `-RunLevel Highest` is high.
    log.say(&format!("token: {}", token::describe()));
    win::launch(cfg, p, log, app.map(|a| (section, a)), request, t0)
}

// ------------------------------------------------------------------ Win32 ---

#[cfg(windows)]
mod win {
    use super::{Config, Log, Paths, State, appmeta, ico, plan};
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// A child of a launcher has no console of its own. Without this the
    /// compositor — a console program — puts a window on the desktop the moment
    /// it starts, and §16.6 is what that costs.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn quiet(cmd: &mut Command) -> &mut Command {
        cmd.creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
    }

    // ------------------------------------------------------------- mutex ---

    /// The session's liveness, as a kernel object rather than as a guess.
    ///
    /// §16.8's rule is that an identity is a pid or a full path, never an image
    /// name — and this is the one identity that is neither and is still sound:
    /// a named mutex exists exactly as long as some process holds a handle to
    /// it, so "the mutex is there" is "the owner is running", enforced by the
    /// kernel and released by a crash.
    ///
    /// It is also the only liveness test available here that does not touch the
    /// compositor. The obvious one — connect to its socket and see if somebody
    /// answers — would be a *client*, and §17.4 made the first client the thing
    /// that releases the compositor's held objects into the arena. A probe that
    /// connected before the host's sweep would write stamps' worth of keymap
    /// over pages the sweep had not reached yet, and the failure would surface
    /// as `located 16374 of 16384` on the other machine.
    pub struct Owner {
        handle: isize,
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            // SAFETY: a handle this process created and has not closed.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle as _) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `Ok(Some)` — we are the owner. `Ok(None)` — somebody else is.
    fn claim(session: &str) -> Result<Option<Owner>, String> {
        use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        // `Local\` — per logon session, which is the right scope: the desktop
        // this compositor draws on belongs to one.
        let name = wide(&format!("Local\\waycompose-launch-{session}"));
        // SAFETY: a null security descriptor and a NUL-terminated name.
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if h.is_null() {
            return Err(format!("CreateMutexW: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: called immediately after the failing/succeeding call above.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: our own handle to an object somebody else owns.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
            return Ok(None);
        }
        Ok(Some(Owner { handle: h as isize }))
    }

    /// §89. A reply from the host that says the Wayland connections went back
    /// to ssh gets a line of its own, in capitals, so that a log read later
    /// cannot mistake a session on the tunnel for one on the rings.
    fn loud_fallback(log: &mut Log, who: &str, said: &str) {
        for l in said.lines().filter(|l| l.contains("SSH FALLBACK")) {
            let why = l.split_once("SSH FALLBACK:").map_or(l, |(_, w)| w).trim();
            log.say(&format!(
                "SSH FALLBACK: the Wayland connections cross the ssh tunnel this session, as {who} says: {why}"
            ));
        }
    }

    fn kill(pid: u32) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };
        if pid == 0 {
            return;
        }
        // SAFETY: a pid, which is what §16.8 says an identity is. A pid that
        // has been reused belongs to somebody else, which is why this is only
        // ever called with one this process wrote down itself.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }

    // ---------------------------------------------------------- the parts ---

    /// One `wc-session` verb on the VM host, with its output collected **in
    /// files**.
    ///
    /// > **`ssh.exe` does not exit when its standard output is a pipe, and this
    /// > cost an hour.** Windows' own OpenSSH client (`C:\Windows\System32\
    /// > OpenSSH\ssh.exe`) runs the remote command, returns its bytes and
    /// > closes its end — and then does not terminate. Redirected to a file it
    /// > is 0.22 s; through an anonymous pipe it was still running after two
    /// > minutes with the answer already delivered. `std::process::Command::
    /// > output()` is exactly that pipe, so the first version of this launcher
    /// > hung on its own liveness check with the reply sitting unread in a
    /// > buffer, which reads as a network problem and is not one.
    ///
    /// So every short-lived ssh writes to a file in the session's own state
    /// directory and is read back after it exits. The long-lived ones — the
    /// tunnel, the relay, the compositor — never had the problem, because their
    /// output goes to `NUL`, which is a device rather than a pipe.
    fn ssh(cfg: &Config, p: &Paths, verb: &[&str]) -> Result<String, String> {
        ssh_in(cfg, p, verb, None)
    }

    /// `ssh`, with a file on the far end's standard input.
    ///
    /// The one caller is the locate, and what it sends is the arena's page
    /// frame list. It goes on stdin rather than in the verb because it is
    /// 16 bytes a page — 256 KiB for a 64 MiB arena — and an argument that size
    /// does not survive a command line, let alone `$SSH_ORIGINAL_COMMAND`.
    ///
    /// It is also the one thing this launcher sends the host that is *bulk*,
    /// and it is worth noting what it is not: it is a list of **page frame
    /// numbers**, which is where the guest's pages are, not what is in them.
    /// The arena is still guest RAM located in QEMU's backing object and this
    /// file changes only how it is found.
    fn ssh_in(
        cfg: &Config,
        p: &Paths,
        verb: &[&str],
        stdin: Option<&std::path::Path>,
    ) -> Result<String, String> {
        let argv = plan::ssh_exec(cfg, &p.known_hosts.to_string_lossy(), verb);
        let stem = p.state_dir.join(format!(
            "wc-launch-{}-{}.ssh",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let (op, ep) = (stem.with_extension("out"), stem.with_extension("err"));
        let make = |path: &std::path::Path| {
            std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))
        };
        let input = match stdin {
            None => Stdio::null(),
            Some(path) => Stdio::from(
                std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?,
            ),
        };
        let status = Command::new(&cfg.ssh)
            .args(&argv)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(input)
            .stdout(Stdio::from(make(&op)?))
            .stderr(Stdio::from(make(&ep)?))
            .status()
            .map_err(|e| format!("{}: {e}", cfg.ssh))?;
        let so = std::fs::read_to_string(&op).unwrap_or_default().trim().to_string();
        let se = std::fs::read_to_string(&ep).unwrap_or_default().trim().to_string();
        let _ = std::fs::remove_file(&op);
        let _ = std::fs::remove_file(&ep);
        if !status.success() {
            return Err(format!(
                "{} {status}: {se}{so}",
                verb.first().copied().unwrap_or("")
            ));
        }
        Ok(so)
    }

    /// Poll a growing log for a line. Everything this launcher waits for is a
    /// line in somebody's log, because the alternative — a second channel back
    /// from a process that already has one — is a second thing to get wrong.
    fn wait_for<T>(
        path: &Path,
        secs: u64,
        mut f: impl FnMut(&str) -> Option<T>,
    ) -> Result<T, String> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                for line in text.lines() {
                    if let Some(v) = f(line) {
                        return Ok(v);
                    }
                }
            }
            if Instant::now() > deadline {
                return Err(format!("{} never said what was expected", path.display()));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn count_mapped(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .map(|t| t.lines().filter(|l| l.contains(" mapped ")).count())
            .unwrap_or(0)
    }

    // -------------------------------------------------------------- verbs ---

    /// Does the key, the link and the host's half work at all — with no
    /// compositor, no arena and no application in the way.
    ///
    /// It exists because every other way of asking that question from a
    /// workstation goes through two shells and an MSYS path translation, and
    /// the first attempt at one turned `C:\Users\<guest-user>\wclaunch\id_host` into
    /// `C:Users<guest-user>wclaunchid_host` and reported a permission problem. This
    /// runs the same `Command` with the same argv the real path uses.
    pub fn ping(cfg: &Config, p: &Paths, log: &mut Log) -> Result<(), String> {
        let t = Instant::now();
        let said = ssh(cfg, p, &["ping"])?;
        log.say(&format!("PING {said} ({} ms)", t.elapsed().as_millis()));
        Ok(())
    }

    pub fn status(cfg: &Config, p: &Paths, log: &mut Log) -> Result<(), String> {
        let s = std::fs::read_to_string(&p.state).map(|t| State::parse(&t)).unwrap_or_default();
        // The mutex, not the state file, is the truth: a state file outlives an
        // owner that was killed, and the mutex does not.
        let live = matches!(claim(&cfg.session), Ok(None));
        log.say(&format!("status: owner-alive={live} state={s:?}"));
        Ok(())
    }

    pub fn stop(cfg: &Config, p: &Paths, log: &mut Log) -> Result<(), String> {
        let Ok(text) = std::fs::read_to_string(&p.state) else {
            log.say("stop: no session");
            return Ok(());
        };
        let s = State::parse(&text);
        log.say(&format!("stop: ending session {} (compositor pid {})", s.id, s.comp_pid));
        // The compositor is the session's life. Ending it makes the owner run
        // its own teardown, which is the only code that knows what to unwind.
        kill(s.comp_pid);
        // And say so to the host as well, in case the owner is already gone.
        let _ = ssh(cfg, p, &["down", &s.id]);
        Ok(())
    }

    /// The whole of it: make sure a session exists, then run the application.
    pub fn launch(
        cfg: &Config,
        p: &Paths,
        log: &mut Log,
        app: Option<(&str, &crate::config::App)>,
        req: crate::Request,
        t0: Instant,
    ) -> Result<(), String> {
        // §92. The first launch of an application on this installation, before
        // anything else: its relaunch file must exist before its first window
        // does, because the compositor stamps a window's relaunch command when
        // it stamps its identity. Every later launch refreshes after its window
        // is up, where the user does not wait for it.
        if let Some((section, a)) = app
            && !relaunch_file(p, section, a).exists()
        {
            refresh(cfg, p, log, section, Refresh::Full);
        }
        // §85. A session now ends `linger-secs` after its last window, so a
        // click can land in the seconds while one is winding down: it joins,
        // and the session goes from under it. That click starts a new session
        // instead of failing, once, after the old owner has let go.
        let mut owned = claim(&cfg.session)?;
        if owned.is_none() {
            log.say("a session is already up; joining it");
            match join_and_start(cfg, p, log, app, req, t0) {
                Ok(()) => return Ok(()),
                Err(e) if !p.state.exists() => {
                    log.say(&format!("the session ended under this click ({e}); starting a new one"));
                    let deadline = Instant::now() + Duration::from_secs(30);
                    loop {
                        owned = claim(&cfg.session)?;
                        if owned.is_some() {
                            break;
                        }
                        if Instant::now() > deadline {
                            return Err(format!("{e}, and the old session never let go"));
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        match owned {
            None => unreachable!("joined above, or claimed"),
            // Nobody does. Become it.
            Some(owner) => {
                let s = match cold(cfg, p, log, &owner) {
                    Ok(s) => s,
                    Err(e) => {
                        // A half-built session must not be left looking like a
                        // whole one: the state file is what the next click
                        // reads, and `ready=0` there is a click that waits and
                        // then says so rather than one that opens nothing.
                        let _ = std::fs::remove_file(&p.state);
                        return Err(e);
                    }
                };
                log.say(&format!("session up in {} ms", t0.elapsed().as_millis()));
                if let Some((section, a)) = app {
                    let before = count_mapped(&p.comp_log);
                    start_app(cfg, p, log, &s.id, a, req)?;
                    let _ = report(log, &p.comp_log, before, t0, "cold", req);
                    refresh(cfg, p, log, section, Refresh::Full);
                }
                // §92. While the session runs, its jump lists follow the
                // history the way Windows' Firefox's do: every
                // `browser.taskbar.lists.refreshInSeconds`.
                start_list_refresher(cfg, &cfg.session);
                supervise(cfg, p, log, s)
            }
        }
    }

    /// Somebody else owns the session. This is the warm path and the one the
    /// user pays for on every click after the first.
    fn join_and_start(
        cfg: &Config,
        p: &Paths,
        log: &mut Log,
        app: Option<(&str, &crate::config::App)>,
        req: crate::Request,
        t0: Instant,
    ) -> Result<(), String> {
        let s = join(p, 60)?;
        let Some((section, a)) = app else {
            log.say("--session with a session already up: nothing to do");
            return Ok(());
        };
        let before = count_mapped(Path::new(&s.comp_log));
        start_app(cfg, p, log, &s.id, a, req)?;
        let shown = report(log, Path::new(&s.comp_log), before, t0, "warm", req);
        refresh(cfg, p, log, section, Refresh::Full);
        shown
    }

    /// Everything that has to happen once. Timed line by line in the log,
    /// because "is it fast" is a question about which of these is expensive and
    /// the answer moved twice while this was being written.
    fn cold(cfg: &Config, p: &Paths, log: &mut Log, _owner: &Owner) -> Result<Session, String> {
        let t = Instant::now();
        let pid = std::process::id();
        let id = plan::run_id(&cfg.session, pid);
        let socket = format!("wl-{id}");
        let socket_path = p.state_dir.join(&socket);
        // A leftover node from a session whose owner was killed: it is ours by
        // name — `wl-<session>-<our own pid>` — and it is removed by full path,
        // never by glob (§17.7's fourth lesson).
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&p.comp_log);

        State { id: id.clone(), comp_log: p.comp_log.display().to_string(), ..State::default() }
            .write(&p.state)
            .map_err(|e| format!("state file: {e}"))?;

        // 1. The compositor, here, on the desktop the user is looking at.
        let mut argv = plan::compositor_argv(cfg, &socket, &p.comp_log.to_string_lossy());
        // §92. Where this installation keeps each application's relaunch
        // file, so its windows can be pinned as the application.
        argv.extend(plan::relaunch_argv(&p.apps.to_string_lossy()));
        // IVSHMEM ARENA (§83). The arena is BAR2 of the VM's ivshmem device, so
        // its size is the device's, set on the VM host, and `arena-mib` decides
        // nothing. Said once per session so that a log read later cannot
        // mistake which substrate ran.
        log.say("arena: ivshmem BAR2, sized by the VM host's device (arena-mib is unused)");
        log.say(&format!("compositor: {} {}", cfg.compositor, argv.join(" ")));
        let comp = quiet(Command::new(&cfg.compositor).args(&argv))
            .spawn()
            .map_err(|e| format!("{}: {e}", cfg.compositor))?;
        let comp_pid = comp.id();

        // 2. Its arena's run id, from the line printed **after** the header is
        //    published. Waiting for the earlier "provided" line would race the
        //    header: the host would read a cleared magic and refuse.
        //
        // **Every failure from here on kills the compositor**, and that is a
        // fix rather than tidiness: a compositor left running holds the
        // device's one mapping — the driver allows one per device, so the next
        // click's compositor could not map it — holds its own executable open,
        // so the next deploy fails with `scp: dest open "sv-guest.exe":
        // Failure`, and holds the mutex's session half-built.
        let run_id = wait_for(&p.comp_log, 60, |l| {
            let rest = l.split_once("header published: run ")?.1;
            Some(rest.split(',').next()?.trim().to_string())
        })
        .map_err(|_| {
            kill(comp_pid);
            "the compositor never published an ivshmem arena".to_string()
        })?;
        log.say(&format!("arena: run {run_id}, {} ms", t.elapsed().as_millis()));
        // §89. Whether the compositor kept the region's tail for the Wayland
        // rings, in its own words: both lines come before the header line this
        // waited for, so they are in the log now.
        if !cfg.wayland_bell.is_empty() {
            let text = std::fs::read_to_string(&p.comp_log).unwrap_or_default();
            match text.lines().find(|l| l.contains("wayland rings:") || l.contains("WAYLAND RINGS UNAVAILABLE")) {
                Some(l) if l.contains("UNAVAILABLE") => log.say(&format!(
                    "SSH FALLBACK: the Wayland connections will cross the ssh tunnel this session -- {}",
                    l.trim()
                )),
                Some(l) => log.say(&format!("rings: {}", l.trim())),
                None => log.say("SSH FALLBACK: the compositor said nothing about Wayland rings (an older sv-guest.exe?)"),
            }
        } else {
            log.say(
                "SSH FALLBACK: wayland-bell is empty in wc-launch.ini, so the compositor was not asked \
                 for Wayland rings; the Wayland connections cross the ssh tunnel this session",
            );
        }

        // 3. The host checks the header against the run id. No locate, no root:
        //    it opens a file its group may open. What used to be §12.5's 2.4 s
        //    sweep and then §34.8's frame-list translation is one read.
        let t_locate = Instant::now();
        let said = match ssh(cfg, p, &["up", &id, &run_id, "ivshmem"]) {
            Ok(s) => s,
            Err(e) => {
                kill(comp_pid);
                return Err(e);
            }
        };
        log.say(&format!("up: {} ({} ms)", said.replace('\n', " | "), t_locate.elapsed().as_millis()));
        loud_fallback(log, "the host", &said);

        // 4. The relay, and the tunnel that gives the host an end of it. The
        //    tunnel is opened whatever the Wayland connections ride (§89): the
        //    icon and clipboard feeds, a connection that falls back, the GPU's
        //    cold path and the sound all use it.
        let mut relay = match quiet(
            Command::new(&cfg.relay).args(plan::relay_argv(cfg, &socket_path.to_string_lossy())),
        )
        .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                kill(comp_pid);
                return Err(format!("{}: {e}", cfg.relay));
            }
        };
        let mut tunnel = match quiet(
            Command::new(&cfg.ssh)
                .args(plan::tunnel_argv(cfg, &p.known_hosts.to_string_lossy(), &id)),
        )
        .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = relay.kill();
                kill(comp_pid);
                return Err(format!("{}: {e}", cfg.ssh));
            }
        };

        // 5. The proxy, on the plan. Retried, because the tunnel's socket
        //    appears on the *other* machine and there is nothing here to watch
        //    for it; the host says so by refusing.
        let t_proxy = Instant::now();
        let mut last = String::new();
        let mut ok = false;
        for _ in 0..60 {
            match ssh(cfg, p, &["proxy", &id]) {
                Ok(s) => {
                    log.say(&format!("proxy: {} ({} ms)", s.replace('\n', " | "), t_proxy.elapsed().as_millis()));
                    loud_fallback(log, "the proxy", &s);
                    ok = true;
                    break;
                }
                Err(e) => {
                    last = e;
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
        }
        if !ok {
            // The relay and the tunnel too: before §89 a failure here left both
            // running, and a tunnel left holding its loopback forwards is one
            // the next session's tunnel binds beside (Windows' ssh.exe allows
            // it) and loses connections to.
            let _ = relay.kill();
            let _ = tunnel.kill();
            kill(comp_pid);
            return Err(format!("the host would not start its proxy: {last}"));
        }

        // 6. The sound (§86). Started after the proxy, which is what started
        //    the session's audio server; never fatal, because a session without
        //    sound is still a session and the host has already given Firefox a
        //    null audio context if there is no server.
        // 7. The forwarded GPU (§86), only when the ini asks for sessions.
        //    Never fatal either: without it the host renders in software.
        let gpus = if cfg.gpu_sessions > 0 { start_gpu(cfg, p, log, &id) } else { Vec::new() };

        if cfg.audio {
            start_audio(cfg, p);
        } else {
            log.say("audio: off in wc-launch.ini; the host gives Firefox a null audio context");
        }

        let state = State {
            id: id.clone(),
            comp_log: p.comp_log.display().to_string(),
            comp_pid,
            ready: true,
        };
        state.write(&p.state).map_err(|e| format!("state file: {e}"))?;
        log.say(&format!("cold start: {} ms", t.elapsed().as_millis()));
        Ok(Session { id, comp, relay, tunnel, gpus, comp_log: p.comp_log.clone() })
    }

    pub struct Session {
        id: String,
        comp: Child,
        relay: Child,
        tunnel: Child,
        /// The GPU sides this session started (§86); empty without `gpu-sessions`.
        gpus: Vec<Child>,
        comp_log: std::path::PathBuf,
    }

    /// The forwarded GPU (§86, §87): one `sv-guest.exe gpu guest` on the GPU
    /// arena's ivshmem device, serving up to `gpu-sessions` sessions in slots
    /// of its BAR, then `wc-session gpu ID N`, which starts this session's
    /// `sv-hostd ivshmem-serve` on the VM host **as the user**. No root on
    /// either machine: the guest maps a device its driver hands to any user,
    /// and the host opens the device's backing file with a group bit.
    ///
    /// Started after the proxy; every failure from here kills what it started
    /// and returns none, and the host then gives Firefox no GPU environment, so
    /// it renders in software rather than failing to start its renderer.
    fn start_gpu(cfg: &Config, p: &Paths, log: &mut Log, id: &str) -> Vec<Child> {
        let t = Instant::now();
        let path = p.state_dir.join(format!("wc-launch-{}-gpu.log", cfg.session));
        let _ = std::fs::remove_file(&path);
        let file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                log.say(&format!("gpu: {}: {e}; no forwarded GPU", path.display()));
                return Vec::new();
            }
        };
        let mut kid = match Command::new(&cfg.compositor)
            .args(plan::gpu_argv(cfg))
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(file))
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                log.say(&format!("gpu: {}: {e}; no forwarded GPU", cfg.compositor));
                return Vec::new();
            }
        };
        let give_up = |kid: &mut Child, log: &mut Log, why: String| {
            let _ = kid.kill();
            log.say(&format!("gpu: {why}; no forwarded GPU this session, the host renders in software"));
            Vec::new()
        };
        if wait_for(&path, 30, |l| l.contains("publishing the extent").then_some(())).is_err() {
            let tail = std::fs::read_to_string(&path).unwrap_or_default();
            let last = tail.lines().last().unwrap_or("").to_string();
            return give_up(&mut kid, log, format!("{} never published ({last})", path.display()));
        }
        // The guest's own account of the device and the doorbell, for the log:
        // the two lines that say whether this session has a slot plan and a bell.
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for l in text.lines().filter(|l| l.contains("ivshmem slot") || l.contains("doorbell")) {
            log.say(&format!("gpu: {}", l.trim()));
        }
        let n = cfg.gpu_sessions.to_string();
        match ssh(cfg, p, &["gpu", id, &n]) {
            Ok(s) => {
                log.say(&format!("gpu: {} ({} ms)", s.replace('\n', " | "), t.elapsed().as_millis()));
                vec![kid]
            }
            Err(e) => give_up(&mut kid, log, format!("the host said: {e}")),
        }
    }

    /// The session's sound (§86): connect to the loopback port the tunnel
    /// forwards to the host's per-session monitor socket, and play what
    /// arrives on a thread of its own, which logs through its own handle.
    ///
    /// It is a socket and not an ssh exec's standard output because that was
    /// tried first and ssh.exe delivered nothing into the pipe this program
    /// gave it, while the same command through an MSYS pipe streamed. The
    /// tunnel is already up and already this session's, so the stream ends when
    /// the session does: `supervise` kills the tunnel and the read sees EOF.
    ///
    /// The forward connects on demand, so a host with no server for the session
    /// shows up here as a connection that closes at once; that is logged, and
    /// the host has then given Firefox a null audio context.
    fn start_audio(cfg: &Config, p: &Paths) {
        let port = cfg.audio_port();
        let log_path = p.log.clone();
        let volume = cfg.audio_volume;
        std::thread::spawn(move || {
            let mut log = Log::open(&log_path);
            let deadline = Instant::now() + Duration::from_secs(15);
            let stream = loop {
                match std::net::TcpStream::connect(("127.0.0.1", port)) {
                    Ok(s) => break s,
                    Err(e) if Instant::now() > deadline => {
                        log.say(&format!("audio: 127.0.0.1:{port} never answered ({e}); no sound this session"));
                        return;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(200)),
                }
            };
            log.say(&format!("audio: connected to the session's monitor through the tunnel, 127.0.0.1:{port}"));
            if let Err(e) = crate::audio::play(stream, volume, |l| log.say(l)) {
                log.say(&format!("audio: {e}; no sound this session"));
            }
        });
    }

    /// A click that is not the first one. It reads the owner's state file and
    /// waits for `ready`, which is the whole synchronisation between two
    /// double-clicks a second apart.
    fn join(p: &Paths, secs: u64) -> Result<State, String> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Ok(t) = std::fs::read_to_string(&p.state) {
                let s = State::parse(&t);
                if s.ready {
                    return Ok(s);
                }
            }
            if Instant::now() > deadline {
                return Err("a session is starting and did not become ready".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn start_app(
        cfg: &Config,
        p: &Paths,
        log: &mut Log,
        id: &str,
        app: &crate::config::App,
        req: crate::Request,
    ) -> Result<(), String> {
        let t = Instant::now();
        // §94. What crosses is the host's id for the application, the action
        // or URL a jump-list entry names, and two numbers only this desktop
        // can measure: its DPI and Windows' caption height at that DPI, from
        // which the host sizes Firefox's tab strip and the GTK dialogs'
        // header bars (§82, §91.8). The command line, the profile, the
        // preferences and the title-bar arrangement are the host's, in
        // `apps.ini` beside `wc-session`; until §94 they crossed from here,
        // and a key that can send a command line can run anything
        // (security-model.md R33).
        let (dpi, caption) = desktop_metrics();
        log.say(&format!("desktop: {dpi} dpi, caption {} px", caption.unwrap_or(0)));
        let verb = plan::launch_verb(id, &app.host_app, req, dpi, caption);
        let words: Vec<&str> = verb.iter().map(String::as_str).collect();
        let said = ssh(cfg, p, &words)?;
        // A page's URL is the user's history and is not written to this log.
        let what = match req {
            crate::Request::Run => "run".to_string(),
            crate::Request::Action(a) => format!("action {a}"),
            crate::Request::Open(_) => "open (a page from the jump list)".to_string(),
        };
        log.say(&format!("{what} {}: {said} ({} ms)", app.title, t.elapsed().as_millis()));
        Ok(())
    }

    /// This desktop's DPI and Windows' caption height at it, which the host
    /// turns into a [`plan::TitlebarFit`] (§94).
    ///
    /// §82. The height is `AdjustWindowRectExForDpi`'s top inset for the
    /// compositor's own window style, which is the number the compositor folds
    /// into the client area; the width is scaled from §31's measurement inside
    /// [`plan::TitlebarFit::at`], because no API answers it without a window on
    /// screen.
    ///
    /// **The thread is made per-monitor aware first**, and that is not
    /// optional: `GetDpiForSystem` answers 96 to a DPI-unaware caller whatever
    /// the desktop is set to, and this program declares no awareness in a
    /// manifest. Every entry point is resolved by name, so a Windows without
    /// them gets the 100% numbers rather than a launcher that will not start.
    fn desktop_metrics() -> (u32, Option<i32>) {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        // `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`, an opaque pseudo-handle.
        const PER_MONITOR_AWARE_V2: isize = -4;
        // The compositor's `WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_CLIPSIBLINGS`.
        const STYLE: u32 = 0x00CF_0000 | 0x0200_0000 | 0x0400_0000;
        let name: Vec<u16> = "user32.dll".encode_utf16().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated name of a module every Win32 process has.
        let user32 = unsafe { LoadLibraryW(name.as_ptr()) };
        if user32.is_null() {
            return (96, None);
        }
        // SAFETY: ASCII NUL-terminated export names; each transmute is to the
        // documented signature of that export.
        unsafe {
            if let Some(f) = GetProcAddress(user32, c"SetThreadDpiAwarenessContext".as_ptr().cast()) {
                let f: unsafe extern "system" fn(isize) -> isize = std::mem::transmute(f);
                f(PER_MONITOR_AWARE_V2);
            }
            let dpi = match GetProcAddress(user32, c"GetDpiForSystem".as_ptr().cast()) {
                Some(f) => {
                    let f: unsafe extern "system" fn() -> u32 = std::mem::transmute(f);
                    f()
                }
                None => 96,
            };
            let caption = GetProcAddress(user32, c"AdjustWindowRectExForDpi".as_ptr().cast())
                .map(|f| {
                    let f: unsafe extern "system" fn(*mut RECT, u32, i32, u32, u32) -> i32 =
                        std::mem::transmute(f);
                    let mut rc = RECT { left: 0, top: 0, right: 100, bottom: 100 };
                    f(&mut rc, STYLE, 0, 0, dpi);
                    -rc.top
                });
            (dpi, caption)
        }
    }

    /// The number the user asked for: from this process starting — which is
    /// what a double-click costs — to a window existing on the desktop.
    ///
    /// The compositor is what creates that window and what says so, so the
    /// measurement is its own log line rather than anything this program can
    /// see. `before` is the count taken before the application was asked for,
    /// so a session with a window already open measures the *new* one.
    fn report(
        log: &mut Log,
        comp_log: &Path,
        before: usize,
        t0: Instant,
        kind: &str,
        req: crate::Request,
    ) -> Result<(), String> {
        let path = comp_log.to_path_buf();
        // §92. A task or a page may open a tab in a window that is already
        // there, and then no window is mapped at all. That is not a failure,
        // so it waits briefly and says what it saw.
        let optional = !matches!(req, crate::Request::Run);
        let deadline = Instant::now() + Duration::from_secs(if optional { 15 } else { 120 });
        loop {
            if count_mapped(&path) > before {
                log.say(&format!(
                    "CLICK-TO-WINDOW {} ms ({kind}, {} window(s) before)",
                    t0.elapsed().as_millis(),
                    before
                ));
                return Ok(());
            }
            if optional && Instant::now() > deadline {
                log.say(&format!("no new window in 15 s ({kind}); the application may have opened a tab"));
                return Ok(());
            }
            if Instant::now() > deadline {
                log.say(&format!("CLICK-TO-WINDOW never ({kind}); no window mapped in 120 s"));
                return Err("no window appeared".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The owner stays for the session's life so that the mutex does, and so
    /// that there is something to unwind it. It waits on the compositor because
    /// the compositor is the session: when its window closes, or its own
    /// deadline passes, the rest has nothing left to carry.
    fn supervise(cfg: &Config, p: &Paths, log: &mut Log, mut s: Session) -> Result<(), String> {
        let rc = s.comp.wait().map_err(|e| format!("waiting on the compositor: {e}"))?;
        log.say(&format!("the compositor exited: {rc}"));
        let _ = s.relay.kill();
        let _ = s.tunnel.kill();
        for g in s.gpus.iter_mut() {
            let _ = g.kill();
        }
        let _ = ssh(cfg, p, &["down", &s.id]);
        let _ = std::fs::remove_file(&p.state);
        let _ = std::fs::remove_file(&s.comp_log);
        log.say("session down");
        Ok(())
    }

    // ------------------------------------------------------------ install ---

    /// Put a shortcut on the desktop and in the Start menu, wearing the
    /// application's own icon and carrying its taskbar identity.
    ///
    /// The icon comes from the VM host, because that is the only machine that
    /// has one: the same fact §16 is about, arriving at install time instead
    /// of at run time. Since §92 both shortcuts are written through
    /// `IShellLinkW` rather than PowerShell's `WScript.Shell`, because that
    /// object cannot set `System.AppUserModel.ID`, and a shortcut without it
    /// pins as a button of its own beside the application's windows.
    pub fn install(cfg: &Config, p: &Paths, log: &mut Log, id: &str) -> Result<(), String> {
        let app = cfg.app(id)?;
        refresh(cfg, p, log, id, Refresh::Full);
        // The desktop is asked for rather than assumed: on this guest it is
        // redirected into OneDrive, and a file written to C:\Users\<guest-user>\Desktop
        // is a file nobody sees.
        let desktop = crate::shell::known_folder(&crate::shell::FOLDER_DESKTOP)?;
        let path = desktop.join(format!("{}.lnk", file_name(&app.title)));
        crate::shell::save_link(&shortcut(p, id, app), &path)?;
        log.say(&format!("installed {}", path.display()));
        Ok(())
    }

    // ------------------------------------------------------------ §92 pins ---

    /// How much of [`refresh`] to do.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Refresh {
        /// Ask the host about the application, and rewrite its icon, its
        /// relaunch file, its Start menu entry and its jump list.
        Full,
        /// The jump list only: the owner's periodic pass, which follows the
        /// history and does not rewrite shortcuts every two minutes.
        ListOnly,
    }

    fn file_name(title: &str) -> String {
        title.chars().map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c }).collect()
    }

    fn exe_path() -> String {
        std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_default()
    }

    fn cached_meta(p: &Paths, section: &str) -> Option<appmeta::Meta> {
        std::fs::read_to_string(p.apps.join(format!("{section}.meta")))
            .ok()
            .and_then(|t| appmeta::Meta::parse(&t).ok())
    }

    fn app_aumid(p: &Paths, section: &str, app: &crate::config::App) -> String {
        appmeta::aumid(&app.app_id, &app.desktop, cached_meta(p, section).as_ref())
    }

    /// The relaunch file the compositor will look for for this application.
    pub fn relaunch_file(p: &Paths, section: &str, app: &crate::config::App) -> std::path::PathBuf {
        let aumid = app_aumid(p, section, app);
        p.apps.join(crate::taskbar::relaunch_file_name(&aumid).unwrap_or_default())
    }

    fn icon_path(p: &Paths, section: &str) -> Option<String> {
        let i = p.exe_dir.join(format!("{section}.ico"));
        i.exists().then(|| i.display().to_string())
    }

    /// The shortcut a click, the Start menu or a pin runs: `wc-launch.exe <section>`.
    fn shortcut(p: &Paths, section: &str, app: &crate::config::App) -> crate::shell::LinkSpec {
        crate::shell::LinkSpec {
            target: exe_path(),
            args: appmeta::quote_arg(section),
            workdir: p.exe_dir.display().to_string(),
            description: format!("{}, running on the Linux host", app.title),
            icon: icon_path(p, section),
            aumid: Some(app_aumid(p, section, app)),
            title: None,
        }
    }

    /// An `.ico` from the host's PNG (or ICO) bytes; `None` if they are neither.
    fn to_ico(bytes: &[u8]) -> Option<Vec<u8>> {
        if bytes.starts_with(&[0, 0, 1, 0]) {
            return Some(bytes.to_vec());
        }
        ico::wrap_png(bytes).ok()
    }

    /// §92. Make `[app.<section>]` pinnable, and bring its jump list up to date.
    ///
    /// Never fatal: every step that fails is logged and the next one runs, so
    /// a host that cannot be reached costs a stale jump list and not a window.
    /// Logged by counts only, because the jump list's pages are the user's
    /// browsing history.
    pub fn refresh(cfg: &Config, p: &Paths, log: &mut Log, section: &str, what: Refresh) {
        let Ok(app) = cfg.app(section) else { return };
        crate::shell::init();
        let t = Instant::now();
        let _ = std::fs::create_dir_all(&p.apps);
        let jump = p.jump.join(section);
        let _ = std::fs::create_dir_all(&jump);
        let dark = crate::shell::taskbar_is_dark();

        // 1. What the host says about the application, kept for the next
        //    launch and the periodic pass.
        let meta_path = p.apps.join(format!("{section}.meta"));
        if what == Refresh::Full {
            let theme = if dark { "dark" } else { "light" };
            // §94: by the host's id; the host knows the desktop entry and the
            // executable it belongs to.
            match ssh(cfg, p, &["app", &app.host_app, theme]) {
                Ok(out) => match appmeta::Meta::parse(&out) {
                    Ok(_) => {
                        let _ = std::fs::write(&meta_path, &out);
                    }
                    Err(e) => log.say(&format!("pins: the host's answer about {}: {e}", app.desktop)),
                },
                Err(e) => log.say(&format!("pins: app {}: {}; using what was cached", app.desktop, first_line(&e))),
            }
        }
        let meta = cached_meta(p, section);
        let aumid = appmeta::aumid(&app.app_id, &app.desktop, meta.as_ref());

        if what == Refresh::Full {
            // 2. The icon, the relaunch file and the Start menu entry.
            let ico_path = p.exe_dir.join(format!("{section}.ico"));
            match meta.as_ref().and_then(|m| m.icon_png.as_deref()).map(to_ico) {
                Some(Some(ico)) => {
                    if let Err(e) = std::fs::write(&ico_path, &ico) {
                        log.say(&format!("pins: {}: {e}", ico_path.display()));
                    }
                }
                _ => log.say("pins: no icon from the host; the pin wears the last one written, or the launcher's"),
            }
            let relaunch = appmeta::relaunch(&exe_path(), section, &app.title, icon_path(p, section).as_deref());
            if let Some(name) = crate::taskbar::relaunch_file_name(&aumid)
                && let Err(e) = std::fs::write(p.apps.join(&name), relaunch.render())
            {
                log.say(&format!("pins: {name}: {e}"));
            }
            match crate::shell::known_folder(&crate::shell::FOLDER_PROGRAMS) {
                Ok(dir) => {
                    let lnk = dir.join(format!("{}.lnk", file_name(&app.title)));
                    if let Err(e) = crate::shell::save_link(&shortcut(p, section, app), &lnk) {
                        log.say(&format!("pins: Start menu {}: {e}", lnk.display()));
                    }
                }
                Err(e) => log.say(&format!("pins: {e}")),
            }
        }

        // 3. The jump list. Pages only for an application whose Exec= takes a
        //    URL; the host answers `prefs none` for one without a profile in
        //    its definition (§94: the profile is the host's to name).
        let Some(meta) = meta else {
            log.say(&format!("pins: {aumid}: no desktop entry known yet, so no jump list"));
            return;
        };
        let (prefs, pages) = if meta.takes_url {
            match ssh(cfg, p, &["frequent", &app.host_app]) {
                Ok(out) => appmeta::parse_frequent(&out),
                Err(e) => {
                    // First line only: a failed reply can carry the pages after it.
                    log.say(&format!("pins: frequent: {}", first_line(&e)));
                    (appmeta::JumpPrefs::default(), Vec::new())
                }
            }
        } else {
            (appmeta::JumpPrefs::default(), Vec::new())
        };
        let _ = std::fs::write(jump.join("refresh-secs"), prefs.refresh_secs.to_string());
        // Icons: the tasks' as `task-<id>.ico`, the pages' as
        // `page-<hash of the URL>.ico`, so a link keeps its file across
        // refreshes and Explorer's icon cache stays right. Old pages' icons
        // are removed.
        let mut task_icons = std::collections::BTreeMap::new();
        for a in &meta.actions {
            if let Some(ico) = a.icon_png.as_deref().and_then(to_ico) {
                let f = jump.join(format!("task-{}.ico", a.id));
                if std::fs::write(&f, ico).is_ok() {
                    task_icons.insert(a.id.clone(), f.display().to_string());
                }
            }
        }
        let mut page_icons = std::collections::BTreeMap::new();
        for (i, page) in pages.iter().enumerate() {
            if let Some(ico) = page.icon.as_deref().and_then(to_ico) {
                let f = jump.join(format!("page-{:016x}.ico", fnv(page.url.as_bytes())));
                if std::fs::write(&f, ico).is_ok() {
                    page_icons.insert(i, f.display().to_string());
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(&jump) {
            let keep: std::collections::BTreeSet<String> = page_icons.values().cloned().collect();
            for e in rd.flatten() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("page-") && !keep.contains(&path.display().to_string()) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        let removed_path = jump.join("removed.txt");
        let mut removed = appmeta::parse_removed(&std::fs::read_to_string(&removed_path).unwrap_or_default());
        let langs = appmeta::lang_candidates(&crate::shell::ui_languages());
        let app_icon = icon_path(p, section);
        let exe = exe_path();
        let workdir = p.exe_dir.display().to_string();
        let committed = crate::shell::serialised("Local\\waycompose-jumplist", || {
            crate::shell::commit_jump_list(&aumid, |gone| {
                for args in gone {
                    if let Some(u) = appmeta::url_of_args(args) {
                        removed.insert(u);
                    }
                }
                let plan = appmeta::plan(section, &meta, &prefs, &pages, &langs, &removed);
                let spec = |l: &appmeta::Link| crate::shell::LinkSpec {
                    target: exe.clone(),
                    args: appmeta::command_line(&l.args),
                    workdir: workdir.clone(),
                    description: l.description.clone(),
                    icon: match &l.icon {
                        appmeta::IconRef::App => app_icon.clone(),
                        appmeta::IconRef::Task(id) => task_icons.get(id).cloned().or_else(|| app_icon.clone()),
                        appmeta::IconRef::Page(i) => page_icons.get(i).cloned().or_else(|| app_icon.clone()),
                    },
                    aumid: None,
                    title: Some(l.title.clone()),
                };
                let cats = plan.categories.iter().map(|(n, ls)| (n.clone(), ls.iter().map(spec).collect())).collect();
                (cats, plan.tasks.iter().map(spec).collect())
            })
        });
        let _ = std::fs::write(&removed_path, removed.iter().map(|u| format!("{u}\n")).collect::<String>());
        match committed {
            Ok(c) => log.say(&format!(
                "pins: {aumid}: jump list {} task(s), {} ({} slot(s)){}; {} page(s) removed by the user; {} ms",
                c.tasks,
                if c.categories.is_empty() {
                    "no pages".to_string()
                } else {
                    c.categories.iter().map(|(n, k)| format!("{n}: {k} page(s)")).collect::<Vec<_>>().join(", ")
                },
                c.slots,
                if c.warnings.is_empty() { String::new() } else { format!(", {}", c.warnings.join(", ")) },
                removed.len(),
                t.elapsed().as_millis()
            )),
            Err(e) => log.say(&format!("pins: {aumid}: jump list not committed: {e}")),
        }
    }

    fn first_line(s: &str) -> String {
        s.lines().next().unwrap_or("").chars().take(160).collect()
    }

    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3))
    }

    /// §92. The owner's periodic pass: every application with a cached host
    /// answer gets its jump list rebuilt every `refreshInSeconds`, so the
    /// frequent pages follow the history while the session runs. A thread of
    /// its own, with its own log handle and its own COM apartment; it ends
    /// with the owner's process.
    fn start_list_refresher(cfg: &Config, session: &str) {
        let cfg = cfg.clone();
        let session = session.to_string();
        std::thread::spawn(move || {
            let Ok(p) = crate::paths(&session) else { return };
            let mut log = Log::open(&p.log);
            loop {
                let secs = cfg
                    .apps
                    .keys()
                    .filter_map(|s| std::fs::read_to_string(p.jump.join(s).join("refresh-secs")).ok())
                    .filter_map(|s| s.trim().parse::<u64>().ok())
                    .min()
                    .unwrap_or(120)
                    .clamp(30, 3600);
                std::thread::sleep(Duration::from_secs(secs));
                for section in cfg.apps.keys() {
                    if p.apps.join(format!("{section}.meta")).exists() {
                        refresh(&cfg, &p, &mut log, section, Refresh::ListOnly);
                    }
                }
            }
        });
    }
}
