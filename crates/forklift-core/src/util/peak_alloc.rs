//! A counting global allocator for the fuzz tests: it measures how much memory a decoder holds at
//! its peak while it reads one input, so a decoder that sizes an allocation from a number in the
//! input, or copies its input more times than it needs, fails a stated bound instead of passing
//! unnoticed on small fixtures.
//!
//! It is installed as the `#[global_allocator]` of two test binaries: core's unit tests (from
//! `lib.rs`, under `cfg(test)`) and `tests/fuzz_formats.rs`, which includes this file by path.
//! Neither shipped binary uses it.
//!
//! Counting is per thread. Test threads run side by side in one process, so a process-wide count
//! would mix one test's decoder with every other test's allocations; a per-thread count sees only
//! the thread that calls [`measure`]. That is a precondition on every bound measured with it: the
//! decoder must do its work on the calling thread. Allocations a decoder makes on a thread it
//! hands work to are not seen.
//!
//! Two more things it does not see, which a bound measured with it must not claim:
//! * Memory C code allocates with its own `malloc`. zstd's decoder context and window are such
//!   memory, so a bound on a zstd-reading decoder covers only its Rust side; the window is
//!   bounded separately, by the decoders' window limit and the tests that refuse a frame past it.
//! * Memory reused after freeing memory that existed before the measurement. The count is of
//!   bytes live since the measurement began, so a decoder that drops `n` bytes it was handed can
//!   then allocate `n` uncounted. Measure a decoder that borrows its input, or keep the input
//!   alive outside the measurement.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// The allocator: [`System`], plus a per-thread tally while a [`measure`] is running.
pub struct PeakAlloc;

thread_local! {
    /// Whether this thread is inside a [`measure`].
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// Bytes allocated on this thread minus bytes freed on it, since the outermost [`measure`]
    /// began. It can go below zero: a decoder may free memory allocated before it was called.
    static LIVE: Cell<isize> = const { Cell::new(0) };
    /// The highest `LIVE` reached since the innermost [`measure`] began.
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

/// Add `delta` bytes to this thread's tally, if it is being measured. `try_with` because an
/// allocation can happen while the thread's locals are being torn down, after which they are
/// gone; such an allocation is outside any measurement.
fn record(delta: isize) {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let live = LIVE.with(|live| {
                live.set(live.get() + delta);
                live.get()
            });
            PEAK.with(|peak| peak.set(peak.get().max(live)));
        }
    });
}

unsafe impl GlobalAlloc for PeakAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record(layout.size() as isize);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record(layout.size() as isize);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        record(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            record(new_size as isize - layout.size() as isize);
        }
        moved
    }
}

/// Restores the enclosing measurement's state when a [`measure`] ends, by return or by panic.
struct Restore {
    was_armed: bool,
    outer_peak: isize,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let inner_peak = PEAK.with(Cell::get);
        PEAK.with(|peak| peak.set(self.outer_peak.max(inner_peak)));
        ARMED.with(|armed| armed.set(self.was_armed));
    }
}

/// Run `f` and return its value with the most memory this thread held at once while it ran,
/// above what it held when `f` began, in bytes. The value `f` returns is counted: a decoder's
/// output is memory it allocated.
///
/// Panics if this binary's global allocator is not [`PeakAlloc`]: every measurement would read
/// zero, and every bound would pass without measuring anything.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, usize) {
    assert_installed();
    let was_armed = ARMED.with(|armed| armed.replace(true));
    if !was_armed {
        LIVE.with(|live| live.set(0));
    }
    let start = LIVE.with(Cell::get);
    let restore = Restore { was_armed, outer_peak: PEAK.with(|peak| peak.replace(start)) };
    let value = f();
    let peak = PEAK.with(Cell::get) - start;
    drop(restore);
    (value, peak.max(0) as usize)
}

/// Whether [`assert_installed`] has already seen this allocator count an allocation.
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Check, by allocating a known size inside a measurement, that this allocator is the one in use.
/// Once it has passed, it is not checked again in this process.
fn assert_installed() {
    if INSTALLED.load(std::sync::atomic::Ordering::Relaxed) || ARMED.with(Cell::get) {
        return;
    }
    ARMED.with(|armed| armed.set(true));
    LIVE.with(|live| live.set(0));
    PEAK.with(|peak| peak.set(0));
    let probe = std::hint::black_box(vec![0u8; 4096]);
    let seen = PEAK.with(Cell::get);
    drop(probe);
    ARMED.with(|armed| armed.set(false));
    assert!(seen >= 4096,
        "the counting allocator is not this binary's global allocator: a 4096-byte allocation was counted as {seen}");
    INSTALLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Assert that `peak`, measured while a decoder read `input_len` bytes, is within `factor` times
/// the input plus `overhead` bytes. `label` and `case` name the decoder and the input in the
/// failure.
pub fn assert_within(label: &str, case: &str, input_len: usize, peak: usize, factor: usize, overhead: usize) {
    let bound = input_len.saturating_mul(factor).saturating_add(overhead);
    assert!(peak <= bound,
        "{label} held {peak} bytes at its peak reading {input_len} bytes ({case}), above its bound of {factor} x input + {overhead} = {bound}");
}
