//! The wire protocol between the harness and the model.
//!
//! User input goes out wrapped in `<ai-harness-query>`. The model must reply
//! with exactly one recognised element and nothing else:
//!
//! ```text
//! <ai-harness-shell>ls -la</ai-harness-shell>
//! <ai-harness-response>All done.</ai-harness-response>
//! ```
//!
//! After a shell action runs, the harness reports the outcome back to the
//! model wrapped in `<ai-harness-shell-result>`:
//!
//! ```text
//! <ai-harness-shell-result>
//! exit code: 0
//! stdout:
//! ...
//! stderr:
//! ...
//! </ai-harness-shell-result>
//! ```
//!
//! This is harness → model only; it is never parsed as a model reply.
//!
//! Parsing is deliberately strict. A reply that is *nearly* right — prose around
//! the tag, a code fence, two elements — is rejected rather than guessed at, so
//! protocol drift shows up immediately instead of silently changing behaviour.

use std::fmt;

use crate::exec::CommandOutput;

pub const QUERY_TAG: &str = "ai-harness-query";
pub const SHELL_TAG: &str = "ai-harness-shell";
pub const RESPONSE_TAG: &str = "ai-harness-response";
/// Harness → model only. Carries the outcome of a shell action; never parsed.
pub const RESULT_TAG: &str = "ai-harness-shell-result";

/// Tags the model is allowed to reply with.
const REPLY_TAGS: [&str; 2] = [SHELL_TAG, RESPONSE_TAG];

/// A validated model reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A shell command the model wants run. Not executed yet.
    Shell(String),
    /// A terminating answer for the user.
    Response(String),
}

impl Action {
    pub fn body(&self) -> &str {
        match self {
            Self::Shell(s) | Self::Response(s) => s,
        }
    }
}

