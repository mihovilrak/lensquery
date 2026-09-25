# 0007 — Typo tolerance comes from walking the FTS5 vocabulary

- **Status:** Accepted
- **Date:** 2026-08-20
- **Supersedes:** [0002-trigram-not-levenshtein.md](0002-trigram-not-levenshtein.md)

## Context

[0002](0002-trigram-not-levenshtein.md) shipped `--fuzzy` as a trigram
substring search and said so in `--help`: `invoi` found `invoice`, `invioce`
found nothing. That was an honest name for what the flag did, but it is not
what the word means to anyone who has used a search box, and the release review
called it out as the last feature gap that would generate support mail.

The three options 0002 rejected were rejected on Python-era grounds. Two of
them still do not fit: a loadable C tokenizer breaks the single-binary promise,
and a precomputed BK-tree is storage we would have to migrate every existing
index onto. The third — post-filtering candidates by edit distance — was
rejected because `rapidfuzz` was a dependency and Python was slow. Neither is
true any more.

What changed is that the candidate set does not have to come from the result
rows at all. FTS5 already keeps a sorted term dictionary for every index it
builds, and `fts5vocab` exposes it as an ordinary table. The words are already
there; nothing has to be computed, stored, or migrated to read them.

## Decision

`--fuzzy` is typo tolerance. Substring matching keeps the behaviour and moves
to `--substring`.

A fuzzy search runs twice. The first pass is a plain search of what the user
typed. The second walks `ocr_vocab` — `fts5vocab(ocr_text, 'row')` — for terms
within a bounded edit distance of each query word, rewrites the query as
`(typed OR near1 OR near2) AND (...)`, and runs that. Exact hits are emitted
first and paths are deduplicated, so widening only ever appends.

Four things keep it cheap and predictable:

- **The budget scales with length.** Zero edits for three characters or fewer,
  one up to six, two beyond that, and never more than three. A short word has
  too few neighbours to spend an edit on.
- **SQLite discards most of the dictionary before Rust sees it**, on a
  `length(term)` window around the query word.
- **Distance is measured against stems, not words.** The vocabulary of a porter
  index holds `invoic`, not `invoice`, so up to two trailing characters of the
  typed word are free. Without that, every query pays for a suffix the stemmer
  removed and nobody got wrong.
- **Shapes are folded before the comparison.** `1`, `l`, `|` and a dotless `i`
  are one stroke; `rn` is `m`, `cl` is `d`, `vv` is `w`. An OCR misread costs
  nothing, which is the difference between typo tolerance and *scan* tolerance.

Distance is optimal string alignment — Damerau-Levenshtein without the repeated
edits — hand-rolled in [src/fuzzy.rs](../../src/fuzzy.rs), abandoned as soon as
every path in a row exceeds the budget. No new dependency.

`--fuzzy-distance N` overrides the budget; `0` is a plain search.

## Consequences

- **The `--fuzzy` flag changed meaning.** Nothing has been released under the
  old meaning, so no user is broken, but `docs/api-contracts.md` moved:
  `search_fuzzy` takes a distance and does something else, and the old
  behaviour is `search_substring`.
- **No schema version bump.** `ocr_vocab` is a view over an index that already
  exists, created by `CREATE VIRTUAL TABLE IF NOT EXISTS` on every `connect()`.
  Existing indexes gain typo tolerance without a reindex.
- **A widened multi-word query is an explicit `AND`.** Bare terms side by side
  are already an implicit `AND` in FTS5, but parenthesised groups may not sit
  side by side, so the operator is spelled out. Same meaning, legal grammar.
- **A quoted phrase is never widened.** The user asked for those words in that
  order; FTS5 cannot express alternatives inside a phrase and guessing at one
  would answer a different question.
- **Folding is lossy and that is the point.** `in` and `ln` are the same two
  letters to this code, and `2024` is `zoz4`. Both sides of every comparison
  are folded identically, so the only words it brings together are words a
  scanner really does confuse.
- **Cost is one extra query plus a scan of a length slice of the dictionary.**
  On the 368-image corpus that is not measurable against process spawn, which
  is already where search latency goes.

## Reconsider when

- An index gets large enough that the length-windowed dictionary scan shows up
  in a profile. The fix is a prefix or n-gram index over `ocr_vocab`, not a
  different algorithm.
- The shape table needs to be per-language. It is Latin-script assumptions
  hard-coded in one function today.
