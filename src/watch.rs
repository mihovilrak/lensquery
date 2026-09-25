//! Incremental indexing driven by filesystem events — the engine behind
//! `lq watch`.
//!
//! Three things shape this module:
//!
//!   * **It is not a second indexer.** A batch ends in [`indexer::index_paths`],
//!     which shares its skip-vs-OCR decision with `index_directory`. Watch mode
//!     decides *which* files to hand over and nothing else about how they are
//!     read.
//!   * **Debounce is not optional.** Saving one file in an editor produces a
//!     create, several writes and a rename; a sync client produces more. Events
//!     accumulate into a set until the directory has been quiet for the
//!     debounce interval, so six events over one file cost one OCR.
//!   * **A move is not a delete plus a create.** The text is already in the
//!     index; re-reading the image to learn what we know is the one avoidable
//!     cost a watcher can have. See [`plan`].
//!
//! Foreground only. There is no daemon, no service install and no autostart:
//! the process runs until Ctrl-C, and backgrounding it is the job of whatever
//! supervisor the machine already has (see `docs/watch.md`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::Watcher as _;
use rusqlite::Connection;

use crate::db::{self, DbError};
use crate::indexer;
use crate::models::IndexStats;

/// Longest a batch may keep growing before it is flushed anyway.
///
/// The debounce is a quiet-period timer, so a directory that never falls quiet
/// — an unpacking archive, an initial cloud sync — would postpone indexing
/// forever. At the cap the batch flushes and the next one starts.
const MAX_BATCH: Duration = Duration::from_secs(60);

/// Something the loop did, for the CLI to print. Returning these instead of
/// printing keeps the watcher usable from a test.
pub enum Event {
    /// The watcher is armed; from here on changes are seen.
    Ready(PathBuf),
    /// A file moved and kept its text: no OCR, one `UPDATE`.
    Renamed { from: String, to: String },
    /// Rows dropped for files that are gone.
    Deleted(u64),
    /// A batch was indexed.
    Indexed(IndexStats),
    /// The watcher backend reported a problem it recovered from.
    Warning(String),
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error(
        "the index {db} is inside the watched directory {root}; \
         indexing would trigger the watcher, which would index again. \
         Pass --db pointing outside {root}."
    )]
    IndexInsideRoot { db: PathBuf, root: PathBuf },
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("filesystem watcher: {0}")]
    Notify(#[from] notify::Error),
}

/// Worker default for watch mode: half the cores, never zero.
///
/// A watcher runs *while* the user works in the directory it watches. Batch
/// indexing owns the machine for a known stretch and should use all of it; a
/// watcher that does the same makes saving a screenshot feel like a stall.
pub fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1)
        .max(1)
}

/// Watch `root` and keep `db_path` current until the process is killed.
///
/// Returns only if the watcher's channel closes; everything else is reported
/// through `report` and the loop continues, because a watcher that exits on the
/// first unreadable file is a watcher nobody leaves running.
pub fn run(
    root: &Path,
    db_path: &Path,
    debounce: Duration,
    index_opts: &indexer::IndexOptions,
    report: &dyn Fn(Event),
) -> Result<(), WatchError> {
    // Same normalisation `lq index` applies, letter case included, so a watched
    // tree and an indexed tree key the same image to the same row.
    let root = indexer::resolve_root(root);
    if index_inside(&root, db_path) {
        return Err(WatchError::IndexInsideRoot {
            db: db_path.to_path_buf(),
            root,
        });
    }

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        // A closed receiver means the loop is gone; nothing to do about it.
        let _ = tx.send(res);
    })?;
    watcher.watch(&root, notify::RecursiveMode::Recursive)?;
    report(Event::Ready(root.clone()));

    loop {
        let Ok(first) = rx.recv() else {
            return Ok(());
        };
        let mut touched: HashSet<PathBuf> = HashSet::new();
        collect(first, &mut touched, report);

        // Quiet-period debounce: every new event restarts the timer, so a
        // burst is one batch no matter how long the burst runs — up to
        // `MAX_BATCH`, which exists so a burst that never ends still indexes.
        let started = Instant::now();
        let closed = loop {
            match rx.recv_timeout(debounce) {
                Ok(res) => {
                    collect(res, &mut touched, report);
                    if started.elapsed() >= MAX_BATCH {
                        break false;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break false,
                Err(mpsc::RecvTimeoutError::Disconnected) => break true,
            }
        };

        if !touched.is_empty() {
            apply(&touched, db_path, index_opts, report)?;
        }
        if closed {
            return Ok(());
        }
    }
}

/// Fold one watcher message into the pending set.
///
/// Extension filtering happens here, before anything is stat'd or opened, and
/// against [`indexer::is_supported`] — the same list the directory walk uses,
/// so no format can be indexable but unwatchable.
fn collect(
    res: notify::Result<notify::Event>,
    touched: &mut HashSet<PathBuf>,
    report: &dyn Fn(Event),
) {
    let event = match res {
        Ok(e) => e,
        Err(e) => {
            report(Event::Warning(e.to_string()));
            return;
        }
    };
    // Reads are not changes. Writes announce themselves as Modify/Create on
    // every backend, so dropping the whole Access family costs nothing and
    // keeps a directory someone is browsing from waking the indexer.
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return;
    }
    for path in event.paths {
        if indexer::is_supported(&path) {
            touched.insert(path);
        }
    }
}

