#!/usr/bin/env bash
# Does the arena give guest RAM back? Measured across a real hypervisor.
#
#   scripts/wc-release-vm.sh            both legs
#   scripts/wc-release-vm.sh --keep     leave the run directory and the guest
#
# Run from inside `nix develop`.
#
# # The question, in the user's words
#
#   "its very important that the arena also releases unneeded memory again.
#    using 1.2Gb of vram is a lot for firefox"
#
# docs/waycompose-design.md §61.5 is the run that could not answer it: growth
# worked, release did not, and two causes were observed. §72 is this harness and
# what it measured.
#
# # Why this is not `wc-arena-pressure.sh` with a release assertion bolted on
#
# Because that harness is Wine on one Linux box over a *file*, and the quantity
# in question is not in it. Release is a claim about **the guest's pages**: the
# host unmapping its view of a region frees nothing at all, because the bytes
# are the guest's, they are `mlock`ed/`VirtualLock`ed and therefore
# non-pageable, and they stay gone from the guest's operating system until the
# guest frees them. So the only number that can answer the user is the
# compositor's **own resident set, read inside the virtual machine**, and a
# harness with no virtual machine in it cannot produce one.
#
# `nix build .#svGuest` is the venue: NixOS in QEMU whose RAM is a
# `memory-backend-memfd` this workstation can map. Same topology as
# `wc-ring-vm.sh` -- Wayland client here, compositor in another kernel, and the
# only thing between them is memory.
#
# # The experiment
#
# The direct test of the leak needs no growth pressure and no browser:
#
#   1. a client connects and allocates pools -- `wc-poolhog`, which creates them
#      and never destroys them;
#   2. it is **killed**, not closed. That is precisely the path on which no
#      `wl_shm_pool.destroy` is ever sent, which is the whole of the bug: before
#      `Drop for Session`, `Pool::releasable` could never become true for a pool
#      whose client had exited, so those bytes stayed `published` forever;
#   3. watch `bytes out` go to zero -- pools returned;
#   4. watch the now-empty region be handed back -- guest RAM returned;
#   5. read the compositor's resident set **in the guest** before, at peak and
#      after.
#
# A second connection (`wc-poolhog 0`) is up throughout leg A, so leg A is the
# case where a *session* still exists to turn the clock. **Leg B removes it**:
# nothing is connected and nothing reconnects, and the region has to come back
# anyway. That was a stated limitation until `Arena::frame_idle` was called from
# the accept loop; leg B is what holds the fix.
#
# # The one thing this run cannot contain, and it is being fixed elsewhere
#
# **The compositor never sees these pools as Wayland objects.** `wc-proxy`
# writes its upstream stream in the in-band-`h` dialect (§2.3), which only the
# **Windows** build of the compositor can read: on Unix the vendored backend's
# `Argument::Fd` is a raw descriptor and there is nowhere to put a
# `wc_handle::Descriptor`, so the rewritten `wl_shm.create_pool` desynchronises
# a Linux server's reader and it stops answering, with nothing logged on either
# side. §72.3 has the measurement and `scripts/wc-pooltest.c` reproduces it.
#
# **That is a defect under repair, not a property of this harness.** The agreed
# shape is a format *negotiated once at the handshake* — where `WIRE_VERSION`
# and the protocol hash already live, so two sides that disagree refuse each
# other explicitly instead of desynchronising in silence — rather than a rewrite
# at the boundary. Until it lands this leg cannot composite, and the honest
# reading of that is: this run answers "does the arena give guest RAM back" and
# says nothing whatever about drawing.
#
# The memory answer is untouched by it. The proxy's allocator takes the arena
# bytes when it *parses* the request, so the pools really are cut out of guest
# RAM, really are returned when the client dies, and the region really is handed
# back to the guest.
#
# # What this harness may and may not claim
#
# `wc-substrate.sh` and `wc-topology.sh` decide that, and this harness declares
# to both. `cross-vm` is proven from the guest-RAM locate rather than asserted:
# `wc-arena-host` translates a run id into offsets in QEMU's memory backend
# through QEMU's own monitor, and a monitor cannot answer for a machine that is
# not running one.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
RUNDIR=${SV_RUNDIR:-/tmp/wc-release-vm}
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

