//! CLI arguments and environment configuration.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

pub const DEFAULT_MODEL: &str = "z-ai/glm-5.2";

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
    #[arg(long, default_value_t = 10, env = "AI_HARNESS_MAX_ITERATIONS")]
    pub max_iterations: usize,

    /// Start with debug mode on, showing raw protocol frames. Toggle with /debug.
    ///
    /// Also enabled by default in non-shipping (`dev`) builds; the flag forces it on
    /// even in release builds.
    #[arg(long, env = "AI_HARNESS_DEBUG")]
    pub debug: bool,

    /// Corrective retries allowed when a reply breaks the protocol.
    #[arg(long, default_value_t = crate::app::DEFAULT_MAX_RETRIES, env = "AI_HARNESS_MAX_RETRIES")]
    pub max_retries: usize,
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
