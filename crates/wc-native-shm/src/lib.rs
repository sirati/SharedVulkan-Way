//! The shipped GTK Wayland allocator's mapping and broker lease.
//!
//! Memory is acquired before Cairo paints it. The fd passed in wl_shm is the
//! original backing, not a token or a copy. A broker region is opened through
//! this process's authenticated connection and its lease outlives the mapping.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
