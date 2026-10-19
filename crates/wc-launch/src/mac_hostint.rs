//! §105 on macOS: the main process's host integration, kept out of `mac.rs`
//! so that file only calls in.
//!
//! * [`start`]: once the host has claimed the `OWNER_HOSTINT` ring slot, serve
//!   it (`wc_compositor::hostint::spawn`): Linux's requests to open URLs and
//!   files here go through `wc_hostint::host::Service` and then
//!   `wc_hostplat::mac::open::MacOpener`; the audio devices and the
//!   handlers are pushed as they change.
//! * [`map_open_key`]: a document a face was asked to open
//!   (`application:openURLs:`) is sent to Linux by its Linux path, copied
//!   into the share's drop folder first if it is outside the share.
//! * [`install_host_bundle`] and [`start_main_in_host_bundle`]: the main
//!   process runs from its own bundle, `org.waycompose.host`, started
//!   through Launch Services, so macOS takes it, not the face that started
//!   it, as the process responsible for the microphone (TCC rule 1).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::channel;
use std::time::Duration;

use sv_transport::wlbytes::ByteChannel;
use wc_hostint::host::{Config as SvcConfig, Event, Service};
use wc_hostint::wire::PathMap;
use wc_hostplat::mac::open::MacOpener;
use wc_hostplat::mac::plan::DEFAULT_BROWSER;
use wc_hostplat::share::{plan as file_plan, unique_dir, FilePlan};

use crate::config::Config;
use crate::macbundle::{host_info_plist, EXECUTABLE, HOST_BUNDLE};

type Say = Box<dyn FnMut(&str) + Send>;

/// How often the handlers are asked for again. Launch Services has no
/// change notification; the Service sends the list only when it changed.
const HANDLERS_EVERY: Duration = Duration::from_secs(60);

/// The share as the Mac sees it. `[hostint]` names it; a value that is not
/// a POSIX path (the Windows default) means no share.
pub fn path_map(cfg: &Config) -> PathMap {
    let h = &cfg.hostint;
    if h.share.0.starts_with('/') && h.share.1.starts_with('/') { h.path_map() } else { PathMap::default() }
}

fn browser(cfg: &Config) -> String {
    let b = cfg.hostint.browser.trim();
    if b.is_empty() || b.contains(['/', '\\']) { DEFAULT_BROWSER.into() } else { b.into() }
}

/// Serve the host-integration slot on threads of their own.
pub fn start(ch: ByteChannel, cfg: &Config, mut say: Say) {
    let svc = Service::new(SvcConfig {
        platform: "macos",
        schemes: wc_hostint::openurl::default_schemes(),
        paths: path_map(cfg),
        windows_paths: false,
    });
    let opener = MacOpener { browser: browser(cfg) };
    let (tx, rx) = channel::<Event>();
    let watch = match wc_hostplat::mac::audio::watch(tx.clone()) {
        Ok(w) => Some(w),
        Err(e) => {
            say(&format!("hostint: no audio device watch: {e}"));
            None
        }
    };
    // The handlers now and every minute; ends with the channel, and the
    // audio watch with it.
    let _ = std::thread::Builder::new().name("hostint-handlers".into()).spawn(move || {
        let _watch = watch;
        while tx.send(Event::Handlers(wc_hostplat::mac::handlers::handlers())).is_ok() {
            std::thread::sleep(HANDLERS_EVERY);
        }
    });
    say(&format!("hostint: serving, browser {:?}, share {:?}", opener.browser, path_map(cfg)));
    // Until the rings have a doorbell this side can sleep on (macos-zero-copy),
    // a short sleep: the channel carries a few lines a minute.
    let wait: wc_compositor::hostint::Wait = Box::new(|_ch, t| std::thread::sleep(t.min(Duration::from_millis(20))));
    if let Err(e) = wc_compositor::hostint::spawn(ch, wait, svc, Box::new(opener), rx, say) {
        eprintln!("wc-launch: hostint: {e}");
    }
}

static COPIES: AtomicU32 = AtomicU32::new(0);

/// A face's request key with a `file://` URL of a host file made into the
/// URL of its Linux path: in the share as it is, outside it copied into a
/// directory of its own in the drop folder. Other keys pass unchanged
/// (Linux checks every URL itself, `url_ok`).
pub fn map_open_key(cfg: &Config, key: &str) -> Result<String, String> {
    let Some(url) = key.strip_prefix("open:") else { return Ok(key.into()) };
    if !url.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("file://")) {
        return Ok(key.into());
    }
    let path = wc_hostint::linux::file_url_path(url).ok_or_else(|| format!("{url:?} is not a local file URL"))?;
    let real = std::fs::canonicalize(&path).map_err(|e| format!("{path}: {e}"))?;
    if !real.is_file() {
        return Err(format!("{path}: only files are opened in Linux applications"));
    }
    let real = real.to_str().ok_or("the path is not UTF-8")?;
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let unique = unique_dir(secs, std::process::id(), COPIES.fetch_add(1, Ordering::Relaxed));
    match file_plan(real, &path_map(cfg), &unique)
        .ok_or("the file is outside the shared folder, and no drop folder is configured ([hostint])")?
    {
        FilePlan::Shared { url } => Ok(format!("open:{url}")),
        FilePlan::Copy { host_dir, host_dest, url } => {
            std::fs::create_dir_all(&host_dir).map_err(|e| format!("{host_dir}: {e}"))?;
            std::fs::copy(real, &host_dest).map_err(|e| format!("{host_dest}: {e}"))?;
            Ok(format!("open:{url}"))
        }
    }
}

