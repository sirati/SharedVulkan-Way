//! §96. Holding the pointer: `zwp_pointer_constraints_v1` and
//! `zwp_relative_pointer_manager_v1`, the arithmetic half.
//!
//! # Why a game needs both
//!
//! A first-person game turns the view by the *distance* the mouse moved, not by
//! where the pointer is. On Wayland it asks for two things together: a **lock**
//! (`zwp_locked_pointer_v1`), so the pointer stops moving and cannot leave the
//! window, and **relative motion** (`zwp_relative_pointer_v1`), so it keeps
//! hearing how far the mouse went while the pointer stands still. GLFW does
//! exactly this for `GLFW_CURSOR_DISABLED`, and Minecraft's mouse-look is that
//! mode; without the two globals GLFW logs "The compositor does not support
//! pointer locking" and the view never turns.
//!
//! On Windows the same thing is two calls: `ClipCursor` keeps the system
//! pointer where it is, and raw input (`WM_INPUT`) reports the device's motion,
//! which a clipped pointer no longer does. Those live in `platform/win32.rs`;
//! what lives here is everything about them that is arithmetic, so that it is
//! tested on Linux by `cargo test` like `crate::input`'s keycode table.
//!
//! # A confinement is a rectangle, and the region is not
//!
//! `zwp_confined_pointer_v1` confines to a *region*, which is any union of
//! rectangles with holes. `ClipCursor` takes one rectangle. The platform is
//! therefore given the region's bounding box, clipped to the surface, and the
//! compositor clamps every position to the same box before a client sees it,
//! so the two agree. For the region every client in practice sends — none, or
//! the whole surface — the bounding box *is* the region. A confinement to an
//! L-shape would let the pointer into the notch; no client this project
//! targets asks for one, and it is written down rather than discovered.
#![allow(dead_code)]

use smithay::wayland::compositor::{RectangleKind, RegionAttributes};

use crate::surface::Rect;

/// What the platform is asked to do with the system pointer.
///
/// Crosses to the UI thread inside `ToUi::PointerHold`. `None` in that message
/// is "let go", and it is sent for every way a hold can end: the client
/// destroyed the constraint, the window lost the keyboard, the pointer left the
/// surface, or the window went away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerHold {
    /// `zwp_locked_pointer_v1`: the pointer stays where it is, and the client
    /// hears only relative motion.
    Locked,
    /// `zwp_confined_pointer_v1`: the pointer moves, inside this rectangle, in
    /// the window's surface coordinates (the coordinates `FromUi::PointerMotion`
    /// carries).
    Confined(Rect),
}

