//! The two halves meet through files on the virtio-fs share.
//!
//! Not a design choice worth defending in a product — it is the cheapest thing
//! that already exists between these two machines, and it lets the host half be
//! driven by hand between phases while the guest half waits. `Z:\probe` in the
//! guest is `/srv/winshare/probe` on the host.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

pub fn write_file(path: &Path, content: &str) {
    if let Err(e) = fs::write(path, content) {
        crate::log!("failed to write {path:?}: {e}");
    }
}

/// Wait for the other side to leave a file, up to `secs`.
///
/// An empty read is a file the other side has created but not finished writing
/// — over virtio-fs that window is real — so an empty file is treated as "not
/// yet" rather than as an answer.
pub fn wait_for(path: &Path, secs: u64) -> Option<String> {
    crate::log!("waiting for {path:?} (up to {secs}s)");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        if let Ok(s) = fs::read_to_string(path)
            && !s.trim().is_empty()
        {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// Pull `key=<hex>` out of a status file.
///
/// Returns `None` rather than a default on a parse failure, and callers are
/// expected not to substitute zero: a run once reported `0/262144` because a
/// flag file had been written with a literal `n` instead of a newline, the
/// parse failed, and the guest dutifully searched for a stamp of zero
/// (`docs/measured-gpu-side.md`, "A false negative worth recording").
pub fn parse_hex_field(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        if let Some(v) = line.trim().strip_prefix(&format!("{key}=")) {
            return u64::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_found_and_a_missing_one_is_not_zero() {
        let s = "run_id=0011223344556677\nstamp=00000000deadbeef\n";
        assert_eq!(parse_hex_field(s, "stamp"), Some(0xdead_beef));
        assert_eq!(parse_hex_field(s, "run_id"), Some(0x0011_2233_4455_6677));
        assert_eq!(parse_hex_field(s, "absent"), None);
    }

    /// The exact shape that produced the false negative: a trailing character
    /// where a newline should be. It must fail to parse, not parse as zero.
    #[test]
    fn a_malformed_value_is_none_rather_than_zero() {
        assert_eq!(parse_hex_field("stamp=abcd2222n", "stamp"), None);
        assert_eq!(parse_hex_field("stamp=\n", "stamp"), None);
    }
}
