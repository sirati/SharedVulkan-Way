//! The translation, driven by a scripted client rather than by a compositor.
//!
//! These are the tests that matter most in this crate, because they exercise
//! the exact path a `wl_shm` frame takes and they do it without Wine, without a
//! window and without a Windows binary. Stage 3's evidence had to be split in
//! two halves for want of a client that could reach both (docs/design.md §8.5);
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
    /// dialect, which is the failure mode docs/design.md §2.4 warns about.
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
    fn pool_descriptor(&self) -> Descriptor {
        let spec = Spec::builtin().unwrap();
        let args = &spec.request("wl_shm", 0).unwrap().args;
        for (_, opcode, body) in self.upstream_messages() {
            if opcode != 0 {
                continue;
            }
            if let Ok(spans) = wire::walk(&body, args, FdEncoding::InBand)
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
/// what makes the cost measurement in docs/design.md §9 a choice rather than a
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

/// The measured client's actual behaviour, reproduced: nine resizes before any
/// other request, and **one** region of the final size, with nothing forwarded
/// in between.
///
/// The figures are `weston-terminal`'s own, taken from a `--trace` run against
/// `wc-compositor.exe`. An earlier version of this proxy announced the pool
/// immediately and then failed all nine resizes, which is why they are here as
/// data rather than as a round number.
#[test]
fn a_pool_that_grows_before_first_use_is_published_once_at_its_final_size() {
    let d = tmpdir("defer");
    let t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
    let mut h = Harness::new(Box::new(t), CopyScope::Buffer, 1);
    let (pool, _registry) = deferred_pool(
        &mut h,
        4096,
        &[12288, 28672, 61440, 126976, 258048, 520192, 1044480, 2093056, 4190208],
    );

    // Nothing pool-related has reached the compositor: not the create_pool,
    // not one of the nine resizes. Only the two messages that set the
    // connection up.
    let before = h.upstream_messages();
    assert_eq!(before.len(), 2, "only get_registry and bind should have gone: {before:?}");
    assert_eq!(h.session.stats.pools_created, 0);
    assert_eq!(h.session.stats.pool_resizes, 9);
    assert_eq!(h.session.stats.resize_overflows, 0, "no resize may fail on this path");

    // The first request that is not a resize publishes it, once, at the size
    // the client settled on.
    let buffer = h.id();
    h.send_words(pool, 0, &[buffer, 0, 800, 600, 3200, 1]);
    assert_eq!(h.session.stats.pools_created, 1);
    match h.pool_descriptor() {
        Descriptor::SharedFile { size, .. } => assert_eq!(size, 4190208),
        other => panic!("{other:?}"),
    }
    // create_pool then create_buffer, in that order and nothing between.
    let msgs = h.upstream_messages();
    assert_eq!(msgs.len(), 4, "{msgs:?}");
    assert_eq!(msgs[2].1, 0, "create_pool");
    assert_eq!(msgs[3], (pool, 0, msgs[3].2.clone()), "create_buffer, on the pool");
    let _ = std::fs::remove_dir_all(&d);
}

/// A resize *after* the pool has been published is a different case, and the
/// one `--pool-headroom` exists for. Beyond the headroom it is counted and
/// named rather than silently producing a short buffer.
#[test]
fn a_resize_after_publication_is_bounded_by_the_headroom_and_says_so() {
    for (headroom, expect_overflow) in [(1u64, true), (4, false)] {
        let d = tmpdir(&format!("resize-{headroom}"));
        let t = SharedFile::new(&d, &format!("{}/", d.display())).unwrap();
        let mut h = Harness::new(Box::new(t), CopyScope::Buffer, headroom);
        let initial = 64 * 1024;
        let (pool, _registry) = deferred_pool(&mut h, initial, &[]);

        // Publish it, then grow.
        let buffer = h.id();
        h.send_words(pool, 0, &[buffer, 0, 64, 64, 256, 1]);
        assert_eq!(h.session.stats.pools_created, 1);
        h.session.take_notes();

        h.send_words(pool, 2, &[(initial * 2) as u32]);
        assert_eq!(h.session.stats.pool_resizes, 1);
        assert_eq!(
            h.session.stats.resize_overflows,
            u64::from(expect_overflow),
            "headroom {headroom}"
        );
        if expect_overflow {
            let notes = h.session.take_notes().join("\n");
            assert!(notes.contains("--pool-headroom"), "{notes}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
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
/// presented. This is docs/design.md §2.5 exactly.
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
/// exercised — docs/design.md §9.10 item 5's "implemented, unexercised". A
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

    /// `wl_shm.create_pool` with a real client object behind it, then one
    /// buffer covering the whole thing.
    fn pool_with_buffer(&mut self, bytes: usize, w: i32, hgt: i32) -> (u32, u32) {
        let (fd, _) = client_pool(bytes);
        let pool = self.h.id();
        let mut body = Vec::new();
        body.extend_from_slice(&pool.to_ne_bytes());
        body.extend_from_slice(&(bytes as u32).to_ne_bytes());
        self.h.send(self.shm, 0, &body, &mut VecDeque::from([fd]));
        let buffer = self.h.id();
        self.h.send_words(pool, 0, &[buffer, 0, w as u32, hgt as u32, (w * 4) as u32, 1]);
        (pool, buffer)
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

    /// How many times `buffer` was created out of `pool` upstream.
    ///
    /// A separate method from [`Pressure::mentions`] because `create_buffer` is
    /// addressed to the *pool* and carries the buffer as its first argument,
    /// and because `wl_shm_pool.create_buffer` and `wl_buffer.destroy` are both
    /// opcode 0 — so counting "opcode 0 messages about this buffer" would count
    /// two different things.
    fn created(&self, pool: u32, buffer: u32) -> usize {
        self.h
            .upstream_messages()
            .iter()
            .filter(|(obj, opcode, body)| {
                *obj == pool
                    && *opcode == op::POOL_CREATE_BUFFER
                    && body.len() >= 4
                    && u32::from_ne_bytes(body[..4].try_into().unwrap()) == buffer
            })
            .count()
    }

    /// Every message the compositor received that names `object` — as the
    /// message's own target, or as the buffer argument of a `wl_surface.attach`.
    ///
    /// This is the assertion the whole stage turns on. A message naming an
    /// object the compositor does not have is a protocol error, and a protocol
    /// error is a dead client.
    fn mentions(&self, object: u32) -> Vec<(u32, u16)> {
        self.h
            .upstream_messages()
            .into_iter()
            .filter(|(obj, opcode, body)| {
                *obj == object
                    || (*opcode == op::SURFACE_ATTACH
                        && body.len() >= 4
                        && u32::from_ne_bytes(body[..4].try_into().unwrap()) == object)
            })
            .map(|(obj, opcode, _)| (obj, opcode))
            .collect()
    }
}

impl Drop for Pressure {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// **The bug, and the fix.** docs/design.md §19.2.
///
/// An arena with no room for a pool used to kill the client, and the log line
/// said so in three steps: `could not publish 2340864 bytes`, then
/// `client 2 stopped: the compositor closed the connection`, then Firefox's
/// `Exiting due to channel error`. The middle step is the one that was not
/// understood — the allocation failure left the pool unannounced, and the
/// client's very next `wl_shm_pool.create_buffer` was forwarded to a compositor
/// that had never heard of that object.
///
/// So this asserts the property rather than the symptom: **not one byte
/// naming an unbacked pool, or a buffer cut from it, reaches the compositor.**
/// And then that the surface recovers when there is room, because a fix that
/// froze the surface for ever would be a different bug.
#[test]
fn a_full_arena_degrades_one_surface_and_never_the_connection() {
    // 32 pages. The outbound window is not reserved here, so all of it is the
    // pool allocator's.
    let mut p = Pressure::new("full", 7400, 32 * 4096);
    let surface = p.surface();

    // 24 pages, published, drawn. This is the pool that fills the arena.
    let (_big, big_buf) = p.pool_with_buffer(24 * 4096, 64, 384);
    p.attach_and_commit(surface, big_buf, 64, 384);
    assert_eq!(p.h.session.stats.pools_created, 1);
    assert!(p.h.session.stats.copies > 0, "the first pool must have worked");

    // 16 pages, and only 8 are left. Everything the client does with it from
    // here is legal and none of it may cross.
    let second = p.surface();
    let (pool_b, buf_b) = p.pool_with_buffer(16 * 4096, 64, 256);
    p.attach_and_commit(second, buf_b, 64, 256);
    p.h.send_words(pool_b, 2, &[16 * 4096]); // a resize, for good measure
    p.attach_and_commit(second, buf_b, 64, 256);

    assert!(p.h.session.stats.alloc_refusals > 0, "the arena should have refused");
    assert!(p.h.session.stats.attaches_deferred > 0, "the attach should have been held");
    assert_eq!(
        p.mentions(pool_b),
        Vec::new(),
        "the compositor was told about a pool that has no memory behind it"
    );
    assert_eq!(
        p.mentions(buf_b),
        Vec::new(),
        "the compositor was told about a buffer cut from a pool it does not have -- \
         this is the exact message that killed Firefox"
    );
    // The first surface is untouched by any of it, which is the difference
    // between degrading and dying.
    let copies_before = p.h.session.stats.copies;
    p.attach_and_commit(surface, big_buf, 64, 384);
    assert!(p.h.session.stats.copies > copies_before, "the healthy surface stopped working");

    // --- and now there is room ------------------------------------------
    //
    // The client finishes with the first pool: buffer, then pool, then the
    // compositor's acknowledgement, which is the only point at which the bytes
    // are safe to reuse (docs/design.md §2.5).
    p.h.send_words(big_buf, 0, &[]); // wl_buffer.destroy
    p.h.send_words(surface, 1, &[0, 0, 0]); // attach nil, so nothing holds it
    p.h.send_words(surface, 6, &[]);
    let big = p.h.session.buffers.get(&big_buf).map(|b| b.pool);
    assert!(big.is_none(), "the buffer should be gone from the table");
    let big_pool = p.mentions(0); // silence the unused warning path
    let _ = big_pool;

    p.h.send_words(
        p.h.session
            .pools
            .keys()
            .copied()
            .find(|id| *id != pool_b)
            .expect("the first pool is still here"),
        1, // wl_shm_pool.destroy
        &[],
    );
    let first_pool = *p
        .h
        .session
        .pools
        .keys()
        .find(|id| **id != pool_b)
        .expect("still tracked until acknowledged");
    p.deliver_event(1, op::DISPLAY_DELETE_ID, &[first_pool]);

    // One more message, and the pool that could not fit now fits.
    p.attach_and_commit(second, buf_b, 64, 256);
    assert!(
        !p.mentions(pool_b).is_empty(),
        "the pool never recovered: {:?}",
        p.h.session.take_notes()
    );
    assert!(
        p.mentions(buf_b).iter().any(|(_, opcode)| *opcode == op::SURFACE_ATTACH),
        "the held attach was never replayed, so the surface would stay frozen for ever"
    );
}

/// **Reclamation.** docs/design.md §19.5, and the answer to §17.10 item 1.
///
/// Firefox destroys one buffer in three and no pools at all, so a policy that
/// waits to be asked gets nothing back. This one is not asked: it destroys the
/// upstream objects of a pool nothing is using, keeps the client's own mapping,
/// and rebuilds the objects from the client's own requests when it comes back.
///
/// The re-attach at the end is the case §17.10 warned about — "a client may
/// legitimately re-attach an old buffer" — and it is the reason the rebuild
/// exists rather than a note saying it cannot happen.
#[test]
fn a_cold_pool_is_reclaimed_under_pressure_and_rebuilt_when_the_client_returns() {
    // 40 pages, and three pools that cannot all fit at once. The sizes are
    // chosen so that every step below is forced rather than incidental.
    let mut p = Pressure::new("reclaim", 7401, 40 * 4096);
    let idle = p.surface();
    let held = p.surface();
    let busy = p.surface();

    // 16 pages that stay in use throughout, so that the reclaimer has an
    // obvious candidate it must *not* take.
    //
    // **It is allocated first on purpose.** With the cold pool at the bottom
    // instead, reclaiming it leaves two 12-page holes with the kept pool
    // wedged between them, and the 20-page pool below does not fit into either
    // — which is §19.4's fragmentation, arriving uninvited in §19.5's test and
    // worth leaving this note about rather than only the ordering.
    let (keep_pool, keep_buf) = p.pool_with_buffer(16 * 4096, 64, 256);
    p.attach_and_commit(held, keep_buf, 64, 256);
    assert_eq!(p.h.session.stats.pools_created, 1);

    // 12 pages, used once and then let go of — the shape of every one of the
    // 88 pools §15.10 measured Firefox holding for the life of the process.
    let (cold_pool, cold_buf) = p.pool_with_buffer(12 * 4096, 64, 192);
    p.attach_and_commit(idle, cold_buf, 64, 192);
    assert_eq!(p.h.session.stats.pools_created, 2);

    // Nothing holds it any more: not the surface, and not the compositor. Both
    // are required and both are observable without asking the client, which is
    // the whole reason this policy can work where §15.11's could not.
    p.h.send_words(idle, 1, &[0, 0, 0]); // attach nil
    p.h.send_words(idle, 6, &[]);
    p.deliver_event(cold_buf, op::BUFFER_RELEASE, &[]);

    // 20 pages, and 12 are left. Not yet cold, so the floor refuses.
    let (_hot, hot_buf) = p.pool_with_buffer(20 * 4096, 64, 320);
    p.attach_and_commit(busy, hot_buf, 64, 320);
    assert!(p.h.session.stats.alloc_refusals > 0, "20 pages must not fit in 12");
    assert_eq!(p.h.session.stats.pools_reclaimed, 0, "a warm pool must not be reclaimed");

    // Now it is cold, and the next message that needs room takes it.
    std::thread::sleep(std::time::Duration::from_millis(300));
    p.attach_and_commit(busy, hot_buf, 64, 320);
    assert!(
        p.mentions(cold_pool).iter().any(|(_, opcode)| *opcode == op::POOL_DESTROY),
        "the cold pool was never taken apart"
    );
    assert!(
        p.mentions(cold_buf).iter().any(|(_, opcode)| *opcode == op::BUFFER_DESTROY),
        "its buffer must go first, or the compositor keeps reading bytes we hand away"
    );
    // The one in use was left alone, which is the condition that stops this
    // policy from blanking a window.
    assert_eq!(
        p.mentions(keep_pool).iter().filter(|(_, o)| *o == op::POOL_DESTROY).count(),
        0,
        "a pool whose buffer is attached to a surface was reclaimed"
    );
    // And the bytes are not free until the compositor says so (§2.5).
    assert_eq!(p.h.session.stats.pools_reclaimed, 0, "freed before the acknowledgement");
    p.deliver_event(1, op::DISPLAY_DELETE_ID, &[cold_pool]);
    assert_eq!(p.h.session.stats.pools_reclaimed, 1, "the acknowledgement did not free it");

    // The pool stays gone rather than being rebuilt by the next message that
    // happens along, which is the whole point of taking it apart. The message
    // below publishes the *hot* pool, into the bytes just recovered.
    let republished = p.h.session.stats.pools_republished;
    p.attach_and_commit(busy, hot_buf, 64, 320);
    assert_eq!(p.h.session.stats.pools_created, 3, "the hot pool should have fitted now");
    assert_eq!(
        p.h.session.stats.pools_republished, republished,
        "a reclaimed pool was rebuilt by a message that had nothing to do with it"
    );

    // The client finishes with the pool it was holding, which makes room.
    p.h.send_words(held, 1, &[0, 0, 0]);
    p.h.send_words(held, 6, &[]);
    p.h.send_words(keep_buf, 0, &[]); // wl_buffer.destroy
    p.h.send_words(keep_pool, 1, &[]); // wl_shm_pool.destroy
    p.deliver_event(1, op::DISPLAY_DELETE_ID, &[keep_pool]);

    // And now it comes back for a buffer it never destroyed. §17.10's warning —
    // "a client may legitimately re-attach an old buffer" — and the reason the
    // rebuild exists rather than a note saying it cannot happen.
    p.attach_and_commit(idle, cold_buf, 64, 192);
    let why = p.h.session.take_notes();
    assert_eq!(
        p.h.session.stats.pools_republished,
        republished + 1,
        "the pool was not rebuilt: {why:?}"
    );
    let after: Vec<(u32, u16)> = p.mentions(cold_buf);
    assert_eq!(
        p.created(cold_pool, cold_buf),
        2,
        "the buffer must be created out of the rebuilt pool a second time, or the compositor \
         has a pool with nothing cut from it and the attach below is a protocol error"
    );
    assert!(
        after.last().is_some_and(|(_, opcode)| *opcode == op::SURFACE_ATTACH),
        "the attach must follow the rebuild, not precede it: {after:?}"
    );
    // And the pixels really moved again, which no message count can show.
    assert!(p.h.session.stats.copies >= 4, "{}", p.h.session.stats.summary());
}

/// The two objects a proxy destroys on its own initiative, and the two
/// acknowledgements that must not go the wrong way.
///
/// A `delete_id` for something *this* proxy destroyed is the compositor
/// answering the proxy, and forwarding it would tell libwayland-client to
/// recycle an id its own application still holds. In the other direction, a
/// destructor the proxy absorbed will never be acknowledged by anybody unless
/// the proxy does it, and libwayland does not reuse an id until it is.
#[test]
fn an_acknowledgement_goes_to_whoever_asked_for_the_destruction() {
    let mut p = Pressure::new("ack", 7402, 32 * 4096);
    let surface = p.surface();
    let (cold_pool, cold_buf) = p.pool_with_buffer(24 * 4096, 64, 384);
    p.attach_and_commit(surface, cold_buf, 64, 384);
    p.h.send_words(surface, 1, &[0, 0, 0]);
    p.h.send_words(surface, 6, &[]);
    p.deliver_event(cold_buf, op::BUFFER_RELEASE, &[]);
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Force the teardown.
    let (_hot, hot_buf) = p.pool_with_buffer(16 * 4096, 64, 256);
    p.attach_and_commit(surface, hot_buf, 64, 256);
    assert!(p.h.session.stats.alloc_refusals > 0);
    assert!(p.mentions(cold_pool).iter().any(|(_, o)| *o == op::POOL_DESTROY));

    // The compositor's answer to *our* destructor is swallowed: the client
    // still owns that id and must not be told to recycle it.
    let _ = p.h.session.take_client_bytes();
    p.deliver_event(1, op::DISPLAY_DELETE_ID, &[cold_pool]);
    assert!(
        p.h.session.take_client_bytes().is_empty(),
        "a delete_id for an object the proxy destroyed was forwarded to the client"
    );

    // One message to let the pool that caused the pressure take the recovered
    // bytes, so that the count below measures the destructors and not that.
    p.attach_and_commit(surface, hot_buf, 64, 256);
    assert_eq!(p.h.session.stats.pools_created, 2);

    // Now the client destroys the buffer and the pool it still believes in.
    // Neither may be forwarded, and both are owed an acknowledgement.
    let before = p.h.upstream_messages().len();
    p.h.send_words(cold_buf, 0, &[]);
    p.h.send_words(cold_pool, 1, &[]);
    assert_eq!(
        p.h.upstream_messages().len(),
        before,
        "a destructor for an object the compositor does not have was forwarded"
    );
    let invented = p.h.session.take_client_bytes();
    let mut seen = Vec::new();
    let mut at = 0;
    while let Some(hdr) = Header::parse(&invented[at..]) {
        assert_eq!(hdr.object, 1, "only wl_display may be invented");
        assert_eq!(hdr.opcode, op::DISPLAY_DELETE_ID);
        seen.push(u32::from_ne_bytes(invented[at + 8..at + 12].try_into().unwrap()));
        at += hdr.size;
    }
    assert_eq!(at, invented.len(), "the invented stream is not framed");
    assert_eq!(seen, vec![cold_buf, cold_pool], "both ids must be released to the client");
}
