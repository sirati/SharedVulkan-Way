//! §103.13. The messages between the macOS main process and an application's
//! face process, and their encoding. Pure, so built and tested on every target;
//! `macos_router.rs` (main) and `macos.rs`'s face mode (face) are the two ends.
//!
//! The main process holds the Wayland state, the arena, the session and the
//! clipboard; a face holds one application's Dock tile, menu bar and windows.
//! What crosses is window commands one way and input and window events the
//! other, on a Unix socket. Pixels never cross here: they are IOSurfaces whose
//! mach ports go by mach message *before* the [`ToFace::Surface`] that names
//! them, so a face that reads the message finds the ports already queued.
//!
//! Framing: a little-endian `u32` length, then the body, whose first byte is
//! the message's tag. Strings are a `u32` length and UTF-8 bytes. Hand-written,
//! like the rest of this workspace's wire formats: the set is small, and a
//! field added on one side and not the other is a decode error, not a silent
//! default.

use std::io::{self, Read, Write};

use super::{CursorSpec, Edge, FromUi, IconImage, WindowId};
use crate::hold::PointerHold;
use crate::surface::Rect;

/// Main process to face.
#[derive(Debug, Clone, PartialEq)]
pub enum ToFace {
    Create { id: WindowId, title: String, app_id: String, popup: Option<(WindowId, i32, i32)> },
    Title { id: WindowId, title: String },
    AppId { id: WindowId, app_id: String },
    Icon { id: WindowId, icons: Vec<IconImage> },
    MovePopup { id: WindowId, x: i32, y: i32, w: i32, h: i32 },
    /// A new pair of IOSurfaces for the window, generation `generation`, whose mach
    /// ports were sent ahead of this message. `w`x`h` are device pixels and
    /// `scale` the shadow's pixels per point (§103.18): the layer's
    /// `contentsScale`.
    Surface { id: WindowId, generation: u32, w: i32, h: i32, alpha: bool, scale: i32 },
    /// Show surface `buffer` (0 or 1) of the current pair.
    Present { id: WindowId, buffer: u8 },
    ShiftOrigin { id: WindowId, dx: i32, dy: i32 },
    Map { id: WindowId, w: i32, h: i32, fold: bool },
    Resize { id: WindowId, w: i32, h: i32 },
    SizeHints { id: WindowId, min: (i32, i32), max: (i32, i32) },
    SetWindowState { id: WindowId, maximized: Option<bool>, minimized: Option<bool> },
    Fullscreen { id: WindowId, on: bool },
    Cursor { spec: CursorSpec },
    PointerHold { id: WindowId, hold: Option<PointerHold> },
    BeginMove { id: WindowId },
    BeginResize { id: WindowId, edge: Edge },
    WindowMenu { id: WindowId, x: i32, y: i32 },
    Destroy { id: WindowId },
    Activate { id: WindowId },
    /// The main process is ending; the face quits.
    Bye,
}

/// Face to main process.
#[derive(Debug)]
pub enum FromFace {
    /// The first message. `token` pairs this socket with the mach port the
    /// face sent to the main process's bootstrap service.
    Hello { section: String, ids: Vec<String>, token: u64, pid: u32 },
    Ui(FromUi),
    /// The work area in the window's own coordinates (§38's popup bounds).
    Bounds { id: WindowId, rect: Option<Rect> },
    /// Run the application (`""`), one of its actions or a URL: a Dock tile
    /// click or a Dock-menu pick (`macbundle::request_for`).
    Request { key: String },
    /// The face is quitting on purpose; do not restart it.
    Bye,
}

// ------------------------------------------------------------- writing ---

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f64(&mut self, v: f64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(u8::from(v))
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
    fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
    fn obool(&mut self, v: Option<bool>) -> &mut Self {
        self.u8(match v {
            None => 2,
            Some(b) => u8::from(b),
        })
    }
    fn rect(&mut self, r: Rect) -> &mut Self {
        self.i32(r.x).i32(r.y).i32(r.w).i32(r.h)
    }
    fn icon(&mut self, i: &IconImage) -> &mut Self {
        self.i32(i.width).i32(i.height).bytes(&i.pixels)
    }
}

struct R<'a>(&'a [u8]);

fn short() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "face message too short")
}

