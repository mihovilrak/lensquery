# Releasing and packaging

Everything here is driven by [`cargo-dist`](https://opensource.axo.dev/cargo-dist/).
Two files hold the whole configuration:

- [`dist-workspace.toml`](../dist-workspace.toml) — targets, installers, tap.
- [`.github/workflows/release.yml`](../.github/workflows/release.yml) —
  **generated**. Never hand-edit it; regenerate instead.

```console
$ dist generate      # after any edit to dist-workspace.toml
$ dist plan          # what a release would produce, without building it
```

The release workflow also runs on every pull request in plan-only mode, so a
stale `release.yml` fails CI rather than surfacing at tag time.

## Cutting a release

1. `CHANGELOG.md` — move `[Unreleased]` into a real version heading with a
   date. `cargo-dist` reads the section for the version being tagged and uses
   it as the GitHub Release body. An empty section means an empty release page.
2. Bump `version` in `Cargo.toml`, then `cargo check` so `Cargo.lock` follows.
3. Run the gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D
   warnings`, `cargo test`.
4. Commit, then tag and push:
   ```console
   $ git tag v0.1.0
   $ git push origin main --tags
   ```
5. The `Release` workflow builds all five targets, uploads the archives,
   installers, and checksums, and creates the GitHub Release.

The tag must match the version in `Cargo.toml`. `dist` refuses to run
otherwise, which is the intended behaviour — a mismatched tag is how you end up
with a `v0.2.0` release containing a `0.1.0` binary.

## What a release contains

Per target, a `.tar.xz` (`.zip` on Windows) holding both binaries — `lq` and
`lensquery` — plus `README.md`, `CHANGELOG.md`, and `LICENSE`. Alongside them:

- `lensquery-installer.sh` / `lensquery-installer.ps1` — fetch the right
  archive for the machine and unpack it into the Cargo bin directory.
- `lensquery.rb` — the Homebrew formula (see below).
- `sha256.sum` plus a `.sha256` next to every artifact.
- `source.tar.gz` and its checksum.

### Targets

| Triple | Notes |
| --- | --- |
| `x86_64-pc-windows-msvc` | |
| `x86_64-unknown-linux-gnu` | |
| `aarch64-unknown-linux-gnu` | cross-compiled |
| `x86_64-apple-darwin` | |
| `aarch64-apple-darwin` | |

**No musl target.** A statically linked musl binary gets a stub `dlopen` that
always fails, and every OCR call in this program goes through `dlopen`
(see [ADR 0005](decisions/0005-runtime-dlopen-tesseract.md)). Such a build
would compile cleanly, ship, and then report "no Tesseract library found" on
every machine — a worse outcome than not offering it. The glibc build covers
the same users.

**Nothing bundles `libtesseract`.** It would pull in Leptonica and its image
codecs, multiply the license surface, and turn one binary into a packaging
project. The runtime `dlopen` design is what makes this a choice rather than a
constraint: the release builders need no Tesseract at all, on any platform.
Revisit only if install friction becomes the top complaint.

### The `dist` profile

`[profile.dist]` in `Cargo.toml` inherits `release` and overrides nothing.
`cargo-dist`'s own template downgrades to `lto = "thin"` to save CI minutes;
we do not, because then the binary handed to users would be a different build
from the one `cargo install lensquery` produces. `panic = "unwind"` is
inherited and must stay — the per-image `catch_unwind` in the indexer is inert
under `abort`.

## Channels

### cargo

The reference channel. `cargo install lensquery` builds from source with the
`release` profile and needs nothing but a Rust toolchain — Tesseract is not a
build dependency.

### The installer scripts

Second-best default, and the one the README leads with for people without a
Rust toolchain. They are generated, versioned, and hash-checked by `dist`; there
is nothing to maintain.

### Homebrew

The formula is **generated as a release artifact but not published
automatically**. Publishing needs a tap that does not exist yet, and
`publish-jobs = ["homebrew"]` fails the entire release when the tap or its
token is missing — a broken release for a channel nobody has asked for yet is a
bad trade.

To turn it on, once:

1. Create a public repo `mihovilrak/homebrew-tap`.
2. Create a fine-grained PAT with `contents: write` on that repo and add it to
   this repo's secrets as `HOMEBREW_TAP_TOKEN`.
3. In `dist-workspace.toml`, add `publish-jobs = ["homebrew"]` to the `[dist]`
   table.
4. `dist generate`, commit both files.

From then on each tag pushes an updated formula and
`brew install mihovilrak/tap/lensquery` works. The formula already carries
`depends_on "tesseract"`, which makes Homebrew the only channel where one
install command leaves you with working OCR.

### winget

Not set up. It needs a manifest PR to
[`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs) with a
stable download URL and hash — both of which exist as soon as the first release
is published. After that it can be automated by adding
[`vedantmgoyal/winget-releaser`](https://github.com/vedantmgoyal9/winget-releaser)
as a job triggered on release publish.

winget has no dependency mechanism, so the Tesseract requirement has to be
stated in the release notes; `lq doctor` is what catches it on the machine.

### Distro packaging (AUR, deb, rpm, nixpkgs)

Deliberately not pursued. Ship a good tarball and the installer script;
packagers appear when there are users, and they maintain their packages better
than an upstream guessing at each distro's conventions. Adding five packaging
channels before there is one user is a pre-launch time sink.

Everything on this list except cargo, the installers, and Homebrew is gated on
**one real user asking for it**.

## Repository metadata

Two places carry it, and they are not interchangeable: `Cargo.toml` is what
crates.io and docs.rs read, the GitHub side is what a browser and GitHub search
read. Keep them saying the same thing.

`Cargo.toml` already has `description`, `license`, `repository`, `homepage`,
`readme`, `keywords`, and `categories`. Note that **crates.io caps `keywords` at
five**, which is why the list is `ocr`, `search`, `tesseract`, `fts5`, `cli`
rather than everything that fits — `rust` and `offline` are implied by the
listing itself and by the categories, so they are the two that got cut.

The GitHub description and topics are set once, after the repository exists:

```console
gh repo edit mihovilrak/lensquery \
  --description "Offline full-text search over the text inside your images. One binary, SQLite FTS5, no server." \
  --add-topic ocr --add-topic tesseract --add-topic sqlite --add-topic fts5 \
  --add-topic cli --add-topic rust --add-topic search --add-topic offline
```

GitHub topics have no five-item cap, so the full list goes there.