/// What a debounced batch turned out to mean.
pub(crate) struct Plan {
    /// `(old, new)` for files that moved with their content intact.
    pub renames: Vec<(String, String)>,
    /// Indexed paths whose files are gone.
    pub deletes: Vec<String>,
    /// Paths to hand to the indexer, which decides what actually changed.
    pub changed: Vec<PathBuf>,
}

/// Sort a batch into moves, deletes and work.
///
/// A rename is recognised by pairing a vanished indexed path with an appeared
/// unindexed one **carrying the same mtime**, both inside the same batch. That
/// is backend-independent: it works whether the platform reports a rename as
/// one paired event, two halves, or a delete and a create, because the debounce
/// window puts both halves in front of us at once.
///
/// The comparison is exact float equality on purpose. The two mtimes describe
/// the same inode and are the same number; if a filesystem ever disagrees the
/// pair is simply not recognised, and the file is re-OCR'd — slower, never
/// wrong.
pub(crate) fn plan(conn: &Connection, touched: &HashSet<PathBuf>) -> Result<Plan, DbError> {
    let (changed, gone): (Vec<PathBuf>, Vec<PathBuf>) =
        touched.iter().cloned().partition(|p| p.exists());

    // Files the index has never seen are the only rename destinations worth
    // considering; anything already indexed is handled by the mtime check.
    let mut arrivals: HashMap<usize, f64> = HashMap::new();
    for (i, path) in changed.iter().enumerate() {
        let key = path.to_string_lossy();
        if db::get_file_mtime(conn, &key)?.is_none() {
            if let Some(mtime) = disk_mtime(path) {
                arrivals.insert(i, mtime);
            }
        }
    }

    let mut renames = Vec::new();
    let mut deletes = Vec::new();
    let mut claimed: HashSet<usize> = HashSet::new();
    for path in &gone {
        let key = path.to_string_lossy().into_owned();
        let Some(stored) = db::get_file_mtime(conn, &key)? else {
            continue; // Never indexed, now absent: nothing to do.
        };
        let matched = arrivals
            .iter()
            .find(|(i, mtime)| !claimed.contains(i) && **mtime == stored)
            .map(|(i, _)| *i);
        match matched {
            Some(i) => {
                claimed.insert(i);
                renames.push((key, changed[i].to_string_lossy().into_owned()));
            }
            None => deletes.push(key),
        }
    }

    let changed = changed
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !claimed.contains(i))
        .map(|(_, p)| p)
        .collect();
    Ok(Plan {
        renames,
        deletes,
        changed,
    })
}

/// Execute one batch: moves first, then deletes, then OCR.
fn apply(
    touched: &HashSet<PathBuf>,
    db_path: &Path,
    index_opts: &indexer::IndexOptions,
    report: &dyn Fn(Event),
) -> Result<(), WatchError> {
    let conn = db::connect(db_path)?;
    let plan = plan(&conn, touched)?;
    for (from, to) in &plan.renames {
        if db::rename_file(&conn, from, to)? {
            report(Event::Renamed {
                from: from.clone(),
                to: to.clone(),
            });
        }
    }
    if !plan.deletes.is_empty() {
        let n = db::delete_files(&conn, &plan.deletes)?;
        if n > 0 {
            report(Event::Deleted(n));
        }
    }
    // The indexer opens its own connection; holding a second one across an OCR
    // batch would keep a reader open on the WAL for no reason.
    drop(conn);

    if !plan.changed.is_empty() {
        let stats = indexer::index_paths(&plan.changed, db_path, index_opts)?;
        if stats.indexed + stats.updated + stats.failed > 0 {
            report(Event::Indexed(stats));
        }
    }
    Ok(())
}

/// `true` when the index file would sit inside the watched tree.
///
/// Every write to the database — and to its `-wal` and `-shm` siblings — is a
/// filesystem event. Inside the watched tree that event arrives back at this
/// loop, which is a feedback loop even though the extension filter would drop
/// it today: the guard is against the design, not against one filename.
fn index_inside(root: &Path, db_path: &Path) -> bool {
    let dir = db_path.parent().unwrap_or(Path::new("."));
    let dir = std::fs::canonicalize(dir)
        .map(strip_verbatim)
        .unwrap_or_else(|_| dir.to_path_buf());
    dir.starts_with(root)
}

