//! Native stack headroom guard for deep CMake recursion.
//!
//! The command depth counter does not measure native stack use. Frame sizes change with
//! the build profile, the compiler, and the recursion path. This guard compares the
//! address of a local variable with the address at the start of a budget window. It
//! compares integer addresses only and uses only safe Rust. Rust does not promise an
//! exact frame layout, so the measurement is an estimate.
//!
//! The window belongs to the thread, so all interpreters on the thread share one budget.
//! Windows nest only lexically through [`with_budget`]. If no window is set, the guard
//! is off.
//!
//! On `wasm32` targets a stack address does not measure the call stack, so the guard is
//! always off there: [`with_budget`] only runs its closure. WebAssembly hosts limit
//! recursion with `Limits::max_recursion_ceiling` instead.

#[cfg(not(target_arch = "wasm32"))]
use std::cell::Cell;

use rcmake_base::BString;

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug)]
struct Window {
    /// Stack address at the start of the window.
    base: usize,
    /// Bytes of stack that the window permits below `base`.
    budget: usize,
    /// Largest stack use seen in the window.
    peak: usize,
}

#[cfg(not(target_arch = "wasm32"))]
thread_local! {
    static WINDOW: Cell<Option<Window>> = const { Cell::new(None) };
}

#[cfg(not(target_arch = "wasm32"))]
/// Return the address of a local variable in a new stack frame.
#[inline(never)]
fn stack_address() -> usize {
    let anchor = 0_u8;
    core::ptr::from_ref(core::hint::black_box(&anchor)).addr()
}

#[cfg(not(target_arch = "wasm32"))]
/// Bytes of stack used in `window` at this point.
fn used(window: &Window) -> usize {
    window.base.abs_diff(stack_address())
}

#[cfg(not(target_arch = "wasm32"))]
/// Run `f` with a native stack budget of `budget` bytes for every interpreter on this
/// thread.
///
/// Inside an outer budget, the new budget is never larger than what remains of the outer
/// budget. The outer budget comes back when `f` returns or unwinds, so a tighter inner
/// budget always ends before its outer budget.
pub fn with_budget<R>(budget: usize, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Window>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = WINDOW.try_with(|cell| cell.set(self.0));
        }
    }

    let outer = WINDOW.try_with(Cell::get).ok().flatten();
    let budget = outer.map_or(budget, |outer| {
        budget.min(outer.budget.saturating_sub(used(&outer)))
    });
    let window = Window {
        base: stack_address(),
        budget,
        peak: 0,
    };
    let _restore = Restore(outer);
    let _ = WINDOW.try_with(|cell| cell.set(Some(window)));
    f()
}

#[cfg(not(target_arch = "wasm32"))]
/// Return `true` if the current thread has a stack budget.
#[must_use]
pub fn is_active() -> bool {
    WINDOW
        .try_with(|cell| cell.get().is_some())
        .unwrap_or(false)
}

#[cfg(not(target_arch = "wasm32"))]
/// Return `true` if this thread has used more native stack than its budget permits.
pub(crate) fn exhausted() -> bool {
    WINDOW
        .try_with(|cell| {
            let Some(mut window) = cell.get() else {
                return false;
            };
            let used = used(&window);
            if used > window.peak {
                window.peak = used;
                cell.set(Some(window));
            }
            used > window.budget
        })
        .unwrap_or(false)
}

#[cfg(not(target_arch = "wasm32"))]
/// Return the largest stack use seen in the current window of this thread.
///
/// Return `None` if no window is set. Measurement tests use this value.
#[must_use]
pub fn peak_bytes() -> Option<usize> {
    WINDOW
        .try_with(|cell| cell.get().map(|window| window.peak))
        .ok()
        .flatten()
}

/// Run `f`. On `wasm32` targets the stack guard is off.
#[cfg(target_arch = "wasm32")]
pub fn with_budget<R>(_budget: usize, f: impl FnOnce() -> R) -> R {
    f()
}

/// Return `false`. On `wasm32` targets the stack guard is off.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn is_active() -> bool {
    false
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn exhausted() -> bool {
    false
}

/// Return `None`. On `wasm32` targets the stack guard is off.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn peak_bytes() -> Option<usize> {
    None
}

/// Return the error message if a command call at `depth` must stop.
///
/// The configured `limit` comes first, so its message does not change. The stack check
/// runs only below that limit.
pub(crate) fn recursion_error(depth: u32, limit: u32) -> Option<BString> {
    if depth > limit {
        Some(format!("Maximum recursion depth of {limit} exceeded").into())
    } else if exhausted() {
        Some(
            format!("Maximum recursion depth exceeded: the native stack is full at depth {depth}.")
                .into(),
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recurse with 512 bytes in each frame. Return the levels left when the guard stops.
    #[inline(never)]
    fn descend(levels: usize) -> usize {
        let pad = core::hint::black_box([0_u8; 512]);
        if exhausted() {
            return levels;
        }
        if levels == 0 {
            return 0;
        }
        descend(levels - 1) + usize::from(pad[0])
    }

    #[test]
    fn guard_is_off_without_a_budget() {
        assert!(!is_active());
        assert!(!exhausted());
        assert_eq!(peak_bytes(), None);
        assert_eq!(recursion_error(5, 10), None);
    }

    #[test]
    fn deep_recursion_stops_at_the_budget_before_the_stack_overflows() {
        // 100,000 levels need about 50 MiB, much more than the 2 MiB test thread stack.
        let left = with_budget(64 * 1024, || {
            assert!(is_active());
            let left = descend(100_000);
            assert!(peak_bytes().is_some_and(|peak| peak > 64 * 1024));
            left
        });
        assert!(left > 0, "the guard must stop the recursion before its end");
        assert!(!is_active());
        assert_eq!(peak_bytes(), None);
    }

    #[test]
    fn inner_budget_cannot_exceed_the_outer_budget() {
        with_budget(64 * 1024, || {
            with_budget(usize::MAX, || assert!(descend(100_000) > 0));
            assert!(is_active());
            assert!(
                !exhausted(),
                "the outer budget comes back after the inner one ends"
            );
        });
    }

    #[test]
    fn tighter_inner_budget_stays_in_force_until_it_ends() {
        with_budget(usize::MAX / 2, || {
            with_budget(64 * 1024, || {
                // The outer budget is large, so only the inner budget can stop this.
                assert!(descend(100_000) > 0);
            });
        });
    }

    #[test]
    fn unwinding_restores_the_previous_budget() {
        let result = std::panic::catch_unwind(|| {
            with_budget(1024, || panic!("unwind through the budget"));
        });
        assert!(result.is_err());
        assert!(!is_active());
    }

    #[test]
    fn depth_limit_keeps_its_message() {
        assert_eq!(
            recursion_error(1001, 1000),
            Some("Maximum recursion depth of 1000 exceeded".into())
        );
    }
}
