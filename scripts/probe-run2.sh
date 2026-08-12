#!/bin/sh
# probe-run2.sh <a|b> <exe> <path>   -- run a probe as two separate processes
W=/mnt/datasets/builds/waycompose-base/scripts/probe-wine.sh
which=$1
exe=$2
path=$3
sh $W $which $exe server "$path" > /tmp/wc-server.log 2>&1 &
srv=$!
sleep 3
sh $W $which $exe client "$path" > /tmp/wc-client.log 2>&1
echo "client rc=$?"
wait $srv
echo "===== SERVER ====="
cat /tmp/wc-server.log
echo "===== CLIENT ====="
cat /tmp/wc-client.log