fn disk_mtime(path: &Path) -> Option<f64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .ok()
}

/// Drop the Windows `\\?\` prefix `canonicalize` adds, so paths compare and
/// print the way the user typed them. Mirrors `indexer::resolve`.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    const NOW: &str = "2026-08-18T10:00:00Z";

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "lq-watch-{}-{}-{tag}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"not really an image").unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Index `path` at whatever mtime it currently has on disk.
    fn seed(conn: &Connection, path: &Path) {
        db::upsert_file(
            conn,
            &path.to_string_lossy(),
            disk_mtime(path).unwrap(),
            NOW,
            "invoice",
            None,
        )
        .unwrap();
    }

    fn touched(paths: &[&Path]) -> HashSet<PathBuf> {
        paths.iter().map(|p| p.to_path_buf()).collect()
    }

    #[test]
    fn a_move_becomes_a_rename_not_an_ocr_job() {
        let dir = TempDir::new("move");
        let conn = db::connect(Path::new(":memory:")).unwrap();
        let old = dir.file("a.png");
        seed(&conn, &old);
        let new = dir.0.join("b.png");
        std::fs::rename(&old, &new).unwrap();

        let plan = plan(&conn, &touched(&[&old, &new])).unwrap();
        assert_eq!(plan.renames.len(), 1, "the pair was not recognised");
        assert!(plan.deletes.is_empty(), "a move is not a delete");
        assert!(
            plan.changed.is_empty(),
            "the moved file must not be re-OCR'd"
        );

        db::rename_file(&conn, &plan.renames[0].0, &plan.renames[0].1).unwrap();
        assert!(db::get_file_mtime(&conn, &new.to_string_lossy())
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_vanished_file_without_a_partner_is_a_delete() {
        let dir = TempDir::new("delete");
        let conn = db::connect(Path::new(":memory:")).unwrap();
        let gone = dir.file("a.png");
        seed(&conn, &gone);
        std::fs::remove_file(&gone).unwrap();

        let plan = plan(&conn, &touched(&[&gone])).unwrap();
        assert_eq!(plan.deletes, vec![gone.to_string_lossy().into_owned()]);
        assert!(plan.renames.is_empty());
    }

    #[test]
    fn an_unindexed_file_that_vanishes_is_nothing_at_all() {
        let dir = TempDir::new("ghost");
        let conn = db::connect(Path::new(":memory:")).unwrap();
        let ghost = dir.0.join("never-indexed.png");

        let plan = plan(&conn, &touched(&[&ghost])).unwrap();
        assert!(plan.deletes.is_empty());
        assert!(plan.renames.is_empty());
        assert!(plan.changed.is_empty());
    }

    #[test]
    fn a_new_file_is_work_and_an_indexed_one_is_still_offered() {
        let dir = TempDir::new("new");
        let conn = db::connect(Path::new(":memory:")).unwrap();
        let known = dir.file("known.png");
        seed(&conn, &known);
        let fresh = dir.file("fresh.png");

        let plan = plan(&conn, &touched(&[&known, &fresh])).unwrap();
        // Both go to the indexer: deciding that `known` is unchanged is the
        // indexer's job, and doing it here would be the second code path this
        // module exists to avoid.
        assert_eq!(plan.changed.len(), 2);
        assert!(plan.renames.is_empty() && plan.deletes.is_empty());
    }

    #[test]
    fn the_index_may_not_live_in_the_watched_tree() {
        let dir = TempDir::new("guard");
        let sub = dir.0.join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(index_inside(&dir.0, &dir.0.join("index.db")));
        // A subdirectory is still the watched tree — the walk is recursive.
        assert!(index_inside(&dir.0, &sub.join("index.db")));
        assert!(!index_inside(
            &dir.0,
            &dir.0.parent().unwrap().join("index.db")
        ));
    }

    #[test]
    fn reads_never_wake_the_indexer() {
        let dir = TempDir::new("access");
        let path = dir.file("a.png");
        let mut set = HashSet::new();
        let quiet = |_: Event| panic!("no report expected");
        collect(
            Ok(
                notify::Event::new(notify::EventKind::Access(notify::event::AccessKind::Read))
                    .add_path(path.clone()),
            ),
            &mut set,
            &quiet,
        );
        assert!(set.is_empty());

        collect(
            Ok(
                notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                    .add_path(path.clone()),
            ),
            &mut set,
            &quiet,
        );
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn non_images_are_dropped_before_anything_touches_the_disk() {
        let dir = TempDir::new("ext");
        let mut set = HashSet::new();
        let quiet = |_: Event| panic!("no report expected");
        collect(
            Ok(
                notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any))
                    .add_path(dir.0.join("notes.txt"))
                    .add_path(dir.0.join("shot.PNG")),
            ),
            &mut set,
            &quiet,
        );
        assert_eq!(set.len(), 1, "case-insensitive image match only: {set:?}");
    }
}
