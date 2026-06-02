// SPDX-License-Identifier: MIT
//! Process/thread initialization and the thread-local default heap
//! (ports the lifecycle core of `src/init.c`).
//!
//! v1 bootstraps lazily: process-wide free-list encoding keys are computed once
//! (from OS randomness), and each thread gets its own [`Heap`] in thread-local
//! storage on first use. The richer lifecycle — `pthread_key` thread-exit page
//! handoff, reentrancy guards for `#[global_allocator]` init-before-main — is
//! follow-up work.
//!
//! The thread-local default heap requires the `std` feature; `no_std` embedders
//! drive their own [`Heap`] instances directly.

use crate::prim::{DefaultPrim, Prim};
use crate::sync::OnceBox;

/// Process-wide free-list encoding keys (random, computed once).
pub fn process_keys() -> [usize; 2] {
    static KEYS: OnceBox<[usize; 2]> = OnceBox::new();
    *KEYS.get_or_init(|| {
        let mut buf = [0u8; 2 * core::mem::size_of::<usize>()];
        if DefaultPrim::random_buf(&mut buf) {
            let mut k = [0usize; 2];
            let w = core::mem::size_of::<usize>();
            let mut b0 = [0u8; core::mem::size_of::<usize>()];
            let mut b1 = [0u8; core::mem::size_of::<usize>()];
            b0.copy_from_slice(&buf[..w]);
            b1.copy_from_slice(&buf[w..2 * w]);
            k[0] = usize::from_ne_bytes(b0);
            k[1] = usize::from_ne_bytes(b1);
            k
        } else {
            // Deterministic fallback if the OS RNG is unavailable.
            [0x9e37_79b9_7f4a_7c15, 0xc2b2_ae3d_27d4_eb4f]
        }
    })
}

/// A unique, stable id for the calling thread (low 2 bits clear, non-zero) used
/// to stamp page ownership and route cross-thread frees.
#[cfg(feature = "std")]
pub fn current_tid() -> usize {
    // Plain process-global counter (not part of any modeled concurrency), so we
    // use core atomics directly — they are usable in `static` (and under loom).
    use core::sync::atomic::{AtomicUsize, Ordering};
    // Start at 2 so the first thread id is `2 << 2 == 8`, strictly greater than
    // `MI_THREADID_ABANDONED_MAPPED` (4): the abandoned-page state encoding uses
    // `owner_tid <= 4` to mean "abandoned", so a real owner tid must exceed it.
    static NEXT: AtomicUsize = AtomicUsize::new(2);
    std::thread_local! {
        static TID: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }
    TID.with(|t| {
        let v = t.get();
        if v != 0 {
            v
        } else {
            // shift left by 2 so the low bits stay free for page flags
            let id = NEXT.fetch_add(1, Ordering::Relaxed) << 2;
            t.set(id);
            id
        }
    })
}

#[cfg(feature = "std")]
mod tls {
    use super::{current_tid, process_keys};
    use crate::heap::Heap;
    use core::ptr::NonNull;

    // NB: this uses std `thread_local!` (its `.with()` carries a state guard)
    // rather than a raw `#[thread_local]` pointer cache like mimalloc-C's
    // `__thread mi_heap_t*`. Such a cache *was* implemented and benchmarked
    // (git tag `archive/a1a-thread-local-cache`) and **parked**: a pinned-machine
    // microbench A/B showed no win — the malloc route already matches C v3, and
    // modern `thread_local!` is cheap enough that TLS access is not the phase-1
    // bottleneck (that is full-page eviction + inlining). See `docs/perf-hotpath.md`
    // §C2. Don't re-add without evidence that TLS access is actually the cost.
    std::thread_local! {
        /// The calling thread's default heap.
        static DEFAULT_HEAP: Heap = Heap::new(process_keys(), current_tid());
    }

    /// Allocate `size` bytes from the calling thread's default heap.
    #[inline]
    pub fn malloc(size: usize) -> Option<NonNull<u8>> {
        DEFAULT_HEAP.with(|h| h.alloc(size))
    }

