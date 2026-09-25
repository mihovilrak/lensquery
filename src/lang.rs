//! Language packs: which ones exist, which are installed, and how to get more.
//!
//! The set of downloadable packs is a manifest compiled into the binary from
//! `assets/tessdata-fast-4.1.0.toml`, pinned to an upstream tag and carrying a
//! SHA-256 per pack. Nothing here reaches the network except [`fetch`], and
//! that is reached only from an explicit `lq lang add`.

#[cfg(feature = "download")]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Orientation and script detection. A legitimate download and a real Tesseract
/// data file, but not a recognition model — see [`validate`].
pub const OSD: &str = "osd";

pub const OFFLINE_ENV_VAR: &str = "LENSQUERY_OFFLINE";

const MANIFEST_TOML: &str = include_str!("../assets/tessdata-fast-4.1.0.toml");

#[derive(Debug, serde::Deserialize)]
pub struct Manifest {
    pub source: String,
    pub tag: String,
    pub generated: String,
    #[serde(default, rename = "pack")]
    pub packs: Vec<Pack>,
}

#[derive(Debug, serde::Deserialize)]
pub struct Pack {
    pub code: String,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Language,
    Detector,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "`osd` is not a language: it detects page orientation and script and \
         contains no recognition model, so Tesseract reads nothing useful with \
         it in `--lang`. Drop it from `{spec}` and keep the real languages"
    )]
    OsdIsNotALanguage { spec: String },

    #[error("empty language code in `{spec}` — expected codes joined by `+`, like `eng+hrv`")]
    EmptyCode { spec: String },

    #[error(
        "`{code}` is not a valid language code: expected letters, digits and \
         underscores, like `eng` or `chi_sim`"
    )]
    BadCode { code: String },

    #[error("unknown language pack `{code}` — run `lq lang available` for the list")]
    UnknownPack { code: String },

    #[error("no writable directory for language packs: {0}")]
    NoInstallDir(String),

    #[error(
        "downloads are disabled ({reason}). Fetch \
         {repo}/raw/{tag}/{code}.traineddata by hand and drop it in {dir}"
    )]
    Offline {
        reason: &'static str,
        repo: String,
        tag: String,
        code: String,
        dir: String,
    },

    #[error("download of `{code}` failed: {msg}")]
    Download { code: String, msg: String },

    #[error(
        "`{code}` failed verification and was not installed: expected sha256 \
         {want}, got {got}. The manifest is pinned to {tag}; a mismatch means \
         the download was corrupted or tampered with"
    )]
    Checksum {
        code: String,
        want: String,
        got: String,
        tag: String,
    },

    #[error("{0}")]
    Io(String),
}

type Result<T> = std::result::Result<T, Error>;

pub fn manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        toml::from_str(MANIFEST_TOML).expect("the compiled-in tessdata manifest is malformed")
    })
}

pub fn find(code: &str) -> Option<&'static Pack> {
    manifest().packs.iter().find(|p| p.code == code)
}

/// Split a Tesseract `lang=` string into its codes.
pub fn parts(spec: &str) -> impl Iterator<Item = &str> {
    spec.split('+').map(str::trim)
}

/// Check a `lang=` string before it reaches Tesseract.
///
/// Rejects `osd` loudly rather than filtering it out: someone who typed it
/// believes it does something, and a silent drop leaves that belief intact.
/// Unknown codes pass — a hand-placed custom `.traineddata` is legitimate, and
/// the missing-pack check in `cli` reports those with the actual install path.
pub fn validate(spec: &str) -> Result<()> {
    for code in parts(spec) {
        if code.is_empty() {
            return Err(Error::EmptyCode {
                spec: spec.to_string(),
            });
        }
        if code.eq_ignore_ascii_case(OSD) {
            return Err(Error::OsdIsNotALanguage {
                spec: spec.to_string(),
            });
        }
        if !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::BadCode {
                code: code.to_string(),
            });
        }
    }
    Ok(())
}

/// Where LensQuery puts packs it downloads itself, when it cannot write to the
/// directory Tesseract already uses.
pub fn user_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("lensquery").join("tessdata"))
}

/// True once the user directory holds at least one pack — the point at which
/// [`crate::tess`] starts preferring it over the one beside the library.
pub fn user_dir_is_active() -> bool {
    user_dir().is_some_and(|d| codes_in(&d).next().is_some())
}

