//! Minimal runtime binding to the Tesseract C API.
//!
//! The library is `dlopen`ed once per process via `libloading` rather than
//! linked at build time. That is what lets one published binary run against
//! whatever Tesseract the user's package manager installed, and lets `lq
//! search` work on a machine with no Tesseract at all.
//!
//! Each worker thread owns its own `TessBaseAPI` — Tesseract is not safe to
//! share one instance across threads — created once and reused for every image
//! that thread handles. Creating an engine loads the language models, which
//! costs more than recognizing an image, so per-image creation would dominate a
//! run.
//!
//! **This is a pure binding.** Image preprocessing lives in [`crate::ocr`].
//!
//! Engine-level failures surface as `None`, distinct from "image had no text"
//! (`Some("")`). Collapsing those two would make a broken install look like a
//! folder of blank photographs.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use libloading::Library;

/// The shipping page segmentation mode: sparse text — run layout analysis and
/// read what it finds. Worth +0.049 findability over [`PSM_UNIFORM_BLOCK`], the
/// largest single accuracy win in the project; see
/// [benchmarks](../docs/benchmarks.md) §3.
pub const PSM_SPARSE: c_int = 11;
/// "One uniform block of text". **Never the default.** It does not hint that
/// the image is a page of text, it *asserts* it, so Tesseract dutifully finds
/// glyphs in brickwork and JPEG noise. Kept only as the second arm of
/// [`crate::ocr`]'s opt-in thorough mode, where a page that came back nearly
/// empty is worth re-reading under the opposite assumption.
pub const PSM_UNIFORM_BLOCK: c_int = 6;
const SOURCE_DPI: c_int = 300;
/// Tesseract's own parameter naming the file its `tprintf` diagnostics go to.
const DEBUG_FILE_VAR: &CStr = c"debug_file";
/// Where those diagnostics are sent instead of our stderr.
#[cfg(windows)]
const NULL_DEVICE: &CStr = c"NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &CStr = c"/dev/null";
/// `L_SEVERITY_NONE` from Leptonica's `environ.h`: emit nothing at all.
const LEPT_SEVERITY_NONE: c_int = 6;
/// `RIL_WORD` from Tesseract's `PageIteratorLevel` enum.
const RIL_WORD: c_int = 3;

/// Resolved function pointers into the loaded Tesseract library.
///
/// Raw `extern "C" fn` values are `Copy` + `Send` + `Sync`; the owning
/// `Library` is kept alive alongside them in a process-global `OnceLock` so the
/// pointers stay valid for the life of the process. `TessBaseAPI*` handles are
/// never shared across threads, so no locking is needed here.
struct TessLib {
    create: unsafe extern "C" fn() -> *mut c_void,
    delete: unsafe extern "C" fn(*mut c_void),
    init3: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    set_psm: unsafe extern "C" fn(*mut c_void, c_int),
    set_image: unsafe extern "C" fn(*mut c_void, *const u8, c_int, c_int, c_int, c_int),
    set_resolution: unsafe extern "C" fn(*mut c_void, c_int),
    get_utf8_text: unsafe extern "C" fn(*mut c_void) -> *mut c_char,
    delete_text: unsafe extern "C" fn(*mut c_char),
    clear: unsafe extern "C" fn(*mut c_void),
    end: unsafe extern "C" fn(*mut c_void),
    set_variable: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    // Result-iterator symbols, needed only for the confidence-filtered path.
    // Note `TessBaseAPIGetTSVText` — the obvious alternative — is NOT exported
    // by the bundled libtesseract-5.dll, so per-word confidence has to come
    // from the iterator.
    recognize_pass: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
    get_iterator: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    iter_text: unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_char,
    iter_conf: unsafe extern "C" fn(*mut c_void, c_int) -> f32,
    iter_next: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    iter_delete: unsafe extern "C" fn(*mut c_void),
    /// Leptonica's `setMsgSeverity`, when it is reachable through this handle.
    /// Leptonica is a *dependency* of libtesseract rather than part of it, so
    /// this resolves on platforms whose loader walks the dependency chain
    /// (`dlsym`) and stays `None` on Windows, where `GetProcAddress` only looks
    /// inside the module it was handed. Nothing depends on it being present.
    lept_set_msg_severity: Option<unsafe extern "C" fn(c_int) -> c_int>,
    /// False when the user asked to see the engine's own diagnostics.
    quiet: bool,
    path: PathBuf,
    // Kept alive so the function pointers above remain valid. Never dropped
    // (lives in a `'static` `OnceLock`).
    _lib: Library,
}

