//! Per-turn snapshots of the files a turn is about to change, and the restore
//! that puts them back.
//!
//! The sandbox root is what commands are confined *to*, not protected from: an
//! auto-approved `rm -rf .` is entirely inside the boundary. A checkpoint is the
//! answer — cheaper than git integration, and it works on a dirty tree, which is
//! the state a working directory is actually in.
//!
//! Two ways of filling one, because two different things are knowable:
//!
//! - A **write or edit** names its file, so exactly that file is copied. Exact
//!   and nearly free, and it is the common case.
//! - A **shell command** could touch anything, so the workspace is walked and
//!   copied within [`Caps`]. This is the case the feature exists for, and the
//!   only one where guessing is not an option.
//!
//! One checkpoint per turn, opened lazily on the first mutating action, so a
//! turn that only reads leaves nothing behind.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::sandbox::Sandbox;

/// The directory holding a session's checkpoints, inside its session folder.
pub const DIR: &str = "checkpoints";

/// What each checkpoint records about itself, beside the copied files.
const MANIFEST: &str = "manifest.json";

/// Where the copied files live inside a checkpoint, so they cannot collide with
/// [`MANIFEST`] — a workspace with its own `manifest.json` at the root would
/// otherwise overwrite ours.
const FILES: &str = "files";

/// Bounds on a workspace snapshot.
///
/// A walk of an unknown tree needs a stop, for the reason `crate::search` gives:
/// the tree can be arbitrarily large, and the harness must stay responsive. Hit
/// any of these and the snapshot is partial — which is reported, never hidden,
/// since a partial checkpoint restores partially.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub files: usize,
    pub bytes: u64,
    pub time: Duration,
}

impl Default for Caps {
    /// Generous enough for a source tree with `target/` and `.git` skipped, and
    /// well short of anything that would stall a keystroke.
    fn default() -> Self {
        Self {
            files: 5_000,
            bytes: 128 * 1024 * 1024,
            time: Duration::from_millis(750),
        }
    }
}

/// One file as it stood before the turn touched it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// Whether the file was there when the checkpoint was taken.
    ///
    /// The flag that makes undo correct rather than merely helpful: a turn that
    /// *created* a file is undone by deleting it, and without this every new
    /// file would survive a restore that claimed to have undone the turn.
    pub existed: bool,
}

/// What a checkpoint knows about itself.
///
/// Deliberately holds no index into `history`. It used to, and that was a bug:
/// `crate::compact` rebuilds the conversation from scratch, so a stored index
/// means nothing afterwards and `Vec::truncate` past the end fails silently —
/// the same hazard `retry_anchor` is documented against. The turn boundary is
/// found by scanning the live history instead; see `App::rewind_rows`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// The turn this checkpoint belongs to, matching its folder.
    ///
    /// The session's turn ordinal — the *n*th thing typed at the prompt — not a
    /// count of checkpoints. Turns that changed nothing leave gaps, which is
    /// what lets a row in the `/rewind` list find its checkpoint by number.
    pub turn: usize,
    pub saved_at: u64,
    /// The prompt that opened the turn, to name it in the picker and the modal.
    #[serde(default)]
    pub prompt: String,
    /// Whether a cap stopped the workspace walk, and which.
    #[serde(default)]
    pub partial: Option<String>,
    /// Workspace-relative path → what it was. Ordered so a manifest diffs
    /// cleanly and the modal lists files predictably.
    pub files: BTreeMap<String, Entry>,
}

/// A checkpoint open for the current turn.
#[derive(Debug)]
pub struct Checkpoint {
    folder: PathBuf,
    manifest: Manifest,
}

