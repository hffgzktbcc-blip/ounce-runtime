#![allow(dead_code)]

//! Switch 2 ("Ounce" / Cortex-A78C) Native Code Execution (`NceEngine`) for AArch64 hosts
//! (Apple Silicon M1/M2/M3/M4 macOS `MAP_JIT` & Linux AArch64).
//!
//! Architecture (inspired by `Eglantine` / `Eden-NX2` NCE + Fastmem):
//! - Compiles guest AArch64 basic blocks directly into host AArch64 `MAP_JIT` executable slabs
//!   with **zero Cranelift IR/regalloc overhead**:
//!   - Guest `X0..X15` & `XZR` map 1:1 to host `X0..X15` & `XZR` (verbatim execution of ALU,
//!     shifts, bitfields, multiplies, divides, conditional selects/compares, and flag updates!).
//!   - Guest `SP` maps to callee-saved host `X21`, and guest `X30` (`LR`) maps to host `X22`.
//!   - Host `X18` is **never touched** (strict Apple Silicon platform-register ABI safety).
//!   - Guest `ADR`/`ADRP` are materialized to exact 64-bit guest virtual addresses (`MOVZ`/`MOVK`).
//!   - Guest `MRS/MSR TPIDR_EL0` (`0x5E82`), `TPIDRRO_EL0` (`0x5E83`), and `NZCV` (`0x5A10`)
//!     execute inline against `NceState` (`[X19]`) with zero traps or host TLS clobbering.
//!   - Guest `LDR`/`STR` (`B`/`H`/`W`/`X` + signed `SB`/`SH`/`SW`) execute via an **8-instruction
//!     zero-`NZCV`-clobber inline Fastmem TLB** (`EOR` + `CBNZ`) backed by `FastmemArena` / `Memory`.
//! - Blocks using `X16..X29`, SIMD/FP, or LSE atomics delegate seamlessly to Tier-4 / Tier-3 /
//!   `Cpu::step`, preserving 100% architectural parity across every instruction in the ISA.

use std::collections::HashMap;
use std::time::Instant;

use super::decoder::decode;
use super::instructions::{BranchRegOp, Instruction, LogicalOp};
use super::interpreter::{Cpu, StepResult};
use super::pauth;
use super::tier2_jit::BlockExit;
use super::tier3_jit::Tier3JitEngine;
#[cfg(feature = "native-jit")]
use super::tier4::Tier4JitEngine;
use crate::mmu::Memory;

/// Number of entries in the direct-mapped NCE inline TLB (`(va >> 12) & 511`).
pub const NCE_TLB_SIZE: usize = 512;
pub const NCE_TLB_MASK: u64 = (NCE_TLB_SIZE as u64) - 1;

const MAX_NCE_BLOCK_INSNS: usize = 64;

// NCE block exit codes (low 16 bits; `SVC` packs `imm16` into bits 16..32).
const NCE_EXIT_FALLTHROUGH: u64 = 0;
const NCE_EXIT_BRANCH: u64 = 1;
const NCE_EXIT_CALL: u64 = 2;
const NCE_EXIT_INDIRECT: u64 = 3;
const NCE_EXIT_SVC: u64 = 4;
const NCE_EXIT_TLB_MISS: u64 = 5;

/// 32-byte `#[repr(C)]` direct-mapped TLB entry read by NCE inline fastmem loads and stores.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NceTlbEntry {
    /// Guest page number (`va >> 12`) if readable, else `u64::MAX`. Offset `0x00`.
    pub read_tag: u64,
    /// Guest page number (`va >> 12`) if writable and `!holds_code`, else `u64::MAX`. Offset `0x08`.
    pub write_tag: u64,
    /// `host_base.wrapping_sub(page_num << 12)` so `va + addend == host_ptr`. Offset `0x10`.
    pub addend: u64,
    pub _pad: u64,
}

impl Default for NceTlbEntry {
    fn default() -> Self {
        Self {
            read_tag: u64::MAX,
            write_tag: u64::MAX,
            addend: 0,
            _pad: 0,
        }
    }
}

/// Guest register bridge passed to NCE native blocks in `X0` (and pinned in `X19` inside the block).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NceState {
    /// `0x000..0x080`: Guest `X0..X15`.
    pub x: [u64; 16],
    /// `0x080`: Guest `X30` (`LR`).
    pub x30: u64,
    /// `0x088`: Guest `SP`.
    pub sp: u64,
    /// `0x090`: Next guest `PC` written on exit.
    pub pc: u64,
    /// `0x098`: Guest `NZCV` in bits 31..28.
    pub nzcv: u64,
    /// `0x0A0`: Guest `TPIDR_EL0`.
    pub tpidr_el0: u64,
    /// `0x0A8`: Guest `TPIDRRO_EL0` (Horizon TLS pointer).
    pub tpidrro_el0: u64,
    /// `0x0B0`: Pointer to `[NceTlbEntry; NCE_TLB_SIZE]`.
    pub tlb_ptr: u64,
    /// `0x0B8`: Number of guest instructions retired before exiting the block.
    pub insns_executed: u64,
    /// `0x0C0`: Exit reason (`NCE_EXIT_*`).
    pub exit_code: u64,
}

#[derive(Debug, Clone, Copy)]
struct IndirectBranchMeta {
    branch_pc: u64,
    rn: u8,
    op: BranchRegOp,
}

// ============================================================================
// Host AArch64 Executable Code Memory (`MAP_JIT` on Apple Silicon, `mprotect` on Linux)
// ============================================================================

#[cfg(target_arch = "aarch64")]
mod aarch64_host {
    use super::*;

    /// Default `MAP_JIT` slab size on Apple Silicon: 1 MiB (holds ~4,000 compiled NCE blocks).
    #[cfg(target_os = "macos")]
    const MACOS_SLAB_BYTES: usize = 1 << 20;

    #[cfg(target_os = "macos")]
    struct JitSlab {
        ptr: *mut u8,
        cap: usize,
        used: usize,
    }

