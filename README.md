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
[`docs/stage6-typing.png`](docs/stage6-typing.png). Under Wine — stage 5 put the
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
  wc-xkb-probe   proves the xkbcommon crate links and runs as a Windows binary
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
                     `menu` right-clicks and gets an xdg_popup; `wine` puts the
                     whole chain together and produces an HWND
  probe-input.c      SendInput/PostMessage into the compositor's window, so the
                     keystrokes in that test are Win32's rather than ours
  probe-filemap.c    is a Windows view of a filesystem path coherent with a Linux
  probe-filemap-peer.c   MAP_SHARED mmap? Measured before stage 4 depended on it,
                     and measured again across the hypervisor in stage 5
  wc-guest.sh        the same acceptance tests on the real guest, plus stage 5:
                     coherency, what a pool costs per transport, and stage 4
                     across a real VM boundary; block 10 is stage 7's arena --
                     guest RAM, located from outside the VM
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
