//! §103.7. The macOS platform's decisions that need no Mac: the key table, the
//! Cmd policy, the keyboard-layout names, the scroll conversion and the cursor
//! names. Pure, so on every target, and tested by the ordinary gate on the
//! machine that builds it, the way `caption.rs` is for Win32. Only `macos.rs`
//! calls it.
//!
//! The key table is written from Apple's `HIToolbox/Events.h` (`kVK_*`) and
//! Linux's `input-event-codes.h`. cocoa-way has one too, but it is GPL-3.0-only
//! and maps winit's `KeyCode`, not `NSEvent.keyCode`, so nothing was taken.

use smithay::input::pointer::CursorIcon;

/// What the Command keys send to a Linux client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CmdKey {
    /// Cmd sends Control, so Cmd+C, Cmd+V, Cmd+T and Cmd+W do in a Linux
    /// application what they do in a Mac one. The default (§103.7).
    #[default]
    Control,
    /// Cmd is Super, which is the literal mapping.
    Super,
}

impl CmdKey {
    pub fn parse(s: &str) -> Option<CmdKey> {
        match s {
            "control" | "ctrl" => Some(CmdKey::Control),
            "super" | "meta" => Some(CmdKey::Super),
            _ => None,
        }
    }
}

pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;

/// `kVK_Command` and `kVK_RightCommand`.
pub const KVK_COMMAND: u16 = 0x37;
pub const KVK_RIGHT_COMMAND: u16 = 0x36;

/// The evdev keycode (no XKB `+8`) for a macOS virtual key code, under a Cmd
/// policy. `None` for keys Linux has no code for (Fn) and for unknown codes.
pub fn evdev_keycode(kvk: u16, cmd: CmdKey) -> Option<u16> {
    match (kvk, cmd) {
        (KVK_COMMAND, CmdKey::Control) => return Some(KEY_LEFTCTRL),
        (KVK_RIGHT_COMMAND, CmdKey::Control) => return Some(KEY_RIGHTCTRL),
        (KVK_COMMAND, CmdKey::Super) => return Some(KEY_LEFTMETA),
        (KVK_RIGHT_COMMAND, CmdKey::Super) => return Some(KEY_RIGHTMETA),
        _ => {}
    }
    let code = match kvk {
        0x00 => 30,  // A
        0x01 => 31,  // S
        0x02 => 32,  // D
        0x03 => 33,  // F
        0x04 => 35,  // H
        0x05 => 34,  // G
        0x06 => 44,  // Z
        0x07 => 45,  // X
        0x08 => 46,  // C
        0x09 => 47,  // V
        0x0A => 86,  // ISO section: KEY_102ND
        0x0B => 48,  // B
        0x0C => 16,  // Q
        0x0D => 17,  // W
        0x0E => 18,  // E
        0x0F => 19,  // R
        0x10 => 21,  // Y
        0x11 => 20,  // T
        0x12 => 2,   // 1
        0x13 => 3,   // 2
        0x14 => 4,   // 3
        0x15 => 5,   // 4
        0x16 => 7,   // 6
        0x17 => 6,   // 5
        0x18 => 13,  // =
        0x19 => 10,  // 9
        0x1A => 8,   // 7
        0x1B => 12,  // -
        0x1C => 9,   // 8
        0x1D => 11,  // 0
        0x1E => 27,  // ]
        0x1F => 24,  // O
        0x20 => 22,  // U
        0x21 => 26,  // [
        0x22 => 23,  // I
        0x23 => 25,  // P
        0x24 => 28,  // Return
        0x25 => 38,  // L
        0x26 => 36,  // J
        0x27 => 40,  // '
        0x28 => 37,  // K
        0x29 => 39,  // ;
        0x2A => 43,  // backslash
        0x2B => 51,  // ,
        0x2C => 53,  // /
        0x2D => 49,  // N
        0x2E => 50,  // M
        0x2F => 52,  // .
        0x30 => 15,  // Tab
        0x31 => 57,  // Space
        0x32 => 41,  // `
        0x33 => 14,  // Delete (backspace)
        0x35 => 1,   // Escape
        0x38 => 42,  // Shift
        0x39 => 58,  // Caps Lock
        0x3A => 56,  // Option: KEY_LEFTALT
        0x3B => 29,  // Control
        0x3C => 54,  // right Shift
        0x3D => 100, // right Option: KEY_RIGHTALT, which is AltGr in PC layouts
        0x3E => 97,  // right Control
        0x40 => 187, // F17
        0x41 => 83,  // keypad .
        0x43 => 55,  // keypad *
        0x45 => 78,  // keypad +
        0x47 => 69,  // keypad Clear: KEY_NUMLOCK, its position
        0x48 => 115, // volume up
        0x49 => 114, // volume down
        0x4A => 113, // mute
        0x4B => 98,  // keypad /
        0x4C => 96,  // keypad Enter
        0x4E => 74,  // keypad -
        0x4F => 188, // F18
        0x50 => 189, // F19
        0x51 => 117, // keypad =
        0x52 => 82,  // keypad 0
        0x53 => 79,  // keypad 1
        0x54 => 80,  // keypad 2
        0x55 => 81,  // keypad 3
        0x56 => 75,  // keypad 4
        0x57 => 76,  // keypad 5
        0x58 => 77,  // keypad 6
        0x59 => 71,  // keypad 7
        0x5A => 190, // F20
        0x5B => 72,  // keypad 8
        0x5C => 73,  // keypad 9
        0x5D => 124, // JIS Yen: KEY_YEN
        0x5E => 89,  // JIS underscore: KEY_RO
        0x5F => 95,  // JIS keypad comma: KEY_KPJPCOMMA
        0x60 => 63,  // F5
        0x61 => 64,  // F6
        0x62 => 65,  // F7
        0x63 => 61,  // F3
        0x64 => 66,  // F8
        0x65 => 67,  // F9
        0x66 => 123, // JIS Eisu: KEY_HANJA (LANG2)
        0x67 => 87,  // F11
        0x68 => 122, // JIS Kana: KEY_HANGEUL (LANG1)
        0x69 => 183, // F13
        0x6A => 186, // F16
        0x6B => 184, // F14
        0x6D => 68,  // F10
        0x6E => 127, // context menu: KEY_COMPOSE
        0x6F => 88,  // F12
        0x71 => 185, // F15
        0x72 => 110, // Help, which sits where Insert does
        0x73 => 102, // Home
        0x74 => 104, // Page Up
        0x75 => 111, // forward Delete
        0x76 => 62,  // F4
        0x77 => 107, // End
        0x78 => 60,  // F2
        0x79 => 109, // Page Down
        0x7A => 59,  // F1
        0x7B => 105, // Left
        0x7C => 106, // Right
        0x7D => 108, // Down
        0x7E => 103, // Up
        _ => return None,
    };
    Some(code)
}

