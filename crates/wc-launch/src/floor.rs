//! How big the startup region is, and **why** — as a policy with its evidence,
//! rather than as a literal.
//!
//! # Why this is a module and not a number in [`crate::config`]
//!
//! Because the number has changed four times and every change was a judgement
//! about a *measurement*, not about a preference: 768 → 32 → 64, and 16 waiting
//! on a condition that had not been met. Each time, the reasoning lived in a
//! doc comment on the field, the field was set somewhere else, and the two
//! drifted — the comment on `arena_mib` was still explaining that "growth is
//! armed and inert" and listing the two absences that made it so, several
//! commits after both had been closed.
//!
//! So the size is chosen *here*, by name, next to the numbers that choose it,
//! and [`Floor::CURRENT`] is the single line that moves. When the guest run
//! happens, the change is one identifier — not a hunt through the launcher for
//! a `64` and a stale paragraph.
//!
//! # The asymmetry that makes all of this worth caring about
//!
//! The startup region's pages are `VirtualLock`ed, so they are **non-pageable**
//! — not merely committed, but gone from Windows entirely, for anything, for as
//! long as the session lasts. And the startup region is **never reclaimed**: it
//! holds the outbound window, whose bytes are keymaps and clipboard offers live
//! in clients' hands, so release can never return it. Its size is therefore a
//! floor for the whole session in the strongest sense available.
//!
//! That is why 768 MiB reaching a user was a bug and not a tuning error, and it
//! is why the correct direction is always "as small as the evidence allows".
//!
//! # IVSHMEM ARENA: this is now a guest-RAM policy the launcher does not use
//!
//! docs/waycompose-design.md §82 moved the launcher onto BAR2 of an ivshmem device,
//! whose size is set on the VM host, is not `VirtualLock`ed and is not charged
//! against the guest's RAM. None of the asymmetry above applies to it. The
//! policy and its evidence stay, because `[session] arena-mib` still defaults
//! from [`Floor::CURRENT`] and the developer harnesses still start guest-RAM
//! arenas; only `mib` has a caller in this binary, which is why the dead-code
//! lint is quieted for the module rather than the evidence deleted.
#![allow(dead_code)]

/// What one region has to hold **on its own**, from measurements on the
/// deployment this launcher runs.
pub mod measured {
    /// What a *finished* Firefox has out across two connections, in bytes.
    ///
    /// 28–30 MB, and the upper end is used because a floor sized by the lower
    /// end of a range is a floor that is sometimes wrong. The trap this
    /// replaced: a `6877184 of … bytes out` high-water taken seconds after
    /// start-up looks like the number and is not — it is Firefox before it has
    /// finished starting.
    pub const FIREFOX_RESIDENT: u64 = 30 * 1000 * 1000;

    /// The largest single pool a running Firefox asks for on top of that, in
    /// bytes. This is the allocation that actually failed in §28.12's report:
    /// `could not publish 4608000 bytes … cannot grow`.
    pub const LARGEST_POOL: u64 = 4_608_000;

    /// Peak resident across *all* regions in a heavier session measured
    /// elsewhere, in bytes. 40 MiB in 2 regions.
    pub const HEAVIER_SESSION_PEAK: u64 = 40 << 20;

    /// **A size measured to refuse**, in bytes.
    ///
    /// Three warm clicks on a 32 MiB region produced **ten refusals** —
    /// `cannot serve 4608000 bytes … 4419584 free across 1 region(s), largest
    /// run 3575808`, which is free space that fragmentation had made unusable —
    /// and warm clicks of **2144–2777 ms** against 1414–1601 before, because a
    /// held-back pool is a surface that stops updating until there is room.
    ///
    /// It is recorded as a hard lower bound rather than as an anecdote: any
    /// stand-alone floor at or below this is known-bad, and
    /// `a_stand_alone_floor_clears_every_measurement` holds the policy to it.
    pub const REFUSES: u64 = 32 << 20;

    /// One step of the proxy's geometric growth series, in bytes.
    ///
    /// **This must equal `wc_proxy::pool::GROW_STEP`**, and it is written out
    /// rather than imported because `wc-launch` is a Windows guest binary with
    /// no dependency on the host-side proxy crate — importing it would drag the
    /// whole host half across the boundary to share a constant. `scripts/wc-session`
    /// spells the same number a third time as `WC_LAUNCH_GROW_STEP`. Three
    /// spellings is two too many and the disagreement would be silent, so the
    /// test below at least pins this one to the intended start size.
    pub const GROW_STEP: u64 = 16 << 20;
}

/// Why the startup region is the size it is.
///
/// The two arms are not "small" and "large" — they are two different *claims
/// about the tree*, and which one is true is a fact somebody has to establish
/// by running it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Floor {
    /// **Growth has not been demonstrated on a guest**, so the startup region
    /// must hold the session's whole peak by itself.
    ///
    /// This is not the same as "growth is not implemented". As of §34 the ring
    /// is wired end to end, the compositor commits on demand and the proxy
    /// locates by frame list — and none of it has run on a guest. A floor
    /// lowered on the strength of passing unit tests is a floor lowered on the
    /// strength of the thing that was already true in §28.12, when the
    /// launcher's `--arena-grow` was passed, printed a reassuring line, and did
    /// nothing.
    MustStandAlone,

    /// **Growth has been demonstrated under real client pressure on a guest.**
    ///
    /// Then the startup region is one [`measured::GROW_STEP`], because the
    /// first region is simply the first step and a separate startup constant
    /// would be a second policy. Everything above it is bought when it is
    /// needed and given back when it is not.
    ///
    /// The evidence required to select this, stated so that it cannot be
    /// selected on less: a guest session at this floor, with a real Firefox,
    /// showing the arena **growing rather than refusing** — `0 refusals` and
    /// click latencies matching the 64 MiB baseline rather than §28.12's
    /// 2144–2777 ms.
    GrowsUnderPressure,
}

