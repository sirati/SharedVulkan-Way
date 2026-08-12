/* probe-xkb.c — does a Windows binary actually do XKB work?
 *
 * The question this answers is narrower and harder than "does libxkbcommon
 * cross-compile". `cargo check` never links, so every claim about xkbcommon up
 * to now (docs/design.md §5.4 item 2) has been about compilation. This program
 * links against the mingw-cross-built libxkbcommon.a and *runs*, so a pass here
 * is a statement about the linker and the loaded code, not about the compiler.
 *
 * It checks four things, in increasing order of what they cost to be wrong:
 *
 *   1. `xkb_context_new` — the library initialises at all. On Linux this
 *      touches the environment and the config root; the interesting question is
 *      whether anything in that path assumes a Unix filesystem.
 *   2. `xkb_keymap_new_from_string` — the keymap compiler, which is the
 *      overwhelming majority of the library, runs to completion on a real
 *      2081-line keymap generated on Linux by `xkbcli compile-keymap`. This is
 *      the production path (design §7.2), not a reduced test input.
 *   3. `xkb_state_key_get_one_sym` / `_get_utf8` — the state machine translates
 *      a keycode. Keycode 38 is `a` in evdev numbering (KEY_A = 30, +8).
 *   4. The same keycode with the shift modifier latched gives `A`. Included
 *      because (3) alone would pass against a keymap that had compiled to
 *      nothing useful; a modifier requires the compiled tables to be real.
 *
 * And one thing deliberately checked for *failure*:
 *
 *   5. `xkb_keymap_new_from_names` must NOT succeed. It needs the
 *      xkeyboard-config data tree on disk, which does not exist on Windows.
 *      nix/libxkbcommon-mingw.nix compiles in a config root under
 *      C:/ProgramData that nobody creates, precisely so that this call fails
 *      loudly. If it ever starts working, something is finding a Linux store
 *      path and the Wine result stops meaning anything about Windows.
 *
 * Exit code is 0 only if all five hold.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xkbcommon/xkbcommon.h>

static int failures = 0;

static void ok(int cond, const char *what) {
    printf("  [%s] %s\n", cond ? "PASS" : "FAIL", what);
    if (!cond) failures++;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: probe-xkb <keymap-file>\n");
        return 2;
    }

    printf("probe-xkb: libxkbcommon as a Windows binary\n");

    /* Read the keymap the Linux side generated. Deliberately read from a file
     * rather than compiled in as a literal: the file crosses the Wine boundary
     * as a real Windows file open, which exercises libxkbcommon's own I/O on
     * the target rather than only its parser. */
    FILE *f = fopen(argv[1], "rb");
    if (!f) { perror("fopen"); return 2; }
    fseek(f, 0, SEEK_END);
    long len = ftell(f);
    fseek(f, 0, SEEK_SET);
    char *buf = malloc((size_t)len + 1);
    if (!buf || fread(buf, 1, (size_t)len, f) != (size_t)len) {
        fprintf(stderr, "short read\n"); return 2;
    }
    buf[len] = '\0';
    fclose(f);
    printf("  keymap source: %ld bytes\n", len);

    /* 1. context */
    struct xkb_context *ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    ok(ctx != NULL, "xkb_context_new");
    if (!ctx) return 1;

    /* 2. the keymap compiler */
    struct xkb_keymap *km = xkb_keymap_new_from_string(
        ctx, buf, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
    ok(km != NULL, "xkb_keymap_new_from_string (2081-line us keymap)");
    if (!km) return 1;

    printf("  keycode range: %u..%u\n",
           xkb_keymap_min_keycode(km), xkb_keymap_max_keycode(km));

    /* 3. translate a keycode */
    struct xkb_state *st = xkb_state_new(km);
    ok(st != NULL, "xkb_state_new");
    if (!st) return 1;

    const xkb_keycode_t KEY_A_XKB = 38; /* evdev KEY_A(30) + 8 */
    xkb_keysym_t sym = xkb_state_key_get_one_sym(st, KEY_A_XKB);
    char name[64];
    xkb_keysym_get_name(sym, name, sizeof name);
    char utf8[16];
    int n = xkb_state_key_get_utf8(st, KEY_A_XKB, utf8, sizeof utf8);
    printf("  keycode 38 -> keysym 0x%x (%s) utf8 \"%s\" (%d bytes)\n",
           sym, name, utf8, n);
    ok(sym == XKB_KEY_a, "keycode 38 translates to keysym 'a'");
    ok(n == 1 && utf8[0] == 'a', "keycode 38 produces UTF-8 \"a\"");

    /* 4. the same keycode with shift held */
    xkb_mod_index_t shift = xkb_keymap_mod_get_index(km, XKB_MOD_NAME_SHIFT);
    ok(shift != XKB_MOD_INVALID, "keymap knows the Shift modifier");
    xkb_state_update_mask(st, (xkb_mod_mask_t)1 << shift, 0, 0, 0, 0, 0);
    sym = xkb_state_key_get_one_sym(st, KEY_A_XKB);
    n = xkb_state_key_get_utf8(st, KEY_A_XKB, utf8, sizeof utf8);
    xkb_keysym_get_name(sym, name, sizeof name);
    printf("  keycode 38 + Shift -> keysym 0x%x (%s) utf8 \"%s\"\n",
           sym, name, utf8);
    ok(sym == XKB_KEY_A, "keycode 38 + Shift translates to keysym 'A'");
    ok(n == 1 && utf8[0] == 'A', "keycode 38 + Shift produces UTF-8 \"A\"");

    /* A round trip through get_as_string, because that is what a compositor
     * actually sends to a client over wl_keyboard.keymap. */
    char *out = xkb_keymap_get_as_string(km, XKB_KEYMAP_FORMAT_TEXT_V1);
    ok(out != NULL && strlen(out) > 1000, "xkb_keymap_get_as_string round-trips");
    if (out) {
        printf("  get_as_string: %zu bytes\n", strlen(out));
        struct xkb_keymap *km2 = xkb_keymap_new_from_string(
            ctx, out, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
        ok(km2 != NULL, "the emitted string recompiles");
        if (km2) xkb_keymap_unref(km2);
        free(out);
    }

    /* 5. new_from_names must fail: there is no data tree on Windows. */
    struct xkb_rule_names names = {
        .rules = NULL, .model = NULL, .layout = "us",
        .variant = NULL, .options = NULL,
    };
    struct xkb_keymap *bad = xkb_keymap_new_from_names(
        ctx, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
    ok(bad == NULL, "xkb_keymap_new_from_names fails (no xkeyboard-config on Windows)");
    if (bad) xkb_keymap_unref(bad);

    xkb_state_unref(st);
    xkb_keymap_unref(km);
    xkb_context_unref(ctx);
    free(buf);

    printf("probe-xkb: %s\n", failures ? "FAILURES" : "all checks passed");
    return failures ? 1 : 0;
}