impl Checkpoint {
    /// This checkpoint's number. Used by the tests that pin the numbering; the
    /// app reads it back off the manifest instead.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn turn(&self) -> usize {
        self.manifest.turn
    }

    /// Whether a cap cut the snapshot short, and which one.
    pub fn partial(&self) -> Option<&str> {
        self.manifest.partial.as_deref()
    }

    /// How many files are held.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.manifest.files.len()
    }

    /// Copy one file in, as it stands right now.
    ///
    /// Idempotent per path, and deliberately so: the *first* capture in a turn is
    /// the one that matters, because that is the state the turn began from. A
    /// second write to the same file within a turn must not overwrite the copy
    /// with the intermediate state.
    pub fn capture_file(&mut self, sandbox: &Sandbox, path: &Path) -> Result<()> {
        let Some(key) = relative(sandbox, path) else {
            return Ok(());
        };
        if self.manifest.files.contains_key(&key) {
            return Ok(());
        }
        let existed = path.is_file();
        if existed {
            self.copy_in(&key, path)?;
        }
        self.manifest.files.insert(key, Entry { existed });
        self.write_manifest()
    }

    /// Copy the whole workspace in, within `caps`.
    ///
    /// For a shell command, whose reach is unknowable before it runs. Files
    /// already captured this turn keep their earlier copy, by `capture_file`'s
    /// rule.
    pub fn capture_workspace(&mut self, sandbox: &Sandbox, caps: Caps) -> Result<()> {
        let mut walk = Walk {
            sandbox,
            caps,
            // The session folder is normally under `.ai_harness`, which the skip
            // list already covers — but `--sessions-dir` can put it anywhere,
            // including inside the workspace. Excluded by path rather than by
            // name so a snapshot can never copy the checkpoints into themselves,
            // wherever they have been told to live.
            exclude: self.folder.parent().and_then(|p| p.parent()),
            deadline: Instant::now() + caps.time,
            files: 0,
            bytes: 0,
            capped: None,
            found: Vec::new(),
        };
        walk.descend(sandbox.root(), 0);
        let capped = walk.capped;
        let found = walk.found;

        for path in found {
            let Some(key) = relative(sandbox, &path) else {
                continue;
            };
            if self.manifest.files.contains_key(&key) {
                continue;
            }
            self.copy_in(&key, &path)?;
            self.manifest.files.insert(key, Entry { existed: true });
        }
        // Recorded rather than returned alone, so a restore run in a later
        // session still knows the snapshot it is working from was incomplete.
        self.manifest.partial = capped.map(|c| c.to_string());
        self.write_manifest()
    }

    fn copy_in(&self, key: &str, from: &Path) -> Result<()> {
        let to = self.folder.join(FILES).join(key);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::copy(from, &to).with_context(|| format!("copying {}", from.display()))?;
        Ok(())
    }

    /// Rewrite the manifest after every change, rather than on drop.
    ///
    /// A checkpoint exists for the case where something goes badly wrong, and a
    /// manifest that only lands if the process exits cleanly would be missing
    /// exactly when it is needed.
    fn write_manifest(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.manifest).context("serialising manifest")?;
        std::fs::write(self.folder.join(MANIFEST), json)
            .with_context(|| format!("writing {}", self.folder.display()))?;
        Ok(())
    }
}

/// Open a checkpoint for turn `turn`, whose prompt was `prompt`.
///
/// `turn` is the session's turn ordinal, so the folder name and the row in the
/// `/rewind` list are the same number. It is floored by what is already on disk:
/// a session file that went missing would otherwise restart the count and let a
/// new checkpoint overwrite one still holding the only copy of a file.
pub fn open(session: &Path, turn: usize, prompt: &str) -> Result<Checkpoint> {
    let turn = turn.max(next_index(session));
    let folder = session.join(DIR).join(format!("{turn:03}"));
    std::fs::create_dir_all(folder.join(FILES))
        .with_context(|| format!("creating {}", folder.display()))?;

    let checkpoint = Checkpoint {
        folder,
        manifest: Manifest {
            turn,
            saved_at: now_secs(),
            prompt: truncate(prompt.trim(), 120),
            partial: None,
            files: BTreeMap::new(),
        },
    };
    checkpoint.write_manifest()?;
    Ok(checkpoint)
}

/// The lowest turn number a new checkpoint may take.
///
/// Read off the folder rather than counted in memory, for the reason
/// [`crate::session::next_archive_index`] gives: a session resumed with `/load`
/// has no idea how many turns an earlier run took, and a counter that restarted
/// at 1 would overwrite checkpoints that are still on disk.
fn next_index(session: &Path) -> usize {
    saved(session).last().map_or(1, |m| m.turn + 1)
}

