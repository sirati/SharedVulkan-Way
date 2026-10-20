//! A userspace DRM render node for the nested KWin, and the tokens that stand
//! in for dma-bufs (docs/waycompose-design.md §104).
//!
//! KWin's nested Wayland backend composites with OpenGL only when the host
//! compositor names a render node it can open. The test host has no GPU and
//! must not get a GPU driver; the GPU is the guest's, reached through
//! `sv-icd`. So the node is a CUSE device served by `sv-drm serve`, it answers
//! the handful of DRM ioctls Mesa's GBM, Zink and KWin issue, and the buffers
//! KWin allocates on it are `sv-icd` allocations in guest-side shared sections,
//! named by [`token`]s.
//!
//! * [`cuse`]: the FUSE kernel protocol over `/dev/cuse`.
//! * [`drm`]: the DRM ioctls, and per-open GEM handles for imported tokens.
//! * [`ns`]: `sv-drm ns`, the namespace in which libdrm sees the node as a
//!   render node on bus "faux".
//! * [`relay`]: the driver-private connect ioctl, through which a sandboxed
//!   client reaches its GPU services, and the registrations behind it.
//! * [`fusenode`]: a test harness serving the node as a FUSE file, without root.

#[cfg(unix)]
pub mod cuse;
#[cfg(unix)]
pub mod drm;
#[cfg(target_os = "linux")]
pub mod ns;
#[cfg(target_os = "linux")]
pub mod token;
#[cfg(target_os = "linux")]
pub mod relay;
#[cfg(target_os = "linux")]
pub mod fusenode;

#[cfg(unix)]
pub mod token_wire;
#[cfg(unix)]
pub mod relay_api;
#[cfg(unix)]
pub mod native_prime;
