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
# Exported as resolved: `wc_topology_declare cross-vm` (wc-topology.sh) checks
# WC_GUEST, and a default that lived only in GUEST made it refuse cross-vm
# unless the caller had happened to set the variable this line defaults.
export WC_GUEST="$GUEST"
HOST="${WC_HOST:-<host-user>@testhost}"
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"

# This stage's own directory on the shared guest, and its own names for
# everything. §16.8: an identity is a path, never an image name.
# `WC_LAUNCH_DIRNAME` so a run can keep out of another's directory on the
# shared guest; the guest's key for the VM host lives there too, so a second
# directory is a second key with its own forced command.
DIRNAME="${WC_LAUNCH_DIRNAME:-wclaunch}"
DIR_WIN="C:\\Users\\<guest-user>\\$DIRNAME"
DIR_MSYS="/c/Users/<guest-user>/$DIRNAME"
SESSION="${WC_LAUNCH_SESSION:-wcl}"
# The shortcut's title, which is also its file name on the desktop.
TITLE="${WC_LAUNCH_TITLE:-Firefox (Linux)}"
# Where `shot` puts its photographs, under docs/, and their file-name prefix.
SHOTDIR="${WC_LAUNCH_SHOTDIR:-stage14}"
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

ARENA_MIB="${WC_LAUNCH_ARENA_MIB:-64}"
OUTPUT="${WC_LAUNCH_OUTPUT:-1600x1000}"
PORT="${WC_LAUNCH_PORT:-57431}"
# §86. The session's sound. The PulseAudio package the VM host's `wc-session`
# runs per session, as a store path that must already be on the host (`nix copy`
# it there); deploy pins it with a GC root at $HOSTDIR/bin/pulse. Unset or absent,
# sessions have no audio server and Firefox gets a null audio context.
PULSE="${WC_LAUNCH_PULSE:-}"
AUDIO_VOLUME="${WC_LAUNCH_AUDIO_VOLUME:-100}"
# §86. The forwarded GPU, **off unless asked for**. WC_LAUNCH_GPU_SESSIONS=N
# (1..8) builds the GPU half into sv-guest.exe (release), copies sv-hostd and the
# ICD to the host, and writes `gpu-sessions = N` into the ini. gpu.env itself is
# written on the host only with WC_LAUNCH_GPU_ENV=write; otherwise deploy prints
# what it would contain. Either way the host also needs `sv-hostd broker`
# running as root (docs/waycompose-design.md §86 has the command).
GPU_SESSIONS="${WC_LAUNCH_GPU_SESSIONS:-0}"
GPU_ENV_MODE="${WC_LAUNCH_GPU_ENV:-print}"
KOPPER_MESA="${WC_LAUNCH_KOPPER_MESA:-}"
REL_WIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/release"
REL_LINUX="${CARGO_TARGET_DIR:-$ROOT/target}/release"

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

# STAGE 42. `fresh` and `deployed_is`. Sourced after the reporters and `g`/`h`.
. "$(dirname "$0")/wc-binaries.sh"
# STAGE 53. The substrate and the topology, which this harness measured for
# four stages without ever declaring either.
#
# It is the harness that most needed them and the last one to get them, and the
# reason is worth naming: every *other* guest harness locates the arena itself
# through `wc_locate_arena`, so sourcing `wc-substrate.sh` was how it got the
# locate and the declaration came along with it. This one does not locate
# anything — `wc-launch.exe` asks `wc-session` on the VM host, which runs
# `wc-arena-host` there — so there was no function it had to call, and therefore
# nothing that made the omission visible. A gate you reach for by accident is
# not a gate; §51 is the same mistake made structural, and this is the harness
# that structure had not reached.
#
# `WC_GUEST_CONFIGURED` is set because this harness ssh's into a guest, and
# setting it is what makes `wc_substrate_declare wine-fixture` **refuse** here.
# The launcher could not open a file arena anyway — `wc-session` spells
# `--transport arena --arena-plan` in its own body and has no verb that names a
# file (§34.7) — so this is the belt to that braces: the escape hatch is never
# handed out on this path, by two independent mechanisms that would each have to
# fail.
WC_GUEST_CONFIGURED=1
. "$(dirname "$0")/wc-substrate.sh"
. "$(dirname "$0")/wc-topology.sh"

