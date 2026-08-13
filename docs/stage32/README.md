# Stage 32: two programs, two taskbar buttons

Every picture here was taken by `scripts/wc-taskbar.sh` on the guest's real
interactive desktop (`WinSta0\Default`, session 1), through
`PrintWindow(PW_RENDERFULLCONTENT)` on `Shell_TrayWnd` — the instrument stage 8
built and §13.4 argues for. `docs/waycompose-design.md` §32 is the argument;
this is the exhibit list.

The requirement is one sentence: *if two different programs are run on Windows
via Wayland, they must not share a taskbar spot.* Before this stage they all
did, because the shell groups buttons by process and every window here is made
by one process.

## The pair that is the stage

| file | the claim it is evidence for |
|---|---|
| `stage32-taskbar-two-applications.png` | **The deliverable.** `weston-terminal` and `weston-flower` — two different programs, unmodified, running on the Linux host through wc-proxy and a hypervisor — with **two separate taskbar buttons**, each carrying its own icon resolved from the host's own files (QEMU's from hicolor, weston's own flower). Next to §16's `docs/stage11/stage11-taskbar-icons.png`, which is the icon on a button that every window still shared. |
| `stage32-taskbar-same.png` | **The negative check**, and the one that is easy to break while fixing the other: two windows of the *same* application are **one** button. Same harness, same minute, one `--app-id` changed. |

## The rest

| file | what it is |
|---|---|
| `stage32-taskbar-two.png` | Two windows, two app ids, two buttons — but both windows made by the *same executable* run twice (`wc-shmclient.exe --app-id …`), so the identity is the only variable there is. `--debug-icon` gives each window a different generated icon, which is what tells the two buttons apart in the picture. |
| `stage32-taskbar-late.png` | An app id that arrived **after** the window was already on screen. Both windows started under one identity and one button; the renamed one moved to its own after the button was rebuilt. See §32.6 — no toolkit does this, so `wc-shmclient --late-app-id` had to be taught to. |
| `stage32-window-apps.png` | `weston-flower`'s window, with the flower icon in its own caption. The icon path is §16's and is unchanged; it is here because "each with its own icon and title" is half the claim. |
| `stage32-window-two.png`, `stage32-window-same.png` | One window from each local run, for the same reason. |
| `windows-apps.txt`, `windows-two.txt`, `windows-same.txt`, `windows-late.txt` | `wc-desk windows` for each run: every top-level window on the desktop, whether the shell gives it a taskbar button, and — added for this stage — the `PKEY_AppUserModel_ID` read back off it with `SHGetPropertyStoreForWindow`. The `windows.groups` block at the end is the measurement the photographs illustrate: how many distinct taskbar buttons the desktop's windows come to. |
| `compositor-apps.log` | The compositor's own account of the two-application run, unfiltered. |
| `compositor-two.log`, `compositor-same.log`, `compositor-late.log` | The same for the three local runs, filtered of per-frame lines (each was ~116 KB of them); the filter is quoted at the top of each file and removes nothing about identities, windows or clients. |
| `proxy-apps.log` | wc-proxy on the host: two clients, two app ids resolved to two different sets of icon pixels. |

## What the pictures cannot show

Windows 11 combines taskbar buttons and hides their labels by default, so these
are two *icons* rather than two labelled buttons. The titles are in the window
captions and in `windows-*.txt`. Hover previews, jump lists and pinning are
§32.8, and none of them is photographed — the harness cannot hover or
right-click.