impl R<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(short());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> io::Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bool(&mut self) -> io::Result<bool> {
        Ok(self.u8()? != 0)
    }
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u32()? as usize;
        if n > 64 << 20 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "face message field too large"));
        }
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
    }
    fn obool(&mut self) -> io::Result<Option<bool>> {
        Ok(match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        })
    }
    fn rect(&mut self) -> io::Result<Rect> {
        Ok(Rect::new(self.i32()?, self.i32()?, self.i32()?, self.i32()?))
    }
    fn icon(&mut self) -> io::Result<IconImage> {
        Ok(IconImage { width: self.i32()?, height: self.i32()?, pixels: self.bytes()? })
    }
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("unknown face message: {what}"))
}

fn edge_code(e: Edge) -> u8 {
    match e {
        Edge::None => 0,
        Edge::Top => 1,
        Edge::Bottom => 2,
        Edge::Left => 3,
        Edge::TopLeft => 4,
        Edge::BottomLeft => 5,
        Edge::Right => 6,
        Edge::TopRight => 7,
        Edge::BottomRight => 8,
    }
}

fn edge_of(c: u8) -> Edge {
    match c {
        1 => Edge::Top,
        2 => Edge::Bottom,
        3 => Edge::Left,
        4 => Edge::TopLeft,
        5 => Edge::BottomLeft,
        6 => Edge::Right,
        7 => Edge::TopRight,
        8 => Edge::BottomRight,
        _ => Edge::None,
    }
}

impl ToFace {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match self {
            ToFace::Create { id, title, app_id, popup } => {
                w.u8(1).u64(*id).str(title).str(app_id);
                match popup {
                    Some((p, x, y)) => w.u8(1).u64(*p).i32(*x).i32(*y),
                    None => w.u8(0),
                };
            }
            ToFace::Title { id, title } => {
                w.u8(2).u64(*id).str(title);
            }
            ToFace::AppId { id, app_id } => {
                w.u8(3).u64(*id).str(app_id);
            }
            ToFace::Icon { id, icons } => {
                w.u8(4).u64(*id).u32(icons.len() as u32);
                for i in icons {
                    w.icon(i);
                }
            }
            ToFace::MovePopup { id, x, y, w: ww, h } => {
                w.u8(5).u64(*id).i32(*x).i32(*y).i32(*ww).i32(*h);
            }
            ToFace::Surface { id, generation, w: ww, h, alpha, scale } => {
                w.u8(6).u64(*id).u32(*generation).i32(*ww).i32(*h).bool(*alpha).i32(*scale);
            }
            ToFace::Present { id, buffer } => {
                w.u8(7).u64(*id).u8(*buffer);
            }
            ToFace::ShiftOrigin { id, dx, dy } => {
                w.u8(8).u64(*id).i32(*dx).i32(*dy);
            }
            ToFace::Map { id, w: ww, h, fold } => {
                w.u8(9).u64(*id).i32(*ww).i32(*h).bool(*fold);
            }
            ToFace::Resize { id, w: ww, h } => {
                w.u8(10).u64(*id).i32(*ww).i32(*h);
            }
            ToFace::SizeHints { id, min, max } => {
                w.u8(11).u64(*id).i32(min.0).i32(min.1).i32(max.0).i32(max.1);
            }
            ToFace::SetWindowState { id, maximized, minimized } => {
                w.u8(12).u64(*id).obool(*maximized).obool(*minimized);
            }
            ToFace::Fullscreen { id, on } => {
                w.u8(13).u64(*id).bool(*on);
            }
            ToFace::Cursor { spec } => {
                w.u8(14);
                match spec {
                    CursorSpec::Hidden => w.u8(0),
                    CursorSpec::Named(icon) => w.u8(1).str(icon.name()),
                    CursorSpec::Image { image, hotspot_x, hotspot_y, scale } => {
                        w.u8(2).icon(image).i32(*hotspot_x).i32(*hotspot_y).i32(*scale)
                    }
                };
            }
            ToFace::PointerHold { id, hold } => {
                w.u8(15).u64(*id);
                match hold {
                    None => w.u8(0),
                    Some(PointerHold::Locked) => w.u8(1),
                    Some(PointerHold::Confined(r)) => w.u8(2).rect(*r),
                };
            }
            ToFace::BeginMove { id } => {
                w.u8(16).u64(*id);
            }
            ToFace::BeginResize { id, edge } => {
                w.u8(17).u64(*id).u8(edge_code(*edge));
            }
            ToFace::WindowMenu { id, x, y } => {
                w.u8(18).u64(*id).i32(*x).i32(*y);
            }
            ToFace::Destroy { id } => {
                w.u8(19).u64(*id);
            }
            ToFace::Activate { id } => {
                w.u8(20).u64(*id);
            }
            ToFace::Bye => {
                w.u8(21);
            }
        }
        w.0
    }

    pub fn decode(b: &[u8]) -> io::Result<ToFace> {
        let mut r = R(b);
        Ok(match r.u8()? {
            1 => {
                let (id, title, app_id) = (r.u64()?, r.str()?, r.str()?);
                let popup = if r.u8()? == 1 { Some((r.u64()?, r.i32()?, r.i32()?)) } else { None };
                ToFace::Create { id, title, app_id, popup }
            }
            2 => ToFace::Title { id: r.u64()?, title: r.str()? },
            3 => ToFace::AppId { id: r.u64()?, app_id: r.str()? },
            4 => {
                let id = r.u64()?;
                let n = r.u32()?.min(16);
                let mut icons = Vec::new();
                for _ in 0..n {
                    icons.push(r.icon()?);
                }
                ToFace::Icon { id, icons }
            }
            5 => ToFace::MovePopup { id: r.u64()?, x: r.i32()?, y: r.i32()?, w: r.i32()?, h: r.i32()? },
            6 => ToFace::Surface {
                id: r.u64()?,
                generation: r.u32()?,
                w: r.i32()?,
                h: r.i32()?,
                alpha: r.bool()?,
                scale: r.i32()?,
            },
            7 => ToFace::Present { id: r.u64()?, buffer: r.u8()? },
            8 => ToFace::ShiftOrigin { id: r.u64()?, dx: r.i32()?, dy: r.i32()? },
            9 => ToFace::Map { id: r.u64()?, w: r.i32()?, h: r.i32()?, fold: r.bool()? },
            10 => ToFace::Resize { id: r.u64()?, w: r.i32()?, h: r.i32()? },
            11 => ToFace::SizeHints { id: r.u64()?, min: (r.i32()?, r.i32()?), max: (r.i32()?, r.i32()?) },
            12 => ToFace::SetWindowState { id: r.u64()?, maximized: r.obool()?, minimized: r.obool()? },
            13 => ToFace::Fullscreen { id: r.u64()?, on: r.bool()? },
            14 => ToFace::Cursor {
                spec: match r.u8()? {
                    0 => CursorSpec::Hidden,
                    1 => CursorSpec::Named(r.str()?.parse().unwrap_or_default()),
                    2 => CursorSpec::Image {
                        image: r.icon()?,
                        hotspot_x: r.i32()?,
                        hotspot_y: r.i32()?,
                        scale: r.i32()?,
                    },
                    t => return Err(bad(&format!("cursor {t}"))),
                },
            },
            15 => {
                let id = r.u64()?;
                let hold = match r.u8()? {
                    0 => None,
                    1 => Some(PointerHold::Locked),
                    _ => Some(PointerHold::Confined(r.rect()?)),
                };
                ToFace::PointerHold { id, hold }
            }
            16 => ToFace::BeginMove { id: r.u64()? },
            17 => ToFace::BeginResize { id: r.u64()?, edge: edge_of(r.u8()?) },
            18 => ToFace::WindowMenu { id: r.u64()?, x: r.i32()?, y: r.i32()? },
            19 => ToFace::Destroy { id: r.u64()? },
            20 => ToFace::Activate { id: r.u64()? },
            21 => ToFace::Bye,
            t => return Err(bad(&format!("to-face tag {t}"))),
        })
    }
}

