# Ounce-Runtime agent orientation

## Start here, not in an old handoff

1. Read the canonical [Ounce Family 1-Year Roadmap (Nov 2026–Nov 2027)](https://github.com/hffgzktbcc-blip/ounce-core/blob/main/docs/roadmap/OUNCE_FAMILY_1_YEAR_ROADMAP.md). It is the family-wide direction for `ounce-core`, `ounce-hb`, `ounce-runtime`, and `deckstream`.
2. Check **current main**, merged and open PRs/issues, published release tags/assets and recent CI before taking a task. Dated roadmap status notes, old release checklists, and handoffs are context, not evidence that work is still needed. As of 17 Nov 2026, runtime **v0.6.0** is published (alongside core v0.9.0 and HB v0.1.0); do not cut those tags again.
3. Read applicable local README, tests, safety and provenance docs before editing. Pick the *current user's requested task* or a genuinely unfinished roadmap item; stay on a topic branch, preserve other work and keep check/test results explicit.
4. Report the exact result, verification evidence and known gaps. A merged PR is not proof of real-device compatibility or independent testing. Don't create a duplicate plan or handoff merely to restate GitHub state.

## Non-negotiable license and safety separation

This repository is experimental **GPL-3.0-or-later**; `ounce-core`, `ounce-hb`, and `deckstream` are MIT siblings. Share behavior and evidence through CLI/JSON contracts and subprocess interfaces; **never copy or port GPL source code into an MIT sibling**. Never introduce retail games, keys, firmware, private content or unauthorized binaries. Inspect current project safety/clean-room notices before making legal or distribution claims.

## Working discipline

Do useful engineering work, run relevant checks, and open a PR rather than merging without owner authorization. Escalate credentials, spending, destructive changes and material distribution/legal decisions. Aim for minimal setup, minimal user intervention and reproducible results.
