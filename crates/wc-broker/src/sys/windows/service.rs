//! `sv-guest broker`: the broker as a Windows service (§111.8).
//!
//! ```text
//! sv-guest broker                    run under the service control manager
//! sv-guest broker --console          run in this console (tests)
//! sv-guest broker show               the devices and their descriptors
//! sv-guest broker revert-acl         give the devices their descriptors from before
//! sv-guest broker holder             the holder process (started by the broker)
//! sv-guest broker holder-quit        tell the holder to let go of everything and exit
//!
//!   --pipe NAME          the pipe (\\.\pipe\waycompose-broker)
//!   --pipe-sddl SDDL     who may connect (PIPE_SDDL: SYSTEM, Administrators, authenticated users)
//!   --device NAME=LOC    a device to lend, LOC is ivshmem:SLOT, port:NAME or path:PATH;
//!                        repeated; without any, the four of a waycompose guest
//!   --keys DIR           the installations' keys (C:\ProgramData\waycompose\keys)
//!   --state DIR          the broker's own files (C:\ProgramData\waycompose)
//!                        raw-device-owners.txt binds PnP instance IDs to reviewed SIDs;
//!                        absent/untrusted policy reserves all raw BARs
//!   --no-acl             leave the devices' descriptors alone
//!   --protected-bar      reserve raw BAR lending, require protected driver capabilities;
//!                        does not enable the unfinished client control protocol
//!   --budget-mib N       the most region memory held at once
//!   --holder-pipe NAME   the holder's pipe (\\.\pipe\waycompose-broker-holder)
//!   --no-holder          keep no copy of the regions in a holder process
//! ```
//!
//! Installed by `scripts/wc-launch.sh deploy` as the service
//! `waycompose-broker` (`LocalSystem`, automatic, restarted on failure), its
//! binary in `C:\Program Files\waycompose\`, which no user can write: a
//! SYSTEM service that ran a file its user could replace would hand that user
//! SYSTEM.

use super::{DeviceAt, DeviceLoc, PIPE_SDDL, Windows, acl, default_devices, pipe::wide};
use crate::core::{Allow, DeviceSpec, Release};
use crate::server::{Broker, Config};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows_sys::Win32::System::Threading::{SetEvent, WaitForSingleObject, INFINITE};

pub const SERVICE_NAME: &str = "waycompose-broker";

/// The installations' keys and the broker's files: SYSTEM and
/// Administrators only, nothing inherited.
pub const DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

#[derive(Clone, Debug)]
pub struct Args {
    pub console: bool,
    pub action: Option<String>,
    pub pipe: String,
    pub pipe_sddl: String,
    pub devices: Vec<DeviceAt>,
    pub keys: PathBuf,
    pub state: PathBuf,
    pub acl: bool,
    pub protected_bar: bool,
    pub budget: Option<u64>,
    pub holder: bool,
    pub holder_pipe: String,
}

fn program_data() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join("waycompose")
}