/// `.traineddata` stems in `dir`, unsorted.
pub fn codes_in(dir: &Path) -> impl Iterator<Item = String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            (path.extension().and_then(|x| x.to_str()) == Some("traineddata"))
                .then(|| path.file_stem()?.to_str().map(str::to_string))
                .flatten()
        })
}

/// Where the next `lq lang add` would write.
///
/// Tesseract takes exactly one datapath, so installing next to the packs the
/// system already provides keeps them all visible; that works for a Homebrew
/// or a portable install and not for `/usr/share`. The fallback is a directory
/// LensQuery owns — which then becomes the active one, hiding whatever sat
/// beside the library. Callers are expected to say so out loud: see
/// [`shadowed_by_install`].
pub fn install_dir() -> Result<PathBuf> {
    if let Some(dir) = crate::tess::tessdata_path() {
        if dir.is_dir() && is_writable(&dir) {
            return Ok(dir);
        }
    }
    let dir = user_dir().ok_or_else(|| {
        Error::NoInstallDir("no user data directory on this platform".to_string())
    })?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::NoInstallDir(format!("{}: {e}", dir.display())))?;
    Ok(dir)
}

/// Packs that would stop being visible if the next install lands in the user
/// directory and activates it. Empty in every other case.
pub fn shadowed_by_install(install: &Path) -> Vec<String> {
    let Some(user) = user_dir() else {
        return Vec::new();
    };
    if install != user || user_dir_is_active() {
        return Vec::new();
    }
    let Some(current) = crate::tess::tessdata_path() else {
        return Vec::new();
    };
    let mut codes: Vec<String> = codes_in(&current).collect();
    codes.sort();
    codes
}

/// Copy `codes` from the directory Tesseract reads today into `install`.
///
/// Tesseract accepts exactly one datapath, so the moment our directory holds a
/// pack it becomes the only one it reads and everything beside the library goes
/// invisible. The caller asks [`shadowed_by_install`] what that would cost
/// *before* installing anything, then calls this once the install has actually
/// happened — a failed download must not leave the search path moved.
pub fn adopt(from: &Path, install: &Path, codes: &[String]) -> Result<()> {
    for code in codes {
        let name = format!("{code}.traineddata");
        let to = install.join(&name);
        if to.exists() {
            continue;
        }
        std::fs::copy(from.join(&name), &to)
            .map_err(|e| Error::Io(format!("{}: {e}", to.display())))?;
    }
    Ok(())
}

fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".lensquery-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The pack matching the system locale, if there is one and it is not already
/// installed. Only ever a suggestion — a language nobody asked for costs index
/// time on every image, so it is never added on our own initiative.
pub fn suggest_from_locale() -> Option<(&'static str, &'static Pack)> {
    let locale = sys_locale::get_locale()?;
    let primary = locale.split(['-', '_']).next()?.to_ascii_lowercase();
    let code = LOCALE_TO_PACK
        .iter()
        .find(|(iso, _)| *iso == primary)
        .map(|(_, code)| *code)
        .or_else(|| find(&primary).map(|p| p.code.as_str()))?;
    if crate::tess::available_langs().iter().any(|l| l == code) {
        return None;
    }
    let pack = find(code)?;
    Some((Box::leak(locale.into_boxed_str()), pack))
}

/// The URL a pack is fetched from, derived from the manifest so the pin lives
/// in exactly one place.
pub fn url(pack: &Pack) -> String {
    let m = manifest();
    let raw = m
        .source
        .replace("https://github.com/", "https://raw.githubusercontent.com/");
    format!("{raw}/{}/{}.traineddata", m.tag, pack.code)
}

/// Lowercase hex, because sha2 0.11 hands back a byte array with no `LowerHex`.
#[cfg(feature = "download")]
fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            acc.push_str(&format!("{b:02x}"));
            acc
        })
}

