//! A doorbell hub reached through the broker (waycompose-design.md §111.10).
//!
//! On a host where the hub runs as a system service, a session's processes
//! cannot open the hub's sockets: they ask the broker (`connect NAME`), which
//! opens a fresh connection and moves it into them. A spec `broker:NAME` names
//! such a service wherever a hub socket's path was accepted
//! ([`crate::vdoorbell::PortHandle::open`], `host_socket`, the cold bell).
//!
//! This crate has no broker client of its own (no fd passing outside
//! `localfd`, no dependency beyond `sv-wire`): the program installs one with
//! [`install`] (`wc_broker::client::install_hooks`). A `broker:` spec with
//! nothing installed fails with a message that says so.

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::sync::OnceLock;

/// The prefix of a spec the broker serves.
pub const PREFIX: &str = "broker:";

/// Opens a fresh connection to the broker's local service `name`.
#[cfg(unix)]
pub type Opener = fn(&str) -> std::io::Result<UnixStream>;

#[cfg(unix)]
static OPENER: OnceLock<Opener> = OnceLock::new();

/// Install the process's broker client. The first one stays.
#[cfg(unix)]
pub fn install(f: Opener) {
    let _ = OPENER.set(f);
}

/// Opens region `broker:ID` read-write (a published BAR, §111.10).
#[cfg(unix)]
pub type RegionOpener = fn(&str) -> std::io::Result<std::fs::File>;

#[cfg(unix)]
static REGION_OPENER: OnceLock<RegionOpener> = OnceLock::new();

/// Install the process's region opener, for `region::open_named` of a
/// `broker:ID` name (the GPU BAR's name that sv-hostd hands the ICD).
#[cfg(unix)]
pub fn install_regions(f: RegionOpener) {
    let _ = REGION_OPENER.set(f);
}

/// Open a `broker:ID` region; `None` if `name` is not one.
#[cfg(unix)]
pub fn open_region(name: &str) -> Option<std::io::Result<std::fs::File>> {
    let id = name.strip_prefix(PREFIX)?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(match REGION_OPENER.get() {
        Some(f) => f(name),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("{name}: this process has no broker client installed"),
        )),
    })
}

/// The service name of a `broker:NAME` spec.
pub fn service(spec: &str) -> Option<&str> {
    spec.strip_prefix(PREFIX).filter(|n| !n.is_empty())
}

/// A connection to `spec`: the broker's service for `broker:NAME`, else the
/// socket at the path.
#[cfg(unix)]
pub fn connect(spec: &str) -> std::io::Result<UnixStream> {
    match service(spec) {
        None => UnixStream::connect(spec),
        Some(name) => match OPENER.get() {
            Some(f) => f(name),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("{spec}: this process has no broker client installed"),
            )),
        },
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_a_path_and_a_broker_spec_needs_a_client() {
        assert_eq!(service("broker:wl-hub"), Some("wl-hub"));
        assert_eq!(service("broker:"), None);
        assert_eq!(service("/run/x.sock"), None);
        // Nothing installed in this test binary.
        let e = connect("broker:wl-hub").unwrap_err();
        assert!(e.to_string().contains("no broker client"), "{e}");
    }
}
