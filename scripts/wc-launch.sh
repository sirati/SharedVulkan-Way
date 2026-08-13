#!/usr/bin/env bash
# Stage 14. Build, install and *measure* the desktop shortcut that runs Linux
# Firefox on the Windows guest.
#
#   scripts/wc-launch.sh deploy    cross-compile, copy both halves, write the ini
#   scripts/wc-launch.sh install   put the shortcut on the guest's real desktop
#   scripts/wc-launch.sh cold      no session; click; time it
#   scripts/wc-launch.sh warm      session already up; click; time it
#   scripts/wc-launch.sh native    the same measurement for Windows' own Firefox
#   scripts/wc-launch.sh shot      photograph the window that is up
#   scripts/wc-launch.sh stop      end the session
#   scripts/wc-launch.sh all       deploy, install, native, cold, warm, shot
#
# Run from inside `nix develop`.
#
# WHAT THIS SCRIPT IS AND IS NOT
#
# It is a **harness**, and the thing it tests is a product that does not need
# it. Every other script in this directory drives the experiment: it starts the
# compositor over ssh, holds the tunnel open from a workstation, runs the client
# on a third machine and reads the logs back. This one does none of that at run
# time -- `wc-launch.exe` on the guest starts the compositor, opens its own
# tunnel to the VM host and asks that host for the application. The workstation
# is here to *build*, to *install* and to *read the numbers back*, and the run
# it measures would happen identically with this machine switched off.
#
# So the measurement has to be taken from inside the guest, and it is:
# `wc-launch.exe` starts a clock when its process starts -- which is what a
# double-click costs -- and stops it when the compositor's log says a window was
# mapped. This script reads that line. There is no clock here that could be
# measuring an ssh round trip from this room.
#
# HOW A DOUBLE-CLICK IS SIMULATED, AND WHY IT IS NOT A CHEAT
#
# The guest's screen shows a lock screen with Windows Hello and there is no
# password for it here (§13). Nobody in this room can double-click anything. So
# `click.ps1` calls `Start-Process` on the **shortcut file itself**, which is
# `ShellExecute` on a `.lnk` -- the same call Explorer makes when the icon is
# double-clicked, resolving the same target, the same arguments and the same
# working directory. It runs through the scheduler's interactive-token route
# (§13's `runint.ps1`, reproduced here rather than shared, since that file is
# stage 8's and another stage is working in it) so that it happens on
# `WinSta0\Default` and not in an ssh window station where the pixels go
# nowhere.
#
# The scheduler costs about a minute to start a task (§16.8). That minute is
# *before* wc-launch's process exists and is therefore outside every number
# below, which is the reason the clock is inside the launcher rather than here.
set -uo pipefail

MODE="${1:-all}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"

GUEST="${WC_GUEST:-<guest-user>@<guest-ip>}"
HOST="${WC_HOST:-<host-user>@testhost}"
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"

# This stage's own directory on the shared guest, and its own names for
# everything. §16.8: an identity is a path, never an image name.
DIR_WIN='C:\Users\<guest-user>\wclaunch'
DIR_MSYS='/c/Users/<guest-user>/wclaunch'
SESSION="${WC_LAUNCH_SESSION:-wcl}"
APPDATA_MSYS='/c/Users/<guest-user>/AppData/Local/waycompose'

# The VM host's half.
HOSTDIR="${WC_LAUNCH_HOSTDIR:-~/wc-launch}"
# The guest reaches the host over the host-only virtio-net link and nothing
# else. No developer workstation is in this path.
HOSTLINK="${WC_LAUNCH_HOSTLINK:-<host-user>@<host-nat-ip>}"
# A NAME, not a path, and the VM host resolves it -- see `resolve_exec` in
# scripts/wc-session. The brief named `/nix/store/9ndgaj…-system-path/bin/firefox`
# and said not to ship that string; four hours later the current system path had
# no `bin/firefox` in it at all, so neither that path nor `command -v firefox`
# nor `/run/current-system/sw/bin/firefox` is an answer on this machine.
FIREFOX="${WC_LAUNCH_FIREFOX:-firefox}"
NATIVE_FIREFOX='C:\Program Files\Mozilla Firefox\firefox.exe'

ARENA_MIB="${WC_LAUNCH_ARENA_MIB:-768}"
OUTPUT="${WC_LAUNCH_OUTPUT:-1600x1000}"
PORT="${WC_LAUNCH_PORT:-57431}"

export SSH_AUTH_SOCK="$AGENT"
export MSYS_NO_PATHCONV=1
SSHOPT=(-o IdentityAgent="$AGENT" -o BatchMode=yes -o LogLevel=ERROR
        -o StrictHostKeyChecking=accept-new)

