# Benchmarks

Every default in LensQuery was chosen by measurement, and this file is where the
measurements live. If you are about to change a default, a preprocessing step,
or a PRAGMA, read the relevant section first — there is a good chance the change
has already been scored and rejected.

## What was measured, and on what

The numbers below come from a personal photo and screenshot archive of **19,839
images** — phone photos, WhatsApp exports, screenshots, and scans, which is the
input this tool was built for and is much harder than a corpus of clean scanned
documents. Accuracy was scored against a **hand-corrected label set of 150
images** drawn from it, with paired bootstrap confidence intervals.

The corpus is private and is not part of this repository. The synthetic fixtures
in `tests/fixtures/` are a regression gate, not a benchmark — they are far
easier than real input and their scores mean nothing in absolute terms.

Hardware: 8 logical / 4 physical cores, Windows, SSD, Tesseract 5 with the
`tessdata_fast` models.

Two definitions used throughout:

- **Findability** — the fraction of labelled images that a plausible search for
  their content actually returns. This is the number that matters to a user.
- **Recall** — the fraction of the labelled words that OCR recovered. A proxy;
  it moves more than findability does and matters less.

---

## 1. Throughput: 3.96 img/s, and it is Tesseract's number

Full corpus, shipping configuration, on the schema this release uses:

| Metric | Value |
| --- | --- |
| Indexed / failed | 19,839 / 0 |
| Wall clock | ≈5,008 s (1.39 h) |
| **Throughput** | **3.96 img/s = 252 ms/image** |
| CPU | 0.966 per core across 8 cores |
| Peak RSS | 1,067 MB |
| Index size | 2,029 B/image (38.4 MiB total) |

The CPU line is the important one: the machine is saturated. There is no idle
time to reclaim and no I/O to overlap.

Where the time goes (n=1000, min-of-N per stage):

| Stage | mean ms | share |
| --- | --- | --- |
| decode | 28.49 | 3.4% |
| preprocess | 102.77 | 12.2% |
| **recognize** | **708.30** | **84.4%** |

Make decode and preprocess *free* — zero cost, magic — and the tool gets
**1.185× faster**. That is the Amdahl ceiling on everything that is not the
recognizer, and the recognizer is Tesseract's LSTM, which this project does not
control.

A smaller earlier profile (n=368) put recognize at 93.8%. The 9.4-point
difference is entirely the upscale, which fires only on small images; the
larger sample has a more representative size mix. Both profiles agree on the
shape.

**Projection.** At 2,029 B/image and 252 ms/image, 200,000 images is roughly
**430 MB of index and ~14 h of one-time cold indexing**. RSS grows across a long
run (quarter means 440 / 501 / 520 / 560 MB, peak 976 MB sampled, 331 MB at the
end) and comes back down, which reads as allocator behaviour rather than a leak.

### What this means on a different machine

Everything above is **one measured point**: 8 logical / 4 physical cores on
Windows with `tessdata_fast`. Indexing is CPU-bound and close to linear in
physical cores (0.966 CPU per core, no idle time left to reclaim), so scaling
by core count is a defensible first approximation and nothing more. Every row
below except the bold one is **extrapolated from that single point, +/-30%**.

| Machine class | Physical cores | Est. throughput | Est. wall clock, 20k images |
| --- | --- | --- | --- |
| Small laptop | 2 | ~2 img/s | ~2.8 h |
| **Measured baseline** | **4** | **3.96 img/s** | **1.39 h** |
| Desktop | 8 | ~8 img/s | ~0.7 h |
| Workstation | 16 | ~16 img/s | ~0.35 h |

The error bars are the honest part of that table. Per-core clock and memory
bandwidth vary more between machines than core count does; the size mix of your
images decides how often the upscale path fires and therefore what the
recognizer costs; `tessdata_best` models cost +55% index time; and every extra
language in `--lang` costs model-load time on every worker. Measure your own
corpus before planning around any row but the bold one.

### The directory walk: take the mtime from the directory entry

The incremental path needs exactly one mtime per file to answer *did anything
change?*, and where that mtime comes from dominates the cost of a no-op run.
On Windows, `DirEntry::metadata()` is served out of the `FindNextFile` record
the enumeration already returned; a standalone `fs::metadata()` on the same file
reopens it:

| | per file |
| --- | --- |
| `DirEntry::metadata()` | 5.2 µs |
| `fs::metadata()` | 122.8 µs |

**23.6×.** Over 19,839 files that is the difference between an `lq index` on an
unchanged corpus finishing before the user lets go of Enter and one that spends
seconds to report that nothing happened. `indexer::discover` therefore carries
the mtime it already has all the way to the writer, and nothing stats a file
twice in a run.

One semantic difference on Windows: for a symlinked image this reports the
link's mtime, not the target's, so edits to the target do not re-trigger OCR.
`--full-reindex` covers that case.

### Run-to-run noise: ~15%

