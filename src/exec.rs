//! Running sandboxed commands.
//!
//! The sandbox confines the filesystem; it does not bound time, output, or
//! process count. Those limits live here:
//!
//! - stdin is `/dev/null`, so interactive commands fail fast instead of hanging,
//! - the child gets its own process group, killed wholesale on timeout,
//! - combined output is capped, since a runaway command would otherwise exhaust
//!   memory and the model's token budget alike.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;

use crate::sandbox::Sandbox;

/// Cap on captured output per stream. Anything past this is dropped with a marker.
pub const MAX_STREAM_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub command: String,
    /// `None` when the process was killed by a signal or timed out.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
}

impl CommandOutput {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// A short status line for the transcript header.
    pub fn summary(&self) -> String {
        if self.timed_out {
            return "timed out".to_string();
        }
        match self.exit_code {
            Some(0) => "exit 0".to_string(),
            Some(code) => format!("exit {code}"),
            None => "killed".to_string(),
        }
    }
}

/// Run `script` inside `sandbox`, giving up after `timeout`.
pub async fn run(sandbox: &Sandbox, script: &str, timeout: Duration) -> Result<CommandOutput> {
    let mut command = sandbox.command(script);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Its own process group, so a timeout can take down grandchildren too.
    // `Child::kill` would only reap the `sh` we spawned directly.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawning sandboxed command: {script}"))?;
    let pid = child.id();

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");

    // Drain both pipes while waiting. Without this a command producing more than
    // a pipe buffer of output would block forever and only surface as a timeout.
    let collect = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (a, b) = tokio::join!(
            read_capped(&mut stdout_pipe, &mut out),
            read_capped(&mut stderr_pipe, &mut err),
        );
        let status = child.wait().await;
        (out, err, a, b, status)
    };

    match tokio::time::timeout(timeout, collect).await {
        Ok((out, err, a, b, status)) => {
            let truncated = a.context("reading stdout")? || b.context("reading stderr")?;
            let status = status.context("waiting for sandboxed command")?;
            Ok(CommandOutput {
                command: script.to_string(),
                exit_code: status.code(),
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
                truncated,
                timed_out: false,
            })
        }
        Err(_) => {
            kill_group(pid);
            Ok(CommandOutput {
                command: script.to_string(),
                exit_code: None,
                stdout: String::new(),
                stderr: format!("command exceeded the {}s timeout", timeout.as_secs()),
                truncated: false,
                timed_out: true,
            })
        }
    }
}

