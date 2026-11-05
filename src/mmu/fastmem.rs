#![allow(dead_code)]

//! Sparse host virtual memory arena (`FastmemArena`) for Switch 2 (`Ounce` / Tegra T239)
//! 42-bit unified memory, multi-core shared execution, Tier-4 inline TLB, and Apple Silicon NCE.
//!
//! Design:
//! - Reserves a contiguous host virtual region with `PROT_NONE` (`MAP_PRIVATE | MAP_ANON`),
//!   consuming **0 bytes of physical RSS** until specific OS pages (4 KiB on Linux/x64,
//!   16 KiB on Apple Silicon macOS) are committed on demand via `mprotect(PROT_READ | PROT_WRITE)`.
//! - Guest virtual addresses below `direct_limit` (`0..direct_limit`) map 1:1 to `base + guest_va`,
//!   keeping contiguous Switch 2 code, VRAM (`0x9000_0000`), and 12 GiB Ounce heap regions
//!   contiguous in host virtual memory for NCE (`base + va`) and zero-copy DMA/GPU uploads.
//! - High guest virtual addresses (`>= direct_limit`, e.g. Horizon stack `0x7F_FFFF_E000` and TLS
//!   `0x7F_FB00_0000`) receive 4 KiB slots from a dedicated high-VA bump slab inside the same arena,
//!   so every guest page shares a single `Arc<FastmemArena>` across multi-core worker threads.

use std::collections::HashSet;
use std::fmt;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use super::memory::PAGE_SIZE;

/// Default sparse fastmem reservation: 16 GiB (14 GiB direct 1:1 guest VA window + 2 GiB high-VA slab).
pub const DEFAULT_FASTMEM_CAPACITY: usize = 16usize << 30;
/// Fraction of the arena reserved for 1:1 direct guest VA placement (`7/8` of capacity = 14 GiB by default).
const DIRECT_NUMERATOR: usize = 7;
const DIRECT_DENOMINATOR: usize = 8;

struct ArenaState {
    /// Host OS page indices (`offset / host_page_size`) already switched to `PROT_READ | PROT_WRITE`.
    committed_host_pages: HashSet<usize>,
    /// Next 4 KiB offset in `[direct_limit, capacity)` for high-VA guest pages (`>= direct_limit`).
    next_slab_offset: usize,
    /// Claimed 4 KiB slots in the direct region (`offset >> 12`), so remapping or collisions fall back cleanly.
    claimed_direct_pages: HashSet<usize>,
}

/// Thread-safe (`Send + Sync`) sparse host virtual memory backing for guest pages.
pub struct FastmemArena {
    base: NonNull<u8>,
    capacity: usize,
    direct_limit: usize,
    host_page_size: usize,
    is_mmap: bool,
    state: Mutex<ArenaState>,
}

// SAFETY: `FastmemArena` owns an OS `mmap` allocation (or heap fallback) whose lifetime is tied
// to `Arc<FastmemArena>` and only unmapped in `Drop` when the last reference disappears.
// Page commits and slot allocation are serialized by `state: Mutex<ArenaState>`.
unsafe impl Send for FastmemArena {}
unsafe impl Sync for FastmemArena {}

impl fmt::Debug for FastmemArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let committed = self
            .state
            .lock()
            .map(|s| s.committed_host_pages.len() * self.host_page_size)
            .unwrap_or(0);
        f.debug_struct("FastmemArena")
            .field("base", &self.base)
            .field("capacity_mib", &(self.capacity >> 20))
            .field("direct_limit_mib", &(self.direct_limit >> 20))
            .field("host_page_size", &self.host_page_size)
            .field("committed_bytes", &committed)
            .finish()
    }
}

impl FastmemArena {
    /// Creates a new default 16 GiB sparse `FastmemArena`, falling back to smaller reservations if needed.
    pub fn new() -> Option<Arc<Self>> {
        for shift in [34usize, 33, 32, 30] {
            if let Some(arena) = Self::with_capacity(1usize << shift) {
                return Some(arena);
            }
        }
        None
    }

