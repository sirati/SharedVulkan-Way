//! Durable whole-device ownership. There is deliberately no first-claim or
//! release operation: process exit does not drain a shared physical BAR.
use crate::User;
use std::collections::BTreeMap;

/// Parse an administrator-provisioned snapshot. Every row binds a canonical
/// physical device identity to one reviewed SID. An invalid snapshot grants
/// nothing, including otherwise valid rows preceding the error.
pub fn bindings(text: &str) -> Result<BTreeMap<String, User>, String> {
    let mut lines = text.lines();
    if lines.next() != Some("wc-raw-device-owners-v1") {
        return Err("missing raw-device ownership version".into());
    }
    let mut result = BTreeMap::new();
    for line in lines {
        let (identity, sid) = line
            .split_once('\t')
            .ok_or("invalid raw-device owner row")?;
        if identity.is_empty() || identity.chars().any(char::is_control) || !valid_sid(sid) {
            return Err("invalid raw-device identity or SID".into());
        }
        if result
            .insert(identity.to_ascii_lowercase(), User::sid(sid))
            .is_some()
        {
            return Err("duplicate raw-device identity".into());
        }
    }
    Ok(result)
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