pub fn is_offline(flag: bool) -> bool {
    flag || std::env::var_os(OFFLINE_ENV_VAR).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Download `pack` into `dir`, verify it, and only then put it in place.
///
/// The bytes land in a `.part` file that is renamed after the hash matches, so
/// a failed or tampered download can never leave something Tesseract would
/// load.
pub fn fetch(
    pack: &Pack,
    dir: &Path,
    offline: bool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf> {
    let m = manifest();
    if offline {
        return Err(Error::Offline {
            reason: "--offline",
            repo: m.source.clone(),
            tag: m.tag.clone(),
            code: pack.code.clone(),
            dir: dir.display().to_string(),
        });
    }
    #[cfg(not(feature = "download"))]
    {
        let _ = progress;
        Err(Error::Offline {
            reason: "this binary was built with --no-default-features",
            repo: m.source.clone(),
            tag: m.tag.clone(),
            code: pack.code.clone(),
            dir: dir.display().to_string(),
        })
    }
    #[cfg(feature = "download")]
    {
        use sha2::Digest;

        std::fs::create_dir_all(dir).map_err(|e| Error::Io(format!("{}: {e}", dir.display())))?;
        let part = dir.join(format!("{}.traineddata.part", pack.code));
        let final_path = dir.join(format!("{}.traineddata", pack.code));

        let resp = ureq::get(url(pack)).call().map_err(|e| Error::Download {
            code: pack.code.clone(),
            msg: e.to_string(),
        })?;
        let mut body = resp.into_body().into_reader();

        let mut out = std::fs::File::create(&part)
            .map_err(|e| Error::Io(format!("{}: {e}", part.display())))?;
        let mut hasher = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut done: u64 = 0;
        loop {
            let n = match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    return Err(Error::Download {
                        code: pack.code.clone(),
                        msg: e.to_string(),
                    });
                }
            };
            hasher.update(&buf[..n]);
            if let Err(e) = std::io::Write::write_all(&mut out, &buf[..n]) {
                let _ = std::fs::remove_file(&part);
                return Err(Error::Io(format!("{}: {e}", part.display())));
            }
            done += n as u64;
            progress(done, pack.size);
        }
        drop(out);

        let got = hex(&sha2::Digest::finalize(hasher));
        if got != pack.sha256 || done != pack.size {
            let _ = std::fs::remove_file(&part);
            return Err(Error::Checksum {
                code: pack.code.clone(),
                want: pack.sha256.clone(),
                got,
                tag: m.tag.clone(),
            });
        }
        std::fs::rename(&part, &final_path)
            .map_err(|e| Error::Io(format!("{}: {e}", final_path.display())))?;
        Ok(final_path)
    }
}

