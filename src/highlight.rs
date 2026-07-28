//! Minimal syntax highlighting for file contents shown in the transcript.
//!
//! Deliberately small: a language is recognised from its file extension, and one
//! line at a time is split into comment / string / number / keyword runs. That
//! is enough to make a diff readable without a grammar engine and a multi-megabyte
//! syntax dump — the same trade every other primitive here makes.
//!
//! **Highlighting is stateless per line.** A multi-line string or a block comment
//! is coloured correctly on its first line and as ordinary code afterwards,
//! because nothing carries over between lines. The failure is cosmetic, and a
//! line-oriented tokeniser is what fits a renderer that already works line by
//! line: the transcript wraps, scrolls, and elides by line, so there is no whole
//! file to keep state across.
//!
//! [`spans`] returns semantic tokens rather than styles. Every colour in this
//! codebase is chosen in `ui`, and keeping this module free of `ratatui` is what
//! lets it be tested as plain data.

use std::ops::Range;

/// A language recognised well enough to colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    Go,
    Json,
    Toml,
    Shell,
    Markdown,
    /// Anything unrecognised. Highlights nothing; the whole line is one run.
    Plain,
}

/// What a run of characters is. `Plain` is everything with no special meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Comment,
    Str,
    Number,
    Keyword,
    Plain,
}

/// The name shown in a code block's header.
pub fn label(lang: Language) -> &'static str {
    match lang {
        Language::Rust => "rust",
        Language::Python => "python",
        Language::JavaScript => "javascript",
        Language::Go => "go",
        Language::Json => "json",
        Language::Toml => "toml",
        Language::Shell => "shell",
        Language::Markdown => "markdown",
        Language::Plain => "text",
    }
}

/// Guess a language from a file path.
pub fn detect(path: &str) -> Language {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);

    // A few files carry their type in the whole name rather than an extension.
    // These map to the language whose *rules* fit, not their own name: both use
    // `#` comments and quoted strings, which is all the tokeniser needs.
    if matches!(name, "Makefile" | "makefile" | "GNUmakefile" | "Dockerfile") {
        return Language::Shell;
    }

    // `rsplit_once` rather than `split_once`, so `foo.tar.gz` keys on `gz`, and
    // a dotfile with no extension (`.gitignore`) yields an empty extension
    // rather than its own name.
    let ext = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => "",
    };
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Language::Rust,
        "py" | "pyi" => Language::Python,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Language::JavaScript,
        "go" => Language::Go,
        "json" => Language::Json,
        "toml" => Language::Toml,
        "sh" | "bash" | "zsh" => Language::Shell,
        "md" | "markdown" => Language::Markdown,
        _ => Language::Plain,
    }
}

/// The lexical rules for one language.
struct Syntax {
    /// Prefixes that comment out the rest of the line.
    line_comment: &'static [&'static str],
    /// Characters that open (and close) a string literal.
    strings: &'static [char],
    keywords: &'static [&'static str],
    numbers: bool,
}

const NO_SYNTAX: Syntax = Syntax {
    line_comment: &[],
    strings: &[],
    keywords: &[],
    numbers: false,
};

fn syntax(lang: Language) -> Syntax {
    match lang {
        Language::Rust => Syntax {
            line_comment: &["//"],
            strings: &['"'],
            keywords: &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            ],
            numbers: true,
        },
        Language::Python => Syntax {
            line_comment: &["#"],
            strings: &['"', '\''],
            keywords: &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "True", "try", "while", "with", "yield",
            ],
            numbers: true,
        },
        Language::JavaScript => Syntax {
            line_comment: &["//"],
            strings: &['"', '\'', '`'],
            keywords: &[
                "as",
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "default",
                "delete",
                "do",
                "else",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "from",
                "function",
                "if",
                "import",
                "in",
                "instanceof",
                "interface",
                "let",
                "new",
                "null",
                "of",
                "return",
                "static",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "type",
                "typeof",
                "undefined",
                "var",
                "void",
                "while",
                "yield",
            ],
            numbers: true,
        },
        Language::Go => Syntax {
            line_comment: &["//"],
            strings: &['"', '`'],
            keywords: &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "fallthrough",
                "false",
                "for",
                "func",
                "go",
                "goto",
                "if",
                "import",
                "interface",
                "map",
                "nil",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "true",
                "type",
                "var",
            ],
            numbers: true,
        },
        Language::Json => Syntax {
            line_comment: &[],
            strings: &['"'],
            keywords: &["true", "false", "null"],
            numbers: true,
        },
        Language::Toml => Syntax {
            line_comment: &["#"],
            strings: &['"', '\''],
            keywords: &["true", "false"],
            numbers: true,
        },
        Language::Shell => Syntax {
            line_comment: &["#"],
            strings: &['"', '\''],
            keywords: &[
                "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
                "if", "in", "local", "return", "then", "until", "while",
            ],
            numbers: false,
        },
        // Prose. Colouring `#` as a comment would paint every heading, and
        // quotation marks are punctuation here, not literals.
        Language::Markdown | Language::Plain => NO_SYNTAX,
    }
}

