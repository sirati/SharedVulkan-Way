//! §106 on macOS: the main process's host integration, kept out of `mac.rs`
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
    // The face the user works in yields the foreground to what is opened.
    let opener = MacOpener {
        browser: browser(cfg),
        before_open: Some(std::sync::Arc::new(|bid: &str| {
            wc_compositor::yield_activation_to(bid);
        })),
    };
    let (tx, rx) = channel::<Event>();
    // The handlers now and every minute; ends with the channel. The audio
    // devices are the sound ring's (sound_mac.rs, §103.16), not this channel's.
    let _ = std::thread::Builder::new().name("hostint-handlers".into()).spawn(move || {
        while tx.send(Event::Handlers(wc_hostplat::mac::handlers::handlers())).is_ok() {
            std::thread::sleep(HANDLERS_EVERY);
        }
    });
    say(&format!("hostint: serving, browser {:?}, share {:?}", opener.browser, path_map(cfg)));
    // Sleeps on the lane's doorbell (macos-zero-copy), not a poll.
    let wait = wc_compositor::hostint::doorbell_wait();
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
            // Plain bytes, not std::fs::copy: on macOS that is fcopyfile,
            // which the virtio-fs share does not implement (measured: ENOSYS).
            let copied = std::fs::File::open(real)
                .and_then(|mut from| std::fs::File::create(&host_dest).and_then(|mut to| std::io::copy(&mut from, &mut to)));
            copied.map_err(|e| format!("{host_dest}: {e}"))?;
            Ok(format!("open:{url}"))
        }
    }
}

