#!/usr/bin/env bash
# Photograph the title bar, on the guest's **interactive desktop**, with the
# pixels in the guest's own RAM.
#
#   scripts/wc-titlebar.sh deploy   cross-compile and copy what this run needs
#   scripts/wc-titlebar.sh local    §46, headless: two clients, one run, one flag
#   scripts/wc-titlebar.sh server   one caption: default_mode(2) + the env var
#   scripts/wc-titlebar.sh client   the arrangement before this stage, for the pair
#   scripts/wc-titlebar.sh tabs     TABS IN THE TITLE BAR: the caption folded into the client
#                                   area, so the tab strip is *inside* the frame
#   scripts/wc-titlebar.sh noglass  the same, without DwmExtendFrameIntoClientArea
#   scripts/wc-titlebar.sh nobuttons  the same, with the client's pixels over the
#                                   whole caption and no system buttons at all
#   scripts/wc-titlebar.sh default  §82: no --decorations, the launcher's profile -- what a user
#                                   gets; WC_TB_GESTURES=1 adds real input (unlocked session)
#   scripts/wc-titlebar.sh both     deploy, then server and client
#   scripts/wc-titlebar.sh pair     deploy, then tabs and server
#   scripts/wc-titlebar.sh all      deploy, then all five
#   scripts/wc-titlebar.sh clean    remove what this script put on the guest
#
# Run from inside `nix develop`. Pictures land in docs/stage17/ and, for the
# two legs the tabs-in-title-bar stage added, docs/tabs-in-titlebar/.
#
# WHY TABS IN THE TITLE BAR's LEGS ARE HERE AND NOT IN A SCRIPT OF THEIR OWN
#
# Because the plumbing below is the claim's other half and duplicating it would
# fork it. §22.6's argument was that a picture of a window with one caption
# proves nothing on its own, and the tabs-in-title-bar stage's is the same shape but stronger: a
# window whose tab strip is inside the caption and a window whose tab strip is
# under a caption differ by about thirty pixels of vertical offset, so the pair
# — the *same* client, the same arena, the same desktop, minutes apart, one
# flag apart — is the only form the evidence can take.
#
# `noglass` is a third leg for the same reason in miniature. It answers "which
# half of the arrangement is load-bearing" with a picture rather than an
# opinion: `WM_NCCALCSIZE` with `DwmExtendFrameIntoClientArea` and
# `WM_NCCALCSIZE` without it, and the difference between the two captures is
# exactly what the DWM call is worth here.
#
# WHY A SCRIPT OF ITS OWN, AND NOT A BLOCK IN wc-desktop.sh OR wc-guest.sh
#
# Because it is the *intersection* of the two and neither has the other half.
# `wc-guest.sh` runs Firefox over guest RAM and runs the compositor in an ssh
# session, which is session 0 — every Win32 call succeeds there and nothing is
# ever on a screen (§10.12 item 3), so there is no caption to photograph.
# `wc-desktop.sh` reaches the interactive desktop and photographs windows, and
# its client is `weston-terminal` over a **file** arena on the virtio-fs export.
# This stage's claim is about what Firefox's pixels look like inside a real
# Windows frame, so it needs the desktop *and* Firefox *and* — by the project's
# standing rule — an arena in guest RAM rather than a file.
#
# That intersection is §17.10 item 5 and §17.11 item 4, which named it as one
# run nobody had done. It is done here because this stage cannot be believed
# without it: the whole claim is a picture.
#
# WHAT THE TWO LEGS ARE FOR
#
# `server` is the arrangement this stage ships. `client` is the arrangement
# every previous stage shipped, run again, on the same machine, minutes apart.
# The pair is the evidence; a single picture of a window with one caption proves
# nothing, because a window with one caption is also what a *failed* Firefox
# looks like.
#
# THREE THINGS THAT ARE NOT OPTIONAL, EACH PAID FOR BY SOMEBODY ELSE FIRST
#
# * **Identity is a path or a pid, never an image name** (§16.8). Three other
#   experiments run against this guest. The directory, the socket, the scheduled
#   task and the export subdirectory all carry `WC_TBNAME`, and every kill is
#   matched on the socket name in a command line.
# * **A backgrounded process in an ssh session dies with the session**
#   (`wc-guest.sh`'s first operational lesson), so the relay gets its own held
#   connection and the compositor is a scheduled task, which does not.
# * **Root on WC_HOST, for exactly one call.** QEMU is a system service there,
#   so its QMP socket and its `/proc/<pid>/fd` are root-only, and the proxy that
#   opens the backing object has to be root too (§12.9). Both go through
#   `~/rootc`, and the long-lived half goes through `systemd-run --collect`
#   because a root call that blocks for a minute frequently returns nothing.
set -uo pipefail

MODE="${1:-both}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
OUT="$ROOT/docs/stage17"
OUTTB="$ROOT/docs/tabs-in-titlebar"

GUEST="${WC_GUEST:-<guest-user>@<guest-ip>}"
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"
# Everything this run owns is named from this. See the header.
TBNAME="${WC_TBNAME:-wcs17tb}"
GDIR="/c/Users/<guest-user>/$TBNAME"
GDIR_WIN="C:\\Users\\<guest-user>\\$TBNAME"
APPDATA_WIN='C:\Users\<guest-user>\AppData\Local\waycompose'

HOST="${WC_HOST:-}"                                   # the machine QEMU runs on
HOST_DIR="${WC_HOST_DIR:-/tmp/$TBNAME}"
SHARE="${WC_HOST_SHARE:-/srv/winshare/$TBNAME}"       # the export, as the host spells it
SHARE_WIN="${WC_GUEST_SHARE:-Z:\\$TBNAME}"            # and as the guest spells it
HOST_GUEST="${WC_HOST_GUEST:-<guest-user>@<guest-nat-ip>}"        # how the host reaches the guest
HOST_KEY="${WC_HOST_KEY:-~/.ssh/wc_guest}" # absolute: the unit is root
HOST_FIREFOX="${WC_HOST_FIREFOX:-}"                   # absolute path; no guessing
QMP="${WC_QMP:-/run/windows-vm/qmp.sock}"
VM_HINT="${WC_VM_HINT:-windows-vm}"
ROOTC="${WC_ROOTC:-\$HOME/rootc}"
PORT="${WC_PORT:-57391}"
SECS="${WC_SECS:-70}"                                 # how long Firefox runs
ARENA_PAGES="${WC_ARENA_PAGES:-16384}"                # 64 MiB
ARENA_OUTBOUND="${WC_ARENA_OUTBOUND:-1048576}"
# The one `wl_output`, and it matters: with none at all GTK sizes Firefox
# against an empty monitor list and produces 552x172 (§15.3), and a different
# one moves the window size the assertions below use. §15.6's number.
OUTPUT="${WC_OUTPUT:-1600x1000}"

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
h() { ssh "${SSHOPT[@]}" "$HOST" "$@"; }
have() { command -v "$1" >/dev/null 2>&1; }

