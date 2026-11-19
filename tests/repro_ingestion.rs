use std::fs;
use std::path::{Path, PathBuf};
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

fn available_tiers() -> Vec<&'static str> {
    let mut tiers = vec!["interp", "tier3"];
    if cfg!(feature = "native-jit") && cfg!(target_arch = "aarch64") {
        tiers.extend(["tier4", "nce"]);
    }
    tiers
}

fn run_repro_manifest_on_tier(
    manifest_path: &Path,
    report_path: &Path,
    tier_override: Option<&str>,
) -> serde_json::Value {
    let raw = fs::read_to_string(manifest_path).expect("read repro manifest");
    let manifest: serde_json::Value =
        serde_json::from_str(&raw).expect("parse ounce.repro.v1 JSON");
    assert_eq!(manifest["schema"].as_str(), Some("ounce.repro.v1"));

    let program_rel = manifest["program_path"]
        .as_str()
        .expect("program_path string in ounce.repro.v1");
    let program = if Path::new(program_rel).is_absolute() {
        PathBuf::from(program_rel)
    } else {
        manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(program_rel)
    };

    let guest_args: Vec<&str> = manifest["guest_args"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let tier = tier_override.or_else(|| manifest["runtime"]["tier"].as_str());

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"));
    cmd.arg("run-nro")
        .arg(&program)
        .arg("--present=headless")
        .arg("--audio=null")
        .arg("--deterministic")
        .arg(format!("--report-json={}", report_path.display()));
    if let Some(t) = tier {
        cmd.arg(format!("--cpu={t}"));
    }
    if !guest_args.is_empty() {
        cmd.arg("--").args(&guest_args);
    }

    let out = cmd.output().expect("spawn ounce-runtime");
    let report_raw = fs::read_to_string(report_path).expect("ounce.run.v1 report written");
    let report: serde_json::Value = serde_json::from_str(&report_raw).expect("parse ounce.run.v1");
    assert_eq!(report["schema"], "ounce.run.v1");

    let core = &manifest["core"];
    if let Some(expected_kind) = core["stop_kind"].as_str() {
        assert_eq!(
            report["stop_kind"].as_str(),
            Some(expected_kind),
            "repro manifest {} (tier={tier:?}) stop_kind mismatch; stderr: {}",
            manifest_path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if let Some(expected_exit) = core["exit_code"].as_i64() {
        assert_eq!(
            report["exit_code"].as_i64(),
            Some(expected_exit),
            "repro manifest {} (tier={tier:?}) exit_code mismatch",
            manifest_path.display()
        );
    }
    if let Some(expected_frames) = core["frames_presented"].as_u64() {
        assert_eq!(
            report["frames_presented"].as_u64(),
            Some(expected_frames),
            "repro manifest {} (tier={tier:?}) frames_presented mismatch",
            manifest_path.display()
        );
    }
    if let Some(expected_hash) = core.as_object().and_then(|obj| obj.get("framebuffer_hash")) {
        assert_eq!(
            &report["framebuffer_hash"],
            expected_hash,
            "repro manifest {} (tier={tier:?}) framebuffer_hash mismatch",
            manifest_path.display()
        );
    }
    if let Some(expected_svc) = core["svc_id"].as_u64() {
        assert_eq!(
            report["svc_id"].as_u64(),
            Some(expected_svc),
            "repro manifest {} (tier={tier:?}) svc_id mismatch",
            manifest_path.display()
        );
    }
    if let Some(expected_break) = core["break_result"].as_u64() {
        assert_eq!(
            report["break_result"].as_u64(),
            Some(expected_break),
            "repro manifest {} (tier={tier:?}) break_result mismatch",
            manifest_path.display()
        );
    }
    report
}

fn run_repro_manifest(manifest_path: &Path, report_path: &Path) {
    run_repro_manifest_on_tier(manifest_path, report_path, None);
}

#[test]
fn ingests_ounce_repro_v1_manifest_and_directory_fixtures() {
    let tmp = std::env::temp_dir().join(format!("ounce_repro_ingest_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Minimal clean-exit NRO (`mov w0, #0; svc #7` -> svcExitProcess).
    let nro_bytes = nro(&[0x5280_0000, 0xD400_00E1]);
    let nro_path = tmp.join("minimal_exit.nro");
    fs::write(&nro_path, &nro_bytes).unwrap();

    let manifest_path = tmp.join("minimal_exit.repro.json");
    let manifest = serde_json::json!({
        "schema": "ounce.repro.v1",
        "program_path": "minimal_exit.nro",
        "program_bytes": nro_bytes.len(),
        "guest_args": [],
        "runtime": { "tier": "tier3", "stop_kind": "cpu_error", "exit_code": 12 },
        "core": { "stop_kind": "clean_exit", "exit_code": 0, "frames_presented": 0, "framebuffer_hash": null }
    });
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let report_path = tmp.join("report.json");
    run_repro_manifest(&manifest_path, &report_path);

    // Also ingest all checked-in `tests/repro/*.repro.json` regression bundles.
    let repro_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/repro");
    let mut checked_in = 0usize;
    if repro_dir.is_dir() {
        for entry in fs::read_dir(&repro_dir).unwrap().flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".repro.json"))
            {
                let rep = tmp.join(format!(
                    "{}.report.json",
                    path.file_stem().unwrap().to_string_lossy()
                ));
                run_repro_manifest(&path, &rep);
                checked_in += 1;
            }
        }
    }
    assert!(
        checked_in >= 3,
        "expected at least 3 checked-in tests/repro/*.repro.json fixtures, found {checked_in}"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn repro_manifest_verifies_pre_hud_framebuffer_hash_across_tiers() {
    let tmp = std::env::temp_dir().join(format!("ounce_repro_fb_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Paint pixel (0, 0) inside the HUD band with alpha 0x11, present via svc #0x18, exit cleanly.
    let nro_bytes = nro(&[
        0xD280_0000, // movz x0, #0
        0xF2B2_0000, // movk x0, #0x9000, lsl #16
        0x5286_6881, // movz w1, #0x3344
        0x72A2_2441, // movk w1, #0x1122, lsl #16
        0xB900_0001, // str w1, [x0]
        0xD400_0301, // svc #0x18 (VSync)
        0x5280_0000, // movz w0, #0
        0xD400_00E1, // svc #7 (svcExitProcess)
    ]);
    fs::write(tmp.join("hud_pixel.nro"), &nro_bytes).unwrap();

    let manifest_path = tmp.join("hud_pixel.repro.json");
    let manifest = serde_json::json!({
        "schema": "ounce.repro.v1",
        "program_path": "hud_pixel.nro",
        "program_bytes": nro_bytes.len(),
        "guest_args": [],
        "runtime": { "tier": "tier3" },
        "core": {
            "stop_kind": "clean_exit",
            "exit_code": 0,
            "frames_presented": 1,
            "framebuffer_hash": "fnv1a64:d1777609f9ade9b2"
        }
    });
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    for tier in available_tiers() {
        let rep = tmp.join(format!("report_{tier}.json"));
        run_repro_manifest_on_tier(&manifest_path, &rep, Some(tier));
    }

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn repro_manifest_verifies_fp_nan_and_simd_consistency_across_tiers() {
    let tmp = std::env::temp_dir().join(format!("ounce_repro_fp_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Exercise FP 0.0/0.0 NaN canonicalization, NaN propagation, unordered fcmp flags,
    // and SIMD movi/dup/orr/umov, then store the result into framebuffer pixel (0, 0).
    let nro_bytes = nro(&[
        0xD280_0000, // movz x0, #0
        0x9E67_0000, // fmov d0, x0 (0.0)
        0xD280_0021, // movz x1, #1
        0xF2EF_FE01, // movk x1, #0x3ff0, lsl #48 (1.0 + eps)
        0x9E67_0021, // fmov d1, x1
        0x1E60_1802, // fdiv d2, d0, d0 -> canonical quiet NaN
        0x1E61_2843, // fadd d3, d2, d1 -> canonical quiet NaN
        0x1E61_2040, // fcmp d2, d1 -> unordered (V=1)
        0x1A9F_77E4, // cset w4, vs -> w4 = 1
        0x9E66_0062, // fmov x2, d3
        0xD360_FC42, // lsr x2, x2, #32 -> w2 = 0x7ff8_0000
        0x4F00_0405, // movi v5.16b, #0x20
        0x4E01_0C86, // dup v6.16b, w4
        0x4EA6_1CA7, // orr v7.16b, v5.16b, v6.16b -> 0x21 in every byte
        0x0E01_3CE5, // umov w5, v7.b[0] -> 0x21
        0x2A05_0042, // orr w2, w2, w5 -> 0x7ff8_0021
        0xD280_0000, // movz x0, #0
        0xF2B2_0000, // movk x0, #0x9000, lsl #16
        0xB900_0002, // str w2, [x0]
        0xD400_0301, // svc #0x18 (VSync)
        0x5280_0000, // movz w0, #0
        0xD400_00E1, // svc #7 (svcExitProcess)
    ]);
    fs::write(tmp.join("fp_nan_simd.nro"), &nro_bytes).unwrap();

    // First capture the interpreter reference hash, then verify every tier against the repro manifest.
    let seed_manifest_path = tmp.join("fp_nan_simd_seed.repro.json");
    let seed_manifest = serde_json::json!({
        "schema": "ounce.repro.v1",
        "program_path": "fp_nan_simd.nro",
        "guest_args": [],
        "core": {
            "stop_kind": "clean_exit",
            "exit_code": 0,
            "frames_presented": 1
        }
    });
    fs::write(
        &seed_manifest_path,
        serde_json::to_vec_pretty(&seed_manifest).unwrap(),
    )
    .unwrap();
    let interp_report = run_repro_manifest_on_tier(
        &seed_manifest_path,
        &tmp.join("interp.json"),
        Some("interp"),
    );
    let expected_hash = interp_report["framebuffer_hash"]
        .as_str()
        .expect("non-null pre-HUD hash")
        .to_string();
    assert_ne!(
        expected_hash, "fnv1a64:52bda05e66a4a325",
        "FP/SIMD pixel write must change framebuffer_hash from blank frame"
    );

    let manifest_path = tmp.join("fp_nan_simd.repro.json");
    let manifest = serde_json::json!({
        "schema": "ounce.repro.v1",
        "program_path": "fp_nan_simd.nro",
        "guest_args": [],
        "core": {
            "stop_kind": "clean_exit",
            "exit_code": 0,
            "frames_presented": 1,
            "framebuffer_hash": expected_hash
        }
    });
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    for tier in available_tiers() {
        let rep = tmp.join(format!("fp_{tier}.json"));
        run_repro_manifest_on_tier(&manifest_path, &rep, Some(tier));
    }

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn repro_manifest_verifies_svc_break_and_unhandled_svc_stop_kinds() {
    let tmp = std::env::temp_dir().join(format!("ounce_repro_svc_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // 1. svcBreak with result code 0x0000_0401 on the stack.
    let break_nro = nro(&[
        0xD100_43FF, // sub sp, sp, #16
        0x5280_8021, // movz w1, #0x0401
        0xB900_03E1, // str w1, [sp]
        0x5280_0000, // movz w0, #0
        0x9100_03E1, // mov x1, sp
        0xD280_0082, // movz x2, #4
        0xD400_04C1, // svc #0x26 (svcBreak)
    ]);
    fs::write(tmp.join("svc_break.nro"), &break_nro).unwrap();
    let break_manifest_path = tmp.join("svc_break.repro.json");
    let break_manifest = serde_json::json!({
        "schema": "ounce.repro.v1",
        "program_path": "svc_break.nro",
        "guest_args": [],
        "core": {
            "stop_kind": "svc_break",
            "exit_code": 13,
            "break_result": 0x0401,
            "frames_presented": 0,
            "framebuffer_hash": null
        }
    });
    fs::write(
        &break_manifest_path,
        serde_json::to_vec_pretty(&break_manifest).unwrap(),
    )
    .unwrap();

    // 2. Unhandled SVC #0x7f.
    let unhandled_nro = nro(&[0xD400_0FE1]);
    fs::write(tmp.join("unhandled_svc.nro"), &unhandled_nro).unwrap();
    let unhandled_manifest_path = tmp.join("unhandled_svc.repro.json");
    let unhandled_manifest = serde_json::json!({
        "schema": "ounce.repro.v1",
        "program_path": "unhandled_svc.nro",
        "guest_args": [],
        "core": {
            "stop_kind": "unhandled_svc",
            "exit_code": 14,
            "svc_id": 0x7f,
            "frames_presented": 0,
            "framebuffer_hash": null
        }
    });
    fs::write(
        &unhandled_manifest_path,
        serde_json::to_vec_pretty(&unhandled_manifest).unwrap(),
    )
    .unwrap();

    for tier in available_tiers() {
        run_repro_manifest_on_tier(
            &break_manifest_path,
            &tmp.join(format!("break_{tier}.json")),
            Some(tier),
        );
        run_repro_manifest_on_tier(
            &unhandled_manifest_path,
            &tmp.join(format!("unhandled_{tier}.json")),
            Some(tier),
        );
    }

    let _ = fs::remove_dir_all(&tmp);
}
