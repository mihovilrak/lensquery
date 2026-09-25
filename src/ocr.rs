//! Image preprocessing and the OCR entry point.
//!
//! `extract_text` decodes an image, runs it through the preprocess pipeline
//! below, and OCRs it on the persistent in-process engine ([`crate::tess`]).
//!
//! **Do not chase resampling parity with other imaging libraries.** Resizing
//! filters differ between implementations by a few least-significant bits, and
//! the obvious worry is that this costs recall. It was measured against a
//! hand-labelled corpus at matched configuration: two independent
//! implementations of this pipeline scored 0.6622 and 0.6617 findability and
//! agreed exactly on 113 of 140 images — statistically indistinguishable. A
//! port of somebody else's resampling code buys nothing and was abandoned on
//! that evidence. Reopen it only with new measurements, not with a new
//! intuition.
//!
//! Grayscale conversion is a different matter and *is* pinned to exact integer
//! coefficients (`LUMA_R`/`LUMA_G`/`LUMA_B` below), because those are cheap to
//! reproduce and make the conversion bit-exact rather than merely close.

use std::path::Path;

use image::imageops::FilterType;
use image::{DynamicImage, GrayImage, RgbImage};

/// Three-value OCR result — never collapse the cases (frozen contract).
///
/// `Text` = non-empty recognized text; `Empty` = the image was read but held no
/// text (`""`, still written to the DB); `Failed` = engine/decoding failure
/// (`None`, no DB row written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ocr {
    Text(String),
    Empty,
    Failed,
}

/// Upscale below this width (Tesseract reads small text poorly).
const MIN_WIDTH: u32 = 1000;
/// Target width when upscaling. 1200 rather than the 1500 this started at:
/// measured indistinguishable on findability (0.674 vs 0.678) for ~23% less OCR
/// time. Not upscaling at all *is* a real loss — see
/// [benchmarks](../docs/benchmarks.md) §3.
const UPSCALE_WIDTH: u32 = 1200;
/// Downscale above this width (diminishing recall, rising cost).
const MAX_WIDTH: u32 = 2400;
/// Thorough mode's default trigger: when the primary (psm 11) pass yields this
/// many words or fewer, the image is probably a dense/uniform block that
/// sparse-text layout analysis walked past, so a second `PSM_UNIFORM_BLOCK` arm
/// is worth its cost. Above the trigger the primary pass already found plenty.
///
/// 3 was chosen by inspection and then scored against 1 and 5 on a
/// hand-labelled corpus: 5 scored identically to 3 on every paired image while
/// costing +26.7% wall time against 3's +12.1%, and 1 was indistinguishable
/// from thorough-off. 3 keeps the whole measured gain at the lower cost. Still
/// overridable, because a denser corpus may disagree. See
/// [benchmarks](../docs/benchmarks.md) §3.
pub const DEFAULT_THOROUGH_TRIGGER_WORDS: usize = 3;

/// Hard pixel-dimension cap fed to the decoder. `ImageReader` defaults to
/// *no* limits, so a corrupt or hostile header (a "decompression bomb": tiny
/// file, enormous declared dimensions) can make the decoder allocate until the
/// process dies — which at 20k images means losing a multi-hour run to one bad
/// file. 32768 is far above any real photo or screenshot; anything past it is
/// `Ocr::Failed`, an outcome the caller already handles.
const MAX_DECODE_DIMENSION: u32 = 32_768;
/// Decoder allocation ceiling in bytes (512 MiB — the `image` crate's own
/// `Limits::default()` value). A 100-megapixel RGB source needs ~300 MB, so
/// this clears real inputs with room to spare while bounding the damage a
/// malformed one can do. Non-strict: some decoders ignore it, which is why the
/// dimension cap above is set as well.
const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;

