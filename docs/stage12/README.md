# Stage 12: Firefox over the guest's own RAM

`docs/design.md` §17 is the argument; this is the exhibit list. Unlike
`docs/stage9/`, **everything here was produced on the real guest** — the Windows
VM on testhost, with the proxy and the client on the VM host and the pixels
crossing as offsets into memory the guest committed, pinned and stamped. Unlike
`docs/stage11/`, none of it was produced on the *interactive* desktop, which is
§17.10 item 5 and the difference between these two directories.

The run recorded here is from master's checkout after stage 11's merge, so
stage 11's icon feed is live in it: `compositor.log` shows six clients where the
proxy served three, which is §17.2.

| file | the claim it is evidence for |
|---|---|
| `stage12-firefox-guest.png` | The stage. Firefox's tab strip, URL bar and toolbar, composited by `wc-compositor.exe` inside the VM out of a `wl_shm` pool that reached it as `(arena, offset, len)` and nothing else. 1028x749, **456 distinct colours** — §15.5's failure, a transparent toplevel with the browser in a subsurface, gives 2. Also §15.15 item 1 for the fourth time: this is the client's own titlebar, and Windows' caption is above it in the window this was dumped from. |
| `guest-run.txt` | `scripts/wc-guest.sh firefox`, block 11, **16 passed, 0 failed on real Windows** — the transport line, the descriptor counts, the empty export, the connection count, the geometry rectangle and the colour count. |
| `compositor.log` | The compositor's own account of the same run, from inside the guest: the arena it provided, the outbound window it reserved, the keymap it held back until a client arrived (§17.4), the `1028x749` configure its 1024x768 screen forced on a 1280x900 window, `mapped: 0 section, 0 shared-file, 3 arena-slice`, and — stage 11's, over this stage's transport — an icon resolver that bound and found nothing for `"firefox"` on the VM host (§16.7). |

The image is the compositor's `--dump-frames` output, so it is what the
compositor believes it composited — the same instrument as
`docs/stage9/stage9-firefox-menu.png` and the weaker of the two §13.4
distinguishes. **Nobody has looked at this particular window.** Stage 11 has
since put Firefox on the guest's real interactive desktop and photographed it
(`docs/stage11/stage11-window-icons-firefox.png`), so the route is proven; what
has never been on that desktop is a guest-RAM arena, which is §17.10 item 5.

## Reproducing

```
nix develop
export WC_GUESTNAME=wctest12 WC_HOST_SHARE=/srv/winshare/wc12 WC_GUEST_SHARE='Z:\wc12'
export WC_HOST=<the VM host> WC_HOST_DIR=/tmp/wc-stage4-s12 WC_PORT_BASE=57371
export WC_HOST_WESTON=<dir with weston-terminal> WC_HOST_FIREFOX=<path to firefox>
scripts/wc-guest.sh deploy
scripts/wc-guest.sh firefox
```

The names are per-experiment on purpose and §12.8 is why: more than one agent
runs against this guest, and this stage shared it with stage 11 throughout.
`WC_HOST_FIREFOX` is an absolute path because Firefox is not on that machine's
`PATH` and a `firefox` that resolved to something else would be a different
measurement.

The Wine rehearsal, which needs no guest and found every bug in the
multi-connection code before any of them cost a guest run:

```
scripts/wc-firefox.sh wine-arena
```