pass=0; fail=0
ok()   { echo "  PASS  $*"; pass=$((pass+1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail+1)); }
note() { echo "  ----  $*"; }
say()  { echo "[$(date +%H:%M:%S)] $*"; }

# ------------------------------------------------------------- the numbers ---
# A 12 MiB floor with a 1 MiB outbound tail, so 11 MiB is allocatable: the first
# 6 MiB pool fits and the second cannot, which is what buys a region. Both
# regions bought here are whole MiB and far larger than any noise in a
# resident-set reading, which is what makes the before/after difference readable
# as one number rather than argued from a trend.
ARENA_MIB=${WC_RELEASE_ARENA_MIB:-12}
POOL_BYTES=${WC_RELEASE_POOL_BYTES:-6291456}
POOLS=${WC_RELEASE_POOLS:-2}
# Frames of *not drawing* before an empty region is handed back. The pump's
# poll wait is at most 20 ms, so this is a small number of seconds.
GROW_IDLE=${WC_RELEASE_GROW_IDLE:-50}
# The growth series is `step`, `2*step`, `4*step` (`GROW_STEP_CAP` aside), and a
# region's frame list is one `u64` per page. So a 32 MiB region is 8192 frames =
# 65536 bytes plus `sv-wire`'s ReplyHeader = 65600 against a 65536-byte ring --
# §61.4, which on a Linux guest is deterministic rather than rare because
# `/proc/self/pagemap` always names every page. 8 MiB keeps both of this run's
# growths (8 MiB, then 16 MiB) under that wall; the wall itself is asserted
# against in leg B rather than left to chance.
GROW_STEP=${WC_RELEASE_GROW_STEP:-8388608}

