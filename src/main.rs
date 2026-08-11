mod app;
mod checkpoint;
mod command;
mod compact;
mod config;
mod diff;
mod exec;
mod fetch;
mod files;
mod highlight;
mod input;
mod jobs;
mod ledger;
mod markdown;
mod memory;
mod openrouter;
mod protocol;
mod sandbox;
mod search;
mod session;
mod sessions;
mod stats;
mod tui;
mod ui;
mod wrap;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
use sessions::{InFlight, Sessions};

/// Spinner animation rate. Only paced the spinner: an idle harness has nothing
/// to animate, so a tick that finds nothing running redraws nothing.
const TICK: Duration = Duration::from_millis(80);

/// Something happened in the background and the UI needs to react.
enum Update {
    /// A chunk of streamed reply text.
    Delta(String),
    /// A chunk of the model's streamed reasoning. Shown, never kept.
    Reasoning(String),
    /// The model reply finished, or the error that replaced it.
    ReplyEnd(Result<Completion, String>),
    /// A piece of output from the command running now. Display-only: the
    /// authoritative text arrives with `Command`.
    CommandChunk(exec::Chunk),
    /// A sandboxed command finished, or failed to start.
    Command(Result<CommandOutput, String>),
    /// The project's check command finished, or failed to start. The command
    /// rides along because the result the model is shown names it, and by the
    /// time this lands `App` has already cleared what it was running.
    Check(String, Result<CommandOutput, String>),
    /// A sandboxed file write finished, or failed to start.
    Write(Result<WriteOutcome, String>),
    /// A URL fetch finished. Refusals and HTTP errors arrive as a `FetchOutcome`
    /// carrying the reason, so `Err` here means the request never started.
    Fetch(Box<FetchOutcome>),
    /// A grep or glob finished. Like a fetch, every failure mode is carried in
    /// the outcome, so there is no `Err` arm to handle.
    Search(Box<search::SearchOutcome>),
    /// A compaction's summary arrived, or the reason it did not. The job rides
    /// along because the app handed it out to be spawned and needs it back to
    /// apply — which is also what keeps `history` untouched until this lands.
    Summary(Box<(compact::Job, Result<Completion, String>)>),
    /// A background job finished. Like the catalog, this belongs to no turn —
    /// see the generation note in [`route_update`].
    Job { id: String, state: jobs::State },
    /// The model catalog arrived, or the reason it did not. Unlike every other
    /// update this belongs to no turn — see [`handle_update`].
    Models(Result<Vec<openrouter::ModelInfo>, String>),
}

/// An [`Update`] tagged with the generation of the task that produced it, so a
/// cancelled task's still-queued updates can be recognised as stale and dropped.
struct Tagged {
    /// Which session asked for this. Sessions run at the same time and share one
    /// channel, so an update has to say where it belongs before the generation
    /// check below can mean anything — a generation is only unique within a
    /// session.
    session: u64,
    generation: u64,
    update: Update,
}

/// The handles needed to stop running jobs, by job id.
///
/// The *only* thing about a job the harness keeps in memory — everything else
/// (command, status, output, timing) is read from the job's directory when it is
/// wanted. That split is the point of the feature rather than an implementation
/// detail: a fact kept in two places goes stale in one of them, and the
/// filesystem is the copy that survives a reload, a second session, and a
/// restart.
///
/// Process-scoped rather than per-`App`: a job outlives the turn that started it
/// and the session it started in, and the quit path has to be able to reach every
/// one of them from a single place.
type JobRegistry = Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>;

/// Shared context the event handlers need to start new background work.
struct Ctx {
    client: Client,
    sandbox: Sandbox,
    /// Its own HTTP client, never the OpenRouter one: a separate connection pool
    /// keeps a vetted connection from being shared across a trust boundary, and
    /// the API-key-bearing client is never pointed at a model-chosen URL.
    fetcher: Fetcher,
    timeout: Duration,
    /// Wall-clock ceiling on a background job. Not `timeout`, which is an *idle*
    /// bound on a foreground command: the two answer different questions, and a
    /// job's whole point may be to sit quiet.
    job_ceiling: Duration,
    jobs: JobRegistry,
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
    // Kept by value: the sandbox moves into `Ctx` below, and both the open-set
    // record and its restore are keyed on which project this is.
    let sandbox_root = sandbox.root().to_path_buf();
    let sessions_dir = args.sessions_dir(&sandbox_root);
    let mut app = App::new(
        client.model().to_string(),
        args.system.clone(),
        args.max_iterations.max(1),
        sessions_dir.clone(),
    );
    app.debug = args.debug || cfg!(debug_assertions);
    app.max_retries = args.max_retries;
    app.strip_preamble = !args.strict_replies;
    app.show_reasoning = !args.no_reasoning;
    app.require_memory = !args.no_require_memory;
    app.keep_checkpoints = args.keep_checkpoints;
    // Reads resolve paths in-process against the sandbox root, so the app needs
    // the sandbox itself, not just its root.
    app.sandbox = Some(sandbox.clone());
    app.confirm_reads = args.confirm_reads;
    app.max_turn_bytes = if args.max_turn_bytes == 0 {
        usize::MAX
    } else {
        args.max_turn_bytes
    };
    app.compact_at = args.compact_at;
    app.check_command = args.check.clone();
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

    // Anything a previous harness left running is written off before the first
    // contract is built. The child died with that process; a `running` left on
    // disk is a claim about something that is not there, and it would both
    // mislead the model and count against the concurrency cap.
    let abandoned = jobs::sweep(&sandbox_root);
    if abandoned > 0 {
        app.push_notice(format!(
            "{abandoned} background job(s) from a previous run were marked abandoned."
        ));
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
        job_ceiling: args.job_ceiling(),
        jobs: JobRegistry::default(),
        tx,
    };
    spawn_catalog_fetch(&ctx);
    // Every conversation, its background work, and its rendering. Starts as the
    // set that was open when this project was last quit, or as one fresh session
    // when there is nothing to resume; `Ctrl+Space` opens more.
    let mut sessions = if args.no_restore {
        Sessions::new(app)
    } else {
        Sessions::restore(app, &sessions_dir, &sandbox_root)
    };
    let mut ticker = tokio::time::interval(TICK);
    let mut metrics = ui::Metrics::default();

    // Nothing has been drawn yet, so the first pass through always draws.
    let mut dirty = true;

