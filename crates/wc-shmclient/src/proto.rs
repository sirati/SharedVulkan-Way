//! The wire format and the picture, written out by hand.
//!
//! Compiled on both platforms even though only the Windows build can use it,
//! for one reason: **`cargo test` cannot run for the Windows target**
//! (docs/design.md §4.1), so anything that lives only behind `cfg(windows)` is
//! checked by the compiler and by nothing else. The encoding is the part of
//! this client most likely to be quietly wrong — a byte of padding, a size
//! field counted from the wrong place — and it is testable without a socket,
//! so it lives here where the tests can reach it.

/// A rectangle, in surface pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

// --- object ids ----------------------------------------------------------
//
// The client owns the id space below 0xff000000 and allocates from it itself.
// These are fixed rather than counted because every object here has one known
// purpose and a named constant reads better in a protocol trace than a number
// that happens to be 9 today.

pub const WL_DISPLAY: u32 = 1;
pub const ID_REGISTRY: u32 = 2;
pub const ID_SYNC: u32 = 3;
pub const ID_COMPOSITOR: u32 = 4;
pub const ID_SHM: u32 = 5;
pub const ID_WM_BASE: u32 = 6;
pub const ID_SURFACE: u32 = 7;
pub const ID_XDG_SURFACE: u32 = 8;
pub const ID_TOPLEVEL: u32 = 9;
pub const ID_POOL: u32 = 10;
pub const ID_BUFFER0: u32 = 11;
pub const ID_BUFFER1: u32 = 12;
/// Frame callbacks are one-shot and this id is reused every frame. That is
/// legal only because the server retires it with `wl_display.delete_id` after
/// each `done` — so reusing it is itself a small test of the server.
pub const ID_FRAME: u32 = 13;

// --- opcodes -------------------------------------------------------------
//
// Verified against `vendor/wayland-server/wayland.xml` and
// wayland-protocols 1.49 `stable/xdg-shell/xdg-shell.xml`.

pub const REQ_DISPLAY_SYNC: u16 = 0;
pub const REQ_DISPLAY_GET_REGISTRY: u16 = 1;
pub const EVT_DISPLAY_ERROR: u16 = 0;
pub const EVT_DISPLAY_DELETE_ID: u16 = 1;

pub const EVT_REGISTRY_GLOBAL: u16 = 0;
pub const EVT_CALLBACK_DONE: u16 = 0;

pub const REQ_COMPOSITOR_CREATE_SURFACE: u16 = 0;
pub const REQ_SHM_CREATE_POOL: u16 = 0;
pub const REQ_POOL_CREATE_BUFFER: u16 = 0;
pub const EVT_BUFFER_RELEASE: u16 = 0;

pub const REQ_SURFACE_ATTACH: u16 = 1;
pub const REQ_SURFACE_DAMAGE: u16 = 2;
pub const REQ_SURFACE_FRAME: u16 = 3;
pub const REQ_SURFACE_COMMIT: u16 = 6;

pub const REQ_WM_BASE_GET_XDG_SURFACE: u16 = 2;
pub const REQ_WM_BASE_PONG: u16 = 3;
pub const EVT_WM_BASE_PING: u16 = 0;

pub const REQ_XDG_SURFACE_GET_TOPLEVEL: u16 = 1;
pub const REQ_XDG_SURFACE_ACK_CONFIGURE: u16 = 4;
pub const EVT_XDG_SURFACE_CONFIGURE: u16 = 0;

pub const REQ_TOPLEVEL_SET_TITLE: u16 = 2;
pub const REQ_TOPLEVEL_SET_APP_ID: u16 = 3;
pub const EVT_TOPLEVEL_CONFIGURE: u16 = 0;
pub const EVT_TOPLEVEL_CLOSE: u16 = 1;

/// `wl_shm.format.xrgb8888`.
///
/// Not `argb8888`: GDI treats a 32-bit `BI_RGB` DIB as opaque, so asking for
/// alpha the compositor is documented not to honour would make a screenshot
/// ambiguous about whose fault a wrong colour was.
pub const FORMAT_XRGB8888: u32 = 1;

// --- encoding ------------------------------------------------------------

/// A message header: the sender's object id, then the size in the high 16 bits
/// and the opcode in the low 16. Native byte order, because a Unix socket
/// cannot cross an endianness boundary and the protocol takes advantage.
pub fn header(object: u32, opcode: u16, size: usize) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&object.to_ne_bytes());
    out[4..8].copy_from_slice(&(((size as u32) << 16) | u32::from(opcode)).to_ne_bytes());
    out
}