/// The test verbs, for the person testing on the Mac:
///
/// ```text
/// wc-launch --hostint-probe            devices, defaults, nodes, handlers
/// wc-launch --bundle-test DIR [--keep] a Linux bundle registers for .txt/.html/https
/// wc-launch --host-bundle-test DIR    the main process's bundle, in DIR
/// ```
pub fn cli(args: &[String]) -> Option<std::process::ExitCode> {
    use std::process::ExitCode;
    if args.iter().any(|a| a == "--hostint-probe") {
        print!("{}", wc_hostplat::mac::probe());
        return Some(ExitCode::SUCCESS);
    }
    if let Some(i) = args.iter().position(|a| a == "--share-test") {
        // List DIR, read every file's first line, write one file: what the
        // main process does to the share when it copies into the drop folder.
        let dir = PathBuf::from(args.get(i + 1).cloned().unwrap_or_default());
        match std::fs::read_dir(&dir) {
            Ok(rd) => {
                for e in rd.flatten() {
                    let first = std::fs::read_to_string(e.path()).map(|t| t.lines().next().unwrap_or("").to_string());
                    println!("{:?}: {:?}", e.file_name(), first.map_err(|e| e.to_string()));
                }
            }
            Err(e) => println!("read_dir {}: {e}", dir.display()),
        }
        let p = dir.join("from-mac-host-bundle.txt");
        println!("write {}: {:?}", p.display(), std::fs::write(&p, "written by org.waycompose.host\n").map_err(|e| e.to_string()));
        return Some(ExitCode::SUCCESS);
    }
    if let Some(i) = args.iter().position(|a| a == "--open-test") {
        let root = args.get(i + 1).cloned().unwrap_or_default();
        for l in open_test(&root, &args[(i + 2).min(args.len())..]) {
            println!("{l}");
        }
        return Some(ExitCode::SUCCESS);
    }
    if let Some(i) = args.iter().position(|a| a == "--host-bundle-test") {
        // The main process's bundle in a scratch folder, for a TCC test.
        let dir = args.get(i + 1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp/wc-bundle-test"));
        let r = std::env::current_exe().map_err(|e| e.to_string()).and_then(|e| install_host_bundle_at(&dir.join(HOST_BUNDLE), &e));
        return Some(match r {
            Ok(b) => {
                println!("{}", b.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("wc-launch: {e}");
                ExitCode::from(1)
            }
        });
    }
    if let Some(i) = args.iter().position(|a| a == "--bundle-test") {
        let dir = args.get(i + 1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp/wc-bundle-test"));
        let keep = args.iter().any(|a| a == "--keep");
        return Some(match bundle_test(&dir, keep) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("wc-launch: {e}");
                ExitCode::from(1)
            }
        });
    }
    None
}

/// Linux's requests as the ring would carry them, through the same
/// `Service` and `MacOpener` the main process uses, with `root` as the only
/// share. Each argument is `url:<url>`, `path:<host path>` or
/// `with:<bundle id>:<url or host path>` (the bundle id is turned into the
/// token the host offers it under, as Linux would name it). Returns the
/// request and reply lines.
fn open_test(root: &str, reqs: &[String]) -> Vec<String> {
    use wc_hostint::host::Opener;
    use wc_hostint::wire::{Msg, Req, Target};
    let mut svc = Service::new(SvcConfig {
        platform: "macos",
        schemes: wc_hostint::openurl::default_schemes(),
        paths: PathMap { shares: vec![(root.to_string(), "/linux/share".into())], drop: None },
        windows_paths: false,
    });
    let hs = wc_hostplat::mac::handlers::handlers();
    let token_of = |bid: &str| hs.iter().find(|h| h.app.eq_ignore_ascii_case(bid)).map(|h| h.offer.token.clone());
    svc.handlers(hs.clone());
    let mut opener = MacOpener::default();
    let mut out = vec![format!("browser {:?} installed: {}", opener.browser, wc_hostplat::mac::open::app_installed(&opener.browser))];
    for (n, r) in reqs.iter().enumerate() {
        let target = |t: &str| if t.starts_with('/') { Target::HostPath(t.into()) } else { Target::Url(t.into()) };
        let req = if let Some(u) = r.strip_prefix("url:") {
            Req::Url(u.into())
        } else if let Some(p) = r.strip_prefix("path:") {
            Req::Path(p.into())
        } else if let Some((bid, t)) = r.strip_prefix("with:").and_then(|x| x.split_once(':')) {
            Req::With { token: token_of(bid).unwrap_or_else(|| "nosuchtoken".into()), target: target(t) }
        } else {
            out.push(format!("{r}: not url:, path: or with:"));
            continue;
        };
        let line = Msg::Req { n: n as u64 + 1, req }.encode();
        let reply = svc.on_line(&line, &mut opener as &mut dyn Opener, std::time::Instant::now());
        let shown = match reply.as_deref().map(Msg::decode) {
            Some(Ok(Msg::Rep { result, .. })) => format!("{result:?}"),
            other => format!("{other:?}"),
        };
        out.push(format!("{r} -> {shown}"));
    }
    out
}

const LSREGISTER: &str =
    "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

/// Write a Linux application's bundle the way `--install` does (its
/// Info.plist from `macbundle::info_plist_for`, a copy of this program),
/// register it, ask Launch Services whether it now opens text/plain,
/// text/html and https, check that the offers to Linux leave it out, then
/// unregister and remove it (unless `keep`).
fn bundle_test(dir: &Path, keep: bool) -> Result<(), String> {
    use wc_hostplat::mac::handlers::{for_scheme, for_uti, handlers};
    let section = "hostint-probe";
    let bid = crate::macbundle::bundle_id(section);
    let mimes: Vec<String> = ["text/plain", "text/html", "x-scheme-handler/https"].iter().map(|s| s.to_string()).collect();
    let b = dir.join("HostInt Probe.app");
    std::fs::create_dir_all(b.join("Contents/MacOS")).map_err(|e| e.to_string())?;
    std::fs::write(b.join("Contents/Info.plist"), crate::macbundle::info_plist_for(section, "HostInt Probe", &mimes))
        .map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::fs::copy(&exe, b.join("Contents/MacOS").join(EXECUTABLE)).map_err(|e| e.to_string())?;
    let st = Command::new(LSREGISTER).arg("-f").arg(&b).status().map_err(|e| e.to_string())?;
    println!("lsregister -f {}: {st}", b.display());
    // Launch Services' database is updated asynchronously.
    let mut ok = false;
    for _ in 0..20 {
        let txt = for_uti("public.plain-text").0;
        let html = for_uti("public.html").0;
        let https = for_scheme("https").0;
        let has = |v: &[String]| v.iter().any(|x| x.eq_ignore_ascii_case(&bid));
        if has(&txt) && has(&html) && has(&https) {
            println!("public.plain-text handlers: {txt:?}");
            println!("public.html handlers: {html:?}");
            println!("https handlers: {https:?}");
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("{bid} opens .txt, .html and https per Launch Services: {ok}");
    let offered = handlers().iter().any(|h| h.app.eq_ignore_ascii_case(&bid));
    println!("{bid} offered to Linux as a host handler: {offered} (must be false)");
    if !keep {
        let _ = Command::new(LSREGISTER).arg("-u").arg(&b).status();
        let _ = std::fs::remove_dir_all(&b);
        println!("unregistered and removed {}", b.display());
    }
    if ok && !offered { Ok(()) } else { Err("the bundle test failed".into()) }
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
    install_host_bundle_at(&host_bundle(), exe)
}

/// [`install_host_bundle`] at another place (`--host-bundle-test`).
pub fn install_host_bundle_at(b: &Path, exe: &Path) -> Result<PathBuf, String> {
    let b = b.to_path_buf();
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
