//! Running sandboxed commands.
//!
//! The sandbox confines the filesystem; it does not bound time, output, or
//! process count. Those limits live here:
//!
//! - stdin is `/dev/null`, so a command that wants to be typed at fails fast
//!   instead of hanging,
//! - the child gets its own process group, killed wholesale on timeout,
//! - combined output is capped, since a runaway command would otherwise exhaust
//!   memory and the model's token budget alike.
//!
//! Output is read incrementally and forwarded as it arrives, which is what lets
//! the caller show a command running rather than only its corpse. The recorded
//! text is still capped; forwarding is a second consumer of the same reads, not
//! a second buffer.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use crate::sandbox::Sandbox;

/// Cap on captured output per stream. Anything past this is dropped with a marker.
pub const MAX_STREAM_BYTES: usize = 32 * 1024;

/// Multiple of the idle timeout at which a command is killed regardless of how
/// busy it looks.
///
/// The timeout below is an *idle* bound — it resets on every read and write — so
/// a command that prints forever would otherwise never hit it. Ten minutes at
/// the default, which is far past anything worth waiting on unattended.
pub const HARD_TIMEOUT_MULTIPLE: u32 = 20;

/// Which pipe a chunk of live output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// A piece of output, forwarded while the command is still running.
///
/// Display-only. The authoritative text is the [`CommandOutput`] returned at the
/// end — the same split streamed model replies use, and for the same reason:
/// nothing should act on a half-arrived buffer.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub stream: Stream,
    pub text: String,
}

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
/// cancellation. The app uses [`run_streaming`]; this convenience wrapper and
/// [`run_cancellable`] beneath it are exercised only by the test suite.
///
/// Dead for two separate reasons, so the condition names both: outside a test
/// build nothing calls them, and inside one on a platform other than macOS
/// their callers go away with the gated test modules.
#[cfg_attr(any(not(test), not(target_os = "macos")), allow(dead_code))]
pub async fn run(sandbox: &Sandbox, script: &str, timeout: Duration) -> Result<CommandOutput> {
    // No cancellation: a future that never resolves.
    run_cancellable(sandbox, script, timeout, std::future::pending()).await
}

/// Like [`run`], but `cancel` resolving interrupts the command. On cancel the
/// process group is killed — reusing the same clean teardown as the timeout, so
/// grandchildren are reaped rather than orphaned.
///
/// Non-streaming: the convenience shape for callers that only want the finished
/// output. [`run_streaming`] does the work.
#[cfg_attr(any(not(test), not(target_os = "macos")), allow(dead_code))]
pub async fn run_cancellable(
    sandbox: &Sandbox,
    script: &str,
    timeout: Duration,
    cancel: impl std::future::Future<Output = ()>,
) -> Result<CommandOutput> {
    // A sender whose receiver is dropped immediately: sends fail and are
    // ignored, which is exactly "nobody is watching".
    let (tx, _) = mpsc::channel(1);
    run_streaming(sandbox, script, timeout, cancel, tx).await
}

