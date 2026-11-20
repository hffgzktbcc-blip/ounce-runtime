//! Machine-readable run report (`ounce.run.v1`) emitted when `--report-json=<path>` is passed,
//! and bounded instruction trace summary (`ounce.trace.v1`) emitted when `OUNCE_TRACE_SUMMARY=<path>` is set.

use std::path::PathBuf;
use std::sync::OnceLock;

pub const DEFAULT_TRACE_STEPS: usize = 1024;
pub const MAX_TRACE_STEPS: usize = 65536;

const X_REG_NAMES: [&str; 31] = [
    "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13", "x14",
    "x15", "x16", "x17", "x18", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27",
    "x28", "x29", "x30",
];

const V_REG_NAMES: [&str; 32] = [
    "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v9", "v10", "v11", "v12", "v13", "v14",
    "v15", "v16", "v17", "v18", "v19", "v20", "v21", "v22", "v23", "v24", "v25", "v26", "v27",
    "v28", "v29", "v30", "v31",
];

static REPORT_JSON_PATH: OnceLock<PathBuf> = OnceLock::new();
static AUDIO_NULL: OnceLock<bool> = OnceLock::new();
static DETERMINISTIC: OnceLock<bool> = OnceLock::new();

pub fn set_report_path(path: PathBuf) {
    let _ = REPORT_JSON_PATH.set(path);
}

pub fn set_audio_null(enabled: bool) {
    let _ = AUDIO_NULL.set(enabled);
}

pub fn is_audio_null() -> bool {
    AUDIO_NULL.get().copied().unwrap_or(false)
}

pub fn set_deterministic(enabled: bool) {
    let _ = DETERMINISTIC.set(enabled);
}

pub fn is_deterministic() -> bool {
    DETERMINISTIC.get().copied().unwrap_or(false)
}

/// Structured stop report matching `ounce.run.v1`.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub tier: &'static str,
    pub silent_fallback: bool,
    pub stop_kind: &'static str,
    pub exit_code: i32,
    pub pc: Option<u64>,
    /// Link register (`x30`) at the stop; set exactly when `pc` is.
    pub lr: Option<u64>,
    /// Stack pointer at the stop; set exactly when `pc` is.
    pub sp: Option<u64>,
    pub opcode: Option<u32>,
    pub fault_addr: Option<u64>,
    pub svc_id: Option<u32>,
    pub break_result: Option<u32>,
    pub insns_executed: u64,
    pub frames_presented: u64,
    pub framebuffer_hash: Option<String>,
    /// Stereo frames the guest mixed through `audout` or `audren`.
    pub audio_frames_mixed: u64,
    /// `fnv1a64:<hex>` over the mixed samples; `None` when nothing was mixed.
    pub audio_hash: Option<String>,
    pub message: Option<String>,
}

/// Write `ounce.run.v1` JSON to the `--report-json` path if configured.
pub fn write_if_configured(report: &RunReport) {
    let Some(path) = REPORT_JSON_PATH.get() else {
        return;
    };
    let payload = serde_json::json!({
        "schema": "ounce.run.v1",
        "engine": "ounce-runtime",
        "engine_version": env!("CARGO_PKG_VERSION"),
        "tier": report.tier,
        "silent_fallback": report.silent_fallback,
        "stop_kind": report.stop_kind,
        "exit_code": report.exit_code,
        "pc": report.pc,
        "lr": report.lr,
        "sp": report.sp,
        "opcode": report.opcode,
        "fault_addr": report.fault_addr,
        "svc_id": report.svc_id,
        "break_result": report.break_result,
        "insns_executed": report.insns_executed,
        "frames_presented": report.frames_presented,
        "framebuffer_hash": report.framebuffer_hash,
        "audio_frames_mixed": report.audio_frames_mixed,
        "audio_hash": report.audio_hash,
        // Homebrew has no exit status; `run-elf`-only in the contract, so always null here.
        "guest_exit_status": serde_json::Value::Null,
        "message": report.message,
    });
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&payload) {
        if let Err(err) = std::fs::write(path, bytes) {
            eprintln!("[Report] Failed to write {}: {err}", path.display());
        }
    }
}

