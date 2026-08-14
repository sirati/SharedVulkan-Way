#!/usr/bin/env bash
# Stage 11: put the client's real icon on the Windows taskbar, and photograph it.
#
#   scripts/wc-icon.sh deploy    cross-compile and copy to the guest
#   scripts/wc-icon.sh taskbar   the run: a client, an icon, two photographs
#   scripts/wc-icon.sh all       both
#
# Run from inside `nix develop`. Everything lands in docs/stage11/.
#
# WHAT THIS IS FOR
#
# docs/waycompose-design.md §13.5 item 6: the compositor's window gets a taskbar button and
# the picture on it is Windows' generic application icon, "because nothing calls
# `WM_SETICON`". Stage 11 makes something call it, and the interesting half of
# that is not the Win32 call — it is that the pixels have to be found on the
# *Linux* side, where the icon theme is, and shipped across as data.
#
# So this harness runs the whole chain and photographs the end of it:
#
#   weston-terminal        wc-proxy                    wc-compositor.exe
#   (unmodified)           (this side has the theme)   (the guest's desktop)
#        |  set_app_id --------> resolve -> PNG -> BGRA
#        |                         |  wc_icon_manager_v1 (a second connection)
#        |                         +------------------------> WM_SETICON
#        |  commit -------------> arena -----------------> a window with a button
#
# TWO RUNS, AND THE SECOND IS THE CONTROL
#
# The same client, the same compositor, the same everything except `--no-icons`
# on the proxy. That run's taskbar is stage 8's picture reproduced on today's
# tree, which is what makes the other one evidence rather than an assertion:
# the difference between the two images is the whole of what this stage did.
#
# WHICH ICON, AND WHY NOT FIREFOX'S
#
# `WC_ICON_NAME` defaults to `qemu`, which is deliberate and is about the
# *measurement* rather than about taste. The guest's taskbar has Firefox pinned
# to it (see docs/stage8/stage8-taskbar.png), so an automated search for the
# Firefox icon in a taskbar capture finds one whether or not this stage works.
# The QEMU icon is in the host's icon theme, is not in the guest's taskbar, and
# is not in Windows at all — so finding it there is only possible if it crossed.
# `WC_ICON_NAME=firefox` runs the goal's own icon for the picture; both are
# checked in.
#
# WESTON SHIPS NO DESKTOP ENTRY, WHICH IS MEASURED AND NOT A COMPLAINT
#
# `weston-terminal`'s app id is `org.freedesktop.weston.wayland-terminal` (it is
# not "weston-terminal": measured, §16.7) and the weston package installs no
# `.desktop` file for it at all. So this harness writes one, in its own data
# directory, naming an icon that the *host's real theme* holds. Everything after
# that is the real path: the theme search, the size choice, the PNG decode, the
# premultiply, the chunking, and `WM_SETICON`. What is synthetic is one file
# with three lines in it, and it is written here rather than pretended about.
#
# STAGE 41. WHICH MEMORY THESE PICTURES WERE TAKEN ON, WHICH WAS THE WRONG ONE
#
# Everything above was measured, photographed and written up while this harness
# had **no guest-RAM path at all**. Its arena was `--arena 0:N:Z:\wc11\arena` —
# a file on the virtio-fs export — and its `WC_TRANSPORT=shared-file` half put
# the pools on that same export by name. So every stage-11 result in
# docs/stage11/ and every number quoted from them in §16 crossed the substrate
# §10.3 measured incoherent for live mappings, on a harness that never said so.
#
# The results are not thereby wrong: an icon that reaches `WM_SETICON` reaches
# it whatever carried the pixels, and §10.3's incoherence is about *live*
# mappings rather than about whether a window appears. But they were taken on
# the fixture, and a run that cannot say which memory it used is a run nobody
# can check. §41 gives this harness the guest-RAM path `wc-guest.sh` has had
# since stage 7, makes it the default and the only thing reachable from a run
# that can see the guest, and makes the substrate an assertion instead of a
# line in a log. The old pictures are **not** re-taken here — that is somebody
# else's measurement to make, and §16.11 records what is on the fixture so the
# choice is an informed one.
set -uo pipefail

MODE="${1:-all}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
OUT="$ROOT/docs/stage11"

GUEST="${WC_GUEST:-<guest-user>@<guest-ip>}"
# STAGE 41. This harness ssh's into a real Windows guest, which is exactly the
# fact `wc_substrate_fixture_guard` needs in order to refuse a file. Set before
# the library is sourced, because the guard reads it.
WC_GUEST_CONFIGURED=1
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"
# STAGE 8 §13.9's rule, applied before the first run rather than after it: this
# stage deploys into its own directory, kills only processes whose command line
# carries its own socket name, and never matches on an image name. Another
# worktree is entitled to be driving this guest right now.
GUESTNAME="${WC_GUESTNAME:-wcicon}"
# AND UNDER ITS OWN IMAGE NAME, WHICH IS NOT PARANOIA.
#
# `wc-guest.sh` on an older tree does `taskkill /IM wc-compositor.exe`, and
# `taskkill` **without `/F` sends WM_CLOSE to every window of every matching
# process**. Another worktree running that reaps nothing of ours — the kill is
# polite and our process survives — but our *window* closes, the compositor
# does the right thing with `xdg_toplevel.close`, the client exits, and the
# photograph is of a taskbar with no button on it. It cost three runs here
# before the compositor's own log ("window 1 close requested", with nothing on
# this side having asked) said what it was. Deployed under a name nobody else
# kills, the whole class of problem goes away, and §13.9's "an image name is
# not an identity on this guest" gets its second demonstration.
COMP_EXE="${WC_COMP_EXE:-wc-s11-compositor.exe}"
DESKDIR="/c/Users/<guest-user>/$GUESTNAME"
DESKDIR_WIN="C:\\Users\\<guest-user>\\$GUESTNAME"
APPDATA_WIN='C:\Users\<guest-user>\AppData\Local\waycompose'
# A DIFFERENT PORT EVERY RUN, WHICH IS §13.10'S SECOND LESSON ARRIVING AGAIN.
#
# Two back-to-back runs on one TCP port leave the second's listener in
# `TIME_WAIT`, its client never connects, and what the harness reports is
# "Failed to process Wayland connection: Broken pipe" from the *client* — which
# reads as the compositor having gone away. The pid is what makes each run's
# pair of ports its own.
PORT=$(( ${WC_PORT_BASE:-57311} + ($$ % 60) * 2 ))