/// One message whose arguments are all 32-bit.
pub fn message(object: u32, opcode: u16, args: &[u32]) -> Vec<u8> {
    let size = 8 + args.len() * 4;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&header(object, opcode, size));
    for a in args {
        out.extend_from_slice(&a.to_ne_bytes());
    }
    out
}

/// A wayland `array`: a 32-bit length, the bytes, then zero padding to a
/// 4-byte boundary.
///
/// **This is also exactly how an `h` argument is encoded on Windows.** The
/// entire deviation of docs/design.md §2.3 is that one sentence, and this is
/// the function that implements it on the client side.
pub fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_ne_bytes());
    out.extend_from_slice(bytes);
    out.extend(std::iter::repeat_n(0u8, (4 - bytes.len() % 4) % 4));
}

/// A wayland `string`: an `array` whose length counts a trailing NUL.
pub fn push_string(out: &mut Vec<u8>, s: &str) {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    push_bytes(out, &b);
}

/// Wrap a hand-built body in a header.
pub fn finish(object: u32, opcode: u16, body: Vec<u8>) -> Vec<u8> {
    let size = 8 + body.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&header(object, opcode, size));
    out.extend_from_slice(&body);
    out
}

/// `wl_registry.bind`, which has the one genuinely awkward encoding in the core
/// protocol: its `new_id` has no interface declared in the XML, so on the wire
/// it expands to three values — the interface name as a string, the version,
/// and only then the id.
pub fn bind(name: u32, interface: &str, version: u32, new_id: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&name.to_ne_bytes());
    push_string(&mut body, interface);
    body.extend_from_slice(&version.to_ne_bytes());
    body.extend_from_slice(&new_id.to_ne_bytes());
    finish(ID_REGISTRY, 0, body)
}

// --- the message sequences -----------------------------------------------
//
// Composing the byte streams here rather than inline in `win.rs` is what makes
// them testable: `cargo test` cannot run for the Windows target, so anything
// behind `cfg(windows)` is checked by the compiler and by nothing else. These
// functions decide *what* to send; `win.rs` decides *when*.

/// `get_registry` and `sync`: the two requests every client opens with.
///
/// The `sync` is what makes it a handshake rather than a hopeful read: Wayland
/// is ordered, so `wl_callback.done` arriving means the whole registry burst
/// that preceded it has arrived too.
pub fn hello() -> Vec<u8> {
    let mut out = message(WL_DISPLAY, REQ_DISPLAY_GET_REGISTRY, &[ID_REGISTRY]);
    out.extend_from_slice(&message(WL_DISPLAY, REQ_DISPLAY_SYNC, &[ID_SYNC]));
    out
}

/// Bind the three globals, then make a surface, an `xdg_surface` and an
/// `xdg_toplevel`, name it, and commit.
///
/// Versions are floored rather than taking whatever is offered: this client
/// uses nothing above `wl_compositor` v4 or `xdg_wm_base` v1, and binding
/// higher would invite events it has no handler for.
///
/// The trailing commit carries **no buffer**. That is not an oversight — it is
/// what asks the compositor for the initial configure, and attaching before
/// the ack is a protocol error.
pub fn create_window(
    compositor: (u32, u32),
    shm: (u32, u32),
    wm_base: (u32, u32),
    title: &str,
    app_id: &str,
) -> Vec<u8> {
    let mut out = bind(
        compositor.0,
        "wl_compositor",
        compositor.1.min(4),
        ID_COMPOSITOR,
    );
    out.extend_from_slice(&bind(shm.0, "wl_shm", shm.1.min(1), ID_SHM));
    out.extend_from_slice(&bind(wm_base.0, "xdg_wm_base", wm_base.1.min(1), ID_WM_BASE));
    out.extend_from_slice(&message(
        ID_COMPOSITOR,
        REQ_COMPOSITOR_CREATE_SURFACE,
        &[ID_SURFACE],
    ));
    out.extend_from_slice(&message(
        ID_WM_BASE,
        REQ_WM_BASE_GET_XDG_SURFACE,
        &[ID_XDG_SURFACE, ID_SURFACE],
    ));
    out.extend_from_slice(&message(
        ID_XDG_SURFACE,
        REQ_XDG_SURFACE_GET_TOPLEVEL,
        &[ID_TOPLEVEL],
    ));
    let mut body = Vec::new();
    push_string(&mut body, title);
    out.extend_from_slice(&finish(ID_TOPLEVEL, REQ_TOPLEVEL_SET_TITLE, body));
    let mut body = Vec::new();
    push_string(&mut body, app_id);
    out.extend_from_slice(&finish(ID_TOPLEVEL, REQ_TOPLEVEL_SET_APP_ID, body));
    out.extend_from_slice(&message(ID_SURFACE, REQ_SURFACE_COMMIT, &[]));
    out
}

