//! `ounce.run.v1` pins pre-HUD `framebuffer_hash` under `--present=headless` / `--deterministic` /
//! `OUNCE_DETERMINISTIC=1` and verifies deterministic clock/scheduler behaviour.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// A minimal NRO: header branches to 0x80, where `code` starts at guest 0x8000_0080.
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

fn run_nro_with(
    tag: &str,
    program_bytes: &[u8],
    args: &[&str],
    envs: &[(&str, &str)],
) -> (String, serde_json::Value) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("ounce-fb-report-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let program = dir.join("p.nro");
    let report = dir.join("report.json");
    fs::write(&program, program_bytes).unwrap();
    let out = run_existing_with(&program, &report, args, envs);
    let json = serde_json::from_str(&fs::read_to_string(&report).expect("report written")).unwrap();
    let _ = fs::remove_dir_all(&dir);
    (out, json)
}

fn run_existing_with(
    program: &std::path::Path,
    report: &std::path::Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ounce-runtime"));
    cmd.arg("run-nro")
        .arg(program)
        .args(args)
        .arg(format!("--report-json={}", report.display()));
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("spawn ounce-runtime");
    assert!(
        output.status.success(),
        "ounce-runtime exited with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

const SVC_VSYNC: u32 = 0xD400_0301; // svc #0x18
const MOVZ_W0_0: u32 = 0x5280_0000;
const SVC_EXIT_PROCESS: u32 = 0xD400_00E1; // svc #7

#[test]
fn blank_headless_frame_hashes_pre_hud_pixels_consistently() {
    let code = [SVC_VSYNC, MOVZ_W0_0, SVC_EXIT_PROCESS];
    let bytes = nro(&code);

    for (tag, args, envs) in [
        (
            "headless",
            &["--present=headless", "--audio=null"][..],
            &[][..],
        ),
        (
            "headless-cli-det",
            &["--present=headless", "--audio=null", "--deterministic"][..],
            &[][..],
        ),
        (
            "headless-env-det",
            &["--present=headless", "--audio=null"][..],
            &[("OUNCE_DETERMINISTIC", "1")][..],
        ),
    ] {
        let (_, r) = run_nro_with(tag, &bytes, args, envs);
        assert_eq!(r["frames_presented"], 1, "{tag}: {r}");
        assert_eq!(
            r["framebuffer_hash"], "fnv1a64:52bda05e66a4a325",
            "{tag}: {r}"
        );
    }
}

#[test]
fn pixels_in_hud_band_are_hashed_pre_hud_with_opaque_alpha() {
    // Write 0x11223344 (R=0x44, G=0x33, B=0x22, A=0x11) to pixel (0, 0) at 0x9000_0000,
    // which sits inside the 20px HUD overlay region, then present and exit.
    let code = [
        0xD280_0000, // movz x0, #0
        0xF2B2_0000, // movk x0, #0x9000, lsl #16
        0x5286_6881, // movz w1, #0x3344
        0x72A2_2441, // movk w1, #0x1122, lsl #16
        0xB900_0001, // str w1, [x0]
        SVC_VSYNC,
        MOVZ_W0_0,
        SVC_EXIT_PROCESS,
    ];
    let bytes = nro(&code);

    let mut tiers = vec![("tier3", "--cpu=tier3"), ("interp", "--cpu=interp")];
    if cfg!(feature = "native-jit") && cfg!(target_arch = "aarch64") {
        tiers.extend([("tier4", "--cpu=tier4"), ("nce", "--cpu=nce")]);
    }

    for (tag, tier) in tiers {
        let (_, r_cli) = run_nro_with(
            &format!("pixel-cli-{tag}"),
            &bytes,
            &[
                "--present=headless",
                "--audio=null",
                "--deterministic",
                tier,
            ],
            &[],
        );
        let (_, r_env) = run_nro_with(
            &format!("pixel-env-{tag}"),
            &bytes,
            &["--present=headless", "--audio=null", tier],
            &[("OUNCE_DETERMINISTIC", "1")],
        );
        for r in [&r_cli, &r_env] {
            assert_eq!(r["frames_presented"], 1, "{tag}: {r}");
            assert_eq!(
                r["framebuffer_hash"], "fnv1a64:d1777609f9ade9b2",
                "{tag}: {r}"
            );
        }
    }
}

#[test]
fn non_presenting_fixtures_report_zero_frames_and_null_hash_even_when_slow() {
    let dir = std::env::temp_dir().join(format!("ounce-non-presenting-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    for (name, extra_args) in [("hello", vec!["--", "fixture"]), ("deko3d", vec![])] {
        let program = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/nro/{name}/{name}.nro"));
        let report = dir.join(format!("{name}.json"));
        let mut args = vec!["--present=headless", "--audio=null", "--deterministic"];
        args.extend(extra_args);
        run_existing_with(&program, &report, &args, &[]);
        let r: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
        assert_eq!(r["frames_presented"], 0, "{name}: {r}");
        assert!(r["framebuffer_hash"].is_null(), "{name}: {r}");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn deterministic_clock_advances_fixed_ticks_for_svcs_sleep_and_slices() {
    let mut words = vec![
        0xD101_03FF, // sub sp, sp, #64
        0xD400_03C1, // svc #0x1e (tick1)
        0xAA00_03F3, // mov x19, x0
        0xD400_03C1, // svc #0x1e (tick2)
        0xAA00_03F4, // mov x20, x0
        0xD288_4800, // movz x0, #0x4240
        0xF2A0_01E0, // movk x0, #0x000f, lsl #16 (1_000_000 ns)
        0xD400_0161, // svc #0x0b (svcSleepThread)
        0xD400_03C1, // svc #0x1e (tick3)
        0xAA00_03F5, // mov x21, x0
        0xD28C_3506, // movz x6, #25000 (50,000-insn loop across two 20,000-insn slices)
        0xF100_04C6, // subs x6, x6, #1
        0x54FF_FFE1, // b.ne -4
        0xD400_03C1, // svc #0x1e (tick4)
        0xAA00_03F6, // mov x22, x0
    ];

    for reg in [19u32, 20, 21, 22] {
        words.extend([
            0xAA00_03F2 | (reg << 16), // mov x18, x_reg
            0x9100_03E5,               // mov x5, sp
            0xD280_0206,               // movz x6, #16
            0x93D2_F252,               // ror x18, x18, #60
            0x9240_0E42,               // and x2, x18, #0xf
            0xF100_285F,               // cmp x2, #10
            0x9100_C043,               // add x3, x2, #0x30
            0x9100_DC44,               // add x4, x2, #0x37
            0x9A84_B062,               // csel x2, x3, x4, lt
            0x3800_14A2,               // strb w2, [x5], #1
            0xF100_04C6,               // subs x6, x6, #1
            0x54FF_FF01,               // b.ne loop
            0x5280_0142,               // movz w2, #10
            0x3900_00A2,               // strb w2, [x5]
            0x9100_03E0,               // mov x0, sp
            0xD280_0221,               // movz x1, #17
            0xD400_04E1,               // svc #0x27 (svcOutputDebugString)
        ]);
    }
    words.extend([MOVZ_W0_0, SVC_EXIT_PROCESS]);

    let bytes = nro(&words);
    let (out_cli, r_cli) = run_nro_with(
        "tick-cli",
        &bytes,
        &["--present=headless", "--audio=null", "--deterministic"],
        &[],
    );
    let (out_env, r_env) = run_nro_with(
        "tick-env",
        &bytes,
        &["--present=headless", "--audio=null"],
        &[("OUNCE_DETERMINISTIC", "true")],
    );

    for (tag, out, r) in [("cli", &out_cli, &r_cli), ("env", &out_env, &r_env)] {
        let ticks: Vec<&str> = out.lines().filter(|l| l.len() == 16).collect();
        assert_eq!(
            ticks,
            vec![
                "0000000000004B00",
                "0000000000004B00",
                "0000000000005100",
                "000000000000E880",
            ],
            "{tag}: stdout:\n{out}"
        );
        assert_eq!(r["insns_executed"], 50624, "{tag}: {r}");
    }
    assert_eq!(r_cli["insns_executed"], r_env["insns_executed"]);
}

#[test]
fn multithreaded_fixture_is_deterministic_across_runs() {
    let dir = std::env::temp_dir().join(format!("ounce-det-threads-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let program =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nro/threads/threads.nro");
    let r1_path = dir.join("r1.json");
    let r2_path = dir.join("r2.json");

    let out1 = run_existing_with(
        &program,
        &r1_path,
        &["--present=headless", "--audio=null", "--deterministic"],
        &[],
    );
    let out2 = run_existing_with(
        &program,
        &r2_path,
        &["--present=headless", "--audio=null"],
        &[("OUNCE_DETERMINISTIC", "1")],
    );

    assert!(out1.contains("ounce-threads result=0x00000EEE"), "{out1}");
    assert!(out2.contains("ounce-threads result=0x00000EEE"), "{out2}");

    let r1: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&r1_path).unwrap()).unwrap();
    let r2: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&r2_path).unwrap()).unwrap();
    assert_eq!(r1["stop_kind"], "clean_exit");
    assert_eq!(r1["insns_executed"], r2["insns_executed"]);
    assert_eq!(r1["frames_presented"], 0);
    assert!(r1["framebuffer_hash"].is_null());

    let _ = fs::remove_dir_all(&dir);
}
