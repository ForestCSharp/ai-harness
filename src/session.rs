//! Saving and loading conversations to disk.
//!
//! A session captures both the model conversation (`history`, needed to
//! continue) and the rendered transcript (needed to restore the screen), as
//! pretty-printed JSON.
//!
//! Each session is a **directory**, not a file:
//!
//! ```text
//! .ai_harness/sessions/<name>/session.json
//! ```
//!
//! The conversation is only the first thing a session owns — a plan file and
//! whatever else turns out to be per-session live beside it. Making the session
//! a directory up front means those follow a [`rename`] for free, rather than
//! each needing to be moved by name.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::app::Entry;
use crate::ledger::Ledger;
use crate::openrouter::Message;

/// The on-disk format version. Bumped only on an incompatible change so an old
/// loader refuses a newer file rather than mis-parsing it.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    /// Unix seconds when saved.
    pub saved_at: u64,
    /// The model in use when the session was saved.
    pub model: String,
    pub history: Vec<Message>,
    pub transcript: Vec<Entry>,
    pub prompt_history: Vec<String>,
    /// Cumulative token spend. Added after `VERSION` 1 without a bump: it
    /// defaults when absent, and older builds ignore the unknown field, so
    /// files stay readable in both directions.
    #[serde(default)]
    pub ledger: Ledger,
}

impl Session {
    pub fn new(
        model: String,
        history: Vec<Message>,
        transcript: Vec<Entry>,
        prompt_history: Vec<String>,
        ledger: Ledger,
    ) -> Self {
        Self {
            version: VERSION,
            saved_at: now_secs(),
            model,
            history,
            transcript,
            prompt_history,
            ledger,
        }
    }
}

/// The conversation file inside a session's directory.
pub const FILE: &str = "session.json";

/// A few lines of the session's tail, for the `/load` picker.
///
/// Kept as its own small file rather than read out of [`FILE`], which runs to
/// hundreds of kilobytes: the picker opens every session at once, and parsing
/// them all to show three lines each would cost more the longer you use the
/// harness. Derived data — a save regenerates it, and losing it costs a nicer
/// picker and nothing else.
pub const PREVIEW_FILE: &str = "preview.txt";

/// Lines kept in a preview, which is also what the picker shows.
pub const PREVIEW_LINES: usize = 3;

/// Longest a preview line may be. Enough to recognise a session by, short
/// enough that one cannot make the file large or the picker row tall.
const PREVIEW_WIDTH: usize = 120;

/// The directory holding one session's files.
///
/// Public because a session is a directory now: anything else that belongs to
/// one — a plan, notes — is written here by its own module rather than by this
/// one, and needs to be able to ask where "here" is.
pub fn dir(root: &Path, name: &str) -> Result<PathBuf> {
    Ok(root.join(sanitize(name)?))
}

/// The conversation file for `name`.
fn file(root: &Path, name: &str) -> Result<PathBuf> {
    Ok(dir(root, name)?.join(FILE))
}

/// Write `session` to `<dir>/<name>/session.json`, creating the folder if needed.
pub fn save(dir_: &Path, name: &str, session: &Session) -> Result<PathBuf> {
    let folder = dir(dir_, name)?;
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("creating session directory {}", folder.display()))?;
    let path = folder.join(FILE);
    let json = serde_json::to_string_pretty(session).context("serialising session")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;

    // Best effort, deliberately: the preview is a convenience, and failing the
    // save of a conversation because a cosmetic file could not be written would
    // be the wrong trade. The next save regenerates it.
    let _ = std::fs::write(folder.join(PREVIEW_FILE), preview_lines(session).join("\n"));
    Ok(path)
}

/// The lines shown under a session's name in the picker.
///
/// Empty when the session has none, and when it was saved before previews
/// existed — those are not backfilled, since reading every `session.json` to do
/// it is the cost this file exists to avoid.
pub fn preview(dir_: &Path, name: &str) -> Vec<String> {
    let Ok(folder) = dir(dir_, name) else {
        return Vec::new();
    };
    std::fs::read_to_string(folder.join(PREVIEW_FILE))
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Summarise a session by its last few lines of prose.
///
/// Only what the user typed and what the model answered: a trailing shell result
/// or a debug frame says nothing about what the session was *about*, which is
/// the one question the picker has to answer.
fn preview_lines(session: &Session) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for entry in session.transcript.iter().rev() {
        let text = match entry {
            Entry::User(text) => format!("you: {text}"),
            Entry::Action {
                action: crate::protocol::Action::Response(text),
                ..
            } => text.clone(),
            _ => continue,
        };
        // First line only: a long answer must not make the row tall.
        let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        if line.trim().is_empty() {
            continue;
        }
        lines.push(truncate(line.trim(), PREVIEW_WIDTH));
        if lines.len() == PREVIEW_LINES {
            break;
        }
    }
    // Walked backwards to find the tail; shown in the order it happened.
    lines.reverse();
    lines
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    text.chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "…"
}

