#!/usr/bin/env bash
# §103.8, §103.15: install the launcher path for the macOS VM, both ends.
#
#   scripts/mac-launch-install.sh [APP...]      (default: console firefox text-editor)
#
# HOST (this workstation), all under $BASE:
#   * wc-session and its binaries (wc-proxy, wc-arena-host, sv-hostd, the Linux
#     wc-launch for --render-app-files) and an apps.ini naming what the guest
#     may start;
#   * two user units beside the VM, and no ssh or socket into it:
#     wc-mac-bell-hub (sv-hostd bell-hub on the rings' legacy virtio-serial
#     port, §99) and wc-mac-sessiond (wc-arena-host mac-sessiond), which finds
#     a session's boot record in guest RAM, locates it, starts its proxy and
#     takes it down when its lease ends (§103.15);
#   * QEMU runs as this user, so wc-session's guest-RAM path runs without root
#     (WC_LAUNCH_GUEST_RAM_USER=1, WC_LAUNCH_ROOTC=bash).
# GUEST (the VM): the LaunchDaemon org.waycompose.broker (§111.9: root; it
#   holds the sessions' guest-RAM windows until the host releases them, lends
#   the doorbell ports, whose nodes become root's, and signs the boot records
#   with the installation's key, which only it can read:
#   /Library/Application Support/waycompose/{bin/wc-broker,keys/<install>.key});
#   the macOS wc-launch in ~/Library/Application Support/waycompose,
#   its wc-launch.ini, and one .app per APP in ~/Applications/Linux, kept in the
#   Dock, built from the host's `wc-session app` answers fetched here. This
#   harness reaches the VM over the VM manager's own ssh, as every harness here
#   does; the product never does.
. "$(dirname "${BASH_SOURCE[0]}")/site.sh"
set -euo pipefail
inst=${MACVM_INSTANCE:-wayland-host}
root=$(cd "$(dirname "$0")/.." && pwd)
idir=/mnt/datasets/macvm/instances/$inst
# The VM's account comes from the VM store, not from this repository.
VMUSER=$(sed -n 's/.*"user": "\([^"]*\)".*/\1/p' "$idir/instance.json")
BASE=${WC_MAC_LAUNCH_BASE:-/mnt/datasets/builds/wcrun/mac-launch}
apps=("$@")
[ $# -eq 0 ] && apps=(console firefox text-editor)
mkdir -p "$HOME/.wc-launch/mac-firefox"
vm() { /mnt/datasets/macvm/bin/macos-vm ssh "$inst" "$@"; }
vport=$(sed -n 's/.*"ssh_port": \([0-9]*\).*/\1/p' "$idir/instance.json")
vssh="ssh -i /mnt/datasets/macvm/support/id_ed25519 -p $vport -o UserKnownHostsFile=$idir/known_hosts -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes"

for b in wc-proxy wc-arena-host wc-launch sv-hostd; do
  [ -x "$root/target/debug/$b" ] || { echo "build $b for Linux first"; exit 2; }
done
for b in wc-launch wc-broker; do
  [ -x "$root/target/x86_64-apple-darwin/debug/$b" ] || { echo "build $b for x86_64-apple-darwin first"; exit 2; }
done
if ! vm "sudo -n true || exit 1; if [ -S /var/run/waycompose/broker.sock ] || sudo -n launchctl print system/org.waycompose.broker >/dev/null 2>&1; then echo 'broker already loaded; drain sessions and unload safely before installing' >&2; exit 1; fi"; then
  exit 1
fi
# The installation's name (the ini's [session] name) and its key (§111.6):
# the host verifies boot records with it, the guest's broker signs with it.
inst_name=wcm

# ---------------------------------------------------------------- host ---
mkdir -p "$BASE/bin" "$BASE/run" "$BASE/bell" "$BASE/meta" "$BASE/keys"
chmod 700 "$BASE" "$BASE/bell" "$BASE/keys"
[ -s "$BASE/keys/$inst_name.key" ] || (umask 077; od -An -tx1 -N32 /dev/urandom | tr -d ' \n' >"$BASE/keys/$inst_name.key")
install -m 755 "$root/scripts/wc-session" "$BASE/wc-session"
for b in wc-proxy wc-arena-host wc-launch sv-hostd; do install -m 755 "$root/target/debug/$b" "$BASE/bin/$b.new" && mv -f "$BASE/bin/$b.new" "$BASE/bin/$b"; done
cat >"$BASE/apps.ini" <<EOF
# §103.8: what the macOS VM's key may start on this machine (scripts/wc-session).
[console]
exec     = $(command -v kgx)
titlebar = mac-tabs
desktop  = org.gnome.Console
gpu      = off
# GTK's own software renderer, which reports the damage it drew (its GL
# renderer through EGL on wl_shm damages the whole window every frame,
# §103.15), and never the RTX 5090.
env      = GSK_RENDERER=cairo
env      = __EGL_VENDOR_LIBRARY_FILENAMES=/run/opengl-driver/share/glvnd/egl_vendor.d/50_mesa.json
env      = CUDA_VISIBLE_DEVICES=

[firefox]
exec     = $(readlink -f "$(command -v firefox)")
profile  = $HOME/.wc-launch/mac-firefox
prefs    = middlemouse.paste=false;media.autoplay.default=0
titlebar = mac-tabs
desktop  = firefox
gpu      = off
# Never the RTX 5090 (it belongs to ML jobs): EGL from Mesa only, rendered in
# software, and no NVIDIA Vulkan either. The pixels reach the Mac as wl_shm.
env      = __EGL_VENDOR_LIBRARY_FILENAMES=/run/opengl-driver/share/glvnd/egl_vendor.d/50_mesa.json
env      = __GLX_VENDOR_LIBRARY_NAME=mesa
env      = LIBGL_ALWAYS_SOFTWARE=1
env      = VK_DRIVER_FILES=/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json
env      = CUDA_VISIBLE_DEVICES=

[text-editor]
exec     = $(command -v gnome-text-editor)
titlebar = mac-tabs
desktop  = org.gnome.TextEditor
gpu      = off
# GTK's own software renderer, which reports the damage it drew (its GL
# renderer through EGL on wl_shm damages the whole window every frame,
# §103.15), and never the RTX 5090.
env      = GSK_RENDERER=cairo
env      = __EGL_VENDOR_LIBRARY_FILENAMES=/run/opengl-driver/share/glvnd/egl_vendor.d/50_mesa.json
env      = CUDA_VISIBLE_DEVICES=
EOF
# The settings wc-session runs with, for the session service and every verb
# it runs (it used to be the forced command's `env`).
cat >"$BASE/sessiond.env" <<EOF
WC_LAUNCH_GUEST_RAM=1
WC_LAUNCH_GUEST_RAM_USER=1
WC_LAUNCH_ROOTC=bash
WC_LAUNCH_QMP=$idir/qmp.sock
WC_LAUNCH_VM=$idir
WC_LAUNCH_APPS=$BASE/apps.ini
WC_LAUNCH_AUDIO=${WC_LAUNCH_AUDIO:-on}
WC_LAUNCH_MAC_BELL=$BASE/bell/wl.sock
WC_LAUNCH_MAC_GPU_BELL=$BASE/bell/sv.sock
EOF
# The private sshd of §103.8, if an older install left it running.
systemctl --user stop wc-mac-sshd 2>/dev/null || true
for u in wc-mac-bell-hub wc-mac-sessiond; do
  systemctl --user stop "$u" 2>/dev/null || true
  systemctl --user reset-failed "$u" 2>/dev/null || true
done
# The VM must have the rings' port (§103.15): a legacy virtio-serial device
# whose chardev socket is $idir/wcbell.sock.
grep -q 'name=org.sirati.wcbell.0' "$idir/instance.json" \
  || echo "note: $inst has no org.sirati.wcbell.0 port; sessions will poll (see docs/waycompose-design.md §103.15)"
systemd-run --user --unit=wc-mac-bell-hub -p Restart=always -p RestartSec=1 \
  "$BASE/bin/sv-hostd" bell-hub --port "$idir/wcbell.sock=$BASE/bell/wl.sock" --port "$idir/svbell.sock=$BASE/bell/sv.sock" >/dev/null
systemd-run --user --unit=wc-mac-sessiond -p Restart=always -p RestartSec=1 -p EnvironmentFile="$BASE/sessiond.env" \
  "$BASE/bin/wc-arena-host" mac-sessiond --qmp "$idir/qmp.sock" --vm "$idir" --session "$BASE/wc-session" \
  --bell "$BASE/bell/wl.sock" --sweep-secs "${WC_MAC_SWEEP_SECS:-0}" --keys "$BASE/keys" >/dev/null
sleep 1
systemctl --user is-active --quiet wc-mac-sessiond || { journalctl --user -u wc-mac-sessiond -n 20 --no-pager; exit 1; }

# What each application's bundle is built from, asked here rather than by
# the guest: installing needs no channel to the host either.
for a in "${apps[@]}"; do
  env $(grep -v '^#' "$BASE/sessiond.env") bash "$BASE/wc-session" app "$a" light >"$BASE/meta/$a.meta"
done

# ---------------------------------------------------------------- guest ---
G='Library/Application Support/waycompose'
vm "mkdir -p \"$G/bin\" \"$G/meta\""
{
  echo "# §103.8, §103.15, written by scripts/mac-launch-install.sh"
  echo "[session]"
  echo "name = $inst_name"
  echo "# The rings' doorbell: the legacy virtio-serial port's callout node."
  echo "wayland-bell = cu.org.sirati.wcbell.0"
  echo "# The GPU server's, when a session starts one (gpu-server)."
  echo "gpu-bell = cu.org.sirati.svbell.0"
  echo "# On macOS the compositor runs inside each application bundle and there is no relay."
  echo "compositor = in-process"
  echo "relay = none"
  echo "arena-mib = ${WC_MAC_ARENA_MIB:-64}"
  echo "linger-secs = 20"
  echo "audio = ${WC_LAUNCH_AUDIO:-on}"
  for a in "${apps[@]}"; do echo; echo "[app.$a]"; echo "host-app = $a"; done
} | $vssh $VMUSER@127.0.0.1 "cat > \"$G/wc-launch.ini\""
rsync -a -e "$vssh" "$root/target/x86_64-apple-darwin/debug/wc-launch" "$VMUSER@127.0.0.1:$G/bin/wc-launch"

# §111.9: the broker, a LaunchDaemon. Its binary, its keys and its plist are
# root's, where no user can write them (a daemon running a user-writable
# file would hand that user root). The key travels through a 0600 file in
# the VM user's home for the one copy, and is gone from there at once.
SYS='/Library/Application Support/waycompose'
rsync -a -e "$vssh" "$root/target/x86_64-apple-darwin/debug/wc-broker" "$VMUSER@127.0.0.1:$G/wc-broker.new"
$vssh $VMUSER@127.0.0.1 "umask 077; cat > \"$G/broker.key.new\"" <"$BASE/keys/$inst_name.key"
vm "set -e; sudo -n mkdir -p \"$SYS/bin\" \"$SYS/keys\" /Library/Logs/waycompose
  sudo -n chown root:wheel \"$SYS\" \"$SYS/bin\" \"$SYS/keys\"; sudo -n chmod 755 \"$SYS\" \"$SYS/bin\"; sudo -n chmod 700 \"$SYS/keys\"
  sudo -n install -o root -g wheel -m 755 \"$G/wc-broker.new\" \"$SYS/bin/wc-broker\"; rm -f \"$G/wc-broker.new\"
  sudo -n install -o root -g wheel -m 600 \"$G/broker.key.new\" \"$SYS/keys/$inst_name.key\"; rm -f \"$G/broker.key.new\"
  \"$SYS/bin/wc-broker\" plist \"$SYS/bin/wc-broker\" | sudo -n tee /Library/LaunchDaemons/org.waycompose.broker.plist >/dev/null
  sudo -n chown root:wheel /Library/LaunchDaemons/org.waycompose.broker.plist; sudo -n chmod 644 /Library/LaunchDaemons/org.waycompose.broker.plist
  sudo -n launchctl bootstrap system /Library/LaunchDaemons/org.waycompose.broker.plist
  for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /var/run/waycompose/broker.sock ] && break; sleep 0.5; done
  [ -S /var/run/waycompose/broker.sock ] || { sudo -n tail -20 /Library/Logs/waycompose/broker.log; exit 1; }
  rm -f \"$G/session.key\""
echo "guest: LaunchDaemon org.waycompose.broker running; ports: $(vm 'ls -l /dev/cu.org.sirati.* 2>/dev/null | awk "{print \$1, \$3, \$NF}" | tr "\n" " "')"
rsync -a -e "$vssh" "$BASE/meta/" "$VMUSER@127.0.0.1:$G/meta/"
vm "cd \"$G\" && ./bin/wc-launch --install ${apps[*]} --pin --meta \"\$HOME/$G/meta\""
echo "installed: host $BASE (units wc-mac-bell-hub, wc-mac-sessiond), guest ~/Applications/Linux"