    loop {
        // Drawing is the expensive part of the loop, so it happens once per
        // batch of work rather than once per wake-up: a tick with nothing
        // running changes nothing on screen, and a burst of stream deltas is
        // one visible change, not thirty.
        if dirty {
            // The sessions view is a screen of its own rather than a panel in
            // the prompt's slot: it is about the harness, not about any one
            // conversation, and nothing in it belongs beside a transcript.
            if sessions.view_open() {
                let rows = sessions.view_rows();
                let view = sessions.view().cloned().unwrap_or_default();
                let tick = sessions.app().tick;
                let armed = sessions.app().quit_armed();
                terminal
                    .draw(|frame| metrics = ui::draw_sessions(frame, &view, &rows, tick, armed))?;
            } else {
                let counts = (sessions.len(), sessions.blocked());
                let slot = sessions.current_mut();
                terminal.draw(|frame| {
                    metrics = ui::draw(frame, &mut slot.app, &mut slot.cache, counts)
                })?;
            }
            dirty = false;
        }

        tokio::select! {
            // Terminal input. Always goes to the focused session, or to the view
            // when it is open — one screen has the keyboard at a time.
            Some(event) = events.next() => {
                match event {
                    Ok(event) => handle_event(event, &mut sessions, &ctx, &metrics),
                    // A read error means the terminal is gone; exit rather than spin.
                    Err(err) => {
                        let app = sessions.app_mut();
                        app.push_error(format!("terminal input error: {err}"));
                        app.should_quit = true;
                    }
                }
                // A key can park a compaction — `/compact`, or a prompt whose
                // turn ends over the threshold. Both converge here rather than
                // in each handler, so there is one place that starts one.
                let slot = sessions.current_mut();
                pump_compaction(slot.id, &mut slot.app, &ctx, &mut slot.inflight);
                // `/sessions` parks a request rather than opening the view, for
                // the reason given where it is parked.
                if sessions.app_mut().take_sessions_request() {
                    sessions.open_view();
                }
                // And a session the view's `l` picked, parked for the same
                // reason: adding a slot is the harness's business.
                if let Some(name) = sessions.app_mut().take_requested_open() {
                    sessions.reveal(name);
                }
                // `/jobs kill` parks an id, since stopping a job means reaching
                // the registry of task handles and that belongs to the harness.
                if let Some(job) = sessions.app_mut().take_pending_job_kill() {
                    let stopped = kill_job(&ctx, &job);
                    sessions.app_mut().push_notice(if stopped {
                        format!("Killed job {job}.")
                    } else {
                        // Not an error: the job finished between the command
                        // being typed and this line running, which is a race the
                        // user cannot lose anything to.
                        format!("Job {job} was not running.")
                    });
                }
                dirty = true;
            }

            // Background work finished — in any session, not just this one.
            Some(tagged) = rx.recv() => {
                route_update(tagged, &mut sessions, &ctx);
                dirty = true;
            }

            // Animate the spinner while we wait. An idle harness has nothing to
            // animate, so this is the one wake-up that can leave the screen
            // alone — which is what keeps an idle session off the CPU. Any
            // session being busy is enough: a spinner in the view has to keep
            // moving for work you are not watching.
            _ = ticker.tick() => {
                if sessions.any_busy() {
                    for slot in sessions.iter_mut() {
                        slot.app.tick = slot.app.tick.wrapping_add(1);
                    }
                    dirty = true;
                }
                // An armed Ctrl+C is offered on screen until its window shuts,
                // and nothing else is happening at the moment it does.
                if sessions.app_mut().expire_quit_arm() {
                    dirty = true;
                }
            }
        }

        // Take whatever else is already waiting before drawing. Streamed deltas
        // and command output arrive far faster than a person can read them, and
        // rendering each one in turn is what made a fast reply feel slow.
        while let Ok(tagged) = rx.try_recv() {
            route_update(tagged, &mut sessions, &ctx);
        }

        // Persist a completed turn, in every session. A cheap no-op unless a
        // conversation just changed and settled back to idle — and a background
        // session settling is exactly a case the focused one cannot cover.
        for slot in sessions.iter_mut() {
            slot.app.maybe_autosave();
        }
        // And which sessions those are — told to each session, so none tries to
        // load one another slot has open, and written to disk so the next launch
        // reopens this set. Before the quit check rather than inside it, so the
        // last iteration records the final state.
        sessions.sync_open_set();

        if sessions.app().should_quit {
            // Jobs go first, and they go by handle rather than by pid: the task
            // holding one kills the whole process group, which is what reaps
            // grandchildren. `kill_on_drop` would catch most of this anyway, but
            // "most" leaves a build running after the terminal is gone.
            if let Ok(mut registry) = ctx.jobs.lock() {
                for (_, cancel) in registry.drain() {
                    let _ = cancel.send(());
                }
            }
            // Leaving takes every session with it, so nothing in flight is left
            // running and nothing unsaved is left behind.
            for slot in sessions.iter_mut() {
                if let Some(inflight) = slot.inflight.take() {
                    let _ = inflight.cancel.send(());
                }
                slot.app.cancel();
                slot.app.maybe_autosave();
            }
            return Ok(());
        }
    }
}

/// Deliver an update to the session that asked for it.
///
/// Two stale checks, in order, because there are now two ways to be stale. An id
/// that matches no session means it was shut down while its work was in flight,
/// and there is nothing left to apply the result to. Within a live session, the
/// generation check is the one that has always been here: a task the user
/// cancelled or superseded. The generation alone would not do — it is unique
/// within a session, not across them.
fn route_update(tagged: Tagged, sessions: &mut Sessions, ctx: &Ctx) {
    // The catalog belongs to no session. It is fetched once at startup, so
    // whatever anyone did while it was in flight, it is still the catalog — and
    // it goes to every session, including ones opened after it landed.
    if let Update::Models(result) = tagged.update {
        return sessions.set_catalog(result);
    }
    let Some(slot) = sessions.route(tagged.session) else {
        return;
    };
    // A job outlives the turn that started it, so the generation it was born
    // under says nothing about whether its result still matters — and every
    // `Esc` between then and now bumped that counter. Checking it here would
    // discard the completion of a job that ran perfectly, silently, which is
    // the one outcome worse than not having jobs at all.
    let outlives_its_turn = matches!(tagged.update, Update::Job { .. });
    if !outlives_its_turn && tagged.generation != slot.app.generation() {
        return;
    }
    let id = slot.id;
    handle_update(tagged, id, &mut slot.app, ctx, &mut slot.inflight);
    pump_compaction(id, &mut slot.app, ctx, &mut slot.inflight);
}

