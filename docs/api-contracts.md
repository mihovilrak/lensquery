# API Contracts

The frozen public surface of the `lensquery` crate. Anything listed here is
depended on by another module, by a test, or by a user's shell pipeline.
Changing a signature, a field name, an output line shape, or an exit code is a
breaking change and needs its own PR — not a drive-by edit inside a feature
task.

Three things in this document are load-bearing beyond the usual "don't rename
it" sense, and each is called out where it appears:

- The **three-state OCR result** (`Ocr::Text` / `Ocr::Empty` / `Ocr::Failed`).
- The **FTS5 content column index** (`1`, not `0`) and passing the table to
  `snippet()` **by name, not by alias**.
- The **`serve` blank-line block terminator**, which is the client's only
  framing signal and therefore its only protection against deadlock.

Module docs (`cargo doc --open`) carry the *why* for individual decisions. This
file carries the *what may not change*.

---

## `lensquery::models`

Plain data, shared by every other module. All three types derive
`Debug, Clone, PartialEq, Serialize, Deserialize`.

**Fields are frozen. Adding a field is fine; renaming or removing one is
breaking.** New optional fields must carry `#[serde(default)]` so an older
serialized form still deserializes.

```rust
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub mtime: f64,          // fractional Unix seconds
    pub indexed_at: String,  // ISO-8601
}

pub struct SearchResult {
    pub path: String,
    pub score: f64,          // FTS5 `rank` (BM25); more negative = better
    #[serde(default)]
    pub snippet: Option<String>,
    #[serde(default)]
    pub mtime: f64,          // as `files.mtime` stores it
    #[serde(default)]
    pub lang: Option<String>,  // None for a row written before `files.lang`
}

pub struct IndexStats {
    pub indexed: u64,
    pub updated: u64,
    pub skipped: u64,
    pub failed: u64,
    pub duration_seconds: f64,
    #[serde(default)]
    pub deleted: u64,
}
```

`SearchResult::snippet` is `None` from the non-snippet search entry points and
`Some(_)` from the snippet ones — never `Some("")` as a stand-in for "no
excerpt".

`SearchResult::lang` is `None` for any row indexed before `files.lang` existed,
which is not the same as "no language was used" — it is unknown. Both surface
as `null` in JSON output.

---

## `lensquery::db`

**All SQL lives in this module.** No other module builds a statement, and every
user-supplied string reaches `MATCH` through `build_fts5_query`. That is the
entire injection story, and it only holds while this stays the single door.

### Schema

```rust
pub const SCHEMA_VERSION: i64 = 2;
```

Bumped only when an existing index stops being readable, which forces a wipe
and re-index rather than a silent upgrade that would return wrong results.

v2 established the invariant the module depends on: **an fts5 row's `rowid` is
the owning `files.id`**. That makes a delete or replace a B-tree probe instead
of a scan over the `UNINDEXED file_id` column. It is a property of how rows are
written, not something the schema can enforce, so it cannot be detected at
runtime — hence the version gate. `connect` refuses to open a v1 database.

**A new column is not a version bump.** `init_schema` calls
`add_missing_columns`, which `ALTER TABLE ... ADD COLUMN`s anything the
`CREATE TABLE IF NOT EXISTS` could not add to a table that already exists
(`files.lang` is the first). Purely additive columns are version-neutral in
both directions: an older binary never names the column, and a newer one reads
`NULL` as "written before this column existed". Bumping instead would refuse
every index already on disk in order to gain a column nothing needs to be
correct.

Tables:

| Table | Purpose |
| --- | --- |
| `files(id, path UNIQUE, mtime, indexed_at, lang)` | one row per indexed image |
| `ocr_text` | fts5, `tokenize = 'porter unicode61'` — standard search |
| `ocr_text_trigram` | fts5, `tokenize = 'trigram'` — `--substring` search |
| `ocr_vocab` | `fts5vocab(ocr_text, 'row')` — the porter term dictionary |
| `meta(key, value)` | holds `schema_version` |

Both fts5 tables are `(file_id UNINDEXED, content)`. **`content` is column
index 1.** Column 0 is the `UNINDEXED file_id`; asking `snippet()` for column 0
silently snippets the rowid instead of failing.

`files.path` is `UNIQUE`, which already gives SQLite an implicit index that
serves every lookup an explicit `idx_files_path` would, so that index is
explicitly `DROP`ped (databases built by earlier versions stop paying for it
too). `idx_files_mtime` is kept.

### PRAGMAs

Applied on every `connect()`, in this order:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA temp_store   = MEMORY;
PRAGMA cache_size   = -64000;
PRAGMA foreign_keys = ON;
```

WAL mode is non-negotiable — the indexer's reader and its single writer thread
overlap.

### Errors

```rust
pub enum DbError {
    Sqlite(rusqlite::Error),
    SchemaMismatch { found: i64, expected: i64, path: String },
    WriterPanic,
}
```

`WriterPanic` is distinct from `Sqlite` because no SQL call returned an error:
the indexer's single writer thread unwound. Everything committed before that
point is durable (one transaction per batch) so a re-run resumes, but the run
as a whole is a failure and must not report success.

### Functions

```rust
pub fn connect(db_path: &Path) -> Result<Connection, DbError>;
pub fn init_schema(conn: &Connection) -> Result<(), DbError>;