// The raw handle pointers we hand around are only ever touched from the thread
// that created them (thread-local `Engine`); the shared `TessLib` holds only
// `Copy` fn pointers plus the `Library`, both `Send`/`Sync`.
unsafe impl Send for TessLib {}
unsafe impl Sync for TessLib {}

impl TessLib {
    /// Stop Leptonica writing to our stderr.
    ///
    /// Leptonica reports on images it dislikes (`Error in pixScanForForeground:
    /// invalid box`) at error severity, and every one of those is a judgement
    /// about a *page*, not about the program: the image still OCRs, and if it
    /// genuinely fails the caller already says so. Left alone the messages land
    /// on top of the progress bar and read like a broken install. The switch is
    /// a process-global inside Leptonica, so it is flipped once, here, while
    /// the `OnceLock` initialiser still has the library to itself.
    fn silence_leptonica(&self) {
        if !self.quiet {
            return;
        }
        if let Some(set_severity) = self.lept_set_msg_severity {
            // SAFETY: `setMsgSeverity(l_int32) -> l_int32` per `environ.h`.
            unsafe { set_severity(LEPT_SEVERITY_NONE) };
        }
    }
}

/// The loaded library, if any, paired with the trail of everything tried to get
/// there. One `OnceLock` rather than two so the trail is guaranteed to describe
/// the load that actually happened, not a later re-derivation of it.
static LIB: OnceLock<(Option<TessLib>, Vec<Attempt>)> = OnceLock::new();

/// Environment variable naming the library file to load, bypassing the search
/// entirely. The escape hatch for anyone whose Tesseract lives somewhere this
/// module would never guess (a Nix store path, a custom build, a container).
pub const LIB_ENV_VAR: &str = "LENSQUERY_TESSERACT_LIB";

/// Set this (to anything but `0`) to let Tesseract and Leptonica write their
/// own diagnostics to stderr. Off by default: that output is per-page chatter
/// ("Detected 37 diacritics"), it is not addressed to the user, and it arrives
/// interleaved with the progress bar.
pub const DEBUG_ENV_VAR: &str = "LENSQUERY_TESSERACT_DEBUG";

/// Whether `DEBUG_ENV_VAR`'s value asks for engine diagnostics.
///
/// Unset, empty and `0` all mean "stay quiet", so `LENSQUERY_TESSERACT_DEBUG=0`
/// does what it looks like it does rather than the opposite.
fn debug_requested(value: Option<&std::ffi::OsStr>) -> bool {
    match value {
        Some(v) => !v.is_empty() && v != "0",
        None => false,
    }
}

/// One entry in the library-resolution trail.
///
/// The trail is recorded during the single real load attempt and kept for the
/// life of the process so `lq doctor` can show the user every path that was
/// tried and why each was rejected. "libtesseract not found" is the single most
/// likely first-run failure; a bare "not found" makes the user guess, and the
/// guesses are wrong often enough (wrong architecture and missing-dependency
/// failures both *look* like absence) to be worth this bookkeeping.
pub struct Attempt {
    pub path: PathBuf,
    pub outcome: Outcome,
}

