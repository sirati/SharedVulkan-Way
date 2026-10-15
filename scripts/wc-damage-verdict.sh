# Sourced, not run. Block 12's verdict on one cross-VM damage run, shared by
# `wc-guest.sh damage` and `wc-desktop.sh damage`.
#
#   wc_damage_verdict <tag> <scroll-log> <proxy-log> <compositor-log>
#
# Needs `ok`/`bad`/`note` from the caller, and `wc-substrate.sh` and
# `wc-topology.sh` sourced, declared and (for the substrate) located: the
# assertions below read WC_SUBSTRATE, WC_SUBSTRATE_LOCATED and WC_TOPOLOGY.
#
# WHY IT IS ITS OWN FILE
#
# This was the second half of `wc-guest.sh`'s `do_damage`. The leg moved to the
# interactive session (`wc-desktop.sh damage`, docs/waycompose-design.md §84)
# because the ssh session's windows are never visible and so are never
# scrolled, and the verdict on what that run produced is the same verdict: a
# second copy of it in the second harness would be a second thing to drift,
# which is how this tree's harnesses failed nine checks on a working
# compositor in one afternoon. Every line below is `do_damage`'s, unchanged in
# what it asserts; only where it reads its three inputs from is a parameter.
#
# Returns non-zero when the run cannot be judged at all (no scroll, or no
# commit verified), so a caller can stop rather than print downstream checks
# about a run that did not contain what it was about.
wc_damage_verdict() {
  local tag="$1" scroll_log="$2" proxy_log="$3" comp_log="$4"

  # **Launched is not injected** (§57). A locked session puts the input
  # desktop on `Winlogon`; `SendInput` returns 0 and the probe would otherwise
  # run to completion having moved nothing, and every number downstream would
  # be a real measurement of an unscrolled browser. The probe counts refusals;
  # this reads its accounting rather than its exit status.
  local scrolled
  scrolled=$(grep -o '[0-9]* of [0-9]* notches towards the user accepted' "$scroll_log" 2>/dev/null | tail -1)
  if [ -z "$scrolled" ]; then
    bad "$tag: the scroll probe never reported an accepted count; the wheel cannot be assumed to have moved"
    sed 's/^/        /' "$scroll_log" 2>/dev/null | head -12
    return 1
  elif grep -q "REFUSED by SendInput" "$scroll_log"; then
    bad "$tag: SendInput refused notches -- $scrolled. If the guest session is LOCKED this is expected and the run is void: unlock it and re-run."
    grep -E "REFUSED|LOCKED|scrolled nothing" "$scroll_log" | sed 's/^/        /'
    return 1
  fi
  ok "$tag: the guest accepted the wheel -- $scrolled"

  # The substrate and the topology, from the run's own evidence. A hypervisor
  # in the path is exactly what makes guest RAM locatable, so the locate is
  # the proof of both.
  wc_assert_substrate "$comp_log" "$tag"
  wc_assert_topology "$comp_log" "$tag"

  # THE ASSERTION, and it is `wc-damage.sh`'s, over a boundary.
  local verified skipped unreached stale worst commits
  verified=$(grep -o 'verified [0-9]* commits' "$proxy_log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  skipped=$(grep -o '([0-9]* state-only, unverifiable' "$proxy_log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  unreached=$(grep -o '[0-9]* never reached the copy' "$proxy_log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  stale=$(grep -o '[0-9]* stale (worst [0-9]* rows)' "$proxy_log" | awk -F' stale' '{print $1}' | awk '{s+=$1} END {print s+0}')
  worst=$(grep -o 'worst [0-9]* rows' "$proxy_log" | grep -o '[0-9]*' | sort -n | tail -1)
  commits=$(grep -o '[0-9]* commits,' "$proxy_log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')

  if [ "${verified:-0}" -eq 0 ]; then
    bad "$tag: no commit was checked -- --verify-damage did not reach the proxy on the host"
    tail -8 "$proxy_log" | sed 's/^/        /'
    return 1
  fi

  # The identity, as §46.3 defines it: every commit is checked, state-only, or
  # never reached the copy.
  local classified=$(( ${verified:-0} + ${skipped:-0} + ${unreached:-0} ))
  [ "$classified" -eq "${commits:-0}" ] \
    && ok "$tag: every one of $commits commits is accounted for -- $verified checked, $skipped attached no buffer, $unreached never reached the copy" \
    || bad "$tag: $classified of $commits commits classified; $(( ${commits:-0} - classified )) unaccounted for"

  if [ "${stale:-0}" -eq 0 ]; then
    ok "$tag: over a real VM boundary, the region matched the client on every one of $verified checked commits"
  else
    bad "$tag: $stale of $verified commits left the region stale, worst ${worst:-?} rows -- and this is guest RAM, not a fixture"
    grep -o 'stale: .*' "$proxy_log" | head -6 | sed 's/^/        /'
  fi

  # What this leg is permitted to say and the Wine legs are not. Called for its
  # permission, not its refusal; the claim itself is not made here (§46.7
  # item 5).
  if wc_latency_claim "the present latency is measurable on this leg"; then
    ok "$tag: this leg may make a claim about cross-VM latency -- there is a boundary in it"
  fi
  return 0
}