fn handle_update(
    tagged: Tagged,
    id: u64,
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
) {
    match tagged.update {
        // Routed away above; the arm exists to keep the match exhaustive.
        Update::Models(_) => {}
        // A job ended. Said in the transcript and nowhere else: the model is
        // told by the contract on its next prompt, and pushing a result here
        // would restart a turn the user has already watched end — possibly
        // while they are halfway through typing something unrelated.
        Update::Job { id, state } => {
            app.note_job_ended();
            app.push_notice(format!("Job {id} finished: {}.", state.as_line()));
        }
        // Live tokens accumulate in the display-only streaming buffer.
        Update::Delta(delta) => app.push_delta(&delta),
        Update::Reasoning(delta) => app.push_reasoning(&delta),
        // The reply is complete. Drop the live view and commit the full text
        // through the normal path — a malformed reply earns a corrective retry,
        // which comes back as messages to resend.
        Update::ReplyEnd(Ok(completion)) => {
            *inflight = None;
            app.mark_request_done();
            app.finish_stream();
            match app.push_response(completion.content, completion.usage) {
                Some(messages) => spawn_request(id, app, ctx, inflight, messages),
                // An auto-approved fetch is parked rather than returned, since
                // it is background work the app layer cannot start itself.
                None => {
                    if let Some(url) = app.take_pending_fetch() {
                        spawn_fetch(id, app, ctx, inflight, url);
                    } else if let Some(request) = app.take_pending_search() {
                        spawn_search(id, app, ctx, inflight, request);
                    // A response that wrote something parks the project check
                    // instead of ending the turn. Same reason as the two above:
                    // the app layer cannot start work of its own.
                    } else if let Some(command) = app.take_pending_check() {
                        spawn_check(id, app, ctx, inflight, command);
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
                        allow(id, app, ctx, inflight);
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
            spawn_request(id, app, ctx, inflight, messages);
        }
        Update::Command(Err(message)) => {
            *inflight = None;
            app.push_error(format!("could not run command: {message}"));
        }
        // The project check. Passing ends the turn; failing feeds the model and
        // the loop carries on, which is the whole verification step.
        Update::Check(command, Ok(output)) => {
            *inflight = None;
            if let Some(messages) = app.finish_check(&command, output) {
                spawn_request(id, app, ctx, inflight, messages);
            }
        }
        // A check that never started says nothing about the change, so it is
        // reported to the user and the turn ends rather than handing the model
        // the user's configuration problem to debug.
        Update::Check(_, Err(message)) => {
            *inflight = None;
            app.check_failed_to_start(message);
        }
        // A finished write does the same — its result feeds the loop.
        Update::Write(Ok(outcome)) => {
            *inflight = None;
            let messages = app.push_write_result(outcome);
            spawn_request(id, app, ctx, inflight, messages);
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
            spawn_request(id, app, ctx, inflight, messages);
        }
        // A search does the same: a bad pattern or an unreachable directory is
        // an outcome the model can act on, not a failed turn.
        Update::Search(outcome) => {
            *inflight = None;
            let messages = app.push_search_result(*outcome);
            spawn_request(id, app, ctx, inflight, messages);
        }
        // A summary either shortens the conversation or fails trying; both end
        // with a shorter history, and the overflow path resends on the spot.
        Update::Summary(payload) => {
            *inflight = None;
            app.mark_request_done();
            let (job, result) = *payload;
            if let Some(messages) = app.apply_summary(job, result) {
                spawn_request(id, app, ctx, inflight, messages);
            }
        }
    }
}

/// Route input to whichever screen has the keyboard.
///
/// The sessions view takes it whole while it is open — it is a screen, not a
/// panel, and nothing behind it should be typed into by accident. Otherwise the
/// focused session gets it, exactly as it did when there was only one.
fn handle_event(event: Event, sessions: &mut Sessions, ctx: &Ctx, metrics: &ui::Metrics) {
    if sessions.view_open() {
        return handle_sessions_event(event, sessions, metrics);
    }
    // Opening the view is the one key that belongs to the harness rather than to
    // a conversation, so it is checked before the session sees anything.
    if let Event::Key(key) = &event
        && key.kind == KeyEventKind::Press
        && sessions_chord(key)
    {
        return sessions.open_view();
    }
    let slot = sessions.current_mut();
    handle_session_event(
        event,
        slot.id,
        &mut slot.app,
        ctx,
        &mut slot.inflight,
        metrics,
    );
}

/// The chord that opens and closes the sessions view.
///
/// `Ctrl+Space` is the one to reach for. Off the kitty keyboard protocol a
/// terminal sends NUL for it, which crossterm reports as `Char(' ')` with
/// CONTROL — the same event kitty sends outright, so nothing special is needed
/// either way.
///
/// `Ctrl+T` still works and is deliberately not advertised, on the same footing
/// as the command aliases in [`crate::command`]. macOS binds `Ctrl+Space` to
/// "select the previous input source" for anyone with more than one keyboard
/// layout, and the system takes it before the terminal ever sees it. A second
/// chord that costs nothing is cheaper than a shortcut that silently does not
/// work.
fn sessions_chord(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(' ') | KeyCode::Char('t'))
}

/// Keys and clicks while the sessions view is up.
fn handle_sessions_event(event: Event, sessions: &mut Sessions, metrics: &ui::Metrics) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            // Typing a filter, on the same terms as the two pickers: the letters
            // belong to the query, and only the unambiguous keys still navigate.
            if sessions.view_searching() {
                match key.code {
                    KeyCode::Up => sessions.view_move(-1),
                    KeyCode::Down => sessions.view_move(1),
                    KeyCode::Enter => sessions.view_confirm(),
                    // Back to navigating, filter intact. A second Esc closes the
                    // view.
                    KeyCode::Esc => sessions.view_search(false),
                    // The two chords that mean the same thing everywhere: the
                    // toggle that opened this view still closes it, and Ctrl+C
                    // still quits.
                    _ if sessions_chord(&key) => sessions.close_view(),
                    KeyCode::Char('c') if ctrl => sessions.app_mut().request_quit(),
                    _ => picker_edit(key, alt, ctrl, |edit| sessions.view_query_input(edit)),
                }
                return;
            }
            match key.code {
                KeyCode::Enter => sessions.view_confirm(),
                KeyCode::Char('/') if !ctrl => sessions.view_search(true),
                KeyCode::Char('n') if !ctrl => sessions.view_spawn(),
                // Closes the view, as `n` does: the picker is a panel in the
                // prompt's slot and this is a whole screen, so they cannot both
                // be up — and a session you just opened is one you want to be in.
                KeyCode::Char('l') if !ctrl => {
                    sessions.close_view();
                    sessions.app_mut().open_session_picker();
                }
                KeyCode::Char('x') if !ctrl => sessions.view_close_selected(),
                // Esc, or the chord that opened it.
                KeyCode::Esc => sessions.close_view(),
                _ if sessions_chord(&key) => sessions.close_view(),
                // Ctrl+C still quits from here, as it does everywhere — and
                // takes two presses here too.
                KeyCode::Char('c') if ctrl => sessions.app_mut().request_quit(),
                // A page here is the list itself, which has no scrollback of its
                // own; the sessions cap keeps it to a screenful anyway.
                _ => {
                    if let Some(delta) = list_motion(&key, sessions::MAX_SESSIONS) {
                        sessions.view_move(delta);
                    }
                }
            }
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = owned_row_at(
                    metrics.sessions_list,
                    &metrics.sessions_rows,
                    mouse.column,
                    mouse.row,
                ) && sessions.view_select(i)
                {
                    sessions.view_confirm();
                }
            }
            MouseEventKind::Moved => {
                if let Some(i) = owned_row_at(
                    metrics.sessions_list,
                    &metrics.sessions_rows,
                    mouse.column,
                    mouse.row,
                ) {
                    sessions.view_select(i);
                }
            }
            _ => {}
        },
        _ => {}
    }
}

