//! The devices' own security (§111.8), which replaces
//! `scripts/wc-port-access.ps1`'s per-user grant (§91, R32).
//!
//! With the broker, no user process opens a device: the broker (SYSTEM)
//! opens it and moves the handle. So each of the four devices gets the
//! descriptor that admits SYSTEM and Administrators only,
//! [`DEVICE_SDDL`], and a user's direct open is refused by the kernel. It is
//! set in the two places the script set its grant, for the same reasons:
//!
//! * the device's PnP security property (`SPDRP_SECURITY_SDS`), which the PnP
//!   manager applies at every start of the device, with nothing of ours
//!   running;
//! * the live device object (`SetSecurityInfo`), so it holds now. A
//!   virtio-serial port is exclusive, so a held port cannot be opened even to
//!   change its descriptor; it is left as it is and logged, and its property
//!   takes over at its next start.
//!
//! The ports are found by name at every start (`\\.\Global\NAME` is a link
//! to the port's device object, whose PnP device reports that object's
//! name), so a port QEMU renumbered still gets it. What was found is recorded
//! in the state directory, with the descriptor each device had before, for
//! [`revert`] and for re-enabling a port that was left disabled (§99.6).

use super::ident::Owned;
use super::pipe::wide;
use super::{DeviceAt, DeviceLoc};
use std::io;
use std::path::Path;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Enable_DevNode, CM_Get_DevNode_Status, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CM_PROB_DISABLED,
    CR_SUCCESS, DIGCF_ALLCLASSES, DIGCF_PRESENT, HDEVINFO, SP_DEVINFO_DATA, SPDRP_PHYSICAL_DEVICE_OBJECT_NAME,
    SPDRP_SECURITY, SPDRP_SECURITY_SDS, SetupDiCreateDeviceInfoList, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDeviceInfoW,
    SetupDiSetDeviceRegistryPropertyW,
};
use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorControl, PROTECTED_DACL_SECURITY_INFORMATION,
    UNPROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    OWNER_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, SACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, QueryDosDeviceW};

/// SYSTEM and Administrators, full access; nobody else.
pub const DEVICE_SDDL: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)";

const READ_CONTROL: u32 = 0x0002_0000;
const WRITE_DAC: u32 = 0x0004_0000;

/// One of our devices as found on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    /// The PnP device instance id.
    pub instance: String,
    /// The path its device object is opened by.
    pub live: String,
}

/// Find `d` on this machine. `Ok(None)`: not present (or a stand-in path,
/// which is not a device of ours to secure).
pub fn find(d: &DeviceAt) -> io::Result<Option<Found>> {
    match &d.loc {
        DeviceLoc::IvshmemSlot(s) => {
            let paths = wc_handle::ivshmem_device_paths()?;
            let Some(p) = wc_handle::ivshmem::path_for_slot(&paths, *s) else { return Ok(None) };
            let instance = crate::sys::instance_from_interface(&p)
                .ok_or_else(|| io::Error::other(format!("{p}: no instance id in this interface path")))?;
            Ok(Some(Found { name: d.name.clone(), instance, live: p }))
        }
        DeviceLoc::Port(n) => Ok(port_instance(n)?.map(|instance| Found {
            name: d.name.clone(),
            instance,
            live: format!(r"\\.\Global\{n}"),
        })),
        DeviceLoc::Path(_) => Ok(None),
    }
}

