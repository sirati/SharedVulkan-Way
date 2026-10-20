#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
source <(sed -n '/^deployment_uses_gpu_firefox() {/,/^}/p' scripts/wc-launch.sh)
source <(sed -n '/^gpu_prerequisite_check() {/,/^}/p' scripts/wc-launch.sh)
STAGE=$(mktemp -d /mnt/datasets/builds/wcrun/native-gtk-policy-XXXXXX)
trap 'rm -rf "$STAGE"' EXIT
ROOT=$PWD HOST=fixture GPU_SESSIONS=0 GPU_ENV_MODE=print GPU_GTK='' KOPPER_MESA='' DMABUF_NODE='' GPU_PCI=''
EXPECTED=$(sha256sum crates/wc-proxy/protocols/wc-native-shm-v1.xml crates/wc-native-shm/include/wc-native-shm.h | cut -d' ' -f1)
FIREFOX=firefox
MOCK=valid
h() { case "$MOCK" in valid) printf '%s\n' "$EXPECTED";; mismatch) echo wrong;; unavailable) return 1;; esac; }
reject() { if gpu_prerequisite_check; then echo "FAIL accepted $1"; exit 1; fi; echo "PASS rejects $1"; }
gpu_prerequisite_check
GPU_SESSIONS=1
WC_LAUNCH_INSTALL_APPS=minecraft; gpu_prerequisite_check; echo 'PASS Minecraft-only needs no GTK/VA'
WC_LAUNCH_INSTALL_APPS=native; gpu_prerequisite_check; echo 'PASS native-only needs no GTK/VA'
cat >"$STAGE/guest.ini" <<'INI'
[app.browser]
host-app = browser-host
INI
cat >"$STAGE/apps.ini" <<'INI'
[browser-host]
exec = /fixture/bin/firefox
gpu = on
INI
WC_LAUNCH_EXTRA_APP_BLOCKS=$STAGE/guest.ini
WC_LAUNCH_EXTRA_APPS=$STAGE/apps.ini
WC_LAUNCH_INSTALL_APPS=browser
reject aliased-gpu-firefox-missing-prerequisites
sed -i 's/gpu = on/gpu = off/' "$STAGE/apps.ini"
gpu_prerequisite_check; echo 'PASS gpu-off Firefox needs no GTK/VA'
unset WC_LAUNCH_INSTALL_APPS WC_LAUNCH_EXTRA_APP_BLOCKS WC_LAUNCH_EXTRA_APPS
reject print-only
GPU_ENV_MODE=write; reject missing-gtk
GPU_GTK=/nix/store/test-gtk; KOPPER_MESA=/nix/store/test-mesa; reject missing-node
DMABUF_NODE=/dev/dri/renderD129; GPU_PCI=invalid; reject missing-pci
GPU_PCI=8086:a7a8; MOCK=mismatch; reject mismatched-library
MOCK=unavailable; reject unavailable-host-prerequisite
MOCK=valid; gpu_prerequisite_check; echo 'PASS matched prerequisites'
if grep -F 'user_pref("gfx.webrender.all"' scripts/wc-session; then echo 'FAIL product force preference'; exit 1; fi
echo 'PASS no product WebRender force preference'
source <(sed -n '/^stock_gpu_prerequisite_check() {/,/^}/p' scripts/wc-session)
die() { echo "$*" >&2; exit 1; }
BASE=$STAGE; DEF_EXEC=/fixture/bin/firefox; PROFILE="$BASE/profile"; DEF_GPU=on
mkdir -p "$BASE/bin" "$BASE/gtk/lib" "$BASE/gtk/nix-support"
touch "$BASE/gpu.env" "$BASE/gtk/lib/libgdk-3.so"
printf 'abi\n' >"$BASE/gtk/nix-support/wc-native-shm-abi"
ln -s "$BASE/gtk" "$BASE/bin/gpu-gtk"
GPU_ON=''; NSWRAP=(); GPUENV=(); DEF_ENV=(); PCI=''
reject_runtime() { if (stock_gpu_prerequisite_check); then echo "FAIL runtime accepted $1"; exit 1; fi; echo "PASS runtime rejects $1"; }
reject_runtime missing-gpu-and-node
GPU_ON=1; NSWRAP=(/fixture/sv-drm ns --); reject_runtime missing-pci
PCI=8086:a7a8; GPUENV=(WC_NATIVE_SHM=1); reject_runtime missing-strict-library
GPUENV=(WC_NATIVE_SHM=1 WC_NATIVE_SHM_STRICT=1 LD_LIBRARY_PATH=/wrong/lib); reject_runtime foreign-library
GPUENV=(WC_NATIVE_SHM=1 WC_NATIVE_SHM_STRICT=1 "LD_LIBRARY_PATH=$BASE/gtk/lib")
stock_gpu_prerequisite_check; echo 'PASS runtime matched native route'
DEF_ENV=(WC_NATIVE_SHM_STRICT=0); reject_runtime per-app-strict-disable
DEF_ENV=(LD_LIBRARY_PATH=/wrong/lib); reject_runtime per-app-foreign-library
DEF_ENV=()

DEF_EXEC=/fixture/bin/native-app; GPU_ON=''; NSWRAP=(); GPUENV=()
stock_gpu_prerequisite_check; echo 'PASS runtime native app needs no GTK/VA'
source <(sed -n '/^clear_legacy_webrender_pref() {/,/^}/p' scripts/wc-session)
mkdir -p "$STAGE/profile"
printf '%s\n' 'user_pref("gfx.webrender.all", true);' 'user_pref("keep.user.setting", 7);' >"$STAGE/profile/prefs.js"
clear_legacy_webrender_pref "$STAGE/profile"
! grep -F 'gfx.webrender.all' "$STAGE/profile/prefs.js"
grep -F 'keep.user.setting' "$STAGE/profile/prefs.js" >/dev/null
echo 'PASS clears only the legacy automatic force preference'
