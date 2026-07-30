//! A markdown subset, for showing model responses as something other than a
//! wall of text.
//!
//! Models write markdown whether or not anything renders it, so the choice is
//! between rendering it and leaving `##` and `**` on screen as punctuation. This
//! covers what that output actually contains — headings, fenced code, lists,
//! quotes, rules, and inline code/bold/italic/links — and nothing else.
//!
//! **Anything outside the subset falls through as literal text.** Nested
//! emphasis, reference links, setext headings, tables, and HTML are not parsed;
//! they render as what they are rather than breaking. That is the trade for a
//! parser small enough to read, in a codebase that hand-rolls its wrapping,
//! highlighting, and diffing too.
//!
//! [`inline`] returns rewritten text *plus* runs over it, rather than runs over
//! the source. Markdown removes characters — `**bold**` displays as `bold` — so
//! runs indexed into the source would leave the markers on screen. Producing the
//! display text first keeps the same tiling invariant [`crate::highlight::spans`]
//! has, so wrapping and slicing work identically for both.

use std::ops::Range;

/// A block-level element. One of these becomes one or more rendered lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading {
        /// 1–6, as in the number of `#`.
        level: u8,
        text: String,
    },
    /// Soft-wrapped prose: source line breaks inside one have been joined, so
    /// the renderer can reflow it to the terminal width.
    Paragraph(String),
    Code {
        /// The fence's info string, when it named one.
        language: Option<String>,
        text: String,
    },
    Item {
        /// Nesting level, from the source indent.
        depth: usize,
        /// The number for an ordered item; `None` for a bullet.
        ordinal: Option<u32>,
        text: String,
    },
    Quote(String),
    Rule,
}

/// How a run of inline text is styled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    Plain,
    Strong,
    Italic,
    Code,
    Link,
}

/// Text ready to display, and how to style it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inline {
    /// The text to show, with markers removed.
    pub text: String,
    /// Runs over `text`: ordered, non-overlapping, covering it exactly.
    pub runs: Vec<(Range<usize>, Emphasis)>,
}

/// Split `source` into blocks.
pub fn parse(source: &str) -> Vec<Block> {
    let lines: Vec<&str> = source.lines().collect();
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // A fence swallows everything up to its close, markdown or not — the
        // whole point of a code block is that its contents are not parsed.
        if let Some(language) = opening_fence(trimmed) {
            flush(&mut paragraph, &mut blocks);
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !is_fence(lines[i].trim()) {
                body.push(lines[i]);
                i += 1;
            }
            // Past the end means the fence was never closed; take what there is
            // rather than dropping the block.
            i += usize::from(i < lines.len());
            blocks.push(Block::Code {
                language,
                text: body.join("\n"),
            });
            continue;
        }

        if trimmed.is_empty() {
            flush(&mut paragraph, &mut blocks);
            i += 1;
            continue;
        }

        // Before the list check: `---` and `***` would otherwise read as items.
        if is_rule(trimmed) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Rule);
            i += 1;
            continue;
        }

        if let Some((level, text)) = heading(trimmed) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Heading {
                level,
                text: text.to_string(),
            });
            i += 1;
            continue;
        }

        if trimmed.starts_with('>') {
            flush(&mut paragraph, &mut blocks);
            // Consecutive quote lines are one quote, so it reflows as prose
            // rather than keeping the model's line endings.
            let mut quoted = Vec::new();
            while i < lines.len() && lines[i].trim().starts_with('>') {
                quoted.push(lines[i].trim().trim_start_matches('>').trim());
                i += 1;
            }
            blocks.push(Block::Quote(quoted.join(" ")));
            continue;
        }

        if let Some((ordinal, text)) = item(trimmed) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Item {
                depth: depth_of(line),
                ordinal,
                text: text.to_string(),
            });
            i += 1;
            continue;
        }

        paragraph.push(trimmed);
        i += 1;
    }

    flush(&mut paragraph, &mut blocks);
    blocks
}

/// Close the open paragraph, if any.
///
/// Joined with a space: markdown soft-wraps, so the source's line endings are
/// not the author's paragraph breaks and keeping them would stop the text
/// reflowing to the terminal width.
fn flush(paragraph: &mut Vec<&str>, blocks: &mut Vec<Block>) {
    if !paragraph.is_empty() {
        blocks.push(Block::Paragraph(paragraph.join(" ")));
        paragraph.clear();
    }
}

/// The info string of an opening fence, or `None` if this is not one.
fn opening_fence(trimmed: &str) -> Option<Option<String>> {
    let info = trimmed
        .strip_prefix("```")
        .or_else(|| trimmed.strip_prefix("~~~"))?;
    let info = info.trim();
    Some((!info.is_empty()).then(|| info.to_string()))
}