/// Why a reply was rejected. Each carries enough detail to show the user what
/// actually came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    Empty,
    /// Reply did not begin with a tag.
    NotATag {
        found: String,
    },
    /// Opening tag was never closed with `>`.
    UnterminatedOpenTag,
    /// A tag we do not recognise.
    UnknownTag {
        tag: String,
    },
    /// The model used the query tag, which only the harness may send.
    QueryTagFromModel,
    /// Opening tag had no matching closing tag.
    MissingClosingTag {
        tag: String,
    },
    /// Content after the closing tag — prose, or a second element.
    TrailingContent {
        tag: String,
        trailing: String,
    },
    /// A recognised tag with nothing inside it.
    EmptyBody {
        tag: String,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the model returned an empty reply"),
            Self::NotATag { found } => write!(
                f,
                "reply must start with a tag, but began with: {}",
                snippet(found)
            ),
            Self::UnterminatedOpenTag => {
                write!(f, "the opening tag was never closed with '>'")
            }
            Self::UnknownTag { tag } => write!(
                f,
                "unknown tag <{tag}>; expected <{SHELL_TAG}> or <{RESPONSE_TAG}>"
            ),
            Self::QueryTagFromModel => write!(
                f,
                "<{QUERY_TAG}> is sent by the harness, not the model; \
                 expected <{SHELL_TAG}> or <{RESPONSE_TAG}>"
            ),
            Self::MissingClosingTag { tag } => {
                write!(f, "missing closing </{tag}>")
            }
            Self::TrailingContent { tag, trailing } => write!(
                f,
                "unexpected content after </{tag}>: {}",
                snippet(trailing)
            ),
            Self::EmptyBody { tag } => write!(f, "<{tag}> was empty"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Wrap user input for sending to the model.
pub fn encode_query(text: &str) -> String {
    format!("<{QUERY_TAG}>{}</{QUERY_TAG}>", text.trim())
}

/// Report a completed command back to the model.
pub fn encode_shell_result(output: &CommandOutput) -> String {
    let mut body = String::new();

    if output.timed_out {
        body.push_str("status: timed out and was killed\n");
    } else {
        match output.exit_code {
            Some(code) => body.push_str(&format!("exit code: {code}\n")),
            None => body.push_str("status: killed by a signal\n"),
        }
    }
    if output.truncated {
        body.push_str("note: output was truncated because it exceeded the size limit\n");
    }

    body.push_str(&section("stdout", &output.stdout));
    body.push_str(&section("stderr", &output.stderr));

    format!("<{RESULT_TAG}>\n{body}</{RESULT_TAG}>")
}

/// Tell the model exactly how its last reply broke the contract, so it can try
/// again. Quotes the specific failure rather than restating the rules in full —
/// the system prompt already carries those, and a targeted correction is more
/// likely to land.
pub fn encode_correction(error: &ProtocolError) -> String {
    format!(
        "Your last reply was rejected by the parser: {error}\n\n\
         Reply again with exactly one element and nothing else — no prose, no \
         markdown fences. The first character must be '<' and the last must be \
         '>'. Use <{SHELL_TAG}>…</{SHELL_TAG}> to run a command, or \
         <{RESPONSE_TAG}>…</{RESPONSE_TAG}> to answer."
    )
}

/// Tell the model the user refused to run the command, so it can offer something
/// else instead of assuming the command ran.
pub fn encode_denied() -> String {
    format!(
        "<{RESULT_TAG}>\nstatus: the user denied permission to run this command; \
         it was not executed\n</{RESULT_TAG}>"
    )
}

fn section(name: &str, content: &str) -> String {
    let trimmed = content.trim_end();
    if trimmed.is_empty() {
        format!("{name}: (empty)\n")
    } else {
        format!("{name}:\n{trimmed}\n")
    }
}

/// Parse a model reply, rejecting anything that is not exactly one valid element.
pub fn parse_reply(raw: &str) -> Result<Action, ProtocolError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ProtocolError::Empty);
    }
    if !trimmed.starts_with('<') {
        return Err(ProtocolError::NotATag {
            found: trimmed.to_string(),
        });
    }

    let close_bracket = trimmed
        .find('>')
        .ok_or(ProtocolError::UnterminatedOpenTag)?;
    let tag = &trimmed[1..close_bracket];

    if tag == QUERY_TAG {
        return Err(ProtocolError::QueryTagFromModel);
    }
    if !REPLY_TAGS.contains(&tag) {
        return Err(ProtocolError::UnknownTag {
            tag: tag.to_string(),
        });
    }

    let closing = format!("</{tag}>");
    let after_open = &trimmed[close_bracket + 1..];
    let closing_at = after_open
        .find(&closing)
        .ok_or_else(|| ProtocolError::MissingClosingTag {
            tag: tag.to_string(),
        })?;

    let body = &after_open[..closing_at];
    let trailing = after_open[closing_at + closing.len()..].trim();
    if !trailing.is_empty() {
        return Err(ProtocolError::TrailingContent {
            tag: tag.to_string(),
            trailing: trailing.to_string(),
        });
    }

    let body = body.trim();
    if body.is_empty() {
        return Err(ProtocolError::EmptyBody {
            tag: tag.to_string(),
        });
    }

    Ok(match tag {
        SHELL_TAG => Action::Shell(body.to_string()),
        _ => Action::Response(body.to_string()),
    })
}