/// Apply the preprocess pipeline into `out`; returns the dimensions of the
/// row-major 8-bit grayscale pixels now in `out`, ready to hand straight to
/// `tess::recognize`.
///
/// Order matches Pillow's `_preprocess`: normalize mode (drop alpha / expand
/// palette to RGB) → resize in that colour space → convert to grayscale last.
///
/// Takes the buffer as an out-parameter so a caller indexing 20k images can
/// hand the same allocation back on every call instead of asking the allocator
/// for a fresh 1–3 MB and returning it 20k times (see the `LUMA` scratch in
/// [`extract_text_timed`]). `img` is taken by value, and `into_luma8` /
/// `into_rgb8` are used over their `to_*` twins, so an image decoded in the
/// colour space we want is *moved* through the pipeline rather than cloned —
/// which is the common case for JPEG (RGB8) and most PNG screenshots.
fn preprocess_into(img: DynamicImage, out: &mut Vec<u8>) -> (u32, u32) {
    // Is the source effectively grayscale? Pillow leaves "L"/"LA" in luma
    // space; everything colourful (RGB/RGBA/P/CMYK) is worked in RGB then
    // converted to L at the very end.
    let is_gray = matches!(
        img,
        DynamicImage::ImageLuma8(_)
            | DynamicImage::ImageLuma16(_)
            | DynamicImage::ImageLumaA8(_)
            | DynamicImage::ImageLumaA16(_)
    );

    if is_gray {
        let mut buf: GrayImage = img.into_luma8();
        if let Some((nw, nh, filter)) = resize_target(buf.width(), buf.height()) {
            buf = image::imageops::resize(&buf, nw, nh, filter);
        }
        let (w, h) = (buf.width(), buf.height());
        // A luma image's own buffer is already byte-for-byte what the engine
        // wants, so it is *moved* into `out`. That donates the caller's
        // scratch allocation to the allocator instead of reusing it, which is
        // still cheaper than copying the pixels through it; the next colour
        // image just grows a fresh scratch.
        *out = buf.into_raw();
        (w, h)
    } else {
        // Pillow `convert("RGB")` on RGBA drops alpha (no compositing);
        // `into_rgb8` matches that.
        let mut buf: RgbImage = img.into_rgb8();
        if let Some((nw, nh, filter)) = resize_target(buf.width(), buf.height()) {
            buf = image::imageops::resize(&buf, nw, nh, filter);
        }
        let (w, h) = (buf.width(), buf.height());
        rgb_to_luma_pillow_into(&buf, out);
        (w, h)
    }
}

/// Decide the resize target and filter, or `None` when the width is in the
/// no-resize band `[MIN_WIDTH, MAX_WIDTH]`. New height is truncated toward zero
/// to match Pillow's `int(height * ratio)`.
fn resize_target(width: u32, height: u32) -> Option<(u32, u32, FilterType)> {
    if width < MIN_WIDTH {
        // BICUBIC upscale — Pillow `Image.Resampling.BICUBIC` ≈ CatmullRom.
        let nh = ((height as u64 * UPSCALE_WIDTH as u64) / width as u64) as u32;
        Some((UPSCALE_WIDTH, nh.max(1), FilterType::CatmullRom))
    } else if width > MAX_WIDTH {
        // LANCZOS downscale — Pillow `Image.Resampling.LANCZOS` ≈ Lanczos3.
        let nh = ((height as u64 * MAX_WIDTH as u64) / width as u64) as u32;
        Some((MAX_WIDTH, nh.max(1), FilterType::Lanczos3))
    } else {
        None
    }
}

/// Pillow's integer luma coefficients (L24). They sum to exactly 65536, which
/// is what makes the `>> 16` exact rather than approximate.
const LUMA_R: u32 = 19595;
const LUMA_G: u32 = 38470;
const LUMA_B: u32 = 7471;

