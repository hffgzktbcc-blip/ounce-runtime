#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::Path;
use std::time::Instant;

use crate::cpu::{Cpu, StepResult};
use crate::gpu::{DEFAULT_FRAMEBUFFER_ADDR, SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::hid::HID_SHARED_MEM_ADDR;
use crate::kernel::{DEFAULT_TLS_ADDR, GpuPersona, HorizonKernel, SvcResult, TLS_SIZE};
use crate::loader::nca::SYNTHETIC_TEST_KEY;
use crate::loader::{BootLoader, HomebrewConfig, LoosePackageLoader, ModuleMapEntry};
use crate::mmu::{Memory, Permission};

const TRACE_BASE_ADDR: u64 = 0x8000_0000;
const TRACE_STACK_TOP: u64 = 0x7FFF_FFE0_00;
const TRACE_STACK_SIZE: usize = 0x20_0000; // 2 MiB stack for retail Switch 2 titles
const MAIN_THREAD_HANDLE: u32 = 0x1000;
const EXIT_STUB_ADDR: u64 = 0x7FFE_0000;
const NRO_CONFIG_ADDR: u64 = 0x7FFF_0000;

#[derive(Debug, Clone)]
pub struct BootTraceOptions {
    pub max_steps: usize,
    pub json_out: Option<String>,
    pub persona_override: Option<GpuPersona>,
}

impl Default for BootTraceOptions {
    fn default() -> Self {
        Self {
            max_steps: 2_000_000,
            json_out: None,
            persona_override: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BootTraceReport {
    pub target: String,
    pub format: String,
    pub title_id: u64,
    pub process_name: String,
    pub gpu_persona: String,
    pub has_rtld: bool,
    pub instructions_executed: usize,
    pub frames_presented: usize,
    pub elapsed_ms: f64,
    pub stop_reason: String,
    pub final_pc: u64,
    pub final_pc_symbol: String,
    pub final_insn_word: Option<u32>,
    pub x0: u64,
    pub sp: u64,
    pub lr: u64,
    pub modules: Vec<ModuleMapEntry>,
    pub pc_trail: Vec<(u64, String)>,
    pub svc_histogram: Vec<(u16, &'static str, u64)>,
    pub opened_services: Vec<String>,
    pub unhandled_ipc: Vec<(String, u32)>,
    pub nvdrv_ioctls: Vec<(String, u32, u64)>,
    pub unhandled_nvdrv: Vec<(String, u32)>,
}

pub fn svc_name(id: u16) -> &'static str {
    match id {
        0x01 => "svcSetHeapSize",
        0x02 => "svcSetMemoryPermission",
        0x03 => "svcSetMemoryAttribute",
        0x04 => "svcMapMemory",
        0x05 => "svcUnmapMemory",
        0x06 => "svcQueryMemory",
        0x07 => "svcExitProcess",
        0x08 => "svcCreateThread",
        0x09 => "svcStartThread",
        0x0A => "svcExitThread",
        0x0B => "svcSleepThread",
        0x0C => "svcGetThreadPriority",
        0x0D => "svcSetThreadPriority",
        0x0E => "svcGetThreadCoreMask",
        0x0F => "svcSetThreadCoreMask",
        0x10 => "svcGetCurrentProcessorNumber",
        0x11 => "svcSignalEvent",
        0x12 => "svcClearEvent",
        0x13 => "svcMapSharedMemory",
        0x14 => "svcUnmapSharedMemory",
        0x15 => "svcCreateTransferMemory",
        0x16 => "svcCloseHandle",
        0x17 => "svcResetSignal",
        0x18 => "svcWaitSynchronization",
        0x19 => "svcCancelSynchronization",
        0x1A => "svcArbitrateLock",
        0x1B => "svcArbitrateUnlock",
        0x1C => "svcWaitProcessWideKeyAtomic",
        0x1D => "svcSignalProcessWideKey",
        0x1E => "svcGetSystemTick",
        0x1F => "svcConnectToNamedPort",
        0x21 => "svcSendSyncRequest",
        0x22 => "svcSendSyncRequestWithUserBuffer",
        0x25 => "svcGetThreadId",
        0x26 => "svcBreak",
        0x27 => "svcOutputDebugString",
        0x28 => "svcReturnFromException",
        0x29 => "svcGetInfo",
        0x2A => "svcFlushEntireDataCache",
        0x2B => "svcFlushDataCache",
        0x2C => "svcMapPhysicalMemory",
        0x2D => "svcUnmapPhysicalMemory",
        0x32 => "svcSetThreadActivity",
        0x33 => "svcGetThreadContext3",
        0x34 => "svcWaitForAddress",
        0x35 => "svcSignalToAddress",
        0x36 => "svcSynchronizePreemptionState",
        _ => "svcUnknown",
    }
}

pub fn symbolize_addr(addr: u64, modules: &[ModuleMapEntry]) -> String {
    for m in modules {
        if addr >= m.base_addr && addr < m.end_addr {
            return format!("{} + 0x{:06X}", m.name, addr - m.base_addr);
        }
    }
    format!("0x{:016X}", addr)
}

pub fn run_boot_trace(target_path: &str, opts: &BootTraceOptions) -> Result<BootTraceReport, String> {
    let mut cpu = Cpu::new();
    let mut mmu = Memory::new();
    let mut kernel = HorizonKernel::new();

    // Map 16 MiB main thread stack region with headroom above STACK_TOP
    let stack_base = TRACE_STACK_TOP - TRACE_STACK_SIZE as u64;
    mmu.map_range(stack_base, TRACE_STACK_SIZE + 0x10_0000, Permission::READ_WRITE);
    mmu.map_range(DEFAULT_TLS_ADDR, TLS_SIZE, Permission::READ_WRITE);
    mmu.map_range(
        DEFAULT_FRAMEBUFFER_ADDR,
        SCREEN_WIDTH * SCREEN_HEIGHT * 4,
        Permission::READ_WRITE,
    );
    mmu.map_range(HID_SHARED_MEM_ADDR, 0x1000, Permission::READ_WRITE);

    cpu.regs.sp = TRACE_STACK_TOP;
    cpu.regs.tpidr_el0 = DEFAULT_TLS_ADDR;
    cpu.regs.tpidrro_el0 = DEFAULT_TLS_ADDR;
    kernel.scheduler.main_handle = Some(MAIN_THREAD_HANDLE);

    let path_obj = Path::new(target_path);

    let (format, title_id, process_name, has_rtld, modules) = if path_obj.is_dir() {
        let pkg = LoosePackageLoader::discover(path_obj).map_err(|e| e.to_string())?;
        let info = LoosePackageLoader::load_into_kernel(&pkg, TRACE_BASE_ADDR, &mut mmu, &mut kernel)
            .map_err(|e| e.to_string())?;

        if let Some(tp) = info.thread_pointer {
            cpu.regs.tpidr_el0 = tp;
        }
        let stub = HomebrewConfig::install_exit_stub(&mut mmu, EXIT_STUB_ADDR)
            .map_err(|e| e.to_string())?;
        cpu.regs.write_x(30, stub);
        cpu.regs.pc = info.entry_pc;
        if info.has_rtld {
            // Horizon OS native process startup ABI: X0 = 0 (no exception context), X1 = main_thread_handle
            cpu.regs.write_x(0, 0);
            cpu.regs.write_x(1, MAIN_THREAD_HANDLE as u64);
        }
        (
            "LoosePackage (OunceCake)".to_string(),
            info.title_id,
            info.process_name,
            info.has_rtld,
            info.module_map,
        )
    } else {
        let bytes = std::fs::read(path_obj).map_err(|e| format!("Failed to read '{}': {}", target_path, e))?;
        let info = BootLoader::load_package_with_key(
            &bytes,
            TRACE_BASE_ADDR,
            &SYNTHETIC_TEST_KEY,
            &mut mmu,
            Some(&mut kernel.vfs),
        )
        .map_err(|e| e.to_string())?;

        let is_nro = info.format == "NRO0";
        if is_nro {
            mmu.map_range(kernel.heap_base, 0x200_0000, Permission::READ_WRITE);
            kernel.heap_size = 0x200_0000;
            let fname = path_obj.file_name().and_then(|s| s.to_str()).unwrap_or("app.nro");
            let sd = format!("sdmc:/{fname}");
            kernel.vfs.mount_file(&sd, &bytes);
            HomebrewConfig {
                main_thread_handle: Some(MAIN_THREAD_HANDLE),
                heap: Some((kernel.heap_base, 0x200_0000)),
                argv: Some(sd),
                ..Default::default()
            }
            .setup(&mut cpu, &mut mmu, NRO_CONFIG_ADDR)
            .map_err(|e| e.to_string())?;
        } else {
            kernel.configure_ounce_process(info.title_id, 39, 0);
            cpu.regs.write_x(0, 0);
            cpu.regs.write_x(1, MAIN_THREAD_HANDLE as u64);
        }
        let stub = HomebrewConfig::install_exit_stub(&mut mmu, EXIT_STUB_ADDR)
            .map_err(|e| e.to_string())?;
        cpu.regs.write_x(30, stub);
        cpu.regs.pc = TRACE_BASE_ADDR;

        let map = vec![ModuleMapEntry {
            name: "main".to_string(),
            base_addr: TRACE_BASE_ADDR,
            end_addr: TRACE_BASE_ADDR + ((info.code_size as u64 + 0xFFF) & !0xFFF).max(0x1000),
            file_size: bytes.len(),
            build_id: String::new(),
        }];
        (
            info.format.to_string(),
            info.title_id,
            path_obj
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Switch2Binary")
                .to_string(),
            false,
            map,
        )
    };

    if let Some(p) = opts.persona_override {
        kernel.ipc.nvdrv.persona = p;
    }

    let start = Instant::now();
    let mut instructions_executed = 0usize;
    let mut frames_presented = 0usize;
    let mut trail: VecDeque<u64> = VecDeque::with_capacity(16);
    let mut stop_reason = format!("BudgetReached ({} steps)", opts.max_steps);

    while instructions_executed < opts.max_steps {
        let cur_pc = cpu.regs.pc;
        if trail.back().copied() != Some(cur_pc) {
            if trail.len() == 16 {
                trail.pop_front();
            }
            trail.push_back(cur_pc);
        }

        match cpu.step(&mut mmu) {
            Ok(StepResult::Continue) => {
                instructions_executed += 1;
                if instructions_executed % crate::kernel::scheduler::PREEMPT_INTERVAL == 0 {
                    kernel.scheduler.preempt(&mut cpu);
                }
            }
            Ok(StepResult::Svc(svc_id)) => {
                instructions_executed += 1;
                match kernel.dispatch_svc(svc_id, &mut cpu, &mut mmu) {
                    SvcResult::Success => {}
                    SvcResult::PresentFrame => {
                        frames_presented += 1;
                        let _ = kernel.nvgpu.drain_draw_ops();
                    }
                    SvcResult::ExitProcess => {
                        stop_reason = format!("ExitProcess (X0 = 0x{:X})", cpu.regs.read_x(0));
                        break;
                    }
                    SvcResult::Break { reason, info1, info2 } => {
                        let extra = if info2 == 4 {
                            mmu.read_u32(info1)
                                .map(|r| {
                                    format!(
                                        ", result=0x{:08X} [module={}, desc={}]",
                                        r,
                                        r & 0x1FF,
                                        (r >> 9) & 0x1FFF
                                    )
                                })
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };
                        stop_reason = format!(
                            "svcBreak (reason=0x{:X}, info1=0x{:X}, info2=0x{:X}{})",
                            reason, info1, info2, extra
                        );
                        break;
                    }
                    SvcResult::Unhandled(id) => {
                        stop_reason = format!("UnhandledSvc (0x{:02X} {})", id, svc_name(id));
                        break;
                    }
                }
            }
            Ok(StepResult::Halt(msg)) => {
                stop_reason = format!("CpuHalt ({})", msg);
                break;
            }
            Err(err) => {
                stop_reason = format!("CpuFault ({})", err);
                break;
            }
        }
    }

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let final_pc = cpu.regs.pc;
    let final_pc_symbol = symbolize_addr(final_pc, &modules);
    let final_insn_word = mmu.read_u32(final_pc).ok();

    let mut svc_histogram = Vec::new();
    for (id, &count) in kernel.svc_counts.iter().enumerate() {
        if count > 0 {
            svc_histogram.push((id as u16, svc_name(id as u16), count));
        }
    }

    let pc_trail = trail
        .into_iter()
        .map(|pc| (pc, symbolize_addr(pc, &modules)))
        .collect();

    let nvdrv_ioctls = kernel
        .ipc
        .nvdrv
        .ioctl_counts
        .iter()
        .map(|((dev, req), &cnt)| (dev.clone(), *req, cnt))
        .collect();

    let gpu_persona = match kernel.ipc.nvdrv.persona {
        GpuPersona::Ga10f => "ga10f (Switch 2 T239 Ampere 0xC797)".to_string(),
        GpuPersona::Gm20b => "gm20b (Switch 1 TX1 Maxwell 0xB197)".to_string(),
    };

    let report = BootTraceReport {
        target: target_path.to_string(),
        format,
        title_id,
        process_name,
        gpu_persona,
        has_rtld,
        instructions_executed,
        frames_presented,
        elapsed_ms,
        stop_reason,
        final_pc,
        final_pc_symbol,
        final_insn_word,
        x0: cpu.regs.read_x(0),
        sp: cpu.regs.sp,
        lr: cpu.regs.read_x(30),
        modules,
        pc_trail,
        svc_histogram,
        opened_services: kernel.ipc.opened_services.clone(),
        unhandled_ipc: kernel.ipc.unhandled_commands.clone(),
        nvdrv_ioctls,
        unhandled_nvdrv: kernel.ipc.nvdrv.unhandled.clone(),
    };

    if let Some(ref json_path) = opts.json_out {
        let json = render_report_json(&report);
        std::fs::write(json_path, json)
            .map_err(|e| format!("Failed to write JSON trace '{}': {}", json_path, e))?;
    }

    Ok(report)
}

pub fn print_boot_trace_report(r: &BootTraceReport) {
    println!("============================================================");
    println!("  🛰️  OUNCE-NX2 FLIGHT RECORDER — BOOT TRACE REPORT");
    println!("============================================================");
    println!("  Target        : {}", r.target);
    println!("  Format        : {} (has_rtld={})", r.format, r.has_rtld);
    println!("  Process       : {} (Title ID 0x{:016X})", r.process_name, r.title_id);
    println!("  GPU Persona   : {}", r.gpu_persona);
    println!(
        "  Executed      : {} steps in {:.2} ms | Frames: {}",
        r.instructions_executed, r.elapsed_ms, r.frames_presented
    );
    println!("  Stop Reason   : {}", r.stop_reason);
    if let Some(insn) = r.final_insn_word {
        println!(
            "  Final PC      : 0x{:016X} ({}) [insn=0x{:08X}]",
            r.final_pc, r.final_pc_symbol, insn
        );
    } else {
        println!("  Final PC      : 0x{:016X} ({})", r.final_pc, r.final_pc_symbol);
    }
    println!(
        "  Final Regs    : X0=0x{:X} | LR={} | SP=0x{:X}",
        r.x0,
        symbolize_addr(r.lr, &r.modules),
        r.sp
    );

    println!("\n  [Loaded Module Map ({})]", r.modules.len());
    for m in &r.modules {
        let bid = if m.build_id.is_empty() {
            String::new()
        } else {
            format!(" build_id={}..", &m.build_id[..m.build_id.len().min(16)])
        };
        println!(
            "    - {:<10} 0x{:010X}..0x{:010X} ({} bytes){}",
            m.name, m.base_addr, m.end_addr, m.file_size, bid
        );
    }

    if !r.svc_histogram.is_empty() {
        println!("\n  [Horizon SVC Histogram]");
        for (id, name, count) in &r.svc_histogram {
            println!("    - 0x{:02X} {:<30} : {}", id, name, count);
        }
    }

    if !r.opened_services.is_empty() {
        println!("\n  [Opened HIPC Services ({})]", r.opened_services.len());
        println!("    {}", r.opened_services.join(", "));
    }

    if !r.unhandled_ipc.is_empty() {
        println!("\n  [Unhandled HIPC Commands ({})]", r.unhandled_ipc.len());
        for (srv, cmd) in &r.unhandled_ipc {
            println!("    ! '{}' command {}", srv, cmd);
        }
    }

    if !r.nvdrv_ioctls.is_empty() {
        println!("\n  [nvdrv Device Ioctls ({})]", r.nvdrv_ioctls.len());
        for (dev, req, cnt) in &r.nvdrv_ioctls {
            println!("    - {:<22} 0x{:08X} : {}", dev, req, cnt);
        }
    }

    if !r.unhandled_nvdrv.is_empty() {
        println!("\n  [Unhandled nvdrv Ioctls ({})]", r.unhandled_nvdrv.len());
        for (dev, req) in &r.unhandled_nvdrv {
            println!("    ! {} ioctl 0x{:08X}", dev, req);
        }
    }

    if !r.pc_trail.is_empty() {
        println!("\n  [Last {} PCs Before Stop]", r.pc_trail.len());
        for (pc, sym) in &r.pc_trail {
            println!("    0x{:016X}  ({})", pc, sym);
        }
    }
    println!("============================================================\n");
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn render_report_json(r: &BootTraceReport) -> String {
    let mods: Vec<String> = r
        .modules
        .iter()
        .map(|m| {
            format!(
                "{{\"name\":\"{}\",\"base\":\"0x{:X}\",\"end\":\"0x{:X}\",\"file_size\":{},\"build_id\":\"{}\"}}",
                escape_json(&m.name),
                m.base_addr,
                m.end_addr,
                m.file_size,
                escape_json(&m.build_id)
            )
        })
        .collect();
    let svcs: Vec<String> = r
        .svc_histogram
        .iter()
        .map(|(id, name, cnt)| format!("{{\"id\":\"0x{:02X}\",\"name\":\"{}\",\"count\":{}}}", id, name, cnt))
        .collect();
    let srvs: Vec<String> = r
        .opened_services
        .iter()
        .map(|s| format!("\"{}\"", escape_json(s)))
        .collect();
    let uipc: Vec<String> = r
        .unhandled_ipc
        .iter()
        .map(|(s, c)| format!("{{\"service\":\"{}\",\"cmd\":{}}}", escape_json(s), c))
        .collect();
    format!(
        "{{\n  \"target\": \"{}\",\n  \"format\": \"{}\",\n  \"title_id\": \"0x{:016X}\",\n  \"process_name\": \"{}\",\n  \"gpu_persona\": \"{}\",\n  \"has_rtld\": {},\n  \"instructions_executed\": {},\n  \"frames_presented\": {},\n  \"elapsed_ms\": {:.3},\n  \"stop_reason\": \"{}\",\n  \"final_pc\": \"0x{:016X}\",\n  \"final_pc_symbol\": \"{}\",\n  \"x0\": \"0x{:X}\",\n  \"modules\": [{}],\n  \"svcs\": [{}],\n  \"opened_services\": [{}],\n  \"unhandled_ipc\": [{}]\n}}\n",
        escape_json(&r.target),
        escape_json(&r.format),
        r.title_id,
        escape_json(&r.process_name),
        escape_json(&r.gpu_persona),
        r.has_rtld,
        r.instructions_executed,
        r.frames_presented,
        r.elapsed_ms,
        escape_json(&r.stop_reason),
        r.final_pc,
        escape_json(&r.final_pc_symbol),
        r.x0,
        mods.join(", "),
        svcs.join(", "),
        srvs.join(", "),
        uipc.join(", "),
    )
}
