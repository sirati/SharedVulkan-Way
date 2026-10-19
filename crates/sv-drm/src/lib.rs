//! A userspace DRM render node for the nested KWin, and the tokens that stand
//! in for dma-bufs (docs/waycompose-design.md §104).
//!
//! KWin's nested Wayland backend composites with OpenGL only when the host
//! compositor names a render node it can open. testhost's host has no GPU and
//! must not get a GPU driver; the GPU is the guest's, reached through
//! `sv-icd`. So the node is a CUSE device served by `sv-drm serve`, it answers
//! the handful of DRM ioctls Mesa's GBM, Zink and KWin issue, and the buffers
//! KWin allocates on it are `sv-icd` allocations in guest-side shared sections,
//! named by [`token`]s.

#[cfg(target_os = "linux")]
pub mod token;
