set -eu
for unit in fixture-cuse fixture-broker fixture-hostd fixture-wayland-broker fixture-wayland-bell; do
 p=$(systemctl show "$unit" -p MainPID --value)
 [ "$p" -gt 0 ]
 echo "UNIT=$unit PID=$p"
 readlink /proc/$p/exe
 sha256sum /proc/$p/exe
 cat /proc/$p/cmdline
 echo
 cat /proc/$p/status /proc/$p/maps /proc/$p/stat
 done
for slot in 1c 1d; do
 p=/sys/bus/pci/devices/0000:00:$slot.0
 stat -c 'BAR path=%n bytes=%s mode=%a uid=%u inode=%i dev=%d' "$p/resource2"
 cat "$p/vendor" "$p/device"
done
cat /tmp/shared/wayland-region.json
echo PRODUCT_LIVE_GUEST_OK