pass=0; fail=0
ok()   { echo "  PASS  $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail + 1)); }
note() { echo "  ----  $*"; }

g() { ssh "${SSHOPT[@]}" "$GUEST" "$@" 2>&1 | grep -v 'post-quantum\|store now, decrypt\|may need to be upgraded'; }
h() { ssh "${SSHOPT[@]}" "$HOST" "$@" 2>&1; }

# Run something on the guest's interactive desktop. See the header.
int() {
  local name="$1" exe="$2" argstr="$3" wait="${4:-}"
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DIR_WIN\\runint.ps1' -Exe '$exe' -Arguments '$argstr' -Name '$name' $wait"
}

write_runint() { cat >"$1" <<'PS1'
# Run a command line on the interactive desktop of the logged-on session.
# Generated by scripts/wc-launch.sh; do not edit on the guest.
#
# This is stage 8's runint.ps1 with the working directory changed. It is copied
# rather than shared because scripts/wc-desktop.sh belongs to another stage that
# is being worked in right now, and a shared file between two live stages is a
# way for one of them to break the other silently.
param(
  [Parameter(Mandatory=$true)][string]$Exe,
  [string]$Arguments = "",
  [string]$Name = "wclaunch",
  [string]$WorkDir = "C:\Users\<guest-user>\wclaunch",
  [switch]$Wait,
  [int]$TimeoutSec = 180
)
$ErrorActionPreference = "Stop"
$a = New-ScheduledTaskAction -Execute $Exe -Argument $Arguments -WorkingDirectory $WorkDir
# sshd sets USERDOMAIN=WORKGROUP, which resolves to no SID at all (§13).
$p = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) `
                                -LogonType Interactive -RunLevel Highest
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
                                  -MultipleInstances Parallel
$s.ExecutionTimeLimit = "PT0S"
$s.DisallowStartIfOnBatteries = $false
Register-ScheduledTask -TaskName $Name -Action $a -Principal $p -Settings $s -Force | Out-Null
Start-ScheduledTask -TaskName $Name
Write-Output ("started " + $Name)
if ($Wait) {
  $t0 = Get-Date
  while (((Get-Date) - $t0).TotalSeconds -lt $TimeoutSec) {
    Start-Sleep -Milliseconds 250
    if ((Get-ScheduledTask -TaskName $Name).State -ne "Running") { break }
  }
  $r = Get-ScheduledTaskInfo -TaskName $Name
  Write-Output ("exit " + $r.LastTaskResult)
  Unregister-ScheduledTask -TaskName $Name -Confirm:$false
}
PS1
}

# The double-click. `Start-Process` on a `.lnk` is `ShellExecute` on a `.lnk`,
# which is exactly what Explorer does with one. The path is read from a file
# rather than passed as an argument because it contains a space and three layers
# of quoting stand between here and there.
write_click() { cat >"$1" <<'PS2'
# Generated by scripts/wc-launch.sh; do not edit on the guest.
$lnk = (Get-Content -Raw "C:\Users\<guest-user>\wclaunch\lnk.txt").Trim()
Start-Process -FilePath $lnk
Write-Output ("clicked " + $lnk)
PS2
}

# The capture, run with its console hidden.
#
# `wc-desk.exe` is a console program and the scheduler gives it a console
# window, which lands *on top of the desktop being photographed* -- the first
# picture of the shortcut has a black rectangle across the middle of it with
# `wc-desk.exe` in its title bar. §16.6 is the same fact in its expensive form:
# a console window of ours is a window on this desktop like any other.
write_shot() { cat >"$1" <<'PS4'
# Generated by scripts/wc-launch.sh; do not edit on the guest.
#
# The arguments arrive in a file for the same reason `click.ps1`'s shortcut path
# does: three layers of quoting stand between the caller and here, and a
# `-A '...'` that lost its quotes produced a capture of nothing with no error.
$a = (Get-Content -Raw "C:\Users\<guest-user>\wclaunch\shotargs.txt").Trim().Split(" ")
Start-Process -FilePath "C:\Users\<guest-user>\wclaunch\wc-desk.exe" -ArgumentList $a `
              -WindowStyle Hidden -Wait
PS4
}