/// Convert RGB to 8-bit luma into `out` (replacing its contents, keeping its
/// allocation) using Pillow's exact integer coefficients, so in-band images are
/// byte-identical to `Image.convert("L")`:
/// `L = (R*19595 + G*38470 + B*7471 + 0x8000) >> 16`.
///
/// Dispatches to an AVX2 implementation when the running CPU has it. That path
/// is not an approximation of the scalar one — it computes the same integer
/// expression in 32-bit lanes and is asserted byte-for-byte identical by
/// `simd_luma_matches_scalar_exactly`. (The 16-bit multiply-high tricks usual
/// for luma cannot be used here: 38470 exceeds `i16::MAX`, so they would round
/// differently and quietly change what we feed Tesseract.)
fn rgb_to_luma_pillow_into(img: &RgbImage, out: &mut Vec<u8>) {
    let src = img.as_raw();
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: the `avx2` target feature was just detected at runtime.
            unsafe { luma_avx2(src, out) };
            return;
        }
    }
    luma_scalar(src, out);
}

/// The reference implementation, and the fallback when there is no AVX2.
/// Appends one luma byte per RGB triple in `src` to `out`.
fn luma_scalar(src: &[u8], out: &mut Vec<u8>) {
    out.reserve(src.len() / 3);
    out.extend(src.as_chunks::<3>().0.iter().map(|px| {
        let r = px[0] as u32;
        let g = px[1] as u32;
        let b = px[2] as u32;
        ((r * LUMA_R + g * LUMA_G + b * LUMA_B + 0x8000) >> 16) as u8
    }));
}

/// AVX2 luma: 8 pixels per iteration, bit-identical to [`luma_scalar`].
///
/// Each 128-bit half holds 4 pixels (12 bytes), loaded at byte offsets +0 and
/// +12, so one 256-bit register covers 24 bytes = 8 pixels. Three in-lane
/// `shuffle_epi8` masks scatter R, G and B into zero-extended 32-bit lanes;
/// the multiplies and the `+0x8000 >> 16` then run at full 32-bit width, where
/// the largest intermediate (16,744,448) has plenty of headroom. The result is
/// narrowed back to 8 bytes with two saturating packs — saturation never
/// engages, since every lane is already in 0..=255.
///
/// # Safety
/// Caller must have verified the `avx2` target feature is available.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2")]
unsafe fn luma_avx2(src: &[u8], out: &mut Vec<u8>) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86 as simd;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64 as simd;

    let pixels = src.len() / 3;
    out.clear();
    out.reserve(pixels);

    // Byte selectors, identical in both 128-bit lanes: -1 writes a zero, so
    // each channel lands in the low byte of its own 32-bit lane.
    let mask_r = simd::_mm256_setr_epi8(
        0, -1, -1, -1, 3, -1, -1, -1, 6, -1, -1, -1, 9, -1, -1, -1, 0, -1, -1, -1, 3, -1, -1, -1,
        6, -1, -1, -1, 9, -1, -1, -1,
    );
    let mask_g = simd::_mm256_setr_epi8(
        1, -1, -1, -1, 4, -1, -1, -1, 7, -1, -1, -1, 10, -1, -1, -1, 1, -1, -1, -1, 4, -1, -1, -1,
        7, -1, -1, -1, 10, -1, -1, -1,
    );
    let mask_b = simd::_mm256_setr_epi8(
        2, -1, -1, -1, 5, -1, -1, -1, 8, -1, -1, -1, 11, -1, -1, -1, 2, -1, -1, -1, 5, -1, -1, -1,
        8, -1, -1, -1, 11, -1, -1, -1,
    );
    let coeff_r = simd::_mm256_set1_epi32(LUMA_R as i32);
    let coeff_g = simd::_mm256_set1_epi32(LUMA_G as i32);
    let coeff_b = simd::_mm256_set1_epi32(LUMA_B as i32);
    let round = simd::_mm256_set1_epi32(0x8000);

    let base = src.as_ptr();
    let dst = out.as_mut_ptr();
    let mut done = 0usize; // pixels written
    let mut off = 0usize; // byte offset into src

    // The +12 load reads 16 bytes, so the last byte touched is off+27: stop
    // while `off + 28 <= src.len()` and finish the remainder scalar.
    while off + 28 <= src.len() {
        let lo = simd::_mm_loadu_si128(base.add(off) as *const simd::__m128i);
        let hi = simd::_mm_loadu_si128(base.add(off + 12) as *const simd::__m128i);
        let px = simd::_mm256_set_m128i(hi, lo);

        let r = simd::_mm256_shuffle_epi8(px, mask_r);
        let g = simd::_mm256_shuffle_epi8(px, mask_g);
        let b = simd::_mm256_shuffle_epi8(px, mask_b);

        let acc = simd::_mm256_add_epi32(
            simd::_mm256_add_epi32(
                simd::_mm256_mullo_epi32(r, coeff_r),
                simd::_mm256_mullo_epi32(g, coeff_g),
            ),
            simd::_mm256_add_epi32(simd::_mm256_mullo_epi32(b, coeff_b), round),
        );
        let shifted = simd::_mm256_srli_epi32::<16>(acc);

        // u32 x8 -> u16 x8 (duplicated per lane) -> u8: the 4 results of each
        // lane end up in that lane's low 4 bytes.
        let packed16 = simd::_mm256_packus_epi32(shifted, shifted);
        let packed8 = simd::_mm256_packus_epi16(packed16, packed16);
        let lane0 = simd::_mm256_extract_epi32::<0>(packed8) as u32;
        let lane1 = simd::_mm256_extract_epi32::<4>(packed8) as u32;

        std::ptr::copy_nonoverlapping(lane0.to_le_bytes().as_ptr(), dst.add(done), 4);
        std::ptr::copy_nonoverlapping(lane1.to_le_bytes().as_ptr(), dst.add(done + 4), 4);

        done += 8;
        off += 24;
    }
    // SAFETY: `done` bytes were just written into the reserved capacity.
    out.set_len(done);
    luma_scalar(&src[off..pixels * 3], out);
}

