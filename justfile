# LensQuery dev commands. `just --list` for the menu, `just check` before you
# open a PR — it is exactly what CI runs.

default:
    @just --list

# Everything CI checks, in the order that fails fastest.
check: lint test privacy

# Formatting and lints. Warnings are errors here; fix them rather than
# `#[allow]`-ing them unless you can say why in a comment.
lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

fmt:
    cargo fmt

# The whole suite: unit tests, plus the `lq serve` integration tests that drive
# the real binary over stdin. No `--lib` — the integration tests are the ones
# that catch protocol regressions.
test:
    cargo test

# Fail if anything private or machine-specific got committed. See
# docs/decisions/0006-rust-only-core.md for why this repo takes that seriously.
privacy:
    bash scripts/check-privacy.sh

# Release binary. Tesseract is loaded at runtime, so this links against nothing
# and the result runs on a machine with no Rust toolchain.
build:
    cargo build --release

# Install to ~/.cargo/bin from this checkout.
install:
    cargo install --path . --locked

# What the user sees when something is wrong. Run it after touching src/tess.rs.
doctor:
    cargo run --quiet --bin lq -- doctor

# Regenerate tests/fixtures/*.png. Only needed when adding a fixture; the
# checked-in PNGs are the test contract. Requires Python + Pillow.
fixtures:
    python scripts/make_fixtures.py

# Indexing throughput on DIR, staged so you can see where the time goes.
# Bring your own images — no corpus ships with this repo.
stage-profile dir:
    cargo run --release --example stage_profile -- "{{dir}}"

# OCR the checked-in fixtures and assert the pinned output. Needs a loadable
# libtesseract, which is why it is not part of `just test`.
recall dir="tests/fixtures":
    cargo run --release --example recall -- "{{dir}}"

# Dry-run the crates.io package: what would ship, and does it build clean.
package:
    cargo package --list
    cargo publish --dry-run

# What a `git tag` would publish, without building anything. Needs `dist`
# (cargo-dist) on PATH — see docs/packaging.md.
dist-plan:
    dist plan

# Regenerate .github/workflows/release.yml. Run after every edit to
# dist-workspace.toml; the workflow is generated and must not be hand-edited.
dist-generate:
    dist generate
