// SPDX-License-Identifier: MIT OR Apache-2.0

//! Run independent observations at the same time.
//!
//! Each job runs exactly once. If a worker thread cannot start, the caller
//! runs that job after its own job. The results keep the order of the jobs.

use std::sync::{Mutex, PoisonError};
use std::thread::{Builder, Scope, ScopedJoinHandle};

/// A job that can run on a scoped worker thread.
pub(in crate::configure) type Job<'a, T> = Box<dyn FnOnce() -> T + Send + 'a>;

/// Run all jobs and return their results in job order.
///
/// The caller thread runs the first job. A result is `None` only when the
/// worker of that job panicked.
pub(in crate::configure) fn run_all<'a, T: Send + 'a>(jobs: Vec<Job<'a, T>>) -> Vec<Option<T>> {
    let slots: Vec<Mutex<Option<Job<'a, T>>>> =
        jobs.into_iter().map(|job| Mutex::new(Some(job))).collect();
    std::thread::scope(|scope| {
        let workers: Vec<_> = slots
            .iter()
            .skip(1)
            .map(|slot| start(scope, slot))
            .collect();
        let mut results = Vec::with_capacity(slots.len());
        if let Some(first) = slots.first() {
            results.push(take(first).map(|job| job()));
        }
        for (slot, worker) in slots.iter().skip(1).zip(workers) {
            let result = worker.and_then(|worker| worker.join().ok()).flatten();
            // A job that did not start is still in its slot.
            results.push(result.or_else(|| take(slot).map(|job| job())));
        }
        results
    })
}

/// Run `first` on a worker while the caller runs `second`.
pub(in crate::configure) fn join<'a, A: Send + 'a, B>(
    first: impl FnOnce() -> A + Send + 'a,
    second: impl FnOnce() -> B,
) -> (Option<A>, B) {
    let slot: Mutex<Option<Job<'a, A>>> = Mutex::new(Some(Box::new(first)));
    std::thread::scope(|scope| {
        let worker = start(scope, &slot);
        let second = second();
        let first = worker.and_then(|worker| worker.join().ok()).flatten();
        (first.or_else(|| take(&slot).map(|job| job())), second)
    })
}

fn start<'scope, 'env, 'a, T: Send + 'a>(
    scope: &'scope Scope<'scope, 'env>,
    slot: &'env Mutex<Option<Job<'a, T>>>,
) -> Option<ScopedJoinHandle<'scope, Option<T>>> {
    Builder::new()
        .spawn_scoped(scope, move || take(slot).map(|job| job()))
        .ok()
}

fn take<J>(slot: &Mutex<Option<J>>) -> Option<J> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Job, join, run_all};

    #[test]
    fn run_all_runs_each_job_once_and_keeps_job_order() {
        let calls = AtomicUsize::new(0);
        let jobs: Vec<Job<'_, usize>> = (0..5)
            .map(|index| -> Job<'_, usize> {
                let calls = &calls;
                Box::new(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    index * 10
                })
            })
            .collect();
        let results = run_all(jobs);
        assert_eq!(
            results,
            vec![Some(0), Some(10), Some(20), Some(30), Some(40)]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert!(run_all::<u8>(Vec::new()).is_empty());
    }

    #[test]
    fn join_returns_both_results() {
        let (first, second) = join(|| 1_u8, || 2_u8);
        assert_eq!((first, second), (Some(1), 2));
    }
}