/// The `NSEventModifierFlags` bit that says whether a modifier key is down,
/// for `flagsChanged:`, which reports a modifier's key code but not whether it
/// went up or down. Device-dependent bits (`NX_DEVICEL…KEYMASK`) tell left
/// from right, which the device-independent ones cannot.
pub fn modifier_down(kvk: u16, flags: u64) -> Option<bool> {
    let bit: u64 = match kvk {
        0x38 => 0x0000_0002, // NX_DEVICELSHIFTKEYMASK
        0x3C => 0x0000_0004, // NX_DEVICERSHIFTKEYMASK
        0x3B => 0x0000_0001, // NX_DEVICELCTLKEYMASK
        0x3E => 0x0000_2000, // NX_DEVICERCTLKEYMASK
        0x3A => 0x0000_0020, // NX_DEVICELALTKEYMASK
        0x3D => 0x0000_0040, // NX_DEVICERALTKEYMASK
        KVK_COMMAND => 0x0000_0008,       // NX_DEVICELCMDKEYMASK
        KVK_RIGHT_COMMAND => 0x0000_0010, // NX_DEVICERCMDKEYMASK
        0x39 => 1 << 16,     // NSEventModifierFlagCapsLock
        _ => return None,
    };
    Some(flags & bit != 0)
}

/// Several Mac keys can send one evdev key (Cmd and Control both send Control
/// by default), and a client must see one press and one release, not two
/// presses and a release while a key is still held. This counts.
#[derive(Default, Debug)]
pub struct KeyFold {
    held: std::collections::HashMap<u16, (u32, std::collections::HashSet<u16>)>,
}

