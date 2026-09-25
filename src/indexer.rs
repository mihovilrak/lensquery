//! Directory walking and multi-threaded OCR fan-out.
//!
//! One process with a worker-thread pool. Each thread owns a thread-local
//! `TessBaseAPI` ([`crate::tess`]) that it reuses across images, because
//! creating an engine costs more than OCRing an image with it. Workers push
//! batch results over a channel to a **single writer thread** that owns the
//! `rusqlite::Connection` — WAL allows exactly one writer — and commits one
//! transaction per batch.
//!
//! Two structural choices worth knowing before editing:
//!
//!   * **Std only**, no `walkdir`/`crossbeam`/`num_cpus`: recursive `read_dir`,
//!     `thread::scope` with an `AtomicUsize` work cursor, `std::sync::mpsc`,
//!     `available_parallelism`. Nothing here is hard enough to be worth the
//!     dependencies, and this is the module most likely to pull in a tree.
//!   * **The indexer is UI-free.** It emits [`Progress`] events; the CLI renders
//!     the bar. No terminal crate leaks in here, so the indexer stays usable
//!     from a test, a library caller, or a future non-terminal front end.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::db::{self, DbError};
use crate::models::IndexStats;
use crate::ocr::{self, Ocr};

/// Image extensions we index (lower-cased, no leading dot).
const SUPPORTED_EXTS: [&str; 8] = ["jpg", "jpeg", "png", "webp", "gif", "bmp", "tiff", "tif"];

/// Small batches keep every worker busy on modest datasets and bound each
/// commit transaction.
const BATCH_SIZE: usize = 16;

/// Progress events emitted during a run so a caller (the CLI) can render a bar
/// without owning the pool. `Start` fires once with the number of images that
/// will be processed; `Advance` fires after each batch commits with that
/// batch's size. Nothing is emitted for skipped/pruned files.
pub enum Progress {
    Start(u64),
    Advance(u64),
}

/// A progress sink shared with the writer thread; `Sync` so a single reference
/// can cross the scope boundary into `writer_loop`.
type ProgressFn<'a> = &'a (dyn Fn(Progress) + Sync);

/// Everything an index run needs beyond "which directory, which database".
///
/// Replaces the ten positional parameters `index_directory` used to take: four
/// of them were bare `bool`/`&str`, so a transposed pair compiled cleanly and
/// silently changed what the run did. Construct from `Default` and override
/// what matters:
///
/// ```ignore
/// let opts = IndexOptions { lang: "eng+hrv", workers: 8, ..Default::default() };
/// ```
pub struct IndexOptions<'a> {
    /// Re-OCR every file even when the stored `mtime` still matches.
    pub full_reindex: bool,
    /// Tesseract language string, handed straight to `ocr::extract_text`.
    pub lang: &'a str,
    /// Worker threads; `0` → `available_parallelism()`.
    pub workers: usize,
    /// Word-confidence floor (0–100).
    pub min_conf: f32,
    /// Opt-in two-arm OCR (see `ocr::extract_text`).
    pub thorough: bool,
    /// Word count at or below which `thorough` fires its second arm. Inert
    /// unless `thorough` is set.
    pub thorough_trigger_words: usize,
    /// Accepted for CLI-surface parity; there is no logging layer yet.
    pub verbose: bool,
    /// Accepted for CLI-surface parity and forward compatibility. The runtime
    /// `dlopen`ed library is the only engine, so this is currently ignored.
    pub engine: &'a str,
    /// When set, every failed image is appended here as `path\treason`. A run
    /// over tens of thousands of images reports failures as a bare count
    /// otherwise, which is not actionable.
    pub failed_log: Option<&'a Path>,
    /// When `Some`, receives `Progress` events for a UI.
    pub progress: Option<ProgressFn<'a>>,
}

impl Default for IndexOptions<'_> {
    fn default() -> Self {
        Self {
            full_reindex: false,
            lang: "eng",
            workers: 0,
            min_conf: 0.0,
            thorough: false,
            thorough_trigger_words: ocr::DEFAULT_THOROUGH_TRIGGER_WORDS,
            verbose: false,
            engine: "auto",
            failed_log: None,
            progress: None,
        }
    }
}

/// Walk `root`, OCR new/changed images, persist to `db_path`.
///
/// Errors on a single image are counted in `IndexStats.failed`, never
/// propagated; DB-level errors do propagate. See `IndexOptions` for the knobs.
pub fn index_directory(
    root: &Path,
    db_path: &Path,
    opts: &IndexOptions,
) -> Result<IndexStats, DbError> {
    let start = Instant::now();
    // Canonicalize so the same directory indexed via different CWDs, spellings
    // or letter case stores identical path strings (and incremental skip works).
    let root = resolve_root(root);
    let mut conn = db::connect(db_path)?;

    let all_paths = discover(&root);

    let existing_mtimes = load_existing_mtimes(&conn)?;
    let deleted = prune_stale(&conn, &root, &all_paths, &existing_mtimes)?;

    index_known_files(&mut conn, all_paths, &existing_mtimes, deleted, start, opts)
}

/// What [`index_directory`] would do, reported without doing any of it.
///
/// The counts are exact — the same skip predicate the real run uses decides
/// them. `sample` is a handful of the images that would be OCR'd, spread across
/// the corpus, for a caller that wants to time this machine rather than trust a
/// published rate; see [`sample_seconds_per_image`].
///
/// No image is OCR'd and no row is inserted, updated, or deleted. An absent
/// database is read as "nothing is indexed yet" rather than created, so a dry
/// run over a fresh directory leaves no trace. An *existing* one is opened
/// normally, which applies any pending schema migration exactly as every other
/// command does.
pub struct DryRun {
    /// Images that would be OCR'd. The number the time estimate is for.
    pub to_process: u64,
    /// Of `to_process`, paths the index has never seen.
    pub new_files: u64,
    /// Of `to_process`, paths already indexed whose mtime has moved.
    pub changed: u64,
    /// Images that would be skipped because their stored mtime still matches.
    pub skipped: u64,
    /// Rows under the root whose file is gone; they would be deleted.
    pub stale: u64,
    /// Up to [`SAMPLE_IMAGES`] of the images that would be OCR'd.
    pub sample: Vec<PathBuf>,
}

