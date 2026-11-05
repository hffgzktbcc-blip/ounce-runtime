//! `AMPERE_COMPUTE_B` (`0xC7C0`) and `MAXWELL_COMPUTE_B` (`0xB1C0`) compute engine & QMD v2/v3 dispatcher.
//!
//! Method offsets and Queue Meta Data (QMD) field positions follow NVIDIA's public class headers
//! `open-gpu-doc/classes/compute/clc7c0.h`, `clc7c0qmd.h`, and `clc6c0qmd.h` (MIT licensed).
//!
//! Switch 2 (`GA10F`) games submit `SET_PROGRAM_REGION_A/B` (optional for QMD v2.2/v3 which carry full
//! 49-bit `PROGRAM_ADDRESS`), write a 256-byte QMD into mapped GPU memory, write `SEND_PCAS_A`
//! (`qmd_va >> 8`), and trigger execution via `SEND_SIGNALING_PCAS_B` (`0x02BC`) or
//! `SEND_SIGNALING_PCAS2_B` (`0x02C0`).
//!
//! Execution prefers the host Metal/Vulkan hardware compute pipeline ([`super::wgpu_backend::WgpuAmpereAccelerator`])
//! backed by [`crate::gpu::sass::ShaderDiskCache`], falling back automatically to the deterministic
//! SSA compute interpreter when no host GPU adapter is active or when a kernel spans multiple disjoint
//! global buffers.

use super::engine::GpuMemoryWriter;
#[cfg(feature = "gpu-present")]
use super::wgpu_backend::WgpuAmpereAccelerator;
use crate::gpu::sass::decode::{
    SR_CTAID_X, SR_CTAID_Y, SR_CTAID_Z, SR_TID_X, SR_TID_Y, SR_TID_Z,
};
use crate::gpu::sass::eval;
use crate::gpu::sass::ir::{self, Function, Op};
use crate::gpu::sass::{maxwell, ShaderStage as SassStage, XlateError};

/// Ampere Compute Engine class (`NVC7C0`, Switch 2 Tegra T239 `GA10F`).
pub const AMPERE_COMPUTE_B: u32 = 0xC7C0;
/// Maxwell Compute Engine class (`NVB1C0`, Switch 1 Tegra X1 `GM20B`).
pub const MAXWELL_COMPUTE_B: u32 = 0xB1C0;

// Method byte offsets (`clc7c0.h` / `clb1c0.h`).
pub const SET_SHADER_SHARED_MEMORY_WINDOW_A: u32 = 0x02A0;
pub const SET_SHADER_SHARED_MEMORY_WINDOW_B: u32 = 0x02A4;
pub const SEND_PCAS_A: u32 = 0x02B4;
pub const SEND_PCAS_B: u32 = 0x02B8;
pub const SEND_SIGNALING_PCAS_B: u32 = 0x02BC;
pub const SEND_SIGNALING_PCAS2_B: u32 = 0x02C0;

pub const SET_SHADER_LOCAL_MEMORY_A: u32 = 0x0790;
pub const SET_SHADER_LOCAL_MEMORY_B: u32 = 0x0794;
pub const SET_SHADER_LOCAL_MEMORY_WINDOW_A: u32 = 0x07B0;
pub const SET_SHADER_LOCAL_MEMORY_WINDOW_B: u32 = 0x07B4;

pub const SET_TEX_SAMPLER_POOL_A: u32 = 0x155C;
pub const SET_TEX_SAMPLER_POOL_B: u32 = 0x1560;
pub const SET_TEX_SAMPLER_POOL_C: u32 = 0x1564;
pub const SET_TEX_HEADER_POOL_A: u32 = 0x1574;
pub const SET_TEX_HEADER_POOL_B: u32 = 0x1578;
pub const SET_TEX_HEADER_POOL_C: u32 = 0x157C;

pub const SET_PROGRAM_REGION_A: u32 = 0x1608;
pub const SET_PROGRAM_REGION_B: u32 = 0x160C;

pub const QMD_BYTE_SIZE: usize = 256;
const COMPUTE_REG_WORDS: usize = 0x1800 / 4;
const COMPUTE_CODE_START_BYTES: usize = 256;
const COMPUTE_CODE_MAX_BYTES: usize = 65_536;

