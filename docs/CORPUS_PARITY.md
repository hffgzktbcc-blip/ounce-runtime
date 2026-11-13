# Corpus parity and measured performance

## 2026-11-13: Apple M3 head-to-head

On this host, ounce-runtime took **2.00–2.22× as long** as ounce-core for four short homebrew fixtures, while using **about 78% less peak RSS**. All four fixtures printed their expected `0x00000EEE` result and exited via `svcExitProcess` in every run. These are end-to-end CLI measurements, including startup, loader, host device initialization, guest execution, and teardown; they are not isolated CPU benchmarks.

### Environment and method

- Host: Apple M3, arm64, 16 GiB RAM; macOS 26.6.2 (25G83).
- Compiler: `rustc 1.98.0 (88d9e12ae 2026-08-18)`; Cargo release profile, checked-in lockfiles.
- Runtime: `578663e4ff152b6279b6db8984d67727ae18e460` (v0.4.0); default features `native-jit,gpu-present`.
- Core: `a052aee94f0935cc25ff62367fde98a2cf607198` (v0.8.1); default features (none); clean local checkout.
- Same runtime-repository fixture bytes supplied to both executables; SHA-256 hashes and exact argument arrays are in [measurements.json](benchmarks/2026-11-13/measurements.json).
- Release builds completed before timed samples. One excluded warmup, then five fresh process runs per fixture/engine; runs sequential, grouped core then runtime. No process overlap or intentional system load; host frequency/background activity uncontrolled. Reported medians and ranges are descriptive, not confidence intervals.
- Wall time: Python `perf_counter()` around `/usr/bin/time -l <binary> ...`; includes time-wrapper/process launch. RSS: macOS `maximum resident set size` in **bytes**, converted to MiB. It is not the VM's mapped-memory count or `peak memory footprint`.
- `OUNCE_CPU` was unset. Both homebrew commands use their default **instruction-at-a-time interpreter** path. Runtime `run --cpu=tier3|tier4|nce` does not route through `CpuEngine`: `run_vm_internal` calls `cpu.step`. Claiming homebrew tier speedups from those flags would be incorrect.

### Homebrew results

Instructions are each engine's own reported count; ranges preserve nondeterministic paths rather than normalizing them. Wall ratio is **core median / runtime median**; values below 1 mean runtime is slower.

| Fixture | Core instructions | Runtime instructions | Core wall ms (range) | Runtime wall ms (range) | Core RSS MiB | Runtime RSS MiB | Runtime speedup |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| hello | 9335 | 9320 | 54.51 (53.58–57.49) | 109.25 (108.89–123.28) | 280.44 | 62.14 | 0.499× |
| services | 5631–6236 | 5616–6221 | 49.64 (48.23–52.20) | 110.00 (108.49–125.51) | 280.73 | 62.41 | 0.451× |
| romfs | 24916 | 24901 | 51.60 (47.65–57.03) | 111.94 (109.18–113.62) | 280.62 | 62.31 | 0.461× |
| threads | 62912 | 62913 | 53.29 (52.90–56.49) | 109.41 (106.19–178.45) | 280.53 | 62.23 | 0.487× |

`hello` receives the required `fixture` guest argument. Services exercises SM/HID/shared-memory setup; romfs reads bundled paths; threads exercises mutex/condition-variable scheduling and TLS. All 48 executions (including warmups) passed their fixture marker, returned host status 0, and logged graceful process exit. Services counts vary by 605 instructions in both engines; the exact logs are retained. Runtime/core instruction counts differ even when results agree; do not interpret these as identical instruction traces.

Runtime unconditionally initializes `HostAudioSink` and the gamepad manager inside its VM timer, whereas core's headless path disables host audio. Runtime's VM summaries therefore include about 50 ms of host setup on these tiny workloads. The measured RSS difference is also affected by loader policy: summaries show runtime's 32 MiB override heap versus core's 256 MiB heap. Neither number isolates CPU-engine efficiency. No causal timing or heap experiment was performed.

### Synthetic architecture and native execution