/// Why a candidate was rejected, or that it was accepted.
pub enum Outcome {
    /// Nothing at this path.
    NotFound,
    /// The file is there but the loader refused it. Usually an architecture
    /// mismatch (x86_64 library, arm64 process) or one of the library's *own*
    /// dependencies missing — the message is the platform loader's.
    OpenFailed(String),
    /// It loaded, but does not export the Tesseract C API. Some other library
    /// with a colliding name, or a Tesseract too old to have the C bindings.
    MissingSymbols,
    /// This is the one in use.
    Loaded,
}

/// Library filenames to try, in order, on this platform.
///
/// Distributions do not agree on what to call this file. Windows builds ship
/// `libtesseract-5.dll` (UB Mannheim) or a vcpkg-style `tesseract5N.dll`;
/// Homebrew installs `libtesseract.5.dylib`; Linux packages install
/// `libtesseract.so.5` with an unversioned `.so` symlink only when the `-dev`
/// package is present. Trying one name is why macOS never worked.
fn library_filenames() -> &'static [&'static str] {
    if cfg!(windows) {
        &[
            "libtesseract-5.dll",
            "libtesseract.dll",
            "tesseract55.dll",
            "tesseract54.dll",
            "tesseract53.dll",
        ]
    } else if cfg!(target_os = "macos") {
        &["libtesseract.5.dylib", "libtesseract.dylib"]
    } else {
        &["libtesseract.so.5", "libtesseract.so"]
    }
}

/// Directories to search, most specific first: a library bundled beside our own
/// binary wins over a system one, and a system one that `tesseract` itself came
/// from wins over a guess at the usual install prefixes.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |d: PathBuf| {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            push(parent.to_path_buf());
        }
    }
    if let Some(dir) = tesseract_dir_on_path() {
        // Windows keeps the DLL beside tesseract.exe; Unix puts the binary in
        // `<prefix>/bin` and the library in `<prefix>/lib`.
        if cfg!(windows) {
            push(dir);
        } else {
            if let Some(prefix) = dir.parent() {
                push(prefix.join("lib"));
            }
            push(dir);
        }
    }

    let system: &[&str] = if cfg!(target_os = "macos") {
        // Homebrew on Apple silicon, Homebrew on Intel, then the system prefix.
        &["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"]
    } else if cfg!(windows) {
        &[]
    } else {
        &[
            "/usr/lib/x86_64-linux-gnu",
            "/usr/lib/aarch64-linux-gnu",
            "/usr/lib64",
            "/usr/local/lib",
            "/usr/lib",
        ]
    };
    for d in system {
        push(PathBuf::from(d));
    }
    dirs
}

/// Every candidate, in the order they will be tried.
///
/// The trailing bare filenames have no directory and are handed to the platform
/// loader as-is, so `LD_LIBRARY_PATH`, the dyld shared cache, and the Windows
/// DLL search order all still get their say after our explicit list is
/// exhausted. That is what covers install prefixes nobody thought to list.
fn candidates() -> Vec<PathBuf> {
    candidates_with(std::env::var_os(LIB_ENV_VAR), &search_dirs())
}

/// The candidate list as a pure function of its two inputs, so the ordering
/// rules can be tested without touching process environment or the filesystem.
fn candidates_with(explicit: Option<std::ffi::OsString>, dirs: &[PathBuf]) -> Vec<PathBuf> {
    if let Some(explicit) = explicit {
        // An override that does not work is an error to report, not a hint to
        // fall back from: silently searching elsewhere would hide the typo.
        return vec![PathBuf::from(explicit)];
    }
    let names = library_filenames();
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        out.extend(names.iter().map(|n| dir.join(n)));
    }
    out.extend(names.iter().map(PathBuf::from));
    out
}

/// Directory containing a `tesseract` executable resolvable on `PATH`.
fn tesseract_dir_on_path() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        "tesseract.exe"
    } else {
        "tesseract"
    };
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find(|dir| dir.join(exe_name).exists())
}

/// Load and bind the library once. Failure is cached: a load that failed once
/// fails the same way every time in this process, and retrying it per image
/// would be tens of thousands of pointless `dlopen` calls.
fn lib() -> Option<&'static TessLib> {
    LIB.get_or_init(load).0.as_ref()
}