impl FromFace {
    /// `None` for a `FromUi` a face never sends (the clipboard, the layout and
    /// touch are the main process's own).
    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut w = W(Vec::new());
        match self {
            FromFace::Hello { section, ids, token, pid } => {
                w.u8(1).str(section).u32(ids.len() as u32);
                for i in ids {
                    w.str(i);
                }
                w.u64(*token).u32(*pid);
            }
            FromFace::Bounds { id, rect } => {
                w.u8(2).u64(*id);
                match rect {
                    Some(r) => w.u8(1).rect(*r),
                    None => w.u8(0),
                };
            }
            FromFace::Request { key } => {
                w.u8(3).str(key);
            }
            FromFace::Bye => {
                w.u8(4);
            }
            FromFace::Ui(e) => {
                w.u8(10);
                match e {
                    FromUi::Resized { id, width, height } => w.u8(1).u64(*id).i32(*width).i32(*height),
                    FromUi::CloseRequested { id } => w.u8(2).u64(*id),
                    FromUi::Maximized { id, on } => w.u8(3).u64(*id).bool(*on),
                    FromUi::Minimized { id, on } => w.u8(4).u64(*id).bool(*on),
                    FromUi::Activated { id, on } => w.u8(5).u64(*id).bool(*on),
                    FromUi::KeyboardFocus { id, on, caps, num } => {
                        w.u8(6).u64(*id).bool(*on).bool(*caps).bool(*num)
                    }
                    FromUi::Key { id, keycode, pressed, time } => {
                        w.u8(7).u64(*id).u32(u32::from(*keycode)).bool(*pressed).u32(*time)
                    }
                    FromUi::PointerEnter { id, x, y, time } => w.u8(8).u64(*id).f64(*x).f64(*y).u32(*time),
                    FromUi::PointerLeave { id, time } => w.u8(9).u64(*id).u32(*time),
                    FromUi::PointerMotion { id, x, y, time } => w.u8(10).u64(*id).f64(*x).f64(*y).u32(*time),
                    FromUi::PointerButton { id, button, pressed, time } => {
                        w.u8(11).u64(*id).u32(*button).bool(*pressed).u32(*time)
                    }
                    FromUi::PointerAxis { id, horizontal, vertical, v120_h, v120_v, time } => w
                        .u8(12)
                        .u64(*id)
                        .f64(*horizontal)
                        .f64(*vertical)
                        .i32(*v120_h)
                        .i32(*v120_v)
                        .u32(*time),
                    FromUi::PointerRelative { id, dx, dy, time } => w.u8(13).u64(*id).f64(*dx).f64(*dy).u32(*time),
                    _ => return None,
                };
            }
        }
        Some(w.0)
    }

    pub fn decode(b: &[u8]) -> io::Result<FromFace> {
        let mut r = R(b);
        Ok(match r.u8()? {
            1 => {
                let section = r.str()?;
                let n = r.u32()?.min(64);
                let mut ids = Vec::new();
                for _ in 0..n {
                    ids.push(r.str()?);
                }
                FromFace::Hello { section, ids, token: r.u64()?, pid: r.u32()? }
            }
            2 => {
                let id = r.u64()?;
                let rect = if r.u8()? == 1 { Some(r.rect()?) } else { None };
                FromFace::Bounds { id, rect }
            }
            3 => FromFace::Request { key: r.str()? },
            4 => FromFace::Bye,
            10 => FromFace::Ui(match r.u8()? {
                1 => FromUi::Resized { id: r.u64()?, width: r.i32()?, height: r.i32()? },
                2 => FromUi::CloseRequested { id: r.u64()? },
                3 => FromUi::Maximized { id: r.u64()?, on: r.bool()? },
                4 => FromUi::Minimized { id: r.u64()?, on: r.bool()? },
                5 => FromUi::Activated { id: r.u64()?, on: r.bool()? },
                6 => FromUi::KeyboardFocus { id: r.u64()?, on: r.bool()?, caps: r.bool()?, num: r.bool()? },
                7 => FromUi::Key { id: r.u64()?, keycode: r.u32()? as u16, pressed: r.bool()?, time: r.u32()? },
                8 => FromUi::PointerEnter { id: r.u64()?, x: r.f64()?, y: r.f64()?, time: r.u32()? },
                9 => FromUi::PointerLeave { id: r.u64()?, time: r.u32()? },
                10 => FromUi::PointerMotion { id: r.u64()?, x: r.f64()?, y: r.f64()?, time: r.u32()? },
                11 => FromUi::PointerButton { id: r.u64()?, button: r.u32()?, pressed: r.bool()?, time: r.u32()? },
                12 => FromUi::PointerAxis {
                    id: r.u64()?,
                    horizontal: r.f64()?,
                    vertical: r.f64()?,
                    v120_h: r.i32()?,
                    v120_v: r.i32()?,
                    time: r.u32()?,
                },
                13 => FromUi::PointerRelative { id: r.u64()?, dx: r.f64()?, dy: r.f64()?, time: r.u32()? },
                t => return Err(bad(&format!("ui event {t}"))),
            }),
            t => return Err(bad(&format!("from-face tag {t}"))),
        })
    }
}