pub fn upsert_file(conn: &Connection, path: &str, mtime: f64, indexed_at: &str,
                   text: &str, lang: Option<&str>) -> Result<i64, DbError>;
pub fn delete_files(conn: &Connection, paths: &[String]) -> Result<u64, DbError>;
pub fn get_file_mtime(conn: &Connection, path: &str) -> Result<Option<f64>, DbError>;

pub fn search_standard(conn: &Connection, query: &str, limit: i64)
    -> Result<Vec<SearchResult>, DbError>;
pub fn search_substring(conn: &Connection, query: &str, limit: i64)
    -> Result<Vec<SearchResult>, DbError>;
pub fn search_fuzzy(conn: &Connection, query: &str, limit: i64,
                    max_distance: Option<u32>)
    -> Result<Vec<SearchResult>, DbError>;
pub fn search_standard_snippet(conn: &Connection, query: &str, limit: i64)
    -> Result<Vec<SearchResult>, DbError>;
pub fn search_substring_snippet(conn: &Connection, query: &str, limit: i64)
    -> Result<Vec<SearchResult>, DbError>;
pub fn search_fuzzy_snippet(conn: &Connection, query: &str, limit: i64,
                            max_distance: Option<u32>)
    -> Result<Vec<SearchResult>, DbError>;

pub fn stats(conn: &Connection) -> Result<DbStats, DbError>;
pub fn compact(conn: &Connection) -> Result<CompactStats, DbError>;
pub fn build_fts5_query(raw: &str) -> String;
```

`connect` creates the parent directory, applies the PRAGMAs, ensures the
schema, and checks the version. `:memory:` is recognized and skips the
directory work.

`upsert_file` writes both fts5 tables as `INSERT OR REPLACE INTO t(rowid,
file_id, content) VALUES (?1, ?1, ?2)`. FTS5 has no `ON CONFLICT`, but it does
honour `INSERT OR REPLACE` on the rowid, which collapses the delete-then-insert
pair into one rowid-addressed statement. The alternative — deleting by the
`UNINDEXED file_id` column — has no index to use (`EXPLAIN QUERY PLAN` reports
`SCAN ocr_text VIRTUAL TABLE`) and made upserts quadratic in corpus size:
measured at 2.9 / 7.0 / 12.1 / 22.7 ms per upsert as the table grew through
2k / 4k / 8k / 16k rows, and at 25.166 ms/image against a real 19,839-row
corpus — 54.9x a new-path write, or 1.221 us per existing row.

`delete_files` prepares every statement through `prepare_cached` (the SQL text
is the cache key, so a batch compiles once, not once per path) and addresses
the fts5 rows by `rowid`, giving O(paths x log corpus) rather than
O(paths x corpus).

Six separate search entry points rather than `snippet: bool` parameters: Rust
has no default arguments, and most callers want the cheap form and would
otherwise all carry a bare `false` at the call site.

`search_substring*` is what `search_fuzzy*` used to be — the trigram table,
renamed when `--fuzzy` became typo tolerance. `search_fuzzy*` now runs the
vocabulary walk described in
[0007-fuzzy-vocabulary-walk.md](decisions/0007-fuzzy-vocabulary-walk.md):
`max_distance` of `None` takes the length-scaled default, `Some(0)` makes it a
plain `search_standard*`.

**Every public entry point calls `build_fts5_query` exactly once**, and the
private `search` behind them takes text that is *already* sanitized. Fuzzy
expansion rewrites the query after sanitization, so it re-checks every term it
pulls out of `ocr_vocab` with `is_bareword` before splicing it into an `OR`
group. Index content must never become query syntax.

### Snippets

`snippet()` budgets in **tokens**, and the two tables tokenize differently:

| Table | Token | Window |
| --- | --- | --- |
| `ocr_text` (porter) | a word | 12 — about a line of prose |
| `ocr_text_trigram` | a single character | 64 — SQLite's hard ceiling, ~60 chars |

Reusing the porter budget of 12 on the trigram table returns a 12-*character*
stub. That is the trap that originally kept `snippet()` off the substring path.

Two frozen details of the generated SQL:

- `snippet()` takes the fts5 table **by name**, not by the query's alias.
  Passing the alias is a `no such column: t` error.
- The content column index is **1**.

Every snippet is whitespace-collapsed inside `db`, not at the print site. OCR
content is mostly newlines; a raw snippet would carry them to stdout and break
the one-result-per-line contract that both `lq search` and `lq serve` rest on.
Collapsing here keeps the two commands from each inventing their own version.

### `compact`

```rust
pub struct CompactStats { pub before_bytes: i64, pub after_bytes: i64 }
impl CompactStats { pub fn reclaimed(&self) -> i64 }
```

`PRAGMA optimize` then `VACUUM`, on the bare connection (`VACUUM` cannot run
inside a transaction). `optimize` goes first so the planner's stat tables are
updated against the pages about to be written out.

**This is a space operation, not a speed one.** Measured yield tracks how
churned the file is: 9.78% reclaimed on a long-lived 19,839-image index, 3.00%
on a freshly built one. Query latency does not move either way — the difference
sits inside the ~15% run-to-run spread of a spawn-dominated search. Do not bolt
it onto the tail of `lq index`: `VACUUM` rewrites the whole database, so a 20k
image index would pay a full copy of a multi-hundred-MB file for a saving the
user did not ask for and cannot skip.

### `build_fts5_query`

**Every user-supplied search string goes through this before reaching `MATCH`.
No bypassing.**

- Balanced double quotes are treated as an intentional phrase query and passed
  through unchanged.
- An unbalanced quote would crash the FTS5 parser, so quotes are stripped and
  the input is sanitized like any other.
- FTS5 barewords allow only alphanumerics; every other token is wrapped in
  double quotes.
- One exception: a **trailing `*`** passes through unquoted. It is FTS5's
  prefix operator, and quoting it turns the token into a phrase whose tokenizer
  drops the `*`, silently converting a prefix search into an exact-term search
  that returns nothing whenever the stem is not itself a word. The rest of the
  token must still be entirely alphanumeric, so this cannot open a quote, start
  a column filter, or introduce a boolean operator.

The bareword rule is the allowlist half of the sanitizer. Widen it only with a
test: anything that gets through reaches the FTS5 parser as syntax, not text.

---

## `lensquery::fuzzy`

The edit-distance half of `--fuzzy`. It knows nothing about SQLite; `db` owns
the vocabulary walk and calls in here to decide what counts as near.

```rust
pub const MAX_DISTANCE: u32 = 3;
pub const STEM_SUFFIX: usize = 2;

