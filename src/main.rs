mod app;
mod command;
mod config;
mod diff;
mod exec;
mod fetch;
mod files;
mod highlight;
mod input;
mod ledger;
mod markdown;
mod openrouter;
mod protocol;
mod sandbox;
mod session;
mod tui;
mod ui;
mod wrap;

use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};

use app::{App, Choice};
use config::Args;
use exec::{CommandOutput, WriteOutcome};
use fetch::{FetchOutcome, Fetcher};
use openrouter::{Client, Completion, Message};
use protocol::Action;
use sandbox::Sandbox;

/// Spinner animation rate; also bounds how long we go without redrawing.
const TICK: Duration = Duration::from_millis(80);

/// Something happened in the background and the UI needs to react.
enum Update {
    /// A chunk of streamed reply text.
    Delta(String),
    /// The model reply finished, or the error that replaced it.
    ReplyEnd(Result<Completion, String>),
    /// A piece of output from the command running now. Display-only: the
    /// authoritative text arrives with `Command`.
    CommandChunk(exec::Chunk),
    /// A sandboxed command finished, or failed to start.
    Command(Result<CommandOutput, String>),
    /// A sandboxed file write finished, or failed to start.
    Write(Result<WriteOutcome, String>),
    /// A URL fetch finished. Refusals and HTTP errors arrive as a `FetchOutcome`
    /// carrying the reason, so `Err` here means the request never started.
    Fetch(Box<FetchOutcome>),
}

/// An [`Update`] tagged with the generation of the task that produced it, so a
/// cancelled task's still-queued updates can be recognised as stale and dropped.
struct Tagged {
    generation: u64,
    update: Update,
}

/// A handle to the current in-flight task. Dropping or sending on `cancel`
/// resolves the task's cancel future, stopping its work cleanly.
struct InFlight {
    cancel: oneshot::Sender<()>,
}

impl InFlight {
    fn new(cancel: oneshot::Sender<()>) -> Self {
        Self { cancel }
    }
}

/// Shared context the event handlers need to start new background work.
struct Ctx {
    client: Client,
    sandbox: Sandbox,
    /// Its own HTTP client, never the OpenRouter one: a separate connection pool
    /// keeps a vetted connection from being shared across a trust boundary, and
    /// the API-key-bearing client is never pointed at a model-chosen URL.
    fetcher: Fetcher,
    timeout: Duration,
    tx: mpsc::Sender<Tagged>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // A .env in the working directory is convenient but entirely optional.
    let _ = dotenvy::dotenv();

    let args = Args::parse();
    let api_key = Args::api_key()?;
    let client = Client::new(api_key, args.model.clone())?;
    // Built before the terminal is taken over, so a sandbox failure prints
    // normally instead of being swallowed by the alternate screen.
    let sandbox = Sandbox::new(args.root()?)?;

    let terminal = tui::init()?;
    let result = run(terminal, client, sandbox, args).await;
    tui::restore();
    result
}

