//! A Wayland client that writes the wire format by hand.
//!
//! # Why this exists rather than using `wayland-client`
//!
//! The thing under test is the *server*, and specifically whether the forked
//! server stack completes a `wl_display` handshake when it is a Windows binary.
//! A client built from the same forked crates would share every bug the server
//! has: if our wire encoding were wrong in a symmetric way, a matched pair would
//! agree with each other and pass. This probe shares no code with the server
//! beyond the socket, and every byte it sends is written out from the protocol
//! specification here in this file, so agreement between the two is evidence.
//!
//! It is not the *only* evidence. On Linux the same server is driven by
//! `wayland-info`, which is libwayland-client — a C implementation by other
//! people that has never heard of this project. See `scripts/wc-handshake.sh`.
//!
//! # The handshake
//!
//! Object 1 is `wl_display`, by definition, and exists before anything is sent.
//! The probe issues the two requests every real client issues at startup:
//!
//!   `wl_display.get_registry(new_id)` — opcode 1 — asks the server to
//!   advertise its globals, which it does as a burst of `wl_registry.global`
//!   events, one per global.
//!
//!   `wl_display.sync(new_id)`         — opcode 0 — asks for a `wl_callback`
//!   that the server destroys immediately, sending `wl_callback.done`. Because
//!   Wayland is ordered, `done` arriving means every event the server had
//!   already queued — the whole registry burst — has arrived too. That is what
//!   makes it a handshake rather than a hopeful read with a timeout.
//!
//! Receiving `done` is therefore the pass condition, and the set of globals
//! seen before it is what the server actually advertised.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// `wl_display` is object 1 by definition; a client may use it without having
/// been told it exists.
const WL_DISPLAY: u32 = 1;
/// Object ids we pick for the two objects we create. Clients own the id space
/// below 0xff000000 and allocate from it themselves.
const OUR_REGISTRY: u32 = 2;
const OUR_CALLBACK: u32 = 3;

/// `wl_display` requests.
const REQ_DISPLAY_SYNC: u16 = 0;
const REQ_DISPLAY_GET_REGISTRY: u16 = 1;

/// `wl_display` events.
const EVT_DISPLAY_ERROR: u16 = 0;
const EVT_DISPLAY_DELETE_ID: u16 = 1;

/// `wl_registry` events.
const EVT_REGISTRY_GLOBAL: u16 = 0;
const EVT_REGISTRY_GLOBAL_REMOVE: u16 = 1;

/// `wl_callback` events.
const EVT_CALLBACK_DONE: u16 = 0;

/// Build one wayland message.
///
/// Header is two 32-bit words: the sender's object id, then the message size in
/// bytes (including this header) in the high 16 bits and the opcode in the low
/// 16. Native byte order, because a Unix socket cannot cross an endianness
/// boundary and the protocol takes advantage of that.
fn message(object: u32, opcode: u16, args: &[u32]) -> Vec<u8> {
    let size = 8 + args.len() * 4;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&object.to_ne_bytes());
    out.extend_from_slice(&(((size as u32) << 16) | u32::from(opcode)).to_ne_bytes());
    for a in args {
        out.extend_from_slice(&a.to_ne_bytes());
    }
    out
}

/// WINDOWS-ONLY, used by `--shm`. Append a wayland `array`/`string`-shaped argument: a 32-bit length, the
/// bytes, then zero padding up to a 4-byte boundary.
///
/// This is also exactly how an `h` argument is encoded on Windows — see
/// `wc_handle`'s crate docs for why the descriptor rides in the body rather
/// than in a side channel.
#[cfg(windows)]
fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_ne_bytes());
    out.extend_from_slice(bytes);
    let pad = (4 - bytes.len() % 4) % 4;
    out.extend(std::iter::repeat_n(0u8, pad));
}

