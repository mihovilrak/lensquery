//! CLI entry point: argument parsing, and nothing else that other modules
//! could need.
//!
//! Every flag default is sourced from [`crate::config`], loaded once per
//! invocation, and resolved CLI flag > config file > built-in default.
//!
//! **The output contract is that `lq` composes with other tools.** Results go
//! to stdout and only results do — paths one per line for `search`,
//! `key: value` for `status` and `doctor`. Progress bars, warnings, and errors
//! go to stderr, so a pipe carries the answer and nothing else.
//!
//! Exit codes: 0 success; 1 a runtime user error (`search --open` with no
//! results); 2 an environment or usage error (bad argument value, malformed
//! config, schema-version mismatch, missing libtesseract).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::config;
use crate::db;
use crate::fuzzy;
use crate::indexer;
use crate::lang;
use crate::tess;
use crate::watch;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Exit-code constants (see module header).
const EXIT_USER: u8 = 1;
const EXIT_ENV: u8 = 2;

#[derive(Parser)]
#[command(name = "lq", version = VERSION, about = "LensQuery — local offline image text search")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Never touch the network. Only `lq lang add` would; this makes it say so
    /// instead. `LENSQUERY_OFFLINE=1` does the same for a whole shell.
    #[arg(long, global = true)]
    offline: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Index images in DIRECTORY (recursive).
    Index(IndexArgs),
    /// Watch DIRECTORY and index changes as they happen.
    Watch(WatchArgs),
    /// Search indexed images.
    Search(SearchArgs),
    /// Answer queries from stdin in one warm process.
    ///
    /// `lq search` pays a fresh process spawn per query (36.3 ms measured, and
    /// none of it is search); this pays it once and then answers on an already
    /// open connection. Protocol is deliberately line-oriented so it composes
    /// with a shell: see `cmd_serve`.
    Serve(ServeArgs),
    /// Reclaim database file space (`PRAGMA optimize` + `VACUUM`).
    Compact {
        #[arg(long = "db")]
        db_path: Option<PathBuf>,
    },
    /// Show index statistics.
    Status {
        #[arg(long = "db")]
        db_path: Option<PathBuf>,
        /// One JSON object on stdout instead of `key: value` lines.
        #[arg(long)]
        json: bool,
    },
    /// List, add, or remove Tesseract language packs.
    Lang(LangArgs),
    /// Print environment diagnostics.
    Doctor {
        #[arg(long = "db")]
        db_path: Option<PathBuf>,
        /// One JSON object on stdout instead of `key: value` lines.
        #[arg(long)]
        json: bool,
    },
}

/// `lq lang` — everything about which languages LensQuery can read.
#[derive(Args)]
struct LangArgs {
    #[command(subcommand)]
    command: Option<LangCommand>,
}

#[derive(Subcommand)]
enum LangCommand {
    /// Language packs installed and visible to Tesseract right now.
    List,
    /// Every pack that can be downloaded, and whether it is installed.
    Available {
        /// Show only codes or names containing this text.
        filter: Option<String>,
    },
    /// Download and install packs, verifying each against the pinned manifest.
    Add {
        /// Language codes, e.g. `hrv deu chi_sim`.
        #[arg(required = true)]
        codes: Vec<String>,
    },
    /// Delete installed packs.
    Remove {
        #[arg(required = true)]
        codes: Vec<String>,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Where packs are read from and where the next one would be written.
    Path,
}

/// `lq search` flags, grouped for the same reason as [`IndexArgs`]: seven of
/// the eight are `Option`/`bool` and transposing two of them would compile.
#[derive(Args)]
struct SearchArgs {
    /// Query string.
    query: String,
    #[arg(long = "db")]
    db_path: Option<PathBuf>,
    #[arg(long)]
    limit: Option<u32>,
    /// Open the top match with the OS default handler.
    #[arg(long = "open")]
    open: bool,
    /// Tolerate typos and OCR misreads: `invoce` still finds `invoice`.
    #[arg(long)]
    fuzzy: bool,
    /// Edits a word may be off by under `--fuzzy`. Default scales with length.
    #[arg(long = "fuzzy-distance", value_parser = clap::value_parser!(u32).range(0..=fuzzy::MAX_DISTANCE as i64))]
    fuzzy_distance: Option<u32>,
    /// Match anywhere inside a word: `nvoi` finds `invoice`.
    #[arg(long, conflicts_with_all = ["fuzzy", "fuzzy_distance"])]
    substring: bool,
    /// Append the matched excerpt to each result line. On by default; kept so
    /// a script can ask for the default explicitly.
    #[arg(long)]
    snippet: bool,
    /// Paths only, no excerpt.
    #[arg(long = "no-snippet", conflicts_with = "snippet")]
    no_snippet: bool,
    /// One JSON object per result on stdout (JSON Lines).
    #[arg(long)]
    json: bool,
    /// Print scores alongside paths.
    #[arg(short, long)]
    verbose: bool,
}

/// `lq serve` flags — the session's starting state. Everything here except the
/// database is changeable mid-session with a `:` directive.
#[derive(Args)]
struct ServeArgs {
    #[arg(long = "db")]
    db_path: Option<PathBuf>,
    /// Starting result limit; changeable per session with `:limit N`.
    #[arg(long)]
    limit: Option<u32>,
    /// Start typo-tolerant; toggle with `:fuzzy on|off`.
    #[arg(long)]
    fuzzy: bool,
    /// Edits a word may be off by under `--fuzzy`. Default scales with length.
    #[arg(long = "fuzzy-distance", value_parser = clap::value_parser!(u32).range(0..=fuzzy::MAX_DISTANCE as i64))]
    fuzzy_distance: Option<u32>,
    /// Start in substring mode; toggle with `:substring on|off`.
    #[arg(long, conflicts_with_all = ["fuzzy", "fuzzy_distance"])]
    substring: bool,
    /// Start with excerpts on; toggle with `:snippet on|off`.
    #[arg(long)]
    snippet: bool,
    /// Print scores alongside paths.
    #[arg(short, long)]
    verbose: bool,
}

/// `lq index` flags, grouped so `cmd_index` takes one argument instead of ten
/// (four of which were adjacent `bool`/`Option<String>` — transposable without
/// a compile error).
#[derive(Args)]
struct IndexArgs {
    /// Directory of images to index (recursive).
    directory: PathBuf,
    /// Database file location.
    #[arg(long = "db")]
    db_path: Option<PathBuf>,
    /// Re-OCR every file even if mtime is unchanged.
    #[arg(long = "full-reindex")]
    full_reindex: bool,
    /// Report what this run would do — image counts and an estimated time —
    /// and exit without OCRing or writing anything.
    ///
    /// Times a few of your own images to make the estimate, so it costs a
    /// couple of seconds and needs Tesseract like a real run does.
    #[arg(long = "dry-run")]
    dry_run: bool,
    /// Tesseract language string.
    #[arg(long)]
    lang: Option<String>,
    /// Worker threads; 0 = auto (cpu count).
    #[arg(long)]
    workers: Option<u32>,
    /// OCR engine: "auto" or "dll" (the Rust core is DLL-only).
    #[arg(long)]
    engine: Option<String>,
    /// Word-confidence floor (0-100); words below it are dropped.
    #[arg(long = "min-conf")]
    min_conf: Option<f32>,
    /// Two-arm OCR: re-read near-empty pages as a uniform text block.
    ///
    /// Off by default because it roughly doubles OCR cost on the images it
    /// triggers on, and OCR is ~90% of indexing. Turn it on for archival
    /// indexing — a one-off pass over a corpus you will search for years,
    /// where a slower index is paid once and a missed page is paid every
    /// search. Leave it off for repeated incremental runs.
    #[arg(long)]
    thorough: bool,
    /// Disable two-arm OCR even when the config file enables it.
    /// Wins over `--thorough` if both are given.
    #[arg(long = "no-thorough")]
    no_thorough: bool,
    /// Word count at or below which `--thorough` re-reads a page (default 3).
    ///
    /// Inert without `--thorough`. Raise it to catch pages the primary pass
    /// read only a caption from; `0` restricts the second arm to pages it
    /// found nothing in at all. 3 was measured against 1 and 5 and held — 5
    /// gained nothing for twice the extra cost — but that was one corpus, and
    /// this flag exists so a denser one can disagree.
    #[arg(long = "thorough-trigger-words")]
    thorough_trigger_words: Option<u32>,
    /// Write every failed image (path + reason) to this file — truncated at
    /// start, so it describes this run — turning a bare failure count into a
    /// worklist.
    #[arg(long = "failed-log")]
    failed_log: Option<PathBuf>,
    /// Verbose logging to stderr.
    #[arg(short, long)]
    verbose: bool,
}

/// `lq watch` flags — `index`'s, minus the two a long-running process cannot
/// mean: no `--full-reindex` (a watcher indexes what changed) and no
/// `--failed-log` (nothing marks the end of a run to write one at).
#[derive(Args)]
struct WatchArgs {
    /// Directory to watch (recursive).
    directory: PathBuf,
    /// Database file location. Must be outside DIRECTORY.
    #[arg(long = "db")]
    db_path: Option<PathBuf>,
    /// Tesseract language string.
    #[arg(long)]
    lang: Option<String>,
    /// Worker threads; 0 = auto (half the cpu count).
    #[arg(long)]
    workers: Option<u32>,
    /// OCR engine: "auto" or "dll" (the Rust core is DLL-only).
    #[arg(long)]
    engine: Option<String>,
    /// Word-confidence floor (0-100); words below it are dropped.
    #[arg(long = "min-conf")]
    min_conf: Option<f32>,
    /// Seconds of quiet before a batch of changes is indexed.
    ///
    /// A camera import or an unzip writes files for as long as it takes, each
    /// one an event, and OCR'ing a file still being written wastes the work
    /// twice. Waiting for the directory to fall silent turns the burst into
    /// one batch. Shorten it for hand-dropped single files; lengthen it for a
    /// source that copies in slow bursts.
    #[arg(long, default_value = "3.0")]
    debounce: f64,
    /// Two-arm OCR: re-read near-empty pages as a uniform text block.
    #[arg(long)]
    thorough: bool,
    /// Disable two-arm OCR even when the config file enables it.
    #[arg(long = "no-thorough")]
    no_thorough: bool,
    /// Word count at or below which `--thorough` re-reads a page (default 3).
    #[arg(long = "thorough-trigger-words")]
    thorough_trigger_words: Option<u32>,
    /// Verbose logging to stderr.
    #[arg(short, long)]
    verbose: bool,
}

pub fn main() -> ExitCode {
    // Before any thread exists — see `tess::init_process_env`.
    tess::init_process_env();
    let cli = Cli::parse();
    match run(cli.command, cli.offline) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(EXIT_ENV)
        }
    }
}

