# Changelog

All notable changes to `ounce-runtime` are documented in this file. The [latest GitHub release](https://github.com/hffgzktbcc-blip/ounce-runtime/releases/latest) is the source for what is actually published. Dated sections below describe historical releases; new work belongs under `Unreleased` until publication.

## Unreleased

## [0.7.0] — 2026-11-20 (published)

### Added
- **Pre-HUD `framebuffer_hash` and deterministic execution (#28, closes #19)**:
  - Hash pre-HUD guest framebuffer pixels (`fnv1a64:...`, alpha forced to `0xFF`) in `DisplayServer` before drawing the telemetry overlay, omit the HUD overlay under `--present=headless` or deterministic mode, and return `null` when a host GPU backend renders 3D draws outside deterministic mode.
  - Attach the headless `DisplayServer` on `run` / `run-nro` when `--present=headless` is selected without `--gui`.
  - Wire `--deterministic` / `OUNCE_DETERMINISTIC=1` (`true`) to a fixed virtual kernel clock (`+20 µs` per SVC, `+1 ms` per 20,000-instruction slice, virtual timed waits, fixed UTC timestamp `1_793_000_000`), software 3D rasterization, and disabled host HID/audio polling.
  - Added integration suite `tests/run_report_framebuffer.rs` and unit tests in `display.rs`, `scheduler.rs`, and `svc_tests.rs`.
- **`ounce.trace.v1` bounded step trace and `ounce.repro.v1` regression suite (#29, closes #20)**:
  - Added `OUNCE_TRACE_SUMMARY=<path>` and `OUNCE_TRACE_STEPS=<n>` (default `1024`, clamped to `1..=65536`) emitting `ounce.trace.v1` step checkpoints (`i`, `pc`, `opcode`, changed `regs`) plus summary stop and final register fields on every stop.
  - Checked in metadata-only `tests/repro/*.repro.json` fixtures (`hello`, `threads`, `deko3d`) and expanded `tests/repro_ingestion.rs` with multi-tier (`interp`, `tier3`, `tier4`, `nce`) regression tests for pre-HUD `framebuffer_hash`, FP NaN canonicalization / SIMD consistency, `svc_break`, and `unhandled_svc`.
- **Q3 supply-chain hardening (#30)**:
  - Pinned all 34 remote GitHub Actions `uses:` references across `.github/workflows/*.yml` to 40-character commit SHAs and added `scripts/check_action_pins.py` (with unit tests) enforced in CI.
  - Added pure-stdlib SPDX 2.3 JSON SBOM generator `scripts/generate_sbom.py` (with unit tests) wired into `.github/workflows/release.yml`, bundling `ounce-runtime.spdx.json` inside every release archive, uploading standalone `<archive>.spdx.json` files, and covering both in `SHA256SUMS`.
- **Additional `ounce.run.v1` fields (#25, #26, #27)**:
  - Report `lr`, `sp`, `guest_exit_status`, `audio_frames_mixed`, and `audio_hash` in `ounce.run.v1`, documented in `docs/QUICKSTART.md`.

### Changed
- **Repository hygiene and CI (#17, #21, #22, #23, #24)**:
  - Added `AGENTS.md` and `docs/README.md`, CI job timeouts, PR concurrency cancellation, docs-only PR fast-path, issue claim/nudge workflows, and anonymized host paths in benchmark logs.

## [0.6.0] — 2026-11-16

- Initial tagged release with `--report-json` (`ounce.run.v1`), `--present=headless`, `--audio=null`, Tier-3/Tier-4/NCE CPU tiers, and 25-stage built-in verification harness.
