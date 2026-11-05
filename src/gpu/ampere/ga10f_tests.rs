//! Phase 4 (`GA10F` / Switch 2 Tegra T239) unit & Metal/Vulkan `wgpu` verification tests:
//! 1. `0xC7B5` (`AMPERE_DMA_COPY_B`) Orin/T239 Sector Layout 1 Pitch <-> BlockLinear round-trip,
//!    component `REMAP` constant fill, and completion semaphore signaling.
//! 2. Host `wgpu` Metal/Vulkan `BLOCK_LINEAR_COMPUTE_WGSL` hardware GOB swizzler/unswizzler
//!    byte-for-byte parity with CPU `tiling::tile` / `tiling::detile` on both Layout 0 & Orin Layout 1.
//! 3. `0xC7C0` (`AMPERE_COMPUTE_B`) 256-byte QMD v03.00 SM86 compute kernel (`S2R`, `IMAD`, `ULDC`,
//!    `LEA`, `LDG.E.32`, `IADD3`, `STG.E.32`, `RELEASE0`) running on Metal/Vulkan `wgpu` and
//!    verified for exact parity with the deterministic CPU SSA compute fallback.
//! 4. `0xC797` (`AMPERE_B`) Metal/Vulkan `wgpu` 3D pipeline rendering into an Orin/T239 Layout-1
//!    `A8B8G8R8` + `Zf32` target using an SM86 Uniform-Datapath (`ULDC` + `URx`) vertex shader and
//!    an SM86 bindless `TEX.B` (`0x361`) fragment shader sampling Orin Layout-1 `BC1` & `ASTC_4x4`
//!    compressed textures via `HardwareTextureCache`.

use super::compute::{
    ComputeQmd, QmdConstantBuffer, AMPERE_COMPUTE_B, QMD_BYTE_SIZE, SEND_PCAS_A,
    SEND_SIGNALING_PCAS2_B,
};
use super::dma_copy::{
    self as dma, AMPERE_DMA_COPY_B,
};
use super::engine::{Ampere3d, Event, GpuMemoryWriter};
use super::methods as m;
use super::pushbuf::{immd_header, inc_header, non_inc_header};
#[cfg(feature = "gpu-present")]
use super::wgpu_backend::WgpuAmpereAccelerator;
use crate::gpu::sass::decode::{RZ, SR_CTAID_X, SR_TID_X};
use crate::gpu::sass::sph::{PixelImap, SphBuilder};
use crate::gpu::sass::AMPERE_SPH_SIZE;
use crate::gpu::texfmt::DXT1;
use crate::gpu::tiling::{self, Format, GobKind, Layout, SectorLayout, Surface};

const ASTC_4X4_CODE: u32 = 0x40;

#[derive(Default, Clone)]
struct TestGpuMem(Vec<(u64, Vec<u8>)>);

impl TestGpuMem {
    fn map(&mut self, va: u64, size: usize) {
        self.0.push((va, vec![0u8; size]));
    }

    fn region(&mut self, va: u64, len: usize) -> Option<&mut [u8]> {
        self.0.iter_mut().find_map(|(base, data)| {
            let off = va.checked_sub(*base)? as usize;
            data.get_mut(off..off.checked_add(len)?)
        })
    }

    fn put(&mut self, va: u64, bytes: &[u8]) {
        self.region(va, bytes.len())
            .expect("mapped region for put")
            .copy_from_slice(bytes);
    }

    fn get(&mut self, va: u64, len: usize) -> Vec<u8> {
        self.region(va, len)
            .expect("mapped region for get")
            .to_vec()
    }

    fn u32_at(&mut self, va: u64) -> u32 {
        u32::from_le_bytes(self.get(va, 4).try_into().unwrap())
    }

    fn u64_at(&mut self, va: u64) -> u64 {
        u64::from_le_bytes(self.get(va, 8).try_into().unwrap())
    }
}

impl GpuMemoryWriter for TestGpuMem {
    fn write(&mut self, gpu_va: u64, bytes: &[u8]) {
        if let Some(dst) = self.region(gpu_va, bytes.len()) {
            dst.copy_from_slice(bytes);
        }
    }

    fn read(&mut self, gpu_va: u64, buf: &mut [u8]) -> bool {
        if buf.is_empty() {
            return true;
        }
        match self.region(gpu_va, buf.len()) {
            Some(src) => {
                buf.copy_from_slice(src);
                true
            }
            None => false,
        }
    }

    fn mapped_region(&self, gpu_va: u64) -> Option<(u64, usize)> {
        self.0.iter().find_map(|(base, data)| {
            let end = base.checked_add(data.len() as u64)?;
            (*base <= gpu_va && gpu_va < end).then_some((*base, data.len()))
        })
    }
}