impl Args {
    pub fn parse(args: &[String]) -> Result<Args, String> {
        let mut a = Args {
            console: false,
            action: None,
            pipe: crate::client::DEFAULT_PATH.to_string(),
            pipe_sddl: PIPE_SDDL.to_string(),
            devices: Vec::new(),
            keys: program_data().join("keys"),
            state: program_data(),
            acl: true,
            protected_bar: false,
            budget: None,
            holder: true,
            holder_pipe: super::holder::HOLDER_PIPE.to_string(),
        };
        let mut it = args.iter();
        while let Some(x) = it.next() {
            let mut val = || it.next().cloned().ok_or_else(|| format!("{x} needs a value"));
            match x.as_str() {
                "--console" => a.console = true,
                "--no-acl" => a.acl = false,
                "--protected-bar" => a.protected_bar = true,
                "--pipe" => a.pipe = val()?,
                "--pipe-sddl" => a.pipe_sddl = val()?,
                "--keys" => a.keys = val()?.into(),
                "--state" => a.state = val()?.into(),
                "--budget-mib" => a.budget = Some(val()?.parse::<u64>().map_err(|e| e.to_string())? << 20),
                "--device" => {
                    let v = val()?;
                    let (n, l) = v.split_once('=').ok_or_else(|| format!("--device {v}: NAME=LOCATION"))?;
                    a.devices.push(DeviceAt { name: n.to_string(), loc: DeviceLoc::parse(l)? });
                }
                "show" | "ready" | "revert-acl" | "revert-acl-check" | "holder" | "holder-quit" if a.action.is_none() => a.action = Some(x.clone()),
                "--holder-pipe" => a.holder_pipe = val()?,
                "--no-holder" => a.holder = false,
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        if a.devices.is_empty() {
            a.devices = default_devices();
        }
        Ok(a)
    }
}

fn log(s: &str) {
    eprintln!("[wc-broker] {s}");
}

/// Make `dir` (and its parents) and give it [`DIR_SDDL`].
pub fn secure_dir(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let sd = super::pipe::Sd::from_sddl(DIR_SDDL).map_err(|e| e.to_string())?;
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl = std::ptr::null_mut();
    // SAFETY: the descriptor just made; live out-parameters.
    unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) };
    let w = wide(&dir.to_string_lossy());
    // SAFETY: a live path and a DACL living in `sd`.
    let r = unsafe {
        SetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if r != 0 {
        return Err(format!("{}: SetNamedSecurityInfoW: {}", dir.display(), std::io::Error::from_raw_os_error(r as i32)));
    }
    Ok(())
}

/// Read through one exclusively held handle and require a privileged owner
/// plus the exact protected SYSTEM/Administrators DACL. Merely residing in
/// ProgramData does not make a pre-existing user-owned file authoritative.
fn read_raw_owners(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo,
    };
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(0x8000_0000 | 0x0002_0000)
        .share_mode(0)
        .open(path)
        .map_err(|e| e.to_string())?;
    let fields = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut sd = std::ptr::null_mut();
    // SAFETY: live file handle with READ_CONTROL and descriptor out-pointer.
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            fields,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if result != 0 {
        return Err(format!("raw owner security query: {result}"));
    }
    let mut text = std::ptr::null_mut();
    // SAFETY: descriptor returned above and live string out-pointer.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            1,
            fields,
            &mut text,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: allocated by GetSecurityInfo.
    unsafe { LocalFree(sd) };
    if ok == 0 {
        return Err("raw owner security conversion failed".into());
    }
    let mut n = 0;
    // SAFETY: conversion returned a terminated UTF-16 string.
    while unsafe { *text.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: measured above, allocated by conversion and freed once.
    let descriptor = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, n) });
    unsafe { LocalFree(text.cast()) };
    if ![
        "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)",
        "O:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)",
        "O:BAD:PAI(A;;FA;;;SY)(A;;FA;;;BA)",
        "O:SYD:PAI(A;;FA;;;SY)(A;;FA;;;BA)",
    ]
    .contains(&descriptor.as_str())
    {
        return Err(format!(
            "untrusted raw-device ownership file security: {descriptor}"
        ));
    }
    if file.metadata().map_err(|e| e.to_string())?.len() >= 1024 * 1024 {
        return Err("raw-device ownership file too large".into());
    }
    let mut text = String::new();
    file.take(1024 * 1024)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    Ok(text)
}