/// Dispatch a parsed command. `Err` is an environment/config failure the caller
/// maps to exit 2; per-command user errors return their own `ExitCode`.
fn run(command: Command, offline: bool) -> anyhow::Result<ExitCode> {
    let cfg = config::load(None)?;
    match command {
        Command::Index(args) => cmd_index(&cfg, args),
        Command::Watch(args) => cmd_watch(&cfg, args),
        Command::Search(args) => cmd_search(&cfg, args),
        Command::Serve(args) => cmd_serve(&cfg, args),
        Command::Compact { db_path } => cmd_compact(&cfg, db_path),
        Command::Status { db_path, json } => cmd_status(&cfg, db_path, json),
        Command::Lang(args) => cmd_lang(args, offline),
        Command::Doctor { db_path, json } => cmd_doctor(&cfg, db_path, json),
    }
}

/// Everything an OCR run needs to be true before it starts, checked once.
///
/// `Some(code)` means stop with that exit code and a message already on
/// stderr. Shared by `index` and `watch` so the two cannot drift into
/// disagreeing about what a runnable environment is.
fn preflight(
    engine: &str,
    min_conf: Option<f32>,
    lang: &str,
    explicit_lang: bool,
) -> Option<ExitCode> {
    // `cli` — shelling out to the `tesseract` executable per image — parses but
    // has no implementation here. Accepting it and quietly doing library work
    // instead would misreport what ran, so it is rejected by name.
    match engine {
        "auto" | "dll" => {}
        "cli" => {
            eprintln!(
                "error: --engine cli (invoking the tesseract executable) is not \
                 implemented; use \"auto\" or \"dll\"."
            );
            return Some(ExitCode::from(EXIT_ENV));
        }
        other => {
            eprintln!("error: --engine must be \"auto\" or \"dll\", got {other:?}");
            return Some(ExitCode::from(EXIT_ENV));
        }
    }

    if let Some(c) = min_conf {
        if !(0.0..=100.0).contains(&c) {
            eprintln!("error: --min-conf must be between 0 and 100, got {c}");
            return Some(ExitCode::from(EXIT_ENV));
        }
    }

    // Without a Tesseract library every image would come back `failed`, and a
    // run over 20,000 images would take an hour to say so. Check once, up
    // front, and fail as the environment error it is.
    if !tess::available() {
        eprintln!(
            "error: no Tesseract library found. Install Tesseract 5, or set {} \
             to its path. Run `lq doctor` to see every path that was tried.",
            tess::LIB_ENV_VAR
        );
        return Some(ExitCode::from(EXIT_ENV));
    }

    if let Err(e) = lang::validate(lang) {
        eprintln!("error: {e}");
        return Some(ExitCode::from(EXIT_ENV));
    }
    if let Some(code) = first_missing_pack(lang) {
        eprintln!(
            "error: language pack `{code}` is not installed. Run `lq lang add {code}`, \
             or `lq lang list` to see what is."
        );
        return Some(ExitCode::from(EXIT_ENV));
    }
    if !explicit_lang {
        suggest_pack_for_locale();
    }
    None
}

fn cmd_index(cfg: &config::Config, args: IndexArgs) -> anyhow::Result<ExitCode> {
    let directory = args.directory;
    if !directory.is_dir() {
        eprintln!("error: {} is not a directory", directory.display());
        return Ok(ExitCode::from(EXIT_ENV));
    }

    let effective_engine = args.engine.as_deref().unwrap_or(&cfg.engine);
    let effective_lang = args.lang.clone().unwrap_or_else(|| cfg.languages.clone());
    if let Some(code) = preflight(
        effective_engine,
        args.min_conf,
        &effective_lang,
        args.lang.is_some(),
    ) {
        return Ok(code);
    }

    let effective_db = args.db_path.unwrap_or_else(|| cfg.default_db.clone());
    let effective_workers = args.workers.unwrap_or(cfg.workers) as usize;
    let effective_min_conf = args.min_conf.unwrap_or(cfg.min_word_conf);
    // Either flag beats the config default, and `--no-thorough` beats
    // `--thorough`; without it a config `thorough = true` could not be turned
    // off from the command line at all.
    let effective_thorough = if args.no_thorough {
        false
    } else {
        args.thorough || cfg.thorough
    };
    let effective_trigger = args
        .thorough_trigger_words
        .unwrap_or(cfg.thorough_trigger_words) as usize;

    if args.dry_run {
        let opts = indexer::IndexOptions {
            lang: &effective_lang,
            workers: effective_workers,
            min_conf: effective_min_conf,
            thorough: effective_thorough,
            thorough_trigger_words: effective_trigger,
            ..Default::default()
        };
        return dry_run_report(&directory, &effective_db, &opts);
    }

    let bar = new_progress_bar();
    let progress = |ev: indexer::Progress| match ev {
        indexer::Progress::Start(total) => bar.set_length(total),
        indexer::Progress::Advance(n) => bar.inc(n),
    };

    let opts = indexer::IndexOptions {
        full_reindex: args.full_reindex,
        lang: &effective_lang,
        workers: effective_workers,
        min_conf: effective_min_conf,
        thorough: effective_thorough,
        thorough_trigger_words: effective_trigger,
        verbose: args.verbose,
        engine: effective_engine,
        failed_log: args.failed_log.as_deref(),
        progress: Some(&progress),
    };
    let result = indexer::index_directory(&directory, &effective_db, &opts)?;
    bar.finish_and_clear();

    println!(
        "Summary: {} indexed, {} updated, {} skipped, {} failed, {} deleted (in {:.1}s)",
        result.indexed,
        result.updated,
        result.skipped,
        result.failed,
        result.deleted,
        result.duration_seconds,
    );
    Ok(ExitCode::SUCCESS)
}

