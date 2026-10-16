#![cfg_attr(not(windows), allow(dead_code))]
//! The token this launch runs with, said in the log once (waycompose-design.md
//! §91).
//!
//! §90.3's bug was a route nobody had named: every harness click ran elevated
//! and every real double-click runs at medium integrity, and the two get
//! different devices. A log that says which one it was makes every later run
//! self-describing, and lets a harness refuse to call a run "a double-click"
//! when it was not one.

/// `medium integrity (S-1-16-8192), not elevated`, or what could not be read.
pub fn describe() -> String {
    imp::describe()
}

/// A mandatory-label RID as Windows names it.
pub fn integrity_name(rid: u32) -> &'static str {
    match rid {
        0x0000 => "untrusted",
        0x1000 => "low",
        0x2000 => "medium",
        0x2100 => "medium-plus",
        0x3000 => "high",
        0x4000 => "system",
        0x5000 => "protected",
        _ => "unnamed",
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_ELEVATION,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenElevation, TokenIntegrityLevel,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub fn describe() -> String {
        let mut tok: HANDLE = core::ptr::null_mut();
        // SAFETY: a pseudo-handle and an out-pointer to a local.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) } == 0 {
            return "unreadable (OpenProcessToken failed)".into();
        }
        let il = integrity(tok);
        let mut el = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut n = 0u32;
        // SAFETY: the buffer is a TOKEN_ELEVATION of the size passed.
        let elevated = unsafe {
            GetTokenInformation(
                tok,
                TokenElevation,
                (&mut el as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut n,
            )
        } != 0
            && el.TokenIsElevated != 0;
        // SAFETY: opened above.
        unsafe { CloseHandle(tok) };
        match il {
            Some(rid) => format!(
                "{} integrity (S-1-16-{rid}), {}",
                super::integrity_name(rid),
                if elevated { "elevated" } else { "not elevated" }
            ),
            None => format!("integrity unreadable, {}", if elevated { "elevated" } else { "not elevated" }),
        }
    }

    fn integrity(tok: HANDLE) -> Option<u32> {
        // A TOKEN_MANDATORY_LABEL followed by its SID; 64 bytes is ample, and
        // u64s keep the buffer aligned for the struct at its head.
        let mut buf = [0u64; 16];
        let mut n = 0u32;
        // SAFETY: `buf` is writable for the size passed.
        let ok = unsafe {
            GetTokenInformation(
                tok,
                TokenIntegrityLevel,
                buf.as_mut_ptr().cast(),
                size_of_val(&buf) as u32,
                &mut n,
            )
        };
        if ok == 0 {
            return None;
        }
        // SAFETY: the call above filled a TOKEN_MANDATORY_LABEL whose SID points
        // into `buf`, and a mandatory-label SID has one sub-authority, the RID.
        unsafe {
            let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let sid = label.Label.Sid;
            let count = *GetSidSubAuthorityCount(sid);
            if count == 0 {
                return None;
            }
            Some(*GetSidSubAuthority(sid, u32::from(count) - 1))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn describe() -> String {
        "not a Windows token".into()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_labels_a_launch_meets_have_their_windows_names() {
        assert_eq!(super::integrity_name(0x2000), "medium");
        assert_eq!(super::integrity_name(0x3000), "high");
        assert_eq!(super::integrity_name(0x1000), "low");
        assert_eq!(super::integrity_name(0x1234), "unnamed");
    }
}
