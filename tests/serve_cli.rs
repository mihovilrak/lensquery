//! End-to-end tests for `lq serve`: the real binary, over real pipes.
//!
//! The in-module tests in `main.rs` drive [`serve_loop`] directly over a
//! `Cursor`/`Vec` pair. That covers the protocol's *logic* but it cannot cover
//! the two things that actually break a long-lived stdio server:
//!
//! 1. **Flushing.** A `Vec<u8>` writer never blocks and never buffers, so an
//!    unflushed block looks identical to a flushed one. Over a real pipe it is
//!    the difference between a client that gets an answer and a client that
//!    hangs forever. Every test here that reads a block reads it *before*
//!    sending the next line, with a timeout that kills the child instead of
//!    wedging the suite.
//! 2. **`cmd_serve` itself** — flag resolution (`--db`, `--limit`, `--snippet`),
//!    the exit code, and what happens on a database that cannot be opened. None
//!    of that is reachable from `serve_loop`.
//!
//! Every test passes `--db` and `--limit` explicitly so a real user config at
//! `~/.lensquery/config.toml` cannot perturb the result.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const NOW: &str = "2026-08-17T10:00:00Z";

/// How long a block may take to arrive before we call it a deadlock. Generous:
/// this is a failure bound, not a performance one, and it is only ever reached
/// when the test is about to fail anyway.
const BLOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// A scratch directory that removes itself. `Cargo.toml` has no
/// `[dev-dependencies]` (see the crate's build notes), so this is the local
/// stand-in for `tempfile` — 15 lines, no registry access.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "lq-serve-cli-{}-{}-{tag}",
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
        // Best effort: a leaked temp dir must never fail a test run.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A three-file index on disk. Two rows share a porter token so `:limit` and
/// `--limit` have something to cut.
fn seeded_db(dir: &TempDir) -> PathBuf {
    let db_path = dir.join("index.db");
    let conn = lensquery::db::connect(&db_path).expect("open seed db");
    lensquery::db::upsert_file(&conn, "/a.png", 1.0, NOW, "the invoice total is due", None)
        .unwrap();
    lensquery::db::upsert_file(&conn, "/b.png", 2.0, NOW, "invoice draft", None).unwrap();
    lensquery::db::upsert_file(&conn, "/c.png", 3.0, NOW, "grocery list milk bread", None).unwrap();
    // Closed before the child opens it: the child must not depend on this
    // process holding a connection.
    drop(conn);
    db_path
}

fn serve_command(db_path: &Path, extra: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lq"));
    cmd.arg("serve")
        .arg("--db")
        .arg(db_path)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Feed the whole session at once and wait for the process to end. Use this
/// when the test is about the exit code or the transcript as a whole; use
/// [`Session`] when it is about the server answering *while* it is still
/// reading.
fn run_session(db_path: &Path, extra: &[&str], input: &str) -> Output {
    let mut child = serve_command(db_path, extra)
        .spawn()
        .expect("spawn lq serve");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write session");
    // stdin dropped above -> EOF -> the loop ends on its own.
    child.wait_with_output().expect("wait for lq serve")
}

/// A live `lq serve` process with its stdout drained on a background thread, so
/// the test can read a block without risking a blocking read that never
/// returns.
struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl Session {
    fn start(db_path: &Path, extra: &[&str]) -> Session {
        let mut child = serve_command(db_path, extra)
            .spawn()
            .expect("spawn lq serve");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Session {
            child,
            stdin: Some(stdin),
            lines: rx,
        }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("session still open");
        writeln!(stdin, "{line}").expect("write line");
        stdin.flush().expect("flush line");
    }

    /// Read one blank-line-terminated block. Kills the child and fails the test
    /// if the block does not arrive — an unflushed writer would otherwise hang
    /// the whole suite, which is the exact bug this file exists to catch.
    fn block(&mut self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        loop {
            match self.lines.recv_timeout(BLOCK_TIMEOUT) {
                Ok(line) if line.is_empty() => return out,
                Ok(line) => out.push(line),
                Err(e) => {
                    let _ = self.child.kill();
                    panic!("no terminated block within {BLOCK_TIMEOUT:?} ({e}); partial: {out:?}");
                }
            }
        }
    }

    /// Close stdin and wait. Returns the exit status.
    fn finish(mut self) -> ExitStatus {
        self.stdin.take();
        self.child.wait().expect("wait for lq serve")
    }
}

/// Split a transcript the way a client does: lines until a blank one are one
/// block. Panics on an unterminated trailing block — the deadlock the protocol
/// exists to prevent.
fn blocks(out: &Output) -> Vec<Vec<String>> {
    let text = stdout_of(out);
    let mut all = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            all.push(std::mem::take(&mut cur));
        } else {
            cur.push(line.to_string());
        }
    }
    assert!(cur.is_empty(), "unterminated trailing block: {cur:?}");
    all
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("stdout is utf-8")
}

