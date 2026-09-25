# Changelog

All notable changes to LensQuery are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**Pre-1.0 means the CLI surface can still change.** Flag names, defaults, and
output details may move in a `0.x` release when a measurement says they should.
Breaking changes get their own entry here and a note in the release body. The
one thing treated as stable even pre-1.0 is the result line shape
(`[<score>\t]<path>[\t<snippet>]`), because scripts depend on it.

## [0.1.0] - 2026-08-20

First public release. Offline OCR indexing and full-text search over a
directory of images, in one binary.

### Added

- `lq watch <dir>` — index changes as they happen instead of rebuilding.
  Filesystem events are debounced over a quiet period (`--debounce`, default
  3 s, forced out after 60 s so a continuous burst still makes progress) and
  the resulting batch goes to the same `index_paths` call `lq index` uses, so
  watch mode is not a second indexing path. A move inside the watched tree is
  detected by mtime pairing and costs one `UPDATE` with no OCR; a vanished
  file is dropped from the index. `--workers` defaults to **half** the cores
  rather than all of them, because a watcher runs while its user is still
  working in the directory. Refuses to start when the index would live inside
  the watched tree (exit 2) — that is a feedback loop. Foreground only, with
  supervisor recipes for systemd, launchd, and Task Scheduler in
  [docs/watch.md](docs/watch.md).

- `lq index --dry-run` — exact counts and a time estimate before committing to
  an hour of CPU. Prints how many images would be OCR'd, how many would be
  skipped on mtime, and an estimated wall time on the worker count that would
  be used, then exits without writing a database. The rate is measured on the
  user's own images, sampled with a stride through the walk so it spans the
  corpus rather than its first folder, and capped by a twelve-second budget so
  the dry run stays cheap. The printed range widens as fewer images could be
  timed, and falls back to the published rate with a wider range still when no
  image in the sample can be read.

- Prebuilt binaries. Tagged releases now build through
  [`cargo-dist`](https://opensource.axo.dev/cargo-dist/) for
  `x86_64-pc-windows-msvc`, `x86_64`/`aarch64-unknown-linux-gnu`, and
  `x86_64`/`aarch64-apple-darwin`, with shell and PowerShell installers, a
  Homebrew formula that declares `depends_on "tesseract"`, and SHA-256 sums
  for everything. No musl target: static musl has a stub `dlopen`, and every
  OCR call goes through `dlopen`. Nothing bundles `libtesseract`. See
  [docs/packaging.md](docs/packaging.md).

- Case-stable index roots. `canonicalize` corrects letter case on Windows but
  not on macOS, where it is `realpath`, so the root is recased against the
  filesystem on every platform: `~/Photos` and `~/photos` are one tree to
  `lq index` and `lq watch` alike, not two copies of every image. Paths below
  the root come from `read_dir` and were always spelled correctly.

- A build-time refusal to compile under `panic = "abort"`, which would quietly
  disarm the per-image `catch_unwind` that keeps one bad file from ending a
  multi-hour index run.

- `lq index <dir>` — recursive OCR indexing with incremental `mtime` skip,
  parallel workers, per-image failure isolation, and a resumable per-batch
  transaction. Flags: `--db`, `--full-reindex`, `--lang`, `--workers`,
  `--min-conf`, `--thorough`, `--no-thorough`, `--thorough-trigger-words`,
  `--failed-log`, `-v`.
- `lq search <query>` — BM25-ranked search over an SQLite FTS5 index, with
  `--limit`, `--open`, and `-v` for scores. Output is tab-separated and greppable. The matched excerpt is
  printed **by default**; `--no-snippet` gives paths only and `--snippet`
  names the default explicitly. (Settled before 0.1.0 shipped, so it is a
  default and never a breaking change. `lq serve` keeps snippets off unless
  asked — its contract is one path per line.)
- `--json` on `search`, `status`, and `doctor`. JSON Lines for `search` (one
  object per hit, so it streams into `jq` unbuffered), a single object for
  `status` and `doctor`. Only JSON goes to stdout in this mode; the human
  lines move to stderr. Schema: [docs/json-output.md](docs/json-output.md).
- `files.lang` — the Tesseract language string that produced each row's text,
  surfaced as `lang` in JSON results. Added to existing indexes in place: a
  purely additive column, so no `SCHEMA_VERSION` bump and no re-index.
- Two kinds of approximate matching, which are not the same question and
  cannot be asked at once. `--fuzzy` is typo tolerance: it walks the index's
  own term dictionary, keeps what is within a small edit distance of the typed
  word, and searches for that too, with the confusions OCR actually makes (`1`
  for `l`, `rn` for `m`) folded out for free. The budget scales with word
  length and `--fuzzy-distance N` overrides it. Exact hits always rank above
  corrections, and a quoted phrase is never widened. `--substring` is the
  trigram index — `nvoi` finds `invoice` from the middle of the word, which no
  edit distance will do. See
  [docs/decisions/0007-fuzzy-vocabulary-walk.md](docs/decisions/0007-fuzzy-vocabulary-walk.md).

- `lq serve` — one warm process answering queries from stdin, to skip the
  ~36 ms process-spawn floor that dominates repeated one-shot searches.
  Directives: `:limit N`, `:fuzzy on|off`, `:substring on|off`,
  `:snippet on|off`, `:quit`.
- `lq status` — file count, database size, last-indexed time.
- `lq compact` — `PRAGMA optimize` plus `VACUUM`. A space feature, not a
  speed feature.
- `lq doctor` — resolved Tesseract library plus every candidate path tried in
  order, language packs, `TESSDATA_PREFIX`, database path, schema version,
  applied PRAGMAs. The thing to paste into a bug report.
- Runtime `dlopen` of libtesseract, so `cargo install lensquery` succeeds on a
  machine with no Tesseract and fails later with an actionable message. See
  [docs/decisions/0005-runtime-dlopen-tesseract.md](docs/decisions/0005-runtime-dlopen-tesseract.md).
- Cross-platform library resolution: ordered candidate filename lists per
  platform, Homebrew and common Linux library directories, and a
  `LENSQUERY_TESSERACT_LIB` override for anything exotic.
- Optional TOML configuration at `~/.lensquery/config.toml`.
- Two installed binaries, `lq` and `lensquery`, for the `$PATH` collision with
  the unrelated `lq` crate. See
  [docs/decisions/0004-crate-name-lensquery.md](docs/decisions/0004-crate-name-lensquery.md).

### Known limitations

- OCR text only. No semantic or CLIP-style image search.
- No PDF support.
- Tesseract must be installed separately.
- Typo tolerance works on whole words. If OCR only recovered a fragment of a
  word, `--substring` reaches it and `--fuzzy` does not.
- No GUI.

[0.1.0]: https://github.com/mihovilrak/lensquery/releases/tag/v0.1.0