# Windows' own Firefox, measured the same way ours is: a clock that starts
# before the process does and stops when a top-level window exists. Our number
# stops at `window N mapped`, which is the same instant on the same desktop --
# the compositor has just returned from `CreateWindowExW`.
write_native() { cat >"$1" <<'PS3'
# Generated by scripts/wc-launch.sh; do not edit on the guest.
#
# WHY THIS DOES NOT WATCH THE PROCESS IT STARTED. Firefox on Windows is a
# *launcher* process: `firefox.exe` re-launches itself and the one that was
# started exits within a tenth of a second. The first version of this script
# watched `$p.MainWindowHandle` and reported **122 ms**, which is not Firefox
# starting -- it is Firefox's stub exiting, with `hwnd 0` in every row saying so.
#
# So the window is looked for across every `firefox.exe` whose *command line*
# carries this run's own profile directory, which is an identity nothing else on
# this shared guest can collide with (§16.8 in the only form available here:
# the image name alone would find the user's own browser).
param([int]$Runs = 3)
$exe = "C:\Program Files\Mozilla Firefox\firefox.exe"
$out = "C:\Users\<guest-user>\wclaunch\native.txt"
Remove-Item $out -ErrorAction SilentlyContinue
function Ours($tag) {
  Get-CimInstance Win32_Process -Filter "Name = 'firefox.exe'" |
    Where-Object { $_.CommandLine -and $_.CommandLine.Contains($tag) }
}
for ($i = 0; $i -lt $Runs; $i++) {
  # A profile of its own, and a fresh one, so this measures Firefox starting
  # rather than Firefox asking an already-running Firefox to open a window --
  # which is what the Linux side is measured doing too.
  $prof = "C:\Users\<guest-user>\wclaunch\ffprof$i"
  Remove-Item -Recurse -Force $prof -ErrorAction SilentlyContinue
  $sw = [Diagnostics.Stopwatch]::StartNew()
  Start-Process $exe -ArgumentList "-no-remote","-profile",$prof,"about:blank" | Out-Null
  $h = 0
  while ($sw.ElapsedMilliseconds -lt 120000) {
    Start-Sleep -Milliseconds 5
    foreach ($q in Ours $prof) {
      $pr = Get-Process -Id $q.ProcessId -ErrorAction SilentlyContinue
      if ($pr -and $pr.MainWindowHandle -ne 0) { $h = [int64]$pr.MainWindowHandle; break }
    }
    if ($h -ne 0) { break }
  }
  $sw.Stop()
  Add-Content $out ("native " + $sw.ElapsedMilliseconds + " ms hwnd " + $h)
  Start-Sleep -Seconds 2
  foreach ($q in Ours $prof) { Stop-Process -Id $q.ProcessId -Force -ErrorAction SilentlyContinue }
  Start-Sleep -Seconds 4
}
Add-Content $out "done"
PS3
}