/// Stamp only large-page sections. The locator reads one stamp per 2 MiB
/// unit, without translating addresses or scanning small pages.
fn stamp_guest_ram(bytes: &mut [u8], _id: u64, unit: u64) -> std::io::Result<Option<String>> {
    use sv_transport::lpstamp;
    if unit != lpstamp::UNIT {
        return Ok(None);
    }
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            algorithm: *mut core::ffi::c_void,
            buffer: *mut u8,
            len: u32,
            flags: u32,
        ) -> i32;
    }
    let mut nonce = [0u8; 16];
    // SAFETY: the system RNG writes exactly the nonce buffer's length.
    if unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            nonce.as_mut_ptr(),
            nonce.len() as u32,
            2,
        )
    } != 0
    {
        return Err(std::io::Error::other(
            "the system RNG could not make a guest RAM stamp",
        ));
    }
    let units = u32::try_from(bytes.len() as u64 / unit)
        .map_err(|_| std::io::Error::other("too many large pages"))?;
    for i in 0..units {
        let at = i as usize * unit as usize;
        bytes[at..at + lpstamp::STAMP_LEN].copy_from_slice(&lpstamp::stamp(&nonce, i, units));
    }
    Ok(Some(nonce.iter().map(|b| format!("{b:02x}")).collect()))
}

/// Start everything and serve until the process ends.
fn serve(a: &Args) -> Result<(), String> {
    let me = super::ident::current_user().unwrap_or_default();
    let system = me == super::ident::SYSTEM_SID;
    log(&format!("starting as {me}; pipe {}; keys {}", a.pipe, a.keys.display()));
    if system {
        // The keys move here from the users' install directories (§111.8):
        // only SYSTEM and Administrators may read them.
        secure_dir(&a.state).and_then(|()| secure_dir(&a.keys))?;
    }
    if a.acl {
        for l in acl::secure(&a.devices, &a.state) {
            log(&l);
        }
    } else {
        log("--no-acl: the devices' descriptors are left alone");
    }
    acl::verify_raw_protection(&a.devices)?;
    // Missing, unreadable or corrupt policy never becomes first-claim. Keep
    // ports/signing available while reserving every unbound raw BAR.
    let owners = read_raw_owners(&a.state.join("raw-device-owners.txt"))
        .and_then(|s| crate::raw_device::bindings(&s));
    if let Err(e) = &owners {
        log(&format!("raw BAR lending disabled: {e}"));
    }
    let mut paths = std::collections::BTreeMap::new();
    let mut devices = Vec::new();
    for d in &a.devices {
        let allow = match &d.loc {
            DeviceLoc::Port(_) => Allow::AnyUser,
            DeviceLoc::IvshmemSlot(_) => {
                let found = acl::find(d).map_err(|e| e.to_string())?;
                let owner = found
                    .as_ref()
                    .and_then(|f| {
                        owners
                            .as_ref()
                            .ok()
                            .and_then(|o| o.get(&f.instance.to_ascii_lowercase()))
                    })
                    .cloned();
                if let (Some(f), Some(owner)) = (found, owner) {
                    // A policy file cannot compensate for a device that a
                    // desktop user can open directly. Failed/unknown ACL
                    // installation must never enable a new broker grant.
                    if acl::live(&f.live).as_deref() != Ok(acl::DEVICE_SDDL)
                        || acl::property(&f.instance).ok().flatten().as_deref() != Some(acl::DEVICE_SDDL)
                    {
                        log(&format!("raw device {} reserved: device ACL protection is unknown", d.name));
                        devices.push(DeviceSpec { name: d.name.clone(), allow: Allow::Users(Vec::new()) });
                        continue;
                    }
                    paths.insert(d.name.clone(), f.live);
                    Allow::Users(vec![owner])
                } else {
                    log(&format!(
                        "raw device {} reserved: no reviewed physical-device SID binding",
                        d.name
                    ));
                    Allow::Users(Vec::new())
                }
            }
            // Arbitrary paths can alias physical memory and bypass slot
            // ownership. They remain available only to explicit library
            // test brokers, never the installed service.
            DeviceLoc::Path(_) => Allow::Users(Vec::new()),
        };
        devices.push(DeviceSpec {
            name: d.name.clone(),
            allow,
        });
    }
    if a.protected_bar && (!system || !a.acl) {
        return Err("protected BAR mode requires SYSTEM and raw-device ACL protection".into());
    }
    let protected_paths = paths.clone();
    let platform = Windows::bind(&a.pipe, &a.pipe_sddl, a.devices.clone()).map_err(|e| format!("pipe {}: {e}", a.pipe))?
        .with_raw_device_paths(paths)
        .with_region_init(Box::new(stamp_guest_ram));
    let platform = if a.protected_bar {
        let mut platform = platform.reserve_bars_for_protected_mode();
        for d in &a.devices {
            if matches!(d.loc, DeviceLoc::IvshmemSlot(_)) {
                let path = protected_paths.get(&d.name)
                    .ok_or_else(|| format!("protected BAR {} has no validated original-device binding", d.name))?;
                let original = wc_handle::open_ivshmem_path(path.clone()).map_err(|e| e.to_string())?;
                let device = super::protected_range::Device::from_original(original)
                    .map_err(|e| format!("protected BAR {} unavailable, raw lending stays disabled: {e}", d.name))?;
                platform = platform.with_protected_bar(&d.name, device).map_err(|e| e.to_string())?;
            }
        }
        log("protected BAR capability mode: no raw handles are lent; client control remains inactive");
        platform
    } else { platform };
    let platform = if a.holder {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let spawn = super::holder::Spawn {
            exe,
            args: vec!["broker".into(), "holder".into(), "--holder-pipe".into(), a.holder_pipe.clone()],
            env: Vec::new(),
        };
        let p = platform.with_holder(&a.holder_pipe, spawn).map_err(|e| format!("holder {}: {e}", a.holder_pipe))?;
        log(&format!("holder: process {}", p.holder_pid().unwrap_or(0)));
        p
    } else {
        log("--no-holder: a crash of the broker while a provider is down frees that provider's regions");
        platform
    };
    log(&format!(
        "large pages: {}",
        if platform.large_pages() { "SeLockMemoryPrivilege held" } else { "not available (alloc ... large is refused)" }
    ));
    let run = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
        ^ (u64::from(std::process::id()) << 40);
    let broker = Broker::new(
        platform,
        Config {
            devices,
            services: Vec::new(),
            keys: Some(a.keys.clone()),
            release: Release::OwnerFree,
            budget: a.budget,
            run,
        },
    );
    broker.serve().map_err(|e| e.to_string())
}