/// The protocol contract sent as the system prompt. `extra` appends any
/// operator-supplied guidance after the rules.
pub fn system_prompt(extra: Option<&str>) -> String {
    let mut prompt = format!(
        "You are the reasoning engine of a terminal agent called ai-harness.

Every user message arrives wrapped in a single element:

<{QUERY_TAG}>the user's request</{QUERY_TAG}>

You MUST reply with exactly one of the following elements, and nothing else.

1. Run a shell command. Use this to gather information or take action:

<{SHELL_TAG}>the command to run</{SHELL_TAG}>

2. Give the user a final answer. This ends the current task:

<{RESPONSE_TAG}>your answer to the user</{RESPONSE_TAG}>

After a shell command, the harness sends you the outcome as:

<{RESULT_TAG}>
exit code: 0
stdout:
...
stderr:
...
</{RESULT_TAG}>

Reply to that with another <{SHELL_TAG}> to keep going, or <{RESPONSE_TAG}> when \
you have what you need. Never emit <{RESULT_TAG}> yourself.

The user approves every command before it runs. If a result says the command was \
denied, it did NOT run: propose a different approach or explain the problem with \
<{RESPONSE_TAG}>. Do not simply repeat the same command.

Commands run in a sandbox rooted at the working directory. Writes outside that \
directory fail, and credential files such as .env and ~/.ssh are unreadable. \
Treat those failures as expected, not as something to work around.

Rules, all strictly enforced by a parser:

- Reply with exactly ONE element. Never two, never zero.
- Emit nothing outside the element: no prose, no explanation, no markdown code \
fences, no leading or trailing text. The very first character of your reply must \
be '<' and the very last must be '>'.
- Use only the two tags listed above. Never emit <{QUERY_TAG}>; that tag belongs \
to the harness.
- The element must be non-empty.
- Put exactly one shell command in <{SHELL_TAG}>. Chain steps with '&&' or ';' if \
you need several. Prefer non-interactive commands that terminate on their own.
- If you can answer without running anything, reply with <{RESPONSE_TAG}> directly.

A reply that breaks any of these rules is discarded and shown to the user as an \
error, so follow them exactly."
    );

    if let Some(extra) = extra {
        let extra = extra.trim();
        if !extra.is_empty() {
            prompt.push_str("\n\nAdditional operator instructions:\n\n");
            prompt.push_str(extra);
        }
    }
    prompt
}

