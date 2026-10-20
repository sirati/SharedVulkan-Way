#!/bin/bash
set -euo pipefail
session=/tmp/wcs.O7F5Jx/h/Library/Caches/waycompose/session-501
hl=/nix/store/pfqnx4dqc71msv70smnhz10d06parm6y-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
dbus=/nix/store/dmpcrsd1y8jnc81xjaf9zwljj47ajn2f-dbus-1.16.2/bin/dbus-send
pulse=/nix/store/ynp47b75fy0jhzg5x6rp5mqwxmdivcz9-pulseaudio-17.0/bin
fixture=$(mktemp -d /tmp/wc-hl-service.XXXXXXXX)
pid=
cleanup() {
  if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  rm -rf -- "$fixture"
}
trap cleanup EXIT
mkdir -p "$fixture"/{home,config,data,state,shm,root/lib}
ln -s /nix/store/cjcj20n6xa0hs5cd0adwx47g058c6z72-glibc-2.42-67/lib/ld-linux-aarch64.so.1 "$fixture/root/lib/ld-linux-aarch64.so.1"
export HOME="$fixture/home" XDG_CONFIG_HOME="$fixture/config" XDG_DATA_HOME="$fixture/data" XDG_STATE_HOME="$fixture/state"
export XDG_RUNTIME_DIR=/run/user/501 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/501/bus
export PULSE_SERVER=unix:/run/user/501/pulse/native PULSE_COOKIE=/run/user/501/pulse/cookie
unset DISPLAY XAUTHORITY DYLD_LIBRARY_PATH DYLD_INSERT_LIBRARIES
run() {
  "$hl" --timeout 30 --audio-backend null --fs-mode=rooted --isolated --sysroot "$fixture/root" \
    --guest-home /home/user --guest-cwd /home/user \
    --bind "$HOME:/home/user" --bind "$fixture/shm:/dev/shm" \
    --bind "$session:/run/user/501" --bind "$session:$session" \
    --bind "/nix/store:/nix/store:ro" "$@"
}
/usr/bin/codesign --verify --strict "$hl"
pa_pid=${1:?native private Pulse PID}
[[ "$pa_pid" =~ ^[0-9]+$ ]]
dd if=/dev/zero of="$HOME/zero.pcm" bs=8192 count=32 2>/dev/null
hl_args=( "$hl" --timeout 30 --audio-backend null --fs-mode=rooted --isolated --sysroot "$fixture/root"
  --guest-home /home/user --guest-cwd /home/user
  --bind "$HOME:/home/user" --bind "$fixture/shm:/dev/shm"
  --bind "$session:/run/user/501" --bind "$session:$session"
  --bind "/nix/store:/nix/store:ro" )
"${hl_args[@]}" "$pulse/pacat" --playback --raw --format=s16le --rate=8000 --channels=1 /home/user/zero.pcm &
pid=$!
echo "HL_PACAT_PID=$pid PULSE_PID=$pa_pid"
# Sample locally while the null sink consumes the bounded zero PCM stream.
sleep 1
/bin/ps -p "$pid" -o pid=,comm=,args=
run "$pulse/pactl" list sink-inputs
/usr/bin/vmmap "$pid" > "$fixture/hl.maps"
/usr/bin/vmmap "$pa_pid" > "$fixture/pulse.maps"
/usr/sbin/lsof -n -P -p "$pid,$pa_pid" > "$fixture/descriptors" 2>&1 || true
wait "$pid"
pid=
echo 'BEGIN hl live vmmap'
cat "$fixture/hl.maps"
echo 'END hl live vmmap'
echo 'BEGIN native Pulse live vmmap'
cat "$fixture/pulse.maps"
echo 'END native Pulse live vmmap'
echo 'BEGIN own live descriptor snapshot'
cat "$fixture/descriptors"
echo 'END own live descriptor snapshot'
echo 'joined live mapping fixture passed'
