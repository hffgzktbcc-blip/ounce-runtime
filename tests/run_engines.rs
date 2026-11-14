//! CLI coverage uses the same redistributable homebrew bytes as corpus benchmarks.
use std::process::Command;

#[test]
fn bundled_homebrew_uses_requested_engine() {
    let mut tiers = vec![("interp", "Interpreter"), ("tier3", "Tier-3")];
    if cfg!(feature = "native-jit") && cfg!(target_arch = "aarch64") {
        tiers.extend([("tier4", "Tier-4"), ("nce", "NCE")]);
    }
    for (tier, name) in tiers {
        for fixture in ["hello", "services", "romfs", "threads"] {
            let path = format!(
                "{}/tests/fixtures/nro/{fixture}/{fixture}.nro",
                env!("CARGO_MANIFEST_DIR")
            );
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"));
            cmd.args(["run", &path, "--present=headless", &format!("--cpu={tier}")]);
            if fixture == "hello" {
                cmd.arg("fixture");
            }
            let result = cmd.output().unwrap();
            let log = String::from_utf8_lossy(&result.stdout);
            assert!(result.status.success(), "{tier}/{fixture}: {log}");
            assert!(log.contains("result=0x00000EEE"), "{tier}/{fixture}: {log}");
            assert!(
                log.contains("Process exited gracefully"),
                "{tier}/{fixture}: {log}"
            );
            assert!(log.contains(&format!("[CPU] Engine: {name}")), "{log}");
            let counter = match tier {
                "tier3" => Some("Super-Blocks fused"),
                "tier4" => Some("native runs"),
                "nce" => Some("NCE runs"),
                _ => None,
            };
            if let Some(label) = counter {
                let line = log.lines().find(|line| line.contains(label)).unwrap();
                let prefix = line.split_once(label).unwrap().0;
                let count: usize = prefix.split_whitespace().last().unwrap().parse().unwrap();
                assert!(count > 0, "{log}");
            }
            assert!(!log.contains("[Audio]"), "headless opened audio: {log}");
        }
    }
}

#[test]
fn environment_selects_engine_and_cli_overrides_it() {
    let path = format!(
        "{}/tests/fixtures/nro/services/services.nro",
        env!("CARGO_MANIFEST_DIR")
    );
    for (flag, expected) in [(None, "Interpreter"), (Some("--cpu=tier3"), "Tier-3")] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"));
        cmd.env("OUNCE_CPU", "interp")
            .args(["run", &path, "--present=headless"]);
        if let Some(flag) = flag {
            cmd.arg(flag);
        }
        let output = cmd.output().unwrap();
        let log = String::from_utf8_lossy(&output.stdout);
        assert!(log.contains(&format!("[CPU] Engine: {expected}")), "{log}");
        assert!(log.contains("result=0x00000EEE"), "{log}");
    }
}

fn synthetic_fault_nro(insn: u32) -> Vec<u8> {
    let mut nro = vec![0u8; 0x84];
    let total_len = (nro.len() as u32).to_le_bytes();
    nro[0x00..0x04].copy_from_slice(&0x1400_0020u32.to_le_bytes()); // b #0x80
    nro[0x10..0x14].copy_from_slice(b"NRO0");
    nro[0x18..0x1C].copy_from_slice(&total_len);
    nro[0x24..0x28].copy_from_slice(&total_len);
    nro[0x80..0x84].copy_from_slice(&insn.to_le_bytes());
    nro
}

fn unique_temp_path(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "ounce_runtime_{}_{}_{}",
        std::process::id(),
        nanos,
        name
    ))
}

