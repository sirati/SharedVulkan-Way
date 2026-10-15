#!/usr/bin/env bash
# Open a real client's menu near the edge of the guest's real screen, and
# photograph where it lands.
#
#   scripts/wc-popup.sh menu [EXE]   one run; EXE defaults to wc-compositor.exe
#   scripts/wc-popup.sh both         the same run twice, before and after
#
# Run from inside `nix develop`, after `scripts/wc-desktop.sh deploy`. Output
# lands in docs/stage38/.
#
# WHY THIS EXISTS AND WHY IT IS NOT PART OF wc-desktop.sh
#
# docs/waycompose-design.md §38 is a **placement** bug: a menu opened near the
# right or bottom edge of the screen had the part that did not fit hanging off
# the display. Every existing harness photographs a window the compositor
# placed by itself, and none of them opens a menu, moves the parent anywhere in
# particular, or looks at where the menu ended up relative to the monitor. So
# the evidence for the fix needs three things nothing else does:
#
#   1. the parent window **deliberately** in the bottom right corner of the
#      work area, because that is where the bug is and CW_USEDEFAULT never
#      puts a window there;
#   2. a **real right-click** from Windows' own input queue, because the
#      compositor's `--click` is the headless platform's and cannot run here;
#   3. the popup's rectangle in **screen** coordinates, next to the work area
#      in the same coordinates, because "it looks right" is not the claim —
#      the claim is that the menu is inside `rcWork`.
#
# The third is what makes this more than a photograph: the numbers are printed
# and compared, and the picture is the corroboration rather than the evidence.
#
# WHAT IS NOT CLAIMED
#
# One monitor, one DPI, one taskbar position. This project has no multi-monitor
# support and no scaling (§38.6), and a run on a second screen would be
# measuring something nobody has built.
set -uo pipefail

MODE="${1:-menu}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
OUT="$ROOT/docs/stage38"

GUEST="${WC_GUEST:-<guest-user>@<guest-ip>}"
# Exported as resolved: `wc_topology_declare cross-vm` (wc-topology.sh) checks
# WC_GUEST, and a default that lived only in GUEST made it refuse cross-vm
# unless the caller had happened to set the variable this line defaults.
export WC_GUEST="$GUEST"
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"
DESKNAME="${WC_DESKNAME:-wcdesk38}"
DESKDIR="/c/Users/<guest-user>/$DESKNAME"
DESKDIR_WIN="C:\\Users\\<guest-user>\\$DESKNAME"
APPDATA_WIN='C:\Users\<guest-user>\AppData\Local\waycompose'
PORT="${WC_PORT_BASE:-57471}"

HOST="${WC_HOST:-}"
HOST_DIR="${WC_HOST_DIR:-/tmp/wc-stage38}"
HOST_GUEST="${WC_HOST_GUEST:-<guest-user>@<guest-nat-ip>}"
HOST_KEY="${WC_HOST_KEY:-\$HOME/.ssh/wc_guest}"
HOST_WESTON="${WC_HOST_WESTON:-}"
CLIENT="${WC_CLIENT:-weston-terminal}"
# **Which client is not a detail here, it is the experiment.**
#
# `xdg_positioner.set_constraint_adjustment` is a *declaration*: the compositor
# may only move a popup in the ways the client named, and a client that named
# nothing is entitled to a menu that hangs off the screen. Measured with
# `WAYLAND_DEBUG=1` against the Linux build:
#
#   weston-terminal   sets anchor, size and gravity, and **no** constraint
#                     adjustment at all. Its menu must be placed exactly where
#                     it asked, off the screen and all — so it is this run's
#                     *negative* control, and a compositor that moved it would
#                     be failing.
#   firefox           sets 63, which is all six bits. Its menu is the one the
#                     user reported and the one that must come back on screen.
#
# So `both` runs Firefox, and `WC_CLIENT=weston-terminal` runs the control.
HOST_FIREFOX="${WC_HOST_FIREFOX:-}"
# With Firefox chosen, `$CLIENT` has to move with it: a run that started Firefox
# and then said "weston-terminal" in its own verdict would be reporting the
# wrong experiment. `wc-desktop.sh` learned this about a caption assertion.
[ -n "$HOST_FIREFOX" ] && CLIENT="firefox"