static ARGS: OnceLock<Args> = OnceLock::new();
static STATUS: AtomicUsize = AtomicUsize::new(0);
static STOP: AtomicUsize = AtomicUsize::new(0);

fn set_status(state: SERVICE_STATUS_CURRENT_STATE, exit: u32) {
    let h = STATUS.load(Ordering::Acquire) as SERVICE_STATUS_HANDLE;
    if h.is_null() {
        return;
    }
    let s = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        dwWin32ExitCode: exit,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING { 5000 } else { 0 },
    };
    // SAFETY: the handle the SCM gave this service, and a live status.
    unsafe { SetServiceStatus(h, &s) };
}

unsafe extern "system" fn handler(ctrl: u32, _t: u32, _d: *mut core::ffi::c_void, _c: *mut core::ffi::c_void) -> u32 {
    match ctrl {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_status(SERVICE_STOP_PENDING, 0);
            // SAFETY: the stop event made before the handler was registered.
            unsafe { SetEvent(STOP.load(Ordering::Acquire) as _) };
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let name = wide(SERVICE_NAME);
    // SAFETY: a live name and a handler with the right signature.
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), std::ptr::null()) };
    if h.is_null() {
        return;
    }
    STATUS.store(h as usize, Ordering::Release);
    set_status(SERVICE_START_PENDING, 0);
    let Some(a) = ARGS.get().cloned() else { return };
    // A service has no console: its log is a file beside its state.
    redirect_stderr(&a.state.join("broker.log"));
    let _ = std::thread::Builder::new().name("wc-broker".into()).spawn(move || {
        if let Err(e) = serve(&a) {
            log(&format!("FAILED: {e}"));
            // Not a clean stop: the service manager's recovery restarts it.
            std::process::exit(1);
        }
    });
    set_status(SERVICE_RUNNING, 0);
    // SAFETY: the stop event; waited for until the SCM asks to stop.
    unsafe { WaitForSingleObject(STOP.load(Ordering::Acquire) as _, INFINITE) };
    log("stopping: regions the broker held are freed with this process; lent devices stay with their holders");
    set_status(SERVICE_STOPPED, 0);
}