fn handle_session_event(
    event: Event,
    id: u64,
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
    metrics: &ui::Metrics,
) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(key, id, app, ctx, inflight, metrics)
        }
        Event::Mouse(mouse) => match mouse.kind {
            // Clicking a modal button is equivalent to confirming it.
            MouseEventKind::Down(MouseButton::Left) if app.pending().is_some() => {
                if ui::hit(metrics.allow_button, mouse.column, mouse.row) {
                    allow(id, app, ctx, inflight);
                } else if ui::hit(metrics.deny_button, mouse.column, mouse.row) {
                    deny(id, app, ctx, inflight);
                }
            }
            // The execute panel shares the approval panel's footer, so it shares
            // the button rects too; only what they do differs.
            MouseEventKind::Down(MouseButton::Left) if app.executing().is_some() => {
                if ui::hit(metrics.allow_button, mouse.column, mouse.row) {
                    execute_plan(id, app, ctx, inflight);
                } else if ui::hit(metrics.deny_button, mouse.column, mouse.row) {
                    app.keep_planning();
                }
            }
            // Same footer again, third meaning.
            MouseEventKind::Down(MouseButton::Left) if app.pending_undo().is_some() => {
                if ui::hit(metrics.allow_button, mouse.column, mouse.row) {
                    app.confirm_undo();
                } else if ui::hit(metrics.deny_button, mouse.column, mouse.row) {
                    app.cancel_undo();
                }
            }
            // Hovering a button focuses it, so click and keyboard agree.
            MouseEventKind::Moved
                if app.pending().is_some()
                    || app.executing().is_some()
                    || app.pending_undo().is_some() =>
            {
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
                    spawn_request(id, app, ctx, inflight, messages);
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
            // Clicking a rewind row goes there; hovering focuses it, so the
            // summary above updates under the cursor.
            MouseEventKind::Down(MouseButton::Left) if app.rewind().is_some() => {
                if let Some(i) = row_at(
                    metrics.rewind_list,
                    metrics.rewind_offset,
                    mouse.column,
                    mouse.row,
                ) && app.rewind_select(i)
                {
                    app.rewind_confirm();
                }
            }
            MouseEventKind::Moved if app.rewind().is_some() => {
                if let Some(i) = row_at(
                    metrics.rewind_list,
                    metrics.rewind_offset,
                    mouse.column,
                    mouse.row,
                ) {
                    app.rewind_select(i);
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
            // Clicking a model chooses it; hovering focuses it. Uniform rows,
            // so the question panel's row arithmetic works here unchanged.
            MouseEventKind::Down(MouseButton::Left) if app.model_picker().is_some() => {
                if let Some(i) = row_at(
                    metrics.models_list,
                    metrics.models_offset,
                    mouse.column,
                    mouse.row,
                ) && app.model_select(i)
                {
                    app.model_confirm();
                }
            }
            MouseEventKind::Moved if app.model_picker().is_some() => {
                if let Some(i) = row_at(
                    metrics.models_list,
                    metrics.models_offset,
                    mouse.column,
                    mouse.row,
                ) {
                    app.model_select(i);
                }
            }
            MouseEventKind::ScrollUp => app.scroll_up(3),
            MouseEventKind::ScrollDown => app.scroll_down(3, metrics.max_scroll()),
            _ => {}
        },
        // A paste into either picker narrows its list — pasting a model id or a
        // session name is exactly how you would use one you already have.
        Event::Paste(text) if app.model_picker().is_some() => {
            let text = text.replace(['\r', '\n'], " ");
            app.model_query_input(|input| input.insert_str(&text));
        }
        Event::Paste(text) if app.picker().is_some() => {
            let text = text.replace(['\r', '\n'], " ");
            app.picker_query_input(|input| input.insert_str(&text));
        }
        // Pasting into the prompt works while a turn is in flight, for the same
        // reason typing does.
        Event::Paste(text) if app.picker().is_none() => {
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
    owned_row_at(metrics.picker_list, &metrics.picker_rows, column, row)
}

/// The same, for any list whose entries span several rows and so report which
/// entry each row belongs to: the `/load` picker and the sessions view.
fn owned_row_at(
    list: Option<ratatui::layout::Rect>,
    owners: &[usize],
    column: u16,
    row: u16,
) -> Option<usize> {
    let list = list?;
    if !ui::hit(Some(list), column, row) {
        return None;
    }
    owners.get((row - list.y) as usize).copied()
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
fn allow(id: u64, app: &mut App, ctx: &Ctx, inflight: &mut Option<InFlight>) {
    let Some(action) = app.approve() else { return };

    // A read needs no subprocess, so it is done inline and the loop continues
    // immediately. Only reachable under `--confirm-reads`; otherwise a read
    // never becomes pending in the first place.
    let action = match action {
        Action::Read {
            path,
            offset,
            limit,
        } => {
            let messages = app.perform_read(&path, offset, limit);
            spawn_request(id, app, ctx, inflight, messages);
            return;
        }
        // Only reachable under `--confirm-fetch`; otherwise the dispatch parks
        // the fetch rather than making it pending.
        Action::Fetch { url } => {
            spawn_fetch(id, app, ctx, inflight, url);
            return;
        }
        // Only reachable under `--confirm-reads`, like a read — but unlike one,
        // a search is spawned rather than run inline, since a walk is unbounded
        // work and would stall the event loop.
        Action::Grep { pattern, dir, glob } => {
            spawn_search(
                id,
                app,
                ctx,
                inflight,
                search::Request {
                    kind: search::SearchKind::Grep,
                    pattern,
                    dir,
                    glob,
                },
            );
            return;
        }
        Action::Glob { pattern, dir } => {
            spawn_search(
                id,
                app,
                ctx,
                inflight,
                search::Request {
                    kind: search::SearchKind::Glob,
                    pattern,
                    dir,
                    glob: None,
                },
            );
            return;
        }
        other => other,
    };

    // A shell command is watched while it runs; a write is not, being a single
    // atomic act with nothing to show in progress.
    if let Action::Shell(command) = action {
        spawn_shell(id, app, ctx, inflight, command);
        return;
    }

    // A job is the one approved action that does not end with the model waiting
    // on it: it is started, and the turn carries straight on with the id.
    if let Action::ShellBackground(command) = action {
        if let Some(messages) = spawn_job(id, app, ctx, command) {
            spawn_request(id, app, ctx, inflight, messages);
        }
        return;
    }

    let generation = app.next_generation();
    let sandbox = action_sandbox(app, ctx);
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
            // `AwaitingChoice` rather than becoming pending, a Read, a Fetch and
            // the two searches were handled above, a Shell was handled just now,
            // and an Edit is converted to a Write by `approve`, so none are
            // reachable.
            Action::Shell(_)
            | Action::ShellBackground(_)
            | Action::Read { .. }
            | Action::Grep { .. }
            | Action::Glob { .. }
            | Action::Fetch { .. }
            | Action::Edit { .. }
            | Action::Options { .. }
            | Action::Response(_) => return,
        };
        let _ = tx
            .send(Tagged {
                session: id,
                generation,
                update,
            })
            .await;
    });
    *inflight = Some(InFlight::new(cancel_tx));
}

/// Run a command the model proposed and the user allowed.
fn spawn_shell(
    id: u64,
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
    command: String,
) {
    spawn_watched(id, app, ctx, inflight, command, |_, result| {
        Update::Command(result)
    });
}

/// Run the project's `--check` command.
///
/// The same machinery as an approved command, and that is the point: a check is
/// a foreground command the turn is waiting on, so it takes the `InFlight` slot,
/// streams into the same live window, and is cancelled by the same `Esc`. The
/// only differences are that nobody approved it — the user did, once, by
/// configuring it — and that its result is routed somewhere else.
fn spawn_check(
    id: u64,
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
    command: String,
) {
    spawn_watched(id, app, ctx, inflight, command, Update::Check);
}

/// Run a command, forwarding its output as it arrives.
///
/// One channel beyond the usual cancel, carrying output chunks back for the live
/// window. The forwarder relays them onto the single `Tagged` channel the event
/// loop drains, so live output is generation-tagged and dropped on cancel exactly
/// like every other update.
///
/// `finish` decides which `Update` the outcome becomes, and is the entire
/// difference between an approved command and a project check — everything that
/// matters (the sandbox, the timeout, cancellation, the generation tag, the
/// live window) is shared rather than reimplemented alongside.
fn spawn_watched(
    id: u64,
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
    command: String,
    finish: impl FnOnce(String, Result<CommandOutput, String>) -> Update + Send + 'static,
) {
    let generation = app.next_generation();
    app.start_running(command.clone());

    let sandbox = action_sandbox(app, ctx);
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
                            session: id,
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
                session: id,
                generation,
                update: finish(command, result.map_err(|e| format!("{e:#}"))),
            })
            .await;
    });
    *inflight = Some(InFlight::new(cancel_tx));
}

