// SPDX-License-Identifier: MIT
//! Linux/Unix implementation of [`Prim`] via `libc` (ports
//! `src/prim/unix/prim.c`). Uses `mmap`/`munmap`/`mprotect`/`madvise`.

use core::ffi::{c_int, c_void};

use super::{OsMemConfig, Prim, PrimAlloc, PrimError, PrimResult};

/// The Linux primitive implementor (zero-sized).
pub struct Sys;

#[inline]
fn errno() -> i32 {
    // SAFETY: `__errno_location` returns a valid pointer to this thread's errno.
    unsafe { *libc::__errno_location() }
}

#[inline]
fn check(ret: c_int) -> PrimResult<()> {
    if ret == 0 {
        Ok(())
    } else {
        Err(PrimError(errno()))
    }
}

/// Read up to `buf.len()` bytes from a small file; returns bytes read (0 on any
/// error). Used for sysfs queries; avoids `std` so it works in `no_std` too.
fn read_small_file(path: &core::ffi::CStr, buf: &mut [u8]) -> usize {
    // SAFETY: `path` is a valid C string; standard open/read/close sequence.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_RDONLY);
        if fd < 0 {
            return 0;
        }
        let n = libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
        libc::close(fd);
        if n > 0 {
            n as usize
        } else {
            0
        }
    }
}

