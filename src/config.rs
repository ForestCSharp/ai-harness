//! CLI arguments and environment configuration.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

pub const DEFAULT_MODEL: &str = "z-ai/glm-5.2";

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
    /// OpenRouter model slug, e.g. `z-ai/glm-5.2` or `anthropic/claude-sonnet-4.5`.
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

    /// Ask before each file read. Off by default: a read mutates nothing and
    /// cannot leave the working directory, so it runs without interrupting you.
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
}

impl Args {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.command_timeout.max(1))
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

    #[test]
    fn an_explicit_sessions_dir_is_used_verbatim() {
        let chosen = args(&["--sessions-dir", "/tmp/elsewhere"]);
        assert_eq!(
            chosen.sessions_dir(Path::new("/projects/thing")),
            PathBuf::from("/tmp/elsewhere"),
            "the escape hatch must not be re-rooted"
        );
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