/// One bound compute constant buffer from the QMD (`CONSTANT_BUFFER_VALID(i)` + `CONSTANT_BUFFER_ADDR(i)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QmdConstantBuffer {
    pub slot: u8,
    pub gpu_va: u64,
    pub size_bytes: u32,
}

/// Decoded 256-byte Queue Meta Data (`QMD` v2 / v3) descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeQmd {
    pub major_version: u8,
    pub program_va: u64,
    pub grid_dim: [u32; 3],
    pub workgroup_size: [u32; 3],
    pub shared_mem_bytes: u32,
    pub cbufs: Vec<QmdConstantBuffer>,
    pub release0: Option<(u64, u64, bool)>,
}

impl ComputeQmd {
    /// Parses a 256-byte (64-dword) QMD structure from guest GPU memory.
    pub fn parse(bytes: &[u8; QMD_BYTE_SIZE], program_region_va: u64) -> Self {
        let mut w = [0u32; 64];
        for (i, slot) in w.iter_mut().enumerate() {
            *slot = u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        }

        let major_version = ((w[2] >> 24) & 0xf) as u8;
        let prog_addr_49 = (((w[49] & 0x1_ffff) as u64) << 32) | (w[48] as u64);
        let program_va = if prog_addr_49 != 0 {
            prog_addr_49
        } else {
            program_region_va.wrapping_add(w[8] as u64)
        };

        let grid_x = w[12].max(1);
        let grid_y = (w[13] & 0xffff).max(1);
        let grid_z = (w[13] >> 16).max(1);

        let shared_mem_bytes = w[18] & 0x3_ffff;
        let wg_x = (w[18] >> 16).max(1);
        let wg_y = (w[19] & 0xffff).max(1);
        let wg_z = (w[19] >> 16).max(1);

        let valid_mask = (w[20] & 0xff) as u8;
        let mut cbufs = Vec::new();
        for i in 0..8u8 {
            if (valid_mask & (1 << i)) == 0 {
                continue;
            }
            let lo = w[32 + 2 * i as usize];
            let hi = w[33 + 2 * i as usize];
            let gpu_va = (((hi & 0x1_ffff) as u64) << 32) | (lo as u64);
            let shifted_size = ((hi >> 19) & 0x1fff) * 16;
            let size_bytes = if shifted_size == 0 { 0x1000 } else { shifted_size };
            cbufs.push(QmdConstantBuffer { slot: i, gpu_va, size_bytes });
        }

        let rel0_flags = (w[29] >> 30) & 0x3;
        let release0 = if rel0_flags != 0 {
            let rel_va = (((w[29] & 0x1_ffff) as u64) << 32) | (w[28] as u64);
            let payload = ((w[31] as u64) << 32) | (w[30] as u64);
            let wide = (rel0_flags & 0x2) != 0;
            Some((rel_va, payload, wide))
        } else {
            None
        };

        Self {
            major_version: if major_version == 0 { 3 } else { major_version },
            program_va,
            grid_dim: [grid_x, grid_y, grid_z],
            workgroup_size: [wg_x, wg_y, wg_z],
            shared_mem_bytes,
            cbufs,
            release0,
        }
    }