/// Write one framed message.
pub fn write_frame(w: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
    w.write_all(&buf)
}

/// Read one framed message; `None` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > 80 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "face frame too large"));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// The ids a face answers to: what a window's `app_id` is matched against
/// (`macos_router`). The section, the host's id for the application, the
/// desktop entry's basename and its `StartupWMClass`, without `.desktop`
/// and compared case-insensitively.
pub fn face_ids(section: &str, host_app: &str, desktop: &str, wm_class: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let base = desktop.rsplit('/').next().unwrap_or(desktop).trim_end_matches(".desktop");
    for s in [Some(section), Some(host_app), Some(base), wm_class].into_iter().flatten() {
        let s = s.trim().to_ascii_lowercase();
        if !s.is_empty() && !v.contains(&s) {
            v.push(s);
        }
    }
    v
}

/// Whether a window's `app_id` belongs to a face with these ids.
pub fn app_id_matches(ids: &[String], app_id: &str) -> bool {
    let a = app_id.trim().to_ascii_lowercase();
    !a.is_empty() && ids.iter().any(|i| *i == a || a.trim_end_matches(".desktop") == i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::input::pointer::CursorIcon;

    fn round(m: ToFace) {
        let b = m.encode();
        let mut framed = Vec::new();
        write_frame(&mut framed, &b).unwrap();
        let got = read_frame(&mut framed.as_slice()).unwrap().unwrap();
        assert_eq!(ToFace::decode(&got).unwrap(), m);
    }

    #[test]
    fn every_window_command_round_trips() {
        round(ToFace::Create { id: 7, title: "Wéb".into(), app_id: "firefox".into(), popup: None });
        round(ToFace::Create { id: 8, title: String::new(), app_id: String::new(), popup: Some((7, -3, 40)) });
        round(ToFace::Icon { id: 7, icons: vec![IconImage { width: 2, height: 1, pixels: vec![1, 2, 3, 4, 5, 6, 7, 8] }] });
        round(ToFace::Surface { id: 7, generation: 3, w: 2560, h: 1418, alpha: true, scale: 2 });
        round(ToFace::Present { id: 7, buffer: 1 });
        round(ToFace::Map { id: 7, w: 640, h: 480, fold: true });
        round(ToFace::SetWindowState { id: 7, maximized: Some(true), minimized: None });
        round(ToFace::Cursor { spec: CursorSpec::Named(CursorIcon::EwResize) });
        round(ToFace::Cursor { spec: CursorSpec::Hidden });
        let image = IconImage { width: 2, height: 2, pixels: vec![9; 16] };
        round(ToFace::Cursor { spec: CursorSpec::Image { image, hotspot_x: 1, hotspot_y: 0, scale: 2 } });
        round(ToFace::PointerHold { id: 7, hold: Some(PointerHold::Confined(Rect::new(1, 2, 3, 4))) });
        round(ToFace::BeginResize { id: 7, edge: Edge::BottomRight });
        round(ToFace::SizeHints { id: 7, min: (500, 120), max: (0, 0) });
        round(ToFace::Bye);
    }

    #[test]
    fn input_round_trips_and_main_only_events_are_not_encoded() {
        let e = FromFace::Ui(FromUi::Key { id: 3, keycode: 30, pressed: true, time: 99 });
        match FromFace::decode(&e.encode().unwrap()).unwrap() {
            FromFace::Ui(FromUi::Key { id: 3, keycode: 30, pressed: true, time: 99 }) => {}
            other => panic!("{other:?}"),
        }
        let a = FromFace::Ui(FromUi::PointerAxis { id: 1, horizontal: 0.5, vertical: -10.0, v120_h: 0, v120_v: -120, time: 5 });
        assert!(matches!(
            FromFace::decode(&a.encode().unwrap()).unwrap(),
            FromFace::Ui(FromUi::PointerAxis { v120_v: -120, .. })
        ));
        assert!(FromFace::Ui(FromUi::Layout { name: "us" }).encode().is_none());
        let h = FromFace::Hello { section: "firefox".into(), ids: vec!["firefox".into()], token: 42, pid: 9 };
        assert!(matches!(FromFace::decode(&h.encode().unwrap()).unwrap(), FromFace::Hello { token: 42, pid: 9, .. }));
    }

    #[test]
    fn a_short_or_unknown_message_is_an_error_not_a_default() {
        assert!(ToFace::decode(&[1, 0]).is_err());
        assert!(ToFace::decode(&[99]).is_err());
        assert!(FromFace::decode(&[]).is_err());
        assert!(read_frame(&mut [].as_slice()).unwrap().is_none(), "a clean end of stream");
    }

    #[test]
    fn windows_find_their_application_by_app_id() {
        let ids = face_ids("console", "console", "/nix/store/x/share/applications/org.gnome.Console.desktop", None);
        assert_eq!(ids, vec!["console", "org.gnome.console"]);
        assert!(app_id_matches(&ids, "org.gnome.Console"));
        assert!(!app_id_matches(&ids, "firefox"));
        assert!(!app_id_matches(&ids, ""));
        let ff = face_ids("firefox", "firefox", "firefox.desktop", Some("firefox"));
        assert_eq!(ff, vec!["firefox"]);
    }
}
