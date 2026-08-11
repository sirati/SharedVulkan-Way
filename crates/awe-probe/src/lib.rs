//! The guest-side toolbox: Win32 allocators that produce arena-shaped memory,
//! and the file rendezvous the Linux host half drives.
//!
//! This started as one program (`awe-probe`, step 0) and became a library when
//! a second one needed the same four allocators. Nothing here is generic for
//! its own sake — it is exactly what step 0 measured, factored out so that the
//! GPU-workload probe measures *the same memory* rather than a re-typed
//! approximation of it. The four allocators, their fragmentation, and the fact
//! that the driver imports all of them are established in
//! `docs/measured-gpu-side.md`; a second implementation of them would put that
//! at risk for no gain.
//!
//! Windows-only in substance: [`win`] and [`alloc`] live on
//! `AllocateUserPhysicalPages`, `VirtualLock` and the token API. [`rendezvous`]
//! is plain file I/O and builds anywhere, which keeps the Linux build of this
//! workspace honest about at least part of the crate.

/// Print with a millisecond timestamp and flush.
///
/// Flushing matters more than it looks: these programs are run over ssh with
/// the host half being driven by hand between phases, and a buffered line that
/// arrives after the phase it describes is worse than no line at all.
#[macro_export]
macro_rules! log {
    ($($a:tt)*) => {{
        println!("[{}] {}", $crate::now_ms(), format!($($a)*));
        let _ = <std::io::Stdout as std::io::Write>::flush(&mut std::io::stdout());
    }};
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// A run id that makes every later claim falsifiable: a page that matches was
/// written by *this* run and not left over from the previous one.
pub fn new_run_id() -> u64 {
    (now_ms() as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(17)
        ^ (std::process::id() as u64) << 32
}

pub mod rendezvous;

#[cfg(windows)]
pub mod alloc;
#[cfg(windows)]
pub mod probe;
#[cfg(windows)]
pub mod win;