    /// Reserves a sparse `FastmemArena` of `capacity` bytes (rounded up to OS page size).
    pub fn with_capacity(capacity: usize) -> Option<Arc<Self>> {
        let host_page_size = host_os_page_size().max(PAGE_SIZE);
        let min_cap = host_page_size * 16;
        let capacity = capacity.max(min_cap).next_multiple_of(host_page_size);
        let direct_limit = ((capacity / DIRECT_DENOMINATOR) * DIRECT_NUMERATOR) & !(PAGE_SIZE - 1);

        let (base, is_mmap) = reserve_sparse_region(capacity)?;
        Some(Arc::new(Self {
            base,
            capacity,
            direct_limit,
            host_page_size,
            is_mmap,
            state: Mutex::new(ArenaState {
                committed_host_pages: HashSet::new(),
                next_slab_offset: direct_limit,
                claimed_direct_pages: HashSet::new(),
            }),
        }))
    }

    /// Raw host base pointer of the sparse arena (`guest_va < direct_limit` maps to `base + guest_va`).
    #[inline]
    pub fn base_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// Total virtual reservation in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Upper bound (exclusive) of the 1:1 direct guest VA region (`guest_va < direct_limit`).
    #[inline]
    pub fn direct_limit(&self) -> usize {
        self.direct_limit
    }

    /// Host kernel page size (e.g. 16384 on Apple Silicon macOS, 4096 on x86_64/Linux).
    #[inline]
    pub fn host_page_size(&self) -> usize {
        self.host_page_size
    }

    /// Bytes committed to `PROT_READ | PROT_WRITE` so far.
    pub fn committed_bytes(&self) -> usize {
        self.state
            .lock()
            .map(|s| s.committed_host_pages.len() * self.host_page_size)
            .unwrap_or(0)
    }

    /// Allocates and zeroes a 4 KiB slot for `guest_page_num` (`VA >> 12`), committing its
    /// enclosing host page if not yet committed. Returns the byte offset inside the arena.
    pub fn alloc_guest_page(&self, guest_page_num: u64) -> Option<usize> {
        let mut guard = self.state.lock().ok()?;
        let direct_byte_off = (guest_page_num as usize).checked_shl(12);

        let offset = match direct_byte_off {
            Some(off)
                if off + PAGE_SIZE <= self.direct_limit
                    && !guard.claimed_direct_pages.contains(&(off >> 12)) =>
            {
                guard.claimed_direct_pages.insert(off >> 12);
                off
            }
            _ => {
                if guard.next_slab_offset + PAGE_SIZE > self.capacity {
                    return None;
                }
                let off = guard.next_slab_offset;
                guard.next_slab_offset += PAGE_SIZE;
                off
            }
        };

        let host_page_idx = offset / self.host_page_size;
        if !guard.committed_host_pages.contains(&host_page_idx) {
            let host_off = host_page_idx * self.host_page_size;
            if !commit_host_page(self.base.as_ptr(), host_off, self.host_page_size, self.is_mmap) {
                return None;
            }
            guard.committed_host_pages.insert(host_page_idx);
        }

        // Zero this 4 KiB guest page slice so newly mapped or remapped pages always start clean.
        // SAFETY: `[offset, offset + PAGE_SIZE)` lies within a committed `PROT_READ | PROT_WRITE`
        // host page of `self.base` and is exclusively initialized here before returning `offset`.
        unsafe {
            std::ptr::write_bytes(self.base.as_ptr().add(offset), 0, PAGE_SIZE);
        }
        Some(offset)
    }

    /// Releases a previously claimed direct-mapped 4 KiB slot so a subsequent `map_range` at the
    /// same guest VA reuses the canonical 1:1 `base + va` offset.
    pub fn release_offset(&self, offset: usize) {
        if offset < self.direct_limit {
            if let Ok(mut guard) = self.state.lock() {
                guard.claimed_direct_pages.remove(&(offset >> 12));
            }
        }
    }

    /// Host pointer to a 4 KiB slot previously returned by [`Self::alloc_guest_page`].
    #[inline]
    pub fn page_ptr(&self, offset: usize) -> *mut u8 {
        debug_assert!(offset + PAGE_SIZE <= self.capacity);
        // SAFETY: `offset + PAGE_SIZE <= self.capacity` within the live arena reservation.
        unsafe { self.base.as_ptr().add(offset) }
    }
}

