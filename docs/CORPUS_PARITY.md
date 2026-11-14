# Corpus parity and measured performance

## 2026-11-14: Headless devices and homebrew engine selection

`run` now uses `CpuEngine`: `--cpu=interp|tier3|tier4|nce` overrides `OUNCE_CPU`, with Tier-3 as the default. Logs name the actual engine and its counters, including native versus fallback instructions. Unavailable native engines retain their explicit diagnostic and report the actual fallback engine. A display backed by `headless`, or no display, constructs neither the host audio sink nor the host gamepad manager; interactive backends retain both. Slices end at scheduler quantum boundaries and instruction limits. Tier-4's native linking uses a soft block budget, so this adapter reserves a maximum block and executes short tails without linking or through Tier-3 to keep those boundaries exact.

### Method and provenance

Runtime measured implementation: `b80503426cefc1468c529757fecf9d4b25cf1c63` (base main `8f00070b1928f4e56596ee2c28a64f6fa08f1595`); core main: `1e9c4d6b1f8905377ffd68f97c9574ac6532820a`. Both release builds used `--locked`, runtime default features (`native-jit,gpu-present`), and the same bundled redistributable NRO bytes. Core source was unchanged. Host/compiler details and build commands are in [environment.json](benchmarks/2026-11-14/environment.json).

One excluded warmup and five measured fresh processes per fixture/engine; `/usr/bin/time -l` and Python wall timing include loader, guest work and teardown. There are **120/120 successful fixture executions** including warmups, each with `0x00000EEE` and graceful exit. The scripts ran sequentially, but independent Orca workers were compiling/testing on this host: background CPU and memory pressure were uncontrolled. These are noisy descriptive measurements, **not an isolated before/after speedup experiment**. RSS is the macOS reported resident-set peak in bytes, converted to MiB. The previous day's results remain historical below; the first measurement pass is retained under [initial/](benchmarks/2026-11-14/initial/).

### Homebrew release measurements

Each cell is median wall milliseconds (five-sample minimum–maximum); full instruction counts, RSS, actual engine, counters, fixture hashes and exact commands are in [measurements.json](benchmarks/2026-11-14/measurements.json).

| Fixture | Core interpreter | Runtime interp | Runtime tier3 | Runtime tier4 | Runtime nce |
| :--- | ---: | ---: | ---: | ---: | ---: |
| hello | 133.67 (109.17–167.17) | 38.79 (35.80–44.99) | 34.65 (30.34–39.95) | 50.29 (45.55–70.62) | 50.61 (38.77–61.81) |
| services | 125.05 (104.08–273.97) | 31.81 (29.70–33.24) | 34.71 (25.50–109.52) | 32.89 (30.05–38.79) | 34.81 (31.20–37.92) |
| romfs | 116.77 (113.11–124.80) | 35.78 (30.96–47.87) | 32.56 (30.20–43.62) | 49.64 (47.27–57.96) | 68.83 (65.39–70.76) |
| threads | 128.63 (120.29–171.53) | 41.17 (33.48–42.19) | 37.99 (35.12–39.62) | 71.92 (69.46–79.17) | 74.23 (69.31–77.35) |

Runtime peak-RSS medians range from 49.55–55.88 MiB; core from 280.44–280.78 MiB. Heap policy still differs (32 versus 256 MiB). Runtime hello/romfs/threads report 9,320/24,901/62,913 instructions in all tiers. Services reports 5,616 or 6,221 across runtime samples; core services reports 5,631 or 6,236. Instruction counts are engine observations rather than identical instruction traces.

All native fixture runs have positive native execution counters, but substantial fallback remains. For example, hello Tier-4 executes 4,292 native / 5,028 fallback instructions; NCE executes 296 native / 9,024 fallback. Threads executes 45,329 native / 17,584 fallback in Tier-4 and 15,491 native / 47,422 fallback in NCE. Selecting a native engine does not imply every instruction runs natively.

### Synthetic tiers