fn hi(va: u64) -> u32 {
    (va >> 32) as u32
}

fn lo(va: u64) -> u32 {
    va as u32
}

fn f(v: f32) -> u32 {
    v.to_bits()
}

fn words_to_bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[test]
fn dma_copy_c7b5_orin_layout1_roundtrip_remap_and_semaphore() {
    const SC_DMA: u8 = 4;
    const SRC_PITCH_VA: u64 = 0x2000_0000;
    const DST_BL_VA: u64 = 0x2001_0000;
    const READBACK_PITCH_VA: u64 = 0x2002_0000;
    const REMAP_DST_VA: u64 = 0x2003_0000;
    const SEM_VA: u64 = 0x2004_0000;

    let width = 64u32;
    let height = 16u32;
    let row_bytes = width * 4;
    let total_bytes = (row_bytes * height) as usize;

    let mut linear_in = vec![0u8; total_bytes];
    for y in 0..height {
        for x in 0..width {
            let o = ((y * width + x) * 4) as usize;
            linear_in[o] = (x * 3 + y) as u8;
            linear_in[o + 1] = (y * 15) as u8;
            linear_in[o + 2] = (x ^ (y * 7)) as u8;
            linear_in[o + 3] = 0xff;
        }
    }

    let mut mem = TestGpuMem::default();
    mem.map(SRC_PITCH_VA, total_bytes);
    mem.map(DST_BL_VA, total_bytes);
    mem.map(READBACK_PITCH_VA, total_bytes);
    mem.map(REMAP_DST_VA, 0x1000);
    mem.map(SEM_VA, 0x100);
    mem.put(SRC_PITCH_VA, &linear_in);

    let mut gpu = Ampere3d::new_orin_t239();
    let mut pb = Vec::new();

    // 1. Bind AMPERE_DMA_COPY_B (0xC7B5) on subchannel 4 and upload Pitch -> Orin Layout 1 BlockLinear.
    pb.extend([inc_header(SC_DMA, m::SET_OBJECT, 1), AMPERE_DMA_COPY_B]);
    pb.extend([
        inc_header(SC_DMA, dma::OFFSET_IN_UPPER, 8),
        hi(SRC_PITCH_VA),
        lo(SRC_PITCH_VA),
        hi(DST_BL_VA),
        lo(DST_BL_VA),
        row_bytes,
        row_bytes,
        row_bytes,
        height,
    ]);
    // Block height = 2 GOBs (`bh_log2 = 1`), depth = 1 GOB (`bd_log2 = 0`).
    pb.extend([
        inc_header(SC_DMA, dma::SET_DST_BLOCK_SIZE, 6),
        1 << 4,
        row_bytes,
        height,
        1,
        0,
        0,
    ]);
    pb.extend([
        inc_header(SC_DMA, dma::SET_SEMAPHORE_A, 3),
        hi(SEM_VA),
        lo(SEM_VA),
        0xDEAD_C7B5,
    ]);
    // xfer_type=1 (pipelined), sem_type=1 (1-word), src_pitch=1 (bit 7), dst_pitch=0 (BL), multi_line=1 (bit 9).
    let ctrl_to_bl = 1 | (1 << 3) | (1 << 7) | (1 << 9);
    pb.extend([inc_header(SC_DMA, dma::LAUNCH_DMA, 1), ctrl_to_bl]);

    gpu.submit(&pb, &mut mem).expect("0xC7B5 upload");
    assert_eq!(mem.u32_at(SEM_VA), 0xDEAD_C7B5);
    assert!(matches!(
        gpu.events(),
        [Event::DmaCopy(rec)]
            if rec.class == AMPERE_DMA_COPY_B
                && rec.sector_layout == SectorLayout::Layout1OrinT239
                && rec.bytes_written == total_bytes
    ));

    // Verify raw GOB bytes match Orin/T239 Sector Layout 1 AND differ from Tegra X1 Layout 0!
    let orin_surf = Surface::new_2d(
        width,
        height,
        Format::R8G8B8A8Unorm,
        Layout::BlockLinearOrin { height_log2: 1, depth_log2: 0, gob_kind: GobKind::Color2d },
    );
    let tx1_surf = Surface::new_2d(
        width,
        height,
        Format::R8G8B8A8Unorm,
        Layout::BlockLinear { height_log2: 1, depth_log2: 0 },
    );
    let expected_orin = tiling::tile(&orin_surf, &linear_in).unwrap();
    let expected_tx1 = tiling::tile(&tx1_surf, &linear_in).unwrap();
    let actual_raw = mem.get(DST_BL_VA, total_bytes);
    assert_eq!(actual_raw, expected_orin, "0xC7B5 must tile using Orin/T239 Sector Layout 1");
    assert_ne!(actual_raw, expected_tx1, "Orin Sector Layout 1 must differ from Tegra X1 Layout 0");

    // 2. Copy back BlockLinearOrin -> Pitch via 0xC7B5 and verify exact pixel round-trip.
    let mut pb2 = Vec::new();
    pb2.extend([
        inc_header(SC_DMA, dma::OFFSET_IN_UPPER, 8),
        hi(DST_BL_VA),
        lo(DST_BL_VA),
        hi(READBACK_PITCH_VA),
        lo(READBACK_PITCH_VA),
        row_bytes,
        row_bytes,
        row_bytes,
        height,
    ]);
    pb2.extend([
        inc_header(SC_DMA, dma::SET_SRC_BLOCK_SIZE, 6),
        1 << 4,
        row_bytes,
        height,
        1,
        0,
        0,
    ]);
    // xfer_type=1, src_pitch=0 (BL), dst_pitch=1 (bit 8), multi_line=1 (bit 9).
    let ctrl_to_pitch = 1 | (1 << 8) | (1 << 9);
    pb2.extend([inc_header(SC_DMA, dma::LAUNCH_DMA, 1), ctrl_to_pitch]);
    gpu.submit(&pb2, &mut mem).expect("0xC7B5 readback");
    assert_eq!(mem.get(READBACK_PITCH_VA, total_bytes), linear_in);

    // 3. Constant fill via REMAP_ENABLE (`dst_map[0] = 4 = CONST_A`, `comp_bytes = 4` (`3 << 16`), 1 comp).
    let mut pb3 = Vec::new();
    pb3.extend([
        inc_header(SC_DMA, dma::OFFSET_OUT_UPPER, 2),
        hi(REMAP_DST_VA),
        lo(REMAP_DST_VA),
    ]);
    pb3.extend([inc_header(SC_DMA, dma::LINE_LENGTH_IN, 2), 64, 1]);
    pb3.extend([
        inc_header(SC_DMA, dma::SET_REMAP_CONST_A, 3),
        0xCAFE_1234,
        0,
        4 | (3 << 16),
    ]);
    let ctrl_remap = 1 | (1 << 7) | (1 << 8) | (1 << 10);
    pb3.extend([inc_header(SC_DMA, dma::LAUNCH_DMA, 1), ctrl_remap]);
    gpu.submit(&pb3, &mut mem).expect("0xC7B5 remap fill");
    for i in 0..64u64 {
        assert_eq!(mem.u32_at(REMAP_DST_VA + i * 4), 0xCAFE_1234);
    }
}

