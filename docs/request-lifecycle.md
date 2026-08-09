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

There is more than one `App`. `Sessions` ([src/sessions.rs](../src/sessions.rs))
holds a `Slot` per running conversation — its `App`, its in-flight handle, its
render cache — and they run at the same time, sharing the one channel. So a
background message names its session as well as its generation, and
`route_update` finds the slot before anything else happens. **Both checks are
needed**: a generation is only unique within a session, and an id that matches
nothing means the session was shut down while its work was in flight. Terminal
events go to the focused slot, or to the sessions view when it is open. Everything
below describes one session; the others are doing the same thing beside it.

The set of slots is not built fresh each launch. `Sessions::restore` reads
`open.json` from the sessions directory and reopens what was running when this
project last quit — the `App` built in `run` becomes the template each restored
session takes its flags from, before adopting its own saved model and
conversation. When anything came back it also leaves the sessions view open, so
the first frame is the list rather than a conversation. `sync_open_set` runs once
per loop iteration beside
`maybe_autosave` and does the mirror of that job: it tells each session which
others are running, so none loads a conversation another slot already holds
(both would auto-save to one file), and writes the record when the set has
changed. Both are derived and compared rather than hooked onto spawn, close,
switch and rename, for the reason `maybe_autosave` works from a fingerprint —
one call site cannot be missed.

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
- every reasoning delta → `Update::Reasoning(text)`, a second event kind that
  stops at the screen (see below),
- usage arrives in its own final chunk,
- at the end, one `Update::ReplyEnd(Ok(Completion))` — or `Err` if the connection
  broke.

Meanwhile the main loop keeps spinning, draining those updates in `handle_update`
([src/main.rs:128](../src/main.rs)):

- **`Delta`** → `app.push_delta` ([src/app.rs:164](../src/app.rs)): appends to the
  `streaming` buffer and flips status to `Streaming`. On the next redraw you see
  the text growing live below the transcript with a `▌` cursor.
- **`Reasoning`** → `app.push_reasoning`: appends to a **separate** buffer, drawn
  as a dim capped window above the reply.
- **`ReplyEnd(Ok)`** → `finish_stream()` ([src/app.rs:174](../src/app.rs)) throws
  away both live buffers, then hands the *full* text to `push_response`.

The live buffer is display-only; the authoritative text is what `push_response`
receives.

Reasoning is `StreamEvent::Reasoning`, its own variant rather than a flag on
`Delta`, so that every consumer of the stream has to say what it does with it.
That is not ceremony: `Client::complete` — the compaction summariser — has an arm
that drops reasoning explicitly, and without the separate variant a chain of
thought would have been folded into a summary that then becomes conversation the
model answers from. `stream_reply`'s reasoning arm likewise never touches
`content`. Nothing downstream of step 3 ever sees it: it is not parsed, not added
to `history`, and not written to a session.

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

Two shapes are recovered rather than retried, both through `recover_reply`. They
are near-misses rather than mistakes — the model said the right thing in a shape
the parser does not take — and both are common enough that correcting them by
round-trip would be the dominant cost of the loop. Everything else is rejected
exactly as before, and `--strict-replies` turns both off.

**A preamble.** A sentence of narration in front of an otherwise perfect element
— "Let me read that.`<ai-harness-read>`…" — is the commonest way a model breaks
the contract and the least informative: the action it wrote was right, so the
round-trip buys nothing. `recover_preamble` drops the prose, runs the element,
and posts a notice saying how much it dropped. Narrow: only
`ProtocolError::NotATag`, only when the element behind the prose parses on its
own, and it is the **stripped** text that goes into history, since sending the
preamble back is how the habit gets reinforced.

**A bare response.** `<ai-harness-response-text>` is required around the prose,
which makes the commonest reply there is into an error until the model adapts —
and an untreated wave of those is retry thrash. `recover_bare_response` wraps the
body itself. Narrow on the same terms: only a response, only when the body
carries no child tag at all, so a *half*-wrapped reply is a real mistake and
still earns its correction. The **wrapped** text goes into history, so the model
sees the shape it should have used rather than its own near-miss echoed back.

When a reply *is* retried and it contained one valid element,
`encode_correction` quotes that element back and asks for it alone, rather than
restating the tag list the system prompt already carries.

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

A response is the one thing rendered as markdown (`ui::render_markdown`, over
`crate::markdown`). It is also the only place that would make sense: a read or a
fetch stays verbatim, because asking to see a file means wanting its source. The
raw reply survives in the `Direction::Received` frame either way, so `/debug`
still shows exactly what came back.

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

