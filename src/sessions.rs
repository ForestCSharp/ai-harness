//! Several conversations at once, and the view for moving between them.
//!
//! Each session is a whole [`App`] with its own conversation, settings, model
//! and in-flight work. They run at the same time: a session you are not looking
//! at goes on streaming and running commands, and its updates find their way
//! back to it by id rather than by being the only thing running.
//!
//! What a background session *cannot* do is ask you something. An approval modal
//! belongs to a screen, and it is not on screen — so a session that proposes
//! something needing approval parks in [`crate::app::Status::AwaitingApproval`]
//! and the view marks it as wanting you. Under `--auto-approve` it carries on
//! instead, and that falls out of the existing dispatch rather than being a case
//! anything here handles.
//!
//! They share one working directory. Nothing isolates them from each other — see
//! the note on checkpoints in the README, which is the sharp edge of that.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::app::{App, Catalog, Status};
use crate::input::Input;
use crate::pty::PtySession;
use crate::session::{ClaudeRecord, OpenSet};
use crate::ui::TranscriptCache;

/// Sessions named in the record that are no longer on disk.
///
/// Named rather than passed over: a session that is gone is more likely to be a
/// surprise than a decision you remember making.
fn missing_notice(missing: &[String]) -> String {
    format!("Could not reopen {}: no longer saved.", missing.join(", "))
}

/// Most sessions kept at once.
///
/// Each holds an entire conversation in memory, and a screenful of them is
/// already more than anyone is tracking. The cap is a memory bound and a
/// usability one at the same time.
pub const MAX_SESSIONS: usize = 8;

/// A handle to a session's current background task.
///
/// Sending or dropping resolves the task's cancel future, stopping its work
/// cleanly. One per session rather than one for the harness: that is what lets
/// two sessions have something in flight at the same time.
pub struct InFlight {
    pub cancel: oneshot::Sender<()>,
}

impl InFlight {
    pub fn new(cancel: oneshot::Sender<()>) -> Self {
        Self { cancel }
    }
}

/// What is on the other side of a session.
///
/// `Harness` is this harness's own loop — the transcript, the approval modal,
/// the protocol in [`crate::protocol`]. `Claude` is Claude Code itself, running
/// on a pseudo-terminal and drawing its own interface in the pane where the
/// transcript would be. They are both sessions: both are named, both are in the
/// list, both are something you switch to. What differs is who is answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionKind {
    #[default]
    Harness,
    Claude,
}

impl SessionKind {
    /// What the new-session modal and the sessions view call it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::Claude => "claude code",
        }
    }

    pub fn other(self) -> Self {
        match self {
            Self::Harness => Self::Claude,
            Self::Claude => Self::Harness,
        }
    }
}

/// One running session: its conversation, its background work, its rendering.
pub struct Slot {
    /// Stable for the life of the session. An index is not: closing a session
    /// renumbers everything after it, and a reply already in flight would come
    /// back addressed to whichever session had shuffled into its place.
    pub id: u64,
    pub app: App,
    pub inflight: Option<InFlight>,
    /// Travels with the slot rather than the screen: it holds this
    /// conversation's wrapped rows, and rebuilding a long one on every switch is
    /// the cost it exists to avoid.
    pub cache: TranscriptCache,
    /// The session that spawned this one as a read-only subagent, if any.
    ///
    /// A child is a *conversation*, which is why it is a slot at all — it needs
    /// its own history, its own `InFlight` and its own place in `route`. But it
    /// is not a *session*: nobody asked for it, nobody switches to it, and it
    /// is gone within the turn. So it is hidden from everything the user sees or
    /// that outlives the process — see [`Sessions::is_child`] for the list.
    pub parent: Option<u64>,
    /// The hosted program, when this session's agent is one rather than this
    /// harness. `Some` is what makes a slot a Claude session; its `app` stays
    /// beside it, holding the name, the working directory and the slot's place
    /// in the list, and is never asked to hold a conversation.
    pub claude: Option<PtySession>,
    /// What reopens that program next launch. Set alone, on a restored slot,
    /// until [`Sessions::attach`] starts it.
    pub claude_record: Option<ClaudeRecord>,
}

/// A subagent that has finished, and whether its parent still wants the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledChild {
    pub child: u64,
    pub parent: u64,
    pub wanted: bool,
}

/// What the view shows for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: u64,
    pub name: String,
    pub model: String,
    /// A short word for the session's state — `ready`, `streaming`, `needs you`.
    pub status: &'static str,
    /// Whether it is waiting on a *person* rather than on time. The one state
    /// worth marking, since it is the only one that will not resolve itself.
    pub blocked: bool,
    pub busy: bool,
    pub turns: usize,
    pub focused: bool,
    /// The directory the session runs in — its sandbox root, which `/cd`
    /// moves. Empty when there is no sandbox, which only tests see.
    pub cwd: String,
    /// The last few things that happened, oldest first. What the session is
    /// *doing* is the reason to look at this list at all — a column of names and
    /// the word "streaming" says which one is busy but not what with.
    pub activity: Vec<String>,
}

/// Lines of activity shown per session.
///
/// Enough to tell what a session is in the middle of, few enough that eight of
/// them still fit on a screen.
pub const ACTIVITY_LINES: usize = 3;

/// The sessions view, while it is open.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct View {
    /// A position in the *matches*, not in the slots — the query can hide rows
    /// between the highlight and the session it names.
    pub selected: usize,
    /// What has been typed to narrow the list, on the same terms as the `/load`
    /// and `/model` pickers: the list is navigable, and `/` starts a search.
    pub query: Input,
    pub searching: bool,
}

/// A question the harness is asking, in front of whichever view is up.
///
/// On `Sessions` rather than on an [`App`] because both questions are about the
/// *set* of sessions rather than about any one conversation — and because they
/// have to show over the sessions view too, which has no `App` behind it and so
/// cannot reach the panel slot every other modal in the harness lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// Naming a new session, and choosing what will be on the other side of
    /// it. An empty name is the name the harness would have picked itself,
    /// which is what `Ctrl+N` did before it asked.
    New { name: Input, kind: SessionKind },
    /// Shutting one down. The name travels with it so the modal can say which
    /// session is about to go, and the slot so that a list shifting underneath
    /// cannot redirect the answer.
    Close { slot: usize, name: String },
}

pub struct Sessions {
    slots: Vec<Slot>,
    current: usize,
    view: Option<View>,
    next_id: u64,
    /// The project these sessions belong to, stamped on the open-set record so a
    /// shared `--sessions-dir` cannot resume another project's work.
    root: PathBuf,
    /// The open set as last written, so [`Sessions::maybe_record_open`] can tell
    /// whether anything changed.
    recorded: Option<OpenSet>,
    /// Whether the session pane is showing beside the transcript. On by
    /// default: the thing it answers — is anything else still working? — is
    /// worth a slice of the screen more often than it is not. `Ctrl+Y` turns
    /// it off and the full-screen view (`Ctrl+Space`) is still there either way.
    sidebar: bool,
    /// The pane's width in columns, once the divider has been dragged. `None`
    /// is the eighth of the screen it starts at. Columns rather than a
    /// fraction because that is what dragging means: the divider stays where
    /// it was let go of, not at the nearest percent of a width that changes
    /// whenever the terminal does.
    pane_cols: Option<u16>,
    /// Where on the divider the pointer took hold of it, as columns right of
    /// the pane's own border — `None` when nothing is holding it.
    ///
    /// The press is what starts a resize, so the drags that follow have to know
    /// the gesture began there: one that merely passes over the divider is not
    /// a resize. The offset is kept because the divider is two columns wide —
    /// the pane's right border and the transcript's left one, which sit next to
    /// each other — and without it, grabbing the right-hand column would snap
    /// the split a column sideways before the pointer had moved.
    pane_grab: Option<u16>,
    /// The question in front of both views, if one is being asked.
    ask: Option<Ask>,
    /// The event loop's channel, so a session hosting a program can say its
    /// screen changed. `None` outside the event loop — in tests, and in the
    /// headless runner, neither of which draws anything.
    tx: Option<mpsc::Sender<crate::Tagged>>,
    /// What to pass `claude --permission-mode`, if anything.
    claude_permission_mode: Option<String>,
}

/// The program a Claude session hosts. Taken from `PATH`, so it is whichever
/// `claude` the user's own shell would run.
const CLAUDE_PROGRAM: &str = "claude";

/// Where Claude Code keeps its conversations.
fn claude_projects() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".claude")))
        .unwrap_or_default()
        .join("projects")
}

/// Whether Claude Code would resume `id`: the file alone is not enough, since
/// it writes one for a session that was never typed into and then refuses it.
///
/// shortcut: reads claude's private transcript format, so a change there means
/// a fresh session rather than a resumed one; the conversation stays on disk.
fn has_conversation(projects: &Path, id: &str) -> bool {
    let Ok(dirs) = std::fs::read_dir(projects) else {
        return false;
    };
    dirs.flatten().any(|dir| {
        std::fs::read_to_string(dir.path().join(format!("{id}.jsonl")))
            .is_ok_and(|text| text.contains("\"type\":\"user\""))
    })
}

/// A random (v4) UUID, the only form `claude --session-id` takes.
fn new_claude_id() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

impl Sessions {
    /// Start with one session, the way the harness has always started.
    pub fn new(app: App) -> Self {
        let root = app.sandbox.as_ref().map_or_else(
            || app.sessions_dir().to_path_buf(),
            |sandbox| sandbox.root().to_path_buf(),
        );
        Self {
            slots: vec![Slot {
                id: 1,
                app,
                inflight: None,
                cache: TranscriptCache::default(),
                parent: None,
                claude: None,
                claude_record: None,
            }],
            current: 0,
            view: None,
            next_id: 2,
            root,
            recorded: None,
            sidebar: true,
            pane_cols: None,
            pane_grab: None,
            ask: None,
            tx: None,
            claude_permission_mode: None,
        }
    }

