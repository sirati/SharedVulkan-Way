//! Protected NTFS storage rooted in retained directory/volume handles.
//!
//! Namespace replacement uses an NT relative rename and then a whole-volume
//! FlushFileBuffers barrier. File-only flushing is not claimed to persist a
//! rename. Unsupported volume/filesystem/flush operations fail closed.
use super::{DurableStore, MAX, invalid};
use crate::sys::windows::{
    ident::Owned,
    pipe::{Sd, wide},
};
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FlushFileBuffers, GetFileInformationByHandle,
    GetFinalPathNameByHandleW, GetVolumeInformationByHandleW, OPEN_EXISTING, VOLUME_NAME_GUID,
};

const SECURITY: &str = "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)";
const READ_CONTROL: u32 = 0x0002_0000;
const DELETE: u32 = 0x0001_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
#[repr(C)]
struct Unicode {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct Attributes {
    length: u32,
    root: HANDLE,
    name: *mut Unicode,
    flags: u32,
    security: *mut c_void,
    qos: *mut c_void,
}
#[repr(C)]
struct Status {
    status: usize,
    information: usize,
}
#[repr(C)]
struct Rename {
    replace: u8,
    root: HANDLE,
    name_length: u32,
    name: [u16; 5],
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut HANDLE,
        access: u32,
        attributes: *mut Attributes,
        status: *mut Status,
        allocation: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_length: u32,
    ) -> i32;
    fn NtSetInformationFile(
        handle: HANDLE,
        status: *mut Status,
        info: *const c_void,
        length: u32,
        class: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
fn nt(result: i32) -> io::Result<()> {
    if result < 0 {
        // SAFETY: pure NTSTATUS-to-Win32 conversion.
        Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ))
    } else {
        Ok(())
    }
}

pub struct DirectoryStore {
    directory: Owned,
    volume: Owned,
    _lock: File,
    known_state: bool,
    #[cfg(test)]
    fault: u8,
}
impl DirectoryStore {
    /// Open an existing directory with exact protected Admin/SYSTEM ownership
    /// and DACL. Neither the directory nor missing state is created here.
    ///
    /// # Safety
    /// The privileged canonical original-backing owner must select its one
    /// trusted state directory, through protected ancestors. Its full lifetime
    /// must exclude restoration of old state images or alternate directories.
    /// The caller must run as SYSTEM or an elevated administrator; this backend
    /// requires volume-flush authority and never weakens ACLs to obtain it.
    pub unsafe fn open(path: &Path) -> io::Result<Self> {
        let path = wide(
            path.to_str()
                .ok_or_else(|| invalid("registry path is not Unicode"))?,
        );
        // SAFETY: terminated path, no template/security pointer, synchronous handle.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let directory = Owned(handle);
        check(directory.0, true)?;
        let volume = volume_for(directory.0)?;
        let lock = child(directory.0, "lock", true, 3)?; // FILE_OPEN_IF, sharing denied
        check(lock.as_raw_handle(), false)?;
        Ok(Self {
            directory,
            volume,
            _lock: lock,
            known_state: false,
            #[cfg(test)]
            fault: 0,
        })
    }
}
fn child(directory: HANDLE, name: &str, writable: bool, disposition: u32) -> io::Result<File> {
    let mut name: Vec<u16> = name.encode_utf16().collect();
    let mut unicode = Unicode {
        length: (name.len() * 2) as u16,
        maximum_length: (name.len() * 2) as u16,
        buffer: name.as_mut_ptr(),
    };
    let security = Sd::from_sddl(SECURITY)?;
    let mut attributes = Attributes {
        length: std::mem::size_of::<Attributes>() as u32,
        root: directory,
        name: &mut unicode,
        flags: 0x40,
        security: security.0,
        qos: std::ptr::null_mut(),
    };
    let mut status = Status {
        status: 0,
        information: 0,
    };
    let mut handle = std::ptr::null_mut();
    let access = GENERIC_READ
        | READ_CONTROL
        | SYNCHRONIZE
        | if writable { GENERIC_WRITE | DELETE } else { 0 };
    // FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE |
    // FILE_OPEN_REPARSE_POINT; writes additionally require FILE_WRITE_THROUGH.
    let options = 0x20 | 0x40 | 0x0020_0000 | if writable { 2 } else { 0 };
    // SAFETY: fixed one-component child name, retained directory and live ABI
    // structs; OBJ_CASE_INSENSITIVE, no user-controlled path traversal.
    nt(unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &mut attributes,
            &mut status,
            std::ptr::null(),
            0x80,
            0,
            disposition,
            options,
            std::ptr::null(),
            0,
        )
    })?;
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(invalid("invalid registry child handle"));
    }
    // SAFETY: newly owned synchronous NT file handle, closed exactly once.
    Ok(unsafe { File::from_raw_handle(handle) })
}
fn information(handle: HANDLE) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = unsafe { std::mem::zeroed() };
    // SAFETY: live handle and output buffer.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}
