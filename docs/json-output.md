# JSON output

`lq search`, `lq status`, and `lq doctor` take `--json`. These are the three
commands whose output someone will want to parse; everything else is either an
action or already a single number.

This document is part of the public API surface. **Adding a field is
backward-compatible and can happen in any release. Renaming or removing one is
a breaking change** and gets a `CHANGELOG.md` entry — the same discipline the
Rust structs in `models.rs` are under.

## Two shapes

| Command | Shape |
| --- | --- |
| `search --json` | **JSON Lines** — one object per result, one per line, no enclosing array |
| `status --json`, `doctor --json` | one object, one line |

JSON Lines for `search` because a result set is a stream: `lq search invoice
--limit 10000 --json \| jq -r .path` starts printing on the first hit instead
of buffering the whole array. Every line is a complete JSON document, so a
reader never has to track brackets.

## Only JSON on stdout

With `--json`, stdout carries JSON and nothing else. Human text — including
`No results found.`, which the plain form prints to stdout — moves to stderr.
An empty result set is therefore an **empty stdout stream and exit 0**, not an
empty array and not an error. `--json` and `-v/--verbose` do not fight:
`--verbose` adds nothing to the JSON, because `score` is always present there.

```console
$ lq search zzzz --json > hits.jsonl
No results found. Try --fuzzy for approximate matching.
$ wc -l < hits.jsonl
0
```

## `search --json`

```json
{"path":"/home/you/receipts/march.png","score":-1.4271,"mtime":1755419040.0,"lang":"eng","snippet":"total due on this [invoice] is"}
```

| Field | Type | Notes |
| --- | --- | --- |
| `path` | string | absolute path as indexed |
| `score` | number | FTS5 `rank` (BM25). **More negative is better**, and results are already sorted best-first |
| `mtime` | number | the indexed file's modification time, fractional Unix seconds |
| `lang` | string or null | the Tesseract language string that produced the text. `null` means the row was indexed before `lq` recorded it — unknown, not "none" |
| `snippet` | string | the matched excerpt, whitespace-collapsed, with the match wrapped in `[`…`]`. **Absent** (not null) under `--no-snippet` |

`snippet` is the only key that varies with the flags, and it is omitted rather
than set to `null` so `jq 'has("snippet")'` answers the question directly.

Field order is fixed and matches the table. Nothing should depend on key order
in JSON, but it is stable, and holding it stable is what keeps the output
diffable.

```bash
# every path, best match first
lq search invoice --json | jq -r .path

# only hits above a score threshold
lq search invoice --json | jq -r 'select(.score < -1.0) | .path'

# what language read each hit
lq search invoice --json | jq -r '[.lang // "unknown", .path] | @tsv'
```

## `status --json`

Flat — the same fields the `key: value` form prints, in the same order, not
nested under a `stats` object.

```json
{"db_path":"/home/you/.lensquery/index.db","file_count":1842,"db_bytes":39428096,"last_indexed_at":"2026-08-18T09:12:44Z","schema_version":2}
```

| Field | Type | Notes |
| --- | --- | --- |
| `db_path` | string | the index the command resolved to |
| `file_count` | number | rows in `files` |
| `db_bytes` | number | on-disk size (`page_count * page_size`) |
| `last_indexed_at` | string or null | ISO-8601; `null` on an empty index |
| `schema_version` | number or null | `null` if the `meta` row is missing |

## `doctor --json`

The same facts as the text form, gathered once and rendered twice, so the two
can never describe different machines.

```json
{
  "lq_version": "0.1.0",
  "platform": "linux x86_64",
  "libtesseract": "/usr/lib/x86_64-linux-gnu/libtesseract.so.5",
  "libtesseract_search": [
    {"path": "/usr/lib/x86_64-linux-gnu/libtesseract.so.5", "outcome": "OK — using this one"}
  ],
  "hints": [],
  "tessdata_path": "/usr/share/tesseract-ocr/5/tessdata",
  "tessdata_install_dir": "/home/you/.local/share/lensquery/tessdata",
  "tesseract_langs": ["eng", "osd"],
  "engine_default": "auto",
  "languages_default": "eng",
  "min_word_conf_default": 40.0,
  "thorough_default": false,
  "thorough_trigger_words_default": 3,
  "db_path": "/home/you/.lensquery/index.db",
  "db_status": "ok",
  "stats": {"file_count": 1842, "db_bytes": 39428096, "last_indexed_at": "2026-08-18T09:12:44Z", "schema_version": 2}
}
```

(Pretty-printed here for reading; `lq` emits it on one line.)

| Field | Type | Notes |
| --- | --- | --- |
| `lq_version` | string | crate version |
| `platform` | string | `"<os> <arch>"` |
| `libtesseract` | string or null | the library actually loaded; `null` when none did |
| `libtesseract_search` | array | every candidate tried, **in order**, as `{path, outcome}`. `outcome` is prose meant for a human — do not match on it |
| `hints` | array of string | what to do about a failed load. Empty when the library loaded |
| `tessdata_path` | string or null | what Tesseract reads; `null` when unset |
| `tessdata_install_dir` | string or null | where `lq lang add` would write |
| `tesseract_langs` | array of string | installed language packs. Includes `osd`, which is **not a language** and must not go in `--lang` |
| `engine_default` … `thorough_trigger_words_default` | | resolved config defaults, before per-command flags |
| `db_path` | string | the index the command resolved to |
| `db_status` | string | `"ok"` or `"not_initialized"` |
| `stats` | object or null | the `status` fields, or `null` when the index does not exist yet |

`doctor` exits 0 whether or not the environment is healthy — it is a report,
not a check. Test `db_status`, or `libtesseract != null`, to decide.

## Stability

- Key names and their meanings: stable, breaking changes documented.
- Key order: stable, but do not depend on it.
- `outcome` and `hints` strings: **not** stable. They are English prose for a
  human reading a bug report.
- Floating point is emitted as JSON numbers, so `mtime` and `score` come back
  as doubles. `mtime` is fractional seconds — do not assume an integer.
