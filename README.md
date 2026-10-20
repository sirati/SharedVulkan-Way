# waycompose

A Wayland compositor that runs on Windows. Phase 2 of the shared_vulkan project.

**Stage 1 is done: the Wayland server stack builds and runs as a Windows
binary.** It composites nothing yet — that was not the question. The question
was whether it could exist at all, and it can.

**Stage 2 is done: smithay compiles for `x86_64-pc-windows-gnu`, and a
mingw-built smithay compositor lets a real C client create an `xdg_toplevel`.**
`libxkbcommon` cross-builds for mingw and a Windows binary translates keycodes
with it — that was the prerequisite, and it needed one upstream patch. Still no
pixels; that is stage 3.

**Stage 6 is done: you can type into it.** `wc-compositor.exe` advertises a
`wl_seat` with a pointer and a keyboard, turns Win32 scan codes into Linux evdev
keycodes, and gets a keymap across the boundary — which is `wl_keyboard.keymap`'s
`h` argument going the *other* way from a `wl_shm` pool, and is the direction
stage 4 implemented without ever exercising. `weston-terminal`, unmodified, in a
window with Windows' own caption, with text typed into it by `SendInput`:
`docs/stage6-typing.png` (a screenshot, kept outside the repository). Under Wine — stage 5 put the
earlier stages on the real guest, and no input has run there yet.

**Two known defects of the keyboard, stated here because both are silent**
(`docs/design.md` §11.4):

* **A layout outside `us`/`gb`/`de`/`fr` produces American letters.** The key
  *positions* are right, so the keyboard feels like it works and the characters
  are not the ones printed on the keys. Nothing errors. Adding a layout is three
  lines.
* **There is no IME, so CJK input is impossible** — not degraded, impossible.
  `WM_IME_*` is not handled and `text-input-v3` is not implemented, so there is
  nowhere for a composed string to go.

Read [`docs/design.md`](docs/design.md) first. It decides things and says why —
§7 is stage 2, §11 is stage 6, §12 is stage 7.
[`vendor/PATCHES.md`](vendor/PATCHES.md) is the inventory of what was
changed in the forked upstream crates.

## Getting a shell

```
nix develop
```

Pinned stable Rust with the `x86_64-pc-windows-gnu` target, the mingw cross
compiler, Wine (staging — see `docs/design.md` §4.4 for why it must be staging),
and `wayland-info` for testing against a client that has never heard of this
project. `flake.lock` is copied from `shared-vulkan-base`, so every store path is
already realised and the shell is instant.

**Never use the `cargo` on `PATH`.** Everything below assumes you are inside
`nix develop`.

## Building and checking

```
cargo build                                        # Linux
cargo build --target x86_64-pc-windows-gnu         # the target that matters
cargo test --workspace
cargo clippy --workspace --all-targets
cargo check --workspace --target x86_64-pc-windows-gnu
# macOS (§103.9): the check needs nothing else; the build links with the SDK
# copied out of the macOS VM once (scripts/macos-sdk.sh).
cargo check --target x86_64-apple-darwin -p wc-compositor -p sv-guest -p wc-launch -p sv-gpu-server
cargo build --target x86_64-apple-darwin -p wc-launch -p sv-guest
```

## Proving it works

```
scripts/wc-handshake.sh all
```

Six checks. Three run everything as Linux binaries; three run the server as a
mingw-built `.exe` under Wine, including one where a **native Linux
`wayland-info`** — libwayland's own C client — enumerates the globals of the
Windows binary, which is possible because Wine backs a Windows AF_UNIX socket
with a real host socket node.

The one to look at is the third Wine check: an `h` argument crossing a socket
that cannot carry file descriptors.

```
scripts/wc-xkb.sh
```

Five checks that `libxkbcommon` *links and runs* in a Windows binary. `cargo
check` never reaches the linker, so nothing before this stage said anything about
whether a Windows `.exe` could be produced at all — and smithay's `xkbcommon`
dependency is not optional. Proven twice, once in C and once through the Rust
crate.

```
scripts/wc-xdg.sh all
```

