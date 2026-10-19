#!/usr/bin/env bash
# §106: host integration's Linux end, end to end on this machine, no VM.
#
#   scripts/wc-hostint-local.sh [--no-build]
#
# 1. builds wc-proxy, wc-host-open and the example `hostint-local` (in the
#    10 GB build cgroup, $WC_CG, default /mnt/datasets/builds/wcrun/cg);
# 2. starts a PRIVATE PipeWire stack (pipewire, wireplumber, pipewire-pulse,
#    the monitor tunnel and `wc-proxy pcm-serve`) from `wc-session pw-stack`,
#    the configuration WC_LAUNCH_AUDIO=pipewire gives a session, in a temporary
#    directory under $WC_SCRATCH (default /mnt/datasets/builds/scratch). The
#    user's own desktop PipeWire is never touched: every pw-* command here
#    and in the example is checked to point into that directory first;
# 3. runs the example, which plays the host over an in-memory control region:
#    devices to nodes, defaults, hot-plug, wc-host-open, the xdg-open shim;
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
  cp "$S"/pw/*.log "$S"/pw/*.conf "$S"/hostint/audio/nodes.log "$EVID/run-$STAMP.files/" 2>/dev/null
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
RT=$(sed -n 's/^D=[^ ]* PW=[^ ]* RT=\([^ ]*\) .*/\1/p' "$SCRIPT")
case "$RT" in
  "$S"/pw) ;;
  *) echo "REFUSING: the stack's runtime directory is $RT, not under $S"; exit 1 ;;
esac
PWX=$(sed -n 's/.* PWX=\([^ ]*\) .*/\1/p' "$SCRIPT")
echo "stack: $SCRIPT (pipewire $PWX, runtime $RT)"

# pw CMD...: a pw-* command against the test's daemon, its environment
# checked before it runs.
pw() {
  local rt="$S/pw"
  case "$rt" in /run/user/*) echo "REFUSING $1: $rt" >&2; return 1 ;; "$T"/*) ;; *) echo "REFUSING $1: $rt" >&2; return 1 ;; esac
  env -u PULSE_SERVER PIPEWIRE_RUNTIME_DIR="$rt" XDG_RUNTIME_DIR="$rt" PIPEWIRE_REMOTE="$rt/pipewire-0" timeout -s KILL 5 "$@"
}

env -u PIPEWIRE_REMOTE -u PULSE_SERVER bash "$SCRIPT" >"$T/stack.out" 2>&1 &
STACK=$!
for _ in $(seq 1 100); do
  [ -S "$S/pw/pipewire-0" ] && [ -S "$S/xdg/pulse/native" ] && [ -S "$S/audio.sock" ] && break
  sleep 0.05
done
[ -S "$S/pw/pipewire-0" ] && [ -S "$S/xdg/pulse/native" ] && [ -S "$S/audio.sock" ] \
  || { echo "the stack did not come up"; cat "$T/stack.out"; exit 1; }
# WirePlumber makes the `default` metadata once it is running.
for _ in $(seq 1 100); do pw pw-metadata -n default 2>/dev/null | grep -q 'default.audio.sink' && break; sleep 0.05; done
echo "--- the private daemon before the test:"
pw pw-metadata -n default
pw pw-dump | grep -E '"node.name"' | sed 's/^ */  /'

echo "--- the example:"
RC=0
env -u PIPEWIRE_REMOTE -u PULSE_SERVER PIPEWIRE_RUNTIME_DIR="$S/pw" XDG_RUNTIME_DIR="$S/pw" \
  "$TD/examples/hostint-local" --dir "$S" --pipewire "$S/pw/pipewire-0" --pw-bin "$(dirname "$PWX")" \
  --opener "$TD/wc-host-open" --shim "$ROOT/scripts/wc-hostint/xdg-open" \
  $(command -v pacat >/dev/null && printf -- '--pacat %s' "$(command -v pacat)") || RC=$?
echo "--- the private daemon after the test:"
pw pw-dump | grep -E '"node.name"' | sed 's/^ */  /'
echo "--- the generated node host configuration was $S/hostint/audio/nodes.conf; its log:"
cat "$S/hostint/audio/nodes.log" 2>/dev/null | tail -5
echo "RESULT: $([ "$RC" = 0 ] && echo PASS || echo "FAIL ($RC failure(s))") -- log $LOG"
exit "$RC"