# The transport, and it is deliberately the **shared-file** one rather than
# stage 13's guest-RAM arena.
#
# This section is about where a window is put, not about how its pixels get
# there, and the two are independent: the popup's `HWND` is placed by
# `SetWindowPos` from a rectangle `state.rs` computed, whatever carried the
# buffer. The arena needs QMP, `/proc/<qemu-pid>/fd` and therefore root on the
# host — three things between a placement question and its answer, each of
# which can fail for reasons that say nothing about placement. The shared-file
# transport needs none of them and is what §32's runs are using against this
# same guest today.
SHARE="${WC_HOST_SHARE:-/srv/winshare/wc38}"
SHARE_WIN="${WC_GUEST_SHARE:-Z:\\wc38}"
COMP_SHARE_ARGS="--shared-dir $SHARE_WIN\\keymaps --shared-peer-prefix $SHARE/keymaps/"

SSHOPT=(-o IdentityAgent="$AGENT" -o BatchMode=yes -o LogLevel=ERROR
        -o StrictHostKeyChecking=accept-new)

RUNDIR=$(mktemp -d /tmp/wc-popup-XXXXXX); chmod 700 "$RUNDIR"
trap 'rm -rf "$RUNDIR"' EXIT

pass=0; fail=0; skip=0
ok()   { echo "  PASS  $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail + 1)); }
note() { echo "  ----  $*"; }
miss() { echo "  SKIP  $*"; skip=$((skip + 1)); }

g() { ssh "${SSHOPT[@]}" "$GUEST" "$@"; }
h() { [ -n "$HOST" ] && ssh "${SSHOPT[@]}" "$HOST" "$@"; }

# STAGE 42. `fresh` and `deployed_is`. Sourced after the reporters and `g`/`h`.
. "$(dirname "$0")/wc-binaries.sh"
. "$(dirname "$0")/wc-topology.sh"

int_bg() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\runint.ps1' -Exe '$2' -Arguments '$3' -Name '$1'" 2>&1
}
int() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\runint.ps1' -Exe '$2' -Arguments '$3' -Name '$1' -Wait -TimeoutSec 120" 2>&1
}
kill_ours() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\ours38.ps1' -Match '$1'" 2>&1
}
comp_pid() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\ours38.ps1' -Match '$1' -PidOnly" 2>/dev/null | tr -d '\r' | head -1
}

# `wc-desktop.sh`'s `killours.ps1` filters on `Name = 'wc-compositor.exe'`, and
# this script runs the **two** binaries side by side — the one before the fix
# and the one after — so one of them is called `wc-compositor-before.exe` and is
# invisible to that filter. The symptom is "the compositor did not start on the
# interactive desktop" while it is plainly running, which cost a round.
#
# `LIKE 'wc-compositor%'` and a socket-name match instead. Both halves are
# needed: the prefix so either binary is seen, and the socket so that another
# worktree's compositor — same image name, same guest — is never touched.
write_ours_ps1() { cat >"$1" <<'PS4'
# Generated by scripts/wc-popup.sh; do not edit on the guest.
param([Parameter(Mandatory=$true)][string]$Match, [switch]$PidOnly)
Get-CimInstance Win32_Process -Filter "Name LIKE 'wc-compositor%'" |
  Where-Object { $_.CommandLine -and $_.CommandLine.Contains($Match) } |
  ForEach-Object {
    if ($PidOnly) { Write-Output $_.ProcessId }
    else {
      Write-Output ("killing pid " + $_.ProcessId)
      Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }
  }
PS4
}