# STAGE 42. `fresh` and `deployed_is`. Sourced after the reporters and `g`/`h`,
# which it uses; this harness deploys across *both* boundaries, so it uses both.
. "$(dirname "$0")/wc-binaries.sh"
. "$(dirname "$0")/wc-topology.sh"

# Run something on the interactive desktop, and wait for it.
int() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$GDIR_WIN\\runint.ps1' -Exe '$1' -Arguments '$2' \
       -Name '$TBNAME-$3' -WorkDir '$GDIR_WIN' -Wait -TimeoutSec 120" 2>&1
}
# The same, without waiting: the compositor has to outlive the call.
int_bg() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$GDIR_WIN\\runint.ps1' -Exe '$1' -Arguments '$2' \
       -Name '$TBNAME-$3' -WorkDir '$GDIR_WIN'" 2>&1
}
# Compositors of *this* run only, matched on the socket name in their command
# line. Never `taskkill /IM`: that is somebody else's compositor too, and
# without `/F` it is a `WM_CLOSE` to their windows as well.
ours() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$GDIR_WIN\\killours.ps1' -Match '$1' ${2:-}" 2>/dev/null | tr -d '\r'
}

# --------------------------------------------------------------- the ps1 -----
# Written from here rather than kept beside this script, for `wc-desktop.sh`'s
# reason: they are one half of this file, and a copy edited on the guest would
# fork it silently.
write_runint() { cat >"$1" <<'PS1'
# Run a command on the interactive desktop of the logged-on session.
# Generated by scripts/wc-titlebar.sh; do not edit on the guest.
param(
  [Parameter(Mandatory=$true)][string]$Exe,
  [string]$Arguments = "",
  [string]$Name = "wctb",
  [string]$WorkDir = "C:\Users\<guest-user>",
  [switch]$Wait,
  [int]$TimeoutSec = 120
)
$ErrorActionPreference = "Stop"
$a = New-ScheduledTaskAction -Execute $Exe -Argument $Arguments -WorkingDirectory $WorkDir
# sshd sets USERDOMAIN=WORKGROUP, which resolves to no SID at all; ask the token
# for the name instead. wc-desktop.sh paid a round for this line.
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
  Write-Output ("exit " + (Get-ScheduledTaskInfo -TaskName $Name).LastTaskResult)
  Unregister-ScheduledTask -TaskName $Name -Confirm:$false
}
PS1
}

