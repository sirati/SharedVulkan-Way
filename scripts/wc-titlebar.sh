#!/usr/bin/env bash
# Photograph the title bar, on the guest's **interactive desktop**, with the
# pixels in the guest's own RAM.
#
#   scripts/wc-titlebar.sh deploy   cross-compile and copy what this run needs
#   scripts/wc-titlebar.sh server   one caption: default_mode(2) + the env var
#   scripts/wc-titlebar.sh client   the arrangement before this stage, for the pair
#   scripts/wc-titlebar.sh both     deploy, then both legs
#   scripts/wc-titlebar.sh clean    remove what this script put on the guest
#
# Run from inside `nix develop`. Pictures land in docs/stage14/.
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
OUT="$ROOT/docs/stage14"

GUEST="${WC_GUEST:-<guest-user>@<guest-ip>}"
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"
# Everything this run owns is named from this. See the header.
TBNAME="${WC_TBNAME:-wcs13tb}"
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
  local e
  for e in wc-compositor.exe wc-desk.exe; do
    [ -f "$WIN_BIN/$e" ] || {
      echo "error: no $WIN_BIN/$e -- cargo build --workspace --target x86_64-pc-windows-gnu"
      exit 1; }
  done
  # `wc-arena-host` is its own workspace — it links shared_vulkan's crates and
  # deliberately inherits nothing from this one — so its binary is not in the
  # shared target directory and looking for it there is how a deploy silently
  # ships the last run's copy.
  local arena_host="$ROOT/crates/wc-arena-host/target/debug/wc-arena-host"
  [ -x "$LINUX_BIN/wc-proxy" ] && [ -x "$arena_host" ] || {
    echo "error: no wc-proxy / wc-arena-host -- cargo build --workspace and"
    echo "       cargo build --manifest-path crates/wc-arena-host/Cargo.toml"
    exit 1; }

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
  mkdir -p "$OUT"
}

