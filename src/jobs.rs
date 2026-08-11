//! Commands that outlive the turn that started them.
//!
//! A foreground command holds its turn open: [`crate::exec::run_streaming`] runs
//! inside the one `InFlight` slot a session has, and the model waits for the exit
//! code. That is the right shape for `ls` and the wrong one for `cargo test`, a
//! build, or a dev server — work worth starting and coming back to.
//!
//! A job is that second shape. It is approved like any other shell command, then
//! returns immediately with an id, and keeps running across turns.
//!
//! ```text
//! .ai_harness/jobs/1786215831-000/
//!     command   the script, verbatim
//!     pid       the process-group leader
//!     stdout    appended as the pipes are read, capped
//!     stderr
//!     status    one line: running | exit N | killed | timed out | abandoned
//!     started   unix seconds
//!     ended     unix seconds, once there is an answer
//! ```
//!
//! **The directory is the state.** Nothing here caches a job in memory, and the
//! only thing the harness keeps that is not on disk is the handle needed to kill
//! one ([`crate::main`]'s registry). That is deliberate rather than austere: the
//! model already opens files with `<ai-harness-read>`, so a job's output needs no
//! protocol of its own — and a session that is reloaded, or a harness that is
//! restarted, reads the same files and reaches the same answer as the process
//! that wrote them.
//!
//! One asymmetry to know about: a **read** reaches this directory, but a
//! **grep or glob does not**. [`crate::config::HARNESS_DIR`] is in
//! [`crate::search::SKIP_DIRS`], because a session file holds a whole prior
//! conversation and a search for any term the user has typed would otherwise
//! match the transcript of them typing it. Jobs live under the same directory and
//! inherit that. It costs nothing here — each log is capped at
//! [`crate::exec::MAX_STREAM_BYTES`], comfortably inside
//! [`crate::files::MAX_READ_BYTES`], so a whole log always fits in one read —
//! but the contract has to say so, or the model greps, finds nothing, and
//! concludes the build was clean.
//!
//! The one thing a restart cannot know is whether a job it did not start is still
//! alive. [`sweep`] settles that at startup by writing off anything still marked
//! running, so the contract never claims a process that died with the last
//! process.

use std::path::{Path, PathBuf};

/// The jobs directory inside `.ai_harness/`.
pub const DIR: &str = "jobs";

/// How many jobs may run at once.
///
/// Not a resource limit so much as a legibility one: the contract carries a line
/// per running job on every request, and a model that has started nine builds has
/// lost track of them rather than parallelised anything. Past this the start is
/// refused as a result the model can act on.
pub const MAX_CONCURRENT: usize = 4;

/// How much of `command` is read back for display.
///
/// The file holds the script exactly as it ran, which is what the model reads it
/// for; this bounds only what the *index* pays, the trick
/// [`crate::memory::description_of`] uses for the same reason.
const HEAD_BYTES: usize = 4 * 1024;

/// Where a project's jobs live, given the sandbox root.
///
/// Keyed on the root rather than on `--sessions-dir`, exactly as
/// [`crate::memory::dir`] is: a job belongs to the project it runs in, and a flag
/// that moves sessions elsewhere should not move it.
pub fn dir(root: &Path) -> PathBuf {
    root.join(crate::config::HARNESS_DIR).join(DIR)
}

/// What became of a job.
///
/// One line in `status`, rewritten on transition. A string rather than a number
/// because the model reads this file directly and `exit 1` says what `1` does
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Exited(i32),
    /// Killed by a signal, or by `/jobs kill`.
    Killed,
    /// Ran past the ceiling.
    TimedOut,
    /// Left running by a harness that is no longer here. See [`sweep`].
    Abandoned,
}

impl State {
    pub fn as_line(self) -> String {
        match self {
            Self::Running => "running".to_string(),
            Self::Exited(code) => format!("exit {code}"),
            Self::Killed => "killed".to_string(),
            Self::TimedOut => "timed out".to_string(),
            Self::Abandoned => "abandoned".to_string(),
        }
    }

    fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        match line {
            "running" => Some(Self::Running),
            "killed" => Some(Self::Killed),
            "timed out" => Some(Self::TimedOut),
            "abandoned" => Some(Self::Abandoned),
            _ => line
                .strip_prefix("exit ")
                .and_then(|code| code.trim().parse().ok())
                .map(Self::Exited),
        }
    }

    pub fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// One job, as the contract and `/jobs` see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub command: String,
    pub state: State,
    /// Unix seconds. Zero when the file is missing or unreadable, which only
    /// costs a duration nobody can act on.
    pub started: u64,
    /// Unix seconds, once there is an answer.
    pub ended: Option<u64>,
}

impl Job {
    /// How long the job has been going, or went on for.
    pub fn elapsed_secs(&self) -> u64 {
        let end = self.ended.unwrap_or_else(crate::session::now_secs);
        end.saturating_sub(self.started)
    }

    /// The first line of the command, bounded — what a list can show on one row.
    pub fn summary(&self) -> String {
        const MAX: usize = 60;
        let line = self.command.lines().next().unwrap_or("").trim();
        if line.chars().count() <= MAX {
            return line.to_string();
        }
        let kept: String = line.chars().take(MAX - 1).collect();
        format!("{kept}…")
    }
}

/// An open job's directory, held by the task running it.
///
/// Exists so the paths are built in one place: the task appends to two of these
/// files from inside `select!` arms, and a second opinion about where they live
/// is the kind of bug that only shows up as an empty log.
#[derive(Debug, Clone)]
pub struct Handle {
    pub id: String,
    pub dir: PathBuf,
}

impl Handle {
    pub fn stdout_path(&self) -> PathBuf {
        self.dir.join("stdout")
    }

    pub fn stderr_path(&self) -> PathBuf {
        self.dir.join("stderr")
    }

    /// Record the process-group leader, so the job can be killed by something
    /// that is not this process — including the model, through an ordinary
    /// approved `<ai-harness-shell>`. That is why there is no kill tag.
    pub fn record_pid(&self, pid: u32) {
        let _ = std::fs::write(self.dir.join("pid"), format!("{pid}\n"));
    }

    /// Write the terminal state and the time it happened.
    ///
    /// Order matters: `ended` first, then `status`. A reader that catches the
    /// pair half-written sees a job still running rather than a finished one
    /// with no end time, and `running` is the state every reader already handles.
    pub fn finish(&self, state: State) {
        let _ = std::fs::write(
            self.dir.join("ended"),
            format!("{}\n", crate::session::now_secs()),
        );
        let _ = std::fs::write(self.dir.join("status"), format!("{}\n", state.as_line()));
    }
}

/// Open a directory for a new job, and mark it running.
///
/// The id is `<unix-seconds>-<counter>`, which sorts chronologically as a string
/// and needs no allocator shared between sessions. The counter is bumped past a
/// directory that already exists rather than trusted, since a restart resets it
/// and two jobs a second apart across that boundary would otherwise collide.
pub fn create(root: &Path, command: &str) -> Result<Handle, String> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);

    let parent = dir(root);
    std::fs::create_dir_all(&parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;

    let secs = crate::session::now_secs();
    // Bounded rather than `loop`: if a thousand ids in one second are all taken,
    // something is wrong that another attempt will not fix.
    for _ in 0..1000 {
        let id = format!("{secs}-{:03}", SEQ.fetch_add(1, Ordering::Relaxed) % 1000);
        let path = parent.join(&id);
        if path.exists() {
            continue;
        }
        std::fs::create_dir(&path).map_err(|e| format!("creating {}: {e}", path.display()))?;
        let _ = std::fs::write(path.join("command"), format!("{}\n", command.trim_end()));
        let _ = std::fs::write(path.join("started"), format!("{secs}\n"));
        // Both logs exist from the start, so a model that reads one before the
        // job has printed anything gets an empty file rather than a failed read.
        let _ = std::fs::write(path.join("stdout"), "");
        let _ = std::fs::write(path.join("stderr"), "");
        std::fs::write(
            path.join("status"),
            format!("{}\n", State::Running.as_line()),
        )
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
        return Ok(Handle { id, dir: path });
    }
    Err("could not find a free job id".to_string())
}

