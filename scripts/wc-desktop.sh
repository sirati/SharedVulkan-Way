#!/usr/bin/env bash
# Run the compositor on the guest's **interactive desktop** and photograph it.
#
#   scripts/wc-desktop.sh deploy    cross-compile and copy wc-desk / wc-compositor
#   scripts/wc-desktop.sh probe     station, windows, tick -- from both sessions
#   scripts/wc-desktop.sh window    a real client's window, captured (needs WC_HOST)
#   scripts/wc-desktop.sh fps       does the poll interval decide the frame rate?
#   scripts/wc-desktop.sh psgate    the PowerShell this script generates, no guest
#   scripts/wc-desktop.sh damage    block 12: Firefox scrolled on the desktop, --verify-damage
#                                   (deploys first; pictures land in docs/block12/)
#   scripts/wc-desktop.sh all       psgate, deploy, probe, window, fps
#
# Run from inside `nix develop`. Everything lands in docs/stage8/.
#
# WHY THIS SCRIPT EXISTS, AND WHAT IT DOES THAT wc-guest.sh DOES NOT
#
# `wc-guest.sh` runs the compositor over ssh. docs/waycompose-design.md §10.12 item 3 is the
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
. "$(dirname "${BASH_SOURCE[0]}")/site.sh"
set -uo pipefail

MODE="${1:-all}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
OUT="$ROOT/docs/stage8"

GUEST="${WC_GUEST:-$SITE_GUEST}"
# Exported as resolved: `wc_topology_declare cross-vm` (wc-topology.sh) checks
# WC_GUEST, and a default that lived only in GUEST made it refuse cross-vm
# unless the caller had happened to set the variable this line defaults.
export WC_GUEST="$GUEST"
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"
# STAGE 13: named, not hard-coded, and it is §16.8's rule arriving here one
# stage late. More than one experiment runs against this guest at a time, and a
# fixed directory means one deploy overwrites another run's binaries in the
# middle of it — which presents as the other run testing code nobody wrote. The
# default is what every previous stage used, so nothing that ran before moves.
DESKNAME="${WC_DESKNAME:-wcdesk}"
DESKDIR="$SITE_GUEST_MSYS_HOME/$DESKNAME"
DESKDIR_WIN="$SITE_GUEST_WIN_HOME\\$DESKNAME"
APPDATA_WIN="$SITE_GUEST_WIN_HOME\\AppData\\Local\\waycompose"
PORT="${WC_PORT_BASE:-57271}"

# The stage-4 half: the machine the VM runs on. It used to be described as "the
# machine that shares a filesystem with the guest", and stage 13 demoted that:
# the export still carries the frame dump and the logs, but **not the pixels**.
# See `locate_arena` below and docs/waycompose-design.md §18.2.
HOST="${WC_HOST:-}"
HOST_DIR="${WC_HOST_DIR:-/tmp/wc-stage8}"
SHARE="${WC_HOST_SHARE:-/srv/winshare/wc}"
SHARE_WIN="${WC_GUEST_SHARE:-Z:\\wc}"
HOST_GUEST="${WC_HOST_GUEST:-$SITE_GUEST_USER@$SITE_GUEST_NAT_IP}"
HOST_KEY="${WC_HOST_KEY:-\$HOME/.ssh/wc_guest}"
HOST_WESTON="${WC_HOST_WESTON:-}"

# STAGE 13. The arena is guest RAM, always, and this is the block of settings
# that makes that possible. It is `wc-guest.sh`'s block, with the same names and
# the same defaults, because the two harnesses had drifted into testing two
# different transports against the same guest and only one of them was the one
# anything ships on.
#
# WHY THIS REPLACED `--arena 0:N:Z:\wc\arena` RATHER THAN JOINING IT.
#
# §10.3 measured the virtio-fs export incoherent for two live mappings — 65280
# of 65536 bytes wrong, in both directions — and stage 7 replaced it with guest
# RAM for `wc-guest.sh`. This script kept the file, and because it is the only
# script that puts a window in front of a human, **the incoherent substrate was
# the only one anyone ever looked at**. §18.1 is the photograph of what that
# costs. There is deliberately no fallback: if the region cannot be located this
# run fails and says so, because a run that quietly degrades to a file is how
# this reached a user's screen with every check green.
QMP="${WC_QMP:-/run/windows-vm/qmp.sock}"            # QEMU's monitor, on WC_HOST
VM_HINT="${WC_VM_HINT:-windows-vm}"                  # how to recognise its process
ROOTC="${WC_ROOTC:-\$HOME/rootc}"                    # how WC_HOST becomes root
ARENA_PAGES="${WC_ARENA_PAGES:-16384}"               # 64 MiB, in 4096-byte pages
ARENA_OUTBOUND="${WC_ARENA_OUTBOUND:-1048576}"       # 1 MiB, the compositor's window
ARENA_BYTES=$((ARENA_PAGES * 4096))

# STAGE 13. Firefox, as an absolute path on WC_HOST, and the `wl_output` size to
# advertise — `wc-guest.sh`'s block 11 settings, arriving here because
# `docs/goal-firefox.md`'s client is the one whose defects this stage is about.
# With it set, `window` runs Firefox instead of `$WC_CLIENT`.
HOST_FIREFOX="${WC_HOST_FIREFOX:-}"
OUTPUT_SIZE="${WC_OUTPUT:-1600x1000}"
# Which unmodified client to put in the window. `weston-terminal` is the
# `docs/goal-firefox.md` stand-in and is what the title and taskbar claims use;
# `weston-presentation-shm` is the one the modal-resize check wants, because it
# commits a frame continuously, so a compositor that stops repainting stops
# producing evidence within a frame rather than within a minute. `weston-flower`
# looks like the right choice and is not one: it draws its flower once.
CLIENT="${WC_CLIENT:-weston-terminal}"
CLIENT_TITLE="${WC_CLIENT_TITLE:-Wayland Terminal}"
# STAGE 33. Arguments for that client, and text to type into it once it is up.
#
# `WC_CLIENT_ARGS` exists for `weston-eventdemo`, whose every log is off by
# default: run without `--log-key` it is silent and looks exactly like a client
# that received nothing, which is the failure this block is here to tell apart.
#
# `WC_TYPE` is the whole of §33's second bug. With it set, `probe-input.exe`
# runs **on the interactive desktop** and injects the string with `SendInput`,
# so a keystroke starts in the real desktop's input queue — the same place the
# user's does — and the window is photographed again afterwards.
CLIENT_ARGS="${WC_CLIENT_ARGS:-}"
TYPE_TEXT="${WC_TYPE:-}"
# STAGE 13. With `WC_HOST_FIREFOX` set the client is Firefox and the caption
# assertion has to move with it: Firefox's title is the page's, not a constant.
# Set here rather than at the call site so that a run cannot end up starting
# Firefox and then asserting on `weston-terminal`'s caption, which is a green
# run of the wrong test.
if [ -n "$HOST_FIREFOX" ]; then
  CLIENT="firefox"
  CLIENT_TITLE="${WC_CLIENT_TITLE:-Mozilla Firefox}"
fi
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

# STAGE 42. `fresh` and `deployed_is`: is the binary newer than the tree, and is
# the copy on the guest the one this tree built. Sourced after `ok`/`bad`/`note`
# and `g`/`h`, which it uses.
. "$(dirname "$0")/wc-binaries.sh"
. "$(dirname "$0")/wc-topology.sh"
# §84: `damage` declares guest-ram and asserts it, and shares its verdict with
# `wc-guest.sh damage`.
. "$(dirname "$0")/wc-substrate.sh"
. "$(dirname "$0")/wc-damage-verdict.sh"
# `hostside_start`'s two knobs, empty for every mode but `damage`.
HOSTSIDE_URL=""; HOSTSIDE_PROXY_EXTRA=""

# ------------------------------------------------------------ the arena ------
# STAGE 13. Everything the guest-RAM transport needs, in one place, because
# `do_window` and `do_fps` both need it and a second spelling of it is a second
# thing to get wrong.
#
# The flags the compositor is started with. There is no `--shared-dir` here and
# that is the point rather than an omission: with a guest-RAM arena the keymap
# is cut out of the compositor's own outbound window (§14), so no `h` argument
# is ever published on a filesystem and the export is not on the pixel path at
# all. §12's `wc-guest.sh` has passed this way for two stages.
# §84: spelled by `wc_arena_comp_args` (wc-substrate.sh) now, with the frame
# list `locate_arena` translates; this script's own copy of the flags is gone.

# Find the region the compositor pinned, and write a plan the proxy can open.
#   locate_arena <compositor-log-name-on-the-guest> <plan-path-on-WC_HOST>
#
# Ordering is the whole of it, and it is §12's: the guest allocates and stamps,
# and only then can the host find the pages. The compositor prints its run id
# *after* the stamps are written, so waiting for that line is waiting for the
# region to be findable — a sweep started earlier returns zero pages, which
# reads exactly like the arena not being shared at all.
#
# Polled over ssh rather than out of `$SHARE`, which is how `wc-guest.sh` does
# it. That is deliberate: this is the one control path that decides whether the
# run is on guest RAM, and reading it through the export would make the check
# depend on the thing the check exists to stop using.
#
# Root, and only for this: QEMU is a system service on this host, so its QMP
# socket and its `/proc/<pid>/fd` are root-only (§12.9).
locate_arena() {
  local log="$1" plan="$2"
  # §84: `wc_locate_arena`, which wc-icon.sh and wc-taskbar.sh already use, in
  # place of this script's own copy. That copy knew only the content sweep, and
  # the first rerun of block 12 here died on it: "3 page index(es) were claimed
  # twice", the sweep's documented coin flip on a busy guest (wc-substrate.sh).
  # The compositor now publishes its frame list beside its log, the locate
  # translates the guest's own PFNs and reads no stamp, and a run that falls
  # back to the sweep says so. It also sets WC_SUBSTRATE_LOCATED, which is what
  # `wc_assert_substrate` needs and what this copy never set.
  wc_locate_arena "$DESKDIR/$log" "$DESKDIR/${log%.log}.frames" "$plan" "$RUNDIR"
}

# The export must not be able to serve pixels behind this script's back. A
# leftover `arena` file from a run of this harness before stage 13 is openable,
# the right size, and completely silent -- so it is removed rather than trusted
# not to be used.
purge_arena_file() {
  h "rm -f $SHARE/arena; rm -rf $SHARE/keymaps" >/dev/null 2>&1
  true
}