HOST="${WC_HOST:-}"
HOST_DIR="${WC_HOST_DIR:-/tmp/wc-stage11}"
SHARE="${WC_HOST_SHARE:-/srv/winshare/wc11}"
SHARE_WIN="${WC_GUEST_SHARE:-Z:\\wc11}"
HOST_GUEST="${WC_HOST_GUEST:-<guest-user>@<guest-nat-ip>}"
HOST_KEY="${WC_HOST_KEY:-\$HOME/.ssh/wc_guest}"
HOST_WESTON="${WC_HOST_WESTON:-}"
# Where the host's icon theme is. Not guessed from the host's environment: this
# runs over ssh, where `XDG_DATA_DIRS` is not what a session would have.
HOST_ICONS="${WC_HOST_ICONS:-/run/current-system/sw/share}"
CLIENT="${WC_CLIENT:-weston-terminal}"
APP_ID="${WC_APP_ID:-org.freedesktop.weston.wayland-terminal}"
# The whole client command line, if the default `$WC_HOST_WESTON/$WC_CLIENT` is
# not enough. Firefox is the reason: it needs `MOZ_ENABLE_WAYLAND=1`, a profile
# of its own and `env -u DISPLAY` (§15), and hard-coding that here would put
# stage 9's harness in this one.
CLIENT_CMD="${WC_CLIENT_CMD:-}"
# Which memory the pixels cross. **`guest-ram` is the only value a run against
# this guest can have**, and §41 is the change that made that true.
#
# What it used to say, and why the reason expired: "the arena is the
# deployment's answer and is the default here, but it serves exactly one
# connection (§12.13 item 6), and Firefox opens two — so the Firefox run needs
# `shared-file`". That limit was lifted in stage 12 (§15.14): `wc-proxy`'s
# `make_transport` now calls `Arena::share()` per session and prints "client N:
# joins arena 0 (M connection(s) on it now)", and §15.13 measured four of
# Firefox's connections on one region and one allocator. So the sentence
# survived four stages after the thing it described was fixed, and it was the
# argument keeping this harness on a file.
#
# `WC_TRANSPORT` is kept rather than deleted so that a person who exports the
# old value gets a sentence instead of silence — deleting the knob would make
# `WC_TRANSPORT=shared-file wc-icon.sh` run on guest RAM while its author
# believed otherwise, which is the same class of quiet disagreement this stage
# is about. Anything but `guest-ram` stops the run, and `wc-guest.sh` is named
# because that is where the file transports are still a deliberate control.
TRANSPORT="${WC_TRANSPORT:-guest-ram}"
if [ "$TRANSPORT" != guest-ram ]; then
  echo "wc-icon.sh: WC_TRANSPORT=$TRANSPORT is refused: this harness has a guest, and"
  echo "  the arena is guest RAM. The file-backed arena and shared-file pools are the"
  echo "  Wine fixture (§18.2) and no result from them is a result about the guest."
  echo "  §10.3 measured that export incoherent for live mappings: 65280 of 65536 bytes"
  echo "  wrong, in both directions. Use scripts/wc-guest.sh, where the file transports"
  echo "  are a deliberate control and the incoherence is re-measured rather than assumed."
  exit 2
fi
# Empty means "do not write a desktop entry": the app id already has a real one
# somewhere in WC_HOST_ICONS, and writing ours would shadow the thing being
# tested. That is the Firefox case, where the resolution is entirely the
# machine's own data.
ICON_NAME="${WC_ICON_NAME-qemu}"
SUFFIX="${WC_SUFFIX:-}"

# STAGE 41. The three settings the guest-RAM locate needs, spelled exactly as
# `wc-guest.sh` and `wc-desktop.sh` spell them so that one export in a shell
# configures all three harnesses. `wc-arena-host` reads QEMU's QMP monitor and
# `/proc/<qemu-pid>/fd`, and QEMU is a *system* service on this VM host, so both
# are root-only — see §12.9, which records that as a property of how testhost
# starts the VM rather than of the design. No descriptor crosses the boundary in
# either direction: what comes back is a plan, which is a list of offsets into
# an object the host opens by name for itself.
QMP="${WC_QMP:-/run/windows-vm/qmp.sock}"
VM_HINT="${WC_VM_HINT:-windows-vm}"
ROOTC="${WC_ROOTC:-\$HOME/rootc}"

SSHOPT=(-o IdentityAgent="$AGENT" -o BatchMode=yes -o LogLevel=ERROR
        -o StrictHostKeyChecking=accept-new)

RUNDIR=$(mktemp -d /tmp/wc-icon-XXXXXX); chmod 700 "$RUNDIR"
trap 'rm -rf "$RUNDIR"' EXIT

