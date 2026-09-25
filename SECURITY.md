# Security Policy

## Reporting a vulnerability

Report privately through GitHub's **Report a vulnerability** button on the
Security tab of this repository, which opens a private advisory. Do not open a
public issue for a security problem.

Expect an acknowledgement within a week. This is a spare-time project, so there
is no formal SLA — but security reports go to the front of the queue, and you
will be told plainly if something will not be fixed.

## Supported versions

Only the latest released version. Pre-1.0, fixes land in a new `0.x` release
rather than being backported.

## Threat model

LensQuery is a local, offline CLI. It has no server, no network listener, no
account, and no telemetry — the index and the text never leave the machine.
That removes most of the usual surface, and leaves four real things:

**It reads arbitrary user images.** Every file under the indexed directory is
decoded by the [`image`] crate and handed to Tesseract, both of which parse
untrusted binary input in memory-unsafe or partially memory-unsafe code. A
malicious image is the most plausible attack path in the whole program. The
indexer wraps each image in `catch_unwind` so one bad file does not kill a run,
but that is robustness, not a sandbox: it does not contain a memory-safety bug
inside libtesseract or a native decoder. Index directories you control.

**It builds SQL from user input.** Search terms become FTS5 `MATCH` queries.
All input routes through `db::build_fts5_query`, which quotes and escapes
tokens; values are bound as parameters and never formatted into SQL text. A
path around that function is a bug worth reporting even if you cannot
demonstrate an exploit with it.

**It loads a shared library at runtime.** libtesseract is resolved with
`dlopen` from an ordered candidate list, and `LENSQUERY_TESSERACT_LIB`
overrides that search entirely. Anyone who can write to a candidate directory,
or set that variable in your environment, can get their code loaded into the
process. This is the normal property of dynamic linking, not a bug, but it does
mean the library search path is trust-sensitive: do not run LensQuery with a
`LENSQUERY_TESSERACT_LIB` you did not set yourself.

**It stores recovered text in a plain SQLite file.** `~/.lensquery/index.db`
holds the OCR text of every indexed image in the clear, which can be far more
sensitive than the images themselves are on disk — text is searchable, images
are not. It is protected by nothing but filesystem permissions. On a shared or
backed-up machine, treat the index with the same care as the pictures.

## Out of scope

- Denial of service from pointing the indexer at a huge or hostile directory.
  It is a local tool run by its own user; consuming your own CPU is the
  documented behaviour.
- Anything requiring an attacker who already has code execution as your user.
- The `lq` binary-name collision with the unrelated `lq` crate on `$PATH`. It
  is documented in the README and in
  [docs/decisions/0004-crate-name-lensquery.md](docs/decisions/0004-crate-name-lensquery.md);
  install the `lensquery` binary instead if it matters to you.

[`image`]: https://crates.io/crates/image