**(c‴) `<ai-harness-memory>`** — a note attached to a reply. It rides on the
reply rather than being an action of its own, because a separate write would cost
a round-trip out of the `iterations` budget and a second approval, which is what
made keeping notes too expensive to do at all.

`take_memory` lifts it off the **front or the back** of any element's body before
the tag-specific parse — never the middle, so a grep pattern or an edit span that
merely mentions the tag keeps it, and never on a `<ai-harness-write>` at all,
whose body is file bytes preserved exactly. Shape decides whether an occurrence
*is* a note (`split_memory`) and attributes decide whether it is a *valid* one
(`read_memory`), which is what lets a trailing mention fall through to content
while a trailing note with a missing `description=` still gets named.

`Attached` has three states, not two: `Absent`, `Declined` — the empty
`<ai-harness-memory/>` — and `Note`. `App` raises
`ProtocolError::MissingMemory` when a response is `Absent` and `require_memory`
is on, routed through `retry_after` like any other violation. The requirement is
checked there rather than in the parser because it is a setting, and
`parse_reply` answers about shape alone. Requiring the *element* rather than a
*note* is deliberate: a model told it must produce a note produces one whether or
not there is anything to say. `App::keep_note`
writes it immediately: the model supplies a *name*, `memory::write_note`
sanitises it and builds the frontmatter from the required `description=`, so
neither a path outside the memory directory nor an unindexable note is
expressible. The result is pushed as an `Entry::WriteResult`, which is what makes
`/stats` and `/memory` see it through the machinery every other memory write
already goes through. In plan mode it is dropped with a notice instead.

The memory rides *beside* the action in `protocol::Reply`, not inside
`Action::Response`. `Action` is serialised into every `session.json`, so changing
that variant's shape would stop saved sessions loading, and bumping
`session::VERSION` would orphan the ones already on disk.

**(c″) A memory note that would not index** is refused on the same principle.
A write into `.ai_harness/memory/` whose contents carry no `description:` in
frontmatter never reaches the modal: `App::memory_note_problem` hands the model a
reason and one round-trip to fix it. Without that the note is written, approved,
and then silently skipped by `memory::list` — a failure that looks like a success.
The check calls `memory::description_in`, the same parser the index uses, because
a validator that disagreed would pass a note that then vanished anyway. An edit
is checked against `EditPlan.updated`, the full post-edit file, so stripping a
description out is caught too.

`App::targets_memory_note` decides what counts, and does it **lexically** rather
than through `files::resolve_target` the way `targets_plan_file` does — resolving
canonicalises the parent, so it needs the directory to exist, and the first note a
project keeps is written into one that does not. That is three path rules at three
strictnesses: exact where a write is *permitted*, lexical where a format is
*checked*, and a loose substring in `stats::note_name` where a *metric* is
counted and missing one only understates a number.

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

A read may carry `offset=`/`limit=` for a line window, and `files::read` streams
line-wise rather than slurping, so paging through a large file costs only the
bytes it skips. When a window does not reach the end, `encode_read_result` names
the exact follow-up read. That matters more than it looks: the note it replaced
said only that the file was longer, which left the model with no better move than
to read the identical head again — measured at 25% of one real session's context.

**(e) `<ai-harness-grep>` / `<ai-harness-glob>`** — the model wants to find
something. Auto-approved on the read's reasoning, but dispatched on the fetch's:
the walk is parked in `App.pending_search` and `handle_update` spawns it via
`take_pending_search`. Two reasons it cannot take the read's inline shortcut.
It is *unbounded* work where a read is capped at 64 KB of one file, and it is
*blocking* filesystem work, which on a runtime worker would freeze the redraw and
`Esc` along with it — so `spawn_search` puts it on `spawn_blocking`. A blocking
task cannot be aborted, only asked to stop, so cancellation is an `AtomicBool`
the walk polls per directory entry; the generation tag catches the race where it
finishes anyway.

Confinement is the read's, applied per entry rather than once. `files::resolve`
handles a `dir=`, and then every file and directory the walk touches is checked
against `Sandbox::denies_read` — a glob of `**/*` that consulted the denylist
only for its starting point would list `.env`. Symlinks are never followed,
which closes escaping the root, double-reporting, and `a -> .` looping in one
rule, and leaves every assembled path canonical, which is what `denies_read`
expects. A hardcoded skip list keeps the walk out of `target/` and friends; it
is a cost heuristic, not part of the boundary.

