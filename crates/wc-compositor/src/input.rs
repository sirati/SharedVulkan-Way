//! Win32 input in the units Wayland speaks: **Linux evdev keycodes**, evdev
//! button codes, and an XKB keymap chosen on Linux at build time.
//!
//! Deliberately free of Win32 and of smithay. Everything here is arithmetic
//! over integers and `&'static str`, which is what lets the part most likely to
//! be quietly wrong — the keycode table — be tested by `cargo test` on Linux
//! against the very keymaps the Windows binary will ship. Nothing about that
//! test needs a window, a client or Wine.
//!
//! # The translation, and why it is from the **scan code** and not the VK
//!
//! `WM_KEYDOWN` hands over two different descriptions of the same key press:
//! `wParam` is a virtual-key code and `lParam` bits 16-23 are the scan code,
//! with bit 24 saying whether an `E0` prefix preceded it. They are not
//! interchangeable and only one of them can be translated correctly.
//!
//! **A virtual-key code is layout-dependent.** On a German layout the key in
//! the position of the American `Z` reports `VK_Y`; every `VK_OEM_*` means a
//! different physical key on a different layout, and Microsoft's documentation
//! says so outright. **A scan code is positional**: it names where the key is
//! on the board and says nothing about what is printed on it.
//!
//! Wayland's `wl_keyboard.key` is positional too. The protocol carries a
//! keycode and a keymap, and the *client* runs the keymap over the keycode —
//! which is the entire point of shipping a keymap at all. So a compositor that
//! translated VK codes would be applying the layout twice: once in Windows to
//! get the VK, and once in the client's xkb, on a keycode that had already had
//! a layout baked into it. On `us` the two layouts agree and the bug is
//! invisible; on `de` the `Z` and `Y` keys swap and it is not.
//!
//! `WM_CHAR` is not used for the same reason, one layer further on. It is the
//! *result* of Windows' own composition, and a Wayland client does its own with
//! its own keymap and its own dead keys. Feeding both would double-compose.
//! `TranslateMessage` still runs in the pump because other parts of Win32
//! expect it to, and its `WM_CHAR` is simply not handled.
//!
//! # The table is mostly an identity, and that is not a coincidence
//!
//! Linux's evdev keycodes were numbered from the IBM PC/XT scan codes, so for
//! the whole of the base set — `0x01` Escape through `0x58` F12 — **the scan
//! code and the evdev keycode are the same number**. `KEY_A` is 30 and the `A`
//! key's scan code is `0x1E`; `KEY_SPACE` is 57 and space is `0x39`; `KEY_F11`
//! is 87 and F11 is `0x57`. That is checked in the tests against a handful of
//! keys spread across the range rather than asserted here, because "obviously
//! an identity" is exactly the kind of claim that is wrong at one end.
//!
//! What is *not* an identity is everything behind an `E0` prefix — the arrow
//! cluster, the right-hand modifiers, the keypad's Enter and slash, the Windows
//! keys and the media keys — because those were added to the scan-code set
//! after the numbering ran out and were added to evdev in a different order.
//! Those are a table, and it is the table Linux's own `atkbd` driver carries.
//!
//! # Why this compiles on Linux, where nothing calls it
//!
//! The scan-code half is called only from `platform/win32.rs`, so a Linux build
//! sees it as dead code and says so. It is not dead: it is exercised by this
//! file's own tests on every platform, which is the entire reason it lives here
//! rather than inside `win32.rs`. **A table that only compiles on the platform
//! that cannot run the tests is a table nobody checks** — and the tests are the
//! only thing standing between a wrong number and a user whose arrow keys type
//! digits.
#![allow(dead_code)]

/// Linux evdev keycodes, from `include/uapi/linux/input-event-codes.h`.
///
/// Only the ones this file names. Spelled out rather than computed so that a
/// reader can check them against the kernel header without running anything.
pub mod evdev {
    pub const KEY_ESC: u16 = 1;
    pub const KEY_A: u16 = 30;
    pub const KEY_Z: u16 = 44;
    pub const KEY_SPACE: u16 = 57;
    pub const KEY_F1: u16 = 59;
    pub const KEY_F10: u16 = 68;
    pub const KEY_NUMLOCK: u16 = 69;
    pub const KEY_SCROLLLOCK: u16 = 70;
    pub const KEY_KPDOT: u16 = 83;
    pub const KEY_102ND: u16 = 86;
    pub const KEY_F11: u16 = 87;
    pub const KEY_F12: u16 = 88;
    pub const KEY_KPENTER: u16 = 96;
    pub const KEY_RIGHTCTRL: u16 = 97;
    pub const KEY_KPSLASH: u16 = 98;
    pub const KEY_SYSRQ: u16 = 99;
    pub const KEY_RIGHTALT: u16 = 100;
    pub const KEY_HOME: u16 = 102;
    pub const KEY_UP: u16 = 103;
    pub const KEY_PAUSE: u16 = 119;
    pub const KEY_LEFTMETA: u16 = 125;
    pub const KEY_COMPOSE: u16 = 127;