async fn run(mut terminal: tui::Tui, client: Client, sandbox: Sandbox, args: Args) -> Result<()> {
    let mut app = App::new(
        client.model().to_string(),
        args.system.clone(),
        args.max_iterations.max(1),
        args.sessions_dir(sandbox.root()),
    );
    app.debug = args.debug || cfg!(debug_assertions);
    app.max_retries = args.max_retries;
    // Reads resolve paths in-process against the sandbox root, so the app needs
    // the sandbox itself, not just its root.
    app.sandbox = Some(sandbox.clone());
    app.confirm_reads = args.confirm_reads;
    app.confirm_fetches = args.confirm_fetch;
    app.auto_approve = args.auto_approve;
    app.price_in = args.price_in;
    app.price_out = args.price_out;
    app.push_notice(format!(
        "Sandbox root: {}   Type /help for commands.",
        sandbox.root().display()
    ));
    // Said at startup rather than left to the status-bar marker: whether the
    // harness will act without asking should be known before the first action,
    // not inferred from the corner of the screen afterwards.
    if app.auto_approve {
        app.push_notice(
            "Auto-approve is on — commands, writes, and edits run without asking, \
             inside the sandbox. Esc cancels; /auto turns it off.",
        );
    }

    let mut events = EventStream::new();
    let (tx, mut rx) = mpsc::channel::<Tagged>(8);
    let ctx = Ctx {
        client,
        sandbox,
        // `Policy::strict` is the only policy production ever builds; the
        // loosened one is `#[cfg(test)]` and does not exist in this binary.
        fetcher: Fetcher::new(fetch::Policy::strict(args.timeout()))?,
        timeout: args.timeout(),
        tx,
    };
    // The current in-flight task, if any, so `Esc` can cancel it.
    let mut inflight: Option<InFlight> = None;
    let mut ticker = tokio::time::interval(TICK);
    let mut metrics = ui::Metrics::default();

    loop {
        terminal.draw(|frame| metrics = ui::draw(frame, &mut app))?;

        tokio::select! {
            // Terminal input.
            Some(event) = events.next() => {
                match event {
                    Ok(event) => handle_event(event, &mut app, &ctx, &mut inflight, &metrics),
                    // A read error means the terminal is gone; exit rather than spin.
                    Err(err) => {
                        app.push_error(format!("terminal input error: {err}"));
                        app.should_quit = true;
                    }
                }
            }

            // Background work finished.
            Some(tagged) = rx.recv() => handle_update(tagged, &mut app, &ctx, &mut inflight),

            // Idle redraw, so the spinner animates while we wait.
            _ = ticker.tick() => {
                if matches!(
                    app.status,
                    app::Status::Waiting | app::Status::Streaming | app::Status::Running
                ) {
                    app.tick = app.tick.wrapping_add(1);
                }
            }
        }

        // Persist a completed turn. A cheap no-op unless the conversation just
        // changed and settled back to idle, so it runs after replies, cancels,
        // errors, and loads alike.
        app.maybe_autosave();

        if app.should_quit {
            return Ok(());
        }
    }
}

fn handle_update(tagged: Tagged, app: &mut App, ctx: &Ctx, inflight: &mut Option<InFlight>) {
    // Drop anything from a task the user has since cancelled or superseded.
    if tagged.generation != app.generation() {
        return;
    }

    match tagged.update {
        // Live tokens accumulate in the display-only streaming buffer.
        Update::Delta(delta) => app.push_delta(&delta),
        // The reply is complete. Drop the live view and commit the full text
        // through the normal path — a malformed reply earns a corrective retry,
        // which comes back as messages to resend.
        Update::ReplyEnd(Ok(completion)) => {
            *inflight = None;
            app.mark_request_done();
            app.finish_stream();
            match app.push_response(completion.content, completion.usage) {
                Some(messages) => spawn_request(app, ctx, inflight, messages),
                // An auto-approved fetch is parked rather than returned, since
                // it is background work the app layer cannot start itself.
                None => {
                    if let Some(url) = app.take_pending_fetch() {
                        spawn_fetch(app, ctx, inflight, url);
                    // Under `--auto-approve` the modal is skipped: the app still
                    // parked a `Pending` exactly as it always does, and the
                    // decision to act on it is made here. `else if` rather than a
                    // second `if` because the two are mutually exclusive — the
                    // fetch arm in `push_response` parks and returns, everything
                    // else falls through to awaiting approval.
                    //
                    // This runs before the loop returns to `terminal.draw`, so
                    // there is no frame in which the modal is visible.
                    } else if app.auto_approve && app.pending().is_some() {
                        allow(app, ctx, inflight);
                    }
                }
            }
        }
        Update::ReplyEnd(Err(message)) => {
            *inflight = None;
            app.mark_request_done();
            app.finish_stream();
            app.push_error(message);
        }
        // Live output for the running window. Display-only; the authoritative
        // text arrives with `Command` below.
        Update::CommandChunk(chunk) => {
            app.push_command_chunk(chunk.stream == exec::Stream::Stderr, &chunk.text)
        }
        // A finished command goes straight back to the model, continuing the loop.
        Update::Command(Ok(output)) => {
            *inflight = None;
            let messages = app.push_command_result(output);
            spawn_request(app, ctx, inflight, messages);
        }
        Update::Command(Err(message)) => {
            *inflight = None;
            app.push_error(format!("could not run command: {message}"));
        }
        // A finished write does the same — its result feeds the loop.
        Update::Write(Ok(outcome)) => {
            *inflight = None;
            let messages = app.push_write_result(outcome);
            spawn_request(app, ctx, inflight, messages);
        }
        Update::Write(Err(message)) => {
            *inflight = None;
            app.push_error(format!("could not write file: {message}"));
        }
        // A fetch always produces an outcome — a refused URL or an HTTP error is
        // reported to the model as a result, so it can try something else.
        Update::Fetch(outcome) => {
            *inflight = None;
            let messages = app.push_fetch_result(*outcome);
            spawn_request(app, ctx, inflight, messages);
        }
    }
}

