//! Compatibility entry point for the old guest-RAM keeper instrument.
//!
//! Region ownership, SID checks, allocation, adoption and crash retention are
//! all implemented by the shared Windows broker. No second keeper pipe or
//! allocator remains here. Production runs the broker as a SYSTEM service.

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// The old `sv-guest ram keeper` test task starts the shared broker in its
/// console mode. The disposable test VM's device ACLs remain untouched.
#[cfg(windows)]
pub fn serve() -> Result<(), String> {
    let code = wc_broker::sys::windows::service::main(&["--console".into(), "--no-acl".into()]);
    if code == std::process::ExitCode::SUCCESS {
        Ok(())
    } else {
        Err("the shared Windows broker exited unsuccessfully".into())
    }
}