fn is_fence(trimmed: &str) -> bool {
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// `---`, `***`, `___` — three or more of one mark, and nothing else.
fn is_rule(trimmed: &str) -> bool {
    ['-', '*', '_'].iter().any(|mark| {
        let stripped: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        stripped.len() >= 3 && stripped.chars().all(|c| c == *mark)
    })
}

fn heading(trimmed: &str) -> Option<(u8, &str)> {
    let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    // A space is required, so `#hashtag` stays prose.
    let rest = trimmed[hashes..].strip_prefix(' ')?;
    Some((hashes as u8, rest.trim()))
}

/// A list item's ordinal (`None` for a bullet) and its text.
fn item(trimmed: &str) -> Option<(Option<u32>, &str)> {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            return Some((None, rest.trim()));
        }
    }
    // `12. text`
    let digits = trimmed.len()
        - trimmed
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    if digits > 0 {
        let rest = trimmed[digits..].strip_prefix(". ")?;
        return Some((trimmed[..digits].parse().ok(), rest.trim()));
    }
    None
}

/// Nesting from the source indent. Two spaces per level, tabs counting as two.
fn depth_of(line: &str) -> usize {
    let spaces: usize = line
        .chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 2 } else { 1 })
        .sum();
    spaces / 2
}

/// Rewrite one line of inline markdown into display text and its styling.
pub fn inline(source: &str) -> Inline {
    let mut text = String::new();
    let mut runs: Vec<(Range<usize>, Emphasis)> = Vec::new();
    // Start, in *display* coordinates, of the unstyled run being accumulated.
    let mut plain: Option<usize> = None;
    let mut i = 0;

    while i < source.len() {
        let rest = &source[i..];

        // `**` before `*`, or every bold marker would open an empty italic.
        let matched = [
            ("`", Emphasis::Code),
            ("**", Emphasis::Strong),
            ("__", Emphasis::Strong),
            ("*", Emphasis::Italic),
            ("_", Emphasis::Italic),
        ]
        .into_iter()
        .find_map(|(marker, style)| delimited(rest, marker).map(|body| (marker, style, body)));

        if let Some((marker, style, body)) = matched {
            close_plain(&mut runs, &mut plain, text.len());
            let start = text.len();
            text.push_str(body);
            runs.push((start..text.len(), style));
            i += body.len() + marker.len() * 2;
            continue;
        }

        if let Some((label, url)) = link(rest) {
            close_plain(&mut runs, &mut plain, text.len());
            let start = text.len();
            // Nothing is clickable in a terminal, so dropping the destination
            // would lose the only part that cannot be inferred.
            text.push_str(label);
            text.push_str(&format!(" ({url})"));
            runs.push((start..text.len(), Emphasis::Link));
            i += label.len() + url.len() + 4;
            continue;
        }

        let ch = rest.chars().next().expect("i is a char boundary");
        plain.get_or_insert(text.len());
        text.push(ch);
        i += ch.len_utf8();
    }

    close_plain(&mut runs, &mut plain, text.len());
    Inline { text, runs }
}

fn close_plain(runs: &mut Vec<(Range<usize>, Emphasis)>, plain: &mut Option<usize>, end: usize) {
    if let Some(start) = plain.take()
        && start < end
    {
        runs.push((start..end, Emphasis::Plain));
    }
}

/// The text between a pair of `marker`s at the front of `rest`.
///
/// `None` when the marker does not open here or is never closed, so an unmatched
/// `**` stays literal instead of swallowing the rest of the line.
fn delimited<'a>(rest: &'a str, marker: &str) -> Option<&'a str> {
    let after = rest.strip_prefix(marker)?;
    let end = after.find(marker)?;
    // An empty body is not emphasis; `****` is just punctuation.
    (end > 0).then(|| &after[..end])
}

