# Plan: harness improvements from the OpenGL cube evaluation

Ranked by value; each item says what changed, where, and how to verify it.

## 1. `/evaluate` can hand the model a path the sandbox refuses

**What happened.** During the evaluation turn, `/evaluate` composed an
absolute write target
(`/Users/.../ai_harness/.ai_harness/sessions/OpenGLHelloWorld/evaluation-001.md`)
from `sessions_dir` + `current_session`, and the prompt instructed the model to
write the report there. But the session had `cd`'d into `tmp/` earlier, so its
sandbox root moved via `Sandbox::rooted_at` (see the `/cd` handler around
`app.rs:1828`) while `sessions_dir` stayed at the project root. The target lay
outside the current sandbox, the write came back "Operation not permitted", and
the report landed in `tmp/evaluation-001.md` instead of the session folder it
was numbered for. The harness handed the model a task it cannot perform — the
same failure mode the code already warns about elsewhere (`sandbox.rs:56-61`).

**Fix.** In `App::evaluate_now`, before sending the prompt, check whether the
computed `path` resolves inside `sandbox.root()`. If it does not, refuse the
command with a notice naming the cause ("the session directory is outside the
sandbox root — run `/evaluate` from the project root, or `/cd` back") rather
than composing a prompt whose one commanded action must fail. `toggle_plan_mode`
already carries an analogous notice for a sessions dir outside the working
tree; reuse that wording where it fits.

Alternative, more permissive: set `Sandbox::writes_limited_to` to that one file
for the evaluation turn, exactly as plan mode narrows to its plan file. More
machinery, same observable benefit — do it only if refusing proves too blunt.

**Verify.** Add a test in `app.rs::evaluate_tests` that builds a session,
moves its sandbox root with `rooted_at` to a subdirectory, runs
`Command::Evaluate`, and asserts the model is *not* handed an out-of-root path.
Run `cargo test` (see the repo's TMPDIR/sandbox caveats), plus
`cargo test show_the_evaluation_prompt -- --ignored --nocapture` to eyeball the
prompt wording.

## 2. De-bait the batch section, or implement consolidated approvals

**What happened.** The evaluation reported the model's most tempting batch —
a `chmod +x build.sh && ./build.sh` next to a read — is exactly what the
deliberate "batch children are the four auto-approved actions" invariant
forbids (`BATCH_CHILD_TAGS` in `protocol.rs:172`; the TODO entry on batching
calls the invariant the whole point). The contract simultaneously calls
batching "the single most valuable thing you can do for your own speed and
cost" and buries the restriction in a subordinate clause, so the
advertisement and the invariant pull in opposite directions. One of the
session's three protocol errors was a `<ai-harness-shell>` inside a
`<ai-harness-batch>`.

**Fix, two tiers in order of cost.**

a) **Contract wording** (do first). In `protocol::system_prompt`, lead the
   batch section with what a batch may *not* hold and with a worked example of
   the pairing the model actually wants — a mutation plus a lookup — said
   plainly, with the one-approval-per-mutation reason up front. The parser's
   error (`BatchChildNotAllowed`) already names the mistake when it happens;
   the fix is making the invitation not set the mistake up.

b) **Consolidated-approval batches** (defer). Widen `BATCH_CHILD_TAGS`;
   make `App::push_reply`'s `Action::Batch` arm and `advance_batch` park a
   *queue of pending approvals* rather than lookups alone; render a multi-item
   modal in `ui.rs`. This deliberately breaks two load-bearing invariants —
   "one mutating action, one approval" (`app.rs` around the `read_only`
   comment and the batch dispatch) and plan mode's refusal-to-modal path — so
   it deserves its own change set. Do it only if shell-in-batch protocol
   errors recur after (a).

**Verify.** For (a), no test needed beyond `cargo test` to confirm the prompt
still formats; read the rendered prompt with the ignored test above. For (b),
the protocol tests pin the batch shape (`a_batch_cannot_nest`,
`a_batch_refuses_a_note_on_a_child`, and friends), and every one of them says
what the new invariant must become.

## 3. The GLAD pivot: a model-behaviour note, not a harness change

The largest cost of the session was my own — I asserted Glad 2's CMake API
and header layout with no evidence, and only investigated after configure
failed. The harness did nothing wrong: it reported the 404, the configure
failure, and the stale-GLFW failure accurately and immediately, and each
report was what enabled the recovery. No code change follows. The action is a
standing rule for future sessions: before writing build scripts that name a
tool's API, run the installed tool's `--help` (or `pkg-config --list-all`)
once — that one command replaces a chain of failed builds and documentation
fetches.

## Order of work

1. Item 1 — smallest, fixes a real failure observed this session.
2. Item 2a — a few lines in `system_prompt`, cheap, de-baits the contract.
3. Item 3 — no code, note kept as a working rule.
4. Item 2b — only if protocol errors of that shape keep appearing.