/// The device object name `\\.\Global\NAME` links to (`\Device\000000NN`).
fn dos_target(name: &str) -> Option<String> {
    let w = wide(&format!("Global\\{name}"));
    let mut buf = vec![0u16; 1024];
    // SAFETY: a live name and buffer of the length given.
    let n = unsafe { QueryDosDeviceW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 {
        return None;
    }
    let end = buf.iter().position(|c| *c == 0).unwrap_or(n as usize);
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// A device-info set, destroyed on drop.
struct Set(HDEVINFO);

impl Drop for Set {
    fn drop(&mut self) {
        // SAFETY: owned, destroyed once.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

fn devinfo() -> SP_DEVINFO_DATA {
    SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() }
}

fn string_property(set: HDEVINFO, data: &SP_DEVINFO_DATA, prop: u32) -> Option<String> {
    let mut buf = vec![0u16; 2048];
    let mut need = 0u32;
    // SAFETY: a live set, record and buffer of the size given.
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(set, data, prop, std::ptr::null_mut(), buf.as_mut_ptr().cast(), (buf.len() * 2) as u32, &mut need)
    };
    if ok == 0 {
        return None;
    }
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

fn instance_id(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    // SAFETY: as above.
    if unsafe { SetupDiGetDeviceInstanceIdW(set, data, buf.as_mut_ptr(), buf.len() as u32, std::ptr::null_mut()) } == 0 {
        return None;
    }
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// The instance id of the virtio-serial port named `name`: the one present
/// `VIOSERIALPORT` device whose device object is the link's target.
pub fn port_instance(name: &str) -> io::Result<Option<String>> {
    let Some(target) = dos_target(name) else { return Ok(None) };
    // SAFETY: every class, present devices only.
    let set = unsafe { SetupDiGetClassDevsW(std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), DIGCF_ALLCLASSES | DIGCF_PRESENT) };
    if set == INVALID_HANDLE_VALUE as HDEVINFO {
        return Err(io::Error::last_os_error());
    }
    let set = Set(set);
    for i in 0.. {
        let mut d = devinfo();
        // SAFETY: a live set and record.
        if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut d) } == 0 {
            break;
        }
        let Some(id) = instance_id(set.0, &d) else { continue };
        if !id.to_ascii_uppercase().contains("VIOSERIALPORT") {
            continue;
        }
        if string_property(set.0, &d, SPDRP_PHYSICAL_DEVICE_OBJECT_NAME).as_deref() == Some(target.as_str()) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

fn open_instance(instance: &str) -> Result<(Set, SP_DEVINFO_DATA), String> {
    // SAFETY: an empty set of no class.
    let set = Set(unsafe { SetupDiCreateDeviceInfoList(std::ptr::null(), std::ptr::null_mut()) });
    let mut d = devinfo();
    let w = wide(instance);
    // SAFETY: a live set, id and record.
    if unsafe { SetupDiOpenDeviceInfoW(set.0, w.as_ptr(), std::ptr::null_mut(), 0, &mut d) } == 0 {
        return Err(format!("SetupDiOpenDeviceInfo({instance}): {}", io::Error::last_os_error()));
    }
    Ok((set, d))
}

/// The device's PnP security property, as SDDL; `None` when it has none.
pub fn property(instance: &str) -> Result<Option<String>, String> {
    let (set, d) = open_instance(instance)?;
    let mut buf = vec![0u8; 16384];
    let mut needed = 0;
    // SPDRP_SECURITY_SDS is a write-only SDDL property. Read its binary form.
    // SAFETY: a live record and writable buffer with its byte length.
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(set.0, &d, SPDRP_SECURITY, std::ptr::null_mut(),
            buf.as_mut_ptr(), buf.len() as u32, &mut needed)
    };
    if ok == 0 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(13) { Ok(None) } else { Err(format!("read PnP security: {e}")) };
    }
    let mut s: *mut u16 = std::ptr::null_mut();
    let fields = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION
        | SACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
    // SAFETY: SetupAPI returned a security descriptor; capture all its fields, including the mandatory label.
    if unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(buf.as_mut_ptr().cast(), 1, fields, &mut s, std::ptr::null_mut()) } == 0 {
        return Err(format!("convert PnP security: {}", io::Error::last_os_error()));
    }
    let mut n = 0;
    // SAFETY: a NUL-terminated string allocated by the conversion above.
    while unsafe { *s.add(n) } != 0 { n += 1; }
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) });
    unsafe { LocalFree(s.cast()) };
    Ok(Some(text))
}

/// Write the PnP security property (`Some`) or remove it (`None`).
pub fn set_property(instance: &str, sddl: Option<&str>) -> Result<(), String> {
    let (set, mut d) = open_instance(instance)?;
    let ok = match sddl {
        Some(s) => {
            let w = wide(s);
            // SAFETY: a live set, record and NUL-terminated string of the byte length given.
            unsafe { SetupDiSetDeviceRegistryPropertyW(set.0, &mut d, SPDRP_SECURITY_SDS, w.as_ptr().cast(), (w.len() * 2) as u32) }
        }
        // SAFETY: as above; no buffer removes the property.
        None => unsafe { SetupDiSetDeviceRegistryPropertyW(set.0, &mut d, SPDRP_SECURITY, std::ptr::null(), 0) },
    };
    if ok == 0 {
        let e = io::Error::last_os_error();
        // ERROR_INVALID_DATA: there was none to remove.
        if sddl.is_none() && e.raw_os_error() == Some(13) {
            return Ok(());
        }
        return Err(format!("SetupDiSetDeviceRegistryProperty({instance}): {e}"));
    }
    Ok(())
}

