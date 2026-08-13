//! Various utilities functions and types

mod geometry;
pub mod os;
pub mod signaling;

#[cfg(feature = "x11rb_event_source")]
pub mod x11rb;

pub(crate) mod ids;
pub mod user_data;

pub(crate) mod alive_tracker;
pub use self::alive_tracker::IsAlive;

#[cfg(feature = "wayland_frontend")]
pub(crate) mod iter;

// `DeviceFd` is an open handle on a *device node* — `/dev/dri/cardN` and the
// like — and everything that touches it (`backend::drm`, `backend::allocator`'s
// dumb and gbm allocators) is already gated to Linux backends. It is Unix by
// subject matter, not by accident: it wraps a `RawFd`, `dev_path()` reads
// `/proc/self/fd`, and there is no Windows analogue because there is no Windows
// device node to open.
#[cfg(unix)]
mod fd;
#[cfg(unix)]
pub use fd::*;

mod sealed_file;
pub use sealed_file::SealedFile;

#[cfg(feature = "wayland_frontend")]
pub(crate) use self::geometry::Client;
pub use self::geometry::{
    Buffer, Coordinate, Logical, Physical, Point, Raw, Rectangle, Scale, Size, Transform,
};

mod serial;
pub use serial::*;

mod clock;
pub use clock::*;

#[cfg(feature = "wayland_frontend")]
pub(crate) mod hook;
#[cfg(feature = "wayland_frontend")]
pub use hook::HookId;

/// This resource is not managed by Smithay
#[derive(Debug)]
pub struct UnmanagedResource;

impl std::fmt::Display for UnmanagedResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This resource is not managed by Smithay.")
    }
}

impl std::error::Error for UnmanagedResource {}

/// This resource has been destroyed and can no longer be used.
#[derive(Debug)]
pub struct DeadResource;

impl std::fmt::Display for DeadResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This resource has been destroyed and can no longer be used.")
    }
}

impl std::error::Error for DeadResource {}