fn handle_event(
    event: Event,
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
    metrics: &ui::Metrics,
) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(key, app, ctx, inflight, metrics)
        }
        Event::Mouse(mouse) => match mouse.kind {
            // Clicking a modal button is equivalent to confirming it.
            MouseEventKind::Down(MouseButton::Left) if app.pending().is_some() => {
                if ui::hit(metrics.allow_button, mouse.column, mouse.row) {
                    allow(app, ctx, inflight);
                } else if ui::hit(metrics.deny_button, mouse.column, mouse.row) {
                    deny(app, ctx, inflight);
                }
            }
            // Hovering a button focuses it, so click and keyboard agree.
            MouseEventKind::Moved if app.pending().is_some() => {
                if ui::hit(metrics.allow_button, mouse.column, mouse.row) {
                    app.set_choice(Choice::Allow);
                } else if ui::hit(metrics.deny_button, mouse.column, mouse.row) {
                    app.set_choice(Choice::Deny);
                }
            }
            // Clicking a choice answers with it; hovering focuses it.
            MouseEventKind::Down(MouseButton::Left) if app.question().is_some() => {
                if let Some(i) = row_at(
                    metrics.question_list,
                    metrics.question_offset,
                    mouse.column,
                    mouse.row,
                ) && app.question_select(i)
                    && let Some(messages) = app.answer_question()
                {
                    spawn_request(app, ctx, inflight, messages);
                }
            }
            MouseEventKind::Moved if app.question().is_some() => {
                if let Some(i) = row_at(
                    metrics.question_list,
                    metrics.question_offset,
                    mouse.column,
                    mouse.row,
                ) {
                    app.question_select(i);
                }
            }
            // Clicking a session row loads it; hovering focuses it.
            MouseEventKind::Down(MouseButton::Left) if app.picker().is_some() => {
                // Through the row map: a picker entry spans several rows, so
                // a click on its preview or its divider still means that entry.
                if let Some(i) = picker_row_at(metrics, mouse.column, mouse.row) {
                    // Only load when the click lands on a real row.
                    if app.picker_select(i) {
                        app.picker_confirm();
                    }
                }
            }
            MouseEventKind::Moved if app.picker().is_some() => {
                if let Some(i) = picker_row_at(metrics, mouse.column, mouse.row) {
                    app.picker_select(i);
                }
            }
            MouseEventKind::ScrollUp => app.scroll_up(3),
            MouseEventKind::ScrollDown => app.scroll_down(3, metrics.max_scroll()),
            _ => {}
        },
        Event::Paste(text) if !app.is_busy() && app.picker().is_none() => {
            // Normalise line endings so pasted CRLF does not leave stray \r.
            app.input
                .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        }
        _ => {}
    }
}

