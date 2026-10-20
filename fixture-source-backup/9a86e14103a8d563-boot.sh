#!/usr/bin/env bash
set -euo pipefail
S=/mnt/datasets/builds/scratch/linux-bar-rdd/product-wayland
E=/mnt/datasets/builds/evidence/linux-bar-rdd/product-wayland
SYS=/nix/store/j6nsiqyviwpcbwrd2lkywnv87x1015y7-nixos-system-host-test
Q=/nix/store/892xlhz8zs7s57sjp34l7rxhzfx2d943-qemu-for-vm-tests-11.0.3/bin
[ ! -e "$S/root.raw" ] || { echo 'private disk exists; inspect before reuse'; exit 1; }
"$Q/qemu-img" create -f raw "$S/root.raw" 1G
/nix/store/ja4ggh64fjyja45f6jarz5iaq2fr3n8d-e2fsprogs-1.47.4-bin/bin/mkfs.ext4 -q -L nixos "$S/root.raw"
exec systemd-run --user --scope --quiet --unit=linux-bar-rdd-product-wayland-vm-20261020 --slice=builds-10g.slice -p MemoryMax=10G -p MemorySwapMax=0 -p TasksMax=256 \
 timeout --signal=TERM --kill-after=10 600 "$Q/qemu-system-x86_64" \
 -enable-kvm -cpu host -machine q35 -m 3072 -smp 2 -nodefaults -no-user-config -display none -no-reboot \
 -serial "file:$E/console.log" -qmp "unix:$S/qmp.sock,server=on,wait=off" \
 -device virtio-serial-pci,id=serial0 \
 -chardev "socket,id=backdoor,path=$S/root.sock,server=on,wait=off" -device virtconsole,chardev=backdoor \
 -chardev "socket,id=svbell,path=$S/svbell.sock,server=on,wait=off" -device virtserialport,bus=serial0.0,chardev=svbell,name=org.sirati.svbell.0 \
 -chardev "socket,id=wcbell,path=$S/wcbell.sock,server=on,wait=off" -device virtserialport,bus=serial0.0,chardev=wcbell,name=org.sirati.wcbell.0 \
 -object "memory-backend-file,id=wcarena,mem-path=/dev/shm/linux-bar-rdd-product-wayland-arena,size=128M,share=on" \
 -device ivshmem-plain,id=wayland-bar,memdev=wcarena,bus=pcie.0,addr=0x1c \
 -object "memory-backend-file,id=gpu,mem-path=/dev/shm/linux-bar-rdd-product-wayland-gpu,size=256M,share=on" \
 -device ivshmem-plain,id=gpu-bar,memdev=gpu,bus=pcie.0,addr=0x1d \
 -virtfs local,path=/nix/store,security_model=none,mount_tag=nix-store,readonly=on \
 -virtfs "local,path=$S,security_model=none,mount_tag=shared" \
 -virtfs "local,path=$S,security_model=none,mount_tag=xchg" \
 -drive "file=$S/root.raw,format=raw,if=virtio,cache=none" \
 -kernel "$SYS/kernel" -initrd /nix/store/vnksspg6wszsdd9scxzayd8k9ka539dw-initrd-linux-6.18.44/initrd \
 -append "$(cat "$SYS/kernel-params") init=$SYS/init regInfo=/nix/.ro-store/l38xqk5wxjrzdzk0wwravwyrfsilbm0n-closure-info/registration console=ttyS0,115200n8"