impl Prim for Sys {
    fn mem_config() -> OsMemConfig {
        // SAFETY: `sysconf` is always safe to call with these queries.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_size = if page_size > 0 {
            page_size as usize
        } else {
            4096
        };
        let phys_pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
        let physical_memory = if phys_pages > 0 {
            (phys_pages as usize).saturating_mul(page_size)
        } else {
            0
        };
        OsMemConfig {
            page_size,
            large_page_size: 2 * 1024 * 1024, // 2 MiB (THP/hugetlb); detection is future work
            alloc_granularity: page_size,
            physical_memory,
            virtual_address_bits: crate::bits::MI_MAX_VABITS,
            has_overcommit: true,
            has_partial_free: true,
            has_virtual_reserve: true,
            has_transparent_huge_pages: false,
        }
    }

    unsafe fn alloc(
        hint: *mut u8,
        size: usize,
        _try_align: usize,
        commit: bool,
        _allow_large: bool,
    ) -> PrimResult<PrimAlloc> {
        let prot = if commit {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_NONE
        };
        // Reserve-only mappings use MAP_NORESERVE so they don't count against
        // the commit limit (Linux overcommit-aware).
        let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        if !commit {
            flags |= libc::MAP_NORESERVE;
        }
        // SAFETY: anonymous mmap with fd = -1; `size` is page aligned per contract.
        let p = unsafe { libc::mmap(hint as *mut c_void, size, prot, flags, -1, 0) };
        if p == libc::MAP_FAILED {
            return Err(PrimError(errno()));
        }
        // Encourage transparent huge pages for large, committed mappings (cuts
        // TLB pressure for arenas). Best-effort: ignore failure.
        if commit && size >= 2 * 1024 * 1024 {
            // SAFETY: `p`/`size` is the mapping we just created.
            unsafe {
                let _ = libc::madvise(p, size, libc::MADV_HUGEPAGE);
            }
        }
        // Explicit large OS pages (MAP_HUGETLB) remain follow-up work.
        Ok(PrimAlloc {
            addr: p as *mut u8,
            is_large: false,
            is_zero: true,
        })
    }

    fn numa_node() -> usize {
        // `getcpu(2)` fills the NUMA node of the calling thread.
        let mut cpu: u32 = 0;
        let mut node: u32 = 0;
        // SAFETY: both out-pointers are valid; third arg is unused (null).
        let r = unsafe {
            libc::syscall(
                libc::SYS_getcpu,
                &mut cpu as *mut u32,
                &mut node as *mut u32,
                core::ptr::null_mut::<c_void>(),
            )
        };
        if r == 0 {
            node as usize
        } else {
            0
        }
    }

    fn numa_node_count() -> usize {
        // Parse the highest node id from `/sys/devices/system/node/online`
        // (e.g. "0-3" ⇒ 4). Best-effort; default 1 on any failure.
        let mut buf = [0u8; 64];
        let n = read_small_file(c"/sys/devices/system/node/online", &mut buf);
        if n == 0 {
            return 1;
        }
        let mut max_node = 0usize;
        let mut cur = 0usize;
        let mut have = false;
        for &c in &buf[..n] {
            if c.is_ascii_digit() {
                cur = cur * 10 + (c - b'0') as usize;
                have = true;
            } else {
                if have && cur > max_node {
                    max_node = cur;
                }
                cur = 0;
                have = false;
            }
        }
        if have && cur > max_node {
            max_node = cur;
        }
        max_node + 1
    }

    unsafe fn free(addr: *mut u8, size: usize) -> PrimResult<()> {
        // SAFETY: caller guarantees `(addr, size)` is an owned mapping/sub-range.
        check(unsafe { libc::munmap(addr as *mut c_void, size) })
    }

    unsafe fn commit(addr: *mut u8, size: usize) -> PrimResult<bool> {
        // SAFETY: caller guarantees a reserved range.
        unsafe {
            check(libc::mprotect(
                addr as *mut c_void,
                size,
                libc::PROT_READ | libc::PROT_WRITE,
            ))?;
        }
        // mprotect does not zero; next-touch behavior depends on prior state.
        Ok(false)
    }

    unsafe fn decommit(addr: *mut u8, size: usize) -> PrimResult<bool> {
        // SAFETY: committed range per contract.
        unsafe {
            check(libc::madvise(
                addr as *mut c_void,
                size,
                libc::MADV_DONTNEED,
            ))?;
        }
        // On Linux MADV_DONTNEED needs no recommit; but in debug/secure builds we
        // also strip access (PROT_NONE) so use-after-free traps — then a recommit
        // is required. (`MI_DEBUG > 0 || MI_SECURE > 2`.)
        let needs_recommit = cfg!(any(feature = "debug", feature = "secure"));
        if needs_recommit {
            // SAFETY: same owned range.
            unsafe {
                check(libc::mprotect(addr as *mut c_void, size, libc::PROT_NONE))?;
            }
        }
        Ok(needs_recommit)
    }

    unsafe fn reset(addr: *mut u8, size: usize) -> PrimResult<()> {
        // Prefer MADV_FREE (lazy, access preserved); fall back to MADV_DONTNEED.
        // SAFETY: committed range per contract.
        let r = unsafe { libc::madvise(addr as *mut c_void, size, libc::MADV_FREE) };
        if r == 0 {
            return Ok(());
        }
        // SAFETY: same range.
        check(unsafe { libc::madvise(addr as *mut c_void, size, libc::MADV_DONTNEED) })
    }

    unsafe fn reuse(_addr: *mut u8, _size: usize) -> PrimResult<()> {
        Ok(()) // no-op on Linux
    }

    unsafe fn protect(addr: *mut u8, size: usize, protect: bool) -> PrimResult<()> {
        let prot = if protect {
            libc::PROT_NONE
        } else {
            libc::PROT_READ | libc::PROT_WRITE
        };
        // SAFETY: owned mapping per contract.
        check(unsafe { libc::mprotect(addr as *mut c_void, size, prot) })
    }

    fn clock_now_msecs() -> i64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid out-pointer.
        let r = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        if r != 0 {
            return 0;
        }
        // `as i64` is portability noise on 64-bit (time_t/c_long are i64) but
        // required on 32-bit targets where they are i32.
        #[allow(clippy::unnecessary_cast)]
        {
            (ts.tv_sec as i64) * 1000 + (ts.tv_nsec as i64) / 1_000_000
        }
    }

    fn random_buf(buf: &mut [u8]) -> bool {
        if buf.is_empty() {
            return true;
        }
        // SAFETY: writing `buf.len()` bytes into a valid mutable slice.
        let n = unsafe { libc::getrandom(buf.as_mut_ptr() as *mut c_void, buf.len(), 0) };
        n == buf.len() as isize
    }

    fn getenv(name: &str, out: &mut [u8]) -> Option<usize> {
        // Build a NUL-terminated name on the stack (option names are short).
        let mut namebuf = [0u8; 256];
        if name.len() >= namebuf.len() {
            return None;
        }
        namebuf[..name.len()].copy_from_slice(name.as_bytes());
        // SAFETY: `namebuf` is NUL-terminated (zero-initialized tail).
        let val = unsafe { libc::getenv(namebuf.as_ptr() as *const core::ffi::c_char) };
        if val.is_null() {
            return None;
        }
        let mut i = 0;
        // SAFETY: `val` is a valid C string from the environment.
        unsafe {
            while i + 1 < out.len() {
                let c = *val.add(i);
                if c == 0 {
                    break;
                }
                out[i] = c as u8;
                i += 1;
            }
        }
        Some(i)
    }

    fn thread_yield() {
        // SAFETY: always safe.
        unsafe {
            libc::sched_yield();
        }
    }

    fn out_stderr(msg: &str) {
        if msg.is_empty() {
            return;
        }
        // SAFETY: writing `msg.len()` bytes from a valid slice to fd 2.
        unsafe {
            let _ = libc::write(2, msg.as_ptr() as *const c_void, msg.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Sys;
    use crate::prim::Prim;

    #[test]
    fn numa_queries_are_sane() {
        let count = Sys::numa_node_count();
        assert!(count >= 1, "at least one NUMA node");
        // numa_node() must not panic and returns a plausible id.
        let node = Sys::numa_node();
        assert!(node < 4096, "node id {node} implausible");
    }
}
