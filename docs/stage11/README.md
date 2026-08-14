# Stage 11: what the taskbar shows

Every picture here was taken by `scripts/wc-icon.sh` on the guest's real
interactive desktop (`WinSta0\Default`, session 1), by the route stage 8 built
and for the same reason: an ssh session is not a desktop and cannot photograph
one (docs/waycompose-design.md §13.2). `docs/design.md` §16 is the argument; this is the
exhibit list.

The window and taskbar captures are `PrintWindow(PW_RENDERFULLCONTENT)`, not
screen grabs — §13.4 for why that is the right instrument and what it cannot
show.

## The pair that is the stage

| file | the claim it is evidence for |
|---|---|
| `stage11-taskbar-icons.png` | **The deliverable.** The guest's taskbar with the compositor's window button on it, carrying the icon that was resolved from the *host's* icon theme and shipped across as pixels. The button is the third from the left of the running group; the icon is QEMU's, which exists on the Linux host and nowhere in Windows. |
| `stage11-taskbar-generic.png` | The control: the same client, the same compositor, the same minute, with `--no-icons` on the proxy. The button is in the same place with Windows' generic application icon on it — stage 8's picture (`docs/stage8/stage8-taskbar.png`) reproduced on today's tree. |
| `stage11-taskbar-before-icons.png`, `stage11-taskbar-before-generic.png` | The same taskbar in each run *before* the client connected: the button is not there at all. Which is what makes the two above a difference of one window rather than of one minute. |
| `stage11-window-icons.png` | The same icon in the window's own caption (`ICON_SMALL`, 16x16), and the measurement: `compare -subimage-search` finds `reference-16-qemu.png` in this capture at +16,+7 with RMSE 595, and in the control below with RMSE 26741. That is the icon claim without a human in it. |
| `stage11-window-generic.png` | The control for the previous one: Windows' generic glyph in the caption. |
| `reference-16-qemu.png` | The host's `hicolor/16x16/apps/qemu.png`, fetched from the machine that resolved it, which is what the search above looks for. |

## Firefox, with no synthetic data anywhere in it

| file | the claim it is evidence for |
|---|---|
| `stage11-taskbar-icons-firefox.png` | The same taskbar, with **Firefox's own icon** on our window's button — third from the left of the running group, next to the pinned Firefox that Windows drew from its own copy. Nothing was written for this run: the app id is `firefox`, the desktop entry is the one in the Firefox package on the host, and the pixels are its `hicolor/32x32/apps/firefox.png`. |
| `stage11-taskbar-before-icons-firefox.png` | The control: the same taskbar seconds earlier, with two buttons instead of three. |
| `stage11-window-icons-firefox.png` | The window, whose caption carries the 16x16 of the same icon. It is Firefox's *"Profile Missing"* dialog, because the harness pointed it at a profile directory that did not exist — which is a fault in the harness's command line and not in anything below it. The window, the caption and the icon are all real. |
| `proxy-icons-firefox.log`, `compositor-icons-firefox.log` | `icon: app id "firefox": icon "firefox" -> 3 image(s) [48x48 32x32 16x16]` on one side and `icon set: 2 of 2 slots` on the other. |

## The logs, which say how it got there

| file | what to read it for |
|---|---|
| `proxy-icons.log` | The resolution, on the Linux side: app id, desktop entry, theme, the three files, and how long it took (3–6 ms). Also `icon: bound wc_icon_manager_v1`, which is the second connection being made. |
| `compositor-icons.log` | The other end: `icon published for …`, then `icon big <- 32x32`, `icon small <- 16x16`, `icon set: 2 of 2 slots`. And `hid this process's own console window`, without which the shell groups the toplevel with the console and the button shows the *executable's* icon (§16.6). |
| `proxy-generic.log`, `compositor-generic.log` | The control's account: `--no-icons, so app ids are forwarded and not resolved`, and `app id "…" has no published icon (the store holds 0)`. |
| `windows-icons.txt`, `windows-generic.txt` | Every top-level window at the moment of capture and which ones Windows gives a taskbar button. Ours is `WaycomposeToplevel`. Firefox is in there too, and it is another worktree's — see §13.9. |

## Reproducing

```
nix develop
WC_HOST=<the VM host> WC_HOST_WESTON=<dir with weston-terminal> \
  scripts/wc-icon.sh all
```

`WC_ICON_NAME` picks the icon the harness's desktop entry names (default
`qemu`; `firefox` for the goal's own). `WC_ONLY=icons|generic` re-takes one half
of the pair. `WC_CLIENT_CMD` replaces the client command line entirely, which is
how Firefox is driven, and `WC_TRANSPORT=shared-file` is needed with it because
the arena serves one connection and Firefox opens two (§12.13 item 6).
