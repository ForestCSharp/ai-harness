mod app;
mod command;
mod config;
mod exec;
mod input;
mod openrouter;
mod protocol;
mod sandbox;
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
use tokio::sync::mpsc;

use app::{App, Choice};
use config::Args;
use exec::CommandOutput;
use openrouter::{Client, Completion, Message};
use sandbox::Sandbox;

/// Spinner animation rate; also bounds how long we go without redrawing.
const TICK: Duration = Duration::from_millis(80);

/// Something happened in the background and the UI needs to react.
enum Update {
    /// A chunk of streamed reply text.
    Delta(String),
    /// The model reply finished, or the error that replaced it.
    ReplyEnd(Result<Completion, String>),
    /// A sandboxed command finished, or failed to start.
    Command(Result<CommandOutput, String>),
}

/// Shared context the event handlers need to start new background work.
struct Ctx {
    client: Client,
    sandbox: Sandbox,
    timeout: Duration,
    tx: mpsc::Sender<Update>,
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
    );
    app.debug = args.debug || cfg!(debug_assertions);
    app.max_retries = args.max_retries;
    app.push_notice(format!(
        "Sandbox root: {}   Type /help for commands.",
        sandbox.root().display()
    ));

    let mut events = EventStream::new();
    let (tx, mut rx) = mpsc::channel::<Update>(8);
    let ctx = Ctx {
        client,
        sandbox,
        timeout: args.timeout(),
        tx,
    };
    let mut ticker = tokio::time::interval(TICK);
    let mut metrics = ui::Metrics::default();

    loop {
        terminal.draw(|frame| metrics = ui::draw(frame, &mut app))?;

        tokio::select! {
            // Terminal input.
            Some(event) = events.next() => {
                match event {
                    Ok(event) => handle_event(event, &mut app, &ctx, metrics),
                    // A read error means the terminal is gone; exit rather than spin.
                    Err(err) => {
                        app.push_error(format!("terminal input error: {err}"));
                        app.should_quit = true;
                    }
                }
            }

            // Background work finished.
            Some(update) = rx.recv() => handle_update(update, &mut app, &ctx),

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

        if app.should_quit {
            return Ok(());
        }
    }
}

fn handle_update(update: Update, app: &mut App, ctx: &Ctx) {
    match update {
        // Live tokens accumulate in the display-only streaming buffer.
        Update::Delta(delta) => app.push_delta(&delta),
        // The reply is complete. Drop the live view and commit the full text
        // through the normal path — a malformed reply earns a corrective retry,
        // which comes back as messages to resend.
        Update::ReplyEnd(Ok(completion)) => {
            app.finish_stream();
            if let Some(messages) = app.push_response(completion.content, completion.usage) {
                spawn_request(ctx, messages);
            }
        }
        Update::ReplyEnd(Err(message)) => {
            app.finish_stream();
            app.push_error(message);
        }
        // A finished command goes straight back to the model, continuing the loop.
        Update::Command(Ok(output)) => {
            let messages = app.push_command_result(output);
            spawn_request(ctx, messages);
        }
        Update::Command(Err(message)) => {
            app.push_error(format!("could not run command: {message}"))
        }
    }
}

fn handle_event(event: Event, app: &mut App, ctx: &Ctx, metrics: ui::Metrics) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(key, app, ctx, metrics),
        Event::Mouse(mouse) => match mouse.kind {
            // Clicking a modal button is equivalent to confirming it.
            MouseEventKind::Down(MouseButton::Left) if app.pending().is_some() => {
                if ui::hit(metrics.allow_button, mouse.column, mouse.row) {
                    allow(app, ctx);
                } else if ui::hit(metrics.deny_button, mouse.column, mouse.row) {
                    deny(app, ctx);
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
            MouseEventKind::ScrollUp => app.scroll_up(3),
            MouseEventKind::ScrollDown => app.scroll_down(3, metrics.max_scroll()),
            _ => {}
        },
        Event::Paste(text) if !app.is_busy() => {
            // Normalise line endings so pasted CRLF does not leave stray \r.
            app.input
                .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        }
        _ => {}
    }
}

/// Approve the pending command and start running it.
fn allow(app: &mut App, ctx: &Ctx) {
    let Some(command) = app.approve() else { return };
    let sandbox = ctx.sandbox.clone();
    let timeout = ctx.timeout;
    let tx = ctx.tx.clone();
    tokio::spawn(async move {
        let result = exec::run(&sandbox, &command, timeout)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(Update::Command(result)).await;
    });
}

/// Refuse the pending command and let the model know.
fn deny(app: &mut App, ctx: &Ctx) {
    if let Some(messages) = app.deny() {
        spawn_request(ctx, messages);
    }
}

fn handle_key(key: KeyEvent, app: &mut App, ctx: &Ctx, metrics: ui::Metrics) {
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
    // cannot leak into the prompt hidden behind it.
    if app.pending().is_some() {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => app.toggle_choice(),
            KeyCode::Char('y') | KeyCode::Char('Y') => allow(app, ctx),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => deny(app, ctx),
            KeyCode::Enter => match app.pending().map(|p| p.selected) {
                Some(Choice::Allow) => allow(app, ctx),
                Some(Choice::Deny) => deny(app, ctx),
                None => {}
            },
            KeyCode::PageUp => app.scroll_up(page),
            KeyCode::PageDown => app.scroll_down(page, max_scroll),
            _ => {}
        }
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
                spawn_request(ctx, messages);
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
        KeyCode::Char(c) if !ctrl => app.input.insert_char(c),
        _ => {}
    }
}

fn spawn_request(ctx: &Ctx, messages: Vec<Message>) {
    let client = ctx.client.clone();
    let tx = ctx.tx.clone();
    tokio::spawn(async move {
        // A send failure just means the UI shut down; stop forwarding if so.
        let end = match client.open_stream(&messages).await {
            Ok(stream) => stream_reply(stream, &tx).await,
            Err(e) => Err(format!("{e:#}")),
        };
        let _ = tx.send(Update::ReplyEnd(end)).await;
    });
}

/// Forward stream deltas to the UI, accumulating the full reply. Returns the
/// completed reply, or the first error encountered.
async fn stream_reply(
    stream: impl futures_util::Stream<Item = anyhow::Result<openrouter::StreamEvent>>,
    tx: &mpsc::Sender<Update>,
) -> Result<Completion, String> {
    use openrouter::StreamEvent;
    futures_util::pin_mut!(stream);

    let mut content = String::new();
    let mut usage = None;
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::Delta(delta)) => {
                content.push_str(&delta);
                // `send().await` back-pressures the HTTP read, so tokens are
                // never dropped when the UI falls behind.
                if tx.send(Update::Delta(delta)).await.is_err() {
                    return Err("UI closed".to_string());
                }
            }
            Ok(StreamEvent::Done { usage: Some(u) }) => usage = Some(u),
            Ok(StreamEvent::Done { usage: None }) => {}
            Err(e) => return Err(format!("{e:#}")),
        }
    }
    Ok(Completion { content, usage })
}