#[cfg(feature = "gpu-present")]
#[test]
fn wgpu_metal_orin_layout1_gob_compute_swizzler() {
    let accel = WgpuAmpereAccelerator::try_new_in_memory();
    #[cfg(target_os = "macos")]
    assert!(accel.is_some(), "Metal wgpu adapter must initialize on macOS");
    let Some(accel) = accel else { return };

    let width = 96u32;
    let height = 24u32;
    let width_bytes = width * 4;
    let linear_len = (width_bytes * height) as usize;
    let linear: Vec<u8> = (0..linear_len)
        .map(|i| ((i.wrapping_mul(131) ^ (i >> 3)) & 0xff) as u8)
        .collect();

    for (sector_layout, layout_enum) in [
        (
            SectorLayout::Layout1OrinT239,
            Layout::BlockLinearOrin { height_log2: 2, depth_log2: 0, gob_kind: GobKind::Color2d },
        ),
        (
            SectorLayout::Layout0Tegrax1,
            Layout::BlockLinear { height_log2: 2, depth_log2: 0 },
        ),
    ] {
        let surf = Surface::new_2d(width, height, Format::R8G8B8A8Unorm, layout_enum);
        let cpu_tiled = tiling::tile(&surf, &linear).unwrap();

        let gpu_tiled = accel
            .gpu_block_linear_transform(
                false,
                &linear,
                cpu_tiled.len(),
                width_bytes,
                height,
                2,
                0,
                sector_layout,
            )
            .expect("GPU swizzle_from_linear");
        assert_eq!(
            gpu_tiled, cpu_tiled,
            "GPU compute swizzle mismatch for {sector_layout:?}"
        );

        let gpu_unswizzled = accel
            .gpu_block_linear_transform(
                true,
                &gpu_tiled,
                linear.len(),
                width_bytes,
                height,
                2,
                0,
                sector_layout,
            )
            .expect("GPU unswizzle_to_linear");
        assert_eq!(
            gpu_unswizzled, linear,
            "GPU compute unswizzle mismatch for {sector_layout:?}"
        );
    }
}

