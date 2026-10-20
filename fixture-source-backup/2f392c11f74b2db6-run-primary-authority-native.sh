#!/usr/bin/env bash
set -euo pipefail
exec 9>&-
exec 8>>/mnt/datasets/builds/wcrun/neo-runtime.lock
flock 8
systemd-run --user --scope --quiet --unit=wc-hl-primary-authority-20261020 --slice=vm-tests.slice -p MemoryMax=512M -p MemorySwapMax=0 timeout 60s ssh testhost.example /usr/bin/python3 /tmp/mac-bounded-controller.py /bin/bash /tmp/mac-primary-authority-intermediate.sh > /mnt/datasets/builds/evidence/hyperlinux/mac-primary-authority-intermediate.log 2>&1
