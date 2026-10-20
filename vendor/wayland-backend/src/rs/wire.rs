//! Types and routines used to manipulate arguments from the wire format

use std::collections::VecDeque;
use std::ffi::CStr;

// WAYCOMPOSE FORK: was `std::os::unix::io::{BorrowedFd, OwnedFd, RawFd}`.
//
// This file is where the two jobs `crate::os` distinguishes actually sit next
// to each other, so the import list is worth reading slowly. `ProtoFd` and
// `RawProtoFd` are the `h` argument — the thing whose encoding
// `docs/waycompose-design.md` §72.3 makes a per-transport choice. `OwnedFd` and
// `BorrowedFd` here are the `SCM_RIGHTS` queues and the `dup()` that fills them:
// real kernel descriptors, and nothing else on any platform.
use crate::os::{FdEncoding, OwnedFd, ProtoFd, RawProtoFd, RawWireFd, WireFd};
#[cfg(unix)]
use crate::os::BorrowedFd;

use crate::protocol::{Argument, ArgumentType, Message};

use smallvec::SmallVec;

// A BAR connection cannot turn a guest-supplied pathname into a host open.
fn descriptor_allowed(encoding: FdEncoding, descriptor: &wc_handle::Descriptor) -> bool {
    match encoding {
        FdEncoding::InBandArena(expected) => matches!(descriptor,
            wc_handle::Descriptor::ArenaSlice { arena, len, .. } if *arena == expected && *len > 0),
        _ => true,
    }
}

/// Error generated when trying to serialize a message into buffers
#[derive(Debug)]
pub enum MessageWriteError {
    /// The buffer is too small to hold the message contents
    BufferTooSmall,
    /// The message contains a FD that could not be dup-ed
    ///
    /// WAYCOMPOSE FORK: unreachable on Windows, where "duplicating" a handle is
    /// an `Arc` clone and cannot fail. The variant stays because it is public
    /// API and because the Unix path still raises it.
    #[cfg_attr(windows, allow(dead_code))]
    DupFdFailed(std::io::Error),
    /// The `h` argument is the wrong kind for this connection's dialect.
    ///
    /// ADDED BY §79. An in-band peer needs a **name** and an out-of-band peer
    /// needs an **open descriptor**, and neither can be produced from the other
    /// down here: naming an arbitrary fd means creating a kernel object and
    /// copying into it, which is a policy decision belonging to whoever owns the
    /// bytes — `wc-proxy` for a relayed pool, smithay for a keymap. So this is
    /// raised rather than papered over, and the caller that picked the wrong
    /// arm is the one that has to be fixed.
    WrongDialect {
        /// What this connection speaks.
        wanted: crate::os::FdEncoding,
    },
}

impl std::error::Error for MessageWriteError {}

impl std::fmt::Display for MessageWriteError {
    #[cfg_attr(unstable_coverage, coverage(off))]
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> Result<(), ::std::fmt::Error> {
        match self {
            Self::BufferTooSmall => {
                f.write_str("The provided buffer is too small to hold message content.")
            }
            Self::DupFdFailed(e) => {
                write!(
                    f,
                    "The message contains a file descriptor that could not be dup()-ed ({e})."
                )
            }
            Self::WrongDialect { wanted } => {
                write!(
                    f,
                    "The message contains an `h` argument of the wrong kind for a {wanted:?} \
                     connection."
                )
            }
        }
    }
}

/// Error generated when trying to deserialize a message from buffers
#[derive(Debug, Clone)]
pub enum MessageParseError {
    /// The message references a FD but the buffer FD is empty
    ///
    /// WAYCOMPOSE FORK: unreachable on Windows. There is no out-of-band fd
    /// buffer to be empty — a missing descriptor shows up as `MissingData` (the
    /// body was short) or `Malformed` (the bytes were there and were wrong).
    #[cfg_attr(windows, allow(dead_code))]
    MissingFD,
    /// More data is needed to deserialize the message
    MissingData,
    /// The message is malformed and cannot be parsed
    Malformed,
}

