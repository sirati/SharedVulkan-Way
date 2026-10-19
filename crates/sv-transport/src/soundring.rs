//! The sound ring (waycompose-design.md §103.16): several PCM streams in both
//! directions over one byte channel, with credit instead of drops.
//!
//! One [`crate::wlbytes::ByteChannel`] (`OWNER_AUDIO`) carries framed
//! messages both ways. The **device side** (the machine with the speakers
//! and microphones: the Mac) announces one stream per device and direction
//! and which is the default; the **session side** (the Linux applications'
//! sound server) makes a sink or source for each. PCM goes as `Data`, never
//! more than the receiver has granted with `Credit`, so a full ring or a
//! receiver that stopped is backpressure on the sender, which stops reading
//! its source; nothing is dropped.
//!
//! ```text
//!   header (8 bytes, little-endian)        payload
//!   type u8 | flags u8 | stream u16 | len u32
//!
//!   Hello    0x01  both   u32 version
//!   Device   0x10  dev->  u8 dir (0 out, 1 in), u8 channels, u8 format (1 s16le), u8 0,
//!                         u32 rate, then the device's name (UTF-8)
//!   Gone     0x11  dev->  (none)
//!   Default  0x12  dev->  u8 dir; `stream` is the default for that direction
//!   Data     0x20  both   PCM, interleaved, the stream's format; output streams
//!                         session->device, input streams device->session
//!   Credit   0x21  both   u32 bytes more the sender of `Data` may send
//!   Start    0x22  ->dev  capture this input stream (someone records)
//!   Stop     0x23  ->dev  stop capturing it
//! ```
//!
//! Rules: a stream id names one device and direction for as long as the
//! device exists; ids are not reused within a connection. A `Device` is
//! followed by the receiver's first `Credit` for an output stream (the device
//! side grants what its buffer holds, then again as it plays) and by the
//! session side's first `Credit` for an input stream. `Data` beyond the
//! credit is a protocol error. An unknown message type is skipped (its length
//! says how far). Capture runs only between `Start` and `Stop`, so a
//! microphone is open only while something records.

use crate::wlbytes::{ByteChannel, ReadOutcome};
use std::time::{Duration, Instant};

pub const VERSION: u32 = 1;
/// The largest payload a message may carry.
pub const MAX_PAYLOAD: usize = 64 * 1024;
const HEADER: usize = 8;

/// A stream's direction, from the device side's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// The session plays, the device side's output device sounds it.
    Out,
    /// The device side's input device records, the session reads it.
    In,
}

impl Dir {
    fn byte(self) -> u8 {
        match self {
            Dir::Out => 0,
            Dir::In => 1,
        }
    }
    fn from_byte(b: u8) -> Option<Dir> {
        match b {
            0 => Some(Dir::Out),
            1 => Some(Dir::In),
            _ => None,
        }
    }
}

/// One message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Hello { version: u32 },
    Device { stream: u16, dir: Dir, channels: u8, rate: u32, name: String },
    Gone { stream: u16 },
    Default { stream: u16, dir: Dir },
    Data { stream: u16, pcm: Vec<u8> },
    Credit { stream: u16, bytes: u32 },
    Start { stream: u16 },
    Stop { stream: u16 },
}

/// `s16le`, the only format version 1 carries.
pub const FORMAT_S16LE: u8 = 1;

