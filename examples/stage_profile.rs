//! Per-image timing split across decode, preprocess, and recognize.
//!
//! This exists to answer one question with a measurement instead of an opinion:
//! **how much of an image's cost is the recognizer, and therefore how much is
//! even available to optimize?** The answer — recognize is ~84% — is what
//! closed SIMD, zero-copy decoding, and faster directory walking as
//! optimization targets. Anyone who wants to reopen one of those should re-run
//! this first; see [benchmarks](../docs/benchmarks.md) §1.
//!
//! Method, and why it is this method:
//!
//!   * **A seeded random sample**, not the first N alphabetically. Directory
//!     order correlates with source (camera, screenshot tool, scanner) and
//!     source correlates with image size, so an alphabetical prefix profiles
//!     one kind of image and calls it a corpus.
//!   * **Min-of-N per image per stage.** The minimum is the right estimator for
//!     "how long does this work take" on a machine with other things running:
//!     noise only ever adds time, so the mean measures the machine and the
//!     minimum measures the code.
//!   * **The same file set `lq index` would take**, via [`indexer::discover`].
//!     A profiler that picked files by its own rule answers a question about a
//!     different corpus.
//!
//! Usage:
//!     cargo run --release --example stage_profile -- <dir> [--sample 1000]
//!         [--repeat 3] [--seed 7] [--lang eng+hrv] [--min-conf 40]

use std::path::PathBuf;

use lensquery::indexer;
use lensquery::ocr;

fn main() {
    // This profiler is single-threaded, so the lazy `setenv` was never a race
    // here — but it read `OMP_THREAD_LIMIT` the indexer sets, and a profile run
    // under different OpenMP settings than the thing being profiled is not a
    // measurement of the thing being profiled.
    lensquery::tess::init_process_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir: Option<PathBuf> = None;
    let mut sample = 1000usize;
    let mut repeat = 3usize;
    let mut seed = 7u64;
    let mut lang = String::from("eng+hrv");
    let mut min_conf = 40.0f32;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let next = |i: &mut usize| -> String {
            *i += 1;
            args.get(*i).cloned().unwrap_or_default()
        };
        match a {
            "--sample" => sample = next(&mut i).parse().unwrap_or(sample),
            "--repeat" => repeat = next(&mut i).parse().unwrap_or(repeat),
            "--seed" => seed = next(&mut i).parse().unwrap_or(seed),
            "--lang" => lang = next(&mut i),
            "--min-conf" => min_conf = next(&mut i).parse().unwrap_or(min_conf),
            other => dir = Some(PathBuf::from(other)),
        }
        i += 1;
    }
    let Some(dir) = dir else {
        eprintln!("usage: stage_profile <dir> [--sample N] [--repeat N] [--seed N]");
        std::process::exit(2);
    };

    // `discover` pairs each file with the mtime it got free from the directory
    // walk; the profiler indexes nothing, so it drops the mtimes here and keeps
    // working on plain paths.
    let mut paths: Vec<PathBuf> = indexer::discover(&dir)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    if paths.is_empty() {
        eprintln!("no supported images under {}", dir.display());
        std::process::exit(1);
    }
    let total_found = paths.len();
    // Sort first: `discover` returns filesystem order, which is not stable
    // across runs, and a "seeded" sample drawn from an unstable order is not
    // reproducible.
    paths.sort();
    shuffle(&mut paths, seed);
    paths.truncate(sample);
    let n = paths.len();

    println!(
        "corpus {total_found} images -> sample {n} (seed {seed}), \
         {repeat} passes, lang={lang}, min_conf={min_conf}"
    );

    // Warm-up: the first recognize call pays the one-time model load, which
    // would otherwise land entirely on image 1 of pass 1.
    let _ = ocr::extract_text_timed(
        &paths[0],
        &lang,
        min_conf,
        false,
        ocr::DEFAULT_THOROUGH_TRIGGER_WORDS,
    );

    let mut best: Vec<ocr::StageTimings> = vec![
        ocr::StageTimings {
            decode: f64::INFINITY,
            preprocess: f64::INFINITY,
            recognize: f64::INFINITY,
        };
        n
    ];
    let mut failed = 0usize;
    for pass in 1..=repeat {
        println!("pass {pass}/{repeat} ...");
        for (idx, p) in paths.iter().enumerate() {
            let (result, t) = ocr::extract_text_timed(
                p,
                &lang,
                min_conf,
                false,
                ocr::DEFAULT_THOROUGH_TRIGGER_WORDS,
            );
            if matches!(result, ocr::Ocr::Failed) {
                if pass == 1 {
                    failed += 1;
                }
                continue;
            }
            let b = &mut best[idx];
            b.decode = b.decode.min(t.decode);
            b.preprocess = b.preprocess.min(t.preprocess);
            b.recognize = b.recognize.min(t.recognize);
        }
    }

    let kept: Vec<ocr::StageTimings> = best.into_iter().filter(|t| t.decode.is_finite()).collect();
    if kept.is_empty() {
        eprintln!("every image failed to decode; nothing to report");
        std::process::exit(1);
    }
    report(&kept, failed);
}

/// Fisher–Yates with a SplitMix64 stream — reproducible without a `rand` dep,
/// which this crate deliberately does not carry.
fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for i in (1..items.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

fn report(t: &[ocr::StageTimings], failed: usize) {
    let n = t.len() as f64;
    let decode: f64 = t.iter().map(|x| x.decode).sum();
    let preprocess: f64 = t.iter().map(|x| x.preprocess).sum();
    let recognize: f64 = t.iter().map(|x| x.recognize).sum();
    let grand = decode + preprocess + recognize;

    println!("\n{}", "=".repeat(66));
    println!(
        "PER-STAGE SPLIT  (n={} images, min-of-N per image per stage, {failed} failed)",
        t.len()
    );
    println!("{}", "=".repeat(66));
    println!(
        "{:<12} {:>9} {:>10} {:>9} {:>8}",
        "stage", "mean ms", "median ms", "p95 ms", "share"
    );
    println!("{}", "-".repeat(66));
    for (name, total, get) in [
        ("decode", decode, 0usize),
        ("preprocess", preprocess, 1),
        ("recognize", recognize, 2),
    ] {
        let mut vals: Vec<f64> = t
            .iter()
            .map(|x| {
                1000.0
                    * match get {
                        0 => x.decode,
                        1 => x.preprocess,
                        _ => x.recognize,
                    }
            })
            .collect();
        vals.sort_by(f64::total_cmp);
        let median = vals[vals.len() / 2];
        let p95 = vals[((vals.len() - 1) as f64 * 0.95) as usize];
        println!(
            "{name:<12} {:>9.2} {median:>10.2} {p95:>9.2} {:>7.1}%",
            total / n * 1000.0,
            total / grand * 100.0
        );
    }
    println!("{}", "-".repeat(66));
    println!(
        "{:<12} {:>9.2} {:>10} {:>9} {:>7.1}%",
        "TOTAL",
        grand / n * 1000.0,
        "",
        "",
        100.0
    );

    let other = grand - recognize;
    println!(
        "\nEverything that is not recognize: {:.1}% ({:.1} ms/img)",
        other / grand * 100.0,
        other / n * 1000.0
    );
    println!(
        "  -> a core that made decode+preprocess *free* would be {:.3}x faster, no more.",
        grand / recognize
    );
    println!(
        "  -> at {:.2} img/s/thread that is {:.1} ms/img saved, best case.",
        1.0 / (grand / n),
        other / n * 1000.0
    );
}