#[test]
fn compute_c7c0_qmd_v03_sm86_metal_and_cpu_parity() {
    const SC_COMP: u8 = 2;
    const PROG_VA: u64 = 0x3000_0000;
    const QMD_VA: u64 = 0x3001_0000;
    const CBUF_VA: u64 = 0x3002_0000;
    const DATA_VA: u64 = 0x3003_0000;
    const SEM_VA: u64 = 0x3004_0000;
    const RELEASE_PAYLOAD: u64 = 0xCAFE_BABE_2026_0001;

    // SM86 compute kernel (64 threads across 2 workgroups of 32):
    //   S2R   R0, SR_CTAID.X
    //   S2R   R1, SR_TID.X
    //   IMAD  R0, R0, 32, R1         ; global_id = ctaid.x * 32 + tid.x
    //   ULDC  UR0, c[0][0x00]        ; base GPU VA (low 32 bits; R3 = 0 for 64-bit [R2, R3])
    //   LEA   R2, R0, UR0, shift=2   ; elem_va = UR0 + (global_id << 2)
    //   LDG.E R4, [R2 + 0]
    //   IADD3 R4, R4, 100, RZ
    //   STG.E [R2 + 0], R4
    //   EXIT
    let op = |full: u16| (full as u128) | (7 << 12);
    let s2r_ctaid = op(0x919) | (0u128 << 16) | ((SR_CTAID_X as u128) << 72);
    let s2r_tid = op(0x919) | (1u128 << 16) | ((SR_TID_X as u128) << 72);
    let imad_gid = op(0x824) | (0u128 << 16) | (0u128 << 24) | (32u128 << 32) | (1u128 << 64);
    let uldc_base = op(0xab9) | (0u128 << 16) | (0u128 << 38);
    let lea_elem = op(0xc11) | (2u128 << 16) | (0u128 << 24) | (0u128 << 32) | (2u128 << 75);
    let ldg_val = op(0x381) | (4u128 << 16) | (2u128 << 24);
    let iadd_100 = op(0x810)
        | (4u128 << 16)
        | (4u128 << 24)
        | (100u128 << 32)
        | ((RZ as u128) << 64)
        | (7u128 << 81)
        | (7u128 << 84);
    let stg_val = op(0x386) | (2u128 << 24) | (4u128 << 32);
    let exit_inst = op(0x94d);

    let sass_words = [
        s2r_ctaid, s2r_tid, imad_gid, uldc_base, lea_elem, ldg_val, iadd_100, stg_val, exit_inst,
    ];
    let mut prog_bytes = vec![0u8; 512];
    for (i, w) in sass_words.iter().enumerate() {
        prog_bytes[i * 16..(i + 1) * 16].copy_from_slice(&w.to_le_bytes());
    }

    let qmd = ComputeQmd {
        major_version: 3,
        program_va: PROG_VA,
        grid_dim: [2, 1, 1],
        workgroup_size: [32, 1, 1],
        shared_mem_bytes: 0,
        cbufs: vec![QmdConstantBuffer { slot: 0, gpu_va: CBUF_VA, size_bytes: 256 }],
        release0: Some((SEM_VA, RELEASE_PAYLOAD, true)),
    };
    let qmd_bytes = qmd.encode_v03();
    assert_eq!(qmd_bytes.len(), QMD_BYTE_SIZE);

    let initial_words: Vec<u32> = (0..64u32).map(|i| i * 7 + 3).collect();
    let initial_bytes = words_to_bytes(&initial_words);

    let build_mem = || {
        let mut m = TestGpuMem::default();
        m.map(PROG_VA, prog_bytes.len());
        m.map(QMD_VA, 0x1000);
        m.map(CBUF_VA, 0x100);
        m.map(DATA_VA, initial_bytes.len());
        m.map(SEM_VA, 0x100);
        m.put(PROG_VA, &prog_bytes);
        m.put(QMD_VA, &qmd_bytes);
        m.put(CBUF_VA, &(DATA_VA as u32).to_le_bytes());
        m.put(DATA_VA, &initial_bytes);
        m
    };

    let mut pb = Vec::new();
    pb.extend([inc_header(SC_COMP, m::SET_OBJECT, 1), AMPERE_COMPUTE_B]);
    pb.extend([inc_header(SC_COMP, SEND_PCAS_A, 1), (QMD_VA >> 8) as u32]);
    pb.extend([inc_header(SC_COMP, SEND_SIGNALING_PCAS2_B, 1), 1]);

    // 1. Deterministic CPU SSA compute path.
    let mut cpu_mem = build_mem();
    let mut cpu_gpu = Ampere3d::new_orin_t239();
    cpu_gpu.submit(&pb, &mut cpu_mem).expect("CPU QMD submit");
    assert!(matches!(
        cpu_gpu.events(),
        [Event::Compute(rec)] if !rec.hardware_accelerated && rec.invocations == 64
    ));
    assert_eq!(cpu_mem.u64_at(SEM_VA), RELEASE_PAYLOAD);
    for (i, &orig) in initial_words.iter().enumerate() {
        assert_eq!(cpu_mem.u32_at(DATA_VA + (i as u64) * 4), orig + 100);
    }

    // 2. Hardware Metal/Vulkan `wgpu` compute path (and parity check against CPU path).
    #[cfg(feature = "gpu-present")]
    {
        let accel = WgpuAmpereAccelerator::try_new_in_memory();
        #[cfg(target_os = "macos")]
        assert!(accel.is_some(), "Metal wgpu adapter must initialize on macOS");
        if let Some(accel) = accel {
            let mut hw_mem = build_mem();
            let mut hw_gpu = Ampere3d::new_orin_t239();
            hw_gpu.set_wgpu_accelerator(Some(accel));
            hw_gpu.submit(&pb, &mut hw_mem).expect("Metal QMD submit");
            assert!(
                matches!(
                    hw_gpu.events(),
                    [Event::Compute(rec)] if rec.hardware_accelerated && rec.invocations == 64
                ),
                "expected hardware_accelerated == true, got {:?}",
                hw_gpu.events()
            );
            assert_eq!(hw_gpu.compute.hardware_dispatches, 1);
            assert_eq!(hw_mem.u64_at(SEM_VA), RELEASE_PAYLOAD);
            assert_eq!(
                hw_mem.get(DATA_VA, initial_bytes.len()),
                cpu_mem.get(DATA_VA, initial_bytes.len()),
                "Metal wgpu compute output must match CPU SSA output byte-for-byte"
            );
        }
    }
}

