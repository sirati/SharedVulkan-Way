//! `wc-broker serve`: the broker as a process of its own, on Linux or macOS
//! (§111.10). The server role on a Linux machine (the stand-in for a guest's
//! privileged service in the harnesses): it signs boot records for the
//! caller's identity with the installations' keys in `--keys DIR`, lends
//! devices, and connects local services.
//!
//! ```text
//! wc-broker serve --socket PATH [--keys DIR] [--device NAME=PATH]... [--service NAME=PATH]...
//! ```
//!
//! The client role's broker is not this: it runs inside the session service
//! (`wc-arena-host sessiond --broker`), which publishes the BAR files for each
//! session's mapped user.

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> std::process::ExitCode {
    use wc_broker::sys::unix::{DevicePath, Unix};
    use wc_broker::{Allow, DeviceSpec};
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: wc-broker serve --socket PATH [--keys DIR] [--device NAME=PATH]... [--service NAME=PATH]...";
    if argv.first().map(String::as_str) != Some("serve") {
        eprintln!("{usage}");
        return std::process::ExitCode::from(2);
    }
    let (mut socket, mut keys, mut devices, mut services) = (None, None, Vec::new(), Vec::new());
    let mut it = argv[1..].iter();
    while let Some(k) = it.next() {
        let Some(v) = it.next() else {
            eprintln!("{k} needs a value");
            return std::process::ExitCode::from(2);
        };
        let pair = || v.split_once('=').map(|(n, p)| DevicePath { name: n.to_string(), path: p.into() });
        match k.as_str() {
            "--socket" => socket = Some(v.clone()),
            "--keys" => keys = Some(std::path::PathBuf::from(v)),
            "--device" | "--service" => match pair() {
                Some(d) if k == "--device" => devices.push(d),
                Some(d) => services.push(d),
                None => {
                    eprintln!("{k} NAME=PATH");
                    return std::process::ExitCode::from(2);
                }
            },
            _ => {
                eprintln!("{usage}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    let Some(socket) = socket else {
        eprintln!("{usage}");
        return std::process::ExitCode::from(2);
    };
    let spec = |d: &DevicePath| DeviceSpec { name: d.name.clone(), allow: Allow::AnyUser };
    let config = wc_broker::server::Config {
        devices: devices.iter().map(spec).collect(),
        services: services.iter().map(spec).collect(),
        keys,
        release: wc_broker::Release::OwnerFree,
        budget: None,
        run: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64),
    };
    let unix = match Unix::bind(&socket, devices) {
        Ok(u) => u.with_services(services),
        Err(e) => {
            eprintln!("wc-broker: {socket}: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    match wc_broker::server::Broker::new(unix, config).serve() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wc-broker: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() -> std::process::ExitCode {
    eprintln!("wc-broker serve is the Unix broker; on Windows the broker is the waycompose service");
    std::process::ExitCode::from(2)
}
