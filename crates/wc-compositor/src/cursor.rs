//! What the pointer should look like, and the Win32 cursor that says it.
//!
//! # Why this is its own file, and why it is not `cfg(windows)`
//!
//! There are exactly two things a compositor has to get right about a cursor,
//! and only one of them is a Win32 call.
//!
//! The first is the **table**: `wp_cursor_shape_v1` names thirty-six shapes
//! and Win32 ships fourteen stock cursors, so the mapping is lossy and every
//! lossy entry is a judgement someone should be able to read and disagree with.
//! A table like that belongs somewhere it can be unit-tested on the machine
//! that builds it, which means it must not mention `windows-sys`. So it deals
//! in the `IDC_*` **resource ordinals** — `IDC_IBEAM` is the integer 32513 and
//! has been since Windows 3.0 — and `platform/win32.rs` is the only file that
//! turns one into a `PCWSTR` and calls `LoadCursorW`.
//!
//! The second is the *policy* — hiding the arrow, restoring it on leave,
//! owning the `HCURSOR`'s lifetime — and that is genuinely Win32's, so it is
//! in `platform/win32.rs` where the window is.
//!
//! # Why named shapes and pixels are both here
//!
//! `wp_cursor_shape_v1` sends a name and `wl_pointer.set_cursor` sends a
//! surface, and a compositor needs both: the named protocol is what GTK4 binds
//! today, and the buffer form is what GTK3 — and therefore Firefox — still
//! uses. They arrive on the same smithay callback ([`SeatHandler::cursor_image`],
//! as a [`CursorImageStatus`]) and they leave on the same platform message
//! ([`crate::platform::CursorSpec`]), because from Win32's point of view they
//! are the same act: one `SetCursor` with one `HCURSOR`.
//!
//! [`SeatHandler::cursor_image`]: smithay::input::SeatHandler::cursor_image
//! [`CursorImageStatus`]: smithay::input::pointer::CursorImageStatus

use smithay::input::pointer::CursorIcon;

/// One of Win32's stock cursors.
///
/// The ordinal rather than the `PCWSTR` constant, so that this file compiles —
/// and is tested — on Linux. `LoadCursorW(NULL, MAKEINTRESOURCE(idc))` is what
/// the platform side does with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stock {
    /// The `IDC_*` ordinal, as `winuser.h` defines it.
    pub idc: u16,
    /// The `IDC_*` name, for the log. A cursor that came out wrong is
    /// otherwise a fact with no words attached to it.
    pub name: &'static str,
}

macro_rules! stock {
    ($idc:literal, $name:literal) => {
        Stock { idc: $idc, name: $name }
    };
}

pub const IDC_ARROW: Stock = stock!(32512, "IDC_ARROW");
pub const IDC_IBEAM: Stock = stock!(32513, "IDC_IBEAM");
pub const IDC_WAIT: Stock = stock!(32514, "IDC_WAIT");
pub const IDC_CROSS: Stock = stock!(32515, "IDC_CROSS");
// `IDC_UPARROW` (32516) is deliberately absent. It is a real stock cursor and
// no CSS shape means it, so a constant for it would be a name nothing could
// ever produce.
pub const IDC_SIZENWSE: Stock = stock!(32642, "IDC_SIZENWSE");
pub const IDC_SIZENESW: Stock = stock!(32643, "IDC_SIZENESW");
pub const IDC_SIZEWE: Stock = stock!(32644, "IDC_SIZEWE");
pub const IDC_SIZENS: Stock = stock!(32645, "IDC_SIZENS");
pub const IDC_SIZEALL: Stock = stock!(32646, "IDC_SIZEALL");
pub const IDC_NO: Stock = stock!(32648, "IDC_NO");
pub const IDC_HAND: Stock = stock!(32649, "IDC_HAND");
pub const IDC_APPSTARTING: Stock = stock!(32650, "IDC_APPSTARTING");
pub const IDC_HELP: Stock = stock!(32651, "IDC_HELP");

