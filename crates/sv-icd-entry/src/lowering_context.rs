//! Lengths an enclosing struct holds for an array several levels below it.
//!
//! `sv_codegen::exceptions::LEN_FROM_CONTEXT` is the list and says why: H.265's
//! `StdVideoH265HrdParameters::pSubLayerHrdParametersNal` is
//! `len="*_max_sub_layers_minus1 + 1"`, and the count is a member of whichever
//! VPS or SPS the HRD parameters belong to. The generated lowering of that VPS
//! or SPS publishes the count here for exactly as long as its own members are
//! being lowered, and the HRD array reads it back.
//!
//! A stack per thread, because lowering is plain synchronous recursion on the
//! application's calling thread and two threads lowering at once must not see
//! each other's counts. An array that reaches for a key nobody above it
//! published is refused with `Shape` — the bound is application memory, and
//! the one thing not to do with an unknown bound is guess it.

use std::cell::RefCell;

thread_local! {
    static STACK: RefCell<Vec<(&'static str, usize)>> = const { RefCell::new(Vec::new()) };
}

/// Pops on drop, so a lowering that returns early with `?` cannot leave its
/// count behind for an unrelated call on the same thread.
struct Pop;

impl Drop for Pop {
    fn drop(&mut self) {
        STACK.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// Run `f` with `key` bound to `value`.
pub fn with<T>(key: &'static str, value: usize, f: impl FnOnce() -> T) -> T {
    STACK.with(|s| s.borrow_mut().push((key, value)));
    let _pop = Pop;
    f()
}

/// The innermost value bound to `key`.
pub fn get(key: &'static str) -> crate::LowerResult<usize> {
    STACK
        .with(|s| s.borrow().iter().rev().find(|(k, _)| *k == key).map(|(_, v)| *v))
        .ok_or(crate::LowerError::Shape(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_innermost_binding_wins_and_unwinds() {
        assert!(get("k").is_err());
        with("k", 3, || {
            assert_eq!(get("k").ok(), Some(3));
            with("k", 5, || assert_eq!(get("k").ok(), Some(5)));
            assert_eq!(get("k").ok(), Some(3));
        });
        assert!(get("k").is_err());
    }

    #[test]
    fn an_early_return_does_not_leave_its_binding_behind() {
        let r: Result<(), ()> = with("k", 1, || Err(()));
        assert!(r.is_err());
        assert!(get("k").is_err());
    }
}