/// Split one line into coloured runs.
///
/// The runs are ordered, non-overlapping, and cover the whole line, so a
/// renderer can concatenate them and be certain it reproduced the input. An
/// empty line yields no runs.
pub fn spans(line: &str, lang: Language) -> Vec<(Range<usize>, Token)> {
    let syntax = syntax(lang);
    let mut out: Vec<(Range<usize>, Token)> = Vec::new();
    // Start of the current uncoloured run, held open so adjacent plain
    // characters merge into one span instead of one span each.
    let mut plain: Option<usize> = None;
    let mut i = 0;

    while i < line.len() {
        let rest = &line[i..];
        let ch = rest.chars().next().expect("i is a char boundary");

        if syntax.line_comment.iter().any(|p| rest.starts_with(p)) {
            flush(&mut out, &mut plain, i);
            out.push((i..line.len(), Token::Comment));
            return out;
        }

        if syntax.strings.contains(&ch) {
            flush(&mut out, &mut plain, i);
            let end = scan_string(line, i, ch);
            out.push((i..end, Token::Str));
            i = end;
            continue;
        }

        if ch.is_alphabetic() || ch == '_' {
            let end = scan_while(line, i, |c| c.is_alphanumeric() || c == '_');
            if syntax.keywords.contains(&&line[i..end]) {
                flush(&mut out, &mut plain, i);
                out.push((i..end, Token::Keyword));
            } else {
                // Not a keyword: part of the surrounding plain run. Taken whole
                // so a digit inside an identifier is not read as a number.
                plain.get_or_insert(i);
            }
            i = end;
            continue;
        }

        if syntax.numbers && ch.is_ascii_digit() {
            flush(&mut out, &mut plain, i);
            let end = scan_while(line, i, |c| c.is_alphanumeric() || c == '.' || c == '_');
            out.push((i..end, Token::Number));
            i = end;
            continue;
        }

        plain.get_or_insert(i);
        i += ch.len_utf8();
    }

    flush(&mut out, &mut plain, line.len());
    out
}

/// Close the open plain run at `end`, if there is one.
fn flush(out: &mut Vec<(Range<usize>, Token)>, plain: &mut Option<usize>, end: usize) {
    if let Some(start) = plain.take()
        && start < end
    {
        out.push((start..end, Token::Plain));
    }
}

/// The end of the string literal opening at `start`, including its closing
/// quote. An unterminated literal runs to the end of the line, which is the
/// right guess for a string that continues onto the next one.
fn scan_string(line: &str, start: usize, quote: char) -> usize {
    let mut i = start + quote.len_utf8();
    while i < line.len() {
        let ch = line[i..].chars().next().expect("i is a char boundary");
        i += ch.len_utf8();
        if ch == '\\' {
            // Skip whatever is escaped, so `"\""` does not end early.
            if i < line.len() {
                i += line[i..]
                    .chars()
                    .next()
                    .expect("i is a char boundary")
                    .len_utf8();
            }
            continue;
        }
        if ch == quote {
            return i;
        }
    }
    line.len()
}

