//! Durable whole-device ownership. There is deliberately no first-claim or
//! release operation: process exit does not drain a shared physical BAR.
use crate::User;
use std::collections::BTreeMap;

/// Parse an administrator-provisioned snapshot. Every row binds a canonical
/// physical device identity to one reviewed SID. An invalid snapshot grants
/// nothing, including otherwise valid rows preceding the error.
pub fn bindings(text: &str) -> Result<BTreeMap<String, User>, String> {
    snapshot(text, "wc-raw-device-owners-v1", |sid| {
        valid_sid(sid).then(|| User::sid(sid))
    })
}

fn snapshot(
    text: &str,
    version: &str,
    owner: impl Fn(&str) -> Option<User>,
) -> Result<BTreeMap<String, User>, String> {
    let mut lines = text.lines();
    if lines.next() != Some(version) {
        return Err("missing raw-device ownership version".into());
    }
    let mut result = BTreeMap::new();
    for line in lines {
        let (identity, account) = line
            .split_once('\t')
            .ok_or("invalid raw-device owner row")?;
        if identity.is_empty() || identity.chars().any(char::is_control) {
            return Err("invalid raw-device identity".into());
        }
        let user = owner(account).ok_or("invalid raw-device owner")?;
        if result.insert(identity.to_ascii_lowercase(), user).is_some() {
            return Err("duplicate raw-device identity".into());
        }
    }
    Ok(result)
}

/// Preparatory restriction only: every PCI backing in this VM is permanently
/// restricted to one uid. This does not implement multi-user per-range lending.
/// There is no alias/device/boot key and no enrollment, reset or release API.
#[derive(Clone, Debug)]
pub struct VmPciOwner(User);
impl VmPciOwner {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut owners = snapshot(text, "wc-pci-vm-owner-preparatory-v1", |text| {
            let uid = text.strip_prefix("uid:")?.parse::<u32>().ok()?;
            let user = User::uid(uid);
            (user.0 == text).then_some(user)
        })?;
        if owners.len() != 1 {
            return Err("PCI staging requires exactly one permanent VM-wide uid binding".into());
        }
        owners
            .remove("all-pci-backings")
            .map(Self)
            .ok_or("PCI staging requires the all-pci-backings scope".into())
    }
    pub fn authorize(&self, user: &User) -> Result<(), String> {
        if &self.0 == user {
            Ok(())
        } else {
            Err(
                "PCI staging forbids uid reassignment across every device, alias, range and boot"
                    .into(),
            )
        }
    }
}

fn valid_sid(sid: &str) -> bool {
    let Some(rest) = sid.strip_prefix("S-1-") else {
        return false;
    };
    let components: Vec<_> = rest.split('-').collect();
    (2..=16).contains(&components.len())
        && components.iter().enumerate().all(|(i, v)| {
            !v.is_empty()
                && v.bytes().all(|b| b.is_ascii_digit())
                && v.parse::<u64>().is_ok_and(|n| {
                    if i == 0 {
                        n < (1 << 48)
                    } else {
                        n <= u32::MAX as u64
                    }
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Allow, DeviceSpec, Peer, core::Core};

    #[test]
    fn corrupt_or_ambiguous_snapshots_never_return_partial_grants() {
        for s in [
            "",
            "PCI\tS-1-5-21-1",
            "wc-raw-device-owners-v1\nPCI\tS-1-5-21-1\nbroken",
            "wc-raw-device-owners-v1\nPCI\tS-1-5-21-1\npci\tS-1-5-21-2",
            "wc-raw-device-owners-v1\nPCI\tS-1-5-21-1\textra",
            "wc-raw-device-owners-v1\nPCI\tS-1-5-4294967296",
        ] {
            assert!(bindings(s).is_err(), "{s:?}");
        }
    }

    #[test]
    fn persisted_identity_survives_exit_and_restart_without_reassignment() {
        let snapshot = "wc-raw-device-owners-v1\nPCI\\VEN_1\tS-1-5-21-1";
        for _restart in 0..2 {
            let owners = bindings(snapshot).unwrap();
            let owner = owners.get("pci\\ven_1").unwrap().clone();
            let mut core = Core::new(vec![DeviceSpec {
                name: "renamed".into(),
                allow: Allow::Users(vec![owner.clone()]),
            }]);
            core.connect(
                1,
                Peer {
                    pid: 101,
                    user: owner,
                    admin: false,
                },
            );
            core.connect(
                2,
                Peer {
                    pid: 102,
                    user: User::sid("S-1-5-21-2"),
                    admin: true,
                },
            );
            core.device(1, "renamed").unwrap();
            core.process_exited(101, std::time::Instant::now());
            core.connect(
                1,
                Peer {
                    pid: 101,
                    user: owners.get("pci\\ven_1").unwrap().clone(),
                    admin: false,
                },
            );
            assert!(core.device(2, "renamed").is_err());
            core.device(1, "renamed").unwrap();
            assert!(!owners.contains_key("pci\\replacement"));
        }
    }
}

#[cfg(test)]
mod vm_pci_tests {
    use super::*;
    const STATE: &str = "wc-pci-vm-owner-preparatory-v1\nall-pci-backings\tuid:501\n";
    #[test]
    fn restart_relocation_and_service_aliases_never_create_a_fresh_uid_claim() {
        for _restart in 0..3 {
            let owner = VmPciOwner::parse(STATE).unwrap();
            // Locators intentionally cannot select another ownership record.
            for _locator in [
                "registry:1@0+4096",
                "registry:99@4096+4096",
                "relocated-device",
                "new-boot",
            ] {
                owner.authorize(&User::uid(501)).unwrap();
                assert!(owner.authorize(&User::uid(502)).is_err());
                assert!(owner.authorize(&User::uid(0)).is_err());
            }
        }
    }
    #[test]
    fn missing_partial_alias_or_ambiguous_staging_snapshots_grant_nothing() {
        for text in [
            "",
            "wc-pci-vm-owner-preparatory-v1",
            "wc-pci-vm-owner-preparatory-v1\nregistry:1\tuid:501",
            "wc-pci-vm-owner-preparatory-v1\nall-pci-backings\tuid:0501",
            "wc-pci-vm-owner-preparatory-v1\nall-pci-backings\tuid:501\nother\tuid:502",
            "wc-pci-vm-owner-preparatory-v1\nall-pci-backings\tuid:501\nALL-PCI-BACKINGS\tuid:502",
            "wc-pci-vm-owner-preparatory-v1\nall-pci-backings\tuid:501\nbroken",
            "wc-pci-vm-owner-preparatory-v1\nall-pci-backings\tuid:4294967296",
        ] {
            assert!(VmPciOwner::parse(text).is_err(), "{text:?}");
        }
    }
}