impl std::error::Error for MessageParseError {}

impl std::fmt::Display for MessageParseError {
    #[cfg_attr(unstable_coverage, coverage(off))]
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> Result<(), ::std::fmt::Error> {
        match *self {
            Self::MissingFD => {
                f.write_str("The message references a FD but the buffer FD is empty.")
            }
            Self::MissingData => f.write_str("More data is needed to deserialize the message"),
            Self::Malformed => f.write_str("The message is malformed and cannot be parsed"),
        }
    }
}

/// Serialize the contents of this message into provided buffers
///
/// Returns the number of elements written in each buffer
///
/// Any serialized Fd will be `dup()`-ed in the process
///
/// WAYCOMPOSE FORK: `encoding` is the parameter that used to be a `#[cfg]`. See
/// `crate::os::FdEncoding` and `docs/waycompose-design.md` §79.
pub fn write_to_buffers(
    msg: &Message<u32, RawProtoFd>,
    payload: &mut [u8],
    fds: &mut Vec<OwnedFd>,
    encoding: FdEncoding,
) -> Result<usize, MessageWriteError> {
    let orig_payload_len = payload.len();
    // Helper function to write a u32 or a RawProtoFd to its buffer
    fn write_buf(u: u32, payload: &mut [u8]) -> Result<&mut [u8], MessageWriteError> {
        if payload.len() >= 4 {
            let (head, tail) = payload.split_at_mut(4);
            head.copy_from_slice(&u.to_ne_bytes());
            Ok(tail)
        } else {
            Err(MessageWriteError::BufferTooSmall)
        }
    }

    // Helper function to write byte arrays in payload
    fn write_array_to_payload<'a>(
        array: &[u8],
        payload: &'a mut [u8],
    ) -> Result<&'a mut [u8], MessageWriteError> {
        // size header
        let payload = write_buf(array.len() as u32, payload)?;

        // Handle padding
        let len = next_multiple_of(array.len(), 4);

        if payload.len() < len {
            return Err(MessageWriteError::BufferTooSmall);
        }

        let (buffer_slice, rest) = payload.split_at_mut(len);
        buffer_slice[..array.len()].copy_from_slice(array);
        Ok(rest)
    }

    let free_size = payload.len();
    if free_size < 2 * 4 {
        return Err(MessageWriteError::BufferTooSmall);
    }

    let (header, mut payload) = payload.split_at_mut(2 * 4);

    // write the contents in the buffer
    for arg in &msg.args {
        payload = match *arg {
            Argument::Int(i) => write_buf(i as u32, payload)?,
            Argument::Uint(u) => write_buf(u, payload)?,
            Argument::Fixed(f) => write_buf(f as u32, payload)?,
            Argument::Str(Some(ref s)) => write_array_to_payload(s.as_bytes_with_nul(), payload)?,
            Argument::Str(None) => write_array_to_payload(&[], payload)?,
            Argument::Object(o) => write_buf(o, payload)?,
            Argument::NewId(n) => write_buf(n, payload)?,
            Argument::Array(ref a) => write_array_to_payload(a, payload)?,

            // WAYCOMPOSE FORK: this arm is where an `h` argument leaves the
            // process, and §79 made it a runtime choice. It was two `#[cfg]`
            // arms; it is now two `match` arms, and both compile on both
            // platforms.
            //
            // OUT OF BAND is libwayland's: the fd is `dup()`-ed and queued for
            // the `SCM_RIGHTS` control message that `Socket::send_msg` will
            // attach, and it occupies **zero** bytes of message body. Note
            // `payload` is returned unchanged.
            //
            // IN BAND is what a peer reached through `wc-proxy` speaks, and on
            // Windows it is the only thing there is: the typed descriptor goes
            // in the body, encoded exactly like an `array` — a `u32` length
            // followed by the bytes, padded to four. The message header's size
            // field accounts for it automatically, and the parser on the other
            // side reads it back positionally from the same signature, so
            // nothing else in the protocol machinery notices.
            //
            // **The lifetime rule the in-band form creates, stated plainly,
            // because it is the one thing about it that a caller must know:**
            // unlike `SCM_RIGHTS`, sending a descriptor transfers nothing. The
            // kernel does not take a reference; the receiver holds only a *name*
            // until it opens the object. So the process that created the object
            // must keep its `Handle` alive for at least as long as the protocol
            // object the handle backs — a `wl_shm_pool`, a keymap, a clipboard
            // pipe. Every real use site does this anyway, because it needs the
            // memory for the same period. A sender that drops its handle
            // immediately after `send` — which is legal on Unix — leaves the
            // receiver with a name for an object that no longer exists, and it
            // will fail at `map()` rather than silently reading somebody
            // else's memory, because section names carry 128 bits of entropy
            // and creation refuses to adopt an existing name.
            Argument::Fd(ref fd) => match (encoding, fd) {
                #[cfg(unix)]
                (FdEncoding::OutOfBand, RawWireFd::Open(raw)) => {
                    let dup_fd = unsafe { BorrowedFd::borrow_raw(*raw) }
                        .try_clone_to_owned()
                        .map_err(MessageWriteError::DupFdFailed)?;
                    fds.push(dup_fd);
                    payload
                }
                (FdEncoding::InBand | FdEncoding::InBandArena(_), RawWireFd::Named(handle)) => {
                    if !descriptor_allowed(encoding, handle.descriptor()) {
                        return Err(MessageWriteError::WrongDialect { wanted: encoding });
                    }
                    let _ = &fds; // no control message on this path
                    write_array_to_payload(&handle.descriptor().encode(), payload)?
                }
                // The caller brought the wrong kind. Down here neither can be
                // made from the other — see `MessageWriteError::WrongDialect`.
                _ => return Err(MessageWriteError::WrongDialect { wanted: encoding }),
            },
        };
    }

    let wrote_size = free_size - payload.len();
    header[..4].copy_from_slice(&msg.sender_id.to_ne_bytes());
    header[4..]
        .copy_from_slice(&(((wrote_size as u32) << 16) | u32::from(msg.opcode)).to_ne_bytes());
    Ok(orig_payload_len - payload.len())
}