pass=0; fail=0; skip=0
ok()   { echo "  PASS  $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail + 1)); }
note() { echo "  ----  $*"; }
miss() { echo "  SKIP  $*"; skip=$((skip + 1)); }

g() { ssh "${SSHOPT[@]}" "$GUEST" "$@"; }
h() { [ -n "$HOST" ] && ssh "${SSHOPT[@]}" "$HOST" "$@"; }

# STAGE 41. Sourced after `ok`/`bad`/`note`/`g`/`h` exist, because the library
# uses all five and defining them twice is how two harnesses end up counting
# different things.
. "$(dirname "$0")/wc-substrate.sh"
. "$(dirname "$0")/wc-topology.sh"
# STAGE 42. `fresh` and `deployed_is`: is the binary newer than the tree, and is
# the copy on the guest the one this tree built.
. "$(dirname "$0")/wc-binaries.sh"

# Run a command line on the interactive desktop and wait for it.
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

kill_ours() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\killours.ps1' -Match '$1'" 2>&1
}

comp_pid() {
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
       -File '$DESKDIR_WIN\\killours.ps1' -Match '$1' -PidOnly" 2>/dev/null | tr -d '\r' | head -1
}

write_killours() { cat >"$1" <<PS2
# Generated by scripts/wc-icon.sh; do not edit on the guest.
param([Parameter(Mandatory=\$true)][string]\$Match, [switch]\$PidOnly)
Get-CimInstance Win32_Process -Filter "Name = '$COMP_EXE'" |
  Where-Object { \$_.CommandLine -and \$_.CommandLine.Contains(\$Match) } |
  ForEach-Object {
    if (\$PidOnly) { Write-Output \$_.ProcessId }
    else {
      Write-Output ("killing pid " + \$_.ProcessId)
      Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue
    }
  }
PS2
}

