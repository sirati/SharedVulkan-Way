# Stage 38: the compositor learns where the screen is

The argument is `docs/waycompose-design.md` §38. These are the artefacts, all
of them from the guest's real interactive desktop — a 1280x800 screen with a
48-pixel taskbar, so a **work area of 1280x752**.

## The menu, which is what the user reported

| | file | menu rectangle, in screen coordinates | inside the work area |
|---|---|---|---|
| before | `stage38-screen-before.png` | 214x236 at 1213, 688 | no |
| after | `stage38-screen-after.png` | 214x236 at 1047, 454 | yes |

Firefox's context menu, opened by a real right-click 40 pixels in from the
bottom right corner of the work area. Before, only the first few pixels of
"Save Page As…" and "Select All" are on the screen and the rest of the menu is
off it. After, the whole menu is there, flipped up and to the left, above the
taskbar. Firefox declares all six constraint adjustments
(`set_constraint_adjustment(63)`, measured with `WAYLAND_DEBUG=1`), so it is
asking to be moved and the compositor is obliged to move it.

`menu-before.txt` is the before run's own numbers, including the parent window
this was clicked in.

## The window, which is the same knowledge applied to the other half

| | file | Firefox's window | inside the work area |
|---|---|---|---|
| before | `stage38-window-before.png`, `spawn-before.txt` | 1296x820 at 26, 26 | no |
| after | `stage38-window-after.png`, `spawn-after.txt` | 960x543 at 104, 104 | yes |

`PrintWindow` on the window's own `HWND`, so this is the real frame at the real
DPI. The before window is wider than the screen it is on — its caption buttons
are cut off at the right edge of the capture — because the compositor was
advertising a 1920x1080 `wl_output` on a 1280x800 machine. The after window is
smaller because **Firefox chose that size itself** against an output that is
now the true work area; nothing clamped it.

## Reproducing

```
nix develop
export WC_DESKNAME=wcdesk38 WC_HOST=testhost WC_HOST_DIR=/tmp/wc-stage38
export WC_PORT_BASE=57471 WC_HOST_SHARE=/srv/winshare/wc38 WC_GUEST_SHARE='Z:\wc38'
export WC_HOST_FIREFOX=<absolute path to firefox on WC_HOST>
scripts/wc-desktop.sh deploy
scripts/wc-popup.sh spawn     # where a window lands; no input, runs on a locked guest
scripts/wc-popup.sh menu      # the menu; needs an unlocked screen
```

The names are per-experiment on purpose (§16.8): more than one agent runs
against this guest, and a fixed directory means one deploy overwrites another
run's binaries in the middle of it.

**`menu` moves a window and clicks on the desktop somebody may be using.**
Take a screen capture before and after and compare them — if a window that is
not ours has moved, the machine is in use and the run should not have happened.
`spawn` needs no input at all and can run against a locked screen, where a
click would reach Winlogon's secure desktop rather than ours.