/// How many images [`dry_run`] hands back for timing. One of them pays the
/// engine's model load, so this is the measured sample size plus one.
///
/// A ceiling, not a target: [`sample_seconds_per_image`] stops early once
/// [`SAMPLE_BUDGET_SECONDS`] is spent, so the ceiling only binds on a machine
/// fast enough to reach it. 32 measured images is where more of them stop
/// moving the answer. Per-image cost is spread wide and skewed right — a
/// corpus of phone screenshots and photographed documents has images costing
/// several times what their neighbours do — but the error of the mean falls
/// off as 1/sqrt(n), so 32 lands well inside the +/-30% band the CLI prints.
/// Four proved too few: on a 19,839-image corpus they read 0.61 s/image
/// against a true 0.84.
pub const SAMPLE_IMAGES: usize = 33;

/// Wall-clock seconds [`sample_seconds_per_image`] may spend measuring, past
/// the warm-up image. A dry run exists to answer "how long will this take?"
/// before committing to a long run, so it may not itself be a long run; 12 s
/// buys ~14 images at the reference rate and the whole sample above it.
pub const SAMPLE_BUDGET_SECONDS: f64 = 12.0;

/// Flat part of the printed range: [`parallel_speedup`] is a one-point fit and
/// knows nothing about this machine's core topology.
const CURVE_UNCERTAINTY: f64 = 0.30;

/// Spread of per-image cost across a mixed corpus of screenshots and
/// photographed pages, as a coefficient of variation. Divided by sqrt(n) it is
/// the standard error of the sample's own mean.
///
/// 0.7 rather than the 0.6 the timings themselves show, because the sample is a
/// stride through a directory walk and not a random draw: on the one corpus
/// whose true cost is known, 21 sampled images read 0.58 s/image against a run
/// that averaged 0.84. The extra 0.1 is what makes the printed range cover that.
const PER_IMAGE_CV: f64 = 0.7;

/// Range half-width when the rate is another machine's rather than this one's.
const UNMEASURED_BAND: f64 = 0.6;

/// Half-width of the range an estimate should be printed with, as a fraction of
/// the estimate, given how many images the rate was measured over.
///
/// Two error sources, added rather than combined in quadrature: a range that is
/// wider than it needed to be costs a user nothing, and one that excludes the
/// truth costs them the trust the whole dry run is for. The curve contributes a
/// flat share; the sample contributes the error of its own mean, which shrinks
/// as the timing budget buys more images.
pub fn estimate_band(measured_images: Option<u32>) -> f64 {
    match measured_images {
        Some(n) if n > 0 => CURVE_UNCERTAINTY + PER_IMAGE_CV / f64::from(n).sqrt(),
        _ => UNMEASURED_BAND,
    }
}

/// Single-image cost on the reference machine, for when nothing can be sampled.
/// 28 ms decode + 103 ms preprocess + 708 ms recognize. docs/benchmarks.md §1.
pub const REFERENCE_SECONDS_PER_IMAGE: f64 = 0.839;

impl DryRun {
    /// Wall-clock seconds the run would take, given `seconds_per_image` of
    /// single-threaded work per image and the run's `workers` setting (`0` =
    /// auto, resolved the way the real run resolves it).
    pub fn estimated_seconds(&self, seconds_per_image: f64, workers: usize) -> f64 {
        self.to_process as f64 * seconds_per_image / parallel_speedup(worker_count(workers))
    }
}

