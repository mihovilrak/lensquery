//! End-to-end tests for `--json`: the real binary, real pipes, real streams.
//!
//! The one thing a `--json` flag reliably gets wrong is letting human chatter
//! onto stdout, where it corrupts the very stream a script is parsing. That is
//! only observable across a process boundary — inside the crate, `println!`
//! and `eprintln!` are equally invisible — so these tests run `lq` and read the
//! two streams apart.
//!
//! Key *order* is asserted, not just presence: it is documented in
//! `docs/json-output.md` and it is what breaks silently if someone swaps the
//! structs for `serde_json::json!`, whose map sorts keys alphabetically.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const NOW: &str = "2026-08-17T10:00:00Z";

/// A scratch directory that removes itself. `Cargo.toml` has no
/// `[dev-dependencies]` (see the crate's build notes), so this is the local
/// stand-in for `tempfile`.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "lq-json-cli-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Two rows sharing a porter token, one with a language recorded and one
/// without — the pre-`files.lang` case that must serialize as `null`.
fn seeded_db(dir: &TempDir) -> PathBuf {
    let db_path = dir.join("index.db");
    let conn = lensquery::db::connect(&db_path).expect("open seed db");
    lensquery::db::upsert_file(
        &conn,
        "/a.png",
        1.0,
        NOW,
        "the invoice total is due",
        Some("eng"),
    )
    .unwrap();
    lensquery::db::upsert_file(&conn, "/b.png", 2.0, NOW, "invoice draft", None).unwrap();
    drop(conn);
    db_path
}

/// Every invocation passes `--db` and `--limit` so a real user config at
/// `~/.lensquery/config.toml` cannot perturb the result.
fn run(db_path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lq"))
        .args(args)
        .arg("--db")
        .arg(db_path)
        .output()
        .expect("run lq")
}

fn stdout_lines(out: &Output) -> Vec<String> {
    String::from_utf8(out.stdout.clone())
        .expect("stdout is utf-8")
        .lines()
        .map(str::to_string)
        .collect()
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("stderr is utf-8")
}

/// The keys of `expected` that appear in `line`, in the order they appear.
fn key_order<'a>(line: &str, expected: &[&'a str]) -> Vec<&'a str> {
    let mut found: Vec<(usize, &str)> = expected
        .iter()
        .filter_map(|k| line.find(&format!("\"{k}\":")).map(|i| (i, *k)))
        .collect();
    found.sort_unstable();
    found.into_iter().map(|(_, k)| k).collect()
}

#[test]
fn search_json_is_one_object_per_line_in_the_documented_order() {
    let dir = TempDir::new("search");
    let db = seeded_db(&dir);
    let out = run(&db, &["search", "invoice", "--json", "--limit", "10"]);

    assert!(out.status.success(), "exit: {:?}", out.status);
    let lines = stdout_lines(&out);
    assert_eq!(lines.len(), 2, "one line per hit: {lines:?}");
    for line in &lines {
        assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
        assert_eq!(
            key_order(line, &["path", "score", "mtime", "lang", "snippet"]),
            ["path", "score", "mtime", "lang", "snippet"],
            "{line}"
        );
    }
    let a = lines.iter().find(|l| l.contains("/a.png")).expect("a.png");
    assert!(a.contains("\"mtime\":1.0"), "{a}");
    assert!(a.contains("\"lang\":\"eng\""), "{a}");
    let b = lines.iter().find(|l| l.contains("/b.png")).expect("b.png");
    assert!(
        b.contains("\"lang\":null"),
        "a row indexed before files.lang: {b}"
    );
}

#[test]
fn search_json_omits_the_snippet_key_when_it_was_refused() {
    let dir = TempDir::new("nosnippet");
    let db = seeded_db(&dir);
    let out = run(
        &db,
        &[
            "search",
            "invoice",
            "--json",
            "--no-snippet",
            "--limit",
            "10",
        ],
    );

    assert!(out.status.success());
    for line in stdout_lines(&out) {
        assert!(!line.contains("\"snippet\""), "absent, not null: {line}");
        assert_eq!(
            key_order(&line, &["path", "score", "mtime", "lang"]),
            ["path", "score", "mtime", "lang"],
            "{line}"
        );
    }
}