    /// `wl_pointer.button` codes. Not keycodes, but the same header and the
    /// same reason for being here: the protocol says the button argument is a
    /// Linux evdev button code and every compositor sends these numbers.
    pub const BTN_LEFT: u32 = 0x110;
    pub const BTN_RIGHT: u32 = 0x111;
    pub const BTN_MIDDLE: u32 = 0x112;
    pub const BTN_SIDE: u32 = 0x113;
    pub const BTN_EXTRA: u32 = 0x114;
}

/// Virtual-key codes used only to *disambiguate* — never to translate.
///
/// Three keys report a scan code that is genuinely ambiguous on Windows, and
/// for those the VK is the only thing that can tell them apart. See
/// [`evdev_keycode`].
mod vk {
    pub const PAUSE: u16 = 0x13;
    pub const SNAPSHOT: u16 = 0x2C;
    pub const NUMLOCK: u16 = 0x90;
}

/// The `E0`-prefixed scan codes, as `(scan code low byte, evdev keycode)`.
///
/// This is `atkbd`'s `atkbd_set2_keycode` table for the `E0` half, which is
/// where the authority for these numbers is: they are not derivable, they are a
/// convention two decades old, and copying the kernel's own list is the only
/// way to be sure a `Home` key produces `KEY_HOME` and not `KEY_KP7`.
///
/// Sorted by scan code so it reads as a table rather than as a history of what
/// somebody remembered to add.
const E0: &[(u8, u16)] = &[
    (0x10, 165), // KEY_PREVIOUSSONG
    (0x19, 163), // KEY_NEXTSONG
    (0x1C, 96),  // KEY_KPENTER
    (0x1D, 97),  // KEY_RIGHTCTRL
    (0x20, 113), // KEY_MUTE
    (0x21, 140), // KEY_CALC
    (0x22, 164), // KEY_PLAYPAUSE
    (0x24, 166), // KEY_STOPCD
    (0x2E, 114), // KEY_VOLUMEDOWN
    (0x30, 115), // KEY_VOLUMEUP
    (0x32, 172), // KEY_HOMEPAGE
    (0x35, 98),  // KEY_KPSLASH
    (0x37, 99),  // KEY_SYSRQ — PrintScreen arrives as E0 2A E0 37
    (0x38, 100), // KEY_RIGHTALT
    (0x47, 102), // KEY_HOME
    (0x48, 103), // KEY_UP
    (0x49, 104), // KEY_PAGEUP
    (0x4B, 105), // KEY_LEFT
    (0x4D, 106), // KEY_RIGHT
    (0x4F, 107), // KEY_END
    (0x50, 108), // KEY_DOWN
    (0x51, 109), // KEY_PAGEDOWN
    (0x52, 110), // KEY_INSERT
    (0x53, 111), // KEY_DELETE
    (0x5B, 125), // KEY_LEFTMETA
    (0x5C, 126), // KEY_RIGHTMETA
    (0x5D, 127), // KEY_COMPOSE — the "menu" key
    (0x5E, 116), // KEY_POWER
    (0x5F, 142), // KEY_SLEEP
    (0x63, 143), // KEY_WAKEUP
    (0x65, 217), // KEY_SEARCH
    (0x66, 156), // KEY_BOOKMARKS
    (0x67, 173), // KEY_REFRESH
    (0x68, 128), // KEY_STOP
    (0x69, 159), // KEY_FORWARD
    (0x6A, 158), // KEY_BACK
    (0x6B, 157), // KEY_COMPUTER
    (0x6C, 155), // KEY_MAIL
    (0x6D, 226), // KEY_MEDIA
];

