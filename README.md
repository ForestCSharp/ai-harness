# ai-harness

A terminal agent harness for models served through [OpenRouter](https://openrouter.ai).
The prompt box is pinned to the bottom of the screen; the conversation scrolls above it.

## The protocol

Input is not sent as free text. Each prompt goes out wrapped in a single element:

```xml
<ai-harness-query>count the rust files in src</ai-harness-query>
```

The model must reply with exactly one of two elements, and nothing else:

```xml
<ai-harness-shell>ls -1 src/*.rs | wc -l</ai-harness-shell>
```

```xml
<ai-harness-response>There are 8 Rust source files.</ai-harness-response>
```

The contract is sent as the system prompt on every request, and replies are
validated strictly — prose around the tag, a markdown code fence, two elements,
an unknown tag, or an empty body are all rejected rather than guessed at, so
protocol drift surfaces immediately. A rejected reply is shown in the transcript
as a `protocol error` alongside the raw text the model actually sent.

A shell action is executed only after you approve it, and its result is fed back
as `<ai-harness-shell-result>`, so the model can keep working until it returns a
final response.

If a reply fails validation, the harness tells the model exactly what was wrong
and asks again, up to `--max-retries` (default 3). After that it gives up and
rolls the failed exchange out of context, so a bad reply is not left behind as an
example to imitate. Retries are always on; `/debug` only changes what you see.

## Slash commands

Commands are handled locally and never sent to the model.

| Command | Action |
| --- | --- |
| `/debug` | Toggle showing the raw protocol sent and received |
| `/clear` | Clear the conversation, keeping the system prompt |
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
| Reads | Open, minus `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config/gh`, Keychains, `.env` |
| Network | Allowed — the approval prompt is the control point |
| Timeout | 30s by default, killing the whole process group |
| Output | Capped at 32 KB per stream |

Every command is shown in an approval modal before it runs. `←`/`→` move between
Allow and Deny, `Enter` confirms, `y`/`n` are shortcuts, and the buttons are
clickable. Denial is reported to the model so it can propose something else.

The agentic loop is bounded by `--max-iterations` (default 10) so a model that
keeps proposing commands cannot spin forever.

**This confines the filesystem; it is not a security boundary against a
determined attacker.** Network is on, so any command you approve can send
anything it can read. Command output is also sent to OpenRouter, which means
reading a secret leaks it even with no network in the command itself — the `.env`
deny closes the obvious case, and the denylist is not exhaustive.

Non-macOS platforms fail closed at startup rather than running commands
unsandboxed.

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

Other flags: `--workdir` sets the sandbox root (default: cwd),
`--command-timeout` the per-command limit in seconds, `--max-iterations` the
agentic loop bound.

## Keys

| Key | Action |
| --- | --- |
| `Enter` | Send the prompt |
| `Alt+Enter` | Insert a newline (also `Shift+Enter` on terminals supporting the kitty keyboard protocol) |
| `Ctrl+C` | Quit |
| `Ctrl+D` | Quit when the prompt is empty |
| `Ctrl+L` | Clear the conversation (keeps the system prompt) |
| `PageUp` / `PageDown` | Scroll the transcript |
| `Ctrl+↑` / `Ctrl+↓` | Scroll one line |
| `End` | Jump back to the newest message when scrolled up |
| `Ctrl+W` / `Alt+Backspace` | Delete the previous word |
| `Ctrl+U` | Delete to the start of the line |
| `Ctrl+K` | Clear the prompt |
| `Ctrl+A` / `Ctrl+E` | Start / end of line |
| `Ctrl+Home` / `Ctrl+End` | Start / end of the prompt |

Mouse wheel scrolls the transcript. Scrolling up detaches the view; it re-attaches
to the bottom once you scroll back down (or press `End`).

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

## Layout of the code

| File | Role |
| --- | --- |
| `src/main.rs` | Event loop; keys, request results, and the redraw tick |
| `src/protocol.rs` | Query encoding, the system prompt, and strict reply parsing |
| `src/command.rs` | Slash-command parsing, the command table, and completion |
| `src/sandbox.rs` | Seatbelt profile generation and the sandboxed command |
| `src/exec.rs` | Running commands: timeout, process-group kill, output caps |
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

Four tests make real API calls and are excluded by default. They check that a
live model obeys the protocol, that a rejected reply recovers when sent our
correction, and that a real request streams in incrementally:

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

The streaming request lives in one `tokio` task (`spawn_request` in
`src/main.rs`); an `Esc`-to-cancel feature would abort exactly that task, which
is why streaming was built before cancellation.