/// Read `<dir>/<name>/session.json`.
pub fn load(dir: &Path, name: &str) -> Result<Session> {
    let path = file(dir, name)?;
    // Named by session rather than by path: `name` is what the user typed, and
    // it is what they would retype to fix a mistake.
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no session {name:?} at {}", path.display()))?;
    let session: Session = serde_json::from_str(&text)
        .with_context(|| format!("reading session {}", path.display()))?;
    if session.version != VERSION {
        bail!(
            "session {name:?} has format version {} but this build reads version {VERSION}",
            session.version
        );
    }
    Ok(session)
}

/// Names of saved sessions, sorted.
///
/// Keys on the session file rather than on "is a directory", so a folder that
/// holds something else cannot appear in the `/load` picker as a session that
/// then fails to load.
pub fn list(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            path.join(FILE)
                .is_file()
                .then(|| path.file_name()?.to_str().map(str::to_string))
                .flatten()
        })
        .collect();
    names.sort();
    names
}

/// A default, unique-ish name for `/save` with no argument.
pub fn default_name() -> String {
    format!("session-{}", now_secs())
}

/// Whether a session has been saved on disk.
pub fn exists(dir: &Path, name: &str) -> bool {
    file(dir, name).is_ok_and(|path| path.is_file())
}

/// Rename a session's directory `<old>/` → `<new>/`.
///
/// The whole directory moves, so a plan file or anything else added beside the
/// conversation follows the rename without this needing to know it exists —
/// which is the reason a session is a directory rather than a file.
///
/// Refuses to clobber an existing `<new>`. If nothing is saved under `old` yet,
/// this succeeds without moving anything — the name simply becomes current and
/// the next save writes there.
pub fn rename(dir_: &Path, old: &str, new: &str) -> Result<PathBuf> {
    let old_path = dir(dir_, old)?;
    let new_path = dir(dir_, new)?;
    if old_path != new_path && new_path.exists() {
        bail!("a session named {new:?} already exists");
    }
    if old_path.exists() {
        std::fs::rename(&old_path, &new_path).with_context(|| {
            format!("renaming {} to {}", old_path.display(), new_path.display())
        })?;
    }
    Ok(new_path)
}

