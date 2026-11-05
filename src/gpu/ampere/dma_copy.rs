//! `AMPERE_DMA_COPY_B` (`0xC7B5`) and `MAXWELL_DMA_COPY_A` (`0xB0B5`) copy engine model.
//!
//! Method offsets and bitfields come from NVIDIA's public class headers
//! `open-gpu-doc/classes/copy/clc7b5.h` (`NVC7B5_*`) and `clb0b5.h` (`NVB0B5_*`, MIT licensed).
//!
//! Switch 2 (`GA10F` / Tegra T239) games submit `0xC7B5` commands on a GPFIFO subchannel to:
//! - Upload linear staging buffers (`nvmap` CPU heap) into **Orin/T239 Sector Layout 1**
//!   BlockLinear textures and render targets (`Pitch -> BlockLinearOrin`),
//! - Read back BlockLinear surfaces to linear CPU-readable memory (`BlockLinearOrin -> Pitch`),
//! - Copy between BlockLinear surfaces (`BlockLinear <-> BlockLinear`),
//! - Fill or swizzle buffers via `REMAP_ENABLE` (`SET_REMAP_CONST_A/B`, `SET_REMAP_COMPONENTS`),
//! - Signal completion semaphores (`SET_SEMAPHORE_A/B/PAYLOAD` + `LAUNCH_DMA.SEMAPHORE_TYPE`).

use super::engine::GpuMemoryWriter;
use crate::gpu::tiling::{BlockLinear, GobKind, SectorLayout};

/// Ampere DMA Copy Engine class (`NVC7B5`, Switch 2 Tegra T239 `GA10F`).
pub const AMPERE_DMA_COPY_B: u32 = 0xC7B5;
/// Maxwell DMA Copy Engine class (`NVB0B5`, Switch 1 Tegra X1 `GM20B`).
pub const MAXWELL_DMA_COPY_A: u32 = 0xB0B5;

// Method byte offsets (`clc7b5.h` / `clb0b5.h`).
pub const SET_SEMAPHORE_A: u32 = 0x0240;
pub const SET_SEMAPHORE_B: u32 = 0x0244;
pub const SET_SEMAPHORE_PAYLOAD: u32 = 0x0248;
pub const SET_SEMAPHORE_PAYLOAD_UPPER: u32 = 0x024C;
pub const LAUNCH_DMA: u32 = 0x0300;

pub const OFFSET_IN_UPPER: u32 = 0x0400;
pub const OFFSET_IN_LOWER: u32 = 0x0404;
pub const OFFSET_OUT_UPPER: u32 = 0x0408;
pub const OFFSET_OUT_LOWER: u32 = 0x040C;
pub const PITCH_IN: u32 = 0x0410;
pub const PITCH_OUT: u32 = 0x0414;
pub const LINE_LENGTH_IN: u32 = 0x0418;
pub const LINE_COUNT: u32 = 0x041C;

pub const SET_REMAP_CONST_A: u32 = 0x0700;
pub const SET_REMAP_CONST_B: u32 = 0x0704;
pub const SET_REMAP_COMPONENTS: u32 = 0x0708;

pub const SET_SRC_BLOCK_SIZE: u32 = 0x070C;
pub const SET_SRC_WIDTH: u32 = 0x0710;
pub const SET_SRC_HEIGHT: u32 = 0x0714;
pub const SET_SRC_DEPTH: u32 = 0x0718;
pub const SET_SRC_LAYER: u32 = 0x071C;
pub const SET_SRC_ORIGIN: u32 = 0x0720;

pub const SET_DST_BLOCK_SIZE: u32 = 0x0728;
pub const SET_DST_WIDTH: u32 = 0x072C;
pub const SET_DST_HEIGHT: u32 = 0x0730;
pub const SET_DST_DEPTH: u32 = 0x0734;
pub const SET_DST_LAYER: u32 = 0x0738;
pub const SET_DST_ORIGIN: u32 = 0x073C;

// `clc7b5.h` unpacked 16/32-bit origin methods on Ampere:
pub const C7B5_SRC_ORIGIN_X: u32 = 0x0744;
pub const C7B5_SRC_ORIGIN_Y: u32 = 0x0748;
pub const C7B5_DST_ORIGIN_X: u32 = 0x074C;
pub const C7B5_DST_ORIGIN_Y: u32 = 0x0750;

