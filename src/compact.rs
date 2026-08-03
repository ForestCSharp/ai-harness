//! Shortening a conversation that no longer fits.
//!
//! The whole conversation is resent on every turn, so a long session walks
//! steadily toward the model's context limit and then stops dead. Compaction is
//! the alternative to `/clear`: keep the recent exchange verbatim, and replace
//! the older part with something much smaller that still says what happened.
//!
//! Two passes, because they discard different things. The **mechanical** pass
//! throws away tool *output* — the contents of files read, the stdout of
//! commands run — while leaving every user prompt and assistant reply byte for
//! byte. That is where the bulk is, and it needs no judgement: a stub naming
//! the file is enough for a model to read it again. The **model** pass then
//! summarises what is left into prose, which is the only way to compress
//! reasoning rather than data.
//!
//! Everything here is a pure function of `&[Message]`. Nothing in this module
//! touches [`crate::app::App`], the filesystem, or the network — a [`Plan`] is
//! worked out and handed back, and only [`apply`] produces a new history. That
//! is what lets a cancelled or failed compaction leave the conversation exactly
//! as it was: until `apply` is called, nothing has happened.

use crate::openrouter::{Message, Role};
use crate::protocol;

/// Bytes of the most recent conversation kept verbatim whatever else happens.
///
/// One [`crate::files::MAX_READ_BYTES`], so the most recent whole-file read
/// always survives intact — it is usually the thing the next turn is about.
pub const KEEP_BYTES: usize = crate::files::MAX_READ_BYTES;

/// Messages kept verbatim even when one of them alone busts [`KEEP_BYTES`].
///
/// The floor that stops a single maximal read from evicting every recent
/// exchange behind it.
pub const KEEP_MESSAGES: usize = 6;

/// Fewest prefix messages worth a round-trip to summarise.
pub const MIN_PREFIX: usize = 8;

/// Below this saving, collapsing alone has not earned dropping the prefix.
pub const MIN_SAVING_BYTES: usize = 16 * 1024;

/// Why a compaction is happening. Reported to the user, and recorded in the
/// archive so a later reader knows whether it was chosen or forced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The conversation crossed the threshold on its own.
    Automatic,
    /// The user asked, with `/compact`.
    Manual,
    /// The provider refused the request as too long.
    Overflow,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::Automatic => "automatic",
            Reason::Manual => "manual",
            Reason::Overflow => "overflow",
        }
    }
}

/// What to do once the summary lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Then {
    /// Return to the prompt. The ordinary case.
    Idle,
    /// Send the conversation again — the request that overflowed, now shorter.
    Resend,
}

/// A compaction worked out but not yet applied.
///
/// Nothing here has touched `history`. That is the point: the summarising
/// request can fail, be refused, or be cancelled, and the conversation is
/// unchanged in every one of those cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// First index of the tail kept verbatim. Always at least 1, since
    /// `history[0]` is the contract.
    pub keep_from: usize,
    /// The prefix, with every collapsible result body replaced by a stub.
    pub collapsed: Vec<Message>,
    /// Whether a *successful* summary should replace the collapsed prefix
    /// rather than heading it.
    ///
    /// Set when collapsing alone saved too little to justify keeping the prefix
    /// — a conversation that is mostly prose. This is the only case in which a
    /// user's own words leave the conversation, which is why it takes a failing
    /// threshold rather than being the default, and why [`apply`] ignores it
    /// when there is no summary to put in their place.
    pub drop_prefix: bool,
    pub reason: Reason,
    pub before_len: usize,
    pub before_bytes: usize,
}

/// A parked compaction: the plan, the request that asks for its summary, and
/// what to do once it comes back.
///
/// Held by `App` and handed to the event loop, which is the only thing that may
/// start a request. Carrying the plan rather than a half-applied history is what
/// lets a cancel be a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub plan: Plan,
    pub request: Vec<Message>,
    pub then: Then,
}

/// Work out what to compact, or `None` when there is not enough to be worth it.
pub fn plan(history: &[Message], reason: Reason) -> Option<Plan> {
    let keep_from = tail_start(history);
    if keep_from < 1 + MIN_PREFIX {
        return None;
    }

    let prefix = &history[1..keep_from];
    let collapsed: Vec<Message> = prefix.iter().map(collapse).collect();

    let before_bytes: usize = prefix.iter().map(|m| m.content.len()).sum();
    let after_bytes: usize = collapsed.iter().map(|m| m.content.len()).sum();
    // A prefix that is mostly prose barely shrinks mechanically, and shortening
    // it by a few hundred bytes is not worth having kept it. Only then is the
    // summary allowed to stand in for it entirely.
    let drop_prefix = before_bytes.saturating_sub(after_bytes) < MIN_SAVING_BYTES;

    Some(Plan {
        keep_from,
        collapsed,
        drop_prefix,
        reason,
        before_len: history.len(),
        before_bytes: history.iter().map(|m| m.content.len()).sum(),
    })
}

