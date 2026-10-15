//! The caption of a window whose title bar is the client's, as numbers.
//!
//! ADDED FOR §82 (docs/waycompose-design.md). `win32.rs` answers `WM_NCHITTEST`
//! and paints the caption-button corner for a window whose caption was folded
//! into its client area (§31.3). Both are decisions about rectangles, and until
//! this module both were written inline against live Win32 calls — so the only
//! test of "does the top edge still resize" was a guest run nobody had done
//! (§31.13 item 4). Everything here is pure, compiled on every target, and
//! tested on the machine that builds it. `win32.rs` gathers the numbers
//! (window rectangle, DPI, metrics, DWM's button bounds) and asks.
//!
//! # What is decided here
//!
//! * [`hit`]: for a point `DefWindowProc` called `HTCLIENT`, whether it is the
//!   top resize band, one of the three caption buttons, or the client's. The
//!   client's includes the empty part of Firefox's tab strip, on purpose:
//!   Wayland has no protocol by which a client publishes its draggable region
//!   (§22.8), so a drag there is the client's to start, and it starts it with
//!   `xdg_toplevel.move`, which `win32.rs` hands to `SC_MOVE` (§22.7).
//! * [`rescale`]: a metric at one DPI expressed at another, with `MulDiv`'s
//!   rounding, for the fallback when `GetSystemMetricsForDpi` is missing.
//! * [`fallback_buttons`]: where Windows' three buttons go when DWM will not
//!   say. Wine's `dwmapi` answers `DWMWA_CAPTION_BUTTON_BOUNDS` with
//!   `E_NOTIMPL` (§67.4); before §82 that meant a folded window under Wine had
//!   no buttons at all.
//! * [`corner_is_clear`]: whether the client drew anything under the button
//!   strip. That is the one symptom §31.9 could only see in a photograph —
//!   Firefox's tab-list chevron and close button hidden under Windows' — and
//!   the compositor has the pixels, so it can say so in its log.

/// Windows' reference DPI: 100% scaling.
pub const BASE_DPI: u32 = 96;

/// A rectangle, `right` and `bottom` exclusive, in whatever coordinates the
/// caller is using. Win32's `RECT` without the dependency on `windows-sys`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }
}

/// `MulDiv(v, to, from)`: a length at `from_dpi`, expressed at `to_dpi`.
///
/// Rounded half away from zero, which is what `MulDiv` does and therefore what
/// Windows' own scaled metrics do. Truncating instead is wrong by one pixel at
/// exactly the scales people use: 23 px of caption at 125% is 28.75, which
/// Windows calls 29 and a truncating division calls 28.
pub fn rescale(v: i32, from_dpi: u32, to_dpi: u32) -> i32 {
    if from_dpi == 0 || from_dpi == to_dpi {
        return v;
    }
    let n = i64::from(v) * i64::from(to_dpi);
    let d = i64::from(from_dpi);
    let r = if n >= 0 { (n + d / 2) / d } else { (n - d / 2) / d };
    r as i32
}

/// The resize band around a sizing window: the frame plus the padded border.
///
/// `SM_CXSIZEFRAME` alone is 4 px at 100% and is the drawn line, not the grab
/// area; since Vista the sensitive band is that plus `SM_CXPADDEDBORDER`.
/// Separate from the Win32 call so that the arithmetic at 125% and 150% is a
/// test rather than a hope.
pub fn resize_band(size_frame: i32, padded_border: i32) -> i32 {
    size_frame + padded_border
}

/// What a point in a folded caption is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// The client's: Firefox's tabs, its empty tab-strip space, everything
    /// below. A drag here is reported to the client as a pointer event and
    /// comes back, if the client wants a move, as `xdg_toplevel.move`.
    Client,
    Top,
    TopLeft,
    TopRight,
    MinButton,
    MaxButton,
    Close,
}

impl Hit {
    /// The `HT*` value `WM_NCHITTEST` answers with. Numbers from `WinUser.h`,
    /// spelled here so this file needs no Win32 import.
    pub fn ht(self) -> isize {
        match self {
            Hit::Client => 1,     // HTCLIENT
            Hit::MinButton => 8,  // HTMINBUTTON (HTREDUCE)
            Hit::MaxButton => 9,  // HTMAXBUTTON (HTZOOM)
            Hit::Top => 12,       // HTTOP
            Hit::TopLeft => 13,   // HTTOPLEFT
            Hit::TopRight => 14,  // HTTOPRIGHT
            Hit::Close => 20,     // HTCLOSE
        }
    }
}