/// The rectangle a constraint's region allows, in surface-local coordinates.
///
/// `None` for the region is the protocol's "the whole surface". Otherwise the
/// bounding box of the rectangles the region *adds*, clipped to the surface;
/// subtracted rectangles do not shrink it (see the module header). `None` back
/// means the region and the surface do not meet, and a constraint there can
/// never activate.
pub fn region_bounds(size: (i32, i32), region: Option<&RegionAttributes>) -> Option<Rect> {
    if size.0 <= 0 || size.1 <= 0 {
        return None;
    }
    let whole = Rect::new(0, 0, size.0, size.1);
    let Some(region) = region else { return Some(whole) };
    let mut bounds: Option<(i32, i32, i32, i32)> = None;
    for (kind, r) in &region.rects {
        if !matches!(kind, RectangleKind::Add) || r.size.w <= 0 || r.size.h <= 0 {
            continue;
        }
        let (x0, y0) = (r.loc.x.max(0), r.loc.y.max(0));
        let (x1, y1) = ((r.loc.x + r.size.w).min(size.0), (r.loc.y + r.size.h).min(size.1));
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        bounds = Some(match bounds {
            None => (x0, y0, x1, y1),
            Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
        });
    }
    bounds.map(|(x0, y0, x1, y1)| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// Keep a point inside a rectangle, **last pixel inclusive**.
///
/// The far edge is `x + w - 1`, not `x + w`: a pointer at `x + w` is outside a
/// surface `w` pixels wide, and WLCS's `confined_pointer_movement_is_constrained`
/// asks for exactly `(w - 1, h - 1)` after pushing the pointer far past the
/// corner. `ClipCursor`'s `RECT` is exclusive on the right and bottom for the
/// same reason, so the two agree.
pub fn clamp_into(r: Rect, x: f64, y: f64) -> (f64, f64) {
    let (x0, y0) = (f64::from(r.x), f64::from(r.y));
    let (x1, y1) = (f64::from(r.x + r.w.max(1) - 1), f64::from(r.y + r.h.max(1) - 1));
    (x.clamp(x0, x1), y.clamp(y0, y1))
}

/// Motion from a raw-input device that reports **positions**, turned into
/// distances.
///
/// `RAWMOUSE` is relative for an ordinary mouse and absolute for a tablet, a
/// touchscreen driven as a mouse, a VM's absolute pointer — and for input
/// injected by remote-desktop software, which is how this guest is often
/// driven. Absolute reports are in `0..=65535` across the screen (or the
/// virtual desktop, with `MOUSE_VIRTUAL_DESKTOP`). The difference between two
/// consecutive reports, scaled to pixels, is the motion; the first report after
/// a reset has nothing to be different from and moves nothing.
///
/// What this cannot do is the thing an absolute device cannot do: once the
/// remote pointer reaches the edge of *its* screen it stops reporting motion in
/// that direction, so a view turned by one keeps turning only until then.
#[derive(Debug, Default)]
pub struct AbsoluteMotion {
    last: Option<(i32, i32)>,
}

impl AbsoluteMotion {
    /// The motion since the previous report, in pixels of a `span`-sized
    /// screen.
    pub fn delta(&mut self, x: i32, y: i32, span: (i32, i32)) -> (f64, f64) {
        let previous = self.last.replace((x, y));
        let Some((px, py)) = previous else { return (0.0, 0.0) };
        let scale = |d: i32, span: i32| f64::from(d) * f64::from(span.max(1)) / 65535.0;
        (scale(x - px, span.0), scale(y - py, span.1))
    }

    /// Forget the last report: the next one starts a new stroke.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::Rectangle;

    fn region(rects: &[(RectangleKind, i32, i32, i32, i32)]) -> RegionAttributes {
        RegionAttributes {
            rects: rects
                .iter()
                .map(|&(kind, x, y, w, h)| (kind, Rectangle::new((x, y).into(), (w, h).into())))
                .collect(),
        }
    }

    #[test]
    fn no_region_is_the_whole_surface() {
        assert_eq!(region_bounds((854, 480), None), Some(Rect::new(0, 0, 854, 480)));
    }

    #[test]
    fn a_surface_with_no_size_can_hold_nothing() {
        assert_eq!(region_bounds((0, 480), None), None);
    }

    #[test]
    fn a_region_larger_than_the_surface_is_clipped_to_it() {
        let r = region(&[(RectangleKind::Add, -10, -10, 5000, 5000)]);
        assert_eq!(region_bounds((300, 200), Some(&r)), Some(Rect::new(0, 0, 300, 200)));
    }

    #[test]
    fn two_rectangles_are_bounded_together_and_a_hole_does_not_shrink_them() {
        let r = region(&[
            (RectangleKind::Add, 10, 10, 20, 20),
            (RectangleKind::Add, 100, 50, 10, 10),
            (RectangleKind::Subtract, 0, 0, 1000, 15),
        ]);
        assert_eq!(region_bounds((300, 200), Some(&r)), Some(Rect::new(10, 10, 100, 50)));
    }

    #[test]
    fn a_region_entirely_off_the_surface_holds_nothing() {
        let r = region(&[(RectangleKind::Add, 400, 400, 10, 10)]);
        assert_eq!(region_bounds((300, 200), Some(&r)), None);
        assert_eq!(region_bounds((300, 200), Some(&region(&[]))), None);
    }

    /// The numbers WLCS asks for: a 300x300 surface, the pointer pushed
    /// 600 px past each corner, and the answer is the corner pixel itself.
    #[test]
    fn clamping_stops_on_the_last_pixel_of_each_edge() {
        let r = Rect::new(0, 0, 300, 300);
        assert_eq!(clamp_into(r, 750.0, 750.0), (299.0, 299.0));
        assert_eq!(clamp_into(r, -450.0, -450.0), (0.0, 0.0));
        assert_eq!(clamp_into(r, -450.0, 750.0), (0.0, 299.0));
        assert_eq!(clamp_into(r, 150.5, 20.25), (150.5, 20.25));
    }

    #[test]
    fn clamping_respects_an_offset_rectangle() {
        let r = Rect::new(10, 20, 5, 5);
        assert_eq!(clamp_into(r, 0.0, 100.0), (10.0, 24.0));
    }

    #[test]
    fn the_first_absolute_report_moves_nothing() {
        let mut m = AbsoluteMotion::default();
        assert_eq!(m.delta(30000, 30000, (1920, 1080)), (0.0, 0.0));
    }

    #[test]
    fn absolute_reports_become_pixels_of_the_screen() {
        let mut m = AbsoluteMotion::default();
        m.delta(0, 0, (1920, 1080));
        let (dx, dy) = m.delta(65535, 65535 / 2, (1920, 1080));
        assert!((dx - 1920.0).abs() < 1e-6, "{dx}");
        assert!((dy - 540.0).abs() < 0.1, "{dy}");
        // And backwards.
        let (dx, _) = m.delta(0, 65535 / 2, (1920, 1080));
        assert!((dx + 1920.0).abs() < 1e-6, "{dx}");
    }

    #[test]
    fn a_reset_starts_a_new_stroke() {
        let mut m = AbsoluteMotion::default();
        m.delta(100, 100, (1920, 1080));
        m.reset();
        assert_eq!(m.delta(60000, 60000, (1920, 1080)), (0.0, 0.0));
    }
}