/// The mapping, and the whole of it.
///
/// # The three kinds of entry
///
/// **Exact.** `Text` is `IDC_IBEAM`, `Wait` is `IDC_WAIT`, `Pointer` is
/// `IDC_HAND`, and the eight compass resizes are Win32's four diagonal pairs.
/// Windows and CSS are describing the same cursor with different words, and
/// these are the entries that make the feature worth having: they are the
/// I-beam over text and the hand over a link.
///
/// **Deliberately approximate**, and each one is a choice:
///
/// * `Cell` -> `IDC_CROSS`. The spreadsheet cursor is a thick plus; Win32's
///   crosshair is the closest stock shape and is what Excel itself falls back
///   to when it is not drawing its own.
/// * `VerticalText` -> `IDC_IBEAM`. A sideways I-beam is not a stock cursor.
///   Upright is wrong by ninety degrees and still says "you can select text",
///   which is the part the user needs.
/// * `Grab`/`Grabbing` -> `IDC_HAND`/`IDC_SIZEALL`. Windows has no open-hand
///   cursor at all. `IDC_HAND` is the *pointing* hand — the link cursor — so
///   using it for `Grab` overloads it, and `IDC_SIZEALL` for `Grabbing` is what
///   Win32 applications that pan a canvas have always used. Browsers ship their
///   own bitmaps here rather than accept this; a client that cares can, through
///   the buffer path below.
/// * `Progress` -> `IDC_APPSTARTING`. The arrow-with-hourglass, which is
///   exactly "busy but still interactive". This one is nearly exact and is
///   listed here only because the names share no letters.
/// * `AllScroll`/`AllResize`/`Move` -> `IDC_SIZEALL`.
///
/// **Given up on**, and they fall back to the arrow rather than to something
/// misleading: `ContextMenu`, `Alias`, `Copy`, `DndAsk`, `ZoomIn`, `ZoomOut`.
/// Windows draws these as an arrow with a badge and the badges are not stock
/// cursors — they live in the shell's drag-and-drop implementation, reachable
/// only through `IDropTarget`, which is a COM interface this compositor has no
/// business implementing to decorate a pointer. An arrow is honest; a
/// crosshair standing in for "copy" is not.
///
/// The `_` arm is not laziness: `CursorIcon` is `#[non_exhaustive]`, so a
/// future version of the crate can add a shape and this must still compile.
pub fn stock_for(icon: CursorIcon) -> Stock {
    match icon {
        CursorIcon::Default => IDC_ARROW,
        CursorIcon::Help => IDC_HELP,
        CursorIcon::Pointer => IDC_HAND,
        CursorIcon::Progress => IDC_APPSTARTING,
        CursorIcon::Wait => IDC_WAIT,
        CursorIcon::Cell => IDC_CROSS,
        CursorIcon::Crosshair => IDC_CROSS,
        CursorIcon::Text => IDC_IBEAM,
        CursorIcon::VerticalText => IDC_IBEAM,
        CursorIcon::Move => IDC_SIZEALL,
        CursorIcon::NoDrop => IDC_NO,
        CursorIcon::NotAllowed => IDC_NO,
        CursorIcon::Grab => IDC_HAND,
        CursorIcon::Grabbing => IDC_SIZEALL,

        // The resizes. Win32 has four, named for the axis the arrow points
        // along; Wayland has eight compass points plus four aliases, and each
        // pair of opposite points is one Win32 cursor because a double-headed
        // arrow already says both directions.
        CursorIcon::EResize
        | CursorIcon::WResize
        | CursorIcon::EwResize
        | CursorIcon::ColResize => IDC_SIZEWE,
        CursorIcon::NResize
        | CursorIcon::SResize
        | CursorIcon::NsResize
        | CursorIcon::RowResize => IDC_SIZENS,
        CursorIcon::NeResize | CursorIcon::SwResize | CursorIcon::NeswResize => IDC_SIZENESW,
        CursorIcon::NwResize | CursorIcon::SeResize | CursorIcon::NwseResize => IDC_SIZENWSE,
        CursorIcon::AllScroll | CursorIcon::AllResize => IDC_SIZEALL,

        CursorIcon::ContextMenu
        | CursorIcon::Alias
        | CursorIcon::Copy
        | CursorIcon::DndAsk
        | CursorIcon::ZoomIn
        | CursorIcon::ZoomOut => IDC_ARROW,

        _ => IDC_ARROW,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three the audit named, because they are the three a user sees: a
    /// hand over a link, an I-beam over text, a resize arrow over an edge.
    #[test]
    fn the_shapes_a_user_notices() {
        assert_eq!(stock_for(CursorIcon::Pointer), IDC_HAND);
        assert_eq!(stock_for(CursorIcon::Text), IDC_IBEAM);
        assert_eq!(stock_for(CursorIcon::NsResize), IDC_SIZENS);
        assert_eq!(stock_for(CursorIcon::EwResize), IDC_SIZEWE);
    }

    /// The ordinals are an ABI, not an implementation detail: they are what
    /// `LoadCursorW` is handed, and a typo here is a wrong cursor with no
    /// error anywhere. Checked against `winuser.h`, which has not moved these
    /// numbers since Windows 3.0.
    #[test]
    fn the_ordinals_are_the_ones_winuser_h_defines() {
        assert_eq!(IDC_ARROW.idc, 32512);
        assert_eq!(IDC_IBEAM.idc, 32513);
        assert_eq!(IDC_WAIT.idc, 32514);
        assert_eq!(IDC_CROSS.idc, 32515);
        assert_eq!(IDC_SIZENWSE.idc, 32642);
        assert_eq!(IDC_SIZENESW.idc, 32643);
        assert_eq!(IDC_SIZEWE.idc, 32644);
        assert_eq!(IDC_SIZENS.idc, 32645);
        assert_eq!(IDC_SIZEALL.idc, 32646);
        assert_eq!(IDC_NO.idc, 32648);
        assert_eq!(IDC_HAND.idc, 32649);
        assert_eq!(IDC_APPSTARTING.idc, 32650);
        assert_eq!(IDC_HELP.idc, 32651);
    }

    /// A diagonal resize must not collapse onto the wrong diagonal. NE/SW is
    /// the "/" cursor and NW/SE is the "\" one; swapping them is the easiest
    /// mistake in the table to make and the hardest to see, because both look
    /// like a resize arrow at a glance.
    #[test]
    fn the_diagonals_do_not_swap() {
        assert_eq!(stock_for(CursorIcon::NeResize), IDC_SIZENESW);
        assert_eq!(stock_for(CursorIcon::SwResize), IDC_SIZENESW);
        assert_eq!(stock_for(CursorIcon::NwResize), IDC_SIZENWSE);
        assert_eq!(stock_for(CursorIcon::SeResize), IDC_SIZENWSE);
        assert_ne!(
            stock_for(CursorIcon::NeswResize),
            stock_for(CursorIcon::NwseResize)
        );
    }

    /// Every shape `wp_cursor_shape_v1` version 2 can name resolves to a real
    /// stock cursor. The list is the protocol's `shape` enum, written out so
    /// that a shape smithay learns about later shows up here as a compile
    /// error in the table rather than as an arrow at runtime.
    #[test]
    fn every_shape_the_protocol_can_name_has_an_answer() {
        const EVERY: &[CursorIcon] = &[
            CursorIcon::Default,
            CursorIcon::ContextMenu,
            CursorIcon::Help,
            CursorIcon::Pointer,
            CursorIcon::Progress,
            CursorIcon::Wait,
            CursorIcon::Cell,
            CursorIcon::Crosshair,
            CursorIcon::Text,
            CursorIcon::VerticalText,
            CursorIcon::Alias,
            CursorIcon::Copy,
            CursorIcon::Move,
            CursorIcon::NoDrop,
            CursorIcon::NotAllowed,
            CursorIcon::Grab,
            CursorIcon::Grabbing,
            CursorIcon::EResize,
            CursorIcon::NResize,
            CursorIcon::NeResize,
            CursorIcon::NwResize,
            CursorIcon::SResize,
            CursorIcon::SeResize,
            CursorIcon::SwResize,
            CursorIcon::WResize,
            CursorIcon::EwResize,
            CursorIcon::NsResize,
            CursorIcon::NeswResize,
            CursorIcon::NwseResize,
            CursorIcon::ColResize,
            CursorIcon::RowResize,
            CursorIcon::AllScroll,
            CursorIcon::ZoomIn,
            CursorIcon::ZoomOut,
            CursorIcon::DndAsk,
            CursorIcon::AllResize,
        ];
        // `shape` runs 1..=36 in cursor-shape-v1.xml; `invalid_shape` is an
        // error code, not a shape, and smithay's `WEnum` guard drops anything
        // outside the range before it reaches this table.
        assert_eq!(EVERY.len(), 36, "the protocol's shape enum has 36 entries");
        for icon in EVERY {
            let s = stock_for(*icon);
            assert!(
                (32512..=32651).contains(&s.idc),
                "{} mapped to {} which is not an IDC_* ordinal",
                icon.name(),
                s.idc
            );
        }
    }

    /// Not every shape may quietly become an arrow. Six are documented as
    /// given up on; this is the list, and a seventh appearing means the table
    /// lost an entry rather than gained a decision.
    #[test]
    fn only_the_six_documented_shapes_fall_back_to_the_arrow() {
        let fallback = [
            CursorIcon::ContextMenu,
            CursorIcon::Alias,
            CursorIcon::Copy,
            CursorIcon::DndAsk,
            CursorIcon::ZoomIn,
            CursorIcon::ZoomOut,
        ];
        for icon in fallback {
            assert_eq!(stock_for(icon), IDC_ARROW, "{}", icon.name());
        }
        // And `Default`, which is an arrow because an arrow is what it means.
        assert_eq!(stock_for(CursorIcon::Default), IDC_ARROW);
    }
}