# The scheduled task's name, which was the last shared identifier on this guest.
#
# STAGE 37.8 item 5, fixed here because `wc-desktop.sh` is where the names are.
# `int`/`int_bg` are called with constants — `wccomp`, `wcshot`, `wcwin` — and
# `runint.ps1` registers whatever it is handed, so **every worktree registered
# `wccomp`** and `Register-ScheduledTask -Force` meant the last run to start
# silently redefined the others'. It is the same family as `wg-s8` and
# `WaycomposeToplevel`, both closed by §33, and it was the member still open.
#
# Two components, and both are needed. `$DESKNAME` separates worktrees, which is
# §16.8's knob and is what makes the reaper below able to recognise its own; the
# pid separates two runs of the *same* worktree, which is the case that actually
# bites — `Unregister-ScheduledTask` on a name another run is about to
# `Start-ScheduledTask` loses that run's step with no error on either side.
#
# The scheduler accepts a backslash in a name as a folder separator, so this
# deliberately uses none: one flat name that `Get-ScheduledTask -TaskName` can
# glob and that leaves no empty task folders behind when it is removed.
TASK_PREFIX="wc-$DESKNAME-$$-"

# Run a command line on the interactive desktop and wait for it.
#   int <name> <exe> <arguments-as-one-string>
int() {
  local name="$1" exe="$2" argstr="$3"
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\runint.ps1' -Exe '$exe' -Arguments '$argstr' \
       -Name '$TASK_PREFIX$name' -DeskDir '$DESKDIR_WIN' -Wait -TimeoutSec 120" 2>&1
}

# The same, without waiting: for the compositor, which has to outlive the call.
#
# **This one never unregisters**, which is how the leftover that found §37.8
# item 5 came to exist: a `Ready` task pointing at a deploy directory whose run
# ended long ago. It cannot unregister — the task has to outlive the ssh call —
# so the cleanup is `reap_tasks` at both ends of the run instead.
int_bg() {
  local name="$1" exe="$2" argstr="$3"
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\runint.ps1' -Exe '$exe' -Arguments '$argstr' \
       -Name '$TASK_PREFIX$name' -DeskDir '$DESKDIR_WIN'" 2>&1
}

# A command on the interactive desktop **with no console window**, its output
# appended to a file on the guest.
#   int_quiet <name> <command-line> <out-file>
#
# §84. `int … cmd.exe /c …` opens a console on the real desktop for as long as
# the command runs, and on this guest that window lands over the middle of
# Firefox: the first block-12 screen capture photographed its own console, and
# a wheel notch sent while it was up goes to the console, not to the page,
# because the probe parks the cursor at the window's centre. A PowerShell
# started `-WindowStyle Hidden` hides the console before the command runs, and
# the command inherits it. `Out-File -Encoding ascii`, because PowerShell 5's
# own redirection writes UTF-16.
int_quiet() {
  local name="$1" cmdline="$2" outfile="$3"
  int "$name" "powershell.exe" \
    "-NoProfile -WindowStyle Hidden -Command $cmdline 2>&1 | Out-File -Append -Encoding ascii $outfile"
}

# Remove the scheduled tasks this *worktree* left behind, and only those.
#
# The rule is §37.8 item 5's, which is the one the agent that found the leftover
# applied by hand and the one this script did not apply to itself:
#
#   * `Actions.Execute` must be under this run's `DESKDIR`. A name is not an
#     identity (§16.8 rule 1) and a task's name can have been redefined by
#     somebody else between registering it and reaping it; what the task will
#     actually *run* cannot.
#   * `State` must not be `Running`. Unregistering a running task is how a
#     concurrent run loses a step, and it is the failure this whole function
#     exists to avoid causing while cleaning up after the failure it avoids
#     suffering.
#
# Matched on `wc-$DESKNAME-` rather than on this run's full prefix, so that a
# previous *interrupted* run of this worktree is cleaned up too — that is the
# state the guest was actually found in. A concurrent run of the same worktree
# is still protected, because its tasks are either `Running` or belong to a
# `DESKDIR` this one shares and has already finished with.
reap_tasks() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\reaptasks.ps1' -Prefix 'wc-$DESKNAME-' -DeskDir '$DESKDIR_WIN'" 2>&1
}