/// Every checkpoint on disk, oldest first.
///
/// Anything unreadable is skipped rather than reported: a checkpoint folder that
/// cannot be parsed is one that cannot be restored either, and the useful answer
/// is the list of the ones that can.
pub fn saved(session: &Path) -> Vec<Manifest> {
    let Ok(entries) = std::fs::read_dir(session.join(DIR)) else {
        return Vec::new();
    };
    let mut manifests: Vec<Manifest> = entries
        .flatten()
        .filter_map(|entry| {
            let text = std::fs::read_to_string(entry.path().join(MANIFEST)).ok()?;
            serde_json::from_str(&text).ok()
        })
        .collect();
    manifests.sort_by_key(|m| m.turn);
    manifests
}

/// What a restore did, for the confirmation panel and the notice after it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Restored {
    /// Files put back to their earlier contents.
    pub restored: Vec<String>,
    /// Files the turn created, and the restore therefore removed.
    pub removed: Vec<String>,
    /// Paths that could not be dealt with, with the reason.
    pub failed: Vec<String>,
}

/// Every checkpoint from `from_turn` onwards, oldest first.
fn from(session: &Path, from_turn: usize) -> Vec<Manifest> {
    saved(session)
        .into_iter()
        .filter(|m| m.turn >= from_turn)
        .collect()
}

/// What rewinding to the start of turn `from_turn` would do, without doing it.
///
/// Merges every checkpoint from that turn onwards. Where two mention the same
/// path the **oldest wins**, because the state being rewound to is the one the
/// oldest turn began from — a later checkpoint's copy is a state that is also
/// being undone.
///
/// The panel is a promise about what is about to happen, so it is built here and
/// the restore below walks the same merged plan. Two functions that agreed only
/// by inspection would be two chances to disagree.
pub fn plan_rewind(session: &Path, from_turn: usize) -> Restored {
    let mut merged: BTreeMap<String, bool> = BTreeMap::new();
    // Oldest first, and `or_insert` keeps the first seen, so the oldest wins.
    for manifest in from(session, from_turn) {
        for (key, entry) in &manifest.files {
            merged.entry(key.clone()).or_insert(entry.existed);
        }
    }
    let mut plan = Restored::default();
    for (key, existed) in merged {
        if existed {
            plan.restored.push(key);
        } else {
            plan.removed.push(key);
        }
    }
    plan
}

/// How many *checkpoints* a rewind to `from_turn` would apply.
///
/// Not the number the app reports, and deliberately so: what a user is undoing
/// is turns of conversation, and rewinding past two turns that changed no files
/// still puts the conversation back two turns. `App::rewind_plan` counts those
/// from its rows. This counts the work on disk, which is what the tests below
/// pin the numbering with.
#[cfg_attr(not(test), allow(dead_code))]
pub fn turns_from(session: &Path, from_turn: usize) -> usize {
    from(session, from_turn).len()
}

/// What restoring checkpoint `turn` alone would do. The single-turn case of
/// [`plan_rewind`], for `/undo`.
pub fn preview(session: &Path, turn: usize) -> Option<(Manifest, Restored)> {
    let manifest = saved(session).into_iter().find(|m| m.turn == turn)?;
    Some((manifest, plan_rewind(session, turn)))
}

