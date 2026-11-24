//! Run one fuzz input with isolation: own thread, 8 MiB stack, panic catch, and a
//! wall-clock budget.

use std::panic::{UnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::time::Duration;

/// Stack size of each input thread. This is the same as the main thread of `rcmake`.
pub const INPUT_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Result of one fuzz input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The surface accepted the input.
    Accepted,
    /// The surface rejected the input with an error. This is not a failure.
    Rejected,
    /// The surface panicked (failure). The text is the panic message.
    Panicked(String),
    /// The input did not finish in the budget (failure).
    TimedOut,
    /// The harness could not start the input thread.
    HarnessError(String),
}

impl Outcome {
    /// `true` for a panic or a timeout.
    #[must_use]
    pub const fn is_failure(&self) -> bool {
        matches!(self, Self::Panicked(_) | Self::TimedOut)
    }
}

/// Run `input` on a new thread and classify the result.
///
/// `input` returns `true` when the surface accepted the input and `false` when the
/// surface rejected it with an error. After a timeout, the input thread keeps running
/// in the background until it ends. The harness process stops it at exit.
pub fn check_input<F>(budget: Duration, input: F) -> Outcome
where
    F: FnOnce() -> bool + Send + UnwindSafe + 'static,
{
    let (send, receive) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("rcmake-fuzz-input".into())
        .stack_size(INPUT_STACK_BYTES)
        .spawn(move || {
            let _ = send.send(catch_unwind(input));
        });
    if let Err(err) = spawned {
        return Outcome::HarnessError(err.to_string());
    }
    match receive.recv_timeout(budget) {
        Ok(Ok(true)) => Outcome::Accepted,
        Ok(Ok(false)) => Outcome::Rejected,
        Ok(Err(payload)) => Outcome::Panicked(panic_message(payload.as_ref())),
        Err(mpsc::RecvTimeoutError::Timeout) => Outcome::TimedOut,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Outcome::HarnessError("the input thread stopped without a result".into())
        }
    }
}

fn panic_message(payload: &(dyn core::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-text panic payload".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: Duration = Duration::from_secs(10);

    #[test]
    fn classifies_accepted_and_rejected_inputs() {
        assert_eq!(check_input(BUDGET, || true), Outcome::Accepted);
        assert_eq!(check_input(BUDGET, || false), Outcome::Rejected);
    }

    #[test]
    fn catches_a_panic() {
        let outcome = check_input(BUDGET, || panic!("boom"));
        assert_eq!(outcome, Outcome::Panicked("boom".into()));
        assert!(outcome.is_failure());
    }

    #[test]
    fn stops_waiting_at_the_budget() {
        let outcome = check_input(Duration::from_millis(20), || {
            std::thread::sleep(Duration::from_millis(500));
            true
        });
        assert_eq!(outcome, Outcome::TimedOut);
    }

    #[test]
    fn deep_recursion_fits_the_input_stack() {
        fn depth(n: u32) -> u32 {
            // The frame holds a 64-byte array, so the recursion uses real stack.
            let pad = std::hint::black_box([n; 16]);
            if n == 0 {
                0
            } else {
                1 + depth(n - 1) + pad[15] - n
            }
        }
        assert_eq!(
            check_input(BUDGET, || depth(10_000) == 10_000),
            Outcome::Accepted
        );
    }
}
