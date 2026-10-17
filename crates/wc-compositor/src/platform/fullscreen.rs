//! A fullscreen window's placement, as numbers.
//!
//! ADDED FOR §96 (docs/waycompose-design.md). `win32.rs`'s `set_fullscreen`
//! makes a window borderless over its monitor. Windows keeps two answers to
//! "where is this window": its rectangle now, and its `WINDOWPLACEMENT`, the
//! normal rectangle and show state that a restore from the taskbar goes back
//! to. GLFW minimises a fullscreen window whenever it loses focus
//! (`GLFW_AUTO_ICONIFY`), so a fullscreen game comes back from the taskbar all
//! the time, and it comes back to the placement. The first version moved only
//! the rectangle. A game made fullscreen from a maximised window came back
//! maximised, a 1296x768 borderless window at -8,-8 with the taskbar showing
//! below it.
//!
//! So while fullscreen, the placement says: normal (not maximised; minimised
//! stays minimised), the monitor's rectangle, and no restore-to-maximised.
//! Everything here is pure and compiled on every target; `win32.rs` reads the
//! placement and the monitor, asks, and writes the answer back.

/// A rectangle as Win32 has them: left, top, right, bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The show state a placement can carry, of the three that matter here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Show {
    Normal,
    Maximized,
    Minimized,
}

/// The normal rectangle for a fullscreen window: the monitor, in the
/// coordinates `rcNormalPosition` is in.
///
/// Those are *workspace* coordinates, whose origin is the work area's corner,
/// unless the window is a tool window (`WS_EX_TOOLWINDOW`), in which case they
/// are screen coordinates. That is the one trap in `SetWindowPlacement`: with
/// the taskbar at the top or on the left, screen coordinates would put the
/// window off by the taskbar's size.
pub(crate) fn normal_rect(monitor: Rect, work: Rect, tool_window: bool) -> Rect {
    let (dx, dy) = if tool_window {
        (0, 0)
    } else {
        (work.left - monitor.left, work.top - monitor.top)
    };
    Rect {
        left: monitor.left - dx,
        top: monitor.top - dy,
        right: monitor.right - dx,
        bottom: monitor.bottom - dy,
    }
}

/// The show state a fullscreen window's placement keeps: never maximised,
/// because a maximised window is restored over the work area, and minimised
/// only if it already is.
pub(crate) fn fullscreen_show(was: Show) -> Show {
    match was {
        Show::Minimized => Show::Minimized,
        Show::Normal | Show::Maximized => Show::Normal,
    }
}

/// The show state to put back on leaving fullscreen: the saved one, except
/// that a window on the taskbar stays there. GLFW can leave fullscreen while
/// minimised, and the window must not pop up behind the user's back.
pub(crate) fn leave_show(saved: Show, minimized_now: bool) -> Show {
    if minimized_now { Show::Minimized } else { saved }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: Rect = Rect { left: 0, top: 0, right: 1280, bottom: 800 };

    #[test]
    fn a_taskbar_at_the_bottom_changes_nothing() {
        // The guest: the taskbar is the bottom 48 px, so the work area starts
        // where the monitor does.
        let work = Rect { left: 0, top: 0, right: 1280, bottom: 752 };
        assert_eq!(normal_rect(MONITOR, work, false), MONITOR);
    }

    #[test]
    fn a_taskbar_at_the_top_or_left_is_subtracted() {
        let work = Rect { left: 0, top: 48, right: 1280, bottom: 800 };
        assert_eq!(
            normal_rect(MONITOR, work, false),
            Rect { left: 0, top: -48, right: 1280, bottom: 752 }
        );
        let work = Rect { left: 60, top: 0, right: 1280, bottom: 800 };
        assert_eq!(
            normal_rect(MONITOR, work, false),
            Rect { left: -60, top: 0, right: 1220, bottom: 800 }
        );
    }

    #[test]
    fn a_tool_window_is_in_screen_coordinates() {
        let work = Rect { left: 0, top: 48, right: 1280, bottom: 800 };
        assert_eq!(normal_rect(MONITOR, work, true), MONITOR);
    }

    #[test]
    fn a_second_monitor_keeps_its_offset() {
        let monitor = Rect { left: 1280, top: 0, right: 3200, bottom: 1080 };
        let work = Rect { left: 1280, top: 0, right: 3200, bottom: 1040 };
        assert_eq!(normal_rect(monitor, work, false), monitor);
    }

    #[test]
    fn fullscreen_is_never_maximised_and_minimised_stays() {
        assert_eq!(fullscreen_show(Show::Maximized), Show::Normal);
        assert_eq!(fullscreen_show(Show::Normal), Show::Normal);
        assert_eq!(fullscreen_show(Show::Minimized), Show::Minimized);
    }

    #[test]
    fn leaving_puts_back_maximised_unless_on_the_taskbar() {
        assert_eq!(leave_show(Show::Maximized, false), Show::Maximized);
        assert_eq!(leave_show(Show::Normal, false), Show::Normal);
        assert_eq!(leave_show(Show::Maximized, true), Show::Minimized);
    }
}
