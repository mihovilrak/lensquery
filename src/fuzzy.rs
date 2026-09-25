//! Typo tolerance for `lq search --fuzzy`: how near two words have to be
//! before one is offered as a correction of the other.
//!
//! This module is pure — it knows nothing about SQLite or FTS5. [`crate::db`]
//! owns the vocabulary lookup and the query rewriting; everything here is
//! string arithmetic over two words, so it is cheap to test exhaustively.
//!
//! Distance is measured on a *canonical* form rather than on the raw text.
//! OCR does not produce random typos: it produces a small, well-known set of
//! shape confusions, and treating those as free is what separates "the engine
//! misread this word" from "the user typed a different word". `1nvo1ce` and
//! `invoice` are the same word seen through a bad scan and come out at
//! distance 0; `invoice` and `invoke` are two words and stay apart.

/// The largest edit budget `--fuzzy-distance` will accept.
///
/// Every additional edit widens the candidate set roughly geometrically while
/// the matches it adds stop resembling the query. Three is already past the
/// point where results are useful and is here as a guard rail, not a
/// recommendation.
pub const MAX_DISTANCE: u32 = 3;

/// Default edit budget for a term of `len` characters.
///
/// Short words are mostly *distinct* words: at four characters a single edit
/// already reaches dozens of unrelated entries, and at three it reaches most
/// of the alphabet. Long words are the opposite — they carry enough signal
/// that two edits still name the same word. So the budget scales with length
/// instead of being one number for every query.
pub fn budget(len: usize) -> u32 {
    match len {
        0..=3 => 0,
        4..=6 => 1,
        _ => 2,
    }
}

/// How many trailing characters a stemmer is assumed to take off a word.
///
/// Porter strips a suffix and leaves a prefix: `invoice` is indexed as
/// `invoic`, `balance` as `balanc`. Two characters covers `-e`, `-s`, `-es`,
/// `-ed` and — with the doubled consonant collapsed — `-ing` on the words this
/// matters for. It is deliberately not larger: every character forgiven here
/// is a character that stops distinguishing one word from another.
pub const STEM_SUFFIX: usize = 2;

/// Edit distance between `query` and `candidate`, or `None` past `max`.
///
/// Both sides go through `canonical` first, so an OCR shape confusion costs
/// nothing and only genuine differences are charged.
pub fn distance_within(query: &str, candidate: &str, max: u32) -> Option<u32> {
    let a = canonical(query);
    let b = canonical(candidate);
    osa_within(&a, &b, max as usize, 0).map(|d| d as u32)
}

/// [`distance_within`] against a *stem* rather than a whole word.
///
/// The vocabulary an FTS5 porter index exposes holds stems, but the user types
/// whole words, so a straight comparison charges for a suffix nobody got
/// wrong: `invoce` against the stored `invoic` is two edits as written and one
/// once the `-e` the stemmer already removed stops counting. So up to
/// [`STEM_SUFFIX`] characters may be dropped from the end of `query` for free,
/// and the best of those alignments wins.
///
/// The forgiveness is one-directional and therefore so is this function —
/// `stem` is the indexed side, `query` is the typed side, and swapping them
/// asks a different question.
pub fn stem_distance_within(query: &str, stem: &str, max: u32) -> Option<u32> {
    let a = canonical(query);
    let b = canonical(stem);
    osa_within(&a, &b, max as usize, STEM_SUFFIX).map(|d| d as u32)
}

/// Fold a word into the form OCR cannot distinguish.
///
/// Two kinds of confusion, in the order they have to run. First the ligature
/// rules, on lowercased but otherwise untouched text: `rn` reads as `m`, `cl`
/// as `d`, `vv` as `w`. Then the stroke rules — `I`, `l`, `1`, `|` and, once a
/// low-resolution scan loses the dot, `i` are all one vertical mark.
///
/// Running the ligatures first is what keeps them honest. Folded the other way
/// round, the `i` in `invoice` becomes a stroke, that stroke plus the
/// preceding `c` reads as the ligature `cl`, and a word with no `cl` in it
/// collapses to `lnvode`. Ligatures are about the shapes *as written*, so they
/// get the text as written.
///
/// This is deliberately lossy in both directions — `2024` folds to `zoz4`, and
/// `in` and `ln` become the same two letters — and that is fine: both sides of
/// every comparison are folded the same way, so the only pairs it brings
/// together are pairs a scanner really does confuse.
fn canonical(s: &str) -> Vec<char> {
    s.to_lowercase()
        .replace("rn", "m")
        .replace("cl", "d")
        .replace("vv", "w")
        .chars()
        .map(|c| match c {
            '1' | 'i' | '|' => 'l',
            '0' => 'o',
            '5' => 's',
            '8' => 'b',
            '2' => 'z',
            '6' => 'g',
            other => other,
        })
        .collect()
}

