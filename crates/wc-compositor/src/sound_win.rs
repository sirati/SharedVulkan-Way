//! §103.16 on Windows: the device end of the sound ring
//! (`sv_transport::soundring`), as `wc-launch`'s `sound_mac.rs` is on macOS.
//!
//! Every active WASAPI endpoint is a stream: an output gets a shared-mode
//! render stream bound to that endpoint, an input a capture stream opened
//! only between the session side's `Start` and `Stop`. The endpoint list and
//! the console-role defaults come from `wc_hostplat::win::audio`'s
//! `IMMNotificationClient` watch, so a device that comes or goes, or a new
//! default, reaches the session within a second.
//!
//! **Credit, not drops, on the output path.** Each output stream grants the
//! session side [`CREDIT_INIT`] bytes, exactly what its queue holds, and
//! grants back every byte the device takes from the queue (the render
//! thread's drain hook). The session side never has more credit than the
//! queue has room, so nothing written within the credit is dropped, and a
//! slow device is backpressure on the Linux application.
//!
//! **A microphone cannot wait**: what it records beyond the session side's
//! credit is counted as an overrun and not queued.
//!
//! An output with nothing to play for [`IDLE_PAUSE`] releases its endpoint
//! (the render stream is dropped; its queue and the credit stay), and starts
//! again when [`PREBUFFER`] is queued or the queued sound has waited
//! [`PREBUFFER_WAIT`].
//!
//! The diffing of endpoint lists into stream messages ([`Streams`]) is pure
//! and tested everywhere; the rest is Windows.

use std::collections::HashMap;

use sv_transport::soundring::Dir;
use wc_hostint::audio::{self, State};

/// What an output stream's queue holds, granted up front: 240 ms of 48 kHz
/// stereo s16le; scaled to the stream's own format.
pub const CREDIT_MS: u32 = 240;
/// Queued before a stopped output starts: 60 ms.
pub const PREBUFFER_MS: u32 = 60;
/// An output with nothing to play for this long releases its endpoint.
pub const IDLE_PAUSE: std::time::Duration = std::time::Duration::from_secs(3);
/// Queued sound shorter than [`PREBUFFER_MS`] starts anyway after this.
pub const PREBUFFER_WAIT: std::time::Duration = std::time::Duration::from_millis(40);

/// Bytes of `ms` milliseconds of s16le at `rate` and `channels`, in whole
/// frames.
pub fn bytes_for(ms: u32, rate: u32, channels: u8) -> u32 {
    (u64::from(rate) * u64::from(ms) / 1000) as u32 * u32::from(channels) * 2
}

/// The initial credit of an output stream.
pub fn credit_init(rate: u32, channels: u8) -> u32 {
    bytes_for(CREDIT_MS, rate, channels)
}

/// What the endpoint list's change means on the ring, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A new stream: announce it (`Device`), and open what it needs.
    Added { stream: u16, dir: Dir, channels: u8, rate: u32, name: String, endpoint: String },
    /// A stream whose endpoint is gone (or changed its format): `Gone`.
    Gone { stream: u16 },
    /// The direction's default is now this stream.
    Default { stream: u16, dir: Dir },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Known {
    stream: u16,
    channels: u8,
    rate: u32,
}

/// Endpoints to stream ids. An id names one endpoint and direction in one
/// format for as long as it exists; ids are never reused (a format change is
/// a new stream, since `Device` fixes the format).
#[derive(Debug, Default)]
pub struct Streams {
    known: HashMap<(String, bool), Known>,
    next: u16,
    default_out: Option<u16>,
    default_in: Option<u16>,
}

fn sdir(d: audio::Dir) -> Dir {
    match d {
        audio::Dir::Output => Dir::Out,
        audio::Dir::Input => Dir::In,
    }
}

impl Streams {
    /// The stream for an endpoint, if it has one.
    pub fn stream_of(&self, endpoint: &str, dir: Dir) -> Option<u16> {
        self.known.get(&(endpoint.to_string(), dir == Dir::Out)).map(|k| k.stream)
    }

