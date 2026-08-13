# Stage 14: one title bar

`docs/design.md` §19 is the account. These are the pictures it rests on.

The two full captures are the **same client, the same compositor, the same
desktop, the same 64 MiB arena in the guest's own RAM, minutes apart**, and the
only difference is the compositor's `--decorations` flag and whether Firefox was
started with `MOZ_GTK_TITLEBAR_DECORATION=none`. A single picture of a window
with one caption would prove nothing — that is also what a Firefox which failed
to start looks like — so the pair is the evidence.

| file | what it is |
|---|---|
| `stage14-client.png` | the guest's real desktop, `--decorations client`: Windows' caption with its three buttons, and in the tab strip **below** it Firefox's own close button. Two title bars, one on top of the other, both able to close the window. This is what every stage from 9 to 13 shipped. The page it happens to be showing is `about:support`, which is not staged and is a useful accident — the build id, the binary path and the profile directory in it are this run's provenance. |
| `stage14-server.png` | the same, `--decorations server` plus the environment variable. One caption, Windows', with the client's own icon and title in it; Firefox's tabs directly beneath with nothing in the corner but the tab-list chevron. |
| `compositor-server.log` | the `server` leg's compositor log, from inside the VM. `decorations: manager default is Server`, `kde-decoration: client asked for Value(Server)`, `mapped: 0 section, 0 shared-file, N arena-slice`. |
| `compositor-client.log` | the `client` leg's, for the same lines with the other answers. |

The three corner crops are the **same 420x46 rectangle of the client's own
pixels** — the compositor's `--dump-frames` output, not a screen capture — taken
on the workstation, where the theme names all three buttons. They are what
§19.9's table is a reading of.

| file | `gtk-decoration-layout` and mode | what is in the corner |
|---|---|---|
| `corner-client.png` | `icon:minimize,maximize,close`, `default_mode(1)` | chevron, minimise, maximise, close |
| `corner-server.png` | the same layout, `default_mode(2)` + the variable | chevron |
| `corner-menu-only.png` | `menu:`, `default_mode(1)` | chevron, **close** |

The third is the one worth looking at twice. Naming no right-hand buttons at all
takes minimise and maximise away and leaves the close button, and putting
`close` on the *left* does not move it. That is why §19.8's plan for restoring
Windows 11 snap layouts — free the corner, draw real caption buttons there,
hit-test them properly — does not work as stated.