#[test]
fn run_nro_subcommand_matches_run_across_engines() {
    let services_path = format!(
        "{}/tests/fixtures/nro/services/services.nro",
        env!("CARGO_MANIFEST_DIR")
    );
    let tier4_label = if cfg!(feature = "native-jit") {
        "Tier-4"
    } else {
        "Tier-3"
    };
    let nce_label = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "NCE"
    } else {
        tier4_label
    };
    for (flag, expected_label) in [
        ("interp", "Interpreter"),
        ("tier3", "Tier-3"),
        ("tier4", tier4_label),
        ("nce", nce_label),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"))
            .args([
                "run-nro",
                &services_path,
                "--present=headless",
                &format!("--cpu={flag}"),
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "run-nro --cpu={flag} failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
        assert!(
            stdout.contains(&format!("[CPU] Engine: {expected_label}")),
            "{stdout}"
        );
        assert!(stdout.contains("result=0x00000EEE"), "{stdout}");
    }

    let hello_path = format!(
        "{}/tests/fixtures/nro/hello/hello.nro",
        env!("CARGO_MANIFEST_DIR")
    );
    let output = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"))
        .args([
            "run-nro",
            &hello_path,
            "--present=headless",
            "--cpu=tier3",
            "fixture",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "hello.nro via run-nro failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
}

#[test]
fn run_nro_exits_nonzero_on_guest_fault_or_missing_file() {
    let bad_nro = unique_temp_path("fault.nro");
    std::fs::write(&bad_nro, synthetic_fault_nro(0x0000_0000)).unwrap();

    for engine in ["interp", "tier3"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"))
            .args([
                "run-nro",
                bad_nro.to_str().unwrap(),
                "--present=headless",
                &format!("--cpu={engine}"),
            ])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "expected non-zero exit for faulting NRO on {engine}, got {:?}",
            output.status
        );
    }
    let _ = std::fs::remove_file(&bad_nro);

    let missing = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"))
        .args(["run-nro", "/nonexistent/missing.nro", "--present=headless"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
}

#[test]
fn fuzz_replay_exits_zero_on_match_and_nonzero_on_mismatch() {
    let zeros32 = vec!["00000000000000000000000000000000"; 32].join(" ");
    let mut in_x = vec!["0"; 31];
    in_x[1] = "ffffffffffffffff";
    let mut ok_x = vec!["0"; 31];
    ok_x[1] = "ffffffffffffffff";
    let mut bad_x = vec!["0"; 31];
    bad_x[0] = "1";
    bad_x[1] = "ffffffffffffffff";

    let ok_line = format!(
        "T ba020020 2 {} {} => 6 0 {} {}\n",
        in_x.join(" "),
        zeros32,
        ok_x.join(" "),
        zeros32
    );
    let bad_line = format!(
        "T ba020020 2 {} {} => 6 0 {} {}\n",
        in_x.join(" "),
        zeros32,
        bad_x.join(" "),
        zeros32
    );

    let ok_path = unique_temp_path("ok.trace");
    let bad_path = unique_temp_path("bad.trace");
    std::fs::write(&ok_path, ok_line).unwrap();
    std::fs::write(&bad_path, bad_line).unwrap();

    let ok_out = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"))
        .args(["fuzz-replay", "--engine", "interp", ok_path.to_str().unwrap()])
        .output()
        .unwrap();
    let ok_stdout = String::from_utf8_lossy(&ok_out.stdout);
    assert!(ok_out.status.success(), "{ok_stdout}");
    assert!(ok_stdout.contains("exact match (X,V,NZCV,QC): 1"), "{ok_stdout}");
    assert!(ok_stdout.contains("emulator other errors: 0"), "{ok_stdout}");
    assert!(ok_stdout.contains("MISMATCH: 0"), "{ok_stdout}");

    let bad_out = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"))
        .args(["fuzz-replay", "--engine", "interp", bad_path.to_str().unwrap()])
        .output()
        .unwrap();
    let bad_stdout = String::from_utf8_lossy(&bad_out.stdout);
    assert!(!bad_out.status.success(), "{bad_stdout}");
    assert!(bad_stdout.contains("MISMATCH: 1"), "{bad_stdout}");

    let _ = std::fs::remove_file(&ok_path);
    let _ = std::fs::remove_file(&bad_path);
}
