//! The bounded PCM queue between a stream's device thread and whoever feeds
//! or drains it (later: the sound ring).
//!
//! **Bounded latency, by dropping**, as `wc-launch`'s waveOut player does
//! (§86): the queue holds at most `cap` frames. Frames written to a full
//! queue are dropped and counted, never queued behind the rest, so the delay
//! never grows past `cap`; a device that finds the queue short plays silence
//! for the rest (an underrun), also counted. Two clocks (Linux's and the
//! host device's) always disagree a little, and this is where that ends.
//!
//! Only whole frames enter the queue. [`PcmWriter`] keeps a partial frame
//! until the rest of it arrives, so a caller can write any byte count.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What a stream did, for a log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Frames that entered the queue.
    pub frames_in: u64,
    /// Frames dropped because the queue was full.
    pub frames_dropped: u64,
    /// Frames taken out of the queue.
    pub frames_out: u64,
    /// Times the device wanted more than the queue had.
    pub underruns: u64,
}

impl Stats {
    pub fn line(&self, rate: u32) -> String {
        let s = |n: u64| n as f64 / f64::from(rate.max(1));
        format!(
            "{:.2} s in, {:.2} s out, {:.3} s dropped, {} underrun(s)",
            s(self.frames_in),
            s(self.frames_out),
            s(self.frames_dropped),
            self.underruns
        )
    }
}

/// A queue of whole frames with a hard bound.
#[derive(Debug)]
pub struct Queue {
    buf: VecDeque<u8>,
    frame: usize,
    cap: usize,
}

impl Queue {
    /// `frame` bytes per frame (channels × 2 for s16le), at most `cap_frames`.
    pub fn new(frame: usize, cap_frames: usize) -> Self {
        let frame = frame.max(1);
        Self { buf: VecDeque::with_capacity(frame * cap_frames), frame, cap: cap_frames.max(1) }
    }
    pub fn frame_bytes(&self) -> usize {
        self.frame
    }
    pub fn frames(&self) -> usize {
        self.buf.len() / self.frame
    }
    pub fn cap_frames(&self) -> usize {
        self.cap
    }
    /// Queue the whole frames of `data` that fit; the rest are dropped.
    /// Returns (queued, dropped) in frames. A trailing partial frame is
    /// ignored (the caller keeps it).
    pub fn push(&mut self, data: &[u8]) -> (usize, usize) {
        let n = data.len() / self.frame;
        let room = self.cap - self.frames();
        let take = n.min(room);
        self.buf.extend(&data[..take * self.frame]);
        (take, n - take)
    }
    /// Move up to `out.len()` bytes of whole frames into `out`; returns the
    /// bytes moved.
    pub fn pop(&mut self, out: &mut [u8]) -> usize {
        let n = (out.len() / self.frame).min(self.frames()) * self.frame;
        for (o, b) in out[..n].iter_mut().zip(self.buf.drain(..n)) {
            *o = b;
        }
        n
    }
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

struct State {
    q: Queue,
    closed: bool,
    stats: Stats,
}

/// The queue shared between two threads, with an end. Cloning gives another
/// handle on the same queue.
#[derive(Clone)]
pub struct Pipe(Arc<(Mutex<State>, Condvar)>);

impl Pipe {
    pub fn new(frame: usize, cap_frames: usize) -> Self {
        Pipe(Arc::new((
            Mutex::new(State { q: Queue::new(frame, cap_frames), closed: false, stats: Stats::default() }),
            Condvar::new(),
        )))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn frame_bytes(&self) -> usize {
        self.lock().q.frame_bytes()
    }

    /// Queue whole frames, dropping what does not fit. Never blocks. Returns
    /// the frames dropped.
    pub fn push(&self, data: &[u8]) -> usize {
        let mut s = self.lock();
        if s.closed {
            return data.len() / s.q.frame_bytes();
        }
        let (took, dropped) = s.q.push(data);
        s.stats.frames_in += took as u64;
        s.stats.frames_dropped += dropped as u64;
        drop(s);
        if took > 0 {
            self.0.1.notify_all();
        }
        dropped
    }

    /// Fill `out` from the queue without waiting; what the queue cannot
    /// cover is zeroed (silence) and counted as an underrun once. Returns
    /// the bytes that were real samples. For a render device's callback.
    pub fn pop_or_silence(&self, out: &mut [u8]) -> usize {
        let mut s = self.lock();
        let n = s.q.pop(out);
        let f = s.q.frame_bytes();
        s.stats.frames_out += (n / f) as u64;
        if n < out.len() {
            out[n..].fill(0);
            if s.stats.frames_out > 0 || n > 0 {
                // Silence before the first sample is the stream starting,
                // not an underrun.
                s.stats.underruns += 1;
            }
        }
        n
    }

    /// Wait until there is at least one frame, the pipe is closed, or
    /// `timeout` passes (`None`: no limit); then take what fits in `out`.
    /// `Ok(0)` means closed and drained, or the time ran out.
    pub fn read(&self, out: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut s = self.lock();
        let f = s.q.frame_bytes();
        if out.len() < f {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "buffer smaller than one frame"));
        }
        loop {
            if s.q.frames() > 0 {
                let n = s.q.pop(out);
                s.stats.frames_out += (n / f) as u64;
                return Ok(n);
            }
            if s.closed {
                return Ok(0);
            }
            match deadline {
                None => s = self.0.1.wait(s).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Ok(0);
                    }
                    s = self.0.1.wait_timeout(s, d - now).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        }
    }