# ------------------------------------------------ what wc-substrate.sh wants --
# `wc_locate_arena` is written for the deployment, where the guest is an ssh
# target and QEMU runs on a *third* machine reached as `$HOST`. Here both are
# this workstation and the guest's `/sv` is this run directory over 9p, so the
# two channels it needs are shells rather than ssh.
#
# They are shims and not a second locate. Everything load-bearing -- the run id
# read out of the compositor's own line, `wc-arena-host` through QEMU's monitor,
# the `verify:` count, the refusal when a page is missing -- is that function's,
# unchanged, and `WC_SUBSTRATE_LOCATED` is set by it from the locator's output.
# A copy of it here that happened to agree with itself is exactly the shape this
# project keeps finding gates in.
HOST="$(hostname)"                 # the machine QEMU runs on: this one
HOST_DIR="$LINUX_BIN"              # where wc-arena-host is
QMP="$RUNDIR/qmp.sock"
# **The run directory, not `sv-guest`.** §71.3: `nix/guest.nix` bakes
# `-name sv-guest,process=sv-guest` into every guest it boots, and
# `find_qemu_pid` matches a substring of the command line — so on a workstation
# where several agents each have a guest up, `--vm sv-guest` matches all of them
# and the locator refuses, which it is right to do. This directory appears in
# this QEMU's command line (the disk image and the 9p share) and in nobody
# else's. Measured here: another agent's `sv-guest` was running throughout.
VM_HINT="$RUNDIR"
ROOTC=bash                         # no privilege to acquire: QEMU is ours
SSHOPT=()
WC_GUEST_CONFIGURED=1              # `nix build .#svGuest`, booted below
export WC_HOST="$HOST"
h() { bash -c "$*"; }
# The guest spells this run directory `/sv`; this workstation spells it
# `$RUNDIR`. Same bytes, one 9p export, two names.
g() { bash -c "$(printf '%s' "$*" | sed "s#/sv/#$RUNDIR/#g")"; }

. "$ROOT/scripts/wc-substrate.sh"
. "$ROOT/scripts/wc-topology.sh"
. "$ROOT/scripts/wc-binaries.sh"

# One spelling of the outbound window, and it is `wc-substrate.sh`'s. The
# compositor is told it on its command line and `wc_locate_arena` writes the
# same number into the plan; a second spelling here would be a number that can
# differ, and the symptom would be the proxy's allocator and the compositor's
# disagreeing about which bytes are whose.
OUTBOUND=$WC_ARENA_OUTBOUND

# ------------------------------------------------------------------ helpers --
# The compositor's resident set, in bytes, **as the guest's own kernel accounts
# it**. Sampled inside the virtual machine by the payload script into
# `/sv/rss.log`; this reads the last sample at or before now.
#
# The guest's own view is the only one that answers the question. This
# workstation's view of the same memory is QEMU's mapping of a memfd, which does
# not shrink when a process inside the guest frees pages -- so a host-side
# number would say "nothing was returned" for a release that worked perfectly,
# and "everything was returned" for one that did nothing.
rss_now() {
  local last
  last=$(grep -a ' rss ' "$RUNDIR/rss.log" 2>/dev/null | tail -1)
  echo "${last##* }"
}
mib() { awk -v b="${1:-0}" 'BEGIN { printf "%.1f", b / 1048576 }'; }

# Wait for a line to appear in a file. `waitfor <file> <regex> <seconds> <what>`
waitfor() {
  local f=$1 re=$2 secs=$3 what=$4 i
  for ((i = 0; i < secs * 5; i++)); do
    grep -aqE "$re" "$f" 2>/dev/null && return 0
    sleep 0.2
  done
  note "timed out after ${secs}s waiting for $what in ${f##*/}"
  return 1
}

QEMU=""; HOSTD=""; PROXY=""; KEEPALIVE=""; HOG=""
cleanup() {
  for p in "$HOG" "$KEEPALIVE" "$PROXY" "$HOSTD" "$QEMU"; do
    [ -n "$p" ] && kill "$p" 2>/dev/null
  done
  wait 2>/dev/null
  if [ "$KEEP" = 1 ]; then
    note "logs in $RUNDIR"
  else
    rm -rf "$RUNDIR"
  fi
}
trap cleanup EXIT

# ------------------------------------------------------------ the run itself --
exec 9>/tmp/.wc-release-vm.lock
flock -n 9 || { echo "another run is in progress"; exit 1; }

echo "== release over a real VM boundary: ${ARENA_MIB} MiB floor, $POOLS pool(s) of $POOL_BYTES bytes"

# The binaries this run is about, checked against the sources they were built
# from rather than against a timestamp.
fresh "$LINUX_BIN/wc-compositor" "cargo build -p wc-compositor" || exit 2
fresh "$LINUX_BIN/wc-proxy"      "cargo build -p wc-proxy"      || exit 2
fresh "$LINUX_BIN/wc-arena-host" "cargo build -p wc-arena-host" || exit 2
fresh "$LINUX_BIN/sv-hostd"      "cargo build -p sv-hostd"      || exit 2

HOG_BIN="$RUNDIR/wc-poolhog"
rm -rf "$RUNDIR"; mkdir -p "$RUNDIR/rt"; chmod 700 "$RUNDIR/rt"
export SV_RUNDIR="$RUNDIR"
gcc -O2 -o "$HOG_BIN" "$ROOT/scripts/wc-poolhog.c" -lwayland-client || {
  echo "could not build wc-poolhog"; exit 2; }

VM=$(nix build "$ROOT#svGuest" --no-link --print-out-paths) || exit 2

# The substrate, before anything is measured. `guest-ram` also *removes*
# SV_ALLOW_FILE_ARENA from this run's environment, so neither binary can open a
# file even if the shell that started this had it set.
wc_substrate_declare guest-ram || exit 2

say "booting the guest"
( cd "$RUNDIR" && "$VM/bin/run-nixos-vm" -nographic ) > "$RUNDIR/console.log" 2>&1 &
QEMU=$!
for _ in $(seq 1 120); do [ -S "$QMP" ] && break; sleep 1; done
[ -S "$QMP" ] || { bad "no QMP socket: the guest did not boot"; tail -20 "$RUNDIR/console.log"; exit 1; }

# The host half of the cold path first: the guest speaks first, and a listener
# that is not there yet is a handshake that hangs.
say "sv-hostd, waiting for the guest's ring region"
"$LINUX_BIN/sv-hostd" qemu-serve \
  --qmp "$QMP" --cold "$RUNDIR/cold.sock" --socket "$RUNDIR/wl.sock" \
  --vm "$VM_HINT" --clients 1 > "$RUNDIR/hostd.log" 2>&1 &
HOSTD=$!

cp "$LINUX_BIN/wc-compositor" "$RUNDIR/wc-compositor"
# The payload. Three jobs: say which kernel this is, sample the compositor's
# resident set from inside the guest, and run the compositor.
#
# `--arena-guest-ram` with an outbound tail, and no `--arena`, no
# `--arena-provide`, no `--shared-dir`: there is no file on this path and
# nothing for either side to open by name.
cat > "$RUNDIR/guest-cmd.sh" <<GUEST
export XDG_RUNTIME_DIR=/tmp
# The assertion that this ran in the virtual machine and not beside it. Printed
# by the guest's own kernel, into the guest's own log.
echo "guest kernel: \$(uname -r)"
echo "guest uname: \$(uname -a)"
# §85: a guest-RAM arena is large pages or nothing, and on Linux that is
# MAP_HUGETLB from the pool this reserves (none by default). The mlock path is
# --arena-small-pages-may-corrupt-guest-memory and this leg does not take it.
echo $((ARENA_MIB / 2 + 128)) > /proc/sys/vm/nr_hugepages
echo "guest hugetlb: \$(grep -E 'HugePages_(Total|Free)' /proc/meminfo | tr -s ' ' | tr '\n' ' ')"
/sv/wc-compositor --wayland-cold /dev/virtio-ports/org.sirati.svcold.0 \
  --socket wcrel --timeout 400 \
  --arena-guest-ram 0:$((ARENA_MIB * 1024 * 1024)) --arena-outbound 0:$OUTBOUND \
  --arena-frames /sv/arena.frames &
CPID=\$!
echo "compositor pid \$CPID"
# The number the whole run is about, from /proc inside the guest. Field 2 of
# statm is the resident set in pages.
while kill -0 \$CPID 2>/dev/null; do
  read -r _ r _ < /proc/\$CPID/statm
  echo "\$(date +%s) rss \$((r * 4096))" >> /sv/rss.log
  sleep 0.5
done
wait \$CPID
GUEST
say "compositor in the guest: ${ARENA_MIB} MiB arena in its own RAM"

# What the guest is about to execute is what was built here. The runner is `g`,
# so the hash is taken on the path the guest will `exec`.
deployed_is "$LINUX_BIN/wc-compositor" "/sv/wc-compositor" g \
  "the copy into the run directory failed" || exit 2

# ---------------------------------------------------------------- the locate --
# `wc_locate_arena` waits for the compositor's own "provided from guest RAM"
# line, so this is also how the run waits for the guest to be up.
wc_locate_arena "/sv/guest.log" "/sv/arena.frames" "$RUNDIR/plan" "$RUNDIR" || {
  say "----- guest -----"; sed 's/^/        /' "$RUNDIR/guest.log" 2>/dev/null | head -30
  exit 1; }

GUEST_KERNEL=$(grep -a '^guest kernel: ' "$RUNDIR/guest.log" | tail -1 | cut -d' ' -f3)
HOST_KERNEL=$(uname -r)
if [ -n "$GUEST_KERNEL" ] && [ "$GUEST_KERNEL" != "$HOST_KERNEL" ]; then
  ok "the compositor ran in another kernel: guest Linux $GUEST_KERNEL, this workstation Linux $HOST_KERNEL"
else
  bad "the guest reported kernel '${GUEST_KERNEL:-none}' against this workstation's $HOST_KERNEL -- a run that cannot tell the two apart cannot claim a boundary"
fi

# Both facts are in now, so the topology can be declared and proven.
wc_topology_declare cross-vm \
  "wc-compositor in a QEMU guest on $HOST, wc-proxy and the clients on $HOST, arena in the guest's own RAM" \
  || exit 2
wc_assert_topology "$RUNDIR/guest.log" "release" || exit 1

waitfor "$RUNDIR/hostd.log" "listening on .* ICD client" 120 "sv-hostd to offer the ring region" || {
  bad "sv-hostd never offered the ring region"; sed 's/^/        /' "$RUNDIR/hostd.log"; exit 1; }

# ----------------------------------------------------------------- the proxy --
say "proxy, over the guest's own RAM, with growth and release armed"
XDG_RUNTIME_DIR="$RUNDIR/rt" "$LINUX_BIN/wc-proxy" \
  --upstream-ring-attach "$RUNDIR/wl.sock" --socket wl-proxy --runtime-dir "$RUNDIR/rt" \
  --transport arena --arena-plan "$RUNDIR/plan" --arena-id 0 \
  --arena-grow --arena-grow-idle "$GROW_IDLE" --arena-grow-step "$GROW_STEP" \
  --arena-locator "$LINUX_BIN/wc-arena-host" --arena-qmp "$QMP" --arena-vm "$VM_HINT" \
  --copy pool --no-icons --no-clipboard \
  --accept-timeout 300 --timeout 300 > "$RUNDIR/proxy.log" 2>&1 &
PROXY=$!
waitfor "$RUNDIR/proxy.log" '^\[proxy\] listening on' 60 "the proxy to listen" || {
  bad "the proxy never listened"; sed 's/^/        /' "$RUNDIR/proxy.log"; exit 1; }

client() { # client <pools> <tag>
  XDG_RUNTIME_DIR="$RUNDIR/rt" WAYLAND_DISPLAY=wl-proxy \
    "$HOG_BIN" "$1" "$POOL_BYTES" "$2" >> "$RUNDIR/client-$2.log" 2>&1 &
  echo $!
}

# =============================================================== LEG A ========
echo
echo "== leg A: a client is killed while another connection keeps the clock turning"

KEEPALIVE=$(client 0 keep)
waitfor "$RUNDIR/client-keep.log" 'pool\(s\) of' 30 "the keepalive to connect" \
  || bad "the keepalive never connected"
# The channel every later assertion rides on. It is created at the *first*
# client, on whichever accept path that client used -- and the ring path had no
# such call until §72, which is why this is asserted and not assumed.
waitfor "$RUNDIR/guest.log" 'growth channel up' 30 "the growth channel" \
  && ok "the growth channel came up on a ring accept: the arena can be asked to grow and to give back" \
  || bad "no growth channel: nothing can be grown and nothing can be released, because Op::Release rides the same ring"
sleep 1
RSS_BEFORE=$(rss_now)
note "before: the compositor's resident set in the guest is $RSS_BEFORE bytes ($(mib "$RSS_BEFORE") MiB)"

say "the hog: $POOLS pools of $POOL_BYTES bytes, held and never destroyed"
HOG=$(client "$POOLS" hog)
waitfor "$RUNDIR/client-hog.log" 'pool\(s\) of' 60 "the hog to take its pools" \
  || bad "the hog never took its pools"
# The pools do not fit the floor, so the allocator asks the guest for a region:
# a request across the hypervisor, a `VirtualAlloc`/`mmap` plus a lock and a
# stamp of every page in the guest, a locate here, and an append to the plan.
# The peak is read after that, because that is the only thing that moves a
# resident set -- writing into pages the guest already committed does not.
waitfor "$RUNDIR/guest.log" 'grew arena' 90 "the guest to commit another region" \
  || note "no growth line in the guest's log"
sleep 3
RSS_PEAK=$(rss_now)
note "at peak: the compositor's resident set in the guest is $RSS_PEAK bytes ($(mib "$RSS_PEAK") MiB)"
OUT_PEAK=$(grep -ao '[0-9]* of [0-9]* bytes out' "$RUNDIR/proxy.log" | tail -1)
GREW=$(grep -a 'grew arena' "$RUNDIR/guest.log" | tail -1)
[ -n "$GREW" ] && ok "the guest grew: ${GREW#wc-compositor: }" \
                || bad "the guest never grew a region, so there is nothing that could come back"

say "kill -9 the hog -- no wl_shm_pool.destroy is sent on this path, which is the bug"
kill -9 "$HOG" 2>/dev/null; wait "$HOG" 2>/dev/null; HOG=""

waitfor "$RUNDIR/proxy.log" 'session ended holding [0-9]+ pool' 60 "the session's Drop" \
  && ok "the pools came back when the client died: $(grep -a 'session ended holding' "$RUNDIR/proxy.log" | tail -1 | sed 's/^\[proxy\] //')" \
  || bad "no session-ended line: the pools of a killed client were not returned"

waitfor "$RUNDIR/proxy.log" 'arena region [0-9]+ returned (while idle|by the accept loop)' 120 "the region to be returned" \
  && ok "the region went back: $(grep -aoE 'arena region [0-9]+ returned (while idle|by the accept loop): .*' "$RUNDIR/proxy.log" | tail -1)" \
  || bad "the empty region was never returned to the guest"

waitfor "$RUNDIR/guest.log" 'released arena [0-9]+' 60 "the guest to free the pages" \
  && ok "the guest freed the pages: $(grep -ao 'released arena [0-9]*: .*' "$RUNDIR/guest.log" | tail -1)" \
  || bad "the compositor never released a region, so no guest RAM was given back"

sleep 3
RSS_AFTER=$(rss_now)
note "after: the compositor's resident set in the guest is $RSS_AFTER bytes ($(mib "$RSS_AFTER") MiB)"

RELEASED_BYTES=$(grep -aoE 'arena region [0-9]+ returned (while idle|by the accept loop): [0-9]+' "$RUNDIR/proxy.log" \
  | tail -1 | grep -o '[0-9]*$')
RELEASED_BYTES=${RELEASED_BYTES:-0}
DROP=$(( ${RSS_PEAK:-0} - ${RSS_AFTER:-0} ))
if [ "${RSS_PEAK:-0}" -gt "${RSS_BEFORE:-0}" ]; then
  ok "the arena is the dominant term: the resident set rose by $(( RSS_PEAK - RSS_BEFORE )) bytes when the pools were taken"
else
  bad "the resident set did not rise under load ($RSS_BEFORE -> $RSS_PEAK): this run did not contain the quantity it is about"
fi
if [ "$RELEASED_BYTES" -gt 0 ] && [ "$DROP" -ge "$RELEASED_BYTES" ]; then
  ok "THE GUEST GOT IT BACK: resident set fell $DROP bytes ($(mib $DROP) MiB) against a $RELEASED_BYTES-byte region returned"
else
  bad "resident set fell $DROP bytes against a $RELEASED_BYTES-byte region returned -- the accounting says released and the guest's kernel does not agree"
fi

# The proxy's own account of the arena, which is a different witness from the
# guest's resident set and has to agree with it.
FINAL=$(grep -ao 'now [0-9.]* MiB in [0-9]* region(s).*' "$RUNDIR/proxy.log" | tail -1)
[ -n "$FINAL" ] && note "the proxy's account: $FINAL"

# =============================================================== LEG B ========
# **The zero-client case, which is now a fix and not a limitation.**
#
# With no client connected there is no pump, so nothing turned the release
# policy's clock: the last session's `Drop` returned its pools and the empty
# region stayed the guest's. Measured before the fix, on this leg: 15 s with
# nobody connected returned nothing, and one reconnect brought it back in
# seconds -- so the region was returnable the whole time and nothing was asking.
#
# `Arena::frame_idle` from the accept loop is that missing asker. This leg is
# what holds it: the last client is killed, **nobody reconnects**, and the
# region has to come back anyway.
echo
echo "== leg B: the same kill with nothing else connected at all"
kill -9 "$KEEPALIVE" 2>/dev/null; wait "$KEEPALIVE" 2>/dev/null; KEEPALIVE=""
sleep 2
MARK=$(( $(wc -l < "$RUNDIR/proxy.log") + 1 ))

HOG=$(client "$POOLS" alone)
waitfor "$RUNDIR/client-alone.log" 'pool\(s\) of' 60 "the lone hog to take its pools" \
  || bad "the lone hog never took its pools"
waitfor "$RUNDIR/guest.log" 'grew arena 2' 90 "the second region" \
  || note "no second growth"
sleep 2
RSS_B_PEAK=$(rss_now)
say "kill -9 the lone client: nothing is left connected, and nothing will reconnect"
kill -9 "$HOG" 2>/dev/null; wait "$HOG" 2>/dev/null; HOG=""

# Deliberately no client for the whole of this wait. If the region comes back it
# came back because the accept loop turned the clock, and there is nothing else
# it could have been.
#
# **Only lines after `$MARK` count**, and that is not tidiness. Leg A's own
# release is already in this file and matches the same pattern, so a check over
# the whole log would pass before this leg had done anything — a gate that
# cannot fail, which is the exact defect this project keeps finding. The first
# spelling of this check had it.
after_mark() { tail -n +"$MARK" "$RUNDIR/proxy.log" 2>/dev/null; }
B_FOUND=""
for _ in $(seq 1 450); do
  B_FOUND=$(after_mark | grep -aoE 'arena region [0-9]+ returned by the accept loop: [0-9]+ bytes.*' | tail -1)
  [ -n "$B_FOUND" ] && break
  sleep 0.2
done
if [ -n "$B_FOUND" ]; then
  ok "the accept loop returned it with nobody connected: $B_FOUND"
else
  bad "with zero clients the empty region stayed the guest's -- the accept loop is not turning the clock"
fi
# The other half of the claim, and it is the half a log line cannot make: that
# there really was nobody. `pgrep -x` on the executable name rather than
# `pgrep -f` on a command line this script's own shell contains -- wc-binaries.sh
# names the three times that has bitten this project.
LIVE=$(pgrep -x wc-poolhog | wc -l)
[ "${LIVE:-0}" -eq 0 ] \
  && ok "and there really was nobody: 0 wc-poolhog processes alive across that wait" \
  || bad "$LIVE wc-poolhog still alive, so this leg did not test the zero-client case"
sleep 3
RSS_B_AFTER=$(rss_now)
B_RELEASED=$(printf '%s' "$B_FOUND" | grep -oE ': [0-9]+ bytes' | grep -oE '[0-9]+')
B_RELEASED=${B_RELEASED:-0}
B_DROP=$(( ${RSS_B_PEAK:-0} - ${RSS_B_AFTER:-0} ))
if [ "$B_RELEASED" -gt 0 ] && [ "$B_DROP" -ge "$B_RELEASED" ]; then
  ok "and the guest's kernel agrees with nobody connected: resident set fell $B_DROP bytes against $B_RELEASED returned"
else
  bad "resident set fell $B_DROP bytes against $B_RELEASED returned with no client connected"
fi

# §61.4's ring, guarded rather than hoped for. A region's frame list is one
# `u64` per page, so a 32 MiB region is 8192 frames = 65536 bytes, plus
# `sv-wire`'s ReplyHeader = 65600 against a 65536-byte ring. On Windows that was
# rare -- it needed full fragmentation. On a Linux guest `/proc/self/pagemap`
# always names every page, so it is **deterministic at the third step** of the
# 8/16/32 MiB series, which is why this harness's step keeps both its growths
# under it. If the wall is hit the run says so rather than reporting a growth
# that did not happen.
if grep -aq 'cannot fit a .* ring' "$RUNDIR/guest.log"; then
  bad "§61.4's ring wall: $(grep -ao 'growth ring: .*' "$RUNDIR/guest.log" | tail -1)"
else
  ok "no growth reply overflowed the ring: every frame list this run produced fitted"
fi

# ------------------------------------------------------------------ the table --
echo
echo "== the three numbers, from the guest's own process view (topology: cross-vm, Linux host -> Linux VM)"
printf '   %-38s %s\n' "before (compositor up, no pools out)" "$RSS_BEFORE bytes ($(mib "$RSS_BEFORE") MiB)"
printf '   %-38s %s\n' "at peak (pools taken, one growth)"     "$RSS_PEAK bytes ($(mib "$RSS_PEAK") MiB)"
printf '   %-38s %s\n' "after (client killed, region back)"    "$RSS_AFTER bytes ($(mib "$RSS_AFTER") MiB)"
echo
say "----- guest -----";  grep -aE "guest kernel|provided from guest RAM|growth channel|grew arena|released arena|outbound window" "$RUNDIR/guest.log" | sed 's/^/  /'
say "----- proxy -----";  grep -aE "transport arena|may grow|region [0-9]+ .*bytes|session ended holding|returned while idle|returned by the accept loop|peak resident|could not publish" "$RUNDIR/proxy.log" | sed 's/^/  /'

echo
echo "== $pass passed, $fail failed =="
[ "$fail" -eq 0 ]
