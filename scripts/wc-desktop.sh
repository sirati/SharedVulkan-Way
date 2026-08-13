#!/usr/bin/env bash
# Run the compositor on the guest's **interactive desktop** and photograph it.
#
#   scripts/wc-desktop.sh deploy    cross-compile and copy wc-desk / wc-compositor
#   scripts/wc-desktop.sh probe     station, windows, tick -- from both sessions
#   scripts/wc-desktop.sh window    a real client's window, captured (needs WC_HOST)
#   scripts/wc-desktop.sh all       deploy, probe, window
#
# Run from inside `nix develop`. Everything lands in docs/stage8/.
#
# WHY THIS SCRIPT EXISTS, AND WHAT IT DOES THAT wc-guest.sh DOES NOT
#
# `wc-guest.sh` runs the compositor over ssh. docs/design.md §10.12 item 3 is the
# discovery that this was never enough: **a Windows ssh session is not on the
# desktop**. sshd logs the user on in session 0 with its own window station,
# `Service-0x0-18f160f$`, whose single desktop has no display device behind it.
# Every Win32 call the compositor makes still succeeds there --
# `CreateWindowExW`, `ShowWindow`, `SetDIBitsToDevice` -- and the pixels go
# nowhere. `GetSystemMetrics(SM_CXSCREEN)` even answers, with 1024x768, for a
# screen that does not exist. Nothing about that session says it is blind.
#
# So this script's whole job is the four steps that get onto `WinSta0\Default`
# of the logged-on session and back again.
#
# HOW THE INTERACTIVE SESSION IS REACHED, AND WHAT WAS REJECTED
#
# The task scheduler, with a principal whose `LogonType` is `Interactive`. A
# task registered that way runs in the session the named user is already logged
# on in, on its real window station, and needs no password and no service.
#
# Rejected, each for a concrete reason on this guest:
#
#   * **RDP.** The guest is Windows 11 *Home*. There is no RDP host, and there
#     is no `qwinsta`/`tscon` either -- both are absent from System32.
#   * **A service plus `WTSQueryUserToken` and `CreateProcessAsUser`.** This is
#     the textbook route and it works, but it means installing a service on a
#     production machine to run a test binary. The scheduler already owns a
#     supported version of exactly that mechanism.
#   * **`psexec -i 1`.** Not present, and it is the same mechanism with a
#     third-party binary in front of it.
#
# Two things about `Register-ScheduledTask` cost a round each and are worth
# keeping:
#
#   * **`$env:USERDOMAIN` is `WORKGROUP` under sshd**, not the machine name, so
#     `-UserId "$env:USERDOMAIN\$env:USERNAME"` fails with "No mapping between
#     account names and security IDs" -- which reads as a broken account and is a
#     wrong environment variable. `runint.ps1` asks the token instead.
#   * **A task's process does not die with the ssh session**, unlike a
#     backgrounded one (`wc-guest.sh`'s first operational lesson). That is the
#     point for the compositor and a leak for everything else, so `probe` waits
#     and `window` kills by socket name -- see `kill_ours`.
#
# WHY THE PICTURES ARE `PrintWindow` AND NOT SCREEN GRABS
#
# The guest's session is logged on -- `explorer.exe` and its taskbar are running
# -- and a full-screen lock-screen window sits on top of everything with Windows
# Hello waiting for a face. There is no password for this machine here and none
# was looked for, so the *screen* shows the lock screen and will keep showing it.
#
# `PrintWindow(hwnd, dc, PW_RENDERFULLCONTENT)` does not read the framebuffer.
# It reads the window's DWM redirection surface, which exists and is up to date
# for every composited window whether or not anything is on top of it. So a
# capture of our window is the real window, drawn by the real theme at the real
# DPI, and a capture of `Shell_TrayWnd` is the real taskbar with our button in
# it. What it cannot show is z-order or whether the panel is even lit, and no
# claim below rests on either.
#
# `--screen` is still run, once, and kept: in the ssh session it *fails*, with
# `ERROR_ACCESS_DENIED` out of `BitBlt`, and on the desktop it succeeds and
# returns the lock screen. That pair is the evidence for the paragraph above.
set -uo pipefail

MODE="${1:-all}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
OUT="$ROOT/docs/stage8"

GUEST="${WC_GUEST:-<guest-user>@<guest-ip>}"
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"
DESKDIR='/c/Users/<guest-user>/wcdesk'
DESKDIR_WIN='C:\Users\<guest-user>\wcdesk'
APPDATA_WIN='C:\Users\<guest-user>\AppData\Local\waycompose'
PORT="${WC_PORT_BASE:-57271}"