Ten checks. The one that matters is `wine: the client created an xdg_toplevel`:
`weston-multi-resource`, a C client from the Weston project using libwayland and
the upstream `xdg-shell` XML, driving smithay inside a mingw-built `.exe`.

Read the header of `wc-xdg.sh` for why the client is that one and not
`weston-flower` — the answer is `docs/design.md` §2.4 arriving on schedule, and
it is a real limit rather than a test convenience.

```
WC_HOST=you@vm-host WC_HOST_WESTON=/path/to/weston/bin scripts/wc-guest.sh all
```

The same thing on **real Windows**, in a VM. Blocks 1-6 run from here; blocks
7-10 run the proxy, the client and the arena locator on `WC_HOST`, the machine
the VM is on, because a pixel has to land in memory both sides can name and
protocol bytes do not. Without `WC_HOST` those four blocks skip and say why.

Everything passes. It did not in stage 5: the shared mount is not coherent while
mapped, so a client that redraws every frame got a frozen window and
`wc-guest.sh` said so. Stage 7 replaced the region with guest RAM located through
QEMU's flat view and the same check now reports 626 frames, 60 distinct, the
commonest 11 — the control's numbers. `docs/design.md` §10.7 is the failure and
§12 is the fix.

```
WC_HOST=you@vm-host WC_HOST_WESTON=/path/to/weston/bin scripts/wc-desktop.sh all
```

The same compositor on the guest's **interactive desktop**, and photographs of
what it puts there. Everything above this line runs inside an ssh session, whose
window station has no display behind it — `CreateWindowExW` succeeds, the blits
succeed, and nothing is ever on a screen. This is the script that gets onto
`WinSta0` and looks; `docs/stage8/` holds what it saw, one image per claim, and
`docs/design.md` §13 is the argument. The most useful thing in it is a bug the
picture shows and no check could: `xdg_surface.set_window_geometry` is not
implemented, so every client with its own decorations is drawn offset by its
shadow margin and clipped by it.

```
WC_HOST=you@vm-host WC_HOST_WESTON=/path/to/weston/bin scripts/wc-icon.sh all
```

The taskbar button, with the **client's own icon** on it. Wayland has no icon
protocol worth the name — `xdg_toplevel.set_app_id` names a desktop entry in an
icon theme that exists on the *client's* machine — so `wc-proxy` resolves it
there and ships pixels, and the compositor builds two `HICON`s. Runs twice, the
second time with `--no-icons`, and photographs both taskbars; `docs/stage11/`
holds the pair and `docs/design.md` §16 is the argument. The most useful thing
in it is again something no log could show: both icons were set correctly for a
whole run in which the taskbar showed the generic one, because the shell had
grouped the window with the compositor's console window.

```
WC_HOST=you@vm-host WC_HOST_FIREFOX=/path/to/firefox scripts/wc-titlebar.sh both
```

**One title bar, and it is Windows'.** Until stage 17 every window had two —
Windows' caption, and directly beneath it Firefox's tab strip with its own
minimise, maximise and close buttons. It takes two levers and neither works
alone: the compositor sends `default_mode(2)` on the KDE decoration manager, and
the client is started with `MOZ_GTK_TITLEBAR_DECORATION=none`. The second is in
the *client's* environment and no protocol carries it, so the launcher sets it —
`docs/waycompose-design.md` §22.3 is the assumption written down. This script photographs
the result on the guest's interactive desktop with the pixels in guest RAM, which
is the run §17.11 item 4 asked for; `docs/stage17/stage17-server.png` is the
picture and `scripts/wc-firefox.sh titlebar` is the same claim as protocol
evidence.

```
WC_HOST=you@vm-host WC_HOST_FIREFOX=/path/to/firefox scripts/wc-titlebar.sh tabs
```