impl Floor {
    /// **The one line that moves.**
    ///
    /// `MustStandAlone` until a guest run says otherwise. See that arm for
    /// exactly what "says otherwise" has to consist of.
    pub const CURRENT: Floor = Floor::GrowsUnderPressure;

    /// The startup region, in MiB.
    pub const fn mib(self) -> u64 {
        match self {
            // 64 MiB: twelve times smaller than the 768 that reached a user,
            // clear of everything in `measured`, and equal to the cap of the
            // growth series — so it is where growth would converge anyway,
            // which makes it the least arbitrary stand-alone size available.
            //
            // Not 256, and the asymmetry in the module header is why: the pages
            // are non-pageable and the region is never reclaimed, so
            // over-reserving is the more expensive mistake.
            Floor::MustStandAlone => 64,
            Floor::GrowsUnderPressure => measured::GROW_STEP / (1024 * 1024),
        }
    }

    /// One line for the launcher's log, so that a session says which policy it
    /// started under and why. A run whose floor cannot be explained from its
    /// own output is a run somebody will have to guess about later.
    pub fn why(self) -> &'static str {
        match self {
            Floor::MustStandAlone => {
                "growth has not been demonstrated on a guest, so the startup region \
                 holds the peak alone"
            }
            Floor::GrowsUnderPressure => {
                "growth is demonstrated, so the startup region is one growth step and \
                 the rest is bought under pressure"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-alone floor has to clear **every** measurement, not the largest
    /// one. The failure it prevents is the one §28.12 recorded: a region big
    /// enough for the steady state and not for the steady state *plus* the
    /// pool that arrives on a click.
    #[test]
    fn a_stand_alone_floor_clears_every_measurement() {
        let bytes = Floor::MustStandAlone.mib() * 1024 * 1024;
        assert!(
            bytes > measured::REFUSES,
            "a floor at or below a size measured to refuse is known-bad"
        );
        assert!(
            bytes >= measured::FIREFOX_RESIDENT + measured::LARGEST_POOL,
            "the steady state plus the pool that arrives on a click"
        );
        assert!(bytes >= measured::HEAVIER_SESSION_PEAK, "and the heavier session's peak");
    }

    /// The growing floor is exactly one step, so that the first region *is* the
    /// first step. A startup size that were not a step would be a second growth
    /// policy hiding in the launcher.
    #[test]
    fn the_growing_floor_is_one_growth_step() {
        assert_eq!(Floor::GrowsUnderPressure.mib() * 1024 * 1024, measured::GROW_STEP);
    }

    /// The point of the module: changing the policy is changing one identifier.
    ///
    /// If this fails because somebody edited a literal in `config.rs`, the
    /// evidence and the number have come apart again — which is the exact
    /// failure this replaced, where `arena_mib`'s doc comment went on
    /// explaining that growth was "armed and inert" for several commits after
    /// it had been wired.
    #[test]
    fn the_config_default_is_the_policy_and_not_a_literal() {
        assert_eq!(crate::config::Config::default().arena_mib, Floor::CURRENT.mib());
    }

    /// Where the policy stands, and what earned it.
    ///
    /// This replaces `growth_has_not_been_demonstrated_on_a_guest_yet`, whose
    /// job was to make flipping `CURRENT` a deliberate act with a failing test
    /// in front of it. It has been flipped and the run that earned it is in the
    /// commit message; what the assertion guards now is the other direction —
    /// that nobody quietly puts the floor back up without saying why.
    ///
    /// The run, on testhost's guest, 2026-08-14, at this exact floor:
    ///
    /// * `4096 pages` located by frame list — 16 MiB, so the floor really was
    ///   the small one. (The first attempt at this measurement ran at 64 MiB
    ///   because the size reaches the guest through the deployed ini rather
    ///   than the environment, and its click times were quietly a 64 MiB
    ///   result. Worth recording: the numbers looked right.)
    /// * `grew 1` — `region 1: 16777216 bytes of guest RAM, 760 ms in the
    ///   guest, 27 ms locating (frame list)`.
    /// * **`refusals = 0`.**
    /// * `peak resident 32.0 MiB in 2 region(s); floor 16.0 MiB`.
    /// * warm clicks **1356 / 1375 / 1357 ms**, against **1414–1601 ms** at the
    ///   64 MiB floor and against §28.12's failing **2144–2777 ms** with ten
    ///   refusals.
    ///
    /// So a small start behaves like a large one — marginally *faster*, in
    /// fact — and peak resident is half what the stand-alone floor reserved.
    #[test]
    fn growth_is_demonstrated_and_the_floor_is_one_step() {
        assert_eq!(Floor::CURRENT, Floor::GrowsUnderPressure);
        assert_eq!(Floor::CURRENT.mib(), 16);
        // The startup region is one growth step, so the first region *is* the
        // first step and there is no second growth policy in the launcher.
        assert_eq!(Floor::CURRENT.mib() * 1024 * 1024, measured::GROW_STEP);
    }
}