impl Msg {
    /// Append the message's bytes to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut head = |t: u8, stream: u16, len: usize| {
            out.push(t);
            out.push(0);
            out.extend_from_slice(&stream.to_le_bytes());
            out.extend_from_slice(&(len as u32).to_le_bytes());
        };
        match self {
            Msg::Hello { version } => {
                head(0x01, 0, 4);
                out.extend_from_slice(&version.to_le_bytes());
            }
            Msg::Device { stream, dir, channels, rate, name } => {
                let n = name.as_bytes();
                let n = &n[..n.len().min(MAX_PAYLOAD - 8)];
                head(0x10, *stream, 8 + n.len());
                out.extend_from_slice(&[dir.byte(), *channels, FORMAT_S16LE, 0]);
                out.extend_from_slice(&rate.to_le_bytes());
                out.extend_from_slice(n);
            }
            Msg::Gone { stream } => head(0x11, *stream, 0),
            Msg::Default { stream, dir } => {
                head(0x12, *stream, 1);
                out.push(dir.byte());
            }
            Msg::Data { stream, pcm } => {
                head(0x20, *stream, pcm.len());
                out.extend_from_slice(pcm);
            }
            Msg::Credit { stream, bytes } => {
                head(0x21, *stream, 4);
                out.extend_from_slice(&bytes.to_le_bytes());
            }
            Msg::Start { stream } => head(0x22, *stream, 0),
            Msg::Stop { stream } => head(0x23, *stream, 0),
        }
    }
}

/// Why a byte stream is not this protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadFrame(pub String);

/// Reassembles messages from whatever the channel hands over.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole message, `Ok(None)` if more bytes are needed.
    pub fn next_msg(&mut self) -> Result<Option<Msg>, BadFrame> {
        loop {
            if self.buf.len() < HEADER {
                return Ok(None);
            }
            let t = self.buf[0];
            let stream = u16::from_le_bytes([self.buf[2], self.buf[3]]);
            let len = u32::from_le_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
            if len > MAX_PAYLOAD {
                return Err(BadFrame(format!("a {len}-byte payload (type 0x{t:02x}) exceeds {MAX_PAYLOAD}")));
            }
            if self.buf.len() < HEADER + len {
                return Ok(None);
            }
            let p: Vec<u8> = self.buf[HEADER..HEADER + len].to_vec();
            self.buf.drain(..HEADER + len);
            let u32_at = |o: usize| p.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            let short = || BadFrame(format!("type 0x{t:02x} with a {len}-byte payload"));
            let m = match t {
                0x01 => Msg::Hello { version: u32_at(0).ok_or_else(short)? },
                0x10 => {
                    if p.len() < 8 || p[2] != FORMAT_S16LE {
                        return Err(short());
                    }
                    Msg::Device {
                        stream,
                        dir: Dir::from_byte(p[0]).ok_or_else(short)?,
                        channels: p[1],
                        rate: u32_at(4).ok_or_else(short)?,
                        name: String::from_utf8_lossy(&p[8..]).into_owned(),
                    }
                }
                0x11 => Msg::Gone { stream },
                0x12 => Msg::Default { stream, dir: Dir::from_byte(*p.first().ok_or_else(short)?).ok_or_else(short)? },
                0x20 => Msg::Data { stream, pcm: p },
                0x21 => Msg::Credit { stream, bytes: u32_at(0).ok_or_else(short)? },
                0x22 => Msg::Start { stream },
                0x23 => Msg::Stop { stream },
                // A later version's message: skipped, by its length.
                _ => continue,
            };
            return Ok(Some(m));
        }
    }
}