impl KeyFold {
    /// `Some(pressed)` if the client should hear about it, for the evdev key
    /// `code` that physical key `kvk` maps to.
    pub fn apply(&mut self, kvk: u16, code: u16, pressed: bool) -> Option<bool> {
        let entry = self.held.entry(code).or_default();
        if pressed {
            if !entry.1.insert(kvk) {
                return None; // already down: an auto-repeat or a duplicate
            }
            entry.0 += 1;
            (entry.0 == 1).then_some(true)
        } else {
            if !entry.1.remove(&kvk) {
                return None; // an orphaned key-up (pressed before focus)
            }
            entry.0 -= 1;
            (entry.0 == 0).then_some(false)
        }
    }

    /// Every evdev key still down, which focus loss releases. Clears the fold.
    pub fn release_all(&mut self) -> Vec<u16> {
        let mut out: Vec<u16> = self.held.drain().filter(|(_, (n, _))| *n > 0).map(|(c, _)| c).collect();
        out.sort_unstable();
        out
    }
}

/// Which compiled-in XKB layout ([`crate::input::LAYOUTS`]) a macOS keyboard
/// input source asks for. `id` is `kTISPropertyInputSourceID`, e.g.
/// `com.apple.keylayout.German`.
pub fn layout_for_input_source(id: &str) -> &'static str {
    let name = id.strip_prefix("com.apple.keylayout.").unwrap_or(id);
    match name {
        "British" | "British-PC" | "Irish" => "gb",
        "German" | "Austrian" | "German-DIN-2137" => "de",
        "French" | "French-PC" | "French-numerical" | "Belgian" => "fr",
        _ => crate::input::FALLBACK,
    }
}

/// One `scrollWheel:` event as `FromUi::PointerAxis`'s numbers:
/// `(horizontal, vertical, v120_h, v120_v)`.
///
/// AppKit's deltas are positive when content should move down or right, i.e.
/// the user scrolled *up*; Wayland's are positive for scrolling down, so the
/// sign flips (natural scrolling is already folded into AppKit's numbers). A
/// mouse wheel (`precise == false`) reports lines, which become `v120` steps
/// and Win32's 10 units per notch; a trackpad reports pixels, which go through
/// as continuous values with no `v120`.
pub fn scroll_axes(precise: bool, dx: f64, dy: f64) -> (f64, f64, i32, i32) {
    if precise {
        (-dx, -dy, 0, 0)
    } else {
        let h120 = (-dx * 120.0).round() as i32;
        let v120 = (-dy * 120.0).round() as i32;
        (f64::from(h120) * 10.0 / 120.0, f64::from(v120) * 10.0 / 120.0, h120, v120)
    }
}

/// The `NSCursor` class method for a cursor-shape name, and whether it is a
/// private one that must be checked with `respondsToSelector:` first (the
/// diagonal resize cursors have no public API before macOS 15).
pub fn nscursor_selector(icon: CursorIcon) -> (&'static str, bool) {
    use CursorIcon as C;
    match icon {
        C::Text | C::VerticalText => ("IBeamCursor", false),
        C::Pointer => ("pointingHandCursor", false),
        C::Grab => ("openHandCursor", false),
        C::Grabbing | C::Move | C::AllScroll => ("closedHandCursor", false),
        C::Crosshair | C::Cell => ("crosshairCursor", false),
        C::NotAllowed | C::NoDrop => ("operationNotAllowedCursor", false),
        C::Copy => ("dragCopyCursor", false),
        C::Alias => ("dragLinkCursor", false),
        C::ContextMenu => ("contextualMenuCursor", false),
        C::EResize | C::WResize | C::EwResize | C::ColResize => ("resizeLeftRightCursor", false),
        C::NResize | C::SResize | C::NsResize | C::RowResize => ("resizeUpDownCursor", false),
        C::NwResize | C::SeResize | C::NwseResize => ("_windowResizeNorthWestSouthEastCursor", true),
        C::NeResize | C::SwResize | C::NeswResize => ("_windowResizeNorthEastSouthWestCursor", true),
        _ => ("arrowCursor", false),
    }
}

/// `FromUi`'s button codes (evdev `BTN_*`) for `NSEvent.buttonNumber`.
pub fn evdev_button(number: i64) -> u32 {
    match number {
        0 => 0x110, // BTN_LEFT
        1 => 0x111, // BTN_RIGHT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x116, // BTN_BACK
        4 => 0x115, // BTN_FORWARD
        n => 0x110 + n.clamp(0, 15) as u32,
    }
}