/// The test verbs, for the person testing on the Mac:
///
/// ```text
/// wc-launch --hostint-probe            devices, defaults, nodes, handlers
/// wc-launch --render-test UID|- MS     play a 440 Hz tone to one device
/// wc-launch --capture-test UID|- MS    record from one (only in the host
///                                      bundle: turns a microphone on)
/// ```
pub fn cli(args: &[String]) -> Option<std::process::ExitCode> {
    use std::process::ExitCode;
    if args.iter().any(|a| a == "--hostint-probe") {
        print!("{}", wc_hostplat::mac::probe());
        return Some(ExitCode::SUCCESS);
    }
    let i = args.iter().position(|a| a == "--render-test" || a == "--capture-test")?;
    let render = args[i] == "--render-test";
    let uid = args.get(i + 1).filter(|u| *u != "-").map(String::as_str);
    let ms: u64 = args.get(i + 2).and_then(|s| s.parse().ok()).unwrap_or(1000);
    Some(match stream_test(render, uid, ms) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wc-launch: {e}");
            ExitCode::from(1)
        }
    })
}

fn stream_test(render: bool, uid: Option<&str>, ms: u64) -> Result<String, String> {
    use std::io::{Read, Write};
    use wc_hostplat::mac::stream::{Capture, Format, Render, Sizing};
    let fmt = Format { rate: 48_000, channels: 2 };
    let t = std::time::Instant::now();
    let end = t + Duration::from_millis(ms);
    if render {
        let r = Render::start(uid, fmt, Sizing::default())?;
        let mut w = r.writer();
        let mut n: u64 = 0;
        // 10 ms at a time, paced by the clock: what a ring would deliver.
        while std::time::Instant::now() < end && r.running() {
            let mut buf = Vec::with_capacity(480 * 4);
            for _ in 0..480 {
                let v = ((n as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin() * 8000.0) as i16;
                buf.extend_from_slice(&v.to_le_bytes());
                buf.extend_from_slice(&v.to_le_bytes());
                n += 1;
            }
            let _ = w.write_all(&buf);
            let due = t + Duration::from_micros(n * 1_000_000 / 48_000);
            if let Some(d) = due.checked_duration_since(std::time::Instant::now()) {
                std::thread::sleep(d);
            }
        }
        std::thread::sleep(Duration::from_millis(150));
        Ok(format!("render {uid:?}: {} ({:?})", r.stats().line(fmt.rate), t.elapsed()))
    } else {
        let c = Capture::start(uid, fmt, Sizing::default())?;
        let mut rd = c.reader();
        rd.set_timeout(Some(Duration::from_millis(200)));
        let mut buf = vec![0u8; 4800];
        let (mut bytes, mut peak) = (0usize, 0i32);
        while std::time::Instant::now() < end && c.running() {
            let n = rd.read(&mut buf).unwrap_or(0);
            bytes += n;
            for s in buf[..n].chunks_exact(2) {
                peak = peak.max(i32::from(i16::from_le_bytes([s[0], s[1]])).abs());
            }
        }
        Ok(format!("capture {uid:?}: {} bytes, peak {peak}, {}", bytes, c.stats().line(fmt.rate)))
    }
}

fn support_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp")).join("Library/Application Support/waycompose")
}

/// `~/Library/Application Support/waycompose/Waycompose Host.app`.
pub fn host_bundle() -> PathBuf {
    support_dir().join(HOST_BUNDLE)
}

/// Write the main process's bundle with a copy of `exe` and tell Launch
/// Services about it. Done by `--install`, which the installer runs. Not
/// signed here (TCC rule 4): ad hoc, as the linker left it, so a new build
/// is a new identity to TCC and the microphone is asked for again; a signed
/// deployment (the real Mac's Nix activation) keeps the grant.
pub fn install_host_bundle(exe: &Path) -> Result<PathBuf, String> {
    let b = host_bundle();
    let macos = b.join("Contents/MacOS");
    std::fs::create_dir_all(&macos).map_err(|e| e.to_string())?;
    std::fs::write(b.join("Contents/Info.plist"), host_info_plist()).map_err(|e| e.to_string())?;
    let target = macos.join(EXECUTABLE);
    let tmp = target.with_extension("new");
    std::fs::copy(exe, &tmp).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
    let _ = Command::new(
        "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
    )
    .args(["-f", &b.to_string_lossy()])
    .status();
    Ok(b)
}

/// Start the main process from its bundle through Launch Services
/// (`open -g -a`), with this process's `WC_*` variables, its output into
/// `log`. False if there is no host bundle (an older install) or `open`
/// failed: the caller then starts it directly, as before.
pub fn start_main_in_host_bundle(log: &Path, say: &dyn Fn(&str)) -> bool {
    let b = host_bundle();
    if !b.join("Contents/MacOS").join(EXECUTABLE).is_file() {
        return false;
    }
    let mut c = Command::new("/usr/bin/open");
    c.arg("-g").arg("-a").arg(&b).arg("--stdout").arg(log).arg("--stderr").arg(log);
    for (k, v) in std::env::vars() {
        if k.starts_with("WC_") {
            c.arg("--env").arg(format!("{k}={v}"));
        }
    }
    c.args(["--args", "--main"]).stdin(Stdio::null());
    match c.status() {
        Ok(s) if s.success() => {
            say(&format!("started the main process from {}", b.display()));
            true
        }
        Ok(s) => {
            say(&format!("open of {} failed ({s}); starting the main process directly", b.display()));
            false
        }
        Err(e) => {
            say(&format!("/usr/bin/open: {e}; starting the main process directly"));
            false
        }
    }
}
