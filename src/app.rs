//! Application state: the transcript, the prompt buffer, and request status.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::command::{self, Command};
use crate::exec::{CommandOutput, WriteOutcome};
use crate::files::ReadOutcome;
use crate::input::Input;
use crate::openrouter::{Message, Usage};
use crate::protocol::{self, Action};
use crate::sandbox::Sandbox;

/// One rendered block in the transcript. Kept separate from [`Message`] so the
/// UI can show things (errors, notices) that are never sent to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Entry {
    User(String),
    /// A model reply that parsed cleanly into a protocol action.
    Action {
        action: Action,
        usage: Option<Usage>,
    },
    /// A reply that reached us but violated the protocol. `raw` is kept so the
    /// user can see exactly what the model said.
    Malformed {
        reason: String,
        raw: String,
    },
    /// The outcome of a command the user allowed.
    CommandResult(Box<CommandOutput>),
    /// The outcome of a file read. Unlike the others, no approval preceded it.
    ReadResult(ReadOutcome),
    /// The outcome of a file write the user allowed.
    WriteResult(WriteOutcome),
    /// A command the user refused.
    Denied(String),
    /// A raw protocol payload crossing the boundary. Always recorded; shown only
    /// in debug mode, so toggling `/debug` reveals earlier traffic too.
    Frame {
        direction: Direction,
        body: String,
    },
    Error(String),
    Notice(String),
}

/// Which way a protocol frame travelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Harness → model.
    Sent,
    /// Model → harness.
    Received,
}

impl Direction {
    pub fn arrow(self) -> &'static str {
        match self {
            Self::Sent => "→",
            Self::Received => "←",
        }
    }
}

/// Which button the approval modal has focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Allow,
    Deny,
}

impl Choice {
    pub fn toggled(self) -> Self {
        match self {
            Self::Allow => Self::Deny,
            Self::Deny => Self::Allow,
        }
    }
}

/// An action awaiting the user's decision. Holds the approvable action itself
/// (a shell command or a file write — never a terminal `Response`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub action: Action,
    pub selected: Choice,
}

/// The `/load` session picker overlay. A UI overlay, not a conversation status:
/// it coexists with `Status::Idle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// Saved session names, snapshotted when the picker opened.
    pub sessions: Vec<String>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    /// A request has been sent but no token has arrived yet.
    Waiting,
    /// Tokens are streaming in.
    Streaming,
    /// The approval modal is up for a proposed command.
    AwaitingApproval(Pending),
    /// An approved command is executing.
    Running,
}

pub struct App {
    pub input: Input,
    pub transcript: Vec<Entry>,
    /// Conversation as sent to the model. Diverges from `transcript`, which
    /// also holds local-only entries.
    pub history: Vec<Message>,
    pub status: Status,
    pub model: String,
    /// Rows scrolled down from the top of the rendered transcript.
    pub scroll: u16,
    /// When true, the view sticks to the bottom as new content arrives.
    pub follow: bool,
    pub should_quit: bool,
    /// Frame counter used to animate the waiting indicator.
    pub tick: usize,
    /// Model round-trips since the user last typed. Bounded so a model that
    /// keeps proposing commands cannot loop forever.
    pub iterations: usize,
    pub max_iterations: usize,
    /// When true, the transcript also shows raw protocol frames.
    pub debug: bool,
    /// Filesystem access for `<ai-harness-read>`. Set by `main` once the
    /// sandbox exists; `None` only in tests that never read a file, which is
    /// why this is a field rather than a constructor argument — the alternative
    /// is threading a macOS-only `Sandbox` through ninety-odd test call sites.
    pub sandbox: Option<Sandbox>,
    /// When true, a read waits for approval like a command or a write. Off by
    /// default: a read is non-mutating and confined to the working directory,
    /// and making context-gathering silent is the point of having the element.
    pub confirm_reads: bool,
    /// Consecutive malformed replies in the current streak; reset on success.
    pub retries: usize,
    pub max_retries: usize,
    /// History length before the current streak of malformed replies, so a
    /// give-up can roll the whole failed exchange back out of context.
    retry_anchor: Option<usize>,
    /// Highlighted entry in the completion menu. Clamped on read rather than on
    /// write, since the list shrinks as the typed prefix narrows.
    completion_cursor: usize,
    /// The reply text accumulated so far while streaming. Display-only — the
    /// authoritative text arrives with the final completion, so this is cleared
    /// before the reply is committed.
    pub streaming: Option<String>,
    /// Identifies the current in-flight task. Bumped whenever a task is spawned,
    /// and again on cancel, so updates from an abandoned task can be dropped.
    generation: u64,
    /// Prompts the user has submitted this session, oldest first, for Up/Down
    /// recall. Kept separate from `history` (which is the model conversation)
    /// and preserved across `/clear`.
    prompt_history: Vec<String>,
    /// While browsing `prompt_history`, the entry currently shown; `None` when
    /// editing a fresh line.
    history_index: Option<usize>,
    /// The `/load` session picker, when open.
    picker: Option<Picker>,
    /// Directory where `/save` and `/load` read and write session files.
    sessions_dir: PathBuf,
    /// The session name auto-save writes to. Updated by save/load/rename/fork.
    current_session: String,
    /// Fingerprint of the last persisted state, so auto-save skips redundant
    /// writes. `(history.len(), transcript.len())` — both only grow within a
    /// session, so a change here means something was appended.
    last_saved: (usize, usize),
    /// Set when an auto-save write fails, so the error is reported once rather
    /// than on every turn; cleared on the next successful save.
    autosave_failed: bool,
}

/// Default cap on consecutive corrective retries after a malformed reply.
pub const DEFAULT_MAX_RETRIES: usize = 3;

impl App {
    /// `extra_system` is optional operator guidance appended to the protocol
    /// contract; the contract itself is always sent.
    ///
    /// `debug` and `max_retries` are plain fields rather than constructor
    /// arguments: both are adjustable at runtime, `debug` via `/debug`.
    pub fn new(
        model: String,
        extra_system: Option<String>,
        max_iterations: usize,
        sessions_dir: PathBuf,
    ) -> Self {
        let history = vec![Message::system(protocol::system_prompt(
            extra_system.as_deref(),
        ))];
        Self {
            input: Input::default(),
            transcript: Vec::new(),
            history,
            status: Status::Idle,
            model,
            scroll: 0,
            follow: true,
            should_quit: false,
            tick: 0,
            iterations: 0,
            max_iterations,
            debug: false,
            sandbox: None,
            confirm_reads: false,
            retries: 0,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_anchor: None,
            completion_cursor: 0,
            streaming: None,
            generation: 0,
            sessions_dir,
            current_session: crate::session::default_name(),
            last_saved: (0, 0),
            autosave_failed: false,
            prompt_history: Vec::new(),
            history_index: None,
            picker: None,
        }
    }

    /// Recall the previous prompt (`Up`).
    ///
    /// From a fresh, empty line this loads the most recent prompt; while already
    /// browsing it steps further back in time. Editing a recalled entry ends
    /// navigation, so a stray `Up` never clobbers a half-typed message.
    pub fn recall_prev(&mut self) {
        match self.history_index {
            None => {
                if !self.input.is_blank() || self.prompt_history.is_empty() {
                    return;
                }
                let i = self.prompt_history.len() - 1;
                self.load_history_entry(i);
            }
            Some(i) => {
                if self.edited_since_recall(i) {
                    self.history_index = None;
                    return;
                }
                if i > 0 {
                    self.load_history_entry(i - 1);
                }
                // Already at the oldest entry: stay put.
            }
        }
    }

    /// Recall the next (more recent) prompt (`Down`).
    ///
    /// Only does anything while browsing. Stepping past the newest entry returns
    /// to an empty prompt.
    pub fn recall_next(&mut self) {
        let Some(i) = self.history_index else {
            return;
        };
        if self.edited_since_recall(i) {
            self.history_index = None;
            return;
        }
        if i + 1 < self.prompt_history.len() {
            self.load_history_entry(i + 1);
        } else {
            // Past the newest: back to a fresh, empty line.
            self.history_index = None;
            self.input.clear();
        }
    }

    /// True if the buffer no longer matches the entry we loaded — i.e. the user
    /// edited it, so navigation should end.
    fn edited_since_recall(&self, i: usize) -> bool {
        self.prompt_history.get(i).map(String::as_str) != Some(self.input.text())
    }

    fn load_history_entry(&mut self, i: usize) {
        self.history_index = Some(i);
        self.input.clear();
        self.input.insert_str(&self.prompt_history[i].clone());
    }

    /// Bump the generation and return the new value. Every spawned task is
    /// tagged with the generation current when it started, so a later cancel can
    /// invalidate its still-queued updates.
    pub fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// The generation identifying the current in-flight task.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Interrupt the in-flight turn: invalidate its updates, drop the live
    /// stream view, and return control to the user. The caller is responsible
    /// for signalling the task to stop its actual work.
    pub fn cancel(&mut self) {
        if !self.is_busy() {
            return;
        }
        // A fresh generation means any update still queued from the abandoned
        // task will be recognised as stale and dropped.
        self.next_generation();
        self.finish_stream();
        self.status = Status::Idle;
        self.push_notice("Cancelled.");
    }