/// Optimal string alignment distance, abandoned once every path exceeds `max`.
///
/// OSA rather than full Damerau-Levenshtein: it charges a transposition once
/// but refuses to edit the transposed characters again afterwards. The
/// difference only shows up at distances we never accept, and OSA needs two
/// previous rows instead of the whole matrix.
///
/// `free_tail` is how many characters may be cut off the end of `a` at no
/// cost. Zero is the plain distance — the answer in the bottom-right cell.
/// Anything higher takes the best of the last `free_tail + 1` cells of the
/// final column, each of which is a prefix of `a` measured against all of `b`.
fn osa_within(a: &[char], b: &[char], max: usize, free_tail: usize) -> Option<usize> {
    let (n, m) = (a.len(), b.len());
    if m > n + max || n > m + max + free_tail {
        return None;
    }
    if max == 0 && free_tail == 0 {
        return (a == b).then_some(0);
    }
    let first_scored = n.saturating_sub(free_tail);
    // Row 0 is the empty prefix of `a`, which only counts when the whole of
    // `a` is inside the free tail.
    let mut best = (first_scored == 0).then_some(m);

    let mut prev2 = vec![0usize; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];

    for i in 1..=n {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(prev2[j - 2] + 1);
            }
            cur[j] = d;
            row_min = row_min.min(d);
        }
        if i >= first_scored {
            best = Some(best.map_or(cur[m], |b| b.min(cur[m])));
        }
        // Every alignment of the first `i` characters already costs more than
        // the budget, and a row never improves on the one above it: nothing
        // further down can come back under.
        if row_min > max {
            break;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    best.filter(|d| *d <= max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_scales_with_word_length() {
        assert_eq!(budget(0), 0);
        assert_eq!(budget(3), 0);
        assert_eq!(budget(4), 1);
        assert_eq!(budget(6), 1);
        assert_eq!(budget(7), 2);
        assert_eq!(budget(40), 2);
    }

    #[test]
    fn identical_words_are_distance_zero() {
        assert_eq!(distance_within("invoice", "invoice", 2), Some(0));
    }

    #[test]
    fn one_substitution_costs_one() {
        assert_eq!(distance_within("invoice", "invoige", 2), Some(1));
    }

    #[test]
    fn one_deletion_and_one_insertion_each_cost_one() {
        assert_eq!(distance_within("invoice", "invice", 2), Some(1));
        assert_eq!(distance_within("invoice", "invoicce", 2), Some(1));
    }

    #[test]
    fn a_transposition_costs_one_edit_not_two() {
        assert_eq!(distance_within("invoice", "invocie", 1), Some(1));
    }

    #[test]
    fn beyond_the_budget_is_none() {
        assert_eq!(distance_within("invoice", "invoke", 1), None);
        assert_eq!(distance_within("invoice", "invoke", 2), Some(2));
    }

    #[test]
    fn a_zero_budget_still_accepts_an_exact_match() {
        assert_eq!(distance_within("cat", "cat", 0), Some(0));
        assert_eq!(distance_within("cat", "car", 0), None);
    }

    #[test]
    fn length_alone_rules_out_distant_pairs() {
        assert_eq!(distance_within("a", "abcdefgh", 2), None);
    }

    #[test]
    fn ocr_shape_confusions_are_free() {
        for (misread, word) in [
            ("1nvo1ce", "invoice"),
            ("INVOICE", "invoice"),
            ("0ctober", "october"),
            ("5ummary", "summary"),
            ("8alance", "balance"),
            ("modem", "modern"),
            ("dear", "clear"),
        ] {
            assert_eq!(
                distance_within(misread, word, 0),
                Some(0),
                "{misread} should read as {word}"
            );
        }
    }

    #[test]
    fn folding_does_not_merge_genuinely_different_words() {
        assert_eq!(distance_within("invoice", "invoke", 0), None);
        assert_eq!(distance_within("cat", "dog", 2), None);
    }

    #[test]
    fn a_shape_confusion_and_a_typo_together_cost_only_the_typo() {
        assert_eq!(distance_within("1nvo1re", "invoice", 1), Some(1));
    }

    #[test]
    fn the_empty_query_matches_only_the_empty_word() {
        assert_eq!(distance_within("", "", 0), Some(0));
        assert_eq!(distance_within("", "ab", 2), Some(2));
    }

    #[test]
    fn distance_is_symmetric() {
        assert_eq!(
            distance_within("invoice", "invoces", 2),
            distance_within("invoces", "invoice", 2)
        );
    }

    #[test]
    fn a_stem_is_not_charged_for_the_suffix_it_lost() {
        // What the porter index actually stores for these words.
        assert_eq!(stem_distance_within("invoice", "invoic", 0), Some(0));
        assert_eq!(stem_distance_within("balance", "balanc", 0), Some(0));
        // ...and a typo on top of the stemming still costs exactly one.
        assert_eq!(stem_distance_within("invoce", "invoic", 1), Some(1));
    }

    #[test]
    fn the_free_tail_does_not_stretch_past_two_characters() {
        // `-es` is inside the allowance and costs nothing; `-ing` is not.
        assert_eq!(stem_distance_within("invoices", "invoic", 0), Some(0));
        assert_eq!(stem_distance_within("invoicing", "invoic", 0), None);
        assert_eq!(stem_distance_within("invoicing", "invoic", 1), Some(1));
    }

    #[test]
    fn a_stem_still_has_to_start_the_same_way() {
        assert_eq!(stem_distance_within("invoice", "balanc", 2), None);
        assert_eq!(stem_distance_within("invoice", "inv", 2), Some(2));
        assert_eq!(stem_distance_within("invoice", "bal", 2), None);
    }

    #[test]
    fn non_ascii_words_are_compared_by_character_not_byte() {
        // A two-byte character is one edit, not two: comparing `Vec<char>`
        // rather than bytes is the whole reason this holds.
        assert_eq!(distance_within("naslov", "nasloč", 1), Some(1));
        assert_eq!(distance_within("ČAKOVEC", "čakovec", 0), Some(0));
    }
}
