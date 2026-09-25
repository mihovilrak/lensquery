# LensQuery — Troubleshooting

Symptom → cause → fix. Add new entries as issues are encountered and resolved.
Keep entries terse.

**Start with `lq doctor`.** It prints the resolved libtesseract path, every
path it tried and why each was rejected, the tessdata directory, the languages
Tesseract can see, the active defaults, and the database status. Most entries
below are read off that output.

---

## Tesseract / OCR

### `libtesseract: not found` — every image fails, or `lq index` exits 2

- **Cause:** no libtesseract shared library on the loader path. LensQuery loads
  it at runtime (`dlopen`), so the binary installs and runs fine without it —
  you only find out at index time.
- **Fix:**
  - Windows: `winget install UB-Mannheim.TesseractOCR`, then open a new shell.
  - macOS: `brew install tesseract tesseract-lang`.
  - Linux: `apt install tesseract-ocr libtesseract5` (or distro equivalent).
- **Verify:** `lq doctor` prints a `libtesseract:` line with a real path.

### `lq doctor` lists the right path but the outcome is `OpenFailed`

- **Cause #1:** architecture mismatch — a 32-bit DLL against a 64-bit `lq`, or
  x86_64 vs arm64 on macOS.
- **Cause #2 (the sneaky one):** the library was found and *its own*
  dependencies were not. libtesseract pulls in leptonica and ICU; a missing
  `libicudt*` looks identical to a missing libtesseract from the point of view
  of the loader.
- **Fix:** the `OpenFailed(..)` string in `lq doctor` carries the OS loader
  message — read it. On Windows, check that the DLLs sitting next to
  `libtesseract-5.dll` are all present.

### `lq doctor` says `MissingSymbols`

- **Cause:** the file loaded but exports no Tesseract C API — usually a
  same-named library from something else, or a C++-only build.
- **Fix:** point `LENSQUERY_TESSERACT_LIB` at the correct file directly. It
  bypasses the search entirely.

### Tesseract is installed somewhere the search does not look

- **Fix:** `LENSQUERY_TESSERACT_LIB=/path/to/libtesseract.so lq doctor`. This
  is the supported escape hatch for Nix store paths, custom builds, and
  containers.

### `Failed loading language 'hrv'` / `tesseract_langs` is missing a language

- **Cause:** only `eng` ships with most Tesseract packages; other languages are
  separate data files.
- **Fix:**
  - Windows: tick "Additional language data" in the UB-Mannheim installer.
  - Linux: `apt install tesseract-ocr-hrv`.
  - macOS: `brew install tesseract-lang` brings them all.
- **Note:** `osd` is **not** a language. It is an orientation and script
  detection model, and putting it in a `--lang` string fails initialization.

### `tessdata_path` points at a directory that does not exist

- **Cause:** `TESSDATA_PREFIX` is set in the environment and wrong. LensQuery
  reports it as-is rather than silently falling back, because that is the
  diagnosis.
- **Fix:** unset it (the `tessdata` directory beside the library is then found
  automatically) or point it at the real one.

### OCR result is garbage or mostly empty

- **Cause #1:** the image is small and was not upscaled. Below `MIN_WIDTH`
  (1000 px) the pipeline upscales to 1200 px; not upscaling is a measured loss.
- **Cause #2:** the page is dense prose and the default sparse mode (psm 11) is
  the wrong fit.
- **Cause #3:** `--min-conf` is filtering real words. The default floor is 40.
- **Fix:** try `lq index <dir> --thorough` (adds a second uniform-block pass
  when the sparse pass finds few words) and `--min-conf 0` to see the
  unfiltered text. `-v` prints per-image detail.

### `Summary: 0 indexed, 0 updated, N failed`

- **Cause:** Tesseract is missing, or every language in `--lang` failed to load.
- **Fix:** `lq doctor` first. Re-run with `--failed-log failed.txt` — each line
  is `path<TAB>reason`.

---

## Indexing

### `database is locked`

- **Cause:** WAL mode was not applied, or another process holds a write lock.
- **Fix:** WAL is applied on every `connect()`; check that nothing else is
  writing the same `.db`. A stale `-wal`/`-shm` pair next to the database is
  normal and is not the cause.

### `schema version mismatch` (exit 2)

- **Cause:** the database was written by an older LensQuery whose FTS5 rows do
  not carry the `rowid == files.id` invariant. There are no live migrations, by
  design — a silent upgrade would return wrong results.
- **Fix:** delete the `.db` and re-index.

### Incremental skip not working — everything gets re-OCR-ed

- **Cause #1:** `--full-reindex` is set. That is what it does.
- **Cause #2:** mtime changed on a copy. FAT32 has 2-second mtime resolution,
  NTFS 100 ns; a cross-filesystem copy shifts every timestamp.
- **Cause #3 (Windows, symlinks):** the walk reports the mtime of the symlink
  itself, not of its target, so edits behind a symlink do not re-trigger OCR.
  `--full-reindex` covers it.

### A run died partway — start over?

- No. One transaction per batch, so everything committed before the failure is
  durable. Re-run the same command; already-indexed files are skipped.

### Indexing pegs every core but throughput is poor

- **Cause:** each worker holds its own Tesseract engine, and the OpenMP threads
  inside Tesseract would oversubscribe on top of that.
- **Fix:** `OMP_THREAD_LIMIT=1` is set from `main` before any thread spawns. If
  you export it yourself to something else, that value wins — unset it.