**The other arrangement: Firefox's tabs *inside* Windows' caption.** Stage 17's
answer is server-side — Windows draws the caption and Firefox draws below it.
Tabs in the title bar is the client-side one, which is how Firefox on Windows actually looks:
`WM_NCCALCSIZE` moves the client rectangle's top edge to the top of the window,
so the client's tab strip *is* the title bar, and the frame stays the system's —
real border, real shadow, real resize edges. The corner is the interesting part.
The compositor draws no caption buttons of its own: it leaves the rectangle
`DWMWA_CAPTION_BUTTON_BOUNDS` reports at zero alpha, which uncovers the ones DWM
already drew, and forwards the frame's mouse messages to `DwmDefWindowProc` — so
the buttons are Windows' own, and Windows 11's snap layouts work for the same
reason they work anywhere else. `docs/tabs-in-titlebar/pair-topstrip.png` is the
two arrangements stacked and `docs/waycompose-design.md` §31 is the argument,
including the thing that is still not solved: no Wayland protocol can tell
Firefox that the corner is occupied.

```
scripts/wc-launch.sh deploy && scripts/wc-launch.sh install
```

**The product.** It puts an icon on the guest's Windows desktop called "Firefox
(Linux)", wearing Firefox's own pixels, and double-clicking it puts Firefox on
the screen. Nothing above this line is in that path: `wc-launch.exe` runs in the
guest, starts the compositor on the interactive desktop over an arena of the
guest's own pinned RAM, opens its own ssh to the VM host on the host-only
virtio-net link, and asks that host to run the application. The workstation
builds it, installs it, and is not needed again.

`cold`, `warm`, `native`, `bench`, `verify` and `shot` are the measurements —
click-to-window against Windows' own Firefox on the same machine, and what
actually carried the pixels — and `docs/waycompose-design.md` §28 is the argument. The
number the design is arranged around is the host's 2.4 s sweep for the arena's
stamps: a warm session pays it once instead of once per click.

```
scripts/sv-video-decode.sh
scripts/sv-firefox-vulkan-decode.sh       # with SVFD_FIREFOX=<patched firefox>
```

**Hardware video decode through the forwarder.** The GPU side serves its
driver's decode queue family, and `video-probe` and ffmpeg's own Vulkan decoder
(H.264, H.265 8- and 10-bit, AV1, VP9) decode through it byte-identically to
libavcodec. A Firefox 157 built with `nix/firefox-vulkan-readback.patch` decodes
H.264 on the forwarded device and about:support lists it as hardware decoding;
an unpatched Firefox decodes in software, cleanly. `docs/video-decode.md` §7 is
the account, with what is still unmeasured — NVIDIA and the guest's Intel among
it.