fn open_object(path: &str, access: u32) -> Result<Owned, String> {
    let w = wide(path);
    // SAFETY: a live path; no attributes, no template.
    let h = unsafe {
        CreateFileW(w.as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
    };
    if h == INVALID_HANDLE_VALUE {
        let e = io::Error::last_os_error();
        let held = if e.raw_os_error() == Some(5) { " (a port has one opener: someone holds it)" } else { "" };
        return Err(format!("not opened: {e}{held}"));
    }
    Ok(Owned(h))
}

/// The live DACL of the device object at `path`, as SDDL.
pub fn live(path: &str) -> Result<String, String> {
    let h = open_object(path, READ_CONTROL)?;
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a live handle with READ_CONTROL and a live out-parameter.
    let r = unsafe {
        GetSecurityInfo(h.0, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut sd)
    };
    if r != 0 {
        return Err(format!("GetSecurityInfo: {}", io::Error::from_raw_os_error(r as i32)));
    }
    let mut s: *mut u16 = std::ptr::null_mut();
    // SAFETY: the descriptor just read.
    let ok = unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, 1, DACL_SECURITY_INFORMATION, &mut s, std::ptr::null_mut()) };
    // SAFETY: allocated by GetSecurityInfo.
    unsafe { LocalFree(sd) };
    if ok == 0 {
        return Err("unconvertible descriptor".into());
    }
    let mut n = 0;
    // SAFETY: a NUL-terminated string the call allocated.
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `n` u16s just counted; then freed once.
    let t = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) });
    unsafe { LocalFree(s.cast()) };
    Ok(t)
}

/// Replace the live DACL and preserve the supplied protection flag.
pub fn set_live(path: &str, sddl: &str) -> Result<(), String> {
    let sd = super::pipe::Sd::from_sddl(sddl).map_err(|e| e.to_string())?;
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl = std::ptr::null_mut();
    // SAFETY: the descriptor just made and live out-parameters.
    unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) };
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: a valid security descriptor and writable output parameters.
    if unsafe { GetSecurityDescriptorControl(sd.0, &mut control, &mut revision) } == 0 {
        return Err(format!("GetSecurityDescriptorControl: {}", io::Error::last_os_error()));
    }
    let protection = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    let h = open_object(path, READ_CONTROL | WRITE_DAC)?;
    // SAFETY: a live handle with WRITE_DAC; the DACL lives in `sd`.
    let r = unsafe {
        SetSecurityInfo(h.0, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION | protection, std::ptr::null_mut(), std::ptr::null_mut(), dacl, std::ptr::null())
    };
    if r != 0 {
        return Err(format!("SetSecurityInfo: {}", io::Error::from_raw_os_error(r as i32)));
    }
    Ok(())
}

/// Re-enable device `instance` if it is disabled (§99.6: a port left
/// disabled pending a reboot would come up as no doorbell). Ok(true): it was.
pub fn enable_if_disabled(instance: &str) -> Result<bool, String> {
    let w = wide(instance);
    let mut dn = 0u32;
    // SAFETY: a live id and out-parameter.
    if unsafe { CM_Locate_DevNodeW(&mut dn, w.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) } != CR_SUCCESS {
        return Err(format!("{instance}: not present"));
    }
    let (mut status, mut problem) = (0, 0);
    // SAFETY: a live devnode and out-parameters.
    if unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, dn, 0) } != CR_SUCCESS {
        return Ok(false);
    }
    if problem != CM_PROB_DISABLED {
        return Ok(false);
    }
    // SAFETY: a live devnode.
    let r = unsafe { CM_Enable_DevNode(dn, 0) };
    if r != CR_SUCCESS {
        return Err(format!("{instance}: CM_Enable_DevNode answered {r}"));
    }
    Ok(true)
}

/// What [`secure`] remembers per device: `NAME\tINSTANCE\tLIVE\tORIGINAL\tPNP`.
fn state_file(dir: &Path) -> std::path::PathBuf {
    dir.join("devices.txt")
}

type SavedDevice = (String, String, String, String, Option<String>);

fn read_state(dir: &Path) -> io::Result<Vec<SavedDevice>> {
    let text = match std::fs::read_to_string(state_file(dir)) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    text.lines().map(|line| {
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 5 || fields[..4].iter().any(|f| f.is_empty()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid saved device descriptor"));
        }
        Ok((fields[0].into(), fields[1].into(), fields[2].into(), fields[3].into(),
            fields.get(4).filter(|s| !s.is_empty()).map(|s| (*s).into())))
    }).collect()
}