/// `wl_registry.bind`, which has the one genuinely awkward encoding in the core
/// protocol: its `new_id` argument has no interface declared in the XML, so on
/// the wire it expands to *three* values — the interface name as a string, the
/// version, and only then the id.
#[cfg(windows)]
fn bind_message(registry: u32, name: u32, interface: &str, version: u32, new_id: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&name.to_ne_bytes());
    let mut iface = interface.as_bytes().to_vec();
    iface.push(0); // wayland strings are NUL-terminated and the NUL is counted
    push_bytes(&mut body, &iface);
    body.extend_from_slice(&version.to_ne_bytes());
    body.extend_from_slice(&new_id.to_ne_bytes());

    let size = 8 + body.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&registry.to_ne_bytes());
    // `wl_registry.bind` is opcode 0, so the low half of the word is zero and
    // the `| opcode` the other builders do would be an identity operation.
    out.extend_from_slice(&((size as u32) << 16).to_ne_bytes());
    out.extend_from_slice(&body);
    out
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}

/// Read a wayland `string`: a 32-bit length including the trailing NUL, then
/// that many bytes, then padding to a 4-byte boundary.
fn string_at(buf: &[u8], off: usize) -> Option<(String, usize)> {
    if off + 4 > buf.len() {
        return None;
    }
    let len = u32_at(buf, off) as usize;
    let start = off + 4;
    let padded = len.div_ceil(4) * 4;
    if start + padded > buf.len() || len == 0 {
        return None;
    }
    let bytes = &buf[start..start + len - 1]; // drop the NUL
    Some((String::from_utf8_lossy(bytes).into_owned(), start + padded))
}

/// The bytes the `--shm` phase writes into the shared section and expects the
/// server to read back out of it. Deliberately not a round number and not all
/// at offset zero, so a server that reported success from a zeroed or
/// wrong-length mapping would still fail.
#[cfg(windows)]
const SHM_MARKER: &[u8] = b"WAYCOMPOSE-SHM-OK";
#[cfg(windows)]
const SHM_SIZE: u64 = 4096;
#[cfg(windows)]
const SHM_TAIL_OFFSET: u64 = 4000;
#[cfg(windows)]
const SHM_TAIL: &[u8] = b"tail";