write_reaptasks() { cat >"$1" <<'PS3'
# Generated by scripts/wc-desktop.sh; do not edit on the guest.
# Remove this worktree's leftover scheduled tasks. See `reap_tasks` for the rule.
#
# **Not `-Home`**, which is a PowerShell automatic variable: a parameter of that
# name shadows `$HOME` for the whole script and is the kind of thing that works
# until something else in the same file wants a home directory.
param([Parameter(Mandatory=$true)][string]$Prefix,
      [Parameter(Mandatory=$true)][string]$DeskDir)

# Does this task run something of ours? `Execute` alone is not enough: half of
# `wc-desktop.sh`'s tasks are `cmd.exe /c <DESKDIR>\wc-desk.exe …`, because a
# task's stdout goes nowhere and the redirect has to be a shell's. For those the
# identity is in the arguments. Either place, and the answer is still "what this
# task will actually run lives in our directory" rather than "it has our name".
function Ours($task, $dir) {
  # `Actions` is a list and a task here has exactly one, but indexing blindly
  # throws on a task somebody else defined under a name we matched.
  $a = $task.Actions | Select-Object -First 1
  if (-not $a) { return $false }
  $hay = "" + $a.Execute + " " + $a.Arguments
  # The trailing separator matters: without it a worktree deployed to
  # `…\wcterm` also matches one deployed to `…\wcterm2`. Every path this script
  # puts in a task is `<DESKDIR>\something`, so requiring it costs nothing.
  return $hay.ToLower().Contains($dir.ToLower().TrimEnd('\') + '\')
}

Get-ScheduledTask -TaskName "$Prefix*" -ErrorAction SilentlyContinue | ForEach-Object {
  $t = $_
  if (-not (Ours $t $DeskDir)) {
    Write-Output ("skip " + $t.TaskName + ": does not run anything under " + $DeskDir)
    return
  }
  if ($t.State -eq "Running") {
    Write-Output ("skip " + $t.TaskName + ": still running")
    return
  }
  # `-TaskPath` as well as `-TaskName`: `Get-ScheduledTask` searches every
  # folder and `Unregister-ScheduledTask` defaults to the root, so a name
  # matched in a folder would not be found again by name alone. Everything this
  # script registers is in the root; passing the path back is what makes that an
  # observation rather than an assumption.
  Unregister-ScheduledTask -TaskName $t.TaskName -TaskPath $t.TaskPath `
                           -Confirm:$false -ErrorAction SilentlyContinue
  Write-Output ("reaped " + $t.TaskPath + $t.TaskName)
}
PS3
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
# **The heredoc is quoted and must stay quoted.** Everything below is
# PowerShell and is full of `$Exe`, `$env:` and `$_`; letting bash expand it
# would replace half the script with empty strings. `__DESKDIR__` is therefore
# substituted afterwards rather than interpolated here — STAGE 13, when the
# directory stopped being one fixed name (§16.8).
write_runint() { cat >"$1" <<'PS1'
# Run a command on the interactive desktop of the logged-on session.
# Generated by scripts/wc-desktop.sh; do not edit on the guest.
param(
  [Parameter(Mandatory=$true)][string]$Exe,
  [string]$Arguments = "",
  [string]$Name = "wcdesk",
  [string]$WorkDir = "__DESKDIR__",
  # STAGE 37.8 item 5. This run's deploy directory, so that the unregister at
  # the bottom can check *what the task runs* rather than trusting the name it
  # was given. See `reap_tasks` in wc-desktop.sh for the rule and why a name is
  # not an identity on this guest.
  [string]$DeskDir = "__DESKDIR__",
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
  # The two checks, and they are not belt-and-braces even though *this* script
  # registered the name a moment ago. Between then and now another run can have
  # redefined it with `-Force`, and unregistering a task another run is about to
  # start loses that run's step silently at both ends — which is the failure
  # §37.8 item 5 says the script must not cause while cleaning up after the one
  # it must not suffer. The name being unique per run makes this unlikely; the
  # checks make it not matter.
  $t = Get-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue
  if ($t) {
    $a = $t.Actions | Select-Object -First 1
    $hay = ("" + $a.Execute + " " + $a.Arguments).ToLower()
    $needle = $DeskDir.ToLower().TrimEnd('\') + '\'
    if ($t.State -eq "Running") {
      Write-Output ("kept " + $Name + ": still running")
    } elseif (-not $hay.Contains($needle)) {
      Write-Output ("kept " + $Name + ": runs nothing under " + $DeskDir)
    } else {
      Unregister-ScheduledTask -TaskName $Name -Confirm:$false
    }
  }
}
PS1
  # The one interpolation, done after the fact for the reason above. The
  # backslashes are doubled first: a Windows path is full of them and sed
  # reads `\U`, `\g` and friends in a replacement as instructions.
  sed -i "s|__DESKDIR__|${DESKDIR_WIN//\\/\\\\}|" "$1"
}

# ---------------------------------------------------------------- deploy -----
do_deploy() {
  echo "== deploy =="
  # STAGE 42. **Existence is not freshness.** This checked that each binary was
  # a file, which a binary built before the change under test also is: §35.4
  # measured a harness reporting the *previous* tree's behaviour after `cargo
  # test` declined to relink, and §35.6 item 6 named this harness among the
  # seventeen with no guard. `probe-relay.exe` stays exempt for the reason it
  # always was — it is a C probe built below rather than by cargo, so no cargo
  # target exists for `fresh` to compare against.
  local how_win="cargo build --workspace --target x86_64-pc-windows-gnu"
  fresh "$WIN_BIN/wc-desk.exe"       "$how_win" || exit 1
  fresh "$WIN_BIN/wc-compositor.exe" "$how_win" || exit 1

  local stage; stage=$(mktemp -d /tmp/wc-desk-stage-XXXXXX)
  cp "$WIN_BIN/wc-desk.exe" "$WIN_BIN/wc-compositor.exe" "$stage/"
  if [ -n "${WC_XKB_MINGW:-}" ]; then
    x86_64-w64-mingw32-gcc -O1 -o "$stage/probe-relay.exe" "$ROOT/scripts/probe-relay.c" -lws2_32 \
      2>"$stage/cc.log" || { echo "  FAILED to build probe-relay.exe"; cat "$stage/cc.log"; }
  fi
  # STAGE 33. `probe-input.exe`, which stage 6 built only for Wine. It is
  # `-mwindows` — a GUI subsystem binary — because a scheduled task that starts
  # a console program flashes a console window onto the desktop being
  # photographed, and its own `--log` is how its output is recovered.
  x86_64-w64-mingw32-gcc -O1 -mwindows -o "$stage/probe-input.exe" "$ROOT/scripts/probe-input.c" \
    -luser32 2>"$stage/cci.log" \
    || { echo "  FAILED to build probe-input.exe"; cat "$stage/cci.log"; }
  # §84: the wheel for `damage`, `-mwindows` for the same reason.
  x86_64-w64-mingw32-gcc -O1 -mwindows -o "$stage/probe-scroll.exe" "$ROOT/scripts/probe-scroll.c" \
    2>"$stage/ccs.log" \
    || { echo "  FAILED to build probe-scroll.exe"; cat "$stage/ccs.log"; }
  write_runint "$stage/runint.ps1"
  write_killours "$stage/killours.ps1"
  write_reaptasks "$stage/reaptasks.ps1"

  # A running .exe holds its own file open and scp over it fails as
  # `dest open ...: Failure` -- wc-guest.sh's lesson, and a task-launched
  # compositor is *more* likely to still be there because it did not die with
  # the session that started it. Only this script's own copies are killed:
  # `wcdesk\wc-compositor.exe` is a different file from `wctest\`'s, which
  # another worktree may legitimately be running.
  kill_ours "$DESKNAME" >/dev/null 2>&1
  g "taskkill //F //IM wc-desk.exe >/dev/null 2>&1; true" >/dev/null 2>&1
  # §84: and anything else still running out of this directory. A run that
  # stopped at the locate left its `probe-relay.exe` alive for its whole
  # 330 s timeout -- killing the local ssh did not end it -- and the next
  # deploy died on `scp: dest open "wcdesk/probe-relay.exe": Failure`.
  # Matched by executable path with a trailing separator, wc-guest.sh's
  # `kill_in_dir` rule, so another worktree's directory is never touched.
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -Command \
     \"\\\$p = '${DESKDIR_WIN}\\\\'; Get-CimInstance Win32_Process | \
      Where-Object { \\\$_.ExecutablePath -and \\\$_.ExecutablePath.StartsWith(\\\$p) } | \
      ForEach-Object { Stop-Process -Id \\\$_.ProcessId -Force -ErrorAction SilentlyContinue }\"" \
    >/dev/null 2>&1
  g "mkdir -p $DESKDIR" || exit 1
  scp "${SSHOPT[@]}" "$stage"/* "$GUEST:$DESKNAME/" >/dev/null || exit 1
  echo "  copied $(ls "$stage" | grep -c .) files to $GUEST:$DESKDIR"

  # STAGE 42. **And the copy is checked.** The comment above says a running
  # `.exe` makes `scp` fail — it does when Windows holds the file open, and the
  # `kill_ours` above is why that is usually avoided. What neither covers is the
  # case where the transfer *appears* to succeed and the old bytes remain, and
  # `scp` returning 0 cannot tell the two apart. The symptom is a behaviour
  # change; on the GPU path it is a transport fault, which `finish_attach`
  # refuses with "This is a version skew between two builds, not a transport
  # fault" — `WIRE_VERSION` went 1 -> 2 while this was written.
  deployed_all "$stage" "$DESKDIR" g "re-run $0 deploy with nothing of ours running on the guest" \
    "wc-desk.exe" "wc-compositor.exe" || exit 1
  rm -rf "$stage"
  mkdir -p "$OUT"
  # After the copy, because `reaptasks.ps1` has to be on the guest to run — and
  # this is the call that clears whatever a previous *interrupted* run of this
  # worktree left `Ready`, which is the state §37.8 item 5 found the guest in.
  reap_tasks | sed 's/^/  /'
}

# ----------------------------------------------------------------- probe -----
# Everything in this block is run twice, once per session, because the whole
# point is the difference between them.
do_probe() {
  echo
  echo "== 1. the two window stations =="
  g "cd $DESKNAME && ./wc-desk.exe station" >"$RUNDIR/station-ssh.txt" 2>&1
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
  g "cd $DESKNAME && ./wc-desk.exe --unaware station" >"$RUNDIR/unaware-ssh.txt" 2>&1
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
  fetch taskbar.bmp stage8-taskbar-empty.png "the taskbar with no compositor window" tray
}

# Pull a BMP off the guest and convert it. `convert` is in the dev shell for
# exactly this (see flake.nix); a PNG is what a repository should hold.
# STAGE 42. **Seven photographs, and the gate on all of them was that
# ImageMagick could open the file.** The geometry went into the success string
# and was compared with nothing, so a 1x1 or an all-black capture read as a
# PASS. §41.8 fixed this helper in `wc-taskbar.sh`; the audit after it found the
# original still here, in `wc-icon.sh`, `wc-popup.sh` and `wc-titlebar.sh`.
#
# `kind` is required and not defaulted, which is §41.8's other half: `tray2.bmp`
# is a `--class Shell_TrayWnd` strip at **1280x48** and `screen.bmp` is a whole
# desktop, so one bound across all seven callers would be the
# impossible-threshold defect rather than a fix for the vacuous one.
fetch() {
  local remote="$1" local_png="$2" caption="$3" kind="${4:-window}"
  scp "${SSHOPT[@]}" "$GUEST:$DESKNAME/$remote" "$RUNDIR/$remote" >/dev/null 2>&1
  if [ -s "$RUNDIR/$remote" ]; then
    mkdir -p "$OUT"
    if magick "$RUNDIR/$remote" "$OUT/$local_png" 2>/dev/null \
       || convert "$RUNDIR/$remote" "$OUT/$local_png" 2>/dev/null; then
      picture_is "$OUT/$local_png" "$kind" "$caption -> ${OUT#"$ROOT"/}/$local_png"
    else
      bad "could not convert $remote"
    fi
  else
    bad "$caption: nothing came back"
  fi
}

# ------------------------------------------------- the session, brought up --
# The compositor on the interactive desktop, its relay, and its arena located:
# everything `window` and `damage` both need before a client can connect. Taken
# out of `do_window` rather than copied into `do_damage`, because the two modes
# must bring the session up the same way and a second spelling of these 130
# lines would be a second thing to keep right.
#
# Sets the globals `sock`, `CPID`, `relay` and `plan`, which the callers use
# afterwards. Returns 1 when the session did not come up, having said why.
desk_up() {
  sock="wg-s8-$$"
  # Stale compositors go **first**, before the frame directory is emptied. An
  # interrupted run leaves one alive for up to its `--timeout`, still dumping
  # into the same directory and still holding the same log file, and a wipe that
  # happens before the kill leaves the new run counting the old run's frames and
  # reading the old run's log. That was diagnosed twice as "the capture does not
  # match the dump" before it was diagnosed as this.
  # STAGE 33: `$DESKNAME`, not `wg-s8`. Every worktree's copy of this script
  # names its socket `wg-s8-$$`, so matching on that substring reaps *other
  # runs'* compositors — the exact thing `comp_pid` and `kill_ours`'s own
  # comments say must not happen, in the one call that was still doing it. The
  # compositor's command line begins with `C:\Users\<guest-user>\<DESKNAME>\`, which is
  # unique per worktree because §16.8 made it so.
  kill_ours "$DESKNAME" >/dev/null 2>&1
  # And the same reasoning one line down: `wg-s8-*` in the shared
  # `AppData\Local\waycompose` is *every* worktree's rendezvous file, including
  # the ones a live compositor is still listening on. A per-pid name is already
  # unique, so there is nothing here this run needs to clear.
  g "rm -f $APPDATA_WIN/$sock 2>/dev/null; true" >/dev/null 2>&1
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
  # STAGE 13: `wc-arena-host` as well, because the arena is now a fact about the
  # hypervisor rather than about the filesystem and this is the program that
  # turns the compositor's run id into pages the proxy can open.
  # `wc-arena-host` is **not a workspace member** — it links shared_vulkan's
  # `sv-hostd` and has its own lockfile, which is why `Cargo.toml` excludes it
  # (see the comment there). So it is built by its own manifest, exactly as
  # `wc-guest.sh` does, and a run that forgot to would fail at the locate with
  # a missing file rather than at the copy.
  # STAGE 33: the workspace's `target/`, with the private one as the fallback.
  # `wc-arena-host` was excluded from the workspace when this line was written
  # and stage 14 made it a member again (see the paragraph in `Cargo.toml`), so
  # its artefact moved and this path stopped existing — which failed the run at
  # the `scp` with `stat local ...: No such file or directory`, a message that
  # reads like the *proxy* being held open rather than like a build layout that
  # changed underneath.
  local locator="$LINUX_BIN/wc-arena-host"
  [ -x "$locator" ] || locator="$ROOT/crates/wc-arena-host/target/debug/wc-arena-host"
  if ! cargo build --manifest-path "$ROOT/crates/wc-arena-host/Cargo.toml" >"$RUNDIR/loc.log" 2>&1; then
    bad "could not build wc-arena-host"; sed 's/^/        /' "$RUNDIR/loc.log" | tail -10; return 1
  fi
  scp "${SSHOPT[@]}" "$LINUX_BIN/wc-proxy" "$locator" "$HOST:$HOST_DIR/" >/dev/null || {
    bad "could not copy wc-proxy/wc-arena-host to $HOST -- something is still holding one"; return 1; }
  h "chmod +x $HOST_DIR/wc-proxy $HOST_DIR/wc-arena-host" >/dev/null
  purge_arena_file

  # STAGE 6, and this block would not get a window without it. The compositor
  # advertises a `wl_seat` now, so a client that binds one is sent a
  # `wl_keyboard.keymap` — an `h` in the server-to-client direction. Published
  # as a `Local\wc-…` section it is unopenable by a Linux process and `wc-proxy`
  # ends the connection; `weston-terminal` binds a seat and
  # `weston-presentation-shm` does not, so exactly one of this script's two
  # clients would start failing and the other would not. That is the same shape
  # stage 7 hit in `wc-guest.sh` when stage 6 landed under it (§12.11), arriving
  # here one merge later. §11.2 is the mechanism, and STAGE 13 is the stage that
  # deletes the block: with a guest-RAM arena the keymap comes out of the
  # compositor's own outbound window, so there is nothing to publish on a
  # filesystem and nothing for the far side to open. If this run still gets a
  # window with no `--shared-dir`, the last reason this harness needed a shared
  # filesystem for *pixels* is gone -- the export keeps only the frame dump.

  # The compositor, on the desktop, detached: a scheduled task is not a child of
  # this ssh session and does not die with it.
  # `--dump-frames` is not decoration here: a PPM is written from `present`, on
  # the UI thread, so counting files in that directory is counting the times the
  # UI thread woke up and repainted. That is the whole of the modal-loop check
  # below, and it is why the frames go to the *host's* filesystem where they can
  # be counted while the guest is busy.
  #
  # STAGE 13. `--timeout` grew from 210 to 330, and it is the locate that spends
  # it: a 13 GiB sweep of the guest's memory is 2.4 s (§12.5) and the round trip
  # through `rootc` and `systemd-run` around it is not. A compositor that timed
  # out mid-run presents as a client that connected and then found the socket
  # closed, which is a long way from the clock.
  local out_extra=""
  [ -n "$HOST_FIREFOX" ] && out_extra=" --output $OUTPUT_SIZE"
  int_bg wccomp "$DESKDIR_WIN\\wc-compositor.exe" \
    "--socket $sock --timeout 330 --log $DESKDIR_WIN\\comp-$sock.log $(wc_arena_comp_args "$DESKDIR_WIN\\comp-$sock.frames") ${WC_COMP_EXTRA:-}--dump-frames $SHARE_WIN\\frames$out_extra" \
    >/dev/null 2>&1
  sleep 6
  # Everything below addresses windows by pid rather than by class. See
  # `comp_pid`: another worktree's compositor registers the same class.
  # Polled rather than slept for: a scheduled task's process appears when the
  # scheduler gets round to it, which on a loaded guest is several seconds after
  # `Start-ScheduledTask` returns.
  CPID=""; local t
  for t in 1 2 3 4 5 6; do
    CPID=$(comp_pid "$sock")
    [ -n "$CPID" ] && break
    sleep 4
  done
  if [ -z "$CPID" ]; then
    bad "the compositor did not start on the interactive desktop"
    return 1
  fi
  note "our compositor is pid $CPID (socket $sock)"

  # **The relay must outlive the compositor, not the setup.** It was `--timeout
  # 120`, and it is started *here* — before the arena locate, which sweeps 13
  # GiB of guest RAM through `rootc` and QMP and on a contended guest takes
  # minutes rather than the 2.4 s §12.5 measured on an idle one. So the relay
  # would exit part-way through a run that had done nothing wrong, and what the
  # harness reported was the *client*: "Failed to process Wayland connection:
  # Broken pipe", with the proxy saying "the compositor closed the connection"
  # and "2 client msgs, 0 server msgs". The compositor had closed nothing; its
  # log ends at "listening" and never mentions a client. Measured on this guest
  # tonight, twice.
  #
  # 330 is the compositor's own `--timeout`, which is the honest coupling: the
  # relay is the compositor's socket seen from the other side of the
  # hypervisor, and there is no run in which one should outlive the other. The
  # sibling call in `do_fps` already derives its timeout instead of naming a
  # constant; this one was the last magic number.
  ssh "${SSHOPT[@]}" "$GUEST" \
    "export MSYS_NO_PATHCONV=1; cd $DESKNAME && ./probe-relay.exe $PORT '$APPDATA_WIN\\$sock' --timeout 330" \
    >"$RUNDIR/relay.log" 2>&1 &
  relay=$!
  sleep 3

  # STAGE 13. The locate, which has to happen between the compositor starting
  # and the proxy running: the guest allocates and stamps, the host then finds
  # the pages. This is the step that did not exist while the arena was a file,
  # and it is the whole difference between the two harnesses.
  plan="$HOST_DIR/$sock.plan"
  if ! locate_arena "comp-$sock.log" "$plan"; then
    kill $relay 2>/dev/null
    kill_ours "$sock" >/dev/null 2>&1
    return 1
  fi
}

# The host side, as root in a transient unit: `wc-proxy` and the client. Uses
# `sock` and `plan` from `desk_up`, and sets `drv`, the pid of the ssh that
# runs it. Two knobs, both empty for `window`:
#   HOSTSIDE_URL          the page Firefox opens (default about:blank)
#   HOSTSIDE_PROXY_EXTRA  extra wc-proxy flags (`damage` passes --verify-damage)
hostside_start() {
  # The client and the proxy both live on WC_HOST, which is the machine the VM
  # runs on -- and, not by coincidence, the machine the client runs on, because
  # the client reaches the proxy over an AF_UNIX socket. See §18.5.
  #
  # STAGE 13: **as root, in a transient unit.** The proxy has to open
  # `/proc/<qemu-pid>/fd/<n>` to reach the guest's pages, and QEMU is a system
  # service here (§12.9), so the whole host side moves under `systemd-run
  # --collect` rather than running as the invoking user. A root call that blocks
  # for two minutes frequently comes back with no output at all; the unit's own
  # files survive it, which is why the script signals completion with a file
  # rather than with an exit code.
  #
  # The `ssh -L` forward stays with the *user*, outside the unit: root has no
  # `$HOME/.ssh/wc_guest`, and root can open a user-owned socket node anyway.
  local ffenv="" ffcmd="$HOST_WESTON/$CLIENT $CLIENT_ARGS"
  if [ -n "$HOST_FIREFOX" ]; then
    # `wc-guest.sh`'s block-11 environment, arriving here for the same reasons,
    # each of which cost a run there: `env -u DISPLAY` because with an X server
    # reachable `MOZ_ENABLE_WAYLAND=1` is a *preference* and Firefox will
    # quietly use X11; `HOME` and `XDG_RUNTIME_DIR` of its own because this unit
    # is root and Firefox refuses to run as root in another user's session; a
    # fresh `--profile` and `--no-remote` because an already-running Firefox
    # would take the request and open a tab in *it*, which looks exactly like a
    # client that connected and drew nothing.
    # `$RUN/xdg` rather than `$RUN`, and the socket by **absolute path**: this
    # is `wc-guest.sh`'s block-11 lesson and it cost this stage a run.
    # Firefox checks the ownership of `$XDG_RUNTIME_DIR`, so it needs one of
    # its own; but the proxy's socket is in `$RUN`, not in `$RUN/xdg`, and a
    # bare `WAYLAND_DISPLAY=wl-proxy` is resolved against the runtime
    # directory. The failure is `Failed to connect to Wayland display
    # '…/xdg/wl-proxy': No such file or directory`, which reads as a proxy that
    # never started. libwayland reads a `WAYLAND_DISPLAY` beginning with `/` as
    # a path, and that is what separates the two.
    ffenv="env -u DISPLAY HOME=\$RUN/home XDG_RUNTIME_DIR=\$RUN/xdg WAYLAND_DISPLAY=\$RUN/wl-proxy MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland"
    ffcmd="$HOST_FIREFOX --no-remote --profile \$RUN/profile ${HOSTSIDE_URL:-about:blank}"
  fi
  h "cd $HOST_DIR && cat > s8.sh <<'EOS'
#!/usr/bin/env bash
set -uo pipefail
RUN=\$(mktemp -d /tmp/wc-s8-XXXXXX); chmod 755 \"\$RUN\"
mkdir -p \"\$RUN/home\" \"\$RUN/profile\" \"\$RUN/xdg\"
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
chown -R 0:0 \"\$RUN/home\" \"\$RUN/profile\" \"\$RUN/xdg\" 2>/dev/null
chmod 700 \"\$RUN/xdg\" 2>/dev/null
$HOST_DIR/wc-proxy --upstream \"\$RUN/up.sock\" --socket wl-proxy \
   --runtime-dir \"\$RUN\" --transport arena --arena-plan $plan --arena-id 0 \
   --copy damage $HOSTSIDE_PROXY_EXTRA --accept-timeout 25 --timeout 150 >\"\$RUN/proxy.log\" 2>&1 &
P=\$!
sleep 2
# The bare WAYLAND_DISPLAY is the weston case; ffenv overrides it with an
# absolute path for Firefox, and a later assignment wins. No backticks in this
# comment: it is inside a double-quoted string and they would be run.
WAYLAND_DISPLAY=wl-proxy $ffenv timeout 130 $ffcmd >\"\$RUN/cli.log\" 2>&1
sleep 2
kill \$P 2>/dev/null; wait 2>/dev/null
chmod -R a+rwX \"\$RUN\" 2>/dev/null
touch \"\$RUN/done\"
INNER
  sed -i \"s#__RUN__#\$RUN#\" \"\$RUN/root.sh\"
  chmod 755 \"\$RUN/root.sh\"
  UNIT=wc-s13-\$\$
  echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
systemd-run --unit=\$UNIT --collect $(command -v bash 2>/dev/null || echo /bin/bash) \$RUN/root.sh >/dev/null 2>&1
echo started\" | eval $ROOTC >/dev/null 2>&1
  echo S8-CLIENT-STARTED
  for _ in \$(seq 1 150); do [ -f \"\$RUN/done\" ] && break; sleep 1; done
  [ -f \"\$RUN/done\" ] || echo \"S8-FAIL: the host-side unit never finished\"
else
  echo S8-FAIL: no forward socket
fi
kill \$FWD 2>/dev/null
sleep 1
echo S8-PROXY-BEGIN; cat \"\$RUN/proxy.log\" 2>/dev/null; echo S8-PROXY-END
echo S8-CLIENT-BEGIN; head -400 \"\$RUN/cli.log\" 2>/dev/null; echo S8-CLIENT-END
EOS
chmod +x s8.sh" >/dev/null
  h "$HOST_DIR/s8.sh" >"$RUNDIR/s8.log" 2>&1 &
  drv=$!
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
  [ -n "$HOST_WESTON" ] || [ -n "$HOST_FIREFOX" ] || {
    miss "WC_HOST_WESTON and WC_HOST_FIREFOX both unset: no client to run"; return; }

  # §51. A hypervisor is in this path. Block 4's timer comparison is a claim
  # about the *guest's own* scheduler rather than about the boundary, so it is
  # not gated -- the declaration is what lets a reader tell those two apart,
  # and what would permit a real cross-VM claim here.
  wc_topology_declare cross-vm \
    "the compositor is on the guest, the client on the VM host, over guest RAM" || return

  desk_up || return

  hostside_start

  # Give the client time to map, then photograph while it is up. Firefox needs
  # longer than a weston demo: §15.5 measured the first pixels arriving several
  # seconds after the connection, and the window is a bad photograph until then.
  if [ -n "$HOST_FIREFOX" ]; then sleep 40; else sleep 22; fi
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

  # --------------------------------------------------------------- STAGE 33 --
  # Type into it, on the desktop the user types on.
  #
  # `--pid`, added to `probe-input.c` for this, and it is the same rule
  # `comp_pid` follows one function up: several worktrees drive this guest and
  # every compositor on it registers `WaycomposeToplevel`, so a probe that took
  # the first window of that class would steal another run's focus and inject
  # into their client — and this run would then report a compositor that
  # ignores input, which is precisely the bug being investigated. A harness
  # that can manufacture its own symptom is worse than no harness.
  #
  # The probe is a scheduled task like everything else here, because `SendInput`
  # reaches the *session's* input queue and an ssh session has none.
  if [ -n "$TYPE_TEXT" ]; then
    echo
    echo "== 5b. typing into it, with SendInput, on WinSta0 =="
    int wctype "$DESKDIR_WIN\\probe-input.exe" \
      "--pid $CPID --class WaycomposeToplevel --wait 20 --type $TYPE_TEXT --log $DESKDIR_WIN\\typed.txt" \
      >/dev/null 2>&1
    g "cat $DESKDIR/typed.txt" >"$RUNDIR/typed.txt" 2>&1
    sed 's/^/        /' "$RUNDIR/typed.txt"
    cp "$RUNDIR/typed.txt" "$OUT/typed$SUFFIX.txt" 2>/dev/null
    grep -q "target HWND" "$RUNDIR/typed.txt" \
      && ok "probe-input found our window: $(grep -o 'target HWND [^,]*' "$RUNDIR/typed.txt")" \
      || bad "probe-input found no window of ours to type into"
    # **No photograph unless the keys actually went into our window.** The probe
    # now refuses to inject when it did not win the foreground, and this is the
    # other half: a shot taken after a refused injection is a picture of our
    # window with nothing typed in it, which is indistinguishable from the bug
    # and would go into `docs/` as evidence about this compositor. Measured on
    # the guest — another worktree's `weston-terminal` held the foreground and
    # the first version of this block shipped a photograph of *their* `htop`.
    if grep -q "REFUSED to inject" "$RUNDIR/typed.txt"; then
      miss "another window held the foreground; nothing was injected and nothing is claimed"
      note "$(grep -o 'REFUSED.*' "$RUNDIR/typed.txt" | head -1)"
    else
      note "$(grep -o 'foreground=.*' "$RUNDIR/typed.txt" | head -1)"
      sleep 2
      int wcshot "cmd.exe" \
        "/c $DESKDIR_WIN\\wc-desk.exe shot --pid $CPID --class WaycomposeToplevel --out $DESKDIR_WIN\\typed.bmp >> $DESKDIR_WIN\\shot.txt 2>&1" \
        >/dev/null 2>&1
      fetch typed.bmp "stage33-typed$SUFFIX.png" "the window after $TYPE_TEXT was typed into it"
    fi
  fi

  # --------------------------------------------------------------- STAGE 37 --
  # Where the click landed, on the desktop the user clicks on.
  #
  # `weston-terminal` draws its own title bar, and a click that lands in it
  # answers with `xdg_toplevel.move` — which this compositor hands to Windows as
  # `SC_MOVE`, and `SC_MOVE` with no mouse point is the *keyboard* move mode,
  # which warps the cursor to the caption. That is the whole of the reported
  # symptom: "clicking under the second titlebar instead starts moving the
  # window", "and in fact it snaps the mouse there".
  #
  # So the assertion is one line of the compositor's own log. A click 40 px down
  # the client area is below the bar the terminal drew, for any inset up to its
  # 32, and must produce no move at all. Before §37 the same click produced one
  # every time, because the click arrived 32 px high.
  #
# **Click first, then type, in one invocation** — `probe-input` does the mouse
  # before the keys, which is the order a user produces and the order the
  # question needs. §33's block above types into a window nobody has clicked in;
  # that is a different question and it keeps its own answer. This one is the
  # user's sequence, and it is the only way to find out whether the reported
  # dead keyboard was a consequence of the spurious move or a bug of its own:
  # Wine reproduces the move and still delivers the keys, so it cannot decide it
  # (§37.7).
  #
  # `--pid` for §33's reason, unchanged: several worktrees drive this guest and
  # every compositor on it registers `WaycomposeToplevel`.
  if [ "$CLIENT" = "weston-terminal" ]; then
    echo
    echo "== 5c. §37: a click below the bar the client drew, and then typing =="
    local moves_before moves_after frames_before frames_after
    g "cat $DESKDIR/comp-$sock.log" >"$RUNDIR/comp-click0.log" 2>&1
    moves_before=$(grep -c "handed a move to Windows" "$RUNDIR/comp-click0.log")
    frames_before=$(h "ls $SHARE/frames/*.ppm 2>/dev/null | wc -l")
    int wcclick "$DESKDIR_WIN\\probe-input.exe" \
      "--pid $CPID --class WaycomposeToplevel --wait 15 --at 200,40 --no-leave --type echo_waycompose_types --log $DESKDIR_WIN\\clicked.txt" \
      >/dev/null 2>&1
    g "cat $DESKDIR/clicked.txt" >"$RUNDIR/clicked.txt" 2>&1
    sed 's/^/        /' "$RUNDIR/clicked.txt" | head -8
    cp "$RUNDIR/clicked.txt" "$OUT/clicked$SUFFIX.txt" 2>/dev/null
    sleep 3
    g "cat $DESKDIR/comp-$sock.log" >"$RUNDIR/comp-click1.log" 2>&1
    moves_after=$(grep -c "handed a move to Windows" "$RUNDIR/comp-click1.log")
    frames_after=$(h "ls $SHARE/frames/*.ppm 2>/dev/null | wc -l")

    # The same refusal §33 added, for the same reason: a click that went into
    # somebody else's window proves nothing about ours, and reporting it as a
    # pass would be the harness manufacturing its own green.
    if grep -q "REFUSED to inject" "$RUNDIR/clicked.txt"; then
      miss "§37: another window held the foreground; nothing was clicked and nothing is claimed"
    elif ! grep -q "target HWND" "$RUNDIR/clicked.txt"; then
      bad "§37: probe-input found no window of pid $CPID to click in"
    else
      if [ "$moves_after" -gt "$moves_before" ]; then
        bad "§37: a click 40 px down the client area started $((moves_after - moves_before)) window move(s) — it landed above where it was aimed"
      else
        ok "§37: a click 40 px down the client area started no window move, so it landed below the bar the client drew"
      fi
      # The keyboard, after the click. **The frame count and not the
      # photograph** is the assertion, for the reason §37.7 records: with the
      # bug present the "picture changed" check was green because the *window
      # had moved*, 95034 pixels against 463 for one line of glyphs. A terminal
      # redraws only when its content changes, so new frames after keys are
      # about the keys — and if the click had wedged Windows in a modal move
      # loop there would be no frames at all.
      if [ "${frames_after:-0}" -gt "${frames_before:-0}" ]; then
        ok "§37: the terminal repainted $((frames_after - frames_before)) times after being clicked in and typed at"
      else
        bad "§37: nothing repainted after the click and the keys ($frames_before -> $frames_after) — the keyboard is dead after a click, and it is not the coordinate bug"
      fi
      grep -q "no evdev keycode for scan" "$RUNDIR/comp-click1.log" \
        && bad "§37: a scan code had no evdev keycode" \
        || note "§37: every scan code of the typed word translated"
      sleep 2
      int wcshot "cmd.exe" \
        "/c $DESKDIR_WIN\\wc-desk.exe shot --pid $CPID --class WaycomposeToplevel --out $DESKDIR_WIN\\clicked.bmp >> $DESKDIR_WIN\\shot.txt 2>&1" \
        >/dev/null 2>&1
      fetch clicked.bmp "stage37-clicked-then-typed$SUFFIX.png" \
        "the terminal after a click 40 px down and then typing"
    fi
  fi

  int wcshot "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe shot --class Shell_TrayWnd --out $DESKDIR_WIN\\tray2.bmp >> $DESKDIR_WIN\\shot.txt 2>&1" \
    >/dev/null 2>&1
  int wcshot "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe shot --screen --out $DESKDIR_WIN\\screen.bmp >> $DESKDIR_WIN\\shot.txt 2>&1" \
    >/dev/null 2>&1
  g "cat $DESKDIR/shot.txt" >"$RUNDIR/shot.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/shot.txt"

  fetch win.bmp    "stage8-window$SUFFIX.png"  "the client's window, frame and all"
  fetch tray2.bmp  "stage8-taskbar$SUFFIX.png" "the taskbar with the window's button in it" tray
  fetch screen.bmp "stage8-screen$SUFFIX.png"  "the screen, which is the lock screen" screen

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
  # STAGE 33: skipped under `WC_TYPE`. The drag holds the window in a modal
  # loop for 17 s of a client that lives 130, and this mode has already spent
  # its budget on the injection; a run that timed out here would lose the
  # keyboard evidence it came for.
  if [ -n "$TYPE_TEXT" ]; then
    note "== 6 skipped: WC_TYPE is set and the run is about the keyboard"
  else
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

  # Presented and *distinct* are two different questions and this stage needs
  # them apart.
  #
  # A dumped PPM is written once per `present`, on the UI thread, so counting
  # files measures the **poll loop's cadence** — how often that thread woke up —
  # and nothing else. Whether the pixels underneath were fresh is a property of
  # the **transport**, and §12.7 is the stage that fixed it by putting the arena
  # in guest RAM. Over a file-backed arena on an incoherent export the two
  # failures look identical from outside: a client that appears to run at half
  # rate, and a window that appears to freeze.
  #
  # So both numbers are printed, always. A low distinct count with a healthy
  # presented count is a transport problem; a low presented count with every
  # frame distinct is a cadence problem; and this stage's frame-rate claim
  # (§13.8) is only about the second, which is why it has to be able to show the
  # first is not what it measured.
  local tot dis
  tot=$(h "ls $SHARE/frames/*.ppm 2>/dev/null | wc -l")
  dis=$(h "md5sum $SHARE/frames/*.ppm 2>/dev/null | cut -d' ' -f1 | sort -u | wc -l")
  note "frames dumped $tot, distinct $dis  (presented = poll-loop cadence; distinct = transport)"
  echo "presented=$tot distinct=$dis" >>"$OUT/frames$SUFFIX.txt"

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
  fi   # STAGE 33: end of the block `WC_TYPE` skips

  wait $drv 2>/dev/null
  kill $relay 2>/dev/null; wait $relay 2>/dev/null
  g "cat $DESKDIR/comp-$sock.log" >"$RUNDIR/comp.log" 2>&1
  echo "        --- the compositor's own account, from the desktop:"
  grep -iE "dpi|screen|window .* (created|mapped)|caption|arena|configure" "$RUNDIR/comp.log" 2>/dev/null \
    | sed 's/^/        /' | head -20
  cp "$RUNDIR/comp.log" "$OUT/compositor$SUFFIX.log" 2>/dev/null
  cp "$RUNDIR/s8.log" "$OUT/hostside$SUFFIX.log" 2>/dev/null

  # STAGE 33. Who drew the title bar, out of the compositor's own mouth.
  #
  # Asserted rather than printed, and asserted in **both** directions, because
  # the whole of §33.4 is that the same toolkit ships a client that needs the
  # warning and a client that must not get it. A check that only looked for the
  # warning would pass a compositor that emitted it for everything.
  local decor
  decor=$(grep -oE "wc-compositor: window [0-9]+ \([^)]*\) (bound no decoration protocol|negotiated) .*" \
          "$RUNDIR/comp.log" 2>/dev/null | head -1)
  if [ -z "$decor" ]; then
    bad "the compositor said nothing about who decorates this window (§33.6)"
  elif echo "$decor" | grep -q "TWO TITLE BARS"; then
    # Not a failure of this run: it is the true answer for a client that binds
    # no decoration protocol and draws its own bar, and §33.3 is why no
    # compositor can change it. The harness records which kind of client it
    # photographed so the picture can be attributed later.
    note "§33: this client decorates itself and the photograph has two title bars"
    echo "        $decor"
  else
    ok "§33: the system's caption is the only title bar on this window"
    echo "        $decor"
  fi
  # STAGE 33. The keyboard half of the compositor's account, printed separately
  # because the grep above cannot match it and because its *absence* is the
  # finding when nothing can be typed.
  if [ -n "$TYPE_TEXT" ]; then
    echo "        --- and what it made of the keyboard:"
    grep -iE "keyboard|keymap|no evdev|layout" "$RUNDIR/comp.log" 2>/dev/null \
      | sed 's/^/        /' | head -20
  fi
  sed -n '/S8-PROXY-BEGIN/,/S8-CLIENT-END/p' "$RUNDIR/s8.log" | sed 's/^/        /' | head -40
  # **The relay, but only when the client did not get a display.** It is the one
  # process in this topology that nothing has ever printed, and it sits exactly
  # where a failure is most easily misattributed: a relay that has exited looks
  # to the proxy like a compositor that closed the connection, and looks to the
  # harness like a client that could not connect. Both readings blame the wrong
  # end. Printed conditionally rather than always, because on a good run it is
  # four uninteresting lines.
  if grep -q "failed to create display\|Broken pipe" "$RUNDIR/s8.log" 2>/dev/null; then
    echo "        --- the relay, which is the half nothing else prints:"
    sed 's/^/        /' "$RUNDIR/relay.log" 2>/dev/null | head -10
  fi
  kill_ours "$sock" >/dev/null 2>&1
  # And the task definitions, which `kill_ours` does not touch: it ends
  # *processes* and `int_bg`'s task outlives its process by design. Without this
  # every run of this mode leaves a `Ready` `…-wccomp` behind for ever, which is
  # exactly the leftover §37.8 item 5 was found by.
  reap_tasks >/dev/null 2>&1
}

# ---------------------------------------------------------------- damage -----
# BLOCK 12 on the interactive desktop: Firefox on the guest's compositor over
# guest RAM, the proxy on the VM host with `--verify-damage`, and the wheel
# injected on the desktop the window is actually on. docs/waycompose-design.md
# §84.
#
# WHY THIS MODE EXISTS, WHEN `wc-guest.sh damage` ALREADY DID
#
# Three runs of `wc-guest.sh damage` (commit 4531325) checked 47, 50 and 44
# commits with 0 stale, and in every one `probe-scroll` reported a window of
# class `WaycomposeToplevel` that EXISTS "but never became visible after
# 150 s" -- so the wheel was never sent, and the leg measured a browser
# nobody scrolled. That harness starts the compositor over ssh, which is
# session 0 (§13.2). This one starts it, and the probe, through the scheduler
# into the logged-on session, which is what `window` already does; the session
# bring-up is `desk_up`, shared with `window`, and the verdict is
# `wc_damage_verdict`, shared with `wc-guest.sh`.
#
# Firefox opens `about:support`, not `about:blank`. The ssh leg's driver opened
# `about:blank` for every Firefox leg, so even a wheel that arrived would have
# scrolled an empty page: a scroll needs something to scroll.
do_damage() {
  echo
  echo "== 12. the damage path, with a hypervisor in it, on the interactive desktop =="
  if [ -z "$HOST" ]; then miss "WC_HOST unset: the proxy has to run where QEMU is"; return; fi
  if [ -z "$HOST_FIREFOX" ]; then miss "WC_HOST_FIREFOX unset: no Firefox on $HOST"; return; fi
  local notches="${WC_NOTCHES:-60}" gap="${WC_GAP:-150}"
  OUT="$ROOT/docs/block12"; mkdir -p "$OUT"

  wc_substrate_declare guest-ram || return
  wc_topology_declare cross-vm \
    "compositor on $GUEST in the interactive session, proxy on $HOST, arena in the guest's own RAM" || return

  HOSTSIDE_URL=about:support
  HOSTSIDE_PROXY_EXTRA=--verify-damage
  desk_up || return
  hostside_start
  HOSTSIDE_URL=""; HOSTSIDE_PROXY_EXTRA=""

  # Wait for Firefox's toplevel to be mapped, from the compositor's own log,
  # rather than for a clock: on a contended guest the first map has taken
  # anywhere from 15 to 40 s.
  local mapped="" t
  for t in $(seq 1 45); do
    mapped=$(g "cat $DESKDIR/comp-$sock.log 2>/dev/null" 2>/dev/null \
             | grep -oE "window [0-9]+ mapped [0-9]+x[0-9]+ \((tabs|argb|xrgb)" | head -1 | sed "s/ (.*//")
    [ -n "$mapped" ] && break
    sleep 2
  done
  if [ -z "$mapped" ]; then
    bad "dmg: Firefox's window was never mapped in 90 s"
  else
    ok "dmg: the compositor mapped Firefox's toplevel ($mapped)"
  fi
  sleep 5   # the first frames of about:support, so the scroll has a page under it

  # The probe, through the scheduler, into the session the window is in, and
  # addressed by pid: every compositor on this guest registers the class.
  local slog="scroll-$sock.log"
  int_bg wcscroll "$DESKDIR_WIN\\probe-scroll.exe" \
    "--pid $CPID --class WaycomposeToplevel --wait 60 --notches $notches --gap $gap --settle 3000 --log $DESKDIR_WIN\\$slog" \
    >/dev/null 2>&1
  # `Start-ScheduledTask` reports success whether or not the task ran (§13.3),
  # so the task is believed when the probe's own first line exists.
  local started=""
  for t in $(seq 1 20); do
    started=$(g "cat $DESKDIR/$slog 2>/dev/null" 2>/dev/null | grep -m1 "probe-scroll: session")
    [ -n "$started" ] && break
    sleep 1
  done
  if [ -z "$started" ]; then
    bad "dmg: the scroll task was 'started' and never ran -- no line from probe-scroll in 20 s"
  else
    note "${started#probe-scroll: }"
  fi

  # Photographs while the page is moving: the whole screen, which is what the
  # person at this guest sees, and the window by pid.
  sleep 6
  g "rm -f $DESKDIR/dmg-shot.txt" >/dev/null 2>&1
  int_quiet wcshot "$DESKDIR_WIN\\wc-desk.exe shot --screen --out $DESKDIR_WIN\\dmg-screen.bmp" \
    "$DESKDIR_WIN\\dmg-shot.txt" >/dev/null 2>&1
  int_quiet wcshot "$DESKDIR_WIN\\wc-desk.exe shot --pid $CPID --class WaycomposeToplevel --out $DESKDIR_WIN\\dmg-win.bmp" \
    "$DESKDIR_WIN\\dmg-shot.txt" >/dev/null 2>&1

  # The probe ends on its own: settle + notches * gap after it found the window.
  local budget=$(( 60 + 3 + notches * gap / 1000 + 20 ))
  for t in $(seq 1 "$budget"); do
    g "cat $DESKDIR/$slog 2>/dev/null" 2>/dev/null \
      | grep -qE "notches towards the user accepted|never became visible|NO window of class" && break
    sleep 1
  done
  g "cat $DESKDIR/$slog" >"$RUNDIR/scroll.log" 2>&1
  sed 's/^/        /' "$RUNDIR/scroll.log" | head -20
  cp "$RUNDIR/scroll.log" "$OUT/scroll$SUFFIX.txt" 2>/dev/null

  # THE QUESTION BLOCK 12 COULD NOT ANSWER FROM SSH: is the window visible?
  # Asked of the probe's own description of the window it picked, and of the
  # session it ran in, so the answer carries its own provenance.
  if grep -q "probe-scroll: target hwnd .* visible=1 " "$RUNDIR/scroll.log"; then
    ok "dmg: Firefox's window is VISIBLE to a process on the interactive desktop: $(grep -o 'target hwnd .* client [0-9]*x[0-9]*' "$RUNDIR/scroll.log" | head -1)"
  else
    bad "dmg: no visible window of pid $CPID on the interactive desktop"
  fi
  sleep 2
  int_quiet wcshot "$DESKDIR_WIN\\wc-desk.exe shot --pid $CPID --class WaycomposeToplevel --out $DESKDIR_WIN\\dmg-after.bmp" \
    "$DESKDIR_WIN\\dmg-shot.txt" >/dev/null 2>&1
  g "cat $DESKDIR/dmg-shot.txt" 2>/dev/null | sed 's/^/        /'
  fetch dmg-screen.bmp "block12-screen$SUFFIX.png" "the guest's screen while Firefox scrolls" screen
  fetch dmg-win.bmp    "block12-window$SUFFIX.png" "Firefox's window, mid-scroll"
  fetch dmg-after.bmp  "block12-after$SUFFIX.png"  "Firefox's window after the scroll"

  # The host side ends when the client's own timeout does.
  wait "$drv" 2>/dev/null
  kill "$relay" 2>/dev/null; wait "$relay" 2>/dev/null
  g "cat $DESKDIR/comp-$sock.log" >"$RUNDIR/comp.log" 2>&1
  sed -n '/S8-PROXY-BEGIN/,/S8-PROXY-END/p' "$RUNDIR/s8.log" >"$RUNDIR/proxy.log"
  cp "$RUNDIR/comp.log" "$OUT/compositor$SUFFIX.log" 2>/dev/null
  cp "$RUNDIR/s8.log" "$OUT/hostside$SUFFIX.log" 2>/dev/null
  grep -E "verified [0-9]* commits" "$RUNDIR/proxy.log" | sed 's/^/        /' | head -6

  wc_damage_verdict dmg "$RUNDIR/scroll.log" "$RUNDIR/proxy.log" "$RUNDIR/comp.log"

  kill_ours "$sock" >/dev/null 2>&1
  reap_tasks >/dev/null 2>&1
}

# --------------------------------------------------------------- pinning -----
# §84. Does a `VirtualLock`ed guest-RAM arena keep its physical frames?
#
# Everything on the guest-RAM path assumes it does: the host locates the
# arena's frames once, at startup, and writes pixels into them for the rest of
# the session. `VirtualLock` promises residency -- the pages are not paged
# out -- and nothing about physical addresses. Block 12's first interactive
# run produced three symptoms that a moving page would produce and a fixed one
# would not: rows of the host's view that changed microseconds after being
# written, a content sweep that found three arena pages stamped in two frames
# each, and a translated frame list of which 4868 pages were somewhere else by
# the time the host verified them.
#
# So this measures it with nothing else running: one compositor, its arena,
# no client, no proxy. The host locates the arena by sweep three times, about
# 30 s apart, and the plans are compared page by page. A page whose backing
# frame differs between two locates has moved. Run in the interactive session
# (the scheduler) and in the ssh session as the control.
#
# §85: the compositor now refuses a guest-RAM arena without large pages, so by
# default this measures large pages only. The `VirtualLock` rows of §84.5 need
# WC_PIN_COMP_EXTRA=--arena-small-pages-may-corrupt-guest-memory, and the
# coordinator's standing rule since §84.7 is not to run that on the real guest.
pin_locate() {
  local runid="$1" pages="$2" plan="$3"
  h "echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
$HOST_DIR/wc-arena-host --qmp '$QMP' --vm '$VM_HINT' --run $runid --pages $pages \
  --outbound $WC_ARENA_OUTBOUND --plan '$plan' >'$plan.log' 2>&1
echo rc=\\\$? >>'$plan.log'
chmod a+r '$plan.log' '$plan' 2>/dev/null; true\" | eval $ROOTC" >/dev/null 2>&1
  h "cat '$plan.log' 2>/dev/null"
}

# arena page -> backing file page, one line per page, from a plan.
pin_expand() {
  awk '/^r / { off = $2 / 4096; f = strtonum($3) / 4096; n = $4 / 4096;
               for (i = 0; i < n; i++) printf "%d %d\n", off + i, f + i }'
}

do_pinning() {
  echo
  echo "== 13. does a VirtualLock'd arena keep its physical frames? (§84) =="
  if [ -z "$HOST" ]; then miss "WC_HOST unset: the locate needs the machine QEMU runs on"; return; fi
  h "mkdir -p $HOST_DIR" >/dev/null
  local locator="$LINUX_BIN/wc-arena-host"
  scp "${SSHOPT[@]}" "$locator" "$HOST:$HOST_DIR/" >/dev/null || { bad "could not copy wc-arena-host"; return; }
  local where
  for where in ${WC_PIN_WHERE:-interactive ssh}; do
    local sock="wg-pin-$where-$$" log runid="" pages="" line="" t
    log="$sock.log"
    if [ "$where" = interactive ]; then
      int_bg wcpin "$DESKDIR_WIN\\wc-compositor.exe" \
        "--socket $sock --timeout 150 --log $DESKDIR_WIN\\$log ${WC_PIN_COMP_EXTRA:-} $(wc_arena_comp_args ${WC_PIN_FRAMES:+"$DESKDIR_WIN\\$sock.frames"})" >/dev/null 2>&1
    else
      ssh "${SSHOPT[@]}" "$GUEST" "export MSYS_NO_PATHCONV=1; cd $DESKNAME && ./wc-compositor.exe \
        --socket $sock --timeout 150 --log '$DESKDIR_WIN\\$log' ${WC_PIN_COMP_EXTRA:-} $(wc_arena_comp_args ${WC_PIN_FRAMES:+"$DESKDIR_WIN\\$sock.frames"})" >/dev/null 2>&1 &
    fi
    for t in $(seq 1 40); do
      line=$(g "cat $DESKDIR/$log 2>/dev/null" 2>/dev/null \
             | grep -o "arena 0 provided from guest RAM: run 0x[0-9a-f]*, [0-9]* bytes, [0-9]* pages, locked=[a-z]*.*" | tail -1)
      [ -n "$line" ] && break
      sleep 2
    done
    if [ -z "$line" ]; then
      bad "pin/$where: the compositor never reported its arena"
      kill_ours "$sock" >/dev/null 2>&1; continue
    fi
    runid=$(echo "$line" | grep -o "run 0x[0-9a-f]*" | cut -d' ' -f2)
    pages=$(echo "$line" | grep -o "[0-9]* pages" | head -1 | cut -d' ' -f1)
    note "pin/$where: $line"
    local i prev="" moved results=""
    for i in 0 1 2; do
      [ "$i" -gt 0 ] && sleep 30
      local plan="$HOST_DIR/$sock-$i.plan"
      pin_locate "$runid" "$pages" "$plan" >"$RUNDIR/pin-$where-$i.log" 2>&1
      local v; v=$(grep -oE "verify: [0-9]+/[0-9]+ pages|claimed twice|not where the arena says|located [0-9]+ of [0-9]+ pages" "$RUNDIR/pin-$where-$i.log" | tr '\n' ' ')
      h "cat '$plan' 2>/dev/null" | pin_expand | sort -k1,1 >"$RUNDIR/pin-$where-$i.map"
      if [ -s "$RUNDIR/pin-$where-$i.map" ] && [ -n "$prev" ] && [ -s "$prev" ]; then
        moved=$(join "$prev" "$RUNDIR/pin-$where-$i.map" | awk '$2 != $3' | wc -l)
        results="$results; locate $i (+$((i * 30)) s): ${v}-> $moved of $(wc -l <"$RUNDIR/pin-$where-$i.map") pages changed frame since locate $((i - 1))"
      else
        results="$results; locate $i (+$((i * 30)) s): ${v:-no plan}"
      fi
      [ -s "$RUNDIR/pin-$where-$i.map" ] && prev="$RUNDIR/pin-$where-$i.map"
    done
    note "pin/$where${results}"
    echo "pin/$where$results" >>"$OUT/pinning$SUFFIX.txt"
    kill_ours "$sock" >/dev/null 2>&1
  done
  reap_tasks >/dev/null 2>&1
  wait 2>/dev/null
}

# ------------------------------------------------------------------ fps -----
# Does the poll loop's wait decide the frame rate? One binary, one flag.
#
# WHY THIS IS A SEPARATE MODE AND NOT A NUMBER FROM `window`
#
# Because `window` runs with `--dump-frames`, and `--dump-frames` writes a PPM
# **over virtiofs** on every present. At 250x250 that is 187 KiB per frame, and
# at the rates involved it is megabytes a second through a paravirtual
# filesystem. The first version of this measurement counted those files and
# attributed the result to the poll loop; it was measuring its own instrument.
# See docs/waycompose-design.md §13.8, which keeps the wrong number and the right one.
#
# So this mode dumps nothing and reads the compositor's own closing line --
# `xdg_toplevel destroyed -> window 1 (N frames presented)` -- which costs a
# counter increment per frame. It runs the same client for the same time twice,
# changing exactly one flag, and prints both.
do_fps() {
  echo
  echo "== 7. does the poll interval decide the frame rate? (§13.8) =="
  if [ -z "$HOST" ] || [ -z "$HOST_WESTON" ]; then
    miss "WC_HOST/WC_HOST_WESTON unset: no client, no frame rate"; return
  fi
  local secs="${WC_FPS_SECS:-45}" mode n
  local pnum=0
  for mode in fine coarse; do
    local sock="wg-s8f-$$-$mode" extra=""
    # A port per mode. Sharing one across two back-to-back runs left the second
    # with a listener in TIME_WAIT and a client that never connected -- which
    # printed as "0 commits" and looked exactly like `--poll-coarse` breaking
    # the compositor.
    local port=$((PORT + pnum)); pnum=$((pnum + 1))
    [ "$mode" = coarse ] && extra=" --poll-coarse"
    kill_ours "wg-s8f" >/dev/null 2>&1
    h "mkdir -p $SHARE $HOST_DIR" >/dev/null
    # STAGE 13: guest RAM here too. This mode is about the poll loop's cadence
    # and the transport is incidental to it -- which is exactly the argument
    # that left the incoherent one in place for five stages. It is one flag.
    purge_arena_file
    g "rm -f $DESKDIR/fps-$mode.frames" >/dev/null 2>&1
    int_bg wcfps "$DESKDIR_WIN\\wc-compositor.exe" \
      "--socket $sock --timeout $((secs + 150)) --log $DESKDIR_WIN\\fps-$mode.log $(wc_arena_comp_args "$DESKDIR_WIN\\fps-$mode.frames")$extra" \
      >/dev/null 2>&1
    sleep 8
    local plan="$HOST_DIR/$sock.plan"
    if ! locate_arena "fps-$mode.log" "$plan"; then
      kill_ours "$sock" >/dev/null 2>&1
      eval "fps_$mode=0"
      continue
    fi
    ssh "${SSHOPT[@]}" "$GUEST" \
      "export MSYS_NO_PATHCONV=1; cd $DESKNAME && ./probe-relay.exe $port '$APPDATA_WIN\\$sock' --timeout $((secs + 40))" \
      >"$RUNDIR/relay-$mode.log" 2>&1 &
    local relay=$!
    sleep 3
    # Written out and invoked rather than inlined, for the reason `do_window`'s
    # `s8.sh` is: **every background process on the far side must have its
    # stdout redirected or the outer `ssh` never returns.** An inlined version
    # of this left `ssh -N -L` holding the connection open and the whole mode
    # hung until the guest went away underneath it, which looked like the guest
    # going away underneath it.
    h "cat > $HOST_DIR/s8fps.sh <<'EOS'
#!/usr/bin/env bash
set -uo pipefail
RUN=\$(mktemp -d /tmp/wc-fps-XXXXXX); chmod 755 \"\$RUN\"
ssh -i $HOST_KEY -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR \
    -N -L \"\$RUN/up.sock:127.0.0.1:$port\" $HOST_GUEST >\"\$RUN/fwd.log\" 2>&1 &
FWD=\$!
sleep 4
if [ -S \"\$RUN/up.sock\" ]; then
  cat > \"\$RUN/root.sh\" <<'INNER'
#!/usr/bin/env bash
export PATH=/run/current-system/sw/bin:/run/wrappers/bin
RUN=__RUN__
export XDG_RUNTIME_DIR=\"\$RUN\"
$HOST_DIR/wc-proxy --upstream \"\$RUN/up.sock\" --socket wl-proxy \
  --runtime-dir \"\$RUN\" --transport arena --arena-plan $plan --arena-id 0 \
  --copy damage --accept-timeout 20 --timeout $((secs + 15)) >\"\$RUN/proxy.log\" 2>&1 &
P=\$!
sleep 2
WAYLAND_DISPLAY=wl-proxy timeout $secs $HOST_WESTON/weston-presentation-shm >\"\$RUN/cli.log\" 2>&1
sleep 2
kill \$P 2>/dev/null; wait 2>/dev/null
chmod -R a+rwX \"\$RUN\" 2>/dev/null
touch \"\$RUN/done\"
INNER
  sed -i \"s#__RUN__#\$RUN#\" \"\$RUN/root.sh\"
  chmod 755 \"\$RUN/root.sh\"
  UNIT=wc-s13f-\$\$
  echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
systemd-run --unit=\$UNIT --collect $(command -v bash 2>/dev/null || echo /bin/bash) \$RUN/root.sh >/dev/null 2>&1
echo started\" | eval $ROOTC >/dev/null 2>&1
  for _ in \$(seq 1 $((secs + 60))); do [ -f \"\$RUN/done\" ] && break; sleep 1; done
  grep -o '[0-9]* commits, [0-9]* copies' \"\$RUN/proxy.log\" 2>/dev/null | tail -1
else
  echo 'no forward socket'
fi
kill \$FWD 2>/dev/null
wait 2>/dev/null
EOS
chmod +x $HOST_DIR/s8fps.sh" >/dev/null 2>&1
    h "$HOST_DIR/s8fps.sh" >"$RUNDIR/fps-$mode.host" 2>&1
    kill $relay 2>/dev/null; wait $relay 2>/dev/null
    sleep 3
    g "cat $DESKDIR/fps-$mode.log" >"$RUNDIR/fps-$mode.log" 2>&1
    kill_ours "$sock" >/dev/null 2>&1
    n=$(grep -oE "\([0-9]+ frames presented\)" "$RUNDIR/fps-$mode.log" | tail -1 | grep -oE "[0-9]+")
    local turn; turn=$(grep -oE "poll interval .* using .*" "$RUNDIR/fps-$mode.log" | tail -1)
    printf '        %-7s %-6s frames in %ss  (%s)  proxy: %s\n' \
      "$mode" "${n:-?}" "$secs" "${turn:-no line}" "$(cat "$RUNDIR/fps-$mode.host" 2>/dev/null | tail -1)"
    eval "fps_$mode=\${n:-0}"
  done
  {
    echo "client weston-presentation-shm, ${secs}s, guest-RAM arena, no --dump-frames"
    echo "fine   (high-resolution waitable timer) $fps_fine frames"
    echo "coarse (--poll-coarse, Sleep)           $fps_coarse frames"
  } >"$OUT/fps.txt"
  if [ "${fps_fine:-0}" -gt 0 ] && [ "${fps_coarse:-0}" -gt 0 ]; then
    note "$(awk -v a="$fps_fine" -v b="$fps_coarse" -v s="$secs" 'BEGIN{printf "%.1f fps with the timer, %.1f with Sleep (%.2fx)", a/s, b/s, a/b}')"
    # Deliberately not an assertion in either direction. The point of this mode
    # is to *find out*, and the answer changed the design doc once already.
    ok "both cadences ran and are recorded in docs/stage8/fps.txt"
  else
    bad "one of the two runs produced no frame count"
  fi
}

# ----------------------------------------------------------------- psgate ----
# Check the PowerShell this script generates, **without a guest**.
#
# STAGE 33, added with §16.8's eighth instance. Three of the four files deployed
# here are PowerShell written inside quoted heredocs, and until now the only way
# to find out whether any of it parsed was to run it on the guest — which is a
# contended machine that is regularly embargoed, and which answers a syntax
# error and a logic error with the same silence, because a scheduled task's
# stdout goes nowhere. `reaptasks.ps1` was written during an embargo and this
# mode is how it was checked at all.
#
# Two levels, and the second is the one that found something. The parser says
# the file is PowerShell; the unit test says `Ours` decides what it is supposed
# to decide. Writing the test immediately caught a case the parser cannot see —
# `function T($exe, $args)` in the *test* silently bound nothing, because
# `$args` is an automatic variable, which is the same hazard the `-Home`
# comment in `reaptasks.ps1` warns about and is why that comment is there.
do_psgate() {
  echo "== the generated PowerShell, checked on this machine =="
  # §51. No guest at all: this leg parses the PowerShell the script generates.
  # `wc-desktop.sh`'s other legs declare cross-vm -- the topology is a property
  # of the leg, not of the file.
  wc_topology_declare same-box \
    "pwsh parses the generated PowerShell here; nothing is deployed and no client runs" || return
  if ! command -v pwsh >/dev/null; then
    miss "pwsh is not on PATH (\`nix-shell -p powershell\`); the generated PowerShell is unchecked"
    return
  fi
  local d="$RUNDIR/ps"; mkdir -p "$d"
  write_runint "$d/runint.ps1"
  write_killours "$d/killours.ps1"
  write_reaptasks "$d/reaptasks.ps1"

  cat >"$d/parse.ps1" <<'PSG'
param([Parameter(Mandatory=$true)][string]$Dir)
$bad = 0
Get-ChildItem -Path $Dir -Filter *.ps1 | Where-Object { $_.Name -ne 'parse.ps1' -and $_.Name -ne 'ours.ps1' } | ForEach-Object {
  $errs = $null; $toks = $null
  [void][System.Management.Automation.Language.Parser]::ParseFile($_.FullName, [ref]$toks, [ref]$errs)
  if ($errs -and $errs.Count -gt 0) {
    $bad = 1
    Write-Output ("PARSE " + $_.Name)
    $errs | ForEach-Object { Write-Output ("    line " + $_.Extent.StartLineNumber + ": " + $_.Message) }
  } else {
    Write-Output ("ok " + $_.Name + " (" + $toks.Count + " tokens)")
  }
}
exit $bad
PSG
  if pwsh -NoProfile -File "$d/parse.ps1" -Dir "$d" >"$d/parse.log" 2>&1; then
    ok "every generated .ps1 parses: $(tr '\n' ' ' <"$d/parse.log")"
  else
    bad "generated PowerShell does not parse"; sed 's/^/        /' "$d/parse.log"
  fi

  # The ownership rule, against the cases it exists to separate. `wcterm2` is
  # the one worth having: a `Contains` without the trailing separator says a
  # sibling worktree's task is ours, and reaping it is the interference this
  # whole function is here to stop causing.
  sed -n '/^function Ours/,/^}$/p' "$d/reaptasks.ps1" >"$d/ours.ps1"
  cat >"$d/ourstest.ps1" <<'PSG'
param([Parameter(Mandatory=$true)][string]$Dir)
. (Join-Path $Dir 'ours.ps1')
# **Not `$args`**: an automatic variable, and naming a parameter after one binds
# nothing. This comment is here because writing it that way is exactly what
# happened, and the test reported a defect in the code under test.
function T($exe, $argstr) {
  [pscustomobject]@{ Actions = @([pscustomobject]@{ Execute = $exe; Arguments = $argstr }) }
}
$dir = 'C:\Users\user\wcterm'
$cases = @(
  @{ n='the compositor, exe under our directory'; t=(T 'C:\Users\user\wcterm\wc-compositor.exe' '--socket wg-s8-1'); want=$true },
  @{ n='cmd.exe with our path in the arguments';  t=(T 'cmd.exe' '/c C:\Users\user\wcterm\wc-desk.exe windows'); want=$true },
  @{ n='a different case for the same path';      t=(T 'c:\users\user\WCTERM\wc-desk.exe' ''); want=$true },
  @{ n='another worktree entirely';               t=(T 'C:\Users\user\wcptr\wc-compositor.exe' '--socket x'); want=$false },
  @{ n='the prefix collision, wcterm2';           t=(T 'C:\Users\user\wcterm2\wc-compositor.exe' ''); want=$false },
  @{ n='cmd.exe running nothing of ours';         t=(T 'cmd.exe' '/c echo hi'); want=$false },
  @{ n='a task with no action at all';            t=([pscustomobject]@{ Actions = @() }); want=$false }
)
$bad = 0
foreach ($c in $cases) {
  $got = [bool](Ours $c.t $dir)
  if ($got -ne $c.want) { $bad = 1; Write-Output ("FAIL " + $c.n + " -> $got, want $($c.want)") }
}
if ($bad -eq 0) { Write-Output ("all " + $cases.Count + " ownership cases agree") }
exit $bad
PSG
  if pwsh -NoProfile -File "$d/ourstest.ps1" -Dir "$d" >"$d/ours.log" 2>&1; then
    ok "reaptasks.ps1 decides ownership by what a task runs: $(cat "$d/ours.log")"
  else
    bad "the ownership rule is wrong"; sed 's/^/        /' "$d/ours.log"
  fi
}

case "$MODE" in
  deploy) do_deploy ;;
  probe)  do_probe ;;
  window) do_window ;;
  fps)    do_fps ;;
  psgate) do_psgate ;;
  damage) do_deploy; do_damage ;;
  pinning) do_deploy; OUT="$ROOT/docs/block12"; mkdir -p "$OUT"; do_pinning ;;
  all)    do_psgate; do_deploy; do_probe; do_window; do_fps ;;
  *) echo "usage: $0 {deploy|probe|window|fps|psgate|damage|pinning|all}"; exit 2 ;;
esac

echo
echo "== $pass passed, $fail failed, $skip skipped =="
[ "$fail" -eq 0 ]