/// The full resolution trail, in the order tried. Forces the load if it has not
/// happened yet, so this is meaningful even as the first call in a process.
pub fn attempts() -> &'static [Attempt] {
    &LIB.get_or_init(load).1
}

/// Platform-appropriate one-liner for how to get libtesseract, for `doctor` to
/// print when the search comes up empty.
pub fn install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install tesseract"
    } else if cfg!(windows) {
        "install Tesseract (https://github.com/UB-Mannheim/tesseract/wiki) and \
         ensure its directory is on PATH"
    } else {
        "apt install libtesseract5  (or: dnf install tesseract, pacman -S tesseract)"
    }
}

/// Open a DLL by absolute path.
///
/// On Windows, `LOAD_WITH_ALTERED_SEARCH_PATH` makes the loader resolve the
/// DLL's own dependencies (leptonica, libpng, …) from the DLL's directory
/// rather than only the exe's directory. Without it, loading libtesseract by
/// full path fails whenever the running exe lives elsewhere (a build dir, a
/// different bundle layout) — which is exactly how a single relocatable
/// `lq.exe` must behave.
///
/// The flag is only valid with an absolute path — `LoadLibraryEx` documents the
/// behaviour with a relative one as undefined — so the bare-filename candidates
/// go through the plain load and get the normal DLL search order instead.
fn open_library(path: &Path) -> Result<Library, libloading::Error> {
    #[cfg(windows)]
    {
        use libloading::os::windows::Library as WinLibrary;
        const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;
        if !path.is_absolute() {
            // SAFETY: as below.
            return unsafe { Library::new(path) };
        }
        // SAFETY: same trust assumption as `Library::new`; only the search-path
        // flag differs.
        let lib = unsafe { WinLibrary::load_with_flags(path, LOAD_WITH_ALTERED_SEARCH_PATH)? };
        Ok(Library::from(lib))
    }
    #[cfg(not(windows))]
    {
        // SAFETY: as above; ELF/Mach-O resolve siblings via rpath/`LD_LIBRARY_PATH`.
        unsafe { Library::new(path) }
    }
}

/// Configure process-wide environment that Tesseract reads at init.
///
/// Keeps Tesseract's OpenMP layer single-threaded per process so it does not
/// oversubscribe against our worker-thread pool.
///
/// # Why this is not done lazily inside `load()`
///
/// `load()` runs inside a `OnceLock` initialiser, which is reached from
/// whichever worker thread happens to OCR first — while the other workers are
/// already running. `setenv` mutates a process-global table that any thread may
/// concurrently read (`getenv` in libc, in the loaded DLL, in a third-party
/// crate), and that is a data race, not merely a race on the value: on glibc a
/// concurrent `setenv` can free the block a `getenv` caller is still reading.
/// The `OnceLock` serialises *its own* initialiser, not the rest of the program.
///
/// Call this from `main`, before any thread is spawned, where the process is
/// still single-threaded and the write is unambiguously safe.
pub fn init_process_env() {
    if std::env::var_os("OMP_THREAD_LIMIT").is_none() {
        std::env::set_var("OMP_THREAD_LIMIT", "1");
    }
}

