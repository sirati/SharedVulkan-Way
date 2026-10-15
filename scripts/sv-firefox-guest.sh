#!/usr/bin/env bash
#
# **Firefox on the Linux host, rendering on the guest's GPU, displayed on the
# guest's desktop.** Both projects at once, on testhost, run *on* testhost.
#
#   scripts/sv-firefox-guest.sh [sessions] [secs] [soft|zink]
#
# The two halves have each been proven and never together:
#
# * shared_vulkan reaches the guest's Intel iGPU from a host with no GPU
#   (`sv-guest-host.sh`, and the commit that added it);
# * waycompose puts a Firefox window from the Linux host onto the Windows
#   guest's interactive desktop over an arena in guest RAM (its docs/waycompose-design.md §17).
#
# This is one process tree containing both. Firefox's *pixels* come back through
# waycompose's arena; Firefox's *rendering* goes out through shared_vulkan's.
#
# # The topology, and why every piece is where it is
#
#   guest   wc-compositor.exe        commits a 64 MiB guest-RAM arena, stamps it
#           probe-relay.exe          TCP <-> the compositor's AF_UNIX socket
#           sv-gpu-server.exe guest  N x 80 MiB of guest RAM, N GPU sessions
#   host    wc-arena-host            finds the compositor's arena in QEMU's memfd
#           sv-hostd qemu-serve      finds the N GPU arenas, serves them by name
#           wc-proxy                 Wayland server to Firefox, client of the guest
#           firefox                  Zink -> sv-icd -> the guest's iGPU
#
# Everything privileged is privileged for one reason: QEMU is a **system**
# service here, so its QMP monitor and its `/proc/<pid>/fd` are root-only, and
# so is anything that opens the object `sv-hostd` names. That is waycompose's
# §12.9 and it is a property of this deployment rather than of either design.
#
# # Two things that will bite, both paid for
#
# * **QEMU's QMP chardev serves one client.** `wc-arena-host` and `sv-hostd`
#   both use it, so they are ordered rather than concurrent, and anything left
#   over from a previous run is killed first — by *path*, never by image name,
#   because other experiments share this machine. A run that skips this stops
#   dead after "the guest offers run 0x..." with no error at all.
# * **Zink is forced, and that is the measurement.** This host has no
#   `/dev/dri`, no `/run/opengl-driver`, and therefore no GL at all until Mesa
#   is brought in. With Mesa present and no override, EGL loads **llvmpipe** —
#   silently, reporting GL 4.6. The `soft` mode below is that, on purpose, as
#   the control; `zink` is the run. If Zink cannot reach a device it fails
#   loudly (`eglInitialize failed`) rather than falling back, which is the one
#   property that makes a green result mean anything.
#
# ---------------------------------------------------------------------------
# BOTH SIDES MUST COME FROM ONE BUILD. The GPU side here is a binary *copied*
# into the guest, so rebuilding on the host changes only the host's half.
#
# `WIRE_VERSION` went 1 -> 2 in docs/design.md §0.18 (`svDriverVersions`, which is
# hand-written in `sv_proto::sv` and so outside the protocol hash). A guest
# binary from before that commit is refused at handshake — correctly, and by
# design. The ICD names the cause and the fix in the refusal message; the
# reason it is also written here is that this project has twice read a stale
# deployed binary as a transport fault and lost an afternoon to it.
# ---------------------------------------------------------------------------
set -uo pipefail

SESSIONS=${1:-4}
SECS=${2:-75}
MODE=${3:-zink}