/// ISO 639-1 to the Tesseract pack for it. Only where the mapping is
/// unambiguous — `zh` deliberately has no entry, because guessing between
/// simplified and traditional is worse than saying nothing.
const LOCALE_TO_PACK: &[(&str, &str)] = &[
    ("af", "afr"),
    ("am", "amh"),
    ("ar", "ara"),
    ("as", "asm"),
    ("az", "aze"),
    ("be", "bel"),
    ("bg", "bul"),
    ("bn", "ben"),
    ("bo", "bod"),
    ("bs", "bos"),
    ("ca", "cat"),
    ("cs", "ces"),
    ("cy", "cym"),
    ("da", "dan"),
    ("de", "deu"),
    ("dv", "div"),
    ("dz", "dzo"),
    ("el", "ell"),
    ("en", "eng"),
    ("eo", "epo"),
    ("es", "spa"),
    ("et", "est"),
    ("eu", "eus"),
    ("fa", "fas"),
    ("fi", "fin"),
    ("fo", "fao"),
    ("fr", "fra"),
    ("fy", "fry"),
    ("ga", "gle"),
    ("gd", "gla"),
    ("gl", "glg"),
    ("gu", "guj"),
    ("he", "heb"),
    ("hi", "hin"),
    ("hr", "hrv"),
    ("ht", "hat"),
    ("hu", "hun"),
    ("hy", "hye"),
    ("id", "ind"),
    ("is", "isl"),
    ("it", "ita"),
    ("iu", "iku"),
    ("ja", "jpn"),
    ("jv", "jav"),
    ("ka", "kat"),
    ("kk", "kaz"),
    ("km", "khm"),
    ("kn", "kan"),
    ("ko", "kor"),
    ("ku", "kmr"),
    ("ky", "kir"),
    ("la", "lat"),
    ("lb", "ltz"),
    ("lo", "lao"),
    ("lt", "lit"),
    ("lv", "lav"),
    ("mi", "mri"),
    ("mk", "mkd"),
    ("ml", "mal"),
    ("mn", "mon"),
    ("mr", "mar"),
    ("ms", "msa"),
    ("mt", "mlt"),
    ("my", "mya"),
    ("ne", "nep"),
    ("nl", "nld"),
    ("no", "nor"),
    ("oc", "oci"),
    ("or", "ori"),
    ("pa", "pan"),
    ("pl", "pol"),
    ("ps", "pus"),
    ("pt", "por"),
    ("qu", "que"),
    ("ro", "ron"),
    ("ru", "rus"),
    ("sa", "san"),
    ("sd", "snd"),
    ("si", "sin"),
    ("sk", "slk"),
    ("sl", "slv"),
    ("sq", "sqi"),
    ("sr", "srp"),
    ("su", "sun"),
    ("sv", "swe"),
    ("sw", "swa"),
    ("ta", "tam"),
    ("te", "tel"),
    ("tg", "tgk"),
    ("th", "tha"),
    ("ti", "tir"),
    ("tk", "tur"),
    ("to", "ton"),
    ("tr", "tur"),
    ("tt", "tat"),
    ("ug", "uig"),
    ("uk", "ukr"),
    ("ur", "urd"),
    ("uz", "uzb"),
    ("vi", "vie"),
    ("yi", "yid"),
    ("yo", "yor"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compiled_manifest_parses_and_is_complete() {
        let m = manifest();
        assert_eq!(m.tag, "4.1.0");
        assert!(m.packs.len() > 100, "only {} packs", m.packs.len());
        for p in &m.packs {
            assert_eq!(p.sha256.len(), 64, "{} has a short hash", p.code);
            assert!(p.size > 0, "{} has no size", p.code);
            assert!(!p.name.is_empty(), "{} has no display name", p.code);
        }
    }

    #[test]
    fn osd_is_a_detector_in_the_manifest_and_rejected_as_a_language() {
        assert_eq!(find(OSD).unwrap().kind, Kind::Detector);
        assert!(matches!(
            validate("eng+osd"),
            Err(Error::OsdIsNotALanguage { .. })
        ));
        assert!(matches!(
            validate("OSD"),
            Err(Error::OsdIsNotALanguage { .. })
        ));
    }

    #[test]
    fn ordinary_language_strings_validate() {
        for spec in ["eng", "eng+hrv", "chi_sim", "eng+deu+fra"] {
            validate(spec).unwrap_or_else(|e| panic!("{spec} rejected: {e}"));
        }
    }

    #[test]
    fn malformed_language_strings_are_rejected_with_the_reason() {
        assert!(matches!(validate("eng+"), Err(Error::EmptyCode { .. })));
        assert!(matches!(validate(""), Err(Error::EmptyCode { .. })));
        assert!(matches!(validate("eng hrv"), Err(Error::BadCode { .. })));
        assert!(matches!(validate("../etc"), Err(Error::BadCode { .. })));
    }

    #[test]
    fn every_locale_mapping_names_a_pack_that_exists() {
        for (iso, code) in LOCALE_TO_PACK {
            assert!(find(code).is_some(), "{iso} maps to missing pack {code}");
        }
    }

    #[test]
    fn the_download_url_points_at_the_pinned_tag() {
        let pack = find("hrv").unwrap();
        assert_eq!(
            url(pack),
            "https://raw.githubusercontent.com/tesseract-ocr/tessdata_fast/4.1.0/hrv.traineddata"
        );
    }

    #[test]
    fn offline_is_off_unless_the_flag_or_a_meaningful_env_value_says_so() {
        assert!(is_offline(true));
        // The env var is process-global; only the flag path is safe to assert
        // here without racing other tests.
    }

    #[cfg(feature = "download")]
    #[test]
    fn hex_matches_the_manifest_formatting() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lensquery-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn adopt_copies_the_shadowed_packs_and_keeps_the_ones_already_there() {
        let root = scratch("adopt");
        let from = root.join("system");
        let install = root.join("user");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(from.join("eng.traineddata"), b"system-eng").unwrap();
        std::fs::write(from.join("hrv.traineddata"), b"system-hrv").unwrap();
        std::fs::write(install.join("hrv.traineddata"), b"ours").unwrap();

        let codes = vec!["eng".to_string(), "hrv".to_string()];
        adopt(&from, &install, &codes).unwrap();

        assert_eq!(
            std::fs::read(install.join("eng.traineddata")).unwrap(),
            b"system-eng"
        );
        // A pack we installed ourselves outranks the system copy.
        assert_eq!(
            std::fs::read(install.join("hrv.traineddata")).unwrap(),
            b"ours"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn adopt_names_the_file_it_could_not_copy() {
        let root = scratch("adopt-missing");
        let from = root.join("system");
        let install = root.join("user");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&install).unwrap();

        let err = adopt(&from, &install, &["eng".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("eng.traineddata"),
            "unhelpful error: {err}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