/// `wl_shm.create_pool` carrying the descriptor as an `h`, then the two
/// buffers that live in it.
///
/// **This is the request that a stock libwayland client cannot express on
/// Windows** and therefore the reason this whole program exists: the `fd`
/// argument is a length-prefixed byte string in the message body, where
/// libwayland puts nothing at all and sends the fd out of band.
pub fn create_pool(descriptor: &[u8], pool_len: u64, width: i32, height: i32) -> Vec<u8> {
    let stride = width * 4;
    let buffer_len = stride as u64 * height as u64;

    let mut body = Vec::new();
    body.extend_from_slice(&ID_POOL.to_ne_bytes());
    push_bytes(&mut body, descriptor);
    body.extend_from_slice(&(pool_len as i32).to_ne_bytes());
    let mut out = finish(ID_SHM, REQ_SHM_CREATE_POOL, body);

    for (i, id) in [ID_BUFFER0, ID_BUFFER1].into_iter().enumerate() {
        out.extend_from_slice(&message(
            ID_POOL,
            REQ_POOL_CREATE_BUFFER,
            &[
                id,
                (i as u64 * buffer_len) as u32,
                width as u32,
                height as u32,
                stride as u32,
                FORMAT_XRGB8888,
            ],
        ));
    }
    out
}

/// Attach, damage, ask for a frame callback, commit.
///
/// The order is the protocol's: `attach` and `damage` are pending state,
/// `frame` registers a callback against the same commit, and `commit` applies
/// all three atomically.
pub fn present(slot: usize, damage: Rect) -> Vec<u8> {
    let buffer_id = if slot == 0 { ID_BUFFER0 } else { ID_BUFFER1 };
    let mut out = message(ID_SURFACE, REQ_SURFACE_ATTACH, &[buffer_id, 0, 0]);
    out.extend_from_slice(&message(
        ID_SURFACE,
        REQ_SURFACE_DAMAGE,
        &[
            damage.x as u32,
            damage.y as u32,
            damage.w as u32,
            damage.h as u32,
        ],
    ));
    out.extend_from_slice(&message(ID_SURFACE, REQ_SURFACE_FRAME, &[ID_FRAME]));
    out.extend_from_slice(&message(ID_SURFACE, REQ_SURFACE_COMMIT, &[]));
    out
}

pub fn ack_configure(serial: u32) -> Vec<u8> {
    message(ID_XDG_SURFACE, REQ_XDG_SURFACE_ACK_CONFIGURE, &[serial])
}

pub fn pong(serial: u32) -> Vec<u8> {
    message(ID_WM_BASE, REQ_WM_BASE_PONG, &[serial])
}

// --- decoding ------------------------------------------------------------

/// An event, once it has been recognised.
///
/// Only the ones this client acts on. Everything else is `Ignored`, which is
/// deliberate: a client that treated an unknown event as an error would break
/// the moment the compositor gained a global, and a client that could not tell
/// "ignored" from "not received" could not report a missing one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Error {
        object: u32,
        code: u32,
        message: String,
    },
    Global {
        name: u32,
        interface: String,
        version: u32,
    },
    SyncDone,
    FrameDone,
    BufferReleased(usize),
    Ping(u32),
    XdgConfigure(u32),
    ToplevelConfigure(i32, i32),
    ToplevelClose,
    Ignored,
}

/// How a byte stream failed to be a message.
#[derive(Debug, PartialEq, Eq)]
pub struct Desync(pub String);

/// Parse as many whole messages as `buf` holds.
///
/// Returns the events and how many bytes were consumed; a trailing partial
/// message is left for the next read, which is the whole reason this returns a
/// length rather than draining.
#[allow(clippy::type_complexity)]
pub fn parse(buf: &[u8]) -> (Vec<Event>, usize, Option<Desync>) {
    let mut events = Vec::new();
    let mut at = 0;
    while buf.len() - at >= 8 {
        let object = u32_at(buf, at);
        let word2 = u32_at(buf, at + 4);
        let opcode = (word2 & 0xffff) as u16;
        let size = (word2 >> 16) as usize;
        // A size below the header length cannot be honoured and cannot be
        // resynchronised from: it is the shape framing loss takes, and the
        // caller has to stop rather than skip forward and guess.
        if size < 8 {
            return (
                events,
                at,
                Some(Desync(format!(
                    "message on object {object} claims size {size}"
                ))),
            );
        }
        if buf.len() - at < size {
            break;
        }
        events.push(event(object, opcode, &buf[at + 8..at + size]));
        at += size;
    }
    (events, at, None)
}

