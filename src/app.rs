//! Application state: the transcript, the prompt buffer, and request status.

use crate::command::{self, Command};
use crate::exec::CommandOutput;
use crate::input::Input;
use crate::openrouter::{Message, Usage};
use crate::protocol::{self, Action};

/// One rendered block in the transcript. Kept separate from [`Message`] so the
/// UI can show things (errors, notices) that are never sent to the model.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// A command awaiting the user's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub command: String,
    pub selected: Choice,
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
}

/// Default cap on consecutive corrective retries after a malformed reply.
pub const DEFAULT_MAX_RETRIES: usize = 3;

impl App {
    /// `extra_system` is optional operator guidance appended to the protocol
    /// contract; the contract itself is always sent.
    ///
    /// `debug` and `max_retries` are plain fields rather than constructor
    /// arguments: both are adjustable at runtime, `debug` via `/debug`.
    pub fn new(model: String, extra_system: Option<String>, max_iterations: usize) -> Self {
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
            retries: 0,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_anchor: None,
            completion_cursor: 0,
            streaming: None,
        }
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
            Command::Unknown(name) => self.push_notice(format!(
                "Unknown command /{name}. Type /help to see what is available."
            )),
        }
        self.follow = true;
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
            Action::Shell(command) => {
                if self.iterations >= self.max_iterations {
                    self.push_notice(format!(
                        "Stopped after {} model round-trips. Send another prompt to continue.",
                        self.iterations
                    ));
                    self.status = Status::Idle;
                } else {
                    self.status = Status::AwaitingApproval(Pending {
                        command,
                        selected: Choice::Allow,
                    });
                }
            }
        }
        None
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

    /// Accept the pending command; the caller runs it. Returns the command.
    pub fn approve(&mut self) -> Option<String> {
        let Status::AwaitingApproval(pending) = &self.status else {
            return None;
        };
        let command = pending.command.clone();
        self.status = Status::Running;
        self.follow = true;
        Some(command)
    }

    /// Refuse the pending command and tell the model, so it can try something
    /// else rather than assuming the command ran. Returns messages to send.
    pub fn deny(&mut self) -> Option<Vec<Message>> {
        let Status::AwaitingApproval(pending) = &self.status else {
            return None;
        };
        let command = pending.command.clone();
        self.transcript.push(Entry::Denied(command));
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
        let mut app = App::new("m".into(), None, 10);
        assert!(app.submit().is_none());
        app.input.insert_str("   \n  ");
        assert!(app.submit().is_none());
    }

    /// Index of the first non-system message; the protocol prompt always leads.
    const FIRST_TURN: usize = 1;

    /// Transcript with debug frames filtered out. Frames are interleaved with
    /// everything else, so tests about visible content must ignore them.
    fn visible(app: &App) -> Vec<&Entry> {
        app.transcript
            .iter()
            .filter(|e| !matches!(e, Entry::Frame { .. }))
            .collect()
    }

    fn last_visible(app: &App) -> &Entry {
        visible(app).pop().expect("transcript should not be empty")
    }

    #[test]
    fn submit_wraps_the_prompt_in_the_query_tag() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("hi there");
        app.submit().unwrap();
        assert!(matches!(visible(&app).as_slice(), [Entry::User(t)] if t == "hi there"));
    }

    #[test]
    fn submit_is_ignored_while_waiting() {
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("one");
        app.submit().unwrap();
        app.input.insert_str("two");
        assert!(app.submit().is_none());
        assert_eq!(app.history.len(), FIRST_TURN + 1);
    }

    #[test]
    fn protocol_system_prompt_always_leads_the_history() {
        let app = App::new("m".into(), None, 10);
        assert_eq!(app.history[0].role, Role::System);
        assert!(app.history[0].content.contains("ai-harness-shell"));
        assert!(app.history[0].content.contains("ai-harness-response"));
    }

    #[test]
    fn operator_guidance_is_appended_to_the_protocol_prompt() {
        let app = App::new("m".into(), Some("be terse".into()), 10);
        assert_eq!(app.history.len(), 1);
        assert!(app.history[0].content.contains("be terse"));
        assert!(
            app.history[0].content.contains("ai-harness-shell"),
            "operator guidance must not replace the protocol contract"
        );
    }

    #[test]
    fn valid_shell_reply_becomes_an_action_entry() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("garbage".into(), None);
        assert_eq!(app.iterations, 0, "a rejected reply is not progress");

        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);
        assert_eq!(app.iterations, 1);
    }

    #[test]
    fn a_valid_reply_resets_the_retry_streak() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_response("<ai-harness-response>ok</ai-harness-response>".into(), None);
        assert!(!app.is_waiting());
        assert_eq!(app.history.len(), FIRST_TURN + 2);
        assert_eq!(app.history[FIRST_TURN + 1].role, Role::Assistant);
    }

    #[test]
    fn error_rolls_back_the_failed_user_turn() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), Some("be terse".into()), 10);
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
        }
    }

    /// Drive an app to a pending shell approval.
    fn awaiting(command: &str) -> App {
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("do the thing");
        app.submit().unwrap();
        app.push_response(
            format!("<ai-harness-shell>{command}</ai-harness-shell>"),
            None,
        );
        app
    }

    #[test]
    fn a_shell_action_requires_approval_before_running() {
        let app = awaiting("ls -la");
        match app.pending() {
            Some(pending) => {
                assert_eq!(pending.command, "ls -la");
                assert_eq!(pending.selected, Choice::Allow, "Allow should be focused");
            }
            None => panic!("shell action must await approval, got {:?}", app.status),
        }
    }

    #[test]
    fn a_final_response_does_not_ask_for_approval() {
        let mut app = App::new("m".into(), None, 10);
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
    fn approving_moves_to_running_and_returns_the_command() {
        let mut app = awaiting("echo hi");
        assert_eq!(app.approve(), Some("echo hi".to_string()));
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
        let mut app = App::new("m".into(), None, 10);
        app.toggle_choice();
        app.set_choice(Choice::Deny);
        assert!(app.deny().is_none());
        assert!(app.approve().is_none());
        assert_eq!(app.status, Status::Idle);
    }

    #[test]
    fn the_loop_stops_at_the_iteration_cap() {
        let mut app = App::new("m".into(), None, 3);
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
        let mut app = App::new("m".into(), None, 10);
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
    fn deltas_accumulate_into_the_live_view() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("hi");
        app.submit().unwrap();
        app.push_delta("partial");
        assert!(app.is_busy());
        app.input.insert_str("more");
        assert!(app.submit().is_none(), "cannot submit mid-stream");
    }

    #[test]
    fn a_stream_error_clears_the_view_and_returns_to_idle() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
        assert!(app.completions().is_empty(), "nothing typed yet");
        app.input.insert_char('/');
        assert_eq!(names(&app), vec!["debug", "clear", "help", "quit"]);
    }

    #[test]
    fn completions_narrow_as_you_type() {
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("/c");
        assert_eq!(names(&app), vec!["clear"]);
        app.input.insert_str("x");
        assert!(app.completions().is_empty(), "no command starts with cx");
    }

    #[test]
    fn ordinary_prompts_offer_no_completions() {
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("what is 2+2");
        assert!(app.completions().is_empty());
    }

    #[test]
    fn the_escape_offers_no_completions() {
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("//debug");
        assert!(
            app.completions().is_empty(),
            "// is a prompt, not a command"
        );
    }

    #[test]
    fn no_completions_while_busy() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_char('/');
        assert_eq!(app.completion_index(), 0);

        app.move_completion(1);
        assert_eq!(app.completion_index(), 1);
        // Backwards past the start wraps to the end.
        app.move_completion(-2);
        assert_eq!(app.completion_index(), 3);
        // Forwards past the end wraps to the start.
        app.move_completion(1);
        assert_eq!(app.completion_index(), 0);
    }

    #[test]
    fn the_highlight_is_clamped_when_the_list_shrinks() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("/de");
        assert!(app.accept_completion());
        assert_eq!(app.input.text(), "/debug");
    }

    #[test]
    fn accepting_uses_the_highlighted_entry() {
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_char('/');
        app.move_completion(2); // help
        assert!(app.accept_completion());
        assert_eq!(app.input.text(), "/help");
    }

    #[test]
    fn accepting_with_nothing_offered_changes_nothing() {
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("hello");
        assert!(!app.accept_completion());
        assert_eq!(app.input.text(), "hello");
    }

    #[test]
    fn accepting_then_submitting_runs_the_command() {
        // The path Enter takes: complete the partial name, then submit it.
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("/de");
        app.accept_completion();
        assert!(app.submit().is_none(), "a command must not be sent");
        assert!(app.debug, "/debug should have run");
    }

    #[test]
    fn a_slash_command_never_reaches_the_model() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
        let before = app.history.len();
        app.input.insert_str("/dubeg");
        assert!(app.submit().is_none());
        assert_eq!(app.history.len(), before);
        assert!(matches!(last_visible(&app), Entry::Notice(n) if n.contains("/help")));
    }

    #[test]
    fn clear_command_resets_the_conversation() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
        app.input.insert_str("/quit");
        app.submit();
        assert!(app.should_quit);
    }

    #[test]
    fn a_double_slash_prompt_is_sent_as_text() {
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
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
        let mut app = App::new("m".into(), None, 10);
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