/// Start a background job, and hand back the result that says so.
///
/// Three things distinguish this from [`spawn_shell`], and all three are the
/// feature rather than incidental:
///
/// - **It does not touch `inflight`.** That slot holds the one piece of work a
///   turn is waiting on, and the turn is about to use it for the model
///   round-trip that carries the job id. A job that occupied it would deadlock
///   the loop it was supposed to free.
/// - **The model is answered immediately**, with the id rather than an exit
///   code. The turn continues; the job does not belong to it.
/// - **Its output goes to disk**, so there is no chunk relay and no live window.
///
/// `None` means the job could not be started and the user has been told; there
/// is nothing to send the model, because from its point of view nothing changed.
fn spawn_job(id: u64, app: &mut App, ctx: &Ctx, command: String) -> Option<Vec<Message>> {
    let root = ctx.sandbox.root().to_path_buf();
    let handle = match jobs::create(&root, &command) {
        Ok(handle) => handle,
        Err(message) => {
            app.push_error(format!("could not start the job: {message}"));
            return None;
        }
    };

    let sandbox = action_sandbox(app, ctx);
    let ceiling = ctx.job_ceiling;
    let tx = ctx.tx.clone();
    let registry = Arc::clone(&ctx.jobs);
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let task_handle = handle.clone();
    // Kept for the notice below; the task takes the original.
    let headline = command.lines().next().unwrap_or("").trim().to_string();

    // Started before the task is spawned, so the pid is on disk by the time this
    // function returns — a `/jobs kill` on the very next keystroke finds it.
    let cancel = async {
        let _ = cancel_rx.await;
    };
    let run = match exec::start_background(sandbox, command, task_handle.clone(), ceiling, cancel) {
        Ok((pid, run)) => {
            task_handle.record_pid(pid);
            run
        }
        Err(message) => {
            // The job never started. Recorded as killed rather than left
            // `running`, which would be a claim about a process that does not
            // exist — the same lie `jobs::sweep` exists to clean up.
            task_handle.finish(jobs::State::Killed);
            app.push_error(format!("could not start the job: {message:#}"));
            return None;
        }
    };

    tokio::spawn(async move {
        let state = run.await;
        task_handle.finish(state);
        // Dropped from the registry before the update, so a `/jobs kill` racing
        // the exit finds nothing rather than signalling a dead channel.
        if let Ok(mut registry) = registry.lock() {
            registry.remove(&task_handle.id);
        }
        let _ = tx
            .send(Tagged {
                session: id,
                // Exempt from the staleness check; see `route_update`.
                generation: 0,
                update: Update::Job {
                    id: task_handle.id.clone(),
                    state,
                },
            })
            .await;
    });

    if let Ok(mut registry) = ctx.jobs.lock() {
        registry.insert(handle.id.clone(), cancel_tx);
    }

    app.note_job_started();
    let shown = jobs::dir(&root);
    let shown = shown
        .strip_prefix(&root)
        .unwrap_or(&shown)
        .to_string_lossy();
    app.push_notice(format!(
        "Started job {} — {headline}. Output in {shown}/{}/.",
        handle.id, handle.id
    ));
    Some(app.push_raw_result(protocol::encode_job_started(
        &handle.id,
        &format!("{shown}/{}", handle.id),
    )))
}

/// Stop a job by id, whether or not this process started it.
///
/// The registry first, since a handle stops the task cleanly and takes the
/// process group with it. Falling back to the recorded pid covers the job this
/// harness did not start — after a restart the directory is all there is.
fn kill_job(ctx: &Ctx, id: &str) -> bool {
    let handle = ctx
        .jobs
        .lock()
        .ok()
        .and_then(|mut registry| registry.remove(id));
    if let Some(cancel) = handle {
        let _ = cancel.send(());
        return true;
    }
    jobs::kill(ctx.sandbox.root(), id)
}

/// Refuse the pending command and let the model know.
fn deny(id: u64, app: &mut App, ctx: &Ctx, inflight: &mut Option<InFlight>) {
    if let Some(messages) = app.deny() {
        spawn_request(id, app, ctx, inflight, messages);
    }
}

/// The sandbox a model-authored action runs under.
///
/// Ordinarily the one `main` built. In plan mode it is narrowed to the session's
/// plan file, so nothing a command does can change the tree being planned about —
/// the guarantee is the kernel's, which is why it holds for a shell command and
/// not only for a write the harness can inspect.
fn action_sandbox(app: &App, ctx: &Ctx) -> Sandbox {
    match app.plan_path().filter(|_| app.planning()) {
        Some(plan) => ctx.sandbox.writes_limited_to(plan),
        None => ctx.sandbox.clone(),
    }
}

/// Accept a finished plan: leave plan mode and start the work.
fn execute_plan(id: u64, app: &mut App, ctx: &Ctx, inflight: &mut Option<InFlight>) {
    if let Some(messages) = app.execute_plan() {
        spawn_request(id, app, ctx, inflight, messages);
    }
}