Repeated runs of the identical workload on the identical file spread about
**15%**. Nothing smaller than that is reportable at this scale, and every
unclaimed performance win in this project is smaller than that.

Cross-session comparisons are worse than useless: the process spawn floor moved
36.3 → 43.0 ms between two sessions on the same machine and manufactured a
fictional 28.3% improvement out of nothing. Measure A/B/B/A within one session
or do not measure.

---

## 2. Search latency: the query was never the problem

18 queries × 15 runs, p50 milliseconds, over the 19,839-document index:

| | one-shot `lq search` | `lq serve` | `lq serve --substring` |
| --- | --- | --- | --- |
| process spawn floor | 28.4 | — | — |
| single term | 49.9–55.1 | 2.6–4.9 | 2.3–3.5 |
| multi-term | 49.9–57.2 | 2.0–2.9 | 3.2–5.6 |
| phrase | 52.0–61.7 | 1.4–2.0 | 2.9–5.4 |
| prefix | 40.7–72.0 | 3.1–4.6 | 2.1–3.9 |
| miss | 37.0–42.4 | 0.5–0.7 | 0.2–0.8 |

**FTS5 over ~20k documents costs 0.5–5 ms.** Everything a one-shot user feels is
process startup — a 21.6–36.3 ms spawn floor — and even the in-process "query
work" (1.5–6.4 ms) is mostly connection setup. No index tuning, query rewriting,
or PRAGMA touches this. The only lever on the spawn floor is not spawning, which
is what `lq serve` is for.

Two consequences worth stating plainly:

- **`--substring` is cheap in time and expensive in space.** 1.9–2.7× slower on
  multi-term and phrase queries through `serve` (2.0 → 5.4 ms), a wash or faster
  on single terms and prefixes, all of it inside 6 ms, with identical hit
  counts. In one-shot mode it is invisible under spawn cost. Its real price is
  the trigram index: **62.8% of the database file**.

  These numbers were taken when this flag was still called `--fuzzy`; the
  measurement is of the trigram path either way. The typo-tolerant `--fuzzy`
  that now owns that name has **not** been measured at this scale — it adds a
  length-sliced scan of the term dictionary and a second FTS5 query per search,
  and the honest thing to say today is that nobody has timed it on 20k
  documents. Two orders of magnitude of headroom under the spawn floor is the
  reason that is not yet urgent, not a claim that the cost is zero.
- **`lq compact` is a space feature, not a speed feature.** −3.00% file size on
  a freshly built index, −9.78% on a long-lived churned one; it pays in
  proportion to churn. Its latency effect is nil. The naive before/after
  measurement read −18.4%, which was cache warmth; a controlled same-session A/B
  put the compacted file **5.8% slower**, well inside the ~15% noise band.
- **`PRAGMA optimize` on its own makes the file 1.48% *bigger*.** It is run as
  part of `compact`, before the `VACUUM`, not instead of it.

`snippet()` costs +0.4 ms median, +1.1 ms worst. Effectively free.

---

## 3. OCR configuration: psm 11, upscale 1200, min-conf 40

The shipping configuration is page segmentation mode **11** (sparse text), OEM
3, upscale short-edge to **1200 px**, word confidence floor **40**, languages
`eng+hrv`, `--thorough` off, `tessdata_fast` models. That gives **0.674
findability** at a plausible precision around **0.90**.

### psm 11 beats psm 6 because photographs are not pages

The default was psm 6, "one uniform block of text", and moving to psm 11,
"sparse text", is worth **+0.049 findability** (p = 0.021, n = 149
hand-labelled images) — the largest single accuracy win in the project.

The mechanism matters more than the number, because it predicts where the win
comes from. psm 6 does not *hint* that the image is a page of text, it
**asserts** it: Tesseract then has to find glyphs in every region, including
brickwork, foliage, and JPEG noise, and it obliges. psm 11 runs layout analysis
first and reads what it finds. On scans of documents the two are close; on
photographs and screenshots — which is what a photo corpus mostly is — psm 6
manufactures text that was never there, and the junk it invents also drags down
`bm25()` scoring for the real hits.

psm 6 survives only as the second arm of `--thorough`, where a page that came
back nearly empty is worth re-reading under the opposite assumption.

### `--min-conf` is a real trade with no winning setting

| `--min-conf` | findability | precision |
| --- | --- | --- |
| 0 | 0.694 | 0.841 |
| **40 (default)** | **0.674** | **0.905** |
| 60 | 0.657 | 0.930 |

Lower it if you would rather search a noisier index than miss a file; raise it
if junk hits bother you more than misses. There is no setting that wins on both
axes, which is exactly why this ships as a flag rather than a fixed constant.

### `--thorough` is a good option and a bad default

Re-reading any image whose first pass found ≤ 3 words with a second page
segmentation mode, and merging the results, buys **+2.1% findability for +26%
indexing time**. That is a bad trade for a repeated incremental index and a
reasonable one for a one-off archival pass over a folder you will search for
years. Hence: a flag, off by default.

