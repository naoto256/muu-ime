//! Test-only observation of allocation requests made by one calling thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub largest_request: usize,
    pub total_requested: usize,
    pub request_count: usize,
}

#[derive(Clone, Copy)]
struct State {
    active: bool,
    usage: Usage,
}

impl State {
    const OFF: Self = Self {
        active: false,
        usage: Usage {
            largest_request: 0,
            total_requested: 0,
            request_count: 0,
        },
    };
}

thread_local! {
    // A constant Cell needs no allocation or destructor. Recording must not
    // recurse through the allocator or panic during thread-local teardown.
    static STATE: Cell<State> = const { Cell::new(State::OFF) };
}

fn record(size: usize) {
    let _ = STATE.try_with(|cell| {
        let mut state = cell.get();
        if state.active {
            state.usage.largest_request = state.usage.largest_request.max(size);
            state.usage.total_requested = state.usage.total_requested.saturating_add(size);
            state.usage.request_count = state.usage.request_count.saturating_add(1);
            cell.set(state);
        }
    });
}

struct ObservedSystem;

// SAFETY: pointer/layout operations are delegated unchanged to System.
// The observer only updates non-allocating thread-local integer counters.
unsafe impl GlobalAlloc for ObservedSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        unsafe { System.realloc(pointer, layout, new_size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}

// Why this does not disturb the other tests:
// - It exists only in this crate's unit-test binary. Both lib.rs files
//   include it under #[cfg(test)], so the product, integration tests and any
//   crate that links this one (muu-session links muu-lm) build without it.
// - Each binary has exactly one global allocator, so the two never meet.
// - Every call goes unchanged to System, which is already Rust's default
//   global allocator. Addresses, alignment and failure behave as before.
// - Outside `measure` the only cost is one thread-local read.
// - Counters are per thread, so tests running in parallel on other threads
//   are neither counted nor affected.
#[global_allocator]
static ALLOCATOR: ObservedSystem = ObservedSystem;

struct Reset;

impl Drop for Reset {
    fn drop(&mut self) {
        let _ = STATE.try_with(|cell| cell.set(State::OFF));
    }
}

// Counts only what `operation` requests on the calling thread, from the
// moment it starts until it returns or unwinds; `Reset` turns counting off
// on both paths. Fixture building, the assert and its message all run
// outside. A thread spawned by `operation` would go uncounted, but the
// parsers measured here do not spawn any. Counts are requests (realloc adds
// its new size), not live bytes.
pub(crate) fn measure<T>(operation: impl FnOnce() -> T) -> (T, Usage) {
    STATE.with(|cell| {
        assert!(!cell.get().active, "allocation probes must not nest");
        cell.set(State {
            active: true,
            ..State::OFF
        });
    });
    let reset = Reset;
    let value = operation();
    let usage = STATE.with(|cell| cell.get().usage);
    drop(reset);
    (value, usage)
}

// Budget basis: measured before the limits were added, over every probed
// call (GGUF 699 calls, Postal ZIP 659), the largest values were:
//
//   GGUF:       one request 256 B, one call's total 330 B, at most 4 requests
//   Postal ZIP: one request  32 B, one call's total  32 B, at most 1 request
//
// So 1 MiB per request leaves 4096x headroom for GGUF and 32768x for ZIP,
// and 4 MiB per call leaves about 12700x and 131072x. Real growth in the
// preflight stays far below the limits. In the review's fault injection, a
// reservation sized by a declared length broke them at once: 64,000,004 B
// for an unchecked GGUF count, 512 MiB for an unchecked ZIP size. The input must be under 1 KiB because
// the budget was set for small malformed fixtures only, not for valid
// models or the inflate path.
pub(crate) fn for_small_invalid<T>(input: &[u8], operation: impl FnOnce() -> T) -> T {
    assert!(
        input.len() < 1024,
        "small-invalid probe input must be under 1 KiB"
    );
    let (value, usage) = measure(operation);
    // These small malformed fixtures reject during container preflight. Keep a
    // generous fixed budget without letting declared lengths reserve a model
    // or inflated archive before that rejection. Fixture/assert allocations
    // are deliberately outside the measured parser call.
    assert!(
        usage.largest_request <= 1024 * 1024,
        "parser exceeded the 1 MiB single-request budget (input {} bytes): {usage:?}",
        input.len()
    );
    assert!(
        usage.total_requested <= 4 * 1024 * 1024,
        "parser exceeded the 4 MiB cumulative budget (input {} bytes): {usage:?}",
        input.len()
    );
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;
    use std::sync::{Arc, Barrier};

    fn allocate(bytes: usize) {
        let layout = Layout::from_size_align(black_box(bytes), 8).unwrap();
        // SAFETY: nonzero valid layout, checked allocation, paired deallocation.
        unsafe {
            let pointer = std::alloc::alloc(layout);
            assert!(!pointer.is_null());
            black_box(pointer);
            std::alloc::dealloc(pointer, layout);
        }
    }

    // The probe's own tests, run in each binary that includes it. This one:
    // requests between two measurements (8 MiB here) are not counted, and
    // alloc_zeroed and realloc are counted with their data intact.
    #[test]
    fn requests_outside_the_measurement_do_not_carry_into_the_next_one() {
        let (_, first) = measure(|| allocate(128));
        assert_eq!(
            first,
            Usage {
                largest_request: 128,
                total_requested: 128,
                request_count: 1
            }
        );
        allocate(8 * 1024 * 1024);
        let (_, next) = measure(|| allocate(64));
        assert_eq!(
            next,
            Usage {
                largest_request: 64,
                total_requested: 64,
                request_count: 1
            }
        );
        let (_, resized) = measure(|| {
            let initial = Layout::from_size_align(32, 8).unwrap();
            // SAFETY: nonzero layouts and a single matching deallocation;
            // realloc's successful result retains the initial live bytes.
            unsafe {
                let pointer = std::alloc::alloc_zeroed(initial);
                assert!(!pointer.is_null());
                assert_eq!(std::slice::from_raw_parts(pointer, 32), &[0; 32]);
                std::slice::from_raw_parts_mut(pointer, 32).fill(0xab);
                let pointer = std::alloc::realloc(pointer, initial, 96);
                assert!(!pointer.is_null());
                assert_eq!(std::slice::from_raw_parts(pointer, 32), &[0xab; 32]);
                std::alloc::dealloc(pointer, Layout::from_size_align(96, 8).unwrap());
            }
        });
        assert_eq!(
            resized,
            Usage {
                largest_request: 96,
                total_requested: 128,
                request_count: 2
            }
        );
    }

    // The barriers hold both measurements open at once. The worker's 8 MiB
    // must not show up in the caller's measurement.
    #[test]
    fn concurrent_threads_keep_independent_measurements() {
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            measure(|| {
                worker_barrier.wait();
                allocate(8 * 1024 * 1024);
                worker_barrier.wait();
            })
            .1
        });
        let (_, caller) = measure(|| {
            barrier.wait();
            allocate(1024);
            barrier.wait();
        });
        let other = worker.join().unwrap();
        assert_eq!(caller.largest_request, 1024);
        assert!(caller.total_requested < 64 * 1024);
        assert_eq!(other.largest_request, 8 * 1024 * 1024);
    }

    // A panicking operation must not leave counting on. If it did, the next
    // `measure` would fail its no-nesting assert.
    #[test]
    fn unwinding_clears_the_active_measurement() {
        assert!(
            std::panic::catch_unwind(|| measure::<()>(|| panic!("probe reset fixture"))).is_err()
        );
        let (_, next) = measure(|| allocate(64));
        assert_eq!(next.largest_request, 64);
        assert_eq!(next.total_requested, 64);
    }
}