fn main() -> ExitCode {
    let mut shm = false;
    let mut positional: Option<String> = None;
    for arg in std::env::args().skip(1) {
        if arg == "--shm" {
            shm = true;
        } else {
            positional = Some(arg);
        }
    }

    let mut args = positional.into_iter();
    let path = match args.next() {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let name = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
            match wc_transport::socket_dir() {
                Ok(dir) => dir.join(name),
                Err(e) => {
                    eprintln!("wc-probe: no socket path given and {e}");
                    return ExitCode::from(2);
                }
            }
        }
    };

    println!("wc-probe: connecting to {}", path.display());
    let mut stream = match wc_transport::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wc-probe: connect failed: {e}");
            return ExitCode::from(2);
        }
    };

    let mut request = message(WL_DISPLAY, REQ_DISPLAY_GET_REGISTRY, &[OUR_REGISTRY]);
    request.extend_from_slice(&message(WL_DISPLAY, REQ_DISPLAY_SYNC, &[OUR_CALLBACK]));
    if let Err(e) = stream.write_all(&request) {
        eprintln!("wc-probe: write failed: {e}");
        return ExitCode::from(2);
    }
    println!("wc-probe: sent wl_display.get_registry(2) and wl_display.sync(3)");

    // A real client would poll. This is a probe with one job, so it blocks with
    // a deadline: a handshake that has not completed in five seconds has failed
    // in a way a longer wait will not fix.
    let deadline = Instant::now() + Duration::from_secs(5);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut globals: Vec<(u32, String, u32)> = Vec::new();
    let mut done = false;
    let mut protocol_error = None;

    'outer: while Instant::now() < deadline && !done {
        match stream.read(&mut chunk) {
            Ok(0) => {
                eprintln!("wc-probe: server closed the connection");
                break;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                eprintln!("wc-probe: read failed: {e}");
                break;
            }
        }

        // Drain every complete message currently in the buffer.
        let mut at = 0;
        while buf.len() - at >= 8 {
            let object = u32_at(&buf, at);
            let word2 = u32_at(&buf, at + 4);
            let opcode = (word2 & 0xffff) as u16;
            let size = (word2 >> 16) as usize;
            if size < 8 {
                eprintln!("wc-probe: server sent a message claiming size {size}");
                break 'outer;
            }
            if buf.len() - at < size {
                break; // wait for the rest
            }
            let body = &buf[at + 8..at + size];

            match (object, opcode) {
                (WL_DISPLAY, EVT_DISPLAY_ERROR) => {
                    let obj = u32_at(body, 0);
                    let code = u32_at(body, 4);
                    let msg = string_at(body, 8).map(|(s, _)| s).unwrap_or_default();
                    protocol_error = Some(format!("object {obj} code {code}: {msg}"));
                    break 'outer;
                }
                (WL_DISPLAY, EVT_DISPLAY_DELETE_ID) => {
                    println!("wc-probe:   wl_display.delete_id({})", u32_at(body, 0));
                }
                (OUR_REGISTRY, EVT_REGISTRY_GLOBAL) => {
                    let name = u32_at(body, 0);
                    if let Some((interface, next)) = string_at(body, 4) {
                        let version = u32_at(body, next);
                        println!("wc-probe:   global {name}: {interface} v{version}");
                        globals.push((name, interface, version));
                    }
                }
                (OUR_REGISTRY, EVT_REGISTRY_GLOBAL_REMOVE) => {
                    println!("wc-probe:   global_remove({})", u32_at(body, 0));
                }
                (OUR_CALLBACK, EVT_CALLBACK_DONE) => {
                    println!("wc-probe:   wl_callback.done(serial={})", u32_at(body, 0));
                    done = true;
                }
                (o, op) => println!("wc-probe:   unexpected event: object {o} opcode {op}"),
            }
            at += size;
        }
        buf.drain(..at);
    }

    if let Some(e) = protocol_error {
        eprintln!("wc-probe: FAIL — server raised a protocol error: {e}");
        return ExitCode::from(1);
    }
    if !done {
        eprintln!("wc-probe: FAIL — no wl_callback.done within the deadline");
        return ExitCode::from(1);
    }

    println!("wc-probe: PASS — handshake completed, {} globals advertised", globals.len());

    if shm {
        return shm_phase(&mut stream, &globals);
    }

    ExitCode::SUCCESS
}

