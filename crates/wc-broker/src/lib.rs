//! wc-broker: the one privileged component per machine (waycompose-design.md
//! §111, docs/security-model.md R44).
//!
//! Waycompose has two roles. The **server** side shows and renders: the
//! compositor and the GPU server, on a Windows or macOS VM, on the real Mac,
//! or on Linux. The **client** side runs the Linux applications, `wc-proxy`
//! and `sv-icd`. On both, something must own the resources one machine has
//! only one of, give them only to the user they belong to, and clean up after
//! a process that dies holding them. That is this crate, the same code on
//! every platform:
//!
//! * [`core`]: the rules, as a state machine with no operating system in it;
//! * [`proto`]: the line protocol on the broker's local socket or pipe;
//! * [`server`]: the serving loop over a [`server::Platform`];
//! * `sys`: the platforms: Unix (Linux, macOS), Windows;
//! * [`client`]: what a compositor, GPU process or proxy calls;
//! * `daemon`: the `wc-broker` program on Unix (the macOS LaunchDaemon, §111.9).
//!
//! **It is a control plane only.** A device is opened by the broker and its
//! handle moved into the process that asked; a region's handle is copied into
//! it. From then on the doorbells, rings and regions are that process's own,
//! exactly as when it opened them itself: nothing on a data path passes
//! through the broker, and nothing is copied.

pub mod client;
pub mod core;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod daemon;
pub mod proto;
pub mod server;
pub mod sys;

pub use core::{Allow, DeviceSpec, Peer, Release, User};
