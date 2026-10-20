#!/usr/bin/env bash
set -euo pipefail
revision=${1:?explicit committed source revision required}
mode=${2:-all}
worktree=/mnt/datasets/builds/shared-vulkan-base/.claude/worktrees/hyperlinux-integration
evidence=/mnt/datasets/builds/evidence/hyperlinux
case "$revision" in *[!0-9a-f]*|'') exit 2;; esac
case "$mode" in clients) names='"primaryOutputClient" "ptracePstateClient" "dcZvaClient" "seqpacketClient" "seqpacketCleanupClient" "seqpacketAliasClient" "seccompClient" "asyncFaultClient" "externalFaultClient" "fatalBusWaitClient"';; all) names='"hl" "probeRunner" "primaryOutputClient" "ptracePstateClient" "dcZvaClient" "seqpacketClient" "seqpacketCleanupClient" "seqpacketAliasClient" "seccompClient" "asyncFaultClient" "externalFaultClient" "fatalBusWaitClient"';; *) exit 2;; esac
expression="let f = builtins.getFlake \"git+file://$worktree?rev=$revision\"; stack = import (f.outPath + \"/nix/hyperlinux/stack.nix\") { inputs = f.inputs // { nixpkgs = f.inputs.hyperlinux-runtime-nixpkgs; nativeNixpkgs = f.inputs.nixpkgs; }; }; in builtins.map (name: builtins.getAttr name stack) [ $names ]"
mkdir -p "$evidence/gcroots"
exec nix build --impure --out-link "$evidence/gcroots/consolidated-$revision-$mode" --json --expr "$expression"