/// Reject a name that could escape the sessions directory. A session name is
/// user-typed and becomes a path, so this is a small security boundary: refuse
/// separators and parent traversal outright rather than trying to normalise.
fn sanitize(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("session name must not be empty");
    }
    if name == "." || name == ".." || name.contains(['/', '\\']) || name.contains("..") {
        bail!("invalid session name {name:?}: names cannot contain path separators or '..'");
    }
    Ok(name)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Direction;
    use crate::exec::CommandOutput;
    use crate::openrouter::Usage;
    use crate::protocol::Action;

    /// A directory of this test's own.
    ///
    /// Counted, not timestamped: tests run in parallel and finish inside the
    /// same second, so two sharing a tag would share a directory — and each ends
    /// by deleting it, pulling the ground from under the other. That failed
    /// perhaps one run in three before the counter.
    fn temp_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-session-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample() -> Session {
        Session::new(
            "test/model".into(),
            vec![Message::system("contract"), Message::user("hi")],
            vec![
                Entry::User("hi".into()),
                Entry::Action {
                    action: Action::Response("hello".into()),
                    usage: Some(Usage {
                        prompt_tokens: 3,
                        completion_tokens: 1,
                    }),
                    diff: None,
                },
            ],
            vec!["hi".into()],
            Ledger::default(),
        )
    }

    #[test]
    fn a_session_is_a_folder_holding_its_conversation() {
        let dir = temp_dir("layout");
        let path = save(&dir, "demo", &sample()).unwrap();

        assert_eq!(path, dir.join("demo").join(FILE));
        assert!(dir.join("demo").is_dir(), "the session is a directory");
        assert!(
            !dir.join("demo.json").exists(),
            "the flat file layout is gone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_saved_before_a_field_was_dropped_still_loads() {
        // Command results once carried the lines a user typed to a running
        // command. That went away with the mode that produced them, but sessions
        // holding the field are on disk — an unknown field must be ignored, not
        // refused, or `/load` would fail on a user's own history.
        let dir = temp_dir("legacy-field");
        let mut session = sample();
        session
            .transcript
            .push(Entry::CommandResult(Box::new(CommandOutput {
                command: "read name".into(),
                exit_code: Some(0),
                stdout: "hi Forest".into(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                cancelled: false,
            })));
        save(&dir, "old", &session).unwrap();

        let path = file(&dir, "old").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let with_field = text.replace(
            "\"cancelled\": false",
            "\"cancelled\": false,\n        \"input\": [\"Forest\"]",
        );
        assert_ne!(text, with_field, "the fixture must contain a shell result");
        std::fs::write(&path, with_field).unwrap();

        let loaded = load(&dir, "old").expect("an unknown field must not fail the load");
        assert_eq!(loaded.transcript.len(), session.transcript.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_ignores_folders_that_are_not_sessions() {
        // Keying on the session file rather than on "is a directory" keeps the
        // `/load` picker from offering something that then fails to load.
        let dir = temp_dir("listing");
        save(&dir, "real", &sample()).unwrap();
        std::fs::create_dir_all(dir.join("not-a-session")).unwrap();
        std::fs::write(dir.join("stray.json"), "{}").unwrap();

        assert_eq!(list(&dir), vec!["real".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_exists_only_once_its_conversation_is_written() {
        let dir = temp_dir("exists");
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        assert!(!exists(&dir, "empty"), "a bare folder is not a session");

        save(&dir, "empty", &sample()).unwrap();
        assert!(exists(&dir, "empty"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session whose transcript is the given alternating prompts and answers.
    fn conversation(turns: &[&str]) -> Session {
        let transcript = turns
            .iter()
            .enumerate()
            .map(|(i, text)| {
                if i % 2 == 0 {
                    Entry::User((*text).into())
                } else {
                    Entry::Action {
                        action: Action::Response((*text).into()),
                        usage: None,
                        diff: None,
                    }
                }
            })
            .collect();
        Session::new("m".into(), vec![], transcript, vec![], Ledger::default())
    }

    #[test]
    fn saving_writes_a_preview_beside_the_conversation() {
        let dir = temp_dir("preview-write");
        save(
            &dir,
            "demo",
            &conversation(&["add a cache", "I added an LRU."]),
        )
        .unwrap();

        assert!(dir.join("demo").join(PREVIEW_FILE).is_file());
        assert_eq!(
            preview(&dir, "demo"),
            vec![
                "you: add a cache".to_string(),
                "I added an LRU.".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preview_is_the_tail_of_the_prose_in_order() {
        // Bounded to the last few, and shown as it happened rather than
        // backwards, which is how it was gathered.
        let dir = temp_dir("preview-tail");
        save(
            &dir,
            "demo",
            &conversation(&["first", "second", "third", "fourth", "fifth"]),
        )
        .unwrap();

        let lines = preview(&dir, "demo");
        assert_eq!(lines.len(), PREVIEW_LINES);
        // Alternating, so the odd ones are the model's and carry no prefix.
        assert_eq!(lines, vec!["you: third", "fourth", "you: fifth"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preview_ignores_everything_that_is_not_prose() {
        // A trailing command result says nothing about what a session was about,
        // which is the only question the picker has to answer.
        let dir = temp_dir("preview-prose");
        let mut session = conversation(&["what changed?", "Two files."]);
        session.transcript.push(Entry::Notice("saved".into()));
        session
            .transcript
            .push(Entry::CommandResult(Box::new(crate::exec::CommandOutput {
                command: "ls".into(),
                exit_code: Some(0),
                stdout: "a.txt".into(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                cancelled: false,
            })));
        save(&dir, "demo", &session).unwrap();

        assert_eq!(
            preview(&dir, "demo"),
            vec!["you: what changed?", "Two files."]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preview_line_is_one_line_and_bounded() {
        let dir = temp_dir("preview-bounds");
        let long = "x".repeat(500);
        save(
            &dir,
            "demo",
            &conversation(&[&format!("{long}\nand a second line")]),
        )
        .unwrap();

        let lines = preview(&dir, "demo");
        assert_eq!(lines.len(), 1, "only the first line of an entry");
        assert!(
            lines[0].chars().count() <= PREVIEW_WIDTH,
            "a long entry must not make a long file: {} chars",
            lines[0].chars().count()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_with_no_prose_has_an_empty_preview() {
        let dir = temp_dir("preview-empty");
        save(&dir, "demo", &sample_without_prose()).unwrap();
        assert!(preview(&dir, "demo").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_saved_before_previews_reads_as_empty() {
        // Not backfilled: parsing every session.json to do it is the cost this
        // file exists to avoid. The next save writes one.
        let dir = temp_dir("preview-absent");
        save(&dir, "demo", &conversation(&["hi"])).unwrap();
        std::fs::remove_file(dir.join("demo").join(PREVIEW_FILE)).unwrap();

        assert!(preview(&dir, "demo").is_empty());
        assert!(load(&dir, "demo").is_ok(), "the session still loads");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preview_that_cannot_be_written_does_not_fail_the_save() {
        // The conversation is the thing that matters; the preview is a
        // convenience the next save regenerates.
        let dir = temp_dir("preview-blocked");
        let folder = dir.join("demo");
        std::fs::create_dir_all(&folder).unwrap();
        // A directory where the file should go: writing it cannot succeed.
        std::fs::create_dir_all(folder.join(PREVIEW_FILE)).unwrap();

        assert!(save(&dir, "demo", &conversation(&["hi"])).is_ok());
        assert!(load(&dir, "demo").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_without_prose() -> Session {
        Session::new(
            "m".into(),
            vec![],
            vec![Entry::Notice("nothing said".into())],
            vec![],
            Ledger::default(),
        )
    }

    #[test]
    fn renaming_carries_the_rest_of_the_session_with_it() {
        // The whole reason a session is a directory: files added beside the
        // conversation follow a rename without `rename` knowing they exist.
        let dir = temp_dir("rename-carries");
        save(&dir, "before", &conversation(&["a question", "an answer"])).unwrap();
        std::fs::write(dir.join("before").join("plan.md"), "# the plan").unwrap();

        rename(&dir, "before", "after").unwrap();

        assert!(!dir.join("before").exists(), "the old folder is gone");
        assert_eq!(
            std::fs::read_to_string(dir.join("after").join("plan.md")).unwrap(),
            "# the plan",
            "a sibling file must travel with the session"
        );
        // The preview is the first real second file this promises anything
        // about, rather than one written by the test to prove the point.
        assert_eq!(
            preview(&dir, "after"),
            vec!["you: a question".to_string(), "an answer".to_string()],
            "the preview must travel too, without `rename` knowing it exists"
        );
        assert!(load(&dir, "after").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_cannot_escape_the_sessions_directory() {
        // A name is now a directory component rather than a filename stem, so
        // this boundary matters at least as much as it did.
        let dir = temp_dir("escape");
        for bad in ["../escape", "a/b", "..", "."] {
            assert!(
                save(&dir, bad, &sample()).is_err(),
                "{bad} should be refused"
            );
            assert!(load(&dir, bad).is_err(), "{bad} should be refused");
            assert!(!exists(&dir, bad), "{bad} should be refused");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp_dir("roundtrip");
        let original = sample();
        let path = save(&dir, "demo", &original).unwrap();
        assert!(path.exists());

        let loaded = load(&dir, "demo").unwrap();
        assert_eq!(loaded.model, "test/model");
        assert_eq!(loaded.history, original.history);
        assert_eq!(loaded.prompt_history, original.prompt_history);
        assert_eq!(loaded.transcript.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_entry_variant_round_trips() {
        let entries = vec![
            Entry::User("q".into()),
            Entry::Action {
                action: Action::Shell("ls".into()),
                usage: None,
                diff: None,
            },
            Entry::Malformed {
                reason: "bad".into(),
                raw: "oops".into(),
            },
            Entry::CommandResult(Box::new(CommandOutput {
                command: "ls".into(),
                exit_code: Some(0),
                stdout: "a".into(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                cancelled: false,
            })),
            Entry::FetchResult(Box::new(crate::fetch::FetchOutcome {
                url: "https://example.com".into(),
                final_url: None,
                status: Some(200),
                content_type: Some("text/html".into()),
                text: "page".into(),
                bytes: 4,
                truncated: false,
                error: None,
            })),
            Entry::Denied("rm -rf /".into()),
            Entry::Frame {
                direction: Direction::Sent,
                body: "<ai-harness-query>q</ai-harness-query>".into(),
            },
            Entry::Error("boom".into()),
            Entry::Notice("hi".into()),
        ];
        let session = Session::new(
            "m".into(),
            vec![],
            entries.clone(),
            vec![],
            Ledger::default(),
        );

        let dir = temp_dir("variants");
        save(&dir, "v", &session).unwrap();
        let loaded = load(&dir, "v").unwrap();
        assert_eq!(loaded.transcript.len(), entries.len());
        // Spot-check a structured variant survived intact.
        match &loaded.transcript[3] {
            Entry::CommandResult(o) => assert_eq!(o.exit_code, Some(0)),
            other => panic!("variant 3 changed shape: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ledger_round_trips() {
        let dir = temp_dir("ledger");
        let mut session = sample();
        session.ledger.record(&Usage {
            prompt_tokens: 120,
            completion_tokens: 40,
        });
        save(&dir, "led", &session).unwrap();

        let loaded = load(&dir, "led").unwrap();
        assert_eq!(loaded.ledger, session.ledger);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ledger was added without a version bump, so files written before it
    /// existed must still load — with the totals simply starting at zero.
    #[test]
    fn a_session_saved_without_a_ledger_still_loads() {
        let dir = temp_dir("no-ledger");
        std::fs::create_dir_all(&dir).unwrap();
        let json = format!(
            r#"{{"version":{VERSION},"saved_at":0,"model":"m","history":[],
                 "transcript":[],"prompt_history":[]}}"#
        );
        std::fs::create_dir_all(dir.join("old")).unwrap();
        std::fs::write(dir.join("old").join(FILE), json).unwrap();

        let loaded = load(&dir, "old").expect("an older session must still load");
        assert!(loaded.ledger.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_returns_sorted_names() {
        let dir = temp_dir("list");
        save(&dir, "beta", &sample()).unwrap();
        save(&dir, "alpha", &sample()).unwrap();
        assert_eq!(list(&dir), vec!["alpha", "beta"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_of_a_missing_dir_is_empty() {
        assert!(list(&temp_dir("missing")).is_empty());
    }

    #[test]
    fn loading_a_missing_session_errors() {
        let dir = temp_dir("absent");
        assert!(load(&dir, "nope").is_err());
    }

    #[test]
    fn an_unknown_version_is_rejected() {
        let dir = temp_dir("version");
        let mut s = sample();
        s.version = 999;
        save(&dir, "future", &s).unwrap();
        let err = load(&dir, "future").unwrap_err().to_string();
        assert!(err.contains("version"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_that_escape_the_directory_are_rejected() {
        let dir = temp_dir("escape");
        for bad in ["../evil", "a/b", "..", "sub/../x", ""] {
            assert!(
                save(&dir, bad, &sample()).is_err(),
                "name {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_plain_name_is_accepted() {
        assert_eq!(sanitize("my-session_2").unwrap(), "my-session_2");
        assert_eq!(sanitize("  spaced  ").unwrap(), "spaced");
    }

    #[test]
    fn exists_reflects_the_filesystem() {
        let dir = temp_dir("exists");
        assert!(!exists(&dir, "nope"));
        save(&dir, "here", &sample()).unwrap();
        assert!(exists(&dir, "here"));
        assert!(!exists(&dir, "../escape"), "a bad name never exists");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_moves_the_file() {
        let dir = temp_dir("rename");
        save(&dir, "old", &sample()).unwrap();
        rename(&dir, "old", "new").unwrap();
        assert!(!exists(&dir, "old"));
        assert!(exists(&dir, "new"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_refuses_to_clobber() {
        let dir = temp_dir("rename-clobber");
        save(&dir, "a", &sample()).unwrap();
        save(&dir, "b", &sample()).unwrap();
        assert!(rename(&dir, "a", "b").is_err(), "must not overwrite b");
        assert!(exists(&dir, "a"), "a is left intact on refusal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_with_nothing_saved_yet_succeeds() {
        let dir = temp_dir("rename-empty");
        // No file for "fresh" yet; renaming just adopts the new name.
        assert!(rename(&dir, "fresh", "named").is_ok());
        assert!(!exists(&dir, "named"), "nothing was on disk to move");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
