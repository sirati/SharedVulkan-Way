#!/usr/bin/env bash
# Build the publishable history from local master, deterministically:
# messages (rewrite.py), authors (mailmap), private values (replacements.txt),
# and no evidence files. Result: /mnt/datasets/builds/svway-publish (branch
# main) and the commit map at /mnt/datasets/builds/svway-publish-commit-map.txt.
set -euo pipefail
R=/mnt/datasets/builds/svway-rewrite
OUT=/mnt/datasets/builds/svway-publish
SRC=${1:-/mnt/datasets/builds/shared-vulkan-base}
rm -rf "$OUT"
git init -q "$OUT"
cd "$OUT"
git fetch -q --no-tags "$SRC" master:refs/heads/main
# Optional user-requested source checkpoints. All refs go through the same scrub.
if [ -n "${SVWAY_CHECKPOINT_REFS:-}" ]; then
  checkpoint_specs=()
  while IFS= read -r checkpoint_ref; do
    case "$checkpoint_ref" in refs/heads/*) ;; *) echo "Invalid checkpoint ref" >&2; exit 1 ;; esac
    git check-ref-format "$checkpoint_ref"
    checkpoint_specs+=("$checkpoint_ref:refs/heads/checkpoint/${checkpoint_ref#refs/heads/}")
  done < "$SVWAY_CHECKPOINT_REFS"
  git fetch -q --no-tags "$SRC" "${checkpoint_specs[@]}"
fi
git symbolic-ref HEAD refs/heads/main
git checkout -q -f main
nix shell nixpkgs#git-filter-repo -c git filter-repo --force \
  --mailmap "$R/mailmap" \
  --replace-text "$R/replacements.txt" \
  --replace-message "$R/replacements.txt" \
  --filename-callback '
import re
p = filename.decode("utf-8", "surrogateescape")
if p.startswith("docs/") and not p.endswith(".md"):
    return None
if re.search(r"\.(png|jpe?g|gif|webp|bmp|ico|log|moz_log|jsonl|gz)$", p, re.I):
    return None
return filename
' \
  --message-callback '
import sys
sys.path.insert(0, "/mnt/datasets/builds/svway-rewrite")
import rewrite
return rewrite.rewrite(message.decode("utf-8")).encode("utf-8")
'
cp .git/filter-repo/commit-map /mnt/datasets/builds/svway-publish-commit-map.txt
echo "main: $(git rev-parse --short main), $(git rev-list --count main) commits"