/// Where a popup goes in AppKit's screen coordinates (origin bottom left, y
/// up), given the parent's content rectangle on screen and the popup's
/// position in the parent's surface coordinates (origin top left, y down).
/// Returns the popup window's frame origin, which is its *bottom* left.
pub fn popup_origin(parent_content: (f64, f64, f64, f64), x: i32, y: i32, height: i32) -> (f64, f64) {
    let (px, py, _pw, ph) = parent_content;
    let top = py + ph - f64::from(y);
    (px + f64::from(x), top - f64::from(height))
}

/// The work area as the parent's own surface coordinates, for `PopupBounds`:
/// `visible` is the screen's `visibleFrame`, `content` the parent's content
/// rectangle, both in AppKit screen coordinates.
pub fn bounds_in_parent(visible: (f64, f64, f64, f64), content: (f64, f64, f64, f64)) -> crate::surface::Rect {
    let (vx, vy, vw, vh) = visible;
    let (cx, cy, _cw, ch) = content;
    let left = vx - cx;
    let top = (cy + ch) - (vy + vh);
    crate::surface::Rect::new(left.round() as i32, top.round() as i32, vw.round() as i32, vh.round() as i32)
}

/// §103.18: the output's integer scale from a screen's `backingScaleFactor`:
/// rounded, at least 1. A Retina screen says 2.0; a fractional factor is
/// rounded, since fractional scale is not built.
pub fn backing_scale(factor: f64) -> i32 {
    if factor.is_finite() { (factor.round() as i32).max(1) } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_the_rows_land_on_their_pc_positions() {
        assert_eq!(evdev_keycode(0x00, CmdKey::Control), Some(30)); // A
        assert_eq!(evdev_keycode(0x0C, CmdKey::Control), Some(16)); // Q
        assert_eq!(evdev_keycode(0x06, CmdKey::Control), Some(44)); // Z
        assert_eq!(evdev_keycode(0x12, CmdKey::Control), Some(2)); // 1
        assert_eq!(evdev_keycode(0x1D, CmdKey::Control), Some(11)); // 0
        assert_eq!(evdev_keycode(0x24, CmdKey::Control), Some(28)); // Return
        assert_eq!(evdev_keycode(0x33, CmdKey::Control), Some(14)); // backspace
        assert_eq!(evdev_keycode(0x75, CmdKey::Control), Some(111)); // forward delete
        assert_eq!(evdev_keycode(0x7E, CmdKey::Control), Some(103)); // Up
    }

    #[test]
    fn the_table_is_injective_apart_from_the_command_keys() {
        let mut seen = std::collections::HashMap::new();
        for kvk in 0u16..0x80 {
            if let Some(code) = evdev_keycode(kvk, CmdKey::Super)
                && let Some(prev) = seen.insert(code, kvk)
            {
                panic!("kVK {prev:#x} and {kvk:#x} both send evdev {code}");
            }
        }
        assert!(seen.len() > 100, "{} keys", seen.len());
    }

    #[test]
    fn cmd_sends_control_by_default_and_super_when_asked() {
        assert_eq!(evdev_keycode(KVK_COMMAND, CmdKey::default()), Some(KEY_LEFTCTRL));
        assert_eq!(evdev_keycode(KVK_RIGHT_COMMAND, CmdKey::default()), Some(KEY_RIGHTCTRL));
        assert_eq!(evdev_keycode(KVK_COMMAND, CmdKey::Super), Some(KEY_LEFTMETA));
        assert_eq!(CmdKey::parse("super"), Some(CmdKey::Super));
        assert_eq!(CmdKey::parse("hyper"), None);
    }

    #[test]
    fn modifier_state_comes_from_the_device_dependent_bits() {
        // Left Shift down, right Shift up.
        assert_eq!(modifier_down(0x38, 0x0002_0002), Some(true));
        assert_eq!(modifier_down(0x3C, 0x0002_0002), Some(false));
        assert_eq!(modifier_down(KVK_COMMAND, 0x0010_0008), Some(true));
        assert_eq!(modifier_down(0x00, 0xffff_ffff), None);
    }

    #[test]
    fn cmd_and_control_held_together_are_one_control_to_the_client() {
        let mut fold = KeyFold::default();
        assert_eq!(fold.apply(KVK_COMMAND, KEY_LEFTCTRL, true), Some(true));
        assert_eq!(fold.apply(0x3B, KEY_LEFTCTRL, true), None);
        assert_eq!(fold.apply(KVK_COMMAND, KEY_LEFTCTRL, false), None, "Control is still held");
        assert_eq!(fold.apply(0x3B, KEY_LEFTCTRL, false), Some(false));
    }

    #[test]
    fn repeats_and_orphaned_releases_are_dropped() {
        let mut fold = KeyFold::default();
        assert_eq!(fold.apply(0x00, 30, true), Some(true));
        assert_eq!(fold.apply(0x00, 30, true), None, "an auto-repeat");
        assert_eq!(fold.apply(0x01, 31, false), None, "released, never pressed");
        assert_eq!(fold.release_all(), vec![30]);
        assert_eq!(fold.apply(0x00, 30, false), None, "focus loss released it already");
    }

    #[test]
    fn input_sources_find_the_compiled_in_layouts() {
        assert_eq!(layout_for_input_source("com.apple.keylayout.US"), "us");
        assert_eq!(layout_for_input_source("com.apple.keylayout.ABC"), "us");
        assert_eq!(layout_for_input_source("com.apple.keylayout.German"), "de");
        assert_eq!(layout_for_input_source("com.apple.keylayout.British-PC"), "gb");
        assert_eq!(layout_for_input_source("com.apple.keylayout.French"), "fr");
        for l in ["us", "gb", "de", "fr"] {
            assert!(crate::input::layout(l).is_some(), "{l} must be compiled in");
        }
    }

    #[test]
    fn a_wheel_notch_is_one_v120_step_and_a_trackpad_is_pixels() {
        // One notch up: AppKit deltaY +1 line, Wayland -120 v120, -10 units.
        assert_eq!(scroll_axes(false, 0.0, 1.0), (0.0, -10.0, 0, -120));
        assert_eq!(scroll_axes(false, -1.0, 0.0), (10.0, 0.0, 120, 0));
        assert_eq!(scroll_axes(true, 0.0, -7.5), (0.0, 7.5, 0, 0));
    }

    #[test]
    fn cursor_names_cover_the_resize_family() {
        assert_eq!(nscursor_selector(CursorIcon::Text), ("IBeamCursor", false));
        assert_eq!(nscursor_selector(CursorIcon::EwResize).0, "resizeLeftRightCursor");
        assert!(nscursor_selector(CursorIcon::NwseResize).1, "private, must be checked");
        assert_eq!(nscursor_selector(CursorIcon::Default).0, "arrowCursor");
    }

    #[test]
    fn buttons_are_evdev_codes() {
        assert_eq!(evdev_button(0), 0x110);
        assert_eq!(evdev_button(1), 0x111);
        assert_eq!(evdev_button(2), 0x112);
    }

    #[test]
    fn a_popup_below_a_menu_item_hangs_down_from_the_parent_s_top() {
        // Parent content 800x600 at screen (100, 200) (bottom left). A 50 px
        // tall popup at surface (10, 30) has its top 30 px below the parent's
        // top edge, which is y = 800, so its bottom is at 800 - 30 - 50.
        assert_eq!(popup_origin((100.0, 200.0, 800.0, 600.0), 10, 30, 50), (110.0, 720.0));
    }

    #[test]
    fn the_work_area_is_said_in_the_parent_s_coordinates() {
        // Screen 1280x800 with a 25 pt menu bar: visibleFrame (0, 0, 1280, 775).
        // Parent content at (100, 100) 600x400: its top is at 500, so the work
        // area's top (775) is 275 above the parent's origin.
        let r = bounds_in_parent((0.0, 0.0, 1280.0, 775.0), (100.0, 100.0, 600.0, 400.0));
        assert_eq!(r, crate::surface::Rect::new(-100, -275, 1280, 775));
    }

    #[test]
    fn the_backing_scale_is_a_whole_number_of_at_least_one() {
        assert_eq!(backing_scale(2.0), 2);
        assert_eq!(backing_scale(1.0), 1);
        assert_eq!(backing_scale(3.0), 3);
        assert_eq!(backing_scale(1.4), 1);
        assert_eq!(backing_scale(1.5), 2);
        assert_eq!(backing_scale(0.0), 1);
        assert_eq!(backing_scale(f64::NAN), 1);
    }
}
