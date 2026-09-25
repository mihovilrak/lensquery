# Contributing to LensQuery

Contributions are welcome. This file covers the mechanics; the reasoning
behind the design lives in [docs/decisions/](docs/decisions/) and the rules
that are not negotiable live in [CLAUDE.md](CLAUDE.md).

## Build and test

```console
git clone https://github.com/mihovilrak/lensquery
cd lensquery
cargo build
just check
```

`just check` is exactly what CI runs: `cargo fmt --check`, `cargo clippy
--all-targets -- -D warnings`, `cargo test`, and the privacy scan. If it is
green locally it should be green in CI, and if it is not, that gap is itself a
bug worth reporting.

Run the tests with **`cargo test`, not `cargo test --lib`.** `--lib` silently
skips the integration tests in `tests/`, which are the only thing that catches
a regression in the `lq serve` line protocol.

Two prerequisites are easy to miss:

- **A C toolchain.** `rusqlite` is built with the `bundled` feature, so SQLite
  is compiled from source. Debian: `build-essential`. macOS: Xcode command line
  tools. Windows: MSVC Build Tools with "Desktop development with C++".
- **libtesseract, for the tests that actually OCR.** Most of the suite does not
  need it — tests that do must skip when `tess::available()` is false rather
  than fail, so a contributor without Tesseract still gets a green run.
  `just recall` is the opt-in check that OCR really works end to end.

## Before you open a PR

1. `just check` passes.
2. New behaviour has a test. New *measured* behaviour has a number in
   [docs/benchmarks.md](docs/benchmarks.md) and the method that produced it.
3. Anything that changes a signature, a schema, an exit code, or the shape of
   a line on stdout is a change to
   [docs/api-contracts.md](docs/api-contracts.md) and should be its own PR.
4. A decision that will be argued about again gets an ADR in
   `docs/decisions/NNNN-slug.md`.
5. Nothing private is in the diff. No real images, no absolute paths from your
   machine, no `.traineddata`, no database files. `just privacy` checks this
   and runs as part of `just check`.

## Things that will be sent back

- A default changed because it seemed better. Defaults here come from
  measurements; change the measurement first.
- Collapsing `Ocr::Empty` and `Ocr::Failed`. They mean different things and the
  difference is load-bearing.
- A user string reaching `MATCH` without going through `db::build_fts5_query`.
- `#[allow(...)]` without a reason. Use `#[expect(lint, reason = "...")]`, or
  fix the lint.
- Comment removal framed as cleanup. Comments here carry measured findings; the
  target is that every reference resolves, not that there are fewer words.

## The two contributions most worth making

### Adding a language

LensQuery does not ship language models — it uses whatever `tessdata` the
machine has, named through `--lang` (or `languages` in the config file). So
"adding a language" is not adding a download; it is proving the pipeline
handles that language correctly and writing down what it takes.

A good language PR contains:

- A **synthetic fixture** rendered by [scripts/make_fixtures.py](scripts/make_fixtures.py),
  never a real photo or screenshot. Extend the script so the fixture can be
  regenerated, and commit both the script change and the resulting PNG.
- A test asserting on **substrings, not exact equality**. Exact-match assertions
  against real OCR output are a flake source across Tesseract versions and
  model tiers.
- A note in the README or [docs/troubleshooting.md](docs/troubleshooting.md) if
  the language needs a package name people will not guess
  (`tesseract-ocr-hrv`, and so on).
- Evidence, if the language exposes a tokenizer problem. Non-ASCII letters that
  FTS5's `unicode61` tokenizer folds away are the usual failure, and the fix
  belongs in `db::build_fts5_query` with a test that pins it.

Remember that `osd` is not a language. It is an orientation and script
detection model, and it must never appear in a `--lang` string.

### Adding an OCR backend

Tesseract is loaded at runtime through `libloading` (see
[docs/decisions/0005-runtime-dlopen-tesseract.md](docs/decisions/0005-runtime-dlopen-tesseract.md)),
and today `src/tess.rs` is the only implementation of that seam while
`src/ocr.rs` owns the image preprocessing and the three-state result.

A second backend is a wanted contribution, but the first PR should be the
**seam, not the engine**: propose in an issue what the trait looks like, and
keep these constraints, which the current code depends on:

- The result stays three-state — text, empty, failed. An engine that cannot
  distinguish "read it, there was nothing" from "could not read it" has to say
  so explicitly rather than guessing.
- Per-word confidence, or an honest statement that the engine has none.
  `--min-conf` is a documented, measured trade and silently ignoring it is
  worse than rejecting it.
- Thread-local engine construction. One engine per worker, not one per image —
  construction costs more than the OCR does.
- No build-time hard dependency on the new engine. The default build must stay
  installable on a machine that has none of it.
- Recall measured on the committed fixtures before and after, in the PR body.
  An unmeasured second backend is worse than one backend.

## Releases

Tag-driven and generated by `cargo-dist`; the release workflow is not
hand-written. If you are changing what gets built, shipped, or packaged, the
one file to edit is `dist-workspace.toml` — then run `just dist-generate` and
commit the regenerated workflow with it. [docs/packaging.md](docs/packaging.md)
has the full procedure and the reasoning behind the target list.

## Reporting bugs

Include the exact command, the OS, and the full output of `lq doctor`. The
issue template asks for all three because they answer most reports without a
second round trip. Security issues go to [SECURITY.md](SECURITY.md) instead,
not to the public tracker.

## License

By contributing you agree that your contribution is licensed under the MIT
license, the same as the rest of the project.
