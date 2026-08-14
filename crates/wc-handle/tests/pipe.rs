//! §81. `Descriptor::Pipe`, which existed for five stages and did nothing.
//!
//! These are the tests §80.6 said could not be written, and the reason they can
//! be written now is that a pipe is the one kind of `h` whose whole behaviour
//! fits in one process: create it, open it as the peer would, move bytes, and
//! check they arrived on the right side. A section needs two mappings to be
//! interesting and an arena slice needs a hypervisor; a stream needs neither.
//!
//! # Why this is a test *binary* rather than a `mod tests` in `lib.rs`
//!
//! `publish` is a process-global registry, and `publish::tests` asserts as its
//! first line that **nothing has been minted before `set`** — an invariant about
//! the whole process, which its own comment says the tests touching it must not
//! run concurrently against. A `mod pipe_tests` inside the library shares that
//! process and calls `set`, so it broke that assertion from another thread: the
//! first run here cost one failing sibling and no failure of its own.
//!
//! Cargo gives every file under `tests/` its own process, which is why the four
//! binaries beside this one exist. Using the same escape hatch is the fix, and
//! it is the same rule as the object it tests: some things are global, and the
//! way to have two of them is to have two processes.
#![cfg(unix)]

use std::io::{Read, Write};
use wc_handle::{publish, Descriptor, Handle};

/// One directory for the whole binary, taken under a lock so that the tests do
/// not race each other to `set` it.
fn dir() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("wc-pipe-tests-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    publish::set(d.to_str().unwrap(), &format!("{}/", d.display())).unwrap();
    guard
}

#[test]
fn bytes_reach_the_peer_that_the_descriptor_says_reads() {
    let _g = dir();
    // `peer_reads: true` is `wl_data_offer.receive`'s shape: the side handed
    // the descriptor reads, and the creator writes.
    let handle = Handle::create_pipe(true).unwrap();
    assert!(matches!(handle.descriptor(), Descriptor::Pipe { peer_reads: true, .. }));

    let mut peer = handle.open_pipe().unwrap();
    handle.pipe_end().unwrap().write_all(b"text/plain payload").unwrap();

    let mut got = vec![0u8; 18];
    peer.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"text/plain payload");
}

#[test]
fn the_other_direction_is_the_other_direction() {
    let _g = dir();
    // `peer_reads: false` is `wl_data_source.send`'s shape: the side handed the
    // descriptor *writes* the selection into it.
    let handle = Handle::create_pipe(false).unwrap();
    let mut peer = handle.open_pipe().unwrap();
    peer.write_all(b"from the source").unwrap();

    let mut got = vec![0u8; 15];
    handle.pipe_end().unwrap().read_exact(&mut got).unwrap();
    assert_eq!(&got, b"from the source");
}

/// The hazard the `O_RDWR` keepalive exists for, asserted rather than
/// described. A FIFO whose creator held only the read end would block the
/// peer's `open()` for writing until a reader appeared — in a compositor's
/// event loop that is a deadlock, not a slow path.
///
/// No thread and no timeout, deliberately: if the keepalive were wrong this
/// would **hang rather than fail**, which is exactly the failure mode being
/// excluded, and a timeout would only convert it into a flake.
#[test]
fn opening_the_peer_end_does_not_block_on_the_other_end_appearing() {
    let _g = dir();
    let handle = Handle::create_pipe(true).unwrap();
    let peer = handle.open_pipe().unwrap();
    drop(peer);
    // And again, to show the first open/close did not tear the FIFO down.
    let _peer = handle.open_pipe().unwrap();
}

#[test]
fn the_name_dies_with_the_handle() {
    let _g = dir();
    let path = {
        let handle = Handle::create_pipe(true).unwrap();
        let Descriptor::Pipe { name, .. } = handle.descriptor().clone() else {
            panic!("not a pipe")
        };
        assert!(std::path::Path::new(&name).exists(), "the FIFO should be live");
        name
    };
    // Same rule as a section: the creator holds the object, and a peer that has
    // not opened it by then loses the race. Stated in `Descriptor`'s docs since
    // stage 1; now there is a stream it applies to.
    assert!(!std::path::Path::new(&path).exists(), "dropping should unlink it");
}

#[test]
fn a_section_is_not_a_stream_and_says_so() {
    let _g = dir();
    let section = Handle::from_descriptor(Descriptor::Section {
        name: "wc-not-a-pipe".into(),
        size: 4096,
    });
    let err = section.open_pipe().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    let pipe = Handle::create_pipe(true).unwrap();
    let err = pipe.write_at(0, b"x").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "a stream has no offsets");
}