/// Everything [`hit`] needs, in **screen** coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// `GetWindowRect`: the whole window, invisible resize border included.
    pub window: Rect,
    /// The resize band, x and y, at this window's DPI. See [`resize_band`].
    pub band: (i32, i32),
    /// The three caption buttons' strip, if this window has them — DWM's
    /// bounds, or [`fallback_buttons`] when DWM will not say. `None` under
    /// `--caption-buttons off`.
    pub buttons: Option<Rect>,
    pub maximized: bool,
}

/// What `WM_NCHITTEST` should answer for a point `DefWindowProc` called
/// `HTCLIENT`, in a window whose caption is folded into its client area.
///
/// # The order, and why
///
/// 1. **The top resize band of a restored window.** Folding the caption took
///    it: it used to be non-client and `DefWindowProc` now calls it client.
///    It wins over the buttons because that is what a native Windows 11 window
///    does — the top few pixels above the close button resize — and because a
///    window that cannot be resized from the top is a regression no picture
///    shows. A maximised window has no top edge to drag; Windows has pushed it
///    off the monitor, and a resize cursor over the first rows of the tab strip
///    would be a bug.
/// 2. **The caption buttons.** Thirds of the strip, minimise, maximise, close,
///    left to right. On real Windows `DwmDefWindowProc` is asked first and
///    answers for DWM's own buttons, so this arm is reached only where DWM did
///    not — but answering `HTMAXBUTTON` here is also what Windows 11 watches
///    for to raise the snap-layouts flyout, so the fallback keeps that working
///    rather than merely drawing a button.
/// 3. **Everything else is the client's**, including empty tab-strip space.
///    §22.8: no protocol says which of Firefox's pixels are a title bar, so
///    `HTCAPTION` would make a tab undraggable by guessing. The client starts
///    the move itself, with `xdg_toplevel.move`; the double-click that
///    maximises is the client's too (`DoTitlebarAction`, `set_maximized`).
pub fn hit(g: &Geometry, x: i32, y: i32) -> Hit {
    let w = g.window;
    let (bx, by) = g.band;
    if !g.maximized && y < w.top + by {
        return if x < w.left + bx {
            Hit::TopLeft
        } else if x >= w.right - bx {
            Hit::TopRight
        } else {
            Hit::Top
        };
    }
    if let Some(b) = g.buttons
        && !b.is_empty()
        && b.contains(x, y)
    {
        // Integer thirds, and the last third takes the remainder, so the close
        // button is never a pixel narrower than the strip it ends.
        let third = b.width() / 3;
        let i = if third <= 0 { 2 } else { ((x - b.left) / third).min(2) };
        return match i {
            0 => Hit::MinButton,
            1 => Hit::MaxButton,
            _ => Hit::Close,
        };
    }
    Hit::Client
}

/// The caption-button strip in **client** coordinates when DWM will not say:
/// three buttons of `button_w` at the client's right edge, `height` tall from
/// the client's top.
///
/// `button_w` is `SM_CXSIZE` at the window's DPI and `height` is the folded
/// caption (`nc_top`), so the strip is exactly the band the caption used to
/// occupy. Nothing here knows what DWM would have answered, and that is the
/// point: it is the fallback for when nothing does.
pub fn fallback_buttons(client_width: i32, button_w: i32, height: i32) -> Rect {
    let w = (3 * button_w).min(client_width).max(0);
    Rect { left: client_width - w, top: 0, right: client_width, bottom: height.max(0) }
}

