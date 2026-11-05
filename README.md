# ounce-runtime

An experimental AArch64 / Horizon-style research runtime written in Rust: a CPU interpreter with optional JIT tiers, models of kernel and system services, software and `wgpu` graphics, audio and input.

It is a research and interoperability project. It is **unaffiliated** with Nintendo, NVIDIA or any other company; their names remain their owners' trademarks and appear only to describe the interfaces being modelled.

## What it is, and is not

- It runs programs you build or supply yourself (synthetic test programs and open-source homebrew) through CPU, kernel and service models.
- It does **not** include, load or decrypt protected content. It has no key handling.
- It does not ship firmware, keys, or game data. Do not add any (see [CONTRIBUTING.md](CONTRIBUTING.md)).
- Many parts are partial. Behaviour that is not covered by a test should be treated as unverified.

## Build and test

Requires Rust 1.85 or newer (edition 2024).

```bash
cargo build --release
cargo test --release
cargo run --release -- test      # synthetic stage harness
cargo run --release -- info      # modelled hardware and engine status
```

Optional features: `gpu-present` (host GPU presentation through `wgpu`) and `native-jit` (Tier-4 recompiler).

## Status

Known failing tests are tracked in the issue list. Homebrew fixture tests require the fixtures in `tests/fixtures`, which are added back once their provenance is recorded (see [docs/PROVENANCE.md](docs/PROVENANCE.md)).

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE) and [THIRD_PARTY.md](THIRD_PARTY.md).
