# Request lifecycle

What happens from pressing Enter to the response the harness settles on. Line
references point at the current code and are the best starting points if you want
to read along.

## The event loop

Everything runs inside one `tokio::select!` loop in `run`
([src/main.rs:68](../src/main.rs)). Each iteration does two things: redraw the
whole screen, then block until one of three things happens —

- a terminal event (keypress, mouse, paste),
- a background message on the `rx` channel,
- an 80 ms timer tick that advances the spinner.

It is a single-threaded state machine; nothing mutates `App` except in response
to one of those. Long work (HTTP requests, command execution) is pushed to
detached `tokio` tasks that report back over the channel, so the UI never blocks.

## 1. You press Enter

The keypress reaches `handle_key` ([src/main.rs:210](../src/main.rs)). With no
approval modal up, `Enter` does:

```rust
app.accept_completion();          // if a /command was half-typed, finish it
if let Some(messages) = app.submit() {
    spawn_request(ctx, messages);
}
```

`submit` ([src/app.rs:277](../src/app.rs)) is the fork in the road. It takes the
prompt buffer and asks `command::parse`:

- **A slash command** (`/debug`, `/clear`, …) → runs locally via `run_command`
  ([src/app.rs:237](../src/app.rs)), returns `None`. Nothing is sent; the journey
  ends here.
- **Ordinary text** → `send_prompt(text)` ([src/app.rs:291](../src/app.rs)).

`send_prompt` does the bookkeeping for a real turn:

1. pushes an `Entry::User` to the transcript (what *you* see),
2. wraps the text as `<ai-harness-query>…</ai-harness-query>` and pushes that to
   `history` (what the *model* sees), plus a `Direction::Sent` debug frame,
3. resets `iterations` and `retries` to 0,
4. sets status to `Waiting`,
5. returns a clone of the full `history` to send.

The transcript and the model's history deliberately diverge: you see plain text,
the model sees the wrapped protocol.

## 2. The request streams in the background

`spawn_request` ([src/main.rs:306](../src/main.rs)) fires a detached task so the
UI never blocks. It calls `client.open_stream`, then `stream_reply`
([src/main.rs:321](../src/main.rs)) forwards each token:

- every content delta → `Update::Delta(text)` on the channel, sent with `.await`
  so a slow UI **back-pressures the HTTP read** rather than dropping tokens,
- usage arrives in its own final chunk,
- at the end, one `Update::ReplyEnd(Ok(Completion))` — or `Err` if the connection
  broke.

Meanwhile the main loop keeps spinning, draining those updates in `handle_update`
([src/main.rs:128](../src/main.rs)):

- **`Delta`** → `app.push_delta` ([src/app.rs:164](../src/app.rs)): appends to the
  `streaming` buffer and flips status to `Streaming`. On the next redraw you see
  the text growing live below the transcript with a `▌` cursor.
- **`ReplyEnd(Ok)`** → `finish_stream()` ([src/app.rs:174](../src/app.rs)) throws
  away the live buffer, then hands the *full* text to `push_response`.

The live buffer is display-only; the authoritative text is what `push_response`
receives.

## 3. The reply is parsed — the decision point

`push_response` ([src/app.rs:315](../src/app.rs)) is the single choke point. It
records the `Direction::Received` debug frame, then runs `protocol::parse_reply`
on the whole reply. Four outcomes:

**(a) Malformed** — prose, a code fence, two elements, an empty body. It records
an `Entry::Malformed`, and under the retry cap (default 3) appends a targeted
correction to history via `retry_after` ([src/app.rs:356](../src/app.rs)) and
returns `Some(messages)`, which `handle_update` immediately re-sends. That is a
loop back to step 2. After the cap it gives up, rolls the failed exchange out of
history, and returns to `Idle`. Malformed replies do **not** count against the
agentic `iterations` budget — only valid ones do.

**(b) `<ai-harness-response>`** — a final answer. Status → `Idle`. **This is where
the journey ends**: the answer sits in the transcript, the prompt is live again.

**(c) `<ai-harness-shell>`** or **`<ai-harness-write>`** — the model wants to
change something. Status → `AwaitingApproval`, which raises the modal. Nothing
runs yet.

**(d) `<ai-harness-read>`** — the model wants to see a file. This one never
reaches the modal. `perform_read` runs `files::read` **synchronously, right
here**, pushes an `Entry::ReadResult`, appends `<ai-harness-read-result>` to
history, and returns `Some(messages)` — the same "here are messages, send them
now" shape `retry_after` uses. So a read is a round-trip with no user in the
loop and no background task at all.