fn write_state(dir: &Path, rows: &[SavedDevice]) -> io::Result<()> {
    let text: String = rows.iter().map(|(a, b, c, d, e)| format!("{a}\t{b}\t{c}\t{d}\t{}\n", e.as_deref().unwrap_or(""))).collect();
    std::fs::create_dir_all(dir)?;
    let pending = dir.join("devices.txt.pending");
    use std::io::Write;
    let mut file = std::fs::File::create(&pending)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(pending, state_file(dir))?;
    std::fs::OpenOptions::new().write(true).open(state_file(dir))?.sync_all()?;
    let checkpoint = dir.join("devices.secured.pending");
    let mut file = std::fs::File::create(&checkpoint)?;
    writeln!(file, "{}", rows.len())?;
    file.sync_all()?;
    std::fs::rename(checkpoint, dir.join("devices.secured"))?;
    std::fs::OpenOptions::new().write(true).open(dir.join("devices.secured"))?.sync_all()
}

fn check_baseline(state: &Path) -> Result<Vec<SavedDevice>, String> {
    if !state_file(state).is_file() {
        return Err("missing device baseline; trusted recovery required".into());
    }
    let rows = read_state(state).map_err(|e| format!("cannot read baseline: {e}"))?;
    match std::fs::read_to_string(state.join("devices.secured")) {
        Ok(count) if count.trim().parse::<usize>().ok() == Some(rows.len()) => {},
        Ok(_) => return Err("incomplete device baseline/checkpoint; trusted recovery required".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}, // retained pre-checkpoint baseline
        Err(e) => return Err(format!("unreadable device baseline checkpoint: {e}")),
    }
    Ok(rows)
}

/// At the broker's start: every device of ours gets [`DEVICE_SDDL`], as PnP
/// property and live, a recorded port that is disabled is enabled again, and
/// each device's descriptor from before the first run is kept for
/// [`revert`]. Returns one line per device for the log.
pub fn secure(devices: &[DeviceAt], state: &Path) -> Vec<String> {
    if !state_file(state).exists() {
        if state.join("devices.secured").exists() || state.join("pre-broker/complete").exists() {
            return vec!["devices not secured: lost baseline; trusted recovery required".into()];
        }
        // Explicitly record a fresh no-device state too. A missing snapshot
        // after any mutation must never masquerade as a fresh installation.
        if let Err(e) = write_state(state, &[]) {
            return vec![format!("devices not secured: cannot establish baseline: {e}")];
        }
    }
    let mut rows = match check_baseline(state) {
        Ok(rows) => rows,
        Err(e) => return vec![format!("devices not secured: {e}")],
    };
    if let Err(e) = write_state(state, &rows) {
        return vec![format!("devices not secured: cannot checkpoint baseline: {e}")];
    }
    let mut out = Vec::new();
    for (name, instance, _, _, _) in &rows {
        if devices.iter().any(|d| d.name == *name && matches!(d.loc, DeviceLoc::Port(_))) {
            match enable_if_disabled(instance) {
                Ok(true) => out.push(format!("{name}: was disabled ({instance}); enabled again")),
                Ok(false) => {}
                Err(e) => out.push(format!("{name}: {e}")),
            }
        }
    }
    for d in devices {
        let f = match find(d) {
            Ok(Some(f)) => f,
            Ok(None) => {
                if !matches!(d.loc, DeviceLoc::Path(_)) {
                    out.push(format!("{}: not on this machine; nothing to secure", d.name));
                }
                continue;
            }
            Err(e) => {
                out.push(format!("{}: could not be looked up: {e}", d.name));
                continue;
            }
        };
        if !rows.iter().any(|r| r.0 == f.name && r.1 == f.instance) {
            // Save both originals before changing either descriptor.
            let original = match live(&f.live) {
                Ok(s) => s,
                Err(e) => { out.push(format!("{}: not secured: cannot record live descriptor: {e}", f.name)); continue; }
            };
            let original_property = match property(&f.instance) {
                Ok(s) => s,
                Err(e) => { out.push(format!("{}: not secured: cannot record PnP property: {e}", f.name)); continue; }
            };
            rows.retain(|r| r.0 != f.name);
            rows.push((f.name.clone(), f.instance.clone(), f.live.clone(), original, original_property));
            if let Err(e) = write_state(state, &rows) {
                out.push(format!("{}: not secured: cannot save descriptors: {e}", f.name));
                rows.pop();
                continue;
            }
        }
        let prop = set_property(&f.instance, Some(DEVICE_SDDL));
        let now = set_live(&f.live, DEVICE_SDDL);
        out.push(format!(
            "{} ({}): PnP property {}; live {}",
            f.name,
            f.instance,
            prop.map_or_else(|e| format!("NOT written: {e}"), |()| DEVICE_SDDL.to_string()),
            now.map_or_else(|e| format!("left as it is ({e}); the property applies at its next start"), |()| DEVICE_SDDL.to_string()),
        ));
    }
    out
}

