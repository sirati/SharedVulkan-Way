#!/usr/bin/env bash
# Stage 32: two programs, two taskbar buttons — photographed on the guest.
#
#   scripts/wc-taskbar.sh deploy   cross-compile and copy to the guest
#   scripts/wc-taskbar.sh local    two clients of ours, three runs, no host needed
#   scripts/wc-taskbar.sh apps     two *real* Linux applications through the proxy
#   scripts/wc-taskbar.sh all      deploy, local, apps
#
# Run from inside `nix develop`. Everything lands in docs/stage32/.
#
# THE REQUIREMENT
#
# "if two different programs are run on windows via wayland, they must not share
# a taskbar spot". Every window here is an `HWND` made by one process, and the
# shell groups buttons by process, so before this stage they all shared one.
# The fix is a per-window Application User Model ID (`wc-compositor`'s
# `taskbar.rs` and `win32.rs`); this is the harness that shows it happened.
#
# WHY THERE ARE TWO EXPERIMENTS AND NOT ONE
#
#   * `local` runs two copies of **our own** client, `wc-shmclient.exe`, on the
#     guest itself. Same executable, same command line except `--app-id`, so the
#     only thing that can split the buttons is the identity. It also runs the
#     two cases that are easy to *break* while fixing the first — two windows of
#     one application, which must stay on ONE button, and an app id that arrives
#     after the window is already up.
#   * `apps` runs `weston-terminal` and `weston-flower` — two genuinely
#     different programs, unmodified, on the Linux host, through wc-proxy and a
#     hypervisor. That is the picture the requirement asks for; the identities
#     are their own (`org.freedesktop.weston.wayland-terminal` and
#     `…flower`) and so are their icons, which are resolved from the host's
#     own files.
#
# Neither is a substitute for the other: the first cannot show two *programs*,
# and the second cannot isolate the variable.
#
# WHAT IS MEASURED, AS WELL AS PHOTOGRAPHED
#
# `wc-desk windows` reads `PKEY_AppUserModel_ID` back off every window on the
# desktop with `SHGetPropertyStoreForWindow` and prints `windows.groups`: how
# many distinct taskbar buttons the desktop's windows come to, an identity for
# each window that has one and its *process* for each that does not. A
# photograph shows the buttons; that line says what the shell is grouping by,
# and the two together are what makes this evidence rather than an impression.
#
# GUEST HYGIENE — §13.9's rules, which are not optional here
#
# A dozen agents share this guest. So: our own executable name, our own
# directory, our own socket names, processes identified by pid, and never a
# `taskkill /IM` (without `/F` it sends WM_CLOSE to *other people's* windows,
# which is how §16 lost three runs).
#
# STAGE 41. WHICH MEMORY THE `apps` PHOTOGRAPHS WERE TAKEN ON
#
# `local` runs two of our own clients on the guest itself, with no proxy and no
# arena, and is unaffected by any of this. `apps` is the block the requirement
# is actually about, and until §41 it had **no guest-RAM path at all**: its
# pools were `--transport shared-file` on the virtio-fs export and its
# compositor was started with `--arena 0:N:Z:\wc32\arena`, a file on that same
# export. So every taskbar-identity photograph in docs/stage32/ crossed the
# substrate §10.3 measured incoherent for live mappings — 65280 of 65536 bytes
# wrong, in both directions — on a harness that never said so.
#
# The identity result is not thereby wrong: two buttons come apart because of
# `PKEY_AppUserModel_ID` and not because of what carried the pixels. But it was
# taken on the fixture, and a run that cannot say which memory it used is a run
# nobody can check. §41 gives `apps` the guest-RAM path, makes it the only one
# reachable, and turns the substrate into an assertion. The old photographs are
# **not** re-taken here — that is somebody else's measurement to make, and
# §32.9 records what is on the fixture so the choice is an informed one.
set -uo pipefail

MODE="${1:-all}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
OUT="$ROOT/docs/stage32"

GUEST="${WC_GUEST:-<guest-user>@<guest-ip>}"
# STAGE 41. This harness ssh's into a real Windows guest, which is the fact
# `wc_substrate_declare` needs in order to refuse to hand this run the
# file-arena escape hatch. Set before the library is sourced, because it reads it.
WC_GUEST_CONFIGURED=1
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"
GUESTNAME="${WC_GUESTNAME:-wctb32}"
COMP_EXE="${WC_COMP_EXE:-wc-s32-compositor.exe}"
CLIENT_EXE="${WC_CLIENT_EXE:-wc-s32-shmclient.exe}"
DESKDIR="/c/Users/<guest-user>/$GUESTNAME"
DESKDIR_WIN="C:\\Users\\<guest-user>\\$GUESTNAME"
APPDATA_WIN='C:\Users\<guest-user>\AppData\Local\waycompose'
PORT=$(( ${WC_PORT_BASE:-57332} + ($$ % 60) * 2 ))