/// Which session a mouse position falls on, through the picker's row map.
///
/// Unlike the question panel's uniform rows, a picker entry is a name, a rule,
/// its preview lines, and a gap — so the row a click lands on has to be looked
/// up rather than derived by adding a scroll offset.
fn picker_row_at(metrics: &ui::Metrics, column: u16, row: u16) -> Option<usize> {
    let list = metrics.picker_list?;
    if !ui::hit(Some(list), column, row) {
        return None;
    }
    metrics.picker_rows.get((row - list.y) as usize).copied()
}

/// Which list row a mouse position falls on, using the geometry from the last
/// rendered frame. `None` when the point is outside the list.
///
/// Shared by the session picker and the model's question, which are the same
/// shape: a scrolled window of rows whose first visible index is `offset`.
fn row_at(
    list: Option<ratatui::layout::Rect>,
    offset: usize,
    column: u16,
    row: u16,
) -> Option<usize> {
    let list = list?;
    if !ui::hit(Some(list), column, row) {
        return None;
    }
    Some(offset + (row - list.y) as usize)
}

/// Approve the pending action and start running it — a shell command or a write.
fn allow(app: &mut App, ctx: &Ctx, inflight: &mut Option<InFlight>) {
    let Some(action) = app.approve() else { return };

    // A read needs no subprocess, so it is done inline and the loop continues
    // immediately. Only reachable under `--confirm-reads`; otherwise a read
    // never becomes pending in the first place.
    let action = match action {
        Action::Read { path } => {
            let messages = app.perform_read(&path);
            spawn_request(app, ctx, inflight, messages);
            return;
        }
        // Only reachable under `--confirm-fetch`; otherwise the dispatch parks
        // the fetch rather than making it pending.
        Action::Fetch { url } => {
            spawn_fetch(app, ctx, inflight, url);
            return;
        }
        other => other,
    };

    // A shell command is watched while it runs; a write is not, being a single
    // atomic act with nothing to show in progress.
    if let Action::Shell(command) = action {
        spawn_shell(app, ctx, inflight, command);
        return;
    }

    let generation = app.next_generation();
    let sandbox = ctx.sandbox.clone();
    let timeout = ctx.timeout;
    let tx = ctx.tx.clone();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        // The receiver resolves when the sender is used or dropped, either way
        // interrupting the work.
        let cancel = async {
            let _ = cancel_rx.await;
        };
        let update = match action {
            Action::Write { path, contents } => Update::Write(
                exec::write_file(&sandbox, &path, &contents, timeout, cancel)
                    .await
                    .map_err(|e| format!("{e:#}")),
            ),
            // A Response is never approvable, a question waits in
            // `AwaitingChoice` rather than becoming pending, a Read and a Fetch
            // were handled above, a Shell was handled just now, and an Edit is
            // converted to a Write by `approve`, so none are reachable.
            Action::Shell(_)
            | Action::Read { .. }
            | Action::Fetch { .. }
            | Action::Edit { .. }
            | Action::Options { .. }
            | Action::Response(_) => return,
        };
        let _ = tx.send(Tagged { generation, update }).await;
    });
    *inflight = Some(InFlight::new(cancel_tx));
}