pub fn budget(len: usize) -> u32;
pub fn distance_within(query: &str, candidate: &str, max: u32) -> Option<u32>;
pub fn stem_distance_within(query: &str, stem: &str, max: u32) -> Option<u32>;
```

| Item | Contract |
| --- | --- |
| `MAX_DISTANCE` | Ceiling on `--fuzzy-distance`. A guard rail, not a recommendation. |
| `STEM_SUFFIX` | Trailing characters `stem_distance_within` forgives, because the porter vocabulary holds stems. |
| `budget(len)` | 0 for ≤3 characters, 1 for 4–6, 2 above. Short words are mostly *distinct* words. |
| `distance_within` | OSA distance after shape folding, or `None` once every alignment exceeds `max`. |
| `stem_distance_within` | Same, with up to `STEM_SUFFIX` characters free off the end of `query`. |

Contracts:

- **Both sides are folded before they are compared.** Confusions OCR actually
  makes — `1`/`l`/`i`/`|`, `0`/`o`, `rn`/`m`, `cl`/`d` — cost nothing, so they
  do not eat the budget a real typo needs. The fold is lossy on purpose.
- **The fold order is load-bearing.** Ligatures run first, on lowercased text;
  the single-stroke rules run second. Reversed, the `i` in `invoice` becomes a
  stroke, that stroke and the `c` before it read as `cl`, and a word containing
  no `cl` collapses to `lnvode`.
- **`stem_distance_within` is one-directional.** `stem` is the indexed side,
  `query` the typed side. Swapping the arguments asks a different question and
  gets a different answer.
- **`None` means "further than `max`", never "error".** Both functions abandon
  a comparison as soon as every path through the row exceeds the budget.
- **Distances are in characters, not bytes.** Everything runs over `Vec<char>`.

---

## `lensquery::ocr`

### The three-state result — frozen

```rust
pub enum Ocr {
    Text(String),  // non-empty recognized text
    Empty,         // image read fine, held no text — "" IS written to the DB
    Failed,        // decode or engine failure — NO DB row is written
}
```

**Never collapse these into two cases.** `Empty` means "we looked and there is
nothing", which is a cacheable answer; `Failed` means "we do not know", which
must be retried on the next run. Merging them either re-OCRs blank images
forever or permanently hides failures.

### Functions

```rust
pub fn extract_text(image_path: &Path, lang: &str, min_conf: f32,
                    thorough: bool, thorough_trigger_words: usize) -> Ocr;

pub fn extract_text_timed(image_path: &Path, lang: &str, min_conf: f32,
                          thorough: bool, thorough_trigger_words: usize)
    -> (Ocr, StageTimings);

pub struct StageTimings { pub decode: f64, pub preprocess: f64, pub recognize: f64 }
impl StageTimings { pub fn total(&self) -> f64 }