/// Collapse all runs of ASCII/Unicode whitespace to single spaces and trim.
///
/// Tesseract's line and block structure is not information the index can use —
/// FTS5 tokenizes on whitespace either way — and flattening it keeps stored
/// text, snippets, and terminal output on one line.
fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Merge a fallback arm's words into the primary arm's, preserving the primary
/// order and appending only fallback words the primary did not already produce
/// (compared case-folded, so "Zagreb" suppresses a fallback "ZAGREB").
fn union_words(primary: &str, fallback: &str) -> String {
    // `HashSet`, not `Vec::contains`: membership is checked once per fallback
    // word against every primary word, so the `Vec` form was O(n·m). A dense
    // screenshot yields a few hundred words per arm — small, but this runs on
    // every `--thorough` image and the set version costs nothing extra.
    let mut seen: std::collections::HashSet<String> =
        primary.split_whitespace().map(str::to_lowercase).collect();
    let mut out: Vec<&str> = primary.split_whitespace().collect();
    for word in fallback.split_whitespace() {
        if seen.insert(word.to_lowercase()) {
            out.push(word);
        }
    }
    out.join(" ")
}

/// Decoder resource limits (see `MAX_DECODE_DIMENSION` / `MAX_DECODE_ALLOC`).
fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

/// Wall time in seconds for each stage of one `extract_text` call.
///
/// Exists so the Amdahl question — *how much of an image's cost is the
/// recognizer, and therefore how much is even available to optimize?* — can be
/// answered by measurement rather than argument. `examples/stage_profile.rs`
/// aggregates these. Stages that never ran (decode failed, so nothing was
/// preprocessed) stay 0.0.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StageTimings {
    /// Header parse plus full pixel decode.
    pub decode: f64,
    /// Resize, grayscale conversion, buffer handoff.
    pub preprocess: f64,
    /// Every `tess::recognize` call, including the thorough second arm.
    pub recognize: f64,
}

impl StageTimings {
    /// Total measured time for the image.
    pub fn total(&self) -> f64 {
        self.decode + self.preprocess + self.recognize
    }
}