/// Run `script`, forwarding output as it arrives.
///
/// `timeout` is an **idle** bound: it resets whenever output is read, so a
/// command is killed after that long doing nothing rather than that long in
/// total. A build that prints progress for two minutes survives; a silent one
/// still dies on schedule. [`HARD_TIMEOUT_MULTIPLE`] bounds the total regardless,
/// so a command that prints forever cannot run forever.
///
/// stdin is `/dev/null`, so a command that wants to be typed at fails fast
/// instead of hanging.
pub async fn run_streaming(
    sandbox: &Sandbox,
    script: &str,
    timeout: Duration,
    cancel: impl std::future::Future<Output = ()>,
    output: mpsc::Sender<Chunk>,
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

    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut truncated = false;
    // Set once a pipe hits EOF, so its arm stops being polled — a closed pipe
    // reads 0 forever and would spin the loop.
    let (mut out_done, mut err_done) = (false, false);
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];

    let started = tokio::time::Instant::now();
    let hard_deadline = started + timeout * HARD_TIMEOUT_MULTIPLE;
    let idle = tokio::time::sleep(timeout);
    tokio::pin!(idle);
    tokio::pin!(cancel);

    let finished = loop {
        tokio::select! {
            // Cancel and the deadlines are checked first so a command that
            // floods its pipes cannot starve them.
            biased;

            _ = &mut cancel => {
                kill_group(pid);
                return Ok(CommandOutput {
                    stderr: "command cancelled".to_string(),
                    cancelled: true,
                    ..empty(script)
                });
            }

            _ = &mut idle => {
                kill_group(pid);
                return Ok(CommandOutput {
                    stderr: format!(
                        "command produced nothing for {}s and was killed",
                        timeout.as_secs()
                    ),
                    timed_out: true,
                    ..empty(script)
                });
            }

            _ = tokio::time::sleep_until(hard_deadline) => {
                kill_group(pid);
                return Ok(CommandOutput {
                    stderr: format!(
                        "command ran longer than the {}s ceiling and was killed",
                        (timeout * HARD_TIMEOUT_MULTIPLE).as_secs()
                    ),
                    timed_out: true,
                    ..empty(script)
                });
            }

            read = stdout_pipe.read(&mut out_buf), if !out_done => {
                match read.context("reading stdout")? {
                    0 => out_done = true,
                    n => {
                        truncated |= record(&mut out, &out_buf[..n]);
                        forward(&output, Stream::Stdout, &out_buf[..n]).await;
                        idle.as_mut().reset(tokio::time::Instant::now() + timeout);
                    }
                }
            }

            read = stderr_pipe.read(&mut err_buf), if !err_done => {
                match read.context("reading stderr")? {
                    0 => err_done = true,
                    n => {
                        truncated |= record(&mut err, &err_buf[..n]);
                        forward(&output, Stream::Stderr, &err_buf[..n]).await;
                        idle.as_mut().reset(tokio::time::Instant::now() + timeout);
                    }
                }
            }

            status = child.wait() => break status.context("waiting for sandboxed command")?,
        }
    };

    // The child is gone, but its pipes may still hold buffered output. Drain
    // what is left, or the last lines of a fast command would be lost.
    if !out_done {
        truncated |= drain(&mut stdout_pipe, &mut out, &output, Stream::Stdout).await?;
    }
    if !err_done {
        truncated |= drain(&mut stderr_pipe, &mut err, &output, Stream::Stderr).await?;
    }

    Ok(CommandOutput {
        command: script.to_string(),
        exit_code: finished.code(),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        truncated,
        timed_out: false,
        cancelled: false,
    })
}

/// A `CommandOutput` for a run that produced nothing usable.
fn empty(script: &str) -> CommandOutput {
    CommandOutput {
        command: script.to_string(),
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
        timed_out: false,
        cancelled: false,
    }
}

/// Append to the capped buffer. Returns whether anything was dropped.
///
/// Past the cap the bytes are discarded but still read, so the child never
/// blocks on a full pipe — it just stops being recorded.
fn record(sink: &mut Vec<u8>, bytes: &[u8]) -> bool {
    if sink.len() >= MAX_STREAM_BYTES {
        return true;
    }
    let room = MAX_STREAM_BYTES - sink.len();
    let take = room.min(bytes.len());
    sink.extend_from_slice(&bytes[..take]);
    take < bytes.len()
}

/// Hand a chunk to the watcher, if there is one.
///
/// A send failure means nobody is listening — the non-streaming path, or a
/// cancelled turn — which is not an error worth propagating.
async fn forward(output: &mpsc::Sender<Chunk>, stream: Stream, bytes: &[u8]) {
    if output.is_closed() {
        return;
    }
    let _ = output
        .send(Chunk {
            stream,
            text: String::from_utf8_lossy(bytes).into_owned(),
        })
        .await;
}

