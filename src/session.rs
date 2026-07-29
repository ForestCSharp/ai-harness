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
    Ok(path)
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

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ai-harness-session-{name}-{}", now_secs()));
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

    #[test]
    fn renaming_carries_the_rest_of_the_session_with_it() {
        // The whole reason a session is a directory: files added beside the
        // conversation follow a rename without `rename` knowing they exist.
        let dir = temp_dir("rename-carries");
        save(&dir, "before", &sample()).unwrap();
        std::fs::write(dir.join("before").join("plan.md"), "# the plan").unwrap();

        rename(&dir, "before", "after").unwrap();

        assert!(!dir.join("before").exists(), "the old folder is gone");
        assert_eq!(
            std::fs::read_to_string(dir.join("after").join("plan.md")).unwrap(),
            "# the plan",
            "a sibling file must travel with the session"
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
                input: Vec::new(),
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
