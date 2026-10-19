//! Linux to macOS: open what `wc_hostint::host::Service` passed, with
//! `/usr/bin/open` and an argv (never a shell, never a command string).
//!
//! * `url`: http(s) with the configured browser by name, `open -a <App> --
//!   <url>` (default "Firefox"). If that application is not installed, or
//!   the scheme is not a web one (`mailto:`), `open -- <url>`: the system's
//!   handler for the scheme, which is the one the user chose. The URL passed
//!   the host's check either way.
//! * `path`: `open -- <path>`, the file's default application.
//! * `with`: `open -b <bundle id> -- <target>`; the bundle id is the host's,
//!   from its own handler enumeration (`handlers.rs`), never Linux's.
//!
//! Two checks of this platform's own, on top of the Service's
//! (security-model R38):
//! * **No application bundle is ever opened as a document.** The Service
//!   refuses `.app` and the other dangerous extensions; a directory is
//!   checked again here, after symlinks are resolved, by extension and by
//!   its contents (`Contents/Info.plist`, `Contents/MacOS`), so a renamed
//!   bundle is refused too.
//! * **No loop through Linux.** Before the system's default is used (a path,
//!   or a URL without the configured browser), Launch Services is asked
//!   which application that is; if it is one of our own bundles (a Linux
//!   application the user made the Mac's default), the request is refused,
//!   because the host would only hand it back to Linux.

use std::path::Path;
use std::process::{Command, Stdio};

use wc_hostint::host::Opener;
use wc_hostint::wire::Target;

use super::cf;
use super::plan::{is_bundle_dir, is_own_bundle, open_path_argv, open_url_argv, open_with_argv, DEFAULT_BROWSER, OPEN};

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSCopyDefaultApplicationURLForURL(url: cf::CFURLRef, roles: u32, err: *mut cf::CFErrorRef) -> cf::CFURLRef;
    fn LSCopyApplicationURLsForBundleIdentifier(bid: cf::CFStringRef, err: *mut cf::CFErrorRef) -> cf::CFArrayRef;
}

const ROLES_ALL: u32 = 0xFFFF_FFFF;

/// Called with the bundle id of the application about to be opened, before
/// it is: the main process asks the active face to yield the foreground to
/// it (macOS 14's cooperative activation).
pub type BeforeOpen = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// Opens on this Mac's desktop.
#[derive(Clone)]
pub struct MacOpener {
    /// The application `url` requests open in, by name, as `open -a` takes
    /// it ("Firefox", "Safari").
    pub browser: String,
    pub before_open: Option<BeforeOpen>,
}

impl Default for MacOpener {
    fn default() -> Self {
        MacOpener { browser: DEFAULT_BROWSER.into(), before_open: None }
    }
}

/// Where `open -a NAME` finds the application, in the usual folders.
pub fn installed_app_path(name: &str) -> Option<String> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    ["/Applications", "/System/Applications", &format!("{home}/Applications")]
        .iter()
        .map(|d| Path::new(d).join(format!("{name}.app")))
        .find(|p| p.is_dir())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Whether `open -a NAME` would find an application: one of the usual
/// application folders holds `NAME.app`. (`open -a` also searches Launch
/// Services; a browser elsewhere still works, it is just not taken as
/// "installed" here, and the system's default is used instead.)
pub fn app_installed(name: &str) -> bool {
    if name.is_empty() || name.contains('/') {
        return false;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    ["/Applications", "/System/Applications", &format!("{home}/Applications")]
        .iter()
        .any(|d| Path::new(d).join(format!("{name}.app")).is_dir())
}

/// Whether a bundle id is an application on this Mac.
pub fn bundle_installed(bid: &str) -> bool {
    let s = cf::string(bid);
    // SAFETY: a live CFString; the result is +1 and released by `Owned`.
    let a = cf::Owned::new(unsafe { LSCopyApplicationURLsForBundleIdentifier(s.get(), std::ptr::null_mut()) });
    a.is_some_and(|a| !cf::array(a.get()).is_empty())
}

/// The bundle id of the application the system would open `url` with
/// (`file://` URLs for paths).
fn default_app_for(url: &cf::Owned) -> Option<String> {
    // SAFETY: a live CFURL; the result is +1 and released by `Owned`.
    let app = cf::Owned::new(unsafe { LSCopyDefaultApplicationURLForURL(url.get(), ROLES_ALL, std::ptr::null_mut()) })?;
    cf::bundle_id_at(&cf::url_path(app.get())?)
}

fn refuse_own_default(url: Option<cf::Owned>) -> Result<(), String> {
    if let Some(bid) = url.as_ref().and_then(default_app_for)
        && is_own_bundle(&bid)
    {
        return Err(format!("the Mac's default for it is a Linux application ({bid}); not handed back to Linux"));
    }
    Ok(())
}

/// The R38 check for a host path: no application bundle, before or after
/// following symlinks.
pub fn check_path(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    let real = std::fs::canonicalize(p).map_err(|e| format!("{path}: {e}"))?;
    for q in [p, real.as_path()] {
        if q.is_dir() && is_bundle_dir(q, &|x: &Path| x.exists()) {
            return Err("that is an application bundle; it is never opened from Linux".into());
        }
    }
    Ok(())
}

fn run_open(argv: &[String]) -> Result<(), String> {
    let out = Command::new(OPEN)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{OPEN}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("open exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim()))
    }
}

impl MacOpener {
    /// Let `bundle_id` come to the front, then give the yield a moment to
    /// reach the face before `open` asks for the activation.
    fn yield_to(&self, bundle_id: Option<String>) {
        if let (Some(f), Some(b)) = (&self.before_open, bundle_id) {
            f(&b);
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    }
}

impl Opener for MacOpener {
    fn open_url(&mut self, url: &str) -> Result<(), String> {
        let browser = Some(self.browser.as_str()).filter(|b| app_installed(b));
        let argv = open_url_argv(url, browser);
        let target = if argv.first().map(String::as_str) == Some("--") {
            refuse_own_default(cf::url(url))?;
            cf::url(url).as_ref().and_then(default_app_for)
        } else {
            browser.and_then(installed_app_path).and_then(|p| cf::bundle_id_at(&p))
        };
        self.yield_to(target);
        run_open(&argv)
    }

    fn open_path(&mut self, path: &str) -> Result<(), String> {
        check_path(path)?;
        refuse_own_default(cf::file_url(path, Path::new(path).is_dir()))?;
        self.yield_to(cf::file_url(path, Path::new(path).is_dir()).as_ref().and_then(default_app_for));
        run_open(&open_path_argv(path))
    }

    fn open_with(&mut self, app: &str, target: &Target) -> Result<(), String> {
        if is_own_bundle(app) {
            return Err("not one of the host's applications".into());
        }
        if let Target::HostPath(p) = target {
            check_path(p)?;
        }
        self.yield_to(Some(app.to_string()));
        run_open(&open_with_argv(app, target))
    }
}