/// Run a shell command, forwarding its output as it arrives.
///
/// One channel beyond the usual cancel, carrying output chunks back for the live
/// window. The forwarder relays them onto the single `Tagged` channel the event
/// loop drains, so live output is generation-tagged and dropped on cancel exactly
/// like every other update.
fn spawn_shell(app: &mut App, ctx: &Ctx, inflight: &mut Option<InFlight>, command: String) {
    let generation = app.next_generation();
    app.start_running(command.clone());

    let sandbox = ctx.sandbox.clone();
    let timeout = ctx.timeout;
    let tx = ctx.tx.clone();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let (chunk_tx, mut chunk_rx) = mpsc::channel::<exec::Chunk>(32);

    tokio::spawn(async move {
        let cancel = async {
            let _ = cancel_rx.await;
        };
        let relay = {
            let tx = tx.clone();
            async move {
                while let Some(chunk) = chunk_rx.recv().await {
                    let _ = tx
                        .send(Tagged {
                            generation,
                            update: Update::CommandChunk(chunk),
                        })
                        .await;
                }
            }
        };
        // Both at once: the relay ends when `run_streaming` drops its sender.
        let (result, ()) = tokio::join!(
            exec::run_streaming(&sandbox, &command, timeout, cancel, chunk_tx),
            relay,
        );
        let _ = tx
            .send(Tagged {
                generation,
                update: Update::Command(result.map_err(|e| format!("{e:#}"))),
            })
            .await;
    });
    *inflight = Some(InFlight::new(cancel_tx));
}

/// Refuse the pending command and let the model know.
fn deny(app: &mut App, ctx: &Ctx, inflight: &mut Option<InFlight>) {
    if let Some(messages) = app.deny() {
        spawn_request(app, ctx, inflight, messages);
    }
}