    /// Diff the host's state against what was announced.
    pub fn update(&mut self, st: &State) -> Vec<Change> {
        let mut out = Vec::new();
        // Deterministic order whatever the enumeration's: direction, name, id.
        let mut devs: Vec<&audio::Device> = st.devices.iter().collect();
        devs.sort_by(|a, b| (a.dir, &a.description, &a.id).cmp(&(b.dir, &b.description, &b.id)));
        devs.dedup_by(|a, b| a.id == b.id && a.dir == b.dir);
        let want: Vec<((String, bool), Known, &audio::Device)> = devs
            .into_iter()
            .map(|d| {
                let channels = if d.channels == 0 { 2 } else { d.channels.min(8) } as u8;
                let rate = if d.rate == 0 { 48_000 } else { d.rate };
                ((d.id.clone(), d.dir == audio::Dir::Output), Known { stream: 0, channels, rate }, d)
            })
            .collect();
        // Gone: no longer listed, or listed in another format.
        let mut gone: Vec<u16> = self
            .known
            .iter()
            .filter(|(k, v)| !want.iter().any(|(wk, w, _)| wk == *k && w.channels == v.channels && w.rate == v.rate))
            .map(|(_, v)| v.stream)
            .collect();
        gone.sort_unstable();
        for s in &gone {
            self.known.retain(|_, v| v.stream != *s);
            if self.default_out == Some(*s) {
                self.default_out = None;
            }
            if self.default_in == Some(*s) {
                self.default_in = None;
            }
            out.push(Change::Gone { stream: *s });
        }
        for (key, mut k, d) in want {
            if self.known.contains_key(&key) {
                continue;
            }
            let Some(n) = self.next.checked_add(1) else { continue };
            self.next = n;
            k.stream = n;
            out.push(Change::Added {
                stream: n,
                dir: sdir(d.dir),
                channels: k.channels,
                rate: k.rate,
                name: d.description.clone(),
                endpoint: d.id.clone(),
            });
            self.known.insert(key, k);
        }
        for (dir, id) in [(Dir::Out, &st.default_output), (Dir::In, &st.default_input)] {
            let s = id.as_deref().and_then(|i| self.stream_of(i, dir));
            let cur = if dir == Dir::Out { &mut self.default_out } else { &mut self.default_in };
            if let Some(s) = s
                && *cur != Some(s)
            {
                *cur = Some(s);
                out.push(Change::Default { stream: s, dir });
            }
        }
        out
    }
}