/// `[label](url)` at the front of `rest`.
fn link(rest: &str) -> Option<(&str, &str)> {
    let after = rest.strip_prefix('[')?;
    let close = after.find("](")?;
    let url_start = close + 2;
    let end = after[url_start..].find(')')?;
    let label = &after[..close];
    let url = &after[url_start..url_start + end];
    (!label.is_empty() && !url.is_empty()).then_some((label, url))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(text: &str) -> Block {
        Block::Paragraph(text.into())
    }

    /// The runs, as (text, style) pairs — what a renderer consumes.
    fn runs(source: &str) -> Vec<(&str, Emphasis)> {
        // Leaked so the slices can borrow it; test-only, and bounded.
        let parsed: &'static Inline = Box::leak(Box::new(inline(source)));
        parsed
            .runs
            .iter()
            .map(|(range, style)| (&parsed.text[range.clone()], *style))
            .collect()
    }

    /// The invariant every caller relies on: runs tile the display text exactly.
    fn assert_tiles(source: &str) {
        let parsed = inline(source);
        let mut at = 0;
        for (range, _) in &parsed.runs {
            assert_eq!(range.start, at, "gap or overlap in {source:?}: {parsed:?}");
            assert!(range.end > range.start, "empty run in {source:?}");
            at = range.end;
        }
        assert_eq!(
            at,
            parsed.text.len(),
            "runs must reach the end of {source:?}"
        );
    }

    #[test]
    fn headings_carry_their_level() {
        assert_eq!(
            parse("# One\n### Three"),
            vec![
                Block::Heading {
                    level: 1,
                    text: "One".into()
                },
                Block::Heading {
                    level: 3,
                    text: "Three".into()
                },
            ]
        );
    }

    #[test]
    fn a_hash_without_a_space_is_prose() {
        assert_eq!(parse("#hashtag"), vec![para("#hashtag")]);
        assert_eq!(parse("####### seven"), vec![para("####### seven")]);
    }

    #[test]
    fn consecutive_lines_join_into_one_paragraph() {
        // Markdown soft-wraps: the source's line endings are not the author's,
        // and keeping them would stop the text reflowing to the terminal.
        assert_eq!(
            parse("one two\nthree four"),
            vec![para("one two three four")]
        );
    }

    #[test]
    fn a_blank_line_separates_paragraphs() {
        assert_eq!(parse("one\n\ntwo"), vec![para("one"), para("two")]);
    }

    #[test]
    fn a_fence_keeps_its_language_and_body_verbatim() {
        let source = "```rust\nfn main() {\n    // # not a heading\n}\n```";
        assert_eq!(
            parse(source),
            vec![Block::Code {
                language: Some("rust".into()),
                text: "fn main() {\n    // # not a heading\n}".into(),
            }]
        );
    }

    #[test]
    fn a_fence_may_have_no_language() {
        assert_eq!(
            parse("```\nplain\n```"),
            vec![Block::Code {
                language: None,
                text: "plain".into()
            }]
        );
    }

    #[test]
    fn an_unclosed_fence_keeps_what_it_has() {
        // Dropping the block would lose the output entirely; a truncated reply
        // is the likeliest cause and the contents still matter.
        assert_eq!(
            parse("```sh\necho hi"),
            vec![Block::Code {
                language: Some("sh".into()),
                text: "echo hi".into()
            }]
        );
    }

    #[test]
    fn bullets_and_ordinals_are_distinguished() {
        assert_eq!(
            parse("- one\n2. two"),
            vec![
                Block::Item {
                    depth: 0,
                    ordinal: None,
                    text: "one".into()
                },
                Block::Item {
                    depth: 0,
                    ordinal: Some(2),
                    text: "two".into()
                },
            ]
        );
    }

    #[test]
    fn indentation_nests_items() {
        match &parse("- top\n  - nested")[1] {
            Block::Item { depth, .. } => assert_eq!(*depth, 1),
            other => panic!("expected an item, got {other:?}"),
        }
    }

    #[test]
    fn rules_are_not_mistaken_for_bullets() {
        assert_eq!(parse("---"), vec![Block::Rule]);
        assert_eq!(parse("***"), vec![Block::Rule]);
        assert_eq!(parse("___"), vec![Block::Rule]);
        // Two is not enough to be a rule.
        assert_eq!(parse("--"), vec![para("--")]);
    }

    #[test]
    fn consecutive_quote_lines_are_one_quote() {
        assert_eq!(parse("> one\n> two"), vec![Block::Quote("one two".into())]);
    }

    #[test]
    fn inline_markers_are_removed_from_the_display_text() {
        assert_eq!(inline("**bold** and `code`").text, "bold and code");
    }

    #[test]
    fn each_inline_style_gets_its_run() {
        assert_eq!(
            runs("a **b** c `d` e *f*"),
            vec![
                ("a ", Emphasis::Plain),
                ("b", Emphasis::Strong),
                (" c ", Emphasis::Plain),
                ("d", Emphasis::Code),
                (" e ", Emphasis::Plain),
                ("f", Emphasis::Italic),
            ]
        );
    }

    #[test]
    fn bold_wins_over_italic() {
        // `*` checked first would open an empty italic on every bold marker.
        assert_eq!(runs("**b**"), vec![("b", Emphasis::Strong)]);
        assert_eq!(runs("__b__"), vec![("b", Emphasis::Strong)]);
    }

    #[test]
    fn a_link_keeps_its_text_and_destination() {
        // Nothing is clickable in a terminal, so the URL has to survive.
        let parsed = inline("see [the docs](https://example.com) now");
        assert_eq!(parsed.text, "see the docs (https://example.com) now");
        assert!(
            parsed
                .runs
                .iter()
                .any(|(_, style)| *style == Emphasis::Link)
        );
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        // Swallowing to the end of the line would be worse than showing an
        // asterisk the model meant literally.
        assert_eq!(inline("2 ** 8 is 256").text, "2 ** 8 is 256");
        assert_eq!(inline("a * b").text, "a * b");
        assert_eq!(inline("[not a link").text, "[not a link");
        assert_eq!(inline("****").text, "****");
    }

    #[test]
    fn runs_always_tile_the_display_text() {
        for source in [
            "",
            "plain",
            "**bold**",
            "`code` at the start",
            "ends with **bold**",
            "*héllo* wörld with `ünicode`",
            "🎉 **emoji** in bold",
            "[link](https://example.com)",
            "2 ** 8",
            "**a** **b** **c**",
        ] {
            assert_tiles(source);
        }
    }
}
