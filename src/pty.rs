//! A program on a pseudo-terminal, and the screen it drew.
//!
//! This is what makes a session whose agent is *another text interface*
//! possible — Claude Code, in the case it was built for. The harness owns the
//! real terminal (see [`crate::tui::init`]: raw mode, alternate screen, mouse
//! capture), so a child with its own interface cannot have it. It gets a pty
//! instead: a terminal that is not a terminal, whose output we parse into a
//! grid of cells and draw inside our own frame.
//!
//! Deliberately not Claude-specific. It hosts a program, and which program is
//! the caller's business — which is also what lets it be tested with `printf`
//! rather than with an agent and an API key.
//!
//! The split of responsibilities:
//!
//! - A blocking reader task owns the read end and feeds a [`vt100::Parser`].
//!   It never touches the UI. When it has processed some bytes it pings the
//!   event loop, and *drops the ping* if the channel is full — see
//!   [`PtySession::spawn`] for why that matters more than it looks.
//! - The draw path locks the parser and renders whatever is in it now.
//! - Keys go the other way, as bytes, through [`PtySession::write`].

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc;

use crate::{Tagged, Update};

/// How long after its last output a child still counts as working.
///
/// The pane's spinner is the only thing that reads this, and what it is really
/// asking is "is something happening in there". A text interface redraws while
/// it is thinking — a spinner of its own, a streamed reply — so recent output
/// is a good proxy, and this is long enough to bridge the gap between frames of
/// someone else's animation.
const WORKING_FOR: Duration = Duration::from_millis(400);

/// Scrollback the emulator keeps. The child has its own, and this is only here
/// so that a burst of output taller than the pane is not lost before it is
/// drawn.
const SCROLLBACK: usize = 200;

/// A running program and its screen.
pub struct PtySession {
    /// The parsed screen. Written by the reader task, read by the draw path —
    /// a plain `std::sync::Mutex` rather than tokio's, because both sides are
    /// synchronous and hold it for a few microseconds.
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Box<dyn MasterPty + Send>,
    /// The size last pushed to the kernel, so a redraw that changed nothing
    /// does not signal the child.
    size: Mutex<(u16, u16)>,
    last_output: Arc<Mutex<Instant>>,
    exited: Arc<Mutex<Option<ExitStatus>>>,
    /// Kept for [`PtySession::kill`]. The reader task waits on the child, so
    /// this is only ever used to end it early.
    pid: Option<u32>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
}

impl PtySession {
    /// Open a pty, start `program` on it, and begin reading.
    ///
    /// `session` is the harness's session id, so the pings can be routed; they
    /// carry no payload and go out with generation 0, because a pty has no
    /// notion of the harness's turns — the same exemption background jobs get.
    pub fn spawn(
        program: &str,
        args: &[&str],
        cwd: &Path,
        rows: u16,
        cols: u16,
        session: u64,
        tx: mpsc::Sender<Tagged>,
    ) -> Result<Self> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .with_context(|| format!("opening a pty for {program}"))?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(cwd);
        // A child that believes it is on a dumb terminal draws nothing worth
        // looking at. This is what the harness's own terminal is, and what the
        // emulator below implements.
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("starting {program}"))?;
        let pid = child.process_id();
        // Dropped so the slave side is closed here; otherwise the read below
        // never sees EOF, because this process would still hold the other end
        // open after the child exits.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .context("taking the pty's read end")?;
        let writer = pair
            .master
            .take_writer()
            .context("taking the pty's write end")?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            rows.max(1),
            cols.max(1),
            SCROLLBACK,
        )));
        let last_output = Arc::new(Mutex::new(Instant::now()));
        let exited = Arc::new(Mutex::new(None));
        let child = Arc::new(Mutex::new(child));

        spawn_reader(
            reader,
            Arc::clone(&parser),
            Arc::clone(&last_output),
            Arc::clone(&exited),
            Arc::clone(&child),
            session,
            tx,
        );

        Ok(Self {
            parser,
            writer: Mutex::new(writer),
            master: pair.master,
            size: Mutex::new((rows, cols)),
            last_output,
            exited,
            pid,
            child,
        })
    }

    /// The screen, for the length of one draw.
    ///
    /// A guard rather than a copy: the screen is a grid of cells and the widget
    /// that draws it wants a reference, so copying it per frame would be the
    /// whole cost of drawing for no gain.
    pub fn parser(&self) -> MutexGuard<'_, vt100::Parser> {
        lock(&self.parser)
    }

    /// Send bytes to the child. Failures are dropped: the only way this fails
    /// is a child that has gone, which [`PtySession::exited`] is how you find
    /// out about.
    pub fn write(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writer.write_all(bytes);
        let _ = writer.flush();
    }

    /// Tell the child the terminal changed size, if it did.
    ///
    /// Both ends have to be told: the kernel, which signals the child, and the
    /// emulator, which has to agree about the grid it is parsing into or the
    /// child's own line wrapping lands in the wrong place.
    pub fn resize(&self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        let mut size = self.size.lock().unwrap_or_else(|e| e.into_inner());
        if *size == (rows, cols) {
            return;
        }
        *size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        lock(&self.parser).screen_mut().set_size(rows, cols);
    }

    /// End the child and everything it started.
    ///
    /// Both halves matter. `kill` reaches the child; the process-group signal
    /// reaches what the child started, and an agent's whole job is starting
    /// things. `portable-pty` puts the child in a session of its own, so its
    /// pid is its process group id.
    pub fn kill(&self) {
        let _ = lock(&self.child).kill();
        crate::exec::kill_group(self.pid);
    }

    /// Whether something is happening in there — see [`WORKING_FOR`].
    pub fn working(&self) -> bool {
        if self.exited().is_some() {
            return false;
        }
        lock(&self.last_output).elapsed() < WORKING_FOR
    }

    /// The child's exit status, once it has one.
    pub fn exited(&self) -> Option<ExitStatus> {
        lock(&self.exited).clone()
    }

    /// The last `want` non-blank lines on screen, oldest first.
    ///
    /// What the sessions view shows as activity. For a harness session that
    /// comes from the transcript; here there is no transcript, and the bottom
    /// of someone else's screen is the closest thing to "what it is doing".
    pub fn activity(&self, want: usize) -> Vec<String> {
        let contents = lock(&self.parser).screen().contents();
        let mut lines: Vec<String> = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.trim_end().to_string())
            .rev()
            .take(want)
            .collect();
        lines.reverse();
        lines
    }
}

