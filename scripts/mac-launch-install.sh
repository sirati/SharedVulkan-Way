#!/usr/bin/env bash
# §103.8: install the launcher path for the macOS VM, both ends.
#
#   scripts/mac-launch-install.sh [APP...]      (default: console firefox text-editor)
#
# HOST (this workstation), all under $BASE, nothing in the user's own ssh setup:
#   * wc-session and its binaries (wc-proxy, wc-arena-host, the Linux wc-launch
#     for --render-app-files) and an apps.ini naming what the guest may start;
#   * a private, unprivileged sshd on 127.0.0.1:$PORT (user unit wc-mac-sshd)
#     with its own host key and authorized_keys, whose one key is the guest's,
#     restricted to the forced command wc-session. QEMU's user network maps the
#     guest's <vm-gateway-ip> to this machine's loopback, so the VM reaches it and
#     nothing else does. The user's own sshd and ~/.ssh are not touched.
#   * QEMU runs as this user, so wc-session's guest-RAM path runs without root
#     (WC_LAUNCH_GUEST_RAM_USER=1, WC_LAUNCH_ROOTC=bash).
# GUEST (the VM): the macOS wc-launch in ~/Library/Application Support/waycompose,
#   its key, its wc-launch.ini, an ssh alias `wchost`, and one .app per APP in
#   ~/Applications/Linux, kept in the Dock.
set -euo pipefail
inst=${MACVM_INSTANCE:-wayland-host}
root=$(cd "$(dirname "$0")/.." && pwd)
idir=/mnt/datasets/macvm/instances/$inst
BASE=${WC_MAC_LAUNCH_BASE:-/mnt/datasets/builds/wcrun/mac-launch}
PORT=${WC_MAC_SSHD_PORT:-22422}
apps=("$@")
[ $# -eq 0 ] && apps=(console firefox text-editor)
mkdir -p "$HOME/.wc-launch/mac-firefox"
vm() { /mnt/datasets/macvm/bin/macos-vm ssh "$inst" "$@"; }
vport=$(sed -n 's/.*"ssh_port": \([0-9]*\).*/\1/p' "$idir/instance.json")
vssh="ssh -i /mnt/datasets/macvm/support/id_ed25519 -p $vport -o UserKnownHostsFile=$idir/known_hosts -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes"

for b in wc-proxy wc-arena-host wc-launch; do
  [ -x "$root/target/debug/$b" ] || { echo "build $b for Linux first"; exit 2; }
done
[ -x "$root/target/x86_64-apple-darwin/debug/wc-launch" ] || { echo "build wc-launch for x86_64-apple-darwin first"; exit 2; }

# ---------------------------------------------------------------- host ---
mkdir -p "$BASE/bin" "$BASE/sshd" "$BASE/run"
chmod 700 "$BASE" "$BASE/sshd"
install -m 755 "$root/scripts/wc-session" "$BASE/wc-session"
for b in wc-proxy wc-arena-host wc-launch; do install -m 755 "$root/target/debug/$b" "$BASE/bin/$b"; done
cat >"$BASE/apps.ini" <<EOF
# §103.8: what the macOS VM's key may start on this machine (scripts/wc-session).
[console]
exec     = $(command -v kgx)
titlebar = mac-tabs
desktop  = org.gnome.Console
gpu      = off

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
EOF
[ -f "$BASE/sshd/host_key" ] || ssh-keygen -q -t ed25519 -N '' -f "$BASE/sshd/host_key"
sshd_bin=$(dirname "$(readlink -f "$(command -v ssh)")")/sshd
cat >"$BASE/sshd/sshd_config" <<EOF
# A private sshd for the macOS VM's launcher key only (§103.8).
Port $PORT
ListenAddress 127.0.0.1
HostKey $BASE/sshd/host_key
PidFile $BASE/sshd/sshd.pid
AuthorizedKeysFile $BASE/sshd/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
AllowTcpForwarding yes
AllowStreamLocalForwarding yes
StreamLocalBindUnlink yes
PermitUserEnvironment no
X11Forwarding no
PrintMotd no
LogLevel VERBOSE
EOF

# ---------------------------------------------------------------- guest ---
G='Library/Application Support/waycompose'
vm "mkdir -p \"$G/bin\" ~/.ssh && chmod 700 ~/.ssh && { [ -f \"$G/id_ed25519\" ] || ssh-keygen -q -t ed25519 -N '' -C wc-launch-macos -f \"$G/id_ed25519\"; }"
pub=$(vm "cat \"$G/id_ed25519.pub\"")
env="WC_LAUNCH_GUEST_RAM=1 WC_LAUNCH_GUEST_RAM_USER=1 WC_LAUNCH_ROOTC=bash WC_LAUNCH_QMP=$idir/qmp.sock WC_LAUNCH_VM=$idir WC_LAUNCH_APPS=$BASE/apps.ini WC_LAUNCH_AUDIO=${WC_LAUNCH_AUDIO:-on}"
echo "restrict,port-forwarding,command=\"env $env $BASE/wc-session\" $pub" >"$BASE/sshd/authorized_keys"
chmod 600 "$BASE/sshd/authorized_keys"
systemctl --user stop wc-mac-sshd 2>/dev/null || true
systemctl --user reset-failed wc-mac-sshd 2>/dev/null || true
systemd-run --user --unit=wc-mac-sshd --collect "$sshd_bin" -D -e -f "$BASE/sshd/sshd_config" >/dev/null
sleep 1
ss -ltnH | grep -q "127.0.0.1:$PORT" || { echo "the private sshd did not listen"; journalctl --user -u wc-mac-sshd -n 20 --no-pager; exit 1; }

vm "grep -q '^Host wchost' ~/.ssh/config 2>/dev/null || printf 'Host wchost\n  HostName <vm-gateway-ip>\n  Port $PORT\n  User $(id -un)\n' >> ~/.ssh/config; chmod 600 ~/.ssh/config"
{
  echo "# §103.8, written by scripts/mac-launch-install.sh"
  echo "[session]"
  echo "host = wchost"
  echo "key = /Users/c3r/$G/id_ed25519"
  echo "name = wcm"
  echo "# On macOS the compositor runs inside each application bundle and there is no relay."
  echo "compositor = in-process"
  echo "relay = none"
  echo "arena-mib = ${WC_MAC_ARENA_MIB:-64}"
  echo "linger-secs = 20"
  echo "audio = ${WC_LAUNCH_AUDIO:-on}"
  for a in "${apps[@]}"; do echo; echo "[app.$a]"; echo "host-app = $a"; done
} | $vssh c3r@127.0.0.1 "cat > \"$G/wc-launch.ini\""
rsync -a -e "$vssh" "$root/target/x86_64-apple-darwin/debug/wc-launch" "c3r@127.0.0.1:$G/bin/wc-launch"
vm "cd \"$G\" && ./bin/wc-launch --install ${apps[*]} --pin"
echo "installed: host $BASE (sshd 127.0.0.1:$PORT), guest ~/Applications/Linux"