# Every identity below is a knob, and not for taste. This machine runs a dozen
# experiments at once and each of the four is something one run can destroy for
# another: the host directory holds the binaries, the guest directory is what
# the cleanup step kills processes *by prefix* of, the share is where the
# compositor's log and frames land, and the ports are bound. A second experiment
# that wants to run beside this one overrides the four and collides with
# nothing.
DIR=${SVFF_DIR:-/tmp/svff-int}
GDIR=${SVFF_GDIR:-svffint}
SHARE=${SVFF_SHARE:-/srv/winshare/svff}
SHARE_WIN=${SVFF_SHARE_WIN:-'Z:\svff'}
G=${SVFF_GUEST:-<guest-user>@<guest-nat-ip>}
GK=(-i "$HOME/.ssh/wc_guest" -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR)
# Root, for sv-hostd's locate and nothing else (waycompose-design.md §86). The
# old default, ~/rootc, is gone from testhost; `sudokey` is how root is
# reached there now, through the forwarded 1Password agent (`ssh -A
# testhost-ts`).
ROOTC=${SVFF_ROOTC:-sudokey run -- bash}
# The compositor's arena. **ivshmem by default** (waycompose-design.md §83):
# BAR2 of the VM's ivshmem device, which the host opens as this user through a
# file, so the compositor needs no locate, no root and no guest RAM under the
# host's writes. `guest-ram` is the old path, for a guest without the device;
# it no longer passes `--arena-frames` (§84.7: no PFN scans on the real guest).
ARENA_KIND=${SVFF_ARENA:-ivshmem}
IVSHMEM_FILE=${SVFF_IVSHMEM_FILE:-/run/windows-vm/wc-arena/arena}
# Who the application runs as. Since §86 `sv-hostd --drop-to` makes the GPU
# sessions openable by this user, so Firefox is not root any more.
ME_UID=$(id -u); ME_GID=$(id -g)
BASH_BIN=$(command -v bash)
# The systemd unit names, the run directory and the compositor's socket name.
# Globbed in the cleanup steps, which is the reason it is a knob rather than a
# literal: a `systemctl stop 'svff-run-*'` from one experiment stops another
# experiment's run, and the symptom lands in theirs rather than in this one.
UNITPFX=${SVFF_UNIT_PREFIX:-svff}
BASE=${SVFF_BASE_PORT:-57360}
WLPORT=${SVFF_WL_PORT:-57359}
ARENA_PAGES=${SVFF_ARENA_PAGES:-196608}   # the compositor's arena, 768 MiB by default:
                                  # 64 MiB fragments and a YouTube frame cannot be
                                  # published ("arena 0 has no run of 2342912"), which
                                  # kills Firefox instantly. Someone else owns the
                                  # allocator fix; this is the knob until it lands.
ARENA_OUTBOUND=1048576            # what it keeps for the objects it creates
GPU_ARENA=$((64 << 20))           # each GPU session's arena
OUTPUT=1600x1000                  # so Firefox's window is 1280x900, as in §15.6
URL=${SVFF_URL:-about:support}

MESA=${SVFF_MESA:-/nix/store/svxxdnik6q27kjw1rmsn9x89mwi7xhck-mesa-26.2.0}
MESA_DEMOS=${SVFF_DEMOS:-/nix/store/29kh8r89yqgg1ndmfybh1wafjzichp2g-mesa-demos-9.0.0}
GLVND=${SVFF_GLVND:-/nix/store/bcm3mwyfja8rd2y995vcnzq06b341nnz-libglvnd-1.7.0}
VKLOADER=${SVFF_VKLOADER:-/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0}
# Not on this machine's PATH -- it has no `firefox` at all -- so an absolute
# store path, and a `firefox` that resolved to something else would be a
# different measurement. waycompose made the same choice for the same reason.
FIREFOX=${SVFF_FIREFOX:-/nix/store/qxgg26945r3f9yzdqg0gfbnjvg6wm7x5-firefox-151.0.2/bin/firefox}
# Only the `x11` mode uses these. The display number is this experiment's own,
# for the same reason every other identity here is: the machine is shared.
XWAYLAND=${SVFF_XWAYLAND:-/nix/store/8ynkvnr05j3pbaq4czb76yxnk940cmhm-xwayland-24.1.13/bin/Xwayland}
XDISPLAY=${SVFF_XDISPLAY:-:77}

