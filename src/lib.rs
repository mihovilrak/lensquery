//! LensQuery — offline full-text search over the text inside your images.
//!
//! The binary is a shim over [`cli`]; everything else here is the library the
//! CLI drives. [`indexer`] walks a directory and OCRs what it finds via
//! [`ocr`]/[`tess`], [`db`] owns the SQLite FTS5 index and query building,
//! [`lang`] owns the language packs, [`watch`] drives incremental indexing
//! from filesystem events, and [`models`] holds the row types the rest
//! exchange.

// The per-image `catch_unwind` in `indexer::ocr_one` is what keeps one bad
// image from ending a multi-hour index run, and it is silently inert under
// `panic = "abort"`. A profile or a `-C panic=abort` that would take it away
// fails the build here instead of shipping a binary whose crash isolation
// quietly does nothing. Checked on every platform and every profile, which is
// the only way to know the release profile survived cross-compilation.
#[cfg(panic = "abort")]
compile_error!("LensQuery needs panic = \"unwind\": the per-image catch_unwind in indexer::ocr_one is inert under abort. See [profile.release] in Cargo.toml.");

pub mod cli;
pub mod config;
pub mod db;
pub mod fuzzy;
pub mod indexer;
pub mod lang;
pub mod models;
pub mod ocr;
pub mod tess;
pub mod watch;
