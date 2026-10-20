set +e
ps -eo pid,ppid,args > /tmp/shared/final-guest-processes.txt
systemctl stop fixture-wayland-bell fixture-wayland-broker fixture-hostd fixture-broker fixture-cuse
systemctl show fixture-wayland-bell fixture-wayland-broker fixture-hostd fixture-broker fixture-cuse -p Id -p ActiveState -p SubState -p MainPID