    /// Append a streamed token chunk to the live reply view.
    pub fn push_delta(&mut self, delta: &str) {
        self.status = Status::Streaming;
        self.streaming
            .get_or_insert_with(String::new)
            .push_str(delta);
        self.follow = true;
    }

    /// Discard the live streaming view. The full reply text is committed
    /// separately via [`App::push_response`].
    pub fn finish_stream(&mut self) {
        self.streaming = None;
    }

    /// Commands offered for the partially-typed name in the prompt.
    ///
    /// Derived from the input buffer rather than cached, so it can never drift
    /// out of step with what has been typed.
    pub fn completions(&self) -> Vec<&'static command::Spec> {
        if self.is_busy() {
            return Vec::new();
        }
        match command::completion_prefix(self.input.text()) {
            Some(prefix) => command::matching(prefix),
            None => Vec::new(),
        }
    }

    /// Index of the highlighted completion, clamped to what is actually offered.
    /// Clamping here means the stored cursor can never point past the list after
    /// the prefix narrows it.
    pub fn completion_index(&self) -> usize {
        let count = self.completions().len();
        if count == 0 {
            0
        } else {
            self.completion_cursor.min(count - 1)
        }
    }

    /// Move the highlight, wrapping in both directions.
    pub fn move_completion(&mut self, delta: isize) {
        let count = self.completions().len();
        if count == 0 {
            return;
        }
        let current = self.completion_index() as isize;
        let next = (current + delta).rem_euclid(count as isize);
        self.completion_cursor = next as usize;
    }

    /// Replace the typed name with the highlighted command. Returns false when
    /// there was nothing to complete.
    pub fn accept_completion(&mut self) -> bool {
        let Some(spec) = self.completions().get(self.completion_index()).copied() else {
            return false;
        };
        self.input.clear();
        self.input.insert_str(&format!("/{}", spec.name));
        self.completion_cursor = 0;
        true
    }

    /// Record a raw protocol payload. Kept regardless of debug mode so turning
    /// `/debug` on later still shows what already happened.
    fn frame(&mut self, direction: Direction, body: impl Into<String>) {
        self.transcript.push(Entry::Frame {
            direction,
            body: body.into(),
        });
    }

    /// Handle a locally-executed slash command. Nothing here reaches the model.
    pub fn run_command(&mut self, command: Command) {
        match command {
            Command::Debug => {
                self.debug = !self.debug;
                let state = if self.debug { "on" } else { "off" };
                self.push_notice(format!("Debug mode {state}."));
            }
            Command::Help => self.push_notice(crate::command::help_text()),
            Command::Clear => self.reset_conversation(),
            Command::Quit => self.should_quit = true,
            Command::Save(name) => self.save_session(name),
            Command::Load(name) => self.load_session_command(name),
            Command::Rename(name) => self.rename_session(name),
            Command::Fork(name) => self.fork_session(name),
            Command::Unknown(name) => self.push_notice(format!(
                "Unknown command /{name}. Type /help to see what is available."
            )),
        }
        self.follow = true;
    }

    /// Snapshot the current session for persistence. Pure.
    pub fn to_session(&self) -> crate::session::Session {
        crate::session::Session::new(
            self.model.clone(),
            self.history.clone(),
            self.transcript.clone(),
            self.prompt_history.clone(),
        )
    }

    /// Replace the in-memory session with a loaded one. Pure — does no I/O.
    pub fn apply_session(&mut self, session: crate::session::Session) {
        let model_mismatch = (session.model != self.model).then(|| session.model.clone());

        self.history = session.history;
        self.transcript = session.transcript;
        self.prompt_history = session.prompt_history;
        self.finish_stream();
        self.status = Status::Idle;
        self.scroll = 0;
        self.follow = true;
        self.history_index = None;

        // Pushed after the transcript is replaced, or it would be overwritten.
        if let Some(saved_model) = model_mismatch {
            self.push_notice(format!(
                "Loaded a session saved with model {saved_model}; new turns use {}.",
                self.model
            ));
        }
    }

    /// A cheap fingerprint of the persisted state, to skip redundant writes.
    fn fingerprint(&self) -> (usize, usize) {
        (self.history.len(), self.transcript.len())
    }

    /// Write the current conversation to `current_session`. Records the
    /// fingerprint so auto-save won't immediately repeat, and reports a write
    /// failure at most once per failure streak.
    fn persist_current(&mut self) {
        let session = self.to_session();
        match crate::session::save(&self.sessions_dir, &self.current_session, &session) {
            Ok(_) => {
                self.last_saved = self.fingerprint();
                self.autosave_failed = false;
            }
            Err(e) => {
                if !self.autosave_failed {
                    self.autosave_failed = true;
                    self.transcript
                        .push(Entry::Error(format!("auto-save failed: {e:#}")));
                }
            }
        }
    }

    /// Save after a completed turn. A no-op unless idle, past the empty startup
    /// state, and actually changed — so it neither writes mid-turn nor litters
    /// the directory before a conversation has begun.
    pub fn maybe_autosave(&mut self) {
        if self.status != Status::Idle
            || self.history.len() <= 1
            || self.fingerprint() == self.last_saved
        {
            return;
        }
        self.persist_current();
    }

    fn save_session(&mut self, name: Option<String>) {
        if let Some(name) = name {
            self.current_session = name;
        }
        let session = self.to_session();
        match crate::session::save(&self.sessions_dir, &self.current_session, &session) {
            Ok(path) => {
                self.last_saved = self.fingerprint();
                self.push_notice(format!("Saved session to {}", path.display()));
            }
            // A local command error, not a failed model turn — no history rollback.
            Err(e) => self.transcript.push(Entry::Error(format!("{e:#}"))),
        }
    }

    fn load_session_command(&mut self, name: Option<String>) {
        // No name: open the picker (or notice, if there is nothing to pick).
        let Some(name) = name else {
            self.open_load_picker();
            return;
        };
        self.load_named(name);
    }

    /// Load a session by name, replacing the conversation. Shared by `/load
    /// <name>` and the picker. A failed load leaves the current session intact.
    fn load_named(&mut self, name: String) {
        match crate::session::load(&self.sessions_dir, &name) {
            Ok(session) => {
                self.apply_session(session);
                // Auto-save now continues to the loaded session's file.
                self.current_session = name.clone();
                self.last_saved = self.fingerprint();
                self.push_notice(format!("Loaded session {name:?}."));
            }
            Err(e) => self.transcript.push(Entry::Error(format!("{e:#}"))),
        }
    }

    /// Open the session picker, or post a notice when nothing is saved yet.
    pub fn open_load_picker(&mut self) {
        let sessions = crate::session::list(&self.sessions_dir);
        if sessions.is_empty() {
            self.push_notice("No saved sessions. Use /save [name] to create one.");
        } else {
            self.picker = Some(Picker {
                sessions,
                selected: 0,
            });
        }
    }

    pub fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref()
    }

    /// Move the highlight, clamped to the list (no wrap).
    pub fn picker_move(&mut self, delta: isize) {
        if let Some(picker) = &mut self.picker {
            let last = picker.sessions.len().saturating_sub(1);
            let next = (picker.selected as isize + delta).clamp(0, last as isize);
            picker.selected = next as usize;
        }
    }

    /// Focus a row directly, for mouse hover/click. Returns whether `i` was a
    /// real row, so a click on empty space below the list does nothing.
    pub fn picker_select(&mut self, i: usize) -> bool {
        if let Some(picker) = &mut self.picker
            && i < picker.sessions.len()
        {
            picker.selected = i;
            return true;
        }
        false
    }

    /// Load the highlighted session and close the picker.
    pub fn picker_confirm(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        if let Some(name) = picker.sessions.into_iter().nth(picker.selected) {
            self.load_named(name);
        }
    }

    pub fn picker_cancel(&mut self) {
        self.picker = None;
    }

    fn rename_session(&mut self, name: Option<String>) {
        let Some(new) = name else {
            self.transcript
                .push(Entry::Error("usage: /rename <name>".into()));
            return;
        };
        match crate::session::rename(&self.sessions_dir, &self.current_session, &new) {
            Ok(_) => {
                self.current_session = new.clone();
                self.last_saved = self.fingerprint();
                self.push_notice(format!(
                    "Renamed session to {new:?} (load with /load {new})."
                ));
            }
            Err(e) => self.transcript.push(Entry::Error(format!("{e:#}"))),
        }
    }

    fn fork_session(&mut self, name: Option<String>) {
        let new = name.unwrap_or_else(crate::session::default_name);
        if new == self.current_session || crate::session::exists(&self.sessions_dir, &new) {
            self.transcript.push(Entry::Error(format!(
                "cannot fork to {new:?}: a session with that name already exists"
            )));
            return;
        }
        // Freeze the original at its current state, then continue under the new
        // name. The in-memory conversation is untouched — you keep talking.
        let original = self.current_session.clone();
        self.persist_current();
        self.current_session = new.clone();
        self.persist_current();
        self.push_notice(format!(
            "Forked to session {new:?}; original preserved as {original:?}."
        ));
    }

    /// Waiting specifically on the model. Prefer [`App::is_busy`] for gating
    /// input; this is the narrower check used by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_waiting(&self) -> bool {
        self.status == Status::Waiting
    }

    /// True while the harness is busy and the prompt should be inert.
    pub fn is_busy(&self) -> bool {
        !matches!(self.status, Status::Idle)
    }

    pub fn pending(&self) -> Option<&Pending> {
        match &self.status {
            Status::AwaitingApproval(pending) => Some(pending),
            _ => None,
        }
    }

    /// Consume the prompt buffer.
    ///
    /// A slash command is executed locally and never reaches the model, so this
    /// returns `None` for those. Otherwise it returns the messages to send.
    pub fn submit(&mut self) -> Option<Vec<Message>> {
        if self.is_busy() || self.input.is_blank() {
            return None;
        }
        match crate::command::parse(&self.input.take()) {
            crate::command::Input::Command(command) => {
                self.run_command(command);
                None
            }
            crate::command::Input::Prompt(text) => self.send_prompt(text),
        }
    }

    /// Send `text` as a user query, bypassing slash-command parsing.
    pub fn send_prompt(&mut self, text: String) -> Option<Vec<Message>> {
        if self.is_busy() || text.trim().is_empty() {
            return None;
        }
        // Record for Up/Down recall, de-duping an immediate repeat. Browsing
        // ends on submit.
        if self.prompt_history.last() != Some(&text) {
            self.prompt_history.push(text.clone());
        }
        self.history_index = None;

        // The transcript shows what the user typed; the model gets it wrapped.
        self.transcript.push(Entry::User(text.clone()));
        let encoded = protocol::encode_query(&text);
        self.frame(Direction::Sent, encoded.clone());
        self.history.push(Message::user(encoded));

        self.iterations = 0;
        self.retries = 0;
        self.retry_anchor = None;
        self.status = Status::Waiting;
        self.follow = true;
        Some(self.history.clone())
    }

    /// Record a model reply, validating it against the protocol.
    ///
    /// The raw text goes into history either way: the model did say it, and
    /// dropping it would leave two user turns adjacent.
    /// Returns messages to send immediately, which happens when a malformed
    /// reply earns a corrective retry.
    pub fn push_response(&mut self, content: String, usage: Option<Usage>) -> Option<Vec<Message>> {
        self.frame(Direction::Received, content.clone());
        self.follow = true;

        let action = match protocol::parse_reply(&content) {
            Ok(action) => action,
            Err(err) => return self.retry_after(content, err),
        };

        // Only a valid reply counts as progress against the loop budget.
        self.iterations += 1;
        self.retries = 0;
        self.retry_anchor = None;
        self.history.push(Message::assistant(content));
        self.transcript.push(Entry::Action {
            action: action.clone(),
            usage,
        });

        match action {
            // A final answer ends the turn.
            Action::Response(_) => self.status = Status::Idle,
            other => {
                if self.iterations >= self.max_iterations {
                    self.push_notice(format!(
                        "Stopped after {} model round-trips. Send another prompt to continue.",
                        self.iterations
                    ));
                    self.status = Status::Idle;
                } else if !self.confirm_reads
                    && let Action::Read { path } = &other
                {
                    // A read mutates nothing and cannot leave the working
                    // directory, so it runs now and the loop continues without
                    // interrupting the user.
                    let path = path.clone();
                    return Some(self.perform_read(&path));
                } else {
                    // Everything else waits for the user.
                    self.status = Status::AwaitingApproval(Pending {
                        action: other,
                        selected: Choice::Allow,
                    });
                }
            }
        }
        None
    }

    /// Read a file and hand the contents straight back to the model.
    ///
    /// Shared by the automatic path above and by the `--confirm-reads` approval
    /// path, so both behave identically. Runs synchronously: a read is capped at
    /// [`crate::files::MAX_READ_BYTES`] from local disk, which is far cheaper
    /// than the task-spawning and generation-tagging a background job would need.
    /// A failed read is reported to the model as a result, not raised as an
    /// error, so a bad path costs one round-trip instead of ending the turn.
    pub fn perform_read(&mut self, path: &str) -> Vec<Message> {
        let outcome = match &self.sandbox {
            Some(sandbox) => crate::files::read(sandbox, path),
            None => ReadOutcome::failed(path, "file access is not configured"),
        };
        let encoded = protocol::encode_read_result(&outcome);
        self.transcript.push(Entry::ReadResult(outcome));
        self.frame(Direction::Sent, encoded.clone());
        self.history.push(Message::user(encoded));
        self.status = Status::Waiting;
        self.follow = true;
        self.history.clone()
    }

    /// Ask the model to try again after a protocol violation, or give up.
    fn retry_after(
        &mut self,
        content: String,
        error: protocol::ProtocolError,
    ) -> Option<Vec<Message>> {
        // Remember where context was clean, before the first bad reply.
        if self.retry_anchor.is_none() {
            self.retry_anchor = Some(self.history.len());
        }
        self.retries += 1;
        self.transcript.push(Entry::Malformed {
            reason: error.to_string(),
            raw: content.clone(),
        });

        if self.retries > self.max_retries {
            // Roll the whole failed exchange out of context. Leaving the model's
            // own malformed output behind makes repeating it more likely, and
            // the transcript still shows what happened.
            if let Some(anchor) = self.retry_anchor.take() {
                self.history.truncate(anchor);
            }
            // Drop the trailing user turn too, matching `push_error`, so history
            // ends on an assistant turn rather than two adjacent user turns.
            if matches!(
                self.history.last(),
                Some(Message {
                    role: crate::openrouter::Role::User,
                    ..
                })
            ) {
                self.history.pop();
            }
            self.retries = 0;
            self.status = Status::Idle;
            self.transcript.push(Entry::Error(format!(
                "The model failed to follow the protocol after {} attempts. Giving up on this turn.",
                self.max_retries
            )));
            return None;
        }

        // Keep the bad reply plus a targeted correction, so the model can see
        // what it did and what was wrong with it.
        self.history.push(Message::assistant(content));
        let correction = protocol::encode_correction(&error);
        self.frame(Direction::Sent, correction.clone());
        self.history.push(Message::user(correction));
        self.push_notice(format!(
            "Reply did not follow the protocol; retrying ({}/{}).",
            self.retries, self.max_retries
        ));
        self.status = Status::Waiting;
        Some(self.history.clone())
    }

    /// Move the focused button in the approval modal.
    pub fn toggle_choice(&mut self) {
        if let Status::AwaitingApproval(pending) = &mut self.status {
            pending.selected = pending.selected.toggled();
        }
    }

    pub fn set_choice(&mut self, choice: Choice) {
        if let Status::AwaitingApproval(pending) = &mut self.status {
            pending.selected = choice;
        }
    }

    /// Accept the pending action; the caller runs it. Returns the action.
    pub fn approve(&mut self) -> Option<Action> {
        let Status::AwaitingApproval(pending) = &self.status else {
            return None;
        };
        let action = pending.action.clone();
        self.status = Status::Running;
        self.follow = true;
        Some(action)
    }

    /// Refuse the pending action and tell the model, so it can try something
    /// else rather than assuming it ran. Returns messages to send.
    pub fn deny(&mut self) -> Option<Vec<Message>> {
        let Status::AwaitingApproval(pending) = &self.status else {
            return None;
        };
        // Show what was refused: the command, or the write's path.
        let refused = match &pending.action {
            Action::Shell(command) => command.clone(),
            Action::Read { path } => format!("read {path}"),
            Action::Write { path, .. } => format!("write {path}"),
            Action::Response(_) => String::new(),
        };
        self.transcript.push(Entry::Denied(refused));
        let encoded = protocol::encode_denied();
        self.frame(Direction::Sent, encoded.clone());
        self.history.push(Message::user(encoded));
        self.status = Status::Waiting;
        self.follow = true;
        Some(self.history.clone())
    }

    /// Record a finished command and hand the result back to the model.
    pub fn push_command_result(&mut self, output: CommandOutput) -> Vec<Message> {
        let encoded = protocol::encode_shell_result(&output);
        self.transcript.push(Entry::CommandResult(Box::new(output)));
        self.frame(Direction::Sent, encoded.clone());
        self.history.push(Message::user(encoded));
        self.status = Status::Waiting;
        self.follow = true;
        self.history.clone()
    }

    /// Record a finished write and hand the result back to the model.
    pub fn push_write_result(&mut self, outcome: WriteOutcome) -> Vec<Message> {
        let encoded = protocol::encode_write_result(&outcome.path, outcome.as_result());
        self.transcript.push(Entry::WriteResult(outcome));
        self.frame(Direction::Sent, encoded.clone());
        self.history.push(Message::user(encoded));
        self.status = Status::Waiting;
        self.follow = true;
        self.history.clone()
    }

    pub fn push_error(&mut self, message: String) {
        // Drop the trailing user turn (a query, or a command result) so a retry
        // does not double-send it.
        if matches!(
            self.history.last(),
            Some(Message {
                role: crate::openrouter::Role::User,
                ..
            })
        ) {
            self.history.pop();
        }
        self.transcript.push(Entry::Error(message));
        self.status = Status::Idle;
        self.follow = true;
    }

    pub fn push_notice(&mut self, message: impl Into<String>) {
        self.transcript.push(Entry::Notice(message.into()));
        self.follow = true;
    }

    /// Clear the conversation, keeping any system prompt in place.
    pub fn reset_conversation(&mut self) {
        self.history
            .retain(|m| m.role == crate::openrouter::Role::System);
        self.transcript.clear();
        self.scroll = 0;
        self.follow = true;
        self.iterations = 0;
        self.retries = 0;
        self.retry_anchor = None;
        self.status = Status::Idle;
        self.push_notice("Conversation cleared.");
        // "Clear" clears the file too: overwrite the current session with the
        // cleared state, but only if it was already saved (don't create a file
        // for a conversation that never began). Use /fork to preserve instead.
        if crate::session::exists(&self.sessions_dir, &self.current_session) {
            self.persist_current();
        }
    }

    pub fn scroll_up(&mut self, rows: u16) {
        self.scroll = self.scroll.saturating_sub(rows);
        self.follow = false;
    }

    pub fn scroll_down(&mut self, rows: u16, max: u16) {
        self.scroll = (self.scroll + rows).min(max);
        if self.scroll >= max {
            self.follow = true;
        }
    }

    pub fn scroll_to_bottom(&mut self, max: u16) {
        self.scroll = max;
        self.follow = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openrouter::Role;

    #[test]
    fn submit_returns_none_when_blank() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        assert!(app.submit().is_none());
        app.input.insert_str("   \n  ");
        assert!(app.submit().is_none());
    }

    /// Index of the first non-system message; the protocol prompt always leads.
    const FIRST_TURN: usize = 1;

    /// Transcript with debug frames filtered out. Frames are interleaved with
    /// everything else, so tests about visible content must ignore them.
    pub(super) fn visible(app: &App) -> Vec<&Entry> {
        app.transcript
            .iter()
            .filter(|e| !matches!(e, Entry::Frame { .. }))
            .collect()
    }

    pub(super) fn last_visible(app: &App) -> &Entry {
        visible(app).pop().expect("transcript should not be empty")
    }

    #[test]
    fn submit_wraps_the_prompt_in_the_query_tag() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi there\n");
        let sent = app.submit().expect("should submit");

        assert_eq!(sent[FIRST_TURN].role, Role::User);
        assert_eq!(
            sent[FIRST_TURN].content,
            "<ai-harness-query>hi there</ai-harness-query>"
        );
        assert!(app.is_waiting());
        assert!(app.input.is_blank());
    }

    #[test]
    fn transcript_shows_the_raw_prompt_not_the_wrapped_one() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi there");
        app.submit().unwrap();
        assert!(matches!(visible(&app).as_slice(), [Entry::User(t)] if t == "hi there"));
    }

    #[test]
    fn submit_is_ignored_while_waiting() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("one");
        app.submit().unwrap();
        app.input.insert_str("two");
        assert!(app.submit().is_none());
        assert_eq!(app.history.len(), FIRST_TURN + 1);
    }

    #[test]
    fn protocol_system_prompt_always_leads_the_history() {
        let app = App::new("m".into(), None, 10, std::env::temp_dir());
        assert_eq!(app.history[0].role, Role::System);
        assert!(app.history[0].content.contains("ai-harness-shell"));
        assert!(app.history[0].content.contains("ai-harness-response"));
    }

    #[test]
    fn operator_guidance_is_appended_to_the_protocol_prompt() {
        let app = App::new(
            "m".into(),
            Some("be terse".into()),
            10,
            std::env::temp_dir(),
        );
        assert_eq!(app.history.len(), 1);
        assert!(app.history[0].content.contains("be terse"));
        assert!(
            app.history[0].content.contains("ai-harness-shell"),
            "operator guidance must not replace the protocol contract"
        );
    }

    #[test]
    fn valid_shell_reply_becomes_an_action_entry() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("list files");
        app.submit().unwrap();
        app.push_response("<ai-harness-shell>ls -la</ai-harness-shell>".into(), None);

        assert!(!app.is_waiting());
        match app.transcript.last() {
            Some(Entry::Action { action, .. }) => {
                assert_eq!(*action, Action::Shell("ls -la".into()))
            }
            other => panic!("expected an action entry, got {other:?}"),
        }
    }

    #[test]
    fn valid_response_reply_becomes_an_action_entry() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response(
            "<ai-harness-response>Hello there.</ai-harness-response>".into(),
            None,
        );
        match app.transcript.last() {
            Some(Entry::Action { action, .. }) => {
                assert_eq!(*action, Action::Response("Hello there.".into()))
            }
            other => panic!("expected an action entry, got {other:?}"),
        }
    }

    #[test]
    fn malformed_reply_is_flagged_and_keeps_the_raw_text() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("Sure, I'll help!".into(), None);

        let flagged = visible(&app)
            .into_iter()
            .find(|e| matches!(e, Entry::Malformed { .. }))
            .expect("a malformed entry should be recorded");
        match flagged {
            Entry::Malformed { raw, reason } => {
                assert_eq!(raw, "Sure, I'll help!");
                assert!(!reason.is_empty(), "the reason must say what went wrong");
            }
            other => panic!("expected a malformed entry, got {other:?}"),
        }
    }

    #[test]
    fn malformed_reply_triggers_a_corrective_retry() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        let resend = app
            .push_response("Sure, I'll help!".into(), None)
            .expect("a malformed reply should be retried");

        assert_eq!(app.status, Status::Waiting, "a retry keeps us waiting");
        assert_eq!(app.retries, 1);

        // The bad reply plus a correction telling the model what was wrong.
        assert_eq!(resend[resend.len() - 2].role, Role::Assistant);
        assert_eq!(resend[resend.len() - 2].content, "Sure, I'll help!");
        let correction = resend.last().unwrap();
        assert_eq!(correction.role, Role::User);
        assert!(
            correction.content.contains("rejected"),
            "the correction should name the failure: {}",
            correction.content
        );
        assert!(correction.content.contains("ai-harness-shell"));
    }

    #[test]
    fn malformed_replies_do_not_consume_the_agentic_budget() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("garbage".into(), None);
        assert_eq!(app.iterations, 0, "a rejected reply is not progress");

        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);
        assert_eq!(app.iterations, 1);
    }

    #[test]
    fn a_valid_reply_resets_the_retry_streak() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("garbage".into(), None);
        assert_eq!(app.retries, 1);

        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);
        assert_eq!(app.retries, 0, "success should clear the streak");
        assert_eq!(app.status, Status::Idle);
    }

    #[test]
    fn retries_give_up_and_roll_the_failed_exchange_out_of_history() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.max_retries = 3;
        let clean = app.history.len();
        app.input.insert_str("hi");
        app.submit().unwrap();

        // Three retries are offered, then the fourth failure gives up.
        for attempt in 1..=3 {
            assert!(
                app.push_response("garbage".into(), None).is_some(),
                "attempt {attempt} should be retried"
            );
        }
        assert!(
            app.push_response("garbage".into(), None).is_none(),
            "the cap should stop the retries"
        );

        assert_eq!(app.status, Status::Idle, "control returns to the user");
        assert_eq!(
            app.history.len(),
            clean,
            "the failed exchange should be rolled out of context"
        );
        assert!(matches!(last_visible(&app), Entry::Error(_)));
    }

    #[test]
    fn response_appends_to_history_and_clears_waiting() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);
        assert!(!app.is_waiting());
        assert_eq!(app.history.len(), FIRST_TURN + 2);
        assert_eq!(app.history[FIRST_TURN + 1].role, Role::Assistant);
    }

    #[test]
    fn error_rolls_back_the_failed_user_turn() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_error("boom".into());
        assert!(!app.is_waiting());
        assert_eq!(
            app.history.len(),
            FIRST_TURN,
            "failed turn should not linger in history"
        );
        assert!(matches!(last_visible(&app), Entry::Error(_)));
    }

    #[test]
    fn reset_keeps_the_system_prompt() {
        let mut app = App::new(
            "m".into(),
            Some("be terse".into()),
            10,
            std::env::temp_dir(),
        );
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);
        app.reset_conversation();
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history[0].role, Role::System);
        assert!(app.history[0].content.contains("ai-harness-shell"));
    }

    fn output(exit: i32) -> CommandOutput {
        CommandOutput {
            command: "ls".into(),
            exit_code: Some(exit),
            stdout: "a\nb".into(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            cancelled: false,
        }
    }

    /// Drive an app to a pending shell approval.
    fn awaiting(command: &str) -> App {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("do the thing");
        app.submit().unwrap();
        app.push_response(
            format!("<ai-harness-shell>{command}</ai-harness-shell>"),
            None,
        );
        app
    }

    #[test]
    fn a_write_action_requires_approval() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("make a file");
        app.submit().unwrap();
        app.push_response(
            "<ai-harness-write file=out.txt>hello\n</ai-harness-write>".into(),
            None,
        );
        match app.pending() {
            Some(pending) => assert_eq!(
                pending.action,
                Action::Write {
                    path: "out.txt".into(),
                    contents: "hello\n".into(),
                }
            ),
            None => panic!("a write must await approval, got {:?}", app.status),
        }
    }

    #[test]
    fn approving_a_write_returns_the_write_action() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("x");
        app.submit().unwrap();
        app.push_response("<ai-harness-write file=a>b</ai-harness-write>".into(), None);
        assert_eq!(
            app.approve(),
            Some(Action::Write {
                path: "a".into(),
                contents: "b".into()
            })
        );
        assert_eq!(app.status, Status::Running);
    }

    #[test]
    fn a_write_result_feeds_back_and_resumes_waiting() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("x");
        app.submit().unwrap();
        app.push_response("<ai-harness-write file=a>b</ai-harness-write>".into(), None);
        app.approve();

        let sent = app.push_write_result(crate::exec::WriteOutcome {
            path: "a".into(),
            bytes: 1,
            error: None,
            timed_out: false,
            cancelled: false,
        });
        assert_eq!(app.status, Status::Waiting);
        assert!(matches!(last_visible(&app), Entry::WriteResult(_)));
        let last = sent.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert!(last.content.contains("ai-harness-write-result"));
        assert!(last.content.contains("wrote 1 bytes to a"));
    }

    #[test]
    fn denying_a_write_shows_the_path() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("x");
        app.submit().unwrap();
        app.push_response(
            "<ai-harness-write file=secret.txt>oops</ai-harness-write>".into(),
            None,
        );
        app.deny().unwrap();
        assert!(matches!(last_visible(&app), Entry::Denied(d) if d.contains("secret.txt")));
    }

    #[test]
    fn a_shell_action_requires_approval_before_running() {
        let app = awaiting("ls -la");
        match app.pending() {
            Some(pending) => {
                assert_eq!(pending.action, Action::Shell("ls -la".into()));
                assert_eq!(pending.selected, Choice::Allow, "Allow should be focused");
            }
            None => panic!("shell action must await approval, got {:?}", app.status),
        }
    }

    #[test]
    fn a_final_response_does_not_ask_for_approval() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response(
            "<ai-harness-response>done</ai-harness-response>".into(),
            None,
        );
        assert_eq!(app.status, Status::Idle);
        assert!(app.pending().is_none());
    }

    #[test]
    fn approving_moves_to_running_and_returns_the_action() {
        let mut app = awaiting("echo hi");
        assert_eq!(app.approve(), Some(Action::Shell("echo hi".into())));
        assert_eq!(app.status, Status::Running);
        // Approving twice must not run it again.
        assert_eq!(app.approve(), None);
    }

    #[test]
    fn denying_tells_the_model_it_did_not_run() {
        let mut app = awaiting("rm -rf /");
        let sent = app.deny().expect("deny should resend");
        assert_eq!(app.status, Status::Waiting);
        assert!(matches!(last_visible(&app), Entry::Denied(c) if c == "rm -rf /"));

        let last = sent.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert!(
            last.content.contains("denied"),
            "the model must learn it was refused: {}",
            last.content
        );
    }

    #[test]
    fn command_result_goes_back_to_the_model_and_resumes_waiting() {
        let mut app = awaiting("ls");
        app.approve();
        let sent = app.push_command_result(output(0));

        assert_eq!(app.status, Status::Waiting);
        assert!(matches!(last_visible(&app), Entry::CommandResult(_)));
        let last = sent.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert!(last.content.contains("ai-harness-shell-result"));
        assert!(last.content.contains("exit code: 0"));
    }

    #[test]
    fn toggling_moves_the_focused_button() {
        let mut app = awaiting("ls");
        assert_eq!(app.pending().unwrap().selected, Choice::Allow);
        app.toggle_choice();
        assert_eq!(app.pending().unwrap().selected, Choice::Deny);
        app.toggle_choice();
        assert_eq!(app.pending().unwrap().selected, Choice::Allow);
        app.set_choice(Choice::Deny);
        assert_eq!(app.pending().unwrap().selected, Choice::Deny);
    }

    #[test]
    fn choice_helpers_are_inert_without_a_pending_command() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.toggle_choice();
        app.set_choice(Choice::Deny);
        assert!(app.deny().is_none());
        assert!(app.approve().is_none());
        assert_eq!(app.status, Status::Idle);
    }

    #[test]
    fn the_loop_stops_at_the_iteration_cap() {
        let mut app = App::new("m".into(), None, 3, std::env::temp_dir());
        app.input.insert_str("go");
        app.submit().unwrap();

        // Each round-trip proposes another command; the cap must end it.
        for _ in 0..3 {
            app.push_response("<ai-harness-shell>ls</ai-harness-shell>".into(), None);
            if app.status == Status::Idle {
                break;
            }
            app.approve();
            app.push_command_result(output(0));
        }

        assert_eq!(
            app.status,
            Status::Idle,
            "cap should return control to the user"
        );
        assert!(
            matches!(last_visible(&app), Entry::Notice(n) if n.contains("Stopped")),
            "the user should be told why it stopped: {:?}",
            last_visible(&app)
        );
    }

    #[test]
    fn a_new_prompt_resets_the_iteration_count() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("first");
        app.submit().unwrap();
        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);
        assert_eq!(app.iterations, 1);

        app.input.insert_str("second");
        app.submit().unwrap();
        assert_eq!(app.iterations, 0, "a fresh prompt starts a fresh budget");
    }

    #[test]
    fn submit_is_ignored_while_awaiting_approval() {
        let mut app = awaiting("ls");
        app.input.insert_str("something else");
        assert!(
            app.submit().is_none(),
            "the prompt must be inert while the modal is up"
        );
    }

    #[test]
    fn error_after_a_command_result_rolls_back_that_result() {
        let mut app = awaiting("ls");
        app.approve();
        app.push_command_result(output(0));
        let before = app.history.len();
        app.push_error("network died".into());
        assert_eq!(
            app.history.len(),
            before - 1,
            "the result turn should roll back"
        );
        assert_eq!(app.status, Status::Idle);
    }

    fn names(app: &App) -> Vec<&'static str> {
        app.completions().iter().map(|s| s.name).collect()
    }

    #[test]
    fn next_generation_yields_distinct_increasing_values() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        let a = app.next_generation();
        let b = app.next_generation();
        assert!(b > a);
        assert_eq!(app.generation(), b);
    }

    #[test]
    fn a_new_prompt_does_not_reuse_the_previous_generation() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        let first = app.next_generation(); // as a spawn would
        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);

        app.input.insert_str("again");
        app.submit().unwrap();
        let second = app.next_generation();
        assert!(second > first, "generations must not repeat across turns");
    }

    #[test]
    fn cancel_returns_to_idle_and_invalidates_the_generation() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        let before = app.next_generation(); // a stream task would carry this
        app.push_delta("half a rep");
        assert_eq!(app.status, Status::Streaming);

        app.cancel();

        assert_eq!(app.status, Status::Idle);
        assert!(app.streaming.is_none(), "the live view must be discarded");
        assert_ne!(
            app.generation(),
            before,
            "a task carrying the old generation is now stale"
        );
        assert!(matches!(last_visible(&app), Entry::Notice(n) if n.contains("Cancelled")));
    }

    #[test]
    fn cancel_is_a_no_op_when_idle() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        let before = app.generation();
        app.cancel();
        assert_eq!(app.status, Status::Idle);
        assert_eq!(
            app.generation(),
            before,
            "nothing in flight, nothing to invalidate"
        );
        assert!(app.transcript.is_empty(), "no spurious notice");
    }

    #[test]
    fn cancel_frees_the_prompt_again() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_delta("partial");
        assert!(app.is_busy());

        app.cancel();
        assert!(!app.is_busy());
        app.input.insert_str("new prompt");
        assert!(
            app.submit().is_some(),
            "the prompt should work after cancel"
        );
    }

    /// Submit a prompt, completing the turn so the app is idle again.
    fn submit_prompt(app: &mut App, text: &str) {
        app.input.insert_str(text);
        app.submit().unwrap();
        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);
    }

    #[test]
    fn submitting_records_the_prompt_for_recall() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        submit_prompt(&mut app, "first");
        submit_prompt(&mut app, "second");
        assert_eq!(app.prompt_history, vec!["first", "second"]);
    }

    #[test]
    fn slash_commands_are_not_recorded() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        submit_prompt(&mut app, "a real prompt");
        app.input.insert_str("/debug");
        app.submit();
        assert_eq!(app.prompt_history, vec!["a real prompt"]);
    }

    #[test]
    fn consecutive_duplicate_prompts_are_deduped() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        submit_prompt(&mut app, "same");
        submit_prompt(&mut app, "same");
        submit_prompt(&mut app, "other");
        submit_prompt(&mut app, "same");
        // Adjacent repeat collapses; the later non-adjacent repeat is kept.
        assert_eq!(app.prompt_history, vec!["same", "other", "same"]);
    }

    #[test]
    fn up_on_an_empty_prompt_walks_back_in_time() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        submit_prompt(&mut app, "one");
        submit_prompt(&mut app, "two");
        submit_prompt(&mut app, "three");

        app.recall_prev();
        assert_eq!(app.input.text(), "three", "Up starts at the most recent");
        app.recall_prev();
        assert_eq!(app.input.text(), "two");
        app.recall_prev();
        assert_eq!(app.input.text(), "one");
        app.recall_prev();
        assert_eq!(app.input.text(), "one", "at the oldest, Up stays put");
    }

    #[test]
    fn down_walks_forward_and_past_newest_clears() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        submit_prompt(&mut app, "one");
        submit_prompt(&mut app, "two");

        app.recall_prev(); // two
        app.recall_prev(); // one
        app.recall_next();
        assert_eq!(app.input.text(), "two");
        app.recall_next();
        assert_eq!(app.input.text(), "", "past the newest returns to empty");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn down_without_browsing_does_nothing() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        submit_prompt(&mut app, "one");
        app.recall_next();
        assert_eq!(app.input.text(), "");
    }

    #[test]
    fn up_on_a_non_empty_prompt_does_not_recall() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        submit_prompt(&mut app, "one");
        app.input.insert_str("half typed");
        app.recall_prev();
        assert_eq!(
            app.input.text(),
            "half typed",
            "a stray Up must not clobber a draft"
        );
    }

    #[test]
    fn up_with_no_history_is_a_no_op() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.recall_prev();
        assert_eq!(app.input.text(), "");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn editing_a_recalled_entry_detaches_from_navigation() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        submit_prompt(&mut app, "one");
        submit_prompt(&mut app, "two");

        app.recall_prev(); // "two"
        app.input.insert_str(" edited");
        assert_eq!(app.input.text(), "two edited");

        // Further Up detaches rather than jumping elsewhere.
        app.recall_prev();
        assert_eq!(app.input.text(), "two edited");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn submitting_a_recalled_entry_resets_navigation() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        submit_prompt(&mut app, "one");
        submit_prompt(&mut app, "two");

        app.recall_prev(); // "two"
        assert!(app.history_index.is_some());
        app.submit().unwrap();
        assert!(app.history_index.is_none(), "submit ends browsing");
    }

    #[test]
    fn clear_preserves_prompt_history() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        submit_prompt(&mut app, "remember me");
        app.reset_conversation();
        assert_eq!(
            app.prompt_history,
            vec!["remember me"],
            "input history is session state, not conversation state"
        );
        // And it is still recallable afterwards.
        app.recall_prev();
        assert_eq!(app.input.text(), "remember me");
    }

    fn session_temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-app-session-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn app_in(dir: &std::path::Path) -> App {
        App::new("test/model".into(), None, 10, dir.to_path_buf())
    }

    #[test]
    fn save_then_load_restores_the_session() {
        let dir = session_temp_dir("roundtrip");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "first question"); // leaves a full user+response turn
        let history_len = app.history.len();
        let transcript_len = app.transcript.len();

        // Idle after a terminal response, so the command runs.
        app.input.insert_str("/save demo");
        app.submit();
        assert!(matches!(last_visible(&app), Entry::Notice(n) if n.contains("Saved")));

        // Load into a fresh app — a faithful "restart" that does not touch the
        // saved file (unlike /clear, which now clears it).
        let mut fresh = app_in(&dir);
        fresh.input.insert_str("/load demo");
        fresh.submit();

        assert_eq!(fresh.history.len(), history_len, "conversation restored");
        assert_eq!(
            fresh.transcript.len(),
            transcript_len + 1,
            "+1 for the load notice"
        );
        assert_eq!(
            fresh.prompt_history,
            vec!["first question"],
            "recall restored"
        );
        assert_eq!(fresh.status, Status::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_lets_the_conversation_continue() {
        let dir = session_temp_dir("continue");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "hello");
        app.input.insert_str("/save s");
        app.submit();

        let mut fresh = app_in(&dir);
        fresh.input.insert_str("/load s");
        fresh.submit();

        // A fresh prompt appends to the restored history rather than starting over.
        fresh.input.insert_str("follow up");
        let sent = fresh.submit().expect("should send");
        assert!(sent.len() > 2, "new turn builds on the loaded history");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_a_missing_session_is_a_non_destructive_error() {
        let dir = session_temp_dir("missing");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "keep me");
        let history_before = app.history.clone();

        app.input.insert_str("/load nonexistent");
        app.submit();

        assert!(matches!(last_visible(&app), Entry::Error(_)));
        assert_eq!(app.history, history_before, "a failed load changes nothing");
    }

    #[test]
    fn load_with_no_name_opens_a_picker() {
        let dir = session_temp_dir("picker-open");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "x");
        app.input.insert_str("/save alpha");
        app.submit();
        app.input.insert_str("/fork beta"); // now two saved sessions
        app.submit();

        app.input.insert_str("/load");
        app.submit();

        let picker = app.picker().expect("picker should open");
        assert_eq!(picker.sessions, vec!["alpha", "beta"]);
        assert_eq!(picker.selected, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_with_no_sessions_shows_a_notice_not_a_picker() {
        let dir = session_temp_dir("picker-empty");
        let mut app = app_in(&dir);
        app.open_load_picker();
        assert!(app.picker().is_none(), "no modal when nothing is saved");
        assert!(matches!(last_visible(&app), Entry::Notice(n) if n.contains("No saved sessions")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picker_move_clamps_at_both_ends() {
        let dir = session_temp_dir("picker-move");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "x");
        for name in ["a", "b", "c"] {
            app.input.insert_str(&format!("/fork {name}"));
            app.submit();
        }
        app.open_load_picker();
        let n = app.picker().unwrap().sessions.len();
        assert!(n >= 3);

        app.picker_move(-1);
        assert_eq!(app.picker().unwrap().selected, 0, "clamped at the top");
        app.picker_move(100);
        assert_eq!(
            app.picker().unwrap().selected,
            n - 1,
            "clamped at the bottom"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picker_confirm_loads_the_selected_session_and_closes() {
        let dir = session_temp_dir("picker-confirm");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "first prompt");
        app.input.insert_str("/save wanted");
        app.submit();
        // Diverge into another session so "wanted" is a distinct load target.
        app.input.insert_str("/fork other");
        app.submit();
        submit_prompt(&mut app, "second prompt");

        app.open_load_picker();
        // Select "other"? No — pick "wanted" and confirm.
        let idx = app
            .picker()
            .unwrap()
            .sessions
            .iter()
            .position(|s| s == "wanted")
            .unwrap();
        app.picker_select(idx);
        app.picker_confirm();

        assert!(app.picker().is_none(), "picker closes after loading");
        assert_eq!(app.current_session, "wanted", "auto-save follows the load");
        assert!(matches!(last_visible(&app), Entry::Notice(n) if n.contains("wanted")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picker_select_reports_out_of_range() {
        let dir = session_temp_dir("picker-select");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "x");
        app.input.insert_str("/save one");
        app.submit();
        app.open_load_picker();
        assert!(app.picker_select(0));
        assert!(
            !app.picker_select(99),
            "an out-of-range row is not selected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picker_cancel_closes_without_changing_the_conversation() {
        let dir = session_temp_dir("picker-cancel");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "keep me");
        app.input.insert_str("/save s");
        app.submit();
        let history_before = app.history.clone();

        app.open_load_picker();
        app.picker_cancel();
        assert!(app.picker().is_none());
        assert_eq!(app.history, history_before, "cancel changes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_session_notes_a_model_mismatch() {
        let dir = session_temp_dir("mismatch");
        let mut app = app_in(&dir); // model = "test/model"
        let session = crate::session::Session::new(
            "other/model".into(),
            vec![Message::system("x")],
            vec![Entry::User("q".into())],
            vec![],
        );
        app.apply_session(session);
        assert!(
            matches!(last_visible(&app), Entry::Notice(n) if n.contains("other/model")),
            "a model mismatch should be surfaced"
        );
    }

    /// Number of `.json` files in a directory.
    fn session_files(dir: &std::path::Path) -> Vec<String> {
        crate::session::list(dir)
    }

    #[test]
    fn autosave_writes_after_a_turn_and_skips_when_unchanged() {
        let dir = session_temp_dir("autosave");
        let mut app = app_in(&dir);

        // Nothing saved before a turn happens.
        app.maybe_autosave();
        assert!(
            session_files(&dir).is_empty(),
            "no file before the first turn"
        );

        submit_prompt(&mut app, "hello");
        app.maybe_autosave();
        let files = session_files(&dir);
        assert_eq!(
            files.len(),
            1,
            "a turn produced one session file: {files:?}"
        );
        assert!(files[0].starts_with("session-"), "default per-launch name");

        // A second call with no change must not rewrite.
        let before = std::fs::metadata(dir.join(format!("{files0}.json", files0 = files[0])))
            .unwrap()
            .modified()
            .unwrap();
        app.maybe_autosave();
        let after = std::fs::metadata(dir.join(format!("{files0}.json", files0 = files[0])))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "unchanged state must not be rewritten");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn autosave_is_skipped_while_busy() {
        let dir = session_temp_dir("busy");
        let mut app = app_in(&dir);
        app.input.insert_str("mid turn");
        app.submit().unwrap(); // now Waiting
        app.maybe_autosave();
        assert!(
            session_files(&dir).is_empty(),
            "a turn in flight must not be saved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_overwrites_the_session_file_to_the_cleared_state() {
        let dir = session_temp_dir("clear-file");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "some content");
        app.maybe_autosave();
        let name = session_files(&dir)[0].clone();

        app.reset_conversation();

        // The file still exists but now holds the cleared conversation.
        let reloaded = crate::session::load(&dir, &name).unwrap();
        assert_eq!(
            reloaded.history.len(),
            1,
            "clear wiped the saved conversation too"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fork_preserves_the_original_and_continues_in_a_new_session() {
        let dir = session_temp_dir("fork");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "shared history");
        app.maybe_autosave();
        let original = session_files(&dir)[0].clone();
        let original_before =
            std::fs::read_to_string(dir.join(format!("{original}.json"))).unwrap();

        app.input.insert_str("/fork branch");
        app.submit();

        // Both files exist; the original is byte-for-byte unchanged.
        assert!(crate::session::exists(&dir, "branch"));
        let original_after = std::fs::read_to_string(dir.join(format!("{original}.json"))).unwrap();
        assert_eq!(
            original_before, original_after,
            "the original must be frozen"
        );

        // The conversation continues; a new turn updates only the branch.
        submit_prompt(&mut app, "only in the branch");
        app.maybe_autosave();
        let branch = crate::session::load(&dir, "branch").unwrap();
        assert!(
            branch
                .transcript
                .iter()
                .any(|e| matches!(e, Entry::User(t) if t == "only in the branch")),
            "the new turn landed in the branch"
        );
        let frozen = crate::session::load(&dir, &original).unwrap();
        assert!(
            !frozen
                .transcript
                .iter()
                .any(|e| matches!(e, Entry::User(t) if t == "only in the branch")),
            "the original did not receive the post-fork turn"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fork_refuses_an_existing_name() {
        let dir = session_temp_dir("fork-clobber");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "x");
        app.input.insert_str("/save taken");
        app.submit();
        app.input.insert_str("/fork taken");
        app.submit();
        assert!(matches!(last_visible(&app), Entry::Error(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_moves_the_file_and_switches_the_current_session() {
        let dir = session_temp_dir("rename");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "content");
        app.maybe_autosave();
        let original = session_files(&dir)[0].clone();

        app.input.insert_str("/rename my-project");
        app.submit();

        assert!(matches!(last_visible(&app), Entry::Notice(n) if n.contains("my-project")));
        assert!(!crate::session::exists(&dir, &original), "old name is gone");
        assert!(
            crate::session::exists(&dir, "my-project"),
            "new name present"
        );

        // Auto-save now targets the new name.
        submit_prompt(&mut app, "more");
        app.maybe_autosave();
        assert_eq!(
            session_files(&dir),
            vec!["my-project"],
            "only the renamed file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_refuses_to_clobber_and_requires_a_name() {
        let dir = session_temp_dir("rename-guard");
        let mut app = app_in(&dir);
        submit_prompt(&mut app, "x");
        app.input.insert_str("/save one");
        app.submit();
        app.input.insert_str("/fork two"); // now current = two, one preserved
        app.submit();

        // Renaming "two" onto the existing "one" must be refused.
        app.input.insert_str("/rename one");
        app.submit();
        assert!(matches!(last_visible(&app), Entry::Error(_)));

        // A bare /rename reports usage.
        app.input.insert_str("/rename");
        app.submit();
        assert!(matches!(last_visible(&app), Entry::Error(n) if n.contains("usage")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deltas_accumulate_into_the_live_view() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        assert_eq!(app.status, Status::Waiting);

        app.push_delta("Hel");
        app.push_delta("lo");
        assert_eq!(app.status, Status::Streaming);
        assert_eq!(app.streaming.as_deref(), Some("Hello"));
    }

    #[test]
    fn finishing_the_stream_commits_the_full_reply_and_clears_the_view() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        // The streamed text is display-only; the committed reply is the full
        // text handed to push_response.
        app.push_delta("<ai-harness-response>partial");
        app.finish_stream();
        app.push_response(
            "<ai-harness-response>Hello there.</ai-harness-response>".into(),
            None,
        );

        assert!(app.streaming.is_none(), "the live view must be cleared");
        assert_eq!(app.status, Status::Idle);
        match last_visible(&app) {
            Entry::Action { action, .. } => {
                assert_eq!(*action, Action::Response("Hello there.".into()))
            }
            other => panic!("expected a committed action, got {other:?}"),
        }
    }

    #[test]
    fn streaming_counts_as_busy_so_the_prompt_is_inert() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_delta("partial");
        assert!(app.is_busy());
        app.input.insert_str("more");
        assert!(app.submit().is_none(), "cannot submit mid-stream");
    }

    #[test]
    fn a_stream_error_clears_the_view_and_returns_to_idle() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_delta("half a rep");
        app.finish_stream();
        app.push_error("connection dropped".into());

        assert!(app.streaming.is_none());
        assert_eq!(app.status, Status::Idle);
        assert!(matches!(last_visible(&app), Entry::Error(_)));
    }

    #[test]
    fn typing_a_slash_offers_every_command() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        assert!(app.completions().is_empty(), "nothing typed yet");
        app.input.insert_char('/');
        assert_eq!(
            names(&app),
            vec![
                "debug", "clear", "save", "load", "rename", "fork", "help", "quit"
            ]
        );
    }

    #[test]
    fn completions_narrow_as_you_type() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("/c");
        assert_eq!(names(&app), vec!["clear"]);
        app.input.insert_str("x");
        assert!(app.completions().is_empty(), "no command starts with cx");
    }

    #[test]
    fn ordinary_prompts_offer_no_completions() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("what is 2+2");
        assert!(app.completions().is_empty());
    }

    #[test]
    fn the_escape_offers_no_completions() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("//debug");
        assert!(
            app.completions().is_empty(),
            "// is a prompt, not a command"
        );
    }

    #[test]
    fn no_completions_while_busy() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        assert!(app.is_busy());
        app.input.insert_str("/de");
        assert!(
            app.completions().is_empty(),
            "the prompt is inert while busy, so the menu must be too"
        );
    }

    #[test]
    fn moving_the_highlight_wraps_both_ways() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_char('/');
        assert_eq!(app.completion_index(), 0);

        let last = app.completions().len() - 1;
        app.move_completion(1);
        assert_eq!(app.completion_index(), 1);
        // Backwards past the start wraps to the end.
        app.move_completion(-2);
        assert_eq!(app.completion_index(), last);
        // Forwards past the end wraps to the start.
        app.move_completion(1);
        assert_eq!(app.completion_index(), 0);
    }

    #[test]
    fn the_highlight_is_clamped_when_the_list_shrinks() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_char('/');
        app.move_completion(3);
        assert_eq!(app.completion_index(), 3);

        // Narrowing to a single match must not leave the cursor out of range.
        app.input.insert_str("c");
        assert_eq!(names(&app), vec!["clear"]);
        assert_eq!(app.completion_index(), 0);
    }

    #[test]
    fn accepting_replaces_the_typed_prefix() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("/de");
        assert!(app.accept_completion());
        assert_eq!(app.input.text(), "/debug");
    }

    #[test]
    fn accepting_uses_the_highlighted_entry() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_char('/');
        app.move_completion(1); // clear (second entry)
        assert!(app.accept_completion());
        assert_eq!(app.input.text(), "/clear");
    }

    #[test]
    fn accepting_with_nothing_offered_changes_nothing() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hello");
        assert!(!app.accept_completion());
        assert_eq!(app.input.text(), "hello");
    }

    #[test]
    fn accepting_then_submitting_runs_the_command() {
        // The path Enter takes: complete the partial name, then submit it.
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("/de");
        app.accept_completion();
        assert!(app.submit().is_none(), "a command must not be sent");
        assert!(app.debug, "/debug should have run");
    }

    #[test]
    fn a_slash_command_never_reaches_the_model() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        let before = app.history.len();
        app.input.insert_str("/debug");

        assert!(
            app.submit().is_none(),
            "a command must not produce a request"
        );
        assert_eq!(app.history.len(), before, "history must be untouched");
        assert!(app.input.is_blank(), "the prompt should be consumed");
        assert_eq!(app.status, Status::Idle);
    }

    #[test]
    fn debug_command_toggles_and_reports() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        assert!(!app.debug);

        app.input.insert_str("/debug");
        app.submit();
        assert!(app.debug);
        assert!(matches!(last_visible(&app), Entry::Notice(n) if n.contains("on")));

        app.input.insert_str("/debug");
        app.submit();
        assert!(!app.debug);
        assert!(matches!(last_visible(&app), Entry::Notice(n) if n.contains("off")));
    }

    #[test]
    fn unknown_command_is_reported_not_sent() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        let before = app.history.len();
        app.input.insert_str("/dubeg");
        assert!(app.submit().is_none());
        assert_eq!(app.history.len(), before);
        assert!(matches!(last_visible(&app), Entry::Notice(n) if n.contains("/help")));
    }

    #[test]
    fn clear_command_resets_the_conversation() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);

        app.input.insert_str("/clear");
        app.submit();
        assert_eq!(app.history.len(), 1, "only the system prompt should remain");
        assert_eq!(app.history[0].role, Role::System);
    }

    #[test]
    fn quit_command_asks_to_exit() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("/quit");
        app.submit();
        assert!(app.should_quit);
    }

    #[test]
    fn a_double_slash_prompt_is_sent_as_text() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.input.insert_str("//debug");
        let sent = app.submit().expect("should be sent as a prompt");
        assert!(
            sent.last()
                .unwrap()
                .content
                .contains("<ai-harness-query>/debug"),
            "the escape should be unwrapped: {}",
            sent.last().unwrap().content
        );
        assert!(!app.debug, "the escaped text must not run the command");
    }

    #[test]
    fn frames_are_recorded_even_when_debug_is_off() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        assert!(!app.debug);
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);

        let frames: Vec<_> = app
            .transcript
            .iter()
            .filter(|e| matches!(e, Entry::Frame { .. }))
            .collect();
        assert_eq!(frames.len(), 2, "one sent, one received: {frames:?}");
        assert!(matches!(
            frames[0],
            Entry::Frame {
                direction: Direction::Sent,
                ..
            }
        ));
        assert!(matches!(
            frames[1],
            Entry::Frame {
                direction: Direction::Received,
                ..
            }
        ));
    }

    #[test]
    fn command_results_are_framed_too() {
        let mut app = awaiting("ls");
        app.approve();
        app.push_command_result(output(0));
        assert!(
            app.transcript.iter().any(|e| matches!(
                e,
                Entry::Frame { direction: Direction::Sent, body } if body.contains("shell-result")
            )),
            "the result we send should be visible in debug"
        );
    }

    #[test]
    fn scrolling_up_disables_follow_and_reaching_bottom_restores_it() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.scroll = 20;
        app.scroll_up(8);
        assert_eq!(app.scroll, 12);
        assert!(!app.follow);

        // Still short of the bottom, so we stay detached.
        app.scroll_down(3, 20);
        assert_eq!(app.scroll, 15);
        assert!(!app.follow);

        // Reaching the bottom re-attaches, and scrolling past it clamps.
        app.scroll_down(50, 20);
        assert_eq!(app.scroll, 20);
        assert!(app.follow);
    }
}

