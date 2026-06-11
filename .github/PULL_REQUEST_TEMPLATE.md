## Summary

<!-- One or two sentences describing what this PR does and why. -->

## Checklist

- [ ] **Spec section referenced or updated.** Any behavior change, new flag, endpoint,
  error code, config key, or data model change is accompanied by an edit to the
  relevant section in `specs/`. (See [CONTRIBUTING.md — Spec-first rule](../CONTRIBUTING.md).)
- [ ] **Tests written first (TDD).** Failing tests were written before the implementation.
  New code does not lower coverage below the gates:
  - ≥ 80% for critical functions (search, fusion, chunking, extraction, config, IDs)
  - ≥ 90% for anything that modifies data (store writes/deletes, index jobs, migrations,
    write-lock path)
- [ ] **Coverage gates met.** `cargo llvm-cov --workspace` confirms gates pass locally.
- [ ] **`cargo fmt --all` clean.** No formatting changes left unformatted.
- [ ] **`cargo clippy --workspace --all-targets -- -D warnings` clean.** Zero warnings.
- [ ] **Docs updated.** If this PR changes user-facing behavior (command output, config
  keys, error messages, JSON shapes), the relevant file in `docs/` is updated.