Both share one `SearchOutcome` and one `Entry::SearchResult`, because a glob is
a grep with the line dimension removed — two types would have doubled four
exhaustive matches to save an `Option<usize>`.

**(f) `<ai-harness-fetch>`** — the model wants to read a web page. Also
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

`app.approve()` has one side effect before it hands the action back:
`checkpoint_before` copies aside whatever the action is about to change. That is
the one place that knows an action is really about to run, which is why it lives
there rather than at dispatch. A write names its file, so exactly that file is
copied; a shell command could touch anything, so the workspace is snapshotted
within caps. The checkpoint is opened lazily on the first such action, so a turn
that only reads leaves no folder behind. See
[src/checkpoint.rs](../src/checkpoint.rs).

A checkpoint records the **turn number** it belongs to and nothing about
`history`. It briefly recorded the history length as well, and that was a bug:
`compact::apply` rebuilds the conversation from scratch, so the index meant
nothing afterwards and `Vec::truncate` past the end fails silently — the same
hazard `retry_anchor` is documented against a few sections down. `/undo` and
`/rewind` find the turn boundary by scanning the live history for
`<ai-harness-query>` messages instead. That works because `encode_query` has
exactly one caller and a compaction only ever collapses or drops a *prefix*, so
the prompts still in history are always the last *n* the session sent — which
also lets each one be matched back to its turn number, and thus to its
checkpoint.

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

One consequence of reading incrementally, deliberate: **the timeout is an idle
bound.** It resets on every read, so a command is killed after that long producing
nothing rather than that long running. `HARD_TIMEOUT_MULTIPLE` bounds the total
anyway, since a command that prints forever would otherwise reset the clock
forever.

stdin stays `/dev/null` throughout, so a command that waits to be answered fails
fast instead of hanging, and the live window is read-only. When an answer is
genuinely needed it comes from the model asking with `<ai-harness-option>`, not
from typing at a running process.

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

### Under plan mode, the sandbox is narrowed and the exit changes

Plan mode reaches the lifecycle in three places, all small:

- **What runs.** `action_sandbox` in [src/main.rs](../src/main.rs) hands every
  spawned command and write a `Sandbox::writes_limited_to(plan)` instead of the
  ordinary one, so the profile the kernel enforces allows exactly one writable
  path. That is why the guarantee covers a shell command and not just the actions
  `App` can inspect — nothing here parses what a command intends to do.
- **What is asked.** A `Write` or `Edit` aimed elsewhere is turned into a write
  *result* by an arm ahead of the approval arms in `push_response`, so the model
  gets a reason and the user is never asked to approve something the kernel would
  refuse anyway. The check is for the message; the profile is the boundary.
- **How the turn ends.** `<ai-harness-response>` normally means `Idle`. In plan
  mode, with a non-empty plan file on disk, it means
  `Status::AwaitingExecute` — the fourth panel, on the approval panel's footer, so
  the same button rects carry the clicks. `execute_plan` clears the mode, rebuilds
  the contract, and calls `send_prompt`, which is why the work begins as an
  ordinary turn with a fresh `iterations` budget rather than as a special case.

The contract itself is `history[0]`, rewritten by `App::refresh_contract` whenever
the mode or the session name changes — the plan path is embedded in the text, so a
`/rename` mid-plan has to update it — **and at the start of every prompt**, since
two of its sections come from disk.

Those two are the project's standing knowledge, both keyed on the sandbox root:

- **`AGENTS.md`**, whole, in its own section beside the one `--system` fills.
  Capped at 16 KB, because this document goes out again on every round-trip of
  an agentic turn rather than once per prompt.
- **The memory index** ([src/memory.rs](../src/memory.rs)): the names and
  descriptions of `.ai_harness/memory/*.md`, and nothing else. The bodies are
  files the model opens with `<ai-harness-read>` when a description matches what
  it is doing, so a note costs a line standing and its real size only when used.
  Descriptions come out of a bounded head read, the trick `session::head` uses.

Both are read on each rebuild rather than stored, the rule `plan_path` and
`rewind_rows` follow: the model writes memories itself, so a cached copy would go
stale inside the session that wrote it. Rebuilding per *prompt* rather than per
round-trip is what makes the disk access affordable — nothing on disk can change
mid-turn without the model having done it, and it will see that next prompt.

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

## When the conversation stops fitting