/// Put the workspace back to the state at the start of turn `from_turn`.
///
/// Applies the checkpoints newest first, so each older one overwrites what the
/// younger left and the oldest state is the one that survives. Files that
/// existed are copied back; files created since are deleted.
///
/// A failure on one path does not abandon the rest — a restore that stopped
/// halfway would leave the tree in a state that was never real. The restored
/// checkpoints are then spent, and dropped.
pub fn restore_to(session: &Path, sandbox: &Sandbox, from_turn: usize) -> Restored {
    let manifests = from(session, from_turn);
    // The plan the caller was shown, computed the same way here. A path touched
    // by three checkpoints is one file to the person reading it, so the report
    // is the merged plan rather than a tally of the copies made below.
    let mut done = plan_rewind(session, from_turn);

    // Newest first: an older checkpoint's copy is the earlier state, so letting
    // it land last is what makes the result the state before `from_turn`.
    for manifest in manifests.iter().rev() {
        let folder = session.join(DIR).join(format!("{:03}", manifest.turn));
        for (key, entry) in &manifest.files {
            let target = sandbox.root().join(key);
            if entry.existed {
                let source = folder.join(FILES).join(key);
                let result = target
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|_| std::fs::copy(&source, &target).map(|_| ()));
                if let Err(e) = result {
                    done.failed.push(format!("{key}: {e}"));
                }
            } else {
                match std::fs::remove_file(&target) {
                    Ok(()) => {}
                    // Absent already is the state we wanted.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => done.failed.push(format!("{key}: {e}")),
                }
            }
        }
    }

    for manifest in &manifests {
        prune_one(session, manifest.turn);
    }
    done
}

/// Drop one checkpoint by number.
///
/// Used after a restore: the turn it described has been undone, so keeping a
/// snapshot of the state before it would offer to undo a turn twice.
pub fn prune_one(session: &Path, turn: usize) {
    let _ = std::fs::remove_dir_all(session.join(DIR).join(format!("{turn:03}")));
}

/// Drop the oldest checkpoints, keeping the newest `keep`.
///
/// `None` keeps everything, which is the default: the point of a checkpoint is
/// to be there when it is wanted, and how far back that is depends on the work.
pub fn prune(session: &Path, keep: Option<usize>) -> usize {
    let Some(keep) = keep else {
        return 0;
    };
    let manifests = saved(session);
    let Some(cut) = manifests.len().checked_sub(keep) else {
        return 0;
    };
    let mut dropped = 0;
    for manifest in &manifests[..cut] {
        let folder = session.join(DIR).join(format!("{:03}", manifest.turn));
        if std::fs::remove_dir_all(&folder).is_ok() {
            dropped += 1;
        }
    }
    dropped
}

/// A workspace-relative path, or `None` for anything outside the root.
fn relative(sandbox: &Sandbox, path: &Path) -> Option<String> {
    path.strip_prefix(sandbox.root())
        .ok()?
        .to_str()
        .filter(|key| !key.is_empty())
        .map(str::to_string)
}

/// Which cap stopped a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capped {
    Files,
    Bytes,
    Time,
}

impl std::fmt::Display for Capped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Files => write!(f, "too many files"),
            Self::Bytes => write!(f, "too large"),
            Self::Time => write!(f, "took too long"),
        }
    }
}

/// The workspace walk, collecting paths to copy.
///
/// The same rules as `crate::search`'s walk, for the same reasons: the skip list
/// keeps it out of `target/` and `.git`, `denies_read` is the actual security
/// boundary and is checked per entry, and symlinks are skipped rather than
/// followed so the walk cannot leave the root or loop.
struct Walk<'a> {
    sandbox: &'a Sandbox,
    caps: Caps,
    /// A subtree to stay out of: the session's own folder.
    exclude: Option<&'a Path>,
    deadline: Instant,
    files: usize,
    bytes: u64,
    capped: Option<Capped>,
    found: Vec<PathBuf>,
}

impl Walk<'_> {
    fn descend(&mut self, dir: &Path, depth: usize) {
        if depth > MAX_DEPTH || self.capped.is_some() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);

        for entry in entries {
            if self.capped.is_some() {
                return;
            }
            if Instant::now() >= self.deadline {
                self.capped = Some(Capped::Time);
                return;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if self.sandbox.denies_read(&path) {
                continue;
            }
            if self.exclude.is_some_and(|dir| path.starts_with(dir)) {
                continue;
            }
            if kind.is_dir() {
                if crate::search::SKIP_DIRS
                    .iter()
                    .any(|skip| entry.file_name() == *skip)
                {
                    continue;
                }
                self.descend(&path, depth + 1);
            } else if kind.is_file() {
                self.files += 1;
                if self.files > self.caps.files {
                    self.capped = Some(Capped::Files);
                    return;
                }
                self.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                if self.bytes > self.caps.bytes {
                    self.capped = Some(Capped::Bytes);
                    return;
                }
                self.found.push(path);
            }
        }
    }
}