fn scan_while(line: &str, start: usize, pred: impl Fn(char) -> bool) -> usize {
    let mut i = start;
    while i < line.len() {
        let ch = line[i..].chars().next().expect("i is a char boundary");
        if !pred(ch) {
            break;
        }
        i += ch.len_utf8();
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runs, as (text, token) pairs — what a renderer actually consumes.
    fn runs(line: &str, lang: Language) -> Vec<(&str, Token)> {
        spans(line, lang)
            .into_iter()
            .map(|(range, token)| (&line[range], token))
            .collect()
    }

    /// The invariant every caller relies on: the runs tile the line exactly.
    fn assert_tiles(line: &str, lang: Language) {
        let spans = spans(line, lang);
        let mut at = 0;
        for (range, _) in &spans {
            assert_eq!(range.start, at, "gap or overlap in {line:?}: {spans:?}");
            assert!(range.end > range.start, "empty span in {line:?}");
            at = range.end;
        }
        assert_eq!(at, line.len(), "runs must reach the end of {line:?}");
        let rebuilt: String = spans.iter().map(|(r, _)| &line[r.clone()]).collect();
        assert_eq!(rebuilt, line);
    }

    #[test]
    fn extensions_map_to_languages() {
        assert_eq!(detect("src/app.rs"), Language::Rust);
        assert_eq!(detect("main.py"), Language::Python);
        assert_eq!(detect("a/b/c.tsx"), Language::JavaScript);
        assert_eq!(detect("Cargo.toml"), Language::Toml);
        assert_eq!(detect("README.md"), Language::Markdown);
        assert_eq!(detect("deploy.sh"), Language::Shell);
    }

    #[test]
    fn an_unknown_or_absent_extension_is_plain() {
        assert_eq!(detect("notes.xyz"), Language::Plain);
        assert_eq!(detect("LICENSE"), Language::Plain);
        // A dotfile is not an extension: `.gitignore` is a name, not a `gitignore` file.
        assert_eq!(detect(".gitignore"), Language::Plain);
    }

    #[test]
    fn files_that_carry_their_type_in_the_name_are_recognised() {
        assert_eq!(detect("Makefile"), Language::Shell);
        assert_eq!(detect("build/Dockerfile"), Language::Shell);
    }

    #[test]
    fn extension_matching_ignores_case() {
        assert_eq!(detect("SETUP.PY"), Language::Python);
    }

    #[test]
    fn a_rust_line_splits_into_its_parts() {
        assert_eq!(
            runs("let x = 42; // note", Language::Rust),
            vec![
                ("let", Token::Keyword),
                (" x = ", Token::Plain),
                ("42", Token::Number),
                ("; ", Token::Plain),
                ("// note", Token::Comment),
            ]
        );
    }

    #[test]
    fn strings_are_taken_whole_including_escapes() {
        assert_eq!(
            runs(r#"f("a\"b")"#, Language::Rust),
            vec![
                ("f(", Token::Plain),
                (r#""a\"b""#, Token::Str),
                (")", Token::Plain),
            ],
            "an escaped quote must not end the literal"
        );
    }

    #[test]
    fn a_comment_marker_inside_a_string_is_not_a_comment() {
        assert_eq!(
            runs(r#"let s = "// not a comment";"#, Language::Rust),
            vec![
                ("let", Token::Keyword),
                (" s = ", Token::Plain),
                (r#""// not a comment""#, Token::Str),
                (";", Token::Plain),
            ]
        );
    }

    #[test]
    fn an_unterminated_string_runs_to_the_end_of_the_line() {
        // The likeliest cause is a literal continuing onto the next line, and
        // colouring the tail as code would be the more surprising guess.
        assert_eq!(
            runs(r#"let s = "open"#, Language::Rust),
            vec![
                ("let", Token::Keyword),
                (" s = ", Token::Plain),
                (r#""open"#, Token::Str),
            ]
        );
    }

    #[test]
    fn a_digit_inside_an_identifier_is_not_a_number() {
        assert_eq!(
            runs("let sha256 = 1", Language::Rust),
            vec![
                ("let", Token::Keyword),
                (" sha256 = ", Token::Plain),
                ("1", Token::Number),
            ]
        );
    }

    #[test]
    fn a_plain_language_yields_one_run_for_the_whole_line() {
        assert_eq!(
            runs("let x = 42; // note", Language::Plain),
            vec![("let x = 42; // note", Token::Plain)]
        );
    }

    #[test]
    fn markdown_is_not_highlighted_as_code() {
        // `#` opens a heading, not a comment, and quotes are punctuation.
        assert_eq!(
            runs("# Heading with \"quotes\"", Language::Markdown),
            vec![("# Heading with \"quotes\"", Token::Plain)]
        );
    }

    #[test]
    fn an_empty_line_has_no_runs() {
        assert!(spans("", Language::Rust).is_empty());
    }

    #[test]
    fn runs_always_tile_the_line() {
        let cases = [
            "",
            "   ",
            "let x = 42; // note",
            r#"let s = "héllo wörld"; // ünicode"#,
            "// 🎉 emoji in a comment",
            r#"{"key": "válue", "n": 3.14}"#,
            "#!/bin/bash",
            "no_keywords_here_at_all",
            r#"unterminated "string"#,
        ];
        for line in cases {
            for lang in [
                Language::Rust,
                Language::Python,
                Language::Json,
                Language::Shell,
                Language::Plain,
            ] {
                assert_tiles(line, lang);
            }
        }
    }
}