write_runint() { cat >"$1" <<'PS1'
# Run a command on the interactive desktop of the logged-on session.
# Generated by scripts/wc-icon.sh; do not edit on the guest. See
# docs/waycompose-design.md §13.3 for why this is a scheduled task and not a service.
param(
  [Parameter(Mandatory=$true)][string]$Exe,
  [string]$Arguments = "",
  [string]$Name = "wcicon",
  [string]$WorkDir = "C:\Users\<guest-user>\wcicon",
  [switch]$Wait,
  [int]$TimeoutSec = 120
)
$ErrorActionPreference = "Stop"
$a = New-ScheduledTaskAction -Execute $Exe -Argument $Arguments -WorkingDirectory $WorkDir
# sshd sets USERDOMAIN=WORKGROUP, which resolves to no SID at all (§13.3).
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
# STAGE 42. **This gate could not fail, and it guards stage 11's evidence.**
# What was here converted the capture and then printed its geometry *into the
# success message*, comparing it with nothing:
#
#   ok "$caption -> docs/stage11/$png ($(identify -format '%wx%h' "$png"))"
#
# A 1x1 capture, or an all-black one, converts fine and read as a PASS — so
# every photograph in docs/stage11/ was gated on ImageMagick being able to open
# the file. §41.8 found and fixed the same helper in `wc-taskbar.sh`; the audit
# that followed found the unfixed original still here and in three more
# harnesses.
#
# `kind` is required rather than defaulted, which is §41.8's other half: the
# tray shots here are **1280x48**, so one bound for every caller would be the
# impossible-threshold bug rather than a fix for the vacuous one.
fetch() {
  local remote="$1" local_png="$2" caption="$3" kind="${4:-window}"
  scp "${SSHOPT[@]}" "$GUEST:$GUESTNAME/$remote" "$RUNDIR/$remote" >/dev/null 2>&1
  if [ -s "$RUNDIR/$remote" ]; then
    mkdir -p "$OUT"
    if magick "$RUNDIR/$remote" "$OUT/$local_png" 2>/dev/null; then
      picture_is "$OUT/$local_png" "$kind" "$caption -> docs/stage11/$local_png"
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
  # STAGE 42. **Existence is not freshness.** What was here checked that each
  # binary was a file, which a binary built before the change under test also
  # is: §35.4 measured a harness reporting the *previous* tree's behaviour after
  # `cargo test` declined to relink, and §35.6 item 6 named this harness as one
  # of seventeen with no guard at all. `fresh` compares the binary's mtime
  # against every source in the tree and names the first file that is newer.
  local how_win="cargo build --workspace --target x86_64-pc-windows-gnu"
  fresh "$WIN_BIN/wc-desk.exe"       "$how_win" || exit 1
  fresh "$WIN_BIN/wc-compositor.exe" "$how_win" || exit 1
  fresh "$LINUX_BIN/wc-proxy"        "cargo build -p wc-proxy" || exit 1

  local stage; stage=$(mktemp -d /tmp/wc-icon-stage-XXXXXX)
  cp "$WIN_BIN/wc-desk.exe" "$stage/"
  # Renamed on the way in; see COMP_EXE.
  cp "$WIN_BIN/wc-compositor.exe" "$stage/$COMP_EXE"
  # The relay is a C probe and needs a mingw compiler; stage 8's copy is
  # reused when there is none, because it is the same program.
  if command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    x86_64-w64-mingw32-gcc -O1 -o "$stage/probe-relay.exe" "$ROOT/scripts/probe-relay.c" -lws2_32 \
      2>"$stage/cc.log" || { echo "  FAILED to build probe-relay.exe"; cat "$stage/cc.log"; }
  fi
  write_runint "$stage/runint.ps1"
  write_killours "$stage/killours.ps1"

  # A running .exe cannot be overwritten. Only this stage's own copies are
  # killed: `wcicon\$COMP_EXE` is a different file from `wcdesk\`s.
  kill_ours "$GUESTNAME" >/dev/null 2>&1
  g "mkdir -p $DESKDIR" || exit 1
  scp "${SSHOPT[@]}" "$stage"/* "$GUEST:$GUESTNAME/" >/dev/null || exit 1
  echo "  copied $(ls "$stage" | grep -c .) files to $GUEST:$DESKDIR"

  # STAGE 42. **And the copy is checked, because copying is the step that can
  # silently not happen.** `scp` returning 0 says the transfer ran, not that the
  # bytes on the guest are this tree's -- a running `.exe` cannot be overwritten
  # on Windows, so a compositor somebody else left running makes `scp` succeed
  # and leaves the old file in place. That is the single most expensive failure
  # here because its symptom is a *behaviour change*, and on the GPU path it
  # presents as a transport fault: `finish_attach` refuses a wire-version skew
  # and says "This is a version skew between two builds, not a transport fault."
  # `WIRE_VERSION` went 1 -> 2 while this stage was being written, so the
  # scenario is current rather than hypothetical.
  #
  # Byte identity rather than a version comparison, and that is not a compromise
  # -- `WIRE_VERSION` is a `pub const u32` the compiler inlines, no binary
  # carries it as data, and nothing prints it, so there is nothing to read. The
  # hash is strictly stronger: it catches a version skew, a protocol-hash skew,
  # and every other difference, without needing any of them to be observable.
  deployed_all "$stage" "$DESKDIR" g "re-run $0 deploy with nothing of ours running on the guest" \
    "wc-desk.exe" "$COMP_EXE" || exit 1
  if [ ! -f "$stage/probe-relay.exe" ]; then
    g "test -f $DESKDIR/probe-relay.exe" >/dev/null 2>&1 \
      && echo "  probe-relay.exe: kept the one already there" \
      || echo "  WARNING: no probe-relay.exe and no mingw compiler to build one"
  fi
  rm -rf "$stage"
  mkdir -p "$OUT"
}

# --------------------------------------------------------------- the run -----
# One pass. `$1` is "icons" or "generic"; everything else is identical, which is
# the point of the pair.
one_run() {
  local mode="$1" tag="$2"
  local sock="wg-s11-$$-$mode"
  local iconflag=""
  [ "$mode" = generic ] && iconflag=" --no-icons"

  echo
  echo "== $tag =="

  # Stale compositors first, then the wipe: §13.9's fourth lesson, which cost
  # two misdiagnoses there.
  kill_ours "wg-s11" >/dev/null 2>&1
  g "rm -f $APPDATA_WIN/wg-s11-* 2>/dev/null; true" >/dev/null 2>&1
  sleep 2

  # STAGE 41. The substrate, declared before anything is measured on it. The
  # `guest-ram` arm unsets `SV_ALLOW_FILE_ARENA`, so if anything below still
  # reaches for a file the binaries refuse it by name rather than serving it.
  wc_substrate_declare guest-ram || return
  # §51. And that a hypervisor is in the path, which is what makes the arena
  # above locatable at all -- `wc_assert_topology` proves the one from the other.
  wc_topology_declare cross-vm \
    "the compositor runs on the guest; this harness drives it from the VM host over an arena in the guest's own RAM" || return

  # And the export loses the two things it used to hold for this harness: the
  # arena, and the keymap directory. Removed rather than left and trusted not to
  # be used -- a leftover `arena` is openable, the right size, and completely
  # silent to anything that was not looking. §18's `purge_arena_file`.
  h "mkdir -p $HOST_DIR" >/dev/null
  wc_purge_arena_file "$SHARE"

  # The desktop entry, written here because weston ships none. Three lines, and
  # the icon it names is one the host's own theme holds. With `WC_ICON_NAME=`
  # empty nothing is written at all and the app id has to resolve against the
  # machine's own data — which is the Firefox run, where it does.
  h "rm -rf $HOST_DIR/data; mkdir -p $HOST_DIR/data/applications" >/dev/null
  if [ -n "$ICON_NAME" ]; then
    h "cat > $HOST_DIR/data/applications/$APP_ID.desktop <<EOD
[Desktop Entry]
Type=Application
Name=Wayland Terminal
Icon=$ICON_NAME
EOD" >/dev/null
  else
    note "no desktop entry written: $APP_ID must resolve against $HOST_ICONS itself"
  fi

  # `wc-proxy` and, new in §41, `wc-arena-host`. The locator is **not a
  # workspace member** — it links shared_vulkan's `sv-hostd` — so it is built
  # through its own manifest, exactly as `wc-desktop.sh` does it.
  h "pkill -f '^$HOST_DIR/wc-proxy' 2>/dev/null; sleep 1; true" >/dev/null 2>&1
  local locator="$LINUX_BIN/wc-arena-host"
  [ -x "$locator" ] || locator="$ROOT/crates/wc-arena-host/target/debug/wc-arena-host"
  if ! cargo build --manifest-path "$ROOT/crates/wc-arena-host/Cargo.toml" \
        >"$RUNDIR/loc-$mode.log" 2>&1; then
    bad "could not build wc-arena-host"; tail -10 "$RUNDIR/loc-$mode.log" | sed 's/^/        /'; return
  fi
  scp "${SSHOPT[@]}" "$LINUX_BIN/wc-proxy" "$locator" "$HOST:$HOST_DIR/" >/dev/null || {
    bad "could not copy wc-proxy/wc-arena-host to $HOST -- something is still holding one"; return; }
  h "chmod +x $HOST_DIR/wc-proxy $HOST_DIR/wc-arena-host" >/dev/null

  # STAGE 41. No `--shared-dir`, and its absence is the point rather than an
  # omission. What used to be here was:
  #
  #   keymap_extra="--shared-dir $SHARE_WIN\keymaps --shared-peer-prefix $SHARE/keymaps/"
  #
  # because stage 6's `wl_keyboard.keymap` is an `h` in the server-to-client
  # direction and a `Local\wc-…` section is unopenable by a Linux process
  # (§11.2, §13.10). With a guest-RAM arena the compositor cuts the keymap out
  # of its own outbound window instead (§14), so nothing is published on any
  # filesystem and the export is off this run entirely -- which is what makes
  # the assertion at the bottom of this function able to demand zero shared
  # files rather than merely few.
  #
  # The frame list is the one file that still crosses, and it is not the arena:
  # it says *where the guest's pages are*, not what is in them. Written to the
  # guest's own directory and fetched over ssh, so this harness needs no export
  # at all now.
  local gframes_win="$DESKDIR_WIN\\arena-$mode.frames"
  local gframes="$DESKDIR/arena-$mode.frames"
  g "rm -f $gframes" >/dev/null 2>&1

  # A task name per pass. Registering the same name twice while the first
  # instance is still winding down leaves `Start-ScheduledTask` with nothing to
  # start and no error, which showed up as "the compositor did not start" for
  # the second pass only — with the compositor's own log on the guest proving it
  # had started. One name each is a line; diagnosing it twice was not.
  int_bg "wciconcomp-$mode" "$DESKDIR_WIN\\$COMP_EXE" \
    "--socket $sock --timeout 320 --log $DESKDIR_WIN\\comp-$mode.log $(wc_arena_comp_args "$gframes_win")" \
    >"$RUNDIR/start-$mode.log" 2>&1
  sleep 6
  # Polled rather than slept for: a scheduled task's process appears when the
  # scheduler gets round to it, which on a guest with six other compositors on
  # it is several seconds after `Start-ScheduledTask` returns.
  local CPID="" t
  for t in 1 2 3 4 5 6 7 8; do
    CPID=$(comp_pid "$sock")
    [ -n "$CPID" ] && break
    sleep 4
  done
  if [ -z "$CPID" ]; then
    bad "the compositor did not start on the interactive desktop"
    sed 's/^/        /' "$RUNDIR/start-$mode.log" | head -10
    g "cat $DESKDIR/comp-$mode.log" 2>&1 | tail -5 | sed 's/^/        /'
    return
  fi
  note "compositor pid $CPID (socket $sock)"

  # The taskbar before this run's window exists: the same instrument, the same
  # session, one window later. Photographed per run so that the pair being
  # compared is minutes apart rather than stages apart.
  int wciconshot "cmd.exe" \
    "/c $DESKDIR_WIN\\wc-desk.exe shot --class Shell_TrayWnd --out $DESKDIR_WIN\\tray0.bmp > $DESKDIR_WIN\\shot.txt 2>&1" \
    >/dev/null 2>&1
  fetch tray0.bmp "stage11-taskbar-before-$mode$SUFFIX.png" "the taskbar before the window ($mode)" tray

  local port=$PORT
  [ "$mode" = generic ] && port=$((PORT + 1))
  ssh "${SSHOPT[@]}" "$GUEST" \
    "export MSYS_NO_PATHCONV=1; cd $GUESTNAME && ./probe-relay.exe $port '$APPDATA_WIN\\$sock' --timeout 280" \
    >"$RUNDIR/relay-$mode.log" 2>&1 &
  local relay=$!
  sleep 3

  # STAGE 41. THE LOCATE, and it has to be exactly here: after the compositor
  # has committed and pinned the region inside the VM, and before the proxy that
  # will map it starts. That ordering is the whole difference between this
  # harness and the one it replaces, which had nothing to locate because its
  # arena was a file somebody truncated in advance.
  local plan="$HOST_DIR/$sock.plan"
  if ! wc_locate_arena "$DESKDIR/comp-$mode.log" "$gframes" "$plan" "$RUNDIR"; then
    kill $relay 2>/dev/null
    kill_ours "$sock" >/dev/null 2>&1
    return
  fi

  # Everything below runs on WC_HOST, which is the machine with the icon theme
  # on it and the machine the VM runs on. Written to a file and invoked: every
  # background process on the far side must have its stdout redirected or the
  # outer ssh never returns (§13.10).
  #
  # STAGE 41: **the proxy and the client are now root, in a transient unit.**
  # The proxy has to open `/proc/<qemu-pid>/fd/<n>` to reach the guest's pages
  # and QEMU is a system service on this host (§12.9), so the whole host side
  # moves under `systemd-run --collect`. This is `wc-desktop.sh`'s arrangement
  # and its two lessons come with it: a root call that blocks for minutes often
  # returns with no output at all, so completion is signalled by a *file* rather
  # than by an exit code; and the `ssh -L` forward stays with the invoking user,
  # outside the unit, because root has no `$HOME/.ssh/wc_guest` and can open a
  # user-owned socket node anyway.
  #
  # `XDG_DATA_DIRS` moves inside the unit with the client, because the icon
  # theme is resolved by the *proxy* and the proxy is now the root process.
  h "cat > $HOST_DIR/s11-$mode.sh <<'EOS'
#!/usr/bin/env bash
set -uo pipefail
RUN=\$(mktemp -d /tmp/wc-s11-XXXXXX); chmod 755 \"\$RUN\"
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
export XDG_DATA_DIRS=__DATA__
$HOST_DIR/wc-proxy --upstream \"\$RUN/up.sock\" --socket wl-proxy \
   --runtime-dir \"\$RUN\" --transport arena --arena-plan $plan --arena-id 0 \
   --copy damage --accept-timeout 25 --timeout 240$iconflag \
   >\"\$RUN/proxy.log\" 2>&1 &
PROXY=\$!
sleep 2
WAYLAND_DISPLAY=wl-proxy ${CLIENT_CMD:-$HOST_WESTON/$CLIENT} >\"\$RUN/cli.log\" 2>&1 &
CLI=\$!
# LONG, AND MEASURED RATHER THAN GENEROUS. Every photograph below is taken by
# a *scheduled task*, and registering, starting, waiting for and unregistering
# one costs the better part of a minute — so the first version of this held
# the client for 60 seconds, took the window's picture inside that, and took
# the taskbar's two tasks later, by which time the window had been gone for a
# minute. The taskbar came out with the right number of buttons and none of
# them ours, which reads exactly like \"Windows refused the icon\".
sleep 210
kill \$CLI \$PROXY 2>/dev/null; wait 2>/dev/null
chmod -R a+rwX \"\$RUN\" 2>/dev/null
touch \"\$RUN/done\"
INNER
  sed -i \"s#__RUN__#\$RUN#; s#__DATA__#$HOST_DIR/data:$HOST_ICONS#\" \"\$RUN/root.sh\"
  chmod 755 \"\$RUN/root.sh\"
  UNIT=wc-s11-$mode-\$\$
  echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
systemd-run --unit=\$UNIT --collect /run/current-system/sw/bin/bash \$RUN/root.sh >/dev/null 2>&1
echo started\" | eval $ROOTC >/dev/null 2>&1
  echo S11-CLIENT-STARTED
  for _ in \$(seq 1 260); do [ -f \"\$RUN/done\" ] && break; sleep 1; done
  [ -f \"\$RUN/done\" ] || echo \"S11-FAIL: the host-side unit never finished\"
else
  echo S11-FAIL: no forward socket
fi
kill \$FWD 2>/dev/null
sleep 1
echo S11-PROXY-BEGIN; cat \"\$RUN/proxy.log\" 2>/dev/null; echo S11-PROXY-END
echo S11-CLIENT-BEGIN; tail -20 \"\$RUN/cli.log\" 2>/dev/null; echo S11-CLIENT-END
rm -rf \"\$RUN\"
EOS
chmod +x $HOST_DIR/s11-$mode.sh" >/dev/null
  h "$HOST_DIR/s11-$mode.sh" >"$RUNDIR/host-$mode.log" 2>&1 &
  local drv=$!

  # Up to three attempts, because the window may not be there.
  #
  # Not flakiness-papering: another worktree is driving this guest, and an older
  # harness's `taskkill /IM wc-compositor.exe` — without `/F` — sends **WM_CLOSE
  # to every window of every process with that image name**, ours included. The
  # compositor then does exactly what it should (`xdg_toplevel.close`), the
  # client exits politely, and the window is gone. It is in the compositor's own
  # log as "window 1 close requested" with nothing on this side having asked for
  # it, and the symptom is a photograph of a taskbar with no button on it.
  local attempt
  for attempt in 1 2 3; do
    sleep 12
    # Wipe first. A shot of a window that is not there leaves the *previous*
    # attempt's file in place, and fetching that is how a run reports a picture
    # of a window it did not photograph — which happened once, and the two
    # passes' images were byte-identical.
    g "rm -f $DESKDIR/win.bmp $DESKDIR/tray.bmp $DESKDIR/windows.txt $DESKDIR/shot.txt" \
      >/dev/null 2>&1
    # One task, two pictures and the window list, so that everything below is
    # about the same instant. Separate tasks cost about a minute each — see the
    # note on the client's hold above, which is the bug that taught this.
    int "wciconshot-$mode" "cmd.exe" \
      "/c $DESKDIR_WIN\\wc-desk.exe windows > $DESKDIR_WIN\\windows.txt 2>&1 & \
$DESKDIR_WIN\\wc-desk.exe shot --class Shell_TrayWnd --out $DESKDIR_WIN\\tray.bmp > $DESKDIR_WIN\\shot.txt 2>&1 & \
$DESKDIR_WIN\\wc-desk.exe shot --pid $CPID --class WaycomposeToplevel --out $DESKDIR_WIN\\win.bmp >> $DESKDIR_WIN\\shot.txt 2>&1" \
      >/dev/null 2>&1
    g "cat $DESKDIR/windows.txt" >"$RUNDIR/windows-$mode.txt" 2>&1
    g "cat $DESKDIR/shot.txt" >"$RUNDIR/shot-$mode.txt" 2>&1
    grep -q WaycomposeToplevel "$RUNDIR/windows-$mode.txt" && break
    note "attempt $attempt: no compositor window on the desktop yet"
  done
  cp "$RUNDIR/windows-$mode.txt" "$OUT/windows-$mode$SUFFIX.txt" 2>/dev/null

  WINDOW_OK=""
  grep -q WaycomposeToplevel "$RUNDIR/windows-$mode.txt" && WINDOW_OK=yes
  fetch win.bmp  "stage11-window-$mode$SUFFIX.png"  "the window, whose caption carries ICON_SMALL ($mode)" window
  fetch tray.bmp "stage11-taskbar-$mode$SUFFIX.png" "the taskbar with the window's button ($mode)" tray

  wait $drv 2>/dev/null
  kill $relay 2>/dev/null; wait $relay 2>/dev/null
  g "cat $DESKDIR/comp-$mode.log" >"$RUNDIR/comp-$mode.log" 2>&1
  sed -n '/S11-PROXY-BEGIN/,/S11-PROXY-END/p' "$RUNDIR/host-$mode.log" >"$RUNDIR/proxy-$mode.log"
  cp "$RUNDIR/comp-$mode.log" "$OUT/compositor-$mode$SUFFIX.log" 2>/dev/null
  grep -v "^S11-" "$RUNDIR/proxy-$mode.log" >"$OUT/proxy-$mode$SUFFIX.log" 2>/dev/null

  echo "        --- the proxy's account:"
  grep -iE "icon|transport|stopped" "$RUNDIR/proxy-$mode.log" | sed 's/^/        /' | head -12
  echo "        --- the compositor's account:"
  grep -iE "icon|app_id" "$RUNDIR/comp-$mode.log" | sed 's/^/        /' | head -12
  # Why the window went away, when it did. A client that closes politely and a
  # window somebody else closed look identical from outside; these two lines are
  # the only place the difference shows.
  grep -iE "close requested|toplevel destroyed" "$RUNDIR/comp-$mode.log" \
    | sed 's/^/        /' | head -4
  sed -n '/S11-CLIENT-BEGIN/,/S11-CLIENT-END/p' "$RUNDIR/host-$mode.log" \
    | grep -v "^S11-" | sed 's/^/        client: /' | head -8

  # ---- the claims ----------------------------------------------------------
  # STAGE 41, AND FIRST, BECAUSE EVERY CLAIM BELOW IS ABOUT SOMETHING THIS ONE
  # NAMES THE SUBSTRATE OF. An icon on a taskbar is an icon on a taskbar
  # whatever carried the pixels — which is exactly why a run has to say what
  # carried them, or four stages of photographs turn out to have been taken on
  # the fixture and nobody notices. This is an assertion rather than the `note`
  # this line used to be: §35.
  wc_assert_substrate "$RUNDIR/comp-$mode.log" "$mode"

  # The *last* handle in `shot.txt`, because the window's picture is taken
  # after the taskbar's and taking the first one identifies `Shell_TrayWnd`.
  # Addressed this way rather than by class for §13.9's reason: another
  # worktree's compositor registers the same class, and its window was in this
  # very run's enumeration.
  local line hw
  hw=$(grep -oE "hwnd 0x[0-9a-f]+" "$RUNDIR/shot-$mode.txt" 2>/dev/null | tail -1 | sed 's/hwnd //')
  line=$(grep -F "$hw " "$RUNDIR/windows-$mode.txt" 2>/dev/null | grep WaycomposeToplevel)
  if [ -n "$line" ]; then
    echo "$line" | grep -qE '^0x[0-9a-f]+ +yes' \
      && ok "$mode: the window is a taskbar entry" \
      || bad "$mode: the window is not a taskbar entry"
  else
    bad "$mode: no compositor window on the desktop"
  fi

  if [ "$mode" = icons ]; then
    grep -qE "icon: app id .*-> [0-9]+ image" "$RUNDIR/proxy-$mode.log" \
      && ok "the proxy resolved the app id to real theme files on the host" \
      || bad "the proxy resolved nothing: $(grep -c icon "$RUNDIR/proxy-$mode.log") icon lines"
    grep -q "icon published for" "$RUNDIR/comp-$mode.log" \
      && ok "the pixels crossed and the compositor stored them" \
      || bad "no icon reached the compositor"
    grep -q "icon set: 2 of 2 slots" "$RUNDIR/comp-$mode.log" \
      && ok "WM_SETICON was sent for both ICON_BIG and ICON_SMALL  <-- goal-firefox.md, clause 1" \
      || bad "the icon did not reach both slots: $(grep -c 'icon ' "$RUNDIR/comp-$mode.log") icon lines"
  else
    grep -q "has no published icon" "$RUNDIR/comp-$mode.log" \
      && ok "the control run degraded to the generic icon and said so" \
      || bad "the control run did not report a missing icon"
    # Not the compositor's closing summary: it is still running when this log is
    # read, and a check that depends on a process having exited is a check that
    # fails for a reason that has nothing to do with what it is checking.
    grep -q "icon published for" "$RUNDIR/comp-$mode.log" \
      && bad "--no-icons published something" \
      || ok "and nothing was published at all, which is what --no-icons means"
  fi
  kill_ours "$sock" >/dev/null 2>&1
}

# Is the icon actually *in* the picture? Measured, not looked at.
#
# `compare -subimage-search` finds the best position of the reference icon
# inside the capture and reports the distance there. The reference is the very
# PNG the host resolved, fetched from the host; if the pixels that ended up on
# the guest's screen are those pixels, this finds them and says where.
do_compare() {
  echo
  echo "== is the host's icon in the guest's picture? =="
  # With no desktop entry of our own the icon's name is the app id's own
  # business, and the app id is the best guess at it — which is exactly what
  # the resolver falls back to.
  local name="${ICON_NAME:-$APP_ID}"
  local ref="$RUNDIR/ref16.png"
  scp "${SSHOPT[@]}" "$HOST:$HOST_ICONS/icons/hicolor/16x16/apps/$name.png" "$ref" \
    >/dev/null 2>&1
  if [ ! -s "$ref" ]; then
    miss "no 16x16 reference icon for $name on the host"
    return
  fi
  cp "$ref" "$OUT/reference-16-$name.png" 2>/dev/null

  local target
  for target in "$OUT/stage11-window-icons$SUFFIX.png" "$OUT/stage11-window-generic$SUFFIX.png"; do
    [ -s "$target" ] || continue
    # The caption is the top 40 rows; ICON_SMALL is drawn in it at 16x16 on a
    # 96 dpi display. Searching a strip rather than the window keeps the search
    # cheap and keeps a coincidence in the client's own pixels out of it.
    magick "$target" -crop "200x40+0+0" +repage "$RUNDIR/strip.png" 2>/dev/null || continue
    # **Flatten the reference onto the caption's own background first.** A theme
    # icon is transparent around its edges and the thing on screen is that icon
    # composited over the caption, so comparing the two directly compares
    # "transparent" against "light grey" over most of the icon's area and finds
    # nothing. That cost a round and reported as "Windows ignored the icon".
    local bg
    bg=$(magick "$RUNDIR/strip.png" -format "%[pixel:p{60,4}]" info: 2>/dev/null)
    magick "$ref" -background "${bg:-white}" -alpha remove -alpha off "$RUNDIR/ref-flat.png" \
      2>/dev/null || cp "$ref" "$RUNDIR/ref-flat.png"
    local out rmse at
    out=$(magick compare -metric RMSE -subimage-search "$RUNDIR/strip.png" "$RUNDIR/ref-flat.png" null: 2>&1)
    rmse=$(echo "$out" | awk '{print $1}')
    at=$(echo "$out" | grep -oE "@ [0-9]+,[0-9]+" | head -1)
    note "$(basename "$target"): best match for ${name}16 is ${rmse:-?} $at"
    case "$target" in
      *icons*) icons_rmse="$rmse" ;;
      *generic*) generic_rmse="$rmse" ;;
    esac
  done
  # STAGE 42. **This block prints "is the host's icon in the guest's picture?"
  # and could not answer no.** The failing arm was a `note`, so the one check
  # that the resolved icon actually reached the guest's screen — the claim §16.9
  # calls "measured, not looked at" — reported nothing whenever it failed. Both
  # arms are verdicts now.
  #
  # The `-z` guard around them was the second half of the same problem: if
  # either search produced no number the whole comparison was skipped in
  # silence, which is the state a missing or unreadable photograph produces.
  # That is now a `bad` too, because "I could not measure it" and "it passed"
  # must not be the same output.
  if [ -n "${icons_rmse:-}" ] && [ -n "${generic_rmse:-}" ]; then
    if awk -v a="$icons_rmse" -v b="$generic_rmse" 'BEGIN { exit !(a + 0 < b / 2) }'; then
      ok "the icon run's caption contains the host's icon and the control run's does not \
($icons_rmse against $generic_rmse)"
    else
      bad "the two captions are not distinguishable by this search ($icons_rmse vs $generic_rmse): \
the icon run's caption should match the reference at least twice as well as the control's"
    fi
  else
    bad "the icon comparison produced no numbers (icons=${icons_rmse:-none} generic=${generic_rmse:-none}) \
-- with no measurement there is no result, and this is the check that the pixels reached the screen"
  fi
}

# One pass, up to three times, because the window may not survive to be
# photographed and the reason is not this program's.
#
# **Something on the guest sends our window a `WM_CLOSE`.** The compositor logs
# it — "window 1 close requested" — with nothing on this side having asked; the
# client then does the correct thing and exits, and the run has a compositor, a
# taskbar and no window. It is not the image-name `taskkill` this stage already
# defended against (that was ruled out by renaming the executable, and it
# happened again afterwards), and it is not reproducible on demand: the control
# pass survives its whole hold and the icon pass has died five frames in, more
# than once, on the same guest in the same minute.
#
# So this retries rather than pretends, and the counters are rolled back
# between attempts so that a run that eventually succeeds does not report the
# failures it walked through. What is *not* rolled back is the note saying it
# happened, because that is the honest part.
retried() {
  local mode="$1" tag="$2" try p0 f0 s0
  for try in 1 2 3; do
    p0=$pass; f0=$fail; s0=$skip
    one_run "$mode" "$tag${try:+ (attempt $try)}"
    [ -n "$WINDOW_OK" ] && return 0
    note "attempt $try of $mode produced no window to photograph; retrying"
    [ "$try" = 3 ] || { pass=$p0; fail=$f0; skip=$s0; }
  done
  return 1
}

do_taskbar() {
  if [ -z "$HOST" ] || { [ -z "$HOST_WESTON" ] && [ -z "$CLIENT_CMD" ]; }; then
    miss "WC_HOST and one of WC_HOST_WESTON / WC_CLIENT_CMD unset: no client, no picture"
    return
  fi
  # `WC_ONLY=icons` re-takes one half without disturbing the other's pictures,
  # which is what a run whose client died before the photograph needs.
  case "${WC_ONLY:-both}" in
    icons)   retried icons   "1. the icon, resolved on the host and set on the guest" ;;
    generic) retried generic "2. the control: the same run with --no-icons" ;;
    *)
      retried icons   "1. the icon, resolved on the host and set on the guest"
      retried generic "2. the control: the same run with --no-icons"
      ;;
  esac
  do_compare
  do_leftovers
}

# Did this run actually leave the guest as it found it?
#
# **Counted, not assumed.** A scheduled task's process does not die with the ssh
# session that started it (§13.3), so every path out of a harness that is not an
# explicit kill leaves a compositor behind — and a run that reaches its cleanup
# block has not thereby established that the cleanup worked. Twice now a stage
# has reported a clean guest that was not one; the second time it was holding six
# abandoned compositors seven hours old, each with a window on the desktop
# (§16.8). The difference between the two claims is one `Get-CimInstance`.
do_leftovers() {
  echo
  echo "== what is left on the guest =="
  local n
  n=$(g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
        -File '$DESKDIR_WIN\\killours.ps1' -Match '$GUESTNAME' -PidOnly" 2>/dev/null \
      | tr -d '\r' | grep -c '[0-9]')
  if [ "${n:-0}" -eq 0 ]; then
    ok "no $COMP_EXE from $GUESTNAME is still running"
  else
    bad "$n $COMP_EXE process(es) from $GUESTNAME left behind; kill them by pid"
  fi
}

case "$MODE" in
  deploy)  do_deploy ;;
  taskbar) do_taskbar ;;
  all)     do_deploy; do_taskbar ;;
  *) echo "usage: $0 {deploy|taskbar|all}"; exit 2 ;;
esac

echo
echo "== $pass passed, $fail failed, $skip skipped =="
[ "$fail" -eq 0 ]