That shortcut is only safe because a read is confined harder than a shell
command: `files::resolve` canonicalises the path and refuses anything landing
outside the working directory, including a symlink inside it that points out.
A failed read is *not* an error that ends the turn — it comes back as a result
the model can react to, exactly like a non-zero exit code. Reads still count
against `iterations`, so "free" does not mean unbounded.

Doing file I/O on the event loop is a deliberate trade: a read is capped at
64 KB from local disk, which is far cheaper than the task spawning, channel
plumbing, and generation tagging a background job would need.

## 4. If it's a command or a write: approval → execution → back to the model

With the modal up, the keyboard is rerouted ([src/main.rs:210](../src/main.rs),
the `app.pending().is_some()` branch) — arrows/Tab move the highlight, Enter/y/n
decide:

- **Deny** → `app.deny()` ([src/app.rs:438](../src/app.rs)) tells the model the
  user refused and it did **not** run, so it proposes an alternative instead of
  assuming success, and re-sends. Back to step 2.
- **Allow** → `allow()` ([src/main.rs:190](../src/main.rs)) spawns another
  background task running `exec::run`, which executes the command under the macOS
  Seatbelt sandbox with a timeout and output caps. When it finishes →
  `Update::Command(output)`.

`handle_update` takes that output and calls `push_command_result`
([src/app.rs:453](../src/app.rs)) — which wraps stdout/exit code as
`<ai-harness-shell-result>`, feeds it back into history, and **re-sends to the
model** (step 2 again).

## The loop closes

A single prompt can bounce through **query → reply → read → result → reply →
command → result → reply → …**, each hop a full round-trip, until the model
finally emits an `<ai-harness-response>` — or the `iterations` cap stops it and
returns control to you. Read hops pass through without pausing for you; command
and write hops stop at the modal.

The shape worth holding onto: **`push_response` is the hub.** Streaming, retries,
the approval modal, and command results are all just different edges feeding back
into it, and it has exactly one terminal exit — `<ai-harness-response>` → `Idle`.

```
                       ┌─────────────────────────────────────────────┐
                       │                                             │
   you type ─▶ submit ─▶ send_prompt ─▶ spawn_request ─▶ (stream) ─▶ push_response
                                            ▲                            │
                                            │                 ┌──────────┼───────────┐
                                            │                 │          │           │
                                        re-send          malformed?   shell?    response?
                                            │             (retry)    (approve)   → Idle ✔
                                            │                 │          │
                                            └───── deny / command result ┘
```

## Cancelling a turn

`Esc` while busy (and not in the modal) interrupts the in-flight work. Two things
happen: the event loop signals the current task's cancel channel — a streaming
task breaks its `select!` loop and drops the connection; a command task kills its
process group via the same `kill_group` the timeout uses — and `App::cancel`
([src/app.rs](../src/app.rs)) bumps the **generation counter**, drops the live
stream view, returns to `Idle`, and posts a `Cancelled.` notice.

The generation counter is what makes this safe. A task may have already queued an
`Update` on the channel at the instant of cancel; bumping the generation makes
every such update stale, and `handle_update` drops anything whose tag no longer
matches ([src/main.rs](../src/main.rs)). Without it, a late token would drag the
UI back into `Streaming`, or a late reply would commit to an abandoned turn.

Inside the approval modal, `Esc` keeps its existing meaning — **Deny** — which
refuses the command and continues the loop rather than abandoning the turn.

## The one caveat that shapes everything

Because `parse_reply` is strict and whole-reply, **streaming is display-only**.
You watch the reply arrive token by token, but nothing acts on it — no modal, no
execution — until it is complete and parsed. A half-arrived `<ai-harness-shell>`
cannot be run.

## Where each piece lives

| File | Role in this flow |
| --- | --- |
| `src/main.rs` | Event loop, key handling, background tasks, `handle_update` |
| `src/app.rs` | State transitions: `submit`, `push_response`, retries, approval |
| `src/protocol.rs` | `encode_query`, `parse_reply`, the correction and result payloads |
| `src/openrouter.rs` | `open_stream` and SSE framing |
| `src/exec.rs` / `src/sandbox.rs` | Sandboxed command execution |
| `src/files.rs` | Path resolution and bounded reads for `<ai-harness-read>` |
| `src/session.rs` | Saving and loading sessions (`/save`, `/load`) |
| `src/ui.rs` | Rendering the transcript, live stream, and approval modal |
