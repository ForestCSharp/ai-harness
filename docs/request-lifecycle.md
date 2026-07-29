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
an `Entry::Malformed`, and under the retry cap (default 3) appends the bad reply
plus a targeted correction to history via `retry_after` ([src/app.rs](../src/app.rs))
and returns `Some(messages)`, which `handle_update` immediately re-sends. That is
a loop back to step 2. Malformed replies do **not** count against the agentic
`iterations` budget — only valid ones do.

A retry loop leaves nothing behind. However it ends — the model recovers, the cap
is hit, or you press `Esc` — `roll_back_retries` truncates history to
`retry_anchor`, the point where context was last clean, so the failed attempts and
their corrections are gone before the next request. Nothing in a malformed
exchange ever ran (a rejected reply never reaches the dispatch above), so there is
nothing to preserve. The transcript still shows every attempt; only the model's
context is rewound.

One class of malformed reply is not merely badly shaped. If the model writes a
**result element itself** — `<ai-harness-fetch>url</ai-harness-fetch>` followed by
its own invented `<ai-harness-fetch-result>` — it has fabricated the outcome of an
action that never ran. `parse_reply` names that specifically
(`ProtocolError::FabricatedResult`) rather than reporting it as trailing content,
because the problem is not the shape of the reply but that its contents are
fiction; the correction says the fetch did not happen and that nothing in the
element may be used. And `retry_after` runs the reply through
`protocol::elide_results` before it goes into history, replacing every result body
with a marker — sending the invented text back verbatim would make it context the
model answers from, which is the failure the rejection exists to prevent.

**(b) `<ai-harness-response>`** — a final answer. Status → `Idle`. **This is where
the journey ends**: the answer sits in the transcript, the prompt is live again.

**(c) `<ai-harness-shell>`** or **`<ai-harness-write>`** — the model wants to
change something. Status → `AwaitingApproval`, which raises the modal. Nothing
runs yet.

**(c′) `<ai-harness-edit>`** — a targeted change to an existing file. Before the
modal, `files::plan_edit` resolves the path, reads the *whole* file, and requires
`<old>` to match **exactly once**. Zero or many matches is not a modal — it is a
failure fed straight back to the model (via the same write-result path), so you
are only ever asked about an edit that will actually apply. On a unique match the
prepared full rewrite is stashed in `Pending.edit_plan` and the modal shows a
`-`/`+` diff. `approve` then turns that plan into an ordinary `Action::Write`, so
execution reuses the write path entirely — the file is not re-read after you
approve, so the bytes that land are exactly the ones the diff showed.

A **write** now has a pre-flight of its own, for a different reason. `App::diff_against_disk`
reads the target through the same `files::read_all` and diffs the proposed
contents against it, so a full rewrite shows what changes rather than what it
contains. Three things about it are deliberate:

- It is a **display** read. Not an `<ai-harness-read>`: it is never shown to the
  model, costs no iteration, and `--confirm-reads` does not gate it. It goes
  through `files::resolve`, so it is confined exactly as a read is.
- It **cannot fail the write**. A new file, an unreadable one, one past
  `MAX_EDIT_BYTES` — each just means no diff, and the renderer falls back to a
  bounded preview. None of them is an error worth reporting.
- The diff is computed **once**, here, and stored on both `Pending` and the
  `Entry::Action`. Rendering is pure and repeats every frame, and by the time the
  transcript is re-rendered the write has landed — "diff against the file" would
  no longer mean what it meant when you needed to see it. Storing it also keeps
  the modal and the scrollback showing one computation rather than two.

**(c″) `<ai-harness-option>`** — the model wants to ask *you* something. It goes
to `Status::AwaitingChoice(Question)`, which is **not** a `Pending` and that is
the point: `App::pending` returns `None`, so the auto-approve hook in
`handle_update` cannot see it and cannot answer on your behalf. A question is the
one action in the protocol that must always reach a person, and keeping it out of
the approval state makes that structural rather than a rule to remember.

Answering (or dismissing) appends an `<ai-harness-option-result>` and re-sends,
exactly like a command result — so unlike `<ai-harness-response>`, asking does not
end the turn. `OPTION_RESULT_TAG` is in `RESULT_TAGS`, so a model that writes its
own answer is caught as fabrication; of all the results it could invent, this is
the one that would put words in the user's mouth.

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

**(e) `<ai-harness-fetch>`** — the model wants to read a web page. Also
auto-approved, but it cannot take the read's shortcut: it is network I/O, so it
needs exactly the task spawning and generation tagging a read avoids. The
dispatch therefore parks the URL in `App.pending_fetch` and returns `None`,
which normally means "the loop pauses here". `handle_update` treats that `None`
as a question rather than an answer: it calls `take_pending_fetch`, and on a hit
runs `spawn_fetch`. That keeps every background task starting from the same
place in the event loop, instead of giving `App` the ability to spawn.

The safety argument is different from the read's, and lives in `src/fetch.rs`.
A read is *more* confined than a shell command; a fetch cannot be, since it is
an outbound request to a host the model chose. What bounds it is the
destination: https only, and no address on this machine or this network. The
address check is installed as the HTTP client's DNS resolver rather than run
beforehand, because checking and then connecting leaves a rebinding race — the
client resolves again at connect time, and a redirect resolves a fresh host.
As the resolver it is the single point every hop must pass.

