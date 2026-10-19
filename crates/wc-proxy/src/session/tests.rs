//! The translation, driven by a scripted client rather than by a compositor.
//!
//! These are the tests that matter most in this crate, because they exercise
//! the exact path a `wl_shm` frame takes and they do it without Wine, without a
//! window and without a Windows binary. Stage 3's evidence had to be split in
//! two halves for want of a client that could reach both (docs/waycompose-design.md §8.5);
//! this half needs neither.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;

use super::*;
use crate::pool::{Arena, SharedFile};
use crate::wire::{self, FdEncoding};

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "wc-session-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A client's pool: a real anonymous object with a real descriptor, exactly as
/// libwayland would produce.
fn client_pool(size: usize) -> (OwnedFd, *mut u8) {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    // SAFETY: a NUL-terminated literal name and a valid flag.
    let raw = unsafe { libc::memfd_create(c"wc-test-pool".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0, "memfd_create failed");
    // SAFETY: freshly returned by `memfd_create`, owned from here.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: a live fd and a non-negative length.
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), size as i64) }, 0);
    // SAFETY: kernel-chosen address, non-zero length, live fd.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    assert_ne!(ptr, libc::MAP_FAILED);
    (fd, ptr.cast())
}

/// Drives a session the way a client would, and reads the compositor's side
/// back the way the compositor would.
struct Harness {
    session: Session,
    /// Everything the session produced for the compositor, concatenated.
    upstream: Vec<u8>,
    next_id: u32,
}

impl Harness {
    fn new(transport: Box<dyn PoolTransport>, scope: CopyScope, headroom: u64) -> Harness {
        Harness {
            session: Session::new(Spec::builtin().unwrap(), transport, scope, headroom),
            upstream: Vec::new(),
            next_id: 2,
        }
    }

    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn send(&mut self, object: u32, opcode: u16, body: &[u8], fds: &mut VecDeque<OwnedFd>) {
        let h = Header { object, opcode, size: 8 + body.len() };
        let out = self
            .session
            .client_message(h, body, fds)
            .unwrap_or_else(|e| panic!("object {object} opcode {opcode}: {e}"));
        self.upstream.extend_from_slice(&out);
    }

    fn send_words(&mut self, object: u32, opcode: u16, words: &[u32]) {
        let body: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
        self.send(object, opcode, &body, &mut VecDeque::new());
    }

    /// Bind a global the way `wl_registry.bind` does.
    fn bind(&mut self, registry: u32, name: u32, interface: &str, version: u32) -> u32 {
        let id = self.id();
        let mut body = Vec::new();
        body.extend_from_slice(&name.to_ne_bytes());
        let mut nul = interface.as_bytes().to_vec();
        nul.push(0);
        wire::push_bytes(&mut body, &nul);
        body.extend_from_slice(&version.to_ne_bytes());
        body.extend_from_slice(&id.to_ne_bytes());
        self.send(registry, 0, &body, &mut VecDeque::new());
        id
    }

    /// Every message the compositor would have received, re-walked from the
    /// concatenated stream. Walking it rather than remembering it is the point:
    /// it proves the bytes really do form a well-framed stream in *our*
    /// dialect, which is the failure mode docs/waycompose-design.md §2.4 warns about.
    fn upstream_messages(&self) -> Vec<(u32, u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while let Some(h) = Header::parse(&self.upstream[at..]) {
            assert!(h.size >= 8 && at + h.size <= self.upstream.len(), "stream is not framed");
            out.push((h.object, h.opcode, self.upstream[at + 8..at + h.size].to_vec()));
            at += h.size;
        }
        assert_eq!(at, self.upstream.len(), "trailing bytes in the upstream stream");
        out
    }

    /// The descriptor the compositor was given for a pool, decoded from the
    /// bytes that actually crossed.
    ///
    /// §109: the last `create_pool` that is not the 4 KiB stub, which is the
    /// extent of the buffer most recently backed, at its offset 0.
    fn pool_descriptor(&self) -> Descriptor {
        let spec = Spec::builtin().unwrap();
        let args = &spec.request("wl_shm", 0).unwrap().args;
        for (_, opcode, body) in self.upstream_messages().into_iter().rev() {
            if opcode != 0 {
                continue;
            }
            if let Ok(spans) = wire::walk(&body, args, FdEncoding::InBand)
                && wire::word_at(&body, &spans[2]) != Some(4096)
                && let Some(bytes) = wire::bytes_at(&body, &spans[1])
                && let Ok(d) = Descriptor::decode(bytes)
            {
                return d;
            }
        }
        panic!("no create_pool with a descriptor was forwarded");
    }
}

/// Everything a `wl_shm` client does, in order, painting a checkable picture.
///
/// The picture is stage 3's: a flat field, and 16x16 markers in three corners
/// and **not** the fourth. The asymmetry is what makes a flip, a mirror and a
/// row offset all fail one measurement, and it is why it is worth drawing
/// something specific rather than noise.
struct Frame {
    w: i32,
    h: i32,
    stride: i32,
    offset: i32,
}

impl Frame {
    fn paint(&self, pixels: &mut [u8], tick: u8) {
        let put = |p: &mut [u8], x: i32, y: i32, c: [u8; 4]| {
            let at = (self.offset + y * self.stride + x * 4) as usize;
            p[at..at + 4].copy_from_slice(&c);
        };
        for y in 0..self.h {
            for x in 0..self.w {
                put(pixels, x, y, [0x20, 0x20, 0x20, 0xff]);
            }
        }
        for (cx, cy, c) in [
            (1, 1, [0x00, 0x00, 0xff, 0xff]),          // red, top-left
            (self.w - 17, 1, [0x00, 0xff, 0x00, 0xff]), // green, top-right
            (1, self.h - 17, [0xff, 0x00, 0x00, 0xff]), // blue, bottom-left
        ] {
            for y in cy..cy + 16 {
                for x in cx..cx + 16 {
                    put(pixels, x, y, c);
                }
            }
        }
        // The moving element, so that "the pixels really change" is checkable.
        for y in 40..60 {
            put(pixels, 30 + i32::from(tick), y, [0xff, 0xff, 0xff, 0xff]);
        }
    }
}

/// Run one client's worth of traffic and hand back the harness plus what the
/// peer can see.
fn run_client(
    transport: Box<dyn PoolTransport>,
    scope: CopyScope,
    frames: u8,
    damage_all: bool,
) -> (Harness, Frame, usize) {
    let frame = Frame { w: 200, h: 120, stride: 200 * 4, offset: 0 };
    let pool_size = (frame.stride * frame.h) as usize;

    let mut h = Harness::new(transport, scope, 1);
    let registry = h.id();
    h.send_words(1, 1, &[registry]); // wl_display.get_registry
    let compositor = h.bind(registry, 1, "wl_compositor", 6);
    let shm = h.bind(registry, 2, "wl_shm", 2);

    let surface = h.id();
    h.send_words(compositor, 0, &[surface]); // create_surface

    let (fd, ptr) = client_pool(pool_size);
    // SAFETY: `ptr`/`pool_size` are what `mmap` returned in `client_pool`, and
    // this slice is the only reference to them for the rest of the test.
    let pixels = unsafe { std::slice::from_raw_parts_mut(ptr, pool_size) };

    let pool = h.id();
    let mut fds = VecDeque::from([fd]);
    let mut body = Vec::new();
    body.extend_from_slice(&pool.to_ne_bytes());
    body.extend_from_slice(&(pool_size as u32).to_ne_bytes());
    h.send(shm, 0, &body, &mut fds); // wl_shm.create_pool
    assert!(fds.is_empty(), "the descriptor must have been consumed");

    let buffer = h.id();
    h.send_words(
        pool,
        0, // create_buffer
        &[
            buffer,
            frame.offset as u32,
            frame.w as u32,
            frame.h as u32,
            frame.stride as u32,
            1, // xrgb8888
        ],
    );

    for tick in 0..frames {
        frame.paint(pixels, tick);
        h.send_words(surface, 1, &[buffer, 0, 0]); // attach
        if damage_all {
            h.send_words(surface, 9, &[0, 0, frame.w as u32, frame.h as u32]); // damage_buffer
        } else {
            // Only the moving element, which is what a real client sends and
            // what makes `CopyScope::Damage` differ from the others.
            h.send_words(surface, 9, &[30, 40, 32, 20]);
        }
        h.send_words(surface, 6, &[]); // commit
    }
    (h, frame, pool_size)
}

/// The claim of the stage, in one test, for one transport: the pixels a client
/// drew are readable by a peer that was given **only a descriptor**, at the
/// right coordinates, with the fourth corner unmarked.
fn pixels_arrive(transport: Box<dyn PoolTransport>, scope: CopyScope, name: &str) {
    let (h, frame, _) = run_client(transport, scope, 3, true);
    let descriptor = h.pool_descriptor();

    // The one thing that must never be true.
    assert!(
        !matches!(descriptor, Descriptor::NtHandle { .. }),
        "{name}: a handle crossed the boundary"
    );

    let peer = wc_handle::Handle::from_descriptor(descriptor);
    let map = peer.map().unwrap();
    let p = map.as_slice();
    let at = |x: i32, y: i32| -> [u8; 4] {
        let i = (frame.offset + y * frame.stride + x * 4) as usize;
        p[i..i + 4].try_into().unwrap()
    };

    assert_eq!(at(8, 8), [0x00, 0x00, 0xff, 0xff], "{name}: top-left marker");
    assert_eq!(at(frame.w - 9, 8), [0x00, 0xff, 0x00, 0xff], "{name}: top-right marker");
    assert_eq!(at(8, frame.h - 9), [0xff, 0x00, 0x00, 0xff], "{name}: bottom-left marker");
    // The corner that is deliberately *not* marked. A vertical or horizontal
    // flip puts a marker here and nothing else in this test would notice.
    assert_eq!(
        at(frame.w - 9, frame.h - 9),
        [0x20, 0x20, 0x20, 0xff],
        "{name}: the fourth corner must be unmarked -- the image is flipped"
    );
    // And the last frame's moving element, not the first's.
    assert_eq!(at(32, 50), [0xff, 0xff, 0xff, 0xff], "{name}: the bar did not move");
    assert_eq!(at(30, 50), [0x20, 0x20, 0x20, 0xff], "{name}: a stale bar was left behind");
}

