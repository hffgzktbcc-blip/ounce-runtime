# Contributing

Thanks for helping. There are only a few hard rules; everything else is about moving quickly and keeping tests green.

## The rules

1. **No keys, firmware, game data or dumps** in the repo, in issues, or in PRs. This includes test data derived from them.
2. **No decryption or key handling.** The runtime reads already-extracted program folders and plaintext homebrew/synthetic containers only. PRs that add key discovery, key parsing, or decryption of protected content will be closed.
3. **No dumping or key-extraction instructions or links** in code, docs, or issues.
4. **No game titles, title IDs, or screenshots of commercial software** in docs, tests, or compatibility lists.
5. **Record provenance** for every fixture or asset in `docs/PROVENANCE.md` (who wrote it, how it was built, licence). Unknown provenance means it does not merge.
6. **Implement from public documentation or from observed behaviour of your own test programs.** Do not copy code or tables from leaked, proprietary, or incompatibly licensed sources. Public headers and docs (Arm, libnx, Mesa/Nouveau, SwitchBrew) are fine; cite them in a comment where a table must match a layout.
7. **Keep wording honest:** the project is unaffiliated with any console maker; do not claim compatibility you have not tested.

## Claiming an issue

Comment `/claim` on an open issue labelled [`good first issue`](../../issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22+no%3Aassignee) or [`help wanted`](../../issues?q=is%3Aissue+is%3Aopen+label%3A%22help+wanted%22+no%3Aassignee) and you are assigned automatically. Open a draft PR or leave a comment within 14 days to keep it; otherwise it is freed for someone else. For any other issue, ask first and a maintainer will assign it.

## Workflow

- `cargo build --release`, `cargo test --release` and `cargo run --release -- test` should pass. Known failing tests are listed in the README.
- Sign off commits with `git commit -s` (DCO).
- Small focused PRs, with tests where they make sense.
