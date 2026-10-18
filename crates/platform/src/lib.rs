//! OS calls that no pure-Rust binding crate covers.
//!
//! This is the only crate in the workspace allowed to contain `unsafe`
//! (plan D01). Every unsafe block must carry a `// SAFETY:` comment and be
//! reviewed. winit, wgpu, rfd and arboard cover every OS interaction
//! needed so far; what lives here is the counting allocator, whose trait
//! is `unsafe` to implement.

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting the heap bytes live and the peak, for
/// the timing and memory logs (`heap_live`, `heap_peak`). Installed as
/// the global allocator by the shell and by the test binaries that report
/// memory; a binary without it reads zeros.
#[derive(Debug, Default, Clone, Copy)]
pub struct CountingAlloc;

fn added(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn removed(size: usize) {
    LIVE.fetch_sub(size, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged, so `System`'s guarantees are ours; the counters only read
// the sizes and never touch the memory.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract (non-zero size),
        // which is `System.alloc`'s.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            added(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            added(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller passes a block this allocator (hence
        // `System`) gave out, with its layout.
        unsafe { System.dealloc(ptr, layout) };
        removed(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller passes a block `System` gave out with its
        // layout and a valid new size, as `System.realloc` requires.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            removed(layout.size());
            added(new_size);
        }
        p
    }
}

/// Heap bytes live now (zero without `CountingAlloc` installed).
pub fn heap_live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// The most heap bytes live at once since the start or the last
/// `reset_heap_peak`.
pub fn heap_peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Start a new peak from the bytes live now.
pub fn reset_heap_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// Bytes as mebibytes, for logs.
pub fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}
