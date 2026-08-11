//! Slash commands, handled locally and never sent to the model.
//!
//! `parse` returns `None` for ordinary prompts so callers fall through to the
//! normal submit path. A leading `//` escapes to a literal `/`, so a prompt can
//! still begin with a slash.

/// A command typed at the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Toggle protocol frame visibility.
    Debug,
    /// Toggle running actions without the approval modal.
    Auto,
    /// Toggle showing the model's reasoning while it streams.
    Reasoning,
    /// Restore the workspace to before the last turn that changed it.
    Undo,
    /// Choose how far back to undo, from a list of the conversation.
    Rewind,
    /// Open the sessions view, the same as `Ctrl+Space`.
    Sessions,
    /// List the project's memory notes and what the index made of them.
    Memory,
    /// List background jobs; `Some("kill <id>")` stops one.
    Jobs(Option<String>),
    /// Open the page of what this session has done.
    Stats,
    /// List checkpoints; `Some` sets how many turns to keep.
    Checkpoints(Option<String>),
    /// Toggle plan mode. `Some` carries the task to start planning immediately.
    Plan(Option<String>),
    Help,
    Clear,
    /// Summarise the older part of the conversation to free context. The
    /// measured alternative to `Clear`, which throws it all away.
    Compact,
    Quit,
    /// Save the session; `None` uses a generated name.
    Save(Option<String>),
    /// Load a saved session; `None` lists what is available.
    Load(Option<String>),
    /// Rename the current session's file; `None` reports usage.
    Rename(Option<String>),
    /// Branch the current conversation into a new session; `None` generates a name.
    Fork(Option<String>),
    /// Report cumulative token spend for the session.
    Cost,
    /// Choose the model; `None` opens the picker, `Some` sets it by id.
    Model(Option<String>),
    /// Something starting with `/` that we do not recognise. Reported to the
    /// user rather than forwarded — silently sending a typo'd command to the
    /// model is the worst available outcome.
    Unknown(String),
}

impl Command {
    /// Whether this can run with a turn already in flight.
    ///
    /// The prompt stays usable while the harness works, so this is what decides
    /// what that usefully means. Two things disqualify a command: rewriting the
    /// `history` the in-flight request was built from — the reply would land on
    /// a conversation that no longer matches what was sent — and moving the
    /// session folder, which is where the turn's open checkpoint is writing.
    ///
    /// Exhaustive on purpose: a command added later has to answer this question
    /// rather than inherit an answer.
    pub fn runs_while_busy(&self) -> bool {
        match self {
            // Display toggles, read where they are used rather than latched.
            // `/auto` is the one worth having mid-turn: it is read at the
            // approval decision, so flipping it is how you stop being asked
            // about the rest of a turn you have decided to trust.
            Command::Debug | Command::Reasoning | Command::Auto => true,
            // Read-only. `/checkpoints <n>` prunes, but oldest-first, so the
            // checkpoint the current turn is filling is never the one dropped.
            Command::Cost | Command::Help | Command::Checkpoints(_) => true,
            // Parks a flag the event loop takes; the view is about the harness
            // rather than about this conversation.
            Command::Sessions => true,
            // Reads the memory directory and reports. Touches no conversation.
            Command::Memory => true,
            // Jobs are not part of a turn — that is what makes them jobs — so
            // neither listing nor killing one touches the conversation, and the
            // turn most worth killing a job during is one that is still running.
            Command::Jobs(_) => true,
            // A page of numbers derived from what already happened.
            Command::Stats => true,
            // The in-flight request already carries its model, so this lands on
            // the next turn — which is usually why it is being typed.
            Command::Model(_) => true,
            // A snapshot of what has happened. The in-flight reply is not in
            // `history` yet, so the file is consistent either way.
            Command::Save(None) => true,
            // Quitting cancels and saves every session on its way out.
            Command::Quit => true,
            // Only pushes "unknown command". Answering a typo with "wait for the
            // turn to finish" would be a worse reply than the right one.
            Command::Unknown(_) => true,

            // `/save <name>` *renames* the current session, exactly as `/rename`
            // does — see `App::save_session`. That moves the folder the turn's
            // checkpoint is being written into, so the argument, not the name,
            // is what makes it unsafe.
            Command::Save(Some(_)) | Command::Rename(_) => false,
            // Each of these rewrites or replaces `history` under the request.
            Command::Clear | Command::Compact | Command::Load(_) | Command::Fork(_) => false,
            // Changes the contract and the sandbox mid-turn, and `/plan <task>`
            // would start a second turn on top of the first.
            Command::Plan(_) => false,
            // Restore files and truncate the conversation. Both would be undoing
            // a turn that is still adding to it.
            Command::Undo | Command::Rewind => false,
        }
    }