`push_response` has one more thing to do before it returns `None`. With the reply
committed, the turn ended, and nothing in flight, it asks whether the
conversation has grown past `--compact-at` of the model's window — real
`prompt_tokens` from the last request against `ModelInfo::context_length`, or
`history_bytes()` against a fixed fallback when the catalog cannot say. This is
the same position and the same reasoning as the `max_turn_bytes` guard: growth
*within* a turn is that guard's problem, and a mid-turn overflow is
`push_error`'s.

A compaction is **worked out but not applied**. `compact::plan` returns a `Plan`
— where the verbatim tail starts, and the prefix with every tool-result body
replaced by a stub — which `App` parks in `pending_compaction` for the event loop
to spawn a summarising request for. That request is deliberately out of band:
`Client::complete` rather than `open_stream`, with a prompt that is *not* the
protocol contract, because the reply is prose that becomes context and
`push_response` would reject it as malformed.

Only when the summary lands does `apply_summary` touch `history`. That ordering
is the whole design: a failed, refused, or cancelled summary leaves the
conversation byte-identical, because nothing had changed yet. It also has four
pieces of bookkeeping to get right, each of which fails silently otherwise —
`retry_anchor` is a raw index (rolled back first), `turn_start_bytes` is a byte
snapshot (carried across), `fingerprint` assumes lengths only grow (so it saves
outright), and `history[0]` carries the plan contract (so it re-derives it). The
pre-compaction conversation goes to `compaction-NNN.json` in the session folder
before any of that happens.

`push_error` uses the same machinery when the provider rejects a request as too
long: compact, then resend the identical request. Once — `overflow_compacted`
is set there and cleared only by a new prompt, so a second overflow in the same
turn gives up instead of looping.

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

### The prompt is not frozen while a turn runs

Typing, pasting and completion all work with a request in flight; `handle_key`
has no busy branch for editing. What such a keystroke is allowed to *do* is
decided in one place, `App::submit`, from
`Command::runs_while_busy` ([src/command.rs](../src/command.rs)) — an exhaustive
match, so a command added later has to answer the question rather than inherit an
answer.

Two things disqualify a command. **Rewriting `history`** (`/clear`, `/compact`,
`/load`, `/fork`, `/plan`, `/undo`, `/rewind`) would leave the in-flight reply
landing on a conversation that no longer matches what was sent. **Moving the
session folder** (`/rename <name>`, and `/save <name>`, which renames too) would
move the directory the turn's open checkpoint is writing into. Everything else
either only reads, or deliberately applies to the next turn — `/model` mid-turn
lands on the next request, because the one in flight already carries its model.

Refused input is left in the buffer rather than consumed, which is why `submit`
parses `input.text()` and only clears once it has decided to act.

`submit` is the *only* entry point, so it is the only guard: `/undo`, `/rewind`
and `/clear` carry no busy check of their own. `reset_conversation` used to,
because `Ctrl+L` called it directly; removing that chord removed the one path
that bypassed the single decision, and the redundant guard went with it.

### The one update that is not part of a turn

The model catalog behind `/model` is fetched once at startup by
`spawn_catalog_fetch` and arrives as `Update::Models`. It sets no `InFlight`, so
`Esc` cannot cancel it, bumps no generation, so it cannot invalidate a turn, and
is exempt from the staleness check above — it is tagged with generation 0 and
applies whenever it lands. Every other update belongs to a turn; this one belongs
to the process.

The chosen model rides on the request rather than on the client: `spawn_request`
builds each request with `ctx.client.with_model(&app.model)`, so `App::model` is
the single source of truth and can change mid-session. It is saved with the
session, and `App::apply_session` adopts it on load.

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
| `src/search.rs` | The confined tree walk behind `<ai-harness-grep>` and `<ai-harness-glob>` |
| `src/compact.rs` | Working out what to drop when the conversation stops fitting |
| `src/diff.rs` | Line-by-line diffs of a write or edit, bounded for storage |
| `src/highlight.rs` | Language detection and per-line tokenising for code blocks |
| `src/markdown.rs` | Markdown subset for rendering `<ai-harness-response>` |
| `src/fetch.rs` | URL policy, guarded DNS, and HTML-to-text for `<ai-harness-fetch>` |
| `src/session.rs` | Session folders under `.ai_harness/` (`/save`, `/load`, `open.json`) |
| `src/memory.rs` | The `.ai_harness/memory/` index: descriptions in the contract, bodies on demand |
| `src/sessions.rs` | Several sessions at once, and the `Ctrl+Space` view |
| `src/checkpoint.rs` | Per-turn file snapshots and the `/undo` restore |
| `src/ui.rs` | Rendering the transcript, live stream, and approval modal |