pub const DEFAULT_THOROUGH_TRIGGER_WORDS: usize = 3;
```

`extract_text_timed` is the real implementation and the one the indexer runs in
production; `extract_text` is a thin wrapper. Four `Instant::now()` calls
against a ~250 ms image are unmeasurable, and keeping two copies of the
pipeline would let them drift.

`extract_text` **never panics** — every failure path returns `Ocr::Failed`.

`thorough` is the opt-in two-arm mode: when the primary sparse-text pass yields
`thorough_trigger_words` words or fewer, the same pixel buffer is re-read at
`PSM_UNIFORM_BLOCK` and the union of both arms' words is returned. A failing
second arm **never** downgrades a successful primary result — it degrades to
the primary text. `thorough_trigger_words` is inert unless `thorough` is set.

### Pipeline constants (measured, see [benchmarks.md](benchmarks.md))

| Constant | Value | Note |
| --- | --- | --- |
| `MIN_WIDTH` | 1000 | upscale below this |
| `UPSCALE_WIDTH` | 1200 | 1500 was indistinguishable on findability (0.674 vs 0.678) for ~23% more OCR time; *not* upscaling is a real loss |
| `MAX_WIDTH` | 2400 | downscale above this |
| `DEFAULT_THOROUGH_TRIGGER_WORDS` | 3 | 5 scored identically on every paired image at +26.7% wall time vs 3's +12.1%; 1 was indistinguishable from thorough-off |

The decoder is given explicit `image::Limits` (max dimension and max
allocation). `ImageReader` defaults to *no* limits, so a corrupt or hostile
header can otherwise ask for an unbounded allocation.

The grayscale scratch buffer is a `thread_local` reused across every image the
thread OCRs. It never escapes `extract_text_timed` — the function returns an
owned `String` — so the zero-copy handling is invisible at this boundary and
**no signature here moves because of it**. Thread-local rather than a
parameter, because the indexer runs one OCR thread per worker and they must not
share it.

---

## `lensquery::tess`

Runtime `dlopen` of libtesseract via `libloading` — never build-time linking.
That is what lets `cargo install lensquery` succeed on a machine with no
Tesseract, and lets `lq doctor` explain the situation instead of the binary
failing to start. See
[decisions/0005-runtime-dlopen-tesseract.md](decisions/0005-runtime-dlopen-tesseract.md).

```rust
pub const PSM_SPARSE: c_int = 11;
pub const PSM_UNIFORM_BLOCK: c_int = 6;
pub const LIB_ENV_VAR: &str = "LENSQUERY_TESSERACT_LIB";

pub struct Attempt { pub path: PathBuf, pub outcome: Outcome }
pub enum Outcome {
    NotFound,             // nothing at this path
    OpenFailed(String),   // present but the loader refused it (arch mismatch,
                          // or one of ITS OWN dependencies missing)
    MissingSymbols,       // loaded, but exports no Tesseract C API
    Loaded,               // the one in use
}

pub fn attempts() -> &'static [Attempt];
pub fn install_hint() -> &'static str;
pub fn init_process_env();
pub fn available() -> bool;
pub fn library_path() -> Option<&'static Path>;
pub fn tessdata_path() -> Option<PathBuf>;
pub fn available_langs() -> Vec<String>;
pub fn shutdown();
pub fn recognize(data: &[u8], width: i32, height: i32, lang: &str,
                 min_conf: f32, psm: c_int) -> Option<String>;
```

### Contracts

**`osd` is not a language.** It never appears in a `lang=` string handed to
`recognize`. It is an orientation/script-detection model and putting it there
fails initialization.

**`recognize` returns `Option`, and the two arms are not interchangeable.**
`Some("")` means the page was read successfully and contained nothing worth
indexing (e.g. every word fell below `min_conf`). `None` means engine-level
failure. The caller must map these to `Ocr::Empty` and `Ocr::Failed`
respectively — see the three-state contract above.

`min_conf` is a word-confidence floor (0–100). `0` keeps everything and takes
the cheaper whole-page text path.

`psm` re-modes the calling thread's cached engine **in place** — no re-init, so
no model reload — and it stays on that mode until a call asks for another.

**`init_process_env()` must be called from `main`, before any thread is
spawned.** It sets `OMP_THREAD_LIMIT=1` if unset. The library load happens
inside a `OnceLock` initialiser reached from whichever worker OCRs first, while
other workers are already running; `setenv` there mutates a process-global
table another thread may be reading, which is a data race, not merely a race on
a value (on glibc a concurrent `setenv` can free the block a `getenv` caller is
still reading). The `OnceLock` serialises its own initialiser, not the rest of
the program.

**`shutdown()` must be called on each worker thread before the pool joins, and
on the main thread before exit.** Tesseract's C++ static destructors run during
library teardown and race the automatic drop of a thread-local engine, printing
spurious ObjectCache "LEAK" warnings for a run that actually succeeded.

The load is attempted exactly once per process and **failure is cached** — a
load that failed once fails the same way every time, and retrying per image
would be tens of thousands of pointless `dlopen` calls.

`attempts()` is the full resolution trail in the order tried, kept for the life
of the process so `doctor` can show every path and why each was rejected.
"libtesseract not found" is the single most likely first-run failure, and the
two most common causes — wrong architecture, and a missing dependency of
libtesseract's own — both *look* like absence.

Candidate filenames, in order:

| Platform | Candidates |
| --- | --- |
| Windows | `libtesseract-5.dll`, `libtesseract.dll`, `tesseract55.dll`, `tesseract54.dll`, `tesseract53.dll` |
| macOS | `libtesseract.5.dylib`, `libtesseract.dylib` |
| other | `libtesseract.so.5`, `libtesseract.so` |

`LENSQUERY_TESSERACT_LIB` names a library file directly and bypasses the search
entirely — the escape hatch for a Nix store path, a custom build, or a
container.

`tessdata_path()` returns `TESSDATA_PREFIX` if set, **as-is, even if it does
not exist** ("you configured this and it does not exist" is the diagnosis, and
hiding it would leave `doctor` silent about the actual cause), otherwise the
existence-checked `tessdata` directory beside the loaded library.

---

## `lensquery::indexer`

```rust
pub enum Progress { Start(u64), Advance(u64) }