/// Count what a run over `root` would do. See [`DryRun`].
pub fn dry_run(root: &Path, db_path: &Path) -> Result<DryRun, DbError> {
    let root = resolve_root(root);
    let existing_mtimes = if db_path.exists() {
        load_existing_mtimes(&db::connect(db_path)?)?
    } else {
        HashMap::new()
    };

    let all_paths = discover(&root);
    let present: HashSet<&str> = all_paths.iter().filter_map(|(p, _)| p.to_str()).collect();
    let stale = existing_mtimes
        .keys()
        .filter(|path| !present.contains(path.as_str()) && Path::new(path).starts_with(&root))
        .count() as u64;

    let mut plan = DryRun {
        to_process: 0,
        new_files: 0,
        changed: 0,
        skipped: 0,
        stale,
        sample: Vec::new(),
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    for (path, current_mtime) in &all_paths {
        match existing_mtimes
            .get(path.to_string_lossy().as_ref())
            .copied()
        {
            Some(stored) if stored == *current_mtime => {
                plan.skipped += 1;
                continue;
            }
            Some(_) => plan.changed += 1,
            None => plan.new_files += 1,
        }
        plan.to_process += 1;
        candidates.push(path.clone());
    }

    // Spread the sample across the corpus instead of taking the first few:
    // photo directories sort by date, and the images at one end of a run are
    // routinely a different size and subject from those at the other.
    let stride = (candidates.len() / SAMPLE_IMAGES).max(1);
    plan.sample = candidates
        .into_iter()
        .step_by(stride)
        .take(SAMPLE_IMAGES)
        .collect();
    Ok(plan)
}

/// Time `sample` sequentially to learn what one image costs *here*: seconds per
/// image, and how many images that average is over.
///
/// Measured rather than quoted because per-image cost moves with CPU clock,
/// image size, language count, and model tier, and those vary more between
/// machines than core count does. The first image is a warm-up — it pays the
/// engine's model load, which a real run pays once per worker and not once per
/// image — so it is timed but not counted, and the budget clock starts after
/// it. Measuring then stops at [`SAMPLE_BUDGET_SECONDS`] or at the end of the
/// sample, whichever comes first.
///
/// `None` when nothing in the sample could be read, which is the caller's signal
/// to fall back to the published rate rather than print a made-up one.
pub fn sample_seconds_per_image(sample: &[PathBuf], opts: &IndexOptions<'_>) -> Option<(f64, u32)> {
    let mut total = 0.0;
    let mut measured = 0u32;
    let mut clock = Instant::now();
    for (i, path) in sample.iter().enumerate() {
        let started = Instant::now();
        let result = ocr_one(
            path,
            0.0,
            opts.lang,
            opts.min_conf,
            opts.thorough,
            opts.thorough_trigger_words,
        );
        if i == 0 {
            clock = Instant::now();
            continue;
        }
        if !matches!(result.ocr, Ocr::Failed) {
            total += started.elapsed().as_secs_f64();
            measured += 1;
        }
        // Never stop at zero measurements: images that fail to read cost time
        // without producing any, and a corpus can begin with a run of them.
        if measured > 0 && clock.elapsed().as_secs_f64() >= SAMPLE_BUDGET_SECONDS {
            break;
        }
    }
    crate::tess::shutdown();
    (measured > 0).then(|| (total / f64::from(measured), measured))
}

/// Wall-clock speedup from `workers`, fitted to the one measured point.
///
/// 839 ms of single-image work came out as 252 ms/image of wall clock on 8
/// workers — 3.33x, not 8x, because those 8 workers sat on 4 physical cores.
/// `w^0.58` passes through that point and through the trivial one (one worker,
/// no speedup). It is a one-point fit and it under-promises on a machine whose
/// cores are all physical, which is part of why the CLI prints a range rather
/// than a number. See docs/benchmarks.md §1.
fn parallel_speedup(workers: usize) -> f64 {
    (workers.max(1) as f64).powf(0.58)
}

/// OCR an explicit list of files into `db_path`, skipping unchanged ones.
///
/// The incremental entry point behind `lq watch`: filesystem events name the
/// files, so there is nothing to walk and nothing to prune — a path absent from
/// the list was not touched, not deleted. Everything past "which files" is
/// [`index_known_files`], the same code `index_directory` runs, so watch mode
/// cannot drift into a second indexing path with its own idea of "unchanged".
///
/// Non-images and paths that no longer exist are dropped silently; a file
/// deleted between the event and this call is normal.
pub fn index_paths(
    paths: &[PathBuf],
    db_path: &Path,
    opts: &IndexOptions,
) -> Result<IndexStats, DbError> {
    let start = Instant::now();
    let mut conn = db::connect(db_path)?;
    let existing_mtimes = load_existing_mtimes(&conn)?;
    // `resolve` matches the spelling `index_directory` stores; without it the
    // same image indexed both ways would occupy two rows.
    let with_mtimes: Vec<(PathBuf, f64)> = paths
        .iter()
        .filter(|p| is_supported(p))
        .filter_map(|p| {
            let path = resolve(p);
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((path, unix_seconds(modified)))
        })
        .collect();
    index_known_files(&mut conn, with_mtimes, &existing_mtimes, 0, start, opts)
}

/// Skip-or-OCR `all_paths` against the mtimes already stored, then write.
///
/// `deleted` and `start` belong to the caller and are carried through only so
/// both entry points can return one `IndexStats`.
fn index_known_files(
    conn: &mut Connection,
    all_paths: Vec<(PathBuf, f64)>,
    existing_mtimes: &HashMap<String, f64>,
    deleted: u64,
    start: Instant,
    opts: &IndexOptions,
) -> Result<IndexStats, DbError> {
    // Decide skip-vs-process. The mtime came free with the directory walk and
    // is carried all the way to the writer, so no file is ever stat'd twice in
    // a run. Carrying a slightly stale mtime is safe: a file changed mid-run
    // just re-indexes on the next run.
    let mut to_process: Vec<(PathBuf, f64)> = Vec::new();
    let mut new_paths: HashSet<String> = HashSet::new();
    let mut skipped: u64 = 0;
    for (path, current_mtime) in &all_paths {
        let key = path.to_string_lossy().into_owned();
        let stored = existing_mtimes.get(&key).copied();
        if !opts.full_reindex && stored == Some(*current_mtime) {
            skipped += 1;
            continue;
        }
        if stored.is_none() {
            new_paths.insert(key);
        }
        to_process.push((path.clone(), *current_mtime));
    }

    if let Some(p) = opts.progress {
        p(Progress::Start(to_process.len() as u64));
    }

    if to_process.is_empty() {
        return Ok(IndexStats {
            indexed: 0,
            updated: 0,
            skipped,
            failed: 0,
            duration_seconds: start.elapsed().as_secs_f64(),
            deleted,
        });
    }

    let n_workers = worker_count(opts.workers);

    // A log that cannot be opened is a warning, never a reason to abandon a
    // multi-hour run before it starts.
    let failed_log = opts
        .failed_log
        .and_then(|path| match open_failed_log(path) {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!(
                    "warning: could not open --failed-log {}: {e}; failures will only be counted",
                    path.display()
                );
                None
            }
        });

    let batches = chunked(to_process, BATCH_SIZE);
    let (indexed, updated, failed) =
        run_pool(conn, batches, n_workers, &new_paths, opts, failed_log)?;

    Ok(IndexStats {
        indexed,
        updated,
        skipped,
        failed,
        duration_seconds: start.elapsed().as_secs_f64(),
        deleted,
    })
}

