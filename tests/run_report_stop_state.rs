//! `ounce.run.v1` reports the link register and stack pointer at the stop, set exactly when `pc` is, plus a
//! `guest_exit_status` that is always null for homebrew. Expected values were taken from ounce-core running the same
//! two tiny programs, so this also pins agreement between the two tools.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// A minimal NRO: the header's first word branches to 0x80, where `code` starts at guest 0x8000_0080.
fn nro(code: &[u32]) -> Vec<u8> {
    let text: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let total = 0x80 + text.len();
    let mut b = vec![0u8; total];
    b[0..4].copy_from_slice(&0x1400_0020u32.to_le_bytes());
    b[0x10..0x14].copy_from_slice(b"NRO0");
    b[0x18..0x1c].copy_from_slice(&(total as u32).to_le_bytes());
    b[0x24..0x28].copy_from_slice(&(total as u32).to_le_bytes());
    b[0x80..].copy_from_slice(&text);
    b
}

fn run(tag: &str, code: &[u32]) -> serde_json::Value {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("ounce-stop-state-{}-{tag}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let program = dir.join("p.nro");
    let report = dir.join("report.json");
    fs::write(&program, nro(code)).unwrap();
    Command::new(env!("CARGO_BIN_EXE_ounce-runtime"))
        .arg("run-nro")
        .arg(&program)
        .args(["--present=headless", "--audio=null", "--deterministic"])
        .arg(format!("--report-json={}", report.display()))
        .output()
        .expect("spawn ounce-runtime");
    let json = serde_json::from_str(&fs::read_to_string(&report).expect("report written")).unwrap();
    let _ = fs::remove_dir_all(&dir);
    json
}

const MOVZ_W0_0: u32 = 0x5280_0000;
const SVC_BREAK: u32 = 0xD400_04C1; // svc #0x26
const SVC_EXIT_PROCESS: u32 = 0xD400_00E1; // svc #7
const BL_PLUS_8: u32 = 0x9400_0002;

#[test]
fn break_with_no_call_reports_initial_lr_and_sp() {
    let r = run("break", &[MOVZ_W0_0, SVC_BREAK]);
    assert_eq!(r["stop_kind"], "svc_break");
    assert_eq!(r["pc"], 0x8000_0088u64);
    assert_eq!(r["lr"], 0x7ffe_0000u64);
    assert_eq!(r["sp"], 0x7f_ffff_e000u64);
    assert!(r["guest_exit_status"].is_null());
}

#[test]
fn break_after_a_call_reports_the_return_address_in_lr() {
    // bl +8 jumps over one word, so lr is the address of the skipped word (base + 0x84).
    let r = run("bl", &[BL_PLUS_8, MOVZ_W0_0, SVC_BREAK]);
    assert_eq!(r["pc"], 0x8000_008cu64);
    assert_eq!(r["lr"], 0x8000_0084u64);
    assert_eq!(r["sp"], 0x7f_ffff_e000u64);
}

#[test]
fn clean_exit_leaves_lr_and_sp_null_like_pc() {
    // movz w0,#0 ; svc #7 (ExitProcess)
    let r = run("exit", &[MOVZ_W0_0, SVC_EXIT_PROCESS]);
    assert_eq!(r["stop_kind"], "clean_exit");
    assert!(r["pc"].is_null());
    assert!(r["lr"].is_null());
    assert!(r["sp"].is_null());
    assert!(r["guest_exit_status"].is_null());
}
