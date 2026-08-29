//! CLI arguments and environment configuration.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

pub const DEFAULT_MODEL: &str = "z-ai/glm-5.3-flash";

/// Everything the harness keeps for a project, under its working directory.
/// Sessions live here today; per-session plans and the like will join them.
pub const HARNESS_DIR: &str = ".ai_harness";

#[derive(Debug, Parser)]
#[command(
    name = "ai-harness",
    version,
    about = "A terminal harness for chatting with models via OpenRouter"
)]
pub struct Args {
    /// OpenRouter model slug, e.g. `z-ai/glm-5.3-flash` or `anthropic/claude-sonnet-4.5`.
    #[arg(short, long, env = "OPENROUTER_MODEL", default_value = DEFAULT_MODEL)]
    pub model: String,

    /// Extra guidance appended to the protocol system prompt (which is always sent).
    #[arg(short, long, env = "AI_HARNESS_SYSTEM_PROMPT")]
    pub system: Option<String>,

    /// Sandbox root. Commands run here and cannot write outside it.
    #[arg(short, long, env = "AI_HARNESS_WORKDIR")]
    pub workdir: Option<PathBuf>,

    /// Seconds before a command is killed.
    #[arg(long, default_value_t = 30, env = "AI_HARNESS_COMMAND_TIMEOUT")]
    pub command_timeout: u64,

    /// A command run after any turn that changed a file, whose failure goes back
    /// to the model.
    ///
    /// This is the harness's only way of asking whether a change *worked*.
    /// Without it a turn ends on the model's word; with it, `cargo check` or a
    /// lint gets the last say, and a failure becomes a result the model has to
    /// answer rather than a surprise you find later.
    ///
    /// Prefer something **fast**. It is paid on every turn that writes, and a
    /// full test suite turns a one-edit turn into a two-minute one. A type
    /// check, a lint, or one focused test target is the shape that pays for
    /// itself.
    ///
    /// One thing it does not cover: the trigger is a *write* or an *edit*, so a
    /// turn that changes files by running a shell command — `sed -i`,
    /// `cargo fix`, a codegen script — does not fire it. Including shell
    /// commands would fire it after `ls` and after most turns that change
    /// nothing at all.
    ///
    /// Unset, the harness infers one — see [`crate::check::detect`] — so a Cargo
    /// project is type-checked whether or not anyone remembered a flag. This
    /// overrides that inference.
    #[arg(long, env = "AI_HARNESS_CHECK")]
    pub check: Option<String>,