write_killours() { cat >"$1" <<'PS2'
# Generated by scripts/wc-titlebar.sh; do not edit on the guest.
param([Parameter(Mandatory=$true)][string]$Match, [switch]$PidOnly)
Get-CimInstance Win32_Process -Filter "Name = 'wc-compositor.exe'" |
  Where-Object { $_.CommandLine -and $_.CommandLine.Contains($Match) } |
  ForEach-Object {
    if ($PidOnly) { Write-Output $_.ProcessId }
    else { Write-Output ("killing pid " + $_.ProcessId)
           Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
  }
PS2
}

# ---------------------------------------------------------------- deploy -----
do_deploy() {
  echo "== deploy =="
  # STAGE 42. **Existence is not freshness**, and this harness already has the
  # better half of the argument written down two comments below: looking in the
  # wrong directory "is how a deploy silently ships the last run's copy". The
  # same is true of looking in the *right* directory at a file that is simply
  # old — §35.4's measurement, and §35.6 item 6 named this harness.
  local e
  local how_win="cargo build --workspace --target x86_64-pc-windows-gnu"
  for e in wc-compositor.exe wc-desk.exe; do
    fresh "$WIN_BIN/$e" "$how_win" || exit 1
  done
  # `wc-arena-host` is its own workspace — it links shared_vulkan's crates and
  # deliberately inherits nothing from this one — so its binary is not in the
  # shared target directory and looking for it there is how a deploy silently
  # ships the last run's copy.
  local arena_host="$ROOT/crates/wc-arena-host/target/debug/wc-arena-host"
  fresh "$LINUX_BIN/wc-proxy" "cargo build --workspace" || exit 1
  fresh "$arena_host" "cargo build --manifest-path crates/wc-arena-host/Cargo.toml" || exit 1

  local stage; stage=$(mktemp -d /tmp/wc-tb-stage-XXXXXX)
  cp "$WIN_BIN/wc-compositor.exe" "$WIN_BIN/wc-desk.exe" "$stage/"
  if [ -n "${WC_XKB_MINGW:-}" ]; then
    x86_64-w64-mingw32-gcc -O1 -o "$stage/probe-relay.exe" "$ROOT/scripts/probe-relay.c" \
      -lws2_32 2>"$stage/cc.log" \
      || { echo "  FAILED to build probe-relay.exe"; cat "$stage/cc.log"; exit 1; }
  else
    echo "error: WC_XKB_MINGW unset -- run inside \`nix develop\`"; exit 1
  fi
  write_runint  "$stage/runint.ps1"
  write_killours "$stage/killours.ps1"

  # A running .exe holds its own file open and scp over it fails as
  # `dest open ...: Failure`. Only this run's own copies are stopped.
  ours "$TBNAME" >/dev/null 2>&1
  g "mkdir -p $GDIR" || exit 1
  scp "${SSHOPT[@]}" "$stage"/* "$GUEST:$TBNAME/" >/dev/null || exit 1
  echo "  copied $(ls "$stage" | grep -c .) files to $GUEST:$GDIR"
  # STAGE 42. `scp` returning 0 says the transfer ran, not that the bytes over
  # there are this tree's. See the comment above: a running `.exe` usually makes
  # it *fail*, and the case this catches is the one where it does not.
  deployed_all "$stage" "$GDIR" g "re-run $0 deploy with nothing of ours running on the guest" \
    "wc-compositor.exe" "wc-desk.exe" || exit 1
  rm -rf "$stage"

  [ -n "$HOST" ] || { echo "  WC_HOST unset: nothing copied to the VM host"; return; }
  h "mkdir -p $HOST_DIR $SHARE" >/dev/null
  # A *live* Linux executable cannot be written over either: `ETXTBSY` arrives
  # as `dest open ...: Failure`. Matched on the full path so another worktree's
  # proxy, in its own directory, is left alone.
  h "pkill -f '^$HOST_DIR/wc-proxy' 2>/dev/null; sleep 1; true" >/dev/null 2>&1
  scp "${SSHOPT[@]}" "$LINUX_BIN/wc-proxy" "$arena_host" "$HOST:$HOST_DIR/" \
    >/dev/null || { echo "  could not copy to $HOST"; exit 1; }
  h "chmod +x $HOST_DIR/wc-proxy $HOST_DIR/wc-arena-host" >/dev/null
  echo "  copied wc-proxy and wc-arena-host to $HOST:$HOST_DIR"
  # STAGE 42, and the *other* boundary. `ETXTBSY` on a live Linux executable is
  # the noisy failure the `pkill` above avoids; a copy that lands next to a
  # binary another worktree is mid-run with is the quiet one. Same check, the
  # caller's own `h` as the runner.
  deployed_is "$LINUX_BIN/wc-proxy"  "$HOST_DIR/wc-proxy"      h "re-run $0 deploy" || exit 1
  deployed_is "$arena_host"          "$HOST_DIR/wc-arena-host" h "re-run $0 deploy" || exit 1
  mkdir -p "$OUT"
}

# ------------------------------------------------------------------ run ------
# One leg. `$1` names it, and it is the only difference between the runs: same
# binaries, same arena, same client, same output size, minutes apart. The
# environment variable moves with the mode because the two levers are a pair
# (§22.4) and a run that changed one without the other would be the control
# rather than the result.
#
# TABS IN THE TITLE BAR turned the leg name and the `--decorations` value into two different
# things, because `noglass` is `tabs` with one more flag and giving it a name of
# its own is what keeps its socket, its port, its log and its picture separate
# from the leg it is being compared with.
do_run() {
  local leg=$1 mode glass_arg="" outdir="$OUT" picture
  case "$leg" in
    server|client) mode=$leg ;;
    tabs)          mode=tabs; outdir="$OUTTB" ;;
    noglass)       mode=tabs; glass_arg="--dwm-glass off"; outdir="$OUTTB" ;;
    nobuttons)     mode=tabs; glass_arg="--caption-buttons off"; outdir="$OUTTB" ;;
    # §82: the compiled-in default (no --decorations) and the launcher's
    # default profile -- the arrangement a user gets from the shortcut.
    default)       mode=auto; outdir="$OUTTB" ;;
    *) bad "unknown leg $leg"; return ;;
  esac
  case "$leg" in
    server|client) picture="$outdir/stage17-$leg.png" ;;
    *)             picture="$outdir/$leg.png" ;;
  esac
  echo
  echo "== $leg: Firefox on the guest's desktop, over the guest's own RAM =="
  [ -n "$HOST" ]         || { miss "WC_HOST unset: the proxy has to run where QEMU is"; return; }
  [ -n "$HOST_FIREFOX" ] || { miss "WC_HOST_FIREFOX unset: no Firefox path on $HOST"; return; }
  # §51. A hypervisor is in this path, declared so that a claim about how long
  # anything took across it is available here -- and refused in the Wine
  # harnesses, which is what §27.6 needed and did not have.
  wc_topology_declare cross-vm \
    "the compositor is on the guest and Firefox is on the VM host, over the guest's own RAM" || return
  have magick            || { miss "no imagemagick here: nothing could be converted"; return; }

  local sock="wtb-$leg-$$"
  # A port of its own per leg. The two legs run minutes apart and a relay from
  # the first can still hold the port when the second binds: the second bind
  # then fails, the *first* relay accepts the connection, forwards it into a
  # socket node whose compositor has gone, and the client reports
  # `we don't have any display` — which reads as a client that never started and
  # is a stale listener. It cost one leg to find.
  local port=$PORT
  case "$leg" in
    client)    port=$((PORT + 1)) ;;
    tabs)      port=$((PORT + 2)) ;;
    noglass)   port=$((PORT + 3)) ;;
    nobuttons) port=$((PORT + 4)) ;;
    default)   port=$((PORT + 5)) ;;
  esac
  local arena=$((ARENA_PAGES * 4096))
  local fxenv=""
  # Only the `server` leg. The other three want Firefox decorating itself —
  # `client` so there is a second title bar to photograph, `tabs` and `noglass`
  # because the tab strip they put inside the caption is the thing this
  # variable turns off (§22.3).
  [ "$leg" = server ] && fxenv="MOZ_GTK_TITLEBAR_DECORATION=none"
  # §82: `default` passes no --decorations at all (the claim is the default),
  # and gives Firefox exactly what `wc-launch` gives it: MOZ_GTK_TITLEBAR_
  # DECORATION=client plus the user.js and userChrome.css `wc-launch
  # --write-firefox-profile` renders, at WC_TB_DPI (the guest desktop's DPI;
  # 96 unless told).
  local decor_arg="--decorations $mode" profjs="" profcss=""
  if [ "$leg" = default ]; then
    decor_arg=""
    fxenv="MOZ_GTK_TITLEBAR_DECORATION=client"
    local pd="$RUNDIR/profile-$leg"
    "$LINUX_BIN/wc-launch" --write-firefox-profile tabs "${WC_TB_DPI:-96}" "$pd" >/dev/null \
      || { bad "$leg: wc-launch would not render the profile"; return; }
    profjs=$(base64 -w0 <"$pd/user.js")
    profcss=$(base64 -w0 <"$pd/chrome/userChrome.css")
  fi
  # Escaped for the same double-quoted-into-quoted-heredoc trip as `gtkini`
  # below: `\$RUN` survives to the host. Asserted on the host afterwards.
  local profline=""
  [ -n "$profjs" ] && profline="echo $profjs | base64 -d > \"\$RUN/profile/user.js\"; mkdir -p \"\$RUN/profile/chrome\"; echo $profcss | base64 -d > \"\$RUN/profile/chrome/userChrome.css\""

  # ----- TABS IN THE TITLE BAR: the corner Windows' own caption buttons are drawn in -------
  #
  # **There is no Wayland protocol that carries this**, and the shape of the
  # problem is §22.8's again rather than a new one: the protocol has no way for
  # a compositor to say "this rectangle of your surface is mine". Firefox does
  # not know that Windows is drawing a minimise, a maximise and a close button
  # over the last 146 px of its tab strip, so it goes on drawing its own
  # widgets underneath them and goes on putting tabs there when there are
  # enough tabs to reach.
  #
  # The one lever that exists is the client's own GTK configuration, and it is
  # delivered exactly the way §22.3's environment variable is — out of band, on
  # the machine the client runs on, by whatever launches it.
  #
  # **The layout is `:minimize,maximize,close`, and the first attempt at this
  # was `menu:`, which was exactly backwards.** `menu:` names no right-hand
  # buttons, so Firefox draws none — and a tab strip with no buttons to make
  # room for extends its tabs and its tab-list chevron all the way to the right
  # edge, which is the corner Windows is painting three caption buttons over.
  # The chevron ends up *behind* them. Naming three buttons is what makes
  # Firefox **reserve** that corner: it lays out its own minimise, maximise and
  # close there, stops the tab strip before them, and the transparent rectangle
  # then covers Firefox's three with Windows' three instead of covering
  # something that needed to be seen.
  #
  # So the reservation is real and it is a coincidence of widths rather than an
  # agreement: GTK's three buttons and DWM's three are within a few pixels of
  # each other at 100%, and nothing makes that true at another scale. §31.8 is
  # still the honest statement — there is no protocol for "this rectangle is
  # mine" — but this is much closer to right than leaving the corner unclaimed.
  #
  # Written for the three `tabs` legs only. The `server` leg has no client-side
  # title bar to lay out and the `client` leg is the before-picture.
  #
  # THE ESCAPING HERE IS THE THIRD ATTEMPT AND THE FIRST TWO LOOKED LIKE
  # SUCCESSES. This string is interpolated into a double-quoted argument to
  # `h`, and lands inside a **quoted** heredoc (`<<'EOS'`) on the VM host, so
  # `$RUN` must survive this side to be expanded when the script runs there —
  # `\$RUN`, one backslash, not two. `\\n` likewise becomes `\n` in the file,
  # where `\\\\n` becomes a literal backslash-n and `printf` writes the
  # characters rather than a newline. Both mistakes produce a run in which
  # everything passes: the leg is green, the picture is plausible, and the
  # setting simply never reached the client. The check that catches it is
  # reading `$RUN/config/gtk-3.0/settings.ini` on the host afterwards, and it
  # is worth doing whenever this line is edited.
  local gtkini=""
  [ "$mode" = tabs ] && gtkini="printf '[Settings]\\ngtk-decoration-layout=:minimize,maximize,close\\n' \
> \"\$RUN/config/gtk-3.0/settings.ini\""

  # Ours only, and before the share is emptied: an interrupted run leaves a
  # compositor alive for up to its `--timeout`, still writing into the same
  # directory, and a wipe that happens first leaves this run reading the last
  # one's log.
  ours "wtb-$leg-" >/dev/null 2>&1
  g "rm -f $APPDATA_WIN/wtb-$leg-* 2>/dev/null; true" >/dev/null 2>&1
  sleep 2
  h "rm -rf $SHARE/frames-$leg; mkdir -p $SHARE/frames-$leg; chmod 777 $SHARE/frames-$leg;
     rm -f $SHARE/tb-$leg.log" >/dev/null

  # ----- 1. the compositor, on WinSta0\Default, over guest RAM ---------------
  #
  # No `--shared-dir`: the keymap is cut out of the arena's outbound window
  # (§14), so nothing this compositor creates has a name on any filesystem. The
  # log and the frame dumps go to the export because this side has to read them
  # while the guest is busy; neither is a protocol object.
  int_bg "$GDIR_WIN\\wc-compositor.exe" \
    "--socket $sock --timeout $((SECS + 150)) --output $OUTPUT $decor_arg $glass_arg \
--arena-guest-ram 0:$arena --arena-outbound 0:$ARENA_OUTBOUND \
--dump-frames $SHARE_WIN\\frames-$leg --log $SHARE_WIN\\tb-$leg.log" comp >/dev/null 2>&1

  local cpid="" t
  for t in 1 2 3 4 5 6 7 8; do
    sleep 4
    cpid=$(ours "$sock" -PidOnly | head -1)
    [ -n "$cpid" ] && break
  done
  # NOBODY IS LOGGED ON IS THE COMMONEST CAUSE, AND IT IS SILENT.
  #
  # `runint.ps1` registers the task with an `Interactive` principal — which is
  # the whole point, because a task with any other principal lands on session 0
  # and nothing it draws is ever on a screen (§10.12 item 3). With no logged-on
  # session there is no interactive desktop for such a task to run on, and
  # **`Start-ScheduledTask` returns success anyway**: no error, no exception, a
  # cheerful `started <name>`, and a task whose `Letzte Laufzeit` stays
  # `30.11.1999` and whose `Letztes Ergebnis` is 267011 —
  # `SCHED_S_TASK_HAS_NOT_RUN`. From this side that is indistinguishable from a
  # compositor that crashed on startup, and it cost this stage two runs and a
  # session of staring at a binary that ran perfectly when started by hand.
  #
  # So the failure says which one it is. `explorer.exe` is the cheap test: it is
  # per-session and it is what a logged-on desktop has.
  if [ -z "$cpid" ]; then
    local shell_n
    shell_n=$(g 'powershell.exe -NoProfile -Command "(Get-Process -Name explorer -ErrorAction SilentlyContinue | Measure-Object).Count"' 2>/dev/null | tr -d '\r ')
    if [ "${shell_n:-0}" -eq 0 ]; then
      bad "$leg: nobody is logged on to the guest, so an Interactive scheduled task cannot run"
      note "     Start-ScheduledTask reported success regardless; the tell is no explorer.exe"
      note "     and 'Letztes Ergebnis: 267011' (SCHED_S_TASK_HAS_NOT_RUN) in schtasks /Query /V."
      note "     Log on to the guest's console and re-run this leg."
    else
      bad "$leg: the compositor never started on the interactive desktop"
      note "     the guest has a logged-on session ($shell_n explorer.exe), so this is not §31.10's"
      note "     no-session case: look at $SHARE/tb-$leg.log and at schtasks /Query /TN $TBNAME-comp /V"
    fi
    return
  fi
  note "$leg: our compositor is pid $cpid, socket $sock"

  # ----- 2. find the guest's pages from outside the guest --------------------
  #
  # §12.5's ordering: the guest allocates and stamps, and only then can the host
  # sweep for them. The run id appears in the log after the stamps are written,
  # so waiting for that line is waiting for the region to be findable — a sweep
  # started earlier returns zero pages and reads exactly like an arena that was
  # never shared.
  local line runid pages
  for t in $(seq 1 40); do
    line=$(h "grep -o 'arena 0 provided from guest RAM: run 0x[0-9a-f]*, [0-9]* bytes, [0-9]* pages, locked=[a-z]*' $SHARE/tb-$leg.log 2>/dev/null | tail -1" | tr -d '\r')
    [ -n "$line" ] && break
    sleep 1
  done
  if [ -z "$line" ]; then
    bad "$leg: the compositor never reported a guest-RAM arena"
    h "head -20 $SHARE/tb-$leg.log 2>/dev/null" | sed 's/^/        /'
    ours "$sock" >/dev/null 2>&1
    return
  fi
  runid=$(echo "$line" | grep -o 'run 0x[0-9a-f]*' | cut -d' ' -f2)
  pages=$(echo "$line" | grep -o '[0-9]* pages' | cut -d' ' -f1)
  note "$leg: $line"

  h "echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
$HOST_DIR/wc-arena-host --qmp '$QMP' --vm '$VM_HINT' --run $runid --pages $pages \
  --outbound $ARENA_OUTBOUND --plan '$HOST_DIR/arena-$leg.plan' >'$HOST_DIR/locate-$leg.log' 2>&1
echo rc=\\\$? >>'$HOST_DIR/locate-$leg.log'
chmod a+r '$HOST_DIR/locate-$leg.log' '$HOST_DIR/arena-$leg.plan' 2>/dev/null; true\" | $ROOTC" \
    >/dev/null 2>&1
  local verify vok vtot
  verify=$(h "grep -o 'verify: [0-9]*/[0-9]* pages of the stitched view carry their own arena index' $HOST_DIR/locate-$leg.log 2>/dev/null" | tr -d '\r')
  # **The harness audit: the ratio inside `$verify` is now compared, not merely
  # quoted.** The check was `rc=0`, and `$verify` — "N/M pages of the stitched
  # view carry their own arena index" — was interpolated into the pass message.
  # A view stitched from the wrong physical pages verifies 3 of 4096 and still
  # exits 0, and this line printed that as a pass with the evidence against it
  # in the same sentence. Every page must carry its own index or the view is not
  # the arena.
  vok=${verify#verify: }; vok=${vok%%/*}
  vtot=${verify#*/}; vtot=${vtot%% *}
  if h "grep -q '^rc=0$' $HOST_DIR/locate-$leg.log" 2>/dev/null; then
    if [ -z "$vok" ] || [ -z "$vtot" ] || [ "$vtot" = "$verify" ]; then
      bad "$leg: wc-arena-host exited 0 but reported no page verification at all"
    elif [ "$vok" -eq "$vtot" ]; then
      ok "$leg: the guest's arena was located in QEMU's backend — $verify"
    else
      bad "$leg: only $vok of $vtot stitched pages carry their own arena index; the view is not the arena"
    fi
  else
    bad "$leg: wc-arena-host did not locate the arena"
    h "tail -12 $HOST_DIR/locate-$leg.log 2>/dev/null" | sed 's/^/        /'
    ours "$sock" >/dev/null 2>&1
    return
  fi

  # ----- 3. the relay, on its own held connection ---------------------------
  ssh "${SSHOPT[@]}" "$GUEST" \
    "export MSYS_NO_PATHCONV=1; cd $TBNAME && ./probe-relay.exe $port '$APPDATA_WIN\\$sock' --timeout $((SECS + 60))" \
    >"$RUNDIR/relay-$leg.log" 2>&1 &
  local relay=$!
  sleep 3

  # ----- 4. the proxy and Firefox, as root, on the VM host -------------------
  #
  # Root because the proxy opens the backing object at `/proc/<qemu-pid>/fd/<n>`
  # (§12.9). Firefox refuses to run as root in somebody else's session — a real
  # check against a real mistake — so the unit gives it a home, a profile and an
  # `XDG_RUNTIME_DIR` of its own rather than arguing with it.
  h "cat > $HOST_DIR/tb-$leg.sh <<'EOS'
#!/usr/bin/env bash
export PATH=/run/current-system/sw/bin:/run/wrappers/bin
RUN=/tmp/$TBNAME-$leg-run
rm -rf \"\$RUN\"; mkdir -p \"\$RUN/home\" \"\$RUN/profile\" \"\$RUN/xdg\" \"\$RUN/config/gtk-3.0\"
chmod 700 \"\$RUN/xdg\"
$gtkini
$profline
ssh -i $HOST_KEY -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR \
    -N -L \"\$RUN/up.sock:127.0.0.1:$port\" $HOST_GUEST >\"\$RUN/fwd.log\" 2>&1 &
FWD=\$!
sleep 5
if [ ! -S \"\$RUN/up.sock\" ]; then echo TB-FAIL: no forward socket >\"\$RUN/why\"; fi
XDG_RUNTIME_DIR=\"\$RUN\" $HOST_DIR/wc-proxy --upstream \"\$RUN/up.sock\" --socket wl-proxy \
  --runtime-dir \"\$RUN\" --transport arena --arena-plan $HOST_DIR/arena-$leg.plan \
  --arena-id 0 --copy damage --accept-timeout 30 --timeout $((SECS + 10)) \
  >\"\$RUN/proxy.log\" 2>&1 &
P=\$!
sleep 2
env -u DISPLAY HOME=\"\$RUN/home\" XDG_RUNTIME_DIR=\"\$RUN/xdg\" \
  XDG_CONFIG_HOME=\"\$RUN/config\" \
  WAYLAND_DISPLAY=\"\$RUN/wl-proxy\" MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland $fxenv \
  timeout $SECS $HOST_FIREFOX --no-remote --profile \"\$RUN/profile\" about:blank \
  >\"\$RUN/fx.log\" 2>&1
echo \"TB-CLIENT-RC: \$?\" >\"\$RUN/rc\"
sleep 2
kill \$P \$FWD 2>/dev/null; wait 2>/dev/null
chmod -R a+rwX \"\$RUN\" 2>/dev/null
touch \"\$RUN/done\"
EOS
chmod 755 $HOST_DIR/tb-$leg.sh" >/dev/null
  # `systemd-run` gives the unit systemd's PATH, which on NixOS has no `bash`
  # and no `env`, so a `#!/usr/bin/env bash` script exits 127 before its first
  # line with nothing but `env: 'bash': No such file or directory` in the
  # journal. Naming the interpreter is the fix.
  h "echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
systemd-run --unit=$TBNAME-$leg --collect /run/current-system/sw/bin/bash \
  $HOST_DIR/tb-$leg.sh >/dev/null 2>&1; echo started\" | $ROOTC" >/dev/null 2>&1

  # ----- 5. photograph it while it is up -------------------------------------
  #
  # `PrintWindow(PW_RENDERFULLCONTENT)`, which reads the window's DWM
  # redirection surface rather than the framebuffer — so it is the real window
  # drawn by the real theme at the real DPI, and it works while a lock screen
  # sits on top of everything (`wc-desktop.sh`'s header has the whole argument).
  # By **pid**, because another worktree's compositor registers the same class.
  sleep 35
  int "cmd.exe" \
    "/c $GDIR_WIN\\wc-desk.exe shot --pid $cpid --class WaycomposeToplevel --out $GDIR_WIN\\tb-$leg.bmp > $GDIR_WIN\\shot-$leg.txt 2>&1" \
    shot >/dev/null 2>&1
  int "cmd.exe" \
    "/c $GDIR_WIN\\wc-desk.exe windows > $GDIR_WIN\\windows-$leg.txt 2>&1" wins >/dev/null 2>&1
  g "cat $GDIR/shot-$leg.txt" >"$RUNDIR/shot-$leg.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/shot-$leg.txt"

  # ----- 5b. §82, the default leg: the hit test, and (opt-in) the gestures ----
  #
  # The hit test is `WM_NCHITTEST` sent to our own window: it injects nothing
  # and is always run. Over DWM's buttons the answer is `DwmDefWindowProc`'s,
  # which is the first time that has been read back rather than inferred.
  #
  # The gestures are **real input on a desktop a person uses**, so they run only
  # with WC_TB_GESTURES=1, and need an *unlocked* session (§55.3: a locked one
  # accepts `SendInput` and discards it). In order: hover the maximise button
  # for 1.5 s and photograph the whole screen (the snap-layouts flyout is the
  # shell's window, so `shot --screen`, not the window); drag the top edge
  # down; drag empty tab strip; double-click empty tab strip; photograph the
  # maximised window; double-click again to restore.
  if [ "$leg" = default ]; then
    int "cmd.exe" "/c $GDIR_WIN\\wc-desk.exe hittest --pid $cpid --class WaycomposeToplevel \
--at -400,2 --at 2,2 --at 800,15 --at -120,15 --at -70,15 --at -20,15 --at -400,45 \
> $GDIR_WIN\\hit-$leg.txt 2>&1" hit >/dev/null 2>&1
    g "cat $GDIR/hit-$leg.txt" | tr -d '\r' >"$RUNDIR/hit-$leg.txt" 2>&1
    sed 's/^/        /' "$RUNDIR/hit-$leg.txt"
    local hh
    hh() { grep -o "hittest *$1 .* -> [A-Z]*" "$RUNDIR/hit-$leg.txt" | grep -o '[A-Z]*$'; }
    [ "$(hh -400,2)" = HTTOP ] && ok "$leg: the top edge is HTTOP" || bad "$leg: the top edge is $(hh -400,2)"
    [ "$(hh 800,15)" = HTCLIENT ] && ok "$leg: empty tab strip is HTCLIENT" || bad "$leg: empty tab strip is $(hh 800,15)"
    [ "$(hh -70,15)" = HTMAXBUTTON ] && ok "$leg: DWM's maximise button answers HTMAXBUTTON (snap layouts' trigger)" \
      || bad "$leg: the middle of DWM's strip is $(hh -70,15), not HTMAXBUTTON"
    [ "$(hh -20,15)" = HTCLOSE ] && ok "$leg: DWM's close button answers HTCLOSE" || bad "$leg: the close button is $(hh -20,15)"
    if [ "${WC_TB_GESTURES:-0}" = 1 ]; then
      int "cmd.exe" "/c $GDIR_WIN\\wc-desk.exe mouse --pid $cpid --class WaycomposeToplevel --hover -70,15 1500 \
> $GDIR_WIN\\hover-$leg.txt 2>&1" hover >/dev/null 2>&1 &
      sleep 2
      int "cmd.exe" "/c $GDIR_WIN\\wc-desk.exe shot --screen --out $GDIR_WIN\\snap-$leg.bmp > $GDIR_WIN\\snapshot-$leg.txt 2>&1" snap >/dev/null 2>&1
      wait
      int "cmd.exe" "/c $GDIR_WIN\\wc-desk.exe mouse --pid $cpid --class WaycomposeToplevel \
--drag -400,2 -400,62 --drag 800,15 920,95 --dblclick 800,15 > $GDIR_WIN\\gest-$leg.txt 2>&1" gest >/dev/null 2>&1
      int "cmd.exe" "/c $GDIR_WIN\\wc-desk.exe shot --pid $cpid --class WaycomposeToplevel --out $GDIR_WIN\\max-$leg.bmp > $GDIR_WIN\\maxshot-$leg.txt 2>&1" maxshot >/dev/null 2>&1
      int "cmd.exe" "/c $GDIR_WIN\\wc-desk.exe mouse --pid $cpid --class WaycomposeToplevel --dblclick 800,15 > $GDIR_WIN\\restore-$leg.txt 2>&1" restore >/dev/null 2>&1
      g "cat $GDIR/gest-$leg.txt $GDIR/restore-$leg.txt" | tr -d '\r' >"$RUNDIR/gest-$leg.txt" 2>&1
      sed 's/^/        /' "$RUNDIR/gest-$leg.txt"
      scp "${SSHOPT[@]}" "$GUEST:$TBNAME/snap-$leg.bmp" "$RUNDIR/" >/dev/null 2>&1 \
        && magick "$RUNDIR/snap-$leg.bmp" "$outdir/default-snap-hover.png" 2>/dev/null \
        && note "$leg: the screen while hovering maximise -> ${outdir#"$ROOT"/}/default-snap-hover.png (look for the flyout)"
      scp "${SSHOPT[@]}" "$GUEST:$TBNAME/max-$leg.bmp" "$RUNDIR/" >/dev/null 2>&1 \
        && magick "$RUNDIR/max-$leg.bmp" "$outdir/default-maximised.png" 2>/dev/null \
        && note "$leg: maximised -> ${outdir#"$ROOT"/}/default-maximised.png"
      grep -q 'mouse.after-dbl .* zoomed=true' "$RUNDIR/gest-$leg.txt" \
        && ok "$leg: a double-click on empty tab strip maximised the window" \
        || bad "$leg: the double-click did not maximise"
    else
      note "$leg: gestures skipped (WC_TB_GESTURES=1 runs them; needs an unlocked session)"
    fi
  fi

  scp "${SSHOPT[@]}" "$GUEST:$TBNAME/tb-$leg.bmp" "$RUNDIR/tb-$leg.bmp" >/dev/null 2>&1
  mkdir -p "$outdir"
  # STAGE 42. Same vacuous gate as the four `fetch` helpers, inlined here: the
  # geometry went into the success string and was compared with nothing, so a
  # 1x1 or an all-black capture read as a PASS. This is a window capture
  # (`tb-$leg.bmp` is the toplevel with its frame), so `window` is the shape.
  if [ -s "$RUNDIR/tb-$leg.bmp" ] && magick "$RUNDIR/tb-$leg.bmp" "$picture" 2>/dev/null; then
    picture_is "$picture" window "$leg: the window, frame and all -> ${picture#"$ROOT"/}"
  else
    bad "$leg: nothing came back from the capture"
  fi

  # ----- 6. what the run says about itself -----------------------------------
  h "test -f /tmp/$TBNAME-$leg-run/done && echo done" >/dev/null 2>&1
  for t in $(seq 1 $((SECS + 40))); do
    h "test -f /tmp/$TBNAME-$leg-run/done && echo yes" 2>/dev/null | grep -q yes && break
    sleep 2
  done
  # Did the client's GTK configuration actually arrive? Asserted rather than
  # assumed, because the two ways of getting the escaping wrong above both
  # produce a green leg with the setting missing — see the comment on `gtkini`.
  if [ "$mode" = tabs ]; then
    local ini
    ini=$(h "cat /tmp/$TBNAME-$leg-run/config/gtk-3.0/settings.ini 2>/dev/null" | tr -d '\r' | tr '\n' ' ')
    case "$ini" in
      *"gtk-decoration-layout=:minimize,maximize,close"*)
        ok "$leg: the client was given a layout naming all three window buttons" ;;
      "") bad "$leg: no settings.ini reached the client; Firefox laid its title bar out with the host's default" ;;
      *)  bad "$leg: the client's settings.ini is ${ini}" ;;
    esac
  fi

  # §82: the profile, for the same reason as the settings.ini above — a lever
  # delivered out of band has to be checked for on the far side.
  if [ "$leg" = default ]; then
    h "head -c 40 /tmp/$TBNAME-$leg-run/profile/chrome/userChrome.css; echo; \
       grep -c GtkCSD /tmp/$TBNAME-$leg-run/profile/user.js" 2>/dev/null | tr -d '\r' >"$RUNDIR/prof-$leg.txt"
    grep -q '^/\* wc-launch: tabs in the title bar\.' "$RUNDIR/prof-$leg.txt" \
      && [ "$(tail -1 "$RUNDIR/prof-$leg.txt")" = 3 ] \
      && ok "$leg: the launcher's userChrome.css and the three GtkCSD prefs reached the client's profile" \
      || bad "$leg: the profile did not arrive on the host: $(tr '\n' ' ' <"$RUNDIR/prof-$leg.txt")"
  fi

  h "cat /tmp/$TBNAME-$leg-run/proxy.log 2>/dev/null" >"$RUNDIR/proxy-$leg.log"
  h "cat $SHARE/tb-$leg.log 2>/dev/null" >"$RUNDIR/comp-$leg.log"
  cp "$RUNDIR/comp-$leg.log" "$outdir/compositor-$leg.log" 2>/dev/null

  grep -q "decorations: manager default is ${mode^}" "$RUNDIR/comp-$leg.log" \
    && ok "$leg: the compositor advertised ${mode^} as the manager default" \
    || bad "$leg: the compositor's log does not say which mode it advertised"

  local asked
  asked=$(grep -o 'kde-decoration: client asked for Value([A-Za-z]*)' "$RUNDIR/comp-$leg.log" \
          | tail -1 | grep -o 'Value([A-Za-z]*)')
  case "$mode:$asked" in
    "server:Value(Server)") ok "$leg: Firefox asked for Server, on real Windows" ;;
    "client:Value(Client)") ok "$leg: Firefox asked for Client, on real Windows" ;;
    # TABS IN THE TITLE BAR. `Tabs` is `Client` on the wire and has to be: the tab strip this
    # leg is putting in the caption is drawn by the client, and it only exists
    # when the manager's default says so. A `tabs` leg where Firefox asked for
    # Server has lost the thing it is photographing.
    "tabs:Value(Client)") ok "$leg: Firefox asked for Client, on real Windows" ;;
    # §82: the default leg asks for Client too -- MOZ_GTK_TITLEBAR_DECORATION=
    # client -- under a manager default of Server (`auto`).
    "auto:Value(Client)") ok "$leg: Firefox asked for Client under the default, on real Windows" ;;
    *) bad "$leg: Firefox asked for ${asked:-nothing}" ;;
  esac

  local mapped
  mapped=$(grep -o 'window 1 mapped [0-9]*x[0-9]*' "$RUNDIR/comp-$leg.log" | tail -1)
  [ -n "$mapped" ] \
    && ok "$leg: a real HWND, sized from the geometry rectangle: $mapped" \
    || bad "$leg: the window never mapped"

  # ----- TABS IN THE TITLE BAR: the frame arithmetic, from the compositor's own log --------
  #
  # The picture is the claim, and these two lines are what stop a *pretty*
  # picture from being believed on its own. The first says Win32 answered
  # `WM_NCCALCSIZE` with a client rectangle that reaches the top of the window
  # and how many pixels that was worth; the second says whether DWM's half
  # happened. Both are absent by construction in the `server` and `client`
  # legs, and asserting their absence there is what makes the `tabs` leg's
  # presence mean something.
  local folded
  folded=$(grep -o 'folds [0-9]* px of caption into its client area[^"]*' \
           "$RUNDIR/comp-$leg.log" | tail -1)
  case "$mode" in
    tabs|auto)
      [ -n "$folded" ] \
        && ok "$leg: $folded" \
        || bad "$leg: the compositor never folded the caption into the client area"
      case "$leg:$folded" in
        tabs:*"DwmExtendFrameIntoClientArea ok"* | nobuttons:*"DwmExtendFrameIntoClientArea ok"* \
        | default:*"DwmExtendFrameIntoClientArea ok"*)
          ok "$leg: DwmExtendFrameIntoClientArea returned S_OK on real Windows" ;;
        noglass:*"--dwm-glass off"*)
          ok "$leg: the DWM call was skipped, which is what this leg is for" ;;
        *) bad "$leg: the DWM half of the arrangement is not what the flag asked for" ;;
      esac

      # The caption buttons, which are the difference between this arrangement
      # and a picture of it. DWM is asked where it put them and the answer is
      # the rectangle the blit leaves transparent, so a leg that reports a
      # rectangle has buttons and a leg that does not, does not.
      local buttons
      buttons=$(grep -o "window 1 \(keeps Windows' own caption buttons: .*\|asked for Windows' \
caption buttons and DWM would not say.*\|has no caption buttons.*\)" \
                "$RUNDIR/comp-$leg.log" | tail -1)
      case "$leg:$buttons" in
        nobuttons:*"has no caption buttons"*)
          ok "$leg: no caption buttons, which is this leg's whole point" ;;
        nobuttons:*) bad "$leg: --caption-buttons off did not take" ;;
        *:*"keeps Windows' own caption buttons"*)
          ok "$leg: ${buttons#window 1 }" ;;
        *) bad "$leg: DWM would not say where its caption buttons are: ${buttons:-nothing logged}" ;;
      esac
      # §82, the default leg only: why it folded, and the corner. The second is
      # the first measurement of the launcher's reservation against DWM's real
      # button bounds -- Wine has none (E_NOTIMPL) and the Linux leg has no
      # Windows -- so it is the one assertion in §82 only this leg can make.
      if [ "$leg" = default ]; then
        grep -q "window 1 (firefox) asked to decorate itself (Client) and was told yes" "$RUNDIR/comp-$leg.log" \
          && ok "$leg: folded because Firefox asked for Client, with no --decorations (§82)" \
          || bad "$leg: the default did not fold Firefox's caption"
        local corner
        corner=$(grep -o "window 1 \(leaves the caption-button corner clear\|DRAWS UNDER THE CAPTION BUTTONS\)[^(]*" "$RUNDIR/comp-$leg.log" | tail -1)
        case "$corner" in
          *"corner clear"*) ok "$leg: $corner -- the launcher's reservation covers DWM's buttons" ;;
          *) bad "$leg: ${corner:-no corner verdict logged}" ;;
        esac
        grep -o "window 1 maximised: client area starts at [^-]*-- [a-zA-Z' ]*" "$RUNDIR/comp-$leg.log" | tail -1 | sed "s/^/        /"
      fi
      ;;
    *)
      [ -z "$folded" ] \
        && ok "$leg: no caption was folded in, which is this leg's whole point" \
        || bad "$leg: a leg that is not \`tabs\` folded the caption in anyway"
      ;;
  esac

  local counts
  counts=$(grep -o 'mapped: [0-9]* section, [0-9]* shared-file, [0-9]* arena-slice' "$RUNDIR/comp-$leg.log" | tail -1)
  case "$counts" in
    "mapped: 0 section, 0 shared-file, "[1-9]*) ok "$leg: every pixel came out of guest RAM ($counts)" ;;
    *) bad "$leg: wrong transport served the pixels: ${counts:-nothing recorded}" ;;
  esac

  # **§31: the failing arm was a `note`, so this could not fail.** A run in which
  # Firefox presented nothing at all — the exact outcome this block exists to
  # detect — printed an informational line and left the scoreboard green. The
  # count is only emitted when the toplevel goes away, which is the reason the
  # soft arm was written, and it is a reason to assert that the toplevel *did*
  # go away rather than a reason to have no opinion about the frames.
  local presents
  presents=$(grep -o '([0-9]* frames presented)' "$RUNDIR/comp-$leg.log" | grep -o '[0-9]*' | tail -1)
  if [ -z "$presents" ]; then
    bad "$leg: the compositor never reported a frame count; no toplevel was destroyed, so nothing here was measured"
  elif [ "$presents" -ge 2 ]; then
    ok "$leg: $presents frames of Firefox's pixels were blitted into it"
  else
    bad "$leg: only $presents frame(s) presented; the window was made and never painted"
  fi

  ours "$sock" >/dev/null 2>&1
  kill "$relay" 2>/dev/null; wait "$relay" 2>/dev/null
}

