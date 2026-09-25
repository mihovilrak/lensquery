# 0004 — Crate is `lensquery`, binary stays `lq`

- **Status:** Accepted
- **Date:** 2026-08-17

## Context

The tool has been called `lq` throughout its development: the command in every
doc, every test, and the author's shell history. Publishing to crates.io
requires a crate name, and `lq` is already taken by an unrelated package.

Crate name and binary name are independent in Cargo, so the collision does not
force the command to be renamed — but it does force a decision about what
appears in `cargo install <name>`, in `use <name>::…`, and on the user's
`$PATH`.

A second, unrelated collision exists downstream: `lq` is a short enough name
that some users will already have something else by that name installed.

## Decision

- **Crate name:** `lensquery`. Installed with `cargo install lensquery`.
- **Library name:** `lensquery`. Public consumers write `use lensquery::db`.
- **Primary binary:** `lq`. Unchanged from every existing doc and habit.
- **Alias binary:** `lensquery`, byte-for-byte identical behaviour.

Both binaries are four-line shims over `lensquery::cli::main`, so there is one
implementation and no risk of the two drifting.

## Consequences

- `cargo install lensquery` installs *two* executables. That is mildly
  surprising, so the README says so explicitly rather than letting the user
  discover it.
- A user who already has a different `lq` gets a silent `$PATH` shadowing
  conflict — whichever directory comes first wins, in either direction. The
  README documents this and points at `lensquery` as the unambiguous name;
  `lq doctor` is not able to detect it (it cannot know what the user meant).
- The package name does not match the primary command name, so
  `cargo install lq` installs someone else's crate. Nothing can be done about
  this beyond documentation.

## Reconsider when

- The `lq` crate is ever yielded or abandoned. crates.io does not transfer
  names on request, so this is unlikely; do not plan for it.