/// Attempts to parse a single wayland message with the given signature.
///
/// If the buffers contains several messages, only the first one will be parsed,
/// and the unused tail of the buffers is returned. If a single message was present,
/// the returned slices should thus be empty.
///
/// Errors if the message is malformed.
#[allow(clippy::type_complexity)]
pub fn parse_message<'a>(
    raw: &'a [u8],
    signature: &[ArgumentType],
    fds: &mut VecDeque<OwnedFd>,
    encoding: FdEncoding,
) -> Result<(Message<u32, ProtoFd>, &'a [u8]), MessageParseError> {
    // helper function to read arrays
    fn read_array_from_payload(
        array_len: usize,
        payload: &[u8],
    ) -> Result<(&[u8], &[u8]), MessageParseError> {
        let len = next_multiple_of(array_len, 4);
        if len > payload.len() {
            return Err(MessageParseError::MissingData);
        }
        Ok((&payload[..array_len], &payload[len..]))
    }

    if raw.len() < 2 * 4 {
        return Err(MessageParseError::MissingData);
    }

    let sender_id = u32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let word_2 = u32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let opcode = (word_2 & 0x0000_FFFF) as u16;
    let len = (word_2 >> 16) as usize;

    if len < 2 * 4 {
        return Err(MessageParseError::Malformed);
    } else if len > raw.len() {
        return Err(MessageParseError::MissingData);
    }

    // WAYCOMPOSE FORK: this pre-check asks "have enough fds arrived
    // out-of-band to satisfy this signature". Nothing arrives out of band on an
    // in-band connection — the descriptors are in the body, and running out of
    // them is detected as a short body like any other truncation. §79 made this
    // a test of the connection rather than of the platform.
    #[cfg(unix)]
    if encoding == FdEncoding::OutOfBand {
        let fd_len = signature.iter().filter(|x| matches!(x, ArgumentType::Fd)).count();
        if fd_len > fds.len() {
            return Err(MessageParseError::MissingFD);
        }
    }
    #[cfg(windows)]
    let _ = (&fds, encoding);

    let (mut payload, rest) = raw.split_at(len);
    payload = &payload[2 * 4..];

    let arguments = signature
        .iter()
        .map(|argtype| {
            // WAYCOMPOSE FORK: on an out-of-band connection an `h` takes no body
            // bytes and is popped from the control-message queue instead. On an
            // in-band one it is an ordinary length-prefixed body argument and
            // falls through to the match below, so this early return is taken
            // per connection rather than per platform (§79).
            #[cfg(unix)]
            if encoding == FdEncoding::OutOfBand && matches!(*argtype, ArgumentType::Fd) {
                // don't consume input but fd
                return match fds.pop_front() {
                    Some(front) => Ok(Argument::Fd(WireFd::Open(front))),
                    None => Err(MessageParseError::MissingFD),
                };
            }
            if payload.len() >= 4 {
                let (front, mut tail) = payload.split_at(4);
                let front = u32::from_ne_bytes(front.try_into().unwrap());
                let arg = match *argtype {
                    ArgumentType::Int => Ok(Argument::Int(front as i32)),
                    ArgumentType::Uint => Ok(Argument::Uint(front)),
                    ArgumentType::Fixed => Ok(Argument::Fixed(front as i32)),
                    ArgumentType::Str(_) => {
                        read_array_from_payload(front as usize, tail).and_then(|(v, rest)| {
                            tail = rest;
                            if !v.is_empty() {
                                match CStr::from_bytes_with_nul(v) {
                                    Ok(s) => Ok(Argument::Str(Some(Box::new(s.into())))),
                                    Err(_) => Err(MessageParseError::Malformed),
                                }
                            } else {
                                Ok(Argument::Str(None))
                            }
                        })
                    }
                    ArgumentType::Object(_) => Ok(Argument::Object(front)),
                    ArgumentType::NewId => Ok(Argument::NewId(front)),
                    ArgumentType::Array => {
                        read_array_from_payload(front as usize, tail).map(|(v, rest)| {
                            tail = rest;
                            Argument::Array(Box::new(v.into()))
                        })
                    }
                    // WAYCOMPOSE FORK: reached only on an in-band connection —
                    // on an out-of-band one the early return above took it. This
                    // is the read side of the in-band dialect: the descriptor
                    // comes back out of the body, **typed**, so we know what kind
                    // of object it names without having to infer it from which
                    // request it arrived in. That inference is the cost waypipe
                    // could not avoid.
                    //
                    // §79 made this reachable on Linux, which is the whole of
                    // §72.3's defect: a Linux compositor behind `wc-proxy` now
                    // reads the same bytes the Windows one always did.
                    ArgumentType::Fd => {
                        read_array_from_payload(front as usize, tail).and_then(|(v, rest)| {
                            tail = rest;
                            match wc_handle::Descriptor::decode(v) {
                                Ok(d) if descriptor_allowed(encoding, &d) => {
                                    Ok(Argument::Fd(WireFd::Named(wc_handle::Handle::from_descriptor(d))))
                                }
                                Ok(_) => Err(MessageParseError::Malformed),
                                // A descriptor we cannot parse is a malformed
                                // message, not a missing fd: the bytes were
                                // there and they were wrong.
                                Err(_) => Err(MessageParseError::Malformed),
                            }
                        })
                    }
                };
                payload = tail;
                arg
            } else {
                Err(MessageParseError::MissingData)
            }
        })
        .collect::<Result<SmallVec<_>, MessageParseError>>()?;

    let msg = Message { sender_id, opcode, args: arguments };
    Ok((msg, rest))
}

