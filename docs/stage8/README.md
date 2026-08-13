# Stage 8: what was looked at

Every file here was produced by `scripts/wc-desktop.sh` on the guest's real
interactive desktop (`WinSta0\Default`, session 1), and every one is attached
below to the single claim it is evidence for. `docs/design.md` §11 is the
argument; this is the exhibit list.

The window captures are `PrintWindow(PW_RENDERFULLCONTENT)`, not screen grabs —
see §11.4 for why that is the right instrument here and what it cannot show.

| file | the claim it is evidence for |
|---|---|
| `stage8-window.png` | §11.5 item 2 and §11.7. `weston-terminal`, on the other side of a hypervisor, in a window with Windows' own caption reading its own title. Also the bug: the client's own decorations are 32 px in from the top-left and clipped at the right and bottom, because `xdg_surface.set_window_geometry` is ignored. |
| `stage8-window-flower.png` | The same, with a second unmodified client, small enough that its whole surface fits and nothing is clipped — which is what makes the clipping in the previous image attributable to size rather than to the blit. |
| `stage8-taskbar-empty.png` | The control for the next one: the guest's taskbar with no compositor running. |
| `stage8-taskbar.png` | §11.5 item 6, and `docs/goal-firefox.md`'s first clause. The same taskbar with the window's button in it, carrying Windows' generic application icon because nothing calls `WM_SETICON`. |
| `stage8-screen.png` | §11.4. The whole screen, which is the lock screen. This is the control that says the window captures above are *not* screen grabs. |
| `tick.txt` | §11.8. `Sleep(1)`, `Sleep(5)`, `Sleep(16)` and a high-resolution waitable timer, 40 samples each. |
| `drag.txt`, `drag-anim.txt` | §11.5 item 4. A window driven through Windows' modal sizing loop from outside, and what its rectangle did. |
| `windows.txt` | Every top-level window on the desktop at the moment of capture, and which of them Windows gives a taskbar button. The compositor's is `WaycomposeToplevel`. |
| `compositor.log`, `compositor-anim.log` | The compositor's own account of the same run, including the DPI report of §11.6 and the per-window `dpi`/`nonclient` line. |

## Reproducing

```
nix develop
WC_HOST=<the VM host> WC_HOST_WESTON=<dir with weston-terminal> \
  scripts/wc-desktop.sh all
```

`WC_CLIENT` and `WC_SUFFIX` pick a different client and a different set of
output names; `weston-presentation-shm` is the one the frame-rate numbers in
§11.8 came from.