fn load() -> (Option<TessLib>, Vec<Attempt>) {
    let mut trail: Vec<Attempt> = Vec::new();
    let quiet = !debug_requested(std::env::var_os(DEBUG_ENV_VAR).as_deref());
    for path in candidates() {
        // Bare filenames (no directory component) are for the platform loader
        // to resolve, so there is nothing to existence-check.
        let has_dir = path.parent().is_some_and(|p| !p.as_os_str().is_empty());
        if has_dir && !path.exists() {
            trail.push(Attempt {
                path,
                outcome: Outcome::NotFound,
            });
            continue;
        }
        // SAFETY: loading a shared library runs its initializers; libtesseract
        // is trusted. A bad or incompatible file returns Err.
        let library = match open_library(&path) {
            Ok(l) => l,
            Err(e) => {
                trail.push(Attempt {
                    path,
                    outcome: Outcome::OpenFailed(e.to_string()),
                });
                continue;
            }
        };
        // SAFETY: the signatures below transcribe the Tesseract C API
        // (`capi.h`). A missing symbol returns Err and we reject the file.
        let bound = unsafe {
            (|| {
                Some(TessLib {
                    create: *library.get(b"TessBaseAPICreate\0").ok()?,
                    delete: *library.get(b"TessBaseAPIDelete\0").ok()?,
                    init3: *library.get(b"TessBaseAPIInit3\0").ok()?,
                    set_psm: *library.get(b"TessBaseAPISetPageSegMode\0").ok()?,
                    set_image: *library.get(b"TessBaseAPISetImage\0").ok()?,
                    set_resolution: *library.get(b"TessBaseAPISetSourceResolution\0").ok()?,
                    get_utf8_text: *library.get(b"TessBaseAPIGetUTF8Text\0").ok()?,
                    delete_text: *library.get(b"TessDeleteText\0").ok()?,
                    clear: *library.get(b"TessBaseAPIClear\0").ok()?,
                    end: *library.get(b"TessBaseAPIEnd\0").ok()?,
                    set_variable: *library.get(b"TessBaseAPISetVariable\0").ok()?,
                    recognize_pass: *library.get(b"TessBaseAPIRecognize\0").ok()?,
                    get_iterator: *library.get(b"TessBaseAPIGetIterator\0").ok()?,
                    iter_text: *library.get(b"TessResultIteratorGetUTF8Text\0").ok()?,
                    iter_conf: *library.get(b"TessResultIteratorConfidence\0").ok()?,
                    iter_next: *library.get(b"TessResultIteratorNext\0").ok()?,
                    iter_delete: *library.get(b"TessPageIteratorDelete\0").ok()?,
                    // Optional, and looked up through libtesseract's own
                    // handle rather than a second `dlopen`: whichever
                    // Leptonica libtesseract was built against is the one
                    // whose global we need to write.
                    lept_set_msg_severity: library.get(b"setMsgSeverity\0").ok().map(|s| *s),
                    quiet,
                    path: path.clone(),
                    _lib: library,
                })
            })()
        };
        match bound {
            Some(tess) => {
                tess.silence_leptonica();
                trail.push(Attempt {
                    path,
                    outcome: Outcome::Loaded,
                });
                return (Some(tess), trail);
            }
            None => trail.push(Attempt {
                path,
                outcome: Outcome::MissingSymbols,
            }),
        }
    }
    (None, trail)
}

/// True when a usable libtesseract is loadable in this process.
pub fn available() -> bool {
    lib().is_some()
}

/// Path of the loaded library, or `None` when unavailable.
pub fn library_path() -> Option<&'static Path> {
    lib().map(|l| l.path.as_path())
}

/// The tessdata directory Tesseract will actually be given, for `doctor`
/// diagnostics and for [`crate::lang`].
///
/// In order: `TESSDATA_PREFIX`, then the LensQuery-owned directory once it
/// holds a pack, then a `tessdata` directory next to the loaded library. A
/// `TESSDATA_PREFIX` pointing somewhere absent is returned as-is rather than
/// suppressed: "you configured this and it does not exist" is the diagnosis,
/// and hiding it would leave `doctor` silent about the actual cause. The other
/// two *are* existence-checked, since those are guesses rather than a stated
/// intent.
pub fn tessdata_path() -> Option<PathBuf> {
    if let Some(prefix) = std::env::var_os("TESSDATA_PREFIX") {
        return Some(PathBuf::from(prefix));
    }
    if crate::lang::user_dir_is_active() {
        return crate::lang::user_dir();
    }
    let beside = library_path()?.parent()?.join("tessdata");
    beside.exists().then_some(beside)
}

