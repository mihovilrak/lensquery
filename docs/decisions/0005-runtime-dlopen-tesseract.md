# 0005 — Tesseract is loaded at runtime, not linked at build time

- **Status:** Accepted
- **Date:** 2026-08-17

## Context

LensQuery needs Tesseract's C API to turn images into text. There are two ways
to get at it:

1. **Link at build time** — a `build.rs` that finds `libtesseract`, emits
   `cargo:rustc-link-lib=tesseract`, and lets the linker resolve the symbols.
   This is what the `leptess`/`tesseract-sys` crates do.
2. **Load at runtime** — `dlopen`/`LoadLibrary` the shared library on first use
   and resolve each symbol by name.

Option 1 makes `cargo install lensquery` fail on any machine without Tesseract
development headers and a discoverable library, with a linker error that names
`-ltesseract` and nothing else. That is a bad first impression for a tool whose
entire pitch is "one binary." It also means the published crate cannot be built
in a plain CI container, and that every packaging channel (cargo, winget, brew)
inherits a build-time system dependency.

It also gets the failure *timing* wrong. Tesseract is only needed for `lq index`
and `lq recall`. `lq search`, `lq stats`, `lq serve`, and `lq doctor` all work
against an already-built index and need no OCR at all. Build-time linking makes
a search-only user pay for a dependency they never call.

The catch with option 2 is discovery: nothing resolves the library for us, so
we have to reproduce a plausible search ourselves, and when it fails the user
is owed an explanation better than "not found."

## Decision

Load `libtesseract` at runtime via `libloading`, in [`src/tess.rs`](../../src/tess.rs).

**Resolution order.** `LENSQUERY_TESSERACT_LIB`, if set, *replaces* the search
rather than extending it — an explicit path that silently falls through to a
different library is worse than an error. Otherwise the search is
directory-major: every candidate filename is tried in one directory before
moving to the next, because directory order encodes priority (next to our own
binary beats a system prefix) while filename order only reflects packaging
convention. Directories, in order:

- the directory holding our own executable (the bundled-DLL case on Windows),
- the install prefix inferred from `tesseract` on `$PATH` (`<prefix>/lib`, then
  the `bin` directory itself on Windows),
- the platform's usual library directories — `/opt/homebrew/lib`,
  `/usr/local/lib`, `/usr/lib` on macOS; `/usr/lib/<triple>`, `/usr/lib64`,
  `/usr/local/lib`, `/usr/lib` on other Unixes; none on Windows, which has no
  such convention.

Bare filenames are appended last, so `LD_LIBRARY_PATH`, the dyld shared cache,
and the Windows DLL search order still get a say after our explicit list is
exhausted.

**Filenames** are per-platform and lead with the versioned SONAME, which is what
package managers actually install: `libtesseract.so.5` before `libtesseract.so`,
`libtesseract.5.dylib` before `libtesseract.dylib`. The unversioned name is
usually a `-dev`/`-devel` symlink and is frequently absent on a machine that can
nonetheless run Tesseract fine. Windows gets `libtesseract-5.dll` plus the
`tesseract5x.dll` names older installers used.

**Windows keeps `LOAD_WITH_ALTERED_SEARCH_PATH`** so a bundled
`libtesseract-5.dll` resolves its own siblings (leptonica, the zlib/png stack)
from its own directory instead of ours. That flag is documented as undefined
behaviour with a relative path, so relative candidates take the plain
`Library::new` path instead.

**Every attempt is recorded**, not just the one that won. `tess::attempts()`
returns the full trail — each candidate path with one of `NotFound`,
`OpenFailed(message)`, `MissingSymbols`, or `Loaded` — and `lq doctor` prints
it. The distinction matters: a 32-bit library on a 64-bit process and a library
whose own dependencies are missing both *look* like "Tesseract isn't installed"
to a user, and both are fixed by something other than installing Tesseract.
`MissingSymbols` exists for the case where the file loads but is not Tesseract.

The whole thing sits behind a `OnceLock<(Option<TessLib>, Vec<Attempt>)>` — one
lock, not two, so the trail is guaranteed to describe the load that actually
happened rather than a later re-derivation of it.

## Consequences

- `cargo install lensquery` succeeds on a machine with no Tesseract. The failure
  moves from link time to first OCR, where `lq doctor` can explain it, print the
  full search trail, and name the install command for the platform.
- Symbol resolution is by string at runtime. A future Tesseract that renames or
  drops a symbol we use surfaces as `MissingSymbols` instead of a compile error.
  This is the real cost of the decision. It is bounded by only using the stable
  C API (`TessBaseAPI*`, `TessResultIterator*`), which has been stable across
  the 4.x and 5.x lines.
- Every FFI call is `unsafe` and hand-written, including the signatures. A
  wrong signature is a silent ABI mismatch, not a type error. The signatures are
  in one place and are not to be edited casually.
- The search is a heuristic and will miss unusual layouts. That is what
  `LENSQUERY_TESSERACT_LIB` is for, and `lq doctor` advertises it whenever the
  search comes up empty.
- The ordering rules are tested as a pure function (`candidates_with`) over an
  explicit-override option and a directory list, so they can be asserted without
  mutating process environment — concurrent `setenv` is a data race under
  parallel test threads — and without a filesystem that has Tesseract on it.

## Reconsider when

- A distribution channel needs a fully static binary with OCR built in. That is
  a different build (vendored Tesseract + Leptonica) and a different decision,
  not a tweak to this one.
- The C API stops being stable enough that string-resolved symbols are a real
  maintenance burden rather than a theoretical one.
