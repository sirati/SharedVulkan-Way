# Tabs in the title bar: Firefox's tabs inside Windows' own caption

`docs/waycompose-design.md` §30 is the account. These are the pictures it rests
on, and they are the *other half* of stage 17's pair: §22 removed the second
title bar by making Windows draw the only one, and this removes it by making
Firefox draw the only one — inside the system frame, with the system's own
caption buttons in the corner of it.

Every capture here is the guest's real interactive desktop, over a 64 MiB arena
in the guest's own RAM, taken with
`wc-desk.exe shot --pid <the compositor> --class WaycomposeToplevel`, which is
`PrintWindow(PW_RENDERFULLCONTENT)` and therefore reads the window's DWM
redirection surface rather than the framebuffer. The black margin around each
window is the **invisible** resize border — eight pixels of window rectangle
that are transparent on screen and come back as zero alpha in a BMP. It is in
`docs/stage17/stage17-server.png` too, for the same reason.

| file | what it is |
|---|---|
| `tabs.png` | the claim. `--decorations tabs`: no Windows caption above the tab strip, because there is no Windows caption — `WM_NCCALCSIZE` gave those 31 pixels to the client area. Firefox's tabs are at the very top of the window, and in the corner of that same strip are **Windows' own** minimise, maximise and close buttons, drawn by DWM and revealed by leaving `alpha = 0` over the rectangle `DWMWA_CAPTION_BUTTON_BOUNDS` reported. |
| `noglass.png` | the same window, same guest, minutes apart, with `--dwm-glass off`. The same `FillRect` that is a hole in the picture above is a **black rectangle** here, because there is no extended frame region for anything to show through. §30.6 is that measurement; this is what it looks like. |
| `nobuttons.png` | the same window with `--caption-buttons off`: the client's pixels over the whole caption and no buttons at all. What the arrangement is worth without `DwmDefWindowProc`. It is also the picture of the *other* problem: with nothing reserving the corner, Firefox's tab-list chevron runs under where the buttons would be. §30.9. |
| `server.png` | **the pair.** §22's arrangement, re-run in this session on the same guest with the same arena and the same output size, minutes apart, one flag apart. Windows' caption with Firefox's icon and title in it, and Firefox's tab strip below. `docs/stage17/stage17-server.png` is stage 17's own capture of the same thing and is left untouched. |
| `pair-topstrip.png` | the top 100 rows of both, stacked: the caption-above-tabs arrangement over the tabs-in-caption one. It is the whole of this stage in one picture, and it is the only form the claim can take — the two windows differ by about thirty pixels of vertical offset, which is also what a client that started at a different size looks like. |
| `corner-three.png` | the same 210x50 rectangle of the same corner from all three, stacked and magnified three times. Top: Windows' minimise, maximise and close, through the hole. Middle: the same hole with `--dwm-glass off`, which is black. Bottom: no hole at all, and what is in the corner is Firefox's own tab-list chevron and its own close button. Three flags, one picture. |
| `compositor-*.log` | each leg's compositor log, from inside the VM. The two lines that make the picture believable are `folds 31 px of caption into its client area, DwmExtendFrameIntoClientArea ok` and `keeps Windows' own caption buttons: client rect …`, plus `nonclient 16x8` — eight pixels of border on each side, eight at the bottom, and **zero at the top**. |

The comparison is `docs/stage17/stage17-server.png` and
`docs/stage17/stage17-client.png`, and the three together are the whole goal
clause:

| picture | title bars | who draws the caption | who draws the buttons |
|---|---|---|---|
| `stage17/stage17-client.png` | **two** | Windows, and Firefox under it | both, twice |
| `stage17/stage17-server.png` | one | Windows | Windows |
| `tabs-in-titlebar/tabs.png` | one | **Firefox, inside Windows' frame** | Windows |

## What the pictures do not show

* **The snap-layouts flyout.** It follows from `DwmDefWindowProc` returning
  `HTMAXBUTTON` over the button it drew, and nobody has hovered it and taken a
  capture — `wc-desk.exe shot` photographs a *window*, and the flyout is a
  separate one. §30.12 item 1.
* **A window being dragged by its tab strip.** §22.11 item 1, inherited whole.
* **Firefox knowing the corner is taken.** It does not, and no Wayland protocol
  can tell it — §30.9 is the account. What the harness does instead is give the
  client `gtk-decoration-layout=:minimize,maximize,close`, so Firefox lays out
  three buttons of its own there and stops its tab strip before them; the
  transparent rectangle then covers three buttons nobody needs rather than the
  tab-list chevron. That is a **coincidence of widths, not an agreement**, and
  it is the residue: the compositor knows the exact rectangle and cannot say so.
* **Anything but 100% scaling and this one GTK theme.** Both the corner
  reservation above and the caption colour in §30.6 are measured at 96 DPI on
  one theme.

## What is not in a picture and is in the logs

The caption colour (§30.6) is the part of this that a still photograph
under-sells, because it is a *following* behaviour. `compositor-tabs.log` has
the line:

```
window 1 caption colour follows the client: #f9f9fb (luma 249, dark glyphs)
```

DWM paints the strip behind its caption buttons in whatever colour Firefox is
painting beside it, re-sampled every frame and pushed through
`DWMWA_CAPTION_COLOR` only when it changes — so it follows the client into dark
mode or a different theme with no protocol and no configuration. The direction
is the point: the request was "tell Firefox what colour Windows' title bar is",
which is impossible, and the answer is "tell Windows what colour Firefox's is",
which needs no channel because both are already in one address space.

## Reproducing

```
nix develop
export WC_HOST=<the VM host> WC_HOST_FIREFOX=<absolute path to firefox there>
export WC_TBNAME=wctabs WC_PORT=57411 WC_OUTPUT=1100x700
export WC_HOST_DIR=/tmp/wctabs WC_HOST_SHARE=/srv/winshare/wctabs
export WC_GUEST_SHARE='Z:\wctabs'
cargo build --workspace
cargo build --workspace --target x86_64-pc-windows-gnu
cargo build --manifest-path crates/wc-arena-host/Cargo.toml
scripts/wc-titlebar.sh deploy
scripts/wc-titlebar.sh tabs
```

The names are per-experiment on purpose and §12.8 is why: more than one agent
runs against this guest. `WC_HOST_FIREFOX` is an absolute path because Firefox
is not on that machine's `PATH`.

**The leg needs a logged-on interactive session on the guest**, and this bites
every harness in this repository that reaches `WinSta0\Default` —
`wc-desktop.sh`, `wc-guest.sh` and `wc-icon.sh` as well as this one. The
compositor runs as a scheduled task with an `Interactive` principal, and with
nobody logged on `Start-ScheduledTask` **returns success while the task never
runs**. The tells are `schtasks /Query /TN <name> /V` reporting
`Letztes Ergebnis: 267011` — `SCHED_S_TASK_HAS_NOT_RUN`, an `S_`-prefixed
*success* code — and no `explorer.exe` in the process list. It cost this stage
two runs and a session of staring at a binary that ran perfectly by hand.
`scripts/wc-titlebar.sh` now makes that check itself and says which of the two
causes it is; the other three do not, and that is §30.13 item 5. The check is
one line:

```
ssh <guest> 'powershell.exe -NoProfile -Command "(Get-Process explorer).Count"'
```

The Linux half, which needs no guest and asserts the part of the arrangement
that is a fact about the client (§30.2's shadow inset, and that the client area
is the surface):

```
scripts/wc-firefox.sh tabs
```
