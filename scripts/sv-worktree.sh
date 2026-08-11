#!/usr/bin/env bash
# Create a shared_vulkan worktree, seeded from the base target/ by reflink.
#
#   sv-worktree.sh new <branch> [name]   create/refresh a worktree for <branch>
#   sv-worktree.sh list                  show worktrees and their sizes
#   sv-worktree.sh rm <name>             remove a worktree
#
# This exists so several agents can implement different parts of the protocol
# at once without sharing a checkout. It is deliberately modelled on
# hnl-worktree.sh, including the two things that script learned the hard way:
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
# on the sibling project here. A reflink copy is the opposite -- each worktree
# owns its files outright from the first write, and CoW shares only the bytes
# neither has modified. Do not "optimise" this into a shared directory.
#
# This project will feel that harder than most: the protocol crate is generated
# from vk.xml and is enormous, so a cold rebuild is expensive and a stale-rmeta
# miscompile in generated marshalling code would be close to undebuggable.
set -uo pipefail

ROOT=/mnt/datasets/builds
BASE="$ROOT/shared-vulkan-base"
TREES="$ROOT/sv-worktrees"

mkdir -p "$TREES"

die() { echo "error: $*" >&2; exit 1; }

[ -d "$BASE/.git" ] || die "$BASE is not a git repository"

case "${1:-}" in
new)
  branch="${2:-}"; [ -n "$branch" ] || die "usage: sv-worktree.sh new <branch> [name]"
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
  name="${2:-}"; [ -n "$name" ] || die "usage: sv-worktree.sh rm <name>"
  dest="$TREES/$name"
  [ -d "$dest" ] || die "no worktree at $dest"
  git -C "$BASE" worktree remove --force "$dest" || die "git worktree remove failed"
  echo "removed $dest"
  ;;

*)
  sed -n '2,26p' "$0"
  ;;
esac