    /// The canonical name, without the slash — for the notice that refuses it.
    ///
    /// Matches the [`COMMANDS`] table, which the tests check.
    pub fn name(&self) -> &str {
        match self {
            Command::Debug => "debug",
            Command::Auto => "auto",
            Command::Reasoning => "reasoning",
            Command::Undo => "undo",
            Command::Rewind => "rewind",
            Command::Sessions => "sessions",
            Command::Memory => "memory",
            Command::Jobs(_) => "jobs",
            Command::Stats => "stats",
            Command::Checkpoints(_) => "checkpoints",
            Command::Plan(_) => "plan",
            Command::Help => "help",
            Command::Clear => "clear",
            Command::Compact => "compact",
            Command::Quit => "quit",
            Command::Save(_) => "save",
            Command::Load(_) => "load",
            Command::Rename(_) => "rename",
            Command::Fork(_) => "fork",
            Command::Cost => "cost",
            Command::Model(_) => "model",
            Command::Unknown(name) => name,
        }
    }
}

/// What the prompt should do with a line of input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Run this command locally.
    Command(Command),
    /// Send this text to the model. May differ from what was typed when a
    /// leading `//` escape has been unwrapped.
    Prompt(String),
}

/// Classify a line of input.
pub fn parse(input: &str) -> Input {
    let trimmed = input.trim();

    // `//foo` is an escape for a prompt that really does start with a slash.
    if let Some(rest) = trimmed.strip_prefix("//") {
        return Input::Prompt(format!("/{rest}"));
    }

    let Some(rest) = trimmed.strip_prefix('/') else {
        return Input::Prompt(input.trim_end().to_string());
    };

    // Split the name from its argument (everything after the first whitespace).
    let name = rest.split_whitespace().next().unwrap_or("");
    let arg = rest[name.len()..].trim();
    let arg = (!arg.is_empty()).then(|| arg.to_string());

    Input::Command(match name.to_ascii_lowercase().as_str() {
        "debug" => Command::Debug,
        "auto" | "auto-approve" => Command::Auto,
        "reasoning" | "thinking" => Command::Reasoning,
        "undo" => Command::Undo,
        "rewind" => Command::Rewind,
        "sessions" => Command::Sessions,
        "memory" | "memories" => Command::Memory,
        "jobs" | "job" => Command::Jobs(arg),
        "stats" => Command::Stats,
        "checkpoints" | "checkpoint" => Command::Checkpoints(arg),
        "plan" => Command::Plan(arg),
        "help" | "h" | "?" => Command::Help,
        "clear" | "reset" => Command::Clear,
        "compact" => Command::Compact,
        "quit" | "exit" | "q" => Command::Quit,
        "save" => Command::Save(arg),
        "load" => Command::Load(arg),
        "rename" => Command::Rename(arg),
        "fork" => Command::Fork(arg),
        "cost" | "tokens" => Command::Cost,
        "model" => Command::Model(arg),
        // A bare "/" has no name; report it the same way as any unknown.
        _ => Command::Unknown(name.to_string()),
    })
}

/// A command as offered by autocomplete and `/help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    /// Canonical name, without the leading slash.
    pub name: &'static str,
    pub description: &'static str,
}

/// Every command, in the order shown. Aliases are deliberately absent: they
/// still work when typed, but listing them would triple the menu for no gain.
/// This table is the single source of truth for both `/help` and completion.
pub const COMMANDS: &[Spec] = &[
    Spec {
        name: "debug",
        description: "toggle showing the raw protocol sent and received",
    },
    Spec {
        name: "auto",
        description: "toggle running actions without the approval modal",
    },
    Spec {
        name: "reasoning",
        description: "toggle showing the model's reasoning while it streams",
    },
    Spec {
        name: "plan",
        description: "toggle plan mode: research and write a plan first (/plan [task])",
    },
    Spec {
        name: "clear",
        description: "clear the conversation, keeping the system prompt",
    },
    Spec {
        name: "compact",
        description: "summarise the older part of the conversation to free context",
    },
    Spec {
        name: "undo",
        description: "restore the files the last changing turn touched, and rewind it",
    },
    Spec {
        name: "rewind",
        description: "choose how far back to undo, from a list of the conversation",
    },
    Spec {
        name: "checkpoints",
        description: "list what can be undone (/checkpoints <n> keeps only the last n)",
    },
    Spec {
        name: "sessions",
        description: "switch between running sessions, or start one (also Ctrl+Space)",
    },
    Spec {
        name: "memory",
        description: "list the notes in .ai_harness/memory and how they index",
    },
    Spec {
        name: "jobs",
        description: "list background jobs, or stop one (/jobs kill <id>)",
    },
    Spec {
        name: "stats",
        description: "what this session has done, including how memory was used",
    },
    Spec {
        name: "save",
        description: "save this session to disk (/save [name])",
    },
    Spec {
        name: "load",
        description: "load a saved session (/load [name], or /load to list)",
    },
    Spec {
        name: "rename",
        description: "rename the current session (/rename <name>)",
    },
    Spec {
        name: "fork",
        description: "branch into a new session, keeping the original (/fork [name])",
    },
    Spec {
        name: "cost",
        description: "show cumulative tokens and estimated spend",
    },
    Spec {
        name: "model",
        description: "choose the model (/model to browse, /model <id> to set)",
    },
    Spec {
        name: "help",
        description: "list these commands",
    },
    Spec {
        name: "quit",
        description: "exit",
    },
];

