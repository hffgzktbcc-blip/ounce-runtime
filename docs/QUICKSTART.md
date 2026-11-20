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
[the latest published release](https://github.com/hffgzktbcc-blip/ounce-runtime/releases/latest)
shows the current tag and downloadable archives. Check its notes before assuming a command on `main` exists in the release; `--report-json` first shipped in v0.6.0.

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

Starting with the **published v0.7.0 release** (not the older v0.6.0 archive), it also writes:

- `lr` and `sp` — the link register (`x30`) and stack pointer at the stop. They
  are set exactly when `pc` is, and `null` otherwise.
- `guest_exit_status` — always `null` here. It is a `run-elf` field in ounce-core's
  schema; homebrew has no exit status.
- `audio_frames_mixed` — stereo frames of audio the guest mixed through `audout`
  or `audren`. It counts as audio is mixed, so a run with `--audio=null` reports
  the same value as one with a sound device. `0` when the guest plays no audio.
- `audio_hash` — `fnv1a64:` and 16 hex digits over every mixed sample (little-endian
  `i16`), or `null` when no frames were mixed. When both `audren` and `audout` were
  used it is the hash of the two digests, `audren` first.

The meanings follow ounce-core's [`ounce.run.v1` schema](https://github.com/hffgzktbcc-blip/ounce-core/blob/main/docs/OUNCE_RUN_V1_SCHEMA.md).
On the 11 shared fixtures ounce-core and this runtime agree on `schema`,
`stop_kind`, `exit_code`, `frames_presented` and `framebuffer_hash`, and the audio
hash is identical on the `audren` fixture. `insns_executed` still differs by a
small constant (15 on most fixtures), cause not yet explained.

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

`OUNCE_TRACE_SUMMARY=<path>` writes a bounded `ounce.trace.v1` JSON trace
summary on every stop (clean exit, fault, `svcBreak`, unhandled SVC, or budget),
recording up to `OUNCE_TRACE_STEPS` retired instruction checkpoints (default
`1024`, clamped to `1..=65536`) alongside summary stop and final register fields
for cross-engine comparison (`ounce-hb diff-trace` / `ounce-core trace-diff`).

## Family tools

`ounce-hb doctor` and `setup-ounce` live in the MIT repo
[ounce-hb](https://github.com/hffgzktbcc-blip/ounce-hb). This runtime does not
embed them. `setup-ounce` installs a release archive only when the workflow
sets `install-runtime: 'true'`, and on ounce-hb `main` that download is not
checksum-checked. Keep the GPL binary a subprocess.
