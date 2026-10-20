# Shared by the VM harness and its local stub regression test.
# Linux host monotonic time, unaffected by wall-clock adjustments.
term_now() {
  local uptime rest
  read -r uptime rest </proc/uptime
  awk -v t="$uptime" 'BEGIN { printf "%.0f\n", t * 1000 }'
}

term_pid() { [[ $1 =~ ^[1-9][0-9]*$ ]]; }

# Only a successful root-owned process-table snapshot can prove PID absence.
# SSH, sudo, ps failure or malformed output is an error, never an exit.
term_probe() {
  local pids
  pids=$(vm 'sudo -n /bin/ps -axo pid=') || return 2
  awk -v pid="$1" '
    NF != 1 || $1 !~ /^[0-9]+$/ { bad = 1 }
    $1 == pid { found = 1 }
    END { if (bad || NR == 0) exit 2; print found ? "alive" : "exited" }
  ' <<<"$pids"
}

# Deadline includes signalling and SSH observation for the released-pool case.
# allow 2 seconds for scheduling/SSH, then reject even a late positive result.
term_wait_restart() {
  local p0=$1 start deadline now remaining state p1
  term_pid "$p0" || { echo 'FAIL: invalid original daemon PID'; return 1; }
  start=$(term_now)
  deadline=$((start + 12000))
  VMT=12 vm "sudo -n kill -TERM $p0" || { echo 'FAIL: SIGTERM command failed'; return 1; }
  while :; do
    now=$(term_now)
    remaining=$((deadline - now))
    (( remaining > 0 )) || { echo 'FAIL: daemon exceeded 10 s shutdown plus 2 s observation margin'; return 1; }
    state=$(VMT="$(awk -v n="$remaining" 'BEGIN { print n / 1000 }')" term_probe "$p0") || { echo 'FAIL: daemon liveness probe failed'; return 1; }
    now=$(term_now)
    (( now <= deadline )) || { echo 'FAIL: shutdown observation exceeded deadline'; return 1; }
    [ "$state" = exited ] && break
    sleep 0.1
  done
  echo "daemon exit observed after $((now - start)) ms (10 s shutdown, 2 s observation margin)"
  # launchd gets a separate bounded 15-second restart observation window.
  deadline=$((now + 15000))
  while :; do
    now=$(term_now)
    remaining=$((deadline - now))
    (( remaining > 0 )) || { echo 'FAIL: launchd did not report a replacement daemon'; return 1; }
    p1=$(VMT="$(awk -v n="$remaining" 'BEGIN { print n / 1000 }')" term_launchd_pid) || { echo 'FAIL: launchd query failed'; return 1; }
    if [ -n "$p1" ]; then
      term_pid "$p1" || { echo 'FAIL: invalid replacement daemon PID'; return 1; }
      if [ "$p1" != "$p0" ]; then
        now=$(term_now)
        remaining=$((deadline - now))
        (( remaining > 0 )) || { echo 'FAIL: replacement observation exceeded deadline'; return 1; }
        state=$(VMT="$(awk -v n="$remaining" 'BEGIN { print n / 1000 }')" term_probe "$p1") || { echo 'FAIL: replacement liveness probe failed'; return 1; }
        now=$(term_now)
        (( now <= deadline )) || { echo 'FAIL: replacement observation exceeded deadline'; return 1; }
        if [ "$state" = alive ]; then
          echo "relaunched by launchd: pid $p1"
          return 0
        fi
      fi
    fi
    sleep 0.1
  done
}

term_launchd_pid() {
  vm 'report=$(sudo -n launchctl print system/org.waycompose.broker) || exit; printf "%s\n" "$report" | sed -n '\''s/.*pid = //p'\'''
}