/// Refuse rollback that would expose uncleared physical memory through a
/// direct device open. A persisted SID binding cannot police a reverted
/// kernel DACL. There is no independently attested drain/reset boundary yet.
/// Verify every present raw device has the exact protected descriptor in both its live object and PnP property. Missing devices are optional and skipped.
pub fn verify_raw_protection(devices: &[DeviceAt]) -> Result<(), String> {
    verify_raw_protection_with(devices, find)
}

fn verify_raw_protection_with(devices: &[DeviceAt], mut lookup: impl FnMut(&DeviceAt) -> io::Result<Option<Found>>) -> Result<(), String> {
    for d in devices {
        if !matches!(d.loc, DeviceLoc::IvshmemSlot(_)) { continue; }
        let Some(found) = lookup(d).map_err(|e| format!("{}: lookup failed: {e}", d.name))? else { continue; };
        if live(&found.live).map_err(|e| format!("{}: live ACL unreadable: {e}", d.name))? != DEVICE_SDDL { return Err(format!("{}: live ACL is not protected", d.name)); }
        if property(&found.instance).map_err(|e| format!("{}: PnP ACL unreadable: {e}", d.name))?.as_deref() != Some(DEVICE_SDDL) { return Err(format!("{}: PnP ACL is not protected", d.name)); }
    }
    Ok(())
}

pub fn revert_check(state: &Path) -> Result<(), String> {
    let rows = check_baseline(state)?;
    for (_, instance, path, original, property) in rows {
        let raw = instance.to_ascii_lowercase().starts_with(r"pci\ven_1af4&dev_1110")
            || path.to_ascii_lowercase().contains("df576976-569d-4672-95a0-f57e4ea0b210");
        if raw && (original != DEVICE_SDDL || property.as_deref() != Some(DEVICE_SDDL)) {
            return Err(format!("raw device {instance}: unsafe ACL restoration refused; no proven clean/drained device boundary"));
        }
    }
    Ok(())
}

/// Undo [`secure`]: each recorded device gets its descriptor from before
/// back, live and in its PnP property. Failed restorations stay recorded.
/// Legacy records without a PnP original are refused, never guessed.
pub fn revert(state: &Path) -> Vec<String> {
    if let Err(e) = revert_check(state) {
        return vec![format!("devices not restored: {e}")];
    }
    let rows = match read_state(state) {
        Ok(rows) => rows,
        Err(e) => return vec![format!("devices not restored: cannot read baseline: {e}")],
    };
    let mut out = Vec::new();
    let mut failed = Vec::new();
    for (name, instance, path, original, original_property) in &rows {
        let prop = set_property(instance, original_property.as_deref());
        let now = if original.is_empty() { Err("no descriptor recorded".to_string()) } else { set_live(path, original) };
        if prop.is_err() || now.is_err() {
            failed.push((name.clone(), instance.clone(), path.clone(), original.clone(), original_property.clone()));
        }
        out.push(format!(
            "{name} ({instance}): PnP property {}; live {}",
            prop.map_or_else(|e| format!("NOT restored: {e}"), |()| "restored".into()),
            now.map_or_else(|e| format!("not restored ({e})"), |()| format!("restored to {original}")),
        ));
    }
    if failed.is_empty() {
        let _ = std::fs::remove_file(state_file(state));
        let _ = std::fs::remove_file(state.join("devices.secured"));
    } else if let Err(e) = write_state(state, &failed) {
        out.push(format!("could not save pending restoration: {e}"));
    }
    out
}