pub struct IndexOptions<'a> {
    pub full_reindex: bool,
    pub lang: &'a str,
    pub workers: usize,           // 0 -> available_parallelism()
    pub min_conf: f32,
    pub thorough: bool,
    pub thorough_trigger_words: usize,
    pub verbose: bool,
    pub engine: &'a str,          // accepted for CLI parity; currently ignored
    pub failed_log: Option<&'a Path>,
    pub progress: Option<&'a (dyn Fn(Progress) + Sync)>,
}

pub fn index_directory(root: &Path, db_path: &Path, opts: &IndexOptions)
    -> Result<IndexStats, DbError>;

pub fn discover(root: &Path) -> Vec<(PathBuf, f64)>;
```

`IndexOptions` replaced ten positional parameters, four of which were bare
`bool`/`&str` — a transposed pair compiled cleanly and silently changed what
the run did. Construct from `Default` and override what matters.

### Contracts

- **Per-image errors are counted in `IndexStats.failed`, never propagated.**
  DB-level errors do propagate. One unreadable file must not end a run over
  20,000 images.
- **Worker callables are module-level functions**, never closures or methods
  captured into the pool.
- `Progress::Start` fires once with the number of images that will be
  processed; `Progress::Advance` fires after each batch commits, carrying that
  batch's size. Nothing is emitted for skipped or pruned files.
- `root` is canonicalized **and recased against the filesystem**, so the same
  directory indexed via different CWDs, spellings or letter case stores
  identical path strings and incremental skip keeps working. Recasing is not
  redundant with `canonicalize`: on macOS the latter is `realpath`, which
  returns the caller's casing verbatim on a case-insensitive volume. Only the
  root is treated this way — every path below it is named by `read_dir` or by
  the watcher, which already report the real spelling.
- The mtime comes free with the directory walk and is carried all the way to
  the writer: **no file is stat'd twice in a run.** Carrying a slightly stale
  mtime is safe — a file changed mid-run simply re-indexes next run.
- One transaction per batch, so an interrupted run is resumable rather than
  lost.
- `failed_log` is truncated at start, so it describes *this* run. Each line is
  `path\treason`. Without it a run over tens of thousands of images reports
  failures as a bare count, which is not actionable.
- `discover` is public so `examples/stage_profile.rs` profiles *the same file
  set the indexer would index*; a profiler picking files by its own rule would
  answer a question about a different corpus. On Windows it reports a symlink's
  own mtime rather than its target's, so edits to the target do not re-trigger
  OCR — `--full-reindex` covers that.

### Dry run

```rust
pub struct DryRun {
    pub to_process: u64,          // new + changed
    pub new_files: u64,
    pub changed: u64,
    pub skipped: u64,             // unchanged on mtime
    pub stale: u64,               // indexed rows whose file is gone
    pub sample: Vec<PathBuf>,     // <= SAMPLE_IMAGES, strided across the walk
}

impl DryRun {
    pub fn estimated_seconds(&self, seconds_per_image: f64, workers: usize) -> f64;
}

pub const SAMPLE_IMAGES: usize = 33;              // measured sample + one warm-up
pub const SAMPLE_BUDGET_SECONDS: f64 = 12.0;
pub const REFERENCE_SECONDS_PER_IMAGE: f64 = 0.839;

pub fn dry_run(root: &Path, db_path: &Path) -> Result<DryRun, DbError>;
pub fn sample_seconds_per_image(sample: &[PathBuf], opts: &IndexOptions)
    -> Option<(f64, u32)>;                        // (rate, images measured)
pub fn estimate_band(measured_images: Option<u32>) -> f64;
```

- **Counts are exact, and exact by construction:** `dry_run` reaches the same
  mtime skip predicate a real run does rather than reimplementing it. A count
  that could drift from the run it predicts is worse than no count.
- **Nothing is written.** `dry_run` reads stored mtimes only when the database
  already exists and never creates one, so a dry run over a directory that has
  never been indexed leaves the filesystem exactly as it found it.
- `sample` is strided (`step_by`) through the `discover` order, not taken from
  its front: a date-sorted corpus has its cheap screenshots and its expensive
  photographed pages in different eras, and a sample bunched at one end reads
  the wrong rate.
- `sample_seconds_per_image` **discards the first image** — it pays the OCR
  engine's model load — and returns `None` when no image in the sample could be
  read, which the caller reports as a rate rather than silently substituting
  one. It stops early once `SAMPLE_BUDGET_SECONDS` of measuring is spent, but
  never at zero measurements: a corpus can begin with a run of unreadable
  files, which cost time without producing any.
- **The band widens as the sample shrinks.** `estimate_band` is a flat share
  for the speedup curve (a one-point fit that knows nothing about the caller's
  core topology) plus the standard error of the sample's own mean. A machine
  too slow to time many images says so by being vague instead of by being
  confidently wrong. Callers print `estimate * (1 +/- band)`.

---

## `lensquery::watch`

Turns filesystem events into indexing work. It decides *which paths to offer*
and never whether a file needs OCR — that stays in `indexer::index_paths`.

```rust
pub fn default_workers() -> usize;

