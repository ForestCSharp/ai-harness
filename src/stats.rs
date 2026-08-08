//! What a session has actually done, counted from its transcript.
//!
//! Nothing here is stored. The transcript already records every outcome, so the
//! numbers are derived when the page is drawn — the rule `rewind_rows` and
//! `Sessions::rows` follow, and here it buys something extra: a session saved
//! long before this module existed reports honestly, because its transcript
//! already holds the reads.
//!
//! Counted from **results, not proposals**. An `Entry::Action` says what the
//! model asked to do; a write it asked for may have been denied, and a command it
//! proposed may never have run. `Entry::WriteResult` and friends say what
//! happened, which is the only thing worth counting.

use std::collections::BTreeSet;

use crate::app::Entry;

/// What the session did, by kind.
///
/// Writes and edits are one number because they arrive as one: an edit is
/// resolved into a full rewrite before the modal, so both land as a
/// `WriteResult`. Splitting them would mean counting *proposals* for one of
/// them, which would quietly include the ones you refused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Actions {
    pub reads: usize,
    /// Greps and globs together — both are `Entry::SearchResult`, and the
    /// distinction is not one anybody reads a stats page to learn.
    pub searches: usize,
    pub fetches: usize,
    pub shells: usize,
    /// Writes and edits that landed.
    pub writes: usize,
    /// Actions the user refused, which by definition did not happen.
    pub denied: usize,
}

impl Actions {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

pub fn actions(transcript: &[Entry]) -> Actions {
    let mut counts = Actions::default();
    for entry in transcript {
        match entry {
            // A read that failed is a `ReadResult` too — the error goes back to
            // the model as data rather than ending the turn — so success has to
            // be asked about here as well as for a write.
            Entry::ReadResult(outcome) if outcome.succeeded() => counts.reads += 1,
            Entry::SearchResult(_) => counts.searches += 1,
            Entry::FetchResult(_) => counts.fetches += 1,
            Entry::CommandResult(_) => counts.shells += 1,
            // A write that failed — refused by the sandbox, timed out — is not a
            // write. It is visible in the transcript as its own error.
            Entry::WriteResult(outcome) if outcome.succeeded() => counts.writes += 1,
            Entry::Denied(_) => counts.denied += 1,
            _ => {}
        }
    }
    counts
}

/// How much the memory index was actually used this session.
///
/// The number the memory system is judged on. A note that is indexed and never
/// opened is the failure mode of the whole design — its description is paying
/// for a line in the contract on every request and buying nothing — and that
/// only shows up as the difference between what is indexed and what is in
/// [`MemoryUse::read`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryUse {
    /// Successful reads of a note. Two reads of one note are two.
    pub reads: usize,
    /// Which notes, by name. Two reads of one note are one entry.
    pub read: BTreeSet<String>,
    /// Notes written or edited, counted only where the write landed.
    pub writes: usize,
}

pub fn memory_use(transcript: &[Entry]) -> MemoryUse {
    let mut use_ = MemoryUse::default();
    for entry in transcript {
        match entry {
            Entry::ReadResult(outcome) if outcome.succeeded() => {
                if let Some(name) = note_name(&outcome.path) {
                    use_.reads += 1;
                    use_.read.insert(name);
                }
            }
            Entry::WriteResult(outcome)
                if outcome.succeeded() && note_name(&outcome.path).is_some() =>
            {
                use_.writes += 1;
            }
            _ => {}
        }
    }
    use_
}