/// Deepest directory nesting the walk will follow, matching the search's.
const MAX_DEPTH: usize = 24;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sandbox rooted at a fresh temp directory, with the directory's path.
    fn workspace(label: &str) -> (Sandbox, PathBuf, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ai-harness-checkpoint-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let session = root.join(".ai_harness").join("sessions").join("s");
        std::fs::create_dir_all(&session).unwrap();
        let sandbox = Sandbox::for_tests(root.clone());
        // Canonicalised by the sandbox (/var → /private/var on macOS), so use its
        // idea of the root for anything compared against a captured path.
        let root = sandbox.root().to_path_buf();
        (sandbox, root, session)
    }

    fn write(root: &Path, name: &str, contents: &str) {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn read(root: &Path, name: &str) -> Option<String> {
        std::fs::read_to_string(root.join(name)).ok()
    }

    #[test]
    fn a_captured_file_restores_to_what_it_was() {
        let (sandbox, root, session) = workspace("restore");
        write(&root, "a.rs", "original");

        let mut cp = open(&session, 0, "change a.rs").unwrap();
        cp.capture_file(&sandbox, &root.join("a.rs")).unwrap();
        write(&root, "a.rs", "clobbered");

        let done = restore_to(&session, &sandbox, 1);
        assert_eq!(read(&root, "a.rs").as_deref(), Some("original"));
        assert_eq!(done.restored, vec!["a.rs"]);
        assert!(done.failed.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The `existed` flag earning its place: undoing a turn that created a file
    /// means removing it, not leaving it behind alongside a restored tree.
    #[test]
    fn a_file_the_turn_created_is_removed_on_restore() {
        let (sandbox, root, session) = workspace("created");
        let mut cp = open(&session, 0, "add b.rs").unwrap();
        cp.capture_file(&sandbox, &root.join("b.rs")).unwrap(); // does not exist yet
        write(&root, "b.rs", "brand new");

        let done = restore_to(&session, &sandbox, 1);
        assert_eq!(read(&root, "b.rs"), None, "the new file must be gone");
        assert_eq!(done.removed, vec!["b.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A turn that writes the same file twice began from the *first* state.
    #[test]
    fn the_first_capture_of_a_path_in_a_turn_wins() {
        let (sandbox, root, session) = workspace("first-wins");
        write(&root, "a.rs", "state one");

        let mut cp = open(&session, 0, "twice").unwrap();
        cp.capture_file(&sandbox, &root.join("a.rs")).unwrap();
        write(&root, "a.rs", "state two");
        cp.capture_file(&sandbox, &root.join("a.rs")).unwrap();
        write(&root, "a.rs", "state three");

        restore_to(&session, &sandbox, 1);
        assert_eq!(read(&root, "a.rs").as_deref(), Some("state one"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The case the feature exists for: a command that removes everything.
    #[test]
    fn a_workspace_snapshot_survives_the_tree_being_deleted() {
        let (sandbox, root, session) = workspace("rm-rf");
        write(&root, "a.rs", "one");
        write(&root, "src/b.rs", "two");
        write(&root, "docs/c.md", "three");

        let mut cp = open(&session, 0, "rm -rf *").unwrap();
        cp.capture_workspace(&sandbox, Caps::default()).unwrap();
        assert!(cp.partial().is_none(), "the fixture fits inside the caps");

        for entry in ["a.rs", "src", "docs"] {
            let path = root.join(entry);
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir_all(&path);
        }
        assert_eq!(read(&root, "a.rs"), None, "the fixture really was deleted");

        restore_to(&session, &sandbox, 1);
        assert_eq!(read(&root, "a.rs").as_deref(), Some("one"));
        assert_eq!(read(&root, "src/b.rs").as_deref(), Some("two"));
        assert_eq!(read(&root, "docs/c.md").as_deref(), Some("three"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_walk_skips_what_a_search_skips_and_its_own_folder() {
        let (sandbox, root, session) = workspace("skips");
        write(&root, "keep.rs", "keep");
        write(&root, "target/huge.bin", "no");
        write(&root, ".git/config", "no");
        write(&root, "node_modules/dep/index.js", "no");

        let mut cp = open(&session, 0, "x").unwrap();
        cp.capture_workspace(&sandbox, Caps::default()).unwrap();

        let files: Vec<&String> = cp.manifest.files.keys().collect();
        assert!(files.iter().any(|f| f.as_str() == "keep.rs"), "{files:?}");
        for skipped in ["target", ".git", "node_modules", ".ai_harness"] {
            assert!(
                !files.iter().any(|f| f.starts_with(skipped)),
                "{skipped} should not be captured: {files:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cap_is_reported_rather_than_silently_truncating() {
        let (sandbox, root, session) = workspace("capped");
        for i in 0..12 {
            write(&root, &format!("f{i}.txt"), "x");
        }
        let caps = Caps {
            files: 4,
            ..Caps::default()
        };
        let mut cp = open(&session, 0, "x").unwrap();
        cp.capture_workspace(&sandbox, caps).unwrap();

        assert_eq!(cp.partial(), Some("too many files"));
        assert!(cp.len() <= 4, "captured {}", cp.len());
        // And it survives to the manifest, so a restore in a later run still
        // knows the snapshot it is working from was incomplete.
        let manifest = saved(&session).pop().unwrap();
        assert_eq!(manifest.partial.as_deref(), Some("too many files"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn numbering_continues_from_what_is_on_disk() {
        let (sandbox, root, session) = workspace("numbering");
        write(&root, "a.rs", "one");
        for expected in 1..=3 {
            let mut cp = open(&session, 0, "turn").unwrap();
            cp.capture_file(&sandbox, &root.join("a.rs")).unwrap();
            assert_eq!(cp.turn(), expected);
        }
        // A fresh read of the folder is what a reloaded session would do.
        assert_eq!(next_index(&session), 4);
        assert_eq!(saved(&session).len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rewinding past several turns lands on the state before the *oldest* of
    /// them, so where two checkpoints hold the same file the older copy is the
    /// one that must survive.
    #[test]
    fn a_rewind_across_turns_lands_on_the_oldest_state() {
        let (sandbox, root, session) = workspace("rewind-across");
        write(&root, "a.rs", "v1");

        for (turn, next) in [(3, "v2"), (4, "v3"), (5, "v4")] {
            let mut cp = open(&session, turn, "turn").unwrap();
            cp.capture_file(&sandbox, &root.join("a.rs")).unwrap();
            write(&root, "a.rs", next);
        }
        assert_eq!(read(&root, "a.rs").as_deref(), Some("v4"));

        // Back to the start of turn 3, not the start of turn 5.
        let done = restore_to(&session, &sandbox, 3);
        assert_eq!(read(&root, "a.rs").as_deref(), Some("v1"));
        assert_eq!(done.restored, vec!["a.rs"], "one file, not three");
        assert!(saved(&session).is_empty(), "the spent checkpoints are gone");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file created midway and edited later is *absent* before the turn that
    /// created it, so a rewind past that turn deletes it however many later
    /// checkpoints hold a copy.
    #[test]
    fn a_rewind_deletes_a_file_created_partway_through() {
        let (sandbox, root, session) = workspace("rewind-created");
        let mut second = open(&session, 2, "add it").unwrap();
        second.capture_file(&sandbox, &root.join("new.rs")).unwrap(); // absent
        write(&root, "new.rs", "created");

        let mut fourth = open(&session, 4, "edit it").unwrap();
        fourth.capture_file(&sandbox, &root.join("new.rs")).unwrap(); // exists
        write(&root, "new.rs", "edited");

        let plan = plan_rewind(&session, 2);
        assert_eq!(plan.removed, vec!["new.rs"], "the oldest entry wins");
        assert!(plan.restored.is_empty());

        restore_to(&session, &sandbox, 2);
        assert_eq!(
            read(&root, "new.rs"),
            None,
            "gone, not restored to 'created'"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Turns that changed nothing take no folder, so the numbering has gaps.
    /// Rewinding to a gap still catches every checkpoint after it.
    #[test]
    fn numbering_tolerates_gaps_from_turns_that_changed_nothing() {
        let (sandbox, root, session) = workspace("gaps");
        write(&root, "a.rs", "one");
        for turn in [2, 5, 9] {
            let mut cp = open(&session, turn, "turn").unwrap();
            cp.capture_file(&sandbox, &root.join("a.rs")).unwrap();
        }
        let turns: Vec<usize> = saved(&session).iter().map(|m| m.turn).collect();
        assert_eq!(turns, vec![2, 5, 9]);

        assert_eq!(turns_from(&session, 5), 2, "5 and 9");
        assert_eq!(turns_from(&session, 6), 1, "a gap still selects 9");
        assert_eq!(turns_from(&session, 10), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_keeps_the_newest_and_none_keeps_everything() {
        let (sandbox, root, session) = workspace("prune");
        write(&root, "a.rs", "one");
        for _ in 0..5 {
            let mut cp = open(&session, 0, "turn").unwrap();
            cp.capture_file(&sandbox, &root.join("a.rs")).unwrap();
        }

        assert_eq!(prune(&session, None), 0, "the default keeps everything");
        assert_eq!(saved(&session).len(), 5);

        assert_eq!(prune(&session, Some(2)), 3);
        let left: Vec<usize> = saved(&session).iter().map(|m| m.turn).collect();
        assert_eq!(left, vec![4, 5], "the newest survive");
        // And numbering still continues past what was pruned.
        assert_eq!(next_index(&session), 6);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn preview_says_what_a_restore_would_do() {
        let (sandbox, root, session) = workspace("preview");
        write(&root, "old.rs", "here");
        let mut cp = open(&session, 7, "do a thing").unwrap();
        cp.capture_file(&sandbox, &root.join("old.rs")).unwrap();
        cp.capture_file(&sandbox, &root.join("new.rs")).unwrap();

        let (manifest, plan) = preview(&session, 7).expect("checkpoint 7");
        assert_eq!(
            manifest.turn, 7,
            "numbered by turn, not by checkpoint count"
        );
        assert_eq!(manifest.prompt, "do a thing");
        assert_eq!(plan.restored, vec!["old.rs"]);
        assert_eq!(plan.removed, vec!["new.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `--sessions-dir` can put the session folder inside the workspace, where
    /// the `.ai_harness` skip does not reach it. A snapshot that walked into it
    /// would copy the checkpoints into themselves, growing on every turn.
    #[test]
    fn a_snapshot_stays_out_of_its_own_session_folder() {
        let (sandbox, root, _) = workspace("self-copy");
        // Not under .ai_harness: exactly what --sessions-dir allows.
        let session = root.join("my-sessions").join("s");
        std::fs::create_dir_all(&session).unwrap();
        write(&root, "keep.rs", "keep");

        let mut first = open(&session, 0, "one").unwrap();
        first.capture_workspace(&sandbox, Caps::default()).unwrap();
        let mut second = open(&session, 0, "two").unwrap();
        second.capture_workspace(&sandbox, Caps::default()).unwrap();

        let files: Vec<&String> = second.manifest.files.keys().collect();
        assert!(files.iter().any(|f| f.as_str() == "keep.rs"), "{files:?}");
        assert!(
            !files.iter().any(|f| f.starts_with("my-sessions")),
            "a snapshot must not contain the checkpoints: {files:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A workspace of its own named `manifest.json` must not collide with ours.
    #[test]
    fn a_workspace_manifest_does_not_overwrite_the_checkpoints() {
        let (sandbox, root, session) = workspace("collide");
        write(&root, "manifest.json", "the project's own");

        let mut cp = open(&session, 3, "x").unwrap();
        cp.capture_workspace(&sandbox, Caps::default()).unwrap();
        // Ours still parses off disk while the workspace's own file is captured.
        assert_eq!(saved(&session)[0].turn, 3);
        write(&root, "manifest.json", "clobbered");

        restore_to(&session, &sandbox, 1);
        assert_eq!(
            read(&root, "manifest.json").as_deref(),
            Some("the project's own")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
