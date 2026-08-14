# wc-topology.sh -- sourced, never run. The one place a harness declares whether
# there is a **hypervisor in the path**, and the gate that makes a latency claim
# unavailable to a run that has none.
#
#   . "$(dirname "$0")/wc-topology.sh"
#
# WHY THIS FILE EXISTS, AND IT IS A MISTAKE RATHER THAN A DESIGN
#
# §27.6 claimed the user's second symptom -- "stop rendering of scrolling" --
# did not reproduce, on the strength of the compositor presenting 1626 of the
# 1628 buffer-carrying commits it received. That statistic was correct. The
# inference from it was not, in two separate ways, and the second is what this
# file is for.
#
#   1. **A presented-over-committed ratio counts drops, not latency**, and the
#      two are independent. At a 216.9 ms present the compositor still presents
#      every commit it is given -- the client is throttled by its own frame
#      callbacks and simply commits less often -- so the ratio sits at 0.999
#      while the screen crawls.
#
#   2. **The harness that produced it has no VM boundary in it at all.**
#      `wc-damage.sh` runs Firefox, `wc-proxy` and `wc-compositor.exe` under
#      Wine on one Linux box. The cross-VM present is not slow there; it does
#      not exist there. The quantity the conclusion was about was structurally
#      absent from the experiment.
#
# §13.8's rule is "keep the instrument out of the loop". This was the same
# error one level up: not an instrument inside the loop, but a **loop with the
# subject outside it**. A rule stated in prose is a rule the next person has to
# remember, so it is stated here as a function that refuses.
#
# WHAT THIS DOES NOT DO
#
# It does not decide whether a run is cross-VM. It cannot: only the harness
# knows how it wired itself up. What it does is make the answer **explicit,
# recorded in the run's own output, and load-bearing** -- a harness that never
# declares gets no latency claims, which is the safe default and the one that
# would have stopped §27.6.
#
# WHAT THE CALLER MUST ALREADY HAVE
#
#   ok/bad/note   the reporters. Fallbacks below, so sourcing cannot break a
#                 harness that spells them differently.
#
# and, if it wants `wc_assert_topology` to be able to *prove* a cross-VM claim,
# `wc-substrate.sh` sourced and `wc_locate_arena` already run -- see the note on
# that function for why the two are coupled rather than independent.

type ok   >/dev/null 2>&1 || ok()   { echo "  PASS  $*"; }
type bad  >/dev/null 2>&1 || bad()  { echo "  FAIL  $*"; }
type note >/dev/null 2>&1 || note() { echo "  ----  $*"; }

# Deliberately empty rather than defaulted to either value. "Nobody said" is a
# third state and it is the one almost every harness in this tree is in; it must
# not be silently equal to `same-box`, because a harness that grew a cross-VM leg
# and forgot to declare it would then be quietly refused its latency claims with
# no line saying why.
WC_TOPOLOGY="${WC_TOPOLOGY:-}"
WC_TOPOLOGY_WHY="${WC_TOPOLOGY_WHY:-}"

# wc_topology_declare cross-vm|same-box [why]
#
# `same-box` is always permitted: a run with no hypervisor is a legitimate run,
# it is what every Wine leg in this tree is, and most of what this project
# measures is unaffected by the boundary. What it may not do is draw a
# conclusion about latency, and `wc_latency_claim` is what enforces that.
#
# `cross-vm` is a claim about the machine and is checked against the two facts
# that make one possible: a VM host to run the proxy on (`WC_HOST`), because the
# arena locate needs QEMU's monitor, and a guest to reach (`WC_GUEST` or
# `WC_GUEST_CONFIGURED`). Declaring it without them is refused rather than
# believed -- the same shape as `wc_substrate_declare` refusing a file fixture on
# a box that has a guest, and for the same reason: the declaration is the thing
# a later reader will trust.
wc_topology_declare() {
  local kind="$1" why="${2:-}"
  case "$kind" in
    cross-vm)
      if [ -z "${WC_HOST:-}" ]; then
        bad "REFUSED to declare cross-vm: WC_HOST is unset, so there is no machine running QEMU to be on the near side of a boundary"
        return 1
      fi
      if [ -z "${WC_GUEST:-}${WC_GUEST_CONFIGURED:-}" ]; then
        bad "REFUSED to declare cross-vm: no guest is configured, so there is nothing on the far side"
        return 1
      fi
      WC_TOPOLOGY=cross-vm
      WC_TOPOLOGY_WHY="${why:-no reason given}"
      note "topology: CROSS-VM -- a hypervisor is in the path. ${WC_TOPOLOGY_WHY}"
      return 0 ;;
    same-box)
      WC_TOPOLOGY=same-box
      WC_TOPOLOGY_WHY="${why:-no reason given}"
      note "topology: SAME-BOX -- no hypervisor in the path. Nothing measured here is"
      note "a statement about cross-VM latency, and wc_latency_claim will refuse one."
      note "Reason: ${WC_TOPOLOGY_WHY}"
      return 0 ;;
    *)
      bad "wc_topology_declare: unknown topology $kind"; return 1 ;;
  esac
}