impl Drop for FastmemArena {
    fn drop(&mut self) {
        release_sparse_region(self.base, self.capacity, self.is_mmap);
    }
}

#[cfg(unix)]
fn host_os_page_size() -> usize {
    // SAFETY: `sysconf(_SC_PAGESIZE)` has no preconditions.
    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if sz > 0 { sz as usize } else { PAGE_SIZE }
}

#[cfg(not(unix))]
fn host_os_page_size() -> usize {
    PAGE_SIZE
}

#[cfg(unix)]
fn reserve_sparse_region(capacity: usize) -> Option<(NonNull<u8>, bool)> {
    // SAFETY: anonymous `PROT_NONE` reservation at an OS-chosen address; touches no existing mappings.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            capacity,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        None
    } else {
        NonNull::new(ptr as *mut u8).map(|nn| (nn, true))
    }
}

#[cfg(not(unix))]
fn reserve_sparse_region(capacity: usize) -> Option<(NonNull<u8>, bool)> {
    // Portable fallback for non-Unix targets: cap reservation to a modest heap slab.
    let capped = capacity.min(64 << 20);
    let mut vec = vec![0u8; capped].into_boxed_slice();
    let ptr = vec.as_mut_ptr();
    std::mem::forget(vec);
    NonNull::new(ptr).map(|nn| (nn, false))
}

#[cfg(unix)]
fn commit_host_page(base: *mut u8, host_offset: usize, len: usize, is_mmap: bool) -> bool {
    if !is_mmap {
        return true;
    }
    // SAFETY: `base + host_offset` is host-page-aligned inside our `mmap` region of `capacity >= host_offset + len`.
    let rc = unsafe {
        libc::mprotect(
            base.add(host_offset) as *mut libc::c_void,
            len,
            libc::PROT_READ | libc::PROT_WRITE,
        )
    };
    rc == 0
}

#[cfg(not(unix))]
fn commit_host_page(_base: *mut u8, _host_offset: usize, _len: usize, _is_mmap: bool) -> bool {
    true
}

#[cfg(unix)]
fn release_sparse_region(base: NonNull<u8>, capacity: usize, is_mmap: bool) {
    if is_mmap {
        // SAFETY: `base` and `capacity` come from `reserve_sparse_region` and `FastmemArena` is being dropped.
        unsafe {
            libc::munmap(base.as_ptr() as *mut libc::c_void, capacity);
        }
    }
}

#[cfg(not(unix))]
fn release_sparse_region(base: NonNull<u8>, capacity: usize, _is_mmap: bool) {
    let capped = capacity.min(64 << 20);
    // SAFETY: reconstructs the boxed slice allocated in `reserve_sparse_region`.
    unsafe {
        let slice = std::ptr::slice_from_raw_parts_mut(base.as_ptr(), capped);
        drop(Box::from_raw(slice));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fastmem_arena_reserves_and_commits_direct_and_high_va_pages() {
        let arena = FastmemArena::with_capacity(64 << 20).expect("64 MiB sparse arena");
        assert_eq!(arena.committed_bytes(), 0);

        // Low VA (0x0010_0000 = 1 MiB) maps 1:1 to offset 0x0010_0000.
        let low_off = arena.alloc_guest_page(0x0010_0000 >> 12).expect("direct slot");
        assert_eq!(low_off, 0x0010_0000);
        assert!(arena.committed_bytes() >= PAGE_SIZE);

        // High VA (Horizon stack 0x7FFF_FFE000) maps into the high-VA bump slab.
        let high_off = arena.alloc_guest_page(0x7FFF_FFE0_00 >> 12).expect("slab slot");
        assert!(high_off >= arena.direct_limit());

        unsafe {
            let p_low = arena.page_ptr(low_off) as *mut u64;
            let p_high = arena.page_ptr(high_off) as *mut u64;
            p_low.write(0xCAFE_BABE_DEAD_BEEF);
            p_high.write(0x1122_3344_5566_7788);
            assert_eq!(p_low.read(), 0xCAFE_BABE_DEAD_BEEF);
            assert_eq!(p_high.read(), 0x1122_3344_5566_7788);
        }
    }
}