fn event(object: u32, opcode: u16, body: &[u8]) -> Event {
    match (object, opcode) {
        (WL_DISPLAY, EVT_DISPLAY_ERROR) if body.len() >= 8 => Event::Error {
            object: u32_at(body, 0),
            code: u32_at(body, 4),
            message: string_at(body, 8).map(|(s, _)| s).unwrap_or_default(),
        },
        // The frame callback id is reused every frame, which is legal only
        // because the server retires it with this event.
        (WL_DISPLAY, EVT_DISPLAY_DELETE_ID) => Event::Ignored,
        (ID_REGISTRY, EVT_REGISTRY_GLOBAL) if body.len() >= 8 => {
            match string_at(body, 4) {
                Some((interface, next)) if next + 4 <= body.len() => Event::Global {
                    name: u32_at(body, 0),
                    interface,
                    version: u32_at(body, next),
                },
                _ => Event::Ignored,
            }
        }
        (ID_SYNC, EVT_CALLBACK_DONE) => Event::SyncDone,
        (ID_FRAME, EVT_CALLBACK_DONE) => Event::FrameDone,
        (ID_BUFFER0, EVT_BUFFER_RELEASE) => Event::BufferReleased(0),
        (ID_BUFFER1, EVT_BUFFER_RELEASE) => Event::BufferReleased(1),
        (ID_WM_BASE, EVT_WM_BASE_PING) if body.len() >= 4 => Event::Ping(u32_at(body, 0)),
        (ID_XDG_SURFACE, EVT_XDG_SURFACE_CONFIGURE) if body.len() >= 4 => {
            Event::XdgConfigure(u32_at(body, 0))
        }
        (ID_TOPLEVEL, EVT_TOPLEVEL_CONFIGURE) if body.len() >= 8 => {
            Event::ToplevelConfigure(u32_at(body, 0) as i32, u32_at(body, 4) as i32)
        }
        (ID_TOPLEVEL, EVT_TOPLEVEL_CLOSE) => Event::ToplevelClose,
        _ => Event::Ignored,
    }
}

pub fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}

/// Read a wayland string and return it with the offset just past its padding.
pub fn string_at(buf: &[u8], off: usize) -> Option<(String, usize)> {
    if off + 4 > buf.len() {
        return None;
    }
    let len = u32_at(buf, off) as usize;
    let start = off + 4;
    let padded = len.div_ceil(4) * 4;
    if start + padded > buf.len() || len == 0 {
        return None;
    }
    Some((
        String::from_utf8_lossy(&buf[start..start + len - 1]).into_owned(),
        start + padded,
    ))
}

// --- the picture ---------------------------------------------------------