Like a read, a refusal is data: a blocked URL or an HTTP error comes back as an
`<ai-harness-fetch-result>` the model can react to, and counts against
`iterations`. Unlike a read, the result carries a note telling the model the
text is untrusted — it is the only result body written by neither the user nor
the harness.

The converse case is not data at all. A `<ai-harness-fetch-result>` the *model*
wrote is not a refusal to react to, it is a page that was never fetched, and it
is elided rather than answered — see the fabrication paragraph in **(a)** above.

## 4. If it's a command, write, or edit: approval → execution → back to the model

With the modal up, the keyboard is rerouted ([src/main.rs:210](../src/main.rs),
the `app.pending().is_some()` branch) — arrows/Tab move the highlight, Enter/y/n
decide:

- **Deny** → `app.deny()` ([src/app.rs:438](../src/app.rs)) tells the model the
  user refused and it did **not** run, so it proposes an alternative instead of
  assuming success, and re-sends. Back to step 2.
- **Allow** → `allow()` ([src/main.rs](../src/main.rs)) spawns another background
  task running `exec::run_streaming`, which executes the command under the macOS
  Seatbelt sandbox with a timeout and output caps. When it finishes →
  `Update::Command(output)`.

A command does not stay silent while it runs. `run_streaming` reads both pipes
incrementally and forwards each read as an `exec::Chunk`; the spawned task relays
those onto the same `Tagged` channel everything else uses, so live output arrives
as `Update::CommandChunk` and is generation-tagged — chunks from a cancelled
command are dropped by the same check that drops a late token. `App.running`
accumulates them into the outlined window the transcript draws in place of the
`running…` spinner.

That buffer is **display-only**, exactly like `streaming` for a model reply: the
authoritative text is the `CommandOutput` at the end, so nothing acts on a
half-read pipe and the window can be bounded without losing anything the model
needs.

Two consequences of reading incrementally, both deliberate:

- **The timeout is an idle bound.** It resets on every read and every line
  written, so a command is killed after that long producing nothing rather than
  that long running. `HARD_TIMEOUT_MULTIPLE` bounds the total anyway, since a
  command that prints forever would otherwise reset the clock forever.
- **stdin can be a pipe.** `run_streaming` takes an optional receiver of lines;
  with `None` stdin stays `/dev/null` and an interactive command fails fast as it
  always has. Under `--interactive` the event loop holds the sender in
  `InFlight.stdin`, and `Enter` routes the prompt line there instead of to the
  model. What was typed is collected by `App` — `exec` wrote bytes to a pipe and
  never saw lines — and attached to the `CommandOutput`, so `encode_shell_result`
  can tell the model a human answered. Without that the model would see a prompt
  in stdout, output that depends on the answer, and no answer: a gap it would
  fill by guessing.

`handle_update` takes that output and calls `push_command_result`
([src/app.rs](../src/app.rs)) — which wraps stdout/exit code as
`<ai-harness-shell-result>`, feeds it back into history, and **re-sends to the
model** (step 2 again).

### Under `--auto-approve`, the modal step is skipped

The dispatch above is unchanged: `push_response` still parks a `Pending` and still
returns `None`. What differs is what `handle_update` does with that `None` — the
same branch that spawns a parked fetch also calls `allow()` directly when the mode
is on. Everything after that point is identical, because it is literally the same
function the Allow button calls.

Two consequences of putting the decision there rather than in `App`:

- The modal never renders. `push_response` and `allow` both run inside one
  `handle_update` call, which returns before the loop reaches `terminal.draw`, so
  no frame is ever drawn with `Status::AwaitingApproval` set.
- `App` still cannot start work of its own. It parks; the event loop spawns. That
  is the same split the parked fetch uses, and it keeps every approval — button,
  click, or automatic — flowing through a single `allow`.

The `iterations` cap is not bypassed: the budget arm in `push_response` is checked
*before* the per-action arms, so past the cap no `Pending` is created and there is
nothing for the mode to approve. `Esc` also still cancels, since an auto-approved
command is `Running` like any other. Those two are the only brakes left, which is
the trade the mode makes.

## The loop closes

A single prompt can bounce through **query → reply → read → result → reply →
command → result → reply → …**, each hop a full round-trip, until the model
finally emits an `<ai-harness-response>` — or the `iterations` cap stops it and
returns control to you. Read and fetch hops pass through without pausing for
you; command and write hops stop at the modal — unless `--auto-approve` is on, in
which case nothing stops and the whole chain runs on the `iterations` cap alone.

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
| `src/diff.rs` | Line-by-line diffs of a write or edit, bounded for storage |
| `src/highlight.rs` | Language detection and per-line tokenising for code blocks |
| `src/fetch.rs` | URL policy, guarded DNS, and HTML-to-text for `<ai-harness-fetch>` |
| `src/session.rs` | Session folders under `.ai_harness/` (`/save`, `/load`) |
| `src/ui.rs` | Rendering the transcript, live stream, and approval modal |
