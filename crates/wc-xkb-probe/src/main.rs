//! Does the `xkbcommon` *crate* link and run as a Windows binary?
//!
//! `scripts/probe-xkb.c` already answered the question for the C library. This
//! answers it for the Rust binding, which is a different question in three
//! ways: the fork's `cfg(unix)` gating has to be right, `#[link(name =
//! "xkbcommon")]` has to resolve against the mingw-cross-built `libxkbcommon.a`
//! through cargo's linker invocation rather than a hand-written `gcc` line, and
//! the `Keysym`/`Keycode` newtypes have to agree with the C ABI.
//!
//! It is a separate crate from `wc-server` on purpose. Stage 1 established that
//! `cargo check` proves nothing about linking (docs/design.md §5.4 item 2); a
//! probe that produces its own `.exe` keeps that lesson enforced by the build
//! graph instead of by memory.
//!
//! Usage: `wc-xkb-probe.exe <keymap-file>`, where the keymap is generated on
//! Linux by `scripts/gen-keymap.sh`. Exit status 0 iff every check passes.

use std::process::ExitCode;
use xkbcommon::xkb;

fn main() -> ExitCode {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: wc-xkb-probe <keymap-file>");
            return ExitCode::from(2);
        }
    };

    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::from(2);
        }
    };

    println!("wc-xkb-probe: the xkbcommon crate as a Windows binary");
    println!("  keymap source: {} bytes", source.len());

    let mut failures = 0usize;
    let mut check = |cond: bool, what: &str| {
        println!("  [{}] {what}", if cond { "PASS" } else { "FAIL" });
        if !cond {
            failures += 1;
        }
    };

    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    // `Context::new` cannot fail in the binding's signature, so the meaningful
    // check is the first thing that uses it.

    let keymap = xkb::Keymap::new_from_string(
        &ctx,
        source,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    );
    check(keymap.is_some(), "Keymap::new_from_string");
    let Some(keymap) = keymap else {
        return ExitCode::from(1);
    };

    println!(
        "  keycode range: {}..{}",
        keymap.min_keycode().raw(),
        keymap.max_keycode().raw()
    );

    let mut state = xkb::State::new(&keymap);

    // evdev KEY_A is 30; XKB keycodes are evdev + 8.
    let key_a = xkb::Keycode::new(38);

    let sym = state.key_get_one_sym(key_a);
    let utf8 = state.key_get_utf8(key_a);
    println!(
        "  keycode 38 -> keysym {:#x} ({}) utf8 {utf8:?}",
        sym.raw(),
        xkb::keysym_get_name(sym)
    );
    check(sym == xkb::Keysym::a, "keycode 38 -> keysym 'a'");
    check(utf8 == "a", "keycode 38 -> UTF-8 \"a\"");

    // Shift is where a keymap that compiled to nothing useful would still have
    // passed the checks above.
    let shift = keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
    check(shift != xkb::MOD_INVALID, "keymap knows the Shift modifier");
    state.update_mask(1 << shift, 0, 0, 0, 0, 0);
    let sym = state.key_get_one_sym(key_a);
    let utf8 = state.key_get_utf8(key_a);
    println!(
        "  keycode 38 + Shift -> keysym {:#x} ({}) utf8 {utf8:?}",
        sym.raw(),
        xkb::keysym_get_name(sym)
    );
    check(sym == xkb::Keysym::A, "keycode 38 + Shift -> keysym 'A'");
    check(utf8 == "A", "keycode 38 + Shift -> UTF-8 \"A\"");

    // What a compositor actually sends over `wl_keyboard.keymap`.
    let emitted = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    println!("  get_as_string: {} bytes", emitted.len());
    check(emitted.len() > 1000, "Keymap::get_as_string round-trips");
    let recompiled = xkb::Keymap::new_from_string(
        &ctx,
        emitted,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    );
    check(recompiled.is_some(), "the emitted string recompiles");

    // Must fail: there is no xkeyboard-config data tree on Windows, and the
    // compiled-in config root deliberately names a path nobody creates. If this
    // ever succeeds under Wine it means a Linux store path leaked in and the
    // result stops being about Windows. On Linux it succeeds, and that is the
    // correct answer there.
    let by_name = xkb::Keymap::new_from_names(
        &ctx,
        "",
        "",
        "us",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    );
    if cfg!(windows) {
        check(
            by_name.is_none(),
            "Keymap::new_from_names fails (no xkeyboard-config on Windows)",
        );
    } else {
        check(
            by_name.is_some(),
            "Keymap::new_from_names succeeds (xkeyboard-config is present on Linux)",
        );
    }

    println!(
        "wc-xkb-probe: {}",
        if failures == 0 {
            "all checks passed"
        } else {
            "FAILURES"
        }
    );
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