    /// Allocate `size` bytes aligned to `align` from the default heap.
    #[inline]
    pub fn malloc_aligned(size: usize, align: usize) -> Option<NonNull<u8>> {
        DEFAULT_HEAP.with(|h| h.alloc_aligned(size, align))
    }

    /// Allocate zeroed memory of `size` bytes.
    #[inline]
    pub fn zalloc(size: usize) -> Option<NonNull<u8>> {
        let p = DEFAULT_HEAP.with(|h| h.alloc(size))?;
        // SAFETY: `p` points to at least `size` writable bytes.
        unsafe {
            core::ptr::write_bytes(p.as_ptr(), 0, size);
        }
        Some(p)
    }

    /// Reclaim memory in the calling thread's default heap (`force` is more
    /// aggressive — see [`crate::heap::Heap::collect`]).
    ///
    /// Documented to run on a live thread (drives `mi_collect`/`init::collect`),
    /// so it uses `.with` — accessing the TLS during/after destruction would
    /// panic, which is the correct signal for that misuse.
    pub fn collect(force: bool) {
        DEFAULT_HEAP.with(|h| h.collect(force));
    }

    /// Force the calling thread's default heap to be initialized (no-op if it
    /// already is). Used by the lifecycle wrappers.
    ///
    /// Uses `try_with` so a late call (after the thread's TLS destructors have
    /// begun) is a safe no-op rather than a "TLS during destruction" panic.
    pub fn touch() {
        let _ = DEFAULT_HEAP.try_with(|_| {});
    }

    /// Like [`collect`], but for the lifecycle wrappers (`thread_done`,
    /// `process_done`): uses `try_with` so a late call after the thread's TLS
    /// destructors started is a safe no-op instead of a panic. Full page
    /// hand-off still happens via the `Heap` `Drop` at real thread exit.
    pub fn collect_lifecycle(force: bool) {
        let _ = DEFAULT_HEAP.try_with(|h| h.collect(force));
    }
}

#[cfg(feature = "std")]
pub use tls::{collect, malloc, malloc_aligned, zalloc};

// ---------------------------------------------------------------------------
// Lifecycle / deferred-free registration (ports `mi_register_deferred_free`
// and the thread/process lifecycle entry points). `std`-only: they drive the
// thread-local default heap.
// ---------------------------------------------------------------------------

#[cfg(feature = "std")]
mod lifecycle {
    use core::cell::Cell;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    /// C deferred-free callback: `(force, heartbeat, arg)`.
    pub type DeferredFreeFun = extern "C" fn(bool, u64, *mut c_void);

    static DEFERRED_FN: AtomicUsize = AtomicUsize::new(0); // fn ptr as usize (0 = none)
    static DEFERRED_ARG: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

    /// Per-thread deferred-free state: a `recurse` reentrancy flag (mirrors the C
    /// reference's `tld->recurse`, page.c:895) and a per-thread `heartbeat` tick.
    /// Per-thread, not process-global, matching mimalloc's per-`theap` heartbeat.
    struct DeferredState {
        recurse: Cell<bool>,
        heartbeat: Cell<u64>,
    }
    std::thread_local! {
        static STATE: DeferredState = const {
            DeferredState { recurse: Cell::new(false), heartbeat: Cell::new(0) }
        };
    }

    /// Register (or, with `None`, clear) the deferred-free callback.
    pub fn register_deferred_free(fun: Option<DeferredFreeFun>, arg: *mut c_void) {
        DEFERRED_ARG.store(arg, Ordering::Release);
        let addr = fun.map_or(0, |f| f as usize);
        DEFERRED_FN.store(addr, Ordering::Release);
    }