// Stabalized in Rust 1.73
fn next_multiple_of(lhs: usize, rhs: usize) -> usize {
    match lhs % rhs {
        0 => lhs,
        r => lhs + (rhs - r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AllowNull;
    use smallvec::smallvec;
    use std::ffi::CString;

    #[test]
    fn into_from_raw_cycle() {
        let mut bytes_buffer = vec![0; 1024];
        let mut fd_buffer = Vec::new();

        let msg = Message {
            sender_id: 42,
            opcode: 7,
            args: smallvec![
                Argument::Uint(3),
                Argument::Fixed(-89),
                Argument::Str(Some(Box::new(CString::new(&b"I like trains!"[..]).unwrap()))),
                Argument::Array(vec![1, 2, 3, 4, 5, 6, 7, 8, 9].into()),
                Argument::Object(88),
                Argument::NewId(56),
                Argument::Int(-25),
            ],
        };
        // write the message to the buffers
        write_to_buffers(&msg, &mut bytes_buffer[..], &mut fd_buffer, FdEncoding::OutOfBand)
            .unwrap();
        // read them back
        let mut fd_buffer = VecDeque::from(fd_buffer);
        let (rebuilt, _) = parse_message(
            &bytes_buffer[..],
            &[
                ArgumentType::Uint,
                ArgumentType::Fixed,
                ArgumentType::Str(AllowNull::No),
                ArgumentType::Array,
                ArgumentType::Object(AllowNull::No),
                ArgumentType::NewId,
                ArgumentType::Int,
            ],
            &mut fd_buffer,
            FdEncoding::OutOfBand,
        )
        .unwrap();
        assert_eq!(rebuilt.map_fd(|_| unreachable!("no h in this signature")), msg);
    }

    /// A `wl_shm.create_pool` in the in-band dialect, written and read back
    /// **on this platform**.
    ///
    /// This is §72.3’s defect as a unit test. Before §79 the in-band arms were
    /// `#[cfg(windows)]`, so on Linux there was no code that could write these
    /// bytes and none that could read them; a proxy that wrote them anyway left
    /// the server’s reader one length-prefixed array out of step, with nothing
    /// logged. The assertion that matters is the second one: the `h` costs body
    /// bytes, which is exactly what the out-of-band reader would not have
    /// consumed.
    #[test]
    fn an_in_band_h_round_trips_on_this_platform() {
        let mut bytes_buffer = vec![0; 1024];
        let mut fd_buffer = Vec::new();

        let descriptor = wc_handle::Descriptor::Section {
            name: "wc-pool-round-trip".into(),
            size: 6291456,
        };
        let msg = Message {
            sender_id: 3,
            opcode: 0,
            args: smallvec![
                Argument::NewId(4),
                Argument::Fd(RawWireFd::Named(wc_handle::Handle::from_descriptor(
                    descriptor.clone()
                ))),
                Argument::Int(6291456),
            ],
        };

        let written = write_to_buffers(
            &msg,
            &mut bytes_buffer[..],
            &mut fd_buffer,
            FdEncoding::InBand,
        )
        .unwrap();
        assert!(fd_buffer.is_empty(), "the in-band dialect sends no control message");
        assert!(
            written > 2 * 4 + 2 * 4,
            "the h must occupy body bytes: {written} is only the header and two words"
        );

        let mut fd_buffer = VecDeque::from(fd_buffer);
        let (rebuilt, rest) = parse_message(
            &bytes_buffer[..],
            &[ArgumentType::NewId, ArgumentType::Fd, ArgumentType::Int],
            &mut fd_buffer,
            FdEncoding::InBand,
        )
        .unwrap();
        assert_eq!(rest.len(), bytes_buffer.len() - written);
        assert_eq!(rebuilt.sender_id, 3);
        match &rebuilt.args[1] {
            Argument::Fd(WireFd::Named(handle)) => assert_eq!(handle.descriptor(), &descriptor),
            other => panic!("expected a named h, got {other:?}"),
        }
    }

    /// The dialect is a property of the connection, and reading with the wrong
    /// one is the defect §72.3 describes.
    ///
    /// An out-of-band reader handed in-band bytes does not fail loudly — it
    /// takes the `h` from an empty queue and reports `MissingFD`, and in the
    /// real socket path it would instead read the descriptor’s length word as
    /// the *next* argument and stay silently out of step. Asserting the
    /// mismatch is detectable at all is what stops it being invisible.
    #[test]
    fn reading_in_band_bytes_out_of_band_does_not_silently_succeed() {
        let mut bytes_buffer = vec![0; 1024];
        let mut fd_buffer = Vec::new();
        let msg = Message {
            sender_id: 3,
            opcode: 0,
            args: smallvec![Argument::Fd(RawWireFd::Named(
                wc_handle::Handle::from_descriptor(wc_handle::Descriptor::Section {
                    name: "wc-pool-mismatch".into(),
                    size: 4096,
                })
            ))],
        };
        write_to_buffers(&msg, &mut bytes_buffer[..], &mut fd_buffer, FdEncoding::InBand).unwrap();

        let mut fd_buffer = VecDeque::from(fd_buffer);
        let parsed = parse_message(
            &bytes_buffer[..],
            &[ArgumentType::Fd],
            &mut fd_buffer,
            FdEncoding::OutOfBand,
        );
        assert!(matches!(parsed, Err(MessageParseError::MissingFD)), "got {parsed:?}");
    }

    /// The two dialects cannot be mixed within one connection, and the write
    /// side says so rather than guessing.
    #[test]
    fn an_open_fd_cannot_be_written_in_band() {
        let mut bytes_buffer = vec![0; 1024];
        let mut fd_buffer = Vec::new();
        let file = std::fs::File::open("/dev/null").unwrap();
        let msg = Message {
            sender_id: 3,
            opcode: 0,
            args: smallvec![Argument::Fd(RawWireFd::Open(
                std::os::unix::io::AsRawFd::as_raw_fd(&file)
            ))],
        };
        let err = write_to_buffers(&msg, &mut bytes_buffer[..], &mut fd_buffer, FdEncoding::InBand)
            .unwrap_err();
        assert!(
            matches!(err, MessageWriteError::WrongDialect { wanted: FdEncoding::InBand }),
            "got {err:?}"
        );
    }
    #[test]
    fn arena_policy_refuses_other_arenas_and_host_names_before_dispatch() {
        use wc_handle::{Descriptor, Handle};
        let cases = [
            (Descriptor::ArenaSlice { arena: 42, offset: 0, len: 4096 }, true),
            (Descriptor::ArenaSlice { arena: 43, offset: 0, len: 4096 }, false),
            (Descriptor::ArenaSlice { arena: 42, offset: 0, len: 0 }, false),
            (Descriptor::SharedFile { path: "/must-never-open".into(), size: 4096 }, false),
            (Descriptor::Section { name: "/must-never-open".into(), size: 4096 }, false),
            (Descriptor::Pipe { name: "/must-never-open".into(), peer_reads: true }, false),
            (Descriptor::NtHandle { value: 4, object_kind: 0 }, false),
        ];
        for (descriptor, allowed) in cases {
            let message = Message { sender_id: 3, opcode: 0,
                args: smallvec![Argument::Fd(RawWireFd::Named(Handle::from_descriptor(descriptor)))] };
            let mut bytes = [0u8; 1024];
            let mut fds = Vec::new();
            let written = write_to_buffers(&message, &mut bytes, &mut fds, FdEncoding::InBand).unwrap();
            let parsed = parse_message(&bytes[..written], &[ArgumentType::Fd],
                &mut VecDeque::new(), FdEncoding::InBandArena(42));
            assert_eq!(parsed.is_ok(), allowed);
            let encoded = write_to_buffers(&message, &mut bytes, &mut fds, FdEncoding::InBandArena(42));
            assert_eq!(encoded.is_ok(), allowed);
        }
    }

}
