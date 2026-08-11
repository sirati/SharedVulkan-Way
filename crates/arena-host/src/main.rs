//! `arena-host` is the host half of step 0 and is Unix-only: it reaches guest
//! RAM through `/proc/<qemu-pid>/fd/<n>` and `mmap(MAP_SHARED)`, which is not a
//! thing on the other side of the boundary.
//!
//! The direction-agnosticism of `design.md` §5.1 says the roles can swap — but
//! swapping them means a *different* attacher, not this one ported, so the
//! honest arrangement is a `cfg` gate rather than a portability layer.

#[cfg(unix)]
mod hostside;

#[cfg(unix)]
fn main() {
    hostside::run();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("arena-host runs on the VM host, which is Linux here.");
    std::process::exit(2);
}
