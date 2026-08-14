#!/usr/bin/env bash
#
# The **cross-VM topology on testhost**, run on the machine QEMU lives on.
#
#   scripts/sv-guest-host.sh <sessions> <base-port> [gl|vk]
#
# This is `sv-guest-e2e.sh`'s shape with the guest swapped for the real one: a
# Windows guest that owns the GPU, and a Linux host that has none at all. It is
# the venue `docs/firefox-accel.md`'s last section says was never reached.
#
#   guest : sv-gpu-server.exe guest --port <tcp>
#           commits a window of *its own RAM*, pins it (`VirtualAlloc`, large
#           pages where the privilege allows), stamps every 4 KiB page, and
#           publishes an ExtentDesc down a loopback TCP listener.
#   host  : sv-hostd qemu-serve
#           reads `info mtree -f` over QMP, sweeps QEMU's 13 GiB memfd for the
#           stamp, coalesces, stitches, checks that extent offset n holds page
#           n, and then serves the object's *name* to ICD processes.
#   host  : the application, through the real loader and our ICD.
#
# # Why this script runs here and not from a workstation
#
# QEMU is a **system** service on this machine, so its QMP socket and its
# `/proc/<pid>/fd` are root-only, and so is every process that opens the object
# `sv-hostd` names — which is the application. waycompose reached the same
# conclusion for the same reason (its docs/waycompose-design.md §12.9) and both projects put the
# privileged half in a `systemd-run --collect` transient unit rather than a
# long-lived root shell: a root call that blocks for a minute frequently returns
# no output at all, and the unit's journal survives it.
#
# # The one thing to clean up, and what it looks like when you do not
#
# QEMU's QMP chardev serves **one** client. An `sv-hostd` left over from a
# previous run holds it, and the next run stops dead after
# "the guest offers run 0x…" with nothing further and no error — the locate
# never starts, because it cannot open the monitor. That cost a run here; the
# kill below is scoped to this directory's own binary, never to an image name,
# because other experiments share this machine.
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

SESSIONS=${1:-1}
BASE=${2:-57340}
MODE=${3:-gl}

DIR=/tmp/svff-int
G=${SVFF_GUEST:-<guest-user>@<guest-nat-ip>}
GK=(-i "$HOME/.ssh/wc_guest" -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR)
ROOTC=${SVFF_ROOTC:-$HOME/rootc}
BASH_BIN=$(command -v bash)

# The GL stack, brought rather than found. This machine has no
# `/run/opengl-driver` at all — `hardware.graphics` is off, which is what having
# no GPU looks like in a NixOS configuration — so Mesa, libglvnd and the Vulkan
# loader are `nix copy`ed in and named here. Naming them is also what makes the
# measurement repeatable: "the Mesa on the machine" is not a version.
MESA=${SVFF_MESA:-/nix/store/svxxdnik6q27kjw1rmsn9x89mwi7xhck-mesa-26.2.0}
DEMOS=${SVFF_DEMOS:-/nix/store/29kh8r89yqgg1ndmfybh1wafjzichp2g-mesa-demos-9.0.0}
GLVND=${SVFF_GLVND:-/nix/store/bcm3mwyfja8rd2y995vcnzq06b341nnz-libglvnd-1.7.0}
VKLOADER=${SVFF_VKLOADER:-/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0}

say() { echo "[$(date +%H:%M:%S)] $*"; }