/// Resolve `IndexOptions::workers`: `0` means one thread per logical core.
///
/// Public so a caller that has to *say* how many threads a run will use — the
/// dry run does — reports the same number the run will actually start.
pub fn worker_count(requested: usize) -> usize {
    if requested > 0 {
        return requested;
    }
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// One image's outcome as it travels from a worker to the writer. `reason` is
/// set only for failures, and only so `--failed-log` can say *why* — the
/// counting logic keys off `ocr` alone.
struct ImageResult {
    path: PathBuf,
    mtime: f64,
    ocr: Ocr,
    reason: &'static str,
}

/// A single batch's OCR outcomes, as sent from a worker to the writer.
type BatchResult = Vec<ImageResult>;

/// Run the worker-thread pool and the single writer thread, returning the
/// `(indexed, updated, failed)` counts. DB errors from the writer propagate.
fn run_pool(
    conn: &mut Connection,
    batches: Vec<Vec<(PathBuf, f64)>>,
    n_workers: usize,
    new_paths: &HashSet<String>,
    opts: &IndexOptions,
    failed_log: Option<FailedLog>,
) -> Result<(u64, u64, u64), DbError> {
    let cursor = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<BatchResult>();
    let batches_ref = &batches;
    let cursor_ref = &cursor;
    let progress = opts.progress;
    let (lang, min_conf, thorough) = (opts.lang, opts.min_conf, opts.thorough);
    let trigger = opts.thorough_trigger_words;

    thread::scope(|scope| {
        // Single writer thread owns the connection (WAL = single writer) and
        // commits one transaction per batch.
        let writer =
            scope.spawn(move || writer_loop(conn, rx, new_paths, lang, progress, failed_log));

        // Worker threads pull batches off the shared cursor, OCR each image on
        // their thread-local Tesseract engine, and forward results. Each tears
        // its engine down before exit — a thread-local holding a raw handle is
        // not dropped for us.
        for _ in 0..n_workers.max(1) {
            let tx = tx.clone();
            scope.spawn(move || {
                loop {
                    let i = cursor_ref.fetch_add(1, Ordering::Relaxed);
                    if i >= batches_ref.len() {
                        break;
                    }
                    let batch = &batches_ref[i];
                    let mut out: BatchResult = Vec::with_capacity(batch.len());
                    for (path, mtime) in batch {
                        out.push(ocr_one(path, *mtime, lang, min_conf, thorough, trigger));
                    }
                    // A disconnected writer (it errored out) ends the worker.
                    if tx.send(out).is_err() {
                        break;
                    }
                }
                crate::tess::shutdown();
            });
        }
        // Drop our sender so the writer's `recv` ends once all workers finish.
        drop(tx);

        // A panicking writer must not panic the whole pool: report it as an
        // error so the caller exits non-zero with everything already committed
        // still durable. `WriterPanic` is the only way this arm is reached.
        match writer.join() {
            Ok(result) => result,
            Err(_) => Err(DbError::WriterPanic),
        }
    })
}

/// OCR one image, containing any panic to that image.
///
/// `ocr::extract_text` is documented never to panic, and a panic crossing the
/// FFI boundary from Tesseract itself would be undefined behaviour this cannot
/// help with. What it does catch is everything on our side of the boundary —
/// an allocation failure or an arithmetic edge in a decoder, on one file out of
/// tens of thousands — which without this kills the worker thread and, at 20k
/// images, several hours of committed-but-unfinished work. A caught panic is
/// just another failed image.
///
/// Requires unwinding panics; the release profile therefore sets
/// `panic = "unwind"` (see `rust/Cargo.toml`).
fn ocr_one(
    path: &Path,
    mtime: f64,
    lang: &str,
    min_conf: f32,
    thorough: bool,
    thorough_trigger_words: usize,
) -> ImageResult {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ocr::extract_text(path, lang, min_conf, thorough, thorough_trigger_words)
    }));
    let (ocr, reason) = match caught {
        Ok(Ocr::Failed) => (Ocr::Failed, "decode or engine failure"),
        Ok(other) => (other, ""),
        Err(_) => (Ocr::Failed, "panic during OCR"),
    };
    ImageResult {
        path: path.to_path_buf(),
        mtime,
        ocr,
        reason,
    }
}

/// Consume batch results, upserting each into one transaction per batch.
///
/// `Ocr::Failed` → counted in `failed`, no DB row (matches "`None` never writes
/// a row"). `Ocr::Empty` → stored as `""`. New vs existing paths split
/// `indexed`/`updated`. A DB error aborts and propagates.
fn writer_loop(
    conn: &mut Connection,
    rx: mpsc::Receiver<BatchResult>,
    new_paths: &HashSet<String>,
    lang: &str,
    progress: Option<ProgressFn>,
    mut failed_log: Option<FailedLog>,
) -> Result<(u64, u64, u64), DbError> {
    let mut indexed = 0u64;
    let mut updated = 0u64;
    let mut failed = 0u64;

    while let Ok(batch) = rx.recv() {
        let batch_len = batch.len() as u64;
        let now = now_iso8601();
        let tx = conn.transaction()?;
        for item in batch {
            let text = match item.ocr {
                Ocr::Failed => {
                    failed += 1;
                    if let Some(log) = failed_log.as_mut() {
                        log_failure(log, &item.path, item.reason);
                    }
                    continue;
                }
                Ocr::Empty => String::new(),
                Ocr::Text(t) => t,
            };
            let key = item.path.to_string_lossy();
            db::upsert_file(&tx, &key, item.mtime, &now, &text, Some(lang))?;
            if new_paths.contains(key.as_ref()) {
                indexed += 1;
            } else {
                updated += 1;
            }
        }
        tx.commit()?;
        // Flush per batch, not per run: a run killed at hour three should still
        // leave behind the list of what had failed by then.
        if let Some(log) = failed_log.as_mut() {
            let _ = log.flush();
        }
        if let Some(p) = progress {
            p(Progress::Advance(batch_len));
        }
    }
    Ok((indexed, updated, failed))
}

