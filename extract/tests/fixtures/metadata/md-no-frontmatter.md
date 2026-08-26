# Notes on Local Development Setup

There is no YAML front matter block in this file at all — it starts
directly with a Markdown heading. This probes the fallback path when
front matter parsing has nothing to find: does the extractor fall back to
a content heuristic (e.g. the first H1 as a title) cleanly, without
crashing on the missing front matter, and correctly report no explicit
date?

## Prerequisites

- Rust toolchain (see `rust-toolchain.toml`)
- SQLite 3.35+
- A local checkout of the repository

## Getting started

Clone the repository and run the build command for your platform.