    /// Encodes a clean QMD v03.00 256-byte descriptor for tests and HLE driver helpers.
    pub fn encode_v03(&self) -> [u8; QMD_BYTE_SIZE] {
        let mut w = [0u32; 64];
        w[2] = (self.major_version.max(3) as u32) << 24;
        w[8] = self.program_va as u32;
        w[48] = self.program_va as u32;
        w[49] = ((self.program_va >> 32) as u32) & 0x1_ffff;

        w[12] = self.grid_dim[0].max(1);
        w[13] = (self.grid_dim[1].max(1) & 0xffff) | ((self.grid_dim[2].max(1) & 0xffff) << 16);
        w[18] = (self.shared_mem_bytes & 0x3_ffff) | ((self.workgroup_size[0].max(1) & 0xffff) << 16);
        w[19] = (self.workgroup_size[1].max(1) & 0xffff) | ((self.workgroup_size[2].max(1) & 0xffff) << 16);

        let mut mask = 0u32;
        for cb in &self.cbufs {
            if cb.slot < 8 {
                mask |= 1 << cb.slot;
                let idx = 32 + 2 * cb.slot as usize;
                w[idx] = cb.gpu_va as u32;
                let size_shifted = ((cb.size_bytes.div_ceil(16)).min(0x1fff)) << 19;
                w[idx + 1] = (((cb.gpu_va >> 32) as u32) & 0x1_ffff) | size_shifted;
            }
        }
        w[20] = mask;

        if let Some((rel_va, payload, wide)) = self.release0 {
            w[28] = rel_va as u32;
            let flags = if wide { 0x3u32 } else { 0x1u32 };
            w[29] = (((rel_va >> 32) as u32) & 0x1_ffff) | (flags << 30);
            w[30] = payload as u32;
            w[31] = (payload >> 32) as u32;
        }

        let mut out = [0u8; QMD_BYTE_SIZE];
        for (i, word) in w.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

/// Snapshot of one executed compute dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeDispatchRecord {
    pub class: u32,
    pub qmd_va: u64,
    pub program_va: u64,
    pub grid_dim: [u32; 3],
    pub workgroup_size: [u32; 3],
    pub hardware_accelerated: bool,
    pub invocations: u64,
}

#[derive(Debug, Clone)]
pub struct AmpereCompute {
    regs: Vec<u32>,
    pub dispatches_completed: u64,
    pub hardware_dispatches: u64,
}

impl Default for AmpereCompute {
    fn default() -> Self {
        Self::new()
    }
}

impl AmpereCompute {
    pub fn new() -> Self {
        Self {
            regs: vec![0u32; COMPUTE_REG_WORDS],
            dispatches_completed: 0,
            hardware_dispatches: 0,
        }
    }

    fn r(&self, method: u32) -> u32 {
        self.regs.get((method / 4) as usize).copied().unwrap_or(0)
    }

    fn program_region_va(&self) -> u64 {
        (((self.r(SET_PROGRAM_REGION_A) & 0x1_ffff) as u64) << 32) | (self.r(SET_PROGRAM_REGION_B) as u64)
    }

    /// Processes a method write on a subchannel bound to `AMPERE_COMPUTE_B` (`0xC7C0`) or
    /// `MAXWELL_COMPUTE_B` (`0xB1C0`). Returns `Some(ComputeDispatchRecord)` when a QMD launches.
    pub fn method(
        &mut self,
        class: u32,
        method: u32,
        data: u32,
        #[cfg(feature = "gpu-present")] hw: Option<&mut WgpuAmpereAccelerator>,
        #[cfg(not(feature = "gpu-present"))] _hw: Option<&mut ()>,
        mem: &mut dyn GpuMemoryWriter,
    ) -> Option<ComputeDispatchRecord> {
        let idx = (method / 4) as usize;
        if idx < self.regs.len() {
            self.regs[idx] = data;
        }
        if method == SEND_SIGNALING_PCAS_B || method == SEND_SIGNALING_PCAS2_B {
            let qmd_va = (self.r(SEND_PCAS_A) as u64) << 8;
            return self.dispatch_qmd(
                class,
                qmd_va,
                #[cfg(feature = "gpu-present")]
                hw,
                mem,
            );
        }
        None
    }