    #[cfg(target_os = "macos")]
    impl Drop for JitSlab {
        fn drop(&mut self) {
            // SAFETY: `ptr` was allocated with `mmap(..., cap, ...)` and is no longer executed.
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.cap);
            }
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    struct SealedPage {
        ptr: *mut u8,
        mapped: usize,
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    impl Drop for SealedPage {
        fn drop(&mut self) {
            // SAFETY: `ptr` was allocated with `mmap` of size `mapped`.
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.mapped);
            }
        }
    }

    /// Owns all executable host code buffers for an [`NceEngine`].
    pub struct NceCodeAllocator {
        #[cfg(target_os = "macos")]
        slabs: Vec<JitSlab>,
        #[cfg(all(unix, not(target_os = "macos")))]
        pages: Vec<SealedPage>,
    }

    // SAFETY: Code pointers inside `NceCodeAllocator` are sealed before execution and only
    // mutated while holding `&mut NceEngine` before invocation; `munmap` occurs only on `Drop`.
    unsafe impl Send for NceCodeAllocator {}
    unsafe impl Sync for NceCodeAllocator {}

    impl NceCodeAllocator {
        pub fn new() -> Self {
            Self {
                #[cfg(target_os = "macos")]
                slabs: Vec::new(),
                #[cfg(all(unix, not(target_os = "macos")))]
                pages: Vec::new(),
            }
        }

        pub fn clear(&mut self) {
            #[cfg(target_os = "macos")]
            self.slabs.clear();
            #[cfg(all(unix, not(target_os = "macos")))]
            self.pages.clear();
        }

        #[cfg(target_os = "macos")]
        pub fn emit(&mut self, words: &[u32]) -> Option<extern "C" fn(*mut NceState) -> u64> {
            unsafe extern "C" {
                fn sys_icache_invalidate(start: *mut libc::c_void, len: usize);
            }

            if words.is_empty() {
                return None;
            }
            let byte_len = words.len() * 4;
            let aligned_len = (byte_len + 15) & !15;

            let need_new = match self.slabs.last() {
                Some(s) => s.used + aligned_len > s.cap,
                None => true,
            };
            if need_new {
                let cap = MACOS_SLAB_BYTES.max(aligned_len.next_power_of_two());
                // SAFETY: Allocates a fresh private anonymous `MAP_JIT` region on macOS AArch64.
                let raw = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        cap,
                        libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                        libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                        -1,
                        0,
                    )
                };
                if raw == libc::MAP_FAILED || raw.is_null() {
                    return None;
                }
                self.slabs.push(JitSlab {
                    ptr: raw as *mut u8,
                    cap,
                    used: 0,
                });
            }

            let slab = self.slabs.last_mut()?;
            // SAFETY: `slab.used + aligned_len <= slab.cap`. Per-thread W^X is opened only for the
            // copy, immediately re-locked, and `sys_icache_invalidate` synchronizes I$/D$ on M-series.
            let dest = unsafe { slab.ptr.add(slab.used) };
            unsafe {
                libc::pthread_jit_write_protect_np(0);
                std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, dest, byte_len);
                libc::pthread_jit_write_protect_np(1);
                sys_icache_invalidate(dest as *mut libc::c_void, byte_len);
            }
            slab.used += aligned_len;
            // SAFETY: `dest` holds a complete AArch64 AAPCS64 function matching `fn(*mut NceState) -> u64`.
            Some(unsafe { std::mem::transmute::<*mut u8, extern "C" fn(*mut NceState) -> u64>(dest) })
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        pub fn emit(&mut self, words: &[u32]) -> Option<extern "C" fn(*mut NceState) -> u64> {
            use std::arch::asm;
            if words.is_empty() {
                return None;
            }
            let byte_len = words.len() * 4;
            // SAFETY: `sysconf(_SC_PAGESIZE)` has no preconditions.
            let page_sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            let page = if page_sz > 0 { page_sz as usize } else { 4096 };
            let mapped = byte_len.div_ceil(page) * page;
            // SAFETY: Fresh RW anonymous mapping, sealed RX before execution.
            let raw = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    mapped,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                )
            };
            if raw == libc::MAP_FAILED || raw.is_null() {
                return None;
            }
            let ptr = raw as *mut u8;
            // SAFETY: Copy machine words, switch to PROT_READ | PROT_EXEC, and flush AArch64 I-cache.
            unsafe {
                std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, ptr, byte_len);
                if libc::mprotect(ptr as *mut libc::c_void, mapped, libc::PROT_READ | libc::PROT_EXEC) != 0 {
                    libc::munmap(ptr as *mut libc::c_void, mapped);
                    return None;
                }
                let start = ptr as usize;
                let end = start + byte_len;
                let ctr: u64;
                asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack));
                let dline = 4usize << ((ctr >> 16) & 0xF);
                let iline = 4usize << (ctr & 0xF);
                let mut addr = start & !(dline - 1);
                while addr < end {
                    asm!("dc cvau, {}", in(reg) addr, options(nostack));
                    addr += dline;
                }
                asm!("dsb ish", options(nostack));
                let mut addr = start & !(iline - 1);
                while addr < end {
                    asm!("ic ivau, {}", in(reg) addr, options(nostack));
                    addr += iline;
                }
                asm!("dsb ish", "isb", options(nostack));
            }
            self.pages.push(SealedPage { ptr, mapped });
            // SAFETY: `ptr` points to a sealed RX AArch64 function matching `fn(*mut NceState) -> u64`.
            Some(unsafe { std::mem::transmute::<*mut u8, extern "C" fn(*mut NceState) -> u64>(ptr) })
        }

        #[cfg(not(unix))]
        pub fn emit(&mut self, _words: &[u32]) -> Option<extern "C" fn(*mut NceState) -> u64> {
            None
        }
    }

    // --- Verified AArch64 machine-code emitters ---
    // Register convention inside an NCE block:
    //   X0..X15 : Guest X0..X15 (1:1 native!)
    //   X16,X17 : Scratch registers (never X18!)
    //   X18     : NEVER TOUCHED (Apple Silicon platform-reserved register)
    //   X19     : *mut NceState
    //   X20     : *const NceTlbEntry
    //   X21     : Guest SP
    //   X22     : Guest X30 (LR)
    //   X23..25 : Inline-TLB scratch (no NZCV clobber)
    //   X26     : NZCV transfer scratch

    #[inline]
    fn map_gpr(reg: u8, r31_is_sp: bool) -> Option<u8> {
        match reg {
            0..=15 => Some(reg),
            30 => Some(22),
            31 => Some(if r31_is_sp { 21 } else { 31 }),
            _ => None,
        }
    }

    #[inline]
    fn patch_reg(word: u32, shift: u32, host_reg: u8) -> u32 {
        (word & !(0x1F << shift)) | (((host_reg as u32) & 0x1F) << shift)
    }

    #[inline]
    fn enc_ldp_x(rt1: u8, rt2: u8, rn: u8, byte_off: u32) -> u32 {
        0xA940_0000
            | (((byte_off / 8) & 0x7F) << 15)
            | ((rt2 as u32) << 10)
            | ((rn as u32) << 5)
            | (rt1 as u32)
    }

    #[inline]
    fn enc_stp_x(rt1: u8, rt2: u8, rn: u8, byte_off: u32) -> u32 {
        0xA900_0000
            | (((byte_off / 8) & 0x7F) << 15)
            | ((rt2 as u32) << 10)
            | ((rn as u32) << 5)
            | (rt1 as u32)
    }

    #[inline]
    fn enc_ldr_x_uoff(rt: u8, rn: u8, byte_off: u32) -> u32 {
        0xF940_0000 | (((byte_off / 8) & 0xFFF) << 10) | ((rn as u32) << 5) | (rt as u32)
    }

    #[inline]
    fn enc_str_x_uoff(rt: u8, rn: u8, byte_off: u32) -> u32 {
        0xF900_0000 | (((byte_off / 8) & 0xFFF) << 10) | ((rn as u32) << 5) | (rt as u32)
    }

    #[inline]
    fn enc_mov_reg(rd: u8, rm: u8) -> u32 {
        // ORR Xd, XZR, Xm
        0xAA00_03E0 | ((rm as u32) << 16) | (rd as u32)
    }

    #[inline]
    fn emit_mov_u64(code: &mut Vec<u32>, rd: u8, val: u64) {
        if rd == 31 {
            return;
        }
        code.push(0xD280_0000 | (((val & 0xFFFF) as u32) << 5) | (rd as u32));
        for hw in 1..4u32 {
            let chunk = ((val >> (hw * 16)) & 0xFFFF) as u32;
            if chunk != 0 {
                code.push(0xF280_0000 | (hw << 21) | (chunk << 5) | (rd as u32));
            }
        }
    }

    #[inline]
    fn enc_b_cond(cond: u8, imm19_words: i32) -> u32 {
        0x5400_0000 | (((imm19_words as u32) & 0x7_FFFF) << 5) | ((cond as u32) & 0xF)
    }

    #[inline]
    fn enc_cbz_cbnz(rt: u8, is_nz: bool, is_64: bool, imm19_words: i32) -> u32 {
        (if is_64 { 1u32 << 31 } else { 0 })
            | 0x3400_0000
            | (if is_nz { 1u32 << 24 } else { 0 })
            | (((imm19_words as u32) & 0x7_FFFF) << 5)
            | (rt as u32)
    }

    #[inline]
    fn enc_tbz_tbnz(rt: u8, bit: u8, is_nz: bool, imm14_words: i32) -> u32 {
        (((bit as u32 >> 5) & 1) << 31)
            | 0x3600_0000
            | (if is_nz { 1u32 << 24 } else { 0 })
            | (((bit as u32) & 0x1F) << 19)
            | (((imm14_words as u32) & 0x3FFF) << 5)
            | (rt as u32)
    }

    enum ExitPc {
        Const(u64),
        RegX16,
    }

    struct DeferredBailout {
        cbnz_word_idx: usize,
        guest_pc: u64,
        insns_done: u64,
    }

    fn emit_prologue(code: &mut Vec<u32>) {
        // Allocate 80-byte frame (16-byte aligned) and save X29, X30, X19..X26 (never X18!)
        code.push(0xD101_43FF); // sub sp, sp, #0x50
        code.push(0xA900_7BFD); // stp x29, x30, [sp]
        code.push(0xA901_53F3); // stp x19, x20, [sp, #0x10]
        code.push(0xA902_5BF5); // stp x21, x22, [sp, #0x20]
        code.push(0xA903_63F7); // stp x23, x24, [sp, #0x30]
        code.push(0xA904_6BF9); // stp x25, x26, [sp, #0x40]
        code.push(0xAA00_03F3); // mov x19, x0  (*mut NceState)

        // Load guest X0..X15 directly into host X0..X15
        for pair in 0..8u8 {
            let r = pair * 2;
            code.push(enc_ldp_x(r, r + 1, 19, (r as u32) * 8));
        }
        // Load guest X30 (0x80 -> X22), SP (0x88 -> X21), NZCV (0x98 -> X26 -> NZCV), TLB (0xB0 -> X20)
        code.push(enc_ldr_x_uoff(22, 19, 0x80));
        code.push(enc_ldr_x_uoff(21, 19, 0x88));
        code.push(enc_ldr_x_uoff(26, 19, 0x98));
        code.push(0xD51B_421A); // msr NZCV, x26
        code.push(enc_ldr_x_uoff(20, 19, 0xB0));
    }

    fn emit_exit_stub(code: &mut Vec<u32>, next_pc: ExitPc, insns_done: u64, exit_code: u64) {
        // Capture NZCV before anything else, then persist X0..X15, X22 (X30), X21 (SP)
        code.push(0xD53B_421A); // mrs x26, NZCV
        for pair in 0..8u8 {
            let r = pair * 2;
            code.push(enc_stp_x(r, r + 1, 19, (r as u32) * 8));
        }
        code.push(enc_str_x_uoff(22, 19, 0x80));
        code.push(enc_str_x_uoff(21, 19, 0x88));
        code.push(enc_str_x_uoff(26, 19, 0x98));

        // Write next_pc (0x90), insns_executed (0xB8), and exit_code (0xC0 + return in X0)
        match next_pc {
            ExitPc::Const(pc_val) => {
                emit_mov_u64(code, 16, pc_val);
                code.push(enc_str_x_uoff(16, 19, 0x90));
            }
            ExitPc::RegX16 => {
                code.push(enc_str_x_uoff(16, 19, 0x90));
            }
        }
        emit_mov_u64(code, 17, insns_done);
        code.push(enc_str_x_uoff(17, 19, 0xB8));
        emit_mov_u64(code, 0, exit_code);
        code.push(enc_str_x_uoff(0, 19, 0xC0));

        // Restore host callee-saved registers and return X0
        code.push(0xA944_6BF9); // ldp x25, x26, [sp, #0x40]
        code.push(0xA943_63F7); // ldp x23, x24, [sp, #0x30]
        code.push(0xA942_5BF5); // ldp x21, x22, [sp, #0x20]
        code.push(0xA941_53F3); // ldp x19, x20, [sp, #0x10]
        code.push(0xA940_7BFD); // ldp x29, x30, [sp]
        code.push(0x9101_43FF); // add sp, sp, #0x50
        code.push(0xD65F_03C0); // ret
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_inline_tlb_mem(
        code: &mut Vec<u32>,
        bailouts: &mut Vec<DeferredBailout>,
        guest_pc: u64,
        insns_before: u64,
        host_rt: u8,
        host_rn: u8,
        offset: i64,
        size: u8,
        is_load: bool,
        is_signed: bool,
        sign_extend_64: bool,
    ) -> bool {
        if !matches!(size, 1 | 2 | 4 | 8) {
            return false;
        }
        if offset < -4095 || offset > 0xFFFF {
            return false;
        }

        // 1. Compute effective guest VA into X23 without touching NZCV!
        if offset == 0 {
            code.push(enc_mov_reg(23, host_rn));
        } else if offset > 0 && offset <= 4095 {
            code.push(0x9100_0000 | (((offset as u32) & 0xFFF) << 10) | ((host_rn as u32) << 5) | 23);
        } else if offset > 4095 {
            emit_mov_u64(code, 16, offset as u64);
            code.push(0x8B10_0000 | ((host_rn as u32) << 5) | 23);
        } else {
            let abs = (-offset) as u32;
            code.push(0xD100_0000 | ((abs & 0xFFF) << 10) | ((host_rn as u32) << 5) | 23);
        }

        // 2. Alignment check without touching NZCV (`AND X24, X23, #(size-1)` + `CBNZ X24, bailout`).
        let align_cbnz_idx = match size {
            8 => {
                code.push(0x9240_0AF8); // and x24, x23, #0x7
                let idx = code.len();
                code.push(0);
                Some(idx)
            }
            4 => {
                code.push(0x9240_06F8); // and x24, x23, #0x3
                let idx = code.len();
                code.push(0);
                Some(idx)
            }
            2 => {
                code.push(0x9240_02F8); // and x24, x23, #0x1
                let idx = code.len();
                code.push(0);
                Some(idx)
            }
            _ => None,
        };

        // 3. Lookup 32-byte NceTlbEntry at X20 + ((va >> 12) & 511) * 32 without touching NZCV!
        code.push(0xD34C_FEF8); // lsr x24, x23, #12
        code.push(0x9240_2319); // and x25, x24, #0x1ff
        code.push(0x8B19_1699); // add x25, x20, x25, lsl #5
        if is_load {
            code.push(0xF940_0330); // ldr x16, [x25, #0]  (read_tag)
        } else {
            code.push(0xF940_0730); // ldr x16, [x25, #8]  (write_tag)
        }
        code.push(0xF940_0B31); // ldr x17, [x25, #16] (addend)
        code.push(0xCA18_0210); // eor x16, x16, x24
        let miss_cbnz_idx = code.len();
        code.push(0); // patched to cbnz x16, bailout

        // 4. Hit! Compute host pointer X23 = X23 + X17 (`va + addend`) and issue native load/store!
        code.push(0x8B11_02F7); // add x23, x23, x17
        let mem_op = if is_load {
            if is_signed {
                match (size, sign_extend_64) {
                    (1, true) => 0x3980_02E0 | (host_rt as u32),  // ldrsb xRt, [x23]
                    (1, false) => 0x39C0_02E0 | (host_rt as u32), // ldrsb wRt, [x23]
                    (2, true) => 0x7980_02E0 | (host_rt as u32),  // ldrsh xRt, [x23]
                    (2, false) => 0x79C0_02E0 | (host_rt as u32), // ldrsh wRt, [x23]
                    (4, _) => 0xB980_02E0 | (host_rt as u32),     // ldrsw xRt, [x23]
                    _ => return false,
                }
            } else {
                match size {
                    1 => 0x3940_02E0 | (host_rt as u32), // ldrb wRt, [x23]
                    2 => 0x7940_02E0 | (host_rt as u32), // ldrh wRt, [x23]
                    4 => 0xB940_02E0 | (host_rt as u32), // ldr  wRt, [x23]
                    8 => 0xF940_02E0 | (host_rt as u32), // ldr  xRt, [x23]
                    _ => return false,
                }
            }
        } else {
            match size {
                1 => 0x3900_02E0 | (host_rt as u32), // strb wRt, [x23]
                2 => 0x7900_02E0 | (host_rt as u32), // strh wRt, [x23]
                4 => 0xB900_02E0 | (host_rt as u32), // str  wRt, [x23]
                8 => 0xF900_02E0 | (host_rt as u32), // str  xRt, [x23]
                _ => return false,
            }
        };
        code.push(mem_op);

        if let Some(aidx) = align_cbnz_idx {
            bailouts.push(DeferredBailout {
                cbnz_word_idx: aidx | (1usize << 62),
                guest_pc,
                insns_done: insns_before,
            });
        }
        bailouts.push(DeferredBailout {
            cbnz_word_idx: miss_cbnz_idx,
            guest_pc,
            insns_done: insns_before,
        });
        true
    }

    pub struct CompiledNceBlock {
        pub entry: extern "C" fn(*mut NceState) -> u64,
        pub start_pc: u64,
        pub end_pc: u64,
        pub guest_insns: usize,
        pub indirect_meta: Option<IndirectBranchMeta>,
    }

    /// Attempts to compile a guest basic block starting at `start_pc` into native AArch64 code.
    /// Returns `None` if the very first instruction is not NCE-eligible (so the caller can delegate
    /// that PC cleanly to Tier-4 / Tier-3).
    pub fn compile_block(
        alloc: &mut NceCodeAllocator,
        mmu: &Memory,
        start_pc: u64,
    ) -> Option<CompiledNceBlock> {
        let mut code = Vec::with_capacity(128);
        let mut bailouts: Vec<DeferredBailout> = Vec::new();
        emit_prologue(&mut code);

        let mut pc = start_pc;
        let mut count = 0usize;
        let mut terminated = false;
        let mut indirect_meta = None;

        while count < MAX_NCE_BLOCK_INSNS {
            let Ok(raw) = mmu.read_u32(pc) else {
                break;
            };
            let inst = decode(raw);
            let done = (count + 1) as u64;

            match inst {
                Instruction::Nop => {
                    code.push(0xD503_201F);
                }

                Instruction::MoveWide { rd, .. } => {
                    let Some(hrd) = map_gpr(rd, false) else { break };
                    code.push(patch_reg(raw, 0, hrd));
                }

                Instruction::AddSubImm { rd, rn, set_flags, .. } => {
                    let (Some(hrn), Some(hrd)) = (map_gpr(rn, true), map_gpr(rd, !set_flags)) else {
                        break;
                    };
                    let w = patch_reg(patch_reg(raw, 5, hrn), 0, hrd);
                    code.push(w);
                }

                Instruction::AddSubReg { rd, rn, rm, is_extended, set_flags, .. } => {
                    let (Some(hrn), Some(hrm), Some(hrd)) = (
                        map_gpr(rn, is_extended),
                        map_gpr(rm, false),
                        map_gpr(rd, is_extended && !set_flags),
                    ) else {
                        break;
                    };
                    let w = patch_reg(patch_reg(patch_reg(raw, 5, hrn), 16, hrm), 0, hrd);
                    code.push(w);
                }

                Instruction::LogicalImm { rd, rn, op, .. } => {
                    let rd_sp = !matches!(op, LogicalOp::Ands);
                    let (Some(hrn), Some(hrd)) = (map_gpr(rn, false), map_gpr(rd, rd_sp)) else {
                        break;
                    };
                    code.push(patch_reg(patch_reg(raw, 5, hrn), 0, hrd));
                }

                Instruction::LogicalReg { rd, rn, rm, .. }
                | Instruction::AdcSbc { rd, rn, rm, .. }
                | Instruction::Extract { rd, rn, rm, .. }
                | Instruction::ShiftReg { rd, rn, rm, .. }
                | Instruction::Divide { rd, rn, rm, .. }
                | Instruction::CondSelect { rd, rn, rm, .. }
                | Instruction::MultiplyHigh { rd, rn, rm, .. } => {
                    let (Some(hrn), Some(hrm), Some(hrd)) =
                        (map_gpr(rn, false), map_gpr(rm, false), map_gpr(rd, false))
                    else {
                        break;
                    };
                    let w = patch_reg(patch_reg(patch_reg(raw, 5, hrn), 16, hrm), 0, hrd);
                    code.push(w);
                }

                Instruction::Bitfield { rd, rn, .. } | Instruction::DataProc1 { rd, rn, .. } => {
                    let (Some(hrn), Some(hrd)) = (map_gpr(rn, false), map_gpr(rd, false)) else {
                        break;
                    };
                    code.push(patch_reg(patch_reg(raw, 5, hrn), 0, hrd));
                }

                Instruction::CondCompare { rn, rm_or_imm, is_imm, .. } => {
                    let Some(hrn) = map_gpr(rn, false) else { break };
                    let mut w = patch_reg(raw, 5, hrn);
                    if !is_imm {
                        let Some(hrm) = map_gpr(rm_or_imm, false) else { break };
                        w = patch_reg(w, 16, hrm);
                    }
                    code.push(w);
                }

                Instruction::MultiplyAddSub { rd, rn, rm, ra, .. }
                | Instruction::MultiplyLong { rd, rn, rm, ra, .. } => {
                    let (Some(hrn), Some(hrm), Some(hra), Some(hrd)) = (
                        map_gpr(rn, false),
                        map_gpr(rm, false),
                        map_gpr(ra, false),
                        map_gpr(rd, false),
                    ) else {
                        break;
                    };
                    let w = patch_reg(
                        patch_reg(patch_reg(patch_reg(raw, 5, hrn), 16, hrm), 10, hra),
                        0,
                        hrd,
                    );
                    code.push(w);
                }

                Instruction::Adr { rd, imm, is_page } => {
                    let Some(hrd) = map_gpr(rd, false) else { break };
                    let base = if is_page { pc & !0xFFF } else { pc };
                    let target = (base as i64).wrapping_add(imm) as u64;
                    emit_mov_u64(&mut code, hrd, target);
                }

                Instruction::SystemReg { rt, sysreg, is_read } => {
                    let Some(hrt) = map_gpr(rt, false) else { break };
                    match (sysreg, is_read) {
                        (0x5E82, true) => {
                            if hrt != 31 {
                                code.push(enc_ldr_x_uoff(hrt, 19, 0xA0));
                            }
                        }
                        (0x5E82, false) => {
                            code.push(enc_str_x_uoff(hrt, 19, 0xA0));
                        }
                        (0x5E83, true) => {
                            if hrt != 31 {
                                code.push(enc_ldr_x_uoff(hrt, 19, 0xA8));
                            }
                        }
                        (0x5A10, true) => {
                            if hrt != 31 {
                                code.push(0xD53B_4200 | (hrt as u32)); // mrs hrt, NZCV
                            }
                        }
                        (0x5A10, false) => {
                            code.push(0xD51B_4200 | (hrt as u32)); // msr NZCV, hrt
                        }
                        _ => break,
                    }
                }

                Instruction::LoadStoreImm {
                    rt,
                    rn,
                    offset,
                    size_bytes,
                    is_load,
                    is_signed,
                    writeback: false,
                    post_index: false,
                } => {
                    let (Some(hrn), Some(hrt)) = (map_gpr(rn, true), map_gpr(rt, false)) else {
                        break;
                    };
                    // In ARM64 LDRSB/LDRSH/LDRSW, `opc` (`raw[23:22]`) == 0b10 sign-extends to 64 bits,
                    // while `opc` == 0b11 sign-extends to 32 bits (zero-extending upper 32 bits).
                    let sign_extend_64 = ((raw >> 22) & 0x3) == 0b10;
                    let snap = code.len();
                    let bail_snap = bailouts.len();
                    if !emit_inline_tlb_mem(
                        &mut code,
                        &mut bailouts,
                        pc,
                        count as u64,
                        hrt,
                        hrn,
                        offset,
                        size_bytes,
                        is_load,
                        is_signed,
                        sign_extend_64,
                    ) {
                        code.truncate(snap);
                        bailouts.truncate(bail_snap);
                        break;
                    }
                }

                Instruction::BranchImm { offset, link } => {
                    let target = (pc as i64).wrapping_add(offset) as u64;
                    if link {
                        emit_mov_u64(&mut code, 22, pc.wrapping_add(4));
                    }
                    emit_exit_stub(
                        &mut code,
                        ExitPc::Const(target),
                        done,
                        if link { NCE_EXIT_CALL } else { NCE_EXIT_BRANCH },
                    );
                    count += 1;
                    pc = pc.wrapping_add(4);
                    terminated = true;
                    break;
                }

                Instruction::BranchCond { offset, cond } => {
                    let taken = (pc as i64).wrapping_add(offset) as u64;
                    let fallthrough = pc.wrapping_add(4);
                    let br_idx = code.len();
                    code.push(0); // patched to B.<cond> taken_stub
                    emit_exit_stub(&mut code, ExitPc::Const(fallthrough), done, NCE_EXIT_FALLTHROUGH);
                    let taken_idx = code.len();
                    let rel_words = (taken_idx as i32) - (br_idx as i32);
                    code[br_idx] = enc_b_cond(cond as u8, rel_words);
                    emit_exit_stub(&mut code, ExitPc::Const(taken), done, NCE_EXIT_BRANCH);
                    count += 1;
                    pc = pc.wrapping_add(4);
                    terminated = true;
                    break;
                }

                Instruction::CompareBranch { rt, offset, is_cbnz, is_64 } => {
                    let Some(hrt) = map_gpr(rt, false) else { break };
                    let taken = (pc as i64).wrapping_add(offset) as u64;
                    let fallthrough = pc.wrapping_add(4);
                    let br_idx = code.len();
                    code.push(0);
                    emit_exit_stub(&mut code, ExitPc::Const(fallthrough), done, NCE_EXIT_FALLTHROUGH);
                    let taken_idx = code.len();
                    let rel_words = (taken_idx as i32) - (br_idx as i32);
                    code[br_idx] = enc_cbz_cbnz(hrt, is_cbnz, is_64, rel_words);
                    emit_exit_stub(&mut code, ExitPc::Const(taken), done, NCE_EXIT_BRANCH);
                    count += 1;
                    pc = pc.wrapping_add(4);
                    terminated = true;
                    break;
                }

                Instruction::TestBranch { rt, bit, offset, is_tbnz } => {
                    let Some(hrt) = map_gpr(rt, false) else { break };
                    let taken = (pc as i64).wrapping_add(offset) as u64;
                    let fallthrough = pc.wrapping_add(4);
                    let br_idx = code.len();
                    code.push(0);
                    emit_exit_stub(&mut code, ExitPc::Const(fallthrough), done, NCE_EXIT_FALLTHROUGH);
                    let taken_idx = code.len();
                    let rel_words = (taken_idx as i32) - (br_idx as i32);
                    code[br_idx] = enc_tbz_tbnz(hrt, bit, is_tbnz, rel_words);
                    emit_exit_stub(&mut code, ExitPc::Const(taken), done, NCE_EXIT_BRANCH);
                    count += 1;
                    pc = pc.wrapping_add(4);
                    terminated = true;
                    break;
                }

                Instruction::BranchReg { rn, op } => {
                    let Some(hrn) = map_gpr(rn, false) else { break };
                    // Read target into X16 BEFORE updating X22 (LR) on `BLR X30`!
                    if hrn == 31 {
                        emit_mov_u64(&mut code, 16, 0);
                    } else {
                        code.push(enc_mov_reg(16, hrn));
                    }
                    if op == BranchRegOp::Blr {
                        emit_mov_u64(&mut code, 22, pc.wrapping_add(4));
                    }
                    emit_exit_stub(&mut code, ExitPc::RegX16, done, NCE_EXIT_INDIRECT);
                    indirect_meta = Some(IndirectBranchMeta { branch_pc: pc, rn, op });
                    count += 1;
                    pc = pc.wrapping_add(4);
                    terminated = true;
                    break;
                }

                Instruction::Svc { imm } => {
                    let code_val = NCE_EXIT_SVC | ((imm as u64) << 16);
                    emit_exit_stub(&mut code, ExitPc::Const(pc.wrapping_add(4)), done, code_val);
                    count += 1;
                    pc = pc.wrapping_add(4);
                    terminated = true;
                    break;
                }

                _ => break,
            }

            count += 1;
            pc = pc.wrapping_add(4);
        }

        if count == 0 {
            return None;
        }

        if !terminated {
            emit_exit_stub(&mut code, ExitPc::Const(pc), count as u64, NCE_EXIT_FALLTHROUGH);
        }

        // Emit cold inline-TLB miss / alignment bailout stubs at the end of the block.
        for b in bailouts {
            let is_x24 = (b.cbnz_word_idx & (1usize << 62)) != 0;
            let idx = b.cbnz_word_idx & !(1usize << 62);
            let stub_idx = code.len();
            let rel_words = (stub_idx as i32) - (idx as i32);
            let rt = if is_x24 { 24 } else { 16 };
            code[idx] = enc_cbz_cbnz(rt, true, true, rel_words);
            emit_exit_stub(&mut code, ExitPc::Const(b.guest_pc), b.insns_done, NCE_EXIT_TLB_MISS);
        }

        let entry = alloc.emit(&code)?;
        Some(CompiledNceBlock {
            entry,
            start_pc,
            end_pc: pc,
            guest_insns: count,
            indirect_meta,
        })
    }
}