say() { echo "[$(date +%H:%M:%S)] $*"; }
KIDS=()
cleanup() {
  kill "${KIDS[@]}" 2>/dev/null
  # And this experiment's processes in the guest, by executable path, as the
  # start of the next run would. Without it the compositor and the GPU sides
  # lived on to their own timeouts after every run — up to four minutes of
  # pinned large pages and a held ivshmem mapping on a machine somebody uses.
  ssh "${GK[@]}" "$G" "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -Command \
     \"\\\$p = 'C:\\\\Users\\\\<guest-user>\\\\$GDIR\\\\'; Get-CimInstance Win32_Process | \
      Where-Object { \\\$_.ExecutablePath -and \\\$_.ExecutablePath.StartsWith(\\\$p) } | \
      ForEach-Object { Stop-Process -Id \\\$_.ProcessId -Force -ErrorAction SilentlyContinue }\"" \
    >/dev/null 2>&1
  echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
pkill -f '$DIR/sv-hostd' 2>/dev/null
systemctl stop '$UNITPFX-run-*' '$UNITPFX-hostd-*' 2>/dev/null; systemctl reset-failed '$UNITPFX-*' 2>/dev/null
true" | eval "$ROOTC" >/dev/null 2>&1
}
trap cleanup EXIT INT TERM

RUN=$(mktemp -d /tmp/$UNITPFX-run-XXXXXX); chmod 755 "$RUN"
mkdir -p "$RUN/profile" "$RUN/home" "$RUN/xdg"
SOCK="$UNITPFX-$$"

# Firefox's own prefs. Hardware WebRender, and the software fallback switched
# off: the brief for this work is that a CPU GL implementation is not an
# acceptable outcome *even as a fallback that silently engages*, and the way to
# hold that is to make the fallback unavailable rather than to check afterwards
# whether it fired.
cat >"$RUN/profile/user.js" <<'PREFS'
user_pref("gfx.webrender.all", true);
user_pref("gfx.webrender.software", false);
user_pref("webgl.force-enabled", true);
user_pref("webgl.disable-fail-if-major-performance-caveat", true);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
PREFS

# `about:support`'s answer is a long way down the page and nothing here can
# scroll: there is no window manager and no route for a keystroke. Shrinking the
# CSS pixel is the one knob that brings the Graphics table onto the first screen
# without touching the graphics stack at all — it changes how many CSS pixels
# fit, not which renderer draws them. Off by default so the ordinary run is the
# ordinary run.
[ -n "${SVFF_DPPX:-}" ] && echo "user_pref(\"layout.css.devPixelsPerPx\", \"$SVFF_DPPX\");" >>"$RUN/profile/user.js"
# More prefs, `name=value;name=value`, values written as JavaScript literals
# (`true`, `5`, `"text"`). The first use was `media.cubeb.force_null_context=true`:
# this harness has no audio server, and design.md §0.28.9 measured about:support
# stalling on that in 6 of 22 runs.
if [ -n "${SVFF_PREFS:-}" ]; then
  IFS=';' read -ra _prefs <<<"$SVFF_PREFS"
  for kv in "${_prefs[@]}"; do
    [ -n "$kv" ] && echo "user_pref(\"${kv%%=*}\", ${kv#*=});" >>"$RUN/profile/user.js"
  done
fi