/// Every job in the project, oldest first.
///
/// A directory without a readable `status` is **left out** rather than listed
/// unknown — it is a half-created job or something else entirely, and neither is
/// worth a line in a budget paid on every request.
pub fn list(root: &Path) -> Vec<Job> {
    let parent = dir(root);
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return Vec::new();
    };
    let mut jobs: Vec<Job> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let id = path.file_name()?.to_str()?.to_string();
            let state = State::parse(&head(&path.join("status"))?)?;
            Some(Job {
                id,
                command: head(&path.join("command")).unwrap_or_default(),
                state,
                started: number(&path.join("started")).unwrap_or(0),
                ended: number(&path.join("ended")),
            })
        })
        .collect();
    jobs.sort_by(|a, b| a.id.cmp(&b.id));
    jobs
}

/// The jobs still going. What the concurrency cap and the status marker count.
pub fn running(root: &Path) -> Vec<Job> {
    list(root)
        .into_iter()
        .filter(|job| job.state.is_running())
        .collect()
}

/// Write off every job still marked running, and say how many.
///
/// Called once at startup. A job belongs to the process that spawned it — the
/// child is killed when the harness exits, and a `running` left behind is a
/// claim about a process that is gone. Without this the contract would report it
/// as live forever, and the cap would count it against jobs that are.
pub fn sweep(root: &Path) -> usize {
    let stale = running(root);
    for job in &stale {
        let handle = Handle {
            dir: dir(root).join(&job.id),
            id: job.id.clone(),
        };
        // Not `finish`: the job ended whenever the last harness did, which is not
        // now, and writing `ended` now would claim a runtime it did not have.
        let _ = std::fs::write(
            handle.dir.join("status"),
            format!("{}\n", State::Abandoned.as_line()),
        );
    }
    stale.len()
}

/// Kill a job by the pid it recorded, and mark it killed.
///
/// Returns whether there was a live job to kill. Used by `/jobs kill` and by the
/// sweep the harness does on its way out.
pub fn kill(root: &Path, id: &str) -> bool {
    let path = dir(root).join(id);
    let Some(pid) = number(&path.join("pid")) else {
        return false;
    };
    crate::exec::kill_group(Some(pid as u32));
    Handle {
        id: id.to_string(),
        dir: path,
    }
    .finish(State::Killed);
    true
}