**macOS as the Wayland host (`docs/waycompose-design.md` §103).** Linux
applications appear as Mac applications in the Intel Sonoma VM on this
workstation (`/mnt/datasets/macvm`, instance `wayland-host`, template
`wayland-dev`). Each has its own `.app` bundle in `~/Applications/Linux`, its
own Dock tile, name, icon and Dock menu (the desktop entry's actions), and
"Keep in Dock". Windows are native `NSWindow`s, popups are child windows,
AppKit's traffic lights sit beside Firefox's tabs, and Cmd acts as Control.
The clipboard goes both ways through `NSPasteboard`, sound plays through
CoreAudio, and the pixels and the GPU path's memory are in the VM's own RAM,
wired by the macOS process and found by the host. No macOS kernel driver is
involved. Vulkan from Linux renders in the VM on lavapipe, because the VM has
no Metal; on a real Mac the same server uses MoltenVK or KosmicKrisp.
Nothing between the Mac and Linux uses ssh or a socket (§103.15): a session
boots through a record in its own guest RAM that the host's session service
(`wc-mac-sessiond`) finds, and both sides wake each other through legacy
virtio-serial doorbells (`wc-mac-bell-hub`).

```
scripts/mac-launch-install.sh             # both ends of the launcher, bundles pinned in the Dock
scripts/mac-window.sh [CLIENT]            # one Linux client against the compositor in the VM
scripts/mac-gpu.sh [probe|present|compute|window]
scripts/mac-pinning.sh                    # do wired pages stay put? (§103.3)
scripts/mac-shot.sh OUT.png; scripts/mac-input.sh click X Y | type TEXT | key meta_l+c
```

**The nested KWin on the guest's GPU (`docs/waycompose-design.md` §104).** The
future Linux desktop of §102 composites with OpenGL on Zink over sv-icd, on the
guest's Intel GPU, with Aero glass. Its render node is userspace: `sv-drm
serve` is a CUSE device, and `sv-drm ns` runs KWin in a namespace where libdrm
sees it. KWin's buffers are LINEAR images in named guest sections that the
compositor maps, so no frame is copied on either side.

```
sv-drm serve --owner U --group G --mode 0660 --state FILE   # root: /dev/dri/renderD128
sv-drm ns --state FILE -- kwin_wayland ...                  # the user: libdrm sees it
wc-proxy ... --dmabuf-node /dev/dri/renderD128              # zwp_linux_dmabuf_v1, zero copy
scripts/sv-dmabuf-e2e.sh; scripts/wc-dmabuf.sh              # the ICD and the proxy halves, locally
```

**Windows without ssh (`docs/waycompose-design.md` §105).** A Windows session
boots from a record the compositor signs with the installation's key and
writes into the ivshmem device's header page. The VM host's session service
(`wc-sessiond.service`, the same `wc-arena-host sessiond` the macOS guest
uses) reads it and starts the session. After that, every request, the sound
(§103.16's ring), the clipboard and icon feeds and the GPU's cold path cross
as ring slots and mailboxes in shared memory, with doorbells. No ssh key, relay,
tunnel or forced command is left; deploy removes them. `scripts/wc-no-ssh.sh`
runs the host side and the Linux compositor on one machine.

```
scripts/wc-no-ssh.sh                      # boot, control, sound ring, hub restart, lease, wrong key, replay, no port
scripts/wc-launch.sh deploy               # also: the installation key, the registration, wc-sessiond
wc-arena-host sessiond --ivshmem /run/windows-vm/wc-arena/arena --bell $XDG_RUNTIME_DIR/wc-bell/wl.sock

**Host integration (`docs/waycompose-design.md` §106).** Every audio device
of the host (Windows or macOS) is its own sink or source in Linux, named after
it, and Linux's defaults follow the host's. A Linux application's link opens in
the host's browser (`xdg-open` and `BROWSER` are `wc-host-open`), the host's
applications appear in Linux's "Open with" ("Notepad (Windows)", "TextEdit
(macOS)"), and the Linux applications are registered on the host for the types
their desktop entries name: Explorer's and Finder's "Open with", `open -a`. All
of it crosses on the session's rings (`OWNER_HOSTINT`, the sound ring), and the
host checks every request Linux makes (`docs/security-model.md` R38). Under
PipeWire each device is a native node, `wc-pw-node`, paced by the ring's credit.

```
scripts/wc-hostint-local.sh [--mode pipewire|on]  # the Linux end without a VM: devices, defaults, credit, links
wc-launch.exe --install APP                # also registers APP's types (HKCU), with a manifest
wc-launch.exe --uninstall-assoc [APP]      # removes exactly what the manifest lists
wc-launch.exe --taskbar-ids [--dry-run]    # deploy: each app's taskbar identity, per installation (§92.12)
wc-launch.exe --uninstall-taskbar          # this installation's jump lists, shortcuts, relaunch files
wc-hostint-probe.exe [--get-defaults]      # the host's endpoints, handlers and defaults
WC_LAUNCH_AUDIO=on|pipewire|system|user-pipewire   # the session's sound server (wc-session)
WC_LAUNCH_TEST_HOME=DIR                    # tests: the applications get a throwaway HOME and XDG dirs
```

## Layout

```
crates/
  wc-handle      the `h`-argument shim — what an fd becomes with no SCM_RIGHTS
                 (and `wc-poolcost`, which measures what reaching one costs)
  wc-arena-host  finds the guest compositor's arena in QEMU's memory backend.
                 NOT a workspace member: it path-depends on shared-vulkan-base,
                 which must stay out of the Windows cross gate. See §12.3
  wc-transport   AF_UNIX on both platforms, peer credentials, accept
  wc-server      the stage-1 server
  wc-probe       a client that writes the wire format by hand
  wc-compositor  the smithay compositor — xdg_shell, one HWND per toplevel, GDI,
                 and a wl_seat driven by Win32 input (`src/input.rs` is the
                 scan-code-to-evdev table and the build-time keymaps)
  wc-shmclient   a Windows client that speaks the dialect, for stage 3
  wc-proxy       stage 4, **Linux only**: a Wayland server to an unmodified
                 local client and a client of the Windows compositor, so a
                 `wl_shm` buffer crosses the boundary with nothing passed
  wc-desk        stage 8, **Windows only**: which window station a process is
                 on, which windows the shell gives a taskbar button, and
                 `PrintWindow` captures that work through a lock screen. Also
                 what `Sleep` costs against a high-resolution waitable timer
  wc-xkb-probe   proves the xkbcommon crate links and runs as a Windows binary
  wc-hostint     §106, pure: the open-URL boundary, device names, MIME/UTI/
                 extension tables, registry and Info.plist entries, the wire
  wc-hostplat    §106: the host's calls (WASAPI, the shell, the registry;
                 CoreAudio, Launch Services) for host integration
  wc-launch      stage 14, **Windows only**: the launcher a desktop shortcut
                 points at. A named mutex is the session, one owner starts the
                 compositor and the tunnel, every later click is one ssh exec.
                 Its portable half — the ini, the command lines, the .ico
                 wrapper — is tested on the machine that compiles it
vendor/
  wayland-backend    forked 0.3.16      | ours; workspace members, linted
  wayland-server     forked 0.31.13     |
  wayland-scanner    forked 0.31.11     |
  smithay            forked 0.7.0       | near-upstream; excluded from the
  xkbcommon          forked 0.8.0       | workspace so they stay rebaseable
nix/
  libxkbcommon-mingw.nix            libxkbcommon cross-built for mingw, static
  libxkbcommon-mingw-basetsd.patch  an upstream bug, written as a submittable commit
scripts/
  wc-handshake.sh    the stage-1 acceptance test
  wc-xdg.sh          the stage-2 acceptance test
  wc-xkb.sh          libxkbcommon, linked and run
  wc-window.sh       the stage-3 acceptance test — a surface becomes a window
  wc-proxy.sh        the stage-4 acceptance test — an **unmodified** third-party
                     client painting into a Windows window; `cost` measures what
                     a commit costs per copy scope and per transport
  wc-input.sh        the stage-6 acceptance test — `linux` is the seat against
                     Weston clients, `wine` is real WM_KEYDOWN across the
                     boundary, `term` types into weston-terminal and photographs
                     the result
  wc-firefox.sh      the stage-9 acceptance test — **Firefox**, the client the
                     whole project is for. `enumerate` is the protocol
                     requirement list, read off a WAYLAND_DEBUG trace rather
                     than off a wiki; `window` counts the browser's own pixels;
                     `titlebar` is stage 17's — the two levers that leave the
                     window one title bar, with the control leg that shows
                     neither works alone; `tabs` is the tabs-in-title-bar stage's client half —
                     that under client-side decorations the shadow inset is real
                     and the window is the geometry rectangle; `menu`
                     right-clicks and gets an xdg_popup; `wine` puts the whole
                     chain together and produces an HWND. docs/stage9/
  wc-titlebar.sh     the stage-17 and stage-29 acceptance test — the same claims
                     photographed on the guest's **interactive desktop**, with
                     the pixels in the guest's own RAM. It is the intersection
                     wc-guest.sh and wc-desktop.sh each have half of. Five legs:
                     `server` and `client` are stage 17's, `tabs` is the caption
                     folded into the client area, and `noglass` and `nobuttons`
                     are its two controls — the same window with
                     DwmExtendFrameIntoClientArea off, and with the caption
                     buttons left covered. docs/stage17/, docs/tabs-in-titlebar/
  probe-input.c      SendInput/PostMessage into the compositor's window, so the
                     keystrokes in that test are Win32's rather than ours
  probe-filemap.c    is a Windows view of a filesystem path coherent with a Linux
  probe-filemap-peer.c   MAP_SHARED mmap? Measured before stage 4 depended on it,
                     and measured again across the hypervisor in stage 5
  wc-guest.sh        the same acceptance tests on the real guest, plus stage 5:
                     coherency, what a pool costs per transport, and stage 4
                     across a real VM boundary; block 10 is stage 7's arena --
                     guest RAM, located from outside the VM
  wc-icon.sh         the stage-11 acceptance test — the client's own icon on the
                     Windows taskbar. `deploy` cross-compiles and copies under
                     its own image name; `taskbar` runs the client twice, once
                     with the resolver and once with `--no-icons`, and
                     photographs the taskbar both times. `WC_CLIENT_CMD` drives
                     Firefox instead. Everything it produces is in docs/stage11/
  wc-arena-pressure.sh   the stage-16 acceptance test — a real Firefox against a
                     deliberately **starved** arena. It is the only place the
                     compositor's own verdict on this stage's messages can be
                     had: a request naming a pool that has no memory behind it
                     is a protocol error and a dead client, and no unit test can
                     produce a real server's answer to one. WC_ARENA_MIB sets how
                     starved; `both` compares against another proxy binary
  wc-desktop.sh      the stage-8 acceptance test — the compositor on the guest's
                     **interactive desktop** rather than in an ssh session, and
                     photographs of what it puts on screen; `fps` is the
                     poll-cadence A/B. Everything it produces is in docs/stage8/
  wc-launch.sh       the stage-14 harness — it builds and installs the product
                     and then measures it. `cold`/`warm`/`bench` time a click to
                     a window from *inside* the guest, `native` times Windows'
                     own Firefox on the same desktop for comparison, `verify`
                     asserts from both machines' logs that the pixels crossed as
                     arena slices, and `shot` photographs the result. What it
                     tests needs none of it. Everything it produces is in
                     docs/stage14/
  wc-session         the VM host's half of stage 14, and the **forced command**
                     of the guest's ssh key there: the guest can open a session
                     and start the applications the host defines in apps.ini,
                     by id, and cannot send a command line (waycompose-design.md
                     §94; tested by crates/wc-launch/tests/wc_session.rs)
  gen-keymap.sh      RMLVO -> keymap string, on Linux, at build time (and why)
  probe-xkb.c        the C half of wc-xkb.sh
  wc-worktree.sh     reflink-seeded worktrees (read the comments before changing)
  probe-*.c          throwaway C probes that established what Wine implements;
                     kept because §4.4's claims rest on them
```

The first three of `vendor/` are **not** upstream code any more; every changed
hunk carries a `WAYCOMPOSE FORK` comment. `smithay` and `xkbcommon` are held
differently — most of those patches are meant to go back upstream, so they are
kept minimal and are reviewed by diff rather than by clippy. `Cargo.toml`'s
`exclude` comment states the criterion; `docs/design.md` §7.6 states what that
costs.

## Working on it

```
/mnt/datasets/builds/wc-worktree.sh new <branch> [name]
```

Worktrees are reflink-seeded from the base `target/` because `/mnt/datasets` is
btrfs. **Do not share a `CARGO_TARGET_DIR` between them** — the comment at the
top of `wc-worktree.sh` explains what happened last time, and this project is
unusually exposed to it because `vendor/` holds path dependencies with the same
names and versions as the crates.io originals.

### Machine-specific values and evidence

The harness scripts read the guest's user, the host and guest addresses and
the ssh aliases from `scripts/site.env`, which is not tracked: copy
`scripts/site.env.example` and fill it in. A worktree without its own copy
uses the main checkout's (`scripts/site.sh`). The docs write these values as
placeholders such as `<guest-user>`, `<guest-ip>` and `<host-ip>`, and call
the Linux machine that runs the Windows VM `testhost`.

Screenshots, logs and captured output are not in the repository; `.gitignore`
keeps everything under `docs/` except Markdown out. Docs still name such files
by path, as a record of where a run put them.
