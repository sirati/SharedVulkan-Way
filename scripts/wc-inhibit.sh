#!/usr/bin/env bash
# Prove that stage 21's four protocols work rather than merely bind.
#
#   scripts/wc-inhibit.sh          against the Linux build of the compositor
#   scripts/wc-inhibit.sh wine     the same probe, into the Windows binary
#
# Run from inside `nix develop`.
#
# WHAT IS BEING CLAIMED
#
# That a client which binds `zwp_idle_inhibit_manager_v1`, `xdg_activation_v1`,
# `zwp_keyboard_shortcuts_inhibit_manager_v1` or `xdg_toplevel_icon_manager_v1`
# gets the behaviour those protocols promise — not that the globals exist.
# `wayland-info` already proves the second, and the second is what a broken
# implementation also passes. The distinction is docs/design.md §25's whole
# argument: a client feature-detects by binding, so an absent global sends it
# down a fallback path and a broken one sends it into a wall.
#
# WHY THE LINUX BUILD
#
# Because this gate is about the *Wayland* half — which surfaces count as
# visible, when an inhibitor is swept, whether the shortcuts grant is sent. The
# Win32 half is one call per decision (`SetThreadExecutionState`,
# `SetForegroundWindow`) and neither can be observed from a client at all: the
# first has no protocol events and the second has no failure event. Those need
# the real guest, and §25 says so rather than letting Wine imply otherwise —
# Wine has stubbed exactly this class of API four times (§13.6).
#
# The assertions are ordered so a failure localises:
#
#   1. the probe bound all seven globals          — the compositor advertises
#   2. `shortcuts_active=1`                        — the grant is a real event
#   3. `token=1 activate_sent=1`                   — the activation token dance
#   4. `idle inhibit -> on`                        — a held inhibitor is honoured
#   5. `idle inhibit -> off` after the destroy     — and released
#   6. `idle inhibit -> off` after the disconnect  — the abandoned one is swept
#
# (6) is the one that would be missing if this had been written the obvious
# way. smithay calls `uninhibit` only for an explicit `destroy` request, so a
# client that dies holding an inhibitor leaves it behind for ever.
set -u

root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

proto="$root/crates/wc-proxy/protocols"
log="$tmp/compositor.log"
sock="wc-inhibit-$$"

fail=0
say() { printf '%s\n' "$*"; }
check() {
	if grep -qF "$2" "$3"; then
		say "  ok    $1"
	else
		say "  FAIL  $1 (looked for: $2)"
		fail=1
	fi
}

# --- build the probe -------------------------------------------------------
#
# The headers are generated from *our* vendored XML rather than from whatever
# wayland-protocols the machine has, for the reason `vendor/` exists: a probe
# whose idea of an opcode comes from somewhere else is a probe that can pass
# against a compositor that is wrong, or fail against one that is right.
say "generating client headers from $proto"
for p in xdg-shell idle-inhibit-unstable-v1 xdg-activation-v1 \
	 keyboard-shortcuts-inhibit-unstable-v1 xdg-toplevel-icon-v1; do
	wayland-scanner client-header "$proto/$p.xml" "$tmp/$p-client-protocol.h" || exit 1
	wayland-scanner private-code "$proto/$p.xml" "$tmp/$p-protocol.c" || exit 1
done

say "building probe-inhibit"
cc -O1 -g -I"$tmp" -o "$tmp/probe-inhibit" \
	"$root/scripts/probe-inhibit.c" "$tmp"/*-protocol.c \
	$(pkg-config --cflags --libs wayland-client) || exit 1

# --- run it against the compositor -----------------------------------------
export XDG_RUNTIME_DIR="$tmp"
mode=${1:-linux}

if [ "$mode" = wine ]; then
	# The Windows binary, reached **through wc-proxy**, which is the real
	# topology and is not optional here.
	#
	# The first attempt pointed the probe straight at the Wine socket, the
	# way `scripts/wc-xdg.sh wine` does, and it got as far as
	# `wl_surface commit` and stopped. That is docs/design.md §2.4 arriving
	# on schedule and is worth recording rather than working around
	# silently: this probe attaches a `wl_shm` buffer, `wl_shm.create_pool`
	# carries an `h`, a stock libwayland client sends it out of band by
	# `SCM_RIGHTS`, and Wine's AF_UNIX carries no ancillary data — so the
	# descriptor is simply absent from the byte stream and the compositor's
	# parser desynchronises at exactly that request. A client that maps a
	# window cannot reach a Windows compositor without the proxy, by
	# construction.
	#
	# Which makes this the better test anyway: with the proxy in the middle,
	# the run proves stage 21's first item end to end. A client binds
	# `xdg_toplevel_icon_manager_v1` and sends a request on it, and before
	# this stage the proxy answered `Error::UnknownOpcode` and ended the
	# connection.
	#
	# **What this still cannot settle.** `SetThreadExecutionState` and
	# `SetForegroundWindow` are called, which catches a wrong argument width
	# or a missing import. It says nothing about whether a display stays on:
	# Wine has no display to keep awake and its `SetForegroundWindow`
	# answers about an X11 window on an Xvfb nobody is looking at. §13.6
	# counts four occasions where Wine's answer and the guest's differed,
	# and this is precisely that class of API.
	command -v wine >/dev/null || { say "wine is not on PATH"; exit 2; }
	exe="$root/target/x86_64-pc-windows-gnu/debug/wc-compositor.exe"
	[ -f "$exe" ] || { say "no $exe — build it first, metadata is not a binary"; exit 2; }
	proxy_bin="$root/target/debug/wc-proxy"
	[ -x "$proxy_bin" ] || { say "no $proxy_bin — cargo build first"; exit 2; }
	# The shared prefix the other Wine gates use, so a first run does not
	# spend its socket timeout on `wineboot` creating one.
	: "${WINEPREFIX:=/tmp/wc-wineprefix}"
	export WINEPREFIX
	export WINEDEBUG=-all
	rm -f "$WINEPREFIX"/drive_c/users/*/AppData/Local/waycompose/"$sock" 2>/dev/null
	mkdir -p "$tmp/keymaps" "$tmp/pools" "$tmp/rt"
	chmod 700 "$tmp/rt"
	# `--shared-dir` is not decoration: the probe binds `wl_seat`, so the
	# compositor sends `wl_keyboard.keymap`, which is an `h` in the
	# server-to-client direction. Without somewhere both sides can see, the
	# compositor publishes a `Local\wc-…` section and the proxy correctly
	# ends the connection.
	xvfb-run -a wine "$exe" --socket "$sock" --timeout 45 \
		--shared-dir "$(winepath -w "$tmp/keymaps" 2>/dev/null || echo "$tmp/keymaps")" \
		--shared-peer-prefix "$tmp/keymaps/" >"$log" 2>&1 &
	comp=$!
	node=""
	for _ in $(seq 200); do
		node=$(find "$WINEPREFIX/drive_c" -type s -name "$sock" 2>/dev/null | head -1)
		[ -n "$node" ] && break
		sleep 0.2
	done
	[ -n "$node" ] || { say "the Windows binary never bound its socket"; cat "$log"; exit 2; }
	say "the Windows binary bound ${node#"$WINEPREFIX/drive_c"}"
	"$proxy_bin" --upstream "$node" --socket "$sock" --runtime-dir "$tmp" \
		--transport shared-file --pool-dir "$tmp/pools" \
		--peer-prefix "$(winepath -w "$tmp/pools" 2>/dev/null || echo "$tmp/pools")\\" \
		--accept-timeout 20 --timeout 30 >"$tmp/proxy.log" 2>&1 &
	proxy=$!
	for _ in $(seq 40); do
		[ -S "$tmp/$sock" ] && break
		sleep 0.1
	done
