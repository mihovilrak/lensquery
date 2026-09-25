# LensQuery

Search the text *inside* your images, offline.

LensQuery OCRs a directory of images and puts the recovered text into a local
SQLite FTS5 index, so `lq search "invoice total"` finds the photo of the invoice
on your disk. One binary. No server, no daemon, no account, no network — your
images and their text never leave the machine.

**In any language Tesseract can read** — over 100 of them, Croatian, Polish,
Turkish, Thai, Arabic, and Chinese among them. That is the sharpest difference
from the photo managers that have added OCR search: they cover a handful of
languages, so for most of the world the search you actually want does not exist
yet. Here it is one `--lang` away.

It was built for the case where the thing you remember is the *text in the
picture*: a screenshot of an error message, a photographed receipt, a
whiteboard, a scanned letter. So the audience is concrete — people with
thousands of screenshots, photographed documents, receipts, or scans sitting on
a laptop, who want to find one of them and do not want to run a server to do it.

```console
$ lq index ~/Pictures
indexed 19839 images, 0 failed (1.39 h)

$ lq search "deployment settings"
/home/me/Pictures/deployment_notes.png
/home/me/Pictures/server_settings.jpg
```

## Install

LensQuery needs a **Tesseract 5** shared library on the machine, plus the
language packs you want. It is loaded at runtime, so installing LensQuery does
not require Tesseract to be present at build time — but OCR will not work until
it is. (Searching an index you already built works without it.)

```console
# Tesseract — Windows
winget install UB-Mannheim.TesseractOCR   # tick "Additional language data"

# Tesseract — macOS
brew install tesseract tesseract-lang

# Tesseract — Debian/Ubuntu
sudo apt install tesseract-ocr tesseract-ocr-hrv

# LensQuery
cargo install lensquery
```

Then check the install:

```console
$ lq doctor
```

`doctor` prints the Tesseract library it resolved and **every path it tried on
the way there**, the available language packs, the database path and schema
version, and the applied PRAGMAs. It is the first thing to run when something is
wrong, and the right thing to paste into a bug report.

If the library lives somewhere unusual, point at it directly:

```console
$ LENSQUERY_TESSERACT_LIB=/opt/custom/lib/libtesseract.so.5 lq doctor
```

### Prebuilt binaries

No Rust toolchain, no compile. Every tagged release carries archives for
Windows, macOS and Linux (x86-64 and arm64) plus their SHA-256 sums:

```console
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/mihovilrak/lensquery/releases/latest/download/lensquery-installer.sh | sh

# Windows
powershell -c "irm https://github.com/mihovilrak/lensquery/releases/latest/download/lensquery-installer.ps1 | iex"
```