# ------------------------------------------------------------------ run ------
# One leg. `$1` is the `--decorations` mode, and it is also the only difference
# between the two runs: same binaries, same arena, same client, same output
# size, minutes apart. The environment variable moves with it because the two
# levers are a pair (§19.4) and a run that changed one without the other would
# be the control rather than the result.
do_run() {
  local mode=$1
  echo
  echo "== $mode: Firefox on the guest's desktop, over the guest's own RAM =="
  [ -n "$HOST" ]         || { miss "WC_HOST unset: the proxy has to run where QEMU is"; return; }
  [ -n "$HOST_FIREFOX" ] || { miss "WC_HOST_FIREFOX unset: no Firefox path on $HOST"; return; }
  have magick            || { miss "no imagemagick here: nothing could be converted"; return; }

  local sock="wtb-$mode-$$"
  # A port of its own per leg. The two legs run minutes apart and a relay from
  # the first can still hold the port when the second binds: the second bind
  # then fails, the *first* relay accepts the connection, forwards it into a
  # socket node whose compositor has gone, and the client reports
  # `we don't have any display` — which reads as a client that never started and
  # is a stale listener. It cost one leg to find.
  local port=$PORT
  [ "$mode" = client ] && port=$((PORT + 1))
  local arena=$((ARENA_PAGES * 4096))
  local fxenv=""
  [ "$mode" = server ] && fxenv="MOZ_GTK_TITLEBAR_DECORATION=none"

  # Ours only, and before the share is emptied: an interrupted run leaves a
  # compositor alive for up to its `--timeout`, still writing into the same
  # directory, and a wipe that happens first leaves this run reading the last
  # one's log.
  ours "wtb-$mode-" >/dev/null 2>&1
  g "rm -f $APPDATA_WIN/wtb-$mode-* 2>/dev/null; true" >/dev/null 2>&1
  sleep 2
  h "rm -rf $SHARE/frames-$mode; mkdir -p $SHARE/frames-$mode; chmod 777 $SHARE/frames-$mode;
     rm -f $SHARE/tb-$mode.log" >/dev/null

  # ----- 1. the compositor, on WinSta0\Default, over guest RAM ---------------
  #
  # No `--shared-dir`: the keymap is cut out of the arena's outbound window
  # (§14), so nothing this compositor creates has a name on any filesystem. The
  # log and the frame dumps go to the export because this side has to read them
  # while the guest is busy; neither is a protocol object.
  int_bg "$GDIR_WIN\\wc-compositor.exe" \
    "--socket $sock --timeout $((SECS + 150)) --output $OUTPUT --decorations $mode \
--arena-guest-ram 0:$arena --arena-outbound 0:$ARENA_OUTBOUND \
--dump-frames $SHARE_WIN\\frames-$mode --log $SHARE_WIN\\tb-$mode.log" comp >/dev/null 2>&1

  local cpid="" t
  for t in 1 2 3 4 5 6 7 8; do
    sleep 4
    cpid=$(ours "$sock" -PidOnly | head -1)
    [ -n "$cpid" ] && break
  done
  [ -n "$cpid" ] || { bad "$mode: the compositor never started on the interactive desktop"; return; }
  note "$mode: our compositor is pid $cpid, socket $sock"

  # ----- 2. find the guest's pages from outside the guest --------------------
  #
  # §12.5's ordering: the guest allocates and stamps, and only then can the host
  # sweep for them. The run id appears in the log after the stamps are written,
  # so waiting for that line is waiting for the region to be findable — a sweep
  # started earlier returns zero pages and reads exactly like an arena that was
  # never shared.
  local line runid pages
  for t in $(seq 1 40); do
    line=$(h "grep -o 'arena 0 provided from guest RAM: run 0x[0-9a-f]*, [0-9]* bytes, [0-9]* pages, locked=[a-z]*' $SHARE/tb-$mode.log 2>/dev/null | tail -1" | tr -d '\r')
    [ -n "$line" ] && break
    sleep 1
  done
  if [ -z "$line" ]; then
    bad "$mode: the compositor never reported a guest-RAM arena"
    h "head -20 $SHARE/tb-$mode.log 2>/dev/null" | sed 's/^/        /'
    ours "$sock" >/dev/null 2>&1
    return
  fi
  runid=$(echo "$line" | grep -o 'run 0x[0-9a-f]*' | cut -d' ' -f2)
  pages=$(echo "$line" | grep -o '[0-9]* pages' | cut -d' ' -f1)
  note "$mode: $line"

  h "echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
$HOST_DIR/wc-arena-host --qmp '$QMP' --vm '$VM_HINT' --run $runid --pages $pages \
  --outbound $ARENA_OUTBOUND --plan '$HOST_DIR/arena-$mode.plan' >'$HOST_DIR/locate-$mode.log' 2>&1
echo rc=\\\$? >>'$HOST_DIR/locate-$mode.log'
chmod a+r '$HOST_DIR/locate-$mode.log' '$HOST_DIR/arena-$mode.plan' 2>/dev/null; true\" | $ROOTC" \
    >/dev/null 2>&1
  local verify
  verify=$(h "grep -o 'verify: [0-9]*/[0-9]* pages of the stitched view carry their own arena index' $HOST_DIR/locate-$mode.log 2>/dev/null" | tr -d '\r')
  if h "grep -q '^rc=0$' $HOST_DIR/locate-$mode.log" 2>/dev/null; then
    ok "$mode: the guest's arena was located in QEMU's backend — $verify"
  else
    bad "$mode: wc-arena-host did not locate the arena"
    h "tail -12 $HOST_DIR/locate-$mode.log 2>/dev/null" | sed 's/^/        /'
    ours "$sock" >/dev/null 2>&1
    return
  fi

  # ----- 3. the relay, on its own held connection ---------------------------
  ssh "${SSHOPT[@]}" "$GUEST" \
    "export MSYS_NO_PATHCONV=1; cd $TBNAME && ./probe-relay.exe $port '$APPDATA_WIN\\$sock' --timeout $((SECS + 60))" \
    >"$RUNDIR/relay-$mode.log" 2>&1 &
  local relay=$!
  sleep 3

  # ----- 4. the proxy and Firefox, as root, on the VM host -------------------
  #
  # Root because the proxy opens the backing object at `/proc/<qemu-pid>/fd/<n>`
  # (§12.9). Firefox refuses to run as root in somebody else's session — a real
  # check against a real mistake — so the unit gives it a home, a profile and an
  # `XDG_RUNTIME_DIR` of its own rather than arguing with it.
  h "cat > $HOST_DIR/tb-$mode.sh <<'EOS'
#!/usr/bin/env bash
export PATH=/run/current-system/sw/bin:/run/wrappers/bin
RUN=/tmp/$TBNAME-$mode-run
rm -rf \"\$RUN\"; mkdir -p \"\$RUN/home\" \"\$RUN/profile\" \"\$RUN/xdg\"
chmod 700 \"\$RUN/xdg\"
ssh -i $HOST_KEY -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR \
    -N -L \"\$RUN/up.sock:127.0.0.1:$port\" $HOST_GUEST >\"\$RUN/fwd.log\" 2>&1 &
FWD=\$!
sleep 5
if [ ! -S \"\$RUN/up.sock\" ]; then echo TB-FAIL: no forward socket >\"\$RUN/why\"; fi
XDG_RUNTIME_DIR=\"\$RUN\" $HOST_DIR/wc-proxy --upstream \"\$RUN/up.sock\" --socket wl-proxy \
  --runtime-dir \"\$RUN\" --transport arena --arena-plan $HOST_DIR/arena-$mode.plan \
  --arena-id 0 --copy damage --accept-timeout 30 --timeout $((SECS + 10)) \
  >\"\$RUN/proxy.log\" 2>&1 &
P=\$!
sleep 2
env -u DISPLAY HOME=\"\$RUN/home\" XDG_RUNTIME_DIR=\"\$RUN/xdg\" \
  WAYLAND_DISPLAY=\"\$RUN/wl-proxy\" MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland $fxenv \
  timeout $SECS $HOST_FIREFOX --no-remote --profile \"\$RUN/profile\" about:blank \
  >\"\$RUN/fx.log\" 2>&1
echo \"TB-CLIENT-RC: \$?\" >\"\$RUN/rc\"
sleep 2
kill \$P \$FWD 2>/dev/null; wait 2>/dev/null
chmod -R a+rwX \"\$RUN\" 2>/dev/null
touch \"\$RUN/done\"
EOS
chmod 755 $HOST_DIR/tb-$mode.sh" >/dev/null
  # `systemd-run` gives the unit systemd's PATH, which on NixOS has no `bash`
  # and no `env`, so a `#!/usr/bin/env bash` script exits 127 before its first
  # line with nothing but `env: 'bash': No such file or directory` in the
  # journal. Naming the interpreter is the fix.
  h "echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
systemd-run --unit=$TBNAME-$mode --collect /run/current-system/sw/bin/bash \
  $HOST_DIR/tb-$mode.sh >/dev/null 2>&1; echo started\" | $ROOTC" >/dev/null 2>&1

  # ----- 5. photograph it while it is up -------------------------------------
  #
  # `PrintWindow(PW_RENDERFULLCONTENT)`, which reads the window's DWM
  # redirection surface rather than the framebuffer — so it is the real window
  # drawn by the real theme at the real DPI, and it works while a lock screen
  # sits on top of everything (`wc-desktop.sh`'s header has the whole argument).
  # By **pid**, because another worktree's compositor registers the same class.
  sleep 35
  int "cmd.exe" \
    "/c $GDIR_WIN\\wc-desk.exe shot --pid $cpid --class WaycomposeToplevel --out $GDIR_WIN\\tb-$mode.bmp > $GDIR_WIN\\shot-$mode.txt 2>&1" \
    shot >/dev/null 2>&1
  int "cmd.exe" \
    "/c $GDIR_WIN\\wc-desk.exe windows > $GDIR_WIN\\windows-$mode.txt 2>&1" wins >/dev/null 2>&1
  g "cat $GDIR/shot-$mode.txt" >"$RUNDIR/shot-$mode.txt" 2>&1
  sed 's/^/        /' "$RUNDIR/shot-$mode.txt"

  scp "${SSHOPT[@]}" "$GUEST:$TBNAME/tb-$mode.bmp" "$RUNDIR/tb-$mode.bmp" >/dev/null 2>&1
  mkdir -p "$OUT"
  if [ -s "$RUNDIR/tb-$mode.bmp" ] && magick "$RUNDIR/tb-$mode.bmp" "$OUT/stage14-$mode.png" 2>/dev/null; then
    ok "$mode: the window, frame and all -> docs/stage14/stage14-$mode.png ($(magick identify -format '%wx%h' "$OUT/stage14-$mode.png"))"
  else
    bad "$mode: nothing came back from the capture"
  fi

  # ----- 6. what the run says about itself -----------------------------------
  h "test -f /tmp/$TBNAME-$mode-run/done && echo done" >/dev/null 2>&1
  for t in $(seq 1 $((SECS + 40))); do
    h "test -f /tmp/$TBNAME-$mode-run/done && echo yes" 2>/dev/null | grep -q yes && break
    sleep 2
  done
  h "cat /tmp/$TBNAME-$mode-run/proxy.log 2>/dev/null" >"$RUNDIR/proxy-$mode.log"
  h "cat $SHARE/tb-$mode.log 2>/dev/null" >"$RUNDIR/comp-$mode.log"
  cp "$RUNDIR/comp-$mode.log" "$OUT/compositor-$mode.log" 2>/dev/null

  grep -q "decorations: manager default is ${mode^}" "$RUNDIR/comp-$mode.log" \
    && ok "$mode: the compositor advertised ${mode^} as the manager default" \
    || bad "$mode: the compositor's log does not say which mode it advertised"

  local asked
  asked=$(grep -o 'kde-decoration: client asked for Value([A-Za-z]*)' "$RUNDIR/comp-$mode.log" \
          | tail -1 | grep -o 'Value([A-Za-z]*)')
  case "$mode:$asked" in
    "server:Value(Server)") ok "$mode: Firefox asked for Server, on real Windows" ;;
    "client:Value(Client)") ok "$mode: Firefox asked for Client, on real Windows" ;;
    *) bad "$mode: Firefox asked for ${asked:-nothing}" ;;
  esac

  local mapped
  mapped=$(grep -o 'window 1 mapped [0-9]*x[0-9]*' "$RUNDIR/comp-$mode.log" | tail -1)
  [ -n "$mapped" ] \
    && ok "$mode: a real HWND, sized from the geometry rectangle: $mapped" \
    || bad "$mode: the window never mapped"

  local counts
  counts=$(grep -o 'mapped: [0-9]* section, [0-9]* shared-file, [0-9]* arena-slice' "$RUNDIR/comp-$mode.log" | tail -1)
  case "$counts" in
    "mapped: 0 section, 0 shared-file, "[1-9]*) ok "$mode: every pixel came out of guest RAM ($counts)" ;;
    *) bad "$mode: wrong transport served the pixels: ${counts:-nothing recorded}" ;;
  esac

  local presents
  presents=$(grep -o '([0-9]* frames presented)' "$RUNDIR/comp-$mode.log" | grep -o '[0-9]*' | tail -1)
  [ "${presents:-0}" -ge 2 ] \
    && ok "$mode: $presents frames of Firefox's pixels were blitted into it" \
    || note "$mode: ${presents:-0} frames presented (the count is printed when the toplevel goes away)"

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

case "$MODE" in
  deploy) do_deploy ;;
  server) do_run server ;;
  client) do_run client ;;
  both)   do_deploy; do_run server; do_run client ;;
  clean)  do_clean ;;
  *) echo "usage: $0 [deploy|server|client|both|clean]"; exit 2 ;;
esac

echo
echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
