//! A small multi-line text buffer with a grapheme-aware cursor.
//!
//! Wrapping is done here rather than delegated to `Paragraph` so that the
//! rendered cursor position is guaranteed to match the wrapped text.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::wrap;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Input {
    text: String,
    /// Byte offset into `text`; always on a grapheme boundary.
    cursor: usize,
}

/// The result of laying out the buffer at a given width.
pub struct Layout {
    pub rows: Vec<String>,
    /// Cursor position as (row, column) in cells.
    pub cursor: (u16, u16),
}

impl Input {
    /// The raw buffer contents. The UI renders via [`Input::layout`] instead,
    /// so this is currently only used by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary(self.cursor) {
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if let Some(next) = self.next_boundary(self.cursor) {
            self.text.replace_range(self.cursor..next, "");
        }
    }

    /// Delete from the cursor back to the start of the previous word.
    pub fn delete_word_before(&mut self) {
        let start = self.word_start_before(self.cursor);
        if start < self.cursor {
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    /// Delete from the cursor forward to the end of the next word.
    ///
    /// The mirror of [`Input::delete_word_before`], and it leaves the cursor
    /// where it is: the text closes up from the right, the way `Delete` does.
    pub fn delete_word_after(&mut self) {
        let end = self.word_end_after(self.cursor);
        if end > self.cursor {
            self.text.replace_range(self.cursor..end, "");
        }
    }

    /// Delete from the start of the current line up to the cursor.
    pub fn delete_to_line_start(&mut self) {
        let start = self.line_start(self.cursor);
        if start < self.cursor {
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    pub fn move_left(&mut self) {
        if let Some(prev) = self.prev_boundary(self.cursor) {
            self.cursor = prev;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(next) = self.next_boundary(self.cursor) {
            self.cursor = next;
        }
    }

    pub fn move_word_left(&mut self) {
        self.cursor = self.word_start_before(self.cursor);
    }

    pub fn move_word_right(&mut self) {
        self.cursor = self.word_end_after(self.cursor);
    }

    pub fn move_to_line_start(&mut self) {
        self.cursor = self.line_start(self.cursor);
    }

    pub fn move_to_line_end(&mut self) {
        self.cursor = self.line_end(self.cursor);
    }

    pub fn move_to_start(&mut self) {
        self.cursor = 0;
    }

    pub fn move_to_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Lay the buffer out at `width` cells, soft-wrapping on word boundaries
    /// and hard-wrapping words that are longer than a line.
    pub fn layout(&self, width: u16) -> Layout {
        let width = width.max(1) as usize;
        let mut rows: Vec<String> = Vec::new();
        let mut cursor = (0u16, 0u16);

        // Hard lines first; `split` keeps a trailing empty segment for "a\n".
        let mut consumed = 0usize;
        for (i, logical) in self.text.split('\n').enumerate() {
            if i > 0 {
                consumed += 1; // the '\n' itself
            }
            let start_row = rows.len();
            let wrapped = wrap::line(logical, width);

            // Where does the cursor fall within this logical line?
            let line_start = consumed;
            let line_end = consumed + logical.len();
            if self.cursor >= line_start && self.cursor <= line_end {
                let offset = self.cursor - line_start;
                let (r, c) = locate(&wrapped, offset);
                cursor = ((start_row + r) as u16, c as u16);
            }

            rows.extend(wrapped.into_iter().map(|r| r.text));
            consumed = line_end;
        }

        if rows.is_empty() {
            rows.push(String::new());
        }
        Layout { rows, cursor }
    }

    fn prev_boundary(&self, at: usize) -> Option<usize> {
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
    }

    fn next_boundary(&self, at: usize) -> Option<usize> {
        self.text[at..].graphemes(true).next().map(|g| at + g.len())
    }

    fn line_start(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |i| at + i)
    }

    fn word_start_before(&self, at: usize) -> usize {
        let head = &self.text[..at];
        let mut idx = at;
        // Skip whitespace immediately before the cursor, then the word itself.
        for (i, g) in head.grapheme_indices(true).rev() {
            if g.chars().all(char::is_whitespace) {
                idx = i;
            } else {
                break;
            }
        }
        for (i, g) in self.text[..idx].grapheme_indices(true).rev() {
            if g.chars().all(char::is_whitespace) {
                break;
            }
            idx = i;
        }
        idx
    }

    fn word_end_after(&self, at: usize) -> usize {
        let mut idx = at;
        let mut it = self.text[at..].grapheme_indices(true).peekable();
        while let Some(&(i, g)) = it.peek() {
            if g.chars().all(char::is_whitespace) {
                idx = at + i + g.len();
                it.next();
            } else {
                break;
            }
        }
        for (i, g) in self.text[idx..].grapheme_indices(true) {
            if g.chars().all(char::is_whitespace) {
                return idx + i;
            }
        }
        self.text.len()
    }
}

/// Map a byte offset within a logical line to (row, column) in its wrapped rows.
fn locate(rows: &[wrap::Row], offset: usize) -> (usize, usize) {
    for (i, row) in rows.iter().enumerate().rev() {
        if offset >= row.start {
            let within = (offset - row.start).min(row.text.len());
            let col = row.text.get(..within).unwrap_or(&row.text).width();
            return (i, col);
        }
    }
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(input: &Input, width: u16) -> Vec<String> {
        input.layout(width).rows
    }

    #[test]
    fn empty_input_has_one_row_and_origin_cursor() {
        let input = Input::default();
        let layout = input.layout(20);
        assert_eq!(layout.rows, vec![String::new()]);
        assert_eq!(layout.cursor, (0, 0));
    }

    #[test]
    fn cursor_tracks_typed_text() {
        let mut input = Input::default();
        for c in "hello".chars() {
            input.insert_char(c);
        }
        assert_eq!(input.layout(20).cursor, (0, 5));
        input.move_left();
        input.move_left();
        assert_eq!(input.layout(20).cursor, (0, 3));
    }

    #[test]
    fn newline_starts_a_new_row() {
        let mut input = Input::default();
        input.insert_str("ab\ncd");
        let layout = input.layout(20);
        assert_eq!(layout.rows, vec!["ab".to_string(), "cd".to_string()]);
        assert_eq!(layout.cursor, (1, 2));
    }

    #[test]
    fn wraps_on_word_boundaries() {
        let mut input = Input::default();
        input.insert_str("the quick brown fox");
        assert_eq!(rows_of(&input, 10), vec!["the quick ", "brown fox"]);
    }

    #[test]
    fn hard_wraps_words_longer_than_the_row() {
        let mut input = Input::default();
        input.insert_str("supercalifragilistic");
        assert_eq!(rows_of(&input, 8), vec!["supercal", "ifragili", "stic"]);
    }

    #[test]
    fn cursor_lands_on_the_wrapped_row() {
        let mut input = Input::default();
        input.insert_str("the quick brown fox");
        // Cursor sits at the end, which is on the second wrapped row.
        assert_eq!(input.layout(10).cursor, (1, 9));
    }

    #[test]
    fn backspace_respects_grapheme_clusters() {
        let mut input = Input::default();
        input.insert_str("a👍🏽b");
        input.backspace();
        assert_eq!(input.text(), "a👍🏽");
        input.backspace();
        assert_eq!(input.text(), "a");
    }

    #[test]
    fn word_deletion_removes_trailing_word_and_space() {
        let mut input = Input::default();
        input.insert_str("hello world");
        input.delete_word_before();
        assert_eq!(input.text(), "hello ");
        input.delete_word_before();
        assert_eq!(input.text(), "");
    }

    #[test]
    fn forward_word_deletion_takes_the_word_and_its_leading_space() {
        let mut input = Input::default();
        input.insert_str("hello world again");
        input.move_to_start();
        input.delete_word_after();
        assert_eq!(input.text(), " world again");
        input.delete_word_after();
        assert_eq!(input.text(), " again");
    }

    #[test]
    fn forward_word_deletion_leaves_the_cursor_put() {
        let mut input = Input::default();
        input.insert_str("keep drop rest");
        input.move_to_start();
        input.move_word_right();
        let before = input.layout(40).cursor;
        input.delete_word_after();
        assert_eq!(input.text(), "keep rest");
        assert_eq!(input.layout(40).cursor, before, "cursor should not move");
    }

    #[test]
    fn forward_word_deletion_at_the_end_does_nothing() {
        let mut input = Input::default();
        input.insert_str("done");
        input.move_to_end();
        input.delete_word_after();
        assert_eq!(input.text(), "done");
    }

    #[test]
    fn word_motions_step_over_one_word_at_a_time() {
        let mut input = Input::default();
        input.insert_str("alpha beta gamma");
        input.move_to_start();

        input.move_word_right();
        assert_eq!(input.layout(40).cursor, (0, 5), "end of `alpha`");
        input.move_word_right();
        assert_eq!(input.layout(40).cursor, (0, 10), "end of `beta`");

        input.move_word_left();
        assert_eq!(input.layout(40).cursor, (0, 6), "start of `beta`");
        input.move_word_left();
        assert_eq!(input.layout(40).cursor, (0, 0), "start of `alpha`");
    }

    #[test]
    fn word_motions_stop_at_the_ends_of_the_buffer() {
        let mut input = Input::default();
        input.insert_str("one two");
        input.move_to_start();
        for _ in 0..5 {
            input.move_word_left();
        }
        assert_eq!(input.layout(40).cursor, (0, 0));
        for _ in 0..5 {
            input.move_word_right();
        }
        assert_eq!(input.layout(40).cursor, (0, 7));
    }

    #[test]
    fn line_motions_are_per_logical_line() {
        let mut input = Input::default();
        input.insert_str("first\nsecond");
        input.move_to_line_start();
        assert_eq!(input.layout(40).cursor, (1, 0));
        input.move_to_line_end();
        assert_eq!(input.layout(40).cursor, (1, 6));
    }

    #[test]
    fn clear_resets_the_buffer() {
        let mut input = Input::default();
        input.insert_str("prompt");
        input.clear();
        assert!(input.is_blank());
        assert_eq!(input.layout(10).cursor, (0, 0));
    }

    #[test]
    fn wide_graphemes_count_two_cells() {
        let mut input = Input::default();
        input.insert_str("日本語");
        assert_eq!(input.layout(20).cursor, (0, 6));
        assert_eq!(
            rows_of(&input, 4),
            vec!["日本".to_string(), "語".to_string()]
        );
    }
}
