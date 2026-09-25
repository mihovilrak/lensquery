//! SQLite schema, PRAGMAs, upsert, and search.
//!
//! **All SQL lives here.** No other module builds a statement, and every
//! user-supplied string reaches `MATCH` through [`build_fts5_query`] — that is
//! the whole injection story, and it only holds while this stays the single
//! door.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

use crate::fuzzy;
use crate::models::SearchResult;

/// Bumped whenever an existing index stops being readable, which forces a wipe
/// and re-index rather than a silent upgrade that would return wrong results.
///
/// v2 established the invariant the rest of this module depends on: **an fts5
/// row's `rowid` is the owning `files.id`**, so a delete or replace is a B-tree
/// probe instead of a scan over the `UNINDEXED` `file_id` column. That is a
/// property of how rows are written, not something the schema can enforce, so
/// it cannot be detected at runtime — it has to be a version gate.
pub const SCHEMA_VERSION: i64 = 2;

const PRAGMAS: &str = "\
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = -64000;
PRAGMA foreign_keys = ON;";

const SCHEMA_SQL: [&str; 7] = [
    "CREATE TABLE IF NOT EXISTS files (
        id INTEGER PRIMARY KEY,
        path TEXT UNIQUE NOT NULL,
        mtime REAL NOT NULL,
        indexed_at TEXT NOT NULL,
        lang TEXT
    )",
    "CREATE VIRTUAL TABLE IF NOT EXISTS ocr_text USING fts5(
        file_id UNINDEXED,
        content,
        tokenize = 'porter unicode61'
    )",
    "CREATE VIRTUAL TABLE IF NOT EXISTS ocr_text_trigram USING fts5(
        file_id UNINDEXED,
        content,
        tokenize = 'trigram'
    )",
    // The term dictionary `--fuzzy` reads to find near-misses. `fts5vocab` is
    // a view over an index that already exists rather than a second copy of
    // it: no rows are written here and nothing has to be rebuilt, which is
    // also why adding it costs no `SCHEMA_VERSION` bump — an index built by an
    // earlier version gains it on the next `connect`.
    "CREATE VIRTUAL TABLE IF NOT EXISTS ocr_vocab USING fts5vocab(ocr_text, 'row')",
    "CREATE TABLE IF NOT EXISTS meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    )",
    // `files.path` is declared UNIQUE, which already gives SQLite an implicit
    // index (`sqlite_autoindex_files_1`) that serves every lookup an explicit
    // `idx_files_path` would. Carrying both meant a second B-tree updated on
    // every insert for no read benefit. `DROP` rather than merely not creating
    // it, so databases built by earlier versions stop paying too — this removes
    // a redundant index, not a constraint, so no data is lost, no query changes
    // plan for the worse, and no `SCHEMA_VERSION` bump is warranted.
    "DROP INDEX IF EXISTS idx_files_path",
    "CREATE INDEX IF NOT EXISTS idx_files_mtime ON files(mtime)",
];

#[derive(Debug, Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("DB schema v{found} found, expected v{expected}. Delete {path} and re-run lq index.")]
    SchemaMismatch {
        found: i64,
        expected: i64,
        path: String,
    },
    /// The indexer's single writer thread unwound. Everything it committed
    /// before that point is durable (one transaction per batch), so a re-run
    /// resumes; the run as a whole is still a failure and must not report
    /// success. Distinct from `Sqlite` because no SQL call returned an error.
    #[error("indexer writer thread panicked; the run is incomplete — re-run to resume")]
    WriterPanic,
}

/// Aggregate metrics for `lq status` / `lq doctor`.
#[derive(Debug, Clone, PartialEq)]
pub struct DbStats {
    pub file_count: i64,
    pub db_bytes: i64,
    pub last_indexed_at: Option<String>,
    pub schema_version: Option<i64>,
}

/// Open the DB, apply PRAGMAs, ensure schema, return the connection.
pub fn connect(db_path: &Path) -> Result<Connection, DbError> {
    let is_memory = db_path.to_str() == Some(":memory:");
    if !is_memory {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    DbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
                })?;
            }
        }
    }

    let conn = if is_memory {
        Connection::open_in_memory()?
    } else {
        Connection::open(db_path)?
    };

    conn.execute_batch(PRAGMAS)?;
    init_schema(&conn)?;
    check_schema_version(&conn, db_path)?;
    Ok(conn)
}