#[test]
fn a_shared_file_carries_the_clients_pixels() {
    let d = tmpdir("sf");
    let t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
    pixels_arrive(Box::new(t), CopyScope::Damage, "shared-file");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_arena_slice_carries_the_clients_pixels() {
    let d = tmpdir("ar");
    let id = 7200;
    let path = d.join("arena");
    let t = Arena::open_or_create(id, &path, 4 << 20).unwrap();
    wc_handle::arena::attach(id, &path.to_string_lossy(), t.size()).unwrap();
    pixels_arrive(Box::new(t), CopyScope::Damage, "arena");
    let _ = std::fs::remove_dir_all(&d);
}

/// All three copy scopes must produce identical pixels for a client that
/// damages honestly. They differ in cost, never in result — which is exactly
/// what makes the cost measurement in docs/waycompose-design.md §9 a choice rather than a
/// trade-off.
#[test]
fn every_copy_scope_produces_the_same_image() {
    let mut images = Vec::new();
    for scope in [CopyScope::Pool, CopyScope::Buffer, CopyScope::Damage] {
        let d = tmpdir(&format!("scope-{}", scope.as_str()));
        let t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
        let (h, _, size) = run_client(Box::new(t), scope, 4, true);
        let peer = wc_handle::Handle::from_descriptor(h.pool_descriptor());
        images.push((scope, peer.map().unwrap().as_slice()[..size].to_vec()));
        let _ = std::fs::remove_dir_all(&d);
    }
    for w in images.windows(2) {
        assert_eq!(
            w[0].1,
            w[1].1,
            "{} and {} disagree",
            w[0].0.as_str(),
            w[1].0.as_str()
        );
    }
}

/// A client that cycles buffers, the way every real toolkit does.
///
/// The distinction this exercises is the one that broke scrolling — see
/// docs/waycompose-design.md §27. `wl_surface.damage` is "what changed on the
/// **surface** since the last commit", not "what I wrote into this **buffer**".
/// A client that double-buffers repaints the whole back buffer (or copies the front
/// buffer into it and then repaints the delta) and still reports only the
/// frame's delta, because that is all that changed on screen. A compositor
/// reading the buffer directly is fine. A proxy keeping a *per-buffer* shadow
/// is not: the back buffer's shadow is stale everywhere the surface changed
/// while some *other* buffer was on screen.
///
/// `canvas` is what the client believes the surface looks like. Every frame it
/// changes one band, writes the **whole** canvas into the next buffer in the
/// cycle, and damages only the band. So the attached buffer is always exactly
/// right, and any disagreement in the region is the proxy's alone.
struct Cycler {
    /// Damage the whole buffer every frame, the way `weston-presentation-shm`
    /// does. The arrears of such a client are N copies of the extent, which is
    /// the case that has to collapse to one.
    damage_all: bool,
    frame: Frame,
    buffers: Vec<u32>,
    canvas: Vec<u8>,
}

impl Cycler {
    /// The colour of band `tick`, chosen so that a band from the wrong frame is
    /// a different colour rather than a plausible one.
    fn band_colour(tick: u8) -> [u8; 4] {
        [tick.wrapping_mul(37), tick.wrapping_mul(91), tick.wrapping_mul(13), 0xff]
    }

    /// The band frame `tick` repaints: full width, four rows, walking down.
    fn band(&self, tick: u8) -> (i32, i32) {
        let y = (i32::from(tick) * 4) % (self.frame.h - 4);
        (y, 4)
    }

    fn tick(&mut self, h: &mut Harness, surface: u32, pixels: &mut [u8], tick: u8) {
        let (y0, bh) = self.band(tick);
        let c = Self::band_colour(tick);
        for y in y0..y0 + bh {
            for x in 0..self.frame.w {
                let at = (y * self.frame.stride + x * 4) as usize;
                self.canvas[at..at + 4].copy_from_slice(&c);
            }
        }
        // Write the whole canvas into the buffer about to be attached. Legal,
        // and what a front-to-back copy amounts to.
        let buffer = self.buffers[usize::from(tick) % self.buffers.len()];
        let base = (usize::from(tick) % self.buffers.len()) * self.canvas.len();
        pixels[base..base + self.canvas.len()].copy_from_slice(&self.canvas);

        h.send_words(surface, 1, &[buffer, 0, 0]); // attach
        let (dy, dh) =
            if self.damage_all { (0, self.frame.h) } else { (y0, bh) };
        h.send_words(surface, 9, &[0, dy as u32, self.frame.w as u32, dh as u32]); // damage_buffer
        h.send_words(surface, 6, &[]); // commit
    }
}

/// Run a buffer-cycling client and hand back the harness, the cycler and the
/// byte offset of the buffer that was attached last.
fn run_cycling_client(
    transport: Box<dyn PoolTransport>,
    scope: CopyScope,
    nbuf: usize,
    frames: u8,
    damage_all: bool,
) -> (Harness, Cycler, usize) {
    let frame = Frame { w: 200, h: 120, stride: 200 * 4, offset: 0 };
    let one = (frame.stride * frame.h) as usize;
    let pool_size = one * nbuf;

    let mut h = Harness::new(transport, scope, 1);
    let registry = h.id();
    h.send_words(1, 1, &[registry]);
    let compositor = h.bind(registry, 1, "wl_compositor", 6);
    let shm = h.bind(registry, 2, "wl_shm", 2);

    let surface = h.id();
    h.send_words(compositor, 0, &[surface]);

    let (fd, ptr) = client_pool(pool_size);
    // SAFETY: `ptr`/`pool_size` are what `mmap` returned in `client_pool`, and
    // this slice is the only reference to them for the rest of the test.
    let pixels = unsafe { std::slice::from_raw_parts_mut(ptr, pool_size) };

    let pool = h.id();
    let mut fds = VecDeque::from([fd]);
    let mut body = Vec::new();
    body.extend_from_slice(&pool.to_ne_bytes());
    body.extend_from_slice(&(pool_size as u32).to_ne_bytes());
    h.send(shm, 0, &body, &mut fds);

    let mut buffers = Vec::new();
    for i in 0..nbuf {
        let buffer = h.id();
        h.send_words(
            pool,
            0,
            &[
                buffer,
                (i * one) as u32,
                frame.w as u32,
                frame.h as u32,
                frame.stride as u32,
                1,
            ],
        );
        buffers.push(buffer);
    }

    let mut c = Cycler { damage_all, frame, buffers, canvas: vec![0x20; one] };
    for tick in 0..frames {
        c.tick(&mut h, surface, pixels, tick);
    }
    let last = (usize::from(frames - 1) % nbuf) * one;
    (h, c, last)
}

/// **The bug this stage exists for.** A buffer-cycling client's last frame must
/// be readable in full from the region, and it was not: everything the surface
/// changed while a *different* buffer was on screen was missing from this
/// buffer's shadow. On a scrolling page that is a band of two-frames-ago
/// content per frame, which is what "blank slides in about:support" was.
///
/// `Pool` and `Buffer` scope copy the whole extent every commit, so they cannot
/// have the bug. That makes them the control: if `Damage` disagrees with them,
/// the disagreement is damage tracking's and nothing else's.
#[test]
fn a_buffer_cycling_client_is_not_left_with_a_stale_shadow() {
    for nbuf in [2usize, 3] {
        let mut images = Vec::new();
        for scope in [CopyScope::Pool, CopyScope::Buffer, CopyScope::Damage] {
            let d = tmpdir(&format!("cycle-{nbuf}-{}", scope.as_str()));
            let t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
            // §109: each buffer is its own extent; the last attached (buffer
            // 11 % nbuf) is also the last backed, so the last re-point is its.
            let (h, c, _last) = run_cycling_client(Box::new(t), scope, nbuf, 12, false);
            let peer = wc_handle::Handle::from_descriptor(h.pool_descriptor());
            let map = peer.map().unwrap();
            let seen = map.as_slice()[..c.canvas.len()].to_vec();
            images.push((scope, seen, c.canvas.clone()));
            let _ = std::fs::remove_dir_all(&d);
        }
        for (scope, seen, canvas) in &images {
            // Count and locate the disagreeing rows rather than dumping 96000
            // bytes. The count is the measurement docs/waycompose-design.md §27 quotes.
            let stride = 200 * 4usize;
            let rows: Vec<usize> = (0..120)
                .filter(|y| seen[y * stride..(y + 1) * stride] != canvas[y * stride..(y + 1) * stride])
                .collect();
            assert!(
                rows.is_empty(),
                "{nbuf} buffers, {} scope: {} of 120 rows in the region are not what the client \
                 drew (rows {:?})",
                scope.as_str(),
                rows.len(),
                &rows[..rows.len().min(12)]
            );
        }
    }
}

/// Paying off the arrears must not degenerate into copying the extent.
///
/// There is a trivially correct fix for the staleness above — copy the whole
/// buffer every commit — and it is `CopyScope::Buffer`, which already exists.
/// This is the test that says the damage scope is still the cheap one after the
/// fix: the arrears of a two-buffer cycle are one extra frame's damage, not a
/// whole buffer. Without it, a later "just to be safe" would quietly make the
/// three scopes the same code again.
#[test]
fn paying_off_the_arrears_still_costs_less_than_the_extent() {
    let mut bytes = Vec::new();
    for scope in [CopyScope::Buffer, CopyScope::Damage] {
        let d = tmpdir(&format!("arrears-{}", scope.as_str()));
        let t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
        let (h, _, _) = run_cycling_client(Box::new(t), scope, 2, 24, false);
        bytes.push((scope, h.session.stats.bytes_copied));
        let _ = std::fs::remove_dir_all(&d);
    }
    let get = |s: CopyScope| bytes.iter().find(|(k, _)| *k == s).unwrap().1;
    // The arithmetic, because a ratio with no derivation is a number nobody can
    // check. The client damages 4 rows of 120 a frame and cycles two buffers,
    // so a steady-state frame owes 8 rows — 15x cheaper than the extent. The
    // run also pays two unavoidable full extents, one per buffer's first
    // commit, which nothing can avoid because the region has never held those
    // bytes: 2 x 96000 + 22 x 8 x 800 = 332800 against 24 x 96000 = 2304000,
    // which is 6.9x. The bound is 4x, loose enough to survive a change in the
    // frame count and tight enough that "copy the extent to be safe" fails it.
    assert!(
        get(CopyScope::Damage) * 4 < get(CopyScope::Buffer),
        "damage {} vs buffer {} -- the arrears fix has eaten the saving",
        get(CopyScope::Damage),
        get(CopyScope::Buffer)
    );
}

/// **A client that damages everything must never cost more than `Buffer`.**
///
/// This is the test the arrears fix shipped without, and the regression it let
/// through is the reason it exists. `weston-presentation-shm` damages its whole
/// 250x250 buffer every frame and cycles buffers, so a buffer coming back round
/// owes one *full-buffer* rectangle per intervening frame — and `copy_damage`
/// copies overlapping rectangles as many times as it is given them, on purpose,
/// because unioning two of them costs more than copying twice. Sixty is not
/// two. `scripts/wc-proxy.sh cost` went from 168 MiB to 8583 MiB for the same
/// 700 commits, 13 MiB per copy of a 244 KiB buffer, and the harness still
/// reported 37 passed 0 failed because it prints that table and does not check
/// it.
///
/// `<=` rather than `<`: with whole-buffer damage the two scopes are copying
/// the same pixels, and the honest assertion is that damage never costs *more*.
/// The strict inequality belongs to the partial-damage test above.
#[test]
fn whole_buffer_damage_never_costs_more_than_the_extent() {
    for nbuf in [2usize, 4] {
        let mut bytes = Vec::new();
        for scope in [CopyScope::Buffer, CopyScope::Damage] {
            let d = tmpdir(&format!("whole-{nbuf}-{}", scope.as_str()));
            let t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
            let (h, _, _) = run_cycling_client(Box::new(t), scope, nbuf, 40, true);
            bytes.push((scope, h.session.stats.bytes_copied));
            let _ = std::fs::remove_dir_all(&d);
        }
        let get = |s: CopyScope| bytes.iter().find(|(k, _)| *k == s).unwrap().1;
        assert!(
            get(CopyScope::Damage) <= get(CopyScope::Buffer),
            "{nbuf} buffers: damage copied {} bytes against buffer's {} -- arrears are \
             being copied once per rectangle instead of collapsing to the extent",
            get(CopyScope::Damage),
            get(CopyScope::Buffer)
        );
    }
}

/// Damage-scoped copying really does copy less. Without this the scope is a
/// setting that changes nothing, and the measurement it exists for would be
/// measuring the same code three times.
#[test]
fn damage_scope_copies_less_than_the_other_two() {
    let mut bytes = Vec::new();
    for scope in [CopyScope::Pool, CopyScope::Buffer, CopyScope::Damage] {
        let d = tmpdir(&format!("cost-{}", scope.as_str()));
        let t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
        let (h, _, _) = run_client(Box::new(t), scope, 6, false);
        bytes.push((scope, h.session.stats.bytes_copied));
        let _ = std::fs::remove_dir_all(&d);
    }
    let get = |s: CopyScope| bytes.iter().find(|(k, _)| *k == s).unwrap().1;
    assert!(
        get(CopyScope::Damage) < get(CopyScope::Buffer),
        "damage {} vs buffer {}",
        get(CopyScope::Damage),
        get(CopyScope::Buffer)
    );
    assert!(get(CopyScope::Buffer) <= get(CopyScope::Pool));
}

/// The descriptor really is the only thing that crossed. Asserted on the bytes
/// rather than on the API, because the API could be right and the encoder
/// wrong.
#[test]
fn nothing_but_a_name_or_an_offset_reaches_the_compositor() {
    for arena in [false, true] {
        let d = tmpdir(if arena { "wire-ar" } else { "wire-sf" });
        let transport: Box<dyn PoolTransport> = if arena {
            let id = 7300;
            let t = Arena::open_or_create(id, &d.join("arena"), 4 << 20).unwrap();
            wc_handle::arena::attach(id, &d.join("arena").to_string_lossy(), t.size()).unwrap();
            Box::new(t)
        } else {
            Box::new(SharedFile::new(&d, "Z:\\pools\\").unwrap())
        };
        let (h, _, _) = run_client(transport, CopyScope::Damage, 1, true);
        match h.pool_descriptor() {
            Descriptor::SharedFile { path, size } => {
                assert!(path.starts_with("Z:\\pools\\"), "{path}");
                assert!(size > 0);
            }
            Descriptor::ArenaSlice { arena: a, offset, len } => {
                assert_eq!(a, 7300);
                assert_eq!(offset % crate::pool::ARENA_GRANULARITY, 0);
                assert!(len > 0);
            }
            other => panic!("{other:?} must not cross the boundary"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// A `wl_shm.create_pool` with no descriptor is the §2.4 failure. It must be a
/// named error and never a message forwarded with a zero-length descriptor,
/// because the second desynchronises the compositor's parser silently.
#[test]
fn a_missing_descriptor_is_a_named_error() {
    let d = tmpdir("nofd");
    let t = SharedFile::new(&d, "Z:\\").unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
    let registry = h.id();
    h.send_words(1, 1, &[registry]);
    let shm = h.bind(registry, 2, "wl_shm", 2);
    let pool = h.id();
    let mut body = Vec::new();
    body.extend_from_slice(&pool.to_ne_bytes());
    body.extend_from_slice(&4096u32.to_ne_bytes());
    let err = h
        .session
        .client_message(Header { object: shm, opcode: 0, size: 16 }, &body, &mut VecDeque::new())
        .unwrap_err();
    assert!(matches!(err, Error::MissingFd { .. }), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

/// A message on an object of unknown type is fatal rather than forwarded.
/// Forwarding it is what would desynchronise the stream, and the whole reason
/// the signature table exists.
#[test]
fn a_message_on_an_unknown_object_is_fatal() {
    let d = tmpdir("unk");
    let t = SharedFile::new(&d, "Z:\\").unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
    let err = h
        .session
        .client_message(Header { object: 4242, opcode: 0, size: 8 }, &[], &mut VecDeque::new())
        .unwrap_err();
    assert!(matches!(err, Error::UnknownObject { id: 4242, .. }), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

/// `wl_display.delete_id` must retire the id, and a client reusing it for
/// another interface must then be walked with the new interface's signatures.
/// Frame callbacks make this happen every frame, so getting it wrong breaks
/// after exactly one.
#[test]
fn delete_id_retires_an_id_for_reuse() {
    let d = tmpdir("del");
    let t = SharedFile::new(&d, "Z:\\").unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
    let registry = h.id();
    h.send_words(1, 1, &[registry]);
    let compositor = h.bind(registry, 1, "wl_compositor", 6);
    let surface = h.id();
    h.send_words(compositor, 0, &[surface]);

    // A frame callback, then the server retires it, then the same number comes
    // back as something else.
    let cb = h.id();
    h.send_words(surface, 3, &[cb]); // wl_surface.frame
    assert_eq!(h.session.objects.interface(cb), Some("wl_callback"));

    let body = cb.to_ne_bytes().to_vec();
    h.session
        .server_message(Header { object: 1, opcode: 1, size: 12 }, &body)
        .unwrap();
    assert_eq!(h.session.objects.interface(cb), None);
    let _ = std::fs::remove_dir_all(&d);
}

/// Set up a pool the way toytoolkit does — create small, then grow by doubling
/// before touching anything else — and return the harness with the pool's id.
fn deferred_pool(h: &mut Harness, initial: usize, grow_to: &[usize]) -> (u32, u32) {
    let registry = h.id();
    h.send_words(1, 1, &[registry]);
    let shm = h.bind(registry, 2, "wl_shm", 2);
    let biggest = grow_to.last().copied().unwrap_or(initial).max(initial);
    let (fd, _) = client_pool(biggest);
    let pool = h.id();
    let mut body = Vec::new();
    body.extend_from_slice(&pool.to_ne_bytes());
    body.extend_from_slice(&(initial as u32).to_ne_bytes());
    h.send(shm, 0, &body, &mut VecDeque::from([fd]));
    for size in grow_to {
        h.send_words(pool, 2, &[*size as u32]);
    }
    (pool, registry)
}

/// The lifetime rule, from the sending side.
///
/// `weston-presentation-shm` creates a pool, creates its buffers and destroys
/// the `wl_shm_pool` immediately — legal and idiomatic, because the `wl_shm`
/// spec keeps the memory alive for the buffers. All three requests reach this
/// proxy before the compositor has read any of them, so a proxy that released
/// the region on `destroy` unlinked the file the compositor was about to open.
/// Measured, against `wc-compositor.exe`: `could not map SharedFile { … }: File
/// not found`, reported to the client as a `wl_shm` protocol error, zero frames
/// presented. This is docs/waycompose-design.md §2.5 exactly.
#[test]
fn a_destroyed_pool_stays_open_until_the_compositor_acknowledges_it() {
    let d = tmpdir("zombie");
    let t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Buffer, 1);
    let (pool, registry) = deferred_pool(&mut h, 64 * 1024, &[]);
    let buffer = h.id();
    h.send_words(pool, 0, &[buffer, 0, 64, 64, 256, 1]);

    // Destroy the pool, as the client is entitled to.
    h.send_words(pool, 1, &[]);
    assert_eq!(h.session.stats.pools_released, 0, "released too early");

    // The name must still resolve: this is the compositor's own call, made at
    // the moment it reads the `create_pool` that is still in flight.
    let peer = wc_handle::Handle::from_descriptor(h.pool_descriptor());
    assert!(peer.map().is_ok(), "the region was unlinked before the peer could open it");

    // And the buffers cut from it still work, which is what the spec requires.
    let surface = h.id();
    let compositor = h.bind(registry, 1, "wl_compositor", 6);
    h.send_words(compositor, 0, &[surface]);
    h.send_words(surface, 1, &[buffer, 0, 0]);
    h.send_words(surface, 6, &[]);
    assert_eq!(h.session.stats.copies, 1, "a zombie pool's buffer stopped copying");

    // The compositor acknowledging the destructor is still not enough: a live
    // buffer keeps the memory, which is the spec's own rule and is what
    // `weston-presentation-shm` relies on for 784 frames after destroying its
    // pool. An earlier version released here and the window froze on frame one
    // while the compositor cheerfully presented the same bytes 783 more times.
    let body = pool.to_ne_bytes().to_vec();
    h.session
        .server_message(Header { object: 1, opcode: 1, size: 12 }, &body)
        .unwrap();
    assert_eq!(h.session.stats.pools_released, 0, "released while a buffer was live");
    h.send_words(surface, 6, &[]);
    assert_eq!(h.session.stats.copies, 2, "copying stopped while a buffer was live");

    // The last buffer going is the last condition.
    h.send_words(buffer, 0, &[]); // wl_buffer.destroy
    assert_eq!(h.session.stats.pools_released, 1);
    let _ = std::fs::remove_dir_all(&d);
}

/// The reverse direction. An `h` in an event arrives in-band and must leave as
/// a real descriptor plus a shorter body — and the descriptor must refer to the
/// bytes the name pointed at, which is what proves nothing was passed.
#[test]
fn an_event_carrying_a_descriptor_becomes_a_real_fd_for_the_client() {
    let d = tmpdir("keymap");
    let t = SharedFile::new(&d, "Z:\\").unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);

    // A keymap-shaped object, created here and named rather than passed.
    let path = d.join("keymap");
    let keymap = b"xkb_keymap { /* a keymap would go here */ };\0";
    let owner = wc_handle::Handle::create_shared_file(
        &path.to_string_lossy(),
        &path.to_string_lossy(),
        4096,
    )
    .unwrap();
    owner.write_at(0, keymap).unwrap();

    // Give the session a wl_keyboard to hang the event on.
    let registry = h.id();
    h.send_words(1, 1, &[registry]);
    let seat = h.bind(registry, 3, "wl_seat", 7);
    let kbd = h.id();
    h.send_words(seat, 1, &[kbd]); // wl_seat.get_keyboard
    assert_eq!(h.session.objects.interface(kbd), Some("wl_keyboard"));

    // wl_keyboard.keymap(uint format, h fd, uint size), in our dialect.
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_ne_bytes());
    wire::push_bytes(&mut body, &owner.descriptor().encode());
    body.extend_from_slice(&4096u32.to_ne_bytes());
    let (out, fds) = h
        .session
        .server_message(Header { object: kbd, opcode: 0, size: 8 + body.len() }, &body)
        .unwrap();

    assert_eq!(fds.len(), 1, "the client must get exactly one descriptor");
    // The body the client sees is the libwayland one: two words, no descriptor.
    assert_eq!(out.len(), 8 + 8, "the in-band descriptor was not removed");
    // And the fd really is the keymap.
    use std::io::Read as _;
    let mut f = std::fs::File::from(fds.into_iter().next().unwrap());
    let mut got = vec![0u8; keymap.len()];
    f.read_exact(&mut got).unwrap();
    assert_eq!(got, keymap);
    let _ = std::fs::remove_dir_all(&d);
}

/// The same event, over the arena, with **no filesystem the two sides share**.
///
/// STAGE 10, and it is the whole stage in one test. The compositor's half is
/// real rather than simulated: `wc_handle::arena::provide_file` and
/// `reserve_outbound` are exactly what `wc-compositor.exe` calls at startup, and
/// `Handle::create_shared` is exactly what smithay's `SealedFile` calls to make
/// a keymap. What crosses is three integers. This side reads them out of the
/// region it already had mapped and manufactures the client's descriptor
/// locally, which is the same rule as ever: **a name — or here, an offset — is
/// what crosses, never a descriptor.**
///
/// One test rather than several, because `wc_handle`'s outbound window is
/// process-global by design and `cargo test` runs these threads at once. It
/// therefore also carries the assertion that the two allocators over the one
/// region do not overlap, which is the property the window exists for.
#[test]
fn a_keymap_cut_from_the_arena_reaches_the_client_with_no_shared_filesystem() {
    let d = tmpdir("arenakeymap");
    let path = d.join("arena");
    let id = 7260;
    let size = 64 * 4096u64;
    let outbound = 8 * 4096u64;

    // This side: the proxy's allocator over the region, told to leave the last
    // eight pages alone.
    let mut t = Arena::open_or_create(id, &path, size).unwrap();
    t.reserve_outbound(outbound).unwrap();
    assert_eq!(t.outbound(), Some((size - outbound, outbound)));

    // The other side: the compositor's, in this process because the region is a
    // file and a file has no side.
    let p = path.to_string_lossy().into_owned();
    wc_handle::arena::provide_file(id, &p, size).unwrap();
    let w = wc_handle::arena::reserve_outbound(id, outbound).unwrap();
    assert_eq!((w.arena, w.offset, w.len), (id, size - outbound, outbound));

    let keymap = b"xkb_keymap { /* compiled by the compositor */ };\0";
    let owner = wc_handle::Handle::create_shared(keymap.len() as u64).unwrap();
    owner.write_at(0, keymap).unwrap();
    // Nothing was created. This is the assertion that distinguishes this
    // transport from every other one in the crate.
    assert!(
        matches!(owner.descriptor(), wc_handle::Descriptor::ArenaSlice { .. }),
        "create_shared did not cut from the arena: {:?}",
        owner.descriptor()
    );

    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
    let registry = h.id();
    h.send_words(1, 1, &[registry]);
    let seat = h.bind(registry, 3, "wl_seat", 7);
    let kbd = h.id();
    h.send_words(seat, 1, &[kbd]);

    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_ne_bytes());
    wire::push_bytes(&mut body, &owner.descriptor().encode());
    body.extend_from_slice(&(keymap.len() as u32).to_ne_bytes());
    let (out, fds) = h
        .session
        .server_message(Header { object: kbd, opcode: 0, size: 8 + body.len() }, &body)
        .unwrap();

    assert_eq!(fds.len(), 1, "the client must get exactly one descriptor");
    assert_eq!(out.len(), 8 + 8, "the in-band descriptor was not removed");
    assert_eq!(h.session.stats.fds_from_arena, 1, "the run must be able to say which kind ran");

    use std::io::Read as _;
    let mut f = std::fs::File::from(fds.into_iter().next().unwrap());
    let mut got = vec![0u8; keymap.len()];
    f.read_exact(&mut got).unwrap();
    assert_eq!(got, keymap, "the client's fd is not the compositor's keymap");

    // And the two allocators cannot meet. The pool allocator is asked for
    // everything it will give and none of it is inside the compositor's window
    // — which is the failure the reservation exists to prevent, and it would be
    // a client's pixels on top of a live keymap.
    let mut pools = Vec::new();
    while let Ok(r) = h.session.transport.create(4096) {
        let wc_handle::Descriptor::ArenaSlice { offset, .. } = r.descriptor else {
            panic!("wrong kind")
        };
        assert!(offset + 4096 <= w.offset, "pool at {offset} is inside the keymap window {w:?}");
        pools.push(r);
    }
    assert_eq!(pools.len() as u64, (size - outbound) / 4096, "the arena short-changed the peer");
    // The keymap is still what it was after the arena filled up.
    let again = wc_handle::Handle::from_descriptor(owner.descriptor().clone()).map().unwrap();
    assert_eq!(&again.as_slice()[..keymap.len()], keymap);

    let _ = std::fs::remove_dir_all(&d);
}

/// A keymap in a **Windows section** is refused, loudly, and the connection
/// ends.
///
/// This is the failure `wl_keyboard.keymap` had all along and that nothing
/// exercised — docs/waycompose-design.md §9.10 item 5's "implemented, unexercised". A
/// compositor that has not been told where to publish objects creates
/// `Local\wc-…`, which is a name only the NT object manager understands. The
/// old code noted it and forwarded the event *without* the descriptor, so the
/// client received a well-formed `keymap` and `mmap`ed a descriptor it never
/// got.
///
/// The assertion is on the message rather than only on the error, because the
/// whole value of failing here is that somebody reading the log learns what to
/// do about it.
#[test]
fn a_keymap_in_a_windows_section_is_a_diagnosis_rather_than_a_broken_client() {
    let d = tmpdir("ntsection");
    let t = SharedFile::new(&d, "Z:\\").unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);

    let registry = h.id();
    h.send_words(1, 1, &[registry]);
    let seat = h.bind(registry, 3, "wl_seat", 7);
    let kbd = h.id();
    h.send_words(seat, 1, &[kbd]);

    let descriptor = wc_handle::Descriptor::Section {
        name: "Local\\wc-0123456789abcdef0123456789abcdef".into(),
        size: 4096,
    };
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_ne_bytes());
    wire::push_bytes(&mut body, &descriptor.encode());
    body.extend_from_slice(&4096u32.to_ne_bytes());

    let err = h
        .session
        .server_message(Header { object: kbd, opcode: 0, size: 8 + body.len() }, &body)
        .expect_err("a section the client cannot open must not be forwarded");
    let text = format!("{err}");
    assert!(text.contains("wl_keyboard.keymap"), "{text}");
    assert!(text.contains("--shared-dir"), "the message must say what to do: {text}");
    let _ = std::fs::remove_dir_all(&d);
}

/// Messages the proxy has never heard of go through byte for byte. This is the
/// property that makes the proxy worth building — `docs/goal-firefox.md` needs
/// protocols nobody has written code for here.
#[test]
fn messages_the_proxy_does_not_understand_are_forwarded_verbatim() {
    let d = tmpdir("passthrough");
    let t = SharedFile::new(&d, "Z:\\").unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
    let registry = h.id();
    h.send_words(1, 1, &[registry]);
    let wm = h.bind(registry, 4, "xdg_wm_base", 6);
    let surface_id = h.id();
    let compositor = h.bind(registry, 1, "wl_compositor", 6);
    h.send_words(compositor, 0, &[surface_id]);
    let xdg = h.id();
    h.send_words(wm, 2, &[xdg, surface_id]); // get_xdg_surface
    let top = h.id();
    h.send_words(xdg, 1, &[top]); // get_toplevel

    let mut body = Vec::new();
    let mut title = b"Terminal \xe2\x80\x94 a title with multibyte in it".to_vec();
    title.push(0);
    wire::push_bytes(&mut body, &title);
    h.send(top, 2, &body, &mut VecDeque::new()); // set_title

    let msgs = h.upstream_messages();
    let (object, opcode, forwarded) = msgs.last().unwrap();
    assert_eq!((*object, *opcode), (top, 2));
    assert_eq!(forwarded, &body, "set_title was not forwarded byte for byte");
    let _ = std::fs::remove_dir_all(&d);
}

/// The whole upstream stream must be well framed in our dialect. This is the
/// end-to-end version of the encoding tests, and the thing that would catch a
/// size field computed from the wrong place after a splice.
#[test]
fn the_upstream_stream_is_well_framed_in_our_dialect() {
    let d = tmpdir("framing");
    let t = SharedFile::new(&d, "Z:\\pools\\").unwrap();
    let (h, _, _) = run_client(Box::new(t), CopyScope::Damage, 3, true);
    let spec = Spec::builtin().unwrap();
    let mut objects = crate::objects::Objects::new();
    let mut checked = 0;
    for (object, opcode, body) in h.upstream_messages() {
        let iface = objects
            .interface(object)
            .unwrap_or_else(|| panic!("object {object} has no interface"))
            .to_owned();
        let msg = spec
            .request(&iface, opcode)
            .unwrap_or_else(|| panic!("{iface} has no request {opcode}"));
        let spans = wire::walk(&body, &msg.args, FdEncoding::InBand)
            .unwrap_or_else(|e| panic!("{iface}.{}: {e}", msg.name));
        for (span, arg) in spans.iter().zip(&msg.args) {
            match arg.kind {
                crate::spec::ArgKind::NewId => {
                    if let (Some(id), Some(i)) =
                        (wire::word_at(&body, span), arg.interface.as_deref())
                    {
                        objects.record(id, i);
                    }
                }
                crate::spec::ArgKind::NewIdUntyped => {
                    if let Some((i, _, id)) = wire::untyped_new_id(&body, span) {
                        objects.record(id, &i);
                    }
                }
                _ => {}
            }
        }
        checked += 1;
    }
    assert!(checked >= 12, "only {checked} messages were forwarded");
    let _ = std::fs::remove_dir_all(&d);
}

/// The opcodes this file acts on, checked against the vendored XML. A protocol
/// bump that renumbered one would otherwise show up as a compositor that gets
/// no pixels and no error anywhere.
#[test]
fn the_opcodes_this_file_hard_codes_are_the_right_ones() {
    let s = Spec::builtin().unwrap();
    for (iface, opcode, name) in [
        ("wl_shm", op::SHM_CREATE_POOL, "create_pool"),
        ("wl_shm_pool", op::POOL_CREATE_BUFFER, "create_buffer"),
        ("wl_shm_pool", op::POOL_DESTROY, "destroy"),
        ("wl_shm_pool", op::POOL_RESIZE, "resize"),
        ("wl_buffer", op::BUFFER_DESTROY, "destroy"),
        ("wl_surface", op::SURFACE_DESTROY, "destroy"),
        ("wl_surface", op::SURFACE_ATTACH, "attach"),
        ("wl_surface", op::SURFACE_DAMAGE, "damage"),
        ("wl_surface", op::SURFACE_COMMIT, "commit"),
        ("wl_surface", op::SURFACE_DAMAGE_BUFFER, "damage_buffer"),
    ] {
        assert_eq!(s.request(iface, opcode).unwrap().name, name, "{iface} opcode {opcode}");
    }
    assert_eq!(s.event("wl_display", op::DISPLAY_DELETE_ID).unwrap().name, "delete_id");
}

// -------------------------------------------------------------------- 16 ---
// Running out of arena, and getting the guest's memory back.

/// A client with a surface, an arena of a stated size, and a way to drive one
/// pool's worth of traffic at a time.
///
/// Separate from `run_client` because these tests care about *which messages
/// reached the compositor* rather than about which pixels arrived, and they
/// have to be able to run out of room half way through.
struct Pressure {
    h: Harness,
    dir: std::path::PathBuf,
    shm: u32,
    compositor: u32,
}

impl Pressure {
    fn new(tag: &str, arena_id: u32, arena_bytes: u64) -> Pressure {
        let dir = tmpdir(tag);
        let path = dir.join("arena");
        let t = Arena::open_or_create(arena_id, &path, arena_bytes).unwrap();
        wc_handle::arena::attach(arena_id, &path.to_string_lossy(), t.size()).unwrap();
        let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
        let registry = h.id();
        h.send_words(1, 1, &[registry]);
        let compositor = h.bind(registry, 1, "wl_compositor", 6);
        let shm = h.bind(registry, 2, "wl_shm", 2);
        Pressure { h, dir, shm, compositor }
    }

    fn surface(&mut self) -> u32 {
        let s = self.h.id();
        self.h.send_words(self.compositor, 0, &[s]);
        s
    }

    fn attach_and_commit(&mut self, surface: u32, buffer: u32, w: i32, hgt: i32) {
        self.h.send_words(surface, 1, &[buffer, 0, 0]);
        self.h.send_words(surface, 9, &[0, 0, w as u32, hgt as u32]);
        self.h.send_words(surface, 6, &[]);
    }

    /// An event from the compositor, the way `pump` would deliver one.
    fn deliver_event(&mut self, object: u32, opcode: u16, words: &[u32]) {
        let body: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
        let (bytes, fds) = self
            .h
            .session
            .server_message(Header { object, opcode, size: 8 + body.len() }, &body)
            .unwrap_or_else(|e| panic!("event {object}.{opcode}: {e}"));
        assert!(fds.is_empty());
        let _ = bytes;
    }
}

impl Drop for Pressure {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ----------------------------------------------------------------------------
// §109: lazy pools, a backing per buffer
// ----------------------------------------------------------------------------

/// The compositor's object map, replayed over everything that crossed: the
/// property §109 exists for. Every message must name a live object, and every
/// new id must be one the compositor accepts — the next slot or a freed one,
/// never past the end (`Invalid new_id`) and never one in use. The compositor
/// frees an id at a destructor, which is what lets the proxy destroy and
/// re-create a pool or a buffer under the client's own id.
fn assert_in_step(h: &Harness) {
    let spec = Spec::builtin().unwrap();
    let mut live: HashMap<u32, String> = HashMap::from([(1, "wl_display".to_string())]);
    // The harness numbers from 3 (`Harness::id`), libwayland from 2.
    let mut slots = 2u32;
    for (i, (obj, opcode, body)) in h.upstream_messages().into_iter().enumerate() {
        let iface = live.get(&obj).unwrap_or_else(|| panic!("message {i} ({obj}.{opcode}) names no live object")).clone();
        let msg = spec.request(&iface, opcode).unwrap_or_else(|| panic!("{iface}.{opcode}?"));
        let spans = wire::walk(&body, &msg.args, FdEncoding::InBand).unwrap();
        for (span, arg) in spans.iter().zip(&msg.args) {
            let made = match arg.kind {
                ArgKind::NewId => wire::word_at(&body, span).zip(arg.interface.clone()),
                ArgKind::NewIdUntyped => wire::untyped_new_id(&body, span).map(|(iface, _, id)| (id, iface)),
                _ => None,
            };
            if let Some((id, iface)) = made {
                assert!(id <= slots + 1, "message {i}: new id {id} skips past {slots}: Invalid new_id");
                assert!(!live.contains_key(&id), "message {i}: new id {id} is in use ({:?})", live.get(&id));
                slots = slots.max(id);
                live.insert(id, iface);
            }
        }
        if msg.destructor {
            live.remove(&obj);
        }
    }
}

/// A client pool with a real object behind it, `size` bytes.
fn lazy_pool(h: &mut Harness, shm: u32, size: usize) -> (u32, *mut u8) {
    let (fd, ptr) = client_pool(size);
    let pool = h.id();
    let mut body = Vec::new();
    body.extend_from_slice(&pool.to_ne_bytes());
    body.extend_from_slice(&(size as u32).to_ne_bytes());
    h.send(shm, 0, &body, &mut VecDeque::from([fd]));
    (pool, ptr)
}

/// `weston-terminal`'s nine doublings before any other request. Until §109 the
/// pool was held back until it stopped growing; now it is the stub at once,
/// every resize is forwarded (at the stub's size, which the compositor takes
/// as no change), and nothing of the arena is held until a buffer cut from it
/// is attached — and then only the buffer's bytes, not the pool's.
#[test]
fn a_pool_is_the_stub_at_once_and_every_resize_is_forwarded() {
    let d = tmpdir("p109-doublings");
    let t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
    let sizes = [12288usize, 28672, 61440, 126976, 258048, 520192, 1044480, 2093056, 4190208];
    let (pool, registry) = deferred_pool(&mut h, 4096, &sizes);
    let msgs = h.upstream_messages();
    // get_registry, bind, create_pool, nine resizes: every request crossed.
    assert_eq!(msgs.len(), 12, "{msgs:?}");
    assert_eq!(h.session.stats.pools_created, 1);
    for (_, opcode, body) in &msgs[3..] {
        assert_eq!(*opcode, op::POOL_RESIZE);
        assert_eq!(body, &4096u32.to_ne_bytes(), "the compositor's pool is the stub");
    }
    assert_eq!(h.session.stats.backed_bytes_now, 0, "no arena bytes for a pool");

    // A buffer: a 1x1 stub, still nothing held.
    let buffer = h.id();
    h.send_words(pool, 0, &[buffer, 0, 800, 600, 3200, 1]);
    let last = h.upstream_messages().pop().unwrap();
    assert_eq!(last.0, pool);
    assert_eq!(last.2[4..24], [0u32, 1, 1, 4, 1].iter().flat_map(|w| w.to_ne_bytes()).collect::<Vec<u8>>()[..]);
    assert_eq!(h.session.stats.backed_bytes_now, 0);

    // Attached: its own extent, 800x600x4 and not the pool's 4 MiB.
    let compositor = h.bind(registry, 1, "wl_compositor", 6);
    let surface = h.id();
    h.send_words(compositor, 0, &[surface]);
    h.send_words(surface, 1, &[buffer, 0, 0]);
    match h.pool_descriptor() {
        Descriptor::SharedFile { size, .. } => assert_eq!(size, 800 * 600 * 4),
        other => panic!("{other:?}"),
    }
    assert_eq!(h.session.stats.buffers_backed, 1);
    assert_in_step(&h);
    let _ = std::fs::remove_dir_all(&d);
}

/// The user's case: a 512 MiB pool (foot's default) against a 32 MiB arena.
/// Buffers cut from it are created, attached, destroyed and the pool resized;
/// the arena holds what the buffers cover, the pixels arrive, and the stream
/// stays in step throughout. Then more buffers than fit at once: the cold ones
/// are reclaimed, and the client never sees it.
#[test]
fn a_512_mib_pool_runs_in_a_32_mib_arena() {
    let mut p = Pressure::new("p109-512", 9109, 32 << 20);
    let surface = p.surface();
    let (pool, ptr) = lazy_pool(&mut p.h, p.shm, 512 << 20);
    let (w, hgt) = (1024i32, 768i32);
    let one = (w * 4 * hgt) as usize; // 3 MiB
    let mut buffers = Vec::new();
    for i in 0..4usize {
        let b = p.h.id();
        p.h.send_words(pool, 0, &[b, (i * one) as u32, w as u32, hgt as u32, (w * 4) as u32, 1]);
        buffers.push(b);
    }
    assert_eq!(p.h.session.stats.backed_bytes_now, 0, "created is not backed");
    // Paint buffer 2 and show it: its own extent carries the client's pixels.
    // SAFETY: a live mapping of the whole pool (`client_pool`), written only here.
    let pixels = unsafe { std::slice::from_raw_parts_mut(ptr, 512 << 20) };
    for (k, px) in pixels[2 * one..3 * one].chunks_mut(4).enumerate() {
        px.copy_from_slice(&(k as u32).to_ne_bytes());
    }
    p.attach_and_commit(surface, buffers[2], w, hgt);
    let peer = wc_handle::Handle::from_descriptor(p.h.pool_descriptor());
    let map = peer.map().unwrap();
    assert_eq!(map.as_slice()[..one], pixels[2 * one..3 * one], "buffer 2's pixels, at 0 of its extent");
    assert_eq!(p.h.session.stats.backed_bytes_now, one as u64);
    // Destroy one, grow the pool (forwarded, no change upstream), cut another.
    p.h.send_words(buffers[0], 0, &[]);
    p.h.send_words(pool, 2, &[(512u32 << 20) + 4096]);
    let b = p.h.id();
    p.h.send_words(pool, 0, &[b, (5 * one) as u32, w as u32, hgt as u32, (w * 4) as u32, 1]);
    p.attach_and_commit(surface, b, w, hgt);
    assert_in_step(&p.h);

    // Ten 3 MiB buffers through a 32 MiB arena, each shown once and released.
    let mut cycle = Vec::new();
    for i in 0..10usize {
        let b = p.h.id();
        p.h.send_words(pool, 0, &[b, ((10 + i) * one) as u32, w as u32, hgt as u32, (w * 4) as u32, 1]);
        p.attach_and_commit(surface, b, w, hgt);
        p.deliver_event(b, op::BUFFER_RELEASE, &[]);
        cycle.push(b);
        std::thread::sleep(super::RECLAIM_FLOOR / 4);
    }
    let s = p.h.session.stats;
    assert_eq!(s.no_memory_errors, 0, "everything fitted, reclaiming: {}", s.summary());
    assert!(s.pools_reclaimed > 0, "the arena is 32 MiB and 33 MiB were shown: {}", s.summary());
    assert!(s.backed_bytes_peak <= 32 << 20, "{}", s.summary());
    // A reclaimed buffer shown again is backed again.
    p.attach_and_commit(surface, cycle[0], w, hgt);
    assert_eq!(p.h.session.stats.no_memory_errors, 0);
    assert_in_step(&p.h);
    let _ = cycle;
}

/// A buffer that cannot fit even after reclaiming is a clean
/// `wl_display.error(no_memory)` naming it, and nothing after it crosses: the
/// stream stays in step to the end.
#[test]
fn a_buffer_bigger_than_the_arena_is_no_memory_naming_it() {
    let mut p = Pressure::new("p109-nomem", 9110, 8 << 20);
    let surface = p.surface();
    let (pool, _) = lazy_pool(&mut p.h, p.shm, 64 << 20);
    let b = p.h.id();
    p.h.send_words(pool, 0, &[b, 0, 4096, 1024, 4096 * 4, 1]); // 16 MiB
    let before = p.h.upstream_messages().len();
    p.h.send_words(surface, 1, &[b, 0, 0]);
    p.h.send_words(surface, 6, &[]);
    assert_eq!(p.h.upstream_messages().len(), before, "nothing crosses after the error");
    let (bytes, _) = p.h.session.take_client_messages();
    let error = client_messages(&bytes).into_iter().find(|(o, op, _)| *o == 1 && *op == 0).expect("wl_display.error");
    assert_eq!(u32::from_ne_bytes(error.2[0..4].try_into().unwrap()), b, "the error names the buffer");
    assert_eq!(u32::from_ne_bytes(error.2[4..8].try_into().unwrap()), 2, "no_memory");
    assert!(String::from_utf8_lossy(&error.2).contains("could not publish"));
    assert_eq!(p.h.session.stats.no_memory_errors, 1);
    assert_in_step(&p.h);
}

/// The proxy's own destroys are answered to the proxy: their `delete_id`s are
/// kept, and the client's own is forwarded, in order, however many re-points
/// one id went through.
#[test]
fn the_proxys_own_destroys_are_answered_to_it() {
    let mut p = Pressure::new("p109-acks", 9111, 8 << 20);
    let surface = p.surface();
    let (pool, _) = lazy_pool(&mut p.h, p.shm, 1 << 20);
    let b = p.h.id();
    p.h.send_words(pool, 0, &[b, 0, 64, 64, 256, 1]);
    p.attach_and_commit(surface, b, 64, 64);
    // The attach destroyed the stub buffer and re-pointed the pool twice
    // (stub at create, extent at attach): one buffer destroy, two pool ones.
    let destroys = |o: u32, opc: u16| p.h.upstream_messages().iter().filter(|(x, y, _)| *x == o && *y == opc).count();
    let (pool_destroys, buffer_destroys) = (destroys(pool, op::POOL_DESTROY), destroys(b, op::BUFFER_DESTROY));
    assert_eq!((pool_destroys, buffer_destroys), (1, 1));
    // The client destroys both; the compositor answers each destroy in turn.
    p.h.send_words(b, 0, &[]);
    p.h.send_words(pool, 1, &[]);
    let mut forwarded = Vec::new();
    for id in [b, pool, b, pool] {
        let (bytes, _) = p
            .h
            .session
            .server_message(Header { object: 1, opcode: op::DISPLAY_DELETE_ID, size: 12 }, &id.to_ne_bytes())
            .unwrap();
        if !bytes.is_empty() {
            forwarded.push(id);
        }
    }
    assert_eq!(forwarded, vec![b, pool], "only the answers to the client's own destroys");
    assert_eq!(p.h.session.stats.backed_bytes_now, 0, "the extent went with the client's delete_id");
    assert_in_step(&p.h);
}

/// weston-presentation-shm: pool, buffers, and the pool destroyed at once. The
/// buffers are backed before the pool goes (there would be no pool left to
/// back them through) and keep working.
#[test]
fn buffers_outlive_their_pool() {
    let mut p = Pressure::new("p109-wps", 9112, 8 << 20);
    let surface = p.surface();
    let (pool, _) = lazy_pool(&mut p.h, p.shm, 3 * 250 * 250 * 4);
    let bs: Vec<u32> = (0..3)
        .map(|i| {
            let b = p.h.id();
            p.h.send_words(pool, 0, &[b, i * 250 * 250 * 4, 250, 250, 1000, 1]);
            b
        })
        .collect();
    p.h.send_words(pool, 1, &[]);
    assert_eq!(p.h.session.stats.buffers_backed, 3, "backed while there was a pool");
    for b in &bs {
        p.attach_and_commit(surface, *b, 250, 250);
    }
    assert_eq!(p.h.session.stats.no_memory_errors, 0);
    assert!(p.h.session.stats.copies >= 3);
    assert_in_step(&p.h);
}

/// The four protocols stage 21 added, driven the way a client drives them.
///
/// # What this is a regression test for
///
/// `xdg-toplevel-icon-v1` was complete in the compositor and advertised at
/// every bind for two stages while this crate had never heard of the interface.
/// The result was not a missing icon: `Session::client_message` resolves the
/// opcode against the spec *before* it walks anything, so the client's first
/// request on the manager — any request, with or without an `h` — is
/// `Error::UnknownOpcode` and the proxy ends the connection. A GTK 4.20 client
/// would have met a wall where a client one version older met a working
/// compositor.
///
/// So this drives each manager past that exact point: bind it, make its child
/// object, and send a request on the child. Binding alone would not do — the
/// bind is a `wl_registry` message and succeeds for any interface name at all.
/// That is precisely why nothing noticed.
#[test]
fn every_protocol_stage_21_added_survives_its_first_request() {
    let d = tmpdir("p21");
    let t = SharedFile::new(&d, "Z:\\pools\\").unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 0);

    // The objects a real client would already have. `record` rather than a
    // scripted `get_registry`, because what is under test is the four managers
    // and not the handshake, which every other test in this file covers.
    let registry = h.id();
    h.session.objects.record(registry, "wl_registry");
    let surface = h.id();
    h.session.objects.record(surface, "wl_surface");
    let seat = h.id();
    h.session.objects.record(seat, "wl_seat");
    let toplevel = h.id();
    h.session.objects.record(toplevel, "xdg_toplevel");

    // xdg-toplevel-icon: create_icon, name it, and set it on a toplevel. The
    // icon's buffer argument is an object id and not an `h`, which is why the
    // whole protocol crosses the boundary without touching the fd shim.
    let icons = h.bind(registry, 3, "xdg_toplevel_icon_manager_v1", 1);
    let icon = h.id();
    h.send_words(icons, 1, &[icon]);
    let mut name = b"firefox".to_vec();
    name.push(0);
    let mut body = Vec::new();
    wire::push_bytes(&mut body, &name);
    h.send(icon, 1, &body, &mut VecDeque::new());
    h.send_words(icons, 2, &[toplevel, icon]);

    // idle-inhibit: an inhibitor on a surface, then destroyed.
    let idle = h.bind(registry, 12, "zwp_idle_inhibit_manager_v1", 1);
    let inhibitor = h.id();
    h.send_words(idle, 1, &[inhibitor, surface]);
    h.send_words(inhibitor, 0, &[]);

    // xdg-activation: mint a token, commit it, and activate a surface with the
    // string that comes back. The token is a `string` on the wire — the one
    // argument shape in these four that is neither a word nor an object, and
    // therefore the one that would desynchronise the stream if its length were
    // walked wrongly.
    let activation = h.bind(registry, 13, "xdg_activation_v1", 1);
    let token = h.id();
    h.send_words(activation, 1, &[token]);
    h.send_words(token, 3, &[]);
    let mut tok = b"a-token-from-the-compositor".to_vec();
    tok.push(0);
    let mut body = Vec::new();
    wire::push_bytes(&mut body, &tok);
    body.extend_from_slice(&surface.to_ne_bytes());
    h.send(activation, 2, &body, &mut VecDeque::new());

    // keyboard-shortcuts-inhibit: an inhibitor for one surface on one seat.
    let shortcuts = h.bind(registry, 14, "zwp_keyboard_shortcuts_inhibit_manager_v1", 1);
    let grab = h.id();
    h.send_words(shortcuts, 1, &[grab, surface, seat]);
    h.send_words(grab, 0, &[]);

    // Every message reached the compositor, and the stream it formed re-frames
    // — which is the property §2.4 says a mis-walked argument destroys, and the
    // one an assertion about return values alone would not have.
    let sent = h.upstream_messages();
    // Four binds and ten requests, none of them absorbed: unlike a pool resize,
    // nothing in these four protocols is something the proxy answers itself.
    assert_eq!(sent.len(), 14, "messages forwarded: {}", sent.len());
    assert!(
        h.session.take_notes().is_empty(),
        "a note means the proxy did not recognise an interface it was handed"
    );
    let _ = std::fs::remove_dir_all(&d);
}
// ---------------------------------------------------------------------------
// The clipboard
// ---------------------------------------------------------------------------
//
// These are the tests for the defect §29 exists to remove and for the
// feature that replaces it, and neither needs a compositor: the whole point of
// the design is that the transfer is **terminated in this process**, so both
// ends of every pipe in these tests are real and local, exactly as they are in
// a running proxy.

/// A request's opcode, by name, out of the vendored XML — because the messages
/// below are the client's and a hand-written opcode here would be testing this
/// file's arithmetic against itself.
fn rq(iface: &str, name: &str) -> u16 {
    let s = Spec::builtin().unwrap();
    s.interface(iface)
        .unwrap_or_else(|| panic!("no {iface}"))
        .requests
        .iter()
        .position(|m| m.name == name)
        .unwrap_or_else(|| panic!("{iface} has no {name}")) as u16
}

fn ev(iface: &str, name: &str) -> u16 {
    let s = Spec::builtin().unwrap();
    s.interface(iface)
        .unwrap()
        .events
        .iter()
        .position(|m| m.name == name)
        .unwrap_or_else(|| panic!("{iface} has no {name} event")) as u16
}

fn nul_bytes(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

fn string_body(s: &str) -> Vec<u8> {
    let mut b = Vec::new();
    wire::push_bytes(&mut b, &nul_bytes(s));
    b
}

/// A session with a `wl_data_device_manager` and a `wl_data_device` bound, which
/// is what every GTK client does before it has a seat at all (docs/waycompose-design.md
/// §15.4).
fn clipboard_harness() -> (Harness, u32, u32) {
    let d = tmpdir("clip");
    let t = SharedFile::new(&d, "Z:\\").unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
    let registry = h.id();
    h.send_words(1, 1, &[registry]);
    let seat = h.bind(registry, 3, "wl_seat", 7);
    let manager = h.bind(registry, 4, "wl_data_device_manager", 3);
    let device = h.id();
    h.send_words(manager, rq("wl_data_device_manager", "get_data_device"), &[device, seat]);
    (h, manager, device)
}

/// Everything the proxy sent its own client on its own account, re-walked from
/// the concatenated stream so that the framing is asserted rather than assumed.
fn client_messages(bytes: &[u8]) -> Vec<(u32, u16, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(hdr) = Header::parse(&bytes[at..]) {
        assert!(hdr.size >= 8 && at + hdr.size <= bytes.len(), "stream is not framed");
        out.push((hdr.object, hdr.opcode, bytes[at + 8..at + hdr.size].to_vec()));
        at += hdr.size;
    }
    assert_eq!(at, bytes.len(), "trailing bytes toward the client");
    out
}

/// **The defect, gone.**
///
/// Before the clipboard work this exact message returned `Error::UnsupportedFd`, which
/// `proxy.rs` turns into a failed drain, which closes the connection. A client
/// that pasted did not fail to paste — it died. The assertion is therefore on
/// `is_ok` first and on the feature second.
#[test]
fn a_paste_is_answered_here_instead_of_ending_the_connection() {
    let (mut h, _manager, device) = clipboard_harness();

    // The compositor announces a selection: data_offer, offer, selection. All
    // three are forwarded verbatim, which is why they are driven through
    // `server_message` rather than invented.
    let offer = 0xff00_0001u32;
    let (to_client, fds) = h
        .session
        .server_message(
            Header { object: device, opcode: ev("wl_data_device", "data_offer"), size: 12 },
            &offer.to_ne_bytes(),
        )
        .expect("a data_offer is an ordinary event");
    assert!(fds.is_empty(), "an announcement carries no descriptor");
    assert!(!to_client.is_empty());
    assert_eq!(h.session.objects.interface(offer), Some("wl_data_offer"));

    // The client pastes. Its pipe is real, because the whole design is that
    // this fd stops here.
    let (read, write) = pipe().unwrap();
    let mut fds = VecDeque::from([write]);
    let body = string_body("text/plain;charset=utf-8");
    let hdr = Header {
        object: offer,
        opcode: rq("wl_data_offer", "receive"),
        size: 8 + body.len(),
    };
    let out = h.session.client_message(hdr, &body, &mut fds);
    assert!(out.is_ok(), "a paste must not be an error: {:?}", out.err());
    assert!(out.unwrap().is_empty(), "receive must not reach the compositor");
    assert!(fds.is_empty(), "the client's descriptor must be consumed, not left to desync");

    // One fetch is pending, for the clipboard rather than for a drag.
    let fetches = h.session.take_fetches();
    assert_eq!(fetches.len(), 1);
    assert_eq!(fetches[0].1, Source::Clipboard);
    assert_eq!(fetches[0].2, "text/plain;charset=utf-8");

    // The compositor answers, and the bytes appear in the client's pipe.
    h.session.deliver(fetches[0].0, Answer::Bytes(b"from Notepad".to_vec()));
    h.session.pump_transfers();
    assert!(!h.session.clip_busy(), "the transfer is finished and the pipe closed");

    use std::io::Read as _;
    let mut f = std::fs::File::from(read);
    let mut got = String::new();
    f.read_to_string(&mut got).unwrap();
    assert_eq!(got, "from Notepad", "and EOF arrived, or read_to_string would hang");
    assert_eq!(h.session.stats.pastes_served, 1);
    assert_eq!(h.session.stats.clip_bytes_in, 12);
}

/// A fetch the compositor refuses closes the pipe rather than leaving it open.
///
/// The difference matters more than it looks: a client blocked in `read` on a
/// descriptor nobody will ever write to or close is hung, and hung is the one
/// outcome worse than the connection error this stage removed.
#[test]
fn a_refused_paste_closes_the_pipe_rather_than_hanging_the_client() {
    let (mut h, _m, device) = clipboard_harness();
    let offer = 0xff00_0002u32;
    h.session
        .server_message(
            Header { object: device, opcode: ev("wl_data_device", "data_offer"), size: 12 },
            &offer.to_ne_bytes(),
        )
        .unwrap();

    let (read, write) = pipe().unwrap();
    let body = string_body("image/png");
    h.session
        .client_message(
            Header { object: offer, opcode: rq("wl_data_offer", "receive"), size: 8 + body.len() },
            &body,
            &mut VecDeque::from([write]),
        )
        .unwrap();
    let token = h.session.take_fetches()[0].0;
    h.session.deliver(token, Answer::Failed("the clipboard holds no such format".into()));
    h.session.pump_transfers();

    use std::io::Read as _;
    let mut f = std::fs::File::from(read);
    let mut got = Vec::new();
    f.read_to_end(&mut got).unwrap();
    assert!(got.is_empty(), "a refusal is an empty paste");
}

/// The other direction: the client copies, and the proxy reads the bytes out of
/// it through a pipe it made itself.
#[test]
fn a_copy_pulls_the_clients_bytes_out_through_a_pipe_this_side_made() {
    let (mut h, manager, device) = clipboard_harness();

    let source = h.id();
    h.send_words(manager, rq("wl_data_device_manager", "create_data_source"), &[source]);
    assert_eq!(
        h.session.objects.interface(source),
        Some("wl_data_source"),
        "the id is tracked here even though the compositor never hears of it"
    );
    // The five names GTK offers one string under, plus HTML.
    for mime in [
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "STRING",
        "TEXT",
        "text/html",
    ] {
        let body = string_body(mime);
        h.send(source, rq("wl_data_source", "offer"), &body, &mut VecDeque::new());
    }
    h.send_words(device, rq("wl_data_device", "set_selection"), &[source, 1]);

    // **`set_selection` did not reach the compositor, and nothing else in the
    // family was held back.** That pair is the whole design: the object graph
    // upstream stays identical to the client's, so no id sequence is broken,
    // and the compositor still never holds a source — which is what makes
    // `wl_data_source.send`, the event that cannot cross, unreachable.
    let upstream = h.upstream_messages();
    let spec = Spec::builtin().unwrap();
    let mut forwarded = Vec::new();
    for (object, opcode, _) in &upstream {
        if let Some(iface) = h.session.objects.interface(*object)
            && iface.starts_with("wl_data_")
        {
            let name = spec.request(iface, *opcode).map(|m| m.name.clone()).unwrap_or_default();
            assert_ne!(name, "set_selection", "set_selection reached the compositor");
            assert_ne!(name, "receive", "receive reached the compositor");
            forwarded.push(name);
        }
    }
    assert_eq!(forwarded.iter().filter(|n| *n == "create_data_source").count(), 1);
    assert_eq!(
        forwarded.iter().filter(|n| *n == "offer").count(),
        6,
        "every offer is forwarded, so the compositor's object graph matches the client's"
    );

    // Two pipes went to the client: one text flavour and one HTML, which is
    // `WANTED_TEXT` doing its job — five names for one string is one pipe.
    let (bytes, fds) = h.session.take_client_messages();
    assert_eq!(fds.len(), 2, "one pipe per mime type actually wanted");
    let msgs = client_messages(&bytes);
    let send = ev("wl_data_source", "send");
    let sent: Vec<String> = msgs
        .iter()
        .filter(|(o, op, _)| *o == source && *op == send)
        .map(|(_, _, b)| {
            let len = u32::from_ne_bytes(b[0..4].try_into().unwrap()) as usize;
            String::from_utf8_lossy(&b[4..4 + len - 1]).into_owned()
        })
        .collect();
    assert_eq!(sent, ["text/plain;charset=utf-8", "text/html"]);

    // The client writes and closes, exactly as libwayland's own clipboard code
    // does. The order of the fds is the order of the messages, which is what
    // libwayland's positional matching requires.
    {
        use std::io::Write as _;
        let mut it = fds.into_iter();
        let mut text = std::fs::File::from(it.next().unwrap());
        text.write_all(b"hello from Firefox").unwrap();
        let mut html = std::fs::File::from(it.next().unwrap());
        html.write_all(b"<b>hello from Firefox</b>").unwrap();
        // Both dropped here: the client closing its end is the EOF the proxy
        // waits for.
    }
    for _ in 0..8 {
        h.session.pump_transfers();
        if !h.session.clip_busy() {
            break;
        }
    }
    let pulled = h.session.take_selection().expect("the pull completed");
    assert!(!pulled.for_drag);
    assert_eq!(
        pulled.payloads,
        vec![
            ("text/plain;charset=utf-8".to_owned(), b"hello from Firefox".to_vec()),
            ("text/html".to_owned(), b"<b>hello from Firefox</b>".to_vec()),
        ]
    );
    assert_eq!(h.session.stats.selections_copied, 1);
    assert_eq!(h.session.stats.clip_bytes_out, 18 + 25);

    // And the client is told it no longer owns the selection, which is what
    // lets a clipboard tool exit instead of waiting to serve a `send` that
    // will never come.
    let (after, _) = h.session.take_client_messages();
    let cancelled = ev("wl_data_source", "cancelled");
    assert!(
        client_messages(&after).iter().any(|(o, op, _)| *o == source && *op == cancelled),
        "the source must be cancelled once the system holds the bytes"
    );
}

/// A selection this clipboard cannot carry leaves the system clipboard alone.
///
/// Publishing an empty set instead would be *destructive*: a client that copied
/// a JPEG (§93: a PNG is carried now) would silently wipe whatever the user
/// had copied in a Windows application a moment earlier.
#[test]
fn a_selection_of_formats_we_cannot_carry_does_not_wipe_the_clipboard() {
    let (mut h, manager, device) = clipboard_harness();
    let source = h.id();
    h.send_words(manager, rq("wl_data_device_manager", "create_data_source"), &[source]);
    let body = string_body("image/jpeg");
    h.send(source, rq("wl_data_source", "offer"), &body, &mut VecDeque::new());
    h.send_words(device, rq("wl_data_device", "set_selection"), &[source, 1]);

    assert!(h.session.take_selection().is_none(), "nothing to publish");
    assert!(!h.session.clip_busy(), "and no pipe was opened");
    let (bytes, fds) = h.session.take_client_messages();
    assert!(fds.is_empty());
    assert!(client_messages(&bytes).is_empty());

    // A null source, by contrast, is an explicit clear and must publish one.
    h.send_words(device, rq("wl_data_device", "set_selection"), &[0, 2]);
    let pulled = h.session.take_selection().expect("clearing publishes an empty selection");
    assert!(pulled.payloads.is_empty());
}

/// **A swallowed `new_id` breaks the client's id sequence**, so none is
/// swallowed.
///
/// This is a regression test for a design that was tried and measured wrong.
/// `wayland-backend`'s server-side object map accepts a new id only when it is
/// the next free one (`rs/map.rs`), so a proxy that keeps a `create_data_source`
/// to itself makes the client's *next* request fail — `wl-copy` died with
/// `wl_display#1: error 0: Invalid new_id: 10.` on the message after the one
/// that was held back. The assertion is therefore on the shape of the upstream
/// stream: every id the client allocated is allocated upstream too, in order,
/// with no gaps.
#[test]
fn every_id_the_client_allocates_reaches_the_compositor_in_sequence() {
    let (mut h, manager, _device) = clipboard_harness();
    let spec = Spec::builtin().unwrap();
    let mut expected = Vec::new();
    for _ in 0..3 {
        let source = h.id();
        expected.push(source);
        h.send_words(manager, rq("wl_data_device_manager", "create_data_source"), &[source]);
        let body = string_body("text/plain");
        h.send(source, rq("wl_data_source", "offer"), &body, &mut VecDeque::new());
        h.send_words(source, rq("wl_data_source", "destroy"), &[]);
    }
    let create = rq("wl_data_device_manager", "create_data_source");
    let got: Vec<u32> = h
        .upstream_messages()
        .into_iter()
        .filter(|(o, op, _)| *o == manager && *op == create)
        .map(|(_, _, b)| u32::from_ne_bytes(b[0..4].try_into().unwrap()))
        .collect();
    assert_eq!(got, expected, "an id the client allocated did not reach the compositor");

    // And the destructor is forwarded too, so `wl_display.delete_id` comes from
    // the compositor as it does for every other object rather than being
    // manufactured here and racing it.
    let destroy = rq("wl_data_source", "destroy");
    assert_eq!(
        h.upstream_messages()
            .into_iter()
            .filter(|(o, op, _)| expected.contains(o) && *op == destroy)
            .count(),
        3
    );
    // Nothing was sent to the client on its own account by any of that.
    let (bytes, fds) = h.session.take_client_messages();
    assert!(bytes.is_empty() && fds.is_empty());
    assert!(spec.request("wl_data_source", destroy).unwrap().destructor);
}

/// A drag is the same pull with a different destination, and it **always
/// ends**.
///
/// The second half is the point. A `wl_data_source` that starts a drag and is
/// never told what happened leaves the client waiting forever, and a drag that
/// does not end is worse than one that does not start.
#[test]
fn a_drag_uses_the_same_pull_and_is_always_answered() {
    let (mut h, manager, device) = clipboard_harness();
    let source = h.id();
    h.send_words(manager, rq("wl_data_device_manager", "create_data_source"), &[source]);
    let body = string_body("text/uri-list");
    h.send(source, rq("wl_data_source", "offer"), &body, &mut VecDeque::new());
    let body = string_body("text/plain");
    h.send(source, rq("wl_data_source", "offer"), &body, &mut VecDeque::new());
    // start_drag(source, origin, icon, serial)
    h.send_words(device, rq("wl_data_device", "start_drag"), &[source, 0, 0, 1]);

    let (_, fds) = h.session.take_client_messages();
    assert_eq!(fds.len(), 1, "one pipe, for the one text flavour offered");
    drop(fds);
    for _ in 0..8 {
        h.session.pump_transfers();
        if !h.session.clip_busy() {
            break;
        }
    }
    let pulled = h.session.take_selection().expect("the pull completed");
    assert!(pulled.for_drag, "and it is marked as a drag rather than a copy");

    // The platform says the drag ended with nothing. The source is told.
    h.session.drag_finished(0);
    let (bytes, _) = h.session.take_client_messages();
    let cancelled = ev("wl_data_source", "cancelled");
    assert!(
        client_messages(&bytes).iter().any(|(o, op, _)| *o == source && *op == cancelled),
        "a drag that dropped nowhere must cancel its source"
    );

    // And a drop is the other answer, in the order the protocol requires.
    let source2 = h.id();
    h.send_words(manager, rq("wl_data_device_manager", "create_data_source"), &[source2]);
    let body = string_body("text/plain");
    h.send(source2, rq("wl_data_source", "offer"), &body, &mut VecDeque::new());
    h.send_words(device, rq("wl_data_device", "start_drag"), &[source2, 0, 0, 2]);
    let _ = h.session.take_client_messages();
    h.session.drag_finished(1); // copy
    let (bytes, _) = h.session.take_client_messages();
    let names: Vec<u16> = client_messages(&bytes)
        .into_iter()
        .filter(|(o, _, _)| *o == source2)
        .map(|(_, op, _)| op)
        .collect();
    assert_eq!(
        names,
        vec![
            ev("wl_data_source", "action"),
            ev("wl_data_source", "dnd_drop_performed"),
            ev("wl_data_source", "dnd_finished"),
        ]
    );
}

/// A client that holds its pipe open forever is published anyway, once.
#[test]
fn a_client_that_never_closes_its_pipe_is_not_a_selection_lost_forever() {
    let (mut h, manager, device) = clipboard_harness();
    let source = h.id();
    h.send_words(manager, rq("wl_data_device_manager", "create_data_source"), &[source]);
    let body = string_body("text/plain");
    h.send(source, rq("wl_data_source", "offer"), &body, &mut VecDeque::new());
    h.send_words(device, rq("wl_data_device", "set_selection"), &[source, 1]);

    let (_, fds) = h.session.take_client_messages();
    let mut it = fds.into_iter();
    let held = std::fs::File::from(it.next().unwrap());
    {
        use std::io::Write as _;
        let mut w = &held;
        w.write_all(b"half a sentence").unwrap();
    }
    // Held open on purpose. Without the deadline this would never publish.
    h.session.pump_transfers();
    assert!(h.session.take_selection().is_none(), "not yet: nothing has expired");
    std::thread::sleep(COPY_DEADLINE + std::time::Duration::from_millis(50));
    h.session.pump_transfers();
    let pulled = h.session.take_selection().expect("the deadline published what arrived");
    assert_eq!(pulled.payloads[0].1, b"half a sentence");
    assert_eq!(h.session.stats.copy_timeouts, 1);
    drop(held);
}

/// §93. The deadline is for a client that went quiet. One that is still
/// writing when two seconds have passed — a long page's HTML — is waited for,
/// and published whole, not cut off in the middle.
#[test]
fn a_copy_still_being_written_past_the_deadline_is_published_whole() {
    let (mut h, manager, device) = clipboard_harness();
    let source = h.id();
    h.send_words(manager, rq("wl_data_device_manager", "create_data_source"), &[source]);
    let body = string_body("text/plain");
    h.send(source, rq("wl_data_source", "offer"), &body, &mut VecDeque::new());
    h.send_words(device, rq("wl_data_device", "set_selection"), &[source, 1]);

    let (_, fds) = h.session.take_client_messages();
    let held = std::fs::File::from(fds.into_iter().next().unwrap());
    let step = COPY_DEADLINE / 4;
    let started = std::time::Instant::now();
    let mut written = Vec::new();
    while started.elapsed() < COPY_DEADLINE + step * 2 {
        use std::io::Write as _;
        let part = format!("part {} ", written.len());
        (&held).write_all(part.as_bytes()).unwrap();
        written.push(part);
        h.session.pump_transfers();
        assert!(
            h.session.take_selection().is_none(),
            "published while the client was still writing, after {:?}",
            started.elapsed()
        );
        std::thread::sleep(step);
    }
    drop(held);
    h.session.pump_transfers();
    let pulled = h.session.take_selection().expect("the EOF published it");
    assert_eq!(pulled.payloads[0].1, written.concat().into_bytes());
    assert_eq!(h.session.stats.copy_timeouts, 0);
}

/// §93. What does not fit in the compositor's cap is left out, the rich
/// flavour before the text, and a text that cannot fit on its own goes too —
/// rather than the whole publish being refused with an error that ended the
/// clipboard feed.
#[test]
fn a_selection_too_big_for_the_compositor_keeps_its_text() {
    let text = ("text/plain;charset=utf-8".to_owned(), vec![b'a'; 60]);
    let html = ("text/html".to_owned(), vec![b'<'; 80]);

    let (kept, dropped) = fit_selection(vec![text.clone(), html.clone()], 200);
    assert_eq!(kept.len(), 2, "within the cap nothing is touched");
    assert!(dropped.is_empty());

    let (kept, dropped) = fit_selection(vec![text.clone(), html.clone()], 100);
    assert_eq!(kept, vec![text.clone()], "the HTML goes first");
    assert_eq!(dropped, vec!["text/html (80 bytes)".to_owned()]);

    let (kept, dropped) = fit_selection(vec![text.clone(), html], 50);
    assert!(kept.is_empty(), "a text larger than the cap cannot be sent at all");
    assert_eq!(dropped.len(), 2);
}

/// §93. A client that writes more than the compositor can hold is not read
/// past the cap — the buffer does not grow without end — and nothing is
/// published: the system clipboard is left as it was.
#[test]
fn a_copy_larger_than_the_cap_stops_being_read_and_publishes_nothing() {
    let (mut h, manager, device) = clipboard_harness();
    let source = h.id();
    h.send_words(manager, rq("wl_data_device_manager", "create_data_source"), &[source]);
    let body = string_body("text/plain");
    h.send(source, rq("wl_data_source", "offer"), &body, &mut VecDeque::new());
    h.send_words(device, rq("wl_data_device", "set_selection"), &[source, 1]);
    let (_, fds) = h.session.take_client_messages();
    let pipe = std::fs::File::from(fds.into_iter().next().unwrap());
    let writer = std::thread::spawn(move || {
        use std::io::Write as _;
        let block = vec![b'a'; 1 << 20];
        let mut sent = 0usize;
        // Until the reader stops: an EPIPE once the proxy has let go.
        while (&pipe).write_all(&block).is_ok() {
            sent += block.len();
            if sent > 2 * MAX_SELECTION_BYTES {
                break;
            }
        }
        sent
    });
    let started = std::time::Instant::now();
    while h.session.clip_busy() && started.elapsed() < std::time::Duration::from_secs(20) {
        h.session.pump_transfers();
    }
    assert!(!h.session.clip_busy(), "the pull ended at the cap");
    assert!(h.session.take_selection().is_none(), "nothing that large is published");
    let sent = writer.join().unwrap();
    assert!(sent <= MAX_SELECTION_BYTES + (2 << 20), "the reader stopped near the cap ({sent})");
    assert!(
        h.session.take_notes().iter().any(|n| n.contains("larger than the compositor holds")),
        "and says so"
    );
}

/// §93. The cap is the compositor's, written down on both sides; this pins
/// the two to each other.
#[test]
fn the_selection_cap_is_the_compositors() {
    let theirs = include_str!("../../../wc-compositor/src/clipboard.rs");
    assert!(
        theirs.contains("pub const MAX_SELECTION_BYTES: usize = 32 << 20;"),
        "wc-compositor's MAX_SELECTION_BYTES moved; move session::MAX_SELECTION_BYTES with it"
    );
    assert_eq!(MAX_SELECTION_BYTES, 32 << 20);
}

/// A paste from a **drag** asks Windows for the drag, not for the clipboard.
///
/// The two are separate selections on Windows and the offer's own identity is
/// the only thing that distinguishes them: an offer announced by
/// `wl_data_device.enter` belongs to a drag over one of our surfaces, one
/// announced by `.selection` belongs to the clipboard. Deciding it any other
/// way returns the last thing dragged to somebody who pressed Ctrl-V.
#[test]
fn an_offer_from_a_drag_is_fetched_from_the_drag_and_not_the_clipboard() {
    let (mut h, _m, device) = clipboard_harness();
    let dragged = 0xff00_0010u32;
    let copied = 0xff00_0011u32;

    // The compositor announces a drag: data_offer, then enter(serial, surface,
    // x, y, id).
    for id in [dragged, copied] {
        h.session
            .server_message(
                Header { object: device, opcode: ev("wl_data_device", "data_offer"), size: 12 },
                &id.to_ne_bytes(),
            )
            .unwrap();
    }
    let mut body = Vec::new();
    for w in [1u32, 0, 0, 0] {
        body.extend_from_slice(&w.to_ne_bytes());
    }
    body.extend_from_slice(&dragged.to_ne_bytes());
    h.session
        .server_message(
            Header { object: device, opcode: ev("wl_data_device", "enter"), size: 8 + body.len() },
            &body,
        )
        .unwrap();

    let receive = rq("wl_data_offer", "receive");
    let mime = string_body("text/uri-list");
    for offer in [dragged, copied] {
        let (_r, w) = pipe().unwrap();
        h.session
            .client_message(
                Header { object: offer, opcode: receive, size: 8 + mime.len() },
                &mime,
                &mut VecDeque::from([w]),
            )
            .unwrap();
    }
    let fetches = h.session.take_fetches();
    assert_eq!(fetches.len(), 2);
    assert_eq!(fetches[0].1, Source::Drag, "the offer that came with an enter");
    assert_eq!(fetches[1].1, Source::Clipboard, "the one that did not");

    // A `leave` ends it: the same offer id would now be the clipboard's, which
    // is wrong in a way nothing else would notice, so it is asserted.
    h.session
        .server_message(
            Header { object: device, opcode: ev("wl_data_device", "leave"), size: 8 },
            &[],
        )
        .unwrap();
    let (_r, w) = pipe().unwrap();
    h.session
        .client_message(
            Header { object: dragged, opcode: receive, size: 8 + mime.len() },
            &mime,
            &mut VecDeque::from([w]),
        )
        .unwrap();
    assert_eq!(h.session.take_fetches()[0].1, Source::Clipboard);
}

/// A `leave` **after a `drop`** does not turn the drag's offer into the
/// clipboard's, because the client has not read from it yet.
///
/// ADDED FOR docs/waycompose-design.md §50, and it is the regression test for the defect that made every
/// drop into a Wayland client deliver nothing. The test above asserts that
/// `leave` forgets a drag offer, which is right for a drag that left the
/// surface — and the code implementing it forgot *every* offer, including one
/// the user had just dropped. The ordering that exposes it cannot be reached by
/// reasoning about the protocol in the abstract; it took a real gesture, and
/// the proxy's own log from that run reads:
///
/// ```text
/// wl_data_device.enter: offer 4278190080 is a drag's
/// wl_data_device.drop
/// wl_data_device.leave: forgetting drag offers {4278190080}
/// receive on offer 4278190080 for "text/plain;charset=utf-8" -> Clipboard
/// paste 1 refused: the clipboard holds nothing of that type
/// ```
///
/// `wl_data_offer.receive` is what a client calls *after* the drop — that is
/// the whole shape of a Wayland drop, and `wl_data_offer.finish` exists because
/// the offer outlives the gesture. So the rule is that `leave` may forget only
/// the offers that were not dropped.
#[test]
fn a_drop_keeps_the_offer_the_drags_even_after_the_leave() {
    let (mut h, _m, device) = clipboard_harness();
    let dragged = 0xff00_0020u32;

    h.session
        .server_message(
            Header { object: device, opcode: ev("wl_data_device", "data_offer"), size: 12 },
            &dragged.to_ne_bytes(),
        )
        .unwrap();
    let mut body = Vec::new();
    for w in [1u32, 0, 0, 0] {
        body.extend_from_slice(&w.to_ne_bytes());
    }
    body.extend_from_slice(&dragged.to_ne_bytes());
    h.session
        .server_message(
            Header { object: device, opcode: ev("wl_data_device", "enter"), size: 8 + body.len() },
            &body,
        )
        .unwrap();

    // The user let go, and *then* the compositor said leave. Both orders happen;
    // this is the one that was wrong.
    h.session
        .server_message(
            Header { object: device, opcode: ev("wl_data_device", "drop"), size: 8 },
            &[],
        )
        .unwrap();
    h.session
        .server_message(
            Header { object: device, opcode: ev("wl_data_device", "leave"), size: 8 },
            &[],
        )
        .unwrap();

    let receive = rq("wl_data_offer", "receive");
    let mime = string_body("text/plain;charset=utf-8");
    let (_r, w) = pipe().unwrap();
    h.session
        .client_message(
            Header { object: dragged, opcode: receive, size: 8 + mime.len() },
            &mime,
            &mut VecDeque::from([w]),
        )
        .unwrap();
    assert_eq!(
        h.session.take_fetches()[0].1,
        Source::Drag,
        "a receive after the drop must still ask Windows for the drag"
    );
}


/// §96 (docs/waycompose-design.md 96.11). GLFW sends `set_app_id("")` when the
/// application names no app id, which Minecraft never does. With a default
/// from the client's `WC_APP_ID`, the compositor gets that id instead, in a
/// well-framed message; a client that names itself is never overruled, and
/// with no default the empty id goes through as it came.
#[test]
fn an_empty_app_id_takes_the_launchers_and_a_named_one_is_left_alone() {
    fn toplevel(h: &mut Harness) -> u32 {
        let registry = h.id();
        h.send_words(1, 1, &[registry]);
        let wm = h.bind(registry, 4, "xdg_wm_base", 6);
        let surface_id = h.id();
        let compositor = h.bind(registry, 1, "wl_compositor", 6);
        h.send_words(compositor, 0, &[surface_id]);
        let xdg = h.id();
        h.send_words(wm, 2, &[xdg, surface_id]); // get_xdg_surface
        let top = h.id();
        h.send_words(xdg, 1, &[top]); // get_toplevel
        top
    }
    fn app_id(h: &mut Harness, top: u32, id: &str) -> Vec<u8> {
        let mut body = Vec::new();
        wire::push_string(&mut body, id);
        h.send(top, 3, &body, &mut VecDeque::new()); // set_app_id
        let msgs = h.upstream_messages();
        let (object, opcode, forwarded) = msgs.last().unwrap().clone();
        assert_eq!((object, opcode), (top, 3));
        forwarded
    }
    let spec = Spec::builtin().unwrap();
    assert_eq!(spec.request("xdg_toplevel", 3).unwrap().name, "set_app_id");
    let d = tmpdir("appid");
    let encoded = |s: &str| {
        let mut b = Vec::new();
        wire::push_string(&mut b, s);
        b
    };

    let mut h = Harness::new(Box::new(SharedFile::new(&d, "Z:\\").unwrap()), CopyScope::Damage, 1);
    h.session.set_default_app_id(Some("wc-minecraft".into()));
    let top = toplevel(&mut h);
    // A client that never calls `set_app_id` (GLFW on libdecor): the default
    // follows `get_toplevel` at once.
    let msgs = h.upstream_messages();
    let (object, opcode, body) = msgs.last().unwrap().clone();
    assert_eq!((object, opcode, body), (top, 3, encoded("wc-minecraft")));
    assert_eq!(h.session.take_app_ids(), vec!["wc-minecraft".to_string()]);
    assert_eq!(app_id(&mut h, top, ""), encoded("wc-minecraft"));
    assert_eq!(h.session.take_app_ids(), vec!["wc-minecraft".to_string()]);
    assert_eq!(app_id(&mut h, top, "org.example.Named"), encoded("org.example.Named"));

    let mut h = Harness::new(Box::new(SharedFile::new(&d, "Z:\\").unwrap()), CopyScope::Damage, 1);
    let top = toplevel(&mut h);
    let msgs = h.upstream_messages();
    assert_eq!(msgs.last().unwrap().1, 1, "with no default, get_toplevel is the last thing sent");
    assert_ne!(msgs.last().unwrap().0, top, "and nothing was sent on the toplevel");
    assert_eq!(app_id(&mut h, top, ""), encoded(""));
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------
// §102: the globals a nested compositor needs from its host
// ---------------------------------------------------------------------------

/// A session with a registry, a compositor and one surface, for the §102 tests.
fn nested_harness(tag: &str) -> (Harness, std::path::PathBuf, u32, u32) {
    let d = tmpdir(tag);
    let t = SharedFile::new(&d, "Z:\\pools\\").unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
    let registry = h.id();
    h.send_words(1, 1, &[registry]);
    let compositor = h.bind(registry, 1, "wl_compositor", 6);
    let surface = h.id();
    h.send_words(compositor, 0, &[surface]);
    (h, d, registry, surface)
}

/// §102. KWin's background: a `wl_buffer` from the single-pixel manager, which
/// belongs to no pool. Its attach must not be held back as "not upstream" (the
/// stage-16 rule for a pool's buffer the compositor does not have), nothing is
/// copied for it, its `release` and `destroy` pass, and every request reaches
/// the compositor byte for byte.
#[test]
fn a_single_pixel_buffer_passes_through_untouched() {
    let (mut h, d, registry, surface) = nested_harness("p102-spb");
    let spb = h.bind(registry, 20, "wp_single_pixel_buffer_manager_v1", 1);
    let buffer = h.id();
    let create = rq("wp_single_pixel_buffer_manager_v1", "create_u32_rgba_buffer");
    h.send_words(spb, create, &[buffer, 0, 0, 0, u32::MAX]);
    let before = h.upstream_messages().len();

    let attach = rq("wl_surface", "attach");
    let damage = rq("wl_surface", "damage_buffer");
    let commit = rq("wl_surface", "commit");
    for _ in 0..3 {
        h.send_words(surface, attach, &[buffer, 0, 0]);
        h.send_words(surface, damage, &[0, 0, 1, 1]);
        h.send_words(surface, commit, &[]);
    }
    // The compositor releases it, and the client destroys it.
    let (released, fds) = h
        .session
        .server_message(Header { object: buffer, opcode: ev("wl_buffer", "release"), size: 8 }, &[])
        .unwrap();
    assert!(fds.is_empty());
    assert_eq!(released, wire::message(buffer, ev("wl_buffer", "release"), &[]));
    h.send_words(buffer, rq("wl_buffer", "destroy"), &[]);

    let sent = h.upstream_messages();
    assert_eq!(sent.len() - before, 3 * 3 + 1, "every request was forwarded");
    let attaches: Vec<_> = sent.iter().filter(|(o, op, _)| *o == surface && *op == attach).collect();
    assert_eq!(attaches.len(), 3);
    assert_eq!(attaches[0].2, [buffer, 0, 0].iter().flat_map(|w| w.to_ne_bytes()).collect::<Vec<_>>());
    let (o, op, b) = &sent[before - 1];
    assert_eq!((*o, *op), (spb, create));
    assert_eq!(&b[16..20], &u32::MAX.to_ne_bytes(), "the colour is the client's");
    assert_eq!(sent.last().map(|m| (m.0, m.1)), Some((buffer, rq("wl_buffer", "destroy"))));
    assert_eq!(h.session.stats.copies, 0, "a buffer with no pool has nothing to copy");
    assert_eq!(h.session.stats.foreign_commits, 3);
    assert_eq!(h.session.stats.attaches_deferred, 0);
    assert!(h.session.take_client_bytes().is_empty(), "nothing invented for the client");
    assert!(h.session.surfaces[&surface].damage.is_empty(), "damage does not pile up");
    assert!(!h.session.foreign_buffers.contains(&buffer), "forgotten once destroyed");
    let _ = std::fs::remove_dir_all(&d);
}

/// §102. A pool's buffer that reuses a single-pixel buffer's id is a pool's
/// buffer again: the copy path is not skipped for it.
#[test]
fn an_id_reused_by_a_pool_buffer_is_no_longer_foreign() {
    let (mut h, d, registry, _) = nested_harness("p102-reuse");
    let spb = h.bind(registry, 20, "wp_single_pixel_buffer_manager_v1", 1);
    let buffer = h.id();
    h.send_words(spb, rq("wp_single_pixel_buffer_manager_v1", "create_u32_rgba_buffer"), &[buffer, 1, 2, 3, 4]);
    assert!(h.session.foreign_buffers.contains(&buffer));
    h.send_words(buffer, rq("wl_buffer", "destroy"), &[]);
    h.session.forget_stale_buffer(buffer, "wl_region");
    assert!(!h.session.foreign_buffers.contains(&buffer));
    let _ = std::fs::remove_dir_all(&d);
}

/// §102. The presentation clock does not cross machines: `presented`'s
/// timestamp leaves the proxy as this machine's `CLOCK_MONOTONIC` at the
/// moment it was forwarded, `clock_id` names that clock, and refresh, sequence
/// and flags are the compositor's.
#[test]
fn a_presented_timestamp_is_rewritten_to_this_machines_clock() {
    let (mut h, d, registry, surface) = nested_harness("p102-pres");
    let presentation = h.bind(registry, 21, "wp_presentation", 2);

    let (clock, _) = h
        .session
        .server_message(
            Header { object: presentation, opcode: ev("wp_presentation", "clock_id"), size: 12 },
            &4u32.to_ne_bytes(),
        )
        .unwrap();
    assert_eq!(&clock[8..12], &(libc::CLOCK_MONOTONIC as u32).to_ne_bytes());

    let feedback = h.id();
    h.send_words(presentation, rq("wp_presentation", "feedback"), &[surface, feedback]);
    // The guest's reading: some other clock entirely.
    let words = [0xdead_u32, 0xbeef, 123, 16_666_666, 0, 42, 0];
    let body: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
    let (lo, _) = monotonic_now();
    let (out, _) = h
        .session
        .server_message(
            Header {
                object: feedback,
                opcode: ev("wp_presentation_feedback", "presented"),
                size: 8 + body.len(),
            },
            &body,
        )
        .unwrap();
    let (hi, _) = monotonic_now();
    let word = |i: usize| u32::from_ne_bytes(out[8 + 4 * i..12 + 4 * i].try_into().unwrap());
    let secs = (u64::from(word(0)) << 32) | u64::from(word(1));
    assert!(lo <= secs && secs <= hi, "{secs} is not between {lo} and {hi}");
    assert!(word(2) < 1_000_000_000);
    assert_eq!((word(3), word(4), word(5), word(6)), (16_666_666, 0, 42, 0));
    assert_eq!(out.len(), 8 + body.len());

    // `discarded` is the compositor's word and passes unchanged.
    let (gone, _) = h
        .session
        .server_message(
            Header { object: feedback, opcode: ev("wp_presentation_feedback", "discarded"), size: 8 },
            &[],
        )
        .unwrap();
    assert_eq!(gone, wire::message(feedback, ev("wp_presentation_feedback", "discarded"), &[]));
    let _ = std::fs::remove_dir_all(&d);
}

/// §102. `wp_viewporter`'s requests walk — two of them are `fixed`, which is a
/// word on the wire — and reach the compositor byte for byte.
#[test]
fn viewport_requests_walk_and_are_forwarded() {
    let (mut h, d, registry, surface) = nested_harness("p102-vp");
    let viewporter = h.bind(registry, 22, "wp_viewporter", 1);
    let viewport = h.id();
    h.send_words(viewporter, rq("wp_viewporter", "get_viewport"), &[viewport, surface]);
    let fixed = |v: f64| (v * 256.0) as i32 as u32;
    h.send_words(viewport, rq("wp_viewport", "set_source"), &[fixed(0.0), fixed(0.0), fixed(1280.0), fixed(800.0)]);
    h.send_words(viewport, rq("wp_viewport", "set_destination"), &[1280, 800]);
    h.send_words(surface, rq("wl_surface", "commit"), &[]);
    h.send_words(viewport, rq("wp_viewport", "destroy"), &[]);
    h.send_words(viewporter, rq("wp_viewporter", "destroy"), &[]);

    let sent = h.upstream_messages();
    let (o, op, b) = &sent[sent.len() - 4];
    assert_eq!((*o, *op), (viewport, rq("wp_viewport", "set_destination")));
    assert_eq!(b, &[1280u32, 800].iter().flat_map(|w| w.to_ne_bytes()).collect::<Vec<_>>());
    assert_eq!(sent.last().map(|m| (m.0, m.1)), Some((viewporter, rq("wp_viewporter", "destroy"))));
    assert!(h.session.take_notes().is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

/// §102 (docs/waycompose-design.md 102.4). A client another user runs for a
/// launcher of ours (the nested desktop's KWin) carries the launcher's title:
/// it is set right after `get_toplevel`, next to the app id, and every title
/// the client sets itself is replaced.
#[test]
fn a_forced_title_is_set_at_once_and_replaces_the_clients() {
    fn toplevel(h: &mut Harness) -> u32 {
        let registry = h.id();
        h.send_words(1, 1, &[registry]);
        let wm = h.bind(registry, 4, "xdg_wm_base", 6);
        let surface_id = h.id();
        let compositor = h.bind(registry, 1, "wl_compositor", 6);
        h.send_words(compositor, 0, &[surface_id]);
        let xdg = h.id();
        h.send_words(wm, 2, &[xdg, surface_id]); // get_xdg_surface
        let top = h.id();
        h.send_words(xdg, 1, &[top]); // get_toplevel
        top
    }
    let spec = Spec::builtin().unwrap();
    assert_eq!(spec.request("xdg_toplevel", 2).unwrap().name, "set_title");
    let encoded = |s: &str| {
        let mut b = Vec::new();
        wire::push_string(&mut b, s);
        b
    };
    let d = tmpdir("p102-title");
    let mut h = Harness::new(Box::new(SharedFile::new(&d, "Z:\\").unwrap()), CopyScope::Damage, 1);
    h.session.set_default_app_id(Some("atp-nested".into()));
    h.session.set_forced_title(Some("Windows 7 (Linux)".into()));
    let top = toplevel(&mut h);
    let msgs = h.upstream_messages();
    let n = msgs.len();
    assert_eq!(msgs[n - 2], (top, 2, encoded("Windows 7 (Linux)")), "the title follows get_toplevel");
    assert_eq!(msgs[n - 1], (top, 3, encoded("atp-nested")), "and the app id after it");
    h.send(top, 2, &encoded("KDE Wayland Compositor WL-0"), &mut VecDeque::new());
    let msgs = h.upstream_messages();
    assert_eq!(msgs.last().unwrap().clone(), (top, 2, encoded("Windows 7 (Linux)")));

    // Without one, the client's own title goes through.
    let mut h = Harness::new(Box::new(SharedFile::new(&d, "Z:\\").unwrap()), CopyScope::Damage, 1);
    let top = toplevel(&mut h);
    h.send(top, 2, &encoded("Smithay"), &mut VecDeque::new());
    assert_eq!(h.upstream_messages().last().unwrap().clone(), (top, 2, encoded("Smithay")));
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------
// §104: `zwp_linux_dmabuf_v1`, answered here (`session/dmabuf.rs`)
// ---------------------------------------------------------------------------

mod dmabuf_tests {
    use super::*;
    use crate::session::dmabuf::{ops, GLOBAL_NAME, MOD_LINEAR};
    use std::os::fd::FromRawFd as _;

    /// A character device every machine has, standing in for the render node.
    const NODE: &str = "/dev/null";
    const SHM_NAME: u32 = 7;
    const W: u32 = 64;
    const H: u32 = 32;

    struct Dm {
        h: Harness,
        d: std::path::PathBuf,
        registry: u32,
        dmabuf: u32,
        surface: u32,
        compositor: u32,
    }

    fn global_body(name: u32, iface: &str, version: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&name.to_ne_bytes());
        wire::push_string(&mut b, iface);
        b.extend_from_slice(&version.to_ne_bytes());
        b
    }

    fn event(h: &mut Harness, object: u32, opcode: u16, body: &[u8]) -> (Vec<u8>, Vec<OwnedFd>) {
        h.session
            .server_message(Header { object, opcode, size: 8 + body.len() }, body)
            .unwrap_or_else(|e| panic!("event {object}.{opcode}: {e}"))
    }

    fn messages(bytes: &[u8]) -> Vec<(u32, u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while let Some(hd) = Header::parse(&bytes[at..]) {
            out.push((hd.object, hd.opcode, bytes[at + 8..at + hd.size].to_vec()));
            at += hd.size;
        }
        assert_eq!(at, bytes.len());
        out
    }

    fn words(b: &[u8]) -> Vec<u32> {
        b.chunks(4).map(|c| u32::from_ne_bytes(c.try_into().unwrap())).collect()
    }

    /// A client that has its registry, the compositor's `wl_shm` announced
    /// (and the private global after it), the dma-buf global bound and a
    /// surface.
    fn setup(tag: &str, on: bool) -> (Dm, Vec<u8>) {
        let d = tmpdir(tag);
        let t = SharedFile::new(&d, "Z:\\pools\\").unwrap();
        let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
        if on {
            h.session.set_dmabuf_node(Some(NODE.into()));
        }
        let registry = h.id();
        h.send_words(1, 1, &[registry]);
        let (announced, fds) = event(&mut h, registry, 0, &global_body(SHM_NAME, "wl_shm", 1));
        assert!(fds.is_empty());
        let compositor = h.bind(registry, 1, "wl_compositor", 6);
        let surface = h.id();
        h.send_words(compositor, 0, &[surface]);
        let dmabuf = if on { h.bind(registry, GLOBAL_NAME, "zwp_linux_dmabuf_v1", 4) } else { 0 };
        (Dm { h, d, registry, dmabuf, surface, compositor }, announced)
    }

    impl Drop for Dm {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.d);
        }
    }

    fn token(name: &str, size: u64) -> OwnedFd {
        let t = sv_drm::token::Token { size, nonce: [7; 16], name: name.into() };
        sv_drm::token::create(&t).unwrap()
    }

    /// The same bytes in a memfd nobody sealed.
    fn unsealed_token(name: &str, size: u64) -> OwnedFd {
        let t = sv_drm::token::Token { size, nonce: [7; 16], name: name.into() };
        let bytes = t.encode();
        // SAFETY: a literal name and valid flags.
        let raw = unsafe { libc::memfd_create(c"unsealed".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw >= 0);
        // SAFETY: just returned.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: a live fd and a slice.
        let n = unsafe { libc::pwrite(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) };
        assert_eq!(n as usize, bytes.len());
        fd
    }

    impl Dm {
        fn params(&mut self) -> u32 {
            let p = self.h.id();
            self.h.send_words(self.dmabuf, ops::DMABUF_CREATE_PARAMS, &[p]);
            p
        }

        fn add(&mut self, params: u32, fd: OwnedFd, offset: u32, stride: u32, modifier: u64) {
            let body: Vec<u8> = [0, offset, stride, (modifier >> 32) as u32, modifier as u32]
                .iter()
                .flat_map(|w| w.to_ne_bytes())
                .collect();
            let mut fds = VecDeque::from([fd]);
            self.h.send(params, ops::PARAMS_ADD, &body, &mut fds);
            assert!(fds.is_empty(), "add takes its descriptor");
        }

        /// `create_immed`; the new buffer's id.
        fn create_immed(&mut self, params: u32, w: u32, h: u32, format: u32) -> u32 {
            let b = self.h.id();
            self.h.send_words(params, ops::PARAMS_CREATE_IMMED, &[b, w, h, format, 0]);
            b
        }

        fn upstream_since(&self, n: usize) -> Vec<(u32, u16, Vec<u8>)> {
            self.h.upstream_messages()[n..].to_vec()
        }
    }

    /// Off, the registry is the compositor's, byte for byte.
    #[test]
    fn without_the_flag_nothing_is_announced() {
        let (dm, announced) = setup("p104-off", false);
        assert_eq!(announced, wire::message(dm.registry, 0, &global_body(SHM_NAME, "wl_shm", 1)));
    }

    #[test]
    fn the_global_follows_wl_shm_and_its_bind_becomes_a_wl_shm_upstream() {
        let (dm, announced) = setup("p104-bind", true);
        let msgs = messages(&announced);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], (dm.registry, 0, global_body(SHM_NAME, "wl_shm", 1)));
        assert_eq!(msgs[1], (dm.registry, 0, global_body(GLOBAL_NAME, "zwp_linux_dmabuf_v1", 4)));
        // The bind upstream names the compositor's wl_shm, at the same id.
        let (o, op, b) = dm.h.upstream_messages().last().unwrap().clone();
        assert_eq!((o, op), (dm.registry, 0));
        let mut want = Vec::new();
        want.extend_from_slice(&SHM_NAME.to_ne_bytes());
        wire::push_string(&mut want, "wl_shm");
        want.extend_from_slice(&1u32.to_ne_bytes());
        want.extend_from_slice(&dm.dmabuf.to_ne_bytes());
        assert_eq!(b, want);
    }

    #[test]
    fn the_default_feedback_names_the_node_and_the_linear_formats() {
        let (mut dm, _) = setup("p104-fb", true);
        let _ = dm.h.session.take_client_messages();
        let n = dm.h.upstream_messages().len();
        let fb = dm.h.id();
        dm.h.send_words(dm.dmabuf, ops::DMABUF_GET_DEFAULT_FEEDBACK, &[fb]);
        // Upstream: only the placeholder that fills the id.
        assert_eq!(dm.upstream_since(n), vec![(1, ops::DISPLAY_SYNC, fb.to_ne_bytes().to_vec())]);

        let (bytes, fds) = dm.h.session.take_client_messages();
        assert_eq!(fds.len(), 1, "the format table");
        let msgs = messages(&bytes);
        let order: Vec<u16> = msgs.iter().map(|m| m.1).collect();
        assert_eq!(
            order,
            [
                ev("zwp_linux_dmabuf_feedback_v1", "format_table"),
                ev("zwp_linux_dmabuf_feedback_v1", "main_device"),
                ev("zwp_linux_dmabuf_feedback_v1", "tranche_target_device"),
                ev("zwp_linux_dmabuf_feedback_v1", "tranche_flags"),
                ev("zwp_linux_dmabuf_feedback_v1", "tranche_formats"),
                ev("zwp_linux_dmabuf_feedback_v1", "tranche_done"),
                ev("zwp_linux_dmabuf_feedback_v1", "done"),
            ]
        );
        assert!(msgs.iter().all(|m| m.0 == fb));
        assert_eq!(words(&msgs[0].2), [32], "two 16-byte entries");
        let mut dev = 8u32.to_ne_bytes().to_vec();
        dev.extend_from_slice(&crate::session::render_node(NODE.as_ref()).unwrap().to_ne_bytes());
        assert_eq!(msgs[1].2, dev, "main_device: st_rdev, 8 bytes, native endian");
        assert_eq!(msgs[2].2, dev);
        assert_eq!(words(&msgs[3].2), [0]);
        let mut idx = 4u32.to_ne_bytes().to_vec();
        idx.extend_from_slice(&0u16.to_ne_bytes());
        idx.extend_from_slice(&1u16.to_ne_bytes());
        assert_eq!(msgs[4].2, idx);

        let mut table = [0u8; 32];
        // SAFETY: a live fd and a buffer of that size.
        let n = unsafe { libc::pread(fds[0].as_raw_fd(), table.as_mut_ptr().cast(), 32, 0) };
        assert_eq!(n, 32);
        let entry = |i: usize| {
            let e = &table[i * 16..i * 16 + 16];
            (
                u32::from_ne_bytes(e[..4].try_into().unwrap()),
                u64::from_ne_bytes(e[8..].try_into().unwrap()),
            )
        };
        assert_eq!(entry(0), (u32::from_le_bytes(*b"AR24"), MOD_LINEAR));
        assert_eq!(entry(1), (u32::from_le_bytes(*b"XR24"), MOD_LINEAR));
        // SAFETY: a live fd.
        let seals = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GET_SEALS) };
        assert_ne!(seals & libc::F_SEAL_WRITE, 0);

        // Destroying it is acknowledged here; the placeholder's answers from
        // the compositor never reach the client.
        dm.h.send_words(fb, ops::FEEDBACK_DESTROY, &[]);
        let (bytes, _) = dm.h.session.take_client_messages();
        assert_eq!(bytes, wire::message(1, 1, &fb.to_ne_bytes()));
        assert!(event(&mut dm.h, fb, 0, &[0, 0, 0, 0]).0.is_empty(), "the sync's done");
        assert!(event(&mut dm.h, 1, 1, &fb.to_ne_bytes()).0.is_empty(), "the sync's delete_id");
    }

    #[test]
    fn a_token_becomes_a_section_pool_and_commits_copy_nothing() {
        let (mut dm, _) = setup("p104-zero", true);
        let name = "Local\\sv-shared-42-1-0123456789abcdef";
        let size = u64::from(W * H * 4);
        let p = dm.params();
        let n = dm.h.upstream_messages().len();
        dm.add(p, token(name, size), 0, W * 4, MOD_LINEAR);
        assert_eq!(dm.upstream_since(n), vec![], "add stays here");
        let b = dm.create_immed(p, W, H, crate::session::DRM_FORMAT_XRGB8888);

        let up = dm.upstream_since(n);
        assert_eq!(up.len(), 3, "{up:?}");
        // wl_shm.create_pool(pool = the params id, Section{name}, size) on the
        // dma-buf object's id, which upstream is a wl_shm.
        assert_eq!((up[0].0, up[0].1), (dm.dmabuf, 0));
        let spec = Spec::builtin().unwrap();
        let spans = wire::walk(&up[0].2, &spec.request("wl_shm", 0).unwrap().args, FdEncoding::InBand)
            .unwrap();
        assert_eq!(wire::word_at(&up[0].2, &spans[0]), Some(p));
        assert_eq!(
            Descriptor::decode(wire::bytes_at(&up[0].2, &spans[1]).unwrap()).unwrap(),
            Descriptor::Section { name: name.into(), size }
        );
        assert_eq!(wire::word_at(&up[0].2, &spans[2]), Some(size as u32));
        // create_buffer(buffer, offset, w, h, stride, wl_shm xrgb8888 = 1)
        assert_eq!((up[1].0, up[1].1), (p, 0));
        assert_eq!(words(&up[1].2), [b, 0, W, H, W * 4, 1]);
        assert_eq!((up[2].0, up[2].1, up[2].2.len()), (p, 1, 0), "the pool is destroyed at once");
        assert!(dm.h.session.take_client_messages().0.is_empty(), "no failed event");

        // attach + damage + commit go through, and nothing is copied.
        let n = dm.h.upstream_messages().len();
        dm.h.send_words(dm.surface, 1, &[b, 0, 0]);
        dm.h.send_words(dm.surface, 9, &[0, 0, W, H]);
        dm.h.send_words(dm.surface, 6, &[]);
        dm.h.send_words(dm.surface, 6, &[]);
        let up = dm.upstream_since(n);
        assert_eq!(up.iter().map(|m| (m.0, m.1)).collect::<Vec<_>>(),
                   [(dm.surface, 1), (dm.surface, 9), (dm.surface, 6), (dm.surface, 6)]);
        let s = dm.h.session.stats;
        assert_eq!((s.copies, s.bytes_copied), (0, 0));
        assert_eq!((s.dmabuf_buffers, s.dmabuf_commits, s.dmabuf_failed), (1, 2, 0));
        assert!(s.summary().contains("dmabuf_buffers=1 dmabuf_commits=2"), "{}", s.summary());

        // The release is the compositor's and passes unchanged.
        let (rel, _) = event(&mut dm.h, b, 0, &[]);
        assert_eq!(rel, wire::message(b, 0, &[]));

        // The compositor's answers to what was done on the client's behalf
        // are dropped: the params placeholder's done and delete_id, the
        // pool's delete_id, and wl_shm.format on the dma-buf object.
        assert!(event(&mut dm.h, p, 0, &[0, 0, 0, 0]).0.is_empty());
        assert!(event(&mut dm.h, 1, 1, &p.to_ne_bytes()).0.is_empty());
        assert!(event(&mut dm.h, 1, 1, &p.to_ne_bytes()).0.is_empty());
        assert!(event(&mut dm.h, dm.dmabuf, 0, &1u32.to_ne_bytes()).0.is_empty());
        // ...and nothing more than that: a third delete_id is the client's.
        dm.h.send_words(p, ops::PARAMS_DESTROY, &[]);
        assert_eq!(dm.h.session.take_client_messages().0, wire::message(1, 1, &p.to_ne_bytes()));

        // The buffer's destroy is forwarded, and so is its delete_id.
        let n = dm.h.upstream_messages().len();
        dm.h.send_words(b, 0, &[]);
        assert_eq!(dm.upstream_since(n), vec![(b, 0, vec![])]);
        assert_eq!(event(&mut dm.h, 1, 1, &b.to_ne_bytes()).0, wire::message(1, 1, &b.to_ne_bytes()));
    }

    #[test]
    fn a_path_name_is_a_shared_file() {
        let (mut dm, _) = setup("p104-path", true);
        let p = dm.params();
        dm.add(p, token("/dev/shm/sv-shared-1-2-0123456789abcdef", 1 << 20), 4096, 1024, MOD_LINEAR);
        let n = dm.h.upstream_messages().len();
        let _ = dm.create_immed(p, 256, 100, crate::session::DRM_FORMAT_ARGB8888);
        let up = dm.upstream_since(n);
        let spec = Spec::builtin().unwrap();
        let spans = wire::walk(&up[0].2, &spec.request("wl_shm", 0).unwrap().args, FdEncoding::InBand)
            .unwrap();
        assert_eq!(
            Descriptor::decode(wire::bytes_at(&up[0].2, &spans[1]).unwrap()).unwrap(),
            Descriptor::SharedFile { path: "/dev/shm/sv-shared-1-2-0123456789abcdef".into(), size: 1 << 20 }
        );
        assert_eq!(words(&up[1].2)[1..], [4096, 256, 100, 1024, 0], "argb8888 is wl_shm 0");
    }

    /// Each failure is a `failed` event, a placeholder for the buffer id, no
    /// pool, and a buffer whose destroy is answered here.
    #[allow(clippy::too_many_arguments)]
    fn fails(tag: &str, fd: OwnedFd, offset: u32, stride: u32, modifier: u64, w: u32, h: u32, format: u32) {
        let (mut dm, _) = setup(tag, true);
        let p = dm.params();
        dm.add(p, fd, offset, stride, modifier);
        let _ = dm.h.session.take_client_messages();
        let n = dm.h.upstream_messages().len();
        let b = dm.create_immed(p, w, h, format);
        assert_eq!(dm.upstream_since(n), vec![(1, ops::DISPLAY_SYNC, b.to_ne_bytes().to_vec())]);
        let (bytes, _) = dm.h.session.take_client_messages();
        assert_eq!(bytes, wire::message(p, ev("zwp_linux_buffer_params_v1", "failed"), &[]));
        assert_eq!(dm.h.session.stats.dmabuf_failed, 1);
        assert_eq!(dm.h.session.stats.dmabuf_buffers, 0);
        assert!(dm.h.session.take_notes().iter().any(|l| l.contains("create_immed failed")));
        // An attach of it is held back, never forwarded.
        let n = dm.h.upstream_messages().len();
        dm.h.send_words(dm.surface, 1, &[b, 0, 0]);
        assert!(dm.upstream_since(n).is_empty());
        // Its destroy is absorbed and acknowledged here.
        dm.h.send_words(b, 0, &[]);
        assert!(dm.upstream_since(n).is_empty());
        assert_eq!(dm.h.session.take_client_messages().0, wire::message(1, 1, &b.to_ne_bytes()));
    }

    #[test]
    fn an_unsealed_fd_fails() {
        fails("p104-unsealed", unsealed_token("Local\\x", 1 << 20), 0, W * 4, 0, W, H, crate::session::DRM_FORMAT_XRGB8888);
    }

    #[test]
    fn a_tiled_modifier_fails() {
        fails("p104-mod", token("Local\\x", 1 << 20), 0, W * 4, 0x0100_0000_0000_0001, W, H, crate::session::DRM_FORMAT_XRGB8888);
    }

    #[test]
    fn a_buffer_past_the_allocation_fails() {
        // One byte short of the last row.
        let size = u64::from(W * 4 * (H - 1) + W * 4 - 1);
        fails("p104-oob", token("Local\\x", size), 0, W * 4, 0, W, H, crate::session::DRM_FORMAT_XRGB8888);
    }

    #[test]
    fn a_stride_narrower_than_a_row_fails() {
        fails("p104-stride", token("Local\\x", 1 << 20), 0, W * 4 - 4, 0, W, H, crate::session::DRM_FORMAT_XRGB8888);
    }

    #[test]
    fn a_format_not_in_the_table_fails() {
        fails("p104-fmt", token("Local\\x", 1 << 20), 0, W * 4, 0, W, H, u32::from_le_bytes(*b"AB24"));
    }

    #[test]
    fn the_last_row_needs_only_its_pixels() {
        // Exactly offset + stride * (h - 1) + w * 4: accepted.
        let (mut dm, _) = setup("p104-exact", true);
        let p = dm.params();
        let stride = W * 4 + 64;
        dm.add(p, token("Local\\x", u64::from(16 + stride * (H - 1) + W * 4)), 16, stride, 0);
        let _ = dm.create_immed(p, W, H, crate::session::DRM_FORMAT_XRGB8888);
        assert_eq!(dm.h.session.stats.dmabuf_buffers, 1);
    }

    /// `create` (the server-allocated id) is not supported and says so.
    #[test]
    fn create_without_immed_fails_with_a_reason() {
        let (mut dm, _) = setup("p104-create", true);
        let p = dm.params();
        dm.add(p, token("Local\\x", 1 << 20), 0, W * 4, 0);
        let _ = dm.h.session.take_client_messages();
        let n = dm.h.upstream_messages().len();
        dm.h.send_words(p, ops::PARAMS_CREATE, &[W, H, crate::session::DRM_FORMAT_XRGB8888, 0]);
        assert!(dm.upstream_since(n).is_empty());
        assert_eq!(dm.h.session.take_client_messages().0, wire::message(p, 1, &[]));
        assert!(dm.h.session.take_notes().iter().any(|l| l.contains("use create_immed")));
    }

    /// The node is looked at when a registry is filled, not at startup: a
    /// client whose registry came while it was missing is not offered the
    /// global (and is told nothing else), one after it appeared is, and
    /// each gets one line saying which.
    #[test]
    fn the_node_is_asked_for_each_registry() {
        let d = tmpdir("p104-late");
        let link = d.join("renderD128");
        std::os::unix::fs::symlink(d.join("absent"), &link).unwrap();
        let t = SharedFile::new(&d, "Z:\\pools\\").unwrap();
        let mut h = Harness::new(Box::new(t), CopyScope::Damage, 1);
        h.session.set_dmabuf_node(Some(link.clone()));
        let r1 = h.id();
        h.send_words(1, 1, &[r1]);
        let (out, _) = event(&mut h, r1, 0, &global_body(SHM_NAME, "wl_shm", 1));
        assert_eq!(out, wire::message(r1, 0, &global_body(SHM_NAME, "wl_shm", 1)));
        let notes = h.session.take_notes();
        assert!(notes.iter().any(|l| l.starts_with("not offered zwp_linux_dmabuf_v1")), "{notes:?}");

        // The node appears (root made it); the next registry is offered it.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(NODE, &link).unwrap();
        let r2 = h.id();
        h.send_words(1, 1, &[r2]);
        let (out, _) = event(&mut h, r2, 0, &global_body(SHM_NAME, "wl_shm", 1));
        assert_eq!(messages(&out).len(), 2, "wl_shm and the dma-buf global");
        let dev = crate::session::render_node(NODE.as_ref()).unwrap();
        let want = format!(
            "offered zwp_linux_dmabuf_v1 v4, main device {}:{}",
            crate::session::dev_major(dev),
            crate::session::dev_minor(dev)
        );
        let notes = h.session.take_notes();
        assert!(notes.iter().any(|l| l.starts_with(&want)), "{notes:?}");
        // A third registry with the same answer says nothing more.
        let r3 = h.id();
        h.send_words(1, 1, &[r3]);
        let _ = event(&mut h, r3, 0, &global_body(SHM_NAME, "wl_shm", 1));
        assert!(h.session.take_notes().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dev_t_splits_like_glibc() {
        let dev = crate::session::render_node(NODE.as_ref()).unwrap();
        assert_eq!((crate::session::dev_major(dev), crate::session::dev_minor(dev)), (1, 3));
        assert_eq!((crate::session::dev_major(0xe280), crate::session::dev_minor(0xe280)), (226, 128));
    }

    /// The opcodes `session/dmabuf.rs` hard-codes, against the XML.
    #[test]
    fn the_dmabuf_opcodes_are_the_xmls() {
        assert_eq!(ops::DMABUF_DESTROY, rq("zwp_linux_dmabuf_v1", "destroy"));
        assert_eq!(ops::DMABUF_CREATE_PARAMS, rq("zwp_linux_dmabuf_v1", "create_params"));
        assert_eq!(ops::DMABUF_GET_DEFAULT_FEEDBACK, rq("zwp_linux_dmabuf_v1", "get_default_feedback"));
        assert_eq!(ops::DMABUF_GET_SURFACE_FEEDBACK, rq("zwp_linux_dmabuf_v1", "get_surface_feedback"));
        assert_eq!(ops::DMABUF_FORMAT, ev("zwp_linux_dmabuf_v1", "format"));
        assert_eq!(ops::DMABUF_MODIFIER, ev("zwp_linux_dmabuf_v1", "modifier"));
        assert_eq!(ops::PARAMS_DESTROY, rq("zwp_linux_buffer_params_v1", "destroy"));
        assert_eq!(ops::PARAMS_ADD, rq("zwp_linux_buffer_params_v1", "add"));
        assert_eq!(ops::PARAMS_CREATE, rq("zwp_linux_buffer_params_v1", "create"));
        assert_eq!(ops::PARAMS_CREATE_IMMED, rq("zwp_linux_buffer_params_v1", "create_immed"));
        assert_eq!(ops::PARAMS_FAILED, ev("zwp_linux_buffer_params_v1", "failed"));
        assert_eq!(ops::FEEDBACK_DESTROY, rq("zwp_linux_dmabuf_feedback_v1", "destroy"));
        let fb = |n| ev("zwp_linux_dmabuf_feedback_v1", n);
        assert_eq!(ops::FB_DONE, fb("done"));
        assert_eq!(ops::FB_FORMAT_TABLE, fb("format_table"));
        assert_eq!(ops::FB_MAIN_DEVICE, fb("main_device"));
        assert_eq!(ops::FB_TRANCHE_DONE, fb("tranche_done"));
        assert_eq!(ops::FB_TRANCHE_TARGET_DEVICE, fb("tranche_target_device"));
        assert_eq!(ops::FB_TRANCHE_FORMATS, fb("tranche_formats"));
        assert_eq!(ops::FB_TRANCHE_FLAGS, fb("tranche_flags"));
        assert_eq!(ops::DISPLAY_SYNC, rq("wl_display", "sync"));
        assert_eq!(ops::REGISTRY_BIND, rq("wl_registry", "bind"));
    }

    /// Every client id reaches the compositor in creation order with no gap,
    /// which is what `wayland-backend` insists on: the dma-buf objects are
    /// filled by a `wl_shm` bind and `wl_display.sync`s.
    #[test]
    fn no_client_id_leaves_a_hole_upstream() {
        let (mut dm, _) = setup("p104-ids", true);
        let fb = dm.h.id();
        dm.h.send_words(dm.dmabuf, ops::DMABUF_GET_SURFACE_FEEDBACK, &[fb, dm.surface]);
        let p = dm.params();
        dm.add(p, token("Local\\x", 1 << 20), 0, W * 4, 0);
        let b = dm.create_immed(p, W, H, crate::session::DRM_FORMAT_XRGB8888);
        let p2 = dm.params();
        dm.add(p2, token("Local\\y", 16), 0, W * 4, 0);
        let b2 = dm.create_immed(p2, W, H, crate::session::DRM_FORMAT_XRGB8888); // fails
        let after = dm.h.id();
        dm.h.send_words(dm.compositor, 0, &[after]);
        // The first message upstream that names each new id.
        let up = dm.h.upstream_messages();
        let first = |id: u32| {
            up.iter().position(|(o, op, body)| {
                let w = words(&body[..body.len() & !3]);
                (*o == 1 && *op == ops::DISPLAY_SYNC && w == [id])
                    || (*op == 0 && w.last() == Some(&id) && *o == dm.registry)
                    || (*op == 0 && *o != dm.registry && w.first() == Some(&id))
            })
        };
        let order: Vec<usize> = [dm.dmabuf, fb, p, b, p2, b2, after]
            .iter()
            .map(|id| first(*id).unwrap_or_else(|| panic!("{id} never reached the compositor")))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    }
}

/// §102.9. KWin's Wayland backend imports each swapchain buffer as its own
/// pool and destroys the pool object at once (`WaylandBackend::importBuffer`,
/// `wayland_backend.cpp`: `wl_shm_create_pool`, `wl_shm_pool_create_buffer`,
/// `wl_shm_pool_destroy`), and it imports lazily, at the first present of each
/// slot. So the second slot's pool is created after the compositor has
/// acknowledged the first pool's destructor, and libwayland hands the second
/// pool **the first pool's id**. The guest's log said so: `pool 29 … slice 0`,
/// then `pool 29 … slice 3608576`.
///
/// Returns, per slot, the client's pixels and the arena slice the compositor
/// was told to read that slot's buffer from.
struct KwinSlots {
    h: Harness,
    dir: std::path::PathBuf,
    surface: u32,
    shm: u32,
    w: i32,
    hgt: i32,
    /// (buffer id, client pixels, the descriptor its `create_pool` carried)
    slots: Vec<(u32, &'static mut [u8], Descriptor)>,
    /// The output as it stands after the last present.
    canvas: Vec<u8>,
}

impl KwinSlots {
    fn new(tag: &str, arena_id: u32, scope: CopyScope) -> KwinSlots {
        let dir = tmpdir(tag);
        let path = dir.join("arena");
        let t = Arena::open_or_create(arena_id, &path, 4 << 20).unwrap();
        wc_handle::arena::attach(arena_id, &path.to_string_lossy(), t.size()).unwrap();
        let mut h = Harness::new(Box::new(t), scope, 1);
        let registry = h.id();
        h.send_words(1, 1, &[registry]);
        let compositor = h.bind(registry, 1, "wl_compositor", 6);
        let shm = h.bind(registry, 2, "wl_shm", 2);
        let surface = h.id();
        h.send_words(compositor, 0, &[surface]);
        KwinSlots { h, dir, surface, shm, w: 160, hgt: 90, slots: Vec::new(), canvas: Vec::new() }
    }

    /// `importBuffer` for one slot, with the pool id the client's libwayland
    /// picked.
    fn import(&mut self, pool: u32) -> usize {
        let bytes = (self.w * 4 * self.hgt) as usize;
        let (fd, ptr) = client_pool(bytes);
        // SAFETY: what `mmap` returned in `client_pool`; leaked for the test's
        // length, and this is the only reference to it.
        let pixels: &'static mut [u8] = unsafe { std::slice::from_raw_parts_mut(ptr, bytes) };
        let before = self.h.upstream.len();
        let mut body = Vec::new();
        body.extend_from_slice(&pool.to_ne_bytes());
        body.extend_from_slice(&(bytes as u32).to_ne_bytes());
        self.h.send(self.shm, op::SHM_CREATE_POOL, &body, &mut VecDeque::from([fd]));
        let buffer = self.h.id();
        let (w, hgt) = (self.w as u32, self.hgt as u32);
        self.h.send_words(pool, op::POOL_CREATE_BUFFER, &[buffer, 0, w, hgt, w * 4, 1]);
        self.h.send_words(pool, op::POOL_DESTROY, &[]);
        // The descriptor this slot's `create_pool` carried, from the bytes
        // that crossed after `before`.
        let spec = Spec::builtin().unwrap();
        let args = &spec.request("wl_shm", 0).unwrap().args;
        let mut at = before;
        let mut found = None;
        while let Some(hd) = Header::parse(&self.h.upstream[at..]) {
            let b = &self.h.upstream[at + 8..at + hd.size];
            if hd.object == self.shm
                && hd.opcode == op::SHM_CREATE_POOL
                && let Ok(spans) = wire::walk(b, args, FdEncoding::InBand)
                && let Some(d) = wire::bytes_at(b, &spans[1]).and_then(|x| Descriptor::decode(x).ok())
            {
                found = Some(d);
            }
            at += hd.size;
        }
        self.slots.push((buffer, pixels, found.expect("the slot's pool was published")));
        self.slots.len() - 1
    }

    /// The compositor's `delete_id`, which is what lets the client recycle.
    fn delete_id(&mut self, id: u32) {
        let body = id.to_ne_bytes().to_vec();
        self.h
            .session
            .server_message(Header { object: 1, opcode: op::DISPLAY_DELETE_ID, size: 12 }, &body)
            .unwrap();
    }

    fn release(&mut self, slot: usize) {
        let buffer = self.slots[slot].0;
        self.h
            .session
            .server_message(Header { object: buffer, opcode: op::BUFFER_RELEASE, size: 8 }, &[])
            .unwrap();
    }

    /// Render frame `tick` into `slot` the way QPainter does — the whole
    /// picture is in the slot, damage is only what changed on the output — and
    /// present it.
    fn present(&mut self, slot: usize, tick: u8, damage: (i32, i32, i32, i32)) {
        let stride = (self.w * 4) as usize;
        if self.canvas.is_empty() {
            self.canvas = vec![0x10; stride * self.hgt as usize];
        }
        let (x0, y0, dw, dh) = damage;
        for y in y0..y0 + dh {
            for x in x0..x0 + dw {
                let at = y as usize * stride + x as usize * 4;
                self.canvas[at..at + 4].copy_from_slice(&[tick, tick.wrapping_mul(3), 0x80, 0xff]);
            }
        }
        // The slot gets the whole output, which is what the protocol asks of
        // an attached buffer and what KWin's repaint of the slot's age does.
        self.slots[slot].1.copy_from_slice(&self.canvas);
        let buffer = self.slots[slot].0;
        let s = self.surface;
        self.h.send_words(s, op::SURFACE_ATTACH, &[buffer, 0, 0]);
        let (x, y, w, hh) = damage;
        self.h.send_words(s, op::SURFACE_DAMAGE_BUFFER, &[x as u32, y as u32, w as u32, hh as u32]);
        self.h.send_words(s, op::SURFACE_COMMIT, &[]);
    }

    /// What the compositor reads for `slot`, against what the client drew.
    fn slice_matches(&self, slot: usize) -> bool {
        let (_, px, d) = &self.slots[slot];
        let peer = wc_handle::Handle::from_descriptor(d.clone());
        let map = peer.map().unwrap();
        map.as_slice()[..px.len()] == px[..]
    }
}

impl Drop for KwinSlots {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// §102.9, **the bug**: with damage-scoped copies the guest's window kept
/// KWin's first frame. The sequence, as KWin's swapchain of two produces it:
///
/// 1. slot A imported: pool P, buffer A, `destroy` P; A presented.
/// 2. the compositor acknowledges P's destructor (`delete_id`); P's id is free
///    in the client, though A still lives on P's memory.
/// 3. A is not yet released, so the next frame takes slot B: imported as a pool
///    **with P's id**, buffer B, `destroy`; B presented; A released.
/// 4. every later frame A: the compositor reads A from P's slice.
///
/// Step 3 used to overwrite the proxy's table entry for P with B's pool, so A's
/// `BufferInfo::pool` named B's pool from then on: A's commits copied B's
/// client memory into B's slice, and A's slice kept frame 1 for ever. Both
/// copy scopes, since both trust that map. Whether the id is recycled depends
/// on when the `delete_id` arrives, which is why a local run usually missed it
/// and, presumably, why the guest's `--copy buffer` run did not show it.
#[test]
fn a_recycled_pool_id_does_not_steal_a_live_buffer() {
    for (n, scope) in [CopyScope::Damage, CopyScope::Buffer].into_iter().enumerate() {
        let mut k = KwinSlots::new(&format!("p102-recycle-{n}"), 7600 + n as u32, scope);
        let pool = k.h.id();
        let full = (0, 0, k.w, k.hgt);
        let a = k.import(pool);
        k.present(a, 1, full);
        k.delete_id(pool);
        let b = k.import(pool);
        k.present(b, 2, full);
        k.release(a);
        k.delete_id(pool);
        for tick in 3..12u8 {
            let slot = if tick % 2 == 1 { a } else { b };
            // A small rectangle that moves, as a cursor or a clock does.
            k.present(slot, tick, (i32::from(tick) * 4, 10, 12, 8));
            k.release(1 - slot);
            assert!(
                k.slice_matches(slot),
                "{scope:?}: frame {tick}: slot {slot}'s arena slice is not what the client drew"
            );
        }
        // And the first slot's memory is given back with its buffer, not lost
        // when its id was taken.
        let (ba, bb) = (k.slots[a].0, k.slots[b].0);
        k.h.send_words(ba, op::BUFFER_DESTROY, &[]);
        k.h.send_words(bb, op::BUFFER_DESTROY, &[]);
        assert_eq!(k.h.session.stats.pools_released, 2, "{scope:?}: a pool's region leaked");
        assert!(k.h.session.pools.is_empty(), "{scope:?}: a pool outlived its buffers");
    }
}