    /// Invoke the registered deferred-free callback (if any), bumping the
    /// per-thread heartbeat. Called from `collect` — our "heartbeat" point
    /// (mimalloc also fires it from the generic alloc slow path; we fire on
    /// collect, which is honest and sufficient).
    ///
    /// Bounded to recursion depth 1 (mirrors C's `tld->recurse`): if the callback
    /// re-enters collection, it is not fired again, so a callback that calls
    /// `mi_collect` cannot self-recurse into a stack overflow.
    pub fn run_deferred_free(force: bool) {
        let addr = DEFERRED_FN.load(Ordering::Acquire);
        if addr == 0 {
            return;
        }
        // Claim the reentrancy flag and take this thread's heartbeat tick. The
        // `try_with` makes a call during TLS teardown a safe no-op. A `None`
        // result means we are already inside a deferred-free callback (recursing)
        // or the TLS is gone — either way, skip.
        let hb = match STATE.try_with(|s| {
            if s.recurse.get() {
                return None;
            }
            s.recurse.set(true);
            let hb = s.heartbeat.get();
            s.heartbeat.set(hb.wrapping_add(1));
            Some(hb)
        }) {
            Ok(Some(hb)) => hb,
            _ => return,
        };
        // Clear the reentrancy flag even if the callback unwinds.
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                let _ = STATE.try_with(|s| s.recurse.set(false));
            }
        }
        let _clear = Clear;
        let arg = DEFERRED_ARG.load(Ordering::Acquire);
        // SAFETY: `addr` was produced from a valid `DeferredFreeFun` in
        // `register_deferred_free` (and is non-zero, checked above); transmute
        // back to call it. Re-registration is not expected to race with
        // collection (single-registration contract); `arg` is opaque and never
        // dereferenced here.
        let fun: DeferredFreeFun = unsafe { core::mem::transmute::<usize, DeferredFreeFun>(addr) };
        fun(force, hb, arg);
    }

    /// `mi_thread_init`: ensure the calling thread's default heap is
    /// initialized. Idempotent — safe to call repeatedly.
    pub fn thread_init() {
        super::tls::touch();
    }

    /// `mi_thread_done`: reclaim the calling thread's pending frees now. Full
    /// page hand-off (abandoning pages that still hold live blocks) happens
    /// automatically at real thread exit via the thread_local `Drop`; this just
    /// drains + retires empties early. Idempotent.
    pub fn thread_done() {
        super::tls::collect_lifecycle(true);
    }

    /// `mi_process_init`: force process-wide initialization (keys + this
    /// thread's heap). Idempotent (keys via `OnceBox`; heap via thread_local).
    pub fn process_init() {
        let _ = super::process_keys();
        thread_init();
    }

    /// `mi_process_done`: best-effort process cleanup. The OS reclaims all
    /// mappings at exit, so this only drains the calling thread; it does NOT
    /// tear down global state (other threads may still be running). Idempotent
    /// and safe to call more than once.
    pub fn process_done() {
        super::tls::collect_lifecycle(true);
    }
}

#[cfg(feature = "std")]
pub use lifecycle::{
    process_done, process_init, register_deferred_free, run_deferred_free, thread_done,
    thread_init, DeferredFreeFun,
};

/// Serializes tests that mutate the process-global deferred-free registry
/// (here and in `capi`), so concurrent test threads don't overwrite each
/// other's registration. Test-only; no effect on the shipped allocator.
#[cfg(all(test, feature = "std"))]
pub(crate) static DEFERRED_REG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Grow/shrink an allocation, preserving its contents.
///
/// # Safety
/// `ptr` must be a live allocation from this allocator.
#[cfg(feature = "std")]
pub unsafe fn realloc(
    ptr: core::ptr::NonNull<u8>,
    new_size: usize,
) -> Option<core::ptr::NonNull<u8>> {
    // Foreign pointer (allocated by the system allocator): realloc it with the
    // real system realloc rather than treating it as one of ours.
    #[cfg(all(feature = "override", feature = "std"))]
    if !crate::heap::is_in_heap_region(ptr.as_ptr()) {
        // SAFETY: not in our heap region ⇒ `ptr` is a live system allocation.
        let p =
            unsafe { crate::sysalloc::realloc(ptr.as_ptr() as *mut core::ffi::c_void, new_size) };
        return core::ptr::NonNull::new(p as *mut u8);
    }
    // SAFETY: ptr is a live allocation.
    let old = unsafe { crate::heap::usable_size(ptr) };
    if new_size <= old {
        return Some(ptr);
    }
    let np = malloc(new_size)?;
    // SAFETY: both regions are valid for `min(old, new_size)` bytes and disjoint.
    unsafe {
        core::ptr::copy_nonoverlapping(ptr.as_ptr(), np.as_ptr(), old.min(new_size));
        free(ptr);
    }
    Some(np)
}