HOST="${WC_HOST:-}"
HOST_DIR="${WC_HOST_DIR:-/tmp/wc-stage32}"
SHARE="${WC_HOST_SHARE:-/srv/winshare/wc32}"
SHARE_WIN="${WC_GUEST_SHARE:-Z:\\wc32}"
HOST_GUEST="${WC_HOST_GUEST:-<guest-user>@<guest-nat-ip>}"
HOST_KEY="${WC_HOST_KEY:-\$HOME/.ssh/wc_guest}"
HOST_WESTON="${WC_HOST_WESTON:-}"
HOST_ICONS="${WC_HOST_ICONS:-/run/current-system/sw/share}"
# The two real applications, and their app ids, which are the clients' own and
# were read out of the binaries rather than guessed:
#
#   weston-terminal  org.freedesktop.weston.wayland-terminal
#   weston-flower    org.freedesktop.weston.flower
#   weston-editor    org.freedesktop.weston.text-editor   (see §32.7: it
#                    connects, allocates one pool and exits on this host, so it
#                    is not the default)
APP_A="${WC_APP_A:-weston-terminal}"
APP_A_ID="${WC_APP_A_ID:-org.freedesktop.weston.wayland-terminal}"
APP_A_ICON="${WC_APP_A_ICON:-qemu}"
APP_B="${WC_APP_B:-weston-flower}"
APP_B_ID="${WC_APP_B_ID:-org.freedesktop.weston.flower}"
APP_B_ICON="${WC_APP_B_ICON:-}"

# STAGE 41. The three settings the guest-RAM locate needs, spelled exactly as
# `wc-guest.sh` and `wc-desktop.sh` spell them so that one export in a shell
# configures every harness. `wc-arena-host` reads QEMU's QMP monitor and
# `/proc/<qemu-pid>/fd`, and QEMU is a *system* service on this VM host, so both
# are root-only — §12.9, which records that as a property of how testhost
# starts the VM rather than of the design. No descriptor crosses the boundary in
# either direction: what comes back is a plan, which is a list of offsets into
# an object the host opens by name for itself.
QMP="${WC_QMP:-/run/windows-vm/qmp.sock}"
VM_HINT="${WC_VM_HINT:-windows-vm}"
ROOTC="${WC_ROOTC:-\$HOME/rootc}"

SSHOPT=(-o IdentityAgent="$AGENT" -o BatchMode=yes -o LogLevel=ERROR
        -o StrictHostKeyChecking=accept-new)

RUNDIR=$(mktemp -d /tmp/wc-tb-XXXXXX); chmod 700 "$RUNDIR"
trap 'rm -rf "$RUNDIR"' EXIT

pass=0; fail=0; skip=0
ok()   { echo "  PASS  $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail + 1)); }
note() { echo "  ----  $*"; }
miss() { echo "  SKIP  $*"; skip=$((skip + 1)); }

g() { ssh "${SSHOPT[@]}" "$GUEST" "$@"; }
h() { [ -n "$HOST" ] && ssh "${SSHOPT[@]}" "$HOST" "$@"; }

# STAGE 41. Sourced after `ok`/`bad`/`note`/`g`/`h` exist, because the library
# uses all five.
. "$(dirname "$0")/wc-substrate.sh"

# Run a command line on the interactive desktop and wait for it.
int() {
  local name="$1" exe="$2" argstr="$3"
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\runint.ps1' -Exe '$exe' -Arguments '$argstr' \
       -Name '$name' -WorkDir '$DESKDIR_WIN' -Wait -TimeoutSec 120" 2>&1
}

# The same, without waiting: for the compositor, which has to outlive the call.
int_bg() {
  local name="$1" exe="$2" argstr="$3"
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\runint.ps1' -Exe '$exe' -Arguments '$argstr' \
       -Name '$name' -WorkDir '$DESKDIR_WIN'" 2>&1
}

# Kill only processes of OUR two executables whose command line carries `$1`.
kill_ours() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\killours.ps1' -Match '$1'" 2>&1
}

# Take this run's scheduled tasks off the guest. A task that started a process
# is not needed once the process is gone, and a guest a dozen agents share does
# not need one per run of one stage left in its library.
unreg() {
  local names="" n
  for n in "$@"; do names="$names,'$n'"; done
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -Command \"foreach (\\\$t in @(${names#,})) { Unregister-ScheduledTask -TaskName \\\$t \
       -Confirm:\\\$false -ErrorAction SilentlyContinue }\"" >/dev/null 2>&1
}

comp_pid() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\killours.ps1' -Match '$1' -PidOnly" 2>/dev/null | tr -d '\r' | head -1
}