    fn dispatch_qmd(
        &mut self,
        class: u32,
        qmd_va: u64,
        #[cfg(feature = "gpu-present")] hw: Option<&mut WgpuAmpereAccelerator>,
        mem: &mut dyn GpuMemoryWriter,
    ) -> Option<ComputeDispatchRecord> {
        if !mem.read(0, &mut []) {
            return None;
        }
        let mut qmd_bytes = [0u8; QMD_BYTE_SIZE];
        if !mem.read(qmd_va, &mut qmd_bytes) {
            return None;
        }
        let qmd = ComputeQmd::parse(&qmd_bytes, self.program_region_va());
        let (code, func) = load_compute_program(class, qmd.program_va, qmd.workgroup_size, mem).ok()?;

        // Stage bound constant buffers.
        let mut staged_cbufs: Vec<(u8, Vec<u8>)> = Vec::with_capacity(qmd.cbufs.len());
        for cb in &qmd.cbufs {
            let mut data = vec![0u8; cb.size_bytes as usize];
            if mem.read(cb.gpu_va, &mut data) {
                staged_cbufs.push((cb.slot, data));
            }
        }

        let mut used_hw = false;
        #[cfg(feature = "gpu-present")]
        if let Some(accel) = hw {
            if accel
                .execute_compute_qmd(class, &qmd, &code, &func, &staged_cbufs, mem)
                .unwrap_or(false)
            {
                used_hw = true;
                self.hardware_dispatches += 1;
            }
        }
        #[cfg(not(feature = "gpu-present"))]
        let _ = &code;

        if !used_hw {
            eval_compute_cpu(&qmd, &func, &staged_cbufs, mem);
        }

        // Signal QMD RELEASE0 semaphore if enabled.
        if let Some((rel_va, payload, wide)) = qmd.release0 {
            if wide {
                mem.write(rel_va, &payload.to_le_bytes());
            } else {
                mem.write(rel_va, &(payload as u32).to_le_bytes());
            }
        }

        self.dispatches_completed += 1;
        let invocations = (qmd.grid_dim[0] as u64)
            * (qmd.grid_dim[1] as u64)
            * (qmd.grid_dim[2] as u64)
            * (qmd.workgroup_size[0] as u64)
            * (qmd.workgroup_size[1] as u64)
            * (qmd.workgroup_size[2] as u64);

        Some(ComputeDispatchRecord {
            class,
            qmd_va,
            program_va: qmd.program_va,
            grid_dim: qmd.grid_dim,
            workgroup_size: qmd.workgroup_size,
            hardware_accelerated: used_hw,
            invocations,
        })
    }
}

fn load_compute_program(
    class: u32,
    program_va: u64,
    workgroup_size: [u32; 3],
    mem: &mut dyn GpuMemoryWriter,
) -> Result<(Vec<u8>, Function), &'static str> {
    let stage = SassStage::Compute { workgroup_size };
    let is_maxwell = class == MAXWELL_COMPUTE_B;
    let mut size = COMPUTE_CODE_START_BYTES;
    loop {
        let mut bytes = vec![0u8; size];
        if !mem.read(program_va, &mut bytes) {
            return Err("compute shader code is not readable");
        }
        let lowered = if is_maxwell { maxwell::lower(stage, &bytes) } else { ir::lower(stage, &bytes) };
        match lowered {
            Ok(func) => {
                // Trim trailing bytes past the decoded listing length so disk cache keys stay minimal.
                let used = if is_maxwell {
                    bytes.len()
                } else {
                    (func.listing.len() * 16).min(bytes.len()).max(16)
                };
                bytes.truncate(used);
                return Ok((bytes, func));
            }
            Err(XlateError::MissingExit) if size < COMPUTE_CODE_MAX_BYTES => size *= 2,
            Err(_) => return Err("compute shader uses unsupported SASS"),
        }
    }
}

/// Discovers a single mapped global memory region referenced by pointers stored in the QMD's
/// constant buffers so the `wgpu` storage buffer binding can stage and write back the exact region.
pub(crate) fn discover_compute_storage_region(
    func: &Function,
    staged_cbufs: &[(u8, Vec<u8>)],
    mem: &dyn GpuMemoryWriter,
) -> Option<(u64, usize)> {
    let uses_gmem = !func.global_stores.is_empty()
        || func.values.iter().any(|op| matches!(op, Op::GlobalLoad { .. }));
    if !uses_gmem {
        return None;
    }

    let cbuf_word = |bank: u8, off: u16| -> Option<u32> {
        let (_, bytes) = staged_cbufs.iter().find(|(b, _)| *b == bank)?;
        let o = off as usize;
        let s = bytes.get(o..o + 4)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };

    // Check every Cbuf word actually read by the compute shader first (both 64-bit aligned pairs and 32-bit VAs).
    let mut candidates: Vec<u64> = Vec::new();
    for op in &func.values {
        if let Op::Cbuf { bank, offset } = *op {
            if let Some(lo) = cbuf_word(bank, offset) {
                if offset % 8 == 0 {
                    if let Some(hi) = cbuf_word(bank, offset + 4) {
                        let va64 = ((hi as u64) << 32) | (lo as u64);
                        if va64 != 0 {
                            candidates.push(va64);
                        }
                    }
                }
                if lo != 0 {
                    candidates.push(lo as u64);
                }
            }
        }
    }

    let mut found: Option<(u64, usize)> = None;
    for cand in candidates {
        if let Some(region) = mem.mapped_region(cand) {
            match found {
                None => found = Some(region),
                Some(prev) if prev == region => {}
                // Kernel touches multiple distinct buffers -> use CPU fallback so both are addressed directly.
                Some(_) => return None,
            }
        }
    }
    found
}