All **24/24** stock architecture runs returned success. Stage 20 still uses different core/runtime programs as explained in the historical section; no identical-workload cross-repo throughput ratio is claimed. Cold compilation is included and caches do not persist between processes. [Raw tier measurements](benchmarks/2026-11-14/tiers.json):

| Engine / tier | Median MIPS | Range | Counters (representative measured run) |
| :--- | ---: | ---: | :--- |
| core / tier3 | 49.92 | 46.04–53.93 | 5094 Super-Blocks fused |
| runtime / tier3 | 52.49 | 2.90–54.19 | 5094 Super-Blocks fused |
| runtime / tier4 | 12.57 | 5.44–15.05 | 4 native blocks compiled, 5037 native runs, 15207 native / 196 Tier-3 insns |
| runtime / nce | 51.85 | 41.90–63.15 | 5 NCE blocks compiled, 5101 NCE runs, 15403 NCE / 0 fallback insns |

Interactive device construction and polling paths remain in place for active window backends; physical-controller and audible-output testing were not performed in this headless worker.

### Verification

`cargo test --locked` and `cargo test --locked --features native-jit` each passed **467 unit tests + 2 CLI integration tests**, with 2 ignored unit tests, on implementation `c9c6bb9` before the final lazy-display initialization fix. The final measured commit also passed `cargo test --release --locked` and `cargo test --release --locked --features native-jit`, with identical counts. CLI coverage checks all four fixtures in interp/tier3/tier4/NCE, positive execution counters, environment selection and CLI precedence; unit coverage checks exact slice budgets and SVC instruction counts.

`cargo clippy --locked --all-targets --features native-jit` completed with warnings. `cargo fmt --check` exits 1 due to broad repository formatting drift; the unchanged main-branch `main.rs` independently fails rustfmt as recorded in [format-check.txt](benchmarks/2026-11-14/format-check.txt). No repository-wide formatting rewrite was included. Exact commands, outcomes and logs are retained in [verification.json](benchmarks/2026-11-14/verification.json) and its neighboring log files.

### Conformance: explicitly unsupported, not passing

The new `fuzz-replay --engine <tier> <trace>` adapter reads the versioned single-instruction register-state contract and invokes the requested engine with an exact one-instruction budget. Reports retain actual engine counters: short budgets can legitimately use that engine's interpreter fallback, so these reports do not establish native opcode conformance. The suite directory (not its JSON filename) is the runner's `--suite` argument.

All four engines report **0/12 full passes**, with exit status 1 per case. FPSR/QC is not modeled by runtime and therefore cannot be checked: the adapter explicitly records an unsupported comparison as `emulator other errors: 1` and never claims an exact match. Partial X/V/NZCV comparison matches 9/12 cases; `default-nan-mul2d`, `sqadd-saturate-16b`, and `adcs-carry-wrap` disagree in every engine. These discrepancies remain open; this is not ISA certification. Exact suite hashes, stdout/stderr, counters and case results are retained in [interp](benchmarks/2026-11-14/conformance-interp.json), [tier3](benchmarks/2026-11-14/conformance-tier3.json), [tier4](benchmarks/2026-11-14/conformance-tier4.json) and [nce](benchmarks/2026-11-14/conformance-nce.json).

```sh
cargo build --release --locked
cargo build --release --locked --manifest-path /Users/nathanaelgovender/orca/ounce-core/Cargo.toml
python3 scripts/benchmark_corpus.py --core /Users/nathanaelgovender/orca/ounce-core/target/release/ounce-core --output docs/benchmarks/2026-11-14
python3 scripts/benchmark_tiers.py --core /Users/nathanaelgovender/orca/ounce-core/target/release/ounce-core --output docs/benchmarks/2026-11-14
# Repeat for interp, tier3, tier4, nce with a fresh report filename:
python3 /Users/nathanaelgovender/orca/ounce-core/scripts/run_conformance.py --suite /Users/nathanaelgovender/orca/ounce-core/conformance/arm64-v0.2.0 --report /tmp/runtime-conformance-tier3.json -- target/release/ounce-runtime fuzz-replay --engine tier3
```

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
