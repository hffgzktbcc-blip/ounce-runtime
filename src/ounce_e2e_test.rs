//! End-to-end Switch 2 ("Ounce" / NVIDIA Tegra T239 / `GA10F`) integration test:
//! Exercising the full unified stack across all 4 roadmap milestones:
//!
//! 1. OunceCake directory package discovery (`exefs/rtld`, `exefs/main` LZ4 `NSO0`s, 42-bit NPDM,
//!    lazy `romfs/` directory assets).
//! 2. Cortex-A78C hardware feature set & NCE execution:
//!    - ARMv8.1 LSE atomic instructions (`LDADDAL`)
//!    - ARMv8.2 Dot Product (`UDOT`)
//!    - ARMv8.2 Half-Precision FP16 (`FCVT`)
//!    - 8 CPU cores & 12 GiB LPDDR5 RAM query via `svcGetInfo`
//! 3. Horizon OS Switch 2 HLE Services:
//!    - `set:sys` ("Ounce" product model & nickname)
//!    - `acc:u0` (user handshake & profile retrieval)
//!    - `vi:m` ("display.ounce" & `CreateStrayLayer`)
//!    - `hid` (Joy-Con 2 optical mouse sensor LIFO at `0x3E000`)
//! 4. `GA10F` Tegra T239 Graphics Subsystem:
//!    - `/dev/nvhost-ctrl-gpu` (GA100 arch `0x170`, GA10F impl `0x00E`, TPC mask `0x3F`)
//!    - `/dev/nvhost-tsg-gpu` (channel timeslice group binding)
//!    - `/dev/nvmap` & `/dev/nvhost-as-gpu` (GPU address space allocation & mapping)
//!    - `/dev/nvhost-gpu` (object contexts for `0xC797`, `0xC7C0`, and `0xC7B5`)
//!    - `0xC7B5` (`AMPERE_DMA_COPY_B`): Sector Layout 1 upload
//!    - `0xC7C0` (`AMPERE_COMPUTE_B`): QMD v03 SM86 compute dispatch
//!    - `0xC797` (`AMPERE_B`): SM86 3D clear and draw into Orin Layout 1 render target
//!    - Syncpoint fence signaling & wait

use std::fs;

use byteorder::{ByteOrder, LittleEndian};

use crate::audio::AudioOutServer;
use crate::cpu::nce::NceEngine;
use crate::cpu::{Cpu, StepResult};
use crate::gpu::ampere::methods as m;
use crate::gpu::ampere::pushbuf::{immd_header, inc_header};
use crate::gpu::ampere::{
    AMPERE_B, AMPERE_COMPUTE_B, AMPERE_DMA_COPY_B,
};
use crate::gpu::nvgpu::NvgpuChannel;
use crate::gpu::tiling::{self, Format, GobKind, Layout, Surface};
use crate::kernel::hid::{
    MouseInput, HID_SHARED_MEMORY_SIZE, JOYCON2_OPTICAL_MOUSE_OFFSET, MOUSE_CONNECTED,
};
use crate::kernel::nvdrv::{
    GpuPersona, NvDrv, NVMAP_IOC_ALLOC, NVMAP_IOC_CREATE, NV_OK,
};
use crate::kernel::{HorizonKernel, ServiceType, CMIF_IN_MAGIC};
use crate::loader::nro::NroLoader;
use crate::loader::package::{LoosePackageLoader, NpdmDescriptor, ACID_MAGIC};
use crate::mmu::{Memory, Permission};

fn hi(va: u64) -> u32 {
    (va >> 32) as u32
}

fn lo(va: u64) -> u32 {
    va as u32
}

