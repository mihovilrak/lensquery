# 0006 — Rust-only: the Python implementation is not shipped

- **Status:** Accepted
- **Date:** 2026-08-17

## Context

LensQuery was built twice. The Python implementation came first and defined the
CLI surface, the schema, and the OCR configuration. The Rust implementation was
written against it function-for-function so the two could be cross-checked, and
for a while both existed in one repository with the Rust core under `rust/`.

Shipping both is not viable:

- **Distribution.** The whole point of the tool is "download one file and search
  your images." Python means an interpreter, a virtualenv or a PyInstaller-style
  bundle, and a per-platform packaging story for each. Rust means one binary.
- **Two of everything.** Two query builders that must produce identical FTS5,
  two schema migrations, two OCR configs. Every fix landed twice or diverged.
- **Speed.** The measured indexing throughput of the Rust core is the reason it
  was written; see [../benchmarks.md](../benchmarks.md).

## Decision

The public repository contains the Rust implementation only. The Python package
(`src/lq/`, its test suite, `pyproject.toml`, `uv.lock`) is not carried over.
The Rust tree is promoted to the repository root — there is no `rust/`
subdirectory, because there is nothing for it to be a sibling of.

The Python implementation remains in the author's private predecessor
repository. It is not a supported artifact and will not be published.

## Consequences

- The cross-implementation parity tooling (a probe that opened one index with
  both cores and diffed results) is dead code and was dropped rather than
  ported.
- Comments and doc-comments that explained Rust code by pointing at its Python
  counterpart ("mirrors `lq.cli`", "same as the Python `_build_fts5_query`")
  became references to something the reader cannot see. These were rewritten to
  explain the code on its own terms.
- Facts that only existed as measurements in the private repo's reports — OCR
  config choices, throughput numbers, the recall/precision trade behind
  `--min-conf` — were consolidated into [../benchmarks.md](../benchmarks.md) so
  they survive the drop.
- The contract documents (`api-contracts.md`) describe Rust signatures. The
  three-valued OCR result, WAL on every connect, and "all user input goes
  through the query builder" carry over unchanged — they were always properties
  of the design, not of the language.

## Reconsider when

- Never, realistically. A Python *binding* to the Rust core is a different
  question and is not precluded by this decision.