fn handle_key(
    key: KeyEvent,
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
    metrics: &ui::Metrics,
) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let max_scroll = metrics.max_scroll();
    let page = metrics.transcript_height.saturating_sub(1).max(1);

    // Ctrl+C always quits, even with the modal up.
    if ctrl && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    // The approval modal owns the keyboard while it is open, so stray typing
    // cannot leak into the prompt hidden behind it. Esc here means Deny (refuse
    // this command, continue the loop), not cancel-the-turn.
    if app.pending().is_some() {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => app.toggle_choice(),
            KeyCode::Char('y') | KeyCode::Char('Y') => allow(app, ctx, inflight),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => deny(app, ctx, inflight),
            KeyCode::Enter => match app.pending().map(|p| p.selected) {
                Some(Choice::Allow) => allow(app, ctx, inflight),
                Some(Choice::Deny) => deny(app, ctx, inflight),
                None => {}
            },
            KeyCode::PageUp => app.scroll_up(page),
            KeyCode::PageDown => app.scroll_down(page, max_scroll),
            _ => {}
        }
        return;
    }

    // The model's question owns the keyboard while it is up, for the same reason
    // the approval modal does. Placed before the Esc-cancel branch below so Esc
    // dismisses the question — which the model is told about and can act on —
    // rather than silently abandoning the turn.
    if app.question().is_some() {
        let on_other = app.question().is_some_and(|q| q.on_other());
        match key.code {
            KeyCode::Up => app.question_move(-1),
            KeyCode::Down | KeyCode::Tab => app.question_move(1),
            KeyCode::BackTab => app.question_move(-1),
            KeyCode::Esc => {
                if let Some(messages) = app.decline_question() {
                    spawn_request(app, ctx, inflight, messages);
                }
            }
            KeyCode::Enter => {
                if let Some(messages) = app.answer_question() {
                    spawn_request(app, ctx, inflight, messages);
                }
            }
            // A digit picks a choice outright — one keypress is the point of
            // offering a list. Only while the free-text row is unfocused, or
            // typing "2 GB" would jump the selection instead of being typed.
            KeyCode::Char(c) if !on_other && c.is_ascii_digit() && c != '0' => {
                let index = c.to_digit(10).expect("checked ascii digit") as usize - 1;
                app.question_select(index);
            }
            // Everything else edits the free-text answer, and only when that row
            // is focused — `question_input` enforces that, so a keystroke aimed
            // at a highlighted choice cannot vanish into an invisible buffer.
            KeyCode::Char(c) if !ctrl => app.question_input(|input| input.insert_char(c)),
            KeyCode::Backspace if alt || ctrl => {
                app.question_input(|input| input.delete_word_before())
            }
            KeyCode::Backspace => app.question_input(|input| input.backspace()),
            KeyCode::Delete => app.question_input(|input| input.delete()),
            KeyCode::Left => app.question_input(|input| input.move_left()),
            KeyCode::Right => app.question_input(|input| input.move_right()),
            KeyCode::Home => app.question_input(|input| input.move_to_line_start()),
            KeyCode::End => app.question_input(|input| input.move_to_line_end()),
            KeyCode::Char('u') if ctrl => app.question_input(|input| input.delete_to_line_start()),
            _ => {}
        }
        return;
    }

    // The session picker owns the keyboard while it is open.
    if app.picker().is_some() {
        match key.code {
            KeyCode::Up => app.picker_move(-1),
            KeyCode::Down => app.picker_move(1),
            KeyCode::PageUp => app.picker_move(-(page as isize)),
            KeyCode::PageDown => app.picker_move(page as isize),
            KeyCode::Enter => app.picker_confirm(),
            KeyCode::Esc => app.picker_cancel(),
            _ => {}
        }
        return;
    }

    // While a stream or command is in flight (no modal), Esc interrupts it.
    if app.is_busy() && key.code == KeyCode::Esc {
        if let Some(handle) = inflight.take() {
            let _ = handle.cancel.send(());
        }
        app.cancel();
        return;
    }

    match key.code {
        // --- Global ---
        KeyCode::Char('d') if ctrl && app.input.is_blank() => app.should_quit = true,

        // --- Scrolling ---
        KeyCode::PageUp => app.scroll_up(page),
        KeyCode::PageDown => app.scroll_down(page, max_scroll),
        KeyCode::Up if ctrl || alt => app.scroll_up(1),
        KeyCode::Down if ctrl || alt => app.scroll_down(1, max_scroll),

        // --- Submit / newline ---
        // Alt+Enter always works. Shift+Enter only reaches us on terminals that
        // support the kitty keyboard protocol, so it is a bonus, not the path.
        KeyCode::Enter if alt || shift => app.input.insert_char('\n'),
        // --- Completion menu ---
        // These only bind while the menu is open, so Tab still indents and
        // Up/Down still scroll the rest of the time.
        KeyCode::Tab if !app.completions().is_empty() => {
            app.accept_completion();
        }
        KeyCode::Down if !app.completions().is_empty() => app.move_completion(1),
        KeyCode::Up if !app.completions().is_empty() => app.move_completion(-1),
        KeyCode::BackTab if !app.completions().is_empty() => app.move_completion(-1),

        // `submit` handles slash commands locally and returns `None` for them,
        // so nothing typed as a command reaches the model.
        KeyCode::Enter => {
            // Enter runs the highlighted command, so a partially-typed name
            // does not need completing first.
            app.accept_completion();
            if let Some(messages) = app.submit() {
                spawn_request(app, ctx, inflight, messages);
            }
        }

        _ if app.is_busy() => {} // editing is frozen while work is in flight

        // --- Editing ---
        KeyCode::Char('l') if ctrl => app.reset_conversation(),
        KeyCode::Char('u') if ctrl => app.input.delete_to_line_start(),
        KeyCode::Char('w') if ctrl => app.input.delete_word_before(),
        KeyCode::Char('a') if ctrl => app.input.move_to_line_start(),
        KeyCode::Char('e') if ctrl => app.input.move_to_line_end(),
        KeyCode::Char('k') if ctrl => app.input.clear(),
        KeyCode::Backspace if alt || ctrl => app.input.delete_word_before(),
        KeyCode::Backspace => app.input.backspace(),
        KeyCode::Delete => app.input.delete(),
        KeyCode::Left if alt || ctrl => app.input.move_word_left(),
        KeyCode::Right if alt || ctrl => app.input.move_word_right(),
        KeyCode::Left => app.input.move_left(),
        KeyCode::Right => app.input.move_right(),
        KeyCode::Home if ctrl => app.input.move_to_start(),
        KeyCode::Home => app.input.move_to_line_start(),
        KeyCode::End if ctrl => app.input.move_to_end(),
        // End resumes following if the transcript is scrolled back, otherwise
        // it does the ordinary line-end move.
        KeyCode::End if !app.follow => app.scroll_to_bottom(max_scroll),
        KeyCode::End => app.input.move_to_line_end(),
        KeyCode::Tab => app.input.insert_str("    "),
        // Prompt history recall. Bare Up/Down reach here only when not busy
        // (above guard), not scrolling (Ctrl/Alt arms above), and with no
        // completion menu open (an empty prompt has none).
        KeyCode::Up => app.recall_prev(),
        KeyCode::Down => app.recall_next(),
        KeyCode::Char(c) if !ctrl => app.input.insert_char(c),
        _ => {}
    }
}