/// Write all of `bytes` to the channel, waiting while the ring is full: the
/// sound ring's backpressure, never a drop. Gives up only if the channel
/// fails, or stays full for `patience` (the peer has stopped reading).
pub fn write_all(ch: &mut ByteChannel, mut bytes: &[u8], patience: Duration) -> Result<(), String> {
    let mut since: Option<Instant> = None;
    while !bytes.is_empty() {
        match ch.write(bytes) {
            Ok(0) => {
                let t = *since.get_or_insert_with(Instant::now);
                if t.elapsed() > patience {
                    return Err(format!("the sound ring stayed full for {patience:?}"));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(n) => {
                bytes = &bytes[n..];
                since = None;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// Read what is there into the decoder, waiting up to `wait` for something.
/// `Ok(false)` is the end of the channel.
pub fn fill(ch: &mut ByteChannel, dec: &mut Decoder, wait: Duration) -> Result<bool, String> {
    let mut buf = [0u8; 16 * 1024];
    let mut got = false;
    loop {
        match ch.read(&mut buf) {
            Ok(ReadOutcome::Bytes(n)) => {
                dec.push(&buf[..n]);
                got = true;
            }
            Ok(ReadOutcome::Eof) => return Ok(false),
            Ok(_) if got => return Ok(true),
            Ok(_) => {
                let _ = ch.wait_readable(wait);
                if !ch.readable().map_err(|e| e.to_string())? {
                    return Ok(true);
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The one thread that owns a sound channel (§103.16): it writes what the
/// other threads queue, in order and never dropping (a full ring is waited
/// out, while incoming messages are still read, so credit can arrive), and
/// hands every incoming message to `on_msg`. [`Link::send`] queues and kicks.
#[derive(Clone)]
pub struct Link {
    tx: std::sync::mpsc::Sender<Msg>,
    kick: std::sync::Arc<crate::wlbytes::Kicker>,
}

impl Link {
    /// Queue a message. Returns false once the channel's thread has ended.
    pub fn send(&self, m: Msg) -> bool {
        let ok = self.tx.send(m).is_ok();
        self.kick.kick();
        ok
    }
}

/// Start the channel's thread. `idle` is how long it sleeps on the lane when
/// there is nothing to do (a doorbell or a [`Link::send`] ends it sooner).
/// `on_end` runs once the channel ends or fails, with the reason.
pub fn spawn(
    mut ch: ByteChannel,
    control: &std::sync::Arc<crate::control::Control>,
    idle: Duration,
    mut on_msg: impl FnMut(Msg, &Link) + Send + 'static,
    on_end: impl FnOnce(String) + Send + 'static,
) -> Result<Link, String> {
    let (tx, rx) = std::sync::mpsc::channel::<Msg>();
    let kick = std::sync::Arc::new(ch.kicker(control).map_err(|e| e.to_string())?);
    let link = Link { tx, kick };
    let l2 = link.clone();
    // The channel points into the control region; the thread keeps the region
    // alive for as long as it may touch it. Without this a caller that dropped
    // its `Control` while the thread still ran left it reading freed memory
    // (`two_links_talk_both_ways` crashed with SIGSEGV in `wake_epoch` under load).
    let keep = std::sync::Arc::clone(control);
    // (Captured by the thread below, which also reads its ring's state.)
    std::thread::Builder::new()
        .name(format!("sound-ring-{}", ch.slot()))
        .spawn(move || {
            let mut pending: Vec<u8> = Vec::new();
            let mut dec = Decoder::default();
            let mut buf = vec![0u8; 16 * 1024];
            let why = 'run: loop {
                // §107: a ring this side's owner closed (its session ended) is
                // not written again, whatever is still queued.
                if keep.ring_state(ch.slot()).ok() == Some(crate::control::RingState::Dead) {
                    break 'run "the ring was closed".into();
                }
                // Before the queue is looked at: a send after this is a kick
                // the wait below will not sleep through.
                let epoch = match ch.wake_epoch() {
                    Ok(e) => e,
                    Err(e) => break 'run e.to_string(),
                };
                while let Ok(m) = rx.try_recv() {
                    m.encode(&mut pending);
                }
                let mut stuck = false;
                while !pending.is_empty() {
                    match ch.write(&pending) {
                        Ok(0) => {
                            stuck = true;
                            break;
                        }
                        Ok(n) => drop(pending.drain(..n)),
                        Err(e) => break 'run e.to_string(),
                    }
                }
                let mut got = false;
                loop {
                    match ch.read(&mut buf) {
                        Ok(ReadOutcome::Bytes(n)) => {
                            dec.push(&buf[..n]);
                            got = true;
                        }
                        Ok(ReadOutcome::Eof) => break 'run "the peer ended the channel".into(),
                        Ok(_) => break,
                        Err(e) => break 'run e.to_string(),
                    }
                }
                loop {
                    match dec.next_msg() {
                        Ok(Some(m)) => on_msg(m, &l2),
                        Ok(None) => break,
                        Err(e) => break 'run e.0,
                    }
                }
                if stuck {
                    // The ring is full: the peer reads it; look again soon.
                    std::thread::sleep(Duration::from_millis(2));
                } else if !got {
                    let _ = ch.wait_readable_since(epoch, idle);
                }
            };
            on_end(why);
        })
        .map_err(|e| e.to_string())?;
    Ok(link)
}

/// Per-stream credit, on the side that sends `Data`.
#[derive(Default)]
pub struct Credits {
    have: std::collections::HashMap<u16, u64>,
}

impl Credits {
    pub fn grant(&mut self, stream: u16, bytes: u32) {
        *self.have.entry(stream).or_default() += u64::from(bytes);
    }

    /// How much of `want` may go now.
    pub fn available(&self, stream: u16, want: usize) -> usize {
        self.have.get(&stream).map_or(0, |h| (*h).min(want as u64) as usize)
    }

    pub fn spend(&mut self, stream: u16, bytes: usize) {
        if let Some(h) = self.have.get_mut(&stream) {
            *h = h.saturating_sub(bytes as u64);
        }
    }

    pub fn forget(&mut self, stream: u16) {
        self.have.remove(&stream);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips_through_any_split() {
        let msgs = vec![
            Msg::Hello { version: VERSION },
            Msg::Device { stream: 3, dir: Dir::Out, channels: 2, rate: 48000, name: "MacBook Pro Speakers".into() },
            Msg::Device { stream: 4, dir: Dir::In, channels: 1, rate: 48000, name: "Micro".into() },
            Msg::Default { stream: 3, dir: Dir::Out },
            Msg::Credit { stream: 3, bytes: 32768 },
            Msg::Data { stream: 3, pcm: (0..=255).collect() },
            Msg::Start { stream: 4 },
            Msg::Stop { stream: 4 },
            Msg::Gone { stream: 4 },
        ];
        let mut bytes = Vec::new();
        for m in &msgs {
            m.encode(&mut bytes);
        }
        // An unknown type in the middle is skipped.
        let mut with_future = bytes.clone();
        with_future.splice(0..0, [0x7f, 0, 0, 0, 3, 0, 0, 0, 1, 2, 3]);
        for split in [1usize, 3, 7, 64, bytes.len()] {
            let mut d = Decoder::default();
            let mut got = Vec::new();
            for chunk in with_future.chunks(split) {
                d.push(chunk);
                while let Some(m) = d.next_msg().unwrap() {
                    got.push(m);
                }
            }
            assert_eq!(got, msgs, "split {split}");
        }
    }

    #[test]
    fn an_oversized_frame_is_refused() {
        let mut d = Decoder::default();
        d.push(&[0x20, 0, 1, 0, 0xff, 0xff, 0xff, 0x7f]);
        assert!(d.next_msg().is_err());
    }

    #[test]
    fn credit_bounds_what_may_be_sent() {
        let mut c = Credits::default();
        assert_eq!(c.available(1, 4096), 0);
        c.grant(1, 1000);
        assert_eq!(c.available(1, 4096), 1000);
        c.spend(1, 600);
        assert_eq!(c.available(1, 4096), 400);
        c.grant(1, 5000);
        assert_eq!(c.available(1, 4096), 4096);
        c.forget(1);
        assert_eq!(c.available(1, 10), 0);
    }

    /// The channel thread: messages queued on one side arrive in order on
    /// the other, both ways, and a send wakes a thread asleep on its lane.
    #[test]
    fn two_links_talk_both_ways() {
        const N_DATA: u16 = 200;
        use crate::control::{Control, Geometry, ProtocolIds};
        use crate::wlbytes::Endpoint;
        let g = Geometry::for_wayland();
        // An owned region: the links' threads keep it alive (through the
        // `Control`) for as long as they run, which a borrowed `Vec` dropped at
        // the test's end did not, and under load a thread still running then
        // read freed memory (SIGSEGV in `wake_epoch`).
        let region = std::sync::Arc::new(crate::region::SharedRegion::heap(g.control_size as usize).unwrap());
        let control = std::sync::Arc::new(Control::create(region, g, ProtocolIds::new(1, 1), 1).unwrap());
        let a = ByteChannel::claim(&control, 9).unwrap();
        let b = ByteChannel::open(&control, a.slot(), Endpoint::Peer).unwrap();
        let (to_a, at_a) = std::sync::mpsc::channel();
        let (to_b, at_b) = std::sync::mpsc::channel();
        // Idle sleeps of 10 s: only the kick can make this test finish in time.
        let la = spawn(a, &control, Duration::from_secs(10), move |m, _| { let _ = to_a.send(m); }, |_| {}).unwrap();
        let lb = spawn(b, &control, Duration::from_secs(10), move |m, l| {
            if let Msg::Hello { .. } = m {
                l.send(Msg::Credit { stream: 1, bytes: 4096 });
            }
            let _ = to_b.send(m);
        }, |_| {}).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        assert!(la.send(Msg::Hello { version: VERSION }));
        assert_eq!(at_b.recv_timeout(Duration::from_secs(5)).unwrap(), Msg::Hello { version: VERSION });
        assert_eq!(at_a.recv_timeout(Duration::from_secs(5)).unwrap(), Msg::Credit { stream: 1, bytes: 4096 });
        assert!(t.elapsed() < Duration::from_secs(2), "a send did not wake the sleeping thread");
        for i in 0..N_DATA {
            lb.send(Msg::Data { stream: 1, pcm: vec![i as u8; 3840] });
        }
        for i in 0..N_DATA {
            assert_eq!(at_a.recv_timeout(Duration::from_secs(5)).unwrap(), Msg::Data { stream: 1, pcm: vec![i as u8; 3840] });
        }
    }

    /// Backpressure, not drops: a writer facing a full ring waits for the
    /// reader, and every byte arrives in order.
    #[test]
    fn a_full_ring_waits_and_loses_nothing() {
        use crate::control::{Control, Geometry, ProtocolIds};
        use crate::wlbytes::Endpoint;
        let g = Geometry::for_wayland();
        let mut mem = vec![0u64; g.control_size as usize / 8];
        // SAFETY: a zeroed, 8-aligned buffer of the geometry's size that
        // outlives both channels.
        let region = std::sync::Arc::new(unsafe {
            crate::region::SharedRegion::from_raw(mem.as_mut_ptr().cast(), g.control_size as usize)
        });
        let control = std::sync::Arc::new(Control::create(region, g, ProtocolIds::new(1, 1), 1).unwrap());
        let mut tx = ByteChannel::claim(&control, 9).unwrap();
        let mut rx = ByteChannel::open(&control, tx.slot(), Endpoint::Peer).unwrap();
        let total = 4 << 20; // far more than a lane holds
        let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let d2 = data.clone();
        let w = std::thread::spawn(move || {
            for chunk in d2.chunks(MAX_PAYLOAD) {
                let mut f = Vec::new();
                Msg::Data { stream: 1, pcm: chunk.to_vec() }.encode(&mut f);
                write_all(&mut tx, &f, Duration::from_secs(10)).unwrap();
            }
        });
        let mut dec = Decoder::default();
        let mut got = Vec::with_capacity(total);
        while got.len() < total {
            assert!(fill(&mut rx, &mut dec, Duration::from_millis(5)).unwrap());
            while let Some(Msg::Data { pcm, .. }) = dec.next_msg().unwrap() {
                got.extend_from_slice(&pcm);
            }
        }
        w.join().unwrap();
        assert!(got == data, "bytes lost or reordered");
        drop(control);
        drop(mem);
    }
}
