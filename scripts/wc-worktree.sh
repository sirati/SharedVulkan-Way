#!/usr/bin/env bash
# Create a waycompose worktree, seeded from the base target/ by reflink.
#
#   wc-worktree.sh new <branch> [name]   create/refresh a worktree for <branch>
#   wc-worktree.sh list                  show worktrees and their sizes
#   wc-worktree.sh rm <name>             remove a worktree
#
# Adapted from shared-vulkan-base/scripts/sv-worktree.sh, which was itself
# adapted from hnl-worktree.sh. The two hard-won lessons carry over unchanged
# and are repeated here rather than referenced, because the next person to read
# this file is the one about to "optimise" them away:
#
# Why reflink rather than `cp -r`: /mnt/datasets is btrfs, so `cp --reflink`
# shares extents copy-on-write. Seeding a worktree with a warm target/ is then
# near-instant and costs almost no space until the trees diverge. It only works
# *within* one filesystem, which is why everything lives under /mnt/datasets and
# why this project is not in ~/devel.
#
# Why NOT a shared CARGO_TARGET_DIR: two worktrees whose sources genuinely
# differ can collide on the same path for a path-dependency of the same name and
# version, and one gets served the other's rmeta. That produced a real miscompile
# on a sibling project here. A reflink copy is the opposite -- each worktree owns
# its files outright from the first write, and CoW shares only the bytes neither
# has modified. Do not "optimise" this into a shared directory.
#
# This project has its own reason to care, distinct from phase 1's generated
# protocol crate: `vendor/` holds *forks* of wayland-backend, wayland-server and
# wayland-scanner, all path dependencies with the same names and versions as the
# crates.io originals. That is exactly the shape of the collision above, and the
# symptom would be a Windows build silently linking against unpatched upstream
# code that cannot possibly work — which reads as "our port is broken" rather
# than "the wrong crate was used".
#
# One extra note for this project: the worktree gets its own target/, which means
# its own `target/x86_64-pc-windows-gnu/`. That is deliberate. A cross build and
# a host build share nothing but the source, and mixing them across worktrees is
# how you end up running last week's .exe under Wine and believing it.
set -uo pipefail

ROOT=/mnt/datasets/builds
BASE="$ROOT/waycompose-base"
TREES="$ROOT/wc-worktrees"

mkdir -p "$TREES"

die() { echo "error: $*" >&2; exit 1; }

[ -d "$BASE/.git" ] || die "$BASE is not a git repository"

case "${1:-}" in
new)
  branch="${2:-}"; [ -n "$branch" ] || die "usage: wc-worktree.sh new <branch> [name]"
  name="${3:-$branch}"
  # Worktree names become directory names; keep them boring.
  case "$name" in *[!A-Za-z0-9._-]*) die "name '$name' has characters that do not belong in a path";; esac
  dest="$TREES/$name"

  if [ -d "$dest" ]; then
    echo "worktree $name already exists at $dest"
  else
    # Branch may or may not exist yet; -B makes both cases the same command.
    git -C "$BASE" worktree add -B "$branch" "$dest" >/dev/null \
      || die "git worktree add failed"
    echo "created $dest on branch $branch"
  fi

  # Seed target/ by reflink so the first build is warm. Failure here is not
  # fatal -- a cold target/ is slow, not wrong.
  if [ -d "$BASE/target" ] && [ ! -d "$dest/target" ]; then
    if cp -a --reflink=always "$BASE/target" "$dest/target" 2>/dev/null; then
      echo "seeded target/ by reflink ($(du -sh --apparent-size "$dest/target" 2>/dev/null | cut -f1) apparent)"
    else
      echo "warning: reflink seed failed; worktree starts with a cold target/" >&2
    fi
  fi

  echo
  echo "  cd $dest && nix develop"
  ;;

list)
  git -C "$BASE" worktree list
  ;;

rm)
  name="${2:-}"; [ -n "$name" ] || die "usage: wc-worktree.sh rm <name>"
  dest="$TREES/$name"
  [ -d "$dest" ] || die "no worktree at $dest"
  git -C "$BASE" worktree remove --force "$dest" || die "git worktree remove failed"
  echo "removed $dest"
  ;;

*)
  sed -n '2,37p' "$0"
  ;;
esac