pub fn run(
    root: &Path,
    db_path: &Path,
    debounce: Duration,
    index_opts: &indexer::IndexOptions,
    report: &dyn Fn(Event),
) -> Result<(), WatchError>;

pub enum Event {
    Ready(PathBuf),
    Renamed { from: String, to: String },
    Deleted(u64),
    Indexed(IndexStats),
    Warning(String),
}

pub enum WatchError { IndexInsideRoot { db, root }, Db(DbError), Notify(notify::Error) }
```

### Contracts

- **Watch mode is not a second indexing path.** Every batch goes through
  `indexer::index_paths`, so the mtime skip, the failure isolation, the
  per-batch transaction, and the supported-extension list are the indexer's,
  not a copy. `lq watch` and `lq index` over the same tree produce the same
  database.
- **Extension filtering happens before anything is stat'd or opened**, against
  `indexer::is_supported` — the same list the directory walk uses, so no
  format can be indexable but unwatchable.
- **`Access(_)` events are dropped.** Reads are not changes, and a directory
  someone is browsing must not wake the indexer.
- **Quiet-period debounce with a ceiling.** Every event restarts the timer, so
  a burst is one batch however long it runs — up to `MAX_BATCH` (60 s), which
  exists so a directory under continuous write still makes progress.
- **A move is a rename, not an OCR job.** A vanished path and an appeared one
  in the same batch are paired by mtime and become one `UPDATE`. A vanished
  path with no partner is a delete; one that was never indexed is nothing.
- **`run` refuses to start when the index is inside `root`** — every DB write
  (and every `-wal`/`-shm` write) is an event that would arrive back at the
  loop. The extension filter would drop it today; the guard is against the
  design, not one filename.
- **`default_workers()` is half the cores, minimum 1**, deliberately below
  `index`'s pool: a watcher runs while its user is still working in the tree.
- `run` returns only when the event channel closes. It does not daemonize,
  and it writes nothing to stdout.

---

## `lensquery::config`

```rust
pub struct Config {
    pub default_db: PathBuf,
    pub languages: String,
    pub workers: u32,
    pub default_limit: u32,
    pub engine: String,
    pub min_word_conf: f32,
    pub thorough: bool,
    pub thorough_trigger_words: u32,
}

pub enum ConfigError {
    Parse { path: PathBuf, source: toml::de::Error },
    Invalid(String),
    Io { path: PathBuf, source: std::io::Error },
}

pub fn load(path: Option<&Path>) -> Result<Config, ConfigError>;
pub fn expanduser(p: &str) -> PathBuf;
```

Default config path is `~/.lensquery/config.toml`; default database is
`~/.lensquery/index.db`.

| Field | Default | TOML location |
| --- | --- | --- |
| `default_db` | `~/.lensquery/index.db` | `[index] db` |
| `languages` | `eng` | `[index] languages` |
| `workers` | `0` (auto) | `[index] workers` |
| `engine` | `auto` (`auto`/`dll`/`cli`) | `[index] engine` |
| `min_word_conf` | `40.0` | `[index] min_word_conf` |
| `thorough` | `false` | `[index] thorough` |
| `thorough_trigger_words` | `3` | `[index] thorough_trigger_words` |
| `default_limit` | `20` | `[search] limit` |

Contracts:

- **A missing config file is not an error** — `load` returns defaults.
- **Unknown keys are ignored deliberately**, so a config written by a newer
  version does not break an older binary.
- `min_word_conf` accepts a TOML integer *or* float (custom deserializer);
  writing `min_word_conf = 40` must not be a parse error.
- `expanduser` expands a leading `~` only.

---

## `lensquery::lang`

Language packs: what exists, what is installed, and how to install more. The
manifest is compiled in with `include_str!`, so `lq lang available` works with
no network and no data files beside the binary.

```rust
pub const OSD: &str = "osd";
pub const OFFLINE_ENV_VAR: &str = "LENSQUERY_OFFLINE";

pub struct Manifest { pub source: String, pub tag: String,
                      pub generated: String, pub packs: Vec<Pack> }
pub struct Pack { pub code: String, pub name: String, pub kind: Kind,
                  pub size: u64, pub sha256: String }
pub enum Kind { Language, Detector }

pub enum Error {
    OsdIsNotALanguage { spec }, EmptyCode { spec }, BadCode { code },
    UnknownPack { code }, NoInstallDir(String),
    Offline { reason, repo, tag, code, dir },
    Download { code, msg },
    Checksum { code, want, got, tag },
    Io(String),
}