#[test]
fn search_json_keeps_stdout_pure_when_verbose_is_also_set() {
    let dir = TempDir::new("verbose");
    let db = seeded_db(&dir);
    let out = run(
        &db,
        &["search", "invoice", "--json", "--verbose", "--limit", "10"],
    );

    assert!(out.status.success());
    for line in stdout_lines(&out) {
        assert!(line.starts_with('{'), "verbose leaked onto stdout: {line}");
        assert!(!line.contains('\t'), "the text format leaked: {line}");
    }
}

#[test]
fn search_json_says_nothing_on_stdout_when_there_are_no_hits() {
    let dir = TempDir::new("empty");
    let db = seeded_db(&dir);
    let out = run(&db, &["search", "zzzznothing", "--json", "--limit", "10"]);

    assert!(out.status.success(), "an empty result set is not an error");
    assert!(
        out.stdout.is_empty(),
        "stdout must stay a valid JSON stream: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr_text(&out).contains("No results found"),
        "the human line belongs on stderr"
    );
}

#[test]
fn search_prints_the_snippet_by_default_and_drops_it_on_request() {
    let dir = TempDir::new("default");
    let db = seeded_db(&dir);

    let out = run(&db, &["search", "invoice", "--limit", "10"]);
    assert!(out.status.success());
    let lines = stdout_lines(&out);
    assert!(
        lines.iter().all(|l| l.contains('\t')),
        "the excerpt is on by default: {lines:?}"
    );

    let out = run(&db, &["search", "invoice", "--no-snippet", "--limit", "10"]);
    let lines = stdout_lines(&out);
    assert!(
        lines.iter().all(|l| !l.contains('\t')),
        "--no-snippet leaves paths only: {lines:?}"
    );
}

#[test]
fn status_json_is_one_flat_object() {
    let dir = TempDir::new("status");
    let db = seeded_db(&dir);
    let out = run(&db, &["status", "--json"]);

    assert!(out.status.success());
    let lines = stdout_lines(&out);
    assert_eq!(lines.len(), 1, "a single object, not JSON Lines: {lines:?}");
    let line = &lines[0];
    assert_eq!(
        key_order(
            line,
            &[
                "db_path",
                "file_count",
                "db_bytes",
                "last_indexed_at",
                "schema_version",
            ]
        ),
        [
            "db_path",
            "file_count",
            "db_bytes",
            "last_indexed_at",
            "schema_version",
        ],
        "{line}"
    );
    assert!(line.contains("\"file_count\":2"), "{line}");
}

#[test]
fn doctor_json_is_one_object_and_reports_the_index() {
    let dir = TempDir::new("doctor");
    let db = seeded_db(&dir);
    let out = run(&db, &["doctor", "--json"]);

    assert!(out.status.success());
    let lines = stdout_lines(&out);
    assert_eq!(lines.len(), 1, "one object: {lines:?}");
    let line = &lines[0];
    assert!(line.starts_with("{\"lq_version\":"), "{line}");
    assert!(line.contains("\"db_status\":\"ok\""), "{line}");
    assert!(line.contains("\"file_count\":2"), "{line}");
}

#[test]
fn doctor_json_reports_a_database_that_is_not_there() {
    let dir = TempDir::new("doctor-missing");
    let out = run(&dir.join("nope.db"), &["doctor", "--json"]);

    assert!(out.status.success());
    let lines = stdout_lines(&out);
    assert_eq!(lines.len(), 1, "one object: {lines:?}");
    assert!(
        lines[0].contains("\"db_status\":\"not_initialized\""),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("\"stats\":null"), "{}", lines[0]);
}
