//! The opaque token a host handler is offered to Linux under. Linux names a
//! handler only by it; the host maps it back to its own identity for the
//! application (an executable path, a bundle id), which never crosses.

/// `h` and 12 hex digits of a 64-bit FNV-1a hash of the identity, which is
/// lowercased first: Windows paths are case-insensitive, and a bundle id is
/// too. Stable across runs and machines, so a Linux desktop entry written
/// for a token stays valid as long as the application is where it was.
pub fn token(identity: &str) -> String {
    let h = identity
        .to_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3));
    format!("h{:012x}", h >> 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wc_hostint::wire::token_ok;

    #[test]
    fn tokens_are_stable_opaque_and_valid() {
        let a = token("C:\\Windows\\System32\\notepad.exe");
        assert_eq!(a, token("c:\\windows\\system32\\NOTEPAD.EXE"));
        assert_ne!(a, token("C:\\Program Files\\Mozilla Firefox\\firefox.exe"));
        assert!(token_ok(&a), "{a}");
        assert_eq!(a.len(), 13);
        assert!(!a.contains("notepad"));
        // Pinned: a token in a Linux desktop entry must survive an upgrade.
        assert_eq!(token("x"), "haf63f54c8602");
    }
}