# The stage-4 half: the machine that shares a filesystem with the guest.
HOST="${WC_HOST:-}"
HOST_DIR="${WC_HOST_DIR:-/tmp/wc-stage8}"
SHARE="${WC_HOST_SHARE:-/srv/winshare/wc}"
SHARE_WIN="${WC_GUEST_SHARE:-Z:\\wc}"
HOST_GUEST="${WC_HOST_GUEST:-<guest-user>@<guest-nat-ip>}"
HOST_KEY="${WC_HOST_KEY:-\$HOME/.ssh/wc_guest}"
HOST_WESTON="${WC_HOST_WESTON:-}"
# Which unmodified client to put in the window. `weston-terminal` is the
# `docs/goal-firefox.md` stand-in and is what the title and taskbar claims use;
# `weston-presentation-shm` is the one the modal-resize check wants, because it
# commits a frame continuously, so a compositor that stops repainting stops
# producing evidence within a frame rather than within a minute. `weston-flower`
# looks like the right choice and is not one: it draws its flower once.
CLIENT="${WC_CLIENT:-weston-terminal}"
CLIENT_TITLE="${WC_CLIENT_TITLE:-Wayland Terminal}"
SUFFIX="${WC_SUFFIX:-}"

SSHOPT=(-o IdentityAgent="$AGENT" -o BatchMode=yes -o LogLevel=ERROR
        -o StrictHostKeyChecking=accept-new)

RUNDIR=$(mktemp -d /tmp/wc-desk-XXXXXX); chmod 700 "$RUNDIR"
trap 'rm -rf "$RUNDIR"' EXIT

