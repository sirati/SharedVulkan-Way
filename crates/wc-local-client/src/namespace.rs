//! A private SID-owned AF_UNIX pathname, authenticated as a retained filesystem
//! object. Windows enforces write access on the socket when connecting.
//! https://devblogs.microsoft.com/commandline/af_unix-comes-to-windows/
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation, INHERIT_ONLY_ACE,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const READ_CONTROL: u32 = 0x0002_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const FILE_ALL_ACCESS: u32 = 0x001f_01ff;
const AF_UNIX_TAG: u32 = 0x8000_0023;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const SYSTEM: &str = "S-1-5-18";
const ADMINISTRATORS: &str = "S-1-5-32-544";
const TRUSTED_INSTALLER: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

struct Owned(HANDLE);
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Sd(PSECURITY_DESCRIPTOR);
impl Drop for Sd {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn deny(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why)
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
unsafe fn sid_text(sid: *mut core::ffi::c_void) -> io::Result<String> {
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut n = 0;
    while unsafe { *text.add(n) } != 0 {
        n += 1;
    }
    let result = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, n) });
    unsafe {
        LocalFree(text.cast());
    }
    Ok(result)
}
fn current_sid() -> io::Result<String> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Owned(token);
    let mut n = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut n);
    }
    if n == 0 || n > 65536 {
        return Err(deny("invalid user token size"));
    }
    let mut data = vec![0u64; (n as usize).div_ceil(8)];
    if unsafe { GetTokenInformation(token.0, TokenUser, data.as_mut_ptr().cast(), n, &mut n) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    unsafe { sid_text(user.User.Sid) }
}
fn descriptor(sid: &str, directory: bool) -> io::Result<Sd> {
    let flags = if directory { "OICI" } else { "" };
    let system = if sid == SYSTEM {
        String::new()
    } else {
        format!("(A;{flags};FA;;;SY)")
    };
    let text = wide(&format!("O:{sid}D:P(A;{flags};FA;;;{sid}){system}"));
    let mut sd = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Sd(sd))
}
fn normalized(path: &Path) -> io::Result<PathBuf> {
    let text = path
        .to_str()
        .ok_or_else(|| deny("local pathname is not Unicode"))?;
    if text.len() > 1024 || text.contains('\0') {
        return Err(deny("local pathname bound"));
    }
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Prefix(prefix)), Some(Component::RootDir))
            if matches!(prefix.kind(), Prefix::Disk(_)) => {}
        _ => {
            return Err(deny(
                "local namespace requires an absolute drive path, not UNC or device paths",
            ));
        }
    }
    let tail: Vec<_> = components.collect();
    if tail.is_empty() || tail.len() > 32 || tail.iter().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(deny("invalid local pathname ancestry"));
    }
    for component in &tail {
        let name = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| deny("non-Unicode component"))?;
        if name.ends_with([' ', '.']) || name.chars().any(|c| c < ' ' || "<>:\"|?*".contains(c)) {
            return Err(deny("ambiguous or alternate-stream pathname"));
        }
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return Err(deny("reserved DOS pathname"));
        }
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        result.push(component.as_os_str());
    }
    Ok(result)
}
fn open(path: &Path, write_security: bool, directory: bool) -> io::Result<Owned> {
    let text = wide(path.to_str().ok_or_else(|| deny("non-Unicode path"))?);
    let rights = READ_CONTROL
        | FILE_READ_ATTRIBUTES
        | if write_security {
            WRITE_DAC | WRITE_OWNER
        } else {
            0
        };
    // Directory handles deny both write and delete sharing. Socket files allow
    // data writes for connect, but deny deletion/replacement throughout use.
    let share = FILE_SHARE_READ | if directory { 0 } else { FILE_SHARE_WRITE };
    let raw = unsafe {
        CreateFileW(
            text.as_ptr(),
            rights,
            share,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(Owned(raw))
}
// Each component opens relative to the already retained parent kernel handle.
// No lexical ancestor is resolved again, so temporary-reparse/restore changes
// cannot redirect acquisition to a different directory tree.
fn open_child(parent: &Owned, name: &std::ffi::OsStr) -> io::Result<Owned> {
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT,
        FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError, UNICODE_STRING,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
    let name = name
        .to_str()
        .ok_or_else(|| deny("non-Unicode relative component"))?;
    if name.is_empty() || name.contains(['/', '\\', ':']) {
        return Err(deny("relative name must be one component"));
    }
    let mut text: Vec<u16> = name.encode_utf16().collect();
    let bytes = u16::try_from(text.len() * 2).map_err(|_| deny("relative pathname bound"))?;
    let unicode = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: text.as_mut_ptr(),
    };
    let object = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.0,
        ObjectName: &unicode,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        ..Default::default()
    };
    let mut status: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let mut raw = std::ptr::null_mut();
    let result = unsafe {
        NtCreateFile(
            &mut raw,
            READ_CONTROL | FILE_READ_ATTRIBUTES | 0x0010_0000,
            &object,
            &mut status,
            std::ptr::null(),
            0,
            FILE_SHARE_READ,
            FILE_OPEN,
            FILE_DIRECTORY_FILE
                | FILE_OPEN_REPARSE_POINT
                | FILE_OPEN_FOR_BACKUP_INTENT
                | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    Ok(Owned(raw))
}
fn attributes(object: &Owned) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
    let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    if unsafe {
        GetFileInformationByHandleEx(
            object.0,
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}
fn security(object: &Owned) -> io::Result<Sd> {
    let mut sd = std::ptr::null_mut();
    let result = unsafe {
        GetSecurityInfo(
            object.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
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
    Ok(Sd(sd))
}
fn trusted_ancestor(sid: &str, user: &str) -> bool {
    sid == user || sid == SYSTEM || sid == ADMINISTRATORS || sid == TRUSTED_INSTALLER
}
fn validate(sd: &Sd, user: &str, strict: bool, directory: bool) -> io::Result<()> {
    let mut owner = std::ptr::null_mut();
    let mut defaulted = 0;
    if unsafe { GetSecurityDescriptorOwner(sd.0, &mut owner, &mut defaulted) } == 0
        || owner.is_null()
    {
        return Err(deny("namespace owner unavailable"));
    }
    let owner = unsafe { sid_text(owner) }?;
    if if strict {
        owner != user
    } else {
        !trusted_ancestor(&owner, user)
    } {
        return Err(deny("namespace owner is another SID"));
    }
    let mut acl: *mut ACL = std::ptr::null_mut();
    let mut present = 0;
    if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted) } == 0
        || present == 0
        || acl.is_null()
    {
        return Err(deny("namespace has no restrictive DACL"));
    }
    let mut control = 0;
    let mut revision = 0;
    if unsafe { GetSecurityDescriptorControl(sd.0, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if strict && control & SE_DACL_PROTECTED == 0 {
        return Err(deny("private namespace inherits an uncontrolled DACL"));
    }
    let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        GetAclInformation(
            acl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of_val(&info) as u32,
            AclSizeInformation,
        )
    } == 0
        || info.AceCount > 64
    {
        return Err(deny("namespace ACL bound"));
    }
    let mut mine = false;
    let mut system = false;
    for index in 0..info.AceCount {
        let mut ace = std::ptr::null_mut();
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceType == ACCESS_DENIED_ACE_TYPE {
            if strict {
                return Err(deny("noncanonical private ACL"));
            }
            continue;
        }
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            return Err(deny("unsupported namespace ACE"));
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = unsafe { sid_text((&allowed.SidStart as *const u32).cast_mut().cast()) }?;
        if strict {
            let flags = if directory { 3 } else { 0 };
            if header.AceFlags != flags
                || allowed.Mask != FILE_ALL_ACCESS
                || !(sid == user || sid == SYSTEM)
            {
                return Err(deny(
                    "private namespace grants another principal or has unexpected access",
                ));
            }
            mine |= sid == user;
            system |= sid == SYSTEM;
        } else if u32::from(header.AceFlags) & INHERIT_ONLY_ACE == 0
            && !trusted_ancestor(&sid, user)
        {
            // Handles deny write-data/delete sharing. Attributes and EAs are
            // not covered by sharing, but cannot redirect a nonempty directory:
            // FSCTL_SET_REPARSE_POINT refuses STATUS_DIRECTORY_NOT_EMPTY.
            // The full chain is revalidated after every next child is pinned.
            // Changing owner/DACL is not covered by sharing and remains fatal.
            const MUTATE: u32 = WRITE_DAC | WRITE_OWNER | 0x1000_0000;
            if allowed.Mask & MUTATE != 0 {
                return Err(deny("ancestor grants namespace mutation to another SID"));
            }
        }
    }
    if strict && (!mine || (user != SYSTEM && !system)) {
        return Err(deny("private namespace lacks its owner/SYSTEM grant"));
    }
    Ok(())
}

pub struct DirectoryPin {
    path: PathBuf,
    _ancestors: Vec<Owned>,
    user: String,
}
impl DirectoryPin {
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = normalized(path)?;
        let user = current_sid()?;
        if path.file_name().and_then(|s| s.to_str()) != Some(user.as_str()) {
            return Err(deny("runtime is not the private SID directory"));
        }
        let mut ancestors = Vec::new();
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component.as_os_str());
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            let object = match ancestors.last() {
                Some(parent) => open_child(parent, component.as_os_str())?,
                None => open(&current, false, true)?,
            };
            let info = attributes(&object)?;
            if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
                || info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(deny("namespace ancestor is not an ordinary directory"));
            }
            validate(&security(&object)?, &user, current == path, true)?;
            ancestors.push(object);
        }
        // Every child was opened relative to its retained parent handle, so
        // each retained parent-child identity edge is exact. Recheck tags and
        // security after acquiring the whole chain. Every ancestor now contains
        // its pinned next child and cannot become empty.
        // Microsoft documents that setting a reparse point on a nonempty
        // directory fails with STATUS_DIRECTORY_NOT_EMPTY:
        // https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/fsctl-set-reparse-point
        for (index, object) in ancestors.iter().enumerate() {
            let info = attributes(object)?;
            if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
                || info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(deny("namespace ancestry changed during acquisition"));
            }
            validate(
                &security(object)?,
                &user,
                index + 1 == ancestors.len(),
                true,
            )?;
        }
        Ok(Self {
            path,
            _ancestors: ancestors,
            user,
        })
    }
    pub fn runtime() -> io::Result<Self> {
        Self::open(
            &std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .ok_or_else(|| deny("missing private runtime"))?,
        )
    }
    pub fn seal(self, name: &Path) -> io::Result<SocketPin> {
        if name.components().count() != 1
            || !matches!(name.components().next(), Some(Component::Normal(_)))
        {
            return Err(deny("private socket name must be one component"));
        }
        let path = normalized(&self.path.join(name))?;
        let file = open(&path, true, false)?;
        socket_type(&file)?;
        let sd = descriptor(&self.user, false)?;
        let mut owner = std::ptr::null_mut();
        let mut acl = std::ptr::null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        if unsafe { GetSecurityDescriptorOwner(sd.0, &mut owner, &mut defaulted) } == 0
            || unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted) }
                == 0
        {
            return Err(io::Error::last_os_error());
        }
        let result = unsafe {
            SetSecurityInfo(
                file.0,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                owner,
                std::ptr::null_mut(),
                acl,
                std::ptr::null_mut(),
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        validate(&security(&file)?, &self.user, true, false)?;
        Ok(SocketPin {
            _directory: self,
            _file: file,
        })
    }
}
fn socket_type(file: &Owned) -> io::Result<()> {
    let info = attributes(file)?;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 || info.ReparseTag != AF_UNIX_TAG {
        return Err(deny("endpoint is not an AF_UNIX socket reparse object"));
    }
    Ok(())
}
pub struct SocketPin {
    _directory: DirectoryPin,
    _file: Owned,
}
impl SocketPin {
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = normalized(path)?;
        if path.to_string_lossy().len() >= 108 {
            return Err(deny("AF_UNIX pathname exceeds socket bound"));
        }
        let directory =
            DirectoryPin::open(path.parent().ok_or_else(|| deny("socket has no parent"))?)?;
        let file = open(&path, false, false)?;
        socket_type(&file)?;
        validate(&security(&file)?, &directory.user, true, false)?;
        Ok(Self {
            _directory: directory,
            _file: file,
        })
    }
}
/// Atomically create, or validate, a private full-SID child of an existing
/// directory. No existing owner/DACL is repaired or adopted implicitly.
pub fn prepare(base: &Path) -> io::Result<PathBuf> {
    let base = normalized(base)?;
    let user = current_sid()?;
    let path = base.join(&user);
    let sd = descriptor(&user, true)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    let text = wide(path.to_str().ok_or_else(|| deny("invalid runtime name"))?);
    if unsafe { CreateDirectoryW(text.as_ptr(), &attributes) } == 0
        && io::Error::last_os_error().raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32)
    {
        return Err(io::Error::last_os_error());
    }
    let _pin = DirectoryPin::open(&path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn broad_modify_ancestor_cannot_replace_or_redirect_pinned_children() {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        let base = std::env::var_os("WC_LOCAL_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("wc-modify-{}", std::process::id()));
        let user = current_sid().unwrap();
        let text = wide(&format!(
            "O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;0x1301bf;;;AU)"
        ));
        let mut sd = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let sd = Sd(sd);
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        assert_ne!(
            unsafe { CreateDirectoryW(wide(base.to_str().unwrap()).as_ptr(), &sa) },
            0
        );
        let directory = prepare(&base).unwrap();
        let extra = |rights| {
            let raw = unsafe {
                CreateFileW(
                    wide(base.to_str().unwrap()).as_ptr(),
                    rights,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                    std::ptr::null_mut(),
                )
            };
            if raw == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Owned(raw))
            }
        };
        for right in [FILE_WRITE_DATA, 0x0001_0000] {
            let before = extra(right).unwrap();
            assert!(
                DirectoryPin::open(&directory).is_err(),
                "preexisting write/delete handle was ignored"
            );
            drop(before);
        }
        let pin = DirectoryPin::open(&directory).unwrap();
        assert!(extra(FILE_WRITE_DATA).is_err());
        assert!(extra(0x0001_0000).is_err());
        assert!(std::fs::rename(&base, base.with_extension("replaced")).is_err());
        assert!(std::fs::remove_dir(&directory).is_err());
        assert!(std::fs::rename(&directory, base.join("replacement-child")).is_err());
        // Build a valid mount-point reparse buffer. Use an existing absolute
        // directory target. A bad buffer must not masquerade as a sharing test.
        let target = format!("\\??\\{}", base.to_str().unwrap());
        let target: Vec<u16> = target.encode_utf16().collect();
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&0xa000_0003u32.to_le_bytes());
        buffer.extend_from_slice(&((8 + (target.len() + 2) * 2) as u16).to_le_bytes());
        buffer.extend_from_slice(&0u16.to_le_bytes());
        for n in [0, target.len() * 2, (target.len() + 1) * 2, 0] {
            buffer.extend_from_slice(&(n as u16).to_le_bytes());
        }
        for c in target.iter().copied().chain([0, 0]) {
            buffer.extend_from_slice(&c.to_le_bytes());
        }
        let control = |handle: &Owned| {
            let mut returned = 0;
            let ok = unsafe {
                DeviceIoControl(
                    handle.0,
                    0x0009_00a4,
                    buffer.as_ptr().cast(),
                    buffer.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            (ok, io::Error::last_os_error().raw_os_error())
        };
        // Attribute-only handles are deliberately not assumed to be denied by
        // sharing. Whether open succeeds or fails, no reparse redirect may work.
        if let Ok(attributes_only) = extra(FILE_WRITE_ATTRIBUTES) {
            let (ok, error) = control(&attributes_only);
            assert_eq!(ok, 0, "attribute-only handle redirected pinned ancestry");
            assert!(
                matches!(error, Some(5 | 32 | 145)),
                "unexpected reparse refusal: {error:?}"
            );
        }
        for object in &pin._ancestors {
            assert_eq!(
                attributes(object).unwrap().FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT,
                0
            );
        }
        drop(pin);
        // Positive control on an owned empty directory verifies the identical
        // reparse buffer and native control call, then removes the junction.
        let probe = base.join("empty-control");
        std::fs::create_dir(&probe).unwrap();
        let raw = unsafe {
            CreateFileW(
                wide(probe.to_str().unwrap()).as_ptr(),
                0x4000_0000,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(raw, INVALID_HANDLE_VALUE);
        let handle = Owned(raw);
        assert_ne!(control(&handle).0, 0, "positive reparse control failed");
        drop(handle);
        std::fs::remove_dir(&probe).unwrap();
        std::fs::remove_dir(&directory).unwrap();
        std::fs::remove_dir(&base).unwrap();
    }
    #[test]
    fn relative_open_refuses_temporary_reparse_and_restore_aba() {
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
        use windows_sys::Win32::System::IO::DeviceIoControl;
        let base = std::env::var_os("WC_LOCAL_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("wc-aba-{}", std::process::id()));
        let parent = base.join("parent");
        let target = base.join("target");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::create_dir_all(target.join("child")).unwrap();
        // Deliberately weaken only this test handle's sharing to permit an ABA
        // attack even where production pin sharing would already stop it.
        let make = |rights| {
            let raw = unsafe {
                CreateFileW(
                    wide(parent.to_str().unwrap()).as_ptr(),
                    rights,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(raw, INVALID_HANDLE_VALUE);
            Owned(raw)
        };
        let retained = make(READ_CONTROL | FILE_READ_ATTRIBUTES);
        let attacker = make(0x4000_0000);
        let nt: Vec<u16> = format!("\\??\\{}", target.display())
            .encode_utf16()
            .collect();
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&0xa000_0003u32.to_le_bytes());
        buffer.extend_from_slice(&((8 + (nt.len() + 2) * 2) as u16).to_le_bytes());
        buffer.extend_from_slice(&0u16.to_le_bytes());
        for n in [0, nt.len() * 2, (nt.len() + 1) * 2, 0] {
            buffer.extend_from_slice(&(n as u16).to_le_bytes());
        }
        for c in nt.into_iter().chain([0, 0]) {
            buffer.extend_from_slice(&c.to_le_bytes());
        }
        let control = |code, bytes: &[u8]| {
            let mut returned = 0;
            unsafe {
                DeviceIoControl(
                    attacker.0,
                    code,
                    bytes.as_ptr().cast(),
                    bytes.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            }
        };
        assert_ne!(
            control(0x0009_00a4, &buffer),
            0,
            "test could not install temporary junction"
        );
        assert!(
            parent.join("child").is_dir(),
            "lexical positive control must follow redirect"
        );
        let acquired = open_child(&retained, std::ffi::OsStr::new("child"));
        let mut delete = Vec::new();
        delete.extend_from_slice(&0xa000_0003u32.to_le_bytes());
        delete.extend_from_slice(&[0; 4]);
        assert_ne!(
            control(0x0009_00ac, &delete),
            0,
            "test could not restore ordinary parent"
        );
        assert_eq!(
            attributes(&retained).unwrap().FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT,
            0
        );
        assert!(
            acquired.is_err(),
            "relative acquisition followed temporary reparse before restoration"
        );
        assert!(!parent.join("child").exists());
        drop(attacker);
        drop(retained);
        std::fs::remove_dir(&parent).unwrap();
        std::fs::remove_dir(target.join("child")).unwrap();
        std::fs::remove_dir(&target).unwrap();
        std::fs::remove_dir(&base).unwrap();
    }
    #[test]
    fn private_acl_refuses_cross_sid_owner_and_untrusted_grants() {
        let user = current_sid().unwrap();
        let good = descriptor(&user, true).unwrap();
        validate(&good, &user, true, true).unwrap();
        assert!(validate(&good, "S-1-5-21-111-222-333-444", true, true).is_err());
        for text in [
            format!("O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;WD)"),
            format!("O:{user}D:(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)"),
        ] {
            let text = wide(&text);
            let mut sd = std::ptr::null_mut();
            assert_ne!(
                unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        text.as_ptr(),
                        1,
                        &mut sd,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            assert!(validate(&Sd(sd), &user, true, true).is_err());
        }
    }
    #[test]
    fn pinned_namespace_and_socket_cannot_be_replaced_before_actual_disconnect() {
        let base = std::env::var_os("WC_LOCAL_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("wc-private-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let directory = prepare(&base).unwrap();
        let pin = DirectoryPin::open(&directory).unwrap();
        assert!(std::fs::rename(&directory, base.join("replacement")).is_err());
        let endpoint = directory.join("socket");
        let listener = wc_transport::Listener::bind(&endpoint).unwrap();
        // Unsealed children are refused even though their ACL was inherited from
        // the private parent. Sealing makes owner/DACL policy explicit.
        assert!(SocketPin::open(&endpoint).is_err());
        let server_pin = pin.seal(Path::new("socket")).unwrap();
        let client_pin = SocketPin::open(&endpoint).unwrap();
        assert!(std::fs::rename(&endpoint, directory.join("replacement-socket")).is_err());
        let client = wc_transport::connect(&endpoint).unwrap();
        let _server = wc_transport::accept(&listener).unwrap().unwrap();
        drop(client);
        drop(client_pin);
        drop(server_pin);
        drop(listener);
        std::fs::rename(&endpoint, directory.join("after-release")).unwrap();
        std::fs::remove_file(directory.join("after-release")).unwrap();
        std::fs::remove_dir(&directory).unwrap();
        std::fs::remove_dir(&base).unwrap();
    }
    #[test]
    fn windows_access_check_denies_foreign_sid_connect_right_in_real_private_descriptor() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, LUID};
        use windows_sys::Win32::Security::Authorization::{
            AUTHZ_ACCESS_REPLY, AUTHZ_ACCESS_REQUEST, AUTHZ_RM_FLAG_NO_AUDIT,
            AUTHZ_SKIP_TOKEN_GROUPS, AuthzAccessCheck, AuthzFreeContext, AuthzFreeResourceManager,
            AuthzInitializeContextFromSid, AuthzInitializeResourceManager, ConvertStringSidToSidW,
        };
        let user = current_sid().unwrap();
        let sd = descriptor(&user, false).unwrap();
        let mut manager = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                AuthzInitializeResourceManager(
                    AUTHZ_RM_FLAG_NO_AUDIT,
                    None,
                    None,
                    None,
                    std::ptr::null(),
                    &mut manager,
                )
            },
            0
        );
        for (sid, allowed) in [(user.as_str(), true), ("S-1-5-21-111-222-333-444", false)] {
            let mut raw_sid = std::ptr::null_mut();
            assert_ne!(
                unsafe { ConvertStringSidToSidW(wide(sid).as_ptr(), &mut raw_sid) },
                0
            );
            let mut context = std::ptr::null_mut();
            assert_ne!(
                unsafe {
                    AuthzInitializeContextFromSid(
                        AUTHZ_SKIP_TOKEN_GROUPS,
                        raw_sid,
                        manager,
                        std::ptr::null(),
                        LUID {
                            LowPart: 0,
                            HighPart: 0,
                        },
                        std::ptr::null(),
                        &mut context,
                    )
                },
                0
            );
            let request = AUTHZ_ACCESS_REQUEST {
                DesiredAccess: 2,
                ..Default::default()
            };
            let mut granted = 0;
            let mut error = 0;
            let mut reply = AUTHZ_ACCESS_REPLY {
                ResultListLength: 1,
                GrantedAccessMask: &mut granted,
                SaclEvaluationResults: std::ptr::null_mut(),
                Error: &mut error,
            };
            assert_ne!(
                unsafe {
                    AuthzAccessCheck(
                        0,
                        context,
                        &request,
                        std::ptr::null_mut(),
                        sd.0,
                        std::ptr::null(),
                        0,
                        &mut reply,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            if allowed {
                assert_eq!(error, 0);
                assert_eq!(granted & 2, 2);
            } else {
                assert_eq!(error, ERROR_ACCESS_DENIED);
                assert_eq!(granted & 2, 0);
            }
            unsafe {
                AuthzFreeContext(context);
                LocalFree(raw_sid);
            }
        }
        unsafe {
            AuthzFreeResourceManager(manager);
        }
    }
}
