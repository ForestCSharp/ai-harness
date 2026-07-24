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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandOutput {
    pub command: String,
    /// `None` when the process was killed by a signal or timed out.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    /// The user interrupted the command before it finished.
    pub cancelled: bool,
}

impl CommandOutput {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// A short status line for the transcript header.
    pub fn summary(&self) -> String {
        if self.cancelled {
            return "cancelled".to_string();
        }
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

/// The outcome of a sandboxed file write.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WriteOutcome {
    pub path: String,
    /// Bytes written on success.
    pub bytes: usize,
    /// Error message when the write failed (e.g. denied by the sandbox).
    pub error: Option<String>,
    pub timed_out: bool,
    pub cancelled: bool,
}

impl WriteOutcome {
    pub fn succeeded(&self) -> bool {
        self.error.is_none() && !self.timed_out && !self.cancelled
    }

    /// Success/failure for the model-facing result.
    pub fn as_result(&self) -> Result<usize, &str> {
        if self.cancelled {
            Err("cancelled")
        } else if self.timed_out {
            Err("timed out")
        } else if let Some(error) = &self.error {
            Err(error.as_str())
        } else {
            Ok(self.bytes)
        }
    }

    /// A short status line for the transcript header.
    pub fn summary(&self) -> String {
        if self.cancelled {
            "cancelled".to_string()
        } else if self.timed_out {
            "timed out".to_string()
        } else if self.error.is_some() {
            "failed".to_string()
        } else {
            format!("wrote {} bytes", self.bytes)
        }
    }
}

/// Run `script` inside `sandbox`, giving up after `timeout`, with no
/// cancellation. The app uses [`run_cancellable`]; this convenience wrapper is
/// exercised by the test suite.
#[cfg_attr(not(test), allow(dead_code))]
pub async fn run(sandbox: &Sandbox, script: &str, timeout: Duration) -> Result<CommandOutput> {
    // No cancellation: a future that never resolves.
    run_cancellable(sandbox, script, timeout, std::future::pending()).await
}

/// Like [`run`], but `cancel` resolving interrupts the command. On cancel the
/// process group is killed — reusing the same clean teardown as the timeout, so
/// grandchildren are reaped rather than orphaned.
pub async fn run_cancellable(
    sandbox: &Sandbox,
    script: &str,
    timeout: Duration,
    cancel: impl std::future::Future<Output = ()>,
) -> Result<CommandOutput> {
    let mut command = sandbox.command(script);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Its own process group, so a timeout or cancel can take down grandchildren
    // too. `Child::kill` would only reap the `sh` we spawned directly.
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

    tokio::pin!(cancel);
    tokio::select! {
        // The user interrupted before the command finished.
        _ = &mut cancel => {
            kill_group(pid);
            Ok(CommandOutput {
                command: script.to_string(),
                exit_code: None,
                stdout: String::new(),
                stderr: "command cancelled".to_string(),
                truncated: false,
                timed_out: false,
                cancelled: true,
            })
        }
        result = tokio::time::timeout(timeout, collect) => match result {
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
                    cancelled: false,
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
                    cancelled: false,
                })
            }
        }
    }
}

/// Write `contents` to `path` under the sandbox.
///
/// The write is driven with `tee` via argv (no shell), so the path cannot inject,
/// and Seatbelt confines it to the working directory regardless of the path — a
/// write outside the root comes back as an error, never an escape. The parent
/// directory is created first (also sandboxed).
pub async fn write_file(
    sandbox: &Sandbox,
    path: &str,
    contents: &str,
    timeout: Duration,
    cancel: impl std::future::Future<Output = ()>,
) -> Result<WriteOutcome> {
    let outcome = |error: Option<String>| WriteOutcome {
        path: path.to_string(),
        bytes: if error.is_none() { contents.len() } else { 0 },
        error,
        timed_out: false,
        cancelled: false,
    };

    // Create the parent directory if the path has one. `mkdir -p` is idempotent
    // and, being sandboxed, cannot create anything outside the root.
    if let Some(parent) = std::path::Path::new(path).parent() {
        let parent = parent.to_string_lossy();
        if !parent.is_empty() && parent != "." {
            let mkdir = sandbox.program("/bin/mkdir", &["-p", "--", &parent]);
            if let Err(e) = run_program(mkdir, None).await? {
                return Ok(outcome(Some(format!("could not create {parent}: {e}"))));
            }
        }
    }

    // `tee` writes stdin to the file; its stdout echo is discarded.
    let tee = sandbox.program("/usr/bin/tee", &["--", path]);
    tokio::pin!(cancel);
    tokio::select! {
        _ = &mut cancel => Ok(WriteOutcome { cancelled: true, ..outcome(None) }),
        result = tokio::time::timeout(timeout, run_program(tee, Some(contents))) => match result {
            Ok(run) => match run? {
                Ok(()) => Ok(outcome(None)),
                Err(stderr) => Ok(outcome(Some(stderr))),
            },
            Err(_) => Ok(WriteOutcome { timed_out: true, ..outcome(None) }),
        },
    }
}