    /// Whether to send explicit prompt-cache breakpoints.
    ///
    /// The whole conversation is re-sent on every round-trip of an agentic turn,
    /// which is the shape prefix caching exists for. Providers differ on how to
    /// ask: most cache a repeated prefix on their own, Anthropic caches only
    /// what is marked. `auto` marks for the ones known to need it.
    ///
    /// `on` forces it for a provider that adopted the field after this was
    /// written; `off` is the way out if a model rejects it.
    #[arg(
        long,
        value_enum,
        default_value_t,
        env = "AI_HARNESS_CACHE_BREAKPOINTS"
    )]
    pub cache_breakpoints: crate::openrouter::CachePolicy,

    /// Do not check anything, whatever the project looks like.
    ///
    /// The opt-out for the inference above. Worth reaching for when the project's
    /// check is too slow to sit through per turn, when it needs an environment
    /// this machine does not have, or when you are working somewhere the check
    /// is meaningless — a scratch directory, a prose repository. The turn then
    /// ends on the model's word, which is what the whole feature exists to stop,
    /// so it is a choice rather than a default.
    #[arg(long, env = "AI_HARNESS_NO_CHECK")]
    pub no_check: bool,

    /// Seconds before a background job is killed, whatever it is doing.
    ///
    /// Not `--command-timeout`, which is an *idle* bound: a foreground command
    /// is killed after that long producing nothing, and the whole point of a job
    /// may be to sit quiet — a dev server that logs on startup and then waits is
    /// exactly the thing that bound would kill. So a job gets no idle bound at
    /// all and this wall-clock ceiling instead, well past any build worth
    /// waiting on and short of running until the machine is rebooted.
    #[arg(long, default_value_t = 3600, env = "AI_HARNESS_JOB_CEILING")]
    pub job_ceiling: u64,

    /// Maximum model round-trips per prompt, bounding the agentic loop.
    ///
    /// Reads consume a round-trip like any other action, so this has to leave
    /// room for gathering context as well as doing the work.
    #[arg(long, default_value_t = 100, env = "AI_HARNESS_MAX_ITERATIONS")]
    pub max_iterations: usize,

    /// Maximum bytes one prompt may add to the conversation.
    ///
    /// The size half of the loop budget: `--max-iterations` bounds round-trips,
    /// but a handful of whole-file reads can exhaust the context window well
    /// inside that. 0 disables the check.
    #[arg(long, default_value_t = 512 * 1024, env = "AI_HARNESS_MAX_TURN_BYTES")]
    pub max_turn_bytes: usize,

    /// Fraction of the model's context window at which the conversation is
    /// compacted automatically. 0 disables it; `/compact` still works.
    ///
    /// When the model's window is unknown — the catalog has not landed, the
    /// fetch failed, or the model is not in it — this falls back to a fixed
    /// byte size, since a fraction of an unknown number is not a threshold.
    #[arg(long, default_value_t = crate::app::DEFAULT_COMPACT_AT, env = "AI_HARNESS_COMPACT_AT")]
    pub compact_at: f64,

    /// Ask before each file read or search. Off by default: none of the three
    /// mutates anything or leaves the working directory, so they run without
    /// interrupting you.
    ///
    /// Searches share this flag rather than taking their own. The meaning is
    /// "ask before auto-approved local filesystem access", and a search is that
    /// — but a search modal would also tell you *less* than a read's: it can
    /// show the pattern, not which files the pattern will open.
    #[arg(long, env = "AI_HARNESS_CONFIRM_READS")]
    pub confirm_reads: bool,

    /// Ask before each URL fetch. Off by default, like reads.
    ///
    /// Turn it on to keep the network step under approval: an auto-approved
    /// read followed by an auto-approved fetch can move file contents off the
    /// machine without asking, and the https-only, no-private-addresses rules
    /// do not prevent that — they only stop a fetch reaching this machine or
    /// this network.
    #[arg(long, env = "AI_HARNESS_CONFIRM_FETCH")]
    pub confirm_fetch: bool,

    /// Run actions without the approval modal. Toggle with /auto.
    ///
    /// The sandbox still applies, so this changes *whether* you are asked, not
    /// where a command can reach. It does remove the structural check that a
    /// fetched page can only make the model *propose* something: under this,
    /// a proposal runs.
    #[arg(long, env = "AI_HARNESS_AUTO_APPROVE")]
    pub auto_approve: bool,

    /// Start with debug mode on, showing raw protocol frames. Toggle with /debug.
    ///
    /// Also enabled by default in non-shipping (`dev`) builds; the flag forces it on
    /// even in release builds.
    #[arg(long, env = "AI_HARNESS_DEBUG")]
    pub debug: bool,

    /// Corrective retries allowed when a reply breaks the protocol.
    #[arg(long, default_value_t = crate::app::DEFAULT_MAX_RETRIES, env = "AI_HARNESS_MAX_RETRIES")]
    pub max_retries: usize,

    /// How many turns of checkpoints to keep, for sessions started fresh.
    ///
    /// Unset keeps everything, which is the default: a checkpoint exists to be
    /// there when it is wanted, and how far back that is depends on the work.
    /// The value is a per-session setting once set with `/checkpoints <n>`, and
    /// a loaded session brings its own.
    #[arg(long, env = "AI_HARNESS_KEEP_CHECKPOINTS")]
    pub keep_checkpoints: Option<usize>,

    /// Do not show the model's reasoning while it streams. Toggle with
    /// `/reasoning`.
    ///
    /// On by default: a reasoning model can think for a minute, and a spinner
    /// is a worse answer to "what is it doing" than the text the API is already
    /// sending. The trace is never parsed, never sent back to the model, and
    /// never saved with the session — this governs whether it is drawn, not
    /// whether it arrives.
    #[arg(long, env = "AI_HARNESS_NO_REASONING")]
    pub no_reasoning: bool,

    /// Reject a reply that puts prose in front of an otherwise valid element,
    /// instead of dropping the prose and running the element.
    ///
    /// Off by default, so the prose is dropped. A narrated element is the
    /// commonest way a model breaks the contract and the least interesting: the
    /// action it wrote was right, and rejecting it costs a round-trip and a
    /// rollback to arrive back where it started. The recovery is narrow — the
    /// element still has to parse on its own, and every other violation is
    /// rejected as before — and it says so in the transcript each time, so drift
    /// stays visible. Turn this on to see the protocol enforced exactly.
    #[arg(long, env = "AI_HARNESS_STRICT_REPLIES")]
    pub strict_replies: bool,

    /// Let a reply end a turn without saying what to remember.
    ///
    /// The requirement is on by default because offering was not enough: a
    /// session that read seven files and summarised them kept nothing, which is
    /// what the whole memory system exists to stop. Requiring the *element*
    /// rather than a *note* makes the judgement mandatory every turn without
    /// making the note mandatory — `<ai-harness-memory/>` means "considered,
    /// nothing durable", and a model told it must produce a note will produce
    /// one whether or not there is anything to say.
    ///
    /// Turn it off if the corrective round-trips cost more than the notes are
    /// worth; `/stats` is how you tell.
    #[arg(long, env = "AI_HARNESS_NO_REQUIRE_MEMORY")]
    pub no_require_memory: bool,

    /// Start with one fresh session instead of reopening the ones that were
    /// open when the harness last quit in this project.
    ///
    /// Restoring is on by default: every session auto-saves, so quitting already
    /// loses nothing except *which* conversations you had going — and rebuilding
    /// that by hand through `/load`, once per session, is the tedious half of
    /// starting work again. The record is per project (see
    /// [`Args::sessions_dir`]), so this only ever reopens sessions belonging to
    /// the directory you launched in.
    #[arg(long, env = "AI_HARNESS_NO_RESTORE")]
    pub no_restore: bool,

    /// Directory holding session folders.
    ///
    /// Defaults to `.ai_harness/sessions` under the sandbox root, so sessions
    /// belong to the project being worked on rather than to whichever directory
    /// the harness happened to be launched from. Given explicitly, the path is
    /// used exactly as written.
    #[arg(long, env = "AI_HARNESS_SESSIONS_DIR")]
    pub sessions_dir: Option<PathBuf>,

    /// Input price in dollars per million tokens, for the `/cost` estimate.
    ///
    /// Rates differ per model and change over time, so they are supplied rather
    /// than baked in — a hardcoded table would go stale without anyone noticing.
    /// Both this and `--price-out` must be set for a cost figure to appear.
    #[arg(long, env = "AI_HARNESS_PRICE_IN")]
    pub price_in: Option<f64>,

    /// Output price in dollars per million tokens. See `--price-in`.
    #[arg(long, env = "AI_HARNESS_PRICE_OUT")]
    pub price_out: Option<f64>,

    /// Run one prompt with no terminal and exit, printing a JSON record.
    ///
    /// This is the mode a benchmark runner drives. There is no screen and nobody
    /// to answer the approval modal, so the run approves its own actions —
    /// `--auto-approve` is implied and cannot be turned off here, since a
    /// headless run that stopped to ask would simply hang until its timeout.
    /// Everything else about a turn is unchanged: the same contract, the same
    /// protocol, the same iteration budget, the same project check.
    #[arg(long, env = "AI_HARNESS_HEADLESS")]
    pub headless: bool,

    /// The prompt for a headless run. `-` reads it from stdin.
    ///
    /// Stdin is worth having because a task statement is often a paragraph with
    /// newlines and quoting in it, and passing that through a shell argument is
    /// how it gets mangled.
    #[arg(long, env = "AI_HARNESS_PROMPT")]
    pub prompt: Option<String>,

    /// Where a headless run writes its JSON record. Defaults to stdout.
    #[arg(long, env = "AI_HARNESS_HEADLESS_OUTPUT")]
    pub headless_output: Option<PathBuf>,

    /// Wall-clock ceiling on a headless run, in seconds.
    ///
    /// `--max-iterations` bounds round-trips and `--command-timeout` bounds one
    /// quiet command, but neither bounds the clock: a turn that keeps making
    /// progress slowly can outlive any benchmark harness willing to wait for it.
    /// A run stopped this way reports `timeout` rather than pretending to have
    /// finished. 0 disables it.
    #[arg(long, default_value_t = 1800, env = "AI_HARNESS_HEADLESS_TIMEOUT")]
    pub headless_timeout: u64,

    /// How commands are confined.
    ///
    /// `auto` is the sandbox this harness is built on and the only value worth
    /// using interactively. `none` exists for one situation: running inside a
    /// container that is *already* the isolation boundary, which is how every
    /// benchmark runner works. It is refused outside `--headless` — see
    /// [`Args::validate`].
    #[arg(long, value_enum, default_value_t, env = "AI_HARNESS_SANDBOX")]
    pub sandbox: SandboxMode,
}