#[cfg(feature = "gpu-present")]
#[test]
fn ampere_c797_metal_3d_orin_layout1_bindless_bc1_and_astc_draw() {
    let accel = WgpuAmpereAccelerator::try_new_in_memory();
    #[cfg(target_os = "macos")]
    assert!(accel.is_some(), "Metal wgpu adapter must initialize on macOS");
    let Some(accel) = accel else { return };

    const SC: u8 = 0;
    const W: u32 = 64;
    const RT_BYTES: usize = (W * W * 4) as usize;
    const RT_VA: u64 = 0x4000_0000;
    const ZT_VA: u64 = 0x4002_0000;
    const VB_VA: u64 = 0x4004_0000;
    const VS_VA: u64 = 0x4005_0000;
    const PS_VA: u64 = 0x4005_4000;
    const VS_CB_VA: u64 = 0x4006_0000;
    const PS_CB_VA: u64 = 0x4006_1000;
    const BC1_TEX_VA: u64 = 0x4007_0000;
    const ASTC_TEX_VA: u64 = 0x4008_0000;
    const TIC_VA: u64 = 0x4009_0000;
    const TSC_VA: u64 = 0x4009_1000;

    let mut mem = TestGpuMem::default();
    mem.map(RT_VA, RT_BYTES);
    mem.map(ZT_VA, RT_BYTES);
    mem.map(VB_VA, 0x1000);
    mem.map(VS_VA, 0x8000);
    mem.map(VS_CB_VA, 0x2000);
    mem.map(BC1_TEX_VA, 0x1000);
    mem.map(ASTC_TEX_VA, 0x1000);
    mem.map(TIC_VA, 0x2000);

    // 1. Build an 8x8 BC1 (`DXT1`) pure green (`0x07e0`) texture tiled in Orin/T239 Sector Layout 1!
    let bc1_block = [0xe0u8, 0x07, 0xe0, 0x07, 0x00, 0x00, 0x00, 0x00];
    let bc1_linear: Vec<u8> = (0..4).flat_map(|_| bc1_block).collect();
    let bc1_surf = Surface::new_2d(
        8,
        8,
        Format::Bc1RgbaUnorm,
        Layout::BlockLinearOrin { height_log2: 0, depth_log2: 0, gob_kind: GobKind::Color2d },
    );
    let bc1_tiled = tiling::tile(&bc1_surf, &bc1_linear).unwrap();
    mem.put(BC1_TEX_VA, &bc1_tiled);

    // 2. Build an 8x8 ASTC_4x4 LDR void-extent pure blue (`R=0, G=0, B=0xFFFF, A=0xFFFF`) texture
    //    tiled in Orin/T239 Sector Layout 1 (`Bc7Unorm` shares the exact 4x4 texel / 16-byte block geometry)!
    let astc_void_blue: [u8; 16] = [
        0xFC, 0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // LDR void-extent header + all-1 coords
        0x00, 0x00, // R = 0
        0x00, 0x00, // G = 0
        0xFF, 0xFF, // B = 65535 -> 1.0
        0xFF, 0xFF, // A = 65535 -> 1.0
    ];
    let astc_linear: Vec<u8> = (0..4).flat_map(|_| astc_void_blue).collect();
    let astc_surf = Surface::new_2d(
        8,
        8,
        Format::Bc7Unorm,
        Layout::BlockLinearOrin { height_log2: 0, depth_log2: 0, gob_kind: GobKind::Color2d },
    );
    let astc_tiled = tiling::tile(&astc_surf, &astc_linear).unwrap();
    mem.put(ASTC_TEX_VA, &astc_tiled);

    // TIC[1] = BC1 8x8 BlockLinear (`bh_log2 = 0`), TIC[2] = ASTC_4x4 8x8 BlockLinear (`bh_log2 = 0`).
    let make_tic = |fmt_code: u32, va: u64, w_px: u32, h_px: u32| -> [u32; 8] {
        let unorm = 2u32;
        let mut t = [0u32; 8];
        t[0] = fmt_code
            | (unorm << 7)
            | (unorm << 10)
            | (unorm << 13)
            | (unorm << 16)
            | (2 << 19)
            | (3 << 22)
            | (4 << 25)
            | (5 << 28);
        t[1] = va as u32;
        t[2] = ((va >> 32) as u32) | (3 << 21); // HeaderVersion::BlockLinear
        t[3] = 0; // bh_log2 = 0, bd_log2 = 0
        t[4] = (w_px - 1) | (1 << 23); // TwoD
        t[5] = (h_px - 1) | (1 << 31); // normalized_coords = true
        t
    };
    mem.put(TIC_VA + 1 * 32, &words_to_bytes(&make_tic(DXT1, BC1_TEX_VA, 8, 8)));
    mem.put(TIC_VA + 2 * 32, &words_to_bytes(&make_tic(ASTC_4X4_CODE, ASTC_TEX_VA, 8, 8)));

    // TSC[0] = ClampToEdge + Point filter.
    let mut tsc = [0u32; 8];
    tsc[0] = 2 | (2 << 3) | (2 << 6);
    tsc[1] = 1 | (1 << 4);
    mem.put(TSC_VA, &words_to_bytes(&tsc));

    // 3. SM86 Vertex Shader using Uniform Datapath (`ULDC UR2, c[1][0]` = scale 1.0, `FFMA R0, R0, UR2, RZ`).
    let op = |full: u16| (full as u128) | (7 << 12);
    let ald = |d: u8, addr: u16, comps: u8| {
        op(0x321)
            | ((d as u128) << 16)
            | ((RZ as u128) << 24)
            | ((RZ as u128) << 32)
            | ((addr as u128) << 40)
            | (((comps - 1) as u128) << 74)
    };
    let ast = |s: u8, addr: u16, comps: u8| {
        op(0x322)
            | ((RZ as u128) << 24)
            | ((s as u128) << 32)
            | ((addr as u128) << 40)
            | ((RZ as u128) << 64)
            | (((comps - 1) as u128) << 74)
    };
    let uldc_ur2 = op(0xab9) | (2u128 << 16) | (0u128 << 38) | (1u128 << 54);
    let ffma_r0_ur2 = op(0xc23) | (0u128 << 16) | (0u128 << 24) | (2u128 << 32) | ((RZ as u128) << 64);

    let mut vs_h = SphBuilder::vertex();
    for a in (0x80..0xa0).step_by(4) {
        vs_h = vs_h.imap(a);
    }
    for a in (0x70..0x90).step_by(4) {
        vs_h = vs_h.omap(a);
    }
    let vs_words = [
        ald(0, 0x80, 4),
        ald(4, 0x90, 4),
        uldc_ur2,
        ffma_r0_ur2,
        ast(0, 0x70, 4),
        ast(4, 0x80, 4),
        op(0x94d),
    ];
    let mut vs_bytes = vs_h.bytes(AMPERE_SPH_SIZE);
    for w in vs_words {
        vs_bytes.extend_from_slice(&w.to_le_bytes());
    }
    mem.put(VS_VA, &vs_bytes);

    // 4. SM86 Fragment Shader using bindless `LDC R28, c[1][0]` + `TEX.B.2D {R0..R3}, R4, R5, handle=R28`.
    let ipa = |d: u8, addr: u16, freq: u8, mul: u8| {
        op(0x326)
            | ((d as u128) << 16)
            | ((RZ as u128) << 24)
            | ((mul as u128) << 32)
            | (((addr >> 2) as u128) << 64)
            | ((freq as u128) << 78)
    };
    let mufu_rcp = |d: u8, s: u8| op(0x308) | ((d as u128) << 16) | ((s as u128) << 32) | (4 << 74);
    let ldc_r28 = op(0xb82)
        | (28u128 << 16)
        | ((RZ as u128) << 24)
        | (0u128 << 38)
        | (1u128 << 54)
        | (4 << 73);
    let tex_b = op(0x361)
        | (0u128 << 16)
        | (4u128 << 24)
        | (28u128 << 32)
        | (1u128 << 61)
        | (2u128 << 64)
        | (0xfu128 << 72);

    let mut ps_h = SphBuilder::pixel().ps_target(0, 0xf);
    for c in 0..2 {
        ps_h = ps_h.ps_imap(0, c, PixelImap::Perspective);
    }
    let ps_words = [
        ipa(8, 0x7c, 0, RZ),
        mufu_rcp(8, 8),
        ipa(4, 0x80, 1, 8),
        ipa(5, 0x84, 1, 8),
        ldc_r28,
        tex_b,
        op(0x94d),
    ];
    let mut ps_bytes = ps_h.bytes(AMPERE_SPH_SIZE);
    for w in ps_words {
        ps_bytes.extend_from_slice(&w.to_le_bytes());
    }
    mem.put(PS_VA, &ps_bytes);

    // Fullscreen quad (2 triangles / QUADS topology = 7) with (u, v) in attribute 1.
    let quad: [[f32; 8]; 4] = [
        [-1.0, -1.0, 0.25, 1.0, 0.0, 0.0, 0.0, 1.0],
        [1.0, -1.0, 0.25, 1.0, 1.0, 0.0, 0.0, 1.0],
        [1.0, 1.0, 0.25, 1.0, 1.0, 1.0, 0.0, 1.0],
        [-1.0, 1.0, 0.25, 1.0, 0.0, 1.0, 0.0, 1.0],
    ];
    let vb_bytes: Vec<u8> = quad.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
    mem.put(VB_VA, &vb_bytes);
    mem.put(VS_CB_VA, &1.0f32.to_le_bytes());

    let mut gpu = Ampere3d::new_orin_t239();
    gpu.set_wgpu_accelerator(Some(accel));

    let mut pb = Vec::new();
    pb.extend([inc_header(SC, m::SET_OBJECT, 1), m::AMPERE_B]);
    // 64x64 A8B8G8R8 BlockLinear (`bh_log2 = 3` -> 8 GOBs tall = 64 rows), Orin Layout 1!
    pb.extend([
        inc_header(SC, m::SET_COLOR_TARGET_A, 9),
        hi(RT_VA),
        lo(RT_VA),
        W,
        W,
        0xd5,
        3 << 4,
        1,
        0,
        0,
    ]);
    pb.push(immd_header(SC, m::SET_CT_SELECT, 1));
    // Zf32 depth target, cleared to 1.0, LESS depth test enabled.
    pb.extend([inc_header(SC, m::SET_ZT_A, 5), hi(ZT_VA), lo(ZT_VA), 0x0a, 3 << 4, 0]);
    pb.extend([inc_header(SC, m::SET_ZT_SIZE_A, 3), W, W, 1]);
    pb.push(immd_header(SC, m::SET_ZT_SELECT, 1));
    pb.extend([
        inc_header(SC, m::SET_VIEWPORT_SCALE_X, 6),
        f(32.0),
        f(32.0),
        f(1.0),
        f(32.0),
        f(32.0),
        f(0.0),
    ]);
    pb.extend([
        inc_header(SC, m::SET_VIEWPORT_CLIP_HORIZONTAL, 4),
        W << 16,
        W << 16,
        f(0.0),
        f(1.0),
    ]);
    pb.extend([inc_header(SC, m::SET_COLOR_CLEAR_VALUE, 4), f(0.0), f(0.0), f(0.0), f(1.0)]);
    pb.extend([inc_header(SC, m::SET_Z_CLEAR_VALUE, 1), f(1.0)]);
    pb.push(immd_header(SC, m::CLEAR_SURFACE, 0x3d));
    pb.push(immd_header(SC, m::SET_DEPTH_TEST, 1));
    pb.push(immd_header(SC, m::SET_DEPTH_WRITE, 1));
    pb.extend([inc_header(SC, m::SET_DEPTH_FUNC, 1), 0x201]); // LESS

    // Bind vertex stream, attributes, shaders, TIC/TSC pools, VS c[1], and PS c[1] = TIC 1 (BC1 green).
    pb.extend([
        inc_header(SC, m::SET_VERTEX_STREAM_A_FORMAT, 4),
        (1 << 12) | 32,
        hi(VB_VA),
        lo(VB_VA),
        0,
    ]);
    pb.extend([inc_header(SC, m::SET_VERTEX_STREAM_SIZE_A, 2), 0, vb_bytes.len() as u32]);
    let attr = |offset: u32| (7 << 27) | (0x01 << 21) | (offset << 7);
    pb.extend([inc_header(SC, m::SET_VERTEX_ATTRIBUTE_A, 2), attr(0), attr(16)]);
    for (pipe, kind, va) in [(1u32, 1u32, VS_VA), (5, 5, PS_VA)] {
        let base = pipe * m::PIPELINE_STRIDE;
        pb.extend([inc_header(SC, m::SET_PIPELINE_SHADER + base, 1), (kind << 4) | 1]);
        pb.extend([inc_header(SC, m::SET_PIPELINE_PROGRAM_ADDRESS_A + base, 2), hi(va), lo(va)]);
    }
    pb.extend([inc_header(SC, m::SET_TEX_HEADER_POOL_A, 3), hi(TIC_VA), lo(TIC_VA), 7]);
    pb.extend([inc_header(SC, m::SET_TEX_SAMPLER_POOL_A, 3), hi(TSC_VA), lo(TSC_VA), 1]);

    // VS group 0, slot 1 -> VS_CB_VA.
    pb.extend([inc_header(SC, m::SET_CONSTANT_BUFFER_SELECTOR_A, 3), 256, hi(VS_CB_VA), lo(VS_CB_VA)]);
    pb.extend([inc_header(SC, m::BIND_GROUP_CONSTANT_BUFFER, 1), (1 << 4) | 1]);

    // PS group 4, slot 1 -> PS_CB_VA holding bindless handle `1` (TIC 1 = BC1 green, TSC 0).
    mem.put(PS_CB_VA, &1u32.to_le_bytes());
    pb.extend([inc_header(SC, m::SET_CONSTANT_BUFFER_SELECTOR_A, 3), 256, hi(PS_CB_VA), lo(PS_CB_VA)]);
    pb.extend([
        inc_header(SC, m::BIND_GROUP_CONSTANT_BUFFER + 4 * m::BIND_GROUP_STRIDE, 1),
        (1 << 4) | 1,
    ]);

    // Draw twice with BC1 green to verify HardwareTextureCache upload + hit!
    pb.extend([
        inc_header(SC, m::DRAW_VERTEX_ARRAY_BEGIN_END_INSTANCE_FIRST, 1),
        4 << 16 | (7 << 28),
    ]);
    // Second draw at z = 0.20 (passes LESS depth test) via LEQUAL (`0x203`) so same z = 0.25 also passes.
    pb.extend([inc_header(SC, m::SET_DEPTH_FUNC, 1), 0x203]); // LEQUAL
    pb.extend([
        inc_header(SC, m::DRAW_VERTEX_ARRAY_BEGIN_END_INSTANCE_FIRST, 1),
        4 << 16 | (7 << 28),
    ]);

    gpu.submit(&pb, &mut mem).expect("Metal 3D BC1 draw");
    assert!(gpu.diagnostics().is_empty(), "{:?}", gpu.diagnostics());
    assert_eq!(gpu.render_stats().hardware_draws, 2);
    let tex_stats = gpu.wgpu_accelerator().unwrap().texture_cache.stats();
    assert_eq!(tex_stats.uploads, 1);
    assert_eq!(tex_stats.hits, 1);

    let rt_orin = Surface::new_2d(
        W,
        W,
        Format::R8G8B8A8Unorm,
        Layout::BlockLinearOrin { height_log2: 3, depth_log2: 0, gob_kind: GobKind::Color2d },
    );
    let detiled_bc1 = tiling::detile(&rt_orin, &mem.get(RT_VA, RT_BYTES)).unwrap();
    for (x, y) in [(0, 0), (31, 31), (63, 63), (12, 49)] {
        let o = ((y * W + x) * 4) as usize;
        assert_eq!(
            &detiled_bc1[o..o + 4],
            &[0, 255, 0, 255],
            "BC1 Orin Layout-1 pixel ({x}, {y}) mismatch"
        );
    }

    // Now switch bindless handle in PS_CB_VA to `2` (TIC 2 = ASTC_4x4 pure blue) and draw again!
    mem.put(PS_CB_VA, &2u32.to_le_bytes());
    let pb_astc = [
        inc_header(SC, m::DRAW_VERTEX_ARRAY_BEGIN_END_INSTANCE_FIRST, 1),
        (4 << 16) | (7 << 28),
    ];
    gpu.submit(&pb_astc, &mut mem).expect("Metal 3D ASTC draw");
    assert_eq!(gpu.render_stats().hardware_draws, 3);
    assert_eq!(gpu.wgpu_accelerator().unwrap().texture_cache.stats().uploads, 2);

    let detiled_astc = tiling::detile(&rt_orin, &mem.get(RT_VA, RT_BYTES)).unwrap();
    for (x, y) in [(0, 0), (31, 31), (63, 63), (45, 19)] {
        let o = ((y * W + x) * 4) as usize;
        assert_eq!(
            &detiled_astc[o..o + 4],
            &[0, 0, 255, 255],
            "ASTC_4x4 Orin Layout-1 pixel ({x}, {y}) mismatch"
        );
    }
}
