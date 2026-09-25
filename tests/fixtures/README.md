# Test fixtures

Seven synthetic images used by the recall gate (`just recall`, source in
[`examples/recall.rs`](../../examples/recall.rs)).

They are **synthetic on purpose**. Real screenshots and photos are the input
this tool was built for, but they carry whatever was on the author's screen, and
a public repository is the wrong place for that. Every pixel here is rendered
from strings in [`scripts/make_fixtures.py`](../../scripts/make_fixtures.py), so
what the generator says is exactly what is in the images.

The PNGs are committed. The generator exists so a new fixture is a diff to
source rather than a binary someone dropped in, and so a font change or a Pillow
upgrade is a reproducible event instead of a mystery. Regenerate with
`just fixtures`, then re-run `just recall` and update the table below if the
recovered text moved.

## What each fixture is for

| Fixture | Exercises |
| --- | --- |
| `hello_world.png` | The sanity case. One line, high contrast. If this fails, nothing else is worth reading. |
| `croatian_text.png` | Diacritics end-to-end: rendering → OCR → UTF-8 → FTS5 tokenizing → terminal output. |
| `invoice.png` | Document structure: headings, a right-aligned numeric column, a total. Digits and letters sharing a line. |
| `ui_panel.png` | Light text on dark chrome at UI sizes, including a monospaced field. The shape of the real corpus, and the one that most often OCRs badly. |
| `low_contrast_scan.png` | Grey on off-white with seeded paper grain and a slight blur. The fixture that notices if the `--min-conf` default starts eating real text. |
| `rotated_page.png` | Four degrees off square — enough to break naive line-finding, well inside what Tesseract's deskew handles. |
| `two_column.png` | Two columns with a rule between them. Reading order across columns is where page-segmentation choices show up. |

## Asserted phrases

Recovered with the shipping configuration — `eng+hrv`, psm 11, upscale 1200,
`--min-conf 40` — and the `tessdata_fast` models.

| Fixture | Must appear in the recovered text |
| --- | --- |
| `hello_world.png` | `Hello World` |
| `croatian_text.png` | `Dobar dan`, `izvještaja`, `Čaša`, `žuto`, `ćevapi`, `đak`, `šuma` |
| `invoice.png` | `INVOICE`, `Northwind Analytics`, `Optical character`, `1629.50` |
| `ui_panel.png` | `Deployment Settings`, `europe-west3`, `cargo build`, `Save changes` |
| `low_contrast_scan.png` | `archive`, `eleven years`, `44-119-B` |
| `rotated_page.png` | `Meeting notes`, `migration is scheduled`, `platform team` |
| `two_column.png` | `Quarterly Field Report`, `reservoirs recovered`, `boreholes were drilled` |

## Substrings, not transcripts

The gate asserts that these phrases appear, not that the whole output matches a
stored string. A rotated page or a low-contrast scan will drop a comma, split a
word, or reorder columns depending on the Tesseract build and version, and a
gate that fails on that is a gate people learn to ignore.

The phrases are picked so a real regression cannot slip through: a language pack
that stopped loading takes out the Croatian row, a confidence floor that started
eating body text takes out the low-contrast row, a segmentation mode that lost a
column takes out `two_column.png`, and a broken preprocessing step takes out
several at once.

Two things worth knowing before you edit the table:

- **Assert real words, not glyph runs.** `croatian_text.png` originally asserted
  the cluster `čćžšđ`. Tesseract scores candidates against a language model, so
  a nonsense run of accented letters comes back below the confidence floor and
  is dropped — the fixture failed for a reason that had nothing to do with
  diacritic handling. It now uses one real Croatian word per accented letter.
- **Every phrase in this table was observed, not predicted.** Run
  `cargo run --release --example recall -- tests/fixtures --dump` to see the full
  recovered text for each fixture before adding an assertion.