/// How far a key means to move in a list, or `None` if it is not a motion.
///
/// One table for every list in the harness, so `j` cannot mean "down" in the
/// sessions view and nothing in the `/rewind` list. Arrows and the page keys as
/// always, plus vim's `j`/`k`, `g`/`G` and `Ctrl+D`/`Ctrl+U`.
///
/// The ends are a very large delta rather than their own case: every list clamps
/// its own bounds already, and inventing a `Top`/`Bottom` variant would mean six
/// lists each learning to handle it.
///
/// `g` alone rather than vim's `gg`, deliberately: a pending-key state is a lot
/// of machinery for one keystroke in a modal you are in for two seconds.
fn list_motion(key: &KeyEvent, page: usize) -> Option<isize> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let page = page as isize;
    Some(match key.code {
        KeyCode::Up => -1,
        KeyCode::Down => 1,
        KeyCode::Char('k') if !ctrl => -1,
        KeyCode::Char('j') if !ctrl => 1,
        KeyCode::PageUp => -page,
        KeyCode::PageDown => page,
        KeyCode::Char('u') if ctrl => -((page / 2).max(1)),
        KeyCode::Char('d') if ctrl => (page / 2).max(1),
        KeyCode::Char('g') => isize::MIN / 2,
        KeyCode::Char('G') => isize::MAX / 2,
        KeyCode::Home => isize::MIN / 2,
        KeyCode::End => isize::MAX / 2,
        _ => return None,
    })
}

/// Apply an editing key to a query row, so a search field behaves like the
/// prompt does — word motions and word deletion included.
///
/// Shared by the two pickers that have one. Motions and the keys that leave the
/// field are handled by the caller; everything reaching here is an edit.
fn picker_edit(
    key: KeyEvent,
    alt: bool,
    ctrl: bool,
    mut apply: impl FnMut(&mut dyn FnMut(&mut input::Input)),
) {
    match key.code {
        KeyCode::Char(c) if !ctrl => apply(&mut |i| i.insert_char(c)),
        KeyCode::Backspace if alt || ctrl => apply(&mut |i| i.delete_word_before()),
        KeyCode::Backspace => apply(&mut |i| i.backspace()),
        KeyCode::Delete if alt || ctrl => apply(&mut |i| i.delete_word_after()),
        KeyCode::Delete => apply(&mut |i| i.delete()),
        KeyCode::Left if alt || ctrl => apply(&mut |i| i.move_word_left()),
        KeyCode::Right if alt || ctrl => apply(&mut |i| i.move_word_right()),
        KeyCode::Left => apply(&mut |i| i.move_left()),
        KeyCode::Right => apply(&mut |i| i.move_right()),
        KeyCode::Home => apply(&mut |i| i.move_to_line_start()),
        KeyCode::End => apply(&mut |i| i.move_to_line_end()),
        KeyCode::Char('u') if ctrl => apply(&mut |i| i.delete_to_line_start()),
        // See the prompt's handler: off the kitty protocol, Ctrl+Backspace
        // reaches us as Ctrl+H.
        KeyCode::Char('h') | KeyCode::Char('w') if ctrl => apply(&mut |i| i.delete_word_before()),
        _ => {}
    }
}

