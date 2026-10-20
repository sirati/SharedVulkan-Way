# All executable copies below come from the frozen, byte-checked snapshot.
mkdir -p /run/audit-snapshot /tmp/shared/bin
cp -a /tmp/shared/snapshot/. /run/audit-snapshot/
chmod 755 /run/audit-snapshot
for name in sv-drm sv-hostd wc-broker vk-probe; do
 cp /run/audit-snapshot/assets/$name /tmp/shared/$name
 chmod 555 /tmp/shared/$name
done
cp /run/audit-snapshot/assets/sv-drm /tmp/shared/bin/sv-drm
chmod 555 /tmp/shared/bin/sv-drm
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