/// `lq index --dry-run` — the size and shape of the run, before an hour of CPU.
///
/// The counts are exact. The time is not, and is printed as a range for that
/// reason: it comes from timing a few of this corpus's own images (which pins
/// down CPU speed, image size, and language cost) scaled by a parallel-speedup
/// curve fitted to a single measured machine (which does not pin down core
/// topology). The range widens when fewer images could be timed, so a machine
/// slow enough to exhaust the sampling budget early says so by being vaguer
/// rather than by being confidently wrong. See docs/benchmarks.md §1.
fn dry_run_report(
    directory: &Path,
    db_path: &Path,
    opts: &indexer::IndexOptions<'_>,
) -> anyhow::Result<ExitCode> {
    let plan = indexer::dry_run(directory, db_path)?;

    println!("Dry run — nothing was OCR'd and nothing was written.");
    println!(
        "  would OCR     {} images ({} new, {} changed)",
        plan.to_process, plan.new_files, plan.changed
    );
    println!("  would skip    {} unchanged", plan.skipped);
    if plan.stale > 0 {
        println!(
            "  would remove  {} indexed files that are gone from disk",
            plan.stale
        );
    }
    if plan.to_process == 0 {
        println!("\nThe index is already up to date.");
        return Ok(ExitCode::SUCCESS);
    }

    let measured = indexer::sample_seconds_per_image(&plan.sample, opts);
    let per_image = measured.map_or(indexer::REFERENCE_SECONDS_PER_IMAGE, |(rate, _)| rate);
    let workers = indexer::worker_count(opts.workers);
    let seconds = plan.estimated_seconds(per_image, opts.workers);
    let band = indexer::estimate_band(measured.map(|(_, images)| images));
    println!(
        "\n  estimated     {} to {} on {workers} workers",
        human_duration(seconds * (1.0 - band).max(0.0)),
        human_duration(seconds * (1.0 + band)),
    );
    match measured {
        Some((rate, images)) => {
            println!("                {rate:.2} s/image, timed on {images} of your own images")
        }
        None => println!(
            "                {per_image:.2} s/image — the published rate; no image \
             in the sample could be read here"
        ),
    }
    Ok(ExitCode::SUCCESS)
}

/// A duration a human can act on: seconds below a minute and a half, whole
/// minutes below an hour and a half, hours and minutes above.
fn human_duration(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{}m", (s + 30) / 60),
        _ => {
            let (h, m) = (s / 3600, (s % 3600 + 30) / 60);
            match m {
                0 => format!("{h}h"),
                60 => format!("{}h", h + 1),
                _ => format!("{h}h {m}m"),
            }
        }
    }
}

/// `lq watch` — index changes as they arrive, until the process is killed.
///
/// Nothing goes to stdout: the output contract gives stdout to results, and a
/// watcher produces none. `lq watch ~/shots 2>>watch.log` is therefore the
/// whole of "run it and keep a log".
fn cmd_watch(cfg: &config::Config, args: WatchArgs) -> anyhow::Result<ExitCode> {
    let directory = args.directory;
    if !directory.is_dir() {
        eprintln!("error: {} is not a directory", directory.display());
        return Ok(ExitCode::from(EXIT_ENV));
    }
    if !(0.1..=300.0).contains(&args.debounce) {
        eprintln!(
            "error: --debounce must be between 0.1 and 300 seconds, got {}",
            args.debounce
        );
        return Ok(ExitCode::from(EXIT_ENV));
    }

    let effective_engine = args.engine.as_deref().unwrap_or(&cfg.engine);
    let effective_lang = args.lang.clone().unwrap_or_else(|| cfg.languages.clone());
    if let Some(code) = preflight(
        effective_engine,
        args.min_conf,
        &effective_lang,
        args.lang.is_some(),
    ) {
        return Ok(code);
    }

    let effective_db = args.db_path.unwrap_or_else(|| cfg.default_db.clone());
    // Auto means *half* the cores here, not all of them: a watcher runs while
    // its user is still working in the directory being watched.
    let effective_workers = match args.workers.unwrap_or(cfg.workers) as usize {
        0 => watch::default_workers(),
        n => n,
    };
    let effective_min_conf = args.min_conf.unwrap_or(cfg.min_word_conf);
    let effective_thorough = if args.no_thorough {
        false
    } else {
        args.thorough || cfg.thorough
    };
    let effective_trigger = args
        .thorough_trigger_words
        .unwrap_or(cfg.thorough_trigger_words) as usize;

    let opts = indexer::IndexOptions {
        full_reindex: false,
        lang: &effective_lang,
        workers: effective_workers,
        min_conf: effective_min_conf,
        thorough: effective_thorough,
        thorough_trigger_words: effective_trigger,
        verbose: args.verbose,
        engine: effective_engine,
        failed_log: None,
        progress: None,
    };

    let debounce = args.debounce;
    let report = |ev: watch::Event| match ev {
        watch::Event::Ready(root) => eprintln!(
            "watching {} ({} workers, {debounce:.1}s debounce) — Ctrl-C to stop",
            root.display(),
            effective_workers,
        ),
        watch::Event::Renamed { from, to } => eprintln!("moved: {from} -> {to}"),
        watch::Event::Deleted(n) => eprintln!("dropped {n} removed file(s) from the index"),
        watch::Event::Indexed(s) => eprintln!(
            "{} indexed, {} updated, {} skipped, {} failed (in {:.1}s)",
            s.indexed, s.updated, s.skipped, s.failed, s.duration_seconds
        ),
        watch::Event::Warning(msg) => eprintln!("warning: {msg}"),
    };

    watch::run(
        &directory,
        &effective_db,
        Duration::from_secs_f64(debounce),
        &opts,
        &report,
    )?;
    Ok(ExitCode::SUCCESS)
}

/// The first code in `spec` that Tesseract would not find.
///
/// Only meaningful once we can see a populated tessdata directory; with none
/// resolved there is nothing to compare against, and guessing "missing" would
/// block a run that works. `available_langs` is the same listing Tesseract
/// itself would produce.
fn first_missing_pack(spec: &str) -> Option<String> {
    let installed = tess::available_langs();
    if installed.is_empty() {
        return None;
    }
    lang::parts(spec)
        .find(|c| !installed.iter().any(|i| i == c))
        .map(str::to_string)
}

/// Say what the machine's locale implies, once, and add nothing.
///
/// A language nobody asked for costs model-load time on every image, so this
/// stays a suggestion no matter how confident the locale looks.
fn suggest_pack_for_locale() {
    if config::default_path().exists() {
        return;
    }
    if let Some((locale, pack)) = lang::suggest_from_locale() {
        eprintln!(
            "note: system locale is {locale} but {} is not installed. \
             `lq lang add {}` to index {} text too.",
            pack.code, pack.code, pack.name
        );
    }
}

fn cmd_lang(args: LangArgs, offline: bool) -> anyhow::Result<ExitCode> {
    match args.command.unwrap_or(LangCommand::List) {
        LangCommand::List => lang_list(),
        LangCommand::Available { filter } => lang_available(filter.as_deref()),
        LangCommand::Add { codes } => lang_add(&codes, offline),
        LangCommand::Remove { codes, yes } => lang_remove(&codes, yes),
        LangCommand::Path => lang_path(),
    }
}

