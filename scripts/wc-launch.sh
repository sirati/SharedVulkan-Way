#!/usr/bin/env bash
# Stage 14. Build, install and *measure* the desktop shortcut that runs Linux
# Firefox on the Windows guest.
#
#   scripts/wc-launch.sh deploy    cross-compile, copy both halves, write the ini
#   scripts/wc-launch.sh install   put the shortcut on the guest's real desktop
#   scripts/wc-launch.sh taskbar-plan       what deploy would decide for each
#                                          application's taskbar identity (§92.12)
#   scripts/wc-launch.sh uninstall-taskbar  this installation's jump lists, shortcuts
#                                          and relaunch files, never another's
#   scripts/wc-launch.sh cold      no session; click; time it
#   scripts/wc-launch.sh warm      session already up; click; time it
#   scripts/wc-launch.sh native    the same measurement for Windows' own Firefox
#   scripts/wc-launch.sh shot      photograph the window that is up
#   scripts/wc-launch.sh stop      end the session
#   scripts/wc-launch.sh broker-show / broker-uninstall   the §111.8 broker service
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
# §91: **and with the token a double-click gets.** The task's run level was
# `Highest` until §91, so every click here ran with the administrator's full
# token, and §90.3 is what that hid: a real double-click runs at medium
# integrity and could not open the virtio-serial doorbells. The click now runs
# at `Limited`, the filtered medium token Explorer uses, and the launcher's
# `token:` line is checked for it. `WC_LAUNCH_CLICK=elevated` is the old route.
#
# The scheduler costs about a minute to start a task (§16.8). That minute is
# *before* wc-launch's process exists and is therefore outside every number
# below, which is the reason the clock is inside the launcher rather than here.
. "$(dirname "${BASH_SOURCE[0]}")/site.sh"
set -uo pipefail

MODE="${1:-all}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
# §105.13: release. The launcher parses every answer of the control ring and
# the compositor copies it; a debug build was most of a 20 KB answer's 0.2 ms.
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/release"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"

GUEST="${WC_GUEST:-$SITE_GUEST}"
# Exported as resolved: `wc_topology_declare cross-vm` (wc-topology.sh) checks
# WC_GUEST, and a default that lived only in GUEST made it refuse cross-vm
# unless the caller had happened to set the variable this line defaults.
export WC_GUEST="$GUEST"
HOST="${WC_HOST:-$SITE_HOST_USER@$SITE_HOST_ALIAS}"
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"

# This stage's own directory on the shared guest, and its own names for
# everything. §16.8: an identity is a path, never an image name.
# `WC_LAUNCH_DIRNAME` so a run can keep out of another's directory on the
# shared guest; the guest's key for the VM host lives there too, so a second
# directory is a second key with its own forced command.
DIRNAME="${WC_LAUNCH_DIRNAME:-wclaunch}"
DIR_WIN="$SITE_GUEST_WIN_HOME\\$DIRNAME"
DIR_MSYS="$SITE_GUEST_MSYS_HOME/$DIRNAME"
SESSION="${WC_LAUNCH_SESSION:-wcl}"
# The shortcut's title, which is also its file name on the desktop.
TITLE="${WC_LAUNCH_TITLE:-Firefox (Linux)}"
# Where `shot` puts its photographs, under docs/, and their file-name prefix.
SHOTDIR="${WC_LAUNCH_SHOTDIR:-stage14}"
APPDATA_MSYS="$SITE_GUEST_MSYS_HOME/AppData/Local/waycompose"

# The VM host's half.
HOSTDIR="${WC_LAUNCH_HOSTDIR:-${SITE_HOST_HOME}/wc-launch}"
# §111.10: where the host keeps its installations.
#   user    the admin's home and its user units (wc-bell-hub, wc-sessiond), as
#           before §111. Refused on a host that runs the system services.
#   system  the host runs them as system services (`services.waycompose`):
#           deploy builds the installation in a staging directory in the admin's
#           home and installs it, as root (WC_LAUNCH_HOST_ROOT), into
#           /var/lib/waycompose/installs/<name> with scripts/wc-users-migrate.sh
#           install-staged. No user unit is written; the services' own binaries
#           are left alone (WC_LAUNCH_SYSTEM_BINS=install replaces them). The
#           session runs as the Linux user /etc/waycompose/users.conf maps it to:
#           a test installation is named in the host's `wctestInstalls` and runs
#           as `wctest`. apps.ini's paths under the admin's home become `~/`,
#           the mapped user's.
LAYOUT="${WC_LAUNCH_HOST_LAYOUT:-user}"
HOST_ROOT="${WC_LAUNCH_HOST_ROOT:-sudo -n}"
SYSTEM_BASE=/var/lib/waycompose/installs
if [ "$LAYOUT" = system ]; then
  HOSTDIR="${WC_LAUNCH_HOSTDIR_STAGE:-${SITE_HOST_HOME}/.cache/waycompose-stage/$SESSION}"
fi
# The guest reaches the host over the host-only virtio-net link and nothing
# else. No developer workstation is in this path.
HOSTLINK="${WC_LAUNCH_HOSTLINK:-$SITE_HOST_USER@$SITE_HOST_NAT_IP}"
# A NAME or an absolute path on the VM host. A name is resolved there at deploy
# time (`wc-session resolve`, the store search of `resolve_exec`), and the path
# goes into the host's apps.ini: since §94 nothing is resolved by name when the
# guest asks for an application. The brief named
# `/nix/store/9ndgaj…-system-path/bin/firefox` and said not to ship that string;
# four hours later the current system path had no `bin/firefox` in it at all,
# so neither that path nor `command -v firefox` nor
# `/run/current-system/sw/bin/firefox` is an answer on this machine. A store
# path that a name resolves to is GC-rooted at $HOSTDIR/bin/app-firefox.
FIREFOX="${WC_LAUNCH_FIREFOX:-firefox}"
# The profile, which must be under the VM host's ~/.wc-launch for `run` to write
# its user.js; a second installation that may run beside the first needs its own.
# WC_LAUNCH_PROFILE: its first word is the profile and any further words are the
# application's fixed arguments (apps.ini `args`).
NATIVE_FIREFOX='C:\Program Files\Mozilla Firefox\firefox.exe'
# §96. Applications beside Firefox: WC_LAUNCH_EXTRA_APPS is a file of apps.ini
# sections for the host (scripts/wc-session documents the keys; every `exec`
# must already be on the host), WC_LAUNCH_EXTRA_APP_BLOCKS a file of `[app.ID]`
# blocks for the guest's ini, and WC_LAUNCH_INSTALL_APPS the ids `install` puts
# on the desktop (default: firefox). docs/minecraft.md has Minecraft's.

ARENA_MIB="${WC_LAUNCH_ARENA_MIB:-64}"
OUTPUT="${WC_LAUNCH_OUTPUT:-1600x1000}"
PORT="${WC_LAUNCH_PORT:-57431}"
# §86. The session's sound. The PulseAudio package the VM host's `wc-session`
# runs per session, as a store path that must already be on the host (`nix copy`
# it there); deploy pins it with a GC root at $HOSTDIR/bin/pulse. Unset or absent,
# sessions have no audio server and Firefox gets a null audio context.
PULSE="${WC_LAUNCH_PULSE:-}"
# §92. sqlite3 (the jump list's frequent pages, read from the profile's
# places.sqlite) and rsvg-convert (Firefox's task glyphs), as store paths that
# must already be on the host; deploy pins each with a GC root under $HOSTDIR/bin.
# Unset, wc-session looks for them in the host's store, where a garbage
# collection can take them.
SQLITE="${WC_LAUNCH_SQLITE:-}"
RSVG="${WC_LAUNCH_RSVG:-}"
# §106. A PipeWire for the session (`WC_LAUNCH_AUDIO=pipewire` on the VM host):
# pipewire (with pipewire-pulse) and wireplumber, as store paths already on the
# host, GC-rooted at $HOSTDIR/bin/pipewire and bin/wireplumber, where
# wc-session looks first. Unset, wc-session searches the store.
PIPEWIRE="${WC_LAUNCH_PIPEWIRE:-}"
WIREPLUMBER="${WC_LAUNCH_WIREPLUMBER:-}"
# §106.3: wc-pw-node, the sound ring's native nodes under PipeWire (a pipe
# module there drops what the ring's credit holds back). A store path on the
# host, GC-rooted at $HOSTDIR/bin/wc-pw-node; unset and with WC_LAUNCH_PIPEWIRE
# set, deploy builds `.#wc-pw-node` here and `nix copy`s it to the host.
PW_NODE="${WC_LAUNCH_PW_NODE:-}"
AUDIO_VOLUME="${WC_LAUNCH_AUDIO_VOLUME:-100}"
# §86. The forwarded GPU, **off unless asked for**. WC_LAUNCH_GPU_SESSIONS=N
# (1..8) builds the GPU half into sv-guest.exe (release), copies sv-hostd and the
# ICD to the host, and writes `gpu-sessions = N` into the ini. gpu.env itself is
# written on the host only with WC_LAUNCH_GPU_ENV=write; otherwise deploy prints
# what it would contain. Since §87 nothing on either machine runs as root for
# it: the guest's VM needs the GPU arena's ivshmem device (slot WC_LAUNCH_GPU_SLOT,
# default 0x1d) and, for the doorbell, the port WC_LAUNCH_GPU_BELL; the host user
# needs the device's group.
# §97. WC_LAUNCH_GPU_ARENA=pool (the default) has the GPU process lease every
# session's memory from one pool over the whole device; =slots is §87's fixed
# slots. With the pool, WC_LAUNCH_GPU_SESSIONS bounds how many run at once.
# §91. **Which token a click runs with, and the default is a double-click's.**
# Explorer starts a shortcut with the user's filtered token at medium integrity;
# until §91 every harness click ran at `-RunLevel Highest`, the administrator's
# full token, which reaches devices a real double-click cannot (§90.3: the
# virtio-serial doorbells). `medium` (the default) is the scheduler's `Limited`
# run level on the interactive session, which is that filtered token;
# `elevated` is the old route, kept for comparing the two.
CLICK_LEVEL="${WC_LAUNCH_CLICK:-medium}"
case "$CLICK_LEVEL" in
  medium) CLICK_RUNLEVEL=Limited ;;
  elevated) CLICK_RUNLEVEL=Highest ;;
  *) echo "WC_LAUNCH_CLICK must be medium or elevated, not $CLICK_LEVEL" >&2; exit 2 ;;