/// One line per device: where it is and what its descriptors say.
pub fn show(devices: &[DeviceAt]) -> Vec<String> {
    devices
        .iter()
        .map(|d| match find(d) {
            Ok(Some(f)) => format!(
                "{} -> {} ({}): live {}; PnP property {}",
                f.name,
                f.live,
                f.instance,
                live(&f.live).unwrap_or_else(|e| format!("unreadable ({e})")),
                property(&f.instance).map_or_else(|e| e, |p| p.unwrap_or_else(|| "none".into()))
            ),
            Ok(None) => format!("{}: not on this machine", d.name),
            Err(e) => format!("{}: {e}", d.name),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_optional_raw_devices_allow_startup_but_lookup_failure_refuses_it() {
        let devices = vec![DeviceAt { name: "optional".into(), loc: DeviceLoc::IvshmemSlot(0) }];
        assert!(verify_raw_protection_with(&devices, |_| Ok(None)).is_ok());
        assert!(verify_raw_protection_with(&devices, |_| Err(io::Error::other("injected lookup failure")))
            .unwrap_err().contains("injected lookup failure"));
    }

    #[test]
    fn completed_empty_revert_allows_fresh_install() {
        let dir = std::env::temp_dir().join(format!("wc-reinstall-{}", std::process::id()));
        write_state(&dir, &[]).unwrap();
        assert!(revert(&dir).is_empty());
        assert!(!state_file(&dir).exists());
        assert!(!dir.join("devices.secured").exists());
        assert!(secure(&[], &dir).is_empty());
        assert!(check_baseline(&dir).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lost_baseline_never_becomes_fresh_but_explicit_fresh_empty_is_recorded() {
        let dir = std::env::temp_dir().join(format!("wc-lost-baseline-{}", std::process::id()));
        assert!(revert_check(&dir).is_err());
        assert!(secure(&[], &dir).is_empty());
        assert!(revert_check(&dir).is_ok());
        let rows = vec![("port".into(), "instance".into(), "path".into(), DEVICE_SDDL.into(), Some(DEVICE_SDDL.into()))];
        write_state(&dir, &rows).unwrap();
        std::fs::write(state_file(&dir), "").unwrap();
        assert!(revert_check(&dir).unwrap_err().contains("incomplete"));
        std::fs::remove_file(state_file(&dir)).unwrap();
        assert!(revert_check(&dir).unwrap_err().contains("missing"));
        assert!(secure(&[], &dir)[0].contains("lost baseline"));
        assert!(!state_file(&dir).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn permissive_raw_rollback_is_refused_before_any_mutation() {
        let dir = std::env::temp_dir().join(format!("wc-raw-rollback-{}", std::process::id()));
        let id = r"PCI\VEN_1AF4&DEV_1110&SUBSYS_11001AF4\PRIVATE-TEST";
        let rows = vec![("renamed".into(), id.into(), "unused".into(),
            "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GR;;;WD)".into(), Some(DEVICE_SDDL.into()))];
        write_state(&dir, &rows).unwrap();
        let before = std::fs::read(state_file(&dir)).unwrap();
        assert!(revert_check(&dir).unwrap_err().contains("unsafe ACL restoration"));
        assert!(revert(&dir)[0].contains("unsafe ACL restoration"));
        assert_eq!(std::fs::read(state_file(&dir)).unwrap(), before);
        assert!(dir.join("devices.secured").exists());
        let safe = vec![("renamed".into(), id.into(), "unused".into(), DEVICE_SDDL.into(), Some(DEVICE_SDDL.into()))];
        write_state(&dir, &safe).unwrap();
        assert!(revert_check(&dir).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saved_descriptors_keep_property_and_reject_corruption() {
        let dir = std::env::temp_dir().join(format!("wc-acl-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rows = vec![
            ("port".into(), "instance".into(), "live".into(), DEVICE_SDDL.into(), Some("D:P(A;;FR;;;WD)".into())),
            ("arena".into(), "pci".into(), "bar".into(), DEVICE_SDDL.into(), None),
        ];
        write_state(&dir, &rows).unwrap();
        assert_eq!(read_state(&dir).unwrap(), rows);
        // A legacy baseline cannot claim an exact restoration without its PnP original.
        std::fs::write(state_file(&dir), "port\tinstance\tlive\tD:P(A;;FA;;;SY)\n").unwrap();
        assert!(read_state(&dir).is_err());
        std::fs::write(state_file(&dir), "port\tinstance\tlive\t\n").unwrap();
        assert!(read_state(&dir).is_err());
        assert!(revert(&dir)[0].contains("cannot read baseline"));
        assert!(state_file(&dir).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