### ObjectCache "LEAK" warnings printed at the end of a successful run

- **Cause:** the C++ static destructors in Tesseract racing thread-local engine
  teardown.
- **Fix:** already handled - engines are shut down on each worker before the
  pool joins. If you see this after a code change, a `shutdown()` call was lost.

### Tesseract diagnostics are missing while debugging OCR

- **Expected:** LensQuery redirects Tesseract and Leptonica's per-image chatter
  away from stderr so it cannot overwrite the progress bar.
- **Debug:** set `LENSQUERY_TESSERACT_DEBUG=1` to restore the native messages
  for that process. LensQuery's own errors and `--failed-log` are unaffected.

---

## Search

### FTS5 parse error / `unknown special character`

- **Cause:** a query reached `MATCH` without passing through
  `build_fts5_query`.
- **Fix:** every public search entry point in `db` sanitizes exactly once, and
  the private `search` behind them takes text that is already sanitized. The
  fuzzy path rewrites the query *after* that, so it re-checks every term it
  takes out of `ocr_vocab` with `is_bareword`. Add the offending input as a
  test case.

### `--fuzzy` does not find an obvious typo

- **Cause:** usually the word is too short. The budget scales with length —
  under 4 characters nothing is widened at all, because at that size every
  word is one edit from every other word. Failing that, the correction is
  further away than the budget allows, or the word never made it into the
  index in any spelling.
- **Fix:** `--fuzzy-distance 2` (or 3, the ceiling) buys a wider search.
  Confirm the word is actually indexed with `--substring` on a fragment: if a
  substring finds nothing either, the OCR never produced that word and no
  amount of typo tolerance will reach it. See
  [decisions/0007-fuzzy-vocabulary-walk.md](decisions/0007-fuzzy-vocabulary-walk.md).

### `--fuzzy` returns too much

- **Cause:** short words at a hand-raised distance. At distance 2, a five-letter
  word is near a large slice of any real dictionary.
- **Fix:** drop back to the default budget, or `--fuzzy-distance 0`, which is a
  plain search. Exact hits are always ranked above corrections, so the ones you
  wanted are at the top either way.

### `--substring` does not find an obvious typo

- **Cause:** trigram is **substring** matching, not edit distance. `invioce`
  will not find `invoice`.
- **Fix:** that is what `--fuzzy` is for. Use `--substring` when only a
  *fragment* of the word is right (`invoi`), `--fuzzy` when the whole word is
  right but misspelled.

### A prefix search (`term*`) returns nothing

- **Cause:** a trailing `*` is the FTS5 prefix operator and is deliberately
  passed through unquoted; the rest of the token must be alphanumeric or the
  whole token gets quoted and the `*` is dropped by the tokenizer.
- **Fix:** use `invoi*`, not `inv-oi*`.

### `--snippet` output looks truncated to a dozen characters

- **Cause:** snippet windows are counted in *tokens*, and the trigram table
  tokenizes per character. The porter window (12) applied to the trigram table
  yields 12 characters.
- **Fix:** the substring path uses 64, the SQLite ceiling. If you see 12 with
  `--substring`, the wrong constant is in play. `--fuzzy` runs on the porter
  table and keeps the 12-*token* window.

### `--open` opens nothing and exits silently

- **Cause:** empty result set. The exit code should be 1 with `--open`, and the
  message goes to stderr.
- **Fix:** if the exit code is 0, that is a bug — `cli` must exit non-zero when
  `--open` is set and results are empty.

### Search feels slow in a loop

- **Cause:** process spawn, not search. A `lq search` invocation pays a measured
  ~36 ms floor before any query work happens.
- **Fix:** use `lq serve` for repeated queries. It pays that cost once and then
  answers on an open connection.

### `lq serve` hangs waiting for a response

- **Cause:** the client is reading past the end of a block, or a directive was
  malformed and the client did not expect a reply.
- **Fix:** every non-blank input line gets exactly one block terminated by one
  blank line — including errors, which report to stderr and still emit an empty
  block. Read until a blank line; do not count lines. Note that a query cannot
  start with `:` — that prefix is reserved for directives.

---

## Build / Install

### `cargo install lensquery` succeeds but OCR does not work

- **Expected.** Tesseract is a runtime dependency, loaded on demand. See the
  Tesseract section above. This is deliberate: the crate builds and installs
  everywhere, and `lq doctor` explains the gap instead of the build failing.
  See [decisions/0005-runtime-dlopen-tesseract.md](decisions/0005-runtime-dlopen-tesseract.md).

### A single corrupt image kills the whole run

- **Cause:** `panic = "abort"` in the release profile. The indexer wraps each
  image in `catch_unwind`, which requires `panic = "unwind"`.
- **Fix:** do not change the release profile in `Cargo.toml`.

### Build fails on a missing C toolchain

- **Cause:** the `bundled` feature of `rusqlite` compiles SQLite from source.
- **Fix:** install a C compiler — build-essential, Xcode command line tools, or
  MSVC Build Tools ("Desktop development with C++").

---

## Tests

### Tests pass locally, fail in CI

- **Cause (most common):** the CI image lacks a language pack, or libtesseract
  entirely.
- **Fix:** tests that need a real engine must skip when `tess::available()` is
  false rather than fail. Install `tesseract-ocr` plus the needed language packs
  in the CI image for the tests that do exercise it.

### A test leaves a `.db` behind

- **Fix:** use `:memory:`, which `connect` recognizes and for which it skips
  parent-directory creation, or a `tempfile` directory.