esac
# WHAT A DEPLOY MAY CHANGE BEYOND ITS OWN INSTALLATION. Each of these is
# guest-wide or host-wide, shared by every installation, and so only done
# when asked for by name (`install`); the default `skip`/`keep` leaves it as
# it is. A test installation (WC_LAUNCH_TEST_HOME, or a `test-home` file in
# its host directory) is refused any of them (`deploy_scope_check`):
#   WC_LAUNCH_BROKER=install        the guest's broker service, device ACLs, §91's task
#   WC_LAUNCH_PORT_ACCESS=install   §91's per-user grant of the doorbell ports, its task
#   WC_LAUNCH_SERVICES=install      the host's hub and session service (user layout)
#   WC_LAUNCH_SYSTEM_BINS=install   the system hub's and session service's binaries
# (2026-10-20: a test installation's deploy with the old default `install`
# put the broker on the deployment guest under production installations.)
#
# §91. `deploy` grants the guest user the two doorbell ports
# (`scripts/wc-port-access.ps1 install`), which is what lets a medium-integrity
# launch open them. `skip` leaves the guest's device security alone.
PORT_ACCESS="${WC_LAUNCH_PORT_ACCESS:-skip}"
# §111.8. `deploy` installs the broker service (`sv-guest broker`, LocalSystem,
# from Program Files) and gives it this installation's key; the broker opens
# the devices for the user's processes, so the per-user port grant above is
# superseded and its task removed. `skip` keeps §91's grant and the key in the
# user's install directory, as before §111.
BROKER="${WC_LAUNCH_BROKER:-skip}"
PORT_USER="${WC_LAUNCH_PORT_USER:-${SITE_GUEST_USER}}"
GPU_SESSIONS="${WC_LAUNCH_GPU_SESSIONS:-0}"
# §105: the pre-§105 transport (ssh exec, relay, tunnel) as an opt-in debug
# mode, off by default. With it the guest's ssh key and its forced command are
# installed as before; without it this installation's key line is removed.
DEBUG_SSH="${WC_LAUNCH_DEBUG_SSH_TRANSPORT:-0}"
# §95. Each session slot of the GPU device is 16 MiB of control and this much
# arena (`gpu-arena-mib`, the launcher's default 64). §95 measured Firefox
# with two windows at about 53 MiB; testhost keeps 3 x 64 until the arena is
# allocated dynamically.
GPU_ARENA_MIB="${WC_LAUNCH_GPU_ARENA_MIB:-64}"
GPU_ENV_MODE="${WC_LAUNCH_GPU_ENV:-print}"
# The Mesa the GPU sessions load (a store path on the host): Zink, its EGL
# and GBM, and since §112 its VA-API driver (nix/mesa-zink-va.nix).
# WC_LAUNCH_KOPPER_MESA is the older name of the same setting.
KOPPER_MESA="${WC_LAUNCH_GPU_MESA:-${WC_LAUNCH_KOPPER_MESA:-}}"
# §112: the guest GPU's PCI ids, VVVV:DDDD, which `sv-drm ns` gives the
# render node ($HOSTDIR/gpu-pci).
GPU_PCI="${WC_LAUNCH_GPU_PCI:-}"
# §104. WC_LAUNCH_DMABUF_NODE=/dev/dri/renderD128: the proxy offers
# zwp_linux_dmabuf_v1 on that node ($HOSTDIR/dmabuf-node, read by wc-session)
# — the nested KWin's userspace render node. WC_LAUNCH_ICD_STORE=1: the ICD
# goes into the host's Nix store (manifest beside it, a relative library
# path) and gpu.env names it there, because another user's session (the
# nested KWin's, <desktop-user>'s) only takes a GPU environment of store paths.
DMABUF_NODE="${WC_LAUNCH_DMABUF_NODE:-}"
ICD_STORE="${WC_LAUNCH_ICD_STORE:-}"
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
  # The first `N bytes at` is the arena's. §89 put the rings' own
  # `transport rings: N bytes at …` on the same line, and taking both made the
  # division below an arithmetic error that skipped this check (§90).
  len=$(echo "$line" | grep -oE '[0-9]+ bytes at' | head -1 | awk '{print $1}')
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
       -File '$DIR_WIN\\runint.ps1' -Exe '$exe' -Arguments '$argstr' -Name '$name' $wait -RunLevel ${INT_LEVEL:-Highest}"
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
  [string]$WorkDir = "C:\Users\@GUEST_USER@\wclaunch",
  [switch]$Wait,
  [int]$TimeoutSec = 180,
  # §91: Limited is the token Explorer gives a double-click (filtered, medium
  # integrity); Highest is the administrator's full token, which every click of
  # every harness had until §91 and which a real double-click never gets.
  [ValidateSet("Highest", "Limited")][string]$RunLevel = "Highest"
)
$ErrorActionPreference = "Stop"
$a = New-ScheduledTaskAction -Execute $Exe -Argument $Arguments -WorkingDirectory $WorkDir
# sshd sets USERDOMAIN=WORKGROUP, which resolves to no SID at all (§13).
$p = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) `
                                -LogonType Interactive -RunLevel $RunLevel
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
  site_fill "$1"
}

# The double-click. `Start-Process` on a `.lnk` is `ShellExecute` on a `.lnk`,
# which is exactly what Explorer does with one. The path is read from a file
# rather than passed as an argument because it contains a space and three layers
# of quoting stand between here and there.
write_click() { cat >"$1" <<'PS2'
# Generated by scripts/wc-launch.sh; do not edit on the guest.
$lnk = (Get-Content -Raw "C:\Users\@GUEST_USER@\wclaunch\lnk.txt").Trim()
Start-Process -FilePath $lnk
Write-Output ("clicked " + $lnk)
PS2
  site_fill "$1"
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
$a = (Get-Content -Raw "C:\Users\@GUEST_USER@\wclaunch\shotargs.txt").Trim().Split(" ")
Start-Process -FilePath "C:\Users\@GUEST_USER@\wclaunch\wc-desk.exe" -ArgumentList $a `
              -WindowStyle Hidden -Wait
PS4
  site_fill "$1"
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
$out = "C:\Users\@GUEST_USER@\wclaunch\native.txt"
Remove-Item $out -ErrorAction SilentlyContinue
function Ours($tag) {
  Get-CimInstance Win32_Process -Filter "Name = 'firefox.exe'" |
    Where-Object { $_.CommandLine -and $_.CommandLine.Contains($tag) }
}
for ($i = 0; $i -lt $Runs; $i++) {
  # A profile of its own, and a fresh one, so this measures Firefox starting
  # rather than Firefox asking an already-running Firefox to open a window --
  # which is what the Linux side is measured doing too.
  $prof = "C:\Users\@GUEST_USER@\wclaunch\ffprof$i"
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
  site_fill "$1"
}

