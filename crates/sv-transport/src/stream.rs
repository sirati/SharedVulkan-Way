//! The cold path **across a VM boundary**, where a descriptor cannot cross.
//!
//! design.md D10 says the cold path is "a socket: virtio-serial at first because
//! it is already attached, vsock once `viosock` is installed", and §4.3 says the
//! locator descriptor rides it. What neither says out loud, because on one
//! machine it never comes up, is the thing that makes this module necessary:
//!
//! > note an fd cannot cross this boundary, so the `SCM_RIGHTS` path in
//! > `cold.rs` does not apply
//!
//! [`crate::cold`]'s `send_with_fd`/`recv_with_fd` are `sendmsg` with
//! `SCM_RIGHTS`, which is a Unix-domain-socket facility and exists only between
//! two processes on one kernel. A virtio-serial port is a byte pipe: it carries
//! bytes and nothing else, in either direction, between a guest and its host.
//!
//! # What replaces the descriptor
//!
//! Nothing, and that is the point. **In the cross-VM topology neither side ever
//! needed the other's descriptor**, because neither side obtains the arena from
//! the other:
//!
//! - the side inside the VM *allocates* the arena out of its own RAM (D20);
//! - the side outside the VM *locates* it, in QEMU's memory backend, using a
//!   descriptor it opens itself through `/proc/<qemu-pid>/fd/<n>` — which is
//!   exactly why `sv-hostd` exists and needs privilege (§3).
//!
//! So what crosses is the [`ExtentDesc`](crate::cold::ExtentDesc): `run_id`,
//! granularity, page counts, locator. Small, address-free, and already the only
//! thing §4.3 puts in the `Hello`. The `SCM_RIGHTS` path is not being replaced
//! by a Windows equivalent; it is simply not on this path at all, and the
//! same-machine loop keeps it because there the arena really is one process's
//! memfd.
//!
//! That is worth stating plainly because the obvious design — "find the Windows
//! spelling of `SCM_RIGHTS`, it is `DuplicateHandle`" — is a real mechanism that
//! this topology does not need, and reaching for it would put a process handle
//! and a pid on the wire for nothing.
//!
//! # Framing, and why it is not the socket's
//!
//! `cold.rs`'s Unix path frames with a 4-byte length in a *separate* `sendmsg`
//! from the body, which is safe on `SOCK_STREAM` between two processes that
//! agree. A virtio-serial port is weaker than that in one specific way: it is a
//! **character device**, so a reader gets whatever bytes have arrived, a writer
//! may write short, and — this is the one that bites — **a port with nothing
//! connected on the far side silently discards everything written to it**.
//!
//! So this module frames explicitly and defensively:
//!
//! | field | bytes | why |
//! |---|---|---|
//! | magic | 4 | `SVCP`. A port may carry a previous session's tail, or a console banner if somebody attached the wrong device. Resynchronising on a magic turns that into a skipped byte rather than a 4-gigabyte length. |
//! | len | 4 | little-endian, bounded by [`crate::cold::MAX_COLD_MESSAGE`] |
//! | body | len | a [`ColdMessage`](crate::cold::ColdMessage) |
//!
//! No checksum. The transport underneath is virtio, which does not corrupt
//! bytes; the magic is for *framing*, not integrity, and a checksum here would
//! imply a guarantee the design does not otherwise make.
//!
//! # Why this is a trait rather than a `File`
//!
//! Because the three things it has to run over are not the same type and two of
//! them are not files:
//!
//! | side | Linux | Windows |
//! |---|---|---|
//! | guest | `/dev/virtio-ports/<name>` — a character device | `\\.\Global\<name>` — a named pipe the virtio-serial driver exposes |
//! | host | the `chardev` QEMU is given: a Unix socket, or a pty | same |
//!
//! The host end is a Unix socket in the deployment (`-chardev socket`), so it is
//! a `UnixStream`; the guest end is a `File` on Linux and a pipe handle on
//! Windows. [`ColdChannel`] is `Read + Write` and this module is the framing
//! over it, which keeps every one of those a detail of how the channel was
//! opened.

use crate::cold::{ColdMessage, MAX_COLD_MESSAGE};
use crate::{Error, Result};
use std::io::{Read, Write};

/// Frame magic. Chosen to be visible in a hex dump of a serial port, because
/// that is how this will be debugged the first time it does not work.
pub const FRAME_MAGIC: [u8; 4] = *b"SVCP";

/// Header bytes before the body.
pub const FRAME_HEADER: usize = 8;

/// A bidirectional byte channel that carries no descriptors.
///
/// Blanket-implemented for everything that is `Read + Write`, so a `UnixStream`,
/// a `File` on `/dev/virtio-ports/...`, a Windows named pipe and an in-memory
/// pair in a test are all channels without any of them knowing it.
pub trait ColdChannel: Read + Write {}
impl<T: Read + Write> ColdChannel for T {}

