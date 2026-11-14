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
