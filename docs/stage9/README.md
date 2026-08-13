# Stage 9: what Firefox did

`docs/design.md` §15 is the argument; this is the exhibit list. Everything here
was produced by `scripts/wc-firefox.sh` on this workstation — **not on the
guest**, which is the limitation §15.14 item 1 states and the reason stage 8's
exhibits and these are not comparable.

The two window captures are of different things and it matters which is which.
`stage9-firefox-menu.png` is the compositor's own `--dump-frames` output, so it
is what the compositor believes it composited. `stage9-firefox-window.png` is an
`xwd` capture of an X root window taken from outside every process involved, so
it is what a display server actually holds — the stronger instrument, and the
only one available for the Wine leg (§8.5's split, still in force).

| file | the claim it is evidence for |
|---|---|
| `stage9-firefox-window.png` | The stage. Firefox, unmodified, on the Linux host, through `wc-proxy`, in an `HWND` made by `wc-compositor.exe`, with Windows' own caption reading its title. Also the bug: **two title bars**, Windows' above Firefox's own tab strip, which is §15.15 item 1 and §8.11 item 7 for the third time. |
| `stage9-firefox-menu.png` | §15.5 and the popup half of §15.2. A right-click at a named point, an `xdg_popup`, and a second window with a real menu drawn in it — the thing stage 6 recorded as "the click arrives and nothing happens". |
| `binds.txt` | §15.2, the requirement list, as `wl_registry.bind` counted off a `WAYLAND_DEBUG=1` trace. Two connections, so the duplicated rows are GTK's and Gecko's rather than a repeat. |
| `pools.txt` | §15.10 and §15.11. Every `wl_shm` pool one run created, by size, with the number of `wl_shm_pool.destroy` calls in the same trace as a comment. That number is the section. |
| `compositor.log` | The compositor's own account of the same run: the output global, the KDE decoration exchange, the geometry-rectangle map line, and one `present` line per frame with a hash and the centre pixel. |

## Reproducing

```
nix develop
scripts/wc-firefox.sh all        # enumerate, window, menu, wine
WC_KEEP=1 scripts/wc-firefox.sh wine   # and keep the screenshots
```

`enumerate`, `window` and `menu` need only Firefox and the Linux build. `wine`
needs `wine`, `Xvfb` and the mingw target, and is a hypothesis rather than a
result (§15.14 item 2).
