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
