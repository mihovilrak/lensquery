# 0001 — SQLite FTS5 for the index store

- **Status:** Accepted
- **Date:** 2026-05-14
- **Supersedes:** —

## Context

LensQuery needs a local, embeddable, zero-dependency text index that can hold
the OCR output of tens of thousands of images and answer phrase queries in
under 50 ms. The tool must be a single binary with no daemon, no server, no
network.

Candidates considered:

- **SQLite FTS5** — embeddable, no server, and available as a single vendored
  C dependency (`rusqlite` with the `bundled` feature), so the build does not
  depend on what SQLite the host happens to ship.
- **Tantivy** — a faster, richer search core, but it brings a full index
  directory format, a schema layer, and a segment/merge policy to own. The
  metadata store would still be separate.
- **Meilisearch / Elasticsearch** — both require a running server. Killed by
  the "no daemon" requirement.
- **A flat-file inverted index** — fun project, not the project.

## Decision

Use **SQLite FTS5** as both the metadata store (`files`, `meta`) and the
full-text store (`ocr_text`, `ocr_text_trigram`).

- Single `.db` file on disk → trivially portable, trivially backed up.
- Phrase search (`"exact phrase"`) and BM25 ranking out of the box.
- The `trigram` tokenizer (FTS5 since SQLite 3.34) gives substring search in a
  second virtual table with no extra dependency.
- One vendored dependency, statically linked — nothing for the user to install.

## Consequences

- **Storage doubles** because we keep two FTS tables (porter + trigram) holding
  the same content. Accepted.
- **Deletes and replaces must be addressed by `rowid`.** `file_id` is
  `UNINDEXED`, so deleting by it is a full virtual-table scan and makes upserts
  quadratic in corpus size. Schema v2 fixes this by writing every fts5 row with
  `rowid == files.id`; see [api-contracts.md](../api-contracts.md) for the
  invariant and the measurements.
- **WAL mode is mandatory** so a long `lq index` does not block `lq search`.
- **No live migrations** — wipe and re-index on a schema-version mismatch.
  A silent upgrade that returned wrong results is worse than an explicit
  re-index.

## Reconsider when

- Corpus regularly exceeds 100k images.
- ~~Users need typo-tolerant (Levenshtein) search — trigram is
  substring-only.~~ They did, and it did not cost us FTS5: the edit distance
  runs over the index's own term dictionary and hands FTS5 an ordinary query.
  See [0007-fuzzy-vocabulary-walk.md](0007-fuzzy-vocabulary-walk.md).
- We start needing structured queries (date range, file size) — FTS5 is not
  the right tool for those; would add a regular table side-by-side.
