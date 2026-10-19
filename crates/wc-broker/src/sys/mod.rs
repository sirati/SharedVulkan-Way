//! The platform halves (§111.5).

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod unix;
