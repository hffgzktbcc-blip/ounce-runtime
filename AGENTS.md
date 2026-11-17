# Working on ounce-runtime

ounce-runtime is one of four sibling repos (`ounce-core`, `ounce-hb`, `ounce-runtime`, `deckstream`).
Direction for the family is the [one-year roadmap in ounce-core](https://github.com/hffgzktbcc-blip/ounce-core/blob/main/docs/roadmap/OUNCE_FAMILY_1_YEAR_ROADMAP.md);
check it before multi-repo, release or roadmap work. Docs map: [docs/README.md](docs/README.md).

## Before you start
- Check what is already done: `git log -20 --oneline`, `CHANGELOG`-style notes in releases, open PRs and issues. Everything under `docs/history/` is past context, never an assignment.
- Linux build needs `sudo apt-get install -y libasound2-dev libudev-dev`.

## Checks (what CI runs)
- `cargo build --release`, `cargo test --release`, `cargo run --release -- test`
- `cargo deny check` (see `deny.toml`); `rust-security.yml` and `rust-coverage.yml` run the audit and coverage jobs
- Python corpus tools: `python3 -m unittest discover -s scripts -p 'test_*.py'`

## Rules
- Unsafe code is limited to the low-level modules that already use it (`crates/ounce-recompiler/src/exec.rs`, `src/mmu/`, `src/cpu/nce.rs`, `src/hid/gamepad_mac.rs`, plus a few lines in `src/main.rs`). Do not add `unsafe` elsewhere; keep each block small with a safety comment.
- Homebrew only: unencrypted NRO/NSO built from source. Never commit keys, firmware or retail content; CI greps for key file names.
- Sign off commits (`git commit -s`, DCO). Keep PRs small, with tests where they make sense.
- Never fabricate pass rates or benchmarks; state what was and was not run. Provenance and notices: [docs/PROVENANCE.md](docs/PROVENANCE.md), `THIRD_PARTY.md`.

## Release state and license separation
- Published: ounce-runtime v0.6.0 (with ounce-core v0.9.0 and ounce-hb v0.1.0). Verify live GitHub state before treating a dated plan or handoff as unfinished; do not retag or reimplement released work.
- This repository is experimental **GPL-3.0-or-later**; `ounce-core`, `ounce-hb` and `deckstream` are MIT. Share behavior and evidence through CLI/JSON contracts and subprocesses; never copy or port GPL source into an MIT sibling.
- Open a PR rather than merging without owner authorization. Escalate credentials, spending, destructive changes and material distribution or legal decisions.
