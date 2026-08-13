#!/bin/sh
# probe-hostclient.sh -- can a *native Linux* process speak to a Wine AF_UNIX
# server socket? If yes, a Linux-side test harness can drive the Windows build.
W=/mnt/datasets/builds/waycompose-base/scripts/probe-wine.sh
SOCK=/tmp/wc-wine/pfx-b/drive_c/wc-test3.sock
rm -f "$SOCK"
sh $W b /tmp/probe-unixproc.exe server 'C:\wc-test3.sock' > /tmp/wc-server3.log 2>&1 &
srv=$!
sleep 4
ls -la "$SOCK"
printf 'PING-crossproc' | /nix/store/n50qh3qh69k1lqbd9ygkd5jar429ax82-socat-1.8.1.1/bin/socat -t2 - UNIX-CONNECT:"$SOCK"
echo ""
echo "socat rc=$?"
wait $srv
echo "===== WINE SERVER ====="
cat /tmp/wc-server3.log
