//! Read-only staging anchor. Provisioning/reassignment requires a separately
//! admitted host drain and backing reset; the broker never creates this state.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use crate::raw_device::VmPciOwner;
use std::{
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::MetadataExt,
};

pub(crate) const ANCHOR: &str = "/Library/Application Support/waycompose/pci-vm-owner";
fn denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}
#[cfg(target_os = "macos")]
fn no_extended_acl(file: &File) -> io::Result<()> {
    use std::{ffi::c_void, ptr};
    unsafe extern "C" {
        fn acl_get_fd_np(fd: i32, kind: i32) -> *mut c_void;
        fn acl_init(count: i32) -> *mut c_void;
        fn acl_valid(acl: *mut c_void) -> i32;
        fn acl_get_entry(acl: *mut c_void, index: i32, entry: *mut *mut c_void) -> i32;
        fn acl_free(acl: *mut c_void) -> i32;
    }
    struct Acl(*mut c_void);
    impl Drop for Acl {
        fn drop(&mut self) {
            unsafe {
                acl_free(self.0);
            }
        }
    }
    fn owned(pointer: *mut c_void) -> io::Result<Acl> {
        if pointer.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Acl(pointer))
        }
    }
    fn first(acl: &Acl) -> (i32, Option<i32>, bool) {
        let mut entry = ptr::null_mut();
        let result = unsafe { acl_get_entry(acl.0, 0, &mut entry) };
        let error = if result < 0 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        (result, error, !entry.is_null())
    }
    let actual = owned(unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) })?;
    let empty = owned(unsafe { acl_init(0) })?;
    if unsafe { acl_valid(actual.0) } != 0 {
        return Err(denied("invalid PCI owner anchor ACL"));
    }
    // Compare with the platform's valid empty ACL result rather than treating
    // any ACL query failure as proof of absence. An entry always refuses.
    let absent = first(&empty);
    let present = first(&actual);
    if absent.2 || present.2 || present != absent {
        return Err(denied(
            "PCI owner anchor has an extended ACL or unknown ACL state",
        ));
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn no_extended_acl(_file: &File) -> io::Result<()> {
    Ok(())
}

fn directory(file: &File, owner: u32) -> io::Result<()> {
    let meta = file.metadata()?;
    if !meta.is_dir() || meta.uid() != owner || meta.mode() & 0o022 != 0 {
        return Err(denied("untrusted PCI owner anchor directory"));
    }
    no_extended_acl(file)
}
fn open_at(parent: &File, name: &std::ffi::CStr, flags: i32) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn load_at(parent: &File, owner: u32) -> io::Result<VmPciOwner> {
    directory(parent, owner)?;
    let file = open_at(parent, c"pci-vm-owner", libc::O_RDONLY | libc::O_NONBLOCK)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != owner || meta.mode() & 0o7777 != 0o400 || meta.nlink() != 1 {
        return Err(denied("untrusted PCI owner anchor file"));
    }
    no_extended_acl(&file)?;
    let mut text = String::new();
    file.take(257).read_to_string(&mut text)?;
    if text.len() > 256 {
        return Err(denied("oversized PCI owner anchor"));
    }
    VmPciOwner::parse(&text).map_err(denied)
}
pub(crate) fn load() -> io::Result<VmPciOwner> {
    let mut parent = File::open("/")?;
    directory(&parent, 0)?;
    for component in [c"Library", c"Application Support", c"waycompose"] {
        parent = open_at(&parent, component, libc::O_RDONLY | libc::O_DIRECTORY)?;
        directory(&parent, 0)?;
    }
    load_at(&parent, 0).map_err(|e| {
        denied(format!(
            "PCI lending held: trusted preprovisioned {ANCHOR} required: {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn missing_and_untrusted_anchors_never_enroll_or_reset() {
        let path = std::env::temp_dir().join(format!("pci-owner-anchor-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let parent = File::open(&path).unwrap();
        let uid = unsafe { libc::geteuid() };
        let anchor = path.join("pci-vm-owner");
        assert!(load_at(&parent, uid).is_err());
        assert!(!anchor.exists());
        std::fs::write(
            &anchor,
            "wc-pci-vm-owner-preparatory-v1\nall-pci-backings\tuid:501\n",
        )
        .unwrap();
        assert!(load_at(&parent, uid).is_err());
        std::fs::set_permissions(&anchor, std::fs::Permissions::from_mode(0o400)).unwrap();
        for _restart in 0..2 {
            let owner = load_at(&parent, uid).unwrap();
            owner.authorize(&crate::User::uid(501)).unwrap();
            assert!(owner.authorize(&crate::User::uid(502)).is_err());
        }
        assert!(load_at(&parent, uid.wrapping_add(1)).is_err());
        let alias = path.join("alias");
        std::fs::hard_link(&anchor, &alias).unwrap();
        assert!(load_at(&parent, uid).is_err());
        std::fs::remove_file(&alias).unwrap();
        std::fs::rename(&anchor, &alias).unwrap();
        symlink(&alias, &anchor).unwrap();
        assert!(load_at(&parent, uid).is_err());
        std::fs::remove_file(&anchor).unwrap();
        std::fs::rename(&alias, &anchor).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o702)).unwrap();
        assert!(load_at(&parent, uid).is_err());
        std::fs::remove_file(&anchor).unwrap();
        std::fs::remove_dir(&path).unwrap();
    }
}
