# All executable copies below come from the frozen, byte-checked snapshot.
mkdir -p /run/product-snapshot /tmp/shared/bin
cp -a /tmp/shared/snapshot-v3/. /run/product-snapshot/
chmod 755 /run/product-snapshot
for name in sv-drm sv-hostd wc-broker vk-probe wc-proxy wc-bar-publisher; do
 cp /run/product-snapshot/assets/$name /tmp/shared/$name
 chmod 555 /tmp/shared/$name
done
for name in sv-drm wc-proxy wc-bar-publisher; do
 cp /run/product-snapshot/assets/$name /tmp/shared/bin/$name
 chmod 555 /tmp/shared/bin/$name
done
set -eu
exec 2>&1
modprobe cuse
mkdir -p /run/private-cuse
systemd-run --unit fixture-cuse -p StandardOutput=append:/tmp/shared/cuse.log -p StandardError=append:/tmp/shared/cuse.log /tmp/shared/sv-drm serve --minor 190 --owner 1000 --group 100 --mode 0600 --state /run/private-cuse/node.state --services /run/private-cuse/services.sock
# A disposable, one-UID device broker. No raw-resource modes or host settings change.
groupadd -g 1100 fixture-access
usermod -aG fixture-access alice
install -d -o root -g 1100 -m 0750 /run/private-cuse/broker
p=/sys/bus/pci/devices/0000:00:1d.0
[ "$(cat "$p/vendor")" = 0x1af4 ]
[ "$(cat "$p/device")" = 0x1110 ]
stat -c 'BAR resource2 size=%s mode=%a owner=%u' "$p/resource2"
cat "$p/resource"
systemd-run --unit fixture-broker -p StandardOutput=append:/tmp/shared/broker.log -p StandardError=append:/tmp/shared/broker.log /tmp/shared/wc-broker serve --socket /run/private-cuse/broker/broker.sock --keys /run/private-cuse/broker/keys --no-default-devices --device "gpu-bar=$p/resource2" --device svbell=/dev/virtio-ports/org.sirati.svbell.0
sleep 1
install -d -o 1000 -g 100 -m 0700 /run/private-cuse/user /run/private-cuse/user/gpu
systemd-run --unit fixture-hostd -p User=alice -p StandardOutput=append:/tmp/shared/hostd.log -p StandardError=append:/tmp/shared/hostd.log --setenv WC_BROKER=/run/private-cuse/broker/broker.sock /tmp/shared/sv-hostd ivshmem-serve --region-device gpu-bar --region-bytes 268435456 --cold pool --bell-device svbell --bell-hub /run/private-cuse/user/bell.sock --socket /run/private-cuse/user/gpu/gpu.sock --drm-services /run/private-cuse/services.sock
sleep 2
cat /tmp/shared/hostd.log
ls -l /dev/dri/renderD190 /run/private-cuse/user

# The product Wayland registry is separate from the proven GPU device broker.
install -d -o root -g 1100 -m 0750 /run/private-cuse/wayland-broker
p=/sys/bus/pci/devices/0000:00:1c.0
[ "$(cat "$p/vendor")" = 0x1af4 ]
[ "$(cat "$p/device")" = 0x1110 ]
[ "$(stat -c %s "$p/resource2")" = 134217728 ]
[ "$(stat -c '%a:%u' "$p/resource2")" = 600:0 ]
chmod 600 /dev/virtio-ports/org.sirati.wcbell.0
stat -Lc "Wayland bell mode=%a uid=%u dev=%t:%T" /dev/virtio-ports/org.sirati.wcbell.0
HOST_RUN=$(cat /tmp/shared/host-run.txt)
systemd-run --unit fixture-wayland-broker -p StandardOutput=append:/tmp/shared/wayland-broker.log -p StandardError=append:/tmp/shared/wayland-broker.log /tmp/shared/bin/wc-bar-publisher --socket /run/private-cuse/wayland-broker/broker.sock --bar "$p/resource2" --uid 1000 --pool wayland --run "$HOST_RUN" --bell /dev/virtio-ports/org.sirati.wcbell.0 --receipt /tmp/shared/wayland-region.json
sleep 1
systemctl is-active fixture-wayland-broker
systemd-run --unit fixture-wayland-bell -p User=alice -p StandardOutput=append:/tmp/shared/wayland-bell.log -p StandardError=append:/tmp/shared/wayland-bell.log --setenv WC_BROKER=/run/private-cuse/wayland-broker/broker.sock /tmp/shared/sv-hostd bell-hub --broker-port wcbell=/run/private-cuse/user/wayland-bell.sock
sleep 1
systemctl is-active fixture-wayland-bell
cat /tmp/shared/wayland-region.json
echo PRODUCT_SETUP_OK