The requested `cargo run --release -- test` passed **27 checks across 25 stages** ([log](benchmarks/2026-11-13/cargo-test.log)). `cargo run --release -- jit-selftest` passed on aarch64, generated function returning **42** ([log](benchmarks/2026-11-13/jit-selftest.log)). Core's stock harness passed 36 checks in all six runs; runtime passed 27 checks in all 18 repeated tier runs.

Stage 20 uses a fresh CPU engine for a short nested loop. Its reported throughput includes cold block construction/compilation, with no persistent code cache between processes. One warmup process is excluded; medians and full five-sample ranges follow. Engine counters confirm actual native execution rather than silent fallback.

| Engine / Stage 20 tier | Reported instructions | Median MIPS | MIPS range | Native execution / fallback |
| :--- | ---: | ---: | ---: | :--- |
| core / tier3 | 15304 | 95.23 | 92.92–95.28 | 5094 Super-Blocks fused |
| runtime / tier3 | 15403 | 70.94 | 54.09–90.52 | 5094 Super-Blocks fused |
| runtime / tier4 | 15403 | 9.86 | 9.00–23.96 | 4 native blocks compiled, 5037 native runs, 15207 native / 196 Tier-3 insns |
| runtime / nce | 15403 | 103.15 | 31.38–106.50 | 5 NCE blocks compiled, 5101 NCE runs, 15403 NCE / 0 fallback insns |

Within runtime's identical Stage 20 program, median NCE throughput was **1.45× Tier-3**; Tier-4 was **0.139× Tier-3**. This tiny cold run penalizes Cranelift compilation and exhibits wide variation; it does not establish steady-state game performance or a general ranking.

**The core/runtime stock Stage 20 programs differ:** core uses `0x54FFFF61` for the outer back-edge (preserves accumulator), runtime uses `0x54FFFF41` (resets accumulator). This explains the 99-instruction difference. Their throughput values are observations of each stock harness, not an identical-workload core/runtime CPU speedup. No cross-engine synthetic speedup is claimed.

| Runtime tier | Stock harness coverage | Comparable throughput available? |
| :--- | :--- | :--- |
| Interpreter / Tier-1 cache | Instruction-at-a-time VM stages and all four homebrew fixtures | End-to-end homebrew wall/RSS above; VM timing includes host setup |
| Tier-2 direct chaining | Stage 16: 65 instructions, 11 chained jumps, Fib(10)=55, exit SVC 7 | No isolated timer in stock harness; no MIPS invented |
| Tier-3 | Stage 20: X0=0xAAA, exit SVC 7 | Measured above |
| Tier-4 Cranelift | Stage 20: X0=0xAAA, exit SVC 7; native self-test returns 42 | Measured above; 15,207 native / 196 fallback instructions |
| Apple Silicon NCE | Stage 20: X0=0xAAA, exit SVC 7 | Measured above; all 15,403 instructions native |

### Reproduction and evidence

```sh
cargo run --release -- test
cargo run --release -- jit-selftest
cargo build --release --manifest-path /path/to/ounce-core/Cargo.toml
python3 scripts/benchmark_corpus.py --core /path/to/ounce-core/target/release/ounce-core --output /tmp/corpus-benchmark
python3 scripts/benchmark_tiers.py --core /path/to/ounce-core/target/release/ounce-core --output /tmp/tier-benchmark
```

Both scripts default to one warmup and five measured repetitions. The homebrew commands are `ounce-core run-nro <nro> --present=headless` and `ounce-runtime run <nro> --present=headless` (`hello` additionally receives `fixture`). [Raw evidence](benchmarks/2026-11-13/) contains every run's stdout/stderr, time counters, command arrays, guest hashes, and [tier measurements](benchmarks/2026-11-13/tiers.json). Synthetic MIPS is read from engine counters, never derived from whole-harness duration.

### Remaining coverage

This establishes parity for four checked-in fixtures, not the separate 50-program `ounce-hb` corpus or outside switch-examples corpus. A common timed loop for Interpreter/Tier-2, matching core/runtime Stage 20 programs, homebrew block-engine wiring in runtime, steady-state native measurements, and core native-tier comparisons remain future work. No retail software, firmware, or private keys were used.