fn os<E: Into<std::io::Error>>(call: &'static str, e: E) -> Error {
    Error::Os { call, errno: e.into().raw_os_error().unwrap_or(0) }
}

/// Write one framed message.
///
/// Flushed before returning. On a character device an unflushed write can sit in
/// a buffer indefinitely while both sides wait for each other, which presents as
/// a handshake that hangs rather than as a write that failed.
pub fn write_message<C: ColdChannel>(ch: &mut C, msg: &ColdMessage) -> Result<()> {
    let mut body = Vec::new();
    msg.encode(&mut body)?;
    if body.len() > MAX_COLD_MESSAGE {
        return Err(Error::BadColdMessage("cold-path message is implausibly long"));
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER + body.len());
    frame.extend_from_slice(&FRAME_MAGIC);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    // One write for header and body together. Two writes would let a reader see
    // a header with no body behind it, which on a byte pipe is indistinguishable
    // from a peer that died between them.
    ch.write_all(&frame).map_err(|e| os("write_all", e))?;
    ch.flush().map_err(|e| os("flush", e))?;
    Ok(())
}

/// Read one framed message, resynchronising on the magic.
///
/// `scratch` is the caller's buffer, reused across calls so a handshake does not
/// allocate per message.
pub fn read_message<C: ColdChannel>(ch: &mut C, scratch: &mut Vec<u8>) -> Result<ColdMessage> {
    // Resynchronise. A byte pipe can hold anything that was written to it before
    // we opened it — a previous session's tail, or a login banner if the wrong
    // device was configured — and the failure mode of *not* doing this is
    // reading four arbitrary bytes as a length.
    let mut window = [0u8; 4];
    let mut have = 0usize;
    let mut skipped = 0u64;
    loop {
        if have == 4 {
            if window == FRAME_MAGIC {
                break;
            }
            // Slide by one byte, not by four: a magic can begin inside what we
            // already read, and dropping the whole window would step over it.
            window.copy_within(1..4, 0);
            have = 3;
            skipped += 1;
            // A port that is producing endless non-frame bytes is a
            // misconfiguration, not something to keep reading through.
            if skipped > MAX_COLD_MESSAGE as u64 {
                return Err(Error::BadColdMessage(
                    "no cold-path frame magic in a very long stream — wrong device?",
                ));
            }
        }
        let n = ch.read(&mut window[have..]).map_err(|e| os("read", e))?;
        if n == 0 {
            return Err(Error::BadColdMessage("the cold path closed"));
        }
        have += n;
    }

    let mut lenbuf = [0u8; 4];
    ch.read_exact(&mut lenbuf).map_err(|e| os("read_exact", e))?;
    let len = u32::from_le_bytes(lenbuf) as usize;
    if len > MAX_COLD_MESSAGE {
        return Err(Error::BadColdMessage("cold-path message is implausibly long"));
    }
    scratch.clear();
    scratch.resize(len, 0);
    ch.read_exact(scratch).map_err(|e| os("read_exact", e))?;
    ColdMessage::decode(scratch)
}

