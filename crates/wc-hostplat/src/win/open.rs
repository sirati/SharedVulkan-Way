//! Linux to Windows: open what `wc_hostint::host::Service` passed.
//!
//! Every call gets a value the host checked or produced: a re-serialised
//! URL, a path inside a share whose extension is not on the dangerous list,
//! or an executable path from the host's own handler enumeration. Nothing
//! goes through `cmd.exe` or a command string: a URL or a path is
//! `ShellExecuteW`'s `lpFile` with the explicit verb `open`, and an
//! application is `CreateProcessW` of its executable with the target as one
//! quoted argument (std's `Command`, which quotes by the MSVC rules and is
//! given `.exe` files only, so it never goes through `cmd.exe` either).
//!
//! Before a path is opened, Windows' own `AssocIsDangerous` is asked too
//! (security-model R38): the Service's list is the floor, the system's list
//! (which administrators extend by policy) is added on top.

use windows_sys::Win32::UI::Shell::{AssocIsDangerous, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use wc_hostint::host::Opener;
use wc_hostint::wire::Target;

use super::com::{Apartment, wide};

/// Opens on this desktop. `browser`: an executable to open URLs with
/// instead of the user's default browser.
#[derive(Debug, Default, Clone)]
pub struct WinOpener {
    pub browser: Option<String>,
    /// The browser's fixed arguments, before the URL (`[hostint] browser-arg`).
    pub browser_args: Vec<String>,
}

/// Whether `exe` names an executable we would start directly: an absolute
/// drive path to an existing `.exe`. A `.bat` or `.cmd` would make
/// `CreateProcess` run `cmd.exe`, so they are refused.
pub fn is_exe(exe: &str) -> bool {
    let b = exe.as_bytes();
    b.len() > 7
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] == b'\\'
        && exe.to_ascii_lowercase().ends_with(".exe")
        && std::path::Path::new(exe).is_file()
}

/// Windows' own verdict on a path's type.
pub fn assoc_is_dangerous(path: &str) -> bool {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path).trim_end_matches(['.', ' ']);
    let Some((_, ext)) = name.rsplit_once('.') else { return false };
    let w = wide(&format!(".{ext}"));
    // SAFETY: a NUL-terminated string.
    unsafe { AssocIsDangerous(w.as_ptr()) != 0 }
}

fn shell_open(file: &str) -> Result<(), String> {
    let _com = Apartment::sta();
    let verb = wide("open");
    let f = wide(file);
    // SAFETY: NUL-terminated strings; no parameters, no directory.
    let h = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), f.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    let code = h as isize;
    if code > 32 { Ok(()) } else { Err(format!("ShellExecuteW failed ({code})")) }
}

fn spawn(exe: &str, args: &[String]) -> Result<(), String> {
    if !is_exe(exe) {
        return Err("the application is not an executable on this machine".into());
    }
    std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("CreateProcess: {e}"))
}

fn check_path(p: &str) -> Result<(), String> {
    if assoc_is_dangerous(p) {
        return Err("Windows considers that type of file dangerous to open".into());
    }
    Ok(())
}

impl Opener for WinOpener {
    fn open_url(&mut self, url: &str) -> Result<(), String> {
        match &self.browser {
            Some(b) => spawn(b, &crate::launch::browser_argv(&self.browser_args, url)),
            None => shell_open(url),
        }
    }

    fn open_path(&mut self, path: &str) -> Result<(), String> {
        check_path(path)?;
        shell_open(path)
    }

    fn open_with(&mut self, app: &str, target: &Target) -> Result<(), String> {
        match target {
            Target::Url(u) => spawn(app, std::slice::from_ref(u)),
            Target::HostPath(p) => {
                check_path(p)?;
                spawn(app, std::slice::from_ref(p))
            }
        }
    }
}
