//! Word wrapping shared by the prompt editor and the transcript.
//!
//! Both need wrapping that we can reason about exactly: the editor to place the
//! cursor, the transcript to know its rendered height for scroll clamping.
//! `Paragraph`'s own wrapping only exposes that height behind an unstable
//! feature, so we wrap up front and hand it text that already fits.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// A wrapped row: its text, and the byte offset in the source line where it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    pub start: usize,
}

impl Row {
    fn new(text: String, start: usize) -> Self {
        Self { text, start }
    }
}

/// Wrap one logical line (no `\n`) to `width` cells.
///
/// Breaks at spaces where possible and hard-breaks words too long to fit.
/// Always returns at least one row, so an empty line still occupies a row.
pub fn line(text: &str, width: usize) -> Vec<Row> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![Row::new(String::new(), 0)];
    }

    let mut rows: Vec<Row> = Vec::new();
    let mut current = String::new();
    let mut current_start = 0usize;
    let mut current_width = 0usize;
    // The word being built up. Held back so it can move to the next row whole.
    let mut pending: Vec<(usize, &str, usize)> = Vec::new();
    // Tracked alongside `pending` rather than summed per grapheme, which made
    // wrapping quadratic in word length.
    let mut pending_width = 0usize;

    for (i, grapheme) in text.grapheme_indices(true) {
        let w = cell_width(grapheme);

        if current_width + pending_width + w > width {
            if !current.is_empty() && !pending.is_empty() {
                // Break before the pending word and carry it to the next row.
                let carry_start = pending[0].0;
                rows.push(Row::new(std::mem::take(&mut current), current_start));
                current_start = carry_start;
                current_width = 0;
                flush(&mut current, &mut current_width, &mut pending, &mut pending_width);
            } else {
                flush(&mut current, &mut current_width, &mut pending, &mut pending_width);
                if current.is_empty() {
                    // One grapheme wider than the whole row; give it its own row.
                    rows.push(Row::new(grapheme.to_string(), i));
                    current_start = i + grapheme.len();
                    continue;
                }
                // A word longer than a row: hard-break it here.
                rows.push(Row::new(std::mem::take(&mut current), current_start));
                current_start = i;
                current_width = 0;
            }
        }

        if grapheme == " " || grapheme == "\t" {
            // Whitespace terminates the pending word, so commit it.
            flush(&mut current, &mut current_width, &mut pending, &mut pending_width);
            current.push_str(grapheme);
            current_width += w;
        } else {
            pending.push((i, grapheme, w));
            pending_width += w;
        }
    }

    flush(&mut current, &mut current_width, &mut pending, &mut pending_width);
    // Only emit a trailing row if it holds something. The overlong-grapheme
    // path above emits its row directly, which would otherwise leave a blank
    // row here and inflate the rendered height.
    if !current.is_empty() || rows.is_empty() {
        rows.push(Row::new(current, current_start));
    }
    rows
}

/// Wrap text that may contain `\n`, returning the flattened rows.
pub fn text(input: &str, width: usize) -> Vec<String> {
    input
        .split('\n')
        .flat_map(|logical| line(logical, width).into_iter().map(|r| r.text))
        .collect()
}

fn flush(
    current: &mut String,
    current_width: &mut usize,
    pending: &mut Vec<(usize, &str, usize)>,
    pending_width: &mut usize,
) {
    for (_, grapheme, w) in pending.drain(..) {
        current.push_str(grapheme);
        *current_width += w;
    }
    *pending_width = 0;
}

/// Display width of a grapheme, treating unknown/zero-width non-controls as 1
/// so an unrenderable character still advances the cursor.
fn cell_width(grapheme: &str) -> usize {
    let w = grapheme.width();
    if w == 0 && !grapheme.chars().all(char::is_control) {
        1
    } else {
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn empty_line_still_occupies_one_row() {
        assert_eq!(texts(&line("", 10)), vec![""]);
    }

    #[test]
    fn short_line_is_untouched() {
        assert_eq!(texts(&line("hello", 10)), vec!["hello"]);
    }

    #[test]
    fn breaks_at_spaces() {
        assert_eq!(
            texts(&line("the quick brown fox", 10)),
            vec!["the quick ", "brown fox"]
        );
    }

    #[test]
    fn hard_breaks_overlong_words() {
        assert_eq!(
            texts(&line("supercalifragilistic", 8)),
            vec!["supercal", "ifragili", "stic"]
        );
    }

    #[test]
    fn rows_record_their_start_offsets() {
        let rows = line("the quick brown fox", 10);
        assert_eq!(rows[0].start, 0);
        assert_eq!(rows[1].start, 10);
    }

    #[test]
    fn wide_graphemes_take_two_cells() {
        assert_eq!(texts(&line("日本語", 4)), vec!["日本", "語"]);
    }

    #[test]
    fn no_row_exceeds_the_width() {
        let sample = "a bb ccc dddd eeeee ffffff ggggggg hhhhhhhh 日本語テスト mixed 12345";
        // Width 1 is excluded: a 2-cell grapheme cannot fit, and we keep the
        // character (letting the terminal clip it) rather than dropping it.
        for width in 2..=20 {
            for row in line(sample, width) {
                assert!(
                    row.text.width() <= width,
                    "width {width}: row {:?} is {} cells",
                    row.text,
                    row.text.width()
                );
            }
        }
    }

    #[test]
    fn width_one_keeps_wide_graphemes_rather_than_dropping_them() {
        let rows = line("日本", 1);
        assert_eq!(texts(&rows), vec!["日", "本"]);
    }

    #[test]
    fn wrapping_preserves_all_characters() {
        let sample = "the quick brown fox jumps over the lazy dog";
        for width in 1..=20 {
            let joined: String = line(sample, width).into_iter().map(|r| r.text).collect();
            assert_eq!(joined, sample, "characters lost at width {width}");
        }
    }

    #[test]
    fn text_splits_on_newlines() {
        assert_eq!(text("ab\n\ncd", 10), vec!["ab", "", "cd"]);
    }

    #[test]
    fn text_wraps_each_logical_line() {
        assert_eq!(
            text("aaa bbb\nccc ddd", 4),
            vec!["aaa ", "bbb", "ccc ", "ddd"]
        );
    }
}