/// Language packs available in the resolved tessdata directory, sorted. Derived
/// from `*.traineddata` filenames (matches how `tesseract --list-langs` reports).
pub fn available_langs() -> Vec<String> {
    let Some(dir) = tessdata_path() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut langs: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) == Some("traineddata") {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            } else {
                None
            }
        })
        .collect();
    langs.sort();
    langs
}

/// The same resolution as [`tessdata_path`], in the form the FFI init wants.
fn datapath() -> Option<CString> {
    let dir = tessdata_path()?;
    if std::env::var_os("TESSDATA_PREFIX").is_none() && !dir.exists() {
        return None;
    }
    CString::new(dir.to_string_lossy().as_bytes()).ok()
}

/// A per-thread initialized engine for one language.
struct Engine {
    api: *mut c_void,
    lang: String,
    /// Page segmentation mode currently set on `api`. Tracked so `recognize`
    /// only calls `set_psm` when the requested mode actually differs.
    psm: c_int,
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Tear down cleanly so Tesseract's static destructors don't emit
        // ObjectCache "LEAK" noise on thread exit (mirrors `_shutdown`).
        if let Some(l) = lib() {
            if !self.api.is_null() {
                unsafe {
                    (l.end)(self.api);
                    (l.delete)(self.api);
                }
                self.api = std::ptr::null_mut();
            }
        }
    }
}

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
    // Langs whose init failed on this thread; never re-init a known-bad lang.
    static FAILED_LANGS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Create and initialize a fresh `TessBaseAPI` for `lang`, or `None` on failure.
fn init_engine(l: &TessLib, lang: &str) -> Option<Engine> {
    // SAFETY: FFI calls with argument types matching the C API.
    unsafe {
        let api = (l.create)();
        if api.is_null() {
            return None;
        }
        let datapath = datapath();
        let datapath_ptr = datapath.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        let lang_c = CString::new(lang).ok()?;
        let rc = (l.init3)(api, datapath_ptr, lang_c.as_ptr());
        if rc != 0 {
            (l.delete)(api);
            return None;
        }
        if l.quiet {
            // Tesseract's running commentary goes through `tprintf`, and
            // `debug_file` is the only supported way to redirect it. It has to
            // be set *after* `Init3`: `Init` may drop and rebuild the
            // underlying instance, taking any parameter set before it along.
            // A rejected parameter returns 0 and is not worth reacting to --
            // the worst case is the noise we already have.
            (l.set_variable)(api, DEBUG_FILE_VAR.as_ptr(), NULL_DEVICE.as_ptr());
        }
        (l.set_psm)(api, PSM_SPARSE);
        Some(Engine {
            api,
            lang: lang.to_string(),
            psm: PSM_SPARSE,
        })
    }
}