/// Read a pipe to EOF after the child has exited.
async fn drain<R>(
    reader: &mut R,
    sink: &mut Vec<u8>,
    output: &mpsc::Sender<Chunk>,
    stream: Stream,
) -> Result<bool>
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
        truncated |= record(sink, &buffer[..n]);
        forward(output, stream, &buffer[..n]).await;
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

/// Kill the child's whole process group.
///
/// `pub(crate)` for [`crate::jobs::kill`], which kills by the pid a job recorded
/// rather than by a handle it holds — the harness that stops a job is not always
/// the task that started it.
#[cfg(unix)]
pub(crate) fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // Negative pid targets the process group created via `process_group(0)`.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_group(_pid: Option<u32>) {}

/// Start a job: run `script`, appending its output to the files in `handle`, and
/// return once the child is spawned.
///
/// The differences from [`run_streaming`] are the whole point of a job, and there
/// are only two.
///
/// **No idle bound.** A foreground command is killed after `timeout` producing
/// nothing, which is right for work someone is waiting on and wrong for a dev
/// server — the thing you most want to leave running is the thing that prints
/// nothing for an hour. `ceiling` is wall-clock and bounds the whole run instead.
///
/// **Output goes to files, not a channel.** There is no live window to feed and
/// no turn to feed it into; the log *is* the record, and the model reads it with
/// `<ai-harness-read>` like any other file. Each stream is still capped at
/// [`MAX_STREAM_BYTES`], so a runaway job cannot fill the disk — past the cap the
/// bytes are read and dropped, exactly as [`record`] does, so the child never
/// blocks on a full pipe.
///
/// Everything else is shared with a foreground command deliberately: `/dev/null`
/// on stdin, its own process group, `kill_on_drop`, and [`kill_group`] on both
/// exits. Those are the parts that keep a runaway from surviving the harness.
///
/// Returns as soon as the child exists, handing back its pid and the future that
/// watches it. **Synchronous, and the arguments are owned**, both deliberately:
/// the pid can be recorded before anything else happens, and the returned future
/// borrows nothing, so the caller can `tokio::spawn` it and forget it. A future
/// that borrowed its sandbox would have to be awaited by someone still holding
/// one, which is exactly what a job is not.
pub fn start_background(
    sandbox: Sandbox,
    script: String,
    handle: crate::jobs::Handle,
    ceiling: Duration,
    cancel: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(
    u32,
    impl std::future::Future<Output = crate::jobs::State> + Send,
)> {
    let mut command = sandbox.command(&script);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawning background command: {script}"))?;
    // A child with no pid cannot be killed by anything — not the ceiling, not
    // `/jobs kill`, not the sweep on quit — so it is not something to run
    // unattended. Reaped here rather than left to leak; `start_kill` rather than
    // `kill().await` so this function has no reason to be async.
    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        anyhow::bail!("the spawned job reported no pid");
    };

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let mut out = Log::open(handle.stdout_path());
    let mut err = Log::open(handle.stderr_path());

    let run = async move {
        let (mut out_done, mut err_done) = (false, false);
        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];
        let deadline = tokio::time::Instant::now() + ceiling;
        tokio::pin!(cancel);

        let finished = loop {
            tokio::select! {
                // Checked first, so a job that floods its pipes cannot starve
                // the two ways it has of being stopped.
                biased;

                _ = &mut cancel => {
                    kill_group(Some(pid));
                    return crate::jobs::State::Killed;
                }

                _ = tokio::time::sleep_until(deadline) => {
                    kill_group(Some(pid));
                    err.append(
                        format!(
                            "\n[harness] job ran past the {}s ceiling and was killed\n",
                            ceiling.as_secs()
                        )
                        .as_bytes(),
                    );
                    return crate::jobs::State::TimedOut;
                }

                read = stdout_pipe.read(&mut out_buf), if !out_done => {
                    match read {
                        Ok(0) | Err(_) => out_done = true,
                        Ok(n) => out.append(&out_buf[..n]),
                    }
                }

                read = stderr_pipe.read(&mut err_buf), if !err_done => {
                    match read {
                        Ok(0) | Err(_) => err_done = true,
                        Ok(n) => err.append(&err_buf[..n]),
                    }
                }

                status = child.wait() => break status,
            }
        };

        // The child is gone, but its pipes may still hold buffered output —
        // the same drain a foreground command does, and for the same reason.
        if !out_done {
            drain_into(&mut stdout_pipe, &mut out).await;
        }
        if !err_done {
            drain_into(&mut stderr_pipe, &mut err).await;
        }

        match finished {
            Ok(status) => match status.code() {
                Some(code) => crate::jobs::State::Exited(code),
                None => crate::jobs::State::Killed,
            },
            Err(_) => crate::jobs::State::Killed,
        }
    };

    Ok((pid, run))
}

