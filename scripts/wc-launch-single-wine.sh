#!/usr/bin/env bash
# §102.14 under Wine: a `single = on` application's clicks, through the real
# wc-launch.exe, with scripts/wc-launch-single-fake.rs standing in for ssh.exe
# (it answers `up`, `proxy`, `run` as the control files say and counts the
# runs), the compositor (it shows one window with the app's title when a run
# was accepted) and the relay.
#
#   scripts/wc-launch-single-wine.sh     (inside `nix develop`, after
#                                         cargo build -p wc-launch --target x86_64-pc-windows-gnu)
#   WC_SINGLE_SLOW=1 ...                 also the 90 s no-window case
#
# Wine has no window property store, so its windows are found by their exact
# title, not by their taskbar identity; the guest checks the identity path.
# Wine's appends to one file from several processes are not atomic, so lines
# of the launcher's log can be lost when clicks race; the checks count exits
# and the fake ssh's runs, and the log lines are printed for reading.
#
# Scratch under $WC_SINGLE_SCRATCH (default /mnt/datasets/builds/kwin-gpu-scratch/single).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
S=${WC_SINGLE_SCRATCH:-/mnt/datasets/builds/kwin-gpu-scratch/single}
EXE=$ROOT/target/x86_64-pc-windows-gnu/debug/wc-launch.exe
for t in wine Xvfb cargo; do command -v "$t" >/dev/null || { echo "need $t (run inside nix develop)"; exit 2; }; done
[ -f "$EXE" ] || { echo "no $EXE"; exit 2; }

mkdir -p "$S/fake/src"
cp "$ROOT/scripts/wc-launch-single-fake.rs" "$S/fake/src/main.rs"
printf '[package]\nname = "wcfake"\nversion = "0.0.0"\nedition = "2024"\n[workspace]\n' > "$S/fake/Cargo.toml"
CARGO_TARGET_DIR=$S/fake/target cargo build -q --offline --manifest-path "$S/fake/Cargo.toml" \
  --target x86_64-pc-windows-gnu || exit 2

R=$S/r
rm -rf "$R"; mkdir -p "$R/bin" "$R/ctl"
cp "$EXE" "$S/fake/target/x86_64-pc-windows-gnu/debug/wcfake.exe" "$R/bin/"
export WINEPREFIX=${WC_SINGLE_WINEPREFIX:-$S/wp} WINEDEBUG=${WINEDEBUG:--all}
export WINEDLLOVERRIDES="winemenubuilder.exe=d"
[ -f "$WINEPREFIX/system.reg" ] || { echo "  ....  booting a Wine prefix"; wineboot -i >/dev/null 2>&1; wineserver -w; }
DPY=${WC_SINGLE_DISPLAY:-:87}
Xvfb "$DPY" -screen 0 1280x1024x24 >/dev/null 2>&1 & XPID=$!
export DISPLAY=$DPY
trap 'touch "$R/ctl/stop"; sleep 1; wineserver -k 2>/dev/null; kill $XPID 2>/dev/null' EXIT
sleep 1
W() { echo "Z:${1//\//\\}"; }
FAKE=$(W "$R/bin/wcfake.exe")
export FAKE_DIR; FAKE_DIR=$(W "$R/ctl")
TITLE="Windows 7 (Linux)"
cat > "$R/bin/wc-launch.ini" <<EOF
[session]
host       = u@fake
key        = C:\\nokey
ssh        = $FAKE
compositor = $FAKE
relay      = $FAKE
name       = wcsingle
audio      = off
wayland-bell =
[app.linux-desktop]
title    = $TITLE
icon     = atp-nested
host-app = linux-desktop
desktop  = atp-nested
app-id   = atp-nested
single   = on
EOF
echo "$TITLE" > "$R/ctl/title"
# The log is named after `--session-name` (default wcl), not the ini's name.
LA=$(winepath -u 'C:\users' 2>/dev/null)/$USER/AppData/Local/waycompose
LOG=$LA/wc-launch-wcl.log
rm -f "$LA"/wc-launch-wcl*

pass=0; fail=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
click() { ( wine "$R/bin/wc-launch.exe" linux-desktop >/dev/null 2>&1; echo "exit $?" >> "$R/ctl/exits" ) & }
count() { local n; n=$(grep -c "$1" "$2" 2>/dev/null); echo "${n:-0}"; }
exits() { count "^exit $1" "$R/ctl/exits"; }
probe() { wine "$R/bin/wcfake.exe" "$@" 2>/dev/null | tr -d '\r'; }
runs() { count "^run " "$R/ctl/ssh.log"; }
nwin() { probe windows "$TITLE" | grep -c window; }
lines() { count "$1" "$LOG"; }
reset() {
  touch "$R/ctl/stop"
  for _ in $(seq 100); do pgrep -f 'wcfake.exe (wayland|-i|5743)' >/dev/null || break; sleep 0.2; done
  sleep 2
  rm -f "$R/ctl/stop" "$R/ctl/ssh.log" "$R/ctl/show-window" "$R/ctl/exits"
  : > "$LOG"
}

