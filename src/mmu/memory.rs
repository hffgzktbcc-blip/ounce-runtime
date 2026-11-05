#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use byteorder::{ByteOrder, LittleEndian};

use super::fastmem::FastmemArena;

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_MASK: u64 = !(PAGE_SIZE as u64 - 1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permission {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl Permission {
    pub const NONE: Self = Self { read: false, write: false, execute: false };
    pub const READ_ONLY: Self = Self { read: true, write: false, execute: false };
    pub const READ_WRITE: Self = Self { read: true, write: true, execute: false };
    pub const READ_EXECUTE: Self = Self { read: true, write: false, execute: true };
    pub const READ_WRITE_EXECUTE: Self = Self { read: true, write: true, execute: true };

    #[inline]
    pub const fn to_horizon_bits(self) -> u32 {
        (if self.read { 1 } else { 0 })
            | (if self.write { 2 } else { 0 })
            | (if self.execute { 4 } else { 0 })
    }

    #[inline]
    pub const fn from_horizon_bits(bits: u32) -> Self {
        Self {
            read: bits & 1 != 0,
            write: bits & 2 != 0,
            execute: bits & 4 != 0,
        }
    }
}

/// ARMv8.1 LSE Atomic Read-Modify-Write operations (`LDADD`, `LDCLR`, `LDEOR`, `LDSET`,
/// `LDSMAX`, `LDSMIN`, `LDUMAX`, `LDUMIN`, `SWP`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicRmwOp {
    Add,
    Clr,
    Eor,
    Set,
    Smax,
    Smin,
    Umax,
    Umin,
    Swp,
}

/// Horizon OS `MemoryState` / `MemoryType` values reported by `svcQueryMemory`.
pub const MEM_STATE_FREE: u32 = 0x00;
pub const MEM_STATE_CODE_STATIC: u32 = 0x03;
pub const MEM_STATE_CODE_MUTABLE: u32 = 0x04;
pub const MEM_STATE_HEAP: u32 = 0x05;
pub const MEM_STATE_SHARED: u32 = 0x06;
pub const MEM_STATE_THREAD_LOCAL: u32 = 0x0C;
pub const MEM_STATE_STACK: u32 = 0x0E;

#[derive(Debug)]
pub enum PageStorage {
    Heap(Box<[u8; PAGE_SIZE]>),
    Fastmem {
        arena: Arc<FastmemArena>,
        offset: usize,
        owns_slot: bool,
    },
}

impl Clone for PageStorage {
    fn clone(&self) -> Self {
        let mut copy = Box::new([0u8; PAGE_SIZE]);
        copy.copy_from_slice(self.as_slice());
        PageStorage::Heap(copy)
    }
}

impl Drop for PageStorage {
    fn drop(&mut self) {
        if let PageStorage::Fastmem {
            arena,
            offset,
            owns_slot: true,
        } = self
        {
            arena.release_offset(*offset);
        }
    }
}

impl PageStorage {
    #[inline]
    pub fn host_ptr(&self) -> *mut u8 {
        match self {
            PageStorage::Heap(b) => b.as_ptr() as *mut u8,
            PageStorage::Fastmem { arena, offset, .. } => arena.page_ptr(*offset),
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8; PAGE_SIZE] {
        match self {
            PageStorage::Heap(b) => b,
            PageStorage::Fastmem { arena, offset, .. } => {
                // SAFETY: `offset` refers to a committed 4 KiB slot inside `arena`.
                unsafe { &*(arena.page_ptr(*offset) as *const [u8; PAGE_SIZE]) }
            }
        }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8; PAGE_SIZE] {
        match self {
            PageStorage::Heap(b) => b,
            PageStorage::Fastmem { arena, offset, .. } => {
                // SAFETY: `offset` refers to a committed 4 KiB slot inside `arena`.
                unsafe { &mut *(arena.page_ptr(*offset) as *mut [u8; PAGE_SIZE]) }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Page {
    pub storage: PageStorage,
    pub perm: Permission,
    /// Horizon OS `MemoryState` low byte (`0x03` CodeStatic, `0x04` CodeMutable, `0x05` Heap, etc.).
    pub mem_state: u32,
    /// Set when a block cache has compiled instructions from this page.
    pub holds_code: bool,
    /// Value of `Memory::code_write_gen` at the last write to this page while it held code.
    pub code_write_gen: u64,
    /// Guarded page (the stage-1 `GP` bit, FEAT_BTI): indirect branches into it need a landing pad.
    pub guarded: bool,
}

impl Page {
    pub fn new(perm: Permission) -> Self {
        Self::with_storage(PageStorage::Heap(Box::new([0u8; PAGE_SIZE])), perm)
    }

    pub fn with_storage(storage: PageStorage, perm: Permission) -> Self {
        let mem_state = if perm.execute || (perm.read && !perm.write) {
            MEM_STATE_CODE_STATIC
        } else {
            MEM_STATE_HEAP
        };
        Self {
            storage,
            perm,
            mem_state,
            holds_code: false,
            code_write_gen: 0,
            guarded: false,
        }
    }

    #[inline]
    pub fn host_ptr(&self) -> *mut u8 {
        self.storage.host_ptr()
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8; PAGE_SIZE] {
        self.storage.as_slice()
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8; PAGE_SIZE] {
        self.storage.as_mut_slice()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("Unmapped memory access at virtual address: 0x{addr:016X}")]
    Unmapped { addr: u64 },

    #[error("Permission denied at 0x{addr:016X}: required {required:?}, actual {actual:?}")]
    PermissionDenied {
        addr: u64,
        required: &'static str,
        actual: Permission,
    },

    #[error("Misaligned memory access at 0x{addr:016X} (size: {size} bytes)")]
    Misaligned { addr: u64, size: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryBucketStats {
    pub total_pages: usize,
    pub total_bytes: usize,
    pub code_bytes: usize,
    pub vram_bytes: usize,
    pub stack_tls_bytes: usize,
    pub heap_data_bytes: usize,
}

/// Virtual Memory Manager (VMM) supporting 64-bit sparse page tables, optional `FastmemArena`
/// backing for zero-copy multi-core execution / NCE, inline TLB generation tracking, and
/// hardware ARMv8.1 LSE atomics.
#[derive(Debug, Default)]
pub struct Memory {
    /// Maps 4KB-aligned virtual page numbers (VA >> 12) to allocated pages
    pages: HashMap<u64, Page>,
    /// Optional sparse host `mmap` arena backing newly mapped pages.
    fastmem: Option<Arc<FastmemArena>>,
    /// Number of mapped pages carrying the FEAT_BTI guarded bit.
    guarded_pages: usize,
    /// Bumped on writes and mapping changes that affect pages holding cached code.
    /// Block caches compare it against the value they last saw to detect self-modifying code.
    code_write_gen: u64,
    /// Bumped on any page mapping, unmapping, move, permission change, or `holds_code` transition
    /// so Tier-4 / NCE inline TLBs flush stale entries immediately.
    tlb_gen: u64,
    /// Last invalidation generation for unmapped pages that held cached code. Retained until a
    /// replacement mapping is installed so unmap+remap between cache sweeps remains observable.
    unmapped_code_gen: HashMap<u64, u64>,
}

impl Memory {
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            fastmem: None,
            guarded_pages: 0,
            code_write_gen: 0,
            tlb_gen: 1,
            unmapped_code_gen: HashMap::new(),
        }
    }

    /// Creates a `Memory` instance backed by a sparse `FastmemArena` (`mmap` virtual reservation).
    pub fn with_fastmem() -> Self {
        let mut mem = Self::new();
        mem.enable_fastmem();
        mem
    }

    /// Creates a `Memory` instance backed by an explicit `FastmemArena` capacity (e.g. in tests).
    pub fn with_fastmem_capacity(capacity: usize) -> Self {
        let mut mem = Self::new();
        mem.fastmem = FastmemArena::with_capacity(capacity);
        mem
    }

    /// Enables `FastmemArena` backing if not already active, migrating any existing heap pages into
    /// the arena so all guest pages live in shared fastmem.
    pub fn enable_fastmem(&mut self) -> bool {
        if self.fastmem.is_none() {
            self.fastmem = FastmemArena::new();
        }
        let Some(arena) = self.fastmem.clone() else {
            return false;
        };
        let mut migrated = false;
        for (&page_idx, page) in &mut self.pages {
            if matches!(page.storage, PageStorage::Heap(_)) {
                if let Some(offset) = arena.alloc_guest_page(page_idx) {
                    let mut tmp = [0u8; PAGE_SIZE];
                    tmp.copy_from_slice(page.storage.as_slice());
                    let mut fast_storage = PageStorage::Fastmem {
                        arena: arena.clone(),
                        offset,
                        owns_slot: true,
                    };
                    fast_storage.as_mut_slice().copy_from_slice(&tmp);
                    page.storage = fast_storage;
                    migrated = true;
                }
            }
        }
        if migrated {
            self.tlb_gen = self.tlb_gen.wrapping_add(1);
        }
        true
    }

    /// Returns the shared `FastmemArena` if fastmem is active.
    #[inline]
    pub fn fastmem_arena(&self) -> Option<&Arc<FastmemArena>> {
        self.fastmem.as_ref()
    }

    /// Creates a lightweight multi-core worker view sharing the exact underlying `FastmemArena`
    /// slots (`owns_slot: false`) so parallel OS worker threads observe real-time hardware memory
    /// and LSE atomic updates without duplicating physical RAM.
    pub fn share_fastmem_view(&self) -> Option<Self> {
        let arena = self.fastmem.as_ref()?.clone();
        let mut shared_pages = HashMap::with_capacity(self.pages.len());
        for (&page_idx, page) in &self.pages {
            let PageStorage::Fastmem { offset, .. } = page.storage else {
                return None;
            };
            shared_pages.insert(
                page_idx,
                Page {
                    storage: PageStorage::Fastmem {
                        arena: arena.clone(),
                        offset,
                        owns_slot: false,
                    },
                    perm: page.perm,
                    mem_state: page.mem_state,
                    holds_code: page.holds_code,
                    code_write_gen: page.code_write_gen,
                    guarded: page.guarded,
                },
            );
        }
        Some(Self {
            pages: shared_pages,
            fastmem: Some(arena),
            guarded_pages: self.guarded_pages,
            code_write_gen: self.code_write_gen,
            tlb_gen: self.tlb_gen,
            unmapped_code_gen: HashMap::new(),
        })
    }

    /// Current code generation. Unchanged means cached code mappings were not written or changed.
    #[inline]
    pub fn code_write_gen(&self) -> u64 {
        self.code_write_gen
    }

    /// Current TLB generation. Bumped whenever page mappings, permissions, or `holds_code` change.
    #[inline]
    pub fn tlb_gen(&self) -> u64 {
        self.tlb_gen
    }

    /// Flags every mapped page in `[start, end)` as holding cached code, so later writes to it
    /// bump `code_write_gen` and inline TLB write tags are cleared.
    pub fn mark_code_range(&mut self, start: u64, end: u64) {
        if end <= start {
            return;
        }
        let mut newly_marked = false;
        for page_idx in (start >> 12)..=((end - 1) >> 12) {
            if let Some(page) = self.pages.get_mut(&page_idx) {
                if !page.holds_code {
                    page.holds_code = true;
                    newly_marked = true;
                }
            }
        }
        if newly_marked {
            self.tlb_gen = self.tlb_gen.wrapping_add(1);
        }
    }

    /// True if any page in `[start, end)` was written, unmapped, or remapped after `since`.
    pub fn code_written_since(&self, start: u64, end: u64, since: u64) -> bool {
        if end <= start {
            return false;
        }
        ((start >> 12)..=((end - 1) >> 12)).any(|page_idx| match self.pages.get(&page_idx) {
            Some(page) => page.code_write_gen > since,
            // Unmapped since compile: the cached copy is stale too.
            None => true,
        })
    }

    /// Computes live memory allocation telemetry categorized by Horizon OS virtual address buckets.
    pub fn bucket_stats(&self) -> MemoryBucketStats {
        let mut stats = MemoryBucketStats {
            total_pages: self.pages.len(),
            total_bytes: self.pages.len() * PAGE_SIZE,
            ..Default::default()
        };
        for (&page_idx, page) in &self.pages {
            let va = page_idx << 12;
            if (0x9000_0000..0xA000_0000).contains(&va) {
                stats.vram_bytes += PAGE_SIZE;
            } else if va >= 0x7FFF_0000_00 {
                stats.stack_tls_bytes += PAGE_SIZE;
            } else if page.perm.execute {
                stats.code_bytes += PAGE_SIZE;
            } else {
                stats.heap_data_bytes += PAGE_SIZE;
            }
        }
        stats
    }

    fn alloc_storage_for_page(&self, page_idx: u64) -> PageStorage {
        if let Some(arena) = &self.fastmem {
            if let Some(offset) = arena.alloc_guest_page(page_idx) {
                return PageStorage::Fastmem {
                    arena: arena.clone(),
                    offset,
                    owns_slot: true,
                };
            }
        }
        PageStorage::Heap(Box::new([0u8; PAGE_SIZE]))
    }

    /// Allocate every page intersecting a contiguous virtual memory range.
    /// An empty range leaves existing mappings unchanged.
    ///
    /// # Panics
    /// Panics before changing any mappings if the range exceeds the 64-bit address space.
    pub fn map_range(&mut self, base_addr: u64, size: usize, perm: Permission) {
        if size == 0 {
            return;
        }

        let last_addr = base_addr
            .checked_add((size - 1) as u64)
            .expect("memory range exceeds the 64-bit address space");
        let start_page = base_addr >> 12;
        let last_page = last_addr >> 12;
        self.tlb_gen = self.tlb_gen.wrapping_add(1);

        for page_idx in start_page..=last_page {
            // Drop any old page first so a Fastmem direct slot is released before re-allocating.
            let old_info = self.pages.remove(&page_idx).map(|old| {
                let g = old.guarded;
                let hc = old.holds_code;
                drop(old);
                (g, hc)
            });
            if let Some((true, _)) = old_info {
                self.guarded_pages -= 1;
            }

            let storage = self.alloc_storage_for_page(page_idx);
            let mut page = Page::with_storage(storage, perm);
            if let Some((_, true)) = old_info {
                // Remapping over cached code replaces it.
                self.code_write_gen += 1;
                page.code_write_gen = self.code_write_gen;
            } else if let Some(generation) = self.unmapped_code_gen.remove(&page_idx) {
                // Preserve an unmap invalidation when the page is remapped before a JIT cache
                // gets a chance to observe that the address was unmapped.
                page.code_write_gen = generation;
            }
            self.pages.insert(page_idx, page);
        }
    }

    /// Moves the pages of `src..src + size` to `dst` (both page aligned): the data appears at `dst` and
    /// `src` becomes unmapped. Models svcMapMemory, after which Horizon makes the source inaccessible.
    /// Unmapped source pages leave the matching destination pages unmapped.
    pub fn move_range(&mut self, src: u64, dst: u64, size: usize) {
        if size == 0 {
            return;
        }
        self.tlb_gen = self.tlb_gen.wrapping_add(1);
        let pages = (size as u64).div_ceil(PAGE_SIZE as u64);
        for i in 0..pages {
            let src_page_idx = (src >> 12) + i;
            let dst_page_idx = (dst >> 12) + i;
            if let Some(mut page) = self.pages.remove(&src_page_idx) {
                let moved_guarded = page.guarded;
                if moved_guarded {
                    self.guarded_pages -= 1;
                }

                // The translation embeds its original guest PC, so moving a code page invalidates
                // the old source address even though the bytes themselves did not change.
                if page.holds_code {
                    self.code_write_gen += 1;
                    self.unmapped_code_gen.insert(src_page_idx, self.code_write_gen);
                }

                let replaced_code = self.pages.remove(&dst_page_idx).is_some_and(|old| {
                    if old.guarded {
                        self.guarded_pages -= 1;
                    }
                    old.holds_code
                });
                let prior_unmapped_generation = self.unmapped_code_gen.remove(&dst_page_idx);
                if replaced_code {
                    self.code_write_gen += 1;
                    page.code_write_gen = self.code_write_gen;
                } else if let Some(generation) = prior_unmapped_generation {
                    page.code_write_gen = page.code_write_gen.max(generation);
                }

                // If Fastmem is active and `dst_page_idx` fits in the direct 1:1 window, relocate
                // the storage to `dst_page_idx`'s canonical direct offset so NCE `base + va` stays 1:1.
                if let Some(arena) = self.fastmem.clone() {
                    if ((dst_page_idx as usize) << 12) + PAGE_SIZE <= arena.direct_limit() {
                        let mut bytes = [0u8; PAGE_SIZE];
                        bytes.copy_from_slice(page.storage.as_slice());
                        // Drop old storage before claiming `dst_page_idx`
                        page.storage = PageStorage::Heap(Box::new([0u8; PAGE_SIZE]));
                        if let Some(new_off) = arena.alloc_guest_page(dst_page_idx) {
                            let mut new_storage = PageStorage::Fastmem {
                                arena,
                                offset: new_off,
                                owns_slot: true,
                            };
                            new_storage.as_mut_slice().copy_from_slice(&bytes);
                            page.storage = new_storage;
                        } else {
                            page.storage.as_mut_slice().copy_from_slice(&bytes);
                        }
                    }
                }

                self.pages.insert(dst_page_idx, page);
                if moved_guarded {
                    self.guarded_pages += 1;
                }
            }
        }
    }

    /// Unmaps every page that overlaps `base_addr..base_addr + size`. Unmapped pages are skipped.
    pub fn unmap_range(&mut self, base_addr: u64, size: usize) {
        if size == 0 {
            return;
        }
        self.tlb_gen = self.tlb_gen.wrapping_add(1);
        let last = base_addr.saturating_add(size as u64 - 1) >> 12;
        for page_idx in (base_addr >> 12)..=last {
            if let Some(page) = self.pages.remove(&page_idx) {
                if page.guarded {
                    self.guarded_pages -= 1;
                }
                if page.holds_code {
                    self.code_write_gen += 1;
                    self.unmapped_code_gen.insert(page_idx, self.code_write_gen);
                }
            }
        }
    }

    /// Updates permissions and optional Horizon `MemoryState` across mapped pages in `[base_addr, base_addr + size)`
    /// without clearing page contents.
    pub fn set_range_protection(
        &mut self,
        base_addr: u64,
        size: usize,
        perm: Permission,
        mem_state: Option<u32>,
    ) {
        if size == 0 {
            return;
        }
        self.tlb_gen = self.tlb_gen.wrapping_add(1);
        let last = base_addr.saturating_add(size as u64 - 1) >> 12;
        for page_idx in (base_addr >> 12)..=last {
            if let Some(page) = self.pages.get_mut(&page_idx) {
                page.perm = perm;
                if let Some(st) = mem_state {
                    page.mem_state = st;
                }
            }
        }
    }

    /// The run of pages around `addr` that are all mapped or all unmapped, as `(start, end, mapped)`.
    /// An unmapped run is bounded by the nearest mapped pages, or by the 39-bit address space.
    pub fn region_at(&self, addr: u64) -> (u64, u64, bool) {
        let page = addr >> 12;
        if self.pages.contains_key(&page) {
            let mut lo = page;
            while lo > 0 && self.pages.contains_key(&(lo - 1)) {
                lo -= 1;
            }
            let mut hi = page + 1;
            while self.pages.contains_key(&hi) {
                hi += 1;
            }
            (lo << 12, hi << 12, true)
        } else {
            let below = self.pages.keys().filter(|&&p| p < page).max().map_or(0, |p| p + 1);
            let above = self.pages.keys().filter(|&&p| p > page).min().map_or(1u64 << 39, |&p| p);
            (below << 12, above << 12, false)
        }
    }

    /// Queries the contiguous run of pages around `addr` that share identical `(mem_state, perm)`,
    /// returning `(start_va, end_va, horizon_state, horizon_perm_bits)` for `svcQueryMemory`.
    pub fn query_region_at(&self, addr: u64, addr_space_bits: u8) -> (u64, u64, u32, u32) {
        let max_page = 1u64 << (addr_space_bits.clamp(36, 48) - 12);
        let page = addr >> 12;
        if let Some(cur) = self.pages.get(&page) {
            let key = (cur.mem_state, cur.perm);
            let mut lo = page;
            while lo > 0 {
                match self.pages.get(&(lo - 1)) {
                    Some(prev) if (prev.mem_state, prev.perm) == key => lo -= 1,
                    _ => break,
                }
            }
            let mut hi = page + 1;
            while let Some(next) = self.pages.get(&hi) {
                if (next.mem_state, next.perm) == key {
                    hi += 1;
                } else {
                    break;
                }
            }
            (lo << 12, hi << 12, cur.mem_state, cur.perm.to_horizon_bits())
        } else {
            let below = self.pages.keys().filter(|&&p| p < page).max().map_or(0, |p| p + 1);
            let above = self
                .pages
                .keys()
                .filter(|&&p| p > page)
                .min()
                .map_or(max_page, |&p| p)
                .min(max_page);
            let end_page = above.max(page + 1);
            (below << 12, end_page << 12, MEM_STATE_FREE, 0)
        }
    }

    pub fn is_mapped(&self, addr: u64) -> bool {
        self.pages.contains_key(&(addr >> 12))
    }

    /// Sets or clears the guarded-page flag (FEAT_BTI) on every mapped page in `[start, start + size)`.
    /// Nothing sets it by default; remapping a page clears it.
    pub fn set_guarded(&mut self, start: u64, size: u64, guarded: bool) {
        if size == 0 {
            return;
        }
        let end = start.saturating_add(size - 1);
        for page_idx in (start >> 12)..=(end >> 12) {
            if let Some(page) = self.pages.get_mut(&page_idx) {
                if page.guarded != guarded {
                    if guarded {
                        self.guarded_pages += 1;
                    } else {
                        self.guarded_pages -= 1;
                    }
                }
                page.guarded = guarded;
            }
        }
    }

    /// True if any mapped page requires FEAT_BTI landing-pad checks.
    #[inline]
    pub fn has_guarded_pages(&self) -> bool {
        self.guarded_pages != 0
    }

    /// True if `addr` is in a mapped guarded page.
    #[inline]
    pub fn is_guarded(&self, addr: u64) -> bool {
        self.pages.get(&(addr >> 12)).is_some_and(|p| p.guarded)
    }

    /// Access the Page for a page number (VA >> 12), if mapped.
    pub fn get_page(&self, page_num: u64) -> Option<&Page> {
        self.pages.get(&page_num)
    }

    /// Read raw byte slice from virtual memory.
    pub fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> Result<(), MemoryError> {
        let mut curr_addr = addr;
        let mut offset = 0;

        while offset < buf.len() {
            let page_idx = curr_addr >> 12;
            let page_offset = (curr_addr as usize) & (PAGE_SIZE - 1);
            let bytes_in_page = (PAGE_SIZE - page_offset).min(buf.len() - offset);

            let page = self.pages.get(&page_idx).ok_or(MemoryError::Unmapped {
                addr: curr_addr,
            })?;

            if !page.perm.read {
                return Err(MemoryError::PermissionDenied {
                    addr: curr_addr,
                    required: "READ",
                    actual: page.perm,
                });
            }

            buf[offset..offset + bytes_in_page]
                .copy_from_slice(&page.as_slice()[page_offset..page_offset + bytes_in_page]);

            curr_addr += bytes_in_page as u64;
            offset += bytes_in_page;
        }

        Ok(())
    }

    /// Write raw byte slice into virtual memory.
    pub fn write_bytes(&mut self, addr: u64, data: &[u8]) -> Result<(), MemoryError> {
        let mut curr_addr = addr;
        let mut offset = 0;

        while offset < data.len() {
            let page_idx = curr_addr >> 12;
            let page_offset = (curr_addr as usize) & (PAGE_SIZE - 1);
            let bytes_in_page = (PAGE_SIZE - page_offset).min(data.len() - offset);

            let page = self.pages.get_mut(&page_idx).ok_or(MemoryError::Unmapped {
                addr: curr_addr,
            })?;

            if !page.perm.write {
                return Err(MemoryError::PermissionDenied {
                    addr: curr_addr,
                    required: "WRITE",
                    actual: page.perm,
                });
            }

            page.as_mut_slice()[page_offset..page_offset + bytes_in_page]
                .copy_from_slice(&data[offset..offset + bytes_in_page]);
            if page.holds_code {
                self.code_write_gen += 1;
                page.code_write_gen = self.code_write_gen;
                self.tlb_gen = self.tlb_gen.wrapping_add(1);
            }

            curr_addr += bytes_in_page as u64;
            offset += bytes_in_page;
        }

        Ok(())
    }

    // Fast intra-page scalar reads (Little Endian) with cross-page fallback
    #[inline]
    pub fn read_u8(&self, addr: u64) -> Result<u8, MemoryError> {
        let page = self
            .pages
            .get(&(addr >> 12))
            .ok_or(MemoryError::Unmapped { addr })?;
        if !page.perm.read {
            return Err(MemoryError::PermissionDenied {
                addr,
                required: "READ",
                actual: page.perm,
            });
        }
        Ok(page.as_slice()[(addr as usize) & (PAGE_SIZE - 1)])
    }

    #[inline]
    pub fn read_u16(&self, addr: u64) -> Result<u16, MemoryError> {
        let off = (addr as usize) & (PAGE_SIZE - 1);
        if off <= PAGE_SIZE - 2 {
            let page = self
                .pages
                .get(&(addr >> 12))
                .ok_or(MemoryError::Unmapped { addr })?;
            if !page.perm.read {
                return Err(MemoryError::PermissionDenied {
                    addr,
                    required: "READ",
                    actual: page.perm,
                });
            }
            return Ok(LittleEndian::read_u16(&page.as_slice()[off..off + 2]));
        }
        let mut buf = [0u8; 2];
        self.read_bytes(addr, &mut buf)?;
        Ok(LittleEndian::read_u16(&buf))
    }

    #[inline]
    pub fn read_u32(&self, addr: u64) -> Result<u32, MemoryError> {
        let off = (addr as usize) & (PAGE_SIZE - 1);
        if off <= PAGE_SIZE - 4 {
            let page = self
                .pages
                .get(&(addr >> 12))
                .ok_or(MemoryError::Unmapped { addr })?;
            if !page.perm.read {
                return Err(MemoryError::PermissionDenied {
                    addr,
                    required: "READ",
                    actual: page.perm,
                });
            }
            return Ok(LittleEndian::read_u32(&page.as_slice()[off..off + 4]));
        }
        let mut buf = [0u8; 4];
        self.read_bytes(addr, &mut buf)?;
        Ok(LittleEndian::read_u32(&buf))
    }

    #[inline]
    pub fn read_u64(&self, addr: u64) -> Result<u64, MemoryError> {
        let off = (addr as usize) & (PAGE_SIZE - 1);
        if off <= PAGE_SIZE - 8 {
            let page = self
                .pages
                .get(&(addr >> 12))
                .ok_or(MemoryError::Unmapped { addr })?;
            if !page.perm.read {
                return Err(MemoryError::PermissionDenied {
                    addr,
                    required: "READ",
                    actual: page.perm,
                });
            }
            return Ok(LittleEndian::read_u64(&page.as_slice()[off..off + 8]));
        }
        let mut buf = [0u8; 8];
        self.read_bytes(addr, &mut buf)?;
        Ok(LittleEndian::read_u64(&buf))
    }

    // Fast intra-page scalar writes (Little Endian) with cross-page fallback
    #[inline]
    pub fn write_u8(&mut self, addr: u64, val: u8) -> Result<(), MemoryError> {
        let page = self
            .pages
            .get_mut(&(addr >> 12))
            .ok_or(MemoryError::Unmapped { addr })?;
        if !page.perm.write {
            return Err(MemoryError::PermissionDenied {
                addr,
                required: "WRITE",
                actual: page.perm,
            });
        }
        page.as_mut_slice()[(addr as usize) & (PAGE_SIZE - 1)] = val;
        if page.holds_code {
            self.code_write_gen += 1;
            page.code_write_gen = self.code_write_gen;
            self.tlb_gen = self.tlb_gen.wrapping_add(1);
        }
        Ok(())
    }

    #[inline]
    pub fn write_u16(&mut self, addr: u64, val: u16) -> Result<(), MemoryError> {
        let off = (addr as usize) & (PAGE_SIZE - 1);
        if off <= PAGE_SIZE - 2 {
            let page = self
                .pages
                .get_mut(&(addr >> 12))
                .ok_or(MemoryError::Unmapped { addr })?;
            if !page.perm.write {
                return Err(MemoryError::PermissionDenied {
                    addr,
                    required: "WRITE",
                    actual: page.perm,
                });
            }
            LittleEndian::write_u16(&mut page.as_mut_slice()[off..off + 2], val);
            if page.holds_code {
                self.code_write_gen += 1;
                page.code_write_gen = self.code_write_gen;
                self.tlb_gen = self.tlb_gen.wrapping_add(1);
            }
            return Ok(());
        }
        let mut buf = [0u8; 2];
        LittleEndian::write_u16(&mut buf, val);
        self.write_bytes(addr, &buf)
    }

    #[inline]
    pub fn write_u32(&mut self, addr: u64, val: u32) -> Result<(), MemoryError> {
        let off = (addr as usize) & (PAGE_SIZE - 1);
        if off <= PAGE_SIZE - 4 {
            let page = self
                .pages
                .get_mut(&(addr >> 12))
                .ok_or(MemoryError::Unmapped { addr })?;
            if !page.perm.write {
                return Err(MemoryError::PermissionDenied {
                    addr,
                    required: "WRITE",
                    actual: page.perm,
                });
            }
            LittleEndian::write_u32(&mut page.as_mut_slice()[off..off + 4], val);
            if page.holds_code {
                self.code_write_gen += 1;
                page.code_write_gen = self.code_write_gen;
                self.tlb_gen = self.tlb_gen.wrapping_add(1);
            }
            return Ok(());
        }
        let mut buf = [0u8; 4];
        LittleEndian::write_u32(&mut buf, val);
        self.write_bytes(addr, &buf)
    }

    #[inline]
    pub fn write_u64(&mut self, addr: u64, val: u64) -> Result<(), MemoryError> {
        let off = (addr as usize) & (PAGE_SIZE - 1);
        if off <= PAGE_SIZE - 8 {
            let page = self
                .pages
                .get_mut(&(addr >> 12))
                .ok_or(MemoryError::Unmapped { addr })?;
            if !page.perm.write {
                return Err(MemoryError::PermissionDenied {
                    addr,
                    required: "WRITE",
                    actual: page.perm,
                });
            }
            LittleEndian::write_u64(&mut page.as_mut_slice()[off..off + 8], val);
            if page.holds_code {
                self.code_write_gen += 1;
                page.code_write_gen = self.code_write_gen;
                self.tlb_gen = self.tlb_gen.wrapping_add(1);
            }
            return Ok(());
        }
        let mut buf = [0u8; 8];
        LittleEndian::write_u64(&mut buf, val);
        self.write_bytes(addr, &buf)
    }

    // --- ARMv8.1 LSE Hardware Atomic Operations (`CAS`, `CASP`, `LDADD`, `SWP`, etc.) ---

    fn checked_atomic_ptr(&mut self, addr: u64, size: usize) -> Result<*mut u8, MemoryError> {
        let off = (addr as usize) & (PAGE_SIZE - 1);
        if (addr as usize) & (size - 1) != 0 || off + size > PAGE_SIZE {
            return Err(MemoryError::Misaligned { addr, size });
        }
        let page = self
            .pages
            .get_mut(&(addr >> 12))
            .ok_or(MemoryError::Unmapped { addr })?;
        if !page.perm.read {
            return Err(MemoryError::PermissionDenied {
                addr,
                required: "READ",
                actual: page.perm,
            });
        }
        if !page.perm.write {
            return Err(MemoryError::PermissionDenied {
                addr,
                required: "WRITE",
                actual: page.perm,
            });
        }
        if page.holds_code {
            self.code_write_gen += 1;
            page.code_write_gen = self.code_write_gen;
            self.tlb_gen = self.tlb_gen.wrapping_add(1);
        }
        // SAFETY: `off + size <= PAGE_SIZE` inside a live 4 KiB-aligned page buffer, and `addr`
        // is `size`-aligned so `host_ptr() + off` satisfies `AtomicU{8,16,32,64}` alignment.
        Ok(unsafe { page.host_ptr().add(off) })
    }

    /// ARMv8.1 LSE `CAS`/`CASA`/`CASL`/`CASAL` (1, 2, 4, or 8 bytes).
    /// Returns the old memory value loaded before the comparison.
    pub fn atomic_cas(
        &mut self,
        addr: u64,
        size_bytes: u8,
        expected: u64,
        desired: u64,
    ) -> Result<u64, MemoryError> {
        let ptr = self.checked_atomic_ptr(addr, size_bytes as usize)?;
        // SAFETY: `checked_atomic_ptr` validated read+write permissions, bounds, and natural alignment.
        let old = unsafe {
            match size_bytes {
                1 => {
                    let a = &*(ptr as *const AtomicU8);
                    match a.compare_exchange(
                        expected as u8,
                        desired as u8,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(v) | Err(v) => v as u64,
                    }
                }
                2 => {
                    let a = &*(ptr as *const AtomicU16);
                    let exp = (expected as u16).to_le();
                    let des = (desired as u16).to_le();
                    match a.compare_exchange(exp, des, Ordering::SeqCst, Ordering::SeqCst) {
                        Ok(v) | Err(v) => u16::from_le(v) as u64,
                    }
                }
                4 => {
                    let a = &*(ptr as *const AtomicU32);
                    let exp = (expected as u32).to_le();
                    let des = (desired as u32).to_le();
                    match a.compare_exchange(exp, des, Ordering::SeqCst, Ordering::SeqCst) {
                        Ok(v) | Err(v) => u32::from_le(v) as u64,
                    }
                }
                8 => {
                    let a = &*(ptr as *const AtomicU64);
                    let exp = expected.to_le();
                    let des = desired.to_le();
                    match a.compare_exchange(exp, des, Ordering::SeqCst, Ordering::SeqCst) {
                        Ok(v) | Err(v) => u64::from_le(v),
                    }
                }
                _ => return Err(MemoryError::Misaligned { addr, size: size_bytes as usize }),
            }
        };
        Ok(old)
    }

    /// ARMv8.1 LSE `CASP`/`CASPA`/`CASPL`/`CASPAL` (pair of 32-bit words or pair of 64-bit doublewords).
    /// Returns `(old_lo, old_hi)`.
    pub fn atomic_cas_pair(
        &mut self,
        addr: u64,
        is_64: bool,
        expected_lo: u64,
        expected_hi: u64,
        desired_lo: u64,
        desired_hi: u64,
    ) -> Result<(u64, u64), MemoryError> {
        if !is_64 {
            // Pair of 32-bit words = single hardware 64-bit CAS!
            let exp64 = (expected_lo & 0xFFFF_FFFF) | ((expected_hi & 0xFFFF_FFFF) << 32);
            let des64 = (desired_lo & 0xFFFF_FFFF) | ((desired_hi & 0xFFFF_FFFF) << 32);
            let old64 = self.atomic_cas(addr, 8, exp64, des64)?;
            return Ok((old64 & 0xFFFF_FFFF, old64 >> 32));
        }

        // Pair of 64-bit doublewords (16-byte aligned).
        let ptr = self.checked_atomic_ptr(addr, 16)?;
        // Serialize 128-bit CASP across threads via a global striped lock so 128-bit pair updates
        // are atomic on all 64-bit hosts without requiring unstable `AtomicU128`.
        static CASP128_LOCKS: [ std::sync::Mutex<()>; 16 ] = [
            std::sync::Mutex::new(()), std::sync::Mutex::new(()),
            std::sync::Mutex::new(()), std::sync::Mutex::new(()),
            std::sync::Mutex::new(()), std::sync::Mutex::new(()),
            std::sync::Mutex::new(()), std::sync::Mutex::new(()),
            std::sync::Mutex::new(()), std::sync::Mutex::new(()),
            std::sync::Mutex::new(()), std::sync::Mutex::new(()),
            std::sync::Mutex::new(()), std::sync::Mutex::new(()),
            std::sync::Mutex::new(()), std::sync::Mutex::new(()),
        ];
        let stripe = ((ptr as usize) >> 4) & 0xF;
        let _guard = CASP128_LOCKS[stripe].lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: `ptr` is 16-byte aligned inside a live RW page and protected by `CASP128_LOCKS`.
        unsafe {
            let a_lo = &*(ptr as *const AtomicU64);
            let a_hi = &*(ptr.add(8) as *const AtomicU64);
            let cur_lo = u64::from_le(a_lo.load(Ordering::SeqCst));
            let cur_hi = u64::from_le(a_hi.load(Ordering::SeqCst));
            if cur_lo == expected_lo && cur_hi == expected_hi {
                a_lo.store(desired_lo.to_le(), Ordering::SeqCst);
                a_hi.store(desired_hi.to_le(), Ordering::SeqCst);
            }
            Ok((cur_lo, cur_hi))
        }
    }

    /// ARMv8.1 LSE Atomic Read-Modify-Write (`LDADD`, `LDCLR`, `LDEOR`, `LDSET`, `LDSMAX`,
    /// `LDSMIN`, `LDUMAX`, `LDUMIN`, `SWP`). Returns the original value before the update.
    pub fn atomic_rmw(
        &mut self,
        addr: u64,
        size_bytes: u8,
        op: AtomicRmwOp,
        operand: u64,
    ) -> Result<u64, MemoryError> {
        let ptr = self.checked_atomic_ptr(addr, size_bytes as usize)?;
        let compute = |cur: u64| -> u64 {
            let bits = (size_bytes as u32) * 8;
            let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
            let c = cur & mask;
            let x = operand & mask;
            let sext = |v: u64| -> i64 {
                let sh = 64 - bits;
                ((v << sh) as i64) >> sh
            };
            let res = match op {
                AtomicRmwOp::Add => c.wrapping_add(x),
                AtomicRmwOp::Clr => c & !x,
                AtomicRmwOp::Eor => c ^ x,
                AtomicRmwOp::Set => c | x,
                AtomicRmwOp::Umax => c.max(x),
                AtomicRmwOp::Umin => c.min(x),
                AtomicRmwOp::Smax => sext(c).max(sext(x)) as u64,
                AtomicRmwOp::Smin => sext(c).min(sext(x)) as u64,
                AtomicRmwOp::Swp => x,
            };
            res & mask
        };

        // SAFETY: `checked_atomic_ptr` validated read+write permissions, bounds, and natural alignment.
        let old = unsafe {
            match size_bytes {
                1 => {
                    let a = &*(ptr as *const AtomicU8);
                    let mut cur = a.load(Ordering::SeqCst);
                    loop {
                        let next = compute(cur as u64) as u8;
                        match a.compare_exchange_weak(cur, next, Ordering::SeqCst, Ordering::SeqCst)
                        {
                            Ok(v) => break v as u64,
                            Err(actual) => cur = actual,
                        }
                    }
                }
                2 => {
                    let a = &*(ptr as *const AtomicU16);
                    let mut cur = a.load(Ordering::SeqCst);
                    loop {
                        let next = (compute(u16::from_le(cur) as u64) as u16).to_le();
                        match a.compare_exchange_weak(cur, next, Ordering::SeqCst, Ordering::SeqCst)
                        {
                            Ok(v) => break u16::from_le(v) as u64,
                            Err(actual) => cur = actual,
                        }
                    }
                }
                4 => {
                    let a = &*(ptr as *const AtomicU32);
                    let mut cur = a.load(Ordering::SeqCst);
                    loop {
                        let next = (compute(u32::from_le(cur) as u64) as u32).to_le();
                        match a.compare_exchange_weak(cur, next, Ordering::SeqCst, Ordering::SeqCst)
                        {
                            Ok(v) => break u32::from_le(v) as u64,
                            Err(actual) => cur = actual,
                        }
                    }
                }
                8 => {
                    let a = &*(ptr as *const AtomicU64);
                    let mut cur = a.load(Ordering::SeqCst);
                    loop {
                        let next = compute(u64::from_le(cur)).to_le();
                        match a.compare_exchange_weak(cur, next, Ordering::SeqCst, Ordering::SeqCst)
                        {
                            Ok(v) => break u64::from_le(v),
                            Err(actual) => cur = actual,
                        }
                    }
                }
                _ => return Err(MemoryError::Misaligned { addr, size: size_bytes as usize }),
            }
        };
        Ok(old)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guarded_page_count_tracks_mapping_changes() {
        let mut memory = Memory::new();
        memory.map_range(0x1000, 0x3000, Permission::READ_WRITE);
        memory.set_guarded(0x1000, 0x1000, true);
        memory.set_guarded(0x3000, 0x1000, true);
        assert!(memory.has_guarded_pages());

        memory.map_range(0x1000, 0x1000, Permission::READ_WRITE);
        assert!(memory.has_guarded_pages());
        memory.move_range(0x3000, 0x4000, PAGE_SIZE);
        memory.unmap_range(0x4000, PAGE_SIZE);
        assert!(!memory.has_guarded_pages());
    }

    #[test]
    fn test_map_range_covers_exactly_the_intersecting_pages() {
        let cases = [
            (0x0000, 1, 1),
            (0x1000, PAGE_SIZE, 1),
            (0x1000, PAGE_SIZE + 1, 2),
            (0x1001, PAGE_SIZE - 1, 1),
            (0x1001, PAGE_SIZE, 2),
            (0x1FFF, 1, 1),
            (0x1FFF, 2, 2),
            (0x1FFF, PAGE_SIZE + 1, 2),
            (0x1FFF, PAGE_SIZE + 2, 3),
        ];

        for (base_addr, size, expected_pages) in cases {
            let mut memory = Memory::new();
            memory.map_range(base_addr, size, Permission::READ_WRITE);

            let first_page = base_addr >> 12;
            assert_eq!(memory.pages.len(), expected_pages);
            for page in first_page..first_page + expected_pages as u64 {
                assert!(memory.is_mapped(page * PAGE_SIZE as u64));
            }
            if first_page > 0 {
                assert!(!memory.is_mapped((first_page - 1) * PAGE_SIZE as u64));
            }
            assert!(!memory.is_mapped((first_page + expected_pages as u64) * PAGE_SIZE as u64));
        }
    }

    #[test]
    fn test_read_write_bytes_across_unaligned_mapped_pages() {
        let mut memory = Memory::new();
        let base_addr = 0x1FFF;
        let data: Vec<u8> = (0..PAGE_SIZE + 2).map(|i| i as u8).collect();
        memory.map_range(base_addr, data.len(), Permission::READ_WRITE);

        memory.write_bytes(base_addr, &data).unwrap();
        let mut actual = vec![0; data.len()];
        memory.read_bytes(base_addr, &mut actual).unwrap();
        assert_eq!(actual, data);
    }

    #[test]
    fn test_read_write_scalar_across_page_boundary() {
        let mut memory = Memory::new();
        memory.map_range(0x1FFC, 8, Permission::READ_WRITE);

        memory.write_u64(0x1FFC, 0x0123_4567_89AB_CDEF).unwrap();
        assert_eq!(memory.read_u64(0x1FFC).unwrap(), 0x0123_4567_89AB_CDEF);
        assert_eq!(memory.read_u32(0x2000).unwrap(), 0x0123_4567);
    }

    #[test]
    fn test_read_only_permissions_apply_to_every_mapped_page() {
        let mut memory = Memory::new();
        memory.map_range(0x1FFF, 2, Permission::READ_ONLY);

        let mut actual = [1; 2];
        memory.read_bytes(0x1FFF, &mut actual).unwrap();
        assert_eq!(actual, [0; 2]);

        for addr in [0x1FFF, 0x2000] {
            assert!(matches!(
                memory.write_u8(addr, 1),
                Err(MemoryError::PermissionDenied {
                    addr: denied_addr,
                    required: "WRITE",
                    actual: Permission::READ_ONLY,
                }) if denied_addr == addr
            ));
            assert_eq!(memory.read_u8(addr).unwrap(), 0);
        }
    }

    #[test]
    fn test_access_to_adjacent_unmapped_page_is_rejected() {
        let mut memory = Memory::new();
        memory.map_range(0x1FFF, 2, Permission::READ_WRITE);

        assert!(matches!(
            memory.read_u8(0x3000),
            Err(MemoryError::Unmapped { addr: 0x3000 })
        ));
        assert!(matches!(
            memory.write_u8(0x3000, 1),
            Err(MemoryError::Unmapped { addr: 0x3000 })
        ));
    }

    #[test]
    fn test_empty_range_does_not_map_or_replace_pages() {
        let mut memory = Memory::new();
        memory.map_range(0x1000, PAGE_SIZE, Permission::READ_WRITE);
        memory.write_u8(0x1FFF, 42).unwrap();

        for base_addr in [0, 0x1000, 0x1FFF, 0x2000, u64::MAX] {
            memory.map_range(base_addr, 0, Permission::NONE);
        }

        assert_eq!(memory.pages.len(), 1);
        assert_eq!(memory.read_u8(0x1FFF).unwrap(), 42);
        memory.write_u8(0x1FFF, 43).unwrap();
        assert_eq!(memory.read_u8(0x1FFF).unwrap(), 43);
    }

    #[test]
    fn test_map_range_can_end_at_the_last_address() {
        let cases = [
            (u64::MAX, 1, 1),
            (u64::MAX - PAGE_SIZE as u64 + 1, PAGE_SIZE, 1),
            (u64::MAX - PAGE_SIZE as u64, PAGE_SIZE + 1, 2),
        ];

        for (base_addr, size, expected_pages) in cases {
            let mut memory = Memory::new();
            memory.map_range(base_addr, size, Permission::READ_WRITE);

            assert_eq!(memory.pages.len(), expected_pages);
            assert!(memory.is_mapped(base_addr));
            assert!(memory.is_mapped(u64::MAX));
            assert!(!memory.is_mapped(0));
        }
    }

    #[test]
    fn test_overflowing_range_is_rejected_before_mapping() {
        let mut memory = Memory::new();
        memory.map_range(0x1000, PAGE_SIZE, Permission::READ_WRITE);
        memory.write_u8(0x1000, 42).unwrap();

        for (base_addr, size) in [
            (u64::MAX, 2),
            (u64::MAX - PAGE_SIZE as u64 + 1, PAGE_SIZE + 1),
        ] {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                memory.map_range(base_addr, size, Permission::READ_WRITE);
            }));

            assert!(result.is_err());
            assert_eq!(memory.pages.len(), 1);
            assert_eq!(memory.read_u8(0x1000).unwrap(), 42);
        }
    }

    #[test]
    fn test_fastmem_shared_multithreaded_hardware_atomics() {
        let mut mem = Memory::with_fastmem_capacity(64 << 20);
        let shared_va = 0x0020_0000u64;
        mem.map_range(shared_va, PAGE_SIZE, Permission::READ_WRITE);
        mem.write_u64(shared_va, 0).unwrap();
        mem.write_u32(shared_va + 8, 10).unwrap();

        // Spawn 4 real OS threads sharing the same `FastmemArena` backing and hammering `LDADD`!
        let mut handles = Vec::new();
        for _ in 0..4 {
            let mut worker_mem = mem.share_fastmem_view().expect("shared fastmem view");
            handles.push(std::thread::spawn(move || {
                for _ in 0..2_500 {
                    worker_mem.atomic_rmw(shared_va, 8, AtomicRmwOp::Add, 1).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(mem.read_u64(shared_va).unwrap(), 10_000);

        // Verify CAS + CASP
        assert_eq!(mem.atomic_cas(shared_va + 8, 4, 10, 99).unwrap(), 10);
        assert_eq!(mem.read_u32(shared_va + 8).unwrap(), 99);
        assert_eq!(mem.atomic_cas(shared_va + 8, 4, 10, 123).unwrap(), 99);
        assert_eq!(mem.read_u32(shared_va + 8).unwrap(), 99);
    }
}
