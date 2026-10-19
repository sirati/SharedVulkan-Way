//! How the §108.11 instruments use a region once it is `MAPPED`: one
//! definition for the guest's `sv-guest ram test` and the host's
//! `wc-arena-host ram-peer`, so the two cannot disagree about an offset.
//!
//! unit 0 + 0: the epoch the host filled for; unit 0 + 8: the epoch the guest
//! verified and answered for; every unit + 64: a counter the host keeps
//! incrementing while it maps the region; each unit's second 4 KiB page: the
//! guest's pattern; everything from 8 KiB into each unit: the host's pattern.

pub const OFF_HOST_FILLED: u64 = 0;
pub const OFF_GUEST_DONE: u64 = 8;
pub const OFF_COUNTER: u64 = 64;
pub const GUEST_PAGE: u64 = 4096;
pub const HOST_FROM: u64 = 8192;

pub fn pattern(epoch: u64, off: u64) -> u64 {
    off ^ epoch.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

pub fn guest_pattern(epoch: u64, off: u64) -> u64 {
    !pattern(epoch, off).rotate_left(29)
}
