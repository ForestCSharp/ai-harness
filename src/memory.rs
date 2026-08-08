//! Notes about the project that outlive a session.
//!
//! The second of the two tiers of standing knowledge. `AGENTS.md` is always in
//! the contract, whole; memory is an **index** in the contract and bodies on
//! demand. A note contributes one line — its name and a description saying when
//! you would want it — and the body enters the conversation only when the model
//! decides that line is relevant and reads the file with `<ai-harness-read>`.
//!
//! ```text
//! .ai_harness/memory/auth-flow.md
//! ```
//!
//! ```markdown
//! ---
//! description: how sessions are validated — read before touching auth/
//! ---
//!
//! Long-form notes…
//! ```
//!
//! That split is the whole point: fifteen tokens standing per note, and the few
//! hundred to few thousand a real one costs paid only when it is used. A
//! directory of forty notes is affordable; forty notes pasted into the contract
//! is not.
//!
//! The model writes these itself, through the ordinary approval-gated
//! `<ai-harness-write>` — so a memory is something you saw and allowed, not
//! something that appeared. See the README for what that does and does not
//! protect against.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The memory directory inside `.ai_harness/`.
pub const DIR: &str = "memory";

/// One note, as the index sees it. Deliberately not the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The file stem — `auth-flow` for `auth-flow.md`.
    pub name: String,
    /// The frontmatter's `description`, which is the entire index entry.
    pub description: String,
    /// Last write, which is what the budget drops by.
    pub modified: SystemTime,
}

/// What the index is allowed to cost.
///
/// The contract is re-sent on every round-trip of an agentic turn, not once per
/// prompt, so this is paid repeatedly. Both limits, not one: a hundred terse
/// notes and ten verbose ones are the same problem.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub entries: usize,
    pub bytes: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            entries: 128,
            bytes: 8 * 1024,
        }
    }
}

/// Where a project's notes live, given the sandbox root.
///
/// Keyed on the root rather than on `--sessions-dir`: memory is a fact about the
/// project, and a flag that moves sessions elsewhere should not move it.
pub fn dir(root: &Path) -> PathBuf {
    root.join(crate::config::HARNESS_DIR).join(DIR)
}

/// How much of a note is read looking for its description.
///
/// The frontmatter is the first thing in the file, so this is generous. The
/// point is the ceiling, not the number: the body may be kilobytes, and nothing
/// in this module ever wants it.
const HEAD_BYTES: usize = 1024;

/// Every note in `dir` that carries a description, by name.
///
/// A file without one is **left out**, not listed blank. The description is the
/// whole index entry, and one that does not say when you would want the note is
/// a note that never gets read — dead weight in a budget paid on every request.
/// [`skipped`] is what tells you it happened.
pub fn list(dir: &Path) -> Vec<Note> {
    let mut notes: Vec<Note> = markdown_files(dir)
        .into_iter()
        .filter_map(|path| {
            let description = description_of(&path)?;
            Some(Note {
                name: stem(&path)?,
                description,
                modified: modified(&path),
            })
        })
        .collect();
    notes.sort_by(|a, b| a.name.cmp(&b.name));
    notes
}

/// Notes in `dir` that were left out for want of a description, by name.
///
/// Derived by difference rather than returned alongside [`list`]: only `/memory`
/// wants it, and every other caller would have to ignore it.
pub fn skipped(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = markdown_files(dir)
        .into_iter()
        .filter(|path| description_of(path).is_none())
        .filter_map(|path| stem(&path))
        .collect();
    names.sort();
    names
}

/// The notes that fit, and how many did not.
///
/// Least-recently-modified first out of the boat: a note you touched today is
/// more likely to be the one this project is about than one you wrote in March
/// and have not opened since.
pub fn within(notes: &[Note], caps: Caps) -> (Vec<Note>, usize) {
    // Drop by recency, then restore the name order the index is read in.
    let mut by_recency: Vec<&Note> = notes.iter().collect();
    by_recency.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));

    let mut kept: Vec<Note> = Vec::new();
    let mut bytes = 0;
    for note in by_recency {
        if kept.len() == caps.entries {
            break;
        }
        let cost = entry_line(note).len();
        if bytes + cost > caps.bytes {
            break;
        }
        bytes += cost;
        kept.push(note.clone());
    }
    let dropped = notes.len() - kept.len();
    kept.sort_by(|a, b| a.name.cmp(&b.name));
    (kept, dropped)
}

