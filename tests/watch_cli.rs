//! `lq watch` refusals, across the process boundary.
//!
//! Only the checks that run *before* the Tesseract pre-flight are asserted
//! here: those are the ones whose message is the same on a machine with a
//! library and on one without, so the test says the same thing in CI as it
//! does on a developer's box. The happy path needs a live watcher and a real
//! OCR pass, and belongs in a manual run, not here.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// A scratch directory that removes itself. `Cargo.toml` has no
/// `[dev-dependencies]`, so this is the local stand-in for `tempfile`.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "lq-watch-cli-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn watch(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_lq"))
        .arg("watch")
        .args(args)
        .output()
        .expect("run lq watch");
    assert!(out.stdout.is_empty(), "watch must write nothing to stdout");
    (
        out.status.code().expect("exit code"),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_missing_directory_is_an_environment_error() {
    let dir = TempDir::new("missing");
    let absent = dir.0.join("nope");
    let (code, err) = watch(&[absent.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(err.contains("is not a directory"), "{err}");
}

#[test]
fn debounce_outside_the_range_is_refused_by_value() {
    let dir = TempDir::new("debounce");
    let root = dir.0.to_str().unwrap();
    for bad in ["0", "0.05", "301"] {
        let (code, err) = watch(&[root, "--debounce", bad]);
        assert_eq!(code, 2, "--debounce {bad}: {err}");
        assert!(err.contains("--debounce must be between"), "{err}");
    }
}

#[test]
fn a_debounce_inside_the_range_gets_past_the_range_check() {
    let dir = TempDir::new("ok-debounce");
    let root = dir.0.to_str().unwrap();
    // 0.1 and 300 are the documented bounds and must both be accepted. What
    // comes next is the Tesseract pre-flight, which may or may not pass here;
    // all this asserts is that the range check is not what stopped it.
    for good in ["0.1", "300"] {
        let (_, err) = watch(&[root, "--debounce", good, "--engine", "nonsense"]);
        assert!(err.contains("--engine must be"), "{err}");
    }
}