/// Shorten a snippet for an error message, on a char boundary.
fn snippet(s: &str) -> String {
    const MAX: usize = 120;
    let s = s.trim();
    if s.chars().count() <= MAX {
        return format!("{s:?}");
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head:?}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_with_the_query_tag() {
        assert_eq!(
            encode_query("list the files"),
            "<ai-harness-query>list the files</ai-harness-query>"
        );
    }

    #[test]
    fn encoding_trims_surrounding_whitespace() {
        assert_eq!(
            encode_query("  hello\n\n"),
            "<ai-harness-query>hello</ai-harness-query>"
        );
    }

    #[test]
    fn encoding_preserves_interior_newlines() {
        assert_eq!(
            encode_query("line one\nline two"),
            "<ai-harness-query>line one\nline two</ai-harness-query>"
        );
    }

    #[test]
    fn parses_a_shell_action() {
        assert_eq!(
            parse_reply("<ai-harness-shell>ls -la</ai-harness-shell>").unwrap(),
            Action::Shell("ls -la".into())
        );
    }

    #[test]
    fn parses_a_response_action() {
        assert_eq!(
            parse_reply("<ai-harness-response>All done.</ai-harness-response>").unwrap(),
            Action::Response("All done.".into())
        );
    }

    #[test]
    fn tolerates_whitespace_around_and_inside_the_element() {
        assert_eq!(
            parse_reply("\n  <ai-harness-shell>  pwd  </ai-harness-shell>  \n").unwrap(),
            Action::Shell("pwd".into())
        );
    }

    #[test]
    fn shell_body_may_contain_angle_brackets() {
        // Redirections and pipes must survive parsing untouched.
        let raw = "<ai-harness-shell>grep -r foo . 2>&1 | head -5 > out.txt</ai-harness-shell>";
        assert_eq!(
            parse_reply(raw).unwrap(),
            Action::Shell("grep -r foo . 2>&1 | head -5 > out.txt".into())
        );
    }

    #[test]
    fn multiline_body_is_preserved() {
        let raw = "<ai-harness-shell>cd /tmp &&\nls</ai-harness-shell>";
        assert_eq!(
            parse_reply(raw).unwrap(),
            Action::Shell("cd /tmp &&\nls".into())
        );
    }

    #[test]
    fn rejects_empty_reply() {
        assert_eq!(parse_reply("   \n ").unwrap_err(), ProtocolError::Empty);
    }

    #[test]
    fn rejects_bare_prose() {
        assert!(matches!(
            parse_reply("Sure, I'll run ls for you.").unwrap_err(),
            ProtocolError::NotATag { .. }
        ));
    }

    #[test]
    fn rejects_prose_before_the_tag() {
        // The classic failure: a chatty preamble in front of a valid element.
        assert!(matches!(
            parse_reply("Sure! <ai-harness-shell>ls</ai-harness-shell>").unwrap_err(),
            ProtocolError::NotATag { .. }
        ));
    }

    #[test]
    fn rejects_prose_after_the_tag() {
        let err = parse_reply("<ai-harness-shell>ls</ai-harness-shell> Let me know!").unwrap_err();
        assert!(
            matches!(err, ProtocolError::TrailingContent { ref trailing, .. } if trailing == "Let me know!"),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_two_elements() {
        let raw =
            "<ai-harness-shell>ls</ai-harness-shell><ai-harness-response>hi</ai-harness-response>";
        assert!(matches!(
            parse_reply(raw).unwrap_err(),
            ProtocolError::TrailingContent { .. }
        ));
    }

    #[test]
    fn rejects_markdown_code_fences() {
        let raw = "```xml\n<ai-harness-shell>ls</ai-harness-shell>\n```";
        assert!(matches!(
            parse_reply(raw).unwrap_err(),
            ProtocolError::NotATag { .. }
        ));
    }

    #[test]
    fn rejects_unknown_tag() {
        let err = parse_reply("<ai-harness-think>hmm</ai-harness-think>").unwrap_err();
        assert_eq!(
            err,
            ProtocolError::UnknownTag {
                tag: "ai-harness-think".into()
            }
        );
    }

    #[test]
    fn rejects_the_query_tag_from_the_model() {
        assert_eq!(
            parse_reply("<ai-harness-query>hi</ai-harness-query>").unwrap_err(),
            ProtocolError::QueryTagFromModel
        );
    }

    #[test]
    fn rejects_missing_closing_tag() {
        assert_eq!(
            parse_reply("<ai-harness-shell>ls").unwrap_err(),
            ProtocolError::MissingClosingTag {
                tag: "ai-harness-shell".into()
            }
        );
    }

    #[test]
    fn rejects_mismatched_closing_tag() {
        // Closing with the wrong name means the right one is simply absent.
        assert_eq!(
            parse_reply("<ai-harness-shell>ls</ai-harness-response>").unwrap_err(),
            ProtocolError::MissingClosingTag {
                tag: "ai-harness-shell".into()
            }
        );
    }

    #[test]
    fn rejects_unterminated_open_tag() {
        assert_eq!(
            parse_reply("<ai-harness-shell").unwrap_err(),
            ProtocolError::UnterminatedOpenTag
        );
    }

    #[test]
    fn rejects_empty_body() {
        assert_eq!(
            parse_reply("<ai-harness-response>  </ai-harness-response>").unwrap_err(),
            ProtocolError::EmptyBody {
                tag: "ai-harness-response".into()
            }
        );
    }

    #[test]
    fn error_messages_name_the_expected_tags() {
        let text = ProtocolError::UnknownTag { tag: "foo".into() }.to_string();
        assert!(text.contains(SHELL_TAG), "got {text}");
        assert!(text.contains(RESPONSE_TAG), "got {text}");
    }

    #[test]
    fn snippet_truncates_on_a_char_boundary() {
        let long = "日".repeat(200);
        let out = snippet(&long); // must not panic on multi-byte input
        assert!(out.ends_with('…'));
    }

    #[test]
    fn system_prompt_documents_every_tag() {
        let prompt = system_prompt(None);
        assert!(prompt.contains(QUERY_TAG));
        assert!(prompt.contains(SHELL_TAG));
        assert!(prompt.contains(RESPONSE_TAG));
    }

    #[test]
    fn system_prompt_appends_operator_guidance() {
        let prompt = system_prompt(Some("Prefer ripgrep."));
        assert!(prompt.contains("Prefer ripgrep."));
        assert!(
            prompt.contains(SHELL_TAG),
            "protocol rules must still be present"
        );
    }

    #[test]
    fn system_prompt_ignores_blank_operator_guidance() {
        assert_eq!(system_prompt(Some("   ")), system_prompt(None));
    }

    /// Does a real model actually obey the contract? Opt-in:
    /// `cargo test -- --ignored live_ --nocapture`
    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY and makes real API calls"]
    async fn live_model_obeys_the_protocol() {
        use crate::openrouter::{Client, Message};

        let _ = dotenvy::dotenv();
        let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
        let model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| crate::config::DEFAULT_MODEL.to_string());
        let client = Client::new(key, model).unwrap();

        // A task needing the shell, and one answerable outright.
        let cases = [
            ("List the files in the current directory.", "shell-ish"),
            ("What is 2 + 2? Just answer.", "response-ish"),
        ];

        for (query, kind) in cases {
            let messages = vec![
                Message::system(system_prompt(None)),
                Message::user(encode_query(query)),
            ];
            let reply = client.complete(&messages).await.expect("live request");
            println!("[{kind}] {query}\n  raw: {:?}", reply.content);

            match parse_reply(&reply.content) {
                Ok(action) => println!("  parsed: {action:?}"),
                Err(err) => panic!(
                    "model broke the protocol on {kind:?}: {err}\nraw reply: {:?}",
                    reply.content
                ),
            }
        }
    }

    /// Does our correction actually get a real model back on track? We cannot
    /// make a model misbehave on demand, so the malformed turn is fabricated and
    /// the correction is the real one from `encode_correction`.
    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY and makes real API calls"]
    async fn live_model_recovers_from_a_correction() {
        use crate::openrouter::{Client, Message};

        let _ = dotenvy::dotenv();
        let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
        let model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| crate::config::DEFAULT_MODEL.to_string());
        let client = Client::new(key, model).unwrap();

        // Each case is a realistic way a model breaks the contract.
        let bad_replies = [
            "Sure! I'll list the files for you.",
            "```xml\n<ai-harness-shell>ls</ai-harness-shell>\n```",
            "<ai-harness-shell>ls</ai-harness-shell> Let me know what you see!",
        ];

        for bad in bad_replies {
            let error = parse_reply(bad).expect_err("fixture should be malformed");
            let messages = vec![
                Message::system(system_prompt(None)),
                Message::user(encode_query("List the files in the current directory.")),
                Message::assistant(bad.to_string()),
                Message::user(encode_correction(&error)),
            ];

            let reply = client.complete(&messages).await.expect("live request");
            println!("after {bad:?}\n  -> {:?}", reply.content);
            match parse_reply(&reply.content) {
                Ok(action) => println!("  recovered: {action:?}"),
                Err(err) => panic!(
                    "correction did not recover the model: {err}\nreply was: {:?}",
                    reply.content
                ),
            }
        }
    }

    /// The examples we show the model must themselves survive our parser.
    #[test]
    fn documented_examples_round_trip() {
        for (raw, expected) in [
            (
                format!("<{SHELL_TAG}>ls -la</{SHELL_TAG}>"),
                Action::Shell("ls -la".into()),
            ),
            (
                format!("<{RESPONSE_TAG}>All done.</{RESPONSE_TAG}>"),
                Action::Response("All done.".into()),
            ),
        ] {
            assert_eq!(parse_reply(&raw).unwrap(), expected);
        }
    }
}