/// Read until EOF or the cap. Returns whether output was truncated.
///
/// Reading continues past the cap and discards the excess, so the child never
/// blocks on a full pipe — it just stops being recorded.
async fn read_capped<R>(reader: &mut R, sink: &mut Vec<u8>) -> Result<bool>
where
    R: AsyncReadExt + Unpin,
{
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            return Ok(truncated);
        }
        if sink.len() < MAX_STREAM_BYTES {
            let room = MAX_STREAM_BYTES - sink.len();
            let take = room.min(n);
            sink.extend_from_slice(&buffer[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
}

/// Kill the child's whole process group.
#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // Negative pid targets the process group created via `process_group(0)`.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn sandbox_in(name: &str) -> (Sandbox, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ai-harness-exec-{name}"));
        let _ = std::fs::create_dir_all(&dir);
        let dir = std::fs::canonicalize(&dir).unwrap();
        (Sandbox::new(&dir).unwrap(), dir)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[tokio::test]
    async fn runs_a_simple_command() {
        let (sandbox, _dir) = sandbox_in("simple");
        let out = run(&sandbox, "echo hello", secs(10)).await.unwrap();
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(out.stdout.trim(), "hello");
        assert!(!out.truncated);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn captures_exit_code_and_stderr() {
        let (sandbox, _dir) = sandbox_in("exitcode");
        let out = run(&sandbox, "echo oops >&2; exit 3", secs(10))
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert_eq!(out.stderr.trim(), "oops");
        assert!(!out.succeeded());
        assert_eq!(out.summary(), "exit 3");
    }

    #[tokio::test]
    async fn writes_inside_the_root_are_allowed() {
        let (sandbox, dir) = sandbox_in("write-ok");
        let out = run(
            &sandbox,
            "echo inside > allowed.txt && cat allowed.txt",
            secs(10),
        )
        .await
        .unwrap();
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(out.stdout.trim(), "inside");
        let _ = std::fs::remove_file(dir.join("allowed.txt"));
    }

    #[tokio::test]
    async fn parent_directory_writes_are_denied() {
        let (sandbox, dir) = sandbox_in("escape-parent");
        let out = run(&sandbox, "echo pwned > ../ai-harness-ESCAPED.txt", secs(10))
            .await
            .unwrap();
        assert!(!out.succeeded(), "escape must fail: {out:?}");
        let escaped = dir.parent().unwrap().join("ai-harness-ESCAPED.txt");
        assert!(!escaped.exists(), "file escaped the sandbox at {escaped:?}");
    }

    #[tokio::test]
    async fn absolute_path_writes_outside_the_root_are_denied() {
        let (sandbox, _dir) = sandbox_in("escape-abs");
        let target = std::env::temp_dir().join("ai-harness-ABS-ESCAPE.txt");
        let _ = std::fs::remove_file(&target);
        let script = format!("echo pwned > {}", target.display());
        let out = run(&sandbox, &script, secs(10)).await.unwrap();
        assert!(!out.succeeded(), "escape must fail: {out:?}");
        assert!(!target.exists(), "file escaped the sandbox");
    }

    #[tokio::test]
    async fn home_directory_writes_are_denied() {
        let (sandbox, _dir) = sandbox_in("escape-home");
        let out = run(&sandbox, "echo pwned > ~/.ai-harness-pwned", secs(10))
            .await
            .unwrap();
        assert!(!out.succeeded(), "escape must fail: {out:?}");
        if let Some(home) = std::env::var_os("HOME") {
            let path = std::path::Path::new(&home).join(".ai-harness-pwned");
            assert!(!path.exists(), "file escaped into HOME");
        }
    }

    #[tokio::test]
    async fn secret_locations_are_unreadable() {
        let (sandbox, _dir) = sandbox_in("secrets");
        let out = run(&sandbox, "cat ~/.ssh/* 2>&1 | head -5", secs(10))
            .await
            .unwrap();
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(
            !combined.contains("PRIVATE KEY"),
            "ssh key material leaked: {combined}"
        );
    }

    #[tokio::test]
    async fn dotenv_in_the_root_is_unreadable() {
        let (sandbox, dir) = sandbox_in("dotenv");
        std::fs::write(dir.join(".env"), "SECRET_TOKEN=supersecret\n").unwrap();
        let out = run(&sandbox, "cat .env", secs(10)).await.unwrap();
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(
            !combined.contains("supersecret"),
            "the key file must stay unreadable: {combined}"
        );
        let _ = std::fs::remove_file(dir.join(".env"));
    }

    #[tokio::test]
    async fn timeout_kills_the_command() {
        let (sandbox, _dir) = sandbox_in("timeout");
        let started = std::time::Instant::now();
        let out = run(&sandbox, "sleep 60", Duration::from_millis(800))
            .await
            .unwrap();
        assert!(out.timed_out, "{out:?}");
        assert_eq!(out.summary(), "timed out");
        assert!(
            started.elapsed() < secs(10),
            "timeout did not fire promptly"
        );
    }

    #[tokio::test]
    async fn timeout_kills_grandchildren_too() {
        let (sandbox, _dir) = sandbox_in("pgroup");
        let marker = "ai-harness-pgroup-marker";
        // The outer sh exits immediately; the marker process must still be reaped.
        let script = format!("sh -c 'sleep 30 # {marker}' & sleep 30");
        let out = run(&sandbox, &script, Duration::from_millis(800))
            .await
            .unwrap();
        assert!(out.timed_out);

        tokio::time::sleep(Duration::from_millis(400)).await;
        let survivors = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .output()
            .unwrap();
        assert!(
            survivors.stdout.is_empty(),
            "grandchild survived the timeout: {}",
            String::from_utf8_lossy(&survivors.stdout)
        );
    }

    #[tokio::test]
    async fn large_output_is_truncated_without_hanging() {
        let (sandbox, _dir) = sandbox_in("truncate");
        let out = run(&sandbox, "yes ABCDEFGH | head -c 5000000", secs(30))
            .await
            .unwrap();
        assert!(out.truncated, "expected truncation");
        assert!(
            out.stdout.len() <= MAX_STREAM_BYTES,
            "captured {} bytes, cap is {MAX_STREAM_BYTES}",
            out.stdout.len()
        );
    }

    #[tokio::test]
    async fn stdin_is_closed_so_interactive_commands_do_not_hang() {
        let (sandbox, _dir) = sandbox_in("stdin");
        // Would block forever on a terminal; with /dev/null it hits EOF at once.
        let out = run(&sandbox, "cat", secs(5)).await.unwrap();
        assert!(!out.timed_out, "stdin should be /dev/null, not a tty");
        assert!(out.succeeded());
    }

    #[tokio::test]
    async fn network_is_permitted() {
        let (sandbox, _dir) = sandbox_in("network");
        let out = run(
            &sandbox,
            "curl -s -m 10 -o /dev/null -w '%{http_code}' https://example.com",
            secs(20),
        )
        .await
        .unwrap();
        assert_eq!(
            out.stdout.trim(),
            "200",
            "network should be allowed: {out:?}"
        );
    }
}