#[test]
fn serve_answers_a_query_before_the_next_line_is_written() {
    // The flush contract. If `serve_loop` buffered its block until EOF this
    // would time out, while the in-process tests would still pass.
    let dir = TempDir::new("flush");
    let db_path = seeded_db(&dir);
    let mut s = Session::start(&db_path, &["--limit", "10"]);

    s.send("total");
    assert_eq!(s.block(), vec!["/a.png".to_string()]);

    // Second round-trip on the same warm process — the point of `serve`.
    s.send("grocery");
    assert_eq!(s.block(), vec!["/c.png".to_string()]);

    // A miss is still a terminated (empty) block, not silence.
    s.send("nothingmatchesthisatall");
    assert!(s.block().is_empty());

    assert!(s.finish().success());
}

#[test]
fn serve_ends_on_stdin_eof_with_exit_zero() {
    let dir = TempDir::new("eof");
    let db_path = seeded_db(&dir);
    let out = run_session(&db_path, &["--limit", "10"], "total\ninvoice\n");
    assert!(out.status.success(), "stderr: {:?}", out.stderr);
    assert_eq!(blocks(&out).len(), 2);
}

#[test]
fn serve_quit_stops_reading_the_rest_of_stdin() {
    let dir = TempDir::new("quit");
    let db_path = seeded_db(&dir);
    let out = run_session(&db_path, &["--limit", "10"], "total\n:quit\ninvoice\n");
    assert!(out.status.success());
    let b = blocks(&out);
    assert_eq!(b.len(), 1, "input after :quit was answered: {b:?}");
    assert_eq!(b[0], vec!["/a.png".to_string()]);
}

#[test]
fn serve_limit_flag_sets_the_starting_limit() {
    let dir = TempDir::new("limit");
    let db_path = seeded_db(&dir);
    // Two rows match "invoice"; the flag has to survive the u32 -> i64 hop in
    // `cmd_serve` for this to be 1 rather than 2.
    let out = run_session(&db_path, &["--limit", "1"], "invoice\n");
    assert_eq!(blocks(&out)[0].len(), 1);
}

#[test]
fn serve_limit_directive_overrides_the_flag_mid_session() {
    let dir = TempDir::new("limit-directive");
    let db_path = seeded_db(&dir);
    let out = run_session(&db_path, &["--limit", "1"], "invoice\n:limit 10\ninvoice\n");
    let b = blocks(&out);
    assert_eq!(b[0].len(), 1);
    assert!(b[1].is_empty(), "a directive answers with an empty block");
    assert_eq!(b[2].len(), 2, "the :limit directive did not take: {b:?}");
}

#[test]
fn serve_snippet_flag_reaches_the_pipe_as_a_tab_field() {
    let dir = TempDir::new("snippet");
    let db_path = seeded_db(&dir);
    let out = run_session(&db_path, &["--limit", "10", "--snippet"], "total\n");
    let b = blocks(&out);
    let line = &b[0][0];
    let (path, snippet) = line
        .split_once('\t')
        .unwrap_or_else(|| panic!("no snippet field on {line:?}"));
    assert_eq!(path, "/a.png");
    assert!(
        snippet.contains("[total]"),
        "match not bracketed: {snippet:?}"
    );
}

#[test]
fn serve_substring_flag_reaches_the_pipe() {
    let dir = TempDir::new("substring");
    let db_path = seeded_db(&dir);
    // A substring no porter token starts with: only the trigram table can find
    // it, so this fails if `--substring` is dropped on the way to `run_query`.
    let out = run_session(&db_path, &["--limit", "10", "--substring"], "nvoic\n");
    assert_eq!(blocks(&out)[0].len(), 2);
}

#[test]
fn serve_fuzzy_flag_reaches_the_pipe() {
    let dir = TempDir::new("fuzzy");
    let db_path = seeded_db(&dir);
    // A typo, not a substring — nothing but the vocabulary walk reaches it.
    let out = run_session(&db_path, &["--limit", "10", "--fuzzy"], "invoce\n");
    assert_eq!(blocks(&out)[0].len(), 2);
}

#[test]
fn serve_refuses_both_matchers_at_once() {
    let dir = TempDir::new("conflict");
    let db_path = seeded_db(&dir);
    let out = run_session(&db_path, &["--fuzzy", "--substring"], "total\n");
    assert!(!out.status.success(), "conflicting flags were accepted");
}

#[test]
fn serve_creates_a_missing_database_and_still_answers() {
    // `cmd_serve` resolves the path and hands it to `db::connect`, which
    // creates the parent and the schema. A first-run user must get empty
    // answers, not a crash.
    let dir = TempDir::new("fresh");
    let db_path = dir.join("nested").join("new.db");
    let out = run_session(&db_path, &["--limit", "10"], "anything\n");
    assert!(out.status.success(), "stderr: {:?}", out.stderr);
    assert!(blocks(&out)[0].is_empty());
    assert!(db_path.exists(), "the database was never created");
}

#[test]
fn serve_exits_two_when_the_database_cannot_be_opened() {
    let dir = TempDir::new("garbage");
    let db_path = dir.join("not-a-db.db");
    std::fs::write(&db_path, b"this is not an sqlite file, not even close").unwrap();
    let out = run_session(&db_path, &["--limit", "10"], "total\n");
    assert_eq!(
        out.status.code(),
        Some(2),
        "environment failures are exit 2; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("error:"),
        "the failure was not reported on stderr"
    );
    assert!(
        stdout_of(&out).is_empty(),
        "stdout must stay clean for the client parser"
    );
}
