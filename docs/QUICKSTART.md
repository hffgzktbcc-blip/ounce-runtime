# Quick start

GPL-3.0-or-later. This runtime is a separate program from MIT `ounce-core` and
MIT `ounce-hb`. Nothing here is copied into those repositories, and this tree
does not vendor them.

Rust 1.85 or newer. On Debian or Ubuntu:

```sh
sudo apt-get install -y libasound2-dev libudev-dev
```

```sh
git clone https://github.com/hffgzktbcc-blip/ounce-runtime.git
cd ounce-runtime
cargo build --release --locked
./target/release/ounce-runtime info
./target/release/ounce-runtime run-nro path/to/app.nro --present=headless
```

`native-jit` and `gpu-present` are on by default. `--no-default-features`
builds without them. A prebuilt archive from
[Releases](https://github.com/hffgzktbcc-blip/ounce-runtime/releases/latest)
is the latest tagged release (v0.6.0, which includes `--report-json`). Commands that need newer code say "main" and need a build from `main`.

## `ounce.run.v1`

```sh
./target/release/ounce-runtime run-nro path/to/app.nro \
  --present=headless --audio=null --report-json=run.json
```

`OUNCE_REPORT_JSON` sets the same path. Guest stdout stays on stdout.
`OUNCE_AUDIO=null` is the same as `--audio=null`.

The file is JSON with `"schema": "ounce.run.v1"` and `"engine": "ounce-runtime"`.
Beside the shared fields (`tier`, `silent_fallback`, `stop_kind`, `exit_code`,
`pc`, `svc_id`, `break_result`, `insns_executed`, `message`) this engine also
writes:

- `frames_presented` — frames the display presented, or `0` when there is no display.
- `framebuffer_hash` — `fnv1a64:` and 16 hex digits over the RGBA8 bytes of the
  last presented guest frame before the telemetry HUD (alpha forced to `0xFF`),
  otherwise `null` (when no frame was presented, or when a host GPU window
  rendered 3D draws outside deterministic mode).

`silent_fallback` is true when Tier-4 or NCE was requested and that tier did
not run. On this homebrew path a fault is reported as `cpu_error` with exit
code 12, and `opcode` and `fault_addr` are null. ounce-core splits faults into
exit 10, 11, or 12. Do not treat those reports as the same classification.

`--present=headless` attaches the headless presentation backend to `run` /
`run-nro` so `frames_presented` and pre-HUD `framebuffer_hash` are recorded
without opening a host window.

`--deterministic` and `OUNCE_DETERMINISTIC=1` or `true` run the homebrew kernel
clock and scheduler on fixed deterministic time (advancing 20 µs per SVC and
1 ms per 20,000-instruction slice, skipping timed waits virtually, fixing the
wall-clock date, omitting the HUD overlay, and keeping 3D draws on the
software rasterizer). `--deterministic` on a future ELF runner is a different
question; this binary's documented guest entry for homebrew is `run` / `run-nro`.

## Family tools

`ounce-hb doctor` and `setup-ounce` live in the MIT repo
[ounce-hb](https://github.com/hffgzktbcc-blip/ounce-hb). This runtime does not
embed them. `setup-ounce` installs a release archive only when the workflow
sets `install-runtime: 'true'`, and on ounce-hb `main` that download is not
checksum-checked. Keep the GPL binary a subprocess.