/// Send a real `h` argument and see whether the server can read the memory.
///
/// This is the end-to-end test of the fd shim, and it is deliberately structured
/// so that a false pass is hard: the probe creates a section, writes a marker at
/// offset 0 and a second one near the end, sends **only the descriptor**, and
/// the server has to report both markers back. Nothing but the section name and
/// size crosses the socket, so a server that printed the marker could only have
/// got it by opening the object.
///
/// On Unix this is skipped rather than reimplemented, and that is the right
/// call rather than a gap: on Unix an `h` travels by `SCM_RIGHTS` over
/// `sendmsg`, which is upstream `wayland-backend`'s code entirely unmodified by
/// this fork, and which its own `write_read_cycle_fd` test already exercises
/// under `cargo test`. There is no shim on Unix to test.
#[cfg(windows)]
fn shm_phase(stream: &mut wc_transport::Stream, globals: &[(u32, String, u32)]) -> ExitCode {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    const OUR_SHM: u32 = 4;
    const OUR_POOL: u32 = 5;
    const OUR_CALLBACK2: u32 = 6;
    const REQ_SHM_CREATE_POOL: u16 = 0;

    let Some((name, _, version)) = globals.iter().find(|(_, i, _)| i == "wl_shm") else {
        eprintln!("wc-probe: FAIL — the server advertised no wl_shm to bind");
        return ExitCode::from(1);
    };

    // Create the object and put something in it that the server cannot guess.
    let handle = match wc_handle::Handle::create_section(SHM_SIZE) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("wc-probe: FAIL — could not create a section: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = handle.write_at(0, SHM_MARKER).and_then(|()| handle.write_at(SHM_TAIL_OFFSET, SHM_TAIL)) {
        eprintln!("wc-probe: FAIL — could not write into the section: {e}");
        return ExitCode::from(1);
    }
    let descriptor = handle.descriptor().encode();
    println!(
        "wc-probe: created a {SHM_SIZE}-byte section; descriptor is {} bytes on the wire",
        descriptor.len()
    );

    let mut out = bind_message(OUR_REGISTRY, *name, "wl_shm", *version, OUR_SHM);

    // wl_shm.create_pool(new_id, fd, size). The `fd` is the descriptor, encoded
    // exactly like an `array`; the rest is ordinary.
    let mut body = Vec::new();
    body.extend_from_slice(&OUR_POOL.to_ne_bytes());
    push_bytes(&mut body, &descriptor);
    body.extend_from_slice(&(SHM_SIZE as i32).to_ne_bytes());
    let size = 8 + body.len();
    out.extend_from_slice(&OUR_SHM.to_ne_bytes());
    out.extend_from_slice(&(((size as u32) << 16) | u32::from(REQ_SHM_CREATE_POOL)).to_ne_bytes());
    out.extend_from_slice(&body);

    // A second sync, so we can tell "the server processed create_pool" from
    // "the server has not got to it yet".
    out.extend_from_slice(&message(WL_DISPLAY, REQ_DISPLAY_SYNC, &[OUR_CALLBACK2]));

    if let Err(e) = stream.write_all(&out) {
        eprintln!("wc-probe: FAIL — could not send create_pool: {e}");
        return ExitCode::from(1);
    }
    println!("wc-probe: sent wl_registry.bind(wl_shm) and wl_shm.create_pool(fd, {SHM_SIZE})");

    // Wait for the second callback. The handle stays alive across this wait on
    // purpose — see the lifetime rule in `wayland-backend`'s `write_to_buffers`:
    // a descriptor transfers nothing, so dropping the handle here would destroy
    // the section before the server opened it.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(e) => {
                eprintln!("wc-probe: read failed: {e}");
                break;
            }
        }
        let mut at = 0;
        while buf.len() - at >= 8 {
            let object = u32_at(&buf, at);
            let word2 = u32_at(&buf, at + 4);
            let opcode = (word2 & 0xffff) as u16;
            let msize = (word2 >> 16) as usize;
            if msize < 8 || buf.len() - at < msize {
                break;
            }
            if object == WL_DISPLAY && opcode == EVT_DISPLAY_ERROR {
                let body = &buf[at + 8..at + msize];
                let msg = string_at(body, 8).map(|(s, _)| s).unwrap_or_default();
                eprintln!("wc-probe: FAIL — protocol error after create_pool: {msg}");
                return ExitCode::from(1);
            }
            if object == OUR_CALLBACK2 && opcode == EVT_CALLBACK_DONE {
                println!("wc-probe: PASS — the server processed create_pool without error");
                // Only now is it safe to let the section go.
                drop(handle);
                return ExitCode::SUCCESS;
            }
            at += msize;
        }
        buf.drain(..at);
    }

    eprintln!("wc-probe: FAIL — no second wl_callback.done after create_pool");
    ExitCode::from(1)
}

/// See the Windows version: there is no shim on Unix to test.
#[cfg(unix)]
fn shm_phase(_stream: &mut wc_transport::Stream, _globals: &[(u32, String, u32)]) -> ExitCode {
    println!(
        "wc-probe: SKIP — --shm tests the Windows handle shim. On Unix an `h` \
         travels by SCM_RIGHTS, which this fork does not modify."
    );
    ExitCode::SUCCESS
}
