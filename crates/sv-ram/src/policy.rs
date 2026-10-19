//! When to grow and when to shrink (waycompose-design.md §108.6). Pure
//! arithmetic over numbers the caller supplies, so every rule is a test.

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// The substrate's unit; every region is a multiple.
    pub unit: u64,
    /// The smallest region announced ahead of demand.
    pub min_step: u64,
    pub max_step: u64,
    /// Free bytes below which a region is announced in the background.
    pub low_water_floor: u64,
    /// A region empty this long is retired.
    pub idle_ms: u64,
    /// No retire within this long of a grow.
    pub calm_ms: u64,
    /// A region empty this long is retired even inside the low-water
    /// margin: an idle pool gives everything back in the end.
    pub deep_idle_ms: u64,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy { unit: 2 * MIB, min_step: 64 * MIB, max_step: GIB, low_water_floor: 32 * MIB, idle_ms: 30_000, calm_ms: 10_000, deep_idle_ms: 300_000 }
    }
}

/// What the pool looks like at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Bytes leased across every region.
    pub used: u64,
    /// Bytes free across every region (including region 0).
    pub free: u64,
    /// Bytes the provider may still add (its budget).
    pub budget: u64,
}

impl Policy {
    /// The defaults, with `SV_RAM_IDLE_MS`, `SV_RAM_DEEP_IDLE_MS` and
    /// `SV_RAM_CALM_MS` overriding the times (harnesses that cannot wait
    /// five minutes).
    pub fn from_env() -> Policy {
        let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
        let d = Policy::default();
        Policy {
            idle_ms: env("SV_RAM_IDLE_MS").unwrap_or(d.idle_ms),
            deep_idle_ms: env("SV_RAM_DEEP_IDLE_MS").unwrap_or(d.deep_idle_ms),
            calm_ms: env("SV_RAM_CALM_MS").unwrap_or(d.calm_ms),
            ..d
        }
    }
}

fn round_up(x: u64, unit: u64) -> u64 {
    x.div_ceil(unit) * unit
}

impl Policy {
    pub fn low_water(&self, used: u64) -> u64 {
        self.low_water_floor.max(used / 4)
    }

    /// The geometric step: `clamp(used / 2, min_step, max_step)`.
    pub fn step(&self, used: u64) -> u64 {
        round_up((used / 2).clamp(self.min_step, self.max_step), self.unit)
    }

    /// A lease of `need` bytes did not fit: the region to ask for, or `None`
    /// when even the request itself is past the budget.
    pub fn on_demand(&self, need: u64, u: Usage) -> Option<u64> {
        let need = round_up(need, self.unit);
        if need > u.budget {
            return None;
        }
        let cap = u.budget / self.unit * self.unit;
        Some(need.max(self.step(u.used)).min(cap).max(need))
    }

    /// After a lease: a region to announce in the background, if free bytes
    /// fell below the low water mark and the budget allows one.
    pub fn ahead(&self, u: Usage) -> Option<u64> {
        if u.free >= self.low_water(u.used) {
            return None;
        }
        let want = self.step(u.used).min(u.budget / self.unit * self.unit);
        (want >= self.unit).then_some(want)
    }

    /// May a region of `len` bytes, empty for `idle_ms`, be retired now?
    pub fn may_retire(&self, len: u64, idle_ms: u64, since_grow_ms: u64, retiring: bool, u: Usage) -> bool {
        !retiring
            && since_grow_ms >= self.calm_ms
            && ((idle_ms >= self.idle_ms && u.free.saturating_sub(len) >= 2 * self.low_water(u.used)) || idle_ms >= self.deep_idle_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_geometric_between_the_bounds() {
        let p = Policy::default();
        assert_eq!(p.step(0), 64 * MIB);
        assert_eq!(p.step(300 * MIB), 150 * MIB);
        assert_eq!(p.step(10 * GIB), GIB);
        assert_eq!(p.step(129 * MIB), 66 * MIB);
    }

    #[test]
    fn on_demand_covers_the_request_and_respects_the_budget() {
        let p = Policy::default();
        let u = Usage { used: 300 * MIB, free: MIB, budget: 4 * GIB };
        assert_eq!(p.on_demand(5 * MIB, u), Some(150 * MIB));
        assert_eq!(p.on_demand(700 * MIB, u), Some(700 * MIB));
        // a budget smaller than the step still serves the request
        let small = Usage { budget: 20 * MIB, ..u };
        assert_eq!(p.on_demand(5 * MIB, small), Some(20 * MIB));
        assert_eq!(p.on_demand(21 * MIB, small), None);
        // a request is rounded to the unit
        assert_eq!(p.on_demand(GIB + 1, u), Some(GIB + 2 * MIB));
    }

    #[test]
    fn ahead_of_demand_only_below_low_water() {
        let p = Policy::default();
        assert_eq!(p.ahead(Usage { used: 100 * MIB, free: 40 * MIB, budget: GIB }), None);
        assert_eq!(p.ahead(Usage { used: 100 * MIB, free: 20 * MIB, budget: GIB }), Some(64 * MIB));
        assert_eq!(p.ahead(Usage { used: 100 * MIB, free: 20 * MIB, budget: MIB }), None);
    }

    #[test]
    fn retire_waits_for_idle_calm_and_hysteresis() {
        let p = Policy::default();
        let u = Usage { used: 100 * MIB, free: 200 * MIB, budget: GIB };
        assert!(p.may_retire(64 * MIB, 30_000, 10_000, false, u));
        assert!(!p.may_retire(64 * MIB, 29_999, 10_000, false, u));
        assert!(!p.may_retire(64 * MIB, 30_000, 9_999, false, u));
        assert!(!p.may_retire(64 * MIB, 30_000, 10_000, true, u));
        // without the region, free would fall below twice the low water mark
        assert!(!p.may_retire(160 * MIB, 30_000, 10_000, false, u));
        // ... until it has been idle for the deep-idle time: an idle pool
        // gives everything back.
        assert!(p.may_retire(160 * MIB, 300_000, 10_000, false, u));
    }
}