/// Extract OCR text from an image file.
///
/// `min_conf` is the word-confidence floor (0–100) handed to the engine.
/// `thorough` enables the opt-in two-arm mode: when the primary sparse-text pass
/// yields `thorough_trigger_words` words or fewer, the same pixel buffer is
/// re-read at `PSM_UNIFORM_BLOCK` and the union of both arms' words is returned.
/// `thorough_trigger_words` is ignored unless `thorough` is set; pass
/// [`DEFAULT_THOROUGH_TRIGGER_WORDS`] for the shipping behaviour.
///
/// Returns `Text` on success, `Empty` when the image held no text, or `Failed`
/// on any decode/engine error. Never panics. A failing thorough second arm never
/// downgrades a successful primary result — it degrades to the primary text.
pub fn extract_text(
    image_path: &Path,
    lang: &str,
    min_conf: f32,
    thorough: bool,
    thorough_trigger_words: usize,
) -> Ocr {
    extract_text_timed(image_path, lang, min_conf, thorough, thorough_trigger_words).0
}

/// Whether the thorough second arm should run for a given primary result.
///
/// Pulled out of [`extract_text_timed`] so the gate is testable without an
/// engine: `thorough` alone decides whether the mode exists at all, and only
/// then does the word count meet the (now caller-supplied) trigger.
fn second_arm_wanted(primary: &str, thorough: bool, trigger_words: usize) -> bool {
    thorough && primary.split_whitespace().count() <= trigger_words
}