/// The note name in `path`, if it points into the memory directory.
///
/// Matched on the directory *segment* rather than resolved against the sandbox,
/// so the several ways a model may spell the same path — bare, `./`-prefixed,
/// absolute, backslashed — all land. Deliberately loose: this is a metric, and
/// missing an odd spelling understates a number, where being strict would report
/// zero and look like a feature that does not work.
fn note_name(path: &str) -> Option<String> {
    let needle = format!("{}/{}/", crate::config::HARNESS_DIR, crate::memory::DIR);
    let normal = path.replace('\\', "/");
    let rest = normal.split_once(&needle)?.1;
    // Directly inside the directory: a path with another separator is something
    // below it, which `memory::list` does not index either.
    if rest.contains('/') {
        return None;
    }
    Some(rest.strip_suffix(".md").unwrap_or(rest).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CommandOutput, WriteOutcome};
    use crate::files::ReadOutcome;

    fn read(path: &str) -> Entry {
        Entry::ReadResult(ReadOutcome::whole_file(path, "contents"))
    }

    fn failed_read(path: &str) -> Entry {
        Entry::ReadResult(ReadOutcome::failed(path, "no such file"))
    }

    fn wrote(path: &str, ok: bool) -> Entry {
        Entry::WriteResult(WriteOutcome {
            path: path.to_string(),
            bytes: 10,
            error: (!ok).then(|| "denied by the sandbox".to_string()),
            timed_out: false,
            cancelled: false,
        })
    }

    fn shell() -> Entry {
        Entry::CommandResult(Box::new(CommandOutput {
            command: "ls".into(),
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            cancelled: false,
        }))
    }

    #[test]
    fn actions_are_counted_from_what_happened() {
        let transcript = vec![
            read("src/app.rs"),
            read("src/ui.rs"),
            shell(),
            wrote("src/app.rs", true),
            Entry::Denied("rm -rf /".into()),
        ];
        let counts = actions(&transcript);
        assert_eq!(counts.reads, 2);
        assert_eq!(counts.shells, 1);
        assert_eq!(counts.writes, 1);
        assert_eq!(counts.denied, 1);
    }

    /// The reason to count results rather than the actions that proposed them:
    /// a write you refused is not a write this session made.
    #[test]
    fn a_refused_or_failed_write_is_not_a_write() {
        let transcript = vec![
            Entry::Denied("write to src/app.rs".into()),
            wrote("src/app.rs", false),
        ];
        let counts = actions(&transcript);
        assert_eq!(counts.writes, 0);
        assert_eq!(counts.denied, 1);
    }

    /// A read that failed comes back as a `ReadResult` carrying its error, not
    /// as an error entry — so it needs the same filter a write does.
    #[test]
    fn a_failed_read_is_not_a_read() {
        assert_eq!(actions(&[failed_read("src/nope.rs")]).reads, 0);
        assert_eq!(
            memory_use(&[failed_read(".ai_harness/memory/deleted.md")]).reads,
            0,
            "and a note that is gone was not consulted"
        );
    }

    #[test]
    fn an_empty_transcript_counts_nothing() {
        assert!(actions(&[]).is_empty());
        assert_eq!(memory_use(&[]), MemoryUse::default());
    }

    #[test]
    fn a_memory_read_is_recognised_however_the_path_is_spelled() {
        for path in [
            ".ai_harness/memory/auth-flow.md",
            "./.ai_harness/memory/auth-flow.md",
            "/Users/x/project/.ai_harness/memory/auth-flow.md",
            ".ai_harness\\memory\\auth-flow.md",
        ] {
            let used = memory_use(&[read(path)]);
            assert_eq!(used.reads, 1, "{path}");
            assert_eq!(
                used.read.iter().next().map(String::as_str),
                Some("auth-flow"),
                "{path}"
            );
        }
    }

    #[test]
    fn an_ordinary_read_is_not_a_memory_read() {
        for path in [
            "src/app.rs",
            "memory/notes.md",
            ".ai_harness/sessions/x/session.json",
            // Below the directory rather than in it — not indexed either.
            ".ai_harness/memory/sub/deep.md",
        ] {
            assert_eq!(memory_use(&[read(path)]).reads, 0, "{path}");
        }
    }

    /// Two numbers because they answer different questions: how often memory was
    /// consulted, and how much of it was.
    #[test]
    fn reads_and_notes_read_are_counted_separately() {
        let used = memory_use(&[
            read(".ai_harness/memory/auth-flow.md"),
            read(".ai_harness/memory/auth-flow.md"),
            read(".ai_harness/memory/deploy.md"),
            read("src/app.rs"),
        ]);
        assert_eq!(used.reads, 3);
        assert_eq!(
            used.read.iter().cloned().collect::<Vec<_>>(),
            vec!["auth-flow", "deploy"]
        );
    }

    #[test]
    fn a_note_written_counts_only_when_the_write_landed() {
        let used = memory_use(&[
            wrote(".ai_harness/memory/learned.md", true),
            wrote(".ai_harness/memory/failed.md", false),
            wrote("src/app.rs", true),
        ]);
        assert_eq!(used.writes, 1);
    }
}