// ============================================================================
// Public `NceEngine` (AArch64 native `MAP_JIT` + Tier-4 / Tier-3 fallback)
// ============================================================================

#[cfg(target_arch = "aarch64")]
enum CachedPc {
    Native(aarch64_host::CompiledNceBlock),
    Delegate,
}

pub struct NceEngine {
    /// Set on non-`aarch64` hosts where direct ARM64 NCE cannot execute natively.
    pub unavailable: Option<&'static str>,
    pub blocks_compiled: u64,
    pub nce_block_runs: u64,
    pub nce_insns: u64,
    pub fallback_insns: u64,
    pub mips_rate: f64,
    tlb: Box<[NceTlbEntry; NCE_TLB_SIZE]>,
    seen_code_gen: u64,
    seen_tlb_gen: u64,
    #[cfg(target_arch = "aarch64")]
    alloc: aarch64_host::NceCodeAllocator,
    #[cfg(target_arch = "aarch64")]
    blocks: HashMap<u64, CachedPc>,
    #[cfg(feature = "native-jit")]
    tier4_fallback: Tier4JitEngine,
    tier3_fallback: Tier3JitEngine,
}

impl Default for NceEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl NceEngine {
    pub fn new() -> Self {
        Self {
            #[cfg(target_arch = "aarch64")]
            unavailable: None,
            #[cfg(not(target_arch = "aarch64"))]
            unavailable: Some("NCE requires an aarch64 host (Apple Silicon or Linux ARM64)"),
            blocks_compiled: 0,
            nce_block_runs: 0,
            nce_insns: 0,
            fallback_insns: 0,
            mips_rate: 0.0,
            tlb: Box::new([NceTlbEntry::default(); NCE_TLB_SIZE]),
            seen_code_gen: 0,
            seen_tlb_gen: 0,
            #[cfg(target_arch = "aarch64")]
            alloc: aarch64_host::NceCodeAllocator::new(),
            #[cfg(target_arch = "aarch64")]
            blocks: HashMap::new(),
            #[cfg(feature = "native-jit")]
            tier4_fallback: Tier4JitEngine::new(),
            tier3_fallback: Tier3JitEngine::new(),
        }
    }

