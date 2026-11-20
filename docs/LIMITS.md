# Ounce-Runtime: what works, and what is not verified

**Ounce-Runtime experiments with running supported ARM64 homebrew and rendering its output.** It is a research runtime, not a promise that arbitrary programs will work.

## What you can try

- **Run your own unencrypted NRO** built from public open-source homebrew tools. The [quick start](QUICKSTART.md) shows the current command and how to write a machine-readable `ounce.run.v1` report.
- **Experiment with execution modes**, such as the interpreter, Tier-3, Tier-4 JIT and supported native paths. Results depend on host architecture, build features and program inputs.
- **Inspect graphics and controller integration** where the host backend supports them. A successfully opened window or rendered host frame does not by itself prove correct guest output.
- **Validate existing evidence files** with [Ounce-Verify](verify/README.md). The validator checks record structure and specific registered oracles; it does not execute a guest or establish provenance.

## What you must not assume

- **No general application compatibility:** A passing sample is evidence for that exact binary, revision, environment and inputs, not arbitrary ARM64, Linux or homebrew programs.
- **No retail-content support:** Do not supply games, console keys, proprietary firmware, encrypted archives or DRM-circumvention material.
- **No universal hardware parity:** Engine agreement and CI are not an independent rerun on another person's hardware.
- **No universal graphics or performance guarantee:** Host GPU backend availability, controller support and throughput depend on the host and tested workload; only reproduce numbers with their recorded conditions.
- **No automatic release equivalence:** A change on `main` may not exist in the published archives. Check the [latest release](https://github.com/hffgzktbcc-blip/ounce-runtime/releases/latest), the [changelog](../CHANGELOG.md) and the source commit used.

## Where to inspect the evidence

- [Quick start and output reports](QUICKSTART.md)
- [Dated corpus parity records](CORPUS_PARITY.md)
- [Existing evidence validator and limits](verify/README.md)
- [Release assets and exact published version](https://github.com/hffgzktbcc-blip/ounce-runtime/releases/latest)

**Licence:** GPL-3.0-or-later. Ounce-Core and Ounce-HB are separate MIT repositories; share runtime observations through external CLI/JSON interfaces rather than copying GPL code into MIT siblings. [Choose the right Ounce project](https://github.com/hffgzktbcc-blip/ounce-core/blob/main/docs/FAMILY.md).
