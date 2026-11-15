# Declared Cargo licence metadata inventory

Inventory date: **2026-11-15**

Scope: all workspace crates and all resolved third-party dependencies in `Cargo.lock` (including
target-specific crates for Linux/macOS/Windows/Android/WASM).

Command used:

```bash
cargo metadata --locked --format-version 1
```

Recorded metadata summary, regenerated from that command on this date:

- Resolved packages in `Cargo.lock`: **321** (2 workspace crates + 319 third-party crates)
- Third-party crates audited: **319**
- Crates with missing SPDX license expression: **0**
- Dominant licence families:
  - `MIT OR Apache-2.0`: 141
  - `MIT`: 66
  - `Apache-2.0 OR MIT`: 17
  - `Apache-2.0 WITH LLVM-exception`: 16
  - `MIT/Apache-2.0`: 14
  - `Zlib OR Apache-2.0 OR MIT`: 13
  - `Apache-2.0`: 11
  - Remaining crates are permissive variants (ISC, BSD, Zlib, Unlicense, Unicode-3.0, or combinations).

Notes:

- `r-efi` declares `MIT OR Apache-2.0 OR LGPL-2.1-or-later`; this is an OR expression, so it can be
  consumed under a permissive option. Two resolved crates use that expression.
- Cranelift/Wasmtime-family crates are `Apache-2.0 WITH LLVM-exception`.
- Release archives bundle the per-crate notice texts through `scripts/bundle_release_notices.py`
  (`THIRD_PARTY_LICENSES.txt`). This inventory records declared crate metadata only. It does not
  inspect licence texts by hand, fixture binaries, or establish redistribution obligations beyond
  the SPDX expressions above.