/// A job's log file, capped.
///
/// Holds the handle open for the life of the job rather than reopening per
/// write: a chatty job produces thousands of chunks, and an open-append-close
/// each time is the difference between a log and a bottleneck.
struct Log {
    file: Option<std::fs::File>,
    written: usize,
    /// Set once the cap is hit, so the marker is written exactly once.
    capped: bool,
}

impl Log {
    fn open(path: PathBuf) -> Self {
        Self {
            file: std::fs::OpenOptions::new().append(true).open(path).ok(),
            written: 0,
            capped: false,
        }
    }

    /// Append what fits. Past the cap the bytes are dropped, not buffered — the
    /// caller keeps reading either way, so the child never blocks on a full pipe.
    fn append(&mut self, bytes: &[u8]) {
        use std::io::Write;

        let Some(file) = self.file.as_mut() else {
            return;
        };
        if self.written >= MAX_STREAM_BYTES {
            if !self.capped {
                self.capped = true;
                let _ = file
                    .write_all(b"\n[harness] output past the size limit is not being recorded\n");
                let _ = file.flush();
            }
            return;
        }
        let room = MAX_STREAM_BYTES - self.written;
        let take = room.min(bytes.len());
        if file.write_all(&bytes[..take]).is_ok() {
            self.written += take;
            // Flushed per chunk: the point of a job log is that it can be read
            // while the job runs, and a buffered write is invisible until it is
            // too late to be useful.
            let _ = file.flush();
        }
    }
}

