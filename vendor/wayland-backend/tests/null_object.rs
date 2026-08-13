// UNIX-ONLY, AND THE REASON IS THE TEST'S SETUP RATHER THAN WHAT IT TESTS.
//
// What is under test — that `send_request` on a null object id returns
// `InvalidId` before touching the wire — is entirely platform-independent, and
// the Windows build of that code path is identical.
//
// What is not portable is `UnixStream::pair()`. There is no `socketpair(2)` for
// Windows AF_UNIX: `afunix.sys` supports `bind`/`listen`/`connect` on a
// filesystem path and nothing else, and `uds_windows` exposes no `pair`
// accordingly. Producing a connected pair there means binding a real socket in
// a real directory and connecting to it — a filesystem dependency that would
// make this unit test an integration test, to cover a code path that has no
// platform-specific arm.
//
// Note also that `cargo test` cannot run for `x86_64-pc-windows-gnu` in this
// project at all (docs/design.md §4.1): the tests run on Linux and the Windows
// behaviour is covered by `scripts/wc-handshake.sh` and `scripts/wc-xdg.sh`
// under Wine. So the gate this `cfg` unblocks is `cargo clippy --all-targets
// --target x86_64-pc-windows-gnu`, which compiles the test without ever being
// able to execute it.
#![cfg(unix)]

use std::os::unix::net::UnixStream;
use wayland_backend::protocol::Message;

#[test]
fn rs_client_null_object_request() {
    use wayland_backend::rs::client::{Backend, InvalidId, ObjectId};

    let (sock, _sock2) = UnixStream::pair().unwrap();
    let backend = Backend::connect(sock).unwrap();
    let null_obj = ObjectId::null();
    let message = Message { sender_id: null_obj, opcode: 42, args: Default::default() };
    assert_eq!(backend.send_request(message, None, None), Err(InvalidId));
}

#[cfg(feature = "client_system")]
#[test]
fn sys_client_null_object_request() {
    use wayland_backend::sys::client::{Backend, InvalidId, ObjectId};

    let (sock, _sock2) = UnixStream::pair().unwrap();
    let backend = Backend::connect(sock).unwrap();
    let null_obj = ObjectId::null();
    let message = Message { sender_id: null_obj, opcode: 42, args: Default::default() };
    assert_eq!(backend.send_request(message, None, None), Err(InvalidId));
}
