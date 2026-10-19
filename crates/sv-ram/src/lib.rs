//! Shared memory that grows and shrinks across the VM boundary
//! (waycompose-design.md §108).
//!
//! * [`policy`]: when to grow and when to shrink;
//! * [`provider`]: the owner's half of the region table, over any substrate;
//! * [`keeper`]: the Windows guest's SYSTEM process that holds every region;
//! * `winsub`: the Windows substrate (the keeper's large-page sections);
//! * `peer`: the Linux host's half (locate, map, verify, answer, let go);
//! * `peerloop`: the peer's loop over a table and its doorbell;
//! * `brokered`: the peer inside the broker daemon, which hands each region
//!   to its user as udmabufs (`udma`).

pub mod keeper;
pub mod policy;
pub mod provider;
pub mod testlayout;
#[cfg(target_os = "linux")]
pub mod simram;
#[cfg(target_os = "linux")]
pub mod peer;
#[cfg(windows)]
pub mod winsub;
#[cfg(windows)]
pub mod winsys;
#[cfg(target_os = "linux")]
pub mod udma;
#[cfg(target_os = "linux")]
pub mod peerloop;
#[cfg(target_os = "linux")]
pub mod brokered;