/// Append one `path<TAB>reason` line. Write errors are swallowed deliberately:
/// losing the diagnostic log is not a reason to fail an otherwise good run.
fn log_failure(log: &mut FailedLog, path: &Path, reason: &str) {
    let _ = writeln!(log, "{}\t{}", path.display(), reason);
}

/// Buffered sink for `--failed-log`.
type FailedLog = std::io::BufWriter<std::fs::File>;

/// Create (or truncate) the failed-image log.
///
/// Truncating rather than appending keeps the file a report of *this* run;
/// resuming an interrupted index would otherwise interleave two runs' failures
/// with no marker between them.
fn open_failed_log(path: &Path) -> std::io::Result<FailedLog> {
    Ok(std::io::BufWriter::new(std::fs::File::create(path)?))
}

/// Read the stored `{path: mtime}` map used for incremental skip decisions.
fn load_existing_mtimes(conn: &Connection) -> Result<HashMap<String, f64>, DbError> {
    let mut stmt = conn.prepare("SELECT path, mtime FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (path, mtime) = row?;
        map.insert(path, mtime);
    }
    Ok(map)
}

/// Delete rows **under `root`** whose files no longer exist on disk; return the
/// count. Rows outside `root` are never touched, so indexes spanning multiple
/// directories survive re-indexing one of them — that is what the
/// `Path::starts_with` guard below is for.
fn prune_stale(
    conn: &Connection,
    root: &Path,
    discovered: &[(PathBuf, f64)],
    existing_mtimes: &HashMap<String, f64>,
) -> Result<u64, DbError> {
    let present: HashSet<&str> = discovered.iter().filter_map(|(p, _)| p.to_str()).collect();
    let stale: Vec<String> = existing_mtimes
        .keys()
        .filter(|path| !present.contains(path.as_str()) && Path::new(path).starts_with(root))
        .cloned()
        .collect();
    if stale.is_empty() {
        return Ok(0);
    }
    db::delete_files(conn, &stale)
}

/// Recursively collect supported image files under `root`, each paired with its
/// modification time as fractional Unix seconds.
///
/// Directories are recursed via their `file_type` (symlinks are not followed
/// into, avoiding cycles); anything else with a supported extension is kept.
/// A walk over a real photo corpus hits permission errors and half-deleted
/// entries, so unreadable directories are skipped and an entry whose metadata
/// cannot be read is dropped, rather than failing the whole run.
///
/// **The mtime must come from `DirEntry::metadata()`, not a separate
/// `fs::metadata()` call.** The directory entry already carries it; asking the
/// filesystem again costs 23.6× as much per file and is what decides whether
/// re-indexing an unchanged corpus feels instant. See
/// [benchmarks](../docs/benchmarks.md) §1. (On Windows this reports a symlink's
/// own mtime rather than its target's, so edits to the target do not re-trigger
/// OCR; `--full-reindex` covers that.)
///
/// Public so `examples/stage_profile.rs` can profile *the same file set the
/// indexer would index*. A profiler that picked files by its own rule would
/// answer a question about a different corpus.
pub fn discover(root: &Path) -> Vec<(PathBuf, f64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
            } else if is_supported(&path) {
                if let Some(modified) = entry.metadata().ok().and_then(|m| m.modified().ok()) {
                    out.push((path, unix_seconds(modified)));
                }
            }
        }
    }
    out
}

/// A `SystemTime` as fractional Unix seconds. Pre-epoch timestamps clamp to
/// `0.0` rather than failing — a nonsensical mtime is still a usable cache key.
fn unix_seconds(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// True when `path`'s extension (case-insensitive) is a supported image type.
///
/// Public so `lq watch` filters filesystem events against the same list the
/// walk uses — two lists would mean a format the indexer reads but the watcher
/// ignores.
pub fn is_supported(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let lower = ext.to_ascii_lowercase();
            SUPPORTED_EXTS.contains(&lower.as_str())
        }
        None => false,
    }
}

/// Split into fixed-size batches.
fn chunked(items: Vec<(PathBuf, f64)>, n: usize) -> Vec<Vec<(PathBuf, f64)>> {
    items.chunks(n).map(<[_]>::to_vec).collect()
}

/// Resolve `root` to an absolute canonical path, stripping the Windows `\\?\`
/// verbatim prefix. The prefix is an API detail of `canonicalize`, not part of
/// the path the user typed, and storing it would make every path in the
/// database unrecognizable to the person reading search output.
fn resolve(root: &Path) -> PathBuf {
    match std::fs::canonicalize(root) {
        Ok(p) => strip_verbatim(p),
        Err(_) => root.to_path_buf(),
    }
}

/// [`resolve`] plus [`recase`] - use this for the one path a run is rooted at.
///
/// Everything below the root is named by `read_dir` or by the filesystem
/// watcher, both of which report the real on-disk spelling. The root is the
/// only path a *person* spells, so it is the only one that can arrive
/// mis-cased, and recasing it costs one `read_dir` per ancestor, once per run.
pub(crate) fn resolve_root(root: &Path) -> PathBuf {
    recase(&resolve(root))
}