/// Deterministic SSA compute fallback: executes every thread `(wgid, lid)` against `GpuMemoryWriter`.
fn eval_compute_cpu(
    qmd: &ComputeQmd,
    func: &Function,
    staged_cbufs: &[(u8, Vec<u8>)],
    mem: &mut dyn GpuMemoryWriter,
) {
    let cbuf_word = |bank: u8, off: u16| -> u32 {
        let Some((_, bytes)) = staged_cbufs.iter().find(|(b, _)| *b == bank) else {
            return 0;
        };
        let o = off as usize;
        bytes
            .get(o..o + 4)
            .map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };

    // High-32-bit fallback hint if shader uses 32-bit `[Ra]` addressing into a >4GiB region.
    let hi_hint = discover_compute_storage_region(func, staged_cbufs, mem)
        .map_or(0u64, |(base, _)| base & !0xffff_ffff);

    let mut vals = vec![0u32; func.values.len()];
    for gz in 0..qmd.grid_dim[2] {
        for gy in 0..qmd.grid_dim[1] {
            for gx in 0..qmd.grid_dim[0] {
                for lz in 0..qmd.workgroup_size[2] {
                    for ly in 0..qmd.workgroup_size[1] {
                        for lx in 0..qmd.workgroup_size[0] {
                            for (i, op) in func.values.iter().enumerate() {
                                vals[i] = match *op {
                                    Op::Cbuf { bank, offset } => cbuf_word(bank, offset),
                                    Op::SpecialReg(sr) => match sr {
                                        SR_TID_X => lx,
                                        SR_TID_Y => ly,
                                        SR_TID_Z => lz,
                                        SR_CTAID_X => gx,
                                        SR_CTAID_Y => gy,
                                        SR_CTAID_Z => gz,
                                        _ => 0,
                                    },
                                    Op::GlobalLoad { addr_lo, addr_hi, offset } => {
                                        let lo = vals[addr_lo.0 as usize];
                                        let hi = vals[addr_hi.0 as usize];
                                        let base = if hi != 0 {
                                            ((hi as u64) << 32) | (lo as u64)
                                        } else {
                                            hi_hint | (lo as u64)
                                        };
                                        let va = (base as i64).wrapping_add(offset as i64) as u64;
                                        let mut word = [0u8; 4];
                                        if mem.read(va, &mut word) {
                                            u32::from_le_bytes(word)
                                        } else {
                                            0
                                        }
                                    }
                                    ref other => eval::op(other, &vals).unwrap_or(0),
                                };
                            }
                            for st in &func.global_stores {
                                if let Some(gv) = st.guard {
                                    if vals[gv.0 as usize] == 0 {
                                        continue;
                                    }
                                }
                                let lo = vals[st.addr_lo.0 as usize];
                                let hi = vals[st.addr_hi.0 as usize];
                                let base = if hi != 0 {
                                    ((hi as u64) << 32) | (lo as u64)
                                } else {
                                    hi_hint | (lo as u64)
                                };
                                let va = (base as i64).wrapping_add(st.offset as i64) as u64;
                                let word = vals[st.val.0 as usize];
                                mem.write(va, &word.to_le_bytes());
                            }
                        }
                    }
                }
            }
        }
    }
}