/// Open the guest end of a virtio-serial port on Linux.
///
/// The port appears as `/dev/virtio-ports/<name>` where `<name>` is the `name=`
/// given to `virtserialport` on the QEMU command line — `org.sirati.wifi.0` and
/// `org.sirati.display.0` already exist on the target, and adding one more is a
/// single entry in `virtualisation.windowsVm.virtioSerialPorts`.
///
/// Opened read-write on one descriptor. Two descriptors on the same port is a
/// way to deadlock: the driver serialises them, and a reader blocked in one
/// while the other wants to write does not make progress.
#[cfg(target_os = "linux")]
pub fn open_virtio_port(name: &str) -> std::io::Result<std::fs::File> {
    let path = format!("/dev/virtio-ports/{name}");
    std::fs::OpenOptions::new().read(true).write(true).open(&path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "{path}: {e} — the port exists only when the host side of the chardev is \
                 connected and the guest's virtio_console module is loaded"
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cold::{ExtentDesc, Granularity, Hello, Locator};
    use crate::control::{ENDIAN_MARKER, Geometry, ProtocolIds};

    fn hello(run_id: u64) -> ColdMessage {
        ColdMessage::Hello(Box::new(Hello {
            ids: ProtocolIds::new(1, 0xdead_beef),
            generation: 3,
            extent: ExtentDesc {
                run_id,
                granularity: Granularity::Large2M,
                control_pages: 4096,
                arena_pages: 524_288,
                locator: Locator::Scan,
            },
            geometry: Geometry::default(),
            endianness: ENDIAN_MARKER,
            pointer_width: 64,
        }))
    }

    /// A channel that is a byte pipe and behaves like one: reads return
    /// whatever has arrived, in whatever chunks it feels like.
    struct Pipe {
        buf: Vec<u8>,
        at: usize,
        /// Bytes to hand back per read, to model a character device that does
        /// not respect the caller's buffer size.
        chunk: usize,
    }

    impl Read for Pipe {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let avail = self.buf.len() - self.at;
            if avail == 0 {
                return Ok(0);
            }
            let n = out.len().min(avail).min(self.chunk);
            out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, src: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(src);
            Ok(src.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn pipe(chunk: usize) -> Pipe {
        Pipe { buf: Vec::new(), at: 0, chunk }
    }

    #[test]
    fn a_message_round_trips_over_a_byte_pipe() {
        let mut p = pipe(4096);
        write_message(&mut p, &hello(0xabc)).unwrap();
        let mut scratch = Vec::new();
        let got = read_message(&mut p, &mut scratch).unwrap();
        let ColdMessage::Hello(h) = got else { panic!("wrong message") };
        assert_eq!(h.extent.run_id, 0xabc);
        assert_eq!(h.extent.arena_pages, 524_288);
        assert_eq!(h.extent.granularity, Granularity::Large2M);
    }

    /// A character device returns short reads. One byte at a time is the
    /// pathological case and it must be indistinguishable from any other.
    #[test]
    fn one_byte_at_a_time_is_the_same_message() {
        let mut p = pipe(1);
        write_message(&mut p, &hello(7)).unwrap();
        let mut scratch = Vec::new();
        let ColdMessage::Hello(h) = read_message(&mut p, &mut scratch).unwrap() else {
            panic!("wrong message")
        };
        assert_eq!(h.extent.run_id, 7);
    }

    /// The reason the magic exists. A port that already holds somebody else's
    /// bytes must cost a resynchronisation, not a wrong length.
    #[test]
    fn junk_before_the_frame_is_skipped_rather_than_read_as_a_length() {
        let mut p = pipe(64);
        p.write_all(b"login: \xff\xfe\x00\x00\x00\x40SVC").unwrap();
        write_message(&mut p, &hello(0x5150)).unwrap();
        let mut scratch = Vec::new();
        let ColdMessage::Hello(h) = read_message(&mut p, &mut scratch).unwrap() else {
            panic!("wrong message")
        };
        assert_eq!(h.extent.run_id, 0x5150);
    }

    /// A partial magic immediately before the real one. The resync must slide by
    /// one byte, not discard its whole window — `SVCSVCP` contains the magic
    /// starting at offset 3 and a four-at-a-time reader steps straight over it.
    #[test]
    fn a_partial_magic_does_not_swallow_the_real_one() {
        let mut p = pipe(64);
        p.write_all(b"SVC").unwrap();
        write_message(&mut p, &hello(0x99)).unwrap();
        let mut scratch = Vec::new();
        let ColdMessage::Hello(h) = read_message(&mut p, &mut scratch).unwrap() else {
            panic!("wrong message")
        };
        assert_eq!(h.extent.run_id, 0x99);
    }

    #[test]
    fn several_messages_stream_back_to_back() {
        let mut p = pipe(7);
        for i in 0..5u64 {
            write_message(&mut p, &hello(i)).unwrap();
        }
        let mut scratch = Vec::new();
        for i in 0..5u64 {
            let ColdMessage::Hello(h) = read_message(&mut p, &mut scratch).unwrap() else {
                panic!("wrong message")
            };
            assert_eq!(h.extent.run_id, i);
        }
    }

    #[test]
    fn a_closed_channel_is_an_error_and_not_a_hang() {
        let mut p = pipe(64);
        let mut scratch = Vec::new();
        assert!(read_message(&mut p, &mut scratch).is_err());
    }

    /// The invariant that makes this path viable at all: what crosses is a
    /// description of *how to find* the extent, and its size does not depend on
    /// how fragmented the extent is. `sv-arena`'s
    /// `a_run_list_has_no_wire_form` asserts the same thing about the encoder;
    /// this asserts it about the frame that actually goes down the wire, which
    /// is what a virtio-serial port has to carry in one piece.
    #[test]
    fn the_frame_is_small_and_does_not_grow_with_the_arena() {
        let mut small = pipe(4096);
        write_message(&mut small, &hello(1)).unwrap();
        let mut big = pipe(4096);
        let ColdMessage::Hello(mut h) = hello(1) else { unreachable!() };
        // A 2 GiB arena, which on the 4 KiB path is ~47,000 runs.
        h.extent.arena_pages = 524_288;
        h.extent.granularity = Granularity::Small4K;
        write_message(&mut big, &ColdMessage::Hello(h)).unwrap();
        assert_eq!(small.buf.len(), big.buf.len());
        // And it fits comfortably in one virtio-serial buffer.
        assert!(small.buf.len() < 256, "the Hello frame is {} bytes", small.buf.len());
    }
}