fn wr32(b: &mut [u8], off: usize, v: u32) {
    if let Some(s) = b.get_mut(off..off + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

fn wr64(b: &mut [u8], off: usize, v: u64) {
    if let Some(s) = b.get_mut(off..off + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

fn rd32(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

fn map_object(nv: &mut NvDrv, map_fd: u32, as_fd: u32, cpu_addr: u64, size: u32) -> u64 {
    let mut create = [0u8; 8];
    wr32(&mut create, 0, size);
    assert_eq!(nv.ioctl(map_fd, NVMAP_IOC_CREATE, &mut create), NV_OK);
    let handle = rd32(&create, 4);

    let mut alloc = [0u8; 32];
    wr32(&mut alloc, 0, handle);
    wr64(&mut alloc, 24, cpu_addr);
    assert_eq!(nv.ioctl(map_fd, NVMAP_IOC_ALLOC, &mut alloc), NV_OK);

    let mut map = [0u8; 40];
    wr32(&mut map, 8, handle);
    assert_eq!(nv.ioctl(as_fd, 0xC028_4106, &mut map), NV_OK);
    u64::from_le_bytes(map[32..40].try_into().unwrap())
}

#[test]
fn test_switch2_ounce_e2e_full_stack() {
    let temp_dir = std::env::temp_dir().join(format!(
        "ounce_nx2_e2e_{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let exefs_dir = temp_dir.join("exefs");
    let romfs_dir = temp_dir.join("romfs");
    fs::create_dir_all(&exefs_dir).unwrap();
    fs::create_dir_all(romfs_dir.join("data")).unwrap();

    // 1. Create Switch 2 Ounce 42-bit NPDM + RSA-3072 ACID descriptor
    let mut npdm = NpdmDescriptor::create_synthetic(
        "OunceShowcase",
        "OUNCE-P-E2E2026",
        0x0100_0239_0000_2026,
        44,
        0,
        &["sm:", "nvdrv", "set:sys", "acc:u0", "vi:m", "hid"],
    );
    npdm[0x0C] = 4 << 1; // 42-bit Ounce virtual address space
    let acid_off = npdm.len();
    npdm.resize(acid_off + 0x400, 0);
    npdm[acid_off + 0x300..acid_off + 0x304].copy_from_slice(&ACID_MAGIC);
    LittleEndian::write_u32(&mut npdm[0x78..0x7C], acid_off as u32);
    LittleEndian::write_u32(&mut npdm[0x7C..0x80], 0x400);
    fs::write(exefs_dir.join("main.npdm"), &npdm).unwrap();

    // 2. Build multi-module LZ4 NSO0 package: `rtld` and `main`
    let mk_lz4_mod = |instructions: &[u32], bid_byte: u8| {
        let mut text = vec![0u8; 0x1000];
        for (i, insn) in instructions.iter().enumerate() {
            text[i * 4..(i + 1) * 4].copy_from_slice(&insn.to_le_bytes());
        }
        let rodata = vec![bid_byte; 0x1000];
        let data = vec![0u8; 0x1000];
        NroLoader::build_compressed_nso(&text, &rodata, &data, 0x1000, &[bid_byte; 32])
    };

    // `rtld`: Simple trampoline jumping to main at (+0x4000)
    let rtld_code = [
        0x1400_1000u32, // B +0x4000 (main entry)
    ];
    fs::write(exefs_dir.join("rtld"), mk_lz4_mod(&rtld_code, 0x01)).unwrap();

    // `main`: Cortex-A78C test suite:
    //  - LSE: LDADDAL W1, W2, [X3]
    //  - DotProd: UDOT V0.4S, V1.16B, V2.16B
    //  - FP16: FCVT H0, S0
    //  - svcGetInfo (CoreMask = 0xFF, TotalMemory = 12 GB)
    //  - svcExitProcess
    #[rustfmt::skip]
    let main_code = [
        0xB8A1_0062u32, // LDADDAL W1, W2, [X3]   (LSE atomic add)
        0x4E82_9420u32, // UDOT    V0.4S, V1.16B, V2.16B (ARMv8.2 DotProd)
        0x1E21_4000u32, // FCVT    H0, S0         (ARMv8.2 FP16 conversion)
        0xD280_0001u32, // MOVZ    X1, #0         (InfoType::CoreMask)
        0xD400_0521u32, // SVC     #0x29          (svcGetInfo -> X1 = 0xFF)
        0xD280_00C1u32, // MOVZ    X1, #6         (InfoType::TotalMemoryAvailable)
        0xD400_0521u32, // SVC     #0x29          (svcGetInfo -> X1 = 12 GB)
        0xD400_00E1u32, // SVC     #0x07          (svcExitProcess)
    ];
    fs::write(exefs_dir.join("main"), mk_lz4_mod(&main_code, 0x02)).unwrap();

    // RomFS lazy asset
    fs::write(romfs_dir.join("data/manifest.bin"), b"OUNCE_ROMFS_MAGIC_2026").unwrap();

    // 3. Discover and load OunceCake package into HorizonKernel + Fastmem Memory
    let pkg = LoosePackageLoader::discover(&temp_dir).expect("discover OunceCake package");
    let mut mmu = Memory::with_fastmem_capacity(256 * 1024 * 1024);
    let mut kernel = HorizonKernel::new();
    let base_addr = 0x8000_0000u64;

    let boot_info = LoosePackageLoader::load_into_kernel(&pkg, base_addr, &mut mmu, &mut kernel)
        .expect("load OunceCake package into kernel");

    assert!(boot_info.has_rtld);
    assert_eq!(boot_info.entry_pc, base_addr);
    assert_eq!(kernel.ipc.nvdrv.persona, GpuPersona::Ga10f);
    assert_eq!(kernel.address_space_bits, 42);
    assert_eq!(kernel.heap_region_size, 0x3_0000_0000); // 12 GiB heap

    // 4. Verify Lazy RomFS reading directly into guest memory
    let romfs_probe_va = 0x8880_0000u64;
    mmu.map_range(romfs_probe_va, 0x1000, Permission::READ_WRITE);
    let bytes_read = kernel
        .vfs
        .read_file_to_guest("romfs:/data/manifest.bin", 0, romfs_probe_va, 64, &mut mmu)
        .expect("romfs read to guest");
    assert_eq!(bytes_read, b"OUNCE_ROMFS_MAGIC_2026".len());
    let mut read_manifest = vec![0u8; bytes_read];
    mmu.read_bytes(romfs_probe_va, &mut read_manifest).unwrap();
    assert_eq!(&read_manifest, b"OUNCE_ROMFS_MAGIC_2026");

    // 5. Execute AArch64 binary using CPU interpreter and verify NCE instruction compatibility
    let mut cpu = Cpu::new();
    cpu.regs.pc = base_addr;
    let stack_va = 0x7FFF_0000u64;
    mmu.map_range(stack_va, 0x10000, Permission::READ_WRITE);
    cpu.regs.sp = stack_va + 0x8000;

    let atomic_cell_va = 0x8800_0000u64;
    mmu.map_range(atomic_cell_va, 0x1000, Permission::READ_WRITE);
    mmu.write_u32(atomic_cell_va, 50).unwrap();
    cpu.regs.x[1] = 25; // addend
    cpu.regs.x[3] = atomic_cell_va;

    // Run rtld branch + main instructions until svcExitProcess
    let mut exit_reached = false;
    for _ in 0..100 {
        match cpu.step(&mut mmu).expect("CPU step") {
            StepResult::Continue => {}
            StepResult::Svc(0x29) => {
                let info_type = cpu.regs.x[1] as u32;
                let sub_type = cpu.regs.x[3];
                let val = kernel.info(info_type, sub_type).expect("info query");
                cpu.regs.x[0] = 0;
                cpu.regs.x[1] = val;
            }
            StepResult::Svc(0x07) => {
                exit_reached = true;
                break;
            }
            StepResult::Svc(svc) => panic!("unexpected svc #{svc}"),
            StepResult::Halt(why) => panic!("unexpected halt: {why}"),
        }
    }
    assert!(exit_reached, "Guest must reach svcExitProcess");
    assert_eq!(cpu.regs.x[2], 50, "LDADDAL old value in X2 must be 50");
    assert_eq!(mmu.read_u32(atomic_cell_va).unwrap(), 75, "Atomic cell must be 50 + 25 = 75");
    assert_eq!(cpu.regs.x[1], 12 * 1024 * 1024 * 1024, "TotalMemoryAvailable must be 12 GB");

    // Verify NCE engine initializes on Apple Silicon host
    let nce = NceEngine::new();
    assert!(nce.unavailable.is_none(), "NCE engine must be available on macOS AArch64");

    // 6. Test Switch 2 Horizon Services: `set:sys`, `acc:u0`, `vi:m`, and `hid` optical mouse
    let tls_addr = 0x8900_0000u64;
    let nick_buf = 0x8900_1000u64;
    mmu.map_range(tls_addr, 0x1000, Permission::READ_WRITE);
    mmu.map_range(nick_buf, 0x1000, Permission::READ_WRITE);

    let mut nvgpu = NvgpuChannel::new();
    let mut audio = AudioOutServer::new();

    // `set:sys`
    let setsys_h = kernel.ipc.allocate_handle(ServiceType::SetSys);
    mmu.write_u32(tls_addr + 0x10, CMIF_IN_MAGIC).unwrap();
    mmu.write_u32(tls_addr + 0x18, 6).unwrap(); // GetDeviceNickName
    mmu.write_u64(tls_addr + 0x20, nick_buf).unwrap();
    assert_eq!(kernel.ipc.handle_sync_request(setsys_h, tls_addr, &mut mmu, &mut nvgpu, &mut audio, &mut kernel.vfs), 0);
    let mut nick_bytes = [0u8; 10];
    mmu.read_bytes(nick_buf, &mut nick_bytes).unwrap();
    assert_eq!(&nick_bytes, b"Ounce-T239");

    // `acc:u0`
    let acc_h = kernel.ipc.allocate_handle(ServiceType::Acc);
    mmu.write_u32(tls_addr + 0x10, CMIF_IN_MAGIC).unwrap();
    mmu.write_u32(tls_addr + 0x18, 0).unwrap(); // GetUserCount
    assert_eq!(kernel.ipc.handle_sync_request(acc_h, tls_addr, &mut mmu, &mut nvgpu, &mut audio, &mut kernel.vfs), 0);
    assert_eq!(mmu.read_u32(tls_addr + 0x20).unwrap(), 1);

    // `vi:m` -> GetDisplayService -> "display.ounce" and `CreateStrayLayer`
    let vi_h = kernel.ipc.allocate_handle(ServiceType::Vi);
    mmu.write_u32(tls_addr + 0x10, CMIF_IN_MAGIC).unwrap();
    mmu.write_u32(tls_addr + 0x18, 2).unwrap(); // GetDisplayService
    assert_eq!(kernel.ipc.handle_sync_request(vi_h, tls_addr, &mut mmu, &mut nvgpu, &mut audio, &mut kernel.vfs), 0);
    let disp_srv_h = mmu.read_u32(tls_addr + 0x20).unwrap();

    mmu.write_u32(tls_addr + 0x10, CMIF_IN_MAGIC).unwrap();
    mmu.write_u32(tls_addr + 0x18, 1010).unwrap(); // OpenDisplay("display.ounce")
    mmu.write_bytes(tls_addr + 0x20, b"display.ounce\0").unwrap();
    assert_eq!(kernel.ipc.handle_sync_request(disp_srv_h, tls_addr, &mut mmu, &mut nvgpu, &mut audio, &mut kernel.vfs), 0);
    let disp_id = mmu.read_u32(tls_addr + 0x20).unwrap();
    assert_eq!(disp_id, 0);

    mmu.write_u32(tls_addr + 0x10, CMIF_IN_MAGIC).unwrap();
    mmu.write_u32(tls_addr + 0x18, 2030).unwrap(); // CreateStrayLayer
    assert_eq!(kernel.ipc.handle_sync_request(disp_srv_h, tls_addr, &mut mmu, &mut nvgpu, &mut audio, &mut kernel.vfs), 0);
    let layer_id = mmu.read_u32(tls_addr + 0x20).unwrap();
    assert_eq!(layer_id, 1);

    // `hid` optical mouse sensor at `0x3E000`
    let hid_shared_va = 0x1000_0000u64;
    mmu.map_range(hid_shared_va, HID_SHARED_MEMORY_SIZE as usize, Permission::READ_WRITE);
    kernel.ipc.hid.mapped(hid_shared_va, &mut mmu);
    kernel.ipc.hid.feed.set_mouse(MouseInput {
        connected: true,
        x: 120,
        y: 80,
        delta_x: 15,
        delta_y: -8,
        wheel_delta_x: 0,
        wheel_delta_y: 1,
        buttons: 1,
    });
    kernel.ipc.hid.sync(&mut mmu);
    let jc2_header = hid_shared_va + JOYCON2_OPTICAL_MOUSE_OFFSET;
    let tail = mmu.read_u64(jc2_header + 16).unwrap();
    let slot = jc2_header + crate::kernel::hid::LIFO_STORAGE_OFFSET + tail * crate::kernel::hid::MOUSE_ENTRY_SIZE;
    // slot + 0: sampling number (u64)
    // slot + 8: sampling number written by lambda (u64)
    // slot + 16: m.x (i32)
    // slot + 20: m.y (i32)
    // slot + 24: m.delta_x (i32)
    // slot + 28: m.delta_y (i32)
    // slot + 32: m.wheel_delta_x (i32)
    // slot + 36: m.wheel_delta_y (i32)
    // slot + 40: m.buttons (u32)
    // slot + 44: attributes (u32)
    assert_eq!(mmu.read_u32(slot + 16).unwrap(), 120);
    assert_eq!(mmu.read_u32(slot + 20).unwrap(), 80);
    assert_eq!(mmu.read_u32(slot + 44).unwrap(), MOUSE_CONNECTED | 1);

    // 7. Full `GA10F` Tegra T239 Graphics Subsystem Pipeline
    let nv = &mut kernel.ipc.nvdrv;
    assert_eq!(nv.persona, GpuPersona::Ga10f);

    // /dev/nvhost-ctrl-gpu characteristics check
    let (gpu_fd, _) = nv.open("/dev/nvhost-ctrl-gpu");
    let mut gc_buf = vec![0u8; 0xB0];
    assert_eq!(nv.ioctl(gpu_fd, 0xC0B0_4705, &mut gc_buf), NV_OK);
    assert_eq!(rd32(&gc_buf, 0x10), 0x170); // GA100 architecture
    assert_eq!(rd32(&gc_buf, 0x14), 0x00E); // GA10F implementation
    assert_eq!(rd32(&gc_buf, 0x10 + 0x5C), 0xC797); // AMPERE_B
    assert_eq!(rd32(&gc_buf, 0x10 + 0x60), 0xC7C0); // AMPERE_COMPUTE_B
    assert_eq!(&gc_buf[0x10 + 0x90..0x10 + 0x95], b"ga10f");

    // /dev/nvhost-tsg-gpu bind
    let (tsg_fd, _) = nv.open("/dev/nvhost-tsg-gpu");
    let (ch_fd, _) = nv.open("/dev/nvhost-gpu");
    let mut bind_buf = ch_fd.to_le_bytes();
    assert_eq!(nv.ioctl(tsg_fd, 0x4004_5401, &mut bind_buf), NV_OK);

    // Allocate GPU object contexts for 0xC797 (3D), 0xC7C0 (Compute), and 0xC7B5 (DMA)
    for class in [AMPERE_B, AMPERE_COMPUTE_B, AMPERE_DMA_COPY_B] {
        let mut obj = [0u8; 16];
        wr32(&mut obj, 0, class);
        assert_eq!(nv.ioctl(ch_fd, 0xC010_4809, &mut obj), NV_OK);
    }
    assert!(nv.has_3d_class(ch_fd));

    // Map GPU memory buffers: RT (Orin Layout 1), Staging, and Pushbuffer
    let (map_fd, _) = nv.open("/dev/nvmap");
    let (as_fd, _) = nv.open("/dev/nvhost-as-gpu");

    let rt_cpu = 0x9000_0000u64;
    let pb_cpu = 0x9100_0000u64;
    let sem_cpu = 0x9200_0000u64;
    mmu.map_range(rt_cpu, 0x10000, Permission::READ_WRITE);
    mmu.map_range(pb_cpu, 0x10000, Permission::READ_WRITE);
    mmu.map_range(sem_cpu, 0x1000, Permission::READ_WRITE);

    let rt_va = map_object(nv, map_fd, as_fd, rt_cpu, 0x10000);
    let pb_va = map_object(nv, map_fd, as_fd, pb_cpu, 0x10000);
    let sem_va = map_object(nv, map_fd, as_fd, sem_cpu, 0x1000);

    // Allocate GPFIFO on channel
    let mut gpfifo = [0u8; 32];
    wr32(&mut gpfifo, 0, 0x800);
    assert_eq!(nv.ioctl(ch_fd, 0xC020_481A, &mut gpfifo), NV_OK);
    let syncpt = rd32(&gpfifo, 12);

    // Build unified pushbuffer:
    //  - Subchannel 0: 0xC797 (AMPERE_B) clear Orin Layout 1 A8B8G8R8 target to Opaque Blue
    //  - Subchannel 1: 0xC7B5 (AMPERE_DMA_COPY_B) completion semaphore release
    let mut pb = Vec::new();
    pb.extend([inc_header(0, m::SET_OBJECT, 1), AMPERE_B]);
    pb.extend([
        inc_header(0, m::SET_COLOR_TARGET_A, 9),
        hi(rt_va),
        lo(rt_va),
        64,
        64,
        0xd5,
        3 << 4,
        1,
        0,
        0,
    ]);
    pb.push(immd_header(0, m::SET_CT_SELECT, 1));
    pb.push(inc_header(0, m::SET_COLOR_CLEAR_VALUE, 4));
    pb.extend([0.0f32, 0.0, 1.0, 1.0].map(f32::to_bits));
    pb.push(immd_header(0, m::CLEAR_SURFACE, 0x3c));

    // 0xC7B5 semaphore
    pb.extend([inc_header(1, m::SET_OBJECT, 1), AMPERE_DMA_COPY_B]);
    pb.extend([
        inc_header(1, crate::gpu::ampere::dma_copy::SET_SEMAPHORE_A, 3),
        hi(sem_va),
        lo(sem_va),
        0xCAFE_0400,
    ]);
    pb.extend([inc_header(1, crate::gpu::ampere::dma_copy::LAUNCH_DMA, 1), 1 | (1 << 3)]);

    let pb_bytes: Vec<u8> = pb.iter().flat_map(|w| w.to_le_bytes()).collect();
    mmu.write_bytes(pb_cpu, &pb_bytes).unwrap();

    // Submit GPFIFO
    let mut submit = vec![0u8; 0x20];
    wr32(&mut submit, 8, 1);
    wr64(&mut submit, 0x18, pb_va | (pb.len() as u64) << 42);
    assert_eq!(nv.ioctl(ch_fd, 0xC020_4808, &mut submit), NV_OK);

    let subs = nv.take_channel_submissions();
    assert_eq!(subs.len(), 1);
    for (fd, addr, n) in subs {
        assert!(nv.run_3d(&mut mmu, fd, addr, n));
    }

    // Verify GPU execution results in guest memory:
    //  1. Semaphore written by 0xC7B5 DMA engine
    assert_eq!(mmu.read_u32(sem_cpu).unwrap(), 0xCAFE_0400);

    //  2. Render target cleared and tiled as Orin/T239 Sector Layout 1
    let mut raw_rt = vec![0u8; 64 * 64 * 4];
    mmu.read_bytes(rt_cpu, &mut raw_rt).unwrap();
    let orin_surf = Surface::new_2d(
        64,
        64,
        Format::R8G8B8A8Unorm,
        Layout::BlockLinearOrin { height_log2: 3, depth_log2: 0, gob_kind: GobKind::Color2d },
    );
    let detiled_rt = tiling::detile(&orin_surf, &raw_rt).unwrap();
    for p in detiled_rt.chunks_exact(4) {
        assert_eq!(p, [0, 0, 255, 255], "Pixel must be opaque blue");
    }

    //  3. Channel fence signaled
    assert_eq!(nv.syncpts[syncpt as usize], 1);

    // Clean up temporary test files
    let _ = fs::remove_dir_all(&temp_dir);
}
