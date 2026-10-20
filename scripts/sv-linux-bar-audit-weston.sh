#!/usr/bin/env bash
# Diagnostic compositor wrapper only. Firefox itself is never ptraced.
set -euo pipefail
: "${SV_AUDIT_REAL_WESTON:?pinned compositor required}" "${SV_AUDIT_STRACE:?pinned tracer required}" "${XDG_STATE_HOME:?private fixture state required}"
AUDIT="$XDG_STATE_HOME/weston-import-audit"
mkdir -m 700 "$AUDIT"
# Bash's file limit uses KiB normally and smaller units in POSIX mode.
# One strace output file bounds the aggregate trace, including all children.
ulimit -f 65536
export WAYLAND_DEBUG=server SV_DMABUF_TRACE=1
child=""
cleanup() {
  trap - EXIT TERM INT
  if [ -n "$child" ]; then
    # setsid owns this exact process group; no unrelated process is targeted.
    kill -TERM -- "-$child" 2>/dev/null || true
    wait "$child" 2>/dev/null || true
  fi
}
trap cleanup EXIT TERM INT
date -Iseconds > "$AUDIT/start-wall.txt"
date +%s.%N > "$AUDIT/start-epoch.txt"
cat /proc/uptime > "$AUDIT/start-uptime.txt"
cat /proc/self/status > "$AUDIT/wrapper-status.txt"
printf '%s\n' "$SV_AUDIT_REAL_WESTON" > "$AUDIT/compositor-path.txt"
printf '%s\n' "$SV_AUDIT_STRACE" > "$AUDIT/tracer-path.txt"
sha256sum "$SV_AUDIT_REAL_WESTON" "$SV_AUDIT_STRACE" > "$AUDIT/executable-hashes.txt"
printf '%s\n' "$@" > "$AUDIT/compositor-arguments.txt"
setsid "$SV_AUDIT_STRACE" -f -ttt -T -yy -v -s 65536 \
  -e trace=recvmsg,sendmsg,ioctl,mmap,munmap,close,dup,dup2,dup3,fcntl,fstat,newfstatat,openat,readlink,readlinkat \
  -o "$AUDIT/native-import.strace" "$SV_AUDIT_REAL_WESTON" "$@" &
child=$!
printf '%s\n' "$child" > "$AUDIT/tracer-pid.txt"
set +e
wait "$child"
status=$?
set -e
printf '%s\n' "$status" > "$AUDIT/tracer-exit.txt"
exit "$status"