thread_local! {
    /// Scratch grayscale buffer, reused across every image this thread OCRs.
    ///
    /// The buffer never escapes [`extract_text_timed`] — the function returns
    /// an `Ocr`, i.e. an owned `String` — so the whole zero-copy change is
    /// invisible at the API boundary: no signature in
    /// `docs/api-contracts.md` moves. Thread-local rather than a parameter
    /// because the indexer runs one OCR thread per worker and they must not
    /// share it. Not re-entrant, which is fine: `extract_text_timed` never
    /// calls itself, and a panic mid-borrow releases the guard while
    /// unwinding (the indexer's `catch_unwind` then sees a normal failure).
    static LUMA: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// [`extract_text`] plus a per-stage timing split.
///
/// This is the real implementation; `extract_text` is the thin wrapper. Four
/// `Instant::now()` calls against a ~250 ms image are unmeasurable, so the
/// indexer runs the instrumented path in production rather than keeping two
/// copies of the pipeline that could drift.
pub fn extract_text_timed(
    image_path: &Path,
    lang: &str,
    min_conf: f32,
    thorough: bool,
    thorough_trigger_words: usize,
) -> (Ocr, StageTimings) {
    let mut t = StageTimings::default();
    let t0 = std::time::Instant::now();
    let img = match image::ImageReader::open(image_path).and_then(|r| r.with_guessed_format()) {
        Ok(mut reader) => {
            reader.limits(decode_limits());
            match reader.decode() {
                Ok(img) => img,
                Err(_) => return (Ocr::Failed, t),
            }
        }
        Err(_) => return (Ocr::Failed, t),
    };
    let t1 = std::time::Instant::now();
    t.decode = (t1 - t0).as_secs_f64();

    LUMA.with(|cell| {
        let mut data = cell.borrow_mut();
        let (width, height) = preprocess_into(img, &mut data);
        let t2 = std::time::Instant::now();
        t.preprocess = (t2 - t1).as_secs_f64();

        let (w, h) = (width as i32, height as i32);
        let primary =
            match crate::tess::recognize(&data, w, h, lang, min_conf, crate::tess::PSM_SPARSE) {
                Some(raw) => normalize_whitespace(&raw),
                None => {
                    t.recognize = t2.elapsed().as_secs_f64();
                    return (Ocr::Failed, t);
                }
            };

        let mut text = primary;
        if second_arm_wanted(&text, thorough, thorough_trigger_words) {
            // A failed second arm is not a failed image: keep the primary result.
            if let Some(raw) =
                crate::tess::recognize(&data, w, h, lang, min_conf, crate::tess::PSM_UNIFORM_BLOCK)
            {
                text = union_words(&text, &normalize_whitespace(&raw));
            }
        }
        t.recognize = t2.elapsed().as_secs_f64();

        let ocr = if text.is_empty() {
            Ocr::Empty
        } else {
            Ocr::Text(text)
        };
        (ocr, t)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `rgb_to_luma_pillow_into` as a plain function, for tests.
    fn rgb_to_luma_pillow(img: &RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        rgb_to_luma_pillow_into(img, &mut out);
        out
    }

    #[test]
    fn pillow_luma_matches_reference_values() {
        // Pure white / black / mid grey / a primary — checked against
        // Pillow's `Image.convert("L")` formula.
        let img = RgbImage::from_raw(4, 1, vec![255, 255, 255, 0, 0, 0, 128, 128, 128, 255, 0, 0])
            .unwrap();
        let luma = rgb_to_luma_pillow(&img);
        assert_eq!(luma[0], 255); // white
        assert_eq!(luma[1], 0); // black
        assert_eq!(luma[2], 128); // mid grey
                                  // red: (255*19595 + 0x8000) >> 16 = 76
        assert_eq!(luma[3], 76);
    }

    #[test]
    fn simd_luma_matches_scalar_exactly() {
        // The whole case for the SIMD path is that it is not a different
        // answer, so this compares it to the scalar reference byte for byte —
        // if it ever diverges, the pixels we hand Tesseract change and every
        // recall number ever measured stops applying.
        //
        // Lengths sweep 0..64 pixels so the vector body (8 at a time), the
        // scalar tail, and the all-tail case are all covered. Pixel values are
        // a cheap deterministic LCG plus the saturation corners.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        };
        for pixels in 0..64usize {
            let mut src: Vec<u8> = (0..pixels * 3).map(|_| next()).collect();
            // Force the corners into any buffer big enough to hold them.
            if pixels >= 2 {
                src[0..3].copy_from_slice(&[0, 0, 0]);
                src[3..6].copy_from_slice(&[255, 255, 255]);
            }
            let mut expected = Vec::new();
            luma_scalar(&src, &mut expected);

            let mut actual = vec![0xAAu8; 7]; // must be cleared, not appended to
            let img = RgbImage::from_raw(pixels as u32, 1, src).unwrap();
            rgb_to_luma_pillow_into(&img, &mut actual);

            assert_eq!(actual.len(), pixels, "wrong length at {pixels} pixels");
            assert_eq!(actual, expected, "divergence at {pixels} pixels");
        }
    }

    #[test]
    fn luma_scratch_buffer_survives_reuse() {
        // The scratch buffer is reused across images; a conversion that
        // appended instead of replacing would silently prepend the previous
        // image's pixels to the next one's.
        let a = RgbImage::from_raw(2, 1, vec![255, 255, 255, 0, 0, 0]).unwrap();
        let b = RgbImage::from_raw(1, 1, vec![128, 128, 128]).unwrap();
        let mut buf = Vec::new();
        rgb_to_luma_pillow_into(&a, &mut buf);
        assert_eq!(buf, vec![255, 0]);
        rgb_to_luma_pillow_into(&b, &mut buf);
        assert_eq!(buf, vec![128]);
    }

    #[test]
    fn resize_target_no_op_in_band() {
        assert_eq!(resize_target(1000, 500), None);
        assert_eq!(resize_target(1200, 300), None);
        assert_eq!(resize_target(2400, 900), None);
    }

    #[test]
    fn resize_target_upscales_narrow() {
        let (w, h, _) = resize_target(500, 200).unwrap();
        assert_eq!(w, UPSCALE_WIDTH);
        // int(200 * 1200 / 500) == 480
        assert_eq!(h, 480);
    }

    #[test]
    fn resize_target_downscales_wide() {
        let (w, h, _) = resize_target(4800, 1200).unwrap();
        assert_eq!(w, 2400);
        // int(1200 * 2400 / 4800) == 600
        assert_eq!(h, 600);
    }

    #[test]
    fn normalize_whitespace_collapses() {
        assert_eq!(normalize_whitespace("  a\n\t b  c "), "a b c");
        assert_eq!(normalize_whitespace("   "), "");
    }

    #[test]
    fn union_words_keeps_primary_order_then_appends_new() {
        assert_eq!(
            union_words("beta alpha", "gamma delta"),
            "beta alpha gamma delta"
        );
    }

    #[test]
    fn union_words_dedupes_case_folded() {
        assert_eq!(
            union_words("Zagreb ulica", "ZAGREB Ulica trg"),
            "Zagreb ulica trg"
        );
        // Duplicates inside the fallback arm collapse too.
        assert_eq!(union_words("a", "b B b"), "a b");
    }

    #[test]
    fn union_words_handles_empty_arms() {
        assert_eq!(union_words("", "only fallback"), "only fallback");
        assert_eq!(union_words("only primary", ""), "only primary");
        assert_eq!(union_words("", ""), "");
    }

    #[test]
    fn decode_limits_are_bounded() {
        let limits = decode_limits();
        assert_eq!(limits.max_image_width, Some(MAX_DECODE_DIMENSION));
        assert_eq!(limits.max_image_height, Some(MAX_DECODE_DIMENSION));
        assert_eq!(limits.max_alloc, Some(MAX_DECODE_ALLOC));
    }

    #[test]
    fn undecodable_file_is_failed_not_empty() {
        // Bytes that are not an image at all: a decode failure must surface as
        // `Failed` (no DB row), never `Empty` (a row saying "no text here").
        // Runs identically with or without libtesseract — it never gets that far.
        let path = std::env::temp_dir().join(format!("lq_ocr_garbage_{}.png", std::process::id()));
        std::fs::write(&path, b"not an image, just bytes").unwrap();
        assert_eq!(
            extract_text(&path, "eng", 40.0, false, DEFAULT_THOROUGH_TRIGGER_WORDS),
            Ocr::Failed
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn timings_stay_zero_for_stages_that_never_ran() {
        // A stage that did not execute must report 0.0, not a fabricated
        // fraction — the whole point of the split is the *share*, and inventing
        // preprocess time for images that never got past decode would tilt it.
        let path = std::env::temp_dir().join(format!("lq_ocr_timing_{}.png", std::process::id()));
        std::fs::write(&path, b"not an image, just bytes").unwrap();
        let (ocr, t) =
            extract_text_timed(&path, "eng", 40.0, false, DEFAULT_THOROUGH_TRIGGER_WORDS);
        assert_eq!(ocr, Ocr::Failed);
        assert_eq!(t.preprocess, 0.0);
        assert_eq!(t.recognize, 0.0);
        assert_eq!(t.total(), t.decode);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn thorough_trigger_only_fires_on_sparse_primaries() {
        // The gate `extract_text` applies: at or below the trigger a second arm
        // runs, above it the primary stands alone.
        let sparse = "a b c";
        let dense = "a b c d";
        assert!(sparse.split_whitespace().count() <= DEFAULT_THOROUGH_TRIGGER_WORDS);
        assert!(dense.split_whitespace().count() > DEFAULT_THOROUGH_TRIGGER_WORDS);
        // A no-op union leaves an already-dense primary byte-identical.
        assert_eq!(union_words(dense, ""), dense);
    }

    #[test]
    fn thorough_trigger_is_a_knob_not_a_constant() {
        // The trigger is a knob, not a constant: one primary text falls on
        // either side of the gate depending on the value passed. Raising the
        // trigger widens the second arm reach; 0 narrows it to "the primary
        // found literally nothing".
        let text = "a b c d";
        assert!(!second_arm_wanted(
            text,
            true,
            DEFAULT_THOROUGH_TRIGGER_WORDS
        ));
        assert!(second_arm_wanted(text, true, 5));
        assert!(!second_arm_wanted(text, true, 0));
        assert!(second_arm_wanted("", true, 0));
    }

    #[test]
    fn thorough_off_ignores_the_trigger_entirely() {
        // A trigger large enough to catch anything must still be inert while
        // `thorough` is false — the knob widens an opt-in mode, it never enables
        // one. Otherwise a config value alone would double indexing cost.
        assert!(!second_arm_wanted("", false, usize::MAX));
        assert!(!second_arm_wanted("a b c", false, 99));
    }
}