/// Reads need a real `Sandbox`, which only exists on macOS.
#[cfg(all(test, target_os = "macos"))]
mod read_tests {
    use super::tests::{last_visible, visible};
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// An app mid-turn, with a sandbox over a temp directory holding `files`.
    fn app_with_files(files: &[(&str, &str)]) -> (App, std::path::PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let unique = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-appread-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let dir = std::fs::canonicalize(&dir).unwrap();
        for (name, body) in files {
            std::fs::write(dir.join(name), body).unwrap();
        }

        let mut app = App::new("m".into(), None, 10, dir.join("sessions"));
        app.sandbox = Some(Sandbox::new(&dir).unwrap());
        app.input.insert_str("what is in that file?");
        app.submit().unwrap();
        (app, dir)
    }

    fn read_reply(path: &str) -> String {
        format!("<ai-harness-read>{path}</ai-harness-read>")
    }

    #[test]
    fn a_read_runs_immediately_without_asking() {
        let (mut app, _dir) = app_with_files(&[("notes.txt", "hello\n")]);
        let messages = app
            .push_response(read_reply("notes.txt"), None)
            .expect("a read should hand messages straight back to the model");

        assert!(
            app.pending().is_none(),
            "a read must not raise the approval modal"
        );
        assert!(app.is_waiting(), "the loop should continue on its own");
        assert!(messages.last().unwrap().content.contains("hello"));
    }