else
	say "starting the compositor on $sock"
	"$root/target/debug/wc-compositor" --socket "$sock" --timeout 25 >"$log" 2>&1 &
	comp=$!
	for _ in $(seq 40); do
		[ -S "$tmp/$sock" ] && break
		sleep 0.1
	done
fi

WAYLAND_DISPLAY="$sock" "$tmp/probe-inhibit" >"$tmp/probe.log" 2>&1
rc=$?
cat "$tmp/probe.log"

# The compositor's per-turn sweep needs a turn after the client has gone.
sleep 2
# Guarded: an unset `proxy` expanded to 0 would signal the whole process
# group, which includes this script.
[ -n "${proxy:-}" ] && kill "$proxy" 2>/dev/null
kill "$comp" 2>/dev/null
wait "$comp" 2>/dev/null

say ""
say "assertions"
if [ "$rc" -ne 0 ]; then
	say "  FAIL  the probe exited $rc"
	fail=1
else
	say "  ok    the probe exited 0"
fi
check "the shortcuts inhibitor was granted, and the client was told" \
	"keyboard shortcuts inhibitor is ACTIVE" "$tmp/probe.log"
check "an activation token was minted and spent" \
	"activate_sent=1" "$tmp/probe.log"
check "the compositor raised the window it was asked to" \
	"xdg_activation: window" "$log"
check "a held idle inhibitor turned inhibition on" \
	"idle inhibit -> on" "$log"
check "destroying it turned inhibition off" \
	"idle inhibit -> off" "$log"
check "an abandoned inhibitor was swept when its surface died" \
	"idle inhibitor(s) went away with their surface" "$log"
check "the client's own icon crossed and was applied" \
	"from the client's own xdg_toplevel_icon_v1" "$log"
if [ "$mode" = wine ]; then
	# The headline of §25.1: through the proxy, on the real topology, a
	# client that binds this global is no longer killed by its first
	# request. `wc-proxy` says so in its own log rather than ours.
	if grep -q "not in the vendored protocols" "$tmp/proxy.log"; then
		say "  FAIL  the proxy met an interface it could not walk"
		grep "not in the vendored protocols" "$tmp/proxy.log" | sed 's/^/        /'
		fail=1
	else
		say "  ok    the proxy walked every interface the client bound"
	fi
fi

# The two on/off transitions have to happen twice: once around the explicit
# destroy, once around the abandoned inhibitor. One of each would pass a
# compositor that only ever turned it on.
ons=$(grep -c "idle inhibit -> on" "$log")
offs=$(grep -c "idle inhibit -> off" "$log")
if [ "$ons" -ge 2 ] && [ "$offs" -ge 2 ]; then
	say "  ok    inhibition went on and off twice ($ons on, $offs off)"
else
	say "  FAIL  expected two on and two off transitions, got $ons on and $offs off"
	fail=1
fi

if [ "$mode" = wine ]; then
	say ""
	say "what Windows was asked to do (Wine's answers, not the guest's):"
	grep -E "SetThreadExecutionState|SetForegroundWindow|activated|refused" "$log" \
		| sed 's/^/  /' || say "  (nothing — the Win32 half was never reached)"
fi

say ""
if [ "$fail" -eq 0 ]; then
	say "PASS"
else
	say "FAIL — the compositor log follows"
	sed -n '1,200p' "$log"
fi
exit "$fail"