/// The index as it appears in the contract, or `None` when there is nothing to
/// say — an absent section beats a section announcing its own emptiness.
pub fn index(notes: &[Note], caps: Caps) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let (kept, dropped) = within(notes, caps);
    if kept.is_empty() {
        return None;
    }
    let mut text = String::new();
    for note in &kept {
        text.push_str(&entry_line(note));
    }
    if dropped > 0 {
        // Said, so a partial list is visibly partial rather than looking like
        // the whole of what exists.
        text.push_str(&format!(
            "  (and {dropped} more, not listed here for space)\n"
        ));
    }
    Some(text)
}

/// One index line. The unit the budget is measured in, so it is built in one
/// place rather than formatted twice.
fn entry_line(note: &Note) -> String {
    format!("  {}.md — {}\n", note.name, note.description)
}

/// The `.md` files directly in `dir`, unsorted. Not recursive: a memory is one
/// file, and a tree would need the index to carry paths.
fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|e| e == "md"))
        .collect()
}

fn stem(path: &Path) -> Option<String> {
    path.file_stem()?.to_str().map(str::to_string)
}

fn modified(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// The `description:` from a note's frontmatter, on disk.
///
/// Read out of a bounded head rather than by loading the file, the trick
/// [`crate::session::head`] uses for the same reason: what is wanted is at the
/// top and what is not may be kilobytes.
fn description_of(path: &Path) -> Option<String> {
    use std::io::Read;

    let mut buffer = vec![0u8; HEAD_BYTES];
    let mut file = std::fs::File::open(path).ok()?;
    let read = file.read(&mut buffer).ok()?;
    // Lossy rather than strict: a note is prose and may well be cut through a
    // multi-byte character by the bounded read. The frontmatter is above the cut.
    description_in(&String::from_utf8_lossy(&buffer[..read]))
}

/// The `description:` from note text that has not been written yet.
///
/// The same parser the index uses, and that is the point rather than a
/// convenience: the pre-flight check that refuses an unindexable note
/// ([`crate::app::App::targets_memory_note`]) has to agree with
/// [`list`] exactly. A validator that disagreed would pass a note that then
/// silently failed to index — which is the bug the check exists to close,
/// restored in a subtler form.
///
/// Hand-parsed because it is two lines of structure; a YAML dependency to read
/// one key would be the tail wagging the dog.
pub fn description_in(text: &str) -> Option<String> {
    let mut lines = text.lines();
    // The fence has to be the very first line: a `---` further down is a
    // horizontal rule in the body, not the start of frontmatter.
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("description:") {
            let value = value.trim().trim_matches('"');
            // One line, and collapsed: the index is a list, and a description
            // that wrapped would break the shape it is read in.
            let value: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-memory-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{name}.md"));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn note(dir: &Path, name: &str, description: &str) -> PathBuf {
        write(
            dir,
            name,
            &format!("---\ndescription: {description}\n---\n\nbody"),
        )
    }

    #[test]
    fn a_description_is_read_out_of_the_head_and_the_body_is_not() {
        let dir = temp_dir("head");
        // A body far past the bounded read. If the parser needed the whole file
        // this would still pass — what it proves is that it does not *fail*, and
        // the cost of a huge note is one bounded read either way.
        let body = "x".repeat(HEAD_BYTES * 8);
        write(
            &dir,
            "big",
            &format!("---\ndescription: the big one\n---\n\n{body}"),
        );

        let notes = list(&dir);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].name, "big");
        assert_eq!(notes[0].description, "the big one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A description that does not say when you would want the note is dead
    /// weight in a budget paid on every request, so it is left out entirely.
    #[test]
    fn a_note_without_a_description_is_skipped_and_reported() {
        let dir = temp_dir("nodesc");
        note(&dir, "good", "when to read me");
        write(&dir, "bare", "just some notes, no frontmatter");
        write(&dir, "empty", "---\ndescription:\n---\nbody");
        // `---` below the first line is a horizontal rule, not frontmatter.
        write(&dir, "rule", "Some prose\n\n---\ndescription: nope\n---\n");

        assert_eq!(
            list(&dir)
                .iter()
                .map(|n| n.name.clone())
                .collect::<Vec<_>>(),
            vec!["good"]
        );
        assert_eq!(skipped(&dir), vec!["bare", "empty", "rule"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_markdown_files_are_notes() {
        let dir = temp_dir("md-only");
        note(&dir, "real", "a real note");
        std::fs::write(dir.join("notes.txt"), "---\ndescription: no\n---\n").unwrap();
        std::fs::create_dir_all(dir.join("sub.md")).unwrap();

        assert_eq!(list(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_index_lists_name_and_description() {
        let dir = temp_dir("index");
        note(&dir, "auth-flow", "how sessions are validated");
        note(&dir, "deploy", "the staging deploy sequence");

        let text = index(&list(&dir), Caps::default()).expect("two notes");
        assert!(text.contains("auth-flow.md — how sessions are validated"));
        assert!(text.contains("deploy.md — the staging deploy sequence"));
        assert!(!text.contains("more, not listed"), "nothing was dropped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_or_missing_directory_has_no_section() {
        let dir = temp_dir("empty");
        assert_eq!(index(&list(&dir), Caps::default()), None);
        assert_eq!(index(&list(&dir.join("nope")), Caps::default()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A note touched today is likelier to be what this project is about than
    /// one written in March and not opened since.
    #[test]
    fn over_budget_drops_the_least_recently_touched_and_says_how_many() {
        let notes: Vec<Note> = (0..10)
            .map(|i| Note {
                name: format!("n{i}"),
                description: "a note".into(),
                // n0 oldest, n9 newest.
                modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i),
            })
            .collect();

        let (kept, dropped) = within(
            &notes,
            Caps {
                entries: 3,
                bytes: usize::MAX,
            },
        );
        assert_eq!(dropped, 7);
        assert_eq!(
            kept.iter().map(|n| n.name.clone()).collect::<Vec<_>>(),
            vec!["n7", "n8", "n9"],
            "the newest three, back in name order"
        );

        let text = index(
            &notes,
            Caps {
                entries: 3,
                bytes: usize::MAX,
            },
        )
        .expect("some fit");
        assert!(text.contains("(and 7 more"), "{text}");
        let _ = ();
    }

    #[test]
    fn the_byte_cap_bites_as_well_as_the_entry_cap() {
        let notes: Vec<Note> = (0..10)
            .map(|i| Note {
                name: format!("n{i}"),
                description: "d".repeat(100),
                modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i),
            })
            .collect();

        let (kept, dropped) = within(
            &notes,
            Caps {
                entries: usize::MAX,
                bytes: 250,
            },
        );
        assert_eq!(kept.len(), 2, "two ~110-byte lines fit in 250");
        assert_eq!(dropped, 8);
    }

    /// Ties on `modified` are broken by name, so the index cannot depend on
    /// directory iteration order — the fixtures write all their files inside one
    /// filesystem timestamp granule.
    #[test]
    fn a_tie_on_recency_falls_back_to_the_name() {
        let same = SystemTime::UNIX_EPOCH;
        let notes: Vec<Note> = ["c", "a", "b"]
            .iter()
            .map(|name| Note {
                name: (*name).to_string(),
                description: "a note".into(),
                modified: same,
            })
            .collect();

        let (kept, _) = within(
            &notes,
            Caps {
                entries: 2,
                bytes: usize::MAX,
            },
        );
        assert_eq!(
            kept.iter().map(|n| n.name.clone()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn a_wrapped_description_is_collapsed_to_one_line() {
        let dir = temp_dir("collapse");
        write(
            &dir,
            "wrapped",
            "---\ndescription:   lots   of    space   \n---\nbody",
        );
        assert_eq!(list(&dir)[0].description, "lots of space");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The check that refuses an unindexable note and the index that would skip
    /// it are the same parser. Run the same cases through both entry points:
    /// if they ever diverge, a note passes validation and vanishes anyway.
    #[test]
    fn the_text_and_file_parsers_agree() {
        let dir = temp_dir("agree");
        for (name, text, expected) in [
            (
                "good",
                "---\ndescription: when to read me\n---\nbody",
                Some("when to read me"),
            ),
            ("bare", "just notes, no frontmatter", None),
            ("empty", "---\ndescription:\n---\nbody", None),
            ("rule", "prose\n\n---\ndescription: nope\n---\n", None),
            (
                "quoted",
                "---\ndescription: \"quoted one\"\n---\n",
                Some("quoted one"),
            ),
            ("closed", "---\n---\ndescription: below the fence\n", None),
        ] {
            let from_text = description_in(text);
            write(&dir, name, text);
            let from_file = description_of(&dir.join(format!("{name}.md")));
            assert_eq!(
                from_text.as_deref(),
                expected,
                "description_in disagreed on {name}"
            );
            assert_eq!(from_file, from_text, "the two parsers disagreed on {name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_memory_directory_sits_under_the_harness_directory() {
        let root = Path::new("/projects/thing");
        assert_eq!(
            dir(root),
            root.join(crate::config::HARNESS_DIR).join("memory")
        );
    }
}