# ------------------------------------------------------------------ clean ---
# Scoped to this directory's own binary. `pkill -f sv-hostd` would take out any
# other experiment's daemon, and the symptom would land in *their* run.
say "clearing anything this directory left behind"
echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
pkill -f '$DIR/sv-hostd' 2>/dev/null
systemctl stop 'svff-hostd-*' 'svff-app-*' 2>/dev/null
systemctl reset-failed 'svff-*' 2>/dev/null
rm -f $DIR/icd.sock $DIR/hostd.ready $DIR/hostd.done
true" | "$ROOTC" >/dev/null 2>&1
rm -f "$DIR"/cold*.sock "$DIR"/*.log "$DIR"/hostd.ready "$DIR"/hostd.done

# ---------------------------------------------------------------- forward ---
# `ssh -L <unix-socket>:127.0.0.1:<port>` is the whole of the cold path's
# transport on this deployment. The guest has exactly two virtio-serial ports
# and both are spoken for; adding a third means editing this machine's NixOS
# configuration and restarting a VM other people are using. What crosses is
# framed text and page counts either way — see `sv-gpu-server`'s `win_guest`.
FWD=()
COLD=()
for i in $(seq 0 $((SESSIONS - 1))); do
  s="$DIR/cold$i.sock"
  rm -f "$s"
  ssh "${GK[@]}" -N -L "$s:127.0.0.1:$((BASE + i))" "$G" >"$DIR/fwd$i.log" 2>&1 &
  FWD+=($!)
  COLD+=(--cold "$s")
done
cleanup() { kill "${FWD[@]}" 2>/dev/null; }
trap cleanup EXIT INT TERM
sleep 5
for i in $(seq 0 $((SESSIONS - 1))); do
  [ -S "$DIR/cold$i.sock" ] || { echo "FAIL: no forward socket $i"; cat "$DIR/fwd$i.log"; exit 1; }
done
say "$SESSIONS cold path(s) forwarded to the guest"

# ----------------------------------------------------------------- hostd ----
UNIT="svff-hostd-$$"
cat >"$DIR/hostd.sh" <<HOSTD
#!/usr/bin/env bash
export PATH=/run/current-system/sw/bin:/run/wrappers/bin
export LD_LIBRARY_PATH=$VKLOADER/lib
"$DIR/sv-hostd" qemu-serve --qmp /run/windows-vm/qmp.sock --vm windows-vm \\
  ${COLD[*]} --socket "$DIR/icd.sock" --clients $SESSIONS >"$DIR/hostd.log" 2>&1 &
H=\$!
for _ in \$(seq 1 180); do [ -S "$DIR/icd.sock" ] && break; sleep 1; done
# The application runs as root too, so this is belt and braces — but a mode that
# only the owner can connect to would make the socket the reason a future
# unprivileged app side fails, rather than the object's permissions.
chmod 666 "$DIR/icd.sock" 2>/dev/null
touch "$DIR/hostd.ready"
wait \$H
echo "hostd rc=\$?" >>"$DIR/hostd.log"
touch "$DIR/hostd.done"
chmod -R a+rwX "$DIR" 2>/dev/null
HOSTD
chmod 755 "$DIR/hostd.sh"
echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
systemd-run --unit=$UNIT --collect '$BASH_BIN' '$DIR/hostd.sh' >/dev/null 2>&1
echo started" | "$ROOTC" >/dev/null 2>&1

for _ in $(seq 1 200); do [ -f "$DIR/hostd.ready" ] && break; sleep 1; done
say "---- sv-hostd ----"
sed 's/^/  /' "$DIR/hostd.log" 2>/dev/null
if [ ! -S "$DIR/icd.sock" ]; then
  echo "FAIL: sv-hostd never offered the extent"
  echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
journalctl -u $UNIT --no-pager 2>&1 | tail -20" | "$ROOTC" 2>&1 | sed 's/^/  unit: /'
  exit 1
fi

# ------------------------------------------------------------------- app ----
# Root, and only because it opens `/proc/<qemu-pid>/fd/<n>` by name. Nothing
# about the design needs privilege: on a deployment where QEMU runs as the
# developer, this is an ordinary process. §1a's handoff is a *name* either way.
cat >"$DIR/app.sh" <<APP
#!/usr/bin/env bash
export PATH=$DEMOS/bin:/run/current-system/sw/bin:/run/wrappers/bin
export LD_LIBRARY_PATH=$MESA/lib:$GLVND/lib:$VKLOADER/lib
export LIBGL_DRIVERS_PATH=$MESA/lib/dri
export __EGL_VENDOR_LIBRARY_DIRS=$MESA/share/glvnd/egl_vendor.d
export VK_DRIVER_FILES=$DIR/sv_icd.json
export SV_SOCKET=$DIR/icd.sock
{
  # **One session is consumed per client, so the order and the count matter.**
  # The first version of this ran vk-probe and then eglinfo against a single
  # session: vk-probe took it, sv-hostd had served its one client and exited,
  # and eglinfo printed "eglInitialize failed" -- which is indistinguishable,
  # from the log alone, from Zink being unable to use the device. Two clients
  # need two sessions, which is what docs/design.md 0.12 is about.
  #
  # (No backticks anywhere in this heredoc. It is unquoted, so that the paths
  # above expand here rather than there, which means a backtick in a *comment*
  # is a command substitution. The first version had four of them and the run
  # printed "eglInitialize: command not found" from inside a comment.)
  echo "=== vk-probe: Vulkan through the ICD ==="
  "$DIR/vk-probe" 2>&1 | head -25
  if [ "$MODE" = gl ]; then
    echo
    echo "=== eglinfo: GL through Zink through the ICD ==="
    # Zink is forced. Left to itself Mesa picks a driver by looking for a DRI
    # device, finds none, and loads llvmpipe -- silently, and reporting GL 4.6
    # while doing it. That is the outcome this whole exercise exists to avoid,
    # and on this machine it is also the *default*, so the override is not a
    # tuning knob but the difference between the measurement and its opposite.
    #
    # It also fails *loudly* when it cannot reach a device: measured on this
    # machine with no ICD present at all, "eglInitialize failed" and no
    # llvmpipe. So a renderer string here is a device, and no renderer string
    # is not a quiet fallback.
    MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink \\
      EGL_PLATFORM=surfaceless eglinfo -B 2>&1 | sed -n '/Surfaceless/,\$p' | head -10
  fi
} >"$DIR/app.log" 2>&1
echo "app rc=\$?" >>"$DIR/app.log"
chmod a+r "$DIR/app.log"
APP
chmod 755 "$DIR/app.sh"
echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
systemd-run --unit=svff-app-$$ --collect --wait '$BASH_BIN' '$DIR/app.sh' >/dev/null 2>&1
echo done" | "$ROOTC" >/dev/null 2>&1

say "---- the app side, on a host with no GPU ----"
sed 's/^/  /' "$DIR/app.log" 2>/dev/null
say "---- sv-hostd, after ----"
sed 's/^/  /' "$DIR/hostd.log" 2>/dev/null

# ------------------------------------------------------------- the verdict ---
# **§31. This script used to `exit 0` here, unconditionally.**
#
# Everything above it is infrastructure: sockets, a transient unit, a root
# shell. The *result* — what `vk-probe` and `eglinfo` said — was written to
# `app.log`, printed for a person, and never read by the script. So a run in
# which the loader found no driver at all, or in which Mesa quietly fell back to
# llvmpipe, exited 0 and looked exactly like a run that worked. The comments in
# the heredoc above spell out precisely what a bad answer looks like ("NO
# PHYSICAL DEVICES", "eglInitialize failed", llvmpipe "silently, and reporting
# GL 4.6 while doing it") — every one of them was described and none of them was
# checked.
fail=0
ok()  { echo "  ok   $*"; }
bad() { echo "  FAIL $*"; fail=1; }
say "---- the verdict ----"

if [ ! -s "$DIR/app.log" ]; then
  bad "the app side produced no log at all"
else
  if grep -q "^app rc=0$" "$DIR/app.log"; then
    ok "the app side exited 0"
  else
    bad "the app side exited $(grep -o '^app rc=.*' "$DIR/app.log" | tail -1 | cut -d= -f2)"
  fi

  # The loader finding our ICD is the whole topology in one line. `vk-probe`
  # says so in words when it does not.
  if grep -q "NO PHYSICAL DEVICES" "$DIR/app.log"; then
    bad "the loader found no driver: the ICD did not reach the extent sv-hostd served"
  elif grep -q "^=== device 0:" "$DIR/app.log"; then
    ok "the loader enumerated a device through the ICD: $(grep -m1 '^=== device 0:' "$DIR/app.log" | cut -d: -f2-)"
  else
    bad "vk-probe printed neither a device nor a refusal; it did not run"
  fi

  if [ "$MODE" = gl ]; then
    # llvmpipe is the failure this whole exercise exists to avoid, and it is
    # also this machine's *default*, which is why it has to be named rather
    # than inferred from a missing error.
    if grep -qi "eglInitialize failed" "$DIR/app.log"; then
      bad "eglinfo could not reach a device"
    elif grep -qi "llvmpipe" "$DIR/app.log"; then
      bad "Zink fell back to llvmpipe; the renderer is software and the measurement is its opposite"
    elif grep -qi "OpenGL renderer string" "$DIR/app.log"; then
      ok "GL through Zink through the ICD: $(grep -m1 -i 'OpenGL renderer string' "$DIR/app.log" | cut -d: -f2-)"
    else
      bad "eglinfo reported no renderer at all"
    fi
  fi
fi

echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
pkill -f '$DIR/sv-hostd' 2>/dev/null; systemctl stop $UNIT 2>/dev/null; true" | "$ROOTC" >/dev/null 2>&1
[ "$fail" -eq 0 ] && say "sv-guest-host: PASS" || say "sv-guest-host: FAIL"
exit "$fail"