/// Spawn a sandboxed program, optionally feeding `stdin`, and wait for it.
/// Returns `Ok(())` on a clean exit, or `Err(stderr-or-status)` otherwise.
async fn run_program(
    mut command: tokio::process::Command,
    stdin: Option<&str>,
) -> Result<std::result::Result<(), String>> {
    use tokio::io::AsyncWriteExt;

    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().context("spawning sandboxed program")?;

    if let Some(bytes) = stdin {
        // Write and close stdin so `tee` sees EOF. A dropped pipe (child already
        // gone) is not fatal — the exit status below is what matters.
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(bytes.as_bytes()).await;
            let _ = pipe.shutdown().await;
        }
    }

    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr).await;
    }
    let status = child
        .wait()
        .await
        .context("waiting for sandboxed program")?;

    if status.success() {
        Ok(Ok(()))
    } else {
        let message = String::from_utf8_lossy(&stderr).trim().to_string();
        let message = if message.is_empty() {
            format!("exited with {status}")
        } else {
            message
        };
        Ok(Err(message))
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

    async fn write(sandbox: &Sandbox, path: &str, contents: &str) -> WriteOutcome {
        write_file(sandbox, path, contents, secs(10), std::future::pending())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn write_file_creates_a_file_with_exact_contents() {
        let (sandbox, dir) = sandbox_in("write-file");
        let body = "line one\nline two\n"; // trailing newline must survive
        let out = write(&sandbox, "out.txt", body).await;
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(out.bytes, body.len());
        assert_eq!(std::fs::read_to_string(dir.join("out.txt")).unwrap(), body);
        let _ = std::fs::remove_file(dir.join("out.txt"));
    }

    #[tokio::test]
    async fn write_file_creates_missing_parent_directories() {
        let (sandbox, dir) = sandbox_in("write-mkdir");
        let out = write(&sandbox, "a/b/c/deep.txt", "hi").await;
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("a/b/c/deep.txt")).unwrap(),
            "hi"
        );
        let _ = std::fs::remove_dir_all(dir.join("a"));
    }

    #[tokio::test]
    async fn write_file_handles_a_path_with_spaces() {
        // The path is an argv element, so spaces are literal — no shell parsing.
        let (sandbox, dir) = sandbox_in("write-spaces");
        let out = write(&sandbox, "my notes.md", "# hi").await;
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("my notes.md")).unwrap(),
            "# hi"
        );
        let _ = std::fs::remove_file(dir.join("my notes.md"));
    }

    #[tokio::test]
    async fn write_outside_the_root_is_denied() {
        let (sandbox, _dir) = sandbox_in("write-escape");
        let target = std::env::temp_dir().join("ai-harness-WRITE-ESCAPE.txt");
        let _ = std::fs::remove_file(&target);
        let out = write(&sandbox, target.to_str().unwrap(), "pwned").await;
        assert!(!out.succeeded(), "escape must fail: {out:?}");
        assert!(!target.exists(), "the file escaped the sandbox");
    }

    #[tokio::test]
    async fn a_path_that_looks_like_a_flag_is_not_treated_as_one() {
        // `--` before the path means tee reads "-a" as a filename, not a flag.
        let (sandbox, dir) = sandbox_in("write-dashflag");
        let out = write(&sandbox, "-a.txt", "x").await;
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(std::fs::read_to_string(dir.join("-a.txt")).unwrap(), "x");
        let _ = std::fs::remove_file(dir.join("-a.txt"));
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
    async fn cancel_stops_the_command_promptly() {
        let (sandbox, _dir) = sandbox_in("cancel");
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        // Fire the cancel shortly after the command starts.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = cancel_tx.send(());
        });

        let started = std::time::Instant::now();
        let out = run_cancellable(&sandbox, "sleep 60", secs(60), async {
            let _ = cancel_rx.await;
        })
        .await
        .unwrap();

        assert!(out.cancelled, "{out:?}");
        assert!(!out.timed_out);
        assert_eq!(out.summary(), "cancelled");
        assert!(
            started.elapsed() < secs(5),
            "cancel did not fire promptly (took {:?})",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn cancel_kills_grandchildren_too() {
        let (sandbox, _dir) = sandbox_in("cancel-pgroup");
        let marker = "ai-harness-cancel-marker";
        let script = format!("sh -c 'sleep 30 # {marker}' & sleep 30");
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = cancel_tx.send(());
        });

        let out = run_cancellable(&sandbox, &script, secs(60), async {
            let _ = cancel_rx.await;
        })
        .await
        .unwrap();
        assert!(out.cancelled);

        tokio::time::sleep(Duration::from_millis(400)).await;
        let survivors = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .output()
            .unwrap();
        assert!(
            survivors.stdout.is_empty(),
            "grandchild survived the cancel: {}",
            String::from_utf8_lossy(&survivors.stdout)
        );
    }

    #[tokio::test]
    async fn a_command_that_finishes_before_cancel_is_not_marked_cancelled() {
        let (sandbox, _dir) = sandbox_in("cancel-race");
        // Cancel never fires; the command completes normally.
        let out = run_cancellable(&sandbox, "echo done", secs(10), std::future::pending())
            .await
            .unwrap();
        assert!(!out.cancelled);
        assert!(out.succeeded());
        assert_eq!(out.stdout.trim(), "done");
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