fn redirect_stderr(path: &std::path::Path) {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    // Kept short: rotated by size at start.
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 8 << 20) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        // SAFETY: a handle this process owns from here on, for its life.
        unsafe { SetStdHandle(STD_ERROR_HANDLE, f.into_raw_handle() as _) };
    }
}

/// `sv-guest broker ARGS`.
pub fn main(args: &[String]) -> ExitCode {
    let a = match Args::parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sv-guest broker: {e}");
            return ExitCode::from(2);
        }
    };
    match a.action.as_deref() {
        Some("ready") => {
            let ready = || -> Result<(), String> {
                // Wait for the authenticated service to finish its startup
                // verification before opening exclusive ports for a second
                // independent check. Do not race its own ACL operations.
                super::readiness::installed(&a.pipe, || {
                    acl::verify_raw_protection(&a.devices).map_err(std::io::Error::other)
                }).map_err(|e| e.to_string())?;
                Ok(())
            };
            return match ready() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => { eprintln!("broker not ready: {e}"); ExitCode::from(1) }
            };
        }
        Some("show") => {
            for l in acl::show(&a.devices) {
                println!("{l}");
            }
            return ExitCode::SUCCESS;
        }
        Some("holder") => {
            return match super::holder::serve(&a.holder_pipe, super::holder::HOLDER_SDDL) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("sv-guest broker holder: {e}");
                    ExitCode::from(1)
                }
            };
        }
        Some("holder-quit") => {
            return match super::holder::quit(&a.holder_pipe) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("sv-guest broker holder-quit: {e}");
                    ExitCode::from(1)
                }
            };
        }
        Some("revert-acl-check") => {
            return match acl::revert_check(&a.state) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => { eprintln!("{e}"); ExitCode::from(1) }
            };
        }
        Some("revert-acl") => {
            if let Err(e) = acl::revert_check(&a.state) {
                eprintln!("{e}");
                return ExitCode::from(1);
            }
            for l in acl::revert(&a.state) {
                println!("{l}");
            }
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    if a.console {
        return match serve(&a) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                log(&format!("FAILED: {e}"));
                ExitCode::from(1)
            }
        };
    }
    let ev = match super::pipe::Event::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("sv-guest broker: {e}");
            return ExitCode::from(1);
        }
    };
    STOP.store(ev.0 as usize, Ordering::Release);
    let _ = ARGS.set(a);
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
    ];
    // SAFETY: a live, terminated table; returns when the service has stopped.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        let e = std::io::Error::last_os_error();
        eprintln!(
            "sv-guest broker: not started by the service manager ({e}); `sc.exe start {SERVICE_NAME}`, or --console to run here"
        );
        return ExitCode::from(1);
    }
    drop(ev);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_parse_and_default_to_the_four_devices() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let a = Args::parse(&s(&["--console"])).unwrap();
        assert!(a.console && a.acl);
        assert_eq!(a.devices.len(), 4);
        assert_eq!(a.pipe, r"\\.\pipe\waycompose-broker");
        let a = Args::parse(&s(&["--console", "--no-acl", "--device", r"arena=path:\\.\pipe\x", "--pipe", r"\\.\pipe\t"])).unwrap();
        assert_eq!(a.devices.len(), 1);
        assert_eq!(a.devices[0].loc, DeviceLoc::Path(r"\\.\pipe\x".into()));
        assert!(!a.acl);
        assert_eq!(Args::parse(&s(&["show"])).unwrap().action.as_deref(), Some("show"));
        assert!(Args::parse(&s(&["--device", "arena"])).is_err());
        assert!(Args::parse(&s(&["--fly"])).is_err());
    }
}