# §99, §105. The two per-user services every installation shares on the VM host:
# the doorbell hub, and the session service that starts sessions from the
# shared memory (no ssh). Each runs its own copy of its binary, so that
# redeploying one installation never swaps a binary under another.
install_services() {
  # §111.10: never beside the system services.
  if host_system_services; then bad "refusing to install the admin's hub and session service: $HOST runs them as system services"; exit 2; fi
  # §99: THE SHARED DOORBELL HUB. One per user on the VM host, for every
  # installation: it, not a session, holds QEMU's two doorbell sockets.
  # Its own copy of sv-hostd, so that redeploying one installation never
  # swaps the binary under another's running hub. A changed binary or unit
  # restarts the hub only when no session runs (below); otherwise it takes
  # effect at the hub's next start.
  local hubbin=${SITE_HOST_HOME}/wc-bell/sv-hostd hubunit=.config/systemd/user/wc-bell-hub.service
  h "mkdir -p ${SITE_HOST_HOME}/wc-bell \$HOME/.config/systemd/user" >/dev/null
  local before; before=$(h "sha256sum $hubbin \$HOME/$hubunit 2>/dev/null | cut -c1-16 | tr -d '\n'")
  scp "${SSHOPT[@]}" "$REL_LINUX/sv-hostd" "$HOST:$hubbin.new" >/dev/null \
    && h "cmp -s $hubbin.new $hubbin && rm -f $hubbin.new || mv -f $hubbin.new $hubbin" >/dev/null \
    || { echo "error: scp of the doorbell hub to the VM host"; exit 1; }
  h "cat >\$HOME/$hubunit" <<UNIT
[Unit]
Description=waycompose shared doorbell hub (docs/waycompose-design.md §99)

[Service]
# §99.6: QEMU's chardev connections live in systemd's file-descriptor
# store, so restarting the hub never closes them and QEMU never sees a
# disconnect.
Type=notify
NotifyAccess=main
FileDescriptorStoreMax=8
FileDescriptorStorePreserve=yes
ExecStart=$hubbin bell-hub --port /run/windows-vm/wc-arena/svbell.sock=%t/wc-bell/sv.sock --port /run/windows-vm/wc-arena/wcbell.sock=%t/wc-bell/wl.sock,rings=/run/windows-vm/wc-arena/arena --drm-services /run/sv-drm/services.sock
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
UNIT
  local after; after=$(h "sha256sum $hubbin \$HOME/$hubunit 2>/dev/null | cut -c1-16 | tr -d '\n'")
  h "export XDG_RUNTIME_DIR=/run/user/\$(id -u); systemctl --user daemon-reload && systemctl --user enable wc-bell-hub.service >/dev/null 2>&1; systemctl --user start wc-bell-hub.service" >/dev/null 2>&1
  # A changed hub is restarted only while no session runs: clients since
  # §99.4 reconnect to a restarted hub, but a session started by an older
  # build does not, and would lose its bells until it ends.
  local live; live=$(h "XDG_RUNTIME_DIR=/run/user/\$(id -u) systemctl --user list-units --state=active --no-legend --plain 'wc-launch-*' 2>/dev/null | wc -l")
  local restarted=""
  if [ -n "$before" ] && [ "$before" != "$after" ] && [ "${live:-1}" = 0 ]; then
    h "export XDG_RUNTIME_DIR=/run/user/\$(id -u); systemctl --user restart wc-bell-hub.service" >/dev/null 2>&1 && restarted=1
  fi
  local hs; hs=$(h "sleep 1; export XDG_RUNTIME_DIR=/run/user/\$(id -u); systemctl --user is-active wc-bell-hub.service; ls /run/user/\$(id -u)/wc-bell/ 2>/dev/null | tr '\n' ' '")
  case "$hs" in
    active*sv.sock*wl.sock*)
      if [ -n "$restarted" ]; then
        note "the doorbell hub changed and was restarted (no session was running)"
      elif [ "$before" != "$after" ] && [ -n "$before" ]; then
        note "the doorbell hub changed and was NOT restarted: $live session unit(s) run; the new binary takes effect at its next start"
      fi
      ok "the shared doorbell hub runs for every installation ($hubbin, wc-bell-hub.service: $hs)" ;;
    *) bad "the shared doorbell hub is not running: $hs"; exit 1 ;;
  esac
  # §105: THE SESSION SERVICE. It reads the ivshmem header page for boot
  # records, checks each against the installation's key, and runs that
  # installation's wc-session (`up`, `proxy`, `down`). It adopts the sessions
  # it started across a restart, so a changed one is simply restarted.
  local sdbin=${SITE_HOST_HOME}/wc-bell/wc-arena-host sdunit=.config/systemd/user/wc-sessiond.service
  local sbefore; sbefore=$(h "sha256sum $sdbin \$HOME/$sdunit 2>/dev/null | cut -c1-16 | tr -d '\n'")
  scp "${SSHOPT[@]}" "$1" "$HOST:$sdbin.new" >/dev/null \
    && h "cmp -s $sdbin.new $sdbin && rm -f $sdbin.new || mv -f $sdbin.new $sdbin" >/dev/null \
    || { echo "error: scp of the session service to the VM host"; exit 1; }
  h "cat >\$HOME/$sdunit" <<UNIT
[Unit]
Description=waycompose session service: guest sessions from the shared memory, no ssh (docs/waycompose-design.md §105)
Wants=wc-bell-hub.service
After=wc-bell-hub.service

[Service]
Environment=PATH=/run/current-system/sw/bin:/run/wrappers/bin
ExecStart=$sdbin sessiond --ivshmem /run/windows-vm/wc-arena/arena --installs %h/.config/waycompose/installs --bell %t/wc-bell/wl.sock
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
UNIT
  local safter; safter=$(h "sha256sum $sdbin \$HOME/$sdunit 2>/dev/null | cut -c1-16 | tr -d '\n'")
  h "export XDG_RUNTIME_DIR=/run/user/\$(id -u); systemctl --user daemon-reload && systemctl --user enable wc-sessiond.service >/dev/null 2>&1; \
     if [ '$sbefore' != '$safter' ]; then systemctl --user restart wc-sessiond.service; else systemctl --user start wc-sessiond.service; fi" >/dev/null 2>&1
  local ss; ss=$(h "sleep 1; XDG_RUNTIME_DIR=/run/user/\$(id -u) systemctl --user is-active wc-sessiond.service")
  case "$ss" in
    active*) ok "the session service runs for every installation ($sdbin, wc-sessiond.service$([ "$sbefore" != "$safter" ] && echo ', restarted: changed'))" ;;
    *) bad "the session service is not running: $ss"; exit 1 ;;
  esac
}

# ---------------------------------------------------------------- deploy -----
# §92.12. Each application's taskbar identity: `Waycompose.install.<name>.<app>`
# for this installation alone, or the application-wide `Waycompose.app.<app>`
# where this is the installation whose pin (or, with no pin, whose desktop
# shortcut) already carries it, so the user's taskbar does not change.
# `wc-launch.exe --taskbar-ids` decides on the guest from the user's shortcuts
# and the claim files, keeps any earlier decision (wc-launch.ini.prev), writes
# `taskbar-id` into the ini, and gives this installation's own unpinned
# shortcuts its own identity where it does not keep the old one. A test
# installation (WC_LAUNCH_TEST_HOME set here, or a `test-home` file beside its
# wc-session on the VM host) and WC_LAUNCH_TASKBAR_ID=install always get their
# own. `dry` only prints the plan.
taskbar_ids() {
  local dry="${1:-}" flags="" why=""
  if [ -n "${WC_LAUNCH_TEST_HOME:-}" ]; then why="WC_LAUNCH_TEST_HOME is set"
  elif [ "${WC_LAUNCH_TASKBAR_ID:-auto}" = install ]; then why="WC_LAUNCH_TASKBAR_ID=install"
  elif h "test -s $HOSTDIR/test-home"; then why="$HOSTDIR/test-home exists"
  fi
  [ -n "$why" ] && flags="--test-install"
  [ "$dry" = dry ] && flags="$flags --dry-run"
  local before out
  # A first deploy has no log yet: the shell's error text is not a count.
  before=$(g "cat $APPDATA_MSYS/wc-launch-$SESSION.log 2>/dev/null | wc -l" | tr -d '\r ')
  [[ "$before" =~ ^[0-9]+$ ]] || before=0
  g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS && ./wc-launch.exe --session-name $SESSION --taskbar-ids $flags" >/dev/null 2>&1
  out=$(g "tail -n +$((${before:-0} + 1)) $APPDATA_MSYS/wc-launch-$SESSION.log" | tr -d '\r' | grep 'taskbar-id:' | sed 's/^\[[0-9]*\] //')
  [ -n "$why" ] && note "a test installation ($why): its applications get its own taskbar identity"
  printf '%s\n' "$out" | sed 's/^/        /'
  [ "$dry" = dry ] && return
  case "$out" in
    *"taskbar-id: wrote "*)
      ok "taskbar identities (§92.12): $(printf '%s\n' "$out" | grep -E '^taskbar-id: [^ ]+ (legacy|install) ' | sed 's/^taskbar-id: //; s/:.*//' | paste -sd ';')" ;;
    *) bad "wc-launch --taskbar-ids did not write the ini" ;;
  esac
}

# §92.12. Take this installation off the taskbar: its own jump lists (and the
# application-wide one only while it holds the claim), its claim, desktop and
# Start menu shortcuts, relaunch and jump-list files. Never another
# installation's. Pins are listed for the user to remove.
do_uninstall_taskbar() {
  echo "== uninstall-taskbar =="
  local before out
  # A first deploy has no log yet: the shell's error text is not a count.
  before=$(g "cat $APPDATA_MSYS/wc-launch-$SESSION.log 2>/dev/null | wc -l" | tr -d '\r ')
  [[ "$before" =~ ^[0-9]+$ ]] || before=0
  g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS && ./wc-launch.exe --session-name $SESSION --uninstall-taskbar ${1:-}" >/dev/null 2>&1
  out=$(g "tail -n +$((${before:-0} + 1)) $APPDATA_MSYS/wc-launch-$SESSION.log" | tr -d '\r' | grep 'taskbar-id:' | sed 's/^\[[0-9]*\] //')
  printf '%s\n' "$out" | sed 's/^/        /'
  case "$out" in
    *"relaunch file(s)"*|*"dry run"*) ok "this installation's taskbar presence removed${1:+ (dry run)}" ;;
    *) bad "wc-launch --uninstall-taskbar said nothing" ;;
  esac
}