#[cfg(windows)]
pub use win::serve;

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use sv_transport::control::Control;
    use sv_transport::soundring::{Dir, Link, MAX_PAYLOAD, Msg, VERSION};
    use sv_transport::wlbytes::ByteChannel;
    use wc_hostplat::pcm::Pipe;
    use wc_hostplat::win::audio;
    use wc_hostplat::win::stream::{Capture, Format, Render, Sizing};

    use super::*;

    /// One output stream: its queue (which outlives the render stream, so an
    /// idle stream can let its endpoint go), and the render stream while
    /// the endpoint is held.
    struct Out {
        endpoint: String,
        name: String,
        fmt: Format,
        pipe: Pipe,
        render: Option<Render>,
        last: Instant,
        /// When the queue last went from empty to not empty while stopped.
        waiting_since: Option<Instant>,
        failed: bool,
    }

    /// One input stream: the credit the session side granted, and the
    /// capture while it records.
    struct In {
        endpoint: String,
        name: String,
        fmt: Format,
        credit: Arc<Mutex<u64>>,
        sent: Arc<AtomicU64>,
        overruns: Arc<AtomicU64>,
        capture: Option<Capture>,
    }

    #[derive(Default)]
    struct Book {
        streams: Streams,
        outs: HashMap<u16, Out>,
        ins: HashMap<u16, In>,
    }

    fn sizing() -> Sizing {
        Sizing { queue: Duration::from_millis(u64::from(CREDIT_MS)), device: Duration::from_millis(40) }
    }

    impl Out {
        fn start(&mut self, stream: u16, link: &Link, say: fn(&str)) {
            if self.render.is_some() || self.failed {
                return;
            }
            let l = link.clone();
            let drain = Box::new(move |n: usize| {
                l.send(Msg::Credit { stream, bytes: n as u32 });
            });
            match Render::start_shared(&self.endpoint, self.fmt, sizing(), self.pipe.clone(), drain, say) {
                Ok(r) => self.render = Some(r),
                Err(e) => {
                    // Not retried on every packet: the endpoint is broken
                    // until the watch lists it again (a new stream).
                    self.failed = true;
                    say(&format!("sound: {}: {e}", self.name));
                }
            }
            self.waiting_since = None;
        }

        fn push(&mut self, stream: u16, pcm: &[u8], link: &Link, say: fn(&str)) {
            let dropped = self.pipe.push(pcm);
            if dropped > 0 {
                say(&format!("sound: {}: {dropped} frame(s) beyond the credit (a protocol error)", self.name));
            }
            self.last = Instant::now();
            if self.render.as_ref().is_some_and(|r| !r.running()) {
                // The endpoint went away under us; the watch's Gone follows.
                self.render = None;
                self.failed = true;
            }
            if self.render.is_none() {
                let queued = self.pipe.frames() * self.fmt.frame_bytes();
                if queued >= bytes_for(PREBUFFER_MS, self.fmt.rate, self.fmt.channels as u8) as usize {
                    self.start(stream, link, say);
                } else if self.waiting_since.is_none() {
                    self.waiting_since = Some(Instant::now());
                }
            }
        }

        fn tick(&mut self, stream: u16, link: &Link, say: fn(&str)) {
            let empty = self.pipe.frames() == 0;
            if self.render.is_some() && empty && self.last.elapsed() > IDLE_PAUSE {
                self.render = None;
            }
            if self.render.is_none() && !empty && self.waiting_since.is_some_and(|t| t.elapsed() >= PREBUFFER_WAIT) {
                self.start(stream, link, say);
            }
        }
    }

    impl In {
        fn start(&mut self, stream: u16, link: &Link, say: fn(&str)) {
            if self.capture.is_some() {
                return;
            }
            let (l, credit, sent, over) = (link.clone(), self.credit.clone(), self.sent.clone(), self.overruns.clone());
            let frame = self.fmt.frame_bytes();
            let on_data = Box::new(move |pcm: &[u8]| {
                let n = {
                    let mut c = credit.lock().unwrap_or_else(|e| e.into_inner());
                    let n = ((*c).min(pcm.len() as u64) as usize) / frame * frame;
                    *c -= n as u64;
                    n
                };
                for chunk in pcm[..n].chunks(MAX_PAYLOAD / frame * frame) {
                    l.send(Msg::Data { stream, pcm: chunk.to_vec() });
                }
                sent.fetch_add(n as u64, Ordering::Relaxed);
                if n < pcm.len() {
                    over.fetch_add(1, Ordering::Relaxed);
                }
            });
            match Capture::start_to(&self.endpoint, self.fmt, sizing(), on_data, say) {
                Ok(c) => {
                    self.capture = Some(c);
                    say(&format!("sound: recording {} for the session", self.name));
                }
                Err(e) => say(&format!("sound: {}: {e}", self.name)),
            }
        }
    }

    fn apply(book: &mut Book, changes: Vec<Change>, link: &Link) {
        for c in changes {
            match c {
                Change::Gone { stream } => {
                    book.outs.remove(&stream);
                    book.ins.remove(&stream);
                    link.send(Msg::Gone { stream });
                }
                Change::Added { stream, dir, channels, rate, name, endpoint } => {
                    let fmt = Format { rate, channels: u16::from(channels) };
                    link.send(Msg::Device { stream, dir, channels, rate, name: name.clone() });
                    match dir {
                        Dir::Out => {
                            let credit = credit_init(rate, channels);
                            let frames = credit as usize / fmt.frame_bytes();
                            book.outs.insert(
                                stream,
                                Out {
                                    endpoint,
                                    name,
                                    fmt,
                                    pipe: Pipe::new(fmt.frame_bytes(), frames),
                                    render: None,
                                    last: Instant::now(),
                                    waiting_since: None,
                                    failed: false,
                                },
                            );
                            link.send(Msg::Credit { stream, bytes: credit });
                        }
                        Dir::In => {
                            book.ins.insert(
                                stream,
                                In {
                                    endpoint,
                                    name,
                                    fmt,
                                    credit: Arc::new(Mutex::new(0)),
                                    sent: Arc::new(AtomicU64::new(0)),
                                    overruns: Arc::new(AtomicU64::new(0)),
                                    capture: None,
                                },
                            );
                        }
                    }
                }
                Change::Default { stream, dir } => {
                    link.send(Msg::Default { stream, dir });
                }
            }
        }
    }

    /// Serve the sound ring's device side on `ch` (the `OWNER_AUDIO` slot)
    /// until the channel ends. Blocks; run it on a thread of its own.
    pub fn serve(ch: ByteChannel, control: &Arc<Control>, say: fn(&str)) -> Result<(), String> {
        let book = Arc::new(Mutex::new(Book::default()));
        let ended = Arc::new(AtomicBool::new(false));
        let (b1, e1) = (book.clone(), ended.clone());
        let link = sv_transport::soundring::spawn(
            ch,
            control,
            crate::ring_wait_slice(),
            move |m, link| {
                let mut b = b1.lock().unwrap_or_else(|e| e.into_inner());
                match m {
                    Msg::Data { stream, pcm } => {
                        if let Some(o) = b.outs.get_mut(&stream) {
                            o.push(stream, &pcm, link, say);
                        }
                    }
                    Msg::Credit { stream, bytes } => {
                        if let Some(i) = b.ins.get(&stream) {
                            *i.credit.lock().unwrap_or_else(|e| e.into_inner()) += u64::from(bytes);
                        }
                    }
                    Msg::Start { stream } => {
                        if let Some(i) = b.ins.get_mut(&stream) {
                            i.start(stream, link, say);
                        }
                    }
                    Msg::Stop { stream } => {
                        if let Some(i) = b.ins.get_mut(&stream)
                            && i.capture.take().is_some()
                        {
                            say(&format!("sound: stopped recording {}", i.name));
                        }
                    }
                    Msg::Hello { version } => say(&format!("sound: the session side speaks version {version}")),
                    _ => {}
                }
            },
            move |why| {
                e1.store(true, Ordering::SeqCst);
                say(&format!("sound: the ring ended: {why}"));
            },
        )?;
        link.send(Msg::Hello { version: VERSION });

        let (b2, l2, e2) = (book.clone(), link.clone(), ended.clone());
        let _watch = audio::watch_states(
            move |st| {
                if e2.load(Ordering::SeqCst) {
                    return false;
                }
                let mut b = b2.lock().unwrap_or_else(|e| e.into_inner());
                let changes = b.streams.update(&st);
                if !changes.is_empty() {
                    say(&format!(
                        "sound: {} output(s), {} input(s): {}",
                        st.devices.iter().filter(|d| d.dir == wc_hostint::audio::Dir::Output).count(),
                        st.devices.iter().filter(|d| d.dir == wc_hostint::audio::Dir::Input).count(),
                        st.devices.iter().map(|d| d.description.as_str()).collect::<Vec<_>>().join(", ")
                    ));
                }
                apply(&mut b, changes, &l2);
                true
            },
            say,
        )?;

        while !ended.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
            let mut b = book.lock().unwrap_or_else(|e| e.into_inner());
            let ids: Vec<u16> = b.outs.keys().copied().collect();
            for s in ids {
                if let Some(o) = b.outs.get_mut(&s) {
                    o.tick(s, &link, say);
                }
            }
        }
        let b = book.lock().unwrap_or_else(|e| e.into_inner());
        for o in b.outs.values() {
            say(&format!("sound: {}: {}", o.name, o.pipe.stats().line(o.fmt.rate)));
        }
        for i in b.ins.values() {
            say(&format!(
                "sound: {} recorded {} bytes for the session, {} overrun(s)",
                i.name,
                i.sent.load(Ordering::Relaxed),
                i.overruns.load(Ordering::Relaxed)
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wc_hostint::audio::Device;

    fn dev(dir: audio::Dir, id: &str, name: &str, rate: u32) -> Device {
        Device { dir, id: id.into(), description: name.into(), channels: 2, rate }
    }

    fn guest() -> State {
        State {
            devices: vec![
                dev(audio::Dir::Input, "{in}", "Eingang (2- High Definition Audio Device)", 44_100),
                dev(audio::Dir::Output, "{out}", "Lautsprecher (2- High Definition Audio Device)", 48_000),
            ],
            default_output: Some("{out}".into()),
            default_input: Some("{in}".into()),
        }
    }

    #[test]
    fn the_first_state_announces_every_endpoint_and_both_defaults() {
        let mut s = Streams::default();
        let c = s.update(&guest());
        assert_eq!(
            c,
            [
                Change::Added {
                    stream: 1,
                    dir: Dir::Out,
                    channels: 2,
                    rate: 48_000,
                    name: "Lautsprecher (2- High Definition Audio Device)".into(),
                    endpoint: "{out}".into()
                },
                Change::Added {
                    stream: 2,
                    dir: Dir::In,
                    channels: 2,
                    rate: 44_100,
                    name: "Eingang (2- High Definition Audio Device)".into(),
                    endpoint: "{in}".into()
                },
                Change::Default { stream: 1, dir: Dir::Out },
                Change::Default { stream: 2, dir: Dir::In },
            ]
        );
        assert!(s.update(&guest()).is_empty(), "the same state again says nothing");
    }

    #[test]
    fn ids_are_never_reused_and_defaults_follow() {
        let mut s = Streams::default();
        s.update(&guest());
        // A headset arrives and becomes the default output.
        let mut st = guest();
        st.devices.push(dev(audio::Dir::Output, "{hs}", "Headset", 48_000));
        st.default_output = Some("{hs}".into());
        assert_eq!(
            s.update(&st),
            [
                Change::Added { stream: 3, dir: Dir::Out, channels: 2, rate: 48_000, name: "Headset".into(), endpoint: "{hs}".into() },
                Change::Default { stream: 3, dir: Dir::Out },
            ]
        );
        // It goes; the default goes back to the speakers.
        assert_eq!(
            s.update(&guest()),
            [Change::Gone { stream: 3 }, Change::Default { stream: 1, dir: Dir::Out }]
        );
        // It comes back: a new id, not 3.
        assert!(matches!(s.update(&st)[0], Change::Added { stream: 4, .. }));
    }

    #[test]
    fn a_format_change_is_a_new_stream() {
        let mut s = Streams::default();
        s.update(&guest());
        let mut st = guest();
        st.devices[1].rate = 96_000;
        let c = s.update(&st);
        assert_eq!(c[0], Change::Gone { stream: 1 });
        assert!(matches!(c[1], Change::Added { stream: 3, rate: 96_000, .. }));
        assert_eq!(c[2], Change::Default { stream: 3, dir: Dir::Out });
    }

    #[test]
    fn no_endpoints_and_odd_formats() {
        let mut s = Streams::default();
        assert!(s.update(&State::default()).is_empty());
        let st = State {
            devices: vec![Device { dir: audio::Dir::Output, id: "{x}".into(), description: "7.1".into(), channels: 12, rate: 0 }],
            default_output: Some("{nonesuch}".into()),
            default_input: None,
        };
        assert_eq!(
            s.update(&st),
            [Change::Added { stream: 1, dir: Dir::Out, channels: 8, rate: 48_000, name: "7.1".into(), endpoint: "{x}".into() }],
            "clamped to what the render stream takes; a default that is not listed is not announced"
        );
    }

    #[test]
    fn the_credit_is_the_queue() {
        assert_eq!(credit_init(48_000, 2), 46_080, "240 ms of 48 kHz stereo s16le");
        assert_eq!(credit_init(44_100, 2) % 4, 0, "whole frames");
        assert_eq!(bytes_for(PREBUFFER_MS, 48_000, 2), 11_520);
    }

    /// The device side against a fake session side over a real byte channel
    /// in this process: every endpoint announced, a default output, 2 s of a
    /// tone sent within the credit and all of it granted back as played (no
    /// drop), and a microphone that records only between Start and Stop.
    /// Plays sound, so it is `#[ignore]`d; run it under Wine:
    /// `wine wc_compositor-*.exe sound_win::tests::against_a_fake_session --ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn against_a_fake_session() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        use sv_transport::control::{Control, Geometry, ProtocolIds};
        use sv_transport::soundring::{self, Msg};
        use sv_transport::wlbytes::{ByteChannel, Endpoint};

        fn say(m: &str) {
            eprintln!("{m}");
        }
        let g = Geometry::for_wayland();
        let mem: &'static mut [u64] = Vec::leak(vec![0u64; g.control_size as usize / 8]);
        // SAFETY: a zeroed, 8-aligned buffer of the geometry's size, leaked.
        let region = std::sync::Arc::new(unsafe {
            sv_transport::region::SharedRegion::from_raw(mem.as_mut_ptr().cast(), g.control_size as usize)
        });
        let control = std::sync::Arc::new(Control::create(region, g, ProtocolIds::new(1, 1), 1).unwrap());
        let dev = ByteChannel::claim(&control, 9).unwrap();
        let ses = ByteChannel::open(&control, dev.slot(), Endpoint::Peer).unwrap();
        let c2 = control.clone();
        std::thread::spawn(move || serve(dev, &c2, say));

        let (tx, rx) = mpsc::channel::<Msg>();
        let session = soundring::spawn(ses, &control, Duration::from_millis(5), move |m, _| { let _ = tx.send(m); }, |_| {}).unwrap();
        session.send(Msg::Hello { version: soundring::VERSION });
        let t0 = Instant::now();
        let mut devices = Vec::new();
        let (mut dout, mut din) = (None, None);
        let mut credit: HashMap<u16, u64> = HashMap::new();
        while t0.elapsed() < Duration::from_secs(5) && (dout.is_none() || din.is_none() || credit.is_empty()) {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Msg::Device { stream, dir, channels, rate, name }) => devices.push((stream, dir, channels, rate, name)),
                Ok(Msg::Default { stream, dir: Dir::Out }) => dout = Some(stream),
                Ok(Msg::Default { stream, dir: Dir::In }) => din = Some(stream),
                Ok(Msg::Credit { stream, bytes }) => *credit.entry(stream).or_default() += u64::from(bytes),
                _ => {}
            }
        }
        eprintln!("devices {devices:?}, default out {dout:?}, in {din:?}, credit {credit:?}");
        let out = dout.expect("a default output");
        let (_, _, ch, rate, _) = devices.iter().find(|d| d.0 == out).cloned().unwrap();
        assert_eq!(credit.get(&out).copied(), Some(u64::from(credit_init(rate, ch))));

        // 2 s of a 440 Hz tone, sent as the credit allows.
        let frame = usize::from(ch) * 2;
        let total = rate as usize * 2 * frame;
        let mut pcm = Vec::with_capacity(total);
        for i in 0..rate as usize * 2 {
            let v = ((i as f64 * 440.0 * std::f64::consts::TAU / f64::from(rate)).sin() * 3000.0) as i16;
            for _ in 0..ch {
                pcm.extend_from_slice(&v.to_le_bytes());
            }
        }
        let mut have = credit[&out];
        let mut sent = 0usize;
        let mut granted_back = 0u64;
        let t1 = Instant::now();
        while sent < total && t1.elapsed() < Duration::from_secs(10) {
            let n = (have as usize).min(total - sent).min(16 * 1024) / frame * frame;
            if n > 0 {
                session.send(Msg::Data { stream: out, pcm: pcm[sent..sent + n].to_vec() });
                sent += n;
                have -= n as u64;
            }
            if let Ok(Msg::Credit { stream, bytes }) = rx.recv_timeout(Duration::from_millis(5))
                && stream == out
            {
                have += u64::from(bytes);
                granted_back += u64::from(bytes);
            }
        }
        let played_in = t1.elapsed();
        // Wait for the queue to drain: everything sent comes back as credit.
        let t2 = Instant::now();
        while granted_back < sent as u64 && t2.elapsed() < Duration::from_secs(3) {
            if let Ok(Msg::Credit { stream, bytes }) = rx.recv_timeout(Duration::from_millis(50))
                && stream == out
            {
                granted_back += u64::from(bytes);
            }
        }
        eprintln!("sent {sent} bytes in {played_in:?}, {granted_back} granted back");
        assert_eq!(sent, total);
        assert_eq!(granted_back, sent as u64, "every byte sent was played, none dropped");
        assert!(played_in >= Duration::from_millis(1700), "paced by the device, not faster");

        // The microphone: nothing before Start, data after, nothing after Stop.
        // Every input in turn, the default first (an endpoint the machine
        // cannot open is reported and skipped); at least one must record.
        let mut inputs: Vec<u16> = din.into_iter().collect();
        inputs.extend(devices.iter().filter(|d| d.1 == Dir::In && Some(d.0) != din).map(|d| d.0));
        let mut recorded = false;
        for inp in inputs {
            std::thread::sleep(Duration::from_millis(300));
            assert!(!rx.try_iter().any(|m| matches!(m, Msg::Data { stream, .. } if stream == inp)), "no data before Start");
            session.send(Msg::Credit { stream: inp, bytes: 32 * 1024 });
            session.send(Msg::Start { stream: inp });
            let mut got = 0usize;
            let t3 = Instant::now();
            while t3.elapsed() < Duration::from_secs(1) {
                if let Ok(Msg::Data { stream, pcm }) = rx.recv_timeout(Duration::from_millis(50))
                    && stream == inp
                {
                    got += pcm.len();
                }
            }
            session.send(Msg::Stop { stream: inp });
            eprintln!("stream {inp}: recorded {got} bytes on a 32 KiB credit");
            assert!(got <= 32 * 1024, "within the credit");
            recorded |= got > 0;
            std::thread::sleep(Duration::from_millis(200));
            let _ = rx.try_iter().count();
            std::thread::sleep(Duration::from_millis(300));
            assert!(!rx.try_iter().any(|m| matches!(m, Msg::Data { stream, .. } if stream == inp)), "no data after Stop");
        }
        assert!(recorded || devices.iter().all(|d| d.1 == Dir::Out), "no input recorded");
    }
}