pub fn manifest() -> &'static Manifest;
pub fn find(code: &str) -> Option<&'static Pack>;
pub fn parts(spec: &str) -> impl Iterator<Item = &str>;
pub fn validate(spec: &str) -> Result<(), Error>;
pub fn user_dir() -> Option<PathBuf>;
pub fn user_dir_is_active() -> bool;
pub fn codes_in(dir: &Path) -> impl Iterator<Item = String>;
pub fn install_dir() -> Result<PathBuf, Error>;
pub fn shadowed_by_install(install: &Path) -> Vec<String>;
pub fn adopt(from: &Path, install: &Path, codes: &[String]) -> Result<(), Error>;
pub fn suggest_from_locale() -> Option<(&'static str, &'static Pack)>;
pub fn url(pack: &Pack) -> String;
pub fn is_offline(flag: bool) -> bool;
pub fn fetch(pack: &Pack, dir: &Path, offline: bool,
             progress: &mut dyn FnMut(u64, u64)) -> Result<PathBuf, Error>;
```

### Invariants

- **`validate` is the single gate for any `lang=` string**, and it rejects `osd`
  by name rather than filtering it out. A user who typed it gets told why it was
  wrong. Called from `config::load` and from `cli::cmd_index`.
- **A download is verified before it is visible.** Bytes go to `<code>.traineddata.part`
  and are renamed only after the size and SHA-256 both match the manifest entry;
  a mismatch deletes the file and returns `Error::Checksum`. There is no code
  path that installs an unverified pack.
- **The manifest tag is pinned** (`tag = "4.1.0"`). `url` builds
  `{source}/raw/{tag}/{code}.traineddata` from the manifest, never from a
  caller-supplied base, so nothing can redirect an install elsewhere.
- **`install_dir` prefers the directory Tesseract already reads** and falls back
  to `user_dir()` only when that one is not writable. Tesseract accepts exactly
  one datapath, so installing into the fallback hides the system packs:
  `shadowed_by_install` reports what would be hidden and `adopt` copies those
  packs across — **after** a successful install, never before, so a failed or
  offline `lang add` cannot move the search path.
- **`suggest_from_locale` suggests and returns**, it never installs, and it
  returns `None` when the matching pack is already present.
- `is_offline(flag)` is true for the flag, or for `LENSQUERY_OFFLINE` set to
  anything other than `0`, `false`, or empty.
- Everything that touches the network lives behind the **`download` cargo
  feature** (on by default). `--no-default-features` removes `ureq` and `sha2`
  from the dependency graph entirely; `fetch` then returns `Error::Offline` with
  the manual-install instructions.

---

## `lensquery::cli`

`pub fn main() -> ExitCode`, plus `src/main.rs` (`lq`) and
`src/bin/lensquery.rs` (`lensquery`), which are both one line calling it. The
two binaries are the same program under two names — the short one to type, the
long one to discover.

### The output contract

**`lq` composes with other tools. Results go to stdout and only results do.**
Progress bars, warnings, hints, and errors go to stderr, so a pipe carries the
answer and nothing else. The progress bar is rendered to stderr and indicatif
hides it automatically when stderr is not a terminal.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | success (including "no results found") |
| 1 | runtime user error — currently only `search --open` with no results |
| 2 | environment or usage error — bad argument value, malformed config, schema-version mismatch, missing libtesseract, `compact` with no database |

### Precedence

CLI flag > config file > built-in default, resolved once per invocation.

### `--offline`

Global. `--offline`, or `LENSQUERY_OFFLINE` set to anything but `0`/`false`/empty,
makes `lq lang add` refuse to reach the network and print the manual-install
instructions instead. It exists so a user can prove the tool stays local; nothing
else in `lq` opens a socket, and `serve` speaks stdin/stdout, not TCP.

### Commands

```
lq index <DIRECTORY> [--db PATH] [--full-reindex] [--lang STR] [--workers N]
                     [--engine auto|dll] [--min-conf 0-100]
                     [--thorough | --no-thorough] [--thorough-trigger-words N]
                     [--failed-log PATH] [-v]
lq watch <DIRECTORY> [--db PATH] [--lang STR] [--workers N] [--debounce SECS]
                     [--engine auto|dll] [--min-conf 0-100]
                     [--thorough | --no-thorough] [--thorough-trigger-words N] [-v]
lq search <QUERY> [--db PATH] [--limit N] [--open] [--fuzzy]
                  [--fuzzy-distance N] [--substring]
                  [--snippet | --no-snippet] [--json] [-v]
lq serve [--db PATH] [--limit N] [--fuzzy] [--fuzzy-distance N]
         [--substring] [--snippet] [-v]
lq compact [--db PATH]
lq status  [--db PATH] [--json]
lq doctor  [--db PATH] [--json]
lq lang [list]
lq lang available [FILTER]
lq lang add <CODE>...
lq lang remove <CODE>...
lq lang path