Both drop `lq` and `lensquery` into your Cargo bin directory (`~/.cargo/bin`,
`%USERPROFILE%\.cargo\bin`) and add it to `PATH` if it is not already there.
Or take the archive from the [releases
page](https://github.com/mihovilrak/lensquery/releases) and put the binary
wherever you like — it depends on nothing but your system libc.

The archives do **not** contain Tesseract. It stays a separate install, on
purpose: bundling it would drag in Leptonica and its image codecs, and it is
the one piece your package manager already handles well. Homebrew is the
exception — `brew install mihovilrak/tap/lensquery` pulls Tesseract in as a
dependency.

### Two names, one program

The crate is `lensquery` because `lq` was already taken on crates.io. Installing
it gives you **two identical executables**: `lq` (the short one, used throughout
these docs) and `lensquery`.

If some other tool on your `PATH` already provides an `lq`, whichever comes first
wins, and LensQuery cannot detect that for you — use `lensquery` instead, or
alias it. See
[docs/decisions/0004-crate-name-lensquery.md](docs/decisions/0004-crate-name-lensquery.md).

## Commands

### `lq index <directory>`

OCRs every supported image under `<directory>`, recursively, and writes the text
into the index. Incremental by default: a file whose modification time matches
what is already stored is skipped.

```console
lq index ~/Pictures
lq index ~/Pictures --dry-run               # how many, and how long, before you commit
lq index ~/Pictures --full-reindex          # re-OCR everything
lq index ~/Pictures --lang eng --workers 4 -v
lq index ~/Pictures --min-conf 60 --thorough
lq index ~/Pictures --failed-log failed.txt
```

| Flag | Default | Description |
| --- | --- | --- |
| `--db` | `~/.lensquery/index.db` | Database file location |
| `--full-reindex` | off | Re-OCR every file even if its mtime is unchanged |
| `--dry-run` | off | Print the counts and an estimated time, then exit without writing |
| `--lang` | `eng` | Tesseract language string |
| `--workers` | 0 (auto) | Parallel OCR workers; 0 picks the pool size for you |
| `--min-conf` | 40 | Word-confidence floor, 0–100. Words below it are dropped; `0` keeps everything |
| `--thorough` | off | Re-read near-empty images with a second segmentation mode |
| `--no-thorough` | — | Force it off even if the config file turns it on |
| `--thorough-trigger-words` | 3 | Word count at or below which `--thorough` re-reads a page |
| `--failed-log <path>` | — | Write every failed image and its reason to a file |
| `-v, --verbose` | off | Detailed progress |

**A long run is resumable, and `--dry-run` tells you how long it is.** Every
batch of 16 images commits in its own transaction, so an `lq index` that is
killed at 90% keeps that 90%: re-run the same command and the finished files are
skipped on mtime. `--dry-run` answers the question you have before starting —

```console
$ lq index ~/Pictures --dry-run
Dry run — nothing was OCR'd and nothing was written.
  would OCR     31487 images (31487 new, 0 changed)
  would skip    0 unchanged

  estimated     65m to 3h 4m on 8 workers
                0.79 s/image, timed on 16 of your own images
```

The counts are exact. The time is a range, because it comes from timing however
many of your own images fit in a twelve-second budget and scaling that by a
parallel-speedup curve fitted to one machine. Fewer images timed means a wider
range: a machine slow enough to run out of budget early says so by being vague
rather than by being confidently wrong. The rate above is a fair reading — a
19,839-image run on that machine really averaged 0.84 s/image against the 0.79
sampled here — but rough is the point. It is the difference between "go get
coffee" and "run it overnight".

**`--min-conf` is a real trade, and no setting wins on both axes.** Measured
against a hand-labelled corpus:

| `--min-conf` | findability | precision |
| --- | --- | --- |
| 0 | 0.694 | 0.841 |
| **40 (default)** | **0.674** | **0.905** |
| 60 | 0.657 | 0.930 |

Lower it if you would rather search a noisier index than miss a file. Raise it
if junk hits bother you more than misses do.

**`--thorough` buys +2.1% findability for +26% indexing time.** That is a bad
default and a good option: worth it for a one-off pass over a small, important
folder you will search for years; not worth it for a nightly incremental run.

More numbers, and the reasoning behind every default, in
[docs/benchmarks.md](docs/benchmarks.md).

### `lq watch <directory>`

Keeps an index current instead of rebuilding it. Arms a filesystem watcher on
`<directory>`, waits for it to go quiet, then hands whatever changed to the
same indexing code `lq index` uses — so the two cannot disagree about what
counts as an image or as changed.

```console
$ lq watch ~/Screenshots --db ~/.lensquery/index.db
watching /home/me/Screenshots (4 workers, 3.0s debounce) — Ctrl-C to stop
1 indexed, 0 updated, 0 skipped, 0 failed (in 0.4s)
moved: /home/me/Screenshots/shot1.png -> /home/me/Screenshots/receipt.png
```

| Flag | Default | Description |
| --- | --- | --- |
| `--db` | `~/.lensquery/index.db` | Database file location. Must be **outside** the watched directory |
| `--debounce` | 3.0 | Seconds of quiet before a batch is indexed (0.1–300) |
| `--workers` | half the cores | Lower than `index` on purpose: a watcher runs while you work |
| `--lang`, `--min-conf`, `--thorough`, `--no-thorough`, `--thorough-trigger-words`, `-v` | — | As `lq index` |

A move inside the watched tree costs one `UPDATE` and no OCR. A deleted file
drops out of the index. Everything is printed to stderr, so `lq watch ~/shots
2>>watch.log` is the whole of "run it and keep a log".

**It runs in the foreground and does not daemonize.** Every platform already
has a supervisor that handles restarts and logs better than a flag would;
[docs/watch.md](docs/watch.md) has a working unit file for systemd, launchd,
and Task Scheduler, plus the debounce and rename details.

Run `lq index` once after any downtime — changes made while the watcher was
not running are invisible to it.

### `lq search <query>`

```console
lq search "invoice total"
lq search "invoice" --no-snippet     # paths only
lq search "invioce" --fuzzy          # typo tolerance
lq search "invoi" --substring        # infix match, see the caveat
lq search "invoice total" --open     # open the top hit in the system viewer
lq search "invoice" --json | jq -r .path
```

| Flag | Default | Description |
| --- | --- | --- |
| `--db` | `~/.lensquery/index.db` | Database file location |
| `--limit` | 20 | Maximum results |
| `--open` | off | Open the best match with the OS default handler |
| `--fuzzy` | off | Tolerate typos and OCR misreads (see below) |
| `--fuzzy-distance` | by word length | Edit budget per word; `0` disables widening |
| `--substring` | off | Trigram infix matching (see below) |
| `--snippet` / `--no-snippet` | on | Append the matched excerpt, match in `[brackets]` |
| `--json` | off | JSON Lines on stdout, one object per hit |
| `-v, --verbose` | off | Show match scores |

Output is one result per line, tab-separated, and stays greppable whatever you
switch on:

```text
[<score>\t]<path>[\t<snippet>]
```

```console
$ lq search "deployment"
/home/me/Pictures/deployment_notes.png	[Deployment] Settings Overview Region...
```

**Two different kinds of approximate.** They answer different questions and
clap will not let you ask both at once.

`--fuzzy` is typo tolerance. It walks the index's own term dictionary, keeps
the words within a small edit distance of what you typed, and searches for
those as well — so `"invioce"` finds `"invoice"`. The budget scales with word
length (nothing under 4 characters, one edit up to 6, two above that) and
`--fuzzy-distance N` overrides it; `0` turns the widening off entirely.
Confusions OCR actually makes are free on top of that budget: `1` for `l`, `0`
for `o`, `rn` for `m`, so a scan that came out `1nvo1ce` is still one word away
from `invoice` rather than three. A quoted `"phrase"` is never widened — asking
for exact words in an exact order is the one case where guessing is wrong.
[docs/decisions/0007-fuzzy-vocabulary-walk.md](docs/decisions/0007-fuzzy-vocabulary-walk.md)
has the design.

`--substring` is the trigram index: `"nvoi"` finds `"invoice"` from the middle
of the word, which no amount of typo tolerance will do. Its snippet brackets
mark the matched *substring* (`i[nvoic]e`) rather than the whole word, because
that is what actually matched.

### `lq serve`

Answers queries from stdin in one warm process: one result block per query,
terminated by a blank line.

A one-shot `lq search` spends 21–36 ms on process startup and 0.5–5 ms on the
actual search. If you are issuing queries from a script, a launcher, or an
editor plugin, `serve` pays that startup once:

```console
$ printf 'invoice\n:limit 5\ntotal\n:quit\n' | lq serve
```

Mid-session directives: `:limit N`, `:fuzzy on|off`, `:substring on|off`,
`:snippet on|off`, `:quit`.
Everything else is a query. A malformed query prints an error and the session
continues — it does not tear down a process the client is still talking to.
`--db`, `--limit`, `--fuzzy`, `--fuzzy-distance`, `--substring`, `--snippet`,
and `-v` set the starting state.

### `lq status`

File count, database size, and last-indexed time, as `key: value` lines, or as
one JSON object with `--json`.

### `lq compact`

`PRAGMA optimize` followed by `VACUUM`. **This is a space feature, not a speed
feature** — it reclaims about 3% on a fresh index and about 10% on a long-lived
churned one, and its effect on query latency is nil. Run it after deleting or
re-indexing a lot of files.

### `lq doctor`

Environment diagnostics: the resolved Tesseract library and every candidate path
tried, in order; language packs; `TESSDATA_PREFIX`; the directory `lq lang add`
installs into; database path; schema version; applied PRAGMAs; version. The
thing to paste into a bug report. `--json` gives the same facts as one object.

### Machine-readable output

`search`, `status`, and `doctor` take `--json`. `search` emits JSON Lines so a
long result set streams; the other two emit a single object. In `--json` mode
stdout carries JSON and nothing else — every human line goes to stderr, so an
empty result set is an empty stream, not a stray sentence in your pipe.

```console
$ lq search "invoice" --json | jq -r 'select(.score < -1) | .path'
```

The schema, and what is and is not stable about it, is in
[docs/json-output.md](docs/json-output.md).

### `lq lang`

Language packs, without a browser. `lq lang` on its own is `lq lang list`.

| Command | What it does |
| --- | --- |
| `lq lang list` | Installed packs with their size, and the directory they are read from |
| `lq lang available [filter]` | The 125 packs in the manifest; the filter matches code or name |
| `lq lang add <code>...` | Download, verify, install |
| `lq lang remove <code>...` | Delete packs LensQuery installed (asks first; `-y` skips) |
| `lq lang path` | The directories involved, as `key: value` lines |

Downloads come from [tessdata_fast](https://github.com/tesseract-ocr/tessdata_fast)
at the pinned tag `4.1.0`, and each one is checked against a SHA-256 committed in
this repo before it is moved into place. A pack that fails verification is
deleted, never installed. Regenerate the manifest yourself with
`python scripts/build-lang-manifest.py --tag 4.1.0 -o assets/tessdata-fast-4.1.0.toml`
if you would rather not trust the copy here.

Packs install beside the system ones when that directory is writable, and into
`%APPDATA%\lensquery\tessdata` (or `~/.local/share/lensquery/tessdata`) when it is
not. Tesseract reads exactly one directory, so the first install into the fallback
would hide the system packs; `lq lang add` copies them across at that point and
says so.

`osd` is downloadable and is not a language — it detects orientation and script.
Passing it to `--lang` is rejected with an error rather than silently filtered.

**Offline.** `--offline`, or `LENSQUERY_OFFLINE=1`, turns `lq lang add` into a
message naming the file to fetch by hand and the directory to drop it in. Nothing
else in LensQuery uses the network. Building with `--no-default-features` removes
the HTTP and TLS dependencies from the graph entirely, so there is no network
stack left to disable.

Installing by hand always works and needs nothing from us: put any
`<code>.traineddata` into the directory `lq lang path` reports as
`tessdata_dir`, and `lq lang list` picks it up. That is also how you use the
slower, slightly more accurate
[tessdata_best](https://github.com/tesseract-ocr/tessdata_best) models, or a
model you trained yourself — LensQuery reads whatever Tesseract reads.

## Configuration

Optional TOML at `~/.lensquery/config.toml`. CLI flags beat the config file,
which beats the built-in defaults.

```toml
[index]
default_db = "~/.lensquery/index.db"
languages = "eng"
workers = 0                 # 0 = auto
engine = "dll"
min_word_conf = 40          # see --min-conf
thorough = false
thorough_trigger_words = 3

[search]
default_limit = 20
```

## Notes from the field

- **One language is faster than two.** Every extra pack in `--lang` costs
  model-load time and slows recognition. Use `--lang eng` when you can.
- **Keep the `tessdata_fast` models.** They are roughly twice as fast as the
  `best` tier, and on real photos they are *not* less accurate — measured null
  across three pairings, for +32.4 MB and +55% index time.
- **Indexing saturates the machine.** Expect around 4 images/second on 8 cores,
  at ~0.97 CPU per core throughout. 84% of that is Tesseract's recognizer, so no
  configuration makes it dramatically faster.
- **If a search misses, reach for `--fuzzy` first and `--substring` second.**
  OCR snaps unknown words toward dictionary words, so names, slang, and product
  codes often come back slightly wrong. `--fuzzy` catches the ones that came
  back close; `--substring` catches the ones where only a fragment survived.

## Limits

Stated here rather than in a reply to your issue:

- **OCR text only.** No semantic or CLIP-style image search. LensQuery finds
  the photo of a receipt because the word *receipt* is printed on it, not
  because the picture looks like a receipt.
- **No PDF support.** The most-asked-for thing that does not exist yet.
- **Tesseract has to be installed separately.** See [Install](#install). The
  binary itself has no other runtime dependency.
- **No GUI.** A CLI, plus `lq serve` for anything that wants to drive it.
- **Handwriting is mostly a miss.** Tesseract is built for print.
- **Indexing is slow the first time**, because OCR is slow — roughly 4 images
  per second on 8 cores. Incremental runs after that are fast.

## Supported input

PNG, JPEG, TIFF, BMP, GIF, and WebP — anything the [`image`] crate decodes and
Tesseract can read.

## Development

```console
just check      # lint + test + privacy scan — this is CI
just test       # cargo test: unit tests and the serve integration tests
just recall     # OCR the committed fixtures, assert the text comes back
just doctor     # what a user sees when their install is broken
```

[CLAUDE.md](CLAUDE.md) has the conventions and the hard rules,
[docs/decisions/](docs/decisions/) has the reasoning behind the design, and
[docs/troubleshooting.md](docs/troubleshooting.md) covers the known failure
modes. Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT — see [LICENSE](LICENSE).

Language packs fetched by `lq lang add` are not part of this project. They come
from [tessdata_fast](https://github.com/tesseract-ocr/tessdata_fast) and are
licensed Apache-2.0 by the Tesseract authors; nothing is bundled in the binary
or in this repository except their names, sizes, and checksums.

[`image`]: https://crates.io/crates/image