/// A bounded head of a small file, trimmed. `None` when it cannot be read.
///
/// Bounded for the reason [`crate::memory::description_of`] is: what is wanted is
/// at the top, and what is not may be large — a `command` is whatever the model
/// wrote.
fn head(path: &Path) -> Option<String> {
    use std::io::Read;

    let mut buffer = vec![0u8; HEAD_BYTES];
    let mut file = std::fs::File::open(path).ok()?;
    let read = file.read(&mut buffer).ok()?;
    // Lossy rather than strict: a command is text the model wrote and may be cut
    // through a multi-byte character by the bounded read.
    let text = String::from_utf8_lossy(&buffer[..read]).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// A single number out of a one-line file.
fn number(path: &Path) -> Option<u64> {
    head(path)?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-jobs-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_new_job_is_running_and_lists_itself() {
        let root = temp_root("create");
        let handle = create(&root, "cargo test").unwrap();

        let jobs = list(&root);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, handle.id);
        assert_eq!(jobs[0].command, "cargo test");
        assert_eq!(jobs[0].state, State::Running);
        assert_eq!(jobs[0].ended, None);
        assert_eq!(running(&root).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Both logs exist before the child has printed anything, so the model's
    /// first read of a just-started job is an empty file rather than an error.
    #[test]
    fn the_log_files_exist_from_the_start() {
        let root = temp_root("logs");
        let handle = create(&root, "sleep 1").unwrap();
        assert!(handle.stdout_path().exists());
        assert!(handle.stderr_path().exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn finishing_records_the_state_and_the_end_time() {
        let root = temp_root("finish");
        let handle = create(&root, "true").unwrap();
        handle.finish(State::Exited(0));

        let job = list(&root).remove(0);
        assert_eq!(job.state, State::Exited(0));
        assert!(job.ended.is_some(), "an ended job records when");
        assert!(running(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_state_survives_the_round_trip_through_the_file() {
        let root = temp_root("states");
        for state in [
            State::Running,
            State::Exited(0),
            State::Exited(137),
            State::Killed,
            State::TimedOut,
            State::Abandoned,
        ] {
            let handle = create(&root, "x").unwrap();
            std::fs::write(handle.dir.join("status"), format!("{}\n", state.as_line())).unwrap();
            let job = list(&root)
                .into_iter()
                .find(|j| j.id == handle.id)
                .expect("the job should list");
            assert_eq!(job.state, state, "round trip failed for {state:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A job left running by a harness that is gone is a claim about a process
    /// that is not there. The contract must not repeat it.
    #[test]
    fn sweeping_writes_off_jobs_left_running() {
        let root = temp_root("sweep");
        let live = create(&root, "sleep 100").unwrap();
        let done = create(&root, "true").unwrap();
        done.finish(State::Exited(0));

        assert_eq!(sweep(&root), 1, "only the running one is stale");

        let jobs = list(&root);
        let swept = jobs.iter().find(|j| j.id == live.id).unwrap();
        assert_eq!(swept.state, State::Abandoned);
        // The finished one is untouched — it already had an answer.
        let kept = jobs.iter().find(|j| j.id == done.id).unwrap();
        assert_eq!(kept.state, State::Exited(0));
        assert!(running(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Ids sort chronologically as strings, which is what `list` relies on to
    /// return oldest first without reading every `started`.
    #[test]
    fn ids_are_unique_and_ordered() {
        let root = temp_root("ids");
        let ids: Vec<String> = (0..5).map(|_| create(&root, "x").unwrap().id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "ids should already be in order");

        let listed: Vec<String> = list(&root).into_iter().map(|j| j.id).collect();
        assert_eq!(listed, ids);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A half-created directory is left out rather than listed as an unknown
    /// job — it would cost a contract line saying nothing.
    #[test]
    fn a_directory_without_a_status_is_not_a_job() {
        let root = temp_root("nostatus");
        create(&root, "real").unwrap();
        std::fs::create_dir_all(dir(&root).join("junk")).unwrap();
        std::fs::write(dir(&root).join("junk").join("command"), "hi").unwrap();
        // And an unparseable status is no better than a missing one.
        let bad = dir(&root).join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("status"), "who knows\n").unwrap();

        let jobs = list(&root);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].command, "real");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_directory_has_no_jobs() {
        let root = temp_root("missing");
        assert!(list(&root).is_empty());
        assert!(running(&root).is_empty());
        assert_eq!(sweep(&root), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The pid file is the interface for stopping a job this process did not
    /// start — after a restart the directory is all there is. Checked against a
    /// real process group, since the whole value is that it reaps grandchildren.
    #[test]
    fn killing_by_the_recorded_pid_stops_a_real_process() {
        let root = temp_root("kill");
        let handle = create(&root, "sleep 60").unwrap();

        // A process group of its own, exactly as `start_background` makes.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 60"])
            .spawn()
            .unwrap();
        handle.record_pid(child.id());

        assert!(kill(&root, &handle.id), "there was a job to kill");
        assert_eq!(list(&root)[0].state, State::Killed);
        assert!(running(&root).is_empty());

        // The signal went where the file said. `wait` reaps it either way, so
        // the test does not leave a process behind if the kill missed.
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn killing_a_job_with_no_pid_recorded_reports_nothing_to_kill() {
        let root = temp_root("kill-nopid");
        let handle = create(&root, "sleep 60").unwrap();
        // Created but never started: no pid file yet.
        assert!(!kill(&root, &handle.id));
        assert!(!kill(&root, "no-such-job"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_summary_is_one_bounded_line() {
        let job = Job {
            id: "1-000".into(),
            command: "echo one\necho two".into(),
            state: State::Running,
            started: 0,
            ended: None,
        };
        assert_eq!(job.summary(), "echo one");

        let long = Job {
            command: "x".repeat(200),
            ..job
        };
        assert!(long.summary().chars().count() <= 60);
        assert!(long.summary().ends_with('…'));
    }

    #[test]
    fn the_jobs_directory_sits_under_the_harness_directory() {
        let root = Path::new("/projects/thing");
        assert_eq!(
            dir(root),
            root.join(crate::config::HARNESS_DIR).join("jobs")
        );
    }
}