/// Tear down the calling thread's engine now, instead of at thread exit.
///
/// Tesseract's C++ static destructors run during library teardown and race the
/// automatic drop of the main thread's thread-local engine, which prints
/// spurious ObjectCache "LEAK" warnings to stderr — alarming output for a run
/// that succeeded. Call this on each worker thread before the pool joins, and
/// on the main thread before exit, so every engine is destroyed while the
/// library is still fully live.
pub fn shutdown() {
    ENGINE.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// OCR raw 8-bit grayscale pixels (one byte per pixel, row-major).
///
/// `min_conf` is a word-confidence floor (0–100); words scoring below it are
/// dropped. `0` keeps everything and takes the cheaper whole-page text path.
///
/// `psm` is the page segmentation mode. Changing it re-modes the calling
/// thread's cached engine in place (no re-init, so no model reload); it stays on
/// that mode until a call asks for another.
///
/// Returns the raw UTF-8 text (caller normalizes whitespace), or `None` on any
/// engine-level failure — the caller must treat that as "fall back / engine
/// failure", never as "image had no text". A page where *every* word fell below
/// `min_conf` yields `Some("")`, not `None` — that page was read successfully
/// and found to contain nothing worth indexing.
pub fn recognize(
    data: &[u8],
    width: i32,
    height: i32,
    lang: &str,
    min_conf: f32,
    psm: c_int,
) -> Option<String> {
    let l = lib()?;

    // Known-bad lang on this thread: don't thrash re-initializing it.
    let known_bad = FAILED_LANGS.with(|f| f.borrow().contains(lang));
    if known_bad {
        return None;
    }

    ENGINE.with(|cell| {
        {
            let mut slot = cell.borrow_mut();
            // Reuse the thread's engine if it's already on the right lang;
            // otherwise tear it down and build one for this lang.
            let needs_new = match slot.as_ref() {
                Some(e) => e.lang != lang,
                None => true,
            };
            if needs_new {
                *slot = None; // Drop the old engine first.
                match init_engine(l, lang) {
                    Some(e) => *slot = Some(e),
                    None => {
                        FAILED_LANGS.with(|f| {
                            f.borrow_mut().insert(lang.to_string());
                        });
                        return None;
                    }
                }
            }
        }

        let mut slot = cell.borrow_mut();
        let engine = slot.as_mut()?;
        let api = engine.api;
        // Re-mode in place only when the requested mode differs.
        // `SetPageSegMode` is not an init, so switching costs no model reload —
        // which is what makes `--thorough`'s second arm cheap enough to be a
        // per-image decision rather than a second pass over the corpus.
        if engine.psm != psm {
            // SAFETY: `api` is a live engine created by `init_engine`.
            unsafe { (l.set_psm)(api, psm) };
            engine.psm = psm;
        }
        // SAFETY: `api` is a live engine; `data` is `width*height` bytes at
        // one byte per pixel with `bytes_per_line == width`. `Clear` always
        // runs afterwards to release the image before the next call.
        unsafe {
            (l.set_image)(api, data.as_ptr(), width, height, 1, width);
            (l.set_resolution)(api, SOURCE_DPI);
            let result = if min_conf <= 0.0 {
                let ptr = (l.get_utf8_text)(api);
                if ptr.is_null() {
                    None
                } else {
                    let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
                    (l.delete_text)(ptr);
                    Some(text)
                }
            } else {
                recognize_filtered(l, api, min_conf)
            };
            (l.clear)(api);
            result
        }
    })
}

/// Recognize the already-set image, keeping only words at or above `min_conf`.
///
/// # Safety
///
/// `api` must be a live `TessBaseAPI` with an image already set.
unsafe fn recognize_filtered(l: &TessLib, api: *mut c_void, min_conf: f32) -> Option<String> {
    if (l.recognize_pass)(api, std::ptr::null_mut()) != 0 {
        return None;
    }
    let it = (l.get_iterator)(api);
    if it.is_null() {
        // Recognition succeeded but produced no result rows: no text, which is
        // "" rather than a failure.
        return Some(String::new());
    }
    let mut words: Vec<String> = Vec::new();
    loop {
        let ptr = (l.iter_text)(it, RIL_WORD);
        if !ptr.is_null() {
            let word = CStr::from_ptr(ptr).to_string_lossy().into_owned();
            (l.delete_text)(ptr);
            let word = word.trim();
            // Confidence is only read for non-blank words: blank iterator rows
            // are structural (line and block boundaries) and carry no score
            // worth thresholding.
            if !word.is_empty() && (l.iter_conf)(it, RIL_WORD) >= min_conf {
                words.push(word.to_string());
            }
        }
        if (l.iter_next)(it, RIL_WORD) == 0 {
            break;
        }
    }
    (l.iter_delete)(it);
    Some(words.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_override_replaces_the_search_rather_than_extending_it() {
        let dirs = vec![PathBuf::from("/somewhere")];
        let got = candidates_with(Some("/opt/custom/libtesseract.so".into()), &dirs);
        assert_eq!(got, vec![PathBuf::from("/opt/custom/libtesseract.so")]);
    }

    #[test]
    fn candidates_exhaust_each_directory_before_moving_to_the_next() {
        // Directory order encodes priority (bundled beside the exe beats a
        // system copy); filename order is only about naming conventions. So a
        // second-choice filename in the first directory must still win over a
        // first-choice filename in the second.
        let dirs = vec![PathBuf::from("/first"), PathBuf::from("/second")];
        let got = candidates_with(None, &dirs);
        let names = library_filenames();

        assert_eq!(got.len(), dirs.len() * names.len() + names.len());
        for (i, dir) in dirs.iter().enumerate() {
            for (j, name) in names.iter().enumerate() {
                assert_eq!(got[i * names.len() + j], dir.join(name));
            }
        }
    }

    #[test]
    fn the_last_candidates_are_bare_names_for_the_platform_loader() {
        let got = candidates_with(None, &[PathBuf::from("/first")]);
        let names = library_filenames();
        let tail = &got[got.len() - names.len()..];
        for (p, name) in tail.iter().zip(names) {
            assert_eq!(p, &PathBuf::from(name));
            assert!(
                p.parent().is_none_or(|d| d.as_os_str().is_empty()),
                "bare-name candidates must have no directory component: {p:?}"
            );
        }
    }

    #[test]
    fn the_platform_filename_list_leads_with_the_versioned_name() {
        // Distributions ship the unversioned name only as a `-dev` symlink, so
        // the versioned one is what an end user actually has installed.
        let names = library_filenames();
        assert!(!names.is_empty());
        if cfg!(target_os = "macos") {
            assert_eq!(names[0], "libtesseract.5.dylib");
        } else if cfg!(windows) {
            assert_eq!(names[0], "libtesseract-5.dll");
        } else {
            assert_eq!(names[0], "libtesseract.so.5");
        }
    }

    #[test]
    fn search_dirs_are_deduplicated() {
        let dirs = search_dirs();
        let mut seen = HashSet::new();
        for d in &dirs {
            assert!(seen.insert(d.clone()), "duplicate search dir: {d:?}");
        }
    }

    #[test]
    fn every_attempt_is_reported_with_a_reason() {
        // Whatever this machine has, `doctor` must be able to explain the
        // outcome of the search; an empty trail would print nothing at all.
        let trail = attempts();
        assert!(!trail.is_empty());
        let loaded = trail
            .iter()
            .filter(|a| matches!(a.outcome, Outcome::Loaded))
            .count();
        assert!(loaded <= 1, "at most one candidate can be the loaded one");
        assert_eq!(loaded == 1, available());
        if loaded == 1 {
            assert!(
                matches!(trail.last().map(|a| &a.outcome), Some(Outcome::Loaded)),
                "the search stops at the first success"
            );
        }
    }

    #[test]
    fn engine_diagnostics_are_off_unless_asked_for() {
        use std::ffi::OsStr;
        // The default has to be quiet: the noise this suppresses is per-page
        // engine chatter that reads, to anyone who has not seen Tesseract
        // before, like their install is broken.
        assert!(!debug_requested(None));
        assert!(!debug_requested(Some(OsStr::new(""))));
        // ...and an explicit 0 means off, not "a value is present, so on".
        assert!(!debug_requested(Some(OsStr::new("0"))));
        assert!(debug_requested(Some(OsStr::new("1"))));
        assert!(debug_requested(Some(OsStr::new("true"))));
    }

    #[test]
    fn null_device_is_the_one_this_platform_has() {
        // A path Tesseract cannot open is worse than no redirect at all: it
        // retries the open per message. Windows has no /dev/null, and Unix has
        // no NUL, so this constant is not interchangeable between them.
        let device = NULL_DEVICE.to_str().unwrap();
        if cfg!(windows) {
            assert_eq!(device, "NUL");
        } else {
            assert_eq!(device, "/dev/null");
            assert!(std::path::Path::new(device).exists());
        }
        assert_eq!(DEBUG_FILE_VAR.to_str().unwrap(), "debug_file");
    }
}