/// Where the verbatim tail begins.
///
/// Bytes rather than a count of turns, because a message here runs from a
/// thirty-byte denial to a 64 KB read and a "turn" from one exchange to forty.
/// Bytes are what the context window actually charges for.
fn tail_start(history: &[Message]) -> usize {
    let mut bytes = 0usize;
    let mut start = history.len();
    for (index, message) in history.iter().enumerate().skip(1).rev() {
        let kept = history.len() - index;
        // Whichever keeps more: the byte budget, or the message floor.
        if bytes >= KEEP_BYTES && kept > KEEP_MESSAGES {
            break;
        }
        bytes += message.content.len();
        start = index;
    }

    // Prefer to cut at a turn boundary, so the tail opens with the user asking
    // for something rather than halfway through a tool loop whose beginning the
    // model can no longer see.
    let floor = start.saturating_sub(KEEP_MESSAGES).max(1);
    for index in (floor..start).rev() {
        if is_query(&history[index]) {
            return index;
        }
    }
    start.max(1)
}

fn is_query(message: &Message) -> bool {
    message.role == Role::User
        && message
            .content
            .starts_with(&format!("<{}>", protocol::QUERY_TAG))
}

/// One message with its result body stubbed, or unchanged if it is not one.
fn collapse(message: &Message) -> Message {
    match protocol::collapsible_result(&message.content) {
        Some((tag, head)) => Message::user(protocol::encode_compacted_result(tag, head)),
        None => message.clone(),
    }
}

/// The out-of-band request that asks the model to summarise what is going away.
///
/// The summariser sees the **collapsed** prefix, not the original. The detail is
/// what we have already decided to discard; what matters for continuity — the
/// user's words and the model's own reasoning — survives the mechanical pass
/// verbatim; and re-sending a full context window is the wrong thing to spend
/// money on at the exact moment the user has run out of room.
pub fn summary_request(plan: &Plan) -> Vec<Message> {
    let mut transcript = String::from("Transcript to summarise:\n\n");
    for message in &plan.collapsed {
        let speaker = match message.role {
            Role::Assistant => "assistant",
            // The contract never reaches here, but a system message in the
            // prefix would be harness narration either way.
            Role::User | Role::System => "user",
        };
        // A stub's note is written for the model that will read the compacted
        // history later. Repeating it here would be the same 170 bytes of
        // boilerplate once per collapsed result, in the one request we most
        // want to keep small — so the summariser gets the identifying line and
        // nothing else.
        match protocol::collapsible_result(&message.content) {
            Some((_, head)) => transcript.push_str(&format!("{speaker}: [{head}]\n\n")),
            None => transcript.push_str(&format!("{speaker}: {}\n\n", message.content)),
        }
    }

    vec![
        Message::system(protocol::compaction_prompt()),
        // Flattened into one message rather than replayed in their real roles:
        // a replayed conversation of protocol elements primes the model to
        // continue the protocol far harder than any instruction undoes.
        Message::user(transcript),
    ]
}

