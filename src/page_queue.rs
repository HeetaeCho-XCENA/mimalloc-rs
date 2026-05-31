// SPDX-License-Identifier: MIT
//! Per-bin page queues (ports `src/page-queue.c`).
//!
//! A heap keeps one [`PageQueue`] per size-class bin (plus the special
//! `MI_BIN_FULL` queue for pages with no free blocks). Queue links are the
//! `next`/`prev` cells inside each [`Page`]. These structures are owned by a
//! single heap/thread, so they use `Cell` (no atomics).

use core::cell::Cell;

use crate::page::Page;

/// A doubly-linked list of pages that share a block size.
///
/// Use [`PageQueue::new`] (a `const fn`) to construct; no `Default` derive so
/// the crate builds on the MSRV (raw-pointer `Default` is newer than 1.84).
pub struct PageQueue {
    first: Cell<*mut Page>,
    last: Cell<*mut Page>,
    count: Cell<usize>,
}

impl Default for PageQueue {
    fn default() -> Self {
        // Manual impl (not derived): a derive would require `*mut Page: Default`,
        // which is newer than our MSRV. Delegating to `new()` avoids that.
        Self::new()
    }
}

impl PageQueue {
    /// An empty queue.
    pub const fn new() -> Self {
        PageQueue {
            first: Cell::new(core::ptr::null_mut()),
            last: Cell::new(core::ptr::null_mut()),
            count: Cell::new(0),
        }
    }

    /// First page, or null.
    #[inline]
    pub fn first(&self) -> *mut Page {
        self.first.get()
    }

    /// Number of pages in the queue.
    #[inline]
    pub fn len(&self) -> usize {
        self.count.get()
    }

    /// Is the queue empty?
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count.get() == 0
    }

    /// Push `page` to the front.
    ///
    /// # Safety
    /// `page` must be a valid page not currently in any queue.
    pub unsafe fn push_front(&self, page: *mut Page) {
        // SAFETY: caller guarantees `page` is valid and unlinked.
        let p = unsafe { &*page };
        let old_first = self.first.get();
        p.prev.set(core::ptr::null_mut());
        p.next.set(old_first);
        if old_first.is_null() {
            self.last.set(page);
        } else {
            // SAFETY: old_first is a valid linked page.
            unsafe { (*old_first).prev.set(page) };
        }
        self.first.set(page);
        self.count.set(self.count.get() + 1);
    }

    /// Remove `page` from this queue.
    ///
    /// # Safety
    /// `page` must currently be linked in *this* queue.
    pub unsafe fn remove(&self, page: *mut Page) {
        // SAFETY: caller guarantees `page` is linked here.
        let p = unsafe { &*page };
        let prev = p.prev.get();
        let next = p.next.get();
        if prev.is_null() {
            self.first.set(next);
        } else {
            // SAFETY: prev is a valid linked page.
            unsafe { (*prev).next.set(next) };
        }
        if next.is_null() {
            self.last.set(prev);
        } else {
            // SAFETY: next is a valid linked page.
            unsafe { (*next).prev.set(prev) };
        }
        p.next.set(core::ptr::null_mut());
        p.prev.set(core::ptr::null_mut());
        self.count.set(self.count.get() - 1);
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::arena::Arena;

    #[test]
    fn push_and_remove() {
        let arena = Arena::create(8, true).unwrap();
        // SAFETY: fresh arena; build three real pages.
        unsafe {
            let a = arena.as_ref();
            let mut pages = alloc::vec::Vec::new();
            for _ in 0..3 {
                let (idx, p) = a.alloc_slices(1, 0).unwrap();
                pages.push(Page::init(p, idx, 1, 64, [1, 2]).as_ptr());
            }
            let q = PageQueue::new();
            for &pg in &pages {
                q.push_front(pg);
            }
            assert_eq!(q.len(), 3);
            assert_eq!(q.first(), pages[2]); // last pushed is first

            // remove the middle one
            q.remove(pages[1]);
            assert_eq!(q.len(), 2);
            // walk the queue and confirm pages[1] is gone, others present
            let mut seen = alloc::vec::Vec::new();
            let mut cur = q.first();
            while !cur.is_null() {
                seen.push(cur);
                cur = (*cur).next.get();
            }
            assert_eq!(seen.len(), 2);
            assert!(seen.contains(&pages[0]));
            assert!(seen.contains(&pages[2]));
            assert!(!seen.contains(&pages[1]));

            Arena::destroy(arena);
        }
    }

    extern crate alloc;
}