const DMA_REG_WORDS: usize = 0x0800 / 4;
const MAX_DMA_BYTES: usize = 256 * 1024 * 1024; // 256 MiB safety cap per transfer

/// Snapshot of one executed `LAUNCH_DMA` transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaCopyRecord {
    pub class: u32,
    pub src_va: u64,
    pub dst_va: u64,
    pub line_length_in: u32,
    pub line_count: u32,
    pub src_pitch: bool,
    pub dst_pitch: bool,
    pub remap: bool,
    pub sector_layout: SectorLayout,
    pub bytes_written: usize,
}

#[derive(Debug, Clone)]
pub struct AmpereDmaCopy {
    regs: Vec<u32>,
    /// Optional explicit override; when `None`, `0xC7B5` uses `Layout1OrinT239` and `0xB0B5` uses `Layout0Tegrax1`.
    sector_override: Option<SectorLayout>,
    pub copies_completed: u64,
}

impl Default for AmpereDmaCopy {
    fn default() -> Self {
        Self::new()
    }
}

impl AmpereDmaCopy {
    pub fn new() -> Self {
        let mut regs = vec![0u32; DMA_REG_WORDS];
        regs[(LINE_COUNT / 4) as usize] = 1;
        regs[(SET_SRC_DEPTH / 4) as usize] = 1;
        regs[(SET_DST_DEPTH / 4) as usize] = 1;
        Self { regs, sector_override: None, copies_completed: 0 }
    }

    pub fn set_sector_override(&mut self, layout: Option<SectorLayout>) {
        self.sector_override = layout;
    }

    fn r(&self, method: u32) -> u32 {
        self.regs.get((method / 4) as usize).copied().unwrap_or(0)
    }

    fn addr49(upper: u32, lower: u32) -> u64 {
        (((upper & 0x1_ffff) as u64) << 32) | (lower as u64)
    }

    fn sector_for(&self, class: u32) -> SectorLayout {
        if let Some(ov) = self.sector_override {
            return ov;
        }
        if class == AMPERE_DMA_COPY_B {
            SectorLayout::Layout1OrinT239
        } else {
            SectorLayout::Layout0Tegrax1
        }
    }

    /// Processes a single method write on a subchannel bound to `AMPERE_DMA_COPY_B` or `MAXWELL_DMA_COPY_A`.
    /// Returns `Some(DmaCopyRecord)` when `LAUNCH_DMA` fires.
    pub fn method(
        &mut self,
        class: u32,
        method: u32,
        data: u32,
        ticks: &mut u64,
        mem: &mut dyn GpuMemoryWriter,
    ) -> Option<DmaCopyRecord> {
        let idx = (method / 4) as usize;
        if idx < self.regs.len() {
            self.regs[idx] = data;
        }
        if method == LAUNCH_DMA {
            return Some(self.launch(class, data, ticks, mem));
        }
        None
    }

