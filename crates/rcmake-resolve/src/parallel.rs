//! Bounded parallel chunk execution with deterministic output order.
//!
//! On `wasm32` targets, all helpers run serially without spawning threads.

/// Minimum number of targets before parallelizing per-target resolution or lowering.
pub(crate) const TARGET_PARALLEL_THRESHOLD: usize = 16;

/// Maximum worker threads for resolution and lowering (keeps memory use bounded).
pub(crate) const MAX_WORKERS: usize = 4;

/// Choose the worker count for `item_count` items, capped at `max_workers`.
///
/// Returns `1` when below `threshold` or on `wasm32`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn worker_count(item_count: usize, threshold: usize, max_workers: usize) -> usize {
    if item_count < threshold || max_workers <= 1 {
        return 1;
    }
    let available = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    available.min(max_workers).min(item_count).max(1)
}

/// Choose the worker count on `wasm32` (always `1`).
#[cfg(target_arch = "wasm32")]
pub(crate) fn worker_count(_item_count: usize, _threshold: usize, _max_workers: usize) -> usize {
    1
}

/// Process `items` in contiguous chunks across up to `workers` threads, returning
/// chunk outputs in exact input chunk order.
///
/// Falls back to running a chunk synchronously on the caller thread if thread
/// creation fails.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn map_chunks<T, R, F>(items: &[T], workers: usize, process: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&[T]) -> R + Sync,
{
    if items.is_empty() {
        return Vec::new();
    }
    if workers <= 1 || items.len() <= 1 {
        return vec![process(items)];
    }
    let effective = workers.min(items.len());
    let chunk_size = items.len().div_ceil(effective);
    let chunks: Vec<&[T]> = items.chunks(chunk_size).collect();
    let slots: Vec<std::sync::Mutex<Option<R>>> =
        std::iter::repeat_with(|| std::sync::Mutex::new(None))
            .take(chunks.len())
            .collect();
    let process_ref = &process;
    if let (Some((&first_chunk, rest_chunks)), Some((first_slot, rest_slots))) =
        (chunks.split_first(), slots.split_first())
    {
        std::thread::scope(|scope| {
            for (&chunk, slot) in rest_chunks.iter().zip(rest_slots.iter()) {
                if std::thread::Builder::new()
                    .spawn_scoped(scope, move || {
                        *slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(process_ref(chunk));
                    })
                    .is_err()
                {
                    *slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(process_ref(chunk));
                }
            }
            *first_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(process_ref(first_chunk));
        });
    }
    slots
        .into_iter()
        .filter_map(|slot| {
            slot.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
        .collect()
}

/// Process `items` serially on `wasm32`.
#[cfg(target_arch = "wasm32")]
pub(crate) fn map_chunks<T, R, F>(items: &[T], _workers: usize, process: F) -> Vec<R>
where
    F: Fn(&[T]) -> R,
{
    if items.is_empty() {
        return Vec::new();
    }
    vec![process(items)]
}
