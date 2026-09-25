//! TOML configuration loader.
//!
//! Resolution order (CLI flag > config file > hardcoded default) is enforced
//! at the CLI layer; `load()` only handles file-vs-default. The file has two
//! sections, `[index]` and `[search]`, holding the defaults for the flags of
//! the same names.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// English only. Every extra pack in `lang=` costs model-load time and slows
/// recognition, so the default is the one language we can assume is installed;
/// `lq lang add` and `--lang` are how you get the others.
const DEFAULT_LANGUAGES: &str = "eng";
const DEFAULT_WORKERS: u32 = 0;
const DEFAULT_LIMIT: u32 = 20;
const DEFAULT_ENGINE: &str = "auto";
const VALID_ENGINES: [&str; 3] = ["auto", "dll", "cli"];
/// Word-confidence floor applied at index time. Dropping words below 40 lifts
/// plausible precision ~0.82 -> ~0.92 at negligible findability cost.
const DEFAULT_MIN_WORD_CONF: f32 = 40.0;
/// Two-arm "thorough" OCR is opt-in: it roughly doubles OCR cost on images the
/// primary pass reads as near-empty.
const DEFAULT_THOROUGH: bool = false;
/// How near-empty "near-empty" is. Sourced from `ocr` so the config default and
/// the engine default cannot drift apart.
const DEFAULT_THOROUGH_TRIGGER_WORDS: u32 = crate::ocr::DEFAULT_THOROUGH_TRIGGER_WORDS as u32;

/// Raised when the config file is present but malformed or invalid.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Malformed config {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("reading config {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Effective configuration for a CLI invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub default_db: PathBuf,
    pub languages: String,
    pub workers: u32,
    pub default_limit: u32,
    pub engine: String,
    pub min_word_conf: f32,
    pub thorough: bool,
    pub thorough_trigger_words: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_db: default_db_path(),
            languages: DEFAULT_LANGUAGES.to_string(),
            workers: DEFAULT_WORKERS,
            default_limit: DEFAULT_LIMIT,
            engine: DEFAULT_ENGINE.to_string(),
            min_word_conf: DEFAULT_MIN_WORD_CONF,
            thorough: DEFAULT_THOROUGH,
            thorough_trigger_words: DEFAULT_THOROUGH_TRIGGER_WORDS,
        }
    }
}

fn default_db_path() -> PathBuf {
    expanduser("~/.lensquery/index.db")
}

/// Where [`load`] looks when no path is given.
pub fn default_path() -> PathBuf {
    expanduser("~/.lensquery/config.toml")
}