    /// Give the set what it needs to host a program: somewhere to report a
    /// redrawn screen, and the permission mode to start `claude` under. Starts
    /// the Claude sessions [`Sessions::restore`] left waiting for exactly that.
    ///
    /// Apart rather than in [`Sessions::new`] because every other caller —
    /// tests, the headless runner — has neither and wants neither, and a
    /// constructor that demanded them would make a session set impossible to
    /// build without an event loop around it.
    pub fn attach(&mut self, tx: mpsc::Sender<crate::Tagged>, permission_mode: Option<String>) {
        self.tx = Some(tx);
        self.claude_permission_mode = permission_mode;
        for i in 0..self.slots.len() {
            let Some(record) = self.slots[i].claude_record.take() else {
                continue;
            };
            if let Err(why) = self.start_claude(i, record.cwd, Some(record.id.clone())) {
                // Left as an empty harness session, so the notice has a
                // transcript to appear in.
                self.slots[i].app.push_notice(format!(
                    "{why} `claude --resume {}` still has the conversation.",
                    record.id
                ));
            }
        }
    }

    /// Reopen the sessions that were open when the harness last quit.
    ///
    /// Falls back to [`Sessions::new`] whenever there is nothing usable to
    /// resume — restore turned off, no record, a record belonging to another
    /// project, or one whose sessions have all been deleted since. `app` is the
    /// template either way: it carries this launch's flags, and each restored
    /// session takes them through [`App::spawn_sibling`] before adopting its own
    /// saved model and conversation.
    pub fn restore(app: App, sessions_dir: &Path, root: &Path) -> Self {
        let Some(open) = crate::session::read_open(sessions_dir) else {
            return Self::new(app);
        };
        if open.root != root {
            // Another project's set, through a shared `--sessions-dir`. The next
            // quit overwrites it with this project's.
            return Self::new(app);
        }

        let mut sessions = Self::new(app);
        let mut missing = Vec::new();
        for name in open.names.iter().take(MAX_SESSIONS) {
            // Always from slot 0, which is the template. Spawning from the
            // previously restored session instead would chain each one's
            // adopted model into the next.
            let mut fresh = sessions.slots[0].app.spawn_sibling(name.clone());
            if let Some(record) = open.claude.get(name) {
                // Its directory, not the template's: this is what the rows show.
                if let Some(moved) = fresh
                    .sandbox
                    .as_ref()
                    .and_then(|sandbox| sandbox.rooted_at(&record.cwd).ok())
                {
                    fresh.sandbox = Some(moved);
                }
                sessions.push(fresh);
                sessions.current_mut().claude_record = Some(record.clone());
                continue;
            }
            if !fresh.open_saved(name) {
                missing.push(name.clone());
                continue;
            }
            sessions.push(fresh);
        }

        let restored = sessions.slots.len() - 1;
        if restored == 0 {
            // Nothing came back, so the template stays as the fresh session the
            // harness has always started with — and says why, if it was meant to
            // be more than that.
            if !missing.is_empty() {
                sessions.app_mut().push_notice(missing_notice(&missing));
            }
            return sessions;
        }
        sessions.slots.remove(0);
        sessions.current = open.current.min(sessions.slots.len() - 1);

        sessions
            .app_mut()
            .push_notice(format!("Reopened {restored} session(s) from last time."));
        if !missing.is_empty() {
            sessions.app_mut().push_notice(missing_notice(&missing));
        }
        // Lands in the conversation it left, not on the view. Resuming used to
        // open the view, so that a session restored with focus could not hide
        // that there were others — the pane says that beside the transcript
        // now, which is the whole reason it is the default.
        sessions
    }

    /// Add a slot holding `app`, focused. The one place a session is appended.
    fn push(&mut self, app: App) {
        let id = self.next_id;
        self.next_id += 1;
        self.slots.push(Slot {
            id,
            app,
            inflight: None,
            cache: TranscriptCache::default(),
            parent: None,
            claude: None,
            claude_record: None,
        });
        self.current = self.slots.len() - 1;
    }