/// Draw one frame into `dst` and return the rectangle that changed.
///
/// The picture is chosen so a screenshot can be checked by something that is
/// not a human eye:
///
/// * a **flat background**, identical in every frame, so any drift is a bug;
/// * a **one-pixel border** in a contrasting colour, which shows the buffer's
///   exact extent — if the blit is offset by a row or the stride is wrong, the
///   border is what shows it;
/// * **corner markers** in pure red, green and blue at three corners and *not*
///   the fourth, so the image is asymmetric and a flipped or mirrored blit is
///   visible rather than plausible;
/// * a **bar that sweeps horizontally**, which is the only thing that changes
///   between frames and therefore the only thing that needs damaging.
///
/// `dst` is one buffer, tightly indexed by `stride`; the caller slices it out
/// of the pool.
pub fn draw(dst: &mut [u8], w: i32, h: i32, stride: i32, frame: u32) -> Rect {
    #[inline]
    fn px(dst: &mut [u8], stride: i32, x: i32, y: i32, bgr: [u8; 3]) {
        let o = (y * stride + x * 4) as usize;
        dst[o] = bgr[0];
        dst[o + 1] = bgr[1];
        dst[o + 2] = bgr[2];
        dst[o + 3] = 0xFF;
    }

    const BG: [u8; 3] = [0x60, 0x40, 0x20]; // dark blue-grey
    const BORDER: [u8; 3] = [0x30, 0x30, 0xF0]; // red-ish
    const BAR: [u8; 3] = [0x20, 0xE0, 0xE0]; // yellow
    const RED: [u8; 3] = [0x00, 0x00, 0xFF];
    const GREEN: [u8; 3] = [0x00, 0xFF, 0x00];
    const BLUE: [u8; 3] = [0xFF, 0x00, 0x00];

    if frame == 0 {
        for y in 0..h {
            for x in 0..w {
                let edge = x == 0 || y == 0 || x == w - 1 || y == h - 1;
                px(dst, stride, x, y, if edge { BORDER } else { BG });
            }
        }
        let m = 16.min(w / 4).min(h / 4);
        for y in 0..m {
            for x in 0..m {
                px(dst, stride, 1 + x, 1 + y, RED);
                px(dst, stride, w - 1 - m + x, 1 + y, GREEN);
                px(dst, stride, 1 + x, h - 1 - m + y, BLUE);
                // Bottom-right is deliberately left as background.
            }
        }
        return Rect { x: 0, y: 0, w, h };
    }

    let bar_w = 24.min(w / 4).max(1);
    let span = (w - bar_w - 4).max(1);
    let prev = 2 + ((frame - 1) as i32 * 7) % span;
    let cur = 2 + (frame as i32 * 7) % span;
    let lo = prev.min(cur);
    let hi = (prev.max(cur) + bar_w).min(w - 1);

    let top = h / 3;
    let bot = (h * 2) / 3;
    for y in top..bot {
        for x in lo..hi {
            let inside = x >= cur && x < cur + bar_w;
            px(dst, stride, x, y, if inside { BAR } else { BG });
        }
    }
    Rect {
        x: lo,
        y: top,
        w: hi - lo,
        h: bot - top,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_carries_size_and_opcode_in_the_right_halves() {
        let h = header(7, 6, 8);
        assert_eq!(u32::from_ne_bytes(h[0..4].try_into().unwrap()), 7);
        let w = u32::from_ne_bytes(h[4..8].try_into().unwrap());
        assert_eq!(w >> 16, 8, "size is the high half");
        assert_eq!(w & 0xffff, 6, "opcode is the low half");
    }

    #[test]
    fn a_string_counts_its_nul_and_is_padded_to_four() {
        // "wl_shm" is 6 bytes, 7 with the NUL, padded to 8. The length field
        // says 7, not 6 and not 8 — getting that wrong desynchronises every
        // message after it, which is precisely the failure §2.4 describes.
        let mut out = Vec::new();
        push_string(&mut out, "wl_shm");
        assert_eq!(u32_at(&out, 0), 7);
        assert_eq!(out.len(), 4 + 8);
        assert_eq!(&out[4..11], b"wl_shm\0");
        assert_eq!(out[11], 0, "padding is zero");
    }

    #[test]
    fn an_exactly_aligned_string_gets_no_extra_padding() {
        // "abc" + NUL is 4 bytes, already aligned. A `(4 - len % 4)` that
        // forgot the outer `% 4` would add four bytes of padding here and
        // break framing only for names whose length is 3 mod 4.
        let mut out = Vec::new();
        push_string(&mut out, "abc");
        assert_eq!(u32_at(&out, 0), 4);
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn a_string_round_trips_through_the_decoder() {
        let mut out = Vec::new();
        push_string(&mut out, "xdg_wm_base");
        out.extend_from_slice(&42u32.to_ne_bytes());
        let (s, next) = string_at(&out, 0).unwrap();
        assert_eq!(s, "xdg_wm_base");
        assert_eq!(u32_at(&out, next), 42, "the offset lands past the padding");
    }

    #[test]
    fn bind_expands_new_id_into_three_values() {
        // name, string, version, id — 4 + 16 + 4 + 4 = 28 of body, plus 8 of
        // header. "wl_compositor" is 13 bytes, 14 with NUL, padded to 16.
        let m = bind(3, "wl_compositor", 4, 99);
        assert_eq!(m.len(), 8 + 4 + 4 + 16 + 4 + 4);
        assert_eq!(u32_at(&m, 0), ID_REGISTRY);
        assert_eq!(u32::from_ne_bytes(m[4..8].try_into().unwrap()) >> 16, m.len() as u32);
        assert_eq!(u32_at(&m, 8), 3, "the global's name");
        let (iface, next) = string_at(&m, 12).unwrap();
        assert_eq!(iface, "wl_compositor");
        assert_eq!(u32_at(&m, next), 4, "version");
        assert_eq!(u32_at(&m, next + 4), 99, "the new id");
    }

    #[test]
    fn an_h_argument_is_encoded_exactly_like_an_array() {
        // The claim of docs/design.md §2.3, as a test. A 20-byte descriptor
        // occupies 4 bytes of length plus 20 of payload; 20 is already a
        // multiple of 4, so nothing is padded.
        let descriptor = [0xABu8; 20];
        let mut a = Vec::new();
        push_bytes(&mut a, &descriptor);
        assert_eq!(u32_at(&a, 0), 20);
        assert_eq!(a.len(), 24);

        // And one that is not aligned: 21 bytes pads to 24.
        let mut b = Vec::new();
        push_bytes(&mut b, &[0u8; 21]);
        assert_eq!(u32_at(&b, 0), 21);
        assert_eq!(b.len(), 4 + 24);
    }

    /// Walk a byte stream the way a server's parser does, returning the
    /// (object, opcode, body length) of each message.
    ///
    /// This is the check that matters for §2.4: a stream whose sizes do not
    /// add up desynchronises, and the symptom is that the server sees *none*
    /// of the requests after the bad one rather than a clean error.
    fn frames(buf: &[u8]) -> Vec<(u32, u16, usize)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + 8 <= buf.len() {
            let object = u32_at(buf, at);
            let word2 = u32_at(buf, at + 4);
            let size = (word2 >> 16) as usize;
            assert!(size >= 8, "message at {at} claims size {size}");
            assert!(at + size <= buf.len(), "message at {at} runs past the buffer");
            assert_eq!(size % 4, 0, "message at {at} is not 4-byte aligned");
            out.push((object, (word2 & 0xffff) as u16, size - 8));
            at += size;
        }
        assert_eq!(at, buf.len(), "trailing bytes that are not a message");
        out
    }

    #[test]
    fn the_opening_handshake_is_two_well_framed_messages() {
        assert_eq!(
            frames(&hello()),
            vec![
                (WL_DISPLAY, REQ_DISPLAY_GET_REGISTRY, 4),
                (WL_DISPLAY, REQ_DISPLAY_SYNC, 4),
            ]
        );
    }

    #[test]
    fn create_window_frames_cleanly_for_titles_of_every_alignment() {
        // A title whose length is 0, 1, 2 or 3 mod 4 exercises each padding
        // case. If any of them is mis-sized the stream desynchronises at the
        // *next* message, so `frames` walking to the exact end is the
        // assertion.
        for title in ["", "a", "ab", "abc", "abcd", "a title with spaces"] {
            let bytes = create_window((1, 6), (2, 2), (3, 6), title, "org.example.App");
            let f = frames(&bytes);
            assert_eq!(f.len(), 9, "title {title:?}");
            assert_eq!(f[0].0, ID_REGISTRY, "bind wl_compositor");
            assert_eq!(f[3], (ID_COMPOSITOR, REQ_COMPOSITOR_CREATE_SURFACE, 4));
            assert_eq!(f[4], (ID_WM_BASE, REQ_WM_BASE_GET_XDG_SURFACE, 8));
            assert_eq!(f[5], (ID_XDG_SURFACE, REQ_XDG_SURFACE_GET_TOPLEVEL, 4));
            assert_eq!(f[6].1, REQ_TOPLEVEL_SET_TITLE);
            assert_eq!(f[7].1, REQ_TOPLEVEL_SET_APP_ID);
            assert_eq!(
                f[8],
                (ID_SURFACE, REQ_SURFACE_COMMIT, 0),
                "the first commit must carry no buffer"
            );
        }
    }

    #[test]
    fn create_window_floors_the_versions_it_binds() {
        // The server offers v6 for both; this client understands 4 and 1. A
        // client that bound what it was offered would receive events it has no
        // handler for, which on a strict server is a protocol error.
        let bytes = create_window((1, 6), (2, 2), (3, 6), "t", "a");
        let (iface, next) = string_at(&bytes, 12).unwrap();
        assert_eq!(iface, "wl_compositor");
        assert_eq!(u32_at(&bytes, next), 4);
    }

    #[test]
    fn create_pool_frames_cleanly_for_descriptors_of_every_alignment() {
        // The descriptor is variable-length and its padding is the one place
        // this project's wire format differs from libwayland's. Every residue
        // class has to frame.
        for len in [1usize, 2, 3, 4, 5, 20, 21, 22, 23, 24, 63, 64] {
            let descriptor = vec![0xCDu8; len];
            let bytes = create_pool(&descriptor, 2 * 400 * 300 * 4, 400, 300);
            let f = frames(&bytes);
            assert_eq!(f.len(), 3, "descriptor of {len} bytes");
            assert_eq!(f[0].0, ID_SHM);
            assert_eq!(f[0].1, REQ_SHM_CREATE_POOL);
            // new_id + (length + payload + padding) + size
            assert_eq!(f[0].2, 4 + 4 + len.div_ceil(4) * 4 + 4);
            assert_eq!(f[1], (ID_POOL, REQ_POOL_CREATE_BUFFER, 24));
            assert_eq!(f[2], (ID_POOL, REQ_POOL_CREATE_BUFFER, 24));
        }
    }

    #[test]
    fn create_pool_places_the_second_buffer_after_the_first() {
        // Two buffers in one section. A second offset of 0 would alias them
        // and produce a client that appears to work until the compositor is
        // fast enough to read one while the other is being drawn.
        let (w, h) = (400, 300);
        let buffer_len = (w * h * 4) as u32;
        let bytes = create_pool(&[0u8; 20], (buffer_len as u64) * 2, w, h);
        // Message 0 is create_pool: 8 header + 4 id + 4 len + 20 + 4 size = 40.
        let b0 = 40 + 8;
        let b1 = b0 + 32;
        assert_eq!(u32_at(&bytes, b0), ID_BUFFER0);
        assert_eq!(u32_at(&bytes, b0 + 4), 0, "first buffer at offset 0");
        assert_eq!(u32_at(&bytes, b1), ID_BUFFER1);
        assert_eq!(u32_at(&bytes, b1 + 4), buffer_len, "second buffer after it");
        assert_eq!(u32_at(&bytes, b1 + 16), (w * 4) as u32, "stride");
        assert_eq!(u32_at(&bytes, b1 + 20), FORMAT_XRGB8888);
    }

    #[test]
    fn present_is_attach_damage_frame_commit_in_that_order() {
        let f = frames(&present(1, Rect { x: 3, y: 4, w: 5, h: 6 }));
        assert_eq!(
            f,
            vec![
                (ID_SURFACE, REQ_SURFACE_ATTACH, 12),
                (ID_SURFACE, REQ_SURFACE_DAMAGE, 16),
                (ID_SURFACE, REQ_SURFACE_FRAME, 4),
                (ID_SURFACE, REQ_SURFACE_COMMIT, 0),
            ]
        );
        let bytes = present(1, Rect { x: 3, y: 4, w: 5, h: 6 });
        assert_eq!(u32_at(&bytes, 8), ID_BUFFER1, "slot 1 attaches buffer 1");
        assert_eq!(u32_at(&present(0, Rect { x: 0, y: 0, w: 1, h: 1 }), 8), ID_BUFFER0);
        assert_eq!(u32_at(&bytes, 20 + 8), 3, "damage x");
        assert_eq!(u32_at(&bytes, 20 + 20), 6, "damage h");
    }

    // --- decoding ---------------------------------------------------------

    /// Build an event the way a server would, so the parser is tested against
    /// bytes rather than against itself.
    fn server_event(object: u32, opcode: u16, args: &[u32]) -> Vec<u8> {
        message(object, opcode, args)
    }

    #[test]
    fn the_parser_leaves_a_partial_message_for_the_next_read() {
        let mut stream = server_event(ID_SYNC, EVT_CALLBACK_DONE, &[1]);
        let full = stream.len();
        stream.extend_from_slice(&server_event(ID_FRAME, EVT_CALLBACK_DONE, &[2])[..5]);
        let (events, consumed, err) = parse(&stream);
        assert_eq!(err, None);
        assert_eq!(events, vec![Event::SyncDone]);
        assert_eq!(consumed, full, "the truncated message is not consumed");
    }

    #[test]
    fn an_impossible_size_is_reported_rather_than_skipped() {
        // Framing loss is exactly what §2.4 describes, so it has to surface as
        // an error and not as silence.
        let mut bad = Vec::new();
        bad.extend_from_slice(&header(ID_SURFACE, 0, 4)); // size 4 < header
        let (_, _, err) = parse(&bad);
        assert!(err.is_some());
    }

    #[test]
    fn every_event_this_client_acts_on_is_recognised() {
        let cases: Vec<(Vec<u8>, Event)> = vec![
            (
                server_event(ID_SYNC, EVT_CALLBACK_DONE, &[7]),
                Event::SyncDone,
            ),
            (
                server_event(ID_FRAME, EVT_CALLBACK_DONE, &[7]),
                Event::FrameDone,
            ),
            (
                server_event(ID_BUFFER0, EVT_BUFFER_RELEASE, &[]),
                Event::BufferReleased(0),
            ),
            (
                server_event(ID_BUFFER1, EVT_BUFFER_RELEASE, &[]),
                Event::BufferReleased(1),
            ),
            (server_event(ID_WM_BASE, EVT_WM_BASE_PING, &[99]), Event::Ping(99)),
            (
                server_event(ID_XDG_SURFACE, EVT_XDG_SURFACE_CONFIGURE, &[5]),
                Event::XdgConfigure(5),
            ),
            (
                server_event(ID_TOPLEVEL, EVT_TOPLEVEL_CONFIGURE, &[640, 480, 0]),
                Event::ToplevelConfigure(640, 480),
            ),
            (
                server_event(ID_TOPLEVEL, EVT_TOPLEVEL_CLOSE, &[]),
                Event::ToplevelClose,
            ),
        ];
        for (bytes, expect) in cases {
            let (events, consumed, err) = parse(&bytes);
            assert_eq!(err, None);
            assert_eq!(consumed, bytes.len());
            assert_eq!(events, vec![expect]);
        }
    }

    #[test]
    fn a_registry_global_and_an_error_decode_their_strings() {
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_ne_bytes());
        push_string(&mut body, "xdg_wm_base");
        body.extend_from_slice(&6u32.to_ne_bytes());
        let (events, _, _) = parse(&finish(ID_REGISTRY, EVT_REGISTRY_GLOBAL, body));
        assert_eq!(
            events,
            vec![Event::Global {
                name: 3,
                interface: "xdg_wm_base".into(),
                version: 6,
            }]
        );

        let mut body = Vec::new();
        body.extend_from_slice(&9u32.to_ne_bytes());
        body.extend_from_slice(&2u32.to_ne_bytes());
        push_string(&mut body, "no");
        let (events, _, _) = parse(&finish(WL_DISPLAY, EVT_DISPLAY_ERROR, body));
        assert_eq!(
            events,
            vec![Event::Error {
                object: 9,
                code: 2,
                message: "no".into()
            }]
        );
    }

    #[test]
    fn an_unknown_event_is_ignored_rather_than_fatal() {
        // A compositor that gains a global must not break this client.
        let (events, _, err) = parse(&server_event(1234, 5, &[1, 2, 3]));
        assert_eq!(err, None);
        assert_eq!(events, vec![Event::Ignored]);
    }

    #[test]
    fn the_first_frame_paints_every_pixel_and_reports_so() {
        let (w, h) = (32, 24);
        let stride = w * 4;
        let mut buf = vec![0u8; (stride * h) as usize];
        let damage = draw(&mut buf, w, h, stride, 0);
        assert_eq!(damage, Rect { x: 0, y: 0, w, h });
        // Every pixel opaque: a partially-drawn first frame would show as
        // transparent gaps that GDI renders black.
        assert!(buf.chunks_exact(4).all(|p| p[3] == 0xFF));
        // The corners are the asymmetry that makes a flip visible.
        let at = |x: i32, y: i32| {
            let o = (y * stride + x * 4) as usize;
            [buf[o], buf[o + 1], buf[o + 2]]
        };
        assert_eq!(at(4, 4), [0x00, 0x00, 0xFF], "top-left is red");
        assert_eq!(at(w - 4, 4), [0x00, 0xFF, 0x00], "top-right is green");
        assert_eq!(at(4, h - 4), [0xFF, 0x00, 0x00], "bottom-left is blue");
        assert_ne!(at(w - 4, h - 4), [0x00, 0x00, 0xFF], "bottom-right is not");
    }

    #[test]
    fn later_frames_damage_only_the_band_they_touch() {
        let (w, h) = (200, 120);
        let stride = w * 4;
        let mut buf = vec![0u8; (stride * h) as usize];
        draw(&mut buf, w, h, stride, 0);
        let before = buf.clone();
        let damage = draw(&mut buf, w, h, stride, 1);

        // The reported damage must be a strict subset — otherwise
        // `--partial-damage` is testing nothing.
        assert!(damage.h < h, "the bar is a band, not the whole surface");
        assert!(damage.w < w);

        // And every byte that changed must be inside it. This is the assertion
        // that matters: a client that under-reports damage produces a
        // compositor that looks broken, and the bug is the client's.
        for y in 0..h {
            for x in 0..w {
                let o = (y * stride + x * 4) as usize;
                if buf[o..o + 4] != before[o..o + 4] {
                    assert!(
                        x >= damage.x
                            && x < damage.x + damage.w
                            && y >= damage.y
                            && y < damage.y + damage.h,
                        "pixel {x},{y} changed outside the reported damage"
                    );
                }
            }
        }
    }

    #[test]
    fn drawing_stays_inside_the_buffer_for_awkward_sizes() {
        // Small and non-square, where the corner markers and the bar would
        // overflow if they were sized by a constant rather than clamped.
        for (w, h) in [(16, 16), (17, 9), (64, 12), (12, 64)] {
            let stride = w * 4;
            let mut buf = vec![0u8; (stride * h) as usize];
            for frame in 0..8 {
                let d = draw(&mut buf, w, h, stride, frame);
                assert!(d.x >= 0 && d.y >= 0, "{w}x{h} frame {frame}");
                assert!(d.x + d.w <= w && d.y + d.h <= h, "{w}x{h} frame {frame}");
            }
        }
    }
}