/// How commands are confined. See [`Args::sandbox`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lower")]
pub enum SandboxMode {
    /// Confine commands with the platform sandbox, refusing to run if there is
    /// not one. The default, and the only thing an interactive session accepts.
    #[default]
    Auto,
    /// Run commands unconfined, because something outside the harness is
    /// providing the isolation.
    None,
}

impl Args {
    /// Reject flag combinations that cannot mean anything, before the terminal
    /// is taken over and before any work starts.
    ///
    /// The one that matters is `--sandbox=none`. The harness's containment
    /// story is that a command you approve is confined and a command you did not
    /// approve does not run; unconfined *and* self-approving is both halves gone
    /// at once. That combination is defensible when a container is the boundary
    /// instead, and indefensible at a prompt where the user believes the
    /// sandbox notice they saw at startup. So it is allowed exactly where the
    /// container assumption holds and refused everywhere else — structurally,
    /// rather than by documenting that you should not.
    pub fn validate(&self) -> Result<()> {
        if self.sandbox == SandboxMode::None && !self.headless {
            anyhow::bail!(
                "--sandbox=none is only accepted with --headless.\n\
                 It exists for running inside a container that already provides \
                 the isolation. Interactively there is nothing else confining \
                 the commands, so the harness refuses rather than running them \
                 unconfined."
            );
        }
        if self.headless && self.prompt.is_none() {
            anyhow::bail!("--headless needs --prompt <TEXT> (or --prompt - to read stdin)");
        }
        Ok(())
    }