    /// Open a read-only subagent for session `parent`, without focusing it.
    ///
    /// Deliberately not [`Sessions::spawn`]: that one focuses what it opens,
    /// names it for the user and announces it, all of which are wrong for a
    /// conversation the user did not ask for. Returns the child's id so the
    /// parent can be told what to wait for.
    pub fn spawn_child(&mut self, parent: u64, app: App) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.slots.push(Slot {
            id,
            app,
            inflight: None,
            cache: TranscriptCache::default(),
            parent: Some(parent),
            // A subagent is this harness's own loop by construction: it is
            // spawned to answer a question with the harness's tools.
            claude: None,
            claude_record: None,
        });
        id
    }

    /// Whether slot `i` is a subagent rather than a session of the user's.
    ///
    /// The one predicate every hiding site shares, so "is this the user's?" is
    /// answered in one place rather than by five `parent.is_none()` checks that
    /// could drift apart.
    fn is_child(&self, i: usize) -> bool {
        self.slots[i].parent.is_some()
    }

    /// How many of the user's sessions are open.
    ///
    /// Children are left out: this is the number in the status bar and the
    /// number the cap is measured against, and both are about sessions the user
    /// opened. A subagent that counted would make spawning one able to refuse
    /// the next `Ctrl+N` with "already running 8 sessions" — for something the
    /// user never asked for.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.parent.is_none()).count()
    }

    pub fn current(&self) -> &Slot {
        &self.slots[self.current]
    }

    pub fn current_mut(&mut self) -> &mut Slot {
        &mut self.slots[self.current]
    }

    /// The focused session's conversation, which is what the prompt and the
    /// transcript are about.
    pub fn app(&self) -> &App {
        &self.current().app
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.current_mut().app
    }

    /// Every session. The loop only ever needs `iter_mut`; this is what the
    /// tests below check the shared state with.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn iter(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Slot> {
        self.slots.iter_mut()
    }

    /// The slot an update belongs to, or `None` if that session has closed.
    ///
    /// A closed session's replies are dropped exactly as a stale generation's
    /// are: the work was abandoned, and there is nothing left to apply it to.
    pub fn route(&mut self, id: u64) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|slot| slot.id == id)
    }

    /// Whether any session has work in flight, which is what the spinner and the
    /// redraw tick want to know — not whether the focused one does.
    pub fn any_busy(&self) -> bool {
        self.slots.iter().any(|slot| {
            matches!(
                slot.app.status,
                Status::Waiting | Status::Streaming | Status::Running | Status::Compacting
            )
        })
    }

    /// How many sessions are waiting on a person rather than on time.
    /// Children are left out, and structurally cannot be in: a read-only
    /// subagent has every blocking action refused, so it can never reach an
    /// approval or a question. The filter is here for the case where that stops
    /// being true — a count that said "needs you" about a conversation the user
    /// cannot switch to would be unanswerable.
    pub fn blocked(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.parent.is_none() && Self::is_blocked(&slot.app))
            .count()
    }

    fn is_blocked(app: &App) -> bool {
        matches!(
            app.status,
            Status::AwaitingApproval(_)
                | Status::AwaitingChoice(_)
                | Status::AwaitingExecute { .. }
        )
    }

    /// Hand the catalog to every session, and remember it for the next one.
    ///
    /// The fetch happens once at startup and belongs to no session, so its
    /// result goes to all of them.
    pub fn set_catalog(&mut self, result: Result<Vec<crate::openrouter::ModelInfo>, String>) {
        if let Some(first) = self.slots.first_mut() {
            first.app.set_catalog(result);
            let catalog = first.app.catalog();
            for slot in self.slots.iter_mut().skip(1) {
                slot.app.share_catalog(catalog.clone());
            }
        }
    }

    fn catalog(&self) -> Arc<Catalog> {
        self.current().app.catalog()
    }

    /// Open a new session beside the current one, and focus it.
    ///
    /// Inherits the current session's settings — see [`App::spawn_sibling`] —
    /// so a session spawned from one running unattended is also unattended, and
    /// one spawned from a careful session also asks.
    pub fn spawn(&mut self) {
        self.spawn_named(None, SessionKind::Harness);
    }

    /// The same, under a name the user typed. `None` takes the name the harness
    /// would have picked.
    ///
    /// Returns whether a session was started, so the modal that asked for the
    /// name can stay up on a name that was refused rather than throwing away
    /// what was typed.
    pub fn spawn_named(&mut self, name: Option<&str>, kind: SessionKind) -> bool {
        if self.len() >= MAX_SESSIONS {
            self.app_mut().push_notice(format!(
                "Already running {MAX_SESSIONS} sessions, the most at once. Shut one \
                 down first — Ctrl+Space, then x."
            ));
            return false;
        }
        let name = match name {
            // A session name becomes a directory name, so it goes through the
            // same check `/rename` and `/load` go through rather than a second
            // rule that could differ from it.
            Some(typed) => match crate::session::sanitize(typed) {
                Ok(clean) if self.name_taken(clean) => {
                    self.app_mut().push_notice(format!(
                        "A session named {clean:?} already exists; pick another name."
                    ));
                    return false;
                }
                Ok(clean) => clean.to_string(),
                Err(e) => {
                    self.app_mut().push_notice(format!("{e:#}"));
                    return false;
                }
            },
            None => self.unique_name(),
        };
        let mut app = self.current().app.spawn_sibling(name);
        app.share_catalog(self.catalog());
        self.push(app);
        let name = self.app().session_name().to_string();
        if kind == SessionKind::Claude {
            // Spawned at a nominal size: the first draw measures the pane and
            // resizes it before the child has drawn anything worth keeping.
            let root = self
                .current()
                .app
                .sandbox
                .as_ref()
                .map_or_else(|| self.root.clone(), |s| s.root().to_path_buf());
            let started = self.start_claude(self.current, root, None);
            match started {
                Ok(()) => {
                    self.app_mut().push_notice(format!(
                        "New claude session {name:?}. Ctrl+Space or Ctrl+J/K switch between them, \
                         Ctrl+N starts another, Ctrl+D shuts it down and Ctrl+C twice quits; \
                         every other key goes to claude."
                    ));
                    return true;
                }
                Err(why) => {
                    // The slot is already open, so it is shut again rather than
                    // left as a session with no agent behind it.
                    self.close(self.current);
                    self.app_mut().push_notice(why);
                    return false;
                }
            }
        }
        self.app_mut().push_notice(format!(
            "New session {name:?}. Ctrl+Space switches between them."
        ));
        true
    }

    /// [`Sessions::spawn_named`]'s hosted path, with the program named — so a
    /// test can exercise the slot without an agent, an API key or a network.
    #[cfg(test)]
    pub fn spawn_hosted_for_test(&mut self, name: &str, program: &str, args: &[&str]) -> bool {
        if !self.spawn_named(Some(name), SessionKind::Harness) {
            return false;
        }
        let Some(tx) = self.tx.clone() else {
            return false;
        };
        let id = self.current().id;
        let root = self.root.clone();
        let root_for_record = root.clone();
        match PtySession::spawn(program, args, &root, 24, 80, id, tx) {
            Ok(session) => {
                self.current_mut().claude = Some(session);
                self.current_mut().claude_record = Some(ClaudeRecord {
                    id: "test-id".into(),
                    cwd: root_for_record,
                });
                true
            }
            Err(_) => false,
        }
    }

    /// Put `claude` on slot `i`, picking up conversation `id` if it has one.
    ///
    /// The fresh path also covers a restored session nobody typed into: claude
    /// has burned that id by then, so it refuses to resume it and refuses to
    /// start under it, and a new one loses nothing.
    fn start_claude(&mut self, i: usize, cwd: PathBuf, id: Option<String>) -> Result<(), String> {
        // Without the event loop's channel there is nowhere to report a redrawn
        // screen, which is every caller outside it — see [`Sessions::attach`].
        let Some(tx) = self.tx.clone() else {
            return Err("Claude sessions need the event loop.".to_string());
        };
        let resume = id.filter(|id| has_conversation(&claude_projects(), id));
        let id = match &resume {
            Some(id) => id.clone(),
            None => new_claude_id().map_err(|e| format!("Could not start claude: {e:#}"))?,
        };
        let args = self.claude_args(&id, resume.is_some());
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let session = PtySession::spawn(
            CLAUDE_PROGRAM,
            &borrowed,
            &cwd,
            24,
            80,
            self.slots[i].id,
            tx,
        )
        .map_err(|e| format!("Could not start claude: {e:#}"))?;
        self.slots[i].claude = Some(session);
        self.slots[i].claude_record = Some(ClaudeRecord { id, cwd });
        Ok(())
    }

    /// How `claude` is launched. Its own permission system decides what it may
    /// do, so the mode is the one thing the harness picks for it — see the
    /// README on what this does and does not confine. The id is chosen here
    /// rather than left to claude so there is something to resume by.
    fn claude_args(&self, id: &str, resume: bool) -> Vec<String> {
        let flag = if resume { "--resume" } else { "--session-id" };
        let mut args = vec![flag.to_string(), id.to_string()];
        // Otherwise nothing, so Claude Code uses its own default and asks.
        if let Some(mode) = &self.claude_permission_mode {
            args.extend(["--permission-mode".to_string(), mode.clone()]);
        }
        args
    }

    /// Whether `name` is already running or already on disk. The check
    /// [`Sessions::unique_name`] makes for itself, for a name that was typed.
    fn name_taken(&self, name: &str) -> bool {
        let dir = self.current().app.sessions_dir();
        self.slots.iter().any(|s| s.app.session_name() == name) || crate::session::exists(dir, name)
    }

    /// Put the focus in session `name`, opening it as a slot of its own if it is
    /// not already running.
    ///
    /// The one answer to "go to that session", however it was asked — the `l`
    /// shortcut in the view, a picker row, or `/load <name>`. Switching comes
    /// first and is not subject to the cap: a session that is already running
    /// takes no new slot, and refusing to *move* to one because eight are open
    /// would be refusing the wrong thing.
    pub fn reveal(&mut self, name: String) {
        if let Some(i) = self.slot_of(&name) {
            self.switch(i);
            return;
        }
        if self.len() >= MAX_SESSIONS {
            return self.app_mut().push_notice(format!(
                "Already running {MAX_SESSIONS} sessions, the most at once. Shut one \
                 down first — Ctrl+Space, then x."
            ));
        }
        let mut app = self.current().app.spawn_sibling(name.clone());
        app.share_catalog(self.catalog());
        if !app.open_saved(&name) {
            return self
                .app_mut()
                .push_notice(format!("Could not open session {name:?}."));
        }
        self.push(app);
        self.app_mut()
            .push_notice(format!("Opened session {name:?}."));
    }

    /// Which slot is running this session, if any.
    ///
    /// Two slots on one session file auto-save over each other, so this is what
    /// every way of opening one asks first — and the answer is where to go
    /// rather than a reason to refuse.
    pub fn slot_of(&self, name: &str) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.app.session_name() == name)
    }

    /// The names running right now, for the pickers to leave out.
    ///
    /// Children are not among them, which matters twice: a subagent is not a
    /// session you could `/load` into, and this same list is what
    /// [`Sessions::maybe_record_open`] writes to disk — so a child here would
    /// be **reopened as a session** on the next launch.
    pub fn open_names(&self) -> Vec<String> {
        self.slots
            .iter()
            .filter(|slot| slot.parent.is_none())
            .map(|slot| slot.app.session_name().to_string())
            .collect()
    }

    /// Make the set of open sessions known — to each session, and to disk.
    ///
    /// Both duties derive from the same list, and both are done by deriving and
    /// comparing rather than by hooking spawn, close, switch and rename: the
    /// reason [`App::maybe_autosave`] works from a fingerprint. One call site
    /// cannot be missed, and `/rename` is exactly the hook that would have been.
    pub fn sync_open_set(&mut self) {
        // What each session may not load. Its own name is excluded: `/load` of
        // the session you are already in is a no-op, not a collision.
        let names = self.open_names();
        for slot in self.slots.iter_mut() {
            let mine = slot.app.session_name();
            let others = names.iter().filter(|n| *n != mine).cloned().collect();
            slot.app.set_open_elsewhere(others);
        }
        self.maybe_record_open(names);
    }

    /// Write the open set when it has changed.
    fn maybe_record_open(&mut self, names: Vec<String>) {
        // `current` indexes `names`, which leaves children out, so it has to be
        // the focused session's position among the user's sessions rather than
        // its slot index. A child sitting before the focus would otherwise
        // shift the two apart and reopen on the wrong conversation.
        let current = self.slots[..self.current]
            .iter()
            .filter(|slot| slot.parent.is_none())
            .count();
        let open = OpenSet {
            version: crate::session::VERSION,
            root: self.root.clone(),
            current,
            names,
            claude: self
                .slots
                .iter()
                .filter(|slot| slot.parent.is_none())
                .filter_map(|slot| {
                    let record = slot.claude_record.clone()?;
                    Some((slot.app.session_name().to_string(), record))
                })
                .collect(),
        };
        if self.recorded.as_ref() == Some(&open) {
            return;
        }
        // Best effort: failing to record which sessions were open must not be
        // reported as if the conversations themselves were at risk. They are
        // each auto-saved in their own right.
        if crate::session::write_open(self.sessions_dir(), &open).is_ok() {
            self.recorded = Some(open);
        }
    }

    fn sessions_dir(&self) -> &Path {
        self.current().app.sessions_dir()
    }

    /// A session name that is taken neither by a running session nor by a
    /// folder on disk.
    ///
    /// `session::default_name` is the clock to the second, so two sessions
    /// spawned in the same second would take the same name and overwrite each
    /// other's file on the next autosave.
    fn unique_name(&self) -> String {
        let base = crate::session::default_name();
        let dir = self.current().app.sessions_dir();
        let taken = |name: &str| {
            self.slots.iter().any(|s| s.app.session_name() == name)
                || crate::session::exists(dir, name)
        };
        if !taken(&base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|name| !taken(name))
            .unwrap_or(base)
    }

    /// Open a read-only subagent for session `parent` and start it on `task`.
    ///
    /// Returns the child's id and the messages its first request should carry,
    /// or `None` if the parent has gone. Here rather than in the event loop
    /// because naming and slot-making are both `Sessions`' business, and doing
    /// it outside would mean borrowing the parent and the child at once.
    ///
    /// Not subject to `MAX_SESSIONS`: the cap is about how many conversations a
    /// person can keep track of, and this is not one of those — the user did not
    /// ask for it and cannot see it. `AGENT_ITERATIONS` is what bounds it.
    pub fn open_agent(
        &mut self,
        parent: u64,
        task: &str,
    ) -> Option<(u64, Vec<crate::openrouter::Message>)> {
        let name = self.unique_name();
        let mut child = self.route(parent)?.app.spawn_agent(name);
        child.share_catalog(self.catalog());
        let messages = child.send_prompt(task.to_string())?;
        Some((self.spawn_child(parent, child), messages))
    }

    /// Subagents that have run to a stop, each with its parent and whether that
    /// parent is still waiting for the answer.
    ///
    /// The `wanted` flag is the one that matters. A parent may have given up —
    /// `Esc`, or an error that ended the turn — and reporting to an idle
    /// conversation would not merely waste the answer: it would push a result
    /// in and start a turn the user deliberately stopped. So an unwanted child
    /// is dropped silently, which is what cancelling asked for.
    ///
    /// Here rather than in the event loop so the decision can be tested without
    /// a client, a runtime and a channel.
    pub fn settled_children(&self) -> Vec<SettledChild> {
        self.slots
            .iter()
            .filter(|slot| slot.parent.is_some() && !slot.app.is_busy() && slot.inflight.is_none())
            .map(|slot| {
                let parent = slot.parent.expect("filtered to children");
                SettledChild {
                    child: slot.id,
                    parent,
                    wanted: self
                        .slots
                        .iter()
                        .find(|s| s.id == parent)
                        .is_some_and(|p| p.app.is_busy()),
                }
            })
            .collect()
    }

    /// Shut down the subagent with this id, without saving it.
    ///
    /// A child is not a session: it was never announced, it cannot be reopened,
    /// and its conversation is the thing the parent was asked to be spared. So
    /// unlike [`Sessions::close`] this writes nothing to disk.
    pub fn close_child(&mut self, id: u64) {
        let Some(i) = self.slots.iter().position(|slot| slot.id == id) else {
            return;
        };
        if self.slots[i].parent.is_none() {
            return;
        }
        if let Some(inflight) = self.slots[i].inflight.take() {
            let _ = inflight.cancel.send(());
        }
        self.slots.remove(i);
        // A child is always appended after its parent, so removing one can only
        // shift the focus down if it sat before it — which it cannot. Clamped
        // anyway, because being wrong here would index out of bounds.
        self.current = self.current.min(self.slots.len().saturating_sub(1));
    }

    /// Focus session `i`, if it exists.
    pub fn switch(&mut self, i: usize) -> bool {
        if i >= self.slots.len() {
            return false;
        }
        self.current = i;
        // The conversation being looked at should be at its end, not wherever
        // the last look left the scroll.
        self.current_mut().app.follow = true;
        true
    }

    /// Shut session `i` down: stop its work, save it, and drop it.
    ///
    /// Not destructive, and so not confirmed: the conversation is on disk and
    /// `/load` brings it back. What is lost is the reply that was in flight, if
    /// there was one, which cancelling would have lost anyway.
    pub fn close(&mut self, i: usize) {
        if i >= self.slots.len() {
            return;
        }
        // The last one is not closable: the harness with no session is a state
        // with no prompt and nothing to type into. `/quit` is how you leave.
        if self.slots.len() == 1 {
            return self
                .app_mut()
                .push_notice("This is the only session; /quit exits the harness.");
        }
        let mut slot = self.slots.remove(i);
        if let Some(inflight) = slot.inflight.take() {
            let _ = inflight.cancel.send(());
        }
        // The hosted program and everything it started, before anything else:
        // a slot dropped with a child still on its pty leaves the child
        // running with nothing reading it.
        let hosted = slot.claude.take();
        if let Some(claude) = &hosted {
            claude.kill();
        }
        slot.app.cancel();
        slot.app.maybe_autosave();
        let name = slot.app.session_name().to_string();

        // Focus moves to the neighbour rather than to the start: closing the
        // session you were in should leave you next to where you were.
        self.current = self.current.min(self.slots.len() - 1);
        if i < self.current || (i == self.current && i > 0) {
            self.current = self.current.saturating_sub(1);
        }
        self.current = self.current.min(self.slots.len() - 1);
        // A claude session is not ours to save: its conversation lives in
        // Claude Code's own history, not in a session folder here.
        self.app_mut().push_notice(if hosted.is_some() {
            format!("Shut down {name:?}; claude's own history keeps the conversation.")
        } else {
            format!("Shut down {name:?}; it is saved and can be /loaded.")
        });
    }

    /// One row per session, in the order they were opened.
    ///
    /// Derived per call rather than stored, the same rule `rewind_rows` and
    /// `picker_matches` follow: the thing it describes changes under it
    /// constantly, and a stored copy would be a second source of truth.
    pub fn rows(&self) -> Vec<Row> {
        self.slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                let app = &slot.app;
                // A hosted program has no `Status` of ours to read: it is
                // working if it is drawing, it asks for what it needs in its
                // own pane rather than parking in our approval state, and it
                // counts no turns because nothing here counts them.
                if let Some(claude) = &slot.claude {
                    let working = claude.working();
                    return Row {
                        id: slot.id,
                        name: app.session_name().to_string(),
                        model: SessionKind::Claude.label().to_string(),
                        status: match claude.exited() {
                            Some(_) => "exited",
                            None if working => "working",
                            None => "ready",
                        },
                        blocked: false,
                        busy: working,
                        turns: 0,
                        focused: i == self.current,
                        cwd: app
                            .sandbox
                            .as_ref()
                            .map(|s| s.root().display().to_string())
                            .unwrap_or_default(),
                        activity: claude.activity(ACTIVITY_LINES),
                    };
                }
                let blocked = Self::is_blocked(app);
                Row {
                    id: slot.id,
                    name: app.session_name().to_string(),
                    model: app.model.clone(),
                    status: match app.status {
                        Status::Idle => "ready",
                        Status::Waiting => "thinking",
                        Status::Streaming => "streaming",
                        Status::Running => "running",
                        Status::Compacting => "compacting",
                        Status::AwaitingApproval(_) => "needs you",
                        Status::AwaitingChoice(_) => "asking you",
                        Status::AwaitingExecute { .. } => "needs you",
                        Status::AwaitingUndo { .. } => "needs you",
                    },
                    blocked,
                    busy: app.is_busy() && !blocked,
                    turns: app.turn_number,
                    focused: i == self.current,
                    cwd: app
                        .sandbox
                        .as_ref()
                        .map(|s| s.root().display().to_string())
                        .unwrap_or_default(),
                    activity: app.activity(ACTIVITY_LINES),
                }
            })
            .collect()
    }

    /// Rows for the pane beside the transcript: the user's sessions, in slot
    /// order. Children are left out for the reason they are left out of the
    /// view — nobody opened them and nobody switches to them.
    pub fn pane_rows(&self) -> Vec<Row> {
        self.rows()
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !self.is_child(*i))
            .map(|(_, row)| row)
            .collect()
    }

    pub fn sidebar(&self) -> bool {
        self.sidebar
    }

    pub fn pane_cols(&self) -> Option<u16> {
        self.pane_cols
    }

    pub fn pane_dragging(&self) -> bool {
        self.pane_grab.is_some()
    }

    /// Take hold of the divider, `offset` columns right of the pane's border.
    pub fn grab_divider(&mut self, offset: u16) {
        self.pane_grab = Some(offset);
    }

    pub fn release_divider(&mut self) {
        self.pane_grab = None;
    }

    /// Drag to `column`: put the part of the divider that was grabbed under the
    /// pointer. The pane is everything left of its own border and that border,
    /// so its width is one more than the column that border lands on.
    ///
    /// Unclamped: the floor and the ceiling depend on how wide the terminal is,
    /// which is known where this is laid out and not here. Storing the raw
    /// drag also means widening the terminal gives back a width an earlier,
    /// narrower one had clamped away.
    pub fn drag_divider(&mut self, column: u16) {
        let offset = self.pane_grab.unwrap_or(0);
        self.pane_cols = Some(column.saturating_sub(offset).saturating_add(1));
    }

    /// Switch view modes. Turning the pane on closes the full-screen view: they
    /// are two answers to one question, so leaving the screen up would make the
    /// key look dead from the one place you are most likely to press it.
    pub fn toggle_sidebar(&mut self) {
        self.sidebar = !self.sidebar;
        if self.sidebar {
            self.close_view();
        }
    }

    // --- the view ---

    pub fn view(&self) -> Option<&View> {
        self.view.as_ref()
    }

    pub fn view_open(&self) -> bool {
        self.view.is_some()
    }

    /// Open the view on the session you are in, so the list starts where you are.
    ///
    /// The query starts empty, so the highlight's position in the matches is its
    /// position in the slots.
    pub fn open_view(&mut self) {
        self.view = Some(View {
            selected: self.current,
            ..View::default()
        });
    }

    pub fn close_view(&mut self) {
        self.view = None;
    }

    /// Slot indices matching what has been typed, in view order.
    ///
    /// Derived per call for the reason [`Sessions::rows`] is: the sessions and
    /// their states change underneath it constantly.
    pub fn view_matches(&self) -> Vec<usize> {
        let Some(view) = &self.view else {
            return Vec::new();
        };
        let terms: Vec<String> = view
            .query
            .text()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        // Children are filtered here rather than in `rows`, which is indexed by
        // these very numbers: one filter hides them from the whole view, and
        // row `i` still means slot `i` everywhere else.
        (0..self.slots.len())
            .filter(|&i| !self.is_child(i) && self.matches(i, &terms))
            .collect()
    }

    /// Whether every one of `terms` appears somewhere in the row for session
    /// `i`. `terms` are expected lowercase.
    ///
    /// Wider than the `/load` picker's rule, which matches on name and model
    /// only, and deliberately so: every session here is named
    /// `session-<timestamp>`, so the name is the least distinguishing thing
    /// about it. What tells two apart is what they are *doing*, which is why the
    /// activity lines are on the row — and so they are what you can search.
    fn matches(&self, i: usize, terms: &[String]) -> bool {
        let app = &self.slots[i].app;
        let mut haystack = format!("{} {}", app.session_name(), app.model).to_lowercase();
        if let Some(sandbox) = &app.sandbox {
            haystack.push(' ');
            haystack.push_str(&sandbox.root().display().to_string().to_lowercase());
        }
        for line in app.activity(ACTIVITY_LINES) {
            haystack.push(' ');
            haystack.push_str(&line.to_lowercase());
        }
        terms.iter().all(|term| haystack.contains(term.as_str()))
    }

    /// The rows the view actually shows: [`Sessions::rows`] narrowed to the
    /// matches.
    pub fn view_rows(&self) -> Vec<Row> {
        let rows = self.rows();
        self.view_matches()
            .into_iter()
            .filter_map(|i| rows.get(i).cloned())
            .collect()
    }

    /// The highlight, clamped to what is actually offered. The matches can shrink
    /// under a stationary highlight when a background session's status changes.
    pub fn view_index(&self) -> usize {
        let count = self.view_matches().len();
        let selected = self.view.as_ref().map_or(0, |view| view.selected);
        if count == 0 {
            0
        } else {
            selected.min(count - 1)
        }
    }

    pub fn view_move(&mut self, delta: isize) {
        let last = self.view_matches().len().saturating_sub(1) as isize;
        let current = self.view_index() as isize;
        if let Some(view) = &mut self.view {
            view.selected = (current + delta).clamp(0, last) as usize;
        }
    }

    /// Focus a row directly, for mouse hover and clicks. `i` is a position in the
    /// filtered list, which is what the row map holds.
    pub fn view_select(&mut self, i: usize) -> bool {
        if i >= self.view_matches().len() {
            return false;
        }
        if let Some(view) = &mut self.view {
            view.selected = i;
            return true;
        }
        false
    }

    /// Start typing a filter, the way `/` starts one in a pager.
    pub fn view_search(&mut self, on: bool) {
        if let Some(view) = &mut self.view {
            view.searching = on;
        }
    }

    pub fn view_searching(&self) -> bool {
        self.view.as_ref().is_some_and(|view| view.searching)
    }

    /// Edit the query. Any edit resets the highlight to the top: the list under
    /// it has just changed, so holding the old position would land it somewhere
    /// unrelated.
    pub fn view_query_input(&mut self, edit: impl FnOnce(&mut Input)) {
        if let Some(view) = &mut self.view {
            edit(&mut view.query);
            view.selected = 0;
        }
    }

    /// Switch to the highlighted session and close the view.
    pub fn view_confirm(&mut self) {
        let Some(&slot) = self.view_matches().get(self.view_index()) else {
            return;
        };
        self.view = None;
        self.switch(slot);
    }

    /// Shut the highlighted session down, leaving the view open.
    pub fn view_close_selected(&mut self) {
        let selected = self.view_index();
        let Some(&slot) = self.view_matches().get(selected) else {
            return;
        };
        self.close(slot);
        // Clamp against the matches after the close, not before: the filter may
        // hide the session that took the closed one's place.
        let last = self.view_matches().len().saturating_sub(1);
        if let Some(view) = &mut self.view {
            view.selected = selected.min(last);
        }
    }

    // --- moving between sessions, and the two questions about the set ---

    /// Move the focus `delta` sessions along, in the order the pane lists them.
    ///
    /// Clamped rather than wrapping, like every other list in the harness.
    /// Subagents are skipped for the reason they are left out of the pane: you
    /// cannot switch to one, so stepping onto it would be a dead stop.
    pub fn step(&mut self, delta: isize) {
        let mine: Vec<usize> = (0..self.slots.len())
            .filter(|&i| !self.is_child(i))
            .collect();
        let Some(at) = mine.iter().position(|&i| i == self.current) else {
            return;
        };
        let last = mine.len().saturating_sub(1) as isize;
        let next = (at as isize + delta).clamp(0, last) as usize;
        self.switch(mine[next]);
    }

    /// Switch to the `n`th session in the pane's order, for a click on its row.
    /// Returns whether there was one there.
    ///
    /// By position rather than by slot index because that is what a click
    /// knows, and the pane's order leaves subagents out — the same mapping
    /// [`Sessions::view_select`] does for the view.
    pub fn pane_select(&mut self, n: usize) -> bool {
        let Some(&slot) = (0..self.slots.len())
            .filter(|&i| !self.is_child(i))
            .collect::<Vec<_>>()
            .get(n)
        else {
            return false;
        };
        self.switch(slot)
    }

    pub fn ask(&self) -> Option<&Ask> {
        self.ask.as_ref()
    }

    /// Ask for a name for a new session.
    pub fn ask_new(&mut self) {
        self.ask = Some(Ask::New {
            name: Input::default(),
            kind: SessionKind::default(),
        });
    }

    /// Switch which kind the new-session modal would create. Does nothing under
    /// the other question, which has no kind to switch.
    pub fn ask_toggle_kind(&mut self) {
        if let Some(Ask::New { kind, .. }) = &mut self.ask {
            *kind = kind.other();
        }
    }

    /// Ask whether to shut down the session the answer would land on: the one
    /// highlighted in the view while it is open, and the one you are in
    /// otherwise.
    ///
    /// Says no rather than asking when there is nothing to close, so the only
    /// session cannot be offered a modal whose yes does nothing.
    pub fn ask_close(&mut self) {
        if self.slots.len() == 1 {
            return self
                .app_mut()
                .push_notice("This is the only session; /quit exits the harness.");
        }
        let slot = if self.view.is_some() {
            match self.view_matches().get(self.view_index()) {
                Some(&slot) => slot,
                None => return,
            }
        } else {
            self.current
        };
        let name = self.slots[slot].app.session_name().to_string();
        self.ask = Some(Ask::Close { slot, name });
    }

    pub fn ask_cancel(&mut self) {
        self.ask = None;
    }

    /// Edit the name being typed. Does nothing under the other question, which
    /// takes a yes or a no and nothing else.
    pub fn ask_input(&mut self, edit: impl FnOnce(&mut Input)) {
        if let Some(Ask::New { name, .. }) = &mut self.ask {
            edit(name);
        }
    }

    /// Answer yes. The modal stays up only when a typed name was refused —
    /// there is something to fix, and the fix is what was already typed.
    pub fn ask_confirm(&mut self) {
        let Some(ask) = self.ask.take() else {
            return;
        };
        match ask {
            Ask::New { name, kind } => {
                let typed = name.text().trim().to_string();
                let started = if typed.is_empty() {
                    self.spawn_named(None, kind)
                } else {
                    self.spawn_named(Some(&typed), kind)
                };
                if started {
                    // A new session is one you want to type into, which is what
                    // `n` in the view has always done.
                    self.view = None;
                } else {
                    self.ask = Some(Ask::New { name, kind });
                }
            }
            Ask::Close { slot, .. } => {
                self.close(slot);
                // The highlight is a position in the matches and one of them has
                // just gone; clamp it the way closing from the view does.
                let last = self.view_matches().len().saturating_sub(1);
                if let Some(view) = &mut self.view {
                    view.selected = view.selected.min(last);
                }
            }
        }
    }

    /// Spawn from the view, which closes it: a new session is one you want to
    /// start typing into.
    pub fn view_spawn(&mut self) {
        self.spawn();
        self.view = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions(label: &str) -> (Sessions, std::path::PathBuf) {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-sessions-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let app = App::new("m".into(), None, 10, dir.clone());
        (Sessions::new(app), dir)
    }

    /// A subagent is a conversation but not a session. The view is the place
    /// that distinction is most visible: the user cannot switch to a child, so
    /// showing one would offer a row that does nothing.
    #[test]
    fn a_subagent_is_not_in_the_view() {
        let (mut sessions, dir) = sessions("hidden");
        let parent = sessions.current().id;
        sessions.open_agent(parent, "who owns naming?").unwrap();
        sessions.open_view();

        assert_eq!(sessions.view_matches(), vec![0], "only the real session");
        assert_eq!(sessions.view_rows().len(), 1);
        assert_eq!(sessions.len(), 1, "and it is not counted as one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spawning a subagent must not be able to refuse the user's next `Ctrl+N`
    /// with "already running 8 sessions" — for a conversation they never asked
    /// for and cannot see.
    #[test]
    fn a_subagent_does_not_count_toward_the_cap() {
        let (mut sessions, dir) = sessions("cap");
        let parent = sessions.current().id;
        for _ in 0..3 {
            sessions.open_agent(parent, "look into it").unwrap();
        }
        assert_eq!(sessions.len(), 1);

        for _ in 1..MAX_SESSIONS {
            sessions.spawn();
        }
        assert_eq!(
            sessions.len(),
            MAX_SESSIONS,
            "the user still gets every slot the cap promises"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The nastiest of the leaks: `open_names` feeds the on-disk open set, so a
    /// child here would be **reopened as a session** on the next launch.
    #[test]
    fn a_subagent_is_not_in_the_open_set() {
        let (mut sessions, dir) = sessions("openset");
        let parent = sessions.current().id;
        let child = sessions.open_agent(parent, "look into it").unwrap().0;
        let child_name = sessions
            .route(child)
            .expect("just opened")
            .app
            .session_name()
            .to_string();

        sessions.sync_open_set();
        let names = sessions.open_names();
        assert_eq!(names.len(), 1, "one session is open, not two");
        assert!(
            !names.contains(&child_name),
            "a subagent must never be recorded as a session to reopen"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Closing a child drops it rather than saving it: the conversation is the
    /// very thing the parent was spared, and it cannot be reopened anyway.
    #[test]
    fn closing_a_subagent_leaves_nothing_behind() {
        let (mut sessions, dir) = sessions("closechild");
        let parent = sessions.current().id;
        let child = sessions.open_agent(parent, "look into it").unwrap().0;
        assert!(sessions.route(child).is_some());

        sessions.close_child(child);
        assert!(sessions.route(child).is_none(), "the slot is gone");
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions.current().id,
            parent,
            "and the focus never moved off the parent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A parent that has given up — `Esc`, or an error that ended the turn —
    /// must not be reported to. Pushing a result into an idle conversation would
    /// start a turn the user deliberately stopped, which is worse than losing
    /// the answer.
    #[test]
    fn a_subagent_whose_parent_gave_up_is_not_reported() {
        let (mut sessions, dir) = sessions("orphan");
        let parent = sessions.current().id;
        let child = sessions.open_agent(parent, "look into it").unwrap().0;

        // While the parent waits, the child is wanted.
        sessions.route(parent).unwrap().app.status = Status::Running;
        sessions.route(child).unwrap().app.status = Status::Idle;
        assert_eq!(
            sessions.settled_children(),
            vec![SettledChild {
                child,
                parent,
                wanted: true,
            }]
        );

        // Once it has stopped waiting, the same finished child is unwanted.
        sessions.route(parent).unwrap().app.status = Status::Idle;
        let settled = sessions.settled_children();
        assert_eq!(settled.len(), 1);
        assert!(
            !settled[0].wanted,
            "an idle parent must not be handed a result"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A child still working is not swept up by the retirement pass.
    #[test]
    fn a_subagent_still_working_is_not_retired() {
        let (mut sessions, dir) = sessions("working");
        let parent = sessions.current().id;
        let child = sessions.open_agent(parent, "look into it").unwrap().0;
        sessions.route(parent).unwrap().app.status = Status::Running;

        // `open_agent` sends the prompt, so the child is already waiting.
        assert!(sessions.route(child).unwrap().app.is_busy());
        assert!(
            sessions.settled_children().is_empty(),
            "a busy child is not finished"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `close_child` is for children only. Pointing it at a real session would
    /// drop a conversation without saving it.
    #[test]
    fn close_child_refuses_a_real_session() {
        let (mut sessions, dir) = sessions("closereal");
        sessions.spawn();
        let real = sessions.current().id;

        sessions.close_child(real);
        assert!(
            sessions.route(real).is_some(),
            "a session the user opened is not a child and is not dropped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawning_inherits_the_settings_of_the_session_it_came_from() {
        let (mut sessions, dir) = sessions("inherit");
        sessions.app_mut().auto_approve = true;
        sessions.app_mut().show_reasoning = false;
        sessions.app_mut().keep_checkpoints = Some(4);
        sessions.app_mut().model = "other/model".into();

        sessions.spawn();
        let fresh = sessions.app();
        assert!(
            fresh.auto_approve,
            "an unattended session spawns unattended"
        );
        assert!(!fresh.show_reasoning);
        assert_eq!(fresh.keep_checkpoints, Some(4));
        assert_eq!(fresh.model, "other/model");
        // But not the conversation.
        assert_eq!(fresh.turn_number, 0);
        assert_eq!(fresh.history.len(), 1, "the contract and nothing else");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `default_name` is the clock to the second, so without a uniquifier two
    /// sessions spawned together would overwrite each other's file.
    #[test]
    fn sessions_spawned_in_the_same_second_get_different_names() {
        let (mut sessions, dir) = sessions("names");
        sessions.spawn();
        sessions.spawn();
        let names: Vec<String> = sessions
            .rows()
            .into_iter()
            .map(|row| row.name)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(names.len(), 3, "three sessions, three names: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_already_on_disk_is_not_reused() {
        let (mut sessions, dir) = sessions("ondisk");
        // Claim the name the next spawn would otherwise take.
        let taken = crate::session::default_name();
        let session =
            crate::session::Session::new("m".into(), vec![], vec![], vec![], Default::default());
        crate::session::save(&dir, &taken, &session).unwrap();

        sessions.spawn();
        assert_ne!(
            sessions.app().session_name(),
            taken,
            "a spawn must not adopt a conversation already saved under that name"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cap_holds_and_says_so() {
        let (mut sessions, dir) = sessions("cap");
        for _ in 1..MAX_SESSIONS {
            sessions.spawn();
        }
        assert_eq!(sessions.len(), MAX_SESSIONS);
        sessions.spawn();
        assert_eq!(sessions.len(), MAX_SESSIONS, "the cap holds");
        assert!(
            matches!(
                sessions.app().transcript.last(),
                Some(crate::app::Entry::Notice(n)) if n.contains("most at once")
            ),
            "and it says why nothing happened"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ids outlive their positions, which is what keeps a reply in flight from
    /// landing in a session that merely shuffled into the closed one's index.
    #[test]
    fn ids_are_stable_when_a_session_in_the_middle_closes() {
        let (mut sessions, dir) = sessions("ids");
        sessions.spawn();
        sessions.spawn();
        let before: Vec<u64> = sessions.rows().iter().map(|r| r.id).collect();
        assert_eq!(before, vec![1, 2, 3]);

        sessions.close(1);
        let after: Vec<u64> = sessions.rows().iter().map(|r| r.id).collect();
        assert_eq!(after, vec![1, 3], "the survivors keep their ids");
        assert!(sessions.route(2).is_none(), "the closed one routes nowhere");
        assert!(sessions.route(3).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_saves_the_conversation_so_it_can_be_loaded_back() {
        let (mut sessions, dir) = sessions("close-saves");
        sessions.spawn();
        let name = sessions.app().session_name().to_string();
        sessions
            .app_mut()
            .input
            .insert_str("something worth keeping");
        sessions.app_mut().submit().unwrap();
        sessions.app_mut().push_response(
            "<ai-harness-response>noted</ai-harness-response>".into(),
            None,
        );

        sessions.close(1);
        assert_eq!(sessions.len(), 1);
        let loaded = crate::session::load(&dir, &name).expect("it was saved on the way out");
        assert!(
            loaded
                .transcript
                .iter()
                .any(|e| matches!(e, crate::app::Entry::User(t) if t == "something worth keeping")),
            "and the conversation really is in it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_session_cannot_be_closed() {
        let (mut sessions, dir) = sessions("last");
        sessions.close(0);
        assert_eq!(sessions.len(), 1, "there is always somewhere to type");
        assert!(matches!(
            sessions.app().transcript.last(),
            Some(crate::app::Entry::Notice(n)) if n.contains("only session")
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_the_focused_session_leaves_you_next_to_where_you_were() {
        let (mut sessions, dir) = sessions("focus");
        sessions.spawn();
        sessions.spawn();
        assert_eq!(sessions.current, 2, "spawning focuses the new one");

        sessions.close(2);
        assert_eq!(sessions.current, 1, "back to the neighbour, not the start");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blocked_session_is_distinguished_from_a_busy_one() {
        let (mut sessions, dir) = sessions("blocked");
        sessions.spawn();
        sessions.app_mut().input.insert_str("do something");
        sessions.app_mut().submit().unwrap();
        assert!(sessions.any_busy(), "waiting on the model counts as busy");
        assert_eq!(sessions.blocked(), 0, "but not as wanting a person");

        sessions
            .app_mut()
            .push_response("<ai-harness-shell>ls</ai-harness-shell>".into(), None);
        assert_eq!(sessions.blocked(), 1, "an approval wants a person");
        let rows = sessions.rows();
        assert_eq!(rows[1].status, "needs you");
        assert!(rows[1].blocked && !rows[1].busy);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_catalog_reaches_every_session_including_later_ones() {
        let (mut sessions, dir) = sessions("catalog");
        sessions.spawn();
        sessions.set_catalog(Ok(vec![crate::openrouter::ModelInfo {
            id: "a/b".into(),
            name: "A B".into(),
            context_length: None,
            pricing: None,
        }]));
        for slot in sessions.iter() {
            assert_eq!(slot.app.catalog.models().len(), 1);
        }

        // And a session opened afterwards gets it without a second fetch.
        sessions.spawn();
        assert_eq!(sessions.app().catalog.models().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_view_opens_on_the_session_you_are_in() {
        let (mut sessions, dir) = sessions("view");
        sessions.spawn();
        sessions.spawn();
        sessions.switch(1);

        sessions.open_view();
        assert_eq!(sessions.view().unwrap().selected, 1);
        sessions.view_move(-1);
        sessions.view_confirm();
        assert_eq!(sessions.current, 0, "Enter switches to the highlight");
        assert!(!sessions.view_open(), "and closes the view");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Type a query into an open view.
    fn type_query(sessions: &mut Sessions, text: &str) {
        sessions.view_search(true);
        for c in text.chars() {
            sessions.view_query_input(|input| input.insert_char(c));
        }
    }

    #[test]
    fn the_view_narrows_to_the_query_and_switches_to_the_match() {
        let (mut sessions, dir) = sessions("search");
        sessions.spawn();
        sessions.spawn();
        sessions.iter_mut().nth(1).unwrap().app.model = "beta/two".into();

        sessions.open_view();
        type_query(&mut sessions, "beta");
        let rows = sessions.view_rows();
        assert_eq!(rows.len(), 1, "one session runs beta/two: {rows:?}");

        // The highlight is a position in the matches, so confirming takes the
        // session the row names rather than the slot at that ordinal.
        sessions.view_confirm();
        assert_eq!(sessions.current, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The names are all `session-<timestamp>`, so what tells two sessions apart
    /// is what they are doing — which is why the activity lines are searchable.
    #[test]
    fn the_query_reaches_what_a_session_is_doing() {
        let (mut sessions, dir) = sessions("activity");
        sessions.spawn();
        sessions
            .app_mut()
            .transcript
            .push(crate::app::Entry::User("refactor the parser".into()));

        sessions.open_view();
        type_query(&mut sessions, "parser");
        assert_eq!(sessions.view_matches(), vec![1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_query_that_matches_nothing_leaves_nothing_to_confirm() {
        let (mut sessions, dir) = sessions("nomatch");
        sessions.spawn();
        sessions.open_view();
        type_query(&mut sessions, "nothing-matches-this");

        assert!(sessions.view_rows().is_empty());
        assert_eq!(sessions.view_index(), 0, "clamped, not out of range");
        sessions.view_confirm();
        assert!(sessions.view_open(), "Enter on nothing does nothing");
        sessions.view_close_selected();
        assert_eq!(sessions.len(), 2, "and neither does x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Leaving the search returns the keyboard to the list without undoing the
    /// narrowing — you filtered in order to walk what was left.
    #[test]
    fn leaving_the_search_keeps_the_filter() {
        let (mut sessions, dir) = sessions("keepfilter");
        sessions.spawn();
        sessions.iter_mut().nth(1).unwrap().app.model = "beta/two".into();

        sessions.open_view();
        type_query(&mut sessions, "beta");
        sessions.view_search(false);
        assert!(!sessions.view_searching());
        assert_eq!(sessions.view_matches(), vec![1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The highlight indexes the matches, so `x` must close the session the row
    /// names — not the slot that happens to sit at that ordinal.
    #[test]
    fn closing_from_a_filtered_view_closes_the_session_on_the_row() {
        let (mut sessions, dir) = sessions("filteredclose");
        sessions.spawn();
        sessions.spawn();
        let doomed = sessions.rows()[2].id;
        sessions.iter_mut().nth(2).unwrap().app.model = "beta/two".into();

        sessions.open_view();
        type_query(&mut sessions, "beta");
        sessions.view_close_selected();

        assert_eq!(sessions.len(), 2);
        assert!(
            sessions.rows().iter().all(|row| row.id != doomed),
            "the filtered row's session is the one that went"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- reopening the set that was open ---

    /// Put a loadable session on disk under `name`.
    fn saved(dir: &Path, name: &str) {
        let session = crate::session::Session::new(
            format!("{name}/model"),
            vec![crate::openrouter::Message::system("contract")],
            Vec::new(),
            Vec::new(),
            crate::ledger::Ledger::default(),
        );
        crate::session::save(dir, name, &session).unwrap();
    }

    fn names_of(sessions: &Sessions) -> Vec<String> {
        sessions.open_names()
    }

    /// A Claude session has no folder to find, so the record alone brings it
    /// back — in its place in the order, and waiting for `attach` to start it.
    #[test]
    fn restore_reopens_a_claude_session_from_the_record() {
        let (_, dir) = sessions("restore-claude");
        saved(&dir, "alpha");
        let record = ClaudeRecord {
            id: "some-id".into(),
            cwd: dir.clone(),
        };
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: dir.clone(),
                current: 1,
                names: vec!["alpha".into(), "inner".into()],
                claude: [("inner".to_string(), record.clone())].into(),
            },
        )
        .unwrap();

        let template = App::new("m".into(), None, 10, dir.clone());
        let restored = Sessions::restore(template, &dir, &dir);

        assert_eq!(names_of(&restored), vec!["alpha", "inner"]);
        assert_eq!(restored.current, 1);
        assert_eq!(restored.current().claude_record, Some(record));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Resuming a conversation claude does not have strands the session on an
    /// error, so only a transcript with a user message in it counts.
    #[test]
    fn only_a_transcript_with_a_user_message_is_a_conversation() {
        let (_, dir) = sessions("claude-history");
        let project = dir.join("-some-project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("empty.jsonl"), r#"{"type":"ai-title"}"#).unwrap();
        std::fs::write(project.join("real.jsonl"), r#"{"type":"user"}"#).unwrap();

        assert!(has_conversation(&dir, "real"));
        assert!(!has_conversation(&dir, "empty"));
        assert!(!has_conversation(&dir, "absent"));
        assert!(!has_conversation(&dir.join("nowhere"), "real"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_is_started_under_an_id_and_resumed_by_it() {
        let (mut sessions, dir) = sessions("claude-args");
        assert_eq!(sessions.claude_args("x", false), ["--session-id", "x"]);
        assert_eq!(sessions.claude_args("x", true), ["--resume", "x"]);
        sessions.claude_permission_mode = Some("plan".into());
        assert_eq!(
            sessions.claude_args("x", true),
            ["--resume", "x", "--permission-mode", "plan"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--session-id` refuses anything that is not a UUID.
    #[test]
    fn a_new_claude_id_is_a_v4_uuid() {
        let id = new_claude_id().unwrap();
        let lengths: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(lengths, [8, 4, 4, 4, 12], "{id}");
        assert_eq!(&id[14..15], "4", "{id}");
        assert!("89ab".contains(&id[19..20]), "{id}");
        assert_ne!(id, new_claude_id().unwrap());
    }

    /// The template carries this launch's flags, so a restored session has to
    /// take them — and then its own saved model on top.
    #[test]
    fn restore_reopens_the_recorded_set_in_order_with_focus_where_it_was() {
        let (_, dir) = sessions("restore");
        saved(&dir, "alpha");
        saved(&dir, "beta");
        saved(&dir, "gamma");
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: dir.clone(),
                current: 1,
                names: vec!["alpha".into(), "beta".into(), "gamma".into()],
                claude: Default::default(),
            },
        )
        .unwrap();

        let mut template = App::new("m".into(), None, 10, dir.clone());
        template.auto_approve = true;
        let restored = Sessions::restore(template, &dir, &dir);

        assert_eq!(names_of(&restored), vec!["alpha", "beta", "gamma"]);
        assert_eq!(restored.current, 1, "focus where it was left");
        assert!(
            !restored.view_open(),
            "the pane is on, so resuming lands in the conversation"
        );
        assert_eq!(restored.app().model, "beta/model", "its own saved model");
        assert!(
            restored.iter().all(|slot| slot.app.auto_approve),
            "and this launch's flags"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_names_a_session_that_is_no_longer_saved() {
        let (_, dir) = sessions("restore-missing");
        saved(&dir, "alpha");
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: dir.clone(),
                current: 0,
                names: vec!["alpha".into(), "deleted-since".into()],
                claude: Default::default(),
            },
        )
        .unwrap();

        let restored = Sessions::restore(App::new("m".into(), None, 10, dir.clone()), &dir, &dir);
        assert_eq!(names_of(&restored), vec!["alpha"]);
        assert!(
            transcript_says(&restored, "deleted-since"),
            "a session that is gone should be named, not passed over"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Stepping moves the focus and stops at the ends; it does not wrap, and it
    /// does not step onto a subagent.
    #[test]
    fn stepping_moves_the_focus_and_skips_subagents() {
        let (mut sessions, dir) = sessions("step");
        sessions.spawn();
        sessions.spawn();
        assert_eq!(sessions.current, 2, "spawning focuses what it opened");
        let parent = sessions.current().id;
        sessions.open_agent(parent, "a task of its own").unwrap();

        sessions.step(1);
        assert_eq!(sessions.current, 2, "the subagent is not somewhere to go");
        sessions.step(-1);
        assert_eq!(sessions.current, 1);
        sessions.step(-1);
        assert_eq!(sessions.current, 0);
        sessions.step(-1);
        assert_eq!(sessions.current, 0, "and it stops rather than wrapping");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session can host a program instead of a conversation: it is in the
    /// list, it says what it is, and the record carries what reopens it —
    /// because nothing writes a folder for it.
    #[tokio::test]
    async fn a_hosted_session_is_listed_and_recorded_with_its_conversation() {
        let (mut sessions, dir) = sessions("hosted");
        let (tx, _rx) = mpsc::channel(8);
        sessions.attach(tx, None);

        // `sh` stands in for `claude`: what is being tested is the slot, not
        // which program is on the pty.
        assert!(sessions.spawn_hosted_for_test("spike", "/bin/sh", &["-c", "sleep 5"]));
        assert_eq!(sessions.len(), 2);
        assert!(sessions.current().claude.is_some(), "it hosts a program");

        let row = sessions
            .rows()
            .into_iter()
            .find(|row| row.name == "spike")
            .expect("it is in the list");
        assert_eq!(row.model, "claude code", "and says what it is");
        assert_eq!(row.turns, 0, "nothing here counts its turns");
        assert!(!row.blocked, "it asks in its own pane, not ours");

        sessions.sync_open_set();
        let record = crate::session::read_open(&dir).expect("recorded");
        assert!(record.names.contains(&"spike".to_string()));
        assert_eq!(record.claude["spike"].id, "test-id");

        // Closing it takes the child with it.
        let slot = sessions.current;
        sessions.close(slot);
        assert_eq!(sessions.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tab in the new-session modal switches what will be on the other side.
    #[test]
    fn the_new_session_modal_switches_agent() {
        let (mut sessions, dir) = sessions("kind-toggle");
        sessions.ask_new();
        assert!(
            matches!(sessions.ask(), Some(Ask::New { kind, .. }) if *kind == SessionKind::Harness),
            "the harness's own is the default"
        );
        sessions.ask_toggle_kind();
        assert!(
            matches!(sessions.ask(), Some(Ask::New { kind, .. }) if *kind == SessionKind::Claude)
        );
        sessions.ask_toggle_kind();
        assert!(
            matches!(sessions.ask(), Some(Ask::New { kind, .. }) if *kind == SessionKind::Harness),
            "and back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both of the divider's columns grab it, and the split follows the pointer
    /// from wherever on the seam it was taken hold of rather than snapping.
    #[test]
    fn either_column_of_the_divider_grabs_it_without_snapping() {
        let (mut sessions, dir) = sessions("grab");
        // A pane 30 wide puts its border on column 29 and the transcript's on
        // column 30.
        sessions.grab_divider(0);
        sessions.drag_divider(29);
        assert_eq!(sessions.pane_cols(), Some(30), "held where it already was");

        sessions.release_divider();
        assert!(!sessions.pane_dragging());

        // The right-hand column: pressed at 30, which is one right of the
        // pane's border, so the pane stays 30 wide until the pointer moves.
        sessions.grab_divider(1);
        sessions.drag_divider(30);
        assert_eq!(sessions.pane_cols(), Some(30), "no jump on taking hold");
        sessions.drag_divider(40);
        assert_eq!(sessions.pane_cols(), Some(40), "and then it follows");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A click lands on the session whose row it was, counting past subagents
    /// the pane does not show.
    #[test]
    fn a_click_in_the_pane_switches_to_that_session() {
        let (mut sessions, dir) = sessions("pane-click");
        sessions.spawn();
        sessions.spawn();
        // A subagent of the middle session: a slot, but not a row.
        let parent = sessions.rows()[1].id;
        sessions.open_agent(parent, "a task of its own").unwrap();

        let names = sessions.pane_rows();
        assert_eq!(names.len(), 3, "the subagent is not a row");
        assert!(sessions.pane_select(0));
        assert_eq!(sessions.app().session_name(), names[0].name);
        assert!(sessions.pane_select(2));
        assert_eq!(sessions.app().session_name(), names[2].name);
        assert!(
            !sessions.pane_select(3),
            "and nothing is below the last row"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The name is typed, and a name that cannot be a directory is refused
    /// without throwing away what was typed.
    #[test]
    fn naming_a_new_session_keeps_the_modal_up_on_a_bad_name() {
        let (mut sessions, dir) = sessions("ask-new");
        sessions.ask_new();
        sessions.ask_input(|name| name.insert_str("../escape"));
        sessions.ask_confirm();
        assert_eq!(sessions.len(), 1, "no session for a name with a traversal");
        assert!(
            matches!(sessions.ask(), Some(Ask::New { name, .. }) if name.text() == "../escape"),
            "and the modal stays up with the name to fix"
        );

        sessions.ask_input(|name| {
            name.clear();
            name.insert_str("spike");
        });
        sessions.ask_confirm();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions.app().session_name(), "spike", "named and focused");
        assert!(sessions.ask().is_none(), "and the modal is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The question is asked about the session the answer will land on, and
    /// answering it closes that one.
    #[test]
    fn the_shutdown_question_names_the_session_it_would_close() {
        let (mut sessions, dir) = sessions("ask-close");
        sessions.ask_close();
        assert!(sessions.ask().is_none(), "the only session is not offered");

        sessions.spawn();
        let doomed = sessions.app().session_name().to_string();
        sessions.ask_close();
        assert!(
            matches!(sessions.ask(), Some(Ask::Close { name, .. }) if *name == doomed),
            "the one you are in, when the view is closed"
        );
        sessions.ask_confirm();
        assert_eq!(sessions.len(), 1);
        assert!(!sessions.open_names().contains(&doomed));

        // And the highlighted one while the view is up, which need not be it.
        sessions.spawn();
        sessions.open_view();
        sessions.view_move(-1);
        let highlighted = sessions.view_rows()[sessions.view_index()].name.clone();
        sessions.ask_close();
        assert!(matches!(sessions.ask(), Some(Ask::Close { name, .. }) if *name == highlighted));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two view modes are two answers to one question, so turning the pane
    /// on takes the screen that was answering it.
    #[test]
    fn turning_the_pane_on_closes_the_full_screen_view() {
        let (mut sessions, dir) = sessions("pane-toggle");
        assert!(sessions.sidebar(), "the pane is the default");

        sessions.toggle_sidebar();
        assert!(!sessions.sidebar());
        sessions.open_view();
        assert!(sessions.view_open());

        sessions.toggle_sidebar();
        assert!(sessions.sidebar());
        assert!(
            !sessions.view_open(),
            "the pane answers it now, so the screen goes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--sessions-dir` can point two projects at one directory, and the set is
    /// a fact about a project.
    #[test]
    fn restore_declines_a_record_from_another_project() {
        let (_, dir) = sessions("restore-elsewhere");
        saved(&dir, "alpha");
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: PathBuf::from("/some/other/project"),
                current: 0,
                names: vec!["alpha".into()],
                claude: Default::default(),
            },
        )
        .unwrap();

        let restored = Sessions::restore(App::new("m".into(), None, 10, dir.clone()), &dir, &dir);
        assert_eq!(restored.len(), 1);
        assert_ne!(names_of(&restored), vec!["alpha".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_with_nothing_to_resume_starts_the_one_fresh_session() {
        let (_, dir) = sessions("restore-empty");
        // No record at all.
        let restored = Sessions::restore(App::new("m".into(), None, 10, dir.clone()), &dir, &dir);
        assert_eq!(restored.len(), 1);
        assert!(
            !restored.view_open(),
            "one fresh session is a prompt to type at, not a list to choose from"
        );

        // A record whose sessions have all been deleted.
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: dir.clone(),
                current: 0,
                names: vec!["gone".into()],
                claude: Default::default(),
            },
        )
        .unwrap();
        let restored = Sessions::restore(App::new("m".into(), None, 10, dir.clone()), &dir, &dir);
        assert_eq!(restored.len(), 1);
        assert!(transcript_says(&restored, "gone"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_holds_the_cap() {
        let (_, dir) = sessions("restore-cap");
        let names: Vec<String> = (0..MAX_SESSIONS + 3).map(|i| format!("s{i}")).collect();
        for name in &names {
            saved(&dir, name);
        }
        crate::session::write_open(
            &dir,
            &OpenSet {
                version: crate::session::VERSION,
                root: dir.clone(),
                current: 0,
                names,
                claude: Default::default(),
            },
        )
        .unwrap();

        let restored = Sessions::restore(App::new("m".into(), None, 10, dir.clone()), &dir, &dir);
        assert_eq!(restored.len(), MAX_SESSIONS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Derived and compared rather than hooked onto every mutation, so this is
    /// what checks the comparison actually notices.
    #[test]
    fn the_record_is_written_when_the_set_changes_and_not_otherwise() {
        let (mut sessions, dir) = sessions("record");
        sessions.sync_open_set();
        let first = crate::session::read_open(&dir).expect("recorded");
        assert_eq!(first.names.len(), 1);
        assert_eq!(first.root, dir);

        // Nothing changed, so nothing is written — the deleted file stays gone.
        std::fs::remove_file(dir.join(crate::session::OPEN_FILE)).unwrap();
        sessions.sync_open_set();
        assert!(crate::session::read_open(&dir).is_none());

        sessions.spawn();
        sessions.sync_open_set();
        assert_eq!(crate::session::read_open(&dir).unwrap().names.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_a_session_drops_it_from_the_record() {
        let (mut sessions, dir) = sessions("record-close");
        sessions.spawn();
        let doomed = sessions.app().session_name().to_string();
        sessions.sync_open_set();
        assert!(
            crate::session::read_open(&dir)
                .unwrap()
                .names
                .contains(&doomed)
        );

        sessions.close(1);
        sessions.sync_open_set();
        let record = crate::session::read_open(&dir).unwrap();
        assert!(
            !record.names.contains(&doomed),
            "a session you shut down should not come back: {record:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- going to a session, opening it if it is not running ---

    #[test]
    fn revealing_a_saved_session_adds_it_rather_than_replacing_anything() {
        let (mut sessions, dir) = sessions("reveal-saved");
        saved(&dir, "archived");
        let already = sessions.app().session_name().to_string();

        sessions.reveal("archived".into());
        assert_eq!(sessions.len(), 2, "the running session is untouched");
        assert_eq!(sessions.app().session_name(), "archived");
        assert_eq!(sessions.app().model, "archived/model");
        assert!(sessions.slot_of(&already).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two slots on one session file auto-save over each other, so the second
    /// ask has to move you rather than duplicate it.
    #[test]
    fn revealing_a_running_session_switches_instead_of_opening_it_twice() {
        let (mut sessions, dir) = sessions("reveal-twice");
        saved(&dir, "archived");
        sessions.reveal("archived".into());
        let first = sessions.app().session_name().to_string();
        sessions.spawn();
        assert_ne!(sessions.app().session_name(), first);

        sessions.reveal("archived".into());
        assert_eq!(sessions.len(), 3, "no duplicate slot");
        assert_eq!(sessions.app().session_name(), "archived", "but you moved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Switching takes no new slot, so refusing to *move* at the cap would be
    /// refusing the wrong thing.
    #[test]
    fn the_cap_stops_opening_a_session_but_not_going_to_one() {
        let (mut sessions, dir) = sessions("reveal-cap");
        saved(&dir, "archived");
        sessions.reveal("archived".into());
        while sessions.len() < MAX_SESSIONS {
            sessions.spawn();
        }
        saved(&dir, "not-running");

        sessions.reveal("not-running".into());
        assert_eq!(sessions.len(), MAX_SESSIONS, "the cap holds");
        assert!(transcript_says(&sessions, "the most at once"));

        sessions.reveal("archived".into());
        assert_eq!(sessions.app().session_name(), "archived");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether the focused session's transcript mentions `needle`.
    fn transcript_says(sessions: &Sessions, needle: &str) -> bool {
        sessions.app().transcript.iter().any(|entry| match entry {
            crate::app::Entry::Notice(text) => text.contains(needle),
            _ => false,
        })
    }
}