# ---------------------------------------------------------------- deploy -----
do_deploy() {
  echo "== deploy =="
  [ -n "${WC_XKB_MINGW:-}" ] || { echo "error: run inside \`nix develop\`"; exit 2; }
  # A session from the last run holds both halves' binaries open. Ended through
  # the product's own verb, which kills by the pid in its own state file.
  g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS 2>/dev/null && ./wc-launch.exe --stop" >/dev/null 2>&1

  echo "  building the Windows half"
  cargo build --target x86_64-pc-windows-gnu \
        -p wc-launch -p wc-desk 2>&1 | grep -E "^error" | head -5
  cargo build --target x86_64-pc-windows-gnu -p sv-guest --no-default-features -F wayland 2>&1 | grep -E '^error|warning: unused' | head -5
  for e in wc-launch.exe sv-guest.exe wc-desk.exe; do
    [ -f "$WIN_BIN/$e" ] || { echo "error: no $WIN_BIN/$e"; exit 1; }
  done

  echo "  building the Linux half"
  # `wc-arena-host` is an ordinary workspace member now rather than a crate with its
  # own manifest and its own lock file: the repositories are one, so the absolute
  # path dependency that kept it excluded no longer exists (one-binary.md §1.3). The
  # merge deleted it outright; stage 14 put it back, because nothing replaced it.
  cargo build -p wc-proxy -p wc-arena-host 2>&1 | grep -E '^error' | head -5
  local locator="$LINUX_BIN/wc-arena-host"
  for b in "$LINUX_BIN/wc-proxy" "$locator"; do
    [ -x "$b" ] || { echo "error: no $b"; exit 1; }
  done

  local stage; stage=$(mktemp -d /tmp/wc-launch-stage-XXXXXX)
  trap 'rm -rf "$stage"' RETURN
  x86_64-w64-mingw32-gcc -O2 -o "$stage/probe-relay.exe" "$ROOT/scripts/probe-relay.c" -lws2_32 \
    || { echo "error: probe-relay.exe did not build"; exit 1; }
  cp "$WIN_BIN/wc-launch.exe" "$WIN_BIN/sv-guest.exe" "$WIN_BIN/wc-desk.exe" "$stage/"
  write_runint "$stage/runint.ps1"
  write_click  "$stage/click.ps1"
  write_native "$stage/native.ps1"
  write_shot   "$stage/shot.ps1"

  # The configuration. Written here because the harness knows the paths; a real
  # installation would ship it beside the exe.
  cat >"$stage/wc-launch.ini" <<INI
# Generated by scripts/wc-launch.sh. See docs/waycompose-design.md §28.
[session]
host       = $HOSTLINK
key        = $DIR_WIN\\id_host
compositor = $DIR_WIN\\sv-guest.exe
relay      = $DIR_WIN\\probe-relay.exe
name       = $SESSION
arena-mib  = $ARENA_MIB
outbound   = 1048576
output     = $OUTPUT
port       = $PORT

[app.firefox]
title = Firefox (Linux)
icon  = firefox
exec  = $FIREFOX --profile ~/.wc-launch/firefox
INI

  # A running .exe holds its own file open, so a session that is up must end
  # before its binaries can be replaced. By path, not by image name.
  g "export MSYS_NO_PATHCONV=1; mkdir -p $DIR_MSYS; \
     powershell.exe -NoProfile -Command \"Get-CimInstance Win32_Process | \
       Where-Object { \\\$_.ExecutablePath -and \\\$_.ExecutablePath.StartsWith('$DIR_WIN\\\\') } | \
       ForEach-Object { Stop-Process -Id \\\$_.ProcessId -Force -ErrorAction SilentlyContinue }\"" \
    >/dev/null 2>&1
  sleep 1
  scp "${SSHOPT[@]}" "$stage"/* "$GUEST:wclaunch/" >/dev/null || { echo "error: scp to the guest"; exit 1; }
  echo "  copied wc-launch.exe, sv-guest.exe, wc-desk.exe, probe-relay.exe and the ini to $DIR_WIN"

  # The guest's key for the VM host, generated on the guest so the private half
  # never leaves it, and restricted on the host to wc-session's verbs.
  local pub
  pub=$(g "export MSYS_NO_PATHCONV=1; test -f $DIR_MSYS/id_host || \
        ssh-keygen -t ed25519 -N '' -C wc-launch-guest -f $DIR_MSYS/id_host >/dev/null; \
        cat $DIR_MSYS/id_host.pub" | grep '^ssh-ed25519')
  [ -n "$pub" ] || { echo "error: the guest produced no key"; exit 1; }

  # `scp` over a running binary is ETXTBSY on Linux and a held file handle on
  # Windows, and a session left up from the last run is running exactly these
  # two. Unlinking first replaces the name and leaves the running process on the
  # inode it already has, which is the whole point of the trick.
  h "rm -f $HOSTDIR/bin/wc-proxy $HOSTDIR/bin/wc-arena-host" >/dev/null 2>&1
  scp "${SSHOPT[@]}" "$ROOT/scripts/wc-session" "$HOST:$HOSTDIR/" >/dev/null
  scp "${SSHOPT[@]}" "$LINUX_BIN/wc-proxy" "$locator" "$HOST:$HOSTDIR/bin/" >/dev/null \
    || { echo "error: scp to the VM host"; exit 1; }
  h "chmod 755 $HOSTDIR/wc-session $HOSTDIR/bin/*; \
     mkdir -p ~/.ssh; chmod 700 ~/.ssh; touch ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys; \
     grep -q '$(echo "$pub" | awk '{print $2}')' ~/.ssh/authorized_keys || \
       echo 'restrict,port-forwarding,command=\"$HOSTDIR/wc-session\" $pub' >> ~/.ssh/authorized_keys; \
     grep -c wc-launch-guest ~/.ssh/authorized_keys" >/dev/null
  echo "  copied wc-session, wc-proxy and wc-arena-host to $HOST:$HOSTDIR, and restricted the guest's key to it"

  # And the one end-to-end check of the path that does not need a compositor.
  # Asked of `wc-launch.exe` itself rather than of a hand-built ssh command
  # line, so that what is tested is the argv the product uses: the first version
  # of this check went through two shells, lost every backslash of the key's
  # path, and reported "Permission denied (publickey)" for a file it had never
  # opened.
  g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS && ./wc-launch.exe --ping" >/dev/null
  local said
  said=$(read_clock | grep 'PING' | tail -1)
  case "$said" in
    *"wc-session ok"*) ok "the guest reaches the VM host on its own key: ${said#*PING }" ;;
    *) bad "the guest cannot reach the VM host"; read_clock | tail -5 | sed 's/^/        /' ;;
  esac
}

# --------------------------------------------------------------- install -----
do_install() {
  echo "== install =="
  local out
  out=$(g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS && ./wc-launch.exe --install firefox; echo rc=\$?" | tr -d '\r')
  note "wc-launch --install: $out"
  local logline
  logline=$(g "tail -20 $APPDATA_MSYS/wc-launch-$SESSION.log" | tr -d '\r' | grep 'installed ' | tail -1)
  local lnk="${logline#*installed }"
  if [ -n "$lnk" ]; then
    ok "the shortcut is on the desktop: $lnk"
    g "printf '%s' '$lnk' > $DIR_MSYS/lnk.txt" >/dev/null
  else
    bad "no shortcut was installed"
    g "tail -6 $APPDATA_MSYS/wc-launch-$SESSION.log" | sed 's/^/        /'
    return
  fi
  local iconline
  iconline=$(g "tail -20 $APPDATA_MSYS/wc-launch-$SESSION.log" | tr -d '\r' | grep '^\[.*\] icon: ' | tail -1)
  case "$iconline" in
    *"icon firefox"*) ok "the shortcut wears Firefox's own icon, resolved on the VM host: ${iconline#*icon: }" ;;
    *) bad "the icon did not come across: ${iconline:-<nothing>}" ;;
  esac
}

# ------------------------------------------------------------------ clock ----
# Everything the launcher measured about its own run, read back.
read_clock() {
  g "tail -40 $APPDATA_MSYS/wc-launch-$SESSION.log" | tr -d '\r'
}

session_up() {
  g "export MSYS_NO_PATHCONV=1; test -f $APPDATA_MSYS/wc-launch-$SESSION.state && \
     grep -q '^ready=1' $APPDATA_MSYS/wc-launch-$SESSION.state && echo yes || echo no" | tr -d '\r' | tail -1
}

# Firefox on the VM host, ended between measurements so that every launch below
# is a *first* launch and comparable with the native number. By its command
# line, which contains this launcher's profile directory, so nothing of the
# user's own is touched.
#
# **A killed browser is not a closed browser, and the difference is nine
# seconds.** Warm clicks measured 10.9 s each, three times, against 1.2 s for
# one taken minutes earlier — and the photograph of the tenth-second window says
# why: *"Sorry. We're having trouble getting your pages back."* `pkill` leaves
# the profile looking like a crash, so the next start does session recovery. It
# was a real measurement of a thing nobody would ever do.
#
# So this waits for the process to be gone rather than sleeping a second, and
# `WC_LAUNCH_FRESH=1` removes the profile as well — which is also what makes a
# Linux measurement comparable with the native one, since `native.ps1` wipes its
# profile before every sample.
kill_host_firefox() {
  [ -n "${WC_LAUNCH_FRESH:-}" ] && h "rm -rf ~/.wc-launch/firefox" >/dev/null 2>&1
  h "pkill -f 'wc-launch/firefox' 2>/dev/null
     for _ in \$(seq 1 40); do
       pgrep -f 'wc-launch/firefox' >/dev/null 2>&1 || break
       sleep 0.5
     done
     rm -f ~/.wc-launch/firefox/.parentlock ~/.wc-launch/firefox/lock 2>/dev/null
     true" >/dev/null 2>&1
}

do_stop() {
  echo "== stop =="
  g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS && ./wc-launch.exe --stop" >/dev/null
  kill_host_firefox
  sleep 2
  note "session: $(session_up)"
}

do_click() {
  local kind="$1"
  echo "== $kind click =="
  g "rm -f $APPDATA_MSYS/wc-launch-$SESSION.log" >/dev/null
  local t0; t0=$(date +%s)
  int "wclaunch-click-$kind" "powershell.exe" \
      "-NoProfile -ExecutionPolicy Bypass -File $DIR_WIN\\click.ps1" >/dev/null
  # The scheduler's own latency is outside the number; this only has to outlast
  # it. §16.8: a task costs about a minute to start.
  local line=""
  for _ in $(seq 1 60); do
    sleep 5
    line=$(read_clock | grep 'CLICK-TO-WINDOW' | tail -1)
    [ -n "$line" ] && break
  done
  local waited=$(( $(date +%s) - t0 ))
  if [ -z "$line" ]; then
    bad "$kind: no window in ${waited}s"
    read_clock | sed 's/^/        /' | tail -20
    return 1
  fi
  local ms; ms=$(echo "$line" | grep -o 'CLICK-TO-WINDOW [0-9]*' | awk '{print $2}')
  ok "$kind click to window: ${ms} ms   (the harness waited ${waited}s for the scheduler around it)"
  read_clock | grep -E 'arena:|locate:|proxy:|run Firefox|cold start|session up' | sed 's/^/        /'
  echo "$ms"
}

do_cold()  { do_stop >/dev/null; sleep 3; do_click cold; }
do_warm()  {
  if [ "$(session_up)" != yes ]; then
    note "no session: starting one first, which is the cold path"
    do_click warm-first >/dev/null
  fi
  kill_host_firefox
  sleep 2
  do_click warm
}

do_native() {
  echo "== native Firefox on Windows, on the same machine =="
  g "rm -f $DIR_MSYS/native.txt" >/dev/null
  int "wclaunch-native" "powershell.exe" \
      "-NoProfile -ExecutionPolicy Bypass -File $DIR_WIN\\native.ps1 -Runs 5" >/dev/null
  local out=""
  for _ in $(seq 1 60); do
    sleep 5
    out=$(g "cat $DIR_MSYS/native.txt 2>/dev/null" | tr -d '\r')
    echo "$out" | grep -q '^done$' && break
  done
  if ! echo "$out" | grep -q '^native'; then
    bad "native Firefox produced no measurement"
    return
  fi
  echo "$out" | grep '^native' | sed 's/^/        /'
  # A row with no window handle is a sample that never saw a window and stopped
  # for some other reason; it is dropped rather than averaged in, and the count
  # says how many were kept.
  local rows avg n
  rows=$(echo "$out" | grep '^native' | grep -vE 'hwnd *$|hwnd 0$')
  n=$(echo "$rows" | grep -c 'native')
  avg=$(echo "$rows" | grep -o 'native [0-9]*' | awk '{s+=$2; n++} END {if (n) printf "%d", s/n}')
  if [ "${n:-0}" -lt 2 ]; then
    bad "native Firefox produced only ${n:-0} usable sample(s)"
    return
  fi
  ok "native Firefox on Windows: ${avg} ms to a top-level window (mean of $n with a window handle)"
}

# ------------------------------------------------------------------ shot -----
do_shot() {
  echo "== the window =="
  local cpid
  cpid=$(g "export MSYS_NO_PATHCONV=1; grep '^comp-pid=' $APPDATA_MSYS/wc-launch-$SESSION.state 2>/dev/null" \
         | tr -d '\r' | cut -d= -f2)
  if [ -z "$cpid" ] || [ "$cpid" = 0 ]; then
    bad "no compositor pid in the state file; there is no session to photograph"
    return
  fi
  note "the compositor of this session is pid $cpid"
  mkdir -p "$ROOT/docs/stage14"
  # Deleted first: a shot of a window that is not there leaves the previous
  # shot's file, and the harness then reports a photograph it did not take
  # (§16.8).
  # Retried, and the widest capture wins. `wc-desk shot --pid` takes the *first*
  # window of the process, and a Firefox that has a tooltip or a popup open has
  # more than one — one attempt came back 338x196, which is a real window of the
  # right process and not the browser.
  local best=0 tries
  rm -f "$ROOT/docs/stage14/stage14-firefox-shortcut.bmp"
  for tries in 1 2 3; do
    g "rm -f $DIR_MSYS/window.bmp; printf '%s' 'shot --pid $cpid --out $DIR_WIN\\\\window.bmp' > $DIR_MSYS/shotargs.txt" >/dev/null
    int "wclaunch-shot" "powershell.exe" \
        "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File $DIR_WIN\\shot.ps1" -Wait >/dev/null
    sleep 3
    g "test -s $DIR_MSYS/window.bmp && echo yes" | grep -q yes || continue
    g "base64 $DIR_MSYS/window.bmp" | base64 -d > "$ROOT/docs/stage14/cand.bmp"
    local w=0
    command -v magick >/dev/null 2>&1 && \
      w=$(magick identify -format '%w' "$ROOT/docs/stage14/cand.bmp" 2>/dev/null)
    if [ "${w:-0}" -gt "$best" ]; then
      best=${w:-0}
      mv "$ROOT/docs/stage14/cand.bmp" "$ROOT/docs/stage14/stage14-firefox-shortcut.bmp"
    else
      rm -f "$ROOT/docs/stage14/cand.bmp"
    fi
    [ "$best" -ge 900 ] && break
  done
  if [ ! -s "$ROOT/docs/stage14/stage14-firefox-shortcut.bmp" ]; then
    bad "wc-desk captured nothing"
    return
  fi
  if command -v magick >/dev/null 2>&1; then
    magick "$ROOT/docs/stage14/stage14-firefox-shortcut.bmp" \
           "$ROOT/docs/stage14/stage14-firefox-shortcut.png" 2>/dev/null
    rm -f "$ROOT/docs/stage14/stage14-firefox-shortcut.bmp"
    local colours geo
    colours=$(magick "$ROOT/docs/stage14/stage14-firefox-shortcut.png" -format %k info: 2>/dev/null)
    geo=$(magick identify -format '%wx%h' "$ROOT/docs/stage14/stage14-firefox-shortcut.png" 2>/dev/null)
    # Both halves, because either alone passes something that is not the
    # browser. A "Profile Missing" dialog is 626x182 with 875 distinct colours
    # and satisfies §17.5's colour count exactly; the first run of this stage
    # photographed one and called it a rendered browser.
    local w=${geo%x*}
    if [ "${colours:-0}" -ge 50 ] && [ "${w:-0}" -ge 900 ]; then
      ok "the window is $geo with $colours distinct colours, so it is the browser and not a dialog or a black rectangle"
    else
      bad "the window is $geo with ${colours:-0} distinct colours -- too small or too flat to be Firefox's own"
    fi
  else
    ok "captured docs/stage14/stage14-firefox-shortcut.bmp"
  fi
}

# The desktop itself, so that the icon is evidence rather than a claim.
do_desktop_shot() {
  echo "== the desktop icon =="
  g "rm -f $DIR_MSYS/desktop.bmp; printf '%s' 'shot --screen --out $DIR_WIN\\\\desktop.bmp' > $DIR_MSYS/shotargs.txt" >/dev/null
  int "wclaunch-deskshot" "powershell.exe" \
      "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File $DIR_WIN\\shot.ps1" -Wait >/dev/null
  sleep 3
  if g "test -s $DIR_MSYS/desktop.bmp && echo yes" | grep -q yes; then
    g "base64 $DIR_MSYS/desktop.bmp" | base64 -d > "$ROOT/docs/stage14/stage14-desktop.bmp"
    if command -v magick >/dev/null 2>&1; then
      magick "$ROOT/docs/stage14/stage14-desktop.bmp" "$ROOT/docs/stage14/stage14-desktop.png" 2>/dev/null
      rm -f "$ROOT/docs/stage14/stage14-desktop.bmp"
    fi
    note "docs/stage14/stage14-desktop.png"
  else
    note "no screen capture (the guest's screen is a lock screen, §13)"
  fi
}

# §12.5's discipline: a range across runs rather than one run's number dressed
# as a constant.
do_bench() {
  local n="${WC_LAUNCH_RUNS:-3}" i colds="" warms="" agains=""
  echo "== $n cold, $n warm, $n second-click =="
  for i in $(seq 1 "$n"); do
    colds="$colds $(do_cold 2>/dev/null | tail -1)"
    sleep 2
  done
  for i in $(seq 1 "$n"); do
    warms="$warms $(do_warm 2>/dev/null | tail -1)"
    sleep 2
  done
  # The third measurement, and the one a Windows application is *actually*
  # compared against on a second click: the browser is already running, so this
  # is a new window from the instance that exists rather than a new instance.
  for i in $(seq 1 "$n"); do
    agains="$agains $(do_click again 2>/dev/null | tail -1)"
    sleep 2
  done
  echo
  note "cold, ms:$colds"
  note "warm, ms:$warms"
  note "second click with the browser already up, ms:$agains"
}

# ---------------------------------------------------------------- verify -----
# What the live session is actually made of, asserted from both machines' own
# logs rather than from the fact that a window appeared. Hard requirement 1 --
# guest RAM, always, with the file-backed arena unreachable -- is a claim about
# the transport, and a window proves nothing about the transport.
do_verify() {
  echo "== what carried the pixels =="
  local id
  id=$(g "grep '^id=' $APPDATA_MSYS/wc-launch-$SESSION.state 2>/dev/null" | tr -d '\r' | cut -d= -f2)
  [ -n "$id" ] || { bad "no session"; return; }

  # Asked of the **running process**, by the pid in the session's own state
  # file, rather than read out of a log that the last click truncated. §16.8:
  # an identity is a pid, and this is the one question where the answer has to
  # come from the process that is actually serving the window.
  local cpid cmdline
  cpid=$(g "grep '^comp-pid=' $APPDATA_MSYS/wc-launch-$SESSION.state 2>/dev/null" \
         | tr -d '\r' | cut -d= -f2)
  cmdline=$(g "export MSYS_NO_PATHCONV=1; powershell.exe -NoProfile -Command \
     \"(Get-CimInstance Win32_Process -Filter 'ProcessId = $cpid').CommandLine\"" | tr -d '\r')
  case "$cmdline" in
    *--arena-guest-ram*) ok "the live compositor (pid $cpid) is over guest RAM: $cmdline" ;;
    *) bad "the compositor's command line has no --arena-guest-ram: $cmdline" ;;
  esac
  case "$cmdline" in
    *--shared-dir*|*--arena\ *|*--arena-provide*)
      bad "a file-backed path reached the compositor's command line" ;;
    *) ok "no --shared-dir, no file arena, no --arena-provide: there is no filesystem in this path" ;;
  esac

  local comp
  comp=$(g "cat $APPDATA_MSYS/wc-launch-$SESSION-comp.log" | tr -d '\r')
  local provided
  provided=$(echo "$comp" | grep -o 'arena 0 provided from guest RAM: .*' | tail -1)
  [ -n "$provided" ] && ok "the guest pinned and stamped it: $provided" \
                     || bad "the compositor never reported a guest-RAM arena"

  local hostlog
  hostlog=$(h "SSH_ORIGINAL_COMMAND='logs $id' $HOSTDIR/wc-session" 2>/dev/null)
  local mapped
  mapped=$(echo "$hostlog" | grep -o 'mapped: [0-9]* section, [0-9]* shared-file, [0-9]* arena-slice' | tail -1)
  case "$mapped" in
    "mapped: 0 section, 0 shared-file, "*) ok "every pool crossed as an arena slice: $mapped" ;;
    "") note "the proxy has not printed its transport line yet (it does so on the way out)" ;;
    *) bad "something crossed that was not an arena slice: $mapped" ;;
  esac
  local joins
  joins=$(echo "$hostlog" | grep -c 'joins arena 0')
  local served
  served=$(echo "$hostlog" | grep -c '^\[proxy\] client [0-9]* connected')
  if [ "${served:-0}" -ge 1 ] && [ "$served" = "$joins" ]; then
    ok "$served Wayland connection(s) served, all $joins on one region and one allocator"
  else
    bad "$served connection(s) served, $joins joined the arena"
  fi
  local fds
  fds=$(echo "$hostlog$comp" | grep -o '[0-9]* fds in, [0-9]* out ([0-9]* from the arena)' | tail -1)
  [ -n "$fds" ] && note "descriptors: $fds  (none of them crossed the VM boundary; §14.7)"
  echo "$hostlog" | grep -o '[0-9]* pools ([0-9]* released, [0-9]* resizes, [0-9]* overflows)' | tail -1 \
    | sed 's/^/  ----  Firefox through the arena: /'
}

n=""; warms=""; i=""
case "$MODE" in
  deploy)  do_deploy ;;
  bench)   do_bench ;;
  warmbench)
    n="${WC_LAUNCH_RUNS:-4}"; warms=""
    echo "== $n warm clicks =="
    for i in $(seq 1 "$n"); do warms="$warms $(do_warm 2>/dev/null | tail -1)"; sleep 2; done
    echo; note "warm, ms:$warms" ;;
  hostbase)
    # The control the warm number needs: how long Firefox takes to start on the
    # VM host with **no compositor of ours anywhere near it**.
    echo "== Firefox on the VM host, headless, no Wayland =="
    note "cores: $(h nproc | tr -d '\r')"
    h 'FF=$(ls -d /nix/store/*-firefox-[0-9]*/bin/firefox | sort -V | tail -1)
       for i in 1 2 3; do
         rm -rf /tmp/ffbase$i; mkdir -p /tmp/ffbase$i
         s=$(date +%s%N)
         timeout 90 env -u DISPLAY MOZ_ENABLE_WAYLAND=0 $FF --headless \
           --profile /tmp/ffbase$i --screenshot /tmp/ffshot$i.png about:blank >/dev/null 2>&1
         e=$(date +%s%N)
         echo "headless start to screenshot $(( (e-s)/1000000 )) ms"
       done' | sed 's/^/        /' ;;
  verify)  do_verify ;;
  install) do_install ;;
  cold)    do_cold ;;
  warm)    do_warm ;;
  native)  do_native ;;
  shot)    do_shot; do_desktop_shot ;;
  stop)    do_stop ;;
  clock)   read_clock ;;
  all)     do_deploy; echo; do_install; echo; do_native; echo; do_cold; echo; do_warm; echo; do_shot ;;
  *) echo "usage: $0 deploy|install|cold|warm|bench|native|verify|shot|stop|clock|all"; exit 2 ;;
esac

echo
echo "  $pass passed, $fail failed"
[ "$fail" -eq 0 ]
