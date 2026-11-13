# Corpus Parity

This document tracks head-to-head execution parity of outside homebrew programs across the Ounce family engines:

1. **`ounce-core`**: The reference engine executing in headless mode.
2. **`ounce-hb`**: The diagnostic runner wrapping the engine CLI.
3. **`ounce-runtime`**: The performance and graphical branch with native execution (NCE/JIT) and host GPU presentation.

## Status

A joint 50-program parity suite (`scripts/corpus_parity.py` in `ounce-hb`) compares stop categories, stdout/stderr streams, and exit codes.

| Program | ounce-core | ounce-runtime (Interpreter) | ounce-runtime (JIT/NCE) | Status |
| :--- | :--- | :--- | :--- | :--- |
| *Baseline execution in progress* | *Pending* | *Pending* | *Pending* | Tracked |

Detailed comparative results and benchmarks will be published here upon completion of the full corpus run.