/// A poisoned lock is not a reason to lose the session: the data behind it is a
/// screen and a timestamp, and the worst a panicking reader leaves is a torn
/// frame.
fn lock<T>(cell: &Mutex<T>) -> MutexGuard<'_, T> {
    cell.lock().unwrap_or_else(|e| e.into_inner())
}

/// Read the pty until the child goes, parsing as we go.
///
/// Blocking, on the blocking pool, because that is what reading a pty is: there
/// is no async pty in this dependency, and a task that spends its life in
/// `read` is exactly what `spawn_blocking` is for.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    last_output: Arc<Mutex<Instant>>,
    exited: Arc<Mutex<Option<ExitStatus>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    session: u64,
    tx: mpsc::Sender<Tagged>,
) {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    lock(&parser).process(&buf[..n]);
                    *lock(&last_output) = Instant::now();
                    // `try_send`, and the failure ignored on purpose. This
                    // says no more than "the screen changed", and the next one
                    // says the same thing — so a full channel is a reason to
                    // drop it, not to wait. A blocking send here would couple
                    // the child's output rate to the harness's frame rate, and
                    // a redrawing text interface produces kilobytes many times
                    // a second.
                    let _ = tx.try_send(Tagged {
                        session,
                        generation: 0,
                        update: Update::Pty,
                    });
                }
            }
        }

        // EOF means the child closed the pty, so this returns at once rather
        // than blocking the lock that `kill` also wants.
        let status = lock(&child).wait().ok();
        *lock(&exited) = status.clone();
        let _ = tx
            .try_send(Tagged {
                session,
                generation: 0,
                update: Update::PtyExit(status),
            })
            .ok();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait for `predicate` to hold, so the test is not a race against a
    /// process start. Returns what the screen held when it gave up.
    async fn until(pty: &PtySession, predicate: impl Fn(&str) -> bool) -> String {
        for _ in 0..200 {
            let contents = pty.parser().screen().contents();
            if predicate(&contents) {
                return contents;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pty.parser().screen().contents()
    }

    /// The whole plumbing, with `printf` standing in for an agent: a child
    /// starts, its output reaches the emulator, the event loop is told, and the
    /// exit is noticed.
    #[tokio::test]
    async fn a_child_writes_to_the_screen_and_its_exit_is_noticed() {
        let (tx, mut rx) = mpsc::channel(8);
        let pty = PtySession::spawn(
            "/bin/sh",
            &["-c", "printf 'hello pty'"],
            &std::env::temp_dir(),
            24,
            80,
            7,
            tx,
        )
        .expect("a pty and a shell");

        let screen = until(&pty, |s| s.contains("hello pty")).await;
        assert!(screen.contains("hello pty"), "screen was {screen:?}");

        // The ping is addressed to the session, and carries generation 0 so
        // the staleness check lets it through.
        let tagged = rx.recv().await.expect("a ping");
        assert_eq!(tagged.session, 7);
        assert_eq!(tagged.generation, 0);

        let _ = until(&pty, |_| pty.exited().is_some()).await;
        let status = pty.exited().expect("the child exited");
        assert!(status.success(), "printf should succeed: {status:?}");
        assert!(!pty.working(), "and a child that has gone is not working");
    }

    /// Activity is the bottom of the screen, blank lines left out — which is
    /// what the sessions view has to show for a session with no transcript.
    #[tokio::test]
    async fn activity_is_the_last_non_blank_lines() {
        let (tx, _rx) = mpsc::channel(8);
        let pty = PtySession::spawn(
            "/bin/sh",
            &["-c", "printf 'first\\n\\nsecond\\n\\nthird\\n'"],
            &std::env::temp_dir(),
            24,
            80,
            1,
            tx,
        )
        .expect("a pty and a shell");

        let _ = until(&pty, |s| s.contains("third")).await;
        assert_eq!(pty.activity(2), vec!["second", "third"]);
    }

    /// Resizing reaches the emulator, so the child and our grid agree.
    #[tokio::test]
    async fn resizing_changes_the_emulated_grid() {
        let (tx, _rx) = mpsc::channel(8);
        let pty = PtySession::spawn(
            "/bin/sh",
            &["-c", "sleep 5"],
            &std::env::temp_dir(),
            24,
            80,
            1,
            tx,
        )
        .expect("a pty and a shell");

        assert_eq!(pty.parser().screen().size(), (24, 80));
        pty.resize(30, 100);
        assert_eq!(pty.parser().screen().size(), (30, 100));
        pty.kill();
    }
}