fn handle_key(
    key: KeyEvent,
    id: u64,
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

    // Ctrl+C is heard everywhere, even with a modal up — but it takes two
    // presses in a second. See `App::request_quit`.
    if ctrl && key.code == KeyCode::Char('c') {
        app.request_quit();
        return;
    }

    // The approval modal owns the keyboard while it is open, so stray typing
    // cannot leak into the prompt hidden behind it. Esc here means Deny (refuse
    // this command, continue the loop), not cancel-the-turn.
    if app.pending().is_some() {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => app.toggle_choice(),
            // `h`/`l` beside the arrows, as everywhere else.
            KeyCode::Char('h') | KeyCode::Char('l') if !ctrl => app.toggle_choice(),
            KeyCode::Char('y') | KeyCode::Char('Y') => allow(id, app, ctx, inflight),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => deny(id, app, ctx, inflight),
            KeyCode::Enter => match app.pending().map(|p| p.selected) {
                Some(Choice::Allow) => allow(id, app, ctx, inflight),
                Some(Choice::Deny) => deny(id, app, ctx, inflight),
                None => {}
            },
            KeyCode::PageUp => app.scroll_up(page),
            KeyCode::PageDown => app.scroll_down(page, max_scroll),
            _ => {}
        }
        return;
    }

    // The execute-the-plan panel, on the approval panel's pattern: it owns the
    // keyboard, and Esc means "not yet" rather than cancelling anything — there is
    // nothing in flight to cancel, and the plan stays on disk either way.
    if app.executing().is_some() {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => app.toggle_choice(),
            // `h`/`l` beside the arrows, as everywhere else.
            KeyCode::Char('h') | KeyCode::Char('l') if !ctrl => app.toggle_choice(),
            KeyCode::Enter => match app.executing() {
                Some(Choice::Allow) => execute_plan(id, app, ctx, inflight),
                _ => app.keep_planning(),
            },
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => app.keep_planning(),
            KeyCode::PageUp => app.scroll_up(page),
            KeyCode::PageDown => app.scroll_down(page, max_scroll),
            _ => {}
        }
        return;
    }

    // The undo panel, on the same pattern. Esc cancels it, and unlike the two
    // above there is a deliberate asymmetry: `y` is not bound. This is the one
    // modal that deletes files, so confirming it takes moving to the button.
    if app.pending_undo().is_some() {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => app.toggle_choice(),
            // `h`/`l` beside the arrows, as everywhere else.
            KeyCode::Char('h') | KeyCode::Char('l') if !ctrl => app.toggle_choice(),
            KeyCode::Enter => match app.undo_choice() {
                Some(Choice::Allow) => app.confirm_undo(),
                _ => app.cancel_undo(),
            },
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => app.cancel_undo(),
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
                    spawn_request(id, app, ctx, inflight, messages);
                }
            }
            KeyCode::Enter => {
                if let Some(messages) = app.answer_question() {
                    spawn_request(id, app, ctx, inflight, messages);
                }
            }
            // A digit picks a choice outright — one keypress is the point of
            // offering a list. Only while the free-text row is unfocused, or
            // typing "2 GB" would jump the selection instead of being typed.
            KeyCode::Char(c) if !on_other && c.is_ascii_digit() && c != '0' => {
                let index = c.to_digit(10).expect("checked ascii digit") as usize - 1;
                app.question_select(index);
            }
            // `j`/`k` and the rest, but only off the free-text row — there they
            // are letters, on the same rule the digits above follow. This modal
            // has no search: its free-text row is an *answer*, not a filter, so
            // there is nothing for `/` to start.
            _ if !on_other && list_motion(&key, page as usize).is_some() => {
                let delta = list_motion(&key, page as usize).unwrap_or(0);
                app.question_move(delta);
            }
            // Everything else edits the free-text answer, and only when that row
            // is focused — `question_input` enforces that, so a keystroke aimed
            // at a highlighted choice cannot vanish into an invisible buffer.
            KeyCode::Char(c) if !ctrl => app.question_input(|input| input.insert_char(c)),
            // Word-wise editing matches the prompt's, so the free-text row
            // behaves like the box it stands in for.
            KeyCode::Backspace if alt || ctrl => {
                app.question_input(|input| input.delete_word_before())
            }
            KeyCode::Backspace => app.question_input(|input| input.backspace()),
            KeyCode::Delete if alt || ctrl => app.question_input(|input| input.delete_word_after()),
            KeyCode::Delete => app.question_input(|input| input.delete()),
            KeyCode::Left if alt || ctrl => app.question_input(|input| input.move_word_left()),
            KeyCode::Right if alt || ctrl => app.question_input(|input| input.move_word_right()),
            KeyCode::Left => app.question_input(|input| input.move_left()),
            KeyCode::Right => app.question_input(|input| input.move_right()),
            KeyCode::Home => app.question_input(|input| input.move_to_line_start()),
            KeyCode::End => app.question_input(|input| input.move_to_line_end()),
            KeyCode::Char('u') if ctrl => app.question_input(|input| input.delete_to_line_start()),
            // See the prompt's handler: off the kitty protocol, Ctrl+Backspace
            // reaches us as Ctrl+H.
            KeyCode::Char('h') if ctrl => app.question_input(|input| input.delete_word_before()),
            KeyCode::Char('w') if ctrl => app.question_input(|input| input.delete_word_before()),
            _ => {}
        }
        return;
    }

    // The stats page owns the keyboard while it is up, but only to give it back:
    // there is nothing on it to choose. Placed above the lists and below the
    // modals, matching where `prepare_panel` puts it.
    if app.stats_open() {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => app.close_stats(),
            KeyCode::PageUp => app.scroll_up(page),
            KeyCode::PageDown => app.scroll_down(page, max_scroll),
            _ => {}
        }
        return;
    }

    // The rewind list owns the keyboard while it is open. No text field, so
    // nothing to type into: it is a list of what already happened.
    if app.rewind().is_some() {
        match key.code {
            KeyCode::Enter => app.rewind_confirm(),
            KeyCode::Esc => app.rewind_cancel(),
            _ => {
                if let Some(delta) = list_motion(&key, page as usize) {
                    app.rewind_move(delta);
                }
            }
        }
        return;
    }

    // The session picker owns the keyboard while it is open. It is a list you
    // navigate, with `/` to narrow it — see `list_motion` and `Picker::searching`.
    if app.picker().is_some() {
        if app.picker_searching() {
            // Typing a filter. Arrows still move the highlight, since they are
            // unambiguous; the letters belong to the query.
            match key.code {
                KeyCode::Up => app.picker_move(-1),
                KeyCode::Down => app.picker_move(1),
                KeyCode::PageUp => app.picker_move(-(page as isize)),
                KeyCode::PageDown => app.picker_move(page as isize),
                KeyCode::Enter => app.picker_confirm(),
                // Back to navigating, filter intact — you narrowed the list in
                // order to walk it. A second Esc closes the picker.
                KeyCode::Esc => app.picker_search(false),
                _ => picker_edit(key, alt, ctrl, |edit| app.picker_query_input(edit)),
            }
            return;
        }
        match key.code {
            KeyCode::Enter => app.picker_confirm(),
            KeyCode::Esc => app.picker_cancel(),
            KeyCode::Char('/') => app.picker_search(true),
            _ => {
                if let Some(delta) = list_motion(&key, page as usize) {
                    app.picker_move(delta);
                }
            }
        }
        return;
    }

    // The model picker works exactly as the session picker above does: a list
    // you navigate, with `/` to narrow it. There is no digit shortcut — digits
    // are part of model ids and have to be typeable.
    if app.model_picker().is_some() {
        if app.model_searching() {
            match key.code {
                KeyCode::Up => app.model_move(-1),
                KeyCode::Down => app.model_move(1),
                KeyCode::PageUp => app.model_move(-(page as isize)),
                KeyCode::PageDown => app.model_move(page as isize),
                KeyCode::Enter => app.model_confirm(),
                KeyCode::Esc => app.model_search(false),
                _ => picker_edit(key, alt, ctrl, |edit| app.model_query_input(edit)),
            }
            return;
        }
        match key.code {
            KeyCode::Enter => app.model_confirm(),
            KeyCode::Esc => app.model_cancel(),
            KeyCode::Char('/') => app.model_search(true),
            _ => {
                if let Some(delta) = list_motion(&key, page as usize) {
                    app.model_move(delta);
                }
            }
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
                spawn_request(id, app, ctx, inflight, messages);
            }
        }

        // --- Editing ---
        // Not frozen while work is in flight: the prompt stays usable so a
        // command can be typed at a session that is busy. What such a command is
        // allowed to *do* is decided in `App::submit`, which every way of
        // clearing a conversation now goes through — there is no chord for it.
        KeyCode::Char('u') if ctrl => app.input.delete_to_line_start(),
        KeyCode::Char('w') if ctrl => app.input.delete_word_before(),
        // Ctrl+Backspace only arrives *as* Ctrl+Backspace under the kitty
        // keyboard protocol. Everywhere else the terminal sends 0x08, which is
        // Ctrl+H — so binding it is what makes the chord work off kitty. Nothing
        // else wants Ctrl+H, and readline's meaning for it (delete one char) is
        // already on Backspace.
        KeyCode::Char('h') if ctrl => app.input.delete_word_before(),
        KeyCode::Char('a') if ctrl => app.input.move_to_line_start(),
        KeyCode::Char('e') if ctrl => app.input.move_to_line_end(),
        KeyCode::Char('k') if ctrl => app.input.clear(),
        KeyCode::Backspace if alt || ctrl => app.input.delete_word_before(),
        KeyCode::Backspace => app.input.backspace(),
        KeyCode::Delete if alt || ctrl => app.input.delete_word_after(),
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
fn spawn_fetch(id: u64, app: &mut App, ctx: &Ctx, inflight: &mut Option<InFlight>, url: String) {
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
                session: id,
                generation,
                update: Update::Fetch(Box::new(outcome)),
            })
            .await;
    });
    *inflight = Some(InFlight::new(cancel_tx));
}

/// Search the working directory in the background.
///
/// Shared by the automatic path and the `--confirm-reads` approval path. Two
/// things keep this off the inline route a read takes: it is unbounded work,
/// where a read is capped at 64 KB of one file, and it is *blocking*
/// filesystem work, which on a runtime worker would stall the redraw and `Esc`
/// along with it. Hence `spawn_blocking`.
///
/// A blocking task cannot be aborted, only asked to stop, so cancellation is
/// cooperative: the flag is set and the walk notices at its next directory
/// entry. Even if it finished first, the generation tag would see the result
/// dropped as stale.
fn spawn_search(
    id: u64,
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
    request: search::Request,
) {
    let generation = app.next_generation();
    let sandbox = ctx.sandbox.clone();
    let tx = ctx.tx.clone();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();

    tokio::spawn(async move {
        let job = tokio::task::spawn_blocking(move || search::run(&sandbox, &request, &stop));
        tokio::pin!(job);
        let outcome = tokio::select! {
            done = &mut job => match done {
                Ok(outcome) => outcome,
                // A panicked walk has nothing to report; the turn ends when the
                // user cancels or asks again.
                Err(_) => return,
            },
            _ = cancel_rx => {
                flag.store(true, Ordering::Relaxed);
                return;
            }
        };
        let _ = tx
            .send(Tagged {
                session: id,
                generation,
                update: Update::Search(Box::new(outcome)),
            })
            .await;
    });
    *inflight = Some(InFlight::new(cancel_tx));
}