/// Same as [`realloc`] but with an explicit alignment for the new block.
///
/// # Safety
/// `ptr` must be a live allocation from this allocator.
#[cfg(feature = "std")]
pub unsafe fn realloc_aligned(
    ptr: core::ptr::NonNull<u8>,
    new_size: usize,
    align: usize,
) -> Option<core::ptr::NonNull<u8>> {
    // Foreign pointer under override: realloc of a foreign pointer ignores the
    // alignment refinement and just system-reallocs (the system allocator's own
    // alignment guarantees apply); we cannot relocate a block we do not own.
    #[cfg(all(feature = "override", feature = "std"))]
    if !crate::heap::is_in_heap_region(ptr.as_ptr()) {
        let _ = align;
        // SAFETY: not in our heap region ⇒ `ptr` is a live system allocation.
        let p =
            unsafe { crate::sysalloc::realloc(ptr.as_ptr() as *mut core::ffi::c_void, new_size) };
        return core::ptr::NonNull::new(p as *mut u8);
    }
    // SAFETY: ptr is a live allocation.
    let old = unsafe { crate::heap::usable_size(ptr) };
    let np = malloc_aligned(new_size, align)?;
    // SAFETY: valid, disjoint regions.
    unsafe {
        core::ptr::copy_nonoverlapping(ptr.as_ptr(), np.as_ptr(), old.min(new_size));
        free(ptr);
    }
    Some(np)
}