    #[test]
    fn the_contents_reach_the_model_and_a_preview_reaches_the_transcript() {
        let (mut app, _dir) = app_with_files(&[("notes.txt", "alpha\nbeta\n")]);
        app.push_response(read_reply("notes.txt"), None).unwrap();

        match last_visible(&app) {
            Entry::ReadResult(outcome) => {
                assert_eq!(outcome.path, "notes.txt");
                assert_eq!(outcome.contents, "alpha\nbeta\n");
                assert_eq!(outcome.lines, 2);
            }
            other => panic!("expected a read result, got {other:?}"),
        }
        assert!(app.history.last().unwrap().content.contains("alpha\nbeta"));
    }

    #[test]
    fn a_failed_read_is_reported_to_the_model_rather_than_ending_the_turn() {
        let (mut app, _dir) = app_with_files(&[]);
        let messages = app
            .push_response(read_reply("nope.txt"), None)
            .expect("a failed read still continues the loop");

        assert!(app.is_waiting(), "a bad path must not end the turn");
        assert!(
            !matches!(last_visible(&app), Entry::Error(_)),
            "a missing file is the model's problem to solve, not a harness error"
        );
        assert!(messages.last().unwrap().content.contains("no such file"));
    }

    #[test]
    fn a_read_outside_the_root_fails_without_leaking_contents() {
        let (mut app, dir) = app_with_files(&[]);
        let outside = dir.parent().unwrap().join("ai-harness-app-outside.txt");
        std::fs::write(&outside, "classified").unwrap();

        let messages = app
            .push_response(read_reply(outside.to_str().unwrap()), None)
            .unwrap();
        let sent = &messages.last().unwrap().content;
        assert!(!sent.contains("classified"), "contents escaped: {sent}");
        assert!(sent.contains("outside the working directory"));
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn a_read_counts_against_the_iteration_budget() {
        let (mut app, _dir) = app_with_files(&[("a.txt", "x")]);
        app.max_iterations = 2;
        app.push_response(read_reply("a.txt"), None).unwrap();
        assert_eq!(app.iterations, 1);

        // The budget is spent, so the next read stops the loop instead of
        // running — an auto-approved action is still a bounded one.
        assert!(app.push_response(read_reply("a.txt"), None).is_none());
        assert_eq!(app.iterations, 2);
        assert!(!app.is_busy());
        assert!(matches!(last_visible(&app), Entry::Notice(_)));
    }

    #[test]
    fn confirm_reads_puts_the_read_behind_the_modal() {
        let (mut app, _dir) = app_with_files(&[("notes.txt", "hello\n")]);
        app.confirm_reads = true;

        assert!(
            app.push_response(read_reply("notes.txt"), None).is_none(),
            "with --confirm-reads the read must wait for the user"
        );
        match app.pending() {
            Some(pending) => assert_eq!(
                pending.action,
                Action::Read {
                    path: "notes.txt".into()
                }
            ),
            None => panic!("expected the approval modal"),
        }

        // Approving runs the very same helper the automatic path uses.
        let Some(Action::Read { path }) = app.approve() else {
            panic!("approve should hand back the read")
        };
        let messages = app.perform_read(&path);
        assert!(messages.last().unwrap().content.contains("hello"));
        assert!(app.is_waiting());
    }

    #[test]
    fn a_denied_read_tells_the_model_what_was_refused() {
        let (mut app, _dir) = app_with_files(&[("notes.txt", "hello\n")]);
        app.confirm_reads = true;
        app.push_response(read_reply("notes.txt"), None);

        assert!(app.deny().is_some());
        assert!(
            visible(&app)
                .iter()
                .any(|e| matches!(e, Entry::Denied(what) if what == "read notes.txt"))
        );
    }

    /// Without a sandbox a read must fail closed, not panic or read anyway.
    #[test]
    fn a_read_without_a_sandbox_fails_safely() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        let messages = app.perform_read("anything.txt");
        assert!(messages.last().unwrap().content.contains("not configured"));
    }
}