# ------------------------------------------------------------------ clean ---
say "clearing what this experiment left behind (by path, never by image name)"
echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
pkill -f '$DIR/sv-hostd' 2>/dev/null
systemctl stop '$UNITPFX-run-*' '$UNITPFX-hostd-*' '$UNITPFX-app-*' 2>/dev/null
systemctl reset-failed '$UNITPFX-*' 2>/dev/null
rm -rf $SHARE; mkdir -p $SHARE/frames; chmod -R 777 $SHARE
rm -f $DIR/icd.sock
true" | eval "$ROOTC" >/dev/null 2>&1
rm -f "$DIR"/cold*.sock "$DIR"/*.log
# Only sockets this experiment made. §17's rule: identity is a path, not a name.
ssh "${GK[@]}" "$G" "rm -f /c/Users/<guest-user>/AppData/Local/waycompose/$UNITPFX-* 2>/dev/null; true" >/dev/null 2>&1

# **Kill this experiment's own leftovers in the guest, by executable path.**
#
# A previous run's `sv-gpu-server.exe` outlives the ssh session that started it
# for long enough to still hold its TCP port, and the next run dies with
# "Normalerweise darf jede Socketadresse ... nur jeweils einmal verwendet
# werden" (WSAEADDRINUSE) — which reads like a firewall and is a leftover.
#
# The prefix is matched with a trailing backslash so that `svffint` cannot match
# `svffint2`, and it is an **ExecutablePath** rather than an image name:
# `taskkill /IM sv-gpu-server.exe` would take out any other experiment's GPU
# side, and the symptom would land in their run rather than in this one.
ssh "${GK[@]}" "$G" "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -Command \
   \"\\\$p = 'C:\\\\Users\\\\<guest-user>\\\\$GDIR\\\\'; Get-CimInstance Win32_Process | \
    Where-Object { \\\$_.ExecutablePath -and \\\$_.ExecutablePath.StartsWith(\\\$p) } | \
    ForEach-Object { Stop-Process -Id \\\$_.ProcessId -Force -ErrorAction SilentlyContinue }\"" \
  >/dev/null 2>&1
sleep 2

# ------------------------------------------------------------- compositor ---
COMP_SECS=$((SECS + 150))
if [ "$ARENA_KIND" = ivshmem ]; then
  COMP_ARENA="--arena-ivshmem 0 --arena-outbound '0:$ARENA_OUTBOUND'"
else
  COMP_ARENA="--arena-guest-ram '0:$((ARENA_PAGES * 4096))' --arena-outbound '0:$ARENA_OUTBOUND'"
fi
# Frames are dumped only when asked (`SVFF_DUMP=1`): a 1280x900 PPM per commit
# into the share is 3.5 MB each, and on the real guest that is disk traffic on
# somebody's machine for a picture nobody looks at.
DUMP=""; [ -n "${SVFF_DUMP:-}" ] && DUMP="--dump-frames '$SHARE_WIN\\frames'"
say "starting the compositor in the guest ($ARENA_KIND arena, $OUTPUT output)"
ssh "${GK[@]}" "$G" "export MSYS_NO_PATHCONV=1; cd $GDIR && ./sv-guest.exe wayland \
  --socket $SOCK --timeout $COMP_SECS $DUMP \
  --log '$SHARE_WIN\\s.log' --output $OUTPUT $COMP_ARENA" \
  >"$DIR/comp.out" 2>&1 &
KIDS+=($!)

# ------------------------------------------------------- the GPU sessions ---
# Started now so their 2.4-second sweeps overlap the compositor coming up. Each
# is its own process on its own port: one pinned window, one extent, one control
# region per session, which is what §0.12 established and what Firefox needs
# because it forks `glxtest` before the browser exists.
say "starting $SESSIONS GPU side(s) in the guest"
for i in $(seq 0 $((SESSIONS - 1))); do
  ssh "${GK[@]}" "$G" "cd $GDIR && ${SVFF_GPU_ENV:+env $SVFF_GPU_ENV} ./sv-guest.exe gpu guest --port $((BASE + i)) \
    --arena $GPU_ARENA --idle $((SECS + 120)) 2>&1" </dev/null >"$DIR/gpu$i.log" 2>&1 &
  KIDS+=($!)
done

# ------------------------------------------- locate the compositor's arena ---
# IVSHMEM: nothing to locate. Wait for the line the compositor prints after it
# has published the header, then check the header's magic and run id in the
# file exactly as `wc-session up` does.
if [ "$ARENA_KIND" = ivshmem ]; then
  RUNID=""
  for _ in $(seq 1 60); do
    RUNID=$(grep -o "header published: run 0x[0-9a-f]*" "$SHARE/s.log" 2>/dev/null | tail -1 | grep -o "0x[0-9a-f]*")
    [ -n "$RUNID" ] && break
    sleep 1
  done
  [ -n "$RUNID" ] || { echo "FAIL: the compositor never published an ivshmem header"; sed -n '1,20p' "$SHARE/s.log" 2>/dev/null; cat "$DIR/comp.out"; exit 1; }
  HAVE=$(od -A n -t x8 -j 16 -N 8 "$IVSHMEM_FILE" | tr -d ' ')
  [ "$(head -c 8 "$IVSHMEM_FILE" | tr -d '\0')" = WCIVSHM1 ] && [ "0x$HAVE" = "$(printf '0x%016x' "$RUNID")" ] \
    || { echo "FAIL: $IVSHMEM_FILE describes run 0x$HAVE, the compositor said $RUNID"; exit 1; }
  say "compositor arena: ivshmem, run $RUNID, header checked in $IVSHMEM_FILE"
else
# The compositor prints its run id *after* the stamps are written, so waiting
# for that line is waiting for the region to be findable. A sweep started
# earlier returns zero pages, which reads exactly like the arena not being
# shared at all.
RUNID=""; FOUNDPAGES=""
for _ in $(seq 1 60); do
  line=$(grep -o "arena 0 provided from guest RAM: run 0x[0-9a-f]*, [0-9]* bytes, [0-9]* pages, locked=[a-z]*" "$SHARE/s.log" 2>/dev/null | tail -1)
  if [ -n "$line" ]; then
    RUNID=$(echo "$line" | grep -o "run 0x[0-9a-f]*" | cut -d' ' -f2)
    FOUNDPAGES=$(echo "$line" | grep -o "[0-9]* pages" | head -1 | cut -d' ' -f1)
    say "compositor arena: $line"
    break
  fi
  sleep 1
done
[ -n "$RUNID" ] || { echo "FAIL: the compositor never reported a guest-RAM arena"; sed -n '1,20p' "$SHARE/s.log" 2>/dev/null; cat "$DIR/comp.out"; exit 1; }

# The frame list the compositor wrote into the share. Translating it is 0.064 s
# where the content sweep is 2.308 s, and — the reason it is wired here rather
# than left as an optimisation — it reads no stamps, so the `page index claimed
# twice` failure that made five consecutive guest runs unusable cannot happen.
# Conditional, because a guest without the query is a slow run and not a broken
# one; the locate line says which locator ran.
FRAMES_ARG=""
if [ -s "$SHARE/arena.frames" ]; then
  FRAMES_ARG="--frames '$SHARE/arena.frames'"
  say "locating by frame list ($(wc -l <"$SHARE/arena.frames") frames from the guest)"
else
  say "no frame list from the guest; falling back to the content sweep"
fi
say "locating the compositor's arena in QEMU's memory"
echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
$DIR/wc-arena-host --qmp /run/windows-vm/qmp.sock --vm windows-vm --run $RUNID \
  --pages $FOUNDPAGES --outbound $ARENA_OUTBOUND $FRAMES_ARG --plan '$RUN/arena.plan' >'$RUN/locate.log' 2>&1
echo \"rc=\$?\" >>'$RUN/locate.log'
chmod a+r '$RUN/locate.log' '$RUN/arena.plan' 2>/dev/null; true" | eval "$ROOTC" >/dev/null 2>&1
sed 's/^/  /' "$RUN/locate.log" 2>/dev/null
grep -q "^rc=0$" "$RUN/locate.log" 2>/dev/null || { echo "FAIL: wc-arena-host did not locate the compositor's arena"; exit 1; }
fi

# ------------------------------------------------ the Wayland byte channel ---
say "relay + ssh -L for the Wayland socket"
ssh "${GK[@]}" "$G" "export MSYS_NO_PATHCONV=1; cd $GDIR && ./probe-relay.exe $WLPORT \
  'C:\\Users\\<guest-user>\\AppData\\Local\\waycompose\\$SOCK' --timeout $((SECS + 90))" \
  >"$DIR/relay.out" 2>&1 &
KIDS+=($!)
sleep 4
ssh "${GK[@]}" -N -L "$RUN/up.sock:127.0.0.1:$WLPORT" "$G" >"$DIR/fwd-wl.log" 2>&1 &
KIDS+=($!)

# ------------------------------------------------------- the cold channels ---
COLD=()
for i in $(seq 0 $((SESSIONS - 1))); do
  s="$DIR/cold$i.sock"; rm -f "$s"
  ssh "${GK[@]}" -N -L "$s:127.0.0.1:$((BASE + i))" "$G" >"$DIR/fwd$i.log" 2>&1 &
  KIDS+=($!)
  COLD+=(--cold "$s")
done
sleep 5
[ -S "$RUN/up.sock" ] || { echo "FAIL: the Wayland forward produced no socket"; cat "$DIR/fwd-wl.log"; exit 1; }
for i in $(seq 0 $((SESSIONS - 1))); do
  [ -S "$DIR/cold$i.sock" ] || { echo "FAIL: no cold socket $i"; cat "$DIR/fwd$i.log"; exit 1; }
  grep -q "publishing the extent" "$DIR/gpu$i.log" 2>/dev/null \
    || { echo "FAIL: GPU side $i never came up"; sed 's/^/    /' "$DIR/gpu$i.log"; exit 1; }
done
say "$SESSIONS cold path(s) and the Wayland path are up"

# ------------------------------------- sv-hostd, then the proxy and Firefox --
# Ordered after `wc-arena-host` because both want QMP and QMP serves one client.
UNIT="$UNITPFX-run-$$"

# The one line that is the whole experiment, and its control. Chosen *before*
# the script that uses it is written, so that the two modes are one command line
# with one variable in it rather than two command lines that can drift.
if [ "$MODE" = soft ]; then
  GLENV='LIBGL_ALWAYS_SOFTWARE=1'
else
  GLENV="${SVFF_GLENV:-MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink} VK_DRIVER_FILES=$DIR/sv_icd.json SV_SOCKET=$DIR/icd.sock"
fi
# Anything else the run wants in front of the client, so that an experiment
# about one environment variable does not need an edit to this file.
EXTRA=${SVFF_EXTRA_ENV:-}

# **Zink on the Wayland platform needs a Mesa that allows it** (docs/design.md
# §0.28). `wc-proxy` offers `wl_shm` and no DRM device, and stock Mesa then loads
# llvmpipe on the Wayland platform whatever this ICD offers. `SVFF_KOPPER_MESA`
# is the store path of `nix/mesa-kopper-wayland.nix` — this host's own Mesa plus
# the override platform_x11.c already reads — built on this host against the
# nixpkgs its Firefox comes from (the command is in docs/design.md §0.28).
# Unset, the run is what it always was. Not used by `x11`, which has kopper on
# stock Mesa already.
KOPPER_MESA=${SVFF_KOPPER_MESA:-}
KOPPER_ENV=""
if [ -n "$KOPPER_MESA" ] && [ "$MODE" != soft ] && [ "$MODE" != x11 ]; then
  KOPPER_ENV="__EGL_VENDOR_LIBRARY_FILENAMES=$KOPPER_MESA/share/glvnd/egl_vendor.d/50_mesa.json LIBGL_KOPPER_DRI2=1"
fi

# `probe` runs eglinfo on the **Wayland** platform instead of Firefox, against
# the same proxy, in the same unit, with the same environment. It exists because
# "Failed to create EGLConfig for WebRender" is Firefox reporting somebody
# else's failure three layers up, and eglinfo says which layer.
if [ "$MODE" = probe ]; then
  CLIENTCMD="${SVFF_PROBE_CMD:-$MESA_DEMOS/bin/eglinfo -B}"
  GLENV="MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=$DIR/sv_icd.json SV_SOCKET=$DIR/icd.sock EGL_PLATFORM=wayland"
elif [ "$MODE" = x11 ]; then
  # **The cheap path**, and the reason it is in the same file as the expensive
  # one: X11 in the middle of a Wayland stack is a step sideways, but the ICD
  # already has `VK_KHR_xlib_surface` and `VK_KHR_xcb_surface`, so Firefox on
  # X11 needs no driver change at all. A *rootful* Xwayland — one `wl_surface`
  # carrying the whole X screen — is what makes this one window on the guest's
  # desktop rather than a rootless server with no window manager to map
  # anything. Its own presentation to the compositor is `wl_shm`, which is what
  # `wc-proxy` offers and all it can offer.
  cat >"$RUN/x11.sh" <<X11
#!/usr/bin/env bash
export XAUTHORITY="\$XDG_RUNTIME_DIR/Xauth"; : >"\$XAUTHORITY"
# **Xwayland must not inherit the client's GL environment**, and getting that
# wrong costs a whole run. The X server is itself a GL program — glamor is how
# it has a GLX extension at all — so \`GALLIUM_DRIVER=zink\` and
# \`VK_DRIVER_FILES\` reach it too, and an X server that cannot initialise its
# own GL offers no fbconfig to anybody. The client's environment is set on the
# client's line below; the server gets the software one.
env -u GALLIUM_DRIVER -u MESA_LOADER_DRIVER_OVERRIDE -u VK_DRIVER_FILES -u SV_SOCKET \\
  -u EGL_PLATFORM LIBGL_ALWAYS_SOFTWARE=1 \\
  "$XWAYLAND" $XDISPLAY -geometry $OUTPUT -noreset >"$RUN/xwayland.log" 2>&1 &
XW=\$!
for _ in \$(seq 1 60); do [ -S /tmp/.X11-unix/X${XDISPLAY#:} ] && break; sleep 0.5; done
if [ ! -S /tmp/.X11-unix/X${XDISPLAY#:} ]; then echo "Xwayland never listened"; exit 1; fi
# **LIBGL_KOPPER_DRI2=1 is not a tuning knob, it is the whole mode.**
# Mesa's platform_x11.c refuses Zink outright when the X server has no DRI3
# multibuffer support — which an Xwayland with no DRM device never has —
# unless this is set, in which case it skips the render-device checks and goes
# straight to dri2_initialize_x11_kopper. Without it the run reports "DRI3 not
# available" and "couldn't find RGB GLX visual or fbconfig" and never reaches
# the ICD at all, which is what the first three attempts at this mode measured.
DISPLAY=$XDISPLAY EGL_PLATFORM=x11 MOZ_ENABLE_WAYLAND=0 GDK_BACKEND=x11 \\
  LIBGL_KOPPER_DRI2=${SVFF_KOPPER_DRI2:-1} \\
  ${SVFF_X11_CMD:-"$FIREFOX" --no-remote --profile "$RUN/profile" $URL}
rc=\$?
kill \$XW 2>/dev/null
exit \$rc
X11
  chmod 755 "$RUN/x11.sh"
  CLIENTCMD="$RUN/x11.sh"
else
  CLIENTCMD="$FIREFOX --no-remote --profile $RUN/profile $URL"
fi
GLENV="$GLENV${KOPPER_ENV:+ $KOPPER_ENV}"
say "mode=$MODE, GL env: $GLENV ${EXTRA:+(extra: $EXTRA)}"

# **Root for sv-hostd's locate, and for nothing after it** (waycompose-design.md
# §86). Until §86 this whole block ran as one root unit, Firefox included,
# because the only name sv-hostd could give the ICD was QEMU's own
# `/proc/<qemu-pid>/fd/<n>`. `--drop-to` makes sv-hostd become this user once
# the locate is done and hand out its own descriptor instead, so the unit below
# is root for a few seconds and the proxy and Firefox never are.
HOSTD_UNIT="$UNITPFX-hostd-$$"
rm -f "$DIR/icd.sock" "$DIR/hostd.log"
echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
systemd-run --unit=$HOSTD_UNIT --collect -p StandardOutput=file:$DIR/hostd.log -p StandardError=file:$DIR/hostd.log \
  '$DIR/sv-hostd' qemu-serve --qmp /run/windows-vm/qmp.sock --vm windows-vm \
  ${COLD[*]} --socket '$DIR/icd.sock' --clients $SESSIONS --drop-to $ME_UID:$ME_GID >/dev/null 2>&1
echo started" | eval "$ROOTC" >/dev/null 2>&1
if [ "$MODE" != soft ]; then
  for _ in $(seq 1 200); do [ -S "$DIR/icd.sock" ] && break; sleep 1; done
  if [ ! -S "$DIR/icd.sock" ]; then
    echo "FAIL: the GPU sessions were never offered"
    sed 's/^/  /' "$DIR/hostd.log" 2>/dev/null | tail -20
    exit 1
  fi
  say "sv-hostd offers $SESSIONS session(s) on $DIR/icd.sock, owned by $(stat -c %U "$DIR/icd.sock")"
fi

# The proxy, as this user. ivshmem: the file the VM's unit made, opened through
# its group or ACL; no plan, no root.
if [ "$ARENA_KIND" = ivshmem ]; then
  PROXY_ARENA=(--arena-ivshmem "$IVSHMEM_FILE" --arena-run "$RUNID")
else
  PROXY_ARENA=(--arena-plan "$RUN/arena.plan")
fi
"$DIR/wc-proxy" --upstream "$RUN/up.sock" --socket wl-proxy \
  --runtime-dir "$RUN" --transport arena "${PROXY_ARENA[@]}" --arena-id 0 \
  --copy damage --accept-timeout 30 --timeout $((SECS + 15)) >"$RUN/proxy.log" 2>&1 &
P=$!
KIDS+=($P)
for _ in $(seq 1 100); do [ -S "$RUN/wl-proxy" ] && break; sleep 0.1; done
[ -S "$RUN/wl-proxy" ] || { echo "FAIL: wc-proxy never bound its socket"; tail -20 "$RUN/proxy.log"; exit 1; }
chmod 700 "$RUN/xdg"

# env -u DISPLAY because with an X server reachable MOZ_ENABLE_WAYLAND is a
# *preference* and Firefox will quietly use X11 instead.
# shellcheck disable=SC2086
env -u DISPLAY \
  HOME="$RUN/home" XDG_RUNTIME_DIR="$RUN/xdg" \
  WAYLAND_DISPLAY="$RUN/wl-proxy" MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland \
  LD_LIBRARY_PATH="$MESA/lib:$GLVND/lib:$VKLOADER/lib" \
  LIBGL_DRIVERS_PATH="$MESA/lib/dri" \
  __EGL_VENDOR_LIBRARY_DIRS="$MESA/share/glvnd/egl_vendor.d" \
  $GLENV $EXTRA \
  timeout $SECS $CLIENTCMD \
  >"$RUN/ff.log" 2>&1
echo "firefox rc=$?" >"$RUN/ff.rc"
sleep 3
kill $P 2>/dev/null; wait $P 2>/dev/null
touch "$RUN/hostside.done"
UNIT=$HOSTD_UNIT


# ---------------------------------------------------------------- report ----
say "---- sv-hostd ----";  sed 's/^/  /' "$DIR/hostd.log" 2>/dev/null | tail -20
say "---- wc-proxy ----";  sed 's/^/  /' "$RUN/proxy.log" 2>/dev/null | tail -12
say "---- firefox ----"
# Kept where the next run can read it without knowing this run's random
# directory. The tail below is a summary; this is the whole thing.
cp "$RUN/ff.log" "$DIR/client.log" 2>/dev/null; chmod a+r "$DIR/client.log" 2>/dev/null
[ -f "$RUN/ff.rc" ] && cat "$RUN/ff.rc"
grep -viE "gdk_seat_get_keyboard|GDK_IS_SEAT|^$" "$RUN/ff.log" 2>/dev/null | tail -20 | sed 's/^/  /'
[ -f "$RUN/hostside.err" ] && sed 's/^/  /' "$RUN/hostside.err"
if [ ! -f "$RUN/hostside.done" ]; then
  echo "FAIL: the host-side unit never finished"
  echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
journalctl -u $UNIT --no-pager 2>&1 | tail -20" | eval "$ROOTC" 2>&1 | sed 's/^/  unit: /'
fi

say "---- the compositor, inside the VM ----"
grep -iE "window 1 mapped|joins arena|pools|client [0-9]* connected" "$SHARE/s.log" 2>/dev/null | tail -12 | sed 's/^/  /'
LAST=$(ls "$SHARE"/frames/window1-*.ppm 2>/dev/null | tail -1)
if [ -n "$LAST" ]; then
  cp "$LAST" "$DIR/ff-last.ppm"; chmod a+r "$DIR/ff-last.ppm"
  say "kept the last composited frame at $DIR/ff-last.ppm ($(wc -c <"$LAST") bytes)"
else
  say "the compositor dumped no frame of window 1"
fi
exit 0