# What every leg below runs on, declared before any of them runs.
#
# Called once, at the top, rather than per-leg: unlike `wc-guest.sh` — where
# `do_damage` is one leg among twelve and the others are Wine — *every* verb in
# this harness drives the same guest through the same launcher, so there is no
# leg here for which a different answer could be correct.
wcl_declare() {
  # IVSHMEM ARENA (§83): the launcher's arena is BAR2 of the VM's ivshmem
  # device, and the developer harnesses keep guest RAM.
  wc_substrate_declare ivshmem || exit 2
  wc_topology_declare cross-vm \
    "sv-guest.exe on $GUEST, wc-proxy on $HOST as the user, arena on the VM's ivshmem device" || exit 2
}

# The declaration checked against the run's own evidence, from the one line the
# launcher already prints.
#
# `wc_assert_topology` wants `WC_SUBSTRATE_LOCATED`, which `wc_locate_arena`
# sets — and this harness never calls it, because the locate happens on the VM
# host under `wc-session`. So the same fact is taken from where it does exist:
# the launcher's own `locate:` line, which carries `located N of N pages` and
# **which locator produced them**. That line is the guest's report of a locate
# it did not perform and could not fake: the numbers come back from
# `wc-arena-host` on the far side of the ssh channel.
#
# Reading the locator from the line rather than assuming it is the point of
# §34.2. `frames=none` is a legitimate outcome — the sweep works, it merely
# costs seconds — so this records which arm ran instead of requiring one.
wcl_assert_located() {
  # IVSHMEM ARENA (§83). There is no locate any more, so the evidence is the
  # header instead: the run id the compositor printed (`arena: run …`, read off
  # the guest) has to be the run id `wc-session up` read out of the file QEMU
  # backs the BAR with (`up: ivshmem run …`, read off the VM host and reported
  # back by the launcher). Two machines agreeing on 64 random bits that only one
  # of them chose, through a file only QEMU's device writes, is the proof.
  local clock guest_run line host_run len
  clock=$(read_clock)
  guest_run=$(echo "$clock" | grep -o 'arena: run 0x[0-9a-f]*' | tail -1 | awk '{print $3}')
  line=$(echo "$clock" | grep 'up: ' | tail -1)
  host_run=$(echo "$line" | grep -o 'ivshmem run 0x[0-9a-f]*' | awk '{print $3}')
  len=$(echo "$line" | grep -oE '[0-9]+ bytes at' | awk '{print $1}')
  if [ -z "$guest_run" ] || [ -z "$host_run" ]; then
    bad "no ivshmem header was matched: guest said '${guest_run:-nothing}', host said '${line:-nothing}'"
    return 1
  fi
  if [ "$guest_run" != "$host_run" ]; then
    bad "the guest's compositor is run $guest_run and the host's header says $host_run"
    return 1
  fi
  WC_SUBSTRATE_LOCATED=$(( ${len:-0} / 4096 - 1 ))
  WC_SUBSTRATE_LOCATOR="the ivshmem header (run $host_run)"
  ok "the arena is the ivshmem device: run $guest_run on the guest is the header's run on $HOST, ${len:-?} bytes"
  wc_assert_topology "/dev/null" "launch"
}

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

  # STAGE 44. **Three builds whose success was never established.** Each is a
  # bare pipeline in a script with no `set -e`, so its status was simply not
  # looked at — and looking at it would not have helped, which is the part worth
  # knowing: with `pipefail` (set on line 49) `cargo … | grep '^error' | head`
  # returns 1 when the build *succeeds*, because grep matching nothing is a 1.
  # Success and failure are both `rc=1`; only `${PIPESTATUS[0]}` tells them
  # apart, and it has to be read on the very next line.
  #
  # §42.3 got the mechanism wrong here and §44.2 says so. It claimed the
  # pipeline "gives grep's exit status" — true without `pipefail`, and no
  # harness in this tree is without it.
  echo "  building the Windows half"
  cargo build --target x86_64-pc-windows-gnu \
        -p wc-launch -p wc-desk 2>&1 | grep -E "^error" | head -5
  built_ok "${PIPESTATUS[0]}" "wc-launch and wc-desk for Windows" || exit 1
  cargo build --target x86_64-pc-windows-gnu -p sv-guest --no-default-features -F wayland 2>&1 | grep -E '^error|warning: unused' | head -5
  built_ok "${PIPESTATUS[0]}" "sv-guest for Windows" || exit 1
  # And freshness on top, because a build that succeeded without recompiling
  # anything is also possible: `cargo` is content when the tree has not changed,
  # and this check is about the tree rather than about cargo. §35.4.
  local e
  for e in wc-launch.exe sv-guest.exe wc-desk.exe; do
    fresh "$WIN_BIN/$e" "cargo build --target x86_64-pc-windows-gnu" || exit 1
  done

  echo "  building the Linux half"
  # `wc-arena-host` is an ordinary workspace member now rather than a crate with its
  # own manifest and its own lock file: the repositories are one, so the absolute
  # path dependency that kept it excluded no longer exists (one-binary.md §1.3). The
  # merge deleted it outright; stage 14 put it back, because nothing replaced it.
  cargo build -p wc-proxy -p wc-arena-host 2>&1 | grep -E '^error' | head -5
  built_ok "${PIPESTATUS[0]}" "wc-proxy and wc-arena-host" || exit 1
  local locator="$LINUX_BIN/wc-arena-host"
  fresh "$LINUX_BIN/wc-proxy" "cargo build -p wc-proxy" || exit 1
  fresh "$locator"            "cargo build -p wc-arena-host" || exit 1
  for b in "$LINUX_BIN/wc-proxy" "$locator"; do
    [ -x "$b" ] || { echo "error: no $b"; exit 1; }
  done

  local stage; stage=$(mktemp -d /tmp/wc-launch-stage-XXXXXX)
  trap 'rm -rf "$stage"' RETURN
  x86_64-w64-mingw32-gcc -O2 -o "$stage/probe-relay.exe" "$ROOT/scripts/probe-relay.c" -lws2_32 \
    || { echo "error: probe-relay.exe did not build"; exit 1; }
  cp "$WIN_BIN/wc-launch.exe" "$WIN_BIN/sv-guest.exe" "$WIN_BIN/wc-desk.exe" "$stage/"
  if [ "$GPU_SESSIONS" -gt 0 ]; then
    # One binary, both halves: the GPU side is `sv-guest.exe gpu guest`. Release,
    # because a debug ICD overflowed Firefox's renderer stack (video-decode.md
    # §7.7) and both halves of the wire must come from one build.
    echo "  building sv-guest.exe with its GPU half, and sv-hostd and the ICD (release)"
    cargo build --release --target x86_64-pc-windows-gnu -p sv-guest 2>&1 | grep -E '^error' | head -5
    built_ok "${PIPESTATUS[0]}" "sv-guest (gpu + wayland) for Windows" || exit 1
    cargo build --release -p sv-hostd -p sv-icd 2>&1 | grep -E '^error' | head -5
    built_ok "${PIPESTATUS[0]}" "sv-hostd and sv-icd" || exit 1
    cp "$REL_WIN/sv-guest.exe" "$stage/sv-guest.exe"
  fi
  write_runint "$stage/runint.ps1"
  write_click  "$stage/click.ps1"
  write_native "$stage/native.ps1"
  write_shot   "$stage/shot.ps1"
  # The scripts name their own directory literally; a run in another one
  # (`WC_LAUNCH_DIRNAME`) gets its own spelled in.
  [ "$DIRNAME" = wclaunch ] || sed -i "s/<guest-user>\\\\wclaunch/<guest-user>\\\\$DIRNAME/g" "$stage"/*.ps1

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
audio      = ${WC_LAUNCH_AUDIO:-on}
audio-port = $((PORT + 1))
audio-volume = $AUDIO_VOLUME
gpu-sessions = $GPU_SESSIONS

[app.firefox]
title = $TITLE
icon  = firefox
exec  = $FIREFOX --profile ~/.wc-launch/firefox
prefs = middlemouse.paste=false${WC_LAUNCH_EXTRA_PREFS:+;$WC_LAUNCH_EXTRA_PREFS}
# §82: the default, named so the deployed ini says which arrangement it ran.
# WC_LAUNCH_TITLEBAR=none runs the opt-out.
titlebar = ${WC_LAUNCH_TITLEBAR:-tabs}
INI

  # A running .exe holds its own file open, so a session that is up must end
  # before its binaries can be replaced. By path, not by image name.
  g "export MSYS_NO_PATHCONV=1; mkdir -p $DIR_MSYS; \
     powershell.exe -NoProfile -Command \"Get-CimInstance Win32_Process | \
       Where-Object { \\\$_.ExecutablePath -and \\\$_.ExecutablePath.StartsWith('$DIR_WIN\\\\') } | \
       ForEach-Object { Stop-Process -Id \\\$_.ProcessId -Force -ErrorAction SilentlyContinue }\"" \
    >/dev/null 2>&1
  sleep 1
  scp "${SSHOPT[@]}" "$stage"/* "$GUEST:$DIRNAME/" >/dev/null || { echo "error: scp to the guest"; exit 1; }
  echo "  copied wc-launch.exe, sv-guest.exe, wc-desk.exe, probe-relay.exe and the ini to $DIR_WIN"
  # STAGE 42. `sv-guest.exe` is the GPU side, which is the binary the
  # wire-version refusal is actually about: it is *copied* here, so rebuilding
  # on this machine changes only this side. `finish_attach` refuses the skew
  # with "This is a version skew between two builds, not a transport fault",
  # and `WIRE_VERSION` went 1 -> 2 while this was written. Checked before the
  # run rather than diagnosed from a handshake failure during it.
  deployed_all "$stage" "$DIR_MSYS" g "re-run $0 deploy with nothing of ours running on the guest" \
    wc-launch.exe sv-guest.exe wc-desk.exe || exit 1

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
  # STAGE 42, the host boundary. The unlink trick above means `scp` cannot fail
  # on a live binary any more, which removes the loud failure and leaves only
  # the quiet one: a copy that did not land where the run will look for it.
  deployed_is "$LINUX_BIN/wc-proxy" "$HOSTDIR/bin/wc-proxy"      h "re-run $0 deploy" || exit 1
  deployed_is "$locator"            "$HOSTDIR/bin/wc-arena-host" h "re-run $0 deploy" || exit 1
  h "chmod 755 $HOSTDIR/wc-session $HOSTDIR/bin/*; \
     mkdir -p ~/.ssh; chmod 700 ~/.ssh; touch ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys; \
     grep -q '$(echo "$pub" | awk '{print $2}')' ~/.ssh/authorized_keys || \
       echo 'restrict,port-forwarding,command=\"$HOSTDIR/wc-session\" $pub' >> ~/.ssh/authorized_keys; \
     grep -c wc-launch-guest ~/.ssh/authorized_keys" >/dev/null
  echo "  copied wc-session, wc-proxy and wc-arena-host to $HOST:$HOSTDIR, and restricted the guest's key to it"
  if [ "$GPU_SESSIONS" -gt 0 ]; then
    h "rm -f $HOSTDIR/bin/sv-hostd $HOSTDIR/bin/libsv_icd.so" >/dev/null 2>&1
    scp "${SSHOPT[@]}" "$REL_LINUX/sv-hostd" "$REL_LINUX/libsv_icd.so" "$HOST:$HOSTDIR/bin/" >/dev/null \
      || { echo "error: scp of the GPU half to the VM host"; exit 1; }
    h "printf '{\"file_format_version\":\"1.0.1\",\"ICD\":{\"library_path\":\"%s\",\"api_version\":\"1.4.0\",\"library_arch\":\"64\",\"is_portability_driver\":false}}\\n' $HOSTDIR/bin/libsv_icd.so >$HOSTDIR/bin/sv_icd.json"
    deployed_is "$REL_LINUX/sv-hostd" "$HOSTDIR/bin/sv-hostd" h "re-run $0 deploy" || exit 1
    local genv
    genv="MESA_LOADER_DRIVER_OVERRIDE=zink
LIBGL_KOPPER_DRI2=1
__EGL_VENDOR_LIBRARY_FILENAMES=${KOPPER_MESA:-<mesa-kopper-wayland>}/share/glvnd/egl_vendor.d/50_mesa.json
LIBGL_DRIVERS_PATH=${KOPPER_MESA:-<mesa-kopper-wayland>}/lib/dri
VK_DRIVER_FILES=$HOSTDIR/bin/sv_icd.json"
    if [ "$GPU_ENV_MODE" = write ] && [ -n "$KOPPER_MESA" ]; then
      printf '%b\n' "$genv" | h "cat >$HOSTDIR/gpu.env"
      echo "  wrote $HOSTDIR/gpu.env: sessions with GPU sides render on the guest's GPU"
    else
      note "gpu.env NOT written (WC_LAUNCH_GPU_ENV=write and WC_LAUNCH_KOPPER_MESA enable it); it would be:"
      printf '%b\n' "$genv" | sed 's/^/          /'
    fi
  fi
  if [ -n "$PULSE" ]; then
    if h "nix-store -r $PULSE --add-root $HOSTDIR/bin/pulse --indirect >/dev/null && test -x $HOSTDIR/bin/pulse/bin/pulseaudio"; then
      echo "  sessions get a PulseAudio null sink from $PULSE (GC root $HOSTDIR/bin/pulse)"
    else
      bad "$PULSE is not on $HOST: nix copy it there first; sessions will have no sound"
    fi
  else
    note "WC_LAUNCH_PULSE unset: wc-session looks for pulseaudio in the host's store, and without one Firefox gets a null audio context"
  fi

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
  wcl_declare
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
  # **§35: the milliseconds are informational and the check is on the number's
  # own coherence.** This was `ok "…${ms} ms…"` unconditionally, so it passed at
  # any value including none at all — `grep` finding no digits yields an empty
  # string and the line still read as a pass.
  #
  # It is deliberately *not* given a ceiling. Click-to-window on a contended
  # guest is a rate, and §35.3 records what asserting on a rate costs: the same
  # tree measured 37 us and 103 us for one 244 KiB copy. A bound loose enough to
  # survive that catches nothing; a tight one fails on load.
  #
  # What can be asserted, and cannot be defeated by load, is that the number is
  # real: positive, and **no larger than the wall time the harness itself
  # waited**. A guest-side clock reporting more milliseconds than elapsed out
  # here is a broken clock, which is the one way this measurement can be wrong
  # rather than merely slow — and §13.8 is the retracted throughput number that
  # says an instrument nobody sanity-checks will eventually publish one.
  local ms; ms=$(echo "$line" | grep -o 'CLICK-TO-WINDOW [0-9]*' | awk '{print $2}')
  if [ -z "$ms" ] || [ "$ms" -lt 1 ]; then
    bad "$kind: the clock line carried no usable figure: ${line}"
  elif [ "$ms" -gt $(( (waited + 5) * 1000 )) ]; then
    bad "$kind: the guest reports ${ms} ms but only ${waited}s elapsed here; the clock is wrong, not slow"
  else
    ok "$kind click to window: ${ms} ms, inside the ${waited}s the harness waited (the figure itself is informational: it is a rate on a contended guest)"
  fi
  read_clock | grep -E 'arena:|locate:|up:|proxy:|run Firefox|cold start|session up' | sed 's/^/        /'
  # Only a click that started the session has a locate of its own to check; a
  # warm click joins one and its `locate:` line belongs to the cold click that
  # came before, which this harness has already deleted the log of.
  case "$kind" in cold|warm-first) wcl_assert_located ;; esac
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
  # §35, as above: the mean is informational — it is the control for a rate —
  # but a mean of nothing is not. `awk` prints an empty string when it divided
  # no samples, and the old line reported that as a pass with a blank where the
  # number should be.
  if [ -z "$avg" ] || [ "$avg" -lt 1 ]; then
    bad "native Firefox: $n sample(s) kept but no usable mean came out of them"
  else
    ok "native Firefox on Windows: ${avg} ms to a top-level window (mean of $n with a window handle; the figure is the control for a rate and is informational)"
  fi
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
  mkdir -p "$ROOT/docs/$SHOTDIR"
  # Deleted first: a shot of a window that is not there leaves the previous
  # shot's file, and the harness then reports a photograph it did not take
  # (§16.8).
  # Retried, and the widest capture wins. `wc-desk shot --pid` takes the *first*
  # window of the process, and a Firefox that has a tooltip or a popup open has
  # more than one — one attempt came back 338x196, which is a real window of the
  # right process and not the browser.
  local best=0 tries
  rm -f "$ROOT/docs/$SHOTDIR/$SHOTDIR-firefox-shortcut.bmp"
  for tries in 1 2 3; do
    g "rm -f $DIR_MSYS/window.bmp; printf '%s' 'shot --pid $cpid --out $DIR_WIN\\\\window.bmp' > $DIR_MSYS/shotargs.txt" >/dev/null
    int "wclaunch-shot" "powershell.exe" \
        "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File $DIR_WIN\\shot.ps1" -Wait >/dev/null
    sleep 3
    g "test -s $DIR_MSYS/window.bmp && echo yes" | grep -q yes || continue
    g "base64 $DIR_MSYS/window.bmp" | base64 -d > "$ROOT/docs/$SHOTDIR/cand.bmp"
    local w=0
    command -v magick >/dev/null 2>&1 && \
      w=$(magick identify -format '%w' "$ROOT/docs/$SHOTDIR/cand.bmp" 2>/dev/null)
    if [ "${w:-0}" -gt "$best" ]; then
      best=${w:-0}
      mv "$ROOT/docs/$SHOTDIR/cand.bmp" "$ROOT/docs/$SHOTDIR/$SHOTDIR-firefox-shortcut.bmp"
    else
      rm -f "$ROOT/docs/$SHOTDIR/cand.bmp"
    fi
    [ "$best" -ge 900 ] && break
  done
  if [ ! -s "$ROOT/docs/$SHOTDIR/$SHOTDIR-firefox-shortcut.bmp" ]; then
    bad "wc-desk captured nothing"
    return
  fi
  if command -v magick >/dev/null 2>&1; then
    magick "$ROOT/docs/$SHOTDIR/$SHOTDIR-firefox-shortcut.bmp" \
           "$ROOT/docs/$SHOTDIR/$SHOTDIR-firefox-shortcut.png" 2>/dev/null
    rm -f "$ROOT/docs/$SHOTDIR/$SHOTDIR-firefox-shortcut.bmp"
    local colours geo
    colours=$(magick "$ROOT/docs/$SHOTDIR/$SHOTDIR-firefox-shortcut.png" -format %k info: 2>/dev/null)
    geo=$(magick identify -format '%wx%h' "$ROOT/docs/$SHOTDIR/$SHOTDIR-firefox-shortcut.png" 2>/dev/null)
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
    ok "captured docs/$SHOTDIR/$SHOTDIR-firefox-shortcut.bmp"
  fi
}

# The desktop itself, so that the icon is evidence rather than a claim.
do_desktop_shot() {
  echo "== the desktop icon =="
  g "rm -f $DIR_MSYS/desktop.bmp; printf '%s' 'shot --screen --out $DIR_WIN\\\\desktop.bmp' > $DIR_MSYS/shotargs.txt" >/dev/null
  int "wclaunch-deskshot" "powershell.exe" \
      "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File $DIR_WIN\\shot.ps1" -Wait >/dev/null
  sleep 3
  # STAGE 42. This writes a **checked-in photograph** — the evidence that the
  # shortcut is on the desktop — and both arms were `note`, so nothing asserted
  # anything about it. A 1x1 or all-black capture landed in `docs/stage14/` and
  # the run said "docs/$SHOTDIR/$SHOTDIR-desktop.png" either way. Informational is
  # a legitimate state for a number (see `wc-proxy.sh`'s audit block) but not
  # for the artefact a section cites as its evidence.
  #
  # The absent case stays a `note`: a locked guest genuinely cannot be
  # photographed (§13), and that is a fact about the machine rather than a
  # failure of this stage.
  if g "test -s $DIR_MSYS/desktop.bmp && echo yes" | grep -q yes; then
    g "base64 $DIR_MSYS/desktop.bmp" | base64 -d > "$ROOT/docs/$SHOTDIR/$SHOTDIR-desktop.bmp"
    if command -v magick >/dev/null 2>&1; then
      magick "$ROOT/docs/$SHOTDIR/$SHOTDIR-desktop.bmp" "$ROOT/docs/$SHOTDIR/$SHOTDIR-desktop.png" 2>/dev/null
      rm -f "$ROOT/docs/$SHOTDIR/$SHOTDIR-desktop.bmp"
      picture_is "$ROOT/docs/$SHOTDIR/$SHOTDIR-desktop.png" screen "the desktop with the shortcut on it -> docs/$SHOTDIR/$SHOTDIR-desktop.png"
    else
      note "docs/$SHOTDIR/$SHOTDIR-desktop.bmp (no imagemagick here, so nothing checked the pixels)"
    fi
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
    *--arena-guest-ram*) bad "the live compositor (pid $cpid) is over guest RAM, which the host reaches only as root: $cmdline" ;;
    *--arena-ivshmem*) ok "the live compositor (pid $cpid) is over the ivshmem device: $cmdline" ;;
    *) bad "the compositor's command line has no --arena-ivshmem: $cmdline" ;;
  esac
  case "$cmdline" in
    *--shared-dir*|*--arena\ *|*--arena-provide*)
      bad "a file-backed path reached the compositor's command line" ;;
    *) ok "no --shared-dir, no file arena, no --arena-provide: there is no filesystem in this path" ;;
  esac

  local comp
  comp=$(g "cat $APPDATA_MSYS/wc-launch-$SESSION-comp.log" | tr -d '\r')
  local provided
  provided=$(echo "$comp" | grep -o 'arena 0 provided from ivshmem: .*' | tail -1)
  [ -n "$provided" ] && ok "the guest mapped the device: $provided" \
                     || bad "the compositor never reported an ivshmem arena"

  # IVSHMEM ARENA (§83): **who the proxy runs as**, asked of the process rather
  # than of the script that started it. The unit is this session's own user
  # unit, and its main pid's owner is the claim.
  local puser
  puser=$(h "XDG_RUNTIME_DIR=/run/user/\$(id -u) systemctl --user show -p MainPID --value wc-launch-$id 2>/dev/null \
             | xargs -r ps -o user= -o pid= -o args= -p" 2>/dev/null | head -1)
  case "$puser" in
    root*) bad "wc-proxy runs as root: $puser" ;;
    sirati*) ok "wc-proxy runs as the user, not root: $puser" ;;
    *) bad "could not find this session's wc-proxy (unit wc-launch-$id): ${puser:-nothing}" ;;
  esac

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
  # **§35: the descriptor line is checked, not echoed.** The `note` beside it
  # asserted the property in *prose* — "none of them crossed the VM boundary" —
  # and the code did nothing with the numbers that would show it. A `note` whose
  # text makes a claim is the most misleading form of this defect, because a
  # reader takes the sentence for the check.
  #
  # What is assertable without inventing a bound: the line must exist at all
  # (its absence meant silence), and **every descriptor out must have come from
  # the arena**. `N from the arena` is the count this side manufactured by
  # resolving an offset rather than by receiving anything; if `out` exceeds it,
  # something was handed to a client that was not cut from guest RAM, which is
  # exactly the boundary claim the sentence makes.
  local fds fds_out fds_arena
  fds=$(echo "$hostlog$comp" | grep -o '[0-9]* fds in, [0-9]* out ([0-9]* from the arena)' | tail -1)
  if [ -z "$fds" ]; then
    bad "the proxy reported no descriptor counts; §14.7's claim is unchecked for this run"
  else
    fds_out=$(echo "$fds" | grep -o '[0-9]* out' | grep -o '[0-9]*')
    fds_arena=$(echo "$fds" | grep -o '([0-9]* from the arena)' | grep -o '[0-9]*')
    if [ "${fds_out:-0}" -eq "${fds_arena:-0}" ]; then
      ok "descriptors: $fds -- every one handed out was cut from the arena, so none crossed (§14.7)"
    else
      bad "descriptors: $fds -- ${fds_out} handed out but only ${fds_arena} from the arena; the rest came from somewhere else"
    fi
  fi

  # **And the pool line, which was piped through `sed` into something that looks
  # like a `note` and is not even that.** `wc-guest.sh` asserts exactly this
  # shape — `*"0 overflows)"` — and has since stage 9; here the same numbers
  # were formatted for a reader. An overflow is a client that asked for arena
  # space and was refused, which is the failure §20 exists about, and it printed
  # as a decorated line.
  # Read by field name: stage 16 added `reclaimed`/`republished` inside the
  # parentheses and the positional pattern stopped matching (wc-guest.sh has
  # the same fix and the longer story).
  local pools overflows
  pools=$(echo "$hostlog" | grep -o '[0-9]* pools ([^)]*)' | tail -1)
  overflows=$(echo "$pools" | grep -o '[0-9]* overflows' | tr -dc 0-9)
  if [ -z "$pools" ] || [ -z "$overflows" ]; then
    bad "the proxy reported no pool counts; the arena's own accounting is unchecked"
  elif [ "$overflows" -eq 0 ]; then
    ok "Firefox through the arena: $pools -- every resize was absorbed"
  else
    bad "Firefox through the arena: $pools -- an overflow is a client refused arena space"
  fi
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
