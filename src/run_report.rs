//! Machine-readable run report (`ounce.run.v1`) emitted when `--report-json=<path>` is passed.

use std::path::PathBuf;
use std::sync::OnceLock;

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

#[allow(dead_code)]
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