/// Fetch a URL in the background.
///
/// Shared by the automatic path and the `--confirm-fetch` approval path. Unlike
/// a read this cannot run inline: it is network I/O, so it needs the same
/// generation tagging and cancel signal a command gets.
fn spawn_fetch(app: &mut App, ctx: &Ctx, inflight: &mut Option<InFlight>, url: String) {
    let generation = app.next_generation();
    let fetcher = ctx.fetcher.clone();
    let tx = ctx.tx.clone();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let cancel = async {
            let _ = cancel_rx.await;
        };
        let outcome = fetcher.fetch(&url, cancel).await;
        let _ = tx
            .send(Tagged {
                generation,
                update: Update::Fetch(Box::new(outcome)),
            })
            .await;
    });
    *inflight = Some(InFlight::new(cancel_tx));
}

fn spawn_request(
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
    messages: Vec<Message>,
) {
    let generation = app.next_generation();
    app.mark_request_sent();
    let client = ctx.client.clone();
    let tx = ctx.tx.clone();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let cancel = async {
            let _ = cancel_rx.await;
        };
        // A send failure just means the UI shut down; stop forwarding if so.
        let end = match client.open_stream(&messages).await {
            Ok(stream) => stream_reply(stream, &tx, generation, cancel).await,
            Err(e) => Some(Err(format!("{e:#}"))),
        };
        // On cancel `end` is None, so we send nothing — the app has already
        // moved on and any late update would be dropped as stale anyway.
        if let Some(end) = end {
            let _ = tx
                .send(Tagged {
                    generation,
                    update: Update::ReplyEnd(end),
                })
                .await;
        }
    });
    *inflight = Some(InFlight::new(cancel_tx));
}

/// Forward stream deltas to the UI, accumulating the full reply.
///
/// Returns `Some(reply-or-error)` normally, or `None` if `cancel` fired — in
/// which case the caller sends no terminal update.
async fn stream_reply(
    stream: impl futures_util::Stream<Item = anyhow::Result<openrouter::StreamEvent>>,
    tx: &mpsc::Sender<Tagged>,
    generation: u64,
    cancel: impl std::future::Future<Output = ()>,
) -> Option<Result<Completion, String>> {
    use openrouter::StreamEvent;
    futures_util::pin_mut!(stream);
    tokio::pin!(cancel);

    let mut content = String::new();
    let mut usage = None;
    loop {
        tokio::select! {
            // Interrupt takes priority so a fast stream cannot starve it.
            biased;
            _ = &mut cancel => return None,
            event = stream.next() => match event {
                Some(Ok(StreamEvent::Delta(delta))) => {
                    content.push_str(&delta);
                    // `send().await` back-pressures the HTTP read, so tokens are
                    // never dropped when the UI falls behind.
                    if tx
                        .send(Tagged { generation, update: Update::Delta(delta) })
                        .await
                        .is_err()
                    {
                        return Some(Err("UI closed".to_string()));
                    }
                }
                Some(Ok(StreamEvent::Done { usage: Some(u) })) => usage = Some(u),
                Some(Ok(StreamEvent::Done { usage: None })) => {}
                Some(Err(e)) => return Some(Err(format!("{e:#}"))),
                None => return Some(Ok(Completion { content, usage })),
            },
        }
    }
}