# §111.10: the host's layout and this deploy's must agree. A host with the
# system services must never get the admin's user units back beside them (two
# session services would race for every boot record), so the old form refuses.
host_system_services() {
  h "test -e /etc/waycompose/users.conf || systemctl cat wc-sessiond.service >/dev/null 2>&1 && echo yes" | grep -qx yes
}
host_layout_check() {
  case "$LAYOUT" in
    user)
      if host_system_services; then
        echo "error: $HOST runs waycompose as system services (§111.10: /etc/waycompose/users.conf, wc-sessiond.service)."
        echo "       An old-form deploy would put the admin's hub and session service back beside them."
        echo "       Re-run with WC_LAUNCH_HOST_LAYOUT=system (and WC_LAUNCH_HOST_ROOT, e.g. 'sudokey run --')."
        echo "       A test installation must also be named in the host's wctestInstalls, so it runs as wctest."
        exit 2
      fi ;;
    system)
      host_system_services || { echo "error: WC_LAUNCH_HOST_LAYOUT=system, and $HOST has no waycompose system services"; exit 2; }
      [ "$DEBUG_SSH" = 1 ] && { echo "error: the debug ssh transport is not available with the system layout"; exit 2; }
      h "$HOST_ROOT true" >/dev/null 2>&1 || { echo "error: '$HOST_ROOT' does not give root on $HOST (set WC_LAUNCH_HOST_ROOT)"; exit 2; }
      local who
      who=$(h "awk -v n=$SESSION '/^#/||NF<3{next} {m=\$3; ok=1; for(i=4;i<=NF;i++) if (\$i ~ /^installs=/) { ok=0; k=split(substr(\$i,10),a,\",\"); for(j=1;j<=k;j++) if (a[j]==n) ok=1 } if (ok) {print \$2\" -> \"m; exit} }' /etc/waycompose/users.conf")
      if [ -z "$who" ]; then
        echo "error: /etc/waycompose/users.conf on $HOST maps no user for installation $SESSION;"
        echo "       a test installation is named in the host's wctestInstalls (it then runs as wctest)"
        exit 2
      fi
      echo "  the host's system layout: $SESSION is staged in $HOSTDIR and installed into $SYSTEM_BASE/$SESSION; its sessions run as $who" ;;
    *) echo "error: WC_LAUNCH_HOST_LAYOUT is user or system, not $LAYOUT"; exit 2 ;;
  esac
}

# The guest-wide and host-wide side effects (see BROKER above): said, and for a
# test installation refused.
deploy_scope_check() {
  local test="" v name val asked=""
  if [ -n "${WC_LAUNCH_TEST_HOME:-}" ]; then test="WC_LAUNCH_TEST_HOME is set"
  elif h "test -s $HOSTDIR/test-home"; then test="$HOSTDIR/test-home exists"
  fi
  for v in "WC_LAUNCH_BROKER=$BROKER" "WC_LAUNCH_PORT_ACCESS=$PORT_ACCESS" \
           "WC_LAUNCH_SERVICES=${WC_LAUNCH_SERVICES:-keep}" "WC_LAUNCH_SYSTEM_BINS=${WC_LAUNCH_SYSTEM_BINS:-keep}"; do
    name=${v%%=*}; val=${v#*=}
    case "$val" in
      install) asked="$asked $name" ;;
      skip|keep) ;;
      *) echo "error: $name is install, skip or keep, not $val"; exit 2 ;;
    esac
  done
  if [ -n "$test" ] && [ -n "$asked" ]; then
    echo "error: a test installation ($test) may not change what every installation shares:$asked=install."
    echo "       Deploy it with those left at their defaults (skip/keep)."
    exit 2
  fi
  if [ -n "$asked" ]; then
    echo "  this deploy changes shared state, asked for by name:$asked"
  else
    echo "  this deploy changes this installation only (broker, port access, host services: left as they are)"
  fi
}

# §111.10: the staged installation into the system layout, as root.
install_staged() {
  scp "${SSHOPT[@]}" "$ROOT/scripts/wc-users-migrate.sh" "$HOST:$HOSTDIR/../wc-users-migrate.sh" >/dev/null \
    || { bad "scp of wc-users-migrate.sh"; exit 1; }
  local out
  out=$(h "$HOST_ROOT bash $HOSTDIR/../wc-users-migrate.sh install-staged \$(id -un) auto $HOSTDIR $SESSION" 2>&1)
  echo "$out" | sed 's/^/        /'
  case "$out" in
    *"[install-staged] done"*) ok "installed into $SYSTEM_BASE/$SESSION (root's; its key the service's alone)" ;;
    *) bad "the staged installation did not install"; exit 1 ;;
  esac
  if [ "${WC_LAUNCH_SYSTEM_BINS:-keep}" = install ]; then
    h "mkdir -p $HOSTDIR/../sysbin" >/dev/null
    scp "${SSHOPT[@]}" "$LINUX_BIN/wc-arena-host" "$REL_LINUX/sv-hostd" "$HOST:$HOSTDIR/../sysbin/" >/dev/null \
      || { bad "scp of the services' binaries"; exit 1; }
    h "$HOST_ROOT install -m 0755 $HOSTDIR/../sysbin/wc-arena-host $HOSTDIR/../sysbin/sv-hostd /var/lib/waycompose/bin/ && \
       $HOST_ROOT systemctl restart wc-bell-hub.service wc-sessiond.service" >/dev/null \
      && ok "WC_LAUNCH_SYSTEM_BINS=install: the system hub and session service run this build (restarted)" \
      || bad "the system services' binaries were not replaced"
  else
    note "the system hub and session service are left as they are (WC_LAUNCH_SYSTEM_BINS=install replaces them)"
  fi
}

