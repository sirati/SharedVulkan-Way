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

Read [`docs/design.md`](docs/design.md) first. It decides things and says why —
§7 is stage 2. [`vendor/PATCHES.md`](vendor/PATCHES.md) is the inventory of what
was changed in the forked upstream crates.

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

## Layout

```
crates/
  wc-handle      the `h`-argument shim — what an fd becomes with no SCM_RIGHTS
  wc-transport   AF_UNIX on both platforms, peer credentials, accept
  wc-server      the stage-1 server
  wc-probe       a client that writes the wire format by hand
  wc-compositor  the stage-2 smithay compositor — wl_display, xdg_shell, no pixels
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