    fn launch(
        &mut self,
        class: u32,
        ctrl: u32,
        ticks: &mut u64,
        mem: &mut dyn GpuMemoryWriter,
    ) -> DmaCopyRecord {
        let xfer_type = ctrl & 0x3;
        let sem_type = (ctrl >> 3) & 0x3;
        let src_pitch_layout = ((ctrl >> 7) & 1) != 0;
        let dst_pitch_layout = ((ctrl >> 8) & 1) != 0;
        let multi_line = ((ctrl >> 9) & 1) != 0;
        let remap = ((ctrl >> 10) & 1) != 0;

        let src_va = Self::addr49(self.r(OFFSET_IN_UPPER), self.r(OFFSET_IN_LOWER));
        let dst_va = Self::addr49(self.r(OFFSET_OUT_UPPER), self.r(OFFSET_OUT_LOWER));
        let line_length_in = self.r(LINE_LENGTH_IN);
        let line_count = if multi_line { self.r(LINE_COUNT).max(1) } else { 1 };
        let sector_layout = self.sector_for(class);

        let mut bytes_written = 0usize;
        if xfer_type != 0 && line_length_in > 0 && mem.read(0, &mut []) {
            bytes_written = self.transfer_data(
                src_va,
                dst_va,
                line_length_in,
                line_count,
                src_pitch_layout,
                dst_pitch_layout,
                multi_line,
                remap,
                sector_layout,
                mem,
            );
            self.copies_completed += 1;
        }

        // Release completion semaphore if requested (`1` = 1-word, `2` = 4-word with timestamp).
        if sem_type == 1 || sem_type == 2 {
            let sem_va = Self::addr49(self.r(SET_SEMAPHORE_A), self.r(SET_SEMAPHORE_B));
            let payload_lo = self.r(SET_SEMAPHORE_PAYLOAD);
            let payload_hi = self.r(SET_SEMAPHORE_PAYLOAD_UPPER);
            *ticks = ticks.wrapping_add(1);
            if sem_type == 1 {
                mem.write(sem_va, &payload_lo.to_le_bytes());
            } else {
                let mut out = [0u8; 16];
                out[0..4].copy_from_slice(&payload_lo.to_le_bytes());
                out[4..8].copy_from_slice(&payload_hi.to_le_bytes());
                out[8..16].copy_from_slice(&ticks.to_le_bytes());
                mem.write(sem_va, &out);
            }
        }

        DmaCopyRecord {
            class,
            src_va,
            dst_va,
            line_length_in,
            line_count,
            src_pitch: src_pitch_layout,
            dst_pitch: dst_pitch_layout,
            remap,
            sector_layout,
            bytes_written,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn transfer_data(
        &self,
        src_va: u64,
        dst_va: u64,
        line_length_in: u32,
        line_count: u32,
        src_pitch_layout: bool,
        dst_pitch_layout: bool,
        multi_line: bool,
        remap: bool,
        sector_layout: SectorLayout,
        mem: &mut dyn GpuMemoryWriter,
    ) -> usize {
        let remap_cfg = remap.then(|| RemapSpec::from_regs(self));
        let src_bpe = remap_cfg.map_or(1usize, |rc| rc.comp_bytes * rc.src_comps);
        let dst_bpe = remap_cfg.map_or(1usize, |rc| rc.comp_bytes * rc.dst_comps);

        // When remap is enabled, `LINE_LENGTH_IN` counts components/elements (`comp_bytes * src_comps` per unit);
        // when disabled, `LINE_LENGTH_IN` is bytes per row.
        let src_row_bytes = if remap {
            (line_length_in as usize).saturating_mul(src_bpe)
        } else {
            line_length_in as usize
        };
        let dst_row_bytes = if remap {
            (line_length_in as usize).saturating_mul(dst_bpe)
        } else {
            line_length_in as usize
        };

        let total_dst = dst_row_bytes.saturating_mul(line_count as usize);
        if total_dst == 0 || total_dst > MAX_DMA_BYTES {
            return 0;
        }

        // Fast path: 1D contiguous Pitch -> Pitch with no remap.
        if !multi_line && src_pitch_layout && dst_pitch_layout && !remap {
            let mut buf = vec![0u8; src_row_bytes];
            if !mem.read(src_va, &mut buf) {
                return 0;
            }
            mem.write(dst_va, &buf);
            return buf.len();
        }

        // Stage linear rows (`line_count * src_row_bytes`) from either Pitch or BlockLinear source.
        let mut linear_rows = vec![0u8; src_row_bytes.saturating_mul(line_count as usize)];
        let needs_src_read = remap_cfg.map_or(true, |rc| rc.reads_source());

        if needs_src_read {
            if src_pitch_layout || !multi_line {
                let pitch_in = if multi_line {
                    let p = self.r(PITCH_IN) as usize;
                    if p == 0 { src_row_bytes } else { p }
                } else {
                    src_row_bytes
                };
                for y in 0..line_count as usize {
                    let row_va = src_va + (y * pitch_in) as u64;
                    let dst_slice = &mut linear_rows[y * src_row_bytes..(y + 1) * src_row_bytes];
                    if !mem.read(row_va, dst_slice) {
                        return 0;
                    }
                }
            } else {
                let Some((bl, origin_x_bytes, origin_y, origin_z)) = self.src_block_linear(src_bpe, src_row_bytes, line_count, sector_layout) else {
                    return 0;
                };
                let surf_size = bl.size();
                if surf_size > MAX_DMA_BYTES {
                    return 0;
                }
                let mut tiled = vec![0u8; surf_size];
                if !mem.read(src_va, &mut tiled) {
                    return 0;
                }
                for y in 0..line_count {
                    let row = &mut linear_rows[y as usize * src_row_bytes..(y as usize + 1) * src_row_bytes];
                    for xb in 0..src_row_bytes {
                        let off = bl.offset(origin_x_bytes + xb as u32, origin_y + y, origin_z);
                        if let Some(&b) = tiled.get(off) {
                            row[xb] = b;
                        }
                    }
                }
            }
        }

        // Apply optional component remap / constant fill row by row.
        let out_rows = if let Some(rc) = remap_cfg {
            rc.apply(&linear_rows, line_length_in as usize, line_count as usize)
        } else {
            linear_rows
        };

        // Write linear rows out to either Pitch or BlockLinear destination.
        if dst_pitch_layout || !multi_line {
            let pitch_out = if multi_line {
                let p = self.r(PITCH_OUT) as usize;
                if p == 0 { dst_row_bytes } else { p }
            } else {
                dst_row_bytes
            };
            for y in 0..line_count as usize {
                let row_va = dst_va + (y * pitch_out) as u64;
                let src_slice = &out_rows[y * dst_row_bytes..(y + 1) * dst_row_bytes];
                mem.write(row_va, src_slice);
            }
            out_rows.len()
        } else {
            let Some((bl, origin_x_bytes, origin_y, origin_z)) = self.dst_block_linear(dst_bpe, dst_row_bytes, line_count, sector_layout) else {
                return 0;
            };
            let surf_size = bl.size();
            if surf_size > MAX_DMA_BYTES {
                return 0;
            }
            let mut tiled = vec![0u8; surf_size];
            // Read-modify-write so partial sub-rect uploads preserve surrounding GOB texels.
            let _ = mem.read(dst_va, &mut tiled);
            for y in 0..line_count {
                let row = &out_rows[y as usize * dst_row_bytes..(y as usize + 1) * dst_row_bytes];
                for (xb, &byte_val) in row.iter().enumerate() {
                    let off = bl.offset(origin_x_bytes + xb as u32, origin_y + y, origin_z);
                    if let Some(slot) = tiled.get_mut(off) {
                        *slot = byte_val;
                    }
                }
            }
            mem.write(dst_va, &tiled);
            out_rows.len()
        }
    }

    fn src_block_linear(
        &self,
        elem_bytes: usize,
        fallback_row_bytes: usize,
        fallback_rows: u32,
        sector_layout: SectorLayout,
    ) -> Option<(BlockLinear, u32, u32, u32)> {
        let bs = self.r(SET_SRC_BLOCK_SIZE);
        let bh_log2 = ((bs >> 4) & 0xf) as u8;
        let bd_log2 = ((bs >> 8) & 0xf) as u8;
        if bh_log2 > 5 || bd_log2 > 5 {
            return None;
        }
        let packed_origin = self.r(SET_SRC_ORIGIN);
        let ox_elems = if self.r(C7B5_SRC_ORIGIN_X) != 0 {
            self.r(C7B5_SRC_ORIGIN_X)
        } else {
            packed_origin & 0xffff
        };
        let oy = if self.r(C7B5_SRC_ORIGIN_Y) != 0 {
            self.r(C7B5_SRC_ORIGIN_Y)
        } else {
            packed_origin >> 16
        };
        let oz = self.r(SET_SRC_LAYER);

        // Note: drivers program `SET_SRC_WIDTH` in bytes (when bpe == 1) or elements; if width < fallback_row_bytes
        // and elem_bytes > 1, scale by `elem_bytes`.
        let raw_w = self.r(SET_SRC_WIDTH);
        let width_bytes = if raw_w == 0 {
            (ox_elems as usize * elem_bytes + fallback_row_bytes) as u32
        } else if (raw_w as usize) < fallback_row_bytes && elem_bytes > 1 {
            raw_w.saturating_mul(elem_bytes as u32)
        } else {
            raw_w
        };
        let height = self.r(SET_SRC_HEIGHT).max(oy + fallback_rows);
        let depth = self.r(SET_SRC_DEPTH).max(oz + 1);
        let bl = BlockLinear::with_options(
            width_bytes,
            height,
            depth,
            bh_log2,
            bd_log2,
            sector_layout,
            GobKind::Color2d,
        );
        Some((bl, ox_elems.saturating_mul(elem_bytes as u32), oy, oz))
    }

    fn dst_block_linear(
        &self,
        elem_bytes: usize,
        fallback_row_bytes: usize,
        fallback_rows: u32,
        sector_layout: SectorLayout,
    ) -> Option<(BlockLinear, u32, u32, u32)> {
        let bs = self.r(SET_DST_BLOCK_SIZE);
        let bh_log2 = ((bs >> 4) & 0xf) as u8;
        let bd_log2 = ((bs >> 8) & 0xf) as u8;
        if bh_log2 > 5 || bd_log2 > 5 {
            return None;
        }
        let packed_origin = self.r(SET_DST_ORIGIN);
        let ox_elems = if self.r(C7B5_DST_ORIGIN_X) != 0 {
            self.r(C7B5_DST_ORIGIN_X)
        } else {
            packed_origin & 0xffff
        };
        let oy = if self.r(C7B5_DST_ORIGIN_Y) != 0 {
            self.r(C7B5_DST_ORIGIN_Y)
        } else {
            packed_origin >> 16
        };
        let oz = self.r(SET_DST_LAYER);

        let raw_w = self.r(SET_DST_WIDTH);
        let width_bytes = if raw_w == 0 {
            (ox_elems as usize * elem_bytes + fallback_row_bytes) as u32
        } else if (raw_w as usize) < fallback_row_bytes && elem_bytes > 1 {
            raw_w.saturating_mul(elem_bytes as u32)
        } else {
            raw_w
        };
        let height = self.r(SET_DST_HEIGHT).max(oy + fallback_rows);
        let depth = self.r(SET_DST_DEPTH).max(oz + 1);
        let bl = BlockLinear::with_options(
            width_bytes,
            height,
            depth,
            bh_log2,
            bd_log2,
            sector_layout,
            GobKind::Color2d,
        );
        Some((bl, ox_elems.saturating_mul(elem_bytes as u32), oy, oz))
    }
}

#[derive(Debug, Clone, Copy)]
struct RemapSpec {
    const_a: u32,
    const_b: u32,
    dst_map: [u8; 4],
    comp_bytes: usize,
    src_comps: usize,
    dst_comps: usize,
}

impl RemapSpec {
    fn from_regs(dma: &AmpereDmaCopy) -> Self {
        let rc = dma.r(SET_REMAP_COMPONENTS);
        Self {
            const_a: dma.r(SET_REMAP_CONST_A),
            const_b: dma.r(SET_REMAP_CONST_B),
            dst_map: [
                (rc & 0x7) as u8,
                ((rc >> 4) & 0x7) as u8,
                ((rc >> 8) & 0x7) as u8,
                ((rc >> 12) & 0x7) as u8,
            ],
            comp_bytes: (((rc >> 16) & 0x3) as usize) + 1,
            src_comps: (((rc >> 20) & 0x3) as usize) + 1,
            dst_comps: (((rc >> 24) & 0x3) as usize) + 1,
        }
    }

    fn reads_source(&self) -> bool {
        self.dst_map[..self.dst_comps].iter().any(|&m| m < 4)
    }

    fn apply(&self, src: &[u8], elems_per_row: usize, rows: usize) -> Vec<u8> {
        let src_stride = self.comp_bytes * self.src_comps;
        let dst_stride = self.comp_bytes * self.dst_comps;
        let mut out = vec![0u8; elems_per_row * dst_stride * rows];
        let ca = self.const_a.to_le_bytes();
        let cb = self.const_b.to_le_bytes();

        for r in 0..rows {
            for e in 0..elems_per_row {
                let s_base = (r * elems_per_row + e) * src_stride;
                let d_base = (r * elems_per_row + e) * dst_stride;
                for c in 0..self.dst_comps {
                    let d_off = d_base + c * self.comp_bytes;
                    match self.dst_map[c] {
                        m @ 0..=3 => {
                            let s_comp = (m as usize).min(self.src_comps - 1);
                            let s_off = s_base + s_comp * self.comp_bytes;
                            if let Some(slice) = src.get(s_off..s_off + self.comp_bytes) {
                                out[d_off..d_off + self.comp_bytes].copy_from_slice(slice);
                            }
                        }
                        4 => out[d_off..d_off + self.comp_bytes].copy_from_slice(&ca[..self.comp_bytes]),
                        5 => out[d_off..d_off + self.comp_bytes].copy_from_slice(&cb[..self.comp_bytes]),
                        _ => {}
                    }
                }
            }
        }
        out
    }
}