pass=0; fail=0; skip=0
ok()   { echo "  PASS  $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail + 1)); }
note() { echo "  ----  $*"; }
miss() { echo "  SKIP  $*"; skip=$((skip + 1)); }

g() { ssh "${SSHOPT[@]}" "$GUEST" "$@"; }
h() { [ -n "$HOST" ] && ssh "${SSHOPT[@]}" "$HOST" "$@"; }

# Run a command line on the interactive desktop and wait for it.
#   int <name> <exe> <arguments-as-one-string>
int() {
  local name="$1" exe="$2" argstr="$3"
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\runint.ps1' -Exe '$exe' -Arguments '$argstr' \
       -Name '$name' -Wait -TimeoutSec 120" 2>&1
}

# The same, without waiting: for the compositor, which has to outlive the call.
int_bg() {
  local name="$1" exe="$2" argstr="$3"
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\runint.ps1' -Exe '$exe' -Arguments '$argstr' -Name '$name'" 2>&1
}

# Kill only the compositors this script started, matched on the socket name in
# their command line.
#
# `wc-guest.sh` kills every `wc-compositor.exe` on the guest, which is right when
# it is the only thing driving the machine. It is wrong here: another worktree is
# allowed to be running one against the same guest at the same time, and a run
# that reaps somebody else's process is worse than a run that leaves its own
# behind. The socket name carries the pid of the shell that made it, so it is
# unique per invocation for the same reason `wc-stage4`'s is.
kill_ours() {
  local sock="$1"
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\killours.ps1' -Match '$sock'" 2>&1
}

write_killours() { cat >"$1" <<'PS2'
# Generated by scripts/wc-desktop.sh; do not edit on the guest.
#   -Match <socket>          kill the compositors whose command line contains it
#   -Match <socket> -PidOnly print their process ids and kill nothing
param([Parameter(Mandatory=$true)][string]$Match, [switch]$PidOnly)
Get-CimInstance Win32_Process -Filter "Name = 'wc-compositor.exe'" |
  Where-Object { $_.CommandLine -and $_.CommandLine.Contains($Match) } |
  ForEach-Object {
    if ($PidOnly) { Write-Output $_.ProcessId }
    else {
      Write-Output ("killing pid " + $_.ProcessId)
      Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }
  }
PS2
}

# The process id of the compositor this run started.
#
# Everything that photographs or drives a window is addressed by pid, never by
# window class. Another worktree drives the same guest, its compositor registers
# the same `WaycomposeToplevel` class and runs the same `weston-terminal`, so a
# class-name search finds *its* window about as often as ours — which was
# diagnosed twice as flakiness before it was diagnosed as this.
comp_pid() {
  local sock="$1"
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\killours.ps1' -Match '$sock' -PidOnly" 2>/dev/null | tr -d '\r' | head -1
}

# --------------------------------------------------------------- runint.ps1 --
# Written here rather than kept as a file next to this script, for the same
# reason `wc-guest.sh` generates its stage-4 driver: it is one half of this
# script and editing it on the guest would fork it silently.
write_runint() { cat >"$1" <<'PS1'
# Run a command on the interactive desktop of the logged-on session.
# Generated by scripts/wc-desktop.sh; do not edit on the guest.
param(
  [Parameter(Mandatory=$true)][string]$Exe,
  [string]$Arguments = "",
  [string]$Name = "wcdesk",
  [string]$WorkDir = "C:\Users\<guest-user>\wcdesk",
  [switch]$Wait,
  [int]$TimeoutSec = 120
)
$ErrorActionPreference = "Stop"
$a = New-ScheduledTaskAction -Execute $Exe -Argument $Arguments -WorkingDirectory $WorkDir
# sshd sets USERDOMAIN=WORKGROUP, which resolves to no SID at all. Ask the
# token for the name instead; this is the line that cost a round.
$p = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) `
                                -LogonType Interactive -RunLevel Highest
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
                                  -MultipleInstances Parallel
$s.ExecutionTimeLimit = "PT0S"          # the compositor decides when it is done
$s.DisallowStartIfOnBatteries = $false  # this guest is a laptop and often is
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

# ---------------------------------------------------------------- deploy -----
do_deploy() {
  echo "== deploy =="
  local missing=0
  for e in wc-desk.exe wc-compositor.exe probe-relay.exe; do
    if [ -f "$WIN_BIN/$e" ]; then :; else
      case "$e" in
        probe-relay.exe) ;;   # built below, it is a C probe
        *) echo "  missing $WIN_BIN/$e"; missing=1 ;;
      esac
    fi
  done
  [ $missing -eq 0 ] || {
    echo "error: cargo build --workspace --target x86_64-pc-windows-gnu first"; exit 1; }

  local stage; stage=$(mktemp -d /tmp/wc-desk-stage-XXXXXX)
  cp "$WIN_BIN/wc-desk.exe" "$WIN_BIN/wc-compositor.exe" "$stage/"
  if [ -n "${WC_XKB_MINGW:-}" ]; then
    x86_64-w64-mingw32-gcc -O1 -o "$stage/probe-relay.exe" "$ROOT/scripts/probe-relay.c" -lws2_32 \
      2>"$stage/cc.log" || { echo "  FAILED to build probe-relay.exe"; cat "$stage/cc.log"; }
  fi
  write_runint "$stage/runint.ps1"
  write_killours "$stage/killours.ps1"

  # A running .exe holds its own file open and scp over it fails as
  # `dest open ...: Failure` -- wc-guest.sh's lesson, and a task-launched
  # compositor is *more* likely to still be there because it did not die with
  # the session that started it. Only this script's own copies are killed:
  # `wcdesk\wc-compositor.exe` is a different file from `wctest\`'s, which
  # another worktree may legitimately be running.
  kill_ours wcdesk >/dev/null 2>&1
  g "taskkill //F //IM wc-desk.exe >/dev/null 2>&1; true" >/dev/null 2>&1
  g "mkdir -p $DESKDIR" || exit 1
  scp "${SSHOPT[@]}" "$stage"/* "$GUEST:wcdesk/" >/dev/null || exit 1
  echo "  copied $(ls "$stage" | grep -c .) files to $GUEST:$DESKDIR"
  rm -rf "$stage"
  mkdir -p "$OUT"
}

# ----------------------------------------------------------------- probe -----
# Everything in this block is run twice, once per session, because the whole
# point is the difference between them.
do_probe() {
  echo
  echo "== 1. the two window stations =="
  g "cd wcdesk && ./wc-desk.exe station" >"$RUNDIR/station-ssh.txt" 2>&1
  int wcstation "$DESKDIR_WIN\\wc-desk.exe" "station" >/dev/null 2>&1
  # A task's stdout goes nowhere, so the desktop run is done again with its
  # output redirected by the shell *it* starts.
  int wcstation "cmd.exe" "/c $DESKDIR_WIN\\wc-desk.exe station > $DESKDIR_WIN\\station.txt 2>&1" \
    >/dev/null 2>&1
  g "cat $DESKDIR/station.txt" >"$RUNDIR/station-desk.txt" 2>&1

  sed 's/^/        ssh   /' "$RUNDIR/station-ssh.txt"
  sed 's/^/        desk  /' "$RUNDIR/station-desk.txt"

  grep -q "winsta *Service-" "$RUNDIR/station-ssh.txt" \
    && ok "the ssh session is on a service window station, not WinSta0" \
    || bad "the ssh session's window station is not what §10.12 item 3 says"
  grep -q "winsta *WinSta0" "$RUNDIR/station-desk.txt" \
    && ok "the scheduled task runs on WinSta0  <-- the stage-8 milestone" \
    || bad "the scheduled task did not reach WinSta0"
  grep -q "desktop *Default" "$RUNDIR/station-desk.txt" \
    && ok "and on WinSta0's Default desktop, which is the one with a display" \
    || bad "not the Default desktop"
  grep -q "screen.capture *FAILS" "$RUNDIR/station-ssh.txt" \
    && ok "a screen capture in the ssh session FAILS -- there is no display there" \
    || bad "the ssh session captured a screen, which it should not be able to"
  grep -q "screen.capture *ok" "$RUNDIR/station-desk.txt" \
    && ok "a screen capture on the desktop succeeds" \
    || bad "the desktop capture failed"

  echo
  echo "== 2. SetProcessDpiAwarenessContext, and why it is refused (§10.13 item 3) =="
  grep -E "dpi\.|screen\.primary" "$RUNDIR/station-desk.txt" |  g "cat $DESKDIR/shot.txt" >"$RUNDIR/shot.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/shot.txt"
  g "cd wcdesk && ./wc-desk.exe --unaware station" >"$RUNDIR/unaware-ssh.txt" 2>&1
  int wcstation "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe --unaware station > $DESKDIR_WIN\\unaware.txt 2>&1" >/dev/null 2>&1
  g "cat $DESKDIR/unaware.txt" >"$RUNDIR/unaware-desk.txt" 2>&1
  note "with --unaware, the same binary reports:"
  grep -E "dpi\.awareness|screen\.primary" "$RUNDIR/unaware-desk.txt" |  g "cat $DESKDIR/shot.txt" >"$RUNDIR/shot.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/shot.txt"

  if grep -q "dpi.call.*-> ok" "$RUNDIR/station-desk.txt"; then
    ok "PER_MONITOR_AWARE_V2 is available on this machine: a second binary gets it"
  elif grep -q "ERROR_ACCESS_DENIED" "$RUNDIR/station-desk.txt"; then
    ok "refused with ERROR_ACCESS_DENIED: awareness was already set, not unavailable"
  else
    bad "SetProcessDpiAwarenessContext refused for a reason that is neither"
  fi

  echo
  echo "== 3. what a millisecond costs (§10.13 item 4) =="
  int wctick "cmd.exe" "/c $DESKDIR_WIN\\wc-desk.exe tick --samples 40 > $DESKDIR_WIN\\tick.txt 2>&1" \
    >/dev/null 2>&1
  g "cat $DESKDIR/tick.txt" >"$RUNDIR/tick.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/tick.txt"
  cp "$RUNDIR/tick.txt" "$OUT/tick.txt" 2>/dev/null
  local s5 hr1
  s5=$(awk '/^sleep\(5\)/ { print $3 }' "$RUNDIR/tick.txt")
  hr1=$(awk '/^hrtimer\(5\)/ { print $3 }' "$RUNDIR/tick.txt")
  if [ -n "$s5" ] && [ -n "$hr1" ]; then
    # A 5 ms wait that takes 5 ms rather than a tick. The threshold is half,
    # not "faster": the claim is that the wait becomes the number asked for, and
    # anything between 5 and 8 ms would still be a scheduler artefact worth
    # failing on.
    awk -v a="$s5" -v b="$hr1" 'BEGIN { exit !(b < a / 2) }' \
      && ok "a 5 ms high-resolution timer wait takes ${hr1} ms against Sleep(5)'s ${s5} ms, with no timeBeginPeriod" \
      || bad "the high-resolution timer is not usefully shorter than Sleep(5) (${hr1} vs ${s5} ms)"
  else
    bad "no tick numbers came back"
  fi

  echo
  echo "== 4. the taskbar, photographed =="
  int wcshot "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe shot --class Shell_TrayWnd --out $DESKDIR_WIN\\taskbar.bmp > $DESKDIR_WIN\\shot.txt 2>&1" \
    >/dev/null 2>&1
  fetch taskbar.bmp stage8-taskbar-empty.png "the taskbar with no compositor window"
}

# Pull a BMP off the guest and convert it. `convert` is in the dev shell for
# exactly this (see flake.nix); a PNG is what a repository should hold.
fetch() {
  local remote="$1" local_png="$2" caption="$3"
  scp "${SSHOPT[@]}" "$GUEST:wcdesk/$remote" "$RUNDIR/$remote" >/dev/null 2>&1
  if [ -s "$RUNDIR/$remote" ]; then
    mkdir -p "$OUT"
    if magick "$RUNDIR/$remote" "$OUT/$local_png" 2>/dev/null \
       || convert "$RUNDIR/$remote" "$OUT/$local_png" 2>/dev/null; then
      ok "$caption -> docs/stage8/$local_png ($(identify -format '%wx%h' "$OUT/$local_png" 2>/dev/null))"
    else
      bad "could not convert $remote"
    fi
  else
    bad "$caption: nothing came back"
  fi
}

# ---------------------------------------------------------------- window -----
# The compositor, on the desktop, with an unmodified third-party client on the
# other side of the hypervisor -- stage 4's topology with stage 8's session.
do_window() {
  echo
  echo "== 5. a real client's window, on the real desktop =="
  if [ -z "$HOST" ]; then
    miss "WC_HOST unset: no machine shares a filesystem with the guest, so no client"
    miss "WC_HOST unset: no window capture"
    return
  fi
  [ -n "$HOST_WESTON" ] || { miss "WC_HOST_WESTON unset: no weston-terminal to run"; return; }

  local sock="wg-s8-$$"
  # Stale compositors go **first**, before the frame directory is emptied. An
  # interrupted run leaves one alive for up to its `--timeout`, still dumping
  # into the same directory and still holding the same log file, and a wipe that
  # happens before the kill leaves the new run counting the old run's frames and
  # reading the old run's log. That was diagnosed twice as "the capture does not
  # match the dump" before it was diagnosed as this.
  kill_ours wg-s8 >/dev/null 2>&1
  g "rm -f $APPDATA_WIN/wg-s8-* 2>/dev/null; true" >/dev/null 2>&1
  sleep 2
  h "mkdir -p $HOST_DIR $SHARE; rm -rf $SHARE/frames; mkdir -p $SHARE/frames; chmod 777 $SHARE/frames" \
    >/dev/null
  # A *Linux* running executable cannot be written to either: `scp` over a live
  # `wc-proxy` fails with `dest open ...: Failure`, which is `ETXTBSY` wearing
  # the same disguise as the Windows-side lesson in `wc-guest.sh`'s header. A
  # previous run that was interrupted between starting the proxy and killing it
  # is the usual cause. Matched on the full path so that another worktree's
  # proxy, in its own directory, is left alone.
  h "pkill -f '^$HOST_DIR/wc-proxy' 2>/dev/null; sleep 1; true" >/dev/null 2>&1
  scp "${SSHOPT[@]}" "$LINUX_BIN/wc-proxy" "$HOST:$HOST_DIR/" >/dev/null || {
    bad "could not copy wc-proxy to $HOST -- something is still holding it"; return; }
  h "chmod +x $HOST_DIR/wc-proxy" >/dev/null

  local arena=$((64 * 1024 * 1024))
  h ": >$SHARE/arena; truncate -s $arena $SHARE/arena; chmod 666 $SHARE/arena" >/dev/null

  # The compositor, on the desktop, detached: a scheduled task is not a child of
  # this ssh session and does not die with it.
  # `--dump-frames` is not decoration here: a PPM is written from `present`, on
  # the UI thread, so counting files in that directory is counting the times the
  # UI thread woke up and repainted. That is the whole of the modal-loop check
  # below, and it is why the frames go to the *host's* filesystem where they can
  # be counted while the guest is busy.
  int_bg wccomp "$DESKDIR_WIN\\wc-compositor.exe" \
    "--socket $sock --timeout 210 --log $DESKDIR_WIN\\comp-$sock.log --arena 0:$arena:$SHARE_WIN\\arena --dump-frames $SHARE_WIN\\frames" \
    >/dev/null 2>&1
  sleep 6
  # Everything below addresses windows by pid rather than by class. See
  # `comp_pid`: another worktree's compositor registers the same class.
  # Polled rather than slept for: a scheduled task's process appears when the
  # scheduler gets round to it, which on a loaded guest is several seconds after
  # `Start-ScheduledTask` returns.
  local CPID=""; local t
  for t in 1 2 3 4 5 6; do
    CPID=$(comp_pid "$sock")
    [ -n "$CPID" ] && break
    sleep 4
  done
  if [ -z "$CPID" ]; then
    bad "the compositor did not start on the interactive desktop"
    return
  fi
  note "our compositor is pid $CPID (socket $sock)"

  ssh "${SSHOPT[@]}" "$GUEST" \
    "export MSYS_NO_PATHCONV=1; cd wcdesk && ./probe-relay.exe $PORT '$APPDATA_WIN\\$sock' --timeout 120" \
    >"$RUNDIR/relay.log" 2>&1 &
  local relay=$!
  sleep 3

  # The client and the proxy both live on WC_HOST, which is the machine that can
  # see the arena. Driven from there because the arena is a filesystem fact.
  h "cd $HOST_DIR && cat > s8.sh <<'EOS'
#!/usr/bin/env bash
set -uo pipefail
RUN=\$(mktemp -d /tmp/wc-s8-XXXXXX); chmod 700 \"\$RUN\"
ssh -i $HOST_KEY -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR \
    -N -L \"\$RUN/up.sock:127.0.0.1:$PORT\" $HOST_GUEST &
FWD=\$!
sleep 4
if [ -S \"\$RUN/up.sock\" ]; then
  XDG_RUNTIME_DIR=\"\$RUN\" $HOST_DIR/wc-proxy --upstream \"\$RUN/up.sock\" --socket wl-proxy \
     --runtime-dir \"\$RUN\" --transport arena --arena-path $SHARE/arena --arena-id 0 \
     --arena-size $arena --copy damage --accept-timeout 25 --timeout 90 >\"\$RUN/proxy.log\" 2>&1 &
  PROXY=\$!
  sleep 2
  XDG_RUNTIME_DIR=\"\$RUN\" WAYLAND_DISPLAY=wl-proxy $HOST_WESTON/$CLIENT >\"\$RUN/cli.log\" 2>&1 &
  CLI=\$!
  echo S8-CLIENT-STARTED
  sleep 120
  kill \$CLI \$PROXY 2>/dev/null
else
  echo S8-FAIL: no forward socket
fi
kill \$FWD 2>/dev/null
sleep 1
echo S8-PROXY-BEGIN; cat \"\$RUN/proxy.log\" 2>/dev/null; echo S8-PROXY-END
echo S8-CLIENT-BEGIN; head -20 \"\$RUN/cli.log\" 2>/dev/null; echo S8-CLIENT-END
rm -rf \"\$RUN\"
EOS
chmod +x s8.sh" >/dev/null
  h "$HOST_DIR/s8.sh" >"$RUNDIR/s8.log" 2>&1 &
  local drv=$!

  # Give the client time to map, then photograph while it is up.
  sleep 22
  int wcwin "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe windows > $DESKDIR_WIN\\windows.txt 2>&1" >/dev/null 2>&1
  g "cat $DESKDIR/windows.txt" >"$RUNDIR/windows.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/windows.txt" | head -40
  cp "$RUNDIR/windows.txt" "$OUT/windows$SUFFIX.txt" 2>/dev/null

  # The class, not the title: the title is the *client's* ("Wayland Terminal"),
  # which is the thing being checked, so selecting on it would be circular.
  int wcshot "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe shot --pid $CPID --class WaycomposeToplevel --out $DESKDIR_WIN\\win.bmp > $DESKDIR_WIN\\shot.txt 2>&1" \
    >/dev/null 2>&1
  int wcshot "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe shot --class Shell_TrayWnd --out $DESKDIR_WIN\\tray2.bmp >> $DESKDIR_WIN\\shot.txt 2>&1" \
    >/dev/null 2>&1
  int wcshot "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe shot --screen --out $DESKDIR_WIN\\screen.bmp >> $DESKDIR_WIN\\shot.txt 2>&1" \
    >/dev/null 2>&1
  g "cat $DESKDIR/shot.txt" >"$RUNDIR/shot.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/shot.txt"

  fetch win.bmp    "stage8-window$SUFFIX.png"  "the client's window, frame and all"
  fetch tray2.bmp  "stage8-taskbar$SUFFIX.png" "the taskbar with the window's button in it"
  fetch screen.bmp "stage8-screen$SUFFIX.png"  "the screen, which is the lock screen"

  # Is the blit the right way up (§8.11 item 3), and is it the right rectangle?
  #
  # Both questions are one image comparison. `--dump-frames` writes the
  # compositor's *last view of the client's pixels* -- the shadow, before GDI --
  # and the `PrintWindow` capture is what came out the other end. So crop the
  # window's client area out of the capture, crop the same-sized rectangle out
  # of the top-left of the buffer, and compare it against that crop and against
  # the vertically flipped buffer. Whichever is closer is the orientation, and
  # the ratio between them is how confident that is.
  #
  # This is the check §8.4 says the project needs and did not have: a dump alone
  # cannot see a blit bug, and a screenshot alone cannot tell a blit bug from a
  # stride bug. It only means anything with both halves.
  # It is compared against **every** dumped frame rather than the newest one,
  # and the best match of each orientation is what is reported. The capture and
  # the dump are taken from two different processes at two different instants,
  # so the newest file is not reliably the frame that was on screen; against one
  # arbitrary frame the two orientations came out 11593 and 12976, which decides
  # nothing. Against the right frame they are 779 and 5853. Taking the minimum
  # over the run is the only version of this that answers the question asked
  # rather than a question about timing.
  local cw ch up dn f n
  cw=$(grep -oE " client [0-9]+x[0-9]+" "$RUNDIR/comp.log" 2>/dev/null | tail -1 | sed "s/ client //")
  if [ -z "$cw" ]; then
    g "cat $DESKDIR/comp-$sock.log" >"$RUNDIR/comp.log" 2>&1
    cw=$(grep -oE " client [0-9]+x[0-9]+" "$RUNDIR/comp.log" | tail -1 | sed "s/ client //")
  fi
  mkdir -p "$RUNDIR/frames"
  scp "${SSHOPT[@]}" "$HOST:$SHARE/frames/*.ppm" "$RUNDIR/frames/" >/dev/null 2>&1
  n=$(ls "$RUNDIR/frames"/*.ppm 2>/dev/null | wc -l)
  if [ -n "$cw" ] && [ -s "$RUNDIR/win.bmp" ] && [ "$n" -gt 0 ]; then
    ch=${cw#*x}; cw=${cw%x*}
    # The client area sits at (8, 39) inside a WS_OVERLAPPEDWINDOW at 96 dpi,
    # which the compositor's `nonclient 16x39` line is the authority for.
    magick "$RUNDIR/win.bmp" -crop "${cw}x${ch}+8+39" +repage "$RUNDIR/client.png"
    up=""; dn=""
    for f in $(ls -t "$RUNDIR/frames"/*.ppm | head -40); do
      magick "$f" -crop "${cw}x${ch}+0+0" +repage "$RUNDIR/up.png" 2>/dev/null || continue
      magick "$f" -flip -crop "${cw}x${ch}+0+0" +repage "$RUNDIR/dn.png" 2>/dev/null || continue
      local a b
      a=$(magick compare -metric RMSE "$RUNDIR/up.png" "$RUNDIR/client.png" null: 2>&1 | awk '{print $1}')
      b=$(magick compare -metric RMSE "$RUNDIR/dn.png" "$RUNDIR/client.png" null: 2>&1 | awk '{print $1}')
      awk -v x="$a" -v y="${up:-1e30}" 'BEGIN { exit !(x + 0 < y + 0) }' && up=$a
      awk -v x="$b" -v y="${dn:-1e30}" 'BEGIN { exit !(x + 0 < y + 0) }' && dn=$b
    done
    note "best RMSE over $n dumped frames, ${cw}x${ch}: upright $up, flipped $dn"
    if awk -v a="$up" -v b="$dn" 'BEGIN { exit !(a > 0 && b > 2 * a) }'; then
      ok "the blit is the right way up: the client area is the buffer's top-left crop, not its flip"
    elif awk -v a="$up" -v b="$dn" 'BEGIN { exit !(a > 0 && a > 2 * b) }'; then
      bad "the window is UPSIDE DOWN: the flipped buffer matches better"
    else
      note "orientation undecided ($up vs $dn) -- no dumped frame is close enough to the captured one to decide"
    fi
  else
    note "no frame dump and window capture pair to compare"
  fi

  # Which row of the enumeration is ours: the `shot` above printed the handle it
  # captured, and that one was selected by pid. Grepping the class name here
  # would re-open the door `comp_pid` closed.
  local hw line
  hw=$(grep -oE "hwnd 0x[0-9a-f]+" "$RUNDIR/shot.txt" 2>/dev/null | head -1 | sed 's/hwnd //')
  line=$(grep -F "$hw " "$RUNDIR/windows.txt" 2>/dev/null | grep WaycomposeToplevel)
  if [ -n "$line" ]; then
    ok "the compositor's window is a top-level window on WinSta0"
    echo "        $line"
    # A substring, not the whole caption: `weston-presentation-shm` rewrites its
    # own title on every frame with the delay it measured, so an equality check
    # here would be testing that the client is *not* animating.
    echo "$line" | grep -q "\"[^\"]*$CLIENT_TITLE" \
      && ok "and its caption is the client's own title, $CLIENT_TITLE  <-- goal-firefox.md, clause 2" \
      || bad "the caption is not the title the client set ($CLIENT_TITLE)"
    echo "$line" | grep -qE '^0x[0-9a-f]+ +yes' \
      && ok "and Windows gives it a taskbar button  <-- goal-firefox.md, clause 1" \
      || bad "the window exists but is not a taskbar entry"
  else
    bad "no compositor window on the desktop"
  fi

  echo
  echo "== 6. the modal resize loop (§8.11 item 4) =="
  # Count the frames the UI thread has repainted, put the window into the loop,
  # and keep counting *while it is in there*. §8.2 predicted that a
  # `PostThreadMessage` wake would be discarded here and a `PostMessage` to a
  # window would not; this is the first time anything has been inside the loop
  # to find out.
  #
  # The samples are a series rather than a before/after pair, and that is not
  # thoroughness: the first attempt took one sample four seconds in, saw the
  # same count as before, and could not tell "the wake was discarded" from "the
  # client had nothing to draw". A series over the hold answers the first
  # question, and running an animating client answers the second.
  local f0 f1 series=""
  f0=$(h "ls $SHARE/frames/*.ppm 2>/dev/null | wc -l")
  int wcdrag "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe drag --pid $CPID --class WaycomposeToplevel --steps 24 --hold-ms 14000 > $DESKDIR_WIN\\drag.txt 2>&1" \
    >/dev/null 2>&1 &
  local dragpid=$!
  # The loop is entered ~0.4 s in and left ~17 s in (24 steps, then the hold).
  local fmid=$f0 i
  for i in 1 2 3 4 5; do
    sleep 3
    fmid=$(h "ls $SHARE/frames/*.ppm 2>/dev/null | wc -l")
    series="$series $fmid"
    if [ "$i" = 3 ]; then
      # Photograph it *while it is inside the loop*: a window that has stopped
      # repainting during a resize is the failure, and it is a picture.
      int wcshot2 "cmd.exe" \
        "/c $DESKDIR_WIN\\wc-desk.exe shot --pid $CPID --class WaycomposeToplevel --out $DESKDIR_WIN\\drag.bmp >> $DESKDIR_WIN\\shot.txt 2>&1" \
        >/dev/null 2>&1
    fi
  done
  wait $dragpid 2>/dev/null
  f1=$(h "ls $SHARE/frames/*.ppm 2>/dev/null | wc -l")
  g "cat $DESKDIR/drag.txt" >"$RUNDIR/drag.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/drag.txt"
  cp "$RUNDIR/drag.txt" "$OUT/drag$SUFFIX.txt" 2>/dev/null
  note "frames presented: $f0 before the loop, then$series inside it, $f1 after"
  fetch drag.bmp "stage8-window-resizing$SUFFIX.png" "the window mid-resize, inside the modal loop"

  if grep -q "^drag.entered *yes" "$RUNDIR/drag.txt"; then
    grep -q "^drag.grew *yes" "$RUNDIR/drag.txt" \
      && ok "the window really entered Windows' modal sizing loop and grew" \
      || bad "the loop was entered but the window did not end up bigger"
    grep -q "^drag.responsive *yes" "$RUNDIR/drag.txt" \
      && ok "the UI thread still answers messages from inside the loop" \
      || bad "the UI thread stopped answering inside the loop"
    # Only ask this of a client that had something to draw. An idle
    # `weston-terminal` produces four frames in two minutes, and "no new frame
    # in twelve seconds" from one of those is not evidence that the wake was
    # discarded -- it is the null result. `WC_CLIENT=weston-presentation-shm` is
    # the run that answers it, and the threshold below is what separates the two
    # cases rather than papering over them.
    if [ "$f0" -lt 30 ]; then
      miss "the repaint claim needs an animating client; this one committed $f0 frames before the loop"
    elif [ "$fmid" -gt "$f0" ]; then
      ok "the compositor presented $((fmid - f0)) frames *inside* the loop -- §8.2's PostMessage wake survives it"
    else
      bad "no frame was presented during the modal loop: the wake is being discarded"
    fi
  else
    # Not a pass and not a failure of the compositor: `DefWindowProc` declines
    # to start a sizing loop for a window on a desktop whose foreground belongs
    # to the lock screen, intermittently, and `wc-desk drag` already retries
    # three times. Saying so is better than either verdict.
    miss "the modal sizing loop would not start (the lock screen owns the foreground); retried 3 times"
    miss "so the in-loop repaint count says nothing this run"
  fi

  wait $drv 2>/dev/null
  kill $relay 2>/dev/null; wait $relay 2>/dev/null
  g "cat $DESKDIR/comp-$sock.log" >"$RUNDIR/comp.log" 2>&1
  echo "        --- the compositor's own account, from the desktop:"
  grep -iE "dpi|screen|window .* (created|mapped)|caption|arena|configure" "$RUNDIR/comp.log" 2>/dev/null \
    | sed 's/^/        /' | head -20
  cp "$RUNDIR/comp.log" "$OUT/compositor$SUFFIX.log" 2>/dev/null
  sed -n '/S8-PROXY-BEGIN/,/S8-CLIENT-END/p' "$RUNDIR/s8.log" | sed 's/^/        /' | head -25
  kill_ours "$sock" >/dev/null 2>&1
}

case "$MODE" in
  deploy) do_deploy ;;
  probe)  do_probe ;;
  window) do_window ;;
  all)    do_deploy; do_probe; do_window ;;
  *) echo "usage: $0 {deploy|probe|window|all}"; exit 2 ;;
esac

echo
echo "== $pass passed, $fail failed, $skip skipped =="
[ "$fail" -eq 0 ]