    fn flush_tlb(&mut self) {
        self.tlb.fill(NceTlbEntry::default());
    }

    fn sync_mmu_generations(&mut self, mmu: &Memory) {
        let cg = mmu.code_write_gen();
        if cg != self.seen_code_gen {
            self.seen_code_gen = cg;
            self.flush_tlb();
            #[cfg(target_arch = "aarch64")]
            {
                self.blocks.clear();
                self.alloc.clear();
            }
        }
        let tg = mmu.tlb_gen();
        if tg != self.seen_tlb_gen {
            self.seen_tlb_gen = tg;
            self.flush_tlb();
        }
    }

    /// Populates the NCE inline TLB slot for the memory operand of the instruction at `pc`, if any.
    fn populate_tlb_for_pc(&mut self, cpu: &Cpu, mmu: &Memory, pc: u64) {
        let Ok(raw) = mmu.read_u32(pc) else { return };
        if let Instruction::LoadStoreImm { rn, offset, size_bytes, writeback: false, post_index: false, .. } = decode(raw) {
            let base = cpu.regs.read_x_or_sp(rn);
            let va = (base as i64).wrapping_add(offset) as u64;
            if size_bytes > 1 && (va & ((size_bytes as u64) - 1)) != 0 {
                return;
            }
            let page_num = va >> 12;
            if let Some(page) = mmu.get_page(page_num) {
                let slot = (page_num & NCE_TLB_MASK) as usize;
                self.tlb[slot] = NceTlbEntry {
                    read_tag: if page.perm.read { page_num } else { u64::MAX },
                    write_tag: if page.perm.write && !page.holds_code { page_num } else { u64::MAX },
                    addend: (page.host_ptr() as u64).wrapping_sub(page_num << 12),
                    _pad: 0,
                };
            }
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    pub fn run_slice(
        &mut self,
        cpu: &mut Cpu,
        mmu: &mut Memory,
        max_instructions: usize,
    ) -> (usize, BlockExit) {
        #[cfg(feature = "native-jit")]
        {
            return self.tier4_fallback.run_slice(cpu, mmu, max_instructions);
        }
        #[cfg(not(feature = "native-jit"))]
        {
            self.tier3_fallback.run_slice(cpu, mmu, max_instructions)
        }
    }

    #[cfg(target_arch = "aarch64")]
    pub fn run_slice(
        &mut self,
        cpu: &mut Cpu,
        mmu: &mut Memory,
        max_instructions: usize,
    ) -> (usize, BlockExit) {
        if max_instructions == 0 {
            return (0, BlockExit::BudgetExceeded);
        }

        let slice_start = Instant::now();
        self.sync_mmu_generations(mmu);

        let mut total = 0usize;
        let mut state = NceState {
            tlb_ptr: self.tlb.as_ptr() as u64,
            ..NceState::default()
        };

        while total < max_instructions {
            if cpu.check_landing_pad(mmu).is_err() {
                return (total, BlockExit::Fault { pc: cpu.regs.pc });
            }

            let pc = cpu.regs.pc;
            let remaining = max_instructions - total;

            if !self.blocks.contains_key(&pc) {
                let cached = match aarch64_host::compile_block(&mut self.alloc, mmu, pc) {
                    Some(compiled) => {
                        mmu.mark_code_range(compiled.start_pc, compiled.end_pc);
                        self.sync_mmu_generations(mmu);
                        self.blocks_compiled += 1;
                        CachedPc::Native(compiled)
                    }
                    None => CachedPc::Delegate,
                };
                self.blocks.insert(pc, cached);
            }

            let entry_opt = match self.blocks.get(&pc) {
                Some(CachedPc::Native(b)) if b.guest_insns <= remaining => {
                    Some((b.entry, b.indirect_meta))
                }
                _ => None,
            };

            if let Some((entry_fn, indirect_meta)) = entry_opt {
                state.x.copy_from_slice(&cpu.regs.x[0..16]);
                state.x30 = cpu.regs.x[30];
                state.sp = cpu.regs.sp;
                state.pc = pc;
                state.nzcv = cpu.regs.pstate.to_bits() as u64;
                state.tpidr_el0 = cpu.regs.tpidr_el0;
                state.tpidrro_el0 = cpu.regs.tpidrro_el0;
                state.tlb_ptr = self.tlb.as_ptr() as u64;

                let raw_exit = entry_fn(&mut state);

                cpu.regs.x[0..16].copy_from_slice(&state.x);
                cpu.regs.x[30] = state.x30;
                cpu.regs.sp = state.sp;
                cpu.regs.pc = state.pc;
                cpu.regs.pstate.from_bits(state.nzcv as u32);
                cpu.regs.tpidr_el0 = state.tpidr_el0;
                cpu.regs.tpidrro_el0 = state.tpidrro_el0;

                let retired = state.insns_executed as usize;
                total += retired;
                self.nce_insns += retired as u64;
                self.nce_block_runs += 1;

                let kind = raw_exit & 0xFFFF;
                match kind {
                    NCE_EXIT_FALLTHROUGH | NCE_EXIT_BRANCH | NCE_EXIT_CALL => {}
                    NCE_EXIT_INDIRECT => {
                        if let Some(meta) = indirect_meta {
                            cpu.regs.pstate.btype = pauth::next_btype(
                                meta.op == BranchRegOp::Blr,
                                meta.op == BranchRegOp::Ret,
                                meta.rn,
                                mmu.is_guarded(meta.branch_pc),
                                mmu.is_guarded(state.pc),
                            );
                        }
                    }
                    NCE_EXIT_SVC => {
                        let svc_id = ((raw_exit >> 16) & 0xFFFF) as u16;
                        self.update_mips(total, slice_start);
                        return (total, BlockExit::Svc { svc_id });
                    }
                    NCE_EXIT_TLB_MISS => {
                        let miss_pc = cpu.regs.pc;
                        self.populate_tlb_for_pc(cpu, mmu, miss_pc);
                        match cpu.step(mmu) {
                            Ok(StepResult::Continue) => {
                                total += 1;
                                self.fallback_insns += 1;
                                self.sync_mmu_generations(mmu);
                            }
                            Ok(StepResult::Svc(svc_id)) => {
                                total += 1;
                                self.fallback_insns += 1;
                                self.sync_mmu_generations(mmu);
                                self.update_mips(total, slice_start);
                                return (total, BlockExit::Svc { svc_id });
                            }
                            Ok(StepResult::Halt(_)) | Err(_) => {
                                self.update_mips(total, slice_start);
                                return (total, BlockExit::Fault { pc: cpu.regs.pc });
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                let step_one = matches!(self.blocks.get(&pc), Some(CachedPc::Native(_)));
                if step_one {
                    match cpu.step(mmu) {
                        Ok(StepResult::Continue) => {
                            total += 1;
                            self.fallback_insns += 1;
                            self.sync_mmu_generations(mmu);
                        }
                        Ok(StepResult::Svc(svc_id)) => {
                            total += 1;
                            self.fallback_insns += 1;
                            self.sync_mmu_generations(mmu);
                            self.update_mips(total, slice_start);
                            return (total, BlockExit::Svc { svc_id });
                        }
                        Ok(StepResult::Halt(_)) | Err(_) => {
                            self.update_mips(total, slice_start);
                            return (total, BlockExit::Fault { pc: cpu.regs.pc });
                        }
                    }
                } else {
                    let sub_budget = remaining.min(MAX_NCE_BLOCK_INSNS);
                    #[cfg(feature = "native-jit")]
                    let (ran, exit) = if self.tier4_fallback.unavailable.is_none() {
                        self.tier4_fallback.run_slice(cpu, mmu, sub_budget)
                    } else {
                        self.tier3_fallback.run_slice(cpu, mmu, sub_budget)
                    };
                    #[cfg(not(feature = "native-jit"))]
                    let (ran, exit) = self.tier3_fallback.run_slice(cpu, mmu, sub_budget);

                    total += ran;
                    self.fallback_insns += ran as u64;
                    self.sync_mmu_generations(mmu);
                    match exit {
                        BlockExit::Svc { .. } | BlockExit::Fault { .. } => {
                            self.update_mips(total, slice_start);
                            return (total, exit);
                        }
                        _ => {
                            if ran == 0 {
                                break;
                            }
                        }
                    }
                }
            }
        }

        self.update_mips(total, slice_start);
        (total, BlockExit::BudgetExceeded)
    }

    #[inline]
    fn update_mips(&mut self, total: usize, start: Instant) {
        let secs = start.elapsed().as_secs_f64();
        if secs > 0.0 && total > 0 {
            self.mips_rate = (total as f64 / 1_000_000.0) / secs;
        }
    }
}

#[cfg(test)]
#[cfg(target_arch = "aarch64")]
mod tests {
    use super::*;
    use crate::mmu::Permission;

    fn setup_code_and_data(words: &[u32]) -> (Cpu, Cpu, Memory, Memory) {
        let mut mmu_nce = Memory::with_fastmem_capacity(64 << 20);
        let mut mmu_ref = Memory::with_fastmem_capacity(64 << 20);
        let code_base = 0x0100_0000u64;
        let data_base = 0x0200_0000u64;
        let stack_base = 0x0300_0000u64;

        for mmu in [&mut mmu_nce, &mut mmu_ref] {
            mmu.map_range(code_base, 0x4000, Permission::READ_WRITE);
            mmu.map_range(data_base, 0x4000, Permission::READ_WRITE);
            mmu.map_range(stack_base, 0x4000, Permission::READ_WRITE);
            for (i, &w) in words.iter().enumerate() {
                mmu.write_u32(code_base + (i as u64) * 4, w).unwrap();
            }
            mmu.set_range_protection(code_base, 0x4000, Permission::READ_EXECUTE, None);
        }

        let mut cpu_nce = Cpu::new();
        let mut cpu_ref = Cpu::new();
        for cpu in [&mut cpu_nce, &mut cpu_ref] {
            cpu.regs.pc = code_base;
            cpu.regs.sp = stack_base + 0x2000;
            cpu.regs.tpidr_el0 = 0xCAFE_BABE_1234_5678;
            cpu.regs.tpidrro_el0 = data_base + 0x1000;
        }
        (cpu_nce, cpu_ref, mmu_nce, mmu_ref)
    }

    #[test]
    fn test_nce_native_alu_loop_fastmem_tls_and_svc_matches_interpreter() {
        let program: &[u32] = &[
            0x9000_8002, // 0x00: adrp x2, +0x1000000  (0x0200_0000)
            0xD53B_D043, // 0x04: mrs  x3, tpidr_el0
            0xF900_0043, // 0x08: str  x3, [x2, #0]
            0xD280_0C81, // 0x0C: mov  x1, #100
            0xD280_0000, // 0x10: mov  x0, #0
            0xF900_0440, // 0x14: str  x0, [x2, #8]
            // loop (0x18):
            0xF940_0445, // 0x18: ldr  x5, [x2, #8]
            0x8B01_00A5, // 0x1C: add  x5, x5, x1
            0xF900_0445, // 0x20: str  x5, [x2, #8]
            0xF100_0421, // 0x24: subs x1, x1, #1
            0x54FF_FF81, // 0x28: b.ne loop (-16 bytes -> 0x18)
            // stack + subroutine call:
            0xD100_43FF, // 0x2C: sub  sp, sp, #16
            0xF900_07E5, // 0x30: str  x5, [sp, #8]
            0x9400_0004, // 0x34: bl   subroutine (+16 bytes -> 0x44)
            0xF940_07E6, // 0x38: ldr  x6, [sp, #8]
            0x9100_43FF, // 0x3C: add  sp, sp, #16
            0xD400_0301, // 0x40: svc  #0x18
            // subroutine (0x44):
            0x9101_54A0, // 0x44: add  x0, x5, #0x55
            0xD65F_03C0, // 0x48: ret
        ];

        let (mut cpu_nce, mut cpu_ref, mut mmu_nce, mut mmu_ref) = setup_code_and_data(program);

        let mut ref_svc = None;
        for _ in 0..2000 {
            match cpu_ref.step(&mut mmu_ref).unwrap() {
                StepResult::Continue => {}
                StepResult::Svc(imm) => {
                    ref_svc = Some(imm);
                    break;
                }
                StepResult::Halt(why) => panic!("unexpected halt: {why}"),
            }
        }
        assert_eq!(ref_svc, Some(0x18));

        let mut nce = NceEngine::new();
        assert!(nce.unavailable.is_none());
        let (_ran, exit) = nce.run_slice(&mut cpu_nce, &mut mmu_nce, 2000);
        assert_eq!(exit, BlockExit::Svc { svc_id: 0x18 });
        assert!(nce.blocks_compiled >= 3);
        assert!(
            nce.nce_insns > 400,
            "hot loop must run natively in NCE (got {})",
            nce.nce_insns
        );

        for r in 0..31 {
            assert_eq!(cpu_nce.regs.x[r], cpu_ref.regs.x[r], "mismatch in X{r}");
        }
        assert_eq!(cpu_nce.regs.sp, cpu_ref.regs.sp, "mismatch in SP");
        assert_eq!(cpu_nce.regs.pc, cpu_ref.regs.pc, "mismatch in PC");
        assert_eq!(
            cpu_nce.regs.pstate.to_bits(),
            cpu_ref.regs.pstate.to_bits(),
            "mismatch in NZCV"
        );
        assert_eq!(mmu_nce.read_u64(0x0200_0000).unwrap(), 0xCAFE_BABE_1234_5678);
        assert_eq!(mmu_nce.read_u64(0x0200_0008).unwrap(), 5050);
        assert_eq!(cpu_nce.regs.x[0], 5050 + 0x55);
        assert_eq!(cpu_nce.regs.x[6], 5050);
    }
}