fn check(handle: HANDLE, directory: bool) -> io::Result<()> {
    let info = information(handle)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || (!directory && info.nNumberOfLinks != 1)
    {
        return Err(invalid(
            "registry object is reparse, wrong type or multiply linked",
        ));
    }
    let mut sd = std::ptr::null_mut();
    let fields = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    // SAFETY: retained READ_CONTROL handle and output descriptor pointer.
    let result = unsafe {
        GetSecurityInfo(
            handle,
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
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let mut text = std::ptr::null_mut();
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            1,
            fields,
            &mut text,
            std::ptr::null_mut(),
        )
    };
    unsafe { LocalFree(sd) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    let descriptor = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    unsafe { LocalFree(text.cast()) };
    if ![
        SECURITY,
        "O:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)",
        "O:BAD:PAI(A;;FA;;;SY)(A;;FA;;;BA)",
        "O:SYD:PAI(A;;FA;;;SY)(A;;FA;;;BA)",
    ]
    .contains(&descriptor.as_str())
    {
        return Err(invalid("untrusted registry object owner/DACL"));
    }
    Ok(())
}
fn volume_info(handle: HANDLE) -> io::Result<(u32, String)> {
    let mut serial = 0;
    let mut filesystem = [0u16; 64];
    if unsafe {
        GetVolumeInformationByHandleW(
            handle,
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let len = filesystem
        .iter()
        .position(|n| *n == 0)
        .ok_or_else(|| invalid("filesystem name too long"))?;
    Ok((serial, String::from_utf16_lossy(&filesystem[..len])))
}
fn volume_for(directory: HANDLE) -> io::Result<Owned> {
    let mut path = vec![0u16; 32768];
    let len = unsafe {
        GetFinalPathNameByHandleW(
            directory,
            path.as_mut_ptr(),
            path.len() as u32,
            VOLUME_NAME_GUID,
        )
    };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize >= path.len() {
        return Err(invalid("registry volume path too long"));
    }
    let text = String::from_utf16_lossy(&path[..len as usize]);
    let prefix = r"\\?\Volume{";
    if !text.starts_with(prefix) {
        return Err(invalid("registry is not on a local GUID volume"));
    }
    let end = text
        .find("}\\")
        .ok_or_else(|| invalid("invalid registry volume GUID path"))?
        + 1;
    let name = wide(&text[..end]);
    // A volume GUID without trailing slash opens the actual volume object,
    // not an arbitrary file that merely lives on that volume.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let volume = Owned(handle);
    let expected = volume_info(directory)?;
    if expected.1 != "NTFS" || volume_info(volume.0)? != expected {
        return Err(invalid("registry volume is not the retained NTFS volume"));
    }
    flush(volume.0)?;
    Ok(volume)
}
fn flush(handle: HANDLE) -> io::Result<()> {
    if unsafe { FlushFileBuffers(handle) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn remove_pending(directory: HANDLE) -> io::Result<()> {
    let file = match child(directory, "pending", true, 1) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    check(file.as_raw_handle(), false)?;
    let mut status = Status {
        status: 0,
        information: 0,
    };
    let delete = 1u8;
    // FileDispositionInformation. Delete only the protected retained object,
    // never a path that could have changed between validation and deletion.
    nt(unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status,
            (&delete as *const u8).cast(),
            1,
            13,
        )
    })
}
// SAFETY: retained protected directory + exclusive no-sharing lock, NT
// relative children, atomic same-NTFS rename and mandatory volume barrier.
unsafe impl DurableStore for DirectoryStore {
    fn load(&mut self) -> io::Result<Vec<u8>> {
        let file = child(self.directory.0, "state", false, 1)?;
        check(file.as_raw_handle(), false)?;
        if file.metadata()?.len() > MAX as u64 {
            return Err(invalid("BAR registry too large"));
        }
        let mut bytes = Vec::new();
        file.take(MAX as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > MAX {
            return Err(invalid("BAR registry too large"));
        }
        self.known_state = true;
        Ok(bytes)
    }
    fn replace_synced(&mut self, bytes: &[u8]) -> io::Result<()> {
        match child(self.directory.0, "state", false, 1) {
            Ok(file) => check(file.as_raw_handle(), false)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound && !self.known_state => {}
            Err(e) => return Err(e),
        }
        remove_pending(self.directory.0)?;
        let mut file = child(self.directory.0, "pending", true, 2)?; // FILE_CREATE
        check(file.as_raw_handle(), false)?;
        file.write_all(bytes)?;
        flush(file.as_raw_handle())?;
        #[cfg(test)]
        if self.fault == 1 {
            return Err(io::Error::other("injected failure before atomic rename"));
        }
        let rename = Rename {
            replace: 1,
            root: self.directory.0,
            name_length: 10,
            name: [
                b's' as u16,
                b't' as u16,
                b'a' as u16,
                b't' as u16,
                b'e' as u16,
            ],
        };
        let mut status = Status {
            status: 0,
            information: 0,
        };
        let length = std::mem::offset_of!(Rename, name) + 10;
        nt(unsafe {
            NtSetInformationFile(
                file.as_raw_handle(),
                &mut status,
                (&rename as *const Rename).cast(),
                length as u32,
                10,
            )
        })?;
        flush(file.as_raw_handle())?;
        #[cfg(test)]
        if self.fault == 2 {
            return Err(io::Error::other(
                "injected uncertain rename before volume flush",
            ));
        }
        flush(self.volume.0)?;
        self.known_state = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bar_registry::{PhysicalBacking, Registry};
    use crate::service_control::{Binding, Origin, Roles, Source};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    fn backing() -> PhysicalBacking {
        PhysicalBacking {
            host_device: 7,
            host_inode: 9,
            bar_bytes: 1024 * 1024,
        }
    }
    fn binding(windows: u64, offset: u64) -> Binding {
        Binding {
            installation: "test".into(),
            source: Source {
                vm: "private".into(),
                windows_device: "PCI\\PRIVATE".into(),
                host_device: 7,
                host_inode: 9,
                qemu_start: 11,
                driver_generation: 12,
                bar_run: 13,
                bar_bytes: backing().bar_bytes,
            },
            roles: Roles {
                grantor: Origin::Windows,
                producer: Origin::Windows,
                consumer: Origin::Linux,
            },
            windows_run: windows,
            linux_run: 101,
            grant: 1,
            session: 1,
            grant_offset: offset,
            grant_length: 4096,
            windows_sid: "S-1-5-21-1".into(),
            linux_uid: 1000,
        }
    }
    fn directory(sddl: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "wc-bar-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sd = Sd::from_sddl(sddl).unwrap();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        let name = wide(path.to_str().unwrap());
        assert_ne!(
            unsafe { CreateDirectoryW(name.as_ptr(), &attributes) },
            0,
            "{}",
            io::Error::last_os_error()
        );
        path
    }
    fn open(path: &Path) -> DirectoryStore {
        unsafe { DirectoryStore::open(path).unwrap() }
    }
    #[test]
    fn crash_worker() {
        let Some(path) = std::env::var_os("WC_WINDOWS_BAR_STORE_CRASH") else {
            return;
        };
        let mut registry = unsafe {
            Registry::initialize_fresh(open(Path::new(&path)), backing(), Origin::Windows).unwrap()
        };
        let local = registry.issue_next_service_epoch(Origin::Windows).unwrap();
        unsafe {
            registry.confirm_peer_epoch(&local, 101).unwrap();
        }
        unsafe {
            registry
                .reserve(&local, binding(local.value(), 4096))
                .unwrap();
        }
        // No Rust destructors. Native process termination must not erase the
        // original physical exclusion or make its epoch/pair resumable.
        unsafe { windows_sys::Win32::System::Threading::ExitProcess(0) }
    }
    #[test]
    fn native_ntfs_protection_crash_and_uncertain_commit() {
        let path = directory(SECURITY);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bar_registry::windows::tests::crash_worker",
                "--nocapture",
            ])
            .env("WC_WINDOWS_BAR_STORE_CRASH", &path)
            .status()
            .unwrap();
        assert!(status.success());
        let store = open(&path);
        assert!(
            unsafe { DirectoryStore::open(&path) }.is_err(),
            "exclusive lock bypassed"
        );
        assert!(
            std::fs::rename(&path, path.with_extension("renamed")).is_err(),
            "retained directory was replaced"
        );
        let mut registry = Registry::open(store, backing(), Origin::Windows).unwrap();
        assert_eq!(registry.exclusions(), vec![(4096, 4096)]);
        let local = registry.issue_next_service_epoch(Origin::Windows).unwrap();
        assert_eq!(local.value(), 2);
        unsafe {
            registry.confirm_peer_epoch(&local, 101).unwrap();
        }
        assert!(unsafe { registry.reserve(&local, binding(local.value(), 4096)) }.is_err());
        unsafe {
            registry
                .reserve(&local, binding(local.value(), 8192))
                .unwrap();
        }
        drop(registry);
        for fault in [1, 2] {
            let mut registry = Registry::open(open(&path), backing(), Origin::Windows).unwrap();
            let local = registry.issue_next_service_epoch(Origin::Windows).unwrap();
            unsafe {
                registry.confirm_peer_epoch(&local, 101).unwrap();
            }
            registry.store.fault = fault;
            assert!(unsafe { registry.reserve(&local, binding(local.value(), 12288)) }.is_err());
            assert!(!registry.confirmed_pair(&local, 101));
            assert!(registry.issue_next_service_epoch(Origin::Windows).is_err());
            drop(registry);
            let registry = Registry::open(open(&path), backing(), Origin::Windows).unwrap();
            assert_eq!(registry.exclusions().len(), if fault == 1 { 2 } else { 3 });
            drop(registry);
        }
        let mut registry = Registry::open(open(&path), backing(), Origin::Windows).unwrap();
        std::fs::remove_file(path.join("state")).unwrap();
        assert!(registry.issue_next_service_epoch(Origin::Windows).is_err());
        assert!(!path.join("state").exists());
        drop(registry);
        assert!(Registry::open(open(&path), backing(), Origin::Windows).is_err());
        std::fs::remove_dir_all(path).unwrap();

        let broad = directory("O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;BU)");
        assert!(unsafe { DirectoryStore::open(&broad) }.is_err());
        std::fs::remove_dir_all(broad).unwrap();
        let owned = directory(&format!(
            "O:{}D:P(A;;FA;;;SY)(A;;FA;;;BA)",
            crate::sys::windows::ident::current_user().unwrap()
        ));
        assert!(
            unsafe { DirectoryStore::open(&owned) }.is_err(),
            "desktop creator ownership trusted"
        );
        std::fs::remove_dir_all(owned).unwrap();

        let path = directory(SECURITY);
        let target = path.join("target");
        std::fs::write(&target, b"not canonical state").unwrap();
        std::os::windows::fs::symlink_file(&target, path.join("state")).unwrap();
        let mut store = open(&path);
        assert!(store.load().is_err(), "state reparse target trusted");
        drop(store);
        std::fs::remove_file(path.join("state")).unwrap();
        std::fs::hard_link(&target, path.join("state")).unwrap();
        let mut store = open(&path);
        assert!(store.load().is_err(), "multiply linked state trusted");
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
        println!("WINDOWS NTFS REGISTRY PROTECTION/CRASH/ATOMIC-COMMIT PASS");
    }
}
