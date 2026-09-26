//! Counting global allocator backing the "no per-frame/per-block heap allocation" rule (§21).
//!
//! The binary installs it once (`se-app` in debug builds; test binaries install their own):
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: se_alloc::Counting = se_alloc::Counting;
//! ```
//!
//! Real-time threads wrap the code that must not allocate in a [`Scope`] and check
//! [`Scope::allocs`] afterwards. Counting is per thread and only while a scope is open, so the
//! cost for every other allocation is one thread-local read. Calls into third-party code that
//! is allowed to allocate (e.g. the GPU driver API) can be excluded with [`Pause`].

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

/// `System` plus per-thread allocation counting while a [`Scope`] is open.
pub struct Counting;

static INSTALLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static FREES: Cell<u64> = const { Cell::new(0) };
}

#[inline]
fn note(counter: &'static std::thread::LocalKey<Cell<u64>>) {
    if !INSTALLED.load(Ordering::Relaxed) {
        INSTALLED.store(true, Ordering::Relaxed);
    }
    // `try_with`: allocations during thread teardown must never panic.
    let tracking = DEPTH.try_with(|d| d.get() > 0).unwrap_or(false);
    if tracking {
        let _ = counter.try_with(|c| c.set(c.get() + 1));
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(&ALLOCS);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(&ALLOCS);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(&ALLOCS);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(&FREES);
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// True once the counting allocator has served an allocation in this process (i.e. it is the
/// global allocator). Without it every count reads 0.
pub fn installed() -> bool {
    // Force at least one allocation through the global allocator.
    drop(std::hint::black_box(Box::new(0u8)));
    INSTALLED.load(Ordering::Relaxed)
}

/// Allocations counted on this thread so far (inside scopes only).
pub fn thread_allocs() -> u64 {
    ALLOCS.with(Cell::get)
}

/// Frees counted on this thread so far (inside scopes only).
pub fn thread_frees() -> u64 {
    FREES.with(Cell::get)
}

/// Counts allocations and frees on the current thread while alive. Scopes nest.
pub struct Scope {
    allocs: u64,
    frees: u64,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Scope {
    pub fn begin() -> Scope {
        DEPTH.with(|d| d.set(d.get() + 1));
        Scope { allocs: thread_allocs(), frees: thread_frees(), _not_send: std::marker::PhantomData }
    }

    /// Allocations (incl. reallocations) since `begin`.
    pub fn allocs(&self) -> u64 {
        thread_allocs() - self.allocs
    }

    /// Frees since `begin`.
    pub fn frees(&self) -> u64 {
        thread_frees() - self.frees
    }

    /// Restart the counts without closing the scope.
    pub fn reset(&mut self) {
        self.allocs = thread_allocs();
        self.frees = thread_frees();
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Suspends counting on this thread while alive (for calls into code that may allocate by
/// design, e.g. GPU API submission).
pub struct Pause {
    depth: u32,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Pause {
    pub fn new() -> Pause {
        Pause { depth: DEPTH.with(|d| d.replace(0)), _not_send: std::marker::PhantomData }
    }
}

impl Default for Pause {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(self.depth));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[global_allocator]
    static A: Counting = Counting;

    #[test]
    fn counts_only_inside_scopes_and_not_while_paused() {
        assert!(installed());
        let _outside = std::hint::black_box(Box::new([1u8; 16]));
        let s = Scope::begin();
        assert_eq!(s.allocs(), 0);
        let v: Vec<u32> = std::hint::black_box(Vec::with_capacity(8));
        assert_eq!(s.allocs(), 1);
        {
            let _p = Pause::new();
            let _w: Vec<u32> = std::hint::black_box(Vec::with_capacity(8));
        }
        assert_eq!(s.allocs(), 1, "paused allocations are not counted");
        drop(v);
        assert!(s.frees() >= 1);
        let mut nested = Scope::begin();
        let _x = std::hint::black_box(Box::new(5u64));
        assert_eq!(nested.allocs(), 1);
        nested.reset();
        assert_eq!(nested.allocs(), 0);
        drop(nested);
        assert_eq!(s.allocs(), 2, "nested scope allocations count for the outer scope too");
    }

    #[test]
    fn other_threads_are_not_counted() {
        let s = Scope::begin();
        std::thread::spawn(|| drop(std::hint::black_box(vec![0u8; 64]))).join().unwrap();
        // spawning itself allocates on this thread; the child's vec must not be counted here
        let spawned = s.allocs();
        let t = std::thread::spawn(|| {
            let s = Scope::begin();
            drop(std::hint::black_box(vec![0u8; 64]));
            s.allocs()
        });
        assert_eq!(t.join().unwrap(), 1);
        assert!(s.allocs() >= spawned);
    }
}