do_clean() {
  echo "== clean =="
  ours "$TBNAME" >/dev/null 2>&1
  ours "wtb-" >/dev/null 2>&1
  g "rm -rf $GDIR; rm -f $APPDATA_WIN/wtb-* 2>/dev/null; true" >/dev/null 2>&1
  [ -n "$HOST" ] && h "rm -rf $HOST_DIR $SHARE /tmp/$TBNAME-*-run" >/dev/null 2>&1
  echo "  removed $GDIR and this run's leftovers"
}

# ---------------------------------------------------------------- local -----
# STAGE 46. The half of the per-window claim that does not need a guest.
#
# Every other leg in this file photographs a window, which needs the interactive
# desktop, the arena and the VM host. `auto` makes a claim a picture of one
# window cannot carry and a headless run can: **two clients in one compositor
# run come out differently.** No per-run flag can pass this leg whatever it is
# set to, which is the point — it is the assertion that separates
# `--decorations auto` from every arrangement before it, and it costs no guest.
#
# Deliberately not folded into `do_run`: that function is deploy, start on the
# desktop, locate the arena, photograph. This shares none of it.
do_local() {
  echo "== auto: two clients, one run, one flag  (headless, no guest) =="
  # §51. This leg has no guest in it, and says so. `wc-titlebar.sh`'s other legs
  # declare cross-vm; the declaration is per-leg because the topology is.
  wc_topology_declare same-box \
    "headless: the Linux wc-compositor and two weston clients, all on this box" || return
  fresh "$LINUX_BIN/wc-compositor" "cargo build --workspace" || return
  local c
  for c in weston-terminal weston-flower; do
    have "$c" || { miss "$c is not installed; the per-window claim needs both clients"; return; }
  done

  local d="$RUNDIR/local"; mkdir -p "$d/rt"; chmod 700 "$d/rt"
  local sock="wl-tb46-$$"
  # One compositor, both clients in sequence. `--once` would exit when the first
  # client left, and running them at the same time would turn "which window
  # folded" into a question about ordering. Sequential in one process is enough:
  # the flag under test is the run's and does not change between the two.
  XDG_RUNTIME_DIR="$d/rt" "$LINUX_BIN/wc-compositor" --socket "$sock" --timeout 30 \
    --decorations auto >"$d/srv.log" 2>&1 &
  local srv=$!
  sleep 1
  for c in weston-terminal weston-flower; do
    ( cd "$d" && XDG_RUNTIME_DIR="$d/rt" WAYLAND_DISPLAY="$sock" timeout 8 "$c" ) >/dev/null 2>&1
  done
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null

  # By app id, so that neither claim can be satisfied by the other window's line.
  local t f folds
  t=$(grep "org.freedesktop.weston.wayland-terminal" "$d/srv.log" | head -1)
  f=$(grep "org.freedesktop.weston.flower" "$d/srv.log" | head -1)
  echo "$t" | grep -q "caption is folded into its client area" \
    && ok "weston-terminal decorates itself, so its caption was folded: one bar, the client's" \
    || { bad "weston-terminal's caption was not folded"; echo "        $t"; }
  echo "$f" | grep -q "Windows' caption is the only title bar" \
    && ok "weston-flower draws no bar, so its caption was kept: one bar, Windows'" \
    || { bad "weston-flower's caption was folded away"; echo "        $f"; }

  # And the pair, which is the sentence a per-run flag cannot make true.
  folds=$(grep -c "the system caption folded in" "$d/srv.log")
  [ "$folds" = 1 ] \
    && ok "exactly one of the two windows folded its caption, in one run, under one flag" \
    || bad "$folds of 2 windows folded; --decorations auto is not deciding per window"
  if grep -q "THIS WINDOW HAS" "$d/srv.log"; then
    bad "the compositor still reports a defect under auto"
    grep "THIS WINDOW HAS" "$d/srv.log" | sed 's/^/        /'
  else
    ok "and neither window is reported as having two title bars or none"
  fi
}

case "$MODE" in
  deploy)  do_deploy ;;
  local)   do_local ;;
  server)  do_run server ;;
  client)  do_run client ;;
  tabs)    do_run tabs ;;
  default) do_run default ;;
  noglass)   do_run noglass ;;
  nobuttons) do_run nobuttons ;;
  both)    do_deploy; do_run server; do_run client ;;
  # TABS IN THE TITLE BAR's pair, and the order is deliberate: `tabs` first, so that a run
  # which has to be abandoned because the guest is contended has produced the
  # picture the section is about rather than its control.
  pair)    do_deploy; do_run tabs; do_run server ;;
  all)     do_deploy; do_run tabs; do_run noglass; do_run nobuttons
           do_run server; do_run client ;;
  clean)   do_clean ;;
  *) echo "usage: $0 [deploy|local|server|client|tabs|noglass|nobuttons|both|pair|all|clean]"
     exit 2 ;;
esac

echo
echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
