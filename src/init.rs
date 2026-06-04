// SPDX-License-Identifier: MIT
//! Process/thread initialization and the thread-local default heap (ports the
//! lifecycle core of `src/init.c`). Process-wide encoding keys are computed once
//! and each thread gets its own [`Heap`] in TLS. `std`-only; `no_std` embedders
//! drive [`Heap`] instances directly.

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
    use core::sync::atomic::{AtomicUsize, Ordering};
    // Start at 2 so the first id is `2 << 2 == 8`, above the abandoned-state
    // sentinels (`owner_tid <= MI_THREADID_ABANDONED_MAPPED == 4`).
    static NEXT: AtomicUsize = AtomicUsize::new(2);
    std::thread_local! {
        static TID: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }
    TID.with(|t| {
        let v = t.get();
        if v != 0 {
            v
        } else {
            // << 2 leaves the low bits free for page flags
            let id = NEXT.fetch_add(1, Ordering::Relaxed) << 2;
            t.set(id);
            id
        }
    })
}

#[cfg(feature = "std")]
mod tls {
    use super::current_tid;
    use crate::heap::{default_heap, ThreadHeap};
    use core::ptr::NonNull;

    // Rust: the whole `ThreadHeap` (mi_theap_t) lives inline in a `thread_local!`
    // (vs C's `__thread mi_theap_t*`); an out-of-line pointer cache showed no win.
    // Every thread's default theap belongs to the one shared `default_heap()`
    // (and is reached via TLS, so it is not added to that heap's theaps list).
    std::thread_local! {
        /// The calling thread's default thread-local heap.
        static DEFAULT_THEAP: ThreadHeap = ThreadHeap::new(default_heap(), current_tid());
    }

    /// Allocate `size` bytes from the calling thread's default theap.
    #[inline]
    pub fn malloc(size: usize) -> Option<NonNull<u8>> {
        DEFAULT_THEAP.with(|th| th.alloc(size))
    }

    /// Allocate `size` bytes aligned to `align` from the default theap.
    #[inline]
    pub fn malloc_aligned(size: usize, align: usize) -> Option<NonNull<u8>> {
        DEFAULT_THEAP.with(|th| th.alloc_aligned(size, align))
    }

    /// Allocate zeroed memory of `size` bytes.
    #[inline]
    pub fn zalloc(size: usize) -> Option<NonNull<u8>> {
        DEFAULT_THEAP.with(|th| th.alloc_zeroed(size))
    }

    /// Allocate `size` zeroed bytes aligned to `align` from the default theap.
    #[inline]
    pub fn zalloc_aligned(size: usize, align: usize) -> Option<NonNull<u8>> {
        DEFAULT_THEAP.with(|th| th.alloc_zeroed_aligned(size, align))
    }

    /// Reclaim memory in the calling thread's default theap (see
    /// [`crate::heap::ThreadHeap::collect`]). Uses `.with` — must run on a live thread.
    pub fn collect(force: bool) {
        DEFAULT_THEAP.with(|th| th.collect(force));
    }

    /// Force-initialize the calling thread's default theap (no-op if already).
    /// `try_with` makes a call during TLS teardown a safe no-op.
    pub fn touch() {
        let _ = DEFAULT_THEAP.try_with(|_| {});
    }

    /// Like [`collect`] but for the lifecycle wrappers: `try_with` makes a late
    /// call (TLS teardown) a safe no-op. Page hand-off still happens via `Drop`.
    pub fn collect_lifecycle(force: bool) {
        let _ = DEFAULT_THEAP.try_with(|th| th.collect(force));
    }
}

#[cfg(feature = "std")]
pub use tls::{collect, malloc, malloc_aligned, zalloc, zalloc_aligned};

// Lifecycle / deferred-free registration (ports `mi_register_deferred_free` and
// the thread/process lifecycle entry points). `std`-only.

#[cfg(feature = "std")]
mod lifecycle {
    use core::cell::Cell;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    /// C deferred-free callback: `(force, heartbeat, arg)`.
    pub type DeferredFreeFun = extern "C" fn(bool, u64, *mut c_void);

    static DEFERRED_FN: AtomicUsize = AtomicUsize::new(0); // fn ptr as usize (0 = none)
    static DEFERRED_ARG: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

    /// Per-thread deferred-free state: a `recurse` reentrancy flag (ports
    /// `tld->recurse`, page.c:895) and a per-thread `heartbeat` tick.
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
    /// per-thread heartbeat. Bounded to recursion depth 1 (ports `tld->recurse`)
    /// so a callback that re-enters collection cannot self-recurse.
    pub fn run_deferred_free(force: bool) {
        let addr = DEFERRED_FN.load(Ordering::Acquire);
        if addr == 0 {
            return;
        }
        // Claim the reentrancy flag and take a heartbeat tick; `None` means we are
        // recursing or the TLS is gone — skip.
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
        // SAFETY: `addr` is non-zero (checked above) and was stored from a valid
        // `DeferredFreeFun` by `register_deferred_free`; `arg` is opaque here.
        let fun: DeferredFreeFun = unsafe { core::mem::transmute::<usize, DeferredFreeFun>(addr) };
        fun(force, hb, arg);
    }

    /// `mi_thread_init`: ensure the calling thread's default heap is
    /// initialized. Idempotent — safe to call repeatedly.
    pub fn thread_init() {
        super::tls::touch();
    }

    /// `mi_thread_done`: drain + retire empties early. Full page hand-off happens
    /// at real thread exit via the thread_local `Drop`. Idempotent.
    pub fn thread_done() {
        super::tls::collect_lifecycle(true);
    }

    /// `mi_process_init`: force process-wide initialization (keys + this
    /// thread's heap). Idempotent (keys via `OnceBox`; heap via thread_local).
    pub fn process_init() {
        let _ = super::process_keys();
        thread_init();
    }

    /// `mi_process_done`: best-effort cleanup — only drains the calling thread
    /// (the OS reclaims mappings at exit; other threads may still run). Idempotent.
    pub fn process_done() {
        super::tls::collect_lifecycle(true);
    }
}

#[cfg(feature = "std")]
pub use lifecycle::{
    process_done, process_init, register_deferred_free, run_deferred_free, thread_done,
    thread_init, DeferredFreeFun,
};

/// Serializes tests that mutate the process-global deferred-free registry so
/// concurrent test threads don't overwrite each other's registration.
/// Test-only; no effect on the shipped allocator.
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

        // Key the callback to its own `arg` sentinel and the registering thread,
        // so parallel tests' collects never bump these process-global counters.
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

        // A callback that re-enters collection must not fire again (recurse guard
        // bounds it to depth 1). Gate to the registering thread for `DEPTH`.
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