/// Rewrite every component of `path` to the spelling the filesystem stores.
///
/// `canonicalize` already does this on Windows, but on macOS it is `realpath`,
/// which hands back whatever the caller typed on a case-insensitive volume.
/// Without this, `lq index ~/Photos` and `lq index ~/photos` are two rows per
/// image. It runs on every platform rather than behind a `cfg(target_os)` so
/// the behaviour is identical everywhere and testable anywhere: on a
/// case-sensitive filesystem the exact name matches first and the path comes
/// back unchanged.
///
/// A component that cannot be resolved (unreadable directory, non-UTF-8 name)
/// is kept as given, so the worst case is the behaviour we had before.
fn recase(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::Normal(name) => match on_disk_name(&out, name) {
                Some(real) => out.push(real),
                None => out.push(name),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The entry of `dir` whose name equals `name`, exactly or ignoring ASCII case.
fn on_disk_name(dir: &Path, name: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    let wanted = name.to_str()?;
    let mut insensitive = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let found = entry.file_name();
        match found.to_str() {
            Some(text) if text == wanted => return Some(found),
            Some(text) if text.eq_ignore_ascii_case(wanted) && insensitive.is_none() => {
                insensitive = Some(found);
            }
            _ => {}
        }
    }
    insensitive
}

/// Strip a leading Windows `\\?\` extended-length prefix if present.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    if cfg!(windows) {
        if let Some(s) = p.to_str() {
            if let Some(rest) = s.strip_prefix(r"\\?\") {
                return PathBuf::from(rest);
            }
        }
    }
    p
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`. Hand-rolled: one timestamp
/// format in one place is not worth a date crate.
fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Convert days since the Unix epoch to `(year, month, day)` in the proleptic
/// Gregorian calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lq_idx_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_png(path: &Path) {
        let img = image::RgbImage::new(40, 40);
        image::DynamicImage::ImageRgb8(img).save(path).unwrap();
    }

    #[test]
    fn recase_leaves_an_exactly_spelled_path_alone() {
        let dir = tmpdir("recase_exact");
        let sub = dir.join("MixedCase");
        std::fs::create_dir_all(&sub).unwrap();
        write_png(&sub.join("Shot.PNG"));

        let target = sub.join("Shot.PNG");
        assert_eq!(recase(&target), target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recase_keeps_components_it_cannot_resolve() {
        let dir = tmpdir("recase_missing");
        let missing = dir.join("nope").join("deeper.png");
        assert_eq!(recase(&missing), missing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Case-insensitive filesystems only: on Linux `MixedCase` and `mixedcase`
    // are two different directories, so there is no way to reach one file by
    // two spellings and nothing to deduplicate.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn resolve_root_normalises_a_miscased_root_to_one_spelling() {
        let dir = tmpdir("recase_root");
        let real = dir.join("MixedCase");
        std::fs::create_dir_all(&real).unwrap();

        let shouted = dir.join("MIXEDCASE");
        let whispered = dir.join("mixedcase");
        assert_eq!(resolve_root(&shouted), resolve_root(&whispered));
        assert_eq!(
            resolve_root(&shouted).file_name().unwrap(),
            real.file_name().unwrap(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The regression this is all for: indexing the same directory twice under
    // two spellings must leave one row per image, not two. `indexed == 0` on
    // the second run is the deterministic half — a fresh spelling would be a
    // path the DB has never seen, and every such path counts as newly indexed
    // whether or not libtesseract is present to produce text for it.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn indexing_a_miscased_root_twice_does_not_duplicate_rows() {
        let dir = tmpdir("case_dup");
        let real = dir.join("Photos");
        std::fs::create_dir_all(&real).unwrap();
        write_png(&real.join("a.png"));
        let db_path = dir.join("index.db");

        let opts = IndexOptions::default();
        index_directory(&real, &db_path, &opts).unwrap();
        let second = index_directory(&dir.join("photos"), &db_path, &opts).unwrap();

        assert_eq!(second.indexed, 0, "second spelling read as a new file");
        assert_eq!(second.updated, 0);

        let conn = db::connect(&db_path).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert!(
            rows <= 1,
            "second spelling of the root added a duplicate row"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_supported_matches_case_insensitively() {
        assert!(is_supported(Path::new("a.PNG")));
        assert!(is_supported(Path::new("a.jpeg")));
        assert!(is_supported(Path::new("a.Tif")));
        assert!(!is_supported(Path::new("a.txt")));
        assert!(!is_supported(Path::new("noext")));
    }

    #[test]
    fn discover_recurses_and_filters() {
        let dir = tmpdir("disc");
        write_png(&dir.join("top.png"));
        std::fs::write(dir.join("note.txt"), b"x").unwrap();
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        write_png(&sub.join("nested.jpg"));

        let mut found: Vec<String> = discover(&dir)
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, vec!["nested.jpg", "top.png"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_carries_a_usable_mtime() {
        // The mtime rides along with the walk instead of being fetched by a
        // second stat per file, so it has to be the *same value* the old
        // `fs::metadata` path produced — the incremental skip compares it for
        // exact equality against what is stored in the DB.
        let dir = tmpdir("disc_mtime");
        let file = dir.join("top.png");
        write_png(&file);

        let found = discover(&dir);
        assert_eq!(found.len(), 1);
        let expected = unix_seconds(std::fs::metadata(&file).unwrap().modified().unwrap());
        assert_eq!(found[0].1, expected);
        assert!(found[0].1 > 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunked_splits_evenly_and_remainder() {
        let items: Vec<(PathBuf, f64)> = (0..35)
            .map(|i| (PathBuf::from(format!("{i}")), i as f64))
            .collect();
        let batches = chunked(items, BATCH_SIZE);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 16);
        assert_eq!(batches[2].len(), 3);
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-07-21 is 20655 days after the epoch.
        assert_eq!(civil_from_days(20_655), (2026, 7, 21));
    }

    #[test]
    fn now_iso8601_shape() {
        let s = now_iso8601();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
    }

    #[test]
    fn prune_only_touches_paths_under_root() {
        let c = db::connect(Path::new(":memory:")).unwrap();
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\data\root")
        } else {
            PathBuf::from("/data/root")
        };
        let under = root.join("gone.png");
        let outside = if cfg!(windows) {
            PathBuf::from(r"C:\other\keep.png")
        } else {
            PathBuf::from("/other/keep.png")
        };
        db::upsert_file(&c, &under.to_string_lossy(), 1.0, "t", "a", None).unwrap();
        db::upsert_file(&c, &outside.to_string_lossy(), 2.0, "t", "b", None).unwrap();

        let mut existing = HashMap::new();
        existing.insert(under.to_string_lossy().into_owned(), 1.0);
        existing.insert(outside.to_string_lossy().into_owned(), 2.0);

        // Nothing discovered under root → the under-root row is stale, the
        // outside row is left alone.
        let deleted = prune_stale(&c, &root, &[], &existing).unwrap();
        assert_eq!(deleted, 1);
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            db::get_file_mtime(&c, &outside.to_string_lossy()).unwrap(),
            Some(2.0)
        );
    }

    #[test]
    fn index_directory_full_run_counts_are_consistent() {
        // Deterministic regardless of whether libtesseract is present: an
        // image either OCRs (Empty/Text → stored) or the engine is missing
        // (Failed → not stored), but every processed image lands in exactly
        // one of indexed/updated/failed.
        let dir = tmpdir("run");
        write_png(&dir.join("a.png"));
        write_png(&dir.join("b.png"));
        std::fs::write(dir.join("skip.txt"), b"x").unwrap();
        let db_path = dir.join("index.db");

        let opts = IndexOptions {
            full_reindex: true,
            workers: 2,
            min_conf: 40.0,
            ..Default::default()
        };
        let stats = index_directory(&dir, &db_path, &opts).unwrap();

        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.deleted, 0);
        assert_eq!(stats.indexed + stats.updated + stats.failed, 2);
        assert!(stats.duration_seconds >= 0.0);

        // Second incremental run: unchanged files are either skipped (if they
        // were stored) or re-failed (if the engine is absent) — never both.
        let opts2 = IndexOptions {
            full_reindex: false,
            ..opts
        };
        let stats2 = index_directory(&dir, &db_path, &opts2).unwrap();
        assert_eq!(stats2.skipped + stats2.failed, 2);
        assert_eq!(stats2.indexed, 0);
        assert_eq!(stats2.updated, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_log_records_every_failure_and_only_failures() {
        // One undecodable file guarantees a failure with or without
        // libtesseract; one real PNG is a failure only when the engine is
        // missing. So the log must have one line per counted failure — the
        // invariant that makes it usable as the re-run worklist.
        let dir = tmpdir("faillog");
        write_png(&dir.join("real.png"));
        std::fs::write(dir.join("bogus.png"), b"not an image").unwrap();
        let db_path = dir.join("index.db");
        let log_path = dir.join("failed.tsv");

        let opts = IndexOptions {
            full_reindex: true,
            workers: 2,
            min_conf: 40.0,
            failed_log: Some(&log_path),
            ..Default::default()
        };
        let stats = index_directory(&dir, &db_path, &opts).unwrap();
        assert!(stats.failed >= 1);

        let body = std::fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len() as u64, stats.failed);
        assert!(lines.iter().any(|l| l.contains("bogus.png")));
        // Every line is `path<TAB>reason` with a non-empty reason.
        for line in &lines {
            let (path, reason) = line.split_once('\t').expect("tab-separated");
            assert!(!path.is_empty());
            assert!(!reason.is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ocr_one_reports_a_reason_for_undecodable_input() {
        let dir = tmpdir("ocrone");
        let bogus = dir.join("bogus.png");
        std::fs::write(&bogus, b"not an image").unwrap();

        let result = ocr_one(
            &bogus,
            1.0,
            "eng",
            40.0,
            false,
            ocr::DEFAULT_THOROUGH_TRIGGER_WORDS,
        );
        assert!(matches!(result.ocr, Ocr::Failed));
        assert!(!result.reason.is_empty());
        assert_eq!(result.mtime, 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Leave behind the row a previous run would have committed for `path`,
    /// with the mtime currently on disk unless `age` shifts it back. A path
    /// with no file behind it stands in for a row whose image was deleted, and
    /// its stored mtime never matters.
    fn commit_row(db_path: &Path, path: &Path, age: f64) {
        let conn = db::connect(db_path).unwrap();
        let on_disk = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(unix_seconds)
            .unwrap_or(0.0);
        let mtime = on_disk - age;
        db::upsert_file(
            &conn,
            &path.to_string_lossy(),
            mtime,
            "2026-08-19T00:00:00Z",
            "already indexed",
            Some("eng"),
        )
        .unwrap();
    }

    #[test]
    fn dry_run_reads_a_fresh_directory_as_all_new_and_creates_no_database() {
        let dir = tmpdir("dry_new");
        for name in ["a.png", "b.png", "c.png"] {
            write_png(&dir.join(name));
        }
        let db_path = dir.join("index.db");

        let plan = dry_run(&dir, &db_path).unwrap();

        assert_eq!(plan.to_process, 3);
        assert_eq!(plan.new_files, 3);
        assert_eq!(plan.changed, 0);
        assert_eq!(plan.skipped, 0);
        assert_eq!(plan.stale, 0);
        assert!(
            !db_path.exists(),
            "a dry run created the database it was only asked about"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_separates_unchanged_changed_new_and_gone() {
        let dir = tmpdir("dry_mixed");
        for name in ["a.png", "b.png", "c.png"] {
            write_png(&dir.join(name));
        }
        let db_path = dir.join("index.db");
        let found = discover(&resolve_root(&dir));
        let key = |name: &str| {
            found
                .iter()
                .find(|(p, _)| p.file_name().unwrap() == name)
                .map(|(p, _)| p.clone())
                .unwrap()
        };
        commit_row(&db_path, &key("a.png"), 0.0); // unchanged
        commit_row(&db_path, &key("b.png"), 500.0); // touched since
        commit_row(&db_path, &dir.join("deleted.png"), 0.0); // gone from disk

        let plan = dry_run(&dir, &db_path).unwrap();

        assert_eq!(plan.skipped, 1, "unchanged file was not skipped");
        assert_eq!(plan.changed, 1);
        assert_eq!(plan.new_files, 1);
        assert_eq!(plan.to_process, 2);
        assert_eq!(plan.stale, 1, "row for a deleted file was not counted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_samples_a_bounded_spread_of_the_work() {
        let dir = tmpdir("dry_sample");
        let total = SAMPLE_IMAGES * 3;
        for i in 0..total {
            write_png(&dir.join(format!("img{i:03}.png")));
        }

        let plan = dry_run(&dir, &dir.join("index.db")).unwrap();

        assert_eq!(plan.to_process, total as u64);
        assert_eq!(plan.sample.len(), SAMPLE_IMAGES);
        let unique: HashSet<&PathBuf> = plan.sample.iter().collect();
        assert_eq!(unique.len(), SAMPLE_IMAGES, "the sample repeats an image");

        // The point of the stride: the sample has to reach the far end of the
        // walk, not stop once it has counted out enough images at the near one.
        let walked: Vec<PathBuf> = discover(&resolve_root(&dir))
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        let last = walked
            .iter()
            .position(|path| path == plan.sample.last().unwrap())
            .unwrap();
        assert!(
            last >= walked.len() - SAMPLE_IMAGES,
            "the sample bunched at the front instead of spanning the corpus"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unmeasured_estimate_is_vaguer_than_a_sampled_one() {
        assert_eq!(estimate_band(None), estimate_band(Some(0)));
        assert!(estimate_band(None) > estimate_band(Some(SAMPLE_IMAGES as u32 - 1)));
        // One timed image is not reassurance; it is allowed to read wider than
        // no timed images at all.
        assert!(estimate_band(Some(1)) > estimate_band(Some(4)));
    }

    #[test]
    fn timing_more_images_narrows_the_range() {
        let (few, many) = (estimate_band(Some(4)), estimate_band(Some(32)));
        assert!(many < few, "more samples did not narrow the range");
        assert!(many > 0.3, "the curve's own error cannot be sampled away");
    }

    #[test]
    fn the_range_covers_a_measured_full_corpus_run() {
        // The regression this guards: a 21-image sample of the 19,839-image
        // corpus read 0.58 s/image where the run really averaged 0.84, and a
        // flat +/-30% band put the true 83 minutes outside the printed range.
        let plan = DryRun {
            to_process: 19_839,
            new_files: 19_839,
            changed: 0,
            skipped: 0,
            stale: 0,
            sample: Vec::new(),
        };
        let estimate = plan.estimated_seconds(0.58, 8);
        let upper = estimate * (1.0 + estimate_band(Some(21)));
        assert!(
            upper >= 83.0 * 60.0,
            "the printed range tops out at {:.0} min, below the measured 83",
            upper / 60.0
        );
    }

    #[test]
    fn dry_run_never_reports_more_samples_than_there_is_work() {
        let dir = tmpdir("dry_small");
        write_png(&dir.join("only.png"));

        let plan = dry_run(&dir, &dir.join("index.db")).unwrap();

        assert_eq!(plan.sample.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Resumability: a run killed at 90% must not start over. Nothing here needs
    // libtesseract — the committed row *is* what a survived batch leaves behind,
    // and the assertion is that the next run reads it as done.
    #[test]
    fn a_run_resumes_instead_of_reprocessing_what_was_already_committed() {
        let dir = tmpdir("resume");
        for name in ["a.png", "b.png", "c.png"] {
            write_png(&dir.join(name));
        }
        let db_path = dir.join("index.db");
        let done = discover(&resolve_root(&dir))[0].0.clone();
        commit_row(&db_path, &done, 0.0);

        let stats = index_directory(&dir, &db_path, &IndexOptions::default()).unwrap();

        assert_eq!(stats.skipped, 1, "committed work was done a second time");
        assert_eq!(
            stats.indexed + stats.updated + stats.failed,
            2,
            "the images left over were not the ones processed"
        );
        assert_eq!(stats.deleted, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_reindex_deliberately_gives_up_resumability() {
        let dir = tmpdir("resume_full");
        for name in ["a.png", "b.png"] {
            write_png(&dir.join(name));
        }
        let db_path = dir.join("index.db");
        for (path, _) in discover(&resolve_root(&dir)) {
            commit_row(&db_path, &path, 0.0);
        }

        let opts = IndexOptions {
            full_reindex: true,
            ..Default::default()
        };
        let stats = index_directory(&dir, &db_path, &opts).unwrap();

        assert_eq!(stats.skipped, 0, "--full-reindex skipped a file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_one_worker_estimate_is_just_the_per_image_cost_times_the_work() {
        let plan = DryRun {
            to_process: 100,
            new_files: 100,
            changed: 0,
            skipped: 0,
            stale: 0,
            sample: Vec::new(),
        };
        assert!((plan.estimated_seconds(0.5, 1) - 50.0).abs() < 1e-9);

        // More workers, less wall clock — but sublinearly, and never below the
        // point where adding threads stops buying anything.
        let eight = plan.estimated_seconds(0.5, 8);
        assert!(eight < plan.estimated_seconds(0.5, 4));
        assert!(eight > 50.0 / 8.0);
    }
}