The trigger threshold of 3 words holds. A threshold of 5 scored *identically*
for +26.7% wall time against 3's +12.1%; a threshold of 1 was indistinguishable
from off.

### The upscale belongs to recall, not to performance

The BICUBIC upscale to 1200 px costs ~95.7 ms/image amortised — 11.4% of total
time, all of it on small images (mean preprocess 102.77 ms against a **median of
7.08 ms**, because images already ≥1200 px skip the resize entirely). It is the
single largest tractable item in the profile, so it was scored as an accuracy
arm rather than landed as a patch (n=50):

| Arm | findability | 95% CI | recall | ms/img |
| --- | --- | --- | --- | --- |
| cap 200 | 0.650 | [0.54, 0.75] | 0.814 | 323 |
| **upscale 1200 (ships)** | **0.645** | [0.54, 0.74] | **0.814** | 332 |
| cap 125 | 0.644 | [0.54, 0.74] | 0.775 | 252 |
| cap 150 | 0.638 | [0.53, 0.74] | 0.808 | 279 |
| no upscale | 0.620 | [0.51, 0.72] | 0.699 | 232 |

Every cap sits within ±0.007 findability of shipping, inside a CI ~±0.10 wide —
no measurable difference, not an improvement. Two things do survive: **cap 125
buys 24% less OCR time for −0.001 findability but drops recall 0.814 → 0.775**
(the only arm in the project worth re-confirming at larger n, and only if you
want that time back), and **not upscaling at all is a genuine loss**.

---

## 4. Levers that were measured and closed

Each of these is individually plausible enough to be proposed again, which is
why they are listed. Each cost real measurement time.

| Lever | Result |
| --- | --- |
| **`tessdata_best` models** | Null: +0.004 (p=0.871), −0.002, −0.003 across three pairings, all CIs inside ±0.055 — for +32.4 MB and **+55% index time** |
| **Nine preprocessing arms** | Best is autocontrast cutoff 2 at +0.009 (p=0.184); cutoff 10 is −0.031 (p=0.013); histogram equalization is −0.154 (p<0.001) |
| **Sauvola / Otsu thresholding, DPI tuning** | Closed, measured, no gain |
| **OpenCV text-detection cascade** | −0.147 to −0.191, all p<0.001 |
| **Per-image language routing** | Best routing gain +0.003, negative in five of six pairings; the oracle gap has a noise floor the size of its own mechanisms |
| **psm 3 / psm 4** | Bought precision at the cost of findability |
| **GPU acceleration** | Permanently closed — Tesseract 5's LSTM has no GPU path |
| **More worker threads** | Past the knee: 4 physical cores scale 2.39×, not 4× (memory bandwidth). 8 workers is *slower* than 6 and costs +320 MB |
| **SIMD grayscale, zero-copy buffers** | Taken, and worth ≈1.015× together. Accuracy-neutral by construction. Must never be quoted as a speedup |

The `tessdata_best` null is the one that should end the argument, because the
intuition it overturns is the strongest one available: `best` genuinely *is* the
more accurate tier in Tesseract's own benchmarks. It does nothing here because
this corpus's failures are not marginal-confidence character errors — they are
photos where the text is small, skewed, or partly out of frame, and both model
tiers fail on the same images. Plausible precision is flat at 0.88–0.93 across
all six arms: the arms are reading the same words.

The autocontrast ladder tells the same story in miniature. Cutoffs 0/1/2/5/10
score +0.000 / +0.004 / +0.009 / −0.016 / −0.031 — a smooth, single-peaked,
genuinely real optimum whose value *at the optimum* is indistinguishable from
zero. A lever with a true peak worth +0.009 is a closed lever, not a tuning
opportunity.

## 5. The headroom that does exist

Oracle-over-all-arms scores 0.761 against the shipping 0.674, so **+0.087 is on
the table**. It is concentrated — the top 20 images hold 72% of it, the top 40
hold 97% — and it has **no describable class**: nobody has found the property
that predicts which image needs which arm, which is precisely why per-image
routing failed.

Every path to it through preprocessing, thresholding, DPI, detection, and
language routing has been measured and closed. What remains unexplored is the
recognizer itself — fine-tuning, a custom `traineddata` for this kind of input.
That is a different project (data collection, a training loop, evaluation
infrastructure, ongoing model maintenance), not an optimization of this one.

## 6. Reproducing any of this

Two probes ship in `examples/`:

- `cargo run --release --example stage_profile -- <dir>` — per-stage timing
  (decode / preprocess / recognize) over a directory of your own images.
- `cargo run --release --example recall -- [fixtures_dir]` — the fixture
  regression gate.

Bring your own images; no corpus ships with this repository. If you re-run the
throughput numbers, report the machine — the absolute figures above are one
machine's, and only the *shares* and *ratios* transfer.
