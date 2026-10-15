//! The GPU sessions' arena on an ivshmem device: how the BAR is cut into
//! session slots, and which slots are in use (waycompose-design.md §87).
//!
//! # Why slots, and why one process
//!
//! Until §87 each GPU session was its own `sv-gpu-server guest` process with
//! its own large-page window of guest RAM, which the VM host could reach only
//! by locating it in QEMU's memory backend — root's, on testhost. The arena
//! now lives in BAR2 of a second ivshmem device whose backing file the host
//! opens as the user, exactly as §83 did for the compositor. Two facts about
//! the device decide the shape:
//!
//! * **its driver gives the BAR to one handle**, so one process maps it, and
//!   every session that process serves is a slice of it;
//! * **its size is fixed when the VM starts** (or the device is hot-plugged),
//!   so the number of sessions that can be live at once is
//!   `region / (control + arena)`, and a session that ends must give its slot
//!   back for Firefox's next one — `glxtest` takes a session and exits before
//!   the browser asks for its own.
//!
//! The doorbell's port has the same one-opener rule, which is the same answer
//! from the other direction: one process, one port, every session's lanes.
//!
//! # Layout
//!
//! Slot `i` starts at byte `i × slot_len` of the region and is one session's
//! extent exactly as the guest-RAM path laid one out: the control region, then
//! the arena. Nothing else is in the region. The host learns the offset from
//! the session's `Hello` (`Locator::Ivshmem`) and checks the stamps there.

use sv_transport::control::Geometry;
use sv_transport::region::PAGE;

/// How a region is cut into session slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotPlan {
    pub region_len: u64,
    pub control_len: u64,
    pub arena_len: u64,
    /// `control_len + arena_len`.
    pub slot_len: u64,
    pub count: u32,
}

impl SlotPlan {
    /// As many slots of `geometry`'s shape as fit in `region_len`, capped at
    /// `max` when it is non-zero. A region too small for one is an error that
    /// says what it would take.
    pub fn new(region_len: u64, geometry: Geometry, max: u32) -> Result<SlotPlan, String> {
        let control_len = geometry.control_size;
        let arena_len = geometry.arena_size;
        if !control_len.is_multiple_of(PAGE as u64) || !arena_len.is_multiple_of(PAGE as u64) {
            return Err("the control region and the arena must be whole pages".into());
        }
        let slot_len = control_len + arena_len;
        let fit = region_len / slot_len;
        if fit == 0 {
            return Err(format!(
                "a {} MiB ivshmem region holds no session of {} MiB ({} MiB control + {} MiB arena)",
                region_len >> 20,
                slot_len >> 20,
                control_len >> 20,
                arena_len >> 20
            ));
        }
        let fit = u32::try_from(fit).unwrap_or(u32::MAX);
        let count = if max == 0 { fit } else { fit.min(max) };
        Ok(SlotPlan { region_len, control_len, arena_len, slot_len, count })
    }

    /// Byte offset of slot `i` in the region.
    pub fn offset(&self, i: u32) -> u64 {
        u64::from(i) * self.slot_len
    }

    pub fn control_pages(&self) -> u64 {
        self.control_len / PAGE as u64
    }

    pub fn arena_pages(&self) -> u64 {
        self.arena_len / PAGE as u64
    }
}

/// What a slot is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Free,
    Busy,
    /// The session ended with objects the driver still holds (`Objects::
    /// leftovers`). Its device and its import are kept rather than destroyed
    /// under them, so the slot is not reused while this process lives.
    Quarantined,
}

/// The slots of one region, shared by the threads that serve them.
pub struct Slots {
    pub plan: SlotPlan,
    states: std::sync::Mutex<Vec<SlotState>>,
}

impl Slots {
    pub fn new(plan: SlotPlan) -> Slots {
        Slots { plan, states: std::sync::Mutex::new(vec![SlotState::Free; plan.count as usize]) }
    }

    /// The lowest free slot, now busy; `None` when every slot is busy or
    /// quarantined.
    pub fn acquire(&self) -> Option<u32> {
        let mut s = self.states.lock().ok()?;
        let i = s.iter().position(|&x| x == SlotState::Free)?;
        s[i] = SlotState::Busy;
        Some(i as u32)
    }

    pub fn release(&self, i: u32) {
        self.set(i, SlotState::Free);
    }

    pub fn quarantine(&self, i: u32) {
        self.set(i, SlotState::Quarantined);
    }

    fn set(&self, i: u32, to: SlotState) {
        if let Ok(mut s) = self.states.lock()
            && let Some(x) = s.get_mut(i as usize)
        {
            *x = to;
        }
    }

    /// `(free, busy, quarantined)`, for the log line each session ends with.
    pub fn census(&self) -> (usize, usize, usize) {
        let s = self.states.lock().map(|s| s.clone()).unwrap_or_default();
        let n = |w| s.iter().filter(|&&x| x == w).count();
        (n(SlotState::Free), n(SlotState::Busy), n(SlotState::Quarantined))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// testhost's device (§87): 256 MiB, the live geometry at a 64 MiB
    /// arena, which is 16 + 64 MiB a session — three slots, at 0, 80 and
    /// 160 MiB, the last ending 16 MiB short of the region.
    #[test]
    fn testhosts_region_holds_three_sessions() {
        let p = SlotPlan::new(256 << 20, Geometry::for_arena(64 << 20), 0).unwrap();
        assert_eq!(p.count, 3);
        assert_eq!(p.slot_len, 80 << 20);
        assert_eq!([p.offset(0), p.offset(1), p.offset(2)], [0, 80 << 20, 160 << 20]);
        assert!(p.offset(2) + p.slot_len <= p.region_len);
        assert_eq!(p.control_pages(), 4096);
        assert_eq!(p.arena_pages(), 16384);
        assert_eq!(SlotPlan::new(256 << 20, Geometry::for_arena(64 << 20), 2).unwrap().count, 2);
        let e = SlotPlan::new(64 << 20, Geometry::for_arena(64 << 20), 0).unwrap_err();
        assert!(e.contains("holds no session"), "{e}");
    }

    /// A slot comes back when its session ends cleanly and does not when it
    /// is quarantined; the lowest free slot is always the one handed out.
    #[test]
    fn slots_are_reused_unless_quarantined() {
        let s = Slots::new(SlotPlan::new(256 << 20, Geometry::for_arena(64 << 20), 0).unwrap());
        assert_eq!((s.acquire(), s.acquire(), s.acquire(), s.acquire()), (Some(0), Some(1), Some(2), None));
        s.release(1);
        assert_eq!(s.acquire(), Some(1));
        s.quarantine(0);
        s.release(2);
        assert_eq!(s.census(), (1, 1, 1));
        assert_eq!(s.acquire(), Some(2));
        assert_eq!(s.acquire(), None);
    }
}
