# Contributing to localdb

**License:** AGPL-3.0-or-later. By submitting a contribution you agree your work is
licensed under the same terms.

**Design authority:** the `specs/` directory. User-facing behavior is described in
`docs/`. If they disagree, `specs/` wins — open an issue to fix the docs, not the spec
(unless the spec is wrong).

---

## Development setup

### Prerequisites

- **Rust toolchain:** install via [rustup](https://rustup.rs/). The project pins its
  MSRV in `Cargo.toml`; `rustup` will pick it up automatically.
- **llvm-tools** (for coverage): `rustup component add llvm-tools-preview`
- **cargo-llvm-cov** (coverage runner): `cargo install cargo-llvm-cov`
- No other system dependencies are required for a development build.

### Build

```sh
cargo build --workspace
```

### Test

```sh
cargo test --workspace
```

### Coverage

Coverage gates are enforced in CI and are a hard PR requirement (see below).
To check locally:

```sh
cargo llvm-cov --workspace --lcov --output-path lcov.info
cargo llvm-cov report --workspace
```

Gates (from [specs/01-architecture.md](specs/01-architecture.md) §7):

- **≥ 80%** line coverage for critical functions — search orchestration, fusion,
  chunking, extraction normalization, config resolution, ID derivation.
- **≥ 90%** for anything that **modifies data** — store upserts/deletes, index job
  execution, document/chunk writes, config/state mutation, migrations, the write-lock
  path.

A PR that drops below either gate will not be merged.

### Lint + format

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

Both must be clean before a PR is opened. CI enforces this with `--deny warnings`.

---

## Spec-first rule

> **Any behavior change or new feature must start with a spec edit in the same PR.**

`specs/` is the design authority. Code is the implementation of the spec; if they
diverge the spec is consulted to decide what is correct. Steps for a change that
touches behavior:

1. Edit the relevant spec section (or add a new section) in `specs/`.
2. Write the failing test that proves the new or changed behavior.
3. Implement until the test passes and coverage gates are met.
4. Update user-facing docs if the surface changes.

Pull requests that add or change behavior without a corresponding spec edit will be
sent back to update the spec first.

---

## Test-driven development (TDD)

TDD is the default mode for all crates (see
[specs/01-architecture.md](specs/01-architecture.md) §7):

1. Write the **failing test** first.
2. Write the minimum implementation to make it pass.
3. Refactor under green.

Trait-based seams (`RetrievalStore`, `Embedder`) exist to make this practical: core
logic is tested against in-memory fakes; adapter crates are tested against the real
backend (LanceDB in a `tempdir`, ONNX tiny model) in integration tests. Do not merge
tests that pass trivially or mock away the behavior under test.

Coverage gates (stated above) are a **PR requirement**, not a guideline.

---

## Commit style

Commits in this repository follow a short imperative-mood subject line, no ticket
prefix required, body optional. Examples drawn from the log:

```
Wire serve and mcp subcommands to their crate implementations
T09 review: strengthen test coverage — full error exit code map, real store list shape test
Fix all spec violations, unmet ACs, and fake tests
```

- One logical change per commit.
- Subject line ≤ 72 characters.
- Reference spec sections in the body when the commit implements spec behavior.

---

## Proposing features

Anything that touches spec-defined behavior (a new command flag, a new API endpoint,
a new chunking preset, a change to error codes, a new roadmap phase) requires an issue
**before** you open a PR.

1. Check [specs/06-roadmap.md](specs/06-roadmap.md) — the item may already be planned
   or explicitly deferred. If it is explicitly deferred, explain in your issue why the
   deferral reasoning no longer applies.
2. Open an issue using the [Feature Request](.github/ISSUE_TEMPLATE/feature_request.yml)
   template.
3. Wait for acknowledgment before writing code.

Small bug fixes, test improvements, and documentation corrections do not need a prior
issue.

---

## AI-assisted development

This project uses Claude Code as a development assistant. Conventions for working in
this repository (worktree layout, agent ground rules, spec-first enforcement) are
documented in `CLAUDE.md` at the repo root. If `CLAUDE.md` does not yet exist, the
conventions in this file apply.

---

## AGPL-3.0-or-later

localdb is free software: you can redistribute it and/or modify it under the terms of
the GNU Affero General Public License as published by the Free Software Foundation,
either version 3 of the License, or (at your option) any later version. See
[LICENSE](LICENSE) for the full text.

Contributions must be compatible with AGPL-3.0-or-later. If you are contributing on
behalf of an employer, confirm that your employer's IP policy permits the contribution
before submitting.