/// Parses `OUNCE_TRACE_STEPS`: default 1024, clamped to `1..=65536`; non-positive or invalid values
/// fall back to 1024.
pub fn parse_trace_step_limit(raw: Option<&str>) -> usize {
    let Some(raw) = raw else {
        return DEFAULT_TRACE_STEPS;
    };
    match raw.trim().parse::<usize>() {
        Ok(n) if n >= 1 => n.min(MAX_TRACE_STEPS),
        _ => {
            eprintln!(
                "[Trace] OUNCE_TRACE_STEPS={raw} is not a positive integer; using {DEFAULT_TRACE_STEPS}"
            );
            DEFAULT_TRACE_STEPS
        }
    }
}

/// One retired guest instruction checkpoint in an `ounce.trace.v1` trace summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceStep {
    pub i: u64,
    pub pc: u64,
    pub opcode: u32,
    pub regs: Vec<(&'static str, String)>,
}

/// Bounded instruction step recorder for `OUNCE_TRACE_SUMMARY` (`ounce.trace.v1`).
#[derive(Debug, Clone)]
pub struct TraceRecorder {
    pub path: Option<PathBuf>,
    pub step_limit: usize,
    pub steps: Vec<TraceStep>,
}

impl TraceRecorder {
    pub fn new(step_limit: usize) -> Self {
        let limit = step_limit.clamp(1, MAX_TRACE_STEPS);
        Self {
            path: None,
            step_limit: limit,
            steps: Vec::with_capacity(limit.min(DEFAULT_TRACE_STEPS)),
        }
    }

    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("OUNCE_TRACE_SUMMARY")?;
        if path.is_empty() {
            return None;
        }
        let steps_raw = std::env::var("OUNCE_TRACE_STEPS").ok();
        let step_limit = parse_trace_step_limit(steps_raw.as_deref());
        let mut rec = Self::new(step_limit);
        rec.path = Some(PathBuf::from(path));
        Some(rec)
    }

    pub fn is_capturing(&self) -> bool {
        self.steps.len() < self.step_limit
    }

    pub fn record_step(
        &mut self,
        pc: u64,
        opcode: u32,
        before: &crate::cpu::registers::CpuRegisters,
        after: &crate::cpu::registers::CpuRegisters,
    ) {
        if !self.is_capturing() {
            return;
        }
        let mut regs = Vec::new();
        for (idx, name) in X_REG_NAMES.iter().enumerate() {
            if before.x[idx] != after.x[idx] {
                regs.push((*name, format!("0x{:016x}", after.x[idx])));
            }
        }
        if before.sp != after.sp {
            regs.push(("sp", format!("0x{:016x}", after.sp)));
        }
        let b_nzcv = before.pstate.to_bits();
        let a_nzcv = after.pstate.to_bits();
        if b_nzcv != a_nzcv {
            regs.push(("nzcv", format!("0x{a_nzcv:08x}")));
        }
        if before.fpsr != after.fpsr {
            regs.push(("fpsr", format!("0x{:08x}", after.fpsr)));
        }
        for (idx, name) in V_REG_NAMES.iter().enumerate() {
            if before.v[idx] != after.v[idx] {
                regs.push((*name, format!("0x{:032x}", after.v[idx])));
            }
        }
        let i = self.steps.len() as u64;
        self.steps.push(TraceStep {
            i,
            pc,
            opcode,
            regs,
        });
    }

    pub fn to_json(
        &self,
        report: &RunReport,
        final_regs: &crate::cpu::registers::CpuRegisters,
    ) -> serde_json::Value {
        let steps_executed = report.insns_executed;
        let steps_recorded = self.steps.len() as u64;
        let truncated = steps_executed > steps_recorded;
        let steps: Vec<serde_json::Value> = self
            .steps
            .iter()
            .map(|s| {
                let mut regs = serde_json::Map::with_capacity(s.regs.len());
                for (k, v) in &s.regs {
                    regs.insert((*k).to_string(), serde_json::Value::String(v.clone()));
                }
                serde_json::json!({
                    "i": s.i,
                    "pc": format!("0x{:016x}", s.pc),
                    "opcode": format!("0x{:08x}", s.opcode),
                    "regs": serde_json::Value::Object(regs),
                })
            })
            .collect();
        serde_json::json!({
            "schema": "ounce.trace.v1",
            "kind": "trace",
            "engine": "ounce-runtime",
            "engine_version": env!("CARGO_PKG_VERSION"),
            "tier": report.tier,
            "insns_executed": report.insns_executed,
            "stop_kind": report.stop_kind,
            "exit_code": report.exit_code,
            "final_pc": format!("0x{:016x}", final_regs.pc),
            "final_sp": format!("0x{:016x}", final_regs.sp),
            "final_lr": format!("0x{:016x}", final_regs.read_x(30)),
            "final_x0": format!("0x{:016x}", final_regs.read_x(0)),
            "final_nzcv": format!("0x{:08x}", final_regs.pstate.to_bits()),
            "step_limit": self.step_limit as u64,
            "steps_executed": steps_executed,
            "steps_recorded": steps_recorded,
            "truncated": truncated,
            "steps": steps,
        })
    }

    pub fn write_if_configured(
        &self,
        report: &RunReport,
        final_regs: &crate::cpu::registers::CpuRegisters,
    ) {
        let Some(ref path) = self.path else {
            return;
        };
        let payload = self.to_json(report, final_regs);
        let steps_executed = report.insns_executed;
        let steps_recorded = self.steps.len() as u64;
        let truncated = steps_executed > steps_recorded;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&payload) {
            match std::fs::write(path, bytes) {
                Ok(()) => {
                    let suffix = if truncated { " (truncated)" } else { "" };
                    eprintln!(
                        "[Trace] {steps_recorded} of {steps_executed} steps written to {}{suffix}",
                        path.display()
                    );
                }
                Err(err) => {
                    eprintln!("[Trace] Failed to write {}: {err}", path.display());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::registers::CpuRegisters;

    #[test]
    fn parse_trace_step_limit_handles_defaults_and_bounds() {
        assert_eq!(parse_trace_step_limit(None), 1024);
        assert_eq!(parse_trace_step_limit(Some("16")), 16);
        assert_eq!(parse_trace_step_limit(Some("999999")), 65536);
        assert_eq!(parse_trace_step_limit(Some("0")), 1024);
        assert_eq!(parse_trace_step_limit(Some("bad")), 1024);
    }

    #[test]
    fn trace_recorder_captures_changed_registers_and_truncation() {
        let mut rec = TraceRecorder::new(2);
        let before = CpuRegisters::new();
        let mut after1 = before.clone();
        after1.write_x(0, 0x42);
        after1.sp = 0x7fff_ffe000;
        after1.pstate.z = true;
        rec.record_step(0x8000_0000, 0xd280_0840, &before, &after1);

        let mut after2 = after1.clone();
        after2.v[3] = 0x1122_3344;
        rec.record_step(0x8000_0004, 0x4e08_1c03, &after1, &after2);

        // Third step beyond limit (2) is ignored by step buffer.
        let mut after3 = after2.clone();
        after3.write_x(1, 9);
        rec.record_step(0x8000_0008, 0xd400_00e1, &after2, &after3);

        let report = RunReport {
            tier: "tier3",
            silent_fallback: false,
            stop_kind: "clean_exit",
            exit_code: 0,
            pc: None,
            lr: None,
            sp: None,
            opcode: None,
            fault_addr: None,
            svc_id: None,
            break_result: None,
            insns_executed: 3,
            frames_presented: 0,
            framebuffer_hash: None,
            audio_frames_mixed: 0,
            audio_hash: None,
            message: None,
        };
        let json = rec.to_json(&report, &after3);
        assert_eq!(json["schema"], "ounce.trace.v1");
        assert_eq!(json["kind"], "trace");
        assert_eq!(json["engine"], "ounce-runtime");
        assert_eq!(json["tier"], "tier3");
        assert_eq!(json["insns_executed"], 3);
        assert_eq!(json["stop_kind"], "clean_exit");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["step_limit"], 2);
        assert_eq!(json["steps_executed"], 3);
        assert_eq!(json["steps_recorded"], 2);
        assert_eq!(json["truncated"], true);
        assert_eq!(json["steps"][0]["regs"]["x0"], "0x0000000000000042");
        assert_eq!(json["steps"][0]["regs"]["sp"], "0x0000007fffffe000");
        assert_eq!(json["steps"][0]["regs"]["nzcv"], "0x40000000");
        assert_eq!(
            json["steps"][1]["regs"]["v3"],
            "0x00000000000000000000000011223344"
        );
    }
}