/// Ask the model to summarise the conversation a compaction is shortening.
///
/// Out of band on purpose: this reply is prose that becomes context, not a
/// protocol action, so it must not go through `push_response` — which parses
/// every reply strictly and would reject a summary as malformed.
///
/// `Client::complete` takes no cancel future of its own, so cancellation
/// selects over the whole request. The job travels with the update because
/// nothing has been applied yet: dropping it on cancel is what makes a
/// cancelled compaction a no-op.
fn spawn_summary(
    id: u64,
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
    job: compact::Job,
) {
    let generation = app.next_generation();
    app.mark_request_sent();
    let client = ctx.client.with_model(&app.model);
    let tx = ctx.tx.clone();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let result = tokio::select! {
            done = client.complete(&job.request) => done.map_err(|e| format!("{e:#}")),
            _ = cancel_rx => return,
        };
        let _ = tx
            .send(Tagged {
                session: id,
                generation,
                update: Update::Summary(Box::new((job, result))),
            })
            .await;
    });
    *inflight = Some(InFlight::new(cancel_tx));
}

/// Start whatever compaction the app has parked, if any.
///
/// Called after handling an update and after handling a key, because a
/// compaction can be parked by several different paths — the end-of-turn
/// trigger, an overflow, `/compact` — and every one of them should reach the
/// single place allowed to start a request.
fn pump_compaction(id: u64, app: &mut App, ctx: &Ctx, inflight: &mut Option<InFlight>) {
    if let Some(job) = app.take_pending_compaction() {
        spawn_summary(id, app, ctx, inflight, job);
    }
}

/// Fetch the model catalog once, in the background, at startup.
///
/// Deliberately outside the turn machinery: it sets no `InFlight`, so `Esc`
/// cannot cancel it, and bumps no generation, so it cannot invalidate a turn.
/// It is tagged with generation 0 and exempted from the staleness check in
/// [`handle_update`] — the catalog is not part of any conversation.
fn spawn_catalog_fetch(ctx: &Ctx) {
    let client = ctx.client.clone();
    let tx = ctx.tx.clone();
    tokio::spawn(async move {
        let result = client.list_models().await.map_err(|e| format!("{e:#}"));
        let _ = tx
            .send(Tagged {
                // Belongs to no session, and is routed away before either
                // staleness check; the ids are placeholders.
                session: 0,
                generation: 0,
                update: Update::Models(result),
            })
            .await;
    });
}

fn spawn_request(
    id: u64,
    app: &mut App,
    ctx: &Ctx,
    inflight: &mut Option<InFlight>,
    messages: Vec<Message>,
) {
    let generation = app.next_generation();
    app.mark_request_sent();
    // The model can change mid-session, so the request is built with the one the
    // app holds now rather than the one the client was constructed with.
    let client = ctx.client.with_model(&app.model);
    let tx = ctx.tx.clone();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let cancel = async {
            let _ = cancel_rx.await;
        };
        // A send failure just means the UI shut down; stop forwarding if so.
        let end = match client.open_stream(&messages).await {
            Ok(stream) => stream_reply(stream, &tx, id, generation, cancel).await,
            Err(e) => Some(Err(format!("{e:#}"))),
        };
        // On cancel `end` is None, so we send nothing — the app has already
        // moved on and any late update would be dropped as stale anyway.
        if let Some(end) = end {
            let _ = tx
                .send(Tagged {
                    session: id,
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
    session: u64,
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
                        .send(Tagged { session, generation, update: Update::Delta(delta) })
                        .await
                        .is_err()
                    {
                        return Some(Err("UI closed".to_string()));
                    }
                }
                // Forwarded to the screen and nowhere else. Note what this arm
                // does *not* do: it never touches `content`, which is the text
                // the protocol parses and the conversation keeps.
                Some(Ok(StreamEvent::Reasoning(delta))) => {
                    if tx
                        .send(Tagged { session, generation, update: Update::Reasoning(delta) })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// One table for every list, so a motion cannot work in one modal and go
    /// missing in another — which is what six hand-written copies had produced.
    #[test]
    fn vim_motions_sit_beside_the_arrows() {
        let page = 10;
        for (down, up) in [
            (KeyCode::Down, KeyCode::Up),
            (KeyCode::Char('j'), KeyCode::Char('k')),
        ] {
            assert_eq!(list_motion(&key(down), page), Some(1));
            assert_eq!(list_motion(&key(up), page), Some(-1));
        }

        // The ends, by either name.
        for top in [KeyCode::Char('g'), KeyCode::Home] {
            assert!(list_motion(&key(top), page).unwrap() < -1000);
        }
        for bottom in [KeyCode::Char('G'), KeyCode::End] {
            assert!(list_motion(&key(bottom), page).unwrap() > 1000);
        }

        // A page, and vim's half page.
        assert_eq!(list_motion(&key(KeyCode::PageDown), page), Some(10));
        assert_eq!(list_motion(&key(KeyCode::PageUp), page), Some(-10));
        assert_eq!(list_motion(&ctrl_key('d'), page), Some(5));
        assert_eq!(list_motion(&ctrl_key('u'), page), Some(-5));
        // A one-row list still moves by one rather than standing still.
        assert_eq!(list_motion(&ctrl_key('d'), 1), Some(1));
        assert_eq!(list_motion(&ctrl_key('u'), 1), Some(-1));
    }

    /// `j` is a motion; `Ctrl+J` is not, and neither is a letter with no
    /// meaning here — those have to fall through to whatever the modal does
    /// with an unrecognised key.
    #[test]
    fn only_the_motion_keys_are_motions() {
        for not_a_motion in [
            key(KeyCode::Char('a')),
            key(KeyCode::Char('/')),
            key(KeyCode::Enter),
            key(KeyCode::Esc),
            ctrl_key('j'),
            ctrl_key('k'),
        ] {
            assert_eq!(
                list_motion(&not_a_motion, 10),
                None,
                "{not_a_motion:?} should not move a list"
            );
        }
    }

    /// Off the kitty protocol a terminal sends NUL for `Ctrl+Space`, which
    /// crossterm reports as `Char(' ')` with CONTROL — this is what pins the
    /// binding to the event that actually arrives.
    #[test]
    fn the_sessions_chord_is_ctrl_space_with_ctrl_t_beside_it() {
        assert!(sessions_chord(&ctrl_key(' ')));
        assert!(sessions_chord(&ctrl_key('t')));
    }

    #[test]
    fn a_bare_space_is_a_space() {
        for not_the_chord in [
            key(KeyCode::Char(' ')),
            key(KeyCode::Char('t')),
            ctrl_key('c'),
            ctrl_key('n'),
        ] {
            assert!(
                !sessions_chord(&not_the_chord),
                "{not_the_chord:?} should not open the sessions view"
            );
        }
    }
}
