//! Boundary structs shared across modules.
//!
//! These are a frozen contract: they are what the database rows deserialize
//! into and what every module passes across its boundaries. Adding a field is
//! fine. Renaming or removing one breaks anything reading an existing index,
//! including older binaries.

use serde::{Deserialize, Serialize};

/// A row from the `files` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub mtime: f64,
    /// ISO-8601 UTC.
    pub indexed_at: String,
}

/// A single search hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub path: String,
    pub score: f64,
    #[serde(default)]
    pub snippet: Option<String>,
    /// The indexed file's mtime, as the `files` table stores it.
    #[serde(default)]
    pub mtime: f64,
    /// The Tesseract language string that produced the text, or `None` for a
    /// row written before `files.lang` existed.
    #[serde(default)]
    pub lang: Option<String>,
}

/// Aggregate counts returned by the indexer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexStats {
    pub indexed: u64,
    pub updated: u64,
    pub skipped: u64,
    pub failed: u64,
    pub duration_seconds: f64,
    /// Stale rows pruned; defaulted for back-compat.
    #[serde(default)]
    pub deleted: u64,
}