/// The partial command name being typed, if the prompt currently holds one.
///
/// `None` once the name is settled — after a space, for a `//` escape, or for
/// ordinary text — so the menu appears only while it is still useful.
pub fn completion_prefix(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if trimmed.starts_with("//") {
        return None;
    }
    let rest = trimmed.strip_prefix('/')?;
    // Once an argument is being typed the command itself is decided.
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// Commands whose name starts with `prefix`, case-insensitively.
pub fn matching(prefix: &str) -> Vec<&'static Spec> {
    let prefix = prefix.to_ascii_lowercase();
    COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(&prefix))
        .collect()
}

/// Listing shown by `/help`, generated from [`COMMANDS`].
pub fn help_text() -> String {
    let width = COMMANDS.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut lines = vec!["Commands (handled locally, never sent to the model):".to_string()];
    for spec in COMMANDS {
        lines.push(format!(
            "  /{:<width$}  {}",
            spec.name,
            spec.description,
            width = width
        ));
    }
    lines.push(String::new());
    lines.push("Tab completes a command; ↑/↓ choose, Enter runs it.".to_string());
    lines.push("Start a prompt with // to send text beginning with a slash.".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(input: &str) -> Command {
        match parse(input) {
            Input::Command(c) => c,
            Input::Prompt(p) => panic!("expected a command, got prompt {p:?}"),
        }
    }

    fn prompt(input: &str) -> String {
        match parse(input) {
            Input::Prompt(p) => p,
            Input::Command(c) => panic!("expected a prompt, got command {c:?}"),
        }
    }

    #[test]
    fn parses_each_command() {
        assert_eq!(command("/debug"), Command::Debug);
        assert_eq!(command("/auto"), Command::Auto);
        assert_eq!(command("/auto-approve"), Command::Auto);
        assert_eq!(command("/plan"), Command::Plan(None));
        assert_eq!(
            command("/plan add a --json flag"),
            Command::Plan(Some("add a --json flag".into()))
        );
        assert_eq!(command("/help"), Command::Help);
        assert_eq!(command("/clear"), Command::Clear);
        assert_eq!(command("/quit"), Command::Quit);
    }

    #[test]
    fn accepts_aliases_and_any_casing() {
        assert_eq!(command("/DEBUG"), Command::Debug);
        assert_eq!(command("/?"), Command::Help);
        assert_eq!(command("/h"), Command::Help);
        assert_eq!(command("/reset"), Command::Clear);
        assert_eq!(command("/exit"), Command::Quit);
        assert_eq!(command("/Q"), Command::Quit);
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        assert_eq!(command("  /debug  "), Command::Debug);
        assert_eq!(command("\n/help\n"), Command::Help);
    }

    #[test]
    fn ignores_trailing_arguments_for_now() {
        assert_eq!(command("/debug on"), Command::Debug);
        assert_eq!(command("/clear   everything"), Command::Clear);
    }

    #[test]
    fn unknown_commands_are_reported_not_forwarded() {
        assert_eq!(command("/dubeg"), Command::Unknown("dubeg".into()));
        assert_eq!(command("/"), Command::Unknown(String::new()));
    }

    #[test]
    fn ordinary_text_is_a_prompt() {
        assert_eq!(prompt("what is 2+2"), "what is 2+2");
        assert_eq!(prompt("count the / characters"), "count the / characters");
    }

    #[test]
    fn double_slash_escapes_to_a_literal_slash() {
        assert_eq!(prompt("//debug"), "/debug");
        assert_eq!(prompt("//not a command"), "/not a command");
    }

    #[test]
    fn a_prompt_keeps_its_interior_formatting() {
        // Only trailing whitespace is trimmed; internal newlines survive.
        assert_eq!(prompt("line one\nline two  "), "line one\nline two");
    }

    #[test]
    fn help_lists_every_command() {
        let text = help_text();
        for spec in COMMANDS {
            assert!(
                text.contains(&format!("/{}", spec.name)),
                "help should mention /{}:\n{text}",
                spec.name
            );
            assert!(text.contains(spec.description));
        }
    }

    /// The table drives both the menu and the dispatcher, so every listed name
    /// must actually parse to a real command.
    #[test]
    fn every_listed_command_parses() {
        for spec in COMMANDS {
            let parsed = command(&format!("/{}", spec.name));
            assert!(
                !matches!(parsed, Command::Unknown(_)),
                "/{} is offered by completion but does not parse",
                spec.name
            );
        }
    }

    /// `name()` is what the refusal notice tells you to retype, so it has to be
    /// the name that parses — and the table is where those names are decided.
    #[test]
    fn every_listed_command_names_itself_the_way_it_is_typed() {
        for spec in COMMANDS {
            assert_eq!(
                command(&format!("/{}", spec.name)).name(),
                spec.name,
                "/{} does not name itself",
                spec.name
            );
        }
    }

    #[test]
    fn commands_that_only_read_or_toggle_run_mid_turn() {
        for input in [
            "/debug",
            "/auto",
            "/reasoning",
            "/cost",
            "/help",
            "/checkpoints",
            "/checkpoints 3",
            "/sessions",
            "/model",
            "/model x/y",
            "/quit",
            "/nonsense",
        ] {
            assert!(
                command(input).runs_while_busy(),
                "{input} should run while a turn is in flight"
            );
        }
    }

    #[test]
    fn commands_that_rewrite_the_conversation_wait_their_turn() {
        for input in [
            "/clear",
            "/compact",
            "/load",
            "/load old",
            "/fork",
            "/plan",
            "/plan do a thing",
            "/undo",
            "/rewind",
            "/rename other",
        ] {
            assert!(
                !command(input).runs_while_busy(),
                "{input} should wait for the turn to finish"
            );
        }
    }

    /// The trap: `/save <name>` renames the session, and the folder it moves is
    /// where the running turn's checkpoint is being written. So the argument
    /// decides, not the name — which is why this is classified on the parsed
    /// command rather than on the `COMMANDS` table.
    #[test]
    fn save_is_safe_mid_turn_only_without_a_name() {
        assert!(command("/save").runs_while_busy());
        assert!(!command("/save elsewhere").runs_while_busy());
    }

    #[test]
    fn completion_prefix_tracks_a_command_being_typed() {
        assert_eq!(completion_prefix("/"), Some(""));
        assert_eq!(completion_prefix("/de"), Some("de"));
        assert_eq!(completion_prefix("/debug"), Some("debug"));
        assert_eq!(completion_prefix("  /de  "), Some("de"));
    }

    #[test]
    fn completion_prefix_stops_once_the_name_is_settled() {
        // An argument means the command itself is already chosen.
        assert_eq!(completion_prefix("/debug on"), None);
        // The escape is a prompt, not a command.
        assert_eq!(completion_prefix("//debug"), None);
        // Ordinary text never offers completions.
        assert_eq!(completion_prefix("hello"), None);
        assert_eq!(completion_prefix(""), None);
        assert_eq!(completion_prefix("what about / this"), None);
    }

    #[test]
    fn matching_filters_by_prefix() {
        // An empty prefix offers the whole table, in table order.
        let names: Vec<_> = matching("").iter().map(|s| s.name).collect();
        let expected: Vec<_> = COMMANDS.iter().map(|s| s.name).collect();
        assert_eq!(names, expected);

        // A prefix shared by several commands offers all of them, in table order.
        let names: Vec<_> = matching("c").iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["clear", "compact", "checkpoints", "cost"]);

        let names: Vec<_> = matching("cl").iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["clear"]);

        let names: Vec<_> = matching("f").iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["fork"]);
    }

    #[test]
    fn parses_commands_with_an_argument() {
        assert_eq!(
            parse("/save foo"),
            Input::Command(Command::Save(Some("foo".into())))
        );
        assert_eq!(parse("/save"), Input::Command(Command::Save(None)));
        assert_eq!(
            parse("/load my-session"),
            Input::Command(Command::Load(Some("my-session".into())))
        );
        assert_eq!(parse("/load"), Input::Command(Command::Load(None)));
        // Surrounding whitespace in the argument is trimmed.
        assert_eq!(
            parse("/save   spaced  "),
            Input::Command(Command::Save(Some("spaced".into())))
        );
    }

    #[test]
    fn matching_ignores_case() {
        let names: Vec<_> = matching("DE").iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["debug"]);
    }

    #[test]
    fn matching_an_unknown_prefix_is_empty() {
        assert!(matching("zzz").is_empty());
    }

    #[test]
    fn an_exact_name_still_matches_itself() {
        // So Enter on a fully-typed command still has something highlighted.
        let names: Vec<_> = matching("debug").iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["debug"]);
    }
}
