//! `awe-probe` is the guest half of step 0 and is Windows-only: it lives on
//! `AllocateUserPhysicalPages`, `VirtualLock` and the Win32 token API, none of
//! which have a meaningful Linux counterpart.
//!
//! It stays a member of the workspace anyway — a crate that is only ever built
//! by hand is a crate that quietly stops compiling — so the real body is behind
//! `cfg(windows)` and the Linux build produces a binary that explains itself.
//!
//!   cargo build -p awe-probe --release --target x86_64-pc-windows-gnu

#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    probe::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "awe-probe runs inside the Windows guest — the GPU side. Build it with\n  \
         cargo build -p awe-probe --release --target x86_64-pc-windows-gnu\n\
         and publish it to the share the guest sees (see docs/measured-gpu-side.md)."
    );
    std::process::exit(2);
}