write_killours() { cat >"$1" <<PS2
# Generated by scripts/wc-taskbar.sh; do not edit on the guest.
# Only this stage's own executables, and only those whose command line carries
# the match. Never an image name on its own -- \$13.9.
param([Parameter(Mandatory=\$true)][string]\$Match, [switch]\$PidOnly)
Get-CimInstance Win32_Process |
  Where-Object { (\$_.Name -eq '$COMP_EXE' -or \$_.Name -eq '$CLIENT_EXE') -and
                 \$_.CommandLine -and \$_.CommandLine.Contains(\$Match) } |
  ForEach-Object {
    if (\$PidOnly) { Write-Output \$_.ProcessId }
    else {
      Write-Output ("killing pid " + \$_.ProcessId + " " + \$_.Name)
      Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue
    }
  }
PS2
}

write_runint() { cat >"$1" <<'PS1'
# Run a command on the interactive desktop of the logged-on session.
# Generated by scripts/wc-taskbar.sh; do not edit on the guest. See
# docs/waycompose-design.md §13.3 for why this is a scheduled task.
param(
  [Parameter(Mandatory=$true)][string]$Exe,
  [string]$Arguments = "",
  [string]$Name = "wctb32",
  [string]$WorkDir = "C:\Users\<guest-user>\wctb32",
  [switch]$Wait,
  [int]$TimeoutSec = 120
)
$ErrorActionPreference = "Stop"
$a = New-ScheduledTaskAction -Execute $Exe -Argument $Arguments -WorkingDirectory $WorkDir
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

# Pull a BMP off the guest and convert it.
fetch() {
  local remote="$1" local_png="$2" caption="$3"
  rm -f "$RUNDIR/$remote"
  scp "${SSHOPT[@]}" "$GUEST:$GUESTNAME/$remote" "$RUNDIR/$remote" >/dev/null 2>&1
  if [ -s "$RUNDIR/$remote" ]; then
    mkdir -p "$OUT"
    # **§35: the geometry and the colour count are compared, not printed.** This
    # was gated only on `magick` converting the file, and the `%wx%h` it printed
    # was decoration — a 1x1 capture, or an all-black one, converted fine and
    # read as a pass. `wc-launch.sh`'s `do_shot` gates on both; this one did
    # not, for either of its callers.
    #
    # The bounds are deliberately weak and absolute rather than exact: this is a
    # screenshot of a Windows desktop whose size the harness does not fix, so
    # what is excluded is a capture that cannot be of a desktop at all. 200x200
    # against a 1280x1024 screen, and 16 colours against the 1 a blank surface
    # gives -- the same floor and the same reasoning as §35.3's screenshot check.
    if magick "$RUNDIR/$remote" "$OUT/$local_png" 2>/dev/null; then
      local geo w hgt colours
      geo=$(identify -format '%wx%h' "$OUT/$local_png" 2>/dev/null)
      colours=$(identify -format '%k' "$OUT/$local_png" 2>/dev/null)
      w=${geo%x*}; hgt=${geo#*x}
      if [ "${w:-0}" -lt 200 ] || [ "${hgt:-0}" -lt 200 ]; then
        bad "$caption: the capture is ${geo:-nothing}, too small to be a desktop"
      elif [ "${colours:-0}" -lt 16 ]; then
        bad "$caption: the capture is $geo with only ${colours:-0} distinct colours -- a blank surface"
      else
        ok "$caption -> docs/stage32/$local_png ($geo, $colours distinct colours)"
      fi
    else
      bad "could not convert $remote"
    fi
  else
    bad "$caption: nothing came back"
  fi
}

# ---------------------------------------------------------------- deploy -----
do_deploy() {
  echo "== deploy =="
  for e in wc-desk.exe wc-compositor.exe wc-shmclient.exe; do
    [ -f "$WIN_BIN/$e" ] || {
      echo "error: missing $WIN_BIN/$e — cargo build --workspace --target x86_64-pc-windows-gnu first"
      exit 1; }
  done

  local stage; stage=$(mktemp -d /tmp/wc-tb-stage-XXXXXX)
  cp "$WIN_BIN/wc-desk.exe" "$stage/"
  cp "$WIN_BIN/wc-compositor.exe" "$stage/$COMP_EXE"
  cp "$WIN_BIN/wc-shmclient.exe" "$stage/$CLIENT_EXE"
  # The relay, which only the `apps` run needs. The compiler is in the dev
  # shell; running this script outside it is how the first attempt at that run
  # died — with `./probe-relay.exe: No such file or directory` in a log nobody
  # reads until the end, and the *proxy* reporting "the compositor closed the
  # connection", which reads like a compositor fault and is not one.
  if command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    x86_64-w64-mingw32-gcc -O1 -o "$stage/probe-relay.exe" "$ROOT/scripts/probe-relay.c" -lws2_32 \
      2>"$stage/cc.log" || { echo "  FAILED to build probe-relay.exe"; cat "$stage/cc.log"; }
    rm -f "$stage/cc.log"
  else
    g "test -f $DESKDIR/probe-relay.exe" >/dev/null 2>&1 \
      && echo "  probe-relay.exe: no mingw here; kept the one already on the guest" \
      || echo "  WARNING: no probe-relay.exe and no mingw to build one. \`apps\` will fail."
  fi
  write_runint "$stage/runint.ps1"
  write_killours "$stage/killours.ps1"

  g "mkdir -p $DESKDIR" || exit 1
  # Our own copies only; a running .exe cannot be overwritten.
  scp "${SSHOPT[@]}" "$stage/killours.ps1" "$GUEST:$GUESTNAME/" >/dev/null || exit 1
  kill_ours "$GUESTNAME" >/dev/null 2>&1
  sleep 1
  scp "${SSHOPT[@]}" "$stage"/* "$GUEST:$GUESTNAME/" >/dev/null || exit 1
  echo "  copied $(ls "$stage" | grep -c .) files to $GUEST:$DESKDIR"
  rm -rf "$stage"
  mkdir -p "$OUT"
}

# Start a compositor on the interactive desktop; echo its pid.
start_compositor() {
  local sock="$1" tag="$2" extra="${3:-}"
  int_bg "wctb32-$tag" "$DESKDIR_WIN\\$COMP_EXE" \
    "--socket $sock --timeout 400 --log $DESKDIR_WIN\\comp-$tag.log $extra" \
    >"$RUNDIR/start-$tag.log" 2>&1
  local p t
  for t in 1 2 3 4 5 6 7 8; do
    sleep 4
    p=$(comp_pid "$sock")
    [ -n "$p" ] && { echo "$p"; return 0; }
  done
  return 1
}

# One photograph and one window list, in a single scheduled task so that both
# are about the same instant. §16.6: two tasks cost a minute each.
photograph() {
  local tag="$1" cpid="$2"
  g "rm -f $DESKDIR/tray-$tag.bmp $DESKDIR/windows-$tag.txt $DESKDIR/shot-$tag.txt" >/dev/null 2>&1
  int "wctb32shot-$tag" "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe windows > $DESKDIR_WIN\\windows-$tag.txt 2>&1 & \
$DESKDIR_WIN\\wc-desk.exe shot --class Shell_TrayWnd --out $DESKDIR_WIN\\tray-$tag.bmp > $DESKDIR_WIN\\shot-$tag.txt 2>&1 & \
$DESKDIR_WIN\\wc-desk.exe shot --pid $cpid --class WaycomposeToplevel --out $DESKDIR_WIN\\win-$tag.bmp >> $DESKDIR_WIN\\shot-$tag.txt 2>&1" \
    >/dev/null 2>&1
  g "cat $DESKDIR/windows-$tag.txt" >"$RUNDIR/windows-$tag.txt" 2>&1
  cp "$RUNDIR/windows-$tag.txt" "$OUT/windows-$tag.txt" 2>/dev/null
}

# The identities our compositor's windows are carrying, one per line.
ours() {
  grep "WaycomposeToplevel" "$RUNDIR/windows-$1.txt" 2>/dev/null \
    | grep -oE 'aumid="[^"]*"' | sed 's/aumid=//; s/"//g' | sort
}

# ------------------------------------------------------------ local runs -----
#
# Two of our own clients against one compositor, on the guest, with no host and
# no hypervisor in it. `--debug-icon` gives each window a different generated
# icon so that the two buttons are told apart in the photograph as well as in
# the table.
local_run() {
  local tag="$1" a_app="$2" a_title="$3" b_app="$4" b_title="$5" late="${6:-}"
  local sock="wg-s32-$$-$tag"
  echo
  echo "== $tag =="
  kill_ours "wg-s32" >/dev/null 2>&1
  g "rm -f $APPDATA_WIN/wg-s32-* 2>/dev/null; true" >/dev/null 2>&1
  sleep 2

  local CPID
  CPID=$(start_compositor "$sock" "$tag" "--debug-icon")
  if [ -z "$CPID" ]; then
    bad "$tag: the compositor did not start on the interactive desktop"
    sed 's/^/        /' "$RUNDIR/start-$tag.log" | head -6
    return 1
  fi
  note "compositor pid $CPID (socket $sock)"

  # THE CLIENTS RUN ON THE INTERACTIVE DESKTOP TOO, AND THAT IS NOT OPTIONAL.
  #
  # Measured here the first time this was run the other way: a `wl_shm` pool on
  # Windows is a **named section**, and `Local\…` is per *session*. A client
  # started over ssh creates its section in session 0's namespace; the
  # compositor, on the logged-on desktop, cannot open the name at all —
  # "Das System kann die angegebene Datei nicht finden", a protocol error on
  # `wl_shm`, and a window that is created and never mapped. Same machine, same
  # path, same name; different session. So both ends go through the scheduler.
  local lateflag=""
  [ -n "$late" ] && lateflag="--late-app-id $late --late-after 40"
  int_bg "wctb32-$tag-a" "cmd.exe" \
    "/c $DESKDIR_WIN\\$CLIENT_EXE --frames 1000000 --seconds 300 --app-id $a_app \
--title $a_title $APPDATA_WIN\\$sock > $DESKDIR_WIN\\cli-a-$tag.log 2>&1" >/dev/null 2>&1
  sleep 4
  int_bg "wctb32-$tag-b" "cmd.exe" \
    "/c $DESKDIR_WIN\\$CLIENT_EXE --frames 1000000 --seconds 300 --app-id $b_app \
--title $b_title $lateflag $APPDATA_WIN\\$sock > $DESKDIR_WIN\\cli-b-$tag.log 2>&1" >/dev/null 2>&1
  sleep 12

  photograph "$tag" "$CPID"
  fetch "tray-$tag.bmp" "stage32-taskbar-$tag.png" "the taskbar ($tag)"
  fetch "win-$tag.bmp"  "stage32-window-$tag.png"  "one of the windows ($tag)"

  g "cat $DESKDIR/cli-a-$tag.log" >"$RUNDIR/cli-a-$tag.log" 2>&1
  g "cat $DESKDIR/cli-b-$tag.log" >"$RUNDIR/cli-b-$tag.log" 2>&1
  kill_ours "$sock" >/dev/null 2>&1
  unreg "wctb32-$tag" "wctb32-$tag-a" "wctb32-$tag-b"
  g "cat $DESKDIR/comp-$tag.log" >"$RUNDIR/comp-$tag.log" 2>&1
  cp "$RUNDIR/comp-$tag.log" "$OUT/compositor-$tag.log" 2>/dev/null
  echo "        --- what the compositor says about identity:"
  grep -iE "taskbar identity|app_id ->|COM apartment" "$RUNDIR/comp-$tag.log" \
    | sed 's/^/        /' | head -14
  grep -iE "^wc-shmclient: (FAIL|PASS)|frame 0 " "$RUNDIR/cli-a-$tag.log" "$RUNDIR/cli-b-$tag.log" \
    2>/dev/null | sed 's/^/        /' | head -6
}

do_local() {
  # 1. Two different applications. The requirement.
  local_run two \
    "org.waycompose.probe.alpha" "stage32-alpha" \
    "org.waycompose.probe.beta"  "stage32-beta"
  local n
  n=$(ours two | wc -l)
  local d
  d=$(ours two | sort -u | wc -l)
  if [ "$n" -ge 2 ] && [ "$d" = "$n" ]; then
    ok "two different app ids -> $d distinct taskbar identities over $n windows"
    ours two | sed 's/^/        /'
  else
    bad "two different app ids -> $d identities over $n windows (wanted 2 and 2)"
    sed -n '/WaycomposeToplevel/p' "$RUNDIR/windows-two.txt" | sed 's/^/        /' | head -6
  fi

  # 2. The negative check, which is the one that is easy to break: two windows
  #    of the SAME application must stay on ONE button.
  local_run same \
    "org.waycompose.probe.same" "stage32-sameA" \
    "org.waycompose.probe.same" "stage32-sameB"
  n=$(ours same | wc -l)
  d=$(ours same | sort -u | wc -l)
  if [ "$n" -ge 2 ] && [ "$d" = 1 ]; then
    ok "two windows of one app id -> 1 taskbar identity over $n windows"
    ours same | sort -u | sed 's/^/        /'
  else
    bad "two windows of one app id -> $d identities over $n windows (wanted 1)"
    sed -n '/WaycomposeToplevel/p' "$RUNDIR/windows-same.txt" | sed 's/^/        /' | head -6
  fi

  # 3. An app id that arrives after the window is already on screen. No toolkit
  #    does this, so the client is told to.
  local_run late \
    "org.waycompose.probe.early" "stage32-unchanged" \
    "org.waycompose.probe.early" "stage32-renamed" \
    "org.waycompose.probe.renamed"
  if grep -q "org.waycompose.probe.renamed" "$RUNDIR/comp-late.log" 2>/dev/null; then
    ok "the late set_app_id reached the compositor"
  else
    bad "the late set_app_id never reached the compositor"
  fi
  if ours late | grep -q "renamed"; then
    ok "and the window's taskbar identity followed it: $(ours late | grep renamed)"
  else
    bad "the window kept its old identity: $(ours late | tr '\n' ' ')"
  fi
  if grep -q "rebuilt under the new identity" "$RUNDIR/comp-late.log" 2>/dev/null; then
    note "the button was rebuilt (hide/show), which is the late path taken"
  else
    note "no rebuild was needed: the identity changed before the window was shown"
  fi
}

# -------------------------------------------------------- two real apps ------
do_apps() {
  if [ -z "$HOST" ] || [ -z "$HOST_WESTON" ]; then
    miss "WC_HOST and WC_HOST_WESTON unset: no real clients, no picture"
    return
  fi
  local tag=apps sock="wg-s32-$$-apps"
  echo
  echo "== two real applications, through the proxy =="
  kill_ours "wg-s32" >/dev/null 2>&1
  g "rm -f $APPDATA_WIN/wg-s32-* 2>/dev/null; true" >/dev/null 2>&1
  sleep 2

  # STAGE 41. The substrate, declared before anything is measured on it. The
  # `guest-ram` arm unsets `SV_ALLOW_FILE_ARENA`, so if anything below still
  # reaches for a file the binaries refuse it by name rather than serving it.
  wc_substrate_declare guest-ram || return

  # And the export loses the three things this block used to put on it: the
  # arena, the keymap directory and the pool directory. Removed rather than left
  # and trusted not to be used — a leftover `arena` is openable, the right size,
  # and completely silent to anything that was not looking. §18's
  # `purge_arena_file`, which this generalises.
  h "mkdir -p $HOST_DIR" >/dev/null
  wc_purge_arena_file "$SHARE"

  # A desktop entry each, naming a DIFFERENT icon each, both of them real files
  # on the host. Weston ships no desktop entries (§16), so these are written —
  # three lines apiece, and everything after them is the real resolver.
  local icon_b="${APP_B_ICON:-$HOST_WESTON/../share/weston/icon_ivi_flower.png}"
  h "rm -rf $HOST_DIR/data; mkdir -p $HOST_DIR/data/applications" >/dev/null
  h "cat > $HOST_DIR/data/applications/$APP_A_ID.desktop <<EOD
[Desktop Entry]
Type=Application
Name=$APP_A
Icon=$APP_A_ICON
EOD
cat > $HOST_DIR/data/applications/$APP_B_ID.desktop <<EOD
[Desktop Entry]
Type=Application
Name=$APP_B
Icon=$icon_b
EOD" >/dev/null

  # STAGE 41. WHAT WAS HERE, AND WHY IT WAS WRONG FOR FOUR STAGES.
  #
  #   # shared-file rather than the arena: the arena serves exactly one
  #   # connection (§12.13 item 6) and this experiment is *about* having two.
  #   transport_args="--transport shared-file --pool-dir $SHARE/pools ..."
  #   keymap_extra="--shared-dir $SHARE_WIN\keymaps ..."
  #
  # The premise expired in stage 12. `wc-proxy`'s `make_transport` calls
  # `Arena::share()` per session and prints "client N: joins arena 0 (M
  # connection(s) on it now)"; §15.14 is the stage that fixed it and §15.13
  # measured four of Firefox's connections on one region and one allocator. So
  # the one-connection limit was gone before this harness was written, and the
  # sentence quoting it is what kept every taskbar-identity photograph on a file
  # on the virtio-fs export — the substrate §10.3 measured incoherent for live
  # mappings. Two applications on one guest-RAM arena is now the *stronger*
  # version of this experiment, not a weaker one: the two buttons have to come
  # apart while both clients' pixels are in the same region.
  #
  # No `--shared-dir` either, and its absence is the point: with a guest-RAM
  # arena the keymap is cut from the compositor's own outbound window (§14), so
  # nothing is published on any filesystem and the assertion at the bottom of
  # this function can demand zero shared files rather than merely few.
  #
  # The frame list is the one file that still crosses and it is not the arena:
  # it says *where the guest's pages are*, not what is in them. Written to the
  # guest's own directory and fetched over ssh, so this block needs no export.
  local gframes_win="$DESKDIR_WIN\\arena-$tag.frames"
  local gframes="$DESKDIR/arena-$tag.frames"
  g "rm -f $gframes" >/dev/null 2>&1

  # `wc-proxy` and, new in §41, `wc-arena-host`. The locator is **not a
  # workspace member** — it links shared_vulkan's `sv-hostd` — so it is built
  # through its own manifest, exactly as `wc-desktop.sh` does it.
  h "pkill -f '^$HOST_DIR/wc-proxy' 2>/dev/null; sleep 1; true" >/dev/null 2>&1
  local locator="$LINUX_BIN/wc-arena-host"
  [ -x "$locator" ] || locator="$ROOT/crates/wc-arena-host/target/debug/wc-arena-host"
  if ! cargo build --manifest-path "$ROOT/crates/wc-arena-host/Cargo.toml" \
        >"$RUNDIR/loc-$tag.log" 2>&1; then
    bad "could not build wc-arena-host"; tail -10 "$RUNDIR/loc-$tag.log" | sed 's/^/        /'; return
  fi
  scp "${SSHOPT[@]}" "$LINUX_BIN/wc-proxy" "$locator" "$HOST:$HOST_DIR/" >/dev/null || {
    bad "could not copy wc-proxy/wc-arena-host to $HOST -- something is still holding one"; return; }
  h "chmod +x $HOST_DIR/wc-proxy $HOST_DIR/wc-arena-host" >/dev/null

  local CPID
  CPID=$(start_compositor "$sock" "$tag" "$(wc_arena_comp_args "$gframes_win")")
  if [ -z "$CPID" ]; then
    bad "the compositor did not start on the interactive desktop"
    sed 's/^/        /' "$RUNDIR/start-$tag.log" | head -6
    return
  fi
  note "compositor pid $CPID (socket $sock)"

  ssh "${SSHOPT[@]}" "$GUEST" \
    "export MSYS_NO_PATHCONV=1; cd $GUESTNAME && ./probe-relay.exe $PORT '$APPDATA_WIN\\$sock' --timeout 360" \
    >"$RUNDIR/relay.log" 2>&1 &
  local relay=$!
  sleep 3

  # STAGE 41. THE LOCATE, and it has to be exactly here: after the compositor
  # has committed and pinned the region inside the VM, and before the proxy that
  # will map it starts. That ordering is the whole difference between this
  # harness and the one it replaces, which had nothing to locate because its
  # pixels were pool files on an export.
  local plan="$HOST_DIR/$sock.plan"
  if ! wc_locate_arena "$DESKDIR/comp-$tag.log" "$gframes" "$plan" "$RUNDIR"; then
    kill $relay 2>/dev/null
    kill_ours "$sock" >/dev/null 2>&1
    return
  fi

  # STAGE 41: **the proxy and both clients are now root, in a transient unit.**
  # The proxy has to open `/proc/<qemu-pid>/fd/<n>` to reach the guest's pages
  # and QEMU is a system service on this host (§12.9), so the whole host side
  # moves under `systemd-run --collect`. `wc-desktop.sh`'s arrangement, and its
  # two lessons come with it: a root call that blocks for minutes often returns
  # with no output at all, so completion is signalled by a *file* rather than by
  # an exit code; and the `ssh -L` forward stays with the invoking user, outside
  # the unit, because root has no `$HOME/.ssh/wc_guest` and can open a
  # user-owned socket node anyway.
  #
  # `XDG_DATA_DIRS` moves inside the unit, because the icons these two buttons
  # are being told apart by are resolved by the *proxy*, which is now root.
  h "cat > $HOST_DIR/s32.sh <<'EOS'
#!/usr/bin/env bash
set -uo pipefail
RUN=\$(mktemp -d /tmp/wc-s32-XXXXXX); chmod 755 \"\$RUN\"
ssh -i $HOST_KEY -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR \
    -N -L \"\$RUN/up.sock:127.0.0.1:$PORT\" $HOST_GUEST >\"\$RUN/fwd.log\" 2>&1 &
FWD=\$!
sleep 4
if [ -S \"\$RUN/up.sock\" ]; then
  cat > \"\$RUN/root.sh\" <<'INNER'
#!/usr/bin/env bash
export PATH=/run/current-system/sw/bin:/run/wrappers/bin
RUN=__RUN__
export XDG_RUNTIME_DIR=\"\$RUN\"
export XDG_DATA_DIRS=__DATA__
$HOST_DIR/wc-proxy --upstream \"\$RUN/up.sock\" --socket wl-proxy \
   --runtime-dir \"\$RUN\" --transport arena --arena-plan $plan --arena-id 0 \
   --copy damage --accept-timeout 40 --timeout 320 \
   >\"\$RUN/proxy.log\" 2>&1 &
PROXY=\$!
sleep 2
WAYLAND_DISPLAY=wl-proxy $HOST_WESTON/$APP_A >\"\$RUN/cli-a.log\" 2>&1 &
A=\$!
sleep 6
WAYLAND_DISPLAY=wl-proxy $HOST_WESTON/$APP_B >\"\$RUN/cli-b.log\" 2>&1 &
B=\$!
sleep 230
kill \$A \$B \$PROXY 2>/dev/null; wait 2>/dev/null
chmod -R a+rwX \"\$RUN\" 2>/dev/null
touch \"\$RUN/done\"
INNER
  sed -i \"s#__RUN__#\$RUN#; s#__DATA__#$HOST_DIR/data:$HOST_ICONS#\" \"\$RUN/root.sh\"
  chmod 755 \"\$RUN/root.sh\"
  UNIT=wc-s32-\$\$
  echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
systemd-run --unit=\$UNIT --collect /run/current-system/sw/bin/bash \$RUN/root.sh >/dev/null 2>&1
echo started\" | eval $ROOTC >/dev/null 2>&1
  echo S32-CLIENTS-STARTED
  for _ in \$(seq 1 300); do [ -f \"\$RUN/done\" ] && break; sleep 1; done
  [ -f \"\$RUN/done\" ] || echo \"S32-FAIL: the host-side unit never finished\"
else
  echo S32-FAIL: no forward socket
fi
kill \$FWD 2>/dev/null
sleep 1
echo S32-PROXY-BEGIN; cat \"\$RUN/proxy.log\" 2>/dev/null; echo S32-PROXY-END
echo S32-A-BEGIN; tail -10 \"\$RUN/cli-a.log\" 2>/dev/null; echo S32-A-END
echo S32-B-BEGIN; tail -10 \"\$RUN/cli-b.log\" 2>/dev/null; echo S32-B-END
rm -rf \"\$RUN\"
EOS
chmod +x $HOST_DIR/s32.sh" >/dev/null
  h "$HOST_DIR/s32.sh" >"$RUNDIR/host-apps.log" 2>&1 &
  local drv=$!

  local attempt
  for attempt in 1 2 3; do
    sleep 25
    photograph "$tag" "$CPID"
    [ "$(ours apps | wc -l)" -ge 2 ] && break
    note "attempt $attempt: fewer than two of our windows on the desktop yet"
  done
  fetch "tray-$tag.bmp" "stage32-taskbar-two-applications.png" "the taskbar, two applications"
  fetch "win-$tag.bmp"  "stage32-window-apps.png"              "one of the two windows"

  wait $drv 2>/dev/null
  kill $relay 2>/dev/null; wait $relay 2>/dev/null
  g "cat $DESKDIR/comp-$tag.log" >"$RUNDIR/comp-$tag.log" 2>&1
  cp "$RUNDIR/comp-$tag.log" "$OUT/compositor-apps.log" 2>/dev/null
  sed -n '/S32-PROXY-BEGIN/,/S32-PROXY-END/p' "$RUNDIR/host-apps.log" \
    | grep -v "^S32-" >"$OUT/proxy-apps.log" 2>/dev/null

  echo "        --- the proxy's account:"
  grep -iE "icon: app id|client|stopped" "$OUT/proxy-apps.log" 2>/dev/null | sed 's/^/        /' | head -10
  echo "        --- the compositor's account:"
  grep -iE "taskbar identity|icon set:" "$RUNDIR/comp-$tag.log" | sed 's/^/        /' | head -12

  # STAGE 41, AND BEFORE THE IDENTITY CLAIM, BECAUSE IT IS WHAT THAT CLAIM IS
  # ABOUT. Two taskbar buttons come apart the same way whatever carried the
  # pixels — which is exactly why a run has to say what carried them, or the
  # photographs in docs/stage32/ turn out to have been taken on a file on the
  # export and nobody notices for four stages. An assertion rather than the
  # `note` this was: §35.
  wc_assert_substrate "$RUNDIR/comp-$tag.log" "$tag"

  # And the other half of the same claim, which only a guest-RAM run can make:
  # two *connections* on one region and one allocator. §15.14 made that possible
  # and nothing outside `wc-firefox.sh` had ever checked it on a real guest.
  local joined
  joined=$(grep -c 'joins arena 0' "$OUT/proxy-apps.log" 2>/dev/null)
  [ "${joined:-0}" -ge 2 ] \
    && ok "$tag: both clients share one guest-RAM arena and one allocator ($joined joined)" \
    || bad "$tag: only ${joined:-0} connection(s) joined the arena; two applications need two"

  local n d
  n=$(ours apps | wc -l); d=$(ours apps | sort -u | wc -l)
  if [ "$n" -ge 2 ] && [ "$d" -ge 2 ]; then
    ok "two real applications -> $d distinct taskbar identities over $n windows"
    ours apps | sed 's/^/        /'
  else
    bad "two real applications -> $d identities over $n windows"
    grep WaycomposeToplevel "$RUNDIR/windows-apps.txt" 2>/dev/null | sed 's/^/        /' | head -6
  fi
  # **§35: the failing arm was a `note`, so this could not fail.** "The identity
  # claim is unaffected" is true and is a reason to keep the two checks
  # *separate*, not a reason for one of them to have no verdict — a taskbar
  # button with the right identity and no icon is a defect §16 spent a whole
  # stage on, and it would have passed here in silence.
  #
  # The count is quoted from the compositor's own line rather than inferred, so
  # "1 of 2" reads as the partial failure it is instead of as an absence.
  local icons
  icons=$(grep -o "icon set: [0-9]* of [0-9]* slots" "$RUNDIR/comp-$tag.log" | tail -1)
  case "$icons" in
    "icon set: 2 of 2 slots") ok "and both windows carry an icon resolved on the host ($icons)" ;;
    "") bad "the compositor never reported an icon slot count; nothing here says an icon was set" ;;
    *) bad "only some icons resolved: $icons -- the identity is right and the picture is not" ;;
  esac
  kill_ours "$sock" >/dev/null 2>&1
  unreg "wctb32-$tag"
}

do_leftovers() {
  echo
  echo "== what is left on the guest =="
  local n
  n=$(g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
        -File '$DESKDIR_WIN\\killours.ps1' -Match '$GUESTNAME' -PidOnly" 2>/dev/null \
      | tr -d '\r' | grep -c '[0-9]')
  if [ "${n:-0}" -eq 0 ]; then
    ok "nothing of $GUESTNAME's is still running"
  else
    bad "$n process(es) from $GUESTNAME left behind; kill them by pid"
  fi
}

case "$MODE" in
  deploy) do_deploy ;;
  local)  do_local; do_leftovers ;;
  apps)   do_apps; do_leftovers ;;
  all)    do_deploy; do_local; do_apps; do_leftovers ;;
  *) echo "usage: $0 {deploy|local|apps|all}"; exit 2 ;;
esac

echo
echo "== $pass passed, $fail failed, $skip skipped =="
[ "$fail" -eq 0 ]
