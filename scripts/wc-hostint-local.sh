#!/usr/bin/env bash
# §106: host integration's Linux end, end to end on this machine, no VM.
#
#   scripts/wc-hostint-local.sh [--no-build]
#
# 1. builds wc-proxy, wc-host-open and the example `hostint-local` (in the
#    10 GB build cgroup, $WC_CG, default /mnt/datasets/builds/wcrun/cg);
# 2. starts a PRIVATE PipeWire stack (pipewire, wireplumber, pipewire-pulse)
#    from `wc-session pw-stack`,
#    the configuration WC_LAUNCH_AUDIO=pipewire gives a session, in a temporary
#    directory under $WC_SCRATCH (default /mnt/datasets/builds/scratch). The
#    user's own desktop PipeWire is never touched: every pactl, pacat, parec
#    and pw-* command here and in the example is checked to point into that
#    directory first;
# 3. runs the example: a fake sound-ring DEVICE side (two outputs, a
#    microphone, Default switches, credit, hot-plug, Start/Stop) against the
#    proxy's sound.rs, and a fake host for wc-host-open and the xdg-open shim;
# 4. stops everything and removes the directory. The log goes to
#    $WC_EVIDENCE (default /mnt/datasets/builds/evidence/host-integration/linux-local).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SCRATCH="${WC_SCRATCH:-/mnt/datasets/builds/scratch}"
EVID="${WC_EVIDENCE:-/mnt/datasets/builds/evidence/host-integration/linux-local}"
CG="${WC_CG:-/mnt/datasets/builds/wcrun/cg}"
mkdir -p "$EVID" "$SCRATCH"
STAMP=$(date +%Y%m%d-%H%M%S)
LOG="$EVID/run-$STAMP.log"
exec > >(tee "$LOG") 2>&1
echo "wc-hostint-local $STAMP, tree $(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet || echo '+dirty')"

if [ "${1:-}" != --no-build ]; then
  (cd "$ROOT" && "$CG" nix develop -c cargo build -q -p wc-proxy --bins --example hostint-local)
fi
TD="$ROOT/target/debug"
for b in "$TD/wc-proxy" "$TD/wc-host-open" "$TD/examples/hostint-local"; do [ -x "$b" ] || { echo "no $b"; exit 1; }; done

T=$(mktemp -d "$SCRATCH/hostint-local-XXXXXX")
S="$T/s"
STACK=""
cleanup() {
  set +e
  [ -n "$STACK" ] && kill "$STACK" 2>/dev/null && wait "$STACK" 2>/dev/null
  mkdir -p "$EVID/run-$STAMP.files"
  cp "$S"/pw/*.log "$S"/pw/*.conf "$EVID/run-$STAMP.files/" 2>/dev/null
  cp -r "$S"/hostint/xdg-data "$S"/hostint/xdg-config "$EVID/run-$STAMP.files/" 2>/dev/null
  local left; left=$(pgrep -af -- "$T" | grep -v pgrep)
  if [ -n "$left" ]; then echo "LEFT OVER, killing: $left"; pkill -f -- "$T/"; sleep 0.5; fi
  rm -rf "$T"
  echo "cleaned up $T"
}
trap cleanup EXIT

mkdir -p "$T/base/bin" "$S"
ln -s "$TD/wc-proxy" "$T/base/bin/wc-proxy"
SCRIPT=$(WC_LAUNCH_BASE="$T/base" bash "$ROOT/scripts/wc-session" pw-stack "$S")
RT=$(sed -n 's/^D=[^ ]* PW=\([^ ]*\) .*/\1/p' "$SCRIPT")
case "$RT" in
  "$S"/pw) ;;
  *) echo "REFUSING: the stack's runtime directory is $RT, not under $S"; exit 1 ;;
esac
PWX=$(sed -n 's/.* PWX=\([^ ]*\) .*/\1/p' "$SCRIPT")
echo "stack: $SCRIPT (pipewire $PWX, runtime $RT)"

# A pulse client tool: PATH, else the newest pulseaudio package in the store
# (wc-session's pulse_bin does the same).
tool() {
  command -v "$1" 2>/dev/null && return 0
  ls -d /nix/store/*-pulseaudio-[0-9]*/bin/"$1" 2>/dev/null | sed 's|^/nix/store/[^-]*-\(.*\)$|\1\t&|' | sort -V | tail -1 | cut -f2 | grep . 
}
PACTL=$(tool pactl) && PACAT=$(tool pacat) && PAREC=$(tool parec) || { echo "no pactl/pacat/parec"; exit 1; }
PWMD="$(dirname "$PWX")/pw-metadata"

# pw / pa CMD...: a command against the test's servers, its target checked
# before it runs.
pw() {
  local rt="$S/pw"
  case "$rt" in /run/user/*) echo "REFUSING $1: $rt" >&2; return 1 ;; "$T"/*) ;; *) echo "REFUSING $1: $rt" >&2; return 1 ;; esac
  env -u PULSE_SERVER PIPEWIRE_RUNTIME_DIR="$rt" XDG_RUNTIME_DIR="$rt" PIPEWIRE_REMOTE="$rt/pipewire-0" timeout -s KILL 5 "$@"
}
pa() {
  local s="$S/xdg/pulse/native"
  case "$s" in "$T"/*) ;; *) echo "REFUSING $1: $s" >&2; return 1 ;; esac
  env -u PIPEWIRE_REMOTE PULSE_SERVER="unix:$s" XDG_RUNTIME_DIR="$S/pw" timeout -s KILL 5 "$@"
}

env -u PIPEWIRE_REMOTE -u PULSE_SERVER bash "$SCRIPT" >"$T/stack.out" 2>&1 &
STACK=$!
for _ in $(seq 1 100); do [ -S "$S/pw/pipewire-0" ] && [ -S "$S/xdg/pulse/native" ] && break; sleep 0.05; done
[ -S "$S/pw/pipewire-0" ] && [ -S "$S/xdg/pulse/native" ] || { echo "the stack did not come up"; cat "$T/stack.out"; exit 1; }
# WirePlumber makes the `default` metadata once it is running.
for _ in $(seq 1 100); do pw "$PWMD" -n default 2>/dev/null | grep -q 'default.audio.sink' && break; sleep 0.05; done
echo "--- the private server before the test:"
pa "$PACTL" info | grep -E "Server Name|Default Sink|Default Source"
pa "$PACTL" list short sinks

echo "--- the example:"
RC=0
env -u PIPEWIRE_REMOTE -u PULSE_SERVER \
  "$TD/examples/hostint-local" --dir "$S" --pulse "$S/xdg/pulse/native" --pipewire "$S/pw/pipewire-0" \
  --pactl "$PACTL" --pacat "$PACAT" --parec "$PAREC" --pw-metadata "$PWMD" \
  --opener "$TD/wc-host-open" --shim "$ROOT/scripts/wc-hostint/xdg-open" || RC=$?
echo "--- the private server after the test:"
pa "$PACTL" list short sinks
pa "$PACTL" list short sources
echo "RESULT: $([ "$RC" = 0 ] && echo PASS || echo "FAIL ($RC failure(s))") -- log $LOG"
exit "$RC"