--offline is global and applies to every command.
```

`lq lang` with no subcommand is `lq lang list`, so the bare word is never an
error.

`--no-thorough` beats `--thorough` if both are given; without it a config
`thorough = true` could not be turned off from the command line at all.

`--engine cli` (shelling out to the `tesseract` executable per image) parses but
is **rejected by name** with exit 2. Accepting it and quietly doing library work
instead would misreport what ran.

`lq index` and `lq watch` check `tess::available()` up front and fail with exit
2 if no library loaded. Without that check, every image would come back failed
and a run over 20,000 images would take an hour to say so. Both go through one
`preflight` function, so the two cannot drift into disagreeing about what a
runnable environment is.

`lq watch --debounce` accepts 0.1-300 seconds; anything else is exit 2. `lq
watch` refuses to start if `--db` resolves inside the watched directory, also
exit 2.

### Result line shape — frozen

```
[<score>\t]<path>[\t<snippet>]
```

Field order is fixed, so a caller can split on `\t` knowing only which flags it
passed. `<score>` appears only with `-v`/`--verbose`, formatted `{:.4}`.
`<snippet>` appears unless `--no-snippet` is given — `lq search` prints the
excerpt by default, `lq serve` does not (its contract is one path per line, and
a client opts in with `:snippet on`). The snippet is whitespace-collapsed
in `db`, so it can never introduce a second line — which in `serve` would also
mean a spurious block terminator.

Empty results print `No results found.` to **stdout** and exit 0, followed by
the matcher the user has not tried yet: `Try --fuzzy for approximate matching.`
on the standard path, `Try --substring to match inside words.` on the fuzzy
one, and nothing after `--substring`, which is the end of the line. With
`--open` and no results, the message goes to stderr and the exit code is 1.

### `--json`

`search`, `status`, and `doctor` take `--json`. `search` emits **JSON Lines**
(one object per result, no enclosing array) so it stays streamable; `status`
and `doctor` emit one object. In this mode **only JSON reaches stdout** — the
`No results found.` line moves to stderr and an empty result set is an empty
stream, still exit 0. Field order is part of the format and the schema is
documented in [json-output.md](json-output.md): adding a field is
backward-compatible, renaming or removing one is not, same discipline as
`models`.

### `lq status` / `lq doctor` output

`key: value`, one per line, fixed order. Absent optional values print the
literal `None` rather than being omitted, so a caller can parse the block
without knowing which keys to expect.

`status` prints `db_path` then `file_count`, `db_bytes`, `last_indexed_at`,
`schema_version`.

`doctor` prints `lq_version`, `platform`, `libtesseract`,
`libtesseract_search: N candidates` (followed by **indented** continuation
lines, one per attempt, so the `key: value` contract still holds for anything
grepping it), `tessdata_path`, `tesseract_langs`, the config defaults
(`engine_default`, `languages_default`, `min_word_conf_default`,
`thorough_default`, `thorough_trigger_words_default`), `db_path`, and then
either the `status` block or `db_status: not_initialized`. The facts are
gathered once by a private `doctor()` and handed to one of two renderers, so
the text and `--json` forms cannot describe different machines.

`tessdata_install_dir` is where `lq lang add` would write — the same directory
as `tessdata_path` when that one is writable, otherwise the per-user fallback,
or `<none>` when neither can be determined.

`lq lang path` prints `tessdata_dir`, `install_dir`, `user_dir` in that order,
same `key: value` contract.

`compact` prints `db_path`, `before_bytes`, `after_bytes`, `reclaimed_bytes`,
`reclaimed_percent`.

`index` prints one summary line:
`Summary: N indexed, N updated, N skipped, N failed, N deleted (in T.Ts)`.

### `lq serve` — the line protocol

`lq search` pays a fresh process spawn per query — measured at 36.3 ms, none of
it search, against a query that itself costs single-digit milliseconds. That
floor is process spawn plus dynamic linking, so no amount of work inside
`search` can move it. `serve` pays it once and then answers on an already-open
connection.

**One response block per non-blank input line, always terminated by exactly one
blank line**, so a client reads until blank without counting:

| Input | Behaviour |
| --- | --- |
| blank line | ignored, **no** response block |
| `:quit` | exit 0 |
| `:limit N` | set the result limit (positive integer) |
| `:fuzzy on` / `:fuzzy off` | switch typo tolerance on and off |
| `:substring on` / `:substring off` | switch trigram/porter matching |
| `:snippet on` / `:snippet off` | append the matched excerpt to each line |
| anything else | a query; matching result lines, one per line |

- Paths are never empty, so a blank line is unambiguously a terminator.
- **A bad directive or a failed query reports to stderr and still emits its
  (empty) block.** A client must never block waiting for a response that is not
  coming.
- **The output is flushed before the next read.** Without that the client
  deadlocks: it waits for a block sitting in our buffer, and we wait for its
  next line.
- A malformed query is a per-line event, not a reason to tear down a warm
  process the client is still talking to.
- Consequence of the `:` prefix: a query cannot begin with a colon. FTS5 has no
  leading-colon syntax, so nothing searchable is lost.
- `-v` is start-only — there is no `:verbose` directive.
- Exit 0 on EOF or `:quit`; exit 2 when the database cannot be opened.

---

## Cross-cutting rules

- **All SQL in `db`**; all user input through `build_fts5_query`.
- **The three OCR states stay three.**
- **`osd` is never a language** — `lang::validate` rejects it by name.
- **No pack is installed without a checksum match** against the pinned manifest.
- **WAL on every `connect()`.**
- **`models` fields are additive-only.**
- `panic = "unwind"` in the release profile is required — the indexer wraps
  each image in `catch_unwind`, and `panic = "abort"` would turn one bad image
  into a lost run.
- `--fuzzy` is **typo tolerance** — bounded edit distance over the term
  dictionary, with OCR shape confusions folded out. `--substring` is the
  trigram infix match. The two are mutually exclusive and clap enforces it.