/// The unprefixed scan codes that are **not** the identity.
///
/// Everything from `0x01` to `0x58` is (see the module header). Beyond it the
/// only unprefixed codes a PC keyboard produces are the Japanese ones, and
/// those were numbered independently in both schemes.
const PLAIN_EXCEPTIONS: &[(u8, u16)] = &[
    (0x70, 93),  // KEY_KATAKANAHIRAGANA
    (0x73, 89),  // KEY_RO
    (0x77, 91),  // KEY_HIRAGANA
    (0x78, 90),  // KEY_KATAKANA
    (0x79, 92),  // KEY_HENKAN
    (0x7B, 94),  // KEY_MUHENKAN
    (0x7D, 124), // KEY_YEN
];

/// Turn one `WM_KEYDOWN`/`WM_KEYUP` into a Linux evdev keycode.
///
/// `scan` is `lParam` bits 16-23, `extended` is bit 24, `vk` is `wParam`.
/// `None` means "no evdev key corresponds to this", which is a thing that
/// happens — Fn combinations and vendor keys reach here with scan codes no
/// keyboard driver on Linux has a name for — and dropping the event is right,
/// because inventing a keycode would make the client's xkb produce a keysym for
/// a key that does not exist.
///
/// # The three ambiguities, and why the VK settles them
///
/// * **Pause and NumLock both claim scan code `0x45`.** The hardware
///   distinguishes them with an `E1` prefix that Windows swallows before the
///   message loop ever sees it, and the extended bit is set for one of them on
///   some drivers and the other on others. Measured behaviour differs between
///   real Windows and Wine, which is precisely why this does not depend on it.
/// * **PrintScreen** sends `E0 2A E0 37`; the first half arrives as a phantom
///   shift press that would otherwise be forwarded as a real one.
///
/// So those three are decided by `wParam` — which is unambiguous for them
/// exactly because they are not keys any layout relabels.
pub fn evdev_keycode(scan: u8, extended: bool, vk: u16) -> Option<u16> {
    match vk {
        vk::PAUSE => return Some(evdev::KEY_PAUSE),
        vk::NUMLOCK => return Some(evdev::KEY_NUMLOCK),
        vk::SNAPSHOT => return Some(evdev::KEY_SYSRQ),
        _ => {}
    }
    if extended {
        // The phantom shift of `E0 2A` / `E0 AA`, which brackets PrintScreen
        // and the arrow keys on some drivers. Forwarding it would give the
        // client a Shift the user never pressed, which changes what every
        // following key produces — the worst kind of input bug, because it
        // makes correct keycodes produce wrong characters.
        if scan == 0x2A || scan == 0x36 {
            return None;
        }
        return E0.iter().find(|(s, _)| *s == scan).map(|(_, k)| *k);
    }
    if (0x01..=0x58).contains(&scan) {
        // The identity. See the module header for why this is a fact about
        // history rather than an assumption.
        return Some(u16::from(scan));
    }
    PLAIN_EXCEPTIONS.iter().find(|(s, _)| *s == scan).map(|(_, k)| *k)
}

// ---------------------------------------------------------------------------
// Keymaps
// ---------------------------------------------------------------------------

/// One layout this binary carries.
pub struct Layout {
    /// The RMLVO layout name it was generated from.
    pub name: &'static str,
    /// `XKB_KEYMAP_FORMAT_TEXT_V1`, resolved on Linux by
    /// `scripts/gen-keymap.sh` and embedded here.
    pub keymap: &'static str,
}

/// Every layout compiled into this binary.
///
/// # Why the set is fixed at build time, and what that costs
///
/// `xkb_keymap_new_from_names` resolves RMLVO names against the
/// xkeyboard-config data tree — ~2,000 files of X11 configuration that does not
/// exist on Windows and that this project deliberately does not ship
/// (docs/design.md §7.2). Only `xkb_keymap_new_from_string` works there, so the
/// names have to be resolved on a machine that has the tree, which is the build
/// machine. Weston's RDP backend does the same thing for the same reason.
///
/// The cost is exactly this list: a user whose Windows layout is not in it gets
/// the fallback, and their keys will be in the right *places* and produce the
/// wrong *characters*. Adding one is `scripts/gen-keymap.sh --all` plus a line
/// here, and nothing else.
pub const LAYOUTS: &[Layout] = &[
    Layout { name: "us", keymap: include_str!("../keymaps/us.xkb") },
    Layout { name: "gb", keymap: include_str!("../keymaps/gb.xkb") },
    Layout { name: "de", keymap: include_str!("../keymaps/de.xkb") },
    Layout { name: "fr", keymap: include_str!("../keymaps/fr.xkb") },
];

/// What a keyboard with no recognised layout gets.
pub const FALLBACK: &str = "us";