do_deploy() {
  echo "== deploy =="
  [ -n "${WC_XKB_MINGW:-}" ] || { echo "error: run inside \`nix develop\`"; exit 2; }
  host_layout_check
  deploy_scope_check
  # A session from the last run holds both halves' binaries open. Ended through
  # the product's own verb, which kills by the pid in its own state file.
  g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS 2>/dev/null && ./wc-launch.exe --session-name $SESSION --stop" >/dev/null 2>&1

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
  cargo build --release --target x86_64-pc-windows-gnu \
        -p wc-launch -p wc-desk 2>&1 | grep -E "^error" | head -5
  built_ok "${PIPESTATUS[0]}" "wc-launch and wc-desk for Windows" || exit 1
  cargo build --release --target x86_64-pc-windows-gnu -p sv-guest --no-default-features -F wayland 2>&1 | grep -E '^error|warning: unused' | head -5
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
  cargo build -p wc-arena-host 2>&1 | grep -E '^error' | head -5
  built_ok "${PIPESTATUS[0]}" "wc-arena-host" || exit 1
  # §88. wc-proxy in release: it copies every frame and parses every message
  # of the session on the VM host's two cores, and the debug build's cost
  # there was measured (a 24 ms p99 between a commit and its copy).
  cargo build --release -p wc-proxy 2>&1 | grep -E '^error' | head -5
  built_ok "${PIPESTATUS[0]}" "wc-proxy (release)" || exit 1
  # §94. The launcher's profile renderer, for the VM host: `wc-session` runs
  # `wc-launch --render-app-files` for every launch, from the host's apps.ini
  # and the two numbers the guest sends, instead of taking the files as text.
  cargo build --release -p wc-launch 2>&1 | grep -E '^error' | head -5
  built_ok "${PIPESTATUS[0]}" "wc-launch for the VM host (release)" || exit 1
  # §99, §105: the doorbell hub, which every session needs since nothing
  # crosses but on rings.
  cargo build --release -p sv-hostd 2>&1 | grep -E '^error' | head -5
  built_ok "${PIPESTATUS[0]}" "sv-hostd for the doorbell hub (release)" || exit 1
  local locator="$LINUX_BIN/wc-arena-host"
  local proxy="$REL_LINUX/wc-proxy"
  local render="$REL_LINUX/wc-launch"
  # §106: the opener the session's applications get as xdg-open.
  local hostopen="$REL_LINUX/wc-host-open"
  fresh "$proxy" "cargo build --release -p wc-proxy" || exit 1
  fresh "$locator"            "cargo build -p wc-arena-host" || exit 1
  fresh "$render" "cargo build --release -p wc-launch" || exit 1
  for b in "$proxy" "$locator" "$render"; do
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
  cp "$ROOT/scripts/wc-port-access.ps1" "$ROOT/scripts/wc-broker-install.ps1" "$stage/"
  write_runint "$stage/runint.ps1"
  write_click  "$stage/click.ps1"
  write_native "$stage/native.ps1"
  write_shot   "$stage/shot.ps1"
  # The scripts name their own directory literally; a run in another one
  # (`WC_LAUNCH_DIRNAME`) gets its own spelled in.
  [ "$DIRNAME" = wclaunch ] || sed -i "s/$SITE_GUEST_USER\\\\wclaunch/$SITE_GUEST_USER\\\\$DIRNAME/g" "$stage"/*.ps1

  # The configuration. Written here because the harness knows the paths; a real
  # installation would ship it beside the exe.
  cat >"$stage/wc-launch.ini" <<INI
# Generated by scripts/wc-launch.sh. See docs/waycompose-design.md §28.
[session]
compositor = $DIR_WIN\\sv-guest.exe
name       = $SESSION
# §105: no ssh. The session starts from a boot record signed with this
# installation's key (session.key beside this file), which the VM host's
# session service checks; every request after that is on the session's rings.
installation = $SESSION
debug-ssh-transport = $([ "$DEBUG_SSH" = 1 ] && echo on || echo off)
$([ "$DEBUG_SSH" = 1 ] && printf 'host       = %s\nkey        = %s\\id_host\nrelay      = %s\\probe-relay.exe\n' "$HOSTLINK" "$DIR_WIN" "$DIR_WIN")
arena-mib  = $ARENA_MIB
outbound   = 1048576
output     = $OUTPUT
port       = $PORT
audio      = ${WC_LAUNCH_AUDIO:-on}
audio-port = $((PORT + 1))
audio-volume = $AUDIO_VOLUME
gpu-sessions = $GPU_SESSIONS
gpu-arena-mib = $GPU_ARENA_MIB
# §97: one pool over the whole GPU device, leased as each session needs it;
# \`slots\` is §87's fixed slots of gpu-arena-mib.
gpu-arena  = ${WC_LAUNCH_GPU_ARENA:-pool}
gpu-ivshmem-slot = ${WC_LAUNCH_GPU_SLOT:-0x1d}
gpu-bell   = ${WC_LAUNCH_GPU_BELL-org.sirati.svbell.0}
# §89, §105: the Wayland rings' doorbell port. Every byte of the session crosses
# on those rings; a VM without the port polls them, loudly.
wayland-bell = ${WC_LAUNCH_WAYLAND_BELL-org.sirati.wcbell.0}
# §102: WC_LAUNCH_NESTED_GLOBALS=1 offers wp_single_pixel_buffer_manager_v1,
# wp_viewporter and wp_presentation, which a nested compositor (KWin) needs.
nested-compositor-globals = $([ "${WC_LAUNCH_NESTED_GLOBALS:-0}" = 1 ] && echo on || echo off)
# §107: one compositor for every installation of the user; off is §105's own
# compositor per installation (WC_LAUNCH_SHARED_COMPOSITOR=off), for an
# installation that is not to join the shared one yet.
shared-compositor = ${WC_LAUNCH_SHARED_COMPOSITOR:-on}

[app.firefox]
title = $TITLE
# §94: what runs is the VM host's to say, in $HOSTDIR/apps.ini; this names it.
host-app = firefox
# §92: the desktop entry on the VM host whose name, icon and actions become the
# Start menu entry, the pin and the jump list.
desktop = firefox
icon  = firefox
INI
  # §96: more applications, each a block of the guest's ini and a section of the
  # host's apps.ini (WC_LAUNCH_EXTRA_APPS and WC_LAUNCH_EXTRA_APP_BLOCKS, files
  # appended as they are). The host's half is checked by `wc-session apps` below
  # like Firefox's; the guest's by the launcher's own parser at the next start.
  if [ -n "${WC_LAUNCH_EXTRA_APP_BLOCKS:-}" ]; then
    [ -f "$WC_LAUNCH_EXTRA_APP_BLOCKS" ] || { echo "error: no $WC_LAUNCH_EXTRA_APP_BLOCKS"; exit 1; }
    { echo; cat "$WC_LAUNCH_EXTRA_APP_BLOCKS"; } >>"$stage/wc-launch.ini"
  fi

  # §94. THE HOST'S DEFINITION OF THE APPLICATION, which the guest names by id
  # and cannot change. Everything that used to be a word of `run`: the
  # executable, the profile, the deployment's preferences, the title bar.
  # Beside wc-session on the VM host, not in the stage the guest receives.
  local hstage; hstage=$(mktemp -d /tmp/wc-launch-host-XXXXXX)
  local profile_words="${WC_LAUNCH_PROFILE:-${SITE_HOST_HOME}/.wc-launch/firefox}"
  local app_profile="${profile_words%% *}" app_args=""
  [ "$app_profile" = "$profile_words" ] || app_args="${profile_words#* }"
  # `exec` is filled in on the host, after wc-session is there to resolve it.
  cat >"$hstage/apps.ini" <<APPS
# Generated by scripts/wc-launch.sh deploy. docs/waycompose-design.md §94.
#
# The applications the guest's key may start, by section name. The guest sends
# the name, an action id or a URL, and the desktop's DPI and caption height;
# everything else is here. scripts/wc-session documents every key.
[firefox]
exec     = @EXEC@
args     = $app_args
profile  = $app_profile
# A middle click on Windows does not paste, so it must not paste here either.
prefs    = middlemouse.paste=false${WC_LAUNCH_EXTRA_PREFS:+;$WC_LAUNCH_EXTRA_PREFS}
# §82: tabs in Windows' caption; WC_LAUNCH_TITLEBAR=none is the opt-out.
titlebar = ${WC_LAUNCH_TITLEBAR:-tabs}
desktop  = firefox
gpu      = on
APPS
  if [ -n "${WC_LAUNCH_EXTRA_APPS:-}" ]; then
    [ -f "$WC_LAUNCH_EXTRA_APPS" ] || { echo "error: no $WC_LAUNCH_EXTRA_APPS"; exit 1; }
    { echo; cat "$WC_LAUNCH_EXTRA_APPS"; } >>"$hstage/apps.ini"
    sed -i "s|@HOST_HOME@|$SITE_HOST_HOME|g" "$hstage/apps.ini"
  fi

  # A running .exe holds its own file open, so a session that is up must end
  # before its binaries can be replaced. By path, not by image name.
  g "export MSYS_NO_PATHCONV=1; mkdir -p $DIR_MSYS; \
     powershell.exe -NoProfile -Command \"Get-CimInstance Win32_Process | \
       Where-Object { \\\$_.ExecutablePath -and \\\$_.ExecutablePath.StartsWith('$DIR_WIN\\\\') } | \
       ForEach-Object { Stop-Process -Id \\\$_.ProcessId -Force -ErrorAction SilentlyContinue }\"" \
    >/dev/null 2>&1
  sleep 1
  # §92.12: the ini about to be replaced carries each application's taskbar
  # identity (`taskbar-id`), decided by an earlier deploy; `--taskbar-ids`
  # below reads it from here, so no decision flips.
  g "cd $DIR_MSYS && if [ -f wc-launch.ini ]; then cp -f wc-launch.ini wc-launch.ini.prev; else rm -f wc-launch.ini.prev; fi" >/dev/null 2>&1
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
  taskbar_ids

  # §91. The doorbell ports for a medium-integrity launch: the two port devices'
  # descriptor gains read and write for $PORT_USER (and nothing else changes),
  # and a SYSTEM task re-applies it at every startup. Run over ssh, which holds
  # the administrator's token. A port another session holds keeps its live
  # descriptor until it is free (opening a held vioser port disconnects its
  # holder); the PnP property, which the next device start applies, is written
  # regardless. `scripts/wc-port-access.ps1 -Action uninstall` undoes all of it.
  if [ "$BROKER" = install ]; then
    # §111.8: the broker service. It sets the four devices to SYSTEM and
    # Administrators only at every start, and lends them to the user's own
    # processes; §91's task and grant are removed by the same script.
    local bi
    bi=$(g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
          -File '$DIR_WIN\\wc-broker-install.ps1' -Action install -Source '$DIR_WIN'" | tr -d '\r')
    echo "$bi" | sed 's/^/        /'
    if echo "$bi" | grep -q 'installed:'; then
      ok "the broker service runs from Program Files as LocalSystem; the devices are lent by it"
    else
      bad "the broker service was not installed; sessions open the devices themselves (and a medium-integrity one is refused)"
    fi
  elif [ "$PORT_ACCESS" = install ]; then
    local pa
    pa=$(g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
          -File '$DIR_WIN\\wc-port-access.ps1' -Action install -User $PORT_USER" | tr -d '\r')
    echo "$pa" | sed 's/^/        /'
    if ! echo "$pa" | grep -q 'installed:'; then
      bad "the doorbell ports were not opened to $PORT_USER; a double-click will fall back to ssh"
    elif echo "$pa" | grep -q 'NOT changed'; then
      note "the ports' PnP descriptor admits $PORT_USER, and the live one waits for the ports to be free (re-run deploy, or the next boot applies it)"
    else
      ok "the doorbell ports admit $PORT_USER at medium integrity, live and at every device start"
    fi
  else
    note "WC_LAUNCH_PORT_ACCESS=$PORT_ACCESS: the guest's port security is left alone"
  fi

  # §105. THE INSTALLATION'S KEY, which replaced the guest's ssh key: 32 random
  # bytes as 64 hex digits, in two places only, the installation's directory
  # on the VM host (0600) and its directory in the guest user's profile
  # (readable by that user alone). The guest signs its boot record with it and
  # the host's session service checks the signature. Made on the host the
  # first time, kept across deploys; it crosses to the guest once, over this
  # deploy channel, which is an administrator's and not the session's.
  local key
  # §111.10: an installed installation's key is the service's; the staging
  # copy is made from it, and install-staged keeps the installed one.
  if [ "$LAYOUT" = system ]; then
    h "mkdir -p $HOSTDIR; k=\$($HOST_ROOT cat $SYSTEM_BASE/$SESSION/session.key 2>/dev/null); \
       [ -n \"\$k\" ] && (umask 077; printf '%s' \"\$k\" >$HOSTDIR/session.key)" >/dev/null 2>&1
  fi
  key=$(h "mkdir -p $HOSTDIR; test -s $HOSTDIR/session.key || \
           (umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >$HOSTDIR/session.key); \
           chmod 600 $HOSTDIR/session.key; cat $HOSTDIR/session.key")
  [[ "$key" =~ ^[0-9a-f]{64}$ ]] || { echo "error: no installation key on the VM host"; exit 1; }
  if [ "$BROKER" = install ]; then
    # §111.8: the key goes to the broker, which signs the boot record with
    # it and with the caller's identity; the user cannot read it, so a user
    # process cannot sign a record that claims another user.
    local ki
    ki=$(printf '%s' "$key" | g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass \
          -File '$DIR_WIN\\wc-broker-install.ps1' -Action key -Install $SESSION" | tr -d '\r')
    key=""
    echo "$ki" | sed 's/^/        /'
    if echo "$ki" | grep -q 'stored in'; then
      g "export MSYS_NO_PATHCONV=1; rm -f $DIR_MSYS/session.key" >/dev/null
      ok "the installation key is the broker's (ProgramData\\waycompose\\keys, SYSTEM and Administrators only); the user's copy is removed"
    else
      bad "the installation key did not reach the broker"
    fi
  else
    printf '%s' "$key" | g "export MSYS_NO_PATHCONV=1; cat >$DIR_MSYS/session.key; \
        icacls '$DIR_WIN\\session.key' /inheritance:r /grant:r '${SITE_GUEST_USER}:R' >/dev/null" >/dev/null
    key=""
    local kchk
    kchk=$(g "export MSYS_NO_PATHCONV=1; icacls '$DIR_WIN\\session.key'" | tr -d '\r' | grep -c ':(R)')
    [ "$kchk" = 1 ] && ok "the installation key is on the guest, readable by ${SITE_GUEST_USER} alone" \
      || bad "the installation key's ACL on the guest is not one reader ($kchk)"
  fi
  # §105: the installation, registered with the host's session service by name.
  # (§111.10: with the system layout, by its directory under $SYSTEM_BASE.)
  [ "$LAYOUT" = user ] && h "mkdir -p \$HOME/.config/waycompose/installs && ln -sfn $HOSTDIR \$HOME/.config/waycompose/installs/$SESSION" >/dev/null

  # The guest's ssh key for the VM host: only for the opt-in debug transport.
  local pub=""
  if [ "$DEBUG_SSH" = 1 ]; then
    note "WC_LAUNCH_DEBUG_SSH_TRANSPORT=1: installing the guest's ssh key and forced command for the DEBUG transport"
    pub=$(g "export MSYS_NO_PATHCONV=1; test -f $DIR_MSYS/id_host || \
          ssh-keygen -t ed25519 -N '' -C wc-launch-guest -f $DIR_MSYS/id_host >/dev/null; \
          cat $DIR_MSYS/id_host.pub" | grep '^ssh-ed25519')
    [ -n "$pub" ] || { echo "error: the guest produced no key"; exit 1; }
  fi

  # `scp` over a running binary is ETXTBSY on Linux and a held file handle on
  # Windows, and a session left up from the last run is running exactly these
  # two. Unlinking first replaces the name and leaves the running process on the
  # inode it already has, which is the whole point of the trick.
  h "rm -f $HOSTDIR/bin/wc-proxy $HOSTDIR/bin/wc-arena-host $HOSTDIR/bin/wc-launch $HOSTDIR/bin/wc-host-open" >/dev/null 2>&1
  scp "${SSHOPT[@]}" "$ROOT/scripts/wc-session" "$HOST:$HOSTDIR/" >/dev/null
  scp "${SSHOPT[@]}" "$proxy" "$locator" "$render" "$hostopen" "$HOST:$HOSTDIR/bin/" >/dev/null \
    || { echo "error: scp to the VM host"; exit 1; }
  # STAGE 42, the host boundary. The unlink trick above means `scp` cannot fail
  # on a live binary any more, which removes the loud failure and leaves only
  # the quiet one: a copy that did not land where the run will look for it.
  deployed_is "$proxy" "$HOSTDIR/bin/wc-proxy"      h "re-run $0 deploy" || exit 1
  deployed_is "$locator"            "$HOSTDIR/bin/wc-arena-host" h "re-run $0 deploy" || exit 1
  deployed_is "$render" "$HOSTDIR/bin/wc-launch" h "re-run $0 deploy" || exit 1
  deployed_is "$hostopen" "$HOSTDIR/bin/wc-host-open" h "re-run $0 deploy" || exit 1
  # §106: the xdg-open shim directory, put first on the applications' PATH.
  h "mkdir -p $HOSTDIR/wc-hostint" && scp "${SSHOPT[@]}" "$ROOT/scripts/wc-hostint/xdg-open" "$HOST:$HOSTDIR/wc-hostint/" >/dev/null \
    && h "chmod 755 $HOSTDIR/wc-hostint/xdg-open" || { echo "error: scp of the xdg-open shim"; exit 1; }
  h "chmod 755 $HOSTDIR/wc-session $HOSTDIR/bin/*" >/dev/null
  if [ "$DEBUG_SSH" = 1 ]; then
    h "mkdir -p ~/.ssh; chmod 700 ~/.ssh; touch ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys; \
       grep -q '$(echo "$pub" | awk '{print $2}')' ~/.ssh/authorized_keys || \
         echo 'restrict,port-forwarding,command=\"$HOSTDIR/wc-session\" $pub' >> ~/.ssh/authorized_keys" >/dev/null
    echo "  copied wc-session, wc-proxy, wc-arena-host and wc-launch to $HOST:$HOSTDIR; DEBUG: the guest's ssh key is restricted to it"
  else
    # §105: this installation's forced-command key, if an earlier deploy put
    # one there, is removed: nothing of the session uses ssh, so nothing may.
    local removed
    removed=$(h "f=\$HOME/.ssh/authorized_keys; [ -f \$f ] || { echo 0; exit; }; \
      n=\$(grep -cF 'command=\"$HOSTDIR/wc-session\"' \$f); \
      [ \"\$n\" = 0 ] || { grep -vF 'command=\"$HOSTDIR/wc-session\"' \$f >\$f.wcnew && chmod 600 \$f.wcnew && mv \$f.wcnew \$f; }; echo \$n")
    echo "  copied wc-session, wc-proxy, wc-arena-host and wc-launch to $HOST:$HOSTDIR; the guest has no ssh key for it (removed ${removed:-0} old forced-command line(s))"
  fi

  # §94. The application's executable, as an absolute path on the host, and
  # then the definitions checked by the script that will read them.
  local exe="$FIREFOX"
  case "$exe" in
    /*) ;;
    *)
      exe=$(h "$HOSTDIR/wc-session resolve '$FIREFOX'" | tail -1)
      case "$exe" in
        /nix/store/*)
          # Rooted, so a garbage collection cannot take the application from
          # under a definition that names it.
          local pkg; pkg=$(echo "$exe" | cut -d/ -f1-4)
          h "nix-store -r $pkg --add-root $HOSTDIR/bin/app-firefox --indirect >/dev/null" \
            && exe="$HOSTDIR/bin/app-firefox/${exe#"$pkg"/}" ;;
      esac ;;
  esac
  sed -i "s|@EXEC@|$exe|" "$hstage/apps.ini"
  # §102: the proxy's copy scope for this installation (wc-session reads
  # $HOSTDIR/copy-scope). Unset: damage, and no file.
  case "$DMABUF_NODE" in
    /dev/dri/renderD[0-9]*) h "printf '%s' $DMABUF_NODE > $HOSTDIR/dmabuf-node" >/dev/null ;;
    "") h "rm -f $HOSTDIR/dmabuf-node" >/dev/null ;;
    *) echo "error: WC_LAUNCH_DMABUF_NODE is a /dev/dri/renderD<N> path"; exit 1 ;;
  esac
  case "$GPU_PCI" in
    [0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]:[0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F])
      h "printf '%s' $GPU_PCI > $HOSTDIR/gpu-pci" >/dev/null ;;
    "") h "rm -f $HOSTDIR/gpu-pci" >/dev/null ;;
    *) echo "error: WC_LAUNCH_GPU_PCI is VVVV:DDDD"; exit 1 ;;
  esac
  case "${WC_LAUNCH_COPY_SCOPE:-}" in
    buffer) h "printf buffer > $HOSTDIR/copy-scope" >/dev/null ;;
    ""|damage) h "rm -f $HOSTDIR/copy-scope" >/dev/null ;;
    *) echo "error: WC_LAUNCH_COPY_SCOPE is damage or buffer"; exit 1 ;;
  esac
  scp "${SSHOPT[@]}" "$hstage/apps.ini" "$HOST:$HOSTDIR/apps.ini" >/dev/null \
    || { echo "error: scp of apps.ini to the VM host"; exit 1; }
  rm -rf "$hstage"
  local apps
  apps=$(h "$HOSTDIR/wc-session apps")
  case "$apps" in
    *"apps ok"*) ok "the host's application definitions ($HOSTDIR/apps.ini): $(echo "$apps" | grep '^app ' | sed 's/ args .*//' | tr '\n' ';')" ;;
    *) bad "$HOSTDIR/apps.ini does not check: $apps"; exit 1 ;;
  esac
  if [ "$GPU_SESSIONS" -gt 0 ]; then
    h "rm -f $HOSTDIR/bin/sv-hostd $HOSTDIR/bin/libsv_icd.so $HOSTDIR/bin/libsv_wlshim.so $HOSTDIR/bin/sv-drm" >/dev/null 2>&1
    scp "${SSHOPT[@]}" "$REL_LINUX/sv-hostd" "$REL_LINUX/libsv_icd.so" "$REL_LINUX/libsv_wlshim.so" "$REL_LINUX/sv-drm" "$HOST:$HOSTDIR/bin/" >/dev/null \
      || { echo "error: scp of the GPU half to the VM host"; exit 1; }
    h "printf '{\"file_format_version\":\"1.0.1\",\"ICD\":{\"library_path\":\"%s\",\"api_version\":\"1.4.0\",\"library_arch\":\"64\",\"is_portability_driver\":false}}\\n' $HOSTDIR/bin/libsv_icd.so >$HOSTDIR/bin/sv_icd.json"
    deployed_is "$REL_LINUX/sv-hostd" "$HOSTDIR/bin/sv-hostd" h "re-run $0 deploy" || exit 1
    deployed_is "$REL_LINUX/libsv_wlshim.so" "$HOSTDIR/bin/libsv_wlshim.so" h "re-run $0 deploy" || exit 1
    local icd_json=$HOSTDIR/bin/sv_icd.json
    if [ -n "$ICD_STORE" ]; then
      # One store path holding the library and a manifest that names it
      # relatively, GC-rooted beside the binaries.
      icd_json=$(h "set -e; t=\$(mktemp -d); cp $HOSTDIR/bin/libsv_icd.so \$t/; \
        printf '{\"file_format_version\":\"1.0.1\",\"ICD\":{\"library_path\":\"./libsv_icd.so\",\"api_version\":\"1.4.0\",\"library_arch\":\"64\",\"is_portability_driver\":false}}\\n' >\$t/sv_icd.json; \
        p=\$(nix-store --add \$t); rm -rf \$t; nix-store -r \$p --add-root $HOSTDIR/bin/icd-store --indirect >/dev/null; echo \$p/sv_icd.json" | tail -1)
      case "$icd_json" in /nix/store/*/sv_icd.json) ok "the ICD in the host's store: $icd_json" ;; *) bad "the ICD did not go into the store: $icd_json"; exit 1 ;; esac
    fi
    local genv
    genv="MESA_LOADER_DRIVER_OVERRIDE=zink
__EGL_VENDOR_LIBRARY_FILENAMES=${KOPPER_MESA:-<mesa-kopper-wayland>}/share/glvnd/egl_vendor.d/50_mesa.json
LIBGL_DRIVERS_PATH=${KOPPER_MESA:-<mesa-kopper-wayland>}/lib/dri
LIBVA_DRIVER_NAME=zink
LIBVA_DRIVERS_PATH=${KOPPER_MESA:-<mesa-zink-va>}/lib/dri
VK_DRIVER_FILES=$icd_json"
    # Only the legacy wl_shm-only path needs a forced software EGLDevice.
    if [ -z "$DMABUF_NODE" ]; then
      genv="$genv
LIBGL_KOPPER_DRI2=1"
    fi
    if [ "$GPU_ENV_MODE" = write ] && [ -n "$KOPPER_MESA" ]; then
      printf '%b\n' "$genv" | h "cat >$HOSTDIR/gpu.env"
      echo "  wrote $HOSTDIR/gpu.env: sessions with GPU sides render on the guest's GPU"
    else
      note "gpu.env NOT written (WC_LAUNCH_GPU_ENV=write and WC_LAUNCH_GPU_MESA enable it); it would be:"
      printf '%b\n' "$genv" | sed 's/^/          /'
    fi
  fi
  # §99, §105: the hub and the session service, for every installation.
  # WC_LAUNCH_SERVICES=keep (a test installation beside production ones):
  # the running hub and session service are left exactly as they are, binary
  # and unit, and only checked.
  if [ "$LAYOUT" = system ]; then
    : # the system services are the host's (installed after the GC roots below)
  elif [ "${WC_LAUNCH_SERVICES:-keep}" = keep ]; then
    local ks; ks=$(h "export XDG_RUNTIME_DIR=/run/user/\$(id -u); systemctl --user is-active wc-bell-hub.service wc-sessiond.service | tr '\n' ' '")
    case "$ks" in
      "active active "*) ok "WC_LAUNCH_SERVICES=keep: the shared hub and session service run, untouched" ;;
      *) bad "WC_LAUNCH_SERVICES=keep, and the shared services are not both running: $ks"; exit 1 ;;
    esac
  else
    install_services "$locator"
  fi
  # §92. The jump list's two tools, GC-rooted where wc-session looks first.
  local tool
  for tool in "sqlite:$SQLITE:sqlite3" "librsvg:$RSVG:rsvg-convert"; do
    IFS=: read -r pkg path bin <<<"$tool"
    [ -n "$path" ] || { note "WC_LAUNCH_$(echo "${pkg#lib}" | tr a-z A-Z) unset: wc-session finds $bin in the host's store, unrooted"; continue; }
    if h "nix-store -r $path --add-root $HOSTDIR/bin/$pkg --indirect >/dev/null && test -x $HOSTDIR/bin/$pkg/bin/$bin"; then
      echo "  $bin from $path (GC root $HOSTDIR/bin/$pkg)"
    else
      bad "$path is not on $HOST, or has no bin/$bin: the jump list goes without it"
    fi
  done
  if [ -n "$PULSE" ]; then
    if h "nix-store -r $PULSE --add-root $HOSTDIR/bin/pulse --indirect >/dev/null && test -x $HOSTDIR/bin/pulse/bin/pulseaudio"; then
      echo "  sessions get a PulseAudio null sink from $PULSE (GC root $HOSTDIR/bin/pulse)"
    else
      bad "$PULSE is not on $HOST: nix copy it there first; sessions will have no sound"
    fi
  else
    note "WC_LAUNCH_PULSE unset: wc-session looks for pulseaudio in the host's store, and without one Firefox gets a null audio context"
  fi
  # §106: the session's own PipeWire, for WC_LAUNCH_AUDIO=pipewire.
  if [ -z "$PW_NODE" ] && [ -n "$PIPEWIRE" ]; then
    if PW_NODE=$(cd "$ROOT" && nix build --no-link --print-out-paths .#wc-pw-node) \
        && NIX_SSHOPTS="${SSHOPT[*]}" nix copy --to "ssh://$HOST" "$PW_NODE"; then
      echo "  wc-pw-node built and copied: $PW_NODE"
    else
      bad "wc-pw-node did not build or copy: under PipeWire the sound ring falls back to pipe modules, which drop"
      PW_NODE=""
    fi
  fi
  for tool in "pipewire:$PIPEWIRE:pipewire" "wireplumber:$WIREPLUMBER:wireplumber" "wc-pw-node:$PW_NODE:wc-pw-node"; do
    IFS=: read -r pkg path bin <<<"$tool"
    [ -n "$path" ] || continue
    if h "nix-store -r $path --add-root $HOSTDIR/bin/$pkg --indirect >/dev/null && test -x $HOSTDIR/bin/$pkg/bin/$bin"; then
      echo "  $bin from $path (GC root $HOSTDIR/bin/$pkg)"
    else
      bad "$path is not on $HOST, or has no bin/$bin: nix copy it there first"
    fi
  done

  # §111.10: the staged installation, complete with its GC roots, goes in.
  [ "$LAYOUT" = system ] && install_staged

  # And the one end-to-end check of the path that does not need a compositor.
  # Asked of `wc-launch.exe` itself rather than of a hand-built ssh command
  # line, so that what is tested is the argv the product uses: the first version
  # of this check went through two shells, lost every backslash of the key's
  # path, and reported "Permission denied (publickey)" for a file it had never
  # opened.
  #
  # §105: since there is no ssh, the guest reaches the host only through a
  # session's rings, so that check is the cold start itself (`cold`). What is
  # checked here is that nothing of ssh is left for the guest to use.
  if [ "$DEBUG_SSH" = 1 ]; then
    g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS && ./wc-launch.exe --session-name $SESSION --ping" >/dev/null
    local said
    said=$(read_clock | grep 'PING' | tail -1)
    case "$said" in
      *"wc-session ok"*) ok "DEBUG: the guest reaches the VM host on its ssh key: ${said#*PING }" ;;
      *) bad "the guest cannot reach the VM host"; read_clock | tail -5 | sed 's/^/        /' ;;
    esac
  else
    g "export MSYS_NO_PATHCONV=1; rm -f $DIR_MSYS/id_host $DIR_MSYS/id_host.pub $DIR_MSYS/known_hosts $DIR_MSYS/probe-relay.exe" >/dev/null
    local left
    left=$(h "grep -cF 'command=\"$HOSTDIR/wc-session\"' \$HOME/.ssh/authorized_keys 2>/dev/null")
    [ "${left:-0}" = 0 ] && ok "no ssh key of this installation on either side (guest key files and host forced command gone)" \
      || bad "the host still has $left forced-command line(s) for $HOSTDIR"
  fi
}

# --------------------------------------------------------------- install -----
do_install() {
  echo "== install =="
  # §96: the applications to install, by the id of their `[app.*]` block;
  # Firefox's unless WC_LAUNCH_INSTALL_APPS says otherwise. Firefox's checks
  # below are Firefox's; any other id gets its shortcut and its log line.
  local apps="${WC_LAUNCH_INSTALL_APPS:-firefox}" a
  for a in $apps; do
    [[ "$a" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$ ]] && [[ "$a" != *..* ]] || { bad "not an app id: $a"; return; }
    [ "$a" = firefox ] && continue
    local o l
    o=$(g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS && ./wc-launch.exe --session-name $SESSION --install $a; echo rc=\$?" | tr -d '\r')
    l=$(g "tail -20 $APPDATA_MSYS/wc-launch-$SESSION.log" | tr -d '\r' | grep 'installed ' | tail -1)
    case "$l" in
      *"installed "*) ok "$a: the shortcut is on the desktop: ${l#*installed }" ;;
      *) bad "$a: no shortcut was installed ($o)" ;;
    esac
  done
  case " $apps " in *" firefox "*) ;; *) return ;; esac
  local out
  out=$(g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS && ./wc-launch.exe --session-name $SESSION --install firefox; echo rc=\$?" | tr -d '\r')
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
  # §92. Since the shortcuts are written through IShellLinkW, the install also
  # wrote the icon, the relaunch file, the Start menu entry and the jump list,
  # and logged one `pins:` line per step that went wrong and one for the list.
  local pins never_run=0
  pins=$(g "tail -30 $APPDATA_MSYS/wc-launch-$SESSION.log" | tr -d '\r' | grep '\] pins: ')
  # §92.12: the identity is this installation's own, or the application-wide
  # one where deploy found the user's pin of it (`taskbar-id` in the ini).
  # (A name with capitals also gets a hash suffix, which this check does not
  # compute; deployed names are lower case.)
  local tid aumid
  tid=$(g "awk '/^\\[/{s=\$0} s==\"[app.firefox]\" && /^taskbar-id/{print \$3}' $DIR_MSYS/wc-launch.ini" | tr -d '\r')
  if [ "$tid" = legacy ]; then aumid=Waycompose.app.firefox
  else aumid="Waycompose.install.$(printf '%s' "$SESSION" | tr 'A-Z' 'a-z').firefox"; fi
  # §107.11: the icon and the jump list are the host's answer about the
  # application, and since §105 that answer comes over a session's control
  # ring. An installation no session has run yet has nothing to ask and
  # nothing cached; the first session's refresh writes both. That is not a
  # failure; any other reason for a missing icon or list still is.
  if printf '%s\n' "$pins" | grep -q 'no session to ask' \
    && ! g "test -s $DIR_MSYS/apps/firefox.meta && echo cached" | grep -q cached; then
    never_run=1
  fi
  if g "test -s $DIR_MSYS/apps/$aumid.relaunch && echo yes" | grep -q yes; then
    ok "Firefox's relaunch file: $DIR_WIN\\apps\\$aumid.relaunch"
  else
    bad "no relaunch file"; printf '%s\n' "$pins" | sed 's/^/        /'
  fi
  if g "test -s $DIR_MSYS/firefox.ico && echo yes" | grep -q yes; then
    ok "Firefox's own icon: $DIR_WIN\\firefox.ico"
  elif [ "$never_run" = 1 ]; then
    ok "no icon yet: no session of $SESSION has run, so there is no host to ask; the first session writes it"
  else
    bad "no icon"; printf '%s\n' "$pins" | sed 's/^/        /'
  fi
  case "$pins" in
    *"jump list "*" task(s)"*) ok "$(printf '%s\n' "$pins" | grep 'jump list ' | tail -1 | sed 's/^.*pins: //')" ;;
    *"no desktop entry known yet"*) [ "$never_run" = 1 ] \
      && ok "no jump list yet: no session of $SESSION has run; the first session commits it" \
      || bad "no jump list: $(printf '%s\n' "$pins" | tail -2)" ;;
    *) bad "no jump list: $(printf '%s\n' "$pins" | tail -2)" ;;
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
#
# §91: **this installation's profile, and only it.** The pattern was
# `wc-launch/firefox`, which every launcher profile matches — the deployed one a
# person uses (`~/.wc-launch/firefox`) included — so a test installation's
# `cold` ended the person's Firefox too, and `WC_LAUNCH_FRESH` deleted *their*
# profile whatever profile this run used.
PROFILE_DIR="${WC_LAUNCH_PROFILE:-${SITE_HOST_HOME}/.wc-launch/firefox}"
PROFILE_DIR="${PROFILE_DIR%% *}"
kill_host_firefox() {
  case "$PROFILE_DIR" in
    ${SITE_HOST_HOME}/.wc-launch/?*) ;;
    *) bad "refusing to touch a profile outside ~/.wc-launch: $PROFILE_DIR"; return ;;
  esac
  [ -n "${WC_LAUNCH_FRESH:-}" ] && h "rm -rf '$PROFILE_DIR'" >/dev/null 2>&1
  h "pkill -f -- '--profile $PROFILE_DIR( |\$)' 2>/dev/null
     for _ in \$(seq 1 40); do
       pgrep -f -- '--profile $PROFILE_DIR( |\$)' >/dev/null 2>&1 || break
       sleep 0.5
     done
     rm -f '$PROFILE_DIR/.parentlock' '$PROFILE_DIR/lock' 2>/dev/null
     true" >/dev/null 2>&1
}

do_stop() {
  echo "== stop =="
  g "export MSYS_NO_PATHCONV=1; cd $DIR_MSYS && ./wc-launch.exe --session-name $SESSION --stop" >/dev/null
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
  INT_LEVEL=$CLICK_RUNLEVEL int "wclaunch-click-$kind" "powershell.exe" \
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
  read_clock | grep -E 'token:|arena:|locate:|up:|proxy:|run Firefox|cold start|session up' | sed 's/^/        /'
  # §91. The route the click took, from the launcher's own line, and what it
  # got. A medium click that says anything but medium is not a double-click,
  # whatever it measured.
  local tok; tok=$(read_clock | grep 'token: ' | tail -1)
  case "$CLICK_LEVEL" in
    medium)
      case "$tok" in
        *"medium integrity"*"not elevated"*) ok "$kind: the click ran as a double-click does: ${tok#*token: }" ;;
        *) bad "$kind: WC_LAUNCH_CLICK=medium but the launcher says: ${tok:-no token line}" ;;
      esac ;;
    elevated) note "$kind: an ELEVATED click (WC_LAUNCH_CLICK=elevated), not what a double-click gets: ${tok#*token: }" ;;
  esac
  # Only the click that started the session brought the transports up.
  case "$kind" in
    cold|warm-first)
      local clock; clock=$(read_clock)
      if echo "$clock" | grep -q 'SSH FALLBACK\|DEBUG SSH TRANSPORT'; then
        bad "$kind: the session used ssh: $(echo "$clock" | grep -m1 'SSH FALLBACK\|DEBUG SSH' | cut -c1-240)"
      elif echo "$clock" | grep -q 'boot: located' && echo "$clock" | grep -q 'shared memory only'; then
        ok "$kind: no ssh: the session booted from the header page and every byte is on the ivshmem rings"
        echo "$clock" | grep -q 'NO DOORBELL' && bad "$kind: the rings have no doorbell and are polled"
      elif echo "$clock" | grep -q 'transport rings:'; then
        ok "$kind: the Wayland connections are on the ivshmem rings, with their doorbell"
      else
        bad "$kind: no 'transport rings:' line, and no fallback line either"
      fi
      if [ "$GPU_SESSIONS" -gt 0 ]; then
        if echo "$clock" | grep -q 'doorbell attached on'; then
          ok "$kind: the GPU side has its doorbell: $(echo "$clock" | grep -m1 'doorbell attached on' | sed 's/.*doorbell attached on/attached on/')"
        else
          bad "$kind: the GPU side has no doorbell: $(echo "$clock" | grep -m1 'no doorbell' | cut -c1-240)"
        fi
      fi ;;
  esac
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
  INT_LEVEL=$CLICK_RUNLEVEL int "wclaunch-native" "powershell.exe" \
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
    "$SITE_HOST_USER"*) ok "wc-proxy runs as the user, not root: $puser" ;;
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
  broker-show)
    g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass -File '$DIR_WIN\\wc-broker-install.ps1' -Action show" | tr -d '\r' ;;
  broker-uninstall)
    # §111.8's revert: the service goes, the devices get their descriptors
    # from before, and the keys stay (re-deploy with WC_LAUNCH_BROKER=skip
    # puts a key back in the user's directory).
    g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -ExecutionPolicy Bypass -File '$DIR_WIN\\wc-broker-install.ps1' -Action uninstall" | tr -d '\r' ;;
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
  taskbar-plan) echo "== taskbar-plan (dry run) =="; taskbar_ids dry ;;
  uninstall-taskbar) do_uninstall_taskbar "${WC_LAUNCH_DRY_RUN:+--dry-run}" ;;
  cold)    do_cold ;;
  warm)    do_warm ;;
  native)  do_native ;;
  shot)    do_shot; do_desktop_shot ;;
  stop)    do_stop ;;
  clock)   read_clock ;;
  all)     do_deploy; echo; do_install; echo; do_native; echo; do_cold; echo; do_warm; echo; do_shot ;;
  *) echo "usage: $0 deploy|install|taskbar-plan|uninstall-taskbar|cold|warm|bench|native|verify|shot|stop|clock|all"; exit 2 ;;
esac

echo
echo "  $pass passed, $fail failed"
[ "$fail" -eq 0 ]
