//! The platform halves (§111.5).

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod unix;
#[cfg(windows)]
pub mod windows;

/// An account name as one word of an identity's text (§111.6): words have
/// no spaces and no `=`, so every byte outside printable ASCII, and `=`,
/// space and `%`, is written `%XX`; at most 80 bytes. Used by the Windows
/// half (`LookupAccountSidW`'s names may hold spaces and any letter).
pub fn identity_word(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        let piece = if b.is_ascii_graphic() && b != b'=' && b != b'%' { (b as char).to_string() } else { format!("%{b:02X}") };
        if out.len() + piece.len() > 80 {
            break;
        }
        out.push_str(&piece);
    }
    if out.is_empty() { "?".into() } else { out }
}

/// The PnP device instance id behind a device interface path (§111.8: the
/// broker sets an ivshmem device's PnP security by its instance):
/// `\\?\pci#ven_1af4&dev_1110&…#3&267a616a&0&e0#{guid}` is the instance
/// `PCI\VEN_1AF4&DEV_1110&…\3&267A616A&0&E0`.
pub fn instance_from_interface(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\").or_else(|| path.strip_prefix(r"\\.\"))?;
    let (inst, guid) = rest.rsplit_once('#')?;
    if !guid.starts_with('{') || inst.split('#').count() != 3 {
        return None;
    }
    Some(inst.replace('#', "\\").to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interface_path_names_its_device_instance() {
        let p = r"\\?\pci#ven_1af4&dev_1110&subsys_11001af4&rev_01#3&267a616a&0&e0#{df576976-569d-4672-95a0-f57e4ea0b210}";
        assert_eq!(
            instance_from_interface(p).as_deref(),
            Some(r"PCI\VEN_1AF4&DEV_1110&SUBSYS_11001AF4&REV_01\3&267A616A&0&E0")
        );
        assert_eq!(instance_from_interface(r"\\.\Global\org.sirati.wcbell.0"), None);
        assert_eq!(instance_from_interface(r"\\?\pci#x#{g}"), None);
    }

    #[test]
    fn an_account_name_becomes_one_identity_word() {
        assert_eq!(identity_word("guest-user"), "guest-user");
        assert_eq!(identity_word("First Last"), "First%20Last");
        assert_eq!(identity_word("a=b%c"), "a%3Db%25c");
        assert_eq!(identity_word("J\u{fc}rgen"), "J%C3%BCrgen");
        assert_eq!(identity_word(&"x".repeat(100)).len(), 80);
        assert_eq!(identity_word(""), "?");
    }
}

/// Host-provided PCI shared memory in a macOS guest. This is separate from
/// the POSIX shm backend for macOS-owned guest-RAM windows.
#[cfg(target_os = "macos")]
pub mod macos_pci;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod wired_holder;

#[cfg(target_os = "macos")]
pub mod macos_mach;

mod machwire;

mod machworkers;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod pci_owner;
