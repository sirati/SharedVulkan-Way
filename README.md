# waycompose

A Wayland compositor that runs on Windows. Phase 2 of the shared_vulkan project.

**Stage 1 is done: the Wayland server stack builds and runs as a Windows
binary.** It composites nothing yet — that was not the question. The question
was whether it could exist at all, and it can.

Read [`docs/design.md`](docs/design.md) first. It decides things and says why.
[`vendor/PATCHES.md`](vendor/PATCHES.md) is the inventory of what was changed in
the forked upstream crates.

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

## Layout

```
crates/
  wc-handle      the `h`-argument shim — what an fd becomes with no SCM_RIGHTS
  wc-transport   AF_UNIX on both platforms, peer credentials, accept
  wc-server      the stage-1 server
  wc-probe       a client that writes the wire format by hand
vendor/
  wayland-backend    forked 0.3.16
  wayland-server     forked 0.31.13
  wayland-scanner    forked 0.31.11
scripts/
  wc-handshake.sh    the acceptance test
  wc-worktree.sh     reflink-seeded worktrees (read the comments before changing)
  probe-*.c          throwaway C probes that established what Wine implements;
                     kept because §4.4's claims rest on them
```

`vendor/` is **not** upstream code any more. Every changed hunk carries a
`WAYCOMPOSE FORK` comment saying what it was and why it is not that any more.

## Working on it

```
/mnt/datasets/builds/wc-worktree.sh new <branch> [name]
```

Worktrees are reflink-seeded from the base `target/` because `/mnt/datasets` is
btrfs. **Do not share a `CARGO_TARGET_DIR` between them** — the comment at the
top of `wc-worktree.sh` explains what happened last time, and this project is
unusually exposed to it because `vendor/` holds path dependencies with the same
names and versions as the crates.io originals.
