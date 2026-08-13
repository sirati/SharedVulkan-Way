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
// These are the tests for the defect stage 13 exists to remove and for the
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
/// is what every GTK client does before it has a seat at all (docs/design.md
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
/// Before stage 13 this exact message returned `Error::UnsupportedFd`, which
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

    // Nothing of the family reached the compositor. This is the property that
    // makes the whole design work: with no source upstream, the compositor can
    // never be asked to write into a descriptor.
    let upstream = h.upstream_messages();
    let spec = Spec::builtin().unwrap();
    for (object, opcode, _) in &upstream {
        if let Some(iface) = h.session.objects.interface(*object)
            && iface.starts_with("wl_data_")
        {
            let name = spec.request(iface, *opcode).map(|m| m.name.clone()).unwrap_or_default();
            assert_eq!(
                name, "get_data_device",
                "{iface}.{name} reached the compositor and should not have"
            );
        }
    }

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
/// an image would silently wipe whatever the user had copied in a Windows
/// application a moment earlier.
#[test]
fn a_selection_of_formats_we_cannot_carry_does_not_wipe_the_clipboard() {
    let (mut h, manager, device) = clipboard_harness();
    let source = h.id();
    h.send_words(manager, rq("wl_data_device_manager", "create_data_source"), &[source]);
    let body = string_body("image/png");
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

/// Destroying a source retires its id toward the client, because the compositor
/// never knew the id and so will never send `wl_display.delete_id` for it.
#[test]
fn a_destroyed_source_gets_its_delete_id_from_the_proxy() {
    let (mut h, manager, _device) = clipboard_harness();
    let source = h.id();
    h.send_words(manager, rq("wl_data_device_manager", "create_data_source"), &[source]);
    let _ = h.session.take_client_messages();
    h.send_words(source, rq("wl_data_source", "destroy"), &[]);
    let (bytes, _) = h.session.take_client_messages();
    assert_eq!(
        client_messages(&bytes),
        vec![(1u32, op::DISPLAY_DELETE_ID, source.to_ne_bytes().to_vec())]
    );
    assert_eq!(h.session.objects.interface(source), None);
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
