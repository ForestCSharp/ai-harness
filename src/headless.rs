//! What a headless run did, as JSON.
//!
//! A benchmark runner drives the harness with no terminal and no one to answer
//! the modal, and then has to be told what happened. This module is that
//! answer: one record per run, written to stdout or a file.
//!
//! Nothing here is stored while the run is going. Every number is derived at
//! the end from the transcript, the ledger, and the session directory — the
//! same rule [`crate::stats`] follows, and for the same reason: a count kept in
//! two places goes stale in one of them. It also means a record can be rebuilt
//! from a saved session long after the process has gone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::app::{App, Entry};
use crate::ledger::Ledger;
use crate::stats::{self, Actions};

/// Why the run stopped.
///
/// Every one of these is a real ending rather than an error to be retried, and
/// the runner needs to tell them apart: a `Budget` run says the harness gave up
/// where a `Complete` run says the model did, and scoring them the same way
/// would hide a bad iteration cap behind a low score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Exit {
    /// The model ended its turn with a response, which is the ordinary ending.
    Complete,
    /// `--max-iterations` ran out first.
    Budget,
    /// `--headless-timeout` ran out first.
    Timeout,
    /// Something failed that the loop could not carry on from.
    Error,
}

/// How the protocol went, counted from the transcript.
///
/// `Entry::Malformed` survives a retry: `App::roll_back_retries` prunes only
/// the model's copy of the conversation, deliberately leaving the transcript
/// holding every attempt. So the tally needs nothing kept during the run — the
/// evidence is already there when it ends.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ProtocolErrors {
    pub total: usize,
    /// Violations by the reason the parser gave, so a run that failed the same
    /// way nine times is distinguishable from one that failed nine ways once.
    pub by_reason: BTreeMap<String, usize>,
}

/// What the project check did, if there was one.
#[derive(Debug, Clone, Serialize)]
pub struct CheckRecord {
    pub command: Option<String>,
    pub runs: usize,
    /// The exit code of the *last* check, which is the one that had the final
    /// say over whether the turn was allowed to end.
    pub final_exit: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    pub exit: Exit,
    pub model: String,
    /// Model round-trips this turn spent, against `--max-iterations`.
    pub iterations: usize,
    pub wall_ms: u64,
    pub ledger: Ledger,
    pub actions: Actions,
    pub protocol_errors: ProtocolErrors,
    /// Questions the run declined on the user's behalf. Non-zero means the
    /// model wanted an answer nobody was there to give, which is worth seeing
    /// in a benchmark result rather than silently absorbing.
    pub questions_declined: usize,
    pub compactions: usize,
    pub check: CheckRecord,
    pub session_path: PathBuf,
    /// Whether commands ran without the sandbox because something outside the
    /// harness was providing the isolation. Recorded rather than assumed, so a
    /// result can never be read as confined when it was not.
    pub unconfined: bool,
}

impl RunRecord {
    pub fn build(
        app: &App,
        exit: Exit,
        wall_ms: u64,
        questions_declined: usize,
        unconfined: bool,
    ) -> Self {
        let session_path = app.sessions_dir().join(app.session_name());
        Self {
            exit,
            model: app.model.clone(),
            iterations: app.iterations,
            wall_ms,
            ledger: app.ledger.clone(),
            actions: stats::actions(&app.transcript),
            protocol_errors: protocol_errors(&app.transcript),
            questions_declined,
            compactions: compactions(&session_path),
            check: check_record(app),
            session_path,
            unconfined,
        }
    }

    /// The record as pretty JSON, which is what a runner reads and what a person
    /// ends up looking at when a run went wrong.
    pub fn to_json(&self) -> String {
        // The record is plain data with no map keys that can fail to serialise,
        // so the fallback is unreachable; it exists so a reporting path can
        // never be the thing that takes a benchmark run down.
        serde_json::to_string_pretty(self)
            .unwrap_or_else(|err| format!("{{\"error\":\"could not serialise record: {err}\"}}"))
    }
}

fn protocol_errors(transcript: &[Entry]) -> ProtocolErrors {
    let mut errors = ProtocolErrors::default();
    for entry in transcript {
        if let Entry::Malformed { reason, .. } = entry {
            errors.total += 1;
            *errors.by_reason.entry(reason.clone()).or_default() += 1;
        }
    }
    errors
}

/// How many times the conversation was compacted, counted from the session
/// directory rather than from anything the run kept.
///
/// A compaction writes `compaction-NNN.json` beside the session, so the files
/// are the record — the same argument background jobs are built on. A session
/// directory that is not there yet (a run that never autosaved) is zero rather
/// than an error: no directory means no compactions happened.
fn compactions(session_path: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(session_path) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("compaction-") && name.ends_with(".json"))
        })
        .count()
}

fn check_record(app: &App) -> CheckRecord {
    let mut runs = 0;
    let mut final_exit = None;
    for entry in &app.transcript {
        if let Entry::CheckResult(output) = entry {
            runs += 1;
            final_exit = output.exit_code;
        }
    }
    CheckRecord {
        command: app.check_command.clone(),
        runs,
        final_exit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::CommandOutput;

    fn output(exit: i32) -> Box<CommandOutput> {
        Box::new(CommandOutput {
            command: "cargo check".to_string(),
            exit_code: Some(exit),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            cancelled: false,
        })
    }

    #[test]
    fn malformed_replies_are_grouped_by_reason() {
        let transcript = vec![
            Entry::Malformed {
                reason: "two elements".to_string(),
                raw: String::new(),
            },
            Entry::Malformed {
                reason: "two elements".to_string(),
                raw: String::new(),
            },
            Entry::Malformed {
                reason: "unknown tag".to_string(),
                raw: String::new(),
            },
        ];
        let errors = protocol_errors(&transcript);
        assert_eq!(errors.total, 3);
        assert_eq!(errors.by_reason["two elements"], 2);
        assert_eq!(errors.by_reason["unknown tag"], 1);
    }

    #[test]
    fn a_clean_run_reports_no_protocol_errors() {
        let errors = protocol_errors(&[Entry::User("hello".to_string())]);
        assert_eq!(errors.total, 0);
        assert!(errors.by_reason.is_empty());
    }

    /// The *last* check is the one that decided whether the turn could end, so
    /// a run that failed twice and then passed reports zero — not the failure it
    /// recovered from.
    #[test]
    fn the_final_check_is_the_one_reported() {
        let mut app = App::new("m".into(), None, 10, std::env::temp_dir());
        app.check_command = Some("cargo check".to_string());
        app.transcript = vec![
            Entry::CheckResult(output(1)),
            Entry::CheckResult(output(1)),
            Entry::CheckResult(output(0)),
        ];
        let record = check_record(&app);
        assert_eq!(record.runs, 3);
        assert_eq!(record.final_exit, Some(0));
    }

    #[test]
    fn no_check_configured_reports_no_runs() {
        let app = App::new("m".into(), None, 10, std::env::temp_dir());
        let record = check_record(&app);
        assert_eq!(record.command, None);
        assert_eq!(record.runs, 0);
        assert_eq!(record.final_exit, None);
    }

    /// A session that never got as far as writing its directory is not an error
    /// to count: nothing was compacted, and saying so is the honest answer.
    #[test]
    fn a_missing_session_directory_counts_no_compactions() {
        assert_eq!(compactions(Path::new("/nonexistent/session")), 0);
    }
}
