use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn run_repro_manifest(manifest_path: &std::path::Path, report_path: &std::path::Path) {
    let raw = fs::read_to_string(manifest_path).expect("read repro manifest");
    let manifest: serde_json::Value =
        serde_json::from_str(&raw).expect("parse ounce.repro.v1 JSON");
    assert_eq!(manifest["schema"].as_str(), Some("ounce.repro.v1"));

    let program_rel = manifest["program_path"]
        .as_str()
        .expect("program_path string in ounce.repro.v1");
    let program = if std::path::Path::new(program_rel).is_absolute() {
        PathBuf::from(program_rel)
    } else {
        manifest_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(program_rel)
    };

    let guest_args: Vec<&str> = manifest["guest_args"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"));
    cmd.arg("run-nro")
        .arg(&program)
        .arg("--present=headless")
        .arg("--audio=null")
        .arg("--deterministic")
        .arg(format!("--report-json={}", report_path.display()));
    if !guest_args.is_empty() {
        cmd.arg("--").args(&guest_args);
    }

    let out = cmd.output().expect("spawn ounce-runtime");
    let report_raw = fs::read_to_string(report_path).expect("ounce.run.v1 report written");
    let report: serde_json::Value = serde_json::from_str(&report_raw).expect("parse ounce.run.v1");
    assert_eq!(report["schema"], "ounce.run.v1");

    if let Some(expected_kind) = manifest["core"]["stop_kind"].as_str() {
        assert_eq!(
            report["stop_kind"].as_str(),
            Some(expected_kind),
            "repro manifest {} stop_kind mismatch; stderr: {}",
            manifest_path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if let Some(expected_exit) = manifest["core"]["exit_code"].as_i64() {
        assert_eq!(
            report["exit_code"].as_i64(),
            Some(expected_exit),
            "repro manifest {} exit_code mismatch",
            manifest_path.display()
        );
    }
}

#[test]
fn ingests_ounce_repro_v1_manifest_and_directory_fixtures() {
    let tmp = std::env::temp_dir().join(format!("ounce_repro_ingest_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Minimal clean-exit NRO (`mov w0, #0; svc #7` -> svcExitProcess).
    let mut nro = vec![0u8; 0x88];
    nro[0..4].copy_from_slice(&0x1400_0020u32.to_le_bytes());
    nro[0x10..0x14].copy_from_slice(b"NRO0");
    nro[0x18..0x1c].copy_from_slice(&0x88u32.to_le_bytes());
    nro[0x24..0x28].copy_from_slice(&0x88u32.to_le_bytes());
    nro[0x80..0x88].copy_from_slice(&[0x00, 0x00, 0x80, 0x52, 0xE1, 0x00, 0x00, 0xD4]);
    let nro_path = tmp.join("minimal_exit.nro");
    fs::write(&nro_path, &nro).unwrap();

    let manifest_path = tmp.join("minimal_exit.repro.json");
    let manifest = serde_json::json!({
        "schema": "ounce.repro.v1",
        "program_path": "minimal_exit.nro",
        "program_bytes": nro.len(),
        "guest_args": [],
        "runtime": { "tier": "tier3", "stop_kind": "cpu_error", "exit_code": 12 },
        "core": { "stop_kind": "clean_exit", "exit_code": 0 }
    });
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    let report_path = tmp.join("report.json");
    run_repro_manifest(&manifest_path, &report_path);

    // Also ingest any checked-in `tests/repro/*.repro.json` regression bundles.
    let repro_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/repro");
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
            }
        }
    }

    let _ = fs::remove_dir_all(&tmp);
}