fn lang_list() -> anyhow::Result<ExitCode> {
    let Some(dir) = tess::tessdata_path() else {
        eprintln!(
            "error: no tessdata directory found. Install Tesseract, or set \
             TESSDATA_PREFIX. `lq doctor` shows every path that was tried."
        );
        return Ok(ExitCode::from(EXIT_ENV));
    };
    println!("tessdata_dir: {}", dir.display());
    let mut codes: Vec<String> = lang::codes_in(&dir).collect();
    codes.sort();
    if codes.is_empty() {
        println!("(no language packs installed)");
    }
    for code in &codes {
        let name = lang::find(code).map_or("(not in the manifest)", |p| p.name.as_str());
        let size = std::fs::metadata(dir.join(format!("{code}.traineddata")))
            .map(|m| human_bytes(m.len()))
            .unwrap_or_else(|_| "?".to_string());
        println!("{code}\t{size}\t{name}");
    }
    // Silence here would be the confusing case: packs sitting beside the
    // library that Tesseract can no longer see, because our directory won.
    for shadowed in shadowed_dirs(&dir) {
        let mut hidden: Vec<String> = lang::codes_in(&shadowed).collect();
        hidden.retain(|c| !codes.contains(c));
        if hidden.is_empty() {
            continue;
        }
        hidden.sort();
        eprintln!(
            "note: {} also holds {}, which Tesseract cannot see while it is \
             reading {}. `lq lang add <code>` installs those here too.",
            shadowed.display(),
            hidden.join(", "),
            dir.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Directories holding packs that the active directory is hiding.
fn shadowed_dirs(active: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(beside) = tess::library_path()
        .and_then(|p| p.parent())
        .map(|p| p.join("tessdata"))
    {
        if beside != active && beside.is_dir() {
            out.push(beside);
        }
    }
    out
}

fn lang_available(filter: Option<&str>) -> anyhow::Result<ExitCode> {
    let m = lang::manifest();
    let installed = tess::available_langs();
    println!("source: {} @ {}", m.source, m.tag);
    let needle = filter.map(str::to_lowercase);
    for pack in &m.packs {
        if let Some(n) = &needle {
            if !pack.code.to_lowercase().contains(n) && !pack.name.to_lowercase().contains(n) {
                continue;
            }
        }
        let status = if installed.contains(&pack.code) {
            "installed"
        } else {
            "available"
        };
        let kind = match pack.kind {
            lang::Kind::Language => "",
            lang::Kind::Detector => "\tdetector — not for --lang",
        };
        println!(
            "{}\t{}\t{}\t{status}{kind}",
            pack.code,
            pack.name,
            human_bytes(pack.size)
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn lang_add(codes: &[String], offline: bool) -> anyhow::Result<ExitCode> {
    for code in codes {
        if lang::find(code).is_none() {
            eprintln!(
                "error: unknown language pack `{code}`. `lq lang available` lists \
                 every code, and `lq lang available {}` searches it.",
                code.chars().take(3).collect::<String>()
            );
            return Ok(ExitCode::from(EXIT_ENV));
        }
    }
    let dir = match lang::install_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(ExitCode::from(EXIT_ENV));
        }
    };

    // Visible to Tesseract now is enough: anything in the directory it reads
    // today is carried into ours by `adopt` below, so it never needs fetching.
    let installed = tess::available_langs();
    let mut wanted = Vec::new();
    for code in codes {
        if installed.iter().any(|i| i == code) {
            println!("{code}: already installed");
        } else {
            wanted.push(code);
        }
    }
    if wanted.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    // Installing shadows whatever Tesseract reads today, so find out what that
    // would cost before writing anything, and carry those packs across after
    // the first pack actually lands.
    let shadowed = lang::shadowed_by_install(&dir);
    let shadowed_from = tess::tessdata_path();
    let mut adopted = false;

    let offline = lang::is_offline(offline);
    for code in wanted {
        let pack = lang::find(code).expect("checked above");
        let bar = new_bytes_bar(pack.size);
        let mut progress = |done: u64, _total: u64| bar.set_position(done);
        let result = lang::fetch(pack, &dir, offline, &mut progress);
        bar.finish_and_clear();
        match result {
            Ok(path) => {
                println!("{code}: installed {} ({})", path.display(), pack.name);
                if !adopted && !shadowed.is_empty() {
                    adopted = true;
                    let from = shadowed_from.as_deref().expect("shadowed implies a source");
                    if let Err(e) = lang::adopt(from, &dir, &shadowed) {
                        eprintln!("error: {e}");
                        return Ok(ExitCode::from(EXIT_ENV));
                    }
                    eprintln!(
                        "note: {} is not writable, so packs live in {} from now on.",
                        from.display(),
                        dir.display()
                    );
                    eprintln!(
                        "note: copied {} there too, so they stay readable.",
                        shadowed.join(", ")
                    );
                }
                if pack.kind == lang::Kind::Detector {
                    eprintln!(
                        "note: {code} detects orientation and script. It is not a \
                         language — leave it out of --lang."
                    );
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                return Ok(ExitCode::from(EXIT_ENV));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn lang_remove(codes: &[String], yes: bool) -> anyhow::Result<ExitCode> {
    let dir = match lang::install_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(ExitCode::from(EXIT_ENV));
        }
    };
    // Only worth asking when someone is there to answer; a pipeline that typed
    // the codes out already meant them.
    if !yes && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprint!("Remove {} from {}? [y/N] ", codes.join(", "), dir.display());
        std::io::Write::flush(&mut std::io::stderr())?;
        let mut answer = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            eprintln!("nothing removed");
            return Ok(ExitCode::SUCCESS);
        }
    }
    let mut code_exit = ExitCode::SUCCESS;
    for code in codes {
        let path = dir.join(format!("{code}.traineddata"));
        if path.exists() {
            std::fs::remove_file(&path)?;
            println!("{code}: removed {}", path.display());
            continue;
        }
        // Refusing to delete out of a directory we do not install into keeps
        // `lq lang remove` from quietly undoing a system package manager.
        match tess::available_langs().iter().any(|i| i == code) {
            true => eprintln!(
                "error: {code} is installed outside {} — it came from the system \
                 Tesseract, so remove it the same way it was installed.",
                dir.display()
            ),
            false => eprintln!("error: {code} is not installed"),
        }
        code_exit = ExitCode::from(EXIT_ENV);
    }
    Ok(code_exit)
}

fn lang_path() -> anyhow::Result<ExitCode> {
    match tess::tessdata_path() {
        Some(p) => println!("tessdata_dir: {}", p.display()),
        None => println!("tessdata_dir: <none>"),
    }
    match lang::install_dir() {
        Ok(p) => println!("install_dir: {}", p.display()),
        Err(e) => println!("install_dir: <none> ({e})"),
    }
    match lang::user_dir() {
        Some(p) => println!(
            "user_dir: {} ({})",
            p.display(),
            if lang::user_dir_is_active() {
                "in use"
            } else {
                "empty"
            }
        ),
        None => println!("user_dir: <none>"),
    }
    println!(
        "manifest: {} @ {}",
        lang::manifest().source,
        lang::manifest().tag
    );
    Ok(ExitCode::SUCCESS)
}

fn human_bytes(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / (1 << 20) as f64)
    } else {
        format!("{:.0} kB", n as f64 / 1024.0)
    }
}

fn new_bytes_bar(total: u64) -> indicatif::ProgressBar {
    let bar = indicatif::ProgressBar::new(total);
    if let Ok(style) =
        indicatif::ProgressStyle::with_template("{bar:40} {bytes}/{total_bytes} ({eta})")
    {
        bar.set_style(style);
    }
    bar
}

fn cmd_search(cfg: &config::Config, args: SearchArgs) -> anyhow::Result<ExitCode> {
    let query = args.query;
    let effective_db = args.db_path.unwrap_or_else(|| cfg.default_db.clone());
    let effective_limit = args.limit.unwrap_or(cfg.default_limit) as i64;
    // On unless refused: the excerpt is what makes a hit readable without
    // opening the file, and `--json` now covers the scripts that want neither.
    let snippet = !args.no_snippet;

    let matcher = Matcher::from_flags(args.fuzzy, args.fuzzy_distance, args.substring);

    let conn = db::connect(&effective_db)?;
    let results = run_query(&conn, &query, effective_limit, matcher, snippet)?;
    drop(conn);

    if results.is_empty() {
        if args.open {
            eprintln!("No results found for \"{query}\". Cannot open.");
            return Ok(ExitCode::from(EXIT_USER));
        }
        let msg = match matcher {
            Matcher::Standard => "No results found. Try --fuzzy for approximate matching.",
            Matcher::Fuzzy(_) => "No results found. Try --substring to match inside words.",
            Matcher::Substring => "No results found.",
        };
        // Nothing but JSON on stdout, so an empty result set is an empty stream
        // and `jq` sees a valid — if zero-length — document either way.
        if args.json {
            eprintln!("{msg}");
        } else {
            println!("{msg}");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for r in &results {
        if args.json {
            let _ = writeln!(out, "{}", serde_json::to_string(&JsonHit::from(r))?);
        } else {
            let _ = writeln!(out, "{}", result_line(r, args.verbose));
        }
    }
    drop(out);

    if args.open {
        open_path(Path::new(&results[0].path));
    }
    Ok(ExitCode::SUCCESS)
}

/// How a query is matched against the index.
///
/// The three are mutually exclusive rather than composable: they ask different
/// questions of different tables, and a request for two of them at once has no
/// single answer. Carrying that as one enum instead of a pair of booleans is
/// what keeps `--fuzzy --substring` from being a state the code has to invent
/// a meaning for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Matcher {
    /// Porter-stemmed whole words. The default, and the only one that uses the
    /// index the way FTS5 is fastest at.
    Standard,
    /// Typo- and misread-tolerant. `Some(n)` overrides the length-scaled edit
    /// budget; `None` takes it.
    Fuzzy(Option<u32>),
    /// Trigram substring: matches inside a word, exactly.
    Substring,
}

impl Matcher {
    /// Resolve the flags a command was given. `--substring` and `--fuzzy` are
    /// declared as conflicting, so at most one arm can be taken.
    fn from_flags(fuzzy: bool, fuzzy_distance: Option<u32>, substring: bool) -> Self {
        if substring {
            Matcher::Substring
        } else if fuzzy || fuzzy_distance.is_some() {
            Matcher::Fuzzy(fuzzy_distance)
        } else {
            Matcher::Standard
        }
    }
}

/// Pick the matcher and the snippet variant. Both `search` and `serve` have to
/// make the same choice, so neither reaches into `db` directly.
fn run_query(
    conn: &rusqlite::Connection,
    query: &str,
    limit: i64,
    matcher: Matcher,
    snippet: bool,
) -> Result<Vec<crate::models::SearchResult>, db::DbError> {
    match (matcher, snippet) {
        (Matcher::Standard, false) => db::search_standard(conn, query, limit),
        (Matcher::Standard, true) => db::search_standard_snippet(conn, query, limit),
        (Matcher::Substring, false) => db::search_substring(conn, query, limit),
        (Matcher::Substring, true) => db::search_substring_snippet(conn, query, limit),
        (Matcher::Fuzzy(d), false) => db::search_fuzzy(conn, query, limit, d),
        (Matcher::Fuzzy(d), true) => db::search_fuzzy_snippet(conn, query, limit, d),
    }
}

/// The `--json` wire format.
///
/// Dedicated structs rather than `serde_json::json!`, because that macro's map
/// is a `BTreeMap` and would silently alphabetize the keys; serde emits struct
/// fields in declaration order, so the order below is the order documented in
/// [`docs/json-output.md`](../docs/json-output.md). Adding a field is
/// backward-compatible, renaming or removing one is not — the same discipline
/// [`crate::models`] is under.
#[derive(serde::Serialize)]
struct JsonHit<'a> {
    path: &'a str,
    score: f64,
    mtime: f64,
    lang: Option<&'a str>,
    /// Absent, not null, when the excerpt was not requested — the one key that
    /// varies with the flags.
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<&'a str>,
}

impl<'a> From<&'a crate::models::SearchResult> for JsonHit<'a> {
    fn from(r: &'a crate::models::SearchResult) -> Self {
        Self {
            path: &r.path,
            score: r.score,
            mtime: r.mtime,
            lang: r.lang.as_deref(),
            snippet: r.snippet.as_deref(),
        }
    }
}

#[derive(serde::Serialize)]
struct JsonStats {
    file_count: i64,
    db_bytes: i64,
    last_indexed_at: Option<String>,
    schema_version: Option<i64>,
}

impl From<&db::DbStats> for JsonStats {
    fn from(s: &db::DbStats) -> Self {
        Self {
            file_count: s.file_count,
            db_bytes: s.db_bytes,
            last_indexed_at: s.last_indexed_at.clone(),
            schema_version: s.schema_version,
        }
    }
}

#[derive(serde::Serialize)]
struct JsonStatus {
    db_path: String,
    /// Flattened, so `--json` has the same shape as the `key: value` form
    /// rather than nesting what that form prints side by side.
    #[serde(flatten)]
    stats: JsonStats,
}

#[derive(serde::Serialize)]
struct JsonAttempt {
    path: String,
    outcome: String,
}

#[derive(serde::Serialize)]
struct JsonDoctor {
    lq_version: String,
    platform: String,
    libtesseract: Option<String>,
    libtesseract_search: Vec<JsonAttempt>,
    hints: Vec<String>,
    tessdata_path: Option<String>,
    tessdata_install_dir: Option<String>,
    tesseract_langs: Vec<String>,
    engine_default: String,
    languages_default: String,
    min_word_conf_default: f32,
    thorough_default: bool,
    thorough_trigger_words_default: u32,
    db_path: String,
    db_status: &'static str,
    stats: Option<JsonStats>,
}

/// One result as its output line: `[score\t]path[\tsnippet]`.
///
/// Fixed field order so a caller can split on `\t` knowing only which flags it
/// passed. The snippet is whitespace-collapsed in [`db`], so it can never
/// introduce a second line and break the one-result-per-line contract — which
/// in `serve` would also mean a spurious block terminator.
fn result_line(r: &crate::models::SearchResult, verbose: bool) -> String {
    let mut line = if verbose {
        format!("{:.4}\t{}", r.score, r.path)
    } else {
        r.path.clone()
    };
    if let Some(s) = &r.snippet {
        line.push('\t');
        line.push_str(s);
    }
    line
}

/// Answer queries from stdin until EOF or `:quit`.
///
/// This exists for one reason: a `lq search` invocation spends 36.3 ms getting
/// to the point where it can run a query that itself takes single-digit
/// milliseconds. That floor is process spawn plus dynamic linking, so no amount
/// of work inside `search` can move it — the only way past it is to not spawn
/// again. A tool driving LensQuery in a loop (a picker, an editor plugin, a
/// batch script) pays the floor once here instead of once per query.
///
/// Protocol — one response block per non-blank input line, always terminated by
/// a blank line, so a client can read until blank without counting:
///
/// * blank line — ignored, no response block
/// * `:quit` — exit 0
/// * `:limit N` — set the result limit for subsequent queries
/// * `:fuzzy on` / `:fuzzy off` — typo-tolerant matching
/// * `:substring on` / `:substring off` — match inside words
/// * `:snippet on` / `:snippet off` — append the matched excerpt to each line
/// * anything else — a query; matching paths, one per line
///
/// Paths are never empty, so a blank line is unambiguously a terminator. A bad
/// directive or a failed query reports to stderr and still emits its (empty)
/// block, so a client never blocks waiting for a response that isn't coming.
/// Consequence of the `:` prefix: a query cannot begin with a colon — FTS5 has
/// no leading-colon syntax, so nothing searchable is lost.
fn cmd_serve(cfg: &config::Config, args: ServeArgs) -> anyhow::Result<ExitCode> {
    let effective_db = args.db_path.unwrap_or_else(|| cfg.default_db.clone());
    // Opened once, held for the whole session — this is the entire point.
    let conn = db::connect(&effective_db)?;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    serve_loop(
        &conn,
        stdin.lock(),
        &mut out,
        ServeState {
            limit: args.limit.unwrap_or(cfg.default_limit) as i64,
            matcher: Matcher::from_flags(args.fuzzy, args.fuzzy_distance, args.substring),
            fuzzy_distance: args.fuzzy_distance,
            snippet: args.snippet,
            verbose: args.verbose,
        },
    )
}

/// What a `serve` session carries between lines. `verbose` and
/// `fuzzy_distance` are start-only (there is no directive for either); the
/// rest is what the directives move. `fuzzy_distance` is kept even while the
/// matcher is something else, so `:fuzzy on` restores the budget the session
/// was started with rather than silently dropping back to the default.
#[derive(Clone, Copy)]
struct ServeState {
    limit: i64,
    matcher: Matcher,
    fuzzy_distance: Option<u32>,
    snippet: bool,
    verbose: bool,
}

/// The `serve` protocol itself, over any reader/writer.
///
/// Split out from [`cmd_serve`] so the loop can be driven from a test without
/// spawning a process and without owning the real stdin/stdout. The contract
/// this enforces, and what the tests pin: a `:`-prefixed line is a directive,
/// anything else is a query, and **every** input line — directive, query, hit
/// or miss — is answered with a block terminated by one blank line, flushed
/// before the next read.
fn serve_loop<R: std::io::BufRead, W: std::io::Write>(
    conn: &rusqlite::Connection,
    input: R,
    out: &mut W,
    mut state: ServeState,
) -> anyhow::Result<ExitCode> {
    for line in input.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(directive) = line.strip_prefix(':') {
            let (verb, arg) = match directive.split_once(char::is_whitespace) {
                Some((v, a)) => (v, a.trim()),
                None => (directive, ""),
            };
            match verb {
                "quit" => return Ok(ExitCode::SUCCESS),
                "limit" => match arg.parse::<i64>() {
                    Ok(n) if n > 0 => state.limit = n,
                    _ => eprintln!("error: :limit takes a positive integer, got {arg:?}"),
                },
                "fuzzy" => match parse_toggle(arg) {
                    Some(true) => state.matcher = Matcher::Fuzzy(state.fuzzy_distance),
                    Some(false) => state.matcher = Matcher::Standard,
                    None => eprintln!("error: :fuzzy takes \"on\" or \"off\", got {arg:?}"),
                },
                "substring" => match parse_toggle(arg) {
                    Some(true) => state.matcher = Matcher::Substring,
                    Some(false) => state.matcher = Matcher::Standard,
                    None => eprintln!("error: :substring takes \"on\" or \"off\", got {arg:?}"),
                },
                "snippet" => match parse_toggle(arg) {
                    Some(on) => state.snippet = on,
                    None => eprintln!("error: :snippet takes \"on\" or \"off\", got {arg:?}"),
                },
                other => eprintln!("error: unknown directive :{other}"),
            }
            writeln!(out)?;
            out.flush()?;
            continue;
        }

        let results = run_query(conn, line, state.limit, state.matcher, state.snippet);
        match results {
            Ok(results) => {
                for r in &results {
                    writeln!(out, "{}", result_line(r, state.verbose))?;
                }
            }
            // A malformed query is a per-line event, not a reason to tear down
            // a warm process the client is still talking to.
            Err(e) => eprintln!("error: {e}"),
        }
        writeln!(out)?;
        // Without this the client deadlocks: it waits for a block that is
        // sitting in our buffer, and we wait for its next line.
        out.flush()?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `"on"` / `"off"`, the argument shape shared by every toggle directive.
fn parse_toggle(arg: &str) -> Option<bool> {
    match arg {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// Reclaim file space. See [`db::compact`] for why this is a space verb and not
/// a speed one, and why it is not attached to the tail of `lq index`.
fn cmd_compact(cfg: &config::Config, db_path: Option<PathBuf>) -> anyhow::Result<ExitCode> {
    let effective_db = db_path.unwrap_or_else(|| cfg.default_db.clone());
    if !effective_db.exists() {
        eprintln!(
            "error: no database at {} — nothing to compact.",
            effective_db.display()
        );
        return Ok(ExitCode::from(EXIT_ENV));
    }
    let conn = db::connect(&effective_db)?;
    let s = db::compact(&conn)?;
    drop(conn);

    println!("db_path: {}", effective_db.display());
    println!("before_bytes: {}", s.before_bytes);
    println!("after_bytes: {}", s.after_bytes);
    println!("reclaimed_bytes: {}", s.reclaimed());
    let pct = if s.before_bytes > 0 {
        s.reclaimed() as f64 / s.before_bytes as f64 * 100.0
    } else {
        0.0
    };
    println!("reclaimed_percent: {pct:.2}");
    Ok(ExitCode::SUCCESS)
}

fn cmd_status(
    cfg: &config::Config,
    db_path: Option<PathBuf>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let effective_db = db_path.unwrap_or_else(|| cfg.default_db.clone());
    let conn = db::connect(&effective_db)?;
    let s = db::stats(&conn)?;
    drop(conn);

    if json {
        let doc = JsonStatus {
            db_path: effective_db.display().to_string(),
            stats: JsonStats::from(&s),
        };
        println!("{}", serde_json::to_string(&doc)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("db_path: {}", effective_db.display());
    print_stats(&s);
    Ok(ExitCode::SUCCESS)
}

fn cmd_doctor(
    cfg: &config::Config,
    db_path: Option<PathBuf>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let facts = doctor(cfg, db_path)?;
    if json {
        println!("{}", serde_json::to_string(&facts)?);
    } else {
        print_doctor(&facts);
    }
    Ok(ExitCode::SUCCESS)
}

/// Collect every diagnostic once, so the two renderers cannot drift apart.
///
/// Gathering and printing were one function until `--json` needed the same
/// facts in a different shape; splitting them is what keeps `lq doctor` and
/// `lq doctor --json` describing the same machine.
fn doctor(cfg: &config::Config, db_path: Option<PathBuf>) -> anyhow::Result<JsonDoctor> {
    let effective_db = db_path.unwrap_or_else(|| cfg.default_db.clone());
    let mut hints = Vec::new();
    if !tess::available() {
        hints.push(tess::install_hint().to_string());
        hints.push(format!(
            "set {} to the library file to skip this search",
            tess::LIB_ENV_VAR
        ));
    }
    let stats = if effective_db.exists() {
        let conn = db::connect(&effective_db)?;
        let s = db::stats(&conn)?;
        drop(conn);
        Some(JsonStats::from(&s))
    } else {
        None
    };
    Ok(JsonDoctor {
        lq_version: VERSION.to_string(),
        platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        libtesseract: tess::library_path().map(|p| p.display().to_string()),
        libtesseract_search: tess::attempts()
            .iter()
            .map(|a| JsonAttempt {
                path: a.path.display().to_string(),
                outcome: outcome_text(&a.outcome),
            })
            .collect(),
        hints,
        tessdata_path: tess::tessdata_path().map(|p| p.display().to_string()),
        tessdata_install_dir: lang::install_dir().ok().map(|p| p.display().to_string()),
        tesseract_langs: tess::available_langs(),
        engine_default: cfg.engine.clone(),
        languages_default: cfg.languages.clone(),
        min_word_conf_default: cfg.min_word_conf,
        thorough_default: cfg.thorough,
        thorough_trigger_words_default: cfg.thorough_trigger_words,
        db_path: effective_db.display().to_string(),
        db_status: if stats.is_some() {
            "ok"
        } else {
            "not_initialized"
        },
        stats,
    })
}

/// The `key: value` rendering. Absent values print a bracketed placeholder
/// rather than being omitted, so a caller can parse the block without knowing
/// which keys to expect.
fn print_doctor(d: &JsonDoctor) {
    println!("lq_version: {}", d.lq_version);
    println!("platform: {}", d.platform);
    match &d.libtesseract {
        Some(p) => println!("libtesseract: {p}"),
        None => println!("libtesseract: not found"),
    }
    // Continuation lines are indented so the `key: value` contract of the rest
    // of `doctor` still holds for anything grepping it.
    println!(
        "libtesseract_search: {} candidates",
        d.libtesseract_search.len()
    );
    for a in &d.libtesseract_search {
        println!("  {} — {}", a.path, a.outcome);
    }
    for hint in &d.hints {
        println!("  hint: {hint}");
    }
    match &d.tessdata_path {
        Some(p) => println!("tessdata_path: {p}"),
        None => println!("tessdata_path: <unset>"),
    }
    match &d.tessdata_install_dir {
        Some(p) => println!("tessdata_install_dir: {p}"),
        None => println!("tessdata_install_dir: <none>"),
    }
    println!(
        "tesseract_langs: {}",
        if d.tesseract_langs.is_empty() {
            "<none>".to_string()
        } else {
            d.tesseract_langs.join(",")
        }
    );
    println!("engine_default: {}", d.engine_default);
    println!("languages_default: {}", d.languages_default);
    println!("min_word_conf_default: {}", d.min_word_conf_default);
    println!("thorough_default: {}", d.thorough_default);
    println!(
        "thorough_trigger_words_default: {}",
        d.thorough_trigger_words_default
    );
    println!("db_path: {}", d.db_path);
    match &d.stats {
        Some(s) => print_stats_json(s),
        None => println!("db_status: not_initialized"),
    }
}

/// Why a candidate library was rejected, or that it was accepted.
///
/// "libtesseract: not found" on its own sends people down the wrong path,
/// because the two most common causes do not look like absence — an
/// architecture mismatch and a missing dependency of libtesseract's own both
/// report as a failed load of a file that is definitely there. Showing the
/// loader's own message for each candidate is the difference between "install
/// Tesseract" (which they already did) and the actual problem.
fn outcome_text(outcome: &tess::Outcome) -> String {
    match outcome {
        tess::Outcome::NotFound => "not present".to_string(),
        tess::Outcome::OpenFailed(e) => format!("load failed: {e}"),
        tess::Outcome::MissingSymbols => "loaded, but no Tesseract C API symbols".to_string(),
        tess::Outcome::Loaded => "OK — using this one".to_string(),
    }
}

/// Emit the four `stats()` fields as `key: value`, one per line, in a fixed
/// order. Absent optional values print the literal `None` rather than being
/// omitted, so a caller can parse the block without knowing which keys to
/// expect.
fn print_stats(s: &db::DbStats) {
    print_stats_json(&JsonStats::from(s));
}

fn print_stats_json(s: &JsonStats) {
    println!("file_count: {}", s.file_count);
    println!("db_bytes: {}", s.db_bytes);
    match &s.last_indexed_at {
        Some(v) => println!("last_indexed_at: {v}"),
        None => println!("last_indexed_at: None"),
    }
    match s.schema_version {
        Some(v) => println!("schema_version: {v}"),
        None => println!("schema_version: None"),
    }
}

/// A determinate progress bar rendered to stderr. When stderr is not a terminal
/// indicatif hides it automatically, so stdout stays clean and greppable.
fn new_progress_bar() -> indicatif::ProgressBar {
    let bar = indicatif::ProgressBar::new(0);
    // `{eta}` is a rolling estimate, not the average since start, which matters
    // because per-image OCR cost varies by an order of magnitude within one
    // directory: indicatif's estimator is a double exponential moving average
    // whose samples decay to 10% weight over 15 seconds. Progress arrives one
    // batch (16 images) at a time, which is a fine sampling cadence for it.
    if let Ok(style) = indicatif::ProgressStyle::with_template(
        "{bar:40} {pos}/{len} images ({elapsed_precise}, ~{eta} left)",
    ) {
        bar.set_style(style);
    }
    // Without this the clock only moves when a batch commits — up to a minute
    // of apparently frozen bar on slow images.
    bar.enable_steady_tick(Duration::from_millis(500));
    bar
}

/// Open `path` with the OS default handler. Best-effort: a failure is reported
/// to stderr but does not change the exit code (the search already succeeded).
fn open_path(path: &Path) {
    #[cfg(windows)]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(path)
        .spawn()
        .map(|_| ());
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ());
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ());

    if let Err(e) = result {
        eprintln!("Could not open {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rusqlite::Connection;

    const NOW: &str = "2026-08-16T10:00:00Z";

    #[test]
    fn human_duration_picks_a_unit_a_reader_can_act_on() {
        assert_eq!(human_duration(0.0), "0s");
        assert_eq!(human_duration(45.4), "45s");
        assert_eq!(human_duration(89.0), "89s");
        assert_eq!(human_duration(90.0), "2m");
        assert_eq!(human_duration(600.0), "10m");
        assert_eq!(human_duration(5400.0), "1h 30m");
        // The minutes band runs to 90 minutes, so an hour is still "60m".
        assert_eq!(human_duration(3600.0), "60m");
        // 59.6 minutes must not round to "1h 60m".
        assert_eq!(human_duration(7199.0), "2h");
    }

    /// A warm session's DB, in memory. Three files so `:limit` has something
    /// to cut and the trigram/porter split has something to disagree about.
    fn seeded_conn() -> Connection {
        let conn = db::connect(Path::new(":memory:")).unwrap();
        db::upsert_file(&conn, "/a.png", 1.0, NOW, "invoice total", None).unwrap();
        db::upsert_file(&conn, "/b.png", 2.0, NOW, "invoice draft", None).unwrap();
        db::upsert_file(&conn, "/c.png", 3.0, NOW, "invoice paid", None).unwrap();
        conn
    }

    /// Drive the loop over a canned session and return what a client would
    /// read on stdout. stderr stays real stderr — the protocol lives on stdout.
    fn serve(
        conn: &Connection,
        session: &str,
        limit: i64,
        matcher: Matcher,
        verbose: bool,
    ) -> String {
        serve_with(
            conn,
            session,
            ServeState {
                limit,
                matcher,
                fuzzy_distance: None,
                snippet: false,
                verbose,
            },
        )
    }

    /// [`serve`] for the cases that need a starting state it does not spell.
    fn serve_with(conn: &Connection, session: &str, state: ServeState) -> String {
        let mut out: Vec<u8> = Vec::new();
        serve_loop(
            conn,
            std::io::Cursor::new(session.as_bytes()),
            &mut out,
            state,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Split a session's stdout the way a client does: read lines until the
    /// blank one, that is one block. Panics if the last block was never
    /// terminated, which is the deadlock this protocol exists to avoid.
    fn blocks(out: &str) -> Vec<Vec<&str>> {
        let mut all = Vec::new();
        let mut cur = Vec::new();
        for line in out.lines() {
            if line.is_empty() {
                all.push(std::mem::take(&mut cur));
            } else {
                cur.push(line);
            }
        }
        assert!(cur.is_empty(), "unterminated trailing block: {cur:?}");
        all
    }

    #[test]
    fn serve_answers_every_query_with_a_blank_terminated_block() {
        let conn = seeded_conn();
        let out = serve(
            &conn,
            "total\nnothingmatchesthis\n",
            10,
            Matcher::Standard,
            false,
        );
        // Second block is empty but still terminated: a client that reads
        // until the blank line must not hang on a query that found nothing.
        assert_eq!(out, "/a.png\n\n\n");
    }

    #[test]
    fn serve_quit_ends_the_session_without_reading_further_input() {
        let conn = seeded_conn();
        let out = serve(&conn, "total\n:quit\ntotal\n", 10, Matcher::Standard, false);
        assert_eq!(out, "/a.png\n\n");
    }

    #[test]
    fn serve_limit_directive_applies_to_later_queries() {
        let conn = seeded_conn();
        let out = serve(
            &conn,
            "invoice\n:limit 1\ninvoice\n",
            10,
            Matcher::Standard,
            false,
        );
        let b = blocks(&out);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0].len(), 3);
        // The directive gets its own (empty) block before the next answer.
        assert!(b[1].is_empty());
        assert_eq!(b[2].len(), 1);
    }

    #[test]
    fn serve_rejects_a_bad_limit_and_keeps_the_old_one() {
        let conn = seeded_conn();
        let out = serve(
            &conn,
            ":limit 0\n:limit abc\ninvoice\n",
            2,
            Matcher::Standard,
            false,
        );
        let b = blocks(&out);
        assert_eq!(b.len(), 3);
        assert!(b[0].is_empty());
        assert!(b[1].is_empty());
        assert_eq!(b[2].len(), 2, "the starting limit of 2 must still hold");
    }

    #[test]
    fn serve_substring_directive_switches_matcher_both_ways() {
        let conn = seeded_conn();
        // "voic" is a substring of "invoice" but not a porter token, so it is
        // a clean probe for which table is being queried.
        let out = serve(
            &conn,
            "voic\n:substring on\nvoic\n:substring off\nvoic\n",
            10,
            Matcher::Standard,
            false,
        );
        let b = blocks(&out);
        assert_eq!(b.len(), 5);
        assert!(b[0].is_empty(), "porter must not match a bare substring");
        assert_eq!(b[2].len(), 3, "trigram must match it");
        assert!(b[4].is_empty(), "off must restore porter");
    }

    #[test]
    fn serve_unknown_directive_still_emits_its_block() {
        let conn = seeded_conn();
        let out = serve(&conn, ":bogus\ntotal\n", 10, Matcher::Standard, false);
        assert_eq!(out, "\n/a.png\n\n");
    }

    #[test]
    fn serve_skips_blank_input_lines_entirely() {
        let conn = seeded_conn();
        // A blank line is the *client's* terminator too; echoing a block back
        // for one would desynchronize a client that pads its writes.
        let out = serve(&conn, "\n   \ntotal\n", 10, Matcher::Standard, false);
        assert_eq!(out, "/a.png\n\n");
    }

    #[test]
    fn serve_verbose_prefixes_the_score() {
        let conn = seeded_conn();
        let out = serve(&conn, "total\n", 10, Matcher::Standard, true);
        let (score, path) = out.lines().next().unwrap().split_once('\t').unwrap();
        assert_eq!(path, "/a.png");
        assert!(score.parse::<f64>().is_ok(), "score not numeric: {score:?}");
    }

    #[test]
    fn serve_starts_in_the_mode_it_was_given() {
        let conn = seeded_conn();
        let out = serve(&conn, "voic\n", 10, Matcher::Substring, false);
        assert_eq!(out.lines().filter(|l| !l.is_empty()).count(), 3);
    }

    #[test]
    fn serve_fuzzy_directive_switches_matcher_both_ways() {
        let conn = seeded_conn();
        // "invoce" is a typo, not a substring: it reaches nothing in either
        // the porter or the trigram table without the vocabulary walk.
        let out = serve(
            &conn,
            "invoce\n:fuzzy on\ninvoce\n:fuzzy off\ninvoce\n",
            10,
            Matcher::Standard,
            false,
        );
        let b = blocks(&out);
        assert_eq!(b.len(), 5);
        assert!(b[0].is_empty(), "porter must not match a typo");
        assert_eq!(b[2].len(), 3, "fuzzy must correct it");
        assert!(b[4].is_empty(), "off must restore porter");
    }

    /// `:fuzzy off` then `:fuzzy on` has to come back with the budget the
    /// session was started with, not the default for that word length.
    #[test]
    fn serve_fuzzy_toggle_restores_the_starting_distance() {
        let conn = seeded_conn();
        let out = serve_with(
            &conn,
            ":fuzzy off\n:fuzzy on\ninvoce\n",
            ServeState {
                limit: 10,
                matcher: Matcher::Fuzzy(Some(0)),
                fuzzy_distance: Some(0),
                snippet: false,
                verbose: false,
            },
        );
        let b = blocks(&out);
        assert!(b[2].is_empty(), "a zero budget must survive the round trip");
    }

    #[test]
    fn serve_snippet_directive_switches_excerpts_both_ways() {
        let conn = seeded_conn();
        let out = serve(
            &conn,
            "total\n:snippet on\ntotal\n:snippet off\ntotal\n",
            10,
            Matcher::Standard,
            false,
        );
        let b = blocks(&out);
        assert_eq!(b.len(), 5);
        assert_eq!(b[0], vec!["/a.png"], "excerpts are off by default");
        let (path, excerpt) = b[2][0].split_once('\t').unwrap();
        assert_eq!(path, "/a.png");
        assert!(
            excerpt.contains("[total]"),
            "match not bracketed in {excerpt:?}"
        );
        assert_eq!(b[4], vec!["/a.png"], "off must restore bare paths");
    }

    #[test]
    fn serve_snippet_starts_on_when_the_flag_says_so() {
        let conn = seeded_conn();
        let out = serve_with(
            &conn,
            "total\n",
            ServeState {
                limit: 10,
                matcher: Matcher::Standard,
                fuzzy_distance: None,
                snippet: true,
                verbose: false,
            },
        );
        assert!(out.starts_with("/a.png\t"), "no excerpt in {out:?}");
    }

    #[test]
    fn serve_rejects_a_bad_snippet_toggle_and_keeps_the_old_state() {
        let conn = seeded_conn();
        let out = serve(&conn, ":snippet yes\ntotal\n", 10, Matcher::Standard, false);
        let b = blocks(&out);
        assert!(b[0].is_empty());
        assert_eq!(b[1], vec!["/a.png"], "a rejected toggle must not apply");
    }

    #[test]
    fn serve_snippet_keeps_one_line_per_result_on_multiline_ocr() {
        // The failure this pins: OCR content is mostly newlines, and a raw
        // snippet would emit one *inside* a block — which a client reading to
        // the first blank line would take as the terminator, desynchronizing
        // every answer after it.
        let conn = db::connect(Path::new(":memory:")).unwrap();
        db::upsert_file(
            &conn,
            "/m.png",
            1.0,
            NOW,
            "header\n\ninvoice total\n\nfooter",
            None,
        )
        .unwrap();
        let out = serve_with(
            &conn,
            "invoice\n",
            ServeState {
                limit: 10,
                matcher: Matcher::Standard,
                fuzzy_distance: None,
                snippet: true,
                verbose: false,
            },
        );
        let b = blocks(&out);
        assert_eq!(b.len(), 1, "excerpt newlines leaked into the protocol");
        assert_eq!(b[0].len(), 1);
        assert!(b[0][0].starts_with("/m.png\t"));
    }

    #[test]
    fn result_line_orders_the_fields_score_path_snippet() {
        let r = crate::models::SearchResult {
            path: "/a.png".to_string(),
            score: -1.25,
            snippet: Some("an [invoice] here".to_string()),
            mtime: 1.0,
            lang: Some("eng".to_string()),
        };
        assert_eq!(result_line(&r, false), "/a.png\tan [invoice] here");
        assert_eq!(result_line(&r, true), "-1.2500\t/a.png\tan [invoice] here");
        let bare = crate::models::SearchResult { snippet: None, ..r };
        assert_eq!(result_line(&bare, false), "/a.png");
        assert_eq!(result_line(&bare, true), "-1.2500\t/a.png");
    }
}