echo "== A: a cold click takes the lock, runs once, and its window comes"
echo ok > "$R/ctl/run-mode"; echo 1500 > "$R/ctl/run-delay"
click
for _ in $(seq 150); do grep -q CLICK-TO-WINDOW "$LOG" 2>/dev/null && break; sleep 0.2; done
[ "$(runs)" = 1 ] && ok "one run" || bad "runs=$(runs)"
grep -q 'launching, holding Local\\waycompose-single-wcsingle-linux-desktop' "$LOG" && ok "took the lock" || bad "no lock line"
grep -q "CLICK-TO-WINDOW .*single" "$LOG" && ok "saw its window and let the lock go" || bad "no window line"
[ "$(nwin)" = 1 ] && ok "one window" || bad "windows: $(nwin)"

echo "== B: a click while it is up and minimised: restored, focused, no run"
probe minimize "$TITLE" >/dev/null; sleep 0.5
wine "$R/bin/wc-launch.exe" linux-desktop >/dev/null 2>&1; rc=$?
[ $rc = 0 ] && ok "exit 0" || bad "exit $rc"
[ "$(runs)" = 1 ] && ok "still one run" || bad "runs=$(runs)"
grep -q "a window is already up (hwnd .*, by title); restored it, .*; no run sent" "$LOG" && ok "logged" || bad "no focus line"
sleep 0.5; w=$(probe windows "$TITLE")
echo "$w" | grep -q "iconic=0" && ok "restored" || bad "still minimised: $w"
echo "$w" | grep -q "foreground=true" && ok "in the foreground" || bad "not in the foreground: $w"
reset

echo "== C: five clicks within a second while it starts: one run, one window"
echo 3000 > "$R/ctl/run-delay"
for _ in 1 2 3 4 5; do click; sleep 0.15; done
for _ in $(seq 300); do [ "$(exits 0)" -ge 4 ] && break; sleep 0.2; done
[ "$(exits 0)" = 4 ] && ok "four clicks exited 0 (the fifth owns the session)" || bad "exits: $(cat "$R/ctl/exits" 2>/dev/null)"
[ "$(runs)" = 1 ] && ok "one run" || bad "runs=$(runs)"
[ "$(nwin)" = 1 ] && ok "one window" || bad "windows: $(nwin)"
echo "  ....  log: $(lines 'another click is launching it') waited, $(lines "the other click's window is up") focused it (of 4)"
reset

echo "== D: the host refuses: one run, one dialog, the other clicks go quietly"
echo refuse > "$R/ctl/run-mode"; echo 1500 > "$R/ctl/run-delay"
for _ in 1 2 3; do click; sleep 0.15; done
d=$(probe dialog "$TITLE" 60); echo "  ....  $d"
echo "$d" | grep -q "dialogs=1 .*already running" && ok "one dialog, saying so" || bad "dialog: $d"
for _ in $(seq 50); do [ "$(exits '[01]')" -ge 3 ] && break; sleep 0.2; done
[ "$(exits 0)" = 2 ] && [ "$(exits 1)" = 1 ] && ok "the launch exited 1, the two others 0" || bad "exits: $(cat "$R/ctl/exits" 2>/dev/null)"
[ "$(runs)" = 1 ] && ok "one run" || bad "runs=$(runs)"
d=$(probe dialog "$TITLE" 2); [ "$d" = "dialogs=0" ] && ok "no second dialog" || bad "$d"
echo "  ....  log: $(lines 'ended without a window') went quietly (of 2)"
reset

echo "== E: the session cannot start, German UI: a German dialog"
cp "$R/bin/wc-launch.ini" "$R/ini.good"
sed -i "s|^compositor = .*|compositor = C:\\\\nope\\\\missing.exe|" "$R/bin/wc-launch.ini"
LC_ALL=de_DE.UTF-8 LANG=de_DE.UTF-8 wine "$R/bin/wc-launch.exe" linux-desktop >/dev/null 2>&1 &
d=$(probe dialog "$TITLE" 60); echo "  ....  $d"
if echo "$d" | grep -q "dialogs=1 .*Sitzung ließ sich nicht starten"; then ok "German dialog"
elif echo "$d" | grep -q "dialogs=1 .*session did not start"; then ok "dialog (in English: this Wine's UI language is not German)"
else bad "dialog: $d"; fi
cp "$R/ini.good" "$R/bin/wc-launch.ini"
sleep 2; reset

if [ "${WC_SINGLE_SLOW:-0}" = 1 ]; then
  echo "== F: accepted, and no window in 90 s: a dialog; a second click goes quietly"
  echo nowindow > "$R/ctl/run-mode"; echo 0 > "$R/ctl/run-delay"
  click; sleep 1; click
  d=$(probe dialog "$TITLE" 120); echo "  ....  $d"
  echo "$d" | grep -q "dialogs=1 .*no window appeared within 90 seconds" && ok "dialog" || bad "dialog: $d"
  for _ in $(seq 50); do [ "$(exits 0)" -ge 1 ] && break; sleep 0.2; done
  [ "$(exits 0)" -ge 1 ] && ok "the second click exited 0" || bad "exits: $(cat "$R/ctl/exits" 2>/dev/null)"
  [ "$(runs)" = 1 ] && ok "one run" || bad "runs=$(runs)"
  reset
fi

echo "$pass passed, $fail failed"
[ $fail = 0 ]