# wc_latency_claim <what-is-being-claimed>
#
# **The gate.** Call this before asserting anything whose truth depends on how
# long something took across the boundary; it returns non-zero, with a line
# saying why, unless the run has declared and proven a cross-VM topology.
#
# Returns rather than exits, so a harness can degrade -- report the measurement
# as a number without the conclusion -- instead of dying. What it must not do is
# let the assertion through, because the assertion is the part that was wrong.
#
#   if wc_latency_claim "the present is not the bottleneck"; then
#       ... assert on it ...
#   else
#       note "measured $n, but this leg cannot conclude from it"
#   fi
# **It reports with `note` and never with `bad`, and that is not softness.** The
# first version called `bad` on every refusal, which is wrong for a reason that
# took a run to see: a refusal is the *expected* outcome for every same-box leg
# in this tree, so a gate that counts its own refusals as failures cannot be
# probed. `wc-damage.sh` calls this expecting to be refused and asserts on it —
# and with `bad` inside, that harness reported "11 passed, 1 failed" with no FAIL
# line anywhere in its output, because the counter had been incremented by a
# message that was redirected away.
#
# So this function answers "is the claim available", and **the caller decides
# what that means**. A harness that wants a refusal to be fatal writes `bad` in
# its own else-branch, where the reader can see it.
wc_latency_claim() {
  local what="${1:-a latency claim}"
  case "$WC_TOPOLOGY" in
    cross-vm)
      if [ -z "${WC_SUBSTRATE_LOCATED:-}" ]; then
        note "REFUSED '$what': cross-vm was declared but no locate proved it -- see wc_assert_topology"
        return 1
      fi
      return 0 ;;
    same-box)
      note "REFUSED '$what': this leg declared same-box, so there is no cross-VM present in it to be fast or slow."
      note "docs/waycompose-design.md §27.6 is what happens when this is claimed anyway:"
      note "a correct statistic, an inference the experiment could not support, about a"
      note "quantity that was not in the experiment at all."
      return 1 ;;
    *)
      note "REFUSED '$what': no topology was declared, so whether a hypervisor is in the path is unknown."
      note "Call wc_topology_declare before making a claim that depends on the answer."
      return 1 ;;
  esac
}

# wc_assert_topology <compositor-log> [tag]
#
# The declaration, checked against the run's own evidence rather than trusted --
# `wc_assert_substrate`'s shape, and coupled to it on purpose.
#
# **Why the substrate is the evidence.** There is no single line in any log that
# says "a hypervisor was in the path", and inventing one would be a fourth place
# for this fact to be recorded and disagree. But the two are not independent: a
# guest-RAM arena is located by reading QEMU's monitor and translating the
# guest's page frames (`wc-arena-host`), which cannot succeed unless there is a
# QEMU with a guest in it. So a proven `guest-ram` substrate **is** a proven
# hypervisor, and a `cross-vm` leg that served its pixels from a file has either
# not crossed a boundary or has crossed it and then measured something else.
#
# The converse is not asserted: a `same-box` run may legitimately use any
# substrate, and this function only says so.
wc_assert_topology() {
  local log="$1" tag="${2:-}"
  case "$WC_TOPOLOGY" in
    cross-vm)
      if [ "${WC_SUBSTRATE:-}" != guest-ram ] || [ -z "${WC_SUBSTRATE_LOCATED:-}" ]; then
        bad "${tag:+$tag: }TOPOLOGY UNPROVEN: declared cross-vm, but the substrate is '${WC_SUBSTRATE:-none}'${WC_SUBSTRATE_LOCATED:+ (located)} -- a hypervisor in the path is what makes guest RAM locatable, so a run that did not locate it has not shown one"
        return 1
      fi
      ok "${tag:+$tag: }TOPOLOGY cross-vm: proven by $WC_SUBSTRATE_LOCATED pages of guest RAM located by ${WC_SUBSTRATE_LOCATOR:-a locate} -- QEMU's monitor cannot answer for a machine that is not running one"
      return 0 ;;
    same-box)
      ok "${tag:+$tag: }TOPOLOGY same-box: no hypervisor in the path, and no claim here is about one"
      return 0 ;;
    *)
      bad "${tag:+$tag: }no topology was declared before this run"
      return 1 ;;
  esac
}