/// Did the client leave the button strip empty?
///
/// `pixels` is the shadow — tightly packed BGRA, `width` pixels a row — and
/// `strip` is the button strip in client (= surface) coordinates. The strip is
/// clear if almost every pixel is within a small distance of the strip's most
/// common colour, which is what an empty, reserved corner of a tab strip looks
/// like and what a corner with a chevron and a close button in it does not.
///
/// Returns `None` when there is nothing to judge — no pixels, or a strip that
/// is off the buffer — rather than an answer about pixels that do not exist,
/// which is §31.6's panic lesson in a second place.
pub fn corner_is_clear(pixels: &[u8], width: i32, height: i32, strip: Rect) -> Option<bool> {
    let s = Rect {
        left: strip.left.max(0),
        top: strip.top.max(0),
        right: strip.right.min(width),
        bottom: strip.bottom.min(height),
    };
    if width <= 0 || height <= 0 || s.is_empty() {
        return None;
    }
    let at = |x: i32, y: i32| -> Option<[u8; 3]> {
        let o = ((y * width + x) * 4) as usize;
        pixels.get(o..o + 3).map(|p| [p[0], p[1], p[2]])
    };
    // The most common colour, from a coarse histogram. A tab strip's empty
    // corner is one flat colour; anti-aliased glyphs are a scatter of others.
    let mut counts: std::collections::HashMap<[u8; 3], u32> = std::collections::HashMap::new();
    let mut total = 0u32;
    for y in s.top..s.bottom {
        for x in s.left..s.right {
            *counts.entry(at(x, y)?).or_insert(0) += 1;
            total += 1;
        }
    }
    let (bg, _) = counts.iter().max_by_key(|(_, n)| **n)?;
    let close = |c: &[u8; 3]| c.iter().zip(bg.iter()).all(|(a, b)| a.abs_diff(*b) <= 12);
    let off: u32 = counts.iter().filter(|(c, _)| !close(c)).map(|(_, n)| *n).sum();
    // One percent, which is a glyph's worth in a 146x31 strip: Firefox's
    // close button alone is ~200 pixels of non-background there, and a strip
    // that is merely a gradient stays well inside it.
    Some(off * 100 <= total)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DPI ---------------------------------------------------------------

    /// The three scales a user picks in Settings, and the metrics this module
    /// is fed. 23 px of caption, 4 px of frame and 4 px of padded border are
    /// Windows 10/11's values at 100%.
    #[test]
    fn rescale_rounds_like_muldiv_at_125_and_150() {
        assert_eq!(rescale(23, 96, 96), 23);
        assert_eq!(rescale(23, 96, 120), 29); // 28.75
        assert_eq!(rescale(23, 96, 144), 35); // 34.5, half away from zero
        assert_eq!(rescale(4, 96, 120), 5);
        assert_eq!(rescale(4, 96, 144), 6);
        // And back down, for a window dragged from a 150% monitor to a 100%
        // one: the fallback scales from the *system* DPI, not from 96.
        assert_eq!(rescale(6, 144, 96), 4);
        assert_eq!(rescale(-8, 96, 144), -12);
    }

    #[test]
    fn resize_band_scales_with_its_two_halves() {
        assert_eq!(resize_band(4, 4), 8);
        assert_eq!(resize_band(rescale(4, 96, 120), rescale(4, 96, 120)), 10);
        assert_eq!(resize_band(rescale(4, 96, 144), rescale(4, 96, 144)), 12);
    }

    // --- hit testing ---------------------------------------------------------

    /// A restored window at `dpi`, 1000x700 at (100, 100), with the measured
    /// 146x30 strip of buttons scaled to that DPI at its top-right.
    fn restored(dpi: u32) -> Geometry {
        let b = rescale(8, 96, dpi);
        let window = Rect { left: 100, top: 100, right: 1100, bottom: 800 };
        let right = window.right - b;
        Geometry {
            window,
            band: (b, b),
            buttons: Some(Rect {
                left: right - rescale(146, 96, dpi),
                top: window.top,
                right,
                bottom: window.top + rescale(31, 96, dpi),
            }),
            maximized: false,
        }
    }

    #[test]
    fn the_top_edge_of_a_restored_window_resizes_at_every_scale() {
        for dpi in [96, 120, 144] {
            let g = restored(dpi);
            let b = g.band.1;
            assert_eq!(hit(&g, 500, 100), Hit::Top, "{dpi}");
            assert_eq!(hit(&g, 500, 100 + b - 1), Hit::Top, "{dpi}");
            // One pixel below the band is the tab strip, and the client's.
            assert_eq!(hit(&g, 500, 100 + b), Hit::Client, "{dpi}");
            assert_eq!(hit(&g, 100, 100), Hit::TopLeft, "{dpi}");
            assert_eq!(hit(&g, 1099, 100), Hit::TopRight, "{dpi}");
        }
    }

    /// The band is the scaled one: at 150% a point 10 px down is still the
    /// edge, where at 100% it is already the tab strip.
    #[test]
    fn the_band_is_thicker_at_150_than_at_100() {
        assert_eq!(hit(&restored(96), 500, 110), Hit::Client);
        assert_eq!(hit(&restored(144), 500, 110), Hit::Top);
    }

    #[test]
    fn the_buttons_are_thirds_left_to_right() {
        for dpi in [96, 120, 144] {
            let g = restored(dpi);
            let s = g.buttons.unwrap();
            let y = (s.top + s.bottom) / 2;
            let third = s.width() / 3;
            assert_eq!(hit(&g, s.left, y), Hit::MinButton, "{dpi}");
            assert_eq!(hit(&g, s.left + third, y), Hit::MaxButton, "{dpi}");
            assert_eq!(hit(&g, s.left + 2 * third, y), Hit::Close, "{dpi}");
            assert_eq!(hit(&g, s.right - 1, y), Hit::Close, "{dpi}");
            // Left of the strip is the tab strip: Firefox's empty space, where
            // a drag is the client's to start.
            assert_eq!(hit(&g, s.left - 1, y), Hit::Client, "{dpi}");
            // Below it too: the strip is the caption's height, not the tab
            // strip's, which is the alignment §82 makes the client match.
            assert_eq!(hit(&g, s.left + third, s.bottom), Hit::Client, "{dpi}");
        }
    }

    /// The top band of a restored window wins over the buttons, as on a
    /// native window; on a maximised one there is no band and the very top
    /// row is the close button — a pointer thrown at the corner of the screen
    /// closes the window, which is the whole reason Windows maximises that way.
    #[test]
    fn a_maximised_window_has_no_top_band_and_the_corner_is_close() {
        let mut g = restored(96);
        let s = g.buttons.unwrap();
        // Restored: the band, over the close button as everywhere else.
        assert_eq!(hit(&g, s.right - 1, s.top), Hit::Top);
        assert_eq!(hit(&g, s.left + 2, s.top), Hit::Top);
        g.maximized = true;
        assert_eq!(hit(&g, s.right - 1, s.top), Hit::Close);
        assert_eq!(hit(&g, s.left + 2, s.top), Hit::MinButton);
        assert_eq!(hit(&g, 500, 100), Hit::Client);
    }

    /// `--caption-buttons off`: the corner is the client's and nothing is a
    /// button. The top edge still resizes.
    #[test]
    fn without_buttons_the_corner_is_the_clients() {
        let mut g = restored(120);
        let s = g.buttons.take().unwrap();
        assert_eq!(hit(&g, s.left + 5, s.top + 15), Hit::Client);
        assert_eq!(hit(&g, 500, 100), Hit::Top);
    }

    #[test]
    fn the_ht_values_are_winuser_h() {
        assert_eq!(Hit::Client.ht(), 1);
        assert_eq!(Hit::MaxButton.ht(), 9);
        assert_eq!(Hit::Close.ht(), 20);
        assert_eq!(Hit::Top.ht(), 12);
    }

    // --- the fallback strip ---------------------------------------------------

    #[test]
    fn the_fallback_strip_is_three_buttons_at_the_right_edge() {
        let r = fallback_buttons(1280, 36, 31);
        assert_eq!(r, Rect { left: 1280 - 108, top: 0, right: 1280, bottom: 31 });
        // At 150%, from scaled metrics.
        let r = fallback_buttons(1280, rescale(36, 96, 144), rescale(31, 96, 144));
        assert_eq!((r.width(), r.bottom - r.top), (162, 47));
        // A window narrower than three buttons is all buttons, never negative.
        assert_eq!(fallback_buttons(50, 36, 31).left, 0);
    }

    // --- the corner check ------------------------------------------------------

    fn canvas(w: i32, h: i32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.iter().copied().cycle().take((w * h * 4) as usize).collect()
    }

    #[test]
    fn an_empty_corner_is_clear_and_a_drawn_one_is_not() {
        let (w, h) = (300, 50);
        let strip = Rect { left: 154, top: 0, right: 300, bottom: 31 };
        let mut px = canvas(w, h, [0xfb, 0xfa, 0xfa, 0xff]);
        assert_eq!(corner_is_clear(&px, w, h, strip), Some(true));
        // A 14x14 dark glyph — Firefox's close button — in the middle of it.
        for y in 8..22 {
            for x in 260..274 {
                let o = ((y * w + x) * 4) as usize;
                px[o..o + 3].copy_from_slice(&[0x20, 0x20, 0x20]);
            }
        }
        assert_eq!(corner_is_clear(&px, w, h, strip), Some(false));
        // A glyph *outside* the strip is not the strip's business.
        let left = Rect { left: 0, top: 0, right: 100, bottom: 31 };
        assert_eq!(corner_is_clear(&px, w, h, left), Some(true));
    }

    #[test]
    fn nothing_to_judge_is_none_not_a_verdict() {
        let strip = Rect { left: 0, top: 0, right: 10, bottom: 10 };
        assert_eq!(corner_is_clear(&[], 0, 0, strip), None);
        let px = canvas(4, 4, [0; 4]);
        let off = Rect { left: 10, top: 10, right: 20, bottom: 20 };
        assert_eq!(corner_is_clear(&px, 4, 4, off), None);
    }
}