    /// Frames queued now.
    pub fn frames(&self) -> usize {
        self.lock().q.frames()
    }

    /// End the stream: readers drain what is left and then see the end;
    /// writes are dropped.
    pub fn close(&self) {
        self.lock().closed = true;
        self.0.1.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    pub fn stats(&self) -> Stats {
        self.lock().stats
    }
}

/// The writing end, as `std::io::Write`. A write never blocks and always
/// "succeeds" while the stream is open (frames that do not fit are dropped
/// and counted); after the stream ended it fails with `BrokenPipe`.
pub struct PcmWriter {
    pipe: Pipe,
    partial: Vec<u8>,
}

impl PcmWriter {
    pub fn new(pipe: Pipe) -> Self {
        Self { pipe, partial: Vec::new() }
    }
    pub fn stats(&self) -> Stats {
        self.pipe.stats()
    }
}

impl io::Write for PcmWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pipe.is_closed() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the stream ended"));
        }
        let f = self.pipe.frame_bytes();
        let mut data = buf;
        if !self.partial.is_empty() {
            let need = f - self.partial.len();
            let k = need.min(data.len());
            self.partial.extend_from_slice(&data[..k]);
            data = &data[k..];
            if self.partial.len() == f {
                self.pipe.push(&self.partial);
                self.partial.clear();
            }
        }
        let whole = data.len() / f * f;
        if whole > 0 {
            self.pipe.push(&data[..whole]);
        }
        self.partial.extend_from_slice(&data[whole..]);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The reading end, as `std::io::Read`: blocks until at least one frame is
/// there, returns `Ok(0)` once the stream ended and is drained. `out` must
/// hold at least one frame.
pub struct PcmReader {
    pipe: Pipe,
    timeout: Option<Duration>,
}

impl PcmReader {
    pub fn new(pipe: Pipe) -> Self {
        Self { pipe, timeout: None }
    }
    /// With a timeout, a read that finds nothing in time fails with
    /// `TimedOut` rather than returning 0, so 0 still means the end.
    pub fn set_timeout(&mut self, t: Option<Duration>) {
        self.timeout = t;
    }
    pub fn stats(&self) -> Stats {
        self.pipe.stats()
    }
}

impl io::Read for PcmReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.pipe.read(out, self.timeout)?;
        if n == 0 && !self.pipe.is_closed() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "no samples in time"));
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn the_queue_is_bounded_and_drops_what_does_not_fit() {
        let mut q = Queue::new(4, 3);
        assert_eq!(q.push(&[1; 8]), (2, 0));
        assert_eq!(q.push(&[2; 12]), (1, 2), "one frame of room, two dropped");
        assert_eq!(q.frames(), 3);
        let mut out = [0u8; 10];
        assert_eq!(q.pop(&mut out), 8, "whole frames only");
        assert_eq!(&out[..8], &[1; 8]);
        assert_eq!(q.frames(), 1);
        assert_eq!(q.push(&[3; 6]), (1, 0), "a partial frame is not queued");
    }

    #[test]
    fn a_render_device_gets_silence_when_short_and_it_is_counted() {
        let p = Pipe::new(4, 100);
        let mut out = [9u8; 16];
        assert_eq!(p.pop_or_silence(&mut out), 0);
        assert_eq!(out, [0; 16]);
        assert_eq!(p.stats().underruns, 0, "silence before the first sample is the start");
        p.push(&[5; 8]);
        let mut out = [9u8; 16];
        assert_eq!(p.pop_or_silence(&mut out), 8);
        assert_eq!(&out[..8], &[5; 8]);
        assert_eq!(&out[8..], &[0; 8]);
        assert_eq!(p.stats().underruns, 1);
        assert_eq!(p.stats().frames_out, 2);
    }

    #[test]
    fn the_writer_keeps_partial_frames_and_never_blocks() {
        let p = Pipe::new(4, 2);
        let mut w = PcmWriter::new(p.clone());
        assert_eq!(w.write(&[1, 2, 3]).unwrap(), 3);
        assert_eq!(p.frames(), 0);
        assert_eq!(w.write(&[4, 5, 6, 7, 8]).unwrap(), 5);
        assert_eq!(p.frames(), 2);
        assert_eq!(w.write(&[0; 40]).unwrap(), 40, "full: dropped, still accepted");
        assert_eq!(p.stats().frames_dropped, 10);
        let mut out = [0u8; 8];
        assert_eq!(p.pop_or_silence(&mut out), 8);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
        p.close();
        assert_eq!(w.write(&[0; 4]).unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn the_reader_blocks_until_samples_or_the_end() {
        let p = Pipe::new(2, 64);
        let mut r = PcmReader::new(p.clone());
        let q = p.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            q.push(&[7; 6]);
            std::thread::sleep(Duration::from_millis(30));
            q.close();
        });
        let mut out = [0u8; 16];
        assert_eq!(r.read(&mut out).unwrap(), 6);
        assert_eq!(r.read(&mut out).unwrap(), 0, "closed and drained");
        t.join().unwrap();
        let p = Pipe::new(2, 4);
        let mut r = PcmReader::new(p);
        r.set_timeout(Some(Duration::from_millis(10)));
        assert_eq!(r.read(&mut out).unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
