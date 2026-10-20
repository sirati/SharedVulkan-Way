#!/usr/bin/env bash
set -euo pipefail
exec 9>&-
exec 8>/mnt/datasets/builds/wcrun/neo-runtime.lock
flock 8
evidence=/mnt/datasets/builds/evidence/hyperlinux
remote=testhost.example
status=0
for fixture in lifecycle primary ptrace dc-valid dc-validtag dc-ro dc-none dc-0x100000 dc-bus seq cleanup alias seccomp async external-handled external-pending buswait; do
  script=/tmp/wc-hl-consolidated-matrix-90ac656d.py
  if [[ "$fixture" == lifecycle ]]; then script=/tmp/wc-hl-consolidated-lifecycle-90ac656d.py; fi
  if systemd-run --user --scope --quiet --unit="wc-hl-90ac-$fixture-$$" --slice=vm-tests.slice -p MemoryMax=512M -p MemorySwapMax=0 ssh -o BatchMode=yes "$remote" /usr/bin/python3 /tmp/wc-hl-consolidated-controller-90ac656d.py /usr/bin/python3 "$script" "$fixture" > "$evidence/mac-consolidated-90ac656d-$fixture.log" 2>&1; then
    echo "PASS $fixture"
  else
    result=$?
    echo "FAIL $fixture controller=$result"
    status=1
  fi
done
# Each synchronous controller has completed its own fixture removal before
# this point. Remove only the coordinator's named non-process artifacts.
ssh -o BatchMode=yes "$remote" /bin/rm -f /tmp/wc-hl-consolidated-controller-90ac656d.py /tmp/wc-hl-consolidated-matrix-90ac656d.py /tmp/wc-hl-consolidated-lifecycle-90ac656d.py /tmp/wc-hl-lifecycle-90ac656d-cba4026cbac6
exit "$status"