/// Free a pointer obtained from [`malloc`]/[`zalloc`].
///
/// # Safety
/// `ptr` must be a live allocation from this allocator.
#[inline]
pub unsafe fn free(ptr: core::ptr::NonNull<u8>) {
    // SAFETY: forwarded contract.
    unsafe { crate::heap::free(ptr) }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn process_keys_stable() {
        let a = process_keys();
        let b = process_keys();
        assert_eq!(a, b, "keys must be stable across calls");
    }

    #[test]
    fn default_heap_malloc_free() {
        // SAFETY: pointers come from this allocator.
        unsafe {
            let p = malloc(123).unwrap();
            core::ptr::write_bytes(p.as_ptr(), 0x42, 123);
            assert_eq!(*p.as_ptr(), 0x42);
            free(p);

            let z = zalloc(64).unwrap();
            for i in 0..64 {
                assert_eq!(*z.as_ptr().add(i), 0, "zalloc must zero");
            }
            free(z);
        }
    }

    #[test]
    fn multithreaded_each_thread_own_heap() {
        // Each thread allocates and frees its own pointers; this exercises the
        // per-thread TLS heaps working concurrently (cross-thread free is
        // covered elsewhere).
        let handles: alloc::vec::Vec<_> = (0..8)
            .map(|t| {
                std::thread::spawn(move || {
                    // SAFETY: each thread frees only what it allocated.
                    unsafe {
                        let mut v = alloc::vec::Vec::new();
                        for i in 0..2000usize {
                            let p = malloc(8 + (i % 200)).unwrap();
                            core::ptr::write_bytes(p.as_ptr(), t as u8, 8);
                            v.push(p);
                        }
                        for p in v {
                            free(p);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn deferred_free_fires_on_collect() {
        use core::ffi::c_void;
        use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

        // Process-global counters; key the callback to its own `arg` sentinel so
        // collects driven by other (parallel) tests' threads never bump them.
        // The heartbeat is *per-thread* and only globally meaningful on this
        // test's own thread, so also gate updates to the registering thread:
        // a `collect` driven by a parallel test's thread would otherwise store
        // that thread's (unrelated, non-monotonic) heartbeat into `LAST_HB`.
        static FIRED: AtomicUsize = AtomicUsize::new(0);
        static LAST_HB: AtomicU64 = AtomicU64::new(0);
        static OWNER_TID: AtomicU64 = AtomicU64::new(0);
        static SENTINEL: u8 = 0;

        extern "C" fn cb(_force: bool, heartbeat: u64, arg: *mut c_void) {
            if arg == core::ptr::addr_of!(SENTINEL) as *mut c_void
                && super::current_tid() as u64 == OWNER_TID.load(Ordering::Relaxed)
            {
                FIRED.fetch_add(1, Ordering::Relaxed);
                LAST_HB.store(heartbeat, Ordering::Relaxed);
            }
        }

        let _guard = super::DEFERRED_REG_TEST_LOCK.lock().unwrap();
        OWNER_TID.store(super::current_tid() as u64, Ordering::Relaxed);
        let arg = core::ptr::addr_of!(SENTINEL) as *mut c_void;
        register_deferred_free(Some(cb), arg);
        let before = FIRED.load(Ordering::Relaxed);
        collect(true);
        let after = FIRED.load(Ordering::Relaxed);
        assert!(
            after > before,
            "deferred-free callback must fire on collect"
        );
        // Heartbeat advances monotonically (other threads may bump it too).
        let hb1 = LAST_HB.load(Ordering::Relaxed);
        collect(true);
        let hb2 = LAST_HB.load(Ordering::Relaxed);
        assert!(hb2 > hb1, "heartbeat must advance across collects");

        // After clearing, our keyed counter must not advance again.
        register_deferred_free(None, core::ptr::null_mut());
        let cleared = FIRED.load(Ordering::Relaxed);
        collect(true);
        assert_eq!(
            FIRED.load(Ordering::Relaxed),
            cleared,
            "callback must not fire after being cleared"
        );
    }

    #[test]
    fn deferred_free_reentrancy_bounded() {
        use core::ffi::c_void;
        use core::sync::atomic::{AtomicUsize, Ordering};

        // A callback that re-enters collection must NOT be fired again (the
        // recurse guard bounds it to depth 1), so this cannot stack-overflow.
        // The recurse guard is *per-thread*, so `DEPTH` is only meaningful on
        // this test's own thread; gate updates to the registering thread so a
        // `collect` driven by a parallel test's thread cannot inflate `DEPTH`.
        static DEPTH: AtomicUsize = AtomicUsize::new(0);
        static MAX_DEPTH: AtomicUsize = AtomicUsize::new(0);
        static OWNER_TID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        static SENTINEL: u8 = 0;

        extern "C" fn cb(_force: bool, _heartbeat: u64, arg: *mut c_void) {
            if arg != core::ptr::addr_of!(SENTINEL) as *mut c_void
                || super::current_tid() as u64 != OWNER_TID.load(Ordering::Relaxed)
            {
                return;
            }
            let d = DEPTH.fetch_add(1, Ordering::Relaxed) + 1;
            MAX_DEPTH.fetch_max(d, Ordering::Relaxed);
            // Re-enter collection from inside the callback: the guard must make
            // this a no-op (our callback must not fire a second time).
            collect(true);
            DEPTH.fetch_sub(1, Ordering::Relaxed);
        }

        let _guard = super::DEFERRED_REG_TEST_LOCK.lock().unwrap();
        OWNER_TID.store(super::current_tid() as u64, Ordering::Relaxed);
        let arg = core::ptr::addr_of!(SENTINEL) as *mut c_void;
        register_deferred_free(Some(cb), arg);
        collect(true); // must return (no unbounded self-recursion)
        register_deferred_free(None, core::ptr::null_mut());
        // Bounded to depth 1: the nested collect never re-fired the callback.
        // (`<= 1` rather than `== 1` to tolerate the global registry being
        // overwritten by a parallel test before our outer collect fires it.)
        assert!(
            MAX_DEPTH.load(Ordering::Relaxed) <= 1,
            "deferred-free callback must be bounded to recursion depth 1"
        );
    }

    #[test]
    fn lifecycle_idempotent() {
        // Repeated lifecycle calls are safe no-ops on a live thread.
        process_init();
        thread_init();
        thread_init();
        thread_done();
        process_done();
        process_done();
        // Allocator still usable after the lifecycle calls.
        // SAFETY: pointer comes from this allocator.
        unsafe {
            let p = malloc(64).unwrap();
            core::ptr::write_bytes(p.as_ptr(), 0x11, 64);
            free(p);
        }
    }

    extern crate alloc;
}
