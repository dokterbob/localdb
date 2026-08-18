# Changelog

All notable changes to this project are documented in this file.

The format follows [Common Changelog](https://common-changelog.org).
## [0.1.0] - 2026-08-18

### Changed

- T01: Workspace scaffold, subcommand skeleton, error taxonomy, and CI ([`91a4a53`](https://github.com/dokterbob/localdb/commit/91a4a530f5c7c1751242ad344fdabec23aa9757f))
- T09: implement CLI — init, store, source, index, search commands with embedded mode ([`9b6921a`](https://github.com/dokterbob/localdb/commit/9b6921aa955b217902b25ecf9d8afc75bc66faae))
- T09 rework: fix write lock exclusivity, add daemon routing, add real store_locked test ([`c62f122`](https://github.com/dokterbob/localdb/commit/c62f122edd448a5feec12d54c47ec2f58747e1c0))
- T09 review: strengthen test coverage — full error exit code map, real store list shape test, full citation canonical shape in e2e ([`cb41ed3`](https://github.com/dokterbob/localdb/commit/cb41ed3b5ddb2733922a2e052e7929601e02f081))
- T12: add packaging & release workflow ([`b097e83`](https://github.com/dokterbob/localdb/commit/b097e83250d1085c8db83d3ba18c8bc9b904a44d))
- Wire serve and mcp subcommands to their crate implementations ([`7c2883b`](https://github.com/dokterbob/localdb/commit/7c2883be82c59fc138cfb2c75d704c001d905d91))
- Single SQLite store: unify libsql backend behind one StoreBackend trait ([#99](https://github.com/dokterbob/localdb/pull/99)) ([`3fc7b92`](https://github.com/dokterbob/localdb/commit/3fc7b9214f47edbe9dcf7b02f638aab29e3ea892))
- Cleanup workspace architecture and refactors ([#107](https://github.com/dokterbob/localdb/pull/107)) ([`4dc3b26`](https://github.com/dokterbob/localdb/commit/4dc3b26ccac0211ba0de3c4c8e9b633d3cce1147))
- Migrate mcp crate to official rmcp SDK, add MCP-over-HTTP (closes #134) ([#145](https://github.com/dokterbob/localdb/pull/145)) ([`f620365`](https://github.com/dokterbob/localdb/commit/f6203652a09e8ecf72b03ab067b22623e5e4bb2c))
- Parsing infrastructure refactor: single Resource pipeline ([#117](https://github.com/dokterbob/localdb/pull/117)) ([#151](https://github.com/dokterbob/localdb/pull/151)) ([`8dcc029`](https://github.com/dokterbob/localdb/commit/8dcc029523398928f233dd3c5148cac4b3cc1554))
- T116: Atom/RSS feed ingestor (closes #116) ([#170](https://github.com/dokterbob/localdb/pull/170)) ([`a4e7712`](https://github.com/dokterbob/localdb/commit/a4e77126f0f8378b6d1aba17e23211a5cfd929a3))
- Make `--store` omission predictable across all commands ([#180](https://github.com/dokterbob/localdb/pull/180)) ([`4bb8c0f`](https://github.com/dokterbob/localdb/commit/4bb8c0f556fe2242325d67571211122806c91350))
- T87/T103: swap pdf-extract → pdf_oxide, add PDF page-number citations ([#169](https://github.com/dokterbob/localdb/pull/169)) ([`9e046ee`](https://github.com/dokterbob/localdb/commit/9e046ee72b442948f3a174ae8522ae152853825b))
- Shrink the DiskANN index 9x: drop float8 neighbors on binary columns (closes #179, #177) ([#202](https://github.com/dokterbob/localdb/pull/202)) ([`9d010b9`](https://github.com/dokterbob/localdb/commit/9d010b97daf71882dd0c3472d6848aefb0bf252b))
- Make `--store` a filter, and enforce it everywhere it is accepted ([#201](https://github.com/dokterbob/localdb/pull/201)) ([#203](https://github.com/dokterbob/localdb/pull/203)) ([`aa7eb0e`](https://github.com/dokterbob/localdb/commit/aa7eb0e2532a5d09fc3475faace3b38bcce17e7c))
- Stop treating "I observed nothing" as "it was deleted" (closes #156, closes #185) ([#204](https://github.com/dokterbob/localdb/pull/204)) ([`cb5fe38`](https://github.com/dokterbob/localdb/commit/cb5fe3853bb4b496f839a4992e8db2b02aa688c0))
- Re-architect CLI↔daemon contract: structural parity, real daemon ingestion, SSE progress ([#212](https://github.com/dokterbob/localdb/pull/212)) ([`88947e7`](https://github.com/dokterbob/localdb/commit/88947e75216c4bf58146f447cf442674e06dabb0))
- Implicit init + versioned config JSON Schema (#119, #120) ([#215](https://github.com/dokterbob/localdb/pull/215)) ([`aa201fb`](https://github.com/dokterbob/localdb/commit/aa201fbd8c3056e51040a77fee374f1cf8691013))
- Job cancellation: DELETE /v1/jobs/{id} + localdb job cancel ([#218](https://github.com/dokterbob/localdb/pull/218)) ([#226](https://github.com/dokterbob/localdb/pull/226)) ([`8bd28f6`](https://github.com/dokterbob/localdb/commit/8bd28f631f0946394ca0980670a6426380af0cc6))
- RELENG 1/2: release tooling foundations — AGPL license fix, release-plz/git-cliff bootstrap, vergen stamping, completions, dist config ([#232](https://github.com/dokterbob/localdb/pull/232)) ([`2800516`](https://github.com/dokterbob/localdb/commit/28005162ff5aaf43079b914fd2c9e4733d9f803d))
- RELENG 2/2: dist pipeline, release-plz workflow, Homebrew tap publish, localdb-* crate namespacing ([#233](https://github.com/dokterbob/localdb/pull/233)) ([`b4ad5af`](https://github.com/dokterbob/localdb/commit/b4ad5afc8e9b0f8d6fc48c86ee41d49a7af94379))

### Added

- feat(extract,core): Markdown-native extraction — replace Block/text contract with unified markdown string ([#37](https://github.com/dokterbob/localdb/pull/37)) ([`3da56d0`](https://github.com/dokterbob/localdb/commit/3da56d0487ed6dde55698664e5d82f0ef309b373))
- feat(cli): quote-free search queries and -s short flag for --store ([#77](https://github.com/dokterbob/localdb/pull/77)) ([`a9841de`](https://github.com/dokterbob/localdb/commit/a9841de07e2e5ede0fdad35d3c2fd786c9de236f))
- feat: migrate to single libsql engine (DiskANN + FTS5) ([#92](https://github.com/dokterbob/localdb/pull/92)) ([`a0fb610`](https://github.com/dokterbob/localdb/commit/a0fb610a5410e3d41ab8b53e95aa1ef81e08e5f0))
- feat(ergonomy): double search snippet length, add --content-length flag, and localdb add alias ([#93](https://github.com/dokterbob/localdb/pull/93)) ([`065f882`](https://github.com/dokterbob/localdb/commit/065f882c68afa6a0cdf7d27daab424ddaf83f1e2))
- feat: Resource/Block/Ingestor framework (Phases 0-5) ([#110](https://github.com/dokterbob/localdb/pull/110)) ([`396a6bb`](https://github.com/dokterbob/localdb/commit/396a6bb52637a6fbada6193eaeafcc191544ae67))
- Add explicit DB schema migrations with old-binary downgrade (closes #127) ([#152](https://github.com/dokterbob/localdb/pull/152)) ([`11a342f`](https://github.com/dokterbob/localdb/commit/11a342f71a211ea584922ef9859d9f8cb1585798))

### Fixed

- fix: wire real embedders via config factory + validate store dim (#8, A4, A5, D3, #16) ([`76cfc21`](https://github.com/dokterbob/localdb/commit/76cfc21bd67735601ddc31c4585f0669bf82a855))
- fix: CLI ergonomics — validation, multi-arg, auto-index, exit codes (Wave 4) ([`363cfd1`](https://github.com/dokterbob/localdb/commit/363cfd153e78f5d69b49a159414e9cf80df1515f))
- fix(store): cascade sources and index data on store remove ([#32](https://github.com/dokterbob/localdb/pull/32)) ([`a5d5956`](https://github.com/dokterbob/localdb/commit/a5d5956df95e5cab43a43536857922cf82563011))
- fix(cli): eliminate nested block_on panic when daemon is running ([#58](https://github.com/dokterbob/localdb/pull/58)) ([`73ba082`](https://github.com/dokterbob/localdb/commit/73ba082f1452da922a0a8ede6882d3c556ec10b5))
- fix(core,cli): narrow redb lock window and surface RuntimeStateLocked (closes #46, closes #56) ([#59](https://github.com/dokterbob/localdb/pull/59)) ([`e63f937`](https://github.com/dokterbob/localdb/commit/e63f93795889662024d8c5b58d548745812b3ec5))
- fix(core,cli): replace redb with SQLite WAL for runtime-state (closes #67) ([#70](https://github.com/dokterbob/localdb/pull/70)) ([`914fb84`](https://github.com/dokterbob/localdb/commit/914fb84a0eb07b29ec0516034ed2470fbac93de1))
- fix(cli): improve search result readability ([#75](https://github.com/dokterbob/localdb/pull/75)) ([`3c288e9`](https://github.com/dokterbob/localdb/commit/3c288e90adbc2da9a263411830b77123ad81fb94))
- fix(cli): quiet pdf-extract/lopdf log noise during index ([#80](https://github.com/dokterbob/localdb/pull/80)) ([`88e5276`](https://github.com/dokterbob/localdb/commit/88e5276499b03fa236acd324bbbac00c7f05aa9e))
- fix(ci): eliminate flaky tests from env var races and SQLite contention ([#91](https://github.com/dokterbob/localdb/pull/91)) ([`243eb11`](https://github.com/dokterbob/localdb/commit/243eb114507c60cb7df8408dbb7a98efc9bccd19))
- fix: address review feedback on single-sql-store changes ([#101](https://github.com/dokterbob/localdb/pull/101)) ([`cd8fbb3`](https://github.com/dokterbob/localdb/commit/cd8fbb32b2df021ecf21f08f7a6d8d65397a97d6))
- Fix five defects found reviewing PR #180's daemon store scope ([#200](https://github.com/dokterbob/localdb/pull/200)) ([`5a1a370`](https://github.com/dokterbob/localdb/commit/5a1a370926fad1a7465d613540a79fdeda1e5b95))
- Address Codex review findings on job cancellation (#218 follow-ups) ([#229](https://github.com/dokterbob/localdb/pull/229)) ([`72561cd`](https://github.com/dokterbob/localdb/commit/72561cd9c2bc24a9e65c6b5a164433adafd91685))