/// The history a finished compaction produces.
///
/// `summary` is `None` when the summarising request failed, was refused, or came
/// back as a protocol element instead of prose. The mechanical pass then stands
/// alone — and the prefix is kept regardless of [`Plan::drop_prefix`], because
/// deleting the user's own words with nothing in their place is the one outcome
/// worth refusing outright.
pub fn apply(history: &[Message], plan: &Plan, summary: Option<&str>) -> Vec<Message> {
    let mut out = Vec::with_capacity(history.len() + 1);
    out.push(history[0].clone());

    if let Some(summary) = summary {
        out.push(Message::user(protocol::encode_compaction(summary)));
    }
    if !(plan.drop_prefix && summary.is_some()) {
        out.extend(plan.collapsed.iter().cloned());
    }
    out.extend(history[plan.keep_from..].iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str) -> Message {
        Message::user(protocol::encode_query(text))
    }

    fn reply(text: &str) -> Message {
        Message::assistant(format!(
            "<{}>{text}</{}>",
            protocol::RESPONSE_TAG,
            protocol::RESPONSE_TAG
        ))
    }

    fn read_result(path: &str, bytes: usize) -> Message {
        let outcome = crate::files::ReadOutcome::whole_file(path, "x".repeat(bytes));
        Message::user(protocol::encode_read_result(&outcome))
    }

    /// A conversation long enough to be worth compacting: `pairs` exchanges,
    /// each a query, a big read result, and a reply.
    fn conversation(pairs: usize, read_bytes: usize) -> Vec<Message> {
        let mut history = vec![Message::system("CONTRACT")];
        for i in 0..pairs {
            history.push(query(&format!("question {i}")));
            history.push(read_result(&format!("src/f{i}.rs"), read_bytes));
            history.push(reply(&format!("answer {i}")));
        }
        history
    }

    #[test]
    fn the_contract_is_never_touched() {
        let history = conversation(12, 8 * 1024);
        let plan = plan(&history, Reason::Automatic).expect("worth compacting");
        assert!(plan.keep_from >= 1);
        for summary in [None, Some("a summary")] {
            let out = apply(&history, &plan, summary);
            assert_eq!(out[0], history[0], "summary: {summary:?}");
        }
    }

    #[test]
    fn a_short_conversation_has_nothing_to_compact() {
        assert_eq!(plan(&conversation(1, 100), Reason::Manual), None);
        assert_eq!(plan(&[Message::system("CONTRACT")], Reason::Manual), None);
    }

    #[test]
    fn a_tool_result_collapses_to_a_stub_naming_what_ran() {
        let history = conversation(12, 8 * 1024);
        let plan = plan(&history, Reason::Automatic).unwrap();
        let stub = plan
            .collapsed
            .iter()
            .find(|m| m.content.contains("src/f0.rs"))
            .expect("the first read should still be named");

        assert!(stub.content.contains("path: src/f0.rs"), "{}", stub.content);
        assert!(
            !stub.content.contains("xxxxxxxx"),
            "the contents must be gone: {}",
            stub.content
        );
        assert!(stub.content.contains("compacted"), "{}", stub.content);
        // Still a well-formed element, so the slot does not read as damage.
        assert!(
            stub.content
                .starts_with(&format!("<{}>", protocol::READ_RESULT_TAG))
        );
        assert!(stub.content.len() < 400, "a stub should be small");
    }

    #[test]
    fn user_prompts_and_assistant_replies_survive_verbatim() {
        let history = conversation(12, 8 * 1024);
        let plan = plan(&history, Reason::Automatic).unwrap();
        let out = apply(&history, &plan, None);
        let joined: String = out.iter().map(|m| m.content.clone()).collect();

        assert!(joined.contains("question 0"), "the user's words stay");
        assert!(joined.contains("answer 0"), "the model's words stay");
    }

    #[test]
    fn the_users_answer_to_a_question_is_never_collapsed() {
        let answer = Message::user(protocol::encode_option_result(&protocol::Answer::Chose(
            "Postgres".into(),
        )));
        let mut history = conversation(12, 8 * 1024);
        history.insert(2, answer.clone());

        let plan = plan(&history, Reason::Automatic).unwrap();
        assert!(
            plan.collapsed.contains(&answer),
            "the one result that came from a person must survive"
        );
    }

    #[test]
    fn an_earlier_compaction_block_is_not_collapsed_again() {
        let block = Message::user(protocol::encode_compaction("what happened before"));
        let mut history = conversation(12, 8 * 1024);
        history.insert(1, block.clone());

        let plan = plan(&history, Reason::Automatic).unwrap();
        assert!(
            plan.collapsed.contains(&block),
            "a summary is already compact"
        );
    }

    #[test]
    fn the_recent_tail_is_kept_verbatim() {
        let history = conversation(12, 8 * 1024);
        let plan = plan(&history, Reason::Automatic).unwrap();
        let out = apply(&history, &plan, None);

        // Whatever else happened, the last exchange is byte-identical.
        let last = history.len() - 1;
        assert_eq!(out[out.len() - 1], history[last]);
        assert_eq!(out[out.len() - 2], history[last - 1]);
        assert!(
            out[out.len() - 2].content.contains("xxxxxxxx"),
            "the most recent read keeps its contents"
        );
    }

    #[test]
    fn the_tail_starts_at_a_user_prompt() {
        let history = conversation(12, 8 * 1024);
        let plan = plan(&history, Reason::Automatic).unwrap();
        assert!(
            is_query(&history[plan.keep_from]),
            "the tail should open on a turn boundary, not mid-loop"
        );
    }

    /// One maximal read must not be able to evict every recent exchange.
    #[test]
    fn a_message_floor_survives_one_enormous_read() {
        let mut history = conversation(12, 1024);
        history.push(read_result("huge.rs", KEEP_BYTES * 2));
        let plan = plan(&history, Reason::Automatic).unwrap();

        assert!(
            history.len() - plan.keep_from >= KEEP_MESSAGES,
            "kept {} messages, floor is {KEEP_MESSAGES}",
            history.len() - plan.keep_from
        );
    }

    /// A long conversation with no tool output in it — nothing for the
    /// mechanical pass to take, so only a summary can shrink it.
    fn prose_conversation() -> Vec<Message> {
        let mut history = vec![Message::system("CONTRACT")];
        for i in 0..12 {
            history.push(query(&format!("question {i}")));
            history.push(reply(&format!("answer {i} {}", "prose ".repeat(2000))));
        }
        history
    }

    /// The one invariant worth stating outright: a failed summary must never
    /// cost the user their own words.
    #[test]
    fn a_failed_summary_never_drops_the_prefix() {
        let history = prose_conversation();
        let plan = plan(&history, Reason::Automatic).unwrap();
        assert!(plan.drop_prefix, "prose should not shrink mechanically");

        let out = apply(&history, &plan, None);
        let joined: String = out.iter().map(|m| m.content.clone()).collect();
        assert!(
            joined.contains("question 0"),
            "without a summary the prefix must stay: {joined}"
        );
    }

    #[test]
    fn a_prose_heavy_conversation_drops_the_prefix_only_with_a_summary() {
        let history = prose_conversation();
        let plan = plan(&history, Reason::Automatic).unwrap();

        let out = apply(&history, &plan, Some("they asked twelve things"));
        let joined: String = out.iter().map(|m| m.content.clone()).collect();
        assert!(joined.contains("they asked twelve things"));
        assert!(
            !joined.contains("question 0"),
            "with a summary standing in, the prefix goes"
        );
        assert!(out.len() < history.len());
    }

    #[test]
    fn the_summary_block_is_a_user_message_right_after_the_contract() {
        let history = conversation(12, 8 * 1024);
        let plan = plan(&history, Reason::Automatic).unwrap();
        let out = apply(&history, &plan, Some("a summary"));

        assert_eq!(out[1].role, Role::User, "System would survive /clear");
        assert!(
            out[1]
                .content
                .starts_with(&format!("<{}>", protocol::COMPACTION_TAG))
        );
    }

    #[test]
    fn the_summary_request_does_not_carry_the_protocol_contract() {
        let history = conversation(12, 8 * 1024);
        let plan = plan(&history, Reason::Automatic).unwrap();
        let request = summary_request(&plan);

        assert_eq!(request.len(), 2);
        assert_eq!(request[0].role, Role::System);
        assert!(
            request[0].content.contains("prose"),
            "{}",
            request[0].content
        );
        assert!(
            !request[0].content.contains(protocol::SHELL_TAG),
            "sending the contract would get an action back, not a summary"
        );
        assert_eq!(request[1].role, Role::User);
        assert!(request[1].content.contains("question 0"));
    }

    /// The stub's note is for the model reading the compacted history later.
    /// Sending it once per collapsed result would be pure boilerplate in the
    /// request we most want to keep small.
    #[test]
    fn the_summary_request_names_results_without_repeating_their_boilerplate() {
        let history = conversation(12, 8 * 1024);
        let plan = plan(&history, Reason::Automatic).unwrap();
        let input = &summary_request(&plan)[1].content;

        assert!(input.contains("[path: src/f0.rs]"), "still says what ran");
        assert!(
            !input.contains("Run it again if you need"),
            "the note must not be repeated per result: {input:.400}"
        );
    }

    #[test]
    fn compaction_actually_shrinks_the_conversation() {
        let history = conversation(12, 16 * 1024);
        let before: usize = history.iter().map(|m| m.content.len()).sum();
        let plan = plan(&history, Reason::Automatic).unwrap();

        let after: usize = apply(&history, &plan, Some("summary"))
            .iter()
            .map(|m| m.content.len())
            .sum();
        assert!(
            after < before / 2,
            "expected a real reduction, got {before} -> {after}"
        );
    }
}