/// Create tables and indexes if missing. Idempotent.
pub fn init_schema(conn: &Connection) -> Result<(), DbError> {
    let tx = conn.unchecked_transaction()?;
    for stmt in SCHEMA_SQL {
        tx.execute_batch(stmt)?;
    }
    add_missing_columns(&tx)?;
    // Only stamps schema_version on first init; updates are not allowed.
    tx.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Add columns that `CREATE TABLE IF NOT EXISTS` cannot add to a table which
/// already exists.
///
/// Purely additive, and therefore version-neutral in both directions: an older
/// binary never names `lang` in a statement, and a newer one reads NULL as
/// "indexed before the column existed". That is why this is not a
/// `SCHEMA_VERSION` bump — a bump would refuse to open every index already on
/// disk to gain a column nothing needs in order to be correct.
fn add_missing_columns(conn: &Connection) -> Result<(), DbError> {
    let has_lang: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('files') WHERE name = 'lang'")?
        .exists([])?;
    if !has_lang {
        conn.execute_batch("ALTER TABLE files ADD COLUMN lang TEXT")?;
    }
    Ok(())
}

fn check_schema_version(conn: &Connection, db_path: &Path) -> Result<(), DbError> {
    let found: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(found) = found else {
        return Ok(());
    };
    let found: i64 = found.parse().unwrap_or(-1);
    if found != SCHEMA_VERSION {
        return Err(DbError::SchemaMismatch {
            found,
            expected: SCHEMA_VERSION,
            path: db_path.display().to_string(),
        });
    }
    Ok(())
}

/// Insert or update a file row and refresh both FTS5 entries.
///
/// The caller wraps this in a transaction (one transaction per batch).
///
/// # Why the fts5 rows carry an explicit rowid
///
/// `file_id` is an `UNINDEXED` fts5 column, so `DELETE FROM ocr_text WHERE
/// file_id = ?` has no index to use — `EXPLAIN QUERY PLAN` reports `SCAN
/// ocr_text VIRTUAL TABLE`. The cost of that scan grows with the corpus, which
/// made the delete-then-insert pair quadratic in the number of indexed images:
/// measured at 2.9 / 7.0 / 12.1 / 22.7 ms per upsert as the table grew through
/// 2k / 4k / 8k / 16k rows, and at 25.166 ms/image against the real 19,839-row
/// corpus — 54.9× a new-path write, or 1.221 µs per existing row.
///
/// Writing the row as `rowid = file_id` makes both deletes rowid lookups in the
/// fts5 `%_content` B-tree, which is O(log n) and independent of how the row was
/// found. The `INSERT OR REPLACE` form then covers the new-path case for free:
/// no row to replace means no work, so the pre-4.6b existence check against
/// `files.path` is no longer needed to keep a cold index off the scan path.
///
/// This is what `SCHEMA_VERSION` 2 buys. A v1 database's fts5 rows have
/// autoincrement rowids unrelated to `file_id`, so this code would insert
/// alongside them rather than replace them; `check_schema_version` refuses to
/// open one.
pub fn upsert_file(
    conn: &Connection,
    path: &str,
    mtime: f64,
    indexed_at: &str,
    text: &str,
    lang: Option<&str>,
) -> Result<i64, DbError> {
    let file_id: i64 = conn
        .prepare_cached(
            "INSERT INTO files(path, mtime, indexed_at, lang) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(path) DO UPDATE SET \
             mtime=excluded.mtime, indexed_at=excluded.indexed_at, lang=excluded.lang \
             RETURNING id",
        )?
        .query_row(rusqlite::params![path, mtime, indexed_at, lang], |row| {
            row.get(0)
        })?;

    // FTS5 has no ON CONFLICT, but it does honour INSERT OR REPLACE on the
    // rowid, which is the delete-then-insert pair as one rowid-addressed
    // statement.
    conn.prepare_cached(
        "INSERT OR REPLACE INTO ocr_text(rowid, file_id, content) VALUES (?1, ?1, ?2)",
    )?
    .execute(rusqlite::params![file_id, text])?;
    conn.prepare_cached(
        "INSERT OR REPLACE INTO ocr_text_trigram(rowid, file_id, content) VALUES (?1, ?1, ?2)",
    )?
    .execute(rusqlite::params![file_id, text])?;
    Ok(file_id)
}

/// Delete file rows and their FTS5 entries; return the count removed.
///
/// Every statement goes through `prepare_cached`: `Connection::execute` and
/// `query_row` compile the SQL afresh on each call, which in a loop over a
/// caller-supplied path list means one parse per path per statement. The cache
/// is keyed on the SQL text, so the compile happens once for the whole batch.
///
/// The two FTS deletes address the row by `rowid` rather than by the
/// `UNINDEXED` `file_id` column, so each is a B-tree probe instead of a full
/// virtual-table scan: O(paths × log corpus) instead of O(paths × corpus). That
/// relies on the `rowid == files.id` invariant established by
/// [`SCHEMA_VERSION`] and maintained by [`upsert_file`] — deleting by `file_id`
/// here would still be correct, just quadratic on a large index.
pub fn delete_files(conn: &Connection, paths: &[String]) -> Result<u64, DbError> {
    let tx = conn.unchecked_transaction()?;
    let mut deleted = 0u64;
    for path in paths {
        let file_id: Option<i64> = tx
            .prepare_cached("DELETE FROM files WHERE path = ?1 RETURNING id")?
            .query_row([path], |row| row.get(0))
            .optional()?;
        let Some(file_id) = file_id else {
            continue;
        };
        tx.prepare_cached("DELETE FROM ocr_text WHERE rowid = ?1")?
            .execute([file_id])?;
        tx.prepare_cached("DELETE FROM ocr_text_trigram WHERE rowid = ?1")?
            .execute([file_id])?;
        deleted += 1;
    }
    tx.commit()?;
    Ok(deleted)
}

/// Point an indexed row at a new path; `false` if `old` was not indexed.
///
/// A moved file is the same file: the OCR text and both FTS rows are keyed on
/// `files.id`, so re-pointing the path costs one `UPDATE` and saves re-reading
/// the image. `lq watch` uses this; a full `lq index` run cannot tell a move
/// from a delete-plus-create and does not try.
///
/// Any row already sitting at `new` is deleted first — `files.path` is UNIQUE,
/// and on disk the move has already overwritten whatever was there.
pub fn rename_file(conn: &Connection, old: &str, new: &str) -> Result<bool, DbError> {
    let tx = conn.unchecked_transaction()?;
    // Inlined rather than delegated to `delete_files`: that opens its own
    // transaction, and SQLite has no nesting — its commit would close this one.
    let clobbered: Option<i64> = tx
        .prepare_cached("DELETE FROM files WHERE path = ?1 RETURNING id")?
        .query_row([new], |row| row.get(0))
        .optional()?;
    if let Some(file_id) = clobbered {
        tx.prepare_cached("DELETE FROM ocr_text WHERE rowid = ?1")?
            .execute([file_id])?;
        tx.prepare_cached("DELETE FROM ocr_text_trigram WHERE rowid = ?1")?
            .execute([file_id])?;
    }
    let updated = tx
        .prepare_cached("UPDATE files SET path = ?2 WHERE path = ?1")?
        .execute([old, new])?;
    tx.commit()?;
    Ok(updated > 0)
}

/// Return the stored mtime for `path`, or `None` if unknown.
pub fn get_file_mtime(conn: &Connection, path: &str) -> Result<Option<f64>, DbError> {
    let mtime: Option<f64> = conn
        .query_row("SELECT mtime FROM files WHERE path = ?1", [path], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(mtime)
}

/// Snippet window, in tokens, for the porter table.
///
/// `snippet()` budgets in *tokens*, and the two tables tokenize differently. On
/// `ocr_text` a token is a word, so 12 is about a line of prose.
const SNIPPET_TOKENS_PORTER: i64 = 12;

/// Snippet window, in tokens, for the trigram table.
///
/// A trigram token is a single character, so the porter budget of 12 would
/// return a 12-*character* stub here — the trap that kept `snippet()` out of
/// the trigram path in the first place. 64 is SQLite's hard ceiling for this
/// argument and yields a comparable ~60-character excerpt.
const SNIPPET_TOKENS_TRIGRAM: i64 = 64;

/// Porter-stemmed full-text search over OCR content.
///
/// `SearchResult::snippet` is `None`; use [`search_standard_snippet`] for the
/// excerpt-bearing variant.
pub fn search_standard(
    conn: &Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<SearchResult>, DbError> {
    search(conn, "ocr_text", &build_fts5_query(query), limit, None)
}

/// Trigram substring search over OCR content.
///
/// Matches any run of characters anywhere inside a word — `nvoi` finds
/// `invoice` — which is a different question from the one [`search_fuzzy`]
/// answers. Substring search cannot tolerate a wrong character in the middle
/// of the query, and fuzzy search cannot find a fragment that starts
/// mid-word, so both are kept.
pub fn search_substring(
    conn: &Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<SearchResult>, DbError> {
    search(
        conn,
        "ocr_text_trigram",
        &build_fts5_query(query),
        limit,
        None,
    )
}

/// [`search_standard`] with `SearchResult::snippet` populated.
///
/// A second name rather than a `snippet: bool` argument: Rust has no default
/// arguments, and most callers want the cheap form and would otherwise all
/// carry a bare `false` at the call site.
pub fn search_standard_snippet(
    conn: &Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<SearchResult>, DbError> {
    search(
        conn,
        "ocr_text",
        &build_fts5_query(query),
        limit,
        Some(SNIPPET_TOKENS_PORTER),
    )
}

/// [`search_substring`] with `SearchResult::snippet` populated.
pub fn search_substring_snippet(
    conn: &Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<SearchResult>, DbError> {
    search(
        conn,
        "ocr_text_trigram",
        &build_fts5_query(query),
        limit,
        Some(SNIPPET_TOKENS_TRIGRAM),
    )
}

/// Typo-tolerant full-text search over OCR content.
///
/// Every word of the query is looked up in the index's own term dictionary and
/// widened to the words within an edit or two of it, so `invoce` finds
/// `invoice` and `1nvo1ce` does too. `max_distance` overrides the
/// length-scaled default from [`fuzzy::budget`]; `None` uses it.
///
/// Exact matches come first, in their own rank order, and approximate ones
/// follow — a document that really contains the word the user typed should
/// never rank below one that contains something merely similar, and FTS5's
/// `rank` has no way to express that inside a single `OR` query.
pub fn search_fuzzy(
    conn: &Connection,
    query: &str,
    limit: i64,
    max_distance: Option<u32>,
) -> Result<Vec<SearchResult>, DbError> {
    search_fuzzy_inner(conn, query, limit, max_distance, None)
}

/// [`search_fuzzy`] with `SearchResult::snippet` populated.
pub fn search_fuzzy_snippet(
    conn: &Connection,
    query: &str,
    limit: i64,
    max_distance: Option<u32>,
) -> Result<Vec<SearchResult>, DbError> {
    search_fuzzy_inner(
        conn,
        query,
        limit,
        max_distance,
        Some(SNIPPET_TOKENS_PORTER),
    )
}

fn search_fuzzy_inner(
    conn: &Connection,
    query: &str,
    limit: i64,
    max_distance: Option<u32>,
    snippet_tokens: Option<i64>,
) -> Result<Vec<SearchResult>, DbError> {
    let sanitized = build_fts5_query(query);
    let mut out = search(conn, "ocr_text", &sanitized, limit, snippet_tokens)?;
    if out.len() as i64 >= limit {
        return Ok(out);
    }
    let Some(widened) = expand_query(conn, &sanitized, max_distance)? else {
        return Ok(out);
    };
    let seen: HashSet<String> = out.iter().map(|r| r.path.clone()).collect();
    for r in search(conn, "ocr_text", &widened, limit, snippet_tokens)? {
        if out.len() as i64 >= limit {
            break;
        }
        if !seen.contains(&r.path) {
            out.push(r);
        }
    }
    Ok(out)
}

/// Most near-misses one query word is allowed to contribute.
///
/// A common short word can have hundreds of neighbours in a large index, and
/// an `OR` group that wide stops being a search for what the user asked for.
/// The cap keeps the nearest ones, which are the only ones worth having.
const MAX_EXPANSIONS_PER_TERM: usize = 16;

/// Widen a sanitized FTS5 query with near-misses from the term dictionary.
///
/// Returns `None` when nothing was added, so the caller can skip a second
/// query that would return exactly what it already has.
///
/// Two shapes are passed over untouched. A query containing a quote is a
/// phrase query, which the user wrote to be taken literally. A token ending in
/// `*` is already a prefix search — a deliberate widening, and combining it
/// with another one produces matches nobody asked for.
fn expand_query(
    conn: &Connection,
    sanitized: &str,
    max_distance: Option<u32>,
) -> Result<Option<String>, DbError> {
    if sanitized.contains('"') {
        return Ok(None);
    }
    let mut widened = Vec::new();
    let mut grew = false;
    for token in sanitized.split_whitespace() {
        let expansions = if token.ends_with('*') {
            Vec::new()
        } else {
            expand_term(conn, token, max_distance)?
        };
        if expansions.is_empty() {
            widened.push(token.to_string());
        } else {
            grew = true;
            let group = std::iter::once(token.to_string())
                .chain(expansions)
                .collect::<Vec<_>>()
                .join(" OR ");
            widened.push(format!("({group})"));
        }
    }
    // Bare terms side by side are an implicit AND in FTS5, but a parenthesised
    // group is not a term, and juxtaposing two of them is a syntax error. The
    // operator has to be spelled out — which is the same query, just written
    // the way the grammar wants it.
    Ok(grew.then(|| widened.join(" AND ")))
}

/// The index's own words that are within an edit budget of `term`.
///
/// Nearest first, then most common first — when the budget admits more
/// neighbours than [`MAX_EXPANSIONS_PER_TERM`] allows, the ones that actually
/// appear in the corpus are the better guess at what was meant.
fn expand_term(
    conn: &Connection,
    term: &str,
    max_distance: Option<u32>,
) -> Result<Vec<String>, DbError> {
    let budget = max_distance
        .unwrap_or_else(|| fuzzy::budget(term.chars().count()))
        .min(fuzzy::MAX_DISTANCE);
    if budget == 0 {
        return Ok(Vec::new());
    }
    // A candidate too far from the query in length alone cannot be within
    // `budget` edits of it, so SQLite discards it before it ever reaches the
    // distance function. The window is lopsided because the vocabulary holds
    // stems: a candidate may be `STEM_SUFFIX` characters shorter than the word
    // the user typed without a single edit being wrong. `length()` counts
    // characters on TEXT, which is what the comparison is measured in too.
    let len = term.chars().count() as i64;
    let lo = (len - budget as i64 - fuzzy::STEM_SUFFIX as i64).max(0);
    let hi = len + budget as i64;
    let mut stmt =
        conn.prepare("SELECT term, cnt FROM ocr_vocab WHERE length(term) BETWEEN ?1 AND ?2")?;
    let rows = stmt.query_map(rusqlite::params![lo, hi], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    let lowered = term.to_lowercase();
    let mut hits: Vec<(u32, i64, String)> = Vec::new();
    for row in rows {
        let (candidate, cnt) = row?;
        // The term is already in the query as itself; and anything the
        // sanitizer would have to quote must not be spliced into an `OR`
        // group as syntax. Vocabulary terms come from the tokenizer and are
        // alphanumeric, so this rejects nothing in practice — it is here so
        // that a future tokenizer change cannot turn index content into query
        // operators.
        if candidate.to_lowercase() == lowered || !is_bareword(&candidate) {
            continue;
        }
        if let Some(d) = fuzzy::stem_distance_within(term, &candidate, budget) {
            hits.push((d, -cnt, candidate));
        }
    }
    hits.sort_unstable();
    hits.truncate(MAX_EXPANSIONS_PER_TERM);
    Ok(hits.into_iter().map(|(_, _, t)| t).collect())
}

/// Run one already-sanitized FTS5 query.
///
/// `match_expr` goes to `MATCH` as written: every caller is responsible for
/// having put its input through [`build_fts5_query`] first. That is the whole
/// reason this function is private — the sanitizer is not optional, and
/// keeping the two steps separate is what lets `--fuzzy` rewrite a query
/// *after* it has been sanitized rather than having to sanitize its own
/// generated syntax back into text.
fn search(
    conn: &Connection,
    table: &str,
    match_expr: &str,
    limit: i64,
    snippet_tokens: Option<i64>,
) -> Result<Vec<SearchResult>, DbError> {
    // `table` is one of two hard-coded literals above — never user input, and
    // so is `n`, which is why both interpolate instead of binding.
    let excerpt = match snippet_tokens {
        // `snippet()` wants the FTS5 table by *name*: passing the `t` alias is
        // a "no such column: t" error. Column 1 is `content` — column 0 is the
        // UNINDEXED `file_id`, and asking for 0 silently snippets the rowid.
        Some(n) => format!("snippet({table}, 1, '[', ']', '...', {n})"),
        None => "NULL".to_string(),
    };
    let sql = format!(
        "SELECT f.path, t.rank, {excerpt}, f.mtime, f.lang FROM {table} t \
         JOIN files f ON t.file_id = f.id \
         WHERE {table} MATCH ?1 ORDER BY t.rank LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![match_expr, limit], |row| {
        Ok(SearchResult {
            path: row.get::<_, String>(0)?,
            score: row.get::<_, f64>(1)?,
            snippet: row
                .get::<_, Option<String>>(2)?
                .map(|s| collapse_whitespace(&s)),
            mtime: row.get::<_, f64>(3)?,
            lang: row.get::<_, Option<String>>(4)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Fold every run of whitespace into a single space.
///
/// OCR content is mostly newlines; a raw snippet would carry them straight into
/// stdout and break the one-result-per-line contract that `lq search` and `lq
/// serve` both rest on. Collapsing here rather than at the print site keeps the
/// two cores and the two commands from each inventing their own version.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Aggregate metrics for `lq status` / `lq doctor`.
pub fn stats(conn: &Connection) -> Result<DbStats, DbError> {
    let file_count: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let last_indexed_at: Option<String> =
        conn.query_row("SELECT MAX(indexed_at) FROM files", [], |r| r.get(0))?;
    let schema_version: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|s| s.parse().ok());
    Ok(DbStats {
        file_count,
        db_bytes: page_count * page_size,
        last_indexed_at,
        schema_version,
    })
}

/// Byte size of the database before and after [`compact`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactStats {
    pub before_bytes: i64,
    pub after_bytes: i64,
}

impl CompactStats {
    /// Bytes reclaimed (never negative in practice, but VACUUM is allowed to
    /// grow a database that was already tighter than its rebuilt form).
    pub fn reclaimed(&self) -> i64 {
        self.before_bytes - self.after_bytes
    }
}

/// Run `PRAGMA optimize` then `VACUUM`, returning the size on either side.
///
/// This is a **space** operation, not a speed one, and the yield tracks how
/// churned the file is: 9.78% back on the long-lived 19,839-image index, but
/// only 3.00% on a freshly built one, which has few free pages to reclaim.
/// Query latency does not move either way — the differences are inside the
/// run-to-run noise of a spawn-dominated search. Sell it as
/// reclaiming disk, and do not be tempted to bolt it onto the tail of
/// `lq index` — VACUUM rewrites the whole database, so a 20k-image index would
/// pay a full copy of a multi-hundred-MB file for a saving the user did not
/// ask for and cannot skip.
///
/// `VACUUM` cannot run inside a transaction, so this deliberately executes on
/// the bare connection. `PRAGMA optimize` goes first: it updates the stat
/// tables the query planner reads, and doing it before the rebuild means the
/// analysis runs against the pages that are about to be written out.
pub fn compact(conn: &Connection) -> Result<CompactStats, DbError> {
    let size = |c: &Connection| -> Result<i64, DbError> {
        let page_count: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok(page_count * page_size)
    };
    let before_bytes = size(conn)?;
    conn.execute_batch("PRAGMA optimize")?;
    conn.execute_batch("VACUUM")?;
    let after_bytes = size(conn)?;
    Ok(CompactStats {
        before_bytes,
        after_bytes,
    })
}

/// Sanitize input for FTS5 MATCH; pass through explicit phrases.
///
/// Balanced quotes = intentional phrase query, passed through. An unbalanced
/// quote would crash the FTS5 parser — strip and sanitize like any other input.
/// FTS5 barewords allow only alphanumerics, so every token containing any
/// non-alphanumeric character is quoted — except a trailing `*`, see
/// `is_bareword`.
pub fn build_fts5_query(raw: &str) -> String {
    if raw.contains('"') && raw.matches('"').count().is_multiple_of(2) {
        return raw.to_string();
    }
    // Unbalanced (or no) quotes: `replace` is a no-op when none are present.
    let cleaned = raw.replace('"', " ");
    cleaned
        .split_whitespace()
        .map(|t| {
            if is_bareword(t) {
                t.to_string()
            } else {
                format!("\"{t}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// True if `token` can go to MATCH unquoted.
///
/// A trailing `*` is FTS5's prefix operator and the one non-alphanumeric
/// character worth letting through: quoting it makes the token a phrase, the
/// tokenizer drops the `*`, and the prefix search silently becomes an
/// exact-term search — returning nothing at all whenever the stem is not itself
/// a word. The rest of the token must still be entirely alphanumeric, so this
/// cannot open a quote, start a column filter, or introduce a boolean operator.
///
/// This is the allowlist half of the sanitizer, so widen it only with a test:
/// anything that gets through here reaches the FTS5 query parser as syntax
/// rather than as text.
fn is_bareword(token: &str) -> bool {
    let Some(stem) = token.strip_suffix('*') else {
        return token.chars().all(char::is_alphanumeric);
    };
    !stem.is_empty() && stem.chars().all(char::is_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-05-16T10:00:00Z";

    fn conn() -> Connection {
        connect(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn fts5_porter_and_trigram_both_compile() {
        // Both tokenizers ship with the bundled SQLite, but a build that
        // dropped either would fail here rather than at a user's first search.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE VIRTUAL TABLE p USING fts5(x, tokenize='porter unicode61');
             CREATE VIRTUAL TABLE t USING fts5(x, tokenize='trigram');",
        )
        .expect("bundled SQLite must have FTS5 with porter+trigram");
    }

    #[test]
    fn init_schema_is_idempotent() {
        let c = conn();
        init_schema(&c).unwrap();
        init_schema(&c).unwrap();
        let v: String = c
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "2");
    }

    #[test]
    fn upsert_insert_then_update_same_id() {
        let c = conn();
        let fid1 = upsert_file(&c, "/tmp/x.png", 1.0, NOW, "hello", None).unwrap();
        let fid2 = upsert_file(&c, "/tmp/x.png", 2.0, NOW, "world", None).unwrap();
        assert_eq!(fid1, fid2);
        let mtime: f64 = c
            .query_row(
                "SELECT mtime FROM files WHERE path = '/tmp/x.png'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mtime, 2.0);
        let content: String = c
            .query_row(
                "SELECT content FROM ocr_text WHERE file_id = ?1",
                [fid1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(content, "world");
    }

    #[test]
    fn reindex_replaces_fts_rows_instead_of_duplicating_them() {
        // Post-4.6b the refresh is a single `INSERT OR REPLACE` keyed on the
        // rowid, so re-indexing must leave one row per table and the stale text
        // must be unsearchable. `upsert_insert_then_update_same_id` cannot catch
        // a regression here: `query_row` returns the first of two rows and
        // reports success.
        let c = conn();
        let fid = upsert_file(&c, "/tmp/z.png", 1.0, NOW, "alpha", None).unwrap();
        upsert_file(&c, "/tmp/z.png", 2.0, NOW, "beta", None).unwrap();

        for table in ["ocr_text", "ocr_text_trigram"] {
            let rows: i64 = c
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE file_id = ?1"),
                    [fid],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(rows, 1, "{table} kept a stale row");
            // The schema-v2 invariant the deletes depend on. If this drifts,
            // deletes silently stop matching and stale rows accumulate.
            let rowid: i64 = c
                .query_row(
                    &format!("SELECT rowid FROM {table} WHERE file_id = ?1"),
                    [fid],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(rowid, fid, "{table} rowid must equal files.id");
        }
        let stale: i64 = c
            .query_row(
                "SELECT count(*) FROM ocr_text WHERE ocr_text MATCH 'alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0, "replaced text is still searchable");
    }

    #[test]
    fn get_file_mtime_known_and_unknown() {
        let c = conn();
        assert_eq!(get_file_mtime(&c, "/tmp/y.png").unwrap(), None);
        upsert_file(&c, "/tmp/y.png", 42.0, NOW, "", None).unwrap();
        assert_eq!(get_file_mtime(&c, "/tmp/y.png").unwrap(), Some(42.0));
    }

    #[test]
    fn search_standard_empty_on_no_match() {
        let c = conn();
        assert!(search_standard(&c, "anything", 10).unwrap().is_empty());
    }

    #[test]
    fn search_standard_finds_token() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "the quick brown fox", None).unwrap();
        let results = search_standard(&c, "quick", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "/x.png");
    }

    #[test]
    fn substring_finds_an_infix_standard_misses() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "invoice", None).unwrap();
        assert!(search_standard(&c, "nvoi", 10).unwrap().is_empty());
        assert_eq!(search_substring(&c, "nvoi", 10).unwrap().len(), 1);
    }

    // --- fuzzy: what the vocabulary walk does and does not widen ---

    #[test]
    fn fuzzy_finds_a_word_the_user_mistyped() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "invoice total", None).unwrap();
        assert!(search_standard(&c, "invoce", 10).unwrap().is_empty());
        assert_eq!(search_fuzzy(&c, "invoce", 10, None).unwrap().len(), 1);
    }

    #[test]
    fn fuzzy_finds_a_word_the_scanner_misread() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "1nvo1ce", None).unwrap();
        assert_eq!(search_fuzzy(&c, "invoice", 10, None).unwrap().len(), 1);
    }

    /// Exact hits come first even though the widened pass runs second and FTS5
    /// ranks each pass on its own.
    #[test]
    fn fuzzy_puts_the_exact_match_above_the_correction() {
        let c = conn();
        upsert_file(&c, "/near.png", 1.0, NOW, "invoce", None).unwrap();
        upsert_file(&c, "/exact.png", 2.0, NOW, "invoice", None).unwrap();
        let r = search_fuzzy(&c, "invoice", 10, None).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].path, "/exact.png");
    }

    #[test]
    fn fuzzy_respects_the_limit_across_both_passes() {
        let c = conn();
        upsert_file(&c, "/a.png", 1.0, NOW, "invoice", None).unwrap();
        upsert_file(&c, "/b.png", 2.0, NOW, "invoce", None).unwrap();
        upsert_file(&c, "/c.png", 3.0, NOW, "invoiice", None).unwrap();
        assert_eq!(search_fuzzy(&c, "invoice", 1, None).unwrap().len(), 1);
    }

    #[test]
    fn fuzzy_never_returns_the_same_file_twice() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "invoice invoce", None).unwrap();
        assert_eq!(search_fuzzy(&c, "invoice", 10, None).unwrap().len(), 1);
    }

    #[test]
    fn a_zero_distance_is_a_standard_search() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "invoice", None).unwrap();
        assert!(search_fuzzy(&c, "invoce", 10, Some(0)).unwrap().is_empty());
    }

    /// A quoted phrase is the user saying "these words, in this order". Widening
    /// it would answer a question nobody asked.
    #[test]
    fn a_phrase_query_is_taken_literally() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "invoice total", None).unwrap();
        assert!(search_fuzzy(&c, "\"invoce total\"", 10, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn fuzzy_widens_every_word_of_a_multi_word_query() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "invoice balance", None).unwrap();
        assert_eq!(
            search_fuzzy(&c, "invoce balence", 10, None).unwrap().len(),
            1
        );
    }

    /// Widening turns each word into a parenthesised group, and FTS5 will not
    /// let two groups sit side by side the way two bare terms can. Spelling the
    /// operator out has to keep the meaning bare juxtaposition already had.
    #[test]
    fn widening_a_multi_word_query_still_requires_every_word() {
        let c = conn();
        upsert_file(&c, "/both.png", 1.0, NOW, "invoice balance", None).unwrap();
        upsert_file(&c, "/one.png", 2.0, NOW, "invoice only", None).unwrap();
        let r = search_fuzzy(&c, "invoce balence", 10, None).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path, "/both.png");
    }

    /// The expansion runs on the sanitized text, and every candidate it pulls
    /// out of the vocabulary is re-checked before it goes near `MATCH`.
    #[test]
    fn fuzzy_expansion_cannot_smuggle_syntax_into_match() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "invoice OR NEAR total", None).unwrap();
        assert!(search_fuzzy(&c, "invoce OR (near*", 10, None).is_ok());
        assert!(search_fuzzy(&c, "a\"b=c", 10, None).is_ok());
    }

    #[test]
    fn fuzzy_falls_back_to_the_standard_result_when_nothing_is_near() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "invoice", None).unwrap();
        assert!(search_fuzzy(&c, "zzzzzzzz", 10, None).unwrap().is_empty());
        assert_eq!(search_fuzzy(&c, "invoice", 10, None).unwrap().len(), 1);
    }

    #[test]
    fn the_snippet_variant_widens_the_same_way() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, LONG, None).unwrap();
        let r = search_fuzzy_snippet(&c, "invoce", 10, None).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].snippet.is_some());
    }

    /// A paragraph long enough that a 12-token window is a *window* and not the
    /// whole string — otherwise the snippet tests pass on a table that ignores
    /// the token budget entirely.
    const LONG: &str = "alpha bravo charlie delta echo foxtrot golf hotel india \
                        juliett kilo lima the invoice total is due mike november \
                        oscar papa quebec romeo sierra tango uniform victor";

    #[test]
    fn search_without_snippet_leaves_the_field_none() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, LONG, None).unwrap();
        assert_eq!(search_standard(&c, "invoice", 10).unwrap()[0].snippet, None);
        assert_eq!(search_substring(&c, "nvoic", 10).unwrap()[0].snippet, None);
    }

    #[test]
    fn snippet_brackets_the_match_and_stays_inside_the_window() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, LONG, None).unwrap();
        let s = search_standard_snippet(&c, "invoice", 10).unwrap()[0]
            .snippet
            .clone()
            .unwrap();
        assert!(s.contains("[invoice]"), "match not bracketed: {s:?}");
        assert!(!s.contains("alpha"), "window ignored, got the lot: {s:?}");
        assert!(s.contains("..."), "an elided window must say so: {s:?}");
    }

    /// The trap that kept `snippet()` out of the trigram path: its tokens are
    /// characters, so the porter budget of 12 would return a 12-character stub.
    /// This pins the separate budget by measuring what the user would see.
    ///
    /// Note the second assertion: because a trigram token is a character, the
    /// brackets land on the matched *substring* rather than on the word that
    /// contains it — `i[nvoic]e`, not `[invoice]`. That is the tokenizer being
    /// honest about what matched, and it is why the substring arm gets its own
    /// expectation instead of sharing the porter one.
    #[test]
    fn trigram_snippet_is_not_a_twelve_character_stub() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, LONG, None).unwrap();
        let s = search_substring_snippet(&c, "nvoic", 10).unwrap()[0]
            .snippet
            .clone()
            .unwrap();
        assert!(s.contains("i[nvoic]e"), "match not bracketed: {s:?}");
        assert!(
            s.chars().count() > 40,
            "trigram window is char-sized, got {} chars: {s:?}",
            s.chars().count()
        );
    }

    /// Column 0 is the UNINDEXED `file_id`; snippetting it would return the id
    /// (or nothing) instead of text. Cheapest possible guard against the index
    /// silently going back to 0.
    #[test]
    fn snippet_reads_the_content_column_not_the_file_id() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, LONG, None).unwrap();
        let s = search_standard_snippet(&c, "invoice", 10).unwrap()[0]
            .snippet
            .clone()
            .unwrap();
        assert!(s.contains("total"), "not the content column: {s:?}");
    }

    #[test]
    fn snippet_is_single_line_even_when_the_ocr_text_is_not() {
        let c = conn();
        upsert_file(
            &c,
            "/x.png",
            1.0,
            NOW,
            "header\n\ninvoice total\n\nfooter",
            None,
        )
        .unwrap();
        let s = search_standard_snippet(&c, "invoice", 10).unwrap()[0]
            .snippet
            .clone()
            .unwrap();
        assert!(!s.contains('\n'), "newline survived: {s:?}");
        assert!(!s.contains('\r'), "carriage return survived: {s:?}");
        assert!(s.contains("[invoice] total"), "text mangled: {s:?}");
    }

    #[test]
    fn snippet_search_still_sanitizes_the_query() {
        // Same `_build_fts5_query` path as the plain search — the snippet
        // variants must not be a second, unguarded way into MATCH.
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, LONG, None).unwrap();
        assert!(search_standard_snippet(&c, "C++ code", 10).is_ok());
        assert!(search_standard_snippet(&c, "foo\"bar", 10).is_ok());
        assert!(search_substring_snippet(&c, "a\"b=c", 10).is_ok());
        assert!(search_fuzzy_snippet(&c, "a\"b=c", 10, None).is_ok());
    }

    #[test]
    fn stats_basic_fields() {
        let c = conn();
        upsert_file(&c, "/a.png", 1.0, NOW, "alpha", None).unwrap();
        upsert_file(&c, "/b.png", 2.0, "2026-05-16T11:00:00Z", "beta", None).unwrap();
        let s = stats(&c).unwrap();
        assert_eq!(s.file_count, 2);
        assert_eq!(s.schema_version, Some(2));
        assert_eq!(s.last_indexed_at.as_deref(), Some("2026-05-16T11:00:00Z"));
        assert!(s.db_bytes > 0);
    }

    #[test]
    fn delete_files_removes_rows_and_fts_entries() {
        let c = conn();
        upsert_file(&c, "/a.png", 1.0, NOW, "alpha", None).unwrap();
        upsert_file(&c, "/b.png", 2.0, NOW, "beta", None).unwrap();
        let deleted = delete_files(&c, &["/a.png".to_string()]).unwrap();
        assert_eq!(deleted, 1);
        assert!(search_standard(&c, "alpha", 10).unwrap().is_empty());
        assert_eq!(search_standard(&c, "beta", 10).unwrap().len(), 1);
        assert_eq!(search_substring(&c, "alph", 10).unwrap().len(), 0);
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn compact_reclaims_space_freed_by_deletes() {
        let c = conn();
        // Enough rows, with enough text each, that deleting most of them
        // leaves whole free pages behind rather than slack inside live ones.
        let text = "alpha beta gamma delta epsilon zeta eta theta ".repeat(20);
        for i in 0..200 {
            upsert_file(&c, &format!("/f{i}.png"), i as f64, NOW, &text, None).unwrap();
        }
        let doomed: Vec<String> = (0..150).map(|i| format!("/f{i}.png")).collect();
        delete_files(&c, &doomed).unwrap();

        let s = compact(&c).unwrap();
        assert!(
            s.after_bytes < s.before_bytes,
            "compact should shrink a database with 150 rows' worth of free pages \
             (before {} after {})",
            s.before_bytes,
            s.after_bytes
        );
        assert_eq!(s.reclaimed(), s.before_bytes - s.after_bytes);
        // `stats` reads the same page_count * page_size, so the two verbs must
        // agree once the rebuild is done.
        assert_eq!(stats(&c).unwrap().db_bytes, s.after_bytes);
    }

    #[test]
    fn compact_preserves_rows_and_both_indexes() {
        let c = conn();
        upsert_file(&c, "/a.png", 1.0, NOW, "invoice total", None).unwrap();
        upsert_file(&c, "/b.png", 2.0, NOW, "receipt", None).unwrap();
        delete_files(&c, &["/b.png".to_string()]).unwrap();

        compact(&c).unwrap();

        assert_eq!(stats(&c).unwrap().file_count, 1);
        assert_eq!(search_standard(&c, "invoice", 10).unwrap().len(), 1);
        assert_eq!(search_substring(&c, "nvoi", 10).unwrap().len(), 1);
        // Still writable afterwards — VACUUM must not have left the connection
        // in a transaction or a read-only state.
        upsert_file(&c, "/c.png", 3.0, NOW, "another", None).unwrap();
        assert_eq!(search_standard(&c, "another", 10).unwrap().len(), 1);
    }

    #[test]
    fn delete_files_unknown_path_returns_zero() {
        let c = conn();
        assert_eq!(delete_files(&c, &["/nope.png".to_string()]).unwrap(), 0);
    }

    #[test]
    fn search_does_not_crash_on_special_chars() {
        let c = conn();
        upsert_file(&c, "/x.png", 1.0, NOW, "some C++ code here", None).unwrap();
        assert!(search_standard(&c, "C++ code", 10).is_ok());
        assert!(search_standard(&c, "foo\"bar", 10).is_ok());
        assert!(search_standard(&c, "hello.world", 10).is_ok());
        assert!(search_substring(&c, "a\"b=c", 10).is_ok());
        assert!(search_fuzzy(&c, "a\"b=c", 10, None).is_ok());
    }

    // --- build_fts5_query: what each class of input turns into ---

    #[test]
    fn fts5_passes_explicit_phrase() {
        assert_eq!(build_fts5_query("\"exact phrase\""), "\"exact phrase\"");
    }

    #[test]
    fn fts5_escapes_specials() {
        assert_eq!(build_fts5_query("C++ code"), "\"C++\" code");
        assert_eq!(build_fts5_query("1*2 foo"), "\"1*2\" foo");
        assert_eq!(build_fts5_query("foo (bar)"), "foo \"(bar)\"");
    }

    #[test]
    fn fts5_passthrough_plain_tokens() {
        assert_eq!(build_fts5_query("hello world"), "hello world");
    }

    #[test]
    fn fts5_unbalanced_quote_is_stripped() {
        assert_eq!(build_fts5_query("foo\"bar"), "foo bar");
        assert_eq!(build_fts5_query("\"dangling phrase"), "dangling phrase");
    }

    #[test]
    fn fts5_quotes_any_non_alnum_token() {
        assert_eq!(build_fts5_query("hello.world"), "\"hello.world\"");
        assert_eq!(build_fts5_query("a=b"), "\"a=b\"");
        assert_eq!(build_fts5_query("50% off"), "\"50%\" off");
    }

    #[test]
    fn fts5_keeps_trailing_star_bare() {
        // Quoting it would make the token a phrase and the tokenizer would
        // drop the "*", turning a prefix search into an exact-term search.
        assert_eq!(build_fts5_query("dana*"), "dana*");
        assert_eq!(build_fts5_query("sviđ*"), "sviđ*");
        assert_eq!(build_fts5_query("foo bar*"), "foo bar*");
    }

    #[test]
    fn fts5_star_exemption_is_only_a_trailing_star() {
        assert_eq!(build_fts5_query("da*na*"), "\"da*na*\"");
        assert_eq!(build_fts5_query("*"), "\"*\"");
        assert_eq!(build_fts5_query("a.b*"), "\"a.b*\"");
    }

    #[test]
    fn fts5_prefix_query_matches_more_than_the_bare_stem() {
        let c = conn();
        upsert_file(&c, "/a.png", 1.0, NOW, "zagreb", None).unwrap();
        upsert_file(&c, "/b.png", 1.0, NOW, "zagorje", None).unwrap();
        // "zag" is not a term in the index and porter reduces neither word to
        // it, so before the exemption this query returned nothing at all.
        assert_eq!(search_standard(&c, "zag*", 10).unwrap().len(), 2);
        assert_eq!(search_standard(&c, "zag", 10).unwrap().len(), 0);
    }

    /// A v1 database is exactly the case 4.6b's bump exists to reject: its fts5
    /// rows are not addressed by `file_id`, so v2 code would insert beside them
    /// instead of replacing them.
    #[test]
    fn schema_mismatch_detected() {
        let c = conn();
        c.execute(
            "UPDATE meta SET value = '1' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
        let err = check_schema_version(&c, Path::new("test.db")).unwrap_err();
        assert!(matches!(err, DbError::SchemaMismatch { found: 1, .. }));
    }
}
