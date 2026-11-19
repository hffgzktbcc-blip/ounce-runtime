//! Integration tests for `OUNCE_TRACE_SUMMARY` / `OUNCE_TRACE_STEPS` (`ounce.trace.v1`).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

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

fn run_trace(
    tag: &str,
    program_bytes: &[u8],
    args: &[&str],
    step_limit: Option<&str>,
    expect_success: bool,
) -> serde_json::Value {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("ounce-trace-v1-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let program = dir.join("p.nro");
    let trace_path = dir.join("trace.json");
    fs::write(&program, program_bytes).unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"));
    cmd.arg("run-nro")
        .arg(&program)
        .arg("--present=headless")
        .arg("--audio=null")
        .arg("--deterministic")
        .args(args)
        .env("OUNCE_TRACE_SUMMARY", &trace_path);
    if let Some(limit) = step_limit {
        cmd.env("OUNCE_TRACE_STEPS", limit);
    }

    let output = cmd.output().expect("spawn ounce-runtime");
    assert_eq!(
        output.status.success(),
        expect_success,
        "{tag}: unexpected status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let raw = fs::read_to_string(&trace_path).expect("trace summary written");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("valid ounce.trace.v1 JSON");
    let _ = fs::remove_dir_all(&dir);
    json
}

#[test]
fn trace_summary_records_steps_registers_and_summary_fields_across_tiers() {
    let code = [
        0xD280_0840, // movz x0, #0x42
        0xF100_0400, // subs x0, x0, #1 -> x0=0x41, NZCV=0x2000_0000 (C=1)
        0xD400_00E1, // svc #7 (svcExitProcess)
    ];
    let bytes = nro(&code);

    let mut tiers = vec![("interpreter", "--cpu=interp"), ("tier3", "--cpu=tier3")];
    if cfg!(feature = "native-jit") && cfg!(target_arch = "aarch64") {
        tiers.extend([("tier4", "--cpu=tier4"), ("nce", "--cpu=nce")]);
    }

    for (tier_name, flag) in tiers {
        let trace = run_trace(&format!("clean-{tier_name}"), &bytes, &[flag], None, true);
        assert_eq!(trace["schema"], "ounce.trace.v1");
        assert_eq!(trace["kind"], "trace");
        assert_eq!(trace["engine"], "ounce-runtime");
        assert_eq!(trace["tier"], tier_name);
        assert_eq!(trace["stop_kind"], "clean_exit");
        assert_eq!(trace["exit_code"], 0);
        assert_eq!(trace["insns_executed"], 4);
        assert_eq!(trace["step_limit"], 1024);
        assert_eq!(trace["steps_executed"], 4);
        assert_eq!(trace["steps_recorded"], 4);
        assert_eq!(trace["truncated"], false);
        assert_eq!(trace["final_x0"], "0x0000000000000041");
        assert_eq!(trace["final_nzcv"], "0x20000000");

        let steps = trace["steps"].as_array().expect("steps array");
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0]["i"], 0);
        assert_eq!(steps[0]["pc"], "0x0000000080000000");
        assert_eq!(steps[0]["opcode"], "0x14000020");
        assert_eq!(steps[1]["pc"], "0x0000000080000080");
        assert_eq!(steps[1]["regs"]["x0"], "0x0000000000000042");
        assert_eq!(steps[2]["pc"], "0x0000000080000084");
        assert_eq!(steps[2]["regs"]["x0"], "0x0000000000000041");
        assert_eq!(steps[2]["regs"]["nzcv"], "0x20000000");
        assert_eq!(steps[3]["pc"], "0x0000000080000088");
        assert_eq!(steps[3]["opcode"], "0xd40000e1");
    }
}

#[test]
fn trace_summary_honors_step_limit_truncation_and_writes_on_fault() {
    let code = [
        0xD280_0020, // movz x0, #1
        0x9100_0400, // add x0, x0, #1
        0x9100_0400, // add x0, x0, #1
        0xD400_00E1, // svc #7
    ];
    let bytes = nro(&code);
    let truncated = run_trace("truncated", &bytes, &["--cpu=tier3"], Some("2"), true);
    assert_eq!(truncated["step_limit"], 2);
    assert_eq!(truncated["steps_executed"], 5);
    assert_eq!(truncated["steps_recorded"], 2);
    assert_eq!(truncated["truncated"], true);
    assert_eq!(truncated["steps"].as_array().unwrap().len(), 2);

    // Unhandled SVC stops non-zero yet still writes ounce.trace.v1.
    let fault_bytes = nro(&[0xD280_00E0, 0xD400_0FE1]); // movz x0, #7; svc #0x7f
    let stop_trace = run_trace("unhandled-svc", &fault_bytes, &["--cpu=tier3"], None, false);
    assert_eq!(stop_trace["schema"], "ounce.trace.v1");
    assert_eq!(stop_trace["stop_kind"], "unhandled_svc");
    assert_eq!(stop_trace["exit_code"], 14);
    assert_eq!(stop_trace["steps_executed"], 3);
    assert_eq!(stop_trace["steps_recorded"], 3);
    assert_eq!(stop_trace["truncated"], false);
    assert_eq!(stop_trace["final_x0"], "0x0000000000000007");
}
