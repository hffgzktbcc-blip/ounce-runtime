//! `ounce.run.v1` reports the audio the guest mixed. The expected values for the shared `audren` fixture are what
//! ounce-core writes for the same program, so this pins agreement between the two tools; programs that mix nothing
//! report `0` and `null`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn report(fixture: &str) -> serde_json::Value {
    let program = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/nro/{fixture}/{fixture}.nro"));
    let dir = std::env::temp_dir().join(format!(
        "ounce-audio-report-{}-{fixture}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let out = dir.join("report.json");
    Command::new(env!("CARGO_BIN_EXE_ounce-runtime"))
        .arg("run-nro")
        .arg(&program)
        .args(["--present=headless", "--audio=null", "--deterministic"])
        .arg(format!("--report-json={}", out.display()))
        .output()
        .expect("spawn ounce-runtime");
    let json = serde_json::from_str(&fs::read_to_string(&out).expect("report written")).unwrap();
    let _ = fs::remove_dir_all(&dir);
    json
}

#[test]
fn audren_fixture_reports_the_same_audio_as_ounce_core() {
    let r = report("audren");
    assert_eq!(r["audio_frames_mixed"], 2640);
    assert_eq!(r["audio_hash"], "fnv1a64:df75b810908350a5");
}

#[test]
fn a_program_that_mixes_nothing_reports_zero_and_null() {
    let r = report("hello");
    assert_eq!(r["audio_frames_mixed"], 0);
    assert!(r["audio_hash"].is_null());
}