# -------------------------------------------------------------- spawn.ps1 ---
#
# Where the window landed, and nothing else: no move, no click, no input of any
# kind — so this is the half that can run against a **locked** guest, where
# `mouse_event` reaches Winlogon's secure desktop rather than ours.
#
# The picture is `PrintWindow` on our own window rather than a screen grab, for
# the reason §13's header gives: it reads DWM's redirection surface, which is
# up to date for a composited window whether or not a lock screen is on top of
# it. So the window can be photographed when the screen cannot.
write_spawn_ps1() { cat >"$1" <<'PS6'
# Generated by scripts/wc-popup.sh; do not edit on the guest.
param(
  [Parameter(Mandatory=$true)][int]$TargetPid,
  [string]$Report = "spawn.txt",
  [string]$Shot = ""
)
$ErrorActionPreference = "Stop"
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
[StructLayout(LayoutKind.Sequential)]
public struct RECT2 { public int left; public int top; public int right; public int bottom; }
public class WcS {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT2 r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool SystemParametersInfoW(uint a, uint b, out RECT2 r, uint c);
}
"@
$work = New-Object RECT2
[void][WcS]::SystemParametersInfoW(0x0030, 0, [ref]$work, 0)
$out = New-Object System.Collections.ArrayList
[void]$out.Add(("work-area {0}x{1}+{2}+{3}" -f ($work.right - $work.left), ($work.bottom - $work.top), $work.left, $work.top))

$found = New-Object System.Collections.ArrayList
$cb = [WcS+EnumProc]{
  param($h, $l)
  $p = 0
  [void][WcS]::GetWindowThreadProcessId($h, [ref]$p)
  if ($p -eq $TargetPid -and [WcS]::IsWindowVisible($h)) {
    $r = New-Object RECT2
    [void][WcS]::GetWindowRect($h, [ref]$r)
    $tb = New-Object System.Text.StringBuilder 256
    [void][WcS]::GetWindowTextW($h, $tb, 256)
    [void]$found.Add([pscustomobject]@{ hwnd = $h; rect = $r; title = $tb.ToString();
                                        style = [int64][WcS]::GetWindowLongPtrW($h, -16) })
  }
  return $true
}
[void][WcS]::EnumWindows($cb, [IntPtr]::Zero)
foreach ($w in $found) {
  $kind = if (($w.style -band 0x80000000L) -ne 0) { "popup" } else { "toplevel" }
  $inside = ($w.rect.left -ge $work.left -and $w.rect.top -ge $work.top -and
             $w.rect.right -le $work.right -and $w.rect.bottom -le $work.bottom)
  [void]$out.Add(("{0} {1}x{2}+{3}+{4} inside-work-area={5} title={6}" -f `
    $kind, ($w.rect.right - $w.rect.left), ($w.rect.bottom - $w.rect.top),
    $w.rect.left, $w.rect.top, $inside, $w.title))
}
if ($Shot -and $found.Count -gt 0) {
  $top = $found | Where-Object { ($_.style -band 0x80000000L) -eq 0 } | Select-Object -First 1
  if ($top) {
    & (Join-Path $PSScriptRoot "wc-desk.exe") shot --hwnd ("0x{0:x}" -f [int64]$top.hwnd) --out $Shot | Out-Null
  }
}
$out | Out-File -Encoding ascii (Join-Path $PSScriptRoot $Report)
$out
PS6
}

# --------------------------------------------------------------- menu.ps1 ---
#
# The interactive half, generated here for `wc-desktop.sh`'s reason: it is one
# half of this script and editing it on the guest would fork it silently.
#
# **The click is `mouse_event`, not a posted `WM_RBUTTONDOWN`.** A posted
# message would go to the window this script names and would prove nothing
# about hit-testing; `SetCursorPos` plus `mouse_event` puts the press into the
# system's input queue, so it is routed by Windows exactly as a user's would be
# and arrives at the compositor as the same `WM_RBUTTONDOWN` a hand produces.
#
# **The window is moved by its bottom right corner, not to a coordinate.** The
# work area is asked for rather than assumed, because the taskbar's height is
# the whole point of `rcWork` and hard-coding 1080-40 would be measuring this
# script's guess instead of the machine.
write_menu_ps1() { cat >"$1" <<'PS3'
# Generated by scripts/wc-popup.sh; do not edit on the guest.
param(
  [Parameter(Mandatory=$true)][int]$TargetPid,
  [int]$Inset = 40,
  [string]$Report = "menu.txt",
  [string]$Shot = ""
)
$ErrorActionPreference = "Stop"
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
[StructLayout(LayoutKind.Sequential)]
public struct RECT { public int left; public int top; public int right; public int bottom; }
public class WcU {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
  [DllImport("user32.dll")] public static extern bool SystemParametersInfoW(uint a, uint b, out RECT r, uint c);
  [DllImport("user32.dll")] public static extern IntPtr MonitorFromWindow(IntPtr h, uint f);
}
"@
function Ours($targetPid) {
  $found = New-Object System.Collections.ArrayList
  $cb = [WcU+EnumProc]{
    param($h, $l)
    $p = 0
    [void][WcU]::GetWindowThreadProcessId($h, [ref]$p)
    if ($p -eq $targetPid -and [WcU]::IsWindowVisible($h)) {
      $sb = New-Object System.Text.StringBuilder 256
      [void][WcU]::GetClassNameW($h, $sb, 256)
      $tb = New-Object System.Text.StringBuilder 256
      [void][WcU]::GetWindowTextW($h, $tb, 256)
      $r = New-Object RECT
      [void][WcU]::GetWindowRect($h, [ref]$r)
      [void]$found.Add([pscustomobject]@{
        hwnd = $h; class = $sb.ToString(); title = $tb.ToString();
        style = [int64][WcU]::GetWindowLongPtrW($h, -16); rect = $r })
    }
    return $true
  }
  [void][WcU]::EnumWindows($cb, [IntPtr]::Zero)
  return $found
}
function Show($tag, $wins, $work) {
  foreach ($w in $wins) {
    $kind = if (($w.style -band 0x80000000L) -ne 0) { "popup" } else { "toplevel" }
    $inside = ($w.rect.left -ge $work.left -and $w.rect.top -ge $work.top -and
               $w.rect.right -le $work.right -and $w.rect.bottom -le $work.bottom)
    "{0} {1,-8} {2}x{3}+{4}+{5} inside-work-area={6} title={7}" -f `
      $tag, $kind, ($w.rect.right - $w.rect.left), ($w.rect.bottom - $w.rect.top),
      $w.rect.left, $w.rect.top, $inside, $w.title
  }
}

$work = New-Object RECT
[void][WcU]::SystemParametersInfoW(0x0030, 0, [ref]$work, 0)   # SPI_GETWORKAREA
$out = New-Object System.Collections.ArrayList
[void]$out.Add(("work-area {0}x{1}+{2}+{3}" -f ($work.right - $work.left), ($work.bottom - $work.top), $work.left, $work.top))

$wins = Ours $TargetPid
$top = $wins | Where-Object { ($_.style -band 0x80000000L) -eq 0 } | Select-Object -First 1
if (-not $top) {
  [void]$out.Add("no-toplevel for pid $TargetPid")
  $out | Out-File -Encoding ascii (Join-Path $PSScriptRoot $Report)
  $out; exit 1
}
$w = $top.rect.right - $top.rect.left
$hgt = $top.rect.bottom - $top.rect.top
# Bottom right corner of the work area: the one place on the desktop where a
# menu that is not constrained cannot possibly fit.
$x = $work.right - $w
$y = $work.bottom - $hgt
# SWP_NOSIZE | SWP_NOZORDER
[void][WcU]::SetWindowPos($top.hwnd, [IntPtr]::Zero, $x, $y, 0, 0, 0x0001 -bor 0x0004)
[void][WcU]::SetForegroundWindow($top.hwnd)
Start-Sleep -Milliseconds 800
$moved = (Ours $TargetPid | Where-Object { $_.hwnd -eq $top.hwnd })
[void]$out.Add((Show "parent" @($moved) $work))

# The click: `$Inset` pixels in from the window's bottom right corner, which is
# `$Inset` pixels in from the work area's.
$cx = $moved.rect.right - $Inset
$cy = $moved.rect.bottom - $Inset
[void]$out.Add(("click {0},{1}" -f $cx, $cy))
[void][WcU]::SetCursorPos($cx, $cy)
Start-Sleep -Milliseconds 300
[WcU]::mouse_event(0x0008, 0, 0, 0, [UIntPtr]::Zero)   # RIGHTDOWN
Start-Sleep -Milliseconds 120
[WcU]::mouse_event(0x0010, 0, 0, 0, [UIntPtr]::Zero)   # RIGHTUP

# **Polled, and photographed from inside this task.** A menu is a grab: it goes
# away the moment anything else takes the focus, and a fixed sleep followed by
# a *separate* scheduled task to take the picture lost one — the compositor's
# log had the popup mapped and destroyed while the enumeration two seconds
# later saw nothing. Waiting for it to appear and capturing immediately is the
# difference between a photograph of the menu and a photograph of the desktop.
$popup = $null
for ($i = 0; $i -lt 60; $i++) {
  Start-Sleep -Milliseconds 200
  $popup = Ours $TargetPid | Where-Object { ($_.style -band 0x80000000L) -ne 0 } | Select-Object -First 1
  if ($popup) { break }
}
if ($popup -and $Shot) {
  & (Join-Path $PSScriptRoot "wc-desk.exe") shot --screen --out $Shot | Out-Null
}
if ($popup) { foreach ($line in (Show "after" @($popup) $work)) { [void]$out.Add($line) } }
foreach ($line in (Show "after" (Ours $TargetPid) $work)) { [void]$out.Add($line) }
$out | Out-File -Encoding ascii (Join-Path $PSScriptRoot $Report)
$out
PS3
}

# STAGE 42. The geometry was printed into the success message and compared with
# nothing, so a 1x1 or all-black capture read as a PASS — the same vacuous gate
# §41.8 fixed in `wc-taskbar.sh`, found here by the audit that followed. `kind`
# is required rather than defaulted for §41.8's other reason: `screen-*.bmp` is
# a whole desktop and `win-*.bmp` is a window, and one bound for both would be
# an impossible threshold for one of them.
fetch() {
  local kind="${4:-window}"
  scp "${SSHOPT[@]}" "$GUEST:$DESKNAME/$1" "$RUNDIR/$1" >/dev/null 2>&1
  if [ -s "$RUNDIR/$1" ]; then
    mkdir -p "$OUT"
    if magick "$RUNDIR/$1" "$OUT/$2" 2>/dev/null; then
      picture_is "$OUT/$2" "$kind" "$3 -> docs/stage38/$2"
    else
      bad "could not convert $1"
    fi
  else
    bad "$3: nothing came back"
  fi
}

# The compositor on the interactive desktop, the relay, the proxy and one
# client — the part both modes need, so that neither can drift from the other.
# Sets $CPID, $RELAY_PID and $DRV_PID; returns non-zero if the compositor
# never appeared.
start_side() {
  local sock="$1" exe="$2" client_cmd="$3"
  h "mkdir -p $HOST_DIR $SHARE/pools $SHARE/keymaps; chmod 777 $SHARE/pools $SHARE/keymaps" >/dev/null
  h "pkill -f '^$HOST_DIR/wc-proxy' 2>/dev/null; sleep 1; true" >/dev/null 2>&1
  # STAGE 42. Fresh here, and identical there. This harness never checked either:
  # it copied whatever was at `$LINUX_BIN/wc-proxy` and trusted `scp`'s exit
  # status, which reports that the transfer ran rather than that the bytes
  # landed. §35.4 and §35.6 item 6.
  fresh "$LINUX_BIN/wc-proxy" "cargo build -p wc-proxy" || return 1
  scp "${SSHOPT[@]}" "$LINUX_BIN/wc-proxy" "$HOST:$HOST_DIR/" >/dev/null || {
    bad "could not copy wc-proxy to $HOST"; return; }
  h "chmod +x $HOST_DIR/wc-proxy" >/dev/null
  deployed_is "$LINUX_BIN/wc-proxy" "$HOST_DIR/wc-proxy" h "re-run this with no wc-proxy of ours live on $HOST" || return 1

  write_menu_ps1 "$RUNDIR/menu.ps1"
  write_ours_ps1 "$RUNDIR/ours38.ps1"
  write_spawn_ps1 "$RUNDIR/spawn.ps1"
  scp "${SSHOPT[@]}" "$RUNDIR/menu.ps1" "$RUNDIR/ours38.ps1" "$RUNDIR/spawn.ps1" \
    "$GUEST:$DESKNAME/" >/dev/null

  int_bg wcpopup "$DESKDIR_WIN\\$exe" \
    "--socket $sock --timeout 240 --log $DESKDIR_WIN\\comp-$sock.log $COMP_SHARE_ARGS" >/dev/null 2>&1
  local t
  CPID=""
  for t in 1 2 3 4 5 6; do
    CPID=$(comp_pid "$sock"); [ -n "$CPID" ] && break; sleep 4
  done
  [ -n "$CPID" ] || { bad "the compositor did not start on the interactive desktop"; return; }
  note "our compositor is pid $CPID (socket $sock)"

  ssh "${SSHOPT[@]}" "$GUEST" \
    "export MSYS_NO_PATHCONV=1; cd $DESKNAME && ./probe-relay.exe $PORT '$APPDATA_WIN\\$sock' --timeout 180" \
    >"$RUNDIR/relay.log" 2>&1 &
  RELAY_PID=$!
  sleep 3

  # The host side: an ssh forward to the guest's relay port, `wc-proxy`
  # translating the in-band `h` arguments, and one unmodified client. As the
  # invoking user, not root — see the transport note at the top.
  h "cd $HOST_DIR && cat > popup.sh <<'EOS'
#!/usr/bin/env bash
set -uo pipefail
RUN=\$(mktemp -d /tmp/wc-p38-XXXXXX); chmod 755 \"\$RUN\"
ssh -i $HOST_KEY -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR \
    -N -L \"\$RUN/up.sock:127.0.0.1:$PORT\" $HOST_GUEST >\"\$RUN/fwd.log\" 2>&1 &
FWD=\$!
sleep 4
if [ -S \"\$RUN/up.sock\" ]; then
  XDG_RUNTIME_DIR=\"\$RUN\" $HOST_DIR/wc-proxy --upstream \"\$RUN/up.sock\" --socket wl-proxy \
     --runtime-dir \"\$RUN\" --transport shared-file --pool-dir $SHARE/pools \
     --peer-prefix '$SHARE_WIN\\pools\\' \
     --copy damage --accept-timeout 30 --timeout 150 >\"\$RUN/proxy.log\" 2>&1 &
  P=\$!
  sleep 2
  mkdir -p \"\$RUN/home\" \"\$RUN/profile\"
  XDG_RUNTIME_DIR=\"\$RUN\" WAYLAND_DISPLAY=wl-proxy timeout 120 $client_cmd \
     >\"\$RUN/cli.log\" 2>&1 &
  C=\$!
  echo P38-CLIENT-STARTED
  wait \$C 2>/dev/null
  kill \$P 2>/dev/null; wait 2>/dev/null
else
  echo P38-FAIL: no forward socket
fi
kill \$FWD 2>/dev/null
echo P38-CLIENT-BEGIN; head -20 \"\$RUN/cli.log\" 2>/dev/null; echo P38-CLIENT-END
echo P38-PROXY-BEGIN; tail -20 \"\$RUN/proxy.log\" 2>/dev/null; echo P38-PROXY-END
rm -rf \"\$RUN\"
EOS
chmod +x popup.sh" >/dev/null
  h "$HOST_DIR/popup.sh" >"$RUNDIR/popup.log" 2>&1 &
  DRV_PID=$!
}

# Stop what `start_side` started, and print what the compositor said about
# placement. Only ever kills by the socket name this run invented: another
# worktree is allowed to be driving the same guest (§16.8).
stop_side() {
  local sock="$1" tag="$2"
  kill $RELAY_PID 2>/dev/null
  wait $DRV_PID 2>/dev/null
  sed -n '/P38-CLIENT-BEGIN/,/P38-CLIENT-END/p' "$RUNDIR/popup.log" | sed 's/^/        /' | head -8
  kill_ours "$sock" >/dev/null 2>&1
  g "cat $DESKDIR/comp-$sock.log" >"$RUNDIR/comp-$tag.log" 2>&1
  grep -E "work area is|wl_output is|xdg_popup created|may put popups|asked for .* and the work area|mapped .* as a popup" \
    "$RUNDIR/comp-$tag.log" | sed 's/^/        /' | head -10
  mkdir -p "$OUT"; cp "$RUNDIR/comp-$tag.log" "$OUT/comp-$tag.log" 2>/dev/null
}

# --------------------------------------------------------- where it spawns ---
#
# The **toplevel** half of §38, and it needs no input at all — which is the
# point of it being a separate mode. `CreateWindowExW` passes `CW_USEDEFAULT`
# for all four of x, y, width and height, so where a window lands is Windows'
# cascade and how large it is is whatever the client asked for; the two
# questions are "is it larger than the work area" and "is its title bar on the
# screen", and both are answered by one `GetWindowRect` against
# `SPI_GETWORKAREA`.
#
# Because there is no click, this mode runs against a **locked** guest, where
# `mouse_event` cannot reach the desktop (input goes to Winlogon's secure
# desktop) and a screen grab returns the lock screen. The window capture is
# `PrintWindow` on our own `HWND`, which reads DWM's redirection surface and is
# up to date whether or not anything is on top of it — §13's mechanism, and the
# reason a photograph of the window is available when a photograph of the
# screen is not.
do_spawn() {
  local exe="${1:-wc-compositor.exe}" tag="${2:-after}"
  echo
  echo "== $tag: $exe, where a window lands when nobody has placed it =="
  [ -n "$HOST" ] || { miss "WC_HOST unset"; return; }
  [ -n "$HOST_WESTON" ] || [ -n "$HOST_FIREFOX" ] || { miss "no client"; return; }
  # §51. A hypervisor is in this path, declared so that a claim about how long
  # anything took across it is available here -- and refused in the Wine
  # harnesses, which is what §27.6 needed and did not have.
  wc_topology_declare cross-vm \
    "the compositor is on the guest and the client on the VM host" || return

  local CLIENT_CMD="$HOST_WESTON/$CLIENT"
  if [ -n "$HOST_FIREFOX" ]; then
    CLIENT_CMD="env -u DISPLAY HOME=\$RUN/home MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland \
$HOST_FIREFOX --no-remote --profile \$RUN/profile about:blank"
  fi

  local sock="wc38$tag-$$"
  start_side "$sock" "$exe" "$CLIENT_CMD" || return
  if [ -n "$HOST_FIREFOX" ]; then sleep 45; else sleep 24; fi

  g "rm -f $DESKDIR/win-$tag.bmp" >/dev/null 2>&1
  int wcspawn "powershell.exe" \
    "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File $DESKDIR_WIN\\spawn.ps1 -TargetPid $CPID -Report spawn-$tag.txt -Shot $DESKDIR_WIN\\win-$tag.bmp" \
    >/dev/null 2>&1
  g "cat $DESKDIR/spawn-$tag.txt" >"$RUNDIR/spawn-$tag.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/spawn-$tag.txt"
  mkdir -p "$OUT"; cp "$RUNDIR/spawn-$tag.txt" "$OUT/spawn-$tag.txt" 2>/dev/null
  fetch "win-$tag.bmp" "stage38-window-$tag.png" "the client's window, frame and all"

  local top
  top=$(grep -E "^toplevel " "$RUNDIR/spawn-$tag.txt" | head -1)
  if [ -z "$top" ]; then
    bad "$tag: no toplevel appeared"
  elif echo "$top" | grep -q "inside-work-area=True"; then
    ok "$tag: the window is inside the work area, caption and all  ($top)"
  else
    bad "$tag: the window is not inside the work area  ($top)"
  fi

  stop_side "$sock" "$tag"
}

# ---------------------------------------------------------------- one run ---
do_menu() {
  local exe="${1:-wc-compositor.exe}" tag="${2:-after}"
  echo
  echo "== $tag: $exe, a menu in the bottom right corner of the real screen =="
  [ -n "$HOST" ] || { miss "WC_HOST unset"; return; }
  [ -n "$HOST_WESTON" ] || [ -n "$HOST_FIREFOX" ] || { miss "no client: set WC_HOST_WESTON or WC_HOST_FIREFOX"; return; }

  # Firefox's environment is `wc-desktop.sh`'s, and each part of it cost that
  # script a run: `env -u DISPLAY` because with an X server reachable
  # `MOZ_ENABLE_WAYLAND=1` is only a preference; a `HOME` and a profile of its
  # own so that an already-running Firefox does not take the request; and the
  # socket by absolute path, because libwayland resolves a bare
  # `WAYLAND_DISPLAY` against `XDG_RUNTIME_DIR`.
  local CLIENT_CMD="$HOST_WESTON/$CLIENT"
  if [ -n "$HOST_FIREFOX" ]; then
    CLIENT_CMD="env -u DISPLAY HOME=\$RUN/home MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland \
$HOST_FIREFOX --no-remote --profile \$RUN/profile about:blank"
  fi

  local sock="wc38$tag-$$"
  start_side "$sock" "$exe" "$CLIENT_CMD" || return

  # Firefox's first pixels arrive several seconds after the connection (§15.5),
  # and a window that is photographed before them is a white rectangle.
  if [ -n "$HOST_FIREFOX" ]; then sleep 45; else sleep 24; fi
  # The whole interaction — move, click, wait for the menu, photograph it — in
  # **one** scheduled task.
  #
  # `-WindowStyle Hidden`, and it is not tidiness: the task's own console is a
  # window on this desktop, it can sit over the point being clicked, and it is
  # in the photograph otherwise. The first `before` run has a `cmd.exe` window
  # across the middle of it.
  #
  # The capture is inside the same task for a sharper reason — see the polling
  # note in `menu.ps1`: a menu is a grab and a second task is long enough for
  # it to be gone.
  g "rm -f $DESKDIR/screen-$tag.bmp" >/dev/null 2>&1
  int wcmenu "powershell.exe" \
    "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File $DESKDIR_WIN\\menu.ps1 -TargetPid $CPID -Report menu-$tag.txt -Shot $DESKDIR_WIN\\screen-$tag.bmp" \
    >/dev/null 2>&1
  g "cat $DESKDIR/menu-$tag.txt" >"$RUNDIR/menu-$tag.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/menu-$tag.txt"
  mkdir -p "$OUT"; cp "$RUNDIR/menu-$tag.txt" "$OUT/menu-$tag.txt" 2>/dev/null

  # The old capture was deleted before the task ran. A `shot` that never
  # happened writes nothing, and `fetch` would then quietly bring back the
  # previous run's screen — which is exactly what happened once here: an
  # `after` picture that was really the `before` one, with a four-minute-old
  # clock in the corner as the only tell.
  fetch "screen-$tag.bmp" "stage38-screen-$tag.png" "the screen with the menu on it" screen

  # The verdict, from the numbers rather than from the picture — and it depends
  # on what the client declared, which is why the log line carries it.
  local popup
  popup=$(grep -E "^after popup " "$RUNDIR/menu-$tag.txt" | head -1)
  g "cat $DESKDIR/comp-$sock.log" >"$RUNDIR/comp-$tag.log" 2>&1
  # What the client declared. Read from the compositor's own log where it says
  # so — and **the binary from before this stage does not print it**, which is
  # why the client decides the expectation and the log only corroborates it.
  # Measured with `WAYLAND_DEBUG=1`: Firefox 63, weston-terminal nothing.
  local declared expect="none"
  declared=$(grep -oE "adjust ConstraintAdjustment\([^)]*\)" "$RUNDIR/comp-$tag.log" | head -1)
  [ -n "$HOST_FIREFOX" ] && expect="moveable"
  echo "$declared" | grep -qE "Slide|Flip|Resize" && expect="moveable"
  # No apostrophe in the fallback: a `'` inside `${x:-...}` is a quote even
  # inside double quotes, and it swallows the rest of the file.
  note "the client declared: ${declared:-not logged by this build; the client is $CLIENT, expected $expect}"
  if [ -z "$popup" ]; then
    bad "$tag: no popup window appeared — the right-click did not become a menu"
  elif [ "$expect" = moveable ]; then
    if echo "$popup" | grep -q "inside-work-area=True"; then
      ok "$tag: the menu asked to be moved and is inside the work area  ($popup)"
    else
      bad "$tag: the menu asked to be moved and hangs off the work area  ($popup)"
    fi
  else
    # STAGE 42. **This arm awarded an unconditional PASS**, so for any client
    # that is not Firefox — which is what `WC_HOST_WESTON` alone gives, i.e. the
    # default — stage 38's negative control could not fail. And it is the arm
    # that most needs to: a compositor that "helpfully" moves a menu the client
    # never asked to have moved is the *other* half of the bug this stage fixed,
    # and §38's whole argument is that `set_constraint_adjustment` is a
    # declaration the compositor may not exceed.
    #
    # The claim is checkable from the same line the other arm uses. A client
    # that declared nothing gets its menu exactly where the positioner asked,
    # and here that position is deliberately off the work area — the parent is
    # put in the bottom-right corner precisely so the menu would not fit. So
    # `inside-work-area=True` for a client that declared nothing means the
    # compositor moved it anyway, which is a failure and used to read as a pass.
    note "$tag: the client declared no adjustment, so the only right answer is"
    note "      wherever the positioner put it — this is the negative control"
    if echo "$popup" | grep -q "inside-work-area=False"; then
      ok "$tag: a menu that declared nothing was placed where it asked and not second-guessed ($popup)"
    elif echo "$popup" | grep -q "inside-work-area=True"; then
      bad "$tag: the menu was moved on-screen for a client that declared no adjustment -- \
the compositor exceeded the declaration ($popup)"
    else
      bad "$tag: the report says nothing about the work area, so the control proves nothing ($popup)"
    fi
  fi

  stop_side "$sock" "$tag"
}

case "$MODE" in
  menu) do_menu "${2:-wc-compositor.exe}" "${3:-after}" ;;
  spawn) do_spawn "${2:-wc-compositor.exe}" "${3:-after}" ;;
  both)
    do_menu wc-compositor-before.exe before
    do_menu wc-compositor.exe after
    ;;
  *) echo "usage: $0 {menu [EXE] [TAG]|spawn [EXE] [TAG]|both}"; exit 2 ;;
esac

echo
echo "  $pass passed, $fail failed, $skip skipped"
[ "$fail" = 0 ]