/// Read a pipe to EOF into a log, after the child has exited.
async fn drain_into<R>(reader: &mut R, log: &mut Log)
where
    R: AsyncReadExt + Unpin,
{
    let mut buffer = [0u8; 8192];
    while let Ok(n) = reader.read(&mut buffer).await {
        if n == 0 {
            return;
        }
        log.append(&buffer[..n]);
    }
}

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

    /// Plan mode's guarantee, checked against the kernel rather than the profile
    /// text: with writes narrowed to one file, a shell command cannot change
    /// anything else — and the plan file itself still writes.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_narrowed_sandbox_lets_only_the_one_file_be_written() {
        let (sandbox, dir) = sandbox_in("plan-only-exec");
        let plan = dir.join("plan.md");
        let other = dir.join("elsewhere.txt");
        let _ = std::fs::remove_file(&plan);
        let _ = std::fs::remove_file(&other);
        let planning = sandbox.writes_limited_to(&plan);

        // A shell command aimed at the tree: refused by Seatbelt, not by us.
        let out = run(&planning, "echo nope > elsewhere.txt", secs(10))
            .await
            .unwrap();
        assert!(!out.succeeded(), "the write should have failed: {out:?}");
        assert!(!other.exists(), "and nothing should be on disk");

        // The plan file is the exception, through the ordinary write path.
        let written = write(&planning, "plan.md", "# Plan\n").await;
        assert!(written.succeeded(), "{written:?}");
        assert_eq!(std::fs::read_to_string(&plan).unwrap(), "# Plan\n");

        // Reading is untouched — it is how the planning gets done.
        let read = run(&planning, "cat plan.md", secs(10)).await.unwrap();
        assert!(read.succeeded(), "{read:?}");
        assert!(read.stdout.contains("# Plan"));

        let _ = std::fs::remove_file(&plan);
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
    async fn stdin_is_closed_so_a_command_wanting_input_does_not_hang() {
        let (sandbox, _dir) = sandbox_in("stdin");
        // Would block forever on a terminal; with /dev/null it hits EOF at once.
        let out = run(&sandbox, "cat", secs(5)).await.unwrap();
        assert!(!out.timed_out, "stdin should be /dev/null, not a tty");
        assert!(out.succeeded());
    }

    /// Run with a watcher attached, returning the outcome and every chunk.
    async fn run_watched(
        sandbox: &Sandbox,
        script: &str,
        timeout: Duration,
    ) -> (CommandOutput, Vec<Chunk>) {
        let (tx, mut rx) = mpsc::channel(64);
        let collect = tokio::spawn(async move {
            let mut chunks = Vec::new();
            while let Some(chunk) = rx.recv().await {
                chunks.push(chunk);
            }
            chunks
        });
        let out = run_streaming(sandbox, script, timeout, std::future::pending(), tx)
            .await
            .unwrap();
        (out, collect.await.unwrap())
    }

    fn joined(chunks: &[Chunk], stream: Stream) -> String {
        chunks
            .iter()
            .filter(|c| c.stream == stream)
            .map(|c| c.text.as_str())
            .collect()
    }

    #[tokio::test]
    async fn output_is_forwarded_as_it_is_produced() {
        let (sandbox, _dir) = sandbox_in("stream");
        let (out, chunks) = run_watched(&sandbox, "echo one; echo two >&2", secs(10)).await;

        assert!(out.succeeded(), "{out:?}");
        assert_eq!(joined(&chunks, Stream::Stdout).trim(), "one");
        assert_eq!(joined(&chunks, Stream::Stderr).trim(), "two");
        // The streamed text and the recorded text are the same content by two
        // routes; the recorded one stays authoritative.
        assert_eq!(out.stdout.trim(), "one");
    }

    #[tokio::test]
    async fn a_chunk_arrives_before_the_command_exits() {
        // The whole point: a long command must be watchable, not just autopsied.
        let (sandbox, _dir) = sandbox_in("stream-early");
        let (tx, mut rx) = mpsc::channel(64);

        let run = tokio::spawn({
            let sandbox = sandbox.clone();
            async move {
                run_streaming(
                    &sandbox,
                    "echo first; sleep 5; echo last",
                    secs(30),
                    std::future::pending(),
                    tx,
                )
                .await
                .unwrap()
            }
        });

        let first = tokio::time::timeout(secs(3), rx.recv())
            .await
            .expect("a chunk should arrive long before the command finishes")
            .expect("the channel should carry it");
        assert_eq!(first.text.trim(), "first");
        assert!(!run.is_finished(), "the command is still running");

        let out = run.await.unwrap();
        assert!(out.stdout.contains("last"));
    }

    #[tokio::test]
    async fn the_timeout_is_idle_time_not_total_time() {
        // A command that keeps talking survives well past the bound; the old
        // total-budget timeout would have killed this at 2s.
        let (sandbox, _dir) = sandbox_in("idle-alive");
        let (out, _) = run_watched(
            &sandbox,
            "for i in 1 2 3 4 5 6; do echo $i; sleep 0.5; done",
            secs(2),
        )
        .await;

        assert!(!out.timed_out, "output should keep it alive: {out:?}");
        assert!(out.succeeded());
        assert!(out.stdout.contains('6'), "{out:?}");
    }

    #[tokio::test]
    async fn silence_still_times_out() {
        let (sandbox, _dir) = sandbox_in("idle-dead");
        let started = std::time::Instant::now();
        let (out, _) = run_watched(&sandbox, "sleep 60", secs(1)).await;

        assert!(out.timed_out, "{out:?}");
        assert!(out.stderr.contains("nothing for"), "{out:?}");
        assert!(started.elapsed() < secs(10), "should die on schedule");
    }

    #[tokio::test]
    async fn a_command_that_never_stops_talking_still_hits_a_ceiling() {
        // Constant output resets the idle clock forever, so without the hard
        // bound this would run until the user noticed.
        let (sandbox, _dir) = sandbox_in("hard-ceiling");
        let idle = Duration::from_millis(100);
        let started = std::time::Instant::now();
        let (out, _) = run_watched(&sandbox, "while true; do echo spew; done", idle).await;

        assert!(out.timed_out, "{out:?}");
        assert!(out.stderr.contains("ceiling"), "{out:?}");
        assert!(
            started.elapsed() >= idle * HARD_TIMEOUT_MULTIPLE,
            "the idle bound must not have fired first"
        );
        assert!(started.elapsed() < secs(30), "took {:?}", started.elapsed());
    }

    /// A job directory of this test's own, under a sandbox root.
    fn job_in(name: &str, command: &str) -> (Sandbox, std::path::PathBuf, crate::jobs::Handle) {
        let (sandbox, dir) = sandbox_in(name);
        let handle = crate::jobs::create(&dir, command).unwrap();
        (sandbox, dir, handle)
    }

    /// The whole point: `start_background` returns while the child is still
    /// running, and the log is readable before it finishes. A job you can only
    /// read once it is over is a foreground command with extra steps.
    #[tokio::test]
    async fn a_job_starts_and_its_log_is_readable_while_it_runs() {
        let script = "echo first; sleep 5; echo last";
        let (sandbox, dir, handle) = job_in("job-live", script);

        let (pid, run) = start_background(
            sandbox,
            script.to_string(),
            handle.clone(),
            secs(30),
            std::future::pending(),
        )
        .unwrap();
        assert!(pid > 0);

        let task = tokio::spawn(run);
        // Long before the command could have finished.
        let mut log = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            log = std::fs::read_to_string(handle.stdout_path()).unwrap();
            if log.contains("first") {
                break;
            }
        }
        assert!(log.contains("first"), "log was {log:?}");
        assert!(!log.contains("last"), "the job should still be running");
        assert!(!task.is_finished());

        let state = task.await.unwrap();
        assert_eq!(state, crate::jobs::State::Exited(0));
        let log = std::fs::read_to_string(handle.stdout_path()).unwrap();
        assert!(log.contains("last"), "the tail must be drained: {log:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_job_records_both_streams_and_its_exit_code() {
        let script = "echo out; echo err >&2; exit 3";
        let (sandbox, dir, handle) = job_in("job-streams", script);

        let (_, run) = start_background(
            sandbox,
            script.to_string(),
            handle.clone(),
            secs(30),
            std::future::pending(),
        )
        .unwrap();
        assert_eq!(run.await, crate::jobs::State::Exited(3));

        assert_eq!(
            std::fs::read_to_string(handle.stdout_path())
                .unwrap()
                .trim(),
            "out"
        );
        assert_eq!(
            std::fs::read_to_string(handle.stderr_path())
                .unwrap()
                .trim(),
            "err"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A job has no idle bound — that is the difference from a foreground
    /// command, and the reason a dev server can be one. The same script would be
    /// killed by `run`'s timeout well before it finished.
    #[tokio::test]
    async fn a_silent_job_is_not_killed_for_being_quiet() {
        let script = "sleep 2; echo done";
        let (sandbox, dir, handle) = job_in("job-quiet", script);

        let (_, run) = start_background(
            sandbox,
            script.to_string(),
            handle.clone(),
            // A ceiling far past the sleep, and no idle bound at all.
            secs(60),
            std::future::pending(),
        )
        .unwrap();
        assert_eq!(run.await, crate::jobs::State::Exited(0));
        assert!(
            std::fs::read_to_string(handle.stdout_path())
                .unwrap()
                .contains("done")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ceiling is the only thing bounding a job, so it has to take the
    /// whole process group with it — the same guarantee the foreground timeout
    /// carries, checked the same way.
    #[tokio::test]
    async fn the_ceiling_kills_the_job_and_its_grandchildren() {
        let marker = "ai-harness-job-ceiling-marker";
        let script = format!("sh -c 'sleep 30 # {marker}' & sleep 30");
        let (sandbox, dir, handle) = job_in("job-ceiling", &script);

        let (_, run) = start_background(
            sandbox,
            script.clone(),
            handle.clone(),
            Duration::from_millis(600),
            std::future::pending(),
        )
        .unwrap();
        assert_eq!(run.await, crate::jobs::State::TimedOut);

        tokio::time::sleep(Duration::from_millis(400)).await;
        let survivors = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .output()
            .unwrap();
        assert!(
            survivors.stdout.is_empty(),
            "grandchild survived the ceiling: {}",
            String::from_utf8_lossy(&survivors.stdout)
        );
        // And the reason is in the log, where someone reading it will look.
        assert!(
            std::fs::read_to_string(handle.stderr_path())
                .unwrap()
                .contains("ceiling")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancelling_a_job_kills_it_and_reports_killed() {
        let marker = "ai-harness-job-cancel-marker";
        let script = format!("sh -c 'sleep 30 # {marker}' & sleep 30");
        let (sandbox, dir, handle) = job_in("job-cancel", &script);
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();

        let (_, run) = start_background(
            sandbox,
            script.clone(),
            handle.clone(),
            secs(60),
            async move {
                let _ = cancel_rx.await;
            },
        )
        .unwrap();
        let task = tokio::spawn(run);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = cancel_tx.send(());

        assert_eq!(task.await.unwrap(), crate::jobs::State::Killed);
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A job that floods its log is capped like a foreground command, and says
    /// so in the file rather than just stopping — a truncated log that looks
    /// complete is worse than one that admits it is not.
    #[tokio::test]
    async fn a_jobs_log_is_capped_and_says_so() {
        let script = "yes ABCDEFGH | head -c 5000000";
        let (sandbox, dir, handle) = job_in("job-cap", script);

        let (_, run) = start_background(
            sandbox,
            script.to_string(),
            handle.clone(),
            secs(60),
            std::future::pending(),
        )
        .unwrap();
        run.await;

        let log = std::fs::read_to_string(handle.stdout_path()).unwrap();
        assert!(
            log.len() <= MAX_STREAM_BYTES + 200,
            "log was {} bytes, cap is {MAX_STREAM_BYTES}",
            log.len()
        );
        assert!(
            log.contains("past the size limit"),
            "the cap must be stated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A job is confined exactly as a foreground command is: the sandbox is the
    /// same object, and nothing about running unattended loosens it.
    #[tokio::test]
    async fn a_job_cannot_write_outside_the_sandbox() {
        let target = std::env::temp_dir().join("ai-harness-JOB-ESCAPE.txt");
        let _ = std::fs::remove_file(&target);
        let script = format!("echo pwned > {}", target.display());
        let (sandbox, dir, handle) = job_in("job-escape", &script);

        let (_, run) = start_background(
            sandbox,
            script.clone(),
            handle.clone(),
            secs(30),
            std::future::pending(),
        )
        .unwrap();
        assert_ne!(run.await, crate::jobs::State::Exited(0));
        assert!(!target.exists(), "the job escaped the sandbox");
        let _ = std::fs::remove_dir_all(&dir);
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
