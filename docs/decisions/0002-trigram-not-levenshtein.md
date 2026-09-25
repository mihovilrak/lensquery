# 0002 — Fuzzy search is substring, not Levenshtein

- **Status:** Superseded by [0007-fuzzy-vocabulary-walk.md](0007-fuzzy-vocabulary-walk.md)
- **Date:** 2026-05-14

## Context

Users asked for fuzzy search. "Fuzzy" can mean two very different things:

1. **Substring / partial match** — `"invoi"` finds `"invoice"`.
2. **Edit-distance / typo-tolerant** — `"invioce"` finds `"invoice"`.

The `trigram` tokenizer in FTS5 implements #1, not #2. Trigram indexing builds
overlapping 3-character windows; a transposition like `oi`→`io` destroys most
trigrams of the original word, so the match fails.

Implementing #2 on top of SQLite requires either:

- A custom tokenizer in C (loadable extension — defeats single-binary goal).
- Post-filtering candidate rows in Python with `rapidfuzz` (adds a dep and is
  slow at query time on large result sets).
- A precomputed n-gram or BK-tree (significant extra code + storage).

## Decision

Ship #1 only in Phase 1. `--fuzzy` uses the FTS5 trigram tokenizer and
explicitly documents the limitation in `--help`.

## Consequences

- A user searching for `"invioce"` with `--fuzzy` gets no results, which is
  surprising relative to "fuzzy" as commonly understood in editors. The CLI
  help and `troubleshooting.md` both call this out.
- The `ocr_text_trigram` table costs the same storage as `ocr_text` — already
  accepted in [0001-sqlite-fts5.md](0001-sqlite-fts5.md).

## Reconsider when

- Real users hit the transposition case often enough that it becomes a support
  burden.
- A Phase 2 release lets us add `rapidfuzz` as a post-filter on top-N candidates
  (cheap because N is small).