/// Expand a leading `~` to the user's home directory (mirrors
/// `pathlib.Path.expanduser`).
pub fn expanduser(p: &str) -> PathBuf {
    if p == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

/// The file's shape, as written on disk. Every field is `Option` so "absent"
/// stays distinguishable from "present and equal to the default" — the defaults
/// are applied below, in one place, rather than being spread through the parse.
///
/// Deriving this instead of walking a `toml::Table` by hand is what keeps
/// `Config` and the file format from drifting: adding a setting is one line
/// here and one line in the `Config` construction, and a mistyped value is
/// rejected by the deserializer with the offending key and line already in the
/// message, rather than by hand-written type checks that each had to remember
/// to name the file.
///
/// Unknown keys are deliberately *not* rejected (`deny_unknown_fields` is
/// absent): a config written for a newer version should not make an older
/// binary refuse to start.
#[derive(Debug, Default, serde::Deserialize)]
struct RawFile {
    #[serde(default)]
    index: RawIndex,
    #[serde(default)]
    search: RawSearch,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawIndex {
    default_db: Option<String>,
    languages: Option<String>,
    /// `u32`, so a negative `workers` is rejected by the deserializer.
    workers: Option<u32>,
    engine: Option<String>,
    #[serde(default, deserialize_with = "de_number")]
    min_word_conf: Option<f64>,
    thorough: Option<bool>,
    /// `u32`, so a negative trigger is rejected by the deserializer. `0` is
    /// legal and means "only re-read images the primary pass found nothing in".
    thorough_trigger_words: Option<u32>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawSearch {
    default_limit: Option<u32>,
}

/// Deserialize a TOML float *or* integer into `f64`.
///
/// `40` and `40.0` must both mean 40.0 — nobody writing a confidence floor by
/// hand types the decimal point — but a plain `Option<f64>` field rejects the
/// bare integer, since TOML keeps the two number types distinct. Strings
/// and booleans still fail, which is the point: this widens the accepted number
/// syntax, it does not turn the field into "anything".
fn de_number<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct NumberVisitor;

    impl serde::de::Visitor<'_> for NumberVisitor {
        type Value = f64;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a number")
        }

        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<f64, E> {
            Ok(v)
        }

        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<f64, E> {
            Ok(v as f64)
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<f64, E> {
            Ok(v as f64)
        }
    }

    deserializer.deserialize_any(NumberVisitor).map(Some)
}

/// Load config from TOML; a missing file returns defaults (no error).
pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
    let path: PathBuf = match path {
        Some(p) => p.to_path_buf(),
        None => default_path(),
    };

    if !path.exists() {
        return Ok(Config::default());
    }

    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
        path: path.clone(),
        source,
    })?;
    let raw: RawFile = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.clone(),
        source: Box::new(source),
    })?;

    // Type errors are the deserializer's job and are already reported above.
    // What remains here is what a type cannot express: range and enum
    // membership.
    let engine = raw
        .index
        .engine
        .unwrap_or_else(|| DEFAULT_ENGINE.to_string());
    if !VALID_ENGINES.contains(&engine.as_str()) {
        return Err(ConfigError::Invalid(format!(
            "Invalid config {}: engine must be one of {:?}, got {:?}",
            path.display(),
            VALID_ENGINES,
            engine
        )));
    }

    let languages = raw
        .index
        .languages
        .unwrap_or_else(|| DEFAULT_LANGUAGES.to_string());
    crate::lang::validate(&languages)
        .map_err(|e| ConfigError::Invalid(format!("Invalid config {}: {e}", path.display())))?;

    let min_word_conf = match raw.index.min_word_conf {
        Some(n) => {
            if !(0.0..=100.0).contains(&n) {
                return Err(ConfigError::Invalid(format!(
                    "Invalid config {}: min_word_conf must be between 0 and 100, got {}",
                    path.display(),
                    n
                )));
            }
            n as f32
        }
        None => DEFAULT_MIN_WORD_CONF,
    };

    // `u32` already excludes negatives; zero is the remaining bad case, and
    // only for `default_limit` — `workers = 0` means "pick a pool size".
    let default_limit = match raw.search.default_limit {
        Some(0) => {
            return Err(ConfigError::Invalid(format!(
                "Invalid config {}: default_limit must be > 0, got 0",
                path.display()
            )));
        }
        Some(n) => n,
        None => DEFAULT_LIMIT,
    };

    Ok(Config {
        default_db: raw
            .index
            .default_db
            .map(|s| expanduser(&s))
            .unwrap_or_else(default_db_path),
        languages,
        workers: raw.index.workers.unwrap_or(DEFAULT_WORKERS),
        default_limit,
        engine,
        min_word_conf,
        thorough: raw.index.thorough.unwrap_or(DEFAULT_THOROUGH),
        thorough_trigger_words: raw
            .index
            .thorough_trigger_words
            .unwrap_or(DEFAULT_THOROUGH_TRIGGER_WORDS),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_defaults() {
        let cfg = load(Some(Path::new("/definitely/not/here.toml"))).unwrap();
        assert_eq!(cfg.languages, "eng");
        assert_eq!(cfg.workers, 0);
        assert_eq!(cfg.default_limit, 20);
        assert_eq!(cfg.engine, "auto");
        assert_eq!(cfg.min_word_conf, 40.0);
        assert!(!cfg.thorough);
        assert_eq!(cfg.thorough_trigger_words, 3);
    }

    #[test]
    fn reads_sections_and_expands_tilde() {
        let dir = std::env::temp_dir().join(format!("lq_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(
            &p,
            "[index]\ndefault_db = \"~/custom/index.db\"\nlanguages = \"eng\"\nworkers = 4\nengine = \"dll\"\nmin_word_conf = 55.5\nthorough = true\n[search]\ndefault_limit = 5\n",
        )
        .unwrap();
        let cfg = load(Some(&p)).unwrap();
        assert_eq!(cfg.languages, "eng");
        assert_eq!(cfg.workers, 4);
        assert_eq!(cfg.default_limit, 5);
        assert_eq!(cfg.engine, "dll");
        assert_eq!(cfg.min_word_conf, 55.5);
        assert!(cfg.thorough);
        assert!(
            cfg.default_db.ends_with("custom/index.db")
                || cfg.default_db.ends_with("custom\\index.db")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_engine_errors() {
        let dir = std::env::temp_dir().join(format!("lq_cfg_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "[index]\nengine = \"nope\"\n").unwrap();
        assert!(load(Some(&p)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn negative_workers_errors() {
        let dir = std::env::temp_dir().join(format!("lq_cfg_w_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "[index]\nworkers = -1\n").unwrap();
        assert!(load(Some(&p)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write `body` to a throwaway config and return the `load()` outcome.
    fn load_snippet(tag: &str, body: &str) -> Result<Config, ConfigError> {
        let dir = std::env::temp_dir().join(format!("lq_cfg_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, body).unwrap();
        let out = load(Some(&p));
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn min_word_conf_accepts_integer_and_rejects_out_of_range() {
        // A bare TOML integer is a valid confidence.
        let cfg = load_snippet("mwc_int", "[index]\nmin_word_conf = 60\n").unwrap();
        assert_eq!(cfg.min_word_conf, 60.0);
        // Boundaries are inclusive.
        assert!(load_snippet("mwc_lo", "[index]\nmin_word_conf = 0\n").is_ok());
        assert!(load_snippet("mwc_hi", "[index]\nmin_word_conf = 100\n").is_ok());
        // Out of range and wrong type both reject.
        assert!(load_snippet("mwc_over", "[index]\nmin_word_conf = 100.5\n").is_err());
        assert!(load_snippet("mwc_neg", "[index]\nmin_word_conf = -1\n").is_err());
        assert!(load_snippet("mwc_str", "[index]\nmin_word_conf = \"40\"\n").is_err());
    }

    #[test]
    fn wrong_types_are_rejected_by_the_deserializer() {
        // These used to be hand-written type checks producing
        // `ConfigError::Invalid`; they now surface as `Parse`. Both are errors
        // and both name the file, but the classification changed, so pin it.
        for (tag, body) in [
            ("w_str", "[index]\nworkers = \"four\"\n"),
            ("w_float", "[index]\nworkers = 1.5\n"),
            ("lim_str", "[search]\ndefault_limit = \"ten\"\n"),
            ("lang_int", "[index]\nlanguages = 7\n"),
            ("db_int", "[index]\ndefault_db = 3\n"),
        ] {
            assert!(
                matches!(load_snippet(tag, body), Err(ConfigError::Parse { .. })),
                "{tag} should be a parse error"
            );
        }
    }

    #[test]
    fn zero_default_limit_errors_but_zero_workers_does_not() {
        // `workers = 0` is meaningful — it means "choose the pool size for me".
        // A limit of zero would return nothing from every search.
        assert!(load_snippet("lim_zero", "[search]\ndefault_limit = 0\n").is_err());
        let cfg = load_snippet("w_zero", "[index]\nworkers = 0\n").unwrap();
        assert_eq!(cfg.workers, 0);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // A config written by a newer version must not stop an older binary.
        let cfg = load_snippet(
            "unknown",
            "[index]\nlanguages = \"eng\"\nfuture_knob = 1\n[nonesuch]\nx = true\n",
        )
        .unwrap();
        assert_eq!(cfg.languages, "eng");
    }

    #[test]
    fn thorough_must_be_boolean() {
        let cfg = load_snippet("th_true", "[index]\nthorough = true\n").unwrap();
        assert!(cfg.thorough);
        assert!(load_snippet("th_int", "[index]\nthorough = 1\n").is_err());
        assert!(load_snippet("th_str", "[index]\nthorough = \"yes\"\n").is_err());
    }

    #[test]
    fn thorough_trigger_words_is_a_non_negative_integer() {
        // `0` is a legal setting, not a missing one — "second arm only where the
        // primary found nothing" — so it must survive rather than fall back to
        // the default of 3. Negatives and floats are the deserializer's to reject.
        let cfg = load_snippet("ttw", "[index]\nthorough_trigger_words = 7\n").unwrap();
        assert_eq!(cfg.thorough_trigger_words, 7);
        let zero = load_snippet("ttw0", "[index]\nthorough_trigger_words = 0\n").unwrap();
        assert_eq!(zero.thorough_trigger_words, 0);
        assert!(load_snippet("ttw_neg", "[index]\nthorough_trigger_words = -1\n").is_err());
        assert!(load_snippet("ttw_f", "[index]\nthorough_trigger_words = 2.5\n").is_err());
    }
}