    /// Wall-clock ceiling on a headless run, or `None` when disabled.
    pub fn headless_timeout(&self) -> Option<Duration> {
        (self.headless_timeout > 0).then(|| Duration::from_secs(self.headless_timeout))
    }

    /// The headless prompt, reading stdin when it is `-`.
    pub fn prompt_text(&self) -> Result<Option<String>> {
        let Some(prompt) = &self.prompt else {
            return Ok(None);
        };
        if prompt != "-" {
            return Ok(Some(prompt.clone()));
        }
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
            .context("reading the prompt from stdin")?;
        Ok(Some(text))
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.command_timeout.max(1))
    }

    /// Wall-clock ceiling on a background job. See [`Args::job_ceiling`].
    pub fn job_ceiling(&self) -> Duration {
        Duration::from_secs(self.job_ceiling.max(1))
    }

    /// Sandbox root, defaulting to the current directory.
    pub fn root(&self) -> Result<PathBuf> {
        match &self.workdir {
            Some(dir) => Ok(dir.clone()),
            None => std::env::current_dir().context("resolving the current directory"),
        }
    }

    /// Where session folders live, given the resolved sandbox root.
    ///
    /// Takes `root` rather than calling [`Args::root`] itself, so the caller's
    /// already-canonicalised root is the one used — the sandbox resolves it, and
    /// two different answers about where the project is would be worse than
    /// asking for it.
    pub fn sessions_dir(&self, root: &Path) -> PathBuf {
        self.sessions_dir
            .clone()
            .unwrap_or_else(|| root.join(HARNESS_DIR).join("sessions"))
    }

    /// Read the API key from the environment, with a message that says how to fix it.
    pub fn api_key() -> Result<String> {
        std::env::var("OPENROUTER_API_KEY")
            .context(
                "OPENROUTER_API_KEY is not set.\n\
                 Get a key at https://openrouter.ai/keys, then either export it:\n\
                 \n    export OPENROUTER_API_KEY=sk-or-...\n\n\
                 or put it in a .env file next to the binary.",
            )
            .and_then(|key| {
                if key.trim().is_empty() {
                    anyhow::bail!("OPENROUTER_API_KEY is set but empty");
                }
                Ok(key)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Parse args as if typed, with the binary name prepended.
    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["ai-harness"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).expect("args should parse")
    }

    #[test]
    fn sessions_default_to_the_harness_dir_under_the_root() {
        // Under the root, not the process cwd: sessions belong to the project
        // being worked on, not to whichever directory this was launched from.
        let root = Path::new("/projects/thing");
        assert_eq!(
            args(&[]).sessions_dir(root),
            root.join(".ai_harness").join("sessions")
        );
    }

    /// The containment rule the mode exists under. Unconfined *and*
    /// self-approving is both halves of the harness's safety story gone at once;
    /// it is defensible only where a container is the boundary instead, which is
    /// what `--headless` stands in for.
    #[test]
    fn unconfined_is_refused_outside_headless() {
        let err = args(&["--sandbox", "none"])
            .validate()
            .expect_err("interactive use must not be able to turn the sandbox off");
        assert!(
            err.to_string().contains("--headless"),
            "the message must say what would make it legal: {err}"
        );
    }

    #[test]
    fn unconfined_is_accepted_with_headless() {
        args(&["--sandbox", "none", "--headless", "--prompt", "hi"])
            .validate()
            .expect("a container-isolated run is the case this mode is for");
    }

    #[test]
    fn headless_needs_a_prompt() {
        let err = args(&["--headless"])
            .validate()
            .expect_err("a headless run with nothing to do is a hang waiting to happen");
        assert!(err.to_string().contains("--prompt"), "{err}");
    }

    /// The default has to stay the confined one: a flag nobody passed must never
    /// be the reason commands ran unsandboxed.
    #[test]
    fn the_sandbox_defaults_to_confined() {
        let default = args(&[]);
        assert_eq!(default.sandbox, SandboxMode::Auto);
        default.validate().expect("the default must be legal");
    }

    #[test]
    fn a_zero_headless_timeout_disables_the_deadline() {
        assert_eq!(args(&["--headless-timeout", "0"]).headless_timeout(), None);
        assert_eq!(
            args(&["--headless-timeout", "60"]).headless_timeout(),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn an_explicit_sessions_dir_is_used_verbatim() {
        let chosen = args(&["--sessions-dir", "/tmp/elsewhere"]);
        assert_eq!(
            chosen.sessions_dir(Path::new("/projects/thing")),
            PathBuf::from("/tmp/elsewhere"),
            "the escape hatch must not be re-rooted"
        );
    }

    /// Searches deliberately share `--confirm-reads` rather than taking a flag
    /// of their own; this records that as a decision rather than an oversight.
    #[test]
    fn one_flag_covers_every_auto_approved_filesystem_action() {
        assert!(!args(&[]).confirm_reads);
        assert!(args(&["--confirm-reads"]).confirm_reads);
        assert!(
            Args::try_parse_from(["ai-harness", "--confirm-search"]).is_err(),
            "a separate search flag would be a surface nobody finds"
        );
    }

    #[test]
    fn compaction_defaults_on_and_zero_turns_it_off() {
        assert_eq!(args(&[]).compact_at, crate::app::DEFAULT_COMPACT_AT);
        assert_eq!(args(&["--compact-at", "0"]).compact_at, 0.0);
        assert_eq!(args(&["--compact-at", "0.5"]).compact_at, 0.5);
    }

    /// Named for what turning it on gets you, like `--confirm-reads`, rather
    /// than for the recovery it disables.
    #[test]
    fn preamble_recovery_is_on_until_strictness_is_asked_for() {
        assert!(!args(&[]).strict_replies);
        assert!(args(&["--strict-replies"]).strict_replies);
    }

    /// Unset means "keep everything", not "keep none" — a checkpoint exists to
    /// be there when it is wanted.
    #[test]
    fn checkpoints_are_kept_unless_a_limit_is_given() {
        assert_eq!(args(&[]).keep_checkpoints, None);
        assert_eq!(args(&["--keep-checkpoints", "5"]).keep_checkpoints, Some(5));
    }

    #[test]
    fn reasoning_shows_until_it_is_turned_off() {
        assert!(!args(&[]).no_reasoning);
        assert!(args(&["--no-reasoning"]).no_reasoning);
    }

    #[test]
    fn the_root_follows_workdir_and_so_do_sessions() {
        let parsed = args(&["--workdir", "/projects/other"]);
        let root = parsed.root().unwrap();
        assert_eq!(root, PathBuf::from("/projects/other"));
        assert_eq!(
            parsed.sessions_dir(&root),
            Path::new("/projects/other")
                .join(".ai_harness")
                .join("sessions")
        );
    }
}