pub fn layout(name: &str) -> Option<&'static Layout> {
    LAYOUTS.iter().find(|l| l.name == name)
}

/// Which of [`LAYOUTS`] a Windows keyboard layout identifier asks for.
///
/// The low word of an `HKL` is a `LANGID`: the low ten bits are the primary
/// language and the rest is the sublanguage. This is FreeRDP's
/// `xkb_layout_ids.c` in miniature — the same mapping, cut down to the layouts
/// that are actually compiled in, because a table naming a hundred layouts none
/// of which shipped would be a table that lies.
///
/// Matched on the whole `LANGID` first and then on the primary language, so
/// that `en-GB` finds `gb` and `en-AU` still finds `us` rather than nothing.
pub fn layout_for_langid(langid: u16) -> &'static str {
    const EXACT: &[(u16, &str)] = &[
        (0x0809, "gb"), // English (United Kingdom)
        (0x0c0c, "fr"), // French (Canada) — the layout is French, not US
    ];
    const PRIMARY: &[(u16, &str)] = &[
        (0x09, "us"), // English
        (0x07, "de"), // German
        (0x0c, "fr"), // French
    ];
    if let Some((_, l)) = EXACT.iter().find(|(id, _)| *id == langid) {
        return l;
    }
    PRIMARY
        .iter()
        .find(|(id, _)| *id == langid & 0x03ff)
        .map(|(_, l)| *l)
        .unwrap_or(FALLBACK)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity claim, sampled across the whole range rather than asserted
    /// in a comment. Each of these is a key whose evdev number a reader can
    /// check against `input-event-codes.h` in one line.
    #[test]
    fn the_base_scan_codes_are_their_own_evdev_keycodes() {
        for (scan, key, what) in [
            (0x01u8, evdev::KEY_ESC, "escape"),
            (0x1E, evdev::KEY_A, "a"),
            (0x2C, evdev::KEY_Z, "z"),
            (0x39, evdev::KEY_SPACE, "space"),
            (0x3B, evdev::KEY_F1, "f1"),
            (0x44, evdev::KEY_F10, "f10"),
            (0x46, evdev::KEY_SCROLLLOCK, "scroll lock"),
            (0x53, evdev::KEY_KPDOT, "keypad dot"),
            (0x56, evdev::KEY_102ND, "the ISO 102nd key"),
            (0x57, evdev::KEY_F11, "f11"),
            (0x58, evdev::KEY_F12, "f12"),
        ] {
            assert_eq!(evdev_keycode(scan, false, 0), Some(key), "{what}");
        }
    }

    /// The half that is *not* an identity, and the half a compositor that
    /// assumed one would get wrong in a way the user notices immediately: the
    /// arrow keys would come out as the numeric keypad.
    #[test]
    fn extended_scan_codes_come_from_the_table() {
        assert_eq!(evdev_keycode(0x48, true, 0), Some(evdev::KEY_UP));
        assert_eq!(evdev_keycode(0x47, true, 0), Some(evdev::KEY_HOME));
        assert_eq!(evdev_keycode(0x1C, true, 0), Some(evdev::KEY_KPENTER));
        assert_eq!(evdev_keycode(0x1D, true, 0), Some(evdev::KEY_RIGHTCTRL));
        assert_eq!(evdev_keycode(0x35, true, 0), Some(evdev::KEY_KPSLASH));
        assert_eq!(evdev_keycode(0x38, true, 0), Some(evdev::KEY_RIGHTALT));
        assert_eq!(evdev_keycode(0x5B, true, 0), Some(evdev::KEY_LEFTMETA));
        assert_eq!(evdev_keycode(0x5D, true, 0), Some(evdev::KEY_COMPOSE));

        // Without the E0 these are entirely different keys, which is the whole
        // reason the flag is carried rather than dropped.
        assert_eq!(evdev_keycode(0x48, false, 0), Some(72)); // KEY_KP8
        assert_eq!(evdev_keycode(0x1C, false, 0), Some(28)); // KEY_ENTER
        assert_eq!(evdev_keycode(0x1D, false, 0), Some(29)); // KEY_LEFTCTRL
    }

    #[test]
    fn the_three_ambiguous_keys_are_settled_by_the_virtual_key() {
        // Both claim 0x45, with the extended bit going either way depending on
        // the driver. The VK is what decides, so all four combinations agree.
        for ext in [false, true] {
            assert_eq!(evdev_keycode(0x45, ext, vk::PAUSE), Some(evdev::KEY_PAUSE));
            assert_eq!(evdev_keycode(0x45, ext, vk::NUMLOCK), Some(evdev::KEY_NUMLOCK));
        }
        assert_eq!(evdev_keycode(0x37, true, vk::SNAPSHOT), Some(evdev::KEY_SYSRQ));
    }

    /// The phantom shift. A test rather than a comment because forwarding it is
    /// silent: every key after it produces the shifted character and the
    /// keycodes all look right.
    #[test]
    fn the_phantom_shift_of_printscreen_is_dropped() {
        assert_eq!(evdev_keycode(0x2A, true, 0), None);
        assert_eq!(evdev_keycode(0x36, true, 0), None);
        // The real shifts, which are not extended, still come through.
        assert_eq!(evdev_keycode(0x2A, false, 0), Some(42)); // KEY_LEFTSHIFT
        assert_eq!(evdev_keycode(0x36, false, 0), Some(54)); // KEY_RIGHTSHIFT
    }

    #[test]
    fn an_unknown_scan_code_is_none_rather_than_a_guess() {
        assert_eq!(evdev_keycode(0x00, false, 0), None);
        assert_eq!(evdev_keycode(0x5A, false, 0), None);
        assert_eq!(evdev_keycode(0x01, true, 0), None);
    }

    #[test]
    fn every_langid_lands_on_a_layout_that_is_compiled_in() {
        for (langid, want) in [
            (0x0409u16, "us"), // en-US
            (0x0809, "gb"),    // en-GB
            (0x0c09, "us"),    // en-AU: English, no layout of its own here
            (0x0407, "de"),    // de-DE
            (0x0c07, "de"),    // de-AT
            (0x040c, "fr"),    // fr-FR
            (0x0c0c, "fr"),    // fr-CA
            (0x0411, "us"),    // ja-JP: not shipped, falls back
            (0xffff, "us"),
        ] {
            let got = layout_for_langid(langid);
            assert_eq!(got, want, "langid {langid:#06x}");
            assert!(layout(got).is_some(), "{got} is named but not compiled in");
        }
    }

    /// **The keymaps really are keymaps**, checked by compiling them with the
    /// same library the Windows binary links and asking what a keycode
    /// produces.
    ///
    /// This is the test that would catch the failure that matters: a keymap
    /// file that was truncated, or generated for the wrong layout, or embedded
    /// under the wrong name. All three produce a compositor that starts, sends
    /// a keymap, and gives the user the wrong characters — with no error
    /// anywhere.
    ///
    /// The keycodes are evdev + 8, because XKB's numbering carries X11's
    /// eight-key offset. That offset is the single most common way to get this
    /// wrong and it is asserted rather than assumed: `KEY_A` is 30 and the
    /// keysym for `a` must come from 38.
    #[test]
    fn each_embedded_keymap_compiles_and_maps_the_key_it_should() {
        // Through smithay's own re-export rather than a fresh dependency: it is
        // the same library the compositor will hand this keymap to, and a
        // second copy of `xkbcommon` in the tree could compile a keymap the
        // running one would not.
        use smithay::input::keyboard::xkb;

        // The key in the position of the American `A`, in each layout: `a` on
        // us/gb/de, and `q` on the French azerty. If the `de` keymap were
        // silently the `us` one, only the fr row would fail — so the fr row is
        // the one carrying this test and the others are the control.
        for (name, at_a, at_z) in
            [("us", 'a', 'z'), ("gb", 'a', 'z'), ("de", 'a', 'y'), ("fr", 'q', 'w')]
        {
            let l = layout(name).unwrap_or_else(|| panic!("{name} is not compiled in"));
            let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let keymap = xkb::Keymap::new_from_string(
                &ctx,
                l.keymap.to_owned(),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap_or_else(|| panic!("{name} did not compile"));
            let state = xkb::State::new(&keymap);

            let at = |evdev: u16| -> String {
                state.key_get_utf8(xkb::Keycode::new(u32::from(evdev) + 8))
            };
            assert_eq!(at(evdev::KEY_A), at_a.to_string(), "{name}: the A-position key");
            assert_eq!(at(evdev::KEY_Z), at_z.to_string(), "{name}: the Z-position key");

            // A modifier has to be real for the compiled tables to be real —
            // the same argument `wc-xkb-probe` makes, and the reason this is
            // not merely "it compiled".
            assert_ne!(
                keymap.mod_get_index(xkb::MOD_NAME_SHIFT),
                xkb::MOD_INVALID,
                "{name}: no Shift modifier"
            );
        }
    }
}
